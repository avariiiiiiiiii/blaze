//! The numbers rustc gives the closures of a definition and the statics of a name in it: their
//! places, in the order rustc comes across them, which goes through what macros expand to
//! after what is written out.

use std::sync::{Arc, Mutex};

use hir::ModuleDef;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use crate::{is_closure, is_definition, name_in_path};

/// The place of a node among those of the definition it is written in that `counts` holds
/// of, in the order rustc comes across them, or none when that order cannot be told. This is
/// how rustc tells apart the closures of a function, and the statics of one name that a
/// macro used twice in it declares.
pub(crate) fn place_in_definition<'db>(
    sema: &hir::view::Semantics<'db>,
    target: ast::Node<'db>,
    counts: Counts<'_>,
) -> Option<u32> {
    let along: Vec<_> = sema.ancestors_with_macros(target).collect();
    let owner = owner_of(&along)?;
    let mut count = Count::new(sema, Some(target), along, counts, owner);
    count.all_in(owner)?;
    count.waiting_ones()?;
    count.earlier
}

/// The definition a node is written in, as [`place_in_definition`] counts in: the first of
/// `along` but the node that is one, or the last.
fn owner_of<'db>(along: &[ast::Node<'db>]) -> Option<ast::Node<'db>> {
    let mut root = None;
    let owner = along.iter().skip(1).copied().find(|&node| {
        root = Some(node);
        is_definition(node)
    });
    owner.or(root)
}

/// [`place_in_definition`] of what `counted` names, every node of the definition counted
/// once and kept in `places`: a definition with many closures, or a macro used many times
/// that declares a static each time (tracing's `META` and `__CALLSITE`), is gone through
/// once instead of once a node, which made a build in full of a large program wait on one
/// thread (#259). A node whose place the count does not find is counted alone as before.
pub(crate) fn place_counted<'db>(
    sema: &hir::view::Semantics<'db>,
    places: &Places,
    counted: Counted,
    target: ast::Node<'db>,
    counts: Counts<'_>,
) -> Option<u32> {
    let along: Vec<_> = sema.ancestors_with_macros(target).collect();
    let owner = owner_of(&along)?;
    if let Some(spot) = spot_in(&along, owner) {
        let of = PlacesOf {
            file: sema.hir_file_for(owner),
            range: owner.text_range(),
            kind: owner.kind(),
            counted,
        };
        let known = places.0.lock().unwrap_or_else(|e| e.into_inner()).get(&of).cloned();
        let all = known.unwrap_or_else(|| {
            let all = all_places(sema, owner, counts).map(Arc::new);
            places.0.lock().unwrap_or_else(|e| e.into_inner()).insert(of, all.clone());
            all
        });
        if let Some(&place) = all.as_ref().and_then(|all| all.get(&spot)) {
            return Some(place);
        }
    }
    place_in_definition(sema, target, counts)
}

/// The places of all the nodes of the definition `owner` that `counts` holds of, by where
/// each is ([`Spot`]), or none when the order rustc numbers them in is not known.
fn all_places<'db>(
    sema: &hir::view::Semantics<'db>,
    owner: ast::Node<'db>,
    counts: Counts<'_>,
) -> Option<FxHashMap<Spot, u32>> {
    let mut count = Count::new(sema, None, Vec::new(), counts, owner);
    count.places = Some(FxHashMap::default());
    count.all_in(owner)?;
    count.waiting_ones()?;
    let mut places = count.places?;
    // Two nodes in one spot are told apart by nothing a spot has.
    for spot in count.twice {
        places.remove(&spot);
    }
    Some(places)
}

