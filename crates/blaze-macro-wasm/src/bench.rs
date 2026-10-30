//! `blaze-engine proc-macro-wasm`: one macro of a module expanded on an input from a file, timed,
//! and, given a library of the same crate and a proc-macro server, the same expansion by the
//! server, timed too, and the outputs compared.
//!
//! ```text
//! blaze-engine proc-macro-wasm <module.wasm> --macro <name> --input <file> [--attr <text>]
//!     [--package <dir>] [--native <library> --server <proc-macro-srv>] [--runs <n>]
//!     [--store <dir>]
//! ```
//!
//! What it prints, a line each, in milliseconds: the module's compile by Cranelift, its first
//! expansion in a crate session and the median of `--runs` more, the median of `--runs` in a
//! fresh instance each (made from the pool, run and dropped); with `--native`,
//! the server's start and load of the library, its first expansion and the median of more.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use base_db::Env;
use paths::AbsPathBuf;
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
    TextRange, TextSize,
};

use crate::{Call, Wasm};

struct Args {
    module: PathBuf,
    name: Option<String>,
    input: PathBuf,
    attr: Option<String>,
    package: Option<PathBuf>,
    native: Option<PathBuf>,
    server: Option<PathBuf>,
    runs: usize,
    store: Option<PathBuf>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let module = PathBuf::from(args.next().ok_or("the module is named first")?);
    let mut parsed = Args {
        module,
        name: None,
        input: PathBuf::new(),
        attr: None,
        package: None,
        native: None,
        server: None,
        runs: 20,
        store: None,
    };
    while let Some(arg) = args.next() {
        // alloc: the message of an argument refused.
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--macro" => parsed.name = Some(value()?),
            "--input" => parsed.input = value()?.into(),
            "--attr" => parsed.attr = Some(value()?),
            "--package" => parsed.package = Some(value()?.into()),
            "--native" => parsed.native = Some(value()?.into()),
            "--server" => parsed.server = Some(value()?.into()),
            "--runs" => parsed.runs = value()?.parse().map_err(|_| "--runs takes a number")?,
            "--store" => parsed.store = Some(value()?.into()),
            // alloc: the message of an argument refused.
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    match parsed.name.as_deref() {
        None | Some("") => Err("--macro names the macro".into()),
        Some(_) => Ok(parsed),
    }
}

/// A time in milliseconds, as the command prints it.
struct Ms(Duration);

impl fmt::Display for Ms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.3}", self.0.as_secs_f64() * 1e3)
    }
}

fn median(all: &mut [Duration]) -> Duration {
    all.sort_unstable();
    all.get(all.len() / 2).copied().unwrap_or_default()
}

