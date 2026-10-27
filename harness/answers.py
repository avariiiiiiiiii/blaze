#!/usr/bin/env python3
"""answers.py: ra-jit answers rustc's questions (`-vV`, `--print cfg`, cargo's probe of file
names, `target-spec-json` and the rest) without running rustc, byte for byte as the pinned
rustc answers them. This compares the two on every target ra-jit has the answers of, under the
arguments and variables cargo and rust-analyzer ask with (docs/no-rustc.md, step B).

    answers.py           compare ra-jit's answers with rustc's; a difference fails
    answers.py --write   write what rustc answers into ra-jit's data, after a toolchain bump

ra-jit's answers are crates/ra-jit/src/answers/rustc.json, of the toolchain in harness/toolchain.
Another toolchain is asked once per question and its answers are kept, as before. The target specs
depend on the host rustc was built for (x86-64 Linux's links with its own lld): --write keeps
those of the other hosts and writes this host's, so it is run on each host CI has (Linux
x86-64 and AArch64, macOS arm64).
"""
import json, os, shutil, subprocess, sys, tempfile

import norustc

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RA_JIT = os.environ.get("RA_JIT_BIN") or os.path.join(ROOT, "target/release/ra-jit")
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
DATA = os.path.join(ROOT, "crates/ra-jit/src/answers", "rustc.json")
TARGETS = [
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl", "aarch64-apple-darwin", "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc", "x86_64-pc-windows-gnu",
    "wasm32-unknown-unknown", "wasm32-wasip1", "wasm32-wasip2",
]
CRATE_TYPES = ["bin", "lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"]
# What changes the cfgs rustc prints beyond the target's own; ra-jit adds these itself.
GIVEN = ("debug_assertions", "overflow_checks", "ub_checks", "panic=")
PROBE = ["-", "--crate-name", "___", "--print=file-names", "--crate-type", "bin", "--crate-type", "rlib",
         "--crate-type", "dylib", "--crate-type", "cdylib", "--crate-type", "staticlib",
         "--crate-type", "proc-macro", "--print=sysroot", "--print=split-debuginfo",
         "--print=crate-name", "--print=cfg", "-Wwarnings"]
# rust-analyzer's `cargo rustc --print cfg -- -O`, as cargo hands it to rustc.
LIBRARY = ["--crate-name", "word", "--edition=2021", "src/lib.rs", "--error-format=json",
           "--json=diagnostic-rendered-ansi,artifacts,future-incompat", "--crate-type", "lib",
           "--emit=dep-info,metadata,link", "-C", "embed-bitcode=no", "-C", "debuginfo=2",
           "--cfg", 'feature="default"', "--check-cfg", "cfg(docsrs,test)",
           "--check-cfg", "cfg(feature, values(\"default\"))", "-C", "metadata=d4ed28d9543870f2",
           "-C", "extra-filename=-c55bb0b8e8b3f5b4", "--out-dir", "target/debug/deps",
           "-L", "dependency=target/debug/deps", "-O", "--print", "cfg"]


def rustc():
    sysroot = subprocess.run(["rustc", f"+{TOOLCHAIN}", "--print", "sysroot"], capture_output=True,
                             text=True, check=True).stdout.strip()
    return os.path.join(sysroot, "bin", "rustc")


def env(extra):
    clean = {k: v for k, v in os.environ.items()
             if not k.startswith(("RUSTC", "RA_JIT", "MACOSX_", "IPHONEOS_", "SDKROOT"))}
    clean.pop("RUST_TARGET_PATH", None)
    return {**clean, **extra}


def ask(argv, extra=None, stdin=""):
    ran = subprocess.run(argv, capture_output=True, text=True, input=stdin, env=env(extra or {}))
    return ran.returncode, ran.stdout, ran.stderr


def said(rust, args, extra=None, stdin=""):
    code, out, err = ask([rust, *args], extra, stdin)
    assert code == 0 and not err.strip(), (args, code, err)
    return out


def write():
    rust = rustc()
    verbose = said(rust, ["-vV"])
    host = next(line.split(": ", 1)[1] for line in verbose.splitlines() if line.startswith("host: "))
    old = json.load(open(DATA)) if os.path.exists(DATA) else {"targets": {}}
    data = {"version": verbose.splitlines()[0].removeprefix("rustc "),
            "verbose": verbose.replace(f"host: {host}\n", "host: {host}\n"), "targets": {}}
    if old.get("version") != data["version"]:
        old = {"targets": {}}
    nightly = {"RUSTC_BOOTSTRAP": "1"}
    for target in TARGETS:
        t = ["--target", target]
        off = ["-C", "debug-assertions=off", "-C", "overflow-checks=off"]
        stable = said(rust, ["--print", "cfg", *t, *off]).splitlines()
        more = said(rust, ["--print", "cfg", *t, *off], nightly).splitlines()
        panic = next(line for line in stable if line.startswith("panic="))
        files = {}
        for kind in CRATE_TYPES:
            code, out, err = ask([rust, "-", "--crate-name", "___", "--print=file-names", "--crate-type", kind, *t])
            files[kind] = out.strip() if code == 0 and not err.strip() else None
        data["targets"][target] = {
            "cfg": [line for line in stable if not line.startswith(GIVEN)],
            "nightly_cfg": [line for line in more if line not in stable and not line.startswith(GIVEN)],
            "panic": panic.split("=", 1)[1].strip('"'),
            "split_debuginfo": said(rust, ["--print", "split-debuginfo", *t]).splitlines(),
            "file_names": files,
            "target_spec_json": {
                **old["targets"].get(target, {}).get("target_spec_json", {}),
                host: said(rust, ["-Z", "unstable-options", "--print", "target-spec-json", *t], nightly),
            },
        }
    os.makedirs(os.path.dirname(DATA), exist_ok=True)
    with open(DATA, "w") as out:
        json.dump(data, out, indent=1, sort_keys=True)
        out.write("\n")
    print(f"wrote what rustc {data['version']} on {host} answers for {len(TARGETS)} targets to {DATA}")


