//! A source file's [`ItemTree`] per item: each top-level item is lowered on its own
//! ([`item_fragment`]) from what [`item_sources`] cuts out of the file for it, and the file's
//! tree is its top level's attributes ([`top_attrs`]) and the items' trees, assembled
//! ([`assemble`]). An edit inside one item lowers that item again and no other; one inside the
//! body of a top-level `fn` or of an `impl`'s or `trait`'s lowers none, as bodies are not part
//! of what lowering reads ([`hash_item`]). One inside a `fn` of an inline module lowers the
//! module again, as it moves the module's later items. The tree assembled is the one
//! [`super::lower_whole`] lowers in one pass, to the byte.

use std::{
    fmt,
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

use base_db::{Db, Crate, EditionedFileId};
use blaze_ids::Domain;
use hir_expand::{HirFileId, span_map::SpanMap};
use span::{AstIdMap, ErasedFileAstId, RealSpanMap};
use syntax::{
    AstNode, FlatTree, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize,
    ast::{self, HasModuleItem},
};

use crate::{
    attrs::parse_extra_crate_attrs,
    item_tree::{
        AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, Function, IdRange, ItemKind,
        ItemTree, Macro2, MacroCall, MacroRules, Mod, ModItemId, NameId, PathId, RawVisibilityId,
        Static, Struct, Trait, TypeAlias, Union, Use, UseTree, UseTreeId, lower,
    },
};

#[cfg(test)]
mod tests;

/// The tree of a source file, from its items' ([`assemble`]).
pub(super) fn file_item_tree(
    db: &Db,
    file: EditionedFileId,
    krate: Crate,
) -> ItemTree {
    let top = top_attrs(db, file, krate);
    // alloc: once a run, the list of the file's parts
    let fragments: Vec<&ItemTree> =
        item_sources(db, file, krate).iter().map(|&item| item_fragment(db, item)).collect();
    assemble(top, &fragments)
}

/// One top-level item of a file, to lower on its own: its identity is its syntax's
/// ([`ItemSyntax`]), so the same item in the next parse is this one and its tree is reused.
#[salsa::tracked]
pub(super) struct ItemSource<'db> {
    krate: Crate,
    #[returns(ref)]
    syntax: ItemSyntax,
}

/// What lowering one top-level item reads: its subtree in its file's tree, the ast ids of the
/// items it is made of and the span anchors lowering looks up within it, all from where it
/// starts. Two are equal when their keys are, which hash what of that lowering reads
/// ([`hash_item`]): two items with equal keys lower to the same tree wherever they are in their
/// files, so one whose `fn` bodies alone changed keeps its tree.
///
/// An item kept from an older parse keeps that parse's tree alive, so a file's trees outlive it
/// until every item of theirs is edited or gone; each is a file's text and words once.
pub(super) struct ItemSyntax {
    key: ItemKey,
    file: Arc<FileSyntax>,
    /// The item's element in `file.tree`.
    at: u32,
    /// The item's of `file.ids` and `file.anchors`.
    ids: Range<u32>,
    anchors: Range<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ItemKey {
    file: span::EditionedFileId,
    ast_id: ErasedFileAstId,
    /// Of the item's subtree, ids and anchors ([`item_sources`]).
    fingerprint: u128,
}

/// The parse a file's [`ItemSyntax`]es were cut from, shared by all of them.
struct FileSyntax {
    tree: Arc<FlatTree>,
    /// Per item, as [`lower::Ctx::for_item`] reads them: `[start, kind, id]` of each item node
    /// lowering asks the id of, from where the top-level item starts, by start.
    ids: Box<[[u32; 3]]>,
    /// Per item: `[start, id]` of each span anchor within it that is one of those nodes, from
    /// where it starts, by start; the first the item's own. The anchors of an `impl`'s or a
    /// `trait`'s items are left out: lowering looks up no span past their start.
    anchors: Box<[[u32; 2]]>,
}

impl PartialEq for ItemSyntax {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for ItemSyntax {}

impl Hash for ItemSyntax {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl fmt::Debug for ItemSyntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemSyntax")
            .field("file", &self.key.file)
            .field("ast_id", &self.key.ast_id)
            .finish_non_exhaustive()
    }
}

fn slice<T>(all: &[T], range: &Range<u32>) -> &[T] {
    &all[range.start as usize..range.end as usize]
}

fn range(range: Range<usize>) -> Range<u32> {
    let end = u32::try_from(range.end).expect("a file's syntax indexes in `u32`");
    range.start as u32..end
}

impl ItemSyntax {
    fn lower(&self, db: &Db, krate: Crate) -> ItemTree {
        let file = &*self.file;
        let root = syntax::Tree::new(file.tree.clone()).root(); // clone: an `Arc`, the cursor's
        let NodeOrToken::Node(node) = root.flat_element(self.at) else {
            panic!("the element of the item {:?} is a token", self.key.ast_id)
        };
        let item = ast::Item::cast(node).expect("an item's element is an item");
        let item_range = item.syntax().text_range();
        let pairs = slice(&file.anchors, &self.anchors)
            .iter()
            .map(|&[start, id]| (TextSize::new(start), ErasedFileAstId::from_raw(id)))
            .collect(); // alloc: the item's span map, once a lowering of a changed item
        let spans = RealSpanMap::from_file(self.key.file, pairs, item_range.len());
        let ids = slice(&file.ids, &self.ids);
        lower::Ctx::for_item(db, krate, ids, &spans, item_range.start()).lower_item(&item)
    }
}

/// The domain of an [`ItemKey`]'s fingerprint.
static ITEM: Domain = Domain::new("blaze v2 item syntax 1");

/// What [`Hash`] writes of one item, collected to hash in one BLAKE3 pass: an item's syntax is
/// tens of small writes, each a copy here where it would be an update of the hasher's state.
struct Feed<'a>(&'a mut Vec<u8>);

