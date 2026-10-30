//! The view [`Semantics`] against the handle [`crate::Semantics`], node by node on one fixture:
//! every node of the file and of every macro expansion in it is asked of both (the handle node
//! found by the view's pointer under the handle's tree of the same file), and both must give
//! the same defs, types, resolutions, ranges and ancestors.

use base_db::EditionedFileId;
use blaze_alloc_count::assert_at_most;
use hir_expand::HirFileId;
use syntax::{AstNode as _, SyntaxNode, ast as handle, view::{self, AstNode as _}};
use test_fixture::WithFixture;

use super::{HasSourceView, Semantics};
use crate::{HasSource, ModuleDef};

const FIXTURE: &str = r#"
//- /main.rs crate:main
macro_rules! id { ($($t:tt)*) => { $($t)* }; }
macro_rules! make_fn { ($name:ident) => { fn $name() -> u32 { 1 } }; }

mod inner {
    pub struct Point { pub x: u32, pub y: u32 }
    pub struct Pair(pub u32, pub i64);
    pub enum Shape { Dot(Point), Line { from: Point, to: Point }, Empty = 3 }
    pub union Bits { pub a: u32, pub b: f32 }
    pub trait Area { const SIDES: u32; type Out; fn area(&self) -> u32; }
    impl Area for Point {
        const SIDES: u32 = 0;
        type Out = u32;
        fn area(&self) -> u32 { self.x * self.y }
    }
    impl Point { pub fn new(x: u32) -> Self { Point { x, y: x } } }
}

use inner::{Area, Pair, Point, Shape};

const LIMIT: usize = 4;
static NAME: &str = "n";
type Alias<'a, T, const N: usize> = &'a [T; N];

make_fn!(made);

fn generic<'a, T: Copy, const N: usize>(xs: &'a [T; N]) -> Option<&'a T> { xs.first() }

fn origin() -> Point { Point::new(0) }

fn main() {
    id! { let p = origin(); }
    let q = Point { x: 1, y: 2 };
    let Point { x, y: why } = q;
    let pair = Pair(1, 2);
    let Pair(first, _) = pair;
    let _sum = p.area() + x + why + first + pair.0 as u32 + made();
    let _s = Shape::Dot(p);
    let _l = Shape::Line { from: q, to: q };
    let arr = [0u8; LIMIT];
    let _g = generic(&arr);
    let add = |a: u32, b: u32| a + b;
    let _r = 'outer: loop { break 'outer add(id!(1), 2); };
    let _n = NAME.len() + <Point as Area>::SIDES as usize;
    match _s { Shape::Dot(Point { x: px, .. }) => { let _ = px; } Shape::Line { .. } | Shape::Empty => {} }
}
"#;

/// The handle node `node` stands for: its pointer found under the handle tree of `file`.
fn handle_of(hsema: &crate::Semantics<'_>, file: HirFileId, node: view::Node<'_>) -> SyntaxNode {
    view::ptr(node).to_node(&hsema.parse_or_expand(file))
}

/// The file's root and the roots of every macro expansion under it, nested ones included,
/// each with its file.
fn roots<'db>(
    vsema: &Semantics<'db>,
    file_id: EditionedFileId,
) -> Vec<(HirFileId, view::Node<'db>)> {
    let mut roots = vec![(HirFileId::from(file_id), vsema.parse(file_id).syntax())];
    let mut at = 0;
    while let Some(&(_, root)) = roots.get(at) {
        at += 1;
        for call in root.descendants().filter_map(view::MacroCall::cast) {
            if let Some(expansion) = vsema.expand_macro_call(call) {
                roots.push((expansion.file_id, expansion.value));
            }
        }
    }
    roots
}

macro_rules! same_def {
    ($vsema:ident, $hsema:ident, $file:ident, $node:ident, $checked:ident: $($kind:ident),* $(,)?) => {$(
        if let Some(v) = view::$kind::cast($node) {
            let h = handle::$kind::cast(handle_of(&$hsema, $file, $node))
                .expect("the handle node has the view's kind");
            assert_eq!(
                $vsema.to_def(v),
                $hsema.to_def(&h),
                "to_def of the {} at {:?} in {:?}",
                stringify!($kind),
                $node.text_range(),
                $file,
            );
            $checked += 1;
        }
    )*};
}

