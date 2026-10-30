//! The columns an [`InferenceResult`](super::InferenceResult) is made of
//! (docs/v2/plan/design/data.md): what is known of each expression, pattern or binding sits in
//! a column indexed by its arena index, lists are CSR ranges into one flat array, sets are
//! bitsets, and what few nodes have (closures, opaque types, `asm!` symbols) sits in a sorted
//! side table.
//!
//! Every column is canonical: two results that say the same thing are the same columns,
//! whatever order inference wrote them in, so derived `Eq` and `Hash` agree with what the
//! result means.

use std::{
    fmt,
    marker::PhantomData,
    ops::{Deref, DerefMut, Index},
};

use hir_def::{
    FieldId, TupleFieldId,
    hir::{ExprId, ExprOrPatIdPacked, PatId},
};
use la_arena::{ArenaMap, Idx, RawIdx};
use rustc_ast_ir::Mutability;
use rustc_hash::FxHashMap;

use crate::{
    infer::{BindingMode, ByRef},
    next_solver::StoredTy,
};

/// No value for this key, in a column of places.
pub(crate) const NONE: u32 = u32::MAX;

/// What a dense column is indexed by: an arena index.
pub trait DenseKey: Copy {
    fn to_index(self) -> usize;
    fn from_index(index: usize) -> Self;
}

impl<T> DenseKey for Idx<T> {
    #[inline]
    fn to_index(self) -> usize {
        self.into_raw().into_u32() as usize
    }

    #[inline]
    fn from_index(index: usize) -> Self {
        Idx::from_raw(RawIdx::from_u32(u32::try_from(index).expect("an arena of 2^32 nodes")))
    }
}

fn place(index: usize) -> u32 {
    let place = u32::try_from(index).expect("a column of 2^32 values");
    assert!(place != NONE, "a column of 2^32 values");
    place
}

/// The values of `entries` by key, in key order.
fn dense<K: DenseKey, V>(entries: impl IntoIterator<Item = (K, V)>) -> Vec<Option<V>> {
    let mut dense: Vec<Option<V>> = Vec::new();
    for (key, value) in entries {
        let index = key.to_index();
        if index >= dense.len() {
            dense.resize_with(index + 1, || None);
        }
        dense[index] = Some(value);
    }
    dense
}

/// A type per key, dictionary-encoded: `at[key]` is where the key's type is in `tys`
/// ([`NONE`] for a key without one), each distinct type in `tys` once, in the order of the
/// first key that has it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TyColumn<K> {
    at: Box<[u32]>,
    tys: Box<[StoredTy]>,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for TyColumn<K> {
    fn default() -> Self {
        TyColumn { at: Box::default(), tys: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey> TyColumn<K> {
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, StoredTy)>) -> Self {
        let dense = dense(entries);
        let mut by_ty: FxHashMap<StoredTy, u32> = FxHashMap::default();
        let mut tys = Vec::new();
        let at = dense
            .into_iter()
            .map(|ty| match ty {
                None => NONE,
                Some(ty) => *by_ty.entry(ty).or_insert_with_key(|ty| {
                    tys.push(ty.clone());
                    place(tys.len() - 1)
                }),
            })
            .collect();
        TyColumn { at, tys: tys.into_boxed_slice(), key: PhantomData }
    }

    /// The column of `at` and `tys` as [`Self::at`] and [`Self::tys`] give them, refused when
    /// it is not one [`Self::from_entries`] makes.
    pub(crate) fn from_parts(at: Box<[u32]>, tys: Box<[StoredTy]>) -> Result<Self, &'static str> {
        if at.last() == Some(&NONE) {
            return Err("a type column that ends in a key without a type");
        }
        let mut next = 0u32;
        for &place in &at {
            match place {
                NONE => {}
                _ if place < next => {}
                _ if place == next && (place as usize) < tys.len() => next += 1,
                _ => return Err("a type column out of its dictionary's order"),
            }
        }
        if next as usize != tys.len() {
            return Err("a type in a column's dictionary that no key has");
        }
        Ok(TyColumn { at, tys, key: PhantomData })
    }

    #[inline]
    pub fn get(&self, key: K) -> Option<&StoredTy> {
        match self.at.get(key.to_index()) {
            Some(&place) if place != NONE => Some(&self.tys[place as usize]),
            _ => None,
        }
    }

    pub fn contains_idx(&self, key: K) -> bool {
        self.at.get(key.to_index()).is_some_and(|&place| place != NONE)
    }

    /// Each key with a type, in key order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &StoredTy)> + '_ {
        (self.at.iter().enumerate())
            .filter(|&(_, &place)| place != NONE)
            .map(|(index, &place)| (K::from_index(index), &self.tys[place as usize]))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &StoredTy> + '_ {
        self.iter().map(|(_, ty)| ty)
    }

    pub fn is_empty(&self) -> bool {
        self.at.iter().all(|&place| place == NONE)
    }

    /// Where each key's type is in [`Self::tys`], [`NONE`] for a key without one.
    pub fn at(&self) -> &[u32] {
        &self.at
    }

    /// Each distinct type once.
    pub fn tys(&self) -> &[StoredTy] {
        &self.tys
    }
}

