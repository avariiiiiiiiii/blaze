use expect_test::Expect;
use hir_def::{DefWithBodyId, ModuleDefId};
use blaze_query::db::{Database as _, EventKind};
use hir_expand::fixture;

use crate::{InferenceResult, method_resolution::TraitImpls, test_db::{TestDB, TestDBExt as _}};

use super::visit_module;

#[test]
fn typing_whitespace_inside_a_function_should_not_invalidate_types() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    $01 + 1
}",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 1)],
        expect_test::expect![[r#"
            [
                "source_root_crates",
                "crate_local_def_map",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "EditionedFileId::parse",
                "real_span_map",
                "Modules::module",
                "infer_body_query",
                "FunctionSignature::of",
                "FunctionSignature::with_source_map",
                "AttrFlags::query",
                "Body::of",
                "Body::relative",
                "body_syntax",
                "trait_environment_query",
                "lang_items",
                "crate_lang_items",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "ModuleScope::traits",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1
    +
    1
}";

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 0)],
        expect_test::expect![[r#"
            [
                "EditionedFileId::parse",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "real_span_map",
                "AttrFlags::query",
                "FunctionSignature::with_source_map",
                "FunctionSignature::of",
                "body_syntax",
                "Body::relative",
                "Body::of",
            ]
        "#]],
    );
}

#[test]
fn typing_inside_a_function_should_not_invalidate_types_in_another() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> f32 {
    1.0 + 2.0
}
fn bar() -> i32 {
    $01 + 1
}
fn baz() -> i32 {
    1 + 1
}",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 3)],
        expect_test::expect![[r#"
            [
                "source_root_crates",
                "crate_local_def_map",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "EditionedFileId::parse",
                "real_span_map",
                "Modules::module",
                "infer_body_query",
                "FunctionSignature::of",
                "FunctionSignature::with_source_map",
                "AttrFlags::query",
                "Body::of",
                "Body::relative",
                "body_syntax",
                "trait_environment_query",
                "lang_items",
                "crate_lang_items",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "ModuleScope::traits",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
                "infer_body_query",
                "FunctionSignature::of",
                "FunctionSignature::with_source_map",
                "AttrFlags::query",
                "Body::of",
                "Body::relative",
                "body_syntax",
                "trait_environment_query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
                "infer_body_query",
                "FunctionSignature::of",
                "FunctionSignature::with_source_map",
                "AttrFlags::query",
                "Body::of",
                "Body::relative",
                "body_syntax",
                "trait_environment_query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> f32 {
    1.0 + 2.0
}
fn bar() -> i32 {
    53
}
fn baz() -> i32 {
    1 + 1
}
";

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            visit_module(&db, crate_def_map, module, &mut |def| {
                if let ModuleDefId::FunctionId(it) = def {
                    InferenceResult::of(&db, DefWithBodyId::from(it));
                }
            });
        },
        &[("infer_body_query", 1)],
        expect_test::expect![[r#"
            [
                "EditionedFileId::parse",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "real_span_map",
                "AttrFlags::query",
                "FunctionSignature::with_source_map",
                "FunctionSignature::of",
                "body_syntax",
                "Body::relative",
                "AttrFlags::query",
                "FunctionSignature::with_source_map",
                "FunctionSignature::of",
                "body_syntax",
                "Body::relative",
                "Body::of",
                "infer_body_query",
                "ExprScopes::body_expr_scopes",
                "body_upvars_mentioned",
                "AttrFlags::query",
                "FunctionSignature::with_source_map",
                "FunctionSignature::of",
                "body_syntax",
            ]
        "#]],
    );
}

#[test]
fn adding_struct_invalidates_infer() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
            [
                "source_root_crates",
                "crate_local_def_map",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "EditionedFileId::parse",
                "real_span_map",
                "TraitImpls::for_crate",
                "lang_items",
                "crate_lang_items",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub struct NewStruct {
    field: i32,
}
";

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            [
                "EditionedFileId::parse",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "real_span_map",
                "crate_local_def_map",
                "TraitImpls::for_crate",
            ]
        "#]],
    );
}

