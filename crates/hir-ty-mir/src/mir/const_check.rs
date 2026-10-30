//! rustc's `check_consts` over the MIR lowering builds: what the body of a const context (a
//! `const fn`, a `const` or `static` item's value, an enum discriminant) may not do, with
//! rustc's error codes and messages (`compiler/rustc_const_eval/src/check_consts`).
//!
//! It reports only what rustc reports, never more: a program rustc accepts in const is
//! accepted here. As rustc, it looks at the blocks control reaches when nothing unwinds (rustc
//! checks after `SimplifyCfg::Initial` and skips cleanup blocks), and of the calls only at
//! those the source writes, to a function known whatever the generic arguments are: a call of
//! a trait's method may be to a `const` impl, which rustc decides by its const conditions,
//! and is left alone. An intrinsic not declared `const` is refused as rustc refuses it.

use base_db::Db;
use blaze_mem::{Frame, Tag, frame};
use hir_def::{
    CallableDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup,
    expr_store::Body,
    hir::Expr,
    signatures::{FunctionSignature, ImplSignature},
};
use rustc_type_ir::inherent::IntoKind;
use salsa::SalsaValue;

use hir_ty::{InferBodyId, InferenceResult, db::HirDatabase, next_solver::TyKind};

use super::{
    BasicBlockId, BinOp, CastKind, MirBody, MirSpan, Operand, OperandKind, Rvalue, StatementKind,
    TerminatorKind,
    framed::{Bits, bits_in},
};

#[derive(Debug, Clone, PartialEq, Eq, SalsaValue)]
pub struct ConstCheckDiagnostic {
    /// rustc's error code, as `E0015`, or `""` for the errors rustc gives none.
    pub code: &'static str,
    pub span: MirSpan,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, SalsaValue)]
pub struct ConstCheck {
    pub diagnostics: Vec<ConstCheckDiagnostic>,
}