impl<K: DenseKey> Index<K> for TyColumn<K> {
    type Output = StoredTy;

    #[track_caller]
    fn index(&self, key: K) -> &StoredTy {
        self.get(key).expect("no type was inferred for this node")
    }
}

impl<K: DenseKey + fmt::Debug> fmt::Debug for TyColumn<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A row per key with one: `at[key]` is the key's place in `rows`, the rows in key order.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IndexColumn<K, V> {
    at: Box<[u32]>,
    rows: Box<[V]>,
    key: PhantomData<fn() -> K>,
}

impl<K, V> Default for IndexColumn<K, V> {
    fn default() -> Self {
        IndexColumn { at: Box::default(), rows: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey, V> IndexColumn<K, V> {
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut rows = Vec::new();
        let at = dense(entries)
            .into_iter()
            .map(|row| match row {
                None => NONE,
                Some(row) => {
                    rows.push(row);
                    place(rows.len() - 1)
                }
            })
            .collect();
        IndexColumn { at, rows: rows.into_boxed_slice(), key: PhantomData }
    }

    /// The column of `at` and `rows` as [`Self::at`] and [`Self::rows`] give them, refused
    /// when it is not one [`Self::from_entries`] makes.
    pub(crate) fn from_parts(at: Box<[u32]>, rows: Box<[V]>) -> Result<Self, &'static str> {
        if at.last() == Some(&NONE) {
            return Err("a row column that ends in a key without a row");
        }
        let mut next = 0u32;
        for &place in &at {
            match place {
                NONE => {}
                _ if place == next => next += 1,
                _ => return Err("a row column out of key order"),
            }
        }
        if next as usize != rows.len() {
            return Err("a row column with a row no key has");
        }
        Ok(IndexColumn { at, rows, key: PhantomData })
    }

    /// Where each key's row is in [`Self::rows`], [`NONE`] for a key without one.
    pub fn at(&self) -> &[u32] {
        &self.at
    }

    /// The rows, in key order.
    pub fn rows(&self) -> &[V] {
        &self.rows
    }

    #[inline]
    pub fn get(&self, key: K) -> Option<&V> {
        match self.at.get(key.to_index()) {
            Some(&place) if place != NONE => Some(&self.rows[place as usize]),
            _ => None,
        }
    }

    pub fn contains_idx(&self, key: K) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &V)> + '_ {
        (self.at.iter().enumerate())
            .filter(|&(_, &place)| place != NONE)
            .map(|(index, &place)| (K::from_index(index), &self.rows[place as usize]))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl<K: DenseKey + fmt::Debug, V: fmt::Debug> fmt::Debug for IndexColumn<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// The same column of expressions and of patterns, keyed by either.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ExprOrPatColumn<V> {
    pub(crate) exprs: IndexColumn<ExprId, V>,
    pub(crate) pats: IndexColumn<PatId, V>,
}

impl<V> Default for ExprOrPatColumn<V> {
    fn default() -> Self {
        ExprOrPatColumn { exprs: IndexColumn::default(), pats: IndexColumn::default() }
    }
}

impl<V> ExprOrPatColumn<V> {
    #[inline]
    pub fn get(&self, node: ExprOrPatIdPacked) -> Option<&V> {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.get(expr),
            (None, Some(pat)) => self.pats.get(pat),
            (None, None) => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (ExprOrPatIdPacked, &V)> + '_ {
        let exprs = self.exprs.iter().map(|(expr, value)| (ExprOrPatIdPacked::from(expr), value));
        exprs.chain(self.pats.iter().map(|(pat, value)| (ExprOrPatIdPacked::from(pat), value)))
    }
}

impl<V: fmt::Debug> fmt::Debug for ExprOrPatColumn<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A map of expressions or patterns to values while inference writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExprOrPatMap<V> {
    exprs: ArenaMap<ExprId, V>,
    pats: ArenaMap<PatId, V>,
}

impl<V> Default for ExprOrPatMap<V> {
    fn default() -> Self {
        ExprOrPatMap { exprs: ArenaMap::default(), pats: ArenaMap::default() }
    }
}

impl<V> ExprOrPatMap<V> {
    pub(crate) fn insert(&mut self, node: ExprOrPatIdPacked, value: V) -> Option<V> {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.insert(expr, value),
            (None, Some(pat)) => self.pats.insert(pat, value),
            (None, None) => None,
        }
    }

    pub(crate) fn get(&self, node: ExprOrPatIdPacked) -> Option<&V> {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.get(expr),
            (None, Some(pat)) => self.pats.get(pat),
            (None, None) => None,
        }
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.exprs.values_mut().chain(self.pats.values_mut())
    }

    pub(crate) fn freeze(self) -> ExprOrPatColumn<V> {
        ExprOrPatColumn {
            exprs: IndexColumn::from_entries(self.exprs),
            pats: IndexColumn::from_entries(self.pats),
        }
    }
}

