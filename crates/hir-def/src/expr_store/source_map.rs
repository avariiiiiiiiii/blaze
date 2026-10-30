//! The source map of an [`ExpressionStore`](super::ExpressionStore): where each of its ids is
//! written, and which id each piece of syntax lowered to.
//!
//! Every part is columns, made once by [`super::ExpressionStoreBuilder::finish`] from what
//! lowering pushed, in one pass each and with no hash map:
//! - the maps back from an id to its syntax ([`BackMap`]) are dense over the id's arena: a `u16`
//!   syntax kind ([`NO_SOURCE`] for an id lowered from no syntax), the range as two `u32`s, and
//!   the file once per run of ids written in it;
//! - the patterns of each binding ([`BindingPats`]) are CSR;
//! - the maps from syntax to an id ([`SourceTable`]) are sorted by [`SourceKey`] and searched;
//! - the maps keyed by a store's own ids ([`IdTable`]) are sorted by id and searched.

use std::{fmt, marker::PhantomData};

use either::Either;
use hir_expand::{HirFileId, InFile, MacroCallId, name::Name};
use la_arena::{Idx, RawIdx};
use salsa::plumbing::AsId;
use syntax::{AstNode, AstPtr, SyntaxKind, SyntaxNodePtr, TextRange, TextSize, ast};

use crate::{
    SyntheticSyntax,
    hir::{
        BindingId, Expr, ExprId, ExprOrPatId, ExprOrPatIdPacked, Label, LabelId, Pat, PatId,
        columns::take_exact,
    },
    type_ref::{LifetimeRef, LifetimeRefId, TypeRef, TypeRefId},
};

use super::{
    ExprOrPatSource, ExprPtr, ExprSource, ExpressionStoreDiagnostics, FieldSource, HygieneId,
    LabelSource, LifetimeSource, MacroCallPtr, PatFieldSource, PatSource, TypeSource,
};

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
        // alloc: the new table, one exact allocation (the iterator is exact-size)
        IdTable(self.0.iter().map(|(id, value)| (*id, f(*id, value))).collect())
    }
}

/// The order of a [`SourceTable`]'s keys, total and the same on every run (a hash map's is
/// neither): the file (files before macro files, each by its salsa id's bits), the range's start
/// and end, the syntax kind. `expr_store/persist/source_map.rs` writes and checks tables in
/// exactly this order (`Src::key`), so a change here is a change of its format.
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
                std::mem::swap(later, kept);
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

/// The kind of an id lowered from no syntax, in [`BackMap::kinds`]: no [`SyntaxKind`] is
/// `u16::MAX`.
pub(in crate::expr_store) const NO_SOURCE: u16 = u16::MAX;
const _: () = assert!((SyntaxKind::__LAST as u16) < NO_SOURCE);

/// The bytes of one id in a [`BackMap`]: its kind, start and end. An `ArenaMap` slot of an
/// `Option<InFile<AstPtr<_>>>` was 24 on 64-bit.
const BACK_ROW: usize = size_of::<u16>() + 2 * size_of::<u32>();
const _: () = assert!(BACK_ROW == 10);
/// The bytes of one run of ids in one file: its first id and the file.
const RUN_ROW: usize = size_of::<u32>() + size_of::<HirFileId>();
const _: () = assert!(RUN_ROW <= 16);
/// The bytes of a binding and of each of its patterns in [`BindingPats`]. An `ArenaMap` slot of
/// an `Option<SmallVec<[PatId; 4]>>` was 32 on 64-bit, on the heap past 4 patterns.
const _: () = assert!(size_of::<u32>() == 4 && size_of::<PatId>() == 4);

/// Where each id of one arena is written, by id: dense columns of the arena's length.
///
/// Lowering allocates ids in preorder, so the ids of a macro's expansion are one range between
/// ids of the file the call is in: the file is kept once per run of ids written in it, not per
/// id, and found by a binary search over the runs (one compare for a body with no macros, the
/// common case). The columns are canonical, so two maps of the same ids to the same syntax
/// are equal: an id with no syntax is [`NO_SOURCE`] with a range of zeros, the first run starts
/// at id 0, and each other run starts at the first id with syntax in a file other than the one
/// before it.
pub(crate) struct BackMap<T, N> {
    /// The syntax kind of each id's node, [`NO_SOURCE`] for an id without one.
    pub(in crate::expr_store) kinds: Box<[u16]>,
    pub(in crate::expr_store) starts: Box<[u32]>,
    pub(in crate::expr_store) ends: Box<[u32]>,
    /// The first id of each run, increasing; empty when no id has syntax.
    pub(in crate::expr_store) run_starts: Box<[u32]>,
    /// The file of each run, none the same as the one before it.
    pub(in crate::expr_store) run_files: Box<[HirFileId]>,
    of: PhantomData<fn() -> (T, N)>,
}

