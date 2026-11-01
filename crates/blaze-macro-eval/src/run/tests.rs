use hir_def::{AdtId, ModuleDefId, test_items};
use hir_expand::fixture;
use hir_ty::next_solver::{DbInterner, GenericArgs, Ty};
use hir_ty_mir::mir::MirEvalError;

use super::stop_of;

/// An evaluator error that holds a type, made as a failing expansion makes one, is rendered
/// by the run with its type named, though nothing has attached the database: the daemon's
/// loop runs `run` outside any attach.
#[test]
fn an_evaluator_error_names_its_type_outside_an_attach() {
    let db = fixture::db("pub struct Wrapper { pub inner: [u8] }").into_db();
    let krate = fixture::test_crate(&db);
    let error = hir_ty::attach_db(&db, || {
        let adt = test_items::declarations(&db, krate)
            .into_iter()
            .find_map(|it| match it {
                ModuleDefId::AdtId(adt @ AdtId::StructId(_)) => Some(adt),
                _ => None,
            })
            .expect("the fixture's struct");
        let ty = Ty::new_adt(DbInterner::new_with(&db, krate), adt, GenericArgs::empty());
        MirEvalError::TypeIsUnsized(ty.store(), "a test")
    });
    let stop = stop_of(&db, error);
    assert!(stop.0.starts_with("stopped in the evaluator: TypeIsUnsized"), "{stop}");
    assert!(stop.0.contains("Wrapper"), "the type is named: {stop}");
}
