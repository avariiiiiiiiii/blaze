//! The view twins of a module's source give the handles' nodes: for modules of every origin
//! (the crate root, a `mod foo;` file, an inline module, one a macro writes, a block with
//! items), [`ModuleData::definition_source_view`] has the file and variant of
//! [`ModuleData::definition_source`] and the same elements, and
//! [`ModuleData::declaration_source_view`] those of [`ModuleData::declaration_source`].

use syntax::{AstNode, NodeOrToken, SyntaxKind, SyntaxNode, TextRange, view};
use hir_expand::fixture;

use crate::{
    DefWithBodyId, ModuleDefId,
    expr_store::Body,
    nameres::{
        DefMap, ModuleData, ModuleOrigin, ModuleSource, ModuleSourceView, crate_def_map,
    },
    test_db::{TestDB, TestDBExt},
};

type Element = (SyntaxKind, TextRange, Option<String>);

/// The kind and range of each element under `node`, in preorder, with each token's text.
fn handle_elements(node: &SyntaxNode) -> Vec<Element> {
    node.descendants_with_tokens()
        .map(|it| match it {
            NodeOrToken::Node(it) => (it.kind(), it.text_range(), None),
            NodeOrToken::Token(it) => (it.kind(), it.text_range(), Some(it.text().to_owned())),
        })
        .collect()
}

/// [`handle_elements`] read through a view.
fn view_elements(node: view::Node<'_>) -> Vec<Element> {
    node.descendants_with_tokens()
        .map(|it| match it {
            NodeOrToken::Node(it) => (it.kind(), it.text_range(), None),
            NodeOrToken::Token(it) => (it.kind(), it.text_range(), Some(it.text().to_owned())),
        })
        .collect()
}

fn handle_variant(source: &ModuleSource) -> &'static str {
    match source {
        ModuleSource::SourceFile(_) => "SourceFile",
        ModuleSource::Module(_) => "Module",
        ModuleSource::BlockExpr(_) => "BlockExpr",
    }
}

fn view_variant(source: ModuleSourceView<'_>) -> &'static str {
    match source {
        ModuleSourceView::SourceFile(_) => "SourceFile",
        ModuleSourceView::Module(_) => "Module",
        ModuleSourceView::BlockExpr(_) => "BlockExpr",
    }
}

/// The origins seen, so the test fails if a fixture stops reaching one.
#[derive(Default)]
struct Seen {
    crate_root: usize,
    file: usize,
    inline: usize,
    in_macro: usize,
    block: usize,
}

fn compare_module(db: &TestDB, data: &ModuleData, seen: &mut Seen) {
    match data.origin {
        ModuleOrigin::CrateRoot { .. } => seen.crate_root += 1,
        ModuleOrigin::File { .. } => seen.file += 1,
        ModuleOrigin::Inline { .. } => seen.inline += 1,
        ModuleOrigin::BlockExpr { .. } => seen.block += 1,
    }

    let handle = data.definition_source(db);
    let view = data.definition_source_view(db);
    assert_eq!(handle.file_id, view.file_id, "{:?}", data.origin);
    assert_eq!(handle_variant(&handle.value), view_variant(view.value), "{:?}", data.origin);
    assert_eq!(
        handle_elements(&handle.value.node()),
        view_elements(view.value.node()),
        "{:?}",
        data.origin
    );
    if handle.file_id.is_macro() {
        seen.in_macro += 1;
    }

    match (data.declaration_source(db), data.declaration_source_view(db)) {
        (None, None) => {}
        (Some(handle), Some(view)) => {
            assert_eq!(handle.file_id, view.file_id, "{:?}", data.origin);
            assert_eq!(
                handle_elements(handle.value.syntax()),
                view_elements(view::AstNode::syntax(&view.value)),
                "{:?}",
                data.origin
            );
        }
        (handle, view) => panic!(
            "{:?}: the handle's declaration is {:?}, the view's {:?}",
            data.origin,
            handle.map(|it| it.value.syntax().text_range()),
            view.map(|it| view::AstNode::syntax(&it.value).text_range()),
        ),
    }
}

/// Every module of `def_map`, then those of the block def maps of its functions' bodies.
fn compare_def_map(db: &TestDB, def_map: &DefMap, seen: &mut Seen) {
    for (_, data) in def_map.modules(db) {
        compare_module(db, data, seen);
        for decl in data.scope(db).declarations() {
            let ModuleDefId::FunctionId(function) = decl else { continue };
            let body = Body::of(db, DefWithBodyId::FunctionId(function));
            for (_, block) in body.blocks(db) {
                compare_def_map(db, block, seen);
            }
        }
    }
}

#[test]
fn a_modules_source_views_are_its_handles() {
    let db = fixture::db(
        r#"
//- /lib.rs
mod file;
mod inline {
    pub fn in_inline() {
        struct InInline;
    }
}
macro_rules! m {
    () => {
        mod written {
            pub struct Written;
        }
    };
}
m!();
fn with_block() {
    struct Outer;
    {
        struct Inner;
        fn deeper() {
            struct Deepest;
        }
    }
}
//- /file.rs
pub mod sub {
    pub struct InSub;
}
fn in_file() {
    struct InFile;
}
"#,
    )
    .into_db();
    let mut seen = Seen::default();
    compare_def_map(&db, crate_def_map(&db, db.fetch_test_crate()), &mut seen);
    assert_eq!(seen.crate_root, 1);
    assert_eq!(seen.file, 1);
    // `inline`, `sub` and `written`.
    assert_eq!(seen.inline, 3);
    assert_eq!(seen.in_macro, 1);
    // The bodies of `in_inline`, `in_file` and `with_block`, its inner block, and `deeper`.
    assert_eq!(seen.block, 5);
}
