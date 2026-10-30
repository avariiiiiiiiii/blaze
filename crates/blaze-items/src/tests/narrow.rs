//! Facts recorded where they are read (docs/v2/items.md section 6.2): each test runs a query,
//! edits, runs it again and checks what reran. A query that reads a narrow fact reruns only
//! when that fact changed; the narrow query itself reruns and comes out equal.

use base_db::{Crate, FileId, SourceDatabase, all_crates};
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, FunctionId, GenericDefId, ImplId, ModuleDefId, StructId,
    TraitId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature, TraitSignature},
};
use hir_ty::{
    InferenceResult, ParamEnvAndCrate, check_orphan_rules,
    db::HirDatabase,
    drop::destructor,
    layout::layout_of_adt_query,
    method_resolution::TraitImpls,
    next_solver::{DbInterner, GenericArgs},
    traits::is_inherent_impl_coherent,
};
use rustc_type_ir::inherent::GenericArgs as _;
use test_fixture::WithFixture;

use super::db::TestDB;

/// Whether `query` is among the salsa debug names in `executed`, which end in `_` for a
/// method of a tracked impl (`TraitImpls < 'db >::for_crate_`).
fn ran(executed: &[String], query: &str) -> bool {
    executed.iter().any(|it| it.trim_end_matches('_').ends_with(query))
}

fn single_file(text: &str) -> (TestDB, FileId) {
    let (db, file) = TestDB::with_single_file(&format!("//- /main.rs crate:main\n{text}"));
    let file = file.file_id(&db);
    (db, file)
}

fn krate(db: &TestDB) -> Crate {
    *all_crates(db).last().expect("a crate")
}

fn declarations(db: &TestDB) -> Vec<ModuleDefId> {
    let def_map = crate_def_map(db, krate(db));
    def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

fn impls(db: &TestDB) -> Vec<ImplId> {
    let def_map = crate_def_map(db, krate(db));
    def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .collect()
}

fn struct_named(db: &TestDB, name: &str) -> StructId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::AdtId(AdtId::StructId(it))
                if StructSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no struct `{name}`"))
}

fn trait_named(db: &TestDB, name: &str) -> TraitId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::TraitId(it) if TraitSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no trait `{name}`"))
}

fn function_named(db: &TestDB, name: &str) -> FunctionId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

/// The function `name` of the first impl that has one.
fn impl_function_named(db: &TestDB, name: &str) -> FunctionId {
    impls(db)
        .into_iter()
        .find_map(|impl_| {
            impl_.impl_items(db).items.iter().find_map(|(item_name, item)| match *item {
                AssocItemId::FunctionId(it) if item_name.as_str() == name => Some(it),
                _ => None,
            })
        })
        .unwrap_or_else(|| panic!("no impl function `{name}`"))
}

/// The impl of the trait `trait_`, or the inherent impl with `None`.
fn impl_of(db: &TestDB, trait_: Option<TraitId>) -> ImplId {
    hir_ty::attach_db(db, || {
        impls(db)
            .into_iter()
            .find(|&it| db.impl_trait(it).map(|it| it.skip_binder().def_id.0) == trait_)
            .expect("an impl")
    })
}

fn infer(db: &TestDB, function: FunctionId) {
    hir_ty::attach_db(db, || {
        let _ = InferenceResult::of(db, DefWithBodyId::from(function));
    })
}

const DROP: &str = r#"
#[lang = "drop"]
pub trait Drop {
    fn drop(&mut self);
}
pub struct A;
pub struct B;
"#;

fn destructor_of(db: &TestDB, adt: AdtId) -> Option<ImplId> {
    hir_ty::attach_db(db, || destructor(db, adt))
}

#[test]
fn a_drop_impl_for_another_type_leaves_destructor() {
    let (mut db, file) = single_file(DROP);
    let a = AdtId::from(struct_named(&db, "A"));
    let cold = db.executed(|| assert_eq!(destructor_of(&db, a), None));
    assert!(ran(&cold, "destructor"), "{cold:#?}");

    db.set_file_text(file, &format!("{DROP}impl Drop for B {{\n    fn drop(&mut self) {{}}\n}}\n"));
    let warm = db.executed(|| assert_eq!(destructor_of(&db, a), None));
    assert!(ran(&warm, "::in_crate_for_trait_and_self_ty"), "{warm:#?}");
    assert!(!ran(&warm, "destructor"), "{warm:#?}");
}

