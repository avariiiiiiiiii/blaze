use std::borrow::Cow;

use base_db::Db;
use hegel::TestCase;
use hegel::generators as gs;
use hir::Crate;
use hir_def::FunctionId;
use syntax::view::{self as ast, AstNode as _, HasName as _};
use test_fixture::WithFixture;

use crate::{Paths, Places, StaticPath, count::Counts};

/// Paths as rustc spells those of a crate that no other crate of the program shares a name
/// with: by its name.
struct Named<'db> {
    db: &'db Db,
    places: Places,
}

impl<'db> Paths<'db> for Named<'db> {
    fn db(&self) -> &'db Db {
        self.db
    }

    fn crate_path(&self, krate: Crate) -> Option<Cow<'_, str>> {
        Some(Cow::Owned(krate.display_name(self.db)?.crate_name().to_string()))
    }

    fn sema(&self) -> hir::view::Semantics<'db> {
        hir::view::Semantics::new(self.db)
    }

    fn places(&self) -> &Places {
        &self.places
    }
}

/// `blaze-wasm`'s copy of the resolver as it was before the two were made one, with what it
/// read of rustc's symbols for a crate's path taken to be the crate's name: the spec the
/// shared paths are checked against.
mod reference {
    use base_db::Db;
    use hir::{AsAssocItem, AssocItemContainer, Module, ModuleDef};
    use hir_def::{FunctionId, ModuleDefId};
    use syntax::{
        SyntaxKind, match_ast,
        view::{self as ast, AstNode as _, HasName as _},
    };