#[test]
fn to_def_is_the_handles_for_every_node() {
    let (db, file_id) = base_db::Db::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let hsema = crate::Semantics::new(&db);
        hsema.parse(file_id);
        let mut checked = 0u32;
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                same_def!(vsema, hsema, file, node, checked:
                    Module, Struct, Enum, Union, Trait, Impl, TypeAlias, Const, Static, Fn,
                    RecordField, TupleField, Variant, TypeParam, LifetimeParam, ConstParam,
                    Macro, SelfParam, Label, Adt, IdentPat, MacroCall,
                );
            }
        }
        assert!(checked > 60, "the fixture has defs of every kind, {checked} checked");
    });
}

#[test]
fn ancestors_ranges_and_scopes_are_the_handles_for_every_node() {
    let (db, file_id) = base_db::Db::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let hsema = crate::Semantics::new(&db);
        hsema.parse(file_id);
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                let h = handle_of(&hsema, file, node);
                assert_eq!(vsema.hir_file_for(node), hsema.hir_file_for(&h));
                let views = vsema
                    .ancestors_with_macros(node)
                    .map(|it| (it.kind(), it.text_range()));
                let handles = hsema.ancestors_with_macros(h.clone()).map(|it| (it.kind(), it.text_range()));
                assert!(views.eq(handles), "ancestors of {:?} in {file:?}", node.text_range());
                assert_eq!(vsema.original_range(node), hsema.original_range(&h));
                assert_eq!(vsema.original_range_opt(node), hsema.original_range_opt(&h));
                assert_eq!(
                    vsema.scope(node).map(|it| (it.module(), it.krate(), it.file_id())),
                    hsema.scope(&h).map(|it| (it.module(), it.krate(), it.file_id())),
                    "scope of {:?} in {file:?}",
                    node.text_range(),
                );
            }
        }
    });
}

