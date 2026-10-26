#!/usr/bin/env python3
"""monitor.py: the daemon's monitor (docs/monitor.md) shows an edit's patch and the tests it
ran over Flecs REST, as an agent would ask for them.

usage: monitor.py [-v]    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A package has a library with unit tests: two that call one function each, one that reads a
file (run every time, or sandboxed where tests are) and one that calls nothing. It is tested
with `RA_JIT_TESTS=affected` until the daemon serves the build. Then a function one test
calls is edited, and the monitor, at the address the daemon wrote to its directory, has to
say: a patch of the edited function, of the `test` build the daemon served; the test that
calls it ran (`Changed`) and passed; the one that calls the other function was `Skipped`;
the one that reads a file ran every time (`Always`, with why) or in the sandbox. Then an edit
that makes a test fail, which the monitor has to show `Failed`, and the observers' totals.
With -v each query and its answer is printed: docs/monitor.md quotes them.

Python 3, stdlib only.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
TIMEOUT = 900
LIB = """pub fn helper(x: u32) -> u32 {
    x + 1
}

pub fn other(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_helper() {
        assert_eq!(helper(1), 2);
    }

    #[test]
    fn uses_other() {
        assert_eq!(other(2), 4);
    }

    #[test]
    fn reads_a_file() {
        assert!(std::fs::read_to_string("data.txt").unwrap().contains("mon"));
    }

    #[test]
    fn plain() {
        assert_eq!(1 + 1, 2);
    }
}
"""
FILES = {
    "Cargo.toml": '[package]\nname = "mon"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
    "src/lib.rs": LIB,
    "data.txt": "mon\n",
}
VERBOSE = "-v" in sys.argv[1:]
# Where tests that reach the world outside their code run sandboxed (Linux, unless
# `RA_JIT_SANDBOX=off`), the one that reads a file runs again only when the file changed.
SANDBOXED = sys.platform == "linux" and os.environ.get("RA_JIT_SANDBOX") not in ("off", "0")


def test(ws, env):
    ran = subprocess.run(["cargo", "ra-jit", "test"], cwd=ws, env=dict(env, RA_JIT_TESTS="affected"),
                         capture_output=True, text=True, timeout=TIMEOUT)
    return ran.returncode, ran.stdout + ran.stderr


def edit(ws, path, old, new):
    text = open(os.path.join(ws, path)).read()
    assert old in text, old
    open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))


def get(url, path):
    with urllib.request.urlopen(url + path, timeout=10) as answer:
        body = answer.read().decode()
    if VERBOSE:
        print(f"GET {urllib.parse.unquote(path)}\n{body}\n", flush=True)
    return json.loads(body)


def query(url, expr):
    return get(url, "/query?expr=" + urllib.parse.quote(expr, safe="")).get("results", [])


def until(what, check, seconds=15):
    """Asks `check` until it is true: the world takes events on a thread of its own."""
    deadline = time.monotonic() + seconds
    while True:
        try:
            if found := check():
                return found
        except OSError:
            pass
        if time.monotonic() > deadline:
            raise AssertionError(f"the monitor never showed {what}")
        time.sleep(0.2)


def named(results):
    return {it["name"]: it for it in results}


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    root = tempfile.mkdtemp(prefix="ra-jit-monitor-")
    # The daemon's sockets are under its home, and a socket's path is short (104 bytes on macOS).
    ws, home = os.path.join(root, "mon"), tempfile.mkdtemp(prefix="rj-mon-", dir="/tmp")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", RA_JIT_HOME=home,
               RUST_BACKTRACE="0")
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RA_JIT_TESTS",
                 "RA_JIT_MONITOR"):
        env.pop(name, None)
    checks = []

    def check(what, verdict):
        checks.append(verdict is True)
        print(f"{what:44} {'ok' if verdict is True else verdict}", flush=True)

    try:
        deadline = time.monotonic() + 300
        while "[ra-jit]" not in (said := test(ws, env))[1]:
            if said[0] != 0:
                raise SystemExit(f"the tests failed before any edit:\n{said[1][-3000:]}")
            if time.monotonic() > deadline:
                raise SystemExit(f"no build said `[ra-jit]`; the daemon's log is in {home}")
            time.sleep(0.5)
        # The daemon's first build of the tests: each is new to it, and runs.
        test(ws, env)
        url = until("where it listens", lambda: open(os.path.join(home, "monitor")).read().strip())
        status = subprocess.run(["cargo", "ra-jit", "status"], cwd=ws, env=env, capture_output=True, text=True)
        check("`status` says where it listens", f"monitor: Flecs REST on {url}" in status.stdout
              or f"SAID: {status.stdout[-500:]}")

        edit(ws, "src/lib.rs", "x + 1", "x + 2 - 1")
        if SANDBOXED:
            edit(ws, "data.txt", "mon", "mon mon")
        code, said = test(ws, env)
        check("the edit's tests passed, served by the daemon", (code == 0 and "[ra-jit]" in said)
              or f"ENDED {code}:\n{said[-2000:]}")

        # The patch of the edit, and the build it was for.
        def patched():
            patches = [it for it in query(url, "blaze.Patch, (blaze.Of, $b)")
                       if it["fields"]["values"][0]["kind"] in ("patch", "object")]
            return max(patches, key=lambda it: it["fields"]["values"][0]["at_us"], default=None)
        patch = until("the edit's patch, of a build", patched)
        values = patch["fields"]["values"][0]
        check("a patch of the edited function", (values["kind"] == "patch" and values["functions"] >= 1)
              or f"PATCH {values}")
        build = get(url, "/entity/" + patch["vars"]["b"].replace(".", "/"))
        check("of the `test` build the daemon served",
              (build["components"]["blaze.Build"]["command"] == "test" and "blaze.ByDaemon" in build["tags"])
              or f"BUILD {build}")

        # The tests: which ran, why, and how they ended.
        changed = until("the tests the edit reached", lambda: named(query(url, "blaze.Test, blaze.Changed")))
        check("the test that calls it ran", "uses_helper" in changed or f"CHANGED {sorted(changed)}")
        passed = named(query(url, "blaze.Test, blaze.Passed"))
        check("and passed", "uses_helper" in passed or f"PASSED {sorted(passed)}")
        skipped = named(query(url, "blaze.Test, blaze.Skipped"))
        check("the others were skipped", set(skipped) == {"uses_other", "plain"} or f"SKIPPED {sorted(skipped)}")
        why_ran = "Sandboxed" if SANDBOXED else "Always"
        reads = named(query(url, f"blaze.Test, blaze.{why_ran}")).get("reads_a_file")
        why = reads and reads["fields"]["values"][0]["why"]
        wanted = "data.txt" if SANDBOXED else "reads or writes files"
        check(f"the one that reads a file ran ({why_ran}), and why", (why and wanted in why)
              or f"{why_ran.upper()}: {reads}")
        target = query(url, "blaze.TestTarget, (blaze.Of, $b)")
        check("the run of the tests is of a build", bool(target) or "NO TARGET WITH A BUILD")

        edit(ws, "src/lib.rs", "other(2), 4", "other(2), 5")
        code, said = test(ws, env)
        check("a test that fails fails", code != 0 or f"ENDED {code}")
        failed = until("the failed test", lambda: named(query(url, "blaze.Test, blaze.Failed")))
        check("the monitor shows it failed", set(failed) == {"uses_other"} or f"FAILED {sorted(failed)}")
        totals = get(url, "/entity/blaze/Totals")["components"]["blaze.Totals"]
        check("the observers counted it", (totals["tests_failed"] == 1 and totals["patches"] >= 2
                                            and totals["by_daemon"] >= 2) or f"TOTALS {totals}")
    finally:
        subprocess.run(["cargo", "ra-jit", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    wrong = checks.count(False)
    print(f"{len(checks) - wrong} of {len(checks)} right; under {root}, the daemon's home {home}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
