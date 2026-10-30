---
status: plan
summary: frontend.md item 14 — a file is reparsed from its previous parse and its item tree is assembled from per-item lowerings, so an edit reparses and re-lowers only the items it touches, and the assembled tree is byte-equal to the whole-file lowering.
date: 2026-10-29
---
# Reparse and re-lower only what an edit touches (frontend-deps-reparse)

Item 14 of [frontend.md](../../design/frontend.md): reparse only the items an edit touches, and
re-lower only those items into the item tree.

## State of the art

- **rustc** has no incremental parse: it parses every file of the crate again and makes
  incrementality from queries over HIR owners (`hir_owner`, `hir_owner_nodes`), each item's
  HIR hashed (`HashStable`) so a query reading one owner is a firewall for the others.
- **Roslyn** keeps a red-green tree: green nodes are position-free and shared, an edit rebuilds
  the green spine above the change and reuses every untouched green subtree; red nodes add
  positions on demand. Its incremental parser reuses old nodes the lexer agrees on.
- **rust-analyzer** reparses a single token or a `{}` block in place (`reparsing.rs`), else the
  whole file, and lowers an item tree per file: any edit outside a body re-lowers the whole file,
  and its `AstIdMap` firewall stops the edit from reaching name resolution only when the ids
  hold.

blaze takes the Roslyn shape for parsing (the flat tree's position-free `hash_subtree` is its
green identity) and the rustc shape for lowering (one query per item, keyed by what it reads).

## Parsing from the previous parse

`SourceFile::parse_file(key, text, edition, outline)` (crates/syntax) keeps the last parse of each
file in a small direct-mapped table (`parsing/kept.rs`, 128 slots by file id) and, when one is
there of the same mode, makes the new parse with `Parse::reparse_to`: the edit is the
difference between the kept tree's text and the new text (`edit_between`: common prefix and
suffix, compared 64 bytes at a time, backed off to char boundaries), and
`incremental_reparse_onto` reparses the smallest token, block, body or item holding it, the
tree sharing the new text. Several edits since the kept parse compose by construction: the
difference is taken from the text the kept tree holds, never from a log of edits, so nothing
stale is trusted, and a kept parse from another database is as good as its text.

When no reparse applies (no kept parse, the edit spans items, the relexed token changes kind),
the file is parsed whole. That is not a fallback: `reparse_to` gives the tree a full parse of the
new text gives (`reparse_to_random_edits` holds them equal), so the two paths are one result
reached at two costs. The outline choice is kept: an outline is only reparsed as an outline.

base-db is db's: the request in `v2/requests/db.md` (`## frontend-deps-reparse: parse a file
from its previous parse`) gives the exact edit to `EditionedFileId::parse`, one call to
`parse_file` keyed by the file's span id.

## The item tree per item

`file_item_tree_query` assembles a source file's tree from parts (`item_tree/items.rs`):

- `item_sources(file, krate)` walks the file's top-level items once and makes one tracked
  `ItemSource` per item. Its identity, `ItemSyntax`, is a key: the file's span id, the item's
  ast id, and a BLAKE3 fingerprint of what lowering reads of the item: its syntax (the flat
  tree's position-free subtree hash, with the bodies of `fn`s, `const`s and `static`s and the
  items of `impl`s and `trait`s left out but for the inner attributes at their start), the ast
  ids of the items it is made of (`[start, kind, id]` from its start) and the span anchors
  lowering looks up in it (`[start, id]`, the anchors of an `impl`'s or `trait`'s items left
  out, as no span past their start is looked up). The ids and anchors of all items are two
  arrays the items share, with the tree, in one `Arc`: no allocation per item.
- `item_fragment(item)` lowers one item (`Ctx::for_item`, `Ctx::lower_item`) from the item's
  node in its tree, its ids by binary search and a `RealSpanMap` made of its anchors. It reads
  nothing tracked but the crate's `cfg`s, so it runs again only when the item's key changes.
- `top_attrs(file, krate)` lowers the file's inner attributes and, for a crate root, the crate
  attributes from the command line: its own part, owner `TOP_OWNER`, first in `attrs` and
  `cfgs` and last in `attr_owners`, as the whole-file lowering has it.
- `assemble(top, fragments)` concatenates the columns in one pass, each allocated once at its
  summed length, moving ids by where the part's columns start: items and attribute owners by
  the item base, payloads by their kind's column, `children`, `use_trees`, `use_tree_lists`,
  `ctor_vis`, names, paths, visibilities, attributes and `cfg`s by theirs, sentinels
  (`NameId::NONE`, `AliasId::NONE`/`UNDERSCORE`, `PathId::NONE`, the builtin visibilities,
  `IdRange::NONE` of `mod m;`, an empty constructor list, a non-prefixed `use` tree's list,
  `AttrOwner::ENABLED`) left as they are.

For the assembled tree to be the whole-file tree to the byte, names, paths and visibilities are
deduplicated within each top-level item only, in every lowering (`Scoped` in `lower.rs`; the
whole-file lowering seals the scope after each top-level item), so no name is rehashed on
assembly. A tree's tables can hold a name once per item that uses it; the persist schema is
bumped to 4 as the tables' contents changed.

Macro files and blocks (`block_item_tree_query`) keep the one-pass lowering (`lower_whole`): an
expansion is made anew and a block's tree is small.

Known costs, all bounded (the bounds and the arithmetic are in
[speed-of-light.md](speed-of-light.md), section frontend-deps-reparse):

- An item kept from an older parse keeps that parse's tree (text and words once per file) until
  every item cut from it is edited or gone.
- `item_sources` runs on every edit of the file: one walk and one BLAKE3 pass over the items'
  lowered syntax. `top_attrs` runs too and comes out equal.
- An edit inside a `fn` of an inline module re-lowers the module: it moves the module's later
  items, whose starts are in its key.
- Attributes are cloned on assembly, which allocates for those holding paths or inputs.

Nothing in `file_item_tree_query` parses a file other than the one asked for.

## What each test proves

crates/syntax:

- `reparse_to_random_edits`: one to three composed random edits on every syntax source, full and
  outline, reparsed from the old tree, give the full parse's tree, errors and text, the tree
  sharing the new text.
- `edit_between_cuts_the_common_ends`: the edit found leaves no common first or last char and
  rebuilds the new text, across multi-byte chars and long runs.
- `a_kept_parse_makes_the_parse_of_the_new_text`: a sequence of texts through `parse_file`, full
  and outline, each the parse of its text; another file in the same slot evicts the kept one.

crates/hir-def/src/item_tree:

- every fixture of `tests.rs` (`check`, `owned_tree`): the assembled tree equals the whole-file
  lowering, bytes included, and is the tree `file_item_tree` gives.
- `real_files_assemble_to_their_whole_trees`: the same for every source of the item tree, name
  resolution and the syntax tree, one database reusing items across them.
- `random_edits_assemble_to_a_fresh_whole_tree`: after each of six random edits per file
  (inserts of items, attributes, `cfg`s, visibilities, macros, braces, or text copied from the
  file), the tree the edited database assembles from kept and re-lowered items equals, to the
  byte, the tree a fresh database lowers the text to whole.
- `an_edit_inside_bodies_lowers_no_item`: edits inside `fn`, `impl` method, `trait` method and
  `const` bodies run `item_fragment` zero times (salsa's executed log), the tree still whole.
- `an_edit_of_one_item_lowers_it_alone`: renaming a `fn`, changing a field's visibility, making
  an `impl` a trait impl, adding `pub`, adding an item, changing an inline module's item, adding
  an inner attribute to a `fn` body each run `item_fragment` exactly once.
