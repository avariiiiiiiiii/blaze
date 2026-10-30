"""rejections.py: what rustc rejects, blaze has to reject with the same error codes (docs/v2/plan/lanes/proof.md
section 11, "Rejections"; #1522, #1440).

    uv run --frozen python harness/v2/rejections.py '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}, "out": "/tmp/rejections.json", "rust": "/path/to/rust"}'
    uv run --frozen python harness/v2/rejections.py '{"engine": {...}, "out": "...", "rust": "...", "filter": "consts/", "shard": {"index": 0, "count": 8}}'
    uv run --frozen python harness/v2/rejections.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine        the blaze under test (engine.EngineSpec)
    out           the path the result JSON is written to (report.py)
    work          the directory everything is built in; empty: a new one under /tmp
    rust          a checkout of rust-lang/rust at the commit of the pinned nightly
                  (harness/toolchain), as uitest.py takes; empty: the fixed cases alone
    filter        only the files of `tests/ui` whose path under it has this in it
    limit         at most this many units of `tests/ui` (0: all of them)
    shard         {"index": i, "count": n}: the files whose path hashes to i modulo n
    fixed         also the fixed cases of harness/v2/rejections (true)
    batch         how many units one workspace has (100)
    whole         one `blaze check --workspace --keep-going` a workspace instead of one
                  `blaze check -p` a unit (false); either way, never the other when one fails
    jobs          how many rustc runs, and how many workspaces, at once (4)
    min_same      fail unless at least this many units of `tests/ui` have rustc's codes (0)
    max_accepted  fail if more than this many units of `tests/ui` are accepted (no limit)
    keep          keep every workspace, not only those of units that did not pass
    cache         where rustc's answers are kept across runs ("~/.cache/blaze-v2-proof"; "" keeps
                  none): by the toolchain (`rustc -vV`, `cargo -vV`) and the unit's argv, and
                  used again only while every file and environment variable its dep-info names
                  has the content it had, so a rerun runs rustc on what changed alone

The units are the files of `tests/ui` (each revision one unit) rustc's suite expects to be
rejected: no `check-pass`, `build-pass` or `run-*`. Each is read as compiletest reads it
(revisions, `edition`, `compile-flags`, `#![crate_type]`, `--test`) and is a library, a
program or a test harness as rustc would build it.

The reference is the pinned rustc on the file as compiletest runs it (its edition and flags,
`--cfg` of its revision, `-Zui-testing`, `-A unused -A internal_features`), with
`--error-format=json --emit=dep-info,metadata`, which is what `cargo check` asks of rustc. A unit rustc
accepts that way (its errors are of codegen), or crashes on, is out of scope.

blaze's side: the units rustc rejects are written as packages (a library, a binary, or a library
checked with `--tests`, of the unit's edition; the files it names with `mod` and `include!`
beside it), up to `batch` of the same kind and flags in one workspace, the flags in
`RUSTFLAGS`, and checked with `blaze check --message-format=json` through the engine under that
workspace's one daemon. An error is keyed by its code (E0277, or a denied lint's name), or, for
an error with no code, by the start of its message with what is in backticks taken out. A unit:

    pass   blaze rejects it with the same set of keys
    fail   blaze accepts it ("accepted what rustc rejects"), rejects it with another set ("other
           codes"), or the engine says the check was not blaze's
    error  the harness could not say: rustc or blaze timed out, or blaze failed without an error

Units it can say nothing about (an auxiliary crate, a `-Z` flag that changes what is rejected,
`only-`/`ignore-` for another target, a `needs-` this machine does not meet, ...) are not
outcomes: they are counted by reason in the result's `extra.out_of_scope`.

The fixed cases (harness/v2/rejections/*.rs) are units too, each with `//@ rejects:` (its codes,
or a message's start in double quotes) and `//@ issue:`. rustc is held to the header first: a
header rustc does not bear out is an error of the case.
"""

import concurrent.futures
import hashlib
import importlib
import json
import platform
import re
import shutil
import subprocess
import sys
import tempfile
from collections import Counter
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine, report

borrowck_ui: Any = importlib.import_module("borrowck_ui")
uitest: Any = importlib.import_module("uitest")

SCRIPT = "rejections.py"
FIXED = HERE / "rejections"
Kind = Literal["lib", "bin", "test"]
# compiletest's directives, `//@[rev1,rev2] name: value` (borrowck_ui.py's reading of them).
DIRECTIVE: re.Pattern[str] = borrowck_ui.DIRECTIVE
CRATE_TYPE = re.compile(r'#!\[crate_type *= *"([\w-]+)"\]')
NO_CORE = re.compile(r"#!\[no_core\]")
TOKEN = re.compile(r'"([^"]+)"|([^\s,"]+)')
EDITIONS = ("2015", "2018", "2021", "2024")
# What compiletest adds for a ui test that is not run, and what the package's own `--check-cfg`
# would warn of the revisions' cfgs.
COMMON = ("-A", "unused", "-A", "internal_features", "-A", "unexpected_cfgs")
PASSES = {"check-pass", "build-pass", "run-pass", "run-fail", "run-crash", "run-fail-or-crash"}
AUXILIARY = {"aux-build", "aux-crate", "aux-bin", "aux-codegen-backend", "proc-macro"}
OUT_DIRECTIVES = {
    "rustc-env": "sets rustc's environment (`rustc-env`)",
    "unset-rustc-env": "sets rustc's environment (`unset-rustc-env`)",
    "add-core-stubs": "builds against minicore (`add-core-stubs`)",
    "should-ice": "a test of an internal compiler error (`should-ice`)",
    "incremental": "an incremental test (`incremental`)",
}
# `ignore-<these>` leaves a test out everywhere.
ALWAYS_IGNORED = {"test", "auxiliary"}
NEEDS_MET = {"needs-unwind", "needs-threads", "needs-subprocess", "needs-dynamic-linking", "needs-target-std",
             "needs-symlink", "needs-relocation-model-pic", "needs-deterministic-layouts", "needs-crate-type"}
# Flags kept on both sides, with their value; flags dropped from both, which change no error.
KEPT = {"--cfg", "--check-cfg", "-A", "-W", "-D", "-F", "--allow", "--warn", "--deny", "--forbid", "--force-warn",
        "--cap-lints"}
DROPPED = {"--error-format", "--json", "--diagnostic-width", "--color", "--remap-path-prefix", "--crate-name",
           "--verbose", "-v"}
