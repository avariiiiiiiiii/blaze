//! The path of an item as `rustc-demangle` prints the v0 symbol rustc gives it, and what a
//! function or static in a body is found by among those symbols: the bridge from what blaze
//! compiles to code rustc built. `blaze-engine`'s resolver and `blaze-wasm`'s are both over
//! [`Paths`], each with its own table of rustc's symbols and its own way of spelling a crate.
//!
//! no-rustc: step G of docs/no-rustc.md, every crate compiled and linked by blaze
//! ([#87](https://github.com/indexable-inc/blaze/issues/87)), deletes this crate. Nothing of
//! rustc's is linked then, so there is no symbol of rustc's to find. What else spells names
//! with it by then (`blaze-engine`'s `blaze.<crate>.<path>` for the functions and statics of a
//! crate rustc built nothing of) takes a spelling of its own in that change. Nothing new is
//! to use it.

#![forbid(unsafe_code)]

mod count;

#[cfg(test)]
mod tests;

pub use count::Places;

use std::borrow::Cow;

use base_db::Db;
use hir::{AsAssocItem, AssocItemContainer, Crate, Module, ModuleDef};
use hir_def::{
    AdtId, FunctionId, ImplId, Lookup, ModuleDefId, StaticId, expr_store::Body, hir::ExprId,
};
use syntax::{
    SyntaxKind, match_ast,
    view::{self as ast, AstNode as _, HasName as _},
};

use count::{Counted, Counts, closure_number, place_counted, place_in_definition};

/// Where a static is among rustc's symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticPath {
    /// One of a module: the symbol of its path.
    Item(String),
    /// One in a body, which a table finds by the named segments of its path (`path`), the
    /// numbers of the closures around it, outermost first (`closures`), its place among the
    /// statics of its name in what it is written in (`place`), and the places of the
    /// constants and statics it is written in among those of their names, outermost first
    /// (`around`). A table tries the closures first and, failing that, the places alone.
    InBody { path: String, closures: Vec<u32>, place: u64, around: Vec<u64> },
}

