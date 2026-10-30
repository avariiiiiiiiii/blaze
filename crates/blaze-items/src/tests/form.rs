//! Forms: which edits reach an item's interface, body and closures, and which leave them.

use hir_def::{DefWithBodyId, expr_store::Body, hir::Expr};
use hir_ty::db::{InternedClosure, InternedClosureId};

use super::{TestDB, edit, item, single};
use crate::{
    ItemId,
    form::{Cx, Missed, Sub},
    key,
};

/// A form's digest, its slots by path and what it missed: slots of one fixture compared across
/// edits, which may renumber their ids.
type Named = (blaze_ids::ContentId, Vec<String>, Option<Missed>);

fn named(db: &TestDB, sub: Sub) -> Named {
    (sub.digest, sub.refs.iter().map(|&it| key::path(db, it)).collect(), sub.missed)
}

fn interface(db: &TestDB, path: &str) -> Named {
    named(db, Cx::new(db).interface(item(db, path)))
}

#[track_caller]
fn body(db: &TestDB, path: &str) -> Named {
    let id = item(db, path);
    let code = Cx::new(db).body(id).unwrap_or_else(|| panic!("`{path}` has no body"));
    named(db, code.sub)
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
fn formatting_and_comments_keep_interface_and_body() {
    let (mut db, file) = single(
        r#"
struct S { x: u32 }
fn f(s: &S) -> u32 { s.x + 1 }
"#,
    );
    let (interface_before, body_before) = (interface(&db, "f"), body(&db, "f"));
    let s = interface(&db, "S");
    edit(
        &mut db,
        file,
        r#"
// A struct.
struct S {
    /// Its field.
    x: u32,
}

/* A function. */
fn f(
    s: &S,
) -> u32 {
    // One more.
    s.x
        + 1
}
"#,
    );
    assert_eq!(interface(&db, "f"), interface_before);
    assert_eq!(body(&db, "f"), body_before);
    assert_eq!(interface(&db, "S"), s);
}

#[test]
fn renaming_a_fn_keeps_interface_and_body() {
    let (mut db, file) = single(
        r#"
struct S;
fn f(s: S) -> S { s }
"#,
    );
    let (interface_before, body_before) = (interface(&db, "f"), body(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
struct S;
fn g(s: S) -> S { s }
"#,
    );
    assert_eq!(interface(&db, "g"), interface_before);
    assert_eq!(body(&db, "g"), body_before);
}

#[test]
fn whitespace_in_a_raw_string_is_code() {
    let (mut db, file) = single(
        r#"
fn f() -> &'static str { r"a b" }
"#,
    );
    let before = body(&db, "f");
    edit(
        &mut db,
        file,
        r#"
fn f() -> &'static str { r"a  b" }
"#,
    );
    assert_ne!(body(&db, "f").0, before.0);
}

#[test]
fn a_field_type_is_the_struct_s_interface() {
    let (mut db, file) = single("struct S { x: u32 }");
    let before = interface(&db, "S");
    edit(&mut db, file, "struct S { x: u64 }");
    assert_ne!(interface(&db, "S").0, before.0);
}

#[test]
fn where_clause_order_is_not_observed() {
    let (mut db, file) = single(
        r#"
trait A {}
trait B {}
fn f<T>() where T: A, T: B {}
"#,
    );
    let before = interface(&db, "f");
    edit(
        &mut db,
        file,
        r#"
trait A {}
trait B {}
fn f<T>() where T: B, T: A {}
"#,
    );
    assert_eq!(interface(&db, "f"), before);
}

#[test]
fn trait_method_order_is_the_trait_s_interface() {
    let (mut db, file) = single(
        r#"
trait Tr {
    fn a(&self) -> u8;
    fn b(&self) -> u16;
}
"#,
    );
    let before = interface(&db, "Tr");
    edit(
        &mut db,
        file,
        r#"
trait Tr {
    fn b(&self) -> u16;
    fn a(&self) -> u8;
}
"#,
    );
    let after = interface(&db, "Tr");
    assert_ne!(after.1, before.1, "a trait's items are slots in declaration order");
}

#[test]
fn a_capture_edit_reaches_the_owner_and_the_closure_s_captures() {
    // #906
    let (mut db, file) = single(
        r#"
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c = || a;
    c() + b
}
"#,
    );
    let f = item(&db, "f");
    let owner = body(&db, "f");
    let captures: Vec<_> =
        closures(&db, f).into_iter().map(|it| Cx::new(&db).captures(it).digest).collect();
    edit(
        &mut db,
        file,
        r#"
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c = || b;
    c() + a
}
"#,
    );
    let f = item(&db, "f");
    let after: Vec<_> =
        closures(&db, f).into_iter().map(|it| Cx::new(&db).captures(it).digest).collect();
    assert_eq!(captures.len(), 1);
    assert_ne!(body(&db, "f").0, owner.0);
    assert_ne!(after, captures);
}

