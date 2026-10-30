"""corpus.py: real crates built, run and tested by blaze and by plain cargo with the pinned
nightly, and what each program prints and how each test ends compared (docs/v2/plan/lanes/proof.md,
"Differential against rustc").

    uv run --frozen python harness/v2/corpus.py '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}, "out": "/tmp/corpus.json"}'
    uv run --frozen python harness/v2/corpus.py '{"engine": {...}, "out": "...", "projects": ["crates.io"], "crates": ["itoa"]}'
    uv run --frozen python harness/v2/corpus.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    projects  which of PROJECTS run; empty: all of them
    crates    which crates of corpus.json the project `crates.io` runs, by name; empty: all
    commit    the commit of this repository the project `blaze` is (HEAD)
    jobs      how many projects run at once (2), each with a daemon and a home of its own
    keep      keep each project's checkout and both target directories after it ran
    cache     where cargo's `test` results are kept across runs ("~/.cache/blaze-v2-proof"; ""
              keeps none), by everything they depend on: the toolchain (`rustc -vV`, `cargo
              -vV`), the platform, the checkout's tree after `cargo fetch` (engine.tree_digest,
              Cargo.lock in it), the step's arguments, cargo's configuration files outside the
              checkout and the environment, the run's own paths in it written as `{ws}` and
              `{root}`; a rerun runs cargo's tests only of what changed

The projects:

    blaze      this repository at `commit`, exported with `git archive`: `blaze-engine`'s
               programs built, `blaze --version` and `blaze-engine --help` run from a directory
               outside any workspace, and the tests of the crates in BLAZE_TESTS
    nushell, ripgrep, helix
               pinned, cloned and run as blaze_cli.PROJECTS says (`build`, `run`, `env`) and
               tested as bench.PROJECTS says (`test`); the two have to pin the same commit
    crates.io  each crate of corpus.json: its `.crate` from static.crates.io, checked against
               the sha256 the sparse index (index.crates.io) lists for that version when it
               is fetched, unpacked, and its own tests run

blaze does each step through the engine into the workspace's `target`; plain cargo, the
reference, into `target-ref` (engine.reference_env). Each (project, step) is one outcome:

    pass   the same: a build's exit status; a program's exit status and what it printed
           (blaze_cli.program_output); every test's outcome and the totals (blaze_cli.test_results)
    fail   they differ, one side's build fails and the other's does not, or the engine says the
           step was not blaze's (`not_blazes`)
    error  the harness could not say: a fetch or a checksum failed, a command timed out, the
           engine gave an answer it could not read, or the reference did not get
           far enough to compare with (cargo's build or test build fails)
    unmeasured
           the engine has no answer to a step (`engine.Unmeasurable`)

A project stops at a build step that does not pass. Each project's dependencies resolve once,
by `cargo fetch`, into the one `Cargo.lock` both sides build with; the result's
`extra.locks` names its sha256 per project, so two runs that resolved differently show it.
`selftest` runs the checks of the parts that
need neither blaze nor the network.
"""

import concurrent.futures
import hashlib
import importlib
import io
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast, get_args

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine, report

blaze_cli: Any = importlib.import_module("blaze_cli")

SCRIPT = "corpus.py"
Name = Literal["blaze", "nushell", "ripgrep", "helix", "crates.io"]
Command = Literal["build", "run", "test", "exec"]
PINNED: tuple[Name, ...] = ("nushell", "ripgrep", "helix")
BLAZE_TESTS = ("syntax-tree", "blaze-protocol", "stdx", "intern")
INDEX = "https://index.crates.io/"
DOWNLOAD = "https://static.crates.io/crates/{name}/{name}-{version}.crate"
FETCH_SECS = 120
EXEC_SECS = 600


@dataclass(frozen=True)
class Crate:
    name: str
    version: str


@dataclass(frozen=True)
class Corpus:
    """corpus.json."""

    about: str
    crates: tuple[Crate, ...]


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    projects: tuple[Name, ...] = ()
    crates: tuple[str, ...] = ()
    commit: str = "HEAD"
    jobs: int = 2
    keep: bool = False
    cache: str = "~/.cache/blaze-v2-proof"


