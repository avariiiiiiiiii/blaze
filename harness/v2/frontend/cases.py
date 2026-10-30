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

A case may depend on crates: `//@ aux-crate: dep=dep.rs` (compiletest's directive) names the
file `auxiliary/dep.rs` beside it, which may name its own the same way and take `//@ edition:`
(else 2015) and `#![crate_type = "proc-macro"]`. rustc builds each first, whole, with its lints
capped (`--cap-lints allow`, as cargo builds a registry crate), and gives it to the case with
`--extern`; a dependency rustc rejects is its answer, as cargo stops there. blaze gets each as
a crates.io package vendored in the workspace (a `directory` source replacing crates.io), so
it is a registry crate, as `hir_def::deps::eligible` wants of a crate it serves. A
dependency's files are compared by name: its root file as `<dep NAME>`, any other by its path
under `auxiliary/`.

The cases of `bodies-cache/` are built by blaze twice, with one store and a fresh daemon,
workspace and target: the first checks the dependencies and keeps what it can, the second must
serve each from the store, which it says on stdout with a line per dependency,
`{"reason": "blaze-served", "package": P, "outcome": W, "bodies": W, "why": null | "..."}`, W one
of `served`, `checked`, `refused`. Both builds are compared with rustc; then each dependency
must have both served, but those a `//@ refused: dep[, dep2]` names, of which one must be
`refused` (the edges where the cache hands over to a check).

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
           puts them at other places, or its program runs otherwise; or the check was not blaze's;
           or a second build did not serve a dependency it should (or served one it should not)
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
import functools
import hashlib
import itertools
import json
import platform
import re
import shutil
import sys
import tempfile
from collections import Counter
from collections.abc import Callable, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Literal, cast

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
AUX = re.compile(r"//@ *aux-crate: *(.*)")
AUX_VALUE = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)=([^\s=]+\.rs)")
REFUSED = re.compile(r"//@ *refused: *(.*)")
AUXILIARY = "auxiliary"
# compiletest's directives of an auxiliary crate that change nothing here.
AUX_INERT = frozenset({"force-host", "no-prefer-dynamic"})
# The areas whose cases blaze builds twice, the second served from the store the first filled.
TWICE = frozenset({"bodies-cache"})
VERSION = "0.1.0"
SOURCE = "blaze-cases"
CONFIG = f'[source.crates-io]\nreplace-with = "{SOURCE}"\n\n[source.{SOURCE}]\ndirectory = "vendor"\n'
CHECKSUMS = ".cargo-checksum.json"
SERVED = "blaze-served"
WAYS = ("served", "checked", "refused")
# A proc-macro crate's file, by platform: what rustc's `--extern` takes.
DYLIB = {"darwin": ("lib", ".dylib"), "win32": ("", ".dll")}.get(sys.platform, ("lib", ".so"))

Category = Literal["same", "case wrong", "accepted what rustc rejects", "rejected what rustc accepts",
                   "other codes", "other spans", "other run", "not blaze's", "not served",
                   "served what is refused", "error"]
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
class Dep:
    """A crate a case (or a dependency) names with `//@ aux-crate: NAME=FILE`, FILE under
    `auxiliary/` beside the case as compiletest has it: its extern name (and crate name), its
    root file, its edition, whether it is a proc-macro crate, and the crates it names in turn."""

    name: str
    path: str
    edition: str
    proc_macro: bool = False
    deps: tuple["Dep", ...] = ()


@dataclass(frozen=True)
class Case:
    """A program compared: its unit (`unit.test` is its name, `<area>/<file>`), what its header
    says rustc does with it (None: a generated program, of which rustc is the truth), the crates
    it depends on, and, for an area blaze builds twice (`twice`), the dependencies the second
    build refuses rather than serves."""

    unit: rejections.Unit
    expect: Expect | None = None
    deps: tuple[Dep, ...] = ()
    refused: frozenset[str] = frozenset()
    twice: bool = False

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


def aux_of(text: str) -> tuple[list[tuple[str, str]], list[str], str] | str:
    """The `//@ aux-crate:` pairs (name, file) and the `//@ refused:` names of `text`, and `text`
    with those lines emptied (rejections.classify leaves a test with an auxiliary crate out; the
    rest it reads); or what is wrong with one of them."""
    pairs: list[tuple[str, str]] = []
    refused: list[str] = []
    lines: list[str] = []
    for line in text.split("\n"):
        if m := AUX.fullmatch(line.strip()):
            got = AUX_VALUE.fullmatch(m[1].strip())
            if got is None:
                return f"`{line.strip()}` is not `//@ aux-crate: NAME=FILE.rs`"
            pairs.append((got[1], got[2]))
            line = "//"
        elif m := REFUSED.fullmatch(line.strip()):
            refused += [word for word in re.split(r"[\s,]+", m[1].strip()) if word]
            line = "//"
        lines.append(line)
    return pairs, refused, "\n".join(lines)


