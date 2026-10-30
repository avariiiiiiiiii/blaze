//! [`InferenceResult`] in blaze's format (docs/v2/plan/design/data.md). [`InferenceRecord`] is
//! its `'static` form: the result's columns as they are, in three flat arrays of `u64`, `u32` and
//! `u8` words, each type an index in a type table of its own (`next_solver::persist`), each
//! definition its salsa id (the database that wrote a buffer is the one reading it, as
//! `crates/hir-def/src/hir/persist.rs` writes ids), and what few nodes have (diagnostics, the
//! closures' captures) as tagged `u32` words beside them. [`Persist`] writes the arrays and the
//! table's buffer into one buffer and reads them back in place;
//! [`InferenceRecordRef::to_result`] makes the result again, every column checked as it is
//! rebuilt.

use std::mem::offset_of;

use blaze_format::{
    ALIGN, Aligned, FormatError, Part, Persist, Record, SchemaHash, Slice, Writer, open_as, schema,
    schema_of,
};
use hir_def::{
    AdtId, AssocItemId, ConstId, ConstParamId, EnumId, EnumVariantId, FieldId, FunctionId,
    GenericDefId, GenericParamId, ImplId, LifetimeParamId, StaticId, StructId, TraitId,
    TupleFieldId, TupleId, TypeAliasId, TypeOrConstParamId, TypeParamId, UnionId, VariantId,
    hir::{
        ArithOp, BinaryOp, CmpOp, ExprId, ExprOrPatId, ExprOrPatIdPacked, LogicOp, Ordering,
        UnaryOp,
    },
};
use hir_expand::name::Name;
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use rustc_abi::ExternAbi;
use rustc_ast_ir::Mutability;
use rustc_type_ir::{ClosureKind, PredicatePolarity};
use salsa::plumbing::{AsId, FromId};
use smallvec::SmallVec;

use crate::{
    Span,
    db::{AnonConstId, InternedOpaqueTyId},
    infer::{
        Adjust, Adjustment, AllowTwoPhase, AutoBorrow, AutoBorrowMutability, CaptureInfo,
        CaptureSourceStack, CapturedPlace, ClosureData, ExplicitDropMethodUseKind,
        InferenceDiagnostic, InferenceResult, OverloadedDeref, PatAdjust, PatAdjustment,
        PointerCast, ReturnKind,
        cast::CastError,
        closure::analysis::{
            BorrowKind,
            expr_use_visitor::{FakeReadCause, Place, PlaceBase, Projection, ProjectionKind},
        },
        columns::{
            Csr, DenseKey, ExprOrPatColumn, ExprOrPatSet, FieldColumn, IdxSet, IndexColumn,
            ModeColumn, Range, SortedMap, TyColumn,
        },
    },
    lower::{
        diagnostics::IncorrectGenericsLenKind,
        persist::{Words, put_diagnostic, put_path_diagnostic},
    },
    method_resolution::CandidateId,
    next_solver::{
        DbInterner, FnSig, FnSigKind, GenericArgs, StoredFnSig, StoredGenericArgs, StoredTraitRef,
        StoredTy, StoredTys, TraitRef, Ty, Tys,
        abi::Safety,
        persist::{TyTableReader, TyTableWriter},
    },
    solver_errors::{SolverDiagnostic, SolverDiagnosticKind, StoredTraitPredicate},
};

const _: () = assert!(align_of::<u128>() == ALIGN, "the type table is placed as `u128`s");

/// The schema of the type table's own buffer inside an inference record.
const TY_TABLE_SCHEMA: SchemaHash = schema("hir_ty::infer::persist::TyTable");

macro_rules! columns {
    (
        $(#[$meta:meta])* $name:ident, $len:ident {
            $( $(#[$column_meta:meta])* $column:ident ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug)]
        enum $name { $( $(#[$column_meta])* $column ),* }
        const $len: usize = [$($name::$column),*].len();
    };
}

columns! {
    /// The columns of `u64` words: salsa ids and bitset words.
    C64, N64 {
        MethodFns,
        FieldVariants,
        VariantExprIds,
        VariantPatIds,
        AssocExprIds,
        AssocPatIds,
        OpaqueIds,
        MismatchedExprs,
        MismatchedPats,
        SkippedRefPats,
        CoercionCasts,
        ClosuresWithoutExpectation,
        AnonConsts,
    }
}

columns! {
    /// The columns of `u32` words: where each key's row is, types as indices in the type table,
    /// CSR ranges as `[start, len]` pairs, arena indices, and the side tables' words.
    C32, N32 {
        MethodAt,
        MethodArgs,
        FieldData,
        FieldLocals,
        /// `[tuple, index]` per tuple field.
        TupleFields,
        VariantExprAt,
        VariantPatAt,
        AssocExprAt,
        AssocPatAt,
        AssocExprArgs,
        AssocPatArgs,
        /// How many types each tuple of `tuple_field_access_types` has, its types in `TupleItems`.
        TupleLens,
        TupleItems,
        ExprTyAt,
        ExprTys,
        PatTyAt,
        PatTys,
        BindingTyAt,
        BindingTys,
        PlaceholderTyAt,
        PlaceholderTys,
        OpaqueTys,
        ExprAdjustRanges,
        ExprAdjustTargets,
        CoercionRanges,
        CoercionTargets,
        PatAdjustRanges,
        PatAdjustSources,
        /// `[asm, operand, type]` per `sym` operand.
        AsmSyms,
        ClosureExprs,
        /// The diagnostics, then the closures' data.
        Words,
    }
}

columns! {
    /// The columns of bytes: tags and codes, and the names diagnostics hold.
    C8, N8 {
        FieldTags,
        FieldKinds,
        VariantExprKinds,
        VariantPatKinds,
        AssocExprKinds,
        AssocPatKinds,
        ExprAdjustKinds,
        CoercionKinds,
        PatAdjustKinds,
        BindingModes,
        Text,
    }
}

blaze_format::record! {
    /// An inference result: its three arrays of words and where each column is in them
    /// (`[start, len]` in words), the type table's buffer and its root in it, the error type
    /// as an index in the table, `has_errors` as bit 0 of `flags`, and how many diagnostics
    /// the words start with.
    #[derive(Clone, Copy)]
    struct InferRoot: 1 {
        table_root: u64,
        u64s: Slice<u64>,
        u32s: Slice<u32>,
        bytes: Slice<u8>,
        table: Slice<u128>,
        spans64: [[u32; 2]; N64],
        spans32: [[u32; 2]; N32],
        spans8: [[u32; 2]; N8],
        error_ty: u32,
        flags: u32,
        diagnostics: u32,
        reserved: u32,
    }
}

const _: () = assert!(size_of::<InferRoot>() == 8 + 4 * 8 + (N64 + N32 + N8) * 8 + 4 * 4);

/// An [`InferenceResult`] in its `'static` form: what [`InferenceResult::to_record`] makes and
/// [`Persist`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceRecord {
    u64s: Vec<u64>,
    u32s: Vec<u32>,
    bytes: Vec<u8>,
    spans64: [[u32; 2]; N64],
    spans32: [[u32; 2]; N32],
    spans8: [[u32; 2]; N8],
    error_ty: u32,
    flags: u32,
    diagnostics: u32,
    /// The type table's buffer, header and all.
    table: Vec<u128>,
    table_root: u64,
}

/// An [`InferenceRecord`] read in place: what [`Persist::read`] gives, and
/// [`InferenceRecord::as_ref`]. Every column is inside its array.
#[derive(Debug, Clone, Copy)]
pub struct InferenceRecordRef<'a> {
    /// Where the record's root is in its buffer, for errors.
    at: u64,
    u64s: &'a [u64],
    u32s: &'a [u32],
    bytes: &'a [u8],
    spans64: &'a [[u32; 2]; N64],
    spans32: &'a [[u32; 2]; N32],
    spans8: &'a [[u32; 2]; N8],
    error_ty: u32,
    flags: u32,
    diagnostics: u32,
    table: &'a [u128],
    table_root: u64,
}

/// The type table's buffer as words, little-endian, to be placed with the columns at `u128`
/// alignment.
fn table_words(bytes: &[u8]) -> Vec<u128> {
    assert!(bytes.len().is_multiple_of(ALIGN), "a finished buffer is padded to whole words");
    bytes
        .chunks_exact(size_of::<u128>())
        .map(|chunk| u128::from_le_bytes(chunk.try_into().expect("a chunk of 16 bytes")))
        .collect()
}

/// The type table's buffer as the reader takes it, from [`table_words`]'s words. A copy, not
/// a view: the table is small next to the types it interns again.
fn table_bytes(words: &[u128]) -> Aligned {
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    Aligned::copy_from(&bytes)
}

fn salsa_id(bits: u64) -> Option<salsa::Id> {
    (bits as u32 != 0).then(|| salsa::Id::from_bits(bits))
}

fn id_bits(id: impl AsId) -> u64 {
    id.as_id().as_bits()
}

fn local<T>(id: Idx<T>) -> u32 {
    id.into_raw().into_u32()
}

fn local_of<T>(raw: u32) -> Idx<T> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

fn variant_key(variant: VariantId) -> (u8, u64) {
    match variant {
        VariantId::EnumVariantId(it) => (0, id_bits(it)),
        VariantId::StructId(it) => (1, id_bits(it)),
        VariantId::UnionId(it) => (2, id_bits(it)),
    }
}

fn variant_of(tag: u8, bits: u64) -> Option<VariantId> {
    let id = salsa_id(bits)?;
    Some(match tag {
        0 => EnumVariantId::from_id(id).into(),
        1 => StructId::from_id(id).into(),
        2 => UnionId::from_id(id).into(),
        _ => return None,
    })
}

fn candidate_key(candidate: CandidateId) -> (u8, u64) {
    match candidate {
        CandidateId::FunctionId(it) => (0, id_bits(it)),
        CandidateId::ConstId(it) => (1, id_bits(it)),
    }
}

fn candidate_of(tag: u8, bits: u64) -> Option<CandidateId> {
    let id = salsa_id(bits)?;
    Some(match tag {
        0 => FunctionId::from_id(id).into(),
        1 => ConstId::from_id(id).into(),
        _ => return None,
    })
}

/// Every [`Adjust`], by its code.
const ADJUSTS: [Adjust; 16] = [
    Adjust::NeverToAny,
    Adjust::Deref(None),
    Adjust::Deref(Some(OverloadedDeref(Mutability::Not))),
    Adjust::Deref(Some(OverloadedDeref(Mutability::Mut))),
    Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Not)),
    Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
        allow_two_phase_borrow: AllowTwoPhase::Yes,
    })),
    Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
        allow_two_phase_borrow: AllowTwoPhase::No,
    })),
    Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Not)),
    Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Mut)),
    Adjust::Pointer(PointerCast::ReifyFnPointer),
    Adjust::Pointer(PointerCast::UnsafeFnPointer),
    Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Safe)),
    Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Unsafe)),
    Adjust::Pointer(PointerCast::MutToConstPointer),
    Adjust::Pointer(PointerCast::ArrayToPointer),
    Adjust::Pointer(PointerCast::Unsize),
];

fn adjust_code(adjust: &Adjust) -> u8 {
    match *adjust {
        Adjust::NeverToAny => 0,
        Adjust::Deref(None) => 1,
        Adjust::Deref(Some(OverloadedDeref(Mutability::Not))) => 2,
        Adjust::Deref(Some(OverloadedDeref(Mutability::Mut))) => 3,
        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Not)) => 4,
        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
            allow_two_phase_borrow: AllowTwoPhase::Yes,
        })) => 5,
        Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut {
            allow_two_phase_borrow: AllowTwoPhase::No,
        })) => 6,
        Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Not)) => 7,
        Adjust::Borrow(AutoBorrow::RawPtr(Mutability::Mut)) => 8,
        Adjust::Pointer(PointerCast::ReifyFnPointer) => 9,
        Adjust::Pointer(PointerCast::UnsafeFnPointer) => 10,
        Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Safe)) => 11,
        Adjust::Pointer(PointerCast::ClosureFnPointer(Safety::Unsafe)) => 12,
        Adjust::Pointer(PointerCast::MutToConstPointer) => 13,
        Adjust::Pointer(PointerCast::ArrayToPointer) => 14,
        Adjust::Pointer(PointerCast::Unsize) => 15,
    }
}

const ARITH_OPS: [ArithOp; 10] = [
    ArithOp::Add,
    ArithOp::Mul,
    ArithOp::Sub,
    ArithOp::Div,
    ArithOp::Rem,
    ArithOp::Shl,
    ArithOp::Shr,
    ArithOp::BitXor,
    ArithOp::BitOr,
    ArithOp::BitAnd,
];

fn arith_code(op: ArithOp) -> u32 {
    match op {
        ArithOp::Add => 0,
        ArithOp::Mul => 1,
        ArithOp::Sub => 2,
        ArithOp::Div => 3,
        ArithOp::Rem => 4,
        ArithOp::Shl => 5,
        ArithOp::Shr => 6,
        ArithOp::BitXor => 7,
        ArithOp::BitOr => 8,
        ArithOp::BitAnd => 9,
    }
}