#[test]
fn an_impl_trait_s_auto_traits_are_the_fn_s_interface() {
    let (mut db, file) = single(
        r#"
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
pub trait Tr {}
pub struct S<T> { a: T }
impl<T> Tr for S<T> {}
pub fn f() -> impl Tr { S { a: 0u8 } }
"#,
    );
    let f = item(&db, "f");
    let (before, auto_before) = (interface(&db, "f"), Cx::new(&db).auto_traits(f, 0));
    assert_eq!(auto_before.missed, None);
    edit(
        &mut db,
        file,
        r#"
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
pub trait Tr {}
pub struct S<T> { a: T }
impl<T> Tr for S<T> {}
pub fn f() -> impl Tr { S { a: NoMarker } }
"#,
    );
    let f = item(&db, "f");
    assert_ne!(Cx::new(&db).auto_traits(f, 0).digest, auto_before.digest);
    assert_ne!(interface(&db, "f").0, before.0);
}

#[test]
fn an_async_fn_s_future_auto_traits_are_its_interface() {
    let (mut db, file) = single(
        r#"
//- minicore: future, sized
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
async fn yielding() {}
pub async fn f() {
    let kept = 0u8;
    yielding().await;
    let _moved = kept;
}
"#,
    );
    let f = item(&db, "f");
    let (before, auto_before) = (interface(&db, "f"), Cx::new(&db).auto_traits(f, 0));
    assert_eq!(auto_before.missed, None);
    edit(
        &mut db,
        file,
        r#"
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
async fn yielding() {}
pub async fn f() {
    let kept = NoMarker;
    yielding().await;
    let _moved = kept;
}
"#,
    );
    let f = item(&db, "f");
    assert_ne!(Cx::new(&db).auto_traits(f, 0).digest, auto_before.digest);
    assert_ne!(interface(&db, "f").0, before.0);
}

#[test]
fn a_const_fn_s_body_is_its_interface_and_a_fn_s_is_not() {
    let (mut db, file) = single(
        r#"
const fn c() -> u8 { 1 }
fn p() -> u8 { 1 }
"#,
    );
    let (c, p, p_body) = (interface(&db, "c"), interface(&db, "p"), body(&db, "p"));
    edit(
        &mut db,
        file,
        r#"
const fn c() -> u8 { 2 }
fn p() -> u8 { 2 }
"#,
    );
    assert_ne!(interface(&db, "c").0, c.0);
    assert_eq!(interface(&db, "p"), p);
    assert_ne!(body(&db, "p").0, p_body.0);
}

#[test]
fn type_name_spells_the_type_s_name() {
    let (mut db, file) = single(
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct A;
fn f() -> &'static str { intrinsics::type_name::<A>() }
"#,
    );
    let before = body(&db, "f");
    edit(
        &mut db,
        file,
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct B;
fn f() -> &'static str { intrinsics::type_name::<B>() }
"#,
    );
    assert_ne!(body(&db, "f").0, before.0);
}

#[test]
fn a_no_mangle_fn_s_name_is_its_interface() {
    let (mut db, file) = single(
        r#"
#[no_mangle]
pub extern "C" fn f() {}
"#,
    );
    let before = interface(&db, "f");
    edit(
        &mut db,
        file,
        r#"
#[no_mangle]
pub extern "C" fn g() {}
"#,
    );
    assert_ne!(interface(&db, "g").0, before.0);
}