/// What a field access resolves to.
const FIELD_NONE: u8 = 0;
const FIELD: u8 = 1;
const TUPLE_FIELD: u8 = 2;

/// The field each field access expression resolves to: a tag per expression ([`FIELD_NONE`],
/// [`FIELD`] or [`TUPLE_FIELD`]) and a word, its place in `fields` or `tuple_fields`.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct FieldColumn {
    tags: Box<[u8]>,
    data: Box<[u32]>,
    fields: Box<[FieldId]>,
    tuple_fields: Box<[TupleFieldId]>,
}

impl FieldColumn {
    pub(crate) fn from_entries(
        entries: impl IntoIterator<Item = (ExprId, either::Either<FieldId, TupleFieldId>)>,
    ) -> Self {
        let dense = dense(entries);
        let (mut fields, mut tuple_fields) = (Vec::new(), Vec::new());
        let mut tags = Vec::with_capacity(dense.len());
        let mut data = Vec::with_capacity(dense.len());
        for field in dense {
            let (tag, word) = match field {
                None => (FIELD_NONE, 0),
                Some(either::Either::Left(field)) => {
                    fields.push(field);
                    (FIELD, place(fields.len() - 1))
                }
                Some(either::Either::Right(field)) => {
                    tuple_fields.push(field);
                    (TUPLE_FIELD, place(tuple_fields.len() - 1))
                }
            };
            tags.push(tag);
            data.push(word);
        }
        FieldColumn {
            tags: tags.into_boxed_slice(),
            data: data.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
            tuple_fields: tuple_fields.into_boxed_slice(),
        }
    }

    /// The column of the parts [`Self::tags`], [`Self::data`], [`Self::fields`] and
    /// [`Self::tuple_fields`] give, refused when it is not one [`Self::from_entries`] makes.
    pub(crate) fn from_parts(
        tags: Box<[u8]>,
        data: Box<[u32]>,
        fields: Box<[FieldId]>,
        tuple_fields: Box<[TupleFieldId]>,
    ) -> Result<Self, &'static str> {
        if tags.len() != data.len() {
            return Err("a field column whose tags and words differ in length");
        }
        if tags.last() == Some(&FIELD_NONE) {
            return Err("a field column that ends in an expression without a field");
        }
        let (mut next_field, mut next_tuple_field) = (0u32, 0u32);
        for (&tag, &word) in tags.iter().zip(&data) {
            let next = match tag {
                FIELD_NONE if word == 0 => continue,
                FIELD => &mut next_field,
                TUPLE_FIELD => &mut next_tuple_field,
                _ => return Err("a field column entry that is none of its kinds"),
            };
            if word != *next {
                return Err("a field column out of expression order");
            }
            *next += 1;
        }
        if next_field as usize != fields.len() || next_tuple_field as usize != tuple_fields.len() {
            return Err("a field column with a field no expression has");
        }
        Ok(FieldColumn { tags, data, fields, tuple_fields })
    }

    /// Per expression, none, a field or a tuple field.
    pub fn tags(&self) -> &[u8] {
        &self.tags
    }

    /// Per expression, its field's place in [`Self::fields`] or [`Self::tuple_fields`].
    pub fn data(&self) -> &[u32] {
        &self.data
    }

    pub fn fields(&self) -> &[FieldId] {
        &self.fields
    }

    pub fn tuple_fields(&self) -> &[TupleFieldId] {
        &self.tuple_fields
    }

    #[inline]
    pub fn get(&self, expr: ExprId) -> Option<either::Either<FieldId, TupleFieldId>> {
        let index = expr.to_index();
        match self.tags.get(index).copied()? {
            FIELD => Some(either::Either::Left(self.fields[self.data[index] as usize])),
            TUPLE_FIELD => Some(either::Either::Right(self.tuple_fields[self.data[index] as usize])),
            _ => None,
        }
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (ExprId, either::Either<FieldId, TupleFieldId>)> + '_ {
        (0..self.tags.len())
            .filter_map(|index| Some((ExprId::from_index(index), self.get(ExprId::from_index(index))?)))
    }
}

impl fmt::Debug for FieldColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Where a key's list is in a CSR column's flat array: [`NONE`] as `start` for a key without
/// one, which is not the same as an empty list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct Range {
    pub start: u32,
    pub len: u32,
}

impl Range {
    pub const ABSENT: Range = Range { start: NONE, len: 0 };

    #[inline]
    fn is_absent(self) -> bool {
        self.start == NONE
    }

    #[inline]
    fn slice(self) -> std::ops::Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }
}

const _: () = assert!(size_of::<Range>() == 8);

/// One key's list of a CSR column, as `[T]`: what [`Csr::get`] hands out, so that callers
/// treat it as the slice it derefs to.
#[repr(transparent)]
pub struct Run<T>([T]);

impl<T> Run<T> {
    #[inline]
    fn new(items: &[T]) -> &Run<T> {
        // SAFETY: `Run<T>` is `#[repr(transparent)]` over `[T]`.
        unsafe { &*(items as *const [T] as *const Run<T>) }
    }

