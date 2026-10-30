
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
