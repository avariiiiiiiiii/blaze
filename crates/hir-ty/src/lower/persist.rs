//! The cold form of a crate's signature batches (`lower::batch`): what a build keeps of a
//! dependency's signatures once nothing reads them, written into a [`blaze_format`] buffer and
//! read back in place.
//!
//! The cold form is columns, as the batch is: the keys as their salsa ids (the database that
//! wrote a buffer is the one reading it, as `hir_def::hir::persist` writes them), each value
//! column as indices into the buffer's type table (`next_solver::persist`), and the side table's
//! diagnostics as words. The type table is a buffer of its own inside this one, at a buffer's
//! alignment, so it is read in place too. [`ColdBatch`] is the `'static` form, which
//! [`Persist`] writes; [`ColdBatch::of_field_types`] and the like make it from a batch through a
//! [`TyTableWriter`], [`ColdBatchRef::field_types`] and the like make the batch again through a
//! [`TyTableReader`].

use std::mem::offset_of;

use blaze_format::{
    ALIGN, FormatError, Part, Persist, Record, SchemaHash, Slice, Writer, open_as, schema,
    schema_of,
};
use hir_def::{
    AdtId, ConstId, ConstParamId, EnumId, EnumVariantId, FunctionId, GenericDefId,
    GenericParamId, ImplId, LifetimeParamId, StaticId, StructId, TraitId, TypeAliasId,
    TypeOrConstParamId, TypeParamId, UnionId, VariantId,
};
use la_arena::{Idx, RawIdx};
use salsa::plumbing::{AsId, FromId};

use crate::{
    Span, TyLoweringDiagnostic,
    db::AnonConstId,
    lower::{
        FieldType, GenericPredicates,
        batch::{
            Batch, CrateFieldTypes, CrateImplPredicates, CrateImplSelfTys, CrateImplTraits,
            SideTable,
        },
        diagnostics::{
            GenericArgsProhibitedReason, IncorrectGenericsLenKind, PathGenericsSource,
            PathLoweringDiagnostic,
        },
    },
    next_solver::{
        DbInterner, StoredEarlyBinder, StoredTraitRef, TraitRef,
        persist::{TyTableReader, TyTableWriter},
    },
};

const _: () = assert!(align_of::<u128>() == ALIGN, "the type table is placed as `u128`s");

/// The schema of the type table's own buffer inside a cold batch.
const TY_TABLE_SCHEMA: SchemaHash = schema("hir_ty::lower::persist::TyTable");

/// Which batch a cold batch is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum BatchKind {
    FieldTypes = 0,
    ImplSelfTys = 1,
    ImplTraits = 2,
    ImplPredicates = 3,
}

impl BatchKind {
    fn from_word(word: u64) -> Option<BatchKind> {
        Some(match word {
            0 => BatchKind::FieldTypes,
            1 => BatchKind::ImplSelfTys,
            2 => BatchKind::ImplTraits,
            3 => BatchKind::ImplPredicates,
            _ => return None,
        })
    }
}

blaze_format::record! {
    /// The predicates of one impl ([`GenericPredicates`]): its clauses as an index in the type
    /// table, where each part of them starts, and `has_trait_implied_predicate` as 0 or 1.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct PredicatesRec: 1 {
        pub clauses: u32,
        pub parent_assoc_ty_bounds_start: u32,
        pub own_predicates_start: u32,
        pub own_assoc_ty_bounds_start: u32,
        pub has_trait_implied_predicate: u32,
    }
}

blaze_format::record! {
    /// A batch's side table: the rows with something, where each one's diagnostics and anonymous
    /// constants start, the diagnostics as words one after another, the constants as salsa ids.
    #[derive(Clone, Copy)]
    struct SideRec: 1 {
        rows: Slice<u32>,
        diagnostics_at: Slice<u32>,
        anon_consts_at: Slice<u32>,
        diagnostics: Slice<u32>,
        anon_consts: Slice<u64>,
    }
}

blaze_format::record! {
    /// A cold batch. Which columns are filled depends on `kind` ([`ColdBatch`] says).
    #[derive(Clone, Copy)]
    struct BatchRoot: 1 {
        kind: u64,
        table_root: u64,
        key_tags: Slice<u8>,
        keys: Slice<u64>,
        starts: Slice<u32>,
        tys: Slice<u32>,
        defaults: Slice<u32>,
        trait_ids: Slice<u64>,
        trait_args: Slice<u32>,
        predicates: Slice<PredicatesRec>,
        side: SideRec,
        table: Slice<u128>,
    }
}

const _: () = assert!(size_of::<SideRec>() == 5 * 8);
const _: () = assert!(size_of::<BatchRoot>() == 2 * 8 + 9 * 8 + size_of::<SideRec>());

/// A batch's side table, cold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ColdSide {
    rows: Vec<u32>,
    diagnostics_at: Vec<u32>,
    anon_consts_at: Vec<u32>,
    diagnostics: Vec<u32>,
    anon_consts: Vec<u64>,
}

/// A crate's batch of one kind of signature, its keys ascending, in its `'static` form:
///
/// - field types: `key_tags` and `keys` the variants, `starts` where each variant's fields start
///   in `tys` and `defaults` (one more than the variants), `tys` each field's type, `defaults`
///   0 for a field with no default and one more than its constant's index otherwise;
/// - impl self types: `keys` the impls, `tys` their self types;
/// - impl traits: `keys` the impls, `trait_ids` each one's trait (0 for an inherent impl) and
///   `trait_args` its arguments;
/// - impl predicates: `keys` the impls, `predicates` theirs.
///
/// Every other column is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdBatch {
    kind: BatchKind,
    key_tags: Vec<u8>,
    keys: Vec<u64>,
    starts: Vec<u32>,
    tys: Vec<u32>,
    defaults: Vec<u32>,
    trait_ids: Vec<u64>,
    trait_args: Vec<u32>,
    predicates: Vec<PredicatesRec>,
    side: ColdSide,
    /// The type table's buffer, header and all.
    table: Vec<u128>,
    table_root: u64,
}