    #[inline]
    fn new_mut(items: &mut [T]) -> &mut Run<T> {
        // SAFETY: `Run<T>` is `#[repr(transparent)]` over `[T]`, and the borrow is unique.
        unsafe { &mut *(items as *mut [T] as *mut Run<T>) }
    }
}

impl<T> Deref for Run<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> DerefMut for Run<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

impl<T> AsRef<[T]> for Run<T> {
    fn as_ref(&self) -> &[T] {
        &self.0
    }
}

impl<'a, T> IntoIterator for &'a Run<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T: PartialEq> PartialEq for Run<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T: fmt::Debug> fmt::Debug for Run<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A list per key as CSR: each key's [`Range`] into one flat array, the lists in key order.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Csr<K, T> {
    ranges: Box<[Range]>,
    items: Box<[T]>,
    key: PhantomData<fn() -> K>,
}

impl<K, T> Default for Csr<K, T> {
    fn default() -> Self {
        Csr { ranges: Box::default(), items: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey, T> Csr<K, T> {
    /// The column of `ranges` and `items` as [`Self::ranges`] and [`Self::items`] give them,
    /// refused when it is not one [`CsrBuilder::freeze`] makes.
    pub(crate) fn from_parts(ranges: Box<[Range]>, items: Box<[T]>) -> Result<Self, &'static str> {
        if ranges.last().is_some_and(|range| range.is_absent()) {
            return Err("a list column that ends in a key without a list");
        }
        let mut next = 0u64;
        for range in &ranges {
            if range.is_absent() {
                if range.len != 0 {
                    return Err("an absent list with a length");
                }
                continue;
            }
            if u64::from(range.start) != next {
                return Err("a list out of its column's order");
            }
            next += u64::from(range.len);
        }
        if next != items.len() as u64 {
            return Err("a list column whose lists do not cover its items");
        }
        Ok(Csr { ranges, items, key: PhantomData })
    }

    #[inline]
    pub fn get(&self, key: &K) -> Option<&Run<T>> {
        match self.ranges.get(key.to_index()) {
            Some(&range) if !range.is_absent() => Some(Run::new(&self.items[range.slice()])),
            _ => None,
        }
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.ranges.get(key.to_index()).is_some_and(|range| !range.is_absent())
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &[T])> + '_ {
        (self.ranges.iter().enumerate())
            .filter(|(_, range)| !range.is_absent())
            .map(|(index, range)| (K::from_index(index), &self.items[range.slice()]))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &[T]> + '_ {
        self.iter().map(|(_, items)| items)
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.iter().all(|range| range.is_absent())
    }

    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    /// Every key's list, one after another.
    pub fn items(&self) -> &[T] {
        &self.items
    }
}

impl<K: DenseKey + fmt::Debug, T: fmt::Debug> fmt::Debug for Csr<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A CSR column while inference writes it: a list that is replaced or grows past the end of
/// the flat array moves to its end, leaving its old items unused until [`Self::freeze`]
/// compacts the lists into key order.
#[derive(Clone, Debug)]
pub(crate) struct CsrBuilder<K, T> {
    ranges: Vec<Range>,
    items: Vec<T>,
    key: PhantomData<fn() -> K>,
}

impl<K, T> Default for CsrBuilder<K, T> {
    fn default() -> Self {
        CsrBuilder { ranges: Vec::new(), items: Vec::new(), key: PhantomData }
    }
}

impl<K: DenseKey, T> CsrBuilder<K, T> {
    fn range(&self, key: &K) -> Range {
        self.ranges.get(key.to_index()).copied().unwrap_or(Range::ABSENT)
    }

    fn set(&mut self, key: &K, range: Range) {
        let index = key.to_index();
        if index >= self.ranges.len() {
            self.ranges.resize(index + 1, Range::ABSENT);
        }
        self.ranges[index] = range;
    }

    fn start(&self) -> u32 {
        place(self.items.len())
    }

