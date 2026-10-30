use span::Edition;

use super::*;

#[test]
fn lint_ids_are_dense_and_round_trip() {
    let all = lints().iter().chain(internal_lints()).chain(unregistered());
    for (at, lint) in all.enumerate() {
        let id = LintId::of(lint);
        assert_eq!(usize::from(id.0), at, "{}", lint.name);
        assert!(std::ptr::eq(id.lint(), lint), "{}", lint.name);
    }
}

#[test]
fn group_of_is_the_first_group_without_aliases() {
    let store = Store::default();
    for lint in lints() {
        let first = groups().iter().filter(|it| it.alias_of.is_none()).find(|it| it.lints.iter().any(|it| it.name == lint.name));
        assert_eq!(store.group_of(lint), first.map(|it| it.name), "{}", lint.name);
    }
}

#[test]
fn counts_are_rustcs() {
    // 108 of the builtin passes and `foreign_modules`, 141 hardwired.
    assert_eq!(lints().len(), 249);
    assert_eq!(internal_lints().len(), 17);
    assert_eq!(internal_group().lints.len(), 16);
    assert_eq!(unregistered().len(), 2);
    let names: Vec<&str> = groups().iter().map(|it| it.name).collect();
    assert_eq!(
        names,
        [
            "rust_2018_compatibility",
            "rust_2021_compatibility",
            "rust_2024_compatibility",
            "future_incompatible",
            "nonstandard_style",
            "unused",
            "let_underscore",
            "rust_2018_idioms",
            "keyword_idents",
            "refining_impl_trait",
            "deprecated_safe",
            "unknown_or_malformed_diagnostic_attributes",
            "bad_style",
        ]
    );
    assert_eq!(group("unused").unwrap().lints.len(), 24);
    assert_eq!(renamed().len(), 18);
    assert_eq!(removed().len(), 66);
}

#[test]
fn names_are_unique_and_lowercase() {
    let mut seen = std::collections::HashSet::new();
    for lint in lints().iter().chain(internal_lints()).chain(unregistered()) {
        assert!(seen.insert(lint.name), "`{}` twice", lint.name);
        assert_eq!(lint.name, lint.name.to_lowercase());
    }
    for (old, new) in renamed() {
        assert!(lint(old).is_none(), "`{old}` is renamed and a lint");
        assert!(lint(new).is_some(), "`{old}` renamed to no lint `{new}`");
    }
}

#[test]
fn a_lint() {
    let it = lint("dead_code").unwrap();
    assert_eq!(it.default_level, Level::Warn);
    assert_eq!(it.desc, "detect unused, unexported items");
    assert!(!it.report_in_external_macro);
    assert!(lint("unreachable_code").unwrap().report_in_external_macro);
    assert!(lint("unused_crate_dependencies").unwrap().crate_level_only);
    assert!(lint("DEAD_CODE").is_none());
    assert_eq!(named("dead_code"), Named::Lint(it));
    assert_eq!(declared("private_macro_use").unwrap().default_level, Level::Deny);
    assert_eq!(named("private_macro_use"), Named::Unknown);
}

#[test]
fn edition_levels() {
    let it = lint("unsafe_op_in_unsafe_fn").unwrap();
    assert_eq!(it.default_level(Edition::Edition2021), Level::Allow);
    assert_eq!(it.default_level(Edition::Edition2024), Level::Warn);
    let it = lint("static_mut_refs").unwrap();
    assert_eq!(it.default_level(Edition::Edition2015), Level::Warn);
    assert_eq!(it.default_level(Edition::Edition2024), Level::Deny);
}

#[test]
fn future_incompatible() {
    let it = lint("ambiguous_glob_imports").unwrap().future_incompatible.unwrap();
    assert_eq!(it.reason, Reason::FutureReleaseError(114095));
    assert!(it.report_in_deps);
    assert_eq!(
        it.reason.reference(),
        "issue #114095 <https://github.com/rust-lang/rust/issues/114095>"
    );
    assert_eq!(
        it.reason.explanation(Edition::Edition2021),
        "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!"
    );
    let it = lint("bare_trait_objects").unwrap().future_incompatible.unwrap();
    assert_eq!(it.reason.edition(), Some(Edition::Edition2021));
    assert_eq!(
        it.reason.reference(),
        "<https://doc.rust-lang.org/edition-guide/rust-2021/warnings-promoted-to-error.html>"
    );
    assert_eq!(
        it.reason.explanation(Edition::Edition2018),
        "this is accepted in the current edition (Rust 2018) but is a hard error in Rust 2021!"
    );
    assert!(!lint("static_mut_refs").unwrap().future_incompatible.unwrap().explain_reason);
    let groups = |name: &str| group(name).unwrap().lints.iter().any(|it| it.name == "bare_trait_objects");
    assert!(groups("rust_2021_compatibility"));
    assert!(!groups("future_incompatible"));
    assert!(groups("rust_2018_idioms"));
}

#[test]
fn renamed_and_removed() {
    assert_eq!(named("bare_trait_object"), Named::Renamed("bare_trait_objects"));
    assert_eq!(named("raw_pointer_derive"), Named::Removed("using derive with raw pointers is ok"));
    assert_eq!(named("broken_intra_doc_links"), Named::Ignored);
    assert_eq!(named("no_such_lint"), Named::Unknown);
    assert!(matches!(named("nonstandard_style"), Named::Group(_)));
    assert_eq!(group("bad_style").unwrap().alias_of, Some("nonstandard_style"));
    let store = Store::default();
    assert_eq!(store.check("bare_trait_object", None), Checked::Renamed("bare_trait_objects"));
    assert_eq!(
        store.check("raw_pointer_derive", None),
        Checked::Removed("using derive with raw pointers is ok")
    );
}

#[test]
fn check_lint_name() {
    let store = Store::default();
    let dead = lint("dead_code").unwrap();
    assert_eq!(store.check("dead_code", None), Checked::Ok(vec![dead]));
    assert_eq!(store.check("broken_intra_doc_links", None), Checked::Ok(Vec::new()));
    assert_eq!(store.check("dead_cod", None), Checked::NoLint(Some(("dead_code".to_owned(), false))));
    assert_eq!(store.check("DEAD_CODE", None), Checked::NoLint(Some(("dead_code".to_owned(), false))));
    assert_eq!(store.check("foo", Some("bar")), Checked::NoTool);
    assert_eq!(store.check("needless_return", Some("clippy")), Checked::MissingTool);
    assert_eq!(store.check("anything", Some("rustdoc")), Checked::MissingTool);
    assert_eq!(store.check("internal", Some("rustc")), Checked::MissingTool);
    let store = Store { internal: true, registered_tools: Vec::new() };
    assert!(matches!(store.check("internal", Some("rustc")), Checked::Tool(lints, None) if lints.len() == 16));
    assert_eq!(
        store.check("default_hash_type", Some("rustc")),
        Checked::NoLint(Some(("rustc::default_hash_types".to_owned(), false)))
    );
    assert!(store.is_lint_group("warnings"));
    assert!(store.is_lint_group("unused"));
    assert!(!store.is_lint_group("dead_code"));
}
