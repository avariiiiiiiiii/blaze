# Requests from lane proof-perf

`harness/v2/budgets.py`, `harness/v2/determinism.py` and `harness/v2/tightmemory.py` are written
against the items below.

## 1. `pyproject.toml`: lint and type-check `harness/v2`

Where: the root `pyproject.toml`, ruff's and pyright's include lists.

Why: nothing checks the three scripts today, and they import their siblings by name.

- Add `harness/v2/*.py` to ruff's and pyright's `include`.
- Set pyright `extraPaths = ["harness", "harness/v2", "python/typed-input"]` so that `engine`,
  `report`, `budgets`, `bench`, `memory`, `blaze_cli` and `typed_input` resolve.
- Add a ruff per-file ignore of `E402` for `harness/v2/*`: the imports follow the `sys.path`
  line.

## 2. `harness/v2/fixture.py`: `write_workspace(dir: Path, seed: int)`

Where: lane proof-corpus's `harness/v2/fixture.py`.

Why: determinism.py builds the oracle's fixture twice on different core counts. It calls
`write_workspace(dir, seed)` with an existing, empty directory, expects a cargo workspace there
that `blaze build` builds from that directory, and ignores what it returns. Until it exists,
determinism's fixture row is an `error` that says so.

## 3. `harness/v2/proof.py`: run and read the three

Where: lane proof's `harness/v2/proof.py`.

Why: each script writes a `report.Report` JSON to `out`. All three need Linux.

- `budgets.py {engine, out, work}`.
- `determinism.py {engine, out, work}`.
- `tightmemory.py {engine, out, work}`, plus `cgroup` (a writable cgroup v2 directory with
  `memory` in its `cgroup.subtree_control`) where `systemd-run --user` has no memory controller
  delegated.

## 4. `harness/v2/engine.py`: the V2 adapter's measuring calls

Where: `harness/v2/engine.py`, `V2`.

Why: `wait_ready`, `stop`, `mark`/`work_since` and `critical_path_s` raise
`NotImplementedError("v2: ...")`. Every row that needs one is an `error` on v2 until it is there.

## 5. Finding for core: what `BLAZE_MEMORY_BUDGET` takes off

Where: `crates/blaze-cargo/src/memory.rs`, and docs/v2/core.md section 4.

Why: the budget keeps back max(5%, 512 MB) and subtracts "others", the whole machine's
`MemTotal - MemAvailable`. Under a small cgroup that can use up the whole budget, and blaze then
refuses where cargo's peak fits. tightmemory.py reports that as a `fail` at 1.0x. "Others" should
be what is in use in the same cgroup.
