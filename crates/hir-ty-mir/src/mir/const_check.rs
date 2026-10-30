//! rustc's `check_consts` over the MIR lowering builds: what the body of a const context (a
//! `const fn`, a `const` or `static` item's value, an enum discriminant) may not do, with
//! rustc's error codes and messages (`compiler/rustc_const_eval/src/check_consts` at the pinned
//! nightly, `16d1ffac1`).
//!
//! It reports only what rustc reports, never more: a program rustc accepts in const is
//! accepted here. As rustc, it looks at the blocks control reaches when nothing unwinds (rustc
//! checks after `SimplifyCfg::Initial` and skips cleanup blocks), and of the calls at those the
//! source writes or an operator, a deref or a desugaring makes ([`calls`]). A body with a call
//! typeck refuses (a `[const]` bound of the callee that does not hold) is not checked further:
//! rustc's typeck error leaves it no MIR to check.
//!
//! Each finding is a [`ConstCheckKind`], what rustc's error is with what it names; `code` and
//! `message` are rustc's.

mod calls;
mod qualifs;
mod stability;

use std::fmt;

use base_db::{Crate, Db};
use blaze_mem::{Frame, Tag, frame};
use hir_def::{
    CallableDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup,
    ModuleIdLt, TraitId,
    expr_store::Body,
    hir::generics::GenericParams,
    lang_item::LangItems,
    signatures::{FunctionSignature, ImplSignature, TraitSignature},
    unstable_features::UnstableFeatures,
};
use intern::Symbol;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};
use salsa::SalsaValue;

use hir_ty::{
    InferBodyId, InferenceResult,
    db::HirDatabase,
    display::{DisplayTarget, HirDisplay},
    next_solver::{GenericArgs, StoredGenericArgs, StoredTy, Ty, TyKind},
};

use super::{
    BasicBlockId, BinOp, CastKind, MirBody, MirSpan, Operand, OperandKind, Rvalue, StatementKind,
    TerminatorKind,
    framed::{Bits, bits_in},
};

#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct ConstCheckDiagnostic {
    pub kind: ConstCheckKind,
    /// The const context of the body the error is in, which rustc's messages name.
    pub context: ConstContext,
    /// rustc's error code, as `E0015`, or `""` for the errors rustc gives none.
    pub code: &'static str,
    pub span: MirSpan,
    pub message: String,
}