@dataclass(frozen=True)
class Step:
    """One command both sides run. `exec` runs a program the build made, `args` being its name
    under `target*/debug` and its arguments."""

    label: str
    command: Command
    args: tuple[str, ...] = ()


@dataclass(frozen=True)
class Program:
    """A workspace both sides build: `prepare` puts its checkout at the path it is given."""

    name: str
    dir: str
    prepare: Callable[[Path], None]
    steps: tuple[Step, ...]
    env: dict[str, str] = field(default_factory=dict[str, str])


@dataclass(frozen=True)
class Found:
    """An outcome, made in a worker and added to the report by the main thread."""

    name: str
    verdict: report.Verdict
    detail: str = ""
    numbers: dict[str, report.Number] = field(default_factory=dict[str, report.Number])


@dataclass(frozen=True)
class Tested:
    """What `blaze_cli.test_results` reads of a test run: with `kept`, the tests `blaze test`
    passed without running them again (its `tests.ndjson`)."""

    returncode: int
    stdout: str
    kept: list[str]


def load_corpus(path: Path = HERE / "corpus.json") -> Corpus:
    return parse(Corpus, str(path), SCRIPT)


def git(*args: str) -> str:
    done = subprocess.run(["git", "-C", str(engine.ROOT), *args], capture_output=True, text=True,
                          env={**os.environ, "TZ": "UTC0"})
    if done.returncode != 0:
        raise engine.Failed(f"`git {' '.join(args)}` exited {done.returncode}: {done.stderr.strip()}")
    return done.stdout.strip()


def export(sha: str, dest: Path) -> None:
    """The tree of commit `sha` of this repository at `dest`, as `git archive` gives it."""
    dest.mkdir(parents=True)
    with subprocess.Popen(["git", "-C", str(engine.ROOT), "archive", "--format=tar", sha],
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE) as proc:
        if proc.stdout is None or proc.stderr is None:
            raise engine.Failed("git archive: no pipe")
        with tarfile.open(fileobj=proc.stdout, mode="r|") as tar:
            tar.extractall(dest, filter="data")
        said = proc.stderr.read().decode(errors="replace")
    if proc.returncode != 0:
        raise engine.Failed(f"`git archive {sha}` exited {proc.returncode}: {said.strip()}")


def blaze_program(commit: str) -> Program:
    sha = git("rev-parse", "--verify", f"{commit}^{{commit}}")
    date = git("show", "-s", "--date=format-local:%Y-%m-%d", "--format=%cd", sha)
    tests = tuple(word for crate in BLAZE_TESTS for word in ("-p", crate))
    steps = (
        Step("build", "build", ("-p", "blaze-engine", "--bins")),
        Step("blaze --version", "exec", ("blaze", "--version")),
        Step("blaze-engine --help", "exec", ("blaze-engine", "--help")),
        Step("test", "test", tests),
    )
    # The export has no `.git`: its build script is told the commit, as release.yaml tells it.
    env = {"BLAZE_GIT_SHA": sha[:7], "BLAZE_GIT_DATE": date}
    return Program(f"blaze@{sha[:12]}", "blaze", lambda ws: export(sha, ws), steps, env)


def pinned_program(name: Name) -> Program:
    bench: Any = importlib.import_module("bench")
    spec: dict[str, Any] = blaze_cli.PROJECTS[name]
    timed: dict[str, Any] = bench.PROJECTS[name]
    if spec["rev"] != timed["rev"]:
        raise engine.Failed(f"{name}: blaze_cli.py pins {spec['rev']} and bench.py {timed['rev']}")
    build = tuple[str, ...](spec.get("build") or ())
    run = tuple[str, ...](spec.get("run") or ())
    steps = [Step("build", "build", build)]
    if run:
        steps.append(Step("run", "run", (*build, "--", *run)))
    steps.append(Step("test", "test", tuple[str, ...](timed.get("test", spec.get("test", ())))))

    def prepare(ws: Path) -> None:
        try:
            source = str(blaze_cli.fetch(name, spec))
        except (AssertionError, subprocess.CalledProcessError) as e:
            raise engine.Failed(f"{name}: fetching {spec['repo']} at {spec['tag']}: {e}") from None
        shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref"))

    env = dict[str, str](spec.get("env") or {})
    return Program(f"{name}@{spec['tag']}", name, prepare, tuple(steps), env)


