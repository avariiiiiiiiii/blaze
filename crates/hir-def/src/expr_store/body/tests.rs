//! [`BodySyntax::tree`] against [`BodySyntax::root`], and the thread's tree cache.

use syntax::view;
use test_fixture::WithFixture;

use crate::{
    AssocItemId, DefWithBodyId, ModuleDefId,
    expr_store::body::body_syntax,
    nameres::{assoc::ImplItems, crate_def_map},
    test_db::TestDB,
};

const CODE: &str = r#"
struct S;

impl S {
    fn a(&self) -> u8 { 1 }
    fn b(x: u8) -> u8 { x + 2 }
}

fn free() {
    fn nested() {}
    let _ = 3;
}

const K: u8 = { 4 };
static V: u8 = K + 1;

mod m {
    pub fn deep() -> u8 { 6 }
}
"#;

fn bodies(db: &TestDB) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    let krate = *base_db::all_crates(db).last().expect("a crate");
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                ModuleDefId::ConstId(it) => bodies.push(it.into()),
                ModuleDefId::StaticId(it) => bodies.push(it.into()),
                _ => {}
            }
        }
        for imp in scope.impls() {
            for (_, item) in &ImplItems::of(db, imp).0.items {
                if let AssocItemId::FunctionId(it) = *item {
                    bodies.push(it.into());
                }
            }
        }
    }
    bodies
}

#[test]
fn tree_viewed_is_root() {
    let db = TestDB::with_files(CODE);
    let bodies = bodies(&db);
    assert!(bodies.len() >= 6, "{bodies:?}");
    for def in bodies {
        let syntax = body_syntax(&db, def).as_ref().expect("a body written in a file");
        let tree = syntax.tree(&db);
        let of_view: Vec<_> = view::Node::root(&tree)
            .descendants_with_tokens()
            .map(|it| (it.kind(), it.text_range()))
            .collect();
        let of_handle: Vec<_> = syntax
            .root(&db)
            .descendants_with_tokens()
            .map(|it| (it.kind(), it.text_range()))
            .collect();
        assert_eq!(of_view, of_handle, "{def:?}");
        assert_eq!(usize::from(view::Node::root(&tree).text_range().len()), syntax.text().len());
    }
}

/// `RA_CHECK_RELATIVE_BODIES` parses every tree again, so there is nothing kept to compare.
#[test]
fn tree_is_kept_on_its_thread() {
    if super::check_relative() {
        return;
    }
    let db = TestDB::with_files(CODE);
    for def in bodies(&db) {
        let syntax = body_syntax(&db, def).as_ref().expect("a body written in a file");
        let (first, second) = (syntax.tree(&db), syntax.tree(&db));
        assert!(std::sync::Arc::ptr_eq(&first, &second), "{def:?}");
    }
}
