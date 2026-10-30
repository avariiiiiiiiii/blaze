//! Allocation budgets of the store's hot paths: reading a store, a memo hit of a body or its
//! scopes, and a scope lookup allocate nothing.

use std::hint::black_box;

use blaze_alloc_count::assert_at_most;
use test_fixture::WithFixture;

use crate::{
    DefWithBodyId, ModuleDefId,
    expr_store::{Body, scope::ExprScopes},
    nameres::crate_def_map,
    test_db::TestDB,
};

const CODE: &str = r#"
struct S { a: u32 }

fn f(s: S, x: u32) -> u32 {
    let (y, z) = (x + 1, [x; 2]);
    let c = |w: u32| w * s.a;
    match y {
        0 => z[0],
        n if n > 3 => c(n),
        _ => {
            let t: &u32 = &y;
            *t
        }
    }
}
"#;

fn the_function(db: &TestDB) -> DefWithBodyId {
    for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
        for decl in module.scope(db).declarations() {
            if let ModuleDefId::FunctionId(it) = decl {
                return it.into();
            }
        }
    }
    panic!("no function in the fixture")
}

#[test]
fn reading_a_store_allocates_nothing() {
    let (db, _) = TestDB::with_single_file(CODE);
    let body = Body::of(&db, the_function(&db));
    assert!(body.expr_ids().len() > 10 && body.pat_ids().len() > 5, "too small to measure");
    assert_at_most(0, "reading every node of a store", || {
        for expr in body.expr_ids() {
            black_box(body.expr(expr));
        }
        for pat in body.pat_ids() {
            black_box(body.pat(pat));
        }
        for (id, type_ref) in body.type_refs() {
            black_box((id, type_ref));
        }
    });
}

#[test]
fn a_memo_hit_allocates_nothing() {
    let (db, _) = TestDB::with_single_file(CODE);
    let def = the_function(&db);
    Body::of(&db, def);
    ExprScopes::of(&db, def);
    assert_at_most(0, "a memo hit of `Body::of`", || black_box(Body::of(&db, def)));
    assert_at_most(0, "a memo hit of `ExprScopes::of`", || black_box(ExprScopes::of(&db, def)));
}

#[test]
fn a_scope_lookup_allocates_nothing() {
    let (db, _) = TestDB::with_single_file(CODE);
    let def = the_function(&db);
    let body = Body::of(&db, def);
    let scopes = ExprScopes::of(&db, def);
    let named = body
        .expr_ids()
        .filter_map(|expr| scopes.scope_for(expr))
        .find_map(|scope| scopes.entries(scope).first())
        .expect("a scope with a binding");
    let name = named.name().clone();
    assert_at_most(0, "resolving a name from every expression's scope", || {
        for expr in body.expr_ids() {
            if let Some(scope) = scopes.scope_for(expr) {
                black_box(scopes.resolve_name_in_scope(scope, &name));
                black_box(scopes.scope_chain(Some(scope)).count());
            }
        }
    });
}
