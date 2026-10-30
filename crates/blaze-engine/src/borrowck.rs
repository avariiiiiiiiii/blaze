//! `blaze-engine borrowck [--edition E]`: the diagnostics of single-file crates, one per line of
//! standard input, each a path, written as one JSON object per line on standard output. A
//! tab after the path starts the `cfg`s the file is read with, as rustc's `--cfg`: `name`
//! or `name="value"`, separated by spaces.
//!
//! The standard library is loaded once; each file then takes the place of the one before as
//! the root of a library crate of the given edition, so what is checked again is only what
//! the file changes. `harness/borrowck_ui.py` scores rustc's own `tests/ui` with it.
//!
//! Both are blaze-diagnostics' diagnostics, every one rustc's checks give, the borrow checker's
//! among them. `blaze-engine borrowck --project DIR` checks a cargo workspace instead: the
//! errors blaze finds in its crates, which for code rustc accepts must be none. With
//! `--timings OUT`, it times the checker on each body of the workspace instead, writing one
//! JSON object per body to `OUT` and a summary to standard output. Without `--project`,
//! `--timings OUT` writes for each file how many bodies were checked and the time by phase.

use std::{
    io::{BufRead as _, Write as _},
    time::Instant,
};

use anyhow::Context as _;
use ide_db::{ChangeWithProcMacros, RootDatabase, RootDatabaseExt as _};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice};
use project_model::{
    CargoConfig, ProjectManifest, ProjectWorkspace, ProjectWorkspaceKind, RustLibSource,
};
use vfs::{AbsPathBuf, VfsPath};

pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    // rustc borrow-checks what a builtin derive expands to (a packed struct's derived `Debug`
    // copies each field out, and moving one that isn't `Copy` is E0507), and so does
    // `blaze check`: the derived methods need their bodies here as there.
    hir_def::nameres::EXPAND_BUILTIN_DERIVES.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut edition = "2015".to_owned();
    let mut project = None;
    let mut timings = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edition" => edition = args.next().context("--edition needs a year")?,
            "--project" => project = Some(args.next().context("--project needs a directory")?),
            "--timings" => timings = Some(args.next().context("--timings needs a file")?),
            _ => anyhow::bail!("unexpected argument {arg}"),
        }
    }
    if let Some(project) = project {
        return match timings {
            Some(out) => time_project(&project, &out),
            None => check_project(&project),
        };
    }
    // With `--timings OUT`, how many bodies the borrow checker checked for each file, and how
    // long it took by phase: an edit that leaves a body as it was must not check it again.
    let mut timings_out = match &timings {
        Some(out) => Some(std::io::BufWriter::new(std::fs::File::create(out)?)),
        None => None,
    };
    let dir = std::env::temp_dir().join(format!("blaze-borrowck-{}-{edition}", std::process::id()));
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
            krate.set_cfg_options(&mut db, options);
            cfgs = wanted;
        }
        let started = Instant::now();
        let text = std::fs::read_to_string(&test).unwrap_or_default();
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file, Some(load_cargo::source_text(text)));
        db.apply_change(change);
        if let Ok(name) = std::env::var("BLAZE_BORROWCK_MIR") {
            dump_mir(&db, file, &name);
        }
        // alloc: the file's name, once a file
        let paths = std::collections::HashMap::from([(file, test.clone())]);
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let found = || diagnostics(&db, krate, &paths);
            hir_ty_borrowck::timings::measure(found)
        }));
        let found = found.map(|(found, timings)| {
            if let Some(out) = &mut timings_out {
                let phases: serde_json::Map<_, _> = hir_ty_borrowck::timings::Phase::ALL
                    .iter()
                    .map(|&it| (it.name().into(), (timings.get(it).as_micros() as u64).into()))
                    .collect();
                let line = serde_json::json!({
                    "path": test,
                    "bodies": timings.counts.bodies,
                    "points": timings.counts.points,
                    "loans": timings.counts.loans,
                    "phases_us": phases,
                });
                let _ = writeln!(out, "{line}").and_then(|()| out.flush());
            }
            found.into_iter().filter(|(path, _)| *path == test).map(|(_, it)| it).collect::<Vec<_>>()
        });
        let mut out =
            serde_json::json!({ "path": test, "ms": started.elapsed().as_millis() as u64 });
        match found {
            Ok(found) => out["diagnostics"] = serde_json::Value::Array(found),
            Err(panic) => out["panic"] = crate::panic_message(&*panic).into(),
        }
        let mut stdout = stdout.lock();
        writeln!(stdout, "{out}")?;
        stdout.flush()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// The cargo workspace at `root`, loaded as `load_cargo::load_workspace_at` loads it, but
