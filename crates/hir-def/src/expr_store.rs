//! Defines `ExpressionStore`: a lowered representation of functions, statics and
//! consts.
pub mod body;
mod expander;
pub mod lower;
pub mod path;
pub mod persist;
pub mod pretty;
pub mod scope;
mod source_map;
#[cfg(test)]
mod tests;

use std::{borrow::Borrow, cell::RefCell, mem, ops::Index, sync::LazyLock};

use base_db::SourceDatabase;
use blaze_ids::radix;
use cfg::CfgExpr;
use either::Either;
use hir_expand::{InFile, MacroCallId, mod_path::ModPath, name::Name};
use la_arena::Arena;
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use span::{Edition, SyntaxContext};
use syntax::{AstPtr, SyntaxNodePtr, ast};
use thin_vec::ThinVec;

use crate::{
    AdtId, BlockId, ExpressionStoreOwnerId, GenericDefId,
    expr_store::path::{AssociatedTypeBinding, GenericArg, GenericArgs, NormalPath, Path},
    hir::{
        Array, AsmOperand, Binding, BindingId, Expr, ExprId, ExprOrPatId, ExprOrPatIdPacked,
        InlineAsm, Label, LabelId, Literal, MatchArm, OffsetOf, Pat, PatId, RecordFieldPat,
        RecordLitField, RecordSpread, Statement,
        columns::{self, Frozen, Growing, HirColumns},
    },
    nameres::{DefMap, block_def_map},
    signatures::VariantFields,
    type_ref::{
        ArrayType, ConstRef, FnType, LifetimeRef, LifetimeRefId, PathId, RefType, TypeBound,
        TypeColumns, TypeRef, TypeRefId, UseArgRef,
    },
};

use self::source_map::{
    BackPushes, BindingPats, ExpressionOnlySourceMap, FormatTemplate, FreezeScratch, IdTable,
    PendingTemplate, SourceTable,
};

pub(crate) use self::source_map::Move;

pub use self::body::{Body, BodySourceMap};
pub use self::lower::{
    hir_assoc_type_binding_to_ast, hir_generic_arg_to_ast, hir_segment_to_ast_segment,
};
pub use self::source_map::ExpressionStoreSourceMap;

/// A wrapper around [`span::SyntaxContext`] that is intended only for comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HygieneId(span::SyntaxContext);

impl HygieneId {
    // The edition doesn't matter here, we only use this for comparisons and to lookup the macro.
    pub const ROOT: Self = Self(span::SyntaxContext::root(Edition::Edition2015));

    pub fn new(mut ctx: span::SyntaxContext) -> Self {
        // See `Name` for why we're doing that.
        ctx.remove_root_edition();
        Self(ctx)
    }

    pub(crate) fn syntax_context(self) -> SyntaxContext {
        self.0
    }

    pub(crate) fn is_root(self) -> bool {
        self.0.is_root()
    }
}

impl crate::hir::persist::Code for HygieneId {
    fn put(&self, w: &mut crate::hir::persist::Words) {
        self.0.put(w);
    }

    fn get(r: &mut crate::hir::persist::Read<'_>) -> Result<Self, blaze_format::FormatError> {
        r.get().map(HygieneId)
    }
}

pub type ExprPtr = AstPtr<ast::Expr>;
pub type ExprSource = InFile<ExprPtr>;

pub type PatPtr = AstPtr<ast::Pat>;
pub type PatSource = InFile<PatPtr>;

/// BlockExpr -> Desugared label from try block
pub type LabelPtr = AstPtr<Either<ast::Label, ast::BlockExpr>>;
pub type LabelSource = InFile<LabelPtr>;

pub type FieldPtr = AstPtr<ast::RecordExprField>;
pub type FieldSource = InFile<FieldPtr>;

pub type PatFieldPtr = AstPtr<Either<ast::RecordExprField, ast::RecordPatField>>;
pub type PatFieldSource = InFile<PatFieldPtr>;

pub type ExprOrPatPtr = AstPtr<Either<ast::Expr, ast::Pat>>;
pub type ExprOrPatSource = InFile<ExprOrPatPtr>;

pub type SelfParamPtr = AstPtr<ast::SelfParam>;
pub type MacroCallPtr = AstPtr<ast::MacroCall>;

pub type TypePtr = AstPtr<ast::Type>;
pub type TypeSource = InFile<TypePtr>;

pub type LifetimePtr = AstPtr<ast::Lifetime>;
pub type LifetimeSource = InFile<LifetimePtr>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExprRoot {
    root: ExprId,
    // We store, for each root, the range of exprs (and pats and bindings) it holds.
    // We store only the end (exclusive), since the start can be inferred from the previous
    // roots or is zero.
    exprs_end: ExprId,
    pats_end: PatId,
    bindings_end: BindingId,
}

// We split the store into types-only and expressions, because most stores (e.g. generics)
// don't store any expressions and this saves memory. Same thing for the source map.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpressionOnlyStore {
    /// The expressions and patterns.
    columns: HirColumns<Frozen>,
    bindings: Arena<Binding>,
    labels: Arena<Label>,
    /// The closure/coroutine/anon const that owns each binding, by binding: [`NO_OWNER`] for a
    /// binding the top level expression owns. Empty when it owns them all.
    binding_owners: Box<[u32]>,
    /// Block expressions in this store that may contain inner items.
    block_scopes: Box<[BlockId]>,

    /// The hygiene of variable usages whose hygiene is not the root's, by expression or pattern
    /// in increasing order.
    ///
    /// Expressions (and destructuring patterns) that can be recorded here are single segment path, although not all single segments path refer
    /// to variables and have hygiene (some refer to items, we don't know at this stage).
    ident_hygiene: Box<[(ExprOrPatIdPacked, HygieneId)]>,

    /// The edition of each block that a macro wrote, by block in increasing order: it is the
    /// macro's, not the one of the crate that uses the macro.
    block_editions: Box<[(ExprId, Edition)]>,

    /// The closures written with `#[rustc_inherit_overflow_checks]`, in increasing order.
    overflow_inheriting: Box<[ExprId]>,

    /// Maps expression roots to their origin.
    ///
    /// Note: while every root expr is an inference root (aka. an `AnonConst`), there could be other roots that do not appear here.
    /// This can happen when anon consts are nested, for example:
    ///
    /// ```
    /// [
    ///     ();
    ///     {
    ///         // this repeat expr is anon const #1, and *only it* appears in this list.
    ///         [
    ///             ();
    ///             {
    ///                 // this repeat expr is anon const #2.
    ///                 0
    ///             }
    ///         ];
    ///         0
    ///     }
    /// ]
    /// ```
    /// We do this because this allows us to search this list using a binary search,
    /// and it does not bother us because we use this list for two things: constructing `ExprScopes`, which
    /// works fine with nested exprs, and retrieving inference results, and we copy the inner const's inference
    /// into the outer const.
    expr_roots: SmallVec<[ExprRoot; 1]>,
}

/// The owner in [`ExpressionOnlyStore::binding_owners`] of a binding the top level owns.
const NO_OWNER: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionStore {
    expr_only: Option<Box<ExpressionOnlyStore>>,
    /// The type refs ([`ExpressionStore::type_ref`]).
    pub(crate) types: TypeColumns<Frozen>,
    pub lifetimes: Arena<LifetimeRef>,
    /// The hygiene of the plain type paths a `macro` wrote, by type in increasing order.
    type_hygiene: Option<Box<[(TypeRefId, HygieneId)]>>,
}

/// The body of an item (function, const etc.) while it is lowered. Its columns, forward maps,
/// back maps and list stacks are a [`Scratch`] taken from this thread's pool by
/// [`ExpressionStoreBuilder::new`] and given back, emptied, by [`ExpressionStoreBuilder::finish`].
#[derive(Debug, Eq, PartialEq)]
pub struct ExpressionStoreBuilder {
    pub(crate) columns: HirColumns<Growing>,
    pub(crate) types: TypeColumns<Growing>,
    /// The stacks lowering collects a node's children on ([`ExpressionStoreBuilder::list_mark`]).
    lists: Lists,
    pub bindings: Arena<Binding>,
    pub labels: Arena<Label>,
    pub lifetimes: Arena<LifetimeRef>,
    pub binding_owners: FxHashMap<BindingId, ExprId>,
    block_scopes: Vec<BlockId>,
    ident_hygiene: FxHashMap<ExprOrPatIdPacked, HygieneId>,
    pub(crate) type_hygiene: Vec<(TypeRefId, HygieneId)>,
    block_editions: FxHashMap<ExprId, Edition>,

    /// The closures written with `#[rustc_inherit_overflow_checks]`.
    overflow_inheriting: FxHashSet<ExprId>,
    inference_roots: Option<SmallVec<[ExprRoot; 1]>>,

