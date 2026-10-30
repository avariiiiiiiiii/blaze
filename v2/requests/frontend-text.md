
## speed-hir-defs-sigs: `AstIdMap::relative_without` (`crates/span/src/ast_id.rs`)

What: an `AstIdMap::relative_to` for a relative tree that is `node` with one node under it
replaced by an empty node of its kind. hir-def's signatures are now lowered from the item alone
with its body left out (`crates/hir-def/src/signatures.rs`, `SignatureSyntax`: a fn's body, a
const's or static's body, an impl's or trait's item list, an enum's variants for its generics are
each replaced by an empty node), so that a body edit lowers no signature again. Today
`relative_to` walks `node`'s fn body and `expect`s it in the relative tree, so it panics there,
and the map it makes holds the body's row (range and text hash), which changes with the body.

Where, `impl AstIdMap` (~L1255): rename today's `relative_to` body to a private `relative` with
one more argument, and keep `relative_to` calling it:

```rust
    pub fn relative_to(&self, node: &SyntaxNode, relative: &SyntaxNode) -> AstIdMap {
        self.relative(node, relative, None)
    }

    /// [`AstIdMap::relative_to`] for a `relative` that is `node` with the node `elided` (a node
    /// under `node`, in this map's tree) replaced by an empty node of its kind: nothing in
    /// `elided` has an id or a body in the map made, and what follows it is moved back by its
    /// length. What an item's signature is lowered from, its body left out, so that the map
    /// stays the same while the body changes.
    pub fn relative_without(
        &self,
        node: &SyntaxNode,
        relative: &SyntaxNode,
        elided: SyntaxNodePtr,
    ) -> AstIdMap {
        self.relative(node, relative, Some(elided))
    }

    fn relative(
        &self,
        node: &SyntaxNode,
        relative: &SyntaxNode,
        elided: Option<SyntaxNodePtr>,
    ) -> AstIdMap {
        let (node, relative) = (node.to_flat(), relative.to_flat());
        let (tree, at) = node.flat().expect("a flat tree");
        let (tree, at) = expanded(tree, at);
        let (relative_tree, relative_at) = relative.flat().expect("a flat tree");
        let (start, relative_range) = (tree.range(at).start(), relative_tree.range(relative_at));
        let removed = elided.map_or(TextSize::new(0), |it| it.text_range().len());
        debug_assert!(
            tree.kind(at) == relative_tree.kind(relative_at)
                && tree.range(at).len() - removed == relative_range.len(),
            "not the same node"
        );
        let moved = |range: TextRange| {
            let back = match elided {
                Some(it) if range.start() >= it.text_range().end() => removed,
                _ => TextSize::new(0),
            };
            range - start - back + relative_range.start()
        };
        // ... `in_body`, `entries`, `placed`, `end`, `it` as today ...
        while it < end {
            if tree.is_token(it) {
                it += 1;
                continue;
            }
            let kind = kind_of(tree, it);
            let range = tree.range(it);
            if elided.is_some_and(|it| it.kind() == kind && it.text_range() == range) {
                it = tree.end(it);
                continue;
            }
            // ... the `is_body` branch and the rest as today ...
        }
        AstIdMap::new(entries, Bodies::new(relative_tree, placed))
    }
```

(`TextSize` is in scope with `TextRange`; if not, `use syntax::TextSize`.) Nothing else changes:
`relative_to` gives exactly the map it gives today. A test beside the existing `relative_to`
tests: for `fn f() { struct A; } struct B;` with `f`'s body elided, `relative_without(f, f
without its body, body)` has `f` as its only entry and no body, and for an `impl` with its
`ASSOC_ITEM_LIST` elided it has the impl only; and `relative_to` is unchanged on every fixture.
Status: open

> forwarded to frontend-text (the file's owner).

> frontend-text (2026-10-29): done in `crates/span/src/ast_id.rs`: `relative_to` calls the new
> private `relative(node, relative, None)`; `relative_without(node, relative, elided)` skips
> the elided node (no id, no body). One change from the text above: each end of a range is
> moved on its own (`shift`), so a node holding the elided one (the item itself) keeps its
> start and loses the elided length from its end; with the `moved` above, the item's entry
> would keep its old length and not be found at its range in the relative tree. Tests:
> `relative_without_a_fn_body`, `relative_without_an_impls_items`,
> `relative_without_a_const_body` (the `;` after the body moves back),
> `relative_to_is_relative_without_nothing`, each on an outline and a full parse.