def questions():
    """(rustc's executable, arguments, variables, standard input) for every question compared:
    of the toolchain rustup installed, and of a copy laid out as norustc.py lays its out."""
    rust = rustc()
    for args, variables, stdin in about_rustc():
        yield rust, args, variables, stdin
    work = tempfile.mkdtemp(prefix="ra-jit-answers-", dir="/tmp" if sys.platform == "darwin" else None)
    copy = os.path.join(norustc.counted_toolchain(work, os.path.join(work, "runs.log")), "bin", "rustc")
    for args in (["--print", "sysroot"], ["--print=target-libdir"], ["-vV"], PROBE):
        yield copy, args, {}, ""
    shutil.rmtree(work, ignore_errors=True)


def about_rustc():
    """(arguments, variables, standard input) for every question compared."""
    nightly = {"RUSTC_BOOTSTRAP": "1"}
    yield ["-vV"], {}, ""
    yield ["-V"], {}, ""
    yield ["--version"], {}, ""
    yield ["--version", "--verbose"], {}, ""
    yield PROBE, {}, ""
    yield LIBRARY, {}, ""
    yield LIBRARY, nightly, ""
    for target in [None, *TARGETS]:
        t = ["--target", target] if target else []
        yield ["--print", "sysroot", *t], {}, ""
        yield ["--print=target-libdir", *t], {}, ""
        yield ["--print", "split-debuginfo", *t], {}, ""
        yield [*PROBE[:4], *t, *PROBE[4:]], {}, ""
        yield ["-Z", "unstable-options", *t, "--print", "target-spec-json"], nightly, ""
        for variables in ({}, nightly):
            yield ["--print", "cfg", *t], variables, ""
            yield ["-O", *t, "--print", "cfg"], variables, ""
            yield ["--print", "cfg", "-C", "opt-level=3", "-C", "panic=abort", *t], variables, ""
            yield ["--print", "cfg", "-C", "opt-level=z", "-C", "debug-assertions=yes", *t], variables, ""
            yield ["--print", "cfg", "-O", "-C", "overflow-checks=on", *t], variables, ""
            yield ["--print", "cfg", "--cfg", 'feature="b"', "--cfg", "foo", "--cfg", 'feature="a"',
                   "--cfg", 'feature="b"', *t], variables, ""
        # What RUSTFLAGS of no bearing on the answers add, as count.py's `-C link-dead-code`.
        flags = ["-C", "link-dead-code", "-C", "force-frame-pointers=yes", "-Crpath", "-C", "force-unwind-tables=no"]
        yield [*flags, "-O", *t, "--print", "cfg"], {}, ""
        yield [*PROBE[:4], *flags, *t, *PROBE[4:]], {}, ""
        yield ["-", "--crate-name", "x", "--print=file-names", "--crate-type", "lib", "--crate-type", "bin",
               "--crate-type", "lib", "--print=crate-name", *t], {}, ""


def check():
    rust = rustc()
    if not os.path.exists(DATA):
        print(f"ra-jit has no answers of rustc {TOOLCHAIN}: answers.py --write writes them")
        sys.exit(1)
    data = json.load(open(DATA))
    host = next(line.split(": ", 1)[1] for line in said(rust, ["-vV"]).splitlines() if line.startswith("host: "))
    home = tempfile.mkdtemp(prefix="ra-jit-answers-")
    wrong, answered, unknown = [], 0, {}
    for rust, args, variables, stdin in questions():
        want = ask([rust, *args], variables, stdin)
        if want[0] != 0 or want[2].strip():
            continue
        if "target-spec-json" in args:
            target = args[args.index("--target") + 1] if "--target" in args else host
            if host not in data["targets"].get(target, {}).get("target_spec_json", {}):
                # rustc is asked, once per toolchain.
                unknown[target] = want[1]
                continue
        got = ask([RA_JIT, "rustc-wrapper", rust, *args],
                  {**variables, "RA_JIT_HOME": home, "RA_JIT_ANSWERS": "own"}, stdin)
        answered += 1
        if got[:2] != want[:2]:
            wrong.append((args, variables, want[1], got))
    for args, variables, want, (code, out, err) in wrong:
        print(f"DIFFERS: {' '.join(args)} {variables or ''}: exit {code}\n  rustc:  {want!r}\n  ra-jit: {out!r} {err.strip()}")
    print(f"{answered - len(wrong)} of {answered} questions answered by ra-jit as rustc {TOOLCHAIN} answers them")
    if unknown:
        print(f"ra-jit has not the target specs of rustc on {host}, which rustc is asked for; "
              f"answers.py --write on this host adds them:\n{json.dumps(unknown)}")
    if wrong:
        sys.exit(1)


if __name__ == "__main__":
    write() if "--write" in sys.argv else check()