    // AST expressions can create patterns in destructuring assignments. Therefore, `ExprSource` can also map
    // to `PatId`, and `PatId` can also map to `ExprSource` (the other way around is unaffected).
    // The forward maps are pushed to in lowering order and never read while lowering; `finish`
    // sorts each into a `SourceTable`, a later push for a pointer replacing an earlier one. The
    // back maps are pushed to in lowering order too, and `finish` makes each the dense columns of
    // a `BackMap`, a later push for an id replacing an earlier one.
    expr_map: Vec<(ExprSource, ExprOrPatIdPacked)>,
    expr_map_back: BackPushes<Expr<'static>, Either<ast::Expr, ast::Pat>>,

    pat_map: Vec<(PatSource, ExprOrPatIdPacked)>,
    pat_map_back: BackPushes<Pat<'static>, Either<ast::Expr, ast::Pat>>,

    label_map: Vec<(LabelSource, LabelId)>,
    label_map_back: BackPushes<Label, Either<ast::Label, ast::BlockExpr>>,

    types_map_back: BackPushes<TypeRef<'static>, ast::Type>,
    types_map: Vec<(TypeSource, TypeRefId)>,

    lifetime_map_back: BackPushes<LifetimeRef, ast::Lifetime>,
    lifetime_map: Vec<(LifetimeSource, LifetimeRefId)>,

    /// Each binding and a pattern that defines it, in lowering order: `finish` makes them CSR.
    binding_definitions: Vec<(BindingId, PatId)>,

    /// We don't create explicit nodes for record fields (`S { record_field: 92 }`).
    /// Instead, we use id of expression (`92`) to identify the field.
    field_map_back: Vec<(ExprId, FieldSource)>,
    pat_field_map_back: Vec<(PatId, PatFieldSource)>,

    template_map: Option<Box<PendingTemplate>>,

    expansions: Vec<(InFile<MacroCallPtr>, MacroCallId)>,

    /// Diagnostics accumulated during lowering. These contain `AstPtr`s and so are stored in
    /// the source map (since they're just as volatile).
    //
    // We store diagnostics on the `ExpressionOnlySourceMap` because diagnostics are rare (except
    // maybe for cfgs, and they are also not common in type places).
    pub(crate) diagnostics: Vec<ExpressionStoreDiagnostics>,

    /// What `finish` builds the back maps in, empty until then.
    freeze: FreezeScratch,

    /// Whether lowering records the source map. Without it lowering pushes to none of the maps
    /// above, and `finish` gives an empty source map: the store is the same either way, as
    /// lowering reads the maps only to fill them.
    record: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum MissingBodyItemKind {
    AssocConst,
    AssocType,
    Const,
    Static,
    TypeAlias,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ExpressionStoreDiagnostics {
    InactiveCode { node: InFile<SyntaxNodePtr>, cfg: CfgExpr },
    UnresolvedMacroCall { node: InFile<MacroCallPtr>, path: ModPath },
    UnreachableLabel { node: InFile<AstPtr<ast::Lifetime>>, name: Name },
    AwaitOutsideOfAsync { node: InFile<AstPtr<ast::AwaitExpr>>, location: String },
    UndeclaredLabel { node: InFile<AstPtr<ast::Lifetime>>, name: Name },
    PatternArgInExternFn { node: InFile<AstPtr<ast::Pat>> },
    FruInDestructuringAssignment { node: InFile<AstPtr<ast::Expr>> },
    MissingBody { node: InFile<SyntaxNodePtr>, kind: MissingBodyItemKind },
}

/// The stacks lowering collects a node's children on before it allocates the node, one per
/// element type of a list. They are used in stack order: a nested node's children go above its
/// parent's and are truncated away before the parent is allocated, so no list needs a `Vec`,
/// `SmallVec` or `Box<[_]>` of its own.
///
/// To allocate a node from the lists above a mark, lowering moves the stacks out of the builder
/// ([`ExpressionStoreBuilder::take_lists`], a move of a few `Vec` headers), builds the view from
/// `lists.exprs_since(mark)` and the like, allocates it, and puts them back truncated to the mark
/// ([`ExpressionStoreBuilder::put_lists`]): borrowing them in place would hold the builder
/// shared while it allocates.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Lists {
    exprs: Vec<ExprId>,
    pats: Vec<PatId>,
    types: Vec<TypeRefId>,
    opt_types: Vec<Option<TypeRefId>>,
    statements: Vec<Statement>,
    arms: Vec<MatchArm>,
    record_fields: Vec<RecordLitField>,
    field_pats: Vec<RecordFieldPat>,
    bounds: Vec<TypeBound>,
}

/// How high each list stack stood: [`ExpressionStoreBuilder::list_mark`]. Public because it is
/// in the type of `ExprCollector::impl_trait_error_allocator`, which other crates pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListMark {
    exprs: u32,
    pats: u32,
    types: u32,
    opt_types: u32,
    statements: u32,
    arms: u32,
    record_fields: u32,
    field_pats: u32,
    bounds: u32,
}

// Each `*_since` is the elements of one stack pushed since `mark`. It panics when the stack is
// below the mark: lowering truncated past a mark it did not own.
impl Lists {
    #[inline]
    pub(crate) fn exprs_since(&self, mark: ListMark) -> &[ExprId] {
        &self.exprs[mark.exprs as usize..]
    }

    /// The expressions pushed since `mark`, to reorder or rewrite in place.
    #[inline]
    pub(crate) fn exprs_since_mut(&mut self, mark: ListMark) -> &mut [ExprId] {
        &mut self.exprs[mark.exprs as usize..]
    }

    #[inline]
    pub(crate) fn pats_since(&self, mark: ListMark) -> &[PatId] {
        &self.pats[mark.pats as usize..]
    }

    #[inline]
    pub(crate) fn pats_since_mut(&mut self, mark: ListMark) -> &mut [PatId] {
        &mut self.pats[mark.pats as usize..]
    }

    /// The elements of a `TypeRef::Tuple`.
    #[inline]
    pub(crate) fn types_since(&self, mark: ListMark) -> &[TypeRefId] {
        &self.types[mark.types as usize..]
    }

    /// A closure's argument types.
    #[inline]
    pub(crate) fn opt_types_since(&self, mark: ListMark) -> &[Option<TypeRefId>] {
        &self.opt_types[mark.opt_types as usize..]
    }

    #[inline]
    pub(crate) fn statements_since(&self, mark: ListMark) -> &[Statement] {
        &self.statements[mark.statements as usize..]
    }

    #[inline]
    pub(crate) fn arms_since(&self, mark: ListMark) -> &[MatchArm] {
        &self.arms[mark.arms as usize..]
    }

    #[inline]
    pub(crate) fn record_fields_since(&self, mark: ListMark) -> &[RecordLitField] {
        &self.record_fields[mark.record_fields as usize..]
    }

    #[inline]
    pub(crate) fn field_pats_since(&self, mark: ListMark) -> &[RecordFieldPat] {
        &self.field_pats[mark.field_pats as usize..]
    }

    /// The bounds of a `TypeRef::ImplTrait` or `DynTrait`.
    #[inline]
    pub(crate) fn bounds_since(&self, mark: ListMark) -> &[TypeBound] {
        &self.bounds[mark.bounds as usize..]
    }

    fn mark(&self) -> ListMark {
        ListMark {
            exprs: columns::len(self.exprs.len()),
            pats: columns::len(self.pats.len()),
            types: columns::len(self.types.len()),
            opt_types: columns::len(self.opt_types.len()),
            statements: columns::len(self.statements.len()),
            arms: columns::len(self.arms.len()),
            record_fields: columns::len(self.record_fields.len()),
            field_pats: columns::len(self.field_pats.len()),
            bounds: columns::len(self.bounds.len()),
        }
    }

    fn truncate(&mut self, mark: ListMark) {
        self.exprs.truncate(mark.exprs as usize);
        self.pats.truncate(mark.pats as usize);
        self.types.truncate(mark.types as usize);
        self.opt_types.truncate(mark.opt_types as usize);
        self.statements.truncate(mark.statements as usize);
        self.arms.truncate(mark.arms as usize);
        self.record_fields.truncate(mark.record_fields as usize);
        self.field_pats.truncate(mark.field_pats as usize);
        self.bounds.truncate(mark.bounds as usize);
    }

    fn clear(&mut self) {
        let Lists {
            exprs,
            pats,
            types,
            opt_types,
            statements,
            arms,
            record_fields,
            field_pats,
            bounds,
        } = self;
        exprs.clear();
        pats.clear();
        types.clear();
        opt_types.clear();
        statements.clear();
        arms.clear();
        record_fields.clear();
        field_pats.clear();
        bounds.clear();
    }

    /// The first stack that is not empty, by name: a builder finishes with every one empty, or
    /// the ids left on it would land in the next store's lists.
    fn stacked(&self) -> Option<&'static str> {
        let Lists {
            exprs,
            pats,
            types,
            opt_types,
            statements,
            arms,
            record_fields,
            field_pats,
            bounds,
        } = self;
        [
            (exprs.is_empty(), "exprs"),
            (pats.is_empty(), "pats"),
            (types.is_empty(), "types"),
            (opt_types.is_empty(), "opt_types"),
            (statements.is_empty(), "statements"),
            (arms.is_empty(), "arms"),
            (record_fields.is_empty(), "record_fields"),
            (field_pats.is_empty(), "field_pats"),
            (bounds.is_empty(), "bounds"),
        ]
        .into_iter()
        .find_map(|(empty, name)| (!empty).then_some(name))
    }

    fn capacity(&self) -> usize {
        self.exprs.capacity()
            + self.pats.capacity()
            + self.types.capacity()
            + self.opt_types.capacity()
            + self.statements.capacity()
            + self.arms.capacity()
            + self.record_fields.capacity()
            + self.field_pats.capacity()
            + self.bounds.capacity()
    }
}