    pub(super) struct Resolver<'a> {
        pub(super) db: &'a Db,
    }

    impl Resolver<'_> {
        pub(super) fn path(&self, func: FunctionId) -> Option<String> {
            let ModuleDef::Function(func) = ModuleDef::from(ModuleDefId::FunctionId(func)) else {
                return None;
            };
            let name = func.name(self.db);
            let Some(assoc) = func.as_assoc_item(self.db) else {
                return self.item_path(func.module(self.db), name.as_str());
            };
            let AssocItemContainer::Impl(imp) = assoc.container(self.db) else { return None };
            let self_ty = self.type_path(&imp.self_ty(self.db))?;
            match imp.trait_(self.db) {
                Some(trait_) => {
                    let mut path =
                        self.item_path(trait_.module(self.db), trait_.name(self.db).as_str())?;
                    let trait_ref = imp.trait_ref(self.db)?;
                    let params = hir::GenericDef::from(trait_).type_or_const_params(self.db);
                    let mut args = Vec::new();
                    for (idx, param) in params.iter().enumerate().skip(1) {
                        let arg = trait_ref.get_type_argument(idx)?;
                        let default =
                            param.as_type_param(self.db).and_then(|it| it.default(self.db));
                        let is_default = default.as_ref().is_some_and(|default| {
                            if default.as_type_param(self.db).is_some() {
                                return hir::attach_db(self.db, || arg == trait_ref.self_ty());
                            }
                            let default = self.type_path(default);
                            default.is_some() && default == self.type_path(&arg)
                        });
                        if idx + 1 == params.len() && is_default {
                            break;
                        }
                        args.push(self.type_path(&arg)?);
                    }
                    if !args.is_empty() {
                        path = format!("{path}<{}>", args.join(", "));
                    }
                    let trait_ = path;
                    Some(format!("<{self_ty} as {trait_}>::{}", name.as_str()))
                }
                None => Some(format!("<{self_ty}>::{}", name.as_str())),
            }
        }

        fn item_path(&self, module: Module, name: &str) -> Option<String> {
            let mut path = self.scope_path(module)?;
            path.push_str("::");
            path.push_str(name);
            Some(path)
        }

        fn function_at<'db>(
            &self,
            sema: &hir::view::Semantics<'db>,
            scope: Module,
            func: ast::Fn<'db>,
        ) -> Option<hir::Function> {
            use hir::view::HasSourceView as _;
            let db = self.db;
            let file = sema.hir_file_for(func.syntax());
            let range = func.syntax().text_range();
            let is_it = |it: &hir::Function| {
                it.source_view(db).is_some_and(|source| {
                    source.file_id == file && source.value.syntax().text_range() == range
                })
            };
            let among = |items: Vec<hir::AssocItem>| {
                items.into_iter().filter_map(hir::AssocItem::as_function).find(is_it)
            };
            let mut module = Some(scope);
            while let Some(it) = module {
                let found = it
                    .declarations(db)
                    .into_iter()
                    .find_map(|def| match def {
                        ModuleDef::Function(func) => Some(func).filter(is_it),
                        ModuleDef::Trait(trait_) => among(trait_.items(db)),
                        _ => None,
                    })
                    .or_else(|| {
                        it.impl_defs(db).into_iter().find_map(|imp| among(imp.items(db)))
                    });
                if found.is_some() || it.nearest_non_block_module(db) == it {
                    return found;
                }
                module = it.parent(db);
            }
            None
        }

        fn scope_path(&self, module: Module) -> Option<String> {
            if module.nearest_non_block_module(self.db) == module {
                let krate = module.krate(self.db);
                let mut path = krate.display_name(self.db)?.crate_name().to_string();
                for segment in module.path_segments(self.db) {
                    path.push_str("::");
                    path.push_str(segment.as_str());
                }
                return Some(path);
            }
            let sema = hir::view::Semantics::new(self.db);
            let source = module.definition_source_range(self.db);
            let block = sema
                .parse_or_expand(source.file_id)
                .descendant_at_range(source.value, SyntaxKind::BLOCK_EXPR)?;
            let mut inner = Vec::new();
            let mut owner = None;
            for node in sema.ancestors_with_macros(block) {
                if is_closure(node) {
                    inner.push(format!("{{closure#{}}}", closure_number(&sema, node)?));
                    continue;
                }
                if let Some(func) = ast::Fn::cast(node) {
                    if func.async_token().is_some() {
                        inner.push("{closure#0}".to_owned());
                    }
                    let func = self.function_at(&sema, module, func)?;
                    owner = Some(self.path(func.try_into().ok()?)?);
                    break;
                }
                match_ast! {
                    match node {
                        ast::Const(it) => inner.push(it.name().map_or("_".to_owned(), |it| it.text().to_string())),
                        ast::Static(it) => inner.push(it.name()?.text().to_string()),
                        ast::BlockExpr(it) => if it.const_token().is_some() { return None },
                        ast::ConstArg(_) => return None,
                        _ => {},
                    }
                }
            }
            let mut path = match owner {
                Some(owner) => owner,
                None => {
                    let outer = inner.pop()?;
                    self.item_path(module.nearest_non_block_module(self.db), &outer)?
                }
            };
            for segment in inner.iter().rev() {
                path.push_str("::");
                path.push_str(segment);
            }
            Some(path)
        }

        pub(super) fn type_path(&self, ty: &hir::Type<'_>) -> Option<String> {
            if let Some((inner, mutability)) = ty.as_reference() {
                let inner = self.type_path(&inner)?;
                return Some(match mutability {
                    hir::Mutability::Shared => format!("&{inner}"),
                    hir::Mutability::Mut => format!("&mut {inner}"),
                });
            }
            if let Some(elem) = ty.as_slice() {
                return Some(format!("[{}]", self.type_path(&elem)?));
            }
            if let Some(builtin) = ty.as_builtin() {
                return Some(builtin.name().as_str().to_owned());
            }
            if ty.is_tuple() {
                let fields: Option<Vec<_>> =
                    ty.tuple_fields(self.db).iter().map(|it| self.type_path(it)).collect();
                return Some(match &*fields? {
                    [only] => format!("({only},)"),
                    fields => format!("({})", fields.join(", ")),
                });
            }
            if ty.as_type_param(self.db).is_some() {
                return Some("_".to_owned());
            }
            let adt = ty.as_adt()?;
            let path = self.item_path(adt.module(self.db), adt.name(self.db).as_str())?;
            let generics = hir::GenericDef::from(adt);
            let params = generics.type_or_const_params(self.db);
            if params.is_empty() {
                return Some(path);
            }
            if !params.iter().all(|it| it.as_type_param(self.db).is_some()) {
                return None;
            }
            let args: Option<Vec<_>> =
                ty.type_arguments().map(|arg| self.type_path(&arg)).collect();
            Some(format!("{path}<{}>", args?.join(", ")))
        }
    }

    fn is_closure(node: ast::Node<'_>) -> bool {
        ast::ClosureExpr::can_cast(node.kind())
            || ast::BlockExpr::cast(node).is_some_and(|it| it.async_token().is_some())
    }

    fn is_definition(node: ast::Node<'_>) -> bool {
        !ast::MacroCall::can_cast(node.kind())
            && (ast::Item::can_cast(node.kind())
                || ast::AssocItem::can_cast(node.kind())
                || ast::ConstArg::can_cast(node.kind())
                || is_closure(node)
                || ast::BlockExpr::cast(node).is_some_and(|it| it.const_token().is_some()))
    }

    fn place_in_definition<'db>(
        sema: &hir::view::Semantics<'db>,
        target: ast::Node<'db>,
        counts: &impl Fn(ast::Node<'db>) -> bool,
    ) -> Option<u32> {
        let mut around = sema.ancestors_with_macros(target).skip(1);
        let mut root = None;
        let owner = around.find(|&node| {
            root = Some(node);
            is_definition(node)
        });
        let mut earlier = 0;
        Place { sema, target, counts, earlier: &mut earlier }
            .find_in(owner.or(root)?)
            .then_some(earlier)
    }

    struct Place<'a, 'db, F: Fn(ast::Node<'db>) -> bool> {
        sema: &'a hir::view::Semantics<'db>,
        target: ast::Node<'db>,
        counts: &'a F,
        earlier: &'a mut u32,
    }

    impl<'db, F: Fn(ast::Node<'db>) -> bool> Place<'_, 'db, F> {
        fn find_in(&mut self, node: ast::Node<'db>) -> bool {
            let mut calls = Vec::new();
            if self.find_written_in(node, &mut calls) {
                return true;
            }
            calls.iter().any(|&call| {
                self.sema
                    .expand_macro_call(call)
                    .is_some_and(|expansion| self.find_in(expansion.value))
            })
        }

        fn find_written_in(
            &mut self,
            node: ast::Node<'db>,
            calls: &mut Vec<ast::MacroCall<'db>>,
        ) -> bool {
            for child in node.children() {
                if let Some(call) = ast::MacroCall::cast(child) {
                    calls.push(call);
                    continue;
                }
                if child == self.target {
                    return true;
                }
                if (self.counts)(child) {
                    *self.earlier += 1;
                }
                if !is_definition(child) && self.find_written_in(child, calls) {
                    return true;
                }
            }
            false
        }
    }

    fn closure_number<'db>(
        sema: &hir::view::Semantics<'db>,
        closure: ast::Node<'db>,
    ) -> Option<u32> {
        place_in_definition(sema, closure, &is_closure)
    }
}

