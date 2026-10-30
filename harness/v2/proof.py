"""proof.py: everything that proves blaze v2 (docs/v2/plan/lanes/proof.md), run as one, into one report.

    uv run --frozen python harness/v2/proof.py '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}, "out": "/tmp/proof"}'
    uv run --frozen python harness/v2/proof.py '{"engine": {...}, "out": "/tmp/proof",
        "checks": [{"name": "oracle", "given": {"rounds": 8}}, {"name": "budgets"}]}'
    uv run --frozen python harness/v2/proof.py selftest

The checks, each a script of this directory run with the same `engine` (v1 now, v2 later):

    oracle        oracle.py: the incremental build equals one from scratch after every edit
    corpus        corpus.py: real crates built, run and tested under blaze and rustc
    rejections    rejections.py: rustc's compile-fail tests fail with the same error codes
    determinism   determinism.py: two builds on different core counts, byte for byte the same
    tightmemory   tightmemory.py: a build under a tight memory budget finishes, never OOMs
    budgets       budgets.py: the targets of docs/v2/plan/goals/targets.md

Each writes its result (report.py's shape) to `OUT/<check>.json` and what it printed to
`OUT/<check>.log`; this writes `OUT/report.json` (one outcome per check, the verdict of the
worst of its outcomes) and `OUT/report.md`: a line per check, the budgets as a table, and
every outcome that did not pass. A check that wrote no result is an error naming its exit
status and the end of its log. Exits 1 when any check failed or erred, or the engine is v2
and anything was unmeasured.

Input (JSON inline, a file's path, or `-`):
    engine        the blaze under test (engine.EngineSpec), given to every check
    out           the directory of the results
    work          a directory for the checks' work (`WORK/<check>`); each makes its own when empty
    checks        which to run, in order (default: all, as above); each a `name`, `given` (the
                  rest of that script's input, a JSON object: its docstring says the fields)
                  and `hours`, how long it may take before it is stopped
"""

import json
import subprocess
import sys
import time
import typing
from collections import Counter
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]

import engine  # noqa: E402
import report  # noqa: E402
from typed_input import fail, parse  # noqa: E402

SCRIPT = "proof.py"
type Name = Literal["oracle", "corpus", "rejections", "determinism", "tightmemory", "budgets"]
SCRIPTS: dict[str, tuple[str, ...]] = {
    "oracle": ("oracle.py", "run"),
    "corpus": ("corpus.py",),
    "rejections": ("rejections.py",),
    "determinism": ("determinism.py",),
    "tightmemory": ("tightmemory.py",),
    "budgets": ("budgets.py",),
}
WORST: tuple[report.Verdict, ...] = ("fail", "error", "unmeasured", "pass")


@dataclass(frozen=True)
class Check:
    name: Name
    given: Any = None
    hours: float = 12.0


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    checks: tuple[Check, ...] = tuple(Check(name) for name in SCRIPTS)


def argv_of(check: Check, spec: engine.EngineSpec, out: Path, work: str) -> list[str]:
    if not isinstance(check.given, dict | None):
        fail(SCRIPT, f"checks: {check.name}: `given` is a JSON object, not {json.dumps(check.given)}")
    given: dict[str, object] = dict(typing.cast(dict[str, object], check.given or {}))
    for field in ("engine", "out", "work"):
        if field in given:
            fail(SCRIPT, f"checks: {check.name}: `{field}` is proof.py's to give, not `given`'s")
    given.update(engine=asdict(spec), out=str(out / f"{check.name}.json"))
    if work:
        given["work"] = str(Path(work) / check.name)
    script, *sub = SCRIPTS[check.name]
    return [sys.executable, str(HERE / script), *sub, json.dumps(given)]


def verdict_of(outcomes: list[dict[str, Any]]) -> report.Verdict:
    seen = {o["verdict"] for o in outcomes}
    return next((v for v in WORST if v in seen), "pass")