/// `&&`, `||`, then the ten arithmetic operators from 2, the comparisons from 12, `=` at 18 and
/// the compound assignments from 19.
fn binary_code(op: BinaryOp) -> u32 {
    match op {
        BinaryOp::LogicOp(LogicOp::And) => 0,
        BinaryOp::LogicOp(LogicOp::Or) => 1,
        BinaryOp::ArithOp(op) => 2 + arith_code(op),
        BinaryOp::CmpOp(CmpOp::Eq { negated }) => 12 + u32::from(negated),
        BinaryOp::CmpOp(CmpOp::Ord { ordering, strict }) => {
            14 + 2 * u32::from(ordering == Ordering::Greater) + u32::from(strict)
        }
        BinaryOp::Assignment { op: None } => 18,
        BinaryOp::Assignment { op: Some(op) } => 19 + arith_code(op),
    }
}

fn binary_of(code: u32) -> Option<BinaryOp> {
    Some(match code {
        0 => BinaryOp::LogicOp(LogicOp::And),
        1 => BinaryOp::LogicOp(LogicOp::Or),
        2..12 => BinaryOp::ArithOp(ARITH_OPS[code as usize - 2]),
        12 | 13 => BinaryOp::CmpOp(CmpOp::Eq { negated: code == 13 }),
        14..18 => BinaryOp::CmpOp(CmpOp::Ord {
            ordering: if code >= 16 { Ordering::Greater } else { Ordering::Less },
            strict: code % 2 == 1,
        }),
        18 => BinaryOp::Assignment { op: None },
        19..29 => BinaryOp::Assignment { op: Some(ARITH_OPS[code as usize - 19]) },
        _ => return None,
    })
}

const CAST_ERRORS: [CastError; 13] = [
    CastError::Unknown,
    CastError::CastToBool,
    CastError::CastToChar,
    CastError::DifferingKinds,
    CastError::SizedUnsizedCast,
    CastError::IllegalCast,
    CastError::IntToWideCast,
    CastError::NeedDeref,
    CastError::NeedViaPtr,
    CastError::NeedViaThinPtr,
    CastError::NeedViaInt,
    CastError::NonScalar,
    CastError::PtrPtrAddingAutoTraits,
];

fn cast_error_code(error: &CastError) -> u32 {
    match error {
        CastError::Unknown => 0,
        CastError::CastToBool => 1,
        CastError::CastToChar => 2,
        CastError::DifferingKinds => 3,
        CastError::SizedUnsizedCast => 4,
        CastError::IllegalCast => 5,
        CastError::IntToWideCast => 6,
        CastError::NeedDeref => 7,
        CastError::NeedViaPtr => 8,
        CastError::NeedViaThinPtr => 9,
        CastError::NeedViaInt => 10,
        CastError::NonScalar => 11,
        CastError::PtrPtrAddingAutoTraits => 12,
    }
}

/// Where a record's types go as it is made: the type table, or in tests a list.
trait TyEncode<'db> {
    fn ty(&mut self, ty: Ty<'db>) -> u32;
    fn args(&mut self, args: GenericArgs<'db>) -> u32;
    /// The trait as two words of its salsa id and its arguments.
    fn trait_ref(&mut self, trait_ref: &StoredTraitRef) -> [u32; 3];
}

/// Where a record's types come back from.
trait TyDecode {
    fn ty(&self, at: u32) -> Result<StoredTy, FormatError>;
    fn args(&self, at: u32) -> Result<StoredGenericArgs, FormatError>;
    fn trait_ref(&self, words: [u32; 3]) -> Result<StoredTraitRef, FormatError>;
}

struct TableEncode<'db> {
    interner: DbInterner<'db>,
    table: TyTableWriter<'db>,
}

impl<'db> TyEncode<'db> for TableEncode<'db> {
    fn ty(&mut self, ty: Ty<'db>) -> u32 {
        self.table.ty(ty)
    }

    fn args(&mut self, args: GenericArgs<'db>) -> u32 {
        self.table.args(args)
    }

    fn trait_ref(&mut self, trait_ref: &StoredTraitRef) -> [u32; 3] {
        let trait_ref = trait_ref.get(self.interner);
        let bits = id_bits(trait_ref.def_id.0);
        let args = trait_ref.args.store();
        [bits as u32, (bits >> 32) as u32, self.table.args(args.as_ref())]
    }
}

struct TableDecode<'a, 'db> {
    interner: DbInterner<'db>,
    table: TyTableReader<'a, 'db>,
    /// Where the record's root is, for errors.
    at: u64,
}

impl TyDecode for TableDecode<'_, '_> {
    fn ty(&self, at: u32) -> Result<StoredTy, FormatError> {
        self.table.ty(at).map(Ty::store)
    }

    fn args(&self, at: u32) -> Result<StoredGenericArgs, FormatError> {
        self.table.args(at).map(GenericArgs::store)
    }

    fn trait_ref(&self, [low, high, args]: [u32; 3]) -> Result<StoredTraitRef, FormatError> {
        let id = salsa_id(u64::from(low) | (u64::from(high) << 32))
            .ok_or(FormatError::Invalid { at: self.at, what: "a trait of id 0" })?;
        let args = self.table.args(args)?;
        Ok(StoredTraitRef::new(TraitRef::new_from_args(
            self.interner,
            TraitId::from_id(id).into(),
            args,
        )))
    }
}

impl<'db> InferenceResult<'db> {
    /// The result in its `'static` form, its types in a type table of its own.
    pub fn to_record(&self, interner: DbInterner<'db>) -> InferenceRecord {
        let mut tys = TableEncode { interner, table: TyTableWriter::new(interner) };
        let mut record = InferenceRecord::of(self, &mut tys);
        let mut out = Writer::new(TY_TABLE_SCHEMA);
        record.table_root = tys.table.write(&mut out);
        let bytes = out.finish(record.table_root);
        record.table = table_words(&bytes);
        record
    }
}

impl InferenceRecord {
    fn empty() -> InferenceRecord {
        InferenceRecord {
            u64s: Vec::new(),
            u32s: Vec::new(),
            bytes: Vec::new(),
            spans64: [[0; 2]; N64],
            spans32: [[0; 2]; N32],
            spans8: [[0; 2]; N8],
            error_ty: 0,
            flags: 0,
            diagnostics: 0,
            table: Vec::new(),
            table_root: 0,
        }
    }

    fn span(start: usize, end: usize) -> [u32; 2] {
        let word = |it: usize| u32::try_from(it).expect("an inference column of 2^32 words");
        [word(start), word(end - start)]
    }

    fn put64(&mut self, column: C64, words: impl IntoIterator<Item = u64>) {
        let start = self.u64s.len();
        self.u64s.extend(words);
        self.spans64[column as usize] = Self::span(start, self.u64s.len());
    }

    fn put32(&mut self, column: C32, words: impl IntoIterator<Item = u32>) {
        let start = self.u32s.len();
        self.u32s.extend(words);
        self.spans32[column as usize] = Self::span(start, self.u32s.len());
    }

    fn put8(&mut self, column: C8, words: impl IntoIterator<Item = u8>) {
        let start = self.bytes.len();
        self.bytes.extend(words);
        self.spans8[column as usize] = Self::span(start, self.bytes.len());
    }

    fn put_variants<K: DenseKey>(
        &mut self,
        column: &IndexColumn<K, VariantId>,
        at: C32,
        kinds: C8,
        ids: C64,
    ) {
        self.put32(at, column.at().iter().copied());
        self.put8(kinds, column.rows().iter().map(|&it| variant_key(it).0));
        self.put64(ids, column.rows().iter().map(|&it| variant_key(it).1));
    }

    fn put_assocs<'db, K: DenseKey>(
        &mut self,
        tys: &mut impl TyEncode<'db>,
        column: &IndexColumn<K, (CandidateId, StoredGenericArgs)>,
        [at, args]: [C32; 2],
        kinds: C8,
        ids: C64,
    ) {
        self.put32(at, column.at().iter().copied());
        self.put8(kinds, column.rows().iter().map(|&(it, _)| candidate_key(it).0));
        self.put64(ids, column.rows().iter().map(|&(it, _)| candidate_key(it).1));
        self.put32(args, column.rows().iter().map(|(_, it)| tys.args(it.as_ref())));
    }

    fn put_tys<'db, K: DenseKey>(
        &mut self,
        tys: &mut impl TyEncode<'db>,
        column: &TyColumn<K>,
        [at, dictionary]: [C32; 2],
    ) {
        self.put32(at, column.at().iter().copied());
        self.put32(dictionary, column.tys().iter().map(|it| tys.ty(it.as_ref())));
    }

    fn put_ranges(&mut self, column: C32, ranges: &[Range]) {
        self.put32(column, ranges.iter().flat_map(|it| [it.start, it.len]));
    }

    fn put_adjustments<'db, K: DenseKey>(
        &mut self,
        tys: &mut impl TyEncode<'db>,
        column: &Csr<K, Adjustment>,
        [ranges, targets]: [C32; 2],
        kinds: C8,
    ) {
        self.put_ranges(ranges, column.ranges());
        self.put8(kinds, column.items().iter().map(|it| adjust_code(&it.kind)));
        self.put32(targets, column.items().iter().map(|it| tys.ty(it.target.as_ref())));
    }

    /// `result` with its types written into `tys`.
    fn of<'db>(result: &InferenceResult<'db>, tys: &mut impl TyEncode<'db>) -> InferenceRecord {
        let InferenceResult {
            method_resolutions,
            field_resolutions,
            variant_resolutions,
            assoc_resolutions,
            tuple_field_access_types,
            type_of_expr,
            type_of_pat,
            type_of_binding,
            type_of_type_placeholder,
            type_of_opaque,
            has_errors,
            diagnostics,
            nodes_with_type_mismatches,
            error_ty,
            expr_adjustments,
            assignment_coercions,
            pat_adjustments,
            binding_modes,
            skipped_ref_pats,
            coercion_casts,
            closures_without_expectation,
            asm_sym_tys,
            closures_data,
            defined_anon_consts,
        } = result;
        let mut record = InferenceRecord::empty();

        record.put32(C32::MethodAt, method_resolutions.at().iter().copied());
        record.put64(C64::MethodFns, method_resolutions.rows().iter().map(|&(it, _)| id_bits(it)));
        record.put32(
            C32::MethodArgs,
            method_resolutions.rows().iter().map(|(_, it)| tys.args(it.as_ref())),
        );

        let fields = field_resolutions.fields();
        record.put8(C8::FieldTags, field_resolutions.tags().iter().copied());
        record.put32(C32::FieldData, field_resolutions.data().iter().copied());
        record.put8(C8::FieldKinds, fields.iter().map(|it| variant_key(it.parent).0));
        record.put64(C64::FieldVariants, fields.iter().map(|it| variant_key(it.parent).1));
        record.put32(C32::FieldLocals, fields.iter().map(|it| local(it.local_id)));
        record.put32(
            C32::TupleFields,
            field_resolutions.tuple_fields().iter().flat_map(|it| [it.tuple.0, it.index]),
        );

        let ExprOrPatColumn { exprs, pats } = variant_resolutions;
        record.put_variants(exprs, C32::VariantExprAt, C8::VariantExprKinds, C64::VariantExprIds);
        record.put_variants(pats, C32::VariantPatAt, C8::VariantPatKinds, C64::VariantPatIds);
        let ExprOrPatColumn { exprs, pats } = assoc_resolutions;
        let expr_columns = [C32::AssocExprAt, C32::AssocExprArgs];
        record.put_assocs(tys, exprs, expr_columns, C8::AssocExprKinds, C64::AssocExprIds);
        let pat_columns = [C32::AssocPatAt, C32::AssocPatArgs];
        record.put_assocs(tys, pats, pat_columns, C8::AssocPatKinds, C64::AssocPatIds);

        let tuple_len = |it: &StoredTys| u32::try_from(it.as_ref().len()).expect("a tuple of 2^32 types");
        record.put32(C32::TupleLens, tuple_field_access_types.iter().map(tuple_len));
        record.put32(
            C32::TupleItems,
            tuple_field_access_types.iter().flat_map(|it| it.as_ref().iter()).map(|it| tys.ty(it)),
        );

        record.put_tys(tys, type_of_expr, [C32::ExprTyAt, C32::ExprTys]);
        record.put_tys(tys, type_of_pat, [C32::PatTyAt, C32::PatTys]);
        record.put_tys(tys, type_of_binding, [C32::BindingTyAt, C32::BindingTys]);
        record.put_tys(tys, type_of_type_placeholder, [C32::PlaceholderTyAt, C32::PlaceholderTys]);
        record.put64(C64::OpaqueIds, type_of_opaque.keys().map(|&it| id_bits(it)));
        record.put32(C32::OpaqueTys, type_of_opaque.values().map(|it| tys.ty(it.as_ref())));

        let ExprOrPatSet { exprs, pats } = nodes_with_type_mismatches;
        record.put64(C64::MismatchedExprs, exprs.words().iter().copied());
        record.put64(C64::MismatchedPats, pats.words().iter().copied());
        record.put64(C64::SkippedRefPats, skipped_ref_pats.words().iter().copied());
        record.put64(C64::CoercionCasts, coercion_casts.words().iter().copied());
        record.put64(
            C64::ClosuresWithoutExpectation,
            closures_without_expectation.words().iter().copied(),
        );

        let expr_columns = [C32::ExprAdjustRanges, C32::ExprAdjustTargets];
        record.put_adjustments(tys, expr_adjustments, expr_columns, C8::ExprAdjustKinds);
        let coercion_columns = [C32::CoercionRanges, C32::CoercionTargets];
        record.put_adjustments(tys, assignment_coercions, coercion_columns, C8::CoercionKinds);
        record.put_ranges(C32::PatAdjustRanges, pat_adjustments.ranges());
        record.put8(
            C8::PatAdjustKinds,
            pat_adjustments.items().iter().map(|it| match it.kind {
                PatAdjust::BuiltinDeref => 0,
                PatAdjust::OverloadedDeref => 1,
            }),
        );
        record.put32(
            C32::PatAdjustSources,
            pat_adjustments.items().iter().map(|it| tys.ty(it.source.as_ref())),
        );
        record.put8(C8::BindingModes, binding_modes.codes().iter().copied());

        record.put32(
            C32::AsmSyms,
            asm_sym_tys
                .iter()
                .flat_map(|(&(asm, operand), ty)| [local(asm), operand, tys.ty(ty.as_ref())]),
        );

        let mut words = Sink::default();
        diagnostics.iter().for_each(|it| words.diagnostic(tys, it));
        record.diagnostics = u32::try_from(diagnostics.len()).expect("2^32 diagnostics");
        record.put32(C32::ClosureExprs, closures_data.keys().map(|&it| local(it)));
        closures_data.values().for_each(|it| words.closure(tys, it));
        record.put32(C32::Words, words.words);
        record.put8(C8::Text, words.text);

        record.put64(C64::AnonConsts, defined_anon_consts.iter().map(|&it| id_bits(it)));
        record.error_ty = tys.ty(error_ty.as_ref());
        record.flags = u32::from(*has_errors);
        record
    }

    pub fn as_ref(&self) -> InferenceRecordRef<'_> {
        InferenceRecordRef {
            at: 0,
            u64s: &self.u64s,
            u32s: &self.u32s,
            bytes: &self.bytes,
            spans64: &self.spans64,
            spans32: &self.spans32,
            spans8: &self.spans8,
            error_ty: self.error_ty,
            flags: self.flags,
            diagnostics: self.diagnostics,
            table: &self.table,
            table_root: self.table_root,
        }
    }
}