    pub(crate) fn get(&self, key: &K) -> Option<&Run<T>> {
        let range = self.range(key);
        (!range.is_absent()).then(|| Run::new(&self.items[range.slice()]))
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut Run<T>> {
        let range = self.range(key);
        (!range.is_absent()).then(|| Run::new_mut(&mut self.items[range.slice()]))
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        !self.range(key).is_absent()
    }

    /// How many items `key`'s list has, 0 for a key without one.
    pub(crate) fn len_of(&self, key: &K) -> usize {
        self.range(key).len as usize
    }

    /// Gives `key` an empty list if it has none.
    pub(crate) fn ensure(&mut self, key: K) {
        if !self.contains_key(&key) {
            let start = self.start();
            self.set(&key, Range { start, len: 0 });
        }
    }

    /// Replaces `key`'s list.
    pub(crate) fn insert(&mut self, key: K, items: impl IntoIterator<Item = T>) {
        let start = self.start();
        self.items.extend(items);
        let len = place(self.items.len() - start as usize);
        self.set(&key, Range { start, len });
    }

    /// Leaves `key` without a list, its items unused until [`Self::freeze`]; returns whether it
    /// had one.
    pub(crate) fn remove_in_place(&mut self, key: &K) -> bool {
        if self.range(key).is_absent() {
            return false;
        }
        self.set(key, Range::ABSENT);
        true
    }

    /// Appends `item` to `key`'s list, making one if it has none.
    pub(crate) fn push(&mut self, key: K, item: T)
    where
        T: Clone,
    {
        let mut range = self.range(&key);
        if range.is_absent() {
            range = Range { start: self.start(), len: 0 };
        } else if range.start as usize + range.len as usize != self.items.len() {
            let start = self.start();
            self.items.extend_from_within(range.slice());
            range.start = start;
        }
        self.items.push(item);
        range.len += 1;
        self.set(&key, range);
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut [T]> {
        let CsrBuilder { ranges, items, .. } = self;
        // An empty list may start inside another one; lists with items never overlap.
        let mut live: Vec<Range> =
            ranges.iter().copied().filter(|it| !it.is_absent() && it.len != 0).collect();
        live.sort_unstable_by_key(|range| range.start);
        let mut rest: &mut [T] = items;
        let mut consumed = 0usize;
        live.into_iter().map(move |range| {
            let taken = std::mem::take(&mut rest);
            let (_, tail) = taken.split_at_mut(range.start as usize - consumed);
            let (run, tail) = tail.split_at_mut(range.len as usize);
            consumed = range.start as usize + range.len as usize;
            rest = tail;
            run
        })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (K, &[T])> + '_ {
        (self.ranges.iter().enumerate())
            .filter(|(_, range)| !range.is_absent())
            .map(|(index, range)| (K::from_index(index), &self.items[range.slice()]))
    }

    pub(crate) fn freeze(self) -> Csr<K, T> {
        let CsrBuilder { mut ranges, items, .. } = self;
        while ranges.last().is_some_and(|range| range.is_absent()) {
            ranges.pop();
        }
        let mut items: Vec<Option<T>> = items.into_iter().map(Some).collect();
        let mut flat = Vec::with_capacity(items.len());
        let ranges = ranges
            .into_iter()
            .map(|range| {
                if range.is_absent() {
                    return Range::ABSENT;
                }
                let start = place(flat.len());
                flat.extend(items[range.slice()].iter_mut().map(|it| it.take().unwrap()));
                Range { start, len: range.len }
            })
            .collect();
        Csr { ranges, items: flat.into_boxed_slice(), key: PhantomData }
    }
}

/// A set of keys as a bitset, its last word never zero.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IdxSet<K> {
    words: Vec<u64>,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for IdxSet<K> {
    fn default() -> Self {
        IdxSet { words: Vec::new(), key: PhantomData }
    }
}

impl<K: DenseKey> IdxSet<K> {
    /// Adds `key`, returning whether it was not in the set.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        let index = key.to_index();
        let word = index / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let bit = 1u64 << (index % 64);
        let new = self.words[word] & bit == 0;
        self.words[word] |= bit;
        new
    }

    #[inline]
    pub fn contains(&self, key: &K) -> bool {
        let index = key.to_index();
        self.words.get(index / 64).is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        self.words.iter().enumerate().flat_map(|(at, &word)| {
            (0..64).filter(move |bit| word & (1 << bit) != 0).map(move |bit| K::from_index(at * 64 + bit))
        })
    }

    pub fn len(&self) -> usize {
        self.words.iter().map(|word| word.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub(crate) fn from_words(words: Vec<u64>) -> Result<Self, &'static str> {
        match words.last() {
            Some(0) => Err("a set whose last word is zero"),
            _ => Ok(IdxSet { words, key: PhantomData }),
        }
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.words.shrink_to_fit();
    }
}

impl<K: DenseKey> FromIterator<K> for IdxSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let mut set = IdxSet::default();
        iter.into_iter().for_each(|key| _ = set.insert(key));
        set
    }
}

impl<K: DenseKey> Extend<K> for IdxSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        iter.into_iter().for_each(|key| _ = self.insert(key));
    }
}

impl<K: DenseKey + fmt::Debug> fmt::Debug for IdxSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A set of expressions and patterns.
#[derive(Clone, Default, PartialEq, Eq, Hash, Debug)]
pub struct ExprOrPatSet {
    pub(crate) exprs: IdxSet<ExprId>,
    pub(crate) pats: IdxSet<PatId>,
}

impl ExprOrPatSet {
    pub(crate) fn insert(&mut self, node: ExprOrPatIdPacked) -> bool {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.insert(expr),
            (None, Some(pat)) => self.pats.insert(pat),
            (None, None) => false,
        }
    }

    pub fn contains(&self, node: ExprOrPatIdPacked) -> bool {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.contains(&expr),
            (None, Some(pat)) => self.pats.contains(&pat),
            (None, None) => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty() && self.pats.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = ExprOrPatIdPacked> + '_ {
        let exprs = self.exprs.iter().map(ExprOrPatIdPacked::from);
        exprs.chain(self.pats.iter().map(ExprOrPatIdPacked::from))
    }
}

