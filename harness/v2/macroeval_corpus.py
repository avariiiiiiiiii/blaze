"""macroeval_corpus.py: blaze-macro-eval's corpus of real proc macros, each call expanded by
blaze's evaluator and by the pinned nightly's rustc build of the macro, token for token, and
kept expansions found again only when every input matches (`fastcompile-macros 6` in
v2/requests/fastcompile.md; the tests are crates/blaze-macro-eval/tests/corpus.rs and kept.rs).

    uv run --frozen python harness/v2/macroeval_corpus.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/macroeval.json"}'
    uv run --frozen python harness/v2/macroeval_corpus.py '{"engine": {...}, "out": "...", "wasm": [{"crate": "serde_derive", "path": "/w/serde_derive.wasm"}]}'
    uv run --frozen python harness/v2/macroeval_corpus.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    target    the target directory of `blaze test` of this repository; empty: `WORK/target`
    wasm      blaze's WebAssembly module of a proc-macro crate, each `{"crate", "path"}`, for
              the tests' wasm reference; empty: none, and the tests hold blaze-macro-eval to
              rustc's library (and the fixture's own macros to their native build) alone
    keep      keep the work directory after the run

In order, each step one outcome, the run stopping at the first that does not pass:

    copy        `crates/blaze-macro-eval/tests/corpus/ws` copied as `WORK/workspace`,
                `WORK/scratch` (which kept.rs changes) and `WORK/reference` (cargo's)
    fetch       `cargo fetch --locked` of its registry packages into `WORK/cargo-home`, a
                CARGO_HOME of its own; every package of its Cargo.lock has to be unpacked there
    build ...   `blaze build -p corpus-calls` in each of `workspace` and `scratch` with that
                CARGO_HOME, one daemon for both; each build script's run (std's among them)
                has to be left as `output` beside its `OUT_DIR`, where load.rs reads it
    reference   the pinned nightly's cargo builds `corpus-calls` in `reference` (`--frozen`,
                into `WORK/target-ref`): rustc's library of each of PROC_MACROS, read from
                cargo's `compiler-artifact` messages; one missing is an error naming it
    server      the toolchain's proc-macro server, `<sysroot>/libexec/rust-analyzer-proc-macro-srv`
                of the pinned nightly (`rustc --print sysroot`), which has to answer `--version`
    test        the `BLAZE_MACRO_CORPUS` config (crates/blaze-macro-eval/tests/corpus/config.rs:
                `workspace`, `scratch`, `cargo_home`, `server`, `dylibs`, `wasm`) written to
                `WORK/corpus.json`, then `blaze test -p blaze-macro-eval --test corpus --test
                kept -- --ignored` in this repository, BLAZE_CACHE and BLAZE_PROC_MACRO_CACHE
                unset; then one outcome per test of the two files, by libtest's name

The server is the toolchain's own and is never built from `vendor/proc-macro-srv`: that one
reads the list of macros blaze writes into the libraries it links (crates/blaze-engine/src/
macro_list.rs) where the toolchain's reads rustc's metadata, so it cannot load the libraries
rustc builds, which are the reference. A toolchain without the server is an error naming the
path, as is every tool, fetch, build or library missing: nothing is skipped.

A step's verdict: pass; fail for a finding about blaze (its build fails, a run is not left
where the plan names it, a test fails, is ignored, does not run, or was kept from an earlier
run rather than run); error for what the harness could not do (a copy, a fetch, cargo's
build, the server, a command that timed out). `selftest` checks the parts that need no build:
the input, the config against config.rs's fields, reading cargo's messages and the lock, and
reading what libtest printed.
"""

import json
import os
import re
import shutil
import sys
import tempfile
import time
import tomllib
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse  # noqa: E402

import engine  # noqa: E402
import report  # noqa: E402

