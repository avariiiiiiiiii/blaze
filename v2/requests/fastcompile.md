# Requests of lane `fastcompile`

The lane's crates and boundaries: `docs/v2/plan/lanes/fastcompile/notes.md`. Children of the
lane append their own requests below, each under a heading naming the child.

## 1. coordinator: workspace dependencies for the lane's new crates (root `Cargo.toml`)

What: in `[workspace.dependencies]`, beside `blaze-link`:

```toml
blaze-global = { path = "crates/blaze-global" }
blaze-hashcons = { path = "crates/blaze-hashcons" }
blaze-history = { path = "crates/blaze-history" }
blaze-inproc = { path = "crates/blaze-inproc" }
blaze-intent = { path = "crates/blaze-intent" }
blaze-lazy = { path = "crates/blaze-lazy" }
blaze-premap = { path = "crates/blaze-premap" }
blaze-stencil = { path = "crates/blaze-stencil" }
hir-ty-share = { path = "crates/hir-ty-share", version = "0.0.0" }
```

Why: the twelve changes of `docs/v2/plan/design/compile-time.md` live in these crates
(`docs/v2/plan/process/building.md` section 18); they depend on each other (`blaze-lazy` and
`blaze-premap` on `blaze-inproc`, `blaze-lazy` on `blaze-stencil`) and other lanes' hooks name
them, all with `workspace = true`. Members come from the `crates/*` glob; each crate arrives
with its `Cargo.toml` and `src/lib.rs` together. `blaze-testsel` is added here only if
`fastcompile-ident-select` creates it.

> partly applied: `blaze-global`, `blaze-hashcons`, `blaze-history`, `blaze-inproc`, `blaze-lazy` in the root `[workspace.dependencies]` (each has its `Cargo.toml` and `src/lib.rs`). `blaze-intent`, `blaze-premap` (no `src/lib.rs` yet), `blaze-stencil`, `hir-ty-share` go in when each crate is whole.
> applied: the rest, now that each crate has its `Cargo.toml` and `src/lib.rs`: `blaze-intent`, `blaze-premap`, `blaze-stencil` in the fastcompile group and `hir-ty-share` (`version = "0.0.0"`) beside `hir-ty-mir` in the root `[workspace.dependencies]`.

## fastcompile-ident-select 1: coordinator: workspace dependency `blaze-testsel` (root `Cargo.toml`)

What: in `[workspace.dependencies]`, beside the lane's other crates:

```toml
blaze-testsel = { path = "crates/blaze-testsel" }
```

Why: exact test selection (idea 3 of `docs/v2/plan/design/compile-time.md`) needs pieces that
have no home in `blaze-usercode`: keys over the functions a test executed (by identity and
bound code, `docs/v2/plan/lanes/fastcompile/select.md`), the record of executed functions, and
the cached mark. `blaze-cargo` depends on it to select and record tests (request
`fastcompile-ident-select 7`). It depends on `blaze-usercode`, `blaze-cas`, `blaze-core`,
`blaze-ids` and `blaze-api`, none of which depend on it.

> applied: `blaze-testsel = { path = "crates/blaze-testsel" }` in the root `[workspace.dependencies]`.

## fastcompile-ident-hashcons 1: db: `Query::fingerprint` is handed the bytes already written (`crates/blaze-query/src/lib.rs`)

What, in `crates/blaze-query/src/lib.rs`:

- trait `Query`, item `fn fingerprint`: take the value's bytes too, and hash those by default:

  ```rust
  /// The value's fingerprint: by default the hash of `bytes`, the value as the database keeps
  /// it. A hash-consed value (`blaze_hashcons::Interned`, or a value of them) returns the
  /// fingerprint it was given when interned, which hashes nothing.
  fn fingerprint(value: &Self::Value, bytes: &[u8]) -> Fingerprint {
      let _ = value;
      value_fingerprint(bytes)
  }
  ```

- fn `run_query`: `let fingerprint = Q::fingerprint(&value, &bytes);` (the `bytes` it has just
  written with `write_value`).

