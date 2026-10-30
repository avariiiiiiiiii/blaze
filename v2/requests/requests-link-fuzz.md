# From requests-link-fuzz (child of `requests`)

## 1. `tools/v2/hot_lint.py`: the `crates` field can't be read

What: in `tools/v2/hot_lint.py`, `Input.crates` is `list[str]`; make it
`tuple[str, ...] = HOT`.

Where: `tools/v2/hot_lint.py` line 39 (`crates: list[str] = field(default_factory=lambda: list(HOT))`).

Why: `typed_input._check` reads `tuple[T, ...]` but not `list[T]`, so the input the coordinator
gave every lane, `{"root": ".", "crates": ["blaze-link"]}`, fails with
`TypeError: typed_input: crates has a type it does not read: list[str]`. Only `{"root": "."}`
runs (every hot crate at once).

## 2. `tools/v2/hot_lint.py`: code compiled only for tests or fuzzing counts as hot

What: treat an item under `#[cfg(any(test, feature = "fuzz"))]` or `#[cfg(feature = "fuzz")]`
as test code, the way `#[cfg(test)]` is treated. Also treat a file as test code when the
`mod` that declares it carries one of those cfgs, as in `crates/blaze-link/src/macho/tbd.rs`'s
`#[cfg(any(test, feature = "fuzz"))] mod reference;`.

Where: `in_test_module` and `lint_file` in `tools/v2/hot_lint.py`.

Why: the reference models the fuzz targets compare against (link.md item 13) are no normal
build's code. They are the old, allocating readers on purpose. Today they make 77 findings in
`crates/blaze-link/src/macho/tbd/reference.rs` and about 20 in the fuzz-only parts of
`macho/dylib.rs` and `macho/tbd.rs`. Marking each line `// alloc:` would mean about 100 comments
that all say "a test model".
