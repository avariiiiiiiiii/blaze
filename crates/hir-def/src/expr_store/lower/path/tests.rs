use expect_test::{Expect, expect};
use span::Edition;
use syntax::{
    AstNode as _, AstPtr,
    ast::{self as tag, make},
    view::{self as ast, AstNode as _},
};
use test_fixture::WithFixture;

use crate::{
    expr_store::{
        ExpressionStore,
        lower::{
            ExprCollector, ImplTraitLowering,
            path::{
                RECORD_SEGMENTS, SEGMENT_LOWERING_MAP, hir_segment_to_ast_segment,
                hir_segment_to_ast_segment_view,
            },
            top_of, view_of,
        },
        path::PathIdx,
        pretty,
    },
    nameres::crate_def_map,
    test_db::{TestDB, TestDBExt},
};

fn lower_path(path: ast::Path<'_>) -> (TestDB, ExpressionStore, Option<PathIdx>) {
    let (db, file_id) = TestDB::with_single_file("");
    let krate = db.fetch_test_crate();
    let mut ctx = ExprCollector::new(
        &db,
        crate_def_map(&db, krate).root_module_id(),
        file_id.into(),
        crate::LoweringMode::Analysis,
    );
    let lowered_path = ctx.lower_path_view(path, &mut ImplTraitLowering::Opaque);
    let (store, _) = ctx.store.finish();
    (db, store, lowered_path)
}