def deps_of(pairs: Sequence[tuple[str, str]], aux: Path, seen: tuple[str, ...] = ()) -> tuple[Dep, ...] | str:
    """The crates `pairs` name, each read from `aux` (with those it names), or what is wrong."""
    found: list[Dep] = []
    for name, file in pairs:
        if file.startswith(("/", "..")) or "/../" in file or "\\" in file:
            return f"the dependency {name}={file} is outside {AUXILIARY}/"
        if any(d.name == name for d in found):
            return f"two dependencies are named {name}"
        path = aux / file
        if str(path) in seen:
            return f"its dependencies form a cycle through {file}"
        if not path.is_file():
            return f"names the dependency {name}={file}, and there is no {path}"
        got = aux_of(path.read_text(errors="replace"))
        if isinstance(got, str):
            return f"{AUXILIARY}/{file}: {got}"
        inner, refused, rest = got
        if refused:
            return f"{AUXILIARY}/{file}: `//@ refused:` is a case's, not a dependency's"
        edition = "2015"
        for d in rejections.directives(rest):
            if d.name in AUX_INERT:
                continue
            if d.name != "edition" or d.revisions is not None:
                return f"{AUXILIARY}/{file}: a dependency takes `//@ edition:` and `//@ aux-crate:` alone, not `{d.name}`"
            year = rejections.edition_of(d.value)
            if year is None:
                return f"{AUXILIARY}/{file}: edition `{d.value}`"
            edition = year
        attribute = rejections.CRATE_TYPE.search(rest)
        kind = attribute[1] if attribute else "lib"
        if kind not in ("lib", "rlib", "proc-macro"):
            return f"{AUXILIARY}/{file}: a dependency of crate type {kind}"
        below = deps_of(inner, aux, (*seen, str(path)))
        if isinstance(below, str):
            return below
        found.append(Dep(name, str(path), edition, kind == "proc-macro", below))
    return tuple(found)


def ordered(deps: Sequence[Dep]) -> list[Dep]:
    """Every crate `deps` names, transitively, each once, each after the crates it names."""
    out: list[Dep] = []

    def visit(dep: Dep) -> None:
        for inner in dep.deps:
            visit(inner)
        if dep not in out:
            out.append(dep)

    for dep in deps:
        visit(dep)
    return out


def case_of(name: str, path: Path, text: str, host: frozenset[str]) -> Case | str:
    """The case at `path` named `name`, or what is wrong with it (the case's fault)."""
    expect = expect_of(text.split("\n", 1)[0])
    if isinstance(expect, str):
        return expect
    got = aux_of(text)
    if isinstance(got, str):
        return got
    pairs, refused, rest = got
    deps = deps_of(pairs, path.parent / AUXILIARY)
    if isinstance(deps, str):
        return deps
    every = ordered(deps)
    if len({d.name for d in every}) != len(every):
        return "two of its dependencies have one name"
    area = name.split("/", 1)[0]
    twice = area in TWICE
    if twice and not deps:
        return f"a case of {area} names a dependency (`//@ aux-crate:`) for blaze to serve"
    if refused and not twice:
        return f"`//@ refused:` is for the areas blaze builds twice ({', '.join(sorted(TWICE))})"
    if unknown := sorted(set(refused) - {d.name for d in every}):
        return f"`//@ refused:` names {', '.join(unknown)}, which it does not depend on"
    found = rejections.classify(name, str(path), rest, host)
    if found.out:
        return f"out of scope as rejections.py reads it: {found.out[0][1]}"
    if found.passing or len(found.units) != 1:
        return "a case is one unit: no revisions, no `check-pass` or `run-pass`"
    return Case(found.units[0], expect, deps, frozenset(refused), twice)


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


def same_file(file: str) -> str:
    return file


def panics_of(text: str, base: Path, root: Path, rename: Callable[[str], str] = same_file) -> tuple[str, ...]:
    """Each panic `text` reports, by thread, place (its file placed as rejections.place does,
    then `rename`d) and message, sorted: threads panic in any order."""
    found: list[str] = []
    for m in PANIC.finditer(text):
        thread, at, colon, message = m.groups()
        where = AT.fullmatch(at)
        if where:
            at = f"{rename(rejections.place(where[1], base, root))}:{where[2]}:{where[3]}"
        found.append(f"{thread} at {at}: {message}" if colon and message is not None else f"{thread} at {at}")
    return tuple(sorted(found))


