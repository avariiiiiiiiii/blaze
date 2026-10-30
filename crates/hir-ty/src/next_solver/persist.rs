//! Interned values as `u32` words, in two places.
//!
//! - In memory: the row of an interned value (`interner::table::Rows`) is its words, each child
//!   (a type, a region, a list, ...) as its index in its own table, an ADT with the flags it was
//!   made with.
//! - Persisted: a query result holds types as their place in a table of its own
//!   ([`TyTableWriter`], [`TyTableReader`]): each value written once, after the values it holds,
//!   as its words, each child as its place in the table, definitions as salsa ids, an ADT as its
//!   id alone (its flags are read again from the database the table is read into).
//!
//! Both are the one codec, [`Code`]: what a value writes to a [`Sink`] and reads back from a
//! [`Source`]. It is prefix-free, which a row's equality relies on: a value's first word says
//! how many words follow.

use std::{convert::Infallible, marker::PhantomData, mem::MaybeUninit, num::NonZeroU32};

use blaze_format::{FormatError, HEADER_LEN, Record, Slice, Writer};
use hir_def::{AdtId, ConstParamId, GenericDefId, LifetimeParamId, TypeOrConstParamId, TypeParamId};
use la_arena::{Idx, RawIdx};
use rustc_abi::{ExternAbi, Size};
use rustc_ast_ir::{FloatTy, IntTy, Mutability, UintTy};
use rustc_hash::FxHashMap;
use rustc_type_ir::{
    AliasRelationDirection, BoundConstness, BoundVar, BoundVarIndexKind, ConstVid, DebruijnIndex,
    FloatVid, InferConst, InferTy, IntVid, PredicatePolarity, RegionVid, TyVid, TypeVisitable,
    UniverseIndex,
    inherent::IntoKind,
};
use salsa::plumbing::{AsId, FromId};
use smallvec::SmallVec;

use super::{
    AdtDef, AliasTerm, AliasTermKind, AliasTy, AliasTyKind, Binder, BoundConst,
    BoundExistentialPredicate, BoundExistentialPredicates, BoundRegion, BoundRegionKind, BoundTy,
    BoundTyKind, BoundVarKinds, BoundVariableKind, CallableIdWrapper, Clause, ClauseKind, Clauses,
    ClosureIdWrapper, CoercePredicate, Const, ConstKind, Consts, CoroutineClosureIdWrapper,
    CoroutineIdWrapper, Ctor, DbInterner, EarlyParamRegion, ErrorGuaranteed, ExistentialPredicate,
    ExistentialProjection, ExistentialTraitRef, ExprConst, FnHeader, FnSigKind, FreeConstAliasId,
    FreeTermAliasId, FreeTyAliasId, GeneralConstIdWrapper, GenericArg, GenericArgKind,
    GenericArgs, HostEffectPredicate, ImplOrTraitAssocConstId, ImplOrTraitAssocTermId,
    ImplOrTraitAssocTyId, InherentAssocConstId, InherentAssocTermId, InherentAssocTyId,
    LateParamRegion, NormalizesTo, OpaqueTyIdWrapper, OutlivesPredicate, ParamConst, ParamTy,
    PatList, Pattern, PatternKind, PlaceholderConst, PlaceholderRegion, PlaceholderType, Predicate,
    PredicateKind, ProjectionPredicate, Region, RegionKind, ScalarInt, SolverDefId,
    SubtypePredicate, Term, TermKind, TraitAssocConstId, TraitAssocTermId, TraitAssocTyId,
    TraitIdWrapper, TraitPredicate, TraitRef, Ty, TyKind, TypeAliasIdWrapper, Tys,
    UnevaluatedConst, ValTree, ValTreeKind, ValueConst, abi::Safety, ensure_sufficient_stack,
    interner::{Symbol, table::Fields},
};

/// Where a value's words go.
pub(crate) trait Sink<'db> {
    fn word(&mut self, word: u32);
    /// A child: its index in its table in memory, its place in the table persisted.
    fn node<N: Node<'db>>(&mut self, node: N);
    /// An ADT: in memory with its flags, persisted as its id alone.
    fn adt(&mut self, adt: AdtDef);
}

/// Where a value's words come from.
pub(crate) trait Source<'db> {
    type Error;
    fn word(&mut self) -> Result<u32, Self::Error>;
    fn node<N: Node<'db>>(&mut self) -> Result<N, Self::Error>;
    fn adt(&mut self) -> Result<AdtDef, Self::Error>;
    /// How many of something follow, each of at least one word.
    fn count(&mut self) -> Result<usize, Self::Error>;
    fn invalid(&self, what: &'static str) -> Self::Error;
}

/// A value as words.
pub(crate) trait Code<'db>: Sized {
    fn put(&self, sink: &mut impl Sink<'db>);
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error>;
}

/// The tables of interned values, as a persisted table names them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u32)]
pub(crate) enum Kind {
    Ty,
    Region,
    Const,
    Pattern,
    Predicate,
    ValTree,
    GenericArgs,
    Tys,
    Consts,
    BoundVarKinds,
    PatList,
    BoundExistentialPredicates,
    Clauses,
}

const KINDS: [Kind; 13] = [
    Kind::Ty,
    Kind::Region,
    Kind::Const,
    Kind::Pattern,
    Kind::Predicate,
    Kind::ValTree,
    Kind::GenericArgs,
    Kind::Tys,
    Kind::Consts,
    Kind::BoundVarKinds,
    Kind::PatList,
    Kind::BoundExistentialPredicates,
    Kind::Clauses,
];

/// An interned value: a row of a table of its own.
pub(crate) trait Node<'db>: Copy {
    const KIND: Kind;
    fn row(self) -> NonZeroU32;
    /// The value of a row of its table: one interning gave, or `published` accepted.
    fn at(row: NonZeroU32) -> Self;
    /// Its words in a persisted table.
    fn put_node(self, sink: &mut impl Sink<'db>);
    /// The value of the words [`Node::put_node`] wrote, interned.
    fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error>;
}

/// A value's words as its row holds them.
pub(crate) type Words = SmallVec<[u32; 16]>;

impl<'db> Sink<'db> for Words {
    #[inline]
    fn word(&mut self, word: u32) {
        self.push(word);
    }

    #[inline]
    fn node<N: Node<'db>>(&mut self, node: N) {
        self.push(node.row().get());
    }

    fn adt(&mut self, adt: AdtDef) {
        SolverDefId::from(adt.def_id()).put(self);
        self.push(adt.flag_bits().into());
    }
}

/// The words of `value`'s row.
#[inline]
pub(crate) fn encode<'db>(value: &impl Code<'db>) -> Words {
    let mut words = Words::new();
    value.put(&mut words);
    words
}

/// The value of a row.
#[inline]
pub(crate) fn decode<'db, T: Code<'db>>(fields: Fields<'_>) -> T {
    match T::get(&mut Row { tag: Some(fields.tag), words: fields.words }) {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

/// The words of a row, read back: what `encode` gave, so none is invalid.
struct Row<'r> {
    tag: Option<u32>,
    words: &'r [u32],
}

impl<'db> Source<'db> for Row<'_> {
    type Error = Infallible;

    #[inline]
    fn word(&mut self) -> Result<u32, Infallible> {
        if let Some(tag) = self.tag.take() {
            return Ok(tag);
        }
        let (&word, rest) = self.words.split_first().expect("a row as long as its value");
        self.words = rest;
        Ok(word)
    }

    #[inline]
    fn node<N: Node<'db>>(&mut self) -> Result<N, Infallible> {
        let row = self.word()?;
        Ok(N::at(NonZeroU32::new(row).expect("a row's child is a row")))
    }

    fn adt(&mut self) -> Result<AdtDef, Infallible> {
        let def = SolverDefId::get(self)?;
        let bits = self.word()?;
        let adt = match def {
            SolverDefId::AdtId(id) => u8::try_from(bits).ok().and_then(|bits| AdtDef::from_parts(id, bits)),
            _ => None,
        };
        Ok(adt.expect("a row's ADT is one `AdtDef::new` made"))
    }

    #[inline]
    fn count(&mut self) -> Result<usize, Infallible> {
        Ok(self.word()? as usize)
    }

    fn invalid(&self, what: &'static str) -> Infallible {
        panic!("a row that is not {what}")
    }
}

/// A word below `end`.
fn below<'db, S: Source<'db>>(
    source: &mut S,
    end: u32,
    what: &'static str,
) -> Result<u32, S::Error> {
    let word = source.word()?;
    if word < end { Ok(word) } else { Err(source.invalid(what)) }
}

impl<'db> Code<'db> for u32 {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(*self);
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        source.word()
    }
}

impl<'db> Code<'db> for bool {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(u32::from(*self));
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(below(source, 2, "a bool")? == 1)
    }
}

/// Types of one value, which take no words.
macro_rules! unit_code {
    ($($ty:ident),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, _: &mut impl Sink<'db>) {}
            #[inline]
            fn get<S: Source<'db>>(_: &mut S) -> Result<Self, S::Error> {
                Ok($ty)
            }
        }
    )*};
}

unit_code!(ErrorGuaranteed, ExprConst, Symbol);

/// `rustc_index` newtypes: one word, below their maximum.
macro_rules! index_code {
    ($($ty:ty),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                sink.word(self.as_u32());
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let word = below(source, <$ty>::MAX_AS_U32 + 1, stringify!($ty))?;
                Ok(<$ty>::from_u32(word))
            }
        }
    )*};
}

index_code!(DebruijnIndex, BoundVar, UniverseIndex, TyVid, IntVid, FloatVid, RegionVid, ConstVid);

/// Enums without fields: their variant's number.
macro_rules! enum_code {
    ($ty:ty { $($index:literal => $variant:path),* $(,)? }) => {
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                sink.word(match *self { $($variant => $index,)* });
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                match source.word()? {
                    $($index => Ok($variant),)*
                    _ => Err(source.invalid(stringify!($ty))),
                }
            }
        }
    };
}

