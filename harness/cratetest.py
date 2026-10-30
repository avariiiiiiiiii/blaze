#!/usr/bin/env python3
"""A crate's own tests under blaze, judged by their own results.

usage: cratetest.py <crate-dir> [--crate <name>] [--features a,b] [--test <target>]... [--skip <test>]...
                    [--known <test>]... [--json out.json]

Each `tests/*.rs` target, and the crate's unit tests (`--test unit`), is built the mixed way
(mixed.py): cargo builds it and prints its link line, and blaze's objects for the crate go on
that line ahead of rustc's rlib. The binary is run in the crate's directory, as `cargo test`
runs it, and passes when every test that runs passes, but those `--known` names (libtest's whole
names: the known failures), which must fail. A run that exits other than 0 with no test failed
(a test that aborts hides the ones after it) is run again a test at a time, so the crash is
named by its test.

Prints PASS, FAIL or UNTESTED (blaze compiled nothing of the crate, which is no result) with
the counts, and exits 0, 1 or 2.
"""
import argparse, collections, json, os, re, shutil, subprocess, sys
import linkline
from blaze_cli import test_results, test_verdict
from mixed import Fixture, RUNNER, bucket, label


UNIT = "(unit tests)"
TEST_SECS = 900


class TestFixture(Fixture):
    def __init__(self, crate_dir, crate, test, path, shared, features=None, skip=()):
        if test == UNIT:
            # The crate itself, built with `cfg(test)`: its `#[cfg(test)]` modules are the tests.
            super().__init__(crate_dir, [crate], ["--lib", "--profile", "test"],
                             touch=path)
            self.blaze_args = ["--unit-test"]
        else:
            # By its file: an example may have the name of the test.
            super().__init__(crate_dir, [crate, os.path.abspath(path)], ["--test", test], touch=path)
            self.blaze_args = ["--dev-deps"]
        if features:
            self.target_args += ["--features", features]
            self.blaze_args += ["--features", features]
        # A test left out by name: `--exact` keeps a name from leaving out the ones it starts.
        self.skips = ["--exact", *(x for name in skip for x in ("--skip", name))] if skip else []
        self.skipped = set(skip)
        self.run_args = ["--test-threads=1", *self.skips]
        self.shared = shared

    def rustc_build(self, target_dir=None):
        super().rustc_build(target_dir)
        self.cargo_binary = linkline.output_of(self.link_args)
        # A test that names nothing of the crate is linked without it: there is nothing of the
        # crate in the binary for blaze to stand in front of.
        # The standard library's own copy of a crate of the same name (hashbrown) is not it.
        linked = any(os.path.basename(r).startswith(f"lib{self.crates[0]}-") and "/rustlib/" not in r
                     for r in self.rlibs)
        if len(self.crates) == 2 and not linked:
            print(f"{self.crates[0]} is not linked into this test")
            self.crates = self.crates[1:]

    def binary_path(self):
        # Beside cargo's: a test may look for the crate's other artifacts from where it runs
        # (autocfg probes the rlibs in the directory of `current_exe`).
        return self.cargo_binary + "-blaze"

    def blaze_build(self, deny=(), env=None):
        # The crate's objects are the same whichever test binary they are linked into.
        # blaze links to what the rlibs already hold, so that only goes for the same rlibs.
        key = tuple(self.rlibs)
        if deny or key not in self.shared:
            super().blaze_build(deny, env)
            if self.leaves_to_rustc():
                # rustc lays a future out its own way, so what the test leaves to rustc decides
                # what of the crate can be blaze's: one run has to see both.
                self.crates = [",".join(self.crates)]
                super().blaze_build(deny, env)
            if not deny:
                self.shared[key] = (self.objects, self.reports)
        else:
            objects, self.reports = self.shared[key]
            self.link(objects)

    def leaves_to_rustc(self):
        """Whether the test, compiled apart from the crate, left a function of its own to rustc."""
        rows = self.reports[self.crates[-1]] if len(self.crates) == 2 else []
        return any(r[0] == "skipped" and bucket(r[2]) != "no rustc symbol for function" for r in rows)

    def run_tests(self, args):
        """The test binary run with `args` in the crate's directory, as `cargo test` runs it;
        None past TEST_SECS."""
        env = {**os.environ, "CARGO_MANIFEST_DIR": self.ws}
        try:
            return subprocess.run([*RUNNER, self.binary, *args], cwd=self.ws, env=env, capture_output=True, text=True,
                                  errors="replace", stdin=subprocess.DEVNULL, timeout=TEST_SECS)
        except subprocess.TimeoutExpired:
            return None


