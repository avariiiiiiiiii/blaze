//! The body surgery of rustc's `inline_call` and its `Integrator`: the callee's locals and
//! blocks appended to the caller's, its parameters the call's arguments, its return place a
//! local the caller copies out of, its unwinding the call's, its spans in a scope of its own.

use std::ops::Range;

use rustc_hash::FxHashMap;
use rustc_type_ir::inherent::IntoKind;

use crate::{
    mir::{
        BasicBlock, BasicBlockId, BorrowKind, FieldIndex, InlinedBinding, InlinedScope,
        InlinedSpan, Local, LocalId, MirBody, MirSpan, MutBorrowKind, Operand, OperandKind,
        Place, ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, Terminator,
        TerminatorKind, return_slot, transform::Ctx,
    },
    next_solver::{Region, Ty, TyKind},
};
use rustc_ast_ir::Mutability;

use super::super::{block_id, block_index, local_index, map_place, successors_mut};
use super::operand_ty;

/// What [`inline_call`] needs of the call besides the bodies.
pub(super) struct Site<'a> {
    pub(super) block: BasicBlockId,
    /// The call passes a closure's arguments as a tuple, which the closure's body takes one
    /// by one.
    pub(super) spread: bool,
    /// The callee's index in the caller's [`Inlined::callees`](crate::mir::Inlined::callees).
    pub(super) callee: u32,
    /// For each of the callee's own inlined callees, its index in the caller's.
    pub(super) nested: &'a [u32],
}

/// Temporaries the call needs: each live from the call on, dead where it returns.
struct Temps {
    span: MirSpan,
    /// Statements before the jump into the callee.
    before: Vec<Statement>,
    /// The temporaries, in the order they were made.
    made: Vec<LocalId>,
}

impl Temps {
    fn new_temp<'db>(&mut self, body: &mut MirBody<'db>, ty: Ty<'db>) -> LocalId {
        let local = body.locals.alloc(Local { ty: ty.store() });
        self.before.push(Statement { kind: StatementKind::StorageLive(local), span: self.span });
        self.made.push(local);
        local
    }

    fn assign(&mut self, place: StoredPlace, rvalue: Rvalue) {
        self.before.push(Statement { kind: StatementKind::Assign(place, rvalue), span: self.span });
    }

    /// rustc's `create_temp_if_necessary`: a temporary the argument is moved out of is the
    /// parameter itself.
    fn argument<'db>(&mut self, ctx: &Ctx<'db>, body: &mut MirBody<'db>, arg: Operand) -> LocalId {
        if let OperandKind::Move(place) = &arg.kind
            && place.projection.is_empty()
            && place.local != return_slot()
            && !body.param_locals.contains(&place.local)
        {
            return place.local;
        }
        let ty = operand_ty(ctx, body, &arg);
        let local = self.new_temp(body, ty);
        self.assign(Place::from(local).store(), Rvalue::Use(arg));
        local
    }
}

/// The spans of the callee, which become [`MirSpan::Inlined`] of the caller.
struct Spans<'a> {
    /// The scope of this call.
    scope: u32,
    /// Where the callee's own inlined spans start among the caller's.
    base: u32,
    own: FxHashMap<MirSpan, u32>,
    spans: &'a mut Vec<InlinedSpan>,
}

impl Spans<'_> {
    fn map(&mut self, span: MirSpan) -> MirSpan {
        if let MirSpan::Inlined(at) = span {
            return MirSpan::Inlined(self.base + at);
        }
        let next = self.spans.len() as u32;
        let at = *self.own.entry(span).or_insert(next);
        if at == next {
            self.spans.push(InlinedSpan { scope: self.scope, span });
        }
        MirSpan::Inlined(at)
    }

    fn operand(&mut self, operand: &mut Operand) {
        if let Some(span) = &mut operand.span {
            *span = self.map(*span);
        }
    }
}

