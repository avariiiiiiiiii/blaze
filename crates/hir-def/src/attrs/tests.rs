use std::{convert::Infallible, ops::ControlFlow};

use base_db::SourceDatabase;
use hir_expand::{
    Lookup,
    attrs::{AstKeyValueMetaExt, AstPathExt},
};
use intern::Symbol;
use la_arena::{Idx, RawIdx};
use span::Edition;
use syntax::{
    AstNode, SourceFile,
    ast::{self, HasName},
};
use test_fixture::WithFixture;

use super::{
    AttrFlags, Completions, DocAtom, DocWord, LinkKey, LinkKind, LinkModifier, MacroStyle,
    NativeLib, NativeLibKind, ReprWord, SkipDuringMethodDispatch, Word, extract_ra_completions,
    extract_ra_macro_style, extract_rustc_skip_during_method_dispatch, link_cfg,
    match_attr_flags, parse_link_tt, parse_repr_tt, symbol_attr, word,
};
use crate::{
    AdtId, AttrDefId, FieldId, FunctionId, ModuleDefId, VariantId, nameres::crate_def_map,
    src::HasSource, test_db::TestDB,
};

#[test]
fn crate_attrs() {
    let fixture = r#"
//- /lib.rs crate:foo crate-attr:no_std crate-attr:cfg(target_arch="x86")
        "#;
    let (db, file_id) = TestDB::with_single_file(fixture);
    let module = db.module_for_file(file_id.file_id(&db));
    let attrs = AttrFlags::query(&db, AttrDefId::ModuleId(module));
    assert!(attrs.contains(AttrFlags::IS_NO_STD | AttrFlags::HAS_CFG));
}

/// Each member of every name set found at its tag, and the near misses of each (a byte short or
/// over, another case, `r#`) found as a linear search of the texts finds them.
macro_rules! check_name_set {
    ($Enum:ident) => {{
        assert_eq!($Enum::ALL.len(), $Enum::TEXTS.len());
        for (at, (&text, &member)) in $Enum::TEXTS.iter().zip($Enum::ALL).enumerate() {
            assert_eq!(member as usize, at, "{text}");
            assert_eq!($Enum::of(text), Some(member), "{text}");
            assert_eq!($Enum::of(&text.to_owned()), Some(member), "{text}");
        }
        let model = |name: &str| {
            $Enum::TEXTS.iter().position(|it| *it == name).map(|at| $Enum::ALL[at])
        };
        for &text in $Enum::TEXTS {
            let near = [
                text[..text.len() - 1].to_owned(),
                text[1..].to_owned(),
                format!("{text}x"),
                format!("x{text}"),
                format!("r#{text}"),
                text.to_uppercase(),
                text.to_lowercase(),
                text.replace('_', "-"),
                text.replace('-', "_"),
            ];
            for near in &near {
                assert_eq!($Enum::of(near), model(near), "{near}");
            }
        }
        for other in ["", "a", "_", "-", "ünused", "allow", "test", "cfg_attr"] {
            assert_eq!($Enum::of(other), model(other), "{other}");
        }
    }};
}

#[test]
fn every_name_set_finds_its_members_and_only_them() {
    check_name_set!(Word);
    check_name_set!(DocWord);
    check_name_set!(Completions);
    check_name_set!(MacroStyle);
    check_name_set!(SkipDuringMethodDispatch);
    check_name_set!(ReprWord);
    check_name_set!(LinkKey);
    check_name_set!(LinkKind);
    check_name_set!(LinkModifier);
}

#[test]
fn every_word_is_found_at_its_tag() {
    for (at, (&text, &found)) in Word::TEXTS.iter().zip(Word::ALL).enumerate() {
        assert_eq!(found as usize, at, "{text}");
        assert_eq!(word(text), Some(found), "{text}");
    }
}

#[test]
fn other_names_are_no_word() {
    for name in [
        "",
        "a",
        "r#inline",
        "inlines",
        "inlin",
        "Inline",
        "rustc",
        "rustc_",
        "rustc_allocator_zero",
        "rustc_allocator_zeroed_",
        "allow",
        "cfg_attr",
        "test",
        "derive",
        "rustc_const_stable",
        "rustc_on_unimplemented",
        "ünused",
    ] {
        assert_eq!(word(name), None, "{name}");
    }
    let model = |name: &str| Word::TEXTS.iter().position(|it| *it == name).map(|at| Word::ALL[at]);
    for &text in Word::TEXTS {
        let (after, before) = (format!("{text}x"), format!("x{text}"));
        for near in [&text[..text.len() - 1], &text[1..], after.as_str(), before.as_str()] {
            assert_eq!(word(near), model(near), "{near}");
        }
    }
}

