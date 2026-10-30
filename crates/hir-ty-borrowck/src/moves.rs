//! Move paths, and where each may be uninitialized or has ever been initialized: rustc's
//! `MoveData` with `MaybeUninitializedPlaces` and `EverInitializedPlaces`.
//!
//! A move path is a local, or a field, variant, element of an array of known length or the
//! contents of a `Box` of another path. Every local has one; a part has one when the body
//! moves out of it, assigns it or drops it. What is behind any other pointer, at an index
//! known only when the body runs, or inside a value whose type implements `Drop`, has none:
//! it cannot be moved out of, and the nearest path above stands for it. A field of a union is
//! the union.

use rustc_hash::FxHashMap;
use rustc_type_ir::inherent::IntoKind;

use hir_ty::{
    consteval::try_const_usize,
    mir::{
        AsmOperand, BasicBlockId, LocalId, MirBody, PlaceElem, PlaceTy, ProjectionElem, Rvalue,
        StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::TyKind,
};

use super::{
    Ctx,
    bitset::{BitMatrix, BitSet},
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, forward},
    relation::{Facts, Relation},
};

pub(super) type Mpi = usize;

pub(super) struct MovePath {
    pub(super) local: LocalId,
    pub(super) projection: Vec<PlaceElem>,
    pub(super) parent: Option<Mpi>,
    pub(super) children: Vec<Mpi>,
}

/// What a place is in terms of move paths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Lookup {
    Exact(Mpi),
    /// The place has no path of its own; this is the nearest one above it.
    Parent(Mpi),
}

