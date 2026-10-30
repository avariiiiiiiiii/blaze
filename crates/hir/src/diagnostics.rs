//! Re-export diagnostics such that clients of `hir` don't have to depend on
//! low-level crates.
//!
//! This probably isn't the best way to do this -- ideally, diagnostics should
//! be expressed in terms of hir types themselves.
use std::mem::discriminant;

use base_db::Db;
use cfg::{CfgExpr, CfgOptions};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, EnumId, EnumVariantId, GenericDefId, GenericParamId, ImplId,
    ItemContainerId, Lookup, MacroId, ModuleDefId, ModuleId, SyntheticSyntax, TraitId,
    attrs::AttrFlags,
    expr_store::{
        Body, ExprOrPatPtr, ExpressionStore, ExpressionStoreDiagnostics, ExpressionStoreSourceMap,
        hir_assoc_type_binding_to_ast, hir_generic_arg_to_ast, hir_segment_to_ast_segment,
    },
    hir::{ExprId, ExprOrPatId, PatId},
    nameres::{
        DefMap,
        assoc::{ImplItems, TraitItems},
        diagnostics::{DefDiagnosticKind, DefDiagnostics},
    },
    signatures::{
        ConstSignature, FunctionSignature, ImplFlags, ImplSignature, TraitFlags, TraitSignature,
        TypeAliasSignature,
    },
    type_ref::TypeRefId,
    unstable_features::UnstableFeatures,
};
use hir_expand::{
    ExpandErrorKind, HirFileId, InFile, MacroCallId, MacroCallKind, MacroKind, RenderedExpandError,
    ValueResult, mod_path::ModPath, name::Name,
};
use hir_ide::{
    CastError, ElidedLifetimeSite, ExplicitDropMethodUseKind, InferBodyId, InferenceDiagnostic,
    InferenceResult, ParamEnvAndCrate, PathGenericsSource, PathLoweringDiagnostic,
    TyLoweringDiagnostic,
    check_orphan_rules,
    db::{AnonConstId, HirDatabase, signature_anon_consts_and_diagnostics},
    diagnostics::BodyValidationDiagnostic,
    display::{DisplayTarget, HirDisplay},
    method_resolution::TraitImpls,
    next_solver::{
        AnyImplId, DbInterner, EarlyBinder, StoredTy, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    solver_errors::SolverDiagnosticKind,
    solver_errors::{self, StoredGoal},
    traits::{is_inherent_impl_coherent, structurally_normalize_ty},
};
use rustc_type_ir::inherent::IntoKind as _;
use span::Edition;
use stdx::{impl_from, never};
use syntax::{
    AstNode, AstPtr, SyntaxError, SyntaxNodePtr, TextRange,
    ast::{self, HasGenericArgs, HasName},
    match_ast, view,
};
use triomphe::Arc;

use crate::{
    AnyFunctionId, AssocItem, Const, EnumVariant, Field, Function, GenericDef, GenericParam, Trait,
    Type, TypeAlias, TypeOwnerId, Variant,
    match_lints::{BindingOrigin, ConstPatternError, SpecialTy},
    struct_tail_raw,
};

pub use hir_def::{VariantId, expr_store::MissingBodyItemKind, hir::UnsafeBinderCastKind};
pub use hir_expand::BuiltinChild;
pub use hir_ide::{
    GenericArgsProhibitedReason, IncorrectGenericsLenKind, MissingLifetimeKind, ReturnKind,
    diagnostics::{CaseType, IdentType, IncorrectCase},
};
pub use hir_ide::solver_errors::{TypeAnnotationNeeded, UnreportedSolverError};

#[derive(Debug, Clone)]
pub enum SpanAst {
    Expr(ast::Expr),
    Pat(ast::Pat),
    Type(ast::Type),
}
const _: () = {
    use syntax::ast::*;
    impl_from!(Expr, Pat, Type for SpanAst);
};

impl From<Either<ast::Expr, ast::Pat>> for SpanAst {
    fn from(value: Either<ast::Expr, ast::Pat>) -> Self {
        match value {
            Either::Left(it) => it.into(),
            Either::Right(it) => it.into(),
        }
    }
}

impl ast::AstNode for SpanAst {
    fn can_cast(kind: syntax::SyntaxKind) -> bool {
        ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind) || ast::Type::can_cast(kind)
    }

    fn cast(syntax: syntax::SyntaxNode) -> Option<Self> {
        let kind = syntax.kind();
        if ast::Expr::can_cast(kind) {
            ast::Expr::cast(syntax).map(SpanAst::Expr)
        } else if ast::Pat::can_cast(kind) {
            ast::Pat::cast(syntax).map(SpanAst::Pat)
        } else {
            ast::Type::cast(syntax).map(SpanAst::Type)
        }
    }

    fn syntax(&self) -> &syntax::SyntaxNode {
        match self {
            SpanAst::Expr(it) => it.syntax(),
            SpanAst::Pat(it) => it.syntax(),
            SpanAst::Type(it) => it.syntax(),
        }
    }
}

pub type SpanSyntax = InFile<AstPtr<SpanAst>>;

macro_rules! diagnostics {
    ($AnyDiagnostic:ident <$db:lifetime> -> $($diag:ident $(<$lt:lifetime>)?,)*) => {
        #[derive(Debug)]
        pub enum $AnyDiagnostic<$db> {$(
            $diag(Box<$diag $(<$lt>)?>),
        )*}

        $(
            impl<$db> From<$diag $(<$lt>)?> for $AnyDiagnostic<$db> {
                fn from(d: $diag $(<$lt>)?) -> $AnyDiagnostic<$db> {
                    // alloc: error path, one box per reported diagnostic
                    $AnyDiagnostic::$diag(Box::new(d))
                }
            }
        )*
    };
}

diagnostics![AnyDiagnostic<'db> ->
    AmbiguousAssociatedItem<'db>,
    ArrayPatternWithoutFixedLength,
    AwaitOutsideOfAsync,
    BorrowckError,
    BorrowckSkipped,
    BreakOutsideOfLoop,
    UnlabeledJumpInLabeledBlock,
    AsyncCoroutinesNotSupported,
    ConstCheckError,
    ConstImpure,
    CannotBeDereferenced<'db>,
    UnaryOperatorCannotBeApplied<'db>,
    BinaryOperatorCannotBeApplied<'db>,
    CannotImplicitlyDerefTraitObject<'db>,
    CannotIndexInto<'db>,
    CastToUnsized<'db>,
    EditionDependentMethodCall,
    ExpectedArrayOrSlicePat<'db>,
    ExpectedFunction<'db>,
    ExplicitDropMethodUse,
    FruInDestructuringAssignment,
    FullyQualifiedCallFrom2018<'db>,
    MissingBody,
    FunctionalRecordUpdateOnNonStruct,
    GenericDefaultRefersToSelf,
    InactiveCode,
    IncoherentImpl,
    IncorrectCase,
    IncorrectGenericsLen,
    IncorrectGenericsOrder,
    InferVarsNotAllowed,
    InvalidCast<'db>,
    InvalidDeriveTarget,
    InvalidLhsOfAssignment,
    InvalidRangePatType<'db>,
    LateBoundLifetimeArgs,
    MacroDefError,
    MacroError,
    MacroExpansionParseError,
    MalformedDerive,
    MethodCallIllegalSizedBound,
    MismatchedArgCount,
    IncorrectArguments,
    MismatchedTupleStructPatArgCount,
    MissingFields,
    MissingMatchArms<'db>,
    MissingUnsafe,
    MutRefInImmRefPat,
    MutableRefBinding,
    NonExhaustiveLet<'db>,
    EmptyRangePattern,
    PatternComplexityLimit,
    MixedDerefPatterns,
    ConstPatternRefused<'db>,
    NonExhaustiveRecordExpr,
    NonExhaustiveRecordPat,
    NoSuchField,
    MismatchedArrayPatLen,
    DuplicateField,
    PatternArgInExternFn,
    PrivateAssocItem,
    PrivateField,
    RemoveTrailingReturn,
    RemoveUnnecessaryElse,
    UnusedMustUse<'db>,
    ReplaceFilterMapNextWithFindMap,
    TraitImplIncorrectSafety,
    TraitImplMissingAssocItems,
    TraitImplOrphan,
    TraitImplRedundantAssocItems,
    TraitItemImport,
    TypedHole<'db>,
    TypeMismatch<'db>,
    UndeclaredLabel,
    UnimplementedBuiltinMacro,
    UnreachableLabel,
    UnresolvedAssocItem<'db>,
    UnresolvedExternCrate,
    UnresolvedField<'db>,
    UnresolvedImport,
    UnresolvedMacroCall,
    UnresolvedMethodCall<'db>,
    UnresolvedModule,
    UnresolvedIdent,
    UnresolvedPath,
    GenericArgsProhibited,
    ParenthesizedGenericArgsWithoutFnTrait,
    BadRtn,
    MissingLifetime,
    MissingLifetimes,
    ElidedLifetimesInPath,
    TypeMustBeKnown<'db>,
    UnionExprMustHaveExactlyOneField,
    UnionPatMustHaveExactlyOneField,
    UnionPatHasRest,
    UnimplementedTrait<'db>,
    ClosureKindMismatch,
    TraitSolverError<'db>,
    YieldOutsideCoroutine,
    ReturnOutsideFunction,
    UnsafeBinderCastOfNonBinder<'db>,
];

#[derive(Debug)]
pub struct BreakOutsideOfLoop {
    pub expr: InFile<ExprOrPatPtr>,
    pub is_break: bool,
    pub bad_value_break: bool,
}

/// E0695: an unlabeled `break` (`is_break`) or `continue` inside a labeled block.
#[derive(Debug)]
pub struct UnlabeledJumpInLabeledBlock {
    pub expr: InFile<ExprOrPatPtr>,
    pub is_break: bool,
}

/// E0727: a `yield` in an `async` body.
#[derive(Debug)]
pub struct AsyncCoroutinesNotSupported {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct TypedHole<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub expected: Type<'db>,
}

#[derive(Debug)]
pub struct UnresolvedModule {
    pub decl: InFile<AstPtr<ast::Module>>,
    pub candidates: Box<[String]>,
}

#[derive(Debug)]
pub struct UnresolvedExternCrate {
    pub decl: InFile<AstPtr<ast::ExternCrate>>,
}

#[derive(Debug)]
pub struct UnresolvedImport {
    pub decl: InFile<AstPtr<ast::UseTree>>,
}

/// An import of a trait's associated items that rustc reports (E0658 without
/// `import_trait_associated_functions`), or whose items blaze cannot list.
#[derive(Debug)]
pub struct TraitItemImport {
    pub decl: InFile<AstPtr<ast::UseTree>>,
    pub why: hir_def::nameres::diagnostics::TraitItemImport,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnresolvedMacroCall {
    pub range: InFile<TextRange>,
    pub path: ModPath,
    pub is_bang: bool,
}
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnreachableLabel {
    pub node: InFile<AstPtr<ast::Lifetime>>,
    pub name: Name,
}

#[derive(Debug)]
pub struct AwaitOutsideOfAsync {
    pub node: InFile<AstPtr<ast::AwaitExpr>>,
    pub location: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UndeclaredLabel {
    pub node: InFile<AstPtr<ast::Lifetime>>,
    pub name: Name,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct InactiveCode {
    pub node: InFile<SyntaxNodePtr>,
    pub cfg: CfgExpr,
    pub opts: CfgOptions,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MacroError {
    pub range: InFile<TextRange>,
    pub message: String,
    pub error: bool,
    pub kind: &'static str,
    /// A builtin error's code: 802 for E0802.
    pub code: Option<u16>,
    /// A builtin error's primary spans after `range`.
    pub also_at: Box<[InFile<TextRange>]>,
    /// A builtin error's notes and helps, in rustc's order.
    pub children: Box<[(BuiltinChild, Box<str>)]>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MacroExpansionParseError {
    pub range: InFile<TextRange>,
    pub errors: Arc<[SyntaxError]>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MacroDefError {
    pub node: InFile<AstPtr<ast::Macro>>,
    pub message: String,
    pub name: Option<TextRange>,
}

#[derive(Debug)]
pub struct UnimplementedBuiltinMacro {
    pub node: InFile<SyntaxNodePtr>,
}

#[derive(Debug)]
pub struct InvalidDeriveTarget {
    pub range: InFile<TextRange>,
}

#[derive(Debug)]
pub struct MalformedDerive {
    pub range: InFile<TextRange>,
}

#[derive(Debug)]
pub struct NoSuchField {
    pub field: InFile<AstPtr<Either<ast::RecordExprField, ast::RecordPatField>>>,
    pub private: Option<Field>,
    pub variant: VariantId,
}

#[derive(Debug)]
pub struct DuplicateField {
    pub field: InFile<AstPtr<Either<ast::RecordExprField, ast::RecordPatField>>>,
    pub variant: Variant,
}

#[derive(Debug)]
pub struct PrivateAssocItem {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    pub item: AssocItem,
}

#[derive(Debug)]
pub struct MismatchedTupleStructPatArgCount {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    pub expected: usize,
    pub found: usize,
    /// The struct or variant the pattern names.
    pub variant: Variant,
    /// Whether the pattern's type did not match the scrutinee's; rustc suggests parentheses
    /// only when it did (`emit_err_pat_wrong_number_of_fields`).
    pub had_err: bool,
}

#[derive(Debug)]
pub struct MismatchedArrayPatLen {
    pub pat: InFile<ExprOrPatPtr>,
    pub expected: u64,
    pub found: u64,
    pub has_rest: bool,
}

#[derive(Debug)]
pub struct ArrayPatternWithoutFixedLength {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct ExpectedArrayOrSlicePat<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
}

#[derive(Debug)]
pub struct InvalidRangePatType<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    /// Each end's type as inference had it when it checked that end (an undecided literal's
    /// as `{integer}`), and whether that end is not `char` or numeric.
    pub lhs: Option<(Type<'db>, bool)>,
    pub rhs: Option<(Type<'db>, bool)>,
}

#[derive(Debug)]
pub struct ExpectedFunction<'db> {
    pub call: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
}

#[derive(Debug)]
pub struct CannotBeDereferenced<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
    /// `found` as rustc prints it, at the error: an integer or float literal's type not
    /// decided yet is `{integer}` / `{float}` (after fallback `found` says `i32` / `f64`).
    /// `None` when it held none.
    pub at_error: Option<String>,
}

#[derive(Debug)]
pub struct UnaryOperatorCannotBeApplied<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub op: ast::UnaryOp,
    pub found: Type<'db>,
    /// `found` as rustc prints it, at the error: an integer or float literal's type not
    /// decided yet is `{integer}` / `{float}` (after fallback `found` says `i32` / `f64`).
    /// `None` when it held none.
    pub at_error: Option<String>,
    /// The leaf goal of each fulfillment error of `found: Trait` (rustc's `errors` of
    /// `check_user_unop`), empty when the trait is missing or the goal holds.
    pub unmet: Vec<SolverGoal<'db>>,
}

#[derive(Debug)]
pub struct BinaryOperatorCannotBeApplied<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub op: ast::BinaryOp,
    pub lhs: Type<'db>,
    pub rhs: Type<'db>,
    /// `lhs` and `rhs` as rustc prints them, at the error ("cannot add `{integer}` to
    /// `bool`"; after fallback `lhs` says `i32`). `None` when neither held such a type.
    pub at_error: Option<(String, String)>,
    /// The leaf goal of each fulfillment error of `lhs: Trait<rhs>` (rustc's `errors` of
    /// `check_overloaded_binop`, `X: PartialEq` for `[X; 3] == [X; 3]`), empty when the
    /// trait is missing or the goal holds.
    pub unmet: Vec<SolverGoal<'db>>,
    /// The type expected of the operation (`ObligationCauseCode::BinOp`'s `output_ty`).
    pub output: Option<Type<'db>>,
}

#[derive(Debug)]
pub struct MutRefInImmRefPat {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct CannotImplicitlyDerefTraitObject<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
}

#[derive(Debug)]
pub struct CannotIndexInto<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub found: Type<'db>,
    /// `found` as rustc prints it, at the error: an integer or float literal's type not
    /// decided yet is `{integer}` / `{float}` (after fallback `found` says `i32` / `f64`).
    /// `None` when it held none.
    pub at_error: Option<String>,
}

#[derive(Debug)]
pub struct ExplicitDropMethodUse {
    pub expr_or_path: Either<InFile<AstPtr<ast::MethodCallExpr>>, InFile<AstPtr<ast::Path>>>,
}

/// An error of rustc's borrow checker.
#[derive(Debug)]
pub struct BorrowckError {
    pub node: InFile<SyntaxNodePtr>,
    /// The span is the brace that closes `node`, where rustc has a scope end.
    pub at_end: bool,
    pub code: &'static str,
    pub message: String,
    /// rustc's `span_label`s, in the order rustc adds them; one at the primary span is its
    /// own. A part whose source is not in the file (what a macro made with no syntax of its
    /// own) is left out.
    pub labels: Vec<(InFile<TextRange>, String)>,
    /// rustc's notes and helps, in the order rustc adds them.
    pub children: Vec<BorrowckChild>,
    /// rustc's suggestions, in the order rustc adds them.
    pub suggestions: Vec<BorrowckSuggestion>,
}

pub use hir_ty_borrowck::{Applicability as BorrowckApplicability, Sublevel as BorrowckSublevel};

/// A note or help of a [`BorrowckError`].
#[derive(Debug)]
pub struct BorrowckChild {
    pub level: BorrowckSublevel,
    pub message: String,
    pub spans: Vec<InFile<TextRange>>,
    /// Labels of spans of the child's `MultiSpan`, which need not be among `spans`.
    pub labels: Vec<(InFile<TextRange>, String)>,
}

/// A suggested edit of a [`BorrowckError`]: each part replaces the source of its range.
#[derive(Debug)]
pub struct BorrowckSuggestion {
    pub message: String,
    pub parts: Vec<(InFile<TextRange>, String)>,
    pub applicability: BorrowckApplicability,
    /// rustc's `span_suggestion_verbose`: shown as a diff of its own, never inline.
    pub verbose: bool,
}

type BorrowckParts =
    (Vec<(InFile<TextRange>, String)>, Vec<BorrowckChild>, Vec<BorrowckSuggestion>);

/// The ranges of `diagnostic`'s labels, notes and suggestions, the nodes of its spans from
/// `node_of`. A label or note span with no node is left out, and so is a suggestion any part
/// of which has none.
fn borrowck_parts(
    db: &Db,
    diagnostic: &hir_ty_borrowck::BorrowckDiagnostic,
    node_of: &impl Fn(hir_ide::mir::MirSpan) -> Option<InFile<SyntaxNodePtr>>,
) -> BorrowckParts {
    use hir_ide::mir::MirSpan;
    use hir_ty_borrowck::PartSpan;
    let syntax = |node: InFile<SyntaxNodePtr>| {
        let root = node.file_id.parse_or_expand(db);
        node.map(|it| it.to_node(&root))
    };
    let expr = |expr| node_of(MirSpan::ExprId(expr));
    let range = |part: PartSpan| -> Option<InFile<TextRange>> {
        Some(match part {
            PartSpan::At(span @ MirSpan::ExprEnd(_)) => {
                let node = syntax(node_of(span)?);
                let whole = node.value.text_range();
                let range = node.value.last_token().map_or(whole, |it| it.text_range());
                node.with_value(range)
            }
            PartSpan::At(span) => node_of(span)?.map(|it| it.text_range()),
            PartSpan::Before(span) => {
                node_of(span)?.map(|it| TextRange::empty(it.text_range().start()))
            }
            PartSpan::After(span) => {
                node_of(span)?.map(|it| TextRange::empty(it.text_range().end()))
            }
            PartSpan::Head(outer, inner) => {
                let (outer, inner) = (expr(outer)?, expr(inner)?);
                if outer.file_id != inner.file_id {
                    return None;
                }
                let start = outer.value.text_range().start();
                outer.with_value(TextRange::new(start, inner.value.text_range().start()))
            }
            PartSpan::Tail(outer, inner) => {
                let (outer, inner) = (expr(outer)?, expr(inner)?);
                if outer.file_id != inner.file_id {
                    return None;
                }
                let end = outer.value.text_range().end();
                outer.with_value(TextRange::new(inner.value.text_range().end(), end))
            }
            PartSpan::MethodName(call) => {
                let node = syntax(expr(call)?);
                let name = ast::MethodCallExpr::cast(node.value)?.name_ref()?;
                InFile::new(node.file_id, name.syntax().text_range())
            }
            // rustc's `fn_decl_span`: a closure through its parameters and return type, a
            // block all of it.
            PartSpan::ClosureHead(closure) => {
                let node = syntax(expr(closure)?);
                let range = node.value.text_range();
                let end = ast::ClosureExpr::cast(node.value).and_then(|it| {
                    let ret = it.ret_type().map(|it| it.syntax().text_range().end());
                    ret.or_else(|| Some(it.param_list()?.syntax().text_range().end()))
                });
                let end = end.map_or(range, |end| TextRange::new(range.start(), end));
                InFile::new(node.file_id, end)
            }
            // rustc's `def_span`: from the item's start, past its attributes, through its name
            // and generics.
            PartSpan::Def(adt) => {
                let (file_id, item) = match adt {
                    AdtId::StructId(it) => {
                        let source = hir_def::src::HasSource::source(it.lookup(db), db);
                        (source.file_id, ast::Adt::Struct(source.value))
                    }
                    AdtId::EnumId(it) => {
                        let source = hir_def::src::HasSource::source(it.lookup(db), db);
                        (source.file_id, ast::Adt::Enum(source.value))
                    }
                    AdtId::UnionId(it) => {
                        let source = hir_def::src::HasSource::source(it.lookup(db), db);
                        (source.file_id, ast::Adt::Union(source.value))
                    }
                };
                let start = item
                    .syntax()
                    .children_with_tokens()
                    .find(|it| {
                        !matches!(
                            it.kind(),
                            syntax::SyntaxKind::ATTR
                                | syntax::SyntaxKind::COMMENT
                                | syntax::SyntaxKind::WHITESPACE
                        )
                    })?
                    .text_range()
                    .start();
                let end = match ast::HasGenericParams::generic_param_list(&item) {
                    Some(generics) => generics.syntax().text_range().end(),
                    None => item.name()?.syntax().text_range().end(),
                };
                InFile::new(file_id, TextRange::new(start, end))
            }
        })
    };
    let labels = |labels: &[(PartSpan, String)]| -> Vec<(InFile<TextRange>, String)> {
        // alloc: error path, the parts of a reported borrowck error
        labels.iter().filter_map(|(at, text)| Some((range(*at)?, text.clone()))).collect()
    };
    let children = diagnostic
        .children
        .iter()
        .map(|child| BorrowckChild {
            level: child.level,
            // clone: error path, a reported borrowck error's note
            message: child.message.clone(),
            // alloc: error path, a reported borrowck error's note
            spans: child.spans.iter().filter_map(|&it| range(it)).collect(),
            labels: labels(&child.labels),
        })
        // alloc: error path, the notes of a reported borrowck error
        .collect();
    let suggestions = diagnostic
        .suggestions
        .iter()
        .filter_map(|suggestion| {
            let parts = suggestion
                .parts
                .iter()
                // clone: error path, a reported borrowck error's suggestion
                .map(|(at, text)| Some((range(*at)?, text.clone())))
                .collect::<Option<Vec<_>>>()?;
            Some(BorrowckSuggestion {
                // clone: error path, a reported borrowck error's suggestion
                message: suggestion.message.clone(),
                parts,
                applicability: suggestion.applicability,
                verbose: suggestion.verbose,
            })
        })
        // alloc: error path, the suggestions of a reported borrowck error
        .collect();
    (labels(&diagnostic.labels), children, suggestions)
}

/// An error of rustc's const checker: what a `const fn`, a `const` or a `static` may not do.
#[derive(Debug)]
pub struct ConstCheckError {
    pub node: InFile<SyntaxNodePtr>,
    pub code: &'static str,
    pub message: String,
}

/// A `const fn` whose result at run time depends on more than its arguments: "const fn reads
/// or writes mutable state: `CALLS` is `AtomicU64`, which is not `Freeze`".
#[derive(Debug)]
pub struct ConstImpure {
    pub node: InFile<SyntaxNodePtr>,
    pub message: String,
}

/// A body the borrow checker skipped, because MIR lowering does not support something in it:
/// always an error, as blaze has not checked what rustc would.
#[derive(Debug)]
pub struct BorrowckSkipped {
    pub node: InFile<SyntaxNodePtr>,
    pub reason: String,
}

#[derive(Debug)]
pub struct FruInDestructuringAssignment {
    pub node: InFile<AstPtr<ast::Expr>>,
}

#[derive(Debug)]
pub struct MissingBody {
    pub node: InFile<SyntaxNodePtr>,
    pub kind: MissingBodyItemKind,
}

#[derive(Debug)]
pub struct FunctionalRecordUpdateOnNonStruct {
    pub base_expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct UnresolvedField<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub receiver: Type<'db>,
    pub name: Name,
    pub method_with_same_name_exists: bool,
}

#[derive(Debug)]
pub struct UnresolvedMethodCall<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub receiver: Type<'db>,
    pub name: Name,
    pub field_with_same_name: Option<Type<'db>>,
    pub assoc_func_with_same_name: Option<Function>,
}

#[derive(Debug)]
pub struct UnresolvedAssocItem<'db> {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    /// The type the item was looked up on.
    pub ty: Type<'db>,
    pub name: Name,
}

