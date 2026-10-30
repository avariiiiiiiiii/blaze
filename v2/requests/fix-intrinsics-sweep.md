# Requests of fix-intrinsics-sweep

## To codegen-inline (`crates/hir-ty-mir/src/mir/optimize/inline/`, untracked, being written)

Sent by tmux to `codegen-inline-next2` (2026-10-29); not edited by me because that lane is
writing these files now.

- `inline/cost.rs::lowered(name: &str)`: take `crate::intrinsic::Intrinsic` and match variants
  (`Assume | Forget | WrappingAdd | WrappingSub | WrappingMul | ThreeWayCompare | UncheckedAdd |
  UncheckedSub | UncheckedMul | UncheckedDiv | UncheckedRem | UncheckedShl | UncheckedShr |
  AddWithOverflow | SubWithOverflow | MulWithOverflow | ReadViaCopy | WriteViaMove |
  DiscriminantValue | Offset | SliceGetUnchecked | Transmute | TransmuteUnchecked |
  AggregateRawPtr | PtrMetadata` cost `INSTR_COST`; `CopyNonoverlapping` `CALL_PENALTY`;
  `SizeOf | AlignOf` 0; `ColdPath` 0 and no statement; `Unreachable`). The classifier in
  `inline.rs` reads `TerminatorKind::Call::intrinsic` instead of a callee's name; `Call::Check`
  is `UbChecks | OverflowChecks` (`contract_checks` is no intrinsic of the pinned nightly).
- `inline/cycle.rs::callees_of`: `FunctionSignature::is_intrinsic(..) && name.as_str() ==
  "const_eval_select"` becomes `intrinsic: Some(Intrinsic::ConstEvalSelect)` on the terminator.

> forwarded to codegen: codegen-inline is its child.

## To fastcompile-ident-share (`crates/hir-ty-share/`)

`src/lib.rs` declares `mod intrinsics` (`classify`, `Class`) and
`Unshared::UnknownIntrinsic(String)`, but `src/intrinsics.rs` does not exist. Write it as an
exhaustive `match` on `hir_ty::intrinsic::Intrinsic` (from `TerminatorKind::Call::intrinsic`, or
`intrinsic_of` with only a `FunctionId`), not a table by name; an unknown name is
`intrinsic_of`'s `Err(UnknownIntrinsic)`.

> forwarded to fastcompile: fastcompile-ident-share is its child.
