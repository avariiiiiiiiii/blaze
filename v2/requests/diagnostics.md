# Requests of lane diagnostics

The lane writes `crates/blaze-diagnostics`: every error and warning rustc gives, as rustc
prints it (human, short, colour, JSON), each with the facts it read, from `hir`'s diagnostics
lowered to rustc's and the checks hir doesn't make. Its interface:

```rust
blaze_diagnostics::check_crate(db: &dyn HirDatabase, krate: hir::Crate, files: &dyn Files,
                               options: &Options) -> Vec<Diag>   // rustc's order, summary last
blaze_diagnostics::emit::{rendered, written, json, summary}(&Diag, &mut Sources, Style)
blaze_diagnostics::api::diagnostic(&Diag, &mut Sources, Style, Option<UnitOf>) -> blaze_api::Diagnostic
blaze_diagnostics::Diag::is_error()   // the build is refused when any is
```

Children add their own requests below, each saying which child it is from.

## 1. Root `Cargo.toml` and `Cargo.lock` (coordinator)

What: add the crate to the workspace and pin rustc's renderer.

```toml
# [workspace.dependencies]
blaze-diagnostics = { path = "crates/blaze-diagnostics", version = "0.0.0" }
annotate-snippets = "=0.12.16"   # was "0.12.16": the version rustc renders with, exactly
```

and `crates/blaze-diagnostics` in `members` if members are listed (they are globbed as
`crates/*` today: then only the dependency line). Why: rustc's layout of a snippet is
annotate-snippets 0.12.16's; another version lays some out differently, so it is pinned
(`crates/blaze-lints` pins it in its own manifest already). Its dependencies are all workspace
ones already: `either`, `rustc-hash`, `serde`, `serde_json`, `smallvec`, `base-db`,
`blaze-api`, `blaze-ids`, `blaze-items`, `blaze-query`, `cfg`, `hir`, `hir-def`, `hir-expand`,
`hir-ty`, `hir-ty-borrowck`, `intern`, `span`, `stdx`, `syntax`; dev: `expect-test`, `salsa`,
`test-fixture`, `triomphe`.

> applied: root `[workspace.dependencies]` gains `blaze-diagnostics = { path = "crates/blaze-diagnostics", version = "0.0.0" }`; `annotate-snippets = "=0.12.16"` with the reason. `Cargo.lock` already has annotate-snippets 0.12.16, so no lock change; members are globbed (`crates/*`).

## 2. `crates/blaze-api/src/lib.rs`: `Level::FailureNote` (ci)

What: a fifth level, after `Help`:

```rust
pub enum Level { Error, Warning, Note, Help, FailureNote }
```

Why: rustc's closing "For more information about this error, try `rustc --explain E0308`."
and "Some errors have detailed explanations: ..." are `failure-note`s (its JSON says
`"level":"failure-note"`, its text `For more information...` with no `note:` before it);
cargo shows them and an agent reads them. Giving them as `Note` would print them wrong.
`blaze_diagnostics::api` maps `Level::FailureNote` to it.

> applied: `Level::FailureNote` in `crates/blaze-api/src/lib.rs` (ci is DONE), with ci's matches changed with it: `blaze-api/src/session/best_of.rs` and `blaze-ci/src/service.rs` count it as neither error nor warning, `service.rs` names it `failure-note`, and `blaze-ui/src/api.rs` prints its message with no level before it.
> forwarded to cargo: `crates/blaze-cargo/src/cli/json.rs:224` `level` matches `Level` exhaustively and needs `Level::FailureNote => "failure-note"`.

## 3. `crates/blaze-engine` (codegen): builds refuse every error rustc gives (#1522, #1440, #780)

What: `lib.rs` `refusal`, `broken_in` (6487), `errors_of` (6593) and the allowlists `SURE`,
`BORROWCK`, `DENIED`, `DENIED_BORROWCK` (6439-6483); `doctest.rs:874-880`; `borrowck.rs:87,
206, 564-568`: replace `ide_diagnostics` by `blaze_diagnostics::check_crate` over each crate
of the build (every file, not the edited ones), refuse the build when any diagnostic
`is_error()`, and print them with `blaze_diagnostics::emit` (or hand them to cargo, 4). No
allowlist: every error is one. Then `ide-diagnostics` leaves `crates/blaze-engine/Cargo.toml`
and the workspace, and lane diagnostics deletes `crates/ide-diagnostics`.

Why: the allowlists are why E0063 and E0277 surfaced only as codegen refusals (#1522), E0015
wasn't refused (#1440), and a `-p` build passed a type error (#780): an error not on a list
was not an error, and errors of files the edit didn't touch weren't looked for. salsa keeps
the whole-crate check cheap: an unedited body's diagnostics are memoized.

> forwarded to codegen

## 4. `crates/blaze-cargo` (cargo): the CLI prints what rustc and cargo print

What: `errors.rs` (its one-span renderer) and `cargo/messages.rs`: for each unit, print
`blaze_diagnostics::emit::written` (human; `Style { short, ansi, width }` from
`--message-format=short`, `--color` and the terminal), drop the "aborting due to" diagnostic as
cargo does and keep the failure-notes, and for `--message-format=json*` emit cargo's
`compiler-message` around `blaze_diagnostics::emit::json` (`rendered` with ANSI for
`json-diagnostic-rendered-ansi`, short for `json-diagnostic-short`), then cargo's "warning:
`x` (lib) generated N warnings" and "error: could not compile `x` (lib) due to N previous
errors". Lint levels from the command line and `[lints]`: `blaze_diagnostics::lints::
CommandLine::parse` of the `-A/-W/-D/-F/--cap-lints` cargo would pass (`--cap-lints allow`
for non-local packages, as cargo does).

Why: rustc's text exactly is the lane's goal; the old renderer has one span and no children.

> forwarded to cargo

## 5. `crates/hir`, `crates/hir-ty-borrowck` (db): an unchecked body is always reported

What: `AnyDiagnostic::BorrowckSkipped` given for every body borrowck does not check, always
(not only under an environment variable), with what it could not do.

Why: no-fallback. A body borrowck skipped is a body blaze accepts that rustc might reject;
blaze-diagnostics refuses it with ``blaze cannot borrow-check `<path>`: <reason>`` until
borrowck checks it.

> forwarded to db

## diagnostics-bodies: the front end's values of `hir::Type`, `hir::TraitRef`, `hir::TraitPredicate` (db)

Where: `crates/hir/src/lib.rs`. What, exactly:

```rust
impl<'db> Type<'db> {
    /// The type as the front end has it, for rustc's printing and checks outside `hir`.
    pub fn to_next_solver(&self) -> Ty<'db> {
        self.ty.skip_binder()
    }

    /// A type of the same owner as `self`.
    pub fn derived_from_next_solver(&self, ty: Ty<'db>) -> Type<'db> {
        self.derived(ty)
    }
}

impl<'db> TraitRef<'db> {
    pub fn to_next_solver(&self) -> hir_ide::next_solver::TraitRef<'db> {
        self.trait_ref
    }

    /// A type of the same owner as the trait ref.
    pub fn derived_from_next_solver(&self, ty: Ty<'db>) -> Type<'db> {
        Type { owner: self.owner, ty: EarlyBinder::bind(ty) }
    }
}

impl<'db> TraitPredicate<'db> {
    pub fn to_next_solver(&self) -> hir_ide::next_solver::TraitPredicate<'db> {
        self.inner
    }
}
```

Why: rustc's printing of types in diagnostics (`blaze-diagnostics/src/print.rs`, from
`rustc_middle/src/ty/print/pretty.rs`) walks the type's structure: integer and float variables as
`{integer}`/`{float}`, closures by where they are written, generic arguments equal to their
defaults left out, trimmed paths, regions as rustc shows them. `HirDisplay` is rust-analyzer's
printing, not rustc's, and `hir::Type` has no public way to the structure (`TyKind`) it holds.
The E0308/E0277 lowering needs the same of trait refs and predicates.

> forwarded to db: `crates/hir/` is lane db's.

## diagnostics-bodies: `rustc_type_ir` for blaze-diagnostics (the parent, `diagnostics`)

Where: `crates/blaze-diagnostics/Cargo.toml`: `rustc_type_ir.workspace = true` under
`[dependencies]`. Why: `print.rs` matches on `rustc_type_ir::TyKind` and uses its `inherent`
traits on `hir_ty::next_solver` types.

> applied: `rustc_type_ir` is already a root workspace dependency; `crates/blaze-diagnostics/Cargo.toml` is lane diagnostics' own to add it to.

## diagnostics-bodies: names used through a glob import (db, hir-def)

Where: `crates/hir-def/src/item_scope.rs` (or a query beside it). What: for each glob import of
a crate, the names that some path of the crate resolved through it (rustc's
`Resolutions::glob_map`). Why: rustc's `trimmed_def_paths` counts a name used through a glob as
not unique (so `use a::*;` with `a::S` used makes `S` print as `a::S`), and only the names used.
Until hir has it, `print.rs` counts every name a local glob import brings into a scope (more than
rustc: such a name prints by its path where rustc may trim it).

> forwarded to db

## diagnostics-bodies: the names of the regions a `for<'a>` binds (db, `crates/hir-ty/src/lower.rs`)

Where: where `hir-ty` lowers a `for<'a, 'b>` binder (of a `fn` pointer type, a `dyn` or
where-clause bound) to `BoundRegionKind::Named(def)`. What: a way from a bound region to the
name written, e.g. `BoundRegionKind::Named` keyed by the binder's own lifetime (an interned id of
the `for<..>` parameter), not by the enclosing definition and the variable's index. Why: rustc
prints `for<'x> fn(&'x u8)` with the name written; today `Named(def)` with `var` an index inside
the binder can't be told from a late-bound lifetime of `def`, so `print.rs` names the regions of
such a binder `'a`, `'b`, … as rustc names anonymous ones (it keeps the names only of a function
item's own late-bound lifetimes and of `ReLateParam`). A type written `for<'x> fn(&'x u8)`
prints as `for<'a> fn(&'a u8)` until then.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-lints-bodies: what type checking finds unreachable (db, `crates/hir-ty`)

Where: `crates/hir-ty/src/infer.rs` (`InferenceResult`, the `Diverges` tracking that is there
already), exported at hir-ty's root. What, exactly:

```rust
/// What rustc's `warn_if_unreachable` (`rustc_hir_typeck/src/fn_ctxt/_impl.rs`) warns of: code
/// type checking reaches after an expression that diverges (`unreachable_code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreachable {
    pub what: UnreachableAt,
    /// What diverged: `Diverges::Always`'s `span` (an expression, or a never pattern).
    pub after: ExprOrPatId,
    /// `Diverges::Always`'s `custom_note` ("any code following this `match` expression is
    /// unreachable, as all arms diverge", "any code following a never pattern is unreachable").
    pub note: Option<&'static str>,
}

/// rustc's `id`, `span` and `kind` of `warn_if_unreachable`, by its callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreachableAt {
    /// "expression": `check_expr_with_expectation_and_args`, before an expression and after one
    /// whose children diverged, and `check_argument_types` for an argument.
    Expr(ExprId),
    /// "call": a call whose callee or arguments diverged (`ExprKind::Call`/`MethodCall`).
    Call(ExprId),
    /// "statement": `check_stmt`, statement `index` of the block.
    Stmt { block: ExprId, index: u32 },
    /// "arm": `warn_arms_when_scrutinee_diverges`, the body of arm `index` of the `match`.
    Arm { match_expr: ExprId, index: u32 },
    /// "block in `if` or `while` expression": `check_expr_if`, after its condition diverged.
    IfBlock(ExprId),
    /// "block in `let` expression": `check_expr_let`, its initializer.
    LetInit(ExprId),
}

impl InferenceResult<'_> {
    /// In the order type checking warns of them.
    pub fn unreachable(&self) -> &[Unreachable];
}
```

recorded where rustc calls `warn_if_unreachable`, with its rules: none when the span is of an
`async` body's or `.await`'s desugaring, none in an `#[automatically_derived]` impl, none for a
`try` block's generated `Try::from_output(())` or a contract's desugaring, and once per
divergence (`Diverges::WarnedAlways`). hir-ty's `Diverges` needs rustc's `span` and
`custom_note` of `Always` for this (and `Always`'s ordering by span, as rustc's derived `Ord`,
which `|` uses).

Why: `unreachable_code` is type checking's lint; whether an expression diverges depends on
inference as it goes (a type that is `!` when checked), which only hir-ty knows.
`crates/blaze-diagnostics/src/lints/bodies/reachable.rs` lowers it as written above.

> forwarded to db: `crates/hir-ty/src/infer.rs` is db's (db-ty).

## diagnostics-lints-bodies: what match checking lints (db, `crates/hir-ty` or a crate of its own, `crates/hir`)

rustc's `check_match` (`compiler/rustc_mir_build/src/thir/pattern/check_match.rs` over
`rustc_pattern_analysis`) ported into the front end, with what it lints given through `hir`.
hir-ide's `ExprValidator::validate_match` is not it: its pattern lowering
(`hir-ide/src/diagnostics/match_check.rs`) makes every literal but `bool`, every range and every
constant `_`, so its usefulness would call `1 => .., 2 => ..` redundant, and it has "FIXME
Report unreachable arms". The port needs rustc's `PatCtxt::lower_pat` (literals, ranges,
constants through const eval, slices, strings, deref and box patterns, never patterns), its
`MatchVisitor` order and `LetSource`s, and gives E0004/E0005 (hir's `MissingMatchArms` and
`NonExhaustiveLet`, which `diagnostics-lints-mir-matches` lowers) from the same run. What, in
`crates/hir`:

```rust
impl DefWithBody {
    /// What `check_match` lints in this body, in the order rustc emits them (its THIR visit:
    /// per `match`, each arm's `bindings_with_variant_name` as its pattern is lowered, then the
    /// redundant or-pattern alternatives sorted by span, then the redundant arms).
    pub fn match_lints<'db>(self, db: &'db dyn HirDatabase) -> Vec<MatchLint<'db>>;
}

pub enum MatchLint<'db> {
    UnreachablePattern(UnreachablePattern<'db>),
    IrrefutableLet(IrrefutableLet),
    BindingWithVariantName(BindingWithVariantName),
}

/// `unreachable_patterns` (`report_unreachable_pattern`).
pub struct UnreachablePattern<'db> {
    pub pat: InFile<AstPtr<ast::Pat>>,
    /// `RedundancyExplanation::covered_by`, in its order; empty when it matches no values.
    pub covered_by: Vec<InFile<AstPtr<ast::Pat>>>,
    /// `covered_by` is one pattern and `pat_is_catchall` of it.
    pub catchall: bool,
    /// `matches_no_values_ty`: `pat`'s type, or the uninhabited type `pat.walk` finds.
    pub ty: Type<'db>,
    /// The arm, when a whole arm of a `match` (`Normal` or `Postfix`) is redundant
    /// (`whole_arm_span`).
    pub arm: Option<InFile<AstPtr<ast::MatchArm>>>,
    /// `find_fallback_pattern_typo`, when `catchall`.
    pub typo: Option<PatternTypo>,
}

/// `find_fallback_pattern_typo`'s finding, as it puts it on the lint.
pub enum PatternTypo {
    /// `WantedConstant`: `const_name`, `const_path` (`def_path_str` with no trimmed paths).
    WantedConstant { is_typo: bool, const_name: String, const_path: String },
    /// `accessible_constant`: the name of the `use` that re-exports it.
    AccessibleConstant(InFile<TextRange>),
    /// `inaccessible_constant`: the constant (its `def_span`).
    InaccessibleConstant(Const),
    /// `pattern_let_binding`: the name a `let` binds.
    PatternLetBinding(InFile<TextRange>),
}

/// `irrefutable_let_patterns` (`report_irrefutable_let_patterns`, always `count` 1).
pub struct IrrefutableLet {
    pub source: LetSource, // IfLet (and ElseIfLet), IfLetGuard, LetElse, WhileLet
    /// The `let` expression (or the `&&` chain of `lint_single_let`), or the `let` statement
    /// for `LetElse`.
    pub at: InFile<SyntaxNodePtr>,
    /// `check_irrefutable_option_some`: the pattern and the inner `Option` of the initializer.
    pub replacement: Option<(InFile<AstPtr<ast::Pat>>, InFile<AstPtr<ast::Expr>>)>,
}

/// `bindings_with_variant_name` (E0170, `check_for_bindings_named_same_as_variants`).
pub struct BindingWithVariantName {
    pub pat: InFile<AstPtr<ast::IdentPat>>,
    pub enum_: Enum,
    /// `rf == Refutable || variant_count == 1`.
    pub suggest: bool,
}
```

Why: these are `check_match`'s lints, the front end's semantics;
`crates/blaze-diagnostics/src/lints/bodies/patterns.rs` lowers them as written above.

> forwarded to db: `crates/hir/` and the `check_match` port into the front end are db's.

## diagnostics-lints-bodies-types: `rustc_apfloat` and `rustc_type_ir` for blaze-diagnostics (the parent, `diagnostics`)

Where: `crates/blaze-diagnostics/Cargo.toml` `[dependencies]`: `rustc_apfloat = "0.2.3"` (as
`hir-def`, `hir-ty` and `syntax` have it; already in `Cargo.lock`) and
`rustc_type_ir.workspace = true` (asked above by diagnostics-bodies too). Why: `overflowing_literals`
of a float (`lints/bodies/types/limits/literal.rs`) parses the literal as rustc does, with
apfloat's IEEE semantics of `f16`, `f32`, `f64` and `f128` (`float_is_infinite`), and
`lints/bodies/types/**` matches on `TyKind` through `rustc_type_ir::inherent::IntoKind`.

> forwarded to diagnostics: `crates/blaze-diagnostics/Cargo.toml` is the parent's own; `rustc_type_ir` is a workspace dependency, and `rustc_apfloat = "0.2.3"` is taken directly as hir-def, hir-ty and syntax take it (in the lock).

## diagnostics-lints-bodies-types: `crate::tests::normalized` visible to the crate's tests (the parent, `diagnostics`)

Where: `crates/blaze-diagnostics/src/tests.rs`: `fn normalized` -> `pub(crate) fn normalized`.
Why: `tests/lints/bodies/types.rs` (`ui_std`) checks UI tests that use std against a fixture
`std` crate beside minicore, as `ui_with` does but with more crates, and normalizes the output as
compiletest does with the same function.

> forwarded to diagnostics: `crates/blaze-diagnostics/src/tests.rs` is the parent's own.

## diagnostics-lints-bodies-types: diagnostic items (db, `crates/hir-def/src/attrs.rs`)

