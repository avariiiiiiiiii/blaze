"""engine.py: the blaze a v2 proof script measures, v1 or v2, behind one interface.

Every script under harness/v2 takes, in its typed input, an `engine`:

    {"kind": "v1", "blaze": "target/release/blaze", "backend": "cranelift"}

and does everything through `Engine`. Builds, runs and tests go through the `blaze` command
line (`blaze build`, `run`, `test`, `check`, `status`, `stop`), which v1 and v2 both have, so a
script is the same for both. What only an engine's insides can say (what an edit recompiled,
the codegen inputs of a build, the critical path of a cold build) is a method here:

- a method v1 has no answer to raises `Unmeasurable`, and the result says "unmeasured": a run
  against v1 still sets the bar;
- v2 answers each from the agent API (`crates/blaze-api`) through the command line's forms of
  it (`crates/blaze-ui/src/api.rs`): what a request did from the `blaze-work` line a build, test
  or run prints under `--message-format=json`; `blaze codegen-inputs --format=json`,
  `blaze trace --out PATH` and `blaze tables --format=json` for the rest. An answer it cannot
  read is `Failed`, naming the command. Nothing is guessed.

`Failed` is a step the harness could not do (a command that timed out, a daemon that did not
come up): an error of the run, never a finding about the compiler.

The cargo reference (`reference_env`) is plain cargo with the pinned nightly
(`harness/toolchain`), no incremental compilation and no wrapper: what every comparison is
against.
"""

import concurrent.futures
import hashlib
import io
import json
import mmap
import os
import re
import stat
import subprocess
import sys
import threading
import time
from abc import ABC, abstractmethod
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, cast

HERE = Path(__file__).resolve().parent
HARNESS = HERE.parent
ROOT = HARNESS.parent
TOOLCHAIN = (HARNESS / "toolchain").read_text().strip()
EXE = ".exe" if os.name == "nt" else ""
# Read from the caller's environment by cargo, rustc or blaze, and set by the harness alone.
SCRUBBED = (
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "BLAZE_CACHE",
    "BLAZE_HOME",
    "BLAZE_BIN",
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_EMIT",
    "BLAZE_MEMORY_BUDGET",
)
# What the v1 daemon's workers log about each build (crates/blaze-engine), read by `work_since`.
V1_COMPILED = re.compile(r"^stats: (\d+) of (\d+) functions compiled", re.M)
V1_STORE = re.compile(r"^store: (\d+) functions found, (\d+) compiled and added$", re.M)
V1_LINK = re.compile(r"^link: \S+ \((.*)\)$", re.M)
V1_DONE_BYTES = re.compile(rb"^(edit-to-binary:|cold: compiled|stats: )", re.M)
SETTLE_SECS = 3.0
QUIET_SECS = 0.25
# The commands whose request ends in `Response::Work` (crates/blaze-api/src/build.rs), which the
# command line prints under `--message-format=json` as a `blaze-work` line
# (crates/blaze-ui/src/api.rs, `work_json`).
WORKING = ("build", "test", "run")
WORK_COUNTS = ("reran", "green", "compiled", "found", "critical_path_us")

type Kind = Literal["v1", "v2"]
type Backend = Literal["cranelift", "llvm"]


@dataclass(frozen=True)
class EngineSpec:
    """The `engine` field of every script's input."""

    kind: Kind
    # The `blaze` command line client (v1: the `blaze` beside `blaze-engine`).
    blaze: str
    backend: Backend = "cranelift"


class Unmeasurable(Exception):
    """What was asked has no answer in this engine (v1 has no codegen inputs to compare)."""


class Failed(Exception):
    """A step the harness could not do, said whole: the command, its status, what it said."""


@dataclass(frozen=True)
class Ran:
    """A command that ran to its end. Shaped as `subprocess.CompletedProcess` where
    `blaze_cli.program_output` and `test_results` read it."""

    argv: tuple[str, ...]
    seconds: float
    returncode: int
    stdout: str
    stderr: str

    def said(self, tail: int = 3000) -> str:
        return (self.stdout + self.stderr)[-tail:]

    def checked(self, step: str) -> "Ran":
        if self.returncode != 0:
            raise Failed(f"{step}: `{' '.join(self.argv)}` exited {self.returncode}:\n{self.said()}")
        return self