/// A batch's side table read in place.
#[derive(Debug, Clone, Copy)]
struct SideRef<'a> {
    rows: &'a [u32],
    diagnostics_at: &'a [u32],
    anon_consts_at: &'a [u32],
    diagnostics: &'a [u32],
    anon_consts: &'a [u64],
}

/// A [`ColdBatch`] read in place: what [`Persist::read`] gives, and [`ColdBatch::as_ref`].
#[derive(Debug, Clone, Copy)]
pub struct ColdBatchRef<'a> {
    kind: BatchKind,
    /// Where the batch's root is in its buffer, for errors.
    at: u64,
    key_tags: &'a [u8],
    keys: &'a [u64],
    starts: &'a [u32],
    tys: &'a [u32],
    defaults: &'a [u32],
    trait_ids: &'a [u64],
    trait_args: &'a [u32],
    predicates: &'a [PredicatesRec],
    side: SideRef<'a>,
    table: &'a [u8],
    table_root: u64,
}

fn bytes_of(words: &[u128]) -> &[u8] {
    // SAFETY: a `u128` is 16 initialized bytes without padding, and a `u8` has no alignment.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), size_of_val(words)) }
}

fn salsa_id(bits: u64) -> Option<salsa::Id> {
    (bits as u32 != 0).then(|| salsa::Id::from_bits(bits))
}

fn id_bits(id: impl AsId) -> u64 {
    id.as_id().as_bits()
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

impl ColdBatch {
    fn new(kind: BatchKind) -> ColdBatch {
        ColdBatch {
            kind,
            key_tags: Vec::new(),
            keys: Vec::new(),
            starts: Vec::new(),
            tys: Vec::new(),
            defaults: Vec::new(),
            trait_ids: Vec::new(),
            trait_args: Vec::new(),
            predicates: Vec::new(),
            side: ColdSide::default(),
            table: Vec::new(),
            table_root: 0,
        }
    }

    pub fn kind(&self) -> BatchKind {
        self.kind
    }

    /// Writes the type table into a buffer of its own, kept beside the columns.
    fn with_table(mut self, tys: TyTableWriter<'_>) -> ColdBatch {
        let mut out = Writer::new(TY_TABLE_SCHEMA);
        self.table_root = tys.write(&mut out);
        let bytes = out.finish(self.table_root);
        self.table = bytes
            .chunks_exact(size_of::<u128>())
            .map(|chunk| u128::from_le_bytes(chunk.try_into().expect("a chunk of 16 bytes")))
            .collect();
        self
    }

    fn impl_keys<V>(kind: BatchKind, batch: &Batch<'_, ImplId, V>) -> ColdBatch {
        let mut cold = ColdBatch::new(kind);
        cold.keys = batch.keys.iter().map(|&it| id_bits(it)).collect();
        cold.side = ColdSide::of(&batch.side);
        cold
    }

    pub fn of_field_types<'db>(
        interner: DbInterner<'db>,
        batch: &CrateFieldTypes<'db>,
    ) -> ColdBatch {
        let mut tys = TyTableWriter::new(interner);
        let mut cold = ColdBatch::new(BatchKind::FieldTypes);
        (cold.key_tags, cold.keys) = batch.variants.iter().map(|&it| variant_key(it)).unzip();
        cold.starts = batch.starts.to_vec();
        cold.tys.reserve_exact(batch.fields.len());
        cold.defaults.reserve_exact(batch.fields.len());
        for field in &batch.fields {
            cold.tys.push(tys.ty(field.ty().skip_binder()));
            cold.defaults.push(field.default().map_or(0, |default| {
                tys.konst(default.skip_binder())
                    .checked_add(1)
                    .expect("2^32 constants in one type table")
            }));
        }
        cold.side = ColdSide::of(&batch.side);
        cold.with_table(tys)
    }

    pub fn of_impl_self_tys<'db>(
        interner: DbInterner<'db>,
        batch: &CrateImplSelfTys<'db>,
    ) -> ColdBatch {
        let mut tys = TyTableWriter::new(interner);
        let mut cold = ColdBatch::impl_keys(BatchKind::ImplSelfTys, batch);
        cold.tys = batch.values.iter().map(|it| tys.ty(it.get().skip_binder())).collect();
        cold.with_table(tys)
    }

    pub fn of_impl_traits<'db>(
        interner: DbInterner<'db>,
        batch: &'db CrateImplTraits<'db>,
    ) -> ColdBatch {
        let mut tys = TyTableWriter::new(interner);
        let mut cold = ColdBatch::impl_keys(BatchKind::ImplTraits, batch);
        cold.trait_ids.reserve_exact(batch.values.len());
        cold.trait_args.reserve_exact(batch.values.len());
        for value in &batch.values {
            let (trait_id, args) = match value {
                Some(trait_ref) => {
                    let trait_ref = trait_ref.get(interner).skip_binder();
                    (id_bits(trait_ref.def_id.0), tys.args(trait_ref.args))
                }
                None => (0, 0),
            };
            cold.trait_ids.push(trait_id);
            cold.trait_args.push(args);
        }
        cold.with_table(tys)
    }

    pub fn of_impl_predicates<'db>(
        interner: DbInterner<'db>,
        batch: &CrateImplPredicates<'db>,
    ) -> ColdBatch {
        let mut tys = TyTableWriter::new(interner);
        let mut cold = ColdBatch::impl_keys(BatchKind::ImplPredicates, batch);
        cold.predicates = batch
            .values
            .iter()
            .map(|it| PredicatesRec {
                clauses: tys.clauses(it.predicates.get().skip_binder()),
                parent_assoc_ty_bounds_start: it.parent_assoc_ty_bounds_start,
                own_predicates_start: it.own_predicates_start,
                own_assoc_ty_bounds_start: it.own_assoc_ty_bounds_start,
                has_trait_implied_predicate: u32::from(it.has_trait_implied_predicate),
            })
            .collect();
        cold.with_table(tys)
    }

    pub fn as_ref(&self) -> ColdBatchRef<'_> {
        ColdBatchRef {
            kind: self.kind,
            at: 0,
            key_tags: &self.key_tags,
            keys: &self.keys,
            starts: &self.starts,
            tys: &self.tys,
            defaults: &self.defaults,
            trait_ids: &self.trait_ids,
            trait_args: &self.trait_args,
            predicates: &self.predicates,
            side: SideRef {
                rows: &self.side.rows,
                diagnostics_at: &self.side.diagnostics_at,
                anon_consts_at: &self.side.anon_consts_at,
                diagnostics: &self.side.diagnostics,
                anon_consts: &self.side.anon_consts,
            },
            table: bytes_of(&self.table),
            table_root: self.table_root,
        }
    }
}

