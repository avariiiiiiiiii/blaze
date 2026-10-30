//! Defines `ExpressionStore`: a lowered representation of functions, statics and
//! consts.
pub mod body;
mod expander;
pub mod lower;
pub mod path;
pub mod persist;
pub mod pretty;
pub mod scope;
#[cfg(test)]
mod tests;

use std::{
    borrow::Borrow,
    cell::RefCell,
    mem,
    ops::{Deref, Index},
    sync::LazyLock,
};

use base_db::SourceDatabase;
use blaze_ids::radix;
use cfg::CfgExpr;
use either::Either;
use hir_expand::{HirFileId, InFile, MacroCallId, mod_path::ModPath, name::Name};
use la_arena::{Arena, ArenaMap, Idx};
use rustc_hash::{FxHashMap, FxHashSet};
use salsa::plumbing::AsId;
use smallvec::SmallVec;
use span::{Edition, SyntaxContext};
use syntax::{AstNode, AstPtr, SyntaxNodePtr, ast};
use thin_vec::ThinVec;
use tt::TextRange;

use crate::{
    AdtId, BlockId, ExpressionStoreOwnerId, GenericDefId, SyntheticSyntax,
    expr_store::path::{AssociatedTypeBinding, GenericArg, GenericArgs, NormalPath, Path},
    hir::{
        Array, AsmOperand, Binding, BindingId, Expr, ExprId, ExprOrPatId, ExprOrPatIdPacked,
        InlineAsm, Label, LabelId, Literal, MatchArm, OffsetOf, Pat, PatId, RecordFieldPat,
        RecordLitField, RecordSpread, Statement,
        columns::{self, Frozen, Growing, HirColumns, take_exact},
    },
    nameres::{DefMap, block_def_map},
    signatures::VariantFields,
    type_ref::{
        ArrayType, ConstRef, FnType, LifetimeRef, LifetimeRefId, PathId, RefType, TypeBound,
        TypeColumns, TypeRef, TypeRefId, UseArgRef,
    },
};

pub use self::body::{Body, BodySourceMap};
pub use self::lower::{
    hir_assoc_type_binding_to_ast, hir_generic_arg_to_ast, hir_segment_to_ast_segment,
};

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

#[derive(Debug, Eq, Default)]
struct ExpressionOnlySourceMap {
    // AST expressions can create patterns in destructuring assignments. Therefore, `ExprSource` can also map
    // to `PatId`, and `PatId` can also map to `ExprSource` (the other way around is unaffected).
    expr_map: SourceTable<ExprSource, ExprOrPatIdPacked>,
    expr_map_back: ArenaMap<ExprId, ExprOrPatSource>,

    pat_map: SourceTable<PatSource, ExprOrPatIdPacked>,
    pat_map_back: ArenaMap<PatId, ExprOrPatSource>,

    label_map: SourceTable<LabelSource, LabelId>,
    label_map_back: ArenaMap<LabelId, LabelSource>,

    binding_definitions:
        ArenaMap<BindingId, SmallVec<[PatId; 2 * size_of::<usize>() / size_of::<PatId>()]>>,

    /// We don't create explicit nodes for record fields (`S { record_field: 92 }`).
    /// Instead, we use id of expression (`92`) to identify the field.
    field_map_back: IdTable<ExprId, FieldSource>,
    pat_field_map_back: IdTable<PatId, PatFieldSource>,

    template_map: Option<Box<FormatTemplate>>,

    expansions: SourceTable<InFile<MacroCallPtr>, MacroCallId>,

    /// Diagnostics accumulated during lowering. These contain `AstPtr`s and so are stored in
    /// the source map (since they're just as volatile).
    //
    // We store diagnostics on the `ExpressionOnlySourceMap` because diagnostics are rare (except
    // maybe for cfgs, and they are also not common in type places).
    diagnostics: ThinVec<ExpressionStoreDiagnostics>,
}

/// A map from a store's own ids to values, as one column sorted by id and searched: what a
/// finished store or source map keeps in place of a hash map keyed by its ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdTable<K, V>(Box<[(K, V)]>);

