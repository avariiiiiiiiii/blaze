//! A source file's [`ItemTree`], made from the file's last tree where the file was edited.
//!
//! One query per file ([`super::file_item_tree_query`]) and no query per item. Beside salsa,
//! a few files' last trees are kept ([`KEPT`]), with the text they were made of and a key per
//! top-level item: all that lowering the item reads ([`item_keys`]: a fingerprint of its
//! syntax but for its bodies, its ast ids and its span anchors) and where its rows are in the
//! tree ([`Marks`]). An edit keys the new parse's items, hashing only those whose text it
//! touched ([`Unchanged`]), keeps the rows of the unchanged items before and after the changed
//! ones (copied, their ids moved), lowers the changed ones alone and assembles the file's tree
//! from them ([`assemble`]). An edit inside a top-level `fn`'s body, or an
//! `impl`'s or `trait`'s, changes no key and returns the last tree itself. A file's first
//! lowering is [`super::lower_whole`] and makes no keys, so a file never edited never pays for
//! them; its first edit lowers it whole again, keying it. The tree is the one `lower_whole`
//! makes, to the byte, however it was made.
//!
//! A slot is one database's file in one crate ([`Kept`]). One another file took, or one
//! another thread holds, is a whole lowering, never a wrong one: what decides reuse is the
//! keys, the crate's `cfg` set and the top level's attributes, compared.

use std::{
    cell::Cell,
    hash::{Hash, Hasher},
    mem,
    sync::{Arc, Mutex, PoisonError},
};

use base_db::{Crate, Db, EditionedFileId, Nonce};
use blaze_ids::Domain;
use cfg::CfgOptions;
use hir_expand::{HirFileId, span_map::SpanMap};
use span::{AstIdMap, ErasedFileAstId, RealSpanMap};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, Text, TextRange, TextSize,
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

/// The tree of a source file: the kept tree of its last lowering, or one assembled from its
/// rows and the changed items', or a whole lowering ([`self`]).
pub(super) fn file_item_tree(db: &Db, file: EditionedFileId, krate: Crate) -> Arc<ItemTree> {
    let db_nonce = db.nonce_and_revision().0;
    let kept =
        with_slot(db_nonce, file, krate, |it| it.lock().unwrap_or_else(PoisonError::into_inner).take());
    let kept = kept.filter(|it| (it.db, it.file, it.krate) == (db_nonce, file, krate));
    // Read on every path, so the query depends on it even where nothing lowered reads it.
    let cfg = krate.cfg_options(db);
    let (tree, keyed) = match kept {
        None => {
            count_lowered(|| file.parse(db).tree().items().count());
            // alloc: the tree's handle, once a lowering
            (Arc::new(super::lower_whole(db, file.into(), krate)), None)
        }
        Some(Kept { keyed: Some(keyed), .. }) if keyed.cfg == *cfg => {
            let keyed = reuse(db, file, krate, keyed);
            (keyed.tree.clone(), Some(keyed)) // clone: an `Arc`, the tree the slot keeps
        }
        Some(Kept { keyed, .. }) => {
            let keyed = lower_keyed(db, file, krate, cfg, keyed);
            (keyed.tree.clone(), Some(keyed)) // clone: an `Arc`, the tree the slot keeps
        }
    };
    let replaced = with_slot(db_nonce, file, krate, |it| {
        (it.lock().unwrap_or_else(PoisonError::into_inner))
            .replace(Kept { db: db_nonce, file, krate, keyed })
    });
    // Dropped after the lock: a tree let go of may be a whole file's columns.
    drop(replaced);
    tree
}

/// How many files' last trees are kept: the files being edited, and the last few others.
const SLOTS: usize = 128;

type Slot = Mutex<Option<Kept>>;

/// The files' last trees, direct-mapped by database, file and crate: a lookup is one hash, one
/// index and one short lock, and the slot is empty while its tree is worked from.
#[cfg(not(test))]
static KEPT: [Slot; SLOTS] = [const { Mutex::new(None) }; SLOTS];