enum_code!(Mutability { 0 => Mutability::Not, 1 => Mutability::Mut });
enum_code!(IntTy {
    0 => IntTy::Isize,
    1 => IntTy::I8,
    2 => IntTy::I16,
    3 => IntTy::I32,
    4 => IntTy::I64,
    5 => IntTy::I128,
});
enum_code!(UintTy {
    0 => UintTy::Usize,
    1 => UintTy::U8,
    2 => UintTy::U16,
    3 => UintTy::U32,
    4 => UintTy::U64,
    5 => UintTy::U128,
});
enum_code!(FloatTy { 0 => FloatTy::F16, 1 => FloatTy::F32, 2 => FloatTy::F64, 3 => FloatTy::F128 });
enum_code!(PredicatePolarity { 0 => PredicatePolarity::Positive, 1 => PredicatePolarity::Negative });
enum_code!(BoundConstness { 0 => BoundConstness::Const, 1 => BoundConstness::Maybe });
enum_code!(AliasRelationDirection {
    0 => AliasRelationDirection::Equate,
    1 => AliasRelationDirection::Subtype,
});

/// Structs whose fields are all public: their fields in order.
macro_rules! struct_code {
    ($($ty:ty { $($field:ident),* $(,)? }),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                $( self.$field.put(sink); )*
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                Ok(Self { $( $field: Code::get(source)?, )* })
            }
        }
    )*};
}

struct_code! {
    ParamTy { id, index },
    ParamConst { id, index },
    EarlyParamRegion { id, index },
    LifetimeParamId { parent, local_id },
    LateParamRegion<'db> { scope, bound_region },
    BoundRegion<'db> { var, kind },
    BoundTy<'db> { var, kind },
    UnevaluatedConst<'db> { def, args },
    ValueConst<'db> { ty, value },
    TraitPredicate<'db> { trait_ref, polarity },
    ProjectionPredicate<'db> { projection_term, term },
    HostEffectPredicate<'db> { trait_ref, constness },
    SubtypePredicate<'db> { a_is_expected, a, b },
    CoercePredicate<'db> { a, b },
    NormalizesTo<'db> { alias, term },
    rustc_type_ir::FnSigTys<DbInterner<'db>> { inputs_and_output },
}

/// A salsa id: its index (never 0) and its generation.
fn put_id<'db>(sink: &mut impl Sink<'db>, id: salsa::Id) {
    let bits = id.as_bits();
    sink.word(bits as u32);
    sink.word((bits >> 32) as u32);
}

fn get_id<'db, S: Source<'db>>(source: &mut S) -> Result<salsa::Id, S::Error> {
    let index = source.word()?;
    let generation = source.word()?;
    if index == 0 {
        return Err(source.invalid("a salsa id of index 0"));
    }
    Ok(salsa::Id::from_bits(u64::from(index) | (u64::from(generation) << 32)))
}

impl<'db> Code<'db> for SolverDefId<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        let (variant, id) = match *self {
            SolverDefId::AdtId(AdtId::StructId(id)) => (0, id.as_id()),
            SolverDefId::AdtId(AdtId::UnionId(id)) => (1, id.as_id()),
            SolverDefId::AdtId(AdtId::EnumId(id)) => (2, id.as_id()),
            SolverDefId::ConstId(id) => (3, id.as_id()),
            SolverDefId::FunctionId(id) => (4, id.as_id()),
            SolverDefId::ImplId(id) => (5, id.as_id()),
            SolverDefId::BuiltinDeriveImplId(id) => (6, id.as_id()),
            SolverDefId::StaticId(id) => (7, id.as_id()),
            SolverDefId::AnonConstId(id) => (8, id.as_id()),
            SolverDefId::TraitId(id) => (9, id.as_id()),
            SolverDefId::TypeAliasId(id) => (10, id.as_id()),
            SolverDefId::InternedClosureId(id) => (11, id.as_id()),
            SolverDefId::InternedCoroutineId(id) => (12, id.as_id()),
            SolverDefId::InternedCoroutineClosureId(id) => (13, id.as_id()),
            SolverDefId::InternedOpaqueTyId(id) => (14, id.as_id()),
            SolverDefId::EnumVariantId(id) => (15, id.as_id()),
            SolverDefId::Ctor(Ctor::Struct(id)) => (16, id.as_id()),
            SolverDefId::Ctor(Ctor::Enum(id)) => (17, id.as_id()),
        };
        sink.word(variant);
        put_id(sink, id);
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let variant = source.word()?;
        let id = get_id(source)?;
        Ok(match variant {
            0 => SolverDefId::AdtId(AdtId::StructId(FromId::from_id(id))),
            1 => SolverDefId::AdtId(AdtId::UnionId(FromId::from_id(id))),
            2 => SolverDefId::AdtId(AdtId::EnumId(FromId::from_id(id))),
            3 => SolverDefId::ConstId(FromId::from_id(id)),
            4 => SolverDefId::FunctionId(FromId::from_id(id)),
            5 => SolverDefId::ImplId(FromId::from_id(id)),
            6 => SolverDefId::BuiltinDeriveImplId(FromId::from_id(id)),
            7 => SolverDefId::StaticId(FromId::from_id(id)),
            8 => SolverDefId::AnonConstId(FromId::from_id(id)),
            9 => SolverDefId::TraitId(FromId::from_id(id)),
            10 => SolverDefId::TypeAliasId(FromId::from_id(id)),
            11 => SolverDefId::InternedClosureId(FromId::from_id(id)),
            12 => SolverDefId::InternedCoroutineId(FromId::from_id(id)),
            13 => SolverDefId::InternedCoroutineClosureId(FromId::from_id(id)),
            14 => SolverDefId::InternedOpaqueTyId(FromId::from_id(id)),
            15 => SolverDefId::EnumVariantId(FromId::from_id(id)),
            16 => SolverDefId::Ctor(Ctor::Struct(FromId::from_id(id))),
            17 => SolverDefId::Ctor(Ctor::Enum(FromId::from_id(id))),
            _ => return Err(source.invalid("a definition")),
        })
    }
}

/// Ids that convert both ways with [`SolverDefId`]: as the one they convert to.
macro_rules! def_id_code {
    ($($ty:ty),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                SolverDefId::from(*self).put(sink);
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let def = SolverDefId::get(source)?;
                <$ty>::try_from(def).map_err(|_| source.invalid(stringify!($ty)))
            }
        }
    )*};
}

def_id_code!(
    GenericDefId,
    TraitIdWrapper,
    TypeAliasIdWrapper,
    CallableIdWrapper,
    ClosureIdWrapper<'db>,
    CoroutineIdWrapper<'db>,
    CoroutineClosureIdWrapper<'db>,
    OpaqueTyIdWrapper<'db>,
    GeneralConstIdWrapper<'db>,
    TraitAssocTyId,
    TraitAssocConstId,
    TraitAssocTermId,
    ImplOrTraitAssocTyId,
    ImplOrTraitAssocConstId,
    ImplOrTraitAssocTermId,
    FreeTyAliasId,
    FreeConstAliasId,
    FreeTermAliasId,
    InherentAssocTyId,
    InherentAssocConstId,
    InherentAssocTermId,
);

impl<'db> Code<'db> for AdtDef {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.adt(*self);
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        source.adt()
    }
}

impl<'db, T> Code<'db> for Idx<T> {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(self.into_raw().into_u32());
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(Idx::from_raw(RawIdx::from_u32(source.word()?)))
    }
}

impl<'db> Code<'db> for TypeParamId {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.parent().put(sink);
        self.local_id().put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let parent = GenericDefId::get(source)?;
        let local_id = Idx::get(source)?;
        Ok(TypeParamId::from_unchecked(TypeOrConstParamId { parent, local_id }))
    }
}

impl<'db> Code<'db> for ConstParamId {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.parent().put(sink);
        self.local_id().put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let parent = GenericDefId::get(source)?;
        let local_id = Idx::get(source)?;
        Ok(ConstParamId::from_unchecked(TypeOrConstParamId { parent, local_id }))
    }
}

/// A struct of `rustc_type_ir` whose one private field is a `()` that has it be built by a
/// function taking an interner, built from its public fields: the check that function would
/// make (`Interner::debug_assert_args_compatible`) is empty for `DbInterner`.
macro_rules! assemble {
    ($ty:ty { $($field:ident: $value:expr),* $(,)? }) => {{
        let mut value = MaybeUninit::<$ty>::uninit();
        let at = value.as_mut_ptr();
        // SAFETY: the fields written are all of the struct's but a `()`, which has no bytes, so
        // the struct is initialized once they are.
        unsafe {
            $( (&raw mut (*at).$field).write($value); )*
            value.assume_init()
        }
    }};
}

impl<'db> Code<'db> for TraitRef<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.def_id.put(sink);
        self.args.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let def_id = Code::get(source)?;
        let args = Code::get(source)?;
        Ok(assemble!(TraitRef<'db> { def_id: def_id, args: args }))
    }
}

impl<'db> Code<'db> for ExistentialTraitRef<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.def_id.put(sink);
        self.args.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let def_id = Code::get(source)?;
        let args = Code::get(source)?;
        Ok(assemble!(ExistentialTraitRef<'db> { def_id: def_id, args: args }))
    }
}

impl<'db> Code<'db> for ExistentialProjection<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.def_id.put(sink);
        self.args.put(sink);
        self.term.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let def_id = Code::get(source)?;
        let args = Code::get(source)?;
        let term = Code::get(source)?;
        Ok(assemble!(ExistentialProjection<'db> { def_id: def_id, args: args, term: term }))
    }
}

impl<'db> Code<'db> for AliasTy<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.args.put(sink);
        self.kind.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let args = Code::get(source)?;
        let kind = Code::get(source)?;
        Ok(assemble!(AliasTy<'db> { args: args, kind: kind }))
    }
}

impl<'db> Code<'db> for AliasTerm<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.args.put(sink);
        self.kind.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let args = Code::get(source)?;
        let kind = Code::get(source)?;
        Ok(assemble!(AliasTerm<'db> { args: args, kind: kind }))
    }
}