@dataclass(frozen=True)
class Work:
    """What one or more builds did, as the engine says it: `compiled` functions compiled (code
    generated, not found in the store), `reran` queries executed rather than found green, and
    how the program was linked. None where the engine does not say."""

    compiled: int | None
    reran: int | None = None
    found: int | None = None
    link: str | None = None
    said: str = ""
    green: int | None = None


@dataclass(frozen=True)
class Table:
    """One of the daemon's long-lived tables (docs/v2/plan/design/data.md): how many elements
    it holds and the bytes of all its columns and side tables together."""

    elements: int
    bytes: int


@dataclass(frozen=True)
class Mark:
    """Where an engine's logs were at a moment: `work_since` reads what came after it."""

    sizes: dict[str, int] = field(default_factory=dict[str, int])


def run(step: str, argv: list[str], cwd: Path, env: dict[str, str], timeout: float = 3600, check: bool = False) -> Ran:
    """Runs `argv` to its end. A command that outlives `timeout`, or with `check` exits other
    than 0, is a `Failed` naming the step."""
    began = time.monotonic()
    try:
        done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8",
                              errors="replace", timeout=timeout, stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} took more than {timeout:.0f} s:\n{out[-3000:]}") from None
    except OSError as e:
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} did not start: {e}") from None
    ran = Ran(tuple(argv), time.monotonic() - began, done.returncode, done.stdout, done.stderr)
    if check and ran.returncode != 0:
        raise Failed(f"{step}: `{' '.join(argv)}` in {cwd} exited {ran.returncode}:\n{ran.said()}")
    return ran


def scrubbed() -> dict[str, str]:
    env = dict(os.environ)
    for name in SCRUBBED:
        env.pop(name, None)
    env.update(RUSTUP_TOOLCHAIN=TOOLCHAIN, RUST_BACKTRACE="0", CARGO_TERM_COLOR="never",
               CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT="false")
    return env


def reference_env(target: Path, extra: dict[str, str] | None = None) -> dict[str, str]:
    """Plain cargo with the pinned nightly into `target`: the reference of every comparison."""
    env = scrubbed()
    env.update(CARGO_TARGET_DIR=str(target), CARGO_INCREMENTAL="0", **(extra or {}))
    return env


def cargo(step: str, args: list[str], ws: Path, env: dict[str, str], timeout: float = 7200) -> Ran:
    return run(step, ["cargo", *args], ws, env, timeout)


def digest(path: Path) -> str:
    """The sha256 of the file at `path`, hashed mapped: one call over its bytes, no copy, and
    hashlib holds no GIL while it hashes, so `digests` uses every core. The file is a finished
    build's output, which nothing writes while it is hashed."""
    with open(path, "rb") as f:
        if os.fstat(f.fileno()).st_size == 0:
            return hashlib.sha256().hexdigest()
        with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as mapped:
            return hashlib.sha256(mapped).hexdigest()


def digests(files: Sequence[tuple[Path, int]]) -> list[str]:
    """`digest` of each (path, size) in `files`, in their order, on every core at once; the
    largest are started first, so one big file does not run alone at the end."""
    if len(files) < 2:
        return [digest(path) for path, _ in files]
    order = sorted(range(len(files)), key=lambda i: -files[i][1])
    out = [""] * len(files)
    with concurrent.futures.ThreadPoolExecutor(min(len(files), os.cpu_count() or 1)) as pool:
        for i, got in zip(order, pool.map(lambda i: digest(files[i][0]), order)):
            out[i] = got
    return out


OUTPUT_SUFFIXES = (".rlib", ".rmeta", ".so", ".dylib", ".dll", ".a", ".lib", ".exe", ".wasm", ".pdb")