impl<K, V> Default for IdTable<K, V> {
    fn default() -> Self {
        IdTable(Box::default())
    }
}

impl<K: Ord + Copy, V> IdTable<K, V> {
    /// The table of `pushed`, in the order they were pushed: a later value for an id replaces
    /// an earlier one, as an insert into a map does.
    pub(crate) fn from_pushed(mut pushed: Vec<(K, V)>) -> Self {
        pushed.reverse();
        // Stable, so the latest value of each id comes first among its equals.
        pushed.sort_by_key(|&(id, _)| id);
        pushed.dedup_by_key(|&mut (id, _)| id);
        IdTable(pushed.into_boxed_slice())
    }

    /// The table of `entries`, which are sorted by id with no id twice.
    pub(crate) fn from_sorted(entries: Box<[(K, V)]>) -> Option<Self> {
        entries.windows(2).all(|pair| pair[0].0 < pair[1].0).then_some(IdTable(entries))
    }

    pub(crate) fn get(&self, id: K) -> Option<&V> {
        let at = self.0.binary_search_by_key(&id, |&(it, _)| it).ok()?;
        Some(&self.0[at].1)
    }

    pub(crate) fn entries(&self) -> &[(K, V)] {
        &self.0
    }

    /// The same ids, each value as `f` makes it of the old.
    pub(crate) fn map<W>(&self, mut f: impl FnMut(K, &V) -> W) -> IdTable<K, W> {
        IdTable(self.0.iter().map(|(id, value)| (*id, f(*id, value))).collect())
    }
}

/// The order of a [`SourceTable`]'s keys, total and the same on every run (a hash map's is
/// neither): the file (files before macro files, each by its salsa id's bits), the range's start
/// and end, the syntax kind. `expr_store/persist.rs` writes and checks tables in exactly this
/// order (`Src::key`), so a change here is a change of its format.
pub(crate) trait SourceKey {
    fn source_key(&self) -> (u8, u64, u32, u32, u16);
}

impl<N: AstNode> SourceKey for InFile<AstPtr<N>> {
    #[inline]
    fn source_key(&self) -> (u8, u64, u32, u32, u16) {
        let (file_kind, file) = match self.file_id {
            HirFileId::FileId(file) => (1, file.as_id().as_bits()),
            HirFileId::MacroFile(call) => (2, call.as_id().as_bits()),
        };
        let range = self.value.text_range();
        (file_kind, file, range.start().into(), range.end().into(), u16::from(self.value.kind()))
    }
}

/// A map from syntax pointers to a store's ids, as one column sorted by [`SourceKey`] and
/// searched: what a source map keeps in place of a hash map. 24 bytes per entry for an
/// `InFile<AstPtr<_>>` key and a `u32` value; a lookup is `log2(n)` key compares with no
/// hashing, and iteration is in key order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceTable<K, V>(Box<[(K, V)]>);

impl<K, V> Default for SourceTable<K, V> {
    fn default() -> Self {
        SourceTable(Box::default())
    }
}

impl<K: SourceKey, V> SourceTable<K, V> {
    /// The table of `pushed`, in the order lowering pushed them: a later value for a key
    /// replaces an earlier one, as an insert into a map does. `pushed` is left empty with its
    /// capacity (it is a pooled builder's column); the table is one allocation of its length.
    /// Lowering pushes in preorder, which is nearly in key order, so the stable sort is close to
    /// one linear pass.
    pub(crate) fn from_pushed(pushed: &mut Vec<(K, V)>) -> Self {
        pushed.sort_by_key(|(key, _)| key.source_key());
        // Stable, so equal keys stay in push order; keep the last of each run.
        pushed.dedup_by(|later, kept| {
            let same = later.0.source_key() == kept.0.source_key();
            if same {
                mem::swap(later, kept);
            }
            same
        });
        SourceTable(take_exact(pushed))
    }

    /// The table of `entries`, which are in strictly increasing key order (no key twice).
    pub(crate) fn from_sorted(entries: Box<[(K, V)]>) -> Option<Self> {
        let sorted = entries.windows(2).all(|pair| pair[0].0.source_key() < pair[1].0.source_key());
        sorted.then_some(SourceTable(entries))
    }