/// Where a column was written: nowhere when it is empty.
#[derive(Clone, Copy)]
struct Placed {
    at: u64,
    len: usize,
}

impl Placed {
    fn push<T: Record>(out: &mut Writer, values: &[T]) -> Placed {
        match values.is_empty() {
            true => Placed { at: 0, len: 0 },
            false => Placed { at: out.push_slice(values), len: values.len() },
        }
    }

    fn slice<T>(self, field_at: u64) -> Slice<T> {
        match self.len {
            0 => Slice::EMPTY,
            len => Slice::between(field_at, self.at, len),
        }
    }
}

impl Persist for InferenceRecord {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("InferenceRecord"),
        Part::U64(1),
        Part::Schema(<InferRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = InferenceRecordRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let u64s = Placed::push(out, &self.u64s);
        let u32s = Placed::push(out, &self.u32s);
        let bytes = Placed::push(out, &self.bytes);
        let table = Placed::push(out, &self.table);

        let at = out.next::<InferRoot>();
        let field = |offset: usize| at + offset as u64;
        let root = InferRoot {
            table_root: self.table_root,
            u64s: u64s.slice(field(offset_of!(InferRoot, u64s))),
            u32s: u32s.slice(field(offset_of!(InferRoot, u32s))),
            bytes: bytes.slice(field(offset_of!(InferRoot, bytes))),
            table: table.slice(field(offset_of!(InferRoot, table))),
            spans64: self.spans64,
            spans32: self.spans32,
            spans8: self.spans8,
            error_ty: self.error_ty,
            flags: self.flags,
            diagnostics: self.diagnostics,
            reserved: 0,
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<InferenceRecordRef<'_>, FormatError> {
        let root = open_as::<InferRoot>(buffer, Self::SCHEMA)?;
        let at = (root as *const InferRoot as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
        if root.reserved != 0 {
            return Err(FormatError::Invalid { at, what: "an inference record with a reserved word set" });
        }
        let record = InferenceRecordRef {
            at,
            u64s: root.u64s.follow(buffer)?,
            u32s: root.u32s.follow(buffer)?,
            bytes: root.bytes.follow(buffer)?,
            spans64: &root.spans64,
            spans32: &root.spans32,
            spans8: &root.spans8,
            error_ty: root.error_ty,
            flags: root.flags,
            diagnostics: root.diagnostics,
            table: root.table.follow(buffer)?,
            table_root: root.table_root,
        };
        record.check()?;
        Ok(record)
    }
}

impl<'a> InferenceRecordRef<'a> {
    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }

    fn ensure(&self, holds: bool, what: &'static str) -> Result<(), FormatError> {
        if holds { Ok(()) } else { Err(self.invalid(what)) }
    }

    /// That every column is inside its array: what the column accessors index without checking.
    fn check(&self) -> Result<(), FormatError> {
        let inside = |spans: &[[u32; 2]], len: usize| {
            spans.iter().all(|&[start, count]| start as usize + count as usize <= len)
        };
        self.ensure(
            inside(self.spans64, self.u64s.len())
                && inside(self.spans32, self.u32s.len())
                && inside(self.spans8, self.bytes.len()),
            "an inference column past the end of its array",
        )
    }

    fn u64s(&self, column: C64) -> &'a [u64] {
        let [start, len] = self.spans64[column as usize];
        &self.u64s[start as usize..][..len as usize]
    }

