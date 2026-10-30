//! What type lowering reports and builds, through the diagnostics a check collects: lifetimes
//! left out (one diagnostic per rustc error), lifetime arguments to late-bound lifetimes,
//! ambiguous associated items, what an unresolved associated item was looked up on, the
//! names a `for<'a>` binds, and the signature columns lowering builds (const parameter types,
//! defaults, variances, unused parameters, supertraits).

use hir_def::ModuleDefId;
use hir_ide::{
    TyDefId,
    db::HirDatabase,
    next_solver::{BoundVariableKind, DbInterner, Symbol, Ty},
};
use rustc_type_ir::{
    BoundRegionKind, BoundVarIndexKind, RegionKind, TyKind, UintTy, inherent::IntoKind as _,
};
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

#[test]
fn a_lifetime_is_the_innermost_binders_that_names_it() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
type G = for<'a> fn(&'a u8, for<'a, 'b> fn(&'a u8, &'b u8));
"#,
    );
    crate::attach_db(&db, || {
        let (_, ModuleDefId::TypeAliasId(alias)) = type_item(&db, "G") else {
            panic!("`G` is a type alias")
        };
        let ty = db.ty(TyDefId::TypeAliasId(alias)).skip_binder();
        let TyKind::FnPtr(outer, _) = ty.kind() else { panic!("`G` is a fn pointer: {ty:?}") };
        let outer = outer.skip_binder().inputs();
        let TyKind::FnPtr(inner, _) = outer[1].kind() else { panic!("a fn pointer: {ty:?}") };
        let inner = inner.skip_binder().inputs();
        // (binder, var) of a reference's region, the binder counted from the innermost.
        let bound = |ty: Ty<'_>| match ty.kind() {
            TyKind::Ref(region, ..) => match region.kind() {
                RegionKind::ReBound(BoundVarIndexKind::Bound(binder), it) => {
                    (binder.as_u32(), it.var.as_u32())
                }
                it => panic!("a bound region, not {it:?}"),
            },
            it => panic!("a reference, not {it:?}"),
        };
        assert_eq!(bound(outer[0]), (0, 0), "the outer `'a`");
        assert_eq!(bound(inner[0]), (0, 0), "the inner `'a` shadows the outer");
        assert_eq!(bound(inner[1]), (0, 1), "`'b`");
    });
}

#[test]
fn const_parameters_between_type_parameters_have_their_types() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
struct S<T, const N: usize, U, const M: bool>([T; N], U);
"#,
    );
    crate::attach_db(&db, || {
        let (_, ModuleDefId::AdtId(adt)) = type_item(&db, "S") else { panic!("`S` is an ADT") };
        let def = hir_def::GenericDefId::AdtId(adt);
        let kinds: Vec<_> = hir_def::hir::generics::GenericParams::of(&db, def)
            .iter_type_or_consts()
            .filter(|(_, it)| {
                matches!(it, hir_def::hir::generics::TypeOrConstParamData::ConstParamData(_))
            })
            .map(|(local_id, _)| {
                let id = hir_def::ConstParamId::from_unchecked(hir_def::TypeOrConstParamId {
                    parent: def,
                    local_id,
                });
                db.const_param_ty(id).kind()
            })
            .collect();
        assert!(
            matches!(kinds[..], [TyKind::Uint(UintTy::Usize), TyKind::Bool]),
            "{kinds:?}"
        );
    });
}

#[test]
fn defaults_follow_the_parameters_they_are_of() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
struct S<A, B = u8, C = u16>(A, B, C);
"#,
    );
    crate::attach_db(&db, || {
        let (_, ModuleDefId::AdtId(adt)) = type_item(&db, "S") else { panic!("`S` is an ADT") };
        let defaults = db.generic_defaults(hir_def::GenericDefId::AdtId(adt));
        let has: Vec<bool> = (0..4).map(|idx| defaults.get(idx).is_some()).collect();
        assert_eq!(has, [false, true, true, false]);
    });
}

#[test]
fn the_variances_of_many_parameters() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
struct S<'a, A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R>(&'a A, fn(B), fn(C) -> C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R);
"#,
    );
    crate::attach_db(&db, || {
        use rustc_type_ir::Variance::{Contravariant, Covariant, Invariant};
        let (_, ModuleDefId::AdtId(adt)) = type_item(&db, "S") else { panic!("`S` is an ADT") };
        let variances = db.variances_of(hir_def::GenericDefId::AdtId(adt));
        let mut expected = vec![Covariant, Covariant, Contravariant, Invariant];
        expected.extend(std::iter::repeat_n(Covariant, 15));
        assert_eq!(variances.as_slice(), &expected[..]);
    });
}

#[test]
fn a_parameter_a_projection_constrains_is_used() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
trait T { type X; }
struct S<A, B, C>(A);
struct U<P, Q>(P) where P: T<X = Q>;
"#,
    );
    crate::attach_db(&db, || {
        let adt = |name| match type_item(&db, name) {
            (_, ModuleDefId::AdtId(it)) => it,
            it => panic!("`{name}` is an ADT, not {it:?}"),
        };
        assert_eq!(hir_ide::adt_unused_params(&db, adt("S")).len(), 2);
        assert!(hir_ide::adt_unused_params(&db, adt("U")).is_empty());
    });
}

#[test]
fn the_memo_supertraits_are_what_elaboration_finds() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
trait A {}
trait B: A {}
trait C: A {}
trait D: B + C where Self: Sized {}
trait F<'a> {}
trait E: for<'a> F<'a> {}
trait G where Self: D + E {}
"#,
    );
    crate::attach_db(&db, || {
        let interner = DbInterner::new_no_crate(&db);
        for name in ["A", "B", "C", "D", "E", "F", "G"] {
            let (_, ModuleDefId::TraitId(trait_)) = type_item(&db, name) else {
                panic!("`{name}` is a trait")
            };
            let mut elaborated: Vec<_> =
                rustc_type_ir::elaborate::supertrait_def_ids(interner, trait_.into())
                    .map(|it| it.0)
                    .collect();
            let mut memo = hir_ide::all_super_traits(&db, trait_).to_vec();
            elaborated.sort();
            memo.sort();
            assert_eq!(elaborated, memo, "`{name}`");
        }
    });
}