def outputs(target: Path, profile: str = "debug") -> dict[str, str]:
    """The files a build of `target/<profile>` delivers, each by its path under it and its
    sha256: every executable and library at its top, in `examples/` and in `deps/` (test
    programs). What cargo keeps for itself (`.fingerprint`, `build`, `incremental`, dep-info,
    `.cargo-lock`) and blaze's own `target/blaze` are not outputs. A directory is listed once
    (its entries' types come with it); only the files chosen are `stat`ed."""
    top = target / profile
    chosen: list[tuple[str, Path, int]] = []
    for sub, prefix in ((top, ""), (top / "examples", "examples/"), (top / "deps", "deps/")):
        try:
            with os.scandir(sub) as listed:
                entries = sorted(listed, key=lambda e: e.name)
        except (FileNotFoundError, NotADirectoryError):
            continue
        for entry in entries:
            try:
                if not entry.is_file(follow_symlinks=False):
                    continue
                suffix = Path(entry.name).suffix
                if suffix and suffix not in OUTPUT_SUFFIXES:
                    continue
                info = entry.stat(follow_symlinks=False)
            except OSError:
                continue
            if suffix or info.st_mode & 0o111:
                chosen.append((prefix + entry.name, Path(entry.path), info.st_size))
    return dict(zip((name for name, _, _ in chosen), digests([(path, size) for _, path, size in chosen])))


def fill(f: io.FileIO, buffer: bytearray) -> int:
    """Reads into `buffer` until it is full or the file ends; how many bytes it holds."""
    view, got = memoryview(buffer), 0
    while got < len(buffer):
        n = f.readinto(view[got:])
        if not n:
            break
        got += n
    return got


def first_difference(a: Path, b: Path) -> str:
    """Where two files first differ: the byte offset, or which is longer. A megabyte of each at
    a time into two buffers made once, compared whole (one memcmp, no copy); the megabyte that
    differs is halved by memcmp too, so no byte is compared in Python but the last 64."""
    size = 1 << 20
    x, y = bytearray(size), bytearray(size)
    with open(a, "rb", buffering=0) as fa, open(b, "rb", buffering=0) as fb:
        at = 0
        while True:
            nx, ny = fill(fa, x), fill(fb, y)
            if nx == ny == size and x == y:
                at += size
                continue
            n = min(nx, ny)
            if n == size or x[:n] != y[:n]:
                lo, hi = 0, n
                while hi - lo > 64:
                    mid = (lo + hi) // 2
                    if x[lo:mid] == y[lo:mid]:
                        lo = mid
                    else:
                        hi = mid
                i = next(i for i in range(lo, hi) if x[i] != y[i])
                return f"byte {at + i:#x}: {x[i]:#04x} against {y[i]:#04x}"
            if nx != ny:
                return f"one ends at byte {at + n:#x}"
            if nx == 0:
                return "the same"
            at += nx


def tree_digest(root: Path, skip: Sequence[str] = ()) -> str:
    """The sha256 of a source tree: every file's path under `root`, whether it is executable
    and its sha256, and every symbolic link's target, in path order; the directories named in
    `skip` at the top are left out (build outputs)."""
    files: list[tuple[str, Path, int, bool]] = []
    links: list[tuple[str, str]] = []
    for here, dirs, names in os.walk(root):
        base = Path(here)
        if base == root:
            dirs[:] = [d for d in dirs if d not in skip]
        for name in [*dirs, *names]:
            path = base / name
            info = path.lstat()
            rel = path.relative_to(root).as_posix()
            if stat.S_ISLNK(info.st_mode):
                links.append((rel, os.readlink(path)))
            elif stat.S_ISREG(info.st_mode):
                files.append((rel, path, info.st_size, bool(info.st_mode & 0o111)))
    files.sort()
    links.sort()
    h = hashlib.sha256(b"blaze v2 proof tree 1\0")
    for (rel, _, _, executable), sha in zip(files, digests([(path, size) for _, path, size, _ in files])):
        h.update(f"f\0{rel}\0{int(executable)}\0{sha}\n".encode())
    for rel, to in links:
        h.update(f"l\0{rel}\0{to}\n".encode())
    return h.hexdigest()


