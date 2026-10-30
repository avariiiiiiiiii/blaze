//! What type inference records for the diagnostics that read it: the assignment an invalid
//! left-hand side is of, scope errors of jumps and `yield`, where a body stops being reachable,
//! an operator's unmet predicates and a call's arguments, and the goals left ambiguous.

use hir_def::{
    AdtId,
    expr_store::Body,
    hir::{ArithOp, BinaryOp, Expr, ExprId, ExprOrPatId},
    signatures::{StructSignature, TraitSignature},
};
use hir_ide::{
    InferBodyId, InferenceDiagnostic, InferenceResult, Unreachable, UnreachableAt,
    next_solver::{TyKind, default_types},
    solver_errors::{StoredGoal, TypeAnnotationNeeded},
};
use rustc_type_ir::{
    InferTy,
    inherent::{GenericArgs as _, IntoKind as _},
};
use test_fixture::WithFixture;
use triomphe::Arc;

use super::{db::TestDB, function};

/// The body of the function `name` and what inference found in it.
fn infer<'db>(db: &'db TestDB, name: &str) -> (Arc<Body>, &'db InferenceResult<'db>) {
    let def = function(db, name);
    (Body::of(db, def), InferenceResult::of(db, InferBodyId::DefWithBodyId(def)))
}

/// The first expression of `body` that `pred` holds for.
fn find_expr(body: &Body, pred: impl Fn(&Expr<'_>) -> bool) -> ExprId {
    body.exprs().find(|(_, it)| pred(it)).map(|(id, _)| id).expect("the expression is in the body")
}

const ASSIGN: &str = r#"
//- minicore: sized, add
fn plain() {
    1 = 2;
}
fn compound() {
    let x = 0;
    1 += x;
}
fn place() {
    let mut x = 0;
    x = 1;
    x += 1;
}
"#;

#[test]
fn an_invalid_left_hand_side_names_its_assignment() {
    let db = TestDB::with_files(ASSIGN);
    crate::attach_db(&db, || {
        let (body, result) = infer(&db, "plain");
        let assign = find_expr(&body, |it| matches!(it, Expr::Assignment { .. }));
        let found: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match *it {
                InferenceDiagnostic::InvalidLhsOfAssignment { assign, op, .. } => Some((assign, op)),
                _ => None,
            })
            .collect();
        assert_eq!(found, [(assign, None)], "`=` is E0070");

        let (body, result) = infer(&db, "compound");
        let assign = find_expr(&body, |it| {
            matches!(it, Expr::BinaryOp { op: Some(BinaryOp::Assignment { op: Some(_) }), .. })
        });
        let found: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match *it {
                InferenceDiagnostic::InvalidLhsOfAssignment { assign, op, .. } => Some((assign, op)),
                _ => None,
            })
            .collect();
        assert_eq!(found, [(assign, Some(ArithOp::Add))], "`+=` is E0067");

        let (_, result) = infer(&db, "place");
        assert!(
            !result
                .diagnostics()
                .iter()
                .any(|it| matches!(it, InferenceDiagnostic::InvalidLhsOfAssignment { .. })),
            "a place is assignable"
        );
    });
}

const AMBIGUOUS: &str = r#"
//- minicore: sized
trait Tr {}
impl Tr for u8 {}
impl Tr for u16 {}
fn need<T: Tr>() {}
fn ambiguous() {
    need();
}
fn tainted() {
    let _x: u8 = true;
    need();
}
fn decided() {
    need::<u8>();
}
"#;