/// refused when `cargo metadata` fails: rust-analyzer then reads the workspace with
/// `--no-deps`, whose crates have no dependencies, so every type of another crate is an error
/// in the signatures that name it, which the bodies using them do not show, and the checker
/// reports what follows from those types.
fn load_project(
    root: &AbsPathBuf,
    cargo_config: &CargoConfig,
    load_config: &LoadCargoConfig,
) -> anyhow::Result<(RootDatabase, vfs::Vfs)> {
    let manifest = ProjectManifest::discover_single(root)?;
    let mut workspace = ProjectWorkspace::load(manifest, cargo_config)?;
    if let ProjectWorkspaceKind::Cargo { error: Some(error), .. } = &workspace.kind {
        anyhow::bail!("`cargo metadata` failed in {root}: {error:#}");
    }
    if load_config.load_out_dirs_from_check {
        let build_scripts = workspace.run_build_scripts(cargo_config)?;
        workspace.set_build_scripts(build_scripts);
    }
    let (db, vfs, _) = load_cargo::load_workspace(workspace, &cargo_config.extra_env, load_config)?;
    Ok((db, vfs))
}

/// `blaze-engine borrowck --project DIR`: the errors blaze-diagnostics gives in the local crates
/// of the cargo workspace at `DIR` (every one: rustc accepts the workspace), one JSON object
/// per line, and a last line counting files, errors, `unused_mut` warnings and crates the
/// checker panicked on, which fail it.
fn check_project(project: &str) -> anyhow::Result<()> {
    let root = AbsPathBuf::assert_utf8(dunce::canonicalize(project)?);
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        all_targets: true,
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: std::thread::available_parallelism().map_or(1, |it| it.get()),
        proc_macro_processes: 1,
    };
    let started = Instant::now();
    let (db, vfs) = load_project(&root, &cargo_config, &load_config)?;
    let crates: Vec<ide_db::base_db::Crate> = hir::attach_db(&db, || {
        let local = hir::Crate::all(&db).into_iter().filter(|it| it.origin(&db).is_local());
        local.map(ide_db::base_db::Crate::from).collect()
    });
    // alloc: the names of the workspace's files, once
    let paths: std::collections::HashMap<vfs::FileId, String> =
        vfs.iter().map(|(file, path)| (file, path.to_string())).collect();
    let files: Vec<vfs::FileId> = hir::attach_db(&db, || {
        let mut files = Vec::new();
        for &krate in &crates {
            for module in hir::Crate::from(krate).modules(&db) {
                let file = module.definition_source_file_id(&db).original_file(&db);
                files.push(file.file_id(&db));
            }
        }
        blaze_ids::radix::sort_copy_by_key(&mut files, |it| it.index());
        files.dedup();
        files
    });
    let stdout = std::io::stdout();
    let mut errors = 0;
    let mut unused_mut = 0;
    let mut panics = 0;
    for &krate in &crates {
        if let Ok(name) = std::env::var("BLAZE_BORROWCK_MIR") {
            let root = hir::Crate::from(krate).root_file(&db);
            dump_mir(&db, root, &name);
        }
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            diagnostics(&db, krate, &paths)
        }));
        let mut stdout = stdout.lock();
        match found {
            // Every error is one: rustc accepts the workspace, so it is one blaze gives wrongly.
            Ok(found) => {
                for (path, it) in found {
                    if it["severity"] == "Error" {
                        errors += 1;
                        writeln!(stdout, "{}", serde_json::json!({ "path": path, "error": it }))?;
                    } else if it["code"] == "unused_mut" {
                        unused_mut += 1;
                        writeln!(stdout, "{}", serde_json::json!({ "path": path, "warning": it }))?;
                    }
                }
            }
            Err(panic) => {
                panics += 1;
                let panic = crate::panic_message(&*panic);
                let path = paths.get(&hir::Crate::from(krate).root_file(&db)).cloned();
                writeln!(stdout, "{}", serde_json::json!({ "path": path, "panic": panic }))?;
            }
        }
    }
    let secs = started.elapsed().as_secs_f64();
    writeln!(
        stdout.lock(),
        "{}",
        serde_json::json!({
            "files": files.len(),
            "errors": errors,
            "unused_mut": unused_mut,
            "panics": panics,
            "secs": secs
        })
    )?;
    // A crate the checker panics on has none of its errors reported.
    anyhow::ensure!(
        panics == 0,
        "the checker panicked on {panics} of {} crates",
        crates.len()
    );
    Ok(())
}