    fn u32s(&self, column: C32) -> &'a [u32] {
        let [start, len] = self.spans32[column as usize];
        &self.u32s[start as usize..][..len as usize]
    }

    fn u8s(&self, column: C8) -> &'a [u8] {
        let [start, len] = self.spans8[column as usize];
        &self.bytes[start as usize..][..len as usize]
    }

    fn id<T: FromId>(&self, bits: u64) -> Result<T, FormatError> {
        salsa_id(bits).map(T::from_id).ok_or(self.invalid("a salsa id of index 0"))
    }

    fn same_len(&self, lens: &[usize], what: &'static str) -> Result<(), FormatError> {
        self.ensure(lens.windows(2).all(|it| it[0] == it[1]), what)
    }

    fn index_column<K: DenseKey, V>(&self, at: C32, rows: Box<[V]>) -> Result<IndexColumn<K, V>, FormatError> {
        IndexColumn::from_parts(self.u32s(at).into(), rows).map_err(|what| self.invalid(what))
    }

    fn variants<K: DenseKey>(&self, at: C32, kinds: C8, ids: C64) -> Result<IndexColumn<K, VariantId>, FormatError> {
        let (kinds, ids) = (self.u8s(kinds), self.u64s(ids));
        self.same_len(&[kinds.len(), ids.len()], "variant columns of different lengths")?;
        let rows = kinds
            .iter()
            .zip(ids)
            .map(|(&kind, &bits)| variant_of(kind, bits).ok_or(self.invalid("a variant of no kind or id 0")))
            .collect::<Result<_, _>>()?;
        self.index_column(at, rows)
    }

    fn assocs<K: DenseKey>(
        &self,
        tys: &impl TyDecode,
        [at, args]: [C32; 2],
        kinds: C8,
        ids: C64,
    ) -> Result<IndexColumn<K, (CandidateId, StoredGenericArgs)>, FormatError> {
        let (kinds, ids, args) = (self.u8s(kinds), self.u64s(ids), self.u32s(args));
        self.same_len(&[kinds.len(), ids.len(), args.len()], "item columns of different lengths")?;
        let rows = kinds
            .iter()
            .zip(ids)
            .zip(args)
            .map(|((&kind, &bits), &args)| {
                let candidate = candidate_of(kind, bits).ok_or(self.invalid("an item of no kind or id 0"))?;
                Ok((candidate, tys.args(args)?))
            })
            .collect::<Result<_, FormatError>>()?;
        self.index_column(at, rows)
    }

    fn tys(&self, tys: &impl TyDecode, column: C32) -> Result<Box<[StoredTy]>, FormatError> {
        self.u32s(column).iter().map(|&it| tys.ty(it)).collect()
    }

    fn ty_column<K: DenseKey>(&self, tys: &impl TyDecode, [at, dictionary]: [C32; 2]) -> Result<TyColumn<K>, FormatError> {
        TyColumn::from_parts(self.u32s(at).into(), self.tys(tys, dictionary)?).map_err(|what| self.invalid(what))
    }

    fn idx_set<K: DenseKey>(&self, column: C64) -> Result<IdxSet<K>, FormatError> {
        IdxSet::from_words(self.u64s(column).to_vec()).map_err(|what| self.invalid(what))
    }

    fn ranges(&self, column: C32) -> Result<Box<[Range]>, FormatError> {
        let words = self.u32s(column);
        self.ensure(words.len() % 2 == 0, "a range column of an odd number of words")?;
        Ok(words.chunks_exact(2).map(|it| Range { start: it[0], len: it[1] }).collect())
    }

    fn adjustments<K: DenseKey>(
        &self,
        tys: &impl TyDecode,
        [ranges, targets]: [C32; 2],
        kinds: C8,
    ) -> Result<Csr<K, Adjustment>, FormatError> {
        let (kinds, targets) = (self.u8s(kinds), self.u32s(targets));
        self.same_len(&[kinds.len(), targets.len()], "adjustment columns of different lengths")?;
        let items = kinds
            .iter()
            .zip(targets)
            .map(|(&kind, &target)| {
                let kind = ADJUSTS.get(kind as usize).cloned().ok_or(self.invalid("an adjustment of no kind"))?;
                Ok(Adjustment { kind, target: tys.ty(target)? })
            })
            .collect::<Result<_, FormatError>>()?;
        Csr::from_parts(self.ranges(ranges)?, items).map_err(|what| self.invalid(what))
    }

    /// The result again, its types read from the record's type table.
    pub fn to_result<'db>(&self, interner: DbInterner<'db>) -> Result<InferenceResult<'db>, FormatError> {
        let bytes = table_bytes(self.table);
        let table = TyTableReader::open(interner, &bytes, self.table_root)?;
        self.decode(&TableDecode { interner, table, at: self.at })
    }

    fn decode<'db>(&self, tys: &impl TyDecode) -> Result<InferenceResult<'db>, FormatError> {
        let (fns, args) = (self.u64s(C64::MethodFns), self.u32s(C32::MethodArgs));
        self.same_len(&[fns.len(), args.len()], "method columns of different lengths")?;
        let rows = fns
            .iter()
            .zip(args)
            .map(|(&bits, &args)| Ok((self.id::<FunctionId>(bits)?, tys.args(args)?)))
            .collect::<Result<_, FormatError>>()?;
        let method_resolutions = self.index_column(C32::MethodAt, rows)?;

        let tuple_field_access_types = {
            let (lens, items) = (self.u32s(C32::TupleLens), self.tys(tys, C32::TupleItems)?);
            let total = lens.iter().try_fold(0usize, |sum, &len| sum.checked_add(len as usize));
            self.ensure(total == Some(items.len()), "tuple lengths that do not add up to their types")?;
            let mut start = 0;
            let mut buffer = Vec::new();
            lens.iter()
                .map(|&len| {
                    buffer.clear();
                    buffer.extend(items[start..][..len as usize].iter().map(StoredTy::as_ref));
                    start += len as usize;
                    Tys::new_from_slice(&buffer).store()
                })
                .collect::<Box<[StoredTys]>>()
        };

        let field_resolutions = {
            let (kinds, variants, locals) =
                (self.u8s(C8::FieldKinds), self.u64s(C64::FieldVariants), self.u32s(C32::FieldLocals));
            self.same_len(&[kinds.len(), variants.len(), locals.len()], "field columns of different lengths")?;
            let fields = kinds
                .iter()
                .zip(variants)
                .zip(locals)
                .map(|((&kind, &bits), &local)| {
                    let parent = variant_of(kind, bits).ok_or(self.invalid("a field of a variant of no kind"))?;
                    Ok(FieldId { parent, local_id: local_of(local) })
                })
                .collect::<Result<_, FormatError>>()?;
            let words = self.u32s(C32::TupleFields);
            self.ensure(words.len() % 2 == 0, "a tuple field column of an odd number of words")?;
            let tuple_fields = words
                .chunks_exact(2)
                .map(|it| {
                    let tuple = tuple_field_access_types.get(it[0] as usize);
                    self.ensure(
                        tuple.is_some_and(|tuple| (it[1] as usize) < tuple.as_ref().len()),
                        "a tuple field past its tuple",
                    )?;
                    Ok(TupleFieldId { tuple: TupleId(it[0]), index: it[1] })
                })
                .collect::<Result<_, FormatError>>()?;
            let (tags, data) = (self.u8s(C8::FieldTags), self.u32s(C32::FieldData));
            FieldColumn::from_parts(tags.into(), data.into(), fields, tuple_fields)
                .map_err(|what| self.invalid(what))?
        };

        let variant_resolutions = ExprOrPatColumn {
            exprs: self.variants(C32::VariantExprAt, C8::VariantExprKinds, C64::VariantExprIds)?,
            pats: self.variants(C32::VariantPatAt, C8::VariantPatKinds, C64::VariantPatIds)?,
        };
        let assoc_resolutions = ExprOrPatColumn {
            exprs: self.assocs(tys, [C32::AssocExprAt, C32::AssocExprArgs], C8::AssocExprKinds, C64::AssocExprIds)?,
            pats: self.assocs(tys, [C32::AssocPatAt, C32::AssocPatArgs], C8::AssocPatKinds, C64::AssocPatIds)?,
        };

        let type_of_opaque = {
            let (ids, opaque_tys) = (self.u64s(C64::OpaqueIds), self.u32s(C32::OpaqueTys));
            self.same_len(&[ids.len(), opaque_tys.len()], "opaque columns of different lengths")?;
            let entries = ids
                .iter()
                .zip(opaque_tys)
                .map(|(&bits, &ty)| Ok((self.id::<InternedOpaqueTyId<'db>>(bits)?, tys.ty(ty)?)))
                .collect::<Result<_, FormatError>>()?;
            SortedMap::from_sorted(entries).map_err(|what| self.invalid(what))?
        };

        let pat_adjustments = {
            let (kinds, sources) = (self.u8s(C8::PatAdjustKinds), self.u32s(C32::PatAdjustSources));
            self.same_len(&[kinds.len(), sources.len()], "pattern adjustment columns of different lengths")?;
            let items = kinds
                .iter()
                .zip(sources)
                .map(|(&kind, &source)| {
                    let kind = match kind {
                        0 => PatAdjust::BuiltinDeref,
                        1 => PatAdjust::OverloadedDeref,
                        _ => return Err(self.invalid("a pattern adjustment of no kind")),
                    };
                    Ok(PatAdjustment { kind, source: tys.ty(source)? })
                })
                .collect::<Result<_, FormatError>>()?;
            Csr::from_parts(self.ranges(C32::PatAdjustRanges)?, items).map_err(|what| self.invalid(what))?
        };

        let asm_sym_tys = {
            let words = self.u32s(C32::AsmSyms);
            self.ensure(words.len() % 3 == 0, "a sym operand column of a partial operand")?;
            let entries = words
                .chunks_exact(3)
                .map(|it| Ok(((local_of(it[0]), it[1]), tys.ty(it[2])?)))
                .collect::<Result<_, FormatError>>()?;
            SortedMap::from_sorted(entries).map_err(|what| self.invalid(what))?
        };

        let mut words = Source { words: self.u32s(C32::Words), next: 0, text: self.u8s(C8::Text), at: self.at };
        let diagnostics = (0..self.diagnostics)
            .map(|_| words.diagnostic(tys))
            .collect::<Result<Box<[_]>, _>>()?;
        let closures_data = {
            let entries = self
                .u32s(C32::ClosureExprs)
                .iter()
                .map(|&expr| Ok((local_of(expr), words.closure(tys)?)))
                .collect::<Result<_, FormatError>>()?;
            SortedMap::from_sorted(entries).map_err(|what| self.invalid(what))?
        };
        self.ensure(words.is_done(), "words after the last closure")?;

        let defined_anon_consts = self
            .u64s(C64::AnonConsts)
            .iter()
            .map(|&bits| self.id::<AnonConstId<'db>>(bits))
            .collect::<Result<_, _>>()?;
        self.ensure(self.flags <= 1, "an inference record with unknown flags")?;

        Ok(InferenceResult {
            method_resolutions,
            field_resolutions,
            variant_resolutions,
            assoc_resolutions,
            tuple_field_access_types,
            type_of_expr: self.ty_column(tys, [C32::ExprTyAt, C32::ExprTys])?,
            type_of_pat: self.ty_column(tys, [C32::PatTyAt, C32::PatTys])?,
            type_of_binding: self.ty_column(tys, [C32::BindingTyAt, C32::BindingTys])?,
            type_of_type_placeholder: self.ty_column(tys, [C32::PlaceholderTyAt, C32::PlaceholderTys])?,
            type_of_opaque,
            has_errors: self.flags & 1 != 0,
            diagnostics,
            nodes_with_type_mismatches: ExprOrPatSet {
                exprs: self.idx_set(C64::MismatchedExprs)?,
                pats: self.idx_set(C64::MismatchedPats)?,
            },
            error_ty: tys.ty(self.error_ty)?,
            expr_adjustments: self.adjustments(
                tys,
                [C32::ExprAdjustRanges, C32::ExprAdjustTargets],
                C8::ExprAdjustKinds,
            )?,
            assignment_coercions: self.adjustments(
                tys,
                [C32::CoercionRanges, C32::CoercionTargets],
                C8::CoercionKinds,
            )?,
            pat_adjustments,
            binding_modes: ModeColumn::from_codes(self.u8s(C8::BindingModes).into())
                .map_err(|what| self.invalid(what))?,
            skipped_ref_pats: self.idx_set(C64::SkippedRefPats)?,
            coercion_casts: self.idx_set(C64::CoercionCasts)?,
            closures_without_expectation: self.idx_set(C64::ClosuresWithoutExpectation)?,
            asm_sym_tys,
            closures_data,
            defined_anon_consts,
        })
    }
}

/// The side tables' words being written, and the text of the names in them.
#[derive(Default)]
struct Sink {
    words: Vec<u32>,
    text: Vec<u8>,
}

impl Sink {
    fn word(&mut self, word: u32) {
        self.words.push(word);
    }

    fn u64(&mut self, word: u64) {
        self.words.extend([word as u32, (word >> 32) as u32]);
    }

    fn usize(&mut self, word: usize) {
        self.u64(word as u64);
    }

    fn bool(&mut self, value: bool) {
        self.word(u32::from(value));
    }

    fn len(&mut self, len: usize) {
        self.word(u32::try_from(len).expect("a side table list of 2^32 entries"));
    }

    fn local<T>(&mut self, id: Idx<T>) {
        self.word(local(id));
    }

    fn id(&mut self, id: impl AsId) {
        self.u64(id_bits(id));
    }

    fn ty<'db>(&mut self, tys: &mut impl TyEncode<'db>, ty: &StoredTy) {
        let at = tys.ty(ty.as_ref());
        self.word(at);
    }

    fn opt_ty<'db>(&mut self, tys: &mut impl TyEncode<'db>, ty: Option<&StoredTy>) {
        match ty {
            None => self.word(0),
            Some(ty) => {
                self.word(1);
                self.ty(tys, ty);
            }
        }
    }

    fn name(&mut self, name: &Name) {
        let text = name.as_str().as_bytes();
        self.len(self.text.len());
        self.len(text.len());
        self.text.extend_from_slice(text);
    }

    fn node(&mut self, node: ExprOrPatIdPacked) {
        self.expr_or_pat(node.unpack());
    }

    fn expr_or_pat(&mut self, node: ExprOrPatId) {
        match node {
            ExprOrPatId::ExprId(it) => self.words.extend([0, local(it)]),
            ExprOrPatId::PatId(it) => self.words.extend([1, local(it)]),
        }
    }

    fn opt_expr(&mut self, expr: Option<ExprId>) {
        match expr {
            None => self.word(0),
            Some(it) => self.words.extend([1, local(it)]),
        }
    }

    fn variant(&mut self, variant: VariantId) {
        let (kind, bits) = variant_key(variant);
        self.word(u32::from(kind));
        self.u64(bits);
    }

    fn generic_def(&mut self, def: GenericDefId) {
        let (tag, bits) = match def {
            GenericDefId::AdtId(AdtId::StructId(it)) => (0, id_bits(it)),
            GenericDefId::AdtId(AdtId::EnumId(it)) => (1, id_bits(it)),
            GenericDefId::AdtId(AdtId::UnionId(it)) => (2, id_bits(it)),
            GenericDefId::ConstId(it) => (3, id_bits(it)),
            GenericDefId::FunctionId(it) => (4, id_bits(it)),
            GenericDefId::ImplId(it) => (5, id_bits(it)),
            GenericDefId::StaticId(it) => (6, id_bits(it)),
            GenericDefId::TraitId(it) => (7, id_bits(it)),
            GenericDefId::TypeAliasId(it) => (8, id_bits(it)),
        };
        self.word(tag);
        self.u64(bits);
    }

    fn generic_param(&mut self, param: GenericParamId) {
        let (tag, parent, local) = match param {
            GenericParamId::TypeParamId(it) => (0, it.parent(), local(it.local_id())),
            GenericParamId::ConstParamId(it) => (1, it.parent(), local(it.local_id())),
            GenericParamId::LifetimeParamId(it) => (2, it.parent, local(it.local_id)),
        };
        self.word(tag);
        self.generic_def(parent);
        self.word(local);
    }

    fn generics_len_kind(&mut self, kind: IncorrectGenericsLenKind) {
        self.word(match kind {
            IncorrectGenericsLenKind::Lifetimes => 0,
            IncorrectGenericsLenKind::TypesAndConsts => 1,
        });
    }

    fn span(&mut self, span: Span) {
        match span {
            Span::ExprId(it) => self.words.extend([0, local(it)]),
            Span::PatId(it) => self.words.extend([1, local(it)]),
            Span::BindingId(it) => self.words.extend([2, local(it)]),
            Span::TypeRefId(it) => self.words.extend([3, local(it)]),
            Span::Dummy => self.word(4),
        }
    }

    fn closure_kind(&mut self, kind: ClosureKind) {
        self.word(match kind {
            ClosureKind::Fn => 0,
            ClosureKind::FnMut => 1,
            ClosureKind::FnOnce => 2,
        });
    }

    fn trait_predicate<'db>(&mut self, tys: &mut impl TyEncode<'db>, predicate: &StoredTraitPredicate) {
        let StoredTraitPredicate { trait_ref, polarity } = predicate;
        let words = tys.trait_ref(trait_ref);
        self.words.extend(words);
        self.word(match polarity {
            PredicatePolarity::Positive => 0,
            PredicatePolarity::Negative => 1,
        });
    }

    /// A lowering diagnostic in `lower::persist`'s words, after their count, so that
    /// [`Source::lowering_words`] reads exactly them.
    fn lowering_words(&mut self, put: impl FnOnce(&mut Vec<u32>)) {
        let at = self.words.len();
        self.word(0);
        put(&mut self.words);
        self.words[at] = u32::try_from(self.words.len() - at - 1).expect("a diagnostic of 2^32 words");
    }

    /// A diagnostic: its kind's place in [`InferenceDiagnostic`]'s declaration, then its fields
    /// in theirs.
    fn diagnostic<'db>(&mut self, tys: &mut impl TyEncode<'db>, diagnostic: &InferenceDiagnostic) {
        use InferenceDiagnostic as D;
        match diagnostic {
            D::NoSuchField { field, private, variant } => {
                self.word(0);
                self.node(*field);
                match private {
                    None => self.word(0),
                    Some(it) => self.words.extend([1, local(*it)]),
                }
                self.variant(*variant);
            }
            D::MismatchedArrayPatLen { pat, expected, found, has_rest } => {
                self.word(1);
                self.local(*pat);
                self.u64(*expected);
                self.u64(*found);
                self.bool(*has_rest);
            }
            D::ArrayPatternWithoutFixedLength { pat } => self.words.extend([2, local(*pat)]),
            D::ExpectedArrayOrSlicePat { pat, found } => {
                self.words.extend([3, local(*pat)]);
                self.ty(tys, found);
            }
            D::InvalidRangePatType { pat } => self.words.extend([4, local(*pat)]),
            D::DuplicateField { field, variant } => {
                self.word(5);
                self.node(*field);
                self.variant(*variant);
            }
            D::PrivateField { expr, field } => {
                self.words.extend([6, local(*expr)]);
                self.variant(field.parent);
                self.local(field.local_id);
            }
            D::PrivateAssocItem { id, item } => {
                self.word(7);
                self.node(*id);
                let (kind, bits) = match *item {
                    AssocItemId::FunctionId(it) => (0, id_bits(it)),
                    AssocItemId::ConstId(it) => (1, id_bits(it)),
                    AssocItemId::TypeAliasId(it) => (2, id_bits(it)),
                };
                self.word(kind);
                self.u64(bits);
            }
            D::UnresolvedField { expr, receiver, name, method_with_same_name_exists } => {
                self.words.extend([8, local(*expr)]);
                self.ty(tys, receiver);
                self.name(name);
                self.bool(*method_with_same_name_exists);
            }
            D::UnresolvedMethodCall {
                expr,
                receiver,
                name,
                field_with_same_name,
                assoc_func_with_same_name,
            } => {
                self.words.extend([9, local(*expr)]);
                self.ty(tys, receiver);
                self.name(name);
                self.opt_ty(tys, field_with_same_name.as_ref());
                match assoc_func_with_same_name {
                    None => self.word(0),
                    Some(it) => {
                        self.word(1);
                        self.id(*it);
                    }
                }
            }
            D::UnresolvedAssocItem { id } => {
                self.word(10);
                self.node(*id);
            }
            D::UnresolvedIdent { id } => {
                self.word(11);
                self.node(*id);
            }
            D::BreakOutsideOfLoop { expr, is_break, bad_value_break } => {
                self.words.extend([12, local(*expr)]);
                self.bool(*is_break);
                self.bool(*bad_value_break);
            }
            D::NonExhaustiveRecordExpr { expr } => self.words.extend([13, local(*expr)]),
            D::NonExhaustiveRecordPat { pat, variant } => {
                self.words.extend([14, local(*pat)]);
                self.variant(*variant);
            }
            D::UnionPatMustHaveExactlyOneField { pat } => self.words.extend([15, local(*pat)]),
            D::UnionPatHasRest { pat } => self.words.extend([16, local(*pat)]),
            D::FunctionalRecordUpdateOnNonStruct { base_expr } => {
                self.words.extend([17, local(*base_expr)])
            }
            D::MismatchedArgCount { call_expr, expected, found, is_fn_trait_call } => {
                self.words.extend([18, local(*call_expr)]);
                self.usize(*expected);
                self.usize(*found);
                self.bool(*is_fn_trait_call);
            }
            D::MismatchedTupleStructPatArgCount { pat, expected, found } => {
                self.words.extend([19, local(*pat)]);
                self.usize(*expected);
                self.usize(*found);
            }
            D::ExpectedFunction { call_expr, found } => {
                self.words.extend([20, local(*call_expr)]);
                self.ty(tys, found);
            }
            D::CannotBeDereferenced { expr, found } => {
                self.words.extend([21, local(*expr)]);
                self.ty(tys, found);
            }
            D::UnaryOperatorCannotBeApplied { expr, op, found } => {
                let op = match op {
                    UnaryOp::Deref => 0,
                    UnaryOp::Not => 1,
                    UnaryOp::Neg => 2,
                };
                self.words.extend([22, local(*expr), op]);
                self.ty(tys, found);
            }
            D::BinaryOperatorCannotBeApplied { expr, op, lhs, rhs } => {
                self.words.extend([23, local(*expr), binary_code(*op)]);
                self.ty(tys, lhs);
                self.ty(tys, rhs);
            }
            D::MutRefInImmRefPat { pat } => self.words.extend([24, local(*pat)]),
            D::CannotImplicitlyDerefTraitObject { pat, found } => {
                self.words.extend([25, local(*pat)]);
                self.ty(tys, found);
            }
            D::CannotIndexInto { expr, found } => {
                self.words.extend([26, local(*expr)]);
                self.ty(tys, found);
            }
            D::TypedHole { expr, expected } => {
                self.words.extend([27, local(*expr)]);
                self.ty(tys, expected);
            }
            D::CastToUnsized { expr, cast_ty } => {
                self.words.extend([28, local(*expr)]);
                self.ty(tys, cast_ty);
            }
            D::InvalidCast { expr, error, expr_ty, cast_ty } => {
                self.words.extend([29, local(*expr), cast_error_code(error)]);
                self.ty(tys, expr_ty);
                self.ty(tys, cast_ty);
            }
            D::TyDiagnostic { diag } => {
                self.word(30);
                self.lowering_words(|words| put_diagnostic(words, diag));
            }
            D::PathDiagnostic { node, diag } => {
                self.word(31);
                self.node(*node);
                self.lowering_words(|words| put_path_diagnostic(words, diag));
            }
            D::MethodCallIncorrectGenericsLen { expr, provided_count, expected_count, kind, def } => {
                self.words.extend([32, local(*expr), *provided_count, *expected_count]);
                self.generics_len_kind(*kind);
                self.generic_def(*def);
            }
            D::MethodCallIllegalSizedBound { call_expr } => self.words.extend([33, local(*call_expr)]),
            D::MethodCallIncorrectGenericsOrder { expr, param_id, arg_idx, has_self_arg } => {
                self.words.extend([34, local(*expr)]);
                self.generic_param(*param_id);
                self.word(*arg_idx);
                self.bool(*has_self_arg);
            }
            D::InvalidLhsOfAssignment { lhs } => self.words.extend([35, local(*lhs)]),
            D::TypeMustBeKnown { at_point, top_term } => {
                self.word(36);
                self.span(*at_point);
                match top_term {
                    None => self.word(0),
                    Some(term) => {
                        let at = tys.args(GenericArgs::new_from_slice(&[term.as_ref()]));
                        self.words.extend([1, at]);
                    }
                }
            }
            D::UnionExprMustHaveExactlyOneField { expr } => self.words.extend([37, local(*expr)]),
            D::TypeMismatch { node, expected, found } => {
                self.word(38);
                self.node(*node);
                self.ty(tys, expected);
                self.ty(tys, found);
            }
            D::SolverDiagnostic(SolverDiagnostic { span, kind }) => {
                self.word(39);
                self.span(*span);
                match kind {
                    SolverDiagnosticKind::TraitUnimplemented { trait_predicate, parent_trait_predicates } => {
                        self.word(0);
                        self.trait_predicate(tys, trait_predicate);
                        self.len(parent_trait_predicates.len());
                        parent_trait_predicates.iter().for_each(|it| self.trait_predicate(tys, it));
                    }
                    SolverDiagnosticKind::ClosureKindMismatch { expected, found, is_async } => {
                        self.word(1);
                        self.closure_kind(*expected);
                        self.closure_kind(*found);
                        self.bool(*is_async);
                    }
                }
            }
            D::ExplicitDropMethodUse { kind } => {
                self.word(40);
                match kind {
                    ExplicitDropMethodUseKind::MethodCall(expr) => self.words.extend([0, local(*expr)]),
                    ExplicitDropMethodUseKind::Path(node) => {
                        self.word(1);
                        self.node(*node);
                    }
                }
            }
            D::MutableRefBinding { pat } => self.words.extend([41, local(*pat)]),
            D::YieldOutsideCoroutine { expr } => self.words.extend([42, local(*expr)]),
            D::ReturnOutsideFunction { expr, kind } => {
                let kind = match kind {
                    ReturnKind::ReturnExpr => 0,
                    ReturnKind::BecomeExpr => 1,
                };
                self.words.extend([43, local(*expr), kind]);
            }
            D::RecordMissingFields { record, variant, missed_fields } => {
                self.word(44);
                self.expr_or_pat(*record);
                self.variant(*variant);
                self.len(missed_fields.len());
                missed_fields.iter().for_each(|&it| self.local(it));
            }
        }
    }

    fn mutability(&mut self, mutability: Mutability) {
        self.word(match mutability {
            Mutability::Not => 0,
            Mutability::Mut => 1,
        });
    }

    fn place<'db>(&mut self, tys: &mut impl TyEncode<'db>, place: &Place) {
        let Place { base_ty, base, projections } = place;
        self.ty(tys, base_ty);
        match *base {
            PlaceBase::Rvalue => self.word(0),
            PlaceBase::StaticItem => self.word(1),
            PlaceBase::Local(binding) => self.words.extend([2, local(binding)]),
            PlaceBase::Upvar { closure, var_id } => {
                self.words.extend([3, local(closure), local(var_id)])
            }
        }
        self.len(projections.len());
        for Projection { ty, kind } in projections {
            self.ty(tys, ty);
            match *kind {
                ProjectionKind::Deref => self.word(0),
                ProjectionKind::Field { field_idx, variant_idx } => {
                    self.words.extend([1, field_idx, variant_idx])
                }
                ProjectionKind::Index => self.word(2),
                ProjectionKind::Subslice => self.word(3),
                ProjectionKind::UnwrapUnsafeBinder => self.word(4),
            }
        }
    }

    fn sources(&mut self, sources: &[CaptureSourceStack]) {
        self.len(sources.len());
        for stack in sources {
            self.len(stack.len());
            stack.iter().for_each(|&it| self.node(it));
        }
    }

    /// A closure's data: its captures by root variable in their order, its fake reads, and its
    /// liberated signature as its types and `FnSigKind`'s flags.
    fn closure<'db>(&mut self, tys: &mut impl TyEncode<'db>, closure: &ClosureData) {
        let ClosureData { min_captures, fake_reads, liberated_sig } = closure;
        self.len(min_captures.len());
        for (&binding, captures) in min_captures {
            self.local(binding);
            self.len(captures.len());
            for CapturedPlace { place, info, mutability } in captures {
                self.place(tys, place);
                let CaptureInfo { sources, capture_kind } = info;
                self.sources(sources);
                self.word(match capture_kind {
                    UpvarCapture::ByRef(BorrowKind::Immutable) => 0,
                    UpvarCapture::ByRef(BorrowKind::UniqueImmutable) => 1,
                    UpvarCapture::ByRef(BorrowKind::Mutable) => 2,
                    UpvarCapture::ByUse => 3,
                    UpvarCapture::ByValue => 4,
                });
                self.mutability(*mutability);
            }
        }
        self.len(fake_reads.len());
        for (place, cause, sources) in fake_reads {
            self.place(tys, place);
            match *cause {
                FakeReadCause::ForMatchGuard => self.word(0),
                FakeReadCause::ForMatchedPlace(expr) => {
                    self.word(1);
                    self.opt_expr(expr);
                }
                FakeReadCause::ForGuardBinding => self.word(2),
                FakeReadCause::ForLet(expr) => {
                    self.word(3);
                    self.opt_expr(expr);
                }
                FakeReadCause::ForIndex => self.word(4),
            }
            self.sources(sources);
        }
        let sig = liberated_sig.get();
        self.len(sig.inputs_and_output.len());
        for ty in sig.inputs_and_output.iter() {
            // `get` ties the types to the borrow of `closure`, shorter than the table's `'db`.
            let ty = ty.store();
            self.ty(tys, &ty);
        }
        let kind = sig.fn_sig_kind;
        self.word(
            u32::from(kind.abi().as_packed())
                | u32::from(kind.is_safe()) << 6
                | u32::from(kind.c_variadic()) << 7,
        );
    }
}