/// What rustc's const checker refuses, by its `NonConstOp` (`check_consts/ops.rs`) and, for a
/// call, by what `build_error_for_const_call` says of it. `conditionally` is rustc's
/// `ConditionallyConstCall`: the callee is const when its `[const]` bounds hold, which they do,
/// and calling it needs `#![feature(const_trait_impl)]` (`E0658`); otherwise the callee is not
/// const (`FnCallNonConst`, `E0015`).
#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub enum ConstCheckKind {
    /// `RawPtrToIntCast`.
    PointerToInt,
    /// `RawPtrComparison`.
    PointerComparison,
    /// `InlineAsm`.
    InlineAsm,
    /// `FnCallIndirect`: a call through a function pointer.
    FnPointerCall,
    /// `IntrinsicNonConst`.
    NonConstIntrinsic { name: String },
    /// A call of a function rustc names: `NonConstFnCall`. `descr` and `path` are as the
    /// message has them (`args` are the callee's, for a printer that writes them as rustc's
    /// `def_path_str_with_args`); `non_const_trait` is the trait of a trait's method when that
    /// trait is not `const`, which rustc's note names.
    NonConstFn {
        callee: FunctionId,
        args: StoredGenericArgs,
        descr: &'static str,
        path: String,
        conditionally: bool,
        non_const_trait: Option<TraitId>,
    },
    /// A call through `Fn`, `FnMut` or `FnOnce`: `NonConstClosure`.
    NonConstClosure { callee: FunctionId, note: ClosureNote, conditionally: bool },
    /// An overloaded operator: `NonConstOperator`. `derefs` is rustc's `ConsiderDereferencing`:
    /// the `*`s that make an `==` of references to a primitive compare the primitives.
    NonConstOperator { callee: FunctionId, trait_: TraitId, derefs: Option<u32>, conditionally: bool },
    /// A deref the source does not write: `NonConstDerefCoercion` of `ty` into `target`.
    NonConstDerefCoercion { callee: FunctionId, ty: StoredTy, target: StoredTy, conditionally: bool },
    /// A desugaring's call on `ty`, the callee's `Self`.
    NonConstDesugaring {
        callee: FunctionId,
        desugaring: Desugaring,
        ty: StoredTy,
        conditionally: bool,
    },
    /// `format_args!`'s `Arguments::new`: `NonConstFmtMacroCall`.
    NonConstFmtMacroCall { callee: FunctionId, conditionally: bool },
    /// `FnCallCVariadic`, refused without `#![feature(const_c_variadic)]`.
    CVariadicCall,
    /// `PanicNonStr`: `begin_panic` of other than `&str`, `panic_display` of other than `&&str`.
    PanicNonStr,
    /// `CallUnstable` of a function: `UnstableConstFn`.
    UnstableConstFn { callee: FunctionId, path: String, feature: Symbol },
    /// `CallUnstable` of a trait's method's trait: `UnstableConstTrait`.
    UnstableConstTrait { trait_: TraitId, path: String, feature: Symbol },
    /// `IntrinsicUnstable`.
    UnstableIntrinsic { name: String, feature: Symbol },
    /// A `const fn` rustc holds to recursive const stability using what `#[feature(gate)]`
    /// allows: `UnstableInStableExposed`. `already_unstable` is whether it is written
    /// `#[rustc_const_unstable]`, when rustc doesn't suggest it.
    UnstableInStableExposed { gate: Symbol, is_function_call: bool, already_unstable: bool },
    /// `UnmarkedConstItemExposed`.
    UnmarkedConstItemExposed { path: String },
    /// `UnmarkedIntrinsicExposed`.
    UnmarkedIntrinsicExposed { path: String },
    /// `LiveDrop`: a value of `ty` dropped at `dropped_at`. Without `needs_non_const_drop` its
    /// destructor is `[const]`, and dropping it needs `#![feature(const_destruct)]`. This and
    /// the next three are reported by [`qualifs`], from rustc's qualifs (`NeedsNonConstDrop`,
    /// `HasMutInterior`, the borrows of the final value). `ThreadLocalAccess` is a read of a
    /// `#[thread_local]` static (`OperandKind::Static`), blaze's `ThreadLocalRef`.
    LiveDrop { ty: StoredTy, dropped_at: MirSpan, needs_non_const_drop: bool },
    /// `EscapingMutBorrow`.
    EscapingMutBorrow,
    /// `EscapingCellBorrow`.
    EscapingCellBorrow,
    /// `ThreadLocalAccess`.
    ThreadLocalAccess,
}

/// rustc's `NonConstClosureNote`: what the closure called is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SalsaValue)]
pub enum ClosureNote {
    FnDef(CallableDefId),
    FnPtr,
    Closure,
    None,
}

/// rustc's `CallDesugaringKind`, of the desugarings a const context can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SalsaValue)]
pub enum Desugaring {
    ForLoopIntoIter,
    ForLoopNext,
    QuestionBranch,
    QuestionFromResidual,
    TryBlockFromOutput,
}

