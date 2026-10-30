"""cases.py: the front end's cases, each run through the pinned rustc and through blaze and
compared (docs/v2/plan/lanes/frontend/fast-diff.md; section 9 of docs/v2/plan/lanes/frontend/fast.md).

    uv run --frozen python harness/v2/frontend/cases.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/cases.json"}'
    uv run --frozen python harness/v2/frontend/cases.py '{"engine": {...}, "out": "...", "areas": ["nameres"], "filter": "e0425"}'
    uv run --frozen python harness/v2/frontend/cases.py selftest

A case is a `.rs` file directly under `cases/<area>/` (the modules it names with `mod` go in
subdirectories, `m/mod.rs`), whose first line is one of

    //@ expect: accept
    //@ expect: reject E0425, E0433

naming each error rustc gives: its code, a denied lint by name, or an error with no code by the
start of its message in double quotes (as rejections.py's fixed cases). Later lines may carry
compiletest's directives (`//@ edition: 2018`, `//@ compile-flags: ...`), read as rejections.py
reads `tests/ui`; `#![crate_type = "lib"]` makes the case a library, else it is a program with
`fn main`. A case is one unit: no revisions, nothing out of scope.

rustc is asked first: a library with `--emit=dep-info,metadata` (what `cargo check` asks), a
program built whole (`--emit=dep-info,link`, `-C debug-assertions=on` as cargo's dev profile)
and, when it builds, run (RUN_SECS at most): its exit status, what it printed on stdout, and
each panic by thread, place and message. A case whose header rustc does not bear out (or whose
program does not finish) is the case's fault, reported as such, and never goes to blaze. Then
blaze, through the engine, as rejections.py runs a unit: the cases written as packages, up to
`batch` of a kind in a workspace, `blaze check` of a library and `blaze build` of a program,
whose program, when blaze builds it, is run as rustc's was. A case:

    pass   the same outcome: rejected with the same keys and each key's primary spans
           (rejections.py's `verdict`); or accepted, and a program exits alike, prints the same
           and panics alike
    fail   blaze accepts what rustc rejects, rejects what rustc accepts, gives other codes,
           puts them at other places, or its program runs otherwise; or the check was not blaze's
    error  the case is wrong ("case wrong"), or the harness could not say (a timeout, a
           program blaze said it built and did not)

Programs must not print what names their crate (`type_name`, `module_path!`): rustc's crate is
named after the file, blaze's package `u<i>_<j>`.

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    dir       the cases' directory ("": cases/ beside this file)
    areas     the areas (directories of `dir`) to run; empty: every one
    filter    only the cases whose name (`<area>/<file>`) has this in it
    batch     how many cases one workspace has (100)
    whole     one `blaze check --workspace` (or `build`) a workspace, not one a case (false)
    jobs      how many rustc runs, and workspaces, at once (4)
    keep      keep every workspace, not only those of cases that did not pass
    cache     where rustc's answers are kept across runs ("~/.cache/blaze-v2-proof"; "": none),
              as rejections.py keeps them, a program's run with its build
"""

import concurrent.futures
import itertools
import platform
import re
import shutil
import sys
import tempfile
from collections import Counter
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent), str(HERE.parent.parent)]
from typed_input import fail, parse  # noqa: E402
import engine, report, rejections  # noqa: E402
from corpus import PANIC  # noqa: E402

SCRIPT = "cases.py"
CASES = HERE / "cases"
EXPECT = re.compile(r"//@ expect: *(accept|reject +(\S.*))")
BUILD_EMIT = "dep-info,link"
# cargo's dev profile: what decides overflow checks and `debug_assert!`.
DEV = ("-C", "debug-assertions=on")
PROGRAM = "program" + engine.EXE
RUN_SECS = 10
# rustc's builds with their programs' runs; rustc's checks are rejections.KEPT_KIND.
KEPT_BUILDS = "rustc-builds"
AT = re.compile(r"(.*):(\d+):(\d+)")

Category = Literal["same", "case wrong", "accepted what rustc rejects", "rejected what rustc accepts",
                   "other codes", "other spans", "other run", "not blaze's", "error"]
