"""engine.py: the blaze a v2 proof script measures, v1 or v2, behind one interface.

Every script under harness/v2 takes, in its typed input, an `engine`:

    {"kind": "v1", "blaze": "target/release/blaze", "backend": "cranelift"}

and does everything through `Engine`. Builds, runs and tests go through the `blaze` command
line (`blaze build`, `run`, `test`, `check`, `status`, `stop`), which v1 and v2 both have, so a
script is the same for both. What only an engine's insides can say (what an edit recompiled,
the codegen inputs of a build, the critical path of a cold build) is a method here:

- a method v1 has no answer to raises `Unmeasurable`, and the result says "unmeasured": a run
  against v1 still sets the bar;
- a method whose v2 answer the agent API (`crates/blaze-api`, docs/v2/ci.md) has no request or
  response for raises `Missing`, naming it, which fails the script: the request for it is in
  `v2/requests/proof.md`. Nothing is guessed.

`Failed` is a step the harness could not do (a command that timed out, a daemon that did not
come up): an error of the run, never a finding about the compiler.

The cargo reference (`reference_env`) is plain cargo with the pinned nightly
(`harness/toolchain`), no incremental compilation and no wrapper: what every comparison is
against.
"""

import hashlib
import os
import re
import stat
import subprocess
import sys
import time
from abc import ABC, abstractmethod
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

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
V1_DONE = re.compile(r"^(edit-to-binary:|cold: compiled|stats: )", re.M)
SETTLE_SECS = 3.0
QUIET_SECS = 0.25
# `blaze_api::SCHEMA`, the agent API `V2` reads.
V2_SCHEMA = 3

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


class Missing(Failed):
    """What was asked needs an interface the engine does not have yet, named."""

    def __init__(self, what: str):
        super().__init__(f"{what}: blaze-api schema {V2_SCHEMA} has no request for it (v2/requests/proof.md)")


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
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


OUTPUT_SUFFIXES = (".rlib", ".rmeta", ".so", ".dylib", ".dll", ".a", ".lib", ".exe", ".wasm", ".pdb")


def outputs(target: Path, profile: str = "debug") -> dict[str, str]:
    """The files a build of `target/<profile>` delivers, each by its path under it and its
    sha256: every executable and library at its top, in `examples/` and in `deps/` (test
    programs). What cargo keeps for itself (`.fingerprint`, `build`, `incremental`, dep-info,
    `.cargo-lock`) and blaze's own `target/blaze` are not outputs."""
    top = target / profile
    found: dict[str, str] = {}
    for sub in (top, top / "examples", top / "deps"):
        if not sub.is_dir():
            continue
        for path in sorted(sub.iterdir()):
            try:
                info = path.lstat()
            except OSError:
                continue
            if not stat.S_ISREG(info.st_mode):
                continue
            executable = bool(info.st_mode & 0o111) and not path.suffix
            if executable or path.suffix in OUTPUT_SUFFIXES:
                found[str(path.relative_to(top))] = digest(path)
    return found


def first_difference(a: Path, b: Path) -> str:
    """Where two files first differ: the byte offset, or which is longer."""
    with open(a, "rb") as fa, open(b, "rb") as fb:
        at = 0
        while True:
            x, y = fa.read(1 << 16), fb.read(1 << 16)
            if x != y:
                for i, (p, q) in enumerate(zip(x, y)):
                    if p != q:
                        return f"byte {at + i:#x}: {p:#04x} against {q:#04x}"
                return f"one ends at byte {at + min(len(x), len(y)):#x}"
            if not x:
                return "the same"
            at += len(x)


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
        instance's content key: what the oracle compares bit for bit (docs/v2/proof.md)."""

    @abstractmethod
    def critical_path_s(self, ws: Path, env: dict[str, str]) -> float:
        """The length in seconds of the longest chain of work the last cold build of `ws` waited on."""


class V1(Engine):
    """Today's blaze (`blaze` beside `blaze-engine`): a daemon of workers, each logging what it
    built under `BLAZE_HOME`; a build it cannot do is cargo's, and says so."""

    def not_blazes(self, ran: Ran) -> str | None:
        import blaze_cli

        how = blaze_cli.how_built(ran.stderr)
        return None if how == "daemon" or how.startswith("unchanged") else how

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
        def text() -> str:
            out: list[str] = []
            for path in self.logs(home):
                with open(path, "rb") as f:
                    f.seek(mark.sizes.get(str(path), 0))
                    out.append(f.read().decode(errors="replace"))
            return "".join(out)

        # A worker writes what it did after it answered the client: wait for it to be quiet.
        began, said, quiet = time.monotonic(), text(), time.monotonic()
        while time.monotonic() - began < SETTLE_SECS:
            time.sleep(0.05)
            now = text()
            if now != said:
                said, quiet = now, time.monotonic()
            elif V1_DONE.search(said) and time.monotonic() - quiet >= QUIET_SECS:
                break
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


class V2(Engine):
    """blaze v2 (docs/v2/README.md): one daemon per machine, whose only interface is the agent
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

    def mark(self, home: Path) -> Mark:
        raise Missing("what a request did (queries rerun and found green, functions compiled, the link), "
                      "for `mark` and `work_since`")

    def work_since(self, home: Path, mark: Mark) -> Work:
        raise Missing("what a request did (queries rerun and found green, functions compiled, the link)")

    def codegen_inputs(self, ws: Path, env: dict[str, str]) -> dict[str, str]:
        raise Missing("the codegen inputs of a view's last build: each `InstanceKey` with its `CodeHash`")

    def critical_path_s(self, ws: Path, env: dict[str, str]) -> float:
        raise Missing("the critical path of a request, as the plane measured it")


def platform_name() -> str:
    return {"linux": "linux", "darwin": "macos", "win32": "windows"}.get(sys.platform, sys.platform)
