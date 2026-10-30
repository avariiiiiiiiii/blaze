#!/usr/bin/env python3
"""runbench.py: how fast the programs blaze's LLVM builds run, against rustc's own builds.

usage: runbench.py [NAME...] [--rounds 5] [--builds release,checked,blaze] [--variant ...]
                   [--json out.json] [--table out.md]

Every program is built each of these ways, then each of its benchmarks is run `--rounds`
times, the builds taking turns, and reported by its median wall time:

  release   `cargo build --release`
  checked   the same with `-C overflow-checks=on -C debug-assertions=on`: release optimization
            with what the dev profile checks, which is what blaze's code of a dev build has
            to keep
  blaze    every crate linked into the program compiled by blaze (`apps.py`'s "alone"
            build) with the release profile's semantics (`--release`: no overflow checks, no
            debug assertions), LLVM at -O3 (`BLAZE_BACKEND=llvm BLAZE_OPT=3`)

`--variant NAME:VAR=VALUE,...` adds an blaze build with those variables set too (say
`--variant blaze-mir2:BLAZE_MIR_OPT=2`). Before timing, each benchmark is run once with
every build, and has to exit 0: a build whose run does not is reported and not timed.

NAME is an entry of apps/apps.json with a `bench` (checked out as apps.py does, under
APPS_DIR). Default: numeric, jsonrt, ripgrep, fd, tokei, eza, bat.

Memory is kept beside the times (memory.py, docs/benchmarks.md "Memory and disk"): each run's
peak resident memory as the kernel kept it (`wait4`), the size of each program, and what each
build held: `build_largest_mb`, the largest one process (rustc, blaze, the linker), exact, and
`build_peak_mb`, the most the build's processes held at once, sampled.

Every build is cold, its target directory removed first, and `build_s` is the whole of it: for
blaze, cargo's pass with blaze standing in for rustc (build scripts, proc macros, the
libraries' metadata; `cargo_s`), blaze's compile (`blaze_s`) and the link (`link_s`).

The ratio is blaze's time over release's (1.00 is as fast, 1.10 is 10% slower), with the
geometric mean over the benchmarks. BLAZE_BIN names the blaze used; it needs `--features
llvm`. Python 3.11+, stdlib only.
"""
import argparse, json, math, os, shlex, shutil, statistics, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import apps, memory  # noqa: E402

DEFAULT = ["numeric", "jsonrt", "ripgrep", "fd", "tokei", "eza", "bat"]
BLAZE_ENV = {"BLAZE_BACKEND": "llvm", "BLAZE_OPT": "3"}
# What a variant may not inherit from the shell: the build is said by its flags alone.
OWN_VARS = ["BLAZE_BACKEND", "BLAZE_OPT", "BLAZE_EMIT", "BLAZE_MIR_OPT", "BLAZE_INLINE",
            "BLAZE_BUILT_IN", "BLAZE_LLVM_INLINE_THRESHOLD",
            "RUSTFLAGS", "CARGO_INCREMENTAL", "BLAZE_ALONE"]


def spec_of(name):
    spec = json.load(open(apps.SPECS))[name]
    if "bench" not in spec:
        raise SystemExit(f"{name}: apps.json has no `bench` for it")
    return {**spec, "benches": {name: spec["bench"]}}


def workspace(name, spec):
    """The program's source, its dependencies vendored beside it."""
    ws = apps.checkout(name, spec)
    apps.prepare(ws, spec)
    apps.localize(ws)
    os.environ.update(spec.get("build_env", {}))
    apps.setup(ws, spec)
    return ws


def cargo_release(ws, spec, label, rustflags):
    target = os.path.join(ws, f"target-{label}")
    # Cold, as blaze's build is: the build time is published beside it.
    shutil.rmtree(target, ignore_errors=True)
    flags = f"--cap-lints allow {apps.own_flags(spec)} {rustflags}"
    env = {**os.environ, "CARGO_TARGET_DIR": target, "RUSTFLAGS": " ".join(flags.split())}
    for var in OWN_VARS[:-3]:
        env.pop(var, None)
    argv = ["cargo", "build", "--release", "-p", spec["package"], *spec.get("cargo_args", [])]
    took, ran, used = memory.run(argv, ws, env)
    if ran.returncode != 0:
        raise SystemExit(f"`{shlex.join(argv)}` failed:\n{(ran.stdout + ran.stderr)[-3000:]}")
    # Beside blaze's, as a program may look for files next to itself (helix its `runtime`).
    kept = os.path.join(ws, f"bin-{label}")
    shutil.copy2(os.path.join(target, "release", spec.get("bin", spec["package"])), kept)
    return kept, took, used


