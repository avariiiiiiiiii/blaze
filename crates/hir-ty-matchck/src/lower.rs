//! rustc's pattern lowering (`rustc_mir_build/src/thir/pattern/mod.rs`, `PatCtxt::lower_pattern`,
//! and `const_to_pat.rs`) folded into `RustcPatCtxt::lower_pat`: a pattern of the body goes
//! straight to the analysis' `DeconstructedPat`, with no THIR pattern between. A constant is
//! evaluated and its value taken apart by its type's layout, as `const_to_pat` takes apart a
//! valtree.

use hir_ty::db::HirDatabase;
use std::cell::Cell;

use hir_def::{
    AdtId, ConstId, DefWithBodyId, VariantId,
    expr_store::{Body, HygieneId, path::Path, scope::ExprScopes},
    hir::{Expr, ExprId, ExprOrPatId, Literal, Pat as HirPat, PatId, RangeOp, UnaryOp},
    resolver::{Resolver, ValueNs, resolver_for_scope},
};
use intern::Symbol;
use rustc_apfloat::{
    Float,
    ieee::{Double, Half, Quad, Single},
};
use rustc_pattern_analysis::{
    constructor::{Constructor, IntRange, MaybeInfiniteInt, OpaqueId, RangeEnd, Slice, SliceKind},
    pat::{DeconstructedPat, IndexedPat},
};
use rustc_type_ir::{FloatTy, inherent::IntoKind};

use hir_ty::{
    BindingMode, ByRef, InferenceResult, MemoryMap, ParamEnvAndCrate,
    consteval::try_const_usize,
    infer::PatAdjust,
    method_resolution::CandidateId,
    next_solver::{Allocation, GenericArgs, Ty, TyKind},
    utils::detect_variant_from_bytes,
};

use crate::cx::{Cx, VariantIdx};

pub(crate) type Pat<'a, 'db> = DeconstructedPat<Cx<'a, 'db>>;

/// Why a pattern was not lowered. rustc reports each and the match is not checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LowerError {
    /// An error type checking or const evaluation reports: a pattern of an erroneous type, a
    /// path that resolves to no pattern, a constant that does not evaluate.
    Failed,
    /// E0030 or E0579.
    EmptyRange { pat: PatId, inclusive: bool },
}

/// How deep a constant's value is taken apart before it is called erroneous: a value has no
/// cycles, so only a broken memory map gets this deep.
const MAX_VALUE_DEPTH: u32 = 256;

pub(crate) struct Lowerer<'c, 'a, 'db> {
    pub(crate) cx: &'c Cx<'a, 'db>,
    pub(crate) body: &'c Body,
    pub(crate) infer: &'db InferenceResult<'db>,
    pub(crate) owner: DefWithBodyId,
    /// An expression the patterns being lowered are in the scope of, to resolve a path to a
    /// constant from: the `match`, the `let`'s initializer or block.
    pub(crate) scope: Cell<ExprId>,
}

/// A bound of a range pattern, before it is checked against the other.
#[derive(Clone, Copy)]
enum RangeBound {
    Int(MaybeInfiniteInt),
    Float(u128),
}

fn fields_of<'a, 'db>(pats: impl IntoIterator<Item = Pat<'a, 'db>>) -> Vec<IndexedPat<Cx<'a, 'db>>> {
    // alloc: `DeconstructedPat::new` takes its fields as a `Vec`; one per lowered pattern.
    pats.into_iter().enumerate().map(|(i, p)| p.at_index(i)).collect()
}

fn read_uint(bytes: &[u8]) -> Option<u128> {
    if bytes.len() > 16 {
        return None;
    }
    let mut buf = [0u8; 16];
    buf[..bytes.len()].copy_from_slice(bytes);
    Some(u128::from_le_bytes(buf))
}

fn truncate(bits: u128, size: u64) -> u128 {
    if size >= 128 { bits } else { bits & ((1u128 << size) - 1) }
}

impl<'c, 'a, 'db> Lowerer<'c, 'a, 'db> {
    pub(crate) fn new(
        cx: &'c Cx<'a, 'db>,
        body: &'c Body,
        infer: &'db InferenceResult<'db>,
        owner: DefWithBodyId,
    ) -> Self {
        Lowerer { cx, body, infer, owner, scope: Cell::new(body.root_expr()) }
    }