/// No binding mode, in a [`ModeColumn`].
const MODE_NONE: u8 = u8::MAX;

/// Every binding mode, by its code: `2 * by_ref + mutability`, `by_ref` being 0 for by value,
/// 1 for `ref` and 2 for `ref mut`.
static BINDING_MODES: [BindingMode; 6] = [
    BindingMode(ByRef::No, Mutability::Not),
    BindingMode(ByRef::No, Mutability::Mut),
    BindingMode(ByRef::Yes(Mutability::Not), Mutability::Not),
    BindingMode(ByRef::Yes(Mutability::Not), Mutability::Mut),
    BindingMode(ByRef::Yes(Mutability::Mut), Mutability::Not),
    BindingMode(ByRef::Yes(Mutability::Mut), Mutability::Mut),
];

fn mode_code(mode: BindingMode) -> u8 {
    let BindingMode(by_ref, mutability) = mode;
    let by_ref = match by_ref {
        ByRef::No => 0,
        ByRef::Yes(Mutability::Not) => 1,
        ByRef::Yes(Mutability::Mut) => 2,
    };
    let mutability = match mutability {
        Mutability::Not => 0,
        Mutability::Mut => 1,
    };
    2 * by_ref + mutability
}

/// A binding mode per key, one byte each.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ModeColumn<K> {
    codes: Box<[u8]>,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for ModeColumn<K> {
    fn default() -> Self {
        ModeColumn { codes: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey> ModeColumn<K> {
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, BindingMode)>) -> Self {
        let codes = dense(entries).into_iter().map(|mode| mode.map_or(MODE_NONE, mode_code));
        ModeColumn { codes: codes.collect(), key: PhantomData }
    }

    pub(crate) fn from_codes(codes: Box<[u8]>) -> Result<Self, &'static str> {
        if codes.last() == Some(&MODE_NONE) {
            return Err("a binding mode column that ends in a pattern without one");
        }
        match codes.iter().all(|&code| code == MODE_NONE || (code as usize) < BINDING_MODES.len()) {
            true => Ok(ModeColumn { codes, key: PhantomData }),
            false => Err("a binding mode that is none of them"),
        }
    }

    #[inline]
    pub fn get(&self, key: K) -> Option<&BindingMode> {
        BINDING_MODES.get(*self.codes.get(key.to_index())? as usize)
    }

    pub fn iter(&self) -> impl Iterator<Item = (K, &BindingMode)> + '_ {
        (0..self.codes.len()).filter_map(|index| Some((K::from_index(index), self.get(K::from_index(index))?)))
    }

    pub fn codes(&self) -> &[u8] {
        &self.codes
    }
}

impl<K: DenseKey> Index<K> for ModeColumn<K> {
    type Output = BindingMode;

    #[track_caller]
    fn index(&self, key: K) -> &BindingMode {
        self.get(key).expect("no binding mode was inferred for this pattern")
    }
}

impl<K: DenseKey + fmt::Debug> fmt::Debug for ModeColumn<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A side table of the few keys that have a value, sorted by key.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SortedMap<K, V> {
    entries: Box<[(K, V)]>,
}

// SAFETY: a boxed slice of `(K, V)`, nothing else: storable as both are.
unsafe impl<K: salsa::SalsaValue, V: salsa::SalsaValue> salsa::SalsaValue for SortedMap<K, V> {}

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        SortedMap { entries: Box::default() }
    }
}

impl<K: Ord, V> SortedMap<K, V> {
    /// The map of `entries`, of distinct keys.
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut entries: Vec<(K, V)> = entries.into_iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        debug_assert!(entries.windows(2).all(|it| it[0].0 != it[1].0), "a key twice in a map");
        SortedMap { entries: entries.into_boxed_slice() }
    }

    pub(crate) fn from_sorted(entries: Vec<(K, V)>) -> Result<Self, &'static str> {
        match entries.windows(2).all(|it| it[0].0 < it[1].0) {
            true => Ok(SortedMap { entries: entries.into_boxed_slice() }),
            false => Err("a side table out of key order"),
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        let at = self.entries.binary_search_by(|(it, _)| it.cmp(key)).ok()?;
        Some(&self.entries[at].1)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> SortedMapIter<'_, K, V> {
        SortedMapIter { entries: self.entries.iter() }
    }

    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + '_ {
        self.entries.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + '_ {
        self.entries.iter().map(|(_, value)| value)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<K: Ord, V> Index<&K> for SortedMap<K, V> {
    type Output = V;

    #[track_caller]
    fn index(&self, key: &K) -> &V {
        self.get(key).expect("no such key in the side table")
    }
}

pub struct SortedMapIter<'a, K, V> {
    entries: std::slice::Iter<'a, (K, V)>,
}

impl<'a, K, V> Iterator for SortedMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(|(key, value)| (key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl<K, V> ExactSizeIterator for SortedMapIter<'_, K, V> {}

