//! A type that does not lower is an error naming why, as rustc's, never an error type with no
//! diagnostic (queue 01168): E0562, E0225, E0719, E0393, E0220 and E0223 of field types.

use hir_def::{AdtId, ModuleDefId, VariantId, nameres::crate_def_map, signatures::StructSignature};
use hir_expand::fixture;

use crate::{
    AssocTypeFailure, AssocTypeQself, PathLoweringDiagnostic, TyLoweringDiagnostic,
    db::HirDatabase,
};

const FIXTURE: &str = r#"
//- minicore: sized
trait A {}
trait B {}
trait Tr {
    type X;
}
trait D<Rhs = Self> {}
struct V<T: Tr> {
    impl_trait: impl A,
    two_traits: &'static (dyn A + B),
    conflicting: &'static dyn Tr<X = u8, X = u16>,
    self_default: &'static dyn D,
    not_found: T::Missing,
    ambiguous: <u8>::X,
    fine: <T as Tr>::X,
}
"#;

fn diagnostics() -> Vec<TyLoweringDiagnostic> {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let v = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::AdtId(AdtId::StructId(it)) if StructSignature::of(&db, it).name.as_str() == "V" => {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `V`");
        db.field_types_with_diagnostics(VariantId::StructId(v)).diagnostics().to_vec()
    })
}

fn path_diagnostics(diagnostics: &[TyLoweringDiagnostic]) -> Vec<&PathLoweringDiagnostic> {
    diagnostics
        .iter()
        .filter_map(|it| match it {
            TyLoweringDiagnostic::PathDiagnostic { diag, .. } => Some(diag),
            _ => None,
        })
        .collect()
}

#[test]
fn impl_trait_in_a_field_is_e0562() {
    let found = diagnostics();
    assert!(
        found.iter().any(|it| matches!(it, TyLoweringDiagnostic::ImplTraitNotAllowed { .. })),
        "{found:?}",
    );
}

#[test]
fn a_second_non_auto_trait_is_e0225() {
    let found = diagnostics();
    assert!(
        found.iter().any(|it| matches!(
            it,
            TyLoweringDiagnostic::AdditionalNonAutoTraits { additional, .. } if additional.len() == 1
        )),
        "{found:?}",
    );
}

#[test]
fn two_values_of_one_associated_type_are_e0719() {
    let found = diagnostics();
    assert!(
        found.iter().any(|it| matches!(it, TyLoweringDiagnostic::ConflictingAssocTypeBindings { .. })),
        "{found:?}",
    );
}

#[test]
fn a_default_naming_self_left_out_of_an_object_is_e0393() {
    let found = diagnostics();
    assert!(
        found.iter().any(|it| matches!(
            it,
            TyLoweringDiagnostic::MissingSelfDefaultParams { params, .. } if params.len() == 1
        )),
        "{found:?}",
    );
}

#[test]
fn a_shorthand_no_bound_gives_is_e0220() {
    let found = diagnostics();
    assert!(
        path_diagnostics(&found).iter().any(|it| matches!(
            it,
            PathLoweringDiagnostic::AssocTypeNotResolved {
                qself: AssocTypeQself::Param(_),
                failure: AssocTypeFailure::NotFound,
                ..
            }
        )),
        "{found:?}",
    );
}

#[test]
fn an_associated_type_of_a_primitive_is_e0223() {
    let found = diagnostics();
    assert!(
        path_diagnostics(&found)
            .iter()
            .any(|it| matches!(it, PathLoweringDiagnostic::AmbiguousAssocType { .. })),
        "{found:?}",
    );
}

#[test]
fn each_field_but_the_valid_one_has_one_error() {
    let found = diagnostics();
    assert_eq!(found.len(), 6, "one per invalid field, none for `fine`: {found:?}");
}