    fn new_pat(
        ctor: Constructor<Cx<'a, 'db>>,
        fields: Vec<IndexedPat<Cx<'a, 'db>>>,
        arity: usize,
        ty: Ty<'db>,
        src: PatId,
    ) -> Pat<'a, 'db> {
        DeconstructedPat::new(ctor, fields, arity, ty, src)
    }

    fn leaf(ctor: Constructor<Cx<'a, 'db>>, ty: Ty<'db>, src: PatId) -> Pat<'a, 'db> {
        Self::new_pat(ctor, Vec::new(), 0, ty, src) // alloc: empty, so none.
    }

    fn opaque(ty: Ty<'db>, src: PatId) -> Pat<'a, 'db> {
        Self::leaf(Constructor::Opaque(OpaqueId::new()), ty, src)
    }

    /// `lower_pattern`: the pattern with the dereferences match ergonomics put around it,
    /// outermost first.
    pub(crate) fn lower(&self, pat: PatId) -> Result<Pat<'a, 'db>, LowerError> {
        let mut lowered = self.lower_unadjusted(pat)?;
        if let Some(adjustments) = self.infer.pat_adjustment(pat) {
            for adjustment in adjustments.iter().rev() {
                let ty = adjustment.source.as_ref();
                let ctor = match adjustment.kind {
                    PatAdjust::BuiltinDeref => Constructor::Ref,
                    PatAdjust::OverloadedDeref => {
                        self.cx.has_deref_pat.set(true);
                        Constructor::DerefPattern(*lowered.ty())
                    }
                };
                // alloc: the fields `DeconstructedPat::new` takes.
                lowered = Self::new_pat(ctor, vec![lowered.at_index(0)], 1, ty, pat);
            }
        }
        Ok(lowered)
    }

