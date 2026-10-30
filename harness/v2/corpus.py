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
    backends  the dev tier's backends to run every project at, two or more of "stencil" and
              "cranelift" (engine.EngineSpec.dev_backend, which is then "default"); empty:
              one run at the engine's own

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

With `backends`, each project's steps run once per backend, in order, each run with a daemon
home of its own and a clean `target` (cargo's side runs once and is compared with each). Each
outcome above is named `<project> [<backend>]: <step>`, and each (project, step) that every
backend ran is one more, `<project>: <step>: <first> against <other>`, for each backend after
the first:

    pass   the two blaze runs exit alike, panic alike (each `thread '…' panicked at` with its
           message, as a multiset: tests panic in any order) and gave the same output
           (what each comparison with cargo reads, as above)
    fail   any of those differ, named with both backends

A project stops at a build step that does not pass. Each project's dependencies resolve once,
by `cargo fetch`, into the one `Cargo.lock` both sides build with; the result's
`extra.locks` names its sha256 per project, so two runs that resolved differently show it.
`selftest` runs the checks of the parts that
need neither blaze nor the network.
"""

import collections
import concurrent.futures
import dataclasses
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
type Forced = Literal["stencil", "cranelift"]
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
    backends: tuple[Forced, ...] = ()


def refused_backends(args: Input) -> str | None:
    """What is wrong with the input's `backends`, or None."""
    if not args.backends:
        return None
    if len(args.backends) < 2:
        return f"backends: two or more to compare, not {list(args.backends)} (one run at one is engine.dev_backend)"
    if len(set(args.backends)) != len(args.backends):
        return f"backends: each once, not {list(args.backends)}"
    if args.engine.dev_backend != "default":
        return (f"backends {list(args.backends)} with engine.dev_backend {args.engine.dev_backend!r}: "
                "each run's backend is the one `backends` gives it")
    return None


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


@dataclass(frozen=True)
class Outcome:
    """What blaze's side of a step gave, as the backends are compared: its exit status, what
    the comparison with cargo reads of it (`blaze_cli.program_output`, `test_results`, or a
    build's status) and its panics (`panics`)."""

    status: int
    said: object
    panics: tuple[str, ...]


@dataclass(frozen=True)
class CargoSide:
    """cargo's side of a step, run once and compared with each backend's run: what it ran,
    what the comparison reads of it, whether it got far enough to compare with, and whether it
    was kept from an earlier run."""

    ran: engine.Ran
    said: object
    compared: bool
    kept: bool


# A panic as the pinned nightly's std prints it: `thread 'main' panicked at src/main.rs:2:5:`
# then the message on the next line (a thread id in parentheses after the name since 1.88).
PANIC = re.compile(r"^thread '([^']*)'(?: \(\d+\))? panicked at (.*?)(:?)$(?:\n(.*))?", re.M)


def panics(text: str) -> tuple[str, ...]:
    """Each panic `text` reports, by thread, place and message, sorted: test threads panic in
    any order."""
    found: list[str] = []
    for match in PANIC.finditer(text):
        thread, at, colon, message = match.groups()
        found.append(f"{thread} at {at}: {message}" if colon and message is not None else f"{thread} at {at}")
    return tuple(sorted(found))


def said_difference(x: str, a: object, y: str, b: object) -> str:
    """The first thing that differs between what backends `x` and `y` gave (`Outcome.said`)."""
    if isinstance(a, tuple) and isinstance(b, tuple):
        left, right = cast(tuple[object, ...], a), cast(tuple[object, ...], b)
        if len(left) == len(right):
            for p, q in zip(left, right):
                if p != q:
                    return said_difference(x, p, y, q)
    if isinstance(a, collections.Counter) and isinstance(b, collections.Counter):
        p, q = cast(collections.Counter[object], a), cast(collections.Counter[object], b)
        return (f"tests differ: {x} only {sorted((p - q).items(), key=repr)[:3]}, "
                f"{y} only {sorted((q - p).items(), key=repr)[:3]}")
    if isinstance(a, str) and isinstance(b, str):
        pairs = [(p, q) for p, q in zip(a.splitlines(), b.splitlines()) if p != q][:2]
        return f"output differs ({x}, {y}): {pairs or (a[-200:], b[-200:])}"
    return f"{x} gave {a!r:.300} and {y} {b!r:.300}"


def backend_difference(x: str, a: Outcome, y: str, b: Outcome) -> str | None:
    """None when backends `x` and `y` ran a step alike: the same exit status, the same panics
    and the same output; else the first of those that differs."""
    if a.status != b.status:
        return f"{x} exits {a.status} and {y} {b.status}"
    if a.panics != b.panics:
        p, q = collections.Counter(a.panics), collections.Counter(b.panics)
        return f"panics differ: {x} only {sorted(p - q)[:3]}, {y} only {sorted(q - p)[:3]}"
    if a.said != b.said:
        return said_difference(x, a.said, y, b.said)
    return None


def compare_backends(project: str, steps: tuple[Step, ...],
                     runs: list[tuple[Forced, dict[str, Outcome]]]) -> list[Found]:
    """One outcome per step every backend of `runs` ran, comparing each backend after the
    first with the first. A step a backend did not get to (its build failed or the harness
    could not do it) is said by that run's own outcomes, not compared."""
    if len(runs) < 2:
        return []
    (first, base), rest = runs[0], runs[1:]
    found: list[Found] = []
    for step in steps:
        for other, ran in rest:
            if step.label not in base or step.label not in ran:
                continue
            differs = backend_difference(first, base[step.label], other, ran[step.label])
            name = f"{project}: {step.label}: {first} against {other}"
            found.append(Found(name, "fail", differs) if differs is not None else Found(name, "pass"))
    return found


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


def cargo_side(name: str, step: Step, ws: Path, scratch: Path, theirs_env: dict[str, str],
               reference: Reference) -> CargoSide:
    """cargo's side of a build, run or test step (an `exec` is run by `step_of`)."""
    if step.command == "test":
        ran, was_kept = cargo_test(name, step.args, ws, scratch, theirs_env, reference)
        said = blaze_cli.test_results(Tested(ran.returncode, ran.stdout, []))
        return CargoSide(ran, said, "test result:" in ran.stdout, was_kept)
    ran = engine.cargo(name, [step.command, *step.args], ws, theirs_env)
    said = blaze_cli.program_output(ran) if step.command == "run" else (ran.returncode,)
    return CargoSide(ran, said, True, False)


def step_of(eng: engine.Engine, program: Program, step: Step, ws: Path, scratch: Path,
            ours_env: dict[str, str], theirs_env: dict[str, str], reference: Reference,
            cargo_sides: dict[str, CargoSide], label: str) -> tuple[Found, Outcome]:
    """The outcome of `step` named `label: <step>`, and what blaze's side gave. cargo's side
    is taken from `cargo_sides` when an earlier run of the project (another backend's) ran it,
    and kept there otherwise."""
    name = f"{label}: {step.label}"
    theirs = cargo_sides.get(step.label)
    if step.command == "exec":
        program_name, *args = step.args
        cwd = scratch / "cwd"
        cwd.mkdir(exist_ok=True)
        env = {**engine.scrubbed(), **program.env, "BLAZE_HOME": str(scratch / "program-home")}
        ours = engine.run(name, [str(ws / "target" / "debug" / (program_name + engine.EXE)), *args], cwd, env, EXEC_SECS)
        if theirs is None:
            ran = engine.run(name, [str(ws / "target-ref" / "debug" / (program_name + engine.EXE)), *args], cwd, env,
                             EXEC_SECS)
            theirs = CargoSide(ran, blaze_cli.program_output(ran), True, False)
        who = None
        mine = blaze_cli.program_output(ours)
    else:
        tests = ws / "target" / "blaze" / "tests.ndjson"
        if step.command == "test":
            tests.unlink(missing_ok=True)
        ours = eng.build(name, ws, ours_env, step.args, command=step.command)
        who = eng.not_blazes(ours)
        if theirs is None:
            theirs = cargo_side(name, step, ws, scratch, theirs_env, reference)
        if step.command == "test":
            kept: list[str] = blaze_cli.kept_passes(str(tests))
            mine = blaze_cli.test_results(Tested(ours.returncode, ours.stdout, kept))
        elif step.command == "run":
            mine = blaze_cli.program_output(ours)
        else:
            mine = (ours.returncode,)
    cargo_sides[step.label] = theirs
    differs: str | None = blaze_cli.difference(mine, theirs.said)
    verdict, detail = judge(step.command, ours.returncode, theirs.ran.returncode, differs, who, theirs.compared)
    if verdict == "fail" and ours.returncode != 0 and theirs.ran.returncode == 0:
        detail += "\n" + ours.said(1500)
    found = Found(name, verdict, detail, {"blaze_s": round(ours.seconds, 2), "cargo_s": round(theirs.ran.seconds, 2),
                                          "cargo_kept": theirs.kept})
    return found, Outcome(ours.returncode, mine, panics(ours.stdout + "\n" + ours.stderr))


def unfinished(label: str, doing: str, e: Exception) -> Found:
    """The outcome of a step the harness could not finish."""
    if isinstance(e, engine.Unmeasurable):
        return Found(f"{label}: {doing}", "unmeasured", str(e))
    if isinstance(e, engine.Failed):
        return Found(f"{label}: {doing}", "error", str(e))
    return Found(f"{label}: {doing}", "error", f"{type(e).__name__}: {e}")


def run_steps(eng: engine.Engine, program: Program, ws: Path, root: Path, home: Path, store: Path,
              theirs_env: dict[str, str], reference: Reference, cargo_sides: dict[str, CargoSide],
              label: str) -> tuple[list[Found], dict[str, Outcome]]:
    """Every step of `program` through `eng` with its daemon at `home`, until a build step
    that does not pass: their outcomes named `label: <step>`, and what blaze gave by step."""
    ours_env = eng.env(home, store, extra=program.env)
    found: list[Found] = []
    ran: dict[str, Outcome] = {}
    doing = ""
    try:
        for step in program.steps:
            doing = step.label
            done, outcome = step_of(eng, program, step, ws, root, ours_env, theirs_env, reference, cargo_sides, label)
            found.append(done)
            ran[step.label] = outcome
            if step.command == "build" and done.verdict != "pass":
                break
    except (engine.Unmeasurable, engine.Failed, OSError) as e:
        found.append(unfinished(label, doing, e))
    finally:
        try:
            if home.is_dir():
                eng.stop(ws, ours_env)
        except engine.Failed as e:
            found.append(Found(f"{label}: stopping the daemon", "error", str(e)))
    return found, ran


def run_program(engines: list[tuple[Forced | None, engine.Engine]], program: Program, root: Path, store: Path,
                keep: bool, kept: engine.Kept, toolchain: str) -> tuple[list[Found], str]:
    """Every outcome of `program`, built in `root` once per engine of `engines` (one per
    backend, or one with None), and the sha256 of the `Cargo.lock` both sides built with (""
    when it never got that far). Never raises for what the harness could not do: that is an
    outcome too."""
    ws = root / "ws"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    try:
        program.prepare(ws)
    except (engine.Failed, OSError) as e:
        return [Found(f"{program.name}: fetch", "error", str(e))], ""
    theirs_env = engine.reference_env(ws / "target-ref", program.env)
    found: list[Found] = []
    lock = ""
    doing = "cargo fetch"
    try:
        engine.run(f"{program.name}: cargo fetch", ["cargo", "fetch"], ws, theirs_env, 1800, check=True)
        lock = hashlib.sha256((ws / "Cargo.lock").read_bytes()).hexdigest()
        doing = "the checkout's tree digest"
        reference = Reference(kept, toolchain, engine.tree_digest(ws, ("target", "target-ref")))
        cargo_sides: dict[str, CargoSide] = {}
        runs: list[tuple[Forced, dict[str, Outcome]]] = []
        for at, (backend, eng) in enumerate(engines):
            if at:
                # Each backend's run starts from no output of the one before it.
                shutil.rmtree(ws / "target", ignore_errors=True)
            home = root / ("h" if backend is None else f"h-{backend}")
            label = program.name if backend is None else f"{program.name} [{backend}]"
            done, ran = run_steps(eng, program, ws, root, home, store, theirs_env, reference, cargo_sides, label)
            found += done
            if backend is not None:
                runs.append((backend, ran))
        found += compare_backends(program.name, program.steps, runs)
    except (engine.Unmeasurable, engine.Failed, OSError) as e:
        found.append(unfinished(program.name, doing, e))
    finally:
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


def engines_of(args: Input) -> list[tuple[Forced | None, engine.Engine]]:
    """The engine of each run of a project: one per backend of `backends`, its dev tier forced
    to it, or the input's engine alone."""
    if not args.backends:
        return [(None, engine.Engine.of(args.engine))]
    return [(b, engine.Engine.of(dataclasses.replace(args.engine, dev_backend=b))) for b in args.backends]


def main(args: Input) -> None:
    if args.jobs < 1:
        fail(SCRIPT, f"jobs: at least 1, not {args.jobs}")
    corpus = load_corpus()
    if unknown := sorted(set(args.crates) - {c.name for c in corpus.crates}):
        fail(SCRIPT, f"crates: corpus.json has no {', '.join(unknown)}")
    if wrong := refused_backends(args):
        fail(SCRIPT, wrong)
    try:
        engines = engines_of(args)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-corpus-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    try:
        programs = programs_of(args, corpus, work / "crates.io")
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), programs=[p.name for p in programs], toolchain=engine.TOOLCHAIN,
                     backends=list(args.backends))
    print(f"work: {work}", flush=True)
    store = work / "store"
    locks: dict[str, str] = {}
    kept = engine.Kept(args.cache, "cargo-test")
    try:
        toolchain = engine.toolchain_identity(engine.scrubbed())
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        running = {pool.submit(run_program, engines, p, work / p.dir, store, args.keep, kept, toolchain): p.name
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


def refused(fn: Callable[[], object]) -> str:
    """What `fn` was refused with: a `Failed`, or the exit `typed_input.parse` and `fail` make."""
    try:
        fn()
    except engine.Failed as e:
        return str(e)
    except SystemExit as e:
        return str(e.code)
    raise AssertionError(f"{fn} was taken")


def test_backends_input() -> None:
    base: dict[str, Any] = {"engine": {"kind": "v2", "blaze": sys.executable}, "out": ""}
    plain = parse(Input, json.dumps(base), SCRIPT)
    assert plain.backends == () and plain.engine.dev_backend == "default" and refused_backends(plain) is None
    both = parse(Input, json.dumps({**base, "backends": ["stencil", "cranelift"]}), SCRIPT)
    assert both.backends == ("stencil", "cranelift") and refused_backends(both) is None, both
    assert "backends[1]" in refused(lambda: parse(Input, json.dumps({**base, "backends": ["stencil", "llvm"]}), SCRIPT))
    assert "backends[0]" in refused(lambda: parse(Input, json.dumps({**base, "backends": ["default", "stencil"]}), SCRIPT))
    assert "no field" in refused(lambda: parse(Input, json.dumps({**base, "backend": ["stencil"]}), SCRIPT))
    for wrong, says in ((["stencil"], "two or more"), (["stencil", "stencil"], "each once")):
        got = refused_backends(parse(Input, json.dumps({**base, "backends": wrong}), SCRIPT))
        assert got is not None and says in got, (wrong, got)
    forced = {**base, "engine": {**base["engine"], "dev_backend": "stencil"}, "backends": ["stencil", "cranelift"]}
    got = refused_backends(parse(Input, json.dumps(forced), SCRIPT))
    assert got is not None and "engine.dev_backend" in got, got


def test_dev_backend() -> None:
    assert "engine.dev_backend" in refused(
        lambda: parse(Input, json.dumps({"engine": {"kind": "v2", "blaze": "b", "dev_backend": "gpu"}, "out": ""}), SCRIPT))
    assert "dev_backend" in refused(lambda: engine.EngineSpec("v2", "b", dev_backend=cast(Any, "gpu")))
    assert engine.V2(engine.EngineSpec("v2", sys.executable)).spec.dev_backend == "default"
    for backend in ("stencil", "cranelift"):
        v2 = engine.EngineSpec("v2", sys.executable, dev_backend=backend)
        said = refused(lambda: engine.Engine.of(v2))
        assert "codegen-backend" in said and "requests-backend-axis.md" in said, said
        assert "v1" in refused(lambda: engine.Engine.of(dataclasses.replace(v2, kind="v1")))
        assert "LLVM" in refused(lambda: engine.Engine.of(dataclasses.replace(v2, backend="llvm")))
        args = Input(engine.EngineSpec("v2", sys.executable), "", backends=("stencil", "cranelift"))
        assert "codegen-backend" in refused(lambda: engines_of(args))
    assert [e.spec for _, e in engines_of(Input(engine.EngineSpec("v2", sys.executable), ""))] == [
        engine.EngineSpec("v2", sys.executable)]
    forced = engine.dev_backend_args
    assert forced("default", "build", ("-p", "x")) == ("-p", "x")
    assert forced("stencil", "check", ("-p", "x")) == ("-p", "x")
    assert forced("stencil", "build", ("-p", "x")) == (
        "-p", "x", "-Zcodegen-backend", "--config", 'profile.dev.codegen-backend="stencil"')
    assert forced("cranelift", "run", ("--bin", "a", "--", "--release")) == (
        "--bin", "a", "-Zcodegen-backend", "--config", 'profile.dev.codegen-backend="cranelift"', "--", "--release")
    assert forced("stencil", "test", ())[-1] == 'profile.test.codegen-backend="stencil"'
    for args, profile in ((("--release",), "release"), (("-r", "-p", "x"), "release"), (("--profile", "ci"), "ci"),
                          (("--profile=ci",), "ci"), (("--", "--profile=ci"), "test")):
        assert engine.profile_of("test", args) == profile, (args, engine.profile_of("test", args))


def test_panics() -> None:
    said = ("thread 'main' (12345) panicked at src/main.rs:2:5:\nboom\n"
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n"
            "thread 'b' panicked at tests/t.rs:9:1:\nassertion failed: ok\n"
            "thread 'old' panicked at 'gone', src/lib.rs:1:1\n")
    assert panics(said) == ("b at tests/t.rs:9:1: assertion failed: ok", "main at src/main.rs:2:5: boom",
                            "old at 'gone', src/lib.rs:1:1"), panics(said)
    assert panics("thread 'main' panicked at src/main.rs:2:5:") == ("main at src/main.rs:2:5",)
    assert panics("all fine\n") == ()


def ran_with(returncode: int, stdout: str, stderr: str = "") -> engine.Ran:
    return engine.Ran(("blaze", "run"), 0.1, returncode, stdout, stderr)


def test_compare_backends() -> None:
    def program(returncode: int, stdout: str, stderr: str = "") -> Outcome:
        ran = ran_with(returncode, stdout, stderr)
        return Outcome(returncode, blaze_cli.program_output(ran), panics(stdout + "\n" + stderr))

    def tests(returncode: int, stdout: str) -> Outcome:
        return Outcome(returncode, blaze_cli.test_results(Tested(returncode, stdout, [])), panics(stdout))

    steps = (Step("build", "build"), Step("run", "run"), Step("test", "test"))
    panicked = "thread 'main' panicked at src/main.rs:2:5:\nboom\n"
    same = {"build": Outcome(0, (0,), ()), "run": program(101, "hi\n", panicked), "test": tests(101, LIBTEST)}
    found = compare_backends("p@1", steps, [("stencil", same), ("cranelift", dict(same))])
    assert [(f.name, f.verdict) for f in found] == [
        ("p@1: build: stencil against cranelift", "pass"), ("p@1: run: stencil against cranelift", "pass"),
        ("p@1: test: stencil against cranelift", "pass")], found
    # A panic's thread id is not the program's behavior.
    assert backend_difference("stencil", same["run"], "cranelift",
                              program(101, "hi\n", panicked.replace("'main'", "'main' (7)"))) is None
    other = {
        "build": Outcome(0, (0,), ()),
        "run": program(101, "hi\n", "thread 'main' panicked at src/main.rs:3:5:\nboom\n"),
        "test": tests(101, LIBTEST.replace("test a ... ok", "test a ... FAILED").replace("    b\n", "    a\n    b\n")),
    }
    found = {f.name: f for f in compare_backends("p@1", steps, [("stencil", same), ("cranelift", other)])}
    run = found["p@1: run: stencil against cranelift"]
    assert run.verdict == "fail" and "panics differ" in run.detail and "stencil" in run.detail \
        and "cranelift" in run.detail, run
    test = found["p@1: test: stencil against cranelift"]
    assert test.verdict == "fail" and "tests differ" in test.detail, test
    exits = backend_difference("stencil", same["run"], "cranelift", program(0, "hi\n"))
    assert exits == "stencil exits 101 and cranelift 0", exits
    printed = backend_difference("stencil", program(0, "a\nb\n"), "cranelift", program(0, "a\nc\n"))
    assert printed is not None and "('b', 'c')" in printed and "stencil" in printed, printed
    # A build that failed at one backend stops its run: the steps after it are not compared.
    stopped = {"build": Outcome(101, (101,), ())}
    found_list = compare_backends("p@1", steps, [("stencil", same), ("cranelift", stopped)])
    assert [(f.name, f.verdict) for f in found_list] == [("p@1: build: stencil against cranelift", "fail")], found_list
    assert compare_backends("p@1", steps, [("stencil", same)]) == []


SELFTESTS = (test_kept_cargo, test_index_path, test_listed_cksum, test_verified, test_unpack, test_judge, test_test_results, test_corpus,
             test_pinned, test_backends_input, test_dev_backend, test_panics, test_compare_backends)


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