    #[inline]
    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        let key = key.source_key();
        let at = self.0.binary_search_by(|(it, _)| it.source_key().cmp(&key)).ok()?;
        Some(&self.0[at].1)
    }

    /// Every entry, in key order.
    #[inline]
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (&K, &V)> + '_ {
        self.0.iter().map(|(key, value)| (key, value))
    }

    #[inline]
    pub(crate) fn entries(&self) -> &[(K, V)] {
        &self.0
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl PartialEq for ExpressionOnlySourceMap {
    fn eq(&self, other: &Self) -> bool {
        // we only need to compare one of the two mappings
        // as the other is a reverse mapping and thus will compare
        // the same as normal mapping
        let Self {
            expr_map: _,
            expr_map_back,
            pat_map: _,
            pat_map_back,
            label_map: _,
            label_map_back,
            // If this changed, our pattern data must have changed
            binding_definitions: _,
            // If this changed, our expression data must have changed
            field_map_back: _,
            // If this changed, our pattern data must have changed
            pat_field_map_back: _,
            template_map,
            expansions,
            diagnostics,
        } = self;
        *expr_map_back == other.expr_map_back
            && *pat_map_back == other.pat_map_back
            && *label_map_back == other.label_map_back
            && *template_map == other.template_map
            && *expansions == other.expansions
            && *diagnostics == other.diagnostics
    }
}

#[derive(Debug, Eq, Default)]
pub struct ExpressionStoreSourceMap {
    expr_only: Option<Box<ExpressionOnlySourceMap>>,

    types_map_back: ArenaMap<TypeRefId, TypeSource>,
    types_map: SourceTable<TypeSource, TypeRefId>,

    lifetime_map_back: ArenaMap<LifetimeRefId, LifetimeSource>,
    lifetime_map: SourceTable<LifetimeSource, LifetimeRefId>,
}

impl PartialEq for ExpressionStoreSourceMap {
    fn eq(&self, other: &Self) -> bool {
        // we only need to compare one of the two mappings
        // as the other is a reverse mapping and thus will compare
        // the same as normal mapping
        let Self { expr_only, types_map_back, types_map: _, lifetime_map_back, lifetime_map: _ } =
            self;
        *expr_only == other.expr_only
            && *types_map_back == other.types_map_back
            && *lifetime_map_back == other.lifetime_map_back
    }
}

/// Moves the pointers into `file` of a source map: [`ExpressionStoreSourceMap::moved`].
pub(crate) struct Move<'a> {
    pub(crate) file: HirFileId,
    pub(crate) to: &'a dyn Fn(SyntaxNodePtr) -> SyntaxNodePtr,
}

impl Move<'_> {
    fn ptr<N: AstNode>(&self, src: InFile<AstPtr<N>>) -> InFile<AstPtr<N>> {
        if src.file_id != self.file {
            return src;
        }
        let moved = (self.to)(src.value.syntax_node_ptr());
        InFile::new(src.file_id, AstPtr::try_from_raw(moved).expect("moved to another kind"))
    }

    fn back<I, N: AstNode>(
        &self,
        map: &ArenaMap<la_arena::Idx<I>, InFile<AstPtr<N>>>,
    ) -> ArenaMap<la_arena::Idx<I>, InFile<AstPtr<N>>> {
        map.iter().map(|(id, &src)| (id, self.ptr(src))).collect()
    }

    /// Moving pointers can change their order, so the table is sorted again.
    fn forth<N: AstNode, V: Copy>(
        &self,
        map: &SourceTable<InFile<AstPtr<N>>, V>,
    ) -> SourceTable<InFile<AstPtr<N>>, V> {
        let mut moved = map.iter().map(|(&src, &id)| (self.ptr(src), id)).collect::<Vec<_>>();
        SourceTable::from_pushed(&mut moved)
    }

    fn raw(&self, src: InFile<SyntaxNodePtr>) -> InFile<SyntaxNodePtr> {
        match src.file_id == self.file {
            true => InFile::new(src.file_id, (self.to)(src.value)),
            false => src,
        }
    }
}