/// The `TypeMustBeKnown`s of a body: their codes, and the trait of each ambiguous goal.
fn undecided(db: &TestDB, name: &str) -> Vec<(TypeAnnotationNeeded, Option<String>)> {
    let (_, result) = infer(db, name);
    result
        .diagnostics()
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::TypeMustBeKnown { kind, ambiguity, .. } => {
                let trait_ = ambiguity.as_ref().map(|it| match it.primary.goal {
                    StoredGoal::Trait { trait_, .. } => {
                        TraitSignature::of(db, trait_).name.as_str().to_owned()
                    }
                    ref goal => panic!("the goal is a trait's, not {goal:?}"),
                });
                Some((*kind, trait_))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn an_ambiguous_bound_is_e0283_with_its_goal() {
    let db = TestDB::with_files(AMBIGUOUS);
    crate::attach_db(&db, || {
        // rustc: E0283 "type annotations needed", "cannot satisfy `_: Tr`"; the `T: Sized` of
        // the same variable gives none after it, and writeback none for the variable.
        assert_eq!(
            undecided(&db, "ambiguous"),
            [(TypeAnnotationNeeded::E0283, Some("Tr".to_owned()))]
        );
        // rustc reports no ambiguity of a body with an error (`report_ambiguity_errors`).
        assert!(undecided(&db, "tainted").is_empty());
        assert!(undecided(&db, "decided").is_empty());
    });
}

#[test]
fn a_mismatch_keeps_an_undecided_literal_type() {
    let db = TestDB::with_files(
        r#"
//- minicore: sized
fn literal() {
    let _x: bool = 1;
}
fn decided() {
    let _x: bool = 1u8;
}
"#,
    );
    crate::attach_db(&db, || {
        let at_error = |name| {
            let (_, result) = infer(&db, name);
            let found: Vec<_> = result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::TypeMismatch { at_error, .. } => Some(
                        at_error.as_deref().map(|(expected, found)| (expected.as_ref().kind(), found.as_ref().kind())),
                    ),
                    _ => None,
                })
                .collect();
            found
        };
        // rustc: "expected `bool`, found integer"; after fallback the literal is `i32`.
        let types = default_types();
        assert_eq!(
            at_error("literal"),
            [Some((types.types.bool.kind(), TyKind::Infer(InferTy::FreshIntTy(0))))]
        );
        assert_eq!(at_error("decided"), [None]);
    });
}

/// rustc's `tests/ui/label/label_break_value_unlabeled_break.rs` and
/// `label_break_value_continue.rs`, with a `try` block, whose label is desugared, and a
/// closure between the jump and the labeled block.
const JUMPS: &str = r#"
//- minicore: sized, try, option
//- /main.rs crate:main edition:2021
#![feature(try_blocks)]
fn break_simple() {
    'b: {
        break;
    }
}
fn break_crossing() {
    loop {
        'b: {
            break;
        }
    }
}
fn continue_crossing() {
    loop {
        'b: {
            continue;
        }
    }
}
fn labeled() {
    loop {
        'b: {
            break 'b;
        }
    }
}
fn in_inner_loop() {
    'b: {
        loop {
            break;
        }
    }
}
fn in_try() {
    loop {
        let _: Option<()> = try {
            break;
        };
    }
}
fn in_closure() {
    'b: {
        let _ = || loop {
            break;
        };
    }
}
"#;

#[test]
fn an_unlabeled_jump_in_a_labeled_block_is_e0695() {
    let db = TestDB::with_files(JUMPS);
    crate::attach_db(&db, || {
        let jumps = |name: &str| -> Vec<bool> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match *it {
                    InferenceDiagnostic::UnlabeledJumpInLabeledBlock { is_break, .. } => Some(is_break),
                    _ => None,
                })
                .collect()
        };
        let outside = |name: &str| {
            let (_, result) = infer(&db, name);
            result.diagnostics().iter().any(|it| matches!(it, InferenceDiagnostic::BreakOutsideOfLoop { .. }))
        };
        assert_eq!(jumps("break_simple"), [true]);
        assert!(!outside("break_simple"), "only E0695, as rustc gives");
        assert_eq!(jumps("break_crossing"), [true]);
        assert_eq!(jumps("continue_crossing"), [false]);
        assert!(!outside("continue_crossing"));
        for name in ["labeled", "in_inner_loop", "in_try", "in_closure"] {
            assert_eq!(jumps(name), [], "{name}");
            assert!(!outside(name), "{name}");
        }
    });
}

/// rustc's `tests/ui/coroutine/async-coroutine-issue-67158.rs`.
const ASYNC_YIELD: &str = r#"
//- minicore: sized, future
//- /main.rs crate:main edition:2021
#![feature(coroutines)]
fn in_async_block() {
    let _ = async { yield 1u8 };
}
async fn in_async_fn() {
    yield;
}
fn in_closure_in_async() {
    let _ = async {
        let _ = #[coroutine] || {
            yield 1u8;
        };
    };
}
"#;

#[test]
fn a_yield_in_an_async_body_is_e0727() {
    let db = TestDB::with_files(ASYNC_YIELD);
    crate::attach_db(&db, || {
        let count = |name: &str| {
            let (body, result) = infer(&db, name);
            let found: Vec<ExprId> = result
                .diagnostics()
                .iter()
                .filter_map(|it| match *it {
                    InferenceDiagnostic::AsyncCoroutinesNotSupported { expr } => Some(expr),
                    InferenceDiagnostic::YieldOutsideCoroutine { .. } => panic!("{name}: not E0627"),
                    _ => None,
                })
                .collect();
            for &it in &found {
                assert!(matches!(body.expr(it), Expr::Yield { .. }), "{name}: on the `yield`");
            }
            found.len()
        };
        assert_eq!(count("in_async_block"), 1);
        assert_eq!(count("in_async_fn"), 1);
        assert_eq!(count("in_closure_in_async"), 0, "a `#[coroutine]` closure may yield");
    });
}