impl<'db> Code<'db> for AliasTyKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            AliasTyKind::Projection { def_id } => {
                sink.word(0);
                def_id.put(sink);
            }
            AliasTyKind::Inherent { def_id } => {
                sink.word(1);
                def_id.put(sink);
            }
            AliasTyKind::Opaque { def_id } => {
                sink.word(2);
                def_id.put(sink);
            }
            AliasTyKind::Free { def_id } => {
                sink.word(3);
                def_id.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => AliasTyKind::Projection { def_id: Code::get(source)? },
            1 => AliasTyKind::Inherent { def_id: Code::get(source)? },
            2 => AliasTyKind::Opaque { def_id: Code::get(source)? },
            3 => AliasTyKind::Free { def_id: Code::get(source)? },
            _ => return Err(source.invalid("an alias type kind")),
        })
    }
}

impl<'db> Code<'db> for AliasTermKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            AliasTermKind::ProjectionTy { def_id } => {
                sink.word(0);
                def_id.put(sink);
            }
            AliasTermKind::InherentTy { def_id } => {
                sink.word(1);
                def_id.put(sink);
            }
            AliasTermKind::OpaqueTy { def_id } => {
                sink.word(2);
                def_id.put(sink);
            }
            AliasTermKind::FreeTy { def_id } => {
                sink.word(3);
                def_id.put(sink);
            }
            AliasTermKind::UnevaluatedConst { def_id } => {
                sink.word(4);
                def_id.put(sink);
            }
            AliasTermKind::ProjectionConst { def_id } => {
                sink.word(5);
                def_id.put(sink);
            }
            AliasTermKind::FreeConst { def_id } => {
                sink.word(6);
                def_id.put(sink);
            }
            AliasTermKind::InherentConst { def_id } => {
                sink.word(7);
                def_id.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => AliasTermKind::ProjectionTy { def_id: Code::get(source)? },
            1 => AliasTermKind::InherentTy { def_id: Code::get(source)? },
            2 => AliasTermKind::OpaqueTy { def_id: Code::get(source)? },
            3 => AliasTermKind::FreeTy { def_id: Code::get(source)? },
            4 => AliasTermKind::UnevaluatedConst { def_id: Code::get(source)? },
            5 => AliasTermKind::ProjectionConst { def_id: Code::get(source)? },
            6 => AliasTermKind::FreeConst { def_id: Code::get(source)? },
            7 => AliasTermKind::InherentConst { def_id: Code::get(source)? },
            _ => return Err(source.invalid("an alias term kind")),
        })
    }
}

impl<'db> Code<'db> for BoundVarIndexKind {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundVarIndexKind::Bound(debruijn) => {
                sink.word(0);
                debruijn.put(sink);
            }
            BoundVarIndexKind::Canonical => sink.word(1),
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundVarIndexKind::Bound(Code::get(source)?),
            1 => BoundVarIndexKind::Canonical,
            _ => return Err(source.invalid("a bound variable index")),
        })
    }
}

impl<'db> Code<'db> for BoundTyKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundTyKind::Anon => sink.word(0),
            BoundTyKind::Param(def) => {
                sink.word(1);
                def.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundTyKind::Anon,
            1 => BoundTyKind::Param(Code::get(source)?),
            _ => return Err(source.invalid("a bound type kind")),
        })
    }
}

impl<'db> Code<'db> for BoundRegionKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundRegionKind::Anon => sink.word(0),
            BoundRegionKind::NamedForPrinting(symbol) => {
                sink.word(1);
                symbol.put(sink);
            }
            BoundRegionKind::Named(def) => {
                sink.word(2);
                def.put(sink);
            }
            BoundRegionKind::ClosureEnv => sink.word(3),
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundRegionKind::Anon,
            1 => BoundRegionKind::NamedForPrinting(Code::get(source)?),
            2 => BoundRegionKind::Named(Code::get(source)?),
            3 => BoundRegionKind::ClosureEnv,
            _ => return Err(source.invalid("a bound region kind")),
        })
    }
}

impl<'db> Code<'db> for BoundVariableKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            BoundVariableKind::Ty(kind) => {
                sink.word(0);
                kind.put(sink);
            }
            BoundVariableKind::Region(kind) => {
                sink.word(1);
                kind.put(sink);
            }
            BoundVariableKind::Const => sink.word(2),
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => BoundVariableKind::Ty(Code::get(source)?),
            1 => BoundVariableKind::Region(Code::get(source)?),
            2 => BoundVariableKind::Const,
            _ => return Err(source.invalid("a bound variable kind")),
        })
    }
}

impl<'db> Code<'db> for BoundConst<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.var.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(BoundConst::new(Code::get(source)?))
    }
}

/// Placeholders: their universe and what they stand for.
macro_rules! placeholder_code {
    ($($ty:ident($bound:ty)),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty<'db> {
            fn put(&self, sink: &mut impl Sink<'db>) {
                self.universe.put(sink);
                self.bound.put(sink);
            }
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let universe = UniverseIndex::get(source)?;
                let bound = <$bound>::get(source)?;
                Ok($ty::new(universe, bound))
            }
        }
    )*};
}

placeholder_code!(
    PlaceholderType(BoundTy<'db>),
    PlaceholderRegion(BoundRegion<'db>),
    PlaceholderConst(BoundConst<'db>),
);

impl<'db> Code<'db> for InferTy {
    fn put(&self, sink: &mut impl Sink<'db>) {
        let (variant, word) = match *self {
            InferTy::TyVar(var) => (0, var.as_u32()),
            InferTy::IntVar(var) => (1, var.as_u32()),
            InferTy::FloatVar(var) => (2, var.as_u32()),
            InferTy::FreshTy(n) => (3, n),
            InferTy::FreshIntTy(n) => (4, n),
            InferTy::FreshFloatTy(n) => (5, n),
        };
        sink.word(variant);
        sink.word(word);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => InferTy::TyVar(Code::get(source)?),
            1 => InferTy::IntVar(Code::get(source)?),
            2 => InferTy::FloatVar(Code::get(source)?),
            3 => InferTy::FreshTy(source.word()?),
            4 => InferTy::FreshIntTy(source.word()?),
            5 => InferTy::FreshFloatTy(source.word()?),
            _ => return Err(source.invalid("an inference type")),
        })
    }
}

impl<'db> Code<'db> for InferConst {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            InferConst::Var(var) => {
                sink.word(0);
                var.put(sink);
            }
            InferConst::Fresh(n) => {
                sink.word(1);
                sink.word(n);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => InferConst::Var(Code::get(source)?),
            1 => InferConst::Fresh(source.word()?),
            _ => return Err(source.invalid("an inference constant")),
        })
    }
}

impl<'db, T: Code<'db> + TypeVisitable<DbInterner<'db>>> Code<'db> for Binder<'db, T> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.as_ref().skip_binder().put(sink);
        sink.node(self.bound_vars());
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let value = T::get(source)?;
        let bound_vars = source.node::<BoundVarKinds<'db>>()?;
        Ok(Binder::bind_with_vars(value, bound_vars))
    }
}

impl<'db> Code<'db> for rustc_type_ir::UnsafeBinderInner<DbInterner<'db>> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        (**self).put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(Binder::<'db, Ty<'db>>::get(source)?.into())
    }
}

impl<'db> Code<'db> for FnHeader<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        let kind = self.fn_sig_kind;
        sink.word(kind.abi().as_packed().into());
        kind.is_safe().put(sink);
        kind.c_variadic().put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let abi = below(source, ExternAbi::ALL_VARIANTS.len() as u32, "an ABI")?;
        let safety = if bool::get(source)? { Safety::Safe } else { Safety::Unsafe };
        let c_variadic = bool::get(source)?;
        let abi = ExternAbi::from_packed(abi as u8);
        Ok(FnHeader { fn_sig_kind: FnSigKind::new(abi, safety, c_variadic) })
    }
}

impl<'db> Code<'db> for ScalarInt {
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.word(self.size().bytes() as u32);
        let data = self.to_bits_unchecked();
        for i in 0..4 {
            sink.word((data >> (32 * i)) as u32);
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let size = source.word()?;
        if !(1..=16).contains(&size) {
            return Err(source.invalid("a scalar of 1 to 16 bytes"));
        }
        let mut data = 0u128;
        for i in 0..4 {
            data |= u128::from(source.word()?) << (32 * i);
        }
        ScalarInt::try_from_uint(data, Size::from_bytes(size))
            .ok_or_else(|| source.invalid("a scalar wider than its size"))
    }
}

impl<'db> Code<'db> for GenericArg<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match self.kind() {
            GenericArgKind::Type(ty) => {
                sink.word(0);
                sink.node(ty);
            }
            GenericArgKind::Lifetime(region) => {
                sink.word(1);
                sink.node(region);
            }
            GenericArgKind::Const(konst) => {
                sink.word(2);
                sink.node(konst);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => source.node::<Ty<'db>>()?.into(),
            1 => source.node::<Region<'db>>()?.into(),
            2 => source.node::<Const<'db>>()?.into(),
            _ => return Err(source.invalid("a generic argument")),
        })
    }
}

impl<'db> Code<'db> for Term<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match self.kind() {
            TermKind::Ty(ty) => {
                sink.word(0);
                sink.node(ty);
            }
            TermKind::Const(konst) => {
                sink.word(1);
                sink.node(konst);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => source.node::<Ty<'db>>()?.into(),
            1 => source.node::<Const<'db>>()?.into(),
            _ => return Err(source.invalid("a term")),
        })
    }
}

impl<'db> Code<'db> for Clause<'db> {
    #[inline]
    fn put(&self, sink: &mut impl Sink<'db>) {
        sink.node(self.0);
    }
    #[inline]
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(Clause(source.node()?))
    }
}

impl<'db, A: Code<'db>> Code<'db> for OutlivesPredicate<'db, A> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        self.0.put(sink);
        self.1.put(sink);
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let a = A::get(source)?;
        let region = Code::get(source)?;
        Ok(rustc_type_ir::OutlivesPredicate(a, region))
    }
}

