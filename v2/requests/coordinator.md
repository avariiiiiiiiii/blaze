
## Workspace: blaze-alloc-count and clippy's allocation lints

- What:
  - add `crates/blaze-alloc-count` as a workspace dependency
    (`blaze-alloc-count = { path = "crates/blaze-alloc-count" }`);
  - in `[workspace.lints.clippy]`, set to `"deny"`: `redundant_clone`, `clone_on_ref_ptr`,
    `implicit_clone`, `unnecessary_to_owned`, `inefficient_to_string`, `format_collect`,
    `needless_pass_by_value`, `large_types_passed_by_value`, `box_collection`, `vec_box`,
    `rc_buffer`, `large_enum_variant`.
- Where: root `Cargo.toml`.
- Why: `docs/v2/plan/goals/principles.md`, "Allocation, `dyn` and clones are controlled
  mechanically".

## tools/v2/hot_lint.py crashes when given `crates` (from db-ty-port-infer-relate)

- What: `{"root": ".", "crates": ["hir-ty"]}` fails with `typed_input: crates has a type it does
  not read: list[str]`. `python/typed-input` does not read `list[str]` fields, so either it
  learns lists or `Input.crates` becomes a type it does read (a `tuple[str, ...]`, if supported).
- Where: `tools/v2/hot_lint.py` (`Input`) or `python/typed-input/src/typed_input/__init__.py`.
- Why: the brief's command doesn't run. Without `crates` it works (all hot crates).
