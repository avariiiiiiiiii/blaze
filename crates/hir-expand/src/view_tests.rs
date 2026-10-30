//! The view twins of hir-expand's handle APIs against the handle functions, element by element,
//! over a database: a real file, and the macro files of calls of builtin macros made by hand; the
//! eager expansion, what a call expands to and path lowering against stored expectations (their
//! handle forms are deleted); and the allocation budgets of the twins'
//! walks (docs/v2/plan/lanes/frontend/tree-move-expand.md).

use std::ops::ControlFlow;

use base_db::{
    AbsPathBuf, Crate, CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, Db, EditionedFileId,
    Env, FileId, FileSet, SourceRoot, SourceRootId, TargetKind, VfsPath, salsa::Durability,
};
use cfg::CfgOptions;
use span::{Edition, SyntaxContext};
use syntax::{
    AstNode, AstPtr, SourceFile, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, ast,
    view::{self, AstNode as _},
};
use triomphe::Arc;

use crate::{
    AstId, ExpandTo, ExpansionInfo, HirFileId, InFile, MacroCallId, MacroCallKind, MacroDefId,
    MacroDefKind,
    attrs::{
        AstKeyValueMetaExt, AstPathExt, AttrId, collect_item_tree_attrs,
        collect_item_tree_attrs_view, expand_cfg_attr, expand_cfg_attr_view,
    },
    builtin::{BuiltinFnLikeExpander, EagerExpander},
    eager::expand_eager_macro_input_view,
    mod_path::{ModPath, PathKind},
    name::AsName,
};

const FIXTURE: &str = r#"
#[rustc_builtin_macro]
macro_rules! stringify { () => {} }
#[rustc_builtin_macro]
macro_rules! line { () => {} }
#[rustc_builtin_macro]
macro_rules! concat { () => {} }

/// The fixture's struct.
#[cfg_attr(all(), derive(Clone), allow(dead_code))]
#[repr(C)]
#[doc = "more"]
pub struct S { pub a: u32, b: (u8, i16) }