impl Feed<'_> {
    fn fingerprint(&self) -> u128 {
        blaze_ids::Hasher::of(&ITEM).update(&self.0[..]).finish().fingerprint().0
    }
}

impl Hasher for Feed<'_> {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn finish(&self) -> u64 {
        self.fingerprint() as u64
    }
}

/// The arrays [`item_sources`] fills, empty between runs and keeping their capacity, so a worker
/// grows none again once it has cut a file as big. A scratch that grew past [`SCRATCH_IDS`]
/// (a generated file) is dropped, so a worker keeps a bounded amount.
#[derive(Default)]
struct Scratch {
    ids: Vec<[u32; 3]>,
    anchors: Vec<[u32; 2]>,
    items: Vec<(ItemKey, u32, Range<u32>, Range<u32>)>,
    bytes: Vec<u8>,
}

thread_local! {
    static SCRATCH: std::cell::Cell<Scratch> = std::cell::Cell::new(Scratch::default());
}

/// About 200 KiB of a [`Scratch`]'s arrays.
const SCRATCH_IDS: usize = 1 << 14;

/// The top-level items of `file`, each cut out to lower on its own; one walk of the file's
/// items, its ids and anchors in two arrays all the items share. A run allocates the two
/// arrays, the `Arc` holding them and the list of items, and nothing for each item.
#[salsa::tracked(returns(ref))]
pub(super) fn item_sources<'db>(
    db: &'db Db,
    file: EditionedFileId,
    krate: Crate,
) -> Box<[ItemSource<'db>]> {
    let _p = tracing::info_span!("item_sources", ?file).entered();
    let source_file = file.parse(db).tree();
    let hir_file = HirFileId::from(file);
    let ast_id_map = hir_file.ast_id_map(db);
    let SpanMap::RealSpanMap(span_map) = hir_file.span_map(db) else {
        unreachable!("a source file's span map is a real file's")
    };
    let file_id = file.span_file_id(db);
    let (tree, _) = source_file.syntax().flat().expect("a source file is a flat tree's root");
    let tree = tree.clone(); // clone: an `Arc`, the file's tree the items keep

    let mut scratch = SCRATCH.take();
    let Scratch { ids, anchors, items, bytes } = &mut scratch;
    for item in source_file.items() {
        let node = item.syntax();
        let (_, at) = node.flat().expect("a top-level item is in its file's tree");
        let item_range = node.text_range();
        let ids_from = ids.len();
        item_ids(&item, ast_id_map, item_range.start(), ids);
        let anchors_from = anchors.len();
        item_anchors(span_map, item_range, &ids[ids_from..], anchors);

        bytes.clear();
        let mut feed = Feed(&mut *bytes);
        file_id.hash(&mut feed);
        hash_item(node, &mut feed);
        ids[ids_from..].hash(&mut feed);
        anchors[anchors_from..].hash(&mut feed);
        let key = ItemKey {
            file: file_id,
            ast_id: ErasedFileAstId::from_raw(ids[ids_from][2]),
            fingerprint: feed.fingerprint(),
        };
        items.push((key, at, range(ids_from..ids.len()), range(anchors_from..anchors.len())));
    }

    // alloc: once a run, the arrays every item of the file shares
    let file = Arc::new(FileSyntax { tree, ids: ids[..].into(), anchors: anchors[..].into() });
    let sources = (items.drain(..))
        .map(|(key, at, ids, anchors)| {
            // clone: an `Arc`, the file's syntax each item holds
            let file = file.clone();
            ItemSource::new(db, krate, ItemSyntax { key, file, at, ids, anchors })
        })
        .collect(); // alloc: the query's value, once a run
    ids.clear();
    anchors.clear();
    if ids.capacity() <= SCRATCH_IDS {
        SCRATCH.set(scratch);
    }
    sources
}