impl ConstCheckKind {
    /// rustc's error code: `E0658` for a conditionally-const call, which rustc sets over the
    /// call's own.
    pub fn code(&self) -> &'static str {
        use ConstCheckKind::*;
        match self {
            NonConstFn { conditionally, .. }
            | NonConstClosure { conditionally, .. }
            | NonConstOperator { conditionally, .. }
            | NonConstDerefCoercion { conditionally, .. }
            | NonConstDesugaring { conditionally, .. }
            | NonConstFmtMacroCall { conditionally, .. } => match conditionally {
                true => "E0658",
                false => "E0015",
            },
            InlineAsm | CVariadicCall => "E0015",
            LiveDrop { .. } => "E0493",
            EscapingMutBorrow => "E0764",
            EscapingCellBorrow => "E0492",
            ThreadLocalAccess => "E0625",
            PointerToInt
            | PointerComparison
            | FnPointerCall
            | NonConstIntrinsic { .. }
            | PanicNonStr
            | UnstableConstFn { .. }
            | UnstableConstTrait { .. }
            | UnstableIntrinsic { .. }
            | UnstableInStableExposed { .. }
            | UnmarkedConstItemExposed { .. }
            | UnmarkedIntrinsicExposed { .. } => "",
        }
    }

    /// rustc's message, in `context`.
    pub fn write_message(
        &self,
        db: &Db,
        krate: Crate,
        context: ConstContext,
        out: &mut impl fmt::Write,
    ) -> fmt::Result {
        use ConstCheckKind::*;
        let kind = context.plural();
        let non = |conditionally: &bool| match conditionally {
            true => "conditionally",
            false => "non",
        };
        // rustc's diagnostics name a type by its trimmed path (`Option<()>`), as hir-ty shows it.
        let target = DisplayTarget::from_crate(db, krate);
        let ty = |ty: &StoredTy| ty.as_ref().display(db, target).to_string();
        match self {
            PointerToInt => out.write_str("pointers cannot be cast to integers during const eval"),
            PointerComparison => {
                out.write_str("pointers cannot be reliably compared during const eval")
            }
            InlineAsm => write!(out, "inline assembly is not allowed in {kind}"),
            FnPointerCall => write!(out, "function pointer calls are not allowed in {kind}"),
            NonConstIntrinsic { name } => {
                write!(out, "cannot call non-const intrinsic `{name}` in {kind}")
            }
            NonConstFn { descr, path, conditionally, .. } => {
                write!(out, "cannot call {}-const {descr} `{path}` in {kind}", non(conditionally))
            }
            NonConstClosure { conditionally, .. } => {
                write!(out, "cannot call {}-const closure in {kind}", non(conditionally))
            }
            NonConstOperator { conditionally, .. } => {
                write!(out, "cannot call {}-const operator in {kind}", non(conditionally))
            }
            NonConstDerefCoercion { ty: self_ty, conditionally, .. } => write!(
                out,
                "cannot perform {}-const deref coercion on `{}` in {kind}",
                non(conditionally),
                ty(self_ty)
            ),
            NonConstDesugaring { desugaring, ty: self_ty, .. } => match desugaring {
                Desugaring::ForLoopIntoIter | Desugaring::ForLoopNext => {
                    write!(out, "cannot use `for` loop on `{}` in {kind}", ty(self_ty))
                }
                Desugaring::QuestionBranch | Desugaring::QuestionFromResidual => {
                    write!(out, "`?` is not allowed on `{}` in {kind}", ty(self_ty))
                }
                Desugaring::TryBlockFromOutput => write!(
                    out,
                    "`try` block cannot convert `{}` to the result in {kind}",
                    ty(self_ty)
                ),
            },
            NonConstFmtMacroCall { conditionally, .. } => {
                write!(out, "cannot call {}-const formatting macro in {kind}", non(conditionally))
            }
            CVariadicCall => write!(out, "calling const c-variadic functions is unstable in {kind}"),
            PanicNonStr => {
                out.write_str("argument to `panic!()` in a const context must have type `&str`")
            }
            UnstableConstFn { path, .. } => write!(out, "`{path}` is not yet stable as a const fn"),
            UnstableConstTrait { path, .. } => {
                write!(out, "`{path}` is not yet stable as a const trait")
            }
            UnstableIntrinsic { name, .. } => {
                write!(out, "`{name}` is not yet stable as a const intrinsic")
            }
            UnstableInStableExposed { gate, .. } => write!(
                out,
                "const function that might be (indirectly) exposed to stable cannot use \
                 `#[feature({gate})]`"
            ),
            UnmarkedConstItemExposed { path } => {
                write!(out, "`{path}` cannot be (indirectly) exposed to stable")
            }
            UnmarkedIntrinsicExposed { path } => {
                write!(out, "intrinsic `{path}` cannot be (indirectly) exposed to stable")
            }
            LiveDrop { ty: dropped, .. } => write!(
                out,
                "destructor of `{}` cannot be evaluated at compile-time",
                ty(dropped)
            ),
            EscapingMutBorrow => out.write_str(
                "mutable borrows of temporaries that have their lifetime extended until the end \
                 of the program are not allowed",
            ),
            EscapingCellBorrow => out.write_str(
                "interior mutable shared borrows of temporaries that have their lifetime \
                 extended until the end of the program are not allowed",
            ),
            ThreadLocalAccess => {
                out.write_str("thread-local statics cannot be accessed at compile-time")
            }
        }
    }
}