/// The flags of one attribute, as the walk before the word index computed them: by string
/// `match`, through `AstPathExt`'s `SmolStr`s, `#[inline]`'s text and [`DocAtom`].
fn old_match_attr_flags(attr_flags: &mut AttrFlags, attr: ast::Meta) -> ControlFlow<Infallible> {
    fn old_extract_doc_tt_attr(attr_flags: &mut AttrFlags, tt: ast::TokenTree) {
        for atom in DocAtom::parse(tt) {
            match atom {
                DocAtom::Flag(flag) => match &*flag {
                    "notable_trait" => attr_flags.insert(AttrFlags::IS_DOC_NOTABLE_TRAIT),
                    "hidden" => attr_flags.insert(AttrFlags::IS_DOC_HIDDEN),
                    _ => {}
                },
                DocAtom::KeyValue { key, value: _ } => match &*key {
                    "alias" => attr_flags.insert(AttrFlags::HAS_DOC_ALIASES),
                    "keyword" => attr_flags.insert(AttrFlags::HAS_DOC_KEYWORD),
                    _ => {}
                },
                DocAtom::Alias(_) => attr_flags.insert(AttrFlags::HAS_DOC_ALIASES),
            }
        }
    }

    match attr {
        ast::Meta::CfgMeta(_) => attr_flags.insert(AttrFlags::HAS_CFG),
        ast::Meta::KeyValueMeta(attr) => {
            let Some(key) = attr.path().as_one_segment() else { return ControlFlow::Continue(()) };
            match &*key {
                "deprecated" => attr_flags.insert(AttrFlags::IS_DEPRECATED),
                "ignore" => attr_flags.insert(AttrFlags::IS_IGNORE),
                "lang" => attr_flags.insert(AttrFlags::LANG_ITEM),
                "must_use" => attr_flags.insert(AttrFlags::IS_MUST_USE),
                "path" => attr_flags.insert(AttrFlags::HAS_PATH),
                "unstable" => attr_flags.insert(AttrFlags::IS_UNSTABLE),
                "rustc_reservation_impl" => attr_flags.insert(AttrFlags::RUSTC_RESERVATION_IMPL),
                "export_name" => {
                    if let Some(value) = attr.value_string()
                        && *value == *"main"
                    {
                        attr_flags.insert(AttrFlags::IS_EXPORT_NAME_MAIN);
                    }
                }
                _ => {}
            }
        }
        ast::Meta::TokenTreeMeta(attr) => {
            let (Some((first_segment, second_segment)), Some(tt)) =
                (attr.path().as_up_to_two_segment(), attr.token_tree())
            else {
                return ControlFlow::Continue(());
            };
            match second_segment {
                None => match &*first_segment {
                    "deprecated" => attr_flags.insert(AttrFlags::IS_DEPRECATED),
                    "used" => attr_flags.insert(AttrFlags::USED),
                    "inline" if !tt.syntax().text().contains_char('n') => {
                        attr_flags.insert(AttrFlags::IS_INLINE);
                        if tt.syntax().text().to_string().contains("always") {
                            attr_flags.insert(AttrFlags::INLINE_ALWAYS)
                        }
                    }
                    "inline" if tt.syntax().text().to_string().contains("never") => {
                        attr_flags.insert(AttrFlags::INLINE_NEVER)
                    }
                    "doc" => old_extract_doc_tt_attr(attr_flags, tt),
                    "repr" | "rustc_scalable_vector" => attr_flags.insert(AttrFlags::HAS_REPR),
                    "target_feature" => attr_flags.insert(AttrFlags::HAS_TARGET_FEATURE),
                    "proc_macro_derive" | "rustc_builtin_macro" => {
                        attr_flags.insert(AttrFlags::IS_DERIVE_OR_BUILTIN_MACRO)
                    }
                    "unstable" => attr_flags.insert(AttrFlags::IS_UNSTABLE),
                    "rustc_layout_scalar_valid_range_start"
                    | "rustc_layout_scalar_valid_range_end" => {
                        attr_flags.insert(AttrFlags::RUSTC_LAYOUT_SCALAR_VALID_RANGE)
                    }
                    "rustc_legacy_const_generics" => {
                        attr_flags.insert(AttrFlags::HAS_LEGACY_CONST_GENERICS)
                    }
                    "rustc_skip_during_method_dispatch" => {
                        extract_rustc_skip_during_method_dispatch(attr_flags, tt)
                    }
                    "rustc_deprecated_safe_2024" => {
                        attr_flags.insert(AttrFlags::RUSTC_DEPRECATED_SAFE_2024)
                    }
                    "rustc_must_implement_one_of" => {
                        attr_flags.insert(AttrFlags::HAS_RUSTC_MUST_IMPLEMENT_ONE_OF)
                    }
                    _ => {}
                },
                Some(second_segment) => match &*first_segment {
                    "rust_analyzer" => match &*second_segment {
                        "completions" => extract_ra_completions(attr_flags, tt),
                        "macro_style" => extract_ra_macro_style(attr_flags, tt),
                        "define_opaque" => attr_flags.insert(AttrFlags::HAS_DEFINE_OPAQUE),
                        _ => {}
                    },
                    _ => {}
                },
            }
        }
        ast::Meta::PathMeta(attr) => {
            let is_test = attr.path().is_some_and(|path| {
                let Some(segment1) = (|| path.segment()?.name_ref())() else { return false };
                let segment2 = path.qualifier();
                let segment3 = segment2.as_ref().and_then(|it| it.qualifier());
                let segment4 = segment3.as_ref().and_then(|it| it.qualifier());
                let segment3 = segment3.and_then(|it| it.segment()?.name_ref());
                let segment4 = segment4.and_then(|it| it.segment()?.name_ref());
                segment1.text() == "test"
                    && segment3.is_none_or(|it| it.text() == "prelude")
                    && segment4.is_none_or(|it| matches!(it.text(), "core" | "std"))
            });
            if is_test {
                attr_flags.insert(AttrFlags::IS_TEST);
            }

            let Some((first_segment, second_segment)) = attr.path().as_up_to_two_segment() else {
                return ControlFlow::Continue(());
            };
            match second_segment {
                None => match &*first_segment {
                    "rustc_has_incoherent_inherent_impls" => {
                        attr_flags.insert(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS)
                    }
                    "rustc_allow_incoherent_impl" => {
                        attr_flags.insert(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL)
                    }
                    "rustc_scalable_vector" => attr_flags.insert(AttrFlags::HAS_REPR),
                    "fundamental" => attr_flags.insert(AttrFlags::FUNDAMENTAL),
                    "no_std" => attr_flags.insert(AttrFlags::IS_NO_STD),
                    "may_dangle" => attr_flags.insert(AttrFlags::MAY_DANGLE),
                    "rustc_paren_sugar" => attr_flags.insert(AttrFlags::RUSTC_PAREN_SUGAR),
                    "rustc_coinductive" => attr_flags.insert(AttrFlags::RUSTC_COINDUCTIVE),
                    "rustc_force_inline" => attr_flags.insert(AttrFlags::RUSTC_FORCE_INLINE),
                    "rustc_promotable" => attr_flags.insert(AttrFlags::RUSTC_PROMOTABLE),
                    "rustc_pass_indirectly_in_non_rustic_abis" => {
                        attr_flags.insert(AttrFlags::RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS)
                    }
                    "unstable" => attr_flags.insert(AttrFlags::IS_UNSTABLE),
                    "deprecated" => attr_flags.insert(AttrFlags::IS_DEPRECATED),
                    "macro_export" => attr_flags.insert(AttrFlags::IS_MACRO_EXPORT),
                    "must_use" => attr_flags.insert(AttrFlags::IS_MUST_USE),
                    "no_mangle" => attr_flags.insert(AttrFlags::NO_MANGLE),
                    "rustc_std_internal_symbol" | "panic_handler" => {
                        attr_flags.insert(AttrFlags::LINKED_BY_NAME)
                    }
                    "used" => attr_flags.insert(AttrFlags::USED),
                    "track_caller" => attr_flags.insert(AttrFlags::TRACK_CALLER),
                    "inline" => attr_flags.insert(AttrFlags::IS_INLINE),
                    "rustc_inherit_overflow_checks" => {
                        attr_flags.insert(AttrFlags::RUSTC_INHERIT_OVERFLOW_CHECKS)
                    }
                    "thread_local" => attr_flags.insert(AttrFlags::THREAD_LOCAL),
                    "pointee" => attr_flags.insert(AttrFlags::IS_POINTEE),
                    "non_exhaustive" => attr_flags.insert(AttrFlags::NON_EXHAUSTIVE),
                    "ignore" => attr_flags.insert(AttrFlags::IS_IGNORE),
                    "bench" => attr_flags.insert(AttrFlags::IS_BENCH),
                    "rustc_const_panic_str" => attr_flags.insert(AttrFlags::RUSTC_CONST_PANIC_STR),
                    "rustc_intrinsic" => attr_flags.insert(AttrFlags::RUSTC_INTRINSIC),
                    "rustc_safe_intrinsic" => attr_flags.insert(AttrFlags::RUSTC_SAFE_INTRINSIC),
                    "rustc_intrinsic_must_be_overridden" => {
                        attr_flags.insert(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN)
                    }
                    "rustc_allocator" => attr_flags.insert(AttrFlags::RUSTC_ALLOCATOR),
                    "rustc_deallocator" => attr_flags.insert(AttrFlags::RUSTC_DEALLOCATOR),
                    "rustc_reallocator" => attr_flags.insert(AttrFlags::RUSTC_REALLOCATOR),
                    "rustc_allocator_zeroed" => {
                        attr_flags.insert(AttrFlags::RUSTC_ALLOCATOR_ZEROED)
                    }
                    "rustc_reservation_impl" => {
                        attr_flags.insert(AttrFlags::RUSTC_RESERVATION_IMPL)
                    }
                    "rustc_deprecated_safe_2024" => {
                        attr_flags.insert(AttrFlags::RUSTC_DEPRECATED_SAFE_2024)
                    }
                    "rustc_skip_array_during_method_dispatch" => {
                        attr_flags.insert(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH)
                    }
                    _ => {}
                },
                Some(second_segment) => match &*first_segment {
                    "rust_analyzer" => match &*second_segment {
                        "skip" => attr_flags.insert(AttrFlags::RUST_ANALYZER_SKIP),
                        "prefer_underscore_import" => {
                            attr_flags.insert(AttrFlags::PREFER_UNDERSCORE_IMPORT)
                        }
                        _ => {}
                    },
                    "diagnostic" => match &*second_segment {
                        "do_not_recommend" => {
                            attr_flags.insert(AttrFlags::DIAGNOSTIC_DO_NOT_RECOMMEND)
                        }
                        _ => {}
                    },
                    _ => {}
                },
            }
        }
        _ => {}
    };
    ControlFlow::Continue(())
}

