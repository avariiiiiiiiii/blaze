# Requests from db-matchck

## 1. Root `Cargo.toml`: the `hir-ty-matchck` workspace dependency (db-matchck)

What: `hir-ty-matchck = { path = "crates/hir-ty-matchck", version = "0.0.0" }` in
`[workspace.dependencies]`, beside `hir-ty-borrowck`.
Where: the root `Cargo.toml`.
Why: already asked by lane db in `v2/requests/db.md` §9; repeated here so it is not missed.
`crates/hir/Cargo.toml` and the crate's own tests name it.

> applied once the crate has its `Cargo.toml`: the manifest lint refuses a workspace path with none, and `crates/hir-ty-matchck/` has none yet (db.md 9 waits the same way).
> applied: the root `Cargo.toml` has it (line 89), and `crates/hir/Cargo.toml` takes it.

## 2. Root `Cargo.toml`: `rustc_apfloat` as a workspace dependency (db-matchck)

What: `rustc_apfloat = "0.2.3"` in `[workspace.dependencies]` (the version `crates/hir-ty-mir`
already pins directly; switch `crates/hir-ty-mir/Cargo.toml` to `rustc_apfloat.workspace = true`
in the same change).
Where: the root `Cargo.toml`.
Why: float literal and range patterns are compared and printed through IEEE values, as rustc
does (`F32Range` / `F64Range` constructors of `rustc_pattern_analysis` take `rustc_apfloat`
values' bits; range validity uses `partial_cmp`).

> applied: `rustc_apfloat = "0.2.3"` is in the root `[workspace.dependencies]` already (line 174). `crates/hir-ty-mir/Cargo.toml` is codegen's: forwarded to codegen (told by tmux).
> applied: root `rustc_apfloat = "0.2.3"` beside the other rustc crates; `crates/hir-ide/Cargo.toml` takes it with `workspace = true`. > forwarded to codegen: the `crates/hir-ty-mir/Cargo.toml` switch (blaze-diagnostics, hir-ty, hir-def and syntax may switch too).


## dedup: stdx::edit_distance

From `dedup-scan` (lane dedup; report `/tmp/blaze-lanes/v2-dedup-scan.md`). `crates/hir-ty-matchck/src/distance.rs`
is one of six copies of rustc's `edit_distance` and three of `find_best_match_for_name`; the one
copy is `stdx::edit_distance` (`v2/requests/coordinator.md`, `## dedup: one stdx::edit_distance`;
this file's two tests are in its tests, `distances_are_rustcs` verbatim and
`best_match_prefers_case_then_distance_then_words` over a name function). `stdx` is already a
dependency. rustc's `find_best_match_for_name` with `dist: None` is what this file computes
(the default third-of-length limit, no substring score). Apply once that lands:

- Delete `src/distance.rs` and `mod distance;` in `src/lib.rs` (line 17).
- `src/typo.rs` line 15: `use crate::{PatternTypo, cx::Cx, distance::find_best_match_for_name};`
  becomes `use crate::{PatternTypo, cx::Cx};` and `use stdx::edit_distance::find_best_match_for_name_by;`
- `src/typo.rs` line 46: `find_best_match_for_name(&found.accessible, name)` becomes
  `find_best_match_for_name_by(&found.accessible, Name::as_str, name.as_str(), None)`;
  line 56: `find_best_match_for_name(&found.imported, name)` becomes
  `find_best_match_for_name_by(&found.imported, Name::as_str, name.as_str(), None)`.
  (Both return `Option<&Name>` as before; `Name` is already imported there.)

Status: open

## 3. diagnostics (blaze-diagnostics, ide-diagnostics): the new match checking fields (db-matchck)

What: `hir`'s match checking types now come from `hir-ty-matchck` (rustc's `check_match`).
The callers that read the old `uncovered_patterns: String` or match exhaustively over these
enums need to move to these types:

