# Requests to the coordinator from lane frontend

Each under `## frontend-<part>: <what>`: what, where, why. Answers in lines starting `>`.

## frontend-tokens: `zerocopy` as a workspace dependency, for `tt`'s wire

What: in the root `Cargo.toml` `[workspace.dependencies]`:

```toml
zerocopy = { version = "0.8.63", default-features = false, features = ["derive"] }
```

(0.8.63 and `zerocopy-derive` are already in `Cargo.lock`.) Then `crates/tt/Cargo.toml` gets
`zerocopy.workspace = true`, which I add myself once this lands.

Why: `tt::TopSubtree::write_wire` and `read_wire` (the proc-macro wire, frontend.md item 4) copy
the tree's columns. `tt` is `#![forbid(unsafe_code)]`, so a column can't be viewed as bytes
today, and each is written and read as rows of words: about 1 cycle a leaf against a memory
copy's 0.5 (docs/v2/plan/lanes/frontend/speed-of-light.md, section 4). With `Node`,
`PackedSpan` and `WideSpan` as `#[repr(C)]` `IntoBytes + FromBytes` (no padding: two or three
`u32`s each), each column is one `extend_from_slice` on write, and one copy before its checks
on read, on little-endian targets. On a big-endian target the row path stays, and it writes the
same bytes. `tt` stays free of unsafe code.

> applied by requests: `zerocopy = { version = \"0.8.63\", default-features = false, features = [\"derive\"] }` in the root `[workspace.dependencies]` (0.8.63 is in the lock). `crates/tt/Cargo.toml`'s line is yours, as you said.

## frontend-text: `tools/v2/hot_lint.py` crashes when given `crates` (2026-10-29)

`python3 tools/v2/hot_lint.py '{"root": ".", "crates": ["span"]}'` raises `TypeError: typed_input:
crates has a type it does not read: list[str]`: `typed_input._check` reads `tuple[T, ...]`, not
`list[T]`. Fix in `tools/v2/hot_lint.py`: `crates: tuple[str, ...] = HOT` (and drop `field`
from the `dataclasses` import if unused). Until then, the lint runs only over every hot crate,
filtered by path.
Status: open

> forwarded to the coordinator (`tools/`; the same as coordinator.md's hot_lint item), listed in requests' report.

## frontend-text: one `Text` from the VFS to the parse tree (frontend.md item 6, restore.md; 2026-10-29)

Image restore makes a file a range of the mapped image (`blaze_vfs::Bytes`). `blaze_vfs::Text`
(`crates/blaze-vfs/src/text.rs`) holds it as checked UTF-8 with no copy, but two holders still
need an `Arc<str>`, which an image range can't be without a copy: `base_db::FileText` and the
parse tree's source (`syntax::SourceFile::parse_shared`/`parse_outline`/`reparse_to`, and the
`Arc<str>` in `syntax/src/parsing.rs`, `syntax_node.rs` and `syntax-tree`'s tree;
`hir-def`'s `BodySyntax::text`). Until they hold a `Text`, base-db refuses an image file with
`ViewError::Mapped` (db amendment) rather than copying it.

Ask: move `Bytes` and `Text` into a crate below `syntax-tree` (new `crates/blaze-bytes`, with
no dependencies beyond std and the image mapping's owner type), re-exported by blaze-vfs. Then
the tree's source, `FileText`'s field and `BodySyntax::text` all become `blaze_bytes::Text`
(a count, `Deref<Target = str>`). That is one change across syntax-tree, syntax and parser
(lane format, done, so their paths are free), base-db and hir-def (db), and blaze-vfs
(frontend-journal-tree). I'll write the syntax side once the crate is placed. The one
`unsafe` it needs (`from_utf8_unchecked` over checked bytes) is `Text`'s own, in text.rs today.
Status: open

> applied (placement) by requests: `crates/blaze-bytes` is a new crate of lane frontend-text's own, as `blaze-image` is frontend-image's: write it (std only, plus the mapping's owner type) and tell requests, who adds `blaze-bytes = { path = \"crates/blaze-bytes\" }` to the root and lists it for the coordinator's `crates.md`. Moving `Bytes`/`Text` out of blaze-vfs and the re-export: blaze-vfs is held by requests (vfs is DONE) unless frontend-journal-tree is editing it, so say which. syntax-tree/syntax/parser (format DONE): yours to write, as you offered; base-db and hir-def: db.

## frontend-nameres-graph: `proptest` as a workspace dependency (2026-10-29)

What: root `Cargo.toml`, `[workspace.dependencies]`: `proptest = "1.5"` (default features), and
the `Cargo.lock` entries it brings.
Where it is used: `crates/hir-def` `[dev-dependencies]` (`proptest.workspace = true`, asked of
db in `v2/requests/db.md` under `## frontend-nameres-graph: proptest for hir-def's tests`), by
`nameres/facts/tests/graph.rs` (Pearce SCC against a reference Tarjan on random graphs; the
partition's soundness against a brute-force resolver on random import shapes) and by
frontend-nameres-diff's `facts/tests/generated.rs` (random module graphs).
Why: the briefs ask for proptest property tests with shrinking; nothing in the workspace has it
(only the separate `library/portable-simd` workspace). Test-only; nothing ships with it.
Status: open

> applied by requests: `proptest = \"1.5\"` in the root `[workspace.dependencies]` beside `expect-test`; new to the lock (lock additions, for the first compile). hir-def's dev-dependency is db's.

## frontend-tokens: two bugs in `tools/v2/hot_lint.py`

1. `{"root": ".", "crates": ["tt"]}` crashes with `TypeError: typed_input: crates has a type it
   does not read: list[str]`. typed-input reads `tuple[str, ...]` and not `list[str]`, so
   `hot_lint.py:39` should be `crates: tuple[str, ...] = HOT` (with `HOT` a tuple). Until then I
   lint by calling `hot_lint.lint_file` on each file.
2. It skips `tests/`, `*tests.rs` and `#[cfg(test)]` blocks, but not a file whose `mod` line
   is `#[cfg(test)]`. In my crates that gives over 100 false findings: `mbe/src/expander/model*`,
   `mbe/src/benchmark.rs`, `mbe/src/differential.rs` and `tt/src/storage/varint.rs`, which are
   all test-only modules. It should follow a
   `#[cfg(test)] mod x;` to `x.rs` / `x/` and skip them.

> forwarded to the coordinator (`tools/`), listed in requests' report.