def blaze_release(ws, spec, label, extra):
    """blaze's program of every crate, with the release profile. Returns (binary, seconds,
    summary, fixture)."""
    saved = {var: os.environ.get(var) for var in OWN_VARS}
    try:
        for var in OWN_VARS:
            os.environ.pop(var, None)
        # Not apps.FLAGS: its `-C link-dead-code` keeps the symbols of rustc's code for blaze's to
        # be linked ahead of, and rustc builds none here. It would link without `-dead_strip` or
        # `--gc-sections`, as the release build does not, and one object of every function then
        # has the calls of code no build keeps (bun's of Windows' API on macOS) to resolve.
        flags = f"--cap-lints allow {apps.own_flags(spec)}"
        os.environ.update({**BLAZE_ENV, **extra, "RUSTFLAGS": flags.strip()})
        fx = apps.App(ws, {**spec, "cargo_args": [*spec.get("cargo_args", []), "--release"]}, alone=True)
        fx.blaze_args = ["--release"]
        # Cold: cargo's pass with blaze standing in for rustc (build scripts, proc macros,
        # the libraries' metadata) is part of the build, as rustc's is of cargo's.
        shutil.rmtree(os.path.join(ws, "target-alone"), ignore_errors=True)
        fx.rustc_build()
        fx.blaze_build()
        kept = os.path.join(ws, f"bin-{label}")
        shutil.copy2(fx.binary, kept)
        parts = {"cargo_s": round(fx.cargo_secs, 1), "blaze_s": round(fx.blaze_secs, 1),
                 "link_s": round(fx.link_secs, 1)}
        return kept, fx.cargo_secs + fx.blaze_secs + fx.link_secs, {**fx.summary(), "parts": parts}, fx
    finally:
        for var, value in saved.items():
            if value is None:
                os.environ.pop(var, None)
            else:
                os.environ[var] = value


def blaze_measured(ws, spec, label, extra):
    """`blaze_release` in a process of its own, so that the memory of what it ran (cargo with
    blaze's stand-ins, blaze, the linker) is its alone: (binary, seconds, summary, memory)."""
    (binary, secs, summary), used = memory.in_child(lambda: blaze_release(ws, spec, label, extra)[:3])
    return binary, secs, summary, used


