---
status: plan
summary: blaze v2 lane fastcompile, idea 3 (exact test selection): what exists in blaze-usercode, what is missing (executed versus reached, keys that miss which function a call binds to, the cached mark), crates/blaze-testsel's executed record, function table and selection over identity and bound code, the hooks requested from usercode, lazy, codegen, items, ci and cargo, the exactness argument and tests, and the state of the art it beats.
date: 2026-10-29
---
# fastcompile: exact test selection (`crates/blaze-testsel`)

Idea 3 of [compile-time.md](../../design/compile-time.md): each test run records what it
executed; after an edit only tests that executed a changed function, or read a changed input,
run again; the rest report their cached result, marked as cached. The code is in
`crates/blaze-testsel`; what it needs of other crates is requested in
`v2/requests/fastcompile.md` under `fastcompile-ident-select <n>`.

## What exists

`blaze-usercode` already selects tests, by what codegen's walk says each one *reaches*:

| Piece | Where | What it does |
| --- | --- | --- |
| A test's key | `crates/blaze-usercode/src/test/select.rs`, `Test::identity` | Hash of its name, attributes and the sorted `CodeHash`es of what it reaches |
| Selection | same file, `select` | Walks the test's history; a pass is kept under the identity and its suite's `given`, then by what it read, through `index::slot`/`index::result_key`/`Shape` |
| Recording | same file, `record`, `keepable`, `PASS` | Keeps a pass when it passed, is not outside, and its reads are known and keepable (no network) |
| What it read | `crates/blaze-usercode/src/trace.rs` (`Inputs`, `normalize`, `encode`); `crates/blaze-usercode/src/index.rs` (`Shape`) | Files, directories, environment, traced by the sandbox, part of the key |
| The skip | `select.rs`, `Decision::Skip { took }`; `crates/blaze-usercode/src/test.rs`, `report_unrun` | A skipped test is reported as `Reported::Unreached` with the last run's duration |
| The wiring | `crates/blaze-cargo/src/serve/unit/test.rs`, `select::select` and `select::record` calls, `result` | Maps the skip to `TestOutcome::Unreached` |
| The fork server | `crates/blaze-usercode/src/test/fork_server.rs`, `test/wire.rs` (`VERSION` 1, `Frame::Ended { id, end, output }`), `test/queue.rs` (`Finished`) | Forks one child per test; nothing about what the child executed |

## What is missing

1. **Executed versus reached.** Nothing records what a test executed. `Test::identity` is over
   the static reached set, so an edit to any function a test *could* call reruns it, though
   most tests reach far more than they run (a parser test reaches every error path). There is
   no per-slot counter or log: `crates/blaze-live/src/slots.rs` has slots and no first-call
   mark; `crates/blaze-lazy` resolves a function once and then calls go straight through.