impl<'db> Code<'db> for ExistentialPredicate<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            ExistentialPredicate::Trait(trait_ref) => {
                sink.word(0);
                trait_ref.put(sink);
            }
            ExistentialPredicate::Projection(projection) => {
                sink.word(1);
                projection.put(sink);
            }
            ExistentialPredicate::AutoTrait(def_id) => {
                sink.word(2);
                def_id.put(sink);
            }
        }
    }
    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => ExistentialPredicate::Trait(Code::get(source)?),
            1 => ExistentialPredicate::Projection(Code::get(source)?),
            2 => ExistentialPredicate::AutoTrait(Code::get(source)?),
            _ => return Err(source.invalid("an existential predicate")),
        })
    }
}

impl<'db> Code<'db> for TyKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            TyKind::Bool => sink.word(0),
            TyKind::Char => sink.word(1),
            TyKind::Int(ty) => {
                sink.word(2);
                ty.put(sink);
            }
            TyKind::Uint(ty) => {
                sink.word(3);
                ty.put(sink);
            }
            TyKind::Float(ty) => {
                sink.word(4);
                ty.put(sink);
            }
            TyKind::Adt(adt, args) => {
                sink.word(5);
                adt.put(sink);
                args.put(sink);
            }
            TyKind::Foreign(def_id) => {
                sink.word(6);
                def_id.put(sink);
            }
            TyKind::Str => sink.word(7),
            TyKind::Array(ty, len) => {
                sink.word(8);
                ty.put(sink);
                len.put(sink);
            }
            TyKind::Pat(ty, pat) => {
                sink.word(9);
                ty.put(sink);
                pat.put(sink);
            }
            TyKind::Slice(ty) => {
                sink.word(10);
                ty.put(sink);
            }
            TyKind::RawPtr(ty, mutability) => {
                sink.word(11);
                ty.put(sink);
                mutability.put(sink);
            }
            TyKind::Ref(region, ty, mutability) => {
                sink.word(12);
                region.put(sink);
                ty.put(sink);
                mutability.put(sink);
            }
            TyKind::FnDef(def_id, args) => {
                sink.word(13);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::FnPtr(sig_tys, header) => {
                sink.word(14);
                sig_tys.put(sink);
                header.put(sink);
            }
            TyKind::UnsafeBinder(inner) => {
                sink.word(15);
                inner.put(sink);
            }
            TyKind::Dynamic(predicates, region) => {
                sink.word(16);
                predicates.put(sink);
                region.put(sink);
            }
            TyKind::Closure(def_id, args) => {
                sink.word(17);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::CoroutineClosure(def_id, args) => {
                sink.word(18);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::Coroutine(def_id, args) => {
                sink.word(19);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::CoroutineWitness(def_id, args) => {
                sink.word(20);
                def_id.put(sink);
                args.put(sink);
            }
            TyKind::Never => sink.word(21),
            TyKind::Tuple(tys) => {
                sink.word(22);
                tys.put(sink);
            }
            TyKind::Alias(alias) => {
                sink.word(23);
                alias.put(sink);
            }
            TyKind::Param(param) => {
                sink.word(24);
                param.put(sink);
            }
            TyKind::Bound(index, bound) => {
                sink.word(25);
                index.put(sink);
                bound.put(sink);
            }
            TyKind::Placeholder(placeholder) => {
                sink.word(26);
                placeholder.put(sink);
            }
            TyKind::Infer(infer) => {
                sink.word(27);
                infer.put(sink);
            }
            TyKind::Error(error) => {
                sink.word(28);
                error.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => TyKind::Bool,
            1 => TyKind::Char,
            2 => TyKind::Int(Code::get(source)?),
            3 => TyKind::Uint(Code::get(source)?),
            4 => TyKind::Float(Code::get(source)?),
            5 => TyKind::Adt(Code::get(source)?, Code::get(source)?),
            6 => TyKind::Foreign(Code::get(source)?),
            7 => TyKind::Str,
            8 => TyKind::Array(Code::get(source)?, Code::get(source)?),
            9 => TyKind::Pat(Code::get(source)?, Code::get(source)?),
            10 => TyKind::Slice(Code::get(source)?),
            11 => TyKind::RawPtr(Code::get(source)?, Code::get(source)?),
            12 => TyKind::Ref(Code::get(source)?, Code::get(source)?, Code::get(source)?),
            13 => TyKind::FnDef(Code::get(source)?, Code::get(source)?),
            14 => TyKind::FnPtr(Code::get(source)?, Code::get(source)?),
            15 => TyKind::UnsafeBinder(Code::get(source)?),
            16 => TyKind::Dynamic(Code::get(source)?, Code::get(source)?),
            17 => TyKind::Closure(Code::get(source)?, Code::get(source)?),
            18 => TyKind::CoroutineClosure(Code::get(source)?, Code::get(source)?),
            19 => TyKind::Coroutine(Code::get(source)?, Code::get(source)?),
            20 => TyKind::CoroutineWitness(Code::get(source)?, Code::get(source)?),
            21 => TyKind::Never,
            22 => TyKind::Tuple(Code::get(source)?),
            23 => TyKind::Alias(Code::get(source)?),
            24 => TyKind::Param(Code::get(source)?),
            25 => TyKind::Bound(Code::get(source)?, Code::get(source)?),
            26 => TyKind::Placeholder(Code::get(source)?),
            27 => TyKind::Infer(Code::get(source)?),
            28 => TyKind::Error(Code::get(source)?),
            _ => return Err(source.invalid("a type")),
        })
    }
}

impl<'db> Code<'db> for RegionKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            RegionKind::ReEarlyParam(param) => {
                sink.word(0);
                param.put(sink);
            }
            RegionKind::ReBound(index, bound) => {
                sink.word(1);
                index.put(sink);
                bound.put(sink);
            }
            RegionKind::ReLateParam(param) => {
                sink.word(2);
                param.put(sink);
            }
            RegionKind::ReStatic => sink.word(3),
            RegionKind::ReVar(var) => {
                sink.word(4);
                var.put(sink);
            }
            RegionKind::RePlaceholder(placeholder) => {
                sink.word(5);
                placeholder.put(sink);
            }
            RegionKind::ReErased => sink.word(6),
            RegionKind::ReError(error) => {
                sink.word(7);
                error.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => RegionKind::ReEarlyParam(Code::get(source)?),
            1 => RegionKind::ReBound(Code::get(source)?, Code::get(source)?),
            2 => RegionKind::ReLateParam(Code::get(source)?),
            3 => RegionKind::ReStatic,
            4 => RegionKind::ReVar(Code::get(source)?),
            5 => RegionKind::RePlaceholder(Code::get(source)?),
            6 => RegionKind::ReErased,
            7 => RegionKind::ReError(Code::get(source)?),
            _ => return Err(source.invalid("a region")),
        })
    }
}

impl<'db> Code<'db> for ConstKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            ConstKind::Param(param) => {
                sink.word(0);
                param.put(sink);
            }
            ConstKind::Infer(infer) => {
                sink.word(1);
                infer.put(sink);
            }
            ConstKind::Bound(index, bound) => {
                sink.word(2);
                index.put(sink);
                bound.put(sink);
            }
            ConstKind::Placeholder(placeholder) => {
                sink.word(3);
                placeholder.put(sink);
            }
            ConstKind::Unevaluated(unevaluated) => {
                sink.word(4);
                unevaluated.put(sink);
            }
            ConstKind::Value(value) => {
                sink.word(5);
                value.put(sink);
            }
            ConstKind::Error(error) => {
                sink.word(6);
                error.put(sink);
            }
            ConstKind::Expr(expr) => {
                sink.word(7);
                expr.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => ConstKind::Param(Code::get(source)?),
            1 => ConstKind::Infer(Code::get(source)?),
            2 => ConstKind::Bound(Code::get(source)?, Code::get(source)?),
            3 => ConstKind::Placeholder(Code::get(source)?),
            4 => ConstKind::Unevaluated(Code::get(source)?),
            5 => ConstKind::Value(Code::get(source)?),
            6 => ConstKind::Error(Code::get(source)?),
            7 => ConstKind::Expr(Code::get(source)?),
            _ => return Err(source.invalid("a constant")),
        })
    }
}

impl<'db> Code<'db> for PatternKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            PatternKind::Range { start, end } => {
                sink.word(0);
                start.put(sink);
                end.put(sink);
            }
            PatternKind::Or(pats) => {
                sink.word(1);
                pats.put(sink);
            }
            PatternKind::NotNull => sink.word(2),
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => PatternKind::Range { start: Code::get(source)?, end: Code::get(source)? },
            1 => PatternKind::Or(Code::get(source)?),
            2 => PatternKind::NotNull,
            _ => return Err(source.invalid("a pattern")),
        })
    }
}

impl<'db> Code<'db> for ClauseKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            ClauseKind::Trait(predicate) => {
                sink.word(0);
                predicate.put(sink);
            }
            ClauseKind::RegionOutlives(predicate) => {
                sink.word(1);
                predicate.put(sink);
            }
            ClauseKind::TypeOutlives(predicate) => {
                sink.word(2);
                predicate.put(sink);
            }
            ClauseKind::Projection(predicate) => {
                sink.word(3);
                predicate.put(sink);
            }
            ClauseKind::ConstArgHasType(konst, ty) => {
                sink.word(4);
                konst.put(sink);
                ty.put(sink);
            }
            ClauseKind::WellFormed(term) => {
                sink.word(5);
                term.put(sink);
            }
            ClauseKind::ConstEvaluatable(konst) => {
                sink.word(6);
                konst.put(sink);
            }
            ClauseKind::HostEffect(predicate) => {
                sink.word(7);
                predicate.put(sink);
            }
            ClauseKind::UnstableFeature(symbol) => {
                sink.word(8);
                symbol.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => ClauseKind::Trait(Code::get(source)?),
            1 => ClauseKind::RegionOutlives(Code::get(source)?),
            2 => ClauseKind::TypeOutlives(Code::get(source)?),
            3 => ClauseKind::Projection(Code::get(source)?),
            4 => ClauseKind::ConstArgHasType(Code::get(source)?, Code::get(source)?),
            5 => ClauseKind::WellFormed(Code::get(source)?),
            6 => ClauseKind::ConstEvaluatable(Code::get(source)?),
            7 => ClauseKind::HostEffect(Code::get(source)?),
            8 => ClauseKind::UnstableFeature(Code::get(source)?),
            _ => return Err(source.invalid("a clause")),
        })
    }
}