pub(crate) type ExprBackMap = BackMap<Expr<'static>, Either<ast::Expr, ast::Pat>>;
pub(crate) type PatBackMap = BackMap<Pat<'static>, Either<ast::Expr, ast::Pat>>;
pub(crate) type LabelBackMap = BackMap<Label, Either<ast::Label, ast::BlockExpr>>;
pub(crate) type TypeBackMap = BackMap<TypeRef<'static>, ast::Type>;
pub(crate) type LifetimeBackMap = BackMap<LifetimeRef, ast::Lifetime>;

impl<T, N> Default for BackMap<T, N> {
    fn default() -> Self {
        BackMap {
            kinds: Box::default(),
            starts: Box::default(),
            ends: Box::default(),
            run_starts: Box::default(),
            run_files: Box::default(),
            of: PhantomData,
        }
    }
}

impl<T, N> PartialEq for BackMap<T, N> {
    fn eq(&self, other: &Self) -> bool {
        self.kinds == other.kinds
            && self.starts == other.starts
            && self.ends == other.ends
            && self.run_starts == other.run_starts
            && self.run_files == other.run_files
    }
}

impl<T, N> Eq for BackMap<T, N> {}

impl<T, N: AstNode> fmt::Debug for BackMap<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[inline]
fn raw<T>(id: Idx<T>) -> u32 {
    id.into_raw().into_u32()
}

#[inline]
fn idx<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

impl<T, N> BackMap<T, N> {
    /// The map of these columns, which [`BackMap`]'s rules hold for: what a buffer that was
    /// checked against them decodes to (`expr_store/persist/source_map.rs`).
    pub(in crate::expr_store) fn from_columns(
        kinds: Box<[u16]>,
        starts: Box<[u32]>,
        ends: Box<[u32]>,
        run_starts: Box<[u32]>,
        run_files: Box<[HirFileId]>,
    ) -> Self {
        debug_assert!(kinds.len() == starts.len() && kinds.len() == ends.len());
        debug_assert!(run_starts.len() == run_files.len());
        BackMap { kinds, starts, ends, run_starts, run_files, of: PhantomData }
    }

    /// The length of the arena this maps: every id below it has a slot.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.kinds.len()
    }

    /// The run `slot` is in. Only for a slot with syntax, which is in a run.
    #[inline]
    fn run_of(&self, slot: u32) -> usize {
        match self.run_starts.len() {
            1 => 0,
            _ => self.run_starts.partition_point(|&start| start <= slot) - 1,
        }
    }
}

impl<T, N: AstNode> BackMap<T, N> {
    /// The pointer of `slot`, whose kind is `kind` (not [`NO_SOURCE`]).
    #[inline]
    fn ptr(&self, slot: usize, kind: u16) -> AstPtr<N> {
        let range = TextRange::new(TextSize::from(self.starts[slot]), TextSize::from(self.ends[slot]));
        let raw = SyntaxNodePtr::from_parts(SyntaxKind::from(kind), range);
        AstPtr::try_from_raw(raw).expect("a kind of the pointer's type, as it was made or checked")
    }

    /// The syntax `id` is lowered from, `None` when it is lowered from none or is past the arena.
    #[inline]
    pub(crate) fn get(&self, id: Idx<T>) -> Option<InFile<AstPtr<N>>> {
        let slot = raw(id);
        let kind = *self.kinds.get(slot as usize)?;
        if kind == NO_SOURCE {
            return None;
        }
        let file = self.run_files[self.run_of(slot)];
        Some(InFile::new(file, self.ptr(slot as usize, kind)))
    }

    /// Every id with syntax and its syntax, by id, walking the runs as it goes.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Idx<T>, InFile<AstPtr<N>>)> + '_ {
        let mut run = 0;
        self.kinds.iter().enumerate().filter(|&(_, &kind)| kind != NO_SOURCE).map(
            move |(slot, &kind)| {
                while self.run_starts.get(run + 1).is_some_and(|&start| start as usize <= slot) {
                    run += 1;
                }
                (idx(slot as u32), InFile::new(self.run_files[run], self.ptr(slot, kind)))
            },
        )
    }

    /// The same map with the pointers into `mv.file` moved, each column written once: the runs
    /// of other files are copied as slices, only those of `mv.file` are walked. Moving keeps each
    /// id in its file, so the runs stay as they are.
    fn moved(&self, mv: &Move<'_>) -> Self {
        let len = self.len();
        let (mut kinds, mut starts, mut ends) =
            (Vec::with_capacity(len), Vec::with_capacity(len), Vec::with_capacity(len));
        let mut slot = 0;
        for (run, &file) in self.run_files.iter().enumerate() {
            let end = self.run_starts.get(run + 1).map_or(len, |&next| next as usize);
            if file != mv.file {
                kinds.extend_from_slice(&self.kinds[slot..end]);
                starts.extend_from_slice(&self.starts[slot..end]);
                ends.extend_from_slice(&self.ends[slot..end]);
                slot = end;
                continue;
            }
            for at in slot..end {
                let kind = self.kinds[at];
                if kind == NO_SOURCE {
                    kinds.push(NO_SOURCE);
                    starts.push(0);
                    ends.push(0);
                    continue;
                }
                let to = (mv.to)(self.ptr(at, kind).syntax_node_ptr());
                let ptr = AstPtr::<N>::try_from_raw(to).expect("moved to another kind");
                let range = ptr.text_range();
                kinds.push(u16::from(ptr.kind()));
                starts.push(range.start().into());
                ends.push(range.end().into());
            }
            slot = end;
        }
        // A map with no syntax has no runs: its slots are all copied here.
        kinds.extend_from_slice(&self.kinds[slot..]);
        starts.extend_from_slice(&self.starts[slot..]);
        ends.extend_from_slice(&self.ends[slot..]);
        BackMap {
            kinds: kinds.into_boxed_slice(),
            starts: starts.into_boxed_slice(),
            ends: ends.into_boxed_slice(),
            // clone: the moved map owns its runs, which a move leaves as they are
            run_starts: self.run_starts.clone(),
            // clone: as `run_starts`
            run_files: self.run_files.clone(),
            of: PhantomData,
        }
    }
}