impl Persist for ColdBatch {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("ColdBatch"),
        Part::U64(1),
        Part::Schema(<BatchRoot as Record>::SCHEMA),
    ]);
    type Ref<'a> = ColdBatchRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let key_tags = Placed::push(out, &self.key_tags);
        let keys = Placed::push(out, &self.keys);
        let starts = Placed::push(out, &self.starts);
        let tys = Placed::push(out, &self.tys);
        let defaults = Placed::push(out, &self.defaults);
        let trait_ids = Placed::push(out, &self.trait_ids);
        let trait_args = Placed::push(out, &self.trait_args);
        let predicates = Placed::push(out, &self.predicates);
        let rows = Placed::push(out, &self.side.rows);
        let diagnostics_at = Placed::push(out, &self.side.diagnostics_at);
        let anon_consts_at = Placed::push(out, &self.side.anon_consts_at);
        let diagnostics = Placed::push(out, &self.side.diagnostics);
        let anon_consts = Placed::push(out, &self.side.anon_consts);
        let table = Placed::push(out, &self.table);

        let at = out.next::<BatchRoot>();
        let field = |offset: usize| at + offset as u64;
        let side_at = field(offset_of!(BatchRoot, side));
        let side_field = |offset: usize| side_at + offset as u64;
        let root = BatchRoot {
            kind: self.kind as u64,
            table_root: self.table_root,
            key_tags: key_tags.slice(field(offset_of!(BatchRoot, key_tags))),
            keys: keys.slice(field(offset_of!(BatchRoot, keys))),
            starts: starts.slice(field(offset_of!(BatchRoot, starts))),
            tys: tys.slice(field(offset_of!(BatchRoot, tys))),
            defaults: defaults.slice(field(offset_of!(BatchRoot, defaults))),
            trait_ids: trait_ids.slice(field(offset_of!(BatchRoot, trait_ids))),
            trait_args: trait_args.slice(field(offset_of!(BatchRoot, trait_args))),
            predicates: predicates.slice(field(offset_of!(BatchRoot, predicates))),
            side: SideRec {
                rows: rows.slice(side_field(offset_of!(SideRec, rows))),
                diagnostics_at: diagnostics_at.slice(side_field(offset_of!(SideRec, diagnostics_at))),
                anon_consts_at: anon_consts_at.slice(side_field(offset_of!(SideRec, anon_consts_at))),
                diagnostics: diagnostics.slice(side_field(offset_of!(SideRec, diagnostics))),
                anon_consts: anon_consts.slice(side_field(offset_of!(SideRec, anon_consts))),
            },
            table: table.slice(field(offset_of!(BatchRoot, table))),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<ColdBatchRef<'_>, FormatError> {
        let root = open_as::<BatchRoot>(buffer, Self::SCHEMA)?;
        let at = (root as *const BatchRoot as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
        let kind = BatchKind::from_word(root.kind)
            .ok_or(FormatError::Invalid { at, what: "a batch of no kind" })?;
        let side = &root.side;
        let cold = ColdBatchRef {
            kind,
            at,
            key_tags: root.key_tags.follow(buffer)?,
            keys: root.keys.follow(buffer)?,
            starts: root.starts.follow(buffer)?,
            tys: root.tys.follow(buffer)?,
            defaults: root.defaults.follow(buffer)?,
            trait_ids: root.trait_ids.follow(buffer)?,
            trait_args: root.trait_args.follow(buffer)?,
            predicates: root.predicates.follow(buffer)?,
            side: SideRef {
                rows: side.rows.follow(buffer)?,
                diagnostics_at: side.diagnostics_at.follow(buffer)?,
                anon_consts_at: side.anon_consts_at.follow(buffer)?,
                diagnostics: side.diagnostics.follow(buffer)?,
                anon_consts: side.anon_consts.follow(buffer)?,
            },
            table: bytes_of(root.table.follow(buffer)?),
            table_root: root.table_root,
        };
        cold.check()?;
        Ok(cold)
    }
}