/// What a builder grows while it lowers a store, kept per thread between stores
/// ([`Scratch::take`], [`Scratch::give`]): a thread's scratch grows to its largest body once,
/// and after that lowering a body allocates nothing per node, only the exact columns of the
/// finished store.
#[derive(Default)]
struct Scratch {
    columns: HirColumns<Growing>,
    types: TypeColumns<Growing>,
    lists: Lists,
    expr_map: Vec<(ExprSource, ExprOrPatIdPacked)>,
    pat_map: Vec<(PatSource, ExprOrPatIdPacked)>,
    label_map: Vec<(LabelSource, LabelId)>,
    types_map: Vec<(TypeSource, TypeRefId)>,
    lifetime_map: Vec<(LifetimeSource, LifetimeRefId)>,
    expansions: Vec<(InFile<MacroCallPtr>, MacroCallId)>,
    expr_map_back: BackPushes<Expr<'static>, Either<ast::Expr, ast::Pat>>,
    pat_map_back: BackPushes<Pat<'static>, Either<ast::Expr, ast::Pat>>,
    label_map_back: BackPushes<Label, Either<ast::Label, ast::BlockExpr>>,
    types_map_back: BackPushes<TypeRef<'static>, ast::Type>,
    lifetime_map_back: BackPushes<LifetimeRef, ast::Lifetime>,
    binding_definitions: Vec<(BindingId, PatId)>,
    /// What [`ExpressionStoreBuilder::finish`] builds the back maps in.
    freeze: FreezeScratch,
}

/// The scratch sets a thread keeps, and the elements above which a set is dropped instead of
/// kept, so that one huge body doesn't pin its memory.
const POOL_SETS: usize = 4;
const POOL_ELEMENTS: usize = 1 << 20;

thread_local! {
    /// This thread's spare scratch, a stack: lowering one store can run a query that lowers
    /// another on the same thread, which takes the next set (or a fresh one) and gives it back
    /// before the outer one finishes. Only this thread touches it: no lock, no shared write.
    static POOL: RefCell<Vec<Scratch>> = const { RefCell::new(Vec::new()) };
}

impl Scratch {
    fn take() -> Scratch {
        POOL.try_with(|pool| pool.borrow_mut().pop()).ok().flatten().unwrap_or_default()
    }

    /// Empties `self` and keeps it for the next builder on this thread, unless the pool is
    /// full, `self` is over [`POOL_ELEMENTS`] or the thread is exiting.
    fn give(mut self) {
        self.clear();
        if self.capacity() > POOL_ELEMENTS {
            return;
        }
        let _exiting = POOL.try_with(move |pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < POOL_SETS {
                pool.push(self);
            }
        });
    }

    fn clear(&mut self) {
        let Scratch {
            columns,
            types,
            lists,
            expr_map,
            pat_map,
            label_map,
            types_map,
            lifetime_map,
            expansions,
            expr_map_back,
            pat_map_back,
            label_map_back,
            types_map_back,
            lifetime_map_back,
            binding_definitions,
            freeze,
        } = self;
        columns.clear();
        types.clear();
        lists.clear();
        expr_map.clear();
        pat_map.clear();
        label_map.clear();
        types_map.clear();
        lifetime_map.clear();
        expansions.clear();
        expr_map_back.clear();
        pat_map_back.clear();
        label_map_back.clear();
        types_map_back.clear();
        lifetime_map_back.clear();
        binding_definitions.clear();
        freeze.clear();
    }

    fn capacity(&self) -> usize {
        self.columns.capacity()
            + self.types.capacity()
            + self.lists.capacity()
            + self.expr_map.capacity()
            + self.pat_map.capacity()
            + self.label_map.capacity()
            + self.types_map.capacity()
            + self.lifetime_map.capacity()
            + self.expansions.capacity()
            + self.expr_map_back.capacity()
            + self.pat_map_back.capacity()
            + self.label_map_back.capacity()
            + self.types_map_back.capacity()
            + self.lifetime_map_back.capacity()
            + self.binding_definitions.capacity()
            + self.freeze.capacity()
    }
}

impl ExpressionStoreBuilder {
    /// A builder on this thread's pooled scratch.
    pub(crate) fn new() -> Self {
        let Scratch {
            columns,
            types,
            lists,
            expr_map,
            pat_map,
            label_map,
            types_map,
            lifetime_map,
            expansions,
            expr_map_back,
            pat_map_back,
            label_map_back,
            types_map_back,
            lifetime_map_back,
            binding_definitions,
            freeze,
        } = Scratch::take();
        ExpressionStoreBuilder {
            columns,
            types,
            lists,
            bindings: Arena::new(),
            labels: Arena::new(),
            lifetimes: Arena::new(),
            binding_owners: FxHashMap::default(),
            block_scopes: Vec::new(),
            ident_hygiene: FxHashMap::default(),
            type_hygiene: Vec::new(),
            block_editions: FxHashMap::default(),
            overflow_inheriting: FxHashSet::default(),
            inference_roots: None,
            expr_map,
            expr_map_back,
            pat_map,
            pat_map_back,
            label_map,
            label_map_back,
            types_map_back,
            types_map,
            lifetime_map_back,
            lifetime_map,
            binding_definitions,
            field_map_back: Vec::new(),
            pat_field_map_back: Vec::new(),
            template_map: None,
            expansions,
            diagnostics: Vec::new(),
            freeze,
            record: true,
        }
    }

    /// Lowers without recording the source map (see `record`).
    pub(crate) fn skip_source_map(&mut self) {
        self.record = false;
    }

    #[inline]
    pub(crate) fn alloc_type_ref(&mut self, type_ref: TypeRef<'_>) -> TypeRefId {
        self.types.push(type_ref)
    }

    /// A `TypeRef::Path`, `path` moved into its table where [`Self::alloc_type_ref`] clones it.
    #[inline]
    pub(crate) fn alloc_path_type(&mut self, path: Path) -> TypeRefId {
        self.types.push_path(path)
    }

    /// A `TypeRef::Fn`, `fn_type` moved into its table.
    #[inline]
    pub(crate) fn alloc_fn_type(&mut self, fn_type: FnType) -> TypeRefId {
        self.types.push_fn(fn_type)
    }