impl Lookup {
    pub(super) fn path(self) -> Mpi {
        match self {
            Lookup::Exact(it) | Lookup::Parent(it) => it,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MoveOut {
    pub(super) path: Mpi,
    pub(super) location: Location,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum InitKind {
    Deep,
    /// The destination of a call, initialized only if the call returns.
    NonPanicPathOnly,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Init {
    pub(super) path: Mpi,
    /// `None` for an argument, initialized before the body starts.
    pub(super) location: Option<Location>,
    pub(super) kind: InitKind,
}

/// The kinds of [`MoveData`]'s facts: the moves and the inits at a point, the moves of a path.
const MOVE_AT: u8 = 0;
const INIT_AT: u8 = 1;
const MOVE_OF: u8 = 2;

pub(super) struct MoveData {
    pub(super) paths: Vec<MovePath>,
    locals: Vec<Mpi>,
    /// The path below a path by one step.
    steps: FxHashMap<(u32, PlaceElem), Mpi>,
    pub(super) moves: Vec<MoveOut>,
    pub(super) inits: Vec<Init>,
    /// The moves and inits at each point and of each path, each in the order it was found.
    facts: Relation,
}

impl MoveData {
    pub(super) fn gather<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, cfg: &Cfg) -> Self {
        let mut data = MoveData {
            paths: Vec::new(),
            locals: Vec::new(),
            steps: FxHashMap::default(),
            moves: Vec::new(),
            inits: Vec::new(),
            facts: Relation::default(),
        };
        for (local, _) in body.locals.iter() {
            let id = data.paths.len();
            data.paths.push(MovePath {
                local,
                projection: Vec::new(),
                parent: None,
                children: Vec::new(),
            });
            data.locals.push(id);
        }
        for &local in &body.param_locals {
            let path = data.locals[local.into_raw().into_u32() as usize];
            data.inits.push(Init { path, location: None, kind: InitKind::Deep });
        }
        for &block in &cfg.order {
            let data_block = &body.basic_blocks[block];
            for (statement, it) in data_block.statements.iter().enumerate() {
                let location = Location { block, statement };
                let compares =
                    matches!(&it.kind, StatementKind::Assign(_, rvalue) if super::compares(rvalue));
                it.kind.for_each_operand(&mut |operand| {
                    if !compares
                        && let Some(place) = ctx.moved_place(body, operand)
                        && !ctx.moves_out_of_guard(
                            place.local,
                            place.projection.as_slice(),
                            operand.span().unwrap_or(it.span),
                        )
                    {
                        data.gather_move(ctx, body, place, location);
                    }
                });
                match &it.kind {
                    StatementKind::Assign(place, rvalue) => {
                        if let Rvalue::Discriminant(moved) = rvalue
                            && ctx.cast_moves(body, moved, it.span)
                        {
                            data.gather_move(ctx, body, moved, location);
                        }
                        data.gather_init(ctx, body, place, location, InitKind::Deep)
                    }
                    StatementKind::StorageDead(local) => {
                        if !body.param_locals.contains(local) {
                            let path = data.locals[local.into_raw().into_u32() as usize];
                            data.record_move(path, location);
                        }
                    }
                    StatementKind::Deinit(place) => data.gather_move(ctx, body, place, location),
                    StatementKind::InlineAsm(asm) => {
                        for operand in &asm.operands {
                            if let AsmOperand::Reg { output: Some(output), .. } = operand {
                                data.gather_init(ctx, body, output, location, InitKind::Deep);
                            }
                        }
                    }
                    _ => (),
                }
            }
            let Some(terminator) = &data_block.terminator else { continue };
            let location = Location { block, statement: data_block.statements.len() };
            terminator.kind.for_each_operand(&mut |operand| {
                if let Some(place) = ctx.moved_place(body, operand)
                    && !ctx.moves_out_of_guard(
                        place.local,
                        place.projection.as_slice(),
                        operand.span().unwrap_or(terminator.span),
                    )
                {
                    data.gather_move(ctx, body, place, location);
                }
            });
            match &terminator.kind {
                TerminatorKind::Drop { place, .. } => data.gather_move(ctx, body, place, location),
                TerminatorKind::DropAndReplace { place, .. } => {
                    data.gather_init(ctx, body, place, location, InitKind::Deep)
                }
                TerminatorKind::Call { destination, .. } => {
                    data.gather_init(ctx, body, destination, location, InitKind::NonPanicPathOnly)
                }
                TerminatorKind::Yield { resume_arg, .. } => {
                    data.gather_init(ctx, body, resume_arg, location, InitKind::Deep)
                }
                _ => (),
            }
        }
        let mut facts = Facts::default();
        for (index, it) in data.moves.iter().enumerate() {
            facts.push(MOVE_AT, cfg.point(it.location), index);
            facts.push(MOVE_OF, it.path, index);
        }
        for (index, it) in data.inits.iter().enumerate() {
            if let Some(location) = it.location {
                facts.push(INIT_AT, cfg.point(location), index);
            }
        }
        data.facts = facts.freeze(&[cfg.points, cfg.points, data.paths.len()]);
        data
    }

    /// The moves out at `point`, in the order they happen.
    pub(super) fn moves_at(&self, point: usize) -> &[u32] {
        self.facts.get(MOVE_AT, point)
    }

    /// The inits at `point`, in the order they happen.
    pub(super) fn inits_at(&self, point: usize) -> &[u32] {
        self.facts.get(INIT_AT, point)
    }

    /// The moves out of `path`.
    pub(super) fn moves_of(&self, path: Mpi) -> &[u32] {
        self.facts.get(MOVE_OF, path)
    }

    fn record_move(&mut self, path: Mpi, location: Location) {
        self.moves.push(MoveOut { path, location });
    }

    fn gather_move<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        place: &StoredPlace,
        location: Location,
    ) {
        // A move of a subslice of an array moves each element of it.
        if let Some((ProjectionElem::Subslice { from, to }, base)) =
            place.projection.as_slice().split_last()
            && let Lookup::Exact(array) = self.create_projection(ctx, body, place.local, base)
            && let TyKind::Array(_, len) = ctx.place_ty(body, place.local, base).ty.kind()
            && let Some(len) = try_const_usize(ctx.db, len)
        {
            for offset in *from..(len as u64).saturating_sub(*to) {
                let path = self.child(
                    array,
                    place.local,
                    ProjectionElem::ConstantIndex {
                        offset,
                        min_length: offset + 1,
                        from_end: false,
                    },
                );
                self.record_move(path, location);
            }
            return;
        }
        match self.create(ctx, body, place) {
            Lookup::Exact(path) => self.record_move(path, location),
            // A move out of a field of a union moves the union, as long as nothing else on the
            // way makes it an error.
            Lookup::Parent(path) => {
                let projection = place.projection.as_slice();
                let depth = self.paths[path].projection.len();
                let base_ty = ctx.place_ty(body, place.local, &projection[..depth]).ty;
                if matches!(projection[depth], ProjectionElem::Field(_))
                    && matches!(base_ty.kind(), TyKind::Adt(def, _) if def.is_union())
                    && projection[depth + 1..]
                        .iter()
                        .all(|it| matches!(it, ProjectionElem::Field(_)))
                {
                    self.record_move(path, location);
                }
            }
        }
    }

    fn child(&mut self, parent: Mpi, local: LocalId, step: PlaceElem) -> Mpi {
        if let Some(&it) = self.steps.get(&(parent as u32, step)) {
            return it;
        }
        let mut projection = self.paths[parent].projection.clone();
        projection.push(step);
        let id = self.paths.len();
        self.paths.push(MovePath { local, projection, parent: Some(parent), children: Vec::new() });
        self.paths[parent].children.push(id);
        self.steps.insert((parent as u32, step), id);
        id
    }

    fn gather_init<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        place: &StoredPlace,
        location: Location,
        kind: InitKind,
    ) {
        let lookup = self.create(ctx, body, place);
        let path = match lookup {
            Lookup::Exact(path) => path,
            // Assigning a field of a union initializes the union.
            Lookup::Parent(path) if self.is_union_field(ctx, body, place, path) => path,
            Lookup::Parent(_) => return,
        };
        self.inits.push(Init { path, location: Some(location), kind });
    }

    fn is_union_field<'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        place: &StoredPlace,
        parent: Mpi,
    ) -> bool {
        let projection = place.projection.as_slice();
        let depth = self.paths[parent].projection.len();
        if projection.len() != depth + 1 || !matches!(projection[depth], ProjectionElem::Field(_)) {
            return false;
        }
        let ty = ctx.place_ty(body, place.local, &projection[..depth]).ty;
        matches!(ty.kind(), TyKind::Adt(def, _) if def.is_union())
    }

    /// The path of `place`, made if it can have one.
    fn create<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        place: &StoredPlace,
    ) -> Lookup {
        self.create_projection(ctx, body, place.local, place.projection.as_slice())
    }