def run_program(program: Path, cwd: Path, env: dict[str, str], base: Path, root: Path,
                rename: Callable[[str], str] = same_file) -> Run:
    """`program` run in `cwd` with no input; its panics placed against `root` as built in `base`."""
    if not program.is_file():
        return Run(error=f"no program at {program}", started=False)
    cwd.mkdir(parents=True, exist_ok=True)
    try:
        ran = engine.run(f"running {program}", [str(program)], cwd, env, RUN_SECS)
    except engine.Failed as e:
        return Run(error=str(e).splitlines()[0])
    return Run(ran.returncode, ran.stdout, panics_of(ran.stderr, base, root, rename))


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


def dep_file(name: str) -> str:
    """How a dependency's root file is compared, on both sides."""
    return f"<dep {name}>"


def renamed(spans: rejections.Placed, rename: Callable[[str], str]) -> rejections.Placed:
    return rejections.placed_of([(key, replace(span, file=rename(span.file))) for key, span in spans])


def rustc_names(case: Case) -> Callable[[str], str]:
    """How rustc's spans and panics name a dependency's files (`dep_file`): placed as
    rejections.place places them against the case's root, a dependency's root file is
    `auxiliary/<file>`, which becomes its `dep_file`; its other files stay under `auxiliary/`."""
    root = Path(case.unit.path)
    named = {rejections.place(d.path, root.parent, root): dep_file(d.name) for d in ordered(case.deps)}
    return lambda file: named.get(file, file)


def package_of(member: str, dep: Dep) -> str:
    """The vendored package of `dep` of the member `member`: one a member, as no two cases share one."""
    return f"{member}_{dep.name}"


def blaze_names(member: str, case: Case) -> Callable[[str], str]:
    """How blaze's spans and panics name a dependency's files, vendored under
    `vendor/<package>/src/`: its root file `lib.rs` as its `dep_file`, any other by its path
    under `auxiliary/`, as `rustc_names` has them."""
    under = {f"/vendor/{package_of(member, d)}/src/": d.name for d in ordered(case.deps)}

    def rename(file: str) -> str:
        path = "/" + file.replace("\\", "/")
        for prefix, name in under.items():
            if (at := path.find(prefix)) >= 0:
                rest = path[at + len(prefix):]
                return dep_file(name) if rest == "lib.rs" else f"{AUXILIARY}/{rest}"
        return file

    return rename


def artifact(out: Path, dep: Dep) -> Path:
    """Where rustc writes `dep` under `out`: a directory a crate, so each has one dep-info file."""
    prefix, suffix = DYLIB if dep.proc_macro else ("lib", ".rlib")
    return out / dep.name / f"{prefix}{dep.name}{suffix}"


def externs(deps: Sequence[Dep], out: str) -> list[str]:
    """rustc's `--extern` of each of `deps`, and `-L dependency=` of every crate below them, as
    built under `out`."""
    words: list[str] = []
    for dep in deps:
        words += ["--extern", f"{dep.name}={artifact(Path(out), dep)}"]
    for dep in ordered(deps):
        words += ["-L", f"dependency={Path(out) / dep.name}"]
    return words


def dep_argv(dep: Dep, out: str) -> list[str]:
    """rustc on a dependency as cargo builds a registry crate: whole, its lints capped, with the
    dev profile's flags."""
    kind = ["--crate-type", "proc-macro", "--extern", "proc_macro"] if dep.proc_macro else ["--crate-type", "lib"]
    return ["rustc", dep.path, "--crate-name", dep.name, "--edition", dep.edition, *kind, "--error-format=json",
            f"--emit={BUILD_EMIT}", "-Zui-testing", *rejections.COMMON, *DEV, "--cap-lints", "allow",
            *externs(dep.deps, out), "-o", str(artifact(Path(out), dep))]


def case_argv(case: Case, out: str) -> list[str]:
    """rustc on a case with dependencies built under `out`: checked, or built whole (a program)."""
    unit, extra = case.unit, externs(case.deps, out)
    if unit.kind == "bin":
        return rejections.rustc_argv(unit, out, BUILD_EMIT, f"{out}/{PROGRAM}", (*DEV, *extra))
    return rejections.rustc_argv(unit, out, extra=extra)