/// `blaze-engine borrowck --project DIR --timings OUT`: for each body of the workspace's crates, the
/// time type inference takes on it and the time the borrow checker takes in each mode, by
/// phase, with the sizes it worked on, one JSON object per line in `OUT`; the totals on
/// standard output. Each body is inferred and then checked, one at a time on one thread:
/// `infer_us` and `first_us` include the queries the body is the first to need, while
/// `infer_warm_us` and each mode's time are what an edit of the body costs in the daemon, the
/// rest answered and the trait solver's cache empty.
fn time_project(project: &str, out: &str) -> anyhow::Result<()> {
    use hir::{AssocItem, DefWithBody, ModuleDef};
    use hir_ty_borrowck::{
        Mode,
        timings::{Phase, measure},
    };
    let root = AbsPathBuf::assert_utf8(dunce::canonicalize(project)?);
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        all_targets: true,
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ProcMacroServerChoice::Sysroot,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let (db, vfs) = load_project(&root, &cargo_config, &load_config)?;
    let mut file = std::io::BufWriter::new(std::fs::File::create(out)?);
    // By mode, as `Mode as usize`: Nll, Polonius.
    let mut totals = [[std::time::Duration::ZERO; Phase::ALL.len()]; 2];
    let mut check_totals = [std::time::Duration::ZERO; 2];
    let (mut infer_total, mut bodies) = (std::time::Duration::ZERO, 0usize);
    let (mut infer_warm_total, mut first_total) =
        (std::time::Duration::ZERO, std::time::Duration::ZERO);
    hir::attach_db(&db, || -> anyhow::Result<()> {
        let mut found: Vec<(DefWithBody, String)> = Vec::new();
        for krate in hir::Crate::all(&db) {
            if !krate.origin(&db).is_local() {
                continue;
            }
            let crate_name = krate.display_name(&db).map_or_else(String::new, |it| it.to_string());
            for module in krate.modules(&db) {
                let path = module
                    .path_to_root(&db)
                    .into_iter()
                    .rev()
                    .filter_map(|it| it.name(&db))
                    .map(|it| it.as_str().to_owned())
                    .collect::<Vec<_>>();
                let prefix = std::iter::once(crate_name.clone()).chain(path).collect::<Vec<_>>();
                let named = |name: String| format!("{}::{name}", prefix.join("::"));
                let mut push = |def: DefWithBody, name: String| found.push((def, named(name)));
                for decl in module.declarations(&db) {
                    match decl {
                        ModuleDef::Function(it) => push(it.into(), it.name(&db).as_str().into()),
                        ModuleDef::Const(it) => push(
                            it.into(),
                            it.name(&db).map_or("_".to_owned(), |it| it.as_str().into()),
                        ),
                        ModuleDef::Static(it) => push(it.into(), it.name(&db).as_str().into()),
                        ModuleDef::Adt(hir::Adt::Enum(it)) => {
                            for variant in it.variants(&db) {
                                push(
                                    variant.into(),
                                    format!(
                                        "{}::{}",
                                        it.name(&db).as_str(),
                                        variant.name(&db).as_str()
                                    ),
                                );
                            }
                        }
                        ModuleDef::Trait(it) => {
                            for item in it.items(&db) {
                                let name = format!(
                                    "{}::{}",
                                    it.name(&db).as_str(),
                                    item.name(&db).map_or("_".to_owned(), |it| it.as_str().into())
                                );
                                match item {
                                    AssocItem::Function(f) => push(f.into(), name),
                                    AssocItem::Const(c) => push(c.into(), name),
                                    AssocItem::TypeAlias(_) => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for imp in module.impl_defs(&db) {
                    let self_ty = hir::HirDisplay::display(
                        &imp.self_ty(&db),
                        &db,
                        krate.to_display_target(&db),
                    )
                    .to_string();
                    for item in imp.items(&db) {
                        let name = format!(
                            "<{self_ty}>::{}",
                            item.name(&db).map_or("_".to_owned(), |it| it.as_str().into())
                        );
                        match item {
                            AssocItem::Function(f) => push(f.into(), name),
                            AssocItem::Const(c) => push(c.into(), name),
                            AssocItem::TypeAlias(_) => {}
                        }
                    }
                }
            }
        }
        // BLAZE_BORROWCK_ONLY=part times only the bodies whose name contains it, to profile one.
        let only = std::env::var("BLAZE_BORROWCK_ONLY").ok();
        for (def, name) in found {
            if only.as_deref().is_some_and(|it| !name.contains(it)) {
                continue;
            }
            let Ok(id) = hir_def::DefWithBodyId::try_from(def) else { continue };
            let path =
                def.module(&db).definition_source_file_id(&db).original_file(&db).file_id(&db);
            let path = vfs.file_path(path).to_string();
            let started = Instant::now();
            let infer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hir_ty::InferenceResult::of(&db, hir_ty::InferBodyId::DefWithBodyId(id));
            }));
            let infer_time = started.elapsed();
            if infer.is_err() {
                continue;
            }
            // What an edit in the daemon pays: the queries of the rest of the workspace answered,
            // the trait solver's cache empty, as it is in a new revision.
            hir::clear_tls_solver_cache();
            let started = Instant::now();
            let _ = hir_ty::infer_query_with_inspect(&db, id, None, hir_ty::LoweringMode::Analysis);
            let infer_warm = started.elapsed();
            // The first check pays for the queries the body is the first to need, as the first
            // inference did; then each mode is timed as an edit.
            let started = Instant::now();
            let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                hir_ty_borrowck::borrowck_in_mode(&db, id, Mode::Polonius)
            }));
            let first_time = started.elapsed();
            if first.is_err() {
                continue;
            }
            let modes = [Mode::Polonius, Mode::Nll];
            let mut by_mode = serde_json::Map::new();
            let mut panicked = false;
            for mode in modes {
                hir::clear_tls_solver_cache();
                let started = Instant::now();
                let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    measure(|| hir_ty_borrowck::borrowck_in_mode(&db, id, mode).skipped.is_some())
                }));
                let total = started.elapsed();
                let Ok((skipped, timings)) = checked else {
                    panicked = true;
                    break;
                };
                let at = mode as usize;
                check_totals[at] += total;
                let mut phases = serde_json::Map::new();
                for phase in Phase::ALL {
                    totals[at][phase as usize] += timings.get(phase);
                    phases.insert(
                        phase.name().into(),
                        (timings.get(phase).as_micros() as u64).into(),
                    );
                }
                let counts = timings.counts;
                by_mode.insert(
                    mode_name(mode).into(),
                    serde_json::json!({
                        "borrowck_us": total.as_micros() as u64,
                        "phases_us": phases,
                        "skipped": skipped,
                        "counts": {
                            "bodies": counts.bodies,
                            "blocks": counts.blocks,
                            "points": counts.points,
                            "locals": counts.locals,
                            "regions": counts.regions,
                            "universals": counts.universals,
                            "constraints": counts.constraints,
                            "localized": counts.localized,
                            "loans": counts.loans,
                        },
                    }),
                );
            }
            if panicked {
                continue;
            }
            bodies += 1;
            infer_total += infer_time;
            infer_warm_total += infer_warm;
            first_total += first_time;
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "name": name,
                    "path": path,
                    "infer_us": infer_time.as_micros() as u64,
                    "infer_warm_us": infer_warm.as_micros() as u64,
                    "first_us": first_time.as_micros() as u64,
                    "modes": by_mode,
                })
            )?;
        }
        Ok(())
    })?;
    file.flush()?;
    let mut summary = serde_json::Map::new();
    for mode in [Mode::Polonius, Mode::Nll] {
        let phases: serde_json::Map<_, _> = Phase::ALL
            .iter()
            .map(|&it| (it.name().into(), totals[mode as usize][it as usize].as_secs_f64().into()))
            .collect();
        summary.insert(
            mode_name(mode).into(),
            serde_json::json!({
                "borrowck_secs": check_totals[mode as usize].as_secs_f64(),
                "phases_secs": phases,
            }),
        );
    }
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::json!({
            "bodies": bodies,
            "infer_secs": infer_total.as_secs_f64(),
            "infer_warm_secs": infer_warm_total.as_secs_f64(),
            "first_check_secs": first_total.as_secs_f64(),
            "modes": summary,
        })
    )?;
    Ok(())
}