class Kept:
    """Reference results (rustc's, cargo's) kept on disk across runs under `root/<kind>`, each
    a JSON file named by the sha256 of everything it depends on, which the caller gives
    whole: a rerun does again only what changed. `root` empty keeps nothing. An entry is
    written to a file of its own and renamed into place, so a reader never sees half of one."""

    def __init__(self, root: str, kind: str):
        self.dir = Path(root).expanduser() / kind if root else None
        self.domain = f"blaze v2 proof {kind} 1"

    def key(self, *parts: str) -> str:
        h = hashlib.sha256(self.domain.encode())
        for part in parts:
            h.update(b"\0" + part.encode())
        return h.hexdigest()

    def path(self, key: str) -> Path | None:
        return None if self.dir is None else self.dir / key[:2] / f"{key}.json"

    def get(self, key: str) -> object | None:
        path = self.path(key)
        if path is None:
            return None
        try:
            text = path.read_text()
        except FileNotFoundError:
            return None
        try:
            return json.loads(text)
        except json.JSONDecodeError as e:
            raise Failed(f"the kept result {path} is not JSON ({e}); delete it") from None

    def put(self, key: str, value: object) -> None:
        path = self.path(key)
        if path is None:
            return
        path.parent.mkdir(parents=True, exist_ok=True)
        part = path.with_name(f"{path.name}.{os.getpid()}.{threading.get_ident()}.part")
        part.write_text(json.dumps(value, sort_keys=True))
        part.replace(path)


def toolchain_identity(env: dict[str, str]) -> str:
    """What the pinned toolchain says it is (`rustc -vV`, `cargo -vV`): part of the key of every
    reference result kept."""
    said = [run(f"`{tool} -vV`", [tool, "-vV"], ROOT, env, 300, check=True).stdout for tool in ("rustc", "cargo")]
    return "\n".join(said)


class Engine(ABC):
    """The blaze under test. `Engine.of(spec)` gives the one of `spec.kind`."""

    def __init__(self, spec: EngineSpec):
        self.spec = spec
        self.blaze = Path(spec.blaze).resolve()
        if not self.blaze.is_file():
            raise Failed(f"engine.blaze: no file {self.blaze}")

    @staticmethod
    def of(spec: EngineSpec) -> "Engine":
        return V1(spec) if spec.kind == "v1" else V2(spec)

    @property
    def kind(self) -> Kind:
        return self.spec.kind

    def env(self, home: Path, store: Path | None = None, target: Path | None = None,
            extra: dict[str, str] | None = None) -> dict[str, str]:
        """The environment of a `blaze` command with a home (daemon, socket, logs) and a store of
        its own; the target directory is the workspace's `target` unless `target` is given."""
        home.mkdir(parents=True, exist_ok=True, mode=0o700)
        env = scrubbed()
        env.update(BLAZE_HOME=str(home), BLAZE_CACHE=str(store or home / "store"), BLAZE_STATS="1",
                   BLAZE_BACKEND=self.spec.backend, PATH=f"{self.blaze.parent}{os.pathsep}{env.get('PATH', '')}")
        if self.spec.backend == "llvm":
            env.update(BLAZE_OPT="2", BLAZE_EMIT="fast")
        if target is not None:
            env["CARGO_TARGET_DIR"] = str(target)
        env.update(extra or {})
        return env

    def command(self, step: str, command: str, args: list[str], ws: Path, env: dict[str, str],
                timeout: float = 7200) -> Ran:
        """`blaze COMMAND ARGS` in `ws`."""
        return run(step, [str(self.blaze), command, *args], ws, env, timeout)

    def stop(self, ws: Path, env: dict[str, str]) -> None:
        self.command("stopping the daemon", "stop", [], ws, env, 300)

    @abstractmethod
    def not_blazes(self, ran: Ran) -> str | None:
        """None when blaze built what `ran` asked for; otherwise who did, and why."""

    def build(self, step: str, ws: Path, env: dict[str, str], args: Sequence[str] = (), command: str = "build",
              timeout: float = 7200) -> Ran:
        """`blaze COMMAND ARGS` (build, run, test, check), served by blaze: see `not_blazes`.
        A command that fails is returned as it is; what it said is the caller's to compare."""
        return self.command(step, command, list(args), ws, env, timeout)

    @abstractmethod
    def wait_ready(self, ws: Path, env: dict[str, str], timeout: float = 3600) -> float:
        """Seconds until the daemon has `ws` loaded and can serve it."""

    @abstractmethod
    def mark(self, home: Path) -> Mark:
        """Where the record of what the daemon of `home` did is now."""

    @abstractmethod
    def work_since(self, home: Path, mark: Mark) -> Work:
        """What the builds since `mark` did (see `Work`)."""

    @abstractmethod
    def codegen_inputs(self, ws: Path, env: dict[str, str]) -> dict[str, str]:
        """The input of every function's code generation in the last build of `ws`, by the
        instance's content key: what the oracle compares bit for bit (docs/v2/plan/lanes/proof.md)."""

    @abstractmethod
    def critical_path_s(self, ws: Path, env: dict[str, str]) -> float:
        """The length in seconds of the longest chain of work the last cold build of `ws` waited on."""

    @abstractmethod
    def tables(self, ws: Path, env: dict[str, str]) -> dict[str, Table]:
        """The daemon's tables for `ws` by name (`token`, `syntax-node`, `hir-expr`,
        `mir-statement`, `memo`, `memo-edge`, `relocation`), for docs/v2/plan/design/data.md's size budgets."""

    @abstractmethod
    def trace(self, ws: Path, env: dict[str, str], to: Path) -> None:
        """Writes to `to` what the last build of `ws` left in the query database, as the
        deterministic CBOR of `blaze_v2_proof::Trace`, which `blaze-v2-proof` compares."""