impl<K, V> DoubleEndedIterator for SortedMapIter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries.next_back().map(|(key, value)| (key, value))
    }
}

impl<'a, K: Ord, V> IntoIterator for &'a SortedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = SortedMapIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for SortedMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.entries.iter().map(|(key, value)| (key, value))).finish()
    }
}

/// The type variables a coercion of `!` reached, by their index: rustc's
/// `diverging_type_vars`, which only ever holds type variables.
#[derive(Clone, Default, Debug)]
pub(crate) struct VarSet {
    words: Vec<u64>,
}

impl VarSet {
    pub(crate) fn insert(&mut self, var: u32) {
        let (word, bit) = (var as usize / 64, var % 64);
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1 << bit;
    }

    #[inline]
    pub(crate) fn contains(&self, var: u32) -> bool {
        self.words.get(var as usize / 64).is_some_and(|word| word & (1 << (var % 64)) != 0)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(at, &word)| {
            (0..64u32).filter(move |bit| word & (1 << bit) != 0).map(move |bit| at as u32 * 64 + bit)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use la_arena::{Idx, RawIdx};

    use super::*;

    struct Node;
    type Key = Idx<Node>;

    fn key(index: u32) -> Key {
        Idx::from_raw(RawIdx::from_u32(index))
    }

    /// A deterministic sequence of `(key, value)` writes, with overwrites.
    fn writes(seed: u64, count: usize, keys: u32, values: u32) -> Vec<(u32, u32)> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..count).map(|_| ((next() % keys as u64) as u32, (next() % values as u64) as u32)).collect()
    }

    #[test]
    fn index_column_agrees_with_a_map() {
        for seed in 0..32 {
            let mut model = BTreeMap::new();
            let mut arena = ArenaMap::default();
            for (k, v) in writes(seed, 200, 300, 1000) {
                model.insert(k, v);
                arena.insert(key(k), v);
            }
            let column = IndexColumn::from_entries(arena);
            for k in 0..400 {
                assert_eq!(column.get(key(k)), model.get(&k));
            }
            let listed: Vec<(u32, u32)> =
                column.iter().map(|(k, v)| (k.into_raw().into_u32(), *v)).collect();
            assert_eq!(listed, model.into_iter().collect::<Vec<_>>());
        }
    }

    #[test]
    fn ty_column_agrees_with_a_map_and_keeps_one_of_each_type() {
        let types = &crate::next_solver::default_types().types;
        let palette = [types.i32, types.u8, types.bool, types.unit, types.never];
        for seed in 0..32 {
            let mut model = BTreeMap::new();
            let mut arena = ArenaMap::default();
            for (k, v) in writes(seed, 150, 200, palette.len() as u32) {
                let ty = palette[v as usize].store();
                model.insert(k, ty.clone());
                arena.insert(key(k), ty);
            }
            let column = TyColumn::from_entries(arena.clone());
            for k in 0..250 {
                assert_eq!(column.get(key(k)), model.get(&k));
                assert_eq!(column.contains_idx(key(k)), model.contains_key(&k));
            }
            let distinct: std::collections::HashSet<_> = model.values().cloned().collect();
            assert_eq!(column.tys().len(), distinct.len());
            let listed: Vec<(u32, StoredTy)> =
                column.iter().map(|(k, ty)| (k.into_raw().into_u32(), ty.clone())).collect();
            assert_eq!(listed, model.into_iter().collect::<Vec<_>>());

            let reversed: Vec<(Key, StoredTy)> = arena.iter().rev().map(|(k, ty)| (k, ty.clone())).collect();
            assert!(TyColumn::from_entries(reversed) == column);
            let again = TyColumn::<Key>::from_parts(column.at().into(), column.tys().into()).unwrap();
            assert!(again == column);
        }
        let swapped = TyColumn::<Key>::from_parts(
            Box::new([1, 0]),
            Box::new([types.i32.store(), types.u8.store()]),
        );
        assert!(swapped.is_err());
    }