/// Lowers the path `text` as a view and hands it to `f`.
fn with_path<R>(text: &str, f: impl FnOnce(ast::Path<'_>) -> R) -> R {
    let handle = make::path_from_text(text);
    let top = top_of(&handle);
    f(view_of(&handle, &top))
}

#[track_caller]
fn check_hir_to_ast(text: &str, ignore_segments: &[&str]) {
    with_path(text, |path| {
        RECORD_SEGMENTS.set(true);
        SEGMENT_LOWERING_MAP.with_borrow_mut(|map| map.clear());
        let _ = lower_path(path).2.expect("failed to lower path");
        let root = path.syntax().ancestors().last().expect("a node is among its ancestors");
        SEGMENT_LOWERING_MAP.with_borrow(|map| {
            for (&segment, &segment_idx) in map {
                let segment_text = segment.to_view(root).syntax().text();
                if ignore_segments.contains(&segment_text) {
                    continue;
                }

                let restored_segment = hir_segment_to_ast_segment_view(path, segment_idx as u32)
                    .unwrap_or_else(|| {
                        panic!(
                            "failed to map back segment `{segment_text}` \
                            numbered {segment_idx} in HIR from path `{text}`"
                        )
                    });
                assert_eq!(
                    segment,
                    AstPtr::<tag::PathSegment>::of(restored_segment),
                    "mapping back `{segment_text}` numbered {segment_idx} in HIR from path \
                    `{text}` produced incorrect segment `{}`",
                    restored_segment.syntax().text()
                );
            }
        });
    });
}

/// The view twin of `hir_segment_to_ast_segment` gives, for every segment number, the node the
/// handle function gives.
#[track_caller]
fn check_segment_twin(text: &str) {
    let handle = make::path_from_text(text);
    let top = top_of(&handle);
    let view = view_of(&handle, &top);
    let segments = handle.syntax().descendants().filter_map(tag::PathSegment::cast).count();
    for segment_idx in 0..=segments as u32 {
        assert_eq!(
            hir_segment_to_ast_segment(&handle, segment_idx).map(|it| AstPtr::new(&it)),
            hir_segment_to_ast_segment_view(view, segment_idx).map(AstPtr::<tag::PathSegment>::of),
            "segment {segment_idx} of `{text}`"
        );
    }
}

/// The handle `lower_path` lowers a path as `lower_path_view` does.
#[track_caller]
fn check_lower_path_twin(text: &str) {
    let handle = make::path_from_text(text);
    let (db, store, lowered) = with_path(text, lower_path);
    let (db_handle, store_handle, lowered_handle) = {
        let (db, file_id) = TestDB::with_single_file("");
        let krate = db.fetch_test_crate();
        let mut ctx = ExprCollector::new(
            &db,
            crate_def_map(&db, krate).root_module_id(),
            file_id.into(),
            crate::LoweringMode::Analysis,
        );
        let lowered = ctx.lower_path(handle, &mut ImplTraitLowering::Opaque);
        let (store, _) = ctx.store.finish();
        (db, store, lowered)
    };
    assert_eq!(
        lowered.map(|it| pretty::print_path(&db, &store, store.path_at(it), Edition::CURRENT)),
        lowered_handle.map(|it| pretty::print_path(
            &db_handle,
            &store_handle,
            store_handle.path_at(it),
            Edition::CURRENT
        )),
        "`{text}`"
    );
}

#[test]
fn segment_twin_gives_the_handles_segments() {
    for text in [
        "<A as B::C::D>::E::F",
        "A::B::C::D::E::F",
        "crate::super::super::A::B::C",
        "self::A::B::C",
        "super::super::super::A::B::C",
        "<A::B>::C::D",
        "A::super::B::super::super::C::D",
        "Fn::(A, B) -> C",
        "a::<B, C = D>::e",
    ] {
        check_segment_twin(text);
        check_lower_path_twin(text);
    }
}

/// A collector made to lower one plain path and dropped unfinished, as an IDE lookup does,
/// allocates nothing once warm: its stacks and the builder's scratch come from the thread's
/// pools and go back when it is dropped, and the path's names and `ModPath` are interner hits.
#[test]
fn a_warm_plain_path_lookup_allocates_nothing() {
    let (db, file_id) = TestDB::with_single_file("");
    let krate = db.fetch_test_crate();
    let module = crate_def_map(&db, krate).root_module_id();
    with_path("a::b::C", |path| {
        let lookup = || {
            let mut ctx =
                ExprCollector::new(&db, module, file_id.into(), crate::LoweringMode::Ide);
            std::hint::black_box(ctx.lower_path_view(path, &mut ImplTraitLowering::Opaque));
        };
        lookup();
        lookup();
        blaze_alloc_count::assert_at_most(0, "a warm plain path lookup", lookup);
    });
}

#[test]
fn hir_to_ast_trait_ref() {
    check_hir_to_ast("<A as B::C::D>::E::F", &["A"]);
}

#[test]
fn hir_to_ast_plain_path() {
    check_hir_to_ast("A::B::C::D::E::F", &[]);
}

#[test]
fn hir_to_ast_crate_path() {
    check_hir_to_ast("crate::A::B::C", &[]);
    check_hir_to_ast("crate::super::super::A::B::C", &[]);
}

#[test]
fn hir_to_ast_self_path() {
    check_hir_to_ast("self::A::B::C", &[]);
    check_hir_to_ast("self::super::super::A::B::C", &[]);
}

#[test]
fn hir_to_ast_super_path() {
    check_hir_to_ast("super::A::B::C", &[]);
    check_hir_to_ast("super::super::super::A::B::C", &[]);
}

#[test]
fn hir_to_ast_type_anchor_path() {
    check_hir_to_ast("<A::B>::C::D", &["A", "B"]);
}

#[test]
fn hir_to_ast_path_super_in_middle() {
    check_hir_to_ast("A::super::B::super::super::C::D", &[]);
}

#[track_caller]
fn check_fail_lowering(text: &str) {
    let (_, _, lowered_path) = with_path(text, lower_path);
    assert!(lowered_path.is_none(), "path `{text}` should fail lowering");
}

#[test]
fn keywords_in_middle_fail_lowering1() {
    check_fail_lowering("self::A::self::B::super::C::crate::D");
}

#[test]
fn keywords_in_middle_fail_lowering2() {
    check_fail_lowering("A::super::self::C::D");
}

#[test]
fn keywords_in_middle_fail_lowering3() {
    check_fail_lowering("A::crate::B::C::D");
}

#[track_caller]
fn check_path_lowering(text: &str, expected: Expect) {
    let (db, store, lowered_path) = with_path(text, lower_path);
    let lowered_path = lowered_path.expect("failed to lower path");
    let buf = pretty::print_path(&db, &store, store.path_at(lowered_path), Edition::CURRENT);
    expected.assert_eq(&buf);
}

#[test]
fn fn_like_path_with_coloncolon() {
    check_path_lowering("Fn::(A, B) -> C", expect![[r#"Fn::<(A, B), Output = C>"#]]);
    check_path_lowering("Fn::(A, B)", expect![[r#"Fn::<(A, B), Output = ()>"#]]);
}