- `hir::MissingMatchArms<'db>` (E0004): `scrutinee_expr: InFile<AstPtr<ast::Expr>>`,
  `scrutinee_ty: Type<'db>`, `no_arms: bool` (rustc's "type `T` is non-empty" for a type that is
  not an enum with variants), `witnesses: Vec<String>` (every witness as `print_witness_pat`
  prints it; rustc shows the first 3), `variants: Vec<EnumVariant>`
  (`maybe_point_at_variant` of the first 5), `special_tys: Vec<(Type<'db>, SpecialTy)>`
  (`collect_special_tys` of the first witness; only for a match with arms),
  `would_be_exhaustive_without_guards: bool`, `ref_to_uninhabited: bool` ("references are always
  considered inhabited"), `consts_like_bindings: Vec<(InFile<AstPtr<ast::Pat>>, Const)>` (arms
  whose pattern is a constant that looks like a binding, in arm order).
- `hir::NonExhaustiveLet<'db>` (E0005): `pat: InFile<ExprOrPatPtr>` (an expression for a
  destructuring assignment), `origin: BindingOrigin { Let, FnParam, ClosureParam, ForLoop,
  Assignment }` (`Assignment` prints as "local binding", as rustc lowers it to a `let`),
  `has_initializer: bool`, `pattern_ty: Type<'db>`, `witnesses`, `variants` (only for an ADT of
  this crate, `point_at_non_local` false), `special_tys` (always empty for E0005, as rustc),
  `privately_uninhabited: bool`, `interpreted_as_const: Option<Const>`.
- `hir::SpecialTy { Usize, Isize, Str, ForeignNonExhaustive, Uninhabited }`: which note rustc's
  `report_non_exhaustive_match` adds for the type.
