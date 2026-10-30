# Requests of diagnostics-lints-bodies-liveness

What `lints/bodies/liveness.rs` (rustc's `check_liveness`: `unused_variables`,
`unused_assignments`) needs from other lanes. The lints are correct without 1 and 2; 3 and 4 are
cases they miss today.

## 1. hir-ty-mir: mark the guard references in `MirBody`

What: `MirBody::guard_locals: ArenaMap<LocalId, BindingId>` (or a `LocalInfo` per local), set
where `lower.rs` takes `tmp = &place` for a by-value binding of a guarded arm (rustc's
`BindingForm::RefForGuard(arm_local)`). Why: liveness merges the guard's reads into the
binding's. It now recognises the reference by its shape: an unprojected `Ref` assigned at the
binding's `PatId` to a local that is no binding's. That holds for what the lowering emits today,
but a later change to it would break it without anything failing loudly.

## 2. hir-ty-mir: mark the capture copies at a closure's start

What: on `MirBody`, the statements (or the locals) that copy a capture out of the closure's own
local at its start (`cap = move _1.f` / `(*_1).f`, at `MirSpan::Unknown`), with the field.
Why: rustc has no such copies, so liveness must treat them as neither a use nor an assignment.
It now finds them as an `Assign` at `MirSpan::Unknown` of an `upvar_locals` local from the
closure's own local. `upvar_locals` already maps the capture to its local.

## 3. the parent (`lints/bodies.rs`) or hir-ty-mir: the MIR of an inline const

What: liveness run on each `const { .. }` block (rustc's `check_liveness` runs on every body
owner, inline consts included), either through a `BodyCx` of its own or through a MIR query for
it. Why: `BodyCx::exprs` leaves inline consts out and no MIR of one is reachable from the
parent's MIR, so `let x = 1;` in a `const { }` gets no `unused_variables`.

## 4. hir-ty-mir: a closure or coroutine whose MIR does not lower

What: nothing to change in liveness; listed so the cases are known. When
`mir_body_for_closure`/`mir_body_for_coroutine` fails for a body that is not tainted, the
closure gets no lints and the parent reads every capture as live. When `mir_body` of the body
itself fails, the body gets no lints. Each such case is a lowering gap of hir-ty-mir (its
`not_supported!`), and is reported there.
