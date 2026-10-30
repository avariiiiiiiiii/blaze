//! The view twins of this module against their handles, element by element: the same node for
//! every item, child and use tree of a fixture rich in each.

use la_arena::{Idx, RawIdx};
use syntax::{SyntaxKind, TextRange};
use hir_expand::fixture;

use super::*;
use crate::{
    AdtId, EnumVariantId, GenericDefId, ModuleDefId,
    nameres::crate_def_map,
    signatures::EnumVariants,
    test_db::{TestDB, TestDBExt},
};

const FIXTURE: &str = r#"
//- /main.rs crate:main cfg:on
use std::{fmt::{self, Display}, collections::*, io as _};
use self::inner as renamed;
mod inner {
    pub struct Unit;
    pub fn nested<'n, N: Copy>(_: &'n N) {}
}
struct Tuple<'a, T: Copy, const N: usize>(#[cfg(on)] u8, #[cfg(off)] u16, &'a T);
struct Record<T> {
    a: T,
    #[cfg(off)]
    gone: u8,
    b: [T; 2],
}
union U {
    x: u32,
    #[cfg(not(on))]
    y: f32,
    z: i64,
}
enum E<'x> {
    A,
    B(u8, #[cfg(off)] u8, &'x str),
    C { f: u8, #[cfg(on)] g: u16 },
}
trait Tr<'a, X, const C: bool> {
    fn m<'b, Y>(&self);
}
fn f<'a, 'b: 'a, T, const K: u8>() {}
impl<'q, Q: Copy> Tr<'q, Q, true> for Tuple<'q, Q, 1> {
    fn m<'b, Y>(&self) {}
}
"#;

fn of_view(node: view::Node<'_>) -> (SyntaxKind, TextRange) {
    (node.kind(), node.text_range())
}

fn of_handle(node: &syntax::SyntaxNode) -> (SyntaxKind, TextRange) {
    (node.kind(), node.text_range())
}

fn either_view<'t, L: view::AstNode<'t>, R: view::AstNode<'t>>(
    it: &Either<L, R>,
) -> (SyntaxKind, TextRange) {
    of_view(view::AstNode::syntax(it))
}

/// Every item, generic owner and variant of the fixture's crate, and every import.
struct Defs {
    sources: Vec<(SyntaxKind, TextRange, SyntaxKind, TextRange)>,
    generics: Vec<GenericDefId>,
    variants: Vec<VariantId>,
    uses: Vec<UseId>,
}

fn defs(db: &TestDB) -> Defs {
    fn source<L: HasSource>(db: &Db, loc: L) -> (SyntaxKind, TextRange, SyntaxKind, TextRange)
    where
        L::Value: Typed,
    {
        let handle = loc.source(db);
        let view = loc.source_view(db);
        assert_eq!(handle.file_id, view.file_id);
        let (vk, vr) = of_view(view::AstNode::syntax(&view.value));
        let (hk, hr) = of_handle(handle.value.syntax());
        (vk, vr, hk, hr)
    }

    let mut defs =
        Defs { sources: Vec::new(), generics: Vec::new(), variants: Vec::new(), uses: Vec::new() };
    let def_map = crate_def_map(db, db.fetch_test_crate());
    for (_, module) in def_map.modules(db) {
        let scope = module.scope(db);
        defs.uses.extend(scope.use_decls());
        for it in scope.use_decls() {
            defs.sources.push(source(db, it.lookup(db)));
        }
        for it in scope.impls() {
            defs.sources.push(source(db, it.lookup(db)));
            defs.generics.push(it.into());
        }
        for decl in scope.declarations() {
            match decl {
                ModuleDefId::FunctionId(it) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                }
                ModuleDefId::AdtId(AdtId::StructId(it)) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                    defs.variants.push(VariantId::StructId(it));
                }
                ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                    defs.variants.push(VariantId::UnionId(it));
                }
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                    for &(variant, _) in EnumVariants::of(db, it).variants.values() {
                        let variant: EnumVariantId = variant;
                        defs.sources.push(source(db, variant.lookup(db)));
                        defs.variants.push(VariantId::EnumVariantId(variant));
                    }
                }
                ModuleDefId::TraitId(it) => {
                    defs.sources.push(source(db, it.lookup(db)));
                    defs.generics.push(it.into());
                }
                _ => {}
            }
        }
    }
    defs
}