/// The flags that gate a value query: set exactly where that query, reading the source, finds
/// its attribute (a key-value or a word attribute whose path `is1` the name).
fn gating_flags(meta: &ast::Meta) -> AttrFlags {
    const KEY_VALUE: &[(&str, AttrFlags)] = &[
        ("export_name", AttrFlags::HAS_EXPORT_NAME),
        ("link_name", AttrFlags::HAS_LINK_NAME),
        ("link_section", AttrFlags::HAS_LINK_SECTION),
        ("linkage", AttrFlags::HAS_LINKAGE),
        ("rustc_diagnostic_item", AttrFlags::HAS_DIAGNOSTIC_ITEM),
    ];
    const WORD: &[(&str, AttrFlags)] = &[
        ("rustc_nounwind", AttrFlags::RUSTC_NOUNWIND),
        ("rustc_comptime", AttrFlags::RUSTC_COMPTIME),
        ("panic_handler", AttrFlags::PANIC_HANDLER),
    ];
    let (path, names) = match meta {
        ast::Meta::KeyValueMeta(meta) => (meta.path(), KEY_VALUE),
        ast::Meta::PathMeta(meta) => (meta.path(), WORD),
        _ => return AttrFlags::empty(),
    };
    names
        .iter()
        .filter(|(name, _)| path.is1(name))
        .fold(AttrFlags::empty(), |flags, &(_, flag)| flags | flag)
}

