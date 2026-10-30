---
status: plan
date: 2026-10-30
summary: no-fallback in hir-ty, hir-ty-matchck and rustc_abi — each site classed as error, rule or dead, with the rustc function it matches and what it becomes.
---
# no-fallback: hir-ty, hir-ty-matchck, rustc_abi

Child `no-fallback-front-ty` of [front](../front.md). Kinds as in [no-fallback](../../no-fallback.md):
**error** (a real fallback; becomes a typed error), **rule** (rustc does the same; stays),
**dead** (deleted). "Done" in the last column means the change is in the tree.

`never!` (`stdx::never!`) panics in debug builds and only logs in release, so in a shipped
blaze each one logs and goes on. Where rustc has `bug!`/`span_bug!` at the same place, it
becomes `unreachable!`/`assert!`: the same stop rustc makes (an ICE), and no change for debug
builds or tests.

## The lines of `/tmp/fallbacks.txt`

| site | kind | what it does | rustc | becomes |
|---|---|---|---|---|
| `hir-ty/src/infer/fallback.rs` (whole file, `type_inference_fallback`, `fallback_types`, `fallback_if_possible`) | rule | unconstrained int vars become `i32`, float vars `f64` | `FnCtxt::type_inference_fallback`, `fallback_if_possible` (`rustc_hir_typeck/src/fallback.rs`) | stays |
| `hir-ty/src/infer/fallback.rs:155` `calculate_diverging_fallback` | rule | a diverging var never constrained becomes `!` | `FnCtxt::calculate_diverging_fallback` at the pinned nightly (never-type fallback to `!`) | stays |
| `hir-ty/src/infer.rs:280,283,294` | rule | fallback runs before cast checks and closure analysis | `typeck_with_inspect` order: `type_inference_fallback`, then `check_casts`, then `closure_analyze` | stays |
| `hir-ty/src/infer.rs:1106` | rule | doc of `has_errors` | — | stays (comment) |
| `hir-ty/src/infer/at_error.rs:3` | rule | doc: a literal at an error type is `i32`/`f64` after fallback | as `type_inference_fallback` | stays |
| `hir-ty/src/infer/unify.rs:296` | rule | doc: int/float vars kept for fallback | — | stays |
| `hir-ty/src/infer/unify.rs:820,824` (writeback resolver: int/float var → `i32`/`f64` after `never!`) | error | an int or float var left after fallback was logged and quietly given `i32`/`f64` | fallback resolves every one; writeback's `Resolver` never sees one | **done**: `unreachable!` |
| `hir-ty/src/infer/unify.rs` fresh ty/const vars in writeback | error | logged, made an error type, went on | `FullTypeResolver`: `bug!("Unexpected type in full type resolver")` | **done**: `unreachable!` |
| `hir-ty/src/infer/coerce.rs:237`, `coerce/fast.rs:5` | rule | coercion falls back to subtyping (`unify_and`), fast path rolls back its snapshot on failure | `Coerce::coerce` ends in `self.unify(a, b)`; `commit_if_ok` | stays |
| `hir-ty/src/infer/closure.rs:918` (`sig_of_closure_with_expectation`: merge fails → `sig_of_closure_no_expectation`, the caller at :871) | rule | a closure whose annotated signature does not merge with the expected one takes the annotated one; the mismatch is reported when the body is checked | `FnCtxt::sig_of_closure_with_expectation` (`rustc_hir_typeck/src/closure.rs`), same `Err(_) => sig_of_closure_no_expectation` | stays |
| `hir-ty/src/infer/closure.rs:1000-1017` (`supplied_sig_of_closure`: async output `next_ty_var`) | rule | the output of an async closure or block with no annotation is a fresh var; one never resolved is E0282 at writeback | `FnCtxt::supplied_sig_of_closure`, same `unwrap_or_else(...)` and `_` | stays |
| `hir-ty/src/infer/opaques.rs:91` | rule | doc: an opaque with a `HasDefiningUse` from fallback | `FnCtxt::handle_opaque_type_uses` | stays |
| `hir-ty/src/infer/pat.rs:262` | rule | edition-2024 inherited-ref matching without the feature gates is `EatBoth { consider_inherited_ref: false }` | `FnCtxt::ref_pat_matches_inherited_ref` (verbatim) | stays |
| `hir-ty/src/infer/pat.rs:1441,1461,1483` | rule | match ergonomics rule 4: fall back to the inherited reference | `FnCtxt::check_pat_ref` | stays |
| `hir-ty/src/infer/closure/analysis/expr_use_visitor.rs:562` | rule | doc: `Delegate::copy` defaults to `borrow` | `expr_use_visitor::Delegate::copy` | stays |
| `hir-ty/src/autoderef.rs:53` (`replace_infer_with_error` in the public `autoderef`) | rule | the steps given out for diagnostics' suggestions have their canonical vars as errors | not a build path: only `hir::Type::autoderef`, read by blaze-diagnostics' suggestion code (`unresolved_method_call`, `invalid_cast`, ...) | stays |
| `hir-ty/src/next_solver/generic_arg.rs:495-540` | rule | `fallback` is the callback name for a parameter with no argument or default; the caller decides (an infer var, an error arg with a diagnostic) | `GenericArgs::for_item` / `fill_rest` | stays (name only) |
| `hir-ty/src/next_solver/infer/type_variable.rs:323`, `union_find.rs:221` | rule | allocation notes for the fallback pass | — | stays |
| `hir-ty/src/next_solver/infer/errors.rs:443`, `infer/mod.rs:832` | rule | solver docs (`NormalizesTo` rigid check; `resolve_vars_if_possible`) | ported comments | stays |
| `hir-ty/src/next_solver/interner.rs:2521` | rule | a cycle's empty result | salsa cycle result, not a case | stays |
| `hir-ty/src/mir.rs:380,388,395` | rule | `SwitchTargets::otherwise`, the switch's "fallback target" | `SwitchTargets::otherwise` | stays |
| `hir-ty/src/layout.rs:147` `fallback_fmt` | rule | `LayoutCalculatorError`'s message without a context | `LayoutCalculatorError::fallback_fmt` | stays |
| `hir-ty-matchck/src/typo.rs`, `visit.rs:47,748`, `lib.rs:93,97` | rule | a catch-all binding that names a constant | `find_fallback_pattern_typo` (`rustc_mir_build/src/thir/pattern/check_match.rs`) | stays |
| `rustc_abi/src/extern_abi.rs:299` | rule | doc: `system` falls back to `C` with varargs | `ExternAbi::supports_varargs` | stays |
| `rustc_abi/src/extern_abi.rs:380` | rule | `ExternAbi::FALLBACK` | `ExternAbi::FALLBACK` | stays |
| `rustc_abi/src/layout.rs:155` | rule | `fallback_fmt` | as above | stays |