2. **Keys that miss what a call binds to.** `blaze_items::CodeHash` fills each ref by the
   callee's *interface* hash (`crates/blaze-items/src/hash.rs`, the "code" domain's doc), and an
   interface leaves out the name (`crates/blaze-items/src/form.rs`, `container` writes a module
   as one byte). So two free functions of one signature in one module hash alike as callees: a
   caller edited from `a()` to `b()` keeps its code hash, and swapping two such bodies keeps the
   reached *set* of hashes. A test that called `a` and now calls `b` keeps its key and is
   skipped with a stale pass. Usercode's reached-set key has this hole today; blaze-testsel's
   keys do not ([Keys](#keys)).
3. **The inputs a test read, as part of its key.** These exist (`index`, `trace`), with one
   hole: the sandbox drops the test binary itself from what a run read (`not_inputs` in
   `crates/blaze-cargo/src/sandbox.rs`, `Recorder::input`, for every scope), so a test that
   re-executes its own binary (a common pattern for testing `main`, panics and exit codes)
   depends on code covered neither by its reached set nor by any record. Requested: drop
   `not_inputs` only for the run's own process, not for what it spawns.
4. **The cached mark.** A skipped test is `TestOutcome::Unreached` with the last run's
   duration in `millis`: an agent can't tell it from a test whose result is unknown, can't see
   the outcome that stands, and can't see what it was selected by. Missing:
   `blaze_api::TestResult::cached: Option<Cached>` with the cached outcome, its duration and
   its basis (executed or reached, with why).
5. **Saying which set was used.** `Decision::Skip` doesn't say whether the stored pass was
   keyed by an executed set or by the reached set, so an incomplete record would be
   indistinguishable from a whole one.
6. **A failure at unchanged code.** `select` reruns a failed test whatever changed (right), but
   only looks at the newest history entry, so a revert to code a test passed at, a few edits
   back, reruns it.

## Design

### The record

A test's child writes what it executed into a region the fork server maps shared before
forking it (`crates/blaze-testsel/src/record.rs`):

- 16 bytes of header: `MAGIC` (`blzexec1`), the slot count, and `armed`, which the runtime
  writes only after arming every slot; then one byte per jump-table slot.
- In the child, before the test's function runs, the harness calls the runtime's
  `__blaze_exec_arm(region, len)`. It marks every slot the server process ran before the fork
  (the prelude: startup, `.init_array`, the harness, everything resolved or compiled ahead), so
  state those functions left behind is covered; re-points every slot so its next call stores
  `1` at `HEADER + slot` and goes on to the target the slot held; and writes `armed = slots`.
- Threads the test spawns write the same idempotent bytes; processes it forks share the
  mapping. A process it *executes* is a new image: it is covered by its executable being a
  traced file input (with item 3's fix for its own binary).
- After reaping, the server reads the region (`Record::of_region`, with the scan
  `record::hits_of`, eight bytes a step) and sends the hits in `Frame::Ended`. The record is
  whole only if the header is ours, `armed == slots`, and the child exited; a signal, a kill,
  a timeout, a short region or a runtime that did not arm make it
  `Record::Incomplete` with why.

A panicking test, a `should_panic` test, unwinding, drop glue and TLS destructors all call
through slots like any code, so they are logged; panics need nothing special because the
child ends with `_exit` after the harness catches the panic. A whole-binary run (no fork
server) has no record: `Record::incomplete("run whole")`.

### Keys

A function is keyed by two hashes (`crates/blaze-testsel/src/table.rs`):

- **`FnId`**: what it is across versions, its item's key and, for an instance, its type
  arguments by identity (idea 15). A rename or a move makes another.
- **`Bound`**: its code as it runs: its code hash, the `FnId` of each function or method each
  ref binds to, the deep hash of each constant and static it refers to, for each unsize
  coercion the identities its vtable holds, and for an instance the facts of its type
  arguments its code reads. Requested from items as `bound_hash`.

The rebinding edit (`a()` to `b()`) changes the caller's `Bound`; a swap of two bodies changes
both `Bound`s. Each test binary carries a table (requested from codegen): its functions as
`Version { id, bound }` in codegen's order, each slot's function, the content id of the code
in no table (the premapped dependency image, `beside`), and each test's reached functions.

Code called not through a slot (and `beside`) is folded into one hash, `Image::unlogged`,
sorted by identity so table order doesn't matter; every set carries it. Any change there
reruns every test: this is where exactness costs selectivity, and it shrinks as more code
goes through slots.

### Sets and selection

A `Set` is a test's functions, sorted and deduplicated, plus the unlogged hash, encoded as
64-byte versions and stored by content (`Set::key_of`). A pass is kept under
`RunKey { code: H(name, attrs, set key), given }` and then by what the run read, through
usercode's `index::slot`/`result_key`/`Shape`, unchanged.

Each test has a history (`select.rs`): an append-only chain of entries in the store, found by
doubling then halving. Each holds passed, kept, attributes, the set key, the duration, the
basis and, for a kept pass or a failure, what the run read. `choose` walks it newest first,
at most `LOOKBACK` (16) entries:

1. Outside: run (`Why::Outside`). All asked: run (`Why::All`). No history: run (`Why::New`).
2. Other attributes: look further.
3. The entry's set is not the binary's now (`Image::changed`: a function changed, is gone, or
   the unlogged code changed): look further. Sets shared by entries are decoded once.
4. A failure at this code: if it read nothing that changed, or what it read is unknown, run
   (`Why::Failed`); else look further.
5. A pass not kept: look further.
6. A pass kept: if the store holds it for the inputs as they are now, the test is cached
   (`Choice::Cached { took, basis }`); else look further.

The `Why` of a run is the first explanation met, newest first.

`keep` stores the set under `Basis::Executed` from a whole record, or the reached set under
`Basis::Reached { why }` when the record is incomplete, saying why. A record of another binary
(slot count or slot out of the table) is a `RecordError`, never a quiet reached set. Failures
are not cached: a failed test always runs again, as Bazel does.

### The cached mark

`blaze_api::TestResult` gains `cached: Option<Cached>`, with
`Cached { basis: Basis, millis: u64, outcome: TestOutcome }` and
`Basis { Executed, Reached { why: String } }`, and `SCHEMA` goes to 8. A cached result has
`outcome: Unreached`, `millis: 0` (it took nothing now) and `cached: Some(..)` with the
cached run's outcome (always `Passed`) and duration. `crates/blaze-testsel/src/api.rs`
converts both ways and refuses a cached outcome that is not a pass.

## Why it is exact

A test's run is a function of the code it executes and what it reads (sandboxing makes the
reads traced; the clock, randomness and network are untraceable and make a pass not
keepable). Take a pass kept under set S and inputs T, and a binary now where every version in
S is the same and the inputs read under T have the same values.

- Every function the run called through a slot is in S (whole record) or in the reached set,
  a superset (incomplete record).
- Every function called not through a slot is in the unlogged hash, which S carries.
- Each function's `Bound` covers its code and which function each of its calls binds to, so
  the same versions make the same calls to the same functions, with the same constants, data
  and vtables.

By induction over the run's steps, it makes the same calls with the same values and reads the
same inputs, so it passes again. A function that is new in the binary can't be reached
without some function in S changing which function it calls (its `Bound`) or which vtable it
builds (also its `Bound`). The prelude is marked, so state left by startup code is covered.

The holes this closes are item 2 (callee rebinding) and item 3 (self-execution). Fallbacks
are named: an incomplete record keeps a pass under the reached set with why, shown as
`Basis::Reached` in `--explain` and in the API, never silently.

## Hooks

Each is a request in `v2/requests/fastcompile.md`:

| n | Owner | What |
| --- | --- | --- |
| 2 | usercode | The fork server maps and arms the record region and sends `Frame::Ended { executed }` (`wire::VERSION` 2); `Finished.executed`; usercode's reached-set selection retires in favor of blaze-testsel |
| 3 | usercode (sandbox, `blaze-cargo/src/sandbox.rs`) | `not_inputs` only for the run's own process |
| 9 (was 4) | link (blaze-live, blaze-link's slot stubs) | Log stubs and a log table; `__blaze_exec_slots` and `__blaze_exec_arm` arm by one store of the table cell; slot writes while armed go through `publish`, which lazy's `Placer` uses too |
| 5 | codegen | The per-binary exec table: versions in order, slot functions, `beside`, each test's reached indices |
| 6 | items | `bound_hash` |
| 7 | cargo | Wire `choose`/`keep` in `serve/unit/test.rs`, the table from `compile.rs`, the cached result in `result` |
| 8 | ci | `blaze_api::Cached`, `Basis`, `TestResult::cached`, `SCHEMA` 8, and every constructor site |

## Proof

The tests live beside the code:

- `record/tests.rs`: the eight-bytes-a-step scan against a byte-by-byte reference over random
  maps; region completeness (unarmed, short, foreign, killed); hits validation.
- `table/tests.rs`: the open-addressing lookup against a map; table refusals; the unlogged
  hash is independent of table order and sensitive to every unlogged version and `beside`;
  sets sort, deduplicate, round-trip and report the first change.
- `select/tests.rs`: an edit to a reached function that wasn't executed keeps the pass; an
  executed one reruns; a revert finds the old pass; failures rerun with the inputs they failed
  with; an incomplete record keeps the pass under the reached set and says why; unlogged
  changes rerun; a gone function, read inputs, unkeepable passes, outside, all, attributes and
  lookback; history end at every length; entry round trip.
- `sim.rs`, a deterministic simulation of a program whose function versions call others (some
  only under a bit of a file they read), read or not, and fail or not. Over random edits,
  reverts, input changes and incomplete records, a test not run must pass now and must have a
  past pass at the same versions of what it executed now and the same input. Over synthetic
  single-pass histories, a test runs exactly when a function it executed, the unlogged code,
  or an input it read changed: never skipped when its executed set meets the changed set, and
  never run when it doesn't.
- `api.rs`: a cached result's round trip through `blaze_api::Envelope<Response::TestResult>`
  in deterministic CBOR, with its outcome, duration and basis.

## State of the art

- **Bazel and Pants** cache a test target's result under the hash of every input of its
  action: the test binary and its runfiles. Any change to the binary (any function in any
  linked library) reruns every test in the target; nothing below the target is tracked. Pants
  does the same per test file with its dependency inference. Both run all tests in a target
  together, and neither sees runtime reads that aren't declared inputs.
- **Microsoft Test Impact Analysis** (Azure DevOps) maps tests to the source *files* they
  touched in an instrumented run, by coverage. A file edit reruns every test that touched any
  line of the file; it is described as safe only for managed code, and falls back to running
  everything when the map is missing or stale.
- **Ekstazi** (Gligoric, Eloussi and Marinov, ISSTA 2015) records the classes each test
  touched and their checksums, and reruns a test when any of them changed. It is class-level,
  so one method edit reruns every test that loaded the class, and it misses changes outside
  the JVM (native code, files read).
- **Facebook's predictive test selection** (Machalica et al., ICSE-SEIP 2019) trains a model
  on change features and past failures to pick likely-failing tests. It catches over 95% of
  faulty changes while running a third of the tests: unsound by design, every miss is a
  regression shipped.

blaze-testsel is function-level, not file- or class-level: it reruns only what executed the
changed code, and code a test merely reaches or loads doesn't count. Its keys are code
hashes, not timestamps or line coverage: a revert or a formatting change keeps the pass, and
going back to code within 16 runs finds its pass. Through `Bound`, it sees which function a
call binds to, which file- or class-level checksums only see as a whole. Runtime reads come
from the sandbox, so files and environment a test reads are in its key without being
declared. It is exact rather than predictive: no learned model, no miss rate. When it can't
be exact (an incomplete record), it says so and uses the reached set, which is still sound.
