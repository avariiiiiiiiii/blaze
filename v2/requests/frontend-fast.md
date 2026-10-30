# Requests of lane frontend-fast

## frontend-bodies: fast.md 5, every body is checked; reachability orders, never skips (2026-10-29)

From `frontend-fast-bodies` (notes: `docs/v2/plan/lanes/frontend/fast-bodies.md`, section 2).

Why: rustc rejects a crate with a type or borrow error in a function nothing calls (`fn
unused() { let x: u8 = "a"; }` is E0308 under `cargo build`). Checking only reached bodies
would accept programs the nightly rejects, which the mandate forbids. Reachability can only
decide the order: reached bodies first, as they feed codegen, and the unit's outcome waits for
every body. The cases under `harness/v2/frontend/cases/bodies/` (`unreached_*.rs`, each
`//@ expect: reject E0xxx`) hold it.

`docs/v2/plan/lanes/frontend/fast.md`, section 5, before:

```markdown
- **Lower bound.** A build checks only the bodies reachable from what it builds (compile-time.md
  1 and 16), in parallel on the plane's pool up to the core count, and after an edit only the
  bodies whose own text or whose read signatures changed. A dependency from crates.io is checked
  once per toolchain for everyone (compile-time.md 8).
```

After:

```markdown
- **Lower bound.** A build checks every body of the crates it compiles (rustc rejects an error
  in a function nothing calls), in parallel on the plane's pool up to the core count, the bodies
  reachable from what it builds first (compile-time.md 1 and 16), as they feed codegen; after an
  edit only the bodies whose own text or whose read signatures changed. A dependency from
  crates.io is checked once per toolchain for everyone (compile-time.md 8).
```

and in its design item 2, before:

```markdown
  2. Bodies are checked as one plane task each, scheduled from the reachable set, not from a
     crate-wide loop.
```

After:

```markdown
  2. Bodies are checked as one plane task each, the reachable set first, not from a crate-wide
     loop; the unit's outcome waits for every body.
```