const GATING: AttrFlags = AttrFlags::HAS_EXPORT_NAME
    .union(AttrFlags::HAS_LINK_NAME)
    .union(AttrFlags::HAS_LINK_SECTION)
    .union(AttrFlags::HAS_LINKAGE)
    .union(AttrFlags::HAS_DIAGNOSTIC_ITEM)
    .union(AttrFlags::RUSTC_NOUNWIND)
    .union(AttrFlags::RUSTC_COMPTIME)
    .union(AttrFlags::PANIC_HANDLER);

fn meta_of(attr: &str) -> ast::Meta {
    let text = format!("{attr} fn f() {{}}");
    let file = SourceFile::parse(&text, Edition::CURRENT).tree();
    file.syntax()
        .descendants()
        .find_map(ast::Attr::cast)
        .and_then(|it| it.meta())
        .unwrap_or_else(|| panic!("no attribute in {text:?}"))
}

/// Every word in every form an attribute takes, unknown names, and the `#[doc(..)]`, `#[test]`
/// and `#[inline(..)]` shapes the flags look into.
fn corpus() -> Vec<String> {
    let mut corpus = Vec::new();
    let names = Word::TEXTS.iter().copied().chain(["allow", "test", "x"]);
    for name in names {
        for form in [
            format!("#[{name}]"),
            format!("#[r#{name}]"),
            format!("#[{name} = \"main\"]"),
            format!("#[{name} = \"x\"]"),
            format!("#[{name} = 1]"),
            format!("#[{name}(always)]"),
            format!("#[{name}(never)]"),
            format!("#[{name}()]"),
            format!("#[{name}(x, y)]"),
            format!("#[a::{name}]"),
            format!("#[a::{name}(x)]"),
            format!("#[a::{name} = \"x\"]"),
            format!("#[a::b::{name}]"),
            format!("#[a::b::{name}(x)]"),
            format!("#[{name}::x]"),
            format!("#[rust_analyzer::{name}]"),
            format!("#[rust_analyzer::{name}(braces)]"),
            format!("#[diagnostic::{name}]"),
            format!("#[unsafe({name})]"),
        ] {
            corpus.push(form);
        }
    }
    for extra in [
        "#[cfg(x)]",
        "#[test]",
        "#[core::prelude::v1::test]",
        "#[std::prelude::test]",
        "#[a::prelude::v1::test]",
        "#[core::x::v1::test]",
        "#[inline(alw)]",
        "#[doc(hidden)]",
        "#[doc(notable_trait)]",
        "#[doc(alias = \"a\")]",
        "#[doc(alias(\"a\", \"b\"))]",
        "#[doc(alias())]",
        "#[doc(keyword = \"k\")]",
        "#[doc(alias = 1)]",
        "#[doc(keyword = \"\\q\")]",
        "#[doc(foo(bar), hidden)]",
        "#[doc(alias(x), keyword = \"y\", hidden)]",
        "#[doc(inline, notable_trait)]",
        "#[doc(test(attr(x)), hidden)]",
        "#[doc(hidden = \"x\")]",
        "#[doc = \"text\"]",
        "#[rustc_skip_during_method_dispatch(array, boxed_slice)]",
        "#[rust_analyzer::completions(ignore_flyimport)]",
        "#[rust_analyzer::macro_style(parentheses)]",
        "#[export_name = \"ma\\u{69}n\"]",
    ] {
        corpus.push(extra.to_owned());
    }
    corpus
}

#[test]
fn the_word_index_flags_every_attribute_as_the_string_match_did() {
    for attr in corpus() {
        let meta = meta_of(&attr);
        let mut old = AttrFlags::empty();
        let _ = old_match_attr_flags(&mut old, meta.clone());
        let mut new = AttrFlags::empty();
        let _ = match_attr_flags(&mut new, meta.clone());
        assert_eq!(new - GATING, old, "{attr}");
        assert_eq!(new & GATING, gating_flags(&meta), "{attr}");
    }
}

