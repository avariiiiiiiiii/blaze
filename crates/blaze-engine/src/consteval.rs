//! `blaze-engine consteval [--edition E]`: evaluates the constants and statics of single-file
//! crates, one per line of standard input, each a path, and writes what each gave as one JSON
//! object per file on standard output. A tab after the path starts the `cfg`s the file is read
//! with, as rustc's `--cfg`: `name` or `name="value"`, separated by spaces.
//!
//! Each item is named as rustc names it in "evaluation of `NAME` failed": its path from the
//! crate root, an associated constant of an inherent impl after its type's name. `eager` is
//! whether rustc evaluates it in every build (a free constant or a static, which `cargo check`
//! does too) or only where it is used (an associated constant).
//!
//! The standard library is loaded once; each file then takes the place of the one before as
//! the root of a library crate of the given edition. `harness/consteval_ui.py` scores rustc's
//! own `tests/ui/consts` and `tests/ui/const-generics` with it.

use std::{
    io::{BufRead as _, Write as _},
    time::Instant,
};

use anyhow::Context as _;
use hir::{AssocItem, HasSource as _, ModuleDef};
use ide_db::{ChangeWithProcMacros, RootDatabase, RootDatabaseExt as _};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice};
use project_model::{CargoConfig, RustLibSource};
use syntax::AstNode as _;
use vfs::{AbsPathBuf, VfsPath};

pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut edition = "2015".to_owned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edition" => edition = args.next().context("--edition needs a year")?,
            _ => anyhow::bail!("unexpected argument {arg}"),
        }
    }
    let dir =
        std::env::temp_dir().join(format!("blaze-consteval-{}-{edition}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"ui\"\nversion = \"0.0.0\"\nedition = \"{edition}\"\n\n\
             [lib]\npath = \"lib.rs\"\n\n[workspace]\n"
        ),
    )?;
    let root = dir.join("lib.rs");
    std::fs::write(&root, "")?;
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        all_targets: false,
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: false,
        with_proc_macro_server: ProcMacroServerChoice::None,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let (mut db, vfs, _) = load_cargo::load_workspace_at(&dir, &cargo_config, &load_config)?;
    let file = [root.clone(), dunce::canonicalize(&root)?]
        .into_iter()
        .find_map(|it| vfs.file_id(&VfsPath::from(AbsPathBuf::assert_utf8(it))))
        .map(|(file, _)| file)
        .context("the crate root is not loaded")?;
    let krate = *ide_db::base_db::relevant_crates(&db, file)
        .first()
        .context("the crate root is in no crate")?;
    let base_cfg = krate.cfg_options(&db).clone();
    let mut cfgs = String::new();
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let (test, wanted) = match line.split_once('\t') {
            Some((test, wanted)) => (test.to_owned(), wanted.to_owned()),
            None => (line, String::new()),
        };
        if wanted != cfgs {
            use ide_db::base_db::salsa::Setter as _;
            let mut options = base_cfg.clone();
            for cfg in wanted.split_whitespace() {
                match cfg.split_once('=') {
                    Some((key, value)) => options.insert_key_value(
                        intern::Symbol::intern(key),
                        intern::Symbol::intern(value.trim_matches('"')),
                    ),
                    None => options.insert_atom(intern::Symbol::intern(cfg)),
                }
            }
            krate.set_cfg_options(&mut db).to(options);
            cfgs = wanted;
        }
        let started = Instant::now();
        let text = std::fs::read_to_string(&test).unwrap_or_default();
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file, Some(load_cargo::source_text(text)));
        db.apply_change(change);
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| evaluated(&db, file)));
        let mut out =
            serde_json::json!({ "path": test, "ms": started.elapsed().as_millis() as u64 });
        match found {
            Ok(items) => out["items"] = serde_json::Value::Array(items),
            Err(panic) => out["panic"] = crate::panic_message(&*panic).into(),
        }
        let mut stdout = stdout.lock();
        writeln!(stdout, "{out}")?;
        stdout.flush()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// What evaluating each constant and static of the crate rooted at `file` gives.
fn evaluated(db: &RootDatabase, file: vfs::FileId) -> Vec<serde_json::Value> {
    hir::attach_db(db, || {
        let Some(krate) = hir::Crate::all(db).into_iter().find(|it| {
            it.root_module(db).definition_source_file_id(db).original_file(db).file_id(db) == file
        }) else {
            return Vec::new();
        };
        let text = db.file_text(file).text(db);
        let line_of = |node: hir::InFile<&syntax::SyntaxNode>| {
            let range = node.original_file_range_rooted(db);
            (range.file_id.file_id(db) == file).then(|| {
                let start = usize::from(range.range.start()).min(text.len());
                text[..start].matches('\n').count() + 1
            })
        };
        let mut items = Vec::new();
        for module in krate.modules(db) {
            let path: Vec<String> = module
                .path_to_root(db)
                .into_iter()
                .rev()
                .filter_map(|it| it.name(db))
                .map(|it| it.as_str().to_owned())
                .collect();
            let named = |name: &str| {
                path.iter().map(String::as_str).chain([name]).collect::<Vec<_>>().join("::")
            };
            for def in module.declarations(db) {
                let (name, line, result) = match def {
                    ModuleDef::Const(it) if it.has_body(db) => {
                        let name = it.name(db).map_or("_".to_owned(), |it| it.as_str().to_owned());
                        let line = it.source(db).and_then(|it| line_of(it.syntax()));
                        (named(&name), line, it.eval(db).map(drop).map_err(|e| format!("{e:?}")))
                    }
                    ModuleDef::Static(it) if it.extern_block(db).is_none() => {
                        let line = it.source(db).and_then(|it| line_of(it.syntax()));
                        let result = it.eval(db).map(drop).map_err(|e| format!("{e:?}"));
                        (named(it.name(db).as_str()), line, result)
                    }
                    _ => continue,
                };
                items.push(item(name, line, true, result));
            }
            for imp in module.impl_defs(db) {
                if imp.trait_(db).is_some() || !hir::GenericDef::from(imp).params(db).is_empty() {
                    continue;
                }
                let Some(adt) = imp.self_ty(db).as_adt() else { continue };
                for assoc in imp.items(db) {
                    let AssocItem::Const(it) = assoc else { continue };
                    let Some(name) = it.name(db) else { continue };
                    let name = named(&format!("{}::{}", adt.name(db).as_str(), name.as_str()));
                    let line = it.source(db).and_then(|it| line_of(it.syntax()));
                    let result = it.eval(db).map(drop).map_err(|e| format!("{e:?}"));
                    items.push(item(name, line, false, result));
                }
            }
        }
        items
    })
}

fn item(
    name: String,
    line: Option<usize>,
    eager: bool,
    result: Result<(), String>,
) -> serde_json::Value {
    let mut out = serde_json::json!({ "name": name, "line": line, "eager": eager });
    match result {
        Ok(()) => out["ok"] = true.into(),
        Err(error) => {
            out["ok"] = false.into();
            out["error"] = error.chars().take(400).collect::<String>().into();
        }
    }
    out
}