#[test]
fn adding_enum_query_log() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
            [
                "source_root_crates",
                "crate_local_def_map",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "EditionedFileId::parse",
                "real_span_map",
                "TraitImpls::for_crate",
                "lang_items",
                "crate_lang_items",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub enum SomeEnum {
    A,
    B
}
";

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            [
                "EditionedFileId::parse",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "real_span_map",
                "crate_local_def_map",
                "TraitImpls::for_crate",
            ]
        "#]],
    );
}

#[test]
fn adding_use_query_log() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
            [
                "source_root_crates",
                "crate_local_def_map",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "EditionedFileId::parse",
                "real_span_map",
                "TraitImpls::for_crate",
                "lang_items",
                "crate_lang_items",
            ]
        "#]],
    );

    let new_text = "
use std::collections::HashMap;

fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}
";

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            [
                "EditionedFileId::parse",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "real_span_map",
                "crate_local_def_map",
                "TraitImpls::for_crate",
                "crate_lang_items",
            ]
        "#]],
    );
}

#[test]
fn adding_impl_query_log() {
    let fx = fixture::db(
        "
//- /lib.rs
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub struct SomeStruct {
    field: i32,
}
$0",
    );
    let pos = fx.position();
    let mut db = fx.into_db();
    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 1)],
        expect_test::expect![[r#"
            [
                "source_root_crates",
                "crate_local_def_map",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "EditionedFileId::parse",
                "real_span_map",
                "TraitImpls::for_crate",
                "lang_items",
                "crate_lang_items",
            ]
        "#]],
    );

    let new_text = "
fn foo() -> i32 {
    1 + 1
}

fn bar() -> f32 {
    2.0 * 3.0
}

pub struct SomeStruct {
    field: i32,
}

impl SomeStruct {
    pub fn new(value: i32) -> Self {
        Self { field: value }
    }
}
";

    db.set_file_text(pos.file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(pos.file_id.file_id(&db));
            let _crate_def_map = module.def_map(&db);
            TraitImpls::for_crate(&db, module.krate(&db));
        },
        &[("TraitImpls::for_crate", 0), ("trait_bodies", 0)],
        expect_test::expect![[r#"
            [
                "EditionedFileId::parse",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "real_span_map",
                "crate_local_def_map",
                "TraitImpls::for_crate",
            ]
        "#]],
    );
}

// FIXME(next-solver): does this test make sense with fast path?
#[test]
fn add_struct_invalidates_trait_solve() {
    let fx = fixture::db(
        "
//- /main.rs crate:main
struct SomeStruct;

trait Trait<T> {
    fn method(&self) -> T;
}
impl Trait<u32> for SomeStruct {}

fn main() {
    let s = SomeStruct;
    s.method();
    s.$0
}",
    );
    let file_id = fx.only_file();
    let mut db = fx.into_db();

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            let mut defs: Vec<DefWithBodyId> = vec![];
            visit_module(&db, crate_def_map, module, &mut |it| {
                let def = match it {
                    ModuleDefId::FunctionId(it) => it.into(),
                    ModuleDefId::EnumVariantId(it) => it.into(),
                    ModuleDefId::ConstId(it) => it.into(),
                    ModuleDefId::StaticId(it) => it.into(),
                    _ => return,
                };
                defs.push(def);
            });

            for def in defs {
                let _inference_result = InferenceResult::of(&db, def);
            }
        },
        // FIXME: What does this test check for now? trait_solve_shim is no longer a query
        &[("trait_solve_shim", 0)],
        expect_test::expect![[r#"
            [
                "source_root_crates",
                "crate_local_def_map",
                "file_item_tree_query",
                "HirFileId::ast_id_map",
                "EditionedFileId::parse",
                "real_span_map",
                "Modules::module",
                "TraitItems::query_with_diagnostics",
                "Body::of",
                "Body::with_source_map",
                "AttrFlags::query",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "ImplItems::of",
                "infer_body_query",
                "TraitSignature::of",
                "TraitSignature::with_source_map",
                "AttrFlags::query",
                "FunctionSignature::of",
                "FunctionSignature::with_source_map",
                "AttrFlags::query",
                "Body::of",
                "Body::with_source_map",
                "trait_environment_query",
                "lang_items",
                "crate_lang_items",
                "generic_predicates_of",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "ModuleScope::traits",
                "body_upvars_mentioned",
                "infer_body_query",
                "FunctionSignature::of",
                "FunctionSignature::with_source_map",
                "trait_environment_query",
                "generic_predicates_of",
                "fn_sig_for_fn",
                "ExprScopes::body_expr_scopes",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "StructSignature::of",
                "StructSignature::with_source_map",
                "AttrFlags::query",
                "generic_predicates_of",
                "InherentImpls::in_crate_for_self_ty",
                "InherentImpls::for_crate",
                "TraitImpls::in_crate_and_deps_for_trait_and_self_ty",
                "TraitImpls::for_crate_and_deps",
                "TraitImpls::for_crate",
                "impl_trait_of_impl",
                "ImplSignature::of",
                "ImplSignature::with_source_map",
                "impl_self_ty_of_impl",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "AttrFlags::query",
                "generic_predicates_of",
                "impl_super_outlives",
                "TraitImpls::blanket_in_crate_and_deps",
                "body_upvars_mentioned",
            ]
        "#]],
    );

    let new_text = "
//- /main.rs crate:main
struct AnotherStruct;

struct SomeStruct;

trait Trait<T> {
    fn method(&self) -> T;
}
impl Trait<u32> for SomeStruct {}

fn main() {
    let s = SomeStruct;
    s.method();
    s.$0
}";

    db.set_file_text(file_id.file_id(&db), new_text);

    execute_assert_events(
        &db,
        || {
            let module = db.module_for_file(file_id.file_id(&db));
            let crate_def_map = module.def_map(&db);
            let mut defs: Vec<DefWithBodyId> = vec![];

            visit_module(&db, crate_def_map, module, &mut |it| {
                let def = match it {
                    ModuleDefId::FunctionId(it) => it.into(),
                    ModuleDefId::EnumVariantId(it) => it.into(),
                    ModuleDefId::ConstId(it) => it.into(),
                    ModuleDefId::StaticId(it) => it.into(),
                    _ => return,
                };
                defs.push(def);
            });

            for def in defs {
                let _inference_result = InferenceResult::of(&db, def);
            }
        },
        &[("trait_solve_shim", 0)],
        expect_test::expect![[r#"
            [
                "EditionedFileId::parse",
                "HirFileId::ast_id_map",
                "file_item_tree_query",
                "real_span_map",
                "crate_local_def_map",
                "TraitItems::query_with_diagnostics",
                "Body::with_source_map",
                "AttrFlags::query",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "Body::of",
                "ImplItems::of",
                "AttrFlags::query",
                "TraitSignature::with_source_map",
                "AttrFlags::query",
                "FunctionSignature::with_source_map",
                "FunctionSignature::of",
                "Body::with_source_map",
                "Body::of",
                "ModuleScope::traits",
                "FunctionSignature::with_source_map",
                "infer_body_query",
                "ExprScopes::body_expr_scopes",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "StructSignature::with_source_map",
                "AttrFlags::query",
                "InherentImpls::for_crate",
                "TraitImpls::for_crate",
                "ImplSignature::with_source_map",
                "ImplSignature::of",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "ModuleScope::legacy_macro",
                "ModuleScope::name",
                "AttrFlags::query",
                "body_upvars_mentioned",
            ]
        "#]],
    );
}

fn execute_assert_events(
    db: &TestDB,
    f: impl FnOnce(),
    required: &[(&str, usize)],
    expect: Expect,
) {
    crate::attach_db(db, || {
        let events = db.log(f);
        let executed: Vec<String> = events
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::WillExecute { database_key } => {
                    Some(db.storage().ingredient_name(database_key.ingredient_index()).to_owned())
                }
                _ => None,
            })
            .collect();
        expect.assert_debug_eq(&executed);
        for (event, count) in required {
            let n = executed.iter().filter(|it| it.contains(event)).count();
            assert_eq!(
                n,
                *count,
                "Expected {event} to be executed {count} times, but only got {n}:\n \
             Executed: {executed:#?}\n \
             Event log: {events:#?}",
                events = events
                    .iter()
                    .filter(|event| !matches!(event.kind, EventKind::WillCheckCancellation))
                    .map(|event| { format!("{:?}", event.kind) })
                    .collect::<Vec<_>>(),
            );
        }
    });
}
