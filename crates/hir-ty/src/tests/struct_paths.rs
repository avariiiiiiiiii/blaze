//! A struct literal's or pattern's path that resolves to something it can't be is an error
//! naming what it found, as rustc's resolver (E0574, E0532) and `check_struct_path` (E0071)
//! give it, never an error type with no diagnostic (queue 01168).

use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceDiagnostic, InferenceResult, StructPathDef, StructPathFound};

const FIXTURE: &str = r#"
//- minicore: sized
enum E { A { x: u8 }, B(u8) }
struct S { x: u8 }
type Alias = E;
type Int = u8;
trait Tr {}
static ST: u8 = 0;
fn f() {}
mod m {}
fn enum_literal() { let _ = E {}; }
fn alias_to_enum() { let _ = Alias {}; }
fn alias_to_builtin() { let _ = Int {}; }
fn builtin_literal() { let _ = u8 {}; }
fn trait_literal() { let _ = Tr {}; }
fn module_literal() { let _ = m {}; }
fn no_such_variant() { let _ = E::C {}; }
fn after_a_variant() { let _ = E::A::X {}; }
fn associated_type_of_a_struct() { let _ = S::X {}; }
fn record_struct_in_a_tuple_pattern(s: S) { let S(_) = s; }
fn function_in_a_tuple_pattern() { match 0u8 { f(_) => {} _ => {} } }
fn static_in_a_path_pattern() { match 0u8 { crate::ST => {} _ => {} } }
fn fine(e: E) {
    let _ = S { x: 0 };
    let _ = E::A { x: 0 };
    match e { E::B(_) => {} _ => {} }
}
"#;

/// What each `NotAStructPath` of the fixture's function `name` found, with whether it has a
/// type.
fn found(name: &str) -> Vec<(StructPathFound, bool)> {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let function = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it) if FunctionSignature::of(&db, it).name.as_str() == name => Some(it),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the fixture's `{name}`"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)));
        infer
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::NotAStructPath { found, ty, .. } => Some((*found, ty.is_some())),
                _ => None,
            })
            .collect()
    })
}

#[track_caller]
fn check(name: &str, expected: StructPathFound) {
    let with_ty = expected == StructPathFound::Ty;
    assert_eq!(found(name), [(expected, with_ty)], "`{name}`");
}

#[test]
fn an_enum_as_a_struct_literal_is_e0574() {
    check("enum_literal", StructPathFound::Def(StructPathDef::Enum));
}

#[test]
fn an_alias_to_an_enum_is_e0071() {
    check("alias_to_enum", StructPathFound::Ty);
}

#[test]
fn an_alias_to_a_builtin_is_e0071() {
    check("alias_to_builtin", StructPathFound::Ty);
}

#[test]
fn a_builtin_a_trait_and_a_module_are_e0574() {
    check("builtin_literal", StructPathFound::Def(StructPathDef::BuiltinType));
    check("trait_literal", StructPathFound::Def(StructPathDef::Trait));
    check("module_literal", StructPathFound::Def(StructPathDef::Module));
}

#[test]
fn a_missing_variant_is_e0599() {
    check("no_such_variant", StructPathFound::NoVariant { segment: 1 });
}

#[test]
fn a_segment_after_a_variant_is_e0433() {
    check("after_a_variant", StructPathFound::AfterVariant { segment: 2 });
}

#[test]
fn an_associated_type_of_a_struct_is_e0223() {
    check("associated_type_of_a_struct", StructPathFound::AssocType { segment: 1 });
}

#[test]
fn a_record_struct_or_function_as_a_tuple_struct_pattern_is_e0532() {
    check("record_struct_in_a_tuple_pattern", StructPathFound::Def(StructPathDef::Struct));
    check("function_in_a_tuple_pattern", StructPathFound::Def(StructPathDef::Function));
}

#[test]
fn a_static_as_a_path_pattern_is_e0532() {
    check("static_in_a_path_pattern", StructPathFound::Def(StructPathDef::Static));
}

#[test]
fn struct_literals_and_patterns_that_resolve_have_none() {
    let found = found("fine");
    assert!(found.is_empty(), "{found:?}");
}
