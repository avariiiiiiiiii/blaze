"""mixed.py: a workspace built the mixed way: cargo with the pinned nightly builds it and prints
its link line, and blaze's objects of the chosen crates go on that line ahead of rustc's.

That is how v1's daemon links (`blaze-engine WORKSPACE --crate ... --link-cmd ...`), and every
harness that drives a daemon that way builds its fixture here. rustc's part of it is the no-rustc
bug of the mixed model (docs/no-rustc.md), not an answer anything is held to: no harness compares
what blaze built with what rustc built. What a harness holds a daemon's incremental program to is
`from_scratch`: blaze's own build of the same source from nothing.

BLAZE_TARGET=wasm32-wasip1 builds for that target and runs the programs under wasmtime
(BLAZE_WASM_RUNNER names another runner); the harnesses take it the same way. See WASM32.md.
"""
import re
import glob, json, os, shlex, subprocess, tempfile
import linkline

# blaze computes layouts with rust-analyzer's copy of rustc_abi, so everything it links against
# has to come from the rustc that copy was taken from.
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "toolchain")).read().strip())

# rustc makes what only one codegen unit uses local to it unless it is told to keep unused code.
# blaze needs the symbols: a private `static mut` has to be the one object for both compilers.
os.environ.setdefault("RUSTFLAGS", "-C link-dead-code")
# Incremental caches of a sweep over many crates fill the disk.
os.environ.setdefault("CARGO_INCREMENTAL", "0")
# Set, not unset: cargo's `[env]` (as in ~/.cargo/config.toml) fills in only what is not already
# set, so an unset RUST_BACKTRACE would take its value and panics would print differently.
os.environ["RUST_BACKTRACE"] = "0"

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WINDOWS = os.name == "nt"
BLAZE = os.environ.get("BLAZE_BIN") or os.path.join(ROOT, "target/release/blaze-engine" + (".exe" if WINDOWS else ""))


def host():
    return subprocess.check_output(["rustc", "-vV"], text=True).split("host: ")[1].split()[0]


# BLAZE_TARGET names the triple to build for when it is not the host. Cargo builds for it, blaze
# emits for it, and the binaries run under what BLAZE_RUNNER names (for aarch64 Linux on another
# machine, qemu's user mode with the sysroot the cross linker links against).
TARGET = os.environ.get("BLAZE_TARGET") or None
CROSS = {
    "aarch64-unknown-linux-gnu": {"linker": "aarch64-linux-gnu-gcc", "runner": "qemu-aarch64 -L /usr/aarch64-linux-gnu",
                                  "binutils": "aarch64-linux-gnu-"},
}
if TARGET and TARGET != host():
    _cross, _env = CROSS.get(TARGET, {}), "CARGO_TARGET_" + TARGET.upper().replace("-", "_")
    if "linker" in _cross:
        os.environ.setdefault(_env + "_LINKER", _cross["linker"])
    if "runner" in _cross:
        os.environ.setdefault("BLAZE_RUNNER", _cross["runner"])
        os.environ.setdefault(_env + "_RUNNER", os.environ["BLAZE_RUNNER"])
if TARGET:
    # Every cargo a harness runs builds for the target, and so does every blaze.
    os.environ["CARGO_BUILD_TARGET"] = TARGET
# What the names of strip and the like begin with for the target.
BINUTILS = os.environ.get("BLAZE_BINUTILS") or (CROSS.get(TARGET, {}).get("binutils", "") if TARGET and TARGET != host() else "")
# What a built binary is started with.
RUNNER = shlex.split(os.environ.get("BLAZE_RUNNER", ""))
# What cargo and blaze take to build for the target.
TARGET_ARGS = ["--target", TARGET] if TARGET else []


def profile_dir(target_dir, profile="debug"):
    """Where cargo puts what it builds under a target directory."""
    return os.path.join(target_dir, *([TARGET] if TARGET else []), profile)


def toolchain():
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    return sysroot, host()


