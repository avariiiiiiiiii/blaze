# syntax-tree

blaze's syntax trees. It started as [rowan 0.17.0](https://github.com/rust-analyzer/rowan) and
has a second, flat kind of tree behind its cursor (`src/flat.rs`, docs/flat-syntax.md), which is
what every parse makes; green trees remain for editing (`clone_for_update`, the syntax editor).

A flat tree is every node and token in preorder, packed: one `u32` an element (its kind, whether
it is a node, token or lazy node, and a token's length or a node's subtree size, with values too
large for it in a small side table), a parent per element and the start of every eighth element,
about 8.5 bytes an element. Its text is not copied: the tree shares the file's source
(`FlatTree::source`, `FlatTreeBuilder::sourced`).

A flat tree may have lazy nodes (`FlatTreeBuilder::lazy`): a node whose text is not parsed, as
an outline parse leaves the body of every `fn`. The arrays show it without children, so walks of
the arrays never parse; the cursor is transparent over it, parsing its part once
(`FlatTree::part`, thread-safe) when it moves into it, so every reader through `SyntaxNode` sees
the full tree. `FlatTree::replace_subtree` splices a reparsed item into a tree.

A library for lossless syntax trees, inspired in part by
Swift's [libsyntax](https://github.com/apple/swift/tree/5e2c815edfd758f9b1309ce07bfc01c4bc20ec23/lib/Syntax).

A conceptual overview is available in the [rust-analyzer repo](https://github.com/rust-analyzer/rust-analyzer/blob/master/docs/dev/syntax.md).

See `examples/s_expressions` for a tutorial, and [rust-analyzer](https://github.com/rust-analyzer/rust-analyzer/) for real-world usage.

## Testing

This crate is primarily tested by various integration tests in rust-analyzer. `src/flat.rs`'s
tests compare flat trees with green ones (random trees, and trees past what an element's word
holds), outlines with the same trees in full through every move of the cursor, parsing a part
once under threads, and `replace_subtree` with building the edited tree directly.

## License

It is distributed under the terms of both the MIT
license and the Apache License (Version 2.0).

See LICENSE-APACHE and LICENSE-MIT for details.