#[test]
fn types_and_resolutions_are_the_handles_for_every_node() {
    let (db, file_id) = base_db::Db::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let hsema = crate::Semantics::new(&db);
        hsema.parse(file_id);
        let (mut exprs, mut paths) = (0u32, 0u32);
        for (file, root) in roots(&vsema, file_id) {
            for node in root.descendants() {
                let h = || handle_of(&hsema, file, node);
                let at = (node.text_range(), file);
                if let Some(v) = view::Expr::cast(node) {
                    let h = handle::Expr::cast(h()).unwrap();
                    assert_eq!(
                        vsema.type_of_expr(v).map(|it| (it.original, it.adjusted)),
                        hsema.type_of_expr(&h).map(|it| (it.original, it.adjusted)),
                        "type of the expression at {at:?}",
                    );
                    assert_eq!(
                        vsema.resolve_expr_as_callable(v).map(|it| it.return_type()),
                        hsema.resolve_expr_as_callable(&h).map(|it| it.return_type()),
                        "callable at {at:?}",
                    );
                    exprs += 1;
                }
                if let Some(v) = view::Pat::cast(node) {
                    let h = handle::Pat::cast(h()).unwrap();
                    assert_eq!(
                        vsema.type_of_pat(v).map(|it| (it.original, it.adjusted)),
                        hsema.type_of_pat(&h).map(|it| (it.original, it.adjusted)),
                        "type of the pattern at {at:?}",
                    );
                }
                if let Some(v) = view::IdentPat::cast(node) {
                    let h = handle::IdentPat::cast(h()).unwrap();
                    assert_eq!(vsema.type_of_binding_in_pat(v), hsema.type_of_binding_in_pat(&h));
                    assert_eq!(vsema.resolve_bind_pat_to_const(v), hsema.resolve_bind_pat_to_const(&h));
                }
                if let Some(v) = view::Path::cast(node) {
                    let h = handle::Path::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_path(v), hsema.resolve_path(&h), "path at {at:?}");
                    let per_ns = |it: crate::PathResolutionPerNs<'_>| (it.type_ns, it.value_ns, it.macro_ns);
                    assert_eq!(
                        vsema.resolve_path_per_ns(v).map(per_ns),
                        hsema.resolve_path_per_ns(&h).map(per_ns),
                        "path per namespace at {at:?}",
                    );
                    let scope = hsema.scope(h.syntax()).expect("a path has a scope");
                    assert_eq!(scope.speculative_resolve_view(v), scope.speculative_resolve(&h));
                    paths += 1;
                }
                if let Some(v) = view::Type::cast(node) {
                    let h = handle::Type::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_type(v), hsema.resolve_type(&h), "type at {at:?}");
                }
                if let Some(v) = view::MethodCallExpr::cast(node) {
                    let h = handle::MethodCallExpr::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_method_call(v), hsema.resolve_method_call(&h));
                    assert_eq!(
                        vsema.resolve_method_call_as_callable(v).map(|it| it.return_type()),
                        hsema.resolve_method_call_as_callable(&h).map(|it| it.return_type()),
                    );
                }
                if let Some(v) = view::FieldExpr::cast(node) {
                    let h = handle::FieldExpr::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_field(v), hsema.resolve_field(&h));
                }
                if let Some(v) = view::RecordExprField::cast(node) {
                    let h = handle::RecordExprField::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_record_field(v), hsema.resolve_record_field(&h));
                }
                if let Some(v) = view::RecordPatField::cast(node) {
                    let h = handle::RecordPatField::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_record_pat_field(v), hsema.resolve_record_pat_field(&h));
                }
                if let Some(v) = view::TupleStructPat::cast(node) {
                    let h = handle::TupleStructPat::cast(h()).unwrap();
                    assert_eq!(
                        vsema.resolve_tuple_struct_pat_fields(v),
                        hsema.resolve_tuple_struct_pat_fields(&h),
                    );
                }
                if let Some(v) = view::RecordExpr::cast(node) {
                    let h = handle::RecordExpr::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_variant(v), hsema.resolve_variant(h));
                }
                if let Some(v) = view::MacroCall::cast(node) {
                    let h = handle::MacroCall::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_macro_call(v), hsema.resolve_macro_call(&h));
                    assert_eq!(vsema.resolve_macro_call_arm(v), hsema.resolve_macro_call_arm(&h));
                }
                if let Some(v) = view::Lifetime::cast(node) {
                    let h = handle::Lifetime::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_lifetime_param(v), hsema.resolve_lifetime_param(&h));
                    assert_eq!(vsema.resolve_label(v), hsema.resolve_label(&h));
                }
                if let Some(v) = view::Item::cast(node) {
                    let h = handle::Item::cast(h()).unwrap();
                    assert_eq!(vsema.resolve_attr_macro_call(v), hsema.resolve_attr_macro_call(&h));
                }
            }
        }
        assert!(exprs > 50 && paths > 30, "{exprs} expressions, {paths} paths");
    });
}

#[test]
fn descending_a_token_is_the_handles_for_every_token() {
    let (db, file_id) = base_db::Db::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let hsema = crate::Semantics::new(&db);
        let hfile = hsema.parse(file_id);
        let mut into_macros = 0u32;
        for token in vsema.parse(file_id).syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
            if token.text_range().is_empty() {
                continue;
            }
            let h = hfile
                .syntax()
                .covering_element(token.text_range())
                .into_token()
                .expect("a token's range covers the handle token");
            let views: Vec<_> = vsema
                .descend_into_macros_exact(token)
                .into_iter()
                .map(|it| (it.kind(), it.text_range(), vsema.hir_file_for(it.parent().unwrap())))
                .collect();
            let handles: Vec<_> = hsema
                .descend_into_macros_exact(h)
                .into_iter()
                .map(|it| (it.kind(), it.text_range(), hsema.hir_file_for(&it.parent().unwrap())))
                .collect();
            assert_eq!(views, handles, "descending {:?} at {:?}", token.text(), token.text_range());
            into_macros += u32::from(views.iter().any(|(.., file)| file.is_macro()));
        }
        assert!(into_macros >= 3, "the tokens of `id!` and `make_fn!` descend, {into_macros} did");
    });
}