class Runner:
    """Runs a benchmark's command line with one build's program."""

    def __init__(self, ws, spec):
        self.ws, self.spec = ws, spec
        self.argv0 = spec.get("bin", spec["package"])
        self.env = {**os.environ, **spec.get("env", {})}
        for var in OWN_VARS:
            self.env.pop(var, None)

    def failure(self, binary, argv):
        """None when one run of the benchmark exits 0; else its status and the end of what it said."""
        try:
            ran = subprocess.run([self.argv0, *argv], executable=binary, cwd=self.ws, env=self.env,
                                 capture_output=True, stdin=subprocess.DEVNULL, timeout=1800)
        except subprocess.TimeoutExpired:
            return "did not finish in 1800 s"
        if ran.returncode == 0:
            return None
        return f"exited {ran.returncode}: {(ran.stdout + ran.stderr).decode(errors='replace').strip()[-300:]}"

    def seconds(self, binary, argv):
        """The seconds of one run, and the peak resident memory of the program (its tree's
        largest process, as the kernel kept it)."""
        began = time.monotonic()
        child = subprocess.Popen([self.argv0, *argv], executable=binary, cwd=self.ws, env=self.env,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
        largest = memory.waited(child)
        return time.monotonic() - began, largest


def geomean(values):
    values = [v for v in values if v and v > 0]
    return math.exp(sum(math.log(v) for v in values) / len(values)) if values else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("names", nargs="*")
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("--builds", default="release,checked,blaze")
    ap.add_argument("--variant", action="append", default=[])
    ap.add_argument("--json")
    ap.add_argument("--table")
    a = ap.parse_args()
    names = a.names or DEFAULT
    builds = [it for it in a.builds.split(",") if it]
    variants = {}
    for variant in a.variant:
        label, _, assigns = variant.partition(":")
        variants[label] = dict(it.split("=", 1) for it in assigns.split(",") if it)
        builds.append(label)
    if "release" not in builds:
        builds.insert(0, "release")
    results = {"builds": builds, "variants": variants, "programs": {}}

    for name in names:
        spec = spec_of(name)
        print(f"== {name}", flush=True)
        ws = workspace(name, spec)
        programs, info = {}, {}
        for build in builds:
            try:
                if build == "release":
                    programs[build], secs, used = cargo_release(ws, spec, build, "")
                    info[build] = {"build_s": round(secs, 1)}
                elif build == "checked":
                    programs[build], secs, used = cargo_release(
                        ws, spec, build, "-C overflow-checks=on -C debug-assertions=on")
                    info[build] = {"build_s": round(secs, 1)}
                else:
                    extra = {} if build == "blaze" else variants[build]
                    binary, secs, summary, used = blaze_measured(ws, spec, build, extra)
                    programs[build] = binary
                    info[build] = {"build_s": round(secs, 1), **summary["parts"], "compiled": summary["compiled"],
                                   "left": summary["left"], "reasons": summary["reasons"]}
                # What building it held (memory.py), and what it built.
                info[build].update(build_peak_mb=used["peak_mb"], build_largest_mb=used["largest_mb"],
                                   size_mb=round(os.path.getsize(programs[build]) / 1e6, 2))
                print(f"  built {build} in {info[build]['build_s']} s"
                      + (f": {info[build]['compiled']} functions by blaze, {info[build]['left']} left"
                         if "compiled" in info[build] else ""), flush=True)
            except SystemExit as failed:
                info[build] = {"error": str(failed)[-2000:]}
                print(f"  {build}: FAILED {str(failed)[-1500:]}", flush=True)
        runner = Runner(ws, spec)
        benches = {}
        for bench, argv in spec["benches"].items():
            timed = []
            for build in builds:
                if build not in programs:
                    continue
                if (why := runner.failure(programs[build], argv)) is not None:
                    print(f"  {bench}: {build} FAILED: {why}", flush=True)
                    info[build].setdefault("failed_runs", []).append(bench)
                    continue
                timed.append(build)
            if "release" not in timed:
                print(f"  {bench}: not timed, with no release run to be the baseline", flush=True)
                continue
            times, held = {build: [] for build in timed}, {build: [] for build in timed}
            for round_ in range(a.rounds):
                # Each round starts with another build, so what the machine does meanwhile
                # falls on all of them alike.
                order = timed[round_ % len(timed):] + timed[: round_ % len(timed)]
                for build in order:
                    took, largest = runner.seconds(programs[build], argv)
                    times[build].append(took)
                    held[build].append(largest)
            medians = {build: statistics.median(ts) for build, ts in times.items()}
            base = medians["release"]
            row = {build: {"median_s": round(m, 4), "ratio": round(m / base, 3),
                           "runs": [round(t, 4) for t in times[build]],
                           "peak_mb": round(statistics.median(held[build])), "peak_max_mb": max(held[build])}
                   for build, m in medians.items()}
            benches[bench] = row
            print(f"  {bench}: " + ", ".join(f"{b} {row[b]['median_s']:.3f}s ({row[b]['ratio']:.2f}) {row[b]['peak_mb']} MB"
                                             for b in timed), flush=True)
        results["programs"][name] = {"builds": info, "benches": benches}
        if a.json:
            json.dump(results, open(a.json, "w"), indent=1)

    all_benches = [(p, b, row) for p, prog in results["programs"].items() for b, row in prog["benches"].items()]
    results["geomean"] = {build: geomean([row.get(build, {}).get("ratio") for _, _, row in all_benches])
                          for build in builds}
    if a.json:
        json.dump(results, open(a.json, "w"), indent=1)
    lines = ["| Benchmark | release (s) | " + " | ".join(f"{b} / release" for b in builds if b != "release") + " |",
             "|---|---|" + "---|" * (len(builds) - 1)]
    for program, bench, row in all_benches:
        cells = [f"{row[b]['ratio']:.2f}" if b in row else "run failed" if bench in results["programs"][program]["builds"].get(b, {}).get("failed_runs", []) else "failed"
                 for b in builds if b != "release"]
        lines.append(f"| {bench} | {row['release']['median_s']:.3f} | " + " | ".join(cells) + " |")
    lines.append("| geomean | | " + " | ".join(
        f"{results['geomean'][b]:.2f}" if results["geomean"][b] else "-" for b in builds if b != "release") + " |")
    table = "\n".join(lines)
    print("\n" + table)
    if a.table:
        open(a.table, "w").write(table + "\n")


if __name__ == "__main__":
    main()