def index_path(name: str) -> str:
    """Where the sparse index keeps the versions of crate `name`."""
    low = name.lower()
    if len(low) <= 2:
        return f"{len(low)}/{low}"
    if len(low) == 3:
        return f"3/{low[0]}/{low}"
    return f"{low[:2]}/{low[2:4]}/{low}"


def listed_cksum(index: str, name: str, version: str) -> str:
    """The sha256 the index file `index` of crate `name` lists for `version`."""
    for line in index.splitlines():
        if not line.strip():
            continue
        entry: Any = json.loads(line)
        if not isinstance(entry, dict) or entry.get("vers") != version:
            continue
        cksum: Any = entry.get("cksum")
        if not isinstance(cksum, str) or not re.fullmatch(r"[0-9a-f]{64}", cksum):
            raise engine.Failed(f"{name} {version}: the index lists the checksum {cksum!r}")
        return cksum
    raise engine.Failed(f"{name}: the index lists no version {version}")


def verified(data: bytes, cksum: str, what: str) -> bytes:
    got = hashlib.sha256(data).hexdigest()
    if got != cksum:
        raise engine.Failed(f"{what}: its sha256 is {got}, and the index lists {cksum}")
    return data


def download(url: str) -> bytes:
    try:
        with urllib.request.urlopen(url, timeout=FETCH_SECS) as answer:
            body: bytes = answer.read()
            return body
    except (urllib.error.URLError, OSError) as e:
        raise engine.Failed(f"fetching {url}: {e}") from None


def fetch_crate(crate: Crate, cache: Path) -> bytes:
    """The `.crate` of `crate`, kept in `cache` once it was verified; verified against the index
    again each time."""
    cksum = listed_cksum(download(INDEX + index_path(crate.name)).decode(), crate.name, crate.version)
    what = f"{crate.name}-{crate.version}.crate"
    kept = cache / what
    if kept.is_file():
        return verified(kept.read_bytes(), cksum, str(kept))
    data = verified(download(DOWNLOAD.format(name=crate.name, version=crate.version)), cksum, what)
    cache.mkdir(parents=True, exist_ok=True)
    part = kept.with_name(what + f".{os.getpid()}.part")
    part.write_bytes(data)
    part.replace(kept)
    return data


def unpack(data: bytes, top: str, dest: Path) -> None:
    """The `.crate` `data`, every entry of which is under `top/`, unpacked as `dest`."""
    staging = dest.with_name(dest.name + ".unpack")
    shutil.rmtree(staging, ignore_errors=True)
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            stray = [n for n in tar.getnames() if n != top and not n.startswith(top + "/")]
            if stray:
                raise engine.Failed(f"{top}.crate has entries outside {top}/: {stray[:3]}")
            tar.extractall(staging, filter="data")
    except (tarfile.TarError, OSError) as e:
        raise engine.Failed(f"unpacking {top}.crate: {e}") from None
    (staging / top).rename(dest)
    shutil.rmtree(staging)


def crate_program(crate: Crate, cache: Path) -> Program:
    top = f"{crate.name}-{crate.version}"
    return Program(f"crates.io/{crate.name}@{crate.version}", f"c-{top}",
                   lambda ws: unpack(fetch_crate(crate, cache), top, ws), (Step("test", "test"),))


def judge(command: Command, ours: int, theirs: int, differs: str | None, who: str | None,
          compared: bool) -> tuple[report.Verdict, str]:
    """The verdict of a step from its exit statuses, the first difference between what the two
    sides gave (None: none), who did blaze's side when blaze did not (`not_blazes`), and
    whether cargo's side got far enough to compare with (its test programs built and ran)."""
    if who is not None:
        return "fail", f"not blaze's: {who}"
    if command == "build":
        if ours == 0 and theirs == 0:
            return "pass", ""
        if ours != 0 and theirs != 0:
            return "error", f"cargo's build fails too (exit {theirs}): nothing to compare"
        return "fail", (f"blaze's build exits {ours} and cargo's 0" if ours != 0
                        else f"cargo's build exits {theirs} and blaze's 0")
    if not compared:
        return "error", f"cargo's {command} did not get to running anything (exit {theirs}): nothing to compare"
    if differs is not None:
        return "fail", differs
    return "pass", ""


