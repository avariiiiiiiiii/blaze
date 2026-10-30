---
status: plan
summary: lane safety's speed of light: the bound each hir-ty-ub component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane safety: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md).
Each component of `hir-ty-ub` has a section here, written by the agent that owns it. Cycle
counts are for one core of the target machines (Graviton 5, EPYC Turin): about 3.5 GHz, an
L1 hit in 4 to 5 cycles, L2 in about 14, and branchy pointer code at about 2 instructions per
cycle. Figures in ns use 3.5 GHz.

## The checker: `check.rs`, `check/` (safety-check)

The checker does no work of its own beyond its hooks: it is called by the evaluator once per
event and hands each event to memory, Tree Borrows and the race detector. So its bound is
per hook, and its own part of each hook should be a few instructions on lines already in L1.
The whole run is then bound by the evaluator's events times these costs. The benchmark is Miri
on the same programs, which does the same checks; `proof` compares the two once things
compile.

### `step`, once per MIR statement and terminator

Work: turn the evaluator's `Site` (body and MIR span) into the dense `SiteId` that the race
detector and Tree Borrows store with each access.

- Bound: compare the body with the current one, pick the span kind's column, check the
  index against the column's length, and load the id. That is about 16 instructions on two
  lines in L1: 8 cycles, 2.3 ns.
- Design: `Sites` keeps one table per body, found through an `FxHashMap` only when the body
  changes (a call or return), and one dense `Vec<SiteId>` per span kind indexed by the span's
  arena index. A statement is interned with no hashing. The first version hashed every
  statement's site, which costs about 30 to 40 cycles from hashing a 16- to 24-byte key,
  probing and comparing it.
- What stands in the way of the last factor: the evaluator passes the site by value. If
  `Site` carried a body-local statement index, `step` would be one indexed load, about 3
  cycles. That is a change to the `Machine` contract (codegen-mir) and is worth making only
  if `proof` measures `step` above 10% of a statement's total cost.
- Tree Borrows' garbage collection runs from `step` every 10,000 retags (Miri's default).
  It visits every live tag once, so it costs a few cycles per tag per 10,000 retags,
  amortized.

### An access (`read`, `write`, `copy`)

Work: memory's bounds, liveness and alignment check; then the race check; then Tree Borrows'
permission check.

- Memory: one allocation record (base, size, alignment, live) by `AllocId` index, one line
  and about 12 instructions.