/// The scratch [`BackPushes::freeze`] builds in, kept with the builder's pooled scratch.
#[derive(Default, Debug, PartialEq, Eq)]
pub(in crate::expr_store) struct FreezeScratch {
    /// The last push of each id, [`NO_PUSH`] for none.
    pushes: Vec<u32>,
    run_starts: Vec<u32>,
    run_files: Vec<HirFileId>,
}

const NO_PUSH: u32 = u32::MAX;

impl FreezeScratch {
    pub(in crate::expr_store) fn clear(&mut self) {
        self.pushes.clear();
        self.run_starts.clear();
        self.run_files.clear();
    }

    pub(in crate::expr_store) fn capacity(&self) -> usize {
        self.pushes.capacity() + self.run_starts.capacity() + self.run_files.capacity()
    }
}

/// A [`BackMap`] while lowering: each id and its syntax, pushed in lowering order, a later push
/// for an id replacing an earlier one. Named as `ArenaMap`'s methods are, so lowering writes it
/// as it wrote the map.
pub(crate) struct BackPushes<T, N: AstNode>(Vec<(Idx<T>, InFile<AstPtr<N>>)>);

impl<T, N: AstNode> Default for BackPushes<T, N> {
    fn default() -> Self {
        // alloc: none (an empty `Vec`); a pooled builder keeps its room across bodies
        BackPushes(Vec::new())
    }
}

impl<T, N: AstNode> PartialEq for BackPushes<T, N> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T, N: AstNode> Eq for BackPushes<T, N> {}

impl<T, N: AstNode> fmt::Debug for BackPushes<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.iter()).finish()
    }
}

impl<T, N: AstNode> BackPushes<T, N> {
    #[inline]
    pub(crate) fn insert(&mut self, id: Idx<T>, src: InFile<AstPtr<N>>) {
        self.0.push((id, src));
    }

    /// The syntax last pushed for `id`, by a search back from the last push: for the few reads
    /// lowering makes of a map it is writing (a parameter's pattern, right after it is pushed).
    pub(crate) fn get(&self, id: Idx<T>) -> Option<&InFile<AstPtr<N>>> {
        self.0.iter().rev().find(|(it, _)| *it == id).map(|(_, src)| src)
    }

    pub(in crate::expr_store) fn clear(&mut self) {
        self.0.clear();
    }

    pub(in crate::expr_store) fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// The map of an arena of `len` ids: one pass over the pushes to find each id's last, one
    /// over the ids to write the columns and runs, each column one allocation of its exact
    /// size. Leaves the pushes empty with their capacity. Panics on a push past `len`, which an
    /// arena of `len` did not allocate.
    pub(in crate::expr_store) fn freeze(
        &mut self,
        len: usize,
        scratch: &mut FreezeScratch,
    ) -> BackMap<T, N> {
        if len == 0 {
            assert!(self.0.is_empty(), "syntax for an id of an empty arena");
            return BackMap::default();
        }
        let FreezeScratch { pushes, run_starts, run_files } = scratch;
        pushes.clear();
        pushes.resize(len, NO_PUSH);
        for (at, &(id, _)) in self.0.iter().enumerate() {
            pushes[raw(id) as usize] = at as u32;
        }
        let (mut kinds, mut starts, mut ends) =
            (Vec::with_capacity(len), Vec::with_capacity(len), Vec::with_capacity(len));
        let mut file = None;
        for (slot, &at) in pushes.iter().enumerate() {
            if at == NO_PUSH {
                kinds.push(NO_SOURCE);
                starts.push(0);
                ends.push(0);
                continue;
            }
            let src = &self.0[at as usize].1;
            let range = src.value.text_range();
            kinds.push(u16::from(src.value.kind()));
            starts.push(u32::from(range.start()));
            ends.push(u32::from(range.end()));
            if file != Some(src.file_id) {
                run_starts.push(if file.is_none() { 0 } else { slot as u32 });
                run_files.push(src.file_id);
                file = Some(src.file_id);
            }
        }
        pushes.clear();
        self.0.clear();
        BackMap {
            kinds: kinds.into_boxed_slice(),
            starts: starts.into_boxed_slice(),
            ends: ends.into_boxed_slice(),
            run_starts: take_exact(run_starts),
            run_files: take_exact(run_files),
            of: PhantomData,
        }
    }
}

