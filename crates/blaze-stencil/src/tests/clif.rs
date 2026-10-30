//! The CLIF corpus (stencil.md, "Tests"): whole functions in Cranelift's text format, run on
//! the `; run:` lines after them as Cranelift's `filetests` `test run` does, in the
//! interpreter, in Cranelift's code and in stencil code.
//!
//! Two halves, one directory (`clif/`, every file in [`FILES`]):
//!
//! - `rust-*.clif`: what `hir-ty-codegen`'s IR builder makes for representative std functions:
//!   overflow checks branching to cold panic blocks, the Rust ABI's scalar pairs as two
//!   returns, `__udivti3` as a call, iterator loops over a pointer pair, `match` as
//!   `br_table`, drop glue in `try_call` cleanup pads, `core::arch` lane ops through pointers,
//!   atomics. Each names the std function it is shaped after.
//! - The rest: Cranelift's own `filetests/filetests/runtests/*.clif` for the operations of the
//!   stencil set, each naming the upstream file it follows in its first comment.
//!
//! Every file is in upstream's form: a header (`test interpret`, `test run`, `target x86_64`,
//! `target aarch64`), then functions, each followed by its run lines. Beyond upstream:
//!
//! - `; trap: %f(args)` says the call traps, by any trap code (stencil code reports a trap by
//!   its signal only).
//! - Each `%name` is renamed `u0:<its index in the file>` before anything runs, in the
//!   functions' own names and in every function they import, since the stencil glue names
//!   only user names (`arch::Cx::name` refuses a test case's). A file's functions are linked
//!   together, unit `i` being function `i`, and `u2:0` is `exec::throw_fn` (host 0).
//! - Signatures are given the target's convention: the IR builder's are always the target's.
//! - The interpreter runs no function that has a `try_call` or calls a host function, nor
//!   one that calls such a function: it has neither exceptions nor host code. Their lines
//!   hold as code only, and their files have no `test interpret`.
//!
//! A run line's expected values hold for all three: the interpreter and each backend's code
//! against them with any NaN for a NaN (`Nans::Any`), and stencil code against Cranelift's
//! bit for bit (`Nans::Exact`). Every function is compiled both ways for every target the IR
//! builder makes code for, on any host: stencil code must compile wherever Cranelift's does.
//! Code runs on the host's variant only (x86-64 Linux, AArch64 Linux, AArch64 macOS): the
//! other two are covered by their own hosts' CI, and Windows by compiling.

use cranelift_codegen::{
    data_value::DataValue,
    ir::{self, Opcode},
    isa::CallConv,
};
use cranelift_reader::{Comparison, RunCommand};

use super::{
    host,
    interp::{self, Nans, Run},
};

/// The user name namespaces of `exec`'s linking: the file's functions, and host functions.
const FUNCS: u32 = 0;
const HOST: u32 = 2;

macro_rules! files {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("clif/", $name)))),*]
    };
}