mod m { #![allow(unused)] pub mod g { pub fn r#try<T>(x: u32) -> u32 { x } } }

fn f(x: u32, v: S) -> u32 {
    let s = stringify!(a + b * c);
    let l = line!();
    let c = concat!("a", stringify!(b), line!(), concat!("c", 1), nope!());
    let S { a, b: (y, _) } = v;
    crate::m::g::r#try::<u8>(x) + self::h() + v.a + v.b.0 as u32 + super::k()
}
"#;

/// Calls in every position a macro call can be in, and some it can't.
const POSITIONS: &str = r#"
m!();
fn g() { m!(); let m!() = m!(); let _: m!() = (m!(), [m!()]); match m!() { m!() => m!() } }
struct T(m!());
impl T { m!(); }
trait U { m!(); }
extern "C" { m!(); }
const _: [u8; m!()] = [];
"#;

struct Fixture {
    db: Db,
    file: EditionedFileId,
    krate: Crate,
}

/// A database of one crate whose root is `FIXTURE`.
fn fixture() -> Fixture {
    let mut db = Db::for_tests();
    let id = FileId::from_raw(0);
    db.set_file_text_with_durability(id, FIXTURE, Durability::LOW);
    db.set_file_source_root_with_durability(id, SourceRootId(0), Durability::MEDIUM);
    let mut set = FileSet::default();
    set.insert(id, VfsPath::new_virtual_path("/a/lib.rs".to_owned()));
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
    let mut graph = CrateGraphBuilder::default();
    let builder = graph.add_crate_root(
        id,
        Edition::CURRENT,
        None,
        None,
        CfgOptions::default(),
        None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    );
    let krate = graph.set_in_db(&mut db)[&builder];
    let file = EditionedFileId::new(&db, id, Edition::CURRENT);
    Fixture { db, file, krate }
}

/// The builtin macro the file's `macro_rules! name` stands for, as `kind` of its definition.
fn builtin(
    f: &Fixture,
    name: &str,
    kind: impl FnOnce(AstId<ast::Macro>) -> MacroDefKind,
) -> MacroDefId {
    let file = HirFileId::from(f.file);
    let tree = f.file.parse(&f.db).tree();
    let rules = tree
        .syntax()
        .descendants()
        .filter_map(ast::MacroRules::cast)
        .find(|it| it.name().is_some_and(|it| it.to_string() == name))
        .expect("the macro's definition");
    let ast_id = InFile::new(
        file,
        file.ast_id_map(&f.db).ast_id_for_ptr(AstPtr::new(&ast::Macro::MacroRules(rules))),
    );
    MacroDefId {
        krate: f.krate,
        edition: Edition::CURRENT,
        kind: kind(ast_id),
        local_inner: false,
        allow_internal_unsafe: false,
        edition_panic: false,
    }
}

/// The file's macro calls, in preorder, with their IDs.
fn calls(f: &Fixture) -> Vec<(ast::MacroCall, AstId<ast::MacroCall>)> {
    let file = HirFileId::from(f.file);
    let map = file.ast_id_map(&f.db);
    let tree = f.file.parse(&f.db).tree();
    tree.syntax()
        .descendants()
        .filter_map(ast::MacroCall::cast)
        .map(|call| {
            let ast_id = InFile::new(file, map.ast_id_for_ptr(AstPtr::new(&call)));
            (call, ast_id)
        })
        .collect()
}

fn name_of(call: &ast::MacroCall) -> Option<String> {
    Some(call.path()?.segment()?.name_ref()?.to_string())
}

fn fn_like(f: &Fixture, name: &str, expander: BuiltinFnLikeExpander) -> MacroDefId {
    builtin(f, name, |id| MacroDefKind::BuiltIn(id, expander))
}

/// The calls of `stringify!` and `line!` of the file's body, each made a macro file.
fn macro_files(f: &Fixture) -> Vec<MacroCallId> {
    let stringify = fn_like(f, "stringify", BuiltinFnLikeExpander::Stringify);
    let line = fn_like(f, "line", BuiltinFnLikeExpander::Line);
    let made: Vec<_> = calls(f)
        .iter()
        .filter_map(|(call, ast_id)| {
            let def = match name_of(call)?.as_str() {
                "stringify" => stringify,
                "line" => line,
                _ => return None,
            };
            let expand_to = ExpandTo::of_call_site_view(ast_id.to_view(&f.db))
                .expect("a call in an expression");
            let kind = MacroCallKind::FnLike { ast_id: *ast_id, expand_to, eager: None };
            Some(def.make_call(&f.db, f.krate, kind, SyntaxContext::root(Edition::CURRENT), 0))
        })
        .collect();
    assert_eq!(made.len(), 2, "the `stringify!` and `line!` of `f`");
    made
}

/// The real file and the macro files.
fn files(f: &Fixture) -> Vec<HirFileId> {
    let mut files = vec![HirFileId::from(f.file)];
    files.extend(macro_files(f).into_iter().map(HirFileId::from));
    files
}

type Shape = Vec<(SyntaxKind, TextRange)>;

fn shape(node: &SyntaxNode) -> Shape {
    node.descendants_with_tokens().map(|it| (it.kind(), it.text_range())).collect()
}

fn shape_view(node: view::Node<'_>) -> Shape {
    node.descendants_with_tokens().map(|it| (it.kind(), it.text_range())).collect()
}

/// Every node of `root` with the same node of `view`, in preorder: the same kinds and ranges.
fn node_pairs<'t>(root: &SyntaxNode, view: view::Node<'t>) -> Vec<(SyntaxNode, view::Node<'t>)> {
    assert_eq!(shape(root), shape_view(view), "the view is the handle's tree");
    root.descendants().zip(view.descendants()).collect()
}