fn mode_name(mode: hir_ty_borrowck::Mode) -> &'static str {
    match mode {
        hir_ty_borrowck::Mode::Polonius => "polonius",
        hir_ty_borrowck::Mode::Nll => "nll",
        hir_ty_borrowck::Mode::Off => "off",
    }
}

/// Prints the MIR of each function named `name` at the top of `file`, and of its closures, to
/// stderr: `BLAZE_BORROWCK_MIR`, for working on the checker.
fn dump_mir(db: &RootDatabase, file: vfs::FileId, name: &str) {
    use hir_ty::db::HirDatabase as _;
    hir::attach_db(db, || {
        let sema = hir::view::Semantics::new(db);
        let Some(module) = sema.file_to_module_def(file) else { return };
        for def in module.declarations(db) {
            if let hir::ModuleDef::Function(function) = def
                && function.name(db).as_str() == name
            {
                let Ok(id) = hir_def::FunctionId::try_from(function) else { continue };
                let mut bodies = match db.mir_body(hir_ty::InferBodyId::DefWithBodyId(id.into())) {
                    Ok(body) => vec![body],
                    Err(err) => {
                        eprintln!("{name}: {err:?}");
                        continue;
                    }
                };
                // The closures after it, as the borrow checker checks them with it.
                while let Some(body) = bodies.pop() {
                    eprintln!("{}", hir_ty_mir::mir::print_mir_body(db, body, name));
                    for &closure in &body.closures {
                        match db.mir_body_for_closure(closure) {
                            Ok(it) => bodies.push(it),
                            Err(err) => eprintln!("{name} closure: {err:?}"),
                        }
                    }
                }
            }
        }
    })
}

