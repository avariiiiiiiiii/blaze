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
- `Node<'t>`, `Token<'t>`: `{ file: &'t Arc<FlatTree>, tree: &'t FlatTree, id: NodeId }`, 24
  bytes, `Copy + Send + Sync`. `tree` is `file` or the part the element is in, so no step looks
  a part up. Equality and hashing are `(the file tree's address, id)`, as the cursors' identity
  was: two `Arc`s of one tree view the same elements. `file()` is `&'t FlatTree`;
  `shared_file()` is the `&'t Arc<FlatTree>`, so something that must walk the tree after the
  view's borrow ends (an `AstIdMap`'s unwalked bodies) keeps a count of it. `Node::root` and
  `Node::at` take `&'t Arc<FlatTree>` (a `Parse` holds one).
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
(`FileAstId<N>`, `AstPtr<N>`, `InFile<AstPtr<N>>` of source maps) keep the handle type as the
tag (`AstPtr<syntax::ast::Fn>`, never made into a handle) until `span`'s `AstIdNode` moves;
`AstPtr::<N>::to_view(root)` gives the view `<N as Typed>::Of<'t>`.

A view of a database's tree borrows the database: `EditionedFileId::parse(db)` is
`returns(ref)`, so `file_id.parse(db).root()` is a `Node<'db>`. For any `HirFileId`,
`HirFileId::root(db: &Db)` and `AstId::to_view(db)` (requested of db) give the same.

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

## Extension layer

`crates/syntax/src/view/{node_ext,expr_ext,token_ext,prec}.rs` port `ast/{node_ext,expr_ext,
token_ext,prec}.rs` onto views: every public item of the handle version, same name and meaning,
so a caller switches with `use syntax::view as ast;` and lifetimes.

- **Shared, not copied.** Types with no handle in them are the handle layer's own:
  `AttrKind`, `SelfParamKind`, `CommentKind`, `CommentShape`, `QuoteOffsets`, `Radix`,
  `prec::{ExprPrecedence, Fixity}` and the operator enums. A type that holds a handle has a
  `Copy` view twin `X<'t>`: `Macro`, `NameLike`, `NameOrNameRef`, `PathSegmentKind`,
  `StructKind`, `FieldKind`, `TypeBoundKind`, `TypeOrConstParam`, `VisibilityKind`,
  `CfgAtomKey`, `TokenTreeChildren`, `SlicePatComponents`, `ElseBranch`, `ArrayExprKind`,
  `LiteralKind`, `BlockModifier`, `CallableExpr`, `AnyComment`, `AnyString`, `IsString<'t>`.
- **Text is lent.** `Meta`/`Attr::{as_simple_atom, simple_name}` give `&'t str` and
  `as_simple_call` `(&'t str, TokenTree<'t>)` where the handles built a `SmolStr`; every `text`
  is `&'t str`. Literal values (`String`/`ByteString`/`CString`/`AnyString::value`) stay `Cow`,
  borrowed unless unescaping or a CRLF changes the value; `value_string` stays `String`.
- **No allocation in a walk.** `Meta::skip_cfg_attrs` (a `SmallVec` built by recursion) is a
  preorder walk that enters each `cfg_attr` and gives the metas it meets, the same metas in the
  same order. `SlicePatComponents` holds `Take`/`Skip` of the patterns instead of two `Vec`s:
  the prefix length is counted once, nothing is gathered. `IntNumber::value` reads the digits in
  place (skipping `_`, the same leading `+`, the first bad digit or overflow failing with the
  `ParseIntError` kind `from_str_radix` gives) instead of `replace('_', "")`. The remaining
  allocations carry `// alloc:`: the CRLF rebuild of a literal, the unescape buffer once an
  escape changes the value, `value_string`.
- **Signatures.** Methods take `self`; functions that took `&ast::NameRef`/`&ast::Name`
  (`Impl::for_trait_name_ref`, `RecordExprField::for_field_name`/`for_name_ref`,
  `RecordPatField::for_field_name_ref`/`for_field_name`) take the view by value, and
  `NameLike::as_name_ref`/`as_lifetime` give it by value. `IsString::escaped_char_ranges` takes
  `impl FnMut` instead of `&mut dyn FnMut` (a caller's `&mut |..| ..` still fits). Iterators are
  `impl Iterator + use<'t>`. `prec::precedence` takes the `Expr<'t>` by value and
  `needs_parens_in{,_place_of}` take `Node<'t>`s.
- **No `unreachable!()`.** `AnyComment::doc_kind` gives `None` for `COMMENT` and the doc kind
  for the two doc kinds, and panics naming the kind and range on any other, where the handle
  has `unreachable!()`. `TypeBound::kind` and `Literal::kind`, which the handles end in
  `unreachable!()` on a tree the parser never makes, panic naming the node's range and what it
  holds.
- **Left out** (builds or edits a tree): `UseTreeList::remove_unnecessary_braces` (takes a
  `SyntaxEditor`). No caller on the build path: `rg remove_unnecessary_braces` over
  `crates/hir*`, `mbe`, `syntax-bridge`, `blaze-*`, `span`, `cfg`, `const-ast` finds none.

Proof: `crates/syntax/src/view/tests.rs` parses one source rich in every construct the layer
reads (attributes and `cfg_attr` nests, doc and plain comments, every visibility, paths with
qualifiers and anchors, use trees, items of every kind, every operator, range and slice
patterns, every literal kind with bad and overflowing numbers, CRLF strings, an outline), pairs
each node or token of each kind with its handle twin by kind and range, and asserts every ported
method gives the same node, token, text or value; `digits_value` is checked against
`u128::from_str_radix` on every string of four bytes from `0 1 7 9 a f z _ + - é`, radixes 2,
8, 10, 16, error kinds included, and the 2^128 edges.