def crate_stats(fx):
    """How much of each crate blaze compiled, and why it left the rest, by reason."""
    found = {}
    for crate, rows in fx.reports.items():
        n = sum(r[0] == "compiled" for r in rows)
        total = sum(r[0] in ("compiled", "skipped") for r in rows)
        reasons = collections.Counter(bucket(r[2]) for r in rows if r[0] == "skipped")
        no_symbol = reasons.pop("no rustc symbol for function", 0)
        found[crate] = {"compiled": n, "roots": total - no_symbol, "not_roots": no_symbol,
                        "instances": int(rows[0][2]), "codegen_ms": int(rows[0][4]) / 1000, "skipped": dict(reasons)}
        print(f"{label(crate)}: {n}/{total - no_symbol} functions from blaze")
        for r in rows:
            if r[0] == "skipped" and bucket(r[2]) != "no rustc symbol for function":
                why = r[2] if len(r[2]) < (10**6 if os.environ.get("BLAZE_WHY") else 400) else r[2][:100] + " ... " + r[2][-300:]
                print(f"  skipped {r[1]}: {why}")
    return found


def one_at_a_time(fx):
    """Every test run on its own: one that aborts hides the ones after it. `test_results`'s shape."""
    listed = fx.run_tests(["--list"])
    names = [l[:-len(": test")] for l in (listed.stdout if listed else "").splitlines() if l.endswith(": test")]
    tests = collections.Counter()
    for name in names:
        if name in fx.skipped:
            continue
        ran = fx.run_tests(["--test-threads=1", "--exact", name])
        if ran is not None and ran.returncode == 0:
            tests[(name, "ran")] += 1
            continue
        tests[(name, "FAILED")] += 1
        tail = " | ".join((ran.stdout + ran.stderr).strip().splitlines()[-3:])[:300] if ran else f"took more than {TEST_SECS} s"
        print(f"  FAILED alone {name}: {'status ' + str(ran.returncode) if ran else 'timeout'}: {tail}")
    failed = sum(outcome == "FAILED" for _, outcome in tests)
    return (1 if failed else 0), tests, (0, len(tests) - failed, failed, 0, 0)