/// Every file of the corpus, by name.
const FILES: &[(&str, &str)] = files![
    // After what the IR builder makes for std.
    "rust-atomics.clif",
    "rust-binary-search.clif",
    "rust-bits.clif",
    "rust-checked-add.clif",
    "rust-drop-glue.clif",
    "rust-f64-clamp.clif",
    "rust-i128-shift.clif",
    "rust-iter-sum.clif",
    "rust-match-enum.clif",
    "rust-memchr.clif",
    "rust-option-map.clif",
    "rust-overflowing-mul.clif",
    "rust-simd-f32x4.clif",
    "rust-simd-i32x4.clif",
    "rust-u128.clif",
    // After Cranelift's runtests: integers.
    "arithmetic.clif",
    "bitops.clif",
    "bitrev.clif",
    "bswap.clif",
    "clz.clif",
    "cls.clif",
    "ctz.clif",
    "extend.clif",
    "icmp.clif",
    "popcnt.clif",
    "rotl.clif",
    "rotr.clif",
    "shifts.clif",
    // Wide integers, control, memory and atomics.
    "atomic-cas.clif",
    "atomic-load-store.clif",
    "atomic-rmw.clif",
    "br_table.clif",
    "div-checks.clif",
    "i128-arithmetic.clif",
    "i128-bitops.clif",
    "i128-icmp.clif",
    "i128-shifts.clif",
    "select.clif",
    "stack.clif",
    // Floats.
    "ceil.clif",
    "fcmp.clif",
    "fcvt.clif",
    "fcvt-narrow.clif",
    "float-arith.clif",
    "floor.clif",
    "fmin-max-pinned.clif",
    "nearest.clif",
    "trunc.clif",
    // Vectors.
    "simd-arithmetic.clif",
    "simd-bitselect.clif",
    "simd-conversions.clif",
    "simd-extra.clif",
    "simd-fcmp.clif",
    "simd-icmp.clif",
    "simd-lane-access.clif",
    "simd-min-max.clif",
    "simd-narrow.clif",
    "simd-shuffle.clif",
    "simd-splat.clif",
    "simd-swizzle.clif",
    "simd-valltrue.clif",
    "simd-vanytrue.clif",
    "simd-vhighbits.clif",
    "simd-widen.clif",
];

/// One file of the corpus, parsed and renamed.
struct File {
    name: &'static str,
    text: &'static str,
    /// Function `i` is named `u0:i`, and so are the calls of it.
    funcs: Vec<ir::Function>,
    /// Each function's name in the text, without its `%`.
    names: Vec<String>,
    /// Whether the interpreter can run each function.
    interpretable: Vec<bool>,
    lines: Vec<Line>,
}

/// One run or trap line.
struct Line {
    /// Its line number in the file, from 1.
    at: usize,
    text: &'static str,
    /// The function it calls, which it follows.
    func: usize,
    args: Vec<DataValue>,
    want: Want,
}

enum Want {
    /// The results are (`equal`) or are not the values.
    Returns { equal: bool, values: Vec<DataValue> },
    Traps,
}

impl File {
    fn at(&self, line: &Line) -> String {
        format!("{}:{}: `{}`", self.name, line.at, line.text.trim())
    }

    fn header(&self, header: &str) -> bool {
        self.text.lines().any(|line| line.trim_end() == header)
    }
}

fn corpus() -> Vec<File> {
    FILES.iter().map(|(name, text)| load(name, text)).collect()
}

/// The lines of a filetest's header, which `parse_functions` does not read.
fn is_header(line: &str) -> bool {
    line.starts_with("test ") || line.starts_with("target ") || line.starts_with("set ")
}