DROPPED_C = {"incremental"}
INERT_Z = {"ui-testing", "deduplicate-diagnostics", "track-diagnostics", "write-long-types-to-disk",
           "translate-remapped-path-to-local-path", "simulate-remapped-rust-src-base", "macro-backtrace",
           "verbose-internals", "unstable-options", "terminal-urls", "ignore-directory-in-diagnostics-source-blocks"}
VALUED_SHORT = "CZAWDFLl"
KIND_ARGS: dict[Kind, tuple[str, ...]] = {"lib": ("--lib",), "bin": ("--bins",), "test": ("--tests",)}
MESSAGE = "message: "
MESSAGE_START = 60
RUSTC_SECS = 120
CHECK_SECS = 1800
WORKSPACE = '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n'


@dataclass(frozen=True)
class Shard:
    index: int = 0
    count: int = 1


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    rust: str = ""
    filter: str = ""
    limit: int = 0
    shard: Shard = Shard()
    fixed: bool = True
    batch: int = 100
    whole: bool = False
    jobs: int = 4
    min_same: int = 0
    max_accepted: int = 1 << 62
    keep: bool = False
    cache: str = "~/.cache/blaze-v2-proof"


@dataclass(frozen=True)
class Directive:
    """`//@[revisions] name: value`; `revisions` None for every revision."""

    revisions: tuple[str, ...] | None
    name: str
    value: str


@dataclass(frozen=True)
class Unit:
    """One revision of a test file, as both sides build it. `flags` are rustc's words, given to
    rustc and in `RUSTFLAGS` to blaze; the edition and the kind are the package's."""

    test: str
    path: str
    revision: str | None
    kind: Kind
    edition: str
    flags: tuple[str, ...] = ()

    @property
    def label(self) -> str:
        return f"{self.test}#{self.revision}" if self.revision else self.test


@dataclass(frozen=True)
class Classified:
    """A file's units, the revisions out of scope (label, reason), and how many revisions are
    not rejections at all (`check-pass` and the like)."""

    units: tuple[Unit, ...] = ()
    out: tuple[tuple[str, str], ...] = ()
    passing: int = 0


@dataclass(frozen=True)
class Reference:
    """What rustc said of a unit: the keys of its errors; or why the unit is out of scope; or
    why the harness could not say."""

    keys: frozenset[str] = frozenset()
    out: str | None = None
    error: str | None = None


@dataclass(frozen=True)
class Said:
    """What blaze said of a unit: the keys of its errors (none: it accepted the unit), or why
    nothing can be read from it, and who did the check when it was not blaze (`not_blazes`)."""

    keys: frozenset[str] = frozenset()
    error: str | None = None
    who: str | None = None


@dataclass(frozen=True)
class Batch:
    """What came of the workspace `ws`: what blaze said of each unit, the units that could not be
    laid out as a package (and why), where each unit's package is, and a daemon that would not
    stop."""

    ws: str
    said: tuple[tuple[Unit, Said], ...] = ()
    out: tuple[tuple[Unit, str], ...] = ()
    packages: dict[str, str] = field(default_factory=dict[str, str])
    stopped: str | None = None


def directives(text: str) -> list[Directive]:
    found: list[Directive] = []
    for m in DIRECTIVE.finditer(text):
        revisions = tuple(m["rev"].split(",")) if m["rev"] else None
        found.append(Directive(revisions, m["name"], m["value"].strip()))
    return found


def host_names(system: str, machine: str) -> frozenset[str]:
    """The names `only-` and `ignore-` match on this machine, as compiletest names them."""
    arch = {"arm64": "aarch64", "amd64": "x86_64", "AMD64": "x86_64"}.get(machine, machine)
    if system.startswith("linux"):
        names, triple = {"linux", "unix", "elf", "gnu"}, f"{arch}-unknown-linux-gnu"
    elif system == "darwin":
        names, triple = {"macos", "apple", "unix"}, f"{arch}-apple-darwin"
    elif system == "win32":
        names, triple = {"windows", "msvc"}, f"{arch}-pc-windows-msvc"
    else:
        names, triple = {system}, arch
    return frozenset(names | {arch, triple, "64bit", "endian-little", "nightly"})


def needs_met(name: str, value: str, host: frozenset[str]) -> bool:
    if name == "needs-asm-support":
        return bool(host & {"x86_64", "aarch64"})
    if name == "needs-target-has-atomic":
        return all(width.strip() in {"8", "16", "32", "64", "ptr"} for width in value.split(","))
    return name in NEEDS_MET


def edition_of(value: str) -> str | None:
    """The edition a test is built with: its own, or the lowest of a range (`2018..`)."""
    m = re.fullmatch(r"(\d{4})?(?:\.\.=?(\d{4})?)?", value.strip())
    if not m or not value.strip():
        return None
    lowest = m[1] or EDITIONS[0]
    return lowest if lowest in EDITIONS else None


def kind_of(crate_type: str) -> Kind | None:
    return "lib" if crate_type in ("lib", "rlib") else "bin" if crate_type == "bin" else None


def flag_of(flag: str) -> tuple[str, str | None]:
    """A flag of `uitest.flags_of` as (option, value)."""
    if flag.startswith("--"):
        for separator in (" ", "="):
            if separator in flag:
                option, _, value = flag.partition(separator)
                return option, value.strip()
        return flag, None
    if len(flag) > 2 and flag[0] == "-" and flag[1] in VALUED_SHORT:
        return flag[:2], flag[2:].strip()
    return flag, None