    #[inline]
    pub(crate) fn type_ref(&self, id: TypeRefId) -> TypeRef<'_> {
        self.types.get(id)
    }

    #[inline]
    pub(crate) fn type_ref_count(&self) -> usize {
        self.types.len()
    }

    #[inline]
    pub(crate) fn list_mark(&self) -> ListMark {
        self.lists.mark()
    }

    #[inline]
    pub(crate) fn push_list_expr(&mut self, expr: ExprId) {
        self.lists.exprs.push(expr);
    }

    #[inline]
    pub(crate) fn push_list_pat(&mut self, pat: PatId) {
        self.lists.pats.push(pat);
    }

    #[inline]
    pub(crate) fn push_list_type(&mut self, type_ref: TypeRefId) {
        self.lists.types.push(type_ref);
    }

    #[inline]
    pub(crate) fn push_list_opt_type(&mut self, type_ref: Option<TypeRefId>) {
        self.lists.opt_types.push(type_ref);
    }

    #[inline]
    pub(crate) fn push_list_statement(&mut self, statement: Statement) {
        self.lists.statements.push(statement);
    }

    #[inline]
    pub(crate) fn push_list_arm(&mut self, arm: MatchArm) {
        self.lists.arms.push(arm);
    }

    #[inline]
    pub(crate) fn push_list_record_field(&mut self, field: RecordLitField) {
        self.lists.record_fields.push(field);
    }

    #[inline]
    pub(crate) fn push_list_field_pat(&mut self, field: RecordFieldPat) {
        self.lists.field_pats.push(field);
    }

    #[inline]
    pub(crate) fn push_list_bound(&mut self, bound: TypeBound) {
        self.lists.bounds.push(bound);
    }

    // For code that only reads its lists; to allocate a node from them, see `take_lists`.

    #[inline]
    pub(crate) fn exprs_since(&self, mark: ListMark) -> &[ExprId] {
        self.lists.exprs_since(mark)
    }

    #[inline]
    pub(crate) fn pats_since(&self, mark: ListMark) -> &[PatId] {
        self.lists.pats_since(mark)
    }

    #[inline]
    pub(crate) fn types_since(&self, mark: ListMark) -> &[TypeRefId] {
        self.lists.types_since(mark)
    }

    /// The list stacks, moved out so a node can be allocated from them ([`Lists`]). The builder
    /// holds empty stacks (no allocation) until [`Self::put_lists`] returns them; what is pushed
    /// onto the builder meanwhile is lost.
    #[inline]
    pub(crate) fn take_lists(&mut self) -> Lists {
        mem::take(&mut self.lists)
    }

    /// Puts back the stacks [`Self::take_lists`] took, truncated to `mark`.
    #[inline]
    pub(crate) fn put_lists(&mut self, mut lists: Lists, mark: ListMark) {
        debug_assert_eq!(self.lists.stacked(), None, "pushed onto this stack while taken");
        lists.truncate(mark);
        self.lists = lists;
    }

    /// The last statement pushed since `mark`, taken off the stack; `None` when there is none
    /// above `mark`.
    #[inline]
    pub(crate) fn pop_list_statement(&mut self, mark: ListMark) -> Option<Statement> {
        match self.lists.statements.len() > mark.statements as usize {
            true => self.lists.statements.pop(),
            false => None,
        }
    }

    /// The bounds pushed since `mark`, moved off the stack.
    #[inline]
    pub(crate) fn drain_bounds(&mut self, mark: ListMark) -> std::vec::Drain<'_, TypeBound> {
        self.lists.bounds.drain(mark.bounds as usize..)
    }

    /// Pops every stack back to `mark`.
    #[inline]
    pub(crate) fn truncate_to(&mut self, mark: ListMark) {
        self.lists.truncate(mark);
    }

    // The by-value allocations below move their cold values (paths, names, literals, generic
    // arguments) into the store where building the view would clone them. Their lists are
    // copied into the store's arrays, as `alloc_expr` copies a view's.

    #[inline]
    pub(crate) fn alloc_path_expr(&mut self, path: Path) -> ExprId {
        self.columns.push_path_expr(path)
    }

    #[inline]
    pub(crate) fn alloc_path_pat(&mut self, path: Path) -> PatId {
        self.columns.push_path_pat(path)
    }

    #[inline]
    pub(crate) fn alloc_literal_expr(&mut self, literal: Literal) -> ExprId {
        self.columns.push_literal_expr(literal)
    }

    #[inline]
    pub(crate) fn alloc_offset_of_expr(&mut self, offset_of: OffsetOf) -> ExprId {
        self.columns.push_offset_of_expr(offset_of)
    }

    #[inline]
    pub(crate) fn alloc_asm_expr(&mut self, asm: InlineAsm) -> ExprId {
        self.columns.push_asm_expr(asm)
    }

    #[inline]
    pub(crate) fn alloc_method_call(
        &mut self,
        receiver: ExprId,
        method_name: Name,
        args: &[ExprId],
        generic_args: Option<GenericArgs>,
    ) -> ExprId {
        self.columns.push_method_call(receiver, method_name, args, generic_args)
    }

    #[inline]
    pub(crate) fn alloc_record_lit(
        &mut self,
        path: Path,
        fields: &[RecordLitField],
        spread: RecordSpread,
    ) -> ExprId {
        self.columns.push_record_lit(path, fields.iter().cloned(), spread)
    }

    #[inline]
    pub(crate) fn alloc_record_pat(
        &mut self,
        path: Path,
        args: &[RecordFieldPat],
        ellipsis: bool,
    ) -> PatId {
        self.columns.push_record_pat(path, args.iter().cloned(), ellipsis)
    }

    #[inline]
    pub(crate) fn alloc_tuple_struct_pat(
        &mut self,
        path: Path,
        args: &[PatId],
        ellipsis: Option<u32>,
    ) -> PatId {
        self.columns.push_tuple_struct_pat(path, args, ellipsis)
    }

    /// Gives the block `block` the tail `tail`, in place. Panics when `block` is not a block.
    #[inline]
    pub(crate) fn set_block_tail(&mut self, block: ExprId, tail: ExprId) {
        self.columns.set_block_tail(block, tail)
    }

    #[inline]
    pub(crate) fn alloc_expr(&mut self, expr: Expr<'_>) -> ExprId {
        self.columns.push_expr(expr)
    }

    #[inline]
    pub(crate) fn alloc_pat(&mut self, pat: Pat<'_>) -> PatId {
        self.columns.push_pat(pat)
    }

    #[inline]
    pub(crate) fn expr(&self, id: ExprId) -> Expr<'_> {
        self.columns.expr(id)
    }

    #[inline]
    pub(crate) fn pat(&self, id: PatId) -> Pat<'_> {
        self.columns.pat(id)
    }

    /// Replaces an expression: read it with [`Self::expr`], build the changed view, set it.
    #[inline]
    pub(crate) fn set_expr(&mut self, id: ExprId, expr: Expr<'_>) {
        self.columns.set_expr(id, expr)
    }

    #[inline]
    pub(crate) fn set_pat(&mut self, id: PatId, pat: Pat<'_>) {
        self.columns.set_pat(id, pat)
    }

    #[inline]
    pub(crate) fn expr_count(&self) -> usize {
        self.columns.expr_count()
    }

    #[inline]
    pub(crate) fn pat_count(&self) -> usize {
        self.columns.pat_count()
    }

    /// The finished store and source map: each column, table and map copied out of the pooled
    /// scratch at its exact length (one allocation per non-empty one, no `shrink_to_fit`
    /// reallocation), the scratch given back to this thread's pool.
    pub fn finish(self) -> (ExpressionStore, ExpressionStoreSourceMap) {
        let Self {
            block_scopes,
            columns,
            types,
            lists,
            mut labels,
            mut bindings,
            binding_owners,
            ident_hygiene,
            type_hygiene,
            block_editions,
            overflow_inheriting,
            inference_roots: mut expr_roots,
            mut lifetimes,

            expr_map,
            expr_map_back,
            pat_map,
            pat_map_back,
            label_map,
            label_map_back,
            types_map_back,
            types_map,
            lifetime_map_back,
            lifetime_map,
            binding_definitions,
            field_map_back,
            pat_field_map_back,
            template_map,
            expansions,
            diagnostics,
            freeze,
            record,
        } = self;
        debug_assert_eq!(lists.stacked(), None, "lowering left a list on this stack");
        let mut scratch = Scratch {
            columns,
            types,
            lists,
            expr_map,
            pat_map,
            label_map,
            types_map,
            lifetime_map,
            expansions,
            expr_map_back,
            pat_map_back,
            label_map_back,
            types_map_back,
            lifetime_map_back,
            binding_definitions,
            freeze,
        };
        let columns = scratch.columns.freeze_into();
        let types = scratch.types.freeze_into();
        let has_exprs = !columns.is_empty() || !labels.is_empty() || !bindings.is_empty();
        let source_map = match record {
            false => ExpressionStoreSourceMap::default(),
            true => {
                // Each back map is dense over its arena, so a source map can be checked on its
                // own.
                let freeze = &mut scratch.freeze;
                let expr_map_back = scratch.expr_map_back.freeze(columns.expr_count(), freeze);
                let pat_map_back = scratch.pat_map_back.freeze(columns.pat_count(), freeze);
                let label_map_back = scratch.label_map_back.freeze(labels.len(), freeze);
                let types_map_back = scratch.types_map_back.freeze(types.len(), freeze);
                let lifetime_map_back = scratch.lifetime_map_back.freeze(lifetimes.len(), freeze);
                let binding_definitions =
                    BindingPats::from_pushed(&mut scratch.binding_definitions, bindings.len());
                let expr_map = SourceTable::from_pushed(&mut scratch.expr_map);
                let pat_map = SourceTable::from_pushed(&mut scratch.pat_map);
                let label_map = SourceTable::from_pushed(&mut scratch.label_map);
                let types_map = SourceTable::from_pushed(&mut scratch.types_map);
                let lifetime_map = SourceTable::from_pushed(&mut scratch.lifetime_map);
                let expansions = SourceTable::from_pushed(&mut scratch.expansions);
                let field_map_back = IdTable::from_pushed(field_map_back);
                let pat_field_map_back = IdTable::from_pushed(pat_field_map_back);
                let template_map = template_map.map(|template| {
                    let FormatTemplate {
                        format_args_to_captures,
                        asm_to_captures,
                        implicit_capture_to_source,
                    } = *template;
                    Box::new(FormatTemplate {
                        format_args_to_captures: IdTable::from_pushed(format_args_to_captures),
                        asm_to_captures: IdTable::from_pushed(asm_to_captures),
                        implicit_capture_to_source: IdTable::from_pushed(
                            implicit_capture_to_source,
                        ),
                    })
                });
                let expr_only = (has_exprs || !expansions.is_empty() || !diagnostics.is_empty())
                    .then(|| {
                        Box::new(ExpressionOnlySourceMap {
                            expr_map,
                            expr_map_back,
                            pat_map,
                            pat_map_back,
                            label_map,
                            label_map_back,
                            binding_definitions,
                            field_map_back,
                            pat_field_map_back,
                            template_map,
                            expansions,
                            diagnostics: ThinVec::from_iter(diagnostics),
                        })
                    });
                ExpressionStoreSourceMap {
                    expr_only,
                    types_map_back,
                    types_map,
                    lifetime_map_back,
                    lifetime_map,
                }
            }
        };
        scratch.give();

        labels.shrink_to_fit();
        bindings.shrink_to_fit();
        lifetimes.shrink_to_fit();

        let store = {
            let expr_only = if has_exprs {
                if let Some(expr_roots) = &mut expr_roots {
                    expr_roots.shrink_to_fit();
                }
                let binding_owners = match binding_owners.is_empty() {
                    true => Box::default(),
                    false => {
                        let mut column = vec![NO_OWNER; bindings.len()].into_boxed_slice();
                        for (binding, owner) in binding_owners {
                            column[binding.into_raw().into_u32() as usize] =
                                owner.into_raw().into_u32();
                        }
                        column
                    }
                };
                let mut ident_hygiene: Box<[_]> = ident_hygiene.into_iter().collect();
                ident_hygiene.sort_unstable_by_key(|&(id, _)| id);
                let mut block_editions: Box<[_]> = block_editions.into_iter().collect();
                radix::sort_copy_by_key(&mut block_editions, |&(id, _)| id.into_raw().into_u32());
                let mut overflow_inheriting: Box<[_]> = overflow_inheriting.into_iter().collect();
                radix::sort_copy_by_key(&mut overflow_inheriting, |it| it.into_raw().into_u32());
                Some(Box::new(ExpressionOnlyStore {
                    columns,
                    bindings,
                    labels,
                    binding_owners,
                    block_scopes: block_scopes.into_boxed_slice(),
                    ident_hygiene,
                    block_editions,
                    overflow_inheriting,
                    expr_roots: expr_roots
                        .expect("should always finish with a `Some(_)` expr_roots"),
                }))
            } else {
                None
            };
            let type_hygiene = (!type_hygiene.is_empty()).then(|| type_hygiene.into_boxed_slice());
            ExpressionStore { expr_only, types, lifetimes, type_hygiene }
        };

        (store, source_map)
    }
}