SCRIPT = "macroeval_corpus.py"
TESTS = engine.ROOT / "crates" / "blaze-macro-eval" / "tests"
FIXTURE = TESTS / "corpus" / "ws"
CONFIG_RS = TESTS / "corpus" / "config.rs"
TEST_FILES = ("corpus", "kept")
# Every proc-macro crate of the fixture by crate name, as the tests look up `dylibs` and `wasm`.
PROC_MACROS = ("corpus_macros", "corpus_state", "serde_derive", "thiserror_impl", "tokio_macros")
PACKAGE = "corpus-calls"
COPIES = ("workspace", "scratch")
TEST_ARGS = ("-p", "blaze-macro-eval", *(w for t in TEST_FILES for w in ("--test", t)), "--", "--ignored")
UNSET = ("BLAZE_CACHE", "BLAZE_PROC_MACRO_CACHE")
SERVER = "rust-analyzer-proc-macro-srv"
# What the server refuses to start without; the tests' client sets it too (proc-macro-api).
SERVER_ENV = {"RUST_ANALYZER_INTERNALS_DO_NOT_USE": "this is unstable"}
DYLIB = {"linux": ".so", "darwin": ".dylib", "win32": ".dll"}
FETCH_SECS = 1800
BUILD_SECS = 3600
TEST_SECS = 7200
LABEL = "macroeval"


@dataclass(frozen=True)
class Module:
    """blaze's WebAssembly module of a proc-macro crate."""

    crate: str
    path: str


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    target: str = ""
    wasm: tuple[Module, ...] = ()
    keep: bool = False


class Stop(Exception):
    """A step did not pass: its outcome is in the report, and nothing after it can run."""


def checked_wasm(modules: tuple[Module, ...]) -> str | None:
    """What is wrong with the input's `wasm`, or None."""
    seen: set[str] = set()
    for at, module in enumerate(modules):
        if module.crate not in PROC_MACROS:
            return f"wasm[{at}].crate: the fixture has no proc-macro crate {module.crate!r} ({', '.join(PROC_MACROS)})"
        if module.crate in seen:
            return f"wasm[{at}].crate: {module.crate} is named twice"
        seen.add(module.crate)
        if not Path(module.path).is_file():
            return f"wasm[{at}].path: no file {module.path}"
    return None


def config_fields(source: str) -> tuple[list[str], bool]:
    """The fields of config.rs's `Corpus`, in order, and whether it refuses unknown ones."""
    found = re.search(r"((?:#\[[^\n]*\]\n)*)pub\(crate\) struct Corpus \{\n(.*?)\n\}", source, re.S)
    if found is None:
        raise engine.Failed(f"{CONFIG_RS} has no `pub(crate) struct Corpus`")
    attrs, body = found.groups()
    return re.findall(r"^\s*pub\(crate\) (\w+):", body, re.M), "deny_unknown_fields" in attrs


def corpus_config(workspace: Path, scratch: Path, cargo_home: Path, server: Path, dylibs: dict[str, str],
                  wasm: tuple[Module, ...]) -> dict[str, object]:
    """The value of `BLAZE_MACRO_CORPUS`, every path absolute."""
    return dict(workspace=str(workspace.resolve()), scratch=str(scratch.resolve()),
                cargo_home=str(cargo_home.resolve()), server=str(server.resolve()),
                dylibs={name: str(Path(path).resolve()) for name, path in sorted(dylibs.items())},
                wasm={m.crate: str(Path(m.path).resolve()) for m in sorted(wasm, key=lambda m: m.crate)})


def registry_packages(lock: str) -> list[tuple[str, str]]:
    """Each package of the Cargo.lock text `lock` that comes from a registry, by name and version."""
    packages = tomllib.loads(lock).get("package", [])
    return sorted((p["name"], p["version"]) for p in packages if str(p.get("source", "")).startswith("registry+"))


def unfetched(cargo_home: Path, packages: list[tuple[str, str]]) -> list[str]:
    """The packages of `packages` not unpacked under `cargo_home`'s registry sources."""
    roots = [d for d in (cargo_home / "registry" / "src").glob("*") if d.is_dir()]
    return [f"{name} {version}" for name, version in packages
            if not any((root / f"{name}-{version}").is_dir() for root in roots)]


def runs_left(target: Path) -> tuple[int, list[str]]:
    """How many build scripts' runs `target` holds (`build/<package>-<hash>/out`, cargo's first
    layout, as the harness's builds ask), and each whose `output` is not beside its `OUT_DIR`."""
    outs = sorted(d for d in target.glob("**/build/*/out") if d.is_dir())
    return len(outs), [str(d) for d in outs if not (d.parent / "output").is_file()]