def unit_of(test: str, path: str, text: str, revision: str | None, given: Sequence[Directive],
            host: frozenset[str]) -> Unit | str | None:
    """The unit of one revision; or why it is out of scope; or None when it is not a rejection."""
    names = {d.name for d in given}
    if names & PASSES:
        return None
    if "build-fail" in names:
        return "rejected only by codegen (`build-fail`)"
    edition = EDITIONS[0]
    kind: Kind | None = None
    crate_type: str | None = None
    flags: list[str] = ["--cfg", revision] if revision else []
    for d in given:
        name = d.name
        if name.startswith("only-") and name[5:] not in host:
            return f"`{name}`: for another target"
        if name.startswith("ignore-") and (name[7:] in host or name[7:] in ALWAYS_IGNORED):
            return f"`{name}`"
        if name.startswith("needs-") and not needs_met(name, d.value, host):
            return f"`{name}`: not met here"
        if name in AUXILIARY:
            return "needs an auxiliary crate"
        if name in OUT_DIRECTIVES:
            return OUT_DIRECTIVES[name]
        if name == "edition":
            year = edition_of(d.value)
            if year is None:
                return f"edition `{d.value}`"
            edition = year
        if name != "compile-flags":
            continue
        try:
            words: list[str] = uitest.flags_of(d.value)
        except ValueError:
            return "compile flags that do not split"
        for word in words:
            option, value = flag_of(word)
            if value is not None and re.search(r"\s", value):
                return f"compile flag {option} with a space in its value"
            if option == "--edition":
                year = edition_of(value or "")
                if year is None:
                    return f"edition `{value}`"
                edition = year
            elif option == "--crate-type":
                crate_type = value or ""
            elif option == "--test":
                kind = "test"
            elif option in KEPT and value:
                flags += [option, value]
            elif option == "-C" and value:
                if value.split("=")[0].replace("_", "-") not in DROPPED_C:
                    flags += [option, value]
            elif option in ("-O", "-g"):
                flags.append(option)
            elif option == "-Z" and value:
                if value.split("=")[0] not in INERT_Z:
                    return f"compile flag -Z{value.split('=')[0]}"
            elif option not in DROPPED:
                return f"compile flag {option}"
    if NO_CORE.search(text):
        return "`#![no_core]`"
    attribute = CRATE_TYPE.search(text)
    crate_type = crate_type if crate_type is not None else attribute[1] if attribute else "bin"
    if kind is None:
        kind = kind_of(crate_type)
        if kind is None:
            return f"crate type {crate_type}"
    return Unit(test, path, revision, kind, edition, tuple(flags))


def classify(test: str, path: str, text: str, host: frozenset[str]) -> Classified:
    found = directives(text)
    revisions: list[str | None] = [r for d in found if d.name == "revisions" and d.revisions is None
                                   for r in d.value.split()] or [None]
    units: list[Unit] = []
    out: list[tuple[str, str]] = []
    passing = 0
    for revision in revisions:
        given = [d for d in found if d.revisions is None or (revision is not None and revision in d.revisions)]
        got = unit_of(test, path, text, revision, given, host)
        if got is None:
            passing += 1
        elif isinstance(got, str):
            out.append((f"{test}#{revision}" if revision else test, got))
        else:
            units.append(got)
    return Classified(tuple(units), tuple(out), passing)


def in_shard(test: str, shard: Shard) -> bool:
    return int(hashlib.sha256(test.encode()).hexdigest()[:8], 16) % shard.count == shard.index


def test_files(ui: Path, needle: str, shard: Shard) -> list[tuple[str, Path]]:
    """The test files of `tests/ui` (by their path under it), as compiletest finds them: not in an
    `auxiliary` directory or one with a `compiletest-ignore-dir`."""
    ignored = tuple(str(d) for d in uitest.ignored_directories(str(ui)))
    found: list[tuple[str, Path]] = []
    for path in sorted(ui.rglob("*.rs")):
        test = path.relative_to(ui).as_posix()
        if needle not in test or "/auxiliary/" in f"/{test}" or str(path).startswith(ignored):
            continue
        if in_shard(test, shard):
            found.append((test, path))
    return found


def key_of(code: str | None, message: str) -> str:
    """What an error is compared by: its code, or the start of its message with what is in
    backticks taken out (names and paths, which blaze writes its own way)."""
    if code:
        return code
    first = message.splitlines()[0] if message else ""
    return MESSAGE + re.sub(r"`[^`]*`", "`_`", first)[:MESSAGE_START]


def json_line(line: str) -> dict[str, Any] | None:
    if not line.startswith("{"):
        return None
    try:
        value: Any = json.loads(line)
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def error_key(diagnostic: Any) -> str | None:
    """The key of a diagnostic (rustc's JSON) that is an error; None for anything else."""
    if not isinstance(diagnostic, dict):
        return None
    d: dict[str, Any] = diagnostic
    message = d.get("message")
    if d.get("level") != "error" or not isinstance(message, str) or message.startswith("aborting due to"):
        return None
    code = d.get("code")
    name = code.get("code") if isinstance(code, dict) else None
    return key_of(name if isinstance(name, str) else None, message)


def reference_of(returncode: int, stderr: str) -> Reference:
    """What rustc's run on a unit says (`--error-format=json`)."""
    keys: set[str] = set()
    for line in stderr.splitlines():
        d = json_line(line)
        if d is None:
            continue
        if "internal compiler error" in str(d.get("level", "")):
            return Reference(out="rustc crashes on it")
        if key := error_key(d):
            keys.add(key)
    if "thread 'rustc' panicked" in stderr:
        return Reference(out="rustc crashes on it")
    if keys:
        return Reference(frozenset(keys))
    if returncode == 0:
        return Reference(out="rustc accepts it as `cargo check` runs it (`--emit=dep-info,metadata`)")
    return Reference(error=f"rustc exited {returncode} with no error:\n{stderr[-600:]}")


def rustc_argv(unit: Unit, out_dir: str) -> list[str]:
    kind = {"lib": ["--crate-type", "lib"], "bin": ["--crate-type", "bin"], "test": ["--test"]}[unit.kind]
    return ["rustc", unit.path, "--edition", unit.edition, *kind, "--error-format=json", "--emit=dep-info,metadata",
            "-Zui-testing", *COMMON, *unit.flags, "--out-dir", out_dir]


type Read = tuple[tuple[tuple[str, str], ...], tuple[tuple[str, str | None], ...]]


def dep_info(text: str, cwd: Path) -> tuple[list[str], list[tuple[str, str | None]]]:
    """The files and environment variables a rustc dep-info file says the crate read: the
    files from its phony `path:` lines (a space in a path written `\\ `), relative ones under
    `cwd`; the variables from its `# env-dep:NAME=VALUE` lines (no `=`: read and unset)."""
    files: list[str] = []
    env: list[tuple[str, str | None]] = []
    for line in text.splitlines():
        if line.startswith("# env-dep:"):
            name, eq, value = line[len("# env-dep:"):].partition("=")
            env.append((name, value if eq else None))
        elif line.endswith(":") and not line.startswith("#"):
            path = line[:-1].replace("\\ ", " ")
            files.append(str(cwd / path) if not Path(path).is_absolute() else path)
    return files, env