/// The side tables' words being read.
struct Source<'a> {
    words: &'a [u32],
    next: usize,
    text: &'a [u8],
    /// Where the record's root is, for errors.
    at: u64,
}

impl Source<'_> {
    fn is_done(&self) -> bool {
        self.next == self.words.len()
    }

    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }

    fn word(&mut self) -> Result<u32, FormatError> {
        let word = *self.words.get(self.next).ok_or(self.invalid("a side table past its words"))?;
        self.next += 1;
        Ok(word)
    }

    fn u64(&mut self) -> Result<u64, FormatError> {
        let low = self.word()?;
        Ok(u64::from(low) | (u64::from(self.word()?) << 32))
    }

    fn usize(&mut self) -> Result<usize, FormatError> {
        let word = self.u64()?;
        usize::try_from(word).map_err(|_| self.invalid("a count past the address space"))
    }

    fn bool(&mut self) -> Result<bool, FormatError> {
        match self.word()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.invalid("a bool that is neither 0 nor 1")),
        }
    }

    fn local<T>(&mut self) -> Result<Idx<T>, FormatError> {
        Ok(local_of(self.word()?))
    }

    fn salsa(&mut self) -> Result<salsa::Id, FormatError> {
        let bits = self.u64()?;
        salsa_id(bits).ok_or(self.invalid("a salsa id of index 0"))
    }

    fn id<T: FromId>(&mut self) -> Result<T, FormatError> {
        self.salsa().map(T::from_id)
    }

    /// A list's length, which is at most the words left: each entry is at least a word.
    fn len(&mut self) -> Result<usize, FormatError> {
        let len = self.word()? as usize;
        match len <= self.words.len() - self.next {
            true => Ok(len),
            false => Err(self.invalid("a list longer than the words left")),
        }
    }

    fn ty(&mut self, tys: &impl TyDecode) -> Result<StoredTy, FormatError> {
        tys.ty(self.word()?)
    }

    fn tys(&mut self, tys: &impl TyDecode) -> Result<StoredTys, FormatError> {
        let len = self.len()?;
        let list = (0..len).map(|_| self.ty(tys)).collect::<Result<Vec<_>, _>>()?;
        let list: Vec<Ty<'_>> = list.iter().map(StoredTy::as_ref).collect();
        Ok(Tys::new_from_slice(&list).store())
    }

    fn opt_ty(&mut self, tys: &impl TyDecode) -> Result<Option<StoredTy>, FormatError> {
        match self.word()? {
            0 => Ok(None),
            1 => self.ty(tys).map(Some),
            _ => Err(self.invalid("an optional type of no kind")),
        }
    }

    fn name(&mut self) -> Result<Name, FormatError> {
        let (start, len) = (self.word()? as usize, self.word()? as usize);
        let bytes = self.text.get(start..).and_then(|it| it.get(..len)).ok_or(self.invalid("a name past the text"))?;
        let text = std::str::from_utf8(bytes).map_err(|_| self.invalid("a name that is not UTF-8"))?;
        Ok(Name::new_symbol_root(Symbol::intern(text)))
    }

    fn expr_or_pat(&mut self) -> Result<ExprOrPatId, FormatError> {
        Ok(match self.word()? {
            0 => ExprOrPatId::ExprId(self.local()?),
            1 => ExprOrPatId::PatId(self.local()?),
            _ => return Err(self.invalid("a node that is neither an expression nor a pattern")),
        })
    }

    fn node(&mut self) -> Result<ExprOrPatIdPacked, FormatError> {
        Ok(match self.expr_or_pat()? {
            ExprOrPatId::ExprId(it) => it.into(),
            ExprOrPatId::PatId(it) => it.into(),
        })
    }

    fn opt_expr(&mut self) -> Result<Option<ExprId>, FormatError> {
        match self.word()? {
            0 => Ok(None),
            1 => self.local().map(Some),
            _ => Err(self.invalid("an optional expression of no kind")),
        }
    }

    fn variant(&mut self) -> Result<VariantId, FormatError> {
        let kind = self.word()?;
        let bits = self.u64()?;
        u8::try_from(kind)
            .ok()
            .and_then(|kind| variant_of(kind, bits))
            .ok_or(self.invalid("a variant of no kind or id 0"))
    }

    fn generic_def(&mut self) -> Result<GenericDefId, FormatError> {
        let tag = self.word()?;
        let id = self.salsa()?;
        Ok(match tag {
            0 => AdtId::StructId(StructId::from_id(id)).into(),
            1 => AdtId::EnumId(EnumId::from_id(id)).into(),
            2 => AdtId::UnionId(UnionId::from_id(id)).into(),
            3 => ConstId::from_id(id).into(),
            4 => FunctionId::from_id(id).into(),
            5 => ImplId::from_id(id).into(),
            6 => StaticId::from_id(id).into(),
            7 => TraitId::from_id(id).into(),
            8 => TypeAliasId::from_id(id).into(),
            _ => return Err(self.invalid("a generic definition of no kind")),
        })
    }

    fn generic_param(&mut self) -> Result<GenericParamId, FormatError> {
        let tag = self.word()?;
        let parent = self.generic_def()?;
        let local = self.word()?;
        let type_or_const = TypeOrConstParamId { parent, local_id: local_of(local) };
        Ok(match tag {
            0 => TypeParamId::from_unchecked(type_or_const).into(),
            1 => ConstParamId::from_unchecked(type_or_const).into(),
            2 => LifetimeParamId { parent, local_id: local_of(local) }.into(),
            _ => return Err(self.invalid("a generic parameter of no kind")),
        })
    }

    fn generics_len_kind(&mut self) -> Result<IncorrectGenericsLenKind, FormatError> {
        Ok(match self.word()? {
            0 => IncorrectGenericsLenKind::Lifetimes,
            1 => IncorrectGenericsLenKind::TypesAndConsts,
            _ => return Err(self.invalid("a generics count of no kind")),
        })
    }

    fn span(&mut self) -> Result<Span, FormatError> {
        Ok(match self.word()? {
            0 => Span::ExprId(self.local()?),
            1 => Span::PatId(self.local()?),
            2 => Span::BindingId(self.local()?),
            3 => Span::TypeRefId(self.local()?),
            4 => Span::Dummy,
            _ => return Err(self.invalid("a span of no kind")),
        })
    }

    fn closure_kind(&mut self) -> Result<ClosureKind, FormatError> {
        Ok(match self.word()? {
            0 => ClosureKind::Fn,
            1 => ClosureKind::FnMut,
            2 => ClosureKind::FnOnce,
            _ => return Err(self.invalid("a closure kind of no kind")),
        })
    }

    fn trait_predicate(&mut self, tys: &impl TyDecode) -> Result<StoredTraitPredicate, FormatError> {
        let words = [self.word()?, self.word()?, self.word()?];
        let trait_ref = tys.trait_ref(words)?;
        let polarity = match self.word()? {
            0 => PredicatePolarity::Positive,
            1 => PredicatePolarity::Negative,
            _ => return Err(self.invalid("a predicate of no polarity")),
        };
        Ok(StoredTraitPredicate { trait_ref, polarity })
    }

    /// What [`Sink::lowering_words`] wrote: `read` must take every one of its words.
    fn lowering_words<T>(
        &mut self,
        read: impl FnOnce(&mut Words<'_>) -> Result<T, FormatError>,
    ) -> Result<T, FormatError> {
        let len = self.len()?;
        let mut words = Words::new(&self.words[self.next..self.next + len], self.at);
        let value = read(&mut words)?;
        if !words.is_done() {
            return Err(self.invalid("a lowering diagnostic with words left over"));
        }
        self.next += len;
        Ok(value)
    }

    fn diagnostic(&mut self, tys: &impl TyDecode) -> Result<InferenceDiagnostic, FormatError> {
        use InferenceDiagnostic as D;
        Ok(match self.word()? {
            0 => D::NoSuchField {
                field: self.node()?,
                private: match self.word()? {
                    0 => None,
                    1 => Some(self.local()?),
                    _ => return Err(self.invalid("an optional field of no kind")),
                },
                variant: self.variant()?,
            },
            1 => D::MismatchedArrayPatLen {
                pat: self.local()?,
                expected: self.u64()?,
                found: self.u64()?,
                has_rest: self.bool()?,
            },
            2 => D::ArrayPatternWithoutFixedLength { pat: self.local()? },
            3 => D::ExpectedArrayOrSlicePat { pat: self.local()?, found: self.ty(tys)? },
            4 => D::InvalidRangePatType { pat: self.local()? },
            5 => D::DuplicateField { field: self.node()?, variant: self.variant()? },
            6 => D::PrivateField {
                expr: self.local()?,
                field: FieldId { parent: self.variant()?, local_id: self.local()? },
            },
            7 => {
                let id = self.node()?;
                let kind = self.word()?;
                let item_id = self.salsa()?;
                let item = match kind {
                    0 => AssocItemId::FunctionId(FunctionId::from_id(item_id)),
                    1 => AssocItemId::ConstId(ConstId::from_id(item_id)),
                    2 => AssocItemId::TypeAliasId(TypeAliasId::from_id(item_id)),
                    _ => return Err(self.invalid("an associated item of no kind")),
                };
                D::PrivateAssocItem { id, item }
            }
            8 => D::UnresolvedField {
                expr: self.local()?,
                receiver: self.ty(tys)?,
                name: self.name()?,
                method_with_same_name_exists: self.bool()?,
            },
            9 => D::UnresolvedMethodCall {
                expr: self.local()?,
                receiver: self.ty(tys)?,
                name: self.name()?,
                field_with_same_name: self.opt_ty(tys)?,
                assoc_func_with_same_name: match self.word()? {
                    0 => None,
                    1 => Some(self.id()?),
                    _ => return Err(self.invalid("an optional function of no kind")),
                },
            },
            10 => D::UnresolvedAssocItem { id: self.node()? },
            11 => D::UnresolvedIdent { id: self.node()? },
            12 => D::BreakOutsideOfLoop {
                expr: self.local()?,
                is_break: self.bool()?,
                bad_value_break: self.bool()?,
            },
            13 => D::NonExhaustiveRecordExpr { expr: self.local()? },
            14 => D::NonExhaustiveRecordPat { pat: self.local()?, variant: self.variant()? },
            15 => D::UnionPatMustHaveExactlyOneField { pat: self.local()? },
            16 => D::UnionPatHasRest { pat: self.local()? },
            17 => D::FunctionalRecordUpdateOnNonStruct { base_expr: self.local()? },
            18 => D::MismatchedArgCount {
                call_expr: self.local()?,
                expected: self.usize()?,
                found: self.usize()?,
                is_fn_trait_call: self.bool()?,
            },
            19 => D::MismatchedTupleStructPatArgCount {
                pat: self.local()?,
                expected: self.usize()?,
                found: self.usize()?,
            },
            20 => D::ExpectedFunction { call_expr: self.local()?, found: self.ty(tys)? },
            21 => D::CannotBeDereferenced { expr: self.local()?, found: self.ty(tys)? },
            22 => D::UnaryOperatorCannotBeApplied {
                expr: self.local()?,
                op: match self.word()? {
                    0 => UnaryOp::Deref,
                    1 => UnaryOp::Not,
                    2 => UnaryOp::Neg,
                    _ => return Err(self.invalid("a unary operator of no kind")),
                },
                found: self.ty(tys)?,
            },
            23 => D::BinaryOperatorCannotBeApplied {
                expr: self.local()?,
                op: {
                    let code = self.word()?;
                    binary_of(code).ok_or(self.invalid("a binary operator of no kind"))?
                },
                lhs: self.ty(tys)?,
                rhs: self.ty(tys)?,
            },
            24 => D::MutRefInImmRefPat { pat: self.local()? },
            25 => D::CannotImplicitlyDerefTraitObject { pat: self.local()?, found: self.ty(tys)? },
            26 => D::CannotIndexInto { expr: self.local()?, found: self.ty(tys)? },
            27 => D::TypedHole { expr: self.local()?, expected: self.ty(tys)? },
            28 => D::CastToUnsized { expr: self.local()?, cast_ty: self.ty(tys)? },
            29 => D::InvalidCast {
                expr: self.local()?,
                error: {
                    let code = self.word()?;
                    CAST_ERRORS.get(code as usize).cloned().ok_or(self.invalid("a cast error of no kind"))?
                },
                expr_ty: self.ty(tys)?,
                cast_ty: self.ty(tys)?,
            },
            30 => D::TyDiagnostic { diag: self.lowering_words(|words| words.diagnostic())? },
            31 => D::PathDiagnostic {
                node: self.node()?,
                diag: self.lowering_words(|words| words.path_diagnostic())?,
            },
            32 => D::MethodCallIncorrectGenericsLen {
                expr: self.local()?,
                provided_count: self.word()?,
                expected_count: self.word()?,
                kind: self.generics_len_kind()?,
                def: self.generic_def()?,
            },
            33 => D::MethodCallIllegalSizedBound { call_expr: self.local()? },
            34 => D::MethodCallIncorrectGenericsOrder {
                expr: self.local()?,
                param_id: self.generic_param()?,
                arg_idx: self.word()?,
                has_self_arg: self.bool()?,
            },
            35 => D::InvalidLhsOfAssignment { lhs: self.local()? },
            36 => D::TypeMustBeKnown {
                at_point: self.span()?,
                top_term: match self.word()? {
                    0 => None,
                    1 => {
                        let args = tys.args(self.word()?)?;
                        match args.as_ref().as_slice() {
                            [term] => Some(term.store()),
                            _ => return Err(self.invalid("a term that is not one argument")),
                        }
                    }
                    _ => return Err(self.invalid("an optional term of no kind")),
                },
            },
            37 => D::UnionExprMustHaveExactlyOneField { expr: self.local()? },
            38 => D::TypeMismatch {
                node: self.node()?,
                expected: self.ty(tys)?,
                found: self.ty(tys)?,
            },
            39 => {
                let span = self.span()?;
                let kind = match self.word()? {
                    0 => {
                        let trait_predicate = self.trait_predicate(tys)?;
                        let len = self.len()?;
                        let parent_trait_predicates =
                            (0..len).map(|_| self.trait_predicate(tys)).collect::<Result<_, _>>()?;
                        SolverDiagnosticKind::TraitUnimplemented { trait_predicate, parent_trait_predicates }
                    }
                    1 => SolverDiagnosticKind::ClosureKindMismatch {
                        expected: self.closure_kind()?,
                        found: self.closure_kind()?,
                        is_async: self.bool()?,
                    },
                    _ => return Err(self.invalid("a solver diagnostic of no kind")),
                };
                D::SolverDiagnostic(SolverDiagnostic { span, kind })
            }
            40 => D::ExplicitDropMethodUse {
                kind: match self.word()? {
                    0 => ExplicitDropMethodUseKind::MethodCall(self.local()?),
                    1 => ExplicitDropMethodUseKind::Path(self.node()?),
                    _ => return Err(self.invalid("an explicit drop of no kind")),
                },
            },
            41 => D::MutableRefBinding { pat: self.local()? },
            42 => D::YieldOutsideCoroutine { expr: self.local()? },
            43 => D::ReturnOutsideFunction {
                expr: self.local()?,
                kind: match self.word()? {
                    0 => ReturnKind::ReturnExpr,
                    1 => ReturnKind::BecomeExpr,
                    _ => return Err(self.invalid("a return of no kind")),
                },
            },
            44 => {
                let record = self.expr_or_pat()?;
                let variant = self.variant()?;
                let len = self.len()?;
                let missed_fields = (0..len).map(|_| self.local()).collect::<Result<_, _>>()?;
                D::RecordMissingFields { record, variant, missed_fields }
            }
            _ => return Err(self.invalid("an inference diagnostic of no kind")),
        })
    }

    fn mutability(&mut self) -> Result<Mutability, FormatError> {
        match self.word()? {
            0 => Ok(Mutability::Not),
            1 => Ok(Mutability::Mut),
            _ => Err(self.invalid("a mutability of no kind")),
        }
    }

    fn place(&mut self, tys: &impl TyDecode) -> Result<Place, FormatError> {
        let base_ty = self.ty(tys)?;
        let base = match self.word()? {
            0 => PlaceBase::Rvalue,
            1 => PlaceBase::StaticItem,
            2 => PlaceBase::Local(self.local()?),
            3 => PlaceBase::Upvar { closure: self.local()?, var_id: self.local()? },
            _ => return Err(self.invalid("a place base of no kind")),
        };
        let len = self.len()?;
        let projections = (0..len)
            .map(|_| {
                let ty = self.ty(tys)?;
                let kind = match self.word()? {
                    0 => ProjectionKind::Deref,
                    1 => ProjectionKind::Field { field_idx: self.word()?, variant_idx: self.word()? },
                    2 => ProjectionKind::Index,
                    3 => ProjectionKind::Subslice,
                    4 => ProjectionKind::UnwrapUnsafeBinder,
                    _ => return Err(self.invalid("a projection of no kind")),
                };
                Ok(Projection { ty, kind })
            })
            .collect::<Result<_, FormatError>>()?;
        Ok(Place { base_ty, base, projections })
    }

    fn sources(&mut self) -> Result<SmallVec<[CaptureSourceStack; 2]>, FormatError> {
        let len = self.len()?;
        (0..len)
            .map(|_| {
                let len = self.len()?;
                if len == 0 {
                    return Err(self.invalid("a capture source stack without a source"));
                }
                let mut stack = CaptureSourceStack::from_single(self.node()?);
                for _ in 1..len {
                    stack.push(self.node()?);
                }
                Ok(stack)
            })
            .collect()
    }

    fn closure(&mut self, tys: &impl TyDecode) -> Result<ClosureData, FormatError> {
        let len = self.len()?;
        let mut min_captures = crate::infer::RootVariableMinCaptureList::default();
        for _ in 0..len {
            let binding = self.local()?;
            let len = self.len()?;
            let captures = (0..len)
                .map(|_| {
                    let place = self.place(tys)?;
                    let sources = self.sources()?;
                    let capture_kind = match self.word()? {
                        0 => UpvarCapture::ByRef(BorrowKind::Immutable),
                        1 => UpvarCapture::ByRef(BorrowKind::UniqueImmutable),
                        2 => UpvarCapture::ByRef(BorrowKind::Mutable),
                        3 => UpvarCapture::ByUse,
                        4 => UpvarCapture::ByValue,
                        _ => return Err(self.invalid("a capture of no kind")),
                    };
                    let info = CaptureInfo { sources, capture_kind };
                    Ok(CapturedPlace { place, info, mutability: self.mutability()? })
                })
                .collect::<Result<_, FormatError>>()?;
            if min_captures.insert(binding, captures).is_some() {
                return Err(self.invalid("a closure that captures a variable twice"));
            }
        }
        let len = self.len()?;
        let fake_reads = (0..len)
            .map(|_| {
                let place = self.place(tys)?;
                let cause = match self.word()? {
                    0 => FakeReadCause::ForMatchGuard,
                    1 => FakeReadCause::ForMatchedPlace(self.opt_expr()?),
                    2 => FakeReadCause::ForGuardBinding,
                    3 => FakeReadCause::ForLet(self.opt_expr()?),
                    4 => FakeReadCause::ForIndex,
                    _ => return Err(self.invalid("a fake read of no cause")),
                };
                Ok((place, cause, self.sources()?))
            })
            .collect::<Result<_, FormatError>>()?;
        let inputs_and_output = self.tys(tys)?;
        let flags = self.word()?;
        let abi = flags & 0b11_1111;
        if flags >> 8 != 0 || abi as usize >= ExternAbi::ALL_VARIANTS.len() {
            return Err(self.invalid("a signature of no ABI"));
        }
        let safety = if flags & (1 << 6) != 0 { Safety::Safe } else { Safety::Unsafe };
        let fn_sig_kind = FnSigKind::new(ExternAbi::from_packed(abi as u8), safety, flags & (1 << 7) != 0);
        let liberated_sig =
            StoredFnSig::new(FnSig { inputs_and_output: inputs_and_output.as_ref(), fn_sig_kind });
        Ok(ClosureData { min_captures, fake_reads, liberated_sig })
    }
}