FROM_REJECTIONS: dict[rejections.Category, Category] = {
    "same": "same", "accepted": "accepted what rustc rejects", "other codes": "other codes",
    "other spans": "other spans", "not blaze's": "not blaze's", "error": "error"}


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    dir: str = ""
    areas: tuple[str, ...] = ()
    filter: str = ""
    batch: int = 100
    whole: bool = False
    jobs: int = 4
    keep: bool = False
    cache: str = "~/.cache/blaze-v2-proof"


@dataclass(frozen=True)
class Expect:
    """A case's header: accepted, or rejected with exactly these keys (rejections.header_of's tokens)."""

    accept: bool
    tokens: tuple[str, ...] = ()

    def __str__(self) -> str:
        return "accept" if self.accept else "reject " + ", ".join(shown_key(t) for t in self.tokens)


@dataclass(frozen=True)
class Case:
    """A program compared: its unit (`unit.test` is its name, `<area>/<file>`), and what its
    header says rustc does with it (None: a generated program, of which rustc is the truth)."""

    unit: rejections.Unit
    expect: Expect | None = None

    @property
    def name(self) -> str:
        return self.unit.test


@dataclass(frozen=True)
class Run:
    """A program's run: its exit status, its stdout and its panics (`panics_of`); or why it did
    not finish (`error`), and whether there was a program at all (`started`)."""

    status: int = 0
    stdout: str = ""
    panics: tuple[str, ...] = ()
    error: str | None = None
    started: bool = True


@dataclass(frozen=True)
class Truth:
    """What rustc does with a case: its answer, and its program's run when it built one."""

    ref: rejections.Reference
    run: Run | None = None
    kept: bool = False


@dataclass(frozen=True)
class Compared:
    case: Case
    truth: Truth
    verdict: report.Verdict
    category: Category
    detail: str
    said: rejections.Said | None = None
    run: Run | None = None
    package: str = ""


def shown_key(key: str) -> str:
    """A key as a header writes it: a code or lint as it is, a message's start in quotes."""
    return f'"{key[len(rejections.MESSAGE):]}"' if key.startswith(rejections.MESSAGE) else key


def expect_of(first_line: str) -> Expect | str:
    """The header of a case's first line, or what is wrong with it."""
    m = EXPECT.fullmatch(first_line.rstrip())
    if not m:
        return "its first line is not `//@ expect: accept` or `//@ expect: reject E0xxx[, E0yyy]`"
    if m[1] == "accept":
        return Expect(True)
    tokens, _ = rejections.header_of(f"//@ rejects: {m[2]}\n")
    if not tokens:
        return f"`//@ expect: {m[1]}` names no error"
    return Expect(False, tokens)


def case_of(name: str, path: Path, text: str, host: frozenset[str]) -> Case | str:
    """The case at `path` named `name`, or what is wrong with it (the case's fault)."""
    expect = expect_of(text.split("\n", 1)[0])
    if isinstance(expect, str):
        return expect
    found = rejections.classify(name, str(path), text, host)
    if found.out:
        return f"out of scope as rejections.py reads it: {found.out[0][1]}"
    if found.passing or len(found.units) != 1:
        return "a case is one unit: no revisions, no `check-pass` or `run-pass`"
    return Case(found.units[0], expect)


def discover(root: Path, areas: Sequence[str], needle: str, host: frozenset[str]) -> tuple[list[Case], list[tuple[str, str]]]:
    """The cases under `root` (of `areas`, every one when empty, whose name has `needle`), and
    each file that is not a case with why."""
    cases: list[Case] = []
    wrong: list[tuple[str, str]] = []
    for area in sorted(p for p in root.iterdir() if p.is_dir() and not p.name.startswith((".", "_"))):
        if areas and area.name not in areas:
            continue
        for path in sorted(area.glob("*.rs")):
            name = f"{area.name}/{path.name}"
            if needle not in name:
                continue
            got = case_of(name, path, path.read_text(errors="replace"), host)
            if isinstance(got, str):
                wrong.append((name, got))
            else:
                cases.append(got)
    missing = sorted(set(areas) - {p.name for p in root.iterdir() if p.is_dir()}) if areas else []
    wrong += [(f"{area}/", f"no area {area} in {root}") for area in missing]
    return cases, wrong