/// A type as `type_name` spells it, `_` where it can't.
struct TypeName(Option<String>);

impl fmt::Display for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_deref().unwrap_or("_"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, SalsaValue)]
pub struct ConstCheck {
    pub diagnostics: Vec<ConstCheckDiagnostic>,
}

/// rustc's `ConstContext`: what kind of const context a body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstContext {
    ConstFn,
    Const,
    Static,
}

impl ConstContext {
    /// As rustc's messages name it, in the plural: "in constant functions".
    pub fn plural(self) -> &'static str {
        match self {
            ConstContext::ConstFn => "constant functions",
            ConstContext::Const => "constants",
            ConstContext::Static => "statics",
        }
    }
}

/// The const context `def`'s body is, if it is one (rustc's `body_const_context`): a `const
/// fn`, a method of a `const trait` (its default body) or of a `const impl`, a `const`, a
/// `static` or an enum discriminant.
pub fn const_context(db: &Db, def: DefWithBodyId) -> Option<ConstContext> {
    match def {
        DefWithBodyId::FunctionId(func) => {
            hir_ty::effects::fn_is_const(db, func).then_some(ConstContext::ConstFn)
        }
        DefWithBodyId::ConstId(_) | DefWithBodyId::VariantId(_) => Some(ConstContext::Const),
        DefWithBodyId::StaticId(_) => Some(ConstContext::Static),
    }
}

/// Whether `func` is a trait's method or one of an `impl` of a trait.
pub(crate) fn of_a_trait(db: &Db, func: FunctionId) -> bool {
    match func.lookup(db).container {
        ItemContainerId::TraitId(_) => true,
        ItemContainerId::ImplId(it) => ImplSignature::of(db, it).target_trait.is_some(),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => false,
    }
}

/// What rustc's const checker reports in `def`'s body, when it is a const context.
#[salsa::tracked(returns(ref))]
pub fn const_check<'db>(db: &'db Db, def: DefWithBodyId) -> ConstCheck {
    let Some(context) = const_context(db, def) else { return ConstCheck::default() };
    if let DefWithBodyId::FunctionId(func) = def
        && not_const_checked(db, func)
    {
        return ConstCheck::default();
    }
    let infer = InferenceResult::of(db, def);
    if infer.has_type_mismatches() || infer.is_erroneous() || !infer.diagnostics().is_empty() {
        return ConstCheck::default();
    }
    let Ok(body) = db.mir_body(InferBodyId::DefWithBodyId(def)) else {
        return ConstCheck::default();
    };
    let mut checker = Checker::new(db, def, body, infer, context);
    frame::task(Tag::Mir, |frame| {
        let order = reachable(body, frame);
        for &block in order.iter() {
            let block = &body.basic_blocks[block];
            if block.is_cleanup {
                continue;
            }
            for statement in &block.statements {
                checker.statement(&statement.kind, statement.span);
            }
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::Call { func, args, destination, intrinsic, .. } => checker
                    .check_call(func, args, Some(destination), intrinsic.as_ref(), terminator.span),
                TerminatorKind::TailCall { func, args, .. } => {
                    checker.check_call(func, args, None, None, terminator.span)
                }
                _ => {}
            }
        }
        qualifs::check_qualifs(db, def, body, context, &order, frame, &mut |kind, span| {
            let status = Status::of_qualif(&kind);
            checker.check_op(kind, status, span)
        });
    });
    checker.finish()
}

/// rustc's `Status` of a `NonConstOp`: refused, or allowed by a feature gate.
#[derive(Clone)]
enum Status {
    Forbidden,
    Unstable {
        gate: Symbol,
        /// The caller checked the gate already: refused only as recursive const stability
        /// refuses it.
        already_checked: bool,
        safe_to_expose: bool,
        is_function_call: bool,
    },
}

