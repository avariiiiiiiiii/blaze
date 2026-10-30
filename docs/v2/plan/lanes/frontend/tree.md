---
status: plan
summary: frontend.md item 16, the handle layer over the flat syntax tree replaced by borrowed Copy views (Node<'t>, typed ast views), every build-path caller moved, the handles deleted.
date: 2026-10-29
---
# frontend-tree: borrowed views instead of syntax handles

[frontend.md](../../design/frontend.md) item 16. `crates/syntax-tree` is a flat tree, but callers
walk it through `SyntaxNode`/`SyntaxToken` cursors: each one counts its tree's root (a `Cell`
bump on every clone and drop), owns part roots in a `RefCell<FxHashMap>`, is not `Send`, and the
typed `ast` wrappers clone one per accessor. Views replace them.

## The view API

In `crates/syntax-tree/src/view.rs` (module `syntax_tree::view`, re-exported by `syntax::view`):

- `NodeId { part, at }`: an element's number `at` in the file's tree (`part == NodeId::FILE`)
  or in the part of lazy node `part`. 8 bytes, `Copy + Ord + Hash`: what a caller stores, with
  the `FileId`, to find a node again (`Node::at`). `AstId`/`AstPtr` stay for what must survive a
  reparse.
- `Node<'t>`, `Token<'t>`: `{ file: &'t FlatTree, tree: &'t FlatTree, id: NodeId }`, 24 bytes,
  `Copy + Send + Sync`. `tree` is `file` or the part the element is in, so no step looks a part
  up. Equality and hashing are `(file address, id)`, as the cursors' identity was.
  `Element<'t> = NodeOrToken<Node<'t>, Token<'t>>`.
- Every cursor method, same name and meaning, taking `self`: `kind`, `text_range`, `index`,
  `parent`, `ancestors`, `children`, `children_with_tokens`, `first_child`, `last_child`,
  `first_child_or_token`, `last_child_or_token`, `next_sibling`, `prev_sibling`,
  `next_sibling_or_token`, `prev_sibling_or_token`, `siblings`, `siblings_with_tokens`,
  `first_token`, `last_token`, `descendants`, `descendants_with_tokens`, `preorder`,
  `preorder_with_tokens` (with `skip_subtree`), `token_at_offset`, `covering_element`,
  `child_or_token_at_range`, `descendant_at_range`, `child_where`, `child_token`,
  `ancestor_where`, `first_child_token_text`, `tree_top`, `is_lazy`, `flat`; on tokens also
  `next_token`, `prev_token`, `parent_ancestors`.
- `text()` is `&'t str`: a tree's ranges index the source it shares (parts too), so a node's
  text is one slice, not a walk of its tokens (`SyntaxText` goes).
- `subtree()` is the old `clone_subtree`: an owned `FlatTree` of the node's subtree.
- Lazy nodes are as transparent as for cursors: going down into one enters its part (parsed
  then, once, by `FlatTree::part`), going up from a child of the part's root comes back to the
  lazy node. A view never stands at a part's root element: that element is the lazy node.

Iteration reads the columns: `descendants` is a scan of the element numbers `at..end`
(entering a part when it passes a lazy node, which an outline has, a full parse has not);
children step by `FlatTree::end`; nothing is allocated, counted or cloned anywhere.

## Typed views

`crates/syntax/src/view.rs` and `view/`, module `syntax::view`:

- `AstNode<'t>: Copy` (`can_cast`, `cast`, `syntax`, `kind`), `AstToken<'t>`,
  `AstChildren<'t, N>`; the traits (`HasName<'t>`, `HasAttrs<'t>`, ...) take `self`.
- `view/generated/{nodes,tokens}.rs`: every node, enum, `Any*` type and accessor of today's
  `ast`, same names, generated from `ast/generated/` by `tools/v2/ast_views.py` while both
  exist (the grammar's hand edits go into both until the handles are deleted).
- `Typed`: a handle type's view type (`<ast::Fn as Typed>::Of<'t> = view::Fn<'t>`), so
  `Parse<T>::view()` and `AstPtr<N>::to_view(root)` work before the handles are gone.
- `Parse::root() -> Node<'_>`, `Parse::view() -> T::Of<'_>`.
- The extension layer (`node_ext`, `expr_ext`, `token_ext`, `traits`, `prec`, `operators`)
  ported onto views.

Callers write `use syntax::view as ast;` so their bodies already say `ast::Fn<'t>`; deleting the
handles makes `syntax::ast` the views and the alias goes. Types that only tag a kind
(`FileAstId<N>`, `AstPtr<N>`) take `ast::Fn<'static>`.

## Proof

- `crates/syntax/tests/view.rs`: for every parser fixture (`crates/parser/test_data/**/*.rs`) and
  every `crates/**/*.rs` file, full and outline parses, a walk through the views and one through
  the cursors give the same elements in the same order with the same kind, range, text, index,
  parent, siblings, and the same answers to `token_at_offset`, `covering_element` and
  `descendant_at_range` at every token boundary; the typed views give the same accessors as the
  handles (every generated accessor of every node). Once the handles are deleted, the cursor side
  is replaced by stored expectations.
- An allocation budget (`blaze-alloc-count`, the same file): a whole-file walk through
  `descendants_with_tokens`, `preorder_with_tokens`, `children` at every node, `text`,
  `parent` and `ancestors` allocates nothing on a full parse, and nothing on an outline whose
  parts are parsed.

## State of the art

rustc's `rustc_ast` walks are borrowed references into an arena (`&'hir Expr<'hir>`), no
counts; Roslyn's red nodes are made lazily and cached; rust-analyzer's rowan counts every
cursor. Views are rustc's shape over a packed array: a step is arithmetic on `u32` words.