# Set by the shell for its session alone; no build or test reads them to decide anything.
SESSION = frozenset({"PWD", "OLDPWD", "SHLVL", "_", "TERM_SESSION_ID", "TMUX", "TMUX_PANE", "WINDOWID", "STY",
                     "SSH_CLIENT", "SSH_CONNECTION", "SSH_TTY", "SSH_AUTH_SOCK", "XDG_SESSION_ID"})


@dataclass(frozen=True)
class Reference:
    """What cargo's side of a step depends on beyond the step: the result cache, the
    toolchain's identity and the checkout's tree digest (`engine.tree_digest`)."""

    kept: engine.Kept
    toolchain: str
    tree: str


def cargo_configs(ws: Path, env: dict[str, str]) -> str:
    """The cargo configuration files outside the checkout that apply to a build in it (those
    of every directory above it and of CARGO_HOME), each by its path and sha256."""
    home = Path(env.get("CARGO_HOME") or Path.home() / ".cargo")
    found: list[str] = []
    for directory in (*[d / ".cargo" for d in ws.parents], home):
        for name in ("config.toml", "config"):
            path = directory / name
            if path.is_file():
                found.append(f"{path}={engine.digest(path)}")
    return "\n".join(found)


def environment_key(env: dict[str, str], ws: Path, root: Path) -> str:
    """`env` as part of a key: sorted, without SESSION, the checkout's and the work directory's
    paths written `{ws}` and `{root}` so that a rerun elsewhere has the same key."""
    return "\n".join(f"{name}={env[name].replace(str(ws), '{ws}').replace(str(root), '{root}')}"
                     for name in sorted(env) if name not in SESSION)


def kept_ran(entry: object, argv: tuple[str, ...], where: str) -> engine.Ran | None:
    """A kept result of cargo as an `engine.Ran`; None when there is none; a Failed when it is
    not of the shape `cargo_test` writes."""
    if entry is None:
        return None
    fields = cast(dict[str, object], entry) if isinstance(entry, dict) else {}
    code, stdout, seconds = fields.get("returncode"), fields.get("stdout"), fields.get("seconds")
    if not (isinstance(code, int) and isinstance(stdout, str) and isinstance(seconds, (int, float))):
        raise engine.Failed(f"the kept result of cargo {where} is not one corpus.py wrote: {entry!r:.300}")
    return engine.Ran(argv, float(seconds), code, stdout, "")


def cargo_test(name: str, args: tuple[str, ...], ws: Path, root: Path, env: dict[str, str],
               reference: Reference) -> tuple[engine.Ran, bool]:
    """cargo's `test ARGS` in `ws`: kept from an earlier run of the same inputs (see the input's
    `cache`), else run and kept; and whether it was kept. A run that timed out raises, and is
    never kept."""
    argv = ("cargo", "test", *args)
    kept = reference.kept
    key = kept.key(reference.toolchain, sys.platform, platform.machine(), reference.tree, *argv,
                   cargo_configs(ws, env), environment_key(env, ws, root))
    if (known := kept_ran(kept.get(key), argv, str(kept.path(key)))) is not None:
        return known, True
    ran = engine.cargo(name, ["test", *args], ws, env)
    kept.put(key, dict(returncode=ran.returncode, stdout=ran.stdout, seconds=ran.seconds))
    return ran, False


