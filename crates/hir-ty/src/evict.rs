//! Dropping the values of per-body results once what was made of them is stored (#806, #734).
//!
//! A dropped value keeps its memo: its fingerprint and the edges it read. Verifying it walks those
//! edges without the value, so a dependent of an unchanged body stays valid, and a read of it
//! computes it again and backdates by fingerprint, so a later edit cuts off early as if the value
//! had been kept (salsa's `evict_value`, docs/v2/db.md section 7).

use hir_def::DefWithBodyId;

use crate::{
    db::HirDatabase,
    infer::{infer_anon_const_body_query, infer_body_query},
};

/// Drops the inference results of `bodies`. What `bodies` names that was never inferred is left
/// as it is.
pub fn drop_inference(db: &mut dyn HirDatabase, bodies: &[DefWithBodyId]) {
    for &body in bodies {
        infer_body_query::evict_value(db, body);
    }
}

/// Drops the inference results of every body, and of every constant of a signature and of a
/// body's array length or repeat count.
pub fn drop_all_inference(db: &mut dyn HirDatabase) {
    infer_body_query::evict_values(db);
    infer_anon_const_body_query::evict_values(db);
}