impl<'a> ColdBatchRef<'a> {
    pub fn kind(&self) -> BatchKind {
        self.kind
    }

    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }

    fn ensure(&self, holds: bool, what: &'static str) -> Result<(), FormatError> {
        if holds { Ok(()) } else { Err(self.invalid(what)) }
    }

    /// That the columns are the ones `kind` fills, of lengths that agree: what a reader indexes
    /// without checking again.
    fn check(&self) -> Result<(), FormatError> {
        let rows = self.keys.len();
        let field_types = self.kind == BatchKind::FieldTypes;
        self.ensure(
            self.key_tags.len() == if field_types { rows } else { 0 },
            "a key tag column of the wrong length",
        )?;
        if field_types {
            let fields = self.tys.len();
            self.ensure(self.starts.len() == rows + 1, "a start column of the wrong length")?;
            self.ensure(
                self.starts.first() == Some(&0)
                    && self.starts.last().map(|&it| it as usize) == Some(fields)
                    && self.starts.is_sorted(),
                "field starts that are not ascending from 0 to the fields",
            )?;
            self.ensure(self.defaults.len() == fields, "a default column of the wrong length")?;
        } else {
            self.ensure(
                self.starts.is_empty() && self.defaults.is_empty(),
                "field columns in a batch of impls",
            )?;
        }
        let impl_column = |kind: BatchKind, len: usize| match self.kind == kind {
            true => len == rows,
            false => len == 0,
        };
        self.ensure(
            field_types || impl_column(BatchKind::ImplSelfTys, self.tys.len()),
            "a type column of the wrong length",
        )?;
        self.ensure(
            impl_column(BatchKind::ImplTraits, self.trait_ids.len())
                && impl_column(BatchKind::ImplTraits, self.trait_args.len()),
            "a trait column of the wrong length",
        )?;
        self.ensure(
            impl_column(BatchKind::ImplPredicates, self.predicates.len()),
            "a predicate column of the wrong length",
        )?;
        self.check_side(rows)
    }

    fn check_side(&self, rows: usize) -> Result<(), FormatError> {
        let side = &self.side;
        let len = side.rows.len();
        self.ensure(
            side.diagnostics_at.len() == len && side.anon_consts_at.len() == len,
            "side table columns of different lengths",
        )?;
        self.ensure(
            side.rows.is_sorted_by(|a, b| a < b) && side.rows.last().is_none_or(|&it| (it as usize) < rows),
            "side table rows that are not ascending rows of the batch",
        )?;
        self.ensure(
            side.diagnostics_at.is_sorted() && side.anon_consts_at.is_sorted(),
            "side table starts that are not ascending",
        )?;
        self.ensure(
            side.anon_consts_at.last().is_none_or(|&it| it as usize <= side.anon_consts.len()),
            "a side table start past its constants",
        )
    }

    fn expect(&self, kind: BatchKind) -> Result<(), FormatError> {
        self.ensure(self.kind == kind, "a batch of another kind")
    }

    fn table<'db>(&self, interner: DbInterner<'db>) -> Result<TyTableReader<'a, 'db>, FormatError> {
        TyTableReader::open(interner, self.table, self.table_root)
    }

    fn impl_keys(&self) -> Result<Box<[ImplId]>, FormatError> {
        let keys: Box<[ImplId]> = self
            .keys
            .iter()
            .map(|&bits| salsa_id(bits).map(ImplId::from_id).ok_or(self.invalid("an impl of id 0")))
            .collect::<Result<_, _>>()?;
        self.ensure(keys.is_sorted_by(|a, b| a < b), "impls that are not ascending")?;
        Ok(keys)
    }

    pub fn field_types<'db>(
        self,
        interner: DbInterner<'db>,
    ) -> Result<CrateFieldTypes<'db>, FormatError> {
        self.expect(BatchKind::FieldTypes)?;
        let tys = self.table(interner)?;
        let variants: Box<[VariantId]> = self
            .key_tags
            .iter()
            .zip(self.keys)
            .map(|(&tag, &bits)| variant_of(tag, bits).ok_or(self.invalid("a variant of no kind or id 0")))
            .collect::<Result<_, _>>()?;
        self.ensure(variants.is_sorted_by(|a, b| a < b), "variants that are not ascending")?;
        let fields = self
            .tys
            .iter()
            .zip(self.defaults)
            .map(|(&ty, &default)| {
                Ok(FieldType {
                    ty: StoredEarlyBinder::bind(tys.ty(ty)?.store()),
                    default: match default {
                        0 => None,
                        at => Some(StoredEarlyBinder::bind(tys.konst(at - 1)?.store())),
                    },
                })
            })
            .collect::<Result<_, FormatError>>()?;
        Ok(CrateFieldTypes {
            variants,
            starts: Box::from(self.starts),
            fields,
            side: self.side_table()?,
        })
    }

    pub fn impl_self_tys<'db>(
        self,
        interner: DbInterner<'db>,
    ) -> Result<CrateImplSelfTys<'db>, FormatError> {
        self.expect(BatchKind::ImplSelfTys)?;
        let tys = self.table(interner)?;
        let values = self
            .tys
            .iter()
            .map(|&ty| Ok(StoredEarlyBinder::bind(tys.ty(ty)?.store())))
            .collect::<Result<_, FormatError>>()?;
        Ok(Batch { keys: self.impl_keys()?, values, side: self.side_table()? })
    }

    pub fn impl_traits<'db>(
        self,
        interner: DbInterner<'db>,
    ) -> Result<CrateImplTraits<'db>, FormatError> {
        self.expect(BatchKind::ImplTraits)?;
        let tys = self.table(interner)?;
        let values = self
            .trait_ids
            .iter()
            .zip(self.trait_args)
            .map(|(&trait_id, &args)| {
                if trait_id == 0 {
                    return Ok(None);
                }
                let trait_id = salsa_id(trait_id)
                    .map(TraitId::from_id)
                    .ok_or(self.invalid("a trait of id index 0"))?;
                let trait_ref = TraitRef::new_from_args(interner, trait_id.into(), tys.args(args)?);
                Ok(Some(StoredEarlyBinder::bind(StoredTraitRef::new(trait_ref))))
            })
            .collect::<Result<_, FormatError>>()?;
        Ok(Batch { keys: self.impl_keys()?, values, side: self.side_table()? })
    }

    pub fn impl_predicates<'db>(
        self,
        interner: DbInterner<'db>,
    ) -> Result<CrateImplPredicates<'db>, FormatError> {
        self.expect(BatchKind::ImplPredicates)?;
        let tys = self.table(interner)?;
        let values = self
            .predicates
            .iter()
            .map(|it| {
                let clauses = tys.clauses(it.clauses)?;
                let has_trait_implied_predicate = match it.has_trait_implied_predicate {
                    0 => false,
                    1 => true,
                    _ => return Err(self.invalid("a bool that is neither 0 nor 1")),
                };
                self.ensure(
                    u32::from(has_trait_implied_predicate) <= it.parent_assoc_ty_bounds_start
                        && it.parent_assoc_ty_bounds_start <= it.own_predicates_start
                        && it.own_predicates_start <= it.own_assoc_ty_bounds_start
                        && it.own_assoc_ty_bounds_start as usize <= clauses.len(),
                    "predicate parts that are not in order within their clauses",
                )?;
                Ok(GenericPredicates {
                    predicates: StoredEarlyBinder::bind(clauses.store()),
                    has_trait_implied_predicate,
                    parent_assoc_ty_bounds_start: it.parent_assoc_ty_bounds_start,
                    own_predicates_start: it.own_predicates_start,
                    own_assoc_ty_bounds_start: it.own_assoc_ty_bounds_start,
                })
            })
            .collect::<Result<_, FormatError>>()?;
        Ok(Batch { keys: self.impl_keys()?, values, side: self.side_table()? })
    }

    fn side_table<'db>(&self) -> Result<SideTable<'db>, FormatError> {
        let side = &self.side;
        let mut words = Words { words: side.diagnostics, next: 0, at: self.at };
        let mut diagnostics = Vec::new();
        while !words.is_done() {
            diagnostics.push(words.diagnostic()?);
        }
        self.ensure(
            side.diagnostics_at.last().is_none_or(|&it| it as usize <= diagnostics.len()),
            "a side table start past its diagnostics",
        )?;
        let anon_consts: Box<[AnonConstId<'db>]> = side
            .anon_consts
            .iter()
            .map(|&bits| {
                salsa_id(bits).map(AnonConstId::from_id).ok_or(self.invalid("a constant of id 0"))
            })
            .collect::<Result<_, _>>()?;
        let range = |starts: &[u32], at: usize, len: usize| {
            starts[at] as usize..starts.get(at + 1).map_or(len, |&it| it as usize)
        };
        for row in 0..side.rows.len() {
            self.ensure(
                !range(side.diagnostics_at, row, diagnostics.len()).is_empty()
                    || !range(side.anon_consts_at, row, anon_consts.len()).is_empty(),
                "a side table row with nothing",
            )?;
        }
        Ok(SideTable {
            rows: Box::from(side.rows),
            diagnostics_at: Box::from(side.diagnostics_at),
            anon_consts_at: Box::from(side.anon_consts_at),
            diagnostics: diagnostics.into_boxed_slice(),
            anon_consts,
        })
    }
}