#[test]
fn source_view_is_source() {
    let db = fixture::db(FIXTURE).into_db();
    let defs = defs(&db);
    assert!(defs.sources.len() >= 14, "the fixture's items: {}", defs.sources.len());
    for (vk, vr, hk, hr) in defs.sources {
        assert_eq!((vk, vr), (hk, hr));
    }
}

#[test]
fn child_source_view_is_child_source() {
    let db = fixture::db(FIXTURE).into_db();
    let defs = defs(&db);
    let mut seen = 0;
    for variant in defs.variants {
        let map = variant.child_source(&db);
        // One past the last: `None` where the map has no entry.
        for raw in 0..=map.value.iter().count() as u32 {
            let id = LocalFieldId::from_raw(RawIdx::from(raw));
            let view = variant.child_source_view(&db, id);
            assert_eq!(view.file_id, map.file_id);
            let handle = map.value.get(id).map(|it| {
                it.as_ref().either(|it| of_handle(it.syntax()), |it| of_handle(it.syntax()))
            });
            assert_eq!(view.value.as_ref().map(either_view), handle, "{variant:?} {raw}");
            seen += usize::from(handle.is_some());
        }
    }
    for def in defs.generics {
        let map = HasChildSource::<LocalTypeOrConstParamId>::child_source(&def, &db);
        for raw in 0..=map.value.iter().count() as u32 {
            let id = LocalTypeOrConstParamId::from_raw(RawIdx::from(raw));
            let view = def.child_source_view(&db, id);
            assert_eq!(view.file_id, map.file_id);
            let handle = map.value.get(id).map(|it| {
                it.as_ref().either(|it| of_handle(it.syntax()), |it| of_handle(it.syntax()))
            });
            assert_eq!(view.value.as_ref().map(either_view), handle, "{def:?} {raw}");
            seen += usize::from(handle.is_some());
        }
        let map = HasChildSource::<LocalLifetimeParamId>::child_source(&def, &db);
        for raw in 0..=map.value.iter().count() as u32 {
            let id = LocalLifetimeParamId::from_raw(RawIdx::from(raw));
            let view = def.child_source_view(&db, id);
            assert_eq!(view.file_id, map.file_id);
            let handle = map.value.get(id).map(|it| of_handle(it.syntax()));
            let view = view.value.map(|it| of_view(view::AstNode::syntax(&it)));
            assert_eq!(view, handle, "{def:?} {raw}");
            seen += usize::from(handle.is_some());
        }
    }
    assert!(seen >= 30, "the fixture's children: {seen}");
}

#[test]
fn use_tree_to_view_is_child_source() {
    let db = fixture::db(FIXTURE).into_db();
    let mut seen = 0;
    for use_ in defs(&db).uses {
        let id = use_.lookup(&db).id;
        let map = use_.child_source(&db);
        let trees = use_trees_view(&db, id);
        assert_eq!(trees.len(), map.value.iter().count());
        let views = use_tree_sources_view(&db, use_);
        assert_eq!(views.file_id, map.file_id);
        assert_eq!(views.value.iter().count(), map.value.iter().count());
        for (idx, tree) in trees.iter() {
            let index: Idx<ast::UseTree> = Idx::from_raw(idx.into_raw());
            let in_map = map.value.get(index).map(|it| of_handle(it.syntax()));
            assert_eq!(Some(of_view(view::AstNode::syntax(tree))), in_map);
            let view = use_tree_to_view(&db, id, index);
            assert_eq!(Some(of_view(view::AstNode::syntax(&view))), in_map);
            let in_views = views.value.get(index).map(|&it| of_view(view::AstNode::syntax(&it)));
            assert_eq!(in_views, in_map);
            seen += 1;
        }
    }
    assert!(seen >= 8, "the fixture's use trees: {seen}");
}