def deps_truth(case: Case, scratch: Path, env: dict[str, str], kept: engine.Kept, toolchain: str) -> Truth:
    """rustc's answer on a case with dependencies: each built in order, a dependency rustc
    rejects being the answer (cargo stops there); else the case, checked or built and run
    against them. Kept, as `built_truth` keeps a build, with every file any of the runs read."""
    unit = case.unit
    cwd, root = Path(unit.path).parent, Path(unit.path)
    order = ordered(case.deps)
    key = kept.key(toolchain, str(cwd), *(w for d in order for w in dep_argv(d, "{out}")), *case_argv(case, "{out}"))
    entry = kept.get(key)
    if (known := rejections.kept_reference(entry, env, str(kept.path(key)))) is not None:
        return Truth(known, run_kept(entry, str(kept.path(key))), kept=True)
    names = rustc_names(case)
    files: set[tuple[str, str]] = set()
    variables: set[tuple[str, str | None]] = set()
    whole = True
    with tempfile.TemporaryDirectory(dir=scratch) as out:
        truth: Truth | None = None
        for dep in order:
            (Path(out) / dep.name).mkdir()
            try:
                ran = engine.run(f"rustc {unit.label}: its dependency {dep.name}", dep_argv(dep, out), cwd, env,
                                 rejections.RUSTC_SECS)
            except engine.Failed as e:
                return Truth(rejections.Reference(error=str(e)))
            ref = rejections.reference_of(ran.returncode, ran.stderr, root)
            if ref.error is not None:
                return Truth(rejections.Reference(error=f"its dependency {dep.name}: {ref.error}"))
            if (read := rejections.what_was_read(Path(out) / dep.name, cwd)) is None:
                whole = False
            else:
                files.update(read[0])
                variables.update(read[1])
            if ref.keys or ref.out == rejections.CRASHES:
                truth = Truth(replace(ref, spans=renamed(ref.spans, names)))
                break
        if truth is None:
            try:
                ran = engine.run(f"rustc {unit.label}", case_argv(case, out), cwd, env, rejections.RUSTC_SECS)
            except engine.Failed as e:
                return Truth(rejections.Reference(error=str(e)))
            ref = rejections.reference_of(ran.returncode, ran.stderr, root)
            run = None
            if unit.kind == "bin" and accepted(ref):
                run = run_program(Path(out) / PROGRAM, scratch / "cwd", env, cwd, root, names)
            if (read := rejections.what_was_read(Path(out), cwd)) is None:
                whole = False
            else:
                files.update(read[0])
                variables.update(read[1])
            truth = Truth(replace(ref, spans=renamed(ref.spans, names)), run)
    ref = truth.ref
    if whole and ref.error is None and (truth.run is None or truth.run.error is None):
        kept.put(key, dict(files=[list(f) for f in sorted(files)],
                           env=[list(v) for v in sorted(variables, key=lambda v: (v[0], v[1] or ""))],
                           keys=sorted(ref.keys), out=ref.out, spans=rejections.kept_spans(ref.spans),
                           run=kept_run_of(truth.run)))
    return truth


def checksums(package: Path) -> dict[str, str]:
    """The sha256 of each file of a vendored package but its checksum file, as cargo's
    `DirectorySource` checks them."""
    return {path.relative_to(package).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(package.rglob("*")) if path.is_file() and path.name != CHECKSUMS}


def dependencies(member: str, deps: Sequence[Dep]) -> str:
    """The `[dependencies]` of a member or a vendored package: each by its extern name, the
    package it is renamed from pinned."""
    if not deps:
        return ""
    return "\n[dependencies]\n" + "".join(
        f'{d.name} = {{ package = "{package_of(member, d)}", version = "={VERSION}" }}\n' for d in deps)


def dep_manifest(member: str, dep: Dep) -> str:
    lib = f'[lib]\nname = "{dep.name}"\npath = "src/lib.rs"\ndoctest = false\n'
    if dep.proc_macro:
        lib += "proc-macro = true\n"
    return (f'[package]\nname = "{package_of(member, dep)}"\nversion = "{VERSION}"\nedition = "{dep.edition}"\n'
            f"autobins = false\nautoexamples = false\nautotests = false\nautobenches = false\n\n{lib}"
            + dependencies(member, dep.deps))


