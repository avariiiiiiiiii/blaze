//! An item a body names, as the identity hashes it: its key (which item) and its signature
//! hash (`blaze_items::signature_hash`): its interface without what bodies leak into it.
//!
//! The identity is computed inside `infer_body_query`, so nothing it hashes may depend on the
//! inference of a body: an interface hash includes what bodies leak (an `async fn`'s or an
//! `impl Trait` return's hidden type, a `const fn`'s, a const's and a static's body), which
//! could reach back to the body being inferred, a salsa cycle. A signature hash leaves each
//! leak out; what a body reads of one (a hidden type, a constant's value) is a fact of its
//! trace instead (section 5). An item of any crate is hashed alike.

use base_db::Db;
use blaze_items::{ItemId, key};
use hir_def::{EnumVariantId, Lookup};

/// An item's key and signature, back to back.
pub(crate) type Part = [u8; 64];

pub(crate) fn item_part(db: &Db, item: ItemId) -> Result<Part, &'static str> {
    Ok(join(&key::item_key(db, item).0.0, &signature(db, item)?))
}

/// A variant: its key and its enum's signature.
pub(crate) fn variant_part(db: &Db, variant: EnumVariantId) -> Result<Part, &'static str> {
    let enum_ = ItemId::Adt(variant.lookup(db).parent.into());
    Ok(join(&key::variant_key(db, variant).0.0, &signature(db, enum_)?))
}

fn signature(db: &Db, item: ItemId) -> Result<[u8; 32], &'static str> {
    blaze_items::signature_hash(db, item).map(|it| it.0.0).map_err(|it| it.what)
}

fn join(key: &[u8; 32], signature: &[u8; 32]) -> Part {
    let mut part = [0; 64];
    part[..32].copy_from_slice(key);
    part[32..].copy_from_slice(signature);
    part
}
