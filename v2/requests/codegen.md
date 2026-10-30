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

## 4. coordinator: index entry

Where: `docs/v2/plan.md`.

What: link `../v2-codegen.md` (status: plan) as the codegen lane's design notes. If lane
notes now belong under `docs/v2/`, move it to `docs/v2/codegen-design.md` and fix the
links in it.

Why: docs-say-what-they-are. The notes are the lane's design and nothing links them yet.