/// Where `along`'s first node is in the definition `owner`, which `along` goes up to: the
/// macros it is expanded from and the expansion of each, or none when an attribute or a
/// derive made a node on the way, which the count does not go into.
fn spot_in(along: &[ast::Node<'_>], owner: ast::Node<'_>) -> Option<Spot> {
    let at_owner = along.iter().position(|&it| it == owner)?;
    let mut calls = Vec::new();
    for at in (1..at_owner).rev() {
        let node = along[at];
        if ast::MacroCall::can_cast(node.kind()) {
            let root = along[at - 1];
            if root.parent().is_some() {
                return None;
            }
            calls.push((node.text_range(), root.kind()));
        } else if node.parent().is_none() && !ast::MacroCall::can_cast(along[at + 1].kind()) {
            // An expansion that no macro call along is of.
            return None;
        }
    }
    if along.first()?.parent().is_none() {
        return None;
    }
    Some((calls, along.first()?.text_range()))
}

/// The places of the counted nodes of the definitions gone through ([`place_counted`]), for
/// every thread of a build.
#[derive(Clone, Default)]
pub struct Places(Arc<Mutex<FxHashMap<PlacesOf, Option<Arc<FxHashMap<Spot, u32>>>>>>);

/// A definition, and what is counted in it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct PlacesOf {
    file: hir::HirFileId,
    range: syntax::TextRange,
    kind: syntax::SyntaxKind,
    counted: Counted,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Counted {
    Closures,
    /// The statics of a name.
    Statics(String),
}

/// The nodes a count counts.
#[derive(Clone, Copy)]
pub(crate) enum Counts<'a> {
    Closures,
    /// The statics of a name.
    Statics(&'a str),
    /// The constants and statics [`name_in_path`] gives a name.
    InPath(&'a str),
}

impl Counts<'_> {
    pub(crate) fn holds(self, node: ast::Node<'_>) -> bool {
        match self {
            Counts::Closures => is_closure(node),
            Counts::Statics(name) => ast::Static::cast(node)
                .is_some_and(|it| it.name().is_some_and(|it| it.text() == name)),
            Counts::InPath(name) => name_in_path(node) == Some(name),
        }
    }
}

/// Where a node is in a definition: the macros it is expanded from, the outermost first,
/// each by where it is used and what it is expanded to (a macro used in a body is expanded
/// both to items and to statements, of the same text), and the node's range in the last.
type Spot = (Vec<(syntax::TextRange, syntax::SyntaxKind)>, syntax::TextRange);

/// The imports of the scope of a definition that rustc has not resolved when it first comes
/// to the macros used in the definition: those a macro expanded to. It resolves the imports
/// it has, then expands every macro it can, and only then resolves the imports that those
/// expanded to.
#[derive(Default)]
struct LateImports {
    names: FxHashSet<String>,
    glob: bool,
}

impl LateImports {
    fn around<'db>(sema: &hir::view::Semantics<'db>, owner: ast::Node<'db>) -> LateImports {
        let mut late = LateImports::default();
        let module_expanded = module_is_expanded(sema, owner);
        let expanded = |node: ast::Node<'db>| {
            module_expanded
                || sema.hir_file_for(node).is_macro()
                || sema
                    .ancestors_with_macros(node)
                    .filter_map(ast::Item::cast)
                    .any(|item| has_attribute_macro(sema, item))
        };
        if expanded(owner) {
            late.add_written_in(owner);
        }
        for scope in sema.ancestors_with_macros(owner) {
            if expanded(scope) {
                let imports = scope.children().filter_map(ast::Use::cast);
                imports.filter_map(|it| it.use_tree()).for_each(|tree| late.add(tree, None));
            }
            if ast::Module::can_cast(scope.kind()) {
                break;
            }
        }
        late
    }

    /// Adds the imports written in the blocks of a definition, those of the definitions in
    /// it apart.
    fn add_written_in(&mut self, node: ast::Node<'_>) {
        for child in node.children() {
            if let Some(tree) = ast::Use::cast(child).and_then(|it| it.use_tree()) {
                self.add(tree, None);
            } else if !is_definition(child) {
                self.add_written_in(child);
            }
        }
    }

    /// Adds the names the tree brings in, `prefix` being the last name of the path before it.
    fn add(&mut self, tree: ast::UseTree<'_>, prefix: Option<String>) {
        let last = tree.path().and_then(|it| it.segment()).map(|it| it.syntax().text().to_owned());
        let own = if last.as_deref() == Some("self") { prefix } else { last };
        if let Some(list) = tree.use_tree_list() {
            list.use_trees().for_each(|it| self.add(it, own.clone()));
        } else if tree.star_token().is_some() {
            self.glob = true;
        } else if let Some(name) = tree.rename().and_then(|it| it.name()) {
            self.names.insert(name.text().to_owned());
        } else {
            self.names.extend(own);
        }
    }
}