#[test]
fn gating_flags_of_known_shapes() {
    for (attr, flags) in [
        ("#[export_name = \"x\"]", AttrFlags::HAS_EXPORT_NAME),
        ("#[export_name = \"main\"]", AttrFlags::HAS_EXPORT_NAME | AttrFlags::IS_EXPORT_NAME_MAIN),
        ("#[link_name = \"x\"]", AttrFlags::HAS_LINK_NAME),
        ("#[link_section = \"x\"]", AttrFlags::HAS_LINK_SECTION),
        ("#[linkage = \"extern_weak\"]", AttrFlags::HAS_LINKAGE),
        ("#[rustc_diagnostic_item = \"x\"]", AttrFlags::HAS_DIAGNOSTIC_ITEM),
        ("#[rustc_nounwind]", AttrFlags::RUSTC_NOUNWIND),
        ("#[r#rustc_nounwind]", AttrFlags::RUSTC_NOUNWIND),
        ("#[a::rustc_nounwind]", AttrFlags::empty()),
        ("#[a::b::rustc_nounwind]", AttrFlags::empty()),
        ("#[rustc_comptime]", AttrFlags::RUSTC_COMPTIME),
        ("#[panic_handler]", AttrFlags::PANIC_HANDLER | AttrFlags::LINKED_BY_NAME),
        ("#[a::b::panic_handler]", AttrFlags::LINKED_BY_NAME),
        ("#[inline(always)]", AttrFlags::IS_INLINE | AttrFlags::INLINE_ALWAYS),
        ("#[inline(never)]", AttrFlags::INLINE_NEVER),
        ("#[inline]", AttrFlags::IS_INLINE),
    ] {
        let mut new = AttrFlags::empty();
        let _ = match_attr_flags(&mut new, meta_of(attr));
        assert_eq!(new, flags, "{attr}");
    }
}

fn functions(db: &TestDB) -> Vec<(String, FunctionId)> {
    let def_map = crate_def_map(db, db.test_crate());
    let mut functions: Vec<_> = def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .filter_map(|decl| match decl {
            ModuleDefId::FunctionId(func) => {
                let name = func.lookup(db).source(db).value.name()?.text().to_string();
                Some((name, func))
            }
            _ => None,
        })
        .collect();
    functions.sort_by(|a, b| a.0.cmp(&b.0));
    functions
}

const GATED: &str = r#"
#[export_name = "exported"] pub fn a() {}
#[export_name = ("paren")] pub fn b() {}
#[link_section = ".text.c"] pub fn c() {}
#[rustc_nounwind] pub fn d() {}
#[rustc_comptime] pub fn e() {}
#[panic_handler] pub fn f() {}
#[rustc_diagnostic_item = "g_item"] pub fn g() {}
#[cfg_attr(all(), export_name = "via_cfg_attr")] pub fn h() {}
#[cfg_attr(any(), export_name = "off")] pub fn i() {}
#[no_builtins] pub fn j() {}
#[linkage = "extern_weak"] pub fn m() {}
#[a::rustc_nounwind] #[export_name = 1] pub fn n() {}
pub fn plain() {}
extern "C" {
    #[link_name = "real"] fn k();
}
"#;

#[test]
fn gated_queries_read_what_the_source_says() {
    let db = TestDB::with_single_file(GATED).0;
    let functions = functions(&db);
    let by_name = |name: &str| {
        functions.iter().find(|(it, _)| it == name).unwrap_or_else(|| panic!("no fn {name}")).1
    };
    let sym = |it: Option<Symbol>| it.map(|it| it.as_str().to_owned());

    assert_eq!(sym(AttrFlags::export_name(&db, by_name("a").into())).as_deref(), Some("exported"));
    assert_eq!(sym(AttrFlags::export_name(&db, by_name("b").into())).as_deref(), Some("paren"));
    assert_eq!(sym(AttrFlags::link_section(&db, by_name("c").into())).as_deref(), Some(".text.c"));
    assert!(AttrFlags::is_rustc_nounwind(&db, by_name("d")));
    assert!(AttrFlags::has_word(&db, by_name("e").into(), "rustc_comptime"));
    assert!(AttrFlags::has_word(&db, by_name("f").into(), "panic_handler"));
    assert_eq!(sym(AttrFlags::diagnostic_item(&db, by_name("g").into())).as_deref(), Some("g_item"));
    assert_eq!(
        sym(AttrFlags::export_name(&db, by_name("h").into())).as_deref(),
        Some("via_cfg_attr")
    );
    assert_eq!(AttrFlags::export_name(&db, by_name("i").into()), None);
    assert!(AttrFlags::has_word(&db, by_name("j").into(), "no_builtins"));
    assert_eq!(sym(AttrFlags::linkage(&db, by_name("m").into())).as_deref(), Some("extern_weak"));
    assert!(!AttrFlags::is_rustc_nounwind(&db, by_name("n")));
    assert_eq!(AttrFlags::export_name(&db, by_name("n").into()), None);
    assert_eq!(sym(AttrFlags::link_name(&db, by_name("k").into())).as_deref(), Some("real"));

    for (name, func) in &functions {
        let owner = AttrDefId::from(*func);
        type Read = fn(&dyn SourceDatabase, AttrDefId) -> Option<Symbol>;
        for (query, read) in [
            ("export_name", AttrFlags::export_name as Read),
            ("link_name", AttrFlags::link_name as Read),
            ("link_section", AttrFlags::link_section as Read),
            ("linkage", AttrFlags::linkage as Read),
            ("rustc_diagnostic_item", AttrFlags::diagnostic_item as Read),
        ] {
            assert_eq!(read(&db, owner), symbol_attr(&db, owner, query), "{query} of {name}");
        }
        for word in ["rustc_nounwind", "rustc_comptime", "panic_handler", "no_builtins", "inline"] {
            assert_eq!(
                AttrFlags::has_word(&db, owner, word),
                AttrFlags::has_word_in_source(&db, owner, word),
                "{word} of {name}"
            );
        }
        assert_eq!(
            AttrFlags::is_rustc_nounwind(&db, *func),
            AttrFlags::has_word_in_source(&db, owner, "rustc_nounwind"),
            "{name}"
        );
    }
}

