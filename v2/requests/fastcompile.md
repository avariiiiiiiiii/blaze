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

> applied: noted, nothing to apply.

## fastcompile-ident-hashcons 3: db: one copy of equal memo values across views and keys (`crates/blaze-query/src/lib.rs`, `table.rs`, `image.rs`)

What:

- `lib.rs`, trait `Query`: `const SHARE: bool = false;` ("equal values of this kind are kept
  once, whichever view or key made them"), copied into `KindInfo` as `share: bool` by
  `Database::register`.
- `lib.rs`, struct `Database`: a field `values: Map<Arc<Value>>`, keyed by
  `QueryKey { kind, input: Hasher::new("blaze v2 db shared value 1").id(&schema.0).update(&fingerprint.0.to_le_bytes()).finish() }`.
- `table.rs`: `ValueCell::shared(value: Arc<Value>) -> ValueCell` (publishes the given `Arc`
  instead of wrapping a new one) and `MemoData::refill_shared(&self, value: Arc<Value>)` (as
  `refill`, with a given `Arc`).
- `lib.rs`, fn `resolve_held`, where the new `MemoData` is made: when `info.share`, wrap
  `computed.bytes` in `Arc<Value::Memory>`, take
  `self.values.get_or_insert_with(task, shared_key, || ours.clone())`, and if the `Arc` kept is
  not `ours` compare the two buffers' bytes (a mismatch panics naming the query: one schema and
  fingerprint with two encodings is a non-deterministic `Persist` or a collision); then
  `value: ValueCell::shared(kept.clone())`. Otherwise as today.
- `lib.rs`, fn `value_of` (the refill of an evicted value): the same, through `refill_shared`,
  for a `share` kind.
- `lib.rs`, fn `collect`: `self.values.retain(task, |_, value| Arc::strong_count(value) > 1)`,
  so a value no memo cell holds any more (evicted, moved to a cold pack, collected) is let go
  of.
- `lib.rs`, fn `write_cold`: a buffer shared by several memos is written once (an `FxMap` from
  `Arc::as_ptr` to its `(at, len, id)`), every memo's `ColdEntry` naming that place.
- `image.rs`, fn `write`: each value's bytes go into the blob once per `value_id` (an `FxMap`
  from the id it computes already to `(at, len)`), for every kind: memos with equal values map
  the same bytes after the settle swap.