- New `AnyDiagnostic` variants: `EmptyRangePattern { pat, inclusive }` (E0030 for `..=`, E0579
  for `..`), `PatternComplexityLimit { at: InFile<SyntaxNodePtr> }` ("reached pattern complexity
  limit"), `MixedDerefPatterns { deref_pat, normal_pat }` ("mix of deref patterns and normal
  constructors").
- `hir::MatchLint` gains `OverlappingRangeEndpoints { pat, overlap: String, overlaps_with }` and
  `NonContiguousRangeEndpoints { pat, gap: String, suggestion: String, gapped_with }`
  (`gapped_with` empty is rustc's `ExclusiveRangeMissingMax`; `suggestion` is the `lo..=hi` text).
  `hir::LetSource` stays `{ IfLet, IfLetGuard, WhileLet, LetElse }` (`else if let` folds into
  `IfLet`, as rustc words both alike).
- `DefWithBody::match_check_failed(db)`: rustc's `check_match` returned `Err`, or the body was
  not checked because type checking failed. In both cases rustc builds no MIR of the body, so the
  body gets no MIR lint (`mir_tainted`).

Where: `crates/blaze-diagnostics/src/lower/{mod.rs (the three new AnyDiagnostic arms),
missing_match_arms.rs:34, non_exhaustive_let.rs}`, `crates/blaze-diagnostics/src/lints/bodies/patterns.rs:35`
(the two range lints), and while it exists `crates/ide-diagnostics/src/lib.rs:531-535` and
`handlers/{missing_match_arms,non_exhaustive_let}.rs` (`d.uncovered_patterns` becomes
`d.witnesses.join(", ")`; the exhaustive `AnyDiagnostic` match needs the three new arms).
Why: the old string came from hir-ide's `match_check.rs`, which made every literal, range and
constant `_`. These are rustc's own facts, so the lowering no longer re-parses witness text.

> told by tmux (window `diagnostics`, 2026-10-29).

## 4. db (hir-ide): drop hir-ide's match checking (db-matchck)

What: delete `validate_match` / `check_non_exhaustive_let` and `BodyValidationDiagnostic::{MissingMatchArms,
NonExhaustiveLet}` from `crates/hir-ide/src/diagnostics/expr.rs`, with
`diagnostics/match_check.rs` and `match_check/pat_analysis.rs` if nothing else reads them.
`crates/hir/src/diagnostics.rs:2083` ignores those two variants now.
Where: `crates/hir-ide/src/diagnostics/`.
Why: no-fallback: two match checkers must not both run, and hir-ide's would still cost a pass per
body. `hir-ty-matchck` replaces it.

> taken by requests (hir-ide is requests'): child `requests-db-fixture`, which holds hir-ide now; the outcome follows. hir's `diagnostics.rs:2115-2117` arms go in the same compile (db's file, requested of db).
> applied by requests (child requests-db-fixture): `validate_match`, `is_known_valid_scrutinee`, `check_non_exhaustive_let`, `lower_pattern`, `types_of_subpatterns_do_match`, `missing_match_arms` and the two variants are gone from `diagnostics/expr.rs` (`validate_block` keeps only the unused-must-use check); `diagnostics/match_check.rs` and `match_check/{pat_analysis,pat_util}.rs` deleted, with `mod match_check;`, `extern crate rustc_pattern_analysis` and hir-ide's `typed-arena`/`ra-ap-rustc_pattern_analysis` dependencies. hir's arm (`hir/src/diagnostics.rs` ~L2280-2283) is requested in `v2/requests/requests.md` for the same compile; db-next2 told. Nothing built.

## 5. db-ty (hir-ty): inhabitedness ignoring visibility (db-matchck, optional)

What: `hir_ty::inhabitedness`: a mode of `UninhabitedFrom` that ignores field visibility (rustc's
`inhabited_predicate(..).apply_ignore_module`).
Where: `crates/hir-ty/src/inhabitedness.rs`.
Why: `hir-ty-matchck/src/visit.rs` (`uninhabited_ignoring_visibility`) has its own walk of it,
for `witness_1_is_privately_uninhabited`. One rule in one place; once it lands, the matchck copy
goes.

## 6. Owner of the hir items (db-matchck)

The hir items are db-matchck-hir's (below). db-matchck (window 62, now 59) keeps
`crates/hir-ty-matchck/src/*` apart from `tests.rs` and `src/tests/`. Its one edit in
`crates/hir/src/match_lints.rs` was hir's own four-variant `LetSource` (rustc's `ElseIfLet`
folded into `IfLet`, as `patterns.rs:141` matches four), plus `// alloc:` notes; db-matchck-hir
was told.

> from db-matchck-hir (tmux window 103, a second agent continuing db-matchck): that agent wrote
> `crates/hir/src/match_lints.rs`, the five diagnostics types and the call in
> `crates/hir/src/diagnostics.rs`, the `crates/hir` manifest line and
> `crates/hir-ty-matchck/src/tests/ui.rs` (more rustc UI cases, through `tests.rs`'s `check`), by
> the split the user chose (2026-10-29). Lane db confirmed the split: db-matchck-hir owns
> `crates/hir/src/match_lints.rs`, the `diagnostics.rs` items above, the `lib.rs` and
> `Cargo.toml` lines, and `crates/hir-ty-matchck/src/tests.rs` with `src/tests/` (report
> `/tmp/blaze-lanes/v2-db-matchck-hir.md`). Briefed by tmux: window 62 and db-ty-next2 (window 89).

## 7. db (hir-ty-borrowck, hir): no borrow checking of a body match checking refused (db-matchck)

What: skip `hir_ty_borrowck::borrowck` (or make it return nothing) for a body where
`hir_ty_matchck::match_check(db, def)` has `failed` or `not_checked`; in `crates/hir/src/diagnostics.rs`
that is the `collect_borrowck(def, body, source_map)` call right after
`match_lints::match_check_diagnostics`.
Where: `crates/hir-ty-borrowck/src/lib.rs` (`borrowck`), or `collect_borrowck` in
`crates/hir/src/diagnostics.rs`.
Why: rustc's `mir_built` gives the error body when `check_match` returns `Err`, so a body with
E0004, E0005 or E0030 gets no borrow checking error. Blaze would report errors rustc does not.
`crates/blaze-diagnostics/src/lints/bodies.rs:132` already stops the MIR lints this way
(`match_check_failed`).

## 8. frontend (hir-def): `never_patterns` in `UnstableFeatures` (db-matchck)

What: a `never_patterns: bool` field in `UnstableFeatures`, set by `#![feature(never_patterns)]`.
Where: `crates/hir-def/src/unstable_features.rs`.
Why: under `never_patterns` rustc prints an uninhabited witness as `!` rather than `_`
(`print_witness_pat`); `crates/hir-ty-matchck/src/print.rs:103` always prints `_`. A crate
with the feature then gets different E0004/E0005 text.