def panics_of(text: str, base: Path, root: Path) -> tuple[str, ...]:
    """Each panic `text` reports, by thread, place (its file placed as rejections.place does)
    and message, sorted: threads panic in any order."""
    found: list[str] = []
    for m in PANIC.finditer(text):
        thread, at, colon, message = m.groups()
        where = AT.fullmatch(at)
        if where:
            at = f"{rejections.place(where[1], base, root)}:{where[2]}:{where[3]}"
        found.append(f"{thread} at {at}: {message}" if colon and message is not None else f"{thread} at {at}")
    return tuple(sorted(found))


def run_program(program: Path, cwd: Path, env: dict[str, str], base: Path, root: Path) -> Run:
    """`program` run in `cwd` with no input; its panics placed against `root` as built in `base`."""
    if not program.is_file():
        return Run(error=f"no program at {program}", started=False)
    cwd.mkdir(parents=True, exist_ok=True)
    try:
        ran = engine.run(f"running {program}", [str(program)], cwd, env, RUN_SECS)
    except engine.Failed as e:
        return Run(error=str(e).splitlines()[0])
    return Run(ran.returncode, ran.stdout, panics_of(ran.stderr, base, root))


def kept_run_of(run: Run | None) -> dict[str, object] | None:
    return None if run is None else dict(status=run.status, stdout=run.stdout, panics=list(run.panics))


def run_kept(entry: object, where: str) -> Run | None:
    """The run kept beside rustc's answer (`kept_run_of`); a Failed when it is of another shape."""
    run = cast(dict[str, object], entry).get("run") if isinstance(entry, dict) else None
    if run is None:
        return None
    fields = cast(dict[str, object], run) if isinstance(run, dict) else {}
    status, stdout, panics = fields.get("status"), fields.get("stdout"), fields.get("panics")
    if not (isinstance(status, int) and isinstance(stdout, str) and isinstance(panics, list)
            and all(isinstance(p, str) for p in cast(list[object], panics))):
        raise engine.Failed(f"the kept run of rustc's program {where} is not one cases.py wrote: {run!r:.300}")
    return Run(status, stdout, tuple(cast(list[str], panics)))


def built_truth(unit: rejections.Unit, scratch: Path, env: dict[str, str], kept: engine.Kept,
                toolchain: str) -> Truth:
    """rustc's build of the program `unit` and its run; kept, as rejections.reference keeps a
    check, while what rustc read is unchanged. Neither a timeout nor a run that did not finish
    is kept."""
    cwd = Path(unit.path).parent
    key = kept.key(toolchain, str(cwd), *rejections.rustc_argv(unit, "{out}", BUILD_EMIT, f"{{out}}/{PROGRAM}", DEV))
    entry = kept.get(key)
    if (known := rejections.kept_reference(entry, env, str(kept.path(key)))) is not None:
        return Truth(known, run_kept(entry, str(kept.path(key))), kept=True)
    with tempfile.TemporaryDirectory(dir=scratch) as out:
        argv = rejections.rustc_argv(unit, out, BUILD_EMIT, str(Path(out) / PROGRAM), DEV)
        try:
            ran = engine.run(f"rustc {unit.label}", argv, cwd, env, rejections.RUSTC_SECS)
        except engine.Failed as e:
            return Truth(rejections.Reference(error=str(e)))
        root = Path(unit.path)
        ref = rejections.reference_of(ran.returncode, ran.stderr, root)
        run = None
        if accepted(ref):
            run = run_program(Path(out) / PROGRAM, scratch / "cwd", env, cwd, root)
        if ref.error is None and (run is None or run.error is None) and (
                read := rejections.what_was_read(Path(out), cwd)) is not None:
            files, variables = read
            kept.put(key, dict(files=[list(f) for f in files], env=[list(v) for v in variables],
                               keys=sorted(ref.keys), out=ref.out, spans=rejections.kept_spans(ref.spans),
                               run=kept_run_of(run)))
    return Truth(ref, run)