/// rustc's `tests/ui/reachable/`: `expr_block.rs`, `unreachable-in-call.rs`, `expr_if.rs`,
/// `expr_match.rs` and `unwarned-match-on-never.rs`, with no macros.
const REACHABLE: &str = r#"
//- minicore: sized, try, option
fn diverge() -> ! { loop {} }
fn get_u8() -> u8 { 1 }
fn call(_: u8, _: u8) {}
fn tail() {
    let _x = {
        return;
        22
    };
}
fn diverge_first() {
    call(diverge(), get_u8());
}
fn diverge_second() {
    call(get_u8(), diverge());
}
fn statements() {
    return;
    let _x = 1;
    let _y = 2;
}
fn if_condition() {
    if { return } {}
}
fn arms_diverge() {
    match () { () if false => return, () => return }
    let _x = 1;
}
fn an_arm_converges() {
    match () { () if false => return, () => () }
    let _x = 1;
}
fn scrutinee() {
    match (return) {
        () => ()
    }
}
fn never(x: !) -> bool {
    match x {}
}
fn question(x: Option<u8>) -> Option<u8> {
    let y = x?;
    Some(y)
}
"#;

#[test]
fn type_checking_finds_where_a_body_stops_being_reachable() {
    let db = TestDB::with_files(REACHABLE);
    crate::attach_db(&db, || {
        let found = |name: &str| -> (Arc<Body>, Vec<Unreachable>) {
            let (body, result) = infer(&db, name);
            (body, result.unreachable().to_vec())
        };
        let after_return = |body: &Body, it: &Unreachable| match it.after {
            ExprOrPatId::ExprId(it) => matches!(body.expr(it), Expr::Return { .. }),
            ExprOrPatId::PatId(_) => false,
        };

        // "unreachable expression": the tail after `return;`.
        let (body, all) = found("tail");
        let literal = find_expr(&body, |it| matches!(it, Expr::Literal(_)));
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::Expr(literal)]);
        assert!(after_return(&body, &all[0]));
        assert_eq!(all[0].note, None);

        // "unreachable expression": an argument after a diverging one, and not the call too.
        let (body, all) = found("diverge_first");
        let outer = find_expr(&body, |it| matches!(it, Expr::Call { args, .. } if args.len() == 2));
        let Expr::Call { args, .. } = body.expr(outer) else { unreachable!() };
        let (first, second) = (args[0], args[1]);
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::Expr(second)]);
        assert_eq!(all[0].after, ExprOrPatId::ExprId(first));

        // "unreachable call": the last argument diverged.
        let (body, all) = found("diverge_second");
        let outer = find_expr(&body, |it| matches!(it, Expr::Call { args, .. } if args.len() == 2));
        let Expr::Call { args, .. } = body.expr(outer) else { unreachable!() };
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::Call(outer)]);
        assert_eq!(all[0].after, ExprOrPatId::ExprId(args[1]));

        // "unreachable statement": the first after `return;`, once.
        let (body, all) = found("statements");
        assert_eq!(
            all.iter().map(|it| it.what).collect::<Vec<_>>(),
            [UnreachableAt::Stmt { block: body.root_expr(), index: 1 }]
        );
        assert!(after_return(&body, &all[0]));

        // "unreachable block in `if` or `while` expression".
        let (body, all) = found("if_condition");
        let if_ = find_expr(&body, |it| matches!(it, Expr::If { .. }));
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::IfBlock(if_)]);
        assert!(after_return(&body, &all[0]));

        // A `match` whose arms all diverge, a guard not counted.
        let (body, all) = found("arms_diverge");
        let match_ = find_expr(&body, |it| matches!(it, Expr::Match { .. }));
        assert_eq!(
            all,
            [Unreachable {
                what: UnreachableAt::Stmt { block: body.root_expr(), index: 1 },
                after: ExprOrPatId::ExprId(match_),
                note: Some(Unreachable::ALL_ARMS_DIVERGE),
            }]
        );
        assert_eq!(found("an_arm_converges").1, []);

        // "unreachable arm": the scrutinee diverged.
        let (body, all) = found("scrutinee");
        let match_ = find_expr(&body, |it| matches!(it, Expr::Match { .. }));
        assert_eq!(
            all.iter().map(|it| it.what).collect::<Vec<_>>(),
            [UnreachableAt::Arm { match_expr: match_, index: 0 }]
        );
        assert!(after_return(&body, &all[0]));

        // An explicit match on `!` is quiet, and so is the `match` of a desugared `?`.
        assert_eq!(found("never").1, []);
        assert_eq!(found("question").1, []);
    });
}