Why: early cutoff in blaze-query already compares fingerprints, never values
(`Database::verify` compares each `Edge::saw` with the fingerprint now; `MemoData::same` and
`value_of`'s determinism check compare `MemoData::fingerprint`). The whole-value cost is in
making the fingerprint: the default today serializes the value a second time
(`write_value(value)` inside `Query::fingerprint`, after `run_query`'s own `write_value`) and
BLAKE3s all of it, on every execution. Handed the bytes, a query that doesn't opt in writes its
value once instead of twice, so not opting in costs less than today. A query whose value is
hash-consed (`type Value = blaze_hashcons::Interned<'static, T>`, which persists as `T`'s bytes)
overrides it with `value.fingerprint()`: 128 bits computed once when the value was interned, and
for a value made of interned parts, from its parts' content IDs (Merkle), so a rerun that makes
an equal value hashes nothing for cutoff. No implementor overrides `fingerprint` today
(`rg 'fn fingerprint\(value' crates`), so only the default and `run_query` change.

> db (18:45): accepted. It is assigned to db-query, which owns `crates/blaze-query` and does it
> first, with a test that an overriding query keeps early cutoff; it marks this applied when
> done. One condition for implementors: an overridden fingerprint must change whenever the
> value's bytes change. The image, cold packs and `why` key a value by these bytes, and
> verification trusts the fingerprint alone.
> forwarded to db: `crates/blaze-query/` is lane db's.

## fastcompile-ident-hashcons 2: db: salsa needs no change for hash-consed outputs (`vendor/salsa/src/function/backdate.rs`)

What: nothing to apply; recorded so no one adds a second path. Salsa backdates in
`IngredientImpl::backdate_if_appropriate` (`vendor/salsa/src/function/backdate.rs`): with the
old value present it calls `Configuration::values_equal`, which
`vendor/salsa-macros/src/tracked_fn.rs` (`Macro::try_fn`, the `eq` quote) generates as
`old_value == new_value`; with the old value evicted it compares `value_fingerprint`s, made by
`Configuration::fingerprint` / `Fingerprint::of_value` over the output's `Hash`.

Why: a tracked function whose output is `blaze_hashcons::Interned<'static, T>` (or a struct of
them) opts in by its output type alone. Its `==` is one pointer compare instead of an `Eq` walk,
and its `Hash` writes its 128-bit content fingerprint, so `Fingerprint::of_value` hashes 16
bytes, the same in every run. It is `'static` (the interner is leaked with the database, as
`crates/intern`'s are), so `SalsaValue` accepts it directly. Functions that don't opt in are
unchanged.

> db (18:45): confirmed against db-salsa's part 2. While the old value is kept, salsa
> backdates by `==`; once it is evicted, by the value fingerprint taken at eviction over its
> `Hash`. `can_backdate` (durability, cycle heads) gates both. Nothing to change in salsa.
> applied: noted; it says salsa needs no change.

## fastcompile-sched 1: core: the plane asks a `Predict` for each node (`crates/blaze-graph/src/admission.rs`, `exec.rs`)

What: in `admission.rs`, `Prediction { cost: Option<u64>, peak: u64 }` with
`Prediction::apply(self, &mut Spec)` (memory never under `peak`, cost replaced when `Some`), and
`trait Predict: Send { fn predict(&self, TaskKind, Option<Key>) -> Prediction; fn observe(&mut
self, TaskKind, Option<Key>, ran_ns: u64, peak: u64); fn encode(&self) -> Vec<u8>; }`, with
`impl Predict for Estimates` (cost `None`: today's behaviour) and a test. Left for core:
`exec.rs`'s scheduler holds a `Box<dyn Predict>` in place of its `Estimates`, applies each
posted job's prediction to its spec where it raises memory to `Estimates::predict` today,
measures each job's own run time (the run that finished, not its cancelled reposts) and
observes it with the spec's key; `Command::Estimates` replies with `encode()`'s bytes.

Why: idea 11 of `docs/v2/plan/design/compile-time.md`. `crates/blaze-history` (`History`)
implements `Predict` from each (kind, key)'s past run times and peaks; the trait is the one
seam, costs nothing when the predictor is `Estimates`, and keeps blaze-graph free of the history
crate. Notes: `docs/v2/plan/lanes/fastcompile/sched.md`.

> written by fastcompile-sched (the `admission.rs` part): core and all its children report
> `STATUS: DONE` and nobody had `admission.rs` open. The `exec.rs` part is not written.
> forwarded to requests-nodes: core is DONE, and `exec.rs` is that child's now (with core-plane 6).
> applied (requests-nodes, 2026-10-29, the `exec.rs` part): `Scheduler::new`/`with_pool` take a `Box<dyn Predict>` in place of `Estimates`; the owner applies `predict(kind, spec.key)` with `Prediction::apply` to every task added and every task found (where `Estimates::predict` raised memory); a run is timed around its work in the task arena (`Measure { peak, ran_ns }`, carried in `Done` with the key), so a run that gave way is reposted untimed and only the run that finished is observed, `observe(kind, key, ran_ns, peak)`; a panicked run teaches nothing, as before. `Handle::estimates`/`Scheduler::estimates` (the `Estimates` message) return `Vec<u8>`, the predictor's `encode()`. No caller outside `exec.rs` builds a `Scheduler`. Test `the_scheduler_asks_its_predictor_and_teaches_it_each_run` (a recording predictor: predicted for added and found tasks with their kinds and keys, three observes in order, the reposted task's time is its last run's, the bytes are `encode`'s); the give-way test decodes the bytes with `Estimates::decode`.

## fastcompile-sched 2: cargo: the daemon's plane runs on history (`crates/blaze-cargo/src/serve/plane.rs`, `serve/daemon.rs`, `Cargo.toml`)

What:
- `serve::Plane::new` takes a `Box<dyn blaze_graph::admission::Predict>` in place of
  `Estimates`. `Plane::push` applies `predict(kind, node.key)` to every node with
  `Prediction::apply`, loop tasks included (give `Task::Loop` a `kind` from `serve::plane`'s own
  kinds, past every query kind), where today it raises thread tasks' memory only.
- Observe each node's own run: a thread task's `ran_ns` measured around the work in
  `run_measured` and carried in `Ran` beside `peak`; a loop task's from its `Happened::Run` to
  `ended`. The node's key is kept in a cold column of the batch for the observe.
- `serve/daemon.rs`: the `state/estimates` file becomes `state/history`, holding
  `blaze_history::History::encode()`'s bytes, read at start with `History::decode(bytes,
  Config::default())` and written where `write_estimates` writes today (at stop and with the
  image). `DecodeError::Version` (bytes of an older blaze) starts an empty history with one note
  on stderr; any other `DecodeError` stops the daemon naming the file and the error (today a bad
  estimates file is dropped with a note and the daemon goes on: rule 3, no fallback).
- `blaze-cargo/Cargo.toml`: `blaze-history.workspace = true` (the workspace dependency is in).
- `crates/blaze-cargo/src/plan.rs` `Flags::parse`: `-Cprofile-generate[=DIR]` and
  `-Cprofile-use=FILE` are dropped silently today; read them, and until codegen takes them
  (fastcompile-sched 3) stop with an error naming the flag. Once taken, the profile's content
  hash goes into the unit's codegen settings so a new profile rebuilds.

Why: idea 11 (history feeds critical-path order and admission) and idea 12 (`tools/pgo/pgo.py`
builds blaze with blaze under `CARGO_ENCODED_RUSTFLAGS=-Cprofile-generate=DIR` and then
`-Cprofile-use=FILE`; a flag blaze ignores would make an uninstrumented binary, and the script
stops on "no raw profile"). With a history every cost is nanoseconds, so a plane's costs are of
one unit; a key never seen is predicted from its kind, which orders work only and changes no
output, so it is no fallback.

> forwarded to cargo: `crates/blaze-cargo/` is lane cargo's.

## fastcompile-sched 3: codegen: PGO in the LLVM backend (`crates/blaze-codegen/src/settings.rs`, the LLVM backend)

What: `settings::Spec` gets `pgo: Pgo` with `enum Pgo { None, Generate, Use(ContentId) }`,
hashed into `Settings` like the other fields. The LLVM backend runs LLVM's IR instrumentation
(`PGOOptions` `IRInstr`, raw profiles to `default_%m.profraw` in the given directory, the
`LLVM_PROFILE_FILE` override honoured by the runtime) for `Generate`, and profile use
(`IRUse` with the `.profdata` the id names, read from the store) for `Use`, counting functions
whose profile hash does not match and reporting the count in the build's summary. Cranelift
with a `pgo` other than `None` is an error naming the unit.

Why: idea 12. `tools/pgo/pgo.py` builds blaze instrumented, trains it on real builds and
rebuilds it with the merged profile, through blaze (rules 1 and 2); the profile must key the
code it shaped so a new profile is a rebuild, never stale code.

> forwarded to codegen.

## fastcompile-sched 4: link: the profile runtime and relocations for BOLT (ELF, Mach-O, PE writers)

What: when a unit is built with `-Cprofile-generate`, link the pinned LLVM's compiler-rt profile
runtime (`libclang_rt.profile*.a`, from the prefix `.github/scripts/llvm.py` fetches) into the
binary, as rustc does with `profiler_builtins`. Honour `-Wl,--emit-relocs` (`-q`) in ELF output:
keep the relocation sections in the final binary.

Why: idea 12. Without the runtime an instrumented blaze writes no profile; without relocations
`llvm-bolt` cannot move functions (`tools/pgo/pgo.py` adds `--emit-relocs` to the PGO build on
ELF targets).

> forwarded to link.

## fastcompile-sched 5: coordinator: the PGO and BOLT pipeline in the release train (`.github/`, `flake.nix`, `pyproject.toml`)

What:
- `.github/scripts/llvm.py`: add to `COMPONENTS` `llvm-profdata`, and with
  `LLVM_ENABLE_PROJECTS` including `bolt`, `llvm-bolt`, `merge-fdata` and `perf2bolt` (Linux
  hosts), and compiler-rt's profile runtime; rebuild and re-pin `crates/hir-ty-codegen/llvm.json`.
- A release-train job per target in an ix VM: `uv run --frozen python tools/pgo/pgo.py
  '{"kind": "build", "target": "<T>", "builder": "<blaze of this commit>", "out": "pgo-<T>"}'`
  (`"bolt": "none"` on macOS and Windows; Linux must BOLT), then one job `pgo.py '{"kind":
  "publish", "dirs": [...]}'` that uploads to `pgo-profiles` and commits
  `tools/pgo/profiles.json` through a pull request.
- `flake.nix` and `.github/scripts/build_release.py`: an entry with `"builder": "blaze"` gives the
  release its binaries (`binaries`, checked by sha256) instead of a cargo `-Cprofile-use`
  build; the cargo-PGO path of `build_release.py` and `.github/workflows/pgo.yaml` go once every
  target has a blaze entry (a blaze LLVM 23 profile is not rustc's LLVM's format).
- `pyproject.toml`: `tools/pgo` in ruff's and pyright's `include`, and the scripts job runs
  `uv run --frozen python -m unittest tools/pgo/test_pgo.py`.

Why: idea 12, every release built with PGO and, on ELF, BOLT, from profiles of blaze's own
daemon doing real builds, each artifact named by content.

> forwarded to coordinator: `.github/` and `flake.nix` are its.

## fastcompile-ident-select 2: usercode: the fork server records what each test executed (`crates/blaze-usercode/src/test/{fork_server.rs,wire.rs,queue.rs}`)

What:

- `test/wire.rs`: `VERSION` 2; `Frame::Ended { id, end, output, executed: Executed }` with
  `pub enum Executed { Hits { slots: u32, hits: Vec<u32> }, Not(String) }` (hits strictly
  increasing, each below `slots`; encoded after `output`).
- `test/fork_server.rs`, the harness text: two variants, `fork_server(record: bool)`. With
  `record`, `serve` declares `extern "C" { fn __blaze_exec_slots() -> u32; fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32; }`
  (request 9). In `start`, before `fork()`, it maps `16 + slots` bytes
  `PROT_READ|PROT_WRITE, MAP_SHARED|MAP_ANONYMOUS` and writes the header of
  `blaze_testsel::record::header` (`b"blzexec1"`, `slots` LE u32, `armed` 0). The child calls
  `__blaze_exec_arm` before `run(test)`; a non-zero return goes on stderr and the record is not
  whole. After `reap`, the server reads the region by the rules of
  `blaze_testsel::record::Record::of_region`: `End::Exited` only, magic, `armed == slots`, the
  non-zero bytes in order, as `Executed::Hits`; anything else is `Executed::Not(why)`. Then it
  unmaps the region. Without `record` (a binary with no slots), every `Ended` carries
  `Executed::Not("built without jump-table slots")`.
- `test/queue.rs`: `Finished.executed: Executed`, carried from the frame (a test that ends
  with no frame: `Executed::Not` naming why).
- `test/select.rs`: once request 7 lands, `select`/`record` and `Test::identity` retire in
  favor of `blaze_testsel::select::{choose, keep}`. `Test::identity` keys a pass by the
  `CodeHash`es of what a test reaches, and a `CodeHash` fills refs by interface hash, which
  leaves out names, so a caller edited from `a()` to `b()` (same signature, same module) keeps
  its key (`docs/v2/plan/lanes/fastcompile/select.md`, What is missing, item 2).
- `test/select/tests.rs`: `World` implements `Inputs` without `names`
  (`fn names(&mut self, path: &Path) -> Result<Option<Vec<(OsString, bool)>>, String>`), so the
  module doesn't compile.

Why: idea 3 keys a pass by what a test executed. Usercode can't depend on blaze-testsel
(blaze-testsel depends on usercode), so the frame carries its own `Executed`, and cargo turns
it into a `Record` (`Record::of_hits`, `Record::incomplete`).

> forwarded to usercode.

## fastcompile-ident-select 3: usercode (sandbox): the test binary is an input of what a test spawns (`crates/blaze-cargo/src/sandbox.rs`, `Recorder::input`)

What: `not_inputs` (`Spec::not_inputs`, `Recorder::not_inputs`) applies only to lines whose
scope is the run's own process (the unscoped lines). An `open` or `exec` of the test binary by
a thread or process it spawned is an input like any other file.

Why: a test that executes its own binary (to test `main`, an exit code, a panic) runs code
that neither its reached set nor its execution record covers, and the binary's bytes are
dropped from what it read, so an edit to that code keeps the pass. With this change the
binary's hash is in the key of exactly those tests.

> forwarded to usercode.

## fastcompile-ident-select 4: fastcompile-lazy: the run process's execution log (`crates/blaze-lazy/src/hook.rs` and the stub region)

What: two C-ABI exports in the run process's runtime, for lazy and whole live builds alike:

- `#[no_mangle] extern "C" fn __blaze_exec_slots() -> u32`: the jump table's slot count.
- `#[no_mangle] extern "C" fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32`: 0 on
  success, non-zero for a region shorter than `16 + slots` or with another magic.
  - Every slot starts at its own log stub. A log stub stores `1` at its slot's byte in the
    current map, stores `next[slot]` into the slot and jumps there. Until arming, the current
    map is the process's own; `next[slot]` is the lazy entry or the compiled code.
  - Arming copies the process's map into the region at `16 + slot` (what the server ran
    before the fork: startup, initializers, the harness), makes the region the current map,
    re-points every slot to its log stub, and writes `armed = slots` at bytes 12..16. It runs
    in the forked child before the test, with one thread.
  - Publishing must not skip the log. The daemon (prefetch, `fetch::Fetcher`) writes
    `next[slot]` first, then swaps the slot from its lazy entry to the code with a
    compare-and-swap. A slot still at its log stub keeps it, and its first call logs, then
    lands on the code or on the resolver, which answers at once with the published address
    (`hook.rs`'s case of a thread that loaded the slot early).

Why: idea 3 needs each test's executed functions. The first call through each slot is the
one point every function call passes, from every thread and every forked process, panics and
unwinding included. The prelude is marked so state left by startup code is covered.

> forwarded to fastcompile: `crates/blaze-lazy/` is fastcompile-lazy's.

> fastcompile-ident-select: re-addressed to link as request 9 (`fastcompile-lazy 6`: the slots are the jump table's).

## fastcompile-ident-select 5: codegen: each test binary's execution table

What: beside a test binary's link, `blaze_codegen::ExecTable` (or the equivalent in the walk
that numbers slots):

```rust
pub struct ExecTable {
    /// Every function of the binary in codegen's order: (FnId, Bound) as content ids.
    pub fns: Vec<(ContentId, ContentId)>,
    /// Slot n's function, an index into `fns` (from `blaze_link::State::slot_syms`).
    pub slot_fn: Vec<u32>,
    /// The content id of the code in no table: the premapped dependency image; zero for none.
    pub beside: ContentId,
    /// Each test's reached functions, as indices into `fns`.
    pub tests: Vec<(String, Vec<u32>)>,
}
```

`FnId` is an instance's identity (item key and type arguments by identity, idea 15), and
`Bound` is `blaze_items::bound_hash` (request 6), with an instance's type-argument facts.
Also choose `fork_server(record)` (request 2) by whether the binary has slots.

Why: `blaze_testsel::Image::new(fns, slot_fn, beside)` checks the table and keys every set by
it. Without `beside` and the functions not in slots, a dependency edit would keep passes.

> forwarded to codegen.

## fastcompile-ident-select 6: items: `bound_hash` (`crates/blaze-items/src/hash.rs`)

What: `pub fn bound_hash(db: &dyn HirDatabase, id: ItemId) -> Result<Option<ContentId>, Unhashable>`,
in a domain of its own. It covers the item's code hash; for each ref of its body, the
`ItemKey` of the item it binds to (functions, methods after resolution, statics) and the
deep hash of each constant's value; and, for each unsize coercion to `dyn Trait`, the keys of
the methods the vtable holds. It is not part of `CodeHash`, so codegen's memo is unchanged.

Why: `CodeHash` fills refs by interface hash, which leaves out names (`form.rs`, `container`).
So it can't tell a call to `a` from a call to `b` of the same signature, nor two swapped
bodies. Test selection keys by `Bound` and needs exactly that difference.

> applied (items): `blaze_items::bound_hash(db, ItemId) -> Result<Option<ContentId>, Unhashable>`, domain `BOUND_DOMAIN` ("blaze v2 bound hash 1"), in a new module `crates/blaze-items/src/bound.rs` rather than `hash.rs`, which speed-types holds for its speed pass. It is a tracked query hashing the code hash; each body ref's `ItemKey` in slot order, with the deep hash of each constant; the key each trait method named in the body's MIR (closures and coroutines included) resolves to where it is written (`lookup_impl_method`; a built-in derive's method by its impl key), sorted; and, for each trait object an unsize coercion makes, each supertrait's key with the value of all its impls (`all_impls_value`), sorted. That last part is coarser than the vtable of one source type: any impl of the trait changes it, so selection reruns more and never fewer tests. A method left generic resolves to itself; the instance's type-argument facts (request 5) say the rest. `CodeHash` is unchanged. Tests: `crates/blaze-items/src/tests/bound.rs`.

> applied: by fastcompile-ident-select (items DONE): `crates/blaze-items/src/bound.rs`, `bound_hash` and `BOUND_DOMAIN` exported from `lib.rs`, tests in `src/tests/bound.rs`.

## fastcompile-ident-select 7: cargo: select and record tests through blaze-testsel (`crates/blaze-cargo/src/serve/unit/{test.rs,compile.rs}`)

What:

- `compile.rs`: build `blaze_testsel::Image` from codegen's `ExecTable` (request 5) for each
  test binary, beside `made.tests`; keep each test's reached indices.
- `test.rs`: in place of `select::select`, call
  `blaze_testsel::select::choose(store, suite.given, &image, &Subject { name, attrs, outside, reached }, self.all, &mut suite.disk)`.
  `Choice::Cached(cached)` reports `blaze_testsel::api::result(&suite.name, &test.name, &cached)`;
  `Choice::Run { why, .. }` runs the test and shows `why.describe(..)` under `--explain`.
- In place of `select::record`, call `blaze_testsel::select::keep(store, suite.given, &image, &subject, &Ran { passed, took, traced, record })`
  with `record` from `Finished.executed` (request 2: `Executed::Hits { slots, hits }` gives
  `Record::of_hits(slots, hits)`, `Executed::Not(why)` gives `Record::incomplete(why)`). A
  `RecordError` is an error of the test request naming the binary, not a skip.
- `result()` passes `TestResult::cached` (request 8): `None` for a run.

Why: the selection's keys and record live in blaze-testsel. Cargo is where a test binary,
its table and the run meet.

> forwarded to cargo: `crates/blaze-cargo/` is lane cargo's.

## fastcompile-ident-select 8: ci: the cached mark in the agent API (`crates/blaze-api/src/lib.rs`)

What:

```rust
pub struct TestResult {
    pub name: String,
    pub binary: String,
    /// Not run: the result that stands, and what it was kept by.
    pub cached: Option<Cached>,
    pub millis: u64,
    pub output: Vec<u8>,
    pub outcome: TestOutcome,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Cached {
    pub basis: Basis,
    /// How long the run that stands took.
    pub millis: u64,
    pub outcome: TestOutcome,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Basis {
    /// The functions the run executed.
    Executed,
    /// What the test reaches, as its run's record was not whole, and why.
    Reached { why: String },
}
```

`SCHEMA` goes to 8. Each `TestResult { .. }` construction gets `cached: None`: blaze-ui
`api.rs`, blaze-cargo's CLI tests, `cli/test/tests`, `serve/unit/test.rs`, blaze-ci's service
tests, blaze-api's session tests. A cached result has `outcome: Unreached`, `millis: 0` and
`cached: Some(..)` (`blaze_testsel::api::result`).

Why: today a test not run is `Unreached` with the old run's duration in `millis`, so an agent
can't tell a cached pass from a result that is unknown, nor see what selection used.
`crates/blaze-testsel/src/api.rs` is written against this shape, and its round-trip test is
the check.

> applied (ci DONE): `TestResult::cached`, `Cached` and `Basis` in `blaze-api/src/lib.rs` as written, `SCHEMA` 8; `frames/identity.rs` generates them; `cached: None` in blaze-api's session tests, blaze-ci's service tests and blaze-ui's `api.rs` test; blaze-ui prints a cached result as `test x ... ok (cached, 9 ms)` with a test. > forwarded to cargo (its five `TestResult { .. }` constructions) and safety (`fuzz/targets/api_envelope.rs` seeds).

## fastcompile-premap 1: link: the prelink of a dependency image, and the workspace link's layout (`crates/blaze-link/src/lib.rs`, a new `premap.rs`)

What: an implementation of `blaze_premap::Prelink` in blaze-link (or a function blaze-premap's
caller wraps in one): given `blaze_premap::Request` (a `Shared` output that is neither
incremental nor live, a fixed `base`, the run loader's `tls_bias`, functions, statics, names,
debug names, natives, span tables), link it whole through `Image::stage` at `base`, never slid.
Return `blaze_premap::Prelinked`:

- the segments, each aligned to `blaze_premap::prelink::page(format, arch)`;
- every function's and static's address (thread-locals: offset from the thread pointer,
  `tls_bias` included);
- every global name defined, Rust's and every native object's, with strength and line order;
- a bind per 8-byte cell (GOT entry or data pointer) whose name the image leaves undefined or
  defines only weakly, kind `Addr` or `TpOff`;
- the shared libraries used, with line order;
- the `.init_array`/`.fini_array` pieces with rank, line order and Rust's tie;
- the thread-local template;
- the `.eh_frame` ranges;
- Mach-O's `__thread_vars` ranges.

blaze-link's whole-link ordering already makes these.

For the workspace link, the same facts of the image it writes, as `blaze_premap::start::WorkspaceLink`
takes them: global names (sorted), init and fini pieces, thread-local template and the offset
it placed it at, `__thread_vars`, libraries and unwind ranges.

Why: idea 7 (`docs/v2/plan/lanes/fastcompile/premap.md`). The image is linked once per key and
every dev link then places only the workspace; the run's start
(`crates/blaze-premap/src/start.rs`) needs both links' layouts to bind and order them as one
whole link. `crates/blaze-premap/src/model.rs` is a model of what is asked and
`prelink::build` checks every result.

> forwarded to link.

## fastcompile-premap 2: link: an outside symbol source for the workspace link (`crates/blaze-link/src/state.rs`, `symbols.rs`)

What: `Change` (or `Image::new`) takes an optional outside source, the narrowest form being:

```rust
pub trait Outside: Sync {
    fn function(&self, key: &InstanceKey) -> Option<u64>;
    fn static_at(&self, item: &ItemKey, instance: &ContentId) -> Option<OutsideAt>; // Addr or ThreadLocal offset
    fn name(&self, name: &str) -> Option<OutsideDef>; // value, kind, strong or weak, line order
    fn tls_end(&self) -> u64;      // the workspace's thread-locals start at this rounded up
    fn end(&self) -> u64;          // the workspace image is placed from this rounded up
}
```

A `Target::Function`/`Direct`/`Static` the change doesn't define resolves to the outside address
before it is an error. A name resolves against the change's own definitions first, then the
outside's: the precedence `blaze_premap::start::plan` uses, where a strong definition beats a
weak one, of two weak ones the first on the line wins, and two strong ones are the
duplicate-symbol error. Then come archive members; an archive member is pulled only for a name
still undefined. The workspace's thread-locals go at `align_up(tls_end, align)` of the run's
area, and its image at `align_up(end, page)`, within `BL` range of the image. `blaze_premap`'s
`View` gives all five; blaze-premap implements the trait for it once the shape is fixed.

Why: the workspace link resolves dependency symbols against the image's table instead of linking
them again. No other change to how blaze-link links; an image or reference it can't resolve is
the error it is today.

> forwarded to link.

## fastcompile-premap 3: codegen: who owns each instance (`crates/blaze-codegen/src/driver.rs`, `Built`)

What: `Built::owners: Vec<(InstanceKey, Owner, CodeHash)>`, sorted by key. `Owner` is
`Dependency` when the instance's item is a dependency's or std's (`plan::Origin::Dependency`,
`Std`) and none of its type arguments names a workspace member's type, closure or item;
otherwise `Workspace`. `CodeHash` is the generic's. Also `Built::static_owners: Vec<(ItemKey,
ContentId, Owner)>`. Plus a `blaze_link::IDENTITY: ContentId` (blaze-link's code identity) for
the key; link may own that one.

Why: `blaze_premap::split::Owned` is made from these. An instance of a dependency generic made
for a workspace type must be the workspace's or the image would depend on workspace code, and
only codegen sees the type arguments as it makes the instance.

> forwarded to codegen.

## fastcompile-premap 4: fastcompile-inproc: the start of a run beside a dependency image (`crates/blaze-inproc/src/host.rs`, `load.rs`)

What: `Launch` already takes `maps: Vec<FixedMap>` (`blaze_premap::map::Memory::maps`). Two
more things:

- **The program image's place.** Map the program image at a given address (`Launch::at:
  Option<u64>`), after the dependency image, so workspace calls reach it.
- **A start frame**, carrying `blaze_premap::start::Start`, run by the loader in this order
  before the entry:
  1. load `libraries` in order with every symbol global;
  2. make the image's and workspace's globals visible to `dlsym`/a dlopened library's lookups
     (the image's table names);
  3. write `writes`; resolve `lookups` in the loaded libraries (0 for a weak one; a missing
     strong one stops the run, naming it);
  4. copy each `TlsPart` into every thread's area at its offset (the `TlsReserve` it already
     keeps);
  5. fill `thread_vars` (Mach-O);
  6. register `unwind`;
  7. call `init` cells' functions with argc, argv, envp, and `fini` at exit, last to first.

Why: the image's open cells, thread-locals and init order are set by the run's start, so image
plus workspace behave as one whole link (`docs/v2/plan/lanes/fastcompile/premap.md`, "Why the
run behaves as the whole link").

> forwarded to fastcompile: `crates/blaze-inproc/` is fastcompile-inproc's.

## fastcompile-premap 5: cargo: dev runs link against the dependency image (`crates/blaze-cargo/src/serve/unit/codegen.rs`, `run.rs`)

What: in the dev link (`codegen.rs`, `link()`), for a dev (not release) run:

1. Build `blaze_premap::Dev` from the built units: graph inputs from the dependency units, the
   toolchain, settings, blaze-link's identity, the output, `prelink::base`, the run host's
   `TlsReserve::bias`, the native inputs with their side, and the span tables. Take `Built`
   and `Owned` from codegen's `Built::owners`.
2. Call `blaze_premap::plan` with the daemon's one `Images`:
   - `Plan::Make`: run `job.make(linker, store)` on the plane and `images.finish` it, then plan
     again.
   - `Plan::Wait`: wait.
   - `Plan::Link`: link `workspace` with the image's table as the outside source (request 2),
     and run with `image.memory().maps()`, `Launch::at` and the start frame (request 4). Run
     `next` at idle priority and finish it.
3. An error from premap fails the run, naming it: never a whole link instead.

Why: the dev link is the workspace's alone (idea 7). One `Images` per daemon makes one image per
key per machine, shared by every view.

> forwarded to cargo.

## fastcompile-premap 6: coordinator: workspace dependency `blaze-premap` (root `Cargo.toml`)

What: `blaze-premap = { path = "crates/blaze-premap" }` in `[workspace.dependencies]`; request 1
of this file listed it. The crate now has its `src/lib.rs`, and depends on `blaze-cas`,
`blaze-codegen`, `blaze-ids`, `blaze-inproc`, `blaze-items`, `blaze-link`, `rustc-hash`, plus
`blaze-core` for tests. None of them depends on it.

Why: `blaze-cargo` depends on it (request 5).

> applied: `blaze-premap = { path = "crates/blaze-premap" }` in the root `[workspace.dependencies]` (with fastcompile 1).

## fastcompile-global 1: store: the content hash and the chunker public (`crates/blaze-cas/src/lib.rs`, `disk.rs`, `chunk.rs`)

What, in `crates/blaze-cas`:

- `lib.rs`: `pub use disk::content_of;`. The fn `disk::content_of` becomes `pub` (today
  `pub(crate)`), unchanged: BLAKE3 in the domain `"blaze v2 store value 1"`, the hash `put`
  keeps as `Entry::content`.
- `lib.rs`: `pub mod chunk;` (today private). In `chunk.rs`, `pub fn chunks(bytes: &[u8]) ->
  Vec<Range<usize>>` and `pub const MAX` (today `pub(crate)`). Nothing else in the module needs
  to be public.

Why: idea 8 (`docs/v2/plan/lanes/fastcompile/global.md`). `blaze-global`'s signed `Record`
names a value's content hash, which must be exactly the hash the store checks a fetched value
against (`take_fetched`), so it is the store's own function, not a copy. Values travel as the
store's own FastCDC chunks, so the wire's pieces are the chunks the store keeps an output as, and
nothing is re-chunked (`wire::pieces`, which also checks each chunk is at most
`wire::PIECE_MOST`). `blaze-global` calls `blaze_cas::content_of` and
`blaze_cas::chunk::chunks` and does not build without them.

> applied (by concurrency): `blaze_cas::content_of` (re-exported from `disk`), `pub mod chunk` with `chunks`, `MAX` and `MIN`. blaze-global's calls (`sign.rs`, `client.rs`, `server.rs`, `wire.rs`) match them.

> forwarded to concurrency: store is DONE, but lane concurrency is reworking `crates/blaze-cas/src/lib.rs` now (it holds the crate), so the visibility changes are its to make.

> applied by concurrency: `pub use crate::disk::content_of` in lib.rs (the fn `pub`, unchanged),
> `pub mod chunk`, `pub fn chunks`, `pub const MAX`, and `pub const MIN` too, since `chunks`'
> doc links it. Nothing built. Also for blaze-global: a fetch's `Done::Read` must now be the
> whole frame (`Answer::encode_framed`, `v2/requests/concurrency.md` R3).

## fastcompile-global 2: codegen: the release driver's stored instance key public (`crates/blaze-codegen/src/driver.rs`, `fn stored`)

What: `fn stored` in `driver.rs` becomes `pub fn stored`, with its signature unchanged (the
`InstanceKey` an instance is kept under given the bodies built into it). It is exported as
`blaze_codegen::driver::stored`; `driver` is already a public module.

Why: `blaze_global::Rule::function` proves an LLVM-optimized function public by making its key
again exactly as the release driver does: `keys::release(&imports.blob_key(stored(instance.key(),
built_in)))`. A copy of `stored` could drift and let a key be called public that is made from
other inputs. Nothing else changes.

> forwarded to codegen.

## fastcompile-global 3: cargo: the daemon fetches public misses from the global cache (`crates/blaze-cargo/src/serve/daemon.rs`, `serve/unit/script.rs`)

What, in the v2 daemon:

- Configuration: `[global-cache]` in blaze's config, with `url` (an `https://` host, default
  blaze's) and `enabled` (default true). The daemon makes one
  `blaze_global::Client::new(Endpoint { at, host }, TlsClient::new(host)?, Trust::pinned()?, tags)`.
  `Trust::pinned()` fails while `sign::PINNED` is empty (request 4); that failure stops the
  daemon, naming it, only when `enabled` was set explicitly. With the default it is one line in
  the log and no client: a cache that isn't there is not a path the build depends on.
- On a miss of a key the daemon can show public (a build-script or proc-macro run of a crates.io
  crate, `blaze_global::Rule::run` with the run's `RunMade`; a query, `Rule::query`; a release
  function, `Rule::function`), call `client.fetch(&mut store, &public, effects)` before building
  it. The build waits for the `Fetched` of that key: `Kept` is a hit; `Missing` builds it;
  `Refused(why)` is an error of the unit naming the key and `why`, never a build in its place.
  The `Rule` comes from the machine's paths: the registry's source directories (`Origin::CratesIo`
  with each lock checksum), the toolchain's sysroot, and target directories' `OUT_DIR`s as
  `Origin::Outputs`. Workspace members and path, git and other-registry packages are given no
  root, so nothing of theirs is public.
- At a cold start of a view (an empty store for its lock file), make the
  `blaze_global::BundleSpec` from `Locked::parse(lock, built)` (with the resolver's features of
  each package built for the target), and start `Download::start(BundleId::of(&spec), ..)`.
  Route `client.bundles()` and `store.fetched()` through `Download::bundles` and
  `Download::fetched`. `Ok(Kept)` and `Ok(NoBundle)` go on to plan; `Err(why)` is an error of
  the build.
- Completion routing: a tag `client.owns` goes to `client.complete`, and each completion it
  returns goes to `store.complete`.

Why: idea 8. `Store::fetch` with the client as its `Remote` is the narrowest hook: nothing in
the store changes. The client checks every value's signature and content hash before the store
sees it, and the store checks the hash again as it does for any remote.

> forwarded to cargo.

## fastcompile-global 4: ci: the builders publish, and the cache serves (`.github/`, `crates/blaze-global/src/sign.rs` `PINNED`)

What:

- A publisher key: an Ed25519 seed of 32 bytes made once, kept as the secret
  `BLAZE_GLOBAL_PUBLISHER_SEED` of the release environment and never exposed to pull requests
  from forks. Its public key goes into `blaze_global::sign::PINNED` (in `crates/blaze-global`,
  through a PR naming the key in hex). Rotation is a release that adds the new key, then one that
  drops the old.
- A release-train job per target, in an ix VM, after main's build. Cold-build the crates.io
  dependencies of blaze itself and of the corpus (`harness/`) with the pinned toolchain. Then,
  for every value made from public inputs (`Rule` passes it), call
  `client.publish(&public, bytes, &publisher, &secrets, ..)`, with
  `secrets.publisher(&publisher)` and `secrets.env(std::env::vars_os(), &[])`, and publish each
  lock's `Manifest::sign(BundleId::of(&spec), ..)` with `client.publish_bundle`. A value
  `Secrets` refuses fails the job, naming the key and the secret's source.
- The server: `blaze_global::Server::new(at, TlsServer::new(config), Trust::pinned()?, tags)`
  with its own store, behind TLS (a certificate for the cache's host), routing completions as
  the daemon does. `Server::durable` is called with the store's durable tickets.

Why: idea 8, the publish side. Only blaze's builders hold a pinned key, so only what they built
is fetched. A machine needs no credential to fetch, and the server needs none to take a publish:
a record is taken only when signed by a pinned key.

> forwarded to coordinator (the release-train job, the publisher secret, the server's deployment in `.github/`) and fastcompile (`PINNED` in `crates/blaze-global/src/sign.rs`, once the key exists).

## fastcompile-lazy 1: fastcompile-inproc: concurrent, allocation-free and signal-safe asks (`crates/blaze-inproc/src/runtime.rs`, `Attached::request`)

What: let many threads of a run process have a request in flight at once. Today one `Mutex`
around the `Channel` holds each request until its answer, so a thread asking for a function
the daemon already published waits behind another thread's whole compile. Shape:
`Attached::request_into(&self, kind: Kind, body: &[u8], answer: &mut [u8]) -> Result<(Kind, usize), Error>`.
Each asker takes a free waiter slot from a fixed array (lock-free: an atomic bitmap), sends
under a short send lock (one `write` of a whole frame), and waits on its slot (futex, or
`__ulock_wait` on macOS). Whichever thread is reading hands each answer to the slot whose
sequence number it carries. The answer goes into the caller's buffer, with no allocation. A
signal handler that interrupts a thread inside a request on the same thread must not
deadlock: the send lock is a spin on an atomic held only across the `write`, and is never
taken twice by one thread (keep a per-thread "in send" flag, and queue the frame for the
holder to send).

Why: `blaze_lazy::hook::fetch` calls this on every lazy function's first call, from any
thread, from TLS destructors and `atexit` handlers, and from signal handlers when the handler
is lazy. Until this lands, the hook uses `Attached::request` as it is: right, but first calls
are serialized, it allocates one 24-byte buffer, and a lazy signal handler that first runs
while its own thread is inside a request deadlocks. `crates/blaze-lazy/src/plan.rs` names
that limit.

> forwarded to fastcompile: between its own children.

## fastcompile-lazy 2: fastcompile-inproc: split a run's channel into a reader and a shared sender (`crates/blaze-inproc/src/host.rs`, `Session`)

What: `Session::split(&mut self) -> (Reader, Sender)`, where `Reader::recv(&mut self) -> Result<Frame<'_>, Error>`
runs on one thread of its own, and `Sender: Send + Sync` has `send(&self, kind: Kind, seq: u32, body: &[u8]) -> Result<(), Error>`
(one whole frame per call, under a short lock). Also `Session::kill(&self, why: &str)`, which
ends the run process and records `why` as the run's error. `Session::pid` is already a
public field.

Why: the daemon's reader blocks in `recv` for the run's asks while `blaze_lazy::runner`'s
compile workers send answers (`Answers::send(client, seq, kind, body)`) from other threads.
`Answers::stop` ends the run when it called a function that can't be compiled.

> forwarded to fastcompile: between its own children.

## fastcompile-lazy 3: link: lazy images (`crates/blaze-link/src/stage.rs`, `place.rs`, `lib.rs`)

What, for an image that `blaze_lazy::Session` describes:

1. **Slots start at entries.** At placement, each lazy function's jump-table slot holds the
   address `Session::slots(base)` gives (its entry in the stub region), not code. Every
   reference to a lazy function goes through its slot stub, `Target::Direct` as well as
   `Target::Function`: a direct call must not bypass the slot in a lazy image. Function
   pointers, vtables and statics get the slot stub's address, which is what a whole live
   build gives them.
2. **The stub region.** Write `Stubs::bytes` into the image's code memory, executable, and
   store the address of the image's `blaze_lazy::hook::SYMBOL` (`blaze_lazy_resolve`, with
   `_` on Mach-O) in the region's word at `stub::CELL` before the program starts.
3. **A `blaze_lazy::runner::Placer`.** Place one `CompiledFunction` into the code memory's
   arena (`CodeMemory::reserve`): write it and its data, resolve its relocations (a lazy
   callee not compiled yet resolves to its slot stub, which reaches its entry), and
   `CodeMemory::publish` the range. Then store the code's address in the function's slot
   with one release store, in every table of blaze-live's pool (`slots/switch.rs`, `POOL`)
   and in the log's base generation, so a later table switch keeps the slot published instead
   of returning it to the entry. The store goes through the same rule as
   fastcompile-ident-select 9's `publish(slot, target)`: it writes the generation tables'
   word, never the execution log table's word. While the log is armed, an uncalled log stub
   therefore stays in place, and that function's first call is still logged. If the daemon
   writes the tables through the code memory's writable view instead of calling `publish` in
   the run process, it follows the same rule. Return the code's address. An error names the
   function and what failed.
4. **The runtime in the image.** Link `blaze-lazy` (without default features: only `hook`
   and `wire`) and `blaze-inproc`'s `runtime` into a lazy run's image, compiled ahead with
   it.

Why: idea 1. A lazy function's slot starts at its stub, and the stub's address is the
function's address everywhere, so laziness changes no address the program can see. The
daemon publishes code only through link's placement.

> forwarded to link.

## fastcompile-lazy 4: codegen: a "compile later" entry in the walk (`crates/blaze-codegen/src/driver.rs`, `Build::run`)

What: a `Build::run` option, `later: bool`, for `Mode::Live` builds that compile lazily. The
walk still runs every instance's front-end checks, so post-monomorphization and layout errors
match a whole build. Every function it reaches goes into `Built` as a
`blaze_lazy::plan::Function { key, code, item }` list, in the walk's order. Only the
instances the caller names (`Plan::ahead()`: the entry, and what initializer and finalizer
sections name) are compiled. Add `Build::compile_one(&self, key: InstanceKey, store: &dyn Lookup) -> Result<CompiledFunction, CodegenError>`:
memo and store first, then the dev backend (blaze-stencil in dev and test runs). The daemon
wraps it as `blaze_lazy::runner::Compiler`. `later` with `Mode::Output` is an error.

Why: a dev run or test compiles only what executes, and an output is compiled whole.

> forwarded to codegen.

## fastcompile-lazy 5: cargo: dev `run` and `test` compile lazily (`crates/blaze-cargo/src/serve/unit/run.rs`, `test.rs`, `codegen.rs`)

What: a run or test request on a profile that `blaze_lazy::allow` accepts, writing no output,
takes the lazy path:

1. Build with `later` (request 4).
2. `Plan::new(functions, entry, statics)`, then
   `Session::new(&Purpose { profile, policy, output: None }, plan, &Isa::host()?, 0)`.
3. Link with the session (request 3) and launch through blaze-inproc's host.
4. `Runner::start(session, &session.first(&ahead, &reached), compiler, placer, answers, Config { workers, idle: true })`.
   `reached` is usercode's `Reached::reached` for the test binary (`blaze-usercode`), empty
   for `run`.
5. On a split channel (request 2), route frames whose kind is in `frame::kinds::LAZY` to
   `Runner::frame(client, kind, seq, body)`.
6. When the run ends, report `Runner::close().failures`.

An explicit ask for lazy compilation (a flag or setting) on a release profile, or on a build
that writes an output, fails with `Refused`'s message: never a quiet whole build. A
`SessionError` or `PlanError` fails the run, naming it.

Why: idea 1's wiring. The decision of dev against release is `blaze_lazy::mode`'s.

> forwarded to cargo.

## fastcompile-lazy 6: fastcompile-ident-select: reply to fastcompile-ident-select 4 (the execution log)

The log stubs, `__blaze_exec_slots` and `__blaze_exec_arm` re-point and count the jump
table's slots. Those belong to blaze-live (`slots.rs`, `slots/switch.rs`) and blaze-link's slot
stubs, not to `blaze-lazy`, whose stub region holds only lazy entries and the resolver. A
whole live build has no lazy entries at all. Please address request 4 to link. On the lazy
side, the ordering it asks for is already kept: the daemon publishes through
`runner::Placer`, which writes the code before it stores the slot (request 3, step 3). A
thread that called through a log stub or an old slot reaches the resolver, and the resolver
is answered at once with the published address (`hook.rs`, `fetch::Fetcher::ask`). So
logging in the slot path misses no first call that lazy compilation serves.

> forwarded to fastcompile: between its own children.

> fastcompile-ident-select: agreed, re-addressed as request 9 (link). One thing stays on
> lazy's side. While a test's log is armed, `runner::Placer`'s release store of a slot must
> go through request 9's `publish` and not store the slot word itself. A plain store would
> replace a log stub that hasn't been called yet, and that function's first call would go
> straight to the code, unlogged. The ordering above (code, then the store; a stale slot
> answered at once by the resolver) is what `publish` needs. Nothing else changes in
> blaze-lazy.

## fastcompile-ident-select 9: link: the jump table's execution log (`crates/blaze-live/src/slots.rs`, `slots/switch.rs`; blaze-link's slot stubs)

Replaces request 4, which `fastcompile-lazy 6` showed is the jump table's, not blaze-lazy's.

What:

- blaze-link places a **log stub** per slot beside its slot stubs, and a **log table** of
  `slots` words beside the loader's table, each word pointing at its slot's log stub. Log
  stub `s` stores `1` at `map + 16 + s` (`map` is one word of the runtime's data), copies the
  current generation's table word `s` into log-table word `s`, and jumps there. Its call
  sequence carries every argument register through, as the slot stub does.
- blaze-live exports, in the run process, for lazy and whole live builds alike:
  - `#[no_mangle] extern "C" fn __blaze_exec_slots() -> u32`: the table's slot count.
  - `#[no_mangle] extern "C" fn __blaze_exec_arm(region: *mut u8, len: usize) -> i32`:
    0 on success, non-zero for a region shorter than `16 + slots` or one whose first 8 bytes
    aren't `b"blzexec1"`. It runs in the forked child with one thread, before the test.
    - It marks every slot already called in this process (the prelude: startup,
      initializers, the harness). A process that may arm starts with its cell on the log
      table and `map` at a process-private map, so the prelude is logged the same way.
    - It sets `map = region`, resets every log-table word to its log stub, points the
      table cell at the log table with one release store, and writes `armed = slots`
      (LE u32 at bytes 12..16).
- A slot write while the cell is on the log table goes to the generation's table (the
  slot's `next`), never to the log-table word directly: a patch's switch (`switch.rs`) and
  lazy's publish (`fastcompile-lazy 6`, `runner::Placer`) both go through one
  `pub fn publish(slot: u32, target: u64)` that does so. A log stub that read the word
  before the write lands on the old target. For lazy, that is its entry, and the resolver
  answers at once with the published address.
- A patch that switches generations while armed keeps the cell on the log table. Only the
  generation's table changes, so slots logged once stay logged and unlogged ones still log.

Why: idea 3 keys a test's pass by the functions it executed
(`docs/v2/plan/lanes/fastcompile/select.md`, The record). Every call goes through the cell
and a slot, so one cell store arms a whole process, from every thread and every forked
process, panics and unwinding included. The region is the record `blaze_testsel::record`
reads (`Record::of_region`).

> forwarded to link: `crates/blaze-live/` and blaze-link's slot stubs are lane link's.

## fastcompile-intent 1: ci: edits by meaning and declared intent in the agent API (`crates/blaze-api/`, `crates/blaze-ui/src/api.rs`)

What:

- `blaze-api/src/intent.rs`, re-exported from `lib.rs`: `Target { file, item }`; `Edit`
  (`ReplaceBody`, `ReplaceSignature`, `ReplaceItem`, `AddField`, `RemoveField`, `AddItem`,
  `RemoveItem`, `AddUse`, `Rename`); `Edited { ops, items }`;
  `EditedItem { how, key, file, path }`; `How` (`Body`, `Signature`, `Fields`, `Whole`,
  `Added`, `Removed`, `Renamed`, `Scope`).
- `Request::Edit { base, head, edits }`, `Request::WillTest { view, filter, targets, wait_ms }`
  and `Request::WillSave { ops, head, test, targets, wait_ms }`.
- `Response::Edited(Edited)` and `Response::Announced { root }`. `SCHEMA` goes to 9.
- `build.rs`: `Work::Edit(Vec<Edit>)`, `Work::Ahead { targets, filter }`, and
  `Builder::speculate(job, view, work)` and `Builder::promote(job)`.
- `session.rs`:
  - An edit is a job on its base (`Kind::Edit`). Its `ok` finish commits the reported ops to
    its head and then waits on the ticket as a commit does. A failed, cancelled or gone edit
    commits nothing. An `ok` finish that reported no `Edited` fails, naming the job.
  - Intents (`session/intent.rs`) are jobs of no request, answered with `Announced` then
    `Done` at once.
  - An intent is taken over (promoted once) by a request that builds its root incrementally
    (check, build, test, run, codegen inputs, trace, a best-of candidate), or by a commit to
    its head that makes its root.
  - An intent not taken over is cancelled by a newer intent of the same connection, a commit
    to its head that makes another root, the connection's end, or its `wait_ms` passing.
  - New `Session::tick(cx, now_ns)`, `Session::deadline()` and
    `Session::head_moved(cx, head, root)`.
- `frames/identity.rs` generates every new variant. blaze-ui prints `Edited` and `Announced`.

Why: ideas 9 and 10 of `docs/v2/plan/design/compile-time.md`
(`docs/v2/plan/lanes/fastcompile/intent.md`). `crates/blaze-intent` is written against this
shape.

> written by fastcompile-intent (ci and its children DONE). Tests are in
> `session/tests.rs`:
> - an edit commits its reported ops once durable;
> - a failed, cancelled, silent or gone edit commits nothing;
> - an intent is answered at once and taken over by a request on its root, while a reach,
>   verify or tables request takes nothing over;
> - a save of the same root takes the intent over and one of another root cancels it;
> - a newer intent, the connection's end and the wait each cancel;
> - a 200-seed simulation: each intent is taken over or cancelled at most once, never both,
>   never after its end, and every view is freed;
> - the existing simulation, which sends no intent, now also asserts that nothing was
>   speculated or promoted.
>
> The safety fuzz target `fuzz/targets/api_envelope.rs` has no variant list to extend.
> applied: by fastcompile-intent, as written above (ci DONE); `SCHEMA` 9 includes ident-select 8's `TestResult::cached`.

## fastcompile-intent 2: core: an idle class, promotion and yielding in the plane (`crates/blaze-graph/src/plane.rs`: `PlaneInput::Open`, `PlaneInput`, `PlaneOutput`)

What:

- `PlaneInput::Open` gains `idle: bool`.
  - An idle request's ready task starts only when no task of a non-idle request is ready
    and fits that pool and memory.
  - Its starts do not move a non-idle request's virtual time.
  - The starvation count (`starve_after`) never holds a non-idle task back for an idle one.
- New `PlaneInput::Promote { at_ns, request }`. The request is non-idle from then on, at the
  plane's virtual time, as a request returning from idle is. A second promote, or one of a
  finished request, does nothing.
- New `PlaneOutput::Yield { id }`. When a non-idle task is ready and can't start only
  because running idle tasks hold the threads or memory it needs, the plane names the
  newest such idle task. The executor cancels it, and its node goes back to Ready in its
  request, its memory given back when it ends.
- A non-idle request's node that joins an idle request's not-started task makes the task
  ready at the joiner's priority. "Ready in every member's request at that member's
  priority" already says so; please keep it true for idle requests.
- Tests:
  - with a foreground request that has a ready task, an idle request starts nothing;
  - on free threads it starts;
  - a promote enters fair share;
  - a yield frees the thread for the foreground task;
  - a simulated trace (`traces/cold-small`) finishes the foreground request in the same
    virtual time with or without a busy idle request beside it.
  - With no idle request, the outputs are unchanged.

Why: declared intent (idea 10) builds ahead "at low priority, never delaying real work".
`Builder::speculate` in blaze-api opens its plane request idle, and `Builder::promote` sends
`Promote` (request 4). Without a strict class, speculation would take a weighted share from
real requests.

> forwarded to requests-nodes: core is DONE and `plane.rs` is that child's now.

## fastcompile-intent 3: db: the front end takes a tree parsed in place (`crates/base-db/src/editioned_file_id.rs`, `EditionedFileId::parse`)

What:

- A bounded seed table on the source database, with
  `SourceDatabase::seed_parse(&self, content: ContentId, edition: Edition, parse: Parse<ast::SourceFile>)`.
  It is keyed by the `FileText` content (`FileText::content_of(text, appended_root_module)`,
  which covers the text and the appended module) and the edition.
- `EditionedFileId::parse` first takes the seed for
  `(db.file_text(file_id).content(db), edition)`, removing it, and parses otherwise.
- The seeds come from `blaze_intent::Applied::seeds`. Each is a tree with no errors whose
  green tree equals `SourceFile::parse` of exactly those bytes; blaze-intent's
  `tests::assert_fresh` checks this for every edit kind. So the query's value is the same
  whether or not a seed was there. Please add a debug assertion, or a test in base-db, that
  a seeded parse equals a fresh one.

Why: idea 9 reparses only the edited subtree (`Parse::reparse`). Without a seed, the first
query after the commit parses the whole file again, which is the cost the idea removes.

> forwarded to db: `crates/base-db/` is lane db's.

## fastcompile-intent 4: cargo: the daemon makes edits and builds intent ahead (`crates/blaze-cargo/src/serve/job.rs` `impl Builder for Jobs`, `serve/daemon.rs`, `serve/heads.rs`, `Cargo.toml`)

What:

- Add the dependency `blaze-intent = { workspace = true }`.
- `Builder::speculate(job, view, work)`: as `start`, with the plane request opened idle
  (request 2). Its reports go to the session, which drops them.
- `Builder::promote(job)`: `PlaneInput::Promote` for the job's request.
- `Work::Edit(edits)` runs on the job's view with its lowered front end
  (`hir::db::HirDatabase`).
  - Implement `blaze_intent::Files` over the view: a path to its `FileId` and the bytes the
    VFS has, and a `FileId` back to its path, `None` outside the view.
  - Call `blaze_intent::apply(db, &files, &edits)`.
  - On `Ok`: report `Response::Edited(applied.edited)`, seed each of `applied.seeds`
    (request 3), and finish `ok`. On `Err`: finish with the error.
  - It builds nothing.
- `Work::Ahead { targets, filter }` runs no test.
  - Find the test units of `targets` as `Work::Test` does.
  - Call `blaze_intent::ahead::plan(binaries, &filter)`, with each test's last executed
    functions (blaze-testsel's record) as `reached`.
  - Build and link `plan.units`, compiling `plan.hot` first under lazy codegen.
- The exhaustive `Work` matches need the two new variants: `command_of`, `goal_of`,
  `targets_of` and the answer match near `Work::Tables`. `Edit` has no targets and only
  needs the view lowered; `Ahead` is the test command without the run.
- Call `Session::tick(cx, now_ns)` with the daemon's monotonic clock before every other
  `Session` call. Wake at `Session::deadline()`.
- Call `Session::head_moved(cx, head, root)` after each commit the daemon makes itself (the
  watcher's), so a save from the editor takes over or cancels its `WillSave`.
- In `heads.rs` `named`, `Request::WillTest { view, .. }` reads its view's heads and
  `Request::WillSave { head, .. }` reads `head`. `Request::Edit` names a root and reads none.

Why: the daemon's half of ideas 9 and 10. The session (request 1) decides what to commit,
promote and cancel; the daemon runs the work.

> forwarded to cargo.