def what_was_read(out: Path, cwd: Path) -> Read | None:
    """Each file rustc's dep-info in `out` names with its sha256, and each variable with its
    value; None when there is no dep-info (rustc stopped before writing it) or a file it names
    is gone: then nothing can say when its answer would still hold."""
    written = sorted(out.glob("*.d"))
    if len(written) != 1:
        return None
    files, env = dep_info(written[0].read_text(errors="replace"), cwd)
    hashed: list[tuple[str, str]] = []
    for path in sorted(set(files)):
        try:
            hashed.append((path, engine.digest(Path(path))))
        except OSError:
            return None
    return tuple(hashed), tuple(sorted(set(env), key=lambda e: (e[0], e[1] or "")))


def kept_reference(entry: object, env: dict[str, str], where: str) -> Reference | None:
    """A kept answer of rustc, when every file it read still has the sha256 it had and every
    variable its value; else None. An entry not of the shape `reference` writes is a Failed."""
    if entry is None:
        return None
    if not isinstance(entry, dict):
        raise engine.Failed(f"the kept answer of rustc {where} is not one rejections.py wrote: {entry!r:.300}")
    fields = cast(dict[str, object], entry)
    files, variables, keys, out = fields.get("files"), fields.get("env"), fields.get("keys"), fields.get("out")
    if not (isinstance(files, list) and isinstance(variables, list) and isinstance(keys, list)
            and "out" in fields and (out is None or isinstance(out, str))):
        raise engine.Failed(f"the kept answer of rustc {where} is not one rejections.py wrote: {entry!r:.300}")
    for pair in cast(list[object], files):
        if not (isinstance(pair, list) and len(pair) == 2 and all(isinstance(p, str) for p in pair)):
            raise engine.Failed(f"the kept answer of rustc {where} has the file {pair!r}")
        path, sha = cast(list[str], pair)
        try:
            if engine.digest(Path(path)) != sha:
                return None
        except OSError:
            return None
    for pair in cast(list[object], variables):
        if not (isinstance(pair, list) and len(pair) == 2 and isinstance(pair[0], str)):
            raise engine.Failed(f"the kept answer of rustc {where} has the variable {pair!r}")
        name, value = cast(list[Any], pair)
        if env.get(name) != value:
            return None
    return Reference(frozenset(str(k) for k in cast(list[object], keys)), cast(str | None, out))


def cargo_messages(stdout: str) -> tuple[dict[str, frozenset[str]], frozenset[str], frozenset[str]]:
    """Of `cargo check --message-format=json`'s lines: the keys of the errors of each target (by
    its name), the keys of every error, and the targets checked (a `compiler-artifact`)."""
    errors: dict[str, set[str]] = {}
    every: set[str] = set()
    checked: set[str] = set()
    for line in stdout.splitlines():
        m = json_line(line)
        if m is None:
            continue
        target: Any = m.get("target")
        name: Any = target.get("name") if isinstance(target, dict) else None
        if m.get("reason") == "compiler-message" and (key := error_key(m.get("message"))):
            every.add(key)
            if isinstance(name, str):
                errors.setdefault(name, set()).add(key)
        elif m.get("reason") == "compiler-artifact" and isinstance(name, str):
            checked.add(name)
    return {k: frozenset(v) for k, v in errors.items()}, frozenset(every), frozenset(checked)


def said_alone(returncode: int, stdout: str, tail: str, who: str | None) -> Said:
    """What a `blaze check -p` of one unit's package says of it: every error of the run is its."""
    _, every, _ = cargo_messages(stdout)
    if every:
        return Said(every, who=who)
    if returncode == 0:
        return Said(who=who)
    return Said(error=f"`blaze check` exited {returncode} with no error of the test:\n{tail}")


def said_together(stdout: str, members: Sequence[str], tail: str, who: str | None) -> dict[str, Said]:
    """What one `blaze check --workspace` says of each member: its errors; accepted only when it
    was checked (a `compiler-artifact` for it), never for having said nothing."""
    errors, _, checked = cargo_messages(stdout)
    said: dict[str, Said] = {}
    for member in members:
        if errors.get(member):
            said[member] = Said(errors[member], who=who)
        elif member in checked:
            said[member] = Said(who=who)
        else:
            said[member] = Said(error=f"`blaze check` said nothing of {member}: no error, no artifact:\n{tail}")
    return said


Category = Literal["same", "accepted", "other codes", "not blaze's", "error"]


def verdict(rustc: frozenset[str], said: Said) -> tuple[report.Verdict, Category, str]:
    if said.error is not None:
        return "error", "error", said.error
    if said.who is not None:
        return "fail", "not blaze's", f"not blaze's: {said.who}"
    shown = ", ".join(sorted(rustc))
    if not said.keys:
        return "fail", "accepted", f"accepted what rustc rejects ({shown})"
    if said.keys != rustc:
        return "fail", "other codes", (f"other codes: rustc {shown}; blaze {', '.join(sorted(said.keys))}"
                                       f" (only blaze: {', '.join(sorted(said.keys - rustc)) or '-'}; "
                                       f"only rustc: {', '.join(sorted(rustc - said.keys)) or '-'})")
    return "pass", "same", shown


def header_of(text: str) -> tuple[tuple[str, ...], str]:
    """A fixed case's `//@ rejects:` as keys (a quoted message start as its key) and its `//@ issue:`."""
    tokens: list[str] = []
    issue = ""
    for d in directives(text):
        if d.name == "rejects":
            tokens += [key_of(None, m[1]) if m[1] else m[2] for m in TOKEN.finditer(d.value)]
        elif d.name == "issue":
            issue = d.value
    return tuple(tokens), issue


def covers(token: str, key: str) -> bool:
    if token == key:
        return True
    return token.startswith(MESSAGE) and key.startswith(MESSAGE) and token[len(MESSAGE):] in key[len(MESSAGE):]


def header_holds(tokens: Sequence[str], keys: frozenset[str]) -> bool:
    """Whether the header names every error rustc gave, and nothing else."""
    return bool(keys) and all(any(covers(t, k) for k in keys) for t in tokens) and all(
        any(covers(t, k) for t in tokens) for k in keys)


def manifest(name: str, unit: Unit) -> str:
    target = (f'[[bin]]\nname = "{name}"\npath = "src/main.rs"\n' if unit.kind == "bin"
              else '[lib]\npath = "src/lib.rs"\ndoctest = false\n')
    return (f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition = "{unit.edition}"\nautobins = false\n'
            f"autoexamples = false\nautotests = false\nautobenches = false\n\n{target}")


