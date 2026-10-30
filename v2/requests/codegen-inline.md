# Requests from `codegen-inline`

rustc's MIR `Inline` pass, in `crates/hir-ty-mir/src/mir/optimize/inline.rs` and
`inline/{cost,cycle,integrate,spans,tests,fixtures}.rs`. It is written against the types of 1
and 2; until they land it does not build. The registration in `optimize.rs` and the test
module line are codegen-mir's (`~/restart-2026-10-28/briefs/v2/codegen-inline.requests.md`).
Report: `/tmp/blaze-lanes/v2-codegen-inline.md`.

## 1. hir-ty `src/mir.rs` (db-ty): what an optimized body inlined

Why: an inlined callee's statements keep spans of the callee's body, `#[track_caller]` and
debug info need the call they were inlined at, and the code generator keys a caller's code
on the code of each body inlined into it.

- Beside `MirBody`:

  ```rust
  /// A body whose code an optimized body has in it: rustc's `Instance` of an inlined callee.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
  pub enum InlinedBody<'db> {
      Fn(FunctionId),
      Closure(InternedClosureId<'db>),
  }

  /// One instance an optimized body inlined, listed once however often it was.
  #[derive(Debug, Clone, PartialEq, Eq, Hash, SalsaValue)]
  pub struct InlinedCallee<'db> {
      pub body: InlinedBody<'db>,
      /// With regions erased, in the caller's generic parameters (instantiated by
      /// monomorphization like the rest of the body).
      pub args: StoredGenericArgs,
      /// The callee's arithmetic is checked: its crate checks overflow, or it is
      /// `#[rustc_inherit_overflow_checks]` and the caller's crate does.
      pub written_checked: bool,
  }

  /// rustc's inlined `SourceScope`: the code of `callees[callee]`, inlined at `call_site`
  /// (a span of the body, itself `Inlined` when the call was in an inlined body).
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
  pub struct InlinedScope { pub callee: u32, pub call_site: MirSpan }

  /// What `MirSpan::Inlined(n)` is, `n` indexing `Inlined::spans`: `span` of the body of
  /// the callee of `scopes[scope]`.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
  pub struct InlinedSpan { pub scope: u32, pub span: MirSpan }

  /// The local an inlined callee's binding is in (its debug info).
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
  pub struct InlinedBinding { pub scope: u32, pub binding: BindingId, pub local: LocalId }

  /// `promoted_locals` of an inlined callee: `expr` is of the expression store of
  /// `callees[callee]`, evaluated with its `args`.
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
  pub struct InlinedPromoted { pub local: LocalId, pub callee: u32, pub expr: ExprId }

  #[derive(Debug, Clone, Default, PartialEq, Eq, Hash, SalsaValue)]
  pub struct Inlined<'db> {
      pub callees: Vec<InlinedCallee<'db>>,
      pub scopes: Vec<InlinedScope>,
      pub spans: Vec<InlinedSpan>,
      pub bindings: Vec<InlinedBinding>,
      pub promoted: Vec<InlinedPromoted>,
  }
  ```

- `MirBody` gains `pub inlined: Inlined<'db>`, empty where a body is built (lowering, shims,
  `Default` of anything that builds one), hashed in `impl Hash for MirBody` (the destructuring
  there lists every field) and its `Vec`s shrunk in `shrink_to_fit`.