#[test]
fn source_view_is_the_handle_source_for_every_def() {
    let (db, file_id) = base_db::Db::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let krate = vsema.file_to_module_def(file_id.file_id(&db)).expect("a module").krate(&db);
        let same = |what: &str, v: Option<(HirFileId, syntax::SyntaxKind, syntax::TextRange)>,
                    h: Option<(HirFileId, syntax::SyntaxKind, syntax::TextRange)>| {
            assert_eq!(v, h, "source of {what}");
        };
        let mut checked = 0u32;
        for module in krate.modules(&db) {
            let def = module.definition_source_view(&db);
            let hdef = module.definition_source(&db);
            assert_eq!(
                (def.file_id, def.value.node().kind(), def.value.node().text_range()),
                (hdef.file_id, hdef.value.node().kind(), hdef.value.node().text_range()),
            );
            assert_eq!(
                module.declaration_source_view(&db).map(|it| (it.file_id, it.value.syntax().text_range())),
                module.declaration_source(&db).map(|it| (it.file_id, it.value.syntax().text_range())),
            );
            for decl in module.declarations(&db) {
                macro_rules! compare {
                    ($def:expr) => {{
                        let def = $def;
                        same(
                            &format!("{def:?}"),
                            def.source_view(&db).map(|it| (it.file_id, it.value.syntax().kind(), it.value.syntax().text_range())),
                            def.source(&db).map(|it| (it.file_id, it.value.syntax().kind(), it.value.syntax().text_range())),
                        );
                        checked += 1;
                    }};
                }
                match decl {
                    ModuleDef::Function(it) => compare!(it),
                    ModuleDef::Adt(it) => {
                        compare!(it);
                        for variant in it.as_enum().map(|it| it.variants(&db)).unwrap_or_default() {
                            compare!(variant);
                            for field in variant.fields(&db) {
                                compare!(field);
                            }
                        }
                        if let crate::Adt::Struct(s) = it {
                            for field in s.fields(&db) {
                                compare!(field);
                            }
                        }
                    }
                    ModuleDef::Const(it) => compare!(it),
                    ModuleDef::Static(it) => compare!(it),
                    ModuleDef::Trait(it) => compare!(it),
                    ModuleDef::TypeAlias(it) => compare!(it),
                    ModuleDef::Macro(it) => compare!(it),
                    _ => {}
                }
            }
            for impl_ in module.impl_defs(&db) {
                let v = impl_.source_view(&db).map(|it| (it.file_id, it.value.syntax().text_range()));
                let h = impl_.source(&db).map(|it| (it.file_id, it.value.syntax().text_range()));
                assert_eq!(v, h, "source of an impl");
                checked += 1;
            }
        }
        assert!(checked > 20, "{checked} defs checked");
    });
}

#[test]
fn a_warm_lookup_walk_allocates_nothing() {
    let (db, file_id) = base_db::Db::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let vsema = Semantics::new(&db);
        let root = vsema.parse(file_id).syntax();
        let strukt = root.descendants().find_map(view::Struct::cast).expect("`struct Point`");
        let field = root.descendants().find_map(view::RecordField::cast).expect("field `x`");
        let walk = || {
            let s = vsema.to_def(strukt);
            let f = vsema.to_def(field);
            let file = vsema.hir_file_for(field.syntax());
            let depth = vsema.ancestors_with_macros(field.syntax()).count();
            (s, f, file, depth)
        };
        let warm = walk();
        let measured = assert_at_most(0, "to_def, hir_file_for and ancestors_with_macros", walk);
        assert_eq!(warm, measured);
        assert!(warm.0.is_some() && warm.1.is_some());
    });
}
