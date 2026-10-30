//! Bodies on demand. A block with items is made as the body it is in is lowered, so what is
//! declared in bodies is found one body at a time, from the body that is asked for, and the body
//! a block is in is found from syntax and the def map, without lowering any other body
//! (#1293, #806).
//!
//! What a driver calls:
//! - every body of a crate checked fully (the workspace's): [`every_body`], then
//!   [`body_blocks`] of each for the items its blocks declare;
//! - a body of a dependency that codegen reached: [`body_blocks`] of that body;
//! - the bodies of a dependency that may declare an item a build keeps whether or not anything
//!   reaches it (`#[used]`, `#[no_mangle]`, `#[export_name]`, `#[link_section]`, the
//!   allocator, ...): [`bodies_declaring_roots`].

use std::ops::ControlFlow;

use base_db::{Crate, SourceDatabase};
use hir_expand::{ErasedAstId, InFile, name::Name};
use intern::Symbol;
use rustc_hash::FxHashSet;
use syntax::{AstNode, SmolStr, SyntaxKind, SyntaxNode, T, TextRange, ast};

use crate::{
    AdtId, AssocItemId, AstId, BlockId, DefWithBodyId, HasModule, ItemContainerId, Lookup,
    MacroExpander, MacroId, ModuleDefId, ModuleId,
    expr_store::Body,
    item_scope::ItemScope,
    nameres::{
        DefMap,
        assoc::{ImplItems, TraitItems},
        crate_def_map,
    },
    signatures::EnumVariants,
    src::HasSource,
};

/// The items of `scope` that have a body, in the order the scope lists them: its functions,
/// consts (named or not) and statics, the variants of its enums written with a discriminant,
/// and the functions and consts of its traits and impls. A function or static of an `extern`
/// block has none.
pub fn scope_body_owners(db: &dyn SourceDatabase, scope: &ItemScope) -> Vec<DefWithBodyId> {
    let mut owners = Vec::new();
    push_scope_body_owners(db, scope, &mut owners);
    owners
}

fn push_scope_body_owners(
    db: &dyn SourceDatabase,
    scope: &ItemScope,
    owners: &mut Vec<DefWithBodyId>,
) {
    let _ = visit_scope_body_owners(db, scope, |it| {
        owners.push(it);
        ControlFlow::Continue(())
    });
}

/// Calls `each` with the items of `scope` that have a body, in [`scope_body_owners`]' order,
/// until it breaks.
fn visit_scope_body_owners(
    db: &dyn SourceDatabase,
    scope: &ItemScope,
    mut each: impl FnMut(DefWithBodyId) -> ControlFlow<DefWithBodyId>,
) -> ControlFlow<DefWithBodyId> {
    for it in scope.unnamed_consts() {
        each(it.into())?;
    }
    let in_extern_block =
        |container: ItemContainerId| matches!(container, ItemContainerId::ExternBlockId(_));
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) if !in_extern_block(it.lookup(db).container) => {
                each(it.into())?
            }
            ModuleDefId::ConstId(it) => each(it.into())?,
            ModuleDefId::StaticId(it) if !in_extern_block(it.lookup(db).container) => {
                each(it.into())?
            }
            ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                let variants = EnumVariants::of(db, it).0.with_explicit_discriminant();
                for (variant, _, explicit) in variants {
                    if explicit {
                        each(DefWithBodyId::VariantId(variant))?;
                    }
                }
            }
            ModuleDefId::TraitId(it) => visit_assoc(&TraitItems::query(db, it).items, &mut each)?,
            _ => {}
        }
    }
    for it in scope.impls() {
        visit_assoc(&ImplItems::of(db, it).0.items, &mut each)?;
    }
    ControlFlow::Continue(())
}

fn visit_assoc(
    items: &[(Name, AssocItemId)],
    each: &mut impl FnMut(DefWithBodyId) -> ControlFlow<DefWithBodyId>,
) -> ControlFlow<DefWithBodyId> {
    for &(_, item) in items {
        match item {
            AssocItemId::FunctionId(it) => each(it.into())?,
            AssocItemId::ConstId(it) => each(it.into())?,
            AssocItemId::TypeAliasId(_) => {}
        }
    }
    ControlFlow::Continue(())
}