#[cfg(test)]
thread_local! {
    /// A test thread's own slots: a test that counts what an edit lowers sees no other test's
    /// file take its slot between its edits.
    static KEPT: [Slot; SLOTS] = const { [const { Mutex::new(None) }; SLOTS] };
}

fn with_slot<R>(db: Nonce, file: EditionedFileId, krate: Crate, f: impl FnOnce(&Slot) -> R) -> R {
    let mut hasher = rustc_hash::FxHasher::default();
    (db, file, krate).hash(&mut hasher);
    // The multiply leaves its best bits at the top.
    let at = (hasher.finish() >> (64 - SLOTS.trailing_zeros())) as usize;
    #[cfg(not(test))]
    return f(&KEPT[at]);
    #[cfg(test)]
    return KEPT.with(|kept| f(&kept[at]));
}

/// A file's last lowering in a crate, of a database: the ids are that database's.
struct Kept {
    db: Nonce,
    file: EditionedFileId,
    krate: Crate,
    /// None after a first lowering, which makes no keys.
    keyed: Option<Keyed>,
}

/// A file's last tree and what its rows were lowered from.
struct Keyed {
    /// The crate's `cfg` set the tree was lowered under: lowering an item's attributes reads it.
    cfg: CfgOptions,
    tree: Arc<ItemTree>,
    /// The file's text the keys were made of: what the next edit's text is compared with.
    text: Text,
    /// Per top-level item of the file, in order: what lowering it reads.
    keys: Keys,
    /// Per top-level item, where its rows begin in each of `tree`'s columns; then where the
    /// last item's end. One more than `keys.items`.
    marks: Vec<Marks>,
}

/// What lowering reads of each top-level item of a file ([`item_keys`]): its syntax, as a
/// fingerprint, and the ast ids and span anchors within it, as they are, all items' in two
/// arrays one after the other.
#[derive(Default)]
struct Keys {
    items: Vec<ItemKey>,
    ids: Vec<[u32; 3]>,
    anchors: Vec<[u32; 2]>,
}

#[derive(Clone, Copy)]
struct ItemKey {
    /// Where the item is in the text it was keyed in.
    range: TextRange,
    /// The fingerprint of the item's syntax but for what lowering does not read ([`hash_item`]).
    syntax: u128,
    /// Where the item's ids end in [`Keys::ids`], and its anchors in [`Keys::anchors`].
    ids_end: u32,
    anchors_end: u32,
}

impl Keys {
    fn clear(&mut self) {
        self.items.clear();
        self.ids.clear();
        self.anchors.clear();
    }

    fn ids(&self, item: usize) -> &[[u32; 3]] {
        let from = item.checked_sub(1).map_or(0, |it| self.items[it].ids_end);
        &self.ids[from as usize..self.items[item].ids_end as usize]
    }

    fn anchors(&self, item: usize) -> &[[u32; 2]] {
        let from = item.checked_sub(1).map_or(0, |it| self.items[it].anchors_end);
        &self.anchors[from as usize..self.items[item].anchors_end as usize]
    }

    /// Whether lowering `item` of these keys reads what lowering `theirs` of `other` does.
    fn same(&self, item: usize, other: &Keys, theirs: usize) -> bool {
        self.items[item].syntax == other.items[theirs].syntax
            && self.ids(item) == other.ids(theirs)
            && self.anchors(item) == other.anchors(theirs)
    }
}

/// The text of `source_file`'s tree: the file's, shared.
fn text_of(source_file: &ast::SourceFile) -> &Text {
    source_file.syntax().flat().expect("a source file's flat tree").0.source()
}

