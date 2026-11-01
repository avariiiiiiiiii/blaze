//! A shift of two integers is of the HIR evaluator's shape, as an addition is (queue 01397:
//! `by_hir` refused exactly the shifts of `hir-ty-mir`'s `consteval::hir_eval` tests). On a
//! refusal the message has what `Program::of` reads of the body: inference's error flags and
//! diagnostics, and each node's type and adjustments.

use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::ConstSignature};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult, consteval::hir_eval};

const FIXTURE: &str = r#"
const ADD: u8 = 1 + 4;
const SHIFT: u8 = 1 << 4;
const SHIFT_TYPED: u8 = 1u8 << 4u32;
"#;

#[test]
fn a_shift_of_two_integers_is_of_the_shape() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let mut refused = String::new();
        let consts = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .filter_map(|def| match def {
                ModuleDefId::ConstId(it) => Some(it),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(consts.len(), 3);
        for konst in consts {
            let owner = InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(konst));
            if hir_eval::by_hir(&db, owner, None).is_some() {
                continue;
            }
            let name =
                ConstSignature::of(&db, konst).name.as_ref().map(|it| it.as_str().to_owned());
            let infer = InferenceResult::of(&db, owner);
            let (store, root) = owner.store_and_root_expr(&db);
            refused.push_str(&format!(
                "{name:?} refused: has_errors {}, mismatches {}, diagnostics {:?}, root {root:?}\n",
                infer.has_errors,
                infer.has_type_mismatches(),
                infer.diagnostics(),
            ));
            for (id, expr) in store.exprs() {
                refused.push_str(&format!(
                    "  {id:?} {expr:?}: {:?}, adjustments {:?}, method {:?}\n",
                    infer.expr_ty(id),
                    infer.expr_adjustment(id),
                    infer.method_resolution(id),
                ));
            }
        }
        assert!(refused.is_empty(), "{refused}");
    });
}