fn span(file: u32) -> Span {
    let file_id = EditionedFileId::new(FileId::from_raw(file), Edition::CURRENT);
    Span {
        range: TextRange::empty(TextSize::new(0)),
        anchor: SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

/// The environment cargo gives a crate of the package at `dir`.
fn env_of(dir: Option<&Path>) -> Env {
    let mut env = Env::default();
    if let Some(dir) = dir {
        let name = dir.file_name().map(|it| it.to_string_lossy()).unwrap_or_default();
        // alloc: setup, once per run of the command: the package's variables.
        env.set("CARGO_MANIFEST_DIR", dir.display().to_string());
        env.set("CARGO_PKG_NAME", &*name);
        env.set("CARGO_CRATE_NAME", name.replace('-', "_"));
        env.set("CARGO_PKG_VERSION", "0.1.0");
    }
    env
}

pub fn command(args: impl Iterator<Item = String>) -> Result<(), String> {
    let args = parse(args)?;
    let name = args.name.as_deref().unwrap_or_default();
    let text = std::fs::read_to_string(&args.input)
        // alloc: the message of an input that can't be read.
        .map_err(|e| format!("{}: {e}", args.input.display()))?;
    let call_site = span(0);
    let subtree = crate::tokens(&text, call_site)?;
    let attr = args.attr.as_deref().map(|it| crate::tokens(it, call_site)).transpose()?;
    let env = env_of(args.package.as_deref());
    let call = Call {
        subtree: &subtree,
        attrs: attr.as_ref(),
        env: &env,
        def_site: call_site,
        call_site,
        mixed_site: call_site,
    };

    // Machine code is kept in the store under `--store`; expansion outputs are not cached.
    let mut driven = args.store.as_deref().map(blaze_cas::Driven::open).transpose()?;
    let code = driven.as_mut().map(blaze_cas::Driven::handle);
    let wasm = Wasm::at(&args.module, code.as_ref())?;
    println!("wasm compile: {}", Ms(wasm.compiled_in));
    if let Some(driven) = &mut driven {
        driven.flush()?;
    }
    let macros = wasm.macros()?;
    let index = macros.iter().position(|it| it.name == name).ok_or_else(|| {
        // alloc: the message of a macro the module has not.
        let names: Vec<_> = macros.iter().map(|it| it.name.as_str()).collect();
        // alloc: the message of a macro the module has not.
        format!("the module has no macro {name}: it has {names:?}")
    })?;
    let listed = &macros[index];
    // alloc: the message of a session that can't start.
    let mut session = wasm.session(&env).map_err(|e| format!("{e:?}"))?;
    let first = session.expand(index as u32, listed, None, &call);
    // alloc: the message of an expansion that failed.
    let output = first.output.map_err(|e| format!("{e:?}"))?;
    println!("wasm first: {}", Ms(first.took));
    let mut runs = Vec::with_capacity(args.runs);
    for _ in 0..args.runs {
        let ran = session.expand(index as u32, listed, None, &call);
        // alloc: the message of an expansion that failed.
        ran.output.map_err(|e| format!("{e:?}"))?;
        runs.push(ran.took);
    }
    println!("wasm warm: {}", Ms(median(&mut runs)));
    // As a module whose expansions keep no state runs: each in a fresh instance in a slot of
    // the pool, made, run and dropped.
    let fresh = Wasm::at_kind(&args.module, code.as_ref(), crate::Kind::Fresh)?;
    runs.clear();
    for _ in 0..args.runs {
        let start = Instant::now();
        // alloc: the message of an instance that can't start.
        let mut instance = fresh.session(&env).map_err(|e| format!("{e:?}"))?;
        let ran = instance.expand(index as u32, listed, None, &call);
        drop(instance);
        // alloc: the message of an expansion that failed.
        ran.output.map_err(|e| format!("{e:?}"))?;
        runs.push(start.elapsed());
    }
    println!("wasm fresh: {}", Ms(median(&mut runs)));
    // alloc: the edge: the output as the command prints and compares it.
    let wasm_text = output.to_string();

    if let (Some(native), Some(server)) = (&args.native, &args.server) {
        let start = Instant::now();
        // alloc: the message of a path that can't be made absolute.
        let absolute = |it: &Path| std::path::absolute(it).map_err(|e| e.to_string());
        let server = AbsPathBuf::assert_utf8(absolute(server)?);
        let none: [(&str, &Option<&str>); 0] = [];
        let client = proc_macro_api::ProcMacroClient::spawn(&server, none, None, 1)
            // alloc: the message of a server that can't start.
            .map_err(|e| format!("{server}: {e}"))?;
        let native = AbsPathBuf::assert_utf8(absolute(native)?);
        let loaded = client
            // clone: the edge: the server's API takes the library's path by value.
            .load_dylib(proc_macro_api::MacroDylib::new(native.clone()))
            // alloc: the message of a library that can't be loaded.
            .map_err(|e| format!("{native}: {e}"))?;
        let found = loaded.into_iter().find(|it| it.name() == name);
        // alloc: the message of a macro the library has not.
        let found = found.ok_or_else(|| format!("{native} has no macro {name}"))?;
        println!("native start: {}", Ms(start.elapsed()));
        // alloc: the edge: the environment as the server's API takes it.
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.into(), v.into())).collect();
        // alloc: the edge: the directory as the server's API takes it.
        let dir = std::env::current_dir().map(|it| it.display().to_string()).unwrap_or_default();
        let expand = || {
            let start = Instant::now();
            let made = found.expand(
                subtree.view(),
                attr.as_ref().map(|it| it.view()),
                // clone: the edge: the server's API takes the environment by value.
                env.clone(),
                call_site,
                call_site,
                call_site,
                // clone: the edge: the server's API takes the directory by value.
                dir.clone(),
                None,
            );
            (made, start.elapsed())
        };
        let (made, took) = expand();
        // alloc: the message of an expansion that failed.
        let made = made.map_err(|e| e.to_string())??;
        println!("native first: {}", Ms(took));
        let mut runs = Vec::with_capacity(args.runs);
        runs.extend((0..args.runs).map(|_| expand().1));
        println!("native warm: {}", Ms(median(&mut runs)));
        // alloc: the edge: the output as the command prints and compares it.
        let native_text = made.to_string();
        println!("same output: {}", native_text == wasm_text);
        if native_text != wasm_text {
            println!("wasm: {wasm_text}");
            println!("native: {native_text}");
        }
    } else {
        println!("output: {wasm_text}");
    }
    Ok(())
}