impl Status {
    /// rustc's `status_in_item` of the ops [`qualifs`] reports: a drop of a value whose
    /// destructor is `[const]` needs `const_destruct`, and every other one is refused.
    fn of_qualif(kind: &ConstCheckKind) -> Status {
        match kind {
            ConstCheckKind::LiveDrop { needs_non_const_drop: false, .. } => Status::Unstable {
                gate: Symbol::intern("const_destruct"),
                already_checked: false,
                safe_to_expose: false,
                is_function_call: false,
            },
            _ => Status::Forbidden,
        }
    }

    /// `ConditionallyConstCall`'s: every conditionally-const call needs `const_trait_impl`.
    fn conditionally_const() -> Status {
        Status::Unstable {
            gate: Symbol::intern("const_trait_impl"),
            already_checked: false,
            safe_to_expose: false,
            is_function_call: false,
        }
    }
}

/// rustc's `Checker` of one body.
struct Checker<'a, 'db> {
    db: &'db Db,
    def: DefWithBodyId,
    body: &'a MirBody<'db>,
    store: &'db Body,
    infer: &'a InferenceResult<'db>,
    context: ConstContext,
    krate: Crate,
    features: &'db UnstableFeatures,
    lang: &'db LangItems,
    /// rustc's `enforce_recursive_const_stability`.
    recursive: bool,
    primary: Vec<ConstCheckDiagnostic>,
    /// The errors rustc reports only when there is no other (`DiagImportance::Secondary`).
    secondary: Vec<ConstCheckDiagnostic>,
    /// A call typeck refuses: rustc checks nothing of the body.
    tainted: bool,
}

impl<'a, 'db> Checker<'a, 'db> {
    fn new(
        db: &'db Db,
        def: DefWithBodyId,
        body: &'a MirBody<'db>,
        infer: &'a InferenceResult<'db>,
        context: ConstContext,
    ) -> Self {
        let krate = def.module(db).krate(db);
        let features = UnstableFeatures::query(db, krate);
        let recursive = match def {
            DefWithBodyId::FunctionId(func) => {
                context == ConstContext::ConstFn
                    && features.staged_api
                    && stability::safe_to_expose(db, stability::Item::Fn(func), krate)
            }
            _ => false,
        };
        Checker {
            db,
            def,
            body,
            store: Body::of(db, def),
            infer,
            context,
            krate,
            features,
            lang: hir_def::lang_item::lang_items(db, krate),
            recursive,
            primary: Vec::new(),   // alloc: none until an error
            secondary: Vec::new(), // alloc: none until an error
            tainted: false,
        }
    }

    fn finish(self) -> ConstCheck {
        eprintln!("DBGCC finish tainted={} primary={:?} secondary={:?}", self.tainted, self.primary, self.secondary);
        if self.tainted {
            return ConstCheck::default();
        }
        let diagnostics = match self.primary.is_empty() {
            true => self.secondary,
            false => self.primary,
        };
        ConstCheck { diagnostics }
    }

    fn statement(&mut self, kind: &StatementKind, span: MirSpan) {
        match kind {
            StatementKind::Assign(_, Rvalue::Cast(CastKind::PointerExposeAddress, _, _)) => {
                self.check_op(ConstCheckKind::PointerToInt, Status::Forbidden, span)
            }
            StatementKind::Assign(
                _,
                Rvalue::BinaryOp(op, lhs, _) | Rvalue::CheckedBinaryOp(op, lhs, _),
            ) if matches!(
                op,
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
            ) && operand_ty(self.body, lhs)
                .is_some_and(|ty| matches!(ty, TyKind::RawPtr(..) | TyKind::FnPtr(..))) =>
            {
                self.check_op(ConstCheckKind::PointerComparison, Status::Forbidden, span)
            }
            StatementKind::InlineAsm(_) => {
                self.check_op(ConstCheckKind::InlineAsm, Status::Forbidden, span)
            }
            _ => {}
        }
    }