    fn pat_ty(&self, pat: PatId) -> Result<Ty<'db>, LowerError> {
        let ty = self.infer.pat_ty(pat);
        if ty.references_non_lt_error() { Err(LowerError::Failed) } else { Ok(ty) }
    }

    fn lower_unadjusted(&self, pat: PatId) -> Result<Pat<'a, 'db>, LowerError> {
        let db = self.cx.db;
        let mut ty = self.pat_ty(pat)?;
        Ok(match self.body.pat(pat) {
            HirPat::Wild | HirPat::Rest | HirPat::Missing | HirPat::Expr(_) | HirPat::NotNull => {
                Self::leaf(Constructor::Wildcard, ty, pat)
            }
            HirPat::Bind { subpat, .. } => {
                // A `ref x` is of type `&T`; the place it binds, which is what it matches, is `T`.
                if let Some(BindingMode(ByRef::Yes(_), _)) = self.infer.binding_mode(pat)
                    && let TyKind::Ref(_, pointee, _) = ty.kind()
                {
                    ty = pointee;
                }
                match subpat {
                    Some(subpat) => return self.lower(subpat),
                    None => Self::leaf(Constructor::Wildcard, ty, pat),
                }
            }
            HirPat::Tuple { args, ellipsis } => {
                let TyKind::Tuple(tys) = ty.kind() else { return Err(LowerError::Failed) };
                let arity = tys.len();
                let fields = self.lower_tuple_fields(args, arity, ellipsis)?;
                Self::new_pat(Constructor::Struct, fields, arity, ty, pat)
            }
            HirPat::TupleStruct { args, ellipsis, .. } => {
                let variant =
                    self.infer.variant_resolution_for_pat(pat).ok_or(LowerError::Failed)?;
                let arity = variant.fields(db).fields().len();
                let fields = self.lower_tuple_fields(args, arity, ellipsis)?;
                self.variant_pat(pat, ty, variant, fields, arity)?
            }
            HirPat::Record { args, .. } => {
                let variant =
                    self.infer.variant_resolution_for_pat(pat).ok_or(LowerError::Failed)?;
                let variant_fields = variant.fields(db);
                let arity = variant_fields.fields().len();
                let mut fields = Vec::with_capacity(args.len());
                for field in args {
                    let fid = variant_fields.field(&field.name).ok_or(LowerError::Failed)?;
                    let idx = u32::from(fid.into_raw()) as usize;
                    fields.push(self.lower(field.pat)?.at_index(idx));
                }
                self.variant_pat(pat, ty, variant, fields, arity)?
            }
            HirPat::Path(path) => match self.infer.variant_resolution_for_pat(pat) {
                Some(variant) => {
                    let arity = variant.fields(db).fields().len();
                    self.variant_pat(pat, ty, variant, Vec::new(), arity)? // alloc: empty.
                }
                None => {
                    let (konst, args) = self
                        .path_const(pat.into(), path, self.body.pat_path_hygiene(pat))
                        .ok_or(LowerError::Failed)?;
                    self.const_pat(pat, ty, konst, args)?
                }
            },
            HirPat::Lit(expr) => self.lit_expr_pat(pat, ty, expr)?,
            HirPat::Range { start, end, range_type } => {
                self.range_pat(pat, ty, start, end, range_type)?
            }
            HirPat::Slice { prefix, slice, suffix } => {
                let array_len = match ty.kind() {
                    TyKind::Array(_, len) => Some(
                        try_const_usize(db, len).ok_or(LowerError::Failed)? as usize,
                    ),
                    TyKind::Slice(_) => None,
                    _ => return Err(LowerError::Failed),
                };
                let kind = if slice.is_some() {
                    SliceKind::VarLen(prefix.len(), suffix.len())
                } else {
                    SliceKind::FixedLen(prefix.len() + suffix.len())
                };
                let mut lowered = Vec::with_capacity(prefix.len() + suffix.len());
                for &p in prefix.iter().chain(suffix) {
                    lowered.push(self.lower(p)?);
                }
                let fields = fields_of(lowered);
                Self::new_pat(Constructor::Slice(Slice::new(array_len, kind)), fields, kind.arity(), ty, pat)
            }
            HirPat::Ref { pat: inner, .. } => {
                // A `&` that edition 2024's match ergonomics ate against an inherited reference.
                if self.infer.is_skipped_ref_pat(pat) {
                    return self.lower(inner);
                }
                if !matches!(ty.kind(), TyKind::Ref(..)) {
                    return Err(LowerError::Failed);
                }
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![self.lower(inner)?.at_index(0)], 1, ty, pat)
            }
            HirPat::Box { inner } | HirPat::Deref { inner } => {
                self.cx.has_deref_pat.set(true);
                let inner_ty = self.infer.type_of_pat_with_adjust(inner);
                let field = self.lower(inner)?.at_index(0);
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::DerefPattern(inner_ty), vec![field], 1, ty, pat)
            }
            HirPat::Or(_) => {
                // alloc: only for an or-pattern, which becomes this many fields.
                let mut leaves = Vec::new();
                self.or_leaves(pat, &mut leaves);
                let mut lowered = Vec::with_capacity(leaves.len());
                for leaf in leaves {
                    lowered.push(self.lower(leaf)?);
                }
                let arity = lowered.len();
                Self::new_pat(Constructor::Or, fields_of(lowered), arity, ty, pat)
            }
        })
    }

    /// `expand_or_pat`: the alternatives of nested or-patterns, flattened.
    fn or_leaves(&self, pat: PatId, out: &mut Vec<PatId>) {
        match self.body.pat(pat) {
            HirPat::Or(alts) if self.infer.pat_adjustment(pat).is_none() => {
                for &alt in alts {
                    self.or_leaves(alt, out);
                }
            }
            _ => out.push(pat),
        }
    }

    /// `lower_tuple_subpats` with `enumerate_and_adjust`: the fields after `..` are counted
    /// from the end.
    fn lower_tuple_fields(
        &self,
        args: &[PatId],
        arity: usize,
        ellipsis: Option<u32>,
    ) -> Result<Vec<IndexedPat<Cx<'a, 'db>>>, LowerError> {
        if args.len() > arity {
            return Err(LowerError::Failed);
        }
        let gap = arity - args.len();
        let ellipsis = ellipsis.map_or(usize::MAX, |at| at as usize);
        let mut fields = Vec::with_capacity(args.len());
        for (i, &arg) in args.iter().enumerate() {
            let idx = if i < ellipsis { i } else { i + gap };
            fields.push(self.lower(arg)?.at_index(idx));
        }
        Ok(fields)
    }

    fn variant_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        variant: VariantId,
        fields: Vec<IndexedPat<Cx<'a, 'db>>>,
        arity: usize,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        let ctor = match (ty.kind(), variant) {
            (TyKind::Adt(..), VariantId::EnumVariantId(v)) => {
                Constructor::Variant(VariantIdx::of(self.cx.db, v))
            }
            (TyKind::Adt(def, _), VariantId::UnionId(_))
                if matches!(def.def_id(), AdtId::UnionId(_)) =>
            {
                Constructor::UnionField
            }
            (TyKind::Adt(..), VariantId::StructId(_)) => Constructor::Struct,
            _ => return Err(LowerError::Failed),
        };
        Ok(Self::new_pat(ctor, fields, arity, ty, pat))
    }

    /// The resolver at [`Self::scope`]. Made for each path to a constant, which few patterns
    /// have; inference keeps no resolution of a path to a free constant.
    fn resolver(&self) -> Resolver<'db> {
        let db = self.cx.db;
        let scope = ExprScopes::of(db, self.owner).scope_for(self.scope.get());
        resolver_for_scope(db, self.owner, scope)
    }

    /// What a path expression at [`Self::scope`] names in the value namespace.
    pub(crate) fn resolve_value(&self, path: &Path, hygiene: HygieneId) -> Option<ValueNs> {
        self.resolver().resolve_path_in_value_ns_fully(self.cx.db, path, hygiene)
    }

    /// The constant a path names, with the generic arguments inference gave it.
    pub(crate) fn path_const(
        &self,
        node: ExprOrPatId,
        path: &Path,
        hygiene: HygieneId,
    ) -> Option<(ConstId, GenericArgs<'db>)> {
        if let Some((CandidateId::ConstId(konst), args)) =
            self.infer.assoc_resolutions_for_expr_or_pat(node)
        {
            return Some((konst, args));
        }
        match self.resolver().resolve_path_in_value_ns_fully(self.cx.db, path, hygiene)? {
            ValueNs::ConstId(konst) => Some((konst, GenericArgs::empty())),
            _ => None,
        }
    }

    /// `is_const_pat_that_looks_like_binding`: a path pattern of one identifier that names a
    /// constant.
    pub(crate) fn const_that_looks_like_binding(&self, pat: PatId) -> Option<ConstId> {
        let HirPat::Path(path) = self.body.pat(pat) else { return None };
        path.mod_path()?.as_ident()?;
        if self.infer.variant_resolution_for_pat(pat).is_some() {
            return None;
        }
        self.path_const(pat.into(), path, self.body.pat_path_hygiene(pat)).map(|(konst, _)| konst)
    }

    fn eval(&self, konst: ConstId, args: GenericArgs<'db>) -> Result<Allocation<'db>, LowerError> {
        let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate };
        self.cx.db.const_eval(konst, args, Some(env)).map_err(|_| LowerError::Failed)
    }

    /// A path to a constant: its value, as `const_to_pat` makes a pattern of it.
    fn const_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        konst: ConstId,
        args: GenericArgs<'db>,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        let value = self.eval(konst, args)?;
        self.value_pat(pat, ty, &value.memory, &value.memory_map, 0)
    }

    fn layout_size(&self, ty: Ty<'db>) -> Result<usize, LowerError> {
        let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate }.store();
        let layout = self.cx.db.layout_of_ty(ty.store(), env).map_err(|_| LowerError::Failed)?;
        Ok(layout.size.bytes_usize())
    }

    /// `const_to_pat`'s `valtree_to_pat` over a value's bytes.
    fn value_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        bytes: &[u8],
        map: &MemoryMap<'db>,
        depth: u32,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        if depth > MAX_VALUE_DEPTH {
            return Err(LowerError::Failed);
        }
        let db = self.cx.db;
        let env = ParamEnvAndCrate { param_env: self.cx.env, krate: self.cx.krate }.store();
        let field = |offset: usize, size: usize| {
            bytes.get(offset..offset + size).ok_or(LowerError::Failed)
        };
        Ok(match ty.kind() {
            TyKind::Bool => Self::leaf(Constructor::Bool(bytes.first().is_some_and(|&b| b != 0)), ty, pat),
            TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) => {
                let info = self.cx.int_info(ty).ok_or(LowerError::Failed)?;
                let bits = truncate(read_uint(bytes).ok_or(LowerError::Failed)?, info.bits);
                let x = Cx::int_of_bits(info, bits);
                Self::leaf(Constructor::IntRange(IntRange::from_singleton(x)), ty, pat)
            }
            TyKind::Float(fty) => {
                let bits = read_uint(bytes).ok_or(LowerError::Failed)?;
                Self::leaf(float_ctor(fty, bits, bits, RangeEnd::Included), ty, pat)
            }
            TyKind::Tuple(tys) => {
                let layout = db.layout_of_ty(ty.store(), env).map_err(|_| LowerError::Failed)?;
                let mut lowered = Vec::with_capacity(tys.len());
                for (i, field_ty) in tys.iter().enumerate() {
                    let offset = layout.fields.offset(i).bytes_usize();
                    let size = self.layout_size(field_ty)?;
                    lowered.push(self.value_pat(pat, field_ty, field(offset, size)?, map, depth + 1)?);
                }
                let arity = lowered.len();
                Self::new_pat(Constructor::Struct, fields_of(lowered), arity, ty, pat)
            }
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::StructId(id) => {
                    let layout = db
                        .layout_of_adt(id.into(), args.store(), env)
                        .map_err(|_| LowerError::Failed)?;
                    // alloc: the fields `DeconstructedPat::new` takes, for a const's value only.
                    let mut lowered = Vec::new();
                    for (fid, field_ty) in self.cx.variant_fields(args, id.into()) {
                        let i = u32::from(fid.into_raw()) as usize;
                        let offset = layout.fields.offset(i).bytes_usize();
                        let size = self.layout_size(field_ty)?;
                        lowered.push(self.value_pat(pat, field_ty, field(offset, size)?, map, depth + 1)?);
                    }
                    let arity = lowered.len();
                    Self::new_pat(Constructor::Struct, fields_of(lowered), arity, ty, pat)
                }
                AdtId::EnumId(id) => {
                    let layout = db
                        .layout_of_adt(id.into(), args.store(), env)
                        .map_err(|_| LowerError::Failed)?;
                    let target = db.target_data_layout(self.cx.krate).map_err(|_| LowerError::Failed)?;
                    let (variant, variant_layout) =
                        detect_variant_from_bytes(&layout, db, target, bytes, id)
                            .ok_or(LowerError::Failed)?;
                    // alloc: the fields `DeconstructedPat::new` takes, for a const's value only.
                    let mut lowered = Vec::new();
                    for (fid, field_ty) in self.cx.variant_fields(args, variant.into()) {
                        let i = u32::from(fid.into_raw()) as usize;
                        let offset = variant_layout.fields.offset(i).bytes_usize();
                        let size = self.layout_size(field_ty)?;
                        lowered.push(self.value_pat(pat, field_ty, field(offset, size)?, map, depth + 1)?);
                    }
                    let arity = lowered.len();
                    let ctor = Constructor::Variant(VariantIdx::of(db, variant));
                    Self::new_pat(ctor, fields_of(lowered), arity, ty, pat)
                }
                // A union's value is opaque to a pattern.
                AdtId::UnionId(_) => Self::opaque(ty, pat),
            },
            TyKind::Array(elem, len) => {
                let len = try_const_usize(db, len).ok_or(LowerError::Failed)? as usize;
                let size = self.layout_size(elem)?;
                let mut lowered = Vec::with_capacity(len);
                for i in 0..len {
                    lowered.push(self.value_pat(pat, elem, field(i * size, size)?, map, depth + 1)?);
                }
                let kind = SliceKind::FixedLen(len);
                Self::new_pat(Constructor::Slice(Slice::new(Some(len), kind)), fields_of(lowered), len, ty, pat)
            }
            TyKind::Ref(_, pointee, _) => {
                let half = bytes.len() / 2;
                let inner = match pointee.kind() {
                    TyKind::Str => {
                        let addr = read_uint(&bytes[..half]).ok_or(LowerError::Failed)? as usize;
                        let len = read_uint(&bytes[half..]).ok_or(LowerError::Failed)? as usize;
                        let text = map.get(addr, len).ok_or(LowerError::Failed)?;
                        let text = std::str::from_utf8(text).map_err(|_| LowerError::Failed)?;
                        Self::leaf(Constructor::Str(Symbol::intern(text)), pointee, pat)
                    }
                    TyKind::Slice(elem) => {
                        let addr = read_uint(&bytes[..half]).ok_or(LowerError::Failed)? as usize;
                        let count = read_uint(&bytes[half..]).ok_or(LowerError::Failed)? as usize;
                        let size = self.layout_size(elem)?;
                        let data = map.get(addr, size * count).ok_or(LowerError::Failed)?;
                        let mut lowered = Vec::with_capacity(count);
                        for i in 0..count {
                            let elem_bytes = data.get(i * size..(i + 1) * size).ok_or(LowerError::Failed)?;
                            lowered.push(self.value_pat(pat, elem, elem_bytes, map, depth + 1)?);
                        }
                        let kind = SliceKind::FixedLen(count);
                        Self::new_pat(Constructor::Slice(Slice::new(None, kind)), fields_of(lowered), count, pointee, pat)
                    }
                    // A wide pointer to another unsized type is no structural value.
                    TyKind::Dynamic(..) | TyKind::Foreign(_) => return Ok(Self::opaque(ty, pat)),
                    _ if bytes.len() != (self.cx.pointer_bits / 8) as usize => {
                        return Ok(Self::opaque(ty, pat));
                    }
                    _ => {
                        let addr = read_uint(bytes).ok_or(LowerError::Failed)? as usize;
                        let size = self.layout_size(pointee)?;
                        let data = map.get(addr, size).ok_or(LowerError::Failed)?;
                        self.value_pat(pat, pointee, data, map, depth + 1)?
                    }
                };
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![inner.at_index(0)], 1, ty, pat)
            }
            // Raw and function pointers, and what else no pattern takes apart, match by value.
            _ => Self::opaque(ty, pat),
        })
    }

    /// A literal pattern, or `-` of one.
    fn lit_expr_pat(&self, pat: PatId, ty: Ty<'db>, expr: ExprId) -> Result<Pat<'a, 'db>, LowerError> {
        let (lit, negated) = self.literal_of(expr).ok_or(LowerError::Failed)?;
        self.lit_pat(pat, ty, lit, negated)
    }

    fn literal_of(&self, expr: ExprId) -> Option<(&'c Literal, bool)> {
        match self.body.expr(expr) {
            Expr::Literal(lit) => Some((lit, false)),
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => match self.body.expr(expr) {
                Expr::Literal(lit) => Some((lit, true)),
                _ => None,
            },
            _ => None,
        }
    }

    fn lit_pat(&self, pat: PatId, ty: Ty<'db>, lit: &Literal, negated: bool) -> Result<Pat<'a, 'db>, LowerError> {
        Ok(match (ty.kind(), lit) {
            (TyKind::Bool, Literal::Bool(b)) => Self::leaf(Constructor::Bool(*b), ty, pat),
            (TyKind::Char | TyKind::Int(_) | TyKind::Uint(_), _) => {
                let x = self.int_lit(ty, lit, negated).ok_or(LowerError::Failed)?;
                Self::leaf(Constructor::IntRange(IntRange::from_singleton(x)), ty, pat)
            }
            (TyKind::Float(fty), Literal::Float(..) | Literal::Int(..) | Literal::Uint(..)) => {
                let bits = float_lit_bits(fty, lit, negated).ok_or(LowerError::Failed)?;
                Self::leaf(float_ctor(fty, bits, bits, RangeEnd::Included), ty, pat)
            }
            // clone: a `Symbol`, which is interned.
            (TyKind::Str, Literal::String(s)) => Self::leaf(Constructor::Str(s.clone()), ty, pat),
            (TyKind::Ref(_, pointee, _), Literal::String(s)) if pointee.is_str() => {
                // clone: as above.
                let inner = Self::leaf(Constructor::Str(s.clone()), pointee, pat);
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![inner.at_index(0)], 1, ty, pat)
            }
            (TyKind::Ref(_, pointee, _), Literal::ByteString(bytes)) => {
                let inner = self.byte_string(pat, pointee, bytes)?;
                // alloc: the fields `DeconstructedPat::new` takes.
                Self::new_pat(Constructor::Ref, vec![inner.at_index(0)], 1, ty, pat)
            }
            (TyKind::Array(..) | TyKind::Slice(_), Literal::ByteString(bytes)) => {
                self.byte_string(pat, ty, bytes)?
            }
            _ => Self::opaque(ty, pat),
        })
    }

    /// `b"..."` against `[u8; N]` or `[u8]`: its bytes, one `u8` pattern each.
    fn byte_string(&self, pat: PatId, ty: Ty<'db>, bytes: &[u8]) -> Result<Pat<'a, 'db>, LowerError> {
        let (elem, array_len) = match ty.kind() {
            TyKind::Array(elem, _) => (elem, Some(bytes.len())),
            TyKind::Slice(elem) => (elem, None),
            _ => return Ok(Self::opaque(ty, pat)),
        };
        let lowered = bytes.iter().map(|&b| {
            let x = MaybeInfiniteInt::new_finite_uint(b as u128);
            Self::leaf(Constructor::IntRange(IntRange::from_singleton(x)), elem, pat)
        });
        let fields: Vec<_> = fields_of(lowered);
        let kind = SliceKind::FixedLen(bytes.len());
        Ok(Self::new_pat(Constructor::Slice(Slice::new(array_len, kind)), fields, bytes.len(), ty, pat))
    }

    fn int_lit(&self, ty: Ty<'db>, lit: &Literal, negated: bool) -> Option<MaybeInfiniteInt> {
        let info = self.cx.int_info(ty)?;
        let bits = match *lit {
            Literal::Int(v, _) => (if negated { v.wrapping_neg() } else { v }) as u128,
            Literal::Uint(v, _) => if negated { v.wrapping_neg() } else { v },
            Literal::Char(c) => c as u128,
            _ => return None,
        };
        Some(Cx::int_of_bits(info, truncate(bits, info.bits)))
    }

    /// The value of a range pattern's bound: a literal, `-` of one, or a constant.
    fn range_bound(&self, expr: ExprId, ty: Ty<'db>) -> Result<RangeBound, LowerError> {
        if let Some((lit, negated)) = self.literal_of(expr) {
            return match ty.kind() {
                TyKind::Float(fty) => {
                    float_lit_bits(fty, lit, negated).map(RangeBound::Float).ok_or(LowerError::Failed)
                }
                _ => self.int_lit(ty, lit, negated).map(RangeBound::Int).ok_or(LowerError::Failed),
            };
        }
        let Expr::Path(path) = self.body.expr(expr) else { return Err(LowerError::Failed) };
        let (konst, args) = self
            .path_const(expr.into(), path, self.body.expr_path_hygiene(expr))
            .ok_or(LowerError::Failed)?;
        let value = self.eval(konst, args)?;
        let bits = read_uint(&value.memory).ok_or(LowerError::Failed)?;
        match ty.kind() {
            TyKind::Float(_) => Ok(RangeBound::Float(bits)),
            _ => {
                let info = self.cx.int_info(ty).ok_or(LowerError::Failed)?;
                Ok(RangeBound::Int(Cx::int_of_bits(info, truncate(bits, info.bits))))
            }
        }
    }

    /// `lower_pattern_range`, with its E0030 and E0579.
    fn range_pat(
        &self,
        pat: PatId,
        ty: Ty<'db>,
        start: Option<ExprId>,
        end: Option<ExprId>,
        range_type: RangeOp,
    ) -> Result<Pat<'a, 'db>, LowerError> {
        let lo = start.map(|e| self.range_bound(e, ty)).transpose()?;
        let hi = end.map(|e| self.range_bound(e, ty)).transpose()?;
        // `lo..` has no end to leave out: it is `lo..=MAX`.
        let range_end = match (range_type, hi) {
            (RangeOp::Exclusive, Some(_)) => RangeEnd::Excluded,
            _ => RangeEnd::Included,
        };
        let inclusive = range_end == RangeEnd::Included;
        let empty = LowerError::EmptyRange { pat, inclusive };
        let ctor = match ty.kind() {
            TyKind::Float(fty) => {
                let (lo, hi) = (
                    lo.map(|b| match b { RangeBound::Float(bits) => bits, RangeBound::Int(_) => 0 }),
                    hi.map(|b| match b { RangeBound::Float(bits) => bits, RangeBound::Int(_) => 0 }),
                );
                let ctor = float_range_ctor(fty, lo, hi, range_end);
                if !float_range_is_valid(&ctor) {
                    return Err(empty);
                }
                ctor
            }
            TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) => {
                let int = |b: RangeBound| match b {
                    RangeBound::Int(x) => Ok(x),
                    RangeBound::Float(_) => Err(LowerError::Failed),
                };
                let lo = lo.map(int).transpose()?.unwrap_or(MaybeInfiniteInt::NegInfinity);
                let hi = hi.map(int).transpose()?.unwrap_or(MaybeInfiniteInt::PosInfinity);
                let valid = match range_end {
                    RangeEnd::Included => lo <= hi,
                    RangeEnd::Excluded => lo < hi,
                };
                if !valid {
                    return Err(empty);
                }
                Constructor::IntRange(IntRange::from_range(lo, hi, range_end))
            }
            _ => return Err(LowerError::Failed),
        };
        Ok(Self::leaf(ctor, ty, pat))
    }
}