def proc_macro_libraries(stdout: str, suffix: str) -> tuple[dict[str, str], list[str]]:
    """rustc's library of each proc-macro crate cargo's `--message-format=json` output says it
    built, by crate name, and what in it could not be read as one."""
    found: dict[str, str] = {}
    wrong: list[str] = []
    for line in stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict) or message.get("reason") != "compiler-artifact":
            continue
        target = message.get("target") or {}
        if "proc-macro" not in (target.get("kind") or []):
            continue
        name = str(target.get("name", "")).replace("-", "_")
        libraries = [f for f in message.get("filenames") or [] if str(f).endswith(suffix)]
        if len(libraries) != 1:
            wrong.append(f"{name}: cargo names {len(libraries)} `{suffix}` files: {message.get('filenames')}")
        elif name in found and found[name] != libraries[0]:
            wrong.append(f"{name}: two libraries, {found[name]} and {libraries[0]}")
        else:
            found[name] = libraries[0]
    return found, wrong


def server_at(sysroot: str) -> Path:
    return Path(sysroot) / "libexec" / (SERVER + engine.EXE)


def expected_tests(sources: dict[str, str]) -> list[str]:
    """The name of every `#[test]` function of each test file's source, in order."""
    return [name for text in sources.values()
            for name in re.findall(r"^#\[test\]\n(?:#\[[^\n]*\]\n)*(?:pub )?fn (\w+)", text, re.M)]


@dataclass(frozen=True)
class Judged:
    name: str
    verdict: report.Verdict
    detail: str = ""