## `never!` that logs and goes on

| site | kind | what it does | rustc | becomes |
|---|---|---|---|---|
| `hir-ty/src/variance.rs:247` (FnDef, closures, coroutines) | error | logged, added no constraint | `add_constraints_from_ty`: `bug!("Unexpected unnameable type in variance computation")` | **done**: `unreachable!` |
| `hir-ty/src/variance.rs:316` (witness, placeholder, infer, binder) | error | logged, went on | `bug!("unexpected type encountered in variance inference")` | **done**: `unreachable!` |
| `hir-ty/src/variance.rs:390` (no return type) | error | logged, added no constraint for the signature | the signature always has an output | **done**: `unreachable!` |
| `hir-ty/src/variance.rs:422` (region kinds) | error | logged, went on | `add_constraints_from_region`: `bug!("unexpected region encountered in variance inference")` | **done**: `unreachable!` |
| `hir-ty/src/infer/path.rs:88` (local with no type) | error | logged, the path resolved to nothing | `FnCtxt::local_ty`: `span_bug!("no type for local variable")` | **done**: `unreachable!` |
| `hir-ty/src/infer/path.rs:426` (assoc item in a module) | error | logged, resolved to nothing | not reachable from rustc's resolver | **done**: `unreachable!` |
| `hir-ty/src/infer/expr.rs:1180` (non-ADT struct expression) | error | logged, fields left unchecked | `check_expr_struct_fields`: `span_bug!("non-ADT passed to check_expr_struct_fields")` | **done**: `assert!` it is an error type (reported where made), else panic |
| `hir-ty/src/infer/expr.rs:1289` (FRU subtyping) | error | logged, went on | `span_bug!("subtyping remaining fields of type changing FRU failed")` | **done**: `unreachable!` |
| `hir-ty/src/infer/cast.rs:372` (array ref to pointer) | error | logged, the cast went on | `check_ref_cast`: `bug!("could not cast from reference to array to pointer to array")` | **done**: `unreachable!` |
| `hir-ty/src/infer/cast.rs:665,711,715` (`pointer_kind`) | rule | logs, returns `Err`, which becomes `CastError::Unknown`, an `InvalidCast` diagnostic | `pointer_kind`: `span_delayed_bug` + `Err(guar)` (fails the build) | stays |
| `hir-ty/src/infer.rs:2901` (infer vars in an anon const's type) | error | logged, an anon const with infer vars in its type was interned | — | **done**: `assert!` |
| `hir-ty/src/infer.rs:3344,3454` (lang item path in variant position) | error | logged, error type with no diagnostic | not reachable | **done**: `unreachable!` |
| `hir-ty/src/lower.rs:3164` (assoc shorthand not found after the check) | error | logged, skipped the bound | not reachable | **done**: `unreachable!` |
| `hir-ty/src/lower/path.rs:251` (bare trait path, 0 segments left) | error | logged, error type | handled before (`lower_path`) | **done**: `unreachable!` |
| `hir-ty/src/lower.rs:1237` (pattern type of another pattern) | error | logged, error type, no diagnostic (`#![feature(pattern_types)]` only) | `rustc_ast_lowering` refuses the pattern with an error | queued: needs a lowering diagnostic (see below) |
| `hir-ty/src/diagnostics/decl_check.rs:376,464,554,627,747` | rule | the naming-convention lint finds no AST node for a replacement and skips the lint | lint emission only; no program meaning | stays |
| `hir-ty/src/display.rs:869,916,1507,1566` | rule | rendering of a type or constant | text only | stays |

## Shapes that don't use the word

| site | kind | what it does | rustc | becomes |
|---|---|---|---|---|
| `hir-ty/src/layout.rs:351` `Normalizer::normalize`: `deeply_normalize(ty).unwrap_or(ty)` | error | a type that did not normalize was laid out as the alias | `layout_of`: `try_normalize_erasing_regions` → `LayoutError::NormalizationFailure` | **done**: `LayoutError::NormalizationFailure { ty }`, returned from `layout_of_ty` and from the pointer-metadata tail walk |
| `hir-ty/src/layout/adt.rs:117` `d.ok()?` | error | a variant whose discriminant failed to evaluate was left out of the enum's layout | `AdtDef::discriminants` + E0080 stops the build | **done**: `LayoutError::DiscriminantFailed { variant }`; test `tests/layout_errors.rs` |
| `hir-ty/src/layout/adt.rs:115` `repr_discr(..).unwrap_or((I8, false))` | error | a `#[repr]` too small for the (truncated) values became `u8` | `Integer::repr_discr`: `bug!("Integer::repr_discr: #[repr] hint too small")` | **done**: panic naming the enum |
| `hir-ty/src/infer.rs:3135` `demand_coerce` | error | a refused coercion became an error type with no diagnostic | `FnCtxt::demand_coerce` → `demand_coerce_diag`: E0308 | **done**: `emit_type_mismatch` (E0308), then the error type |
| `hir-ty-matchck/src/cx.rs:206` field types: `deeply_normalize(ty).unwrap_or(ty)` | error | a field type that did not normalize was checked as the alias | `ctor_sub_tys`: `normalize_erasing_regions` (panics) | **done**: panic naming the field; bodies with errors are skipped before (`lib.rs:291`) |
| `hir-ty/src/lower.rs:936,944` `lower_expr_as_const` / `lower_path_as_const` `unwrap_or(consts.error)` | error | a const argument in a type that does not lower (unresolved, a static or local, a forbidden generic, a literal of the wrong type) is an error const with no diagnostic | E0425 / E0435 / "generic parameters may not be used in const operations" / E0308 | queued (diagnostic plumbing) |
| `hir-ty/src/infer.rs:2928,2935` `create_anon_const` / `make_path_as_const` `unwrap_or_else(next_const_var(Span::Dummy))` | error | in a body the same failure becomes a **fresh inference var**, which other constraints can solve: a program rustc rejects can type-check. `Span::Dummy` also hides it from E0282 | as above; only `_` (`UnderscoreExpr`) is a var in rustc | queued (diagnostic plumbing); worst of the set |
| `hir-ty/src/infer/unify.rs` `err_on_span` returns on a dummy span; const var with no span | error | an unresolved var made with `Span::Dummy` becomes an error type with no E0282 | `Resolver::report_error` always reports | after the const-arg fix removes the `Span::Dummy` vars, report or `unreachable!` |
| `hir-ty/src/mir.rs:2142` place projection: `structurally_normalize` `Err(_) => error` | error | a place type that does not normalize becomes an error type | `PlaceTy::projection_ty`: `normalize_erasing_regions` (panics) | open: callers in borrowck and MIR lowering decide; owner hir-ty-mir side |
| `hir-ty/src/mir.rs:2177,2183` Deref/Index of a non-pointer: error type | error | "lowering a deref pattern on a smart pointer gets here": an unsupported feature becomes an error type | `bug!("deref projection of non-dereferenceable ty")`; deref patterns are lowered differently | open: MIR lowering should refuse deref patterns with a typed error (hir-ty-mir) |
| `hir-ty/src/layout.rs:593,650,654,711,716` `LayoutError::Unknown` | error (untyped) | a MIR or const failure under a layout becomes `Unknown` | rustc's `LayoutError::ReferencesError` / `TooGeneric` / `Unknown(ty)` | stays an error; typed variants later (not a fallback) |
| `hir-ty/src/consteval/hir_eval.rs` `by_hir` → `by_mir` | rule | constants of a closed shape run over HIR, all others over MIR; both give the same bytes and errors (`docs/v2/plan/lanes/frontend/fast-const.md`) | — (blaze speed path) | stays: dispatch by shape, not on failure. `ok()?` at :147,:439 hide an error the MIR path then reports |
| `hir-ty/src/consteval.rs:73,81,88` `bit_width().unwrap_or(pointer_size)` | rule | `usize`/`isize` have no fixed width | `Integer::from_int_ty` | stays |
| `hir-ty/src/consteval.rs:75,83,90` literal not fitting → `Const::error` | error | a const-generic literal too large for its type is an error const; rustc truncates it (and `overflowing_literals` denies by default) | `lit_to_const`: `trunc` | queued with the const-arg diagnostics |
| `hir-ty/src/layout/adt.rs:84,89,96`, `layout.rs:271,429`, `consteval/discriminants.rs:42` `AttrFlags::repr(..).unwrap_or_default()` | rule | no `#[repr]` is the default repr | `AdtDef::repr` default | stays |
| `hir-ty/src/traits.rs:82` `structurally_normalize_ty` | rule | "used in `hir` only" (IDE) | — | stays |
| `hir-ty/src/inhabitedness.rs:133` | rule | an alias that does not normalize is opaquely inhabited | `InhabitedPredicate::GenericType` is inhabited when unknown | stays |
| `hir-ty/src/method_resolution/confirm.rs:506` | rule | receiver mismatch reported only with `arbitrary_self_types` | `ConfirmContext::unify_receivers` (else `span_delayed_bug`; wfcheck reports E0307) | stays |
| `hir-ty/src/infer/pat.rs:1318`, `infer/expr.rs:160`, `infer/pat.rs:1760`, `infer/cast.rs:246`, `infer/coerce.rs:910,1026`, `infer/expr.rs:2014,2102` | rule | error recovery after a diagnostic is pushed | the same `FnCtxt` functions | stays |
| `rustc_abi/src/*` (`unwrap_or` in `lib.rs`, `layout.rs`; `layout.rs:628` `.ok()?` in niche filling) | rule | copies of rustc's `rustc_abi` (`LayoutCalculator::layout_of_enum` tries a niche layout and takes the tagged one when it fails) | `rustc_abi` verbatim | stays |
| `hir-ty-matchck/src/lower.rs:968-1000`, `print.rs:37,39`, `visit.rs:809` | rule | open range ends; float text; printing; a witness's type | `rustc_pattern_analysis` / `check_match` | stays |

## Silent error types (`FIXME: report`)

Each of these gives `TyKind::Error` or an error const/region **without a diagnostic**, and
no other pass reports the code (checked in blaze-diagnostics and hir: none emits E0220,
E0223, E0225, E0393, E0562, E0719, E0071). A program rustc rejects can get through, or fail
later at a layout with no place. Each becomes a typed diagnostic variant (the rustc code
with its place). That means a new `TyLoweringDiagnostic`/`PathLoweringDiagnostic`/
`InferenceDiagnostic` variant, its words in `lower/persist.rs` or `infer/persist.rs`, an arm in
`hir`'s exhaustive `path_diagnostic` / inference match, and an emitter in blaze-diagnostics.
All of them have to land in one save to compile, across three owners. They are queued as one
request (see the report).

| site | rustc |
|---|---|
| `lower/path.rs:243` assoc type not found on a trait path | E0220 `complain_about_assoc_item_not_found` |
| `lower/path.rs:179,254` ambiguous associated type (`Trait::Assoc`, more than one segment) | E0223 `report_ambiguous_assoc_item` |
| `lower/path.rs:534,555` `Self::Name` / `T::Name` with no such assoc type | E0220 |
| `lower/path.rs:274` an enum variant or module as a type | E0573 / E0433 (`expected type, found variant`) |
| `lower/path.rs:1386` `<T as Struct>::X` | E0404 |
| `lower.rs:960,974,996` a forbidden generic parameter (a default naming a later one) | E0128 (blaze-diagnostics `checks/defs/generics.rs` reports it from names; to confirm it covers every case here) |
| `lower.rs:1188` `impl Trait` where not allowed | E0562 |
| `lower.rs:1649` a second non-auto trait in `dyn` | E0225 |
| `lower.rs:1660` a second lifetime bound in `dyn` | E0226 (blaze-diagnostics `ast_validation/types.rs` reports it) |
| `lower.rs:1710` conflicting assoc bindings | E0719 |
| `lower.rs:1829` `Self` in a `dyn` default | E0393 |
| `lower.rs:1885` `dyn` with no principal resolved | E0224 |
| `infer.rs:3251,3262,3372,3434,3472,3485,3525` struct path that resolves to a non-struct, or with extra segments | E0071 / E0109 / E0223 |
| `infer/path.rs:98,189` invalid `Self` reference, first segment unresolved | E0411 / E0433 |
| `infer/closure.rs:512` closure signature deduction obligations dropped | rule: rustc drops them too (`deduce_sig_from_projection` "may discard the goal") |
| `method_resolution.rs:219` a private method picked | rule: E0624 is reported by blaze-diagnostics `lower/private_assoc_item.rs` from the pick |
| `method_resolution/probe.rs:411` autoderef reached the recursion limit while probing | E0055 `report_autoderef_recursion_limit_error` (emitted nowhere) |
| `method_resolution/probe.rs:425` method on a raw pointer to an unknown pointee, edition 2018+ | E0699 `MethodCallOnUnknownRawPointee` (the 2015 lint `tyvar_behind_raw_pointer` too) |
| `method_resolution/probe.rs:451` self type still a var | rule: E0282 is reported at writeback for a var still unresolved (`TypeMustBeKnown`); rustc reports it here, earlier |
| `method_resolution/confirm.rs:619` | rule: the allow-by-default `supertrait_item_shadowing_usage` lint; no program meaning |
| `infer/pat.rs:594` deref pattern past the recursion limit | E0055 |
| `infer/pat.rs:1053` a path pattern naming a function, static, local or generic | E0532 / E0530 (emitted nowhere) |
| `infer/pat.rs:1426,1464,1501` `&mut` pattern on an inherited shared ref (`ref_pat_eat_one_layer_2024*` only) | E0308 |
| `infer/expr.rs:1259,1351` missing fields | rule: E0063 is reported by blaze-diagnostics `lower/missing_fields.rs` |
| `infer/expr.rs:2319` `Fn*` call whose `Args` is not a tuple (`unboxed_closures` only) | E0059 |