def step_of(eng: engine.Engine, program: Program, step: Step, ws: Path, scratch: Path,
            ours_env: dict[str, str], theirs_env: dict[str, str], reference: Reference) -> Found:
    name = f"{program.name}: {step.label}"
    was_kept = False
    if step.command == "exec":
        program_name, *args = step.args
        cwd = scratch / "cwd"
        cwd.mkdir(exist_ok=True)
        env = {**engine.scrubbed(), **program.env, "BLAZE_HOME": str(scratch / "program-home")}
        ours = engine.run(name, [str(ws / "target" / "debug" / (program_name + engine.EXE)), *args], cwd, env, EXEC_SECS)
        theirs = engine.run(name, [str(ws / "target-ref" / "debug" / (program_name + engine.EXE)), *args], cwd, env,
                            EXEC_SECS)
        who = None
        mine, other = blaze_cli.program_output(ours), blaze_cli.program_output(theirs)
        compared = True
    else:
        tests = ws / "target" / "blaze" / "tests.ndjson"
        if step.command == "test":
            tests.unlink(missing_ok=True)
        ours = eng.build(name, ws, ours_env, step.args, command=step.command)
        who = eng.not_blazes(ours)
        if step.command == "test":
            theirs, was_kept = cargo_test(name, step.args, ws, scratch, theirs_env, reference)
        else:
            theirs = engine.cargo(name, [step.command, *step.args], ws, theirs_env)
        compared = True
        if step.command == "test":
            kept: list[str] = blaze_cli.kept_passes(str(tests))
            mine = blaze_cli.test_results(Tested(ours.returncode, ours.stdout, kept))
            other = blaze_cli.test_results(Tested(theirs.returncode, theirs.stdout, []))
            compared = "test result:" in theirs.stdout
        elif step.command == "run":
            mine, other = blaze_cli.program_output(ours), blaze_cli.program_output(theirs)
        else:
            mine, other = (ours.returncode,), (theirs.returncode,)
    differs: str | None = blaze_cli.difference(mine, other)
    verdict, detail = judge(step.command, ours.returncode, theirs.returncode, differs, who, compared)
    if verdict == "fail" and ours.returncode != 0 and theirs.returncode == 0:
        detail += "\n" + ours.said(1500)
    return Found(name, verdict, detail, {"blaze_s": round(ours.seconds, 2), "cargo_s": round(theirs.seconds, 2),
                                         "cargo_kept": was_kept})


def run_program(eng: engine.Engine, program: Program, root: Path, store: Path, keep: bool, kept: engine.Kept,
                toolchain: str) -> tuple[list[Found], str]:
    """Every outcome of `program`, built in `root`, and the sha256 of the `Cargo.lock` both
    sides built with ("" when it never got that far). Never raises for what the harness could
    not do: that is an outcome too."""
    ws, home = root / "ws", root / "h"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    try:
        program.prepare(ws)
    except (engine.Failed, OSError) as e:
        return [Found(f"{program.name}: fetch", "error", str(e))], ""
    ours_env = eng.env(home, store, extra=program.env)
    theirs_env = engine.reference_env(ws / "target-ref", program.env)
    found: list[Found] = []
    lock = ""
    doing = "cargo fetch"
    try:
        engine.run(f"{program.name}: cargo fetch", ["cargo", "fetch"], ws, theirs_env, 1800, check=True)
        lock = hashlib.sha256((ws / "Cargo.lock").read_bytes()).hexdigest()
        doing = "the checkout's tree digest"
        reference = Reference(kept, toolchain, engine.tree_digest(ws, ("target", "target-ref")))
        for step in program.steps:
            doing = step.label
            done = step_of(eng, program, step, ws, root, ours_env, theirs_env, reference)
            found.append(done)
            if step.command == "build" and done.verdict != "pass":
                break
    except engine.Unmeasurable as e:
        found.append(Found(f"{program.name}: {doing}", "unmeasured", str(e)))
    except engine.Failed as e:
        found.append(Found(f"{program.name}: {doing}", "error", str(e)))
    except OSError as e:
        found.append(Found(f"{program.name}: {doing}", "error", f"{type(e).__name__}: {e}"))
    finally:
        try:
            if home.is_dir():
                eng.stop(ws, ours_env)
        except engine.Failed as e:
            found.append(Found(f"{program.name}: stopping the daemon", "error", str(e)))
        if not keep:
            shutil.rmtree(root, ignore_errors=True)
    return found, lock


def programs_of(args: Input, corpus: Corpus, cache: Path) -> list[Program]:
    names = args.projects or get_args(Name)
    chosen: list[Program] = []
    for name in dict.fromkeys(names):
        if name == "blaze":
            chosen.append(blaze_program(args.commit))
        elif name == "crates.io":
            chosen += [crate_program(c, cache) for c in corpus.crates if not args.crates or c.name in args.crates]
        else:
            chosen.append(pinned_program(name))
    return chosen