impl<'db> Code<'db> for PredicateKind<'db> {
    fn put(&self, sink: &mut impl Sink<'db>) {
        match *self {
            PredicateKind::Clause(clause) => {
                sink.word(0);
                clause.put(sink);
            }
            PredicateKind::DynCompatible(def_id) => {
                sink.word(1);
                def_id.put(sink);
            }
            PredicateKind::Subtype(predicate) => {
                sink.word(2);
                predicate.put(sink);
            }
            PredicateKind::Coerce(predicate) => {
                sink.word(3);
                predicate.put(sink);
            }
            PredicateKind::ConstEquate(a, b) => {
                sink.word(4);
                a.put(sink);
                b.put(sink);
            }
            PredicateKind::Ambiguous => sink.word(5),
            PredicateKind::NormalizesTo(predicate) => {
                sink.word(6);
                predicate.put(sink);
            }
            PredicateKind::AliasRelate(a, b, direction) => {
                sink.word(7);
                a.put(sink);
                b.put(sink);
                direction.put(sink);
            }
        }
    }

    fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        Ok(match source.word()? {
            0 => PredicateKind::Clause(Code::get(source)?),
            1 => PredicateKind::DynCompatible(Code::get(source)?),
            2 => PredicateKind::Subtype(Code::get(source)?),
            3 => PredicateKind::Coerce(Code::get(source)?),
            4 => PredicateKind::ConstEquate(Code::get(source)?, Code::get(source)?),
            5 => PredicateKind::Ambiguous,
            6 => PredicateKind::NormalizesTo(Code::get(source)?),
            7 => PredicateKind::AliasRelate(
                Code::get(source)?,
                Code::get(source)?,
                Code::get(source)?,
            ),
            _ => return Err(source.invalid("a predicate")),
        })
    }
}

/// Interned values as children: their row, or their place.
macro_rules! child_code {
    ($($ty:ident),* $(,)?) => {$(
        impl<'db> Code<'db> for $ty<'db> {
            #[inline]
            fn put(&self, sink: &mut impl Sink<'db>) {
                sink.node(*self);
            }
            #[inline]
            fn get<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                source.node()
            }
        }
    )*};
}

child_code!(
    Ty,
    Region,
    Const,
    Pattern,
    Predicate,
    ValTree,
    GenericArgs,
    Tys,
    Consts,
    BoundVarKinds,
    PatList,
    BoundExistentialPredicates,
    Clauses,
);

/// The interned kinds with a structure: a persisted node is the words of their row.
macro_rules! row_node {
    ($($ty:ident: $kind:ty),* $(,)?) => {$(
        impl<'db> Node<'db> for $ty<'db> {
            const KIND: Kind = Kind::$ty;
            #[inline]
            fn row(self) -> NonZeroU32 {
                $ty::row(self)
            }
            #[inline]
            fn at(row: NonZeroU32) -> Self {
                $ty::at(row)
            }
            fn put_node(self, sink: &mut impl Sink<'db>) {
                self.kind().put(sink);
            }
            fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                Ok($ty::intern(<$kind>::get(source)?))
            }
        }
    )*};
}

row_node!(
    Ty: TyKind<'db>,
    Region: RegionKind<'db>,
    Const: ConstKind<'db>,
    Pattern: PatternKind<'db>,
    Predicate: Binder<'db, PredicateKind<'db>>,
);

impl<'db> Node<'db> for ValTree<'db> {
    const KIND: Kind = Kind::ValTree;
    #[inline]
    fn row(self) -> NonZeroU32 {
        ValTree::row(self)
    }
    #[inline]
    fn at(row: NonZeroU32) -> Self {
        ValTree::at(row)
    }
    fn put_node(self, sink: &mut impl Sink<'db>) {
        match *self.inner() {
            ValTreeKind::Leaf(scalar) => {
                sink.word(0);
                scalar.put(sink);
            }
            ValTreeKind::Branch(consts) => {
                sink.word(1);
                consts.put(sink);
            }
        }
    }
    fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
        let kind = match source.word()? {
            0 => ValTreeKind::Leaf(Code::get(source)?),
            1 => ValTreeKind::Branch(Code::get(source)?),
            _ => return Err(source.invalid("a value tree")),
        };
        Ok(ValTree::new(kind))
    }
}

/// The interned lists: a persisted node is their length and their items.
macro_rules! list_node {
    ($($ty:ident: $item:ty),* $(,)?) => {$(
        impl<'db> Node<'db> for $ty<'db> {
            const KIND: Kind = Kind::$ty;
            #[inline]
            fn row(self) -> NonZeroU32 {
                $ty::row(self)
            }
            #[inline]
            fn at(row: NonZeroU32) -> Self {
                $ty::at(row)
            }
            fn put_node(self, sink: &mut impl Sink<'db>) {
                sink.word(self.len() as u32);
                self.iter().for_each(|item| item.put(sink));
            }
            fn get_node<S: Source<'db>>(source: &mut S) -> Result<Self, S::Error> {
                let len = source.count()?;
                let items = (0..len)
                    .map(|_| <$item>::get(source))
                    .collect::<Result<SmallVec<[$item; 8]>, _>>()?;
                Ok($ty::new_from_slice(&items))
            }
        }
    )*};
}

list_node!(
    GenericArgs: GenericArg<'db>,
    Tys: Ty<'db>,
    Consts: Const<'db>,
    BoundVarKinds: BoundVariableKind<'db>,
    PatList: Pattern<'db>,
    BoundExistentialPredicates: BoundExistentialPredicate<'db>,
    Clauses: Clause<'db>,
);

blaze_format::record! {
    /// A value of a persisted table: which kind, and where its words start.
    #[derive(Clone, Copy, Debug)]
    struct NodeRec: 1 {
        kind: u32,
        at: u32,
    }
}

blaze_format::record! {
    /// A persisted table of types and the other interned values they hold, each after the
    /// values it holds. A value's words end where the next value's start.
    #[derive(Clone, Copy, Debug)]
    struct TableRec: 1 {
        nodes: Slice<NodeRec>,
        words: Slice<u32>,
    }
}

/// Writes the types (and regions, constants, lists and clauses) of a query result into a table
/// of its own, each once: each is named by its place in the table, from the `u32` its method
/// returns.
pub struct TyTableWriter<'db> {
    /// The place of each value written, by kind and row.
    places: FxHashMap<(Kind, NonZeroU32), u32>,
    nodes: Vec<NodeRec>,
    words: Vec<u32>,
    _db: PhantomData<DbInterner<'db>>,
}

impl<'db> TyTableWriter<'db> {
    /// A writer of a table read back with an interner of the same database (any revision).
    pub fn new(_interner: DbInterner<'db>) -> Self {
        Self::empty()
    }

    fn empty() -> Self {
        TyTableWriter {
            places: FxHashMap::default(),
            nodes: Vec::new(),
            words: Vec::new(),
            _db: PhantomData,
        }
    }

    pub fn ty(&mut self, ty: Ty<'db>) -> u32 {
        self.place(ty)
    }

    pub fn region(&mut self, region: Region<'db>) -> u32 {
        self.place(region)
    }

    pub fn konst(&mut self, konst: Const<'db>) -> u32 {
        self.place(konst)
    }

    pub fn args(&mut self, args: GenericArgs<'db>) -> u32 {
        self.place(args)
    }

    pub fn clauses(&mut self, clauses: Clauses<'db>) -> u32 {
        self.place(clauses)
    }

    /// Writes the table, returning where it is in `out` for [`TyTableReader::open`].
    pub fn write(self, out: &mut Writer) -> u64 {
        let nodes = out.push_slice(&self.nodes);
        let words = out.push_slice(&self.words);
        let at = out.next::<TableRec>();
        let field = |offset: usize| at + offset as u64;
        out.push(&TableRec {
            nodes: Slice::between(field(std::mem::offset_of!(TableRec, nodes)), nodes, self.nodes.len()),
            words: Slice::between(field(std::mem::offset_of!(TableRec, words)), words, self.words.len()),
        })
    }

    /// The place of `node`, written with what it holds if it is not in the table yet.
    fn place<N: Node<'db>>(&mut self, node: N) -> u32 {
        let key = (N::KIND, node.row());
        if let Some(&place) = self.places.get(&key) {
            return place;
        }
        let mut words = Vec::new();
        ensure_sufficient_stack(|| node.put_node(&mut Children { writer: self, words: &mut words }));
        let place = u32::try_from(self.nodes.len()).expect("a type table of fewer than 2^32 values");
        let at = u32::try_from(self.words.len()).expect("a type table of fewer than 2^32 words");
        self.nodes.push(NodeRec { kind: N::KIND as u32, at });
        self.words.extend(words);
        self.places.insert(key, place);
        place
    }
}

/// The words of a node being written: its children are written first.
struct Children<'w, 'db> {
    writer: &'w mut TyTableWriter<'db>,
    words: &'w mut Vec<u32>,
}

impl<'db> Sink<'db> for Children<'_, 'db> {
    fn word(&mut self, word: u32) {
        self.words.push(word);
    }

    fn node<N: Node<'db>>(&mut self, node: N) {
        let place = self.writer.place(node);
        self.words.push(place);
    }

    fn adt(&mut self, adt: AdtDef) {
        SolverDefId::from(adt.def_id()).put(self);
    }
}

/// A table [`TyTableWriter`] wrote, read back: every value checked and interned when it is
/// opened, so a value's place gives it with one lookup.
pub struct TyTableReader<'a, 'db> {
    /// Each value of the table by its place: its kind and its row.
    values: Vec<(Kind, NonZeroU32)>,
    _buffer: PhantomData<(&'a [u8], DbInterner<'db>)>,
}