/// Every diagnostic blaze-diagnostics gives compiling `krate`, by the name in `paths` of the
/// file its first primary span is in (`<file N>` for one not named there, empty for one of no
/// place), with the line and column (from 1) of where it starts. rustc's closing lines
/// ("aborting due to ...", "N warnings emitted", the failure notes) are not among them.
fn diagnostics(
    db: &RootDatabase,
    krate: ide_db::base_db::Crate,
    paths: &std::collections::HashMap<vfs::FileId, String>,
) -> Vec<(String, serde_json::Value)> {
    use blaze_diagnostics::Level;
    let files = blaze_diagnostics::Files::new(paths, "");
    let options = blaze_diagnostics::Options::default();
    hir::attach_db(db, || {
        let found = blaze_diagnostics::check_crate(db, hir::Crate::from(krate), files, &options);
        let named: std::collections::HashMap<String, vfs::FileId> =
            paths.keys().map(|&file| (files.name(file), file)).collect();
        let closing = |it: &blaze_diagnostics::Diag| {
            it.message.starts_with("aborting due to")
                || it.message.ends_with("warning emitted")
                || it.message.ends_with("warnings emitted")
        };
        found
            .into_iter()
            .filter(|it| matches!(it.level, Level::Error | Level::Warning) && !closing(it))
            .map(|it| {
                let span = it.span.primary_span();
                let path = span.map_or_else(String::new, |it| it.file.clone());
                let text = named.get(&path).map(|&file| db.file_text(file).text(db));
                let (line, col) = match (span, text) {
                    (Some(span), Some(text)) => {
                        let start = (span.lo as usize).min(text.len());
                        let start = (0..=start).rev().find(|&at| text.is_char_boundary(at));
                        let before = &text[..start.unwrap_or(0)];
                        let line = before.matches('\n').count() + 1;
                        let col = before.rsplit('\n').next().map_or(0, |it| it.chars().count()) + 1;
                        (line, col)
                    }
                    _ => (0, 0),
                };
                let expanded = span.is_some_and(|it| it.expansion.is_some());
                let code = it.code.as_ref().map_or("", |it| it.as_str());
                let out = serde_json::json!({
                    "code": code,
                    "severity": format!("{:?}", it.level),
                    "line": line,
                    "col": col,
                    "message": it.message,
                    "expanded": expanded,
                });
                (path, out)
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package depending on a path that is not there: `cargo metadata` fails, and
    /// `--no-deps`, which rust-analyzer falls back to, does not.
    #[test]
    fn refuses_a_workspace_whose_metadata_failed() {
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join("src")).unwrap();
        std::fs::write(
            work.path().join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
             [dependencies]\nmissing = { path = \"vendor/missing\" }\n",
        )
        .unwrap();
        std::fs::write(work.path().join("src/lib.rs"), "").unwrap();
        let load_config = LoadCargoConfig {
            load_out_dirs_from_check: false,
            with_proc_macro_server: ProcMacroServerChoice::None,
            prefill_caches: false,
            num_worker_threads: 1,
            proc_macro_processes: 1,
        };
        let root = AbsPathBuf::assert_utf8(dunce::canonicalize(work.path()).unwrap());
        let error = load_project(&root, &CargoConfig::default(), &load_config)
            .err()
            .expect("a workspace whose metadata failed was loaded");
        let error = format!("{error:#}");
        assert!(error.contains("`cargo metadata` failed"), "{error}");
        let missing = std::path::Path::new("vendor").join("missing");
        assert!(error.contains(&*missing.to_string_lossy()), "{error}");
    }
}