fn load(name: &'static str, text: &'static str) -> File {
    // Blanked line for line, so the parser's line numbers are the file's.
    let mut body = String::with_capacity(text.len());
    for line in text.lines() {
        if !is_header(line) {
            body.push_str(line);
        }
        body.push('\n');
    }
    let mut funcs =
        cranelift_reader::parse_functions(&body).unwrap_or_else(|e| panic!("{name}: {e}"));
    let names: Vec<String> = funcs
        .iter()
        .map(|func| match &func.name {
            ir::UserFuncName::Testcase(it) => {
                String::from_utf8(it.raw().to_vec()).expect("an ASCII name")
            }
            ir::UserFuncName::User(it) => panic!("{name}: function {it} is not named `%name`"),
        })
        .collect();
    for (i, it) in names.iter().enumerate() {
        assert!(!names[..i].contains(it), "{name}: two functions %{it}");
    }
    rename(name, &mut funcs, &names);
    let interpretable = interpretable(&funcs);

    let mut lines = Vec::new();
    let mut current = None;
    let mut seen = 0;
    for (k, line) in text.lines().enumerate() {
        let at = k + 1;
        if line.starts_with("function") {
            current = Some(seen);
            seen += 1;
            continue;
        }
        let Some(directive) = line.trim_start().strip_prefix(';').map(str::trim_start) else {
            continue;
        };
        let (command, traps) = if let Some(call) = directive.strip_prefix("trap:") {
            (format!("print:{call}"), true)
        } else if directive.starts_with("run") {
            (directive.to_owned(), false)
        } else if directive.starts_with("print") {
            panic!("{name}:{at}: a print line checks nothing: write `run:` or `trap:`");
        } else {
            continue;
        };
        let Some(func) = current else { panic!("{name}:{at}: a run line before any function") };
        let parsed = cranelift_reader::parse_run_command(&command, &funcs[func].signature)
            .unwrap_or_else(|e| panic!("{name}:{at}: {e}: `{line}`"));
        let (invocation, want) = match parsed {
            Some(RunCommand::Run(invocation, comparison, values)) if !traps => {
                let equal = matches!(comparison, Comparison::Equals);
                (invocation, Want::Returns { equal, values })
            }
            Some(RunCommand::Print(invocation)) if traps => (invocation, Want::Traps),
            _ => panic!("{name}:{at}: `{line}` is neither a run line nor a trap line"),
        };
        assert_eq!(
            invocation.func.trim_start_matches('%'),
            names[func],
            "{name}:{at}: a run line calls a function other than the one it follows"
        );
        lines.push(Line { at, text: line, func, args: invocation.args, want });
    }
    assert_eq!(seen, funcs.len(), "{name}: `function` lines and functions parsed");
    File { name, text, funcs, names, interpretable, lines }
}

/// Names function `i` `u0:i`, and every call of a function of the file by that name.
fn rename(file: &str, funcs: &mut [ir::Function], names: &[String]) {
    for (i, func) in funcs.iter_mut().enumerate() {
        func.name = ir::UserFuncName::user(FUNCS, i as u32);
        let refs: Vec<ir::FuncRef> = func.dfg.ext_funcs.keys().collect();
        for at in refs {
            let callee = match &func.dfg.ext_funcs[at].name {
                ir::ExternalName::TestCase(callee) => {
                    String::from_utf8(callee.raw().to_vec()).expect("an ASCII name")
                }
                _ => continue,
            };
            let Some(index) = names.iter().position(|it| *it == callee) else {
                panic!("{file}: %{} calls %{callee}, which the file does not define", names[i]);
            };
            let user =
                func.declare_imported_user_function(ir::UserExternalName::new(FUNCS, index as u32));
            func.dfg.ext_funcs[at].name = ir::ExternalName::User(user);
        }
    }
}

/// Whether the interpreter can run each function: none with a `try_call` or a call of
/// anything but the file's own functions, nor any calling one of those.
fn interpretable(funcs: &[ir::Function]) -> Vec<bool> {
    let mut ok: Vec<bool> = funcs
        .iter()
        .map(|func| {
            let foreign = func.dfg.ext_funcs.values().any(|ext| match &ext.name {
                ir::ExternalName::User(at) => func.params.user_named_funcs()[*at].namespace != FUNCS,
                _ => true,
            });
            let throws = func.layout.blocks().any(|block| {
                func.layout.block_insts(block).any(|inst| {
                    matches!(func.dfg.insts[inst].opcode(), Opcode::TryCall | Opcode::TryCallIndirect)
                })
            });
            !foreign && !throws
        })
        .collect();
    loop {
        let mut changed = false;
        for (i, func) in funcs.iter().enumerate() {
            let calls_one = func.dfg.ext_funcs.values().any(|ext| match &ext.name {
                ir::ExternalName::User(at) => !ok[func.params.user_named_funcs()[*at].index as usize],
                _ => false,
            });
            if ok[i] && calls_one {
                ok[i] = false;
                changed = true;
            }
        }
        if !changed {
            return ok;
        }
    }
}

/// `func` with its signature and every signature it imports in convention `conv`.
fn with_conv(func: &ir::Function, conv: CallConv) -> ir::Function {
    let mut func = func.clone();
    func.signature.call_conv = conv;
    for sig in func.dfg.signatures.values_mut() {
        sig.call_conv = conv;
    }
    func
}

