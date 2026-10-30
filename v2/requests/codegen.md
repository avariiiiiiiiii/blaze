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
notes now belong under `docs/v2/`, move it to `docs/v2/codegen-design.md` and fix the
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

> forwarded to codegen: every finding is in lane codegen's own crates (`blaze-codegen`, `hir-ty-codegen`, `hir-ty-mir`); lane codegen is still running.

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

## 7. cargo: `ItemHashes::new` takes a `Send + Sync` closure

Where: `crates/blaze-cargo/src/serve/unit/codegen.rs:260-265` (`names`).

What: `hir_ty_codegen::v2::ItemHashes::new` takes
`Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>` (`Hashes: Sync`, read from every
worker). The closure there captures `mounted` and `job.lowered.paths()`; both must be
`Send + Sync` (an `Arc` of an immutable map, not an `Rc` or a `RefCell`).

Why: otherwise the call doesn't type-check.

> forwarded to cargo: `serve/unit/codegen.rs` is lane cargo's.

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

### blaze-engine
- `crates/blaze-engine/src/lib.rs:7968` block: 3 ops (`getpgrp`, `getpid`, `killpg`). Split, with `#[cfg(unix)]` on a plain block and the same evaluation order: `#[cfg(unix)] {` / `// SAFETY: \`getpgrp\` takes no pointer and cannot fail.` `let group = unsafe { libc::getpgrp() };` / `// SAFETY: \`getpid\` takes no pointer and cannot fail.` `let me = unsafe { libc::getpid() };` / `if group == me {` `// SAFETY: this process leads its group (checked just above), which the daemon made for this worker alone, so \`killpg(0, ..)\` kills only what the worker started, and itself.` `unsafe { libc::killpg(0, libc::SIGKILL) }; } }`
- `crates/blaze-engine/src/live.rs:212` block: 2 ops (`SetEvent`, `CloseHandle`). Split: `// SAFETY: \`event\` is the non-null handle \`OpenEventW\` opened with \`EVENT_MODIFY_STATE\`, the right \`SetEvent\` needs; it is closed only below.` `unsafe { SetEvent(event) };` then `// SAFETY: \`event\` is this function's own open handle, closed once here and not used after.` `unsafe { CloseHandle(event) };`

### hir-ty-mir
- `crates/hir-ty-mir/src/lib.rs:1` crate: no unsafe; add `#![forbid(unsafe_code)]` (after the `//!` docs). Its only `unsafe` matches are a field pattern (`unsafe_: _`, `mir/lower.rs:1024`) and two strings (`mir/type_name.rs:483`, `mir/eval/shim/reflection.rs:601`). The `unsafe impl`s that `#[salsa::tracked]`/`#[salsa::input]` expand to come from `salsa-macro-rules`' external `macro_rules!`, which `unsafe_code` does not report; `blaze-items` and `hir-ty-borrowck` already pair `forbid(unsafe_code)` with tracked fns. `src/lib.rs` is the only target root.

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