def run_check(check: Check, spec: engine.EngineSpec, out: Path, work: str) -> tuple[report.Verdict, str, dict[str, Any] | None]:
    argv = argv_of(check, spec, out, work)
    log = out / f"{check.name}.log"
    result_path = out / f"{check.name}.json"
    result_path.unlink(missing_ok=True)
    print(f"proof: {check.name}: {' '.join(argv[1:3])} (log {log})", flush=True)
    began = time.monotonic()
    with open(log, "w") as to:
        try:
            done = subprocess.run(argv, cwd=engine.ROOT, stdout=to, stderr=subprocess.STDOUT,
                                  stdin=subprocess.DEVNULL, timeout=check.hours * 3600)
            status = f"exit {done.returncode}"
        except subprocess.TimeoutExpired:
            status = f"stopped after {check.hours} h"
    took = time.monotonic() - began
    if not result_path.is_file():
        tail = log.read_text(errors="replace")[-3000:]
        return "error", f"{SCRIPTS[check.name][0]} wrote no result ({status}, {took:.0f} s):\n{tail}", None
    result: dict[str, Any] = json.loads(result_path.read_text())
    outcomes: list[dict[str, Any]] = result["outcomes"]
    counts = Counter(o["verdict"] for o in outcomes)
    verdict = verdict_of(outcomes) if outcomes else "error"
    if status.startswith("stopped"):
        verdict = "error"
    shown = ", ".join(f"{counts[v]} {v}" for v in WORST if counts[v])
    return verdict, f"{shown or 'no outcomes'} ({status}, {took:.0f} s)", result


def cell(value: object) -> str:
    return "" if value is None else str(value).replace("|", "\\|").replace("\n", " ")


def markdown(spec: engine.EngineSpec, rows: list[tuple[Check, report.Verdict, str, dict[str, Any] | None]]) -> str:
    lines = [
        f"# blaze v2 proof: {spec.kind}, {spec.blaze} ({spec.backend})",
        "",
        f"commit {report.commit()}, {report.now()}",
        "",
        "| check | verdict | outcomes |",
        "|---|---|---|",
    ]
    lines += [f"| {check.name} | {verdict} | {cell(detail.splitlines()[0])} |" for check, verdict, detail, _ in rows]
    budgets = next((result for check, _, _, result in rows if check.name == "budgets" and result), None)
    if budgets is not None:
        lines += ["", "## Budgets (docs/v2/plan/goals/targets.md)", "", "| budget | verdict | numbers | detail |",
                  "|---|---|---|---|"]
        for o in budgets["outcomes"]:
            numbers = "  ".join(f"{k}={v}" for k, v in o["numbers"].items() if v is not None)
            first = o["detail"].splitlines()[0] if o["detail"] else ""
            lines.append(f"| {cell(o['name'])} | {o['verdict']} | {cell(numbers)} | {cell(first)} |")
    for check, verdict, detail, result in rows:
        bad = [o for o in (result or {}).get("outcomes", []) if o["verdict"] != "pass"]
        if result is None:
            lines += ["", f"## {check.name}: {verdict}", "", "```", detail, "```"]
        elif bad:
            lines += ["", f"## {check.name}: what did not pass", ""]
            for o in bad[:200]:
                first = o["detail"].splitlines()[0] if o["detail"] else ""
                lines.append(f"- `{o['name']}` {o['verdict']}: {first[:300]}")
            if len(bad) > 200:
                lines.append(f"- and {len(bad) - 200} more, in {check.name}.json")
    return "\n".join(lines) + "\n"