/// rustc's `binop/issue-93927.rs` and `[X; 3] == [X; 3]` (minicore has no array `PartialEq`,
/// so a local wrapper with the same impl), and `-X`.
const OPERATORS: &str = r#"
//- minicore: sized, eq, unary_ops
struct X;
struct W<T>(T);
impl<T: PartialEq> PartialEq for W<T> {
    fn eq(&self, _: &W<T>) -> bool { true }
}
fn nested() {
    let _b: bool = W(X) == W(X);
}
fn own() {
    let _ = X == X;
}
fn neg() {
    let _ = -X;
}
"#;

#[test]
fn an_operator_error_carries_its_unmet_goals() {
    let db = TestDB::with_files(OPERATORS);
    crate::attach_db(&db, || {
        // Each goal as `Trait` and the name of its self type.
        let names = |goals: &[StoredGoal]| -> Vec<(String, String)> {
            goals
                .iter()
                .map(|goal| {
                    let StoredGoal::Trait { trait_, args, .. } = goal else {
                        panic!("the goal is a trait's, not {goal:?}")
                    };
                    let trait_ = TraitSignature::of(&db, *trait_).name.as_str().to_owned();
                    let self_ty = match args.as_ref().type_at(0).kind() {
                        TyKind::Adt(adt, _) => match adt.def_id() {
                            AdtId::StructId(it) => StructSignature::of(&db, it).name.as_str().to_owned(),
                            it => panic!("the self type is a struct, not {it:?}"),
                        },
                        it => panic!("the self type is an ADT, not {it:?}"),
                    };
                    (trait_, self_ty)
                })
                .collect()
        };
        let binary = |name: &str| {
            let (_, result) = infer(&db, name);
            let found: Vec<_> = result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::BinaryOperatorCannotBeApplied { unmet, output, .. } => {
                        Some((names(unmet), output.as_ref().map(|it| it.as_ref().kind())))
                    }
                    _ => None,
                })
                .collect();
            found
        };
        let pair = |trait_: &str, self_ty: &str| (trait_.to_owned(), self_ty.to_owned());
        let types = default_types();

        // rustc: "an implementation of `PartialEq` might be missing for `X`", from the leaf
        // goal of `W<X>: PartialEq`, and the `bool` the comparison is expected to be.
        assert_eq!(
            binary("nested"),
            [(vec![pair("PartialEq", "X")], Some(types.types.bool.kind()))]
        );
        let own: Vec<_> = binary("own").into_iter().map(|(goals, _)| goals).collect();
        assert_eq!(own, [vec![pair("PartialEq", "X")]], "the trait's own goal is its leaf");

        let (_, result) = infer(&db, "neg");
        let found: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::UnaryOperatorCannotBeApplied { unmet, .. } => Some(names(unmet)),
                _ => None,
            })
            .collect();
        assert_eq!(found, [vec![pair("Neg", "X")]]);
    });
}

/// rustc's `binop/binop-typeck.rs` ("cannot add `{integer}` to `bool`"), E0614 of `*1` and
/// E0600 of `!1.0`: the literal's type is undecided when the error is found.
const AT_ERROR: &str = r#"
//- minicore: sized, add, unary_ops
fn add() {
    let _ = true + 1;
}
fn decided() {
    let _ = true + 1u8;
}
fn deref() {
    let _ = *1;
}
fn not() {
    let _ = !1.0;
}
"#;

#[test]
fn an_operator_error_keeps_an_undecided_literal_type() {
    let db = TestDB::with_files(AT_ERROR);
    crate::attach_db(&db, || {
        let types = default_types();
        let fresh_int = TyKind::Infer(InferTy::FreshIntTy(0));
        let at_error = |name: &str| -> Vec<Vec<TyKind<'_>>> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::BinaryOperatorCannotBeApplied { at_error, .. } => {
                        Some(at_error.as_deref().map_or(Vec::new(), |(lhs, rhs)| {
                            vec![lhs.as_ref().kind(), rhs.as_ref().kind()]
                        }))
                    }
                    InferenceDiagnostic::CannotBeDereferenced { at_error, .. }
                    | InferenceDiagnostic::UnaryOperatorCannotBeApplied { at_error, .. } => {
                        Some(at_error.iter().map(|it| it.as_ref().kind()).collect())
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(at_error("add"), [vec![types.types.bool.kind(), fresh_int]]);
        assert_eq!(at_error("decided"), [Vec::new()], "no literal undecided: `None`");
        assert_eq!(at_error("deref"), [vec![fresh_int]]);
        assert_eq!(at_error("not"), [vec![TyKind::Infer(InferTy::FreshFloatTy(0))]]);
    });
}