impl ExpressionStore {
    /// The store with nothing in it. A `static` rather than a `const`: boxed columns can't be
    /// made in a `const`. Empty columns allocate nothing.
    #[inline]
    pub fn empty() -> &'static ExpressionStore {
        static EMPTY: LazyLock<ExpressionStore> = LazyLock::new(|| ExpressionStore {
            expr_only: None,
            types: TypeColumns::default(),
            lifetimes: Arena::new(),
            type_hygiene: None,
        });
        &EMPTY
    }

    /// The type ref `id`, decoded from the store's columns.
    #[inline]
    pub fn type_ref(&self, id: TypeRefId) -> TypeRef<'_> {
        self.types.get(id)
    }

    #[inline]
    pub fn type_refs(&self) -> impl Iterator<Item = (TypeRefId, TypeRef<'_>)> {
        self.types.iter()
    }

    pub fn of(db: &dyn SourceDatabase, def: ExpressionStoreOwnerId) -> &ExpressionStore {
        match def {
            ExpressionStoreOwnerId::Signature(def) => {
                use crate::signatures::{
                    ConstSignature, EnumSignature, FunctionSignature, ImplSignature,
                    StaticSignature, StructSignature, TraitSignature, TypeAliasSignature,
                    UnionSignature,
                };
                match def {
                    GenericDefId::AdtId(AdtId::EnumId(id)) => &EnumSignature::of(db, id).store,
                    GenericDefId::AdtId(AdtId::StructId(id)) => &StructSignature::of(db, id).store,
                    GenericDefId::AdtId(AdtId::UnionId(id)) => &UnionSignature::of(db, id).store,
                    GenericDefId::ConstId(id) => &ConstSignature::of(db, id).store,
                    GenericDefId::FunctionId(id) => &FunctionSignature::of(db, id).store,
                    GenericDefId::ImplId(id) => &ImplSignature::of(db, id).store,
                    GenericDefId::StaticId(id) => &StaticSignature::of(db, id).store,
                    GenericDefId::TraitId(id) => &TraitSignature::of(db, id).store,
                    GenericDefId::TypeAliasId(id) => &TypeAliasSignature::of(db, id).store,
                }
            }
            ExpressionStoreOwnerId::Body(body) => &Body::of(db, body).store,
            ExpressionStoreOwnerId::VariantFields(variant_id) => {
                &VariantFields::of(db, variant_id).store
            }
        }
    }

    pub fn with_source_map(
        db: &dyn SourceDatabase,
        def: ExpressionStoreOwnerId,
    ) -> (&ExpressionStore, &ExpressionStoreSourceMap) {
        match def {
            ExpressionStoreOwnerId::Signature(def) => {
                use crate::signatures::{
                    ConstSignature, EnumSignature, FunctionSignature, ImplSignature,
                    StaticSignature, StructSignature, TraitSignature, TypeAliasSignature,
                    UnionSignature,
                };
                match def {
                    GenericDefId::AdtId(AdtId::EnumId(id)) => {
                        let sig = EnumSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::AdtId(AdtId::StructId(id)) => {
                        let sig = StructSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::AdtId(AdtId::UnionId(id)) => {
                        let sig = UnionSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::ConstId(id) => {
                        let sig = ConstSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::FunctionId(id) => {
                        let sig = FunctionSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::ImplId(id) => {
                        let sig = ImplSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::StaticId(id) => {
                        let sig = StaticSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::TraitId(id) => {
                        let sig = TraitSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                    GenericDefId::TypeAliasId(id) => {
                        let sig = TypeAliasSignature::with_source_map(db, id);
                        (&sig.0.store, &sig.1)
                    }
                }
            }
            ExpressionStoreOwnerId::Body(body) => {
                let (store, sm) = Body::with_source_map(db, body);
                (&store.store, &sm.store)
            }
            ExpressionStoreOwnerId::VariantFields(variant_id) => {
                let (store, sm) = VariantFields::with_source_map(db, variant_id);
                (&store.store, sm)
            }
        }
    }

    /// Returns all expression root `ExprId`s found in this store.
    pub fn expr_roots(&self) -> impl DoubleEndedIterator<Item = ExprId> {
        self.expr_only
            .as_ref()
            .map_or(&[][..], |expr_only| &expr_only.expr_roots)
            .iter()
            .map(|root| root.root)
    }

    fn find_root_for(
        &self,
        mut get: impl FnMut(&ExprRoot) -> la_arena::RawIdx,
        find: la_arena::RawIdx,
    ) -> ExprId {
        let expr_only = self.assert_expr_only();
        let find = find.into_u32();
        let entry = expr_only.expr_roots.partition_point(|root| get(root).into_u32() <= find);
        expr_only.expr_roots[entry].root
    }

    pub fn find_root_for_expr(&self, expr: ExprId) -> ExprId {
        self.find_root_for(|root| root.exprs_end.into_raw(), expr.into_raw())
    }

    pub fn find_root_for_pat(&self, pat: PatId) -> ExprId {
        self.find_root_for(|root| root.pats_end.into_raw(), pat.into_raw())
    }

    pub fn find_root_for_binding(&self, binding: BindingId) -> ExprId {
        self.find_root_for(|root| root.bindings_end.into_raw(), binding.into_raw())
    }

    /// Returns an iterator over all block expressions in this store that define inner items.
    pub fn blocks<'a>(
        &'a self,
        db: &'a dyn SourceDatabase,
    ) -> impl Iterator<Item = (BlockId, &'a DefMap)> + 'a {
        self.expr_only
            .as_ref()
            .map(|it| &*it.block_scopes)
            .unwrap_or_default()
            .iter()
            .map(move |&block| (block, block_def_map(db, block)))
    }

    pub fn walk_bindings_in_pat(&self, pat_id: PatId, mut f: impl FnMut(BindingId)) {
        self.walk_pats(pat_id, &mut |pat| {
            if let Pat::Bind { id, .. } = self.pat(pat) {
                f(id);
            }
        });
    }

    pub fn visit_pat_children(&self, pat_id: PatId, mut visitor: impl StoreVisitor) {
        // Do not use `..` patterns or field accesses here, only destructuring, to ensure we cover all cases
        // (we've had multiple bugs with this in the past).
        match self.pat(pat_id) {
            Pat::Range { start, end, range_type: _ } => {
                visitor.on_expr_opt(start);
                visitor.on_expr_opt(end);
            }
            Pat::Lit(expr) | Pat::Expr(expr) => visitor.on_expr(expr),
            Pat::Path(path) => visitor.on_path(path),
            Pat::Wild | Pat::Missing | Pat::Rest | Pat::NotNull => {}
            Pat::Bind { subpat, id: _ } => visitor.on_pat_opt(subpat),
            Pat::Or(args) | Pat::Tuple { args, ellipsis: _ } => visitor.on_pats(args),
            Pat::TupleStruct { args, ellipsis: _, path } => {
                visitor.on_pats(args);
                visitor.on_path(path);
            }
            Pat::Ref { pat, mutability: _ } => visitor.on_pat(pat),
            Pat::Slice { prefix, slice, suffix } => {
                visitor.on_pats(prefix);
                visitor.on_pat_opt(slice);
                visitor.on_pats(suffix);
            }
            Pat::Record { args, ellipsis: _, path } => {
                args.iter().for_each(|RecordFieldPat { pat, name: _ }| visitor.on_pat(*pat));
                visitor.on_path(path);
            }
            Pat::Box { inner } | Pat::Deref { inner } => visitor.on_pat(inner),
        }
    }

    pub fn walk_pats_shallow(&self, pat_id: PatId, f: impl FnMut(PatId)) {
        return self.visit_pat_children(pat_id, Visitor(f));

        struct Visitor<F>(F);

        impl<F: FnMut(PatId)> StoreVisitor for Visitor<F> {
            fn on_pat(&mut self, pat: PatId) {
                (self.0)(pat);
            }
        }
    }

    pub fn walk_pats(&self, pat_id: PatId, f: &mut impl FnMut(PatId)) {
        f(pat_id);
        self.walk_pats_shallow(pat_id, |p| self.walk_pats(p, f));
    }

    #[inline]
    pub fn binding_owner(&self, id: BindingId) -> Option<ExprId> {
        let owners = &self.expr_only.as_ref()?.binding_owners;
        let owner = *owners.get(id.into_raw().into_u32() as usize)?;
        (owner != NO_OWNER).then(|| ExprId::from_raw(la_arena::RawIdx::from_u32(owner)))
    }

    pub fn visit_expr_children(&self, expr_id: ExprId, visitor: impl StoreVisitor) {
        // A walk of the expressions recurses through here, once for each that is in another:
        // rustc's 5,204-arm `else if`.
        stdx::ensure_sufficient_stack(|| self.visit_expr_children_here(expr_id, visitor))
    }

    fn visit_expr_children_here(&self, expr_id: ExprId, mut visitor: impl StoreVisitor) {
        // Do not use `..` patterns or field accesses here, only destructuring, to ensure we cover all cases
        // (we've had multiple bugs with this in the past).
        match self.expr(expr_id) {
            Expr::OffsetOf(OffsetOf { container, fields: _ }) => visitor.on_type(*container),
            Expr::Path(path) => visitor.on_path(path),
            Expr::Continue { label: _ } | Expr::Missing | Expr::Literal(_) | Expr::Underscore => {}
            Expr::InlineAsm(InlineAsm { operands, .. }) => {
                operands.iter().for_each(|(_, op)| match op {
                    AsmOperand::In { expr, reg: _ }
                    | AsmOperand::Out { expr: Some(expr), late: _, reg: _ }
                    | AsmOperand::InOut { expr, late: _, reg: _ }
                    | AsmOperand::Const(expr)
                    | AsmOperand::Label(expr) => visitor.on_expr(*expr),
                    AsmOperand::SplitInOut { in_expr, out_expr, late: _, reg: _ } => {
                        visitor.on_expr(*in_expr);
                        visitor.on_expr_opt(*out_expr);
                    }
                    AsmOperand::Out { expr: None, late: _, reg: _ } | AsmOperand::Sym(_) => (),
                })
            }
            Expr::If { condition, then_branch, else_branch } => {
                visitor.on_expr(condition);
                visitor.on_expr(then_branch);
                visitor.on_expr_opt(else_branch);
            }
            Expr::Let { expr, pat } => {
                visitor.on_pat(pat);
                visitor.on_expr(expr);
            }
            Expr::Block { statements, tail, id: _, label: _, unsafe_: _ } => {
                for stmt in statements {
                    match stmt {
                        Statement::Let { initializer, else_branch, pat, type_ref, is_super: _ } => {
                            visitor.on_expr_opt(*initializer);
                            visitor.on_expr_opt(*else_branch);
                            visitor.on_pat(*pat);
                            visitor.on_type_opt(*type_ref);
                        }
                        Statement::Expr { expr: expression, has_semi: _ } => {
                            visitor.on_expr(*expression)
                        }
                        Statement::Item(_) => (),
                    }
                }
                visitor.on_expr_opt(tail);
            }
            Expr::Loop { body, label: _, source: _ } => visitor.on_expr(body),
            Expr::Call { callee, args } => {
                visitor.on_expr(callee);
                visitor.on_exprs(args);
            }
            Expr::MethodCall { receiver, args, generic_args, method_name: _ } => {
                visitor.on_expr(receiver);
                visitor.on_exprs(args);
                visitor.on_generic_args_opt(&generic_args);
            }
            Expr::Match { expr, arms } => {
                visitor.on_expr(expr);
                arms.iter().for_each(|MatchArm { pat, guard, expr }| {
                    visitor.on_expr(*expr);
                    visitor.on_expr_opt(*guard);
                    visitor.on_pat(*pat);
                });
            }
            Expr::Break { expr, label: _ }
            | Expr::Return { expr }
            | Expr::Yield { expr }
            | Expr::Yeet { expr } => visitor.on_expr_opt(expr),
            Expr::Become { expr } => visitor.on_expr(expr),
            Expr::RecordLit { fields, spread, path } => {
                for RecordLitField { name: _, expr } in fields.iter() {
                    visitor.on_expr(*expr);
                }
                match spread {
                    RecordSpread::Expr(expr) => visitor.on_expr(expr),
                    RecordSpread::None | RecordSpread::FieldDefaults => {}
                }
                visitor.on_path(path);
            }
            Expr::Closure { body, args, arg_types, ret_type, capture_by: _, closure_kind: _ } => {
                visitor.on_expr(body);
                visitor.on_pats(args);
                arg_types.iter().for_each(|arg_type| visitor.on_type_opt(*arg_type));
                visitor.on_type_opt(ret_type);
            }
            Expr::BinaryOp { lhs, rhs, op: _ } => {
                visitor.on_expr(lhs);
                visitor.on_expr(rhs);
            }
            Expr::Index { base, index } => {
                visitor.on_expr(base);
                visitor.on_expr(index);
            }
            Expr::Cast { expr, type_ref } => {
                visitor.on_expr(expr);
                visitor.on_type(type_ref);
            }
            Expr::Field { expr, name: _ }
            | Expr::Await { expr }
            | Expr::Ref { expr, mutability: _, rawness: _ }
            | Expr::UnaryOp { expr, op: _ } => visitor.on_expr(expr),
            Expr::Const(expr) => visitor.on_anon_const_expr(expr),
            Expr::Tuple { exprs } => visitor.on_exprs(exprs),
            Expr::Array(a) => match a {
                Array::ElementList { elements } => visitor.on_exprs(elements),
                Array::Repeat { initializer, repeat } => {
                    visitor.on_expr(initializer);
                    visitor.on_anon_const_expr(repeat)
                }
            },
            Expr::Assignment { target, value } => {
                visitor.on_pat(target);
                visitor.on_expr(value);
            }
            Expr::IncludeBytes => {}
        }
    }

    /// Walks the immediate children expressions and calls `f` for each child expression.
    pub fn walk_child_exprs(&self, expr_id: ExprId, callback: impl FnMut(ExprId)) {
        return self.visit_expr_children(expr_id, Visitor { callback, store: self });

        struct Visitor<'a, F> {
            callback: F,
            store: &'a ExpressionStore,
        }

        impl<F: FnMut(ExprId)> StoreVisitor for Visitor<'_, F> {
            fn on_expr(&mut self, expr: ExprId) {
                (self.callback)(expr);
            }

            fn on_pat(&mut self, pat: PatId) {
                self.store.walk_exprs_in_pat(pat, &mut self.callback);
            }
        }
    }

    /// Walks the immediate children expressions and calls `f` for each child expression but does
    /// not walk expressions within patterns.
    pub fn walk_child_exprs_without_pats(&self, expr_id: ExprId, callback: impl FnMut(ExprId)) {
        return self.visit_expr_children(expr_id, Visitor { callback });

        struct Visitor<F> {
            callback: F,
        }

        impl<F: FnMut(ExprId)> StoreVisitor for Visitor<F> {
            fn on_expr(&mut self, expr: ExprId) {
                (self.callback)(expr);
            }
        }
    }

    pub fn walk_exprs_in_pat(&self, pat_id: PatId, callback: impl FnMut(ExprId)) {
        return Visitor { callback, store: self }.on_pat(pat_id);

        struct Visitor<'a, F> {
            callback: F,
            store: &'a ExpressionStore,
        }

        impl<F: FnMut(ExprId)> StoreVisitor for Visitor<'_, F> {
            fn on_expr(&mut self, expr: ExprId) {
                (self.callback)(expr);
            }

            fn on_pat(&mut self, pat: PatId) {
                self.store.visit_pat_children(pat, self);
            }
        }
    }

    pub fn visit_type_ref_children(&self, type_ref: TypeRefId, mut visitor: impl StoreVisitor) {
        match self.type_ref(type_ref) {
            TypeRef::Never | TypeRef::Placeholder | TypeRef::TypeParam(_) | TypeRef::Error => {}
            TypeRef::PatternType(ty, pat) => {
                visitor.on_type(ty);
                visitor.on_pat(pat)
            }
            TypeRef::Tuple(types) => visitor.on_types(types),
            TypeRef::Path(path) => visitor.on_path(path),
            TypeRef::RawPtr(inner, _) | TypeRef::Slice(inner) => visitor.on_type(inner),
            TypeRef::Reference(RefType { ty, lifetime, mutability: _ }) => {
                visitor.on_type(*ty);
                visitor.on_lifetime_opt(*lifetime);
            }
            TypeRef::Array(ArrayType { ty, len: ConstRef { expr: len } }) => {
                visitor.on_type(*ty);
                visitor.on_anon_const_expr(*len);
            }
            TypeRef::Fn(FnType { params, is_varargs: _, is_unsafe: _, abi: _, binder: _ }) => {
                params.iter().for_each(|(_, param_ty)| visitor.on_type(*param_ty));
            }
            TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
                visitor.on_type_bounds(bounds)
            }
        }
    }

    #[inline]
    #[track_caller]
    fn assert_expr_only(&self) -> &ExpressionOnlyStore {
        self.expr_only.as_ref().expect("should have `ExpressionStore::expr_only`")
    }

    fn binding_hygiene(&self, binding: BindingId) -> HygieneId {
        self.assert_expr_only().bindings[binding].hygiene
    }

    /// Whether a closure takes the overflow checks of the crate that compiles it, as a function
    /// with `#[rustc_inherit_overflow_checks]` does.
    pub fn inherits_overflow_checks(&self, closure: ExprId) -> bool {
        self.assert_expr_only().overflow_inheriting.binary_search(&closure).is_ok()
    }

    /// The edition a block was written in, when a macro wrote it.
    pub fn block_edition(&self, block: ExprId) -> Option<Edition> {
        let editions = &self.assert_expr_only().block_editions;
        let index = editions.binary_search_by_key(&block, |&(id, _)| id).ok()?;
        Some(editions[index].1)
    }

    pub fn type_path_hygiene(&self, type_ref: TypeRefId) -> HygieneId {
        let Some(hygiene) = &self.type_hygiene else { return HygieneId::ROOT };
        match hygiene.binary_search_by_key(&type_ref, |&(id, _)| id) {
            Ok(index) => hygiene[index].1,
            Err(_) => HygieneId::ROOT,
        }
    }

    fn ident_hygiene(&self, id: ExprOrPatIdPacked) -> HygieneId {
        let hygiene = &self.assert_expr_only().ident_hygiene;
        match hygiene.binary_search_by_key(&id, |&(id, _)| id) {
            Ok(index) => hygiene[index].1,
            Err(_) => HygieneId::ROOT,
        }
    }

    pub fn expr_path_hygiene(&self, expr: ExprId) -> HygieneId {
        self.ident_hygiene(expr.into())
    }

    pub fn pat_path_hygiene(&self, pat: PatId) -> HygieneId {
        self.ident_hygiene(pat.into())
    }

    pub fn expr_or_pat_path_hygiene(&self, id: ExprOrPatId) -> HygieneId {
        match id {
            ExprOrPatId::ExprId(id) => self.expr_path_hygiene(id),
            ExprOrPatId::PatId(id) => self.pat_path_hygiene(id),
        }
    }

    /// The expression `id`, decoded from the store's columns.
    #[inline]
    pub fn expr(&self, id: ExprId) -> Expr<'_> {
        self.assert_expr_only().columns.expr(id)
    }

    /// The pattern `id`, decoded from the store's columns.
    #[inline]
    pub fn pat(&self, id: PatId) -> Pat<'_> {
        self.assert_expr_only().columns.pat(id)
    }

    #[inline]
    pub fn exprs(&self) -> impl Iterator<Item = (ExprId, Expr<'_>)> {
        self.expr_only.as_deref().into_iter().flat_map(|it| it.columns.exprs())
    }

    /// How many expressions the store holds.
    #[inline]
    pub fn expr_count(&self) -> usize {
        self.expr_only.as_deref().map_or(0, |it| it.columns.expr_count())
    }

    /// How many patterns the store holds.
    #[inline]
    pub fn pat_count(&self) -> usize {
        self.expr_only.as_deref().map_or(0, |it| it.columns.pat_count())
    }

    /// Every expression's id, in order, without decoding any: for a scan that wants the ids
    /// (or tests only some of them), where [`Self::exprs`] decodes every node.
    #[inline]
    pub fn expr_ids(&self) -> impl DoubleEndedIterator<Item = ExprId> + ExactSizeIterator {
        (0..self.expr_count() as u32).map(|raw| ExprId::from_raw(la_arena::RawIdx::from_u32(raw)))
    }

    /// Every pattern's id, in order, without decoding any.
    #[inline]
    pub fn pat_ids(&self) -> impl DoubleEndedIterator<Item = PatId> + ExactSizeIterator {
        (0..self.pat_count() as u32).map(|raw| PatId::from_raw(la_arena::RawIdx::from_u32(raw)))
    }

    /// The store's columns, `None` for a store without expressions.
    #[cfg(test)]
    pub(crate) fn hir_columns(&self) -> Option<&crate::hir::columns::HirColumns> {
        self.expr_only.as_deref().map(|it| &it.columns)
    }

    #[inline]
    pub fn pats(&self) -> impl Iterator<Item = (PatId, Pat<'_>)> {
        self.expr_only.as_deref().into_iter().flat_map(|it| it.columns.pats())
    }

    #[inline]
    pub fn bindings(&self) -> impl Iterator<Item = (BindingId, &Binding)> {
        match &self.expr_only {
            Some(it) => it.bindings.iter(),
            None => const { &Arena::new() }.iter(),
        }
    }

    /// The coroutine associated with a coroutine closure.
    #[inline]
    pub fn coroutine_for_closure(coroutine_closure: ExprId) -> ExprId {
        // We keep the async closure exactly one expr before.
        ExprId::from_raw(la_arena::RawIdx::from_u32(coroutine_closure.into_raw().into_u32() - 1))
    }

    /// The opposite of [`Self::coroutine_for_closure()`].
    #[inline]
    pub fn closure_for_coroutine(coroutine: ExprId) -> ExprId {
        // We keep the async closure exactly one expr before.
        ExprId::from_raw(la_arena::RawIdx::from_u32(coroutine.into_raw().into_u32() + 1))
    }
}