/// The paths rustc's symbols print as, over what a resolver says of the crates.
pub trait Paths<'db> {
    fn db(&self) -> &'db Db;

    /// How the paths of symbols spell the crate: its name, or `name[id]` where rustc's symbols
    /// tell apart two crates of the name.
    fn crate_path(&self, krate: Crate) -> Option<Cow<'_, str>>;

    /// What reads the syntax.
    fn sema(&self) -> hir::view::Semantics<'db>;

    /// The places of the closures and statics of the definitions counted so far, kept for
    /// every thread of a build.
    fn places(&self) -> &Places;

    /// The function's path as `rustc-demangle` prints its v0 symbol.
    fn function_path(&self, func: FunctionId) -> Option<String> {
        let db = self.db();
        let ModuleDef::Function(func) = ModuleDef::from(ModuleDefId::FunctionId(func)) else {
            return None;
        };
        let name = func.name(db);
        let Some(assoc) = func.as_assoc_item(db) else {
            return self.item_path(func.module(db), name.as_str());
        };
        // A method a trait provides is named after the trait, as what its body declares is.
        let imp = match assoc.container(db) {
            AssocItemContainer::Impl(imp) => imp,
            AssocItemContainer::Trait(trait_) => {
                let trait_ = self.item_path(trait_.module(db), trait_.name(db).as_str());
                return Some(format!("{}::{}", trait_?, name.as_str()));
            }
        };
        let self_ty = self.type_path(&imp.self_ty(db))?;
        match imp.trait_(db) {
            Some(trait_) => {
                let mut path = self.item_path(trait_.module(db), trait_.name(db).as_str())?;
                // `impl PartialEq<str> for String` and `impl PartialEq<&str> for String` are
                // told apart by the trait's arguments alone.
                let trait_ref = imp.trait_ref(db)?;
                let params = hir::GenericDef::from(trait_).type_or_const_params(db);
                let mut args = Vec::new();
                for (idx, param) in params.iter().enumerate().skip(1) {
                    let arg = trait_ref.get_type_argument(idx)?;
                    let default = param.as_type_param(db).and_then(|it| it.default(db));
                    // FIXME: a default that names a parameter is understood only as `= Self`.
                    let is_default = default.as_ref().is_some_and(|default| {
                        if default.as_type_param(db).is_some() {
                            return hir::attach_db(db, || arg == trait_ref.self_ty());
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
                Some(format!("<{self_ty} as {path}>::{}", name.as_str()))
            }
            None => Some(format!("<{self_ty}>::{}", name.as_str())),
        }
    }

    /// The path of the item `name` declared in `module`.
    fn item_path(&self, module: Module, name: &str) -> Option<String> {
        let mut path = self.scope_path(module)?;
        path.push_str("::");
        path.push_str(name);
        Some(path)
    }

    /// The path of an item, e.g. `core::any::TypeId`.
    fn item_path_of(&self, item: ModuleDefId) -> Option<String> {
        let db = self.db();
        let item = ModuleDef::from(item);
        self.item_path(item.module(db)?, item.name(db)?.as_str())
    }

    /// The function written at `func`, which holds the block that `scope` is the module of.
    ///
    /// `Semantics::to_def` answers the same, but starts from every crate the file is part of,
    /// which for a package with tests and examples analyses all its dev-dependencies.
    fn function_at<'t>(
        &self,
        sema: &hir::view::Semantics<'t>,
        scope: Module,
        func: ast::Fn<'t>,
    ) -> Option<hir::Function> {
        let db = self.db();
        let file = sema.hir_file_for(func.syntax());
        let written =
            hir::InFile::new(file, file.ast_id_map(db).ast_id_of_view::<syntax::ast::Fn>(func));
        let is_it = |it: &hir::Function| {
            FunctionId::try_from(*it).is_ok_and(|it| it.lookup(db).id == written)
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
                .or_else(|| it.impl_defs(db).into_iter().find_map(|imp| among(imp.items(db))));
            if found.is_some() || it.nearest_non_block_module(db) == it {
                return found;
            }
            module = it.parent(db);
        }
        None
    }

    /// The path of what holds a module's items: the module, or for the items of a block the
    /// function, closure, constant or static the block is in, which only the syntax knows.
    fn scope_path(&self, module: Module) -> Option<String> {
        let db = self.db();
        if module.nearest_non_block_module(db) == module {
            let mut path = self.crate_path(module.krate(db))?.into_owned();
            for segment in module.path_segments(db) {
                path.push_str("::");
                path.push_str(segment.as_str());
            }
            return Some(path);
        }
        let sema = self.sema();
        // A block module's definition is its block.
        let source = module.definition_source_range(db);
        let block = sema
            .parse_or_expand(source.file_id)
            .descendant_at_range(source.value, SyntaxKind::BLOCK_EXPR)?;
        let mut inner = Vec::new();
        let mut owner = None;
        for node in sema.ancestors_with_macros(block) {
            if is_closure(node) {
                inner.push(format!("{{closure#{}}}", closure_number(&sema, self.places(), node)?));
                continue;
            }
            if let Some(func) = ast::Fn::cast(node) {
                // The body of an `async fn` is a closure to rustc.
                if func.async_token().is_some() {
                    inner.push("{closure#0}".to_owned());
                }
                let func = self.function_at(&sema, module, func)?;
                owner = Some(self.function_path(func.try_into().ok()?)?);
                break;
            }
            match_ast! {
                match node {
                    ast::Const(it) => inner.push(it.name().map_or("_".to_owned(), |it| it.text().to_owned())),
                    ast::Static(it) => inner.push(it.name()?.text().to_owned()),
                    // Anonymous constants have numbers we don't know.
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
                self.item_path(module.nearest_non_block_module(db), &outer)?
            }
        };
        for segment in inner.iter().rev() {
            path.push_str("::");
            path.push_str(segment);
        }
        Some(path)
    }

    /// A type the way rustc writes it in a symbol.
    fn type_path(&self, ty: &hir::Type<'_>) -> Option<String> {
        let db = self.db();
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
                ty.tuple_fields(db).iter().map(|it| self.type_path(it)).collect();
            return Some(match &*fields? {
                [only] => format!("({only},)"),
                fields => format!("({})", fields.join(", ")),
            });
        }
        // A parameter of the `impl` an item is in, where the item is not generic itself: a
        // static, or what a closure or constant in a generic function holds.
        if ty.as_type_param(db).is_some() {
            return Some("_".to_owned());
        }
        let adt = ty.as_adt()?;
        let path = self.item_path(adt.module(db), adt.name(db).as_str())?;
        let generics = hir::GenericDef::from(adt);
        let params = generics.type_or_const_params(db);
        if params.is_empty() {
            return Some(path);
        }
        // rustc leaves a defaulted const argument out of the name, so `Bump` may be `Bump<1>`.
        // FIXME: name those too.
        if !params.iter().all(|it| it.as_type_param(db).is_some()) {
            return None;
        }
        // Lifetimes are erased, and rustc leaves erased ones out of the name.
        let args: Option<Vec<_>> = ty.type_arguments().map(|arg| self.type_path(&arg)).collect();
        Some(format!("{path}<{}>", args?.join(", ")))
    }

    /// The path of the closure at `expr` in the body of `owner`, which is not generic. None
    /// for one in an `async fn`, whose body is a closure of its own to rustc.
    fn closure_path(&self, owner: FunctionId, expr: ExprId) -> Option<String> {
        let db = self.db();
        let source_map = &Body::with_source_map(db, owner.into()).1;
        let written = source_map.expr_syntax(expr).ok()?;
        let sema = self.sema();
        let node = sema.to_view(written).syntax();
        let mut inner = Vec::new();
        for node in sema.ancestors_with_macros(node) {
            if is_closure(node) {
                inner.push(format!("{{closure#{}}}", closure_number(&sema, self.places(), node)?));
                continue;
            }
            if let Some(func) = ast::Fn::cast(node) {
                if func.async_token().is_some() {
                    return None;
                }
                break;
            }
        }
        let mut path = self.function_path(owner)?;
        for segment in inner.iter().rev() {
            path.push_str("::");
            path.push_str(segment);
        }
        Some(path)
    }

    /// Where the static is among rustc's symbols.
    fn static_path(&self, id: StaticId) -> Option<StaticPath> {
        let db = self.db();
        let ModuleDef::Static(it) = ModuleDef::from(ModuleDefId::StaticId(id)) else {
            return None;
        };
        let module = it.module(db);
        let name = it.name(db);
        if module.nearest_non_block_module(db) == module {
            return Some(StaticPath::Item(self.item_path(module, name.as_str())?));
        }
        // A static in a body is named after the items around it, which only the syntax knows.
        let sema = self.sema();
        let source = sema.source(it)?;
        let mut names = vec![name.as_str().to_owned()];
        let mut closures = Vec::new();
        let mut around: Vec<u64> = Vec::new();
        let mut owner = None;
        for node in sema.ancestors_with_macros(source.value.syntax()).skip(1) {
            if is_closure(node) {
                closures.push(closure_number(&sema, self.places(), node)?);
                continue;
            }
            if let Some(func) = ast::Fn::cast(node) {
                if func.async_token().is_some() {
                    closures.push(0);
                }
                let func = self.function_at(&sema, module, func)?;
                owner = Some(self.function_path(func.try_into().ok()?)?);
                break;
            }
            if let Some(name) = name_in_path(node) {
                around.push(place_in_definition(&sema, node, Counts::InPath(name))?.into());
                names.push(name.to_owned());
            }
        }
        let owner = match owner {
            Some(owner) => owner,
            None => {
                around.pop();
                let outer = names.pop()?;
                self.item_path(module.nearest_non_block_module(db), &outer)?
            }
        };
        names.push(owner);
        names.reverse();
        closures.reverse();
        around.reverse();
        let path = names.join("::");
        // Only a path through a closure or an anonymous constant has segments left out of it.
        let named = Counts::Statics(name.as_str());
        let counted = Counted::Statics(name.as_str().to_owned());
        let place: u64 =
            place_counted(&sema, self.places(), counted, source.value.syntax(), named)?.into();
        Some(StaticPath::InBody { path, closures, place, around })
    }

    /// The function a type is declared in the body of, when nothing else is around it there
    /// and no other type of the body has its name.
    fn declaring_function(&self, adt: AdtId) -> Option<FunctionId> {
        let db = self.db();
        let adt = hir::Adt::from(adt);
        let sema = self.sema();
        let source = sema.source(adt)?;
        let mut around = sema.ancestors_with_macros(source.value.syntax()).skip(1);
        let func = ast::Fn::cast(around.find(|it| is_definition(*it))?)?;
        // The body of an `async fn` is a closure to rustc.
        if func.async_token().is_some() {
            return None;
        }
        let name = adt.name(db);
        let named_so = func.syntax().descendants().filter_map(ast::Name::cast).filter(|it| {
            it.text() == name.as_str()
                && it.syntax().parent().is_some_and(|it| {
                    ast::Adt::can_cast(it.kind())
                        || ast::Trait::can_cast(it.kind())
                        || ast::TypeAlias::can_cast(it.kind())
                        || ast::Module::can_cast(it.kind())
                })
        });
        if named_so.count() != 1 {
            return None;
        }
        self.function_at(&sema, adt.module(db), func)?.try_into().ok()
    }
}

/// How many `impl`s of its module rustc numbers before this one.
pub fn impl_place(db: &Db, id: ImplId) -> Option<u32> {
    use hir::view::HasSourceView as _;
    let it = hir::Impl::from(id);
    let source = it.source_view(db)?;
    // FIXME: where rustc puts the `impl`s a macro makes among the ones written out.
    if source.file_id.is_macro() {
        return None;
    }
    let start = |it: &hir::InFile<ast::Impl<'_>>| it.value.syntax().text_range().start();
    let mut earlier = 0;
    for other in it.module(db).impl_defs(db) {
        let other = other.source_view(db)?;
        // rustc numbers what a module has written out before what its macros and derives
        // make, which it only has once they are expanded.
        if other.file_id.is_macro() {
            continue;
        }
        if other.file_id != source.file_id {
            return None;
        }
        earlier += u32::from(start(&other) < start(&source));
    }
    Some(earlier)
}

/// The segment a constant or static that items are written in adds to their path, as rustc
/// prints it: its name, or `_` for an unnamed constant. objc2's `declare_class!` keeps a
/// class's ivar offsets in statics in a `const _: () = { .. }`, which rustc exports as
/// `<module>::_::__OBJC2_IVAR_OFFSET`.
fn name_in_path(node: ast::Node<'_>) -> Option<&str> {
    match_ast! {
        match node {
            ast::Const(it) => it
                .name()
                .map(|it| it.text())
                .or_else(|| it.underscore_token().map(|_| "_")),
            ast::Static(it) => it.name().map(|it| it.text()),
            _ => None,
        }
    }
}

/// Whether rustc numbers the node as a closure: a closure, or an `async` block.
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