impl ExpressionStoreSourceMap {
    /// The same map with what it points to in `mv.file` moved: what a body lowered as a tree of
    /// its own maps to, as a map of where the body is in its file
    /// ([`crate::expr_store::body::BodySyntax`]).
    pub(crate) fn moved(&self, mv: &Move<'_>) -> ExpressionStoreSourceMap {
        let types_map_back = mv.back(&self.types_map_back);
        let lifetime_map_back = mv.back(&self.lifetime_map_back);
        let expr_only = self.expr_only.as_deref().map(|it| {
            let template_map = it.template_map.as_deref().map(|it| FormatTemplate {
                format_args_to_captures: it.format_args_to_captures.clone(),
                asm_to_captures: it.asm_to_captures.clone(),
                implicit_capture_to_source: it.implicit_capture_to_source.map(|_, src| {
                    let (ptr, range) = src.value;
                    let ptr = mv.ptr(InFile::new(src.file_id, ptr)).value;
                    InFile::new(src.file_id, (ptr, range))
                }),
            });
            Box::new(ExpressionOnlySourceMap {
                expr_map: mv.forth(&it.expr_map),
                expr_map_back: mv.back(&it.expr_map_back),
                pat_map: mv.forth(&it.pat_map),
                pat_map_back: mv.back(&it.pat_map_back),
                label_map: mv.forth(&it.label_map),
                label_map_back: mv.back(&it.label_map_back),
                binding_definitions: it.binding_definitions.clone(),
                field_map_back: it.field_map_back.map(|_, &src| mv.ptr(src)),
                pat_field_map_back: it.pat_field_map_back.map(|_, &src| mv.ptr(src)),
                template_map: template_map.map(Box::new),
                expansions: mv.forth(&it.expansions),
                diagnostics: it.diagnostics.iter().map(|it| it.moved(mv)).collect(),
            })
        });
        let mut types_map: Vec<_> = types_map_back.iter().map(|(id, &src)| (src, id)).collect();
        let mut lifetime_map: Vec<_> =
            lifetime_map_back.iter().map(|(id, &src)| (src, id)).collect();
        ExpressionStoreSourceMap {
            expr_only,
            types_map: SourceTable::from_pushed(&mut types_map),
            types_map_back,
            lifetime_map: SourceTable::from_pushed(&mut lifetime_map),
            lifetime_map_back,
        }
    }
}

impl ExpressionStoreDiagnostics {
    fn moved(&self, mv: &Move<'_>) -> Self {
        use ExpressionStoreDiagnostics::*;
        match self {
            InactiveCode { node, cfg } => InactiveCode { node: mv.raw(*node), cfg: cfg.clone() },
            UnresolvedMacroCall { node, path } => {
                UnresolvedMacroCall { node: mv.ptr(*node), path: path.clone() }
            }
            UnreachableLabel { node, name } => {
                UnreachableLabel { node: mv.ptr(*node), name: name.clone() }
            }
            AwaitOutsideOfAsync { node, location } => {
                AwaitOutsideOfAsync { node: mv.ptr(*node), location: location.clone() }
            }
            UndeclaredLabel { node, name } => {
                UndeclaredLabel { node: mv.ptr(*node), name: name.clone() }
            }
            PatternArgInExternFn { node } => PatternArgInExternFn { node: mv.ptr(*node) },
            FruInDestructuringAssignment { node } => {
                FruInDestructuringAssignment { node: mv.ptr(*node) }
            }
            MissingBody { node, kind } => MissingBody { node: mv.raw(*node), kind: *kind },
        }
    }
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
    // sorts each into a `SourceTable`, a later push for a pointer replacing an earlier one.
    expr_map: Vec<(ExprSource, ExprOrPatIdPacked)>,
    expr_map_back: ArenaMap<ExprId, ExprOrPatSource>,

    pat_map: Vec<(PatSource, ExprOrPatIdPacked)>,
    pat_map_back: ArenaMap<PatId, ExprOrPatSource>,

    label_map: Vec<(LabelSource, LabelId)>,
    label_map_back: ArenaMap<LabelId, LabelSource>,

    types_map_back: ArenaMap<TypeRefId, TypeSource>,
    types_map: Vec<(TypeSource, TypeRefId)>,