/// The tree of an edited file from its kept one: the rows of the items whose keys are the
/// same before and after the edit's, kept, and the items between lowered again.
fn reuse(db: &Db, file: EditionedFileId, krate: Crate, mut keyed: Keyed) -> Keyed {
    let source_file = file.parse(db).tree();
    let mut scratch = SCRATCH.take();
    scratch.keys.clear();
    item_keys(db, file, &source_file, &mut scratch, Some(&keyed));
    keyed.text = text_of(&source_file).clone(); // clone: one count of the file's text
    let (old, new) = (&keyed.keys, &scratch.keys);
    let (old_items, new_items) = (old.items.len(), new.items.len());
    let before = (0..old_items.min(new_items)).take_while(|&it| old.same(it, new, it)).count();
    let after = (0..(old_items - before).min(new_items - before))
        .take_while(|&it| old.same(old_items - 1 - it, new, new_items - 1 - it))
        .count();
    let top = top_attrs(db, file, krate);
    if before == old_items
        && before == new_items
        && same_top(&keyed.tree, &keyed.marks[0], &top)
    {
        // The tree and its marks stand; the items' ranges moved with the edit.
        mem::swap(&mut keyed.keys, &mut scratch.keys);
        release(scratch);
        return keyed;
    }

    let changed = new_items - before - after;
    count_lowered(|| changed);
    let fresh: Vec<ItemTree> = (source_file.items().skip(before).take(changed))
        .map(|item| lower::Ctx::new(db, file.into(), krate).lower_item(&item))
        .collect(); // alloc: once an edit, the list of the items it changed
    let fresh_marks: Vec<[Marks; 2]> =
        fresh.iter().map(|it| [Marks::default(), Marks::ends(it)]).collect(); // alloc: as `fresh`
    let tree = {
        let (kept, marks) = (&*keyed.tree, &keyed.marks[..]);
        // alloc: once an edit, the list of its parts
        let mut parts = Vec::with_capacity(fresh.len() + 2);
        if before > 0 {
            parts.push(Rows { tree: kept, marks: &marks[..=before] });
        }
        parts.extend(fresh.iter().zip(&fresh_marks).map(|(tree, marks)| Rows { tree, marks }));
        if after > 0 {
            parts.push(Rows { tree: kept, marks: &marks[old_items - after..] });
        }
        scratch.marks.clear();
        assemble(&top, &parts, &mut scratch.marks)
    };
    keyed.tree = Arc::new(tree); // alloc: the tree's handle, once an edit that changed it
    mem::swap(&mut keyed.keys, &mut scratch.keys);
    mem::swap(&mut keyed.marks, &mut scratch.marks);
    release(scratch);
    keyed
}

/// Whether `top`, the top level's attributes lowered again, are those `old` was made with:
/// `old`'s first attributes and `cfg`s, up to where its first item's (`first`) begin, and its
/// top level's owner.
fn same_top(old: &ItemTree, first: &Marks, top: &ItemTree) -> bool {
    let top_owner = |tree: &ItemTree| {
        tree.attr_owners.last().copied().filter(|it| it.owner == ModItemId::TOP_OWNER)
    };
    let (attrs, cfgs) = (top.attrs.len(), top.cfgs.len());
    first.attrs as usize == attrs
        && first.cfgs as usize == cfgs
        && old.attrs[..attrs] == top.attrs[..]
        && old.cfgs[..cfgs] == top.cfgs[..]
        && top_owner(old) == top_owner(top)
}

/// The tree of a file lowered whole, its items keyed: a file's first edit, or the first under
/// a new `cfg` set. `last` lends its arrays.
fn lower_keyed(
    db: &Db,
    file: EditionedFileId,
    krate: Crate,
    cfg: &CfgOptions,
    last: Option<Keyed>,
) -> Keyed {
    let source_file = file.parse(db).tree();
    let mut scratch = SCRATCH.take();
    if let Some(last) = last {
        (scratch.keys, scratch.marks) = (last.keys, last.marks);
    }
    scratch.keys.clear();
    scratch.marks.clear();
    item_keys(db, file, &source_file, &mut scratch, None);
    count_lowered(|| scratch.keys.items.len());

    let root_file_id = krate.root_file_id(db);
    let extra = (file == root_file_id)
        .then(|| parse_extra_crate_attrs(db, krate))
        .flatten()
        .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
    let mut ctx = lower::Ctx::new(db, file.into(), krate);
    ctx.lower_top_attrs(&source_file, extra);
    let tree = ctx.lower_module_items_marked(&source_file, &mut scratch.marks);
    let keyed = Keyed {
        cfg: cfg.clone(), // clone: once a file's first edit or a new `cfg` set
        tree: Arc::new(tree), // alloc: the tree's handle, once a lowering
        text: text_of(&source_file).clone(), // clone: one count of the file's text
        keys: mem::take(&mut scratch.keys),
        marks: mem::take(&mut scratch.marks),
    };
    release(scratch);
    keyed
}