def vendor(ws: Path, members: dict[str, rejections.Unit], cases_of: dict[str, Case]) -> list[tuple[str, str]]:
    """rejections.run_batch's `layout`: each dependency of the cases among `members` written as a
    crates.io package vendored in `ws/vendor` (a `directory` source replacing crates.io: a
    registry package, never a member or a path package), and each member's `[dependencies]`;
    the members a dependency of which cannot be laid out, with why."""
    wrong: list[tuple[str, str]] = []
    vendored = False
    for member, unit in members.items():
        case = cases_of[unit.label]
        if not case.deps:
            continue
        vendored = True
        why: str | None = None
        for dep in ordered(case.deps):
            package = ws / "vendor" / package_of(member, dep)
            (package / "src").mkdir(parents=True)
            root = package / "src" / "lib.rs"
            shutil.copyfile(dep.path, root)
            if why := rejections.uitest.copy_named(dep.path, str(root), dep.path):
                why = f"its dependency {dep.name} {why}"
                break
            (package / "Cargo.toml").write_text(dep_manifest(member, dep))
            (package / CHECKSUMS).write_text(json.dumps({"files": checksums(package), "package": None}, sort_keys=True))
        if why:
            wrong.append((member, why))
            continue
        with open(ws / "crates" / member / "Cargo.toml", "a") as manifest:
            manifest.write(dependencies(member, case.deps))
    if vendored:
        (ws / ".cargo").mkdir(exist_ok=True)
        (ws / ".cargo" / "config.toml").write_text(CONFIG)
    return wrong


def served_lines(stdout: str) -> dict[str, dict[str, Any]]:
    """The `blaze-served` lines of a run's stdout, by package."""
    found: dict[str, dict[str, Any]] = {}
    for line in stdout.splitlines():
        m = rejections.json_line(line)
        if m is not None and m.get("reason") == SERVED and isinstance(m.get("package"), str):
            found[m["package"]] = m
    return found


def serving(case: Case, member: str, stdout: str) -> tuple[report.Verdict, Category, str] | None:
    """None when the second build of `case` (the member `member`) served each of its
    dependencies but those its `//@ refused:` names, which it refused; else what it did."""
    lines = served_lines(stdout)
    for dep in ordered(case.deps):
        package = package_of(member, dep)
        line = lines.get(package)
        if line is None:
            return "error", "error", f"the second build printed no `{SERVED}` line of the dependency {dep.name} ({package})"
        outcome, bodies, why = line.get("outcome"), line.get("bodies"), line.get("why")
        if outcome not in WAYS or bodies not in WAYS or not (why is None or isinstance(why, str)):
            return "error", "error", f"a `{SERVED}` line of another shape: {line}"
        shown = f"outcome {outcome}, bodies {bodies}" + (f": {why}" if why else "")
        both = outcome == "served" and bodies == "served"
        if dep.name in case.refused:
            if both:
                return "fail", "served what is refused", f"the second build served {dep.name}, which the case says is refused"
            if "refused" not in (outcome, bodies):
                return "fail", "not served", f"the second build neither served nor refused {dep.name}: {shown}"
        elif not both:
            return "fail", "not served", f"the second build did not serve {dep.name}: {shown}"
    return None


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
        if case.deps:
            kept = self.builds if case.unit.kind == "bin" else self.checks
            return deps_truth(case, self.work / "rustc", self.env, kept, self.toolchain)
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
        once: list[rejections.Unit] = []
        twice: list[rejections.Unit] = []
        for case, truth in zip(cases, truths):
            problem = rustc_problem(case, truth)
            if problem is not None:
                results[case.unit.label] = Compared(case, truth, *problem)
            else:
                (twice if case.twice else once).append(case.unit)
        of = {c.unit.label: (c, t) for c, t in zip(cases, truths)}
        chunks = [(next(self.indices), chunk, again) for sent, again in ((once, False), (twice, True))
                  for chunk in rejections.batches_of(sent, self.batch)]
        with concurrent.futures.ThreadPoolExecutor(self.jobs) as pool:
            running = [pool.submit(self.blaze_side, i, chunk, of, again) for i, chunk, again in chunks]
            for done in running:
                for found in done.result():
                    results[found.case.unit.label] = found
        return [results[label] for label in labels]

    def blaze_side(self, index: int, units: list[rejections.Unit], of: dict[str, tuple[Case, Truth]],
                   twice: bool = False) -> list[Compared]:
        """blaze on a workspace of `units`, each compared with rustc; `twice`: built a second
        time with a fresh daemon, workspace and target and the same store, compared again, and
        held to serve each case's dependencies (`serving`)."""
        command: Literal["check", "build"] = "build" if units[0].kind == "bin" else "check"
        cases_of = {unit.label: of[unit.label][0] for unit in units}
        layout: rejections.Layout | None = None
        if any(case.deps for case in cases_of.values()):
            layout = functools.partial(vendor, cases_of=cases_of)
        batch = rejections.run_batch(self.eng, index, units, self.work, self.work / "store", self.whole, command,
                                     layout)
        found = self.judged_batch(batch, of)
        if twice:
            shutil.rmtree(self.work / "h" / str(index), ignore_errors=True)
            second = rejections.run_batch(self.eng, index, units, self.work, self.work / "store", self.whole,
                                          command, layout)
            again = {c.case.unit.label: c for c in self.judged_batch(second, of)}
            found = [self.second(first, again[first.case.unit.label], second) for first in found]
            batch = second
        if not self.keep and all(f.verdict == "pass" for f in found):
            shutil.rmtree(batch.ws, ignore_errors=True)
        return found

    def second(self, first: Compared, again: Compared, batch: rejections.Batch) -> Compared:
        """A case built twice: the first build's comparison if it did not pass, else the second's,
        else whether the second served what it should."""
        if first.verdict != "pass":
            return replace(first, detail=f"first build: {first.detail}")
        if again.verdict != "pass":
            return replace(again, detail=f"second build: {again.detail}")
        label = again.case.unit.label
        problem = serving(again.case, Path(batch.packages[label]).name, batch.stdout.get(label, ""))
        if problem is None:
            return replace(again, detail=f"{again.detail}; served on the second build")
        verdict, category, detail = problem
        return replace(again, verdict=verdict, category=category, detail=detail)

    def judged_batch(self, batch: rejections.Batch, of: dict[str, tuple[Case, Truth]]) -> list[Compared]:
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
            rename = blaze_names(package.name, case) if case.deps else same_file
            if case.deps:
                said = replace(said, spans=renamed(said.spans, rename))
            ran = None
            if truth.run is not None and not said.keys and said.error is None and said.who is None:
                ran = run_program(ws / "target" / "debug" / (package.name + engine.EXE), self.work / "cwd", self.env,
                                  ws, rejections.root_of(ws, package.name, unit), rename)
            result, category, detail = judged(truth, said, ran)
            found.append(Compared(case, truth, result, category, detail, said, ran, str(package)))
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