def judge(fx, known):
    """The binary's run judged by its own tests: (the tests that ran, the failed ones, what is
    wrong or None). A known failure is held to the binary that runs it."""
    ran = fx.run_tests(fx.run_args)
    if ran is None:
        return set(), [], f"the run took more than {TEST_SECS} s"
    results = test_results(ran)
    if results[0] != 0 and not any(outcome == "FAILED" for _, outcome in results[1]):
        print(f"  exited {results[0]} with no test failed: each test again on its own")
        results = one_at_a_time(fx)
    tests = results[1]
    names = {name for name, _ in tests}
    failed = sorted(name for name, outcome in tests if outcome == "FAILED")
    for name in failed:
        print(f"  FAILED {name}{' (known)' if name in known else ''}")
    return names, failed, test_verdict(results, frozenset(known) & names)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("crate_dir")
    ap.add_argument("--crate")
    ap.add_argument("--test", action="append", help="only these test binaries; `unit` is the crate's own")
    ap.add_argument("--features", help="cargo features to turn on, comma separated")
    ap.add_argument("--skip", action="append", default=[], help="a test, by its whole name, to leave out")
    ap.add_argument("--known", action="append", default=[], help="a test, by its whole name, known to fail")
    ap.add_argument("--json")
    a = ap.parse_args()
    crate = a.crate or os.path.basename(os.path.abspath(a.crate_dir)).replace("-", "_")
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=a.crate_dir))
    package = next(p for p in meta["packages"] if p["name"].replace("-", "_") == crate
                   or any(t["name"].replace("-", "_") == crate for t in p["targets"]))
    # The test targets as cargo sees them: one may be named otherwise than its file, or be a
    # directory.
    tests = sorted((t["name"], t["src_path"]) for t in package["targets"] if "test" in t["kind"])
    lib_target = next((t for t in package["targets"] if "lib" in t["kind"]), None)
    lib = lib_target and lib_target["src_path"]
    if lib_target and not a.crate:
        # A package may name its library otherwise (`md-5` has `md5`, `redox_syscall` `syscall`).
        crate = lib_target["name"].replace("-", "_")
    if lib:
        tests.append((UNIT, lib))
    tests = [(t, path) for t, path in tests if not a.test or t in a.test or (t == UNIT and "unit" in a.test)]
    results, shared, left_out, seen = [], {}, [], set()
    for test, path in tests:
        print(f"--- {test}", flush=True)
        fx = TestFixture(a.crate_dir, crate, test, path, shared, a.features, a.skip)
        try:
            fx.rustc_build()
            fx.blaze_build()
        except SystemExit as e:
            # One test target that does not build or link must not hide the others.
            # blaze sees the crate with its default features, so only those are tested.
            if "requires the features" in str(e):
                print("left out: needs features that are off by default")
                left_out.append({"test": test, "why": "needs features"})
                continue
            # The mixed model's cargo build of it failed: a dev-dependency the published crate lacks, say.
            if str(e).startswith("cargo failed"):
                print(f"left out: cargo does not build it: {str(e)[-300:]}")
                errors = re.findall(r"^error(?:\[E\d+\])?: .*", str(e), re.M)
                left_out.append({"test": test, "why": "cargo fails", "error": (errors or [str(e)[-200:]])[0][:200]})
                continue
            print(f"ERROR: {str(e)[:1500]}")
            results.append({"test": test, "passed": False, "tests_run": 0, "failed": [], "error": str(e)[:300],
                            "crates": {}, "work": fx.work})
            continue
        crates = crate_stats(fx)
        names, failed, wrong = judge(fx, a.known)
        seen |= names
        print(f"{'PASS' if wrong is None else 'FAIL'} {test}: {len(names)} tests" + (f": {wrong}" if wrong else ""))
        results.append({"test": test, "passed": wrong is None, "tests_run": len(names), "failed": failed,
                        "wrong": wrong, "crates": crates, "work": fx.work})
    unseen = sorted(set(a.known) - seen)
    ok = bool(results) and all(r["passed"] for r in results) and not unseen
    total = sum(r["tests_run"] for r in results)
    merged = {"fixture": crate, "passed": ok, "tests_run": total,
              "crates": results[0]["crates"] if results else {},
              "binaries": results, "left_out": left_out, "known": sorted(a.known), "known_unseen": unseen}
    if unseen:
        print(f"known to fail, and no test binary ran: {', '.join(unseen[:8])}")
    # A crate blaze compiled nothing of passes by what rustc built alone, which is no result.
    untested = ok and not sum(c["compiled"] for r in results for c in r["crates"].values())
    if untested:
        merged["untested"] = True
        print(f"UNTESTED: {len(results)} test binaries, {total} tests, no function from blaze")
    else:
        print(f"{'PASS' if ok else 'FAIL'}: {len(results)} test binaries, {total} tests")
    if a.json:
        json.dump(merged, open(a.json, "w"), indent=1)
    for r in results:
        # Scratch directories are only worth keeping for what did not pass.
        if r["passed"]:
            shutil.rmtree(r["work"], ignore_errors=True)
    sys.exit(2 if untested else 0 if ok else 1)


if __name__ == "__main__":
    main()