/// A float literal's bits, read as rustc reads one: by `rustc_apfloat` from its text.
fn float_lit_bits(fty: FloatTy, lit: &Literal, negated: bool) -> Option<u128> {
    // alloc: `rustc_apfloat` parses text; only for a float pattern.
    let text = match lit {
        Literal::Float(value, _) => value.to_string(), // alloc: as above.
        Literal::Int(v, _) => v.to_string(), // alloc: as above.
        Literal::Uint(v, _) => v.to_string(), // alloc: as above.
        _ => return None,
    };
    let text = text.as_str();
    fn parse<F: Float + std::str::FromStr>(text: &str, negated: bool) -> Option<u128> {
        let value: F = text.parse().ok()?;
        Some(if negated { (-value).to_bits() } else { value.to_bits() })
    }
    match fty {
        FloatTy::F16 => parse::<Half>(text, negated),
        FloatTy::F32 => parse::<Single>(text, negated),
        FloatTy::F64 => parse::<Double>(text, negated),
        FloatTy::F128 => parse::<Quad>(text, negated),
    }
}

fn float_ctor<'a, 'db>(fty: FloatTy, lo: u128, hi: u128, end: RangeEnd) -> Constructor<Cx<'a, 'db>> {
    float_range_ctor(fty, Some(lo), Some(hi), end)
}