#[derive(Debug)]
pub struct UnresolvedIdent {
    pub node: InFile<(ExprOrPatPtr, Option<TextRange>)>,
}

/// A path of a body (an expression, a struct literal's, a pattern's) that resolves to nothing:
/// `segment` indexes the first of its segments that fails, the written segments after any
/// `crate`/`self`/`super`/`$crate`. A prefix failing is rustc's E0433; the last segment E0425
/// for a value, E0422 for a struct literal or record pattern, E0531 for a tuple struct
/// pattern.
#[derive(Debug)]
pub struct UnresolvedPath {
    pub node: InFile<ExprOrPatPtr>,
    pub segment: u32,
}

#[derive(Debug)]
pub struct PrivateField {
    pub expr: InFile<ExprOrPatPtr>,
    pub field: Field,
}

pub use hir_ty_unsafeck::{UnsafeLint, UnsafetyReason};

/// An operation that needs `unsafe` where it is not: `hir_ty_unsafeck::UnsafeOp` at its syntax.
#[derive(Debug)]
pub struct MissingUnsafe {
    pub node: InFile<ExprOrPatPtr>,
    pub lint: UnsafeLint,
    pub reason: UnsafetyReason,
    /// The first `unsafe_op_in_unsafe_fn` of its function or closure, where rustc notes that an
    /// unsafe function restricts its caller.
    pub first: bool,
    /// In the body of an `unsafe fn`, outside a block: a [`UnsafeLint::DeprecatedSafe2024`]
    /// there is given only where `unsafe_op_in_unsafe_fn` is not allowed.
    pub in_unsafe_fn: bool,
}

#[derive(Debug)]
pub struct MissingFields {
    pub file: HirFileId,
    pub field_list_parent: AstPtr<Either<ast::RecordExpr, ast::RecordPat>>,
    pub field_list_parent_path: Option<AstPtr<ast::Path>>,
    /// The struct or variant built or matched; `missed_fields` is empty for a struct expression
    /// whose `..` has no base and leaves out only fields with defaults.
    pub variant: Variant,
    pub missed_fields: Vec<(Name, Field)>,
}

#[derive(Debug)]
pub struct ReplaceFilterMapNextWithFindMap {
    pub file: HirFileId,
    /// This expression is the whole method chain up to and including `.filter_map(..).next()`.
    pub next_expr: AstPtr<ast::Expr>,
}

#[derive(Debug)]
pub struct MismatchedArgCount {
    pub call_expr: InFile<ExprOrPatPtr>,
    pub expected: usize,
    pub found: usize,
    /// True when the call is through a `Fn`/`FnMut`/`FnOnce` trait (E0057)
    /// rather than a regular function call (E0061).
    pub is_fn_trait_call: bool,
    pub args: CallArgs,
}

/// rustc's `report_arg_errors` of a call whose argument count fits but some argument does not
/// (E0308 "arguments to this function are incorrect"); each such argument also has its
/// `TypeMismatch`.
#[derive(Debug)]
pub struct IncorrectArguments {
    pub call_expr: InFile<ExprOrPatPtr>,
    pub args: CallArgs,
}

/// What rustc's `report_arg_errors` starts from, every type as rustc prints it at the error
/// (`{integer}` for an undecided literal, `{type error}` for any other variable).
#[derive(Debug)]
pub struct CallArgs {
    /// rustc's `callee_ty`: the callee's type after adjustments, the receiver's of a method call.
    pub callee: String,
    /// The method a method call resolved to.
    pub method: Option<Function>,
    pub is_method: bool,
    pub c_variadic: bool,
    /// The callee's inputs, without a method's receiver.
    pub formal_inputs: Vec<String>,
    /// `formal_inputs` as the call's expectation has them.
    pub expected_inputs: Vec<String>,
    /// The provided arguments' types after adjustments (of a C-variadic call, those of its
    /// fixed inputs only).
    pub provided: Vec<String>,
    compatible_ends: Box<[u32]>,
    compatible: Box<[u32]>,
}

impl CallArgs {
    /// `ArgMatrix::new`'s row of the `provided`th argument: the inputs it fits, in order.
    pub fn compatible_with(&self, provided: usize) -> &[u32] {
        let start = provided.checked_sub(1).map_or(0, |it| self.compatible_ends[it] as usize);
        &self.compatible[start..self.compatible_ends[provided] as usize]
    }

    fn new(db: &Db, target: DisplayTarget, args: &hir_ide::CallArguments) -> Self {
        // alloc: error path, the types of a reported call's arguments as rustc prints them
        let show = |ty: &StoredTy| ty.as_ref().display(db, target).to_string();
        // alloc: error path, as above
        let shows = |tys: &[StoredTy]| tys.iter().map(show).collect();
        CallArgs {
            callee: show(&args.callee),
            method: args.method.map(Into::into),
            is_method: args.is_method,
            c_variadic: args.c_variadic,
            formal_inputs: shows(&args.formal_inputs),
            expected_inputs: shows(&args.expected_inputs),
            provided: shows(&args.provided),
            // clone: error path, out of the inference result a reported call borrows
            compatible_ends: args.compatible_ends.clone(),
            // clone: error path, as above
            compatible: args.compatible.clone(),
        }
    }
}

/// E0004.
#[derive(Debug)]
pub struct MissingMatchArms<'db> {
    pub scrutinee_expr: InFile<AstPtr<ast::Expr>>,
    pub scrutinee_ty: Type<'db>,
    /// The match has no arms: rustc says "type `T` is non-empty" for a type that is not an
    /// enum with variants.
    pub no_arms: bool,
    /// Every pattern not covered, as rustc prints it (`print_witness_pat`).
    pub witnesses: Vec<String>,
    /// The variants of the scrutinee's enum the first witnesses are of, for "not covered"
    /// labels at them (`maybe_point_at_variant`).
    pub variants: Vec<EnumVariant>,
    /// The types in the first witness rustc adds a note about (`collect_special_tys`).
    pub special_tys: Vec<(Type<'db>, SpecialTy)>,
    pub would_be_exhaustive_without_guards: bool,
    /// The scrutinee is a reference to a type uninhabited from here: "references are always
    /// considered inhabited".
    pub ref_to_uninhabited: bool,
    /// The arms whose pattern is a constant that looks like a binding, in arm order.
    pub consts_like_bindings: Vec<(InFile<AstPtr<ast::Pat>>, Const)>,
}

/// E0005: a refutable pattern of a `let`, a parameter, a `for` loop or a destructuring
/// assignment (whose pattern is an expression).
#[derive(Debug)]
pub struct NonExhaustiveLet<'db> {
    pub pat: InFile<ExprOrPatPtr>,
    pub origin: BindingOrigin,
    /// The `let` has an initializer, for "consider using `if let`" or "`let else`".
    pub has_initializer: bool,
    pub pattern_ty: Type<'db>,
    /// As [`MissingMatchArms::witnesses`].
    pub witnesses: Vec<String>,
    pub variants: Vec<EnumVariant>,
    pub special_tys: Vec<(Type<'db>, SpecialTy)>,
    /// The first witness's variant is uninhabited only through fields not visible here.
    pub privately_uninhabited: bool,
    /// The pattern is a constant that looks like a binding.
    pub interpreted_as_const: Option<Const>,
}

/// E0030 (`lo..=hi` with `lo > hi`, `inclusive`) or E0579 (`lo..hi` with `lo >= hi`).
#[derive(Debug)]
pub struct EmptyRangePattern {
    pub pat: InFile<AstPtr<ast::Pat>>,
    pub inclusive: bool,
}

/// "reached pattern complexity limit", at a match's scrutinee or a binding's pattern.
#[derive(Debug)]
pub struct PatternComplexityLimit {
    pub at: InFile<SyntaxNodePtr>,
}

/// "mix of deref patterns and normal constructors".
#[derive(Debug)]
pub struct MixedDerefPatterns {
    pub deref_pat: InFile<AstPtr<ast::Pat>>,
    pub normal_pat: InFile<AstPtr<ast::Pat>>,
}

/// A constant used as a pattern that rustc's `const_to_pat` refuses, at the path that names it.
#[derive(Debug)]
pub struct ConstPatternRefused<'db> {
    pub pat: InFile<AstPtr<ast::Pat>>,
    pub error: ConstPatternError<'db>,
}

#[derive(Debug)]
pub struct NonExhaustiveRecordExpr {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct NonExhaustiveRecordPat {
    pub pat: InFile<ExprOrPatPtr>,
    pub variant: Variant,
}

#[derive(Debug)]
pub struct TypeMismatch<'db> {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    pub expected: Type<'db>,
    pub actual: Type<'db>,
    /// `expected` and `actual` printed as inference had them at the error, as rustc prints
    /// them: an integer or float literal's type not decided yet is `{integer}` / `{float}`
    /// (after fallback `expected` and `actual` say `i32` / `f64`). `None` when neither held one.
    pub at_error: Option<(String, String)>,
    pub cause: ObligationCauseCode,
}

/// Why inference required a type or a bound (rustc's `ObligationCauseCode`, the kinds the
/// front end records), for the notes rustc adds to an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObligationCauseCode {
    /// Nothing more to say than the span.
    Misc,
    /// The `bound_index`th predicate of `item` (counting its parents' first), required by a use
    /// of `item` in an expression: rustc's "required by a bound in `item`".
    WhereClauseInExpr { item: GenericDef, bound_index: u32, arg_index: Option<u32> },
    /// A function argument's type must be `Sized`.
    SizedArgumentType,
    /// The declared return type of a body must be `Sized`.
    SizedReturnType,
    /// The tail of a block, or a `match`'s first arm (with no arm of a type other than `!`
    /// before it).
    BlockTailExpression,
    /// The value of a `return` or of a body's tail.
    ReturnValue,
    /// The `else` of an `if` must agree with its `then`.
    IfExpression,
    /// A `match` arm must agree with the `prior_arms` arms before it whose type is not `!`.
    MatchExpressionArm { prior_arms: u32 },
    /// A `let`'s type must be `Sized`.
    LetType,
    /// An `if` without an `else` is `()`, which must agree with what the `if` is expected to be.
    IfExpressionWithNoElse,
    /// A `return` without a value returns `()`, which must agree with the return type.
    ReturnNoExpression,
}