#[cfg(test)]
mod tests {
    use blaze_format::to_bytes;
    use either::Either;
    use hir_def::hir::{BindingId, PatId};
    use la_arena::ArenaMap;

    use super::*;
    use crate::{
        infer::{BindingMode, ByRef, columns::CsrBuilder},
        lower::diagnostics::{
            GenericArgsProhibitedReason, PathGenericsSource, PathLoweringDiagnostic,
            TyLoweringDiagnostic,
        },
        next_solver::{GenericArg, StoredGenericArg, default_types},
    };

    /// Types and arguments as a list, without a database: what the round trip of everything but
    /// the type table is tested with. A trait reference needs an interner, so the tests hold none.
    #[derive(Default)]
    struct Model {
        tys: Vec<StoredTy>,
        args: Vec<StoredGenericArgs>,
    }

    impl<'db> TyEncode<'db> for Model {
        fn ty(&mut self, ty: Ty<'db>) -> u32 {
            self.tys.push(ty.store());
            self.tys.len() as u32 - 1
        }

        fn args(&mut self, args: GenericArgs<'db>) -> u32 {
            self.args.push(args.store());
            self.args.len() as u32 - 1
        }

        fn trait_ref(&mut self, _: &StoredTraitRef) -> [u32; 3] {
            panic!("the tests hold no trait reference")
        }
    }