def judge_tests(expected: list[str], stdout: str, kept: list[str]) -> list[Judged]:
    """One verdict per test of `expected`, and one per test that ran and is not in it, from
    what libtest printed and the tests `blaze test` kept rather than ran."""
    results = dict(re.findall(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b", stdout, re.M))
    failed = {n.strip() for block in re.findall(r"^failures:\n((?:    \S.*\n)+)", stdout, re.M)
              for n in block.splitlines() if n.strip()}
    said = dict(re.findall(r"^---- (\S+) stdout ----\n(.*?)(?=^---- |^failures:|\Z)", stdout, re.M | re.S))
    out: list[Judged] = []
    for name in [*expected, *sorted(set(results) - set(expected))]:
        result = "FAILED" if name in failed else results.get(name)
        if name in kept:
            out.append(Judged(name, "fail", "kept from an earlier run, not run: the corpus is run, never taken "
                                            "from a result kept before (no-fallback)"))
        elif result == "ok":
            out.append(Judged(name, "pass"))
        elif result == "FAILED":
            out.append(Judged(name, "fail", f"{name} failed:\n{said.get(name, '').strip()[-3000:]}"))
        elif result == "ignored":
            out.append(Judged(name, "fail", f"{name} was ignored: `-- --ignored` did not reach libtest"))
        else:
            out.append(Judged(name, "fail", f"{name} did not run"))
    return out


def step[T](rep: report.Report, name: str, fn: Callable[[], T]) -> T:
    """`fn()`, its outcome `LABEL/<name>`: pass with its seconds, or the `Failed` that stopped it
    as an error; a `Stop` it raised has its outcome already."""
    try:
        began = time.monotonic()
        value = fn()
    except Stop:
        raise
    except engine.Failed as e:
        rep.add(f"{LABEL}/{name}", "error", str(e))
        raise Stop from None
    except OSError as e:
        rep.add(f"{LABEL}/{name}", "error", f"{type(e).__name__}: {e}")
        raise Stop from None
    rep.add(f"{LABEL}/{name}", "pass", seconds=round(time.monotonic() - began, 2))
    return value


def copy(work: Path) -> dict[str, Path]:
    copies: dict[str, Path] = {}
    for name in (*COPIES, "reference"):
        dest = work / name
        shutil.rmtree(dest, ignore_errors=True)
        shutil.copytree(FIXTURE, dest, symlinks=True, ignore=shutil.ignore_patterns("target"))
        copies[name] = dest
    return copies


def fetch(ws: Path, cargo_home: Path, work: Path) -> None:
    env = engine.reference_env(work / "target-ref", {"CARGO_HOME": str(cargo_home)})
    engine.run("cargo fetch", ["cargo", "fetch", "--locked", "--manifest-path", str(ws / "Cargo.toml")], work, env,
               FETCH_SECS, check=True)
    if missing := unfetched(cargo_home, registry_packages((ws / "Cargo.lock").read_text())):
        raise engine.Failed(f"`cargo fetch --locked` exited 0 and {cargo_home}/registry/src has no "
                            f"{', '.join(missing)}")


def build(rep: report.Report, eng: engine.Engine, name: str, ws: Path, env: dict[str, str]) -> None:
    """`blaze build -p corpus-calls` in `ws`: a fail when it does not build or leaves a run
    where load.rs does not find it."""
    label = f"{LABEL}/build {name}"
    try:
        ran = eng.build(label, ws, env, ("-p", PACKAGE), timeout=BUILD_SECS)
    except engine.Failed as e:
        rep.add(label, "error", str(e))
        raise Stop from None
    who = eng.not_blazes(ran)
    if ran.returncode != 0 or who is not None:
        detail = f"not blaze's: {who}" if who is not None else f"exit {ran.returncode}"
        rep.add(label, "fail", f"`blaze build -p {PACKAGE}` in {ws}: {detail}:\n{ran.said()}")
        raise Stop
    count, left = runs_left(ws / "target")
    if not count or left:
        what = (f"no build script's run under {ws / 'target'}" if not count
                else f"{len(left)} of {count} runs have no `output` beside their OUT_DIR: {', '.join(left[:5])}")
        rep.add(label, "fail", f"`blaze build -p {PACKAGE}` in {ws}: {what}")
        raise Stop
    rep.add(label, "pass", seconds=round(ran.seconds, 2), runs=count)


def reference(ws: Path, cargo_home: Path, work: Path) -> dict[str, str]:
    """rustc's library of each of PROC_MACROS, built by the pinned nightly's cargo."""
    suffix = DYLIB.get(sys.platform)
    if suffix is None:
        raise engine.Failed(f"the libraries of proc macros on {sys.platform}: the harness knows no suffix of them")
    env = engine.reference_env(work / "target-ref", {"CARGO_HOME": str(cargo_home)})
    argv = ["cargo", "build", "--frozen", "-p", PACKAGE, "--message-format=json-render-diagnostics"]
    ran = engine.run("cargo build (the reference)", argv, ws, env, BUILD_SECS, check=True)
    found, wrong = proc_macro_libraries(ran.stdout, suffix)
    if missing := [name for name in PROC_MACROS if name not in found]:
        wrong.append(f"cargo built no library of {', '.join(missing)}")
    wrong += [f"{name}: no file {path}" for name, path in found.items() if not Path(path).is_file()]
    if wrong:
        raise engine.Failed(f"`{' '.join(argv)}` in {ws}: " + "; ".join(wrong))
    return found


def server() -> tuple[Path, str]:
    """The pinned nightly's proc-macro server and what its `--version` says."""
    env = engine.scrubbed()
    sysroot = engine.run("rustc --print sysroot", ["rustc", "--print", "sysroot"], engine.ROOT, env, 300,
                         check=True).stdout.strip()
    at = server_at(sysroot)
    if not at.is_file():
        raise engine.Failed(f"the toolchain {engine.TOOLCHAIN} has no proc-macro server at {at} (its `rustc` "
                            f"component ships it); the vendored one cannot load rustc's libraries")
    said = engine.run(f"{at} --version", [str(at), "--version"], engine.ROOT, {**env, **SERVER_ENV}, 300,
                      check=True).stdout.strip()
    return at, said


def run_tests(rep: report.Report, eng: engine.Engine, work: Path, target: Path, config: Path) -> None:
    home = work / "h-test"
    env = eng.env(home, target=target, extra={"BLAZE_MACRO_CORPUS": str(config)})
    for name in UNSET:
        env.pop(name, None)
    kept_at = target / "blaze" / "tests.ndjson"
    kept_at.unlink(missing_ok=True)
    label = f"{LABEL}/blaze test"
    try:
        ran = eng.build(label, engine.ROOT, env, TEST_ARGS, command="test", timeout=TEST_SECS)
    except engine.Failed as e:
        rep.add(label, "error", str(e))
        return
    finally:
        try:
            if home.is_dir():
                eng.stop(engine.ROOT, env)
        except engine.Failed as e:
            rep.add(f"{LABEL}: stopping the test daemon", "error", str(e))
    who = eng.not_blazes(ran)
    if who is not None:
        rep.add(label, "fail", f"not blaze's: {who}")
    elif ran.returncode != 0:
        rep.add(label, "fail", f"`blaze test {' '.join(TEST_ARGS)}` exited {ran.returncode}:\n{ran.said()}")
    else:
        rep.add(label, "pass", seconds=round(ran.seconds, 2))
    import blaze_cli

    kept: list[str] = blaze_cli.kept_passes(str(kept_at))
    sources = {t: (TESTS / f"{t}.rs").read_text() for t in TEST_FILES}
    for judged in judge_tests(expected_tests(sources), ran.stdout + "\n" + ran.stderr, kept):
        rep.add(f"{LABEL}/{judged.name}", judged.verdict, judged.detail)


def main(args: Input) -> None:
    if wrong := checked_wasm(args.wasm):
        fail(SCRIPT, wrong)
    for needed in (FIXTURE / "Cargo.lock", CONFIG_RS, *(TESTS / f"{t}.rs" for t in TEST_FILES)):
        if not needed.is_file():
            fail(SCRIPT, f"no file {needed}")
    fields, refuses = config_fields(CONFIG_RS.read_text())
    if not refuses or sorted(fields) != sorted(corpus_config(Path(), Path(), Path(), Path(), {}, ())):
        fail(SCRIPT, f"{CONFIG_RS}'s `Corpus` has the fields {fields} (deny_unknown_fields: {refuses}); "
                     "this writes another config")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-macroeval-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    target = Path(args.target).resolve() if args.target else work / "target"
    cargo_home, home, store = work / "cargo-home", work / "h", work / "store"
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), target=str(target), toolchain=engine.TOOLCHAIN,
                     wasm={m.crate: m.path for m in args.wasm})
    print(f"work: {work}", flush=True)
    build_env = eng.env(home, store, extra={"CARGO_HOME": str(cargo_home)})
    try:
        rep.extra["toolchain_identity"] = step(rep, "toolchain", lambda: engine.toolchain_identity(engine.scrubbed()))
        copies = step(rep, "copy", lambda: copy(work))
        step(rep, "fetch", lambda: fetch(copies["workspace"], cargo_home, work))
        try:
            for name in COPIES:
                build(rep, eng, name, copies[name], build_env)
        finally:
            try:
                if home.is_dir():
                    eng.stop(copies["workspace"], build_env)
            except engine.Failed as e:
                rep.add(f"{LABEL}: stopping the daemon", "error", str(e))
        dylibs = step(rep, "reference", lambda: reference(copies["reference"], cargo_home, work))
        at, version = step(rep, "server", server)
        config = corpus_config(copies["workspace"], copies["scratch"], cargo_home, at, dylibs, args.wasm)
        rep.extra.update(server=version, config=config)
        config_at = work / "corpus.json"
        config_at.write_text(json.dumps(config, indent=1) + "\n")
        run_tests(rep, eng, work, target, config_at)
    except Stop:
        pass
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    rep.finish()