impl From<hir_ide::next_solver::infer::traits::ObligationCauseCode> for ObligationCauseCode {
    fn from(code: hir_ide::next_solver::infer::traits::ObligationCauseCode) -> Self {
        use hir_ide::next_solver::infer::traits::ObligationCauseCode as C;
        match code {
            C::Misc => Self::Misc,
            C::WhereClauseInExpr { item, bound_index, arg_index } => {
                Self::WhereClauseInExpr { item: item.into(), bound_index, arg_index }
            }
            C::SizedArgumentType => Self::SizedArgumentType,
            C::SizedReturnType => Self::SizedReturnType,
            C::BlockTailExpression => Self::BlockTailExpression,
            C::ReturnValue => Self::ReturnValue,
            C::IfExpression => Self::IfExpression,
            C::MatchExpressionArm { prior_arms } => Self::MatchExpressionArm { prior_arms },
            C::LetType => Self::LetType,
            C::IfExpressionWithNoElse => Self::IfExpressionWithNoElse,
            C::ReturnNoExpression => Self::ReturnNoExpression,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct IncoherentImpl {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplOrphan {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
}

// FIXME: Split this off into the corresponding 4 rustc errors
#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplIncorrectSafety {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
    pub should_be_safe: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplMissingAssocItems {
    pub file_id: HirFileId,
    pub impl_: AstPtr<ast::Impl>,
    pub missing: Vec<(Name, AssocItem)>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TraitImplRedundantAssocItems {
    pub file_id: HirFileId,
    pub trait_: Trait,
    pub impl_: AstPtr<ast::Impl>,
    pub assoc_item: (Name, AssocItem),
}

#[derive(Debug)]
pub struct RemoveTrailingReturn {
    pub return_expr: InFile<AstPtr<ast::ReturnExpr>>,
}

#[derive(Debug)]
pub struct RemoveUnnecessaryElse {
    pub if_expr: InFile<AstPtr<ast::IfExpr>>,
}

#[derive(Debug)]
pub struct UnusedMustUse<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub message: Option<&'db str>,
}

#[derive(Debug)]
pub struct CastToUnsized<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub cast_ty: Type<'db>,
}

#[derive(Debug)]
pub struct InvalidCast<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub error: CastError,
    pub expr_ty: Type<'db>,
    pub cast_ty: Type<'db>,
    /// rustc's `PointerKind` of the expression's pointee, where the check of a pointer cast
    /// found it: `DifferingKinds`, `SizedUnsizedCast`, `NeedViaThinPtr`, and
    /// `UnknownCastPtrKind` of a cast to a wide pointer of unknown pointee.
    pub expr_pointer: Option<CastPointerKind>,
    /// Likewise of the pointee of the type cast to: also `IntToWideCast` and the
    /// `IllegalCast` of a function pointer.
    pub cast_pointer: Option<CastPointerKind>,
    /// Of `PtrPtrAddingAutoTraits`: the auto traits the target `dyn` adds, in its order.
    pub added_auto_traits: Vec<crate::Trait>,
}

/// The metadata of a pointer, as rustc's cast errors tell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastPointerKind {
    Thin,
    /// A trait object, with its principal trait if it has one.
    VTable(Option<crate::Trait>),
    /// A slice or `str`.
    Length,
    /// A projection or opaque type's.
    OfAlias,
    /// A type parameter's.
    OfParam(crate::TypeParam),
}

#[derive(Debug)]
pub struct GenericArgsProhibited {
    /// The first of `all_args`.
    pub args: InFile<AstPtr<Either<ast::GenericArgList, ast::ParenthesizedArgList>>>,
    /// The arguments of every segment rustc's one `prohibit_generic_args` checks, in order:
    /// a path's leading segments, with the last when it is a type parameter, `Self`, a
    /// primitive type or a module. Return type notation is left out: segments of only that
    /// are `BadRtn`.
    pub all_args: Box<[InFile<AstPtr<Either<ast::GenericArgList, ast::ParenthesizedArgList>>>]>,
    /// The last segment's reason.
    pub reason: GenericArgsProhibitedReason,
}

#[derive(Debug)]
pub struct ParenthesizedGenericArgsWithoutFnTrait {
    pub args: InFile<AstPtr<ast::ParenthesizedArgList>>,
}

#[derive(Debug)]
pub struct BadRtn {
    pub rtn: InFile<AstPtr<ast::ReturnTypeSyntax>>,
}

#[derive(Debug)]
pub struct InferVarsNotAllowed {
    /// The first of `nodes`.
    pub node: InFile<SyntaxNodePtr>,
    /// Every placeholder of the error in source order: one, or all of a type its body infers
    /// (a return type, a const's or a static's), as rustc reports them together.
    pub nodes: Box<[InFile<SyntaxNodePtr>]>,
}

#[derive(Debug)]
pub struct IncorrectGenericsLen {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub kind: IncorrectGenericsLenKind,
    pub provided: u32,
    pub expected: u32,
    pub def: GenericDef,
}

#[derive(Debug)]
pub struct MissingLifetime {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub expected: u32,
    pub def: GenericDef,
    /// [`MissingLifetimeKind::Missing`] or [`MissingLifetimeKind::ElisionFailure`].
    pub kind: MissingLifetimeKind,
}

/// Lifetimes left out with none to elide to: one site, or for
/// [`MissingLifetimeKind::ElisionFailure`] every site of one return type, rustc's one error.
#[derive(Debug)]
pub struct MissingLifetimes {
    pub sites: Box<[MissingLifetimeSite]>,
    pub kind: MissingLifetimeKind,
}

#[derive(Debug)]
pub enum MissingLifetimeSite {
    /// `&T`.
    Reference(InFile<AstPtr<ast::RefType>>),
    /// `'_`.
    Placeholder(InFile<AstPtr<ast::Lifetime>>),
    /// A path to `def` without its lifetime arguments.
    Path {
        /// Points at the name if there are no generics.
        generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
        expected: u32,
        def: GenericDef,
    },
}

/// Lifetime arguments given to a function with late-bound lifetime parameters: E0794 when
/// `hard_error`, else the `late_bound_lifetime_arguments` lint.
#[derive(Debug)]
pub struct LateBoundLifetimeArgs {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub def: GenericDef,
    pub hard_error: bool,
}

/// `Self::Name` in `impl Trait for Enum` where `Enum` has a variant `Name`, resolved to
/// `Trait`'s associated type `item`: the `ambiguous_associated_items` lint.
#[derive(Debug)]
pub struct AmbiguousAssociatedItem<'db> {
    pub path: InFile<AstPtr<ast::Path>>,
    pub variant: Variant,
    pub item: TypeAlias,
    /// The impl's self type, the enum.
    pub self_ty: Type<'db>,
    /// The trait the item is of.
    pub bound: Trait,
}

#[derive(Debug)]
pub struct ElidedLifetimesInPath {
    /// Points at the name if there are no generics.
    pub generics_or_segment: InFile<AstPtr<Either<ast::GenericArgList, ast::NameRef>>>,
    pub expected: u32,
    pub def: GenericDef,
    pub hard_error: bool,
}

#[derive(Debug)]
pub struct TypeMustBeKnown<'db> {
    pub at_point: SpanSyntax,
    pub top_term: Option<Either<Type<'db>, String>>,
    pub kind: TypeAnnotationNeeded,
    /// The goal left ambiguous, for an error of a goal still pending when inference is done
    /// (rustc's `maybe_report_ambiguity`); `None` for a type writeback found undecided.
    pub ambiguity: Option<TypeAmbiguity<'db>>,
    /// Where the undecided variable (the first of `top_term` made for a parameter) was made and
    /// the type or const parameter it was made for, declared on its `parent` (rustc's
    /// `TypeVariableOrigin`: ", where the type for type parameter `T` is specified" and the
    /// turbofish suggestion). A type parameter only when the variable was not made in a macro
    /// expansion, as rustc's `extract_inference_diagnostics_data`.
    pub origin: Option<(SpanSyntax, GenericParam)>,
    /// `top_term` as rustc prints it in "consider giving `x` an explicit type" (its
    /// `ty_infer_name_resolver`). Each undecided variable made for a parameter is that
    /// parameter, and every other one is unknown. A trait's `Self`, an `impl Trait` argument's
    /// parameter, and a type parameter whose variable was made in a macro expansion are unknown.
    pub named_term: Option<Either<Type<'db>, String>>,
    /// Why the ambiguous goal was required, for rustc's closing `note_obligation_cause`
    /// ("required by a bound in `f`"); `Misc` for a type writeback found undecided.
    pub cause: ObligationCauseCode,
}

/// An ambiguous goal and the goals rustc merges into its error as notes ("the type must also
/// implement `Tr`", "cannot satisfy `..`"): those of the same inference variable at the same
/// place.
#[derive(Debug)]
pub struct TypeAmbiguity<'db> {
    pub primary: AmbiguousGoal<'db>,
    pub related: Vec<AmbiguousGoal<'db>>,
}

#[derive(Debug)]
pub struct AmbiguousGoal<'db> {
    pub goal: SolverGoal<'db>,
    /// The impls and where clauses that could meet a trait goal, when rustc lists them
    /// (`annotate_source_of_ambiguity`): two to nine of them and an argument of the trait known.
    pub candidates: Vec<AmbiguityCandidate<'db>>,
}

#[derive(Debug)]
pub enum AmbiguityCandidate<'db> {
    Impl(crate::Impl),
    /// A where clause of the body's owner.
    WhereClause(crate::TraitRef<'db>),
}