    impl TyDecode for Model {
        fn ty(&self, at: u32) -> Result<StoredTy, FormatError> {
            self.tys.get(at as usize).cloned().ok_or(FormatError::Invalid { at: 0, what: "no such type" })
        }

        fn args(&self, at: u32) -> Result<StoredGenericArgs, FormatError> {
            self.args.get(at as usize).cloned().ok_or(FormatError::Invalid { at: 0, what: "no such arguments" })
        }

        fn trait_ref(&self, _: [u32; 3]) -> Result<StoredTraitRef, FormatError> {
            Err(FormatError::Invalid { at: 0, what: "no trait references" })
        }
    }

    fn id(index: u32) -> salsa::Id {
        salsa::Id::from_bits(u64::from(index) | (3 << 32))
    }

    fn expr(index: u32) -> ExprId {
        local_of(index)
    }

    fn pat(index: u32) -> PatId {
        local_of(index)
    }

    fn binding(index: u32) -> BindingId {
        local_of(index)
    }

    fn place(base: PlaceBase) -> Place {
        let types = &default_types().types;
        Place {
            base_ty: types.i32.store(),
            base,
            projections: vec![
                Projection { ty: types.u8.store(), kind: ProjectionKind::Field { field_idx: 1, variant_idx: 0 } },
                Projection { ty: types.bool.store(), kind: ProjectionKind::Deref },
            ],
        }
    }

    fn sources(lens: &[u32]) -> SmallVec<[CaptureSourceStack; 2]> {
        lens.iter()
            .map(|&len| {
                let mut stack = CaptureSourceStack::from_single(expr(1).into());
                (1..len).for_each(|it| stack.push(if it % 2 == 0 { expr(it).into() } else { pat(it).into() }));
                stack
            })
            .collect()
    }