    /// rustc's `check_op_spanned`.
    fn check_op(&mut self, kind: ConstCheckKind, status: Status, span: MirSpan) {
        if let Status::Unstable { gate, already_checked, safe_to_expose, is_function_call } =
            status
            && (already_checked || self.features.is_enabled(&gate))
        {
            if !safe_to_expose
                && self.recursive
                && let DefWithBodyId::FunctionId(func) = self.def
                && !stability::allows_const_fn_unstable(self.db, func, &gate)
            {
                let already_unstable = stability::written_const_unstable(self.db, func);
                self.report(
                    ConstCheckKind::UnstableInStableExposed {
                        gate,
                        is_function_call,
                        already_unstable,
                    },
                    span,
                );
            }
            return;
        }
        self.report(kind, span);
    }

    fn report(&mut self, kind: ConstCheckKind, span: MirSpan) {
        eprintln!("DBGCC report {kind:?} {span:?} written={}", self.written(span));
        if !self.written(span) {
            return;
        }
        let code = kind.code();
        // alloc: the text of a diagnostic, made when it is reported
        let mut message = String::new();
        kind.write_message(self.db, self.krate, self.context, &mut message)
            .expect("writing to a `String` does not fail");
        let secondary =
            matches!(kind, ConstCheckKind::EscapingMutBorrow | ConstCheckKind::EscapingCellBorrow);
        let diagnostic = ConstCheckDiagnostic { kind, context: self.context, code, span, message };
        match secondary {
            true => self.secondary.push(diagnostic),
            false => self.primary.push(diagnostic),
        }
    }

    /// Whether `span` is of something written in the source, not of what lowering made. Asked
    /// last, of what would be reported: a body with nothing to report is checked without
    /// building its source map.
    fn written(&self, span: MirSpan) -> bool {
        let source_map = || &Body::with_source_map(self.db, self.def).1;
        match span {
            MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => source_map().expr_syntax(expr).is_ok(),
            MirSpan::PatId(pat) => source_map().pat_syntax(pat).is_ok(),
            MirSpan::BindingId(_) | MirSpan::SelfParam => true,
            MirSpan::Unknown | MirSpan::Inlined(_) => false,
        }
    }
}

/// Whether `func` is written with `#[rustc_do_not_const_check]`, whose body rustc does not
/// check: core's `panic!` makes such a `const fn` in the body it is written in.
fn not_const_checked(db: &Db, func: FunctionId) -> bool {
    use hir_def::src::HasSource;
    use syntax::view::HasAttrs;
    let source = func.lookup(db).ast_ptr(db);
    source
        .value
        .to_view(source.file_id.root(db))
        .attrs()
        .any(|attr| attr.simple_name() == Some("rustc_do_not_const_check"))
}

/// Whether a call of `func` in a const context is one rustc does not refuse as a call of a
/// function that is not `const`: it is `const`, or a method of a trait or of an `impl` of one
/// (conditionally const), or `#[rustc_comptime]` (always const to rustc: `size_of`,
/// `align_of`, `needs_drop` and the reflection intrinsics are written so). An intrinsic is
/// otherwise callable only when it is declared `const`, as the nightly declares the others const
/// eval runs (`atomic_load` and the rest of the atomics, `transmute`, ..).
///
/// A comptime function called from a `const fn` is accepted too, where rustc may refuse it: this
/// check reports only what rustc reports.
fn is_const_callable(db: &Db, func: FunctionId) -> bool {
    FunctionSignature::of(db, func).is_const()
        || of_a_trait(db, func)
        || hir_def::attrs::AttrFlags::has_word(db, func.into(), "rustc_comptime")
}