#[test]
fn field_flags_are_indexed_by_field() {
    let db = TestDB::with_single_file(
        r#"
pub struct S {
    #[cfg(any())] gone: u8,
    #[doc(hidden)] a: u8,
    b: u8,
    #[cfg_attr(all(), deprecated)] c: u8,
}
"#,
    )
    .0;
    let def_map = crate_def_map(&db, db.test_crate());
    let strukt = def_map
        .modules(&db)
        .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
        .find_map(|decl| match decl {
            ModuleDefId::AdtId(AdtId::StructId(it)) => Some(it),
            _ => None,
        })
        .expect("a struct");
    let flags = |at: u32| {
        let local_id = Idx::from_raw(RawIdx::from_u32(at));
        AttrFlags::query_field(&db, FieldId { parent: VariantId::StructId(strukt), local_id })
    };
    assert_eq!(flags(0), AttrFlags::IS_DOC_HIDDEN);
    assert_eq!(flags(1), AttrFlags::empty());
    assert_eq!(flags(2), AttrFlags::IS_DEPRECATED);
    assert_eq!(flags(3), AttrFlags::empty());
}

#[test]
fn inner_docs_of_a_fn_body() {
    let db = TestDB::with_single_file(
        r#"
/// outer
pub fn documented() {
    //! inner
    let _ = 1;
}
"#,
    )
    .0;
    let (_, func) = functions(&db).into_iter().find(|(name, _)| name == "documented").unwrap();
    let docs = AttrFlags::docs(&db, func.into()).expect("docs");
    assert!(docs.docs().contains("outer") && docs.docs().contains("inner"), "{:?}", docs.docs());
}

fn tt_of(attr: &str) -> ast::TokenTree {
    match meta_of(attr) {
        ast::Meta::TokenTreeMeta(meta) => meta.token_tree().expect("a token tree"),
        _ => panic!("no token tree in {attr:?}"),
    }
}

/// Each option of a name set, the near misses of it, and other identifiers, as the one option
/// of `#[path(..)]`.
fn option_corpus(path: &str, texts: &[&str]) -> Vec<String> {
    let mut options: Vec<String> = Vec::new();
    for text in texts.iter().chain(&["xy", "Braces", "array_", "ignore"]) {
        options.push((*text).to_owned());
        options.push(format!("r#{text}"));
        options.push(format!("{text}s"));
        options.push(text[..text.len() - 1].to_owned());
    }
    let mut corpus: Vec<String> =
        options.iter().filter(|it| !it.is_empty()).map(|it| format!("#[{path}({it})]")).collect();
    corpus.push(format!("#[{path}()]"));
    corpus.push(format!("#[{path}({}, {})]", texts[0], texts[texts.len() - 1]));
    corpus.push(format!("#[{path}(\"{}\")]", texts[0]));
    corpus
}

#[test]
fn option_sets_flag_as_the_string_match_did() {
    use syntax::{NodeOrToken, ast::TokenTreeChildren};

    fn old_ra_completions(attr_flags: &mut AttrFlags, tt: ast::TokenTree) {
        let tt = TokenTreeChildren::new(&tt);
        if let Ok(NodeOrToken::Token(option)) = itertools::Itertools::exactly_one(tt)
            && option.kind().is_any_identifier()
        {
            match option.text() {
                "ignore_flyimport" => attr_flags.insert(AttrFlags::COMPLETE_IGNORE_FLYIMPORT),
                "ignore_methods" => attr_flags.insert(AttrFlags::COMPLETE_IGNORE_METHODS),
                "ignore_flyimport_methods" => {
                    attr_flags.insert(AttrFlags::COMPLETE_IGNORE_FLYIMPORT_METHODS)
                }
                _ => {}
            }
        }
    }
    fn old_ra_macro_style(attr_flags: &mut AttrFlags, tt: ast::TokenTree) {
        let tt = TokenTreeChildren::new(&tt);
        if let Ok(NodeOrToken::Token(option)) = itertools::Itertools::exactly_one(tt)
            && option.kind().is_any_identifier()
        {
            match option.text() {
                "braces" => attr_flags.insert(AttrFlags::MACRO_STYLE_BRACES),
                "brackets" => attr_flags.insert(AttrFlags::MACRO_STYLE_BRACKETS),
                "parentheses" => attr_flags.insert(AttrFlags::MACRO_STYLE_PARENTHESES),
                _ => {}
            }
        }
    }
    fn old_skip_during_method_dispatch(attr_flags: &mut AttrFlags, tt: ast::TokenTree) {
        for kind in TokenTreeChildren::new(&tt) {
            if let NodeOrToken::Token(kind) = kind
                && kind.kind().is_any_identifier()
            {
                match kind.text() {
                    "array" => {
                        attr_flags.insert(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH)
                    }
                    "boxed_slice" => {
                        attr_flags.insert(AttrFlags::RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH)
                    }
                    _ => {}
                }
            }
        }
    }

    type Extract = fn(&mut AttrFlags, ast::TokenTree);
    for (path, texts, new, old) in [
        (
            "rust_analyzer::completions",
            Completions::TEXTS,
            extract_ra_completions as Extract,
            old_ra_completions as Extract,
        ),
        (
            "rust_analyzer::macro_style",
            MacroStyle::TEXTS,
            extract_ra_macro_style as Extract,
            old_ra_macro_style as Extract,
        ),
        (
            "rustc_skip_during_method_dispatch",
            SkipDuringMethodDispatch::TEXTS,
            extract_rustc_skip_during_method_dispatch as Extract,
            old_skip_during_method_dispatch as Extract,
        ),
    ] {
        for attr in option_corpus(path, texts) {
            let (mut got, mut want) = (AttrFlags::empty(), AttrFlags::empty());
            new(&mut got, tt_of(&attr));
            old(&mut want, tt_of(&attr));
            assert_eq!(got, want, "{attr}");
        }
    }
}