def check_args(unit: Unit, member: str | None) -> list[str]:
    """`blaze check`'s arguments for one member (`-p`), or for the whole workspace (None)."""
    picked = ["-p", member] if member is not None else ["--workspace", "--keep-going"]
    return [*picked, *KIND_ARGS[unit.kind], "--message-format=json"]


def batches_of(units: Sequence[Unit], size: int) -> list[list[Unit]]:
    """The workspaces: units of one kind and the same flags (one `RUSTFLAGS`), `size` at most."""
    groups: dict[tuple[Kind, tuple[str, ...]], list[Unit]] = {}
    for unit in units:
        groups.setdefault((unit.kind, unit.flags), []).append(unit)
    return [group[i:i + size] for group in groups.values() for i in range(0, len(group), size)]


def write_workspace(ws: Path, index: int, units: Sequence[Unit]) -> tuple[dict[str, Unit], list[tuple[Unit, str]]]:
    """Writes a package of each unit under `ws/crates`: its members by name, and the units that
    name a file that cannot be put beside their copy, with why."""
    members: dict[str, Unit] = {}
    out: list[tuple[Unit, str]] = []
    for i, unit in enumerate(units):
        name = f"u{index}_{i}"
        crate = ws / "crates" / name
        (crate / "src").mkdir(parents=True)
        root = crate / "src" / ("main.rs" if unit.kind == "bin" else "lib.rs")
        shutil.copyfile(unit.path, root)
        why: str | None = uitest.copy_named(unit.path, str(root), unit.path)
        if why:
            shutil.rmtree(crate)
            out.append((unit, why))
            continue
        (crate / "Cargo.toml").write_text(manifest(name, unit))
        members[name] = unit
    (ws / "Cargo.toml").write_text(WORKSPACE)
    return members, out


def reference(unit: Unit, scratch: Path, env: dict[str, str], kept: engine.Kept, toolchain: str) -> tuple[Reference, bool]:
    """rustc's answer on `unit`, and whether it was kept from an earlier run (`kept_reference`:
    while what it read is unchanged); else rustc is run and its answer kept with what it read.
    An answer the harness could not get (a timeout) is never kept."""
    cwd = Path(unit.path).parent
    key = kept.key(toolchain, str(cwd), *rustc_argv(unit, "{out}"))
    if (known := kept_reference(kept.get(key), env, str(kept.path(key)))) is not None:
        return known, True
    with tempfile.TemporaryDirectory(dir=scratch) as out:
        try:
            ran = engine.run(f"rustc {unit.label}", rustc_argv(unit, out), cwd, env, RUSTC_SECS)
        except engine.Failed as e:
            return Reference(error=str(e)), False
        ref = reference_of(ran.returncode, ran.stderr)
        if ref.error is None and (read := what_was_read(Path(out), cwd)) is not None:
            files, variables = read
            kept.put(key, dict(files=[list(f) for f in files], env=[list(v) for v in variables],
                               keys=sorted(ref.keys), out=ref.out))
    return ref, False


def run_batch(eng: engine.Engine, index: int, units: Sequence[Unit], work: Path, store: Path, whole: bool) -> Batch:
    ws, home = work / "ws" / str(index), work / "h" / str(index)
    shutil.rmtree(ws, ignore_errors=True)
    members, out = write_workspace(ws, index, units)
    packages = {unit.label: str(ws / "crates" / name) for name, unit in members.items()}
    if not members:
        return Batch(str(ws), out=tuple(out), packages=packages)
    env = eng.env(home, store, extra={"RUSTFLAGS": " ".join((*COMMON, *units[0].flags))})
    said: list[tuple[Unit, Said]] = []
    stopped: str | None = None
    try:
        if whole:
            first = next(iter(members.values()))
            ran = eng.build(f"blaze check of workspace {index}", ws, env, check_args(first, None), command="check",
                            timeout=CHECK_SECS)
            got = said_together(ran.stdout, list(members), ran.said(600), eng.not_blazes(ran))
            said += [(unit, got[name]) for name, unit in members.items()]
        else:
            for name, unit in members.items():
                ran = eng.build(f"blaze check of {unit.label}", ws, env, check_args(unit, name), command="check",
                                timeout=CHECK_SECS)
                said.append((unit, said_alone(ran.returncode, ran.stdout, ran.said(600), eng.not_blazes(ran))))
    except (engine.Unmeasurable, engine.Failed) as e:
        why = f"unmeasured: {e}" if isinstance(e, engine.Unmeasurable) else str(e)
        done = {unit.label for unit, _ in said}
        said += [(unit, Said(error=why)) for unit in members.values() if unit.label not in done]
    finally:
        try:
            eng.stop(ws, env)
        except engine.Failed as e:
            stopped = str(e)
    return Batch(str(ws), tuple(said), tuple(out), packages, stopped)


def rustc_commit(env: dict[str, str]) -> str:
    ran = engine.run("rustc -vV", ["rustc", "-vV"], engine.ROOT, env, 300, check=True)
    m = re.search(r"^commit-hash: (\w+)$", ran.stdout, re.M)
    if not m:
        raise engine.Failed(f"`rustc -vV` names no commit:\n{ran.stdout}")
    return m[1]


def checkout_commit(rust: Path) -> str:
    done = subprocess.run(["git", "-C", str(rust), "rev-parse", "HEAD"], capture_output=True, text=True)
    if done.returncode != 0:
        raise engine.Failed(f"{rust}: not a git checkout: {done.stderr.strip()}")
    return done.stdout.strip()


def fixed_cases(host: frozenset[str]) -> list[tuple[Unit, tuple[str, ...], str]]:
    """Each fixed case's unit, its header's keys and its issue; exits naming a case that is not
    one unit in scope with a header."""
    cases: list[tuple[Unit, tuple[str, ...], str]] = []
    for path in sorted(FIXED.glob("*.rs")):
        text = path.read_text()
        found = classify(f"fixed/{path.name}", str(path), text, host)
        tokens, issue = header_of(text)
        if len(found.units) != 1 or found.out or found.passing:
            fail(SCRIPT, f"{path}: a fixed case is one unit in scope, and this is {found}")
        if not tokens or not issue:
            fail(SCRIPT, f"{path}: a fixed case has `//@ rejects:` and `//@ issue:`")
        cases.append((found.units[0], tokens, issue))
    return cases