def dep_tree(root: Path) -> Path:
    """An area `bodies-cache` under `root` whose `auxiliary/` has `dep.rs` (2021, naming `pm`,
    a proc-macro crate, and a module `m`); its case's path."""
    aux = root / "bodies-cache" / AUXILIARY
    aux.mkdir(parents=True)
    (aux / "dep.rs").write_text("//@ edition: 2021\n//@ aux-crate: pm=pm.rs\nmod m;\npub fn f() -> u8 { pm::two!() }\n")
    (aux / "m.rs").write_text("pub fn g() {}\n")
    (aux / "pm.rs").write_text('//@ edition: 2021\n//@ force-host\n#![crate_type = "proc-macro"]\n')
    case = root / "bodies-cache" / "c.rs"
    case.write_text("//@ expect: accept\n//@ edition: 2021\n//@ aux-crate: dep=dep.rs\n//@ refused: pm\n"
                    "fn main() { println!(\"{}\", dep::f()); }\n")
    return case


def test_deps() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        path = dep_tree(root)
        case = case_of("bodies-cache/c.rs", path, path.read_text(), HOST)
        assert isinstance(case, Case), case
        assert case.twice and case.refused == {"pm"} and case.unit.edition == "2021" and case.unit.kind == "bin"
        (dep,) = case.deps
        assert (dep.name, dep.edition, dep.proc_macro) == ("dep", "2021", False), dep
        assert [(d.name, d.proc_macro) for d in ordered(case.deps)] == [("pm", True), ("dep", False)]
        cases, wrong = discover(root, (), "", HOST)
        assert [c.name for c in cases] == ["bodies-cache/c.rs"] and not wrong, (cases, wrong)

        def wrong_with(text: str, name: str = "bodies-cache/w.rs") -> str:
            at = root / name
            at.parent.mkdir(parents=True, exist_ok=True)
            at.write_text("//@ expect: accept\n//@ edition: 2021\n" + text + "fn main() {}\n")
            got = case_of(name, at, at.read_text(), HOST)
            assert isinstance(got, str), (text, got)
            return got

        assert "no " in wrong_with("//@ aux-crate: x=missing.rs\n")
        assert "outside" in wrong_with("//@ aux-crate: x=../dep.rs\n")
        assert "not `//@ aux-crate: NAME=FILE.rs`" in wrong_with("//@ aux-crate: dep.rs\n")
        assert "two dependencies" in wrong_with("//@ aux-crate: dep=dep.rs\n//@ aux-crate: dep=m.rs\n")
        assert "does not depend on" in wrong_with("//@ aux-crate: dep=dep.rs\n//@ refused: other\n")
        assert "names a dependency" in wrong_with("")
        (root / "nameres" / AUXILIARY).mkdir(parents=True)
        (root / "nameres" / AUXILIARY / "dep.rs").write_text("pub fn f() {}\n")
        assert "builds twice" in wrong_with("//@ aux-crate: dep=dep.rs\n//@ refused: dep\n", "nameres/w.rs")
        (root / "bodies-cache" / AUXILIARY / "a.rs").write_text("//@ aux-crate: b=b.rs\n")
        (root / "bodies-cache" / AUXILIARY / "b.rs").write_text("//@ aux-crate: a=a.rs\n")
        assert "cycle" in wrong_with("//@ aux-crate: a=a.rs\n")
        (root / "bodies-cache" / AUXILIARY / "flags.rs").write_text("//@ compile-flags: -O\n")
        assert "alone" in wrong_with("//@ aux-crate: f=flags.rs\n")
        plain = root / "nameres" / "p.rs"
        plain.write_text("//@ expect: accept\n//@ aux-crate: dep=dep.rs\nfn main() {}\n")
        once = case_of("nameres/p.rs", plain, plain.read_text(), HOST)
        assert isinstance(once, Case) and not once.twice and once.deps[0].edition == "2015", once