/// The call at `site.block` replaced by `callee`, a body of the callee instantiated for the
/// call. Says which blocks are the callee's now.
pub(super) fn inline_call<'db>(
    ctx: &Ctx<'db>,
    body: &mut MirBody<'db>,
    site: Site<'_>,
    mut callee: MirBody<'db>,
) -> Range<usize> {
    let interner = ctx.infcx.interner;
    let Some(terminator) = body.basic_blocks[site.block].terminator.take() else {
        return 0..0;
    };
    let span = terminator.span;
    let TerminatorKind::Call { args, destination, target, cleanup, .. } = terminator.kind else {
        body.basic_blocks[site.block].terminator = Some(terminator);
        return 0..0;
    };
    let return_block = target.map(|target| {
        let is_cleanup = body.basic_blocks[target].is_cleanup;
        body.basic_blocks.alloc(BasicBlock {
            statements: Vec::new(),
            terminator: Some(Terminator { span, kind: TerminatorKind::Goto { target } }),
            is_cleanup,
        })
    });
    let mut temps = Temps { span, before: Vec::new(), made: Vec::new() };

    // `a[*i] = f(i)`, where `f` may change `i`: what the place is is taken before the call.
    let needs_borrow = destination
        .projection
        .as_slice()
        .iter()
        .any(|it| matches!(it, ProjectionElem::Deref | ProjectionElem::Index(_)));
    let destination_ty = destination.as_ref().ty(body, &ctx.infcx, ctx.env).ty;
    let dest = if needs_borrow {
        let ty = Ty::new_ref(interner, Region::new_erased(interner), destination_ty, Mutability::Mut);
        let temp = temps.new_temp(body, ty);
        let borrow = Rvalue::Ref(BorrowKind::Mut { kind: MutBorrowKind::Default }, destination);
        temps.assign(Place::from(temp).store(), borrow);
        Place::from(temp).project(ProjectionElem::Deref).store()
    } else {
        destination
    };
    // `_0` may be where a place with projections is wanted, so it is always a local.
    let destination_local = temps.new_temp(body, destination_ty);

    let mut params: Vec<LocalId> = Vec::with_capacity(callee.param_locals.len());
    let mut args = args.into_vec().into_iter();
    if site.spread {
        let first = args.next();
        let second = args.next();
        let (receiver, tuple) = match (first, second) {
            (Some(receiver), Some(tuple)) => (Some(receiver), tuple),
            (Some(tuple), None) => (None, tuple),
            (None, _) => (None, Operand { kind: OperandKind::Copy(Place::from(return_slot()).store()), span: None }),
        };
        if let Some(receiver) = receiver {
            params.push(temps.argument(ctx, body, receiver));
        }
        let tuple = temps.argument(ctx, body, tuple);
        if let TyKind::Tuple(fields) = body.locals[tuple].ty.as_ref().kind() {
            for index in 0..fields.len() {
                let field =
                    Place::from(tuple).project(ProjectionElem::Field(FieldIndex(index as u32)));
                let field = Operand { kind: OperandKind::Move(field.store()), span: None };
                params.push(temps.argument(ctx, body, field));
            }
        }
    } else {
        for arg in args {
            params.push(temps.argument(ctx, body, arg));
        }
    }

    // Each local of the callee: `_0` the destination, a parameter its argument, any other a
    // new local of the caller.
    let mut always_live = vec![true; callee.locals.len()];
    for (_, block) in callee.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                statement.kind
            {
                always_live[local_index(local)] = false;
            }
        }
    }
    let mut map: Vec<LocalId> = Vec::with_capacity(callee.locals.len());
    let mut fresh: Vec<LocalId> = Vec::new();
    let locals = std::mem::take(&mut callee.locals);
    for (local, decl) in locals.into_iter().enumerate() {
        let local = super::super::local_id(local);
        let mapped = if local == return_slot() {
            destination_local
        } else if let Some(at) = callee.param_locals.iter().position(|it| *it == local) {
            params[at]
        } else {
            let new = body.locals.alloc(decl);
            if always_live[local_index(local)] {
                fresh.push(new);
            }
            new
        };
        map.push(mapped);
    }
    let lookup = |local: LocalId| map[local_index(local)];
    for &local in &fresh {
        temps.before.push(Statement { kind: StatementKind::StorageLive(local), span });
    }

    // The scope of this call, the callee's own scopes and spans after it.
    let scope = body.inlined.scopes.len() as u32;
    body.inlined.scopes.push(InlinedScope { callee: site.callee, call_site: span });
    let scope_base = scope + 1;
    let base = body.inlined.spans.len() as u32;
    body.inlined.spans.extend(
        callee
            .inlined
            .spans
            .iter()
            .map(|it| InlinedSpan { scope: scope_base + it.scope, span: it.span }),
    );
    let mut spans =
        Spans { scope, base, own: FxHashMap::default(), spans: &mut body.inlined.spans };
    let scopes: Vec<InlinedScope> = callee
        .inlined
        .scopes
        .iter()
        .map(|it| InlinedScope {
            callee: site.nested[it.callee as usize],
            call_site: spans.map(it.call_site),
        })
        .collect();

    let offset = body.basic_blocks.len();
    let map_block = |block: BasicBlockId| block_id(offset + block_index(block));
    callee.walk_places(|place| map_place(place, &mut |local| lookup(local)));
    for (_, block) in callee.basic_blocks.iter_mut() {
        for statement in &mut block.statements {
            statement.span = spans.map(statement.span);
            statement.kind.for_each_operand_mut(&mut |it| spans.operand(it));
            if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                &mut statement.kind
            {
                *local = lookup(*local);
            }
        }
        let Some(terminator) = &mut block.terminator else { continue };
        terminator.span = spans.map(terminator.span);
        terminator.kind.for_each_operand_mut(&mut |it| spans.operand(it));
        successors_mut(&mut terminator.kind, |it| *it = map_block(*it));
        match &mut terminator.kind {
            TerminatorKind::Return => {
                terminator.kind = match return_block {
                    Some(target) => TerminatorKind::Goto { target },
                    None => TerminatorKind::Unreachable,
                };
            }
            TerminatorKind::UnwindResume => {
                if let Some(target) = cleanup {
                    terminator.kind = TerminatorKind::Goto { target };
                }
            }
            // Unwinding out of the callee is unwinding out of the call; in a cleanup block
            // no unwind edge is one that terminates.
            kind if !block.is_cleanup => {
                if let Some(unwind @ None) = kind.unwind_mut() {
                    *unwind = cleanup;
                }
            }
            _ => {}
        }
    }
    let callee_start = map_block(callee.start_block);

    let bindings = callee.binding_locals.iter().map(|(binding, &local)| InlinedBinding {
        scope,
        binding,
        local: lookup(local),
    });
    let nested_bindings = callee.inlined.bindings.iter().map(|it| InlinedBinding {
        scope: scope_base + it.scope,
        binding: it.binding,
        local: lookup(it.local),
    });
    let bindings: Vec<InlinedBinding> = bindings.chain(nested_bindings).collect();
    body.inlined.scopes.extend(scopes);
    body.inlined.bindings.extend(bindings);

    if let Some(block) = return_block {
        let statements = &mut body.basic_blocks[block].statements;
        statements.reserve(1 + fresh.len() + temps.made.len());
        let moved = Operand { kind: OperandKind::Move(Place::from(destination_local).store()), span: None };
        statements.push(Statement { kind: StatementKind::Assign(dest, Rvalue::Use(moved)), span });
        for &local in fresh.iter().rev().chain(temps.made.iter().rev()) {
            statements.push(Statement { kind: StatementKind::StorageDead(local), span });
        }
    }
    let caller_block = &mut body.basic_blocks[site.block];
    caller_block.statements.append(&mut temps.before);
    caller_block.terminator =
        Some(Terminator { span, kind: TerminatorKind::Goto { target: callee_start } });

    let unreached = callee.unreached.iter().map(|&it| map_block(it));
    body.unreached.extend(unreached);
    let panic_unwinds =
        callee.panic_unwinds.iter().map(|&(block, at, to)| (map_block(block), at, map_block(to)));
    body.panic_unwinds.extend(panic_unwinds);
    let ends: Vec<LocalId> = callee
        .scope_ends
        .iter()
        .map(|&it| lookup(it))
        .filter(|it| local_index(*it) >= offset_locals(&map, &params, destination_local))
        .collect();
    body.scope_ends.splice(0..0, ends);

    for (_, block) in std::mem::take(&mut callee.basic_blocks).into_iter() {
        body.basic_blocks.alloc(block);
    }
    offset..body.basic_blocks.len()
}