    fn create_projection<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Lookup {
        let mut path = self.locals[local.into_raw().into_u32() as usize];
        let mut ty = PlaceTy::from_ty(body.locals[local].ty.as_ref());
        for elem in projection {
            let Some(step) = ctx.trackable(ty, elem) else { return Lookup::Parent(path) };
            path = self.child(path, local, step);
            ty = ctx.project(ty, elem);
        }
        Lookup::Exact(path)
    }

    /// The path of `place` if it has been made, or the nearest one above it.
    pub(super) fn find<'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Lookup {
        let mut path = self.locals[local.into_raw().into_u32() as usize];
        let mut ty = PlaceTy::from_ty(body.locals[local].ty.as_ref());
        for elem in projection {
            let Some(step) = ctx.trackable(ty, elem) else { return Lookup::Parent(path) };
            match self.steps.get(&(path as u32, step)) {
                Some(&it) => path = it,
                None => return Lookup::Parent(path),
            }
            ty = ctx.project(ty, elem);
        }
        Lookup::Exact(path)
    }

    pub(super) fn local_path(&self, local: LocalId) -> Mpi {
        self.locals[local.into_raw().into_u32() as usize]
    }

    /// `path` and every path below it.
    pub(super) fn with_descendants(&self, path: Mpi, f: &mut impl FnMut(Mpi)) {
        f(path);
        for &child in &self.paths[path].children {
            self.with_descendants(child, f);
        }
    }

    pub(super) fn find_in_descendants(&self, path: Mpi, f: &impl Fn(Mpi) -> bool) -> Option<Mpi> {
        if f(path) {
            return Some(path);
        }
        self.paths[path].children.iter().find_map(|&child| self.find_in_descendants(child, f))
    }

    pub(super) fn is_ancestor_or_self(&self, ancestor: Mpi, mut path: Mpi) -> bool {
        loop {
            if path == ancestor {
                return true;
            }
            match self.paths[path].parent {
                Some(parent) => path = parent,
                None => return false,
            }
        }
    }
}

