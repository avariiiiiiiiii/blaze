"""runs.py: generated programs built by blaze and run, and how two runs of one program differ.

What edits.py and store.py share: the backends they can name, the manifest of the one-crate
project a program is written into, a run within the limits a generated program has, and the
findings of a run that is not the one it is held to, case by case.
"""
import os, re, subprocess, sys

try:
    import resource
except ImportError:  # Windows: a generated program is held to its time alone.
    resource = None
if sys.platform == "darwin":  # macOS refuses to lower RLIMIT_AS, so there too.
    resource = None

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)
import gen
from mixed import Fixture, RUNNER

# What `--backends` can name. `llvm` is the pair the other harnesses run; the two after it are
# the other ends: no passes, and the passes with LLVM's own instruction selection.
BACKENDS = {
    "cranelift": {"BLAZE_BACKEND": "cranelift"},
    "llvm": {"BLAZE_BACKEND": "llvm", "BLAZE_OPT": "2", "BLAZE_EMIT": "fast"},
    "llvm-O0": {"BLAZE_BACKEND": "llvm", "BLAZE_OPT": "0", "BLAZE_EMIT": "fast"},
    "llvm-full": {"BLAZE_BACKEND": "llvm", "BLAZE_OPT": "2", "BLAZE_EMIT": "optimized"},
}
MANIFEST = """[package]
name = "fz"
version = "0.0.0"
edition = "{edition}"

[profile.dev]
debug = 0
"""
# Seconds a generated program may run, and bytes it may map, before it is the generator's fault.
RUN_SECONDS, RUN_BYTES = 120, 8 << 30


class Finding:
    """One way a build went wrong: what kind, in which case, and a key that names its cause."""

    def __init__(self, kind, case, key, detail):
        self.kind, self.case, self.key, self.detail = kind, case, key, detail


class Project:
    """A one-crate cargo project that programs are written into and built by blaze from."""

    def __init__(self, directory):
        self.dir = directory
        os.makedirs(os.path.join(directory, "src"), exist_ok=True)
        self.main = os.path.join(directory, "src", "main.rs")
        self.fx = None
        self.edition = None

    def write(self, text, edition):
        if edition != self.edition:
            open(os.path.join(self.dir, "Cargo.toml"), "w").write(MANIFEST.format(edition=edition))
            self.edition = edition
        open(self.main, "w", encoding="utf-8").write(text)
        if self.fx is None:
            self.fx = Fixture(self.dir, ["fz"], ["-p", "fz"], touch=self.main)
            self.fx.blaze_timeout = 600

    def build(self, text, edition, backend, store=None):
        """Builds the source with blaze under a backend, with the store `store` or none, and runs
        it. Returns what it did (as `run` gives it), or None when cargo, asked for the program's
        link line, turned the source down."""
        self.write(text, edition)
        try:
            self.fx.rustc_build()
        except SystemExit:
            return None
        env = {k: v for k, v in os.environ.items() if k != "BLAZE_CACHE"}
        env.update(BACKENDS[backend], **({"BLAZE_CACHE": store} if store else {}))
        self.fx.blaze_build(env=env)
        return run(self.fx.binary)

    def left(self):
        """The functions the last build left out, which blaze was to compile."""
        return [f"{row[1]}: {row[2]}" for rows in self.fx.reports.values() for row in rows
                if row[0] == "skipped" and "no rustc symbol" not in row[2]]


def run(binary):
    """What a program prints and how it ends, within the limits a generated program has."""
    limit = (lambda: resource.setrlimit(resource.RLIMIT_AS, (RUN_BYTES, RUN_BYTES))) if resource else None
    try:
        done = subprocess.run([*RUNNER, binary], capture_output=True, timeout=RUN_SECONDS, preexec_fn=limit)
    except subprocess.TimeoutExpired:
        return "timeout", "", ""
    return done.returncode, done.stdout.decode(errors="replace"), done.stderr.decode(errors="replace")


def per_case(stdout):
    """The output split by case: what each printed, up to and including its checksum line."""
    cases, lines = {}, []
    for line in stdout.splitlines():
        lines.append(line)
        found = re.fullmatch(r"(c\d+) [0-9a-f]{16} (ok|panic)", line)
        if found:
            cases[found.group(1)] = "\n".join(lines)
            lines = []
    return cases, "\n".join(lines)


def differences(expected, got):
    """A finding per case whose output in the run `got` is not what it printed in `expected`."""
    if expected == got:
        return []
    want, _ = per_case(expected[1])
    have, rest = per_case(got[1])
    findings = []
    for name, text in want.items():
        if name not in have:
            # The program died in the first case it did not finish.
            detail = f"the program ended with status {got[0]} in this case\n{rest[-600:]}\n{got[2][-600:]}"
            findings.append(Finding("differs", name, "died", detail))
            break
        if have[name] != text:
            findings.append(Finding("differs", name, difference_key(text, have[name]), f"expected:\n{text}\ngot:\n{have[name]}"))
    if not findings:
        findings.append(Finding("differs", None, "status", f"expected: status {expected[0]} {expected[2][-300:]}\ngot: status {got[0]} {got[2][-300:]}"))
    return findings


def difference_key(want, have):
    """What kind of difference two outputs of a case have: enough to tell causes apart roughly."""
    a, b = want.splitlines(), have.splitlines()
    for x, y in zip(a, b):
        if x != y:
            if x.startswith("panic: panicked at") and y.startswith("panic: panicked at"):
                return "panic location"
            if x.startswith("panic:") != y.startswith("panic:"):
                return "panics differ"
            return "panic message" if len(a) > 1 and x is not a[-1] else "checksum"
    return "panics differ"


def edition_of(seed):
    """Half of the programs are built as 2021 and half as 2024: drop order differs between them."""
    return "2021" if sum(seed.encode()) % 2 else "2024"


def smoke_programs():
    """The programs of the short run on fixed seeds, for CI: (seed, source, edition)."""
    programs = []
    for number in range(2):
        seed = f"smoke-{number}"
        modules, _, _ = gen.program(seed, 25)
        programs.append((seed, gen.render(modules), edition_of(seed)))
    return programs