/// The tree of `file`'s top level's attributes alone: its inner attributes and, for a crate's
/// root, the crate attributes given on the command line.
pub(super) fn top_attrs(db: &Db, file: EditionedFileId, krate: Crate) -> ItemTree {
    let source_file = file.parse(db).tree();
    let root_file_id = krate.root_file_id(db);
    let extra = (file == root_file_id)
        .then(|| parse_extra_crate_attrs(db, krate))
        .flatten()
        .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
    lower::Ctx::new(db, HirFileId::from(file), krate).lower_top(&source_file, extra)
}

/// The tree of one top-level item of `file` alone, as an edit lowers a changed item.
#[cfg(test)]
pub(super) fn item_alone(
    db: &Db,
    file: EditionedFileId,
    krate: Crate,
    item: &ast::Item,
) -> ItemTree {
    lower::Ctx::new(db, file.into(), krate).lower_item(item)
}

#[cfg(test)]
thread_local! {
    /// How many top-level items this thread lowered, whole or alone, for the tests that count
    /// what an edit lowers.
    pub(super) static LOWERED: Cell<usize> = const { Cell::new(0) };
    /// How many top-level items' syntax this thread hashed ([`item_keys`]).
    pub(super) static HASHED: Cell<usize> = const { Cell::new(0) };
}

fn count_lowered(items: impl FnOnce() -> usize) {
    #[cfg(test)]
    LOWERED.set(LOWERED.get() + items());
    #[cfg(not(test))]
    let _ = items;
}

fn count_hashed() {
    #[cfg(test)]
    HASHED.set(HASHED.get() + 1);
}

/// The domain of an item's key.
static ITEM: Domain = Domain::new("blaze v2 item syntax 2");

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

/// The arrays [`item_keys`] and [`reuse`] fill, empty between runs and keeping their capacity,
/// so a worker grows none again once it has keyed a file as big; an edit swaps its keys and
/// marks with the slot's, so the slot's last arrays are the next edit's. A scratch that grew
/// past [`SCRATCH_IDS`] (a generated file) is dropped, so a worker keeps a bounded amount.
#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    keys: Keys,
    marks: Vec<Marks>,
}

thread_local! {
    static SCRATCH: Cell<Scratch> = Cell::new(Scratch::default());
}

/// About 200 KiB of a [`Scratch`]'s arrays.
const SCRATCH_IDS: usize = 1 << 14;

fn release(mut scratch: Scratch) {
    scratch.keys.clear();
    scratch.marks.clear();
    let keys = &scratch.keys;
    if keys.ids.capacity() <= SCRATCH_IDS && keys.items.capacity() <= SCRATCH_IDS {
        SCRATCH.set(scratch);
    }
}