    lifetime_map_back: ArenaMap<LifetimeRefId, LifetimeSource>,
    lifetime_map: Vec<(LifetimeSource, LifetimeRefId)>,

    binding_definitions:
        ArenaMap<BindingId, SmallVec<[PatId; 2 * size_of::<usize>() / size_of::<PatId>()]>>,

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
}

/// The captures of a `format_args!()`: its hygiene and each implicitly captured name.
type FormatCaptures = (HygieneId, Vec<(syntax::TextRange, Name)>);
/// The captures of each template piece of an `asm!()`.
type AsmCaptures = Vec<Vec<(syntax::TextRange, usize)>>;
/// Where an implicit capture is written: its template literal and the offset inside it.
type CaptureSource = InFile<(ExprPtr, TextRange)>;
/// A [`FormatTemplate`] while lowering: each map pushed to in the order it is made.
type PendingTemplate = FormatTemplate<
    Vec<(ExprId, FormatCaptures)>,
    Vec<(ExprId, AsmCaptures)>,
    Vec<(ExprId, CaptureSource)>,
>;

/// The maps of a store's templates: pushed to while lowering ([`ExpressionStoreBuilder`]),
/// tables once finished.
#[derive(Default, Debug, Eq, PartialEq)]
struct FormatTemplate<
    F = IdTable<ExprId, FormatCaptures>,
    A = IdTable<ExprId, AsmCaptures>,
    C = IdTable<ExprId, CaptureSource>,
> {
    /// A map from `format_args!()` expressions to their captures.
    format_args_to_captures: F,
    /// A map from `asm!()` expressions to their captures.
    asm_to_captures: A,
    /// A map from desugared expressions of implicit captures to their source.
    ///
    /// The value stored for each capture is its template literal and offset inside it. The template literal
    /// is from the `format_args[_nl]!()` macro and so needs to be mapped up once to go to the user-written
    /// template.
    implicit_capture_to_source: C,
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
    expr_map_back: ArenaMap<ExprId, ExprOrPatSource>,
    pat_map_back: ArenaMap<PatId, ExprOrPatSource>,
    label_map_back: ArenaMap<LabelId, LabelSource>,
    types_map_back: ArenaMap<TypeRefId, TypeSource>,
    lifetime_map_back: ArenaMap<LifetimeRefId, LifetimeSource>,
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
    }

    fn capacity(&self) -> usize {
        self.columns.capacity()
            + self.types.capacity()
            + self.lists.capacity()
            + self.expr_map.capacity()
            + self.pat_map.capacity()
            + self.types_map.capacity()
    }
}

