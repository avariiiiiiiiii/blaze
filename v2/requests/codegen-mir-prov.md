# Requests of codegen-mir-prov

## 1. To codegen-mir: terminators read their scalars with `read_scalar` (`interpret_mir`)

S1 item 2 says the condition of an `Assert` and a `SwitchInt`'s discriminant are read with
`require_init`; both are in `interpret_mir`, which is codegen-mir's. Today they are
`self.eval_operand(cond, locals)?.get(self)?[0]` and
`self.eval_operand(discr, locals)?.get(self)?` (an untyped `read`, no `require_init`). Wanted:

```rust
let cond = self.eval_operand(cond, locals)?;
let holds = self.read_scalar(cond)?[0] != 0;
// ...
let discr = self.eval_operand(discr, locals)?;
let val = u128::from_le_bytes(pad16(self.read_scalar(discr)?, IsSigned::No));
```

`assert_message`'s operands likewise (`read_scalar`), though they are only read on the way to a
panic.

## 2. To codegen-mir: say in `machine.rs` what a `put` past the end leaves

`Shadow::put`'s doc says "growing them when it ends past them" but not what the bytes between
the old end and `at` are. `eval_rvalue`'s aggregates (`construct_shadowed`) rely on them being
uninitialized (padding, a union's other bytes), which `hir-ty-ub`'s `Bytes::put` does
(`memory/bytes.rs`: it materializes and zero-fills the init mask). Wanted in the doc:
"`other` put over these at `at`, growing them when it ends past them; bytes between their old
end and `at` are uninitialized."

## 3. To codegen-mir: `fill_locals_for_body` and call arguments take `IntervalOrOwned<M::Shadow>`

`eval_rvalue` now returns `IntervalOrOwned<M::Shadow>`, and `Owned` there means initialized
bytes without provenance. `interpret_mir`'s `args: impl Iterator<Item = IntervalOrOwned>` and
`fill_locals_for_body` are `IntervalOrOwned<()>`: an argument that is a computed pointer (a
shim's callback with a pointer it made) loses its provenance there. Where such a value is
built, it should be `IntervalOrOwned<M::Shadow>` (`pointer_value` / `Shadowed`) and
`fill_locals_for_body` should write it with `copy_from_interval_or_owned`.