- `MirSpan` gains `Inlined(u32)`. `MirSpan::end` keeps it as it is.
- `MirOptions` (codegen-mir's request 1) gains `pub inline: bool`: rustc's `Inline::is_enabled`,
  which the level alone does not give (rustc inlines at mir-opt-level 2 only at `-Copt-level`
  2 or 3, not incremental). `MirOptions::at` sets it `false`: rustc's CTFE MIR is not inlined,
  and the interpreter does not read `Inlined`.

> forwarded to db (db-ty; window `db-next2`, told by tmux).

## 2. hir-def `src/attrs.rs` (db): the attributes the inliner reads

Why: rustc's `is_inline_valid_on_fn` and `check_codegen_attributes` refuse on each. Five new
`AttrFlags` bits (73 to 77 are free), set in the single-word match by line 336 and given the
`Word`s they need in `intern`'s symbols:

- `RUSTC_NO_MIR_INLINE`: `#[rustc_no_mir_inline]`.
- `COLD`: `#[cold]`.
- `OPTIMIZE_NONE`: `#[optimize(none)]` (rustc's `OptimizeAttr::DoNotOptimize`).
- `HAS_INSTRUCTION_SET`: `#[instruction_set(..)]`, any argument.
- `HAS_SANITIZE`: `#[sanitize(..)]` or `#[no_sanitize(..)]`, any argument.

The inliner refuses a callee with either of the last two, or a caller with `HAS_SANITIZE`,
where rustc compares the sets: blaze reads neither attribute's arguments yet.

> forwarded to db (window `db-next2`, told by tmux).

## 3. Exhaustive `MirSpan` matches (each crate's owner)

`MirSpan::Inlined(n)` is a span of another body: where a match maps a span to source, it goes
through `hir_ty_mir::mir::source_span(db, body, span)`, which gives the owner and the span in
it; where it only compares or hashes, `Inlined(n)` is its own value. Arms at:
`hir/src/diagnostics.rs:1259,1404`, `blaze-items/src/form.rs:343`,
`hir-ty-codegen/mod.rs:4407`, `hir-ty-codegen/places.rs:165`, `hir-ty-codegen/v2/hashes.rs:173`,
`hir-ty-codegen/v2/backend.rs:397`, `hir-ty-ub/src/check/report.rs:148`,
`hir-ide/src/mir_pretty/errors.rs:244`, `hir-ty-mir/src/mir/location.rs:347`,
`hir-ty-mir/src/mir/eval.rs:677`, `hir-ty-mir/src/mir/lower.rs:3315`,
`blaze-diagnostics/src/lints/bodies.rs:237`, `hir-ty-borrowck/src/check.rs:192`,
`hir-ty-borrowck/src/regions.rs:4174`. `v2/hashes.rs` hashes `body.inlined` whole with the
body (the scopes, spans, bindings and promoted are what the inlined code means).

> forwarded: `hir/`, `hir-ty-borrowck` to db; `hir-ty-codegen`, `hir-ty-mir` to codegen; `blaze-diagnostics` to diagnostics; `hir-ty-ub` to safety (each told by tmux). `hir-ide/src/mir_pretty/errors.rs:244` and `blaze-items/src/form.rs:343` (items is DONE) are requests': their arms go in once 1 adds `MirSpan::Inlined`, which is not in `hir-ty/src/mir.rs` yet.

## 4. hir-ty-codegen (codegen, codegen-adapter)

- **Turn it on.** `v2/build.rs` `Build::options` and `mod.rs` `mir_options_of`: `inline: level ==
  MirOptLevel::Release` (the library's crates are always level 2 and inlined, as shipped).
  `inline.rs`'s `callee_options` gives a callee of another crate the options the code
  generator asks of it (same crate: the caller's; a library crate: level 2, no UB checks,
  inlined; else UB checks as its `cfg(debug_assertions)`): keep the two in step.
- **Spans.** Where codegen maps a `MirSpan` to a file and line (debug info, `Location`,
  panics), use `source_span(db, body, span)`; a span of an inlined body is in its owner's file.
  rustc gives an inlined scope its own `DILocation` with `inlinedAt` the call site:
  `body.inlined.scopes[..].call_site` is that.
- **`#[track_caller]`.** `mod.rs:13312` `caller_location`: the location a span reports is
  `caller_location(db, body, span, track_caller(db, func))`: `At(owner, span)` is a constant
  location, `Argument` the function's caller-location argument. An inlined
  `#[track_caller]` callee reports its call site, as rustc's `caller_location_span`.
- **Overflow checks.** `mod.rs:13035` `body_written_checked(db, body)`: per span, through
  `written_checked_at(db, body, span)`; an inlined callee's arithmetic is checked as its own
  crate says (`InlinedCallee::written_checked`).
- **Promoted constants.** Every reading of `body.promoted_locals` (`mod.rs:10329`, 19513,
  19542, 19622, 19720, 19730, 19811, 19818, `v2/refs.rs:866`) also covers
  `body.inlined.promoted`: the constant is `expr` of the expression store of
  `callees[callee].body` (`owner_of(db, ..)`), evaluated with `callees[callee].args`.
- **The inlined list** (`CompiledFunction::inlined`, `Described::built_in`). In the refs walk
  (`v2/refs.rs`), for each of `body.inlined.callees` (already monomorphized with the body):
  the instance `InstanceDef::Fn(func)` or `InstanceDef::Closure(closure)` with `args` is listed
  as a ref (`places::call_ref`) and its place pushed to `built_in`, as `instance_call` does
  for an always-inlinable callee, without `declare_inlined` (its code is in the body already).
  `frontend.rs` `inlined()` then hashes `code_of` of each, transitively: the caller's code is
  stale when an inlined callee's source moves (spans, `Location` lines) or changes. A callee
  listed once already as called (`listed_call` gives `None`) still needs its place in
  `built_in`: keep the place of each `(def, args)` in `called` so it can be found.

> forwarded to codegen (window `codegen-next1`, told by tmux).