/// The items with a body declared in `module`, outside any body. Lowers no body.
pub fn module_body_owners(db: &dyn SourceDatabase, module: ModuleId) -> Vec<DefWithBodyId> {
    scope_body_owners(db, module.def_map(db).module(db, module).scope(db))
}

/// The items with a body declared in the modules of `krate`, outside any body. Lowers no body.
pub fn crate_body_owners(db: &dyn SourceDatabase, krate: Crate) -> Vec<DefWithBodyId> {
    let mut owners = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        push_scope_body_owners(db, module.scope(db), &mut owners);
    }
    owners
}

/// The blocks with items of one body, and the items with a body they declare.
#[derive(Debug)]
pub struct BodyBlocks<'db> {
    /// Each block of the body that declares items, nested ones included, with its def map.
    pub blocks: Vec<(BlockId, &'db DefMap)>,
    /// The items with a body declared in those blocks, in any of their modules. What these
    /// declare is in their own bodies, which this does not lower.
    pub owners: Vec<DefWithBodyId>,
}

/// The blocks of the body of `owner` and what they declare, by lowering that body and no other.
pub fn body_blocks(db: &dyn SourceDatabase, owner: DefWithBodyId) -> BodyBlocks<'_> {
    let blocks: Vec<_> = Body::of(db, owner).blocks(db).collect();
    let mut owners = Vec::new();
    for &(_, def_map) in &blocks {
        for (_, module) in def_map.modules(db) {
            push_scope_body_owners(db, module.scope(db), &mut owners);
        }
    }
    BodyBlocks { blocks, owners }
}

/// Visits the bodies of `roots` that `lower` accepts and, in turn, those of the items their
/// blocks declare, with their blocks; lowers no body `lower` refuses. A body is visited once.
pub fn walk_bodies<'db>(
    db: &'db dyn SourceDatabase,
    roots: Vec<DefWithBodyId>,
    mut lower: impl FnMut(DefWithBodyId) -> bool,
    mut each: impl FnMut(DefWithBodyId, &BodyBlocks<'db>),
) {
    let mut seen: FxHashSet<DefWithBodyId> = roots.iter().copied().collect();
    let mut queue = roots;
    queue.reverse();
    while let Some(owner) = queue.pop() {
        if !lower(owner) {
            continue;
        }
        let blocks = body_blocks(db, owner);
        each(owner, &blocks);
        queue.extend(blocks.owners.iter().rev().copied().filter(|&it| seen.insert(it)));
    }
}

/// Every body of `krate`, those declared in bodies included, each lowered: for a crate that is
/// checked fully. The order is the def map's, each body before those declared in it.
pub fn every_body(db: &dyn SourceDatabase, krate: Crate) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    walk_bodies(db, crate_body_owners(db, krate), |_| true, |owner, _| bodies.push(owner));
    bodies
}

/// The bodies of `krate` lowered to find the items in them that a build keeps whether or not
/// anything reaches them ([`may_declare_roots`]), and those declared in them that may too: the
/// only bodies of a dependency a build lowers before codegen reaches them. Lowers no other body.
pub fn bodies_declaring_roots(db: &dyn SourceDatabase, krate: Crate) -> Vec<DefWithBodyId> {
    // The macros of the blocks lowered so far: a `macro_rules!` of a block is seen by the
    // bodies declared in it, which are visited after it. Seen by the others too, which only
    // lowers more.
    let local = std::cell::RefCell::new(FxHashSet::<SmolStr>::default());
    let mut bodies = Vec::new();
    let roots = crate_body_owners(db, krate);
    walk_bodies(
        db,
        roots,
        |owner| may_declare_roots_with(db, owner, &local.borrow()),
        |owner, blocks| {
            bodies.push(owner);
            for &(_, def_map) in &blocks.blocks {
                for (_, module) in def_map.modules(db) {
                    let found = declaring_macros_of_scope(db, module.scope(db), &local.borrow());
                    local.borrow_mut().extend(found.iter().map(|it| SmolStr::new(it.as_str())));
                }
            }
        },
    );
    bodies
}