def only_the_link_failed(stderr):
    """Whether all that failed in a build with stand-ins is the link of the program. The
    stand-ins have none of the workspace's code, so native code linked into the program that
    calls a function of the workspace (C calling a `#[no_mangle]` function, as bun's does) finds
    it nowhere; blaze's objects define it in the link that is run, and what is taken from
    cargo's is the link line, which rustc prints first."""
    errors = [line for line in stderr.splitlines() if line.startswith("error")]
    return bool(errors) and errors[0].startswith("error: linking with ") and all(
        line.startswith(("error: linking with ", "error: could not compile ")) for line in errors)


def linker_line(output):
    """The line of `--print link-args`, as the arguments from the linker on: the C compiler
    where it drives the linker, and a linker of the `link.exe` kind (rustc's own `rust-lld`,
    or MSVC's) for the MSVC target."""
    for line in output.splitlines():
        if '"' not in line:
            continue
        try:
            args = shlex.split(line)
        except ValueError:
            continue
        if "-o" in args:
            # The environment the driver runs in comes first, as assignments.
            link = args[next(i for i, a in enumerate(args) if "=" not in a):]
            if link[0] == "rust-lld":
                # The linker of a wasm target, which rustc finds in what it puts first in `PATH`.
                path = next(a for a in args if a.startswith("PATH=")).removeprefix("PATH=")
                link[0] = next(p for d in path.split(os.pathsep) if os.path.exists(p := os.path.join(d, link[0])))
            return link
        for at, arg in enumerate(args[:8]):
            if os.path.basename(arg).lower().removesuffix(".exe") in ("rust-lld", "lld-link", "link") and "/NOLOGO" in args:
                if arg == "rust-lld":
                    # rustc finds its own linker beside itself rather than on PATH.
                    sysroot, triple = toolchain()
                    arg = os.path.join(sysroot, "lib", "rustlib", triple, "bin", "rust-lld.exe")
                return [arg, *args[at + 1:]]
    raise SystemExit(f"no linker line:\n{output[-3000:]}")


# `wasm32-wasip1` runs under wasmtime.
WASM = bool(TARGET) and TARGET.startswith("wasm32")
if WASM and not RUNNER:
    # What runs a wasm program, which gets the program and its arguments after it.
    RUNNER = shlex.split(os.environ.get("BLAZE_WASM_RUNNER", "wasmtime run -W max-wasm-stack=8388608 -S inherit-env"))
# What a link line gets after blaze's objects, which define again what rustc's do.
AHEAD = linkline.allowing_duplicates(WASM)


def command(binary):
    """What runs a built program."""
    return [*RUNNER, binary]


def built(target_dir, name):
    """Where cargo leaves the program `name`."""
    return os.path.join(profile_dir(target_dir), name + (".wasm" if WASM else ".exe" if WINDOWS else ""))


def sysroot_rlibs():
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    lib = f"{sysroot}/lib/rustlib/{TARGET or host()}/lib"
    return [p for n in ("core", "alloc", "std") for p in glob.glob(f"{lib}/lib{n}-*.rlib")]


def label(crate):
    """A crate may be given as its root file, and several that one run compiles with commas."""
    return "+".join(os.path.basename(c).removesuffix(".rs") for c in crate.split(","))


