//! The four hashes and the keys: what an edit changes and what it keeps.

use hir_def::{AdtId, DefWithBodyId, expr_store::Body, hir::Expr};
use hir_ty::db::{InternedClosure, InternedClosureId};

use super::{TestDB, edit, item, single};
use crate::{ItemHashes, ItemId, Resolved, hashes, item_key, key, layout_hash, resolve};

fn of(db: &TestDB, path: &str) -> ItemHashes {
    hashes(db, item(db, path)).unwrap_or_else(|err| panic!("{}", err.describe(db)))
}

fn adt(db: &TestDB, path: &str) -> AdtId {
    match item(db, path) {
        ItemId::Adt(it) => it,
        other => panic!("`{path}` is {other:?}"),
    }
}

#[test]
fn formatting_keeps_every_hash_and_key() {
    let (mut db, file) = single(
        r#"
struct S { x: u32 }
fn f(s: &S) -> u32 { s.x + 1 }
"#,
    );
    let (f, s) = (of(&db, "f"), of(&db, "S"));
    let key = item_key(&db, item(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
// a comment
struct S {
    x: u32,
}

fn f(s: &S) -> u32 {
    // another
    s.x  +  1
}
"#,
    );
    assert_eq!(of(&db, "f"), f);
    assert_eq!(of(&db, "S"), s);
    assert_eq!(item_key(&db, item(&db, "f")), key);
}

#[test]
fn rename_keeps_hashes_and_changes_the_key() {
    let (mut db, file) = single(
        r#"
struct S { x: u32 }
fn f(s: &S) -> u32 { s.x }
fn g(s: &S) -> u32 { f(s) }
"#,
    );
    let (f, s, g) = (of(&db, "f"), of(&db, "S"), of(&db, "g"));
    let f_key = item_key(&db, item(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
struct T { x: u32 }
fn h(s: &T) -> u32 { s.x }
fn g(s: &T) -> u32 { h(s) }
"#,
    );
    assert_eq!(of(&db, "h"), f);
    assert_eq!(of(&db, "T"), s);
    assert_eq!(of(&db, "g"), g);
    assert_ne!(item_key(&db, item(&db, "h")), f_key);
}

#[test]
fn move_keeps_hashes_and_changes_the_key() {
    let (mut db, file) = single(
        r#"
pub(crate) fn f(x: u32) -> u32 { x * 2 }
"#,
    );
    let f = of(&db, "f");
    let key = item_key(&db, item(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
mod m {
    pub(crate) fn f(x: u32) -> u32 { x * 2 }
}
"#,
    );
    assert_eq!(of(&db, "m::f"), f);
    assert_ne!(item_key(&db, item(&db, "m::f")), key);
}

#[test]
fn a_visibility_is_relative_to_the_module() {
    let (mut db, file) = single(
        r#"
fn f() {}
mod m { pub(super) fn g() {} }
"#,
    );
    let (f, g) = (of(&db, "f"), of(&db, "m::g"));
    assert_ne!(f.interface, g.interface, "private at the root is not `pub(super)` in `m`");
    edit(
        &mut db,
        file,
        r#"
mod m { fn f() {} }
mod n { mod m { pub(super) fn g() {} } }
"#,
    );
    assert_eq!(of(&db, "m::f").interface, f.interface, "private is private wherever it is");
    assert_eq!(of(&db, "n::m::g").interface, g.interface);
}

#[test]
fn a_body_edit_keeps_the_interface_and_callers_code() {
    let (mut db, file) = single(
        r#"
fn f() -> u32 { 1 }
fn g() -> u32 { f() }
"#,
    );
    let (f, g) = (of(&db, "f"), of(&db, "g"));
    edit(
        &mut db,
        file,
        r#"
fn f() -> u32 { 2 }
fn g() -> u32 { f() }
"#,
    );
    let (f2, g2) = (of(&db, "f"), of(&db, "g"));
    assert_eq!(f2.interface, f.interface);
    assert_ne!(f2.code, f.code);
    assert_ne!(f2.deep, f.deep);
    assert_eq!(g2.code, g.code, "a caller's code names its callee by interface");
    assert_ne!(g2.deep, g.deep, "a caller's deep hash has its callee's body");
}

#[test]
fn a_signature_edit_changes_callers_code() {
    let (mut db, file) = single(
        r#"
fn f(x: u32) -> u32 { x }
fn g() -> u32 { f(1) }
"#,
    );
    let (f, g) = (of(&db, "f"), of(&db, "g"));
    edit(
        &mut db,
        file,
        r#"
fn f(x: u64) -> u32 { x as u32 }
fn g() -> u32 { f(1) }
"#,
    );
    assert_ne!(of(&db, "f").interface, f.interface);
    assert_ne!(of(&db, "g").code, g.code);
}

#[test]
fn a_body_edit_executes_no_interface_form_of_another_item() {
    let (mut db, file) = single(
        r#"
fn f() -> u32 { 1 }
fn g() -> u32 { f() }
"#,
    );
    let _ = (of(&db, "f"), of(&db, "g"));
    edit(
        &mut db,
        file,
        r#"
fn f() -> u32 { 2 }
fn g() -> u32 { f() }
"#,
    );
    let g = item(&db, "g");
    let executed = db.executed(|| {
        crate::interface_hash(&db, g).unwrap();
    });
    assert!(
        !executed.iter().any(|query| query.contains("interface_form")),
        "re-ran forms: {executed:?}"
    );
}

#[test]
fn whitespace_inside_a_literal_is_code() {
    // #1326: a formatter that dropped the bytes of a literal with the whitespace around it.
    let (mut db, file) = single(
        r#"
fn f() -> &'static str { "a b" }
"#,
    );
    let f = of(&db, "f");
    edit(
        &mut db,
        file,
        r#"
fn f() -> &'static str { "a  b" }
"#,
    );
    assert_ne!(of(&db, "f").code, f.code);
}

#[test]
fn layout_is_deep() {
    let (mut db, file) = single(
        r#"
struct B { x: u8 }
struct A { b: B, y: u16 }
impl B { fn get(&self) -> u8 { self.x } }
"#,
    );
    let a = layout_hash(&db, adt(&db, "A")).unwrap();
    edit(
        &mut db,
        file,
        r#"
struct B { x: u8 }
struct A { b: B, y: u16 }
impl B { fn get(&self) -> u8 { self.x + 1 } }
"#,
    );
    assert_eq!(layout_hash(&db, adt(&db, "A")).unwrap(), a, "an impl is not in a layout");
    edit(
        &mut db,
        file,
        r#"
struct B { x: u64 }
struct A { b: B, y: u16 }
impl B { fn get(&self) -> u8 { self.x as u8 } }
"#,
    );
    assert_ne!(layout_hash(&db, adt(&db, "A")).unwrap(), a, "a field's field is in a layout");
}

#[test]
fn a_cycle_hashes_by_content() {
    let (mut db, file) = single(
        r#"
struct A { b: *const B, x: u8 }
struct B { a: *const A }
"#,
    );
    let (a, b) = (of(&db, "A"), of(&db, "B"));
    assert_ne!(a.interface, b.interface);
    edit(
        &mut db,
        file,
        r#"
struct B { a: *const A }
struct A { b: *const B, x: u8 }
"#,
    );
    assert_eq!(of(&db, "A"), a, "order of declaration is not content");
    edit(
        &mut db,
        file,
        r#"
struct A { b: *const C, x: u8 }
struct C { a: *const A }
"#,
    );
    assert_eq!(of(&db, "A"), a, "a rename inside a cycle is not content");
    assert_eq!(of(&db, "C"), b);
    edit(
        &mut db,
        file,
        r#"
struct A { b: *const C, x: u16 }
struct C { a: *const A }
"#,
    );
    assert_ne!(of(&db, "C").interface, b.interface, "an edit of one member is the whole cycle's");
}

#[test]
fn type_name_observes_the_name() {
    let (mut db, file) = single(
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct S;
fn f() -> &'static str { intrinsics::type_name::<S>() }
"#,
    );
    let f = of(&db, "f");
    edit(
        &mut db,
        file,
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct T;
fn f() -> &'static str { intrinsics::type_name::<T>() }
"#,
    );
    assert_ne!(of(&db, "f").code, f.code, "`type_name` spells the name");
}

fn closures(db: &TestDB, f: ItemId) -> Vec<InternedClosureId<'_>> {
    let ItemId::Function(func) = f else { panic!("{f:?} is no fn") };
    let owner = DefWithBodyId::from(func);
    Body::of(db, owner)
        .exprs()
        .filter_map(|(expr, it)| match it {
            Expr::Closure { closure_kind, .. } => Some(InternedClosureId::new(
                db,
                InternedClosure { owner: owner.into(), expr, kind: *closure_kind },
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn a_capture_edit_is_its_closure_s_alone() {
    // #906: a sibling closure stayed stale after one's captures changed.
    let (mut db, file) = single(
        r#"
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c1 = || a;
    let c2 = || b;
    c1() + c2()
}
"#,
    );
    let f = item(&db, "f");
    let before: Vec<_> =
        closures(&db, f).into_iter().map(|it| crate::closure_code(&db, it).unwrap()).collect();
    edit(
        &mut db,
        file,
        r#"
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c1 = || b;
    let c2 = || b;
    c1() + c2()
}
"#,
    );
    let f = item(&db, "f");
    let after: Vec<_> =
        closures(&db, f).into_iter().map(|it| crate::closure_code(&db, it).unwrap()).collect();
    assert_eq!(before.len(), 2);
    assert_ne!(after[0], before[0]);
    assert_eq!(after[1], before[1]);
}

#[test]
fn every_key_resolves_back() {
    let (db, _) = single(
        r#"
mod m {
    pub enum E { A, B(u8) }
    pub trait Tr { fn m(&self) -> u8; }
}
struct S;
impl m::Tr for S { fn m(&self) -> u8 { 0 } }
const _: () = ();
fn f() -> impl Sized {
    struct Inner;
    let c = || 1u8;
    c();
    Inner
}
"#,
    );
    for id in super::items(&db) {
        assert_eq!(resolve(&db, item_key(&db, id)), Some(Resolved::Item(id)), "{id:?}");
    }
    let f = item(&db, "f");
    for closure in closures(&db, f) {
        let loc = closure.loc(&db);
        let found = resolve(&db, key::closure_key(&db, closure));
        assert!(
            matches!(found, Some(Resolved::Closure { expr, .. }) if expr == loc.expr),
            "{found:?}"
        );
    }
    assert_eq!(
        resolve(&db, key::opaque_key(&db, f, 0)),
        Some(Resolved::Opaque { owner: f, index: 0 })
    );
    let ItemId::Adt(AdtId::EnumId(e)) = item(&db, "m::E") else { panic!() };
    for (_, &(variant, _)) in &e.enum_variants(&db).variants {
        assert_eq!(resolve(&db, key::variant_key(&db, variant)), Some(Resolved::Variant(variant)));
    }
    let ItemId::Function(func) = f else { panic!() };
    let mut inner = 0;
    for (_, block) in Body::of(&db, func.into()).blocks(&db) {
        for (_, module) in block.modules(&db) {
            for def in module.scope(&db).declarations() {
                if let hir_def::ModuleDefId::AdtId(it) = def {
                    let id = ItemId::Adt(it);
                    assert_eq!(resolve(&db, item_key(&db, id)), Some(Resolved::Item(id)));
                    inner += 1;
                }
            }
        }
    }
    assert_eq!(inner, 1);
}

#[test]
fn an_edit_before_a_block_keeps_its_items_keys() {
    let (mut db, file) = single(
        r#"
fn f() {
    let x = 1;
    { struct Inner; }
}
"#,
    );
    let key_of_inner = |db: &TestDB| {
        let ItemId::Function(func) = item(db, "f") else { panic!() };
        let mut keys = Vec::new();
        for (_, block) in Body::of(db, func.into()).blocks(db) {
            for (_, module) in block.modules(db) {
                for def in module.scope(db).declarations() {
                    if let hir_def::ModuleDefId::AdtId(it) = def {
                        keys.push(item_key(db, ItemId::Adt(it)));
                    }
                }
            }
        }
        keys
    };
    let before = key_of_inner(&db);
    assert_eq!(before.len(), 1);
    edit(
        &mut db,
        file,
        r#"
fn f() {
    let x = 1;
    let y = x + 2;
    { struct Inner; }
}
"#,
    );
    assert_eq!(key_of_inner(&db), before);
}

#[test]
fn keys_of_two_crates_differ() {
    let db = <TestDB as test_fixture::WithFixture>::with_files(
        r#"
//- /a.rs crate:a
pub struct S;
//- /b.rs crate:b
pub struct S;
"#,
    );
    let krates: Vec<_> = base_db::all_crates(&db).iter().copied().collect();
    let keys: Vec<_> = krates.iter().map(|&krate| key::crate_key(&db, krate)).collect();
    for (at, a) in keys.iter().enumerate() {
        assert!(!keys[at + 1..].contains(a), "{krates:?}");
    }
}

#[test]
fn a_batch_of_keys_is_each_key_alone() {
    let (db, _) = single(
        r#"
mod m { pub struct A; pub fn f() {} pub enum E { X, Y } }
trait Tr { fn a(); const C: u8; type T; }
impl Tr for m::A { fn a() {} const C: u8 = 0; type T = u8; }
macro_rules! mac { () => {} }
extern "C" { fn ext(); }
const _: () = ();
"#,
    );
    let items = super::items(&db);
    assert!(items.len() >= 12, "{items:?}");
    for id in items {
        assert_eq!(item_key(&db, id), key::item_key_alone(&db, id), "{id:?}");
    }
}