/// The body that `block` is in. Found from the syntax around the block, up to the innermost
/// item with a body (in a macro's expansion, up to its call), and that item from the def map of
/// the module the block is in; the one body lowered is that one's, to check that the block is
/// in it. A block in an expression of a signature (an array length, a const argument, a
/// field's default) is in no body.
pub fn block_owner(db: &dyn SourceDatabase, block: BlockId) -> Result<DefWithBodyId, String> {
    let item = owning_item(db, block.ast_id(db))?;
    let mut module = unsafe { block.module(db).to_static() };
    let owner = loop {
        if let Some(owner) = owner_in_module(db, module, item) {
            break owner;
        }
        // The module of a block in a block of the same body is the outer block's.
        let outer = match module.block(db) {
            Some(outer) if module.is_block_module(db) => unsafe { outer.to_static() },
            _ => {
                return Err(format!(
                    "the item a block is in is not declared around it ({:?})",
                    block.ast_id(db)
                ));
            }
        };
        module = unsafe { outer.module(db).to_static() };
    };
    if !Body::of(db, owner).blocks(db).any(|(it, _)| it == block) {
        return Err(format!("a block not in the body of the item it is in ({owner:?})"));
    }
    Ok(owner)
}

/// The item with a body that `owner` is, as its id.
fn owner_ast_id(db: &dyn SourceDatabase, owner: DefWithBodyId) -> ErasedAstId {
    match owner {
        DefWithBodyId::FunctionId(it) => it.lookup(db).id.erase(),
        DefWithBodyId::ConstId(it) => it.lookup(db).id.erase(),
        DefWithBodyId::StaticId(it) => it.lookup(db).id.erase(),
        DefWithBodyId::VariantId(it) => it.lookup(db).id.erase(),
    }
}

fn owner_in_module(
    db: &dyn SourceDatabase,
    module: ModuleId,
    item: ErasedAstId,
) -> Option<DefWithBodyId> {
    let scope = module.def_map(db).module(db, module).scope(db);
    visit_scope_body_owners(db, scope, |it| {
        if owner_ast_id(db, it) == item {
            ControlFlow::Break(it)
        } else {
            ControlFlow::Continue(())
        }
    })
    .break_value()
}

/// The innermost item with a body whose body holds `block`: a function, a const, a static or
/// an enum variant's discriminant. Inside a macro's expansion with no such item, the call is
/// where the expansion is, and the search goes on from there in the file of the call.
fn owning_item(
    db: &dyn SourceDatabase,
    block: AstId<ast::BlockExpr>,
) -> Result<ErasedAstId, String> {
    fn range<N: AstNode>(it: N) -> TextRange {
        it.syntax().text_range()
    }
    let mut file = block.file_id;
    let mut node: SyntaxNode = block.to_node(db).syntax().clone();
    loop {
        let at = node.text_range();
        let map = file.ast_id_map(db);
        for ancestor in node.ancestors() {
            if let Some(variant) = ast::Variant::cast(ancestor.clone()) {
                within(variant.const_arg().map(range), at, "an enum variant's fields")?;
                return Ok(InFile::new(file, map.ast_id(&variant).erase()));
            }
            let Some(item) = ast::Item::cast(ancestor) else { continue };
            let (body, item) = match &item {
                ast::Item::Fn(it) => (it.body().map(range), map.ast_id(it).erase()),
                ast::Item::Const(it) => (it.body().map(range), map.ast_id(it).erase()),
                ast::Item::Static(it) => (it.body().map(range), map.ast_id(it).erase()),
                // In a body these are expressions or statements, with the block around them:
                // a macro call's arguments are tokens.
                ast::Item::MacroCall(_) | ast::Item::AsmExpr(_) => continue,
                _ => return Err(format!("a block in the signature of an item ({block:?})")),
            };
            within(body, at, "the signature of an item")?;
            return Ok(InFile::new(file, item));
        }
        let Some(call) = file.call_node(db) else {
            return Err(format!("a block in no item with a body ({block:?})"));
        };
        file = call.file_id;
        node = call.value;
    }
}

/// Whether `at` is in `body`; else it is in `elsewhere`, which has no body.
fn within(body: Option<TextRange>, at: TextRange, elsewhere: &str) -> Result<(), String> {
    match body {
        Some(body) if body.contains_range(at) => Ok(()),
        _ => Err(format!("a block in {elsewhere}, which is in no body")),
    }
}

