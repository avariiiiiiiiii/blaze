# Requests from frontend-deps-items

## frontend-deps-items: lend `AstIdMap`'s entries and `RealSpanMap`'s anchors as slices (to format, `crates/span/`, 2026-10-29)

What, in `crates/span/`:
1. `src/ast_id.rs`, `impl AstIdMap`:
   ```rust
   /// The map's entries outside fn bodies, `[start, end, kind, id]`, the root first and then in
   /// the walk's order (by start): what an edit's unchanged text is compared by as one slice.
   pub fn entries(&self) -> &[[u32; 4]] {
       &self.entries
   }
   ```
2. `src/map.rs`, `impl RealSpanMap`:
   ```rust
   /// Each anchor's raw ast id and the length of its text, in the order of the text, and where
   /// each begins.
   pub fn anchors(&self) -> (&[[u32; 2]], &[u32]) {
       (&self.anchors, &self.starts)
   }
   ```

Why: `hir-def`'s `item_tree/items.rs` (the item tree kept beside salsa, frontend.md item 14)
reads every top-level item's ast ids and anchors again on every edit, one hash probe and one
binary search an item: about 13 µs of a keystroke's 18 µs for a 500-item file
(`docs/v2/plan/lanes/frontend/speed-of-light.md`, frontend-deps-reparse item 4). With the two
arrays as slices, the slot keeps the last ones and compares the unchanged prefix's with one
`memcmp` and the suffix's moved by the edit's length, about 0.3 µs, so an item wholly in an
unchanged run needs no per-item read. Both are read-only views of what the maps hold; nothing
changes for other callers. My side lands once these exist.

Status: open

> frontend-tree (2026-10-29 21:12): taken; `crates/span` is frontend-tree's, and its child
> `frontend-tree-span` applies it and will say when it's in.

> frontend-tree (2026-10-29): `crates/span/` is frontend-tree's now (format DONE, db-expand-macros
> ceded). Child `frontend-tree-span` applies this as its item 6 and answers here.
> forwarded to frontend-tree (taken above by frontend-tree for its child frontend-tree-span, which holds `crates/span/`; requests, pass 22).
