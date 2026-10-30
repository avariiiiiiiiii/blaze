//! What an edit of a file makes lowered again. A body is lowered from its own text
//! ([`crate::expr_store::body::BodySyntax`]), so an edit that changes nothing a body is made of
//! leaves its store as it was, and an edit of one body leaves every other body and every
//! signature as they were, not lowered again.

use base_db::Db;
use test_fixture::WithFixture;
use triomphe::Arc;

use crate::{
    AssocItemId, DefWithBodyId, GenericDefId, ModuleDefId,
    expr_store::{Body, scope::ExprScopes},
    hir::generics::GenericParams,
    nameres::{assoc::ImplItems, crate_def_map},
    signatures::FunctionSignature,
    test_db::{TestDB, TestDBExt},
};

const CODE: &str = r#"
pub struct S { a: u32 }

impl S {
    pub fn get(&self) -> u32 { self.a }
    pub fn add(&self, b: u32) -> u32 {
        let c = self.get() + b;
        c * 2
    }
}

pub fn first(x: u32) -> u32 {
    let y = x + 1;
    if y > 2 { y } else { 0 }
}

pub fn second(x: u32) -> u32 {
    match x {
        0 => first(x),
        n => n - 1,
    }
}

pub const K: u32 = 3;
pub static L: u32 = K + 1;
"#;

/// What reads a signature's store: whether it ran again is whether the store changed.
#[salsa::tracked(returns(clone))]
fn signature_store_bytes(db: &Db, def: GenericDefId) -> Vec<u8> {
    let (_, store) = GenericParams::with_store(db, def);
    blaze_format::to_bytes(store)
}