/// Whether an attribute of the item is a macro, which rustc expands the item with: `#[test]`
/// is one, though it leaves the item as it is written.
fn has_attribute_macro<'db>(sema: &hir::view::Semantics<'db>, item: ast::Item<'db>) -> bool {
    sema.resolve_attr_macro_call(item).is_some()
        || ast::HasAttrs::attrs(&item).filter_map(|it| it.path()).any(|path| {
            matches!(
                sema.resolve_path(path),
                Some(hir::PathResolution::Def(ModuleDef::Macro(found))) if found.is_attr(sema.db)
            )
        })
}

/// Whether a macro expanded to the module the node is in, or to one around it.
fn module_is_expanded<'db>(sema: &hir::view::Semantics<'db>, node: ast::Node<'db>) -> bool {
    let mut module = sema.scope(node).map(|it| it.module());
    while let Some(it) = module {
        if it.declaration_source_view(sema.db).is_some_and(|it| it.file_id.is_macro()) {
            return true;
        }
        module = it.parent(sema.db);
    }
    false
}

/// When rustc expands a macro used in a definition.
enum Turn {
    /// As it comes to it.
    Now,
    /// After every macro it can expand at first, the last such first.
    Later,
}

struct Count<'a, 'db> {
    sema: &'a hir::view::Semantics<'db>,
    target: Option<ast::Node<'db>>,
    /// The target and what it is in, the macros it is expanded from with their expansions.
    along: Vec<ast::Node<'db>>,
    counts: Counts<'a>,
    late: LateImports,
    seen: u32,
    earlier: Option<u32>,
    /// The macros to expand after the others, each with the macros it is in.
    waiting: Vec<(ast::MacroCall<'db>, Vec<(syntax::TextRange, syntax::SyntaxKind)>)>,
    /// The macros the nodes being counted are in, as a [`Spot`] has them.
    calls: Vec<(syntax::TextRange, syntax::SyntaxKind)>,
    /// The place of every counted node, when they are all wanted.
    places: Option<FxHashMap<Spot, u32>>,
    twice: Vec<Spot>,
}

impl<'a, 'db> Count<'a, 'db> {
    fn new(
        sema: &'a hir::view::Semantics<'db>,
        target: Option<ast::Node<'db>>,
        along: Vec<ast::Node<'db>>,
        counts: Counts<'a>,
        owner: ast::Node<'db>,
    ) -> Self {
        let late = LateImports::around(sema, owner);
        Count {
            sema,
            target,
            along,
            counts,
            late,
            seen: 0,
            earlier: None,
            waiting: Vec::new(),
            calls: Vec::new(),
            places: None,
            twice: Vec::new(),
        }
    }

    /// Expands the macros that wait for the others, the last come to first.
    fn waiting_ones(&mut self) -> Option<()> {
        while let Some((call, calls)) = self.waiting.pop() {
            self.calls = calls;
            self.expansion_of(call)?;
        }
        Some(())
    }

    /// Counts the nodes of the definition `node` is of, or none when the order rustc numbers
    /// them in is not known.
    ///
    /// rustc numbers what is written in the definition before what the macros used in it
    /// expand to, and then goes through the macros in turn, each the same way.
    fn all_in(&mut self, node: ast::Node<'db>) -> Option<()> {
        let mut calls = Vec::new();
        self.written_in(node, &mut calls);
        for call in calls {
            match self.turn(call) {
                Some(Turn::Now) => self.expansion_of(call)?,
                Some(Turn::Later) => self.waiting.push((call, self.calls.clone())),
                None => self.nothing_in(call)?,
            }
        }
        Some(())
    }