def main(args: Input) -> None:
    for name, value, least in (("jobs", args.jobs, 1), ("batch", args.batch, 1), ("shard.count", args.shard.count, 1),
                               ("limit", args.limit, 0), ("min_same", args.min_same, 0)):
        if value < least:
            fail(SCRIPT, f"{name}: at least {least}, not {value}")
    if not 0 <= args.shard.index < args.shard.count:
        fail(SCRIPT, f"shard.index: from 0 to {args.shard.count - 1}, not {args.shard.index}")
    if not args.rust and not args.fixed:
        fail(SCRIPT, "nothing to check: no `rust` checkout and `fixed` false")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    host = host_names(sys.platform, platform.machine())
    env = engine.scrubbed()
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-rej-", dir="/tmp")).resolve()
    (work / "rustc").mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    out_of_scope: Counter[str] = Counter()
    candidates: list[Unit] = []
    passing = files = 0
    if args.rust:
        rust = Path(args.rust).resolve()
        try:
            pinned, checked_out = rustc_commit(env), checkout_commit(rust)
        except engine.Failed as e:
            fail(SCRIPT, str(e))
        if pinned != checked_out:
            fail(SCRIPT, f"rust: {rust} is at {checked_out}, and the pinned nightly ({engine.TOOLCHAIN}) at {pinned}")
        for test, path in test_files(rust / "tests" / "ui", args.filter, args.shard):
            files += 1
            found = classify(test, str(path), path.read_text(errors="replace"), host)
            candidates += found.units
            out_of_scope.update(reason for _, reason in found.out)
            passing += found.passing
        if args.limit:
            candidates = candidates[:args.limit]
    fixed = fixed_cases(host) if args.fixed else []
    rep = report.Report(SCRIPT, args.engine, args.out)
    every = [*candidates, *(unit for unit, _, _ in fixed)]
    kept = engine.Kept(args.cache, "rustc")
    try:
        toolchain = engine.toolchain_identity(env)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        answers = list(pool.map(lambda u: reference(u, work / "rustc", env, kept, toolchain), every))
    references = {u.label: ref for u, (ref, _) in zip(every, answers)}
    rustc_kept = sum(hit for _, hit in answers)
    rejected: list[Unit] = []
    issues: dict[str, str] = {}
    for unit in candidates:
        ref = references[unit.label]
        if ref.error is not None:
            rep.add(unit.label, "error", ref.error)
        elif ref.out is not None:
            out_of_scope[ref.out] += 1
        else:
            rejected.append(unit)
    for unit, tokens, issue in fixed:
        ref = references[unit.label]
        rustc_said = ref.error or ref.out or "rustc rejects it with " + ", ".join(sorted(ref.keys))
        if ref.error is not None or ref.out is not None or not header_holds(tokens, ref.keys):
            rep.add(unit.label, "error", f"its header says {', '.join(tokens)}; {rustc_said}")
        else:
            rejected.append(unit)
            issues[unit.label] = issue
    categories: Counter[Category] = Counter()
    fixed_categories: Counter[Category] = Counter()
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        running = [pool.submit(run_batch, eng, i, chunk, work, work / "store", args.whole)
                   for i, chunk in enumerate(batches_of(rejected, args.batch))]
        # In the order the workspaces were made, not the order they end in: one report for one input.
        for done in running:
            batch = done.result()
            for _, why in batch.out:
                out_of_scope[why] += 1
            if batch.stopped is not None:
                rep.add(f"stopping the daemon of {batch.ws}", "error", batch.stopped)
            passed = True
            for unit, said in batch.said:
                result, category, detail = verdict(references[unit.label].keys, said)
                (fixed_categories if unit.label in issues else categories)[category] += 1
                passed &= result == "pass"
                if unit.label in issues:
                    detail += f"; {issues[unit.label]}"
                if result != "pass":
                    detail += f"\npackage: {batch.packages[unit.label]}"
                rep.add(unit.label, result, detail)
            if passed and not args.keep:
                shutil.rmtree(batch.ws, ignore_errors=True)
    in_scope = sum(categories.values())
    same, accepted = categories["same"], categories["accepted"]
    if args.rust:
        rep.add("floor: units of tests/ui with rustc's codes", "pass" if same >= args.min_same else "fail",
                f"{same} of {in_scope}; the floor is {args.min_same}", same=same, floor=args.min_same)
        rep.add("ceiling: units of tests/ui accepted that rustc rejects", "pass" if accepted <= args.max_accepted
                else "fail", f"{accepted} of {in_scope}; the ceiling is {args.max_accepted}", accepted=accepted,
                ceiling=args.max_accepted)
    rep.extra.update(work=str(work), toolchain=engine.TOOLCHAIN, rust=args.rust, files=files,
                     not_rejections=passing, units=len(candidates), in_scope=in_scope,
                     rustc_runs=len(every) - rustc_kept, rustc_kept=rustc_kept,
                     categories=dict(categories), fixed=dict(fixed_categories),
                     out_of_scope=dict(out_of_scope.most_common()))
    if not args.keep:
        shutil.rmtree(work / "store", ignore_errors=True)
        shutil.rmtree(work / "h", ignore_errors=True)
    rep.finish()


HOST = host_names("linux", "x86_64")


def units(text: str, test: str = "t.rs") -> Classified:
    return classify(test, f"/ui/{test}", text, HOST)


def test_directives() -> None:
    found = directives("//@ revisions: a b\n//@[a,b] compile-flags: --cfg x\n//@ edition:2021\nfn main() {}\n")
    assert found == [Directive(None, "revisions", "a b"), Directive(("a", "b"), "compile-flags", "--cfg x"),
                     Directive(None, "edition", "2021")], found


def test_classify_modes() -> None:
    plain = units("fn main() { let x: u32 = \"\"; }\n")
    assert plain.units == (Unit("t.rs", "/ui/t.rs", None, "bin", "2015"),), plain
    assert units("//@ check-pass\nfn main() {}\n") == Classified(passing=1)
    assert units("//@ run-pass\nfn main() {}\n").passing == 1
    assert units("//@ build-fail\nfn main() {}\n").out == (("t.rs", "rejected only by codegen (`build-fail`)"),)
    mixed = units("//@ revisions: ok bad\n//@[ok] check-pass\nfn main() {}\n")
    assert mixed.passing == 1 and [u.label for u in mixed.units] == ["t.rs#bad"], mixed
    assert mixed.units[0].flags == ("--cfg", "bad")


