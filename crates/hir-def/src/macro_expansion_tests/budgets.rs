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