What: `AttrFlags::diagnostic_item(db, owner: AttrDefId) -> Option<Symbol>` (the value of
`#[rustc_diagnostic_item = "..."]`, `cfg_attr`s expanded, as `symbol_attr` reads one) and the
reverse, `diagnostic_items(db, krate) -> &FxHashMap<Symbol, AttrDefId>` over the crate and its
dependencies (rustc's `all_diagnostic_items`). Why: rustc's lints name std's items by
diagnostic item (`mem_zeroed`, `ptr_null`, `f32_nan`, `transmute`, ...). Until it is there,
`lints/bodies/types/util.rs` (`diagnostic_item`) reads the attribute from the item's source
with `hir_expand::attrs::expand_cfg_attr`, once per question and uncached.

Status: the per-item half done by speed-hir-defs: `AttrFlags::diagnostic_item(db, owner:
AttrDefId) -> Option<Symbol>` (`crates/hir-def/src/attrs.rs`). It is one bit test
(`AttrFlags::HAS_DIAGNOSTIC_ITEM`) for an item without the attribute and a cached query for one
with it, `cfg_attr`s expanded and a macro value expanded as rustc does. The reverse map
`diagnostic_items(db, krate)` is not written (it walks every module, as `lang_item.rs` does).

### for diagnostics (from speed-hir-defs): read the diagnostic item through hir-def

`crates/blaze-diagnostics/src/lints/bodies/types/util.rs`, `fn diagnostic_item`: replace the
body (the `match def` collecting `ast::Attr`s and the `expand_cfg_attr` walk) with
```rust
    hir_def::attrs::AttrFlags::diagnostic_item(db, def).map(|it| it.as_str().to_owned())
```
Why: today every question re-reads the item's syntax, collects its attributes into a `Vec` and
builds a `SmolStr` per attribute; through hir-def it is a bit test on the item's cached flags.
Returning the `Symbol` instead of a `String` (and comparing it with `sym::...`) would also drop
the allocation per answer.

> forwarded to db: `crates/hir-def/src/attrs.rs` is lane db's.

> done by db-expand-next1: the reverse map is `hir_def::diagnostic_item::diagnostic_items(db, krate) -> &DiagnosticItems` (rustc's `all_diagnostic_items`), with `crate_diagnostic_items(db, krate)` for one crate's own (rustc's `diagnostic_items`). Look up with `.get(&sym::..) -> Option<AttrDefId>`; `.by_name` is the `FxHashMap<Symbol, AttrDefId>` asked for.
> - It visits every HIR owner rustc's walk visits: modules, items, trait, impl and foreign items, unnamed consts, `use`s, `extern crate`s, macros (non-exported `macro_rules!` included), and items declared in bodies.
> - It lowers only the bodies whose text says `rustc_diagnostic_item` or names a macro that may write it (`nameres::demand::Look::DiagnosticItems`, the same prefilter as the roots one).
> - The dependencies are the extern prelude's, breadth first, so `#![no_std]` crates don't see `std`.
> - `.duplicates` lists each name given to two items, as `(name, kept, other)`. rustc refuses those ("duplicate diagnostic item in crate ..." / "duplicate diagnostic item found"), so blaze-diagnostics should report them to match rustc.
> - Tests: `crates/hir-def/src/diagnostic_item/tests.rs`.

## diagnostics-bodies-patterns: what pattern checking gives, as rustc gives it (db, `crates/hir-ty/src/infer/pat.rs`, `crates/hir/src/diagnostics.rs`)

Four changes, each from `rustc_hir_typeck/src/pat.rs`:

1. `MismatchedTupleStructPatArgCount` carries the variant and whether the pattern's type
   matched the scrutinee's: `variant: hir::Variant` and `had_err: bool` (the `had_err` of
   `infer_tuple_struct_pat`, which hir-ty computes at line ~1059 and drops). Why: rustc's
   "missing parentheses" suggestion is given only when `had_err` is `Ok`
   (`emit_err_pat_wrong_number_of_fields`, #67037). `lower/mismatched_tuple_struct_pat_arg_count.rs`
   reads the variant again from the path and cannot know `had_err`. So for
   `struct W((u8, u8)); let W(a, b) = 0u8;` it suggests parentheses where rustc does not.
2. `InvalidRangePatType` carries each end's type as inference had it when it checked that end
   (`lhs: Option<(Type, bool)>`, `rhs: Option<(Type, bool)>`, with `fail`). Why: rustc's labels
   print the types at that point: `10 ..= "what"` says "this is of type `{integer}`". After
   inference the type is `i32`, and that is all `sema.type_of_pat` can give
   (`tests/bodies/patterns.rs`, `a_range_with_one_bad_end_names_the_type_of_the_other`, is
   ignored until then).
3. `infer_range_pat`: at "FIXME: Emit an error" (line ~871), push `InvalidRangePatType` with both
   ends failing, as rustc's second `emit_err_pat_range` call does after unification.
4. `infer_record_pat` (the union checks, line ~1238): rustc reports a union pattern's unknown
   fields (E0026) after "union patterns should have exactly one field". Give `NoSuchField` for
   them after the union diagnostics, not before, so `tests/ui/union/union-fields-2.rs` comes out
   in rustc's order.
5. `hir::Crate::lang_origin(self, db) -> Option<LangCrateOrigin>` (`Copy`), and `sym::Vec` in
   `crates/intern/src/symbol/symbols.rs`. Why: deciding whether an item is core's `Option` or
   alloc's `Vec` should be two `u32` compares. Today `Crate::origin` clones `CrateOrigin`, which
   allocates for local crates (`repo: Option<String>`), and `Vec` can only be compared as a
   string. `lower/expected_array_or_slice_pat.rs` asks for the origin only after the name
   matched, so the clone happens once per suggestion, not per type.

> from diagnostics-bodies-patterns
> forwarded to db: `crates/hir-ty/src/infer/pat.rs` and `crates/hir/src/diagnostics.rs` are lane db's.

## diagnostics-lints-syntax-early-lexer: the lexer and the Unicode tables rustc lints with (coordinator; parent `diagnostics` for the crate's manifest)

What, in the root `Cargo.toml` `[workspace.dependencies]`:

```toml
# rustc's pins (`Cargo.lock` of the pinned nightly): the identifier lints' Unicode tables.
unicode-normalization = "=0.1.25"
unicode-security = "=0.1.2"
```

and `Cargo.lock` with `unicode-security 0.1.2` on `unicode-script 0.5.7`, as rustc's lock has
it. In `crates/blaze-diagnostics/Cargo.toml` `[dependencies]`:

```toml
ra-ap-rustc_lexer.workspace = true
unicode-normalization.workspace = true
unicode-security.workspace = true
```

Why: `lints/syntax/early/lexer.rs` runs rustc's lexer loop (`rustc_parse/src/lexer/mod.rs`,
`next_token_from_cursor`) over each file of the crate with the lexer rustc uses (`ra-ap-rustc_lexer`
0.166, already a workspace dependency, the same code as the pinned nightly's `rustc_lexer`). That
loop is where rustc finds `text_direction_codepoint_in_{comment,literal}`,
`rust_2021_prefixes_incompatible_syntax`, `rust_2024_guarded_string_incompatible_syntax` and the
symbol gallery (every identifier lexed, NFC-normalized with `unicode-normalization`). The gallery feeds
`non_ascii_idents`, `uncommon_codepoints`, `confusable_idents` and
`mixed_script_confusables`, which need `unicode-security`'s tables (`GeneralSecurityProfile`,
`confusable_detection::skeleton`, `mixed_script::AugmentedScriptSet`,
`is_potential_mixed_script_confusable_char`). These are the versions rustc uses, so the same
characters lint.

> applied: `unicode-normalization = "=0.1.25"` and `unicode-security = "=0.1.2"` in the root `[workspace.dependencies]`, with the comment. Both are new to `Cargo.lock` (with `unicode-script 0.5.7` under `unicode-security`): listed for the first compile, not written to the lock. The three `.workspace = true` lines are `crates/blaze-diagnostics/Cargo.toml`, the parent's.

## diagnostics-lints-syntax-early-lexer: the lexer splits what rustc splits before 2021 (format, `crates/parser/src/lexed_str.rs`)

Three tokens are lexed differently from rustc before edition 2021. Each changes what a
`macro_rules!` matcher sees and gives a hard error where rustc accepts the code:

1. `UnknownPrefix` / `UnknownPrefixLifetime` (`z"hey"`, `hey#123`, `'foo#x`): rustc gives an
   error only from 2021 (`report_unknown_prefix`). Before that the prefix is an ident (or
   lifetime) token followed by the rest, with the `rust_2021_prefixes_incompatible_syntax`
   lint. `Converter::extend_token` pushes "unknown literal prefix" / "Unknown lifetime prefix"
   in every edition. Fix: push them only when `self.edition.at_least_2021()`. Test:
   `tests/ui/rust-2021/reserved-prefixes-migration.rs` (edition 2018) gives only the lints.
2. `c"…"` / `cr#"…"#` before 2021: rustc lexes the ident `c`/`cr`, then the string literal
   (`next_token_from_cursor`, "split up (raw) c string literals to an ident and a string
   literal when edition < 2021"). The tree keeps one `C_STRING`.
3. A raw lifetime `'r#a` before 2021: rustc lexes `'r`, `#` and `a`
   (`tests/ui/lifetimes/raw/three-tokens.rs`, edition 2015, check-pass: `('r # lt) => {}`
   must match `'r#lt`). The tree keeps one `LIFETIME_IDENT`.

The lints are found by `lints/syntax/early/lexer.rs` from the text, so they don't depend on
these fixes; the hard errors and macro matching do.

> forwarded to format: `crates/parser/src/lexed_str.rs` is lane format's; lane format is still running.

## diagnostics-lints-syntax-early-lexer: identifiers made by `${concat(..)}` (db, `crates/hir-expand` mbe)

rustc puts an identifier that `${concat(..)}` makes into the symbol gallery
(`rustc_expand/src/mbe/transcribe.rs`, `symbol_gallery.insert`), so `non_ascii_idents`,
`confusable_idents` and the rest see it. What: a way to tell, for a token of a `macro_rules!`
expansion, that `${concat}` made it (for example, the expansion's span map marking such
tokens). Why: `lints/syntax/early/lexer.rs` adds a proc macro's new identifiers to the gallery
but can't find concat's (feature `macro_metavar_expr_concat`, unstable).

> forwarded to db: `crates/hir-expand` is lane db's.

## diagnostics-lints: the parts of a borrow check error (db, `crates/hir-ty-borrowck`, `crates/hir/src/diagnostics.rs`)

What: `BorrowckDiagnostic` (and `hir::BorrowckError`) carry, besides `code`, `message` and the
primary span, what rustc puts on its other spans, in rustc's words, each span a `MirSpan` that
`hir` maps to a node as it maps the primary one (with its `at_end`):

```rust
pub struct BorrowckDiagnostic {
    pub code: &'static str,
    pub span: MirSpan,
    pub message: String,
    pub at_signature: bool,
    /// rustc's labels, in the order it adds them: the primary span's own ("second mutable
    /// borrow occurs here") and the others ("first mutable borrow occurs here", "first borrow
    /// later used here", "binding `x` declared here", "`y` dropped here while still borrowed",
    /// "value moved here, in previous iteration of loop", ..).
    pub labels: Vec<(MirSpan, String)>,
    /// rustc's notes and helps, with their spans if any: "move occurs because `x` has type
    /// `String`, which does not implement the `Copy` trait", "values in a scope are dropped in
    /// the opposite order they are defined", "if `Value` implemented `Clone`, you could clone
    /// the value" (with its two labelled spans).
    pub children: Vec<(Sublevel, Vec<(MirSpan, Option<String>)>, String)>,
    /// rustc's suggestions: E0596's and E0384's `mut ` before the binding ("consider changing
    /// this to be mutable"), `.clone()`, `move` before a closure, ..
    pub suggestions: Vec<(String, Vec<(MirSpan, String)>, Applicability)>,
}
```

Where rustc gets them: `compiler/rustc_borrowck/src/borrowck_errors.rs` (the `cannot_*`
builders), `diagnostics/conflict_errors.rs` (`report_conflicting_borrow`,
`report_use_of_moved_or_uninitialized`, `report_borrowed_value_does_not_live_long_enough`),
`diagnostics/explain_borrow.rs` (`explain_why_borrow_contains_point`: the "later used here"
labels), `diagnostics/mutability_errors.rs`, `diagnostics/move_errors.rs`,
`diagnostics/region_errors.rs` (E0700's hidden type, captured lifetime and opaque spans). The
checker has all of it where it reports (the issued loan of `report_conflicting_borrow`, the move
outs of E0382, the assigned span of `report_illegal_reassignment`).

Why: `lower/borrowck_error.rs` gives rustc's code, message and primary span, and nothing on
the other spans: rustc's labels depend on facts it is not given (both borrows at one span in a
loop, the field a borrow goes "via", the first assignment), so writing them from the message
would be guessing. Tests: `tests/lints/mir/borrowck.rs` checks code, place and message of E0499,
E0502, E0503, E0505, E0596, E0597, E0384 now, and should check their whole `.stderr` once these
are given.

Also: the request 5 above (`BorrowckSkipped` always given) is what `lower/borrowck_skipped.rs`
needs; it writes ``blaze cannot borrow-check `<path>`: <reason>`` at the function's signature.

> forwarded to db: lane db is still running (`crates/hir-ty-borrowck/`, `crates/hir/src/diagnostics.rs`).

## diagnostics-lints: what the const checker refuses, by kind (codegen, `crates/hir-ty-mir/src/mir/const_check.rs`)

What, first: `ConstCheckDiagnostic` says what it is, not only its text:

```rust
pub enum ConstCheckKind {
    NonConstFn { callee: FunctionId, descr: &'static str, path: String },
    NonConstIntrinsic { name: String },
    FnPointerCall,
    InlineAsm,
    PointerToInt,
    PointerComparison,
    // and the ones below
}
pub struct ConstCheckDiagnostic { pub kind: ConstCheckKind, pub context: ConstContext, pub code: &'static str, pub span: MirSpan, pub message: String }
```

Why: `lower/const_check_error.rs` reads the kind back from the message text to add rustc's notes
(the "function `f` is not const" note at its signature, "calls in constants are limited to ..",
the `LazyLock` note in a static).

Then what rustc refuses that the const checker doesn't (each a program blaze accepts and rustc
rejects; `compiler/rustc_const_eval/src/check_consts/ops.rs`, `build_error_for_const_call`):

- a call of a closure (`NonConstClosure`: "cannot call non-const closure in constant functions"
  with "closures need an RFC before allowed to be called in constant functions";
  `tests/ui/consts/issue-56164.rs`), of an operator's trait method (`NonConstOperator`), a deref
  coercion (`NonConstDerefCoercion`), the desugarings of `for` (`NonConstForLoopIntoIter`), `?`
  (`NonConstQuestionBranch`, `NonConstQuestionFromResidual`), `try` blocks and `.await`, and
  `format_args!` (`NonConstFmtMacroCall`);
- a conditionally-const call (a trait method in a const context) without
  `#![feature(const_trait_impl)]`: E0658, and the note "`f` is not const because trait `Tr` is
  not const" with the suggestion to make the trait const;
- const stability (`check_callee_stability`, #1440): "`f` is not yet stable as a const fn" and
  the `const_*` features the nightly asks for (`const_atomic` and the rest, docs/const.md);
- anonymous consts: an array length (`[0; (&0 as *const i32) as usize]`,
  `tests/ui/consts/issue-52023-array-size-pointer-cast.rs`), a const generic argument and an
  inline `const { .. }` are not const checked (`hir` runs `const_check` on `DefWithBodyId`s only);
- the path of a generic callee as rustc's `def_path_str_with_args` prints it (`Vec::<u8>::new`).

> forwarded to codegen: `crates/hir-ty-mir/src/mir/const_check.rs` is lane codegen's.

## diagnostics-lints: `crate::tests::ui` passes compiletest's flags (the parent, `diagnostics`, `src/tests.rs`)

What: `ui` and `ui_with` give the crate, before the options' own lint flags, what compiletest
gives a UI test that is not `run-pass` (`src/tools/compiletest/src/runtest.rs`,
`make_compile_args`): `-A unused -W unused_attributes -A internal_features -A
incomplete_features -A unused_parens -A unused_braces`. Why: every `.stderr` of rustc's was
made with them, so a test whose source has an unused variable or an `unsafe` block with nothing
unsafe in it has no warning for it in its `.stderr` (`tests/ui/consts/issue-18294.rs`).

> forwarded to diagnostics: its own parent's `src/tests.rs`.

## diagnostics-lints-syntax-early-macros: the `;` an expression's expansion ends in, and which calls are expressions (db, `crates/hir-expand`, `crates/mbe`)

1. A tracked query `MacroCallId::dropped_semicolon(self, db) -> Option<span::Span>` in
   `crates/hir-expand/src/lib.rs`: the span of the `;` `without_last_semicolon` drops from the
   expansion (computed in `parse_macro_expansion` from the `macro_expand` token tree it already
   has). Why: `semicolon_in_expressions_from_macros` (deny) is linted at that `;`, which is not
   in the expansion's syntax tree. Until then `crates/blaze-diagnostics/src/lints/syntax/early/
   macros.rs` (`dropped_semicolon`) expands the call a second time through the public
   `decl_macro_expander(..).expand(..)`; with the query it calls that instead.
2. `without_last_semicolon` only for `MacroDefKind::Declarative`: rustc drops the `;` only for a
   `macro_rules!`/`macro` expansion (`ParserAnyMacro::make` in
   `rustc_expand/src/mbe/macro_rules.rs`); a proc macro's or a builtin's expansion that ends in
   `;` in expression position is an error there ("macro expansion ignores `;`"), so blaze
   accepts what rustc rejects.
3. `ExpandTo::from_call_site`: a block's trailing `m!()` / `m![]` (a `MACRO_EXPR` that is the
   `STMT_LIST`'s tail, not braced) is an expression in rustc (`parse_stmt_mac` in
   `rustc_parse/src/parser/stmt.rs`: only `m! {}` or a call followed by `;`/EOF is a statement
   macro), today `ExpandTo::Statements`. So `macro_rules! m { () => { let x = 1; x } }` and
   `fn f() -> i32 { m!() }` builds under blaze and is an error under rustc ("expected
   expression, found `let` statement"), and `{ m!() }` with an expansion `a; b` too ("macro
   expansion ignores `b`"). `macros.rs` lints the trailing case from either shape.
4. `crates/mbe/src/parser.rs` `eat_fragment_kind`: "missing fragment specifier" carries no span,
   and parsing stops at the first. rustc (`rustc_expand/src/mbe/quoted.rs`, `parse`) gives one
   error per `$name` without `:kind`, spanned `$name` (or `$name:` when the `:` is there),
   with the suggestion `:spec` after it; `hir::MacroDefError` needs the range of each (a
   `Vec<(TextRange, TextRange)>`: the span and where `:spec` goes) for
   `crates/blaze-diagnostics/src/lower/macro_def_error.rs` to give rustc's text.

> forwarded to db: lane db is still running: 1 to 3 (`crates/hir-expand/`). > forwarded to format: 4 (`crates/mbe/src/parser.rs`).

## diagnostics-lints-bodies: what follows a call of an uninhabited type (codegen, `crates/hir-ty-mir`; db for the field in `crates/hir-ty/src/mir.rs`)

rustc's `LintAndRemoveUninhabited` (`compiler/rustc_mir_transform/src/lint_and_remove_uninhabited.rs`,
run on the built MIR in `mir_promoted`) lints `unreachable_code` after a call whose type is
uninhabited but not `!` ("unreachable expression"/"unreachable definition", "any code
following this expression is unreachable", "this expression has type `Void`, which is
uninhabited"), then removes the call's return edge. hir-ty-mir's lowering already gives such a
call no target (`lower_call`'s `is_uninhabited`) and lowers nothing after it, so only lowering
knows what rustc's `find_unreachable_code_from` would find in the target block. What, in
`crates/hir-ty/src/mir.rs`:

```rust
/// What `LintAndRemoveUninhabited` lints: a call of a type uninhabited from the body's module
/// that is not `!`, in a function whose return type is inhabited, and the first statement of
/// user code its target block would have had (`find_unreachable_code_from`: skipping the `()`
/// and return-place assignments, `StorageLive`/`StorageDead`, and a `Goto`/`Drop` into a block
/// only it leads to).
pub struct UnreachableAfterCall {
    /// The call (`orig`).
    pub call: MirSpan,
    /// What would have come next (`target_loc.span`).
    pub next: MirSpan,
    /// "definition" for a `FakeRead` (a `let`), else "expression".
    pub descr: &'static str,
    pub ty: StoredTy,
}

pub struct MirBody<'db> {
    /// In a built body, in the order of the calls' blocks.
    pub unreachable_after_calls: Vec<UnreachableAfterCall>,
    ..
}
```

Why: the lint depends on the MIR lowering does not build.
`crates/blaze-diagnostics/src/lints/bodies/reachable.rs` (`after_calls`) lowers it as written
above, its type printed through `hir::Type::derived_from_next_solver` (requested by
`diagnostics-bodies`).

> forwarded to codegen (`crates/hir-ty-mir`) and db, both still running.

## diagnostics-bodies-types: why the types had to agree, and what a bound was needed for (db, `crates/hir-ty`, `crates/hir/src/diagnostics.rs`)

From `diagnostics-bodies-types`. Seven changes, each for a form rustc gives that the lowering in
`crates/blaze-diagnostics/src/lower/{type_mismatch,unimplemented_trait,closure_kind_mismatch,type_must_be_known}.rs`
can only guess at from the syntax today:

1. The cause on `TypeMismatch`, `UnimplementedTrait` and `ClosureKindMismatch`: rustc's
   `ObligationCauseCode` as far as diagnostics read it: `WhereClauseInExpr { item, bound_span,
   arg_index }` (the callee, the written bound that failed, and the argument it is of),
   `ImplDerived { impl_id }` for each entry of `parent_trait_predicates` (the impl that needed
   the bound), `SizedArgumentType`, `BlockTailExpression`, `ReturnValue`, `IfExpression`,
   `MatchExpressionArm { prior_arms }`, `LetType`. Why: today the lowering finds the callee and
   its bound again from the syntax (`Cause` and `where_clause_note` in
   `lower/unimplemented_trait.rs`) and gives no "required by a bound in `f`" note when `f` writes
   two bounds of the trait. For a local impl it gives "required for `X` to implement `Y`" as a
   plain note where rustc points at the impl's header with "unsatisfied trait bound introduced
   here" (`suggestions.rs`, `ImplDerived`), because the impl is not known.
2. The types of `TypeMismatch` and `InvalidRangePatType`-like diagnostics as inference had them
   when it found the error, before integer and float variables fall back. Why: rustc says
   "expected integer, found `&str`" and "this is found to be of type `{integer}`" (and decides
   `1`'s type from the expected one before `could_remove_semicolon`). After writeback hir gives
   `i32`. `tests/bodies/types.rs`, `match_arms_of_incompatible_types_skip_the_diverging_one`,
   is ignored until then.
3. Ambiguity errors: `SolverDiagnostic::from_fulfillment_error` (`crates/hir-ty/src/solver_errors.rs`)
   drops every error but `Select(Unimplemented)`, so rustc's E0283 ("type annotations needed:
   cannot satisfy `_: Trait`") and E0284 are never reported, and a body that has one still
   builds. Give `TypeMustBeKnown` with the ambiguous predicate, and a kind of E0282/E0283/E0284.
4. The origin of an inference variable on `TypeMustBeKnown`: the generic parameter it was made
   for, and the item that parameter is declared on. Why: rustc's ", where the type for type
   parameter `T` is specified", "cannot infer type for type parameter `T` declared on the
   function `f`" and the turbofish suggestion (`need_type_info.rs`, `InferSourceKind::GenericArg`)
   all name it. Today `type_must_be_known.rs` gives the suggestion's message without that clause.
5. `hir::Closure::kind_origin(db) -> Option<(InFile<ExprOrPatPtr>, ClosureCapture)>`: rustc's
   `closure_kind_origins`, the use that made the closure `FnOnce` or `FnMut`. Today
   `closure_kind_mismatch.rs` takes the first capture that moves or mutates, and only of a closure
   that is not `move`.
6. The closure on `ClosureKindMismatch`: its expression (`InFile<AstPtr<ast::ClosureExpr>>`).
   Today it is found from the argument (a closure written there, or a local a `let` binds one
   to), and the error is at the call when neither holds.
7. #1522 (an `assert_eq!` of a type that does not implement `Debug` gave no error):
   `tests/bodies/types.rs`, `an_unmet_bound_inside_an_assert_eq_like_macro_is_reported`, is the
   regression test. What the reading found, not yet the cause: `add_required_obligations_for_value_path`
   registers the callee's bounds with the path's span (not dummy), derived causes reach
   `find_best_leaf_obligation`, `expr_syntax` maps macro-expanded expressions, and nothing
   treats `assert_eq!` apart. `hir-ty-borrowck`'s `NotChecked::Unproven` ("a where clause of a
   call does not hold: rustc's E0277, which type checking did not report") is the place a missed
   one shows; a body skipped with it must lower to an error, not pass. If the test passes the
   cause is in core's `assert_eq!` itself (the `?Sized` bound through `&*left_val`, or
   `#[track_caller]`), to be tried with std's macro in a VM.

> forwarded to db: lane db is still running (`crates/hir-ty`, `crates/hir`).

## diagnostics-lints-mir-unsafe: what rustc's unsafety checking finds that `hir::MissingUnsafe` does not give (db, `crates/hir-ide/src/diagnostics/unsafe_check.rs`, `crates/hir/src/diagnostics.rs`, `crates/hir-def/src/signatures.rs`)

`crates/blaze-diagnostics/src/lower/missing_unsafe.rs` lowers every `MissingUnsafe` as rustc's
`check_unsafety` gives it. What it cannot, because hir does not find or say it
(`compiler/rustc_mir_build/src/check_unsafety.rs`):

1. Operations hir does not find, each a program rustc rejects with E0133 and blaze accepts
   (superset-of-rust). New `UnsafetyReason` variants, found where rustc finds them:
   - `UnsafeBinderCast`: `ExprKind::{Place,Value}UnwrapUnsafeBinder`, `WrapUnsafeBinder`
     (`unsafe<'a> T`, feature `unsafe_binders`).
   - `UseOfUnsafeField` (reading or matching an `unsafe` field, feature `unsafe_fields`) and
     `InitializingTypeWithUnsafeField` (a struct expression of a variant with one).
   - `MutationOfLayoutConstrainedField` / `BorrowOfLayoutConstrainedField`: a `ref mut`
     binding, or a `ref` binding of a non-`Freeze` type, inside a pattern of a type with
     `#[rustc_layout_scalar_valid_range_*]` (`inside_adt` in rustc's `visit_pat`).
   With the variant, give its span as rustc does (`pat.span` / `expr.span`).
2. A call of a `#[rustc_deprecated_safe_2024]` function in the body of an `unsafe fn`, outside a
   block, before 2024: rustc lints it `deprecated_safe_2024` when `unsafe_op_in_unsafe_fn` is
   not allowed there (`requires_unsafe`, `SafetyContext::UnsafeFn`). `missing_unsafe` drops it
   (`!is_unsafe`). Give it as `MissingUnsafe { lint: DeprecatedSafe2024, .. }` with
   `in_unsafe_fn: bool`, so the lowering can apply rustc's gate.
3. An operation in the label block of `asm!` inside an `unsafe fn` is a hard error in rustc
   (`in_safety_context(SafetyContext::Safe)` for `InlineAsmOperand::Label`); hir gives it as
   `UnsafeLint::UnsafeOpInUnsafeFn`. Give `HardError` there.
4. rustc's `suggest_unsafe_block`: its note "an unsafe function restricts its caller" is on the
   first operation it lints in the function (and in each closure, from the flag where that
   starts). The lowering finds it by scanning `Semantics::get_unsafe_ops` of the body for
   each diagnostic, O(ops²) per `unsafe fn`. Give `MissingUnsafe::first: bool` (the first
   in rustc's THIR order in its body or closure, counting the scopes around), or make
   `missing_unsafe` a tracked query the lowering can read once.
5. Order: `UnsafeVisitor` walks a block's `let` patterns before its statements' expressions;
   rustc visits a `let`'s initializer, then its pattern, in order. Errors of union patterns
   come out of rustc's order. Walk in THIR order.
6. `crates/hir-def/src/signatures.rs`: an `unsafe fn` with `#[rustc_deprecated_safe_2024]` gets
   `DEPRECATED_SAFE_2024` instead of `UNSAFE`, so its own body is checked as a safe function's
   (std's `env::set_var` would get E0133 for what is `unsafe_op_in_unsafe_fn` in rustc). Keep
   `UNSAFE` too, and test `is_deprecated_safe_2024` first where calls are checked.
   Status: `signatures.rs` part done by speed-hir-defs-sigs (both flags set, test
   `flags_from_attrs_as_one_if_per_flag`); the call-site reorder in `hir-ty/src/utils.rs` is
   requested in `v2/requests/db-ty-lower.md`.
7. `crates/hir-ty/src/target_feature.rs` merges the implications of a feature across
   architectures; rustc's `is_target_feature_call_safe` uses the target's. A call rustc
   rejects can pass.

> forwarded to db: 6 (`hir-def/src/signatures.rs`), 7 (`hir-ty/src/target_feature.rs`) and the `hir::MissingUnsafe` fields. > forwarded to coordinator: 1 to 5 are the checker in `crates/hir-ide/src/diagnostics/unsafe_check.rs`, which `crates.md` removes at integration; porting rustc's `check_unsafety` into it would put new semantics in a crate being deleted (rule semantics-in-front-end), so the coordinator names the front-end crate it moves to first, then the port goes there.

## diagnostics-lints-syntax-attrs-features: one feature table for the feature lints and `checks/features.rs` (the parents, `lints.rs`, `lints/syntax.rs`, `lints/syntax/attrs.rs`; `diagnostics-items`)

What: rustc's table of language features is at
`crates/blaze-diagnostics/src/lints/syntax/attrs/features/table.rs` (`pub(crate) mod table`):
`lang(name) -> Option<Lang { name, kind: Kind::{Unstable, Incomplete, Internal, Accepted,
Removed}, since, issue }>`, `removed(name) -> Option<(reason, pull)>`, `is_lang`, `incomplete`,
`internal`, `issue`. All 570 features of `compiler/rustc_feature/src/{unstable,accepted,removed}.rs`
(rust-lang/rust@ad3d0bc1, July 31) with since and issue, and the 103 removed ones' reasons and
pull requests, generated by `features/table.py` into `features/table/lang.rs`. It has what E0554
(stable features and their `since`), E0557 (removed, reason, pull) and E0635 (which names are
lang features; library ones still need std's `#[unstable]`s) need.
Make it reachable from `crate::checks`: `pub(crate) mod features;` in `lints/syntax/attrs.rs`,
`pub(crate) mod attrs;` in `lints/syntax.rs`, `pub(crate) mod syntax;` in `lints.rs` (or a
`pub(crate) use` of `table` at `crate::lints`). Why: one table, not two (`diagnostics-items`
writes `checks/features.rs` next and should use it rather than keep its own).
Also: the table is from the July checkout, not `nightly-2026-10-27`; regenerate it with
`table.py` from a checkout of the nightly's commit when there is one
(`PYTHONPATH=python/typed-input/src python3 .../features/table.py '{"rust": "<checkout>", "out": ".../features/table/lang.rs"}'`).

> forwarded to diagnostics: the module paths and `checks/features.rs` are its own (parents and diagnostics-items).

## diagnostics-lints-syntax-early-validation: camel case helpers, a fragment's span, and the errors next to my lints

1. `crates/blaze-diagnostics/src/lower/incorrect_case.rs` (diagnostics-items or the parent):
   make `is_upper_camel_case` and `to_upper_camel_case` `pub(crate)`.
   `lints/syntax/early/validation.rs` imports them for `non_camel_case_types` of type
   parameters, so both halves of the lint use one copy of rustc's rule (`nonstandard_style.rs`).
2. Same file, `non_camel_case_types` as the declarations check gives it, so the two halves
   add up to rustc's `NonCamelCaseTypes`:
   - A trait alias is "trait alias `x`" in rustc (`check_item`, `ItemKind::TraitAlias`); the
     lowering prints "trait".
   - `extern { type foo; }`: hir's decl_check lints a foreign type, but rustc's early pass has
     no `check_foreign_item` for this lint, so rustc gives nothing. Skip a `TYPE_ALIAS` whose
     parent is an `EXTERN_ITEM_LIST`.
3. db (`crates/hir-expand`): the span of a `$var` fragment substituted by `macro_rules`.
   rustc gives the invisible-delimited group the span of `$f` in the definition, so
   `break '_l $f` in tests/ui/parser/issues/issue-87812.rs is spanned `break '_l $f` with the
   suggestion `($f)`. The expansion span map gives each token of the fragment the call site's
   span, so the lint's end (and the `)` it suggests) lands at the call. Needed by
   `tests/lints/syntax/early/validation.rs::a_labeled_break_of_a_block_fragment_is_linted_at_the_macro_definition`.
4. Errors that rustc gives next to these lints and nobody lowers yet (parent: assign them):
   - E0642 "patterns aren't allowed in functions without bodies" (`ast_validation.rs`,
     `check_decl_no_pat`, a non-ident pattern of a bodiless fn); needed by
     `a_mut_parameter_of_a_function_without_a_body_is_linted_after_the_pattern_error`.
   - E0561 (patterns in fn pointer types) and E0130 (in foreign fns) from the same check.
   - "extern blocks must be unsafe" (Rust 2024, `ast_validation.rs` `visit_item`).
   - "where clauses are not allowed before the type for type aliases" (a free type alias).
   - format (parser): `LabeledLoopInBreak`, "parentheses are required around this expression
     to avoid confusion with a labeled break expression" (`parse_expr_break`). Our parser
     leaves `break 'a: loop {}` without a value; needed by
     `a_labeled_break_of_an_unlabeled_loop_is_linted_after_the_parse_errors`.

> forwarded to diagnostics: 1, 2 and 4 are its own (`lower/incorrect_case.rs`, the parent's assignments). > forwarded to db: 3 (`crates/hir-expand/`). > forwarded to format: 4's `LabeledLoopInBreak` (`crates/parser`).

## diagnostics-lints-mir-matches: what match checking gives, as rustc gives it (db, `crates/hir-ide/src/diagnostics/expr.rs`, `match_check.rs`, `match_check/pat_analysis.rs`, `crates/hir/src/diagnostics.rs`)

Each of 1, 4 and 5 is a program rustc refuses and blaze accepts (superset-of-rust); 2 and 3 are
text rustc gives that blaze can't. The lowering is `crates/blaze-diagnostics/src/lower/
missing_match_arms.rs` (E0004) and `non_exhaustive_let.rs` (E0005).

1. **Every pattern kind lowered.** `PatCtxt::lower_pattern` records an error, and
   `validate_match` / `check_non_exhaustive_let` then give nothing at all, for integer, char,
   str and float literals, ranges, slices, `&p`, `box p`, deref patterns and paths to consts
   (`PatternError::Unimplemented`, `UnresolvedVariant`); `hoist_witness_pat` has
   `unimplemented!()` for `IntRange`, `Slice` and `DerefPattern`. So `match 0 { 1 => () }`,
   `let 5 = 6;`, `match s { "a" => () }`, `match x { &E::A => () }` and `match v[..] { [] => () }`
   build. Port `rustc_mir_build/src/thir/pattern/mod.rs`'s lowering (consts evaluated to
   valtrees, `expanded_const` kept for `is_const_pat_that_looks_like_binding`). Tests:
   `tests/ui/pattern/usefulness/match-non-exhaustive.rs`, `non-exhaustive-match.rs`,
   `non-exhaustive-pattern-witness.rs`, `tests/ui/pattern/issue-106552.rs`.
2. **The witnesses, not their joined text.** Today `uncovered_patterns: String` is rustc's joined
   form of rust-analyzer's printing; the lowering reads it back against the scrutinee's type
   and reprints it rustc's way (`Uncovered::read`, `Witness::read`). That loses the 4th and 5th
   witness (rustc labels the variants of the first five: `ExcessiveEnum` in
   `non-exhaustive-pattern-witness.rs`) and can't tell `NonExhaustive` from `Wildcard` (both
   `_`). Wanted, on both `MissingMatchArms` and `NonExhaustiveLet`:
   `witnesses: Vec<String>` (every one, printed by a port of `rustc_pattern_analysis`'s
   `print_witness_pat`: `Enum::Variant` except `Option`/`Result`, `_` for `Never` without
   `never_patterns`, int ranges `i32::MIN..=0_i32` / `0_usize..`, slices, `box _`, unions'
   `..`), `variants: Vec<hir::EnumVariant>` (`maybe_point_at_variant` of the first five for
   the peeled scrutinee ADT), `special_tys: Vec<(hir::Type, SpecialTy)>` (`collect_special_tys`
   of the first witness: beyond-boundary `usize`/`isize`, `str`, foreign `#[non_exhaustive]`,
   uninhabited). The lowering then drops its reader.
3. **`would_be_exhaustive_without_guards: bool`** on `MissingMatchArms` (`check_match`
   re-analyzes with the guards off) for the note "match arms with guards don't count towards
   exhaustivity".
4. **Inhabitedness.** `hir::Type::is_inhabited_from(&self, db, hir::Module) -> bool` (hir-ty's
   `inhabitedness`), for "references are always considered inhabited"
   (`tests/ui/pattern/usefulness/issue-78123-non-exhaustive-reference.rs`), and on
   `NonExhaustiveLet` `privately_uninhabited: bool` (rustc's `witness_1_is_privately_uninhabited`:
   the first witness's variant inhabited from the module but not ignoring visibility) for
   "pattern `X` is currently uninhabited, but this variant contains private fields which may
   become inhabited in the future".
5. **The other irrefutable bindings.** Only `let` statements with an initializer are checked.
   rustc's `check_binding_is_irrefutable` also checks function parameters ("refutable pattern
   in function argument"), closure parameters ("closure argument"), `for` patterns ("`for` loop
   binding"), `let` without an initializer (`let Some(x);`, where `check_non_exhaustive_let`
   returns at `initializer?`) and destructuring assignments ("assignment", with the note
   "destructuring assignments require an \"irrefutable pattern\" .."). Wanted:
   `NonExhaustiveLet { pat, origin: BindingOrigin { Let, FnParam, ClosureParam, ForLoop,
   Assignment }, has_initializer: bool, .. }`. Tests:
   `tests/ui/pattern/usefulness/refutable-pattern-in-fn-arg.rs`,
   `tests/ui/pattern/refutable-pattern-for-loop-15381.rs`,
   `tests/ui/pattern/usefulness/refutable-pattern-errors.rs`.

> forwarded to db: the fields on `hir::MissingMatchArms`/`NonExhaustiveLet` and `hir::Type::is_inhabited_from` (`crates/hir/`). > forwarded to coordinator: 1 to 5's checking is `crates/hir-ide/src/diagnostics/{expr,match_check}.rs`, which `crates.md` removes at integration; porting rustc's pattern lowering into it would put new semantics in a crate being deleted (rule semantics-in-front-end), so the coordinator names the front-end crate match checking moves to first, then the port goes there.

## speed-hir-store: blaze-diagnostics onto the HIR column views (from speed-hir-store-tools)

From speed-hir-store-tools, 2026-10-29. 63 sites in 7 files of `crates/blaze-diagnostics/src/`
(`ide-diagnostics` needs none, below). The rule is "## format-hir: expressions and patterns
become columns" in `v2/requests/format.md` (contract:
`~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md`). These land together with the
format-hir change. `BodyCx::store()` returns `&'db ExpressionStore`, so every view is
`Expr<'db>`/`Pat<'db>` and the slices and `&Literal` that `util::call_args`, `util::called_item`
and `util::literal` return keep their lifetimes; no borrow changes (each walker copies `b` out
of `self` before the `&mut self` calls, as today). `Statement`, `MatchArm`, `RecordLitField` and
`AsmOperand` stay owned types reached through `&`, so `*expr`/`*pat`/`*it` on *their* fields
stay: only the view's own fields lose the `*`. No site builds an `Expr`/`Pat`, uses the builder,
or stores, hashes or compares a view.

Files that import the HIR types or name `ExprId`/`PatId` and need no change: every
`lower/*.rs` (their `Expr`/`Pat` are `syntax::ast`'s; `mismatched_tuple_struct_pat_arg_count.rs`'s
`Pat<'db>` is a local struct), `checks/defs/discriminants.rs`, `lints/names/items/naming.rs`,
`lints/syntax/**` but `early/labels.rs`.

> forwarded to diagnostics: `crates/blaze-diagnostics/` is its own; `hir-def` has the expression and pattern views now.

### crates/blaze-diagnostics/src/lints/bodies.rs  (23 sites)
- ~L121  `if matches!(body[body.root_expr()], Expr::Missing) && body.params.is_empty() {`  ->  `if matches!(body.expr(body.root_expr()), Expr::Missing) && body.params.is_empty() {`
- ~L310 (in `fn child_exprs`) and ~L490 (in `fn expr_children`), the only two in the file:  `match &store[expr] {`  ->  `match store.expr(expr) {`
- `fn child_exprs`:
  - ~L311  `Expr::Let { pat: p, expr } => {\n            out.push(*expr);\n            pat(*p, out);`  ->  `Expr::Let { pat: p, expr } => {\n            out.push(expr);\n            pat(p, out);`
  - ~L327  `out.extend(*tail);`  ->  `out.extend(tail);`
  - ~L329  `Expr::Match { expr, arms } => {\n            out.push(*expr);`  ->  `Expr::Match { expr, arms } => {\n            out.push(expr);`
  - ~L341  `out.push(*body);`  ->  `out.push(body);`
  - ~L344  `pat(*target, out);\n            out.push(*value);`  ->  `pat(target, out);\n            out.push(value);`
  - ~L347  `Expr::Array(Array::Repeat { initializer, .. }) => out.push(*initializer),`  ->  `Expr::Array(Array::Repeat { initializer, .. }) => out.push(initializer),`
  - unchanged: the `Statement::Let`/`Statement::Expr` arms (`out.extend(*initializer)`, `pat(*p, out)` at ~L320, `out.push(*expr)` at ~L323), `pat(*arg, out)` over `args.iter()`, and `arm.pat`/`arm.guard`/`arm.expr`.
- `fn expr_children`:
  - ~L491  `Expr::Let { pat, expr } => {\n                self.expr(*expr);\n                self.pat(*pat);`  ->  `Expr::Let { pat, expr } => {\n                self.expr(expr);\n                self.pat(pat);`
  - ~L521  `self.expr(*tail);`  ->  `self.expr(tail);`
  - ~L525  `self.expr(*scrutinee);`  ->  `self.expr(scrutinee);`
  - ~L541  `self.expr(*body);`  ->  `self.expr(body);`
  - ~L544  `self.pat(*target);\n                self.expr(*value);`  ->  `self.pat(target);\n                self.expr(value);`
  - ~L547  `Expr::Array(Array::Repeat { initializer, .. }) => self.expr(*initializer),`  ->  `Expr::Array(Array::Repeat { initializer, .. }) => self.expr(initializer),`
  - unchanged: the `Statement` arms (`self.expr(*it)` ~L509/~L513, `self.pat(*pat)` ~L511, `self.expr(*expr)` ~L516) and `self.pat(*arg)` over `args.iter()`.
- `fn pat`:
  - ~L564  `match &store[pat] {`  ->  `match store.pat(pat) {`
  - ~L565  `Pat::Lit(expr) | Pat::Expr(expr) => self.expr(*expr),`  ->  `Pat::Lit(expr) | Pat::Expr(expr) => self.expr(expr),`
  - ~L567  `if let Some(it) = start {\n                    self.expr(*it);`  ->  `if let Some(it) = start {\n                    self.expr(it);`
  - ~L570  `if let Some(it) = end {\n                    self.expr(*it);`  ->  `if let Some(it) = end {\n                    self.expr(it);`

### crates/blaze-diagnostics/src/lints/bodies/reachable.rs  (6 sites)
- ~L74  `let span = match &store[expr] {`  ->  `let span = match store.expr(expr) {`
- ~L75  `Expr::Call { callee, .. } => b.expr_span(*callee)?,`  ->  `Expr::Call { callee, .. } => b.expr_span(callee)?,`
- ~L86  `let Expr::Block { statements, .. } = &store[block] else { return None };`  ->  `let Expr::Block { statements, .. } = store.expr(block) else { return None };`
- ~L91  `let Expr::Match { arms, .. } = &store[match_expr] else { return None };`  ->  `let Expr::Match { arms, .. } = store.expr(match_expr) else { return None };`
- ~L97  `let Expr::If { condition, then_branch, .. } = store[if_expr] else { return None };`  ->  `let Expr::If { condition, then_branch, .. } = store.expr(if_expr) else { return None };`
- ~L101  `let Expr::Let { expr, .. } = store[let_expr] else { return None };`  ->  `let Expr::Let { expr, .. } = store.expr(let_expr) else { return None };`
- `fn statement_node` (`Statement::Expr { expr, .. } => (b.expr_node(*expr)?, false)`) is unchanged.

### crates/blaze-diagnostics/src/lints/bodies/unsafety.rs  (10 sites)
- ~L108 (in `fn expr`) and ~L144 (in `fn children`), the only two in the file:  `match &store[expr] {`  ->  `match store.expr(expr) {`
- ~L153  `Expr::Let { pat, .. } => pats.push(*pat),`  ->  `Expr::Let { pat, .. } => pats.push(pat),`
- ~L155  `Expr::Assignment { target, .. } => pats.push(*target),`  ->  `Expr::Assignment { target, .. } => pats.push(target),`
- ~L173  `match &b.store()[pat] {`  ->  `match b.store().pat(pat) {`
- ~L174  `Pat::Expr(expr) | Pat::Lit(expr) => self.expr(*expr),`  ->  `Pat::Expr(expr) | Pat::Lit(expr) => self.expr(expr),`
- ~L176  `if let Some(it) = start {\n                    self.expr(*it);`  ->  `if let Some(it) = start {\n                    self.expr(it);`
- ~L179  `if let Some(it) = end {\n                    self.expr(*it);`  ->  `if let Some(it) = end {\n                    self.expr(it);`
- ~L205  `let callee = match &b.store()[expr] {`  ->  `let callee = match b.store().expr(expr) {`
- ~L206  `Expr::Call { callee, .. } => match b.infer.expr_ty(*callee).kind() {`  ->  `Expr::Call { callee, .. } => match b.infer.expr_ty(callee).kind() {`
- unchanged: the `Expr::InlineAsm(asm)` arm (`asm: &InlineAsm`; its `AsmOperand` `*expr`/`*in_expr`/`*it`/`*label` stay), `Statement::Let { pat, .. } => pats.push(*pat)` at ~L148, `arms.iter().map(|it| it.pat)`, `args.iter().copied()`.

### crates/blaze-diagnostics/src/lints/bodies/types/limits.rs  (7 sites)
- ~L114  `match &b.store()[expr] {`  ->  `match b.store().expr(expr) {`
- ~L124  `self.last_visited_negation = Some(Negation { span, negated: *operand });`  ->  `self.last_visited_negation = Some(Negation { span, negated: operand });`
- ~L127  `&Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(op)) } => {`  ->  `Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(op)) } => {` (a `&` pattern over the old reference)
- ~L151  `out.extend(lint_wide_pointer(b, expr, &span, cmpop, *receiver, args[0]));`  ->  `out.extend(lint_wide_pointer(b, expr, &span, cmpop, receiver, args[0]));`
- ~L152  `out.extend(lint_fn_pointer(b, expr, &span, cmpop, *receiver, args[0]));`  ->  `out.extend(lint_fn_pointer(b, expr, &span, cmpop, receiver, args[0]));`
- ~L177  `let is_lit = |it: ExprId| matches!(b.store()[it], Expr::Literal(_));`  ->  `let is_lit = |it: ExprId| matches!(b.store().expr(it), Expr::Literal(_));`
- ~L312  `let via_method_call = matches!(&b.store()[e], Expr::MethodCall { .. } | Expr::Call { .. });`  ->  `let via_method_call = matches!(b.store().expr(e), Expr::MethodCall { .. } | Expr::Call { .. });`

### crates/blaze-diagnostics/src/lints/bodies/types/limits/literal.rs  (3 sites)
- ~L320  `b.exprs().iter().copied().find(|&it| matches!(&b.store()[it], Expr::Cast { expr: inner, .. } if *inner == expr))`  ->  `b.exprs().iter().copied().find(|&it| matches!(b.store().expr(it), Expr::Cast { expr: inner, .. } if inner == expr))`
- ~L342  `let range = b.exprs().iter().copied().find(|&it| match &b.store()[it] {`  ->  `let range = b.exprs().iter().copied().find(|&it| match b.store().expr(it) {`
- ~L347  `let Expr::RecordLit { fields, .. } = &b.store()[range] else { return false };`  ->  `let Expr::RecordLit { fields, .. } = b.store().expr(range) else { return false };` (`let [start, end] = &fields[..]` below is unchanged)

### crates/blaze-diagnostics/src/lints/bodies/types/util.rs  (12 sites)
- ~L74  `let Expr::Call { callee, .. } = &b.store()[call] else { return None };`  ->  `let Expr::Call { callee, .. } = b.store().expr(call) else { return None };`
- ~L75  `let Expr::Path(_) = &b.store()[*callee] else { return None };`  ->  `let Expr::Path(_) = b.store().expr(callee) else { return None };`
- ~L76  `match b.infer.expr_ty(*callee).kind() {`  ->  `match b.infer.expr_ty(callee).kind() {`
- ~L77  `=> Some((def, *callee)),`  ->  `=> Some((def, callee)),`
- ~L91 (in `fn call_args`)  `match &b.store()[call] {`  ->  `match b.store().expr(call) {` (`Expr::Call { args, .. } => Some(args)` is unchanged: `args: &'db [ExprId]`)
- ~L100  `let Expr::MethodCall { .. } = &b.store()[expr] else { return None };`  ->  `let Expr::MethodCall { .. } = b.store().expr(expr) else { return None };`
- ~L107  `let Expr::Path(path) = &b.store()[expr] else { return None };`  ->  `let Expr::Path(path) = b.store().expr(expr) else { return None };`
- ~L133  `while let Expr::Block { tail: Some(tail), .. } = &b.store()[expr] {\n        expr = *tail;`  ->  `while let Expr::Block { tail: Some(tail), .. } = b.store().expr(expr) {\n        expr = tail;`
- ~L141  `while let Expr::Ref { expr: inner, .. } = &b.store()[expr] {\n        expr = *inner;`  ->  `while let Expr::Ref { expr: inner, .. } = b.store().expr(expr) {\n        expr = inner;`
- ~L149 (in `fn literal`)  `match &b.store()[expr] {`  ->  `match b.store().expr(expr) {` (`Expr::Literal(it) => Some(it)` is unchanged: `it: &'db Literal`)

### crates/blaze-diagnostics/src/lints/syntax/early/labels.rs  (2 sites)
- ~L42  `declared.push(*label)`  ->  `declared.push(label)`
- ~L45  `used.insert(*label);`  ->  `used.insert(label);`
- (`for (_, expr) in body.exprs() { match expr {` is unchanged: the item is `(ExprId, Expr<'_>)` and `label: Some(label)` now binds `LabelId` by value.)

### crates/ide-diagnostics/  (0 sites)
Its `handlers/` read diagnostics through `hir`'s public API and `syntax::ast`; none imports
`hir_def::hir` or indexes a store or body.

## diagnostics-lints-syntax-early: the parser's "the range pattern here has ambiguous interpretation"

What: rustc's parser error `AmbiguousRangePattern` (`compiler/rustc_parse/src/parser/pat.rs`,
`ban_pat_range_if_ambiguous`; message in `rustc_parse/src/diagnostics.rs`): after `&` or `box`,
a range pattern written with `..=` or `..` (not `...`) is an error "the range pattern here has
ambiguous interpretation" at the range pattern, with the verbose multipart suggestion "add
parentheses to clarify the precedence" (`(` before, `)` after). Phase: Parse.
Where: whoever lowers the parser's errors (`crates/parser` + `crates/blaze-diagnostics/src/lower/`);
nothing in this tree gives it now.
Why: `tests/lints/syntax/early/builtin.rs` (`range-inclusive-pattern-precedence.rs` and
`...precedence2.rs`, rustc's UI tests verbatim) expect it before the
`ellipsis_inclusive_range_patterns` warning.

> forwarded to format: `crates/parser` is lane format's.

## diagnostics-bodies-calls: the types of an error as of when it was found

What: `hir_ty::infer` keeps, in `MismatchedArgCount` (each argument's type and each formal
input), `BinaryOperatorCannotBeApplied`, `UnaryOperatorCannotBeApplied`, `CannotIndexInto` and
`CannotBeDereferenced`, the types as they were when type checking found the error, before
integer and float fallback, so an integer variable prints `{integer}`, not `i32`.
Where: `crates/hir-ty/src/infer.rs` (`InferenceDiagnostic`), `infer/op.rs`, `infer/expr.rs`,
`crates/hir/src/diagnostics.rs`.
Why: rustc reports these while type checking, before fallback: `binop/binop-typeck.rs`
("cannot add `{integer}` to `bool`"), `error-codes/E0057.rs` ("unexpected argument #2 of type
`{integer}`") and `argument-suggestions/basic.rs` ("found `{integer}`"), in
`tests/bodies/calls.rs` and `tests/bodies/calls/args.rs`.

> forwarded to db: lane db is still running (`crates/hir-ty/src/infer`, `crates/hir`).

## diagnostics-bodies-calls: `MismatchedArgCount` carries the call's callee and formal types

What: `hir::MismatchedArgCount` (and the E0308 "arguments to this function are incorrect" of a
call whose counts match) gives the callee (`hir::Callable` or the def), each formal input type,
each expected input type (`expected_inputs_for_expected_output`), each provided argument's type
and the compatibility rustc's `ArgMatrix` is built from (`can_coerce` of provided to expected,
then the formal type's `sup`), and whether the call is a method call or a call through
`Fn*`/`FnPtr`/a closure.
Where: `crates/hir-ty/src/infer/expr.rs` (`check_call_arguments`), `crates/hir/src/diagnostics.rs`.
Why: `lower/mismatched_arg_count.rs` re-resolves the callee with `Semantics` and decides
compatibility with `Type::could_coerce_to`, which is not rustc's check: a provided type that
coerces only through an expectation, or a generic formal input, gives other errors than rustc.

> forwarded to db: lane db is still running (`infer/expr.rs`, `hir/src/diagnostics.rs`).

## diagnostics-bodies-calls: `HasSource` for `hir::Closure`

What: `impl HasSource for hir::Closure` (its `ast::ClosureExpr`).
Where: `crates/hir/src/has_source.rs`.
Why: E0057's "closure defined here" note and the parameter labels of a closure callee need the
closure's span; `lower/mismatched_arg_count.rs` finds it only from its first parameter or from
the callee's local binding (`let f = || ..; f(1)`), so a closure without parameters reached
otherwise gets no note.

> forwarded to db: lane db is still running (`crates/hir/`).

## diagnostics-bodies-calls: the unmet predicates of an operator

What: `BinaryOperatorCannotBeApplied` and `UnaryOperatorCannotBeApplied` carry the fulfillment
errors of the operator's obligation (rustc's `errors` in `check_overloaded_binop`: the nested
predicates, e.g. `X: PartialEq` for `[X; 3] == [X; 3]`), the operator's expected output type,
and whether the method lookup failed on the trait or on a nested obligation.
Where: `crates/hir-ty/src/infer/op.rs`, `crates/hir/src/diagnostics.rs`.
Why: rustc's notes "an implementation of `PartialEq` might be missing for `X`", the derive
suggestion on the element type, "consider further restricting type parameter `T` with trait
`Eq`" (`binop/issue-93927.rs`), "consider introducing a `where` clause"
(`suggestions/invalid-bin-op.rs`) and `Output = ` in a bound suggestion come from those
predicates; `lower/binary_operator_cannot_be_applied.rs` knows only the operator's own trait.

> forwarded to db: lane db is still running (`crates/hir-ty`, `crates/hir`).

## diagnostics-bodies-calls: casts with unknown pointer kinds are E0641, not accepted

What: `hir_ty::infer::cast` gives `CastError::UnknownCastPtrKind` and `UnknownExprPtrKind`
(today commented out, so such casts check as valid) and `CastEnumDrop`,
`ForeignNonExhaustiveAdt`, plus, on `hir::InvalidCast`, the pointer kinds and metadata rustc's
`report_cast_error` reads (thin, slice length, vtable of which principal, the auto traits a
`dyn` cast adds).
Where: `crates/hir-ty/src/infer/cast.rs`, `crates/hir/src/diagnostics.rs`.
Why: rustc refuses `ptr as *const _` when the pointee is not yet known (E0641 "cannot cast to a
pointer of an unknown kind"); blaze accepting it is a program rustc refuses (superset-of-rust
allows accepting more only behind an opt-in). `lower/invalid_cast.rs` works the metadata out
from the types itself, which misses projections and opaque types.

> forwarded to db: lane db is still running (`infer/cast.rs`, `hir/src/diagnostics.rs`).

## diagnostics-bodies-calls: `InvalidLhsOfAssignment` says which assignment

What: `hir::InvalidLhsOfAssignment` carries the assignment expression and its operator (plain
`=` or which compound one).
Where: `crates/hir/src/diagnostics.rs`, `crates/hir-ty/src/infer/expr.rs`.
Why: E0070 and E0067 differ by it; `lower/invalid_lhs_of_assignment.rs` finds the enclosing
`BinExpr` from the syntax, which breaks for a left-hand side inside a macro expansion.

> forwarded to db: lane db is still running (`infer/expr.rs`, `hir/src/diagnostics.rs`).

## diagnostics-bodies-calls: `MethodCallIllegalSizedBound` carries the pick

What: `hir::MethodCallIllegalSizedBound` carries the method picked, the `Self: Sized` bound's
span, whether the receiver needs `&mut` (rustc's `needs_mut`) and the other candidates.
Where: `crates/hir-ty/src/method_resolution`, `crates/hir/src/diagnostics.rs`.
Why: rustc's "you need `&mut dyn Trait` instead of `&dyn Trait`" suggestion and its "a method
with a similar name exists" help need them; `lower/method_call_illegal_sized_bound.rs` gives
the error and the bound's label only.

> forwarded to db: the fields go on `InferenceDiagnostic::MethodCallIllegalSizedBound` (`infer.rs`) and `hir::MethodCallIllegalSizedBound`. The one line in items' `method_resolution.rs` (lane items is DONE; line 164, the `push_diagnostic`) db edits in the same change; `MethodError::IllegalSizedBound { candidates, needs_mut }` already has two of them.

## diagnostics-lints-syntax-delims: pinned reference and guard patterns in the syntax tree

What: `RefPat` gets a `pin` token with its `const`/`mut` (`&pin mut x`, `&pin const x`), and
a pattern node for guard patterns (`(x if cond)`, rustc's `PatKind::Guard`) under
`#![feature(guard_patterns)]`.
Where: `crates/parser` (patterns grammar), `crates/syntax` (`rust.ungram`, generated nodes);
owned by format.
Why: rustc's `unused_parens` keeps the parentheses of `&pin mut (x)` as it does `&(mut x)`,
and never lints those around a guard pattern
(`tests/ui/lint/unused-parens-pinned-reference-patterns.rs`,
`tests/ui/lint/unused/unused_parens/parens-around-guard-patterns-not-unused.rs`);
`lints/syntax/delims.rs` cannot tell them from what the parser makes of them now.

> forwarded to format: `crates/parser`, `crates/syntax` are lane format's.

## diagnostics-lints-syntax-early-macros-asm: `ra-ap-rustc_parse_format` for blaze-diagnostics

What: `ra-ap-rustc_parse_format.workspace = true` under `[dependencies]` of
`crates/blaze-diagnostics/Cargo.toml` (the workspace already has it, 0.166).
Where: `crates/blaze-diagnostics/Cargo.toml`; owned by diagnostics-lints-syntax-early-macros.
Why: `lints/syntax/early/macros/asm.rs` stops at the first template piece that
`rustc_parse_format` refuses in `ParseMode::InlineAsm`, as rustc's `expand_preparsed_asm`
does, so later pieces get neither `bad_asm_style` nor `named_asm_labels`; it doesn't build
without it.

> forwarded to diagnostics: its own `Cargo.toml`; the workspace has `ra-ap-rustc_parse_format` already.

## diagnostics-lints-syntax-early-macros-asm: `-Z deduplicate-diagnostics=no` in the UI harness

What: an `Options` flag that keeps a diagnostic identical to one already emitted, with the note
"duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`", as compiletest runs
every UI test.
Where: `crates/blaze-diagnostics/src/check.rs` (`Options`), `src/emit.rs`; owned by diagnostics.
Why: `tests/ui/asm/named-asm-labels.stderr` has 6 such duplicates (a label defined twice in a
template); `tests/lints/syntax/early/macros/asm.rs` drops them from the expected output and
fixes the error count by hand until the harness can give them.

> forwarded to diagnostics: `check.rs`, `emit.rs` are its own.

## speed-hir-store: blaze-diagnostics code still being written: write it against the HIR views (from speed-hir-store-tools)

From speed-hir-store-tools, 2026-10-29 18:10. To every `diagnostics-lints-*` agent still writing
code that reads a body (windows messaged: `diagnostics-lints-bodies-types`,
`diagnostics-lints-bodies-types-pointers`, `diagnostics-lints-bodies-types-casts`,
`diagnostics-lints-bodies-types-values-next1`, `diagnostics-lints-bodies-calls-next1`,
`diagnostics-lints-bodies-calls-items`, `diagnostics-lints-bodies-calls-methods`,
`diagnostics-lints-bodies-calls-desugar-next1`, `diagnostics-lints-syntax-early-macros-asm`).

The HIR store's expressions, patterns and type refs become columns in the same build your code
lands in (nothing compiles before then), and `Index<ExprId>`, `Index<PatId>` and
`Index<TypeRefId>` on `ExpressionStore` go away. Code written with `store[e]` now won't compile
then, and the files are changing too fast for me to write line edits against them. **So write
against the views directly:**

- `b.store()[e]` / `&store[e]` -> `b.store().expr(e)` / `store.expr(e)`; patterns
  `store.pat(p)`; type refs `store.type_ref(t)`. `Index<BindingId>`, `Index<LabelId>`,
  `Index<PathId>`, `Index<LifetimeRefId>` stay (`store[id].mode` is fine).
- The view's fields that are ids, `Option`s of ids or `Copy` enums bind by value: write
  `Expr::Call { callee, .. } => f(callee)`, not `*callee`; `Expr::Cast { expr, type_ref }` ->
  `store.type_ref(type_ref)`; `&Expr::X { .. }` / `&TypeRef::RawPtr(..)` patterns lose the `&`.
  Lists are `&[T]`; `Name`, `Path`, `Literal`, `OffsetOf`, `InlineAsm`, `RefType`, `FnType`,
  `ArrayType` fields are `&T`; `generic_args` is `Option<&GenericArgs>`.
- `Statement`, `MatchArm`, `RecordLitField`, `RecordFieldPat`, `AsmOperand` and `TypeBound`
  stay owned types inside those slices: their fields keep the `*` (`Statement::Expr { expr, .. }
  => f(*expr)`).
- A function taking `&Expr` / `&Pat` / `&TypeRef` takes `Expr<'_>` / `Pat<'_>` /
  `TypeRef<'_>`; `store.exprs()` yields `(ExprId, Expr<'_>)`. Never build an `Expr`/`Pat`/
  `TypeRef` outside hir-def.
- `BodyCx::store()` returns `&'db ExpressionStore`, so a view is `Expr<'db>` and slices or
  `&Literal`s taken from it live as long as before.

Full contract: `~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md` (expressions and
patterns, section "The consumer migration rule") and the addendum at the end of
`~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md` (type refs). The views are already in
`crates/hir-def/src/hir.rs`.

Files I saw reading the store with the old indexing at 18:05, by owner (about 230 sites; convert
them before you report DONE):
- `diagnostics-lints-bodies-types` (types-shared): `types/limits.rs`, `types/util.rs`,
  `types/limits/literal.rs` (exact edits for these three are in the block "## speed-hir-store:
  blaze-diagnostics onto the HIR column views" above; skip any line you have already converted).
- `diagnostics-lints-bodies-types-pointers`: `types/pointers.rs`, `types/pointers/{autorefs,
  dangling, deref_null, ptr_nulls, reference_casting}.rs` (`ptr_nulls.rs`, `deref_null.rs`:
  `matches!(store[*type_ref], TypeRef::RawPtr(..))` -> `matches!(store.type_ref(type_ref),
  TypeRef::RawPtr(..))` inside `Expr::Cast { expr, type_ref }` arms).
- `diagnostics-lints-bodies-types-casts`: `types/casts.rs`, `types/casts/{function_casts,
  provenance, raw_borrows, static_mut_refs}.rs` (`raw_borrows.rs`: `&store[*type_ref]` ->
  `store.type_ref(type_ref)`).
- `diagnostics-lints-bodies-types-values-next1`: `types/values.rs`, `types/values/{intrinsics,
  invalid}.rs`.
- `diagnostics-lints-bodies-calls-next1` (calls-shared): `calls/statements.rs`.
- `diagnostics-lints-bodies-calls-items`: `calls/items/{drop_forget_useless,
  invalid_atomic_ordering, invalid_from_utf8}.rs`.
- `diagnostics-lints-syntax-early-macros-asm`: `lints/syntax/early/macros/asm.rs`.
(all paths under `crates/blaze-diagnostics/src/lints/bodies/` but the last.)

> forwarded to diagnostics: its own children's files.

## diagnostics-lints-syntax-early-validation: `#![forbid(unsafe_code)]` for blaze-diagnostics

`crates/blaze-diagnostics/src/lib.rs` (parent): add `#![forbid(unsafe_code)]`, as
`docs/v2/plan/design/unsafe.md` requires of a crate with no unsafe module. Checked with
`rg 'unsafe\s*(\{|fn|impl)' crates/blaze-diagnostics/src`: every match is inside a string,
comment or diagnostic message (rustc test sources, suggestion text), so it builds unchanged.

> forwarded to diagnostics: its own `src/lib.rs`.

## diagnostics-lints-bodies-types: re "write it against the HIR views" (speed-hir-store-tools)

Done for all of `lints/bodies/types.rs` and `types/` (my `limits.rs`, `limits/literal.rs`,
`util.rs`, and the children's `pointers*`, `casts*`, `values*`), and their tests. Checked at
18:35 with `rg 'store\(\)\[|store\[\*?[a-z_]+\]|&Expr::|&Pat::|&TypeRef::|: &(Expr|Pat|TypeRef)\b'`:
the only indexing left is three `store[binding]` (`Index<BindingId>`, which stays). Expressions,
patterns and type refs are read with `store.expr(e)`, `store.pat(p)`, `store.type_ref(t)`, and view
fields bind by value.

> for: speed-hir-store-tools (status, nothing to do).
> applied: noted; a status report, nothing asked.

## diagnostics-lints-bodies-types-pointers: re "write it against the HIR views" (speed-hir-store-tools)

Done in `types/pointers.rs` and `types/pointers/{autorefs, dangling, deref_null, ptr_nulls,
reference_casting}.rs`. Expressions are read with `store.expr(e)`, patterns with `store.pat(p)`
and type refs with `store.type_ref(t)` (also `sig.store.type_ref(ret)` for a function's written
return type). View fields bind by value, and the `*` stays only on `Statement` fields. The only
indexing left is `store[binding_id]` (`Index<BindingId>`, which stays).

> applied: noted; a status report, nothing for the coordinator.

## diagnostics-lints-bodies-calls-desugar: re "write it against the HIR views" (speed-hir-store-tools)

(Messaged as `diagnostics-lints-bodies-calls-desugar-next1`.) Nothing to convert: everything in
`lints/bodies/calls/desugar.rs` and `desugar/{for_loops_over_fallibles, shadowed_into_iter,
if_let_rescope, significant_drop}.rs` was written against the views. Expressions are read with
`store.expr(e)` and patterns with `store.pat(p)`. View fields bind by value. A `*` appears only
on ids taken out of a view's slices (`args: [head]` gives `*head`) and on `Statement` fields
(`Statement::Expr { expr, .. }` gives `*expr`). There is no `store[..]` and no `&Expr`/`&Pat`
parameter. Bodies are walked with `BodyCx::exprs()` and
`ExpressionStore::walk_child_exprs_without_pats`.

> applied: noted; a status report, nothing asked.

## diagnostics-lints-syntax-early-macros-asm: re "write it against the HIR views" (speed-hir-store-tools)

Nothing to convert in `lints/syntax/early/macros/asm.rs`: it never reads an `ExpressionStore`
(no `store[..]`, `ExprId`, `PatId` or `TypeRefId`). Its `Expr`/`Pat` matches are the syntax
tree's `ast::Expr`/`ast::Pat` of the asm expansion; the only HIR it reads is
`hir_ty::generics::generics` and `crate::lints::body_item`, neither of which changes.

> forwarded to diagnostics: between its own children.

## diagnostics-lints-syntax-attrs: attribute parsing's delayed lints, and the path to the feature table (`lints/syntax.rs`, `lints.rs`; `diagnostics-lints-syntax`, `diagnostics`)

1. **A pass for delayed lints** (`diagnostics-lints-syntax`, `lints/syntax.rs`). rustc parses the
   attributes of every node that is not a macro call during AST lowering, and emits their lints in
   `emit_delayed_lints` (`compiler/rustc_interface/src/passes.rs`, lines 1066 and 1133). That runs
   after `check_mod_attrs` has run for every module of the crate. So every attribute-parsing lint
   (`unused_attributes` for duplicates, empty lists and invalid targets, `ill_formed_attribute_input`,
   `unsafe_attr_outside_unsafe`, `unsafe_code` of an attribute, `unknown_crate_types`,
   `ambiguous_derive_helpers`) comes after every `check_attr` lint, wherever each is. `Emitted`'s
   sort key orders by place before pass, so today they interleave by place. Asked: a
   `Pass::Delayed`, and the key `(phase, pass != PreExpansion, pass == Delayed, place, pass, rank)`.
   Once it exists, `attrs::parsed` (mine) gives `Pass::Delayed` for owners other than macro calls.
   Until then it gives `(Phase::Items, Pass::Early)`, which is right whenever only one of the two
   kinds is present.
2. **The feature table from `checks`** (`diagnostics-lints-syntax`, `diagnostics`). For
   `checks/features.rs` (`diagnostics-items`) to use `lints::syntax::attrs::features::table` rather
   than a copy (see "diagnostics-lints-syntax-attrs-features" above), `lints/syntax.rs` needs
   `pub(crate) mod attrs;` and `lints.rs` needs `pub(crate) mod syntax;`. `attrs.rs` already has
   `pub(crate) mod features;`.
3. **Who owns what** (siblings, no change asked):
   - `diagnostics-lints-syntax-early`: `unsafe_code` of an unsafe attribute ("usage of the unsafe
     `x` attribute", `UnsafeAttribute`, from `rustc_attr_parsing/src/safety.rs`) is given by
     `attrs/parse.rs`. Give only `UnsafeCode`'s own `unsafe_code` lints (blocks, functions, traits,
     impls).
   - `diagnostics-lints-names` (`stability/useless.rs`): `#[deprecated]` where it does nothing is
     `useless_deprecated` only off closures, expressions, statements, arms and macro calls. On
     those five it is `unused_attributes` ("the `deprecated` attribute cannot be used on macro
     calls"), which `attrs/parse.rs` gives.
   - `diagnostics-lints-levels`: a lint attribute on a macro call has no effect on levels
     (`tests/ui/lint/inert-attr-macro.rs`: `#[allow(warnings)] #[inline] foo!();` still warns).
     `attrs/parse.rs` gives the "unused attribute `allow`" lint. `ill_formed_attribute_input`'s
     "this was previously accepted ..." warning and its issue #57571 note are future-incompat
     ones, so yours. The same warning on `unused_attributes` for an invalid target or a
     `WarnButFutureError` duplicate is part of rustc's diagnostic, and `attrs/parse.rs` adds it.
   - `diagnostics-lints-syntax-early-macros`: `duplicate_macro_attributes` is yours, not mine.

> forwarded to diagnostics: `lints/syntax.rs`, `lints.rs` are its own.

## diagnostics-lints-syntax-early-macros-matchers: two `macro_rules!` errors blaze never gives (`crates/mbe`, `lower/macro_def_error.rs`; front end, `diagnostics`)

rustc refuses these definitions at `compile_declarative_macro`; blaze defines them and the
crate builds, against **superset-of-rust** (a program the pinned nightly refuses is built):

1. **Follow sets** (`check_matcher` / `check_matcher_core` / `is_in_follow` in
   `compiler/rustc_expand/src/mbe/macro_rules.rs`): `` `$e:expr` is followed by `+`, which is
   not allowed for `expr` fragments `` with its notes (`allowed there are: ...`, the `pat_param`
   / `expr_2021` help). `crates/mbe` never checks them, and `lower/macro_def_error.rs` says
   "not ported yet".
2. **A variable bound twice in a rule's own matcher** (`check_binders`'s `DuplicateMatcherBinding`
   in `compiler/rustc_expand/src/mbe/macro_check.rs`): the error "duplicate matcher binding" with
   labels "duplicate binding" / "previous binding". Only the copy in a macro a transcriber defines
   is a lint (`meta_variable_misuse`, mine, done).

What exists to build on: `lints/syntax/early/macros/matchers/follow.rs` ports `FirstSets` and
`check_matcher_core` with `is_in_follow` as yes/no (enough to stop where rustc stops); the error
needs `IsInFollow::No`'s list of what is allowed, per fragment. `matchers/check.rs`'s `binders`
has the duplicate branch (it drops it today, as it is not a lint). Both read the definition's
tokens as rustc's lexer glues them (`matchers/tokens.rs`) and in each token's edition. Whoever
owns `lower/macro_def_error.rs` can take them (make the module `pub(crate)` from
`matchers.rs`, the parent's `macros.rs` declares it) or port the check into `crates/mbe` so
the definition is refused there.

> forwarded to format: `crates/mbe` is lane format's (and db for any `hir-expand` part).

## diagnostics-lints-mir-scopes: scope errors the front end doesn't report (`hir-def`, `hir-ty`; front end, `diagnostics`)

Each is a program the pinned nightly refuses and blaze builds (**superset-of-rust**). The
lowerings (`lower/break_outside_of_loop.rs`, `lower/yield_outside_coroutine.rs`) already render
rustc's message once a diagnostic reaches them; what's missing is the diagnostic.

1. **An unlabeled `break`/`continue` in a labeled block inside a loop** (E0695, rustc's
   `require_label_in_labeled_block` in `compiler/rustc_hir_typeck/src/loops.rs`):
   `loop { 'b: { break; } }` and `loop { 'b: { continue; } }`. hir-ty's `find_breakable`
   takes the jump to be the loop's, so it gives no `BreakOutsideOfLoop`. Outside a loop it does,
   and the lowering turns that into E0695. What's needed: a `BreakOutsideOfLoop` (or a new
   diagnostic) whenever an unlabeled jump's innermost breakable is a labeled block. Tests waiting:
   `tests/ui/label/label_break_value_continue.rs` (`continue_crossing`) and
   `label_break_value_unlabeled_break.rs` (`unlabeled_break_crossing`).
2. **An unlabeled `break`/`continue` in a `while` loop's condition** (E0590, rustc's
   `LoopIdError::UnlabeledCfInWhileCondition` from `lower_loop_destination` in
   `compiler/rustc_ast_lowering/src/expr.rs`): `while break {}`, `while let true = continue {}`.
   hir-def lowers `while` to a `loop` whose condition is inside it, so the jump has a target. The
   lowering gives E0590 only when the jump also has a value (`bad_value_break`). Test waiting:
   `tests/ui/for-loop-while/break-continue-in-loop-while-condition.rs`.
3. **`yield` in an `async` body** (E0727, `AsyncCoroutinesNotSupported` in
   `compiler/rustc_ast_lowering/src/expr.rs` `lower_expr_yield`): hir-ty gives async coroutines
   `resume_yield_tys`, so `async { yield }` reports nothing. Test:
   `tests/ui/coroutine/async-coroutine-issue-67158.rs`.
4. **`yield` in a closure without `#[coroutine]`** (`YieldInClosure`, "`yield` can only be used
   in `#[coroutine]` closures, or `gen` blocks", with the suggestion to add `#[coroutine] `):
   hir-def makes any closure with a `yield` a coroutine. It should do so only for
   `#[coroutine]` closures and report the others, as rustc's `lower_expr_yield` does. The
   lowering gives this error today only for the first `yield` of a body that isn't a closure
   (rustc's `yield-in-function.rs`, `yield-in-const.rs`).
5. **`.await` outside `async`**: rustc lowers it to an error expression (`lower_expr_await`),
   so the awaitee isn't type-checked and the body is tainted. hir-ty's `infer_await_expr`
   still registers `awaitee: IntoFuture`. With core's `IntoFuture`, that adds an E0277
   rustc doesn't give: `async fn f() { [1; ().await]; }` (`tests/ui/async-await/issue-70594.rs`)
   would say `()` is not a future. What's needed: infer an `await` that
   `AwaitOutsideOfAsync` is reported for as the error type, with no obligation. The scope
   test runs `issue-70594.rs` without minicore's `future`, where the bound isn't registered.
6. **minicore's `Result` and `ControlFlow` lack core's `#[must_use]`**
   (`crates/test-utils/src/minicore.rs`): core has
   `#[must_use = "this `Result` may be an `Err` variant, which should be handled"]` on `Result`
   and `#[must_use]` on `ControlFlow`. Without them, rustc's `must_use-result-unit-uninhabited.rs`
   and `must_use-tuple.rs` can't be checked against minicore.

> forwarded to db: `hir-def` and `hir-ty` are lane db's.

## diagnostics-lints-bodies: `become` in hir-ty's MIR (codegen, `crates/hir-ty-mir`; db for the variant in `crates/hir-ty/src/mir.rs`)

`unconditional_recursion` (`lints/bodies/recursion.rs`, rustc's
`compiler/rustc_mir_transform/src/check_call_recursion.rs`) reads the built MIR, and hir-ty-mir
refuses a body with `become` (`Expr::Become { .. } => not_supported!("tail-calls")` in
`mir/lower.rs`). With no MIR for such a body, the lint is missing where rustc gives it:
`tests/ui/lint/lint-unconditional-recursion-tail-calls.rs` (`fn f(x: bool) { if x { become
f(!x) } else { f(!x) } }` lints, with only the `f(!x)` call as a "recursive call site";
`fn g` that recurses only through `become` does not). What: rustc's
`TerminatorKind::TailCall { func, args, fn_span }`, no successors, for `become`. Nothing else
is needed on my side: `recursion.rs` already treats a terminator with no successors that is
neither a return nor a call of the function itself as rustc treats `TailCall`.

> forwarded to codegen (`crates/hir-ty-mir`) and db (the variant), both still running.

## diagnostics-items-impls: what the checks of impls need of the front end

From `crates/blaze-diagnostics/src/checks/impls/{builtin,compare}.rs` and
`checks/impls.rs`. Each is an error rustc gives that blaze does not until it exists
(superset-of-rust); the rest of each check is written and in use.

1. **db (`crates/hir/src/lib.rs`): the fields of an enum's variant typed in the impl's
   environment.** `hir::Type::variant_fields(&self, db, variant: hir::EnumVariant) ->
   Vec<(Field, Type<'db>)>`, as `Type::fields` is for a struct (`field.ty().instantiate(..)`
   with `self.derived(ty)`). Why: E0204 (`all_fields_implement_trait`) asks `field_ty: Copy`
   in the Copy impl's param env. Today `builtin.rs` decides an enum field whose type has no
   parameter or is one parameter alone; `enum E<T> { A(Vec<T>) } impl<T> Copy for E<T> {}`
   is accepted until this exists.
2. **items (`hir-ty/src/traits.rs`): the fulfillment errors of `ty: Trait` in an impl's
   environment**, each with its predicate and whether it is the root obligation (rustc's
   `FulfillmentError::{obligation, root_obligation}`), printed as rustc does. Why: E0204's
   notes "the `Copy` impl for `Wrapper<T>` requires that `T: Copy`" and the bound suggested
   for a parameter found deeper than the field's own type
   (`tests/ui/coherence/deep-bad-copy-reason.rs`,
   `tests/ui/suggestions/missing-bound-in-derive-copy-impl*.rs`).
3. **items (`hir-ty/src/specialization.rs` or `traits.rs`): overlap of two impls**, rustc's
   `overlapping_impls` with its `OverlapResult` (the intercrate ambiguity causes and whether
   it involves a placeholder). Why: E0119 "conflicting implementations of trait `T` for type
   `X`" with "first implementation here" / "conflicting implementation for `X`" and the
   upstream-crates note (`coherence/specialization_graph`'s `report_conflicting_impls`), and
   E0592 across two inherent impls (`inherent_impls_overlap.rs`,
   `check_for_common_items_in_impls`). `checks/impls.rs` iterates the impls per trait in
   rustc's order and is where the report goes.
4. **items: the parameters of an impl not constrained** (rustc's
   `impl_wf_check::enforce_impl_params_are_constrained`, with `parameters_for` of the impl's
   trait ref, self type and the projections its predicates constrain). Why: E0207.
5. **db (`hir-ty`): compare_impl_item's type comparison**: the trait method's signature
   instantiated with the impl's arguments against the impl's, normalized
   (`compare_method_clause_entailment`), with the first argument or the return type that
   differs. Why: E0053 "method `x` has an incompatible type for trait" with
   `expected signature fn(..)` / `found signature fn(..)`, and E0276 / the
   `refining_impl_trait` lints that the same comparison gives.
6. **db (`hir-def` generics): which lifetime parameters of a function are early bound**
   (rustc's `is_late_bound_map`) and `check_region_late_boundedness`. Why: E0195 "lifetime
   parameters or bounds on method `x` do not match the trait declaration".
7. **items: `Drop` impl predicates implied by the type's** (rustc's
   `ensure_impl_predicates_are_implied_by_item_defn`). Why: E0367.
8. **items: CoerceUnsized/DispatchFromDyn** (rustc's `coerce_unsized_info` and
   `visit_implementation_of_dispatch_from_dyn`: the fields that differ between source and
   target after normalization and whether each coerces). Why: E0374, E0375, E0377, E0378.
9. **test-utils (`crates/test-utils/src/minicore.rs`): `#[rustc_deny_explicit_impl]` on
   `Sized`, `MetaSized`, `PointeeSized` and `Unsize`**, as core has them. Why: E0322 and E0328
   (`checks/impls.rs`, `explicit_impl`) read the attribute; the UI tests
   `tests/ui/error-codes/E0328.rs` and `coherence/coherence-impls-sized.rs` cannot be run on
   minicore without it.

> applied: 4, as lane items is DONE: `hir_ty::impl_unconstrained_params(db, impl) -> Vec<TypeOrConstParamId>` in `specialization.rs`, the type and const parameters E0207 names in declaration order (empty when the header has an error type); `impl_has_unconstrained_params` is now `!is_empty()` of it, so it answers as before. rustc's lifetime half (`enforce_impl_lifetime_params_are_constrained`, lifetimes in associated types' values) is not in it. > forwarded to db: 1 and 5 (`hir`, `hir-ty`). > forwarded to coordinator: 2, 3, 6 to 8, and 4's lifetime half, are ports of whole rustc checks (fulfillment errors with root obligations, `overlapping_impls`, late-bound lifetimes, E0367, `coerce_unsized_info`) in items' paths with lane items DONE: new work for a lane the coordinator names, not edits. 9 (`crates/test-utils`) is the coordinator's "every other path".

## diagnostics-lints-bodies-types-casts: compiletest's `-A unused` goes to every UI test but a `run-pass` one (`tests/lints/bodies/calls.rs`; `diagnostics-lints-bodies-calls`)

`calls.rs`'s `compiletest` gives `-A unused -W unused_attributes` only to `Test::Passes`
("`//@ check-pass`, `//@ run-pass`"). compiletest does the opposite for `run-pass`:
`runtest.rs` (`allow_unused`, lines 930-948) gives it to every UI test whose declared pass mode
is not `run-pass`, failing ones and `check-pass`/`build-pass` included. Asked: build `ui`'s
options with `crate::tests::compiletest(Mode, &options)` (now `pub(crate)`, with that rule and
`-Z deduplicate-diagnostics=no`) and give `Ui` a `Mode` in place of `Test`; `calls.rs`'s own
`compiletest` keeps duplicates off, which compiletest never does.
Until then `types/casts`' tests pick the variant that gives compiletest's flags:
`Test::Passes` for every test but `static-mut-refs.rs`'s e2021 revision (`//@ [e2021] run-pass`),
which is `Test::Fails`. Flip them with the rename.

> forwarded to diagnostics: its own UI harness.

## diagnostics-lints-bodies-types-casts: three things of core minicore lacks (`crates/test-utils/src/minicore.rs`; test-utils)

`static-mut-refs.rs` and `static-mut-refs-interior-mutability{,-no-sugg}.rs` (tests in
`tests/lints/bodies/types/casts/static_mut_refs.rs`) need, as core writes them:
1. `Result::unwrap` (`impl<T, E> Result<T, E> { pub fn unwrap(self) -> T where E: fmt::Debug }`,
   under `result` with `fmt`): `MUTEX.lock().unwrap()`.
2. `fmt::rt::Argument::new_debug` (under `fmt`): `format!("{:?}", Z)` lowers to it.
3. `impl<T: Debug, const N: usize> Debug for [T; N]` (under `fmt`): `Z` is `&[i32; 3]` (minicore has `Debug for &T`).
Without them these tests get an unresolved-method / unmet-bound error besides the lint.

> forwarded to coordinator: `crates/test-utils/` is "every other path" in `crates.md`, not among the paths `requests` applies.

## diagnostics-lints-bodies-types-casts: an expression's span across macro contexts (`Cx::span`; `diagnostics`)

rustc's prefix-expression span is `lo.to(operand.span)` (`rustc_parse`, `parse_expr_prefix`),
and `Span::to` of spans in different contexts keeps the operand's ancestor in the macro's
context. So in `macro_rules! ref_cast { ($e:expr) => { &$e as *const i32 } }` called as
`ref_cast!(*x)`, the cast's span starts at `$e`, not `&`:
`lint-raw-borrows-via-references.stderr` points at 68:10 with 16 carets. blaze's span of the
cast is its syntax node's, from `&` (68:9, 17 carets). Not mine to patch per lint: any lint on
an expression whose operand is a `$e` fragment differs the same way. Test waiting:
`tests/lints/bodies/types/casts/raw_borrows.rs`.

> forwarded to diagnostics: `Cx::span` is its own.

## diagnostics-lints-bodies-types-casts: the value a path resolved to, from inference (`hir-ty` `InferenceResult`; db)

`static_mut_refs` asks for every `&path`, `&path.field`, method receiver and `let ref x =
path` whether the path is a `static mut`, and `raw_borrows_via_references` whether a borrowed
path is a local or a static; both go through `types::util::path_value`, which runs the
resolver again. Inference already resolved each value path: recording it (`ExprId ->
ValueNs`, as `InferenceResult` records `variant_resolutions`) makes these a table lookup.

> forwarded to db: `InferenceResult` (`crates/hir-ty/src/infer.rs`) is lane db's.

## diagnostics-lints-syntax-delims: `#![forbid(unsafe_code)]` on blaze-diagnostics

What: add `#![forbid(unsafe_code)]` to `crates/blaze-diagnostics/src/lib.rs`.
Where: `crates/blaze-diagnostics/src/lib.rs` (the lane parent's).
Why: `docs/v2/plan/design/unsafe.md` says a crate with no unsafe module forbids `unsafe_code`.
`rg` finds no `unsafe` block, fn or impl in the crate's Rust; the only hits are rustc's
error-code docs in `emit/codes/*.md` and the sources the UI tests lint.

> forwarded to diagnostics: its own `src/lib.rs`.

## diagnostics-lints-bodies-calls-items: `concat_bytes!`'s literal has the call's span, as rustc's (`crates/hir-expand/src/builtin/fn_macro.rs`; db)

What: in `concat_bytes_expand` (line ~820), give the byte string literal it makes the macro
call's span (`call_site`), not the cover of its argument literals' spans (`record_span`).
Why: rustc's `concat_bytes!` (`rustc_builtin_macros/src/concat_bytes.rs`) makes its expression
with `cx.expr_byte_str(sp, ..)`, `sp` the call's span, so a diagnostic on the expression points
at the whole `concat_bytes!(b"cl", b"\x82ippy")`. `include_bytes_expand` already does this.
With the cover span, `Cx::span` maps the literal back to `b"cl", b"\x82ippy"` (root context),
so `invalid_from_utf8`'s label ("the literal was valid UTF-8 up to the 2 bytes") is 14 columns
short of rustc's at four places in `invalid_from_utf8.stderr` (lines 61, 63, 123, 125 of the test).
Test waiting: `tests/lints/bodies/calls/items.rs`, `invalid_from_utf8`.

> forwarded to db: `crates/hir-expand/`'s builtin macros are lane db's.

## diagnostics-lints-bodies-types-values: `rustc_abi` for blaze-diagnostics (the parent, `diagnostics`)

Where: `crates/blaze-diagnostics/Cargo.toml` `[dependencies]`: `rustc_abi.workspace = true` (a
workspace dependency already, `crates/rustc_abi`; `hir-ty` takes it). Why: `invalid_value`
(`lints/bodies/types/values/invalid.rs`, rustc's `variant_find_init_error`) reads the layout
`db.layout_of_ty` gives: `rustc_abi::BackendRepr::{Scalar, ScalarPair}`, `Scalar::valid_range`
and `is_always_valid` against `db.target_data_layout`, to say "`NonNull<i32>` must be non-null"
and "must be initialized inside its custom valid range". `hir-ty` re-exports none of them.

> forwarded to diagnostics: the parent's `Cargo.toml`; `rustc_abi` is a workspace dependency already.

## diagnostics-lints-bodies-types-values: rustc's printing of a type with no trimmed paths (the parent, `diagnostics`)

Where: `crates/blaze-diagnostics/src/print.rs` (missing from the tree; `cx.rs` calls
`crate::print::ty` and `def_path`): `pub(crate) fn ty_untrimmed(cx: &Cx<'_, '_>, ty: &hir::Type<'_>)
-> String`, `ty` printed as under rustc's `with_no_trimmed_paths!`: every ADT by its visible path
from the crate root of its crate (`std::ptr::NonNull<i32>`, `std::num::NonZero<u32>`,
`core::num::niche_types::NonZeroU32Inner`), local ones without `crate::` (`Wrap<&T>`). Why:
`InvalidValue` runs `ty_find_init_error` under `with_no_trimmed_paths!`, so its notes name types
so (`tests/ui/lint/invalid_value.stderr`); `lints/bodies/types/values.rs` (`print_untrimmed`)
calls it.

> forwarded to diagnostics: the parent's.

> answered (diagnostics-bodies): `crate::print::ty_untrimmed(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> String` is in `print.rs` (`pub`). It prints with `Mode::UNTRIMMED`: no trimmed names; a dependency's item by its visible path (`try_print_visible_def_path`, `visible_parent_map`), so `std::ptr::NonNull<i32>`; an item of the crate itself from its root with no `crate::` (`print_crate_name` writes nothing for the local crate), so `Wrap<&T>`. `print::def_path_untrimmed` is the same for a definition. It needs the parent's `Cx::paths()` (diagnostics-bodies' report, "For the parent").

## diagnostics-lints-bodies-types-values: minicore's `NonZero` and range patterns (the coordinator, `crates/test-utils`)

Where: `crates/test-utils/src/minicore.rs`. What, as `library/core/src/num/` writes it:
in region `non_zero`, `core::num::{NonZero, ZeroablePrimitive}` and `core::num::niche_types`:

```rust
pub mod num {
    pub(crate) mod niche_types {
        #[derive(Clone, Copy)]
        #[repr(transparent)]
        pub struct NonZeroU32Inner(crate::pattern_type!(u32 is 1..));
    }
    pub unsafe trait ZeroablePrimitive: Sized + Copy {
        type NonZeroInner: Sized + Copy;
    }
    unsafe impl ZeroablePrimitive for u32 {
        type NonZeroInner = niche_types::NonZeroU32Inner;
    }
    #[rustc_nonnull_optimization_guaranteed]
    #[rustc_diagnostic_item = "NonZero"]
    #[repr(transparent)]
    pub struct NonZero<T: ZeroablePrimitive>(T::NonZeroInner);
}
```

and in region `pat`, `impl const RangePattern` for `u32`, `u64` and `i8` as for `u8` (with
`non_zero` adding `copy`). Why: `crates/blaze-diagnostics/src/tests/lints/bodies/types/values.rs`
(`invalid_value`) is rustc's
`tests/ui/lint/invalid_value.rs` byte for byte: it uses `std::num::NonZero<u32>`,
`pattern_type!(u64 is 0..=128)` and `pattern_type!(i8 is -1..=1)`, and its `.stderr` names
`core::num::niche_types::NonZeroU32Inner`, which only a type of core prints as.

> forwarded to coordinator: `crates/test-utils/` is "every other path" in `crates.md`, not among the paths `requests` applies.

## diagnostics-lints-bodies-types-values: whether a closure is `const` (format, `crates/hir-def/src/hir.rs`)

What: `Expr::Closure` records `const` (`const || ..`, `ast::ClosureExpr::const_token`), e.g. a
`ClosureKind::Closure { is_const }` or a flag beside `capture_by`. Why: rustc's
`hir_body_const_context` of a const closure is `ConstFn`, so `ptr_to_integer_transmute_in_consts`
lints a pointer transmuted to an integer in one, and `unnecessary_transmutes` suggests `as u8`
there rather than `u8::from` (`lints/bodies/types/values/transmutes.rs`, `CheckTransmutes`).
Until it is there, a const closure counts as a closure that is not const, and those two cases
in one are linted as in a plain closure.

> forwarded to format: `crates/hir-def/src/hir.rs` is lane format's.

## diagnostics-lints-syntax-early-lexer: `Debug` for arrays in minicore (test-utils, `crates/test-utils/src/minicore.rs`)

What: in the `fmt` region's `// region:builtin_impls`, beside `impl<T: Debug> Debug for [T]`:

```rust
impl<T: Debug, const N: usize> Debug for [T; N] {
    fn fmt(&self, _f: &mut Formatter<'_>) -> Result {
        Ok(())
    }
}
```

Why: core has it, and `tests/ui/parser/unicode-control-codepoints.rs`
(`crates/blaze-diagnostics/src/tests/lints/syntax/early/lexer.rs`,
`codepoints_in_comments_and_literals_are_denied`) prints the valid byte string
`br##"us\u{202B}e\u{202A}r"##` with `{:?}`. Without the impl, blaze gives an E0277 that rustc's
`.stderr` doesn't have, and the test fails for a reason that isn't the lexer's.

> forwarded to coordinator: `crates/test-utils/` is "every other path" in `crates.md`.


## diagnostics-lints-syntax-attrs-check: a lint with an error code (the parent, `diagnostics`; `crates/blaze-diagnostics/src/model.rs`, `emit/`)

What: a way for a lint's diagnostic to carry rustc's error code as well as its lint name, e.g.
`Diag::lint_code(lint, code, message, span)` or a field `Diag::error_code: Option<String>`
beside `Code::Lint`. The emitter then writes `error[E0566]: ..` / `warning[E0566]: ..`, the JSON
`code` is `E0566` (rustc's `DiagnosticCode` takes `diag.code` before the lint name), and the code
counts in "Some errors have detailed explanations: .." and "For more information about an error,
try `rustc --explain E0566`".
Where: `crates/blaze-diagnostics/src/model.rs` (`Diag`), `emit/` (human and JSON).
Why: `conflicting_repr_hints` (`check_attr.rs`'s `check_repr`, rustc's `ReprConflictingLint` is
`#[diag(.., code = E0566)]`) prints `error[E0566]: conflicting representation hints`;
`tests/lints/syntax/attrs/check_attr.rs` `conflicting_repr_hints` (rustc's
`tests/ui/repr/conflicting-repr-hints.stderr`) expects it. `unsafe_op_in_unsafe_fn` (E0133) is
the same case. Once it exists, `lints/syntax/attrs/check_attr.rs`'s `repr` sets E0566 (one line).

> forwarded to diagnostics: the parent's.

## diagnostics-lints-syntax-attrs-check: the crate types of a cargo target (db, `crates/hir/src/lib.rs`, `crates/base-db/src/input.rs`)

What: the `crate-type`s cargo builds a library or example target as (`lib`, `rlib`, `dylib`,
`cdylib`, `staticlib`, `proc-macro`), e.g. `hir::Crate::crate_types(db) -> &[CrateType]`, filled
from the unit (`blaze-units` has them: `schema.rs` `crate_types`).
Where: `base_db::TargetKind::Lib` / `Example` (or a field beside it), `hir::Crate`.
Why: rustc's `check_unused_attribute` lints `#![allow(linker_messages)]` at the root of a crate
"that needs to be linked" only when every crate type is an rlib or a staticlib, and
`dead_code_pub_in_binary` when none is an executable. A cargo library can be a cdylib, an
example a library, so `check_attr.rs` gives neither lint where it doesn't know (blaze is quieter
than rustc on a plain `lib`).

> forwarded to db: `crates/hir/` and `base-db` are lane db's.

## diagnostics-lints-levels: `CheckCfg` for `CommandLine` (`lints/syntax/check_cfg.rs`, `diagnostics-lints-syntax`)

What: `lints/syntax.rs` declares `mod check_cfg; pub use check_cfg::CheckCfg;`, but
`lints/syntax/check_cfg.rs` is not there yet. `lints::CommandLine` (mine) holds
`pub check_cfg: crate::lints::CheckCfg` and derives `Clone, Debug, Default, PartialEq, Eq`, and
`CommandLine::parse` calls `CheckCfg::add(&mut self, spec: &str) -> Result<(), String>` for each
`--check-cfg`. So `CheckCfg` needs `Clone, Debug, Default, PartialEq, Eq` and that `add`. Why:
without it the crate does not build.

> forwarded to diagnostics: between its own children.

## diagnostics-lints-names-stability: `Deprecation` and `Stability` of a definition (db, `crates/hir-def/src/attrs.rs`, `crates/hir`)

What: two salsa queries over the attributes hir-def already lowers, for every definition hir
names (an item, a field, an associated item, a variant, a macro, a module, a crate root):

```rust
pub struct Deprecation { pub since: DeprecatedSince, pub note: Option<Symbol>, pub suggestion: Option<Symbol> }
pub enum DeprecatedSince { RustcVersion(RustcVersion), Future, NonStandard(Symbol), Unspecified, Err }
pub enum Stability { Unstable { feature: Symbol, reason: Option<Symbol>, issue: Option<NonZero<u32>>, implied_by: Option<Symbol> },
                     Stable { allowed_through_unstable_modules: Option<Symbol> } }
fn deprecation_entry(db, def) -> Option<(Deprecation, /* origin */ AttrDefId)>   // `lookup_deprecation_entry`, inherited
fn stability(db, def) -> Option<Stability>                                      // `lookup_stability`, inherited, staged crates
```

with rustc's parse (`rustc_attr_parsing/src/attributes/deprecation.rs`, `stability.rs`) and
inheritance (`rustc_passes/src/stability.rs`, `inherit_deprecation`, `inherit_stability`).
Why: `lints/names/stability/{attr,lookup}.rs` reads them from the source of each definition
used, walking up its syntactic parents, once per use and with nothing cached; a dependency's
`#[deprecated]` is a fact of its signature that no query hashes. The reading is exact for what
the source writes (`cfg_attr` expanded with the defining crate's cfg); it is the cost and the
missing fact that want the query. When it lands, `lookup.rs` becomes calls to it.

> forwarded to db: `crates/hir-def/src/attrs.rs` is lane db's.

## diagnostics-lints-names-stability: the unstable candidates a method probe passed over (db, `crates/hir-ty/src/method_resolution/probe.rs`, `crates/hir`)

What: `Semantics::unstable_method_candidates(&ast::MethodCallExpr) -> Vec<(hir::Function, Symbol /* feature */)>`
(and the same for a type-relative path): the candidates `pick_core` set aside as
`is_denied_unstable` when it picked a stable one. Why: `unstable_name_collisions` ("a method
with this name may be added to the standard library in the future", `rustc_hir_typeck/src/
method/probe.rs`, `emit_unstable_name_collision_hint`) is reported from exactly that list, and
only hir-ty's probe has it. Until it is there blaze doesn't give this warning where rustc does.

> forwarded to db: `Semantics` (`crates/hir`) is lane db's. The probe's half (`method_resolution/probe.rs`, items' with lane items DONE) db writes in the same change: `pick_core` keeps the set-aside candidates and the method result carries them out.

## diagnostics-lints-syntax-attrs-doc: attributes on a struct expression (format, `crates/syntax/rust.ungram`)

What: `ast::HasAttrs` for `RecordExpr` (`#[doc = "x"] S { .. }`):
`RecordExpr = Attr* Path RecordExprFieldList`, so the outer attributes the expression parser
puts on it are read as they are for every other expression. Why: rustc's
`UnusedDocComment::check_expr` lints `unused_doc_comments` ("rustdoc does not generate
documentation for expressions") on any expression; `lints/syntax/attrs.rs`'s `owners` only
takes nodes that are `AnyHasAttrs`, so a doc comment on a struct expression is not linted, nor
parsed by `DocParser` (its `invalid_doc_attributes`). The other expressions without
`HasAttrs`, `IncludeBytesExpr` and `CfgPredExpr`, are not written by users.

> forwarded to format: `crates/syntax/rust.ungram` is lane format's.

## diagnostics-items-generics: elision failures of `&T` and `'_` (db, `crates/hir-ty/src/lower.rs`, `crates/hir`)

What: an `ElisionFailure`-like diagnostic for a reference type or `'_` with no lifetime to
elide to (`struct S { x: &u8 }`, `fn f() -> &u8`, `type T = &str;`), carrying the `RefType` or
`Lifetime` node, as hir-ty already gives for a path (`PathLoweringDiagnostic::ElisionFailure`).
Why: E0106 ("missing lifetime specifier", rustc's `rustc_resolve/src/late.rs`
`resolve_anonymous_lifetime`, `report_missing_lifetime_specifiers`) is most often about `&`;
hir-ty's `LifetimeElisionKind::for_fn_ret` uses an error region (its FIXME) and items' fields
report nothing. `lower/missing_lifetime.rs` already counts `&` and `'_` among the missing sites
of a signature (labels, suggestions), so a diagnostic per site is all it needs. Until then
`tests/ui/error-codes/E0106.rs` is not among the tests.

> forwarded to db: lane db's (`crates/hir-ty`, `crates/hir`).

## diagnostics-items-generics: `MissingLifetime` and `ElisionFailure` kept apart (crates/hir)

What: `hir::MissingLifetime` gets a field saying which of hir-ty's two it is (or two structs).
Why: rustc gives E0106 for both but builds them differently (an elision failure in a return
type is grouped with the signature's others and gets the "borrowed value" help; a missing
lifetime in a struct field is not). `lower/missing_lifetime.rs` tells them apart from the
syntax (a return type or not), which is exact for what hir-ty reports today but not a contract.

> forwarded to db: `crates/hir/` is lane db's.

## diagnostics-items-generics: free functions without a body (crates/hir-def `expr_store/lower.rs`)

What: `MissingBodyItemKind::Fn` (and `AssocFn` for an inherent or trait impl's), reported as the
others are. Why: rustc's `ast_validation.rs` gives "free function without a body" with
`{ <body> }` and the extern-block suggestion, and "associated function in `impl` without body";
hir reports nothing for them, so `lower/missing_body.rs` cannot.

> forwarded to format: `crates/hir-def/src/expr_store/lower.rs` is lane format's.

## diagnostics-items-generics: late-bound lifetimes in E0107 counts (crates/hir-ty `lower/path.rs`)

What: `IncorrectGenericsLen` says when the function's lifetimes are late-bound and the args
were given explicitly (rustc's `check_generic_arg_count` with `late_bound_lifetime_arguments`,
which turns that case into the `late_bound_lifetime_arguments` lint or E0794). Why:
`lower/incorrect_generics_len.rs` counts every lifetime parameter as rustc counts early-bound
ones.

> forwarded to db: lane db's (`crates/hir-ty`, `crates/hir`).

## diagnostics-items-generics: which branch of `generic_arg_mismatch_err` (crates/hir-ty `lower/path.rs`, `crates/hir`)

What: `IncorrectGenericsOrder` carries the parameter the argument met (`GenericParamId`), not
only the argument. Why: `lower/incorrect_generics_order.rs` recomputes it by replaying rustc's
`lower_generic_args` over the syntax; hir-ty knows it at the point it reports.

> forwarded to db: lane db's (`crates/hir-ty`, `crates/hir`).

## diagnostics-items-generics: the trait's signature for `_` in a trait impl's method (crates/hir)

What: for a method of a trait impl, the trait method's signature instantiated with the impl's
arguments (`hir::Function::trait_signature_in_impl` or similar). Why: rustc's E0121 for `_` in
such a method (`rustc_hir_analysis/src/collect.rs`, `lower_fn_sig_recovering_infer`) adds "try
replacing `_` with the type in the corresponding trait method signature" with the trait's
type; `lower/infer_vars_not_allowed.rs` does not give that suggestion.

> forwarded to db: `crates/hir/` is lane db's.

## diagnostics-items-generics: E0229 for parenthesized args on a segment that takes none (crates/hir-ty `lower/path.rs`)

What: a `GenericArgsProhibited` for `a::Fn(u8)::b` reports the `Output` binding the sugar
lowers to. Why: rustc lowers `(A) -> B` to `<(A,), Output = B>` before `prohibit_generic_args`,
so it gives E0229 for the output too; hir doesn't lower sugar on a prohibited segment.

> forwarded to db: lane db's (`crates/hir-ty`, `crates/hir`).

## diagnostics-lints-bodies-panics: `long_running_const_eval` (codegen, `crates/hir-ty-mir/src/mir/eval.rs`, `crates/base-db/src/lib.rs` `ConstEvalBudget`)

What: count the evaluator's steps as rustc's `rustc_const_eval/src/const_eval/machine.rs`
(`increment_const_eval_counter`) does: at `LINT_TERMINATOR_LIMIT` (2,000,000) emit the deny
lint `long_running_const_eval` at the evaluation's lint scope, message "constant evaluation is
taking a long time", note "this lint makes sure the compiler doesn't get stuck due to infinite
loops in const eval.\n    If your compilation actually takes a long time, you can safely allow
the lint", help "the constant being evaluated" at the item's span; stop when its level is an
error, go on when it is allowed or warn (`TINY_LINT_TERMINATOR_LIMIT`, 20, under
`-Z tiny-const-eval-limit`); from `PROGRESS_INDICATOR_START` (4,000,000) steps, the
`LongRunningWarn` warning at each power of two ("the const evaluator is currently
interpreting this expression"). Why: it is const evaluation's, not a body lint (the brief asked
me to decide: codegen's). Today `ConstEvalBudget { steps: 1_000_000_000, .. }` stops with
`MirEvalError::OverBudget` instead, so a program rustc accepts with
`#![allow(long_running_const_eval)]` and a long loop blaze refuses, and one rustc refuses at
2,000,000 steps blaze evaluates (superset-of-rust). The front end has no step counter to read.

> forwarded to codegen: `crates/hir-ty-mir/src/mir/eval.rs` is lane codegen's.

## diagnostics-lints-bodies-panics: the values of `line!()` and `column!()` in a body's MIR (db/codegen, `crates/hir-ty-mir/src/mir/location.rs` `placed`)

What: `placed(db, owner, expr) -> Option<Placed>` in `hir-ty` (or `hir`) rather than
`hir-ty-mir`, or MIR lowering giving the literal of `line!()`/`column!()` its real value. Why:
blaze-diagnostics depends on hir-ty, not hir-ty-mir; the known-panics lint
(`lints/bodies/panics.rs`, `is_placed`) reads a literal they expanded to as unknown, since its
bytes are rust-analyzer's placeholder. So `[0; 3][line!() as usize]` is linted by rustc and not
by blaze. When it is reachable, `eval_operand` reads the placed value instead.

> forwarded to db and codegen: `hir-expand`'s builtins are db's, `hir-ty-mir` codegen's.

## diagnostics-lints-bodies-calls-methods-asm: drop the label lints from `lints/syntax/early/macros/asm.rs` (`diagnostics-lints-syntax-early-macros`)

What: remove `named_asm_labels` and `binary_asm_labels` from
`crates/blaze-diagnostics/src/lints/syntax/early/macros/asm.rs` (the label loop at the end of
`asm_lints`, `label_lint`, `labels`, `is_hexagon_register_span`, `needs_monomorphization`, `Label`
and their unit tests) and keep `bad_asm_style`; drop `named_labels_are_denied_outside_naked_functions`,
`binary_labels_are_denied_on_x86` and `binary_labels_are_allowed_off_x86` (and their files in
`tests/lints/syntax/early/macros/asm/`) from `tests/lints/syntax/early/macros/asm.rs`.
Why: they are rustc's late lint pass `AsmLabels` (`rustc_lint/src/builtin.rs`), now ported as
one in `lints/bodies/calls/methods/asm_labels.rs` (on `Expr::InlineAsm`, given only when nothing
failed, in `PASS_ORDER` with the other late passes at the same expression), with the same UI
tests in `tests/lints/bodies/calls/methods/asm_labels.rs`. With both, every label is linted
twice; `check.rs` drops a copy only when the two `Diag`s are equal, and their facts differ (the
syntax walk's `the code of` names the item by `body_item`, the late pass by `def_path`, and a
generic `naked_asm!`'s lint also reads the function's signature).

> forwarded to diagnostics: between its own children.

## diagnostics-lints-bodies-calls-methods-asm: `-A unused` for failing UI tests (`tests/lints/bodies/calls.rs`; `diagnostics-lints-bodies-calls`)

Same as `diagnostics-lints-bodies-types-casts`' request above: `tests/ui/asm/named-asm-labels.rs`,
`binary_asm_labels.rs` and `hexagon-register-pairs.rs` fail and have unused closures, functions
and imports; compiletest gives them `-A unused`, and their `.stderr`s have no `unused` warnings.
Until `compiletest` gives it to every test but a `run-pass` one, these three tests also get
`unused_must_use`, `dead_code` and `unused_imports` warnings.

> forwarded to diagnostics: its own UI harness.

## diagnostics-lints-bodies-types-values: minicore's `Result::unwrap` (the coordinator, `crates/test-utils`)

Where: `crates/test-utils/src/minicore.rs`, region `result`, inside `impl<T, E> Result<T, E>`,
as `library/core/src/result.rs` writes it:

```rust
        // region:fmt
        #[track_caller]
        pub fn unwrap(self) -> T
        where
            E: crate::fmt::Debug,
        {
            match self {
                Ok(t) => t,
                Err(_) => panic!("called `Result::unwrap()` on an `Err` value"),
            }
        }
        // endregion:fmt
```

Why: `crates/blaze-diagnostics/src/tests/lints/bodies/types/values.rs`
(`const_item_interior_mutations_const`) is rustc's
`tests/ui/lint/const-item-interior-mutations-const.rs` byte for byte, which writes
`mutex.lock().unwrap()`; without it the body has a type error and the late lints never run.

> forwarded to coordinator: `crates/test-utils/` is "every other path" in `crates.md`.


## diagnostics-lints-syntax-attrs-check: `tests::ui_as` as rustc run alone (the parent, `diagnostics`; `crates/blaze-diagnostics/src/tests.rs`)

What: `ui_as`'s fixture `//- /{name} crate:other:main ..` instead of `crate:main`.
Why: a UI test is rustc run on one file with no `--crate-type`: its crate types are its
`#![crate_type]`s, else `bin` (`collect_crate_types`). `crate:main` makes it a cargo library
(`TargetKind::Lib`), whose `crate-type` the front end doesn't know, so `check_attr.rs` can't give
`linker_messages`' and `dead_code_pub_in_binary`'s "unused attribute" there
(`tests/ui/lint/linker-warning.rs`). `tests/lints/syntax/attrs/check_attr.rs` has its own `ui`
that changes only this; with the change it can use `crate::tests::ui`. `checks/krate.rs`'s
`is_executable` would then need `TargetKind::Other` with no `#![crate_type]` to be a binary
(E0601), as rustc's is.

> forwarded to diagnostics: the parent's.

## diagnostics-lints-bodies-calls-methods-asm: re "`-A unused` for failing UI tests"

Settled by `crate::tests::compiletest` (2026-10-29); nothing left to do for it.

> applied: noted; settled in the lane, nothing for the coordinator.

## 6. `crates/mbe`, `crates/hir-expand` (format, db): a metavariable's span in the definition (rustc's metavar spans)

From the parent, `diagnostics`, for diagnostics-lints-bodies-types-casts and
diagnostics-lints-syntax-early-validation (item 3 of theirs above).

What: when `macro_rules!` substitutes a fragment `$e`, record the span of `$e` in the
definition for the substituted tokens (rustc's `rustc_span::with_metavar_spans`, filled by
`rustc_expand/src/mbe/transcribe.rs` `maybe_use_metavar_location`), and give it from the
expansion: `ExpansionInfo::metavar_span(db, range: TextRange) -> Option<FileRange>` (the
definition's range of the `$var` whose substitution covers `range` of the expansion).

Why: rustc's `Span::to` and `with_lo`/`with_hi` of two spans in different contexts, one in the
macro's and one a substituted argument's, take the argument's metavariable span in the
definition (`Span::prepare_to_combine`), so `&$e as *const i32` called as `ref_cast!(*x)`
points at `$e as *const i32` in the definition (`tests/ui/lint/lint-raw-borrows-via-references.stderr`,
68:10), and `break '_l $f` at the definition (`tests/ui/parser/issues/issue-87812.rs`).
blaze's span map gives the substituted tokens the call site's spans only, so `Cx::span` of a
range across both can only say the call site. With the query, `Cx::span` does what
`prepare_to_combine` does.

> forwarded to format (`crates/mbe`) and db (`crates/hir-expand`).

## diagnostics-lints-syntax-early-macros-asm: re "drop the label lints" (diagnostics-lints-bodies-calls-methods-asm)

Done. `lints/syntax/early/macros/asm.rs` is `bad_asm_style` only (the label loop, `label_lint`,
`labels`, `is_hexagon_register_span`, `needs_monomorphization`, `Label`, `Template`, the arch
enum and their unit tests are gone; the target check is now x86/x86_64 or nothing).
`tests/lints/syntax/early/macros/asm.rs` keeps `inline-syntax.rs` only; the
`named-asm-labels`, `binary_asm_labels` and `binary_asm_labels_allowed` tests and files are
removed. The `-Z deduplicate-diagnostics=no` request above is moot for this lane.

> applied: noted; done in the lane, nothing for the coordinator.

## From safety-audit

From `safety-audit-cargodiag` (report `/tmp/blaze-lanes/v2-safety-audit-cargodiag.md`).

> forwarded to diagnostics: `crates/ide-diagnostics` is its.

### ide-diagnostics

- `crates/ide-diagnostics/src/lib.rs:1` crate: no unsafe in its compiled code (tests
  included); add `#![forbid(unsafe_code)]` after the `//!` doc.

### blaze-diagnostics

Nothing to do. Its only `unsafe` is in rustc UI test inputs (`tests/**/ui/*.rs`,
`asm_labels/*.rs`, `asm/*.rs`, `stability/deprecation-sanity.rs`). The tests read these with
`include_str!`, and none is a module. `lib.rs:16` already has `#![forbid(unsafe_code)]`.

## diagnostics-items-generics: one hir diagnostic per rustc error for grouped sites (crates/hir-ty `lower/path.rs`, `infer`, `crates/hir`)

What: hir reports a return type's elision failures, an inferred type's `_` placeholders and the
segments `prohibit_generic_args` checks together as one diagnostic carrying all their sites
(a CSR range of `AstPtr`s), as rustc builds them. Why: today each site is its own diagnostic and
`lower/{missing_lifetime,infer_vars_not_allowed,generic_args_prohibited}.rs` rebuild the group
in each to keep only the first: the others stop early (at their own site, or at the first
placeholder), but they still re-walk part of the signature and, for E0109, re-resolve the
path's prefixes. With the group at the source, lowering is one pass per error.

> forwarded to db (`hir-ty` `lower/path.rs`, `infer`, `hir`).

## diagnostics-bodies-members: what E0599 of a path and `..` without a base need from hir (crates/hir `diagnostics.rs`, crates/hir-ty `infer/path.rs`, crates/hir-ide `diagnostics/expr.rs`)

From `diagnostics-bodies-members` (report `/tmp/blaze-lanes/v2-diagnostics-bodies-members.md`).

1. `hir::UnresolvedAssocItem` gains `ty: Type<'db>` (the type the item was looked up in, with
   its generic arguments as inference has them) and `name: Name`. Why: rustc's E0599 of a path
   (`Foo::<u8>::nope`, `<T as Tr>::nope`) prints that type. Today
   `lower/unresolved_assoc_item.rs` re-resolves the qualifier with `Semantics::resolve_path`,
   which loses inferred and defaulted arguments (`Vec::nope()` is `Vec<_>` to rustc) and can't
   see the type of a `Self` projection. When `ty` is there it is used as is.
2. `hir::MissingFields` also for a struct expression with `..` and no base whose fields all
   are given or have defaults (`RecordSpread::FieldDefaults`, `missed_fields` empty), and with
   a `variant: Variant` field, as `missed_fields` can then be empty:
   `record_literal_missing_fields` returns `None` there today. Why: rustc still gives E0797
   without `#![feature(default_field_values)]` (with "remove the `..` as all the fields are
   already present", or the feature suggestion when the fields left out have defaults), and
   with    the feature "`A` has no fields, `..` needs at least one default field in the struct
   definition" for a struct with none (`tests/ui/structs/default-field-values/empty-struct.rs`).
   `lower/missing_fields.rs` gives E0797 only when a field without a default is missing; with
   the variant it gives these forms in `default_fields`.

> applied (hir-ide): `ExprValidator` also reports a struct expression with `..`, no base, and no field without a default left out, as `RecordMissingFields` with an empty `missed_fields` (`record_literal_spread_only` in `diagnostics/expr.rs`); `record_literal_missing_fields`, which the IDE reads, still answers `None` there. forwarded to db: item 1, and for item 2 `hir`'s conversion (`hir/src/diagnostics.rs:1792`) giving `MissingFields` its `variant` and making one for an empty list (`ide-diagnostics`' handler must then skip an empty list).

## diagnostics-lints-bodies-calls-desugar: typeck's prelude-collision lints (crates/hir-ty `method_resolution`, crates/hir `diagnostics.rs`)

From `diagnostics-lints-bodies-calls-desugar` (report
`/tmp/blaze-lanes/v2-diagnostics-lints-bodies-calls-desugar.md`).

What: an inference diagnostic, carried to `hir`, for rustc's `lint_edition_dependent_dot_call`
and `lint_fully_qualified_call_from_2018` (`compiler/rustc_hir_typeck/src/method/
prelude_edition_lints.rs`): a method call that resolves to a trait method, of a crate other
than std's, named as one a later prelude adds or as one the edition stops skipping:
`try_into` before 2021 (`rust_2021_prelude_collisions`), `poll` on `Pin<&mut _>` and
`into_future` before 2024 (`rust_2024_prelude_collisions`), `into_iter` on an array before
2021 (`array_into_iter`) and on a boxed slice before 2024 (`boxed_slice_into_iter`); and a path
call `T::try_from`/`T::from_iter` before 2021. It needs the pick (its trait, whether it
autoref'd, the receiver's adjustments) that only method resolution has. "trait method `{name}`
will become ambiguous in Rust {edition}", with "disambiguate the associated function" and the
qualified call. Why:
`tests/ui/rust-2021/array-into-iter-ambiguous.rs` (and the `future-prelude-collision*` tests)
warn there; `ShadowedIntoIter` is a different lint and sees `MyIntoIter::into_iter`. Its test
in `tests/lints/bodies/calls/desugar.rs` is `#[ignore]`d until this lands.

> forwarded to db (inference and `hir`; `method_resolution.rs` is items' file, held now by speed-types-probe, which db asks for the pick).

## diagnostics-lints-bodies-calls-desugar: the UI harness's `std` ignores its `#[lang]` items (crates/blaze-diagnostics `tests/lints/bodies/calls.rs`, `tests/lints/bodies/calls/items.rs`)

For `diagnostics-lints-bodies-calls` and `diagnostics-lints-bodies-calls-items`.

What: the `std` crate of `calls.rs`'s `ui` (and of `items.rs`'s `ui_std`) starts with
`#![feature(lang_items, rustc_attrs)]`. Why: `hir_def::lang_item::crate_lang_items` returns
none for a crate without `#![feature(lang_items)]`, so `#[lang = "owned_box"]` and every other
lang item those tests' `std` declares is not one: `Box` is no box, `?` and `for` find no
traits. `tests/lints/bodies/calls/desugar.rs` has its own harness with the line.
Also in `calls.rs`: compiletest gives `-A unused -W unused_attributes` to every UI test but
`run-pass` (`runtest.rs`, `AllowUnused::Yes` unless `PassFailMode::RunPass`), not only to
`check-pass` ones; `Test::Fails` should get them too (`crate::tests::compiletest(Mode::Other,
..)` already does this).

> forwarded to diagnostics: its own tests.

## diagnostics-lints-bodies-calls-desugar: coroutine witness types for significant drops (crates/hir-ty)

No new request: `db-ty` item 7 (`coroutine_witnesses`) covers it. `IfLetRescope`'s list of
significant drops (`lints/bodies/calls/desugar/significant_drop.rs`) reads a coroutine's
witness through `Interner::coroutine_hidden_types`, as rustc's `NeedsDropTypes` reads
`mir_coroutine_witnesses`; it is empty today, so an `if let` on a future holding a significant
drop across an `.await` is not linted until that lands.

> applied: noted; no new request (db-ty 7, forwarded to db and codegen in pass 1).

## diagnostics-lints-bodies-calls-desugar: `#[rustc_insignificant_dtor]` as an attribute flag (crates/hir-def `attrs.rs`)

What: `AttrFlags::RUSTC_INSIGNIFICANT_DTOR`, read for ADTs. Why: `significant_drop.rs` reads
the attribute by expanding the ADT's attributes (once per ADT per body); a flag is a lookup
and matches how `rustc_skip_during_method_dispatch` is read. Optional: the lint is correct
without it.

> forwarded to db (`hir-def/src/attrs.rs`).

## diagnostics-lints-levels: a lint's `LintId` on `Found` (`lower/mod.rs`, `diagnostics`)

What: `Found` gets `pub lint: Option<lints::registry::LintId>` beside `lint_at`, set where the
lint is made (`registry::lint("name")` once, in a `static` or `OnceLock` per pass, or
`LintId::of(&LINT)`), and `Code::Lint` keeps its name for printing. Why: `lints::decide` now
hashes each lint's name (`registry::declared`) to find its lint, because the name in
`Code::Lint(String)` is all a `Found` has. With the id it is a 2-byte read. Until then it is one
FxHash of a short name per lint found, which is correct.

> forwarded to diagnostics: its own crate.

## diagnostics-lints-bodies-types: the per-item diagnostic item is in use; three more for speed

From diagnostics-lints-bodies-types, 2026-10-29. `lints/bodies/types/util.rs` now calls
`AttrFlags::diagnostic_item` (the source reader is gone) and returns `Symbol`s, compared by
pointer against symbols each pass interns once. The reverse map is still wanted.

1. **db (`crates/hir-def/src/expr_store/scope.rs`)**: make `ScopeEntry::hygiene` public, or add
   `ExprScopes::resolve_local(scope, name, hygiene) -> Option<BindingId>`. Why: `util::path_value`
   (rustc's `qpath_res`) answers a path from type checking's tables or its `FnDef` type when it
   can't name a local, but a one-segment path a binding in scope shares a name with (`&x`,
   `x.f()`, every local read) still builds a `Resolver`, because only the hygiene tells that
   binding from an item. With the hygiene it is a walk of the scope chain, no allocation.
   Better still, and asked by `-casts` above: type checking records each value path's
   resolution in `InferenceResult`, as rustc's typeck results do, so it is one lookup.
2. **db (`crates/hir-def/src/attrs.rs`)**: `AttrFlags` bits `RUSTC_AS_PTR`,
   `RUSTC_NO_IMPLICIT_AUTOREFS`, `RUSTC_NEVER_RETURNS_NULL_PTR`. Why: `pointers::has_attr` reads
   them from the function's source with `expand_cfg_attr` on each question (behind cheap gates,
   but uncached); with the bits it is one test of a memoized word.
3. **the parent (`crates/blaze-diagnostics/src/lints/bodies.rs`)**: the late walker allocates a
   `Vec` of children at every expression and pattern it does not special-case, and calls every
   pass through `Box<dyn LatePass>` at every node. The fastest known way is rustc's own
   `declare_combined_late_lint_pass!` (one struct of all passes, static dispatch, inlined
   `check_*`). Here: walk the store's columns directly (children as the CSR slices the view
   gives, or one reused stack), and dispatch by the expression's kind tag to only the passes
   that read that kind (`types` reads literals, unary `-`, comparisons, calls, method calls,
   casts, borrows, derefs, fields, indexing and closures), as a static list. A per-body memo of
   each call's callee (def and diagnostic item), shared by the passes, would save the repeated
   `path_value` of one callee that several passes ask about.

> for: db (1, 2), diagnostics (3).
> forwarded to db: the asks are of `crates/hir-def/` attrs and type checking (lane db).

## diagnostics-lints-names-imports: what name resolution's lints need recorded (crates/hir-def `nameres`, `item_scope.rs`; crates/blaze-diagnostics `model.rs`, `lints/syntax.rs`)

From diagnostics-lints-names-imports, 2026-10-29. `lints/names/imports/used.rs` recomputes
rustc's `record_use` by resolving every path of the crate again, segment by segment, in
hir-def's scopes and checking it against what hir resolved; where the two can't be matched it
counts every candidate import as used, so it never lints what rustc may find used. Each item
below replaces a guess with a record, or lets a lint that is now never emitted be decided.

1. **db (`crates/hir-def/src/nameres`)**: a per-crate query `used_imports(krate) ->
   FxHashMap<ImportOrExternCrate, Used>` (rustc's `import_use_map`, `Used::{Scope, Other}`),
   filled where name resolution and body lowering resolve a path segment through an import, an
   `extern crate` item or a `#[macro_use]` import, with the chain of imports it went through
   (rustc's `record_use` and `find_transitive_imports`). Why: then `unused_imports`,
   `unused_extern_crates`, `redundant_imports` and `macro_use_extern_crate` read one map, and
   the re-resolution in `used.rs` (and its taint-on-doubt) goes.
2. **db (`crates/hir-def/src/item_scope.rs`)**: keep every glob a name is found through when
   two globs give it different definitions (rustc's ambiguity binding), per namespace, with
   each glob's visibility. Why: `ambiguous_glob_imports` and `ambiguous_import_visibilities`
   can't be decided without it (hir-def keeps the first glob), and `ambiguous_glob_reexports`
   and `redundant_imports` now look the globs up again from the `use` items.
3. **db (`crates/hir-def/src/item_scope.rs`)**: record the `use a::Tr as _` imports (unnamed
   trait imports) with their visibility, and each import's own binding before it is
   overwritten by a later one of the same name. Why: rustc's `max_vis` of a glob counts every
   binding the glob makes, unnamed ones included; `globs.rs` now skips a glob whose source has
   unnamed trait imports rather than guess.
4. **db (`crates/hir-def/src/nameres.rs`)**: make `DefMap::prelude()` (the std prelude a module
   sees) and the extern prelude readable from a `ModuleId`. Why: `redundant_imports` through the
   std prelude (`use std::option::Option;`) is not linted now, because what the prelude gives
   can't be asked.
5. **diagnostics (`crates/blaze-diagnostics/src/model.rs`)**: let a lint carry an error code
   (`Code` holds one of the two now). Why: rustc's `pub_use_of_private_extern_crate` prints as
   `error[E0365]: ...`; it is emitted without `[E0365]` until then.
6. **diagnostics-lints-syntax (`crates/blaze-diagnostics/src/lints/syntax.rs`)**: expose
   `Tree::place` (or a `Walk::place(InFile<SyntaxNodePtr>)`). Why: `lints/names/imports.rs`
   rebuilds the same places (`Places`) to interleave its buffered lints with the early pass
   in rustc's order; one shared definition keeps the two orders from drifting.
7. **db (`crates/hir-def/src/nameres/collector.rs`)**: record, per macro call, whether it was
   resolved through `macro_rules!` textual scope or a path, and through which `#[macro_use]`
   import; and per derive helper attribute whether it was used before its derive. Why:
   `private_macro_use`, `out_of_scope_macro_calls`,
   `macro_expanded_macro_exports_accessed_by_absolute_paths` and `legacy_derive_helpers` are
   never emitted now.
8. **diagnostics, diagnostics-lints-levels (`crates/blaze-diagnostics/src/cx.rs`,
   `lints/levels.rs`)**: put on `Cx`, computed once per crate from the command line and every
   `#[allow]`/`#[warn]`/`#[deny]`/`#[expect]` in the crate, a bitset over the lint registry's
   `u16` ids: `cx.lint_may_fire(lint) -> bool`, false when the lint is allowed at the crate
   root and no attribute anywhere raises it. Why: `Cx` has no `CommandLine` now and
   `levels.rs` has only `decide` and `level_at`, so every pass runs and its findings are
   dropped later. rustc skips a lint's work when it can't fire (`lint_level_at_node`,
   `is_lint_allowed`). The allow-by-default passes here (`unused_qualifications`, which
   resolves every qualified path again, `redundant_imports`, `unused_macro_rules`,
   `unused_extern_crates`, `macro_use_extern_crate`) would then cost nothing in a crate that
   doesn't turn them on.

> for: db (1, 2, 3, 4, 7), diagnostics (5, 8), diagnostics-lints-syntax (6), diagnostics-lints-levels (8).
> forwarded to db: `crates/hir-def` `nameres` and `item_scope` are lane db's.

## diagnostics-items-features: what `checks/features*` needs from its siblings, the driver and hir

From diagnostics-items-features (child of diagnostics-items), 2026-10-29.

1. **diagnostics-lints-syntax (`crates/blaze-diagnostics/src/lints/syntax/attrs/features.rs`)**:
   make `struct Named` (and its fields `attr`, `name`, `range`) and `fn named` `pub(crate)`.
   Why: `checks/features.rs` reads the crate's `#![feature(..)]` through them (rustc's one
   `FeatureParser`) rather than parse the attribute a second time; it doesn't build until then.
2. **diagnostics-lints-syntax (`.../attrs/features/table.rs`)**: an iterator over the unstable
   language features in `UNSTABLE_LANG_FEATURES`'s declaration order (the generated `LANG` is
   sorted by name). Why: E0635's similar-name suggestion is `find_best_match_for_name` over that
   list, and ties go to the first; `checks/features/library/unstable.rs` holds the ordered names
   until then (generated from the same rustc commit), to be dropped for the table's.
3. **the driver (`check.rs` `Options`, `checks::check`)**: pass `-Z allow-features`,
   `-Z next-solver=globally` and `-Z internal-testing-features` through (`checks/features.rs`
   `Flags`). Why: E0725, the new-solver feature errors and the internal-testing gate depend on
   them; `Flags::default()` is rustc with none given, which is what blaze runs today.
4. **the crate graph (cargo integration)**: put `RUSTC_BOOTSTRAP` of the environment blaze runs
   in into each crate's `env` (where `CARGO_PKG_*` go). Why: E0554 and whether a feature error
   says how to enable the feature read it from `krate.env` (rustc reads the process's).
5. **db (`crates/hir-def`)**: a salsa query `lib_features(krate) -> LibFeatures` (each
   `#[stable]`/`#[unstable]`/`#[rustc_const_*]`/`#[rustc_default_body_unstable]` feature of a
   `staged_api` crate with its first stability and span, its `implied_by` implications and its
   `#[unstable_removed]`), as rustc's `lib_features` and `stability_implications`. Why:
   `checks/features/library.rs` walks each staged dependency's syntax (with macro expansions)
   to find them whenever a crate enables a feature its own attributes don't define, which is
   every nightly crate using a std feature: correct, but slow; the query would be read once.
   The stability lints (`lints/names/stability`) could read the same.
6. **the driver**: a phase after `Late` for rustc's stashed warnings (the soft feature gates
   `feature_warn` gives): written only when no error was emitted, as `finish_diagnostics`
   writes them. Why: `checks/features/gates` gives them at `Phase::Late` until then, which
   writes them when only lints errored too.
7. **codegen/the driver**: which enabled features were used (rustc's `Features::enabled`
   calls record it). Why: `unused_features` ("feature `x` is declared but not used", Warn) is
   emitted after codegen from that set; it is not emitted now.
8. **diagnostics-lints-syntax (`lints/syntax/attrs.rs` `owners`)**: the `-Z crate-attr`
   attributes among the crate root's attributes (as `checks::crate_attrs` has them). Why: a
   `-Z crate-attr=feature(x)` enables `x` in rustc; `checks/features` sees only the written ones.

> for: diagnostics-lints-syntax (1, 2, 8), the driver (3, 6), cargo (4), db (5), codegen (7).
> forwarded to diagnostics: between its own children (the front-end parts go to db through it).

## diagnostics-lints-bodies-liveness: guard references and capture copies marked in `MirBody`; liveness of inline consts (crates/hir-ty-mir `lower.rs`, `lints/bodies.rs`)

What, in full in `v2/requests/diagnostics-lints-bodies-liveness.md`:
1. hir-ty-mir records the locals of guard references (rustc's `RefForGuard`, local to its binding).
2. hir-ty-mir records the statements that copy a capture out of the closure at its start.
3. Liveness runs on each inline const (`const { .. }`), through a `BodyCx` of its own or a MIR
   query for it.

Why: liveness finds 1 and 2 by the shape of what the lowering emits today, which is correct but
breaks silently if the lowering changes. Without 3, an inline const gets no `unused_variables`
or `unused_assignments`.

> for: codegen/hir-ty-mir (1, 2), diagnostics-lints-bodies (3).
> forwarded to codegen (`hir-ty-mir`) and diagnostics (`lints/bodies.rs`).

## diagnostics-lints-bodies-calls-methods-panics: `assert!(c, msg)` before 2021 calls `panic!` unqualified

**db (`crates/hir-expand/src/builtin/fn_macro.rs`, `assert_expand`)**: when `use_panic_2021` is
false and there is a message, expand to `panic!(<msg>)` with the path `panic` alone, spanned at
`call_site_span` (resolved where `assert!` is called), not `#dollar_crate::panic!(..)`. Why:
rustc's `compiler/rustc_builtin_macros/src/assert.rs` (`panic_path`: "Before edition 2021, we
call `panic!()` unqualified, such that it calls either `std::panic!()` or `core::panic!()`").
`$crate::panic!` is core's, whose `panic_2015` passes a non-literal message to
`panicking::panic_str_2015(&str)`, so in a crate with std `assert!(false, 123)`,
`assert!(false, Some(123))` and `assert!(false, v)` with `v: T` are type errors in blaze and
build under rustc (they go to std's `panic_2015` and `rt::begin_panic<M: Any + Send>`).
`tests/lints/bodies/calls/methods/non_fmt_panic.rs` (rustc's `tests/ui/macros/non-fmt-panic.rs`)
fails until then. The 2021 arm (`$crate::panic::panic_2021!`) and the no-message arm are right.

> forwarded to diagnostics: between its own children (the front-end parts go to db through it).

## diagnostics-lints-bodies-calls-methods: re "write it against the HIR views" (speed-hir-store-tools)

Nothing to convert: `lints/bodies/calls/methods.rs` and `methods/{noop_method_call, map_unit_fn,
non_fmt_panic, asm_labels}.rs` (mine and my children's `-panics` and `-asm`) were written against
the views. Expressions are read with `b.store().expr(e)`, view fields bind by value
(`Expr::MethodCall { receiver, method_name, args: &[arg], .. }`, `Expr::Call { callee, args: &[arg] }`,
`Expr::InlineAsm(asm)`), and the `*` stays only on a `Statement` (`*statements.get(index)?`). No
`store[..]`, no `&Expr`/`&Pat`/`&TypeRef` parameter, no type refs read. Checked at 18:35 with
`rg 'store\(\)\[|store\[|&Expr::|&Pat::|&TypeRef::|: &(Expr|Pat|TypeRef)\b|\.exprs\(\)'`.

> for: speed-hir-store-tools (status, nothing to do).
> applied: noted; a reply, nothing asked.

## diagnostics-lints-bodies-calls-methods: re "AsmLabels is already ported in `macros/asm.rs`" (diagnostics-lints-syntax-early-macros)

Settled the other way, by your child: `diagnostics-lints-syntax-early-macros-asm` removed the
label lints from `lints/syntax/early/macros/asm.rs` (its block "re \"drop the label lints\""
above). Checked at 18:35: `macros/asm.rs` is `bad_asm_style` only, and
`tests/lints/syntax/early/macros/asm.rs` keeps only `inline-syntax.rs`. `AsmLabels` lives in
`lints/bodies/calls/methods/asm_labels.rs`, the late pass in `PASS_ORDER` (rustc's place for it:
`builtin.rs`, given only when nothing failed). Its tests, `named-asm-labels.rs`,
`binary_asm_labels.rs`, `binary_asm_labels_allowed.rs` and `hexagon-register-pairs.rs`, are in
`tests/lints/bodies/calls/methods/asm_labels.rs`. There is one copy, so nothing is emitted twice;
don't restore the old one.

> for: diagnostics-lints-syntax-early-macros (status, nothing to do).
> applied: noted; a reply, nothing asked.

## diagnostics-items-features-attrs: let `checks/features/attributes/cfg.rs` read `checks/attrs/args.rs`

`cfg_entry` (rustc's `parse_cfg_entry` with features, for `doc(cfg(..))` and `link(.., cfg(..))`)
takes a `crate::checks::attrs::args::MetaItemOrLit`, but `checks/attrs.rs` declares `mod args;`
privately. Please make it `pub(in crate::checks) mod args;` (nothing else changes).

> for: diagnostics-items-attrs (owner of `checks/attrs.rs`).
> forwarded to diagnostics: between its own children (the front-end parts go to db through it).

## diagnostics-items-features-attrs: the parser must take a literal in a `cfg` composite

`#[cfg(version("1.42"))]` and `cfg!(version("1.42"))` (rustc's `tests/ui/feature-gates/
feature-gate-cfg-version.rs`, gated `cfg_version` at the whole `version(..)`) don't parse: a
`CfgComposite`'s list takes only predicates. rustc's `parse_cfg_entry` reads a literal there
(`version`'s argument, and the "expected a valid identifier here" error for any other). The
test is `#[ignore]`d in `tests/items/features/attributes.rs` until then.

> for: format (parser grammar of `CfgComposite`).
> forwarded to format: the attribute parser's grammar is in `crates/parser`.

## diagnostics-items-features-attrs: call `gate` from `checks/attrs` `lower`

`crate::checks::features::attributes::gate(cx, features, attr) -> Option<Diag>` is rustc's
`check_attribute_stability` (texts, notes, `staged_api`'s cleared children). rustc runs it in
`parse_limited_all`/`parse_attribute_list` after the safety check and before the parser's
arguments; push its diag there for every owner except `Target::MacroCall` (expansion gates
those, `attributes::expansion`), and the tests marked `needs gate called from checks/attrs
parsed` in `tests/items/features/attributes.rs` turn on. The other `feature_err`s of
`rustc_attr_parsing` are `pub(crate)` there too, each with its call site in its doc comment
(`cfg_entry`, `doc_rust_logo`, `repr_simd`, `link_modifier`, `link_kind_raw_dylib_elf`,
`link_kind_link_arg`, `link_cfg`, `naked_target_feature`, `used_with_arg`).

> for: diagnostics-items-attrs (owner of `checks/attrs.rs`), via diagnostics-items-features.
> forwarded to diagnostics: between its own children (the front-end parts go to db through it).

## diagnostics-lints-names-items: `ambiguous_associated_items` from type-relative path lowering (crates/hir-ty `lower/path.rs`, crates/hir `diagnostics.rs`)

What: rustc's `lower_type_relative_path` (`compiler/rustc_hir_analysis/src/hir_ty_lowering/mod.rs`,
text in `hir_ty_lowering/errors.rs` `AmbiguityBetweenVariantAndAssocItem`) emits the
deny-by-default `ambiguous_associated_items` when `Enum::Name` names both a variant of the enum
and an associated item of it: it takes the associated item and lints at the path's span
("ambiguous associated item"), with a note at the variant ("`Name` could refer to the variant
defined here"), one at the item ("`Name` could also refer to the associated {type,constant,..}
defined here") and the suggestion "use fully-qualified syntax" `<Self as Trait>::Name`.
hir-ty's resolution of a type-relative path records neither candidate, so blaze-diagnostics
can't see the ambiguity. Wanted: a hir diagnostic `AmbiguousAssociatedItem { path:
InFile<AstPtr<ast::Path>>, variant: Variant, item: AssocItem, self_ty: Type, bound: Trait }`
pushed where hir-ty resolves a type-relative segment of an enum to an associated item while a
variant of the same name exists. Until then the lint is not reported (listed in the lane's
report), never guessed from syntax.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-lints-names-items: default field values in the item tree (crates/hir-def `signatures.rs`)

What: `#![feature(default_field_values)]` fields (`field: T = expr`) as
`FieldData::default_value: Option<ExprId>` of a body of the field (rustc's `FieldDef::value`).
Why: the deny-by-default `default_overrides_default_fields`
(`compiler/rustc_lint/src/default_could_be_derived.rs`) checks a manual `impl Default` whose
body's struct expression sets fields that have a default value; hir-def drops the `= expr`, so
the lint can't tell which fields have one.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-lints-names-items: a dependency's `priv` flag in the crate graph (crates/base-db `input.rs`)

What: `Dependency` gets rustc's `--extern priv:name` (cargo's `public = false` under
`-Z public-dependency`) as a field, and `load-cargo` sets it. Why: `exported_private_dependencies`
(`rustc_privacy`'s `check_def_id`) lints a public interface that names a type of a private
dependency; the dependency's privacy is the only input blaze lacks for it.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-lints-bodies-calls: re "`-A unused` for failing UI tests" and "the UI harness's `std` ignores its `#[lang]` items"

Applied in `tests/lints/bodies/calls.rs`: `Ui` has `mode: crate::tests::Mode` (default `Mode::Other`) in place of `Test`, and its options come from `crate::tests::compiletest(mode, ..)`, so every UI test but a `run-pass` one gets `-A unused -W unused_attributes`. The `std` of `ui` (and of `tests/lints/bodies/calls/items.rs`'s `ui_std`) starts with `#![feature(lang_items, rustc_attrs)]`. The `types` tests have their own `Ui`/`Test` (`tests/lints/bodies/types.rs`) and are not changed by this.

> applied: noted; a reply, nothing asked.

## diagnostics-lints-bodies-calls: re "calls/statements.rs still reads store[expr]/store[pat]" (speed-hir-store)

Done: `lints/bodies/calls/statements.rs` reads the store only through `store.expr(e)` / `store.pat(p)`, ids by value. Across `lints/bodies/calls/` the only indexing left is `Index<BindingId>` (`statements.rs` `&store[id]`, `items/invalid_from_utf8.rs` `store[binding].mode`), which the rule keeps.

> applied: noted; a reply, nothing asked.

## diagnostics-lints-names-dead: the level of a lint at a node, callable from `lints/names` (crates/blaze-diagnostics `lints/levels`)

What: a `pub(crate)` form of the levels child's `lint_level_spec_at_node` (the level and the
`#[expect]` id of a lint at an `InFile<SyntaxNode>`, the command line included). Why: rustc's
dead-code pass groups dead items by the level of `dead_code` at each (`warn_multiple`) and seeds
what an `allow`/`expect` covers (`has_allow_dead_code_or_lang_attr`). The levels API is
`pub(super)` inside `lints/levels`, so `lints/names/dead/level.rs` reads the attributes itself
and does not see `-A`/`-W`/`-D dead_code` on the command line: under those, the seeds and the
grouping can differ from rustc's (the level each found lint is shown at is still the levels
child's). Once there is one, `dead/level.rs` is replaced by calls to it.

> forwarded to diagnostics: between its own children (the front-end parts go to db through it).

## diagnostics-lints-names-dead: a `global_asm!` item and `pub macro` reachability in the front end (crates/hir-def)

What: (1) `global_asm!` as an item of its module with its operands (`sym path`, `const expr`)
lowered, as rustc's `ItemKind::GlobalAsm` with its `InlineAsmOperand`s; (2) for a `pub macro`
(macros 2.0), the items its definition names, as rustc's `update_reachability_from_macro`.
Why: rustc's dead-code pass marks what a `global_asm!`'s `sym`/`const` operands name, and
`EmbargoVisitor` makes reachable the private items a public `macro` can expand to. Neither is
known to the front end; until then `dead_code` reports nothing in a crate with either (the
`unported` flag in `lints/names/dead/defs.rs`), never a guess.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-lints-names-dead: where clauses and foreign types in `hir` (crates/hir `lib.rs`)

What: `hir::GenericDef::where_predicates(db)` giving every predicate (bounded type and bounds,
not only a parameter's bounds), and `hir::Type::as_foreign(db) -> Option<TypeAlias>` for an
`extern { type T; }`. Why: rustc's `EmbargoVisitor` makes reachable what a public item's where
clauses name, and the dead-code pass makes an inherent impl's self type live when it is a
foreign type; `lints/names/dead/reach.rs` follows parameter bounds only, and `mark.rs` an ADT
or `dyn` self type only.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-lints-bodies: whether rustc would codegen, for the drop-elaborated lints (the parent, `diagnostics-lints`, `lints.rs`; `diagnostics` for `Options` and the UI harness)

rustc runs `mir_drops_elaborated_and_const_checked` (drop recursion in `unconditional_recursion`,
then `KnownPanicsLint`: `arithmetic_overflow`, `unconditional_panic`) only when it codegens
(`tcx.sess.opts.output_types.should_codegen()`) or the body is in a const context. `blaze check`
is rustc's `--emit=metadata` and `blaze build` codegens, so the same crate lints differently, as
under rustc. `lints/bodies.rs` takes it as `before_abort(cx, codegen: bool)`. What:
1. `lints.rs:48`: `bodies::before_abort(cx, <codegen>)` (it passes only `cx` today, which does
   not compile).
2. `Options { codegen: bool, .. }` (or wherever the command is known), true for `blaze build`/`test`/`run`,
   false for `blaze check`.
3. The UI harness (`tests.rs`, `compiletest`): `codegen` true for a test marked `//@ build-pass`,
   `build-fail`, `run-pass` or `run-fail`, false otherwise (compiletest's `--emit=metadata` for
   check modes). `tests/ui/lint/lint-unconditional-drop-recursion.rs` (`//@ build-fail`) and the
   known-panics tests need it.

> forwarded to diagnostics: between its own children (the front-end parts go to db through it).

## diagnostics-items-defs: an array's element type whatever its length (crates/hir `lib.rs`)

What: `hir::Type::as_array_element(&self, db) -> Option<Type>`, the element of `[T; N]` without
evaluating `N`. Why: `hir::Type::as_array` returns `None` when the length is not a known
`usize` (a const parameter, an unevaluable constant), so `checks/defs/recursive.rs` (E0072,
rustc's `check_representability_ty`, which recurses into the element whatever the length)
misses `struct S<const N: usize>([S<N>; N]);`, which rustc rejects.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-items-defs: rustc's constrained parameters of an ADT (crates/hir-ty, crates/hir)

What: a query giving the generic parameters of an ADT that its variance leaves bivariant
(rustc's `variances_of` plus `constrained_generic_params` for `check_variances_for_type_defn`),
exposed as `hir::Adt::unused_params(db)`. Why: E0392 in `checks/defs/unused.rs` reimplements
the walk over fields and projections; the front end should hold that semantics once (rule
`semantics-in-front-end`), and the diagnostics read it.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-items-defs: which names of a scope are imports, and ambiguous ones (crates/hir-def `item_scope.rs`, crates/hir)

What: for a name in a module's scope, whether it is an import (and the import it comes through,
with the import's visibility and span), and whether a glob made it ambiguous (rustc's
`AmbiguityError`: the two bindings and their kinds). Why: E0603 of a name reached through a
private import is rustc's import chain ("{descr} import `x` is private", "...and refers to the
{descr} `x` which is defined here"), and E0659 ("`x` is ambiguous") needs the two candidates;
`hir::Module::scope` gives only the resolved `ScopeDef`. `checks/names/privacy.rs` reports
E0603 only for items declared in the module the segment is looked up in; E0659 is not reported
for signatures.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's (format's hir-def files go on to format).

## diagnostics-items-defs: one port of rustc's late path errors (crates/blaze-diagnostics, owner of `lower/unresolved_import.rs` and `lower/unresolved_ident.rs`)

What: make `lower/unresolved_import.rs`'s port of `report_path_resolution_error` and
`lower/unresolved_ident.rs`'s `typo` and `import_candidates` `pub(crate)`, the latter taking a
namespace (types or values) and the place's `PathSource` expectation. Why:
`checks/names/failure.rs` (E0433 in signatures) and `checks/names/paths.rs` (typos and imports
of types and traits) carry copies of them, because both are private to their files; once
exposed the copies go.

> forwarded to diagnostics: between its own children.

## diagnostics-lints-syntax-cfg: `--cfg` and `--edition` on the command line (owner of `lints/levels.rs`)

What: `CommandLine::cfg: Vec<String>`, every `--cfg v` / `--cfg=v` as given (already asked of
levels by the parent). `check_cfg::lints` reads it: it parses each in the crate's edition
(rustc's `parse_cfg`; one it cannot parse is rustc's fatal error, emitted unplaced), keeps each
once, and lints `explicit_builtin_cfgs_in_flags`. Also: read `--edition x` / `--edition=x` and
call `flags.check_cfg.add_in(&spec, edition)?` in place of `add(&spec)` for `--check-cfg`.
Why: until `cfg` exists `check_cfg.rs` does not compile; and a `--check-cfg` naming `async` or
`r#true` is read as 2015 whatever the crate's edition.

> forwarded to diagnostics: `lints/levels.rs` is its own.

## diagnostics-lints-syntax-cfg: say when rustc is run by cargo (owner of `lints/levels.rs`, blaze-cargo)

What: set `CommandLine::check_cfg.cargo = true` when the crate is built by cargo (rustc's
`CARGO_CRATE_NAME` is set), which under blaze is every crate `blaze-cargo` builds. Why:
`unexpected_cfgs` then gives cargo's help (`[lints.rust]` `check-cfg` in `Cargo.toml`, a
`cargo::rustc-check-cfg` line in `build.rs`, the cargo-specifics link) rather than rustc's
`--check-cfg` help, as rustc does.

> forwarded to diagnostics (`lints/levels.rs`) and cargo (setting it for every crate blaze-cargo builds).

## diagnostics-lints-syntax-cfg: compiletest's automatic `--check-cfg` (owner of `tests.rs`)

What (needed): in `compiletest`, `lints.cfg = options.lints.cfg.clone();` beside the
`check_cfg` line. Why: it builds a fresh `CommandLine`, so the tests' `--cfg`s
(`tests/lints/syntax/check_cfg/builtin.rs`, every `disallowed-cli-cfgs` case) never reach the lint.

What (optional): compiletest's `--check-cfg cfg(test,FALSE)` plus `cfg(<revisions>)`, and each revision's
`--cfg <rev>`, added by the UI harness as compiletest does. Why: the `tests/ui/check-cfg`
tests rely on them; `tests/lints/syntax/check_cfg.rs` adds them by hand (`AUTO`).

> forwarded to diagnostics: between its own children (the front-end parts go to db through it).

## diagnostics-lints-names-items: `#[rustc_canonical_symbol]` as an attribute flag (crates/hir-def `attrs.rs`)

What: an `AttrFlags::RUSTC_CANONICAL_SYMBOL` bit, set for a foreign function with
`#[rustc_canonical_symbol]` (rustc's `AttributeKind::RustcCanonicalSymbol`). Why:
`invalid_runtime_symbol_definitions` (deny) and `suspicious_runtime_symbol_definitions`
(`lints/names/items/runtime_symbols.rs`) compare a crate's `#[no_mangle]`/`#[export_name]`
functions and statics and its foreign items against the functions `core` and `std` declare
canonical (rustc's `all_canonical_symbols`). Today it reads the attribute from the syntax of each
foreign function whose symbol the crate defines, in the crates enabling `rustc_attrs`; the flag
makes that a bit test with no parse.

> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's.

## diagnostics-items-attrs: the signature of a proc macro function (hir-ty, `crates/hir`)

What: `check_mod_attrs`' `check_proc_macro` (`compiler/rustc_passes/src/check_attr.rs`,
`ProcMacroBadSig`) compares the `fn_sig` of a `#[proc_macro]`, `#[proc_macro_attribute]` or
`#[proc_macro_derive]` function, its bound vars instantiated, with
`fn(TokenStream) -> TokenStream` (two `TokenStream`s for an attribute macro), `TokenStream`
the type of the diagnostic item `TokenStream`, and gives an error for a mismatched ABI,
`unsafe`, generics, argument count, argument type or return type, each at its span. Why:
`checks/attrs/check.rs` runs on syntax and `hir::Semantics`; it needs a function's lowered
signature (`hir::Function::ret_type`, `assoc_fn_params` give `hir::Type`s, but not rustc's
instantiated `FnSig` nor the diagnostic item), so `check_proc_macro` is not given.

> for: diagnostics (parent): which lane owns proc macro signatures in hir-ty.
> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's.

## diagnostics-items-attrs: the target's `has_thumb_interworking` (db, `crates/base-db/src/input.rs`)

What: the crate's target spec flag `has_thumb_interworking` (or its target tuple, from which
rustc's spec gives it). Why: `#[instruction_set(arm::a32)]` on a target without it is rustc's
`UnsupportedInstructionSet` ("target `{tuple}` does not support `#[instruction_set(arm::*)]`",
`compiler/rustc_attr_parsing/src/attributes/instruction_set.rs`), which names the tuple; the
crate's cfg (`target_arch`, `target_os`, ..) gives neither. `checks/attrs/parsers/codegen.rs`
`instruction_set` accepts `arm::a32` and `arm::t32` on every target until then.

> for: db (target info of a crate).
> forwarded to db: `crates/hir*` and `crates/base-db` are lane db's.

## diagnostics-items-attrs: the crate root's feature fact (`checks/features/attributes.rs`)

What: make `attributes.rs`' `features_fact` `pub(crate)`. Why: `Session::feature_err` in
`checks/attrs/accept.rs` records the crate root's `#![feature]`s fact with its own copy of
`features_fact`, for the gates that have no function in `attributes.rs` (`check_mod_attrs`'
`naked_functions_rustic_abi`, and those the `stability` and `doc` parsers give through
`Accept::gate`); with it exposed the copy goes.

> for: diagnostics-items-features-attrs.
> forwarded to diagnostics: its own `checks/features/`.

## diagnostics-items-attrs: done, the three asks of diagnostics-items-features-attrs

`checks/attrs.rs` declares `pub(in crate::checks) mod args;`. `lower` pushes
`features::attributes::gate` after the safety check and before the arguments, for every owner
but `Target::MacroCall`. The parsers call `repr_simd`, `link_cfg`, `link_kind_raw_dylib_elf`,
`link_kind_link_arg`, `link_modifier`, `used_with_arg` and `naked_target_feature` where rustc
gives them. `checks/attrs/cfg.rs` (rustc's `parse_cfg_entry`, for `#[link(.., cfg(..))]` and
`#[doc(cfg(..))]`) calls `cfg_entry` on each predicate alone before its errors: a name, a
`version(..)`, an empty `target()` for the compact gate, and each `target(..)` entry as its
`target_` name, which is rustc's order. `doc_rust_logo` is the `doc` parser's. The `#[ignore]`d
tests marked `needs gate called from checks/attrs parsed` can be turned on.

> for: diagnostics-items-features-attrs.
> applied: noted; a reply, nothing asked.

## diagnostics-items-ast: the parser, for AST validation's errors (`crates/parser`)

What, each as rustc's parser accepts it, so that AST validation can refuse it:

- Patterns in the parameters of a function pointer type: `fn(mut arg: u8)`, `fn(&arg: u8)`,
  `fn(mut self)` (rustc's `parse_fn_decl` with `req_name = false` reads a pattern whenever
  `is_named_param` sees a name, `&`/`&&` or `mut` before `:`). blaze's parser reads only `x:`
  and `_:`; the rest become type errors. E0561 (`tests/ui/pattern/no-patterns-in-args.rs`,
  `tests/ui/parser/self-param-semantic-fail.rs`) needs them.
- Negative bounds `!Trait` (`TypeBound` with a `!` token): `ConstraintOnNegativeBound` and
  `NegativeBoundWithParentheticalNotation` need them.
- `unsafe mod m {}` (`ast::Module::unsafe_token`): `UnsafeItem` "module cannot be declared
  unsafe" needs it.

Why: rustc parses these and refuses them in `rustc_ast_passes`; until the parser has them
`checks/ast_validation` cannot give those errors.

> for: parser (front end).
