use std::{path::Path, sync::Once};

use paths::AbsPathBuf;
use span::{EditionedFileId, ErasedFileAstId, SpanAnchor, SyntaxContext, TextRange};
use tt::{IdentIsRaw, LitKind, Spacing};

use super::{
    verified::{QUOTE, SERDE_DERIVE, SYN_2, SYN_3, THISERROR_IMPL_1, THISERROR_IMPL_2, TOKIO_MACROS},
    *,
};

const GUEST: &str = include_str!("../../../blaze-macro-wasm/guest/proc_macro.rs");
const WASM_WIRE: &str = include_str!("../../../blaze-macro-wasm/src/wire.rs");
const WASM_LEX: &str = include_str!("../../../blaze-macro-wasm/src/lex.rs");

fn span(start: u32, end: u32) -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(span::Edition::CURRENT),
    }
}

fn tokens(text: &str, at: Span) -> TopSubtree {
    wire::top_subtree(lex::stream(text, at).expect("the test's text lexes"), at)
}

/// The text from the first `from` to the next `to` after it.
fn between<'a>(text: &'a str, from: &str, to: &str) -> &'a str {
    let start = text.find(from).unwrap_or_else(|| panic!("no {from:?}"));
    let end = start + text[start..].find(to).unwrap_or_else(|| panic!("no {to:?} after {from:?}"));
    &text[start..end]
}

fn vendor(dir: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/native-macros").join(dir)
}

#[test]
fn the_proc_macro_is_the_guests_but_for_its_imports_and_exports() {
    let shim = include_str!("proc_macro/lib.rs");
    assert_eq!(
        between(shim, "\nfn taken(", "\n/// How blaze runs a macro natively"),
        between(GUEST, "\nfn taken(", "\n/// What blaze's list of a crate's macros calls"),
    );
    let uses = |text: &'static str| between(text, "\nuse std::{", "};");
    assert_eq!(uses(shim), uses(GUEST));
}

#[test]
fn the_wire_and_the_lexer_are_the_wasm_hosts() {
    let from = |text: &'static str, start: &str| &text[text.find(start).unwrap()..];
    assert_eq!(from(include_str!("wire.rs"), "\nuse intern::Symbol;"), from(WASM_WIRE, "\nuse intern::Symbol;"));
    let lex = from(include_str!("lex.rs"), "\nuse intern::Symbol;");
    let wasm_lex = from(WASM_LEX, "\nuse intern::Symbol;")
        .replacen("use crate::wire::Tree;", "use super::wire::Tree;", 1);
    assert_eq!(lex, wasm_lex);
}

/// The verified copy whose crate is `root` of [`PACKAGES`], as a description of it would say.
fn verified_closure(root: usize) -> Closure {
    let all = closure_of(root);
    let packages = all
        .iter()
        .map(|&at| {
            let package = &PACKAGES[at];
            let actual = &actual()[at];
            Package {
                name: package.name.to_owned(),
                version: package.version.to_owned(),
                files: actual.files.iter().map(|(path, hash)| ((*path).to_owned(), *hash)).collect(),
                cfg: actual.cfg.clone().expect("blaze's proc-macro2 is known"),
                deps: package
                    .deps
                    .iter()
                    .map(|(name, dep)| {
                        ((*name).to_owned(), all.iter().position(|it| it == dep).unwrap())
                    })
                    .collect(),
            }
        })
        .collect();
    Closure { packages }
}

fn package_of(closure: &mut Closure, name: &str, version: &str) -> &mut Package {
    closure.packages.iter_mut().find(|it| it.name == name && it.version == version).unwrap()
}

#[test]
fn the_verified_copy_matches_itself() {
    for root in &ROOTS {
        let closure = verified_closure(root.package);
        assert_eq!(shaped(&closure), Ok(()));
        assert_eq!(matches(&closure, root.package), Ok(()), "{}", PACKAGES[root.package].name);
    }
}