/// The words in a body that may declare an item a build keeps whether or not anything reaches
/// it: the attributes that export or keep an item, link one by name or make one the allocator,
/// and the trait of an allocator. They are looked for as words, so a body that only says one is
/// lowered all the same.
const ROOT_WORDS: &[&str] = &[
    "no_mangle",
    "export_name",
    "used",
    "link_section",
    "linkage",
    "rustc_std_internal_symbol",
    "panic_handler",
    "lang",
    "global_allocator",
    "alloc_error_handler",
    "GlobalAlloc",
];

fn is_root_word(text: &str) -> bool {
    ROOT_WORDS.contains(&text.strip_prefix("r#").unwrap_or(text))
}

/// Whether `f` accepts a word of `node`: an identifier, but those of a macro's metavariables
/// (`$name`), without `r#`. Read in place from the tokens: a body's words are not interned, so
/// scanning a dependency's bodies writes nothing to the symbol table.
fn any_word(node: &SyntaxNode, mut f: impl FnMut(&str) -> bool) -> bool {
    let mut after_dollar = false;
    for token in node.descendants_with_tokens().filter_map(|it| it.into_token()) {
        let kind = token.kind();
        if kind.is_trivia() {
            continue;
        }
        let dollar = std::mem::replace(&mut after_dollar, kind == T![$]);
        if kind == SyntaxKind::IDENT && !dollar {
            let text = token.text();
            if f(text.strip_prefix("r#").unwrap_or(text)) {
                return true;
            }
        }
    }
    false
}

/// The words of `node` ([`any_word`]), interned: for a macro's definition, read once per macro.
fn words(node: &SyntaxNode) -> Vec<Symbol> {
    let mut words = Vec::new();
    any_word(node, |word| {
        words.push(Symbol::intern(word));
        false
    });
    words
}

/// The expression of the body of `owner`, as it is written.
fn body_syntax(db: &dyn SourceDatabase, owner: DefWithBodyId) -> Option<SyntaxNode> {
    match owner {
        DefWithBodyId::FunctionId(it) => {
            it.lookup(db).source(db).value.body().map(|it| it.syntax().clone())
        }
        DefWithBodyId::ConstId(it) => {
            it.lookup(db).source(db).value.body().map(|it| it.syntax().clone())
        }
        DefWithBodyId::StaticId(it) => {
            it.lookup(db).source(db).value.body().map(|it| it.syntax().clone())
        }
        DefWithBodyId::VariantId(it) => {
            it.lookup(db).source(db).value.const_arg().map(|it| it.syntax().clone())
        }
    }
}

/// Whether the body of `owner` may declare an item a build keeps whether or not anything
/// reaches it: its text says one of [`ROOT_WORDS`], or names a macro of its crate or of a
/// crate it depends on that may write one ([`macros_declaring_roots`]). Reads the body's
/// syntax, and lowers no body. A body of which this is false declares no such item, though it
/// may declare others; those are reached through it.
pub fn may_declare_roots(db: &dyn SourceDatabase, owner: DefWithBodyId) -> bool {
    may_declare_roots_with(db, owner, &FxHashSet::default())
}

/// [`may_declare_roots`], with the macros of the blocks around the body in `local`.
fn may_declare_roots_with(
    db: &dyn SourceDatabase,
    owner: DefWithBodyId,
    local: &FxHashSet<SmolStr>,
) -> bool {
    let Some(body) = body_syntax(db, owner) else { return false };
    let declaring = macros_declaring_roots(db, owner.module(db).krate(db));
    any_word(&body, |word| {
        is_root_word(word)
            || local.contains(word)
            || declaring.binary_search_by(|it| it.as_str().cmp(word)).is_ok()
    })
}

/// What a macro is made of, for whether it may write an item a build keeps.
enum MacroText {
    /// It may write anything: a procedural macro, or `include!`.
    Anything,
    /// It writes only what its definition says: the words of it.
    Words(Vec<Symbol>),
    /// A built-in macro that writes no item.
    Nothing,
}