impl<'db> Ctx<'_, 'db> {
    /// The step of a move path that `elem` of a place of type `ty` is, if it has one.
    pub(super) fn trackable(&self, ty: PlaceTy<'db>, elem: &PlaceElem) -> Option<PlaceElem> {
        match elem {
            ProjectionElem::Deref => ty.ty.is_box().then_some(ProjectionElem::Deref),
            ProjectionElem::Field(_) => match ty.ty.kind() {
                TyKind::Adt(def, _) if def.is_union() => None,
                TyKind::Adt(def, _)
                    if ty.variant_id.is_none() && self.has_destructor(def.def_id()) =>
                {
                    None
                }
                TyKind::Adt(def, _)
                    if ty.variant_id.is_some() && self.has_destructor(def.def_id()) =>
                {
                    None
                }
                _ => Some(*elem),
            },
            ProjectionElem::Downcast(_) => match ty.ty.kind() {
                TyKind::Adt(def, _) if self.has_destructor(def.def_id()) => None,
                _ => Some(*elem),
            },
            ProjectionElem::Index(_) | ProjectionElem::Subslice { .. } => None,
            &ProjectionElem::ConstantIndex { offset, from_end, .. } => match ty.ty.kind() {
                TyKind::Array(_, len) => {
                    let len = try_const_usize(self.db, len)? as u64;
                    let offset = if from_end { len.checked_sub(offset)? } else { offset };
                    Some(ProjectionElem::ConstantIndex {
                        offset,
                        min_length: offset + 1,
                        from_end: false,
                    })
                }
                _ => None,
            },
        }
    }
}

/// Which paths may be uninitialized at the start of each block: rustc's
/// `MaybeUninitializedPlaces`. And per local, whether it may have been initialized since its
/// storage last ended: `EverInitializedPlaces`, of whole locals.
pub(super) struct InitFlow {
    /// A row of move paths per block.
    pub(super) uninit_entry: BitMatrix,
    /// A row of locals per block.
    pub(super) ever_entry: BitMatrix,
}

/// What `run` does to the empty and to the full sets of paths and of locals: the gen and kill
/// sets of both, as [`GenKill::set_from`] takes them.
fn runs(
    paths: usize,
    locals: usize,
    mut run: impl FnMut(&mut BitSet, &mut BitSet),
) -> [(BitSet, BitSet); 2] {
    let (mut uninit_made, mut ever_made) = (BitSet::new(paths), BitSet::new(locals));
    let (mut uninit_kept, mut ever_kept) = (BitSet::filled(paths), BitSet::filled(locals));
    run(&mut uninit_made, &mut ever_made);
    run(&mut uninit_kept, &mut ever_kept);
    [(uninit_made, uninit_kept), (ever_made, ever_kept)]
}