- Race detector, one thread: `race.rs` skips everything until a second thread is spawned
  (Miri's `multi_threaded`), about 2 instructions. Several threads: one epoch compare per
  location in the common case, about 10 instructions and a line.
- Tree Borrows: the tag's node and the location's permission, one or two lines, about 20
  instructions when the access changes nothing (Miri's fast path).
- The checker's glue: about 10 instructions, no allocation, no hashing.
- Bound: about 45 instructions on 4 lines in L1 or L2, about 25 cycles (7 ns) per access,
  plus about 5 cycles with several threads.
- Design: `place`, `plain_access` and `atomic_access` call each part once, in Miri's order,
  and return at the first error. The error path is `#[cold]`. The read's shadow is a
  `memory::Bytes`, which holds values of up to 64 bytes with one provenance run inline, so
  a scalar or pointer read allocates nothing.
- What stands in the way: a read of more than 64 bytes builds a heap `Bytes` (memory's
  representation and the `Machine` trait's owned `Shadow`). A borrowed view of memory's
  shadow would remove that allocation. It is proportional to the bytes moved, so it matters
  only for large copies, and `copy` (the common way large values move) already works on
  memory without building a shadow.

### `typed`, once per typed copy (validity and retags)

Work: check that the N bytes copied are a valid value of their type, retag each of the P
references in them, and reset padding and provenance as Miri's `validity.rs` does.

- Bound: one lookup of the type's plan, then about 5 cycles per plan op (K ops), one cycle
  per 64 bytes for the init check, and one Tree Borrows retag per reference (about 50
  cycles plus the pointee's locations). That is about 10 + 5K + N/64 + 50P cycles.
- Design: a type's plan is built once per run (`Plans`, reused across schedules) as a flat
  list of ops, with enum, variant, pointee and `UnsafeCell` tables as columns. The plan is
  found with one `FxHashMap` probe on the interned `Ty`, which is a pointer. The walk reuses
  scratch vectors, so a copy allocates nothing. Strings are built only for an error.
- What stands in the way: none known in the checker. Validating every typed copy is what
  Miri does, and the checks are what make it Miri's semantics.

### `run_test`, a test under several schedules

Work: S schedules of a test, each a full run of time T.

- Bound: with C cores and many tests, total work over cores, Σ S·T / C. The critical path is
  the longest single schedule, max T, because schedules are independent.
- Design: `run_test` runs the schedules of one test in a loop, reusing the type plans, and
  stops at the first that fails. A runner reaches the critical-path bound by giving each
  seed its own job: `run_test(db, test, &Config { seed: s + i, schedules: 1, .. })`. A job
  then rebuilds the plans of the types it uses, a few microseconds each.
- What stands in the way: within one `run_test` call, schedules run one after another,
  because `&dyn HirDatabase` can't be shared across threads. With one test and many
  schedules, splitting the seeds is the runner's job (`blaze test --ub`, lane cargo), and
  the call above is how it does it.

### Memory by lifetime ([memory.md](../../design/memory.md))

The checker's warm loop is the hooks above, and they make no allocation once an allocation's
columns, a type's plan and a site exist (`tests/ub/budget.rs`). What it does allocate, by
lifetime:

- **A schedule** (one `Checker`): the per-allocation columns (`made_at`, `freed_at`), the
  runnable list, and the parts' tables. These grow to the run's high-water mark and are
  dropped with the checker. They belong in the request arena of the test run, with its own
  tag (`ub`).
- **A test** (one `run_test`): `Plans` and its scratch buffers, kept across the test's
  schedules. They also belong in the test run's request arena. They can't outlive the
  database revision, because they hold `Ty<'db>`.
- **A failure:** the report's strings and the `Outcome`. These come from the general-purpose
  allocator, once per failing test, at the edge.

When `blaze-mem` lands, these columns move onto its arena and pool types. Until then they
are `Vec`s grown by doubling, which allocate O(log n) times per run.

### What `proof` should measure

- ns per `step`, per access, and per typed copy against the bounds above;
- the checker's run time over the plain evaluator's (`Plain`) on the same program;
- the checker against Miri on the same programs.

A component more than 10% off its bound is a bug.

## The proc-macro wire's fuzz targets: `fuzz/targets/proc_macro_*` (safety-fuzz-procmacro)

These targets are not part of `hir-ty-ub`, but they are safety's code, so their bounds go here.
A fuzz target's speed is runs per second per core. The engine runs one process per core with
nothing shared, so runs scale with cores. The bound for one run is the work its invariants
need: the reads, writes and comparisons each check names, at the reader's own cost. Anything
the target adds beyond that is overhead to remove. Costs are per input byte (n bytes), from
the per-op figures at the top:

- **Postcard decode:** about 4 cycles a byte. Most of a message is `u32` varints of 1 to 5
  bytes at about 5 cycles each, and COBS decoding plus `read_until`'s `memchr` add about 1.
- **Postcard and COBS encode** (`Message::write`): about 3 cycles a byte.
- **JSON** (legacy) decode or encode: about 5 cycles a byte.
- **`Debug` formatting:** about 50 cycles per `u32` through `fmt::Formatter`, so 15 to 50
  cycles a byte, which is 5 to 15 times a decode.
- **A one-byte-at-a-time read:** a `fill_buf`, `memchr`, `extend` and `consume` through
  `&mut dyn BufRead` per byte, about 60 cycles a byte on top of the decode. That is about 15
  times a whole read. Two-byte pieces cost about half that, and the uneven cut about a third.

The engine's own cost per run (mutation, resetting and comparing the coverage map, the 2 to 3
times slowdown of instrumented code) is blaze-fuzz's (safety-fuzz) and is the same for every
target, so it is not counted here.

### `proc_macro_message`: one postcard message

- Work: frame the input (1), read (4), write (3), read the frame back (4), and compare the two
  messages. The messages have no `PartialEq`, so the comparison is a second write (3) and a
  `memcmp` of the two frames. That makes 15n.
- Bound: with `PartialEq` on the messages, the comparison is about 0.5n, for 12.5n. The target
  is at 83% of that.
- Before this change: both messages were compared by `Debug` (`canonical`), about 2 × 25n, for
  62n in all, 20% of the bound.
- Design: compare the written frames, and use `canonical` only when they differ. That happens
  only when a hash table's entries come out in another order after a round trip (colliding or
  duplicate keys), which is rare.
- What stands in the way of the last 17%: the messages lack `#[derive(PartialEq, Eq)]`.
  Requested from usercode. `HashMap` and `HashSet` equality ignore order, so deriving it also
  removes `canonical`.

### `proc_macro_frames`: a stream of frames, whole and in pieces

- Work: read the stream whole (4n) and frame by frame as the reference (4n), read it once in
  pieces (4n plus the cut's overhead, which averages about 25n over the five cuts), and write
  every message of the three reads to compare them (9n). That makes 46n.
- Bound: with `PartialEq`, the writes become about 1n, for 38n. The target is at 83%.
- Before this change: every run read through all five cuts (about 144n in all) and compared
  seven reads by `Debug` (about 175n), for about 327n, 12% of the bound.
- Design: one cut per run (`shared::sizes`), picked by the input's length and last byte. The
  engine's mutations reach every cut over many runs, at one fifth of the cost. All three reads
  are of the same frame bytes, so they decode to the same message with its hash tables in the
  same order, and comparing written frames is exact here.
- What stands in the way: `PartialEq`, as above. Optionally, a public frame-level reader
  (`transport::postcard::read` is `pub(crate)`) would let the pieces read check framing alone,
  with no decode: 23n would become about 19n.

### `proc_macro_legacy`: lines of JSON

- Work: read whole and write each answer (10n), read once in pieces and write (5n, plus the
  cut's overhead of about 25n, plus 5n), then read each written line back and write it again
  (10n). That makes 55n.
- Bound: with `PartialEq`, one write per answer remains (the round trip needs it), and the
  others become comparisons, for about 46n. The target is at 84%.
- Before this change: four cuts per run, each read and written, about 173n in all, 27% of the
  bound.
- What stands in the way: `PartialEq` on `legacy_protocol::msg::Response`, as above.

### `proc_macro_flat_tree`: the token-tree encoding inside an answer

- Work: decode (4n, plus `to_subtree_resolved` at about 30 cycles a token, about 5n), encode
  (`from_subtree` with a hash-set insert per span, about 8n, plus 3n to write), decode again
  (9n), and compare the trees with tt's columnar `PartialEq` (about 1n). That makes 30n, which
  is the bound: each of these steps is the round trip itself.
- Before this change, `reshaped`, which collects every token into a `Vec` (about 3n), ran on
  every run. It now runs only when the trees differ, so the target is at its bound.

### `proc_macro_tt_wire`: `TopSubtree::read_wire`

- Work: read (about 2 cycles a byte for the checks, plus about 75 cycles per symbol to intern
  it, about 3n), write (0.5n, mostly copying columns), compare the bytes (0.1n), rebuild with
  `from_serialized` (a span-part hash lookup per token plus a refcount per symbol, about 6n),
  and read one strict prefix (at most 3n, 1.5n on average). That makes about 12.6n, the bound.
- Design: one prefix per run, its length set by the byte after the tree, or all but the last
  byte when nothing follows. The first version read four prefixes, about 2.5 reads (7.5n),
  for 17.1n.

### Memory by lifetime ([memory.md](../../design/memory.md))

The targets run in the fuzz engine's process, not in blaze's warm loop, so the zero-allocation
rule doesn't bind them. They also must not move the reader's allocations off the global
allocator: `blaze_fuzz::largest_allocation` counts the reader's calls to it, and that count is
the allocation bound each target checks. By lifetime:

- **One run** (a task, in memory.md's terms): what the reader under test allocates, which is what is
  measured; the written frames or lines the check compares, one per message; and the `Vec`
  of reads, a few doublings per run. The piece sizes live in a caller's `[usize; 8]`. If the
  engine ever gives each run a `blaze-mem` frame allocator, the target's own buffers can move
  onto it, outside the measured closure. Until then they are a few allocations per message,
  next to the reader's own.
- **The process:** seeds and fixtures (`shared::trees`, `shared::messages`), built once.
- **A failure:** `canonical`'s `Debug` text and the assert messages, at the edge.

### What `proof` should measure

- Runs per second per core for each target, at its corpus's mean input length.
- Cycles per input byte against the figures above.
- The targets' own share of a run, measured by profiling the engine with a target whose check
  does nothing.

`proc_macro_message`, `proc_macro_frames` and `proc_macro_legacy` sit at 83 to 84% of their
bounds until the messages derive `PartialEq`. That gap is a known bug, waiting on usercode.

## The fuzzer: the engine `fuzz/src/` and a fuzz job (safety-fuzz)

A fuzz run is the target's `check` on one input, plus the engine's work around it. The
engine's job is to stay a small, fixed cost per run, so that the job's rate is set by the
targets. The benchmark is libFuzzer, which does the same work per run: `ResetMaps`, then
`CollectFeatures` over the 8-bit counters, then the value profile and TORC. Its own bound
is the same as ours.

Figures are for one core of the target machines (3.5 GHz, as above). Per core, L2 delivers
64 B per cycle on both Turin and Graviton 5; L3 about half that. Stores run at 1 to 2 per
cycle, and a mispredicted branch costs about 15 cycles. L2 is 1 MiB on Turin and 2 MiB on
Graviton 5.

Symbols used below:
- N: the counters in the binary, one per instrumented block. Expect about 2^18 for a store or
  linker target and about 2^20 for the `hir_*` targets, which link hir-def and salsa.
- H: the counters a run reaches, about 10^3 to 10^4 for a parser.
- V: the value-profile bits it sets.
- L: the input's length.

### The engine's work per run

| step | work | bound (cycles) |
|---|---|---|
| clear before the run | read N bytes a line at a time; zero the D lines that hold a count | N/64 + D |
| drain after the run | read N bytes a line at a time; one feature per reached counter | N/64 + 3H |
| value profile | 1 Ki loads and 1 Ki stores (relaxed atomics: the watchdog may trace into it), an 8 KiB snapshot, one feature per bit | about 1.5 K + 3V |
| merge into the seen set | one bit test-and-set per feature, in a bitset of N + 8 KiB bytes held in L2 | 2(H + V) |
| pick and mutate | copy the parent, up to 5 mutations, each at most one move of L bytes | 6L/32 + 100 |
| clock | one vDSO read per run, shared by the deadline, the watchdog stamp and the status line | about 90 |

For N = 2^18, H = 2000, V = 500 and L = 1 KiB, the bound is 4096 + 500 + 4096 + 6000 + 3000 +
5000 + 300 + 90, about 23 K cycles or 6.5 µs. The scans are 8 K cycles of that, and the
per-feature work is most of the rest.

How the code reaches each term:
- `__sanitizer_blaze_fuzzer_run` and `__sanitizer_blaze_fuzzer_drain` (`raw.rs`) test a cache
  line of eight words for zero with one OR, which compiles to two 32-byte or four 16-byte
  loads. Only the lines with a count go word by word.
- The clear writes only the dirty lines. A `memset` would store all N bytes at store
  bandwidth (half the load rate) and dirty every line, which costs a write-back of N bytes per
  run once N outgrows L2.
- Both functions are named `__sanitizer_*`. SanitizerCoverage skips such functions (LLVM's
  `SanitizerCoverage.cpp`, and blaze's backend by the same rule), so the scan carries no
  counters and no trace-cmp calls. It also can't add bits to the value profile while reading
  it. Both run with ASan off (`#[sanitize(address = "off")]`).
- The merge indexes a bitset directly, at `index << 3 | bucket`, so a counter's eight buckets
  share one byte.
- The fuzz loop reads the clock once per run.
- The run buffer is reused, even after a keep; tests hold a run that keeps nothing and a
  keep to zero general-purpose allocations.

### Memory by lifetime ([memory.md](../../design/memory.md))

- **The process.** The kept corpus is three columns over reserved regions
  (`blaze_mem::RegionVec`): the inputs' bytes end to end, their ends, and the running sums
  of their weights. Each column grows by committing pages and never moves, so a keep is one
  copy and two pushes, and a pick is a binary search and a slice. Reaching the reservation
  (16 GiB of bytes, 2^26 inputs) stops the job with an error naming it. The seen bitset, the
  feature list and the run buffer are sized once at setup.
- **One run.** Nothing from the general-purpose allocator. A kept input's file is written
  from scratch buffers (text and two paths) that grow once to the longest input. The set of
  kept files is keyed by content hash and reserved for 2^14 entries, so it grows only past
  that, by doubling.
- **The edge.** Reading another worker's file (once per file), the directory listing (once a
  second), a finding's report and error messages allocate, each marked `// alloc:`.
- Until blaze-mem has a fuzzer tag, the corpus counts under `Tag::Other`
  (`v2/requests/mem.md`, "From safety-fuzz").

The previous version of the engine was far from these bounds:
- Its drain made a variable-length copy per 8-byte word, about 15 cycles, or 2N cycles in
  all: 60 times the scan's bound.
- It cleared the value profile with 1 Ki atomic swaps. Each is a locked `xchg` on x86, about
  20 K cycles in all.
- It cleared the counters with a full `memset` before every run.
- The drain was instrumented, so every comparison in it called a trace-cmp callback.

**Left between the code and the bound:**
- One mispredicted branch per run of nonzero lines in each scan, about 15 cycles each. With
  coverage in a few hundred clusters, that is a few thousand cycles per run.
  - If `proof` measures it above 10%, the fix is branch-free: append each line's index and
    add one when it is nonzero (`lines[n] = w; n += (any != 0) as usize`), then walk that
    list. The clear does the same over the indexes the drain found, plus the lines the
    engine's own code reached between runs.
  - Those lines are what keeps the clear a full scan. The mutator, the corpus pick and std
    (`Instant::now`, `Vec`) are instrumented, and they run between the drain and the next
    run. Making all of them uninstrumented would let the clear skip its scan and write only
    the D lines the drain listed, halving the scans.
- N itself. Every term above with N in it is time spent on code the input never reached, and
  a counter of a function the link dropped should not be there at all. That is requested of
  link (`v2/requests/link.md`, "From safety-fuzz"): drop SanitizerCoverage's entries with
  their function, as lld and ld64 do. Until then, a `hir_*` target's N may pass L2, and its
  scans run from L3 at half the rate.

### The comparison callbacks, once per comparison the target makes

Work per traced comparison:
- the return address;
- the operands' Hamming distance (`xor`, `popcnt`) and leading zeros (`sub`, `lzcnt`);
- two value-profile bits (each a mask, a shift, a load, an `or` and a store, in 8 KiB that
  stays in L1);
- one TORC slot (load and bump the index, three stores).

That is about 20 instructions, roughly 7 cycles at 3 per cycle, and the code is now exactly
that. The earlier version spent two locked `or`s and a locked `xadd` on each comparison:
- on x86, each locked instruction is a full barrier that drains the store buffer, about 20
  cycles;
- on Graviton, LSE atomics may execute far, in the interconnect;
- either way, 40 to 60 or more cycles per comparison, 6 to 8 times the bound, on the hot
  path of every target.

The relaxed load and store can lose a bit that two of the target's threads set at once,
which libFuzzer's maps accept too.

### A fuzz job: the rate of runs

- Bound: C cores give C / (T_check + T_engine) runs per second.
- One process uses one core, because a process's counters are its own. On a 16-vCPU CI
  runner that was a sixteenth of the bound.
- `fuzz --jobs N` runs N worker processes with seeds `seed + k`. They share the kept corpus
  through `out/<target>/corpus/`.
- Each worker reads the directory once a second: one listing with no `stat` per file, and
  only new names are read. Files appear whole, written under a `.` name and renamed.
- The parent polls its workers every 100 ms.
- Both costs are below 0.01% of a core, so N workers give at least 99% of N times one
  worker's rate.
- What remains is search overlap between workers, which is the same as with libFuzzer's
  `-jobs`, and shared L3 bandwidth once N passes L2. The second is covered by the link request
  above.
- CI is asked to run `--jobs "$(nproc)"`, and to build the 52 binaries in one build rather
  than 52 (`v2/requests/safety.md`, F1 amended).

### The plain `#[test]` of each target

- Work: 20000 changed inputs plus the corpus, each one `check` without coverage.
- Bound: the total over all cores. It used to run on one thread; `seeds` now hands out inputs
  through one shared atomic index on every core, and the first failure stops the rest.
- `Target::check` must allow calls on several threads at once. Every target does: the shared
  ones (`macro_wasm_*`) take their own lock, and `blaze_fuzz`'s allocation counter is per
  thread.

### Minimizing a failure

- Work per try: the target's `check`, plus handing over the input.
- Before, each try was a new process (`run`, about 2 to 5 ms with ASan's start-up), a file
  write, and a 1 ms polling sleep.
- Now `minimize` keeps one `serve` child and sends each try over a pipe, so a passing try
  costs one round trip, about 10 µs plus `check`.
- A failing try replaces the child, because a panic may leave state behind, such as a
  poisoned lock. Only failing tries pay for a process.
- A timeout is the child's own timer exiting with code 3, so the parent never polls.
- Bound: P passing tries times (T_check + 10 µs), plus F failing tries times the process
  start-up. ddmin needs about as many passing tries as failing ones, so this is within 2 times
  the in-process bound.
  - Getting the failing tries into the process too would need forking a warmed child per
    failure, and fork is unsound with the watchdog thread running. It isn't done, because
    minimizing runs only after a finding and is budgeted at 300 s.

### What `proof` should measure

- The engine's cycles per run with a `check` that does nothing, against
  `2N/64 + D + 5H + 5V + 1.5K`, on a small target (`cas_head`) and a large one (`hir_body`).
- Cycles per traced comparison: a `check` that runs 10^6 comparisons, against an
  uninstrumented build, over 10^6.
- Runs per second with `--jobs C` against C times one worker's rate.
- The plain test's wall time against its single-threaded time over the core count.

A component more than 10% off its bound is a bug.