def test_dep_argv() -> None:
    pm = Dep("pm", "/c/a/auxiliary/pm.rs", "2021", True)
    dep = Dep("dep", "/c/a/auxiliary/dep.rs", "2018", False, (pm,))
    argv = dep_argv(dep, "/o")
    assert argv[:2] == ["rustc", dep.path] and "--cap-lints" in argv and argv[argv.index("--cap-lints") + 1] == "allow"
    assert argv[argv.index("--crate-name") + 1] == "dep" and argv[argv.index("--edition") + 1] == "2018"
    assert f"pm=/o/pm/{DYLIB[0]}pm{DYLIB[1]}" in argv and "dependency=/o/pm" in argv
    assert argv[-2:] == ["-o", "/o/dep/libdep.rlib"], argv
    macro = dep_argv(pm, "/o")
    assert macro[macro.index("--crate-type") + 1] == "proc-macro" and "proc_macro" in macro
    case = Case(rejections.Unit("a/t.rs", "/c/a/t.rs", None, "bin", "2021"), Expect(True), (dep,))
    built = case_argv(case, "/o")
    assert "dep=/o/dep/libdep.rlib" in built and "dependency=/o/pm" in built and built[-1] == f"/o/{PROGRAM}", built
    lib = case_argv(replace(case, unit=replace(case.unit, kind="lib")), "/o")
    assert f"--emit={rejections.CHECK_EMIT}" in lib and "--out-dir" in lib and "dep=/o/dep/libdep.rlib" in lib


def test_vendor() -> None:
    import tomllib

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        path = dep_tree(root / "cases")
        case = case_of("bodies-cache/c.rs", path, path.read_text(), HOST)
        assert isinstance(case, Case), case
        ws = root / "ws"
        members, out = rejections.write_workspace(ws, 0, [case.unit])
        assert not out and list(members) == ["u0_0"], (members, out)
        assert vendor(ws, members, {case.unit.label: case}) == []
        config = tomllib.loads((ws / ".cargo" / "config.toml").read_text())
        assert config["source"]["crates-io"]["replace-with"] == SOURCE and config["source"][SOURCE]["directory"] == "vendor"
        member = tomllib.loads((ws / "crates" / "u0_0" / "Cargo.toml").read_text())
        assert member["dependencies"] == {"dep": {"package": "u0_0_dep", "version": f"={VERSION}"}}, member
        dep = tomllib.loads((ws / "vendor" / "u0_0_dep" / "Cargo.toml").read_text())
        assert dep["package"]["name"] == "u0_0_dep" and dep["package"]["edition"] == "2021"
        assert dep["lib"]["name"] == "dep" and "proc-macro" not in dep["lib"]
        assert dep["dependencies"] == {"pm": {"package": "u0_0_pm", "version": f"={VERSION}"}}
        pm = tomllib.loads((ws / "vendor" / "u0_0_pm" / "Cargo.toml").read_text())
        assert pm["lib"]["proc-macro"] is True and "dependencies" not in pm
        assert (ws / "vendor" / "u0_0_dep" / "src" / "m.rs").read_text() == "pub fn g() {}\n", "a module of a dependency"
        sums = json.loads((ws / "vendor" / "u0_0_dep" / CHECKSUMS).read_text())
        assert sums["package"] is None and set(sums["files"]) == {"Cargo.toml", "src/lib.rs", "src/m.rs"}, sums
        for file, sha in sums["files"].items():
            assert hashlib.sha256((ws / "vendor" / "u0_0_dep" / file).read_bytes()).hexdigest() == sha, file
        plain = rejections.Unit("a/p.rs", str(path), None, "bin", "2021")
        ws2 = root / "ws2"
        members, _ = rejections.write_workspace(ws2, 1, [plain])
        assert vendor(ws2, members, {plain.label: Case(plain)}) == [] and not (ws2 / ".cargo").exists()