impl InitFlow {
    pub(super) fn compute(body: &MirBody<'_>, cfg: &Cfg, moves: &MoveData) -> Self {
        let blocks = body.basic_blocks.len();
        let paths = moves.paths.len();
        let locals = body.locals.len();
        let mut uninit = GenKill::new(blocks, paths);
        let mut ever = GenKill::new(blocks, locals);
        // A call's return: what its destination being initialized does on that edge only.
        let mut returned_uninit = GenKill::new(blocks, paths);
        let mut returned_ever = GenKill::new(blocks, locals);
        let mut returns_to: Vec<Option<BasicBlockId>> = vec![None; blocks];
        for &block in &cfg.order {
            let data = &body.basic_blocks[block];
            let [(made, kept), (ever_made, ever_kept)] = runs(paths, locals, |uninit, ever| {
                for statement in 0..=data.statements.len() {
                    apply(body, moves, cfg, Location { block, statement }, uninit, ever);
                }
            });
            uninit.set_from(index(block), &made, &kept);
            ever.set_from(index(block), &ever_made, &ever_kept);
            if let Some(TerminatorKind::Call { target: Some(target), .. }) =
                data.terminator.as_ref().map(|it| &it.kind)
            {
                returns_to[index(block)] = Some(*target);
                let location = Location { block, statement: data.statements.len() };
                let [(made, kept), (ever_made, ever_kept)] =
                    runs(paths, locals, |uninit, ever| {
                        call_returned(moves, cfg, location, uninit, ever)
                    });
                returned_uninit.set_from(index(block), &made, &kept);
                returned_ever.set_from(index(block), &ever_made, &ever_kept);
            }
        }
        let mut uninit_start = BitSet::filled(paths);
        let mut ever_start = BitSet::new(locals);
        for init in moves.inits.iter().filter(|it| it.location.is_none()) {
            moves.with_descendants(init.path, &mut |it| uninit_start.remove(it));
            ever_start.insert(moves.paths[init.path].local.into_raw().into_u32() as usize);
        }
        // A block with no terminator yet leads nowhere.
        let ends = |from: BasicBlockId, state: &mut [u64]| {
            if body.basic_blocks[from].terminator.is_none() {
                state.fill(0);
            }
        };
        let uninit_entry =
            forward(cfg, &uninit, body.start_block, &uninit_start, |from, to, state| {
                if returns_to[index(from)] == Some(to) {
                    returned_uninit.apply(index(from), state);
                }
                ends(from, state);
            });
        let ever_entry = forward(cfg, &ever, body.start_block, &ever_start, |from, to, state| {
            if returns_to[index(from)] == Some(to) {
                returned_ever.apply(index(from), state);
            }
            ends(from, state);
        });
        InitFlow { uninit_entry, ever_entry }
    }
}

/// The effect of the statement or terminator at `location` on what may be uninitialized and
/// what has ever been initialized, the return of a call aside.
pub(super) fn apply(
    body: &MirBody<'_>,
    moves: &MoveData,
    cfg: &Cfg,
    location: Location,
    uninit: &mut BitSet,
    ever: &mut BitSet,
) {
    let point = cfg.point(location);
    for &index in moves.moves_at(point) {
        moves.with_descendants(moves.moves[index as usize].path, &mut |it| {
            uninit.insert(it);
        });
    }
    for &index in moves.inits_at(point) {
        let init = moves.inits[index as usize];
        if init.kind == InitKind::Deep {
            moves.with_descendants(init.path, &mut |it| uninit.remove(it));
            if moves.paths[init.path].projection.is_empty() {
                ever.insert(moves.paths[init.path].local.into_raw().into_u32() as usize);
            }
        }
    }
    let data = &body.basic_blocks[location.block];
    if let Some(statement) = data.statements.get(location.statement)
        && let StatementKind::StorageDead(local) = statement.kind
    {
        ever.remove(local.into_raw().into_u32() as usize);
    }
}

/// What the call at `location` returning does: its destination is initialized.
fn call_returned(
    moves: &MoveData,
    cfg: &Cfg,
    location: Location,
    uninit: &mut BitSet,
    ever: &mut BitSet,
) {
    for &index in moves.inits_at(cfg.point(location)) {
        let init = moves.inits[index as usize];
        if init.kind == InitKind::NonPanicPathOnly {
            moves.with_descendants(init.path, &mut |it| uninit.remove(it));
            if moves.paths[init.path].projection.is_empty() {
                ever.insert(moves.paths[init.path].local.into_raw().into_u32() as usize);
            }
        }
    }
}