def run(it: Input) -> None:
    if not it.checks:
        fail(SCRIPT, "checks: name at least one")
    names = [c.name for c in it.checks]
    if len(set(names)) != len(names):
        fail(SCRIPT, f"checks: each at most once, not {names}")
    out = Path(it.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    whole = report.Report(SCRIPT, it.engine, str(out / "report.json"))
    rows: list[tuple[Check, report.Verdict, str, dict[str, Any] | None]] = []
    for check in it.checks:
        verdict, detail, result = run_check(check, it.engine, out, it.work)
        rows.append((check, verdict, detail, result))
        counts: dict[str, report.Number] = dict((result or {}).get("counts", {}))
        whole.add(check.name, verdict, detail, **counts)
    whole.extra.update(results={c.name: str(out / f"{c.name}.json") for c, *_ in rows})
    (out / "report.md").write_text(markdown(it.engine, rows))
    print(f"proof: {out / 'report.md'}", flush=True)
    whole.finish()


def selftest() -> list[str]:
    wrong: list[str] = []
    if [verdict_of([{"verdict": v} for v in vs]) for vs in (["pass"], ["pass", "unmeasured"], ["error", "fail"], [])] != [
        "pass", "unmeasured", "fail", "pass"]:
        wrong.append("verdict_of does not give the worst verdict")
    spec = engine.EngineSpec("v1", "/b/blaze")
    argv = argv_of(Check("oracle", {"rounds": 2}), spec, Path("/o"), "/w")
    given = json.loads(argv[-1])
    if argv[1:3] != [str(HERE / "oracle.py"), "run"]:
        wrong.append(f"argv_of: {argv}")
    if given != {"rounds": 2, "engine": asdict(spec), "out": "/o/oracle.json", "work": "/w/oracle"}:
        wrong.append(f"argv_of gives {given}")
    for name in SCRIPTS:
        if name not in typing.get_args(Name.__value__):
            wrong.append(f"{name} is a script but not a Name")
        if not (HERE / SCRIPTS[name][0]).is_file():
            wrong.append(f"{SCRIPTS[name][0]} is not in {HERE}")
    md = markdown(spec, [(Check("budgets"), "fail", "1 fail (exit 1, 3 s)", {"outcomes": [
        {"name": "nushell body", "verdict": "fail", "detail": "took 80 ms", "numbers": {"ms": 80, "budget": 30}}]})])
    if "| nushell body | fail | ms=80  budget=30 | took 80 ms |" not in md:
        wrong.append(f"markdown has no row of the budget:\n{md}")
    return wrong + v2_work_wrong()


def v2_work_wrong() -> list[str]:
    """`engine.V2` reading what requests did from the `blaze-work` lines of canned output, the
    form `blaze_ui::api::work_json` prints."""
    line = ('{"reason": "blaze-work", "reran": %d, "green": 300, "compiled": %d, "found": 40, '
            '"link": "%s", "critical_path_us": %d}')

    class Canned(engine.V2):
        def __init__(self, said: list[str]):
            super().__init__(engine.EngineSpec("v2", sys.executable))
            self.said = said

        def command(self, step: str, command: str, args: list[str], ws: Path, env: dict[str, str],
                    timeout: float = 7200) -> engine.Ran:
            return engine.Ran((command, *args), 0.1, 0, self.said.pop(0), "")

    wrong: list[str] = []
    home, ws = Path("/h"), Path("/w")
    env = {"BLAZE_HOME": str(home)}
    json_ = ["--message-format=json"]
    eng = Canned(['{"reason": "compiler-artifact"}\n' + line % (12, 2, "patched", 1500) + "\n",
                  line % (3, 1, "placed", 2_500_000) + "\n", "test a ... ok\n" + line % (1, 0, "patched", 9) + "\n"])
    eng.build("cold", ws, env, json_)
    mark = eng.mark(home)
    eng.build("edit", ws, env, json_)
    eng.build("tests", ws, env, json_, command="test")
    work = eng.work_since(home, mark)
    if (work.compiled, work.reran, work.found, work.green) != (1, 4, 80, 600):
        wrong.append(f"V2.work_since sums {work}")
    if work.link != "placed whole: edit (`blaze build --message-format=json`)":
        wrong.append(f"V2.work_since: link {work.link!r}")
    if eng.critical_path_s(ws, env) != 9e-6:
        wrong.append(f"V2.critical_path_s: {eng.critical_path_s(ws, env)}")
    for args, said in ((json_, "test a ... ok\n"), ([], line % (1, 1, "patched", 1))):
        mark = eng.mark(home)
        eng.said.append(said)
        eng.build("unsaid", ws, env, args, command="test")
        try:
            eng.work_since(home, mark)
            wrong.append(f"V2.work_since counted a request with args {args} that said {said!r}")
        except engine.Failed as e:
            if "unsaid (`blaze test" not in str(e) or "no `blaze-work` line" not in str(e):
                wrong.append(f"V2.work_since: {e}")
        try:
            eng.critical_path_s(ws, env)
            wrong.append("V2.critical_path_s read a request that printed no `blaze-work` line")
        except engine.Failed:
            pass
    bad = [line % (1, 1, "spliced", 1), line.replace('"found": 40', '"found": -1') % (1, 1, "patched", 1)]
    for said in bad:
        try:
            engine.required("bad", engine.worked_lines(said)[0])
            wrong.append(f"engine.required took {said}")
        except engine.Failed:
            pass
    if len(engine.worked_lines(line % (1, 1, "patched", 1) + "\n{not json\n")) != 1:
        wrong.append("engine.worked_lines does not skip a line that is not JSON")
    return wrong


def main() -> None:
    match sys.argv[1:]:
        case ["selftest"] | ["selftest", _]:
            wrong = selftest()
            for line in wrong:
                print(line)
            print(f"{SCRIPT} selftest: {len(wrong)} wrong")
            sys.exit(1 if wrong else 0)
        case [text]:
            run(parse(Input, text, SCRIPT))
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} JSON, or selftest (see the top of the file)")


if __name__ == "__main__":
    main()