def test_names() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        path = dep_tree(Path(tmp))
        case = case_of("bodies-cache/c.rs", path, path.read_text(), HOST)
        assert isinstance(case, Case), case
        theirs, ours = rustc_names(case), blaze_names("u3_1", case)
        aux = path.parent / AUXILIARY
        placed = rejections.place(str(aux / "dep.rs"), path.parent, path)
        module = rejections.place(str(aux / "m.rs"), path.parent, path)
        assert theirs(placed) == ours("/w/ws/3/vendor/u3_1_dep/src/lib.rs") == "<dep dep>", (placed, theirs(placed))
        assert theirs(module) == ours("vendor\\u3_1_dep\\src\\m.rs") == f"{AUXILIARY}/m.rs", module
        assert ours("/w/ws/3/vendor/u3_11_dep/src/lib.rs") == "/w/ws/3/vendor/u3_11_dep/src/lib.rs", "another member's"
        assert theirs(rejections.ROOT_FILE) == ours(rejections.ROOT_FILE) == rejections.ROOT_FILE
        at = rejections.Span("/w/ws/3/vendor/u3_1_dep/src/lib.rs", 4, 1, 4, 2)
        assert renamed((("E0308", at),), ours) == (("E0308", replace(at, file="<dep dep>")),)
        text = "thread 'main' panicked at /w/ws/3/vendor/u3_1_dep/src/lib.rs:4:5:\nboom\n"
        assert panics_of(text, Path("/w/ws/3"), Path("/w/ws/3/crates/u3_1/src/main.rs"), ours) == (
            "main at <dep dep>:4:5: boom",)


def served(package: str, outcome: str, bodies: str, why: str | None = None) -> str:
    return json.dumps({"reason": SERVED, "package": package, "outcome": outcome, "bodies": bodies, "why": why})


def test_serving() -> None:
    pm = Dep("pm", "/a/pm.rs", "2021", True)
    dep = Dep("dep", "/a/dep.rs", "2021", False, (pm,))
    case = Case(rejections.Unit("bodies-cache/c.rs", "/c.rs", None, "bin", "2021"), Expect(True), (dep,), twice=True)
    both = "\n".join([served("u0_0_dep", "served", "served"), served("u0_0_pm", "served", "served"),
                      '{"reason": "blaze-work"}', "not json"])
    assert serving(case, "u0_0", both) is None
    missing = serving(case, "u0_0", served("u0_0_dep", "served", "served"))
    assert missing is not None and missing[:2] == ("error", "error") and "pm" in missing[2], missing
    checked = serving(case, "u0_0", both.replace('"bodies": "served", "why": null}', '"bodies": "checked", "why": null}', 1))
    assert checked is not None and checked[1] == "not served" and "dep" in checked[2], checked
    odd = serving(case, "u0_0", served("u0_0_dep", "yes", "served") + "\n" + served("u0_0_pm", "served", "served"))
    assert odd is not None and odd[:2] == ("error", "error"), odd
    refuses = replace(case, refused=frozenset({"pm"}))
    said = serving(refuses, "u0_0", both)
    assert said is not None and said[1] == "served what is refused", said
    edge = "\n".join([served("u0_0_dep", "served", "served"), served("u0_0_pm", "served", "refused", "block-local item")])
    assert serving(refuses, "u0_0", edge) is None
    neither = "\n".join([served("u0_0_dep", "served", "served"), served("u0_0_pm", "checked", "checked")])
    assert (serving(refuses, "u0_0", neither) or ("", "", ""))[1] == "not served"


SELFTESTS = (test_expect, test_case_of, test_tree, test_panics, test_judged, test_rustc_problem, test_kept_runs,
             test_run_program, test_deps, test_dep_argv, test_vendor, test_names, test_serving)


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