/// rustc's `ConstContext`: what kind of const context a body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// The const context `def`'s body is, if it is one: a `const fn` (not one of a trait or of a
/// trait's `impl`, which rustc checks by the trait's constness), a `const`, a `static` or an
/// enum discriminant.
pub fn const_context(db: &Db, def: DefWithBodyId) -> Option<ConstContext> {
    match def {
        DefWithBodyId::FunctionId(func) => {
            if !FunctionSignature::of(db, func).is_const() || of_a_trait(db, func) {
                return None;
            }
            Some(ConstContext::ConstFn)
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
    let mut result = ConstCheck::default();
    let Some(kind) = const_context(db, def) else { return result };
    if let DefWithBodyId::FunctionId(func) = def
        && not_const_checked(db, func)
    {
        return result;
    }
    let infer = InferenceResult::of(db, def);
    if infer.has_type_mismatches() || infer.is_erroneous() || !infer.diagnostics().is_empty() {
        return result;
    }
    let Ok(body) = db.mir_body(InferBodyId::DefWithBodyId(def)) else { return result };
    let store = Body::of(db, def);
    // Whether `span` is an expression written in the source, not one lowering made. Asked
    // last, of what would be reported: a body with nothing to report is checked without
    // building its source map.
    let written = |span: MirSpan| match span {
        MirSpan::ExprId(expr) => Body::with_source_map(db, def).1.expr_syntax(expr).is_ok(),
        _ => false,
    };
    let mut report = |code: &'static str, span: MirSpan, message: String| {
        result.diagnostics.push(ConstCheckDiagnostic { code, span, message });
    };
    frame::task(Tag::Mir, |frame| {
        for &block in reachable(body, frame).iter() {
            let block = &body.basic_blocks[block];
            if block.is_cleanup {
                continue;
            }
            for statement in &block.statements {
                match &statement.kind {
                    StatementKind::Assign(_, Rvalue::Cast(CastKind::PointerExposeAddress, _, _))
                        if written(statement.span) =>
                    {
                        report(
                            "",
                            statement.span,
                            // alloc: the text of a diagnostic, made when it is reported
                            "pointers cannot be cast to integers during const eval".to_owned(),
                        );
                    }
                    StatementKind::Assign(
                        _,
                        Rvalue::BinaryOp(op, lhs, _) | Rvalue::CheckedBinaryOp(op, lhs, _),
                    ) if matches!(
                        op,
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
                    ) && operand_ty(body, lhs)
                        .is_some_and(|ty| matches!(ty, TyKind::RawPtr(..) | TyKind::FnPtr(..)))
                        && written(statement.span) =>
                    {
                        report(
                            "",
                            statement.span,
                            // alloc: the text of a diagnostic, made when it is reported
                            "pointers cannot be reliably compared during const eval".to_owned(),
                        );
                    }
                    StatementKind::InlineAsm(_) if written(statement.span) => {
                        report(
                            "E0015",
                            statement.span,
                            // alloc: the text of a diagnostic, made when it is reported
                            format!("inline assembly is not allowed in {}", kind.plural()),
                        );
                    }
                    _ => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            let TerminatorKind::Call { func, from_hir_call: true, intrinsic, .. } = &terminator.kind
            else {
                continue;
            };
            let MirSpan::ExprId(expr) = terminator.span else { continue };
            match operand_ty(body, func) {
                Some(TyKind::FnPtr(..))
                    if matches!(store.expr(expr), Expr::Call { .. }) && written(terminator.span) =>
                {
                    report(
                        "",
                        terminator.span,
                        // alloc: the text of a diagnostic, made when it is reported
                        format!("function pointer calls are not allowed in {}", kind.plural()),
                    );
                }
                Some(TyKind::FnDef(callee, _)) => {
                    let CallableDefId::FunctionId(callee) = callee.0 else { continue };
                    // The function the source calls, and not one lowering calls for it.
                    let called = match store.expr(expr) {
                        Expr::Call { callee: path, .. } => match infer.expr_ty(path).kind() {
                            TyKind::FnDef(it, _) => it.0 == CallableDefId::FunctionId(callee),
                            _ => false,
                        },
                        Expr::MethodCall { .. } => {
                            infer.method_resolution(expr).is_some_and(|(it, _)| it == callee)
                        }
                        _ => false,
                    };
                    if !called || is_const_callable(db, callee) || !written(terminator.span) {
                        continue;
                    }
                    // rustc's `IntrinsicNonConst`, for an intrinsic that is not `const` (nor
                    // `#[rustc_comptime]`, which `is_const_callable` accepts).
                    if let Some(intrinsic) = intrinsic {
                        report(
                            "",
                            terminator.span,
                            // alloc: the text of a diagnostic, made when it is reported
                            format!(
                                "cannot call non-const intrinsic `{intrinsic}` in {}",
                                kind.plural()
                            ),
                        );
                        continue;
                    }
                    let (descr, path) = described(db, def, callee);
                    report(
                        "E0015",
                        terminator.span,
                        // alloc: the text of a diagnostic, made when it is reported
                        format!("cannot call non-const {descr} `{path}` in {}", kind.plural()),
                    );
                }
                _ => {}
            }
        }
    });
    result
}

/// Whether `func` is written with `#[rustc_do_not_const_check]`, whose body rustc does not
/// check: core's `panic!` makes such a `const fn` in the body it is written in.
fn not_const_checked(db: &Db, func: FunctionId) -> bool {
    use hir_def::src::HasSource;
    use syntax::ast::HasAttrs;
    let source = func.lookup(db).source(db);
    source
        .value
        .attrs()
        .any(|attr| attr.simple_name().as_deref() == Some("rustc_do_not_const_check"))
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
/// function`, and its path, which leaves out the crate when it is the caller's.
fn described(
    db: &Db,
    caller: DefWithBodyId,
    func: FunctionId,
) -> (&'static str, String) {
    let signature = FunctionSignature::of(db, func);
    let name = signature.name.as_str();
    match func.lookup(db).container {
        ItemContainerId::ImplId(impl_) => {
            let descr = match signature.has_self_param() {
                true => "method",
                false => "associated function",
            };
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
            (descr, format!("{ty}::{name}"))
        }
        _ => {
            // alloc: the text of a diagnostic, made when it is reported
            let mut segments = vec![name.to_owned()];
            let mut module = func.module(db);
            loop {
                let Some(parent) = module.containing_module(db) else {
                    let krate = module.krate(db);
                    if krate != caller.module(db).krate(db)
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
            ("function", segments.join("::"))
        }
    }
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
