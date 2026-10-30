# dedyn: requests to the coordinator

## 1. `tools/v2/hot_lint.py`: the `crates` filter crashes, and hir-ty-codegen is never linted

- `Input.crates: list[str]` makes `typed_input` raise ("has a type it does not read") on any
  input that names `crates`. Change it to `crates: tuple[str, ...] = HOT`.
- `hir-ty-codegen` keeps its sources at the crate root (`cached.rs`, `mod.rs`, `v2/`, ...), not
  under `src/`, so `if not src.is_dir(): continue` skips it silently; it has 125 `dyn [A-Z]`
  lines. When `src/` is missing, walk `root / "crates" / crate` instead (still skipping `tests`
  and `*tests.rs`).
- Add `"blaze-diagnostics"` to `HOT`: its late passes run per body.
- The `dyn` pattern `\bdyn\s+[A-Z]` misses path-qualified traits (`&dyn ast::HasAttrs`,
  `&dyn cranelift_codegen::isa::TargetIsa`, `&mut dyn FnMut(&dyn radix::Workers)`): about 25
  such lines in the hot crates. Make it `\bdyn\s+(::)?([a-z_][a-z0-9_]*::)*[A-Z]`.
- A module declared `#[cfg(test)] mod reference;` in its parent is test code, but the lint only
  skips `#[cfg(test)]` blocks inside a file, so it flags every line of
  `hir-expand/src/builtin/derive_macro/reference.rs` (15 `dyn`), `mbe/src/differential.rs`,
  `blaze-core/src/wheel/reference.rs` and the like. Before linting, collect the paths of modules
  declared under `#[cfg(test)]` (the `mod x;` line after the attribute: `x.rs` and `x/` beside
  the declaring file, or `<parent dir>/<file stem>/x.rs` for a non-`mod.rs` parent) and skip them.

> applied by the coordinator (checked: `tools/v2/hot_lint.py` has `crates: tuple[str, ...] = HOT`, walks the crate root when `src/` is missing, and `HOT` names `blaze-diagnostics`) (requests, pass 18).