def refused(fn: Callable[[], object]) -> str:
    try:
        fn()
    except engine.Failed as e:
        return str(e)
    except SystemExit as e:
        return str(e.code)
    raise AssertionError(f"{fn} was taken")


def test_input() -> None:
    args = parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": ""}', SCRIPT)
    assert (args.work, args.target, args.wasm, args.keep) == ("", "", (), False), args
    assert "no field module" in refused(
        lambda: parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "module": []}', SCRIPT))
    assert "no field wasm[0].file" in refused(lambda: parse(
        Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": "", "wasm": [{"crate": "x", "file": "y"}]}', SCRIPT))
    with tempfile.NamedTemporaryFile(suffix=".wasm") as module:
        given = json.dumps({"engine": {"kind": "v2", "blaze": "b"}, "out": "",
                            "wasm": [{"crate": "serde_derive", "path": module.name}]})
        args = parse(Input, given, SCRIPT)
        assert args.wasm == (Module("serde_derive", module.name),) and checked_wasm(args.wasm) is None, args
        said = checked_wasm((Module("serde", module.name),))
        assert said is not None and "no proc-macro crate 'serde'" in said, said
        said = checked_wasm((Module("serde_derive", module.name), Module("serde_derive", module.name)))
        assert said is not None and "named twice" in said, said
    said = checked_wasm((Module("tokio_macros", "/nowhere/tokio.wasm"),))
    assert said is not None and "no file /nowhere/tokio.wasm" in said, said


def test_config() -> None:
    fields, refuses = config_fields(CONFIG_RS.read_text())
    assert refuses and fields == ["workspace", "scratch", "cargo_home", "server", "dylibs", "wasm"], fields
    with tempfile.TemporaryDirectory() as tmp:
        top = Path(tmp).resolve()
        (top / "m.wasm").write_bytes(b"")
        config = corpus_config(top / "workspace", top / "scratch", top / "cargo-home", top / "srv",
                               {"tokio_macros": str(top / "t.so"), "corpus_macros": str(top / "c.so")},
                               (Module("serde_derive", str(top / "m.wasm")),))
    assert list(config) == fields, list(config)
    assert config["workspace"] == str(top / "workspace") and config["server"] == str(top / "srv"), config
    assert config["dylibs"] == {"corpus_macros": str(top / "c.so"), "tokio_macros": str(top / "t.so")}, config
    assert list(cast(dict[str, str], config["dylibs"])) == ["corpus_macros", "tokio_macros"]
    assert config["wasm"] == {"serde_derive": str(top / "m.wasm")}, config
    assert json.loads(json.dumps(config)) == config
    assert config_fields("#[derive(Deserialize)]\npub(crate) struct Corpus {\n    pub(crate) a: String,\n}") == (["a"], False)
    assert "no `pub(crate) struct Corpus`" in refused(lambda: config_fields("struct Other {}"))


def test_lock() -> None:
    packages = registry_packages((FIXTURE / "Cargo.lock").read_text())
    names = dict(packages)
    assert names.get("serde_derive") == "1.0.228" and names.get("thiserror-impl") == "2.0.18", packages
    assert names.get("tokio-macros") == "2.6.1" and names.get("syn", "").startswith("2."), packages
    assert not {"corpus-calls", "corpus-macros", "corpus-state", "corpus-panics"} & set(names), packages
    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        assert unfetched(home, [("syn", "2.0.117")]) == ["syn 2.0.117"]
        (home / "registry" / "src" / "index.crates.io-1949cf8c6b5b557f" / "syn-2.0.117").mkdir(parents=True)
        assert unfetched(home, [("syn", "2.0.117"), ("quote", "1.0.45")]) == ["quote 1.0.45"]


def test_fixture() -> None:
    """PROC_MACROS is every proc-macro crate the fixture builds `corpus-calls` with."""
    own = []
    for manifest in sorted(FIXTURE.glob("*/Cargo.toml")):
        parsed = tomllib.loads(manifest.read_text())
        if parsed.get("lib", {}).get("proc-macro"):
            own.append(parsed["package"]["name"].replace("-", "_"))
    assert own == ["corpus_macros", "corpus_state"], own
    registry = {name.replace("-", "_") for name, _ in registry_packages((FIXTURE / "Cargo.lock").read_text())}
    assert set(PROC_MACROS) - set(own) <= registry, registry
    assert TEST_ARGS == ("-p", "blaze-macro-eval", "--test", "corpus", "--test", "kept", "--", "--ignored"), TEST_ARGS


def test_runs() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        target = Path(tmp)
        assert runs_left(target) == (0, [])
        for package in ("serde-0123456789abcdef", "thiserror-fedcba9876543210"):
            (target / "debug" / "build" / package / "out").mkdir(parents=True)
        (target / "debug" / "build" / "serde-0123456789abcdef" / "output").write_text("cargo:rustc-cfg=x\n")
        count, left = runs_left(target)
        assert count == 2 and left == [str(target / "debug" / "build" / "thiserror-fedcba9876543210" / "out")], left


def test_libraries() -> None:
    def artifact(name: str, kind: str, files: list[str]) -> str:
        return json.dumps({"reason": "compiler-artifact", "target": {"name": name, "kind": [kind]}, "filenames": files})

    stdout = "\n".join([
        artifact("serde_derive", "proc-macro", ["/t/deps/libserde_derive-1.so"]),
        artifact("syn", "lib", ["/t/deps/libsyn-2.rlib", "/t/deps/libsyn-2.rmeta"]),
        artifact("corpus-macros", "proc-macro", ["/t/deps/libcorpus_macros-3.so"]),
        json.dumps({"reason": "build-finished", "success": True}),
        "warning: not json",
        "{broken",
    ])
    found, wrong = proc_macro_libraries(stdout, ".so")
    assert found == {"serde_derive": "/t/deps/libserde_derive-1.so", "corpus_macros": "/t/deps/libcorpus_macros-3.so"}, found
    assert wrong == [], wrong
    found, wrong = proc_macro_libraries(artifact("tokio_macros", "proc-macro", ["/t/a.dylib"]), ".so")
    assert found == {} and len(wrong) == 1 and "tokio_macros" in wrong[0], wrong
    twice = artifact("x", "proc-macro", ["/a/libx.so"]) + "\n" + artifact("x", "proc-macro", ["/b/libx.so"])
    found, wrong = proc_macro_libraries(twice, ".so")
    assert len(wrong) == 1 and "two libraries" in wrong[0], wrong
    assert server_at("/sys/root") == Path("/sys/root/libexec") / (SERVER + engine.EXE)


LIBTEST = """running 2 tests
test every_call_is_expanded ... ok
test every_call_matches_its_references ... FAILED

failures:

---- every_call_matches_its_references stdout ----
thread 'every_call_matches_its_references' panicked at tests/corpus.rs:9:1:
serde_derive `Serialize` differs

failures:
    every_call_matches_its_references

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 5 filtered out; finished in 0.10s

running 1 test
test a_kept_expansion_is_found_and_one_whose_input_changed_is_not ... ignored

test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
"""


def test_tests() -> None:
    sources = {t: (TESTS / f"{t}.rs").read_text() for t in TEST_FILES}
    expected = expected_tests(sources)
    assert {"every_call_is_expanded", "every_call_matches_its_references", "a_panic_is_its_message",
            "a_kept_expansion_is_found_and_one_whose_input_changed_is_not"} <= set(expected), expected
    assert len(expected) == len(set(expected)), expected
    assert expected_tests({"x": "#[test]\n#[ignore = \"why\"]\nfn a() {}\n#[test]\nfn b() {}\nfn c() {}\n"}) == ["a", "b"]
    wanted = ["every_call_is_expanded", "every_call_matches_its_references",
              "a_kept_expansion_is_found_and_one_whose_input_changed_is_not", "a_panic_is_its_message",
              "every_macro_kind_is_called"]
    judged = {j.name: j for j in judge_tests(wanted, LIBTEST + "test stray ... ok\n", ["every_macro_kind_is_called"])}
    assert list(judged) == [*wanted, "stray"], list(judged)
    assert judged["every_call_is_expanded"].verdict == "pass"
    failed = judged["every_call_matches_its_references"]
    assert failed.verdict == "fail" and "`Serialize` differs" in failed.detail and "failures:" not in failed.detail, failed
    ignored = judged["a_kept_expansion_is_found_and_one_whose_input_changed_is_not"]
    assert ignored.verdict == "fail" and "ignored" in ignored.detail, ignored
    assert judged["a_panic_is_its_message"] == Judged("a_panic_is_its_message", "fail", "a_panic_is_its_message did not run")
    kept = judged["every_macro_kind_is_called"]
    assert kept.verdict == "fail" and "kept from an earlier run" in kept.detail, kept
    assert judged["stray"].verdict == "pass"


SELFTESTS = (test_input, test_config, test_lock, test_fixture, test_runs, test_libraries, test_tests)


def selftest() -> int:
    failed = 0
    for check in SELFTESTS:
        try:
            check()
            print(f"ok    {check.__name__}", flush=True)
        except (Exception, SystemExit) as e:
            failed += 1
            print(f"FAIL  {check.__name__}: {type(e).__name__}: {e}", flush=True)
    print(f"{SCRIPT} selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} pass", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        sys.exit(selftest())
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