#[test]
fn one_changed_byte_is_another_package() {
    let mut closure = verified_closure(SERDE_DERIVE);
    let (path, bytes) =
        PACKAGES[SYN_3].files.iter().find(|it| it.0 == "src/lib.rs").copied().unwrap();
    let mut changed = bytes.to_vec();
    changed[0] ^= 1;
    let syn = package_of(&mut closure, "syn", "3.0.9");
    let file = syn.files.iter_mut().find(|it| it.0 == path).unwrap();
    file.1 = *blake3::hash(&changed).as_bytes();
    let why = matches(&closure, SERDE_DERIVE).unwrap_err();
    assert_eq!(why, "syn 3.0.9: its `src/lib.rs` is not the verified copy's");
}

#[test]
fn a_file_more_or_less_is_another_package() {
    let mut closure = verified_closure(TOKIO_MACROS);
    let quote = package_of(&mut closure, "quote", "1.0.47");
    quote.files.push(("zzz.rs".to_owned(), [0; 32]));
    let why = matches(&closure, TOKIO_MACROS).unwrap_err();
    assert_eq!(why, "quote 1.0.47: it has `zzz.rs`, which the verified copy has not");

    let mut closure = verified_closure(TOKIO_MACROS);
    package_of(&mut closure, "quote", "1.0.47").files.retain(|it| it.0 != "build.rs");
    let why = matches(&closure, TOKIO_MACROS).unwrap_err();
    assert_eq!(why, "quote 1.0.47: it has no `build.rs`");
}

#[test]
fn one_more_feature_is_another_package() {
    let mut closure = verified_closure(SERDE_DERIVE);
    let syn = package_of(&mut closure, "syn", "3.0.9");
    syn.cfg.push("feature=\"extra-traits\"".to_owned());
    syn.cfg.sort();
    let why = matches(&closure, SERDE_DERIVE).unwrap_err();
    assert_eq!(why, "syn 3.0.9: it is built with `feature=\"extra-traits\"`, which the verified copy is not");

    let mut closure = verified_closure(SERDE_DERIVE);
    package_of(&mut closure, "syn", "3.0.9").cfg.retain(|it| it != "feature=\"full\"");
    let why = matches(&closure, SERDE_DERIVE).unwrap_err();
    assert_eq!(why, "syn 3.0.9: it is built without `feature=\"full\"`, which the verified copy is");

    let mut closure = verified_closure(SERDE_DERIVE);
    package_of(&mut closure, "proc-macro2", "1.0.107").cfg.retain(|it| it != "proc_macro_span");
    let why = matches(&closure, SERDE_DERIVE).unwrap_err();
    assert!(why.contains("without `proc_macro_span`"), "{why}");
}

#[test]
fn another_syn_is_another_closure() {
    let mut closure = verified_closure(THISERROR_IMPL_2);
    let syn_2 = verified_closure(SYN_2).packages.remove(0);
    *package_of(&mut closure, "syn", "3.0.9") = syn_2;
    let why = matches(&closure, THISERROR_IMPL_2).unwrap_err();
    assert_eq!(why, "syn 2.0.119 is where the verified copy has syn 3.0.9");

    let mut closure = verified_closure(SERDE_DERIVE);
    package_of(&mut closure, "serde_derive", "1.0.229").deps.retain(|it| it.0 != "quote");
    let why = matches(&closure, SERDE_DERIVE).unwrap_err();
    assert_eq!(
        why,
        "serde_derive 1.0.229 depends on [proc_macro2, syn] where the verified copy depends on \
         [proc_macro2, quote, syn]"
    );
}

