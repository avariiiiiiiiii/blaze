"""appstests.py: the tests stage of apps.py.

`tests` of an application has two kinds of test:

program  arguments of `cargo test` that pick test binaries which start the program
         (`["--test", "integration"]`). Cargo builds them; each is run with blaze's program
         in the place of cargo's.
crates   directories of workspace crates whose unit and integration tests are linked again
         with blaze's objects for the crate, by cratetest.py, and judged by their own results.

Each test is judged by its own result: every one that runs passes (`ok`, or `ignored`), and the
ones that do not are listed. A test known to fail is left out by name (`tests.skip`).
"""
import json, os, re, shutil, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))


def results(binary, cwd, env, skip):
    """What each test of a test binary came to."""
    argv = [binary, *(x for name in skip for x in ("--skip", name))]
    try:
        ran = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, errors="replace",
                             stdin=subprocess.DEVNULL, timeout=3600)
    except subprocess.TimeoutExpired:
        return {"(the binary)": "timeout"}
    found = dict(re.findall(r"^test (.*?) \.\.\. (\w+)", ran.stdout, re.M))
    return found or {"(the binary)": f"status {ran.returncode}"}


def program_tests(fx, ws, spec, args, skip):
    """Builds the test binaries with cargo, puts blaze's program where they start it, and runs
    them; returns (tests, failing names). cargo's program is put back after."""
    target = os.path.join(ws, "target-tests")
    env = {**os.environ, "CARGO_TARGET_DIR": target, "RUSTFLAGS": "--cap-lints allow"}
    built = subprocess.run(["cargo", "test", "-p", spec["package"], *args, "--no-run", "--message-format=json"],
                           cwd=ws, env=env, capture_output=True, text=True)
    if built.returncode != 0:
        raise SystemExit(f"cargo test --no-run failed:\n{built.stderr[-2000:]}")
    name = spec.get("bin", spec["package"])
    tests, program = [], os.path.join(target, "debug", name)
    for line in built.stdout.splitlines():
        unit = json.loads(line) if line.startswith("{") else {}
        if unit.get("profile", {}).get("test") and unit.get("executable"):
            tests.append((unit["executable"], os.path.dirname(unit["manifest_path"])))
    kept = program + ".cargo"
    shutil.copy2(program, kept)
    try:
        shutil.copy2(fx.binary, program)
        run_env = {**fx.run_env, "CARGO": shutil.which("cargo")}
        seen = {f"{os.path.basename(exe).rsplit('-', 1)[0]}::{test}": result
                for exe, cwd in tests
                for test, result in results(exe, cwd, {**run_env, "CARGO_MANIFEST_DIR": cwd}, skip).items()}
    finally:
        shutil.move(kept, program)
    failing = sorted(test for test, result in seen.items() if result not in ("ok", "ignored"))
    for test in failing[:40]:
        print(f"  FAILED {test}: {seen[test]}")
    return len(seen), failing


def crate_tests(ws, directory):
    """cratetest.py on a crate of the workspace; returns (tests, failing names, error)."""
    out = os.path.join(ws, ".apps-cratetest.json")
    if os.path.exists(out):
        os.remove(out)
    ran = subprocess.run([sys.executable, os.path.join(HERE, "cratetest.py"), os.path.join(ws, directory), "--json", out],
                         capture_output=True, text=True)
    last = (ran.stdout.strip().splitlines() or [ran.stderr.strip()[-300:]])[-1]
    print(f"  {directory}: {last}")
    if not os.path.exists(out):
        return 0, [], last[:300]
    merged = json.load(open(out))
    failing = [f"{directory}::{test}" for binary in merged["binaries"] for test in binary["failed"]]
    broken = [f"{directory}: {binary['error'] if 'error' in binary else binary['wrong']}" for binary in merged["binaries"]
              if "error" in binary or (binary.get("wrong") and not binary["failed"])]
    return merged["tests_run"], failing + broken, None


def run(fx, ws, spec, result):
    tests = spec.get("tests")
    if not tests:
        return True
    row = {"run": 0, "failing": [], "errors": []}
    if "program" in tests:
        try:
            count, failing = program_tests(fx, ws, spec, tests["program"], tests.get("skip", []))
            row["run"] += count
            row["failing"] += failing
        except SystemExit as failed:
            row["errors"].append(str(failed)[-600:])
    for directory in tests.get("crates", []):
        count, failing, error = crate_tests(ws, directory)
        row["run"] += count
        row["failing"] += failing
        if error:
            row["errors"].append(f"{directory}: {error}")
    result["tests"] = row
    print(f"tests: {row['run']} run, {len(row['failing'])} failing, {len(row['errors'])} test targets not run")
    for error in row["errors"]:
        print(f"  NOT RUN {error[:300]}")
    return not row["failing"] and not row["errors"]