impl ColdSide {
    fn of(side: &SideTable<'_>) -> ColdSide {
        let mut diagnostics = Vec::new();
        for diagnostic in &side.diagnostics {
            put_diagnostic(&mut diagnostics, diagnostic);
        }
        ColdSide {
            rows: side.rows.to_vec(),
            diagnostics_at: side.diagnostics_at.to_vec(),
            anon_consts_at: side.anon_consts_at.to_vec(),
            diagnostics,
            anon_consts: side.anon_consts.iter().map(|&it| id_bits(it)).collect(),
        }
    }
}

fn put_u64(w: &mut Vec<u32>, word: u64) {
    w.extend([word as u32, (word >> 32) as u32]);
}

fn put_local<T>(w: &mut Vec<u32>, id: Idx<T>) {
    w.push(id.into_raw().into_u32());
}

fn put_generic_def(w: &mut Vec<u32>, def: GenericDefId) {
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
    w.push(tag);
    put_u64(w, bits);
}

fn put_generic_param(w: &mut Vec<u32>, param: GenericParamId) {
    let (tag, parent, local) = match param {
        GenericParamId::TypeParamId(it) => (0, it.parent(), it.local_id().into_raw().into_u32()),
        GenericParamId::ConstParamId(it) => (1, it.parent(), it.local_id().into_raw().into_u32()),
        GenericParamId::LifetimeParamId(it) => (2, it.parent, it.local_id.into_raw().into_u32()),
    };
    w.push(tag);
    put_generic_def(w, parent);
    w.push(local);
}

fn put_generics_source(w: &mut Vec<u32>, source: PathGenericsSource) {
    match source {
        PathGenericsSource::Segment(segment) => w.extend([0, segment]),
        PathGenericsSource::AssocType { segment, assoc_type } => w.extend([1, segment, assoc_type]),
    }
}

fn put_span(w: &mut Vec<u32>, span: Span) {
    match span {
        Span::ExprId(it) => w.extend([0, it.into_raw().into_u32()]),
        Span::PatId(it) => w.extend([1, it.into_raw().into_u32()]),
        Span::BindingId(it) => w.extend([2, it.into_raw().into_u32()]),
        Span::TypeRefId(it) => w.extend([3, it.into_raw().into_u32()]),
        Span::Dummy => w.push(4),
    }
}

pub(crate) fn put_diagnostic(w: &mut Vec<u32>, diagnostic: &TyLoweringDiagnostic) {
    match diagnostic {
        TyLoweringDiagnostic::PathDiagnostic { source, diag } => {
            w.push(0);
            put_local(w, *source);
            put_path_diagnostic(w, diag);
        }
        TyLoweringDiagnostic::InferVarsNotAllowed { source } => {
            w.push(1);
            put_span(w, *source);
        }
    }
}