/// The ids of `item` and the items it is made of, the nodes [`lower::Ctx`] asks the ids of, in
/// preorder (by start).
fn item_ids(item: &ast::Item, map: &AstIdMap, start: TextSize, out: &mut Vec<[u32; 3]>) {
    push_id(out, item.syntax(), start, map.ast_id(item).erase());
    match item {
        ast::Item::Module(module) => {
            for child in module.item_list().into_iter().flat_map(|list| list.items()) {
                item_ids(&child, map, start, out);
            }
        }
        ast::Item::ExternBlock(block) => {
            for child in block.extern_item_list().into_iter().flat_map(|list| list.extern_items()) {
                let child = ast::Item::from(child);
                push_id(out, child.syntax(), start, map.ast_id(&child).erase());
            }
        }
        _ => {}
    }
}

fn push_id(out: &mut Vec<[u32; 3]>, node: &SyntaxNode, start: TextSize, id: ErasedFileAstId) {
    let at = u32::from(node.text_range().start() - start);
    out.push([at, node.kind() as u32, id.into_raw()]);
}

/// The anchors of `map` that start within `item` at one of its `ids`, from its start: the
/// item's own first, as every top-level item is one.
fn item_anchors(map: &RealSpanMap, item: TextRange, ids: &[[u32; 3]], out: &mut Vec<[u32; 2]>) {
    let from = out.len();
    let mut offset = item.end().checked_sub(TextSize::new(1)).unwrap_or(item.start());
    loop {
        let (start, id) = map.anchor_at(offset);
        assert!(
            start >= item.start(),
            "the top-level item at {item:?} is no anchor of its file's span map"
        );
        let at = u32::from(start - item.start());
        if ids.binary_search_by_key(&at, |it| it[0]).is_ok() {
            out.push([at, id.into_raw()]);
        }
        if start == item.start() {
            break;
        }
        offset = start - TextSize::new(1);
    }
    out[from..].reverse();
}

/// Hashes what lowering reads of `item`'s syntax ([`Ctx`](lower::Ctx)): all of it, but of the
/// bodies of its `fn`s only the inner attributes at their start, of the items of its `impl`s
/// and `trait`s the same, and of the bodies of its `const`s and `static`s nothing. Nothing
/// lowering reads comes after what is left out within the node holding it, so leaving it out
/// moves nothing that is hashed; what it moves in the item ([`item_ids`], [`item_anchors`]) is
/// hashed with the key.
fn hash_item(item: &SyntaxNode, state: &mut Feed<'_>) {
    // clone: a cursor, which allocates nothing; `cast` takes it by value
    let Some(ast) = ast::Item::cast(item.clone()) else { return hash_full(item, state) };
    let (inner, skipped, items) = match &ast {
        ast::Item::Fn(it) => (element(it.body()), None, None),
        ast::Item::Impl(it) => (element(it.assoc_item_list()), None, None),
        ast::Item::Trait(it) => (element(it.assoc_item_list()), None, None),
        ast::Item::Const(it) => (None, element(it.body()), None),
        ast::Item::Static(it) => (None, element(it.body()), None),
        ast::Item::Module(it) => (None, None, element(it.item_list())),
        _ => (None, None, None),
    };
    if inner.is_none() && skipped.is_none() && items.is_none() {
        return hash_full(item, state);
    }
    (NODE, item.kind() as u16).hash(state);
    for child in item.children_with_tokens() {
        let at = match &child {
            NodeOrToken::Node(node) => node.flat().map(|(_, at)| at),
            NodeOrToken::Token(_) => None,
        };
        match child {
            NodeOrToken::Token(token) => hash_token(&token, state),
            NodeOrToken::Node(node) if at == inner => hash_inner_attrs(&node, state),
            NodeOrToken::Node(_) if at == skipped => SKIPPED.hash(state),
            NodeOrToken::Node(node) if at == items => {
                (NODE, node.kind() as u16).hash(state);
                for child in node.children_with_tokens() {
                    match child {
                        NodeOrToken::Token(token) => hash_token(&token, state),
                        NodeOrToken::Node(node) => hash_item(&node, state),
                    }
                    END.hash(state);
                }
            }
            NodeOrToken::Node(node) => hash_full(&node, state),
        }
        END.hash(state);
    }
}