/// Every function written in the file, by its name, with the shared path of each and the
/// reference's.
fn function_paths(text: &str) -> Vec<(String, Option<String>, Option<String>)> {
    let (db, file) = Db::with_single_file(text);
    hir::attach_db(&db, || {
        let shared = Named { db: &db, places: Places::default() };
        let reference = reference::Resolver { db: &db };
        let sema = hir::view::Semantics::new(&db);
        let tree = sema.parse(file);
        let mut paths = Vec::new();
        for written in tree.syntax().descendants().filter_map(ast::Fn::cast) {
            let name = written.name().map_or_else(String::new, |it| it.text().to_owned());
            let Some(func) = sema.to_def(written) else { continue };
            let Ok(func) = FunctionId::try_from(func) else { continue };
            paths.push((name, shared.function_path(func), reference.path(func)));
        }
        paths
    })
}

/// The self type of every `impl` written in the file, as the shared paths and the reference
/// write it.
fn impl_types(text: &str) -> Vec<(Option<String>, Option<String>)> {
    let (db, file) = Db::with_single_file(text);
    hir::attach_db(&db, || {
        let shared = Named { db: &db, places: Places::default() };
        let reference = reference::Resolver { db: &db };
        let sema = hir::view::Semantics::new(&db);
        let tree = sema.parse(file);
        let impls = tree.syntax().descendants().filter_map(ast::Impl::cast);
        let impls = impls.filter_map(|it| sema.to_def(it));
        impls
            .map(|imp| {
                let ty = imp.self_ty(&db);
                (shared.type_path(&ty), reference.type_path(&ty))
            })
            .collect()
    })
}

