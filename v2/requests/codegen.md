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

## 7. cargo: `ItemHashes::new` takes a `Send + Sync` closure

Where: `crates/blaze-cargo/src/serve/unit/codegen.rs:260-265` (`names`).

What: `hir_ty_codegen::v2::ItemHashes::new` takes
`Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>` (`Hashes: Sync`, read from every
worker). The closure there captures `mounted` and `job.lowered.paths()`; both must be
`Send + Sync` (an `Arc` of an immutable map, not an `Rc` or a `RefCell`).

Why: otherwise the call doesn't type-check.

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

## 9. items (optional, speed): spans of a body form and a by-move code query

Where: `crates/blaze-items/src/hash.rs`.

What: `ItemHashes::code` rebuilds each body's span ids from `mir_body`, `mir_body_for_closure`
and `mir_body_for_coroutine_by_move` and hashes them beside `code_hash`/`closure_code`/
`coroutine_code`, and has no by-move code query. A code hash that already covers the span ids
the MIR names, and a `coroutine_by_move_code` query, would hash each body once in one place.

Why: hash once (principles.md); today the MIR's spans are walked twice per body.
