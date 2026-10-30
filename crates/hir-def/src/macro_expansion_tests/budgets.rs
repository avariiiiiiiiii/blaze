//! Allocation budgets of the expansion queries' memo hits: once a call is expanded, reading its
//! expansion, argument, span map and ast id map again allocates nothing.

use hir_expand::{HirFileId, MacroCallId, MacroCallKind};
use test_fixture::WithFixture;

use super::builtin_errors::macro_calls;
use crate::test_db::TestDB;

#[test]
fn expansion_memo_hits_allocate_nothing() {
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(false);
    let db = TestDB::with_files(
        r#"
//- minicore: derive, clone
macro_rules! make { ($($t:tt)*) => { fn made() { $($t)* } } }
make!(let _x = 1;);
#[derive(Clone)]
struct S { a: u8, b: u16 }
"#,
    );
    let calls = macro_calls(&db);
    assert!(
        calls.iter().any(|call| matches!(call.loc(&db).kind, MacroCallKind::FnLike { .. }))
            && calls.iter().any(|call| matches!(call.loc(&db).kind, MacroCallKind::Derive { .. })),
        "the `macro_rules!` call and the derive: {calls:?}"
    );
    let read = |call: MacroCallId| {
        let expansion = call.parse_macro_expansion(&db);
        let (arg, _, span) = call.macro_arg_considering_derives(&db, &call.loc(&db).kind);
        let file = HirFileId::from(call);
        let _ = file.span_map(&db);
        let _ = file.ast_id_map(&db);
        (std::ptr::from_ref(expansion), std::ptr::from_ref(arg), span)
    };
    let warm: Vec<_> = calls.iter().map(|&call| read(call)).collect();
    for (at, &call) in calls.iter().enumerate() {
        let hit = blaze_alloc_count::assert_at_most(0, "an expansion's memo hits", || read(call));
        assert!(hit == warm[at], "a hit gives the memo's own values");
    }
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(true);
}

/// A new id for `call`: its location one expansion deeper, so its argument and expansion are
/// computed again (a miss) while the file's parse, span map, ast ids and the macro's definition
/// stay warm.
fn fresh(db: &TestDB, call: MacroCallId) -> MacroCallId {
    let loc = call.loc(db);
    loc.def.make_call(db, loc.krate, loc.kind.clone(), loc.ctxt, loc.macro_depth + 1)
}

/// What the argument tree of `call` owns: one allocation per non-empty column, counted as a copy
/// of it costs.
fn arg_columns(db: &TestDB, call: MacroCallId) -> u64 {
    let (tree, _, _) = call.macro_arg_considering_derives(db, &call.loc(db).kind);
    blaze_alloc_count::measure(|| tree.copy()).1.allocations
}

/// A miss of `macro_arg` for a derive's attribute call is the argument tree's columns and salsa's
/// run cost (the same query for a derive of a unit struct, less its tree): the cfg walk's
/// attribute list and inserted leaves are the frame's.
#[test]
fn a_derive_argument_miss_allocates_its_tree_alone() {
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(false);
    let db = TestDB::with_files(
        r#"
//- minicore: derive, clone
#[derive(Clone)]
#[cfg_attr(all(), allow(unused))]
struct Wide { #[cfg(all())] a: u8, b: u16, #[cfg(any())] c: u32, d: (u8, u8) }
#[derive(Clone)]
struct Unit;
"#,
    );
    let mut derives: Vec<_> = macro_calls(&db)
        .into_iter()
        .filter_map(|call| match call.loc(&db).kind {
            MacroCallKind::Derive { ast_id, derive_macro_id, .. } => {
                Some((ast_id.to_ptr(&db).text_range().start(), derive_macro_id))
            }
            _ => None,
        })
        .collect();
    derives.sort_by_key(|&(at, _)| at);
    let [(_, wide), (_, unit)] = derives[..] else { panic!("two derives: {derives:?}") };
    let (wide, unit) = (fresh(&db, wide), fresh(&db, unit));
    let (_, run) = blaze_alloc_count::measure(|| {
        let _ = unit.macro_arg_considering_derives(&db, &unit.loc(&db).kind);
    });
    let salsa = run.allocations.saturating_sub(arg_columns(&db, unit));
    let (_, miss) = blaze_alloc_count::measure(|| {
        let _ = wide.macro_arg_considering_derives(&db, &wide.loc(&db).kind);
    });
    let owned = arg_columns(&db, wide);
    crate::nameres::ENABLE_BUILTIN_DERIVE_FAST_PATH.set(true);
    assert!(
        miss.allocations <= salsa + owned,
        "`macro_arg` of a derive: {} allocations; salsa's run {salsa}, its tree {owned}",
        miss.allocations,
    );
}