class V1(Engine):
    """Today's blaze (`blaze` beside `blaze-engine`): a daemon of workers, each logging what it
    built under `BLAZE_HOME`; a build it cannot do is cargo's, and says so."""

    def not_blazes(self, ran: Ran) -> str | None:
        import blaze_cli

        how = cast(str, blaze_cli.how_built(ran.stderr))
        if how == "daemon" or how.startswith("unchanged"):
            return None
        # Only the `Finished` line of a build says `[blaze]`: a build blaze refused is its own
        # unless it says it went to cargo, or cargo's own `Finished` line is there.
        if how == "cargo" and ran.returncode != 0 and not blaze_cli.by_rustc(ran.stderr):
            return None
        return how

    def build(self, step: str, ws: Path, env: dict[str, str], args: Sequence[str] = (), command: str = "build",
              timeout: float = 7200) -> Ran:
        # The first build of a checkout can come before its worker has loaded it, and is then
        # cargo's: the next ones are the daemon's once it is ready.
        began = time.monotonic()
        while True:
            ran = self.command(step, command, list(args), ws, env, timeout)
            if ran.returncode != 0 or self.not_blazes(ran) is None or time.monotonic() - began > timeout:
                return ran
            if "loading" not in self.command(f"{step}: status", "status", [], ws, env, 300).stdout:
                return ran
            time.sleep(0.5)

    def wait_ready(self, ws: Path, env: dict[str, str], timeout: float = 3600) -> float:
        root = os.path.realpath(ws)
        began = time.monotonic()
        while time.monotonic() - began < timeout:
            status = self.command("the daemon's status", "status", [], ws, env, 300).stdout
            states = re.findall(rf"^    {re.escape(root)}: (.*)$", status, re.M)
            if states and not any(s.startswith("loading") for s in states):
                if any(s != "ready" for s in states):
                    raise Failed(f"the daemon did not load {root}: {states}")
                return time.monotonic() - began
            time.sleep(0.1)
        raise Failed(f"the daemon did not have {root} ready in {timeout:.0f} s")

    def logs(self, home: Path) -> list[Path]:
        # A worker's log is `workers/<id>/log`; one that began as a spare keeps the spare's.
        return sorted(set(home.glob("*/*/log")))

    def mark(self, home: Path) -> Mark:
        return Mark({str(p): p.stat().st_size for p in self.logs(home)})

    def work_since(self, home: Path, mark: Mark) -> Work:
        # Each log's bytes after the mark, read once: a poll reads only what was added since.
        read: dict[str, bytearray] = {}

        def grew() -> bool:
            more = False
            for path in self.logs(home):
                got = read.setdefault(str(path), bytearray())
                with open(path, "rb") as f:
                    f.seek(mark.sizes.get(str(path), 0) + len(got))
                    added = f.read()
                if added:
                    got += added
                    more = True
            return more

        def done() -> bool:
            return any(V1_DONE_BYTES.search(got) for got in read.values())

        # A worker writes what it did after it answered the client: wait for it to be quiet.
        began = time.monotonic()
        grew()
        quiet, finished = time.monotonic(), done()
        while time.monotonic() - began < SETTLE_SECS:
            time.sleep(0.05)
            if grew():
                quiet, finished = time.monotonic(), done()
            elif finished and time.monotonic() - quiet >= QUIET_SECS:
                break
        said = "".join(read[path].decode(errors="replace") for path in sorted(read, key=Path))
        compiled = sum(int(n) for n, _ in V1_COMPILED.findall(said))
        stored = V1_STORE.findall(said)
        links = V1_LINK.findall(said)
        again = [link for link in links if not link.startswith("spliced")]
        link = None if not links else "spliced" if not again else "full link: " + again[0][:160]
        return Work(compiled=compiled, reran=None, found=sum(int(f) for f, _ in stored) if stored else None,
                    link=link, said=said[-4000:])

    def codegen_inputs(self, ws: Path, env: dict[str, str]) -> dict[str, str]:
        raise Unmeasurable("v1 has no per-function codegen inputs to read; its programs are compared instead")

    def critical_path_s(self, ws: Path, env: dict[str, str]) -> float:
        raise Unmeasurable("v1 does not record the critical path of a build")

    def tables(self, ws: Path, env: dict[str, str]) -> dict[str, Table]:
        raise Unmeasurable("v1 keeps rowan trees and salsa memos, not columns it can count")

    def trace(self, ws: Path, env: dict[str, str], to: Path) -> None:
        raise Unmeasurable("v1 has no query database of fingerprints and edges to trace")