fn macro_text(db: &dyn SourceDatabase, id: MacroId) -> MacroText {
    let (expander, node) = match id {
        MacroId::ProcMacroId(_) => return MacroText::Anything,
        MacroId::MacroRulesId(it) => {
            let loc = it.lookup(db);
            (loc.expander, loc.id.to_node(db).syntax().clone())
        }
        MacroId::Macro2Id(it) => {
            let loc = it.lookup(db);
            (loc.expander, loc.id.to_node(db).syntax().clone())
        }
    };
    match expander {
        MacroExpander::Declarative { .. } => {
            let mut words = words(&node);
            words.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
            words.dedup();
            MacroText::Words(words)
        }
        MacroExpander::BuiltInEager(it) if it.is_include() => MacroText::Anything,
        MacroExpander::BuiltIn(_)
        | MacroExpander::BuiltInAttr(_)
        | MacroExpander::BuiltInDerive(_)
        | MacroExpander::BuiltInEager(_) => MacroText::Nothing,
        // Its expansion is unknown, so it may be anything.
        MacroExpander::UnimplementedBuiltIn => MacroText::Anything,
    }
}

/// The macros of `scope`, by every name it has them under, that may write an item a build keeps,
/// given that those named in `declaring` may.
fn declaring_macros_of_scope(
    db: &dyn SourceDatabase,
    scope: &ItemScope,
    declaring: &FxHashSet<SmolStr>,
) -> Vec<Symbol> {
    let mut found = Vec::new();
    let mut calling = Vec::new();
    for (name, id) in scope_macros(scope) {
        classify(db, name, id, &mut found, &mut calling);
    }
    settle(found, calling, |word| declaring.contains(word.as_str()))
}

fn scope_macros(scope: &ItemScope) -> impl Iterator<Item = (Symbol, MacroId)> + '_ {
    let named = scope.macros().map(|(name, it)| (name.symbol().clone(), it.def));
    let legacy = scope
        .legacy_macros()
        .flat_map(|(name, ids)| ids.iter().map(move |&id| (name.symbol().clone(), id)));
    named.chain(legacy)
}

fn classify(
    db: &dyn SourceDatabase,
    name: Symbol,
    id: MacroId,
    found: &mut Vec<Symbol>,
    calling: &mut Vec<(Symbol, Vec<Symbol>)>,
) {
    match macro_text(db, id) {
        MacroText::Anything => found.push(name),
        MacroText::Nothing => {}
        MacroText::Words(words) if words.iter().any(|it| is_root_word(it.as_str())) => {
            found.push(name)
        }
        MacroText::Words(words) => calling.push((name, words)),
    }
}

/// `found`, with each of `calling` that names one of them, or one `known` says may write a
/// kept item, until no more do.
fn settle(
    mut found: Vec<Symbol>,
    mut calling: Vec<(Symbol, Vec<Symbol>)>,
    known: impl Fn(&Symbol) -> bool,
) -> Vec<Symbol> {
    let mut writing: FxHashSet<Symbol> = found.iter().cloned().collect();
    loop {
        let before = found.len();
        calling.retain(|(name, words)| {
            let writes = words.iter().any(|it| known(it) || writing.contains(it));
            if writes {
                found.push(name.clone());
                writing.insert(name.clone());
            }
            !writes
        });
        if found.len() == before {
            return found;
        }
    }
}

/// The names of the macros of `krate` and of the crates it depends on that may write an item a
/// build keeps whether or not anything reaches it: a declarative one whose text says one of
/// [`ROOT_WORDS`] or names one that may, a procedural one, and `include!`. By name, under each
/// name a module has one, so a macro of the same name costs a lowering and nothing else.
/// Sorted. Reads def maps and macro definitions, no body.
#[salsa::tracked(returns(deref))]
pub fn macros_declaring_roots(db: &dyn SourceDatabase, krate: Crate) -> Box<[Symbol]> {
    let mut known: FxHashSet<Symbol> = FxHashSet::default();
    for dep in &krate.data(db).dependencies {
        known.extend(macros_declaring_roots(db, dep.crate_id).iter().cloned());
    }
    let mut found = Vec::new();
    let mut calling = Vec::new();
    let mut seen = FxHashSet::default();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for (name, id) in scope_macros(module.scope(db)) {
            if seen.insert((name.clone(), id)) {
                classify(db, name, id, &mut found, &mut calling);
            }
        }
    }
    let settled = settle(found, calling, |word| known.contains(word));
    known.extend(settled);
    let mut names: Vec<Symbol> = known.into_iter().collect();
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    names.into()
}