/// [`node_pairs`] of the tokens.
fn token_pairs<'t>(root: &SyntaxNode, view: view::Node<'t>) -> Vec<(SyntaxToken, view::Token<'t>)> {
    assert_eq!(shape(root), shape_view(view), "the view is the handle's tree");
    let tokens = root.descendants_with_tokens().filter_map(|it| it.into_token());
    tokens.zip(view.descendants_with_tokens().filter_map(|it| it.into_token())).collect()
}

fn root_ctxt(_: TextRange) -> SyntaxContext {
    SyntaxContext::root(Edition::CURRENT)
}

#[test]
fn roots_and_pointers_are_the_handles() {
    let f = fixture();
    let db = &f.db;
    for file in files(&f) {
        assert_eq!(shape(&file.parse_or_expand(db)), shape_view(file.root(db)), "{file:?}");
        let in_file = InFile::new(file, ());
        assert_eq!(shape(&in_file.file_syntax(db)), shape_view(in_file.root(db)), "{file:?}");
    }
    for (call, ast_id) in calls(&f) {
        let (node, view) = (ast_id.to_node(db), ast_id.to_view(db));
        assert_eq!(shape(node.syntax()), shape_view(view.syntax()), "{call}");
        let (node, view) = (ast_id.to_in_file_node(db), ast_id.to_in_file_view(db));
        assert_eq!(node.file_id, view.file_id, "{call}");
        assert_eq!(shape(node.value.syntax()), shape_view(view.value.syntax()), "{call}");
        let range = node.value.syntax().text_range();
        assert_eq!((node.file_id, range), (view.file_id, view.node_file_range_view().range));
        let (syntax, syntax_view) = (node.syntax(), view.syntax_view());
        assert_eq!(
            (syntax.file_id, shape(syntax.value)),
            (syntax_view.file_id, shape_view(syntax_view.value))
        );
        let ptr = InFile::new(ast_id.file_id, ast_id.to_ptr(db));
        assert_eq!(shape(ptr.to_node(db).syntax()), shape_view(ptr.to_view(db).syntax()), "{call}");
    }
}

#[test]
fn walks_and_ranges_up_are_the_handles() {
    let f = fixture();
    let db = &f.db;
    let up = |it: InFile<SyntaxNode>| (it.file_id, it.value.kind(), it.value.text_range());
    let up_view = |it: InFile<view::Node<'_>>| (it.file_id, it.value.kind(), it.value.text_range());
    for file in files(&f) {
        for (node, view) in node_pairs(&file.parse_or_expand(db), file.root(db)) {
            let (handle, view) = (InFile::new(file, node), InFile::new(file, view));
            let at = (file, view.value.kind(), view.value.text_range());
            assert_eq!(
                handle.clone().ancestors_with_macros(db).map(up).collect::<Vec<_>>(),
                view.ancestors_with_macros_view(db).map(up_view).collect::<Vec<_>>(),
                "{at:?}"
            );
            assert_eq!(
                handle.clone().parent_ancestors_with_macros(db).map(up).collect::<Vec<_>>(),
                view.parent_ancestors_with_macros_view(db).map(up_view).collect::<Vec<_>>(),
                "{at:?}"
            );
            assert_eq!(
                handle.clone().original_file_range_rooted(db),
                view.original_file_range_rooted_view(db),
                "{at:?}"
            );
            assert_eq!(
                handle.clone().original_file_range_with_macro_call_input(db),
                view.original_file_range_with_macro_call_input_view(db),
                "{at:?}"
            );
            assert_eq!(
                handle.as_ref().original_file_range_opt(db),
                view.original_file_range_opt_view(db),
                "{at:?}"
            );
            let rooted = handle.clone().original_syntax_node_rooted(db);
            let rooted_view = view.original_syntax_node_rooted_view(db);
            assert_eq!(
                rooted.map(|it| (it.file_id, shape(&it.value))),
                rooted_view.map(|it| (it.file_id, shape_view(it.value))),
                "{at:?}"
            );
            if let (Some(item), Some(item_view)) =
                (ast::Item::cast(handle.value.clone()), view::Item::cast(view.value))
            {
                assert_eq!(
                    InFile::new(file, item)
                        .original_ast_node_rooted(db)
                        .map(|it| (it.file_id, shape(it.value.syntax()))),
                    InFile::new(file, item_view)
                        .original_ast_node_rooted_view(db)
                        .map(|it| (it.file_id, shape_view(it.value.syntax()))),
                    "{at:?}"
                );
            }
        }
        for (token, view) in token_pairs(&file.parse_or_expand(db), file.root(db)) {
            let at = (file, view.kind(), view.text_range());
            assert_eq!(
                InFile::new(file, token.clone()).original_file_range(db),
                InFile::new(file, view).original_file_range(db),
                "{at:?}"
            );
            assert_eq!(
                InFile::new(file, token).original_file_range_opt(db),
                InFile::new(file, view).original_file_range_opt(db),
                "{at:?}"
            );
        }
    }
}