pub trait StoreVisitor: Sized {
    fn on_expr(&mut self, expr: ExprId) {
        let _ = expr;
    }
    fn on_anon_const_expr(&mut self, expr: ExprId) {
        self.on_expr(expr);
    }
    fn on_pat(&mut self, pat: PatId) {
        let _ = pat;
    }
    fn on_type(&mut self, ty: TypeRefId) {
        let _ = ty;
    }
    fn on_lifetime(&mut self, lifetime: LifetimeRefId) {
        let _ = lifetime;
    }

    fn on_generic_args(&mut self, args: &GenericArgs) {
        visit_generic_args(self, args);
    }
}

pub(crate) fn visit_generic_args<V: StoreVisitor>(visitor: &mut V, args: &GenericArgs) {
    let GenericArgs { args, bindings, parenthesized: _, has_self_type: _ } = args;
    for arg in args {
        match arg {
            GenericArg::Type(arg) => visitor.on_type(*arg),
            GenericArg::Const(ConstRef { expr }) => visitor.on_anon_const_expr(*expr),
            GenericArg::Lifetime(arg) => visitor.on_lifetime(*arg),
        }
    }
    for AssociatedTypeBinding { name: _, args, type_ref, bounds } in bindings {
        visitor.on_generic_args_opt(args);
        visitor.on_type_opt(*type_ref);
        visitor.on_type_bounds(bounds);
    }
}