class V2(Engine):
    """blaze v2 (docs/v2/plan.md): one daemon per machine, whose only interface is the agent
    API; the command line is its client. Every build is its own (there is no path to cargo), so
    a build that exits 0 was blaze's."""

    def not_blazes(self, ran: Ran) -> str | None:
        return None

    def wait_ready(self, ws: Path, env: dict[str, str], timeout: float = 3600) -> float:
        # `Request::Status` answers once the daemon serves requests; a view is opened by the
        # request that names it, so there is no loading of a workspace to wait out.
        began = time.monotonic()
        while time.monotonic() - began < timeout:
            if self.command("the daemon's status", "status", [], ws, env, 300).returncode == 0:
                return time.monotonic() - began
            time.sleep(0.1)
        raise Failed(f"the daemon did not answer `blaze status` for {ws} in {timeout:.0f} s")

    def __init__(self, spec: EngineSpec):
        super().__init__(spec)
        # Each build, test and run through `build`, by its daemon's home: its step and its
        # `blaze-work` line, or None where it printed none.
        self.worked: dict[str, list[tuple[str, dict[str, object] | None]]] = {}

    def build(self, step: str, ws: Path, env: dict[str, str], args: Sequence[str] = (), command: str = "build",
              timeout: float = 7200) -> Ran:
        ran = self.command(step, command, list(args), ws, env, timeout)
        if command in WORKING:
            label = f"{step} (`{' '.join(['blaze', command, *args])}`)"
            lines = worked_lines(ran.stdout) if "--message-format=json" in args else []
            if len(lines) > 1:
                raise Failed(f"{label}: {len(lines)} `blaze-work` lines; a request reports what it did once")
            self.worked.setdefault(env["BLAZE_HOME"], []).append((label, lines[0] if lines else None))
        return ran

    def mark(self, home: Path) -> Mark:
        return Mark({"requests": len(self.worked.get(str(home), []))})

    def work_since(self, home: Path, mark: Mark) -> Work:
        since = self.worked.get(str(home), [])[mark.sizes["requests"]:]
        if not since:
            raise Failed(f"what the daemon of {home} did: no build, test or run since the mark")
        lines = [(label, required(label, line)) for label, line in since]

        def total(field: str) -> int:
            return sum(work_count(label, line, field) for label, line in lines)

        placed = [label for label, line in lines if line["link"] == "placed"]
        link = "patched in place" if not placed else f"placed whole: {placed[0]}"
        return Work(compiled=total("compiled"), reran=total("reran"), found=total("found"), link=link,
                    said="\n".join(json.dumps(line, sort_keys=True) for _, line in lines), green=total("green"))

    def codegen_inputs(self, ws: Path, env: dict[str, str]) -> dict[str, str]:
        # `blaze codegen-inputs --format=json` (`blaze_ui::api::codegen_inputs_json`): one object
        # from each instance key's hex to its code hash's hex.
        ran = self.command("the codegen inputs", "codegen-inputs", ["--format=json"], ws, env, 600)
        inputs = json_answer("the codegen inputs", ran)
        if not isinstance(inputs, dict) or not all(isinstance(v, str) for v in cast(dict[str, object], inputs).values()):
            raise Failed(f"`blaze codegen-inputs --format=json` printed no object of hex strings:\n{ran.said()}")
        return cast(dict[str, str], inputs)

    def critical_path_s(self, ws: Path, env: dict[str, str]) -> float:
        kept = self.worked.get(env["BLAZE_HOME"], [])
        if not kept:
            raise Failed(f"the critical path of the last build of {ws}: no build of it went through the harness")
        label, line = kept[-1]
        return work_count(label, required(label, line), "critical_path_us") / 1e6

    def tables(self, ws: Path, env: dict[str, str]) -> dict[str, Table]:
        # `blaze tables --format=json`: `{"<name>": {"elements": N, "bytes": N}}`.
        ran = self.command("the tables", "tables", ["--format=json"], ws, env, 600)
        answer = json_answer("the tables", ran)
        if not isinstance(answer, dict):
            raise Failed(f"`blaze tables --format=json` printed no object:\n{ran.said()}")
        out: dict[str, Table] = {}
        for name, sizes in cast(dict[str, object], answer).items():
            label = f"`blaze tables` table {name}"
            if not isinstance(sizes, dict):
                raise Failed(f"{label}: {sizes!r} is not {{\"elements\": N, \"bytes\": N}}")
            sizes = cast(dict[str, object], sizes)
            out[name] = Table(work_count(label, sizes, "elements"), work_count(label, sizes, "bytes"))
        return out

    def trace(self, ws: Path, env: dict[str, str], to: Path) -> None:
        # `blaze trace --out PATH` (`blaze_ui::api::trace_cbor`): the CBOR of `blaze_v2_proof::Trace`.
        to.unlink(missing_ok=True)
        self.command("the trace", "trace", ["--out", str(to)], ws, env, 600).checked("the trace")
        if not to.is_file():
            raise Failed(f"`blaze trace --out {to}` exited 0 and wrote no file")