#[test]
fn calls_and_expansions_are_the_handles() {
    let f = fixture();
    let db = &f.db;
    let in_file = |it: InFile<SyntaxNode>| (it.file_id, shape(&it.value));
    let in_file_view = |it: InFile<view::Node<'_>>| (it.file_id, shape_view(it.value));
    for id in macro_files(&f) {
        let loc = id.loc(db);
        assert_eq!(in_file(loc.to_node(db)), in_file_view(loc.to_node_view(db)), "{id:?}");
        let (item, item_view) = (loc.to_node_item(db), loc.to_node_item_view(db));
        assert_eq!(
            (item.file_id, shape(item.value.syntax())),
            (item_view.file_id, shape_view(item_view.value.syntax())),
            "{id:?}"
        );
        assert_eq!(in_file(id.call_node(db)), in_file_view(id.call_node_view(db)), "{id:?}");
        let file = HirFileId::from(id);
        assert_eq!(file.call_node(db).map(in_file), file.call_node_view(db).map(in_file_view));
        assert_eq!(
            file.original_call_node(db).map(|it| (it.file_id, shape(&it.value))),
            file.original_call_node_view(db).map(|it| (it.file_id, shape_view(it.value))),
            "{id:?}"
        );

        let info = ExpansionInfo::new(db, id);
        let (expanded, expanded_view) = (info.expanded(), info.expanded_view());
        assert_eq!(
            (expanded.file_id, shape(&expanded.value)),
            (expanded_view.file_id, shape_view(expanded_view.value)),
            "{id:?}"
        );
        let (arg, arg_view) = (info.arg(), info.arg_view());
        assert_eq!(
            (arg.file_id, arg.value.map(shape)),
            (arg_view.file_id, arg_view.value.map(shape_view)),
            "{id:?}"
        );
        let Some(arg_node) = arg_view.value else { continue };
        let map = arg_view.file_id.span_map(db);
        for token in arg_node.descendants_with_tokens().filter_map(|it| it.into_token()) {
            let span = map.span_for_range(token.text_range());
            let down = info.map_range_down(span).map(|it| {
                (it.file_id, it.value.map(|(t, ctx)| (t.text_range(), ctx)).collect::<Vec<_>>())
            });
            let down_view = info.map_range_down_view(span).map(|it| {
                (it.file_id, it.value.map(|(t, ctx)| (t.text_range(), ctx)).collect::<Vec<_>>())
            });
            assert_eq!(down, down_view, "{id:?} {span:?}");
            // What the deleted handle `map_range_down_exact` gave, over the handle tree.
            let exact = (span.anchor.ast_id != span::NO_DOWNMAP_ERASED_FILE_AST_ID_MARKER).then(|| {
                let tokens = info.exp_map.ranges_with_span_exact(span).flat_map(|(range, ctx)| {
                    expanded.value.covering_element(range).into_token().zip(Some(ctx))
                });
                (expanded.file_id, tokens.map(|(t, ctx)| (t.text_range(), ctx)).collect::<Vec<_>>())
            });
            let exact_view = info.map_range_down_exact_view(span).map(|it| {
                (it.file_id, it.value.map(|(t, ctx)| (t.text_range(), ctx)).collect::<Vec<_>>())
            });
            assert_eq!(exact, exact_view, "{id:?} {span:?}");
        }
    }
}