impl<V: StoreVisitor> StoreVisitor for &mut V {
    fn on_expr(&mut self, expr: ExprId) {
        V::on_expr(self, expr);
    }
    fn on_anon_const_expr(&mut self, expr: ExprId) {
        V::on_anon_const_expr(self, expr);
    }
    fn on_pat(&mut self, pat: PatId) {
        V::on_pat(self, pat);
    }
    fn on_type(&mut self, ty: TypeRefId) {
        V::on_type(self, ty);
    }
    fn on_lifetime(&mut self, lifetime: LifetimeRefId) {
        V::on_lifetime(self, lifetime);
    }

    fn on_generic_args(&mut self, args: &GenericArgs) {
        V::on_generic_args(self, args);
    }
}

pub trait StoreVisitorExt: StoreVisitor {
    fn on_type_bound(&mut self, bound: &TypeBound) {
        match bound {
            TypeBound::Path(path_id, _) => self.on_type(path_id.type_ref()),
            TypeBound::ForLifetime(_, path_id) => self.on_type(path_id.type_ref()),
            TypeBound::Lifetime(lifetime) => self.on_lifetime(*lifetime),
            TypeBound::Use(args) => {
                for arg in args {
                    match arg {
                        UseArgRef::Lifetime(lifetime) => self.on_lifetime(*lifetime),
                        UseArgRef::Name(_) => {}
                    }
                }
            }
            TypeBound::Error => {}
        }
    }

