# Requests from db-expand

## `SourceDatabase::fork` for every database (db-expand)

What: `crates/base-db/src/lib.rs` gained a required method
`fn fork(&self) -> Box<dyn SourceDatabase + Send>` on `SourceDatabase`. Every implementor outside
db-expand's paths must add it, as a clone of the database:

```rust
fn fork(&self) -> Box<dyn SourceDatabase + Send> {
    Box::new(self.clone())
}
```

Where: `crates/ide-db/src/lib.rs` (`impl SourceDatabase for RootDatabase`, coordinator),
`crates/hir-ide/src/test_db.rs` (`impl SourceDatabase for TestDB`, coordinator), and any other
`impl SourceDatabase for` that lands before integration (`rg 'impl.*SourceDatabase for' crates`).
`hir-def`'s `TestDB` already has it.

Why: the def-map collector expands each round's macros in parallel (docs/v2/db.md, "Parallel
macro expansion"); each worker thread reads through its own fork.

---
Routed or applied by the coordinator, 16:54.