def json_answer(step: str, ran: Ran) -> object:
    ran.checked(step)
    try:
        return json.loads(ran.stdout)
    except json.JSONDecodeError as e:
        raise Failed(f"{step}: `{' '.join(ran.argv)}` printed no JSON ({e}):\n{ran.said()}") from None


def worked_lines(stdout: str) -> list[dict[str, object]]:
    """The `blaze-work` lines of a command's `--message-format=json` output."""
    found: list[dict[str, object]] = []
    for text in stdout.splitlines():
        if not text.startswith("{") or '"blaze-work"' not in text:
            continue
        try:
            line = json.loads(text)
        except json.JSONDecodeError:
            continue
        if isinstance(line, dict) and cast(dict[str, object], line).get("reason") == "blaze-work":
            found.append(cast(dict[str, object], line))
    return found


def required(label: str, line: dict[str, object] | None) -> dict[str, object]:
    """`line`, checked whole; a request without one is a `Failed` naming it."""
    if line is None:
        raise Failed(f"{label}: no `blaze-work` line, so what it did is not known (it is printed only under "
                     "`--message-format=json`, and every build, test and run that runs reports it)")
    for field_name in WORK_COUNTS:
        work_count(label, line, field_name)
    if line.get("link") not in ("patched", "placed"):
        raise Failed(f"{label}: `blaze-work` has link {line.get('link')!r}, not \"patched\" or \"placed\": {line}")
    return line


def work_count(label: str, line: dict[str, object], field_name: str) -> int:
    value = line.get(field_name)
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise Failed(f"{label}: `blaze-work` has {field_name} {value!r}, not a count: {line}")
    return value


def platform_name() -> str:
    return {"linux": "linux", "darwin": "macos", "win32": "windows"}.get(sys.platform, sys.platform)