/// `map`'s entries in a map of exactly their room, leaving `map` empty with its capacity.
fn take_map<K, V: Copy>(map: &mut ArenaMap<Idx<K>, V>) -> ArenaMap<Idx<K>, V> {
    let len = map.iter().next_back().map_or(0, |(id, _)| id.into_raw().into_u32() as usize + 1);
    let mut exact = ArenaMap::with_capacity(len);
    exact.extend(map.iter().map(|(id, &value)| (id, value)));
    map.clear();
    exact
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
            binding_definitions: ArenaMap::default(),
            field_map_back: Vec::new(),
            pat_field_map_back: Vec::new(),
            template_map: None,
            expansions,
            diagnostics: Vec::new(),
        }
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
            mut binding_definitions,
            field_map_back,
            pat_field_map_back,
            template_map,
            expansions,
            diagnostics,
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
        };
        let columns = scratch.columns.freeze_into();
        let types = scratch.types.freeze_into();
        let expr_map = SourceTable::from_pushed(&mut scratch.expr_map);
        let expr_map_back = take_map(&mut scratch.expr_map_back);
        let pat_map = SourceTable::from_pushed(&mut scratch.pat_map);
        let pat_map_back = take_map(&mut scratch.pat_map_back);
        let label_map = SourceTable::from_pushed(&mut scratch.label_map);
        let label_map_back = take_map(&mut scratch.label_map_back);
        let types_map = SourceTable::from_pushed(&mut scratch.types_map);
        let types_map_back = take_map(&mut scratch.types_map_back);
        let lifetime_map = SourceTable::from_pushed(&mut scratch.lifetime_map);
        let lifetime_map_back = take_map(&mut scratch.lifetime_map_back);
        let expansions = SourceTable::from_pushed(&mut scratch.expansions);
        scratch.give();

        labels.shrink_to_fit();
        bindings.shrink_to_fit();
        lifetimes.shrink_to_fit();
        binding_definitions.shrink_to_fit();
        let field_map_back = IdTable::from_pushed(field_map_back);
        let pat_field_map_back = IdTable::from_pushed(pat_field_map_back);
        let template_map = template_map.map(|template| {
            let FormatTemplate { format_args_to_captures, asm_to_captures, implicit_capture_to_source } =
                *template;
            Box::new(FormatTemplate {
                format_args_to_captures: IdTable::from_pushed(format_args_to_captures),
                asm_to_captures: IdTable::from_pushed(asm_to_captures),
                implicit_capture_to_source: IdTable::from_pushed(implicit_capture_to_source),
            })
        });

        let has_exprs = !columns.is_empty() || !labels.is_empty() || !bindings.is_empty();

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

        let source_map = {
            let expr_only = if has_exprs || !expansions.is_empty() || !diagnostics.is_empty() {
                Some(Box::new(ExpressionOnlySourceMap {
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
                }))
            } else {
                None
            };
            ExpressionStoreSourceMap {
                expr_only,
                types_map_back,
                types_map,
                lifetime_map_back,
                lifetime_map,
            }
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

// FIXME: Change `node_` prefix to something more reasonable.
// Perhaps `expr_syntax` and `expr_id`?
impl ExpressionStoreSourceMap {
    pub fn expr_or_pat_syntax(&self, id: ExprOrPatId) -> Result<ExprOrPatSource, SyntheticSyntax> {
        match id {
            ExprOrPatId::ExprId(id) => self.expr_syntax(id),
            ExprOrPatId::PatId(id) => self.pat_syntax(id),
        }
    }

    #[inline]
    fn expr_or_synthetic(&self) -> Result<&ExpressionOnlySourceMap, SyntheticSyntax> {
        self.expr_only.as_deref().ok_or(SyntheticSyntax)
    }

    #[inline]
    fn expr_only(&self) -> Option<&ExpressionOnlySourceMap> {
        self.expr_only.as_deref()
    }

    #[inline]
    #[track_caller]
    fn assert_expr_only(&self) -> &ExpressionOnlySourceMap {
        self.expr_only.as_ref().expect("should have `ExpressionStoreSourceMap::expr_only`")
    }

    pub fn expr_syntax(&self, expr: ExprId) -> Result<ExprOrPatSource, SyntheticSyntax> {
        self.expr_or_synthetic()?.expr_map_back.get(expr).cloned().ok_or(SyntheticSyntax)
    }

    pub fn node_expr(&self, node: InFile<&ast::Expr>) -> Option<ExprOrPatId> {
        let src = node.map(AstPtr::new);
        self.expr_only()?.expr_map.get(&src).cloned().map(ExprOrPatIdPacked::unpack)
    }

    pub fn node_macro_file(&self, node: InFile<&ast::MacroCall>) -> Option<MacroCallId> {
        let src = node.map(AstPtr::new);
        self.expr_only()?.expansions.get(&src).cloned()
    }

    pub fn macro_calls(&self) -> impl Iterator<Item = (InFile<MacroCallPtr>, MacroCallId)> + '_ {
        self.expr_only().into_iter().flat_map(|it| it.expansions.iter().map(|(&a, &b)| (a, b)))
    }

    pub fn pat_syntax(&self, pat: PatId) -> Result<ExprOrPatSource, SyntheticSyntax> {
        self.expr_or_synthetic()?.pat_map_back.get(pat).cloned().ok_or(SyntheticSyntax)
    }

    pub fn node_pat(&self, node: InFile<&ast::Pat>) -> Option<ExprOrPatId> {
        self.expr_only()?
            .pat_map
            .get(&node.map(AstPtr::new))
            .cloned()
            .map(ExprOrPatIdPacked::unpack)
    }

    pub fn type_syntax(&self, id: TypeRefId) -> Result<TypeSource, SyntheticSyntax> {
        self.types_map_back.get(id).cloned().ok_or(SyntheticSyntax)
    }

    pub fn node_type(&self, node: InFile<&ast::Type>) -> Option<TypeRefId> {
        self.types_map.get(&node.map(AstPtr::new)).cloned()
    }

    pub fn label_syntax(&self, label: LabelId) -> LabelSource {
        self.assert_expr_only().label_map_back[label]
    }

    pub fn patterns_for_binding(&self, binding: BindingId) -> &[PatId] {
        self.assert_expr_only().binding_definitions.get(binding).map_or(&[], Deref::deref)
    }

    pub fn node_label(&self, node: InFile<&ast::Label>) -> Option<LabelId> {
        let src = node.map(AstPtr::new).map(AstPtr::wrap_left);
        self.expr_only()?.label_map.get(&src).cloned()
    }

    pub fn field_syntax(&self, expr: ExprId) -> FieldSource {
        *self.assert_expr_only().field_map_back.get(expr).expect("no field with this expression")
    }

    pub fn pat_field_syntax(&self, pat: PatId) -> PatFieldSource {
        *self.assert_expr_only().pat_field_map_back.get(pat).expect("no field with this pattern")
    }

    pub fn macro_expansion_expr(&self, node: InFile<&ast::MacroExpr>) -> Option<ExprOrPatId> {
        let src = node.map(AstPtr::new).map(AstPtr::upcast::<ast::MacroExpr>).map(AstPtr::upcast);
        self.expr_only()?.expr_map.get(&src).copied().map(ExprOrPatIdPacked::unpack)
    }

    pub fn expansions(&self) -> impl Iterator<Item = (&InFile<MacroCallPtr>, &MacroCallId)> {
        self.expr_only().into_iter().flat_map(|it| it.expansions.iter())
    }

    pub fn expansion(&self, node: InFile<&ast::MacroCall>) -> Option<MacroCallId> {
        self.expr_only()?.expansions.get(&node.map(AstPtr::new)).copied()
    }

    pub fn implicit_format_args(
        &self,
        node: InFile<&ast::FormatArgsExpr>,
    ) -> Option<(HygieneId, &[(syntax::TextRange, Name)])> {
        let expr_only = self.expr_only()?;
        let src = node.map(AstPtr::new).map(AstPtr::upcast::<ast::Expr>);
        let (hygiene, names) = expr_only
            .template_map
            .as_ref()?
            .format_args_to_captures
            .get(expr_only.expr_map.get(&src)?.as_expr()?)?;
        Some((*hygiene, &**names))
    }

    pub fn format_args_implicit_capture(
        &self,
        capture_expr: ExprId,
    ) -> Option<InFile<(ExprPtr, TextRange)>> {
        self.expr_only()?
            .template_map
            .as_ref()?
            .implicit_capture_to_source
            .get(capture_expr)
            .copied()
    }

    pub fn asm_template_args(
        &self,
        node: InFile<&ast::AsmExpr>,
    ) -> Option<(ExprId, &[Vec<(syntax::TextRange, usize)>])> {
        let expr_only = self.expr_only()?;
        let src = node.map(AstPtr::new).map(AstPtr::upcast::<ast::Expr>);
        let expr = expr_only.expr_map.get(&src)?.as_expr()?;
        Some(expr).zip(
            expr_only.template_map.as_ref()?.asm_to_captures.get(expr).map(std::ops::Deref::deref),
        )
    }

    /// Get a reference to the source map's diagnostics.
    pub fn diagnostics(&self) -> &[ExpressionStoreDiagnostics] {
        self.expr_only().map(|it| &*it.diagnostics).unwrap_or_default()
    }
}