    fn expansion_of(&mut self, call: ast::MacroCall<'db>) -> Option<()> {
        // A macro used in a body is expanded twice, to items and to statements, and of the
        // same text: the target is in one of them, which need not be the one sema gives.
        let at = self.along.iter().position(|&it| it == call.syntax());
        let along = at.and_then(|at| at.checked_sub(1)).map(|at| self.along[at]);
        let root = match along.filter(|root| root.parent().is_none()) {
            Some(root) => root,
            None => match self.sema.expand_macro_call(call) {
                Some(expansion) => expansion.value,
                None => return Some(()),
            },
        };
        self.calls.push((call.syntax().text_range(), root.kind()));
        self.all_in(root)?;
        self.calls.pop();
        Some(())
    }

    /// That the macro expands to nothing that is counted, so that when it is expanded is
    /// the same to the count.
    fn nothing_in(&mut self, call: ast::MacroCall<'db>) -> Option<()> {
        let (seen, waiting) = (self.seen, self.waiting.len());
        self.expansion_of(call)?;
        (self.seen == seen && self.waiting.len() == waiting).then_some(())
    }

    /// As `all_in`, of what is not in a macro: `calls` gets the macros come across.
    fn written_in(&mut self, node: ast::Node<'db>, calls: &mut Vec<ast::MacroCall<'db>>) {
        for child in node.children() {
            if let Some(call) = ast::MacroCall::cast(child) {
                calls.push(call);
                continue;
            }
            let target = self.target == Some(child);
            if target {
                self.earlier = Some(self.seen);
            }
            let counted = self.counts.holds(child);
            if counted && let Some(places) = &mut self.places {
                let spot = (self.calls.clone(), child.text_range());
                if places.insert(spot.clone(), self.seen).is_some() {
                    self.twice.push(spot);
                }
            }
            if target || counted {
                self.seen += 1;
            }
            if !target && !is_definition(child) {
                self.written_in(child, calls);
            }
        }
    }

    /// When rustc expands the macro, or none when that is not known: a macro it finds by a
    /// late import waits for the import.
    fn turn(&self, call: ast::MacroCall<'db>) -> Option<Turn> {
        let Some(path) = call.path() else { return Some(Turn::Now) };
        let Some(first) = path.first_segment().and_then(|it| it.name_ref()) else {
            return Some(Turn::Now);
        };
        let alone = path.qualifier().is_none();
        let imported = self.late.names.contains(first.text());
        if !alone && !imported {
            return self.reached_by_path(call).then_some(Turn::Now);
        }
        if !imported && !self.late.glob {
            return Some(Turn::Now);
        }
        if alone && self.in_textual_scope(call)? {
            return Some(Turn::Now);
        }
        if imported {
            return Some(Turn::Later);
        }
        // A name alone may be found by the glob import: not one of this crate or of the
        // standard library, which the macros in scope everywhere are.
        let home = self.sema.resolve_macro_call(call)?.module(self.sema.db).krate(self.sema.db);
        let here = self.sema.scope(call.syntax())?.krate();
        (home == here || home.origin(self.sema.db).is_lang()).then_some(Turn::Now)
    }

    /// Whether a path finds the macro with no late import: any but a `macro_rules!` one of
    /// this crate that is not exported, which is in no module but by an import of it, and is
    /// found at once only through an import written in the module the path names.
    fn reached_by_path(&self, call: ast::MacroCall<'db>) -> bool {
        let sema = self.sema;
        let Some(found) = sema.resolve_macro_call(call) else { return true };
        let here = sema.scope(call.syntax()).map(|it| it.krate());
        if Some(found.module(sema.db).krate(sema.db)) != here {
            return true;
        }
        match sema.source(found).and_then(|it| it.value.left()) {
            Some(ast::Macro::MacroRules(rules)) => {
                ast::HasAttrs::attrs(&rules)
                    .any(|it| it.simple_name().is_some_and(|name| name == "macro_export"))
                    || self.imported_where_written(call)
            }
            _ => true,
        }
    }