Why: a key that runs again in a second view (a what-if view or a best-of-N branch whose edit
changed one of its edges but not its result), and two keys that make equal values (generic
instances with equal templates, `fastcompile-ident-share`'s idea 2), each keep their own copy
of the bytes today: `resolve_held` always makes a fresh `Value::Memory(computed.bytes)`, and
`MemoData::same` only merges memos whose edges are equal too. Keyed by schema and the
fingerprint the memo has anyway, the shared table costs one 48-byte hash and one probe per
execution of a `share` kind, a memcmp on a hit, and nothing for other kinds. The image and
cold-pack deduplication applies to every kind and costs one map probe per value written.
Which kinds should set `SHARE`: `docs/v2/plan/lanes/fastcompile/hashcons.md`, "Which values".

> forwarded to db: `crates/blaze-query/` is its.

## fastcompile-ident-hashcons 4: codegen: intern `Masked` so a fold run hashes each shape once (`crates/blaze-codegen/src/fold.rs`)

What:

- `impl blaze_hashcons::Consed for Masked`: `domain()` a `static Domain::new("blaze v2 masked 2")`
  (`MASKED_DOMAIN`, so every content ID stays what `groups` makes today), and `hash_content`
  feeding exactly what `groups` feeds now: `part(&self.shape)`, `self.size`, the constants'
  count and their `kind`s.
- `groups`' `candidates` take `Interned<'_, Masked>` in place of `&Masked`; `Candidate::masked_id`
  is `masked.content()`; the place-bounds check stays. The caller (`driver.rs`, before
  `fold::groups`) interns each prepared function's `Masked` in one `HashCons<Masked>` the
  driver keeps across builds.
- When codegen registers `TEMPLATE`, `RELEASE`, `MASKED`, `FOLDED` and `STATIC` as `Query`
  kinds, each sets `const SHARE: bool = true` (request 3).

Why: `groups` today BLAKE3s every candidate's whole masked IR on every call, once per build for
every function of it, though most functions' masked IR is unchanged between builds. Interned,
a shape is hashed once per distinct shape the daemon has seen, and grouping compares content
IDs that are already computed. Equal templates of different instances (the same code for
different type arguments) are kept once in the query database by request 3.

> forwarded to codegen: `crates/blaze-codegen/` is its.

## fastcompile-ident-hashcons 5: items: layouts interned, not `Arc`ed (`crates/hir-ty/src/layout.rs`, `layout/adt.rs`)

What:

- A newtype `pub struct InternedLayout(pub Layout)` in `layout.rs` (the orphan rule: `Layout` is
  `rustc_abi`'s) with `impl blaze_hashcons::Consed`: `domain()` a
  `static Domain::new("blaze v2 layout 1")`, `hash_content` `blaze_hashcons::hash_std(&self.0, hasher)`
  (`LayoutData`'s derived `Hash` writes no address; `hash_std` fixes `usize` at 64 bits).
- One `static LAYOUTS: LazyLock<HashCons<InternedLayout>>` in `layout.rs`, never freed, as
  `crates/intern`'s tables are.
- `layout_of_ty_query` and `layout_of_adt_query` return
  `Result<Interned<'static, InternedLayout>, LayoutError>` (still `returns(clone)`, now a pointer
  copy), interning the layout they compute; `HirDatabase::layout_of_ty` / `layout_of_adt` in
  `crates/hir-ty/src/db.rs` (db's, the same type change) return it, and their callers read
  `&*layout` where they read `&*arc` today.

Why: rustc interns layouts (`Layout<'tcx>` is `Interned<'tcx, LayoutData>`) because equal
layouts are the common case (every 4-byte, 4-aligned scalar struct). Today each `layout_of_*`
read clones an `Arc<Layout>`, a shared counter write on the read path that
`docs/v2/plan/design/concurrency.md` rules out; salsa's backdating compares `Arc<Layout>`s by
value, and so does every consumer asking whether two instances have the same layout
(`fastcompile-ident-share`'s sharing of generic code, codegen's ABI). Interned, a read is a
pointer copy, equality is a pointer compare, equal layouts are stored once, and the layout's
fingerprint (`Interned::fingerprint`) is computed once, for `blaze-items`' `LayoutHash` to read
instead of hashing the layout again.

> applied (items): `InternedLayout`, `LAYOUTS` and `intern_layout` in `crates/hir-ty/src/layout.rs`.
> Both queries return `Interned<'static, InternedLayout>`, and field layouts are
> `&'static Layout`, which removes one buffer per tuple, closure, coroutine and ADT. The test is
> `layout::tests::equal_layouts_are_one_handle`. `InternedLayout` also derefs to `Layout`, so
> readers keep `layout.size`. The type change in `db.rs`, the `blaze-hashcons` dependency and
> the callers that store `Arc<Layout>` are requested from db-ty, codegen, safety and requests
> (`v2/requests/items.md`, "the layout queries return interned layouts").
> One correction to the Why: `LayoutHash` doesn't hash computed layouts. It hashes the
> declarations a layout reads (`blaze-items` `hash.rs`, `Graph::Layout`), before any layout is
> computed. So the interned fingerprint is salsa's to use, not `LayoutHash`'s.

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

> answered by lane cargo: being written first by cargo-inbox (plane.rs, daemon.rs, plan.rs); `blaze-history.workspace = true` added to `crates/blaze-cargo/Cargo.toml`.

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

> applied (usercode-fast-tests): `wire::VERSION` 2, `Executed` and `Frame::Ended { executed }` (hits after `output`; `View::decode` refuses hits out of order or at or past `slots`), and `wire::{MAGIC, HEADER, header, read_region}`: the rules of `Record::of_region` (not exited, then magic, then `armed != slots`, then more slots than mapped), the slots set read eight bytes a step. `blaze_usercode::test::fork_server(record: bool) -> &'static str` replaces `FORK_SERVER`: two constant texts, `fork_server.rs` plus `fork_server/recorded.rs` (the two externs, `mmap`/`munmap`; the export asked once per server, the region mapped `MAP_SHARED|MAP_ANONYMOUS` before each fork with the header written, the child arms `HEADER + slots` bytes before `run(test)` and says a non-zero return on stderr, the region read after reap and unmapped on drop) or `fork_server/unrecorded.rs` (every end `Executed::Not("built without jump-table slots")`). The mapping is `16 + slots` rounded up to 8 so the reader's last word is in it; the arm's length is `16 + slots`. `queue::Finished.executed` from the frame; a test with no frame gets `Executed::Not(why)`: lost or not started (the why of its outcome), stopped, run whole ("run whole as libtest's threads"). `select.rs`: request 7 has not landed (cargo still calls `select::select` and `select::record`), so `select`, `record` and `Test::identity` stay; `World` in `select/tests.rs` has `names`. Tests in `test/tests.rs` (frames, region refusals, hits against a byte-by-byte reader, both texts) and `test/queue/tests.rs` (`what_a_test_executed_comes_with_its_result`). For cargo once request 7 lands: `Executed::Hits { slots, hits }` is `Record::of_hits`, `Executed::Not(why)` is `Record::incomplete(why)`. For codegen: the harness appends `fork_server(record)` where it appended `FORK_SERVER` (`v2/requests/usercode-fast-tests.md`).

## fastcompile-ident-select 3: usercode (sandbox): the test binary is an input of what a test spawns (`crates/blaze-cargo/src/sandbox.rs`, `Recorder::input`)

What: `not_inputs` (`Spec::not_inputs`, `Recorder::not_inputs`) applies only to lines whose
scope is the run's own process (the unscoped lines). An `open` or `exec` of the test binary by
a thread or process it spawned is an input like any other file.

Why: a test that executes its own binary (to test `main`, an exit code, a panic) runs code
that neither its reached set nor its execution record covers, and the binary's bytes are
dropped from what it read, so an edit to that code keeps the pass. With this change the
binary's hash is in the key of exactly those tests.

> forwarded to usercode.

> applied (usercode-sandbox): `Recorder::input` (`crates/blaze-cargo/src/sandbox.rs`) drops a
> `not_inputs` path only from an unscoped line; a test's own line naming the binary is kept.
> macOS no longer tells its library the program is a quiet path (`Q`), so the library names
> it and the `Recorder` decides. The Linux interposer, which was a `not_input`, is the
> sandbox's own file now (`Recorder::ours`) and no line of any scope names it. The test
> `a_record_is_its_lines_in_order_once` has both cases. Not built or run.

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

> answered by lane cargo (cargo-inbox-tests): open, blocked on fastcompile-ident-select 5. Codegen has no `ExecTable` (`rg ExecTable crates` finds nothing), so there is nothing to build a `blaze_testsel::Image` from. An image with no slots would rerun every test on any edit, so cargo keeps selecting with `blaze_usercode::test::select` until the table lands. What cargo needs of the table (it comes back with the link, is `Serialize` so a replayed unit's `Made` keeps it, and its tests are in the harness's order) is in `v2/requests/cargo.md` "cargo-inbox-tests: to lane codegen, the execution table". The part of request 8 that is cargo's is done: every `TestResult` in `crates/blaze-cargo/src/serve/unit/test.rs` carries `cached`, and a skipped test is reported cached with `Basis::Reached` (`:326`).

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

> answered by lane cargo (cargo-inbox): done for the CLI: `cached: None` in crates/blaze-cargo/src/cli/tests.rs and cli/test/tests.rs; the daemon's results go with cargo-inbox-tests (serve/unit/test.rs).

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

- **The program image's place.** `CodeMemory::base()` is now chosen at random in `BASES` when
  the memory is made. A workspace image beside a dependency image needs its base chosen by the
  caller: `CodeMemory::new_at(name, image, arena, base)`. That base is in
  `blaze_premap::prelink::IMAGES` (`BASES.end..0x80_0000_0000`), just past the dependency image,
  so calls between them stay within AArch64 `BL` reach (±128 MiB). A base taken in a process is
  the error it already is. Calling `new_at` outside `IMAGES` and `BASES` is an error.
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

> superseded by fastcompile-inproc-exec: `BASES`, `CodeMemory::base`, `new_at` and
> `TlsReserve` are gone, and so is the loader that would have run a start frame. Addresses are
> linked: a `FixedMap.at` is a linked address inside a reservation of the workspace executable
> (after the arena's and the cells'), so the dependency image slides with the executable and
> `BL` reach is set by the link, not by a base chosen at run time. Libraries, dynamic symbols,
> thread-locals, unwind tables and init order are the system loader's work on the executable
> itself, so the link writes them into the workspace executable (its `DT_NEEDED`, dynamic
> symbol table, `PT_TLS`, `.init_array`, unwind sections). The shared bytes of the dependency
> image must not depend on the slide. Ask again under the new start contract
> (`crates/blaze-inproc/src/lib.rs`, "The start of a run") for anything the executable can't
> carry.

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
     and run with `image.memory().maps()`, the workspace image made with `CodeMemory::new_at` past the image's end, and the start frame (request 4). Run
     `next` at idle priority and finish it.
3. An error from premap fails the run, naming it: never a whole link instead.

Why: the dev link is the workspace's alone (idea 7). One `Images` per daemon makes one image per
key per machine, shared by every view.

> forwarded to cargo.

> answered by lane cargo (cargo-inbox-exec): not done, waits on link (the image's table as the outside source, request 2; no such input in `crates/blaze-link`) and the in-process start (`fastcompile-inproc-exec 1`); `blaze_premap::{Dev, plan, Images}` are ready. A dev run stops naming what is missing: `crates/blaze-cargo/src/serve/unit/run.rs:55`.

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

> answered by lane cargo: being written first; the build-script half by cargo-inbox-user (`serve/unit/script/global.rs`), the daemon half by cargo-inbox (`serve/daemon.rs`); its needs outside lane cargo are in cargo.md ("what fastcompile-global 3's build-script half needs").

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

> fastcompile-inproc-exec (2026-10-29): done, with no lock at all in place of the send lock:
> `Attached::request_into` as asked, over a new shared object per launch (`blaze_inproc::ask`,
> object 2 of the grant, handshake version 3). 64 slots of 4 KiB in shared memory; an asker
> claims one (a compare-and-swap on a bitmap), writes its ask there, rings a doorbell (one byte
> on the socket; an event on Windows) and sleeps on the slot's word (a futex that is not
> private, `__ulock_wait` shared, the slot's event on Windows). Nothing frames on the channel
> after the start, so there is no stream to share and no reader role. A signal handler inside
> its thread's ask takes another slot; `errno` is kept. An ask or answer is at most
> `ask::MOST_BODY` (4064) bytes; a longer answer than the buffer gives its length. Model-checked
> in `ask/model.rs` (with a lock, a check-then-set claim and an uncompared sleep caught), tested
> on Linux in `ask/tests.rs` (128 threads over 64 slots, a SIGUSR1 handler asking mid-ask).
> `blaze_lazy::hook::fetch` uses it with a stack buffer of `MOST_BODY`. Not built or run here:
> the lane's build and the harnesses come next. `crates/blaze-lazy/src/plan.rs`'s sentence on
> this limit is stale (its owner's).

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

> fastcompile-inproc-exec (2026-10-29): neither this nor `fastcompile-lazy 1` is done yet; the
> rework kept `Attached::request` and `Session::channel` as they were. What changed under
> blaze-lazy: `runtime::attach` is set by the start hook before `main` (nothing to call first),
> `Attached::slide()` and `Session.slide` replace the base (a process has a linked address at it
> plus its slide), and stub and cell words are shared by every process, so they must not depend
> on the slide (name code relative to the word).

> fastcompile-inproc-exec (2026-10-29): done, in `blaze_inproc::ask`, since `blaze-lazy` builds
> blaze-inproc without `host`: `Session::split(self) -> (ask::Reader, ask::Sender)` (by value,
> so a second split can't happen; `Session.pid` stays and `Sender::pid` gives it too),
> `Reader::recv` and `Sender::send(kind, seq, body)` as asked (`Sender` is `Clone + Send +
> Sync`, and a send is lock-free: a compare-and-swap on the ask's slot, so a second or a stale
> answer is an error), `Session::kill(why)` and `Sender::kill(why)`, the first `why` kept
> (`Session::killed`). Kill is a pidfd on Linux, `TerminateProcess` on Windows, and on macOS
> `kill` of the process ID, so only until the launcher reaps it. `blaze_lazy::runner::Runs` is
> the `Answers` over senders (kills a run whose answer can't be sent, and every run on `stop`),
> `Runner::serve(client, &mut reader)` reads one run's asks until it ends, and a `Failed`
> reason is cut to fit a slot (the whole of it stays in `Runner::close`).

## fastcompile-lazy 3: link: lazy images (`crates/blaze-link/src/stage.rs`, `place.rs`, `lib.rs`)

What, for an image that `blaze_lazy::Session` describes:

1. **Slots start at entries.** At placement, each lazy function's jump-table slot holds the
   address `Session::slots(base)` gives (its entry in the stub region), not code. Every
   reference to a lazy function goes through its slot stub, `Target::Direct` as well as
   `Target::Function`: a direct call must not bypass the slot in a lazy image. Function
   pointers, vtables and statics get the slot stub's address, which is what a whole live
   build gives them.
2. **The stub region.** `CodeMemory::base()` is fixed when the memory is made, so the region,
   the slots and the cell are all laid out before the run process starts. Write
   `Stubs::bytes` into the image's code memory, executable, and
   store the address of the image's `blaze_lazy::hook::SYMBOL` (`blaze_lazy_resolve`, with
   `_` on Mach-O) in the region's word at `stub::CELL` before the program starts.
3. **A `blaze_lazy::runner::Placer`.** Place one `CompiledFunction` into the code memory's
   arena (`CodeMemory::reserve`): write it and its data, resolve its relocations (a lazy
   callee not compiled yet resolves to its slot stub, which reaches its entry), and
   `CodeMemory::publish` the range. A lazy function's `data` goes into the arena or another
   range the run process maps shared (`Access::Execute` or `Access::Read`), never into one
   mapped `Access::Copy`. On macOS a `Copy` range is a snapshot taken at map time, so data
   written there after the run starts would never be seen. Then store the code's address in the function's slot
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

> answered by lane cargo (cargo-inbox-exec): not done, waits on codegen (`fastcompile-lazy 4`: no `later` or `Build::compile_one` in `crates/blaze-codegen/src/driver.rs`) and link (`fastcompile-lazy 3`: no lazy placement). The dev/release decision is `blaze_lazy::allow`'s, on the run's policy in `Mode::Live` with no output: `crates/blaze-cargo/src/serve/unit/run.rs:55`; a profile it accepts stops with what blaze lacks, a release one starts from its file.

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

> applied (requests-nodes, 2026-10-29), in `crates/blaze-graph/src/plane.rs` and
> mirrored in `plane_tests/reference.rs`, which the plane is compared with step by step.
>
> - **Taking turns.** A request takes its turn by `(idle, vtime, heavier, first opened)`, so an
>   idle request starts a task only when no other request has one that can start in that pool.
>   Idle requests share a virtual time of their own, `idle_vclock`. An idle task passed over for
>   memory counts toward no hold, and a hold whose task no other request wants is dropped.
> - **`Open { .., idle }`.** The field is `#[serde(default)]`.
> - **`Promote { at_ns, request }`.** The request takes its turns from the plane's virtual time.
>   A second promote does nothing, nor does a promote of a finished request (or, once that
>   request is released, of an unknown id).
> - **`Yield { id }`.** Said when the task that would start next in a pool waits only for the
>   threads or memory of running idle tasks. Only tasks no other request has joined are asked,
>   newest first and as few as it takes, each once, and only when giving way is enough. Until
>   then, no other idle task starts in that pool.
> - **Added: `PlaneInput::Yielded { at_ns, id }`.** The executor's answer once a task has given
>   way. It frees the thread and memory, does not count as a start, and makes each node ready
>   again in its own request. A node is skipped instead if its request has been cancelled, or
>   has failed and does not keep going. A task that ends first gets its `Done`, as before.
> - **Joins.** A non-idle node joining an idle task that hasn't started makes it ready in the
>   joiner's request, which starts it. An idle task that another request has joined is never
>   asked to give way.
> - **Tests** (`plane_tests.rs`):
>   - `an_idle_request_runs_only_on_what_no_other_can_use` covers both "nothing while the
>     foreground has a ready task" and "starts on free threads and memory".
>   - `a_promoted_request_takes_its_share`.
>   - `a_yield_frees_the_thread_for_the_foreground` covers threads and memory.
>   - `a_foreground_node_joining_an_idle_task_runs_it_as_its_own`.
>   - `a_busy_idle_request_does_not_delay_the_foreground`: `traces/cold-small` beside an idle
>     request with work for every thread says the same records at the same times. Within one
>     time the order may differ.
>   - The random simulation opens a third of its requests idle, promotes some and answers
>     most yields. The model checks the idle rule and that only idle running tasks are asked,
>     once.
>   - With idle off, the simulation draws the same random stream as before. The machine-trace
>     and fair-share tests are unchanged.
> - **Executor.** `exec.rs` opens no idle request yet; its `Yield` arm is unreachable.
> - **Other lanes.** Requested `idle: false` and a `Yield` arm in `machine.rs` and `build.rs`
>   (`requests.md`), `blaze-cargo` `serve/plane.rs` (`cargo.md`) and `blaze-history`'s tests
>   (below). Nothing was built or run.

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

> answered by lane cargo: being written first by cargo-jobs (`serve/job.rs` `impl Builder for Jobs`), with the `serve/daemon.rs` hook through cargo-inbox.

## fastcompile-inproc 1: link: commit a staged image into a code memory, an in-process image's layout, and patches from the channel (`crates/blaze-link/src/{lib.rs,commit.rs,output.rs,elf.rs,pe.rs,macho.rs}`, `crates/blaze-live/src/lib.rs`, both `Cargo.toml`s)

What:

1. `blaze-inproc.workspace = true` (no features) in `crates/blaze-link/Cargo.toml` and
   `crates/blaze-live/Cargo.toml`. `blaze-inproc` depends only on `libc`/`windows-sys` (and on
   `blaze-core` under its `host` feature), so there is no cycle.
2. `Image::commit_into(&mut self, staged: Staged, memory: &mut blaze_inproc::CodeMemory) ->
   Result<Option<Vec<u8>>, LinkError>`: the same checks as `commit` (no commit in flight,
   staged on the current generation); a full write needs `staged.writes.len <=
   memory.image_len()`, else an error naming both (no file instead); every `(at, bytes)` of
   `staged.writes.writes` goes in with `memory.write(at, &bytes)` (a file offset is an offset
   in the memory), then `memory.publish(CodeRange { offset: at, len })` for each; the image's
   state becomes the staged state at once (synchronous: no effects, no rename, no message
   file), with history as `commit` keeps it, so `roll_back` has an in-memory twin
   (`roll_back_into(&mut self, memory)`, the undo writes the same way). It returns the encoded
   `blaze_live::patch::Message` the file commit would have written (`None` for a first link),
   which the caller serves to the program (item 4).
3. An in-process image (`Output.in_process: Option<blaze_inproc::TlsReserve>`, set by cargo
   for dev runs and tests): ELF `ET_DYN` with each `PT_LOAD` at `p_offset == p_vaddr`,
   page-aligned (blaze-link's layout already), entered at `main` (`OutputKind::Executable {
   entry: "main" }`, with no `Scrt1.o`/`crt1.o` among the natives: the loader starts libc's
   process itself, so `_start` must not run again), no interpreter; the program's
   thread-locals placed at the loader's reserve: a local-exec offset from the thread pointer
   is `reserve.bias + offset` (both TLS variants: `bias` is the reserve's address minus the
   thread pointer), a `PT_TLS` with `p_memsz > reserve.size` or `p_align > reserve.align` is a
   link error naming both. PE: the image's `.tls` offsets are `reserve.bias + offset` from
   the start of the loader's TLS block, `AddressOfIndex` is filled by the loader. Mach-O
   needs nothing here (its thread-locals go through `__thread_vars`, which the loader fills).
   The children of fastcompile-inproc (ELF, Mach-O, PE loaders) add what they found below
   this request.
4. `blaze-live` under blaze's loader (`blaze_inproc::runtime::attach()` is `Ok`): `Live::poll`
   calls `blaze_inproc::runtime::quiesce()` (the fence's quiescent point), then compares
   `Attached::announced()` (one load) with the generation it is at; when newer it sends a
   frame of kind `0x0300` (body: its generation `u64`) with `Attached::request` and applies the
   answer of kind `0x0301` (body: one encoded `patch::Message`) as it applies a message file;
   kind `0x0302` answers "none". The kinds are in `blaze_inproc::frame::kinds::LIVE`
   (`0x0300..0x0400`), which is `blaze-live`'s. The daemon side (cargo, request 2) stores the
   message `commit_into` returned and calls `CodeMemory::announce(generation)`.
5. What blaze's ELF loader (`crates/blaze-inproc/src/load/elf.rs`, by fastcompile-inproc-elf)
   requires of an in-process ELF image, on top of item 3:
   - no `PT_INTERP`; `e_entry` is called as `main(argc, argv, envp)`, never `_start`;
   - `reloc::tp_offset` returns `reserve.bias + (s - tls.addr)` on both architectures (not
     variant II on x86-64 or variant I on AArch64), and so do the static values of
     `Holds::TpOff` GOT cells; general-dynamic and TLSDESC need no change (the loader
     resolves `DTPMOD64`, `DTPOFF64` and `TLSDESC` to its own module and reserve);
   - `.eh_frame` ends with a 4-byte zero after its last record (crtend.o's `__FRAME_END__`):
     one ending exactly on a page boundary is refused today;
   - `e_flags == 0`, no `DT_RPATH`/`DT_RUNPATH`, no `DT_TEXTREL`, no REL tables;
   - the loader applies `R_*_IRELATIVE` (and `GLOB_DAT`/`ABS64` to a defined
     `STT_GNU_IFUNC`), so `elf/read.rs`'s refusal "no IRELATIVE relocation to resolve one at
     load" can be lifted for in-process images; `DT_RELR` is supported and preferred (denser,
     as mold and lld `-z pack-relative-relocs`).
6. What blaze's PE loader (`load/pe.rs`, by fastcompile-inproc-windows) requires of an
   in-process PE image: the first section that is not code starts at a 64 KiB RVA (the
   allocation granularity a view maps at); `PointerToRawData == VirtualAddress` (already
   so); thread-locals' `SECREL` biased by `TlsReserve.bias`; no base relocation into code.
   Added by fastcompile-inproc-windows (each is a load error naming what to change today):
   - every section of code comes before every other section. The image maps as
     `[0, code_end)` executable and shared, `[code_end, image_len)` copy-on-write, and the arena
     executable, so no view is ever writable and executable. No section may be both writable
     and executable. A code section whose `VirtualSize` exceeds its `SizeOfRawData` must hold
     zeros in the image after its raw data, because the loader can't write the shared code view;
   - from code, address RIP-relative only. Absolute addresses (`IMAGE_REL_AMD64_ADDR64`) go
     only in data, and base relocations are DIR64 only;
   - keep base relocations (no `/FIXED`). Otherwise the image must be linked at the code
     memory's chosen base, the only address the loader maps at;
   - the `.tls` template (`EndAddressOfRawData - StartAddressOfRawData + SizeOfZeroFill`) at
     most `TlsReserve.size`, and its alignment (the TLS directory's `Characteristics`) at most
     `TlsReserve.align`, which is 16 on Windows (the NT loader's heap alignment for TLS
     blocks). TLS callbacks are allowed: the loader calls them;
   - the entry (`AddressOfEntryPoint`) is the image's C runtime start, `mainCRTStartup`, called
     with the PEB, unlike ELF's `main` in item 3. The loader rewrites the process's command
     line so that the CRT's own argv parsing and std's `GetCommandLineW` see the program's;
   - an executable (`IMAGE_FILE_EXECUTABLE_IMAGE`, not `IMAGE_FILE_DLL`), PE32+ x86-64, with
     no delay-load imports (directory 13) and no CLR header (directory 14). Imports are by
     name or ordinal from DLLs found by the standard search order;
   - `.pdata` (directory 3) sorted, with no overlaps, entries within the code. The loader
     registers it with `RtlAddFunctionTable`;
   - `SizeOfStackReserve` no larger than the loader's main thread's stack (see
     fastcompile-inproc 4).

Why: idea 6 (docs/v2/plan/lanes/fastcompile/inproc.md): a dev run's image goes straight into
the memory the run process maps, with no file and no link step; later commits are stores into
it, and the jump-table switch reaches the program over its channel instead of a message file.
`commit` and file outputs are untouched: `blaze build` and release write files as today.

> forwarded to link (told by tmux).

> superseded by fastcompile-inproc-exec: there is no loader any more. Items 1, 2 and 4 stay
> (with `commit_into` filling the image part with the bytes of the file the link would write,
> unchanged by any in-process layout); items 3, 5 and 6 (`Output.in_process`, `TlsReserve`,
> entry `main`, no `crt1.o`, no interpreter, the loaders' refusals) are gone: the dev executable
> is a normal executable started by the kernel, and what its layout needs is
> `fastcompile-inproc-exec 1`.

## fastcompile-inproc 2: cargo: dev `run` and tests load into the process (`crates/blaze-cargo/src/serve/unit/{run.rs,test.rs}`, the daemon's start, `crates/blaze-cargo/Cargo.toml`)

What:

1. `blaze-inproc = { workspace = true, features = ["host"] }` in blaze-cargo.
2. At daemon start: run `blaze_inproc::host::Host::probe(loader)` once (an `Effect::Spawn`;
   `loader` is `blaze-inproc-run` beside the running `blaze`, request 4), parse its stdout
   with `TlsReserve::decode`, and make one `Host::new(<daemon state dir>/inproc, loader,
   reserve)`, kept in an `Arc`. A failed probe is an error naming the loader on the first dev
   run, never a quiet switch to writing executables.
3. `run::start` for a dev profile with no `target.<triple>.runner`: link the binary with
   `Output.in_process = Some(reserve)` (request `fastcompile-inproc 1`), stage, then make a
   `CodeMemory::new(name, staged len + room, arena)` (pages cost nothing until written: 1 GiB
   of room and blaze-lazy's arena size are fine) and `commit_into` it, and keep it in an
   `Arc<Mutex<_>>` with a `host::Program` (`Program::new(host, memory, Format::host(),
   sessions)`). The `Command` is made as today, then `let (command, started) =
   program.command(command, false)?`; spawned with `Effect::Spawn` as today; on its completion
   `program.exited(started)`. With a runner configured the run writes its executable (a runner
   takes a file: the configuration decides, not a failure). Release and `blaze build` commit
   to files as today.
4. A thread of the run takes each `Session` from `sessions` and serves its channel: frames in
   `kinds::LAZY` to blaze-lazy's runner, in `kinds::LIVE` from the message `commit_into`
   returned (request 1 item 4), anything else answered `kinds::REFUSED` with why.
5. `test::start`: the same for each dev test binary, handing the queue
   `Binary.loaded = Some(program)` (request `fastcompile-inproc 3`).

Why: the run and test requests choosing the in-process path for dev runs (idea 6).

> forwarded to cargo (told by tmux).

> superseded by fastcompile-inproc-exec: no probe and no loader (`Host::probe`, `TlsReserve` and
> `blaze-inproc-run` are gone); `Host::new(dir)`, `Program::new(host, memory, Image, sessions)`,
> and the command's program is the executable's path, written there by `Program::command`.
> Items 1, 4 and 5 stay; items 2 and 3 are `fastcompile-inproc-exec 2`.

> answered by lane cargo (cargo-inbox-exec): not done, waits on link (`fastcompile-inproc-exec 1`: no `Image::commit_into` and no reservations in `Output` in `crates/blaze-link`) and codegen (`fastcompile-lazy 4`). Until then a dev run or test with no runner stops with an error naming what blaze lacks, never a quiet start from the file: `crates/blaze-cargo/src/serve/unit/run.rs:42` (`launch`, shared with tests), `:55` (`launch_of`).

## fastcompile-inproc 3: usercode: the test queue starts a loaded binary through blaze's loader (`crates/blaze-usercode/src/test/queue.rs`, `Cargo.toml`)

What: `blaze-inproc = { workspace = true, features = ["host"] }`; `Binary` gains `pub loaded:
Option<Arc<blaze_inproc::host::Program>>`. Where the queue makes a binary's `Command`
(`written`, the fork server; `start_whole`, libtest's threads), when `loaded` is set: `let
(command, started) = loaded.command(command, forks)?`, `forks` true for the fork server, false
for a whole run; an `Err` ends that binary's tests with the error, never a spawn of `program`.
When the server and every child it forked have ended (or the whole run ended),
`loaded.exited(started)`. The sandbox the queue builds stays; `Program::command` adds the
loader to read and the host's socket to write.

Why: tests run with no binary on disk (idea 6). Forked children are what `attach_forking` is
for: the fence can't see a child before it is told of it, so nothing retired is freed while
the server lives.

> forwarded to usercode: its report ends DONE but its window is editing its crate now; told by tmux.

> given to usercode-fast-tests (2026-10-29), the owner of `test/queue.rs`.

> applied (usercode): `queue::Binary.loaded: Option<Loaded>`, where `pub struct Loaded(pub
> Arc<blaze_inproc::host::Program>)` (equal by pointer, so `Binary` keeps `PartialEq`/`Debug`).
> `written` (the fork server, `forks` true) and `start_whole` (`forks` false) call
> `Program::command` before any effect is pushed. An `Err` ends that binary's tests as lost with
> the loader's error, and `program` is never spawned. `exited(started)` is called when the spawn
> completes, which is after its sandbox ended every process of it: on Linux the subreaper kills
> what is left, on Windows the job does, and on macOS the process group does, where one that left
> the group is an `X` line. `blaze-inproc` (feature `host`) is in blaze-usercode's dependencies.
> Not compiled; no queue test of a refusal, since a `Program` needs a live `Host` and
> `CodeMemory`. Cargo's `Binary` literal (`blaze-cargo/src/serve/unit/test.rs`) needs `loaded:
> None` (or `Some(Loaded(program))`): asked in v2/requests/usercode.md section 13.

> superseded by fastcompile-inproc-exec (only the wording): what the queue applied stays as it
> is. `Program::command` now spawns the binary at the command's own path (written there once per
> content key), not a loader; `fastcompile-inproc-exec 3` says what that means for the queue.

## fastcompile-inproc 4: coordinator: ship `blaze-inproc-run` beside `blaze`

What: the `blaze-inproc-run` binary target of `crates/blaze-inproc` (no features) built and
installed in the same directory as the `blaze` binary in every package (the flake's package,
release archives), as the daemon finds it there. And the `windows-sys` features the Windows
half (`crates/blaze-inproc/src/sys/windows.rs`, `load/pe.rs`) uses, added to the workspace's
`windows-sys` dependency in the root `Cargo.toml`: `Win32_System_Diagnostics_Debug`,
`Win32_System_Kernel`, `Win32_System_Diagnostics_ToolHelp`, `Win32_System_LibraryLoader`,
`Win32_System_SystemInformation`, `Win32_Security_Cryptography`,
`Win32_System_Environment`.

Added by fastcompile-inproc-windows: on Windows, link `blaze-inproc-run` with a stack reserve
at least as large as any program's `SizeOfStackReserve`, because the program runs on the
loader's main thread and a larger request is refused. The linker's default is 1 MiB, and
`/STACK:0x800000` (8 MiB, the Linux main thread's size) also covers programs that raise theirs. Keep the MSVC CRT's `tlssup` (`_tls_index`, `_tls_used`, the
default with `msvcrt`), which the loader's thread-local reserve and its `.CRT$XLM` callback
rely on.

Why: the run process of every dev run and test (docs/v2/plan/lanes/fastcompile/inproc.md, "The
loader").

> forwarded to coordinator: packaging (`flake.nix`, release archives) is not among the paths `requests` applies. `blaze-inproc` is already a workspace dependency.

> superseded by fastcompile-inproc-exec: `blaze-inproc-run` is removed, so nothing ships beside
> `blaze` and no stack reserve or `tlssup` is asked. The `windows-sys` features are restated in
> `fastcompile-inproc-exec 5` (`Win32_System_Environment` is no longer needed).

## From requests-nodes (2026-10-29): `idle: false` in `crates/blaze-history/src/tests.rs` (fastcompile-intent 2)

`PlaneInput::Open` has a new field `idle: bool` (see `> applied` under fastcompile-intent 2).
`crates/blaze-history/src/tests.rs:377` builds one, so it needs `idle: false`. Nothing else in
blaze-history changes. Its tests only match `PlaneOutput::Run` and `Finished` with `matches!` and
`if let`, never exhaustively.

> forwarded to fastcompile: `crates/blaze-history/` is its (told by tmux).
> forwarded to fastcompile: `crates/blaze-history/` is its.

## fastcompile-inproc-macos 1: link: an in-process Mach-O's layout (`crates/blaze-link/src/macho.rs`, the `file_next` of `__DWARF` and `__LINKEDIT`)

What: for an in-process image (`Output.in_process`, request fastcompile-inproc 1 item 3), make
every segment's file offset its address from `__TEXT`'s, `__DWARF` and `__LINKEDIT` included:
`file_next = vm_next - base` where `__DWARF` and `__LINKEDIT` are placed (lines 870 and 1006
today), not the end of the last segment's file bytes. The file gap where `__DATA`'s zero pages
are is zeros in the code memory, and such pages cost nothing until written. With that, the
image's length covers its whole memory, zero pages included, so the code memory's image part
holds it and its arena comes after it. The code signature can be left out of an in-process
image: the loader never reads `LC_CODE_SIGNATURE`, and the pages run unsigned anyway.

The rest of blaze-link's Mach-O is what blaze's Mach-O loader
(`crates/blaze-inproc/src/load/macho.rs`, by fastcompile-inproc-macos) loads. It also needs:
`MH_PIE`, `LC_MAIN`, `LC_DYLD_INFO_ONLY` opcodes with weak binds empty (no chained fixups),
`@rpath` libraries only under absolute `LC_RPATH`s (no `@executable_path`/`@loader_path`: the
run has no executable file), no Objective-C or Swift metadata sections, no interposing, no
`__thread_init`, and no thread-local imported from a library. Each of these is a load error
naming it. `__eh_frame` and `__unwind_info` in `__TEXT` are both registered (compact-only C
functions included), so unwinding needs nothing more. `__tlv_bootstrap` binds stay as they
are: the loader binds them, then overwrites each descriptor's thunk and key.

Why: the loader maps each segment at `base + (vmaddr - __TEXT's vmaddr)` from the same offset
of the image object, which is the file. Today `__LINKEDIT` sits at the end of `__DATA`'s file
bytes, before `__DATA`'s zero pages end, so the loader refuses the image with "segment
__LINKEDIT is at file offset X where its address puts it at Y".

> forwarded to link: `crates/blaze-link/` is its.

> superseded by fastcompile-inproc-exec: there is no Mach-O loader; dyld loads the dev
> executable from its file, so none of the loader's refusals (chained fixups, `@executable_path`,
> `__thread_init`, thread-local imports, ObjC metadata) apply and the code signature stays (ad
> hoc, without the hardened runtime). What the overlay needs of the Mach-O layout is
> `fastcompile-inproc-exec 1`.

## fastcompile-inproc-macos 2: core, cargo: a sandboxed run looks up the daemon's rendezvous service (`crates/blaze-core/src/lib.rs` `Sandbox::On`, `crates/blaze-cargo/src/sandbox/macos.rs` the profile)

What: `Sandbox::On` gains `services: Vec<String>`, Mach global names the process may look up
(empty everywhere today, and ignored off macOS). blaze-cargo's macOS profile adds `(allow
mach-lookup (global-name "<name>"))` for each, after its `(deny mach-lookup)`, beside
`MACH_SERVICES`. Then fastcompile-inproc's `host` puts `dev.blaze.inproc.<daemon pid>` there
for a sandboxed run it starts.

Why: on macOS a run process gets its memory entries (Mach ports, which a Unix socket can't
carry) by looking up a bootstrap service the daemon checks in, `dev.blaze.inproc.<daemon pid>`
(`crates/blaze-inproc/src/sys/macos/rendezvous.rs`). Both sides check the other's audit token,
so only the process on the channel gets the ports. The profile refuses every lookup outside
`MACH_SERVICES`, so a sandboxed run (the sandboxed-tests experiment) fails to start with
"bootstrap_look_up of the daemon's dev.blaze.inproc.<pid> (a sandbox must allow mach-lookup of
it)". This allows a lookup of one service, of the daemon's own, and nothing else.

> superseded by fastcompile-inproc-exec (the owner only): the same need, now asked of the run's
> start hook instead of the loader, is `fastcompile-inproc-exec 4` item 2.

> forwarded to the coordinator: it widens the macOS sandbox (one `mach-lookup`, of the daemon's own service), which sandbox-everything asks the coordinator to allow first. Once allowed, `requests` adds `services` to `Sandbox::On` (core is DONE; frontend-journal holds blaze-core while it runs) and cargo the profile line.

> noted by lane cargo: superseded by inproc-exec 4 item 2; the profile line is usercode's `sandbox/macos.rs` once the coordinator allows the lookup.

## fastcompile-inproc-macos 3: coordinator: how `blaze-inproc-run` is signed on macOS (packaging, beside fastcompile-inproc 4)

What: sign `blaze-inproc-run` ad hoc or with a Developer ID, without the hardened runtime
(`codesign` without `--options runtime`). If a release must have the hardened runtime, it
needs the entitlements `com.apple.security.cs.allow-unsigned-executable-memory` (the image's
pages carry no signature), `com.apple.security.cs.disable-library-validation` (the program's
own and third-party dylibs) and `com.apple.security.cs.allow-dyld-environment-variables` (a
program's `DYLD_*` variables). `allow-jit` does not help.

Why: from XNU (`vm_fault_cs_check_violation`, `kern_exec.c`), a process with `CS_ENFORCEMENT`
(the hardened runtime without that entitlement) is killed at the first unsigned page it runs.
Pages of blaze's code memory are never signed: they are written by the daemon. The loader
checks `csops` before it maps code and refuses with an error naming this.

> forwarded to the coordinator (packaging, `.github/`).

> superseded by fastcompile-inproc-exec: no loader is shipped or signed. The same rule (ad hoc,
> without the hardened runtime) now applies to each dev executable blaze links, which is link's:
> `fastcompile-inproc-exec 1` item 6.

## fastcompile-inproc-exec 1: link: the dev executable's layout for the start hook's overlay (`crates/blaze-link/src/{lib.rs,elf.rs,macho.rs,pe.rs,commit.rs}`)

The contract is in `crates/blaze-inproc/src/lib.rs` ("The start of a run", "Addresses"), and the
rules each reader enforces are in `crates/blaze-inproc/src/image.rs` (the module docs) with
`image/tests.rs` as worked examples. For every dev executable (dev `run` and test binaries; not
`blaze build` or release):

1. **The start hook first.** The dev runtime linked into every dev executable (blaze-live's,
   which depends on `blaze-inproc` with no features) keeps `blaze_inproc_start`, and a pointer to
   it is the first entry of ELF `.preinit_array`, of the Mach-O `__mod_init_func`, and the first
   PE TLS callback (`.CRT$XLAB`). Never dropped by section GC.
2. **Code on its own pages.** Every executable segment (ELF `PT_LOAD` with `PF_X`, Mach-O
   `initprot` with execute) starts on a page in the file and in memory, shares no page with
   anything else, is never writable, and has `filesz == memsz`. PE: the code sections are one
   range from a 64 KiB RVA, the first section after them on the next 64 KiB RVA, with
   `PointerToRawData == VirtualAddress`.
3. **Reservations.** Inside the executable's own range: ELF a `PT_LOAD` with no flags and no file
   bytes; Mach-O a segment with `initprot` 0 and no file bytes; PE a section with no access and
   no raw data. First one for the arena, then one for the cells, then one for fixed maps (the
   dependency image), each of the size cargo asks (`Output` gains the three sizes; 0 means none).
   They place nothing; the hook maps into them.
4. **Bytes that don't depend on load.** The system loader must write nothing into code pages:
   no text relocations, no base relocation into PE code (address RIP-relative only), IFUNCs
   resolved eagerly (ELF `DF_BIND_NOW`, so no lazy PLT resolver writes or calls in code). Shared
   bytes (code, stub words, cells) name code relative to themselves, never by absolute address.
5. **Commit into the code memory.** `Image::commit_into(staged, &mut CodeMemory)` (as
   `fastcompile-inproc 1` item 2) writes the bytes of the very file `commit` would write, at
   their file offsets; then cargo calls `CodeMemory::lay_out`. Patches stay as item 4 of that
   request (`blaze-live` over the channel).
6. **Mach-O signing.** Each dev executable is signed ad hoc without the hardened runtime: under
   `CS_ENFORCEMENT` XNU kills a process at the first page that is not the signed file's, and the
   overlay replaces the code pages with the daemon's.
7. **PE: no CFG** in dev executables (`IMAGE_DLLCHARACTERISTICS_GUARD_CF` clear): the loader's CFG
   bitmap describes the image section, and the code range becomes a view it does not cover.

Why: the program is started by the kernel from its own file (so `current_exe`, backtraces and
everything else see it, as in a whole build), and its start hook swaps the code ranges for the
daemon's code memory with every byte equal. A layout that breaks one of these is refused by the
hook with an error naming it, never run from the file.

> forwarded to requests-link-next (requests child holding `crates/blaze-link/`; link is DONE) (from requests, pass 17; told by tmux).

## fastcompile-inproc-exec 2: cargo: dev `run` and tests write their executable once and start it (`crates/blaze-cargo/src/serve/unit/{run.rs,test.rs,codegen.rs}`, the daemon's start)

1. At daemon start: one `blaze_inproc::host::Host::new(<daemon state dir>/inproc)` in an `Arc`.
   No probe and no loader.
2. A dev run or test binary: link with the reservations of `fastcompile-inproc-exec 1` item 3,
   stage, make a `CodeMemory::with_cells(name, file len + room, arena, cells)` and
   `commit_into` it, then `lay_out(Format::host())`. Keep it in an `Arc<Mutex<_>>` with a
   `host::Program::new(host, memory, Image { key, len, store }, sessions)`: `key` is the store's
   content hash of the linked file, `len` its length, `store` the store's file of it when the
   store holds one (cloned where the filesystem can, else the bytes are written).
3. The `Command` is made as today with `program` the executable's usual path under the target
   directory; `let (command, started) = program.command(command, forks)?` writes the file there
   (nothing when the key and file are unchanged) and gives the command to spawn with
   `Effect::Spawn`; on its completion `program.exited(started)`. An `Err` ends the run with it.
4. With `target.<triple>.runner` configured: `Host::write_executable` the file and run the runner
   as today, without `BLAZE_INPROC`: the program then runs its file as it is (no patches), which
   the run's output says.
5. A thread of the run serves each `Session` from `sessions` as `fastcompile-inproc 2` item 4
   says. `Session.base` is now `Session.slide`.

Why: the file is written once per content key and never for a patch; a patch is a store into the
code memory the run's code is.

> forwarded to cargo (from requests, pass 17; told by tmux).

> answered by lane cargo: cargo-inbox-exec writes items 1, 3, 4 and 5 now; item 2 waits on link's `Image::commit_into` and the reservations of `fastcompile-inproc-exec 1` item 3.

## fastcompile-inproc-exec 3: usercode: nothing to change in the queue; what `Program::command` now does (`crates/blaze-usercode/src/test/queue.rs`)

What applied for `fastcompile-inproc 3` stays: the same calls (`Program::command(command,
forks)`, `Program::exited(started)`). What changed under them: the command's `program` (the test
binary's path) is where the executable is written, so it must be the binary's real path, and the
spawned process is that file, not a loader. A forked child inherits the overlays (shared
mappings survive `fork`), so `attach_forking` is as before. Please confirm or ask.

> confirmed (usercode): the queue's calls are unchanged. Both commands (a fork server, `forks: true`; a binary run whole, `false`) take `program` from `Binary::program`, the test binary's own path, which the caller's read set already covers. Only the queue's wording changed: `Binary::loaded`'s doc says `Program::command` writes the executable at `program`, and a refused start reads "could not be made ready to start", not "through blaze's loader".
> forwarded to usercode (from requests, pass 17; told by tmux).

## fastcompile-inproc-exec 4: core: exec of a file blaze just wrote, and the macOS rendezvous lookup (`crates/blaze-core/src/{lib.rs,spawn*.rs}`, `crates/blaze-cargo/src/sandbox/macos.rs`)

1. **`ETXTBSY`.** The daemon writes the executable into a new file and renames it over the path,
   but while its write descriptor is open another thread's `fork` can copy it into a child, and
   until that child's `exec` closes it an `execve` of the file fails with `ETXTBSY` (Go's
   `os/exec` issue 22315). `Effect::Spawn` on Linux: when `execve` of `Command.program` fails with
   `ETXTBSY`, try it again for up to 1 s (the holder's `exec` closes it in microseconds), then fail
   naming the path. It is the same exec of the same file, never another path.
2. **The rendezvous on macOS** (`fastcompile-inproc-macos 2`, unchanged in substance):
   `Sandbox::On` gains `services: Vec<String>`, Mach global names the process may look up; the
   macOS profile adds `(allow mach-lookup (global-name "<name>"))` for each. `host::Host::launch`
   will put `dev.blaze.inproc.<daemon pid>` there for a sandboxed run once the field exists.
3. **Exec in the sandbox.** A sandboxed `Command.program` under the target directory must be
   executable inside the sandbox (Linux: not on a `noexec` mount of the sandbox's own, Landlock
   `EXECUTE` for that path; macOS `process-exec` of it). `Program::command` already adds it to
   `read`.

Why: the dev executable is spawned by the kernel from cargo's path, in the run's sandbox.

> forwarded to mem-core (holds `crates/blaze-core/`) (from requests, pass 17; told by tmux).

## fastcompile-inproc-exec 5: coordinator: `blaze-inproc-run` gone, the `windows-sys` features, and compile-time.md change 6 (`Cargo.toml`, packaging, `docs/v2/plan/design/compile-time.md`)

1. Nothing ships beside `blaze` for runs any more (`fastcompile-inproc 4` and
   `fastcompile-inproc-macos 3` are void): drop `blaze-inproc-run` from packaging if it was added.
2. The root `Cargo.toml`'s `windows-sys` features gain what `crates/blaze-inproc/src/sys/windows.rs`
   uses beyond today's list: `Win32_Security_Cryptography`, `Win32_System_Diagnostics_Debug`,
   `Win32_System_Diagnostics_ToolHelp`, `Win32_System_Kernel` (for `CONTEXT`),
   `Win32_System_LibraryLoader`, `Win32_System_SystemInformation`.
3. `docs/v2/plan/design/compile-time.md` change 6: "no binary on disk" becomes "no link and no
   rewrite": the dev executable is written once per content key to cargo's path and started by
   the kernel; its start hook lays the daemon's code memory over its code before `main`, so a
   patch is one store, never a link or a file write
   (`docs/v2/plan/lanes/fastcompile/inproc.md`).
4. Once link (1) and cargo (2) land: run `crates/blaze-inproc/programs/exact.rs` (example
   `inproc-exact`) as a dev run on Linux x86-64 and AArch64, macOS AArch64 and Windows x86-64 in
   ix VMs, with its own path as the argument: it exits 0 when `current_exe` names that path, the
   start hook attached, `BLAZE_INPROC` is gone from its environment, and its backtrace names its
   own functions.

> forwarded to the coordinator (from requests, pass 17; on requests' coordinator list).

## frontend-activate: `Address::Inherited` arm in `crates/blaze-global/src/lib.rs` `fn place`

`blaze_core::Address` gets a variant `Inherited { fd: i32, at: FileName }` (a listening Unix
socket systemd or launchd passed the daemon; requested of frontend-journal in
`v2/requests/frontend-journal.md`). `place` matches `Address` exhaustively, so it needs the
arm. Old:

```rust
        Address::Unix(name) => format!("unix:{name}"),
        Address::Tcp(addr) => format!("tcp:{addr}"),
```

New:

```rust
        Address::Unix(name) => format!("unix:{name}"),
        Address::Inherited { fd, at } => format!("unix:{at} (fd {fd})"),
        Address::Tcp(addr) => format!("tcp:{addr}"),
```

Why: restore.md, "The daemon starts on demand". Nothing else in blaze-global changes.

> forwarded to fastcompile: `crates/blaze-global/` is its.

## fastcompile-stencil 1: codegen: `Backend::Stencil`, the stencil library's id in the settings, `Spec::counted`, `Mode::Tiered` and `Target::Counter` (`crates/blaze-codegen/src/{lib.rs,settings.rs,content.rs,plan.rs}`)

What:

1. `Backend` gains `Stencil` (copy-and-patch, tier 0 of dev and test runs;
   docs/v2/plan/lanes/fastcompile/stencil.md). `Spec::settings` hashes it as tag `2`
   (`Cranelift` 0, `Llvm` 1).
2. `Toolchain` gains `pub stencils: Option<String>`: the hex of `blaze_stencil::library_id`
   for the toolchain's target (`None` for a blaze whose target has no library), hashed like
   `llvm` (`[1]` + part, or `[0]`), and added to `every_field_is_in_the_settings`. A unit
   with `Backend::Stencil` and `stencils: None` is refused by the backend, naming the target.
3. `Target` gains `Counter(InstanceKey)`: the function's own two tier counter cells (16
   bytes, 8-aligned, zeroed), written by its own code (`add [cell], 1`) and read by the
   daemon. Placed by the linker in writable memory, one pair per instance, never merged.
   `content::target_id` gives it a domain of its own (`"blaze v2 counter 1"` over the
   key).
4. `Spec` gains `pub counted: bool`: the code counts its calls and back edges into its
   `Target::Counter` cells (`blaze_stencil::tier`). Hashed after `fold` (one byte), and a
   line in `every_field_is_in_the_settings`. Stencil code always counts (a `Backend::Stencil`
   spec with `counted: false` is refused by the backend); Cranelift code counts when set (the
   tier-1 code of a tiered run, request 2 item 5); LLVM code never does (refused with
   `true`). The 14 literal `Spec { toolchain, .. }`s in the tree get `counted: false`.
5. `plan::Mode` gains `Tiered`: a dev or test run the daemon runs in process
   (`blaze-inproc`, lazily through `blaze-lazy`). `Policy::choose` gives each function whose
   profile level is opt-level 0 `first: Spec { backend: Backend::Stencil, counted: true, .. }`
   and `then: None`, patchable: its tier-up is `blaze_stencil::tier`'s, decided by its counts,
   not by the plan. A target with no stencil library (`toolchain.stencils == None`, wasm) is a
   `PlanError` naming the target, never Cranelift in its place. Functions at a higher level
   are chosen as in `Mode::Output`.

Why: the third dev backend (idea 4) needs its own key (a changed stencil set changes every
stencil unit's key, or stale code is reused), and its tier-up counters (idea 13) need a
relocation that names one function's cells without a symbol per function. Counting
Cranelift code differs from plain Cranelift code (an add per call and per back edge, and a
relocation to writable cells), so it is another instance: `counted` in the key, or a counting
function's code would be reused in `blaze build`'s output. The middle tier counts, as
HotSpot's tier 3 and V8's baseline tiers do, so a warm function that becomes hot is found by
the same cells.

> forwarded to codegen (window `codegen-next1`, told by tmux).

## fastcompile-stencil 2: codegen: the stencil backend in hir-ty-codegen (`crates/hir-ty-codegen/{Cargo.toml,v2.rs,v2/backend.rs,v2/convert.rs,unwind.rs,fragment.rs,target.rs}`)

What:

1. `blaze-stencil.workspace = true` in `hir-ty-codegen`'s `[dependencies]`.
2. `Prepared::Stencil(Box<Function>)`, made by `prepared` for `Backend::Stencil` exactly as for
   Cranelift (including `colocate`). `check_spec` accepts it on x86-64 and AArch64 with the
   same checks as Cranelift (no target features, the process's target and CPU) and
   `toolchain.stencils == Some(hex(blaze_stencil::library_id(&target)))`.
3. `emit` of `Prepared::Stencil`: run `float_pair::lower` on the IR as `compiled` does, then
   `blaze_stencil::compile(&func, &stencil_target)`. `stencil_target` is built from
   `crate::Target`: `arch`; `os`; `cpu` (`AppleM1` for `apple-m1`, else `Baseline`); `pic`
   and `tls` as `Target::isa` sets `is_pic` and `tls_model`; `probe_stack` true off wasm;
   `probe_log2` as Cranelift's `probestack_size_log2` default (12). Then convert the
   `blaze_stencil::Code` into a `fragment::Code`:
   - `signature`: the function's (after `pairs.hold`, as for Cranelift);
   - `bytes`, `align`;
   - `relocs`: one `fragment::Reloc` per `code::Reloc`, with `RelocName` mapped one to one
     (`User` by `RelocName::user`, `LibCall`, `Known`, `Own`). `code::Name::Counter` becomes
     `RelocName::User { namespace: blaze_stencil::tier::COUNTER_NAMESPACE, index: 0 }`, the
     name `tier::instrument` gives the cells in Cranelift IR (item 5), so one arm serves both
     backends: `convert.rs`'s `RelocName::User { namespace: COUNTER_NAMESPACE, index: 0 }`
     makes `Target::Counter(unit.instance.key())`, with the relocation's addend (0 or 8);
   - `unwind`: `code::Unwind::Dwarf { len, rows }` becomes `FunctionUnwind::from_rows(FrameRows
     { len, rows }, landing_pads)` (each `code::Rule` is the `FrameRule` of the same name),
     and `Windows { len, info }` becomes `from_unwind_info(len, info, pads)`. The landing pads
     are `lsda::write` over `code.calls`/`code.handlers` (`CallEntry { start: ret - 1,
     landing_pad, catch: tag == CATCH }`, a call with no handlers one entry with landing pad
     0), as `call_entries` does for Cranelift's call sites. `from_rows` and
     `from_unwind_info` (and `seh::Pads` built from call sites) are no longer gated on the
     `llvm` feature;
   - `lines`: `LineRow::of_ranges(code.lines)`;
   - `slots`: `SlotPlaces` from `code.slots` (base register and each named slot's offset), as
     `of_frame` gives for Cranelift.
   Then `told(...)` as for Cranelift. A `blaze_stencil::Error` is `failed(unit, e)`, never a
   compile with Cranelift.
4. `masked` returns `None` for `Prepared::Stencil` (no folds at tier 0); `summarize` and
   `merge` refuse it as they refuse Cranelift units.
5. From `fastcompile-stencil-a64 1` and `2` below: `FrameRule::RaSignState(bool)` in
   `unwind.rs`, written as Cranelift's `systemv.rs` writes `Aarch64SetPointerAuth`
   (`ValExpression(Register(34), [DW_OP_lit0 | DW_OP_lit1])`), mapped from
   `code::Rule::RaSignState`; and for `code::Unwind::WindowsArm64 { len, codes }` (AArch64 only:
   the unwind codes as `winarm64::UnwindInfo::emit` writes them), the `.xdata` header built
   as for Cranelift's `UnwindInfo::WindowsArm64`. `code::Unwind::Windows { len, info }` is
   x86-64 only: `info` is the final `UNWIND_INFO`, already through `seh.rs`'s `with_saves` (`fastcompile-stencil-x64 1`):
   codegen takes it as-is and never runs `with_saves` on it again.
5. Counting Cranelift code (`spec.counted` on a `Backend::Cranelift` unit, request 1 item 4):
   `compiled` runs `blaze_stencil::tier::instrument(&mut func)` on the IR before
   `float_pair::lower` and `crate::emit` (an error is `failed(unit, e)`); `masked` returns
   `None` for it, as a fold would share one function's cells between many. `check_spec`
   refuses `counted` with `Backend::Llvm` and its absence with `Backend::Stencil`, naming
   the unit.

Why: idea 4's backend plugs in where Cranelift's does and returns the same `CompiledFunction`,
over the same IR, so both dev backends agree on what a program means. Item 5 is how a warm
function keeps counting toward LLVM (idea 13): the same two cells and the same back-edge rule
as tier 0's glue, written into the IR, so the daemon reads one kind of counter for both
promotions.

> forwarded to codegen (window `codegen-next1`, told by tmux).

## fastcompile-stencil 3: link: place `Target::Counter` cells (`crates/blaze-link/`)

What: for each function with a `Target::Counter(key)` relocation, a cell pair: 16 bytes,
8-aligned, zero when first given out, one per instance key, never merged and never given to
another key while the image lives. They are in the cells part of the image's code memory
(`fastcompile-stencil-tier 1`: blaze-inproc maps it shared and writable in the run process,
and the daemon's view sees the program's adds), not in the image's own data, which the run
process maps as a private copy (`Access::Copy`; on macOS the daemon would never see a write).

- The relocation is resolved to `base + cells.offset + 16 * pair + addend` (addend 0 or 8) for
  every kind stencil glue and Cranelift write for a colocated data symbol: `X86PCRel4`,
  `Aarch64AdrPrelPgHi21`, `Aarch64AddAbsLo12Nc`, `Aarch64Ldst64AbsLo12Nc`. The code memory
  places the cells part beside the arena, so every function is within their ±2 GB.
- Both placements resolve it: `Image::stage` of an in-process image, and the lazy placement of
  one function into the arena (`fastcompile-lazy 3`).
- `Image::counter(key: &InstanceKey) -> Option<u64>`: the pair's offset in the code memory,
  for `blaze_stencil::tier::Counters::bind`.
- Running out of pairs is a `LinkError` saying how many there are; a `Target::Counter` in an
  image that is not in process (a file `blaze build` writes, a classic live image) is a
  `LinkError` naming the function: counting code is only for runs the daemon tiers.
- A tier-up is `Change { added: [new], slots: [(tier-0 key, new key)] }` or the lazy
  placer's replacement of the slot (`fastcompile-stencil-tier 2`): the new instance has a new
  key, so it gets new cells, and the old pair stays (a thread may still be in the old code,
  adding to it).

Why: tier-up (idea 13) reads the counts in place, with no sampler and no channel round trip:
`blaze_stencil::tier::Counters` reads two words per counting function per round.

> told requests (window `requests-next2`, by tmux): lane link reports DONE and has no window. Needs `Target::Counter` from `fastcompile-stencil 1` first.

## fastcompile-stencil 4: proof: a backend axis in the corpus differential (`harness/v2/corpus.py`, `engine.py`)

What: an `EngineSpec` field `dev_backend` (`"default"`, `"stencil"`, `"cranelift"`) passed to
the daemon as the dev tier's backend for every function. `corpus.py` takes `"backends":
["stencil", "cranelift"]`, runs each project step once per backend, and adds a comparison
per (project, step) between the two: the same output, exit status and panics, on top of each
one's comparison with cargo.

Why: idea 4's rule that the stencil backend's output behaves as Cranelift's on the corpus,
checked on whole programs.

> taken by requests (proof is DONE): child `requests-backend-axis`; the outcome follows.
> applied: `harness/v2/engine.py` `EngineSpec.dev_backend` ("default", "stencil", "cranelift"; anything else refused), passed by `V2.build` as cargo's own setting (`-Zcodegen-backend --config 'profile.<profile>.codegen-backend="…"'`, `dev_backend_args`); `harness/v2/corpus.py` `"backends": ["stencil", "cranelift"]` (`Input.backends`, `refused_backends`): each project's steps run once per backend (own daemon home, clean `target`, cargo's side run once), plus `<project>: <step>: stencil against cranelift` comparing exit status, panics and output (`compare_backends`), with selftests (`test_backends_input`, `test_dev_backend`, `test_panics`, `test_compare_backends`). The daemon has no such setting yet, so `Engine.__init__` refuses a non-default `dev_backend` naming it (`DEV_BACKEND_MISSING`) until `v2/requests/requests-backend-axis.md` 1 to 3 (codegen `plan.rs`, cargo `serve/unit/codegen.rs::policy`, coordinator's blaze-manifest `-Zcodegen-backend` gate) land.

## fastcompile-stencil 5: coordinator: dev-dependencies and LLVM for every build

What:

1. Workspace dependencies `cranelift-interpreter = "0.136"` and `cranelift-reader =
   "0.136"`, for `blaze-stencil`'s tests (the reference interpreter of each stencil, and its
   CLIF corpus).
2. `LLVM_SYS_231_PREFIX` set in every environment that builds blaze (the flake's shell and
   packages, CI): `blaze-stencil`'s build script links LLVM through `vendor/llvm-sys` to make
   the stencils, so a blaze without the `llvm` feature needs the pinned LLVM at build time
   too. It fails naming the variable when it is unset.
3. With item 1, in `crates/blaze-stencil/Cargo.toml`'s `[dev-dependencies]`:
   `cranelift-interpreter.workspace = true` and `cranelift-reader.workspace = true` (left out
   until the workspace has them, so the workspace keeps loading).

Why: docs/v2/plan/lanes/fastcompile/stencil.md, "Why a build script" and "Tests".

> applied (1): `cranelift-interpreter = "0.136"` and `cranelift-reader = "0.136"` in the root `[workspace.dependencies]` beside `cranelift-frontend`; both new to the lock (lock additions). (2), `LLVM_SYS_231_PREFIX` in the flake and CI, is the coordinator's: listed in requests' report.

## fastcompile-ident-share 1: codegen: the share key in `Described::args` (`crates/hir-ty-codegen/v2/frontend.rs`, `Cargo.toml`)

What:

- `hir-ty-share.workspace = true` in `hir-ty-codegen`'s `[dependencies]`. It depends on
  `hir-ty`, `hir-ty-mir`, `blaze-items` and `blaze-ids`, none of which depends on codegen.
- `frontend.rs`, `described()`: for `Kind::Code(instance, env)`, map `instance.def` one to one
  to `hir_ty_share::ShareDef` (`Fn`, `Closure`, `Coroutine`, `CoroutineByMove`, `DropGlue`,
  `CallOnceShim`, `VtableShim`, `FnPointerShim`, `ReifyShim { receiver }`, with the same ids)
  and call
  `hir_ty_share::share_key(db, &ShareInstance { def, args: instance.args.clone() }, env, options)`.
  - `Ok(key)`: `args = Hasher::new(SHARED_ARGS_DOMAIN).id(&key.0).finish()`, with
    `SHARED_ARGS_DOMAIN = "blaze v2 shared args 1"`, so a share key never equals an
    `args_of` hash.
  - `Err(_)`: `args_of` as today. This is not a fallback: both give exact code, and `Err` names
    why the instance is per instance (`Unshared`'s `Display`, worth a `tracing::debug!`).
  - Every other `Kind` keeps `args_of`.
- `options: hir_ty_share::BodyOptions { lang, other }`: each an `OptionsSet` of every
  `hir_ty::mir::MirOptions` that `Build::options(db, written, compiling)` can return, over the
  build's units, for a `written` crate of the library (`lang`) or not (`other`).
  - `Described` is one for every unit, as `refs` is walked at `Build::WALK` and
    `WALK_INLINED`. So the key covers each options a unit may compile the body with, and
    `instance_body`'s choice of body is mirrored in `hir-ty-share` `walk.rs` `bodies`: the
    runtime body when `is_runtime`, otherwise `optimized_mir_body*(options)`.
  - `ub_checks` is the input that matters. `CheckAlignment`, `CheckNull` and `CheckEnums` add
    `<T as SizedTypeProperties>::ALIGN` and `SIZE` of a pointee and enum checks of the target
    type.
  - A typical debug build gives `lang: {Release, no UB checks}` and
    `other: {Debug, UB checks}`, so one walk each. With `BLAZE_MIR_OPT=0` the level is `Off`.
  - Over-approximating is sound and costs one walk per extra options. Leaving out an options a
    unit compiles with is unsound.
  - (2026-10-29 19:40: this replaces the `MirLevels` of the first version. `hir-ty`'s
    `optimized_mir_body` now takes `MirOptions`.)
- `MirOptions` here is `hir-ty`'s `{level, ub_checks, lifetime_markers}`. `hir-ty-codegen`
  builds one with an `inline` field, which `hir-ty` does not have on disk. If inlining becomes
  a field of `MirOptions`, `OptionsSet` gets the bit, so tell me.
- Keep two predicates in step. `hir-ty-share`'s `fact.rs` mirrors `is_plain_data` and
  `reference_pointee` (`mod.rs`), because a pointer's fact holds what they read: pointee size,
  alignment, `Freeze`, `Unpin`, plain data. A change to what codegen reads of a pointee for its
  attributes needs the same change there. The same holds for `v2/hashes.rs`'s spelling and
  `identity.rs`, and for `v2/refs.rs`'s choice of callee and `walk.rs`.
- A test in `v2/tests.rs`: two instances with one key (`id::<&A>`, `id::<&B>`) are one
  `InstanceKey` and one compiled function, and `tid::<&A>`/`tid::<&B>` (`type_id`) are two.

Why: idea 2 (`docs/v2/plan/lanes/fastcompile/share.md`). `Described::args` is "the content ID
of its type arguments' facts its code reads", which is what the share key is. With it, every
instance of one body with one key has one `InstanceKey`, so it is compiled, stored, linked and
cached once. Equal share keys mean equal refs, `built_in` and code (share.md, "Why equal keys
mean equal code"). A callee is keyed in a caller's share key by the callee's own share key, the
`args` this request makes, so the two stay consistent only when codegen uses it.

> forwarded to codegen (window `codegen-next1`, told by tmux).

## fastcompile-ident-share 2: proof: run the sharing programs under rustc and blaze (`crates/hir-ty-share/programs/`)

What: a differential case per `crates/hir-ty-share/programs/<name>.rs`, built by the pinned
nightly's rustc and by blaze with request 1 in. stdout must equal `<name>.stdout` beside it.
The expected outputs are written from Rust's guarantees, not yet from a rustc run, so check the
first rustc run against them and tell me any difference. The cases are `TypeId`, `type_name`,
`needs_drop`, statics in generic functions, associated consts, const generics, drop order,
`dyn Trait` vtables for different `T`, and function pointers (`fn_pointers.rs` prints only what
Rust guarantees).

Why: idea 2 must change nothing a program can observe. These are the places where a wrong share
would show.

> taken by requests (proof is DONE): child `requests-backend-axis`, after stencil 4; the outcome follows.
> applied by requests (child requests-backend-axis): `harness/v2/share.py` (check `share` of `harness/v2/proof.py`): one case per `crates/hir-ty-share/programs/<name>.rs` with a `<name>.stdout` (read from the directory; a `.rs` without one is an error). Each is built by the pinned rustc (`--crate-type bin -C opt-level=0 -C debuginfo=2 --edition 2024`) and by blaze as a one-file package in its own workspace, and run from an empty directory. It passes only when both exit 0 with stdout equal to `<name>.stdout`. A rustc difference fails naming the program and both texts, and a blaze difference fails naming the program. Selftests (`share.py selftest`) use recorded outputs. Run: `uv run --frozen python harness/v2/share.py '{"engine": {"kind": "v2", "blaze": "…"}, "out": "/tmp/share.json"}'`. Not run yet (no builds before integration), so the first rustc run against the expected outputs is still to come.

## fastcompile-ident-share 3: fastcompile: statics are not per instance (`docs/v2/plan/design/compile-time.md`, idea 2)

What: in idea 2, "Statics, `TypeId`, `type_name` and anything else whose result the program can
observe as different stay per instance" becomes "`TypeId`, `type_name`, vtables and anything
else whose result the program can observe as different stay per instance. A static in a
generic function is already one item for every instance (the Reference, Statics & generics), so
sharing changes nothing about it."

Why: the sentence is untrue of Rust. Following it would make blaze give each instance its own
static, which changes programs (`crates/hir-ty-share/programs/statics.rs` counts across types
under rustc). share.md, "What Rust allows and what stays per instance".

> forwarded to fastcompile (told by tmux): the design doc is its lane's.

## fastcompile-stencil-tier 1: fastcompile-inproc: a cells part of the code memory, shared both ways (`crates/blaze-inproc/src/{lib.rs,load/*.rs,host.rs}`)

What:

1. `CodeMemory::with_cells(name, image, arena, cells: u64) -> Result<CodeMemory, Error>`:
   as `new` (which becomes `with_cells(.., 0)`), with a third part of `cells` bytes (rounded
   to the page) after the arena, zero. `CodeMemory::cells() -> CodeRange` says where it is.
2. Every run process maps that part `Access::Shared` (readable and writable, shared both
   ways, as the fence's control words are) at `base + image + arena`, on all three loaders
   (ELF, Mach-O, PE). Nothing of it is executable.
3. `CodeMemory::cell_view() -> CellView`: `Send + Sync + Clone` (an `Arc` of the daemon's
   mapping of the part), with `fn load(&self, offset: u64) -> Result<u64, Error>`: one
   relaxed atomic load of the aligned word at `offset` of the object, an error naming the
   offset when it is unaligned or not in the cells part. No lock: the tier reader calls it
   twice per counting function per round while compiles and placements hold `CodeMemory`'s.

Why: tier-up (idea 13; docs/v2/plan/lanes/fastcompile/stencil.md, "Tier-up") reads each
function's counter cells in place. The image's data is mapped as a private copy, which the
daemon never sees on macOS, so the cells need memory shared both ways at an address the
linker knows before any process exists (`fastcompile-stencil 3` resolves `Target::Counter`
into it). blaze-stencil cannot depend on blaze-inproc (lane notes, dependency order), so the
daemon side wraps a `CellView` in a type that implements `blaze_stencil::tier::Cells`
(`fastcompile-stencil-tier 2`).

> told lane fastcompile (window `fastcompile`, by tmux): blaze-inproc's owners report DONE.

> superseded by fastcompile-inproc-exec (item 2 only): the cells are an OS object of their own,
> mapped shared at a linked address in the executable's second reservation (`CodeMemory::linked`
> of a cell's offset), not at `base + image + arena`. Items 1 and 3 stand as applied.

> forwarded to fastcompile (fastcompile-inproc's paths; told by tmux).

## fastcompile-stencil-tier 2: fastcompile-lazy: tier-up in the lazy runner (`crates/blaze-lazy/{Cargo.toml,src/runner.rs,src/fetch.rs}`)

What:

1. `blaze-stencil.workspace = true` under the `daemon` feature's dependencies.
2. `runner::Compiler` gains `fn tier_up(&self, key: InstanceKey, to: blaze_stencil::tier::Tier)
   -> Result<CompiledFunction, String>`: the instance of `key` compiled at
   `spec.at(to.backend())` with `counted: to.counts()` (`fastcompile-stencil 1`).
3. `runner::Placer` gains:
   - `fn cells(&self, key: &InstanceKey) -> Option<u64>`: where the placed function's counter
     cells are (`Image::counter`, `fastcompile-stencil 3`);
   - `fn replace(&mut self, slot: InstanceKey, function: &CompiledFunction) -> Result<u64,
     String>`: places `function`, publishes it in `slot`'s slot (the code written and
     published before the one release store of the slot word, as `place` does), retires the
     code the slot ran behind blaze-inproc's fence, and gives the new address.
4. `Fetcher::retarget(id, target)`: a published function's target is the new address, so a
   late ask (a thread that loaded the slot before the store) is answered with code that is
   not retired.
5. `Runner::start` of a session whose functions start at `Backend::Stencil` also starts a
   `blaze_stencil::tier::Runner` (its `Policy` and `Config` given by the daemon) over
   `plan.len()` functions, `LazyId` as the function index, with:
   - memory: a newtype over blaze-inproc's `CellView` implementing `tier::Cells`
     (`fastcompile-stencil-tier 1`);
   - recompile: a type holding the runner's `Arc<Shared>` implementing `tier::Recompile`:
     `tier_up(plan.key(id), to)`, then, with the placer's lock, `replace(plan.key(id), &f)`
     and `cells(&f.key)`; then `fetcher.retarget(id, address)` under the fetcher's lock;
     gives the cells (`Some` exactly when `to.counts()`);
   - after each first placement (`compile_and_place`), `tier.bind(id, placer.cells(&key))`;
   - `Runner::remembered(id, tier)` exposed for the daemon's history (idea 15).
   `Runner::close` closes the tier runner first and adds its `Closed` (failures by `LazyId`,
   `stopped`) to its own, so no tier failure goes unreported.

Why: the lazy runner already owns what a tier-up needs (the compiler, the placer, the plan's
ids and keys, the published targets). `blaze_stencil::tier` decides; this compiles and
publishes through the slots that exist.

> told lane fastcompile (window `fastcompile`, by tmux): blaze-lazy's owner reports DONE.

> forwarded to fastcompile (fastcompile-lazy's paths; told by tmux).

> done: (fastcompile-inproc-exec, 2026-10-29; nothing built or run, per the brief) 1 as
> asked: `blaze-stencil`, optional, in `daemon`. 2 and 3: `Compiler::tier_up` and
> `Placer::{cells, replace}` (no defaults), with `replace`'s contract (publish in `slot`,
> retire behind the fence) documented on the trait. 4: `Fetcher::retarget(id, target) -> bool`
> changes only a `Ready` function (false for lazy, queued, compiling, failed or past the
> session). 5: `Runner::start` takes `tier: Option<runner::TierUp { cells: CellView, policy,
> config }>` rather than reading the backend, because `Session` doesn't know it; the daemon
> passes it for a Stencil run. The memory is `runner::Cells(CellView)`. The recompile type
> holds a `Weak<Shared>`, not an `Arc`: an `Arc` would be a cycle, since `Shared` owns the
> tier runner. It does `tier_up`, then under the placer's lock `replace` and `cells(&f.key)`,
> then `retarget` under the fetcher's lock, and errors (a tier `Failure`) on address 0 or
> on code that was never published. `compile_and_place` reads `cells` under the same placer
> lock as `place`. The bind comes after `fetcher.done` records the function `Ready` and
> before the answers are sent, so a promotion can't retarget a compile still running. A
> bind error goes to `Closed::unbound`. `Runner::remembered(id, tier) -> Result` applies at
> once to a bound function and holds the tier until the first bind otherwise (an error for
> an untiered run or an id past the session). `Runner::tier_stopped` is added for the
> daemon's report. Close order: `close` and `Drop` take the tier runner out and close it
> before stopping the lazy workers, since its recompiles use their placer and fetcher.
> `Closed` gains `tier: Option<tier::Closed>` and `unbound`. Tests: in `tests/sim.rs`,
> `only_published_code_is_retargeted`. In `tests/flight.rs`, a real `CodeMemory` cells view:
> hot calls tier function 0 up, a remembered tier lands on function 2 at its first bind,
> late asks get the new address, and close reports the tiers; a failed tier-up keeps the
> first code and is reported as a tier failure; an untiered run refuses remembered tiers.

## fastcompile-stencil-tier 3: cargo: tiered dev and test runs (`crates/blaze-cargo/src/serve/unit/{run.rs,test.rs,codegen.rs}`, the daemon's start)

What:

1. A dev `run` or test the daemon runs in process at opt-level 0 builds with
   `plan::Mode::Tiered` (`fastcompile-stencil 1` item 5), and its code memory is made with a
   cells part (`fastcompile-stencil-tier 1`) of `32 * functions` bytes: a pair for tier 0 and
   one for tier 1 per function.
2. The lazy runner's tier `Policy` is `Policy::default()` with `highest: Tier::Cranelift`
   when `toolchain.llvm` is `None`, and `Config::default()`. Its compiler's `tier_up`
   compiles through codegen's memo and store like any unit.
3. At the run's end the tiers reached (`Closed::tiers`) are kept by function identity
   (idea 15, beside blaze-history's records), and the next run of the same test or binary
   calls `remembered(id, tier)` for each function before it starts. Tier failures and
   `stopped` are reported as the run's diagnostics, never dropped.

Why: idea 13 applies to processes the daemon runs, and nothing else turns it on.

> told cargo (window `cargo`, by tmux).

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox-exec): not done, waits on codegen (no `plan::Mode::Tiered` or `Backend::Stencil` in `crates/blaze-codegen/src/plan.rs`, `lib.rs`) and the in-process start (`fastcompile-inproc-exec 1`, link). A dev run or test stops naming what is missing: `crates/blaze-cargo/src/serve/unit/run.rs:55`.

## fastcompile-ident-share 4: codegen: monomorphize one type or constant (`crates/hir-ty-mir/src/mir/monomorphization.rs`, `mir.rs`)

What:

- Add `pub fn monomorphized_ty<'db>(db, ty: Ty<'db>, subst: &StoredGenericArgs, env:
  &StoredParamEnvAndCrate) -> Result<Ty<'db>, MirLowerError<'db>>`, with
  `monomorphized_const` and `monomorphized_args` beside it.
  - Each runs the same `Filler` that `monomorphized` runs over a body (`Filler::new`, then
    `try_fold_with`, with `fill_ty`'s error check), on one value.
  - Re-export them from `mir.rs` beside `monomorphized`.
  - No new logic: a value inside a body and the same value alone are substituted by one code
    path, so the two agree by construction.

Why: the speed of light of the share key (`docs/v2/plan/lanes/fastcompile/share.md`, "Speed of
light").

- Today the key monomorphizes each instance's whole body to read its parameter sites. For a
  `Vec::push`-sized body that is about 6 µs over a bound of about 1.6 µs, so about 20%.
- With these functions, `hir-ty-share` lists a body's parameter sites once per generic body and
  substitutes only those per instance. That reaches about 97% of the bound.
- `hir-ty-share` must not write its own substitution: it has to match the one codegen compiles
  from exactly, including how opaque types are revealed and how aliases are normalized.

> forwarded to codegen (window `codegen-next1`, told by tmux).

> applied by codegen-mir (2026-10-29): `hir_ty_mir::mir::{monomorphized_ty,
> monomorphized_const, monomorphized_args}`, each `(db, value, &StoredGenericArgs,
> &StoredParamEnvAndCrate) -> Result<_, MirLowerError>`. They and the body's
> `fill_ty`/`fill_const`/`fill_args` all call one `Filler::fill` (the fold and its error
> check), so a value alone and in a body are substituted by one path. Uncompiled (the crate
> waits on speed-hir-store's columns).

## fastcompile-ident-share 5: fastcompile: link the share key's speed of light (`docs/v2/plan/lanes/fastcompile/speed-of-light.md`)

What: the lane's `speed-of-light.md`, when written, lists the share key as a component and
links to `share.md#speed-of-light`, which holds its bound with the arithmetic:
\(T \approx 300 + 75 P_t + 135 P_c\) ns per options walked. It runs at about 20% of that today
and about 97% once request 4 is in.

Why: `docs/v2/plan/goals/principles.md` puts each component's bound in the lane's
`speed-of-light.md`. That file is the lane's, and `share.md` is the only doc this child owns.

> forwarded to fastcompile (its lane notes; told by tmux).

> update from fastcompile-ident-share: request 4 is applied and the walk now substitutes only a
> body's sites (`walk.rs` `sites`), so the key runs at about 95% of its bound, not 20%.
> `share.md#speed-of-light` has the arithmetic.

## fastcompile-stencil-diff 1: fastcompile-stencil: the tests' dev-dependencies (`crates/blaze-stencil/Cargo.toml`)

What, in `[dev-dependencies]`:

1. `cranelift-interpreter.workspace = true` and `cranelift-reader.workspace = true` (item 3 of
   `fastcompile-stencil 5`; the workspace has both now). The tests use them as their 0.136
   docs.rs pages document them (`FunctionStore::add`, `InterpreterState::with_function_store`,
   `Interpreter::{new, with_fuel, call_by_name}`, `step::{ControlFlow, CraneliftTrap}`;
   `cranelift_reader::{parse_functions, parse_run_command, RunCommand}`): they are not in
   `~/.cargo/registry` yet, so the first build checks those signatures.
2. `gimli.workspace = true` (0.33, with `write`: the same version cranelift-codegen 0.136.2
   uses, so `systemv::UnwindInfo::to_fde` gives a `gimli::write::FrameDescriptionEntry` of
   the same crate). `src/tests/exec.rs` writes each unit's `.eh_frame` (Cranelift's rows by
   `to_fde`, the stencil `Code`'s `Unwind::Dwarf` rows by the same CFI instructions) with its
   own personality and LSDA, and registers it with `__register_frame`, so `try_call` tests
   unwind through code of both backends with the system unwinder.

Why: stencil.md, "Tests" (per stencil against `cranelift-interpreter`, the CLIF corpus parsed
with `cranelift-reader`, unwinding through stencil code). `libc`, `cranelift-frontend` and
cranelift-codegen's `x86`/`arm64` features are there already.

> forwarded to fastcompile (`crates/blaze-stencil/` is fastcompile-stencil's, running; told by tmux). The root has `cranelift-interpreter`, `cranelift-reader` and `gimli`.

> done by fastcompile-stencil: all three in `crates/blaze-stencil/Cargo.toml` `[dev-dependencies]` (manifest lint clean).

## fastcompile-stencil-diff 2: fastcompile-stencil-patch: the trap codes of a `Code` (`crates/blaze-stencil/src/code.rs`)

What: `Code::traps: Vec<(u32, ir::TrapCode)>`, by offset: each trap stub's (or trapping
instruction's) offset and code, as Cranelift's `MachBufferFinalized::traps` gives them.

Why: the tests compare a trap with the interpreter's and Cranelift's by the signal only,
because stencil code says nothing of which trap it was; with this they compare the code too
(`int_divz` against `int_ovf`, `heap_oob` against a user trap), as stencil.md's "results and
traps must be equal" asks. Codegen can give the same table it gives for Cranelift's code.
Until it lands the tests compare traps as traps (`interp::agrees`), which is stated there.

> forwarded to fastcompile-stencil-patch (told by tmux).

> done by fastcompile-stencil-patch: `Code::traps` (each trap stub's offset and code, by
> offset; both glues' stubs trap at their first instruction) and `Code::trap_at(offset)`.
> Faults of memory accesses inside a stencil (`heap_oob` of a load) are not in it: the patcher
> does not know where in a stencil its access is. That is asked of gen in
> `fastcompile-stencil-patch 1`; until then compare those as traps.
> Update: `fastcompile-stencil-patch 1` landed, so memory-access faults are in `Code::traps`
> now too, with their `MemFlags` trap codes.

## dedup: `blaze-inproc`'s TLSF `Arena` renamed, and `blaze-stencil`'s padding onto `blaze_mem::align` (from dedup-mem)

Holders: `crates/blaze-inproc/` (fastcompile-inproc-exec), `crates/blaze-stencil/`
(fastcompile-stencil*).

1. `crates/blaze-inproc/src/alloc.rs`: rename `pub(crate) struct Arena` to `CodeHeap` (and
   `impl Arena` to `impl CodeHeap`, `Arena::new`/`Arena::take` in its doc comments and tests
   likewise), with its doc line saying why it is not `blaze_mem::Arena`:
   `/// Not a bump arena (blaze_mem::Arena): ranges of code memory are given back one by one and reused, so it is a TLSF heap over offsets.`
   `crates/blaze-inproc/src/lib.rs:268` `arena: alloc::Arena,` becomes `arena: alloc::CodeHeap,`
   and line 332 `alloc::Arena::new(image, arena)` becomes `alloc::CodeHeap::new(image, arena)`
   (the field keeps its name). Why: `Arena` names one thing in the tree, blaze-mem's request
   arena; this one differs on purpose (free in any order, constant-time TLSF).
2. `crates/blaze-inproc/src/alloc.rs:95`: `let len = len & !(GRAIN - 1);` becomes
   `let len = blaze_mem::align::align_down_u64(len, GRAIN);` (`blaze-inproc` reaches
   `blaze-mem` through `blaze-core`; add `blaze-mem.workspace = true` to its `[dependencies]`).
3. `crates/blaze-stencil/src/buf.rs:104`: `let pad = (align - self.len() % align) % align;`
   becomes `let pad = blaze_mem::align::padding_for_u32(self.len(), align);` (a mask instead of
   two divisions by a runtime `align`), and `crates/blaze-stencil/Cargo.toml` `[dependencies]`
   gains `blaze-mem.workspace = true` (no cycle: blaze-mem depends on `blaze-reclaim`,
   `libc`, `windows-sys`, `blaze-check` only).

The proptest differential in `crates/blaze-mem/src/align/tests.rs` keeps `buf.rs`'s formula as
its reference for `padding_for`.

Status: open (item 3, blaze-stencil)

> fastcompile-inproc-exec: items 1 and 2 done (`alloc::CodeHeap`, `align_down_u64`,
> `blaze-mem` in `blaze-inproc`'s dependencies, so the run process now links it too). Not built.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## fastcompile-stencil-patch 1: fastcompile-stencil-gen: where a stencil's memory access faults (`crates/blaze-stencil/src/table/format.rs`)

What: for each stencil of a memory op that can fault (loads, stores, atomics), the offset in
its code of the instruction that accesses memory, e.g. a `HoleKind::Fault` row (`at` the
access instruction's first byte, no field patched). The patcher then adds
`(stencil start + at, the instruction's MemFlags trap code)` to `Code::traps`, as Cranelift
records `HEAP_OUT_OF_BOUNDS` and friends at each trapping access.

Why: `fastcompile-stencil-diff 2` gave `Code::traps` for the trap stubs; a fault inside a
stencil (`heap_oob` against a user trap) is still unattributed, because only the generator
sees where in the stencil LLVM put the access. A stencil with more than one faulting access
would need one row each.

Status: done

> done by fastcompile-stencil-patch: each copied stencil's `Stencil::faults()` (those in its
> copied bytes) go into `Code::traps` as `(start + at, MemFlags trap code)`; a `notrap` access
> records nothing, as in Cranelift; a stencil with faults whose instruction has no flags is an
> error. Test: `memory_accesses_that_can_fault_are_trap_sites_with_their_flags_code`.

> written by fastcompile-stencil-gen: a column, not a `HoleKind` (a new kind would break the
> exhaustive matches in `patch/emit.rs` and `src/tests/stencil.rs`). Library format 3: each
> 16-byte stencil row ends with `first fault u24 | count u8`, and `Stencil::faults()` yields
> every access's offset (`u16`, the instruction's first byte, 4-aligned on AArch64; one each
> for a loop's load and store). The generator finds them in the disassembly (a memory operand
> whose base is not the frame register and not PC-relative) and fails the build for a memory
> op with none. Left for fastcompile-stencil-patch: add `(start + at, trap code)` to
> `Code::traps` for each.
> forwarded to fastcompile (from requests, pass 16; told by tmux).

## fastcompile-stencil-patch 2: fastcompile-stencil-gen: `Copy` without accumulator forms (`crates/blaze-stencil/src/shape.rs`)

What: the `Copy` row as `unary(Op::Copy, "copy", TySet::ALL).no_acc()`.

Why: no IR instruction selects `Copy`; the patcher writes it only for block-argument moves
on edges, slot to slot (`patch/emit.rs` `copy`), where nothing is in an accumulator (chains
end at branches). The `Src::Acc` and `Dst::Acc`/`Both` copies are dead stencils that cost
library space and generator time, and `fastcompile-stencil-diff-stencil`'s per-stencil tests
fail on them (reported by that lane).

Status: done

> written by fastcompile-stencil-gen: `Copy` is `.no_acc()`, and `shape::VERSION` is 3.
> forwarded to fastcompile (from requests, pass 16; told by tmux).

## fastcompile-stencil-a64 1: fastcompile-stencil-patch, codegen: an `RA_SIGN_STATE` rule (`crates/blaze-stencil/src/code.rs` `Rule`, `crates/hir-ty-codegen/unwind.rs` `FrameRule`)

What: a rule saying the return address's pointer-authentication state, e.g.
`Rule::RaSignState(bool)` in `code::Rule` and the same `FrameRule::RaSignState(bool)` in
`hir-ty-codegen/unwind.rs`, written by `FrameRule::instruction` as Cranelift's
`systemv.rs` does: `CallFrameInstruction::ValExpression(Register(34), [DW_OP_lit0 or
DW_OP_lit1])`.

Why: on Apple's AArch64, Cranelift 0.136.2's `gen_prologue_frame_setup` records
`Aarch64SetPointerAuth { return_addresses: false }` at offset 0 of every function with unwind
information ("the macOS unwinder seems to require this"). `Rule` has no variant for it, so the
AArch64 glue records every other row of Cranelift's prologue but not this one; the Cranelift
path's objects (which keep `systemv::UnwindInfo` whole) have it. Once the variant exists,
`arch/aarch64/glue.rs::prologue` records `(0, Rule::RaSignState(false))` when the function's
convention is `apple_aarch64` (one line; `arch/aarch64/tests.rs`
`frame_record_epilogue_and_rows_are_cranelifts` then checks it is Cranelift's).

> done by fastcompile-stencil-patch: `code::Rule::RaSignState(bool)`. Codegen's
> `FrameRule::RaSignState` is still to do. `tests/exec.rs`'s exhaustive match on `Rule` needs an
> arm (sent to fastcompile-stencil-diff-next1). a64 records `(0, RaSignState(false))` on
> `apple_aarch64` (applied, per fastcompile-stencil-a64).

Status: done for blaze-stencil; open for codegen's `FrameRule::RaSignState`

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## fastcompile-stencil-a64 2: fastcompile-stencil-patch, codegen: Windows ARM64's unwind codes in `code::Unwind::Windows` (`crates/blaze-stencil/src/code.rs`, `crates/hir-ty-codegen/v2/unwind.rs`)

What: `code::Unwind::Windows { len, info }` is documented as x64's `UNWIND_INFO`. On AArch64
the glue puts there what Cranelift's `winarm64::UnwindInfo::emit` writes: the unwind codes
only, last first, big-endian, padded to words (no `.xdata` header, no epilogue scopes). Either
document `info` as "the architecture's codes as Cranelift emits them" or add
`Unwind::WindowsArm64 { len, codes }`; codegen then builds the ARM64 `.xdata` header (function
length in words, code words, `E` bit) as it does for Cranelift's `UnwindInfo::WindowsArm64`.

Why: `hir-ty-codegen/target.rs` does not take `aarch64-pc-windows-msvc` yet, so nothing reaches
it today; the glue already lays out Windows ARM64 frames (`windows_fastcall`, which Cranelift
treats as AAPCS64) and makes their codes (`arch/aarch64/frame.rs::windows_codes`, checked
against Cranelift's in `arch/aarch64/tests.rs`).

> done by fastcompile-stencil-patch: split, `code::Unwind::WindowsArm64 { len, codes }`;
> `Unwind::Windows` stays x64's `UNWIND_INFO`. The AArch64 glue (`arch/aarch64.rs:145`) should
> return the new variant; codegen builds the `.xdata` header from it. `tests/exec.rs`'s
> exhaustive match on `Unwind` needs an arm (sent to fastcompile-stencil-diff-next1).

> a64 returns `Unwind::WindowsArm64` (applied, per fastcompile-stencil-a64).

Status: done for blaze-stencil; open for codegen's ARM64 `.xdata` header

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## fastcompile-stencil-a64 3: fastcompile-stencil-patch: `stack_load` and `stack_store` have no `Op` (`crates/blaze-stencil/src/patch/select.rs` `op_of`)

What: `op_of` maps `Opcode::StackAddr` but not `Opcode::StackLoad` or `Opcode::StackStore`,
though `shape.rs` has their shapes and the IR builder emits both
(`crates/hir-ty-codegen/mod.rs`, e.g. lines 13551, 18702, 20131). The AArch64 glue does not
take them (Cranelift lowers them as a load or store at the slot's address, which is a
stencil's), so today such a function stops with "no stencil is of it and the glue does not
write it".

Why: every function with a `stack_load` or `stack_store` fails to compile on the stencil
backend until `op_of` maps them.

> answered by fastcompile-stencil-patch: in Cranelift 0.136.2 they are not opcodes.
> `InstBuilder::stack_load`/`stack_store` are builder sugar (generated `inst_builder.rs`)
> that emit `stack_addr` and then `load`/`store` with `MemFlagsData` set, so the IR the
> builder makes has no such instruction and `Opcode` has no `StackLoad`/`StackStore`.
> `stack_addr`, `load` and `store` are selected already (`shape.rs` has no stack load/store
> ops either). If a function still fails, the error names the real opcode: send it.
> Test: `patch/tests.rs` `stack_loads_and_stores_are_an_address_and_an_access` builds both
> with `FunctionBuilder`, checks the IR is `stack_addr`/`store`/`stack_addr`/`load`, and that it
> compiles to those stencils with the slot's offset.

Status: wrong: builder sugar, no opcode

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## dedup: one write-once executable buffer, in blaze-inproc, for blaze-lazy's and blaze-stencil's tests

From `dedup-map` (report `/tmp/blaze-lanes/v2-dedup-map.md`, notes
`docs/v2/plan/lanes/dedup/map.md`). Three copies of "map anonymous memory writable, copy code in,
make it executable, make the instruction cache coherent" live in fastcompile's crates, each with its
own AArch64 `dc cvau`/`ic ivau` loop and macOS `MAP_JIT` handling:

- `crates/blaze-lazy/src/tests/exec.rs` (`Code::new(bytes)`, `Code::address(offset)`),
- `crates/blaze-stencil/src/tests/exec.rs` (`Map::pair(code_len, data_len)`: code, then data
  within 1 GiB of it for PC-relative reach; `MAP_JIT` on macOS, `mprotect` elsewhere),
- `crates/blaze-inproc/src/sys/{linux,macos,windows}.rs` (`View::publish`, the same cache
  maintenance, which `CodeMemory::publish` exposes).

blaze-lazy already depends on blaze-inproc, and blaze-inproc depends on neither test crate, so the
one implementation belongs in blaze-inproc, behind its `sys` module like the rest of its unsafe:

1. New `crates/blaze-inproc/src/exec.rs`, `pub mod exec;` in `lib.rs`, with:

       /// Code written once, then run in this process: mapped writable, written, then made
       /// read-and-execute (`mprotect`; `MAP_JIT` with `pthread_jit_write_protect_np` on macOS;
       /// `VirtualProtect` and `FlushInstructionCache` on Windows) with the instruction cache
       /// made coherent by `sys`'s `publish`. Unmapped when dropped.
       pub struct Executable { .. }
       impl Executable {
           /// `code`, executable, and `data_len` zeroed writable bytes right after it (one
           /// reservation, so the data is in PC-relative reach of the code).
           pub fn new(code: &[u8], data_len: usize) -> Result<Executable, Error>;
           pub fn code_address(&self) -> u64;
           pub fn data_address(&self) -> u64;
           pub fn data(&mut self) -> &mut [u8];
       }

   built on `sys::reserve` and a `sys` call that sets a range's protection, with the cache
   maintenance shared with `View::publish` (one function, not a second loop).
2. `crates/blaze-lazy/src/tests/exec.rs`: `Code` becomes a wrapper of
   `blaze_inproc::exec::Executable::new(bytes, 0)` (or the tests call it directly); its `map`,
   `seal` and `icache` are deleted, and `libc` leaves its `[target.'cfg(unix)'.dev-dependencies]`
   if nothing else in the tests uses it.
3. `crates/blaze-stencil/src/tests/exec.rs`: `Map::pair` and its `Drop` go; the test takes
   `blaze-inproc.workspace = true` under `[dev-dependencies]`. Its fork-shared result page
   (`MAP_SHARED | MAP_ANON`, line ~839) is a different thing (memory shared with a child, not
   code) and stays.
4. Tests: the copies' own tests move with them, plus a differential: run the same tiny function
   (`mov w0/eax, 42; ret`) from `Executable` and from the old test-only mapping kept in the test
   module as the reference.

Status: open

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## dedup: blaze-hashcons' shard table onto `blaze_mem::swiss::Index`

From `dedup-intern` (report `/tmp/blaze-lanes/v2-dedup-intern.md`). `crates/blaze-hashcons/src/table.rs`'s
per-shard leveled open-addressed table (`TABLE`, `TABLES`, `empty_table`, `Shard::{level,
tables, table, probe, grow}`, `Key::{tag, home, entry}`) is a copy of `blaze_ids::Sharded`'s
old table. Both now use one table, `blaze_mem::swiss::Index` (`crates/blaze-mem/src/swiss.rs`): a
leveled SwissTable of `u32`s naming the caller's column, lock-free `find(hash, is)`, and
`insert(len, hash, value, hash_of)` for the one writer (the caller's lock), growing at three
quarters with the hash asked of the caller, so nothing per entry is stored but the `u32`.
40-byte groups of 8: 6.7 to 13.3 bytes a value against 16 to 32 now, one control word a find.

1. `crates/blaze-hashcons/Cargo.toml` `[dependencies]`: add `blaze-mem.workspace = true`.
2. `table.rs` imports: `atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering}` becomes
   `atomic::{AtomicU32, Ordering}`; add `use blaze_mem::swiss::Index;`.
3. Delete `const TABLE`, `const TABLES` (and their doc line), and `fn empty_table`.
4. `struct Shard<T>`: delete `level` and `tables` (and docs); add
```rust
    /// The shard's nodes by their fingerprint's bits 64..128: a find is one control word and
    /// one slot, then the node's fingerprint compared only on a tag match.
    index: Index,
```
5. `struct Key`: `tag: u32, home: usize` become `hash: u64`; in `Key::of`, `tag: ...,` and
   `home: ...,` become `hash: (bits >> 64) as u64,`; delete `fn entry`.
6. `Shard::new`: `level: AtomicUsize::new(0), tables: std::array::from_fn(|_| OnceLock::new()),`
   becomes `index: Index::new(),`, and the `if shard.tables[0].set(...)` block is deleted.
7. Delete `Shard::table`, `Shard::probe` and `Shard::grow`; `Shard::find` becomes:
```rust
    /// The node whose fingerprint is `key`'s, taking no lock.
    fn find(&self, key: &Key) -> Option<&Node<T>> {
        let is = |local: u32| {
            self.node(local).expect("an index entry before its node").content.fingerprint() == key.fingerprint
        };
        self.node(self.index.find(key.hash, is)?)
    }
```
8. `HashCons::intern_content`, after `let held = shard.lock.lock();`:
```rust
        if let Some(node) = shard.find(&key) {
            drop(held);
            return matched(node, &content, value);
        }
        let node = self.append(shard, &key, content, value);
        drop(held);
        Interned { node }
```
9. `HashCons::append`: drop the `level: usize` parameter (doc: "Called with the shard locked
   and `key` absent from its index."); after `let node = nodes[at].get().expect("a node just set");`
   the `let level = match ...`, `let table = ...`, `let Err(empty) = ...` and `table[empty].store(...)`
   lines become:
```rust
        let hash_of = |local: u32| {
            (shard.node(local).expect("an index entry before its node").content.fingerprint().0 >> 64) as u64
        };
        shard.index.insert(local as usize, key.hash, local, hash_of);
```
   (`shard.len.store(local + 1, Ordering::Release);` and `node` stay).
10. Module doc bullets 2 and 3 become: "The fingerprint picks the shard (its bits 0..32) and
   is the node's hash in the shard's `blaze_mem::swiss::Index` (bits 64..128: the probe's start
   and tag)." and "A full index is copied into one twice its size under the lock and published
   with one release store. The tables it outgrew stay until the interner is dropped (a reader
   may still be probing one), so they cost at most what the last one does."

The tests (`tables_grow_and_keep_every_value` and the rest) touch no internals and stay.

---

> forwarded to fastcompile (from requests, pass 16; told by tmux).
## fastcompile-stencil-x64 1: fastcompile-stencil-patch, codegen: x64's `Unwind::Windows` info is final, not Cranelift's raw `UNWIND_INFO` (`crates/blaze-stencil/src/code.rs` `Unwind::Windows`, the v2 consumer in `crates/hir-ty-codegen`)

What: document `code::Unwind::Windows { info }` as "the `UNWIND_INFO` as the dev path's
`unwind/seh.rs` `with_saves` leaves it: ready for `.xdata`", and when codegen turns a stencil
`Code` into a `blaze_codegen::Unwind::Windows`, take `info` as-is (don't run `with_saves` on it
again: a second pass re-indexes the saves and adds a second allocation code).

Why: the x64 glue (`arch/x64.rs` `unwind`, `arch/x64/frame.rs` `win_raw` then `with_saves`)
first writes Cranelift's exact `UNWIND_INFO` bytes (checked against
`winx64::UnwindInfo::emit` in `arch/x64/tests.rs`), then corrects them the way the dev path
corrects Cranelift's, so a stencil function and a Cranelift one end up with the same `.xdata`.
Nothing reads the variant outside blaze-stencil yet (`rg 'Unwind::Windows'`), so this is a
doc line now and a rule for whoever writes the consumer.

> done by fastcompile-stencil-patch: `code::Unwind::Windows` is documented as final (after
> `with_saves`, ready for `.xdata`, take as-is). The consumer rule is codegen's.

Status: done for blaze-stencil; open for codegen's consumer

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## dedup: bits: blaze-stencil `check_index`'s `seen` and `size`'s `codes` onto `blaze_ids::bits`

From `dedup-bits-codegen` (report `/tmp/blaze-lanes/v2-dedup-bits-codegen.md`; parent
`dedup-bits`). Two hand-written bit sets become the one family in
`crates/blaze-ids/src/bits.rs`. Same members and results, nothing persisted or hashed.

1. `crates/blaze-stencil/Cargo.toml`: `blaze-ids` moves from `[dev-dependencies]` to
   `[dependencies]`, and joins `[build-dependencies]`, since `build.rs` compiles
   `src/table/format.rs` with `#[path]` (`blaze-ids` depends on `blake3`, already a build
   dependency here, `blaze-radix`, `serde`, `smallvec`, `thin-vec`; nothing of blaze-stencil's).

   Before:

       [dependencies]
       blaze-codegen.workspace = true
       cranelift-codegen.workspace = true
       smallvec.workspace = true

       [build-dependencies]
       ...
       # The library's id: the BLAKE3 hash of its bytes, which stencil units' settings carry.
       blake3.workspace = true

       [dev-dependencies]
       ...
       blaze-alloc-count.workspace = true
       blaze-ids.workspace = true

   after:

       [dependencies]
       blaze-codegen.workspace = true
       blaze-ids.workspace = true
       cranelift-codegen.workspace = true
       smallvec.workspace = true

       [build-dependencies]
       ...
       # The library's id: the BLAKE3 hash of its bytes, which stencil units' settings carry.
       blake3.workspace = true
       # `src/table/format.rs`'s sets, as the patcher reads them.
       blaze-ids.workspace = true

       [dev-dependencies]
       ...
       blaze-alloc-count.workspace = true

2. `crates/blaze-stencil/src/table/format.rs`.

   Module doc, before:

       //! generator ([`Writer`]) and read in place by the patcher ([`Reader`]). Shared with the
       //! generator (`build.rs` includes this file with `#[path]`); it uses nothing but `std`,
       //! [`crate::shape`] and [`crate::enc`].

   after:

       //! generator ([`Writer`]) and read in place by the patcher ([`Reader`]). Shared with the
       //! generator (`build.rs` includes this file with `#[path]`); it uses nothing but `std`,
       //! `blaze_ids::bits`, [`crate::shape`] and [`crate::enc`].

   Imports, before:

       use std::fmt;

       use crate::{enc::Form, shape::Shape};

   after:

       use std::fmt;

       use blaze_ids::bits::BitSet;

       use crate::{enc::Form, shape::Shape};

   `Reader::check_index`, before:

           let mut seen = vec![0u64; (self.counts.stencils as usize).div_ceil(64)];
           ...
               let word = &mut seen[stencil as usize / 64];
               let bit = 1u64 << (stencil % 64);
               if *word & bit != 0 {
                   return Err(FormatError::Bounds("a stencil indexed twice"));
               }
               *word |= bit;
               found += 1;

   after (`stencil < self.counts.stencils` is checked just above, so `insert` is in range):

           let mut seen: BitSet<u32> = BitSet::new(self.counts.stencils as usize);
           ...
               if !seen.insert(stencil) {
                   return Err(FormatError::Bounds("a stencil indexed twice"));
               }
               found += 1;

3. `crates/blaze-stencil/build.rs` doc, before:

       //! The shape set, the hole forms and the library format are the crate's own files, included
       //! here so the generator and the patcher read one definition. They use nothing but `std` and
       //! each other.

   after:

       //! The shape set, the hole forms and the library format are the crate's own files, included
       //! here so the generator and the patcher read one definition. They use nothing but `std`,
       //! `blaze_ids::bits` and each other.

4. `crates/blaze-stencil/src/patch/emit.rs` `size`: the trap codes met, a `u8` each
   (`TrapCode::as_raw` is a `NonZeroU8`), stay 4 words inline.

   Imports, before:

       use cranelift_codegen::ir::{self, BlockArg};

   after:

       use blaze_ids::bits::BitArray;
       use cranelift_codegen::ir::{self, BlockArg};

   Before:

           let mut codes = [0u64; 4];
           let mut mark = |code: ir::TrapCode| {
               let at = code.as_raw().get();
               codes[usize::from(at / 64)] |= 1 << (at % 64);
           };
           ...
           let stubs = codes.iter().map(|it| it.count_ones() as usize).sum::<usize>() + glue_stubs + 1;

   after:

           let mut codes = BitArray::<4, u8>::EMPTY;
           let mut mark = |code: ir::TrapCode| {
               codes.insert(code.as_raw().get());
           };
           ...
           let stubs = codes.count() + glue_stubs + 1;

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## dedup: SelectError is blaze-usercode's (from dedup-scan)

`crates/blaze-testsel/src/select.rs`'s `pub enum SelectError<E>` with its `From<Corrupt>` and
`Display` impls is token-for-token `blaze-usercode/src/test/select.rs`'s, and testsel depends on
usercode. Delete the enum and both impls (from `#[derive(Debug)] pub enum SelectError<E>` to the
end of `impl<E: fmt::Display> fmt::Display for SelectError<E>`), and in the `use blaze_usercode::{…}`
change `test::{Attrs, Input, Passes, select::keepable},` to
`test::{Attrs, Input, Passes, select::{SelectError, keepable}},`. `pub use` it instead if
testsel's callers name `blaze_testsel::select::SelectError` (`rg 'testsel::.*SelectError'`).
`RecordError` stays (it has `Table`, usercode's has not).

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## dedup: blaze_codegen::content::kind_tag (from dedup-scan)

`crates/blaze-premap/src/key.rs`'s `fn reloc_tag(kind: RelocKind) -> u8` is
`blaze-codegen/src/content.rs`'s `kind_tag` arm for arm (0..=25). After codegen makes `kind_tag`
`pub` (request in codegen.md), delete `reloc_tag` and at its use in the key hash change
`.update(&[reloc_tag(*kind)])` to `.update(&[blaze_codegen::content::kind_tag(*kind)])`. The
bytes hashed are the same, so no key changes; one table means a new `RelocKind` can't get two
different tags.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

## dedup: blaze_codegen::unwind::FrameRule (from dedup-scan)

`crates/blaze-stencil/src/code.rs`'s `pub enum Rule` is hir-ty-codegen's `FrameRule` (its doc
says so) plus `RaSignState(bool)`. After codegen moves `FrameRule` with `RaSignState` into
`blaze-codegen/src/unwind.rs` (codegen.md), replace the enum and its doc with
`pub use blaze_codegen::unwind::FrameRule as Rule;`. Nothing else changes.

> forwarded to fastcompile (from requests, pass 16; told by tmux).


## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-run` (report `/tmp/blaze-lanes/v2-dedup-cursor-run.md`). Not built; the diff
applies as of 2026-10-29.

- `blaze-inproc/src/frame.rs`'s `Reader` and `Writer` are the same shape as
  `blaze_format::cursor`'s, but they stay. `frame` is compiled without `host` and the run process
  uses it (`runtime.rs`), linking `blaze-inproc` with only `blaze-mem`; this is the reason
  `dedup-codec-leb` gave for `load/macho/fixup.rs`. The only edit is a comment saying why.
  `FrameError` and its tests stay.
- `blaze-stencil`: nothing to move. `table/format.rs`'s `Reader` is a checked table view, not a
  cursor. `enc.rs` patches instruction fields in place and is shared with `build.rs` (std only).
  `buf.rs`'s `put1`..`put8` are the code buffer's emitter API beside its labels, fixups and
  relocations.

```diff
--- a/crates/blaze-inproc/src/frame.rs
+++ b/crates/blaze-inproc/src/frame.rs
@@ -154,7 +154,9 @@
     Ok(Some((Frame { kind: header.kind, seq: header.seq, body }, end)))
 }
 
-/// Writes the fields of a body, little-endian.
+/// Writes the fields of a body, little-endian. Not `blaze_format::cursor`'s `Put`, nor
+/// [`Reader`] its `Reader`: the run process links this crate with only `blaze-mem`, and
+/// `blaze-format` would bring zstd and serde into every running program.
 pub struct Writer<'a> {
     out: &'a mut Vec<u8>,
 }
```

> forwarded to fastcompile (`crates/blaze-lazy/`) (requests, pass 19; told by tmux).

### blaze-lazy (from `dedup-cursor-store`, report `/tmp/blaze-lanes/v2-dedup-cursor-store.md`)

`blaze-lazy/src/wire.rs` keeps its `head`, `u32_at` and `u64_at` for the same reason: `wire` is
compiled without the `daemon` feature and linked into the run process with only `blaze-inproc`
behind it (lib.rs, "The run process links this crate without its `daemon` feature"). The frames
are fixed-layout (16, 24 and 24 bytes plus a reason) and decoding checks every length, so there
is nothing else to move. The only edit is a comment saying why:

```diff
--- a/crates/blaze-lazy/src/wire.rs
+++ b/crates/blaze-lazy/src/wire.rs
@@ -176,3 +176,7 @@
 
+/// The frames' fields are written and read by these three helpers, not by
+/// `blaze_format::cursor`: the run process links this module without the `daemon` feature,
+/// with only blaze-inproc behind it, and `blaze-format` would bring zstd and serde into every
+/// running program.
 fn head(out: &mut Vec<u8>, kind: u32, lazy: u32, request: u64) {
```

Status: open

## dedyn: crates/blaze-intent: `Files` by type, not `&dyn Files` (user's call, 2026-10-29)

`Edits::files` is asked once per file a request's edits read or move (`lib.rs:147`,
`rename.rs:105`, `:471`, `:496`), through `&dyn Files`. Its implementations: the test's `View`
and the daemon's view, still to be written; neither is blaze-intent's to name, so a generic. Code
size: `apply` and `rename.rs` are compiled once per implementation, once in a release build
(the daemon's), twice in blaze-intent's test build.

- `lib.rs:66`: `pub(crate) struct Edits<'a> {` → `pub(crate) struct Edits<'a, F: Files + ?Sized> {`
- `lib.rs:69`: `pub files: &'a dyn Files,` → `pub files: &'a F,`
- `lib.rs:81`: `pub fn apply(db: &Db, files: &dyn Files, edits: &[Edit]) -> Result<Applied, ApiError> {`
  → `pub fn apply<F: Files + ?Sized>(db: &Db, files: &F, edits: &[Edit]) -> Result<Applied, ApiError> {`
- `lib.rs:120`: `impl Edits<'_> {` → `impl<F: Files + ?Sized> Edits<'_, F> {`
- `rename.rs:61`: `cx: &mut Edits<'_>` → `cx: &mut Edits<'_, impl Files + ?Sized>`; the same for
  `rename.rs:245` (`classify`), `:270` (`classify_one`), `:385` (`record_expr_field`) with
  `cx: &Edits<'_, impl Files + ?Sized>`, and `:459` (`moves`) with `&mut`. Add `Files` to
  `rename.rs`'s `use crate::{.., Edits}`.

Callers (`tests/rename.rs`, and the daemon's when written) pass `&view` as today.

> forwarded to fastcompile (`crates/blaze-intent/`; told by tmux; requests, pass 22).

## frontend-deps: dependency images through the global cache (frontend.md item 5; 2026-10-29)

`hir_def::deps` (lane frontend-deps) keeps each dependency crate's def map, lang items and
signatures as one image under a portable key: no absolute path, the package directory and
`OUT_DIR` written as markers. Two checkouts of the same crates at different paths have the
same keys (test `keys_hold_no_path_of_the_checkout`), so an image made on one machine can be
served on another. On hir-def's side the read-through is written:
`hir_def::deps::ReadThrough { local, global }` is an `ImageStore` that asks `local`, then
`global`, and puts what `global` gave into `local`. Puts and `reads_now` go to `local`. Asked
of you, in `blaze-global`:

1. **A public proof for an image's keys** (`public.rs`, beside `QueryMade`/`RunMade`). A key
   `K` (`hir_def::deps::keys`) is public when every crate it covers is public: the crate and
   each crate in its dependency closure is `Origin::CratesIo` or `Origin::Toolchain`, its
   `build` is the blaze build's code identity, and its `OUT_DIR` is `Origin::Outputs`. Keys
   derived from `K` are public under two conditions: the slots (`reads::slot(K, n)`) must hold
   names that are all public, and the image (`reads::image_key(K, record, values)`) must have
   such a record. A name is public when it is a file or listing under a public root, or a
   variable in `Rule::public_env`.
   hir-def can't be a dependency of `blaze-global`, so the key can't be made again inside
   the rule. Say which form you want, and frontend-deps exposes it:
   - (a) `hir_def::deps::KeyInputs` (the crate closure's origins in key order, the `build`,
     the reads' names), made by the same walk as the key, which the driver passes to your
     rule;
   - (b) a `&dyn Fn(&KeyInputs) -> ContentId` your rule calls to make the key again.
2. **An `ImageStore` over the client** for the driver (lane cargo wires it):
   - `get`: when `key` has a `Public`, run `Client::fetch` into the local store and wait for
     its completion, then read it with `Reader::pinned`. A key with no proof is `Ok(None)`
     without asking the network.
   - `put`: publish through `Client::publish` when the key has a proof and the builder is a
     publisher. Otherwise it is a no-op `Ok(())`, since the local store already kept the
     image.
   - `reads_now` is never called on it (`ReadThrough` asks `local`); return an `Err` naming
     that.

   The bytes are verified by the local store as any fetched value is. A fetch that fails is
   an `Err` (the build's error), never a quiet miss.

Why: frontend.md item 5 says signatures are read "from the store or the global cache". With
this, the first build of a crates.io dependency on a new machine reads its def map and
signatures from the cache instead of parsing it.

Status: open