def accepted(ref: rejections.Reference) -> bool:
    return not ref.keys and ref.error is None and ref.out == rejections.ACCEPTED


def spans_shown(spans: rejections.Placed) -> str:
    return ", ".join(f"{shown_key(k)} at {s}" for k, s in spans) or "no span"


def rustc_said(truth: Truth) -> str:
    ref = truth.ref
    if ref.error is not None:
        return f"rustc could not be run: {ref.error}"
    if ref.keys:
        return f"rustc rejects it with {', '.join(shown_key(k) for k in sorted(ref.keys))} ({spans_shown(ref.spans)})"
    if not accepted(ref):
        return str(ref.out)
    if truth.run is None:
        return "rustc accepts it"
    if truth.run.error is not None:
        return f"rustc accepts it, and its program does not finish: {truth.run.error}"
    return f"rustc accepts it, and its program exits {truth.run.status}"


def rustc_problem(case: Case, truth: Truth) -> tuple[report.Verdict, Category, str] | None:
    """Why `case` cannot go to blaze: the harness could not ask rustc, rustc gives no answer to
    compare with, or the case's header is not what rustc does (the case's fault)."""
    ref = truth.ref
    if ref.error is not None:
        return "error", "error", rustc_said(truth)
    if not ref.keys and not accepted(ref):
        return "error", "case wrong" if case.expect else "error", rustc_said(truth)
    if truth.run is not None and truth.run.error is not None:
        return "error", "case wrong" if case.expect else "error", rustc_said(truth)
    expect = case.expect
    if expect is None:
        return None
    holds = accepted(ref) if expect.accept else rejections.header_holds(expect.tokens, ref.keys)
    if not holds:
        return "error", "case wrong", f"the case says {expect}; {rustc_said(truth)}"
    return None


def run_difference(theirs: Run, ours: Run) -> str | None:
    """None when blaze's program ran as rustc's did; else the first thing that differs."""
    if ours.error is not None:
        return f"rustc's program exits {theirs.status}; blaze's {ours.error}"
    if ours.status != theirs.status:
        return f"rustc's program exits {theirs.status}, blaze's {ours.status}"
    if ours.panics != theirs.panics:
        p, q = Counter(theirs.panics), Counter(ours.panics)
        return f"panics differ: rustc only {sorted(p - q)[:3]}, blaze only {sorted(q - p)[:3]}"
    if ours.stdout != theirs.stdout:
        a, b = theirs.stdout.splitlines(), ours.stdout.splitlines()
        for line, (x, y) in enumerate(zip(a, b), 1):
            if x != y:
                return f"stdout differs at line {line}: rustc {x!r:.200}, blaze {y!r:.200}"
        return f"stdout differs: rustc prints {len(a)} lines, blaze {len(b)}"
    return None


def judged(truth: Truth, said: rejections.Said, ran: Run | None) -> tuple[report.Verdict, Category, str]:
    """blaze's answer (and its program's run) against rustc's."""
    if said.error is not None:
        return "error", "error", said.error
    if said.who is not None:
        return "fail", "not blaze's", f"not blaze's: {said.who}"
    if truth.ref.keys:
        result, category, detail = rejections.verdict(truth.ref, said)
        return result, FROM_REJECTIONS[category], detail
    if said.keys:
        return "fail", "rejected what rustc accepts", (
            f"rejected what rustc accepts: blaze gives {', '.join(shown_key(k) for k in sorted(said.keys))} "
            f"({spans_shown(said.spans)})")
    if truth.run is None:
        return "pass", "same", "accepted"
    if ran is None or not ran.started:
        return "error", "error", f"blaze said it built the program and there is none: {ran.error if ran else '-'}"
    if (differs := run_difference(truth.run, ran)) is not None:
        return "fail", "other run", differs
    return "pass", "same", f"accepted; the program exits {ran.status}"