    /// Whether the module that the path of `call` names imports the macro by a `use` written
    /// in it, which no macro made: rustc resolves those before it expands any macro, as
    /// `crate::tracing::debug!` finds salsa's `pub(crate) use debug;`.
    fn imported_where_written(&self, call: ast::MacroCall<'db>) -> bool {
        let sema = self.sema;
        let Some(path) = call.path() else { return false };
        let (Some(qualifier), Some(name)) =
            (path.qualifier(), path.segment().and_then(|it| it.name_ref()))
        else {
            return false;
        };
        let Some(hir::PathResolution::Def(ModuleDef::Module(module))) =
            sema.resolve_path(qualifier)
        else {
            return false;
        };
        let mut around = Some(module);
        while let Some(it) = around {
            if it.declaration_source_view(sema.db).is_some_and(|it| it.file_id.is_macro()) {
                return false;
            }
            around = it.parent(sema.db);
        }
        let written = sema.module_definition_node(module);
        if written.file_id.is_macro() {
            return false;
        }
        let items = match ast::Module::cast(written.value) {
            Some(inline) => inline.item_list().map(|it| it.syntax()),
            None => ast::SourceFile::can_cast(written.value.kind()).then_some(written.value),
        };
        let uses = items.into_iter().flat_map(|it| it.children()).filter_map(ast::Use::cast);
        uses.filter(|&it| !has_attribute_macro(sema, ast::Item::Use(it))).any(|it| {
            let mut imported = LateImports::default();
            it.use_tree().into_iter().for_each(|tree| imported.add(tree, None));
            imported.names.contains(name.text())
        })
    }

    /// Whether the macro is a `macro_rules!` one found by where it is written, which is
    /// before an import of the name is looked for: one written earlier, beside something
    /// the use is in, or in a `#[macro_use]` module that is. None when that is not known:
    /// a macro expanded to it or to a module around it.
    fn in_textual_scope(&self, call: ast::MacroCall<'db>) -> Option<bool> {
        let sema = self.sema;
        let Some(found) = sema.resolve_macro_call(call) else { return Some(false) };
        let Some(source) = sema.source(found) else { return Some(false) };
        let Some(ast::Macro::MacroRules(rules)) = source.value.left() else {
            return Some(false);
        };
        let named = rules.name().map(|it| it.text());
        let used = call.path().and_then(|it| it.segment()).map(|it| it.syntax().text());
        // Another crate's are in scope by `#[macro_use]`, which is looked in after imports.
        let home = found.module(sema.db).krate(sema.db);
        if named != used || Some(home) != sema.scope(call.syntax()).map(|it| it.krate()) {
            return Some(false);
        }
        let mut module = found.module(sema.db);
        let mut written = hir::InFile::new(source.file_id, rules.syntax());
        loop {
            if written.file_id.is_macro() {
                return None;
            }
            let beside = sema.ancestors_with_macros(call.syntax()).any(|around| {
                around.parent() == written.value.parent()
                    && sema.hir_file_for(around) == written.file_id
                    && written.value.text_range().end() <= around.text_range().start()
            });
            if beside {
                return Some(true);
            }
            // The scope goes on after a module that is `#[macro_use]`.
            let inline = written.value.ancestors().skip(1).find_map(ast::Module::cast);
            let around = match inline {
                Some(inline) => written.with_value(inline),
                None => match module.declaration_source_view(sema.db) {
                    Some(declared) => declared,
                    None => return Some(false),
                },
            };
            let goes_on = ast::HasAttrs::attrs(&around.value)
                .any(|it| it.simple_name().is_some_and(|name| name == "macro_use"));
            if !goes_on {
                return Some(false);
            }
            module = match module.parent(sema.db) {
                Some(parent) => parent,
                None => return Some(false),
            };
            written = around.map(|it| it.syntax());
        }
    }
}

/// The `n` of the `{closure#n}` rustc names a closure: its place among the closures of the
/// definition it is written in, a closure's own closures being counted apart.
pub(crate) fn closure_number<'db>(
    sema: &hir::view::Semantics<'db>,
    places: &Places,
    closure: ast::Node<'db>,
) -> Option<u32> {
    place_counted(sema, places, Counted::Closures, closure, Counts::Closures)
}