/// A goal of the trait solver as rustc's errors print it, its undecided types `{unknown}`.
/// A term that is a constant is its text.
#[derive(Debug)]
pub enum SolverGoal<'db> {
    /// `T: Trait` or `T: !Trait`.
    Trait(crate::TraitClause<'db>),
    /// `T: [const] Trait` (`maybe_const`) or `T: const Trait`.
    HostEffect { trait_ref: crate::TraitRef<'db>, maybe_const: bool },
    /// `alias == term`: `<T as Trait>::Assoc == U`.
    Projection { alias: Either<Type<'db>, String>, term: Either<Type<'db>, String> },
    /// What `alias` normalizes to: rustc's "cannot normalize `alias`".
    NormalizesTo { alias: Either<Type<'db>, String> },
    WellFormed(Either<Type<'db>, String>),
    /// `a <: b`.
    Subtype { a: Type<'db>, b: Type<'db> },
    /// `a` coerces to `b`.
    Coerce { a: Type<'db>, b: Type<'db> },
    ConstEvaluatable(String),
    ConstArgHasType { ct: String, ty: Type<'db> },
    ConstEquate { a: String, b: String },
    DynCompatible(Trait),
    /// An impl's `#[unstable_feature_bound(feature)]`.
    UnstableFeature(intern::Symbol),
    /// `a: b`: a type, or a region printed as rustc prints it, outlives the region `b`.
    Outlives { a: Either<Type<'db>, String>, b: String },
    Ambiguous,
}

/// A trait solver error other than an unmet trait bound or a closure of the wrong kind: rustc's
/// `report_fulfillment_error` for the other codes.
#[derive(Debug)]
pub struct TraitSolverError<'db> {
    pub span: SpanSyntax,
    pub cause: ObligationCauseCode,
    pub kind: TraitSolverErrorKind<'db>,
}

#[derive(Debug)]
pub enum TraitSolverErrorKind<'db> {
    /// A goal that does not hold that is not a trait's: a `[const] Trait` bound, a trait that
    /// is not dyn-compatible (E0038), a type that is not well-formed; `non_const_impl`, for a
    /// `[const] Trait` or `const Trait` goal of a `const` trait, the impl that meets the trait
    /// goal when it is not `const` (rustc's `report_host_effect_error`).
    GoalUnmet { goal: SolverGoal<'db>, non_const_impl: Option<crate::Impl> },
    /// E0275: evaluating the goal overflowed; `suggest_increasing_limit`: rustc's help to raise
    /// `#![recursion_limit]`.
    Overflow { goal: SolverGoal<'db>, suggest_increasing_limit: bool },
    /// E0271: "type mismatch resolving `goal`"; `normalized`, what the goal's alias normalizes
    /// to where that is not the goal's term (rustc's "expected `term`, found `normalized`"): a
    /// type, or a constant's text.
    ProjectionMismatch { goal: SolverGoal<'db>, normalized: Option<Either<Type<'db>, String>> },
    /// E0308 of a subtyping or coercion goal.
    TypesDiffer { expected: Type<'db>, found: Type<'db> },
    /// "the constant `ct` is not of type `expected_ty`", which is `ct_ty`.
    ConstArgHasWrongType { ct: String, ct_ty: Type<'db>, expected_ty: Type<'db> },
    /// "higher-ranked lifetime bound could not be satisfied".
    HigherRankedOutlives,
    /// An error only rustc's old solver gives, which blaze does not describe.
    Unreported(UnreportedSolverError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericArgKind {
    Lifetime,
    Type,
    Const,
}

impl GenericArgKind {
    fn from_id(id: GenericParamId) -> Self {
        match id {
            GenericParamId::TypeParamId(_) => GenericArgKind::Type,
            GenericParamId::ConstParamId(_) => GenericArgKind::Const,
            GenericParamId::LifetimeParamId(_) => GenericArgKind::Lifetime,
        }
    }
}

#[derive(Debug)]
pub struct IncorrectGenericsOrder {
    pub provided_arg: InFile<AstPtr<ast::GenericArg>>,
    pub expected_kind: GenericArgKind,
    /// The parameter the argument met, of `expected_kind`.
    pub param: GenericParam,
}

#[derive(Debug)]
pub struct GenericDefaultRefersToSelf {
    /// The `Self` segment.
    pub segment: InFile<AstPtr<ast::PathSegment>>,
}

#[derive(Debug)]
pub struct UnionExprMustHaveExactlyOneField {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct UnionPatMustHaveExactlyOneField {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct UnionPatHasRest {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct InvalidLhsOfAssignment {
    pub lhs: InFile<AstPtr<Either<ast::Expr, ast::Pat>>>,
    /// The `=` or compound assignment, which may come from another expansion than `lhs`.
    pub assign: InFile<ExprOrPatPtr>,
    /// `None` for `=` (E0070), the operator of a compound assignment (E0067).
    pub op: Option<ast::ArithOp>,
}

#[derive(Debug)]
pub struct MethodCallIllegalSizedBound {
    pub call_expr: InFile<ExprOrPatPtr>,
    pub method: crate::Function,
    /// Whose own bounds or where clause (the method's, or its trait's or impl's) hold the
    /// `Self: Sized` bound; `None` where it is only implied (rustc gives it no label).
    pub bound: Option<crate::GenericDef>,
    /// rustc's `needs_mut`: the receiver of the other mutability picks another method.
    pub needs_mut: bool,
    /// The traits, in scope or not, a probe of every crate's traits picks the method from.
    pub candidates: Vec<crate::Trait>,
}

/// rustc's `lint_edition_dependent_dot_call`: a method call that a later edition's prelude or
/// `IntoIterator` impl resolves to another method.
#[derive(Debug)]
pub struct EditionDependentMethodCall {
    pub call_expr: InFile<AstPtr<ast::MethodCallExpr>>,
    pub receiver: InFile<ExprOrPatPtr>,
    pub lint: crate::EditionLint,
    /// The picked method, which the suggestion calls by its trait's or type's path.
    pub method: Function,
    /// An inherent or trait object pick: the suggestion only adjusts the receiver.
    pub inherent: bool,
    pub autoderefs: u32,
    pub autoref: Option<crate::ReceiverAdjustment>,
}

/// rustc's `lint_fully_qualified_call_from_2018`: a type-relative path to `try_from`,
/// `try_into` or `from_iter` that the 2021 prelude makes ambiguous.
#[derive(Debug)]
pub struct FullyQualifiedCallFrom2018<'db> {
    pub expr_or_pat: InFile<ExprOrPatPtr>,
    /// The picked item, whose trait the suggestion names.
    pub item: AssocItem,
    pub self_ty: Type<'db>,
}

#[derive(Debug)]
pub struct PatternArgInExternFn {
    pub node: InFile<AstPtr<ast::Pat>>,
}

#[derive(Debug)]
pub struct UnimplementedTrait<'db> {
    pub span: SpanSyntax,
    pub cause: ObligationCauseCode,
    pub trait_predicate: crate::TraitClause<'db>,
    pub parent_trait_predicates: Vec<crate::TraitClause<'db>>,
    /// For each of `parent_trait_predicates`, the impl that would prove it, whose where clause
    /// required the predicate nearer the error: rustc's "required for `T` to implement `Trait`"
    /// and "unsatisfied trait bound introduced here". `None` where it came from no impl.
    pub parent_impls: Vec<Option<crate::Impl>>,
    /// For each of `parent_impls`, the impl's bound the step went through (rustc's
    /// `impl_def_predicate_index`), where "unsatisfied trait bound introduced here" goes.
    pub parent_impl_bounds: Vec<Option<BoundSource>>,
}

/// Where a predicate of an item comes from, its written bound found in syntax: for rustc's
/// "required by this bound in `f`", "unsatisfied trait bound introduced here" and "the implicit
/// `Sized` requirement on this type parameter".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundSource {
    /// A bound written on a parameter, in a where clause, or an `impl Trait` argument's.
    Written(InFile<AstPtr<ast::TypeBound>>),
    /// A type parameter's implicit `Sized`.
    ImplicitSized(crate::TypeParam),
    /// A trait's own `Self: Trait`, which no one writes.
    ImpliedTrait(Trait),
    /// A const parameter's type.
    ConstParamType(crate::ConstParam),
}

impl GenericDef {
    /// The source of the item's `bound_index`th predicate, counted as
    /// [`ObligationCauseCode::WhereClauseInExpr`] counts them (parents first); `None` for one
    /// with no syntax.
    pub fn bound_source(self, db: &Db, bound_index: u32) -> Option<BoundSource> {
        bound_source(db, GenericDefId::try_from(self).ok()?, bound_index)
    }
}

pub(crate) fn bound_source(db: &Db, def: GenericDefId, bound_index: u32) -> Option<BoundSource> {
    use hir_def::hir::{
        generics::{GenericParams, WherePredicate},
        type_ref::TypeBound,
    };
    use hir_ide::PredicateSource;
    Some(match *hir_ide::predicate_sources(db, def).get(bound_index as usize)? {
        PredicateSource::ImpliedTrait(id) => BoundSource::ImpliedTrait(Trait { id }),
        PredicateSource::ImplicitSized(id) => BoundSource::ImplicitSized(crate::TypeParam { id }),
        PredicateSource::ConstParamType(id) => BoundSource::ConstParamType(crate::ConstParam { id }),
        PredicateSource::Written { def, index } => {
            let (params, _, source_map) = GenericParams::with_source_map(db, def);
            let bound = match *params.where_predicates().get(index as usize)? {
                WherePredicate::TypeBound {
                    bound: TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path),
                    ..
                } => {
                    let ptr = source_map.type_syntax(path.type_ref()).ok()?;
                    let root = ptr.file_id.parse_or_expand(db);
                    ptr.map(|it| {
                        it.to_node(&root).syntax().ancestors().find_map(ast::TypeBound::cast)
                    })
                }
                WherePredicate::TypeBound { bound: TypeBound::Lifetime(lifetime), .. }
                | WherePredicate::Lifetime { bound: lifetime, .. } => {
                    let ptr = source_map.lifetime_syntax(lifetime).ok()?;
                    let root = ptr.file_id.parse_or_expand(db);
                    ptr.map(|it| {
                        it.to_node(&root).syntax().ancestors().find_map(ast::TypeBound::cast)
                    })
                }
                WherePredicate::TypeBound { bound: TypeBound::Use(_) | TypeBound::Error, .. } => {
                    return None;
                }
            };
            let file_id = bound.file_id;
            BoundSource::Written(InFile::new(file_id, AstPtr::new(&bound.value?)))
        }
    })
}

#[derive(Debug)]
pub struct ClosureKindMismatch {
    pub span: SpanSyntax,
    pub cause: ObligationCauseCode,
    pub expected: crate::FnTrait,
    pub found: crate::FnTrait,
    /// The closure, which rustc points at; `None` when it has no syntax.
    pub closure: Option<InFile<AstPtr<ast::ClosureExpr>>>,
}

#[derive(Debug)]
pub struct MutableRefBinding {
    pub pat: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct YieldOutsideCoroutine {
    pub expr: InFile<ExprOrPatPtr>,
}

#[derive(Debug)]
pub struct ReturnOutsideFunction {
    pub expr: InFile<ExprOrPatPtr>,
    pub kind: ReturnKind,
}

/// rustc's two errors of `check_expr_unsafe_binder_cast`: for `Wrap`, "`wrap_binder!()` can
/// only wrap into unsafe binder, not {found}" (note "unsafe binders are the only valid output
/// of wrap"), at the written type or else the cast; for `Unwrap`, "expected unsafe binder,
/// found {found} as input of `unwrap_binder!()`" (note "only an unsafe binder type can be
/// unwrapped"). `expr` is the cast.
#[derive(Debug)]
pub struct UnsafeBinderCastOfNonBinder<'db> {
    pub expr: InFile<ExprOrPatPtr>,
    pub kind: UnsafeBinderCastKind,
    pub found: Type<'db>,
}

pub(crate) struct DiagnosticsCollector<'a, 'db, F: ?Sized> {
    db: &'db Db,
    krate: base_db::Crate,
    edition: Edition,
    style_lints: bool,
    acc: &'a mut Vec<AnyDiagnostic<'db>>,
    /// The bodies whose diagnostics are asked for ([`crate::check::every_body`] for all).
    bodies: &'a F,
    /// The bodies a plan checks as tasks of their own, which the walk of a module leaves
    /// ([`crate::check::bodies`]).
    deferred: Option<crate::check::bodies::Defer<'a>>,
}

/// What a [`MacroError`] keeps of a builtin error besides its text: its code, its other primary
/// spans and its notes and helps. Nothing for any other error.
fn builtin_parts(
    db: &Db,
    err: &ExpandErrorKind,
) -> (Option<u16>, Box<[InFile<TextRange>]>, Box<[(BuiltinChild, Box<str>)]>) {
    let ExpandErrorKind::Builtin(builtin) = err else {
        return (None, Box::default(), Box::default());
    };
    let also_at = builtin
        .also_at
        .iter()
        .map(|&span| {
            let at = hir_expand::resolve_span(db, span);
            InFile::new(HirFileId::from(at.file_id), at.range)
        })
        // alloc: an error's few spans, on the error path
        .collect();
    // clone: an error's few lines, on the error path
    (builtin.code, also_at, builtin.children.clone())
}

fn precise_macro_call_location(
    ast: &MacroCallKind,
    db: &Db,
    krate: base_db::Crate,
) -> InFile<TextRange> {
    // FIXME: maybe we actually want slightly different ranges for the different macro diagnostics
    // - e.g. the full attribute for macro errors, but only the name for name resolution
    match ast {
        MacroCallKind::FnLike { ast_id, .. } => {
            let node = ast_id.to_node(db);
            let range = node
                .path()
                .and_then(|it| it.segment())
                .and_then(|it| it.name_ref())
                .map(|it| it.syntax().text_range());
            let range = range.unwrap_or_else(|| node.syntax().text_range());
            ast_id.with_value(range)
        }
        MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
            let range = derive_attr_index.find_derive_range(db, krate, *ast_id, *derive_index);
            ast_id.with_value(range)
        }
        MacroCallKind::Attr { ast_id, censored_attr_ids: attr_ids, .. } => {
            let attr_range =
                attr_ids.invoc_attr().find_attr_range(db, krate, *ast_id).1.syntax().text_range();
            ast_id.with_value(attr_range)
        }
    }
}

impl<'a, 'db, F: Fn(crate::DefWithBody) -> bool + ?Sized> DiagnosticsCollector<'a, 'db, F> {
    pub(crate) fn collect(
        db: &'db Db,
        module: ModuleId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
    ) {
        Self::collect_with(db, module, acc, style_lints, bodies, None);
    }

    /// [`DiagnosticsCollector::collect`] of a module whose bodies listed in `deferred` are
    /// checked as tasks of their own: each is left where the walk meets it, its place marked.
    pub(crate) fn collect_deferred(
        db: &'db Db,
        module: ModuleId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
        deferred: crate::check::bodies::Defer<'a>,
    ) {
        Self::collect_with(db, module, acc, style_lints, bodies, Some(deferred));
    }

    fn collect_with(
        db: &'db Db,
        module: ModuleId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
        deferred: Option<crate::check::bodies::Defer<'a>>,
    ) {
        let krate = module.krate(db);
        let edition = krate.data(db).edition;
        DiagnosticsCollector { db, krate, edition, style_lints, acc, bodies, deferred }
            .collect_module(module);
    }

    /// The diagnostics of body `def`, whose types are written in `type_owner`'s generics, with
    /// those of the items inside it that `bodies` accepts: what the walk of its module gives
    /// where it meets the body.
    pub(crate) fn collect_body(
        db: &'db Db,
        def: DefWithBodyId,
        type_owner: TypeOwnerId,
        acc: &'a mut Vec<AnyDiagnostic<'db>>,
        style_lints: bool,
        bodies: &'a F,
    ) {
        let krate = hir_def::HasModule::krate(&def, db);
        let edition = krate.data(db).edition;
        DiagnosticsCollector { db, krate, edition, style_lints, acc, bodies, deferred: None }
            .check_body(def, type_owner);
    }

    fn emit_def_diagnostic(&mut self, diag: &DefDiagnosticKind) {
        match diag {
            DefDiagnosticKind::UnresolvedModule { ast: declaration, candidates } => {
                let decl = declaration.to_ptr(self.db);
                self.acc.push(
                    UnresolvedModule {
                        decl: InFile::new(declaration.file_id, decl),
                        // clone: error path, a reported diagnostic
                        candidates: candidates.clone(),
                    }
                    .into(),
                )
            }
            DefDiagnosticKind::UnresolvedExternCrate { ast } => {
                let item = ast.to_ptr(self.db);
                self.acc
                    .push(UnresolvedExternCrate { decl: InFile::new(ast.file_id, item) }.into());
            }

            DefDiagnosticKind::MacroError { ast, path, err } => {
                let item = ast.to_ptr(self.db);
                let RenderedExpandError { message, error, kind } = err.render_to_string(self.db);
                let (code, also_at, children) = builtin_parts(self.db, err);
                self.acc.push(
                    MacroError {
                        range: InFile::new(ast.file_id, item.text_range()),
                        // alloc: error path, a reported diagnostic's message
                        message: format!("{}: {message}", path.display(self.db, self.edition)),
                        error,
                        kind,
                        code,
                        also_at,
                        children,
                    }
                    .into(),
                )
            }
            DefDiagnosticKind::UnresolvedImport { id, index } => {
                let file_id = id.file_id;

                let use_tree = hir_def::src::use_tree_to_ast(self.db, *id, *index);
                self.acc.push(
                    UnresolvedImport { decl: InFile::new(file_id, AstPtr::new(&use_tree)) }.into(),
                );
            }
            DefDiagnosticKind::TraitItemImport { id, index, why } => {
                let use_tree = hir_def::src::use_tree_to_ast(self.db, *id, *index);
                let decl = InFile::new(id.file_id, AstPtr::new(&use_tree));
                self.acc.push(TraitItemImport { decl, why: *why }.into());
            }

            DefDiagnosticKind::UnconfiguredCode { ast_id, cfg } => {
                let ast_id_map = ast_id.file_id.ast_id_map(self.db);
                let ptr = ast_id_map.get_erased(ast_id.value);
                self.acc.push(
                    InactiveCode {
                        node: InFile::new(ast_id.file_id, ptr),
                        // clone: error path, a reported diagnostic
                        cfg: cfg.clone(),
                        // clone: error path, a reported diagnostic
                        opts: self.krate.cfg_options(self.db).clone(),
                    }
                    .into(),
                );
            }
            DefDiagnosticKind::UnresolvedMacroCall { ast, path } => {
                let location = precise_macro_call_location(ast, self.db, self.krate);
                self.acc.push(
                    UnresolvedMacroCall {
                        range: location,
                        // clone: error path, a reported diagnostic
                        path: path.clone(),
                        is_bang: matches!(ast, MacroCallKind::FnLike { .. }),
                    }
                    .into(),
                );
            }
            DefDiagnosticKind::UnimplementedBuiltinMacro { ast } => {
                let node = ast.to_node(self.db);
                // Must have a name, otherwise we wouldn't emit it.
                let name = node.name().expect("unimplemented builtin macro with no name");
                self.acc.push(
                    UnimplementedBuiltinMacro {
                        node: ast.with_value(SyntaxNodePtr::from(AstPtr::new(&name))),
                    }
                    .into(),
                );
            }
            DefDiagnosticKind::InvalidDeriveTarget { ast, id } => {
                let (_, attr) = id.find_attr_range(self.db, self.krate, *ast);
                let derive = attr
                    .path()
                    .map(|path| path.syntax().text_range())
                    .unwrap_or_else(|| attr.syntax().text_range());
                self.acc.push(InvalidDeriveTarget { range: ast.with_value(derive) }.into());
            }
            DefDiagnosticKind::MalformedDerive { ast, id } => {
                let derive = id.find_attr_range(self.db, self.krate, *ast).1.syntax().text_range();
                self.acc.push(MalformedDerive { range: ast.with_value(derive) }.into());
            }
            DefDiagnosticKind::MacroDefError { ast, message } => {
                let node = ast.to_node(self.db);
                self.acc.push(
                    MacroDefError {
                        node: InFile::new(ast.file_id, AstPtr::new(&node)),
                        name: node.name().map(|it| it.syntax().text_range()),
                        // clone: error path, a reported diagnostic
                        message: message.clone(),
                    }
                    .into(),
                );
            }
        }
    }

    fn emit_def_diagnostics(&mut self, diagnostics: &DefDiagnostics) {
        diagnostics.iter().for_each(|diag| self.emit_def_diagnostic(&diag.kind));
    }

    fn emit_case_diagnostics(&mut self, def: ModuleDefId) {
        self.acc
            .extend(hir_ide::diagnostics::incorrect_case(self.db, def).into_iter().map(Into::into));
    }

    fn collect_macro_call(&mut self, macro_call_id: MacroCallId) {
        let Some(e) = macro_call_id.parse_macro_expansion_error(self.db) else {
            return;
        };
        let ValueResult { value: parse_errors, err } = e;
        if let Some(err) = err {
            let loc = macro_call_id.loc(self.db);
            let file_id = loc.kind.file_id();
            let mut range = precise_macro_call_location(&loc.kind, self.db, loc.krate);
            let RenderedExpandError { message, error, kind } = err.render_to_string(self.db);
            if Some(err.span().anchor.file_id)
                == file_id.file_id().map(|it| it.span_file_id(self.db))
            {
                range.value = err.span().range
                    + file_id
                        .ast_id_map(self.db)
                        .get_erased(err.span().anchor.ast_id)
                        .text_range()
                        .start();
            }
            let (code, also_at, children) = builtin_parts(self.db, err.kind());
            self.acc.push(
                MacroError { range, message, error, kind, code, also_at, children }.into(),
            );
        }

        if !parse_errors.is_empty() {
            let loc = macro_call_id.loc(self.db);
            let range = precise_macro_call_location(&loc.kind, self.db, loc.krate);
            // clone: error path, a reported diagnostic
            self.acc.push(MacroExpansionParseError { range, errors: parse_errors.clone() }.into())
        }
    }

    fn collect_assoc_items(&mut self, defs: &[(Name, AssocItemId)], def_map: &DefMap) {
        for &(_, def) in defs {
            self.collect_module_def(def.into(), def_map);
        }
    }

    fn collect_trait(&mut self, def: TraitId, def_map: &DefMap) {
        let (signature, source_map) = TraitSignature::with_source_map(self.db, def);
        let items = TraitItems::query_with_diagnostics(self.db, def);

        self.collect_generic_def(&signature.store, source_map, def.into());
        self.collect_wfcheck(def.into());
        self.emit_def_diagnostics(&items.1);
        items.0.macro_calls().for_each(|(_, call)| self.collect_macro_call(call));
        self.collect_assoc_items(&items.0.items, def_map);
    }

    fn collect_impl(
        &mut self,
        def: ImplId,
        infcx: &InferCtxt<'db>,
        def_map: &'db DefMap,
        impl_assoc_items_scratch: &mut Vec<(Name, AssocItemId)>,
    ) {
        let (impl_signature, source_map) = ImplSignature::with_source_map(self.db, def);
        let impl_items = ImplItems::of(self.db, def);

        self.collect_generic_def(&impl_signature.store, source_map, def.into());
        self.emit_def_diagnostics(&impl_items.1);
        impl_items.0.macro_calls().for_each(|(_, call)| self.collect_macro_call(call));
        self.collect_assoc_items(&impl_items.0.items, def_map);

        let loc = def.lookup(self.db);

        let file_id = loc.id.file_id;
        if file_id.macro_file().is_some_and(|it| it.kind(self.db) == MacroKind::DeriveBuiltIn) {
            // these expansion come from us, diagnosing them is a waste of resources
            // FIXME: Once we diagnose the inputs to builtin derives, we should at least extract those diagnostics somehow
            return;
        }
        self.collect_wfcheck(def.into());

        let ast_id_map = file_id.ast_id_map(self.db);

        let trait_impl = impl_signature.target_trait.is_some();
        if !trait_impl && !is_inherent_impl_coherent(self.db, def_map, def) {
            self.acc.push(IncoherentImpl { impl_: ast_id_map.get(loc.id.value), file_id }.into())
        }

        if trait_impl && !check_orphan_rules(self.db, def) {
            self.acc.push(TraitImplOrphan { impl_: ast_id_map.get(loc.id.value), file_id }.into())
        }

        let trait_ = trait_impl
            .then(|| self.db.impl_trait(def))
            .flatten()
            .map(|trait_ref| trait_ref.instantiate_identity().skip_norm_wip().def_id.0);
        let mut trait_is_unsafe = trait_.is_some_and(|trait_| {
            TraitSignature::of(self.db, trait_).flags.contains(TraitFlags::UNSAFE)
        });
        let impl_is_negative = impl_signature.is_negative();
        let impl_is_unsafe = impl_signature.flags.contains(ImplFlags::UNSAFE);

        let trait_is_unresolved = trait_.is_none() && trait_impl;
        if trait_is_unresolved {
            // Ignore trait safety errors when the trait is unresolved, as otherwise we'll treat it as safe,
            // which may not be correct.
            trait_is_unsafe = impl_is_unsafe;
        }

        let drop_maybe_dangle = (|| {
            let trait_ = trait_?;
            let drop_trait = infcx.interner.lang_items().Drop?;
            if drop_trait != trait_ {
                return None;
            }
            let parent = def.into();
            let (lifetimes_attrs, type_and_consts_attrs) =
                AttrFlags::query_generic_params(self.db, parent);
            let res = lifetimes_attrs.values().any(|it| it.contains(AttrFlags::MAY_DANGLE))
                || type_and_consts_attrs.values().any(|it| it.contains(AttrFlags::MAY_DANGLE));
            Some(res)
        })()
        .unwrap_or(false);

        match (impl_is_unsafe, trait_is_unsafe, impl_is_negative, drop_maybe_dangle) {
                // unsafe negative impl
                (true, _, true, _) |
                // unsafe impl for safe trait
                (true, false, _, false) => self.acc.push(TraitImplIncorrectSafety { impl_: ast_id_map.get(loc.id.value), file_id, should_be_safe: true }.into()),
                // safe impl for unsafe trait
                (false, true, false, _) |
                // safe impl of dangling drop
                (false, false, _, true) => self.acc.push(TraitImplIncorrectSafety { impl_: ast_id_map.get(loc.id.value), file_id, should_be_safe: false }.into()),
                _ => (),
            };

        // Negative impls can't have items, don't emit missing items diagnostic for them
        if let (false, Some(trait_)) = (impl_is_negative, trait_) {
            let trait_items = &trait_.trait_items(self.db).items;
            let required_items = trait_items.iter().filter(|&(_, assoc)| match *assoc {
                AssocItemId::FunctionId(it) => !FunctionSignature::of(self.db, it).has_body(),
                AssocItemId::ConstId(id) => !ConstSignature::of(self.db, id).has_body(),
                AssocItemId::TypeAliasId(it) => TypeAliasSignature::of(self.db, it).ty.is_none(),
            });
            impl_assoc_items_scratch.extend(impl_items.0.items.iter().cloned());

            let redundant = impl_assoc_items_scratch
                .iter()
                .filter(|(name, id)| {
                    !trait_items.iter().any(|(impl_name, impl_item)| {
                        discriminant(impl_item) == discriminant(id) && impl_name == name
                    })
                })
                .map(|(name, item)| (*name, AssocItem::from(*item)));
            for (name, assoc_item) in redundant {
                self.acc.push(
                    TraitImplRedundantAssocItems {
                        trait_: trait_.into(),
                        file_id,
                        impl_: ast_id_map.get(loc.id.value),
                        assoc_item: (name, assoc_item),
                    }
                    .into(),
                )
            }

            let mut missing: Vec<_> = required_items
                .filter(|(name, id)| {
                    !impl_assoc_items_scratch.iter().any(|(impl_name, impl_item)| {
                        discriminant(impl_item) == discriminant(id) && impl_name == name
                    })
                })
                .map(|(name, item)| (*name, AssocItem::from(*item)))
                // alloc: empty, so no allocation, unless items are missing: then reported
                .collect();

            if !missing.is_empty() {
                let env = ParamEnvAndCrate {
                    param_env: self.db.trait_environment(def.into()),
                    krate: self.krate,
                };
                let self_ty = self.db.impl_self_ty(def).instantiate_identity().skip_norm_wip();
                let self_ty = structurally_normalize_ty(infcx, self_ty, env.param_env);
                let tail_ty = struct_tail_raw(self.db, infcx.interner, self_ty, |ty| {
                    structurally_normalize_ty(infcx, ty, env.param_env)
                });
                let self_ty_is_guaranteed_unsized =
                    matches!(tail_ty.kind(), TyKind::Dynamic(..) | TyKind::Slice(..) | TyKind::Str);
                if self_ty_is_guaranteed_unsized {
                    missing.retain(|(_, assoc_item)| {
                            let assoc_item = match *assoc_item {
                                AssocItem::Function(it) => match it.id {
                                    AnyFunctionId::FunctionId(id) => id.into(),
                                    AnyFunctionId::BuiltinDeriveImplMethod { .. } => {
                                        never!("should not have an `AnyFunctionId::BuiltinDeriveImplMethod` here");
                                        return false;
                                    },
                                },
                                AssocItem::Const(it) => it.id.into(),
                                AssocItem::TypeAlias(it) => it.id.into(),
                            };
                            !hir_ide::dyn_compatibility::generics_require_sized_self(self.db, assoc_item)
                        });
                }
            }

            // HACK: When specialization is enabled in the current crate, and there exists
            // *any* blanket impl that provides a default implementation for the missing item,
            // suppress the missing associated item diagnostic.
            // This can lead to false negatives when the impl in question does not actually
            // specialize that blanket impl, but determining the exact specialization
            // relationship here would be significantly more expensive.
            if !missing.is_empty() {
                let features = UnstableFeatures::query(self.db, self.krate);
                if features.specialization || features.min_specialization {
                    missing.retain(|(assoc_name, assoc_item)| {
                        let AssocItem::Function(_) = assoc_item else {
                            return true;
                        };

                        for &impl_ in
                            TraitImpls::for_crate(self.db, self.krate).blanket_impls(trait_)
                        {
                            if impl_ == def {
                                continue;
                            }

                            for (name, item) in &impl_.impl_items(self.db).items {
                                let AssocItemId::FunctionId(fn_) = item else {
                                    continue;
                                };
                                if name != assoc_name {
                                    continue;
                                }

                                if FunctionSignature::of(self.db, *fn_).is_default() {
                                    return false;
                                }
                            }
                        }

                        true
                    });
                }
            }

            if !missing.is_empty() {
                self.acc.push(
                    TraitImplMissingAssocItems {
                        impl_: ast_id_map.get(loc.id.value),
                        file_id,
                        missing,
                    }
                    .into(),
                )
            }
            impl_assoc_items_scratch.clear();
        }
    }

    fn collect_module(&mut self, def: ModuleId) {
        let _p = tracing::info_span!("diagnostics", name = ?def.name(self.db)).entered();

        let def_map = def.def_map(self.db);
        let scope = def_map.module(self.db, def).scope(self.db);

        for diag in def_map.diagnostics_of(self.db, def) {
            self.emit_def_diagnostic(&diag.kind);
        }

        if !def.is_block_module(self.db) {
            // These are reported by the body of block modules
            scope.all_macro_calls().for_each(|call| self.collect_macro_call(call));
        }

        scope
            .declarations()
            .chain(scope.unnamed_consts().map(ModuleDefId::ConstId))
            .for_each(|def| self.collect_module_def(def, def_map));

        scope.legacy_macros().flat_map(|(_, it)| it).for_each(|&def| {
            self.emit_case_diagnostics(def.into());
            self.collect_macro_def(def);
        });

        if scope.impls().len() == 0 {
            return;
        }
        let interner = DbInterner::new_with(self.db, self.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        // alloc: one scratch a module, reused by each of its impls
        let mut impl_assoc_items_scratch = Vec::new();
        scope.impls().for_each(|def| {
            impl_assoc_items_scratch.clear();
            self.collect_impl(def, &infcx, def_map, &mut impl_assoc_items_scratch)
        });
    }

    fn collect_macro_def(&mut self, def: MacroId) {
        let id = def.definition(self.db);
        if let hir_expand::MacroDefKind::Declarative(ast, _) = id.kind
            && let expander = ast.decl_macro_expander(self.db, id.krate)
            && let Some(e) = expander.mac.err()
        {
            self.emit_def_diagnostic(&DefDiagnosticKind::MacroDefError {
                ast,
                // alloc: error path, a reported diagnostic's message
                message: e.to_string(),
            });
        }
    }

    fn collect_anon_const(&mut self, source_map: &ExpressionStoreSourceMap, def: AnonConstId<'db>) {
        self.emit_inference_errors(
            def.into(),
            source_map,
            TypeOwnerId::from_anon_const(def, self.db),
        );
        self.collect_anon_const_borrowck(source_map, def);
    }

    fn collect_anon_const_borrowck(
        &mut self,
        source_map: &ExpressionStoreSourceMap,
        def: AnonConstId<'db>,
    ) {
        use hir_ide::mir::MirSpan;
        let result = hir_ty_borrowck::borrowck_anon_const(self.db, def);
        let node_of = |span: MirSpan| -> Option<InFile<SyntaxNodePtr>> {
            let source = match span {
                MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => source_map.expr_syntax(expr),
                MirSpan::PatId(pat) => source_map.pat_syntax(pat),
                MirSpan::BindingId(binding) => {
                    source_map.pat_syntax(*source_map.patterns_for_binding(binding).first()?)
                }
                _ => source_map.expr_syntax(def.loc(self.db).expr),
            };
            Some(source.ok()?.map(|it| it.syntax_node_ptr()))
        };
        for diagnostic in &result.diagnostics {
            let Some(node) = node_of(diagnostic.span) else { continue };
            let (labels, children, suggestions) = borrowck_parts(self.db, diagnostic, &node_of);
            self.acc.push(
                BorrowckError {
                    node,
                    at_end: matches!(diagnostic.span, MirSpan::ExprEnd(_)),
                    code: diagnostic.code,
                    // clone: error path, a reported borrowck error
                    message: diagnostic.message.clone(),
                    labels,
                    children,
                    suggestions,
                }
                .into(),
            );
        }
    }

    fn collect_enum(&mut self, def: EnumId) {
        self.collect_only_generic_def(def);
        self.collect_wfcheck(def.into());

        let variants = def.enum_variants_with_diagnostics(self.db);
        variants.0.variants.values().for_each(|&(def, _)| self.collect_enum_variant(def));

        let file = def.lookup(self.db).id.file_id;
        let ast_id_map = file.ast_id_map(self.db);
        for diag in &variants.1 {
            self.acc.push(
                InactiveCode {
                    node: InFile::new(file, ast_id_map.get(diag.ast_id).syntax_node_ptr()),
                    // clone: error path, a reported diagnostic
                    cfg: diag.cfg.clone(),
                    // clone: error path, a reported diagnostic
                    opts: diag.opts.clone(),
                }
                .into(),
            );
        }
    }

    fn collect_enum_variant(&mut self, def: EnumVariantId) {
        self.collect_def_with_body(
            def.into(),
            TypeOwnerId::GenericDefId(def.loc(self.db).parent.into()),
        );
        self.collect_variant(def.into());
    }

    fn collect_anon_consts_and_ty_diagnostics(
        &mut self,
        source_map: &ExpressionStoreSourceMap,
        anon_consts: &[AnonConstId<'db>],
        diagnostics: &[TyLoweringDiagnostic],
    ) {
        anon_consts.iter().for_each(|&anon_const| self.collect_anon_const(source_map, anon_const));

        diagnostics
            .iter()
            .filter_map(|diag| AnyDiagnostic::ty_diagnostic(diag, source_map, self.db))
            .for_each(|diag| self.acc.push(diag));
    }

    fn collect_generic_def(
        &mut self,
        store: &ExpressionStore,
        source_map: &ExpressionStoreSourceMap,
        def: GenericDefId,
    ) {
        self.collect_expr_store(store, source_map);
        for (anon_consts, diagnostics) in signature_anon_consts_and_diagnostics(self.db, def) {
            self.collect_anon_consts_and_ty_diagnostics(source_map, anon_consts, diagnostics);
        }
    }

    fn collect_def_with_body(&mut self, def: DefWithBodyId, type_owner: TypeOwnerId) {
        if !(self.bodies)(def.into()) {
            return;
        }
        let at = self.acc.len();
        if let Some(deferred) = &mut self.deferred
            && deferred.take(def, at)
        {
            return;
        }
        self.check_body(def, type_owner);
    }

    /// The checks of body `def`, which `bodies` asks for.
    fn check_body(&mut self, def: DefWithBodyId, type_owner: TypeOwnerId) {
        let (body, source_map) = Body::with_source_map(self.db, def);

        self.collect_expr_store(body, source_map);
        self.emit_inference_errors(def.into(), source_map, type_owner);

        // FIXME: Body validation should be defined for any `InferBodyId`.
        for op in hir_ty_unsafeck::unsafety_check(self.db, def).ops() {
            match source_map.expr_or_pat_syntax(op.node()) {
                Ok(node) => self.acc.push(
                    MissingUnsafe {
                        node,
                        lint: op.lint,
                        reason: op.reason,
                        first: op.first,
                        in_unsafe_fn: op.in_unsafe_fn,
                    }
                    .into(),
                ),
                Err(SyntheticSyntax) => {
                    // FIXME: Here and elsewhere in this file, the `expr` was
                    // desugared, report or assert that this doesn't happen.
                }
            }
        }

        for diagnostic in BodyValidationDiagnostic::collect(self.db, def, self.style_lints) {
            self.acc
                .extend(AnyDiagnostic::body_validation_diagnostic(self.db, diagnostic, source_map));
        }
        crate::match_lints::match_check_diagnostics(
            self.db, def, source_map, type_owner, self.acc,
        );
        self.collect_borrowck(def, body, source_map);
    }

    fn collect_borrowck(
        &mut self,
        def: DefWithBodyId,
        body: &Body,
        source_map: &hir_def::expr_store::BodySourceMap,
    ) {
        use hir_ide::mir::MirSpan;
        let result = hir_ty_borrowck::borrowck(self.db, def);
        // An `async fn`'s body is the future it makes, which rustc points at as its block.
        let block = || -> Option<InFile<SyntaxNodePtr>> {
            let DefWithBodyId::FunctionId(function) = def else { return None };
            let source = hir_def::src::HasSource::source(function.lookup(self.db), self.db);
            let block = source.value.body()?;
            Some(InFile::new(source.file_id, SyntaxNodePtr::new(syntax::AstNode::syntax(&block))))
        };
        let node_of = |span: MirSpan| -> Option<InFile<SyntaxNodePtr>> {
            let source = match span {
                MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => {
                    match source_map.expr_syntax(expr) {
                        Ok(it) => it,
                        // A borrow `format_args!` lowers to is at what it borrows, and a
                        // variable its template names at the template.
                        Err(_) => match body.expr(expr) {
                            hir_def::hir::Expr::Ref { expr: inner, .. } => {
                                match source_map.expr_syntax(inner) {
                                    Ok(it) => it,
                                    Err(_) => source_map
                                        .format_args_implicit_capture(inner)?
                                        .map(|(template, _)| template.wrap_left()),
                                }
                            }
                            _ if expr == body.root_expr() => return block(),
                            // What else a macro lowers to, as the call of `Arguments::new` of
                            // `format_args!`, is at the nearest expression the source has,
                            // where rustc points at the macro call.
                            _ => {
                                let mut child = expr;
                                loop {
                                    let (parent, _) = body.exprs().find(|&(it, _)| {
                                        let mut found = false;
                                        body.walk_child_exprs(it, |it| found |= it == child);
                                        found
                                    })?;
                                    if let Ok(it) = source_map.expr_syntax(parent) {
                                        break it;
                                    }
                                    child = parent;
                                }
                            }
                        },
                    }
                }
                MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok()?,
                // `self` is a binding no pattern binds.
                MirSpan::BindingId(binding)
                    if body
                        .self_param
                        .is_some_and(|it| it.formal == binding || it.user_written == binding) =>
                {
                    return source_map
                        .self_param_syntax()
                        .map(|it| it.map(|it| it.syntax_node_ptr()));
                }
                MirSpan::BindingId(binding) => {
                    let pat = *source_map.patterns_for_binding(binding).first()?;
                    source_map.pat_syntax(pat).ok()?
                }
                MirSpan::SelfParam => {
                    return source_map
                        .self_param_syntax()
                        .map(|it| it.map(|it| it.syntax_node_ptr()));
                }
                MirSpan::Unknown => match source_map.expr_syntax(body.root_expr()) {
                    Ok(it) => it,
                    Err(_) => return block(),
                },
                // Only an optimized body inlines; the borrow checker reads built ones.
                MirSpan::Inlined(_) => return None,
            };
            Some(source.map(|it| it.syntax_node_ptr()))
        };
        // rustc's span of a function's signature starts at `fn`, the line of its name.
        let signature = || -> Option<InFile<SyntaxNodePtr>> {
            let DefWithBodyId::FunctionId(function) = def else { return None };
            let source = hir_def::src::HasSource::source(function.lookup(self.db), self.db);
            let name = source.value.name()?;
            Some(InFile::new(source.file_id, SyntaxNodePtr::new(syntax::AstNode::syntax(&name))))
        };
        for diagnostic in &result.diagnostics {
            let node = match diagnostic.at_signature {
                true => signature().or_else(|| node_of(diagnostic.span)),
                false => node_of(diagnostic.span),
            };
            let Some(node) = node else { continue };
            // A variable lowering declares `mut` itself, as an `async fn`'s argument, points
            // at what the user wrote, which has no `mut` to remove.
            if diagnostic.code == "unused_mut" {
                let pat = node.value.to_node(&node.file_id.parse_or_expand(self.db));
                let written = match ast::SelfParam::can_cast(pat.kind()) {
                    true => ast::SelfParam::cast(pat).is_some_and(|it| it.mut_token().is_some()),
                    false => ast::IdentPat::cast(pat).is_some_and(|it| it.mut_token().is_some()),
                };
                if !written {
                    continue;
                }
            }
            let (labels, children, suggestions) = borrowck_parts(self.db, diagnostic, &node_of);
            self.acc.push(
                BorrowckError {
                    node,
                    at_end: matches!(diagnostic.span, MirSpan::ExprEnd(_)),
                    code: diagnostic.code,
                    // clone: error path, a reported borrowck error
                    message: diagnostic.message.clone(),
                    labels,
                    children,
                    suggestions,
                }
                .into(),
            );
        }
        let const_check = &hir_ide::mir::const_check::const_check(self.db, def).diagnostics;
        for diagnostic in const_check {
            let Some(node) = node_of(diagnostic.span) else { continue };
            self.acc.push(
                ConstCheckError {
                    node,
                    code: diagnostic.code,
                    // clone: error path, a reported const check error
                    message: diagnostic.message.clone(),
                }
                .into(),
            );
        }
        // `blaze::const_impure` is fn-cache's lint: no other crate's `const fn` is walked for it.
        if let DefWithBodyId::FunctionId(func) = def
            && hir_ide::fn_cache::uses_fn_cache(self.db, hir_def::HasModule::krate(&def, self.db))
            && const_check.is_empty()
            && hir_ide::mir::const_check::const_context(self.db, def)
                == Some(hir_ide::mir::const_check::ConstContext::ConstFn)
            && let Some(impure) = hir_ide::mir::purity::const_purity(self.db, func)
            && impure.told
            && let Some(node) = node_of(impure.span)
        {
            // alloc: error path, a reported lint's message
            let message = format!("const fn {}", impure.said());
            self.acc.push(ConstImpure { node, message }.into());
        }
        if let Some(reason) = &result.skipped {
            use hir_def::src::HasSource as _;
            let node = node_of(MirSpan::Unknown).unwrap_or_else(|| {
                let db = self.db;
                match def {
                    DefWithBodyId::FunctionId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    DefWithBodyId::StaticId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    DefWithBodyId::ConstId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                    DefWithBodyId::VariantId(it) => it.lookup(db).ast_ptr(db).map(Into::into),
                }
            });
            // clone: error path, a reported diagnostic
            self.acc.push(BorrowckSkipped { node, reason: reason.clone() }.into());
        }
    }

    fn emit_inference_errors(
        &mut self,
        def: InferBodyId<'db>,
        source_map: &ExpressionStoreSourceMap,
        type_owner: TypeOwnerId,
    ) {
        let infer = InferenceResult::of(self.db, def);
        // rustc's `check_coroutine_obligations`, after the body's own errors.
        let stalled = hir_ide::coroutine_obligations::coroutine_obligation_errors(self.db, def);

        self.acc.extend(infer.diagnostics().iter().chain(stalled).filter_map(|diag| {
            AnyDiagnostic::inference_diagnostic(
                self.db,
                self.krate,
                self.edition,
                diag,
                source_map,
                type_owner,
            )
        }));
    }

    fn collect_variant(&mut self, def: VariantId) {
        let (fields, source_map) = def.fields_with_source_map(self.db);
        self.collect_expr_store(&fields.store, source_map);

        let lowering = self.db.field_types_with_diagnostics(def);
        self.collect_anon_consts_and_ty_diagnostics(
            source_map,
            lowering.defined_anon_consts(),
            lowering.diagnostics(),
        );
    }

    fn collect_generic_def_with_body(
        &mut self,
        def: impl Into<GenericDefId> + Into<DefWithBodyId> + Copy,
    ) {
        let generic_def: GenericDefId = def.into();
        let (signature_store, signature_source_map) =
            ExpressionStore::with_source_map(self.db, generic_def.into());
        self.collect_generic_def(signature_store, signature_source_map, generic_def);

        let def_with_body: DefWithBodyId = def.into();
        self.collect_def_with_body(def_with_body, generic_def.into());
        if let GenericDefId::FunctionId(_) = generic_def {
            self.collect_wfcheck(generic_def);
        }
    }

    fn collect_generic_variant(&mut self, def: impl Into<GenericDefId> + Into<VariantId> + Copy) {
        let generic_def: GenericDefId = def.into();
        let (store, source_map) = ExpressionStore::with_source_map(self.db, generic_def.into());
        self.collect_generic_def(store, source_map, generic_def);
        self.collect_variant(def.into());
        self.collect_wfcheck(generic_def);
    }

    /// rustc's well-formedness errors of an ADT or an impl, reported as the borrow checker's.
    fn collect_wfcheck(&mut self, def: GenericDefId) {
        use hir_ty_wfcheck::Site;
        let db = self.db;
        for diagnostic in &hir_ty_wfcheck::wfcheck(db, def).diagnostics {
            let mut self_param = None;
            let source = match diagnostic.site {
                Site::Field(variant, field) => {
                    let (fields, source_map) = variant.fields_with_source_map(db);
                    source_map.type_syntax(fields.fields()[field].type_ref).ok()
                }
                Site::AssocType(alias) => {
                    let (signature, source_map) = TypeAliasSignature::with_source_map(db, alias);
                    signature.ty.and_then(|ty| source_map.type_syntax(ty).ok())
                }
                Site::ImplTrait(impl_) => {
                    let (signature, source_map) = ImplSignature::with_source_map(db, impl_);
                    signature
                        .target_trait
                        .as_ref()
                        .and_then(|it| source_map.type_syntax(it.path.type_ref()).ok())
                }
                Site::Bound(owner, index) => {
                    use hir_def::hir::{
                        generics::{GenericParams, WherePredicate},
                        type_ref::TypeBound,
                    };
                    let (_, source_map) = ExpressionStore::with_source_map(db, owner.into());
                    match GenericParams::of(db, owner).where_predicates().get(index as usize) {
                        Some(WherePredicate::TypeBound { target, bound, .. }) => match bound {
                            TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path) => {
                                source_map.type_syntax(path.type_ref()).ok()
                            }
                            _ => source_map.type_syntax(*target).ok(),
                        },
                        _ => None,
                    }
                }
                Site::Param(function, index) => {
                    let (signature, source_map) = FunctionSignature::with_source_map(db, function);
                    let typed = signature
                        .params
                        .get(index as usize)
                        .and_then(|&ty| source_map.type_syntax(ty).ok());
                    // `&'a mut self` has no type written: rustc points at the parameter.
                    if typed.is_none() && index == 0 && signature.has_self_param() {
                        let source = hir_def::src::HasSource::source(function.lookup(db), db);
                        if let Some(param) =
                            source.value.param_list().and_then(|it| it.self_param())
                        {
                            self_param =
                                Some(source.with_value(SyntaxNodePtr::new(param.syntax())));
                        }
                    }
                    typed
                }
                Site::Output(function) => {
                    let (signature, source_map) = FunctionSignature::with_source_map(db, function);
                    signature.ret_type.and_then(|ty| source_map.type_syntax(ty).ok())
                }
                Site::SelfTy(impl_) => {
                    let (signature, source_map) = ImplSignature::with_source_map(db, impl_);
                    source_map.type_syntax(signature.self_ty).ok()
                }
                Site::Opaque(function, index) => {
                    use hir_def::{
                        expr_store::StoreVisitor,
                        hir::type_ref::{TypeRef, TypeRefId},
                    };
                    struct Find<'a> {
                        store: &'a ExpressionStore,
                        found: Vec<TypeRefId>,
                    }
                    impl StoreVisitor for Find<'_> {
                        fn on_type(&mut self, ty: TypeRefId) {
                            if let TypeRef::ImplTrait(_) = self.store.type_ref(ty) {
                                self.found.push(ty);
                            }
                            let store = self.store;
                            store.visit_type_ref_children(ty, self);
                        }
                    }
                    let (signature, source_map) = FunctionSignature::with_source_map(db, function);
                    // alloc: error path, the site of a reported wfcheck error
                    let mut find = Find { store: &signature.store, found: Vec::new() };
                    if let Some(ret_type) = signature.ret_type {
                        find.on_type(ret_type);
                    }
                    let store = &signature.store;
                    find.found.get(index as usize).and_then(|&it| {
                        source_map.type_syntax(it).ok().or_else(|| {
                            // An `async fn`'s future has no syntax: its output, the type
                            // the function says it returns, has.
                            let TypeRef::ImplTrait(bounds) = store.type_ref(it) else { return None };
                            let [hir_def::hir::type_ref::TypeBound::Path(path, _)] = &bounds[..]
                            else {
                                return None;
                            };
                            let args = store.path(*path).generic_args()?.last()??;
                            let output = args.bindings.first()?.type_ref?;
                            source_map.type_syntax(output).ok()
                        })
                    })
                }
            };
            let Some(node) = source.map(|it| it.map(|it| it.syntax_node_ptr())).or(self_param)
            else {
                continue;
            };
            self.acc.push(
                BorrowckError {
                    node,
                    at_end: false,
                    code: diagnostic.code,
                    // clone: error path, a reported wfcheck error
                    message: diagnostic.message.clone(),
                    // alloc: empty
                    labels: Vec::new(),
                    // alloc: empty
                    children: Vec::new(),
                    // alloc: empty
                    suggestions: Vec::new(),
                }
                .into(),
            );
        }
    }

    fn collect_only_generic_def(&mut self, def: impl Into<GenericDefId>) {
        let generic_def: GenericDefId = def.into();
        let (store, source_map) = ExpressionStore::with_source_map(self.db, generic_def.into());
        self.collect_generic_def(store, source_map, generic_def);
    }

    fn collect_module_def(&mut self, def: ModuleDefId, def_map: &DefMap) {
        self.emit_case_diagnostics(def);

        match def {
            ModuleDefId::ModuleId(def) => {
                // Only add diagnostics from inline modules
                if def_map.module(self.db, def).origin.is_inline() {
                    self.collect_module(def);
                }
            }
            ModuleDefId::TraitId(def) => self.collect_trait(def, def_map),
            ModuleDefId::MacroId(def) => self.collect_macro_def(def),
            ModuleDefId::FunctionId(def) => self.collect_generic_def_with_body(def),
            ModuleDefId::ConstId(def) => self.collect_generic_def_with_body(def),
            ModuleDefId::StaticId(def) => self.collect_generic_def_with_body(def),
            ModuleDefId::EnumVariantId(def) => self.collect_enum_variant(def),
            ModuleDefId::AdtId(AdtId::StructId(def)) => self.collect_generic_variant(def),
            ModuleDefId::AdtId(AdtId::UnionId(def)) => self.collect_generic_variant(def),
            ModuleDefId::AdtId(AdtId::EnumId(def)) => self.collect_enum(def),
            ModuleDefId::TypeAliasId(def) => self.collect_only_generic_def(def),
            ModuleDefId::BuiltinType(_) => {}
        }
    }

    fn collect_expr_store(
        &mut self,
        store: &ExpressionStore,
        source_map: &ExpressionStoreSourceMap,
    ) {
        for (_, def_map) in store.blocks(self.db) {
            self.collect_module(def_map.root_module_id());
        }

        for diag in source_map.diagnostics() {
            self.acc.push(match diag {
                ExpressionStoreDiagnostics::InactiveCode { node, cfg } => InactiveCode {
                    node: *node,
                    // clone: error path, a reported diagnostic
                    cfg: cfg.clone(),
                    // clone: error path, a reported diagnostic
                    opts: self.krate.cfg_options(self.db).clone(),
                }
                .into(),
                ExpressionStoreDiagnostics::UnresolvedMacroCall { node, path } => {
                    UnresolvedMacroCall {
                        range: node.map(|ptr| ptr.text_range()),
                        // clone: error path, a reported diagnostic
                        path: path.clone(),
                        is_bang: true,
                    }
                    .into()
                }
                ExpressionStoreDiagnostics::AwaitOutsideOfAsync { node, location } => {
                    // clone: error path, a reported diagnostic
                    AwaitOutsideOfAsync { node: *node, location: location.clone() }.into()
                }
                ExpressionStoreDiagnostics::UnreachableLabel { node, name } => {
                    UnreachableLabel { node: *node, name: *name }.into()
                }
                ExpressionStoreDiagnostics::UndeclaredLabel { node, name } => {
                    UndeclaredLabel { node: *node, name: *name }.into()
                }
                ExpressionStoreDiagnostics::PatternArgInExternFn { node } => {
                    PatternArgInExternFn { node: *node }.into()
                }
                ExpressionStoreDiagnostics::FruInDestructuringAssignment { node } => {
                    FruInDestructuringAssignment { node: *node }.into()
                }
                ExpressionStoreDiagnostics::MissingBody { node, kind } => {
                    MissingBody { node: *node, kind: *kind }.into()
                }
            });
        }

        source_map.macro_calls().for_each(|(_ast_id, call_id)| self.collect_macro_call(call_id));
    }
}

impl<'db> AnyDiagnostic<'db> {
    fn body_validation_diagnostic(
        db: &'db Db,
        diagnostic: BodyValidationDiagnostic<'db>,
        source_map: &hir_def::expr_store::BodySourceMap,
    ) -> Option<AnyDiagnostic<'db>> {
        match diagnostic {
            BodyValidationDiagnostic::RecordMissingFields { record, variant, missed_fields } => {
                let variant_data = variant.fields(db);
                let missed_fields = missed_fields
                    .into_iter()
                    .map(|idx| {
                        (variant_data.fields()[idx].name, Field { parent: variant.into(), id: idx })
                    })
                    // alloc: error path, a reported diagnostic
                    .collect();

                let record = match record {
                    Either::Left(record_expr) => source_map.expr_syntax(record_expr).ok()?,
                    Either::Right(record_pat) => source_map.pat_syntax(record_pat).ok()?,
                };
                let file = record.file_id;
                let root = record.file_syntax(db);
                match record.value.to_node(&root) {
                    Either::Left(ast::Expr::RecordExpr(record_expr))
                        if record_expr.record_expr_field_list().is_some() =>
                    {
                        let field_list_parent_path =
                            record_expr.path().map(|path| AstPtr::new(&path));
                        return Some(
                            MissingFields {
                                file,
                                field_list_parent: AstPtr::new(&Either::Left(record_expr)),
                                field_list_parent_path,
                                variant: variant.into(),
                                missed_fields,
                            }
                            .into(),
                        );
                    }
                    Either::Right(ast::Pat::RecordPat(record_pat))
                        if record_pat.record_pat_field_list().is_some() =>
                    {
                        let field_list_parent_path =
                            record_pat.path().map(|path| AstPtr::new(&path));
                        return Some(
                            MissingFields {
                                file,
                                field_list_parent: AstPtr::new(&Either::Right(record_pat)),
                                field_list_parent_path,
                                variant: variant.into(),
                                missed_fields,
                            }
                            .into(),
                        );
                    }
                    _ => {}
                }
            }
            BodyValidationDiagnostic::ReplaceFilterMapNextWithFindMap { method_call_expr } => {
                if let Ok(next_source_ptr) = source_map.expr_syntax(method_call_expr) {
                    return Some(
                        ReplaceFilterMapNextWithFindMap {
                            file: next_source_ptr.file_id,
                            next_expr: next_source_ptr.value.cast()?,
                        }
                        .into(),
                    );
                }
            }
            BodyValidationDiagnostic::RemoveTrailingReturn { return_expr } => {
                if let Ok(source_ptr) = source_map.expr_syntax(return_expr)
                    // Filters out desugared return expressions (e.g. desugared try operators).
                    && let Some(ptr) = source_ptr.value.cast::<ast::ReturnExpr>()
                {
                    return Some(
                        RemoveTrailingReturn { return_expr: InFile::new(source_ptr.file_id, ptr) }
                            .into(),
                    );
                }
            }
            BodyValidationDiagnostic::RemoveUnnecessaryElse { if_expr } => {
                if let Ok(source_ptr) = source_map.expr_syntax(if_expr)
                    && let Some(ptr) = source_ptr.value.cast::<ast::IfExpr>()
                {
                    return Some(
                        RemoveUnnecessaryElse { if_expr: InFile::new(source_ptr.file_id, ptr) }
                            .into(),
                    );
                }
            }
            BodyValidationDiagnostic::UnusedMustUse { expr, message } => {
                if let Ok(source_ptr) = source_map.expr_syntax(expr) {
                    return Some(UnusedMustUse { expr: source_ptr, message }.into());
                }
            }
        }
        None
    }

    fn inference_diagnostic(
        db: &'db Db,
        krate: base_db::Crate,
        edition: Edition,
        d: &'db InferenceDiagnostic,
        source_map: &ExpressionStoreSourceMap,
        type_owner: TypeOwnerId,
    ) -> Option<AnyDiagnostic<'db>> {
        let expr_syntax = |expr| Self::expr_syntax(expr, source_map);
        let pat_syntax = |pat| Self::pat_syntax(pat, source_map);
        let expr_or_pat_syntax = |id| match id {
            ExprOrPatId::ExprId(expr) => expr_syntax(expr),
            ExprOrPatId::PatId(pat) => pat_syntax(pat),
        };
        let interner = DbInterner::new_no_crate(db);
        let new_ty = |ty| Type { owner: type_owner, ty: EarlyBinder::bind(interner, ty) };
        let span_syntax = |span| Self::span_syntax(span, source_map);
        Some(match d {
            &InferenceDiagnostic::NoSuchField { field: expr, private, variant } => {
                let expr_or_pat = match expr.unpack() {
                    ExprOrPatId::ExprId(expr) => {
                        source_map.field_syntax(expr).map(AstPtr::wrap_left)
                    }
                    ExprOrPatId::PatId(pat) => source_map.pat_field_syntax(pat),
                };
                let private = private.map(|id| Field { id, parent: variant.into() });
                NoSuchField { field: expr_or_pat, private, variant }.into()
            }
            &InferenceDiagnostic::MismatchedArrayPatLen { pat, expected, found, has_rest } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                MismatchedArrayPatLen { pat, expected, found, has_rest }.into()
            }
            &InferenceDiagnostic::ArrayPatternWithoutFixedLength { pat } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                ArrayPatternWithoutFixedLength { pat }.into()
            }
            InferenceDiagnostic::ExpectedArrayOrSlicePat { pat, found } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                ExpectedArrayOrSlicePat {
                    pat,
                    found: Type {
                        owner: type_owner,
                        ty: EarlyBinder::bind(interner, found.as_ref()),
                    },
                }
                .into()
            }
            InferenceDiagnostic::InvalidRangePatType { pat, lhs, rhs } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                let end = |side: &Option<(StoredTy, bool)>| {
                    side.as_ref().map(|(ty, fail)| (new_ty(ty.as_ref()), *fail))
                };
                InvalidRangePatType { pat, lhs: end(lhs), rhs: end(rhs) }.into()
            }
            &InferenceDiagnostic::DuplicateField { field: expr, variant } => {
                let expr_or_pat = match expr.unpack() {
                    ExprOrPatId::ExprId(expr) => {
                        source_map.field_syntax(expr).map(AstPtr::wrap_left)
                    }
                    ExprOrPatId::PatId(pat) => source_map.pat_field_syntax(pat),
                };
                DuplicateField { field: expr_or_pat, variant: variant.into() }.into()
            }
            InferenceDiagnostic::MismatchedArgCount {
                call_expr,
                expected,
                found,
                is_fn_trait_call,
                args,
            } => {
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                MismatchedArgCount {
                    call_expr: expr_syntax(*call_expr)?,
                    expected: *expected,
                    found: *found,
                    is_fn_trait_call: *is_fn_trait_call,
                    args: CallArgs::new(db, target, args),
                }
                .into()
            }
            InferenceDiagnostic::IncorrectArguments { call_expr, args } => {
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let args = CallArgs::new(db, target, args);
                IncorrectArguments { call_expr: expr_syntax(*call_expr)?, args }.into()
            }
            &InferenceDiagnostic::PrivateField { expr, field } => {
                let expr = expr_syntax(expr)?;
                let field = field.into();
                PrivateField { expr, field }.into()
            }
            &InferenceDiagnostic::PrivateAssocItem { id, item } => {
                let expr_or_pat = expr_or_pat_syntax(id.unpack())?;
                let item = item.into();
                PrivateAssocItem { expr_or_pat, item }.into()
            }
            InferenceDiagnostic::ExpectedFunction { call_expr, found } => {
                let call_expr = expr_syntax(*call_expr)?;
                ExpectedFunction { call: call_expr, found: new_ty(found.as_ref()) }.into()
            }
            InferenceDiagnostic::UnresolvedField {
                expr,
                receiver,
                name,
                method_with_same_name_exists,
            } => {
                let expr = expr_syntax(*expr)?;
                UnresolvedField {
                    expr,
                    name: *name,
                    receiver: new_ty(receiver.as_ref()),
                    method_with_same_name_exists: *method_with_same_name_exists,
                }
                .into()
            }
            InferenceDiagnostic::UnresolvedMethodCall {
                expr,
                receiver,
                name,
                field_with_same_name,
                assoc_func_with_same_name,
            } => {
                let expr = expr_syntax(*expr)?;
                UnresolvedMethodCall {
                    expr,
                    name: *name,
                    receiver: new_ty(receiver.as_ref()),
                    field_with_same_name: field_with_same_name
                        .as_ref()
                        .map(|ty| new_ty(ty.as_ref())),
                    assoc_func_with_same_name: assoc_func_with_same_name.map(Into::into),
                }
                .into()
            }
            InferenceDiagnostic::UnresolvedAssocItem { id, ty, name } => {
                let expr_or_pat = expr_or_pat_syntax(id.unpack())?;
                UnresolvedAssocItem { expr_or_pat, ty: new_ty(ty.as_ref()), name: *name }.into()
            }
            &InferenceDiagnostic::UnresolvedIdent { id } => {
                let node = match id.unpack() {
                    ExprOrPatId::ExprId(id) => match source_map.expr_syntax(id) {
                        Ok(syntax) => syntax.map(|it| (it, None)),
                        Err(SyntheticSyntax) => source_map
                            .format_args_implicit_capture(id)?
                            .map(|(node, range)| (node.wrap_left(), Some(range))),
                    },
                    ExprOrPatId::PatId(id) => pat_syntax(id)?.map(|it| (it, None)),
                };
                UnresolvedIdent { node }.into()
            }
            &InferenceDiagnostic::UnresolvedPath { id, segment } => {
                UnresolvedPath { node: expr_or_pat_syntax(id.unpack())?, segment }.into()
            }
            &InferenceDiagnostic::BreakOutsideOfLoop { expr, is_break, bad_value_break } => {
                let expr = expr_syntax(expr)?;
                BreakOutsideOfLoop { expr, is_break, bad_value_break }.into()
            }
            &InferenceDiagnostic::UnlabeledJumpInLabeledBlock { expr, is_break } => {
                UnlabeledJumpInLabeledBlock { expr: expr_syntax(expr)?, is_break }.into()
            }
            &InferenceDiagnostic::AsyncCoroutinesNotSupported { expr } => {
                AsyncCoroutinesNotSupported { expr: expr_syntax(expr)? }.into()
            }
            &InferenceDiagnostic::NonExhaustiveRecordExpr { expr } => {
                NonExhaustiveRecordExpr { expr: expr_syntax(expr)? }.into()
            }
            &InferenceDiagnostic::NonExhaustiveRecordPat { pat, variant } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                NonExhaustiveRecordPat { pat, variant: variant.into() }.into()
            }
            &InferenceDiagnostic::UnionPatMustHaveExactlyOneField { pat } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                UnionPatMustHaveExactlyOneField { pat }.into()
            }
            &InferenceDiagnostic::UnionPatHasRest { pat } => {
                let pat = pat_syntax(pat)?.map(Into::into);
                UnionPatHasRest { pat }.into()
            }
            &InferenceDiagnostic::FunctionalRecordUpdateOnNonStruct { base_expr } => {
                FunctionalRecordUpdateOnNonStruct { base_expr: expr_syntax(base_expr)? }.into()
            }
            InferenceDiagnostic::TypedHole { expr, expected } => {
                let expr = expr_syntax(*expr)?;
                TypedHole { expr, expected: new_ty(expected.as_ref()) }.into()
            }
            &InferenceDiagnostic::MismatchedTupleStructPatArgCount {
                pat,
                expected,
                found,
                variant,
                had_err,
            } => {
                let InFile { file_id, value } = pat_syntax(pat)?;
                // cast from Either<Pat, SelfParam> -> Either<_, Pat>
                let ptr = AstPtr::try_from_raw(value.syntax_node_ptr())?;
                let expr_or_pat = InFile { file_id, value: ptr };
                MismatchedTupleStructPatArgCount {
                    expr_or_pat,
                    expected,
                    found,
                    variant: variant.into(),
                    had_err,
                }
                .into()
            }
            InferenceDiagnostic::CastToUnsized { expr, cast_ty } => {
                let expr = expr_syntax(*expr)?;
                CastToUnsized { expr, cast_ty: new_ty(cast_ty.as_ref()) }.into()
            }
            InferenceDiagnostic::InvalidCast { expr, error, expr_ty, cast_ty, pointers } => {
                let expr = expr_syntax(*expr)?;
                let expr_ty = new_ty(expr_ty.as_ref());
                let cast_ty = new_ty(cast_ty.as_ref());
                let kind = |kind: Option<hir_ide::CastPointerKind>| {
                    kind.map(|kind| match kind {
                        hir_ide::CastPointerKind::Thin => CastPointerKind::Thin,
                        hir_ide::CastPointerKind::VTable(principal) => {
                            CastPointerKind::VTable(principal.map(|id| crate::Trait { id }))
                        }
                        hir_ide::CastPointerKind::Length => CastPointerKind::Length,
                        hir_ide::CastPointerKind::OfAlias => CastPointerKind::OfAlias,
                        hir_ide::CastPointerKind::OfParam(id) => {
                            CastPointerKind::OfParam(crate::TypeParam { id })
                        }
                    })
                };
                let (expr_pointer, cast_pointer, added_auto_traits) = match pointers {
                    Some(it) => (
                        kind(it.expr),
                        kind(it.cast),
                        // alloc: once per refused pointer cast
                        it.added_auto_traits.iter().map(|&id| crate::Trait { id }).collect(),
                    ),
                    // alloc: empty
                    None => (None, None, Vec::new()),
                };
                InvalidCast {
                    expr,
                    error: *error,
                    expr_ty,
                    cast_ty,
                    expr_pointer,
                    cast_pointer,
                    added_auto_traits,
                }
                .into()
            }
            InferenceDiagnostic::CannotBeDereferenced { expr, found, at_error } => {
                let expr = expr_syntax(*expr)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                // alloc: error path, a reported diagnostic's type as rustc prints it
                let at_error = at_error.as_ref().map(|it| it.as_ref().display(db, target).to_string());
                CannotBeDereferenced { expr, found: new_ty(found.as_ref()), at_error }.into()
            }
            InferenceDiagnostic::UnaryOperatorCannotBeApplied { expr, op, found, at_error, unmet } => {
                let expr = expr_syntax(*expr)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                // alloc: error path, a reported diagnostic's type as rustc prints it
                let at_error = at_error.as_ref().map(|it| it.as_ref().display(db, target).to_string());
                let unmet =
                    // alloc: error path, a reported diagnostic's unmet goals
                    unmet.iter().map(|it| Self::solver_goal(db, it, type_owner, target)).collect();
                let found = new_ty(found.as_ref());
                UnaryOperatorCannotBeApplied { expr, op: *op, found, at_error, unmet }.into()
            }
            InferenceDiagnostic::BinaryOperatorCannotBeApplied {
                expr,
                op,
                lhs,
                rhs,
                at_error,
                unmet,
                output,
            } => {
                let expr = expr_syntax(*expr)?;
                let (lhs, rhs) = (new_ty(lhs.as_ref()), new_ty(rhs.as_ref()));
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let at_error = at_error.as_deref().map(|(lhs, rhs)| {
                    (
                        // alloc: error path, a reported diagnostic's types as rustc prints them
                        lhs.as_ref().display(db, target).to_string(),
                        // alloc: error path, as above
                        rhs.as_ref().display(db, target).to_string(),
                    )
                });
                let unmet =
                    // alloc: error path, a reported diagnostic's unmet goals
                    unmet.iter().map(|it| Self::solver_goal(db, it, type_owner, target)).collect();
                let output = output.as_ref().map(|it| new_ty(it.as_ref()));
                BinaryOperatorCannotBeApplied { expr, op: *op, lhs, rhs, at_error, unmet, output }
                    .into()
            }
            InferenceDiagnostic::MutRefInImmRefPat { pat } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                MutRefInImmRefPat { pat }.into()
            }
            InferenceDiagnostic::CannotImplicitlyDerefTraitObject { pat, found } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                CannotImplicitlyDerefTraitObject { pat, found: new_ty(found.as_ref()) }.into()
            }
            InferenceDiagnostic::CannotIndexInto { expr, found, at_error } => {
                let expr = expr_syntax(*expr)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                // alloc: error path, a reported diagnostic's type as rustc prints it
                let at_error = at_error.as_ref().map(|it| it.as_ref().display(db, target).to_string());
                CannotIndexInto { expr, found: new_ty(found.as_ref()), at_error }.into()
            }
            InferenceDiagnostic::TyDiagnostic { diag } => {
                Self::ty_diagnostic(diag, source_map, db)?
            }
            InferenceDiagnostic::PathDiagnostic { node, diag } => {
                let source = expr_or_pat_syntax(node.unpack())?;
                let syntax = source.value.to_node(&source.file_id.parse_or_expand(db));
                let path = match_ast! {
                    match (syntax.syntax()) {
                        ast::RecordExpr(it) => it.path()?,
                        ast::RecordPat(it) => it.path()?,
                        ast::TupleStructPat(it) => it.path()?,
                        ast::PathExpr(it) => it.path()?,
                        ast::PathPat(it) => it.path()?,
                        _ => return None,
                    }
                };
                Self::path_diagnostic(diag, source.with_value(path), db)?
            }
            &InferenceDiagnostic::MethodCallIncorrectGenericsLen {
                expr,
                provided_count,
                expected_count,
                kind,
                def,
            } => {
                let syntax = expr_syntax(expr)?;
                let file_id = syntax.file_id;
                let syntax =
                    syntax.with_value(syntax.value.cast::<ast::MethodCallExpr>()?).to_node(db);
                let generics_or_name = syntax
                    .generic_arg_list()
                    .map(Either::Left)
                    .or_else(|| syntax.name_ref().map(Either::Right))?;
                let generics_or_name = InFile::new(file_id, AstPtr::new(&generics_or_name));
                IncorrectGenericsLen {
                    generics_or_segment: generics_or_name,
                    kind,
                    provided: provided_count,
                    expected: expected_count,
                    def: def.into(),
                }
                .into()
            }
            &InferenceDiagnostic::MethodCallIncorrectGenericsOrder {
                expr,
                param_id,
                arg_idx,
                has_self_arg,
            } => {
                let syntax = expr_syntax(expr)?;
                let file_id = syntax.file_id;
                let syntax =
                    syntax.with_value(syntax.value.cast::<ast::MethodCallExpr>()?).to_node(db);
                let generic_args = syntax.generic_arg_list()?;
                let provided_arg = hir_generic_arg_to_ast(&generic_args, arg_idx, has_self_arg)?;
                let provided_arg = InFile::new(file_id, AstPtr::new(&provided_arg));
                let expected_kind = GenericArgKind::from_id(param_id);
                IncorrectGenericsOrder { provided_arg, expected_kind, param: param_id.into() }
                    .into()
            }
            &InferenceDiagnostic::MethodCallLateBoundLifetimeArgs { expr, def } => {
                let syntax = expr_syntax(expr)?;
                let file_id = syntax.file_id;
                let call = syntax.value.cast::<ast::MethodCallExpr>()?;
                let call = call.try_to_view(file_id.root(db))?;
                let generics_or_segment = match view::HasGenericArgs::generic_arg_list(&call) {
                    Some(list) => AstPtr::<ast::GenericArgList>::of(list).wrap_left(),
                    None => AstPtr::<ast::NameRef>::of(call.name_ref()?).wrap_right(),
                };
                let generics_or_segment = InFile::new(file_id, generics_or_segment);
                LateBoundLifetimeArgs { generics_or_segment, def: def.into(), hard_error: false }
                    .into()
            }
            &InferenceDiagnostic::InvalidLhsOfAssignment { lhs, assign, op } => {
                let lhs = expr_syntax(lhs)?;
                let assign = expr_syntax(assign)?;
                InvalidLhsOfAssignment { lhs, assign, op }.into()
            }
            &InferenceDiagnostic::MethodCallIllegalSizedBound {
                call_expr,
                method,
                bound,
                needs_mut,
                ref candidates,
            } => MethodCallIllegalSizedBound {
                call_expr: expr_syntax(call_expr)?,
                method: method.into(),
                bound: bound.map(Into::into),
                needs_mut,
                // alloc: the error path's candidate traits.
                candidates: candidates.iter().map(|&id| crate::Trait { id }).collect(),
            }
            .into(),
            &InferenceDiagnostic::EditionDependentMethodCall {
                call_expr,
                receiver,
                lint,
                method,
                inherent,
                autoderefs,
                autoref,
            } => {
                let call_expr = expr_syntax(call_expr)?;
                let call_expr =
                    call_expr.with_value(call_expr.value.cast::<ast::MethodCallExpr>()?);
                EditionDependentMethodCall {
                    call_expr,
                    receiver: expr_syntax(receiver)?,
                    lint,
                    method: method.into(),
                    inherent,
                    autoderefs,
                    autoref,
                }
                .into()
            }
            InferenceDiagnostic::FullyQualifiedCallFrom2018 { node, item, self_ty } => {
                FullyQualifiedCallFrom2018 {
                    expr_or_pat: expr_or_pat_syntax(node.unpack())?,
                    item: (*item).into(),
                    self_ty: new_ty(self_ty.as_ref()),
                }
                .into()
            }
            &InferenceDiagnostic::TypeMustBeKnown {
                at_point,
                ref top_term,
                kind,
                ref ambiguity,
                origin,
                ref named,
                cause,
            } => {
                let at_point = span_syntax(at_point)?;
                let origin = origin.and_then(|(span, param)| {
                    let span = span_syntax(span)?;
                    let in_expansion = span.file_id.is_macro();
                    match param {
                        hir_def::GenericParamId::TypeParamId(_) if in_expansion => None,
                        _ => Some((span, GenericParam::from(param))),
                    }
                });
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let ambiguity = ambiguity.as_deref().map(|ambiguity| {
                    let note = |note: &'db solver_errors::AmbiguityNote| {
                        Self::ambiguous_goal(db, note, type_owner, target)
                    };
                    TypeAmbiguity {
                        primary: note(&ambiguity.primary),
                        // alloc: error path, a reported diagnostic's notes
                        related: ambiguity.related.iter().map(note).collect(),
                    }
                });
                let term = |term: hir_ide::next_solver::GenericArg<'db>| match term.kind() {
                    rustc_type_ir::GenericArgKind::Type(ty) => Either::Left(new_ty(ty)),
                    // FIXME: Printing the const to string is definitely not the correct thing to do here.
                    rustc_type_ir::GenericArgKind::Const(konst) => Either::Right(
                        konst
                            .display(db, target)
                            // alloc: error path, a reported diagnostic's const as rustc prints it
                            .to_string(),
                    ),
                    rustc_type_ir::GenericArgKind::Lifetime(_) => {
                        unreachable!("we currently don't emit TypeMustBeKnown for lifetimes")
                    }
                };
                let top_term = top_term.as_ref().map(|top_term| term(top_term.as_ref()));
                // rustc's `ty_infer_name_resolver` names a const parameter wherever its variable
                // was made, a type parameter only outside a macro expansion.
                let named_term = named.as_deref().map(|named| {
                    term(named.named(interner, |span, param| match param {
                        hir_def::GenericParamId::TypeParamId(_) => {
                            span_syntax(span).is_none_or(|span| !span.file_id.is_macro())
                        }
                        _ => true,
                    }))
                });
                TypeMustBeKnown {
                    at_point,
                    top_term,
                    kind,
                    ambiguity,
                    origin,
                    named_term,
                    cause: cause.into(),
                }
                .into()
            }
            &InferenceDiagnostic::UnionExprMustHaveExactlyOneField { expr } => {
                let expr = expr_syntax(expr)?;
                UnionExprMustHaveExactlyOneField { expr }.into()
            }
            InferenceDiagnostic::TypeMismatch { node, expected, found, at_error, cause } => {
                let expr_or_pat = expr_or_pat_syntax(node.unpack())?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                let at_error = at_error.as_deref().map(|(expected, found)| {
                    (
                        // alloc: error path, a reported diagnostic's types as rustc prints them
                        expected.as_ref().display(db, target).to_string(),
                        // alloc: error path, as above
                        found.as_ref().display(db, target).to_string(),
                    )
                });
                TypeMismatch {
                    at_error,
                    cause: (*cause).into(),
                    expr_or_pat,
                    expected: Type {
                        owner: type_owner,
                        ty: EarlyBinder::bind(interner, expected.as_ref()),
                    },
                    actual: Type {
                        owner: type_owner,
                        ty: EarlyBinder::bind(interner, found.as_ref()),
                    },
                }
                .into()
            }
            InferenceDiagnostic::SolverDiagnostic(d) => {
                let span = span_syntax(d.span)?;
                let target = DisplayTarget::from_crate_and_edition(db, krate, edition);
                Self::solver_diagnostic(db, &d.kind, span, d.cause.into(), type_owner, target)?
            }
            InferenceDiagnostic::ExplicitDropMethodUse { kind } => {
                let expr_or_path = match kind {
                    ExplicitDropMethodUseKind::MethodCall(expr) => {
                        let expr = expr_syntax(*expr)?;
                        let expr = expr.with_value(expr.value.cast::<ast::MethodCallExpr>()?);
                        Either::Left(expr)
                    }
                    ExplicitDropMethodUseKind::Path(path_expr_id) => {
                        let syntax = expr_or_pat_syntax(path_expr_id.unpack())?;
                        let file_id = syntax.file_id;
                        let syntax =
                            syntax.with_value(syntax.value.cast::<ast::PathExpr>()?).to_node(db);
                        let path = syntax.path()?;
                        let path = InFile::new(file_id, AstPtr::new(&path));
                        Either::Right(path)
                    }
                };
                ExplicitDropMethodUse { expr_or_path }.into()
            }
            InferenceDiagnostic::MutableRefBinding { pat } => {
                let pat = pat_syntax(*pat)?.map(Into::into);
                MutableRefBinding { pat }.into()
            }
            &InferenceDiagnostic::YieldOutsideCoroutine { expr } => {
                YieldOutsideCoroutine { expr: expr_syntax(expr)? }.into()
            }
            &InferenceDiagnostic::ReturnOutsideFunction { expr, kind } => {
                ReturnOutsideFunction { expr: expr_syntax(expr)?, kind }.into()
            }
            InferenceDiagnostic::UnsafeBinderCastOfNonBinder { expr, kind, found } => {
                UnsafeBinderCastOfNonBinder {
                    expr: expr_syntax(*expr)?,
                    kind: *kind,
                    found: new_ty(found.as_ref()),
                }
                .into()
            }
            &InferenceDiagnostic::RecordMissingFields { record, variant, ref missed_fields } => {
                let record = expr_or_pat_syntax(record)?;
                let file = record.file_id;
                let root = record.file_syntax(db);
                let variant_data = variant.fields(db);
                let missed_fields = missed_fields
                    .iter()
                    .map(|&idx| {
                        (variant_data.fields()[idx].name, Field { parent: variant.into(), id: idx })
                    })
                    // alloc: error path, a reported diagnostic
                    .collect();
                match record.value.to_node(&root) {
                    Either::Left(ast::Expr::RecordExpr(record_expr))
                        if record_expr.record_expr_field_list().is_some() =>
                    {
                        let field_list_parent_path =
                            record_expr.path().map(|path| AstPtr::new(&path));
                        return Some(
                            MissingFields {
                                file,
                                field_list_parent: AstPtr::new(&Either::Left(record_expr)),
                                field_list_parent_path,
                                variant: variant.into(),
                                missed_fields,
                            }
                            .into(),
                        );
                    }
                    Either::Right(ast::Pat::RecordPat(record_pat))
                        if record_pat.record_pat_field_list().is_some() =>
                    {
                        let field_list_parent_path =
                            record_pat.path().map(|path| AstPtr::new(&path));
                        MissingFields {
                            file,
                            field_list_parent: AstPtr::new(&Either::Right(record_pat)),
                            field_list_parent_path,
                            variant: variant.into(),
                            missed_fields,
                        }
                        .into()
                    }
                    _ => return None,
                }
            }
        })
    }

    fn solver_diagnostic(
        db: &'db Db,
        d: &'db SolverDiagnosticKind,
        span: SpanSyntax,
        cause: ObligationCauseCode,
        type_owner: TypeOwnerId,
        target: DisplayTarget,
    ) -> Option<AnyDiagnostic<'db>> {
        let interner = DbInterner::new_no_crate(db);
        let ty = |ty: &'db hir_ide::next_solver::StoredTy| Type {
            owner: type_owner,
            ty: EarlyBinder::bind(interner, ty.as_ref()),
        };
        let goal = |goal| Self::solver_goal(db, goal, type_owner, target);
        let error = |kind| -> AnyDiagnostic<'db> { TraitSolverError { span, cause, kind }.into() };
        Some(match d {
            &SolverDiagnosticKind::GoalUnmet { goal: ref unmet, non_const_impl } => {
                error(TraitSolverErrorKind::GoalUnmet {
                    goal: goal(unmet),
                    non_const_impl: non_const_impl.map(|id| crate::Impl { id }),
                })
            }
            SolverDiagnosticKind::Overflow { goal: overflowed, suggest_increasing_limit } => {
                error(TraitSolverErrorKind::Overflow {
                    goal: goal(overflowed),
                    suggest_increasing_limit: *suggest_increasing_limit,
                })
            }
            SolverDiagnosticKind::ProjectionMismatch { goal: projection, normalized } => {
                let normalized = normalized.as_ref().map(|arg| match arg.as_ref().kind() {
                    rustc_type_ir::GenericArgKind::Type(it) => {
                        Either::Left(Type { owner: type_owner, ty: EarlyBinder::bind(interner, it) })
                    }
                    // alloc: error path, a reported diagnostic's const as rustc prints it
                    _ => Either::Right(arg.as_ref().display(db, target).to_string()),
                });
                error(TraitSolverErrorKind::ProjectionMismatch { goal: goal(projection), normalized })
            }
            SolverDiagnosticKind::TypesDiffer { expected, found } => {
                error(TraitSolverErrorKind::TypesDiffer { expected: ty(expected), found: ty(found) })
            }
            SolverDiagnosticKind::ConstArgHasWrongType { ct, ct_ty, expected_ty } => {
                error(TraitSolverErrorKind::ConstArgHasWrongType {
                    // alloc: error path, a reported diagnostic's const as rustc prints it
                    ct: ct.as_ref().display(db, target).to_string(),
                    ct_ty: ty(ct_ty),
                    expected_ty: ty(expected_ty),
                })
            }
            SolverDiagnosticKind::HigherRankedOutlives => {
                error(TraitSolverErrorKind::HigherRankedOutlives)
            }
            &SolverDiagnosticKind::Unreported { code } => {
                error(TraitSolverErrorKind::Unreported(code))
            }
            SolverDiagnosticKind::TraitUnimplemented {
                trait_predicate,
                parent_trait_predicates,
                parent_impls,
            } => {
                let trait_predicate =
                    crate::TraitClause::new(interner, type_owner, trait_predicate.get(interner));
                let parent_trait_predicates = parent_trait_predicates
                    .iter()
                    .map(|trait_predicate| {
                        crate::TraitClause::new(interner, type_owner, trait_predicate.get(interner))
                    })
                    // alloc: error path, the diagnostic's own list
                    .collect();
                let parent_impls_of = parent_impls;
                let parent_impls = parent_impls_of
                    .iter()
                    .map(|&it| it.map(|(impl_, _)| crate::Impl::from(impl_)))
                    // alloc: error path, the diagnostic's own list
                    .collect();
                let parent_impl_bounds = parent_impls_of
                    .iter()
                    .map(|&it| match it? {
                        (AnyImplId::ImplId(impl_), bound_index) => {
                            bound_source(db, impl_.into(), bound_index)
                        }
                        (AnyImplId::BuiltinDeriveImplId(_), _) => None,
                    })
                    // alloc: error path, the diagnostic's own list
                    .collect();
                UnimplementedTrait {
                    span,
                    cause,
                    trait_predicate,
                    parent_trait_predicates,
                    parent_impls,
                    parent_impl_bounds,
                }
                .into()
            }
            &SolverDiagnosticKind::ClosureKindMismatch { expected, found, is_async, ref closure } => {
                let fn_trait = |kind| match (kind, is_async) {
                    (rustc_type_ir::ClosureKind::Fn, false) => crate::FnTrait::Fn,
                    (rustc_type_ir::ClosureKind::FnMut, false) => crate::FnTrait::FnMut,
                    (rustc_type_ir::ClosureKind::FnOnce, false) => crate::FnTrait::FnOnce,
                    (rustc_type_ir::ClosureKind::Fn, true) => crate::FnTrait::AsyncFn,
                    (rustc_type_ir::ClosureKind::FnMut, true) => crate::FnTrait::AsyncFnMut,
                    (rustc_type_ir::ClosureKind::FnOnce, true) => crate::FnTrait::AsyncFnOnce,
                };
                ClosureKindMismatch {
                    span,
                    cause,
                    expected: fn_trait(expected),
                    found: fn_trait(found),
                    closure: Self::closure_syntax(db, closure.as_ref()),
                }
                .into()
            }
        })
    }

    /// The syntax of the closure `ty` is the type of, in whichever body it is written.
    fn closure_syntax(
        db: &'db Db,
        ty: hir_ide::next_solver::Ty<'db>,
    ) -> Option<InFile<AstPtr<ast::ClosureExpr>>> {
        let hir_ide::db::InternedClosure { owner, expr, .. } = match ty.kind() {
            TyKind::Closure(id, _) => id.0.loc(db),
            TyKind::CoroutineClosure(id, _) => id.0.loc(db),
            _ => return None,
        };
        let (_, source_map) = ExpressionStore::with_source_map(db, owner.expression_store_owner(db));
        let ptr = source_map.expr_syntax(expr).ok()?;
        Some(ptr.with_value(ptr.value.cast::<ast::ClosureExpr>()?))
    }

    fn ambiguous_goal(
        db: &'db Db,
        note: &'db solver_errors::AmbiguityNote,
        type_owner: TypeOwnerId,
        target: DisplayTarget,
    ) -> AmbiguousGoal<'db> {
        let interner = DbInterner::new_no_crate(db);
        let candidates = note
            .candidates
            .iter()
            .map(|candidate| match candidate {
                &solver_errors::AmbiguityCandidate::Impl(id) => {
                    AmbiguityCandidate::Impl(crate::Impl { id })
                }
                solver_errors::AmbiguityCandidate::ParamEnv { trait_, args } => {
                    let trait_ref = hir_ide::next_solver::TraitRef::new_from_args(
                        interner,
                        (*trait_).into(),
                        args.as_ref(),
                    );
                    AmbiguityCandidate::WhereClause(crate::TraitRef::new(
                        interner, type_owner, trait_ref,
                    ))
                }
            })
            // alloc: error path, a reported ambiguity's candidates
            .collect();
        AmbiguousGoal { goal: Self::solver_goal(db, &note.goal, type_owner, target), candidates }
    }

    fn solver_goal(
        db: &'db Db,
        goal: &'db StoredGoal,
        type_owner: TypeOwnerId,
        target: DisplayTarget,
    ) -> SolverGoal<'db> {
        let interner = DbInterner::new_no_crate(db);
        let ty = |ty: &'db hir_ide::next_solver::StoredTy| Type {
            owner: type_owner,
            ty: EarlyBinder::bind(interner, ty.as_ref()),
        };
        let text = |arg: &'db hir_ide::next_solver::StoredGenericArg| {
            // alloc: error path, a reported goal's argument as rustc prints it
            arg.as_ref().display(db, target).to_string()
        };
        let term = |arg: &'db hir_ide::next_solver::StoredGenericArg| match arg.as_ref().kind() {
            rustc_type_ir::GenericArgKind::Type(it) => Either::Left(Type {
                owner: type_owner,
                ty: EarlyBinder::bind(interner, it),
            }),
            _ => Either::Right(text(arg)),
        };
        let trait_ref = |trait_: hir_def::TraitId, args: &'db hir_ide::next_solver::StoredGenericArgs| {
            hir_ide::next_solver::TraitRef::new_from_args(interner, trait_.into(), args.as_ref())
        };
        match goal {
            &StoredGoal::Trait { trait_, ref args, polarity } => {
                let clause =
                    hir_ide::next_solver::TraitClause { trait_ref: trait_ref(trait_, args), polarity };
                SolverGoal::Trait(crate::TraitClause::new(interner, type_owner, clause))
            }
            &StoredGoal::HostEffect { trait_, ref args, maybe_const } => SolverGoal::HostEffect {
                trait_ref: crate::TraitRef::new(interner, type_owner, trait_ref(trait_, args)),
                maybe_const,
            },
            StoredGoal::Projection { alias, term: projected } => {
                SolverGoal::Projection { alias: term(alias), term: term(projected) }
            }
            StoredGoal::NormalizesTo { alias, term: _ } => {
                SolverGoal::NormalizesTo { alias: term(alias) }
            }
            StoredGoal::WellFormed { term: wf } => SolverGoal::WellFormed(term(wf)),
            StoredGoal::Subtype { a, b } => SolverGoal::Subtype { a: ty(a), b: ty(b) },
            StoredGoal::Coerce { a, b } => SolverGoal::Coerce { a: ty(a), b: ty(b) },
            StoredGoal::ConstEvaluatable { ct } => SolverGoal::ConstEvaluatable(text(ct)),
            StoredGoal::ConstArgHasType { ct, ty: of } => {
                SolverGoal::ConstArgHasType { ct: text(ct), ty: ty(of) }
            }
            StoredGoal::ConstEquate { a, b } => SolverGoal::ConstEquate { a: text(a), b: text(b) },
            &StoredGoal::DynCompatible { trait_ } => SolverGoal::DynCompatible(Trait { id: trait_ }),
            StoredGoal::UnstableFeature { feature } => SolverGoal::UnstableFeature(*feature),
            StoredGoal::Outlives { a, b } => SolverGoal::Outlives { a: term(a), b: text(b) },
            StoredGoal::Ambiguous => SolverGoal::Ambiguous,
        }
    }

    fn path_diagnostic(
        diag: &PathLoweringDiagnostic,
        path: InFile<ast::Path>,
        db: &'db Db,
    ) -> Option<AnyDiagnostic<'db>> {
        Some(match *diag {
            PathLoweringDiagnostic::GenericArgsProhibited { ref segments, reason } => {
                let mut rtn = None;
                // alloc: an error's sites.
                let mut all_args = Vec::with_capacity(segments.len());
                for &segment in segments.iter() {
                    let segment = hir_segment_to_ast_segment(&path.value, segment)?;
                    if let Some(it) = segment.return_type_syntax() {
                        // RTN errors are emitted as `GenericArgsProhibited` or `ParenthesizedGenericArgsWithoutFnTrait`.
                        rtn.get_or_insert(it);
                        continue;
                    }
                    let args = if let Some(generics) = segment.generic_arg_list() {
                        AstPtr::new(&generics).wrap_left()
                    } else {
                        AstPtr::new(&segment.parenthesized_arg_list()?).wrap_right()
                    };
                    all_args.push(path.with_value(args));
                }
                let Some(&args) = all_args.first() else {
                    return Some(BadRtn { rtn: path.with_value(AstPtr::new(&rtn?)) }.into());
                };
                GenericArgsProhibited { args, all_args: all_args.into_boxed_slice(), reason }.into()
            }
            PathLoweringDiagnostic::ParenthesizedGenericArgsWithoutFnTrait { segment } => {
                let segment = hir_segment_to_ast_segment(&path.value, segment)?;

                if let Some(rtn) = segment.return_type_syntax() {
                    // RTN errors are emitted as `GenericArgsProhibited` or `ParenthesizedGenericArgsWithoutFnTrait`.
                    return Some(BadRtn { rtn: path.with_value(AstPtr::new(&rtn)) }.into());
                }

                let args = AstPtr::new(&segment.parenthesized_arg_list()?);
                let args = path.with_value(args);
                ParenthesizedGenericArgsWithoutFnTrait { args }.into()
            }
            PathLoweringDiagnostic::IncorrectGenericsLen {
                generics_source,
                provided_count,
                expected_count,
                kind,
                def,
            } => {
                let generics_or_segment =
                    path_generics_source_to_ast(&path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::new(&generics_or_segment));
                IncorrectGenericsLen {
                    generics_or_segment,
                    kind,
                    provided: provided_count,
                    expected: expected_count,
                    def: def.into(),
                }
                .into()
            }
            PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source,
                param_id,
                arg_idx,
                has_self_arg,
            } => {
                let generic_args =
                    path_generics_source_to_ast(&path.value, generics_source)?.left()?;
                let provided_arg = hir_generic_arg_to_ast(&generic_args, arg_idx, has_self_arg)?;
                let provided_arg = path.with_value(AstPtr::new(&provided_arg));
                let expected_kind = GenericArgKind::from_id(param_id);
                IncorrectGenericsOrder { provided_arg, expected_kind, param: param_id.into() }
                    .into()
            }
            PathLoweringDiagnostic::MissingLifetime { generics_source, expected_count, def }
            | PathLoweringDiagnostic::ElisionFailure { generics_source, expected_count, def } => {
                let generics_or_segment =
                    path_generics_source_to_ast(&path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::new(&generics_or_segment));
                let kind = match diag {
                    PathLoweringDiagnostic::ElisionFailure { .. } => {
                        MissingLifetimeKind::ElisionFailure
                    }
                    _ => MissingLifetimeKind::Missing,
                };
                MissingLifetime {
                    generics_or_segment,
                    expected: expected_count,
                    def: def.into(),
                    kind,
                }
                .into()
            }
            PathLoweringDiagnostic::LateBoundLifetimeArgs { generics_source, def, hard_error } => {
                let generics_or_segment =
                    path_generics_source_to_ast(&path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::new(&generics_or_segment));
                LateBoundLifetimeArgs { generics_or_segment, def: def.into(), hard_error }.into()
            }
            PathLoweringDiagnostic::AmbiguousAssociatedItem { segment, variant, item, impl_ } => {
                let ItemContainerId::TraitId(bound) = item.lookup(db).container else {
                    never!("an associated type of a trait has a trait");
                    return None;
                };
                AmbiguousAssociatedItem {
                    path: path.map(|it| AstPtr::new(&it)),
                    variant: VariantId::from(variant).into(),
                    item: item.into(),
                    self_ty: crate::Impl::from(AnyImplId::ImplId(impl_)).self_ty(db),
                    bound: bound.into(),
                }
                .into()
            }
            PathLoweringDiagnostic::ElidedLifetimesInPath {
                generics_source,
                expected_count,
                def,
                hard_error,
            } => {
                let generics_or_segment =
                    path_generics_source_to_ast(&path.value, generics_source)?;
                let generics_or_segment = path.with_value(AstPtr::new(&generics_or_segment));
                ElidedLifetimesInPath {
                    generics_or_segment,
                    expected: expected_count,
                    def: def.into(),
                    hard_error,
                }
                .into()
            }
            PathLoweringDiagnostic::GenericDefaultRefersToSelf { segment } => {
                let segment = hir_segment_to_ast_segment(&path.value, segment)?;
                let segment = path.with_value(AstPtr::new(&segment));
                GenericDefaultRefersToSelf { segment }.into()
            }
        })
    }

    fn expr_syntax(
        expr: ExprId,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<ExprOrPatPtr>> {
        source_map
            .expr_syntax(expr)
            .inspect_err(|_| stdx::never!("inference diagnostic in desugared expr"))
            .ok()
    }

    fn pat_syntax(
        pat: PatId,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<ExprOrPatPtr>> {
        source_map
            .pat_syntax(pat)
            .inspect_err(|_| stdx::never!("inference diagnostic in desugared pattern"))
            .ok()
    }

    fn type_syntax(
        type_ref: TypeRefId,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<AstPtr<ast::Type>>> {
        source_map
            .type_syntax(type_ref)
            .inspect_err(|_| stdx::never!("inference diagnostic in desugared type"))
            .ok()
    }

    fn span_syntax(
        span: hir_ide::Span,
        source_map: &ExpressionStoreSourceMap,
    ) -> Option<InFile<AstPtr<SpanAst>>> {
        Some(match span {
            hir_ide::Span::ExprId(idx) => Self::expr_syntax(idx, source_map)?.map(|it| it.upcast()),
            hir_ide::Span::PatId(idx) => Self::pat_syntax(idx, source_map)?.map(|it| it.upcast()),
            hir_ide::Span::TypeRefId(idx) => {
                Self::type_syntax(idx, source_map)?.map(|it| it.upcast())
            }
            hir_ide::Span::BindingId(idx) => {
                let &pat = source_map.patterns_for_binding(idx).first()?;
                Self::pat_syntax(pat, source_map)?.map(|it| it.upcast())
            }
            hir_ide::Span::Dummy => {
                never!("should never create a diagnostic for dummy spans");
                return None;
            }
        })
    }

    fn ty_diagnostic(
        diag: &TyLoweringDiagnostic,
        source_map: &ExpressionStoreSourceMap,
        db: &'db Db,
    ) -> Option<AnyDiagnostic<'db>> {
        Some(match diag {
            TyLoweringDiagnostic::PathDiagnostic { source, diag } => {
                let source = Self::type_syntax(*source, source_map)?;
                let syntax = source.value.to_node(&source.file_id.parse_or_expand(db));
                let ast::Type::PathType(syntax) = syntax else { return None };
                Self::path_diagnostic(diag, source.with_value(syntax.path()?), db)?
            }
            TyLoweringDiagnostic::InferVarsNotAllowed { sources } => {
                let mut nodes = sources
                    .iter()
                    .map(|&source| Some(Self::span_syntax(source, source_map)?.map(Into::into)))
                    .collect::<Option<Box<[InFile<SyntaxNodePtr>]>>>()?;
                nodes.sort_by_key(|it| it.value.text_range().start());
                InferVarsNotAllowed { node: *nodes.first()?, nodes }.into()
            }
            TyLoweringDiagnostic::MissingLifetimes { sites, kind } => {
                let sites = sites
                    .iter()
                    .map(|&site| Self::missing_lifetime_site(site, source_map, db))
                    .collect::<Option<Box<[_]>>>()?;
                MissingLifetimes { sites, kind: *kind }.into()
            }
        })
    }

    fn missing_lifetime_site(
        site: ElidedLifetimeSite,
        source_map: &ExpressionStoreSourceMap,
        db: &'db Db,
    ) -> Option<MissingLifetimeSite> {
        Some(match site {
            ElidedLifetimeSite::Reference(type_ref) => {
                let source = Self::type_syntax(type_ref, source_map)?;
                let source = source.map(|it| it.cast::<ast::RefType>()).transpose()?;
                MissingLifetimeSite::Reference(source)
            }
            ElidedLifetimeSite::Placeholder(lifetime) => {
                let source = source_map
                    .lifetime_syntax(lifetime)
                    .inspect_err(|_| stdx::never!("lowering diagnostic in a desugared lifetime"))
                    .ok()?;
                MissingLifetimeSite::Placeholder(source)
            }
            ElidedLifetimeSite::Path { source, generics_source, def, expected_count } => {
                let source = Self::type_syntax(source, source_map)?;
                let syntax = source.value.to_node(&source.file_id.parse_or_expand(db));
                let ast::Type::PathType(syntax) = syntax else { return None };
                let generics_or_segment =
                    path_generics_source_to_ast(&syntax.path()?, generics_source)?;
                MissingLifetimeSite::Path {
                    generics_or_segment: source.with_value(AstPtr::new(&generics_or_segment)),
                    expected: expected_count,
                    def: def.into(),
                }
            }
        })
    }
}

fn path_generics_source_to_ast(
    path: &ast::Path,
    generics_source: PathGenericsSource,
) -> Option<Either<ast::GenericArgList, ast::NameRef>> {
    Some(match generics_source {
        PathGenericsSource::Segment(segment) => {
            let segment = hir_segment_to_ast_segment(path, segment)?;
            segment
                .generic_arg_list()
                .map(Either::Left)
                .or_else(|| segment.name_ref().map(Either::Right))?
        }
        PathGenericsSource::AssocType { segment, assoc_type } => {
            let segment = hir_segment_to_ast_segment(path, segment)?;
            let segment_args = segment.generic_arg_list()?;
            let assoc = hir_assoc_type_binding_to_ast(&segment_args, assoc_type)?;
            assoc
                .generic_arg_list()
                .map(Either::Left)
                .or_else(|| assoc.name_ref().map(Either::Right))?
        }
    })
}