pub(crate) fn put_path_diagnostic(w: &mut Vec<u32>, diagnostic: &PathLoweringDiagnostic) {
    match *diagnostic {
        PathLoweringDiagnostic::GenericArgsProhibited { segment, reason } => {
            let reason = match reason {
                GenericArgsProhibitedReason::Module => 0,
                GenericArgsProhibitedReason::TyParam => 1,
                GenericArgsProhibitedReason::SelfTy => 2,
                GenericArgsProhibitedReason::PrimitiveTy => 3,
                GenericArgsProhibitedReason::Const => 4,
                GenericArgsProhibitedReason::Static => 5,
                GenericArgsProhibitedReason::LocalVariable => 6,
                GenericArgsProhibitedReason::EnumVariant => 7,
            };
            w.extend([0, segment, reason]);
        }
        PathLoweringDiagnostic::ParenthesizedGenericArgsWithoutFnTrait { segment } => {
            w.extend([1, segment]);
        }
        PathLoweringDiagnostic::IncorrectGenericsLen {
            generics_source,
            provided_count,
            expected_count,
            kind,
            def,
        } => {
            w.push(2);
            put_generics_source(w, generics_source);
            let kind = match kind {
                IncorrectGenericsLenKind::Lifetimes => 0,
                IncorrectGenericsLenKind::TypesAndConsts => 1,
            };
            w.extend([provided_count, expected_count, kind]);
            put_generic_def(w, def);
        }
        PathLoweringDiagnostic::IncorrectGenericsOrder {
            generics_source,
            param_id,
            arg_idx,
            has_self_arg,
        } => {
            w.push(3);
            put_generics_source(w, generics_source);
            put_generic_param(w, param_id);
            w.extend([arg_idx, u32::from(has_self_arg)]);
        }
        PathLoweringDiagnostic::ElidedLifetimesInPath {
            generics_source,
            def,
            expected_count,
            hard_error,
        } => {
            w.push(4);
            put_generics_source(w, generics_source);
            put_generic_def(w, def);
            w.extend([expected_count, u32::from(hard_error)]);
        }
        PathLoweringDiagnostic::ElisionFailure { generics_source, def, expected_count } => {
            w.push(5);
            put_generics_source(w, generics_source);
            put_generic_def(w, def);
            w.push(expected_count);
        }
        PathLoweringDiagnostic::MissingLifetime { generics_source, def, expected_count } => {
            w.push(6);
            put_generics_source(w, generics_source);
            put_generic_def(w, def);
            w.push(expected_count);
        }
        PathLoweringDiagnostic::GenericDefaultRefersToSelf { segment } => w.extend([7, segment]),
    }
}

/// Diagnostics' words being read.
pub(crate) struct Words<'a> {
    words: &'a [u32],
    next: usize,
    /// Where the batch's root is, for errors.
    at: u64,
}

impl<'a> Words<'a> {
    /// `words` as [`put_diagnostic`] and [`put_path_diagnostic`] wrote them, in a record at `at`.
    pub(crate) fn new(words: &'a [u32], at: u64) -> Words<'a> {
        Words { words, next: 0, at }
    }
}

impl Words<'_> {
    pub(crate) fn is_done(&self) -> bool {
        self.next == self.words.len()
    }

    fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.at, what }
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        let word = *self.words.get(self.next).ok_or(self.invalid("a diagnostic past its words"))?;
        self.next += 1;
        Ok(word)
    }

    fn u64(&mut self) -> Result<u64, FormatError> {
        let low = self.u32()?;
        Ok(u64::from(low) | (u64::from(self.u32()?) << 32))
    }

    fn bool(&mut self) -> Result<bool, FormatError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.invalid("a bool that is neither 0 nor 1")),
        }
    }

    fn local<T>(&mut self) -> Result<Idx<T>, FormatError> {
        Ok(Idx::from_raw(RawIdx::from_u32(self.u32()?)))
    }

    fn id(&mut self) -> Result<salsa::Id, FormatError> {
        let bits = self.u64()?;
        salsa_id(bits).ok_or(self.invalid("a salsa id of index 0"))
    }

    fn generic_def(&mut self) -> Result<GenericDefId, FormatError> {
        let tag = self.u32()?;
        let id = self.id()?;
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
        let tag = self.u32()?;
        let parent = self.generic_def()?;
        let local = self.u32()?;
        let type_or_const =
            || TypeOrConstParamId { parent, local_id: Idx::from_raw(RawIdx::from_u32(local)) };
        Ok(match tag {
            0 => TypeParamId::from_unchecked(type_or_const()).into(),
            1 => ConstParamId::from_unchecked(type_or_const()).into(),
            2 => LifetimeParamId { parent, local_id: Idx::from_raw(RawIdx::from_u32(local)) }
                .into(),
            _ => return Err(self.invalid("a generic parameter of no kind")),
        })
    }

    fn generics_source(&mut self) -> Result<PathGenericsSource, FormatError> {
        Ok(match self.u32()? {
            0 => PathGenericsSource::Segment(self.u32()?),
            1 => PathGenericsSource::AssocType { segment: self.u32()?, assoc_type: self.u32()? },
            _ => return Err(self.invalid("a generics source of no kind")),
        })
    }

    fn span(&mut self) -> Result<Span, FormatError> {
        Ok(match self.u32()? {
            0 => Span::ExprId(self.local()?),
            1 => Span::PatId(self.local()?),
            2 => Span::BindingId(self.local()?),
            3 => Span::TypeRefId(self.local()?),
            4 => Span::Dummy,
            _ => return Err(self.invalid("a span of no kind")),
        })
    }

    pub(crate) fn diagnostic(&mut self) -> Result<TyLoweringDiagnostic, FormatError> {
        Ok(match self.u32()? {
            0 => TyLoweringDiagnostic::PathDiagnostic {
                source: self.local()?,
                diag: self.path_diagnostic()?,
            },
            1 => TyLoweringDiagnostic::InferVarsNotAllowed { source: self.span()? },
            _ => return Err(self.invalid("a lowering diagnostic of no kind")),
        })
    }

    pub(crate) fn path_diagnostic(&mut self) -> Result<PathLoweringDiagnostic, FormatError> {
        Ok(match self.u32()? {
            0 => {
                let segment = self.u32()?;
                let reason = match self.u32()? {
                    0 => GenericArgsProhibitedReason::Module,
                    1 => GenericArgsProhibitedReason::TyParam,
                    2 => GenericArgsProhibitedReason::SelfTy,
                    3 => GenericArgsProhibitedReason::PrimitiveTy,
                    4 => GenericArgsProhibitedReason::Const,
                    5 => GenericArgsProhibitedReason::Static,
                    6 => GenericArgsProhibitedReason::LocalVariable,
                    7 => GenericArgsProhibitedReason::EnumVariant,
                    _ => return Err(self.invalid("a prohibition of no reason")),
                };
                PathLoweringDiagnostic::GenericArgsProhibited { segment, reason }
            }
            1 => PathLoweringDiagnostic::ParenthesizedGenericArgsWithoutFnTrait {
                segment: self.u32()?,
            },
            2 => {
                let generics_source = self.generics_source()?;
                let (provided_count, expected_count) = (self.u32()?, self.u32()?);
                let kind = match self.u32()? {
                    0 => IncorrectGenericsLenKind::Lifetimes,
                    1 => IncorrectGenericsLenKind::TypesAndConsts,
                    _ => return Err(self.invalid("a generics count of no kind")),
                };
                PathLoweringDiagnostic::IncorrectGenericsLen {
                    generics_source,
                    provided_count,
                    expected_count,
                    kind,
                    def: self.generic_def()?,
                }
            }
            3 => PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source: self.generics_source()?,
                param_id: self.generic_param()?,
                arg_idx: self.u32()?,
                has_self_arg: self.bool()?,
            },
            4 => PathLoweringDiagnostic::ElidedLifetimesInPath {
                generics_source: self.generics_source()?,
                def: self.generic_def()?,
                expected_count: self.u32()?,
                hard_error: self.bool()?,
            },
            5 => PathLoweringDiagnostic::ElisionFailure {
                generics_source: self.generics_source()?,
                def: self.generic_def()?,
                expected_count: self.u32()?,
            },
            6 => PathLoweringDiagnostic::MissingLifetime {
                generics_source: self.generics_source()?,
                def: self.generic_def()?,
                expected_count: self.u32()?,
            },
            7 => PathLoweringDiagnostic::GenericDefaultRefersToSelf { segment: self.u32()? },
            _ => return Err(self.invalid("a path diagnostic of no kind")),
        })
    }
}