#[test]
fn what_a_call_expands_to_in_every_position() {
    use ExpandTo::*;
    let parse = SourceFile::parse(POSITIONS, Edition::CURRENT);
    let got: Vec<_> = parse
        .root()
        .descendants()
        .filter_map(view::MacroCall::cast)
        .map(ExpandTo::of_call_site_view)
        .collect();
    let expected = [
        // `m!();` in the file
        Items,
        // `fn g`: a statement, a pattern and its initializer, a type, a tuple's and an array's
        // element, a scrutinee, an arm's pattern and its body
        Statements,
        Pattern,
        Expr,
        Type,
        Expr,
        Expr,
        Expr,
        Pattern,
        Expr,
        // a tuple field's type, the impl's, trait's and extern block's items, an array length
        Type,
        Items,
        Items,
        Items,
        Expr,
    ];
    assert_eq!(got, expected.map(Ok));
}

#[test]
fn names_and_paths_are_the_handles() {
    let f = fixture();
    let db = &f.db;
    let file = HirFileId::from(f.file);
    for (node, view) in node_pairs(&file.parse_or_expand(db), file.root(db)) {
        if let (Some(h), Some(v)) = (ast::NameRef::cast(node.clone()), view::NameRef::cast(view)) {
            assert_eq!(h.as_name(), v.as_name(), "{h}");
        }
        if let (Some(h), Some(v)) = (ast::Name::cast(node.clone()), view::Name::cast(view)) {
            assert_eq!(h.as_name(), v.as_name(), "{h}");
        }
        if let (Some(h), Some(v)) =
            (ast::RecordPatField::cast(node.clone()), view::RecordPatField::cast(view))
        {
            let (name, name_view) = (h.field_name(), v.field_name());
            assert_eq!(name.map(|it| it.as_name()), name_view.map(|it| it.as_name()), "{h}");
        }
        if let (Some(h), Some(v)) =
            (ast::FieldExpr::cast(node.clone()), view::FieldExpr::cast(view))
        {
            let (field, field_view) = (h.field_access(), v.field_access());
            assert_eq!(field.map(|it| it.as_name()), field_view.map(|it| it.as_name()), "{h}");
        }
        if let (Some(h), Some(v)) = (ast::Path::cast(node.clone()), view::Path::cast(view)) {
            assert_eq!(h.is1("Clone"), v.is1("Clone"), "{h}");
            assert_eq!(h.as_one_segment(), v.as_one_segment(), "{h}");
            assert_eq!(h.as_up_to_two_segment(), v.as_up_to_two_segment(), "{h}");
            assert_eq!(h.qualifier().as_one_segment(), v.qualifier().as_one_segment(), "{h}");
        }
        if let (Some(h), Some(v)) = (ast::KeyValueMeta::cast(node), view::KeyValueMeta::cast(view))
        {
            assert_eq!(h.value_string(), v.value_string(), "{h}");
        }
    }
}

#[test]
fn paths_lower_to_mod_paths() {
    let f = fixture();
    let db = &f.db;
    let lowered: Vec<_> = HirFileId::from(f.file)
        .root(db)
        .descendants()
        .filter_map(view::Path::cast)
        .filter(|it| it.syntax().parent().and_then(view::Path::cast).is_none())
        .map(|it| {
            let path = ModPath::from_src_view(db, it, &mut root_ctxt);
            let path = path.map(|it| (it.kind, it.display_verbatim(db).to_string()));
            (it.syntax().text().to_string(), path)
        })
        .collect();
    for (text, kind, display) in [
        ("self::h", PathKind::SELF, "self::h"),
        ("super::k", PathKind::Super(1), "super::k"),
        ("stringify", PathKind::Plain, "stringify"),
        ("u32", PathKind::Plain, "u32"),
    ] {
        let want = (text.to_owned(), Some((kind, display.to_owned())));
        assert!(lowered.contains(&want), "{text}: {lowered:?}");
    }
    let call = lowered.iter().find(|(text, _)| text.starts_with("crate::m::g::"));
    let call = call.expect("the path of the call of `r#try`");
    assert_eq!(call.1.as_ref().map(|it| it.0), Some(PathKind::Crate), "{call:?}");
}