#[test]
fn a_drop_impl_for_the_type_reruns_destructor() {
    let (mut db, file) = single_file(DROP);
    let a = AdtId::from(struct_named(&db, "A"));
    assert_eq!(destructor_of(&db, a), None);

    db.set_file_text(file, &format!("{DROP}impl Drop for A {{\n    fn drop(&mut self) {{}}\n}}\n"));
    let warm = db.executed(|| assert!(destructor_of(&db, a).is_some()));
    assert!(ran(&warm, "destructor"), "{warm:#?}");
}

const CALLER: &str = "mod a;\nfn caller(x: a::A) {\n    x.m();\n}\n";
const IMPL: &str = "pub struct A;\nimpl A {\n    pub fn m(&self) {}\n}\n";

/// A caller of `A::m` in one file and `impl A` in another, so an edit of the impl moves
/// nothing in the caller's file.
fn method_db() -> (TestDB, FileId, FunctionId) {
    let (db, files) =
        TestDB::with_many_files(&format!("//- /main.rs crate:main\n{CALLER}//- /a.rs\n{IMPL}"));
    let impl_file = files[1].file_id(&db);
    let caller = function_named(&db, "caller");
    (db, impl_file, caller)
}

#[test]
fn a_method_of_another_name_leaves_the_lookup() {
    let (mut db, impl_file, caller) = method_db();
    let cold = db.executed(|| infer(&db, caller));
    assert!(ran(&cold, "::for_body"), "{cold:#?}");
    assert!(ran(&cold, "impl_value_item"), "{cold:#?}");

    db.set_file_text(
        impl_file,
        "pub struct A;\nimpl A {\n    pub fn m(&self) {}\n    pub fn n(&self) {}\n}\n",
    );
    let warm = db.executed(|| infer(&db, caller));
    assert!(ran(&warm, "impl_value_item"), "{warm:#?}");
    assert!(!ran(&warm, "::for_body"), "{warm:#?}");
}

#[test]
fn removing_the_method_reruns_the_lookup() {
    let (mut db, impl_file, caller) = method_db();
    infer(&db, caller);

    db.set_file_text(impl_file, "pub struct A;\nimpl A {\n    pub fn k(&self) {}\n}\n");
    let warm = db.executed(|| infer(&db, caller));
    assert!(ran(&warm, "::for_body"), "{warm:#?}");
}

const SPECIALIZING: &str = "#![feature(specialization)]
pub trait Tr {
    fn f(&self) {}
}
pub trait Other {}
pub struct S;
impl<T> Tr for T {}
impl Tr for S {}
";

fn impls_of_trait(db: &TestDB, trait_: TraitId) -> usize {
    hir_ty::attach_db(db, || TraitImpls::in_crate_and_deps_for_trait(db, krate(db), trait_).len())
}

#[test]
fn an_impl_of_another_trait_leaves_the_impls_of_a_trait() {
    let (mut db, file) = single_file(SPECIALIZING);
    let tr = trait_named(&db, "Tr");
    let cold = db.executed(|| assert_eq!(impls_of_trait(&db, tr), 2));
    assert!(ran(&cold, "::in_crate_and_deps_for_trait"), "{cold:#?}");

    db.set_file_text(file, &format!("{SPECIALIZING}impl Other for S {{}}\n"));
    let warm = db.executed(|| assert_eq!(impls_of_trait(&db, tr), 2));
    assert!(ran(&warm, "::in_crate_for_trait"), "{warm:#?}");
    assert!(!ran(&warm, "::in_crate_and_deps_for_trait"), "{warm:#?}");
}

#[test]
fn an_impl_of_the_trait_reruns_the_impls_of_the_trait() {
    let (mut db, file) = single_file(SPECIALIZING);
    let tr = trait_named(&db, "Tr");
    assert_eq!(impls_of_trait(&db, tr), 2);

    db.set_file_text(file, &format!("{SPECIALIZING}impl Tr for u32 {{}}\n"));
    let warm = db.executed(|| assert_eq!(impls_of_trait(&db, tr), 3));
    assert!(ran(&warm, "::in_crate_and_deps_for_trait"), "{warm:#?}");
}

const COHERENCE: &str = "pub trait Tr {}\npub struct S;\nimpl Tr for S {}\nimpl S {}\n";

/// Whether the trait impl of `Tr` satisfies the orphan rules and the inherent impl is
/// coherent.
fn coherence(db: &TestDB) -> (bool, bool) {
    let tr = trait_named(db, "Tr");
    let trait_impl = impl_of(db, Some(tr));
    let inherent = impl_of(db, None);
    hir_ty::attach_db(db, || {
        let def_map = crate_def_map(db, krate(db));
        (check_orphan_rules(db, trait_impl), is_inherent_impl_coherent(db, def_map, inherent))
    })
}