def test_classify_scope() -> None:
    cases = {
        "//@ aux-build: helper.rs\n": "needs an auxiliary crate",
        "//@ proc-macro: m.rs\n": "needs an auxiliary crate",
        "//@ compile-flags: -Znext-solver\n": "compile flag -Znext-solver",
        "//@ compile-flags: -Z polonius=next\n": "compile flag -Zpolonius",
        "//@ only-windows\n": "`only-windows`: for another target",
        "//@ ignore-linux\n": "`ignore-linux`",
        "//@ ignore-test\n": "`ignore-test`",
        "//@ needs-sanitizer-address\n": "`needs-sanitizer-address`: not met here",
        "//@ rustc-env: X=1\n": "sets rustc's environment (`rustc-env`)",
        "//@ edition: future\n": "edition `future`",
        "//@ compile-flags: --extern foo\n": "compile flag --extern",
        "//@ compile-flags: --crate-type proc-macro\n": "crate type proc-macro",
        "#![no_core]\n": "`#![no_core]`",
    }
    for head, why in cases.items():
        got = units(head + "fn main() {}\n")
        assert got.out == (("t.rs", why),), (head, got)
    for head in ("//@ only-x86_64\n", "//@ only-linux\n", "//@ ignore-windows\n", "//@ ignore-compare-mode-next-solver\n",
                 "//@ needs-unwind\n", "//@ needs-target-has-atomic: 64,ptr\n", "//@ compile-flags: -Zui-testing\n",
                 "//@ known-bug: #1\n"):
        got = units(head + "fn main() {}\n")
        assert len(got.units) == 1 and not got.out, (head, got)


def test_classify_shape() -> None:
    lib = units('#![crate_type = "lib"]\npub fn f() {}\n').units[0]
    assert lib.kind == "lib", lib
    assert units("//@ compile-flags: --crate-type=rlib\n").units[0].kind == "lib"
    assert units('//@ compile-flags: --crate-type bin\n#![crate_type = "lib"]\n').units[0].kind == "bin"
    assert units("//@ compile-flags: --test\n").units[0].kind == "test"
    assert units("//@ edition: 2021\n").units[0].edition == "2021"
    assert units("//@ edition: 2018..\n").units[0].edition == "2018"
    assert units("//@ edition: ..2021\n").units[0].edition == "2015"
    assert units("//@ compile-flags: --edition=2024\n").units[0].edition == "2024"
    got = units("//@ compile-flags: -A dead_code -Dwarnings -C opt-level=3 -Cincremental=/x --error-format=human -O\n")
    assert got.units[0].flags == ("-A", "dead_code", "-D", "warnings", "-C", "opt-level=3", "-O"), got
    assert units("//@ compile-flags: --cfg 'feature=\"a b\"'\n").out[0][1].startswith("compile flag --cfg with a space")


def test_flag_of() -> None:
    assert flag_of("--cfg foo") == ("--cfg", "foo")
    assert flag_of("--edition=2021") == ("--edition", "2021")
    assert flag_of("-Copt-level=3") == ("-C", "opt-level=3")
    assert flag_of("-C opt-level=3") == ("-C", "opt-level=3")
    assert flag_of("-O") == ("-O", None)
    assert flag_of("--test") == ("--test", None)


def test_host_names() -> None:
    mac = host_names("darwin", "arm64")
    assert {"macos", "apple", "aarch64", "aarch64-apple-darwin", "unix", "64bit"} <= mac and "linux" not in mac
    assert "x86_64-unknown-linux-gnu" in HOST


def test_shard() -> None:
    tests = [f"dir/t{i}.rs" for i in range(200)]
    shards = [[t for t in tests if in_shard(t, Shard(i, 4))] for i in range(4)]
    assert sorted(t for shard in shards for t in shard) == sorted(tests)
    assert all(shards), "a shard of 200 files is empty"


def diagnostic(level: str, message: str, code: str | None = None) -> str:
    return json.dumps({"$message_type": "diagnostic", "level": level, "message": message,
                       "code": {"code": code, "explanation": None} if code else None, "spans": []})


def test_keys() -> None:
    assert key_of("E0277", "anything") == "E0277"
    assert key_of(None, "`AtomicU32::load` is not yet stable as a const fn") == \
        "message: `_` is not yet stable as a const fn"
    assert key_of(None, "x" * 100) == MESSAGE + "x" * MESSAGE_START
    stderr = "\n".join([diagnostic("error", "mismatched types", "E0308"), diagnostic("warning", "unused", "unused"),
                        diagnostic("error", "`f` is not yet stable as a const fn"),
                        diagnostic("error", "aborting due to 2 previous errors"), "not json"])
    assert reference_of(1, stderr) == Reference(frozenset({"E0308", "message: `_` is not yet stable as a const fn"}))
    assert reference_of(0, "").out is not None and reference_of(0, "").error is None
    assert reference_of(1, "error: linking failed").error is not None
    assert reference_of(101, diagnostic("error: internal compiler error", "boom")).out == "rustc crashes on it"


def message(reason: str, target: str, body: str | None = None) -> str:
    line: dict[str, Any] = {"reason": reason, "package_id": target, "target": {"name": target, "kind": ["lib"]}}
    if body is not None:
        line["message"] = json.loads(body)
    return json.dumps(line)


def test_cargo_messages() -> None:
    stdout = "\n".join([message("compiler-message", "u0_0", diagnostic("error", "missing field", "E0063")),
                        message("compiler-message", "u0_0", diagnostic("warning", "unused", "dead_code")),
                        message("compiler-artifact", "u0_1"), '{"reason":"build-finished","success":false}'])
    errors, every, checked = cargo_messages(stdout)
    assert errors == {"u0_0": frozenset({"E0063"})} and every == frozenset({"E0063"}) and checked == {"u0_1"}
    together = said_together(stdout, ["u0_0", "u0_1", "u0_2"], "", None)
    assert together["u0_0"].keys == {"E0063"} and together["u0_1"] == Said() and together["u0_2"].error
    assert said_alone(101, stdout, "", None).keys == {"E0063"}
    assert said_alone(0, "", "", None) == Said()
    assert said_alone(101, "", "", None).error is not None