fn returns(func: &ir::Function) -> Vec<ir::Type> {
    func.signature.returns.iter().map(|ret| ret.value_type).collect()
}

/// Whether `got` is what a run line says, any NaN being any NaN.
fn holds(tys: &[ir::Type], equal: bool, want: &[DataValue], got: &[DataValue]) -> bool {
    interp::same_all(tys, want, got, Nans::Any) == equal
}

#[test]
fn every_file_is_a_filetest_with_run_lines() {
    for file in corpus() {
        for header in ["test run", "target x86_64", "target aarch64"] {
            assert!(file.header(header), "{}: no `{header}` line", file.name);
        }
        assert!(!file.lines.is_empty(), "{}: no run lines", file.name);
        if file.header("test interpret") {
            for line in &file.lines {
                assert!(
                    file.interpretable[line.func],
                    "{}: `test interpret`, but the interpreter can't run %{}",
                    file.at(line),
                    file.names[line.func]
                );
            }
        }
        let source = if file.name.starts_with("rust-") { "hir-ty-codegen" } else { "runtests/" };
        let first = file.text.lines().find(|line| line.starts_with(';')).unwrap_or("");
        assert!(first.contains(source), "{}: the first comment names no {source} source", file.name);
    }
}

#[test]
fn every_file_of_the_directory_is_listed() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tests/clif");
    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{dir}: {e}"))
        .map(|entry| entry.expect("a directory entry").file_name().into_string().expect("UTF-8"))
        .filter(|name| name.ends_with(".clif"))
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = FILES.iter().map(|(name, _)| (*name).to_owned()).collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}