impl<'a, 'db> TyTableReader<'a, 'db> {
    /// The table [`TyTableWriter::write`] wrote at `at` in `buffer`. An ADT's flags are read
    /// from `interner`'s database.
    pub fn open(
        interner: DbInterner<'db>,
        buffer: &'a [u8],
        at: u64,
    ) -> Result<Self, FormatError> {
        Self::open_with(buffer, at, |id| AdtDef::new(id, interner))
    }

    /// As [`Self::open`], an ADT made by `adt`.
    pub(crate) fn open_with(
        buffer: &'a [u8],
        at: u64,
        adt: impl Fn(AdtId) -> AdtDef,
    ) -> Result<Self, FormatError> {
        let table = record_at::<TableRec>(buffer, at)?;
        let nodes = table.nodes.follow(buffer)?;
        let words = table.words.follow(buffer)?;
        let mut values = Vec::with_capacity(nodes.len());
        for (place, node) in nodes.iter().enumerate() {
            let at = (node as *const NodeRec as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
            let start = node.at as usize;
            let end = nodes.get(place + 1).map_or(words.len(), |next| next.at as usize);
            if start > end || end > words.len() {
                return Err(FormatError::Invalid { at, what: "a type table value's words out of order" });
            }
            let kind = KINDS
                .get(node.kind as usize)
                .copied()
                .ok_or(FormatError::Invalid { at, what: "a type table value of no kind" })?;
            let mut source =
                Reading { values: &values, words: &words[start..end], adt: &adt, at };
            let row = ensure_sufficient_stack(|| match kind {
                Kind::Ty => Ty::get_node(&mut source).map(Ty::row),
                Kind::Region => Region::get_node(&mut source).map(Region::row),
                Kind::Const => Const::get_node(&mut source).map(Const::row),
                Kind::Pattern => Pattern::get_node(&mut source).map(Pattern::row),
                Kind::Predicate => Predicate::get_node(&mut source).map(Predicate::row),
                Kind::ValTree => ValTree::get_node(&mut source).map(ValTree::row),
                Kind::GenericArgs => GenericArgs::get_node(&mut source).map(GenericArgs::row),
                Kind::Tys => Tys::get_node(&mut source).map(Tys::row),
                Kind::Consts => Consts::get_node(&mut source).map(Consts::row),
                Kind::BoundVarKinds => {
                    BoundVarKinds::get_node(&mut source).map(BoundVarKinds::row)
                }
                Kind::PatList => PatList::get_node(&mut source).map(PatList::row),
                Kind::BoundExistentialPredicates => BoundExistentialPredicates::get_node(&mut source)
                    .map(BoundExistentialPredicates::row),
                Kind::Clauses => Clauses::get_node(&mut source).map(Clauses::row),
            })?;
            if !source.words.is_empty() {
                return Err(FormatError::Invalid { at, what: "a type table value with words left over" });
            }
            values.push((kind, row));
        }
        Ok(TyTableReader { values, _buffer: PhantomData })
    }

    pub fn ty(&self, at: u32) -> Result<Ty<'db>, FormatError> {
        self.value(at)
    }

    pub fn region(&self, at: u32) -> Result<Region<'db>, FormatError> {
        self.value(at)
    }

    pub fn konst(&self, at: u32) -> Result<Const<'db>, FormatError> {
        self.value(at)
    }

    pub fn args(&self, at: u32) -> Result<GenericArgs<'db>, FormatError> {
        self.value(at)
    }

    pub fn clauses(&self, at: u32) -> Result<Clauses<'db>, FormatError> {
        self.value(at)
    }

    fn value<N: Node<'db>>(&self, at: u32) -> Result<N, FormatError> {
        match self.values.get(at as usize) {
            Some(&(kind, row)) if kind == N::KIND => Ok(N::at(row)),
            Some(_) => {
                Err(FormatError::Invalid { at: u64::from(at), what: "a type table value of another kind" })
            }
            None => Err(FormatError::Invalid { at: u64::from(at), what: "a place past the type table" }),
        }
    }
}

/// The `T` at `at` in `buffer`, checked.
fn record_at<T: Record>(buffer: &[u8], at: u64) -> Result<&T, FormatError> {
    let bad = FormatError::OutOfBounds { at };
    let start = usize::try_from(at).map_err(|_| bad.clone())?;
    let end = start
        .checked_add(size_of::<T>())
        .filter(|&end| end <= buffer.len() && start >= HEADER_LEN)
        .ok_or(bad.clone())?;
    if !(buffer.as_ptr() as usize).wrapping_add(start).is_multiple_of(align_of::<T>()) {
        return Err(bad);
    }
    let bytes = &buffer[start..end];
    T::check(bytes, buffer)?;
    // SAFETY: `bytes` are in `buffer`, `size_of::<T>()` of them, aligned for `T`, and
    // `T::check` accepted them, which `Record`'s contract makes a valid `T`.
    Ok(unsafe { &*bytes.as_ptr().cast::<T>() })
}

/// The words of one value of a persisted table, its children among the values before it.
struct Reading<'r, A> {
    values: &'r [(Kind, NonZeroU32)],
    words: &'r [u32],
    adt: &'r A,
    /// Where the value's record is, for errors.
    at: u64,
}

