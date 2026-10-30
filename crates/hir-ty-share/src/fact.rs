//! The fact of a type: what code that moves, copies, projects or passes a value of it reads,
//! where that code does not tell types apart. Two types with one fact are handled by the same
//! machine code wherever only their facts are read.
//!
//! - A scalar (`bool`, `char`, an integer, a float), `str` and `!` are their identity.
//! - An ADT is its definition's identity ([`identity::adt`]: its key, interface, layout hash
//!   and drop impls, so `repr`, `#[repr(transparent)]`, lang-item ADTs and `Drop` are in it),
//!   its layout, its discriminants and its fields' facts, with the arguments put in. Two
//!   instances of one ADT whose fields have one fact (`Vec<&A>`, `Vec<&B>`) have one fact.
//! - A tuple, an array and a closure are their layouts and their parts' facts.
//! - A pointer is its mutability and a summary of what it points to: its kind, size and
//!   alignment, whether it is `Freeze`, `Unpin` and plain data, which the code generator reads
//!   to say what memory a reference argument is the only way to. Never the pointee's fact: a
//!   type reaches itself only through a pointer, so a fact never cycles.
//! - A function item is a value of no size: what calls it, casts it to a pointer or makes a
//!   vtable of it keys it there ([`crate::walk`]).
//! - Everything else (a function pointer, a trait object, a coroutine) is its identity: the
//!   code that handles it tells it apart.
//!
//! Each fact ends with whether the type needs drop and whether it is `Freeze`.

use blaze_ids::ContentId;
use hir_def::VariantId;
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    drop::{DropGlue, has_drop_glue, is_freeze},
    layout::Layout,
    next_solver::{
        DbInterner, StoredTy, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::{StoredParamEnvAndCrate, implements_trait_unique, structurally_normalize_ty},
};
use rustc_type_ir::inherent::{AdtDef as _, GenericArgs as _, IntoKind};
use triomphe::Arc;

use crate::{FACT_DOMAIN, Unshared, identity, layout};

/// What a fact or a pointee summary is of: its first byte.
mod tag {
    pub(super) const LEAF: u8 = 1;
    pub(super) const ADT: u8 = 2;
    pub(super) const TUPLE: u8 = 3;
    pub(super) const ARRAY: u8 = 4;
    pub(super) const SLICE: u8 = 5;
    pub(super) const REF: u8 = 6;
    pub(super) const RAW: u8 = 7;
    pub(super) const CLOSURE: u8 = 8;
    pub(super) const IDENTITY: u8 = 9;
    pub(super) const FN_ITEM: u8 = 10;
    pub(super) const POINTEE_SIZED: u8 = 16;
    pub(super) const POINTEE_STR: u8 = 17;
    pub(super) const POINTEE_SLICE: u8 = 18;
    pub(super) const POINTEE_OPAQUE: u8 = 19;
}

/// How deep [`Cx::is_plain_data`] looks, as the code generator's `is_plain_data` does.
const PLAIN_DATA_DEPTH: u32 = 8;

/// What the queries of one environment share: the database, an inference context in it and
/// the environment, borrowed and stored.
pub(crate) struct Cx<'db> {
    pub(crate) db: &'db dyn HirDatabase,
    pub(crate) interner: DbInterner<'db>,
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnvAndCrate<'db>,
    pub(crate) stored: StoredParamEnvAndCrate,
}

impl<'db> Cx<'db> {
    pub(crate) fn new(db: &'db dyn HirDatabase, stored: &StoredParamEnvAndCrate) -> Cx<'db> {
        let env = stored.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        Cx { db, interner, infcx, env, stored: stored.clone() }
    }

    /// `ty` with a top-level alias normalized, as the code generator's `normalize` does.
    pub(crate) fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        structurally_normalize_ty(&self.infcx, ty, self.env.param_env)
    }

    pub(crate) fn layout(&self, ty: Ty<'db>) -> Result<Arc<Layout>, Unshared> {
        self.db
            .layout_of_ty(ty.store(), self.stored.clone())
            .map_err(|error| Unshared::NoLayout(format!("`{ty:?}`: {error:?}")))
    }

    pub(crate) fn needs_drop(&self, ty: Ty<'db>) -> bool {
        has_drop_glue(&self.infcx, ty, self.env.param_env) != DropGlue::None
    }

    /// The fact of `ty`: one query per (type, environment), so a hit is one lookup.
    pub(crate) fn fact(&self, ty: Ty<'db>) -> Result<ContentId, Unshared> {
        let ty = self.normalize(ty);
        fact(self.db, ty.store(), self.stored.clone()).clone()
    }