/// The patterns that define each binding, as CSR: binding `b`'s are `pats[starts[b]..starts[b +
/// 1]]`, in the order lowering added them. `starts` has one entry more than the store has
/// bindings, and is empty when it has none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BindingPats {
    pub(in crate::expr_store) starts: Box<[u32]>,
    pub(in crate::expr_store) pats: Box<[PatId]>,
}

impl BindingPats {
    /// The lists of `pushed` for a store of `bindings` bindings, by a counting sort (stable, so
    /// each binding's patterns stay in push order): one pass to count, one to sum, one to place.
    /// Leaves `pushed` empty with its capacity.
    pub(in crate::expr_store) fn from_pushed(
        pushed: &mut Vec<(BindingId, PatId)>,
        bindings: usize,
    ) -> Self {
        if bindings == 0 {
            assert!(pushed.is_empty(), "a pattern for a binding of a store without bindings");
            return BindingPats::default();
        }
        // alloc: the output offsets, one exact allocation
        let mut starts = vec![0u32; bindings + 1];
        for &(binding, _) in pushed.iter() {
            starts[raw(binding) as usize + 1] += 1;
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        // alloc: the output patterns, one exact allocation
        let mut pats = vec![idx::<Pat<'static>>(0); pushed.len()];
        // Each binding's start is its cursor; once placed, it is where the next one starts.
        for &(binding, pat) in pushed.iter() {
            let cursor = &mut starts[raw(binding) as usize];
            pats[*cursor as usize] = pat;
            *cursor += 1;
        }
        for at in (1..starts.len()).rev() {
            starts[at] = starts[at - 1];
        }
        starts[0] = 0;
        pushed.clear();
        BindingPats { starts: starts.into_boxed_slice(), pats: pats.into_boxed_slice() }
    }

    /// The patterns of `binding`, empty for one past the store's.
    #[inline]
    pub(crate) fn get(&self, binding: BindingId) -> &[PatId] {
        let at = raw(binding) as usize;
        match (self.starts.get(at), self.starts.get(at + 1)) {
            (Some(&start), Some(&end)) => &self.pats[start as usize..end as usize],
            _ => &[],
        }
    }
}

/// The captures of a `format_args!()`: its hygiene and each implicitly captured name.
pub(crate) type FormatCaptures = (HygieneId, Vec<(syntax::TextRange, Name)>);
/// The captures of each template piece of an `asm!()`.
pub(crate) type AsmCaptures = Vec<Vec<(syntax::TextRange, usize)>>;
/// Where an implicit capture is written: its template literal and the offset inside it.
pub(crate) type CaptureSource = InFile<(ExprPtr, TextRange)>;
/// A [`FormatTemplate`] while lowering: each map pushed to in the order it is made.
pub(crate) type PendingTemplate = FormatTemplate<
    Vec<(ExprId, FormatCaptures)>,
    Vec<(ExprId, AsmCaptures)>,
    Vec<(ExprId, CaptureSource)>,
>;

/// The maps of a store's templates: pushed to while lowering ([`super::ExpressionStoreBuilder`]),
/// tables once finished.
#[derive(Default, Debug, Eq, PartialEq)]
pub(crate) struct FormatTemplate<
    F = IdTable<ExprId, FormatCaptures>,
    A = IdTable<ExprId, AsmCaptures>,
    C = IdTable<ExprId, CaptureSource>,
> {
    /// A map from `format_args!()` expressions to their captures.
    pub(in crate::expr_store) format_args_to_captures: F,
    /// A map from `asm!()` expressions to their captures.
    pub(in crate::expr_store) asm_to_captures: A,
    /// A map from desugared expressions of implicit captures to their source.
    ///
    /// The value stored for each capture is its template literal and offset inside it. The template literal
    /// is from the `format_args[_nl]!()` macro and so needs to be mapped up once to go to the user-written
    /// template.
    pub(in crate::expr_store) implicit_capture_to_source: C,
}

#[derive(Debug, Eq, Default)]
pub(in crate::expr_store) struct ExpressionOnlySourceMap {
    // AST expressions can create patterns in destructuring assignments. Therefore, `ExprSource` can also map
    // to `PatId`, and `PatId` can also map to `ExprSource` (the other way around is unaffected).
    pub(in crate::expr_store) expr_map: SourceTable<ExprSource, ExprOrPatIdPacked>,
    pub(in crate::expr_store) expr_map_back: ExprBackMap,

    pub(in crate::expr_store) pat_map: SourceTable<PatSource, ExprOrPatIdPacked>,
    pub(in crate::expr_store) pat_map_back: PatBackMap,

    pub(in crate::expr_store) label_map: SourceTable<LabelSource, LabelId>,
    pub(in crate::expr_store) label_map_back: LabelBackMap,

    pub(in crate::expr_store) binding_definitions: BindingPats,

    /// We don't create explicit nodes for record fields (`S { record_field: 92 }`).
    /// Instead, we use id of expression (`92`) to identify the field.
    pub(in crate::expr_store) field_map_back: IdTable<ExprId, FieldSource>,
    pub(in crate::expr_store) pat_field_map_back: IdTable<PatId, PatFieldSource>,

    pub(in crate::expr_store) template_map: Option<Box<FormatTemplate>>,

    pub(in crate::expr_store) expansions: SourceTable<InFile<MacroCallPtr>, MacroCallId>,

    /// Diagnostics accumulated during lowering. These contain `AstPtr`s and so are stored in
    /// the source map (since they're just as volatile).
    //
    // We store diagnostics on the `ExpressionOnlySourceMap` because diagnostics are rare (except
    // maybe for cfgs, and they are also not common in type places).
    pub(in crate::expr_store) diagnostics: thin_vec::ThinVec<ExpressionStoreDiagnostics>,
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
    pub(in crate::expr_store) expr_only: Option<Box<ExpressionOnlySourceMap>>,

    pub(in crate::expr_store) types_map_back: TypeBackMap,
    pub(in crate::expr_store) types_map: SourceTable<TypeSource, TypeRefId>,

    pub(in crate::expr_store) lifetime_map_back: LifetimeBackMap,
    pub(in crate::expr_store) lifetime_map: SourceTable<LifetimeSource, LifetimeRefId>,
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
    // dyn: one indirect call per pointer moved, beside the tree search each `to` makes
    pub(crate) to: &'a dyn Fn(SyntaxNodePtr) -> SyntaxNodePtr,
}

impl Move<'_> {
    pub(in crate::expr_store) fn ptr<N: AstNode>(&self, src: InFile<AstPtr<N>>) -> InFile<AstPtr<N>> {
        if src.file_id != self.file {
            return src;
        }
        let moved = (self.to)(src.value.syntax_node_ptr());
        InFile::new(src.file_id, AstPtr::try_from_raw(moved).expect("moved to another kind"))
    }

    /// Moving pointers can change their order, so the table is sorted again.
    fn forth<N: AstNode, V: Copy>(
        &self,
        map: &SourceTable<InFile<AstPtr<N>>, V>,
    ) -> SourceTable<InFile<AstPtr<N>>, V> {
        // alloc: the moved pairs, sorted in place and copied once into the exact table
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
        let types_map_back = self.types_map_back.moved(mv);
        let lifetime_map_back = self.lifetime_map_back.moved(mv);
        let expr_only = self.expr_only.as_deref().map(|it| {
            let template_map = it.template_map.as_deref().map(|it| FormatTemplate {
                // clone: the moved map owns its tables; captures hold no pointers to move
                format_args_to_captures: it.format_args_to_captures.clone(),
                // clone: as `format_args_to_captures`
                asm_to_captures: it.asm_to_captures.clone(),
                implicit_capture_to_source: it.implicit_capture_to_source.map(|_, src| {
                    let (ptr, range) = src.value;
                    let ptr = mv.ptr(InFile::new(src.file_id, ptr)).value;
                    InFile::new(src.file_id, (ptr, range))
                }),
            });
            // alloc: the moved map's one box, as the map it moves has
            Box::new(ExpressionOnlySourceMap {
                expr_map: mv.forth(&it.expr_map),
                expr_map_back: it.expr_map_back.moved(mv),
                pat_map: mv.forth(&it.pat_map),
                pat_map_back: it.pat_map_back.moved(mv),
                label_map: mv.forth(&it.label_map),
                label_map_back: it.label_map_back.moved(mv),
                // clone: ids only, which a move leaves as they are
                binding_definitions: it.binding_definitions.clone(),
                field_map_back: it.field_map_back.map(|_, &src| mv.ptr(src)),
                pat_field_map_back: it.pat_field_map_back.map(|_, &src| mv.ptr(src)),
                template_map: template_map.map(Box::new),
                expansions: mv.forth(&it.expansions),
                // alloc: diagnostics are rare; empty, this allocates nothing
                diagnostics: it.diagnostics.iter().map(|it| it.moved(mv)).collect(),
            })
        });
        // alloc: the forward pairs, sorted in place and copied once into the exact table
        let mut types_map: Vec<_> = types_map_back.iter().map(|(id, src)| (src, id)).collect();
        let mut lifetime_map: Vec<_> =
            // alloc: as `types_map`
            lifetime_map_back.iter().map(|(id, src)| (src, id)).collect();
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
            // clone: error path
            InactiveCode { node, cfg } => InactiveCode { node: mv.raw(*node), cfg: cfg.clone() },
            UnresolvedMacroCall { node, path } => {
                // clone: error path
                UnresolvedMacroCall { node: mv.ptr(*node), path: path.clone() }
            }
            UnreachableLabel { node, name } => {
                // clone: error path
                UnreachableLabel { node: mv.ptr(*node), name: name.clone() }
            }
            AwaitOutsideOfAsync { node, location } => {
                // clone: error path
                AwaitOutsideOfAsync { node: mv.ptr(*node), location: location.clone() }
            }
            UndeclaredLabel { node, name } => {
                // clone: error path
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
        self.expr_or_synthetic()?.expr_map_back.get(expr).ok_or(SyntheticSyntax)
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
        self.expr_or_synthetic()?.pat_map_back.get(pat).ok_or(SyntheticSyntax)
    }

    pub fn node_pat(&self, node: InFile<&ast::Pat>) -> Option<ExprOrPatId> {
        self.expr_only()?
            .pat_map
            .get(&node.map(AstPtr::new))
            .cloned()
            .map(ExprOrPatIdPacked::unpack)
    }

    pub fn type_syntax(&self, id: TypeRefId) -> Result<TypeSource, SyntheticSyntax> {
        self.types_map_back.get(id).ok_or(SyntheticSyntax)
    }

    pub fn node_type(&self, node: InFile<&ast::Type>) -> Option<TypeRefId> {
        self.types_map.get(&node.map(AstPtr::new)).cloned()
    }

    #[track_caller]
    pub fn label_syntax(&self, label: LabelId) -> LabelSource {
        self.assert_expr_only().label_map_back.get(label).expect("no syntax for this label")
    }

    pub fn patterns_for_binding(&self, binding: BindingId) -> &[PatId] {
        self.assert_expr_only().binding_definitions.get(binding)
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
pub(in crate::expr_store) mod tests {
    use blaze_ids::rng::SplitMix64;
    use rustc_hash::FxHashMap;
    use salsa::plumbing::FromId;
    use smallvec::SmallVec;
    use syntax::{SyntaxKind, TextSize};

    use super::*;
    use crate::{
        expr_store::ExpressionStoreBuilder,
        hir::{Binding, BindingAnnotation},
    };

    /// A pointer to a node of `kind` at `start..end` in file (or, when `in_macro`, macro file)
    /// `file`.
    pub(in crate::expr_store) fn src<N: AstNode>(
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

    const EXPR_KINDS: [SyntaxKind; 3] =
        [SyntaxKind::PATH_EXPR, SyntaxKind::CALL_EXPR, SyntaxKind::LITERAL];

    /// A pointer from a small domain, so that pushes repeat keys.
    fn seeded_src(rng: &mut SplitMix64) -> ExprSource {
        let start = rng.below(8) as u32;
        let end = start + rng.below(4) as u32;
        let kind = EXPR_KINDS[rng.below(3) as usize];
        src(rng.below(3) as u32, rng.below(2) == 1, kind, start, end)
    }

    /// What a map of `pushes` answers for `id`: the last push for it, by a linear search.
    fn last_push<K: PartialEq + Copy, V: Copy>(pushes: &[(K, V)], id: K) -> Option<V> {
        pushes.iter().rev().find(|(it, _)| *it == id).map(|&(_, value)| value)
    }

    #[test]
    fn a_source_table_answers_as_a_hash_map_does() {
        for seed in 0..64 {
            let mut rng = SplitMix64::new(seed);
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

    /// Seeded pushes for an arena of `len` ids: ids pushed twice, ids never pushed, runs of ids
    /// in one file, macro files between.
    fn seeded_back_pushes(
        rng: &mut SplitMix64,
        len: u32,
    ) -> Vec<(ExprId, InFile<AstPtr<Either<ast::Expr, ast::Pat>>>)> {
        let mut pushes = Vec::new();
        let mut file = (0, false);
        for _ in 0..rng.below(u64::from(len) * 2 + 1) {
            if rng.below(8) == 0 {
                file = (rng.below(3) as u32, rng.below(2) == 1);
            }
            let id = idx(rng.below(u64::from(len)) as u32);
            let start = rng.below(64) as u32;
            let end = start + rng.below(16) as u32;
            let kind = [SyntaxKind::PATH_EXPR, SyntaxKind::WILDCARD_PAT][rng.below(2) as usize];
            pushes.push((id, src(file.0, file.1, kind, start, end)));
        }
        pushes
    }

    /// Checks `map` against `pushes` for an arena of `len`: each id, the order of `iter`, and
    /// that the runs are canonical.
    fn assert_back_map_is(
        map: &ExprBackMap,
        pushes: &[(ExprId, ExprOrPatSource)],
        len: u32,
    ) {
        assert_eq!(map.len(), len as usize);
        for id in (0..len + 3).map(idx::<Expr<'static>>) {
            assert_eq!(map.get(id), last_push(pushes, id), "the last push for {id:?}");
        }
        let model: Vec<_> =
            (0..len).map(idx).filter_map(|id| Some((id, last_push(pushes, id)?))).collect();
        assert_eq!(map.iter().collect::<Vec<_>>(), model, "every id with syntax, by id");
        assert!(map.run_files.windows(2).all(|pair| pair[0] != pair[1]), "a run per change of file");
        assert_eq!(map.run_starts.first().copied(), (!model.is_empty()).then_some(0));
        for (run, &start) in map.run_starts.iter().enumerate().skip(1) {
            assert_ne!(map.kinds[start as usize], NO_SOURCE, "run {run} starts at an id with syntax");
        }
        for (slot, &kind) in map.kinds.iter().enumerate() {
            if kind == NO_SOURCE {
                assert_eq!((map.starts[slot], map.ends[slot]), (0, 0), "no syntax is zeros");
            }
        }
    }

    #[test]
    fn a_back_map_answers_as_its_pushes_do() {
        let mut scratch = FreezeScratch::default();
        for seed in 0..64 {
            let mut rng = SplitMix64::new(seed);
            let len = rng.below(200) as u32 + 1;
            let pushes = seeded_back_pushes(&mut rng, len);
            let mut pushed = BackPushes::default();
            for &(id, src) in &pushes {
                pushed.insert(id, src);
                assert_eq!(pushed.get(id), Some(&src), "a read while lowering sees the last push");
            }
            let room = pushed.capacity();
            let map: ExprBackMap = pushed.freeze(len as usize, &mut scratch);
            assert!(pushed.0.is_empty() && pushed.capacity() == room, "the scratch keeps its room");
            assert_back_map_is(&map, &pushes, len);

            let again = BackMap::from_columns(
                map.kinds.clone(),
                map.starts.clone(),
                map.ends.clone(),
                map.run_starts.clone(),
                map.run_files.clone(),
            );
            assert_eq!(again, map);
        }
        let mut empty = BackPushes::<Expr<'static>, Either<ast::Expr, ast::Pat>>::default();
        assert_eq!(empty.freeze(0, &mut scratch), BackMap::default());
        let none = empty.freeze(5, &mut scratch);
        assert!(none.run_files.is_empty() && none.iter().next().is_none() && none.len() == 5);
    }

    #[test]
    fn moving_a_back_map_moves_its_files_pointers_only() {
        let mut scratch = FreezeScratch::default();
        let to = |ptr: SyntaxNodePtr| {
            SyntaxNodePtr::from_parts(ptr.kind(), ptr.text_range() + TextSize::from(100))
        };
        for seed in 0..32 {
            let mut rng = SplitMix64::new(seed);
            let len = rng.below(100) as u32 + 1;
            let pushes = seeded_back_pushes(&mut rng, len);
            let mut pushed = BackPushes::default();
            pushes.iter().for_each(|&(id, src)| pushed.insert(id, src));
            let map: ExprBackMap = pushed.freeze(len as usize, &mut scratch);
            let file = src::<ast::Expr>(1, false, SyntaxKind::PATH_EXPR, 0, 0).file_id;
            let mv = Move { file, to: &to };
            let moved_pushes: Vec<_> = pushes.iter().map(|&(id, src)| (id, mv.ptr(src))).collect();
            assert_back_map_is(&map.moved(&mv), &moved_pushes, len);
        }
    }

    #[test]
    fn binding_patterns_are_their_pushes_in_order() {
        for seed in 0..64 {
            let mut rng = SplitMix64::new(seed);
            let bindings = rng.below(50) as u32;
            let mut pushed = Vec::new();
            if bindings > 0 {
                for _ in 0..rng.below(u64::from(bindings) * 3) {
                    let binding: BindingId = idx(rng.below(u64::from(bindings)) as u32);
                    let pat: PatId = idx(rng.below(500) as u32);
                    pushed.push((binding, pat));
                }
            }
            let model = pushed.clone();
            let room = pushed.capacity();
            let lists = BindingPats::from_pushed(&mut pushed, bindings as usize);
            assert!(pushed.is_empty() && pushed.capacity() == room, "the scratch keeps its room");
            assert_eq!(lists.starts.len(), if bindings == 0 { 0 } else { bindings as usize + 1 });
            assert!(lists.starts.windows(2).all(|pair| pair[0] <= pair[1]), "sorted offsets");
            for binding in (0..bindings + 2).map(idx::<Binding>) {
                let want: Vec<PatId> =
                    model.iter().filter(|&&(it, _)| it == binding).map(|&(_, pat)| pat).collect();
                assert_eq!(lists.get(binding), &want[..], "the patterns of {binding:?}, in order");
            }
        }
    }

    fn binding(builder: &mut ExpressionStoreBuilder) -> BindingId {
        builder.bindings.alloc(Binding {
            name: Name::missing(),
            mode: BindingAnnotation::Unannotated,
            problems: None,
            hygiene: HygieneId::ROOT,
        })
    }

    /// A store lowered by hand: expressions, patterns, labels, types and lifetimes, some pushed
    /// twice, some with no syntax, some in a macro file; and the same pushes kept as pairs.
    #[test]
    fn every_accessor_answers_as_its_pushes_do() {
        let mut builder = ExpressionStoreBuilder::new();
        builder.inference_roots = Some(SmallVec::new());
        let exprs: Vec<ExprId> = (0..6).map(|_| builder.alloc_expr(Expr::Missing)).collect();
        let pats: Vec<PatId> = (0..4).map(|_| builder.alloc_pat(Pat::Wild)).collect();
        let labels: Vec<LabelId> =
            (0..3).map(|_| builder.labels.alloc(Label { name: Name::missing() })).collect();
        let types: Vec<TypeRefId> =
            (0..3).map(|_| builder.alloc_type_ref(TypeRef::Never)).collect();
        let lifetimes: Vec<LifetimeRefId> =
            (0..2).map(|_| builder.lifetimes.alloc(LifetimeRef::Static)).collect();
        let bindings: Vec<BindingId> = (0..3).map(|_| binding(&mut builder)).collect();

        let (call, path) = (SyntaxKind::CALL_EXPR, SyntaxKind::PATH_EXPR);
        let wild = SyntaxKind::WILDCARD_PAT;
        let expr_back = [
            (exprs[0], src(0, false, call, 0, 4)),
            (exprs[1], src(0, false, path, 5, 6)),
            (exprs[3], src(0, true, path, 0, 1)),
            (exprs[1], src(0, false, path, 5, 7)),
            (exprs[4], src(0, false, wild, 9, 10)),
        ];
        let pat_back = [
            (pats[0], src(0, false, wild, 11, 12)),
            (pats[2], src(0, true, path, 2, 3)),
            (pats[0], src(0, false, wild, 11, 13)),
        ];
        let label_back = [
            (labels[0], src(0, false, SyntaxKind::LABEL, 20, 22)),
            (labels[2], src(0, false, SyntaxKind::BLOCK_EXPR, 23, 30)),
        ];
        let type_back = [
            (types[0], src(0, false, SyntaxKind::NEVER_TYPE, 31, 32)),
            (types[2], src(0, true, SyntaxKind::TUPLE_TYPE, 4, 6)),
        ];
        let lifetime_back = [(lifetimes[1], src(0, false, SyntaxKind::LIFETIME, 33, 35))];
        let binding_pats =
            [(bindings[0], pats[0]), (bindings[2], pats[3]), (bindings[0], pats[1])];
        for &(id, src) in &expr_back {
            builder.expr_map_back.insert(id, src);
        }
        for &(id, src) in &pat_back {
            builder.pat_map_back.insert(id, src);
        }
        for &(id, src) in &label_back {
            builder.label_map_back.insert(id, src);
        }
        for &(id, src) in &type_back {
            builder.types_map_back.insert(id, src);
            builder.types_map.push((src, id));
        }
        for &(id, src) in &lifetime_back {
            builder.lifetime_map_back.insert(id, src);
            builder.lifetime_map.push((src, id));
        }
        builder.binding_definitions.extend(binding_pats);
        let node = src::<ast::Expr>(0, false, call, 0, 4);
        builder.expr_map.push((node, exprs[0].into()));
        builder.expr_map.push((node, exprs[2].into()));
        let (_, map) = builder.finish();

        for &id in &exprs {
            assert_eq!(map.expr_syntax(id).ok(), last_push(&expr_back, id), "{id:?}");
            let either = map.expr_or_pat_syntax(ExprOrPatId::ExprId(id)).ok();
            assert_eq!(either, last_push(&expr_back, id));
        }
        for &id in &pats {
            assert_eq!(map.pat_syntax(id).ok(), last_push(&pat_back, id), "{id:?}");
        }
        assert_eq!(map.label_syntax(labels[0]), label_back[0].1);
        assert_eq!(map.label_syntax(labels[2]), label_back[1].1);
        for &id in &types {
            assert_eq!(map.type_syntax(id).ok(), last_push(&type_back, id), "{id:?}");
        }
        for &(id, src) in &type_back {
            assert_eq!(map.types_map.get(&src), Some(&id));
        }
        for &id in &lifetimes {
            assert_eq!(map.lifetime_map_back.get(id), last_push(&lifetime_back, id), "{id:?}");
        }
        for &id in &bindings {
            let want: Vec<PatId> =
                binding_pats.iter().filter(|&&(it, _)| it == id).map(|&(_, pat)| pat).collect();
            assert_eq!(map.patterns_for_binding(id), &want[..], "{id:?}");
        }
        let expr_only = map.expr_only.as_deref().expect("a store with expressions");
        assert_eq!(expr_only.expr_map.get(&node), Some(&exprs[2].into()), "the last push wins");
        assert_eq!(expr_only.expr_map_back.len(), exprs.len(), "a slot for every expression");
        assert_eq!(expr_only.pat_map_back.len(), pats.len());
        assert_eq!(expr_only.label_map_back.len(), labels.len());
        assert_eq!(map.types_map_back.len(), types.len());
        assert_eq!(map.lifetime_map_back.len(), lifetimes.len());
        assert_eq!(expr_only.binding_definitions.starts.len(), bindings.len() + 1);
        assert_eq!(expr_only.expr_map_back.run_files.len(), 3, "file, macro file, file");
    }
}
