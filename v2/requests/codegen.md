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