/// Pushes onto `scratch.keys` what lowering reads of each of `source_file`'s top-level items,
/// in order: the fingerprint of the item's syntax but for what lowering does not read
/// ([`hash_item`]), the ast ids of the items it is made of and the span anchors within it, all
/// from where it starts, so an item keeps its key wherever it moves in the file. An item whose
/// text `last`'s also had, unchanged ([`Unchanged`]), takes the fingerprint it had there, so an
/// edit hashes only the items it touched. Two walks of the items, which allocate nothing once
/// the scratch is grown.
fn item_keys(
    db: &Db,
    file: EditionedFileId,
    source_file: &ast::SourceFile,
    scratch: &mut Scratch,
    last: Option<&Keyed>,
) {
    let hir_file = HirFileId::from(file);
    let ast_id_map = hir_file.ast_id_map(db);
    let SpanMap::RealSpanMap(span_map) = hir_file.span_map(db) else {
        unreachable!("a source file's span map is a real file's")
    };
    let Scratch { bytes, keys, .. } = scratch;
    for item in source_file.items() {
        let range = item.syntax().text_range();
        keys.items.push(ItemKey { range, syntax: 0, ids_end: 0, anchors_end: 0 });
    }
    let unchanged = last.map(|last| Unchanged::new(last, text_of(source_file), &keys.items));
    for (at, item) in source_file.items().enumerate() {
        let range = keys.items[at].range;
        let ids_from = keys.ids.len();
        item_ids(&item, ast_id_map, range.start(), &mut keys.ids);
        item_anchors(span_map, range, &keys.ids[ids_from..], &mut keys.anchors);
        let kept = last.zip(unchanged.as_ref()).and_then(|(last, unchanged)| {
            unchanged.kept(&last.keys.items, &keys.items, at).map(|it| it.syntax)
        });
        let syntax = kept.unwrap_or_else(|| {
            count_hashed();
            bytes.clear();
            let mut feed = Feed(&mut *bytes);
            hash_item(item.syntax(), &mut feed);
            feed.fingerprint()
        });
        keys.items[at] =
            ItemKey { range, syntax, ids_end: id(keys.ids.len()), anchors_end: id(keys.anchors.len()) };
    }
}

fn id(len: usize) -> u32 {
    u32::try_from(len).expect("a file of more than `u32` ast ids")
}

/// What an edit left of a file's text: the longest prefix and suffix its text before and after
/// share. The parse of a file is a function of its text ([`syntax::Parse::reparse_to`]), and
/// the parser reads a top-level item from the item list's state, its tokens and at most three
/// tokens after the one it is at; the lexer starts a token at an item's start the same way
/// whatever came before. So an item with the same text at the same place in the suffix, which
/// runs to the end, is the same syntax; and so is one in the prefix with two whole items after
/// it there, which hold the three tokens the parser may look at past it.
struct Unchanged {
    /// The new parse's leading items that are in the prefix with two items after them.
    prefix_items: usize,
    /// Where the suffix begins in the new text.
    suffix_from: TextSize,
    /// The old text's length less the new's: where an item of the suffix was, from where it is.
    shift: i64,
}

impl Unchanged {
    fn new(last: &Keyed, text: &Text, items: &[ItemKey]) -> Unchanged {
        let (old, new) = (last.text.as_bytes(), text.as_bytes());
        let (prefix, suffix) = common_ends(old, new);
        let in_prefix = items.iter().take_while(|it| usize::from(it.range.end()) <= prefix).count();
        Unchanged {
            prefix_items: in_prefix.saturating_sub(2),
            suffix_from: TextSize::new(id(new.len() - suffix)),
            shift: old.len() as i64 - new.len() as i64,
        }
    }

    /// The key `old` had for item `at` of `new`, if the item is one this edit left unchanged.
    fn kept<'a>(&self, old: &'a [ItemKey], new: &[ItemKey], at: usize) -> Option<&'a ItemKey> {
        let range = new[at].range;
        let (was, start) = if at < self.prefix_items {
            (old.get(at)?, i64::from(u32::from(range.start())))
        } else if range.start() >= self.suffix_from {
            let from_end = new.len() - at;
            let was = old.get(old.len().checked_sub(from_end)?)?;
            (was, i64::from(u32::from(range.start())) + self.shift)
        } else {
            return None;
        };
        let same = i64::from(u32::from(was.range.start())) == start && was.range.len() == range.len();
        same.then_some(was)
    }
}

/// The lengths of the longest common prefix of `old` and `new` and of their longest common
/// suffix in what is left: one `memcmp` per 4 KiB at memory bandwidth, then per 64 bytes and
/// per byte to find where they differ.
fn common_ends(old: &[u8], new: &[u8]) -> (usize, usize) {
    let len = old.len().min(new.len());
    let mut prefix = 0;
    for step in [4096, 64, 1] {
        while prefix + step <= len && old[prefix..prefix + step] == new[prefix..prefix + step] {
            prefix += step;
        }
    }
    let (most, a, b) = (len - prefix, old.len(), new.len());
    let mut suffix = 0;
    for step in [4096, 64, 1] {
        while suffix + step <= most
            && old[a - suffix - step..a - suffix] == new[b - suffix - step..b - suffix]
        {
            suffix += step;
        }
    }
    (prefix, suffix)
}

