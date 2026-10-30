//! Allocation budgets of the checks' no-diagnostic path: a clean crate's signatures are
//! resolved with as many allocations at 8 items as at 64 (a type parameter of the item and a
//! lifetime its binder declares are ruled out from the syntax, before Semantics), and the
//! attribute reads every check makes (a path, one owner's lowering) allocate nothing.

use blaze_alloc_count::{assert_at_most, measure};
use hir::Semantics;

use super::{attrs, defs, features, names};
use crate::{
    cx::Cx,
    lints::syntax::attrs::{Owner, owners},
};

/// `check` given the context of the one crate of `fixture`.
fn with_cx<R>(fixture: &str, check: impl FnOnce(&Cx<'_, '_>) -> R) -> R {
    let checked = crate::tests::checked(fixture);
    let db = &checked.db;
    hir::attach_db(db, || {
        let sema = Semantics::new(db);
        let krate = hir::check::workspace_crates(db)
            .into_iter()
            .find(|it| it.display_name(db).is_some_and(|name| name.to_string() == "main"))
            .expect("the fixture has a crate `main`");
        let frame = crate::tests::frame();
        let cx = Cx::new(&sema, krate, checked.files(), &frame);
        check(&cx)
    })
}

/// A crate of one file with `n` generic items whose signatures name only their own type
/// parameters and lifetimes, and attributes of every common shape.
fn generic_items(n: usize) -> String {
    let mut out = String::from("//- /main.rs crate:main\n");
    for i in 0..n {
        out.push_str(&format!(
            "#[inline]\n#[allow(dead_code)]\npub fn f{i}<'a, T>(x: &'a T, y: T) -> &'a T {{ let _ = y; x }}\n\
             /// Doc.\n#[repr(C)]\npub struct S{i}<'a, T>(&'a T, T);\n"
        ));
    }
    out.push_str("fn main() {}\n");
    out
}

/// The allocations of `names::check` of [`generic_items`]`(n)`, its queries memoized.
fn names_allocations(n: usize) -> u64 {
    with_cx(&generic_items(n), |cx| {
        let scopes = defs::scopes(cx);
        assert!(names::check(cx, &scopes).is_empty(), "the fixture's signatures resolve");
        measure(|| names::check(cx, &scopes)).1.allocations
    })
}

/// Paths of the item's own type parameters and lifetimes its binders declare are ruled out
/// with nothing allocated: what is left is a constant number of tables per scope.
#[test]
fn names_of_a_clean_crate_cost_the_same_at_any_size() {
    let (small, large) = (names_allocations(8), names_allocations(64));
    assert_eq!(small, large, "the clean path's allocations grow with the items");
    assert!(small <= 8, "the clean path allocates {small} times");
}

/// The owners of the attributes of [`generic_items`]`(8)`.
fn with_owners<R>(check: impl FnOnce(&Cx<'_, '_>, &[Owner]) -> R) -> R {
    with_cx(&generic_items(8), |cx| {
        let owners = owners(cx, cx.walk());
        assert!(owners.iter().map(|it| it.attrs.len()).sum::<usize>() >= 32, "the fixture has attributes");
        check(cx, &owners)
    })
}

/// What rustc's tables key an attribute by, and whether it is one name, are read from its
/// syntax with nothing allocated.
#[test]
fn attribute_paths_are_read_without_allocating() {
    with_owners(|_, owners| {
        assert_at_most(0, "an attribute's path, and a test of its name", || {
            for attr in owners.iter().flat_map(|it| &it.attrs) {
                std::hint::black_box(attrs::with_path(attr, |path| path.len()));
                std::hint::black_box(attrs::is(attr, "inline"));
            }
        });
    });
}

/// Attribute parsing lowers each owner's attributes once, for `check_mod_attrs` too: reading
/// an owner's back is a slice of the one list.
#[test]
fn an_owners_lowering_is_read_without_allocating() {
    with_owners(|cx, owners| {
        let (features, _) = features::configured(cx, owners, &features::Flags::default());
        let session = attrs::Session::of(cx, owners, features);
        let mut found = Vec::new();
        let lowered = attrs::parsed(cx, &session, owners, &mut found);
        assert!(found.is_empty(), "the fixture's attributes parse");
        assert!((0..owners.len()).any(|at| !lowered.of(at).is_empty()), "some attribute lowers to something");
        assert_at_most(0, "an owner's lowered attributes", || {
            for at in 0..owners.len() {
                std::hint::black_box(lowered.of(at));
            }
        });
    });
}
