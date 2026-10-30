//! Owners of a build's functions and statics, and the refusal of a dependency's code that
//! refers to the workspace's.

use super::*;

fn key(n: u8) -> InstanceKey {
    InstanceKey(ContentId([n; 32]))
}

fn item(n: u8) -> ItemKey {
    ItemKey(ContentId([n + 100; 32]))
}

fn code(n: u8) -> CodeHash {
    CodeHash(ContentId([n + 200; 32]))
}

fn function(n: u8, owner: Owner, refs: &[Target]) -> Function<'_> {
    Function { key: key(n), item: item(n), owner, code: code(n), refs }
}

#[test]
fn an_origin_owns_what_no_type_argument_makes_another_s() {
    assert_eq!(Owner::of(&Origin::Std), Owner::Dependency);
    use crate::plan::Package;
    assert_eq!(Owner::of(&Origin::Dependency(Package::new("regex"))), Owner::Dependency);
    assert_eq!(Owner::of(&Origin::Member(Package::new("app"))), Owner::Workspace);
    assert_eq!(Owner::of(&Origin::BuildTime(Package::new("serde_derive"))), Owner::Workspace);
    assert!(Owner::Dependency < Owner::Workspace, "of two describers, the image's wins");
}

#[test]
fn owners_are_by_key_with_the_generic_s_code() {
    let none: [Target; 0] = [];
    let functions = [function(1, Owner::Workspace, &none), function(2, Owner::Dependency, &none)];
    let static_id = (item(9), ContentId::default());
    let statics = [StaticOf { id: static_id, unit: key(9), owner: Owner::Dependency, refs: &none }];
    let mut errors = Vec::new();
    let owned = owners(functions.into_iter(), statics.into_iter(), &mut errors);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(owned.functions, [(key(1), Owner::Workspace, code(1)), (key(2), Owner::Dependency, code(2))]);
    assert_eq!(owned.statics, [(item(9), ContentId::default(), Owner::Dependency)]);
}

#[test]
fn the_workspace_may_call_a_dependency_and_a_dependency_may_not_call_the_workspace() {
    let calls_dependency = [Target::Direct(key(2)), Target::Extern("memcpy".to_owned())];
    let calls_workspace = [Target::Function(key(1))];
    let none: [Target; 0] = [];
    let mut errors = Vec::new();
    let fine = [function(1, Owner::Workspace, &calls_dependency), function(2, Owner::Dependency, &none)];
    owners(fine.into_iter(), std::iter::empty(), &mut errors);
    assert!(errors.is_empty(), "{errors:?}");

    let wrong = [function(1, Owner::Workspace, &none), function(2, Owner::Dependency, &calls_workspace)];
    owners(wrong.into_iter(), std::iter::empty(), &mut errors);
    assert_eq!(errors.len(), 1);
    assert_eq!((errors[0].unit, errors[0].item), (key(2), item(2)));
    assert!(errors[0].what.contains("the workspace's"), "{}", errors[0].what);
}

#[test]
fn a_dependency_s_static_and_a_reference_to_a_workspace_static_are_checked() {
    let workspace_static = (item(8), ContentId([1; 32]));
    let names_static = [Target::Static { item: workspace_static.0, instance: workspace_static.1 }];
    let names_function = [Target::Direct(key(1))];
    let none: [Target; 0] = [];
    let functions = [function(1, Owner::Workspace, &none), function(2, Owner::Dependency, &names_static)];
    let statics = [
        StaticOf { id: workspace_static, unit: key(8), owner: Owner::Workspace, refs: &none },
        StaticOf { id: (item(9), ContentId::default()), unit: key(9), owner: Owner::Dependency, refs: &names_function },
    ];
    let mut errors = Vec::new();
    owners(functions.into_iter(), statics.into_iter(), &mut errors);
    let named: Vec<_> = errors.iter().map(|it| (it.unit, it.item)).collect();
    assert_eq!(named, [(key(2), item(2)), (key(9), item(9))]);
}