/// Hashes the inner attributes of a `fn`'s body or an `impl`'s or `trait`'s list of items, what
/// attribute lowering reads of it ([`ast::HasAttrs::inner_attributes_node`]): the tokens,
/// attributes and doc comments before its first statement or item, and after that any inner
/// attribute or doc comment with its offset. A lazy body is hashed whole.
fn hash_inner_attrs(node: &SyntaxNode, state: &mut Feed<'_>) {
    if node.is_lazy() {
        return hash_full(node, state);
    }
    (NODE, node.kind() as u16).hash(state);
    let start = node.text_range().start();
    let mut leading = true;
    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::STMT_LIST => {
                hash_inner_attrs(&node, state)
            }
            NodeOrToken::Node(node) if ast::AnyAttr::can_cast(node.kind()) => {
                let attr = ast::AnyAttr::cast(node).expect("an attribute");
                let node = attr.syntax();
                if !leading {
                    if !attr.kind().is_inner() {
                        continue;
                    }
                    u32::from(node.text_range().start() - start).hash(state);
                }
                hash_full(node, state);
            }
            NodeOrToken::Node(_) => leading = false,
            NodeOrToken::Token(token) if leading => hash_token(&token, state),
            NodeOrToken::Token(_) => {}
        }
    }
    END.hash(state);
}

/// Where `node` is in its file's flat tree.
fn element(node: Option<impl AstNode>) -> Option<u32> {
    node?.syntax().flat().map(|(_, at)| at)
}

const NODE: u8 = 0;
const TOKEN: u8 = 1;
const SKIPPED: u8 = 2;
const END: u8 = 3;

fn hash_full(node: &SyntaxNode, state: &mut Feed<'_>) {
    let (tree, at) = node.flat().expect("a node of a flat tree");
    NODE.hash(state);
    tree.hash_subtree(at, state);
}

fn hash_token(token: &SyntaxToken, state: &mut Feed<'_>) {
    (TOKEN, token.kind() as u16, token.text()).hash(state);
}

/// The tree of one top-level item of a file ([`Ctx::lower_item`](lower::Ctx::lower_item)). It
/// reads the item's syntax, which is its identity, and the crate's `cfg`s: an edit elsewhere in
/// the file, or inside a body of the item, does not run it again.
#[salsa::tracked(returns(ref))]
pub(super) fn item_fragment<'db>(db: &'db Db, item: ItemSource<'db>) -> ItemTree {
    let syntax = item.syntax(db);
    let _p = tracing::info_span!("item_fragment", ?syntax).entered();
    syntax.lower(db, item.krate(db))
}

/// The tree of `file`'s top level's attributes alone: its inner attributes and, for a crate's
/// root, the crate attributes given on the command line.
#[salsa::tracked(returns(ref))]
pub(super) fn top_attrs(db: &Db, file: EditionedFileId, krate: Crate) -> ItemTree {
    let source_file = file.parse(db).tree();
    let root_file_id = krate.root_file_id(db);
    let extra = (file == root_file_id)
        .then(|| parse_extra_crate_attrs(db, krate))
        .flatten()
        .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
    lower::Ctx::new(db, HirFileId::from(file), krate).lower_top(&source_file, extra)
}