def test_verdict() -> None:
    rustc = frozenset({"E0063"})
    assert verdict(rustc, Said(frozenset({"E0063"})))[:2] == ("pass", "same")
    assert verdict(rustc, Said())[:2] == ("fail", "accepted")
    assert verdict(rustc, Said())[2].startswith("accepted what rustc rejects")
    assert verdict(rustc, Said(frozenset({"E0308"})))[:2] == ("fail", "other codes")
    assert verdict(rustc, Said(frozenset({"E0063", "E0308"})))[1] == "other codes"
    assert verdict(rustc, Said(rustc, who="cargo"))[:2] == ("fail", "not blaze's")
    assert verdict(rustc, Said(error="timed out"))[:2] == ("error", "error")


def test_header() -> None:
    tokens, issue = header_of('//@ rejects: E0277, "is not yet stable as a const fn"\n//@ issue: #1440\n')
    assert tokens == ("E0277", "message: is not yet stable as a const fn") and issue == "#1440", tokens
    keys = frozenset({"E0277", "message: `_` is not yet stable as a const fn"})
    assert header_holds(tokens, keys)
    assert not header_holds(tokens[:1], keys), "a header that leaves out an error rustc gives holds"
    assert not header_holds(("E0277", "E0308"), frozenset({"E0277"})), "a header naming more holds"
    assert not header_holds(("E0277",), frozenset()), "a header holds of what rustc accepts"


def test_packages() -> None:
    lib = Unit("a.rs", "/ui/a.rs", None, "lib", "2021")
    assert '[lib]\npath = "src/lib.rs"' in manifest("u0_0", lib) and 'edition = "2021"' in manifest("u0_0", lib)
    assert '[[bin]]\nname = "u0_1"' in manifest("u0_1", Unit("b.rs", "/ui/b.rs", None, "bin", "2015"))
    assert check_args(lib, "u0_0") == ["-p", "u0_0", "--lib", "--message-format=json"]
    assert check_args(Unit("c.rs", "", None, "test", "2015"), None) == [
        "--workspace", "--keep-going", "--tests", "--message-format=json"]
    many = [Unit(f"{i}.rs", "", None, "bin", "2015", ("--cfg", "x") if i % 2 else ()) for i in range(5)]
    assert sorted(len(b) for b in batches_of(many, 2)) == [1, 2, 2]
    assert all(len({(u.kind, u.flags) for u in b}) == 1 for b in batches_of([*many, lib], 10))
    with tempfile.TemporaryDirectory() as tmp:
        source = Path(tmp) / "ui"
        (source / "sub").mkdir(parents=True)
        (source / "sub" / "t.rs").write_text("mod helper;\nfn main() {}\n")
        (source / "sub" / "helper.rs").write_text("")
        (source / "sub" / "lost.rs").write_text("mod gone;\nfn main() {}\n")
        found = [Unit("sub/t.rs", str(source / "sub" / "t.rs"), None, "bin", "2015"),
                 Unit("sub/lost.rs", str(source / "sub" / "lost.rs"), None, "bin", "2015")]
        members, out = write_workspace(Path(tmp) / "ws", 3, found)
        assert list(members) == ["u3_0"] and [u.test for u, _ in out] == ["sub/lost.rs"], (members, out)
        assert (Path(tmp) / "ws" / "crates" / "u3_0" / "src" / "helper.rs").is_file()
        assert (Path(tmp) / "ws" / "Cargo.toml").read_text() == WORKSPACE


def test_fixed_cases() -> None:
    cases = fixed_cases(HOST)
    assert len(cases) >= 4, cases
    issues = {issue for _, _, issue in cases}
    assert {"#1522", "#1440"} <= issues, issues
    named = {Path(u.path).name: (u, tokens) for u, tokens, _ in cases}
    assert named["missing-field-in-test.rs"][0].kind == "test" and named["missing-field-in-test.rs"][1] == ("E0063",)
    assert named["assert-eq-without-debug.rs"][1] == ("E0277",)
    assert named["non-const-call-in-const.rs"][1] == ("E0015",)
    assert named["const-atomic-load.rs"][1] == (MESSAGE + "is not yet stable as a const fn",)
    assert named["const-atomic-load.rs"][0].kind == "lib"


def test_kept_answers() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "a b").mkdir()
        main, module = root / "t.rs", root / "a b" / "m.rs"
        main.write_text("mod m;\n")
        module.write_text("fn f() {}\n")
        out = root / "out"
        out.mkdir()
        assert what_was_read(out, root) is None, "no dep-info is not an answer that can be kept"
        (out / "t.d").write_text(f"{out}/libt.rmeta: {main} a\\ b/m.rs\n\n{out}/t.d: {main} a\\ b/m.rs\n\n"
                                 f"{main}:\na\\ b/m.rs:\n\n# env-dep:SET=1\n# env-dep:UNSET\n")
        files, variables = dep_info((out / "t.d").read_text(), root)
        assert files == [str(main), str(module)], files
        assert variables == [("SET", "1"), ("UNSET", None)], variables
        read = what_was_read(out, root)
        assert read is not None and [p for p, _ in read[0]] == sorted([str(main), str(module)]), read
        kept = engine.Kept(str(root / "kept"), "rustc")
        files_read, env_read = read
        kept.put("k", dict(files=[list(f) for f in files_read], env=[list(v) for v in env_read],
                           keys=["E0063"], out=None))
        env = {"SET": "1"}
        assert kept_reference(kept.get("k"), env, "k") == Reference(frozenset({"E0063"}))
        assert kept_reference(kept.get("k"), {"SET": "2"}, "k") is None, "a variable changed"
        assert kept_reference(kept.get("k"), {"SET": "1", "UNSET": "x"}, "k") is None, "a variable unset is set"
        module.write_text("fn g() {}\n")
        assert kept_reference(kept.get("k"), env, "k") is None, "a module it read changed"
        module.unlink()
        assert kept_reference(kept.get("k"), env, "k") is None, "a module it read is gone"
        assert kept_reference(None, env, "k") is None
        for bad in ([], {"files": [["a"]], "env": [], "keys": [], "out": None}, {"files": [], "env": [], "keys": []}):
            try:
                kept_reference(bad, env, "k")
            except engine.Failed:
                continue
            raise AssertionError(f"a kept answer of another shape was taken: {bad}")
    unit = Unit("t.rs", "/x/t.rs", None, "lib", "2021")
    assert "--emit=dep-info,metadata" in rustc_argv(unit, "/o")


SELFTESTS = (test_directives, test_classify_modes, test_classify_scope, test_classify_shape, test_flag_of,
             test_host_names, test_shard, test_keys, test_cargo_messages, test_verdict, test_header, test_packages,
             test_fixed_cases, test_kept_answers)


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