/// How rustc's message names the function called: `function`, `method` or `associated
/// function`, and its path, which leaves out the crate when it is the caller's. A trait's
/// method is `<Self as Trait>::name`, `Self` as `args` have it.
fn described(
    db: &Db,
    caller: Crate,
    func: FunctionId,
    args: GenericArgs<'_>,
) -> (&'static str, String) {
    let signature = FunctionSignature::of(db, func);
    let name = signature.name.as_str();
    let assoc_descr = match signature.has_self_param() {
        true => "method",
        false => "associated function",
    };
    match func.lookup(db).container {
        ItemContainerId::ImplId(impl_) => {
            let self_ty = db.impl_self_ty(impl_).skip_binder();
            let ty = super::type_name::rustc_type_name(db, self_ty)
                .map(|it| {
                    let bare = it.split('<').next().unwrap_or(&it);
                    // alloc: the text of a diagnostic, made when it is reported
                    bare.rsplit("::").next().unwrap_or(bare).to_owned()
                })
                // alloc: the text of a diagnostic, made when it is reported
                .unwrap_or_else(|| "_".to_owned());
            // alloc: the text of a diagnostic, made when it is reported
            (assoc_descr, format!("{ty}::{name}"))
        }
        ItemContainerId::TraitId(trait_) => {
            let self_ty = self_ty_name(db, args.type_at(0));
            let trait_name = TraitSignature::of(db, trait_).name.as_str();
            // alloc: the text of a diagnostic, made when it is reported
            (assoc_descr, format!("<{self_ty} as {trait_name}>::{name}"))
        }
        _ => ("function", def_path(db, caller, func.module(db), name)),
    }
}

/// A trait method's `Self` as rustc's path writes it: a type parameter by its name.
fn self_ty_name(db: &Db, ty: Ty<'_>) -> TypeName {
    if let TyKind::Param(param) = ty.kind() {
        let params = GenericParams::of(db, param.id.parent());
        let name = params[param.id.local_id()].name().map_or("Self", |it| it.as_str());
        // alloc: the text of a diagnostic, made when it is reported
        return TypeName(Some(name.to_owned()));
    }
    TypeName(super::type_name::rustc_type_name(db, ty))
}

/// rustc's `def_path_str` of the item `name` in `module`: its modules' names and its own,
/// with the crate's name first when it is not `caller`.
fn def_path<'db>(db: &'db Db, caller: Crate, mut module: ModuleIdLt<'db>, name: &str) -> String {
    // alloc: the text of a diagnostic, made when it is reported
    let mut segments = vec![name.to_owned()];
    loop {
        let Some(parent) = module.containing_module(db) else {
            let krate = module.krate(db);
            if krate != caller
                && let Some(it) = krate.extra_data(db).display_name.as_ref()
            {
                // alloc: the text of a diagnostic, made when it is reported
                segments.push(it.crate_name().as_str().to_owned());
            }
            break;
        };
        if let Some(it) = module.name(db) {
            // alloc: the text of a diagnostic, made when it is reported
            segments.push(it.as_str().to_owned());
        }
        module = parent;
    }
    segments.reverse();
    segments.join("::")
}

/// The type of an operand, when it is a constant or a local.
fn operand_ty<'db>(body: &MirBody<'db>, operand: &Operand) -> Option<TyKind<'db>> {
    let ty = match &operand.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        OperandKind::Copy(place) | OperandKind::Move(place) => {
            body.locals[place.as_ref().as_local()?].ty.as_ref()
        }
        OperandKind::Static(_) => return None,
    };
    Some(ty.kind())
}

/// The blocks control reaches from the start when nothing unwinds, as rustc's
/// `traversal::reachable` after `SimplifyCfg` removed the others, without the blocks lowering
/// builds that rustc builds no way into.
fn reachable<'f>(body: &MirBody<'_>, frame: &'f Frame) -> Vec<BasicBlockId, &'f Frame> {
    let blocks = body.basic_blocks.len();
    let block_index = |block: BasicBlockId| block.into_raw().into_u32() as usize;
    let mut seen: Bits<'f> = bits_in(blocks, frame);
    let mut stack = Vec::with_capacity_in(blocks, frame);
    stack.push(body.start_block);
    let mut order = Vec::with_capacity_in(blocks, frame);
    while let Some(block) = stack.pop() {
        if seen.contains(block_index(block)) || body.unreached.contains(&block) {
            continue;
        }
        seen.insert(block_index(block));
        order.push(block);
        match body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind) {
            // `successors` copies these into a `SmallVec` that spills past two.
            Some(TerminatorKind::SwitchInt { targets, .. }) => {
                stack.extend_from_slice(targets.all_targets())
            }
            Some(kind) => stack.extend(kind.successors()),
            None => {}
        }
    }
    order
}