/// A file's tree from its parts: `top`, its top level's attributes ([`top_attrs`]), and its
/// items' trees in order, each one's ids moved by where its columns start. One pass over every
/// part, each column allocated once at its length. The attributes are cloned, which allocates
/// for those whose paths or inputs do.
pub(super) fn assemble(top: &ItemTree, fragments: &[&ItemTree]) -> ItemTree {
    macro_rules! column {
        ($field:ident) => {
            Vec::with_capacity(
                top.$field.len() + fragments.iter().map(|it| it.$field.len()).sum::<usize>(),
            )
        };
    }
    let mut top_level = column!(top_level);
    let mut kinds = column!(kinds);
    let mut data = column!(data);
    let mut ast_ids = column!(ast_ids);
    let mut children = column!(children);
    let mut consts = column!(consts);
    let mut enums = column!(enums);
    let mut extern_blocks = column!(extern_blocks);
    let mut extern_crates = column!(extern_crates);
    let mut functions = column!(functions);
    let mut impls = column!(impls);
    let mut macro2s = column!(macro2s);
    let mut macro_calls = column!(macro_calls);
    let mut macro_rules = column!(macro_rules);
    let mut mods = column!(mods);
    let mut statics = column!(statics);
    let mut structs = column!(structs);
    let mut traits = column!(traits);
    let mut type_aliases = column!(type_aliases);
    let mut unions = column!(unions);
    let mut uses = column!(uses);
    let mut use_trees = column!(use_trees);
    let mut use_tree_lists = column!(use_tree_lists);
    let mut ctor_vis = column!(ctor_vis);
    let mut names = column!(names);
    let mut paths = column!(paths);
    let mut vis = column!(vis);
    let mut attr_owners = column!(attr_owners);
    let mut attrs = column!(attrs);
    let mut cfgs = column!(cfgs);

    attrs.extend(top.attrs.iter().cloned());
    cfgs.extend(top.cfgs.iter().cloned());
    for part in fragments {
        let base = Bases {
            item: base(kinds.len()),
            children: base(children.len()),
            use_trees: base(use_trees.len()),
            use_tree_lists: base(use_tree_lists.len()),
            ctor_vis: base(ctor_vis.len()),
            names: base(names.len()),
            paths: base(paths.len()),
            vis: base(vis.len()),
            attrs: base(attrs.len()),
            cfgs: base(cfgs.len()),
        };
        for (&kind, &at) in part.kinds.iter().zip(&part.data) {
            let column = match kind {
                ItemKind::Const => consts.len(),
                ItemKind::Enum => enums.len(),
                ItemKind::ExternBlock => extern_blocks.len(),
                ItemKind::ExternCrate => extern_crates.len(),
                ItemKind::Function => functions.len(),
                ItemKind::Impl => impls.len(),
                ItemKind::Macro2 => macro2s.len(),
                ItemKind::MacroCall => macro_calls.len(),
                ItemKind::MacroRules => macro_rules.len(),
                ItemKind::Mod => mods.len(),
                ItemKind::Static => statics.len(),
                ItemKind::Struct => structs.len(),
                ItemKind::Trait => traits.len(),
                ItemKind::TypeAlias => type_aliases.len(),
                ItemKind::Union => unions.len(),
                ItemKind::Use => uses.len(),
            };
            kinds.push(kind);
            data.push(base(column) + at);
        }
        ast_ids.extend_from_slice(&part.ast_ids);
        top_level.extend(part.top_level.iter().map(|&it| base.item(it)));
        children.extend(part.children.iter().map(|&it| base.item(it)));

        consts.extend(
            part.consts
                .iter()
                .map(|it| Const { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        enums.extend(
            part.enums
                .iter()
                .map(|it| Enum { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        extern_blocks.extend(
            part.extern_blocks
                .iter()
                .map(|it| ExternBlock { children: base.children(it.children) }),
        );
        extern_crates.extend(part.extern_crates.iter().map(|it| ExternCrate {
            name: base.name(it.name),
            alias: base.alias(it.alias),
            visibility: base.vis(it.visibility),
        }));
        functions.extend(
            part.functions.iter().map(|it| Function {
                name: base.name(it.name),
                visibility: base.vis(it.visibility),
            }),
        );
        impls.extend_from_slice(&part.impls);
        macro2s.extend(
            part.macro2s
                .iter()
                .map(|it| Macro2 { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        macro_calls.extend(
            part.macro_calls.iter().map(|it| MacroCall { path: base.path(it.path), ..*it }),
        );
        macro_rules
            .extend(part.macro_rules.iter().map(|it| MacroRules { name: base.name(it.name) }));
        mods.extend(part.mods.iter().map(|it| Mod {
            name: base.name(it.name),
            visibility: base.vis(it.visibility),
            items: match it.items {
                IdRange::NONE => IdRange::NONE,
                items => base.children(items),
            },
        }));
        statics.extend(
            part.statics
                .iter()
                .map(|it| Static { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        structs.extend(part.structs.iter().map(|it| Struct {
            name: base.name(it.name),
            visibility: base.vis(it.visibility),
            value_ns_ctor: match it.value_ns_ctor.len {
                0 => it.value_ns_ctor,
                _ => shift(it.value_ns_ctor, base.ctor_vis),
            },
        }));
        traits.extend(
            part.traits
                .iter()
                .map(|it| Trait { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        type_aliases.extend(
            part.type_aliases.iter().map(|it| TypeAlias {
                name: base.name(it.name),
                visibility: base.vis(it.visibility),
            }),
        );
        unions.extend(
            part.unions
                .iter()
                .map(|it| Union { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        uses.extend(part.uses.iter().map(|it| Use {
            visibility: base.vis(it.visibility),
            use_tree: base.use_tree(it.use_tree),
        }));
        use_trees.extend(part.use_trees.iter().map(|it| UseTree {
            kind: it.kind,
            path: base.path(it.path),
            alias: base.alias(it.alias),
            list: match it.kind {
                UseTree::PREFIXED => shift(it.list, base.use_tree_lists),
                _ => it.list,
            },
        }));
        use_tree_lists.extend(part.use_tree_lists.iter().map(|&it| base.use_tree(it)));
        ctor_vis.extend(part.ctor_vis.iter().map(|&it| base.vis(it)));
        names.extend(part.names.iter().cloned());
        paths.extend(part.paths.iter().cloned());
        vis.extend(part.vis.iter().cloned());
        attr_owners.extend(part.attr_owners.iter().map(|it| AttrOwner {
            owner: base.item(it.owner),
            attrs: shift(it.attrs, base.attrs),
            cfg: match it.cfg {
                AttrOwner::ENABLED => AttrOwner::ENABLED,
                cfg => cfg + base.cfgs,
            },
        }));
        attrs.extend(part.attrs.iter().cloned());
        cfgs.extend(part.cfgs.iter().cloned());
    }
    attr_owners.extend_from_slice(&top.attr_owners);

    ItemTree {
        top_level: top_level.into(),
        kinds: kinds.into(),
        data: data.into(),
        ast_ids: ast_ids.into(),
        children: children.into(),
        consts: consts.into(),
        enums: enums.into(),
        extern_blocks: extern_blocks.into(),
        extern_crates: extern_crates.into(),
        functions: functions.into(),
        impls: impls.into(),
        macro2s: macro2s.into(),
        macro_calls: macro_calls.into(),
        macro_rules: macro_rules.into(),
        mods: mods.into(),
        statics: statics.into(),
        structs: structs.into(),
        traits: traits.into(),
        type_aliases: type_aliases.into(),
        unions: unions.into(),
        uses: uses.into(),
        use_trees: use_trees.into(),
        use_tree_lists: use_tree_lists.into(),
        ctor_vis: ctor_vis.into(),
        names: names.into(),
        paths: paths.into(),
        vis: vis.into(),
        attr_owners: attr_owners.into(),
        attrs: attrs.into(),
        cfgs: cfgs.into(),
    }
}

/// Where one part's columns start in the tree assembled.
struct Bases {
    item: u32,
    children: u32,
    use_trees: u32,
    use_tree_lists: u32,
    ctor_vis: u32,
    names: u32,
    paths: u32,
    vis: u32,
    attrs: u32,
    cfgs: u32,
}

fn base(len: usize) -> u32 {
    match u32::try_from(len) {
        Ok(len) if len < u32::MAX - 4 => len,
        _ => panic!("an item tree with {len} elements in one array: more than `u32` ids name"),
    }
}

fn shift(list: IdRange, by: u32) -> IdRange {
    IdRange { start: list.start + by, len: list.len }
}

impl Bases {
    fn item(&self, it: ModItemId) -> ModItemId {
        ModItemId::new(it.index + self.item)
    }

    fn children(&self, list: IdRange) -> IdRange {
        shift(list, self.children)
    }

    fn use_tree(&self, it: UseTreeId) -> UseTreeId {
        UseTreeId { raw: it.raw + self.use_trees }
    }

    fn name(&self, it: NameId) -> NameId {
        match it {
            NameId::NONE => it,
            NameId { raw } => NameId { raw: raw + self.names },
        }
    }

    fn alias(&self, it: AliasId) -> AliasId {
        match it {
            AliasId::NONE | AliasId::UNDERSCORE => it,
            AliasId { raw } => AliasId { raw: raw + self.names },
        }
    }

    fn path(&self, it: PathId) -> PathId {
        match it {
            PathId::NONE => it,
            PathId { raw } => PathId { raw: raw + self.paths },
        }
    }

    fn vis(&self, it: RawVisibilityId) -> RawVisibilityId {
        match it.is_builtin() {
            true => it,
            false => RawVisibilityId { raw: it.raw + self.vis },
        }
    }
}