#[test]
fn what_no_source_names_is_not_compared() {
    for root in &ROOTS {
        assert!(!mentioned(root.package, "debug_assertions"), "{}", PACKAGES[root.package].name);
    }
    let mut closure = verified_closure(SERDE_DERIVE);
    for package in &mut closure.packages {
        package.cfg.retain(|it| it != "debug_assertions");
    }
    package_of(&mut closure, "syn", "3.0.9").cfg.push("debug_assertions".to_owned());
    assert_eq!(matches(&closure, SERDE_DERIVE), Ok(()));
    package_of(&mut closure, "syn", "3.0.9").cfg.push("test".to_owned());
    assert!(matches(&closure, SERDE_DERIVE).is_err(), "syn's sources name `test`");
}

#[test]
fn the_build_of_proc_macro2_is_what_its_code_shows() {
    assert_eq!(
        proc_macro2_cfg(),
        Ok(&[
            "proc_macro_span",
            "proc_macro_span_file",
            "proc_macro_span_location",
            "wrap_proc_macro"
        ][..])
    );
}

/// The closures [`find`] is told of in these tests, by the library's path.
static DESCRIBED: Mutex<Vec<(AbsPathBuf, Option<Closure>)>> = Mutex::new(Vec::new());

fn described(name: &str, closure: Option<Closure>) -> AbsPathBuf {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        describe_with(Box::new(|dylib: &AbsPath| {
            let described = lock(&DESCRIBED);
            match described.iter().find(|it| *it.0 == *dylib) {
                Some((_, closure)) => Ok(closure.clone()),
                None => Err(format!("the test describes no {dylib}")),
            }
        }))
        .unwrap();
    });
    let path = AbsPathBuf::assert_utf8(std::env::temp_dir().join(format!("{name}.wasm")));
    lock(&DESCRIBED).push((path.clone(), closure));
    path
}

#[test]
fn find_gives_an_expander_only_of_the_verified_copy() {
    let dylib = described("blaze-native-serde", Some(verified_closure(SERDE_DERIVE)));
    let found = find(&dylib, "Serialize", ProcMacroKind::CustomDerive).unwrap().unwrap();
    let again = find(&dylib, "Serialize", ProcMacroKind::CustomDerive).unwrap().unwrap();
    assert_eq!(found.identity(), again.identity());
    let other = find(&dylib, "Deserialize", ProcMacroKind::CustomDerive).unwrap().unwrap();
    assert_ne!(found.identity(), other.identity());
    assert!(find(&dylib, "Serialize", ProcMacroKind::Attr).is_err());

    let mut changed = verified_closure(SERDE_DERIVE);
    package_of(&mut changed, "quote", "1.0.47").cfg.push("feature=\"nightly\"".to_owned());
    let dylib = described("blaze-native-serde-changed", Some(changed));
    assert!(find(&dylib, "Serialize", ProcMacroKind::CustomDerive).unwrap().is_none());
    assert!(report().unwrap().contains("built with `feature=\"nightly\"`"));

    let dylib = described("blaze-native-none", None);
    assert!(find(&dylib, "Serialize", ProcMacroKind::CustomDerive).unwrap().is_none());
    let unknown = AbsPathBuf::assert_utf8(std::env::temp_dir().join("blaze-native-unknown.wasm"));
    assert!(find(&unknown, "Serialize", ProcMacroKind::CustomDerive).is_err());
}

