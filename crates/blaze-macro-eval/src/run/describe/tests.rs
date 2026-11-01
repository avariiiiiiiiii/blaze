use hir_def::{DefWithBodyId, ExpressionStoreOwnerId, expr_store::ExpressionStore, hir::Expr, test_items};
use hir_expand::fixture;
use hir_ty::{
    InferBodyId,
    db::{HirDatabase as _, InternedClosure, InternedClosureId},
    mir::MirEvalError,
};

use super::describe;

/// A closure whose call has no function stops its lowering with `MissingFunctionDefinition`,
/// as the bridge's `run_client` closure did under serde_derive: the stop names the closure's
/// function and line, the callee as written, and why inference resolved it to nothing.
#[test]
fn a_call_with_no_function_names_its_callee_its_function_and_why() {
    let db = fixture::db("pub fn outer() -> i32 {\n    let f = || missing(1);\n    f()\n}\n").into_db();
    let krate = fixture::test_crate(&db);
    let text = hir_ty::attach_db(&db, || {
        let outer = test_items::function(&db, krate, "outer");
        let owner: InferBodyId<'_> = outer.into();
        let store = ExpressionStore::of(&db, ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(outer)));
        let (expr, kind) = store
            .exprs()
            .find_map(|(id, it)| match it {
                Expr::Closure { closure_kind, .. } => Some((id, closure_kind)),
                _ => None,
            })
            .expect("the fixture's closure");
        let closure = InternedClosureId::new(&db, InternedClosure { owner, expr, kind });
        let lower = match db.mir_body_for_closure(closure) {
            Ok(_) => panic!("the closure's call of `missing` has no function, yet its MIR was built"),
            Err(it) => it,
        };
        describe(&db, &MirEvalError::MirLowerErrorForClosure(closure, lower))
    });
    assert!(text.starts_with("stopped in the evaluator: the MIR of the closure at "), "{text}");
    assert!(text.contains("::outer`"), "the closure's function is named: {text}");
    assert!(text.contains("the call of `missing` at "), "the callee is named as written: {text}");
    assert!(text.contains(":2:"), "the places are the closure's line: {text}");
    assert!(text.contains("resolves to nothing"), "inference's reason is given: {text}");
}

/// The frames of a stop are named innermost first by their paths, each with where it was
/// called from.
#[test]
fn the_frames_are_named_by_path_innermost_first() {
    let db = fixture::db("pub fn inner() {}\npub fn outer() {\n    inner();\n}\n").into_db();
    let krate = fixture::test_crate(&db);
    let text = hir_ty::attach_db(&db, || {
        let (inner, outer) = (test_items::function(&db, krate, "inner"), test_items::function(&db, krate, "outer"));
        let store = ExpressionStore::of(&db, ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(outer)));
        let call = store
            .exprs()
            .find_map(|(id, it)| matches!(it, Expr::Call { .. }).then_some(id))
            .expect("the fixture's call");
        let caller: InferBodyId<'_> = outer.into();
        let stack = vec![
            (either::Either::Left(inner), hir_ty::mir::MirSpan::ExprId(call), caller),
            (either::Either::Left(outer), hir_ty::mir::MirSpan::Unknown, caller),
        ];
        describe(&db, &MirEvalError::InFunction(Box::new(MirEvalError::StackOverflow), stack))
    });
    let (inner, outer) = (text.find("in `").expect("a frame"), text.rfind("in `").expect("a frame"));
    assert!(text[inner..].starts_with("in `") && text[inner..].contains("::inner`, called from `"), "{text}");
    assert!(inner < outer && text[outer..].contains("::outer`"), "innermost first: {text}");
    assert!(text.contains(":3:"), "the call's line: {text}");
}