#[test]
fn every_run_line_holds_in_the_interpreter() {
    let mut failures = Vec::new();
    let mut ran = 0;
    for file in corpus() {
        let funcs: Vec<&ir::Function> = file.funcs.iter().collect();
        for line in &file.lines {
            if !file.interpretable[line.func] {
                continue;
            }
            ran += 1;
            let got = interp::run(&funcs, line.func, &line.args);
            let ok = match (&line.want, &got) {
                (Want::Returns { equal, values }, Ok(Run::Returned(got))) => {
                    holds(&returns(&file.funcs[line.func]), *equal, values, got)
                }
                (Want::Traps, Ok(Run::Trapped(_))) => true,
                _ => false,
            };
            if !ok {
                failures.push(format!(
                    "{}: the interpreter gives {got:?}\n{}",
                    file.at(line),
                    file.funcs[line.func].display()
                ));
            }
        }
    }
    assert!(ran > 0, "no run line ran");
    assert!(
        failures.is_empty(),
        "{} of {ran} run lines fail in the interpreter:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn stencil_code_compiles_wherever_cranelift_code_does() {
    let corpus = corpus();
    let (mut failures, mut refused, mut compiled) = (Vec::new(), Vec::new(), 0);
    for target in &host::TARGETS {
        let isa = host::isa(target);
        let conv = host::call_conv(target);
        for file in &corpus {
            for (i, func) in file.funcs.iter().enumerate() {
                let func = with_conv(func, conv);
                let what = format!("{} %{} for {}", file.name, file.names[i], host::triple(target));
                match (host::cranelift_caught(&func, &*isa), crate::compile(&func, target)) {
                    (Ok(_), Ok(_)) => compiled += 1,
                    (Ok(_), Err(e)) => failures.push(format!("{what}: {e}\n{}", func.display())),
                    (Err(e), _) => refused.push(format!("{what}: {e}")),
                }
            }
        }
    }
    if !refused.is_empty() {
        eprintln!(
            "Cranelift refuses {} of the corpus's functions for their target, which no code of runs there:\n{}",
            refused.len(),
            refused.join("\n")
        );
    }
    assert!(compiled > 0, "nothing compiled");
    assert!(
        failures.is_empty(),
        "stencil code refuses {} functions Cranelift compiles:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Run lines in code of both backends, on the host's variant.
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod native {
    use super::*;
    use crate::tests::exec::{self, Exec, Link, Unit};

    const _: () = assert!(exec::FUNCS == FUNCS && exec::HOST == HOST);

    #[test]
    fn every_run_line_holds_in_both_backends_code() {
        let Some(target) = host::host() else {
            eprintln!(
                "no corpus code runs on this host: it runs on x86-64 Linux, AArch64 Linux and AArch64 macOS, each in its own CI"
            );
            return;
        };
        let isa = host::isa(&target);
        let conv = host::call_conv(&target);
        let throw = [exec::throw_fn()];
        let (mut failures, mut skipped, mut ran) = (Vec::new(), Vec::new(), 0);
        for file in corpus() {
            let funcs: Vec<ir::Function> = file.funcs.iter().map(|func| with_conv(func, conv)).collect();
            let (mut cranelift, mut stencil) = (Vec::new(), Vec::new());
            let mut refused = None;
            for (i, func) in funcs.iter().enumerate() {
                let what = format!("{} %{}", file.name, file.names[i]);
                match host::cranelift_caught(func, &*isa) {
                    Ok(it) => cranelift.push(it),
                    Err(e) => {
                        refused = Some(format!("{what}: Cranelift: {e}"));
                        break;
                    }
                }
                match crate::compile(func, &target) {
                    Ok(it) => stencil.push(it),
                    Err(e) => {
                        failures.push(format!("{what}: stencil code: {e}\n{}", func.display()));
                        refused = Some(format!("{what}: stencil code: {e}"));
                        break;
                    }
                }
            }
            if let Some(why) = refused {
                skipped.push(why);
                continue;
            }
            let units: Vec<Unit<'_>> =
                cranelift.iter().zip(&funcs).map(|(code, func)| Unit::Cranelift(code, &func.signature)).collect();
            let theirs = Exec::link(&Link { isa: &*isa, units: &units, data: &[], host: &throw });
            let units: Vec<Unit<'_>> =
                stencil.iter().zip(&funcs).map(|(code, func)| Unit::Stencil(code, &func.signature)).collect();
            let ours = Exec::link(&Link { isa: &*isa, units: &units, data: &[], host: &throw });
            for line in &file.lines {
                ran += 1;
                let tys = returns(&funcs[line.func]);
                let problem = match &line.want {
                    Want::Traps => {
                        let theirs = theirs.values_forked(line.func, &line.args);
                        let ours = ours.values_forked(line.func, &line.args);
                        (!theirs.trapped() || !ours.trapped()).then(|| {
                            format!("a trap, where Cranelift's code gives {theirs:?} and stencil code {ours:?}")
                        })
                    }
                    Want::Returns { equal, values } => {
                        let theirs = theirs.values(line.func, &line.args);
                        let ours = ours.values(line.func, &line.args);
                        if !holds(&tys, *equal, values, &theirs) {
                            Some(format!("Cranelift's code returns {theirs:?}"))
                        } else if !holds(&tys, *equal, values, &ours) {
                            Some(format!("stencil code returns {ours:?}"))
                        } else if !interp::same_all(&tys, &theirs, &ours, Nans::Exact) {
                            Some(format!("Cranelift's code returns {theirs:?} and stencil code {ours:?}: other bits"))
                        } else {
                            None
                        }
                    }
                };
                if let Some(problem) = problem {
                    failures.push(format!(
                        "{}: {problem}\n{}",
                        file.at(line),
                        funcs[line.func].display()
                    ));
                }
            }
        }
        if !skipped.is_empty() {
            eprintln!(
                "{} files run no code on {}:\n{}",
                skipped.len(),
                host::triple(&target),
                skipped.join("\n")
            );
        }
        assert!(ran > 0, "no run line ran as code");
        assert!(
            failures.is_empty(),
            "{} failures in code on {}:\n\n{}",
            failures.len(),
            host::triple(&target),
            failures.join("\n\n")
        );
    }
}