macro_rules! marks {
    ($($column:ident),* $(,)?) => {
        /// Where a run of rows begins (or ends) in each column of a tree: the run of one or
        /// more top-level items. `kinds` is also the per-item `data` and `ast_ids`'.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub(super) struct Marks {
            $(pub(super) $column: u32,)*
        }

        impl Marks {
            /// Where the rows of `tree`'s items end: its columns' lengths, but for its top
            /// level's attribute owner, which is no item's.
            fn ends(tree: &ItemTree) -> Marks {
                let top =
                    tree.attr_owners.last().is_some_and(|it| it.owner == ModItemId::TOP_OWNER);
                let mut ends = Marks { $($column: base(tree.$column.len()),)* };
                ends.attr_owners -= top as u32;
                ends
            }

            /// This mark of a run that began at `from`, where the run begins at `to`.
            fn moved(&self, from: &Marks, to: &Marks) -> Marks {
                Marks { $($column: to.$column + (self.$column - from.$column),)* }
            }
        }

        /// The marks of the columns being assembled, named as the tree's.
        macro_rules! marks_of {
            () => { Marks { $($column: base($column.len()),)* } };
        }
    };
}

marks!(
    top_level,
    kinds,
    children,
    consts,
    enums,
    extern_blocks,
    extern_crates,
    functions,
    impls,
    macro2s,
    macro_calls,
    macro_rules,
    mods,
    statics,
    structs,
    traits,
    type_aliases,
    unions,
    uses,
    use_trees,
    use_tree_lists,
    ctor_vis,
    names,
    paths,
    vis,
    attr_owners,
    attrs,
    cfgs,
);

/// The rows of a run of top-level items of `tree`: `marks` are where each item's begin, then
/// where the last one's end.
pub(super) struct Rows<'a> {
    pub(super) tree: &'a ItemTree,
    pub(super) marks: &'a [Marks],
}

impl Rows<'_> {
    fn from(&self) -> &Marks {
        &self.marks[0]
    }

    fn to(&self) -> &Marks {
        self.marks.last().expect("a run of rows ends")
    }
}

