# syntax-tree

blaze's syntax trees. It started as [rowan 0.17.0](https://github.com/rust-analyzer/rowan) and
has a second, flat kind of tree behind its cursor (`src/flat.rs`, docs/flat-syntax.md).

A library for lossless syntax trees, inspired in part by
Swift's [libsyntax](https://github.com/apple/swift/tree/5e2c815edfd758f9b1309ce07bfc01c4bc20ec23/lib/Syntax).

A conceptual overview is available in the [rust-analyzer repo](https://github.com/rust-analyzer/rust-analyzer/blob/master/docs/dev/syntax.md).

See `examples/s_expressions` for a tutorial, and [rust-analyzer](https://github.com/rust-analyzer/rust-analyzer/) for real-world usage.

## Testing

This crate is primarily tested by various integration tests in rust-analyzer.

## License

It is distributed under the terms of both the MIT
license and the Apache License (Version 2.0).

See LICENSE-APACHE and LICENSE-MIT for details.