    fn every_diagnostic() -> Vec<InferenceDiagnostic> {
        use InferenceDiagnostic as D;
        let types = &default_types().types;
        let ty = |it: Ty<'static>| it.store();
        let variant: VariantId = StructId::from_id(id(4)).into();
        let enum_variant: VariantId = EnumVariantId::from_id(id(5)).into();
        let def: GenericDefId = ImplId::from_id(id(9)).into();
        let param = TypeOrConstParamId { parent: def, local_id: local_of(2) };
        let segment = PathGenericsSource::Segment(1);
        let assoc = PathGenericsSource::AssocType { segment: 2, assoc_type: 3 };
        let mut all = vec![
            D::NoSuchField { field: expr(1).into(), private: Some(local_of(3)), variant },
            D::NoSuchField { field: pat(2).into(), private: None, variant: enum_variant },
            D::MismatchedArrayPatLen { pat: pat(1), expected: 1 << 40, found: 3, has_rest: true },
            D::ArrayPatternWithoutFixedLength { pat: pat(2) },
            D::ExpectedArrayOrSlicePat { pat: pat(3), found: ty(types.u8) },
            D::InvalidRangePatType { pat: pat(4) },
            D::DuplicateField { field: pat(5).into(), variant },
            D::PrivateField { expr: expr(2), field: FieldId { parent: enum_variant, local_id: local_of(7) } },
            D::PrivateAssocItem { id: expr(3).into(), item: AssocItemId::TypeAliasId(TypeAliasId::from_id(id(6))) },
            D::UnresolvedField {
                expr: expr(4),
                receiver: ty(types.bool),
                name: Name::new_root("label"),
                method_with_same_name_exists: true,
            },
            D::UnresolvedMethodCall {
                expr: expr(5),
                receiver: ty(types.i32),
                name: Name::new_root("frobnicate"),
                field_with_same_name: Some(ty(types.unit)),
                assoc_func_with_same_name: Some(FunctionId::from_id(id(7))),
            },
            D::UnresolvedMethodCall {
                expr: expr(6),
                receiver: ty(types.i32),
                name: Name::new_root(""),
                field_with_same_name: None,
                assoc_func_with_same_name: None,
            },
            D::UnresolvedAssocItem { id: pat(6).into() },
            D::UnresolvedIdent { id: expr(7).into() },
            D::BreakOutsideOfLoop { expr: expr(8), is_break: false, bad_value_break: true },
            D::NonExhaustiveRecordExpr { expr: expr(9) },
            D::NonExhaustiveRecordPat { pat: pat(7), variant },
            D::UnionPatMustHaveExactlyOneField { pat: pat(8) },
            D::UnionPatHasRest { pat: pat(9) },
            D::FunctionalRecordUpdateOnNonStruct { base_expr: expr(10) },
            D::MismatchedArgCount { call_expr: expr(11), expected: 2, found: 5, is_fn_trait_call: true },
            D::MismatchedTupleStructPatArgCount { pat: pat(10), expected: 1, found: 0 },
            D::ExpectedFunction { call_expr: expr(12), found: ty(types.never) },
            D::CannotBeDereferenced { expr: expr(13), found: ty(types.u8) },
            D::MutRefInImmRefPat { pat: pat(11) },
            D::CannotImplicitlyDerefTraitObject { pat: pat(12), found: ty(types.bool) },
            D::CannotIndexInto { expr: expr(14), found: ty(types.i32) },
            D::TypedHole { expr: expr(15), expected: ty(types.unit) },
            D::CastToUnsized { expr: expr(16), cast_ty: ty(types.u8) },
            D::TyDiagnostic {
                diag: TyLoweringDiagnostic::PathDiagnostic {
                    source: local_of(4),
                    diag: PathLoweringDiagnostic::IncorrectGenericsOrder {
                        generics_source: assoc,
                        param_id: ConstParamId::from_unchecked(param).into(),
                        arg_idx: 1,
                        has_self_arg: true,
                    },
                },
            },
            D::TyDiagnostic { diag: TyLoweringDiagnostic::InferVarsNotAllowed { source: Span::Dummy } },
            D::MethodCallIncorrectGenericsLen {
                expr: expr(17),
                provided_count: 3,
                expected_count: 1,
                kind: IncorrectGenericsLenKind::Lifetimes,
                def: AdtId::UnionId(UnionId::from_id(id(10))).into(),
            },
            D::MethodCallIllegalSizedBound { call_expr: expr(18) },
            D::MethodCallIncorrectGenericsOrder {
                expr: expr(19),
                param_id: LifetimeParamId { parent: def, local_id: local_of(0) }.into(),
                arg_idx: 0,
                has_self_arg: false,
            },
            D::InvalidLhsOfAssignment { lhs: expr(20) },
            D::TypeMustBeKnown { at_point: Span::BindingId(binding(3)), top_term: Some(arg_of(types.u8)) },
            D::TypeMustBeKnown { at_point: Span::PatId(pat(1)), top_term: None },
            D::UnionExprMustHaveExactlyOneField { expr: expr(21) },
            D::TypeMismatch { node: pat(13).into(), expected: ty(types.i32), found: ty(types.bool) },
            D::SolverDiagnostic(SolverDiagnostic {
                span: Span::ExprId(expr(22)),
                kind: SolverDiagnosticKind::ClosureKindMismatch {
                    expected: ClosureKind::Fn,
                    found: ClosureKind::FnOnce,
                    is_async: true,
                },
            }),
            D::ExplicitDropMethodUse { kind: ExplicitDropMethodUseKind::MethodCall(expr(23)) },
            D::ExplicitDropMethodUse { kind: ExplicitDropMethodUseKind::Path(pat(14).into()) },
            D::MutableRefBinding { pat: pat(15) },
            D::YieldOutsideCoroutine { expr: expr(24) },
            D::ReturnOutsideFunction { expr: expr(25), kind: ReturnKind::BecomeExpr },
            D::RecordMissingFields {
                record: ExprOrPatId::PatId(pat(16)),
                variant,
                missed_fields: vec![local_of(0), local_of(2)],
            },
        ];
        let unary = [UnaryOp::Deref, UnaryOp::Not, UnaryOp::Neg];
        all.extend(unary.map(|op| D::UnaryOperatorCannotBeApplied { expr: expr(26), op, found: ty(types.u8) }));
        all.extend((0..29).map(|code| D::BinaryOperatorCannotBeApplied {
            expr: expr(27),
            op: binary_of(code).unwrap(),
            lhs: ty(types.i32),
            rhs: ty(types.u8),
        }));
        all.extend(CAST_ERRORS.iter().map(|error| D::InvalidCast {
            expr: expr(28),
            error: error.clone(),
            expr_ty: ty(types.i32),
            cast_ty: ty(types.bool),
        }));
        let paths = [
            PathLoweringDiagnostic::GenericArgsProhibited { segment: 4, reason: GenericArgsProhibitedReason::Static },
            PathLoweringDiagnostic::ParenthesizedGenericArgsWithoutFnTrait { segment: 5 },
            PathLoweringDiagnostic::IncorrectGenericsLen {
                generics_source: segment,
                provided_count: 3,
                expected_count: 1,
                kind: IncorrectGenericsLenKind::TypesAndConsts,
                def,
            },
            PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source: segment,
                param_id: TypeParamId::from_unchecked(param).into(),
                arg_idx: 2,
                has_self_arg: false,
            },
            PathLoweringDiagnostic::ElidedLifetimesInPath { generics_source: assoc, def, expected_count: 2, hard_error: true },
            PathLoweringDiagnostic::ElisionFailure { generics_source: segment, def, expected_count: 1 },
            PathLoweringDiagnostic::MissingLifetime { generics_source: assoc, def, expected_count: 3 },
            PathLoweringDiagnostic::GenericDefaultRefersToSelf { segment: 0 },
        ];
        all.extend(paths.into_iter().map(|diag| D::PathDiagnostic { node: expr(29).into(), diag }));
        all
    }

    fn arg_of(ty: Ty<'static>) -> StoredGenericArg {
        GenericArg::from(ty).store()
    }

    fn closure() -> ClosureData {
        let types = &default_types().types;
        let mut min_captures = crate::infer::RootVariableMinCaptureList::default();
        min_captures.insert(
            binding(4),
            vec![
                CapturedPlace {
                    place: place(PlaceBase::Upvar { closure: expr(3), var_id: binding(4) }),
                    info: CaptureInfo { sources: sources(&[1, 2, 5]), capture_kind: UpvarCapture::ByRef(BorrowKind::UniqueImmutable) },
                    mutability: Mutability::Mut,
                },
                CapturedPlace {
                    place: place(PlaceBase::Local(binding(4))),
                    info: CaptureInfo { sources: sources(&[3]), capture_kind: UpvarCapture::ByValue },
                    mutability: Mutability::Not,
                },
            ],
        );
        min_captures.insert(binding(1), Vec::new());
        ClosureData {
            min_captures,
            fake_reads: Box::new([
                (place(PlaceBase::Rvalue), FakeReadCause::ForMatchedPlace(Some(expr(2))), sources(&[1])),
                (place(PlaceBase::StaticItem), FakeReadCause::ForLet(None), sources(&[])),
                (place(PlaceBase::Local(binding(0))), FakeReadCause::ForIndex, sources(&[4, 1])),
            ]),
            liberated_sig: StoredFnSig::new(FnSig {
                inputs_and_output: Tys::new_from_slice(&[types.i32, types.u8, types.bool]),
                fn_sig_kind: FnSigKind::new(ExternAbi::RustCall, Safety::Safe, false),
            }),
        }
    }

    /// A result with something in every column.
    fn result() -> InferenceResult<'static> {
        let types = &default_types().types;
        let args = |tys: &[Ty<'static>]| {
            let args: Vec<_> = tys.iter().map(|&it| GenericArg::from(it)).collect();
            GenericArgs::new_from_slice(&args).store()
        };
        let mut type_of_expr = ArenaMap::default();
        for (at, ty) in [(0, types.i32), (2, types.u8), (3, types.i32), (7, types.bool)] {
            type_of_expr.insert(expr(at), ty.store());
        }
        let mut expr_adjustments = CsrBuilder::default();
        expr_adjustments.insert(expr(1), ADJUSTS.iter().map(|kind| Adjustment { kind: kind.clone(), target: types.u8.store() }));
        expr_adjustments.ensure(expr(4));
        let mut assignment_coercions = CsrBuilder::default();
        assignment_coercions.insert(expr(6), [Adjustment { kind: Adjust::NeverToAny, target: types.unit.store() }]);
        let mut pat_adjustments = CsrBuilder::default();
        pat_adjustments.push(pat(2), PatAdjustment { kind: PatAdjust::OverloadedDeref, source: types.i32.store() });
        pat_adjustments.push(pat(2), PatAdjustment { kind: PatAdjust::BuiltinDeref, source: types.u8.store() });
        let mut mismatches = ExprOrPatSet::default();
        mismatches.insert(expr(70).into());
        mismatches.insert(pat(3).into());
        InferenceResult {
            method_resolutions: IndexColumn::from_entries([
                (expr(3), (FunctionId::from_id(id(20)), args(&[types.i32]))),
                (expr(9), (FunctionId::from_id(id(21)), args(&[]))),
            ]),
            field_resolutions: FieldColumn::from_entries([
                (expr(2), Either::Left(FieldId { parent: StructId::from_id(id(22)).into(), local_id: local_of(1) })),
                (expr(5), Either::Right(TupleFieldId { tuple: TupleId(1), index: 1 })),
                (expr(8), Either::Right(TupleFieldId { tuple: TupleId(0), index: 0 })),
            ]),
            variant_resolutions: ExprOrPatColumn {
                exprs: IndexColumn::from_entries([(expr(4), UnionId::from_id(id(23)).into())]),
                pats: IndexColumn::from_entries([(pat(1), EnumVariantId::from_id(id(24)).into())]),
            },
            assoc_resolutions: ExprOrPatColumn {
                exprs: IndexColumn::from_entries([(expr(6), (ConstId::from_id(id(25)).into(), args(&[types.u8])))]),
                pats: IndexColumn::from_entries([(pat(0), (FunctionId::from_id(id(26)).into(), args(&[])))]),
            },
            tuple_field_access_types: Box::new([
                Tys::new_from_slice(&[types.i32]).store(),
                Tys::new_from_slice(&[types.u8, types.bool]).store(),
                Tys::new_from_slice(&[]).store(),
            ]),
            type_of_expr: TyColumn::from_entries(type_of_expr),
            type_of_pat: TyColumn::from_entries([(pat(1), types.bool.store()), (pat(3), types.bool.store())]),
            type_of_binding: TyColumn::from_entries([(binding(0), types.unit.store())]),
            type_of_type_placeholder: TyColumn::from_entries([(local_of(2), types.never.store())]),
            type_of_opaque: SortedMap::from_entries([
                (InternedOpaqueTyId::from_id(id(30)), types.i32.store()),
                (InternedOpaqueTyId::from_id(id(27)), types.u8.store()),
            ]),
            has_errors: true,
            diagnostics: every_diagnostic().into_boxed_slice(),
            nodes_with_type_mismatches: mismatches,
            error_ty: types.unit.store(),
            expr_adjustments: expr_adjustments.freeze(),
            assignment_coercions: assignment_coercions.freeze(),
            pat_adjustments: pat_adjustments.freeze(),
            binding_modes: ModeColumn::from_entries([
                (pat(0), BindingMode(ByRef::Yes(Mutability::Mut), Mutability::Not)),
                (pat(4), BindingMode(ByRef::No, Mutability::Mut)),
            ]),
            skipped_ref_pats: [pat(130)].into_iter().collect(),
            coercion_casts: [expr(1), expr(64)].into_iter().collect(),
            closures_without_expectation: [expr(3)].into_iter().collect(),
            asm_sym_tys: SortedMap::from_entries([((expr(9), 2), types.i32.store()), ((expr(9), 0), types.u8.store())]),
            closures_data: SortedMap::from_entries([(expr(3), closure()), (expr(11), ClosureData {
                min_captures: Default::default(),
                fake_reads: Box::new([]),
                liberated_sig: StoredFnSig::new(FnSig {
                    inputs_and_output: Tys::new_from_slice(&[types.unit]),
                    fn_sig_kind: FnSigKind::new(ExternAbi::C { unwind: true }, Safety::Unsafe, true),
                }),
            })]),
            defined_anon_consts: Box::new([AnonConstId::from_id(id(31)), AnonConstId::from_id(id(32))]),
        }
    }

    fn round_trip(result: &InferenceResult<'static>) -> InferenceResult<'static> {
        let mut model = Model::default();
        let record = InferenceRecord::of(result, &mut model);
        let bytes = blaze_format::Aligned::copy_from(&to_bytes(&record));
        let read = InferenceRecord::read(&bytes).unwrap();
        let back: InferenceResult<'static> = read.decode(&model).unwrap();
        let in_memory: InferenceResult<'static> = record.as_ref().decode(&model).unwrap();
        assert_eq!(in_memory, back);
        back
    }

    #[test]
    fn a_result_with_every_column_round_trips() {
        let result = result();
        let back = round_trip(&result);
        assert_eq!(back, result);
        assert_eq!(salsa_hash(&back), salsa_hash(&result));
        assert_eq!(back.diagnostics.len(), every_diagnostic().len());
    }

    #[test]
    fn an_empty_result_round_trips() {
        let result = InferenceResult::new(default_types().types.unit);
        assert_eq!(round_trip(&result), result);
    }

    fn salsa_hash(result: &InferenceResult<'_>) -> u128 {
        use std::hash::Hash;
        let mut hasher = salsa::StableHasher::new();
        result.hash(&mut hasher);
        let (low, high) = hasher.finish128();
        u128::from(high) << 64 | u128::from(low)
    }

    #[test]
    fn the_type_table_comes_back_byte_for_byte() {
        let bytes: Vec<u8> = (0..3 * ALIGN).map(|it| (it * 37 % 251) as u8).collect();
        let words = table_words(&bytes);
        assert_eq!(words.len(), 3);
        assert_eq!(words[0] & 0xff, u128::from(bytes[0]));
        let back = table_bytes(&words);
        assert_eq!(&back[..], &bytes[..]);
        assert_eq!(back.as_ptr().align_offset(ALIGN), 0);
    }

    #[test]
    fn every_code_names_its_own_kind() {
        for (code, adjust) in ADJUSTS.iter().enumerate() {
            assert_eq!(adjust_code(adjust) as usize, code);
        }
        for code in 0..29 {
            assert_eq!(binary_code(binary_of(code).unwrap()), code);
        }
        assert!(binary_of(29).is_none());
        for (code, error) in CAST_ERRORS.iter().enumerate() {
            assert_eq!(cast_error_code(error) as usize, code);
        }
    }

    #[test]
    fn truncated_words_are_refused_not_misread() {
        let mut model = Model::default();
        let mut sink = Sink::default();
        let diagnostics = every_diagnostic();
        diagnostics.iter().for_each(|it| sink.diagnostic(&mut model, it));
        sink.closure(&mut model, &closure());
        for len in 0..sink.words.len() {
            let mut source = Source { words: &sink.words[..len], next: 0, text: &sink.text, at: 0 };
            let read = (0..diagnostics.len())
                .map(|_| source.diagnostic(&model))
                .collect::<Result<Vec<_>, _>>()
                .and_then(|_| source.closure(&model));
            assert!(read.is_err(), "{len} of {} words read as a whole", sink.words.len());
        }
    }

    #[test]
    fn a_corrupt_record_is_an_error() {
        let mut model = Model::default();
        let record = InferenceRecord::of(&result(), &mut model);
        let mut past = record.clone();
        past.spans32[C32::Words as usize][1] += 1;
        let bytes = blaze_format::Aligned::copy_from(&to_bytes(&past));
        assert!(InferenceRecord::read(&bytes).is_err());

        let mut misordered = record.clone();
        let [start, len] = misordered.spans32[C32::ExprTyAt as usize];
        assert!(len >= 3);
        misordered.u32s.swap(start as usize, start as usize + 2);
        assert!(!decodes(&misordered, &model));

        let mut flagged = record.clone();
        flagged.flags = 2;
        assert!(!decodes(&flagged, &model));

        let mut no_kind = record.clone();
        let [start, len] = no_kind.spans8[C8::ExprAdjustKinds as usize];
        assert!(len >= 1);
        no_kind.bytes[start as usize] = ADJUSTS.len() as u8;
        assert!(!decodes(&no_kind, &model));

        assert!(decodes(&record, &model));
    }

    fn decodes(record: &InferenceRecord, model: &Model) -> bool {
        let result: Result<InferenceResult<'static>, _> = record.as_ref().decode(model);
        result.is_ok()
    }
}
