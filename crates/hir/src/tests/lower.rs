//! What type lowering reports and builds, through the diagnostics a check collects: lifetimes
//! left out (one diagnostic per rustc error), lifetime arguments to late-bound lifetimes,
//! ambiguous associated items, what an unresolved associated item was looked up on, and the
//! names a `for<'a>` binds.

use hir_def::ModuleDefId;
use hir_ide::{
    TyDefId,
    db::HirDatabase,
    next_solver::{BoundVariableKind, Symbol},
};
use rustc_type_ir::{BoundRegionKind, TyKind, inherent::IntoKind as _};
use test_fixture::WithFixture;

use super::{db::TestDB, type_item};
use crate::{AnyDiagnostic, MissingLifetimeKind, MissingLifetimeSite, Variant};

/// Every diagnostic the check of the workspace found, given to `f`.
fn check(fixture: &str, f: impl FnOnce(&TestDB, &[AnyDiagnostic<'_>])) {
    let db = TestDB::with_files(fixture);
    crate::attach_db(&db, || {
        let found = crate::check::check_workspace(&db, false);
        let found: Vec<_> = found.into_iter().flat_map(|(_, it)| it).collect();
        f(&db, &found);
    });
}

/// The kind of each `MissingLifetimes`, and what each of its sites is.
fn missing_lifetimes(
    found: &[AnyDiagnostic<'_>],
) -> Vec<(MissingLifetimeKind, Vec<&'static str>)> {
    found
        .iter()
        .filter_map(|it| match it {
            AnyDiagnostic::MissingLifetimes(it) => Some(it),
            _ => None,
        })
        .map(|it| {
            let sites = it.sites.iter().map(|site| match site {
                MissingLifetimeSite::Reference(_) => "&",
                MissingLifetimeSite::Placeholder(_) => "'_",
                MissingLifetimeSite::Path { .. } => "path",
            });
            (it.kind, sites.collect())
        })
        .collect()
}

#[test]
fn a_return_type_with_nothing_to_elide_to_is_one_error_for_all_its_lifetimes() {
    check(
        r#"
//- minicore: sized
fn none() -> &u8 { loop {} }
fn two(a: &u8, b: &u8) -> (&u8, &'_ u8) { loop {} }
fn one(a: &u8) -> (&u8, &u8) { loop {} }
fn ptr(f: fn(&u8, &u8) -> &u8) {}
"#,
        |_, found| {
            use MissingLifetimeKind::ElisionFailure;
            assert_eq!(
                missing_lifetimes(found),
                [
                    (ElisionFailure, vec!["&"]),
                    (ElisionFailure, vec!["&", "'_"]),
                    (ElisionFailure, vec!["&"]),
                ],
                "`one` elides to its argument: {found:#?}"
            );
        },
    );
}

#[test]
fn a_lifetime_left_out_of_an_item_is_an_error_per_site() {
    check(
        r#"
//- minicore: sized
struct S { a: &u8, b: &'_ u8 }
type T = &str;
static OK: &str = "";
"#,
        |_, found| {
            use MissingLifetimeKind::Missing;
            let mut found = missing_lifetimes(found);
            found.sort_by_key(|(_, sites)| sites.clone());
            assert_eq!(
                found,
                [(Missing, vec!["&"]), (Missing, vec!["&"]), (Missing, vec!["'_"])],
                "a static's `&str` is `'static`"
            );
        },
    );
}

#[test]
fn an_anonymous_lifetime_in_a_bound_is_not_allowed() {
    check(
        r#"
//- minicore: sized
trait Tr<'a> {}
struct S<T: Tr<'_>>(T);
"#,
        |_, found| {
            let not_allowed = (MissingLifetimeKind::NotAllowed, vec!["'_"]);
            assert_eq!(missing_lifetimes(found), [not_allowed], "{found:#?}");
        },
    );
}

#[test]
fn a_lifetime_argument_to_a_late_bound_lifetime_is_an_error_on_a_path() {
    check(
        r#"
//- minicore: sized
fn late<'a>(x: &'a u8) {}
fn early<'a: 'a>(x: &'a u8) {}
fn main() {
    late::<'static>(&0);
    early::<'static>(&0);
}
"#,
        |_, found| {
            let late: Vec<_> = found
                .iter()
                .filter_map(|it| match it {
                    AnyDiagnostic::LateBoundLifetimeArgs(it) => Some(it.hard_error),
                    _ => None,
                })
                .collect();
            assert_eq!(late, [true], "only `late` has a late-bound lifetime: {found:#?}");
            assert!(
                !found.iter().any(|it| matches!(it, AnyDiagnostic::IncorrectGenericsLen(_))),
                "the late-bound lifetime is not counted: {found:#?}"
            );
        },
    );
}

#[test]
fn self_naming_a_variant_and_an_associated_type_is_ambiguous() {
    check(
        r#"
//- minicore: sized
enum E { V }
trait Tr { type V; fn f() -> Self::V; }
impl Tr for E {
    type V = u8;
    fn f() -> Self::V { 0 }
}
"#,
        |db, found| {
            let ambiguous: Vec<_> = found
                .iter()
                .filter_map(|it| match it {
                    AnyDiagnostic::AmbiguousAssociatedItem(it) => Some(it),
                    _ => None,
                })
                .collect();
            assert_eq!(ambiguous.len(), 1, "{found:#?}");
            let it = ambiguous[0];
            assert_eq!(it.variant.name(db).as_str(), "V");
            assert_eq!(it.item.name(db).as_str(), "V");
            assert_eq!(it.bound.name(db).as_str(), "Tr");
            assert!(it.self_ty.as_adt().is_some(), "the self type is `E`");
        },
    );
}

#[test]
fn an_unresolved_associated_item_names_the_type_it_was_looked_up_on() {
    check(
        r#"
//- minicore: sized
struct W<T>(T);
struct P { a: u8 }
fn main() {
    W::nope();
    let _ = P {};
}
"#,
        |db, found| {
            let unresolved: Vec<_> = found
                .iter()
                .filter_map(|it| match it {
                    AnyDiagnostic::UnresolvedAssocItem(it) => Some(it),
                    _ => None,
                })
                .collect();
            assert_eq!(unresolved.len(), 1, "{found:#?}");
            assert_eq!(unresolved[0].name.as_str(), "nope");
            assert!(unresolved[0].ty.as_adt().is_some(), "looked up on `W<_>`");

            let missing = found.iter().find_map(|it| match it {
                AnyDiagnostic::MissingFields(it) => Some(it),
                _ => None,
            });
            let missing = missing.expect("`P {}` misses `a`");
            assert!(matches!(missing.variant, Variant::Struct(it) if it.name(db).as_str() == "P"));
        },
    );
}

#[test]
fn the_placeholders_of_a_type_the_body_infers_are_one_error() {
    check(
        r#"
//- minicore: sized
fn ret() -> (_, _) { (1u8, 2u8) }
const C: [_; 2] = [0u8; 2];
struct S { a: _, b: _ }
"#,
        |_, found| {
            let mut counts: Vec<usize> = found
                .iter()
                .filter_map(|it| match it {
                    AnyDiagnostic::InferVarsNotAllowed(it) => {
                        assert_eq!(it.node, it.nodes[0]);
                        Some(it.nodes.len())
                    }
                    _ => None,
                })
                .collect();
            counts.sort_unstable();
            assert_eq!(counts, [1, 1, 1, 2], "a field's placeholders are one each: {found:#?}");
        },
    );
}

#[test]
fn the_leading_segments_of_a_path_are_one_error() {
    check(
        r#"
//- minicore: sized
mod a { pub mod b { pub struct S; } }
fn f(_: a::<u8>::b::<u8>::S) {}
fn g<T>(_: T::<u8>) {}
"#,
        |_, found| {
            let mut groups: Vec<usize> = found
                .iter()
                .filter_map(|it| match it {
                    AnyDiagnostic::GenericArgsProhibited(it) => {
                        assert_eq!(it.args, it.all_args[0]);
                        Some(it.all_args.len())
                    }
                    _ => None,
                })
                .collect();
            groups.sort_unstable();
            assert_eq!(groups, [1, 2], "{found:#?}");
        },
    );
}

#[test]
fn a_for_lifetime_binds_its_name() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
type F = for<'x, 'y> fn(&'x u8, &'y u8);
"#,
    );
    crate::attach_db(&db, || {
        let (_, ModuleDefId::TypeAliasId(alias)) = type_item(&db, "F") else {
            panic!("`F` is a type alias")
        };
        let ty = db.ty(TyDefId::TypeAliasId(alias)).skip_binder();
        let TyKind::FnPtr(sig_tys, _) = ty.kind() else { panic!("`F` is a fn pointer: {ty:?}") };
        let names: Vec<_> = sig_tys
            .bound_vars()
            .iter()
            .map(|it| match it {
                BoundVariableKind::Region(BoundRegionKind::NamedForPrinting(Symbol(name))) => {
                    name.as_str().trim_start_matches('\'').to_owned()
                }
                it => panic!("a `for<'a>` binds a named region, not {it:?}"),
            })
            .collect();
        assert_eq!(names, ["x", "y"]);
    });
}