#[test]
fn attributes_are_the_handles() {
    let f = fixture();
    let db = &f.db;
    let cfg = CfgOptions::default();
    let file = HirFileId::from(f.file);
    let mut adts = 0;
    for (node, view) in node_pairs(&file.parse_or_expand(db), file.root(db)) {
        let (Some(handle), Some(view)) =
            (ast::AnyHasAttrs::cast(node), view::AnyHasAttrs::cast(view))
        else {
            continue;
        };
        let (mut metas, mut metas_view) = (Vec::new(), Vec::new());
        let _ = expand_cfg_attr(ast::attrs_including_inner(&handle), || &cfg, |meta, top| {
            metas.push((meta.syntax().text_range(), top.syntax().text_range()));
            ControlFlow::<()>::Continue(())
        });
        let _ = expand_cfg_attr_view(view::attrs_including_inner(&view), || &cfg, |meta, top| {
            metas_view.push((meta.syntax().text_range(), top.syntax().text_range()));
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(metas, metas_view, "{handle}");

        let (mut kept, mut kept_view) = (Vec::new(), Vec::new());
        let attrs = ast::attrs_including_inner(&handle);
        let result = collect_item_tree_attrs(attrs, || &cfg, |meta, top| {
            kept.push((meta.syntax().text_range(), top.syntax().text_range()));
            ControlFlow::<()>::Continue(())
        });
        let result_view = collect_item_tree_attrs_view(view, || &cfg, |meta, top| {
            kept_view.push((meta.syntax().text_range(), top.syntax().text_range()));
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(kept, kept_view, "{handle}");
        assert_eq!(result.is_some(), result_view.is_some(), "{handle}");

        if let Some(adt) = ast::Adt::cast(handle.syntax().clone()) {
            let ast_id = InFile::new(file, file.ast_id_map(db).ast_id_for_ptr(AstPtr::new(&adt)));
            let adt_view = ast_id.to_view(db);
            for index in 0..kept.len() as u32 {
                let id = AttrId::from_item_tree_index(index);
                let (attr, meta) = id.find_attr_range(db, f.krate, ast_id);
                let (attr_view, meta_view) = id.find_attr_range_view(db, f.krate, ast_id);
                assert_eq!(
                    (attr.syntax().text_range(), meta.syntax().text_range()),
                    (attr_view.syntax().text_range(), meta_view.syntax().text_range()),
                    "{adt} attribute {index}"
                );
                let (_, meta) = id.find_attr_range_with_source(db, f.krate, &adt);
                let owner = view::AnyHasAttrs::new(adt_view);
                let (_, meta_view) = id.find_attr_range_with_source_view(db, f.krate, owner);
                assert_eq!(meta.syntax().text_range(), meta_view.syntax().text_range());
            }
            // `derive(Clone)`, `allow(dead_code)`, `repr(C)`, `doc = ..`; the item tree keeps
            // only the `derive`.
            assert_eq!((metas.len(), kept.len()), (4, 1), "{adt}");
            adts += 1;
        }
    }
    assert_eq!(adts, 1, "`S`");
}

#[test]
fn an_eager_expansion_replaces_its_nested_calls() {
    let f = fixture();
    let db = &f.db;
    let stringify = fn_like(&f, "stringify", BuiltinFnLikeExpander::Stringify);
    let line = fn_like(&f, "line", BuiltinFnLikeExpander::Line);
    let concat = builtin(&f, "concat", |id| MacroDefKind::BuiltInEager(id, EagerExpander::Concat));
    let resolver = |path: &ModPath| match path.as_ident()?.as_str() {
        "stringify" => Some(stringify),
        "line" => Some(line),
        "concat" => Some(concat),
        _ => None,
    };
    let root = HirFileId::from(f.file).root(db);
    let views: Vec<_> = root.descendants().filter_map(view::MacroCall::cast).collect();
    let ctxt = SyntaxContext::root(Edition::CURRENT);
    let mut expanded = 0;
    for ((call, ast_id), view) in calls(&f).into_iter().zip(views) {
        if name_of(&call).as_deref() != Some("concat") {
            continue;
        }
        let mut nested = Vec::new();
        let result = expand_eager_macro_input_view(
            db, f.krate, view, ast_id, concat, ctxt, 0, 128, &resolver, &mut nested,
        );
        assert!(result.err.is_some(), "`nope!` is unresolved");
        let id = result.value.expect("the call expands past the unresolved one");
        let defs: Vec<_> = nested.iter().map(|(_, id)| id.loc(db).def).collect();
        assert_eq!(defs, [stringify, line, concat], "the nested calls, in preorder");
        let MacroCallKind::FnLike { eager: Some(info), .. } = &id.loc(db).kind else {
            panic!("an eager call");
        };
        let arg = info.arg.to_string();
        for kept in ["\"a\"", "\"b\"", "\"c1\"", "nope"] {
            assert!(arg.contains(kept), "{kept} in {arg}");
        }
        for replaced in ["stringify", "line", "concat"] {
            assert!(!arg.contains(replaced), "{replaced} in {arg}");
        }
        expanded += 1;
    }
    assert_eq!(expanded, 1);
}

#[test]
fn the_twins_walks_allocate_nothing() {
    let f = fixture();
    let db = &f.db;
    for file in files(&f) {
        let root = file.root(db);
        let walk = || {
            root.descendants()
                .map(|node| InFile::new(file, node).ancestors_with_macros_view(db).count())
                .sum::<usize>()
        };
        let warm = walk();
        let (again, used) = blaze_alloc_count::measure(walk);
        assert_eq!(again, warm);
        assert_eq!(used.allocations, 0, "{file:?}: the walk up from every node, budget 0");
    }

    let root = HirFileId::from(f.file).root(db);
    let names =
        || root.descendants().filter_map(view::NameRef::cast).map(|it| it.as_name()).count();
    let warm = names();
    let (again, used) = blaze_alloc_count::measure(names);
    assert_eq!(again, warm);
    assert_eq!(used.allocations, 0, "a `Name` of every name reference, warm, budget 0");

    let parse = SourceFile::parse(POSITIONS, Edition::CURRENT);
    let kinds = || {
        parse
            .root()
            .descendants()
            .filter_map(view::MacroCall::cast)
            .filter(|it| ExpandTo::of_call_site_view(*it).is_ok())
            .count()
    };
    let warm = kinds();
    let (again, used) = blaze_alloc_count::measure(kinds);
    assert_eq!(again, warm);
    assert_eq!(used.allocations, 0, "what each call expands to, budget 0");

    let cfg = CfgOptions::default();
    let item = SourceFile::parse("#[a] #[b(c)] #[c = \"d\"] struct E;", Edition::CURRENT);
    let owner = item.root().descendants().find_map(view::AnyHasAttrs::cast).expect("the struct");
    let attrs = || {
        let mut n = 0;
        let _ = collect_item_tree_attrs_view(owner, || &cfg, |_, _| {
            n += 1;
            ControlFlow::<()>::Continue(())
        });
        n
    };
    let warm = attrs();
    let (again, used) = blaze_alloc_count::measure(attrs);
    assert_eq!((again, warm), (3, 3));
    assert_eq!(used.allocations, 0, "an item's attributes, budget 0");
}