const CORPUS: &str = r#"
//- /main.rs crate:app edition:2021
pub mod outer {
    pub mod inner { pub fn deep() {} }
    pub struct S;
    impl S { pub fn method(&self) {} }
}
pub struct G<T>(T);
impl G<u8> { fn of_u8() {} }
impl<'a> G<&'a [u16]> { fn of_slice() {} }
impl G<(u8,)> { fn of_one() {} }
impl G<(u8, &'static mut i32)> { fn of_pair() {} }
pub trait Tr<Rhs = Self> { fn call(&self); fn provided(&self) {} }
impl Tr for outer::S { fn call(&self) {} }
impl Tr<u8> for G<u8> { fn call(&self) {} }
fn host() {
    fn in_body() {}
    let _ = || { fn in_closure() {} };
    let _ = || { fn in_second() {} };
}
const C: () = { fn in_const() {} };
const _: () = { fn in_unnamed() {} };
static V: u8 = { fn in_static() -> u8 { 0 } in_static() };
async fn later() { fn in_async() {} }
"#;

#[test]
fn a_function_is_named_as_rustc_demangle_prints_its_symbol() {
    let paths = function_paths(CORPUS);
    let shared: Vec<(&str, Option<&str>)> =
        paths.iter().map(|(name, shared, _)| (&**name, shared.as_deref())).collect();
    assert_eq!(
        shared,
        [
            ("deep", Some("app::outer::inner::deep")),
            ("method", Some("<app::outer::S>::method")),
            ("of_u8", Some("<app::G<u8>>::of_u8")),
            ("of_slice", Some("<app::G<&[u16]>>::of_slice")),
            ("of_one", Some("<app::G<(u8,)>>::of_one")),
            ("of_pair", Some("<app::G<(u8, &mut i32)>>::of_pair")),
            ("call", Some("app::Tr::call")),
            ("provided", Some("app::Tr::provided")),
            ("call", Some("<app::outer::S as app::Tr>::call")),
            ("call", Some("<app::G<u8> as app::Tr<u8>>::call")),
            ("host", Some("app::host")),
            ("in_body", Some("app::host::in_body")),
            ("in_closure", Some("app::host::{closure#0}::in_closure")),
            ("in_second", Some("app::host::{closure#1}::in_second")),
            ("in_const", Some("app::C::in_const")),
            ("in_unnamed", Some("app::_::in_unnamed")),
            ("in_static", Some("app::V::in_static")),
            ("later", Some("app::later")),
            ("in_async", Some("app::later::{closure#0}::in_async")),
        ]
    );
}

/// The old copy named nothing a trait provides or requires; everything else is named alike.
#[test]
fn the_shared_paths_are_the_old_copys_where_it_named_the_function() {
    for (name, shared, reference) in function_paths(CORPUS) {
        match reference {
            Some(reference) => assert_eq!(shared, Some(reference), "{name}"),
            None => assert!(matches!(&*name, "call" | "provided"), "{name}: {shared:?}"),
        }
    }
}

#[test]
fn a_type_is_written_as_rustc_writes_it_in_a_symbol() {
    let types = impl_types(CORPUS);
    let shared: Vec<Option<&str>> = types.iter().map(|(it, _)| it.as_deref()).collect();
    assert_eq!(
        shared,
        [
            Some("app::outer::S"),
            Some("app::G<u8>"),
            Some("app::G<&[u16]>"),
            Some("app::G<(u8,)>"),
            Some("app::G<(u8, &mut i32)>"),
            Some("app::outer::S"),
            Some("app::G<u8>"),
        ]
    );
    for (shared, reference) in types {
        assert_eq!(shared, reference);
    }
}

#[test]
fn a_static_in_a_body_is_found_by_the_items_and_closures_around_it() {
    let text = r#"
//- /main.rs crate:app edition:2021
static TOP: u8 = 0;
fn host() {
    let _ = || {};
    let _ = || { static IN_CLOSURE: u8 = 0; };
}
const C: () = { static A: u8 = 0; };
"#;
    let (db, file) = Db::with_single_file(text);
    hir::attach_db(&db, || {
        let shared = Named { db: &db, places: Places::default() };
        let sema = hir::view::Semantics::new(&db);
        let tree = sema.parse(file);
        let statics = tree.syntax().descendants().filter_map(ast::Static::cast);
        let found: Vec<StaticPath> = statics
            .filter_map(|it| sema.to_def(it))
            .filter_map(|it| shared.static_path(it.into()))
            .collect();
        assert_eq!(
            found,
            [
                StaticPath::Item("app::TOP".to_owned()),
                StaticPath::InBody {
                    path: "app::host::IN_CLOSURE".to_owned(),
                    closures: vec![1],
                    place: 0,
                    around: Vec::new(),
                },
                StaticPath::InBody {
                    path: "app::C::A".to_owned(),
                    closures: Vec::new(),
                    place: 0,
                    around: Vec::new(),
                },
            ]
        );
    });
}

/// Each kind of node a count counts holds of its nodes alone: closures and `async` blocks,
/// the statics of a name, and the constants and statics of a name in a path (`_` for an
/// unnamed constant).
#[test]
fn a_count_counts_the_nodes_of_its_kind() {
    let text = r#"
static A: u8 = 0;
static B: u8 = 0;
const A: u8 = 0;
const _: u8 = 0;
fn f() { let _ = || 0; let _ = async {}; let _ = {}; }
"#;
    let parse = syntax::SourceFile::parse(text, syntax::Edition::CURRENT);
    let held = |counts: Counts<'_>| {
        let nodes = parse.root().descendants().filter(|&it| counts.holds(it));
        nodes.map(|it| it.text()).collect::<Vec<_>>()
    };
    assert_eq!(held(Counts::Closures), ["|| 0", "async {}"]);
    assert_eq!(held(Counts::Statics("A")), ["static A: u8 = 0;"]);
    assert_eq!(held(Counts::InPath("A")), ["static A: u8 = 0;", "const A: u8 = 0;"]);
    assert_eq!(held(Counts::InPath("_")), ["const _: u8 = 0;"]);
    assert!(held(Counts::Statics("C")).is_empty());
}

