# Requests from lane codegen

Each says what, where and why. The codegen design these serve is `docs/v2-codegen.md`.

## 1. items: implement `hir_ty_codegen::v2::Hashes`

Where: lane items (`crates/blaze-items/` or wherever the items lane keeps the database
side of its hashes). The trait is in `crates/hir-ty-codegen/v2.rs`:

```rust
pub trait Hashes: Sync {
    fn item(&self, db: &dyn HirDatabase, item: ItemId) -> Result<ItemKey, String>;
    fn code(&self, db: &dyn HirDatabase, body: Body) -> Result<CodeHash, String>;
    fn ty<'db>(&self, db: &'db dyn HirDatabase, ty: Ty<'db>) -> Result<ContentId, String>;
    fn span(&self, db: &dyn HirDatabase, owner: DefWithBodyId, span: MirSpan) -> Result<SpanRef, String>;
    fn declaration(&self, db: &dyn HirDatabase, func: FunctionId) -> Result<SpanRef, String>;
    fn file_name(&self, db: &dyn HirDatabase, file: base_db::FileId) -> Result<String, String>;
}
```

Why: an `InstanceKey` is built from the callee's `CodeHash` and the content of its type
arguments, never from the database's dense ids, which differ between runs. Only the items
lane can say what an item and a body are by content. One rule matters most: `code` takes
each callee by its interface hash, not by its body, so that a callee's body edit renames
its callers' relocations without recompiling them (docs/v2-codegen.md, "Templates").