/// A file's tree from its parts: `top`, its top level's attributes ([`top_attrs`]), and runs
/// of its items' rows in order, each run's ids moved by where its rows go. One pass over every
/// part, each column allocated once at its length. Pushes onto `marks` where each item's rows
/// begin in the tree made, then where the last one's end. The attributes are cloned, which
/// allocates for those whose paths or inputs do.
pub(super) fn assemble(top: &ItemTree, parts: &[Rows<'_>], marks: &mut Vec<Marks>) -> ItemTree {
    macro_rules! column {
        ($field:ident) => {
            column!($field, $field)
        };
        ($field:ident, $by:ident) => {
            // alloc: the tree's column, once a tree at its length
            Vec::with_capacity(
                top.$field.len()
                    + parts.iter().map(|it| (it.to().$by - it.from().$by) as usize).sum::<usize>(),
            )
        };
    }
    let mut top_level = column!(top_level);
    let mut kinds = column!(kinds);
    let mut data = column!(data, kinds);
    let mut ast_ids = column!(ast_ids, kinds);
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
    for part in parts {
        let (tree, from, to) = (part.tree, part.from(), part.to());
        macro_rules! rows {
            ($field:ident) => {
                rows!($field, $field)
            };
            ($field:ident, $by:ident) => {
                &tree.$field[from.$by as usize..to.$by as usize]
            };
        }
        let now = marks_of!();
        marks.extend(part.marks[..part.marks.len() - 1].iter().map(|it| it.moved(from, &now)));
        // An id `i` of the part's is `i - from` in its run, so `i - from + now` in the tree.
        let by = |now: u32, from: u32| now.wrapping_sub(from);
        let base = Bases {
            item: by(now.kinds, from.kinds),
            children: by(now.children, from.children),
            use_trees: by(now.use_trees, from.use_trees),
            use_tree_lists: by(now.use_tree_lists, from.use_tree_lists),
            ctor_vis: by(now.ctor_vis, from.ctor_vis),
            names: by(now.names, from.names),
            paths: by(now.paths, from.paths),
            vis: by(now.vis, from.vis),
            attrs: by(now.attrs, from.attrs),
            cfgs: by(now.cfgs, from.cfgs),
        };
        for (&kind, &at) in rows!(kinds).iter().zip(rows!(data, kinds)) {
            let (column, origin) = match kind {
                ItemKind::Const => (now.consts, from.consts),
                ItemKind::Enum => (now.enums, from.enums),
                ItemKind::ExternBlock => (now.extern_blocks, from.extern_blocks),
                ItemKind::ExternCrate => (now.extern_crates, from.extern_crates),
                ItemKind::Function => (now.functions, from.functions),
                ItemKind::Impl => (now.impls, from.impls),
                ItemKind::Macro2 => (now.macro2s, from.macro2s),
                ItemKind::MacroCall => (now.macro_calls, from.macro_calls),
                ItemKind::MacroRules => (now.macro_rules, from.macro_rules),
                ItemKind::Mod => (now.mods, from.mods),
                ItemKind::Static => (now.statics, from.statics),
                ItemKind::Struct => (now.structs, from.structs),
                ItemKind::Trait => (now.traits, from.traits),
                ItemKind::TypeAlias => (now.type_aliases, from.type_aliases),
                ItemKind::Union => (now.unions, from.unions),
                ItemKind::Use => (now.uses, from.uses),
            };
            kinds.push(kind);
            data.push(by(column, origin).wrapping_add(at));
        }
        ast_ids.extend_from_slice(rows!(ast_ids, kinds));
        top_level.extend(rows!(top_level).iter().map(|&it| base.item(it)));
        children.extend(rows!(children).iter().map(|&it| base.item(it)));

        consts.extend(
            rows!(consts)
                .iter()
                .map(|it| Const { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        enums.extend(
            rows!(enums)
                .iter()
                .map(|it| Enum { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        extern_blocks.extend(
            rows!(extern_blocks)
                .iter()
                .map(|it| ExternBlock { children: base.children(it.children) }),
        );
        extern_crates.extend(rows!(extern_crates).iter().map(|it| ExternCrate {
            name: base.name(it.name),
            alias: base.alias(it.alias),
            visibility: base.vis(it.visibility),
        }));
        functions.extend(
            rows!(functions).iter().map(|it| Function {
                name: base.name(it.name),
                visibility: base.vis(it.visibility),
            }),
        );
        impls.extend_from_slice(rows!(impls));
        macro2s.extend(
            rows!(macro2s)
                .iter()
                .map(|it| Macro2 { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        macro_calls.extend(
            rows!(macro_calls).iter().map(|it| MacroCall { path: base.path(it.path), ..*it }),
        );
        macro_rules
            .extend(rows!(macro_rules).iter().map(|it| MacroRules { name: base.name(it.name) }));
        mods.extend(rows!(mods).iter().map(|it| Mod {
            name: base.name(it.name),
            visibility: base.vis(it.visibility),
            items: match it.items {
                IdRange::NONE => IdRange::NONE,
                items => base.children(items),
            },
        }));
        statics.extend(
            rows!(statics)
                .iter()
                .map(|it| Static { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        structs.extend(rows!(structs).iter().map(|it| Struct {
            name: base.name(it.name),
            visibility: base.vis(it.visibility),
            value_ns_ctor: match it.value_ns_ctor.len {
                0 => it.value_ns_ctor,
                _ => shift(it.value_ns_ctor, base.ctor_vis),
            },
        }));
        traits.extend(
            rows!(traits)
                .iter()
                .map(|it| Trait { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        type_aliases.extend(
            rows!(type_aliases).iter().map(|it| TypeAlias {
                name: base.name(it.name),
                visibility: base.vis(it.visibility),
            }),
        );
        unions.extend(
            rows!(unions)
                .iter()
                .map(|it| Union { name: base.name(it.name), visibility: base.vis(it.visibility) }),
        );
        uses.extend(rows!(uses).iter().map(|it| Use {
            visibility: base.vis(it.visibility),
            use_tree: base.use_tree(it.use_tree),
        }));
        use_trees.extend(rows!(use_trees).iter().map(|it| UseTree {
            kind: it.kind,
            path: base.path(it.path),
            alias: base.alias(it.alias),
            list: match it.kind {
                UseTree::PREFIXED => shift(it.list, base.use_tree_lists),
                _ => it.list,
            },
        }));
        use_tree_lists.extend(rows!(use_tree_lists).iter().map(|&it| base.use_tree(it)));
        ctor_vis.extend(rows!(ctor_vis).iter().map(|&it| base.vis(it)));
        names.extend(rows!(names).iter().cloned());
        paths.extend(rows!(paths).iter().cloned());
        vis.extend(rows!(vis).iter().cloned());
        attr_owners.extend(rows!(attr_owners).iter().map(|it| AttrOwner {
            owner: base.item(it.owner),
            attrs: shift(it.attrs, base.attrs),
            cfg: match it.cfg {
                AttrOwner::ENABLED => AttrOwner::ENABLED,
                cfg => cfg.wrapping_add(base.cfgs),
            },
        }));
        attrs.extend(rows!(attrs).iter().cloned());
        cfgs.extend(rows!(cfgs).iter().cloned());
    }
    marks.push(marks_of!());
    attr_owners.extend_from_slice(&top.attr_owners);
    let attr_index = super::attr_index(&attr_owners, kinds.len());

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
        attr_index,
        attrs: attrs.into(),
        cfgs: cfgs.into(),
    }
}

/// How one part's ids move into the tree assembled: added, wrapping, to each id of the part
/// that is not a sentinel ([`assemble`]).
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
    IdRange { start: list.start.wrapping_add(by), len: list.len }
}

impl Bases {
    fn item(&self, it: ModItemId) -> ModItemId {
        ModItemId::new(it.index.wrapping_add(self.item))
    }

    fn children(&self, list: IdRange) -> IdRange {
        shift(list, self.children)
    }

    fn use_tree(&self, it: UseTreeId) -> UseTreeId {
        UseTreeId { raw: it.raw.wrapping_add(self.use_trees) }
    }

    fn name(&self, it: NameId) -> NameId {
        match it {
            NameId::NONE => it,
            NameId { raw } => NameId { raw: raw.wrapping_add(self.names) },
        }
    }

    fn alias(&self, it: AliasId) -> AliasId {
        match it {
            AliasId::NONE | AliasId::UNDERSCORE => it,
            AliasId { raw } => AliasId { raw: raw.wrapping_add(self.names) },
        }
    }

    fn path(&self, it: PathId) -> PathId {
        match it {
            PathId::NONE => it,
            PathId { raw } => PathId { raw: raw.wrapping_add(self.paths) },
        }
    }

    fn vis(&self, it: RawVisibilityId) -> RawVisibilityId {
        match it.is_builtin() {
            true => it,
            false => RawVisibilityId { raw: it.raw.wrapping_add(self.vis) },
        }
    }
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
const KEPT_HASH: u8 = 4;

/// Subtrees of this much text keep their hash in their tree ([`syntax::FlatTree::subtree_hash`]),
/// across edits that don't touch them.
const KEPT_TEXT: u32 = 4096;

fn hash_full(node: &SyntaxNode, state: &mut Feed<'_>) {
    let (tree, at) = node.flat().expect("a node of a flat tree");
    if u32::from(node.text_range().len()) >= KEPT_TEXT {
        return (KEPT_HASH, tree.subtree_hash(at)).hash(state);
    }
    NODE.hash(state);
    tree.hash_subtree(at, state);
}

fn hash_token(token: &SyntaxToken, state: &mut Feed<'_>) {
    (TOKEN, token.kind() as u16, token.text()).hash(state);
}