#[test]
fn an_unrelated_item_leaves_coherence() {
    let (mut db, file) = single_file(COHERENCE);
    let cold = db.executed(|| assert_eq!(coherence(&db), (true, true)));
    assert!(ran(&cold, "orphan_rules_hold"), "{cold:#?}");
    assert!(ran(&cold, "inherent_impl_coherent"), "{cold:#?}");

    db.set_file_text(file, &format!("{COHERENCE}pub fn unrelated() {{}}\n"));
    let warm = db.executed(|| assert_eq!(coherence(&db), (true, true)));
    assert!(!ran(&warm, "orphan_rules_hold"), "{warm:#?}");
    assert!(!ran(&warm, "inherent_impl_coherent"), "{warm:#?}");
}

#[test]
fn changing_the_impls_reruns_coherence() {
    let (mut db, file) = single_file(COHERENCE);
    assert_eq!(coherence(&db), (true, true));

    db.set_file_text(file, "pub trait Tr {}\npub struct S;\nimpl Tr for u32 {}\nimpl u32 {}\n");
    let warm = db.executed(|| assert_eq!(coherence(&db), (true, false)));
    assert!(ran(&warm, "orphan_rules_hold"), "{warm:#?}");
    assert!(ran(&warm, "inherent_impl_coherent"), "{warm:#?}");
}

const LAYOUT: &str = "pub struct S {\n    a: u32,\n    b: u8,\n}\n";

fn size_of_struct(db: &TestDB, name: &str) -> u64 {
    let adt = AdtId::from(struct_named(db, name));
    hir_ty::attach_db(db, || {
        let interner = DbInterner::new_no_crate(db);
        let args = GenericArgs::identity_for_item(interner, adt.into());
        let env = ParamEnvAndCrate {
            param_env: db.trait_environment(GenericDefId::AdtId(adt)),
            krate: krate(db),
        };
        layout_of_adt_query(db, adt, args.store(), env.store()).expect("a layout").size.bytes()
    })
}

#[test]
fn renaming_a_field_leaves_the_layout() {
    let (mut db, file) = single_file(LAYOUT);
    let cold = db.executed(|| assert_eq!(size_of_struct(&db, "S"), 8));
    assert!(ran(&cold, "layout_of_adt_query"), "{cold:#?}");

    db.set_file_text(file, "pub struct S {\n    c: u32,\n    b: u8,\n}\n");
    let warm = db.executed(|| assert_eq!(size_of_struct(&db, "S"), 8));
    assert!(ran(&warm, "variant_field_ids"), "{warm:#?}");
    assert!(!ran(&warm, "layout_of_adt_query"), "{warm:#?}");
}

#[test]
fn changing_a_field_type_reruns_the_layout() {
    let (mut db, file) = single_file(LAYOUT);
    assert_eq!(size_of_struct(&db, "S"), 8);

    db.set_file_text(file, "pub struct S {\n    a: u32,\n    b: u64,\n}\n");
    let warm = db.executed(|| assert_eq!(size_of_struct(&db, "S"), 16));
    assert!(ran(&warm, "layout_of_adt_query"), "{warm:#?}");
}

const ATPIT: &str = "pub trait Tr {
    fn m(&self) -> u32;
}
pub struct A;
impl Tr for A {
    fn m(&self) -> u32 {
        1
    }
";

#[test]
fn a_constant_added_to_the_impl_leaves_its_bodies() {
    let (mut db, file) = single_file(&format!("{ATPIT}}}\n"));
    let m = impl_function_named(&db, "m");
    let cold = db.executed(|| infer(&db, m));
    assert!(ran(&cold, "::for_body"), "{cold:#?}");
    assert!(ran(&cold, "impl_type_aliases"), "{cold:#?}");

    db.set_file_text(file, &format!("{ATPIT}    const C: u32 = 0;\n}}\n"));
    let m = impl_function_named(&db, "m");
    let warm = db.executed(|| infer(&db, m));
    assert!(ran(&warm, "impl_type_aliases"), "{warm:#?}");
    assert!(!ran(&warm, "::for_body"), "{warm:#?}");
}

#[test]
fn an_associated_type_added_to_the_impl_reruns_its_bodies() {
    let (mut db, file) = single_file(&format!("{ATPIT}}}\n"));
    let m = impl_function_named(&db, "m");
    infer(&db, m);

    db.set_file_text(file, &format!("{ATPIT}    type X = u32;\n}}\n"));
    let m = impl_function_named(&db, "m");
    let warm = db.executed(|| infer(&db, m));
    assert!(ran(&warm, "::for_body"), "{warm:#?}");
}