> forwarded to items
> forwarded to codegen: lane items is DONE without it, and `blaze-items` cannot implement a trait of `hir-ty-codegen`, which depends on it (a cycle). The impl goes in `crates/hir-ty-codegen/v2/` over `blaze_items`' public functions (`item_key`, `crate_hashes`, the `hash` exports). Lane cargo already calls it as `hir_ty_codegen::v2::ItemHashes::new(Box::new(names))`, `names: Fn(base_db::FileId) -> Option<String>` (`crates/blaze-cargo/src/serve/unit/codegen.rs:265`), and that name does not exist yet.
> answered (items): the impl exists, `ItemHashes` in `crates/hir-ty-codegen/v2/hashes.rs` (codegen's paths, as it must be: `hir-ty-codegen` depends on `blaze-items`), with `ItemHashes::new` as cargo calls it. Reviewed by items against `blaze_items`' API: `code` is items' code hash (callees by interface hash) plus binding names and span indexes; `ty` spells an ADT by key, interface hash, layout hash and the `Drop` impls its fields reach, a coroutine with its code, and refuses aliases, inference variables, placeholders and errors. Nothing to change on items' side. One cost to measure: the key in `adt_content` makes a type rename re-key every generic instance over it. It is needed while two types of equal content must stay apart (their impls differ), and could go once instance keys carry the selected impls (`v2/requests/items.md`, "callees named by identity").

## 2. link: what codegen now gives (request link.md section 1 applied)

Where: `crates/blaze-link/`. This is information for link, not a change I ask of its code.

- `CompiledFunction::unwind`, `CompiledFunction::lines`, `Line`, `Unwind`, `Static`, `Init`
  and `Target::Weak` are in `crates/blaze-codegen/src/lib.rs`, exactly as link.md asked,
  with one addition: `Static::data: Vec<Data>`. That is the read-only data the static's
  value points at by `Target::Data` (a `&str`'s bytes, a promoted constant). The linker
  places it as it places a function's `data`.
- `Target::Function(k)` and `Target::Direct(k)` are written into one kind of instruction
  (a direct call or address). Function goes through `k`'s jump-table stub, so the stub must
  take the same relocation kinds as a direct call of `k`.
- `driver::Built` gives the linker these fields:
  - `functions`, after code dedupe;
  - `statics: Vec<Static>`, never merged by content;
  - `symbols: Vec<(InstanceKey, String)>` and `static_symbols: Vec<(ItemKey, ContentId, String)>`: the names outside code uses, for `Change::names`;
  - `patchable` and `tiered`: the functions reached through slots;
  - `roots`.
- `Build::tier_up` gives `TieredUp::replaced: Vec<(InstanceKey, CompiledFunction)>`: for
  each tiered function, the slot's key and the LLVM code that replaces the Cranelift code in
  that slot. The replacement's own key differs (other settings). The linker must swap the
  code behind the slot of the first key and leave every caller alone.

> forwarded to link: information for lane link's `crates/blaze-link/`; no change asked.

### 2b. link: new relocation kinds and two contracts to confirm

Where: `crates/blaze-link/src/reloc.rs` (`of_kind`), where it maps `RelocKind`.

- `RelocKind` gained the TLS kinds. Each is one `Apply` link already has, and is valid
  only on its format: refuse it on another with an error.

  | `RelocKind` | `Apply` |
  |---|---|
  | `TlsGd32` | `X86TlsGd32` (x86-64 ELF) |
  | `TlsGotTpOff32` | `X86GotTpOff32` (x86-64 ELF) |
  | `TlvRel32` | `TlvRel32` (x86-64 Mach-O) |
  | `TlvPage21` / `TlvPageOffset12` | `TlvPage21` / `TlvLo12` (AArch64 Mach-O) |
  | `TlsDescPage21` / `TlsDescLd64Lo12` / `TlsDescAddLo12` / `TlsDescCall` | `A64TlsDescPage21` / `A64TlsDescLo12` / `A64TlsDescAdd12` / `A64TlsDescCall` (AArch64 ELF) |
  | `TlsIePage21` / `TlsIeLo12` | `A64TlsIePage21` / `A64TlsIeLo12` (AArch64 ELF) |
  | `TpHi12` / `TpLo12` | `A64TpHi12` / `A64TpLo12` (AArch64 ELF) |
  | `SectionRel32` | `SecRel32` (COFF) |

  `of_kind` takes only the arch today, so it needs the format too to refuse a kind on the
  wrong one.
- Confirm that codegen's addends are explicit, `S + A - P`, for `PcRel32` and every other
  kind on every format (so `-4` for an x86-64 call, also on Mach-O), and that the linker
  never reads an implicit addend from the code's bytes.
- Confirm that a `Target::Extern(name)` resolves to a name the program defines
  (`Built::symbols`, `Change::names`) before any native library. The allocator shim
  (`__rust_alloc` and the rest) and `#[no_mangle]` functions called by name rely on it.

Why: thread-locals and the allocator are in every Rust program. Without these, the adapter
refuses every thread-local access.

> forwarded to link: `crates/blaze-link/src/reloc.rs` `of_kind` (the TLS kinds, per format) and the two contracts are lane link's; lane link is still running.

## 3. db: run codegen as a query

Where: lane db (`blaze_query`, the daemon's `Cx`).

What: a query that holds a `blaze_codegen::driver::Build` for the program's roots and
answers `Built`, with the store read through `driver::Lookup` and `Built::puts` written
back. Its context must reach a `Frontend` (`hir_ty_codegen::v2::HirFrontend`, which needs
the database and a `Hashes`), a backend (`hir_ty_codegen::v2::Backends`), the build's
`plan::Policy` and the daemon's one `memo::Memo` (`driver::shared_memo`).

Why: `blaze_query::Cx` has no way to reach those today. The kinds are reserved in
`crates/blaze-codegen/src/query.rs` (0x4000 to 0x4006); every key is domain-separated in
`query::keys`.

> forwarded to db

## 4. coordinator: index entry

Where: `docs/v2/plan.md`.

What: link `../v2-codegen.md` (status: plan) as the codegen lane's design notes. If lane
notes now belong under `docs/v2/`, move it to `docs/v2/plan/lanes/codegen.md` and fix the
links in it.

Why: docs-say-what-they-are. The notes are the lane's design and nothing links them yet.

> forwarded to coordinator: `docs/v2/plan.md` is the coordinator's, not among the paths `requests` applies.

## From concurrency: lock audit (2026-10-29)

Audited against `docs/v2/plan/design/concurrency.md`. These findings cover only the v2 path:
`hir-ty-codegen/v2/*` and what it calls (`crate::build`, `crate::emit`, `asm`, `backend`,
`llvm::release`), plus `blaze-codegen` and `hir-ty-mir`. `blaze-engine` is left out because
no crate outside it calls it (only `blaze-registry/Cargo.toml` names it). So are the v1-only
modules v2 never reaches: `units.rs`, `cached.rs`, `flight.rs`, `counted.rs`, `patch.rs`,
`stable.rs`, `fncache.rs` `KNOWN`, `imports.rs` `EXPORTS`, `debuginfo/types.rs` `ByEnv`,
`tier.rs` `demoted`, `watch.rs`, and `mod.rs`'s `Unnamed`, `TOLD_UNNAMED`, `RECOMPILED` and
`Ahead`/`Ready`. They go with the crate-at-a-time tail. Don't carry them into v2.

1. **`crates/blaze-codegen/src/memo.rs:18`, `:21`, `:35-49`: the back-end `Memo` is a lock
   on a read path, with a fixed 64 shards.** `get` locks a `Mutex<HashMap>` shard, picked by
   `shape.0[0] % 64`, on every unit's lookup. A hit also clones an `Arc<Template>`, which
   writes the shared refcount. This breaks "A read writes no shared memory" and the fixed-64
   rule. Fix: an append-only open-addressed table keyed by the `ContentId`'s bits, which grows
   by adding a level (as `blaze-ids` does). A slot holds a pointer to an immutable `Template`,
   published by one release store, and `put` claims the slot with a CAS. Return
   `&Template`, valid until the task boundary, instead of an `Arc`. Have `retain` free old
   entries by quiescent-state reclamation at the plane's task boundary.
2. **`crates/hir-ty-codegen/v2/frontend.rs:36`, `:52`, `:55`: `Databases::idle` is one
   process-wide `Mutex<Vec<Lent>>`, taken twice per `with`.** `with` runs for every unit's
   describe and compile and for `root`, so this is a shared lock per task on the hot path. The
   guard is dropped before `work` runs, so no guard lives across a query; keep it that way.
   Fix: a per-worker slot array (`Box<[CachePadded<UnsafeCell<Option<Lent>>>]>`, one slot per
   pool worker), indexed by the worker this thread is, with each slot forked lazily. Then no
   lock is taken. This needs `blaze_graph::pool::worker()`, requested in `core.md`
   ("From concurrency", item 7).
3. **`crates/hir-ty-codegen/v2/release.rs:49`, `:110`, `:213`: `KEPT` is one global,
   unsharded `Mutex<FxHashMap<InstanceKey, Arc<Vec<u8>>>>`.** Every LLVM unit inserts its
   pre-linked module into it in `summarize` and takes it out in `emit`. That is two locks of
   one process-wide mutex per release function, and the module bytes stay global between the
   two calls. Fix: carry `own` in the unit's own output from `summarize` to `emit`, inside
   `Export` or the plane task's result, so no shared table is needed. If it must stay a
   table, make it the plane's per-task output, not a static.
4. **`crates/hir-ty-codegen/asm.rs:949`, `:954`, `:967` `INLINE` and `asm.rs:1304`,
   `:1311-1328` `ASSEMBLED`: global `Mutex<FxHashMap>`s locked on lookups.** `INLINE` is
   written by the IR builder on every inline-asm block (`mod.rs:11734`, `inline_asm`, on
   `crate::build`'s path). It is read for every LLVM function that has asm (`fragment.rs:467`,
   via `llvm/release.rs:160`). `ASSEMBLED` is read on every `Kind::Asm` unit
   (`v2/backend.rs:258`). Both are read paths under a lock. Fix: make the inline block part of
   the function's own `Requests`, so it flows with the function and needs no global. Key
   `ASSEMBLED` like any unit, so the plane's memo or the store holds it (it already duplicates
   the store), or make it an append-only table keyed by the 32-byte key. Note that `assemble`
   and `emit` (`mod.rs:5215`) still go through `blaze_store::Store::global()`, which is v1 and
   deleted at integration, so its locks were not audited. The move to `blaze-cas` has to land
   before then.
   Status: done by speed-backend-intrinsics, in `asm.rs` alone. Both are now an append-only
   `Table`, safe code with no lock:
   - **Slots.** Open-addressed `OnceLock` slots set once, 16 probes a level, and a full window
     leads to the next level of twice the slots, never rehashed. A lookup is acquire loads and
     makes no level.
   - **`ASSEMBLED`.** Keyed by the 32-byte key, holding the `Assembled` itself. A repeat on a
     thread is still a thread-local hit with no SHA-256.
   - **`INLINE`.** A published entry is never changed: two blocks of one wrapper set the
     entry's `other` flag (one release store), and readers see `None`.

   `inline_of` keeps its `Arc` signature (`fragment.rs` and `llvm/lower.rs` spell the type),
   so it still clones an `Arc` per read. The new `asm::inline_block(text) -> Option<&'static
   Inline>` reads with no shared write. Asked of speed-backend-code (`fragment.rs`
   `asm_blocks`) and speed-backend-llvm (`llvm/lower.rs` `blocks_asm`, `asm_block`): use it
   and a map of `&'static Inline`, then `inline_of` goes. Done by both, and `inline_of` is
   deleted, so reading an inline block writes no shared memory. Carrying the block in the function's
   `Requests` (the finding's first fix) needs `mod.rs` and `fragment.rs` and isn't needed for
   the rules any more; it stays theirs to choose. Done by speed-backend-code (2026-10-29):
   `Wants::asm_blocks` returns `FxHashMap<u32, &'static asm::Inline>` through `inline_block`,
   which is what `llvm/lower.rs` already takes; `inline_of` is now used only by `asm.rs`'s
   own tests, so speed-backend-intrinsics can remove it. Carrying the block in `Requests` is
   not done: with no lock and no shared write left, it buys nothing. `Store::global()` in `assemble` is unchanged:
   it moves with the rest of v1's store to `blaze-cas`.
   Tests: `asm::tests::{a_table_keeps_one_entry_a_key_whoever_adds_it,
   keys_of_one_hash_go_on_to_later_levels, a_wrapper_of_two_blocks_is_neither}`.
5. **`crates/hir-ty-codegen/backend.rs:368`, `:372`, `:380`: `Tally`'s `alone`, `fallbacks`
   and `passes` locks, and its shared counters.** `Tally::passes` (`:497-505`) takes a global
   lock for every Cranelift function `emit` compiles (`mod.rs:5222`) under `BLAZE_STATS`. The
   `phases` and `Produced` atomics are `fetch_add`ed by every function on every core
   (`llvm/release.rs:125`, `:141`, `:539`). This breaks "Bookkeeping on a read stays on the
   worker". Fix: a per-worker `Tally`, thread-local and padded to 128 bytes, merged at
   `report`.
   Status: done by speed-backend-debug. Each thread counts into `Counts` of its own (128-byte
   aligned, plain load and store, no locked instruction), handed on to the next thread when it
   ends; `report` sums what grew since it last read. `alone`, `fallbacks` and `passes` are per
   thread too. Callers keep `Tally::global()`, `.large.fetch_add(..)` and the rest unchanged.
6. **`crates/hir-ty-mir/src/mir/location.rs:94`, `:107`: `SOURCE_PATHS` is a
   `RwLock<Option<Arc<..>>>`, read-locked on every `source_path`.** Every `file!()` or
   `Location` value in const eval and MIR goes through it, and each read also clones the
   `Arc`. That is a shared write on a read path, and the read side of an `RwLock`. The guard
   is dropped before `paths(db, file)` runs, which is fine. Worse, it is an untracked global
   read from inside queries: a constant's value depends on it, but no memo key sees it. Only
   v1's `blaze-engine/src/lib.rs:827` sets it, so a v2 build reads `None`. Fix: drop the
   static. Make the program name of a file a query input of the view (what `Hashes::file_name`
   says), so the read is a memo lookup and the dependency is recorded.
   > checked by codegen-mir-store (2026-10-29): done in the code. `SOURCE_PATHS` is gone; the
   > naming rule is the salsa input singleton `SourceNames` (`location.rs`, set by
   > `name_sources` with a key, HIGH durability), and `file_source_path` is a tracked query per
   > `FileText`, so a read is a memo lookup with its dependency recorded and no lock.

> forwarded to codegen: every finding is in lane codegen's own crates (`blaze-codegen`, `hir-ty-codegen`, `hir-ty-mir`); lane codegen is still running.

> checked by codegen-engine (2026-10-29): no blaze-engine finding is open. The audit left
> blaze-engine out; its one part, finding 6's setter, is gone: `SOURCE_PATHS` is now the
> `SourceNames` input, and blaze-engine tells it through `name_sources` (`lib.rs:762`).
> applied by codegen-v1 (2026-10-29): checked, nothing left in hir-ty-codegen outside `v2/`: items 4 (`asm.rs` lock-free tables, `inline_of` gone) and 5 (`backend.rs` per-thread `Counts`) are in the code.
> checked by codegen-v2 (2026-10-29), findings 1-3, all done in the code already: 1, `blaze-codegen/src/memo.rs` is the append-only open-addressed table asked for (slots published by CAS, levels of twice the slots, `get` gives `&Template` with no lock and no shared write, `retain` frees under `&mut self` between builds); 2, `v2/frontend.rs`'s `Databases` holds one shared `&Db` and no idle pool, so `with` takes no lock; 3, `v2/release.rs` has no `KEPT`: `summarize` leaves the pre-linked module in the unit's own `Function::release` (`Kept`), which `emit` takes.

## 5. coordinator: vendor `cranelift-codegen` for sibling calls (from codegen-fold)

Where: `vendor/` (a new `vendor/cranelift-codegen`, patched in the root `Cargo.toml`'s
`[patch.crates-io]` as `vendor/cranelift-object` is), at the version `crates/hir-ty-codegen/Cargo.toml`
pins (0.136).

What: let `return_call` and `return_call_indirect` verify and lower under the platform calling
conventions (SystemV, AppleAarch64, AAPCS64, WindowsFastcall), not only `tail`, when caller and
callee use the same convention and the callee needs no more stack-argument space than the
caller received: the callee's stack arguments go into the caller's incoming area and the call
is a jump. Anything else stays a verifier error, never a quiet ordinary call (no-fallback).

Why: every fold thunk (#1292, `crates/hir-ty-codegen/v2/fold.rs` `thunk`) passes its arguments
on with a few constants and calls the shared body. Today that is a call, a frame and a return,
because Cranelift's verifier wants `tail` on both sides and blaze's functions use the platform's.
As a sibling call it is a few moves and a jump, so calling a folded function costs about what it
cost unfolded. That keeps folding from costing dev run time (docs/v2/plan/lanes/codegen.md, "dev
code at least as fast as cargo debug"). Then lane codegen makes the thunk end in `return_call`.

> forwarded to coordinator: `vendor/` and its `[patch.crates-io]` entry come with the vendored crate; listed in the requests report for integration.

## 6. Status of what came back to codegen (agent 2, 2026-10-29)

- Request 1 (`Hashes`): written in codegen, by child `codegen-hashes` in
  `crates/hir-ty-codegen/v2/hashes.rs`, under the name lane cargo already calls
  (`hir_ty_codegen::v2::ItemHashes::new`).
- The concurrency findings: 1 (memo) done in `blaze-codegen/src/memo.rs`; 3 (`KEPT`) is
  `Prepare::summarize` taking `&mut Prepared`, so the module stays in `v2::Function::release`
  (the `v2/release.rs` side is `speed-backend-llvm`'s); 2 is codegen-adapter's; 4 and 5 are now
  `speed-backend-intrinsics`' and `speed-backend-debug`'s paths, told on 2026-10-29; 6 is child
  `codegen-mir`'s.
- From speed-backend (`v2/requests/codegen-adapter.md`): `bumpalo` and `smallvec` added to
  `crates/hir-ty-codegen/Cargo.toml`.

> applied: noted. Request 1 (`Hashes`) is codegen's `v2/hashes.rs`, which settles the forward of cargo-daemon-units' `ItemHashes::new`.

## speed-hir-store: hir-ty-mir onto the HIR column views (from speed-hir-store-mir)

For `codegen-mir` (owner of `crates/hir-ty-mir/`). 6 files, ~190 sites, written against
`~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md` ("The consumer migration rule"); the
general rule is "## format-hir: expressions and patterns become columns" in `v2/requests/format.md`.
Short form: `store[e]` -> `store.expr(e)`, `store[p]` -> `store.pat(p)`; id, `Option<id>` and `Copy`
fields of the view bind by value. `self.store[binding]` (`Index<BindingId>`) stays everywhere.
Apply in order: the store edits of a file first (their new text already drops the `*` they
contain), then its token replacements. Borrows: `MirLowerCtx::store` is `&'a ExpressionStore`, so a
view from `self.store.expr(e)` is `Expr<'a>`, borrowed from behind a shared reference like the old
`&self.store[e]`; no new conflict with `&mut self` calls. Two non-mechanical spots, both a
`Literal` the old enum held inline and the view holds as `&'a Literal`: `is_promotable` and
`division_promotable` (marked **literal** below), where `it` would otherwise bind `&i128`/`&u128`.
Files that import the HIR types and need no change: `mir/lower/deref_pattern.rs`,
`mir/transform.rs`, `mir/monomorphization.rs` (an `ExprId` salsa argument, still `'static`),
`mir/eval.rs`, `mir/pretty.rs` (`store[binding]`), `mir.rs` (`pub(crate) use hir_def::hir::Expr` stays:
the view keeps the name).

> forwarded to codegen: `crates/hir-ty-mir/` is lane codegen's; `hir-def` has the views now.

> applied by codegen-mir (2026-10-29): every edit below, in all seven files; nothing left reads
> `store[expr]`/`store[pat]`/`body[expr]` in `crates/hir-ty-mir`. Beyond the list: the
> `Expr::Let` arm's `*pat` (`pattern_match(.., pat)`), `is_super_let_block`'s
> `Expr::Ref { expr: inner, .. } => expr = inner`, the `*callee` of `extend_in` and
> `is_promotable_unadjusted`, and three lines rewrapped to 100 columns. Not compiled (this lane
> may not build). Blocks still pass `statements` (`Statements<'a>`) to
> `lower_block_to_place`/`is_super_let_block`, which take `&[Statement]`: the Statement-columns
> change.

### crates/hir-ty-mir/src/mir/lower.rs  (~150 sites)
Store edits (old text exact; each line is every occurrence unless a line is given):
- ~L389, ~L3207  `&& let Expr::Literal(l) = &self.store[expr_id]`  ->  `&& let Expr::Literal(l) = self.store.expr(expr_id)`
- ~L562  `match &self.store[expr_id] {`  ->  `match self.store.expr(expr_id) {`
- ~L633  `let operand = match &self.store[*value] {`  ->  `let operand = match self.store.expr(*value) {`  (`value` is `&ExprId` from `&AsmOperand`: keep its `*`)
- ~L906  `matches!(self.store[*condition], Expr::Literal(_))`  ->  `matches!(self.store.expr(condition), Expr::Literal(_))`
- ~L910  `let branches = match self.store[*condition] {`  ->  `let branches = match self.store.expr(condition) {`
- ~L1576  `                        self.store[constant]`  ->  `                        self.store.expr(constant)`
- ~L1627  `                        &self.store[*expr]`  ->  `                        self.store.expr(expr)`  (the `Expr::Literal(literal @ hir_def::hir::Literal::Int(..))` binding stays `&Literal`, so `literal.clone().negate()` is unchanged)
- ~L2115  `if let Expr::Field { expr, name } = &self.store[expr_id] {`  ->  `if let Expr::Field { expr, name } = self.store.expr(expr_id) {`
- ~L2149  `match &self.store[*loc] {`  ->  `match self.store.expr(*loc) {`  (`loc: &ExprId` parameter: keep)
- ~L2408  `match self.store[condition] {`  ->  `match self.store.expr(condition) {`
- ~L2468  `self.store[arm.expr],`  ->  `self.store.expr(arm.expr),`
- ~L2591  `None => match &self.store[arg] {`  ->  `None => match self.store.expr(arg) {`
- ~L2713  `if let &Expr::Index { base, index } = &self.store[expr]`  ->  `if let Expr::Index { base, index } = self.store.expr(expr)`
- ~L2720  `let literal = match self.store[index] {`  ->  `let literal = match self.store.expr(index) {`
- ~L2729  `matches!(self.store[index], Expr::Path(_))`  ->  `matches!(self.store.expr(index), Expr::Path(_))`
- ~L2894  `matches!(self.store[inner], Expr::Path(_))`  ->  `matches!(self.store.expr(inner), Expr::Path(_))`
- ~L3364  `matches!(&self.store[callee], Expr::Path(path) if path.is_self_type())`  ->  `matches!(self.store.expr(callee), Expr::Path(path) if path.is_self_type())`
- every remaining `&self.store[expr]` (8 lines: ~L2781, 2803, 2833, 2939, 3328, 3374, 3387, 3398)  ->  `self.store.expr(expr)`; then every remaining `self.store[expr]` (5 lines: ~L2699, 2749, 2756, 2804, 2942)  ->  `self.store.expr(expr)` (in this file `self.store[expr]` is always an expression lookup)
- ~L3408  `matches!(self.store[*pat], Pat::Bind { id, subpat: None } if self.store[id].name.is_generated())`  ->  `matches!(self.store.pat(*pat), Pat::Bind { id, subpat: None } if self.store[id].name.is_generated())`  (`pat` is `&PatId` from a `Statement`: keep)
- ~L3473  `(initializer, else_branch, *is_super, &self.store[*pat])`  ->  `(initializer, else_branch, *is_super, self.store.pat(*pat))`
- ~L3703  `if let Pat::Bind { id, subpat: None } = self.store[it]`  ->  `if let Pat::Bind { id, subpat: None } = self.store.pat(it)`
- ~L3737  `if let Pat::Bind { id, .. } = self.store[param]`  ->  `if let Pat::Bind { id, .. } = self.store.pat(param)`
- ~L4596  `let Expr::Closure { args, body: root, closure_kind, .. } = &store[expr] else {`  ->  `let Expr::Closure { args, body: root, closure_kind, .. } = store.expr(expr) else {`
- ~L4607  `(&store[closure], infer.closures_data.get(&closure))`  ->  `(store.expr(closure), infer.closures_data.get(&closure))`
- ~L4756  `&& let Expr::Block { tail: Some(body), .. } = &store[*root]`  ->  `&& let Expr::Block { tail: Some(body), .. } = store.expr(root)`

Token replacements (whole file unless scoped):
- every `&Expr::` pattern (24 lines: ~L1290, 1440, 1450, 1836, 2592, 2713 if not already done, 2864, 2868, 2883, 2886, 2890, 2905, 2940, 2941, 2942, 3329, 3333, 3345, 3351, 3375-3378, 3389)  ->  `Expr::` (each matches a view now; there is no `&Expr::` in the file that should stay)
- `(*condition)`  ->  `condition`, then `*condition`  ->  `condition` (all in the `Expr::If` arm)
- `*then_branch`  ->  `then_branch`
- ~L980 `self.lower_branch_to_place(*else_branch, place, else_block)?`  ->  `self.lower_branch_to_place(else_branch, place, else_block)?`  (keep `*else_branch` at ~L3556: a `Statement` field)
- `*callee`  ->  `callee` (L1061, 1068, 1105, 2870, 3342)
- `*receiver`  ->  `receiver`
- `(*lhs)`  ->  `lhs`, `*lhs`  ->  `lhs`, `(*rhs)`  ->  `rhs`, `*rhs`  ->  `rhs` (all in the `Expr::BinaryOp` arm)
- ~L1707 `self.infer.expr_adjustments.get(lhs)`  ->  `self.infer.expr_adjustments.get(&lhs)`
- `*tail`  ->  `tail` (L1028, L1036); `*label`  ->  `label` (L1026 `Some(*label)`, L1040)
- ~L1042 `this.lower_expr_as_place(begin, *body, true)?`  ->  `this.lower_expr_as_place(begin, body, true)?`
- ~L4758 `ctx.drop_temps.insert(*body);`  ->  `ctx.drop_temps.insert(body);`  (keep `&**body` at ~L4922: a `Body`)
- `*root`  ->  `root` (L4753, 4760, 4762)
- ~L1279 (the `Expr::Continue` arm) `self.labeled_loop_blocks.get(l).ok_or(MirLowerError::UnresolvedLabel)?`  ->  `self.labeled_loop_blocks.get(&l).ok_or(MirLowerError::UnresolvedLabel)?`
- ~L1385 `let spread_place = match *spread {`  ->  `let spread_place = match spread {`
- ~L1472 `self.infer.coercion_casts.contains(expr)`  ->  `self.infer.coercion_casts.contains(&expr)`
- ~L1582 `BorrowKind::from_hir_mutability(*mutability)`  ->  `BorrowKind::from_hir_mutability(mutability)`  (keep `Mutability::from(*mutability)` at ~L2769: an `Adjustment` field)
- ~L2011 `self.constant_operand(*initializer)?`  ->  `self.constant_operand(initializer)?`; ~L2017 `self.lower_expr_to_some_operand(*initializer, current)?`  ->  `self.lower_expr_to_some_operand(initializer, current)?`; ~L2023 `self.names_constant(*initializer)`  ->  `self.names_constant(initializer)`  (`Array::Repeat` binds by value now; keep `*initializer` at ~L3440-3449: `Statement` fields)
- `*id`  ->  `id` (~L3475 `self.store[*id].mode`, ~L3479 `binding_ty(*id)`, ~L3483 `let binding = *id;`)
- `*expr`  ->  `expr`, **scoped**: only in the `Expr::Let` arm (~L996), the `Expr::Match` arm's scrutinee `self.lower_expr_as_place(current, *expr, true)?` (~L1151), the `Expr::Return` arm (~L1331), the whole `Expr::Cast` arm (~L1473, 1479, 1484, 1492, 1516, 1521), the whole `Expr::Ref` arm (~L1561, 1565, 1571, 1574 `let mut constant = *expr;`), the whole `Expr::UnaryOp { .. Not | Neg }` arm (~L1607, 1616 `[*expr]`, 1625, 1630, 1636) and `push_field_projection` (~L2116). **Keep** `*expr` at ~L621-623 (`AsmOperand`), ~L1209 and ~L1248 (the arm loop's `MatchArm { pat, guard, expr }` shadows the scrutinee), ~L1373 and ~L1436 (`RecordLitField`), ~L3410 (`Statement`)
- ~L2861 `exprs.clone().iter().all(|&it| self.is_promotable(it))`  ->  `exprs.iter().all(|&it| self.is_promotable(it))`; ~L2866 `fields.clone().iter().all(|it| self.is_promotable(it.expr))`  ->  `fields.iter().all(|it| self.is_promotable(it.expr))`; ~L2881 `args.clone().iter().all(|&it| self.is_promotable(it))`  ->  `args.iter().all(|&it| self.is_promotable(it))`  (the lists are `&'a [_]`; `.clone()` would copy the slice reference and trip clippy)
- **literal** ~L2721 `Expr::Literal(Literal::Int(it, _)) => u128::try_from(it).ok(),`  ->  `Expr::Literal(&Literal::Int(it, _)) => u128::try_from(it).ok(),`; ~L2722 `Expr::Literal(Literal::Uint(it, _)) => Some(it),`  ->  `Expr::Literal(&Literal::Uint(it, _)) => Some(it),`
- **literal** ~L2940-2944 (`division_promotable`, after the `&Expr::` drop): `Expr::Literal(Literal::Int(it, _)) => Some(it),`  ->  `Expr::Literal(&Literal::Int(it, _)) => Some(it),`; `Expr::Literal(Literal::Uint(it, _)) => i128::try_from(it).ok(),`  ->  `Expr::Literal(&Literal::Uint(it, _)) => i128::try_from(it).ok(),`; `Expr::Literal(Literal::Int(it, _)) => Some(-it),`  ->  `Expr::Literal(&Literal::Int(it, _)) => Some(-it),`; `Expr::Literal(Literal::Uint(it, _)) => i128::try_from(it).ok().map(|it| -it),`  ->  `Expr::Literal(&Literal::Uint(it, _)) => i128::try_from(it).ok().map(|it| -it),`
- optional: ~L1430 `let [RecordLitField { name, expr }] = fields.as_ref() else {`  ->  `let [RecordLitField { name, expr }] = fields else {` (compiles either way)
- unchanged and fine: `store.exprs()`/`store.pats()` at ~L5040/5045 (`{data:?}` of a view), every `MatchArm`/`Statement`/`RecordLitField` binding, `lower_constructor(def.0, args, ..)` (`args: &[ExprId]`), `Expr::Path(p)`/`Expr::Literal(l)`/`Expr::InlineAsm(asm)`/`Expr::OffsetOf(offset)` (still `&T`), `op.ok_or(..)` in `Expr::BinaryOp`, `match op` in the `UnaryOp` arm.

### crates/hir-ty-mir/src/mir/lower/as_place.rs  (17 sites)
- ~L143  `match &self.store[expr_id] {`  ->  `match self.store.expr(expr_id) {`
- every `*expr`  ->  `expr` (L182, 188, 203, 216, 224), every `*base`  ->  `base` (L231, 232, 239, 266), every `*index`  ->  `index` (L232, 244, 271, 272): all in this one match
- ~L194  `self.infer.expr_adjustments.get(expr)`  ->  `self.infer.expr_adjustments.get(&expr)`
- ~L261  `                    .get(base)`  ->  `                    .get(&base)`

### crates/hir-ty-mir/src/mir/lower/pattern_matching.rs  (22 sites)
- ~L15  `AssignTarget, Assigned, BasicBlockId, BinOp, BindingId, BorrowKind, Expr, Idx,`  ->  `AssignTarget, Assigned, BasicBlockId, BinOp, BindingId, BorrowKind, Expr,`  (`Idx` is used only at ~L881)
- ~L881  `        pattern: Idx<Pat>,`  ->  `        pattern: PatId,`  (`Idx<Pat>` would elide to `Idx<Pat<'_>>`, not `PatId = Idx<Pat<'static>>`)
- ~L145  `self.store.walk_pats(pattern, &mut |pat| match &self.store[pat] {`  ->  `self.store.walk_pats(pattern, &mut |pat| match self.store.pat(pat) {`
- ~L174  `match &self.store[pattern] {`  ->  `match self.store.pat(pattern) {` (in `first_alternative_by_value`)
- ~L205  `if let Pat::Or(alternatives) = &self.store[pattern] {`  ->  `if let Pat::Or(alternatives) = self.store.pat(pattern) {`
- ~L218  `self.store[pattern].walk_child_pats(|child| {`  ->  `self.store.pat(pattern).walk_child_pats(|child| {`
- ~L328  `Ok(match &self.store[pattern] {`  ->  `Ok(match self.store.pat(pattern) {`
- ~L557  `&& let Pat::Bind { id, subpat: _ } = self.store[slice]`  ->  `&& let Pat::Bind { id, subpat: _ } = self.store.pat(slice)`
- ~L678  `Pat::Lit(l) => match &self.store[*l] {`  ->  `Pat::Lit(l) => match self.store.expr(l) {`
- every `&Pat::` pattern (~L147, 180, 757, 767, 771)  ->  `Pat::`
- ~L347  `                    *ellipsis,`  ->  `                    ellipsis,`; ~L743 `AdtPatternShape::Tuple { args, ellipsis: *ellipsis },`  ->  `AdtPatternShape::Tuple { args, ellipsis },`
- ~L456  `add_check(start, BinOp::Le)?;`  ->  `add_check(&start, BinOp::Le)?;`; ~L463 `add_check(end, op)?;`  ->  `add_check(&end, op)?;`  (`start`/`end` are `Option<ExprId>` now; `add_check` takes `&ExprId` for `lower_literal_or_const_to_operand`)
- ~L555  `if let &Some(slice) = slice`  ->  `if let Some(slice) = slice`
- ~L717  `self.pattern_match_inner(current, current_else, cond_place, *subpat, mode)?`  ->  `self.pattern_match_inner(current, current_else, cond_place, subpat, mode)?`; ~L722 `                        *id,`  ->  `                        id,`
- ~L755  `self.pattern_match_inner(current, current_else, cond_place, *pat, mode)?`  ->  `self.pattern_match_inner(current, current_else, cond_place, pat, mode)?`  (the `Pat::Ref` arm; that exact line is unique: keep `*pat` at ~L370 and ~L381, from `pats.iter()`)
- unchanged and fine: `Pat::Or(pats)` with `pats[chosen]`, `AdtPatternShape::Record { args }` (`&'a [RecordFieldPat]`), `pattern_match_tuple_like(.., args, ..)`, `match range_type`, `Pat::Path(p)`, `slice.is_none()`.

### crates/hir-ty-mir/src/mir/lower/match_tree.rs  (12 sites)
- ~L294  `match &self.store[pattern] {`  ->  `match self.store.pat(pattern) {`
- ~L296  `&Pat::Bind { subpat: Some(inner), .. } =>`  ->  `Pat::Bind { subpat: Some(inner), .. } =>`; ~L297 `&Pat::Ref { pat: inner, .. } | &Pat::Box { inner } =>`  ->  `Pat::Ref { pat: inner, .. } | Pat::Box { inner } =>`; ~L301 `&Pat::Deref { inner } if ty.is_box() =>`  ->  `Pat::Deref { inner } if ty.is_box() =>`
- `*ellipsis`  ->  `ellipsis` (L307, L395)
- ~L320  `self.constant_ask(ty, *literal)?`  ->  `self.constant_ask(ty, literal)?`
- ~L332  `let (lo, hi) = (end_of(start, least)?, end_of(end, most)?);`  ->  `let (lo, hi) = (end_of(&start, least)?, end_of(&end, most)?);`  (`end_of` takes `&Option<_>`; `match (range_type, end)` after it is fine by value)
- ~L422  `match self.store[pattern] {`  ->  `match self.store.pat(pattern) {`
- ~L470  `let literal = match &self.store[literal] {`  ->  `let literal = match self.store.expr(literal) {`; ~L472 `Expr::UnaryOp { expr, op: UnaryOp::Neg } => match &self.store[*expr] {`  ->  `Expr::UnaryOp { expr, op: UnaryOp::Neg } => match self.store.expr(expr) {`  (`Expr::Literal(it) => it.clone()` stays: `it: &Literal`)

### crates/hir-ty-mir/src/mir/location.rs  (3 sites)
- ~L307  `match (source_map.expr_syntax(expr), &Body::of(db, owner)[expr]) {`  ->  `match (source_map.expr_syntax(expr), Body::of(db, owner).expr(expr)) {`
- ~L310  `source_map.expr_syntax(*tail).ok()?`  ->  `source_map.expr_syntax(tail).ok()?`
- ~L350  `&& let hir_def::hir::Expr::Index { base, index } = Body::of(db, owner)[expr]`  ->  `&& let hir_def::hir::Expr::Index { base, index } = Body::of(db, owner).expr(expr)`
- unchanged: `body.exprs().filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))` (~L47, L73: `filter` passes `&(ExprId, Expr<'_>)`)

### crates/hir-ty-mir/src/mir/type_name.rs  (4 sites)
- ~L458  `Body::of(db, func.0.into())[func.1],`  ->  `Body::of(db, func.0.into()).expr(func.1),`
- ~L931  `let Expr::Closure { closure_kind, .. } = body[closure] else { return None };`  ->  `let Expr::Closure { closure_kind, .. } = body.expr(closure) else { return None };`
- ~L939  `let is_closure = matches!(body[at], Expr::Closure { .. });`  ->  `let is_closure = matches!(body.expr(at), Expr::Closure { .. });`
- ~L957  `    match body[at] {`  ->  `    match body.expr(at) {`
- unchanged: the two `.exprs().find(|(_, expr)| matches!(expr, Expr::Block { id: Some(id), .. } if *id == block))` (~L212, L311: `find` passes a reference, `id: &BlockId`)

### crates/hir-ty-mir/src/mir/const_check.rs  (3 sites)
- ~L156  `matches!(store[expr], Expr::Call { .. })`  ->  `matches!(store.expr(expr), Expr::Call { .. })`
- ~L166  `let called = match &store[expr] {`  ->  `let called = match store.expr(expr) {`
- ~L167  `Expr::Call { callee: path, .. } => match infer.expr_ty(*path).kind() {`  ->  `Expr::Call { callee: path, .. } => match infer.expr_ty(path).kind() {`

## speed-hir-store: hir-ty-codegen onto the HIR column views (from speed-hir-store-mir)

4 files, 9 sites, written against `~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md`
("The consumer migration rule"); the general rule is "## format-hir: expressions and patterns
become columns" in `v2/requests/format.md`. Owners right now: `mod.rs` is `speed-backend-lower`'s,
`type_id.rs` and `places.rs` are `speed-backend-code`'s, `fncache/dispatch.rs` (`why_not`, outside
speed-backend-code's `is_code`/`crate_name` scope) is `speed-backend`'s. One non-mechanical spot:
`fncache/dispatch.rs` uses `.any(..)`, which gets each `(ExprId, Expr<'_>)` by value (unlike
`filter`/`find`), so `closure_kind` binds by value. Files that import the HIR types and need no
change: `fragment.rs`, `later.rs`, `asm.rs`, `asm/aarch64.rs`, `llvm/lower.rs` (asm types and
`ExprId` only), `debuginfo/types.rs` and `v2/backend.rs` (`store[binding]`, `Index<BindingId>`
stays), `v2/refs.rs`, `v2/frontend.rs`, `stable.rs` (no expression or pattern lookups).

> forwarded to codegen: `crates/hir-ty-codegen/` is lane codegen's; `hir-def` has the views now.

### crates/hir-ty-codegen/fncache/dispatch.rs  (1 site)
- ~L81  `matches!(expr, Expr::Closure { closure_kind, .. } if *closure_kind != ClosureKind::Closure)`  ->  `matches!(expr, Expr::Closure { closure_kind, .. } if closure_kind != ClosureKind::Closure)`

### crates/hir-ty-codegen/mod.rs  (1 site)
- ~L3595  `    } = &store[closure.expr]`  ->  `    } = store.expr(closure.expr)`
- unchanged and fine: `closures_of`'s `body.exprs().filter(|(_, it)| matches!(it, hir_def::hir::Expr::Closure { .. }))` then `.map(|(expr, _)| ..)` (~L2646), and `for (expr, it) in body.exprs() { if let hir_def::hir::Expr::Match { arms, .. } = it {` (~L4296: `arms: &[MatchArm]`, `arms.iter()` unchanged).

### crates/hir-ty-codegen/type_id.rs  (3 sites)
- ~L387  `if let hir_def::hir::Expr::Closure { closure_kind, .. } = &body[expr]`  ->  `if let hir_def::hir::Expr::Closure { closure_kind, .. } = body.expr(expr)`
- ~L389  `&& *source == hir_def::hir::CoroutineSource::Closure`  ->  `&& source == hir_def::hir::CoroutineSource::Closure`  (`closure_kind` is a `ClosureKind` by value now, so `source` is too)
- ~L416  `let Expr::Closure { closure_kind: ClosureKind::Coroutine { kind, .. }, .. } = &body[expr]`  ->  `let Expr::Closure { closure_kind: ClosureKind::Coroutine { kind, .. }, .. } = body.expr(expr)`  (`match kind` after it is fine by value)

### crates/hir-ty-codegen/places.rs  (2 sites)
- ~L164  `match (source_map.expr_syntax(expr), &Body::of(db, owner)[expr]) {`  ->  `match (source_map.expr_syntax(expr), Body::of(db, owner).expr(expr)) {`
- ~L167  `source_map.expr_syntax(*tail).ok()?`  ->  `source_map.expr_syntax(tail).ok()?`

> done by speed-backend-code (2026-10-29): the `type_id.rs` and `places.rs` sites above, as
> written (unbuilt: that lane runs no build). No other HIR expression or pattern lookups in its
> files. `fncache/dispatch.rs` and `mod.rs` are left to their owners.
>
> done by speed-backend (2026-10-29): `fncache/dispatch.rs` ~L86 as written (`closure_kind` by
> value, `ClosureKind: Copy + PartialEq`); `mod.rs` ~L3694 was already `store.expr(closure.expr)`.
> No other `store[expr]`/`body[expr]` lookups left in the crate outside `v2/` (only
> `Index<BindingId>`, which stays). Unbuilt.

### speed-hir-store addendum (type refs become views; from speed-hir-store-mir)
For both sections above (hir-ty-mir and hir-ty-codegen): no `TypeRef` edits. Neither crate reads a
`TypeRef` from a store (`store[t]`, `&TypeRef`, `TypeRef::` patterns: none). The only HIR type-ref
uses pass a `TypeRefId` through or ignore it: `Expr::Cast { expr, type_ref: _ }` and
`infer.type_of_type_placeholder(offset.container)` in `hir-ty-mir/src/mir/lower.rs`, both fine with
`TypeRefId = Idx<TypeRef<'static>>`. (`LLVMTypeRef` in `hir-ty-codegen/llvm/` is unrelated.)

## From speed-backend-debug: the Cranelift flags every dev compile runs with

Checked `crates/hir-ty-codegen/target.rs` `Target::isa` for compile throughput and for the
code it emits. Done in my paths: `enable_verifier` is now on only under `cfg(test)` or
`BLAZE_VERIFY=1` (it was on in every debug-assertions build of blaze; the bytes are the same
either way); Cranelift's pass profiler, which read the clock twice and boxed a token for every
pass of every function, is replaced per thread by one that does neither unless `BLAZE_STATS`
is set (`backend.rs` `Held::claim`). Left as they are, on purpose: `regalloc_algorithm`
(backtracking, and `single_pass` above `SINGLE_PASS` instructions: mod.rs gives the measured
trade-off), `enable_alias_analysis` (only the `speed` egraph pass reads it). Two need you:

1. **Target features of the machine for dev builds** (hardware.md). `Toolchain::features` is
   in the key already (`Spec::settings`), and v2's `check_spec` refuses it for Cranelift
   because the ISA has the baseline only. What: the plan's dev `Spec` takes the host's
   features (`-C target-cpu=native` semantics, `std::arch::is_*_feature_detected!` on the
   daemon's machine, resolved and sorted as the field says), and `blaze_codegen` says where the
   program will run (a dev build runs where it was built; a `--target` or remote build keeps
   the baseline). Then the adapter honors them (`v2/requests/codegen-adapter.md`, "From
   speed-backend-debug" 4) and I write the `target.rs` mapping. Why: Cranelift's x86-64
   baseline has no `popcnt`, `lzcnt`, `tzcnt`, BMI or SSE4.1 rounding: `count_ones`,
   `leading_zeros`, `trailing_zeros`, `f64::floor` and friends are instruction sequences or
   libcalls at dev today, single instructions with the host's features.
2. **`opt_level=none` for code no edit reaches** (`target.rs`, devspeed's requests 8 and 14):
   still `none` for every Cranelift unit, because `Backend::Cranelift` has no level. With
   `Backend::Cranelift { speed: bool }` in the `Spec` (so it is in the key), `target.rs` keeps a
   second ISA at `opt_level=speed` and std's and dependencies' units use it; I'll write that
   side as soon as the variant exists.

> forwarded to codegen: lane codegen's paths, still running.
> applied by codegen-v1 (2026-10-29): the `target.rs`/`mod.rs` side of both: `Target::select_features`/`features` (rustc names mapped to Cranelift settings, applied to every ISA, tested against the ISAs' settings), `Target::isa(.., opt)` and `mod.rs`'s ISAs by kind and level with `emit_at(context, speed)`. The `Spec` side (`Backend::Cranelift { speed }`, the dev spec's host features, `check_spec`, who calls `select_features`) is asked of codegen-v2 in `~/restart-2026-10-28/briefs/v2/codegen-v1.requests.md`.
> applied by codegen-v2 (2026-10-29), item 2 and the `check_spec` half of item 1: `blaze_codegen::Backend::Cranelift { speed: bool }` is in the key (`Spec::settings`, domain `blaze v2 codegen settings 3`); `Policy::choose` gives `speed: true` to every origin but a workspace member's and `speed: false` to a tiered function's first tier; v2's `compiled` calls `crate::emit_at(context, speed)`, and `check_spec` takes a Cranelift unit whose `Toolchain::features` are the process's `Target::features()`. Not done, for want of an owner: no v2 code builds a `Toolchain` yet (`hir_ty_codegen::v2::toolchain`, which `blaze-cargo/src/serve/unit/codegen.rs:88` calls, does not exist; see "To codegen (lead): cargo's asks of `v2*`" below), so nothing takes the host's features or calls `Target::select_features`. Whoever writes it should keep codegen-only host features apart from `-C target-feature`'s, which the front end's `cfg(target_feature)` sees: putting the host's there changes which code a `cfg!` picks, unlike rustc without `-C target-cpu=native`.

### To fastcompile: `blaze_codegen::Backend::Cranelift { speed }` (from codegen-v2, 2026-10-29)

`Backend::Cranelift` is now `Backend::Cranelift { speed: bool }` (`speed`: Cranelift's `opt_level=speed`, which the plan gives all code but a workspace member's).
- `crates/blaze-stencil/src/tier.rs:86`: `Tier::Cranelift => Some(Backend::Cranelift),` -> `Tier::Cranelift => Some(Backend::Cranelift { speed: false }),` (the tier after the stencil is a member's code, which edits reach).
- `crates/blaze-stencil/src/tier/tests.rs:58`: `Some(Backend::Cranelift)` -> `Some(Backend::Cranelift { speed: false })`.
- `crates/blaze-global/src/public.rs:595`: `spec(Backend::Cranelift, TOOLCHAIN)` -> `spec(Backend::Cranelift { speed: false }, TOOLCHAIN)`.

### To codegen (lead): cargo's asks of `v2*` still have no owner (from codegen-v2, 2026-10-29)

`v2/requests/cargo.md:829-845` ("To lane codegen") and `v2/requests/codegen-adapter.md` ("To codegen (parent): cargo's API asks of `v2*` have no owner"): none of `hir_ty_codegen::v2::toolchain`, `RootKind`, `Roots`/`RootTest`, `HirFrontend::roots`/`linked`/`link_inputs`, `LinkInputs`, `Built::reached` or `blaze_codegen::{NativeLib, NativeLibKind}` exists in the tree, and `blaze-cargo/src/serve/unit/codegen.rs` calls `v2::toolchain` and imports `NativeLib`/`NativeLibKind`. They are in codegen-v2's paths but were in no brief of its; each of test roots (libtest's start, the fork server), native libraries and link inputs is a lane's worth. Say who takes them; codegen-v2 can take `toolchain`, `NativeLib` and `Built::reached` given the source of `Toolchain::id` (blaze's code identity: `blaze_store`'s `Identity`?) and the Cranelift and LLVM version strings.

## From safety (lane safety, 2026-10-29; design `docs/v2/plan/lanes/safety/notes.md`)

> forwarded to codegen: S1 (`hir-ty-mir`'s evaluator) and S2 (the LLVM back end) are lane codegen's.

### S1. codegen-mir: the evaluator's `Machine` hooks (`crates/hir-ty-mir/src/mir/eval*`)

What: make `Evaluator` generic over a `Machine` and call it at every memory event, typed copy,
call, return and thread switch, exactly as `docs/v2/plan/lanes/safety/notes.md` section "The
evaluator's `Machine`" gives it: the trait text there (in a new
`crates/hir-ty-mir/src/mir/eval/machine.rs`, exported from `hir_ty_mir::mir`), `Plain` (a ZST
whose hooks are empty `#[inline(always)]` functions and whose `Prov` and `Shadow` are `()`,
the default type parameter, used by const eval and every existing caller), and the six numbered
call-site rules under it: `Ptr<M::Prov>` for addresses, a `M::Shadow` beside `Owned` bytes,
every program-memory access through `read`/`write`/`copy`, `alloc`/`free` per local (at
`StorageLive`/`StorageDead`) and per heap, static, constant and promoted block, `typed` after
each typed copy by the retag-mode table, `call`/`ret`, real threads (`pthread_create`, `join`,
futexes and `__ulock_*`/`os_sync_*`, `sched_yield`, one code stack and TLS per thread, the one
`schedule` picks run at each terminator), atomics through `Access::Atomic` with
`atomic_load`/`atomic_store`/`fence`, and `run_function::<M>(db, func, machine)` running a
`#[test]` fn and calling `finish`. The hooks return `Result<_, Stop>`; map `Stop` to
`MirEvalError::MachineStop` (requested of db-ty in `v2/requests/db-ty.md`, "From safety").
Why: `crates/hir-ty-ub` (lane safety's `check::Checker`) implements exactly this trait:
Tree Borrows, data races, bounds, init, provenance, leaks (docs/v2/plan/design/unsafe.md "Our own
Miri"). With `Plain` nothing changes and nothing is spent: every hook inlines to nothing, so
const eval's speed and results are unchanged (safety-check tests that they are).
If a part of the contract doesn't fit the evaluator, say so in `v2/requests/safety.md`
before writing around it; lane safety changes the trait and the checker together.

### S2. The LLVM back end: ASan, TSan, MSan and SanitizerCoverage (`crates/hir-ty-codegen/llvm.rs`, `vendor/llvm-sys/`)

What: `-Zsanitizer=address|thread|memory` (and `-Zsanitizer-recover`,
`-Zsanitizer-memory-track-origins`) and `-Csanitizer-coverage-level=N`,
`-Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp` (rustc's names) make the LLVM
back end instrument as rustc does: the function attributes `sanitize_address`,
`sanitize_thread`, `sanitize_memory` on every function not marked `#[no_sanitize]`/
`#[sanitize(..= "off")]`, and the passes in the pipeline where rustc's
`LLVMRustOptimize` puts them (`AddressSanitizerPass` with `ModuleAddressSanitizerPass`,
`ThreadSanitizerPass` with `ModuleThreadSanitizerPass`, `MemorySanitizerPass`,
`SanitizerCoveragePass` with the options above; `vendor/llvm-sys/wrappers/passes.cpp` names
them), plus `cfg(sanitize = "address")` etc. set for the crate. The runtime links:
`librustc-nightly_rt.asan.a` and the others from the pinned nightly's compiler-rt (or built
by blaze from compiler-rt's source, as std is: no rustc), with `-Wl,--whole-archive` on Linux
and the dylib on macOS as rustc links them. The Cranelift back end refuses these flags with an
error naming the LLVM back end (no-fallback); it keeps its bounds checks.
Why: docs/v2/plan/design/unsafe.md "Sanitizers from our code generator": every unsafe module's
tests run under ASan and TSan, and `fuzz/` (lane safety) is built with SanitizerCoverage and
ASan. Today `crates/blaze-cargo/src/unstable.rs:237` refuses `-Zsanitizer` and
`docs/support.md` says why; when this lands, cargo and the coordinator flip those (asked in
`v2/requests/cargo.md` and `v2/requests/safety.md`). The rustc differential: a test per
sanitizer that a known out-of-bounds, a data race and an uninitialized read are each reported
by the instrumented binary with the same report as rustc's build.

> taken by speed-backend (2026-10-29), with F2 of `v2/requests/safety.md`: child
> `speed-backend-sanitize` writes it in `llvm.rs`, `llvm/` and `vendor/llvm-sys/`; what it needs
> of `blaze-codegen`'s settings, the adapter's `check_spec` and cargo comes as requests.

> done by speed-backend-sanitize (2026-10-29): the LLVM side (attributes at `prelink`, the four
> passes in clang's order with rustc's options after the post-link pipeline in `compile`,
> SanitizerCoverage's arrays and PC table read back, `llvm.returnaddress`), not built or run
> (no builds in this round). rustc has no `-Csanitizer-coverage-level`/`-Zsanitizer-coverage`:
> its SanitizerCoverage is `-Cpasses=sancov-module` with `-Cllvm-args=-sanitizer-coverage-*`,
> which is what is taken. Waits on: the lead for `Spec::sanitize`, `CompiledFunction::coverage`
> and `Target::Coverage` (Z1, Z2 below; `hir-ty-codegen` does not build until they land);
> codegen-adapter for the Cranelift refusal, `Function::unsanitized`, its shape key and the
> `COVERAGE` relocations (Z3); db for the `#[sanitize]` query and items for its hash
> (`db.md`, `items.md`); speed-backend for `return_address` in `mod.rs`; cargo for the flags
> and `cfg(sanitize)` (`cargo.md`); link for the runtimes, MSan's options, ASan's globals and
> the coverage sections (`link.md`). The rustc differential (a reported out-of-bounds, race and
> uninitialized read) waits on all of these and on a VM to run in.

### S3. codegen-mir: `MemKind::Mmap` for anonymous `mmap` (`crates/hir-ty-mir/src/mir/eval/machine.rs`, 2026-10-29)

What:
- Add a variant `MemKind::Mmap`: memory from the shims' anonymous `mmap`, freed by `munmap`,
  which calls `free(.., MemKind::Mmap)`.
- `MemKind::Machine` keeps the rest of the shims' memory: errno cells, dispatch semaphores
  (freed by `dispatch_release` with `MemKind::Machine`), TLS, the arguments and the environment.
- In the same change, make two exact replacements in `crates/hir-ty-ub/src/check.rs` (lane
  safety's), so the checker's matches stay exhaustive:
  1. In `fn kind`: after `MemKind::Machine => Kind::Machine,` add `MemKind::Mmap => Kind::Mmap,`.
  2. In `Checker::alloc`: `MemKind::Stack | MemKind::Rust | MemKind::C => Some(self.thread),`
     becomes `MemKind::Stack | MemKind::Rust | MemKind::C | MemKind::Mmap => Some(self.thread),`.
- `Kind::Mmap` is already in `crates/hir-ty-ub/src/memory.rs`, with its test
  `munmap_frees_only_mmap_memory_and_neither_leaks`.

Why: this ports Miri's rule exactly. Miri has a memory kind of its own for `mmap`
(`MiriMemoryKind::Mmap`). Its race detector counts it as written by the allocating thread
(`data_race.rs`, `VClockAlloc::new_allocation`, the arm with `Rust | Miri | C | WinHeap |
WinLocal | Mmap`), while runtime memory such as errno is written at time zero. Its free checks
the kind, so `munmap` of anything that `mmap` didn't make is UB ("deallocating .., which is
machine-managed memory memory, using mmap deallocation operation"). With `mmap` folded into
`MemKind::Machine`, an unsynchronized `munmap` on another thread isn't reported as a race with
the mapping, and `munmap` of an errno cell or a semaphore passes. Both kinds may leak and
neither is a leak-check root, as in Miri. Plain needs no change: the hook takes the kind as data.

The other changes in your note need nothing from the checker:
- `free` of `MemKind::Machine`: `Memory::free` already accepts it when the kinds match.
- `Shadow::put` leaving the gap uninitialized: `Bytes::put` already does.
- The main thread's `at_thread_exit` destructors running before `finish`: `finish` exits the
  main thread in the race detector itself, then checks for threads left and leaks. The
  destructors' frees and accesses come first, as Miri's TLS destructors do.

## 7. cargo: `ItemHashes::new` takes a `Send + Sync` closure

Where: `crates/blaze-cargo/src/serve/unit/codegen.rs:260-265` (`names`).

What: `hir_ty_codegen::v2::ItemHashes::new` takes
`Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>` (`Hashes: Sync`, read from every
worker). The closure there captures `mounted` and `job.lowered.paths()`; both must be
`Send + Sync` (an `Arc` of an immutable map, not an `Rc` or a `RefCell`).

Why: otherwise the call doesn't type-check.

> forwarded to cargo: `serve/unit/codegen.rs` is lane cargo's.

> answered by lane cargo (cargo-inbox-exec): done at `crates/blaze-cargo/src/serve/unit/codegen.rs:261-266`: the closure captures an `Arc<Mounted>` and an `Arc<HashMap<FileId, String>>`, both `Send + Sync` (checked in `serve/unit/codegen/tests.rs`).

## 8. link: who builds the span tables `Target::Position` names

Where: lane link (`crates/blaze-link/src/position.rs` decodes a `SpanTable`; nothing in v2
produces one yet).

What: a `Target::Position(SpanRef { anchor, index })` from codegen names entry `index` of the
table `anchor`. `hir_ty_codegen::v2::ItemHashes::span_tables(db, owner)` lists every table of a
body by anchor, with the `MirSpan` at each index, and `ItemHashes::declaration_table(db, func)`
the one-entry table of a function's declaration. The position of each entry is
`hir_ty_mir::mir::location::source_location_in_file`; an entry it finds no syntax for (a
binding, the self parameter, `MirSpan::Unknown`) is its owner's position. Whoever feeds the
linker its span tables (the daemon's link step, lane cargo or link) should build them from
these, keyed by anchor, so a reformat rebuilds tables and compiles nothing.

Why: without them every panic location and `#[track_caller]` `Location` points at nothing.

> forwarded to link: lane link's call.

## 9. items (optional, speed): spans of a body form and a by-move code query

Where: `crates/blaze-items/src/hash.rs`.

What: `ItemHashes::code` rebuilds each body's span ids from `mir_body`, `mir_body_for_closure`
and `mir_body_for_coroutine_by_move` and hashes them beside `code_hash`/`closure_code`/
`coroutine_code`, and has no by-move code query. A code hash that already covers the span ids
the MIR names, and a `coroutine_by_move_code` query, would hash each body once in one place.

Why: hash once (principles.md); today the MIR's spans are walked twice per body.

> forwarded to coordinator: lane items is DONE and this is a restructure of its hashing (`hash.rs`) with a new code query, optional: new work for a lane the coordinator names, not an edit.

## From speed-backend-code (2026-10-29): `blaze_codegen::Reloc` in 16 bytes (lead)

`crates/blaze-codegen/src/lib.rs:176`: `Reloc { at: u32, kind: RelocKind, target: Target,
addend: i64 }` holds its `Target` inline (`InstanceKey`, `ContentId`, `ItemKey` + `ContentId`,
or an owned `String`), so a relocation is far over data.md's 16 bytes and each `Extern` one
owns a heap string. `hir-ty-codegen`'s own `fragment::Relocs` is now 16-byte rows; the
conversion in `v2/convert.rs` widens every one back. Exact layout asked:

```rust
/// 16 bytes (const-asserted): read in place, sorted by `at`.
#[repr(C)]
pub struct Reloc {
    pub at: u32,
    /// Index into `CompiledFunction::targets`, each distinct target once.
    pub target: u32,
    /// The addend, or `i32::MIN` for one kept in `CompiledFunction::wide` by this row's index.
    pub addend: i32,
    pub kind: RelocKind, // #[repr(u8)]
    pad: [u8; 3],
}
pub struct CompiledFunction { /* ... */ pub relocs: Vec<Reloc>, pub targets: Vec<Target>, pub wide: Vec<(u32, i64)>, /* ... */ }
```

The serialized form changes, so the function schema's version and every key over it bump. A
call site per target is then 16 bytes and a lookup an index; `targets` dedups the dozen calls
of one callee a function has. Not blocking: `hir-ty-codegen` builds without it.

> forwarded to codegen: `blaze_codegen::Reloc` is lane codegen's.

> applied by codegen-v2 (2026-10-29): `blaze_codegen::Reloc` is the 16-byte row asked (const-asserted, `crates/blaze-codegen/src/relocs.rs`), and every relocation list is a `Relocs<T>` table of rows sorted by `at`, distinct targets in first-use order and wide addends by row (`CompiledFunction::relocs`, `Data::relocs`, `Init::Bytes`, and `Relocs<Ref>` in templates); the old four-field struct is `blaze_codegen::Relocation`, what a table is built from and yields. Hashes and blobs frame the rows, the targets once each and the wide addends, so domains and keys moved: `blaze v2 data 2`, `blaze v2 code 3`, `blaze v2 template 3`, blob magic `BZT4`/`BZS2`, store keys `template`/`release template`/`folded`/`thunk`/`static` to 2. Every site in blaze-codegen and `hir-ty-codegen/v2` is converted; the sites in other lanes' crates are requested below.

### To link: `crates/blaze-link` onto `Relocs` (from codegen-v2, 2026-10-29)

Exact before/after for each site: `v2/requests/codegen-v2-relocs-link.md`. In short:
- `src/stage.rs:18` import `Relocs` for `Reloc`; add `static NO_RELOCS: Relocs = Relocs::new();` after `CIE_DOMAIN` (line 35).
- `src/stage.rs:183-205` `function_keys`/`key_count`, `:229-236` `reloc_syms`, `:568-587` `fixups`, `:592-603` `add_function`, `:785-787` `add_static`: intern each of `relocs.targets()` once and index the symbols by `row.target`; slices of `syms` are measured by `targets().len()`. Same fixups (now in offset order, which `write::split` already wants).
- `src/tests.rs:7,57,63,73-74,569` and `src/intern.rs:351,499` (tests): `blaze_codegen::Relocation` by path (clashes with `object::write::Relocation`), `relocs: relocs.into()` / `Relocs::new()`.
- Already broken at HEAD, not by this change: the test literals of `CompiledFunction` lack `coverage: None` (`tests.rs:58`, `intern.rs:494`), and `stage.rs`'s `wide` lacks the `Target::Coverage` arm asked in "From codegen (lead): what Z1, Z2 ..." below.

### To fastcompile: `crates/blaze-premap` and `crates/blaze-lazy` onto `Relocs` (from codegen-v2, 2026-10-29)

Exact before/after for each site: `v2/requests/codegen-v2-relocs-fastcompile.md`. In short:
- `blaze-premap/src/key.rs:19` import, `:326-332` `relocs_into(&Relocs)` iterating `Relocation` (same bytes per relocation; `STATIC_DOMAIN` unchanged unless the faster rows-then-targets framing is taken, then `"blaze v2 premap static 2"`).
- `blaze-premap/src/split.rs:151-156` and `blaze-lazy/src/plan.rs:218-227`: a static's own relocations as `Option<&Relocs>` flattened (no `as_slice`); `Some(*key)` in `named_functions`.
- Tests: `blaze-premap/src/tests.rs:3,43,49,58-60,80,336`, `blaze-lazy/src/tests.rs:15,73,78,87`, `blaze-lazy/src/tests/plan.rs:4,15,112-113`: `Relocation` literals (by path where `settings::Relocation` is imported), `relocs.into()`, `Relocs::new()`.
- Already broken at HEAD: `coverage: None` missing in `blaze-premap/src/tests.rs:44` and `blaze-lazy/src/tests.rs:82`; `key.rs` `target_into`'s `Target::Coverage` arm is asked below.
- `blaze-global`, `blaze-stencil`, `blaze-cargo` and `blaze-api` need no change.

## From safety-audit

From `safety-audit-linkgen` (child `-cg` for hir-ty-codegen). Every file below was BUSY (owners
`codegen`, `codegen-mir`, `codegen-inline`, `speed-backend`, `fastcompile-stencil`,
`fastcompile-ident-share`, `cargo-std`), so nothing was edited: these are comments, `# Safety`
sections, block splits and crate-root attributes only, no behavior change, unless a line says
MAY BE UNSOUND. Full reasoning: `/tmp/blaze-lanes/v2-safety-audit-linkgen.md` and
`-linkgen-cg.md`.

> forwarded to codegen: every file named is lane codegen's (or its children's), still running.

### blaze-codegen
- `crates/blaze-codegen/src/memo.rs:183` block: no SAFETY. Proposed: `// SAFETY: a non-null slot holds a \`Box<Entry>\` that \`put\` leaked and published once; \`&mut self\` means no reader holds it, and the slot was nulled just above, so this is its only owner (else a double free).`
  > applied by codegen-v2 (2026-10-29): the comment as proposed, above `drain`'s `Box::from_raw`; every other unsafe block of the memo already had one.

### blaze-engine
- `crates/blaze-engine/src/lib.rs:7968` block: 3 ops (`getpgrp`, `getpid`, `killpg`). Split, with `#[cfg(unix)]` on a plain block and the same evaluation order: `#[cfg(unix)] {` / `// SAFETY: \`getpgrp\` takes no pointer and cannot fail.` `let group = unsafe { libc::getpgrp() };` / `// SAFETY: \`getpid\` takes no pointer and cannot fail.` `let me = unsafe { libc::getpid() };` / `if group == me {` `// SAFETY: this process leads its group (checked just above), which the daemon made for this worker alone, so \`killpg(0, ..)\` kills only what the worker started, and itself.` `unsafe { libc::killpg(0, libc::SIGKILL) }; } }`
- `crates/blaze-engine/src/live.rs:212` block: 2 ops (`SetEvent`, `CloseHandle`). Split: `// SAFETY: \`event\` is the non-null handle \`OpenEventW\` opened with \`EVENT_MODIFY_STATE\`, the right \`SetEvent\` needs; it is closed only below.` `unsafe { SetEvent(event) };` then `// SAFETY: \`event\` is this function's own open handle, closed once here and not used after.` `unsafe { CloseHandle(event) };`

> applied by codegen-engine (2026-10-29): both blocks split as written (`end_with_input`'s
> `killpg` block, now near `lib.rs:7985`, and `live.rs` `wake`).

### hir-ty-mir
- `crates/hir-ty-mir/src/lib.rs:1` crate: no unsafe; add `#![forbid(unsafe_code)]` (after the `//!` docs). Its only `unsafe` matches are a field pattern (`unsafe_: _`, `mir/lower.rs:1024`) and two strings (`mir/type_name.rs:483`, `mir/eval/shim/reflection.rs:601`). The `unsafe impl`s that `#[salsa::tracked]`/`#[salsa::input]` expand to come from `salsa-macro-rules`' external `macro_rules!`, which `unsafe_code` does not report; `blaze-items` and `hir-ty-borrowck` already pair `forbid(unsafe_code)` with tracked fns. `src/lib.rs` is the only target root.
  > not applied, by codegen-mir-store (2026-10-29): the crate has an `unsafe` block now,
  > `mir/eval/hooks.rs:37` (`Hooks::with`, `&mut *self.0.get()` on an `UnsafeCell`, with its
  > `// SAFETY:`; codegen-mir's S1), so `#![forbid(unsafe_code)]` would not compile. The rule
  > gives forbid to a crate with no unsafe module; this one has one. If `hooks.rs` goes back to
  > safe code (a `RefCell`), the attribute goes in after the `//!` docs of `src/lib.rs`.

### hir-ty-codegen

The crate's unsafe is LLVM-C/inkwell FFI plus `backend.rs:527`, `mod.rs:8297`/`8301` and
`fncache/dispatch.rs:57,59`; it has no JIT entries. Line numbers as of 2026-10-29 18:30.

- `crates/hir-ty-codegen/build.rs:1` crate: build-script target root with no unsafe; add `#![forbid(unsafe_code)]` after its `//!` docs.
  > done by speed-backend (2026-10-29).
- `crates/hir-ty-codegen/mod.rs:8301` MAY BE UNSOUND: `Lent::alloc` takes `at` from `&*value` before moving `value: Box<_>` into `push`; under Stacked Borrows the by-value move retags the Box unique and invalidates `at` (Tree Borrows accepts it; `Arc` is fine). Proposed (code change, owner's call): `let mut list = self.0.borrow_mut(); list.push(value); let at: *const P::Target = &**list.last().expect("just pushed"); drop(list);` then the same `unsafe { &*at }`.
  > done by speed-backend (2026-10-29): as proposed, and `Lent` moved to its own module `crates/hir-ty-codegen/lent.rs` (safe API; `Stable` is now an `unsafe trait` with documented impls for `Arc` and `Box`; a test against a safe list read by index).
- `crates/hir-ty-codegen/mod.rs:149` block: weak SAFETY ("as for the name."). Proposed: `// SAFETY: LLVMGetHostCPUFeatures only reads the host CPU and needs no LLVM state set up first; it returns a string LLVM allocated, which this code now owns and passes to \`owned\` once.`
  > done by speed-backend (2026-10-29).
- `crates/hir-ty-codegen/mod.rs:151` block: weak SAFETY ("as for the name."). Proposed: `// SAFETY: \`features\` is the NUL-terminated string LLVMGetHostCPUFeatures just allocated, owned here and given to \`owned\` once, which disposes of it; a second use would read freed memory.`
  > done by speed-backend (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:420` block: weak SAFETY (no owner, nor where upheld). Proposed: `// SAFETY: \`function\` is owned by \`module\`, and \`replace_all_uses_with\` just above left no use of it; \`delete\` erases and frees it, and this handle is not used after. A use left behind would point at freed memory.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:608` block: weak SAFETY. Proposed: `// SAFETY: \`function\` is owned by \`imported\` and is not \`own\` (filtered out above); \`replace_all_uses_with(own)\` just above left no use of it, and neither \`names\` nor the loop keeps it, so freeing it leaves nothing dangling.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:665` block: weak SAFETY. Proposed: `// SAFETY: \`instruction\` is a call or an invoke (opcode checked just above) owned by \`module\`, which outlives this walk; \`CallSiteValue\` on anything else would read the wrong operand layout.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:668` block: weak SAFETY. Proposed: `// SAFETY: \`instruction\` is a live call or invoke of \`module\`, so it has a called operand, owned by \`module\` for this walk. Not \`get_called_fn_value\`, which has no function for a call of another type.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:669` block: no SAFETY (it is in the \`if\` condition). Proposed above the \`if\`: `// SAFETY: \`callee\` is the called operand just read, a value of \`module\` live for this walk; LLVMIsAFunction only reads its kind.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:673` block: weak SAFETY ("it is a function."). Proposed: `// SAFETY: LLVMIsAFunction just above said \`callee\` is a function of \`module\`, live for \`'ctx\`; a non-function would make inkwell call function-only C APIs on it.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:678` block: 2 ops (`LLVMGetCalledFunctionType`, `FunctionType::new`). Split: `let called = unsafe { core::LLVMGetCalledFunctionType(instruction.as_value_ref()) };` (SAFETY: `// SAFETY: \`instruction\` is a live call or invoke of \`module\`; the type it is called through is owned by \`context\` and lives for \`'ctx\`.`) then `let through = unsafe { FunctionType::new(called) };` (SAFETY: `// SAFETY: \`called\` is a function type of \`context\`, which outlives \`'ctx\`; a non-function type would make \`FunctionType\` lie about its kind.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:709` block: 4 ops (`LLVMGetNormalDest`, `BasicBlock::new`, `LLVMGetUnwindDest`, `BasicBlock::new`). Split, keeping the order: `let at = instruction.as_value_ref();` then `let then = unsafe { core::LLVMGetNormalDest(at) };` (SAFETY: `// SAFETY: \`at\` is an invoke (this arm of the opcode match) of \`module\`, live for this walk, and an invoke always has a normal destination.`), `let then = unsafe { BasicBlock::new(then) };` (SAFETY: `// SAFETY: \`then\` is a block of \`function\`, owned by \`module\` for \`'ctx\`; \`BasicBlock::new\` maps null to \`None\`.`), `let catch = unsafe { core::LLVMGetUnwindDest(at) };` (SAFETY: `// SAFETY: \`at\` is a live invoke of \`module\`, which always has an unwind destination.`), `let catch = unsafe { BasicBlock::new(catch) };` (SAFETY: `// SAFETY: \`catch\` is a block of \`function\`, owned by \`module\` for \`'ctx\`; null maps to \`None\`.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:743` block: weak SAFETY ("both are instructions."). Proposed: `// SAFETY: \`made\` is the call or invoke just built into \`module\`, an instruction owned by it for \`'ctx\`.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:744` block: no SAFETY, 2 ops (`LLVMInstructionGetDebugLoc`, `LLVMInstructionSetDebugLoc`). Split: `let location = unsafe { debuginfo::LLVMInstructionGetDebugLoc(instruction.as_value_ref()) };` (SAFETY: `// SAFETY: \`instruction\` is a live instruction of \`module\`; its location is metadata owned by \`context\`, which outlives the module.`) then `if !location.is_null() { unsafe { debuginfo::LLVMInstructionSetDebugLoc(made.as_value_ref(), location) }; }` (SAFETY above the inner block: `// SAFETY: \`made\` is the instruction just built in \`module\`, and \`location\` non-null metadata of the same context, which outlives both.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:778` block: weak SAFETY. Proposed: `// SAFETY: \`body\` is owned by \`module\`, and \`replace_all_uses_with\` just above sent every use of its address to \`function\`; \`shown\` is not read after \`unshow\`, so freeing it leaves nothing dangling.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:927` block: 4 ops (`LLVMGetDiagInfoDescription`, `CStr::from_ptr`, `LLVMDisposeMessage`, `LLVMGetDiagInfoSeverity`). Split, keeping the order: `let message = unsafe { llvm::LLVMGetDiagInfoDescription(info) };` (SAFETY: `// SAFETY: LLVM passes \`info\`, a diagnostic it owns that lives through this call; the description is a new message this handler owns.`), `let text = unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy().into_owned();` (SAFETY: `// SAFETY: \`message\` is the NUL-terminated string LLVM just allocated, live until disposed of below.`), `unsafe { llvm::LLVMDisposeMessage(message) };` (SAFETY: `// SAFETY: \`message\` was allocated by LLVM for this handler and is not read after this.`), `let severity = unsafe { llvm::LLVMGetDiagInfoSeverity(info) };` (SAFETY: `// SAFETY: \`info\` lives through this call.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:941` block: weak SAFETY. Proposed: `// SAFETY: \`context\` was just created and is owned here; \`handle\` is a \`'static\` function that takes no context pointer, so the null one it is given is never read.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:960` block: weak SAFETY. Proposed: `// SAFETY: \`machine\` is a live target machine; \`Machinery\` declares \`passes\` before \`machine\`, so this object is dropped before the machine it points to. Dropped after, its pipelines would use a freed machine.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:971` block: weak SAFETY. Proposed: `// SAFETY: \`self.0\` is live until \`Drop\`, and \`Kept\` is \`!Send\` (a \`NonNull\`) inside a thread-local \`Machine\`, so one thread uses it; \`module\` is live for the call and the wrapper refuses one of another context (\`vendor/llvm-sys/wrappers/passes.cpp\`); \`passes\` is read for \`passes.len()\` bytes during the call only.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:984` block: 3 ops (`LLVMGetErrorMessage`, `CStr::from_ptr`, `LLVMDisposeErrorMessage`). Split (outer binding stays the \`String\` \`message\`): `let raw = unsafe { LLVMGetErrorMessage(failed) };` (SAFETY: `// SAFETY: \`failed\` is the non-null error LLVMBlazeRunPasses just returned, owned here; LLVMGetErrorMessage consumes it and returns a message this code owns.`), `let message = unsafe { std::ffi::CStr::from_ptr(raw) }.to_string_lossy().into_owned();` (SAFETY: `// SAFETY: \`raw\` is NUL-terminated and live until disposed of below.`), `unsafe { LLVMDisposeErrorMessage(raw) };` (SAFETY: `// SAFETY: \`raw\` came from LLVMGetErrorMessage, is disposed of once and not read after.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:997` block: weak SAFETY. Proposed: `// SAFETY: \`self.0\` was made by \`LLVMBlazeCreatePasses\` and \`Kept\` is not \`Clone\`, so this is its one disposal; the machine it was made for is still live (field order of \`Machinery\`).`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm.rs:1213` block: weak SAFETY. Proposed: `// SAFETY: \`address\` is a global of \`module\` and the index an \`i64\` constant of \`context\`, so this builds a constant expression owned by \`context\`; not \`inbounds\`, so any \`addend\` is defined.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/lower.rs:200` block: 2 ops (`LLVMValueAsMetadata`, `LLVMGlobalSetMetadata`). Split: `let node = unsafe { core::LLVMValueAsMetadata(node.as_value_ref()) };` (SAFETY: `// SAFETY: \`node\` is a metadata node of \`context\`, which owns it for \`'ctx\`; this only views it as metadata.`) then `unsafe { core::LLVMGlobalSetMetadata(function.as_value_ref(), context.get_kind_id("prof"), node) };` (SAFETY: `// SAFETY: \`function\` is a global of a module of \`context\` and \`node\` metadata of the same context, both live for \`'ctx\`; metadata of another context would corrupt LLVM's uniquing.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/lower.rs:1106` block: weak SAFETY. Proposed: `// SAFETY: \`base\` and \`by\` are values of this function's module, live for \`'ctx\`, and the GEP is not \`inbounds\`, so any offset is defined (no poison).`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/lower.rs:1708` block: weak SAFETY (does not say where upheld). Proposed above \`let address\`: `// SAFETY: only an \`iadd\` marked by \`unwind::seh::in_bounds\` gets here, and \`mod.rs\` marks only the add of the \`offset\` intrinsic, whose result Rust requires to stay within \`base\`'s allocation, as rustc emits it \`inbounds\`; otherwise the result would be poison.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/lower.rs:1984` block: weak SAFETY. Proposed: `// SAFETY: \`base\` and \`offset\` are values of this module, live for \`'ctx\`; the GEP is not \`inbounds\`, so any offset is defined, and the wrapper's offsets are within the slot.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:99` unsafe fn `rewrite`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`inttoptr\` is a live \`inttoptr\` instruction of a module nothing else uses while this runs, and \`builder\` and \`byte\` (\`i8\`) are of that module's context.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:119` unsafe fn `add`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`value\` is a live value of the module the web is of, and \`pointer\` a type of its context.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:152` unsafe fn `ordered`: no `# Safety`. Proposed: `/// # Safety\n///\n/// Every value of the web is still a live value of its module.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:178` unsafe fn `node`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`value\` is a live value of a module, and \`pointer\` a type of its context.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:213` unsafe fn `sum`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`value\` is a live \`add\` or \`or\` (an instruction or a constant expression), which has two operands.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:226` unsafe fn `may_be_address`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`value\` is a live value of a module.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:260` unsafe fn `build`: no `# Safety`. Proposed: `/// # Safety\n///\n/// The web was just built by [\`Web::add\`] from \`root\`, the operand of \`inttoptr\`, and passed [\`Web::ordered\`]; every value of it and \`inttoptr\` are live; \`builder\`, \`byte\` and \`pointer\` are of the module's context. \`inttoptr\` is erased here.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:338` unsafe fn `first_after_phis`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`block\` is a live block of a well-formed function, so it ends in a terminator and the walk stops before running off its end.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs` comment texts used by the eight split lines below. Each split puts one op per `unsafe { }`, binding each result to a `let` in the original order; an op in an `if`/`while`/`match`-guard condition becomes `unsafe { op }` in place, with its comment above that line.
  - (R) a read of a value, instruction, block or function of the module: `// SAFETY: \`<v>\` is a live value of the module being rewritten, which owns it and outlives this rewrite; this only reads it.`
  - (B) a builder call: `// SAFETY: \`builder\` was made in the module's context and is disposed of only at the end of \`rebase\`; the position and the operands are live values of the module, of the types the instruction takes.`
  - (F) a call of this file's unsafe fns: `// SAFETY: the values are live values of the module and the types of its context, as \`<fn>\`'s \`# Safety\` asks.`
  - (E) an `LLVMReplaceAllUsesWith`: `// SAFETY: both are live values of the module of one type (\`<a>\` and its pointer, or its \`ptrtoint\`).`
  - (X) the `LLVMInstructionEraseFromParent`: `// SAFETY: \`inttoptr\` has no use left (replaced just above), and the walk in \`rebase\` already took the instruction after it, so nothing reads it again.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:42` block: 12 ops (`LLVMGetModuleContext`, `LLVMCreateBuilderInContext`, `LLVMInt8TypeInContext`, `LLVMGetFirstFunction`, `LLVMGetFirstBasicBlock`, `LLVMGetFirstInstruction`, `LLVMGetNextInstruction`, `LLVMGetInstructionOpcode`, `Web::rewrite`, `LLVMGetNextBasicBlock`, `LLVMGetNextFunction`, `LLVMDisposeBuilder`). Split one per block: the getters with (R) (`module` / `function` / `block` / `inst` live), `LLVMCreateBuilderInContext` with `// SAFETY: \`context\` is \`module\`'s, live for the call; the builder is owned here and disposed of at the end of this function.`, `Web::rewrite` with (F) plus `\`inst\` is an \`inttoptr\` (checked on the left of \`&&\`) and \`next\` is already taken, so erasing \`inst\` does not break the walk`, `LLVMDisposeBuilder` with `// SAFETY: \`builder\` was made above, is disposed of once, and not used after.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:100` block: no SAFETY, 6 ops (`LLVMTypeOf`, `LLVMGetOperand`, `LLVMTypeOf`, `web.add`, `web.ordered`, `web.build`). Split one per block: the three LLVM reads with (R) (`inttoptr` live, and an `inttoptr` has operand 0), `web.add`/`web.ordered`/`web.build` with (F) (`web.add(..)` in the \`if\` condition becomes `unsafe { web.add(root, pointer) }.is_none()`, and `!web.ordered()` becomes `!unsafe { web.ordered() }`, keeping the \`||\` short-circuits).
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:120` block: no SAFETY, 10 ops (`LLVMTypeOf`, `Self::node`, `LLVMCountIncoming`, `LLVMGetIncomingValue`, `self.add`, `LLVMGetOperand`, `self.add`, `LLVMGetOperand`, `self.add`, `self.add`). Split one per block: reads with (R) (a phi has `LLVMCountIncoming` incoming values; a select has operands 1 and 2), `node`/`add` with (F); `LLVMTypeOf(value) != self.int` stays right of the `||` as `unsafe { LLVMTypeOf(value) } != self.int`.
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:153` block: no SAFETY, 2 ops (`LLVMGetOperand` x2 in the `Node::Select` arm). Split: `let then = unsafe { LLVMGetOperand(value, 1) };` and `let otherwise = unsafe { LLVMGetOperand(value, 2) };`, each with (R) plus `a select has three operands`, then `[then, otherwise].iter().all(..)`.
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:179` block: no SAFETY, 13 ops (`LLVMIsAConstantInt`, `LLVMIsNull`, `LLVMIsAInstruction`, `LLVMGetInstructionOpcode`, `LLVMIsAConstantExpr`, `LLVMGetConstOpcode`, `LLVMGetOperand`, `LLVMTypeOf`, `LLVMIsAInstruction` (select guard), `Self::sum`, `LLVMIsAInstruction` (or guard), `LLVMGetIsDisjoint`, `Self::sum`). Split one per block in place, all reads (R), `sum` (F) with `\`value\` is an \`add\` or a disjoint \`or\` (opcode matched)`; `LLVMGetIsDisjoint` with `// SAFETY: \`value\` is a live \`or\` instruction (both checked left of \`&&\`), which has the disjoint flag.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:214` block: no SAFETY, 4 ops (`LLVMGetOperand` x2, `may_be_address` x2). Split: `let a = unsafe { LLVMGetOperand(value, 0) };`, `let b = unsafe { LLVMGetOperand(value, 1) };` (R, `an add or or has two operands`), then `match (unsafe { Self::may_be_address(a) }, unsafe { Self::may_be_address(b) })` with (F) above the `match`.
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:227` block: no SAFETY, 10 ops (`LLVMIsAInstruction`, `LLVMGetInstructionOpcode`, `LLVMIsAConstantExpr`, `LLVMGetConstOpcode`, `LLVMGetOperand` x2, and in the closure `LLVMIsAInstruction`, `LLVMGetInstructionOpcode`, `LLVMIsAConstantExpr`, `LLVMGetConstOpcode`). Split one per block in place, all (R); the two operands as in `sum`.
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:268` block: no SAFETY, 26 ops (`LLVMPositionBuilderBefore`, `LLVMBuildPhi`, `LLVMConstNull`, `LLVMPositionBuilderBefore`, `LLVMGetOperand` x2, `LLVMBuildSelect`, `LLVMGetOperand`(0), `LLVMIsAInstruction` (guard), `LLVMConstGEP2`, `LLVMPositionBuilderBefore`, `LLVMBuildGEP2`, `LLVMCountIncoming`, `LLVMGetIncomingValue`, `LLVMGetIncomingBlock`, `LLVMAddIncoming`, `LLVMReplaceAllUsesWith`, `LLVMInstructionEraseFromParent`, `LLVMIsAInstruction`, `first_after_phis`, `LLVMGetInstructionParent`, `LLVMGetNextInstruction`, `LLVMPositionBuilderBefore`, `LLVMBuildPtrToInt`, `LLVMTypeOf`, `LLVMReplaceAllUsesWith`). Split one per block, keeping argument order: bind `LLVMGetOperand(value, 0)` to a `let` before `LLVMBuildSelect` (it was its second argument, after `builder`, which has no side effect); `LLVMConstNull`/`LLVMConstGEP2` with `// SAFETY: \`pointer\`/\`byte\` are types of the module's context and \`pointers[&based]\` a constant pointer of it; the constant is owned by the context.`; builder calls (B); `LLVMAddIncoming` with `// SAFETY: \`pointers[&phi]\` is the pointer phi built above and \`incoming\`/\`from\` one live value and block, passed as arrays of length 1.`; `first_after_phis` (F); RAUWs (E); erase (X); the reads (R).
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/rebase.rs:339` block: no SAFETY, 3 ops (`LLVMGetFirstInstruction`, `LLVMGetInstructionOpcode`, `LLVMGetNextInstruction`). Split: `let mut inst = unsafe { LLVMGetFirstInstruction(block) };`, `while matches!(unsafe { LLVMGetInstructionOpcode(inst) }, ..)`, `inst = unsafe { LLVMGetNextInstruction(inst) };`, each with `// SAFETY: \`block\` (and so \`inst\`) is live and ends in a terminator, as the \`# Safety\` asks, so \`inst\` is never null here.`
  > done by speed-backend-safety-rebase (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:297` block: weak SAFETY. Proposed: `// SAFETY: \`address\` is a global of \`module\` and the index an \`i64\` constant of \`context\`, so this builds a constant expression owned by \`context\`; not \`inbounds\`, so any \`addend\` is defined.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:314` block: weak SAFETY. Proposed: `// SAFETY: \`old\` is owned by \`module\`, and \`replace_all_uses_with\` just above sent every use to \`told\`; \`old\` is not used after, so freeing it leaves nothing dangling.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:369` block: no SAFETY, 5 ops (`LLVMIsACallInst`, `LLVMIsAInvokeInst`, `LLVMGetCalledValue`, `LLVMGetNumArgOperands`, `LLVMGetOperand`). Split (same short-circuits, as early returns): `// SAFETY: \`user\` is a live value of one module, as this fn's \`# Safety\` says; these only read its kind.` above `let call = !unsafe { core::LLVMIsACallInst(user) }.is_null() || !unsafe { core::LLVMIsAInvokeInst(user) }.is_null();`, then `if !call { return false; }`, then `// SAFETY: \`user\` is a live call or invoke (just checked), which has a called operand.` above `if unsafe { core::LLVMGetCalledValue(user) } != function { return false; }`, then `// SAFETY: \`user\` is a live call or invoke, whose first operands are its arguments.` above `let args = unsafe { core::LLVMGetNumArgOperands(user) };`, then `// SAFETY: \`at\` is below \`args\`, so an argument operand of the live \`user\`.` above `(0..args).all(|at| unsafe { core::LLVMGetOperand(user, at) } != function)`.
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:381` block: 4 ops (`LLVMGetFirstUse`, `LLVMGetUser`, `only_calls`, `LLVMGetNextUse`). Split: `let mut used = unsafe { core::LLVMGetFirstUse(target) };` (SAFETY: `// SAFETY: \`target\` is a live function of its module, which owns its use list; nothing changes it during this walk.`), `let user = unsafe { core::LLVMGetUser(used) };` (SAFETY: `// SAFETY: \`used\` is a non-null use of \`target\`, live during the walk.`), `if unsafe { only_calls(user, target) } && !calls.contains(&user)` (SAFETY: `// SAFETY: \`user\` and \`target\` are live values of one module, as \`only_calls\` asks.`), `used = unsafe { core::LLVMGetNextUse(used) };` (SAFETY: `// SAFETY: \`used\` is a live use of \`target\`; the list is unchanged during the walk.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:395` MAY BE UNSOUND: safe `fn arg_of(value: LLVMValueRef)` hands any raw pointer to LLVM; its only caller (`:442`) passes a live operand, but the signature cannot require it. Proposed (signature change, owner's call): `unsafe fn arg_of` with `/// # Safety\n///\n/// \`value\` is a live value of a module.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:397` block: 4 ops (`LLVMIsAConstantInt`, `LLVMTypeOf`, `LLVMGetIntTypeWidth`, `LLVMConstIntGetZExtValue`). Split: `if unsafe { core::LLVMIsAConstantInt(value) }.is_null() { return Arg::Unknown; }` (SAFETY: `// SAFETY: \`value\` is a live value (the caller's operand of a live instruction); this reads its kind.`), `let ty = unsafe { core::LLVMTypeOf(value) };` (SAFETY: `// SAFETY: \`value\` is live; its type is owned by its context.`), `let bits = unsafe { core::LLVMGetIntTypeWidth(ty) };` (SAFETY: `// SAFETY: \`ty\` is the integer type of a constant int.`), and in the arm `value: unsafe { core::LLVMConstIntGetZExtValue(value) }` (SAFETY above it: `// SAFETY: \`value\` is a constant int of at most 64 bits (checked by the arm).`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:420` block: weak SAFETY. Proposed: `// SAFETY: \`param\` is a parameter of \`function\`, owned by \`module\` for \`'ctx\`; this only reads whether its use list is empty.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:433` block: weak SAFETY. Proposed: `// SAFETY: \`at\` is a live call or invoke of \`module\` (opcode checked above), so it has a called operand, owned by \`module\` for \`'ctx\`; it may be any value.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:434` block: no SAFETY (in the \`if\` condition). Proposed above the \`if\`: `// SAFETY: \`callee\` is the called operand just read, a value of \`module\` live for \`'ctx\`; LLVMIsAFunction only reads its kind.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:438` block: weak SAFETY ("it is a function."). Proposed: `// SAFETY: LLVMIsAFunction just above said \`callee\` is a function of \`module\`, live for \`'ctx\`.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:441` block: 2 ops (`LLVMGetNumArgOperands`, `LLVMGetOperand`). Split: `let count = unsafe { core::LLVMGetNumArgOperands(at) };` (SAFETY: `// SAFETY: \`at\` is a live call or invoke of \`module\`, whose first operands are its arguments.`) then `let args: Vec<Arg> = (0..count).map(|it| arg_of(unsafe { core::LLVMGetOperand(at, it) })).collect();` (SAFETY above it: `// SAFETY: \`it\` is below \`count\`, so an argument operand of the live \`at\`.`; if \`arg_of\` becomes \`unsafe\`, give its call its own block: `// SAFETY: the operand is a live value of \`module\`, as \`arg_of\` asks.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:466` block: 4 ops (`LLVMGetFirstUse`, `LLVMGetUser`, `only_calls`, `LLVMGetNextUse`). Split as `:381`, inside `let taken = { ...; taken };`: `let mut used = unsafe { core::LLVMGetFirstUse(target) };`, `let user = unsafe { core::LLVMGetUser(used) };`, `taken = !unsafe { only_calls(user, target) };`, `used = unsafe { core::LLVMGetNextUse(used) };`, with the four SAFETY texts of `:381` (\`module\` for "its module").
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:601` block: weak SAFETY. Proposed: `// SAFETY: \`param\` is a parameter of \`function\` and \`constant\` a constant of \`context\`, both live for \`'ctx\` and of one type (matched just above); a different type would break the IR's typing.`
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/llvm/release.rs:649` block: 6 ops (`LLVMGetNumArgOperands`, `LLVMGetOperand`, `LLVMTypeOf`, `LLVMGetPoison`, `LLVMSetOperand`, `LLVMRemoveCallSiteEnumAttribute`). Split, keeping the order: `let args = unsafe { core::LLVMGetNumArgOperands(call) };` (SAFETY: `// SAFETY: \`call\` is a live call or invoke of \`module\` (from \`calls_of\`), changed below only in its operands and attributes.`), then in the loop `let arg = unsafe { core::LLVMGetOperand(call, at) };` (SAFETY: `// SAFETY: \`at\` is below \`args\`, so an argument operand of \`call\`.`), `let ty = unsafe { core::LLVMTypeOf(arg) };` (SAFETY: `// SAFETY: \`arg\` is live; its type is owned by the context.`), `let poison = unsafe { core::LLVMGetPoison(ty) };` (SAFETY: `// SAFETY: \`ty\` is a first-class type of the module's context, which owns the poison constant.`), `unsafe { core::LLVMSetOperand(call, at, poison) };` (SAFETY: `// SAFETY: \`poison\` has the type of the argument it replaces, so the call stays well typed; \`at\` is an argument operand of \`call\`.`), and in the inner loop `unsafe { core::LLVMRemoveCallSiteEnumAttribute(call, at + 1, attribute_kind(name)) };` (SAFETY: `// SAFETY: \`at + 1\` is the attribute index of argument \`at\` of the live \`call\`.`).
  > done by speed-backend-safety-llvm (2026-10-29).
- `crates/hir-ty-codegen/fncache/dispatch.rs:59` block: no SAFETY (in the \`while let\` loop). Proposed: `// SAFETY: \`it\`'s module is only used within this call to find the next outer module, and none is stored or returned (\`owners\` returns body ids), so no \`ModuleId\` leaks out of the query, as \`ModuleIdLt::to_static\` requires.`
  > done by `speed-backend-code` (2026-10-29, see `v2/requests/safety.md`): both `dispatch.rs` blocks (57, 59 then; now in `why_not`) carry a SAFETY comment naming `to_static`'s invariant; checked by safety-audit-linkgen, nothing left.

> applied by codegen-v1 (2026-10-29): checked, nothing left for hir-ty-codegen or `vendor/llvm-sys/wrappers/passes.cpp`: every line above is in the code (`build.rs` forbids unsafe, `Lent` is in `lent.rs`, `arg_of` is `unsafe fn`), and `LLVMBlazeRunPasses` refuses a module of another context as `llvm.rs`'s SAFETY says.

## From requests-nodes (2026-10-29): say the crate graph as one `Event::Nodes` (core-plane.md section 6)

- What: `blaze-protocol` now has `Event::Nodes { first: u32, kinds: Vec<NodeKind>, names: String,
  name_at: Vec<u32>, deps: Vec<u32>, deps_at: Vec<u32> }`: the nodes `first..first + kinds.len()`,
  node `i` named `names[name_at[i]..name_at[i + 1]]` (UTF-8 bytes) and waiting for
  `deps[deps_at[i]..deps_at[i + 1]]`; `name_at` and `deps_at` hold `kinds.len() + 1` offsets from 0 to
  the end of `names` / `deps`, never going back. Readers check it with
  `blaze_protocol::events::NodeBatch::new` (or `Event::nodes(&mut Order)`), which refuses a malformed
  one with `Malformed`. `blaze-ui` and the plane read and say it; `Event::Node` still works.
- Where: `crates/blaze-engine/src/events.rs`, `graph` (lines 74-110). The ids are already a run
  (`first + index.len()` in push order) and each node already comes after its deps (the cycle
  branch says the rest waiting for nothing), so build the columns in place of `nodes: Vec<Event>`:
  `kinds` with capacity `order.len()`, `name_at`/`deps_at` starting `vec![0]` with capacity
  `order.len() + 1`; for each node push its kind (`Std` for `STD`, else `Names`), `names.push_str(name)`,
  `name_at.push(names.len() as u32)`, `deps.extend_from_slice(&on)`, `deps_at.push(deps.len() as u32)`.
  Then one `say(Event::Nodes { first, kinds, names, name_at, deps, deps_at })` in place of the
  `for event in nodes { say(event) }` loop, said only when `order` is not empty. The doc comment at
  line 21 names `Event::Node`: name `Event::Nodes`.
- Why: cargo.md section 16 and core-plane.md section 6. Each `Event::Node` owns a `String` and a
  `Vec<u32>` and is a record of its own through `say`: two allocations and one record a crate. One
  `Event::Nodes` is five allocations and one record for the whole graph, as the plane now says a
  batch.

> forwarded to codegen: written by `requests`' child requests-nodes, which told the lane.

> applied by codegen-engine (2026-10-29): `graph` says one `Event::Nodes` built by a new pure
> `events::nodes` (one reused deps buffer, no `Event` per crate), tested through `NodeBatch::new`.

## From safety-fuzz-persist (2026-10-29): make `Assembled`'s store format public, for its fuzz target

What: in `crates/hir-ty-codegen/asm.rs`, make `struct Assembled` `pub` (its fields can stay
`pub(super)`) and `Assembled::from_bytes(&[u8]) -> Option<Assembled>` and
`Assembled::to_bytes(&self) -> Vec<u8>` `pub`; in `crates/hir-ty-codegen/mod.rs`, add
`pub use asm::Assembled;` (the module stays private). Nothing else changes.

Why: speed-backend-intrinsics named `from_bytes` a parser of untrusted bytes (it reads the
store's copy of assembled code; `v2/requests/safety.md`, "From speed-backend-intrinsics"), and
`fuzz/targets/codegen_asm.rs` feeds it arbitrary bytes and checks `None` or a value whose
`to_bytes` is the input. Today both are private to `asm.rs`, so the target is written against
`hir_ty_codegen::Assembled::{from_bytes, to_bytes}` and won't build until this lands.

One correction to the section's claim: `from_bytes` does allocate past the input's length. Each
21-byte record becomes a 32-byte `Named` in a vector that grows by doubling, so its largest
allocation is about 3 times the input. The target holds it to `4 * len + 1 KiB`, which is still
linear and needs no change.

> forwarded to codegen: `crates/hir-ty-codegen/` is its (told by tmux).
> forwarded to codegen: `crates/hir-ty-codegen/` is its, and it is running.
> applied by codegen-v1 (2026-10-29): `Assembled`, `Assembled::to_bytes` and `Assembled::from_bytes` are `pub` and `mod.rs` has `pub use asm::Assembled;`; fields and the module stay private.

## From speed-backend-sanitize (2026-10-29): what S2 and F2 need outside `llvm.rs`, `llvm/` and `vendor/llvm-sys/`

The LLVM side is written (`crates/hir-ty-codegen/llvm/sanitize.rs`, `llvm/release.rs`,
`llvm/emitted.rs`, `vendor/llvm-sys/wrappers/passes.cpp`; design in
`docs/v2/plan/lanes/codegen/speed-backend-llvm.md` "Sanitizers"). It programs against the items
below, and `hir-ty-codegen` does not build until the lead adds Z1 and Z2 and the adapter adds
`Function::unsanitized` (Z3.2).

> forwarded to codegen.

### Z1. lead codegen: the sanitizer set in `Spec` (`crates/blaze-codegen/src/settings.rs`)

What: add these types and `pub sanitize: Sanitize` as the last field of `Spec`, exactly as
written (names, bit values and methods are what `llvm/sanitize.rs` uses; the coverage bits are
checked at compile time against `LLVMBlazeCoverage*` of `vendor/llvm-sys`):

```rust
/// rustc's `-Zsanitizer` (address, thread, memory), `-Zsanitizer-recover`,
/// `-Zsanitizer-memory-track-origins`, and SanitizerCoverage as rustc gets it
/// (`-Cpasses=sancov-module` with `-Cllvm-args=-sanitizer-coverage-*`): one for the whole
/// build, as cargo passes `RUSTFLAGS` to every crate. `Sanitize::default()` is none.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Sanitize {
    pub sanitizers: Sanitizers,
    /// Of `sanitizers`, those that report and go on (`-Zsanitizer-recover`).
    pub recover: Sanitizers,
    /// `-Zsanitizer-memory-track-origins`: 0, 1 or 2; 0 without `Sanitizers::MEMORY`.
    pub track_origins: u8,
    pub coverage: Coverage,
}

impl Sanitize {
    pub const fn is_empty(&self) -> bool {
        self.sanitizers.is_empty() && !self.coverage.is_on()
    }
}

/// A set of sanitizers, as bits.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Sanitizers(pub u8);

impl Sanitizers {
    pub const NONE: Sanitizers = Sanitizers(0);
    pub const ADDRESS: Sanitizers = Sanitizers(1 << 0);
    pub const THREAD: Sanitizers = Sanitizers(1 << 1);
    pub const MEMORY: Sanitizers = Sanitizers(1 << 2);
    /// In a function's set of what is off for it only (`Function::unsanitized`): LLVM's
    /// SanitizerCoverage skips it, as it skips a function named `__sanitizer_*`. Never in
    /// `Sanitize::sanitizers` or `recover`.
    pub const COVERAGE: Sanitizers = Sanitizers(1 << 3);

    pub const fn contains(self, other: Sanitizers) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub const fn union(self, other: Sanitizers) -> Sanitizers {
        Sanitizers(self.0 | other.0)
    }
    pub const fn without(self, other: Sanitizers) -> Sanitizers {
        Sanitizers(self.0 & !other.0)
    }
}

/// SanitizerCoverage: `-sanitizer-coverage-level` (1 the entry block, 2 every block, 3 and
/// critical edges, 4 and indirect calls) and the `-sanitizer-coverage-*` switches as bits.
/// Canonical: `Coverage::default()` (level 0, no flags) unless `-Cpasses` names
/// `sancov-module` and the level is 1 to 4, so two builds without coverage have one key.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Coverage {
    pub level: u8,
    pub flags: u16,
}

impl Coverage {
    pub const TRACE_PC: u16 = 1 << 0;
    pub const TRACE_PC_GUARD: u16 = 1 << 1;
    pub const INLINE_8BIT_COUNTERS: u16 = 1 << 2;
    pub const INLINE_BOOL_FLAG: u16 = 1 << 3;
    pub const PC_TABLE: u16 = 1 << 4;
    pub const TRACE_CMP: u16 = 1 << 5;
    pub const TRACE_DIV: u16 = 1 << 6;
    pub const TRACE_GEP: u16 = 1 << 7;
    pub const TRACE_LOADS: u16 = 1 << 8;
    pub const TRACE_STORES: u16 = 1 << 9;
    pub const NO_PRUNE: u16 = 1 << 10;
    pub const STACK_DEPTH: u16 = 1 << 11;
    pub const TRACE_PC_ENTRY_EXIT: u16 = 1 << 12;
    pub const CONTROL_FLOW: u16 = 1 << 13;

    pub const fn is_on(self) -> bool {
        matches!(self.level, 1..=4)
    }
}
```

Keys: in `Spec::settings`, destructure `sanitize` with the other fields and, after the bytes of
`fold`, hash
`let Sanitize { sanitizers, recover, track_origins, coverage } = sanitize;`
`hasher.update(&[sanitizers.0, recover.0, *track_origins, coverage.level]);`
`hasher.update(&coverage.flags.to_le_bytes());`
and bump `SETTINGS_DOMAIN` to `"blaze v2 codegen settings 2"` (the hashed bytes change).
`Shape::key` (`backend.rs`) hashes `unit.instance.settings`, so the template key follows, and
the instance (unit) key has the settings, so both keys change with the set. In
`every_field_is_in_the_settings` add
`|it| it.sanitize.sanitizers = Sanitizers::ADDRESS`,
`|it| it.sanitize.recover = Sanitizers::ADDRESS`, `|it| it.sanitize.track_origins = 2`,
`|it| it.sanitize.coverage.level = 3`, `|it| it.sanitize.coverage.flags = Coverage::PC_TABLE`.
Every `Spec { .. }` literal (`rg 'Spec \{' crates`) gets `sanitize: Sanitize::default()`.
Why: S2 (rustc's flags as data, the same for every function) and keys: a sanitized build's code
differs from a plain one's, so it must never reuse the other's units or templates.

### Z2. lead codegen: a function's SanitizerCoverage arrays (`crates/blaze-codegen/src/lib.rs`, `template.rs`, `content.rs`)

What:
1. `CompiledFunction` gains `pub coverage: Option<Covered>` (`None` without SanitizerCoverage;
   every literal gets `coverage: None`), with

   ```rust
   /// What LLVM's SanitizerCoverage made for one function: its arrays, which start zeroed and
   /// which the link gathers with every other function's into one section each, in one order,
   /// and its PC table.
   #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
   pub struct Covered {
       /// Bytes of its 8-bit counters (`__sancov_cntrs`), flags (`__sancov_bools`) and
       /// guards (`__sancov_guards`, 4 bytes each); 0 for an array it has not.
       pub counters: u32,
       pub flags: u32,
       pub guards: u32,
       /// Of each block it counts, in the order of its arrays: how far into `code` it starts,
       /// and its flags (1 for the function's entry), as `__sancov_pcs` has them.
       pub pcs: Vec<(u32, u64)>,
   }
   ```
2. `Target::Coverage(u32)`: the function's own array 0 (counters), 1 (flags) or 2 (guards),
   the relocation's addend into it. A function names no other's.
3. The function's content hash (`content.rs`) and the template's instantiation (`template.rs`
   carries `coverage` from template to function) include it.

Why: `v2/release.rs` sets `emitted.function.coverage`; the link lays the arrays out and builds
the PC table (`v2/requests/link.md`, "From speed-backend-sanitize").

### Z3. codegen-adapter (`crates/hir-ty-codegen/v2/backend.rs`, `v2.rs`, `v2/shape.rs`, `v2/convert.rs`)

1. `check_spec`: `if spec.backend == Backend::Cranelift && !spec.sanitize.is_empty()`, fail with
   `"-Zsanitizer and SanitizerCoverage are instrumented by the LLVM back end (Backend::Llvm), which this unit is not compiled by"`.
   (no-fallback: never a build without the instrumentation.)
2. `v2.rs` `Function` gains `pub(crate) unsanitized: blaze_codegen::settings::Sanitizers`;
   `prepared` sets it from the front end's effective `#[sanitize(.. = "off")]` set (requested of
   db in `v2/requests/db.md`, "From speed-backend-sanitize"): for `InstanceDef::Fn(func)` that
   set; a closure's is its enclosing function's (rustc walks `opt_local_parent`); shims and drop
   glue `Sanitizers::NONE` (rustc's default attrs). Add `Sanitizers::COVERAGE` when the
   function's symbol (`#[no_mangle]`/`#[export_name]`) starts with `__sanitizer_`, which LLVM's
   SanitizerCoverage skips by name and blaze's `blaze.own` hides from it; the fuzz engine's
   `__sanitizer_cov_*` callbacks need it (F2 item 1).
3. `v2/shape.rs`: hash `function.unsanitized.0` under a new `part::` tag next to `HINT`, so a
   function with a sanitizer off has another template.
4. `v2/convert.rs` `reloc_target`: map `RelocName::User { namespace: llvm::sanitize::COVERAGE,
   index }` (`u32::MAX - 2`) to `Target::Coverage(index)` for `index < 3`, and refuse others; set
   `coverage: None` in the function it builds (release sets it after).
Why: S2's attributes per function, its Cranelift refusal, and the keys of the per-function set.

> written by speed-backend-sanitize (2026-10-29); told codegen and codegen-adapter by tmux.

## frontend: MIR reads source maps only where a position is emitted (docs/v2/plan/design/frontend.md item 7)

What: `crates/hir-ty-mir/src/mir/lower.rs:4471`, `mir/location.rs:45,71,342`,
`mir/type_name.rs:914`, `mir/eval.rs:670`, `mir/const_check.rs:100` read `Body::unmoved_source_map`
or `with_source_map` on every MIR lowering. Asked: MIR keeps HIR ids (`ExprId`, `PatId`) as its
span, and turns one into a file position from the source map only when debug info, a
`#[track_caller]` `Location`, a panic message or a diagnostic is emitted (items.md 6.1: positions
are side-table references, filled at link time). Lowering a body then never reads its map, so
the body lowering can skip building it (request to speed-hir-store-srcmap in
`v2/requests/speed-hir.md`). The positions emitted must be byte-identical to today's: a test
comparing every `Location` and debug-info line of the MIR fixtures before and after.

> forwarded to codegen: `hir-ty-mir` is its.

> applied by codegen-mir-store (2026-10-29): `failed_expansion` (`lower.rs`) reads the store's
> macro calls instead of the map, `location_macros`/`located_concats` and `closures_around_query`
> read the map only when the body called a location macro (or any macro), and `const_check` only
> for what it reports; the position code (`source_range`, `location_of`) is unchanged. Needs
> `ExpressionStore::macro_calls()` from format ("## codegen-mir-store: the store keeps its macro
> calls without the source map", below); `eval.rs`'s unused `resolve_mir_span` is asked of
> codegen-mir. Test: `hir-ty-mir/tests/mir/source_maps.rs` (no `Body::relative_` or
> `with_source_map_` while lowering and evaluating macro-free bodies; the macros' places).

## frontend: `hir_ty_codegen::v2::Databases` takes the shared database (frontend.md item 10)

What: `Databases::new` takes `Box<dyn HirDatabase + Send>` per worker (a salsa handle each;
`crates/load-cargo/src/v2/db.rs:32-35`). Asked: take `&'db dyn HirDatabase` shared by every
worker (the database is `Sync`, decided), once db answers item 2 of "frontend: what
frontend.md items 7-12 still need" in `v2/requests/db.md`.

> forwarded to codegen.

> done already (checked by codegen-v2, 2026-10-29): `Databases::new(db: &'db Db)` holds the one shared database and locks nothing (`v2/frontend.rs`); `crates/load-cargo/src/v2/db.rs` is gone, and the one caller, `blaze-cargo/src/serve/unit/codegen.rs:220`, passes `reader.db()` (`&Db`). Nothing left to request of cargo.

## frontend-text: caller locations, debug-info rows and blaze-engine's lines on the one line index (frontend.md item 6; 2026-10-29)

`span::LineIndex` is new (lane `frontend-text`, `crates/span/src/lines.rs`): a bitmap of a
text's `\n` bytes made with SIMD (AVX-512BW/AVX2/SSE2/NEON, chosen at run time, each checked
against the scalar path) and a rank table per 512 bytes, so an offset's line is one rank and a
line's start one select, with no `Vec` of line starts. `place(offset)` gives a `span::Place`
whose `line()`, `char_column(text)`, `display_column(text)` and `byte_column(text)` are
rustc's (`lookup_char_pos`; tested against a model of `rustc_span::SourceFile` at every char
boundary); `row(offset)` gives the row and row start in a text that is part of a file. The
per-file query is `base_db::line_index(db, FileText) -> &LineIndex` (request in
`v2/requests/db.md`, same lane): made once per text, the first time anything asks. Today
hir-ty-mir and hir-ty-codegen each scan texts into `Vec`s of line starts, and blaze-engine uses
ide-db's `line_index` (going away with ide-db). Land after the db request.

> forwarded to codegen (told by tmux).

### 1. `crates/hir-ty-mir/src/mir/location.rs`

Old (from `/// File, line and column of where a range of a file begins.` to the end of
`column_width`):

```rust
/// File, line and column of where a range of a file begins.
pub fn line_and_column(
    db: &dyn HirDatabase,
    FileRange { file_id, range }: FileRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let starts = line_starts(db, text);
    let at = u32::from(range.start());
    let line = starts.partition_point(|&start| start <= at);
    let line_start = *starts.get(line.checked_sub(1)?)?;
    let written = text.text(db).get(line_start as usize..at as usize)?;
    Some((file_id, line as u32, written.chars().map(column_width).sum::<usize>() as u32 + 1))
}

/// Where each line of a file begins, the first of them at 0.
#[salsa::tracked(returns(ref))]
fn line_starts(db: &dyn HirDatabase, text: base_db::FileText) -> Vec<u32> { ... }

/// How many columns rustc counts a character of a source line as ... 
pub fn column_width(ch: char) -> usize { ... }
```

New:

```rust
/// File, line and column of where a range of a file begins: rustc's line, and its
/// `col_display + 1` (what `Location::caller` and debug info are given).
pub fn line_and_column(
    db: &dyn HirDatabase,
    FileRange { file_id, range }: FileRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let place = base_db::line_index(db, text).place(range.start())?;
    Some((file_id, place.line(), place.display_column(text.text(db))?))
}
```

and drop `use unicode_width::UnicodeWidthChar;` (nothing else in hir-ty-mir uses it; then
`unicode-width.workspace = true` can leave `crates/hir-ty-mir/Cargo.toml`). `span::char_width`
is `column_width` moved, the same match.

### 2. `crates/hir-ty-codegen/mod.rs`

In the `pub use hir_ty_mir::mir::location::{...}` list (~L94) drop `column_width,`.

### 3. `crates/hir-ty-codegen/places.rs`: the body anchor's rows

Old (`anchor_rows` and `anchored`):

```rust
/// Where each row of the anchor of the body begins.
#[salsa::tracked(returns(ref))]
fn anchor_rows(db: &dyn HirDatabase, owner: DefWithBodyId) -> Option<Box<[u32]>> {
    let text = body_syntax(db, owner).as_ref()?.text();
    let ends = text.bytes().enumerate().filter(|&(_, byte)| byte == b'\n');
    Some(std::iter::once(0).chain(ends.map(|(at, _)| at as u32 + 1)).collect())
}

/// The place of `offset` in the anchor of the body.
fn anchored(db: &dyn HirDatabase, owner: DefWithBodyId, offset: TextSize) -> Option<Place> {
    let starts = anchor_rows(db, owner).as_ref()?;
    let text = body_syntax(db, owner).as_ref()?.text();
    let at = u32::from(offset);
    let row = starts.partition_point(|&start| start <= at).checked_sub(1)?;
    let written = text.get(starts[row] as usize..at as usize)?;
    let column = written.chars().map(column_width).sum::<usize>() as u32;
    Some(Place::Anchored { row: row as u32, column })
}
```

New:

```rust
/// Where the rows of the anchor of the body are.
#[salsa::tracked(returns(ref))]
fn anchor_rows(db: &dyn HirDatabase, owner: DefWithBodyId) -> Option<span::LineIndex> {
    Some(span::LineIndex::new(body_syntax(db, owner).as_ref()?.text()))
}

/// The place of `offset` in the anchor of the body.
fn anchored(db: &dyn HirDatabase, owner: DefWithBodyId, offset: TextSize) -> Option<Place> {
    let (row, start) = anchor_rows(db, owner).as_ref()?.row(offset)?;
    let text = body_syntax(db, owner).as_ref()?.text();
    let written = text.get(usize::from(start)..usize::from(offset))?;
    Some(Place::Anchored { row, column: span::display_width(written) })
}
```

`row` counts the newlines before `offset` and starts a row after a last `\n`, as the old
`starts` did, and `display_width` is the old sum of `column_width` (ASCII as a length plus 3
per tab), so `Place::Anchored` is the same for every offset.

> applied by codegen-v1 (2026-10-29): items 2 and 3, `mod.rs` no longer imports `column_width` and `places.rs`'s `anchor_rows`/`anchored` are on `span::LineIndex::row` and `span::display_width`.

### 4. `crates/blaze-engine`: ide-db's `line_index` -> base-db's

- `src/mirdump.rs`, `impl_place`:
  `let line = ide_db::line_index(self.db, file).line_col(at).line + 1;` ->
  ```rust
  let text = self.db.file_text(file);
  let line = base_db::line_index(self.db, text).place(at).map_or(1, |it| it.line());
  ```
  (`at` is where an `impl` is written, always in the text; `map_or` only keeps the old
  infallible shape: if you prefer, `.expect("an impl is in its file")`.)
- `src/test_harness.rs` (~L158):
  ```rust
  let text = db.file_text(file);
  let lines = base_db::line_index(db, text);
  let at = |offset| {
      let place = lines.place(offset).expect("a test's name is in its file");
      let column = place.char_column(text.text(db)).expect("on a char boundary");
      (place.line() as usize, column as usize)
  };
  ```
  libtest's `start_line`/`start_col`/`end_line`/`end_col` are rustc's `span_to_location_info`:
  1-based lines and `col + 1` in chars; the old `line_col` gave UTF-8 byte columns, wrong after a
  non-ASCII char on the line.
- `src/doctest/extract.rs`, `line_of`:
  ```rust
  let text = self.db.file_text(file);
  let line = base_db::line_index(self.db, text).place(at)?.line();
  Some((self.path(file)?, line as usize))
  ```
- Add `base-db.workspace = true` to `crates/blaze-engine/Cargo.toml` if it is not there; drop
  `ide-db` when nothing else uses it.

> applied by codegen-engine (2026-10-29): item 4, the three sites on `base_db::line_index` as
> written (`impl_place` with the `.expect`); `base-db` was already in the manifest, and `ide-db`
> stays, since `RootDatabase`, `base_db`, `rust_doc` and the rest are still read through it.

Why: frontend.md item 6 ("line and column are computed only when a diagnostic or debug info
needs them, from a newline bitmap built with SIMD on first use"). One index per text, built
once, ~0.13 bytes per byte of text, instead of a `Vec<u32>` per query per text.
`places_match_the_old_line_starts` and `rows_are_newlines_before` in
`crates/span/src/lines/tests.rs` are the differentials against the code removed here; the old
and new places differ only where the old was not rustc's (at the `\n` of a `\r\n`, at the end
of a text ending in `\n`, in a text with nothing after its byte order mark), none of which a
call or a statement can be at.
Status: open

> applied by codegen-mir-store (2026-10-29): item 1, `line_and_column` is on
> `base_db::line_index`'s `place(..).display_column`, `line_starts` and `column_width` are gone,
> and so is `unicode-width` from hir-ty-mir's manifest. Also: `column!()` is rustc's char column
> (`lookup_char_pos(..).col + 1`), not the display one, through the new
> `location::macro_location_of` that `placed` uses; codegen-v1 is asked to use it at its two
> `column!()` sites (`~/restart-2026-10-28/briefs/v2/codegen-mir-store.requests.md`).

## speed-hir-store-stmts: statements become columns, `Statement` a view (hir-ty-mir)

From speed-hir-store-stmts (child of speed-hir-store), 2026-10-29. For `codegen-mir` (owner of
`crates/hir-ty-mir/`). `hir-def` stores a block's statements as rows of two columns (5 bytes a
statement, a `let`'s ids in `extra`), where it was a 32-byte enum in a flat array. These land
together with that change.

**Applies to the current `mir/lower.rs`**, with "## speed-hir-store: hir-ty-mir onto the HIR
column views" applied (checked 2026-10-29 20:15: every old text below is in the file as written,
`fn is_super_let_block` now at ~L3419, `fn lower_block_to_place` at ~L3438; the line numbers
below are a few lines low). That section's notes "keep `*pat` / `*else_branch` / `*initializer`
/ `*expr`: a `Statement` field" are reversed by this one.

**The rule.** `Statement` is now `Statement<'a>`, a `Copy` view decoded from the rows, with
today's variant and field names. `Expr::Block { statements, .. }` gives `statements:
Statements<'a>`, a `Copy` handle (`len`, `is_empty`, `get(i) -> Option<Statement<'a>>`, `first`,
`last`, `iter()` double-ended and exact size, `IntoIterator` for it and for `&` it), no longer
`&'a [Statement]`. So:
1. A statement is a value: `for statement in statements` and `statements.iter()` still compile
   and yield `Statement<'_>`. Its id, `Option` and `bool` fields bind by value: `*pat` ->
   `pat`, `*initializer` -> `initializer`, `*is_super` -> `is_super`; a `&Statement::..`
   pattern drops the `&`. `Item::MacroDef(id)` still binds `&MacroDefId`.
2. A parameter `&[Statement]` -> `Statements<'_>` (`hir_def::hir::Statements`).
3. No slice operations on `statements` (`.iter().any/all` work as before).
`MirLowerCtx::store` is `&'a ExpressionStore`, so `Statements<'a>` borrows from behind a shared
reference as the slice did; the `Expr::Block` arm's `lower_block_to_place(statements, ..)` calls
(~L1030, ~L1038, inside and outside `lower_loop`'s closure) are unchanged.

Examples (real sites):
- ~L3436 `        statements: &[hir_def::hir::Statement],`  ->  `        statements: hir_def::hir::Statements<'_>,`
- ~L3591 `&hir_def::hir::Statement::Expr { expr, has_semi: _ } => {`  ->  `hir_def::hir::Statement::Expr { expr, has_semi: _ } => {`
- ~L3459 `self.extend_temporaries(*initializer, *pat, scope);`  ->  `self.extend_temporaries(initializer, pat, scope);`

Counts: 29 sites, all in `crates/hir-ty-mir/src/mir/lower.rs`, in `fn is_super_let_block` and
`fn lower_block_to_place`. No other file of `crates/hir-ty-mir/` or `crates/hir-ty-codegen/`
reads a HIR statement (`location.rs`, `type_name.rs`, `places.rs` match `Expr::Block` only for
its `id`/`tail`).

> forwarded to codegen (window `codegen-next1`, told by tmux).

> applied by codegen-mir-store (2026-10-29): all 29 sites in `lower.rs`, `is_super_let_block` and
> `lower_block_to_place` take `Statements<'_>` and bind the statement fields by value.

### crates/hir-ty-mir/src/mir/lower.rs  (29)
In `fn is_super_let_block` (~L3415-3432):
- `    fn is_super_let_block(&self, statements: &[hir_def::hir::Statement]) -> bool {`  ->  `    fn is_super_let_block(&self, statements: hir_def::hir::Statements<'_>) -> bool {`
- `                    *is_super\n                        || matches!(self.store.pat(*pat), Pat::Bind { id, subpat: None } if self.store[id].name.is_generated())`  ->  `                    is_super\n                        || matches!(self.store.pat(pat), Pat::Bind { id, subpat: None } if self.store[id].name.is_generated())`
- `hir_def::hir::Statement::Expr { expr, .. } => literal(*expr),`  ->  `hir_def::hir::Statement::Expr { expr, .. } => literal(expr),`
- unchanged: `statements.iter().any(|it| matches!(it, hir_def::hir::Statement::Let { .. }))` and `statements.iter().all(|statement| match statement {`.

In `fn lower_block_to_place` (~L3434-3640): the parameter, then every `*` on a statement
field. Within this function every `*pat`, `*initializer`, `*expr_id`, `*else_branch` and
`*is_super` is one (the tail half after the loop has none), so they can be done as token
replacements scoped to the function. One by one:
- ~L3436  `        statements: &[hir_def::hir::Statement],`  ->  `        statements: hir_def::hir::Statements<'_>,`
- the `super let` loop (~L3452-3474): `self.extend_temporaries(*initializer, *pat, scope);`  ->  `self.extend_temporaries(initializer, pat, scope);`; `self.declare_bindings(*pat);` (this and the one at ~L3485, the only two)  ->  `self.declare_bindings(pat);`; `self.lower_expr_as_place(current, *initializer, true)?`  ->  `self.lower_expr_as_place(current, initializer, true)?`; `self.resolver.update_to_inner_scope(self.db, self.store_owner, *initializer);`  ->  `self.resolver.update_to_inner_scope(self.db, self.store_owner, initializer);`; `let matched = self.pattern_match(c, None, init_place, *pat);`  ->  `let matched = self.pattern_match(c, None, init_place, pat);`
- the `Statement::Let` arm (~L3481-3590). `initializer` and `else_branch` are `Option<ExprId>` by value now, so `Some(expr_id)` binds `ExprId`, not `&ExprId`:
  - `(initializer, else_branch, *is_super, self.store.pat(*pat))`  ->  `(initializer, else_branch, is_super, self.store.pat(pat))`
  - `self.extend_temporaries(*expr_id, *pat, self.drop_scopes.len() - 1);` (both, ~L3503 and ~L3538)  ->  `self.extend_temporaries(expr_id, pat, self.drop_scopes.len() - 1);`
  - `let Some(c) = self.lower_expr_to_place(*expr_id, local.into(), current)?`  ->  `let Some(c) = self.lower_expr_to_place(expr_id, local.into(), current)?`
  - `self.push_fake_read(c, local.into(), MirSpan::PatId(*pat));`  ->  `self.push_fake_read(c, local.into(), MirSpan::PatId(pat));`
  - `self.lower_expr_as_place(current, *expr_id, true)?`  ->  `self.lower_expr_as_place(current, expr_id, true)?`
  - `self.push_fake_read(current, init_place, MirSpan::PatId(*pat));`  ->  `self.push_fake_read(current, init_place, MirSpan::PatId(pat));`
  - in `self.resolver.update_to_inner_scope(` (~L3554-3558): `                            *expr_id,`  ->  `                            expr_id,`
  - `self.super_let = enclosing.filter(|_| *is_super);`  ->  `self.super_let = enclosing.filter(|_| is_super);`
  - `.pattern_match_linked(current, init_place, *pat)`  ->  `.pattern_match_linked(current, init_place, pat)`; `None => self.pattern_match(current, None, init_place, *pat),`  ->  `None => self.pattern_match(current, None, init_place, pat),`
  - `self.lower_let_else(else_block, *else_branch, declared, span)?;`  ->  `self.lower_let_else(else_block, else_branch, declared, span)?;`
  - `self.store.walk_bindings_in_pat(*pat, |b| {`  ->  `self.store.walk_bindings_in_pat(pat, |b| {`
- ~L3591  `&hir_def::hir::Statement::Expr { expr, has_semi: _ } => {`  ->  `hir_def::hir::Statement::Expr { expr, has_semi: _ } => {`
- unchanged: `for statement in statements {` (~L3452), `for statement in statements.iter() {` (~L3479), `self.is_super_let_block(statements)` (~L3443), `hir_def::hir::Statement::Item(_) => (),`, the column-view section's `self.store[id].mode`, `binding_ty(id)`, `let binding = id;` (`id` from the `Pat::Bind` view).

Status: done (checked by codegen-mir, 2026-10-29 20:55: both functions take `Statements<'_>`,
no `*` on a statement field is left in `lower_block_to_place`, no `&Statement::` pattern).

> forwarded to codegen (window `codegen-next1`, told by tmux).

## From codegen-inlined (the code-generator side of the MIR inliner)

Full list: `~/restart-2026-10-28/briefs/v2/codegen-inlined.requests.md`. For other lanes:

- **cargo**: call `.with_policy(&policy)` on the `HirFrontend` that `generate` uses
  (`crates/blaze-cargo/src/serve/unit/codegen.rs`, `frontend()`, line 220). Without it every
  v2 describe/prepare stops with "the front end was made with no policy".
- **codegen (blaze-codegen)**: `through_slot` should refuse only `patchable` callees, not
  tiered ones (std is MIR-inlined into every unit); a live member's LLVM tier inlines
  patchable callees unless `Spec` says the build is live; `always_inlinable` is a salsa query
  that reads the thread-local `building()`; `FunctionCx::always_inlinable_callee` is still
  missing (codegen-adapter request, line 461); the v2 DWARF writer should emit
  `Side::inlined` as `DW_TAG_inlined_subroutine`.

Status: open

> forwarded to cargo (`with_policy` in `serve/unit/codegen.rs`) and codegen (`through_slot`), told by tmux; nothing in requests' paths.

## frontend-db: hir-ty-mir registers the MIR providers; the codegen crates take `&Db` (frontend.md item 15; 2026-10-29)

> applied by frontend-db-ty (hir-ty-mir only; hir-ty-codegen and blaze-engine are
> frontend-db-tyx's and frontend-db-edge's): items 1-4 (`compute_X`, registration in
> `crates/hir-ty-mir/src/mir/providers.rs`, `type_name`'s `None` is a `MirEvalError::NotSupported`
> naming the type, the crate doc) and the tool on hir-ty-mir (its test database is the alias).

From `frontend-db` (notes `docs/v2/plan/lanes/frontend/db.md`; the hir-ty side is in
`v2/requests/db.md`, "frontend-db: the concrete database", section 5). The memos behind
`HirDatabase`'s MIR methods move into hir-ty (`hir_ty::providers`), because hir-ty,
hir-ty-borrowck and blaze-items read them from below. Their code stays here and becomes the
providers.

1. In `crates/hir-ty-mir/src/mir/{lower,monomorphization,optimize}.rs` (and wherever the
   `runtime_*` forms are), each `#[salsa::tracked(...)] pub fn X_query` that a `HirDatabase`
   method calls (today through `impl_hir_database!`) loses its attribute and becomes
   `pub(crate) fn compute_X`. The rename is what makes every caller that bypassed the memo
   fail to compile; such a caller calls `hir_ty::providers::X(db, ..)` or `db.X(..)` instead.
   The `promoted_*` queries stay tracked here, as they are. Their cycle-result fns move to
   hir-ty with the memos.
2. New in hir-ty-mir's `lib.rs` (or a `providers.rs`), the one registration:
   ```rust
   inventory::submit! {
       hir_ty::providers::Mir {
           mir_body: mir::lower::compute_mir_body,
           // ... one field per moved query, and
           interpret_mir: mir::eval::interpret_mir,
           opaque_of_impl: mir::monomorphization::opaque_of_impl,
       }
   }
   ```
   `crates/hir-ty-mir/Cargo.toml`: `inventory.workspace = true` (the coordinator adds the
   workspace entry, 0.3.24, the version already locked for salsa). There is no `provide()` to
   call. rustc links a `#[used]` static from every rlib in the graph (rust-lang/rust#95604), so
   any binary that links hir-ty-mir has the table, and one that doesn't panics on the first MIR
   miss, naming what to link.
3. `db.type_name(ty, module)` returns `Option<String>` now. The `type_name` intrinsic in
   `mir/eval` turns `None` into a `MirEvalError` that names the type. The old body's fallback
   to the human-readable display (`impl_db_macro.rs` L267-275) goes, because rustc's
   `type_name` is always the path.
4. The module doc at `lib.rs:6` ("`impl_hir_database!` implements with them") becomes "hir-ty
   declares the memos (`hir_ty::providers`); this crate registers their code".
5. Run `tools/v2/concrete_db.py` on `hir-ty-mir`, `hir-ty-codegen` (its `mod.rs` and `v2/`)
   and `blaze-engine` (dry run: 170, 382 and 19 sites; 2, 3 and 1 left, each printed with what
   to do):
   ```sh
   PYTHONPATH=python/typed-input/src python3 tools/v2/concrete_db.py \
     '{"root": ".", "crates": ["hir-ty-mir"], "write": true}'
   ```
   blaze-engine's `Cargo.toml` needs `base-db.workspace = true`. Test databases become
   `type TestDB = base_db::Db;` made by `Db::for_tests()` (the tool rewrites
   `TestDB::default()`), and their `impl_hir_database!` goes.
   `optimize/inline/fixtures.rs` is one of them.

> applied by frontend-db-tyx (item 5, hir-ty-codegen only): the tool (383 sites); `v2/tests/db.rs`
> is `pub(crate) type TestDB = base_db::Db;` with `use hir as _;`; `Databases` in
> `v2/frontend.rs` holds `&'db Db` (the tool had written `&'db (Db + Sync)`). The tool's third
> note, `v2/fold.rs:1007`, is an `as &dyn Fn`, not a database, and stays. No caller bypasses a
> moved memo: the direct calls left are `*_promoted_mir_body_query` (still tracked in hir-ty-mir)
> and the unmemoized `interpret_mir`.

> applied by codegen-engine (2026-10-29): item 5 for blaze-engine. The `&Db` rewrites were
> already in, and `base-db.workspace = true` was already in the manifest. The tool's last 8 sites
> were `SourceDatabase` imports that `Db`'s inherent methods made unused. It left nothing.

Why: a memo hit on `&Db` is a static call with no vtable (frontend.md item 15). The table is
read only on a miss.

Status: open

> forwarded to codegen (codegen-next1 told). The root `inventory = "0.3.24"` is in (requests).

## dedup: rename `hir-ty-codegen`'s IR-emitting `align_up` (from dedup-mem)

`crates/hir-ty-codegen/mod.rs:8697` `fn align_up(&mut self, value: Value, align: Value) -> Value`
emits Cranelift instructions (`iadd_imm`, `iadd`, `band_not`); it is not a copy of
`blaze_mem::align::align_up` (a number), which the tree now has one of. Rename it
`emit_align_up` (definition and its three calls, lines 8671, 8673, 8727), so the name `align_up`
means one thing in the tree.

Status: open
> applied by codegen-v1 (2026-10-29): `FunctionCx::align_up` is `emit_align_up`, definition and its three calls in `mod.rs`.

## From codegen (lead): what Z1, Z2 and `Spec::mir_inline` change outside codegen's crates (2026-10-29)

Codegen landed Z1 (`Sanitize` in `Spec` and `Policy`), Z2 (`CompiledFunction::coverage`,
`Target::Coverage`) and `Spec::mir_inline`. These sites in other lanes don't build until
they change:

1. lane cargo, `crates/blaze-cargo/src/serve/unit/codegen.rs:188`: the `Policy` gets
   `sanitize`, from the unit's `-Zsanitizer=` (`ADDRESS`, `THREAD`, `MEMORY`),
   `-Zsanitizer-recover=`, `-Zsanitizer-memory-track-origins` and `-Cpasses=sancov-module` with
   `-Cllvm-args=-sanitizer-coverage-*` (`Coverage`, canonical as documented in
   `blaze-codegen/src/settings.rs`). Without those flags it is `Sanitize::default()`. The same
   file makes the `HirFrontend` it builds with: call `.with_policy(&policy)` on it
   (`hir-ty-codegen/v2/frontend.rs:126`), or every function fails with "the front end was made
   with no policy".
2. lane lazy, `crates/blaze-lazy/src/tests.rs:25`: `sanitize: Sanitize::default()` in the
   `Policy`, and `coverage: None` in its two `CompiledFunction` literals.
3. lane global, `crates/blaze-global/src/public.rs:550`: the test `Spec` gets
   `mir_inline: false, sanitize: Sanitize::default()`.
4. lane link:
   - `crates/blaze-link/src/stage.rs:170` `wide`: an arm for `Target::Coverage(array)`. It is
     the function's own array, so it is keyed by the function's key and the array, never
     shared between functions. `CompiledFunction::coverage` gives each array's size.
   - The link lays the arrays out in `__sancov_cntrs`, `__sancov_bools` and
     `__sancov_guards`, and the PC table in `__sancov_pcs` (`v2/requests/link.md`, "From
     speed-backend-sanitize").
   - `coverage: None` in the `CompiledFunction` literals of `src/tests.rs` (2) and
     `src/tests/ties.rs` (1).
5. lane premap:
   - `crates/blaze-premap/src/key.rs:342` `target_into`: an arm for `Target::Coverage(array)`
     under a new tag. A premapped image of covered code keys on it.
   - `src/model.rs:193`: an arm that places the function's own array.
   - `coverage: None` in the two literals of `src/tests.rs`.

Also:
- Code dedupe (`blaze-codegen/src/dedupe.rs`) never merges a function that has `coverage`:
  two copies would share counters.
- A static that names a coverage array is refused (`TemplateError::Coverage`).
- `CODE_DOMAIN`, `TARGET_DOMAIN`, `TEMPLATE_DOMAIN` and the template blob magic (`BZT3`) are
  bumped, so no older entry is read as a newer one.

Status: open

> forwarded: 1 to cargo, 2, 3 and 5 to fastcompile (lazy, global, premap), 4 to requests-link-fuzz (holds blaze-link). Each told by tmux.
> applied by requests (child requests-link-next): item 4: stage.rs wide arm for Target::Coverage(array), keyed SymKey::Coverage(function id, array) (symbols.rs); add_coverage lays out __sancov_cntrs/__sancov_bools/__sancov_guards (guards 4-aligned, a size not a multiple of 4 is an error) as Class::Named zero pieces, and __sancov_pcs as (function address + at, flags) Abs64 pairs (a pc past the code is an error), all in function key order; names per format (ELF __sancov_*, Mach-O __DATA,__sancov_*, COFF .SCOV$CM/.SCOV$BM/.SCOV$GM/.SCOVP$M); wasm stops naming SanitizerCoverage; a removed function drops its arrays and pcs. Tests in tests.rs: two covered functions with a Coverage(1) addend 3, a relink that drops one, and an allocation budget. Open: the __sanitizer_cov_*_init constructors and the rest of speed-backend-sanitize.

## From speed-link-edge-sol-live: level-2 counters share cache lines across functions (`hir-ty-codegen/tier.rs` `count`, and whoever picks `first`)

Status: open, does not block. For lead `codegen`. See "The live runtime" in
`docs/v2/plan/lanes/link/speed-of-light.md`, "The sampler's counters".

`count` makes each counted function `call blaze_live_counters()` at entry, then a plain
`load; iadd_imm 1; store` per counter, into `first .. first + 1 + 2 * branches` of one
`[u64; 65536]`. Functions' ranges are packed one after another, so eight counters share a
64-byte line (sixteen a 128-byte pair, which x86 and arm64 prefetch together), and two hot
functions on two threads write one line: each add then waits for the line to come from the other
core (about 50 to 100 ns on one socket, against about one cycle for an add to a line the core
owns). The adds are also not atomic, so two threads in the same function lose counts, and the
sampler's relaxed loads race them (item 12 d of `v2/requests/link.md`).
1. Start every function's range on a multiple of 16 counters (128 bytes): `first` rounded up
   where ranges are handed out, so no two functions share a line pair. That costs at most 15
   counters per function of the 65536.
2. Make the add `atomic_rmw add` with relaxed ordering (`ldadd` / `lock xadd`), so no count is
   lost and the sampler's loads don't race. It costs nothing more while one thread runs the
   function, the case 1 leaves.
3. The bound, if one function is hot on many threads at once: a block of counters per thread,
   summed by the sampler. That needs `blaze_live_counters` to hand out a thread's own block and
   the sampler to know every block; ask blaze-live's owner if 1 and 2 measure short.

> applied by codegen-v1 (2026-10-29): items 1 and 2 in `tier.rs` and `llvm/lower.rs`: `Prepared::finish` starts each function's counters on a multiple of `tier::COUNTER_ALIGN` (16) through `counted_from` (tested), so the engine's `first + used` needs no change; each add is an `atomic_rmw add` in the alias region `tier::COUNTER_REGION`, which LLVM's lowering makes `monotonic` (other atomics stay seq-cst). Item 1 holds only if the array starts on 128 bytes: asked of link below ("To link: `blaze-live`'s counters on 128 bytes"). Item 3 left for measurement, as the item says.

### To link: `blaze-live`'s counters on 128 bytes (from codegen-v1, 2026-10-29)

What: in `crates/blaze-live/src/lib.rs`, give `COUNTERS` a 128-byte alignment, e.g.

```rust
#[repr(C, align(128))]
struct Counters([std::sync::atomic::AtomicU64; format::COUNTERS]);

static COUNTERS: Counters =
    Counters([const { std::sync::atomic::AtomicU64::new(0) }; format::COUNTERS]);
```

with `blaze_live_counters` returning `COUNTERS.0.as_ptr() as *mut u64` and `sample.rs:249`
reading `crate::COUNTERS.0[..used]`.
Why: `hir-ty-codegen/tier.rs` now starts every function's counters on a multiple of 16 (128
bytes from the array's start), so two functions never share a cache-line pair; with the
array only 8-aligned, the pairs straddle and the fix is lost. The daemon's adds are now
atomic and relaxed, as the comment on `COUNTERS` asks.

> forwarded to codegen (from requests, pass 16; told by tmux).

## dedup: blaze-engine off memmap2, onto blaze_core::fs::Mapped

From `dedup-map` (report `/tmp/blaze-lanes/v2-dedup-map.md`). Every read-only file map moves onto
one type, `blaze_core::fs::Mapped` (`crates/blaze-core/src/fs/mapped.rs`): the same `MAP_SHARED`
`PROT_READ` map, `Deref<Target = [u8]>` like `memmap2::Mmap`, an empty file maps nothing, and
`MapOptions` for `populate`, huge pages and `madvise`. memmap2 then leaves the workspace.

1. `crates/blaze-engine/src/link.rs:456`, before:

       let bytes = unsafe { memmap2::Mmap::map(&file) }
           .with_context(|| format!("reading {}", exe.display()))?;

   after:

       let bytes = unsafe { blaze_core::fs::Mapped::map(&file) }
           .with_context(|| format!("reading {}", exe.display()))?;

   (The SAFETY comment above it stays as it is.)
2. `crates/blaze-engine/src/lib.rs:1366`, before:

       let Ok(bytes) = (unsafe { memmap2::Mmap::map(&file) }) else { continue };

   after:

       let Ok(bytes) = (unsafe { blaze_core::fs::Mapped::map(&file) }) else { continue };

3. `crates/blaze-engine/Cargo.toml`: delete `memmap2 = "0.9"` (line 50) and add
   `blaze-core.workspace = true` to `[dependencies]` (it has it only through blaze-cargo and
   blaze-splice today; blaze-core depends on none of blaze-engine's crates).

`&bytes` and `&*bytes` read as `&[u8]` exactly as with `Mmap`, so nothing else changes.

Status: open

> applied by codegen-engine (2026-10-29): both maps are `blaze_core::fs::Mapped::map`, `memmap2`
> is out of the manifest and `blaze-core.workspace = true` is in.


## dedup: hir_def::test_items

From `dedup-scan` (lane dedup; report `/tmp/blaze-lanes/v2-dedup-scan.md`). The fixture lookups
`declarations`, `function`, `alias`, `static_named`, `constant` are written again in four test
modules of this lane's crates; the one copy is `hir_def::test_items` (`v2/requests/db.md`,
`## dedup: hir_def::test_items`; same bodies, same panic messages). Each copy becomes a wrapper
binding the fixture's crate, so no call site changes. Apply once db lands it.

- `crates/hir-ty-codegen/v2/hashes/tests.rs`: replace `fn declarations`, `fn function` and `fn alias`
  (lines 34-74, with their `#[track_caller]`s) with
  ```rust
  #[track_caller]
  fn function(db: &TestDB, name: &str) -> FunctionId {
      hir_def::test_items::function(db, db.test_crate(), name)
  }

  #[track_caller]
  fn alias(db: &TestDB, name: &str) -> TypeAliasId {
      hir_def::test_items::type_alias(db, db.test_crate(), name)
  }
  ```
  (nothing else in the file calls `declarations`); drop imports left unused
  (`crate_def_map`, `FunctionSignature`, `TypeAliasSignature`, `ModuleDefId` if so).
- `crates/hir-ty-codegen/v2/refs/tests.rs`: replace `fn declarations`, `fn function` and
  `fn static_named` (lines 93-133) with
  ```rust
  #[track_caller]
  fn function(db: &TestDB, name: &str) -> FunctionId {
      hir_def::test_items::function(db, db.test_crate(), name)
  }

  #[track_caller]
  fn static_named(db: &TestDB, name: &str) -> StaticId {
      hir_def::test_items::static_item(db, db.test_crate(), name)
  }
  ```
  (same import cleanup).
- `crates/hir-ty-codegen/v2/tests.rs` 122-138: `fn function` becomes the same three-line
  `hir_def::test_items::function(db, db.test_crate(), name)` wrapper.
- `crates/hir-ty-mir/tests/mir/main.rs`: replace `fn declarations`, `fn function` and `fn constant`
  (lines 25-66) with
  ```rust
  #[track_caller]
  fn function(db: &TestDB, name: &str) -> FunctionId {
      hir_def::test_items::function(db, db.test_crate(), name)
  }

  #[track_caller]
  fn constant(db: &TestDB, name: &str) -> ConstId {
      hir_def::test_items::constant(db, db.test_crate(), name)
  }
  ```
  and drop `ModuleDefId`, `nameres::crate_def_map`, `signatures::{ConstSignature, FunctionSignature}`
  from its `use hir_def::{..}` if nothing else there uses them.

These take `&TestDB` as `&Db`: `hir-ty-codegen`'s is already `type TestDB = base_db::Db;`;
`hir-ty-mir/tests/mir/db.rs` is still a struct, which frontend-db-ty is turning into the same
alias (common.md, "The database is concrete").

Status: open

> applied by codegen-v2 (2026-10-29), the three `crates/hir-ty-codegen/v2/` items only: `hashes/tests.rs`, `refs/tests.rs` and `tests.rs` look their items up through `hir_def::test_items` as written in `v2/requests/db.md` (not yet in the tree, so they build once db lands it), with the imports left unused dropped. The `hir-ty-mir/tests/mir/main.rs` item is codegen-mir-store's.

> applied by codegen-mir-store (2026-10-29): `hir-ty-mir/tests/mir/main.rs`'s `function` and `constant` are the two wrappers as written, `declarations` is gone and the `use hir_def` list is `{ConstId, FunctionId}` (builds once db lands `hir_def::test_items`).

> forwarded to codegen (from requests, pass 16; told by tmux).

## dedup: blaze-engine's build-script SHA-256 onto blaze-sha256

From `dedup-codec` (report `/tmp/blaze-lanes/v2-dedup-codec.md`). The four SHA-256 copies
(`blaze-cargo/build.rs`, `blaze-engine/build.rs`, `blaze-link/src/macho/sha256.rs`,
`blaze-splice/src/sha256.rs`) are now one crate, `crates/blaze-sha256` (no dependencies;
`digest`, `fast`, and a streaming `Sha256` with `new`/`update`/`finish` that never allocates;
SHA-NI on x86-64, ARMv8 SHA-256 on aarch64, portable otherwise; every copy's tests moved there plus
a proptest against the build scripts' copy). Its workspace dependency line is requested in
`v2/requests/coordinator.md` ("## dedup: workspace dependency `blaze-sha256`").

1. `crates/blaze-engine/Cargo.toml`: add

       [build-dependencies]
       blaze-sha256.workspace = true

2. `crates/blaze-engine/build.rs`:
   - doc lines 12-14, before: "The SHA-256 is written here rather than taken from `sha2`: a
     build dependency is built for the machine that builds, and `tools/nix/units` holds one graph
     per target computed on Linux, where `sha2`'s `cpufeatures` needs no `libc`, which it needs on
     Apple's aarch64."
     after: "The SHA-256 is `blaze-sha256`'s, which has no dependencies, rather than `sha2`'s: a
     build dependency is built for the machine that builds, and `tools/nix/units` holds one graph
     per target computed on Linux, where `sha2`'s `cpufeatures` needs no `libc`, which it needs on
     Apple's aarch64."
   - after `use std::path::{Path, PathBuf};` add a blank line and `use blaze_sha256::Sha256;`.
   - delete everything from `/// SHA-256 (FIPS 180-4).` (line 168) to the end of the file. The
     call sites (`Sha256::new()`, `hash.update(..)` inside the `field` closure, `hash.finish()`)
     are unchanged.

> applied by codegen-engine (2026-10-29): `build.rs` hashes with `blaze_sha256::Sha256` (doc and
> `use` as written, its own copy deleted) and the manifest has the build dependency.

## dedup: hir-ty-codegen/v2/fold.rs's hasher forwarders and SplitMix64 onto blaze-ids

From `dedup-codec` (report `/tmp/blaze-lanes/v2-dedup-codec.md`). `hir-ty-codegen` already
depends on `blaze-ids`. New in `blaze-ids`: the macro `blaze_ids::fixed_width_writes!()`
(`crates/blaze-ids/src/std_hash.rs`: the `write_u8`..`write_isize` methods of a
`std::hash::Hasher`, each the little-endian bytes through `self.write`, `usize`/`isize` as 64 bits,
the same bytes `Le` writes) and `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`; its
`below(n)` is `mix(state += GAMMA) % n`, the same stream `Draws::below` draws).

1. `crates/hir-ty-codegen/v2/fold.rs`, `impl std::hash::Hasher for Le<'_>` (about lines 273-327):
   keep `finish` and `write`; replace `write_u8` through `write_isize` (twelve methods) by the one
   line `blaze_ids::fixed_width_writes!();`. The doc on `struct Le` stays.

2. Same file, test module, `struct Draws` (about line 1400):

   before:

       struct Draws {
           state: u64,
           count: u32,
           mutate: Option<u32>,
       }

       impl Draws {
           fn new(seed: u64, mutate: Option<u32>) -> Draws {
               Draws { state: seed, count: 0, mutate }
           }

           fn below(&mut self, n: u32) -> u32 {
               self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
               let mut z = self.state;
               z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
               z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
               let value = ((z ^ (z >> 31)) % u64::from(n)) as u32;

   after:

       struct Draws {
           rng: blaze_ids::rng::SplitMix64,
           count: u32,
           mutate: Option<u32>,
       }

       impl Draws {
           fn new(seed: u64, mutate: Option<u32>) -> Draws {
               Draws { rng: blaze_ids::rng::SplitMix64::new(seed), count: 0, mutate }
           }

           fn below(&mut self, n: u32) -> u32 {
               let value = self.rng.below(u64::from(n)) as u32;

   (the rest of `below` unchanged; the doc "SplitMix64. With `mutate`, ..." stays). Same draws for
   every seed, so no test's inputs change.

> applied by codegen-v2 (2026-10-29): `Le`'s twelve integer writes are `blaze_ids::fixed_width_writes!()` and the test `Draws` draws from `blaze_ids::rng::SplitMix64`, both as asked.

> forwarded to codegen (from requests, pass 16; told by tmux).


## speed-hir-store-paths: paths as columns (2026-10-29)

From `speed-hir-store-paths-consumers` (child of `speed-hir-store-paths`). hir-def's paths and
generic arguments are columns now (`crates/hir-def/src/expr_store/path.rs`; the API is in
`/tmp/blaze-lanes/v2-speed-hir-store-paths.md`, "The API"): `Path<'a>`, `GenericArgs<'a>`,
`AssociatedTypeBinding<'a>`, `PathGenericArgs<'a>` and `Bindings<'a>` are `Copy` views decoded
from the store; `TypeBound` and `WherePredicate` are `Copy` records whose `for<..>` binders and
`use<..>` arguments are runs (`Names`, `UseArgs`) read with `store.names(..)` and
`store.use_args(..)`. Apply these with the paths lane's own edits to `hir.rs`/`expr_store*`
(one compile). Nothing here is built (common.md).

The consumer rule:

1. `&Path` parameters and fields become `Path<'_>` (by value; `Copy`). Where a function keeps
   `&Path<'_>`, callers pass `&path`. `&store[p]` / `&map[p]` for a `PathId` -> `store.path(p)`.
2. `Path::Normal(it) => &it.mod_path` -> `it.mod_path` (already a reference).
   `it.generic_args` / `it.type_anchor` field reads are unchanged.
3. `generic_args()` yields slots by value: `.into_iter().flatten().flatten()` still gives
   `GenericArgs<'_>`; `.iter().any(Option::is_some)` -> `.iter().any(|it| it.is_some())`;
   `generic_args()?.last()?.as_ref()?` -> `generic_args()?.last()??`; `&generic_args[i]` ->
   `generic_args.get(i)`.
4. `&GenericArgs` parameters -> `GenericArgs<'_>`; `Option<&GenericArgs>` -> `Option<GenericArgs<'_>>`
   (drop `.as_ref()`/`.as_deref()`; `*args` -> `args`). `args.bindings.iter()`, `for b in
   &args.bindings`, `.first()`, `.is_empty()`, `.len()` keep working; `bindings[i]` ->
   `bindings.get(i).unwrap()` (or restructure), `&*bindings`/`bindings.as_ref()` -> iterate.
5. `binding.args.as_ref()` -> `binding.args`; `&binding.bounds` / `binding.bounds.iter()`
   unchanged (`&'a [TypeBound]`).
6. `TypeBound::ForLifetime(lifetimes, path)`: `lifetimes` is `Names`; read with
   `store.names(lifetimes)`. `TypeBound::Use(args)`: `store.use_args(args)`. `bound.clone()` ->
   `*bound`; `&TypeBound::Path(p, m)` patterns keep working.
7. `WherePredicate::TypeBound { lifetimes: Some(l), .. }`: `l` is `Names`; `store.names(l)`.
8. `Path::from_known_path_with_no_generic(m)` -> `Path::from_known_path_with_no_generic(&m)`
   (bind the `ModPath` to a local if it is a temporary used past the statement).
9. `path.qualifier()` gives `Qualifier<'_>`: use `.path()` for a `Path<'_>` (bind the
   qualifier to a local first: `let q = path.qualifier()?; let q = q.path();`).

Applied the same way everywhere, so that callers in one crate agree with signatures in another:
every `&Path`, `Option<&GenericArgs>`, `&GenericArgs`, `&[Option<GenericArgs>]` parameter,
field and return becomes the by-value view, hir-def's `Resolver::resolve_path_in_*` and
`record_path_uses` and hir-ty's path lowering included. Bodies are by-value views
(`store.expr(id) -> Expr<'_>`), so `Expr::Path(p)` binds `p: Path<'_>` and most call sites
keep their text. Also `args.args` is `&[GenericArg]` now, so `for arg in &args.args` becomes
`for arg in args.args` (`&&[T]` is not `IntoIterator`).

The edits assume, from the paths lane: `Expr::Path`/`Pat::Path`/`RecordLit`/`Pat::Record`/
`Pat::TupleStruct` hold `Path<'a>`; `Expr::MethodCall { generic_args: Option<GenericArgs<'a>> }`;
`AsmOperand::Sym(PathIdx)` read with `store.path_at(idx)` (both `pub`); `Index<PathId> for
ExpressionStore` removed; `StoreVisitor::on_path`/`on_generic_args`/`on_type_bound` taking the
views by value; `ExprCollector::lower_path -> Option<PathIdx>`.

Line numbers are as of 20:50; several of these files are being edited, so find each edit by its
old text (where the same text is on several lines, the function named beside it picks).

> forwarded to codegen (from requests, pass 16; told by tmux).


### crates/hir-ty-mir/src/mir/lower.rs

- line 660-669 (`asm!` `sym` operand; `operand: &AsmOperand`, so `path: &PathIdx`):

```old
                                self.db,
                                path,
                                HygieneId::ROOT,
```

```new
                                self.db,
                                self.store.path_at(*path),
                                HygieneId::ROOT,
```

- line 813: `path: p.clone(),` -> `path: p.mod_path().copied(),`
- line 1360: `path: path.clone(),` -> `path: path.mod_path().copied(),`
- line 2161: `path: c.clone(),` -> `path: c.mod_path().copied(),`

> forwarded to codegen (from requests, pass 16; told by tmux).

### crates/hir-ty-mir/src/mir/lower/pattern_matching.rs

- line 598: `path: p.clone(),` -> `path: p.mod_path().copied(),`

> applied by codegen-mir-store (2026-10-29): the `lower.rs` and `pattern_matching.rs` edits above
> as written (`MirLowerError::UnresolvedName { path }` is `Option<ModPath>` from the db lane's
> part); `eval.rs`'s is asked of codegen-mir.

> forwarded to codegen (from requests, pass 16; told by tmux).

### crates/hir-ty-mir/src/mir/eval.rs

- line 5324: `&hir_def::expr_store::path::Path::from_known_path_with_no_generic(path![std::fmt::format]),` -> `hir_def::expr_store::path::Path::from_known_path_with_no_generic(&path![std::fmt::format]),`
  (the `path!` temporary lives to the end of the `let .. else`; the result borrows nothing)

> forwarded to codegen (from requests, pass 16; told by tmux).

> applied by codegen-mir (2026-10-29): eval.rs's lookup of std::fmt::format passes the Path view by value.

### No change (checked)

- hir-ty-mir: `lower/as_place.rs`, `lower/match_tree.rs` (`Pat::Path(_)`, `Pat::TupleStruct
  { args, ellipsis, .. }`, `Pat::Record { args, .. }`: no path read), `const_check.rs`
  (`Expr::MethodCall { .. }`), `type_name.rs` (`StoreVisitor` impl overrides only `on_expr`,
  `on_anon_const_expr`, `on_pat`), `monomorphization.rs`, `optimize.rs`, `pretty.rs` (MIR's
  `AsmOperand::Sym`), `lower.rs` `Expr::MethodCall { receiver, args, method_name, .. }` (no
  `generic_args`), `names_constructor`'s `path.is_self_type()`, every other listed file
  (`next_solver` `GenericArgs` only).
- hir-ty-codegen: no hir-def path, generic-argument or bound use anywhere in the crate. Every hit
  is `next_solver`/`rustc_type_ir` `GenericArgs`, MIR's `AsmOperand::Sym`, `std::path::Path` or
  the symbol table's `symbols.path(..)`.
- blaze-engine (`lib.rs`, `doctest.rs` and the rest): `std::path::Path` and syntax `ast` paths
  (`call.path()`, `path.qualifier()` at lib.rs ~L2224-2276) only. `DOES_NOT_COMPILE`'s
  `"UnresolvedName"` (lib.rs L6366) matches the variant name, so the field change below keeps it
  working.

Status: open

> forwarded to codegen (from requests, pass 16; told by tmux).

## dedup: one type identity (hashes.rs onto hir-ty-share)

From `dedup-scan-ident` (lane `dedup`), 2026-10-29. Notes:
`docs/v2/plan/lanes/dedup/scan-ident.md`.

This is paired with `v2/requests/fastcompile-ident-share.md`, `## dedup: one type identity for
the code generator and the share key`, which has the full reasoning, the shared code and the
differential test. Apply that one first, since this one calls what it makes public. The
fastcompile request 1 (the share key in `Described::args`) adds the same `hir-ty-share`
dependency.

**Why.** `v2/hashes.rs`'s `Spell`, `adt_content` and `variants` are a copy of
`hir-ty-share/src/identity.rs`: the scan found seven runs, the same walk tag for tag. Also,
`FunctionCx::is_plain_data` (`mod.rs`) is a copy of `hir-ty-share/src/fact.rs`'s.

The two type spellings tell the same types apart within one revision, but not across an edit:
share's old one spelled a coroutine by `blaze_items::coroutine_code` alone, without its binding
names. This crate's spelling is the finer one, because `Hashes::ty` is also the `TypeId` value
(`places.rs` `fill_type_ids`) and the debug-info type (`side::Variable::ty`), and a coroutine's
debug info names its saved locals. With the share key in `Described::args`, the coarser spelling
would miss an input of the cache key.

The fix is one spelling, this crate's semantics, in `hir_ty_share::identity`. The body code
(`with_spans`, `names`, `table_of`, `item_code`, `closure_code_in`, `coroutine_code_in`,
`by_move_code_in`) moves with it, verbatim, into `hir_ty_share::code`, because a coroutine's
identity holds its compiled code. Consequences:

- `Hashes::code` values do not change: same bytes, same domains.
- `Hashes::ty` and `Hashes::konst` values change: new domains, and the FnPtr ABI is spelled as
  share's (`Debug` text ended by `0xff`). The types they tell apart do not change; share's
  differential test checks this.
- Error strings become `Unshared`'s `Display`, for example `no content: a type with an error`.

`hashes.rs` is being moved to `&Db` by `frontend-db` right now. The text below is written
against that `&Db` state; apply it after that edit lands, or merge by function name.

> forwarded to codegen (from requests, pass 16; told by tmux).

### 1. `crates/hir-ty-codegen/Cargo.toml`

Under `[dependencies]`, after `hir-ty-mir.workspace = true`, add the line below (it is the
same line as fastcompile request 1; add it once):

```toml
hir-ty-share.workspace = true
```

No cycle: `hir-ty-share` depends on `base-db`, `hir-def`, `hir-ty`, `hir-ty-mir`, `blaze-ids`
and `blaze-items`, and none of these depends on `hir-ty-codegen`.

> forwarded to codegen (from requests, pass 16; told by tmux).
> applied by codegen-v1 (2026-10-29): the line is in `crates/hir-ty-codegen/Cargo.toml` (manifest lint passes). Item 3 (`is_plain_data` in `mod.rs`) waits on `hir_ty_share::is_plain_data` being public (`v2/requests/fastcompile-ident-share.md`); it is not yet, so `FunctionCx::is_plain_data` stays until it is.

### 2. `crates/hir-ty-codegen/v2/hashes.rs` (replace the whole file)

Deleted from today's file:

- the domains `TY_DOMAIN`, `CONST_DOMAIN`, `ADT_DOMAIN`, `CODE_DOMAIN`, `DISCRIMINANT_DOMAIN`
  and `NAMES_DOMAIN` (nothing outside this file names them);
- the table constants `EXPRS` to `UNKNOWN` and `TABLES`, now `hir_ty_share::code::*`;
- `ITEM`, `CLOSURE`, `COROUTINE`, `BY_MOVE`;
- `table_of`, `unhashable`, `with_spans`, `names`, `no_mir`, `item_code`, `closure_code_in`,
  `coroutine_code_in`, `by_move_code_in`, `adt_content`, `variants` and `Spell`.

Kept: `SPAN_DOMAIN`, `DECLARATION`, `ItemHashes`, `Anchors`, `anchors`.

```rust
//! [`super::Hashes`] over the front end's code identity: what the code generator puts in place
//! of the database's dense ids.
//!
//! - A body's code is [`hir_ty_share::code`]'s: its item's code hash, which takes each callee by
//!   its interface hash, with its bindings' names and the positions its MIR names (the spans of
//!   [`ItemHashes::span`]) beside it: a template that names an expression by its id is never
//!   reused once that id is another expression's.
//! - A type or constant is [`hir_ty_share::identity`]'s spelling by content, the one the share
//!   key reads too: an ADT by its key, its interface, its layout and what drop glue reads of
//!   it, a coroutine by its compiled code, and its arguments the same way. Regions are left out.
//! - A position is a [`SpanRef`] into one of the tables of the body's owner, one per kind of
//!   span ([`hir_ty_share::code::table_of`]), indexed by the span's id in the owner's store: an
//!   edit elsewhere in the file moves no index, and a formatting edit changes the table only
//!   ([`ItemHashes::span_tables`]).
//!
//! Nothing here is shared between threads but salsa's memos: no lock on any path.

#[cfg(test)]
mod tests;

use base_db::Db;
use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemId, ItemKey, item_key, key};
use hir_def::{DefWithBodyId, ExpressionStoreOwnerId, FunctionId, expr_store::Body as BodyStore};
use hir_ty::{
    db::{InternedClosureId, InternedCoroutineId},
    next_solver::{Const, Ty},
};
use hir_ty_mir::mir::MirSpan;
use hir_ty_share::{
    code::{self, TABLES},
    identity,
};
use salsa::plumbing::FromId;

use super::{Body, Hashes};

pub const SPAN_DOMAIN: &str = "blaze v2 codegen span table 1";

/// The table of a function's declaration, apart from its body's.
const DECLARATION: u8 = TABLES as u8;

/// The items lane's hashes as the code generator asks for them, and what the program calls
/// each file.
pub struct ItemHashes {
    names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>,
}

impl ItemHashes {
    /// `names` says what the program calls a file (`file!()`): its path relative to its
    /// package's root, `None` for a file the build did not name.
    pub fn new(names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>) -> ItemHashes {
        ItemHashes { names }
    }

    /// Every span table of `owner`'s body that [`Hashes::span`] names, by its anchor: the span
    /// at each index, for the linker's table to give the position of
    /// (`hir_ty_mir::mir::location::source_location_in_file`). A span that function finds no syntax for
    /// (a binding, the self parameter, [`MirSpan::Unknown`]) is where its owner is. The tables
    /// are in [`code::table_of`]'s order.
    pub fn span_tables(db: &Db, owner: DefWithBodyId) -> Vec<(ContentId, Vec<MirSpan>)> {
        let anchors = anchors(db, owner).0;
        let store = BodyStore::of(db, owner);
        let spans: [Vec<MirSpan>; TABLES] = [
            store.exprs().map(|(it, _)| MirSpan::ExprId(it)).collect(),
            store.exprs().map(|(it, _)| MirSpan::ExprEnd(it)).collect(),
            store.pats().map(|(it, _)| MirSpan::PatId(it)).collect(),
            store.bindings().map(|(it, _)| MirSpan::BindingId(it)).collect(),
            vec![MirSpan::SelfParam],
            vec![MirSpan::Unknown],
        ];
        anchors.into_iter().zip(spans).collect()
    }

    /// The anchor of the table [`Hashes::declaration`] names: one span, where `func` is
    /// declared.
    pub fn declaration_table(db: &Db, func: FunctionId) -> ContentId {
        let mut h = Hasher::new(SPAN_DOMAIN);
        h.id(&item_key(db, ItemId::Function(func)).0).update(&[DECLARATION]);
        h.finish()
    }
}

impl Hashes for ItemHashes {
    fn item(&self, db: &Db, item: ItemId) -> Result<ItemKey, String> {
        Ok(item_key(db, item))
    }

    fn code(&self, db: &Db, body: Body) -> Result<CodeHash, String> {
        hir_ty::attach_db(db, || {
            let code = match body {
                Body::Item(def) => code::of_item(db, def),
                Body::Closure(id) => code::of_closure(db, InternedClosureId::from_id(id)),
                Body::Coroutine(id) => code::of_coroutine(db, InternedCoroutineId::from_id(id)),
                Body::CoroutineByMove(id) => {
                    code::of_coroutine_by_move(db, InternedCoroutineId::from_id(id))
                }
            };
            code.clone().map(CodeHash).map_err(|why| why.to_string())
        })
    }

    fn ty<'db>(&self, db: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        hir_ty::attach_db(db, || identity::ty(db, ty).map_err(|why| why.to_string()))
    }

    fn konst<'db>(&self, db: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        hir_ty::attach_db(db, || identity::konst(db, konst).map_err(|why| why.to_string()))
    }

    fn span(
        &self,
        db: &Db,
        owner: DefWithBodyId,
        span: MirSpan,
    ) -> Result<SpanRef, String> {
        let Ok((table, index)) = code::table_of(span) else {
            return Err(format!(
                "{span:?} of the body of {owner:?}, a span of code inlined into it, which is of \
                 the body it was inlined from: its place is the inlined span's"
            ));
        };
        Ok(SpanRef { anchor: anchors(db, owner).0[table], index })
    }

    fn declaration(&self, db: &Db, func: FunctionId) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: ItemHashes::declaration_table(db, func), index: 0 })
    }

    fn file_name(&self, _: &Db, file: base_db::FileId) -> Result<String, String> {
        (self.names)(file).ok_or_else(|| {
            format!(
                "the file {file:?} has no name in the program: the build did not name it, so \
                 `file!()` and the positions of its code have none"
            )
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Anchors([ContentId; TABLES]);

/// The anchors of a body's span tables: of the key of its store, which an edit of what the
/// body is keeps.
#[salsa::tracked(returns(copy))]
fn anchors(db: &Db, owner: DefWithBodyId) -> Anchors {
    let store = key::store_key(db, ExpressionStoreOwnerId::Body(owner));
    Anchors(std::array::from_fn(|table| {
        let mut h = Hasher::new(SPAN_DOMAIN);
        h.id(&store.0).update(&[table as u8]);
        h.finish()
    }))
}
```

`hir_ty_mir::mir::MirSpan` is `hir_ty::mir::MirSpan` re-exported (`hir-ty-mir/src/mir.rs`
`pub use hir_ty::mir::*`), so it is the type `code::table_of` takes.

`v2/hashes/tests.rs` stays as it is. It goes through `ItemHashes` and `Hashes`, whose API does
not change, and its assertions hold under the shared spelling:

- a type is its arguments and not its alias;
- a fields' type's drop impl is in the type;
- a constant is its value;
- the callee's body edit and the local rename (both of `Hashes::code`, which is
  bit-identical).

> forwarded to codegen (from requests, pass 16; told by tmux).
> applied by codegen-v2 (2026-10-29): `v2/hashes.rs` is the file above, word for word (`hir_ty_share::{code, identity}` are public in the tree; the kept items matched today's file).

### 3. `crates/hir-ty-codegen/mod.rs`: `FunctionCx::is_plain_data` onto `hir_ty_share::is_plain_data`

In `reference_pointee`, before:

```rust
        if !element.is_none_or(|it| self.is_plain_data(it, 0)) {
            return Ok(None);
        }
```

After:

```rust
        let plain = |it: Ty<'db>| {
            hir_ty_share::is_plain_data(self.cg.db, self.interner, &self.infcx, self.env.param_env, it)
        };
        if !element.is_none_or(plain) {
            return Ok(None);
        }
```

Then delete the method `FunctionCx::is_plain_data`: from its doc comment `/// Whether `ty`
is made of scalars, pointers and references only, through arrays, tuples` down to its closing
brace (on disk about lines 11230 to 11272, just after `reference_pointee`). It has no other
caller: `rg -n 'is_plain_data' crates/hir-ty-codegen` finds only the call in
`reference_pointee` and the method's own recursion.

`hir_ty_share::is_plain_data` is the same body. The one difference is that its normalization
(hir-ty's `traits::structurally_normalize_ty`) also turns inference variables into errors.
That changes no answer, because both reach `_ => false`. Share's
`is_plain_data_is_the_code_generators` test checks it on the corpus against a copy of this
method.

> forwarded to codegen (from requests, pass 16; told by tmux).

### Not part of this request (recorded so nobody merges them)

- `v2/frontend.rs` `args_of` composes `Hashes::ty` and `Hashes::konst` over an instance's
  arguments. It is not a second spelling: it goes through the `Hashes` seam that
  `v2/tests.rs` and `v2/refs/tests.rs` fake with `ById`. It stays.
- `type_id.rs` (rustc's `TypeId` stable hash, for code that compares with rustc's),
  `stable.rs` (stable symbol names) and `fncache.rs` walk types too, but compute other things,
  on the v1 paths.
- `blaze-items/src/form.rs` spells types for the code and interface hashes: by item key, with
  regions, recording rather than failing. That is another grammar on purpose.
- `v2/shape.rs` `part` tags, `hir-ty-share` `walk.rs` and `fact.rs` `tag`, and
  `blaze-link/src/wasm/object.rs` `id`: the scan matched them, but they are separate schemas
  numbered 1 to N (the wasm ones are fixed by the wasm spec). They are not copies.
- `v2/fold.rs` `Le` is a little-endian `std::hash::Hasher` feed, as are
  `hir-ty-share/src/layout.rs`'s `Feed` and `blaze-hashcons`' `hash_std`. That is sibling
  `dedup-codec`'s, which has been told.

> forwarded to codegen (from requests, pass 16; told by tmux).

## dedup: bits: hir-ty-mir, hir-ty-codegen, blaze-codegen onto `blaze_ids::bits`

From `dedup-bits-codegen` (report `/tmp/blaze-lanes/v2-dedup-bits-codegen.md`; parent
`dedup-bits`). Every bit set below becomes the one family in `crates/blaze-ids/src/bits.rs`:
`BitSet<I>` (`Bits<Vec<u64>, I>`), `Bits<SmallVec<[u64; 4]>>`, `BitMatrix<I>`. All three crates
already depend on `blaze-ids`. Same members, same results, same panics on a bad index for
`insert` (past the capacity); `contains`/`remove` past the capacity are `false` where the copies
indexed out of bounds, and every caller here only asks below the domain it sized the set to.
Nothing below is persisted or hashed. After it, `hir-ty-mir` names `rustc_index` nowhere, so it
drops `ra-ap-rustc_index`; `hir-ty-codegen` keeps it for `rustc_index::Idx`.

> forwarded to codegen (from requests, pass 16; told by tmux).

### hir-ty-mir

1. `crates/hir-ty-mir/src/mir/transform/move_paths.rs`: the `Bits` copy goes; `InitState`
   holds `BitSet`s.

   Imports, before:

       use la_arena::ArenaMap;
       use rustc_hash::{FxHashMap, FxHashSet};

   after:

       use blaze_ids::bits::BitSet;
       use la_arena::ArenaMap;
       use rustc_hash::{FxHashMap, FxHashSet};

   Delete, whole (about lines 391-421):

       /// A set of move paths.
       #[derive(Clone, PartialEq, Eq, Debug)]
       pub(super) struct Bits(Vec<u64>);

       impl Bits {
           ...
           /// Adds the members of `other`, and says whether that changed anything.
           fn union(&mut self, other: &Bits) -> bool {
               ...
           }
       }

   `InitState`, before:

       pub(super) struct InitState {
           pub(super) maybe_init: Bits,
           pub(super) maybe_uninit: Bits,
       }

       impl InitState {
           pub(super) fn bottom(len: usize) -> Self {
               InitState { maybe_init: Bits::new(len), maybe_uninit: Bits::new(len) }
           }

           fn join(&mut self, other: &InitState) -> bool {
               let init = self.maybe_init.union(&other.maybe_init);
               let uninit = self.maybe_uninit.union(&other.maybe_uninit);
               init || uninit
           }

   after:

       pub(super) struct InitState {
           pub(super) maybe_init: BitSet<PathId>,
           pub(super) maybe_uninit: BitSet<PathId>,
       }

       impl InitState {
           pub(super) fn bottom(len: usize) -> Self {
               InitState { maybe_init: BitSet::new(len), maybe_uninit: BitSet::new(len) }
           }

           fn join(&mut self, other: &InitState) -> bool {
               let init = self.maybe_init.union_with(&other.maybe_init);
               let uninit = self.maybe_uninit.union_with(&other.maybe_uninit);
               init || uninit
           }

   `InitState::apply`, before (`insert` now returns whether it added, which `for_each` won't
   take):

               Change::Borrowed => paths.iter().for_each(|&path| self.maybe_init.insert(path)),

   after:

               Change::Borrowed => {
                   for &path in paths {
                       self.maybe_init.insert(path);
                   }
               }

   `InitAnalysis::run`, before:

                   if skip_init {
                       unwound.maybe_init = Bits::new(len);
                   }
                   if skip_uninit {
                       unwound.maybe_uninit = Bits::new(len);
                   }

   after:

                   if skip_init {
                       unwound.maybe_init.clear();
                   }
                   if skip_uninit {
                       unwound.maybe_uninit.clear();
                   }

   (the same empty set of `len` paths, its words kept rather than a new allocation; `clear` is
   `fill(0)`). The calls `insert`, `remove` and `contains` in `set` and `of` and in
   `holds_nothing` (`state.maybe_init.contains(it)`) are unchanged.

2. `crates/hir-ty-mir/src/mir/optimize/dse.rs`: the `Bits` copy goes.

   Before:

       use crate::mir::{
           LocalId, MirBody, OperandKind, ProjectionElem, Rvalue, StatementKind, TerminatorKind,
           return_slot,
       };

   after:

       use blaze_ids::bits::BitSet;

       use crate::mir::{
           LocalId, MirBody, OperandKind, ProjectionElem, Rvalue, StatementKind, TerminatorKind,
           return_slot,
       };

   Delete, whole (lines 21-42): `#[derive(Clone, PartialEq, Eq)] struct Bits(Vec<u64>);` and its
   `impl Bits { new, contains, insert, remove, union }`.

   Then, in the file, every type `Bits` becomes `BitSet` (`always_live: &Bits` in
   `removable_destination` and `statement_effect`; `live: &mut Bits` in `transfer`,
   `statement_effect`, `terminator_effect`, `insert_index_locals`; `Vec<Bits>` and `&[Bits]` in
   `dead_store_elimination`), every `Bits::new(` becomes `BitSet::new(` (four: `always_live`,
   `entry`, `exit_of`'s `live`, `remove_unused_storage_markers`' `used`), and:

   `transfer`, before:

           Mention::Assigned | Mention::CallReturned => live.remove(local_index(local)),
           Mention::AssignedPart | Mention::Storage => (),
           Mention::Read | Mention::SharedBorrow | Mention::Mutated => live.insert(local_index(local)),

   after (the arms are `()` again, `remove`/`insert` returning a `bool` now):

           Mention::Assigned | Mention::CallReturned => {
               live.remove(local_index(local));
           }
           Mention::AssignedPart | Mention::Storage => (),
           Mention::Read | Mention::SharedBorrow | Mention::Mutated => {
               live.insert(local_index(local));
           }

   `exit_of`, before:

                   live.union(&entry[block_index(target)]);

   after:

                   live.union_with(&entry[block_index(target)]);

   Every other `insert`/`remove`/`contains` call there is already a statement or a test.

3. `crates/hir-ty-mir/src/mir/storage.rs`: rustc_index's `DenseBitSet`/`BitMatrix` (local to the
   pass; `StorageConflicts`' fields are private) become `BitSet`/`BitMatrix`. `DenseBitSet` keeps
   its words in a `Vec<u64>` (ra-ap-rustc_index 0.166), so the allocations are the same.

   Before:

       use rustc_index::bit_set::{BitMatrix, DenseBitSet};

   after:

       use blaze_ids::bits::{BitMatrix, BitSet};

   `StorageConflicts`, before:

           always: DenseBitSet<usize>,
           conflicts: BitMatrix<usize, usize>,

   after:

           always: BitSet<usize>,
           conflicts: BitMatrix<usize>,

   (`conflict`'s `self.conflicts.contains(a, b)` is unchanged.)

   In `frame_storage_conflicts`, every `DenseBitSet<usize>` becomes `BitSet<usize>` (the
   closures `read`, `write`, `through_statement`, `live_in`, `through_terminator`, `entry` and
   `holding`), and:

   - `DenseBitSet::new_empty(count)` becomes `BitSet::new(count)` (seven: `marked`, `borrowed`,
     `entry[start]`, `through_terminator`'s `live`, `holding`'s `vec!`, the two `None =>` arms);
   - `DenseBitSet::new_filled(count)` becomes `BitSet::filled(count)` (`always`, `tracked`);
   - `.intersect(&` becomes `.intersect_with(&` (`always.intersect(&borrowed)`,
     `marked.intersect(&borrowed)`);
   - `.union(&` becomes `.union_with(&`, all ten (`state.union(&live)` in the marked-locals
     flow, whose `bool` is `changed`; the four `live.union(&live_in(..))` in
     `through_terminator`; the five `at_once.union(..)` below);
   - `.subtract(&` stays (`always.subtract(&marked)`, `tracked.subtract(..)` twice,
     `marked.subtract(&always)`).

   `all_pairs`, before:

           let mut conflicts = BitMatrix::new(count, count);
           let all_pairs = |conflicts: &mut BitMatrix<usize, usize>, at_once: &DenseBitSet<usize>| {
               for at in at_once.iter() {
                   conflicts.union_row_with(at_once, at);
               }
           };

   after (the row first here):

           let mut conflicts = BitMatrix::new(count, count);
           let all_pairs = |conflicts: &mut BitMatrix<usize>, at_once: &BitSet<usize>| {
               for at in at_once.iter() {
                   conflicts.union_row_with(at, at_once);
               }
           };

4. `crates/hir-ty-mir/src/lib.rs`, before:

       extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
       extern crate ra_ap_rustc_index as rustc_index;

   after:

       extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;

5. `crates/hir-ty-mir/Cargo.toml`: delete the line `ra-ap-rustc_index.workspace = true`
   (`rg rustc_index crates/hir-ty-mir` finds nothing else after 3 and 4).

> forwarded to codegen (from requests, pass 16; told by tmux).

### hir-ty-codegen

6. `crates/hir-ty-codegen/overlap.rs`, before:

       use rustc_hash::FxHashMap;
       use rustc_index::bit_set::{BitMatrix, DenseBitSet};

   after:

       use blaze_ids::bits::{BitMatrix, BitSet};
       use rustc_hash::FxHashMap;

   `FrameLocals`, before:

           pub(super) conflicts: BitMatrix<usize, usize>,

   after:

           pub(super) conflicts: BitMatrix<usize>,

   `share_slots`, before:

           let mut groups: Vec<(ir::StackSlot, DenseBitSet<usize>)> = Vec::new();
           ...
               groups.push((slot(at), DenseBitSet::new_empty(frame.slots.len())));
           ...
               let into = &mut groups[index];
               for it in frame.conflicts.iter(at) {
                   into.1.insert(it);
               }

   after (the row or'ed in a word at a time, as the comment above `groups` says; the matrix's
   rows are `frame.slots.len()` bits, as both makers of `FrameLocals` size it):

           let mut groups: Vec<(ir::StackSlot, BitSet<usize>)> = Vec::new();
           ...
               groups.push((slot(at), BitSet::new(frame.slots.len())));
           ...
               let into = &mut groups[index];
               into.1.union_with(&frame.conflicts.row(at));

   The test module (`use super::*`) is unchanged: `BitMatrix::new(locals, locals)`,
   `conflicts.insert(a, b);` and `frame.conflicts.contains(it, at)` read the same.

7. `crates/hir-ty-codegen/mod.rs`, about line 13336, before:

           let mut conflicts = rustc_index::bit_set::BitMatrix::new(count, count);

   after:

           let mut conflicts = blaze_ids::bits::BitMatrix::new(count, count);

8. `crates/hir-ty-codegen/v2/refs.rs`: `Blocks` keeps its name and its `BasicBlockId`
   methods (an `la_arena::Idx` can't implement `blaze_ids::bits::BitIndex` from here or from
   hir-ty, the orphan rule), its bits a `BitSet`.

   Before:

       use salsa::plumbing::AsId;
       use smallvec::{SmallVec, smallvec};

   after:

       use blaze_ids::bits::BitSet;
       use salsa::plumbing::AsId;
       use smallvec::{SmallVec, smallvec};

   Before (about lines 419-444):

       /// A set of the blocks of a body, a bit a block.
       struct Blocks(Vec<u64>);

       impl Blocks {
           fn new(count: usize) -> Self {
               Blocks(vec![0; count.div_ceil(64)])
           }

           fn bit(id: BasicBlockId) -> (usize, u64) {
               let index = id.into_raw().into_u32() as usize;
               (index / 64, 1 << (index % 64))
           }

           /// Adds `id`, saying whether it was not in the set.
           fn insert(&mut self, id: BasicBlockId) -> bool {
               let (word, bit) = Self::bit(id);
               let new = self.0[word] & bit == 0;
               self.0[word] |= bit;
               new
           }

           fn contains(&self, id: BasicBlockId) -> bool {
               let (word, bit) = Self::bit(id);
               self.0[word] & bit != 0
           }
       }

   after:

       /// A set of the blocks of a body, a bit a block, by the block's index in the body's arena.
       struct Blocks(BitSet);

       impl Blocks {
           fn new(count: usize) -> Self {
               Blocks(BitSet::new(count))
           }

           /// Adds `id`, saying whether it was not in the set.
           fn insert(&mut self, id: BasicBlockId) -> bool {
               self.0.insert(id.into_raw().into_u32() as usize)
           }

           fn contains(&self, id: BasicBlockId) -> bool {
               self.0.contains(id.into_raw().into_u32() as usize)
           }
       }

   Callers (`reachable`, and the walk at about lines 874-898) are unchanged.

9. `crates/hir-ty-codegen/v2/backend.rs` `colocate`: the inline words become a `Bits` over the
   same `SmallVec<[u64; 4]>` (no allocation up to 256 callees, as before).

   Before:

       use blaze_ids::ContentId;

   after:

       use blaze_ids::{ContentId, bits::Bits};

   Before:

           // One bit a callee: no allocation for the usual few hundred.
           let mut addressed: SmallVec<[u64; 4]> = SmallVec::new();
           addressed.resize(stencil.dfg.ext_funcs.len().div_ceil(64), 0);
           for block in stencil.layout.blocks() {
               for inst in stencil.layout.block_insts(block) {
                   if let ir::InstructionData::FuncAddr { func_ref, .. } = stencil.dfg.insts[inst] {
                       let at = func_ref.as_u32() as usize;
                       addressed[at / 64] |= 1 << (at % 64);
                   }
               }
           }
           for (func_ref, ext) in stencil.dfg.ext_funcs.iter_mut() {
               let at = func_ref.as_u32() as usize;
               match own(&ext.name) {
                   Some(true) => ext.colocated = true,
                   Some(false) if addressed[at / 64] & (1 << (at % 64)) == 0 => ext.colocated = true,

   after:

           // One bit a callee: no allocation for the usual few hundred.
           let mut addressed: Bits<SmallVec<[u64; 4]>> = Bits::new(stencil.dfg.ext_funcs.len());
           for block in stencil.layout.blocks() {
               for inst in stencil.layout.block_insts(block) {
                   if let ir::InstructionData::FuncAddr { func_ref, .. } = stencil.dfg.insts[inst] {
                       addressed.insert(func_ref.as_u32() as usize);
                   }
               }
           }
           for (func_ref, ext) in stencil.dfg.ext_funcs.iter_mut() {
               let at = func_ref.as_u32() as usize;
               match own(&ext.name) {
                   Some(true) => ext.colocated = true,
                   Some(false) if !addressed.contains(at) => ext.colocated = true,

   (`use smallvec::SmallVec;` stays: `wanted` at about line 451 uses it too.)

> forwarded to codegen (from requests, pass 16; told by tmux).
> applied by codegen-v1 (2026-10-29): items 6 (`overlap.rs`) and 7 (`mod.rs`), as written; items 8 and 9 are in `v2/`, codegen-v2's.
> applied by codegen-v2 (2026-10-29): items 8 (`v2/refs.rs` `Blocks` over a `BitSet`) and 9 (`v2/backend.rs` `colocate` over `Bits<SmallVec<[u64; 4]>>`), as written.

### blaze-codegen

10. `crates/blaze-codegen/src/imports.rs`: the `Bits` copy (node numbers, `u32`) goes.

    Before:

        use rayon::prelude::*;
        use rustc_hash::{FxHashMap, FxHashSet};

    after:

        use blaze_ids::bits::BitSet;
        use rayon::prelude::*;
        use rustc_hash::{FxHashMap, FxHashSet};

    Delete, whole (about lines 141-160): `/// A set of node numbers, one bit each.`,
    `struct Bits(Vec<u64>);` and its `impl Bits { new, get, set, unset }`.

    Then: the field types `closed: Bits` (in `Graph`) and `reached: Bits` (in `Passed`) become
    `closed: BitSet<u32>` and `reached: BitSet<u32>`; the four `Bits::new(` (`closed`,
    `addressed` in `Graph::of`, `reached` in `constants`, `on_stack` in `components`) become
    `BitSet::new(`; on those sets `get` becomes `contains`, `set` becomes `insert` and `unset`
    becomes `remove`. Every `set`/`unset` call is a statement, so the `bool` `insert`/`remove`
    now return is dropped.

    Exactly, by line in the current file:

    - 271 `addressed.set(node);` → `addressed.insert(node);`
    - 277 `!addressed.get(node as u32)` → `!addressed.contains(node as u32)`
    - 278 `graph.closed.set(node as u32);` → `graph.closed.insert(node as u32);`
    - 306 `self.closed.get(node as u32)` → `self.closed.contains(node as u32)`
    - 314 `!self.closed.get(callee)` → `!self.closed.contains(callee)`
    - 319 `!passed.reached.get(callee)` → `!passed.reached.contains(callee)`
    - 320 `passed.reached.set(callee);` → `passed.reached.insert(callee);`
    - 422 `passed.reached.get(importer)` → `passed.reached.contains(importer)`
    - 441 `self.closed.get(it)` → `self.closed.contains(it)`
    - 567 `on_stack.set(root);` → `on_stack.insert(root);`
    - 578 `on_stack.set(to);` → `on_stack.insert(to);`
    - 580 `on_stack.get(to)` → `on_stack.contains(to)`
    - 593 `on_stack.unset(member);` → `on_stack.remove(member);`

    (`call.args.get(at)`, `constants.get(key)`, `edges[node].get(edge)` and `index.get(..)` are
    not sets and stay.)

> forwarded to codegen (from requests, pass 16; told by tmux).
> applied by codegen-v2 (2026-10-29): item 10 as written (the local sets as `BitSet::<u32>::new`, so their index type doesn't rest on inference).

## dedup: kind_tag is pub (from dedup-scan)

`crates/blaze-codegen/src/content.rs`: `pub(crate) fn kind_tag(kind: RelocKind) -> u8` →
`pub fn kind_tag(kind: RelocKind) -> u8`, with the doc `/// The byte a [`RelocKind`] hashes as
in a content key, and in blaze-premap's.` blaze-premap deletes its copy `reloc_tag`
(fastcompile.md).

> forwarded to codegen (from requests, pass 16; told by tmux).
> applied by codegen-v2 (2026-10-29): `blaze_codegen::content::kind_tag` is `pub` with that doc; blaze-premap's `reloc_tag` is fastcompile's.

## dedup: one FrameRule in blaze-codegen (from dedup-scan)

hir-ty-codegen `unwind.rs`'s `FrameRule` and blaze-stencil `code.rs`'s `Rule` are one enum (stencil
adds `RaSignState`); both crates depend on blaze-codegen, which has serde's derive.

- New `crates/blaze-codegen/src/unwind.rs`, `pub mod unwind;` in lib.rs:
  ```rust
  //! The call frame rules a function's code gives its unwinder, as DWARF has them.

  /// A DWARF call frame instruction. Registers are DWARF register numbers, offsets are bytes.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
  pub enum FrameRule {
      Cfa(u16, i32),
      CfaRegister(u16),
      CfaOffset(i32),
      Offset(u16, i32),
      Restore(u16),
      SameValue(u16),
      Undefined(u16),
      RememberState,
      RestoreState,
      ArgsSize(u32),
      /// AArch64's return-address signing state from here (`true`: signed), DWARF's
      /// `RA_SIGN_STATE` pseudo-register 34 as `ValExpression(34, [DW_OP_lit0 or DW_OP_lit1])`,
      /// as Cranelift's `Aarch64SetPointerAuth` row writes it.
      RaSignState(bool),
  }
  ```
  `RaSignState` is last, so every `FrameRule` persisted today decodes the same.
- `crates/hir-ty-codegen/unwind.rs`: delete the enum, `use blaze_codegen::unwind::FrameRule;`
  with `pub(super) use` where `super::` names it. The `impl FrameRule` becomes two free
  functions (an inherent impl can't stay on a foreign type): `pub(super) fn on_entry(arch:
  super::Arch) -> &'static [FrameRule]` (same `#[cfg(feature = "llvm")]`) and `fn
  instruction(rule: FrameRule) -> CallFrameInstruction`, which gains
  ```rust
  FrameRule::RaSignState(signed) => {
      let mut expression = gimli::write::Expression::new();
      expression.op(if signed { gimli::DW_OP_lit1 } else { gimli::DW_OP_lit0 });
      CallFrameInstruction::ValExpression(register(34), expression)
  }
  ```
  Callers: `rule.instruction()` → `instruction(rule)` in unwind.rs, and
  `FrameRule::on_entry(arch)` → `super::unwind::on_entry(arch)` in `llvm/emitted.rs`.
- blaze-stencil re-exports it as its `Rule` (fastcompile.md).

> forwarded to codegen (from requests, pass 16; told by tmux).
> applied by codegen-v2 (2026-10-29), the blaze-codegen part: `crates/blaze-codegen/src/unwind.rs` with `FrameRule` as written and `pub mod unwind;`. The `hir-ty-codegen/unwind.rs` and `llvm/emitted.rs` part is codegen-v1's, blaze-stencil's re-export fastcompile's (both told by tmux).
> applied by codegen-v1 (2026-10-29): `hir-ty-codegen/unwind.rs` drops its enum for `pub(super) use blaze_codegen::unwind::FrameRule;`, with `on_entry` and `instruction` free functions (`RaSignState` as `ValExpression(34, lit0/lit1)`), and `llvm/emitted.rs` calls `crate::unwind::on_entry(arch)`.

## dedup: no-op landing pads once, in hir-ty-mir (from dedup-scan)

`hir-ty-codegen/mod.rs`'s `fn resumes_only(body: &MirBody<'db>) -> FxHashSet<BasicBlockId>` is
the fixpoint of `hir-ty-mir/src/mir/transform/cleanup.rs`'s `remove_noop_landing_pads` (rustc's
`RemoveNoopLandingPads`: cleanup, only storage/fake-read/nop statements, resume or goto one).
Same set; the cleanup copy only skips ones already found.

- `cleanup.rs`: split the loop out as
  ```rust
  /// The cleanup blocks of `body` that do nothing but go on unwinding: only storage markers,
  /// fake reads and nops, ending in resuming the unwind or going to such a block.
  pub fn noop_landing_pads(body: &MirBody<'_>) -> FxHashSet<BasicBlockId> {
      let mut noop: FxHashSet<BasicBlockId> = FxHashSet::default();
      loop { /* the loop as it is, `break` → `return noop` */ }
  }

  pub(in crate::mir) fn remove_noop_landing_pads(body: &mut MirBody<'_>) {
      let noop = noop_landing_pads(body);
      for (_, block) in body.basic_blocks.iter_mut() { /* as it is */ }
  }
  ```
  `transform.rs`: `pub(super) use cleanup::remove_noop_landing_pads;` →
  `pub use cleanup::{noop_landing_pads, remove_noop_landing_pads};` (the second stays
  `pub(in crate::mir)` at its definition). `mir.rs`: add `noop_landing_pads` to `pub use transform::{…}`.
- `hir-ty-codegen/mod.rs`: delete `fn resumes_only` and its doc, keeping the why in one line at
  the call: `// rustc's RemoveNoopLandingPads: such a pad costs a call LLVM doesn't know resumes.`
  `Self::resumes_only(body)` → `hir_ty_mir::mir::noop_landing_pads(body)`; in `v2/refs.rs`
  `FunctionCx::resumes_only(body)` → the same.

> forwarded to codegen (from requests, pass 16; told by tmux).
> applied by codegen-v1 (2026-10-29): the `hir-ty-codegen/mod.rs` part (`resumes_only` deleted, the call is `hir_ty_mir::mir::noop_landing_pads(body)`); the `cleanup.rs`/`transform.rs`/`mir.rs` part asked of codegen-mir-store and `v2/refs.rs` of codegen-v2 (`~/restart-2026-10-28/briefs/v2/codegen-v1.requests.md`).
> applied by codegen-mir-store (2026-10-29): `cleanup.rs` has `pub fn noop_landing_pads` (the fixpoint) and `remove_noop_landing_pads` uses it; `transform.rs` re-exports it `pub` (the remover stays `pub(super)`, as a `pub use` of a `pub(in crate::mir)` fn doesn't compile) and `mir.rs` adds it to `pub use transform::{..}`.
> applied by codegen-v2 (2026-10-29): `v2/refs.rs` `reachable` calls `hir_ty_mir::mir::noop_landing_pads(body)`.

## dedup: optimize.rs successors are TerminatorKind::all_successors (from dedup-scan)

`hir-ty-mir/src/mir/optimize.rs`'s `fn successors(kind: &TerminatorKind) ->
SmallVec<[BasicBlockId; 4]>` is borrowck's `cfg.rs` `successors` and blaze-items' `form.rs`
`push_successors` reversed: every successor, unwind last. After db adds
`TerminatorKind::all_successors` (db.md), delete `fn successors` and call
`kind.all_successors()` where it was called (the stack becomes `SmallVec<[BasicBlockId; 2]>`
where it was `[_; 4]`, or `.into_iter()` into the existing one). `successors_mut` stays.

> forwarded to codegen (from requests, pass 16; told by tmux).

## codegen-mir-store: the store keeps its macro calls without the source map (to format; 2026-10-29)

From `codegen-mir-store` (child of codegen), for "frontend: MIR reads source maps only where a
position is emitted" above. `crates/hir-def/src/expr_store.rs` and `expr_store/lower.rs` are
format's.

What: `ExpressionStore` keeps the `MacroCallId` of every macro call lowered into it, recorded
whether or not the source map is (`record`), with
```rust
impl ExpressionStore {
    /// Every macro call lowered into the store, in the order lowering met them: the calls of
    /// [`ExpressionStoreSourceMap::macro_calls`], without the map.
    pub fn macro_calls(&self) -> &[MacroCallId] { .. }
}
```
- `ExpressionStoreBuilder` and `Scratch`: `macro_calls: Vec<MacroCallId>` beside `expansions`
  (cleared in `Scratch::clear`, counted in `capacity`).
- `expr_store/lower.rs`, both places that push `expansions` (~L2810 in `enter_expand`'s
  callback, ~L2836 after a call expands): push the id to `macro_calls` first, outside
  `if self.store.record`; `expansions` stays under it.
- `finish`: `ExpressionOnlyStore` gets `macro_calls: Box<[MacroCallId]>` (from the scratch
  `Vec`, empty boxed when none), and a store whose only content is macro calls still has an
  `expr_only` (as `!expansions.is_empty()` makes one for the map). `macro_calls()` is `&[]`
  without `expr_only`.
- Both lowerings record it, so `Body::of`'s store stays equal to `relative`'s and
  `with_source_map`'s (the existing `assert_eq!`s check it). A test in `tests/body.rs`: for every
  fixture, the set of `macro_calls()` equals the set of ids of the map's `macro_calls()`.

Why: MIR lowering refuses a body whose macro call failed to expand or expands to what rustc
parses otherwise (`hir-ty-mir/src/mir/lower.rs` `failed_expansion`, on every `mir_body`); it
read the calls from the source map, so every lowered body built its map a second time. Now
`failed_expansion` reads `macro_calls()` and each call's site (`MacroCallLoc::to_node`);
`location.rs` (`location_macros`, `located_concats`) and `type_name.rs`
(`closures_around_query`) read the map only when `macro_calls()` names a `line!`, `column!`,
`file!` or `concat!`, or any call at all. One `u32` per macro call. Written against this, in
hir-ty-mir already (not compiled).

Status: open

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-run` (report `/tmp/blaze-lanes/v2-dedup-cursor-run.md`; the cursor is
`crates/blaze-format/src/cursor.rs`, docs/v2/plan/lanes/dedup/cursor.md). Not built; the diff
applies to the tree as of 2026-10-29 (`git apply --check`).

- `blaze-codegen/src/blob.rs`: `In`'s primitives (`take`, `u8`, `u32`, `i64`, `count`; the
  `id`/`bytes` copies) and the writer `Out` are deleted. `In` becomes
  `struct In<'a>(Reader<'a>)` keeping only its domain reads (`id`, `bytes` as an owned `Vec`,
  `relocs`, `data`, `bool`, `index`, `unwind`, `lines`, `coverage`, `magic`, `end`). The writer
  becomes `put_*` free functions on `Vec<u8>` over `Put`. One
  `impl From<cursor::Error> for BlobError` keeps the words: `Bytes` is "ends early", `Count` is
  "a count longer than what is left" (which `a_count_past_the_end_allocates_nothing` checks),
  and `End` is "bytes after the end". `In::bytes` keeps `count_u32(1)` and then `take`, so a
  length past the end is still that count error. One difference: a count error's `at` is now
  the byte where the count starts, not the byte after it (no test reads `at`). Also fixes the
  file's four hot-lint findings (three `// alloc:` on owned copies; `encode_masked` sized with
  `with_capacity`). Same bytes.
- `blaze-codegen/src/summary.rs`: `Imports::encode`'s `len` closure and `to_le_bytes` appends
  become `Put` (same bytes, so the same `blob_key`s). Also justifies the thread's empty reused
  buffer for the hot lint. `blaze-codegen` already depends on `blaze-format`.
- `blaze-engine/src/manifest.rs`: `Encoder` and `Decoder` are deleted for `Put` and `Reader`
  (`?` through `.ok()`, since `decode` returns `Option`). Two helpers stay, for the tag-then-bytes
  option. Counts are now bounded by what is left (`count_u64(1)`); before, they were unbounded
  but harmless. Same bytes. `blaze-engine/Cargo.toml` gains `blaze-format.workspace = true`
  (`blaze-format` depends on nothing of the engine's).
- Kept on purpose: `blaze-engine/src/lib.rs`'s `metadata_hash` (one fixed-offset `u64` of an
  rmeta header), `link/split.rs` and `frozen.rs` (random-access words of `.eh_frame` and a
  2-part split, not cursors), and `relocs.rs`'s 16-byte row (a fixed layout).

```diff
--- a/crates/blaze-codegen/src/blob.rs
+++ b/crates/blaze-codegen/src/blob.rs
@@ -3,7 +3,10 @@
 //! what is left and every tag against what it can be, so bytes that are not a template are an
 //! error, never a panic or an allocation of what a length claims.
 
-use blaze_format::SpanRef;
+use blaze_format::{
+    SpanRef,
+    cursor::{self, Put, Reader, Wanted},
+};
 use blaze_ids::ContentId;
 use blaze_items::CodeHash;
 
@@ -30,145 +33,130 @@
     }
 }
 
-struct Out(Vec<u8>);
-
-impl Out {
-    fn u8(&mut self, it: u8) {
-        self.0.push(it);
+impl From<cursor::Error> for BlobError {
+    fn from(error: cursor::Error) -> BlobError {
+        let what = match error.wanted {
+            Wanted::Bytes { .. } => "ends early",
+            Wanted::Count { .. } => "a count longer than what is left",
+            Wanted::End { .. } => "bytes after the end",
+            _ => error.as_str(),
+        };
+        BlobError { at: error.at, what }
     }
+}
 
-    fn u32(&mut self, it: u32) {
-        self.0.extend_from_slice(&it.to_le_bytes());
-    }
+fn put_len(out: &mut Vec<u8>, len: usize) {
+    out.put_u32(u32::try_from(len).expect("a template's parts are under 4 GiB"));
+}
 
-    fn i64(&mut self, it: i64) {
-        self.0.extend_from_slice(&it.to_le_bytes());
+/// Rows as their 16 bytes, then each reference, then the wide addends.
+fn put_relocs(out: &mut Vec<u8>, relocs: &Relocs<Ref>) {
+    put_len(out, relocs.len());
+    for row in relocs.rows() {
+        out.put_bytes(&row.to_le_bytes());
     }
-
-    fn id(&mut self, it: &ContentId) {
-        self.0.extend_from_slice(&it.0);
-    }
-
-    fn len(&mut self, len: usize) {
-        self.u32(u32::try_from(len).expect("a template's parts are under 4 GiB"));
-    }
-
-    fn bytes(&mut self, it: &[u8]) {
-        self.len(it.len());
-        self.0.extend_from_slice(it);
-    }
-
-    /// Rows as their 16 bytes, then each reference, then the wide addends.
-    fn relocs(&mut self, relocs: &Relocs<Ref>) {
-        self.len(relocs.len());
-        for row in relocs.rows() {
-            self.0.extend_from_slice(&row.to_le_bytes());
-        }
-        self.len(relocs.targets().len());
-        for target in relocs.targets() {
-            match *target {
-                Ref::Place(at) => {
-                    self.u8(0);
-                    self.u32(at);
-                }
-                Ref::Data(at) => {
-                    self.u8(1);
-                    self.u32(at);
-                }
-                Ref::Position(SpanRef { anchor, index }) => {
-                    self.u8(2);
-                    self.id(&anchor);
-                    self.u32(index);
-                }
-                Ref::Extern(at) => {
-                    self.u8(3);
-                    self.u32(at);
-                }
-                Ref::Coverage(array) => {
-                    self.u8(4);
-                    self.u32(array);
-                }
+    put_len(out, relocs.targets().len());
+    for target in relocs.targets() {
+        match *target {
+            Ref::Place(at) => {
+                out.put_u8(0);
+                out.put_u32(at);
             }
+            Ref::Data(at) => {
+                out.put_u8(1);
+                out.put_u32(at);
+            }
+            Ref::Position(SpanRef { anchor, index }) => {
+                out.put_u8(2);
+                out.put_bytes(&anchor.0);
+                out.put_u32(index);
+            }
+            Ref::Extern(at) => {
+                out.put_u8(3);
+                out.put_u32(at);
+            }
+            Ref::Coverage(array) => {
+                out.put_u8(4);
+                out.put_u32(array);
+            }
         }
-        self.len(relocs.wide().len());
-        for &(row, addend) in relocs.wide() {
-            self.u32(row);
-            self.i64(addend);
-        }
     }
-
-    fn data(&mut self, data: &[TemplateData]) {
-        self.len(data.len());
-        for it in data {
-            self.u32(it.align);
-            self.bytes(&it.bytes);
-            self.relocs(&it.relocs);
-        }
+    put_len(out, relocs.wide().len());
+    for &(row, addend) in relocs.wide() {
+        out.put_u32(row);
+        out.put_i64(addend);
     }
+}
 
-    fn index(&mut self, it: Option<u32>) {
-        match it {
-            Some(at) => {
-                self.u8(1);
-                self.u32(at);
-            }
-            None => self.u8(0),
-        }
+fn put_data(out: &mut Vec<u8>, data: &[TemplateData]) {
+    put_len(out, data.len());
+    for it in data {
+        out.put_u32(it.align);
+        out.put_bytes_u32(&it.bytes);
+        put_relocs(out, &it.relocs);
     }
+}
 
-    fn unwind(&mut self, unwind: Option<&TemplateUnwind>) {
-        match unwind {
-            None => self.u8(0),
-            Some(TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
-                self.u8(1);
-                self.u32(*code_align);
-                self.0.extend_from_slice(&data_align.to_le_bytes());
-                self.0.extend_from_slice(&return_address.to_le_bytes());
-                self.bytes(cie);
-                self.bytes(fde);
-                self.index(*personality);
-                self.index(*lsda);
-            }
-            Some(TemplateUnwind::Windows { info, handler, lsda }) => {
-                self.u8(2);
-                self.bytes(info);
-                self.index(*handler);
-                self.index(*lsda);
-            }
+fn put_index(out: &mut Vec<u8>, it: Option<u32>) {
+    match it {
+        Some(at) => {
+            out.put_u8(1);
+            out.put_u32(at);
         }
+        None => out.put_u8(0),
     }
+}
 
-    fn lines(&mut self, lines: &[Line]) {
-        self.len(lines.len());
-        for line in lines {
-            self.u32(line.at);
-            self.id(&line.span.anchor);
-            self.u32(line.span.index);
+fn put_unwind(out: &mut Vec<u8>, unwind: Option<&TemplateUnwind>) {
+    match unwind {
+        None => out.put_u8(0),
+        Some(TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
+            out.put_u8(1);
+            out.put_u32(*code_align);
+            out.put_i32(*data_align);
+            out.put_u16(*return_address);
+            out.put_bytes_u32(cie);
+            out.put_bytes_u32(fde);
+            put_index(out, *personality);
+            put_index(out, *lsda);
         }
+        Some(TemplateUnwind::Windows { info, handler, lsda }) => {
+            out.put_u8(2);
+            out.put_bytes_u32(info);
+            put_index(out, *handler);
+            put_index(out, *lsda);
+        }
     }
+}
 
-    fn coverage(&mut self, covered: Option<&Covered>) {
-        let Some(Covered { counters, flags, guards, pcs }) = covered else {
-            self.u8(0);
-            return;
-        };
-        self.u8(1);
-        self.u32(*counters);
-        self.u32(*flags);
-        self.u32(*guards);
-        self.len(pcs.len());
-        for (at, pc_flags) in pcs {
-            self.u32(*at);
-            self.0.extend_from_slice(&pc_flags.to_le_bytes());
-        }
+fn put_lines(out: &mut Vec<u8>, lines: &[Line]) {
+    put_len(out, lines.len());
+    for line in lines {
+        out.put_u32(line.at);
+        out.put_bytes(&line.span.anchor.0);
+        out.put_u32(line.span.index);
     }
 }
 
-struct In<'a> {
-    bytes: &'a [u8],
-    at: usize,
+fn put_coverage(out: &mut Vec<u8>, covered: Option<&Covered>) {
+    let Some(Covered { counters, flags, guards, pcs }) = covered else {
+        out.put_u8(0);
+        return;
+    };
+    out.put_u8(1);
+    out.put_u32(*counters);
+    out.put_u32(*flags);
+    out.put_u32(*guards);
+    put_len(out, pcs.len());
+    for (at, pc_flags) in pcs {
+        out.put_u32(*at);
+        out.put_u64(*pc_flags);
+    }
 }
 
+/// A template's fields read off a [`Reader`], each tag checked against what it can be.
+struct In<'a>(Reader<'a>);
+
 /// The bytes of one relocation row, and the least one reference and one data object take:
 /// what bounds a count.
 const RELOC_BYTES: usize = 16;
@@ -177,85 +165,57 @@
 
 impl<'a> In<'a> {
     fn fail<T>(&self, what: &'static str) -> Result<T, BlobError> {
-        Err(BlobError { at: self.at, what })
+        Err(BlobError { at: self.0.at(), what })
     }
 
-    fn take(&mut self, len: usize) -> Result<&'a [u8], BlobError> {
-        match self.bytes.get(self.at..).and_then(|rest| rest.get(..len)) {
-            Some(it) => {
-                self.at += len;
-                Ok(it)
-            }
-            None => self.fail("ends early"),
-        }
-    }
-
-    fn u8(&mut self) -> Result<u8, BlobError> {
-        Ok(self.take(1)?[0])
-    }
-
-    fn u32(&mut self) -> Result<u32, BlobError> {
-        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
-    }
-
-    fn i64(&mut self) -> Result<i64, BlobError> {
-        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
-    }
-
     fn id(&mut self) -> Result<ContentId, BlobError> {
-        Ok(ContentId(self.take(32)?.try_into().expect("32 bytes")))
-    }
-
-    /// A count of things at least `each` bytes long, checked against what is left.
-    fn count(&mut self, each: usize) -> Result<usize, BlobError> {
-        let count = self.u32()? as usize;
-        match count.checked_mul(each) {
-            Some(need) if need <= self.bytes.len() - self.at => Ok(count),
-            _ => self.fail("a count longer than what is left"),
-        }
+        Ok(ContentId(*self.0.array()?))
     }
 
+    /// A `u32` length, then that many bytes, copied: a length past what is left is a count
+    /// longer than what is left.
     fn bytes(&mut self) -> Result<Vec<u8>, BlobError> {
-        let len = self.count(1)?;
-        Ok(self.take(len)?.to_vec())
+        let len = self.0.count_u32(1)?;
+        Ok(self.0.take(len)?.to_vec()) // alloc: the template owns its bytes
     }
 
     fn relocs(&mut self) -> Result<Relocs<Ref>, BlobError> {
-        let count = self.count(RELOC_BYTES)?;
+        let count = self.0.count_u32(RELOC_BYTES)?;
         let mut rows = Vec::with_capacity(count);
         for _ in 0..count {
-            let row = self.take(RELOC_BYTES)?.try_into().expect("a row's bytes");
-            let Some(row) = Reloc::from_le_bytes(row) else { return self.fail("no such relocation kind, or padding") };
+            let Some(row) = Reloc::from_le_bytes(*self.0.array::<RELOC_BYTES>()?) else {
+                return self.fail("no such relocation kind, or padding");
+            };
             rows.push(row);
         }
-        let count = self.count(REF_BYTES)?;
+        let count = self.0.count_u32(REF_BYTES)?;
         let mut targets = Vec::with_capacity(count);
         for _ in 0..count {
-            targets.push(match self.u8()? {
-                0 => Ref::Place(self.u32()?),
-                1 => Ref::Data(self.u32()?),
-                2 => Ref::Position(SpanRef { anchor: self.id()?, index: self.u32()? }),
-                3 => Ref::Extern(self.u32()?),
-                4 => match self.u32()? {
+            targets.push(match self.0.u8()? {
+                0 => Ref::Place(self.0.u32()?),
+                1 => Ref::Data(self.0.u32()?),
+                2 => Ref::Position(SpanRef { anchor: self.id()?, index: self.0.u32()? }),
+                3 => Ref::Extern(self.0.u32()?),
+                4 => match self.0.u32()? {
                     array if array < Covered::ARRAYS => Ref::Coverage(array),
                     _ => return self.fail("no such coverage array"),
                 },
                 _ => return self.fail("no such reference"),
             });
         }
-        let count = self.count(4 + 8)?;
+        let count = self.0.count_u32(4 + 8)?;
         let mut wide = Vec::with_capacity(count);
         for _ in 0..count {
-            wide.push((self.u32()?, self.i64()?));
+            wide.push((self.0.u32()?, self.0.i64()?));
         }
-        Relocs::from_parts(rows, targets, wide).map_err(|what| BlobError { at: self.at, what })
+        Relocs::from_parts(rows, targets, wide).map_err(|what| BlobError { at: self.0.at(), what })
     }
 
     fn data(&mut self) -> Result<Vec<TemplateData>, BlobError> {
-        let count = self.count(DATA_BYTES)?;
+        let count = self.0.count_u32(DATA_BYTES)?;
         let mut data = Vec::with_capacity(count);
         for _ in 0..count {
-            let align = self.u32()?;
+            let align = self.0.u32()?;
             let bytes = self.bytes()?;
             let relocs = self.relocs()?;
             data.push(TemplateData { align, bytes, relocs });
@@ -264,7 +224,7 @@
     }
 
     fn bool(&mut self) -> Result<bool, BlobError> {
-        match self.u8()? {
+        match self.0.u8()? {
             0 => Ok(false),
             1 => Ok(true),
             _ => self.fail("not a bool"),
@@ -273,18 +233,18 @@
 
     fn index(&mut self) -> Result<Option<u32>, BlobError> {
         Ok(match self.bool()? {
-            true => Some(self.u32()?),
+            true => Some(self.0.u32()?),
             false => None,
         })
     }
 
     fn unwind(&mut self) -> Result<Option<TemplateUnwind>, BlobError> {
-        Ok(match self.u8()? {
+        Ok(match self.0.u8()? {
             0 => None,
             1 => Some(TemplateUnwind::Dwarf {
-                code_align: self.u32()?,
-                data_align: i32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")),
-                return_address: u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes")),
+                code_align: self.0.u32()?,
+                data_align: self.0.i32()?,
+                return_address: self.0.u16()?,
                 cie: self.bytes()?,
                 fde: self.bytes()?,
                 personality: self.index()?,
@@ -296,11 +256,11 @@
     }
 
     fn lines(&mut self) -> Result<Vec<Line>, BlobError> {
-        let count = self.count(4 + 32 + 4)?;
+        let count = self.0.count_u32(4 + 32 + 4)?;
         let mut lines = Vec::with_capacity(count);
         for _ in 0..count {
-            let at = self.u32()?;
-            lines.push(Line { at, span: SpanRef { anchor: self.id()?, index: self.u32()? } });
+            let at = self.0.u32()?;
+            lines.push(Line { at, span: SpanRef { anchor: self.id()?, index: self.0.u32()? } });
         }
         Ok(lines)
     }
@@ -309,73 +269,70 @@
         if !self.bool()? {
             return Ok(None);
         }
-        let (counters, flags, guards) = (self.u32()?, self.u32()?, self.u32()?);
-        let count = self.count(4 + 8)?;
+        let (counters, flags, guards) = (self.0.u32()?, self.0.u32()?, self.0.u32()?);
+        let count = self.0.count_u32(4 + 8)?;
         let mut pcs = Vec::with_capacity(count);
         for _ in 0..count {
-            let at = self.u32()?;
-            pcs.push((at, u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes"))));
+            let at = self.0.u32()?;
+            pcs.push((at, self.0.u64()?));
         }
         Ok(Some(Covered { counters, flags, guards, pcs }))
     }
 
     fn magic(&mut self, magic: [u8; 4]) -> Result<(), BlobError> {
-        if self.take(4)? != magic {
+        if *self.0.array::<4>()? != magic {
             return self.fail("another kind of blob, or another version of this one");
         }
         Ok(())
     }
 
     fn end(&self) -> Result<(), BlobError> {
-        if self.at != self.bytes.len() {
-            return self.fail("bytes after the end");
-        }
-        Ok(())
+        Ok(self.0.finish()?)
     }
 }
 
 pub fn encode(template: &Template) -> Vec<u8> {
-    let mut out = Out(Vec::with_capacity(64 + template.code.len()));
-    out.0.extend_from_slice(&TEMPLATE_MAGIC);
-    out.u8(arch_tag(template.arch));
-    out.u32(template.align);
-    out.bytes(&template.code);
-    out.relocs(&template.relocs);
-    out.data(&template.data);
-    out.len(template.inlined.len());
+    let mut out = Vec::with_capacity(64 + template.code.len());
+    out.put_bytes(&TEMPLATE_MAGIC);
+    out.put_u8(arch_tag(template.arch));
+    out.put_u32(template.align);
+    out.put_bytes_u32(&template.code);
+    put_relocs(&mut out, &template.relocs);
+    put_data(&mut out, &template.data);
+    put_len(&mut out, template.inlined.len());
     for code in &template.inlined {
-        out.id(&code.0);
+        out.put_bytes(&code.0.0);
     }
     match &template.side {
         Some(side) => {
-            out.u8(1);
-            out.id(side);
+            out.put_u8(1);
+            out.put_bytes(&side.0);
         }
-        None => out.u8(0),
+        None => out.put_u8(0),
     }
-    out.unwind(template.unwind.as_ref());
-    out.lines(&template.lines);
-    out.u32(template.refs);
-    out.len(template.externs.len());
+    put_unwind(&mut out, template.unwind.as_ref());
+    put_lines(&mut out, &template.lines);
+    out.put_u32(template.refs);
+    put_len(&mut out, template.externs.len());
     for name in &template.externs {
-        out.bytes(name.as_bytes());
+        out.put_bytes_u32(name.as_bytes());
     }
-    out.coverage(template.coverage.as_ref());
-    out.0
+    put_coverage(&mut out, template.coverage.as_ref());
+    out
 }
 
 pub fn decode(bytes: &[u8]) -> Result<Template, BlobError> {
-    let mut input = In { bytes, at: 0 };
+    let mut input = In(Reader::new(bytes));
     input.magic(TEMPLATE_MAGIC)?;
-    let Some(arch) = arch_of(input.u8()?) else { return input.fail("no such architecture") };
-    let align = input.u32()?;
+    let Some(arch) = arch_of(input.0.u8()?) else { return input.fail("no such architecture") };
+    let align = input.0.u32()?;
     if !align.is_power_of_two() {
         return input.fail("an alignment that is not a power of two");
     }
     let code = input.bytes()?;
     let relocs = input.relocs()?;
     let data = input.data()?;
-    let count = input.count(32)?;
+    let count = input.0.count_u32(32)?;
     let inlined = (0..count).map(|_| input.id().map(CodeHash)).collect::<Result<_, _>>()?;
     let side = match input.bool()? {
         true => Some(input.id()?),
@@ -383,10 +340,11 @@
     };
     let unwind = input.unwind()?;
     let lines = input.lines()?;
-    let refs = input.u32()?;
-    let count = input.count(4)?;
+    let refs = input.0.u32()?;
+    let count = input.0.count_u32(4)?;
     let mut externs = Vec::with_capacity(count);
     for _ in 0..count {
+        // alloc: the template owns its names, moved out of the bytes read for them
         match String::from_utf8(input.bytes()?) {
             Ok(name) => externs.push(name),
             Err(_) => return input.fail("a symbol name that is not UTF-8"),
@@ -398,57 +356,58 @@
 }
 
 pub fn encode_static(template: &StaticTemplate) -> Vec<u8> {
-    let mut out = Out(Vec::with_capacity(64));
-    out.0.extend_from_slice(&STATIC_MAGIC);
-    out.u32(template.align);
+    let mut out = Vec::with_capacity(64);
+    out.put_bytes(&STATIC_MAGIC);
+    out.put_u32(template.align);
     match &template.init {
         TemplateInit::Bytes { bytes, relocs } => {
-            out.u8(0);
-            out.bytes(bytes);
-            out.relocs(relocs);
+            out.put_u8(0);
+            out.put_bytes_u32(bytes);
+            put_relocs(&mut out, relocs);
         }
         TemplateInit::Zeroed { size } => {
-            out.u8(1);
-            out.0.extend_from_slice(&size.to_le_bytes());
+            out.put_u8(1);
+            out.put_u64(*size);
         }
     }
-    out.data(&template.data);
-    out.u8(template.mutable as u8);
-    out.u8(template.thread_local as u8);
+    put_data(&mut out, &template.data);
+    out.put_u8(template.mutable as u8);
+    out.put_u8(template.thread_local as u8);
     match &template.section {
         Some(section) => {
-            out.u8(1);
-            out.bytes(section.as_bytes());
+            out.put_u8(1);
+            out.put_bytes_u32(section.as_bytes());
         }
-        None => out.u8(0),
+        None => out.put_u8(0),
     }
-    out.u32(template.refs);
-    out.0
+    out.put_u32(template.refs);
+    out
 }
 
 pub fn decode_static(bytes: &[u8]) -> Result<StaticTemplate, BlobError> {
-    let mut input = In { bytes, at: 0 };
+    let mut input = In(Reader::new(bytes));
     input.magic(STATIC_MAGIC)?;
-    let align = input.u32()?;
+    let align = input.0.u32()?;
     if !align.is_power_of_two() {
         return input.fail("an alignment that is not a power of two");
     }
-    let init = match input.u8()? {
+    let init = match input.0.u8()? {
         0 => TemplateInit::Bytes { bytes: input.bytes()?, relocs: input.relocs()? },
-        1 => TemplateInit::Zeroed { size: u64::from_le_bytes(input.take(8)?.try_into().expect("eight bytes")) },
+        1 => TemplateInit::Zeroed { size: input.0.u64()? },
         _ => return input.fail("no such initializer"),
     };
     let data = input.data()?;
     let mutable = input.bool()?;
     let thread_local = input.bool()?;
     let section = match input.bool()? {
+        // alloc: the template owns its section name, moved out of the bytes read for it
         true => match String::from_utf8(input.bytes()?) {
             Ok(it) => Some(it),
             Err(_) => return input.fail("a section name that is not UTF-8"),
         },
         false => None,
     };
-    let refs = input.u32()?;
+    let refs = input.0.u32()?;
     input.end()?;
     Ok(StaticTemplate { align, init, data, mutable, thread_local, section, refs })
 }
@@ -458,59 +417,63 @@
 /// What a function is with its constants masked (#1292), or that it can't be folded: kept per
 /// instance, so a warm build groups without building any IR.
 pub fn encode_masked(masked: Option<&Masked>) -> Vec<u8> {
-    let mut out = Out(MASKED_MAGIC.to_vec());
-    let Some(masked) = masked else {
-        out.u8(0);
-        return out.0;
+    // The magic, the tag, the shape and its length, the size, the count, and at most 18 bytes
+    // a constant.
+    let most = masked.map_or(0, |it| it.shape.len() + 18 * it.constants.len());
+    let mut out = Vec::with_capacity(MASKED_MAGIC.len() + 1 + 4 + 4 + 4 + most);
+    out.put_bytes(&MASKED_MAGIC);
+    let Some(masked) = masked else {
+        out.put_u8(0);
+        return out;
     };
-    out.u8(1);
-    out.bytes(&masked.shape);
-    out.u32(masked.size);
-    out.len(masked.constants.len());
+    out.put_u8(1);
+    out.put_bytes_u32(&masked.shape);
+    out.put_u32(masked.size);
+    put_len(&mut out, masked.constants.len());
     for constant in &masked.constants {
         match *constant {
             Constant::Int { bits, value } => {
-                out.u8(0);
-                out.u8(bits);
-                out.0.extend_from_slice(&value.to_le_bytes());
+                out.put_u8(0);
+                out.put_u8(bits);
+                out.put_u128(value);
             }
             Constant::F32(bits) => {
-                out.u8(1);
-                out.u32(bits);
+                out.put_u8(1);
+                out.put_u32(bits);
             }
             Constant::F64(bits) => {
-                out.u8(2);
-                out.0.extend_from_slice(&bits.to_le_bytes());
+                out.put_u8(2);
+                out.put_u64(bits);
             }
             Constant::Place(at) => {
-                out.u8(3);
-                out.u32(at);
+                out.put_u8(3);
+                out.put_u32(at);
             }
         }
     }
-    out.0
+    out
 }
 
 pub fn decode_masked(bytes: &[u8]) -> Result<Option<Masked>, BlobError> {
-    let mut input = In { bytes, at: 0 };
+    let mut input = In(Reader::new(bytes));
     input.magic(MASKED_MAGIC)?;
     if !input.bool()? {
         input.end()?;
         return Ok(None);
     }
     let shape = input.bytes()?;
-    let size = input.u32()?;
-    let count = input.count(5)?;
+    let size = input.0.u32()?;
+    let count = input.0.count_u32(5)?;
     let mut constants = Vec::with_capacity(count);
     for _ in 0..count {
-        constants.push(match input.u8()? {
+        constants.push(match input.0.u8()? {
             0 => {
-                let bits = input.u8()?;
-                Constant::Int { bits, value: u128::from_le_bytes(input.take(16)?.try_into().expect("16 bytes")) }
+                let bits = input.0.u8()?;
+                Constant::Int { bits, value: input.0.u128()? }
             }
-            1 => Constant::F32(input.u32()?),
-            2 => Constant::F64(u64::from_le_bytes(input.take(8)?.try_into().expect("8 bytes"))),
-            3 => Constant::Place(input.u32()?),
+            1 => Constant::F32(input.0.u32()?),
+            2 => Constant::F64(input.0.u64()?),
+            3 => Constant::Place(input.0.u32()?),
             _ => return input.fail("no such constant"),
         });
     }
--- a/crates/blaze-codegen/src/summary.rs
+++ b/crates/blaze-codegen/src/summary.rs
@@ -12,6 +12,7 @@
 
 use std::cell::RefCell;
 
+use blaze_format::cursor::Put;
 use blaze_ids::{ContentId, Hasher};
 use serde::{Deserialize, Serialize};
 
@@ -43,10 +44,10 @@
     /// Its bytes in [`Imports::blob_key`]: a tag, the width and the value, 10 in all.
     fn encode(self, out: &mut Vec<u8>) {
         match self {
-            Arg::Unknown => out.extend_from_slice(&[0; 10]),
+            Arg::Unknown => out.put_bytes(&[0; 10]),
             Arg::Int { bits, value } => {
-                out.extend_from_slice(&[1, bits]);
-                out.extend_from_slice(&value.to_le_bytes());
+                out.put_bytes(&[1, bits]);
+                out.put_u64(value);
             }
         }
     }
@@ -151,7 +152,7 @@
     /// every field laid out in a buffer the thread reuses.
     pub fn blob_key(&self, key: InstanceKey) -> ContentId {
         thread_local! {
-            static BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
+            static BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) }; // alloc: empty, the buffer the thread reuses
         }
         BYTES.with_borrow_mut(|bytes| {
             bytes.clear();
@@ -165,29 +166,28 @@
     /// Every field, each list as its length then its elements at fixed widths, little-endian:
     /// two different `Imports` never give the same bytes.
     fn encode(&self, out: &mut Vec<u8>) {
-        let len = |n: usize| (n as u32).to_le_bytes();
         let dead: usize = self.dead.iter().map(|it| 36 + 4 * it.1.len()).sum();
         out.reserve(16 + 32 * self.imported.len() + 14 * self.constants.len() + 33 * self.callees.len() + dead);
-        out.extend_from_slice(&len(self.imported.len()));
+        out.put_u32(self.imported.len() as u32);
         for key in &self.imported {
-            out.extend_from_slice(&key.0.0);
+            out.put_bytes(&key.0.0);
         }
-        out.extend_from_slice(&len(self.constants.len()));
+        out.put_u32(self.constants.len() as u32);
         for &(param, arg) in &self.constants {
-            out.extend_from_slice(&param.to_le_bytes());
+            out.put_u32(param);
             arg.encode(out);
         }
-        out.extend_from_slice(&len(self.callees.len()));
+        out.put_u32(self.callees.len() as u32);
         for (key, attrs) in &self.callees {
-            out.extend_from_slice(&key.0.0);
-            out.push(attrs.bits());
+            out.put_bytes(&key.0.0);
+            out.put_u8(attrs.bits());
         }
-        out.extend_from_slice(&len(self.dead.len()));
+        out.put_u32(self.dead.len() as u32);
         for (key, params) in &self.dead {
-            out.extend_from_slice(&key.0.0);
-            out.extend_from_slice(&len(params.len()));
+            out.put_bytes(&key.0.0);
+            out.put_u32(params.len() as u32);
             for param in params {
-                out.extend_from_slice(&param.to_le_bytes());
+                out.put_u32(*param);
             }
         }
     }
--- a/crates/blaze-engine/Cargo.toml
+++ b/crates/blaze-engine/Cargo.toml
@@ -76,6 +76,7 @@
 blaze-macro-wasm.workspace = true
 blaze-store.workspace = true
 blaze-doc.workspace = true
+blaze-format.workspace = true
 blaze-lints.workspace = true
 blaze-graph.workspace = true
 blaze-ids.workspace = true
--- a/crates/blaze-engine/src/manifest.rs
+++ b/crates/blaze-engine/src/manifest.rs
@@ -25,6 +25,7 @@
     time::{Duration, Instant},
 };
 
+use blaze_format::cursor::{Put, Reader};
 use ide_db::{RootDatabase, base_db::Crate};
 use rustc_hash::FxHashMap;
 use sha2::{Digest, Sha256};
@@ -291,111 +292,74 @@
 }
 
 const MAGIC: &[u8] = b"blaze manifest 1\n";
-
-struct Encoder(Vec<u8>);
-
-impl Encoder {
-    fn bytes(&mut self, bytes: &[u8]) {
-        self.0.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
-        self.0.extend_from_slice(bytes);
-    }
 
-    fn count(&mut self, count: usize) {
-        self.0.extend_from_slice(&(count as u64).to_le_bytes());
-    }
-
-    fn option(&mut self, bytes: Option<&[u8]>) {
-        match bytes {
-            Some(bytes) => {
-                self.0.push(1);
-                self.bytes(bytes);
-            }
-            None => self.0.push(0),
+/// A tag, then the bytes when there are some.
+fn put_option(out: &mut Vec<u8>, bytes: Option<&[u8]>) {
+    match bytes {
+        Some(bytes) => {
+            out.put_u8(1);
+            out.put_bytes_u64(bytes);
         }
+        None => out.put_u8(0),
     }
 }
 
-struct Decoder<'a>(&'a [u8]);
-
-impl<'a> Decoder<'a> {
-    fn count(&mut self) -> Option<usize> {
-        let (it, rest) = self.0.split_first_chunk::<8>()?;
-        self.0 = rest;
-        usize::try_from(u64::from_le_bytes(*it)).ok()
-    }
-
-    fn bytes(&mut self) -> Option<&'a [u8]> {
-        let len = self.count()?;
-        let (it, rest) = self.0.split_at_checked(len)?;
-        self.0 = rest;
-        Some(it)
-    }
-
-    fn string(&mut self) -> Option<String> {
-        String::from_utf8(self.bytes()?.to_vec()).ok()
-    }
-
-    fn hash(&mut self) -> Option<Hash> {
-        self.bytes()?.try_into().ok()
-    }
-
-    fn option(&mut self) -> Option<Option<&'a [u8]>> {
-        let (&tag, rest) = self.0.split_first()?;
-        self.0 = rest;
-        match tag {
-            0 => Some(None),
-            1 => Some(Some(self.bytes()?)),
-            _ => None,
-        }
+fn option<'a>(input: &mut Reader<'a>) -> Option<Option<&'a [u8]>> {
+    match input.u8().ok()? {
+        0 => Some(None),
+        1 => Some(Some(input.bytes_u64().ok()?)),
+        _ => None,
     }
 }
 
 impl Manifest {
     fn encode(&self) -> Vec<u8> {
-        let mut out = Encoder(MAGIC.to_vec());
-        out.count(self.reads.vars.len());
+        let mut out = MAGIC.to_vec();
+        out.put_u64(self.reads.vars.len() as u64);
         for (name, value) in &self.reads.vars {
-            out.bytes(name.as_bytes());
-            out.option(value.as_deref().map(str::as_bytes));
+            out.put_bytes_u64(name.as_bytes());
+            put_option(&mut out, value.as_deref().map(str::as_bytes));
         }
-        out.count(self.reads.files.len());
+        out.put_u64(self.reads.files.len() as u64);
         for (path, hash) in &self.reads.files {
-            out.bytes(path.as_bytes());
-            out.option(hash.as_ref().map(|it| &it[..]));
+            out.put_bytes_u64(path.as_bytes());
+            put_option(&mut out, hash.as_ref().map(|it| &it[..]));
         }
-        out.count(self.files.len());
+        out.put_u64(self.files.len() as u64);
         for file in &self.files {
-            out.bytes(file.path.as_bytes());
-            out.bytes(&file.content);
+            out.put_bytes_u64(file.path.as_bytes());
+            out.put_bytes_u64(&file.content);
         }
-        out.0
+        out
     }
 
+    /// Each list's count is checked against the bytes left at one byte an entry.
     fn decode(bytes: &[u8]) -> Option<Manifest> {
-        let mut input = Decoder(bytes.strip_prefix(MAGIC)?);
+        let mut input = Reader::new(bytes.strip_prefix(MAGIC)?);
         let mut vars = Vec::new();
-        for _ in 0..input.count()? {
-            let name = input.string()?;
-            let value = match input.option()? {
-                Some(it) => Some(String::from_utf8(it.to_vec()).ok()?),
+        for _ in 0..input.count_u64(1).ok()? {
+            let name = input.str_u64().ok()?.to_owned();
+            let value = match option(&mut input)? {
+                Some(it) => Some(std::str::from_utf8(it).ok()?.to_owned()),
                 None => None,
             };
             vars.push((name, value));
         }
         let mut files = Vec::new();
-        for _ in 0..input.count()? {
-            let path = input.string()?;
-            let hash = match input.option()? {
+        for _ in 0..input.count_u64(1).ok()? {
+            let path = input.str_u64().ok()?.to_owned();
+            let hash = match option(&mut input)? {
                 Some(it) => Some(it.try_into().ok()?),
                 None => None,
             };
             files.push((path, hash));
         }
         let mut written = Vec::new();
-        for _ in 0..input.count()? {
-            written.push(Written { path: input.string()?, content: input.hash()? });
+        for _ in 0..input.count_u64(1).ok()? {
+            let path = input.str_u64().ok()?.to_owned();
+            written.push(Written { path, content: input.bytes_u64().ok()?.try_into().ok()? });
         }
-        input.0.is_empty().then_some(Manifest { reads: Reads { vars, files }, files: written })
+        input.is_done().then_some(Manifest { reads: Reads { vars, files }, files: written })
     }
 }
 
```

> applied by codegen-v2 (2026-10-29): the `blaze-codegen` hunks (`blob.rs`, `summary.rs`) applied as written (`git apply --check` clean, hot lint clean on both); the `blaze-engine` hunks (`Cargo.toml`, `manifest.rs`) are codegen-engine's, asked in `codegen-v2.requests.md` "To codegen-engine".

## dedyn: crates/hir-ty-codegen (outside `v2/`): the `dyn` sites (from dedyn-codegen, 2026-10-29)

From `dedyn-codegen` (child of lane `dedyn`, report `/tmp/blaze-lanes/v2-dedyn-codegen.md`). Every
`dyn` in these files outside tests, by what it becomes. Anchored on code text: the line numbers are
as of 2026-10-29 21:30 and move. Nothing here changes behavior; the tests named are updated in the
same item. hot_lint accepts a `// dyn:` on the line or the line above.

### dedyn 1: `unwind.rs`, `unwind/seh.rs`: `dyn Module` becomes `ObjectModule`

`cranelift_module::Module` has one implementation, `cranelift_object::ObjectModule`, and every
caller passes `ObjectCodegen::module` (`mod.rs` `UnwindContext::new(&mut module)`,
`self.unwind.name_personality(&mut self.module)`, `.add_function(&mut self.module, id, unwind)`,
`self.unwind.runtime(&mut self.module, symbol)`). No caller changes.

`unwind.rs`: add `ObjectModule` to the `cranelift_object` import:

```rust
use cranelift_object::{
    ObjectModule, ObjectProduct,
```

and replace each of these lines (keep `use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};`: the methods are `Module`'s):

```rust
fn personality(module: &mut dyn Module) -> Result<FuncId, String> {
    pub(super) fn new(module: &mut dyn Module) -> Result<Self, String> {
    pub(super) fn name_personality(&self, module: &mut dyn Module) -> Result<(), String> {
        module: &mut dyn Module,          // in `runtime` and in `add_function`
```

with

```rust
fn personality(module: &mut ObjectModule) -> Result<FuncId, String> {
    pub(super) fn new(module: &mut ObjectModule) -> Result<Self, String> {
    pub(super) fn name_personality(&self, module: &mut ObjectModule) -> Result<(), String> {
        module: &mut ObjectModule,
```

`unwind/seh.rs`: the same import (`use cranelift_object::{ ObjectModule, ObjectProduct, object::{..} };`),
and every `&mut dyn Module` becomes `&mut ObjectModule` (`Tables::add_function`, `Tables::runtime`,
`Tables::personality`, `Tables::define`, the field `module: &'a mut dyn Module` of `Shim`, which
becomes `module: &'a mut ObjectModule`), and

```rust
fn signature(module: &dyn Module, params: &[ir::Type], returns: &[ir::Type]) -> Signature {
```

becomes

```rust
fn signature(module: &ObjectModule, params: &[ir::Type], returns: &[ir::Type]) -> Signature {
```

(`signature(self.module, ..)` in `Shim` and `signature(module, ..)` in `define` coerce `&mut` to `&`
as before.)

### dedyn 2: `cached.rs`: `links` and `usable` take the resolver, not a predicate

All seven callers pass the same closure, `|symbol| symbols.provides(symbol) && symbols.linked(symbol)`,
and `usable` memoizes its answer by name alone (`self.usable`), so the predicate must be that one.
The closure's virtual call goes; the resolver's stays (dedyn 11). No test calls either.

`cached.rs`: add `SymbolResolver` to the `use super::{..}` list, and

```rust
    /// Whether the program has everything of rustc's that the entry was linked against.
    pub(super) fn links(&self, defines: &dyn Fn(&str) -> bool) -> bool {
        let funcs = self.funcs.iter().all(|it| match &it.target {
```

becomes

```rust
    /// Whether the program has everything of rustc's that the entry was linked against: what
    /// `symbols` provides and the executable has.
    // dyn: the build's one resolver (dedyn 11)
    pub(super) fn links(&self, symbols: &dyn SymbolResolver) -> bool {
        let defines = |symbol: &str| symbols.provides(symbol) && symbols.linked(symbol);
        let funcs = self.funcs.iter().all(|it| match &it.target {
```

(the body is unchanged: it calls `defines(symbol)`), and

```rust
    /// Whether there is an entry for `name` and for everything it calls, and what they import
    /// is in the program, as `defines` tells.
    pub(super) fn usable(&self, name: &str, defines: &dyn Fn(&str) -> bool) -> bool {
```

becomes

```rust
    /// Whether there is an entry for `name` and for everything it calls, and what they import
    /// is in the program, as [`Entry::links`] tells.
    // dyn: the build's one resolver (dedyn 11)
    pub(super) fn usable(&self, name: &str, symbols: &dyn SymbolResolver) -> bool {
```

with its two `it.links(defines)` becoming `it.links(symbols)` (`links` is on `impl Entry`,
`usable` on `impl Entries`).

Callers:
- `mod.rs` (`stable_name` of a root, ~5911):
  `entries.usable(&name, &|symbol| symbols.provides(symbol) && symbols.linked(symbol))` becomes
  `entries.usable(&name, symbols)`.
- `mod.rs` (~5960, in `in_parallel(threads, &|_, symbols, _| { .. })`): delete
  `let usable = |symbol: &str| symbols.provides(symbol) && symbols.linked(symbol);` and
  `entries.usable(name, &usable)` becomes `entries.usable(name, symbols)`.
- `mod.rs` (~6309) `entries.usable(name, &|symbol| symbols.provides(symbol) && symbols.linked(symbol))`
  becomes `entries.usable(name, symbols)`.
- `mod.rs` (~6819) the same; and (~6823) `if !it.links(&|symbol| symbols.provides(symbol) && symbols.linked(symbol)) =>`
  becomes `if !it.links(symbols) =>`.
- `units.rs` (~144) `entries.usable(&name, &|symbol| symbols.provides(symbol) && symbols.linked(symbol))`
  becomes `entries.usable(&name, symbols)`; (~614)
  `entries.usable(&name, &|symbol| {\n symbols.provides(symbol) && symbols.linked(symbol)\n })`
  becomes `entries.usable(&name, symbols)`.

### dedyn 3: `imports.rs`: `chosen_for` takes the set of instances it has

Its three callers pass "none" (`&|_| false`, twice) or `&|it| defined.contains(it)` over an
`FxHashSet<Instance>`. An empty `FxHashSet` allocates nothing.

```rust
pub(super) fn chosen_for(
    recorder: &mut Recorder<'_>,
    defined: &dyn Fn(&Instance) -> bool,
    budget: usize,
) -> Vec<Import> {
```

becomes

```rust
pub(super) fn chosen_for(
    recorder: &mut Recorder<'_>,
    defined: &FxHashSet<Instance>,
    budget: usize,
) -> Vec<Import> {
```

and in its body `if !super::tier::shared(db, instance) || defined(instance) {` becomes
`if !super::tier::shared(db, instance) || defined.contains(instance) {`. Callers:
- `imports.rs` `tallied(chosen_for(recorder, &|_| false, BUDGET))` becomes
  `tallied(chosen_for(recorder, &FxHashSet::default(), BUDGET))`.
- `units.rs` (~195) `imports::chosen_for(&mut recorder, &|_| false, imports::BUDGET)` becomes
  `imports::chosen_for(&mut recorder, &FxHashSet::default(), imports::BUDGET)` (`units.rs` imports
  `FxHashSet` already: `let mut defined: FxHashSet<Instance>`).
- `units.rs` (~755) `imports::chosen_for(&mut recorder, &|it| defined.contains(it), budget)` becomes
  `imports::chosen_for(&mut recorder, &defined, budget)`.

### dedyn 4: `fncache/dispatch.rs`, `fncache.rs`: `is_cache` is a generic

One closure (`fncache.rs`, `let cache = |id| self.dispatch_cache(id);`), so one instance; nothing
is copied. Asked per assignment of a function pointer the walk follows.

`fncache/dispatch.rs`: in `loaded_from` and in `loaded`, `is_cache: &dyn Fn(StaticId) -> bool,`
becomes `is_cache: &impl Fn(StaticId) -> bool,`. `fncache.rs` `fn_from_data`:
`cache: &dyn Fn(StaticId) -> bool,` becomes `cache: &impl Fn(StaticId) -> bool,`. Callers pass
`&cache` / `is_cache` as now; `.is_some_and(is_cache)` takes the `&impl Fn` as before.

### dedyn 5: `llvm/release.rs`: `instances` is a generic

One closure in production (`v2/release.rs` `prelinked`, `let instances = |request: u32| ..`) and
one in the test `the_facts_of_a_function_alone`: one instance of `prelink` and `optimistic`, two
of `facts` (~60 lines).

In `prelink`, `optimistic`: `instances: &dyn Fn(u32) -> bool,` becomes
`instances: &impl Fn(u32) -> bool,`; in `facts`:

```rust
fn facts<'ctx>(module: &Module<'ctx>, function: FunctionValue<'ctx>, instances: &dyn Fn(u32) -> bool) -> Facts {
```

becomes

```rust
fn facts<'ctx>(module: &Module<'ctx>, function: FunctionValue<'ctx>, instances: &impl Fn(u32) -> bool) -> Facts {
```

Callers (`&instances`, `facts(&module, function, instances)`, `optimistic(.., instances, ..)`)
are unchanged.

### dedyn 6: `vtable.rs`: `plain` is a generic

Two callers: `FunctionCx::vtable_layout` (`&|it| self.plain_trait(it)`) and `debuginfo/types.rs`
(`&|it| vtable::without_lifetimes(interner, it)`): two copies of `VtableLayout::of` and
`Walk::visit` (~45 lines together).

```rust
    pub(super) fn of(
        interner: DbInterner<'db>,
        trait_ref: TraitRef<'db>,
        plain: &dyn Fn(TraitRef<'db>) -> TraitRef<'db>,
    ) -> Self {
```

becomes

```rust
    pub(super) fn of(
        interner: DbInterner<'db>,
        trait_ref: TraitRef<'db>,
        plain: &impl Fn(TraitRef<'db>) -> TraitRef<'db>,
    ) -> Self {
```

and

```rust
struct Walk<'a, 'db> {
    interner: DbInterner<'db>,
    plain: &'a dyn Fn(TraitRef<'db>) -> TraitRef<'db>,
```

becomes

```rust
struct Walk<'a, 'db, P> {
    interner: DbInterner<'db>,
    plain: &'a P,
```

with `impl<'db> Walk<'_, 'db> {` becoming
`impl<'db, P: Fn(TraitRef<'db>) -> TraitRef<'db>> Walk<'_, 'db, P> {`. (`.map(self.plain)` takes
the `&P` as before.)

### dedyn 7: `stable.rs`: `declared`'s callbacks and `file_name`'s `named` are generics

`declared` has two callers (`lower_declared` with blaze-engine's `lower` and a no-op `found`;
`declared_statics` with a no-op `lower` and a pushing `found`): two copies of ~70 lines.
`lower_declared` has one caller (`blaze-engine` `lower_declared(db, &crates, &lower)`): one copy.
`file_name` has four closures (`StableNames::file`'s and the three `place_path` ones): four copies
of ~40 lines. `found` is called per item declared, `named` per file named.

```rust
pub fn lower_declared(db: &Db, crates: &[Crate], lower: &dyn Fn(&[DefWithBodyId])) {
```
becomes
```rust
pub fn lower_declared(db: &Db, crates: &[Crate], lower: &impl Fn(&[DefWithBodyId])) {
```

```rust
fn declared(
    db: &Db,
    crates: &[Crate],
    lower: &dyn Fn(&[DefWithBodyId]),
    found: &mut dyn FnMut(Declared),
) {
```
becomes
```rust
fn declared(
    db: &Db,
    crates: &[Crate],
    lower: &impl Fn(&[DefWithBodyId]),
    found: &mut impl FnMut(Declared),
) {
```

and in `file_name`, `named: &dyn Fn(Option<String>) -> Option<String>,` becomes
`named: &impl Fn(Option<String>) -> Option<String>,`. Callers unchanged (`&|_| ()`, `&mut |_| ()`,
`&mut |it| {..}`, `&named`, `&|path| place_path(symbols, path)`; the recursive
`file_name(db, symbols, out, file, named)` passes the same type).

### dedyn 8: `patch.rs`: the parts to ask are an `Either`, not a `Box<dyn Iterator>`

Each of `roots_to_ask`, `instances_to_ask`, `hydrated_to_ask` returns one of two iterators; the
`Box` was one allocation per call. `either` is a dependency already. Add `use either::Either;`.
In each of the three:

```rust
    ) -> Box<dyn Iterator<Item = usize> + '_> {
```
becomes
```rust
    ) -> impl Iterator<Item = usize> + '_ {
```

and
- `return Box::new((part..self.compiled.len()).step_by(parts));` becomes
  `return Either::Left((part..self.compiled.len()).step_by(parts));`
- `return Box::new((part..self.instances.len()).step_by(parts));` becomes
  `return Either::Left((part..self.instances.len()).step_by(parts));`
- `return Box::new((part..self.hydrated.len()).step_by(parts));` becomes
  `return Either::Left((part..self.hydrated.len()).step_by(parts));`
- each `Box::new(part_of(asked, part, parts))` becomes `Either::Right(part_of(asked, part, parts))`.

Callers (`for i in self.roots_to_ask(..)` and the two others) are unchanged.

### dedyn 9: `mod.rs`: three closures become concrete

(a) `Spelling::symbol_type`'s `list` takes two iterator types: a method of two instances.
Delete

```rust
        let list = |tys: &mut dyn Iterator<Item = Ty<'db>>| {
            tys.map(|it| self.symbol_type(it)).collect::<Option<Vec<_>>>().map(|it| it.join(", "))
        };
```

replace `list(&mut tys.iter())` (twice, the two `TyKind::Tuple` arms) with
`self.symbol_types(tys.iter())` and `list(&mut sig.inputs().iter().copied())` with
`self.symbol_types(sig.inputs().iter().copied())`, and add to `impl<'db> Spelling<'_, 'db>`, after
`symbol_type`:

```rust
    /// [`Self::symbol_type`] of each of `tys`, between commas.
    fn symbol_types(&self, tys: impl Iterator<Item = Ty<'db>>) -> Option<String> {
        tys.map(|it| self.symbol_type(it)).collect::<Option<Vec<_>>>().map(|it| it.join(", "))
    }
```

(b) `settled_function`'s `nameless` has one caller (`&nameless` in `settled`): one instance.
`nameless: &dyn Fn() -> Result<bool>,` becomes `nameless: &impl Fn() -> Result<bool>,`.

(c) The pass counting a body's whole-local mentions (~19611) has one `plain`. Replace

```rust
    let read = |operand: &Operand, plain: &mut dyn FnMut(&StoredPlace)| {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            plain(place);
        }
    };
```

with

```rust
    fn read(operand: &Operand, plain: &mut impl FnMut(&StoredPlace)) {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            plain(place);
        }
    }
```

The calls `read(operand, &mut plain)` are unchanged. (If `Operand` there carries a lifetime the
closure inferred, write it: `fn read(operand: &Operand<'_>, ..)`.)

### dedyn 10: `cranelift`'s `dyn TargetIsa`: kept, at cranelift's API

`Context::compile`, `Context::compile_stencil` and `CompiledCode::create_unwind_info` take
`&dyn TargetIsa`, and cranelift's backends are private (`X64Backend` is `pub(crate)`), so there is
nothing concrete to hold; the one ISA is made once per process (`crate::target()`). Insert above
each of these lines:

```rust
// dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
```

(indented as the line): `fragment.rs` `pub(super) isa: &'static dyn TargetIsa,` (both fields),
`pub(super) fn new(isa: &'static dyn TargetIsa) -> Wants {`, `isa: &'static dyn TargetIsa,` (the
parameter, ~1412); `debuginfo.rs` `isa: &dyn TargetIsa,` (twice) and
`fn new(isa: &dyn TargetIsa, directory: &str, variables: bool, apart: Option<String>) -> Self {`;
`unwind.rs` `pub(super) fn of(isa: &dyn TargetIsa, context: &Context) -> Result<Option<Self>, String> {`.

### dedyn 11: `dyn SymbolResolver`: kept, the v1 resolver chosen once per build

`SymbolResolver` is only on the v1 crate-at-a-time path (`ObjectCodegen`, `Spelling`, `later`,
`units`, `stable`, `patch`, `type_id`, `settled`, `fncache`); v2 never takes one (`FunctionCx`
holds a concrete `Recorder`). Its three implementations (`blaze-engine` `Resolver`, `blaze-wasm`
`names::Resolver` and `Names`, the last two chosen per build at `blaze-wasm/src/lib.rs`
`let resolver: &dyn SymbolResolver = match host {`) are in crates that depend on this one, so no
enum here can name them, and a generic would copy `ObjectCodegen` (most of `mod.rs`) three times.
Each call is a lookup that returns an allocated `String`. Insert above each line below (indented
as it):

```rust
// dyn: the build's one resolver, made by blaze-engine or blaze-wasm (which depend on this crate)
```

- `fncache.rs`: `symbols: &dyn SymbolResolver,` (~202),
  `fn compile_ahead(symbols: &dyn SymbolResolver, nodes: &[&Node]) {`
- `later.rs`: `pub(super) symbols: &'k dyn SymbolResolver,`
- `mod.rs`: every `symbols: &dyn SymbolResolver,` / `symbols: &'a dyn SymbolResolver,` /
  `symbols: Option<&dyn SymbolResolver>,` (~792, 826, 1358, 1530, 1603, 1910, 2236, 2932, 2956,
  2986, 4055, 5497), and the closures `let run = |db: &Db, symbols: &dyn SymbolResolver| {`,
  `let work = |db: &Db, symbols: &dyn SymbolResolver, _: usize| {` (twice)
- `patch.rs`: `symbols: &dyn SymbolResolver,` (~1581, 1668, 1800, 2642), `symbols: Option<&dyn SymbolResolver>,` (~2246)
- `settled.rs`: `symbols: &dyn SymbolResolver,`
- `stable.rs`: `symbols: &'a dyn SymbolResolver,` (~167, 197), `symbols: &dyn SymbolResolver,`
  (~873, 981, 997, 1010), `fn place_path(symbols: &dyn SymbolResolver, path: Option<String>) -> Option<String> {`
- `type_id.rs`: `symbols: &'a dyn SymbolResolver,` (~115, 141)
- `units.rs`: `symbols: &dyn SymbolResolver,` (~135, 552, 595, 654),
  `fn add(&mut self, symbols: &dyn SymbolResolver, found: Found, wave: &mut Vec<Kind>) {`,
  `fn found(&mut self, symbols: &dyn SymbolResolver) {`

And above `pub type ParallelWork<'a> = dyn Fn(&Db, &dyn SymbolResolver, usize) + Sync + 'a;` and
`fn in_parallel_beside(&self, count: usize, work: &ParallelWork<'_>, beside: &mut dyn FnMut()) {`:

```rust
// dyn: a method of the object-safe `SymbolResolver`, run once per parallel phase of a build
```

### dedyn 12: the other kept callbacks

Insert above each (indented as it):

- `mod.rs` `compile_or_hold`'s `hold: &dyn Fn() -> bool,` and `compile_ahead`'s
  `beside: &mut dyn FnMut(&mut Self, Option<&mut Ready<'_>>),`:
  `// dyn: called once per build`
- `mod.rs` `pub fn compile_unnamed(db: &Db, unnamed: &Unnamed, done: &dyn Fn() -> bool) {`:
  `// dyn: blaze-engine's stop signal (itself a `&dyn Fn`), asked once per function compiled`
- `watch.rs` `pub type Sink = dyn Fn(u32, bool, Vec<Placed>) + Send + Sync;`:
  `// dyn: the observer blaze-engine installs once per process, told once per batch of functions`

### Tests

No test calls what dedyn 1-3 and 8-9 change. dedyn 5's test `the_facts_of_a_function_alone`
passes `&instances` as before. Nothing new to dispatch: each is a concrete type or a generic.