#[test]
fn doc_atoms_flag_as_the_string_match_did() {
    let mut corpus = Vec::new();
    for &text in DocWord::TEXTS.iter().chain(&["inline", "test", "xy"]) {
        for atom in [
            text.to_owned(),
            format!("r#{text}"),
            format!("{text}_"),
            text[..text.len() - 1].to_owned(),
        ] {
            if atom.is_empty() {
                continue;
            }
            corpus.push(format!("#[doc({atom})]"));
            corpus.push(format!("#[doc({atom} = \"v\")]"));
            corpus.push(format!("#[doc({atom} = 1)]"));
            corpus.push(format!("#[doc({atom}(\"a\"))]"));
            corpus.push(format!("#[doc({atom}, hidden)]"));
        }
    }
    for attr in corpus {
        let meta = meta_of(&attr);
        let (mut old, mut new) = (AttrFlags::empty(), AttrFlags::empty());
        let _ = old_match_attr_flags(&mut old, meta.clone());
        let _ = match_attr_flags(&mut new, meta);
        assert_eq!(new - GATING, old, "{attr}");
    }
}

/// `parse_repr_tt` as it was, by string `match` and `BuiltinInt::from_suffix`.
fn old_parse_repr_tt(tt: &ast::TokenTree) -> Option<rustc_abi::ReprOptions> {
    use crate::builtin_type::{BuiltinInt, BuiltinUint};
    use either::Either;
    use rustc_abi::{Align, Integer, IntegerType, ReprFlags, ReprOptions};
    use syntax::{NodeOrToken, ast::TokenTreeChildren};

    let mut tts = TokenTreeChildren::new(tt).peekable();
    let mut acc = ReprOptions::default();
    while let Some(tt) = tts.next() {
        let NodeOrToken::Token(ident) = tt else { continue };
        if !ident.kind().is_any_identifier() {
            continue;
        }
        let literal = |tts: &mut std::iter::Peekable<TokenTreeChildren>| {
            if let Some(NodeOrToken::Node(tt)) = tts.peek() {
                let tt = tt.clone();
                tts.next();
                let mut tt_iter = TokenTreeChildren::new(&tt);
                if let Some(NodeOrToken::Token(lit)) = tt_iter.next()
                    && let Some(lit) = ast::IntNumber::cast(lit)
                    && let Ok(lit) = lit.value()
                    && let Ok(lit) = u64::try_from(lit)
                {
                    return (true, Some(lit));
                }
                return (true, None);
            }
            (false, None)
        };
        let repr = match ident.text().trim_start_matches("r#") {
            "packed" => {
                let pack = literal(&mut tts).1.unwrap_or(0);
                ReprOptions {
                    pack: Some(Align::from_bytes(pack).unwrap_or(Align::ONE)),
                    ..Default::default()
                }
            }
            "align" => {
                let align = literal(&mut tts).1.and_then(|it| Align::from_bytes(it).ok());
                ReprOptions { align, ..Default::default() }
            }
            "C" => ReprOptions { flags: ReprFlags::IS_C, ..Default::default() },
            "transparent" => ReprOptions { flags: ReprFlags::IS_TRANSPARENT, ..Default::default() },
            "simd" => ReprOptions { flags: ReprFlags::IS_SIMD, ..Default::default() },
            repr => {
                let int = BuiltinInt::from_suffix(repr)
                    .map(Either::Left)
                    .or_else(|| BuiltinUint::from_suffix(repr).map(Either::Right))
                    .map(|builtin| match builtin {
                        Either::Left(bi) => match bi {
                            BuiltinInt::Isize => IntegerType::Pointer(true),
                            BuiltinInt::I8 => IntegerType::Fixed(Integer::I8, true),
                            BuiltinInt::I16 => IntegerType::Fixed(Integer::I16, true),
                            BuiltinInt::I32 => IntegerType::Fixed(Integer::I32, true),
                            BuiltinInt::I64 => IntegerType::Fixed(Integer::I64, true),
                            BuiltinInt::I128 => IntegerType::Fixed(Integer::I128, true),
                        },
                        Either::Right(bu) => match bu {
                            BuiltinUint::Usize => IntegerType::Pointer(false),
                            BuiltinUint::U8 => IntegerType::Fixed(Integer::I8, false),
                            BuiltinUint::U16 => IntegerType::Fixed(Integer::I16, false),
                            BuiltinUint::U32 => IntegerType::Fixed(Integer::I32, false),
                            BuiltinUint::U64 => IntegerType::Fixed(Integer::I64, false),
                            BuiltinUint::U128 => IntegerType::Fixed(Integer::I128, false),
                        },
                    });
                ReprOptions { int, ..Default::default() }
            }
        };
        super::merge_repr(&mut acc, repr);
    }
    Some(acc)
}