#[cfg(test)]
mod tests {
    use blaze_format::{Aligned, to_bytes};
    use hir_def::type_ref::TypeRefId;

    use super::*;

    #[test]
    fn bytes_of_equals_the_words_copied_out_byte_by_byte() {
        for len in [0usize, 1, 2, 7, 64] {
            let words: Vec<u128> = (0..len as u128)
                .map(|it| it.wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C834) ^ !it)
                .collect();
            let copied: Vec<u8> = words.iter().flat_map(|it| it.to_ne_bytes()).collect();
            assert_eq!(bytes_of(&words), &copied[..], "{len} words");
        }
    }

    fn id(index: u32) -> salsa::Id {
        salsa::Id::from_bits(u64::from(index) | (3 << 32))
    }

    fn every_diagnostic() -> Vec<TyLoweringDiagnostic> {
        let source: TypeRefId = Idx::from_raw(RawIdx::from_u32(7));
        let def: GenericDefId = ImplId::from_id(id(9)).into();
        let trait_def: GenericDefId = TraitId::from_id(id(10)).into();
        let adt: GenericDefId = AdtId::EnumId(EnumId::from_id(id(11))).into();
        let param = TypeOrConstParamId { parent: def, local_id: Idx::from_raw(RawIdx::from_u32(2)) };
        let path = |diag| TyLoweringDiagnostic::PathDiagnostic { source, diag };
        let segment = PathGenericsSource::Segment(1);
        let assoc = PathGenericsSource::AssocType { segment: 2, assoc_type: 3 };
        vec![
            path(PathLoweringDiagnostic::GenericArgsProhibited {
                segment: 4,
                reason: GenericArgsProhibitedReason::EnumVariant,
            }),
            path(PathLoweringDiagnostic::ParenthesizedGenericArgsWithoutFnTrait { segment: 5 }),
            path(PathLoweringDiagnostic::IncorrectGenericsLen {
                generics_source: assoc,
                provided_count: 3,
                expected_count: 1,
                kind: IncorrectGenericsLenKind::TypesAndConsts,
                def: adt,
            }),
            path(PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source: segment,
                param_id: TypeParamId::from_unchecked(param).into(),
                arg_idx: 2,
                has_self_arg: true,
            }),
            path(PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source: segment,
                param_id: ConstParamId::from_unchecked(param).into(),
                arg_idx: 0,
                has_self_arg: false,
            }),
            path(PathLoweringDiagnostic::IncorrectGenericsOrder {
                generics_source: assoc,
                param_id: LifetimeParamId { parent: trait_def, local_id: Idx::from_raw(RawIdx::from_u32(0)) }
                    .into(),
                arg_idx: 1,
                has_self_arg: false,
            }),
            path(PathLoweringDiagnostic::ElidedLifetimesInPath {
                generics_source: segment,
                def,
                expected_count: 2,
                hard_error: true,
            }),
            path(PathLoweringDiagnostic::ElisionFailure { generics_source: assoc, def, expected_count: 1 }),
            path(PathLoweringDiagnostic::MissingLifetime { generics_source: segment, def: trait_def, expected_count: 3 }),
            path(PathLoweringDiagnostic::GenericDefaultRefersToSelf { segment: 0 }),
            TyLoweringDiagnostic::InferVarsNotAllowed { source: Span::TypeRefId(source) },
            TyLoweringDiagnostic::InferVarsNotAllowed { source: Span::ExprId(Idx::from_raw(RawIdx::from_u32(1))) },
            TyLoweringDiagnostic::InferVarsNotAllowed { source: Span::Dummy },
        ]
    }

    #[test]
    fn every_diagnostic_round_trips_through_words() {
        let diagnostics = every_diagnostic();
        let mut words = Vec::new();
        diagnostics.iter().for_each(|it| put_diagnostic(&mut words, it));
        let mut read = Words { words: &words, next: 0, at: 0 };
        let mut back = Vec::new();
        while !read.is_done() {
            back.push(read.diagnostic().unwrap());
        }
        assert_eq!(back, diagnostics);
    }

    #[test]
    fn truncated_diagnostic_words_are_refused() {
        let mut words = Vec::new();
        put_diagnostic(&mut words, &every_diagnostic()[2]);
        words.pop();
        let mut read = Words { words: &words, next: 0, at: 0 };
        assert!(read.diagnostic().is_err());
    }

    /// A cold batch of field types with a side table, its type table some bytes: the columns
    /// round-trip whatever the table holds.
    fn cold_field_types() -> ColdBatch {
        let mut table = Writer::new(TY_TABLE_SCHEMA);
        let table_root = table.push(&[1u32, 2, 3, 4]);
        let table = table.finish(table_root);
        let mut words = Vec::new();
        put_diagnostic(&mut words, &every_diagnostic()[0]);
        ColdBatch {
            kind: BatchKind::FieldTypes,
            key_tags: vec![1, 1, 2],
            keys: vec![id_bits(StructId::from_id(id(1))), id_bits(StructId::from_id(id(4))), id_bits(UnionId::from_id(id(2)))],
            starts: vec![0, 2, 2, 3],
            tys: vec![0, 1, 0],
            defaults: vec![0, 3, 0],
            trait_ids: Vec::new(),
            trait_args: Vec::new(),
            predicates: Vec::new(),
            side: ColdSide {
                rows: vec![1],
                diagnostics_at: vec![0],
                anon_consts_at: vec![0],
                diagnostics: words,
                anon_consts: vec![id(12).as_bits()],
            },
            table: table.chunks_exact(16).map(|it| u128::from_le_bytes(it.try_into().unwrap())).collect(),
            table_root,
        }
    }

    fn same(a: ColdBatchRef<'_>, b: ColdBatchRef<'_>) -> bool {
        a.kind == b.kind
            && a.key_tags == b.key_tags
            && a.keys == b.keys
            && a.starts == b.starts
            && a.tys == b.tys
            && a.defaults == b.defaults
            && a.trait_ids == b.trait_ids
            && a.trait_args == b.trait_args
            && a.predicates == b.predicates
            && a.side.rows == b.side.rows
            && a.side.diagnostics_at == b.side.diagnostics_at
            && a.side.anon_consts_at == b.side.anon_consts_at
            && a.side.diagnostics == b.side.diagnostics
            && a.side.anon_consts == b.side.anon_consts
            && a.table == b.table
            && a.table_root == b.table_root
    }

    #[test]
    fn a_cold_batch_round_trips_in_place() {
        let cold = cold_field_types();
        let buffer = Aligned::copy_from(&to_bytes(&cold));
        let read = ColdBatch::read(&buffer).unwrap();
        assert!(same(read, cold.as_ref()));
        assert_eq!(read.table.as_ptr() as usize % ALIGN, 0, "the type table is a buffer in place");
        assert_eq!(blaze_format::header(read.table).unwrap().root, cold.table_root);
        assert_eq!(read.side_table().unwrap().diagnostics.as_ref(), &every_diagnostic()[..1]);
    }

    #[test]
    fn equal_cold_batches_are_equal_bytes() {
        assert_eq!(to_bytes(&cold_field_types()), to_bytes(&cold_field_types()));
    }

    #[test]
    fn a_cold_batch_of_columns_that_disagree_is_refused() {
        let mut cold = cold_field_types();
        cold.defaults.pop();
        assert!(ColdBatch::read(&Aligned::copy_from(&to_bytes(&cold))).is_err());

        let mut cold = cold_field_types();
        cold.starts = vec![0, 3, 2, 3];
        assert!(ColdBatch::read(&Aligned::copy_from(&to_bytes(&cold))).is_err());

        let mut cold = cold_field_types();
        cold.trait_ids = vec![0, 0, 0];
        assert!(ColdBatch::read(&Aligned::copy_from(&to_bytes(&cold))).is_err());

        let mut cold = cold_field_types();
        cold.side.rows = vec![3];
        assert!(ColdBatch::read(&Aligned::copy_from(&to_bytes(&cold))).is_err());
    }

    #[test]
    fn a_side_table_row_with_nothing_is_refused() {
        let mut cold = cold_field_types();
        cold.side.rows = vec![0, 1];
        cold.side.diagnostics_at = vec![0, 0];
        cold.side.anon_consts_at = vec![0, 0];
        let buffer = Aligned::copy_from(&to_bytes(&cold));
        let read = ColdBatch::read(&buffer).unwrap();
        assert!(read.side_table().is_err());
    }

    #[test]
    fn a_batch_read_as_another_kind_is_refused() {
        let cold = cold_field_types();
        let read = cold.as_ref();
        assert!(read.expect(BatchKind::ImplTraits).is_err());
        assert!(read.expect(BatchKind::FieldTypes).is_ok());
    }

    #[test]
    fn variant_keys_round_trip() {
        for variant in [
            VariantId::from(EnumVariantId::from_id(id(5))),
            VariantId::from(StructId::from_id(id(6))),
            VariantId::from(UnionId::from_id(id(7))),
        ] {
            let (tag, bits) = variant_key(variant);
            assert_eq!(variant_of(tag, bits), Some(variant));
        }
        assert_eq!(variant_of(3, id(1).as_bits()), None);
        assert_eq!(variant_of(0, 0), None);
    }
}
