# Requests of lane requests

Its children append below, each request saying which child it comes from.

## From requests-nodes (2026-10-29): `idle` and `PlaneOutput::Yield` in `machine.rs` and `build.rs` (fastcompile-intent 2)

`PlaneInput::Open` has a new field `idle: bool`, and `PlaneOutput` a new variant
`Yield { id }` (`crates/blaze-graph/src/plane.rs`). Two files of blaze-graph that I don't own
no longer compile without them:

- `crates/blaze-graph/src/machine.rs` `Machine::new` (the `PlaneInput::Open` at ~174): add
  `idle: false`. `Machine::step`'s match on the plane's outputs (~213): add
  `PlaneOutput::Yield { id } => unreachable!("node {id} asked to give way: the machine opens no idle request")`.
  A plane with no idle request never says `Yield`.
- `crates/blaze-graph/src/build.rs` (~208): `idle: false` in its `PlaneInput::Open`. The
  match at ~215 needs `PlaneOutput::Yield { .. } => unreachable!("no request is idle")`.

Nothing else changes for either: with no idle request the plane says what it said before.

> applied: `machine.rs` had both already; `build.rs` gains `idle: false` in its `PlaneInput::Open` and `PlaneOutput::Yield { .. } => unreachable!("no request is idle")`.
> applied: `machine.rs` `Machine::new` opens with `idle: false` and `step` says `unreachable!` on `Yield`; `build.rs` the same in its test runner.