/// Every body and every signature of the test crate, in the order the crate declares them.
fn defs(db: &TestDB) -> (Vec<DefWithBodyId>, Vec<GenericDefId>) {
    let (mut bodies, mut signatures) = (Vec::new(), Vec::new());
    for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
        let scope = module.scope(db);
        for decl in scope.declarations() {
            match decl {
                ModuleDefId::FunctionId(it) => {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
                ModuleDefId::ConstId(it) => {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
                ModuleDefId::StaticId(it) => {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
                ModuleDefId::AdtId(it) => signatures.push(it.into()),
                _ => {}
            }
        }
        for imp in scope.impls() {
            signatures.push(imp.into());
            for &(_, item) in ImplItems::of(db, imp).0.items.iter() {
                if let AssocItemId::FunctionId(it) = item {
                    bodies.push(it.into());
                    signatures.push(it.into());
                }
            }
        }
    }
    (bodies, signatures)
}

fn function_named(db: &TestDB, name: &str) -> DefWithBodyId {
    let (bodies, _) = defs(db);
    let named = |def: &DefWithBodyId| match *def {
        DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).name.as_str() == name,
        _ => false,
    };
    *bodies.iter().find(|it| named(it)).unwrap_or_else(|| panic!("no fn `{name}`"))
}

/// The body of `def` lowered from its own text, as the query keeps it.
fn relative_body(db: &TestDB, def: DefWithBodyId) -> Arc<Body> {
    Body::relative(db, def).as_ref().expect("a body written in a file").0.clone()
}

fn count(log: &[String], query: &str) -> usize {
    log.iter().filter(|it| *it == query).count()
}

/// (a) `edited` changes only whitespace or comments in `first`'s body: the store stays equal,
/// and so do its bytes; the source map, which says where each part is, moves.
#[track_caller]
fn check_store_unchanged_by(edited: &str) {
    assert_ne!(edited, CODE);
    let (mut db, file) = TestDB::with_single_file(CODE);
    let def = function_named(&db, "first");
    let before = relative_body(&db, def);
    let bytes = blaze_format::to_bytes(&*before);
    let map = blaze_format::to_bytes(&Body::with_source_map(&db, def).1);

    db.set_file_text(file.file_id(&db), edited);
    let after = Body::of(&db, def);
    assert_eq!(*before, *after, "the store changed for:\n{edited}");
    assert_eq!(bytes, blaze_format::to_bytes(after), "the store's bytes changed for:\n{edited}");
    let moved = blaze_format::to_bytes(&Body::with_source_map(&db, def).1);
    assert_ne!(map, moved, "the source map did not move for:\n{edited}");
}

#[test]
fn whitespace_in_a_body_keeps_its_store() {
    check_store_unchanged_by(&CODE.replace("let y = x + 1;", "let   y =\n        x  +  1 ;"));
}

#[test]
fn comments_in_a_body_keep_its_store() {
    check_store_unchanged_by(&CODE.replace("let y = x + 1;", "let y = x /* one */ + 1; // two"));
}

/// (b) An edit of `second`'s body lowers that body again and nothing else: every other body's
/// store is the one kept from before (not lowered again), and every signature's store is equal,
/// so nothing that reads one runs again. Nothing here reads a source map, so `Body::relative`
/// doesn't run at all.
#[test]
fn an_edit_in_one_body_lowers_only_that_body() {
    let (mut db, file) = TestDB::with_single_file(CODE);
    let (bodies, signatures) = defs(&db);
    assert_eq!((bodies.len(), signatures.len()), (6, 8), "{bodies:?}\n{signatures:?}");
    let edited = function_named(&db, "second");
    // `Body::of` lends its body only until the next edit: keep where it was and its bytes.
    let before: Vec<(*const Body, Vec<u8>)> = bodies
        .iter()
        .map(|&def| {
            let body = Body::of(&db, def);
            (std::ptr::from_ref(body), blaze_format::to_bytes(body))
        })
        .collect();
    for &def in &bodies {
        ExprScopes::body_expr_scopes(&db, def);
    }
    let signatures_before: Vec<Vec<u8>> =
        signatures.iter().map(|&def| signature_store_bytes(&db, def)).collect();

    db.set_file_text(file.file_id(&db), &CODE.replace("n => n - 1,", "n => n - 2,"));
    let log = db.executed_in_read_order(|| {
        for &def in &bodies {
            ExprScopes::body_expr_scopes(&db, def);
        }
        for &def in &signatures {
            signature_store_bytes(&db, def);
        }
    });
    for query in ["Body::of_", "ExprScopes::body_expr_scopes_"] {
        assert_eq!(count(&log, query), 1, "`{query}` ran for more than the edited body: {log:#?}");
    }
    assert_eq!(count(&log, "Body::relative_"), 0, "a source map was lowered: {log:#?}");
    assert_eq!(count(&log, "signature_store_bytes"), 0, "a signature's store changed: {log:#?}");

    for (&def, (at, bytes)) in bodies.iter().zip(&before) {
        let after = Body::of(&db, def);
        if def == edited {
            assert_ne!(*bytes, blaze_format::to_bytes(after), "the edit changed nothing of `second`");
            continue;
        }
        assert_eq!(*bytes, blaze_format::to_bytes(after), "{def:?}");
        assert!(std::ptr::eq(*at, after), "{def:?} was lowered again");
    }
    for (&def, before) in signatures.iter().zip(&signatures_before) {
        assert_eq!(*before, signature_store_bytes(&db, def), "{def:?}");
    }
}

/// (c) A body lowered again on this thread, on the scratch the lowering of other bodies left,
/// is the store it was lowered as the first time.
#[test]
fn a_body_lowered_again_on_a_reused_scratch_is_the_same() {
    let (mut db, file) = TestDB::with_single_file(CODE);
    let add = function_named(&db, "add");
    let first = function_named(&db, "first");
    let before = relative_body(&db, add);

    // `add` and then `first`, both bigger, so the scratch holds more than `add` needs.
    let bigger = CODE
        .replace("c * 2", "{ let d = [c; 4]; d[0] * 2 }")
        .replace("let y = x + 1;", "let y = (x + 1, x * 3, [x; 2]).0;");
    db.set_file_text(file.file_id(&db), &bigger);
    assert_ne!(*before, *relative_body(&db, add), "the edit changed nothing of `add`");
    relative_body(&db, first);

    db.set_file_text(file.file_id(&db), CODE);
    let log = db.executed_in_read_order(|| {
        relative_body(&db, add);
    });
    assert_eq!(count(&log, "Body::relative_"), 1, "`add` was not lowered again: {log:#?}");
    let again = relative_body(&db, add);
    assert!(!Arc::ptr_eq(&before, &again), "`add` was not lowered again");
    assert_eq!(*before, *again);
    assert_eq!(blaze_format::to_bytes(&*before), blaze_format::to_bytes(&*again));
}