class Fixture:
    def __init__(self, workspace, crates, target, touch=None):
        """`target` selects what cargo links: ["-p", "app"] or ["--test", "name"]."""
        self.ws, self.crates, self.target_args = os.path.abspath(workspace), crates, target
        self.work = tempfile.mkdtemp(prefix="blaze-mixed-")
        self.run_args = []
        self.blaze_args = []
        self.blaze_timeout = None
        if touch is None:
            meta = json.loads(subprocess.check_output(
                ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=self.ws))
            touch = next(t["src_path"] for p in meta["packages"] if p["name"] == target[1]
                         for t in p["targets"] if "bin" in t["kind"])
        self.touch = touch
        # Whether rustc builds stand-ins for the workspace's crates, all of their code being blaze's.
        self.alone = bool(os.environ.get("BLAZE_ALONE"))

    def rustc_build(self, target_dir=None):
        """Builds with cargo and captures the linker line so blaze objects can be spliced in.
        `target_dir` is cargo's target directory; none is the workspace's own."""
        os.utime(self.touch)
        env, target = dict(os.environ), []
        # In the workspace's own target directory: cargo names a package's artifacts by its
        # path in its workspace, so programs of one name built at once in the caller's would
        # take each other's place (fuzz/edits.py's, all of them `fz`).
        env.pop("CARGO_TARGET_DIR", None)
        if target_dir:
            env["CARGO_TARGET_DIR"] = target_dir
        if self.alone:
            env.update(RUSTC_WORKSPACE_WRAPPER=BLAZE, BLAZE_WRAPS_RUSTC="1",
                       CARGO_TARGET_DIR=target_dir or os.path.join(self.ws, "target-alone"))
            target = ["--target", TARGET or host()]
        elif TARGET:
            target = TARGET_ARGS
        out = subprocess.run(
            ["cargo", "rustc", *self.target_args, *target, "--", "--print", "link-args", "-C", "save-temps"],
            cwd=self.ws, capture_output=True, text=True, env=env)
        if out.returncode != 0 and not (self.alone and only_the_link_failed(out.stderr)):
            raise SystemExit(f"cargo failed:\n{out.stderr[-3000:]}")
        # rustc's object of the names the program keeps is how GNU ld, reading each archive once,
        # takes the panic runtime that comes before the standard library calling it.
        self.link_args = [a for a in linker_line(out.stdout + out.stderr)
                          if not a.endswith("symbols.o") or os.path.exists(a)]
        # The target's own objects too: a crate blaze rebuilds may be the one being linked.
        self.rlibs = [a for a in self.link_args if a.endswith((".rlib", ".rcgu.o"))]

    def blaze_build(self, deny=(), env=None):
        """Compiles each crate with blaze and links the objects ahead of rustc's rlibs."""
        deny_file = os.path.join(self.work, "deny.txt")
        open(deny_file, "w").write("\n".join(deny))
        symbols = [x for rlib in self.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
        objects, self.reports = [], {}
        for crate in self.crates:
            obj, report = (os.path.join(self.work, f"{label(crate)}.{ext}") for ext in ("o", "tsv"))
            try:
                run = subprocess.run(
                    [BLAZE, self.ws, *(x for c in crate.split(",") for x in ("--crate", c)), "--out", obj, "--report", report,
                     "--deny", deny_file, *self.blaze_args, *TARGET_ARGS, *symbols],
                    capture_output=True, text=True, timeout=self.blaze_timeout, env=env or dict(os.environ))
            except subprocess.TimeoutExpired:
                raise SystemExit(f"blaze crashed on {crate}:\nerror: no result after {self.blaze_timeout}s")
            open(os.path.join(self.work, f"{label(crate)}.stderr"), "w").write(run.stderr)
            if os.environ.get("BLAZE_STDERR"): open(os.environ["BLAZE_STDERR"], "a").write(run.stderr)
            if run.returncode != 0:
                raise SystemExit(f"blaze crashed on {crate}:\n{run.stderr[-2000:]}")
            objects.append(obj)
            self.reports[crate] = [l.rstrip("\n").split("\t") for l in open(report)]
        self.objects = objects
        self.link(objects)

    def link(self, objects):
        args = list(self.link_args)
        first = linkline.first_of_rustc(args)
        if self.alone:
            # Nothing of the workspace is rustc's for blaze's to be defined a second time.
            args[first:first] = objects
        else:
            args = linkline.ahead(args, objects, BLAZE)
        self.binary = self.binary_path()
        args = linkline.with_output(args, self.binary)
        run = subprocess.run(args, capture_output=True, text=True)
        if run.returncode != 0:
            raise SystemExit(f"link failed:\n{(run.stderr + run.stdout)[-20000:]}")

    def binary_path(self):
        """Where the binary with blaze's objects is linked."""
        return os.path.join(self.work, "bin-blaze")

    def compiled(self):
        return [r[1] for rows in self.reports.values() for r in rows if r[0] == "compiled"]

    def behaviour(self, binary, allowed=900):
        """(exit status, stdout, stderr) of a run of `binary`, with what differs from run to run
        (thread ids, times, addresses) written alike; ("timeout", b"", b"") past `allowed` seconds."""
        try:
            run = subprocess.run([*RUNNER, binary, *self.run_args], capture_output=True, timeout=allowed)
            # The thread id in a panic or stack overflow message differs from run to run.
            stderr = re.sub(rb"' \(\d+\) (panicked|has overflowed)", rb"' \1", run.stderr)
            if WASM:
                # After the program's own output the runtime says where in which file it
                # trapped: only the kind of trap is the program's.
                program, _, runtime = stderr.partition(b"Error: failed to run main module")
                stderr = program + b"".join(re.findall(rb"wasm trap: [^\n]*\n?", runtime))
            # Loggers stamp their lines with the time, on either stream.
            stamp = rb"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?"
            # What prints an address prints another one in every run.
            address = rb"0x[0-9a-f]{12}\b"
            stdout, stderr = (
                re.sub(address, b"<address>", re.sub(stamp, b"<time>", it)) for it in (run.stdout, stderr)
            )
            return run.returncode, self.normalize(stdout), stderr
        except subprocess.TimeoutExpired:
            return "timeout", b"", b""

    def normalize(self, stdout):
        return stdout


def from_scratch(workspace, crates, target, run_args=(), env=None, blaze_args=()):
    """What blaze's build from nothing of the workspace as it is now does when run: (exit status,
    stdout, stderr), as `Fixture.behaviour` gives it. Every crate of `crates` is compiled by a
    blaze process of its own, with no daemon and no store, and linked as a daemon links it, into
    a target directory of its own beside the workspace's, so the source is read at the path the
    daemon reads it at. A harness holds a daemon's incremental program to this: the two differ
    only where an edit reached the daemon's build wrong. `blaze_args` are what each blaze is
    given besides, as `Fixture.blaze_args`."""
    fx = Fixture(workspace, crates, target)
    fx.run_args = list(run_args)
    fx.blaze_args = list(blaze_args)
    fx.rustc_build(os.path.join(fx.work, "target"))
    clean = {k: v for k, v in {**os.environ, **(env or {})}.items() if k != "BLAZE_CACHE"}
    fx.blaze_build(env=clean)
    return fx.behaviour(fx.binary)


def printed(behaviour):
    """A run as `Fixture.behaviour` or `from_scratch` gives it, written as the harnesses that
    compare text write one: what it printed, then `[exit N]` where it did not exit 0."""
    status, stdout, _ = behaviour
    if status == "timeout":
        return "did not run: timeout"
    return stdout.decode(errors="replace") + (f"[exit {status}]" if status else "")


def symptom(expected, got):
    """How the run `got` differs from `expected`, both as `Fixture.behaviour` gives them: the
    exit status, else the first line of standard output that differs."""
    if got[0] != expected[0]:
        return f"exit status {got[0]}"
    want, have = expected[1].decode(errors="replace").splitlines(), got[1].decode(errors="replace").splitlines()
    for w, h in zip(want, have):
        if w != h:
            return f"expected `{w}`, got `{h}`"
    return "output length differs"


def bucket(error):
    """Why blaze left a function out, without the payload, so the same gap in different functions
    counts once."""
    reason = error.rsplit("`: ", 1)[-1]
    reason = re.sub(r"^(unsupported|frontend|backend): ", "", reason)
    for prefix in ("drop glue", "unsizing", "aggregate of", "expected scalar", "cast "):
        if reason.startswith(prefix):
            return {"aggregate of": "odd-sized aggregate in a register", "cast ": reason.split("(")[-1].rstrip(")") + " cast"}.get(prefix, prefix)
    return re.split(r"[({]", reason)[0].strip()[:60]


def minimize(items, fails):
    """Delta debugging: a smaller list for which `fails` still holds."""
    n = 2
    while len(items) >= 2:
        size = max(len(items) // n, 1)
        chunks = [items[i:i + size] for i in range(0, len(items), size)]
        for i, chunk in enumerate(chunks):
            rest = [x for j, c in enumerate(chunks) if j != i for x in c]
            if fails(chunk):
                items, n = chunk, 2
                break
            if len(chunks) > 2 and fails(rest):
                items, n = rest, max(n - 1, 2)
                break
        else:
            if size == 1:
                break
            n = min(n * 2, len(items))
    return items