    /// The code generator's `is_plain_data`: made of scalars, pointers and references only,
    /// through arrays, tuples and the fields of structs and enums.
    fn is_plain_data(&self, ty: Ty<'db>, depth: u32) -> bool {
        if depth > PLAIN_DATA_DEPTH {
            return false;
        }
        match self.normalize(ty).kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnPtr(..)
            | TyKind::FnDef(..) => true,
            TyKind::Array(element, _) | TyKind::Pat(element, _) => {
                self.is_plain_data(element, depth + 1)
            }
            TyKind::Tuple(fields) => fields.iter().all(|it| self.is_plain_data(it, depth + 1)),
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if let hir_def::AdtId::UnionId(_) = adt {
                    return false;
                }
                identity::variants(self.db, adt).into_iter().all(|variant| {
                    self.db.field_types(variant).iter().all(|(_, field)| {
                        let ty = field.ty().instantiate(self.interner, args).skip_norm_wip();
                        self.is_plain_data(ty, depth + 1)
                    })
                })
            }
            _ => false,
        }
    }

    fn is_unpin(&self, ty: Ty<'db>) -> bool {
        self.interner
            .lang_items()
            .Unpin
            .is_some_and(|unpin| implements_trait_unique(ty, self.db, self.env, unpin))
    }

    /// What a pointer to `pointee` says of its memory.
    fn pointee(&self, pointee: Ty<'db>, out: &mut Vec<u8>) -> Result<(), Unshared> {
        let pointee = self.normalize(pointee);
        let (size, align) = match pointee.kind() {
            TyKind::Str => {
                out.push(tag::POINTEE_STR);
                (0, 1)
            }
            TyKind::Slice(element) => {
                out.push(tag::POINTEE_SLICE);
                let layout = self.layout(self.normalize(element))?;
                out.extend_from_slice(&layout.size.bytes().to_le_bytes());
                (0, layout.align.abi.bytes())
            }
            TyKind::Dynamic(..) | TyKind::Foreign(_) => {
                out.push(tag::POINTEE_OPAQUE);
                out.extend_from_slice(&identity::ty(self.db, pointee)?.0);
                (0, 1)
            }
            _ => {
                let layout = self.layout(pointee)?;
                if layout.is_unsized() {
                    // A struct with an unsized tail: its metadata is the tail's.
                    out.push(tag::POINTEE_OPAQUE);
                    out.extend_from_slice(&identity::ty(self.db, pointee)?.0);
                } else {
                    out.push(tag::POINTEE_SIZED);
                    out.push(u8::from(layout.uninhabited));
                }
                (layout.size.bytes(), layout.align.abi.bytes())
            }
        };
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&align.to_le_bytes());
        let element = match pointee.kind() {
            TyKind::Str => None,
            TyKind::Slice(element) => Some(element),
            _ => Some(pointee),
        };
        let plain = element.is_none_or(|it| self.is_plain_data(it, 0));
        let freeze = is_freeze(&self.infcx, pointee, self.env.param_env);
        let unpin = self.is_unpin(pointee);
        out.push(u8::from(plain) | u8::from(freeze) << 1 | u8::from(unpin) << 2);
        Ok(())
    }

    fn write(&self, ty: Ty<'db>, out: &mut Vec<u8>) -> Result<(), Unshared> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Never => {
                out.push(tag::LEAF);
                out.extend_from_slice(&identity::ty(db, ty)?.0);
            }
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                out.push(tag::ADT);
                out.extend_from_slice(&identity::adt(db, adt).clone()?.0);
                layout::write(&self.layout(ty)?, out);
                for variant in identity::variants(db, adt) {
                    if let VariantId::EnumVariantId(variant) = variant {
                        let discriminant = db.const_eval_discriminant(variant).map_err(|error| {
                            Unshared::ConstEval(format!("a discriminant of `{ty:?}`: {error:?}"))
                        })?;
                        out.extend_from_slice(&discriminant.to_le_bytes());
                    }
                    for (_, field) in db.field_types(variant).iter() {
                        let field = field.ty().instantiate(self.interner, args).skip_norm_wip();
                        out.extend_from_slice(&self.fact(field)?.0);
                    }
                }
            }
            TyKind::Tuple(fields) => {
                out.push(tag::TUPLE);
                layout::write(&self.layout(ty)?, out);
                for field in fields.iter() {
                    out.extend_from_slice(&self.fact(field)?.0);
                }
            }
            TyKind::Array(element, len) => {
                out.push(tag::ARRAY);
                layout::write(&self.layout(ty)?, out);
                out.extend_from_slice(&self.fact(element)?.0);
                out.extend_from_slice(&identity::konst(db, len)?.0);
            }
            TyKind::Slice(element) => {
                out.push(tag::SLICE);
                out.extend_from_slice(&self.fact(element)?.0);
            }
            TyKind::Ref(_, pointee, mutability) => {
                out.push(tag::REF);
                out.push(u8::from(mutability.is_mut()));
                self.pointee(pointee, out)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                out.push(tag::RAW);
                out.push(u8::from(mutability.is_mut()));
                self.pointee(pointee, out)?;
            }
            TyKind::Closure(_, args) => {
                out.push(tag::CLOSURE);
                layout::write(&self.layout(ty)?, out);
                out.extend_from_slice(&self.fact(args.as_closure().tupled_upvars_ty())?.0);
            }
            // A function item is a value of no size; a call of it, a cast of it to a pointer
            // and a vtable of it key it where they are.
            TyKind::FnDef(..) => out.push(tag::FN_ITEM),
            // A parameter, an alias that did not normalize or an error has no identity, which
            // says so.
            _ => {
                out.push(tag::IDENTITY);
                out.extend_from_slice(&identity::ty(db, ty)?.0);
            }
        }
        let freeze = is_freeze(&self.infcx, ty, self.env.param_env);
        out.push(u8::from(self.needs_drop(ty)) | u8::from(freeze) << 1);
        Ok(())
    }
}

#[salsa::tracked(returns(ref), cycle_result = fact_cycle)]
fn fact(
    db: &dyn HirDatabase,
    ty: StoredTy,
    env: StoredParamEnvAndCrate,
) -> Result<ContentId, Unshared> {
    let cx = Cx::new(db, &env);
    let mut out = Vec::with_capacity(256);
    cx.write(ty.as_ref(), &mut out)?;
    Ok(ContentId::of(FACT_DOMAIN, &out))
}

fn fact_cycle(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _: StoredTy,
    _: StoredParamEnvAndCrate,
) -> Result<ContentId, Unshared> {
    Err(Unshared::Recursive)
}
