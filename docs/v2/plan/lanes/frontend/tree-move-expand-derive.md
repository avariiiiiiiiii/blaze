---
status: plan
summary: The built-in derives read their own item in place (views of the parse of the derive's token tree), and CoercePointee writes its impls as tokens instead of editing a tree; the handle and editor forms stay in derive_macro/reference.rs as the reference the tests compare against.
date: 2026-10-29
---

# frontend-tree-move-expand-derive: the built-in derives onto views

Lane `frontend-tree-move-expand-derive`, a child of `frontend-tree-move-expand`
([tree-move-expand.md](tree-move-expand.md)). The files are
`crates/hir-expand/src/builtin/derive_macro.rs` and `builtin/derive_macro/{reference,tests}.rs`.
Their owner is lane db (db-expand). Every edit is in `v2/requests/db-expand.md` under
`## frontend-tree: hir-expand builtin/derive_macro.rs: onto views (10)`. That request needs
request (7) applied first, which moves `derives_copy`.

## What it was

The derive parses its input token tree into a flat tree. `token_tree_to_syntax_node` is the
only parse, and it is unchanged. The rest went through handles:

- `to_adt_syntax` made a `SyntaxNode` per call (`parsed.syntax_node()`) and cast it to
  `ast::Adt`.
- `parse_adt_from_syntax` walked handles, made each tree with `syntax_node_to_token_tree`, and
  cloned the field list's node.
- `from_derive`, `coerce_shared_target` and `attr_words_have` read handles too.
  `coerce_shared_target` made a second `SyntaxNode` for its reparse check.
- `coerce_pointee_expand` copied the item into a `SyntaxEditor`. It rebuilt each new `where`
  predicate from text with `make` (a reparse per constructor), rebuilt the whole `where` clause
  from text, finished the edit into a new green tree, and converted that tree back to tokens.

## What it is

- `parse_item` (the parse) and `adt_of` (a view `ast::Adt<'_>` of the parse's root) replace
  `to_adt_syntax`. They raise the same three errors.
- `parse_adt_from_syntax`, `push_fields` and `name_to_token` take views, generic over
  `&impl SpanMapper`. Every tree comes from `syntax_bridge::call_input`, which runs the same
  `FlatConverter` `syntax_node_to_token_tree` ran on the flat node, so the output is the same
  tree. `span_for(range)` is `span_at(range.start())` for a `SpanMap`. No node is cloned.
- `from_derive` and `coerce_shared_expand` read the view. `coerce_shared_target_tokens` uses
  `call_input_modified` with the parentheses' token ids removed. Its reparse check casts
  `parse.root()`.
- `CoercePointee` is a direct token build (`coerce_pointee_derive`, `write_pointee_predicates`,
  `pointee_self_type`, `pointee_paths`, `write_respanned`). There is no mutable tree, no green
  rewrite and no reparse. The checks run in the same order on the view.
  - `info` is the struct as parsed, every span `span` (the `AtSpan` mapper). That is what the
    one-entry span map over the edited copy gave.
  - The added predicates are appended in the editor's order. They are spaced as the editor's
    text reparsed them:
    - the original predicates keep their own tokens;
    - a written `:` and `+` are `Alone`;
    - the last punct of a type is `Joint` before its `:`;
    - a punct before a substituted path is `Alone`, because `__S` is an ident.
  - The pointee's paths are found by the same walk as `substitute_type_in_bound`, which never
    short-circuits. They are skipped when copied, and `__S` is written at their start.
  - Names are raw where `make::name_ref` escaped them: a keyword of `Edition::CURRENT`.
  - The `#[pointee]` attribute the editor deleted is never read by the impls (type parameters
    are written by name and bounds), so it is not written either.

Each handle helper nothing in the file uses any more moves to `reference.rs`'s `handle` module,
test-only: `attr_words_have`, `parse_adt_from_handle` with `push_fields`, `from_derive`,
`coerce_shared_target_tokens`, `type_param_span` and the editor's `coerce_pointee_derive` with
its nested functions. `to_adt_syntax` is deleted.

## One deliberate difference

The editor reparsed every predicate of a `CoercePointee` item, its own and the added ones, in
`Edition::CURRENT`. A name that is a keyword only in a later edition than the item's (`gen`,
`async` and `dyn` in 2015) could lex differently there. The new build keeps the tokens as the
item's own edition parsed them, which is what rustc's derive, working on the AST, uses. Every
fixture is in the current edition, so the tests see no difference.

## What the tests prove

- `derives_match_nested_quotes` checks every derive over the view against the nested-quote
  reference.
- `parse_adt_matches_the_handle_form` compares the view parse with the handle parse field by
  field (name, shape, parameters, lifetimes, `where`, associated types, `packed`), and their
  errors, on `ITEMS` and `POINTEE_ITEMS`.
- `from_matches_nested_quotes` compares `From` with the handle form and with the nested quotes,
  including every refusal: enum, union, no field, two fields, a missing field type, generics.
- `coerce_shared_target_matches_the_handle_form` covers one and two attributes, a bare one, a
  `=` one, an empty one, junk tokens, and a path with generics.
- `coerce_pointee_matches_the_editor` runs every item of `POINTEE_ITEMS` and `ITEMS` through
  the editor form and the token build and compares the tree, `Debug`, `Display`, encoding and
  error. It covers each refusal in order: not a struct, no field, no type parameter, several
  with no `#[pointee]`, two `#[pointee]`s (the error's span covers the default), no `?Sized`,
  `?marker::Sized`, invalid item, and not `repr(transparent)`. It also covers every
  `substitute_type_in_bound` arm in a bound and in a `where` predicate, and a punct before a
  substituted `::T`.
- `the_item_read_in_place_allocates_no_more` checks that the view parse allocates no more than
  the handle parse did.
- The `Clone` budget (`a_derive_allocates_nothing_per_field`) now runs over the view.

No build was run (the lane's brief). The tests are for db-expand to run when it applies the
request.