#[test]
fn repr_hints_read_as_the_string_match_did() {
    let mut hints: Vec<String> = Vec::new();
    for &text in ReprWord::TEXTS.iter().chain(&["Rust", "c", "i256", "f32", "x"]) {
        hints.push(text.to_owned());
        hints.push(format!("r#{text}"));
        hints.push(format!("{text}_"));
        hints.push(text.to_uppercase());
        hints.push(text[..text.len() - 1].to_owned());
    }
    let mut corpus: Vec<String> = hints
        .iter()
        .filter(|it| !it.is_empty() && !it.starts_with(|c: char| c.is_ascii_digit()))
        .map(|it| format!("#[repr({it})]"))
        .collect();
    corpus.extend(
        [
            "#[repr(packed(4))]",
            "#[repr(packed(3))]",
            "#[repr(packed)]",
            "#[repr(align(16))]",
            "#[repr(align(3))]",
            "#[repr(align)]",
            "#[repr(C, u8)]",
            "#[repr(u8, C, align(8), packed(2))]",
            "#[repr(transparent, simd)]",
            "#[repr(i32, u64)]",
            "#[repr(\"C\")]",
            "#[repr()]",
        ]
        .map(str::to_owned),
    );
    for attr in corpus {
        let tt = tt_of(&attr);
        assert_eq!(parse_repr_tt(&tt), old_parse_repr_tt(&tt), "{attr}");
    }
}

/// `parse_link_tt` as it was, by string `match`.
fn old_parse_link_tt(tt: &ast::TokenTree) -> Option<(NativeLib, Option<cfg::CfgExpr>)> {
    use syntax::{NodeOrToken, SyntaxKind, T, ast::TokenTreeChildren};

    let mut lib = NativeLib {
        name: Symbol::empty(),
        kind: NativeLibKind::Unspecified,
        verbatim: false,
        whole_archive: false,
        bundle: true,
    };
    let mut cfg = None;
    let mut named = false;
    let mut children = TokenTreeChildren::new(tt).peekable();
    while let Some(child) = children.next() {
        let NodeOrToken::Token(key) = child else { continue };
        if key.kind() != SyntaxKind::IDENT {
            continue;
        }
        match children.peek() {
            Some(NodeOrToken::Node(group)) if key.text() == "cfg" => {
                cfg = link_cfg(group);
                children.next();
            }
            Some(NodeOrToken::Token(eq)) if eq.kind() == T![=] => {
                children.next();
                let Some(NodeOrToken::Token(value)) = children.next() else { continue };
                let Some(value) =
                    ast::String::cast(value).and_then(|it| Some(it.value().ok()?.into_owned()))
                else {
                    continue;
                };
                match key.text() {
                    "name" => {
                        lib.name = Symbol::intern(&value);
                        named = true;
                    }
                    "kind" => {
                        lib.kind = match &*value {
                            "static" => NativeLibKind::Static,
                            "dylib" => NativeLibKind::Dylib,
                            "framework" => NativeLibKind::Framework,
                            "raw-dylib" => NativeLibKind::RawDylib,
                            _ => NativeLibKind::Unspecified,
                        }
                    }
                    "modifiers" => {
                        for modifier in value.split(',') {
                            let (on, name) = modifier.split_at(modifier.len().min(1));
                            match name {
                                "verbatim" => lib.verbatim = on == "+",
                                "whole-archive" => lib.whole_archive = on == "+",
                                "bundle" => lib.bundle = on == "+",
                                _ => (),
                            }
                        }
                    }
                    _ => (),
                }
            }
            _ => (),
        }
    }
    named.then_some((lib, cfg))
}

#[test]
fn link_attrs_read_as_the_string_match_did() {
    let mut corpus: Vec<String> = Vec::new();
    for &key in LinkKey::TEXTS.iter().chain(&["wasm_import_module", "names", "Kind", "r#name"]) {
        corpus.push(format!("#[link({key} = \"foo\")]"));
        corpus.push(format!("#[link(name = \"a\", {key} = \"static\")]"));
        corpus.push(format!("#[link(name = \"a\", {key}(unix))]"));
        corpus.push(format!("#[link(name = \"a\", {key} = 1)]"));
    }
    let kinds = LinkKind::TEXTS.iter().chain(&["Static", "dylib_", "raw_dylib", "", "framewor"]);
    for kind in kinds {
        corpus.push(format!("#[link(name = \"a\", kind = \"{kind}\")]"));
    }
    let modifiers =
        LinkModifier::TEXTS.iter().chain(&["whole_archive", "Bundle", "verbatim_", "", "x"]);
    for modifier in modifiers {
        for sign in ["+", "-", "", "*"] {
            corpus.push(format!("#[link(name = \"a\", modifiers = \"{sign}{modifier}\")]"));
        }
    }
    corpus.extend(
        [
            "#[link(name = \"a\", modifiers = \"+verbatim,-bundle,+whole-archive\")]",
            "#[link(name = \"a\", modifiers = \"\")]",
            "#[link(name = \"a\", kind = \"static\", kind = \"dylib\")]",
            "#[link(name = \"a\", cfg(target_os = \"linux\"))]",
            "#[link(name = \"a\", cfg = \"x\")]",
            "#[link(wasm_import_module = \"m\")]",
            "#[link()]",
        ]
        .map(str::to_owned),
    );
    for attr in corpus {
        let tt = tt_of(&attr);
        assert_eq!(parse_link_tt(&tt), old_parse_link_tt(&tt), "{attr}");
    }
}