    fn on_path(&mut self, path: &Path) {
        match path {
            Path::Normal(path) => {
                let NormalPath { generic_args, type_anchor, mod_path: _ } = &**path;
                generic_args.iter().for_each(|generic_arg| self.on_generic_args_opt(generic_arg));
                self.on_type_opt(*type_anchor);
            }
            Path::BarePath(_) | Path::LangItem(..) => {}
        }
    }

    fn on_expr_opt(&mut self, expr: Option<ExprId>) {
        if let Some(expr) = expr {
            self.on_expr(expr);
        }
    }
    fn on_pat_opt(&mut self, pat: Option<PatId>) {
        if let Some(pat) = pat {
            self.on_pat(pat);
        }
    }
    fn on_type_opt(&mut self, ty: Option<TypeRefId>) {
        if let Some(ty) = ty {
            self.on_type(ty);
        }
    }
    fn on_lifetime_opt(&mut self, lifetime: Option<LifetimeRefId>) {
        if let Some(lifetime) = lifetime {
            self.on_lifetime(lifetime);
        }
    }
    fn on_generic_args_opt(&mut self, args: &Option<impl Borrow<GenericArgs>>) {
        if let Some(args) = args {
            self.on_generic_args(args.borrow());
        }
    }

    fn on_exprs(&mut self, exprs: impl IntoIterator<Item: Borrow<ExprId>>) {
        exprs.into_iter().for_each(|expr| self.on_expr(*expr.borrow()));
    }
    fn on_pats(&mut self, pats: impl IntoIterator<Item: Borrow<PatId>>) {
        pats.into_iter().for_each(|pat| self.on_pat(*pat.borrow()));
    }
    fn on_types(&mut self, types: impl IntoIterator<Item: Borrow<TypeRefId>>) {
        types.into_iter().for_each(|ty| self.on_type(*ty.borrow()));
    }
    fn on_type_bounds(&mut self, bounds: impl IntoIterator<Item: Borrow<TypeBound>>) {
        bounds.into_iter().for_each(|bound| self.on_type_bound(bound.borrow()));
    }
}
impl<V: StoreVisitor> StoreVisitorExt for V {}

impl Index<LabelId> for ExpressionStore {
    type Output = Label;

    #[inline]
    fn index(&self, label: LabelId) -> &Label {
        &self.assert_expr_only().labels[label]
    }
}

impl Index<BindingId> for ExpressionStore {
    type Output = Binding;

    #[inline]
    fn index(&self, b: BindingId) -> &Binding {
        &self.assert_expr_only().bindings[b]
    }
}

impl Index<LifetimeRefId> for ExpressionStore {
    type Output = LifetimeRef;

    #[inline]
    fn index(&self, b: LifetimeRefId) -> &LifetimeRef {
        &self.lifetimes[b]
    }
}

impl Index<PathId> for ExpressionStore {
    type Output = Path;

    #[inline]
    fn index(&self, index: PathId) -> &Self::Output {
        self.types.path(index)
    }
}

#[cfg(test)]
mod scratch_tests {
    use syntax::SyntaxKind;

    use super::{source_map::tests::src, *};

    fn empty_pool() {
        POOL.with(|pool| pool.borrow_mut().clear());
    }

    fn pooled() -> usize {
        POOL.with(|pool| pool.borrow().len())
    }

    /// A small store: type refs, expressions and patterns made through the list stacks, and
    /// source maps with repeated pointers.
    fn lower_one(builder: &mut ExpressionStoreBuilder, shift: u32) {
        builder.inference_roots = Some(SmallVec::new());
        let unit = builder.alloc_type_ref(TypeRef::UNIT);
        let mark = builder.list_mark();
        builder.push_list_type(unit);
        builder.push_list_type(unit);
        let lists = builder.take_lists();
        let tuple = builder.alloc_type_ref(TypeRef::Tuple(lists.types_since(mark)));
        builder.put_lists(lists, mark);
        let missing = builder.alloc_expr(Expr::Missing);
        let outer = builder.list_mark();
        builder.push_list_expr(missing);
        let inner = builder.list_mark();
        builder.push_list_expr(missing);
        builder.push_list_expr(missing);
        let lists = builder.take_lists();
        let tuple_expr = builder.alloc_expr(Expr::Tuple { exprs: lists.exprs_since(inner) });
        builder.put_lists(lists, inner);
        assert_eq!(builder.exprs_since(outer), &[missing], "the inner list is gone");
        builder.push_list_expr(tuple_expr);
        let lists = builder.take_lists();
        let call = builder.alloc_expr(Expr::Call { callee: missing, args: lists.exprs_since(outer) });
        builder.put_lists(lists, outer);
        let wild = builder.alloc_pat(Pat::Wild);
        for at in [shift, shift + 1, shift] {
            builder.expr_map.push((src(0, false, SyntaxKind::CALL_EXPR, at, at + 2), call.into()));
        }
        builder.pat_map.push((src(0, false, SyntaxKind::WILDCARD_PAT, 4, 5), wild.into()));
        builder.types_map.push((src(0, false, SyntaxKind::TUPLE_TYPE, 6, 9), tuple));
        builder.expr_map_back.insert(call, src(0, false, SyntaxKind::CALL_EXPR, shift, shift + 2));
        builder.types_map_back.insert(tuple, src(0, false, SyntaxKind::TUPLE_TYPE, 6, 9));
    }

    fn finished(shift: u32) -> (ExpressionStore, ExpressionStoreSourceMap) {
        let mut builder = ExpressionStoreBuilder::new();
        lower_one(&mut builder, shift);
        builder.finish()
    }

    #[test]
    fn nested_builders_take_the_pool_in_stack_order() {
        empty_pool();
        let mut outer = ExpressionStoreBuilder::new();
        outer.alloc_expr(Expr::Missing);
        outer.inference_roots = Some(SmallVec::new());
        let mut inner = ExpressionStoreBuilder::new();
        inner.alloc_expr(Expr::Missing);
        inner.inference_roots = Some(SmallVec::new());
        let (outer_at, inner_at) =
            (outer.columns.expr_kind.as_ptr(), inner.columns.expr_kind.as_ptr());
        assert_ne!(outer_at, inner_at, "a nested builder has scratch of its own");

        let _ = inner.finish();
        let _ = outer.finish();
        assert_eq!(pooled(), 2);
        let first = ExpressionStoreBuilder::new();
        let second = ExpressionStoreBuilder::new();
        assert_eq!(first.columns.expr_kind.as_ptr(), outer_at, "the last given is taken first");
        assert_eq!(second.columns.expr_kind.as_ptr(), inner_at);
        assert!(first.columns.is_empty() && second.columns.is_empty(), "given back emptied");
        assert_eq!(pooled(), 0);
    }

    #[test]
    fn a_store_is_the_same_on_fresh_or_reused_scratch() {
        empty_pool();
        let (fresh, fresh_map) = finished(0);
        assert_eq!(pooled(), 1);
        let _ = finished(10);
        let (reused, reused_map) = finished(0);
        assert_eq!(pooled(), 1, "one set is enough for builders one after another");
        assert_eq!(reused, fresh);
        assert_eq!(reused_map, fresh_map);
        let (fresh_only, reused_only) =
            (fresh_map.expr_only.as_deref().unwrap(), reused_map.expr_only.as_deref().unwrap());
        assert_eq!(reused_only.expr_map, fresh_only.expr_map);
        assert_eq!(reused_only.pat_map, fresh_only.pat_map);
        assert_eq!(reused_map.types_map, fresh_map.types_map);

        assert_eq!(fresh_only.expr_map.len(), 2, "a repeated pointer is one entry");
        let call = fresh.exprs().find(|(_, expr)| matches!(expr, Expr::Call { .. })).unwrap().0;
        let pushed_twice = src(0, false, SyntaxKind::CALL_EXPR, 0, 2);
        assert_eq!(fresh_only.expr_map.get(&pushed_twice), Some(&call.into()));
        let Expr::Call { args, .. } = fresh.expr(call) else { unreachable!() };
        assert!(matches!(fresh.expr(args[1]), Expr::Tuple { exprs } if exprs.len() == 2));
        let tuple = fresh.type_refs().find(|(_, ty)| matches!(ty, TypeRef::Tuple(_))).unwrap();
        assert!(matches!(tuple.1, TypeRef::Tuple(types) if types.len() == 2));
        let tuple_src = src(0, false, SyntaxKind::TUPLE_TYPE, 6, 9);
        assert_eq!(fresh_map.types_map.get(&tuple_src), Some(&tuple.0));
    }

    #[test]
    fn the_pool_keeps_few_and_small_sets() {
        empty_pool();
        let builders: Vec<_> = (0..POOL_SETS + 2).map(|_| ExpressionStoreBuilder::new()).collect();
        builders.into_iter().for_each(|builder| drop(builder.finish()));
        assert_eq!(pooled(), POOL_SETS);

        empty_pool();
        let mut huge = ExpressionStoreBuilder::new();
        huge.columns.expr_kind.reserve(POOL_ELEMENTS + 1);
        let _ = huge.finish();
        assert_eq!(pooled(), 0, "a set over the cap is dropped");
    }
}