class Runner:
    """Compares programs (cases, or fuzz.py's): rustc's side in `jobs` threads with its answers
    kept in `cache`, blaze's in workspaces of `batch` cases under `work`."""

    def __init__(self, eng: engine.Engine, work: Path, jobs: int = 4, batch: int = 100, whole: bool = False,
                 cache: str = "~/.cache/blaze-v2-proof", keep: bool = False):
        self.eng, self.work, self.jobs, self.batch, self.whole, self.keep = eng, work, jobs, batch, whole, keep
        self.env = engine.scrubbed()
        self.toolchain = engine.toolchain_identity(self.env)
        self.checks = engine.Kept(cache, rejections.KEPT_KIND)
        self.builds = engine.Kept(cache, KEPT_BUILDS)
        self.indices = itertools.count()
        self.stopped: list[tuple[str, str]] = []
        self.rustc_runs = self.rustc_kept = 0
        (work / "rustc").mkdir(parents=True, exist_ok=True)

    def truth(self, case: Case) -> Truth:
        if case.unit.kind == "bin":
            return built_truth(case.unit, self.work / "rustc", self.env, self.builds, self.toolchain)
        ref, hit = rejections.reference(case.unit, self.work / "rustc", self.env, self.checks, self.toolchain)
        return Truth(ref, kept=hit)

    def truths(self, cases: Sequence[Case]) -> list[Truth]:
        with concurrent.futures.ThreadPoolExecutor(self.jobs) as pool:
            found = list(pool.map(self.truth, cases))
        hits = sum(t.kept for t in found)
        self.rustc_kept += hits
        self.rustc_runs += len(found) - hits
        return found

    def compare(self, cases: Sequence[Case]) -> list[Compared]:
        """Each case compared, in their order. Names are unique."""
        labels = [c.unit.label for c in cases]
        if len(set(labels)) != len(labels):
            raise engine.Failed(f"two cases of one name among {len(cases)}: {sorted(Counter(labels) - Counter(set(labels)))[:3]}")
        truths = self.truths(cases)
        results: dict[str, Compared] = {}
        sent: list[rejections.Unit] = []
        for case, truth in zip(cases, truths):
            problem = rustc_problem(case, truth)
            if problem is not None:
                results[case.unit.label] = Compared(case, truth, *problem)
            else:
                sent.append(case.unit)
        of = {c.unit.label: (c, t) for c, t in zip(cases, truths)}
        chunks = [(next(self.indices), chunk) for chunk in rejections.batches_of(sent, self.batch)]
        with concurrent.futures.ThreadPoolExecutor(self.jobs) as pool:
            running = [pool.submit(self.blaze_side, i, chunk, of) for i, chunk in chunks]
            for done in running:
                for found in done.result():
                    results[found.case.unit.label] = found
        return [results[label] for label in labels]

    def blaze_side(self, index: int, units: list[rejections.Unit], of: dict[str, tuple[Case, Truth]]) -> list[Compared]:
        command: Literal["check", "build"] = "build" if units[0].kind == "bin" else "check"
        batch = rejections.run_batch(self.eng, index, units, self.work, self.work / "store", self.whole, command)
        if batch.stopped is not None:
            self.stopped.append((batch.ws, batch.stopped))
        found: list[Compared] = []
        for unit, why in batch.out:
            case, truth = of[unit.label]
            found.append(Compared(case, truth, "error", "error", f"cannot be laid out as a package: {why}"))
        ws = Path(batch.ws)
        for unit, said in batch.said:
            case, truth = of[unit.label]
            package = Path(batch.packages[unit.label])
            ran = None
            if truth.run is not None and not said.keys and said.error is None and said.who is None:
                ran = run_program(ws / "target" / "debug" / (package.name + engine.EXE), self.work / "cwd", self.env,
                                  ws, rejections.root_of(ws, package.name, unit))
            result, category, detail = judged(truth, said, ran)
            found.append(Compared(case, truth, result, category, detail, said, ran, str(package)))
        if not self.keep and all(f.verdict == "pass" for f in found):
            shutil.rmtree(ws, ignore_errors=True)
        return found