fn float_range_ctor<'a, 'db>(
    fty: FloatTy,
    lo: Option<u128>,
    hi: Option<u128>,
    end: RangeEnd,
) -> Constructor<Cx<'a, 'db>> {
    match fty {
        FloatTy::F16 => Constructor::F16Range(
            lo.map_or(-Half::INFINITY, Half::from_bits),
            hi.map_or(Half::INFINITY, Half::from_bits),
            end,
        ),
        FloatTy::F32 => Constructor::F32Range(
            lo.map_or(-Single::INFINITY, Single::from_bits),
            hi.map_or(Single::INFINITY, Single::from_bits),
            end,
        ),
        FloatTy::F64 => Constructor::F64Range(
            lo.map_or(-Double::INFINITY, Double::from_bits),
            hi.map_or(Double::INFINITY, Double::from_bits),
            end,
        ),
        FloatTy::F128 => Constructor::F128Range(
            lo.map_or(-Quad::INFINITY, Quad::from_bits),
            hi.map_or(Quad::INFINITY, Quad::from_bits),
            end,
        ),
    }
}

/// rustc's check of a float range: `lo <= hi` for `..=`, `lo < hi` for `..`, neither a NaN.
fn float_range_is_valid(ctor: &Constructor<Cx<'_, '_>>) -> bool {
    fn valid<F: Float>(lo: &F, hi: &F, end: RangeEnd) -> bool {
        match (lo.partial_cmp(hi), end) {
            (Some(std::cmp::Ordering::Less), _) => true,
            (Some(std::cmp::Ordering::Equal), RangeEnd::Included) => true,
            _ => false,
        }
    }
    match ctor {
        Constructor::F16Range(lo, hi, end) => valid(lo, hi, *end),
        Constructor::F32Range(lo, hi, end) => valid(lo, hi, *end),
        Constructor::F64Range(lo, hi, end) => valid(lo, hi, *end),
        Constructor::F128Range(lo, hi, end) => valid(lo, hi, *end),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_the_type_s_bits() {
        assert_eq!(truncate(u128::MAX, 8), 0xff);
        assert_eq!(truncate((-1i128) as u128, 32), 0xffff_ffff);
        assert_eq!(truncate(5, 128), 5);
    }

    #[test]
    fn values_are_read_little_endian() {
        assert_eq!(read_uint(&[1, 2]), Some(0x0201));
        assert_eq!(read_uint(&[0; 17]), None);
    }

    #[test]
    fn float_literals_are_read_by_apfloat() {
        let one = Literal::Int(1, None);
        assert_eq!(float_lit_bits(FloatTy::F32, &one, false), Some(1.0f32.to_bits() as u128));
        assert_eq!(float_lit_bits(FloatTy::F64, &one, true), Some((-1.0f64).to_bits() as u128));
    }
}
