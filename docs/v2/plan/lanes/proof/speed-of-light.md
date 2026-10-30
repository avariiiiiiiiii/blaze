---
status: plan
summary: lane proof's speed of light: the bound of the oracle's comparisons and the harness, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane proof: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). The machine figures are those of
[link's speed of light](../link/speed-of-light.md#the-machine), unless a section says otherwise.
`proof` is also where every other lane's bounds are measured: its budgets are each bound times
1.1.

## The oracle and the harness: `crates/blaze-v2-proof/`, `harness/v2/` (speed-link-edge-sol-proof)

The machine is link's: 16 cores, 25 GB/s from DRAM a core and 400 GB/s a socket, BLAKE3 at
6 GB/s a core, SHA-256 at 2 GB/s. One correction to link's table: a read fault on a file in the
page cache maps 16 pages at once (Linux's fault-around, 64 KiB), so a mapped file costs about
0.25 µs per 64 KiB, not per 4 KiB page. The figures below use that. Every "now" is arithmetic on
the code as written; nothing here was run (this lane's brief ran nothing). Proof's budgets are what
turn these figures into measurements.

### First difference (`crates/blaze-v2-proof/src/bytes.rs`)

**Workload.** Two outputs that should be the same, compared byte for byte: a 512 MiB debug binary
pair (nushell's size) that differs, at worst, in its last byte. Most determinism checks compare
many small files, and those are bounded by the open and map calls (0.4 µs + 0.25 µs a file), not by
bytes.

**Bound.** 2 × 512 MiB = 1.07 GB read. On one core that is 1.07 GB ÷ 25 GB/s = 43 ms, plus faults
of 16,384 × 0.25 µs = 4 ms, so 47 ms. Across 16 cores it is 1.07 GB ÷ 400 GB/s = 2.7 ms, plus
the faults spread over 16 threads (0.25 ms), so 2.9 ms. For many files at once the bound is the
same 2 × Σ bytes ÷ 400 GB/s.

**Was.** One thread with an AVX2 or NEON loop (about 24 bytes a cycle a side, so memory-bound),
over maps made with `MAP_POPULATE`. Populating faults in both files on the calling thread before
the compare starts. The whole run is 60 to 70 ms on one core, 1.4× the one-core bound and 22× the
16-core bound.

**Changed.** `first_difference_across_cores` splits a pair of at least 64 MiB into 4 MiB blocks
over the cores. A shared atomic "earliest piece with a difference" (`fetch_min`) lets later
pieces stop early. The earliest such piece never stops, so the answer is the same as the serial
scan's. Files of that size are mapped lazily (`Mapped::open_lazily`, no populate), so each thread
faults in its own blocks. Tests compare the parallel answer with the scalar one for differences
in every block and for both length orders.

**Now.** About 3.3 to 3.6 ms on 16 cores: 2.7 ms streaming, 0.25 ms faults, and 16 thread spawns
at about 15 µs each. That is within 25% of the bound. The rest is thread start-up, which a
persistent pool would remove. One path is still slow: `harness/v2/engine.py`'s
`first_difference` (Python `readinto` in 1 MiB pieces, then `memcmp`) runs at about 150 ms, 3× the
one-core bound. It runs only after a check has already failed, to name the first differing byte.
The fix is for `determinism.py` to call `blaze-v2-proof '{"first": …, "second": …}'`, which is
this code.

**State of the art.** glibc's `memcmp` (AVX2/EVEX) and `cmp(1)` are one-core scans, about 43 ms
here. The same loop across cores reaches the socket's bandwidth instead of one core's.

```bound
{"name": "proof: first difference, 512 MiB pair", "bound": 2.9, "unit": "ms", "program": "blaze-v2-proof", "args": ["{\"bench\": \"first-difference\"}"]}
```

### Hashing artifacts (`harness/v2/engine.py`)

**Workload.** The digests `engine.digests` and `tree_digest` take of a build's outputs and trees.
For nushell's debug outputs that is about 3 GB in about 1,000 files, the largest about 500 MB.
For a source tree it is about 30,000 files and 300 MB.

**Bound.** Each byte read once, hashed at full width: 3 GB ÷ (6 GB/s × 16) = 31 ms for the
outputs. For the tree it is 300 MB ÷ 96 GB/s = 3 ms, plus 30,000 opens at 0.4 µs ÷ 16 = 1 ms, so
4 ms (6 ms with the stat of each entry). SHA-256 at 2 GB/s a core is 3× slower at the same width
(94 ms). On one thread per file it is worse still: the critical path is the largest file alone,
500 MB ÷ 2 GB/s = 250 ms, 8× the bound.

**Was.** SHA-256 (`hashlib`), one file per pool task, one core per file. The critical path was
the largest file at about 250 ms, 8× off.

**Changed.**
- SHA-256 is gone from every digest that blaze or the harness picks. Digests use BLAKE3 (the
  `blake3` Python package, Rust's `blake3` crate underneath).
- A file of at least 8 MiB is hashed across cores (`max_threads=AUTO`), so the 500 MB file no
  longer sets the critical path.
- Files under 1 MiB are read with one `read()`; larger files are mapped.
- Small files go to the pool in batches (at most 256 files or 4 MiB a task), so pool overhead is
  paid per batch, not per file.
- Files are queued largest first.
- `tree_digest`'s domain moved to `blaze v2 proof tree 2`, so no old SHA-256 tree digest is
  compared with a new one.
- What stays SHA-256 is what something outside chose: the crates.io check in `corpus.verified`
  (the index lists SHA-256), and `Kept.key` (bytes, not files). The package is a request in
  `v2/requests/speed.md`: add `blake3` to `pyproject.toml` and `uv.lock`.

**Now.** For outputs, about 35 ms: 31 ms hashing, with per-file Python overhead (about 8 µs a file
holding the GIL, 8 ms in all) overlapping it. That is within 15% of the bound. For trees, Python's
`os.walk` and `lstat` cost about 20 µs a file, 0.6 s for 30,000 files, 100× the bound. That is
still under 0.1% of a corpus run, but it is the one component left far off. The fix is a walker
in `blaze-v2-proof` (`getdents64` plus `statx` across cores, BLAKE3 in place), called the way the
first difference is.

**State of the art.** `b3sum` maps each file and hashes it across cores with BLAKE3's tree mode.
That is the same work done here, at about 6 GB/s a core. `sha256sum` is one core at 2 GB/s.

### Trace join (`crates/blaze-v2-proof/src/join.rs`, `wire.rs`)

**Workload.** The oracle's `under` (what an incremental build kept that a build from scratch
says is stale) and `over` (what it reran for nothing). These are taken over traces of nushell's
size: n = 5 M keys and m = 50 M edges. A trace is about 1.27 GB: keys 34 B, a fingerprint 16 B,
an edge 4 B for its read plus 16 B for what it saw, and the two bitsets.

**Bound.**
- Reading both traces: 2 × 1.27 GB ÷ 400 GB/s = 6.4 ms. Only the columns the join touches are
  read; with the edges read once, that comes to about 3.75 ms.
- Probes: each of the 50 M edges of a kept key checks the fingerprint of the key it reads, a
  random access into an 80 MB array (a DRAM miss). With 10 to 20 misses in flight a core, a probe
  costs about 8 ns on one core: 50 M × 8 ns ÷ 16 = 25 ms.
- Checking the trace's invariants in place: about 1 ms.
- Total: about 30 ms on 16 cores, about 450 ms on one.

**Was.** Each trace was decoded from the derived serde CBOR, which writes a `[u8; 32]` as 32
items: 35 items a key and 17 an edge, roughly 1.2 G items a trace. That is about 1.5 s a trace,
three traces a case. Then `under` ran on one core (about 450 ms). In all about 5 s, 150× the bound.

**Changed.**
- `wire.rs` is version 2 of the trace file: a CBOR map of the version plus one byte string per
  column, fixed width and little-endian.
- `View::parse` borrows the columns in place (`as_chunks`) after a parallel pass that checks
  them. The pass checks that keys are strictly ascending, that `edges_from` runs from 0 to m
  without going down, and that every read is below n. A bad file is refused with the place named,
  never patched up.
- `join.rs` takes both traces through the `Columns` trait, implemented by `Trace` and by `View`.
  It splits the keys into ranges, one or more a core and at least 16,384 keys each. Each range
  seeks its start in the other trace (a gallop, then a binary search).
- The pieces' results are concatenated in key order and sorted exactly as the serial `crate::under`
  sorts. Tests check that `under` and `over` equal the serial ones for 1 to 7 pieces, on `Trace`
  and `View`, with and without roots.
- `main.rs` maps the traces lazily and joins the views. It builds owned traces only for a nameres
  case.
- Two lines of `lib.rs` (`pub mod join; pub mod wire;`, and `Trace` dropping its derived serde
  for `wire.rs`'s) are a request in `v2/requests/proof.md`, because `lib.rs` is shared. Until that
  lands, `main.rs` does not build.

**Now.** About 35 ms on 16 cores once the request lands: 25 ms of probes, 4 to 6 ms of streaming,
1 ms of checking, and thread start-up. That is within 15% of the bound.

**State of the art.** rustc reads its dep graph by decoding every node and edge (LEB128) at
start-up, at a few hundred MB/s. salsa keeps none on disk. Columnar formats read in place
(Arrow IPC, FlatBuffers) cost no decode, which is what `wire.rs` does. Splitting the join into key
ranges, one per core, is morsel-driven parallelism (HyPer, DuckDB) applied to a merge join.

```bound
{"name": "proof: trace join, 5 M keys and 50 M edges", "bound": 30.0, "unit": "ms", "program": "blaze-v2-proof", "args": ["{\"bench\": \"join\"}"]}
```

Both blocks run `blaze-v2-proof`'s bench mode (`src/bench.rs`). The bench makes the workload in
memory, times only the work the bound counts, and prints `{"value": MS}`. The join bench joins a
trace with itself: every key is matched and every kept edge is probed, which is the worst case.
The bounds are for the 16-core machine; a VM with fewer cores reports against the same figure, so
run them on one of that size.

### Corpus run (`harness/v2/corpus.py`)

**Workload.** Every project built and tested twice, by blaze and by rustc. The projects are 52
crates.io crates plus pinned blaze, nushell, ripgrep and helix.

**Bound.** The critical path: max(Σ work ÷ 16 cores, the longest single project's own critical
path). The rustc side of a project costs nothing on a rerun, because its kept result
(`engine.Kept`, keyed by toolchain, source and steps) is reused, so after the first run the bound
is blaze's side. The harness's own work (unpacking, hashing, comparing, writing the report) must be
under 1% of that.

**Was.**
- Two projects ran at once (`jobs`, default 2), each with the blaze side and then the cargo side,
  one after the other.
- Projects started in input order, so a big workspace listed late (blaze, which builds for
  minutes) could run alone at the end while the other slot sat empty.
- The harness's overhead was not measured.

**Changed.**
- Projects start largest first (`largest_first`): the four workspaces before the crates.io
  crates, each group in input order. This is Graham's LPT rule, whose makespan is at most 4/3 of
  the optimum and, here, removes the lone tail. The report keeps the input's order.
- The report now carries `harness_cpu_s`, `children_cpu_s`, `harness_share` and `wall_s`, read from
  `os.times()` around the pool. The share is the harness's own CPU over its children's, the
  quantity the 1% bar is about.
- Hashing in the comparisons is the BLAKE3 path above.

**Now.** The estimated overhead is 0.2 to 1%, now measured on every run by `harness_share`. The
gap left is idle cores. With two projects at once, a small crate's build of a few seconds does not
fill 16 cores, and the blaze and cargo sides of one step do not overlap. The fix is to choose
`jobs` from the measured busy share of the cores (as `budgets.py` measures for `cold-blaze`), or
to run a step's two sides at once. Either is a change to what a run measures, so it waits for a
measured `harness_share` and busy share to say which.

**State of the art.** crater builds each crate in its own container, on both toolchains on every
run, and keeps no reference result. Here the reference side is kept, so a rerun costs one side.

### Budgets: how many runs each needs (`harness/v2/budgets.py`)

**Workload.** Each timed budget's samples, judged by the median against the limit.

**Bound.** The runs needed for a 95% interval of the median narrower than 10% of the bound. For
timings with coefficient of variation cv, the median's standard error is √(π/2) · cv/√n. An interval
2 × 1.96 × that wide is under 10% when n ≥ (2 × 1.96 × √(π/2) × cv ÷ 0.1)² ≈ (49 cv)². The
order-statistic interval below needs at least 6 runs.

| budget | cv (expected) | runs |
|---|---|---|
| `noop` (ripgrep, a few ms) | 5 to 10% | 7 to 25 |
| `body-edit`, `big-crate` | 3 to 5% | 6 to 7 |
| `warm-start` | about 5% | 7 |
| `restart` | about 10% | 25 |
| `ci`, `cold-blaze` (minutes) | about 2% | 6 |
| `dev-speed`, `release-speed` (each side; the ratio's error is both) | 1 to 2% | 6 a side |
| `bounds` (the benches above, and other lanes' programs) | 1 to 3% | 6 |
| memory, layout and count budgets | none (exact) | 1 |

**Was.** A fixed count per budget (`runs`, default 10), judged by the median with no interval.
Ten runs were too few for `restart` and `noop` at their noise, and more than needed for most of
the rest.

**Changed.** Every timed budget samples until its interval settles. The interval is a
distribution-free 95% interval of the median from binomial order statistics, so it assumes
nothing about the timing's tails. A budget stops once that interval is at most 10% of the limit
wide, or lies wholly on one side of the limit (`enough`), and never above `most_runs` (default
60). `runs` stays the least. `dev-speed` and `release-speed` run until both sides' medians are
known to 5%. Each outcome reports `runs` and `runs_needed`, which is `runs_for(cv)` from its own
samples, so the table above gets replaced by measured figures run by run. Checking the interval
after every run lowers its real coverage somewhat below 95%. The 60-run cap and the 10% width,
far tighter than the 10% slack the budgets allow, keep that from deciding a verdict.

**State of the art.** hyperfine runs a fixed count (at least 10, or 3 s), reports the mean and
standard deviation, and flags outliers. criterion.rs takes 100 samples and a bootstrap interval.
Neither stops when the answer is known, nor sizes the run to the question.

### Bounds as budgets

Every lane's `speed-of-light.md` says what proof measures. For each component a program can
measure, the lane also writes, beside that component's arithmetic, a fenced block with the info
string `bound` holding one JSON object, as the two blocks above do. Its fields:

- `name`: unique across lanes.
- `bound`: a number, in `unit`.
- `unit`: what the program's value is in.
- `program`: a program the build delivers beside `blaze`.
- `args`: the program's arguments.
- `rate`: true when more is better, such as a throughput.

`harness/v2/bounds.py` reads every such block in `docs/v2/plan/lanes/*/speed-of-light.md` into a
typed `Bound`. It refuses a block that is not exactly one such object, a bound not above 0, and
two bounds of one name, each refusal naming the file and line. `budgets.py`'s budget `bounds`
makes one outcome a bound, `bound-<name>`. Its limit is bound × 1.1, or bound ÷ 1.1 for a rate.
The outcome runs the program until its interval settles, as above, and takes `value` from the
JSON object on the program's last line of output.

So a lane adds a budget by writing one block and shipping one program that does the reference
workload once. No harness change is needed. A component with no such program yet stays prose:
the outcome is missing, so proof has nothing to measure for it, and nothing for it is reported as
passing.