def main(args: Input) -> None:
    for name, value in (("jobs", args.jobs), ("batch", args.batch)):
        if value < 1:
            fail(SCRIPT, f"{name}: at least 1, not {value}")
    root = Path(args.dir).resolve() if args.dir else CASES
    if not root.is_dir():
        fail(SCRIPT, f"dir: no directory {root}")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    host = rejections.host_names(sys.platform, platform.machine())
    cases, wrong = discover(root, args.areas, args.filter, host)
    if not cases and not wrong:
        fail(SCRIPT, f"no case under {root} for areas {list(args.areas)} and filter {args.filter!r}")
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-cases-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    for name, why in wrong:
        rep.add(name, "error", f"case wrong: {why}")
    try:
        runner = Runner(eng, work, args.jobs, args.batch, args.whole, args.cache, args.keep)
        compared = runner.compare(cases)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    by_area: dict[str, Counter[Category]] = {}
    for c in compared:
        by_area.setdefault(c.case.name.split("/", 1)[0], Counter())[c.category] += 1
        detail = c.detail if c.verdict == "pass" or not c.package else f"{c.detail}\npackage: {c.package}"
        rep.add(c.case.name, c.verdict, detail)
    for ws, why in runner.stopped:
        rep.add(f"stopping the daemon of {ws}", "error", why)
    rep.extra.update(work=str(work), toolchain=engine.TOOLCHAIN, dir=str(root), cases=len(cases),
                     wrong=len(wrong), rustc_runs=runner.rustc_runs, rustc_kept=runner.rustc_kept,
                     areas={area: dict(counts) for area, counts in sorted(by_area.items())})
    if not args.keep:
        shutil.rmtree(work / "store", ignore_errors=True)
        shutil.rmtree(work / "h", ignore_errors=True)
    rep.finish()


HOST = rejections.host_names("linux", "x86_64")
ROOT = Path("/cases/a/t.rs")


def test_expect() -> None:
    assert expect_of("//@ expect: accept") == Expect(True)
    assert expect_of("//@ expect: reject E0425") == Expect(False, ("E0425",))
    assert expect_of("//@ expect: reject E0425, E0433\n") == Expect(False, ("E0425", "E0433"))
    assert expect_of('//@ expect: reject arithmetic_overflow, "cannot find macro"') == Expect(
        False, ("arithmetic_overflow", rejections.MESSAGE + "cannot find macro"))
    for bad in ("// expect: accept", "//@ expect: rejects E0425", "//@ expect: reject", "fn main() {}"):
        assert isinstance(expect_of(bad), str), bad
    assert str(Expect(False, ("E0425", rejections.MESSAGE + "cannot find macro"))) == \
        'reject E0425, "cannot find macro"'
    assert expect_of(f"//@ expect: {Expect(False, ('E0412', rejections.MESSAGE + 'x y'))}") == \
        Expect(False, ("E0412", rejections.MESSAGE + "x y")), "a header written from an Expect reads back"


def test_case_of() -> None:
    lib = case_of("a/l.rs", Path("/c/a/l.rs"), '//@ expect: accept\n#![crate_type = "lib"]\npub fn f() {}\n', HOST)
    assert isinstance(lib, Case) and lib.unit.kind == "lib" and lib.name == "a/l.rs", lib
    old = case_of("a/o.rs", Path("/c/a/o.rs"), "//@ expect: reject E0433\n//@ edition: 2015\nfn main() {}\n", HOST)
    assert isinstance(old, Case) and old.unit.edition == "2015" and old.unit.kind == "bin", old
    assert isinstance(case_of("a/r.rs", Path("/c/a/r.rs"), "//@ expect: accept\n//@ revisions: a b\nfn main() {}\n",
                              HOST), str)
    assert isinstance(case_of("a/x.rs", Path("/c/a/x.rs"), "//@ expect: accept\n//@ aux-build: h.rs\nfn main() {}\n",
                              HOST), str)
    assert isinstance(case_of("a/n.rs", Path("/c/a/n.rs"), "fn main() {}\n//@ expect: accept\n", HOST), str)


