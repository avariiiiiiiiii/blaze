mod db;

use base_db::{SourceDatabase, all_crates};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature,
};
use hir_ide::{InferBodyId, InferenceResult, db::HirDatabase};
use hir_ty_borrowck::{Checked, NotChecked, Refused};
use test_fixture::WithFixture;

use self::db::TestDB;

fn function(db: &TestDB, name: &str) -> DefWithBodyId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return FunctionId::into(it);
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

fn count(executed: &[String], query: &str) -> usize {
    executed.iter().filter(|it| *it == query).count()
}

#[test]
fn check_infers_only_workspace_bodies() {
    let db = TestDB::with_files(
        r#"
//- /dep.rs crate:dep library
pub struct S;
impl S {
    pub fn method(&self) -> u32 { let x = 3; x }
}
pub fn called() -> u32 { let x = 1; x }
pub fn not_called() -> u32 { let x = 2; x }
//- /main.rs crate:main deps:dep
fn main() {
    let _ = dep::called();
    let _ = dep::S.method();
}
"#,
    );
    crate::attach_db(&db, || {
        let workspace = crate::check::workspace_crates(&db);
        assert_eq!(workspace.len(), 1, "only `main` is a workspace member");
        let executed = db.executed(|| {
            crate::check::check_workspace(&db, false);
        });
        assert_eq!(count(&executed, "infer_body_query"), 1, "{executed:#?}");
    });
}

const MOVED_TWICE: &str = r#"
//- minicore: sized, copy
struct S;
fn consume(_: S) {}
fn moved_twice() {
    let s = S;
    consume(s);
    consume(s);
}
fn moved_once() {
    let s = S;
    consume(s);
}
fn mut_unused() {
    let mut s = S;
    consume(s);
}
fn mismatched() -> u32 {
    S
}
"#;

#[test]
fn a_build_gets_no_mir_of_a_body_whose_borrow_check_failed() {
    let db = TestDB::with_files(MOVED_TWICE);
    crate::attach_db(&db, || {
        let bad = function(&db, "moved_twice");
        match hir_ty_borrowck::checked(&db, bad) {
            Err(Refused::Errors { owner, errors }) => {
                assert_eq!(owner, bad);
                assert!(errors.iter().any(|it| it.code == "E0382"), "{errors:?}");
            }
            other => panic!("expected a borrowck error, got {other:?}"),
        }
        let good = function(&db, "moved_once");
        let checked = hir_ty_borrowck::checked(&db, good).unwrap();
        let mir = checked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(good))).unwrap();
        assert_eq!(mir.checked().owner(), good);
    });
}

#[test]
fn a_lint_of_the_borrow_checker_lets_the_body_build() {
    let db = TestDB::with_files(MOVED_TWICE);
    crate::attach_db(&db, || {
        let def = function(&db, "mut_unused");
        assert!(
            hir_ty_borrowck::borrowck(&db, def).diagnostics.iter().any(|it| !it.is_error()),
            "`unused_mut` is reported"
        );
        assert_eq!(hir_ty_borrowck::checked(&db, def).map(Checked::owner), Ok(def));
    });
}

#[test]
fn a_body_with_type_errors_is_refused_not_passed() {
    let db = TestDB::with_files(MOVED_TWICE);
    crate::attach_db(&db, || {
        let def = function(&db, "mismatched");
        assert_eq!(
            hir_ty_borrowck::checked(&db, def),
            Err(Refused::NotChecked { owner: def, why: NotChecked::TypeErrors })
        );
    });
}

#[test]
fn a_check_takes_no_mir_of_another_body() {
    let db = TestDB::with_files(MOVED_TWICE);
    crate::attach_db(&db, || {
        let good = function(&db, "moved_once");
        let other = function(&db, "mut_unused");
        let checked = hir_ty_borrowck::checked(&db, good).unwrap();
        match checked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(other))) {
            Err(Refused::NotOf { owner, .. }) => assert_eq!(owner, good),
            other => panic!("expected the MIR to be refused, got {other:?}"),
        }
        let (unchecked, pending) = hir_ty_borrowck::speculate(good);
        assert!(matches!(
            unchecked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(other))),
            Err(Refused::NotOf { .. })
        ));
        assert_eq!(pending.finish(&db).map(Checked::owner), Ok(good));
    });
}

#[test]
fn a_live_patch_has_code_before_the_check_that_refuses_it() {
    let db = TestDB::with_files(MOVED_TWICE);
    crate::attach_db(&db, || {
        let bad = function(&db, "moved_twice");
        let (unchecked, pending) = hir_ty_borrowck::speculate(bad);
        let executed = db.executed(|| {
            let mir = unchecked.mir(&db, db.mir_body(InferBodyId::DefWithBodyId(bad))).unwrap();
            assert_eq!(mir.unchecked().owner(), bad);
            assert!(!mir.basic_blocks.is_empty());
        });
        assert_eq!(count(&executed, "borrowck"), 0, "the code comes before the check");
        match pending.finish(&db) {
            Err(Refused::Errors { owner, errors }) => {
                assert_eq!(owner, bad);
                assert!(errors.iter().any(|it| it.code == "E0382"), "{errors:?}");
            }
            other => panic!("the patch must be rolled back, got {other:?}"),
        }
    });
}

#[test]
fn dropped_results_keep_cutoff_and_come_back_equal() {
    let (mut db, files) = TestDB::with_many_files(
        r#"
//- minicore: sized, copy
//- /main.rs crate:main
struct S;
fn consume(_: S) {}
fn evicted() -> u32 {
    let s = S;
    consume(s);
    1
}
//- /other.rs crate:other
pub fn g() {}
"#,
    );
    let other = files
        .iter()
        .map(|it| it.file_id(&db))
        .find(|&it| db.file_text(it).text(&db).contains("pub fn g"))
        .unwrap();
    let f = crate::attach_db(&db, || function(&db, "evicted"));
    let before = crate::attach_db(&db, || {
        hir_ty_borrowck::borrowck(&db, f);
        let mir = db.mir_body(InferBodyId::DefWithBodyId(f)).unwrap();
        (mir.basic_blocks.len(), mir.locals.len())
    });

    crate::evict::drop_after_store(&mut db, &[f]);
    db.set_file_text(other, "pub fn g() {}\npub fn h() {}\n");

    crate::attach_db(&db, || {
        let executed = db.executed(|| {
            hir_ty_borrowck::borrowck(&db, f);
        });
        assert_eq!(count(&executed, "borrowck"), 0, "{executed:#?}");
        assert_eq!(count(&executed, "infer_body_query"), 0, "{executed:#?}");

        let executed = db.executed(|| {
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(f));
        });
        assert_eq!(count(&executed, "infer_body_query"), 1, "{executed:#?}");

        let executed = db.executed(|| {
            let mir = db.mir_body(InferBodyId::DefWithBodyId(f)).unwrap();
            assert_eq!((mir.basic_blocks.len(), mir.locals.len()), before);
        });
        assert_eq!(count(&executed, "mir_body_query"), 1, "{executed:#?}");
    });
}
