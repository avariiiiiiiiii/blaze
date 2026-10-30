//! The MIR passes over fixtures, through the queries a build asks: rustc's UB checks, storage
//! markers kept for a code generator that emits them, and `file!()` as the build names files.
//! An integration test, so the test database's queries are of the one copy of this crate the
//! tests call.

mod db;
mod machine;
mod source_names;
mod storage;
mod threads;
mod ub_checks;
mod unwind;

use hir_def::{
    ConstId, FunctionId, ModuleDefId,
    nameres::crate_def_map,
    signatures::{ConstSignature, FunctionSignature},
};
use hir_ty::db::HirDatabase;
use hir_ty_mir::mir::{MirOptions, print_mir_body};
use test_fixture::WithFixture;

use db::TestDB;

fn declarations(db: &TestDB) -> Vec<ModuleDefId> {
    crate_def_map(db, db.test_crate())
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    let found: Vec<FunctionId> = declarations(db)
        .into_iter()
        .filter_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} functions named `{name}`", found.len()),
    }
}

#[track_caller]
fn constant(db: &TestDB, name: &str) -> ConstId {
    let found: Vec<ConstId> = declarations(db)
        .into_iter()
        .filter_map(|def| match def {
            ModuleDefId::ConstId(it)
                if ConstSignature::of(db, it).name.as_ref().is_some_and(|it| it.as_str() == name) =>
            {
                Some(it)
            }
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} constants named `{name}`", found.len()),
    }
}

/// The MIR of the function `name` in a one-file fixture, as asked with `options`, printed.
#[track_caller]
fn mir(fixture: &str, name: &str, options: MirOptions) -> String {
    let (db, _) = TestDB::with_single_file(fixture);
    hir_ty::attach_db(&db, || {
        let func = function(&db, name);
        let body = db
            .optimized_mir_body(func.into(), options)
            .unwrap_or_else(|error| panic!("the MIR of `{name}`: {error:?}"));
        print_mir_body(&db, &body, name)
    })
}