def main(args: Input) -> None:
    if args.jobs < 1:
        fail(SCRIPT, f"jobs: at least 1, not {args.jobs}")
    corpus = load_corpus()
    if unknown := sorted(set(args.crates) - {c.name for c in corpus.crates}):
        fail(SCRIPT, f"crates: corpus.json has no {', '.join(unknown)}")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-corpus-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    try:
        programs = programs_of(args, corpus, work / "crates.io")
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), programs=[p.name for p in programs], toolchain=engine.TOOLCHAIN)
    print(f"work: {work}", flush=True)
    store = work / "store"
    locks: dict[str, str] = {}
    kept = engine.Kept(args.cache, "cargo-test")
    try:
        toolchain = engine.toolchain_identity(engine.scrubbed())
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        running = {pool.submit(run_program, eng, p, work / p.dir, store, args.keep, kept, toolchain): p.name
                   for p in programs}
        # In the order of the projects, not the order they end in: one report for one input.
        for done, name in running.items():
            founds, lock = done.result()
            locks[name] = lock
            for found in founds:
                rep.add(found.name, found.verdict, found.detail, **found.numbers)
    rep.extra["locks"] = dict(sorted(locks.items()))
    if not args.keep:
        shutil.rmtree(store, ignore_errors=True)
    rep.finish()


def test_index_path() -> None:
    assert index_path("a") == "1/a"
    assert index_path("cc") == "2/cc"
    assert index_path("syn") == "3/s/syn"
    assert index_path("serde") == "se/rd/serde"
    assert index_path("Inflector") == "in/fl/inflector"


def test_listed_cksum() -> None:
    a, b = "a" * 64, "b" * 64
    index = "\n".join(json.dumps(dict(name="x", vers=v, cksum=c, yanked=False)) for v, c in (("1.0.0", a), ("1.1.0", b)))
    assert listed_cksum(index + "\n", "x", "1.1.0") == b
    for wrong, version in ((index, "2.0.0"), (json.dumps(dict(vers="1.0.0", cksum="nope")), "1.0.0")):
        try:
            listed_cksum(wrong, "x", version)
        except engine.Failed:
            continue
        raise AssertionError(f"{version} of {wrong!r} was taken")


def test_verified() -> None:
    data = b"crate"
    assert verified(data, hashlib.sha256(data).hexdigest(), "x") == data
    try:
        verified(data, "0" * 64, "x")
    except engine.Failed:
        return
    raise AssertionError("a checksum that differs was taken")


def crate_bytes(entries: dict[str, bytes]) -> bytes:
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz") as tar:
        for name, body in entries.items():
            info = tarfile.TarInfo(name)
            info.size = len(body)
            tar.addfile(info, io.BytesIO(body))
    return out.getvalue()


def test_unpack() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dest = Path(tmp) / "ws"
        unpack(crate_bytes({"x-1.0.0/Cargo.toml": b"[package]\n", "x-1.0.0/src/lib.rs": b""}), "x-1.0.0", dest)
        assert (dest / "Cargo.toml").read_bytes() == b"[package]\n" and (dest / "src" / "lib.rs").is_file()
        for entries in ({"x-1.0.0/a": b"", "y/b": b""}, {"x-1.0.0/../../escaped": b""}):
            try:
                unpack(crate_bytes(entries), "x-1.0.0", Path(tmp) / "other")
            except engine.Failed:
                continue
            raise AssertionError(f"{list(entries)} was unpacked")
        assert not (Path(tmp) / "escaped").exists()


def test_judge() -> None:
    assert judge("build", 0, 0, None, None, True) == ("pass", "")
    assert judge("build", 101, 0, "(101,) for (0,)", None, True)[0] == "fail"
    assert judge("build", 0, 101, "(0,) for (101,)", None, True)[0] == "fail"
    assert judge("build", 101, 101, None, None, True)[0] == "error"
    assert judge("build", 0, 0, None, "cargo: no daemon", True) == ("fail", "not blaze's: cargo: no daemon")
    assert judge("test", 101, 101, None, None, False)[0] == "error"
    assert judge("test", 0, 0, "tests differ", None, True) == ("fail", "tests differ")
    assert judge("run", 1, 1, None, None, True) == ("pass", "")


LIBTEST = """running 2 tests
test a ... ok
test b ... FAILED

failures:

---- b stdout ----
boom

failures:
    b

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
"""