def test_tree() -> None:
    """Every file of cases/ is a case, and the areas this lane owns have their cases."""
    cases, wrong = discover(CASES, (), "", HOST)
    assert not wrong, wrong[:5]
    names = Counter(c.name.split("/", 1)[0] for c in cases)
    assert names["nameres"] >= 40, names
    codes = {t for c in cases if c.name.startswith("nameres/") and c.expect and not c.expect.accept
             for t in c.expect.tokens}
    for code in ("E0425", "E0432", "E0433", "E0412", "E0659", "E0603", "E0252", "E0255"):
        assert code in codes, f"no nameres case rejects with {code}"
    editions = {c.unit.edition for c in cases if c.name.startswith("nameres/")}
    assert {"2015", "2018"} <= editions, editions
    assert any(c.expect and c.expect.accept for c in cases if c.name.startswith("nameres/"))


def test_panics() -> None:
    rustc = "thread 'main' panicked at /cases/a/t.rs:4:5:\nattempt to add with overflow\nnote: run with ...\n"
    blaze = "thread 'main' (1234) panicked at crates/u0_0/src/main.rs:4:5:\nattempt to add with overflow\n"
    ws = Path("/ws")
    blaze_root = rejections.root_of(ws, "u0_0", rejections.Unit("a/t.rs", "", None, "bin", "2021"))
    assert panics_of(rustc, ROOT.parent, ROOT) == panics_of(blaze, ws, blaze_root) == (
        "main at <root>:4:5: attempt to add with overflow",)
    std = "thread 'main' panicked at /rustc/abc/library/core/src/option.rs:9:1:\nboom\n"
    assert panics_of(std, ROOT.parent, ROOT) == ("main at <std>/core/src/option.rs:9:1: boom",)
    two = "thread 'b' panicked at t.rs:2:1:\nx\nthread 'a' panicked at t.rs:1:1:\ny\n"
    assert panics_of(two, ROOT.parent, ROOT) == ("a at <root>:1:1: y", "b at <root>:2:1: x")


def truth_of(keys: Sequence[str] = (), spans: rejections.Placed = (), run: Run | None = None,
             out: str | None = None) -> Truth:
    if keys:
        return Truth(rejections.Reference(frozenset(keys), spans=spans))
    return Truth(rejections.Reference(out=out or rejections.ACCEPTED), run)


def test_judged() -> None:
    at = rejections.Span(rejections.ROOT_FILE, 3, 5, 3, 6)
    rejected = truth_of(["E0425"], (("E0425", at),))
    Said = rejections.Said
    assert judged(rejected, Said(frozenset({"E0425"}), spans=(("E0425", at),)), None)[:2] == ("pass", "same")
    assert judged(rejected, Said(), None)[1] == "accepted what rustc rejects"
    assert judged(rejected, Said(frozenset({"E0412"})), None)[1] == "other codes"
    assert judged(rejected, Said(frozenset({"E0425"})), None)[1] == "other spans"
    assert judged(rejected, Said(error="timed out"), None)[:2] == ("error", "error")
    assert judged(rejected, Said(frozenset({"E0425"}), who="cargo"), None)[1] == "not blaze's"
    lib = truth_of()
    assert judged(lib, Said(), None)[:2] == ("pass", "same")
    result, category, detail = judged(lib, Said(frozenset({"E0425"}), spans=(("E0425", at),)), None)
    assert (result, category) == ("fail", "rejected what rustc accepts") and "E0425 at <root>:3:5-3:6" in detail
    program = truth_of(run=Run(0, "1\n2\n"))
    assert judged(program, Said(), Run(0, "1\n2\n"))[:2] == ("pass", "same")
    assert judged(program, Said(), Run(1, "1\n2\n"))[1] == "other run"
    assert "line 2" in judged(program, Said(), Run(0, "1\n3\n"))[2]
    assert "2 lines, blaze 1" in judged(program, Said(), Run(0, "1\n"))[2]
    assert judged(program, Said(), Run(error="took more than 10 s"))[1] == "other run"
    assert judged(program, Said(), Run(error="no program", started=False))[:2] == ("error", "error")
    assert judged(program, Said(), None)[:2] == ("error", "error")
    panicked = truth_of(run=Run(101, "", ("main at <root>:1:1: x",)))
    assert judged(panicked, Said(), Run(101, "", ("main at <root>:2:1: x",)))[1] == "other run"


