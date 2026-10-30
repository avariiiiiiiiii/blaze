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

> forwarded to db: `crates/hir-def/src/attrs.rs` is lane db's.

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

## diagnostics-lints: `crate::tests::ui` passes compiletest's flags (the parent, `diagnostics`, `src/tests.rs`)

What: `ui` and `ui_with` give the crate, before the options' own lint flags, what compiletest
gives a UI test that is not `run-pass` (`src/tools/compiletest/src/runtest.rs`,
`make_compile_args`): `-A unused -W unused_attributes -A internal_features -A
incomplete_features -A unused_parens -A unused_braces`. Why: every `.stderr` of rustc's was
made with them, so a test whose source has an unused variable or an `unsafe` block with nothing
unsafe in it has no warning for it in its `.stderr` (`tests/ui/consts/issue-18294.rs`).