def test_test_results() -> None:
    cargo = blaze_cli.test_results(Tested(101, LIBTEST, []))
    assert blaze_cli.difference(blaze_cli.test_results(Tested(101, LIBTEST, [])), cargo) is None
    # `a` kept by blaze (passed as it is now) is the pass cargo ran.
    kept = LIBTEST.replace("test a ... ok\n", "").replace("1 passed", "0 passed")
    assert blaze_cli.difference(blaze_cli.test_results(Tested(101, kept, ["a"])), cargo) is None
    worse = LIBTEST.replace("test a ... ok", "test a ... FAILED").replace("    b\n", "    a\n    b\n")
    assert blaze_cli.difference(blaze_cli.test_results(Tested(101, worse, [])), cargo) is not None


def test_corpus() -> None:
    corpus = load_corpus()
    names = [c.name for c in corpus.crates]
    assert 45 <= len(names) <= 60, len(names)
    assert len(set(names)) == len(names), "a crate is listed twice"
    assert names == sorted(names), "corpus.json's crates are in order of name"
    for crate in corpus.crates:
        assert re.fullmatch(r"[A-Za-z0-9_-]+", crate.name), crate
        assert re.fullmatch(r"\d+\.\d+\.\d+", crate.version), crate


def test_pinned() -> None:
    for name in PINNED:
        program = pinned_program(name)
        assert [s.command for s in program.steps][0] == "build" and program.steps[-1].command == "test", program
    helix = pinned_program("helix")
    assert helix.env.get("HELIX_DISABLE_AUTO_GRAMMAR_BUILD") == "1"


def test_kept_cargo() -> None:
    ws, root = Path("/w/root/ws"), Path("/w/root")
    env = {"CARGO_TARGET_DIR": "/w/root/ws/target-ref", "HOME_ISH": "/w/root/h", "PWD": "/anywhere", "A": "1"}
    key = environment_key(env, ws, root)
    assert key == "A=1\nCARGO_TARGET_DIR={ws}/target-ref\nHOME_ISH={root}/h", key
    assert environment_key({**env, "PWD": "/elsewhere"}, ws, root) == key, "a session variable is in the key"
    assert environment_key({**env, "A": "2"}, ws, root) != key
    elsewhere = {name: value.replace("/w/root", "/v/other") for name, value in env.items()}
    assert environment_key(elsewhere, Path("/v/other/ws"), Path("/v/other")) == key, "a rerun elsewhere misses"
    argv = ("cargo", "test")
    ran = kept_ran({"returncode": 101, "stdout": "test result: FAILED.", "seconds": 2}, argv, "k")
    assert ran is not None and (ran.returncode, ran.stdout, ran.seconds, ran.argv) == (101, "test result: FAILED.", 2.0, argv)
    assert kept_ran(None, argv, "k") is None
    for bad in ([], {"returncode": "0", "stdout": "", "seconds": 1}, {"returncode": 0, "stdout": ""}):
        try:
            kept_ran(bad, argv, "k")
        except engine.Failed:
            continue
        raise AssertionError(f"a kept result of another shape was taken: {bad}")
    with tempfile.TemporaryDirectory() as tmp:
        top = Path(tmp)
        (top / ".cargo").mkdir()
        (top / "a" / "ws").mkdir(parents=True)
        home = top / "cargo-home"
        home.mkdir()
        before = cargo_configs(top / "a" / "ws", {"CARGO_HOME": str(home)})
        (top / ".cargo" / "config.toml").write_text("[build]\nrustflags = []\n")
        after = cargo_configs(top / "a" / "ws", {"CARGO_HOME": str(home)})
        assert before != after and str(top / ".cargo" / "config.toml") in after, after
        (home / "config.toml").write_text("[net]\noffline = true\n")
        assert cargo_configs(top / "a" / "ws", {"CARGO_HOME": str(home)}) != after, "CARGO_HOME's config is missed"


SELFTESTS = (test_kept_cargo, test_index_path, test_listed_cksum, test_verified, test_unpack, test_judge, test_test_results, test_corpus,
             test_pinned)


def selftest() -> int:
    failed = 0
    for test in SELFTESTS:
        try:
            test()
            print(f"ok    {test.__name__}", flush=True)
        except (Exception, SystemExit) as e:
            failed += 1
            print(f"FAIL  {test.__name__}: {type(e).__name__}: {e}", flush=True)
    print(f"{SCRIPT} selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} pass", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        sys.exit(selftest())
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