#[cfg(test)]
mod scratch_tests {
    use salsa::plumbing::FromId;
    use syntax::{SyntaxKind, TextSize};

    use super::*;

    /// A pointer to a node of `kind` at `start..end` in file (or, when `in_macro`, macro file)
    /// `file`.
    fn src<N: AstNode>(
        file: u32,
        in_macro: bool,
        kind: SyntaxKind,
        start: u32,
        end: u32,
    ) -> InFile<AstPtr<N>> {
        let id = salsa::Id::from_bits(u64::from(file) + 1);
        let file_id = match in_macro {
            true => HirFileId::MacroFile(FromId::from_id(id)),
            false => HirFileId::FileId(FromId::from_id(id)),
        };
        let range = TextRange::new(TextSize::from(start), TextSize::from(end));
        let ptr = AstPtr::try_from_raw(SyntaxNodePtr::from_parts(kind, range));
        InFile::new(file_id, ptr.expect("a pointer of its type"))
    }

    /// xorshift64*: the same pushes on every run.
    struct Seeded(u64);

    impl Seeded {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
        }
    }

    const EXPR_KINDS: [SyntaxKind; 3] =
        [SyntaxKind::PATH_EXPR, SyntaxKind::CALL_EXPR, SyntaxKind::LITERAL];

    /// A pointer from a small domain, so that pushes repeat keys.
    fn seeded_src(rng: &mut Seeded) -> ExprSource {
        let start = rng.below(8) as u32;
        let end = start + rng.below(4) as u32;
        let kind = EXPR_KINDS[rng.below(3) as usize];
        src(rng.below(3) as u32, rng.below(2) == 1, kind, start, end)
    }

    #[test]
    fn a_source_table_answers_as_a_hash_map_does() {
        for seed in 0..64 {
            let mut rng = Seeded(seed * 2 + 1);
            let mut pushed = Vec::new();
            let mut model = FxHashMap::default();
            for value in 0..rng.below(300) as u32 {
                let key = seeded_src(&mut rng);
                pushed.push((key, value));
                model.insert(key, value);
            }
            let room = pushed.capacity();
            let table = SourceTable::from_pushed(&mut pushed);
            assert!(pushed.is_empty() && pushed.capacity() == room, "the scratch keeps its room");

            assert_eq!(table.len(), model.len());
            for (key, value) in &model {
                assert_eq!(table.get(key), Some(value), "the last push of a key wins");
            }
            let absent = src::<ast::Expr>(7, false, SyntaxKind::PATH_EXPR, 0, 0);
            assert_eq!(table.get(&absent), None);

            let keys: Vec<_> = table.iter().map(|(key, _)| key.source_key()).collect();
            assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "iteration is in key order");
            let mut sorted: Vec<_> = model.keys().map(SourceKey::source_key).collect();
            sorted.sort_unstable();
            assert_eq!(keys, sorted);

            let again = SourceTable::from_sorted(table.entries().to_vec().into_boxed_slice());
            assert_eq!(again.as_ref(), Some(&table));
            if table.len() > 1 {
                let mut reversed = table.entries().to_vec();
                reversed.reverse();
                assert_eq!(SourceTable::from_sorted(reversed.into_boxed_slice()), None);
            }
        }
    }

    #[test]
    fn files_come_before_macro_files_then_ranges_then_kinds() {
        let ordered: [ExprSource; 5] = [
            src(0, false, SyntaxKind::CALL_EXPR, 0, 9),
            src(0, false, SyntaxKind::CALL_EXPR, 1, 2),
            src(0, false, SyntaxKind::CALL_EXPR, 1, 3),
            src(1, false, SyntaxKind::PATH_EXPR, 0, 1),
            src(0, true, SyntaxKind::PATH_EXPR, 0, 1),
        ];
        let mut pushed: Vec<_> = ordered.iter().rev().map(|&key| (key, ())).collect();
        let table = SourceTable::from_pushed(&mut pushed);
        assert_eq!(table.iter().map(|(&key, _)| key).collect::<Vec<_>>(), ordered);
        let (call, path) = (SyntaxKind::CALL_EXPR, SyntaxKind::PATH_EXPR);
        assert!(u16::from(path) != u16::from(call));
        let same_range = [src::<ast::Expr>(0, false, call, 0, 1), src(0, false, path, 0, 1)];
        let by_kind = same_range[0].source_key() < same_range[1].source_key();
        assert_eq!(by_kind, u16::from(call) < u16::from(path));
    }

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
