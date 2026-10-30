---
status: plan
summary: speed-hir's index for lane db: name resolution, hir-def's attributes, signatures and interned locs, macro expansion (hir-expand) and base-db's inputs brought to the v2 speed bar; headline before/after per part and what is left.
date: 2026-10-29
---
# speed-hir (lane db): name resolution, expansion, inputs

`speed-hir` split `crates/hir-def/`, `crates/hir-expand/` and `crates/base-db/` by whole files.
The HIR layout parts (expression store, item tree) are lane format's:
[../format/speed-hir.md](../format/speed-hir.md). Each part's notes give the detail, the
benchmark it is measured against and the before/after.

| part | paths | notes |
|---|---|---|
| name resolution (fixed point, scopes, paths, find_path) | hir-def `nameres*`, `item_scope.rs`, `per_ns.rs`, `visibility.rs`, `resolver.rs`, `find_path.rs`, `import_map.rs` | [speed-hir-nameres.md](speed-hir-nameres.md) and its four parts |
| attributes | hir-def `attrs.rs`, `attrs/` | [speed-hir-defs.md](speed-hir-defs.md) |
| signatures | hir-def `signatures.rs` | [speed-hir-defs-sigs.md](speed-hir-defs-sigs.md) |
| interned locs, lang items, upvars | hir-def `lib.rs`, `lang_item.rs`, `upvars.rs`, small files | [speed-hir-defs-locs.md](speed-hir-defs-locs.md) |
| macro expansion | `crates/hir-expand/` but `builtin/` | [speed-hir-expand.md](speed-hir-expand.md) |
| builtin macros | hir-expand `builtin.rs`, `builtin/` | [speed-hir-expand-builtin.md](speed-hir-expand-builtin.md) |
| inputs | `crates/base-db/` | [speed-hir-basedb.md](speed-hir-basedb.md) |

## Headline numbers

| measure | before | after | part |
|---|---|---|---|
| imports retried per pass of the fixed point | every unresolved import | only those whose awaited slot was written | nameres |
| `resolve_import` calls, a chain of 64 modules each importing the next | about 4000 (O(n²)) | 127 (O(n)) | nameres |
| hashes and probes per `ItemScope::get` | 3 and 3 | 1 probe run for all namespaces | nameres |
| allocations per path lookup | one `ModPath` per def map crossed | 0 | nameres |
| name compares per attribute read | up to 38 string `eq`s, a `SmolStr` per segment | 1 hash, 1.18 probes on average, 0 allocations | attrs |
| memos per item `blaze-items` hashes | 5 | 1 | attrs |
| allocations for `EnumVariants` of 10 variants | about 11 | 2 | signatures |
| Fx rounds to intern a loc (`Macro2Loc`, `ItemLoc`) | 8, 4 | 3, 3 | locs |
| `ModPath` | heap `SmallVec` per 2+ segment path, then a second intern | 16 bytes, `Copy`, segments interned once; stored by value everywhere | expand |
| hygiene interns per transcribed `macro_rules` token | up to 3 lookups and 3 interns | once per distinct context per expansion | expand |
| span lookups per token, syntax to token tree | 1 binary search | 2 compares (in-order cursor) | expand |
| trees built per derive of a ten-field struct | 100+ | 1 | builtin |
| locks per `file_text`/`source_root` read | 1 `DashMap` shard `RwLock` (2 shared RMWs) | 0 (two acquire loads) | basedb |
| per unchanged file when a view is set | 2 allocations, 3 locks | 0 and 0 | basedb |
| `Arc` RMWs per `EditionedFileId::parse` | 4 | 0 (`returns(ref)`) | basedb |

Invalidation: a body edit runs no def-map or signature query (`demand_tests.rs`), and an item
edit that changes no name, visibility or import leaves both crates' frozen def-map bytes equal and
runs no dependent def map (`nameres/tests/backdate.rs`). rustc re-runs its resolver for the whole
crate on any edit, so this is finer than its red-green graph, and on par with Unison for edits
that keep names.

## Fitting the parts together (speed-hir)

- `Interned<ModPath>` removal across five children and `hir-expand`: `AstIdWithPath::path`
  (`hir-def/src/lib.rs`) and `hir_expand::attrs::Attr::path` are `ModPath`; the collector,
  `assoc.rs` and `attr_resolution.rs` read them by copy. `crates/hir/src/semantics.rs:2843` is
  requested in `v2/requests/db.md`; `intern::impl_internable!(ModPath)` in
  `hir-expand/src/mod_path.rs` can go once that edit lands.

## Left, and what it would take

- `parse_macro_expansion` and `EditionedFileId::parse` need `Hash` for `syntax::Parse<T>` and
  `mbe::ValueResult` under db-salsa's `lru` bound (requested from format by db-expand).
- hir-expand: one parse per `#[derive]` attribute instead of one per derive needs the derive's
  index in `BuiltinDeriveExpander::expander()`'s public signature; spans are 20 bytes against
  rustc's 8 (format's `Span`).
- base-db: `all_crates` still clones an `Arc` per call (callers in other lanes); `ContentId::of_many`
  hashes inputs one by one (format's choice not to use `blake3::platform`'s many-input path);
  `vfs::FileSet` has no remove, so adding or removing a file rebuilds its root's file set; views
  share no structure beyond salsa's inputs.