#[test]
fn a_description_is_read_as_a_closure() {
    let dir = std::env::temp_dir().join(format!("blaze-native-described-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(dir.join(".cargo-ok"), "{\"v\":1}").unwrap();
    let crate_ = |name: &str, version: &str| {
        serde_json::json!({
            "display_name": name,
            "root_module": dir.join("src/lib.rs").to_str().unwrap(),
            "edition": "2021",
            "deps": [],
            "cfg": ["feature=\"default\"", "debug_assertions"],
            "env": {
                "CARGO_MANIFEST_DIR": dir.to_str().unwrap(),
                "CARGO_PKG_NAME": name,
                "CARGO_PKG_VERSION": version,
            },
            "is_workspace_member": true,
            "is_proc_macro": true,
        })
    };
    let description = serde_json::json!({ "crates": [crate_("serde_derive", "1.0.229")] });
    let closure = closure_of_description(&description.to_string()).unwrap().unwrap();
    let package = &closure.packages[0];
    assert_eq!((package.name.as_str(), package.version.as_str()), ("serde_derive", "1.0.229"));
    assert_eq!(package.cfg, ["debug_assertions", "feature=\"default\""]);
    let files: Vec<&str> = package.files.iter().map(|it| it.0.as_str()).collect();
    assert_eq!(files, ["Cargo.toml", "src/lib.rs"]);
    assert_eq!(package.files[1].1, *blake3::hash(b"pub fn f() {}\n").as_bytes());
    assert!(matches(&closure, SERDE_DERIVE).is_err());

    let description = serde_json::json!({ "crates": [crate_("my_macros", "0.1.0")] });
    assert_eq!(closure_of_description(&description.to_string()), Ok(None));
    assert!(closure_of_description("{}").is_err());
    _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn equal_trees_have_no_difference() {
    let text = "#[derive(Debug)] struct S<T> { a: [u8; 3], b: (T, \"s\", 2.5e3f64) }";
    let tree = tokens(text, span(3, 9));
    assert_eq!(difference(&tree, &tokens(text, span(3, 9))), None);
}

/// `tree` with its token tree `at` changed by `change`.
fn changed(tree: &TopSubtree, at: usize, change: impl FnOnce(&mut TokenTree)) -> TopSubtree {
    let mut flat: Vec<TokenTree> = tree.as_token_trees().iter_flat_tokens().collect();
    change(&mut flat[at]);
    TopSubtree::from_serialized(flat)
}

fn leaf_span(tree: &mut TokenTree) -> &mut Span {
    match tree {
        TokenTree::Leaf(Leaf::Ident(it)) => &mut it.span,
        TokenTree::Leaf(Leaf::Punct(it)) => &mut it.span,
        TokenTree::Leaf(Leaf::Literal(it)) => &mut it.span,
        TokenTree::Subtree(it) => &mut it.delimiter.open,
    }
}

#[test]
fn a_difference_names_what_differs() {
    // 0 the invisible top, 1 `a`, 2 `:`, 3 `:`, 4 `b`, 5 `(`, 6 `1u8`.
    let tree = tokens("a::b(1u8)", span(0, 9));
    let said = |other: &TopSubtree| difference(other, &tree).expect("a difference");

    let renamed = changed(&tree, 1, |it| {
        if let TokenTree::Leaf(Leaf::Ident(ident)) = it {
            ident.sym = intern::Symbol::intern("c");
        }
    });
    assert_eq!(said(&renamed), "token tree 1, `a`: its text is \"c\" natively and \"a\" by the macro");

    let raw = changed(&tree, 1, |it| {
        if let TokenTree::Leaf(Leaf::Ident(ident)) = it {
            ident.is_raw = IdentIsRaw::Yes;
        }
    });
    assert!(said(&raw).contains("its rawness is Yes natively"), "{}", said(&raw));

    let spaced = changed(&tree, 2, |it| {
        if let TokenTree::Leaf(Leaf::Punct(punct)) = it {
            punct.spacing = Spacing::Alone;
        }
    });
    assert_eq!(said(&spaced), "token tree 2, `:`: its spacing is Alone natively and Joint by the macro");

    let moved = changed(&tree, 4, |it| leaf_span(it).range = TextRange::new(1.into(), 2.into()));
    assert!(said(&moved).starts_with("token tree 4, `b`: its range is 1..2 natively"), "{}", said(&moved));

    let anchored = changed(&tree, 6, |it| leaf_span(it).anchor.ast_id = ErasedFileAstId::from_raw(1));
    assert!(said(&anchored).starts_with("token tree 6, the literal `1u8`: its anchor is"), "{}", said(&anchored));

    let hygienic = changed(&tree, 5, |it| {
        leaf_span(it).ctx = SyntaxContext::root(span::Edition::Edition2015);
    });
    assert!(
        said(&hygienic).starts_with("token tree 5, the group `(`: its opening delimiter's syntax context is"),
        "{}",
        said(&hygienic)
    );

    let suffixed = changed(&tree, 6, |it| {
        if let TokenTree::Leaf(Leaf::Literal(literal)) = it {
            *literal = tt::Literal::new("1", literal.span, LitKind::Integer, "u16");
        }
    });
    assert!(said(&suffixed).contains("its suffix is \"u16\" natively and \"u8\" by the macro"));

    let shorter = tokens("a::b", span(0, 9));
    assert!(difference(&shorter, &tree).is_some());
}

/// The macro `name` of the crate `package` of [`PACKAGES`], expanding `input` (and `attr`,
/// for an attribute), its spans within the text of each, with no database to ask.
fn expanded(
    package: usize,
    name: &str,
    attr: Option<&str>,
    input: &str,
) -> Result<TopSubtree, ProcMacroExpansionError> {
    let root = ROOTS.iter().position(|it| it.package == package).unwrap();
    let entry = ROOTS[root].macros.iter().position(|it| it.name == name).unwrap();
    let native = Macro { root, entry, identity: identity(root, entry) };
    let input = tokens(input, span(0, input.len() as u32));
    let attr = attr.map(|it| tokens(it, span(1000, 1000 + it.len() as u32)));
    native.expand_with(None, &input, attr.as_ref(), span(5, 5), span(6, 6), span(7, 7))
}

/// The text of each token of `tree` in order, a group as its opening delimiter.
fn words(tree: &TopSubtree) -> Vec<String> {
    tree.as_token_trees()
        .iter_flat_tokens()
        .filter_map(|it| match it {
            TokenTree::Subtree(subtree) => match subtree.delimiter.kind {
                tt::DelimiterKind::Parenthesis => Some("(".to_owned()),
                tt::DelimiterKind::Brace => Some("{".to_owned()),
                tt::DelimiterKind::Bracket => Some("[".to_owned()),
                tt::DelimiterKind::Invisible => None,
            },
            TokenTree::Leaf(Leaf::Ident(it)) => {
                Some(format!("{}{}", it.is_raw.as_str(), it.sym.as_str()))
            }
            TokenTree::Leaf(Leaf::Punct(it)) => Some(it.char.to_string()),
            TokenTree::Leaf(Leaf::Literal(it)) if it.kind == LitKind::Str => {
                Some(format!("\"{}\"", it.text()))
            }
            TokenTree::Leaf(Leaf::Literal(it)) => Some(format!("{}{}", it.text(), it.suffix())),
        })
        .collect()
}

#[track_caller]
fn assert_has(tree: &TopSubtree, window: &str) {
    let words = words(tree);
    let window: Vec<&str> = window.split(' ').collect();
    assert!(
        words.windows(window.len()).any(|it| it.iter().zip(&window).all(|(a, b)| a == b)),
        "no `{}` in `{}`",
        window.join(" "),
        words.join(" "),
    );
}

/// Every span of `tree` is of the test's one file and of its one context.
#[track_caller]
fn assert_spans_known(tree: &TopSubtree) {
    for it in tree.as_token_trees().iter_flat_tokens() {
        let spans = match &it {
            TokenTree::Subtree(it) => vec![it.delimiter.open, it.delimiter.close],
            TokenTree::Leaf(it) => vec![*it.span()],
        };
        for it in spans {
            assert_eq!((it.anchor, it.ctx), (span(0, 0).anchor, span(0, 0).ctx), "{it:?}");
        }
    }
}

#[test]
fn serde_derives_serialize() {
    let out = expanded(SERDE_DERIVE, "Serialize", None, "struct Point { x: i32, y: i32 }").unwrap();
    assert_has(&out, "impl _serde : : Serialize for Point");
    assert_has(&out, "serialize_struct");
    assert_has(&out, "\"Point\"");
    assert_has(&out, "\"x\"");
    assert_spans_known(&out);

    let renamed = "#[serde(rename = \"pt\")] struct Point { #[serde(skip)] x: i32, y: i32 }";
    let out = expanded(SERDE_DERIVE, "Serialize", None, renamed).unwrap();
    assert_has(&out, "\"pt\"");
    assert!(!words(&out).contains(&"\"x\"".to_owned()));

    let generic = "struct Wrap<T> { inner: T }";
    let out = expanded(SERDE_DERIVE, "Serialize", None, generic).unwrap();
    assert_has(&out, "for Wrap < T >");
    assert_has(&out, "T : _serde : : Serialize");

    let out = expanded(SERDE_DERIVE, "Serialize", None, "union U { a: u8 }").unwrap();
    assert_has(&out, "compile_error");
}

#[test]
fn serde_derives_deserialize() {
    let input = "enum Shape { Circle(f64), Square { side: f64 } }";
    let out = expanded(SERDE_DERIVE, "Deserialize", None, input).unwrap();
    assert_has(&out, "Deserialize < ' de > for Shape");
    assert_has(&out, "\"Circle\"");
    assert_has(&out, "\"side\"");
    assert_spans_known(&out);
}

#[test]
fn thiserror_2_derives_error() {
    let input = "enum E { #[error(\"bad {0}\")] Bad(u8), #[error(transparent)] Io(#[from] std::io::Error) }";
    let out = expanded(THISERROR_IMPL_2, "Error", None, input).unwrap();
    assert_has(&out, "thiserror : : __private21 : : Error for E");
    assert_has(&out, "From < std : : io : : Error > for E");
    assert_spans_known(&out);

    let out = expanded(THISERROR_IMPL_2, "Error", None, "enum E { Bad(u8) }").unwrap();
    assert_has(&out, "compile_error");
}

#[test]
fn thiserror_1_derives_error() {
    let input = "struct E { #[source] inner: std::io::Error }";
    let out = expanded(THISERROR_IMPL_1, "Error", None, input).unwrap();
    assert_has(&out, "std : : error : : Error for E");
    assert_has(&out, "source");

    let input = "#[error(\"failed: {0}\")] struct Failed(String);";
    let out = expanded(THISERROR_IMPL_1, "Error", None, input).unwrap();
    assert_has(&out, "core : : fmt : : Display for Failed");
    assert_spans_known(&out);
}

#[test]
fn tokio_expands_main_and_test() {
    let input = "async fn main() { println!(\"hi\"); }";
    let out = expanded(TOKIO_MACROS, "main", None, input).unwrap();
    assert_has(&out, "Builder : : new_multi_thread ( )");
    assert_has(&out, "block_on");
    assert_has(&out, "fn main ( )");

    let out = expanded(TOKIO_MACROS, "main", Some("flavor = \"current_thread\""), input).unwrap();
    assert_has(&out, "Builder : : new_current_thread ( )");

    let out = expanded(TOKIO_MACROS, "main", None, "fn main() {}").unwrap();
    assert_has(&out, "compile_error");

    let out = expanded(TOKIO_MACROS, "test", None, "async fn it_works() { assert!(true); }").unwrap();
    assert_has(&out, "core : : prelude : : v1 : : test");
    assert_has(&out, "fn it_works ( )");
    assert_spans_known(&out);
}

#[test]
fn tokio_expands_its_function_like_macros_and_panics_as_it_does() {
    let out = expanded(TOKIO_MACROS, "select_priv_clean_pattern", None, "Some(ref mut x)").unwrap();
    assert_eq!(words(&out), ["Some", "(", "x"]);

    let out = expanded(TOKIO_MACROS, "select_priv_declare_output_enum", None, "(_ _ _)").unwrap();
    assert_has(&out, "_2");
    assert_has(&out, "u8");

    let panicked = expanded(TOKIO_MACROS, "select_priv_declare_output_enum", None, "x");
    assert!(
        matches!(&panicked, Err(ProcMacroExpansionError::Panic(it)) if it == "unexpected macro input"),
        "{panicked:?}"
    );
}

#[test]
fn every_package_is_its_vendored_copy() {
    for package in &PACKAGES {
        let dir = vendor(package.dir);
        let mut on_disk = Vec::new();
        walk(&dir, "", &mut on_disk).unwrap();
        on_disk.sort();
        let published: Vec<&str> =
            on_disk.iter().filter_map(|it| it.0.strip_prefix(".published/")).collect();
        let built: Vec<&str> =
            on_disk.iter().map(|it| it.0.as_str()).filter(|it| !it.starts_with(".published/")).collect();
        let listed: Vec<&str> = package.files.iter().map(|it| it.0).collect();
        assert_eq!(built, listed, "{}: files.rs lists every file of its directory", package.dir);
        let macro_crate = ROOTS.iter().any(|it| PACKAGES[it.package].dir == package.dir);
        let replaced: &[&str] = if macro_crate { &["Cargo.toml", "src/lib.rs"] } else { &["Cargo.toml"] };
        assert_eq!(published, replaced, "{}", package.dir);
        for (path, bytes) in package.files {
            let file = match replaced.contains(path) {
                true => dir.join(".published").join(path),
                false => dir.join(path),
            };
            assert_eq!(std::fs::read(&file).unwrap(), *bytes, "{}", file.display());
        }
    }
}

#[test]
fn a_macro_crate_is_built_from_its_published_source_less_its_attributes() {
    let attribute = |line: &str| {
        let line = line.trim_start();
        line.starts_with("#[proc_macro_derive(")
            || line == "#[proc_macro_attribute]"
            || line == "#[proc_macro]"
    };
    for root in &ROOTS {
        let package = &PACKAGES[root.package];
        let published = package.files.iter().find(|it| it.0 == "src/lib.rs").unwrap().1;
        let published = std::str::from_utf8(published).unwrap();
        let built = std::fs::read_to_string(vendor(package.dir).join("src/lib.rs")).unwrap();
        let expected: String =
            published.split_inclusive('\n').filter(|line| !attribute(line)).collect();
        assert_eq!(built, expected, "{}", package.dir);
        let declared = published.lines().filter(|line| attribute(line)).count();
        assert_eq!(declared, root.macros.len(), "{}: one entry for each macro", package.dir);
    }
}

#[test]
fn the_table_is_well_formed() {
    for package in &PACKAGES {
        let names: Vec<&str> = package.deps.iter().map(|it| it.0).collect();
        assert!(names.is_sorted(), "{}", package.dir);
        assert!(package.files.windows(2).all(|it| it[0].0 < it[1].0), "{}", package.dir);
        let manifest = package.files.iter().find(|it| it.0 == "Cargo.toml").unwrap().1;
        let manifest = std::str::from_utf8(manifest).unwrap();
        let features = manifest.split("\n[features]\n").nth(1).unwrap_or_default();
        let features = features.split("\n[").next().unwrap_or_default();
        for feature in package.features {
            let declared = features.lines().any(|line| {
                line.split_once('=').is_some_and(|(name, _)| name.trim() == *feature)
            });
            assert!(declared, "{}: no feature `{feature}`", package.dir);
        }
        // Every file a library compiles from is under `src/` (`mentioned`).
        for (path, bytes) in package.files.iter().filter(|it| compiled(it.0)) {
            let text = std::str::from_utf8(bytes).unwrap();
            for line in text.lines().map(str::trim_start).filter(|it| !it.starts_with("//")) {
                for named in ["include!(", "include_str!(", "include_bytes!("] {
                    assert!(!line.contains(named), "{}/{path}: {line}", package.dir);
                }
                if let Some(at) = line.strip_prefix("#[path = \"") {
                    assert!(!at.contains(".."), "{}/{path}: {line}", package.dir);
                }
            }
        }
    }
    assert_eq!(PACKAGES[QUOTE].build, Build::Nothing);
}