impl<'db, A: Fn(AdtId) -> AdtDef> Source<'db> for Reading<'_, A> {
    type Error = FormatError;

    fn word(&mut self) -> Result<u32, FormatError> {
        let (&word, rest) = self.words.split_first().ok_or(FormatError::Invalid {
            at: self.at,
            what: "a type table value shorter than its kind",
        })?;
        self.words = rest;
        Ok(word)
    }

    fn node<N: Node<'db>>(&mut self) -> Result<N, FormatError> {
        let place = self.word()?;
        match self.values.get(place as usize) {
            Some(&(kind, row)) if kind == N::KIND => Ok(N::at(row)),
            Some(_) => Err(self.invalid("a child of another kind")),
            None => Err(self.invalid("a child that is not before its parent")),
        }
    }

    fn adt(&mut self) -> Result<AdtDef, FormatError> {
        match SolverDefId::get(self)? {
            SolverDefId::AdtId(id) => Ok((self.adt)(id)),
            _ => Err(self.invalid("an ADT that is another definition")),
        }
    }

    fn count(&mut self) -> Result<usize, FormatError> {
        let count = self.word()? as usize;
        match count <= self.words.len() {
            true => Ok(count),
            false => Err(self.invalid("a list longer than its words")),
        }
    }

    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Barrier};

    use blaze_format::{Aligned, Writer};
    use hir_def::{CallableDefId, FunctionId, TypeAliasId};
    use rustc_type_ir::{FlagComputation, Flags, INNERMOST, TypeFlags};

    use super::*;
    use crate::{db::GeneralConstId, next_solver::TermId};

    fn id<T: FromId>(index: u32) -> T {
        T::from_id(salsa::Id::from_bits(u64::from(index)))
    }

    fn adt(id: AdtId) -> AdtDef {
        AdtDef::from_parts(id, 0).expect("no flags are valid flags")
    }

    /// Values of every kind and every variant of each.
    struct Samples<'db> {
        tys: Vec<TyKind<'db>>,
        regions: Vec<RegionKind<'db>>,
        consts: Vec<ConstKind<'db>>,
        patterns: Vec<PatternKind<'db>>,
        predicates: Vec<Binder<'db, PredicateKind<'db>>>,
        args: GenericArgs<'db>,
    }

    fn samples<'db>() -> Samples<'db> {
        let u8 = Ty::intern(TyKind::Uint(UintTy::U8));
        let statik = Region::intern(RegionKind::ReStatic);
        let function: FunctionId = id(3);
        let generic = GenericDefId::FunctionId(function);
        let local = |n| Idx::from_raw(RawIdx::from_u32(n));
        let param = TypeParamId::from_unchecked(TypeOrConstParamId { parent: generic, local_id: local(0) });
        let const_param =
            ConstParamId::from_unchecked(TypeOrConstParamId { parent: generic, local_id: local(1) });
        let lifetime = LifetimeParamId { parent: generic, local_id: local(0) };
        let trait_ = TraitIdWrapper(id(4));
        let alias: TypeAliasId = id(5);
        let general = GeneralConstIdWrapper(GeneralConstId::ConstId(id(6)));
        let args = GenericArgs::new_from_slice(&[u8.into(), statik.into()]);
        let vars = BoundVarKinds::new_from_slice(&[
            BoundVariableKind::Region(BoundRegionKind::Anon),
            BoundVariableKind::Region(BoundRegionKind::NamedForPrinting(Symbol)),
            BoundVariableKind::Region(BoundRegionKind::Named(SolverDefId::FunctionId(function))),
            BoundVariableKind::Region(BoundRegionKind::ClosureEnv),
            BoundVariableKind::Ty(BoundTyKind::Anon),
            BoundVariableKind::Ty(BoundTyKind::Param(SolverDefId::TypeAliasId(alias))),
            BoundVariableKind::Const,
        ]);
        let seven = Const::intern(ConstKind::Value(ValueConst::new(
            u8,
            ValTreeKind::Leaf(ScalarInt::from(7u8)),
        )));
        let pair = Tys::new_from_slice(&[u8, Ty::intern(TyKind::Bool)]);
        let trait_ref = assemble!(TraitRef<'db> { def_id: trait_, args: args });
        let alias_term = |kind| assemble!(AliasTerm<'db> { args: args, kind: kind });
        let alias_ty = |kind| assemble!(AliasTy<'db> { args: args, kind: kind });
        let bound_ty = |var, kind| BoundTy { var: BoundVar::from_u32(var), kind };
        let bound_region = |var, kind| BoundRegion { var: BoundVar::from_u32(var), kind };
        let range = PatternKind::Range { start: seven, end: seven };

        let tys = vec![
            TyKind::Bool,
            TyKind::Char,
            TyKind::Int(IntTy::I32),
            TyKind::Uint(UintTy::U128),
            TyKind::Float(FloatTy::F64),
            TyKind::Adt(adt(AdtId::StructId(id(1))), args),
            TyKind::Adt(adt(AdtId::EnumId(id(2))), GenericArgs::empty()),
            TyKind::Foreign(TypeAliasIdWrapper(alias)),
            TyKind::Str,
            TyKind::Array(u8, seven),
            TyKind::Pat(u8, Pattern::intern(range)),
            TyKind::Slice(u8),
            TyKind::RawPtr(u8, Mutability::Mut),
            TyKind::Ref(statik, u8, Mutability::Not),
            TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(function)), args),
            TyKind::FnPtr(
                Binder::bind_with_vars(rustc_type_ir::FnSigTys { inputs_and_output: pair }, vars),
                FnHeader { fn_sig_kind: FnSigKind::new(ExternAbi::C { unwind: false }, Safety::Safe, true) },
            ),
            TyKind::FnPtr(
                Binder::dummy(rustc_type_ir::FnSigTys { inputs_and_output: pair }),
                FnHeader { fn_sig_kind: FnSigKind::new(ExternAbi::Rust, Safety::Unsafe, false) },
            ),
            TyKind::UnsafeBinder(Binder::bind_with_vars(u8, vars).into()),
            TyKind::Dynamic(
                BoundExistentialPredicates::new_from_slice(&[
                    Binder::bind_with_vars(
                        ExistentialPredicate::Trait(assemble!(ExistentialTraitRef<'db> {
                            def_id: trait_,
                            args: args,
                        })),
                        vars,
                    ),
                    Binder::dummy(ExistentialPredicate::Projection(assemble!(
                        ExistentialProjection<'db> {
                            def_id: TraitAssocTermId(TermId::TypeAliasId(alias)),
                            args: args,
                            term: u8.into(),
                        }
                    ))),
                    Binder::dummy(ExistentialPredicate::AutoTrait(trait_)),
                ]),
                statik,
            ),
            TyKind::Closure(ClosureIdWrapper(id(7)), args),
            TyKind::CoroutineClosure(CoroutineClosureIdWrapper(id(8)), args),
            TyKind::Coroutine(CoroutineIdWrapper(id(9)), args),
            TyKind::CoroutineWitness(CoroutineIdWrapper(id(9)), args),
            TyKind::Never,
            TyKind::Tuple(pair),
            TyKind::Tuple(Tys::empty()),
            TyKind::Alias(alias_ty(AliasTyKind::Projection { def_id: TraitAssocTyId(alias) })),
            TyKind::Alias(alias_ty(AliasTyKind::Inherent { def_id: InherentAssocTyId(alias) })),
            TyKind::Alias(alias_ty(AliasTyKind::Opaque { def_id: OpaqueTyIdWrapper(id(10)) })),
            TyKind::Alias(alias_ty(AliasTyKind::Free { def_id: FreeTyAliasId(alias) })),
            TyKind::Param(ParamTy { id: param, index: 0 }),
            TyKind::Bound(BoundVarIndexKind::Bound(INNERMOST), bound_ty(4, BoundTyKind::Anon)),
            TyKind::Bound(
                BoundVarIndexKind::Canonical,
                bound_ty(5, BoundTyKind::Param(SolverDefId::TypeAliasId(alias))),
            ),
            TyKind::Placeholder(PlaceholderType::new(
                UniverseIndex::from_u32(2),
                bound_ty(0, BoundTyKind::Anon),
            )),
            TyKind::Infer(InferTy::TyVar(TyVid::from_u32(1))),
            TyKind::Infer(InferTy::IntVar(IntVid::from_u32(2))),
            TyKind::Infer(InferTy::FloatVar(FloatVid::from_u32(3))),
            TyKind::Infer(InferTy::FreshTy(4)),
            TyKind::Infer(InferTy::FreshIntTy(5)),
            TyKind::Infer(InferTy::FreshFloatTy(6)),
            TyKind::Error(ErrorGuaranteed),
        ];

        let regions = vec![
            RegionKind::ReEarlyParam(EarlyParamRegion { id: lifetime, index: 0 }),
            RegionKind::ReBound(
                BoundVarIndexKind::Bound(DebruijnIndex::from_u32(1)),
                bound_region(2, BoundRegionKind::Named(SolverDefId::FunctionId(function))),
            ),
            RegionKind::ReBound(BoundVarIndexKind::Canonical, bound_region(0, BoundRegionKind::Anon)),
            RegionKind::ReLateParam(LateParamRegion {
                scope: SolverDefId::FunctionId(function),
                bound_region: bound_region(1, BoundRegionKind::ClosureEnv),
            }),
            RegionKind::ReStatic,
            RegionKind::ReVar(RegionVid::from_u32(9)),
            RegionKind::RePlaceholder(PlaceholderRegion::new(
                UniverseIndex::from_u32(1),
                bound_region(0, BoundRegionKind::NamedForPrinting(Symbol)),
            )),
            RegionKind::ReErased,
            RegionKind::ReError(ErrorGuaranteed),
        ];

        let consts = vec![
            ConstKind::Param(ParamConst { id: const_param, index: 1 }),
            ConstKind::Infer(InferConst::Var(ConstVid::from_u32(3))),
            ConstKind::Infer(InferConst::Fresh(2)),
            ConstKind::Bound(BoundVarIndexKind::Bound(INNERMOST), BoundConst::new(BoundVar::from_u32(6))),
            ConstKind::Bound(BoundVarIndexKind::Canonical, BoundConst::new(BoundVar::from_u32(0))),
            ConstKind::Placeholder(PlaceholderConst::new(
                UniverseIndex::ROOT,
                BoundConst::new(BoundVar::from_u32(0)),
            )),
            ConstKind::Unevaluated(UnevaluatedConst { def: general, args }),
            ConstKind::Value(ValueConst::new(u8, ValTreeKind::Leaf(ScalarInt::from(7u8)))),
            ConstKind::Value(ValueConst::new(
                Ty::intern(TyKind::Tuple(pair)),
                ValTreeKind::Branch(Consts::new_from_slice(&[seven, seven])),
            )),
            ConstKind::Error(ErrorGuaranteed),
            ConstKind::Expr(ExprConst),
        ];

        let patterns = vec![
            range,
            PatternKind::Or(PatList::new_from_slice(&[Pattern::intern(range), Pattern::intern(PatternKind::NotNull)])),
            PatternKind::NotNull,
        ];

        let clause = |kind| Binder::dummy(PredicateKind::Clause(kind));
        let normalizes = |kind, term| {
            Binder::dummy(PredicateKind::NormalizesTo(NormalizesTo { alias: alias_term(kind), term }))
        };
        let predicates = vec![
            clause(ClauseKind::Trait(TraitPredicate { trait_ref, polarity: PredicatePolarity::Positive })),
            clause(ClauseKind::Trait(TraitPredicate { trait_ref, polarity: PredicatePolarity::Negative })),
            clause(ClauseKind::RegionOutlives(rustc_type_ir::OutlivesPredicate(statik, statik))),
            clause(ClauseKind::TypeOutlives(rustc_type_ir::OutlivesPredicate(u8, statik))),
            clause(ClauseKind::Projection(ProjectionPredicate {
                projection_term: alias_term(AliasTermKind::ProjectionTy { def_id: TraitAssocTyId(alias) }),
                term: u8.into(),
            })),
            clause(ClauseKind::ConstArgHasType(seven, u8)),
            clause(ClauseKind::WellFormed(u8.into())),
            clause(ClauseKind::ConstEvaluatable(seven)),
            clause(ClauseKind::HostEffect(HostEffectPredicate { trait_ref, constness: BoundConstness::Const })),
            clause(ClauseKind::HostEffect(HostEffectPredicate { trait_ref, constness: BoundConstness::Maybe })),
            clause(ClauseKind::UnstableFeature(Symbol)),
            Binder::dummy(PredicateKind::DynCompatible(trait_)),
            Binder::dummy(PredicateKind::Subtype(SubtypePredicate { a_is_expected: true, a: u8, b: u8 })),
            Binder::dummy(PredicateKind::Coerce(CoercePredicate { a: u8, b: u8 })),
            Binder::dummy(PredicateKind::ConstEquate(seven, seven)),
            Binder::dummy(PredicateKind::Ambiguous),
            normalizes(AliasTermKind::InherentTy { def_id: InherentAssocTyId(alias) }, u8.into()),
            normalizes(AliasTermKind::OpaqueTy { def_id: OpaqueTyIdWrapper(id(10)) }, u8.into()),
            normalizes(AliasTermKind::FreeTy { def_id: FreeTyAliasId(alias) }, u8.into()),
            normalizes(AliasTermKind::UnevaluatedConst { def_id: general }, seven.into()),
            normalizes(AliasTermKind::ProjectionConst { def_id: TraitAssocConstId(id(6)) }, seven.into()),
            normalizes(AliasTermKind::FreeConst { def_id: FreeConstAliasId(id(6)) }, seven.into()),
            normalizes(AliasTermKind::InherentConst { def_id: InherentAssocConstId(id(6)) }, seven.into()),
            Binder::dummy(PredicateKind::AliasRelate(
                u8.into(),
                seven.into(),
                AliasRelationDirection::Subtype,
            )),
            Binder::dummy(PredicateKind::AliasRelate(u8.into(), u8.into(), AliasRelationDirection::Equate)),
            Binder::bind_with_vars(
                PredicateKind::Clause(ClauseKind::Trait(TraitPredicate {
                    trait_ref,
                    polarity: PredicatePolarity::Positive,
                })),
                vars,
            ),
        ];

        Samples { tys, regions, consts, patterns, predicates, args }
    }

    #[test]
    fn interned_values_are_four_bytes() {
        assert_eq!(size_of::<Option<Ty<'static>>>(), 4);
        assert_eq!(size_of::<Option<Region<'static>>>(), 4);
        assert_eq!(size_of::<Option<Const<'static>>>(), 4);
        assert_eq!(size_of::<Option<Pattern<'static>>>(), 4);
        assert_eq!(size_of::<Option<Predicate<'static>>>(), 4);
        assert_eq!(size_of::<Option<Clause<'static>>>(), 4);
        assert_eq!(size_of::<Option<ValTree<'static>>>(), 4);
        assert_eq!(size_of::<Option<GenericArg<'static>>>(), 4);
        assert_eq!(size_of::<Option<Term<'static>>>(), 4);
        assert_eq!(size_of::<Option<GenericArgs<'static>>>(), 4);
        assert_eq!(size_of::<Option<Clauses<'static>>>(), 4);
    }

    #[test]
    fn a_value_interned_twice_has_one_index() {
        let samples = samples();
        for &kind in &samples.tys {
            let ty = Ty::intern(kind);
            assert_eq!(Ty::intern(kind).index(), ty.index());
        }
        for &kind in &samples.regions {
            assert_eq!(Region::intern(kind), Region::intern(kind));
        }
        for &kind in &samples.consts {
            assert_eq!(Const::intern(kind).index(), Const::intern(kind).index());
        }
        for &kind in &samples.predicates {
            assert_eq!(Predicate::intern(kind), Predicate::intern(kind));
        }
        let distinct: HashSet<_> = samples.tys.iter().map(|&kind| Ty::intern(kind)).collect();
        assert_eq!(distinct.len(), samples.tys.len());
        assert_eq!(
            GenericArgs::new_from_slice(samples.args.as_slice()).index(),
            samples.args.index()
        );
    }

    #[test]
    fn every_kind_reads_back_from_its_row() {
        let samples = samples();
        for &kind in &samples.tys {
            assert!(Ty::intern(kind).kind() == kind);
        }
        for &kind in &samples.regions {
            assert!(Region::intern(kind).kind() == kind);
        }
        for &kind in &samples.consts {
            assert!(Const::intern(kind).kind() == kind);
        }
        for &kind in &samples.patterns {
            assert!(Pattern::intern(kind).kind() == kind);
        }
        for &kind in &samples.predicates {
            assert!(Predicate::intern(kind).kind() == kind);
        }
    }

    #[test]
    fn flags_are_those_computed_again() {
        let samples = samples();
        for &kind in &samples.tys {
            let ty = Ty::intern(kind);
            let computed = FlagComputation::for_kind(&kind);
            assert_eq!(ty.flags(), computed.flags);
            assert_eq!(ty.outer_exclusive_binder(), computed.outer_exclusive_binder);
        }
        for &kind in &samples.consts {
            let konst = Const::intern(kind);
            let computed = FlagComputation::for_const_kind(&kind);
            assert_eq!(konst.flags(), computed.flags);
            assert_eq!(konst.outer_exclusive_binder(), computed.outer_exclusive_binder);
        }
        for &kind in &samples.predicates {
            let predicate = Predicate::intern(kind);
            let computed = FlagComputation::for_predicate(kind);
            assert_eq!(predicate.flags(), computed.flags);
            assert_eq!(predicate.outer_exclusive_binder(), computed.outer_exclusive_binder);
        }
        let clauses: Vec<_> =
            samples.predicates.iter().map(|&kind| Clause(Predicate::intern(kind))).collect();
        let computed = FlagComputation::for_clauses(&clauses);
        let clauses = Clauses::new_from_slice(&clauses);
        assert_eq!(clauses.flags(), computed.flags);
        assert_eq!(clauses.outer_exclusive_binder(), computed.outer_exclusive_binder);
        for &kind in &samples.patterns {
            let pattern = Pattern::intern(kind);
            let (flags, binder) = match kind {
                PatternKind::Range { start, end } => (
                    FlagComputation::for_const_kind(&start.kind()).flags
                        | FlagComputation::for_const_kind(&end.kind()).flags,
                    start.outer_exclusive_binder().max(end.outer_exclusive_binder()),
                ),
                PatternKind::Or(pats) => (
                    pats.iter().fold(TypeFlags::empty(), |flags, pat| flags | pat.flags()),
                    pats.iter().map(|pat| pat.outer_exclusive_binder()).max().unwrap_or(INNERMOST),
                ),
                PatternKind::NotNull => (TypeFlags::empty(), INNERMOST),
            };
            assert_eq!(pattern.flags(), flags);
            assert_eq!(pattern.outer_exclusive_binder(), binder);
        }
        let region = |kind| Region::intern(kind);
        assert_eq!(region(RegionKind::ReStatic).flags(), TypeFlags::HAS_FREE_REGIONS);
        assert_eq!(region(RegionKind::ReErased).flags(), TypeFlags::HAS_RE_ERASED);
        assert_eq!(
            region(RegionKind::ReVar(RegionVid::from_u32(0))).flags(),
            TypeFlags::HAS_FREE_REGIONS | TypeFlags::HAS_FREE_LOCAL_REGIONS | TypeFlags::HAS_RE_INFER
        );
        let bound = region(RegionKind::ReBound(
            BoundVarIndexKind::Bound(DebruijnIndex::from_u32(1)),
            BoundRegion { var: BoundVar::from_u32(0), kind: BoundRegionKind::Anon },
        ));
        assert_eq!(bound.flags(), TypeFlags::HAS_RE_BOUND);
        assert_eq!(bound.outer_exclusive_binder(), DebruijnIndex::from_u32(2));
    }

    #[test]
    fn eight_threads_interning_at_once_get_one_index_per_type() {
        const THREADS: u32 = 8;
        const TYPES: u32 = 4000;
        let barrier = Barrier::new(THREADS as usize);
        let kind = |n: u32| {
            let leaf = Ty::intern(TyKind::Infer(InferTy::FreshTy(n % 64)));
            match n % 3 {
                0 => TyKind::Slice(leaf),
                1 => TyKind::Tuple(Tys::new_from_slice(&[leaf, Ty::intern(TyKind::Infer(InferTy::FreshIntTy(n)))])),
                _ => TyKind::Array(leaf, Const::intern(ConstKind::Infer(InferConst::Fresh(n)))),
            }
        };
        let seen: Vec<Vec<u32>> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..THREADS)
                .map(|thread| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let mut indices = vec![0; TYPES as usize];
                        for i in 0..TYPES {
                            let n = (i * 7919 + thread * 104_729) % TYPES;
                            indices[n as usize] = Ty::intern(kind(n)).index();
                        }
                        indices
                    })
                })
                .collect();
            threads.into_iter().map(|thread| thread.join().unwrap()).collect()
        });
        assert!(seen.iter().all(|indices| *indices == seen[0]));
        assert_eq!(seen[0].iter().collect::<HashSet<_>>().len(), TYPES as usize);
        for (n, &index) in seen[0].iter().enumerate() {
            assert!(Ty::at(NonZeroU32::new(index).unwrap()).kind() == kind(n as u32));
        }
    }

    #[test]
    fn a_table_reads_back_as_written() {
        let samples = samples();
        let tys: Vec<_> = samples.tys.iter().map(|&kind| Ty::intern(kind)).collect();
        let regions: Vec<_> = samples.regions.iter().map(|&kind| Region::intern(kind)).collect();
        let consts: Vec<_> = samples.consts.iter().map(|&kind| Const::intern(kind)).collect();
        let clauses = Clauses::new_from_slice(
            &samples.predicates.iter().map(|&kind| Clause(Predicate::intern(kind))).collect::<Vec<_>>(),
        );
        let patterns: Vec<_> = samples
            .patterns
            .iter()
            .map(|&kind| Ty::intern(TyKind::Pat(tys[2], Pattern::intern(kind))))
            .collect();

        let mut out = Writer::new(blaze_format::schema("a type table, in a test"));
        let mut writer = TyTableWriter::empty();
        let ty_places: Vec<_> = tys.iter().chain(&patterns).map(|&ty| writer.ty(ty)).collect();
        let region_places: Vec<_> = regions.iter().map(|&region| writer.region(region)).collect();
        let const_places: Vec<_> = consts.iter().map(|&konst| writer.konst(konst)).collect();
        let args_place = writer.args(samples.args);
        let clauses_place = writer.clauses(clauses);
        assert_eq!(writer.ty(tys[0]), ty_places[0], "a value is written once");
        let table = writer.write(&mut out);
        let buffer = Aligned::copy_from(&out.finish(table));

        let reader = TyTableReader::open_with(&buffer, table, adt).unwrap();
        for (&ty, &place) in tys.iter().chain(&patterns).zip(&ty_places) {
            assert_eq!(reader.ty(place).unwrap(), ty);
        }
        for (&region, &place) in regions.iter().zip(&region_places) {
            assert_eq!(reader.region(place).unwrap(), region);
        }
        for (&konst, &place) in consts.iter().zip(&const_places) {
            assert_eq!(reader.konst(place).unwrap(), konst);
        }
        assert_eq!(reader.args(args_place).unwrap(), samples.args);
        assert_eq!(reader.clauses(clauses_place).unwrap(), clauses);
        assert!(reader.region(ty_places[0]).is_err(), "a place of another kind");
        assert!(reader.ty(u32::MAX).is_err(), "a place past the table");
    }

    #[test]
    fn a_table_that_names_a_later_value_is_refused() {
        let mut out = Writer::new(blaze_format::schema("a broken type table, in a test"));
        let nodes_at = out.push_slice(&[NodeRec { kind: Kind::Ty as u32, at: 0 }]);
        // A tuple of the list at place 5, which is not before it.
        let words_at = out.push_slice(&[22u32, 5]);
        let at = out.next::<TableRec>();
        let table = out.push(&TableRec {
            nodes: Slice::between(at + std::mem::offset_of!(TableRec, nodes) as u64, nodes_at, 1),
            words: Slice::between(at + std::mem::offset_of!(TableRec, words) as u64, words_at, 2),
        });
        let buffer = Aligned::copy_from(&out.finish(table));
        assert!(TyTableReader::open_with(&buffer, table, adt).is_err());
        assert!(TyTableReader::open_with(&buffer, table + 1, adt).is_err());
    }
}