def test_rustc_problem() -> None:
    accept = Case(rejections.Unit("a/t.rs", str(ROOT), None, "bin", "2021"), Expect(True))
    reject = Case(accept.unit, Expect(False, ("E0425",)))
    generated = Case(accept.unit)
    ok = truth_of(run=Run(0, ""))
    rejected = truth_of(["E0425"])
    assert rustc_problem(accept, ok) is None and rustc_problem(reject, rejected) is None
    assert rustc_problem(generated, ok) is None and rustc_problem(generated, rejected) is None
    wrong = rustc_problem(accept, rejected)
    assert wrong is not None and wrong[1] == "case wrong" and "the case says accept; rustc rejects it with E0425" in wrong[2]
    assert (rustc_problem(reject, ok) or ("", "", ""))[1] == "case wrong"
    assert (rustc_problem(Case(accept.unit, Expect(False, ("E0412",))), rejected) or ("", "", ""))[1] == "case wrong"
    more = truth_of(["E0425", "E0433"])
    assert rustc_problem(reject, more) is not None, "a header that leaves out an error rustc gives holds"
    crash = truth_of(out=rejections.CRASHES)
    assert (rustc_problem(accept, crash) or ("", "", ""))[1] == "case wrong"
    assert (rustc_problem(generated, crash) or ("", "", ""))[1] == "error"
    hangs = truth_of(run=Run(error="took more than 10 s"))
    assert (rustc_problem(accept, hangs) or ("", "", ""))[1] == "case wrong"
    assert (rustc_problem(accept, Truth(rejections.Reference(error="timed out"))) or ("", "", ""))[:2] == ("error", "error")


def test_kept_runs() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        kept = engine.Kept(tmp, KEPT_BUILDS)
        run = Run(101, "a\n", ("main at <root>:1:1: x",))
        kept.put("k", dict(files=[], env=[], keys=[], out=rejections.ACCEPTED, spans=[], run=kept_run_of(run)))
        entry = kept.get("k")
        assert rejections.kept_reference(entry, {}, "k") == rejections.Reference(out=rejections.ACCEPTED)
        assert run_kept(entry, "k") == run
        kept.put("n", dict(files=[], env=[], keys=["E0425"], out=None, spans=[], run=None))
        assert run_kept(kept.get("n"), "n") is None
        try:
            run_kept({"run": {"status": "0", "stdout": "", "panics": []}}, "bad")
        except engine.Failed:
            pass
        else:
            raise AssertionError("a kept run of another shape was taken")
    unit = rejections.Unit("a/t.rs", str(ROOT), None, "bin", "2021")
    argv = rejections.rustc_argv(unit, "/o", BUILD_EMIT, f"/o/{PROGRAM}", DEV)
    assert "--emit=dep-info,link" in argv and "debug-assertions=on" in argv and argv[-1] == f"/o/{PROGRAM}"


def test_run_program() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        missing = run_program(root / "nothing", root / "cwd", engine.scrubbed(), root, root / "t.rs")
        assert not missing.started and missing.error, missing
        if sys.platform != "win32":
            script = root / "p"
            script.write_text("#!/bin/sh\necho one\necho \"thread 'main' panicked at t.rs:2:3:\" >&2\n"
                              "echo boom >&2\nexit 101\n")
            script.chmod(0o755)
            ran = run_program(script, root / "cwd", engine.scrubbed(), root, root / "t.rs")
            assert ran == Run(101, "one\n", ("main at <root>:2:3: boom",)), ran


SELFTESTS = (test_expect, test_case_of, test_tree, test_panics, test_judged, test_rustc_problem, test_kept_runs,
             test_run_program)


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