    #[test]
    fn csr_builder_agrees_with_a_map_of_lists() {
        for seed in 0..32 {
            let mut model: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            let mut builder = CsrBuilder::<Key, u32>::default();
            for (step, (k, v)) in writes(seed, 400, 40, 1000).into_iter().enumerate() {
                match step % 5 {
                    0 => {
                        let items = vec![v, v + 1, v + 2];
                        model.insert(k, items.clone());
                        builder.insert(key(k), items);
                    }
                    1 => {
                        let removed = builder.get(&key(k)).map(|it| it.to_vec());
                        assert_eq!(removed, model.remove(&k));
                        assert_eq!(builder.remove_in_place(&key(k)), removed.is_some());
                    }
                    2 => {
                        model.entry(k).or_default();
                        builder.ensure(key(k));
                    }
                    3 => {
                        if let Some(items) = model.get_mut(&k) {
                            items.iter_mut().for_each(|it| *it += 1);
                        }
                        if let Some(items) = builder.get_mut(&key(k)) {
                            items.iter_mut().for_each(|it| *it += 1);
                        }
                    }
                    _ => {
                        model.entry(k).or_default().push(v);
                        builder.push(key(k), v);
                    }
                }
                for k in 0..40 {
                    assert_eq!(builder.get(&key(k)).map(|it| it.to_vec()), model.get(&k).cloned());
                    assert_eq!(builder.len_of(&key(k)), model.get(&k).map_or(0, Vec::len));
                }
            }
            let mut doubled = builder.clone();
            doubled.values_mut().flatten().for_each(|it| *it *= 2);
            let frozen = builder.freeze();
            let listed: Vec<(u32, Vec<u32>)> =
                frozen.iter().map(|(k, items)| (k.into_raw().into_u32(), items.to_vec())).collect();
            assert_eq!(listed, model.clone().into_iter().collect::<Vec<_>>());
            for k in 0..40 {
                assert_eq!(frozen.contains_key(&key(k)), model.contains_key(&k));
                let twice = doubled.get(&key(k)).map(|it| it.to_vec());
                assert_eq!(twice, model.get(&k).map(|it| it.iter().map(|it| it * 2).collect()));
            }
            let again = Csr::<Key, u32>::from_parts(
                frozen.ranges().to_vec().into_boxed_slice(),
                frozen.items().to_vec().into_boxed_slice(),
            )
            .unwrap();
            assert!(again == frozen);
        }
    }

    #[test]
    fn a_csr_written_in_another_order_is_the_same_column() {
        let mut forward = CsrBuilder::<Key, u32>::default();
        forward.insert(key(1), [1, 2]);
        forward.insert(key(5), [5]);
        let mut backward = CsrBuilder::<Key, u32>::default();
        backward.insert(key(5), [9]);
        backward.push(key(1), 1);
        backward.insert(key(5), [5]);
        backward.push(key(1), 2);
        assert!(forward.freeze() == backward.freeze());
    }

    #[test]
    fn an_empty_list_is_not_an_absent_one() {
        let mut builder = CsrBuilder::<Key, u32>::default();
        builder.ensure(key(3));
        let frozen = builder.freeze();
        assert!(frozen.contains_key(&key(3)));
        assert_eq!(frozen.get(&key(3)).map(|it| it.len()), Some(0));
        assert!(!frozen.contains_key(&key(2)));
        assert!(frozen.get(&key(2)).is_none());
    }

    #[test]
    fn idx_set_agrees_with_a_set() {
        for seed in 0..32 {
            let mut model = std::collections::BTreeSet::new();
            let mut set = IdxSet::<Key>::default();
            for (k, _) in writes(seed, 300, 700, 1) {
                assert_eq!(set.insert(key(k)), model.insert(k));
            }
            for k in 0..800 {
                assert_eq!(set.contains(&key(k)), model.contains(&k));
            }
            let listed: Vec<u32> = set.iter().map(|k| k.into_raw().into_u32()).collect();
            assert_eq!(listed, model.iter().copied().collect::<Vec<_>>());
            assert_eq!(set.len(), model.len());
            assert!(set.words().last().is_none_or(|&word| word != 0));
        }
    }

    #[test]
    fn mode_column_round_trips_every_mode() {
        let modes: Vec<(Key, BindingMode)> =
            BINDING_MODES.iter().enumerate().map(|(at, &mode)| (key(at as u32 * 2), mode)).collect();
        let column = ModeColumn::from_entries(modes.iter().copied());
        for (k, mode) in &modes {
            assert_eq!(column.get(*k), Some(mode));
            assert_eq!(column[*k], *mode);
        }
        assert_eq!(column.get(key(1)), None);
        assert_eq!(column.get(key(100)), None);
        for (at, &mode) in BINDING_MODES.iter().enumerate() {
            assert_eq!(mode_code(mode) as usize, at);
        }
        assert!(ModeColumn::<Key>::from_codes(Box::new([6])).is_err());
    }

    #[test]
    fn sorted_map_agrees_with_a_map() {
        let model: BTreeMap<u32, u32> = writes(7, 100, 1000, 1000).into_iter().collect();
        let mut shuffled: Vec<(u32, u32)> = model.iter().map(|(k, v)| (*k, *v)).collect();
        shuffled.reverse();
        let map = SortedMap::from_entries(shuffled);
        for k in 0..1000 {
            assert_eq!(map.get(&k), model.get(&k));
        }
        let listed: Vec<(u32, u32)> = map.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(listed, model.into_iter().collect::<Vec<_>>());
        assert!(SortedMap::from_sorted(vec![(2, 0), (1, 0)]).is_err());
    }

    #[test]
    fn var_set_lists_what_it_holds() {
        let mut set = VarSet::default();
        for var in [0, 63, 64, 200, 3, 63] {
            set.insert(var);
        }
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![0, 3, 63, 64, 200]);
        for var in 0..300 {
            assert_eq!(set.contains(var), [0, 3, 63, 64, 200].contains(&var));
        }
    }
}