/// What a generated program is made of. Where a node can't be written (a module in a body, a
/// closure among items) it is written as the nearest that can: a constant, a function.
#[derive(Debug, Clone)]
enum Node {
    Fn(Vec<Node>),
    Mod(Vec<Node>),
    Const(Vec<Node>),
    /// A struct and an `impl` of it with one method.
    Method(Vec<Node>),
    Closure(Vec<Node>),
}

/// A node of at most `depth` levels, children four at most: a leaf as often as not.
fn draw_node(tc: &TestCase, depth: u32) -> Node {
    if depth == 0 || tc.draw(gs::booleans()) {
        return tc.draw(gs::sampled_from(vec![
            Node::Fn(Vec::new()),
            Node::Method(Vec::new()),
            Node::Closure(Vec::new()),
        ]));
    }
    let len = tc.draw(gs::integers::<usize>().max_value(3));
    let children = (0..len).map(|_| draw_node(tc, depth - 1)).collect();
    match tc.draw(gs::integers::<u8>().max_value(4)) {
        0 => Node::Fn(children),
        1 => Node::Mod(children),
        2 => Node::Const(children),
        3 => Node::Method(children),
        _ => Node::Closure(children),
    }
}

fn draw_nodes(tc: &TestCase) -> Vec<Node> {
    let len = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
    (0..len).map(|_| draw_node(tc, 4)).collect()
}

/// Writes `nodes` as items (`in_body` false) or as the statements of a body, each named apart
/// by `next`.
fn write(nodes: &[Node], in_body: bool, next: &mut u32, out: &mut String) {
    for node in nodes {
        *next += 1;
        let n = *next;
        match node {
            Node::Closure(body) if in_body => {
                out.push_str("let _ = || {\n");
                write(body, true, next, out);
                out.push_str("};\n");
            }
            Node::Fn(body) | Node::Closure(body) => {
                out.push_str(&format!("fn f{n}() {{\n"));
                write(body, true, next, out);
                out.push_str("}\n");
            }
            Node::Mod(items) if !in_body => {
                out.push_str(&format!("mod m{n} {{\n"));
                write(items, false, next, out);
                out.push_str("}\n");
            }
            Node::Mod(body) | Node::Const(body) => {
                out.push_str(&format!("const C{n}: () = {{\n"));
                write(body, true, next, out);
                out.push_str("};\n");
            }
            Node::Method(body) => {
                out.push_str(&format!("struct T{n};\nimpl T{n} {{ fn f{n}() {{\n"));
                write(body, true, next, out);
                out.push_str("} }\n");
            }
        }
    }
}

/// Without macros or traits the old copy names every function as the shared paths do.
#[hegel::test(test_cases = 48)]
fn the_shared_paths_name_generated_programs_as_the_old_copy_did(tc: TestCase) {
    let program = draw_nodes(&tc);
    let mut text = "//- /main.rs crate:app edition:2021\n".to_owned();
    write(&program, false, &mut 0, &mut text);
    for (name, shared, reference) in function_paths(&text) {
        assert_eq!(&shared, &reference, "{} in\n{}", name, text);
        assert!(shared.is_some(), "{} in\n{}", name, text);
    }
    for (shared, reference) in impl_types(&text) {
        assert_eq!(shared, reference);
    }
}
