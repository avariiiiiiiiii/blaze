//! The deprecation and stability of a definition: its own attribute, else what it inherits
//! from the definitions it is in; and whether a macro's expansion may use an unstable
//! feature.
//!
//! One [`Index`] per check of a crate, the lazy form of rustc's `stability_index` (one
//! annotator pass filling `FxHashMap`s per `DefId`): it is filled only for the definitions a
//! use names and the nodes above them, each read once. A use then costs one probe of a
//! hash-once table (`hashbrown::HashTable` with Fx, the hash computed once per key and reused
//! for the insert), and a hit allocates nothing. Deprecations and stabilities live in arenas
//! and are named by `u32` ids; those inheriting one share its id, so `same_origin` is an
//! integer compare.
//!
//! Follows `compiler/rustc_passes/src/stability.rs` (`lookup_deprecation_entry`,
//! `inherit_deprecation`, `lookup_stability`, `inherit_stability`),
//! `compiler/rustc_middle/src/middle/stability.rs` (`DeprecationEntry`, `same_origin`) and
//! `compiler/rustc_span/src/lib.rs` (`Span::allows_unstable`) (MIT or Apache-2.0).

use std::hash::{BuildHasher, Hash};

use blaze_mem::Frame;
use hashbrown::HashTable;
use hir::{HirFileId, InFile, view::ModuleSourceView};
use intern::{Symbol, sym};
use rustc_hash::{FxBuildHasher, FxHashMap};
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, Node},
};

use super::{
    attr::{self, Deprecation, Stability},
    def::{Def, attr_parent},
};
use crate::cx::Cx;

/// A deprecation of [`Index::entries`] or a stability of [`Index::stabilities`].
pub(super) type Id = u32;

/// Not looked up yet.
const UNKNOWN: u32 = u32::MAX;
/// Looked up: there is none.
const NONE: u32 = u32::MAX - 1;
/// A definition's stability slot: it is of this crate, whose uses rustc doesn't check.
const LOCAL: u32 = u32::MAX - 2;

fn decode(slot: u32) -> Option<Id> {
    (slot != NONE).then_some(slot)
}

/// `DeprecationEntry`: a deprecation, and whether its attribute is in this crate (only those
/// have an origin to share).
pub(super) struct Entry {
    pub(super) depr: Deprecation,
    pub(super) local: bool,
}

/// What is known of a node or a definition: its deprecation and its stability, each
/// [`UNKNOWN`], [`NONE`] or an id.
#[derive(Clone, Copy)]
struct Slots {
    depr: u32,
    stab: u32,
}

const _: () = assert!(size_of::<Slots>() == 8);

impl Slots {
    const UNKNOWN: Slots = Slots { depr: UNKNOWN, stab: UNKNOWN };
}

/// A definition's node: its file and where in it.
type Key = InFile<SyntaxNodePtr>;

/// A hash-once memo table in the check's frame: 8 bytes of slots per key, one probe a lookup.
struct Memo<'f, K> {
    table: HashTable<(K, Slots), &'f Frame>,
}

impl<'f, K: Hash + Eq + Copy> Memo<'f, K> {
    fn new(frame: &'f Frame) -> Memo<'f, K> {
        Memo { table: HashTable::new_in(frame) }
    }

    fn hash(key: &K) -> u64 {
        FxBuildHasher.hash_one(key)
    }

    fn get(&self, hash: u64, key: &K) -> Slots {
        self.table.find(hash, |(it, _)| it == key).map_or(Slots::UNKNOWN, |(_, it)| *it)
    }

    fn set(&mut self, hash: u64, key: K, update: impl FnOnce(&mut Slots)) {
        match self.table.find_mut(hash, |(it, _)| *it == key) {
            Some((_, slots)) => update(slots),
            None => {
                let mut slots = Slots::UNKNOWN;
                update(&mut slots);
                self.table.insert_unique(hash, (key, slots), |(it, _)| Self::hash(it));
            }
        }
    }
}

/// The index's tables live in the check's frame and go with it; nothing in them is part of a
/// `Found`.
pub(super) struct Index<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    pub(super) entries: Vec<Entry, &'b Frame>,
    pub(super) stabilities: Vec<Stability, &'b Frame>,
    nodes: Memo<'b, Key>,
    /// Of a definition: its deprecation, and its stability when rustc checks it across
    /// crates (public, of another crate with `staged_api`).
    defs: Memo<'b, Def>,
    macros: Memo<'b, hir::Macro>,
    staged: FxHashMap<hir::Crate, bool>,
    enabled: HashTable<(Symbol, bool), &'b Frame>,
}

impl<'a, 'b, 'db> Index<'a, 'b, 'db> {
    pub(super) fn new(cx: &'a Cx<'b, 'db>) -> Self {
        let frame = cx.frame();
        Index {
            cx,
            entries: Vec::new_in(frame),
            stabilities: Vec::new_in(frame),
            nodes: Memo::new(frame),
            defs: Memo::new(frame),
            macros: Memo::new(frame),
            staged: FxHashMap::default(),
            enabled: HashTable::new_in(frame),
        }
    }

    /// Whether `krate` is compiled with `#![feature(staged_api)]`.
    pub(super) fn staged(&mut self, krate: hir::Crate) -> bool {
        let db = self.cx.db;
        *self.staged.entry(krate).or_insert_with(|| krate.is_unstable_feature_enabled(db, &sym::staged_api))
    }

    /// `lookup_deprecation_entry` of `def`.
    pub(super) fn deprecation(&mut self, def: Def) -> Option<Id> {
        let hash = Memo::hash(&def);
        let known = self.defs.get(hash, &def).depr;
        if known != UNKNOWN {
            return decode(known);
        }
        let cx = self.cx;
        let found = match (def.source(cx), def.krate(cx.db)) {
            (Some(node), Some(krate)) => self.deprecation_at(node, krate),
            _ => None,
        };
        self.defs.set(hash, def, |it| it.depr = found.unwrap_or(NONE));
        found
    }

    /// `lookup_deprecation_entry` of the definition of this crate at `ptr`.
    pub(super) fn local_deprecation(&mut self, ptr: Key) -> Option<Id> {
        let known = self.nodes.get(Memo::hash(&ptr), &ptr).depr;
        if known != UNKNOWN {
            return decode(known);
        }
        let node = self.cx.syntax_view(ptr);
        self.deprecation_at(node, self.cx.krate)
    }

    /// `lookup_deprecation_entry` of the definition of `krate` at `node`: up its parents to
    /// the first known or with an attribute, then that answer kept for each node passed.
    fn deprecation_at(&mut self, node: InFile<Node<'db>>, krate: hir::Crate) -> Option<Id> {
        let cx = self.cx;
        let staged = self.staged(krate);
        let cfg = krate.cfg(cx.db);
        let mut passed: SmallVec<[(u64, Key); 8]> = SmallVec::new();
        let mut at = Some(node);
        let found = loop {
            let Some(node) = at else { break NONE };
            let key = node.map(ast::ptr);
            let hash = Memo::hash(&key);
            let known = self.nodes.get(hash, &key).depr;
            if known != UNKNOWN {
                break known;
            }
            passed.push((hash, key));
            if let Some(depr) = own(cx, node, |it| attr::deprecation(it, cfg, staged)) {
                self.entries.push(Entry { depr, local: krate == cx.krate });
                break (self.entries.len() - 1) as u32;
            }
            at = attr_parent(cx, node).or_else(|| crate_root(cx, node, krate));
        };
        for (hash, key) in passed {
            self.nodes.set(hash, key, |it| it.depr = found);
        }
        decode(found)
    }

    /// Whether the uses of `a` in `b` share a deprecation: `same_origin`.
    pub(super) fn same_origin(&self, a: Id, b: Id) -> bool {
        a == b && self.entries[a as usize].local
    }

    /// The stability of `def` rustc checks a use of across crates: of a public definition of
    /// another crate compiled with `staged_api`.
    pub(super) fn foreign_stability(&mut self, def: Def) -> Option<Id> {
        let slot = self.stability_slot(def);
        if slot == LOCAL { None } else { decode(slot) }
    }

    /// Whether `def` is of this crate: then so is every parent segment of a path to it.
    pub(super) fn is_local(&mut self, def: Def) -> bool {
        self.stability_slot(def) == LOCAL
    }

    fn stability_slot(&mut self, def: Def) -> u32 {
        let hash = Memo::hash(&def);
        let known = self.defs.get(hash, &def).stab;
        if known != UNKNOWN {
            return known;
        }
        let cx = self.cx;
        let db = cx.db;
        let slot = match def.krate(db) {
            Some(krate) if krate == cx.krate => LOCAL,
            Some(krate) if self.staged(krate) && def.is_public(db) => {
                def.source(cx).and_then(|node| self.stability_at(node, krate)).unwrap_or(NONE)
            }
            _ => NONE,
        };
        self.defs.set(hash, def, |it| it.stab = slot);
        slot
    }

    /// A macro's own `#[deprecated]` (`SyntaxExtension::deprecation`: nothing inherited).
    pub(super) fn macro_deprecation(&mut self, mac: hir::Macro) -> Option<Id> {
        let hash = Memo::hash(&mac);
        let known = self.macros.get(hash, &mac).depr;
        if known != UNKNOWN {
            return decode(known);
        }
        let cx = self.cx;
        let def = Def::Item(hir::ModuleDef::Macro(mac));
        let mut slot = NONE;
        if let (Some(node), Some(krate)) = (def.source(cx), def.krate(cx.db)) {
            let staged = self.staged(krate);
            let cfg = krate.cfg(cx.db);
            if let Some(depr) = attr::deprecation(node.value, cfg, staged) {
                self.entries.push(Entry { depr, local: false });
                slot = (self.entries.len() - 1) as u32;
            }
        }
        self.macros.set(hash, mac, |it| it.depr = slot);
        decode(slot)
    }

    /// A macro's own stability when it is unstable (`SyntaxExtension::stability`), of a crate
    /// with `staged_api`, this one or another.
    pub(super) fn macro_stability(&mut self, mac: hir::Macro) -> Option<Id> {
        let hash = Memo::hash(&mac);
        let known = self.macros.get(hash, &mac).stab;
        if known != UNKNOWN {
            return decode(known);
        }
        let cx = self.cx;
        let def = Def::Item(hir::ModuleDef::Macro(mac));
        let mut slot = NONE;
        if let (Some(node), Some(krate)) = (def.source(cx), def.krate(cx.db))
            && self.staged(krate)
            && let Some(stability) = attr::stability(node.value, krate.cfg(cx.db))
            && stability.is_unstable()
        {
            self.stabilities.push(stability);
            slot = (self.stabilities.len() - 1) as u32;
        }
        self.macros.set(hash, mac, |it| it.stab = slot);
        decode(slot)
    }

    /// Whether `eval_stability` denies a use at `range` in `file` of what is `stability`:
    /// unstable, its feature and the one implying it not enabled, nor allowed by the macro
    /// the use comes from. Checked cheapest first; nothing is cloned.
    pub(super) fn denied(&mut self, stability: Id, file: HirFileId, range: TextRange) -> bool {
        let Index { cx, stabilities, enabled, .. } = self;
        let cx = *cx;
        let Stability::Unstable { feature, implied_by, .. } = &stabilities[stability as usize] else { return false };
        !(enabled_in(cx, enabled, feature)
            || implied_by.as_ref().is_some_and(|it| enabled_in(cx, enabled, it))
            || allows_unstable(cx, file, range, feature))
    }

    /// `check_stability_and_deprecation`'s test of a macro's `stability`: denied unless its
    /// feature, or the one implying it, is enabled or allowed where the macro is used.
    pub(super) fn macro_denied(&mut self, stability: Id, file: HirFileId, range: TextRange) -> bool {
        let Index { cx, stabilities, enabled, .. } = self;
        let cx = *cx;
        let Stability::Unstable { feature, implied_by, .. } = &stabilities[stability as usize] else { return false };
        let mut allowed = |it: &Symbol| enabled_in(cx, enabled, it) || allows_unstable(cx, file, range, it);
        !(allowed(feature) || implied_by.as_ref().is_some_and(|it| allowed(it)))
    }

    /// The stability of `def` when it is stable with `#[rustc_allowed_through_unstable_modules]`
    /// (`lookup_stability`, of any definition of a staged dependency, public or not).
    pub(super) fn through(&mut self, def: Def) -> Option<Id> {
        let id = match self.foreign_stability(def) {
            Some(id) => id,
            None => {
                let cx = self.cx;
                let db = cx.db;
                let krate = def.krate(db)?;
                if krate == cx.krate || def.is_public(db) || !self.staged(krate) {
                    return None;
                }
                self.stability_at(def.source(cx)?, krate)?
            }
        };
        matches!(&self.stabilities[id as usize], Stability::Stable { allowed_through_unstable_modules: Some(_) })
            .then_some(id)
    }

    /// `lookup_stability` of `def`, of this crate or another, public or not: none unless its
    /// crate is compiled with `staged_api`.
    pub(super) fn stability(&mut self, def: Def) -> Option<Id> {
        let cx = self.cx;
        let krate = def.krate(cx.db)?;
        if !self.staged(krate) {
            return None;
        }
        self.stability_at(def.source(cx)?, krate)
    }

    /// `lookup_stability` of a definition of this crate, staged, at `ptr`.
    pub(super) fn local_stability(&mut self, ptr: Key) -> Option<Id> {
        let known = self.nodes.get(Memo::hash(&ptr), &ptr).stab;
        if known != UNKNOWN {
            return decode(known);
        }
        let node = self.cx.syntax_view(ptr);
        self.stability_at(node, self.cx.krate)
    }

    /// `lookup_stability` of the definition of `krate`, staged, at `node`: up its parents to
    /// the first known or with an attribute, then down again with rustc's inheritance (an
    /// unstable parent's stability, or any of a field's or a variant's).
    fn stability_at(&mut self, node: InFile<Node<'db>>, krate: hir::Crate) -> Option<Id> {
        let cx = self.cx;
        let cfg = krate.cfg(cx.db);
        let mut passed: SmallVec<[(u64, Key, bool); 8]> = SmallVec::new();
        let mut at = Some(node);
        let mut value = loop {
            let Some(node) = at else { break NONE };
            let key = node.map(ast::ptr);
            let hash = Memo::hash(&key);
            let known = self.nodes.get(hash, &key).stab;
            if known != UNKNOWN {
                break known;
            }
            if let Some(stability) = own(cx, node, |it| attr::stability(it, cfg)) {
                self.stabilities.push(stability);
                let id = (self.stabilities.len() - 1) as u32;
                self.nodes.set(hash, key, |it| it.stab = id);
                break id;
            }
            let inherits = matches!(node.value.kind(), SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD | SyntaxKind::VARIANT);
            passed.push((hash, key, inherits));
            at = attr_parent(cx, node).or_else(|| crate_root(cx, node, krate));
        };
        for (hash, key, inherits) in passed.into_iter().rev() {
            if value != NONE && !inherits && !self.stabilities[value as usize].is_unstable() {
                value = NONE;
            }
            self.nodes.set(hash, key, |it| it.stab = value);
        }
        decode(value)
    }
}

/// Whether this crate enables `feature`, kept in `enabled`. A hit clones no `Symbol` (no
/// shared refcount written).
fn enabled_in(cx: &Cx<'_, '_>, enabled: &mut HashTable<(Symbol, bool), &Frame>, feature: &Symbol) -> bool {
    let hash = FxBuildHasher.hash_one(feature);
    if let Some((_, it)) = enabled.find(hash, |(it, _)| it == feature) {
        return *it;
    }
    let it = cx.krate.is_unstable_feature_enabled(cx.db, feature);
    // clone: the miss, once per distinct feature a use asks about; the table keeps its own `Symbol`
    enabled.insert_unique(hash, (feature.clone(), it), |(it, _)| FxBuildHasher.hash_one(it));
    it
}

/// `Span::allows_unstable(feature)` of `range` in `file`: the macro whose expansion its first
/// token comes from is defined with `#[allow_internal_unstable(feature)]`. A builtin macro's
/// expansion is its own, not the one rustc checks with the features its definition allows,
/// so it may use any.
fn allows_unstable(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange, feature: &Symbol) -> bool {
    use hir_expand::MacroDefKind;
    let db = cx.db;
    let ctxt = file.span_map(db).span_for_range(range).ctx;
    let Some(call) = ctxt.outer_expn(db) else { return false };
    let def = hir_expand::MacroCallId::from(call).loc(db).def;
    let node: Node<'_> = match def.kind {
        MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => match def.ast_id() {
            either::Either::Left(id) => id.to_view(db).syntax(),
            either::Either::Right(id) => id.to_view(db).syntax(),
        },
        MacroDefKind::BuiltIn(..)
        | MacroDefKind::BuiltInAttr(..)
        | MacroDefKind::BuiltInDerive(..)
        | MacroDefKind::BuiltInEager(..)
        | MacroDefKind::UnimplementedBuiltIn(..) => return true,
    };
    attr::allows_internal_unstable(node, hir::Crate::from(def.krate).cfg(db), feature)
}

/// What `read` finds in the attributes of the definition at `node`: a `mod foo;`'s, then
/// those its file starts with.
fn own<'db, T>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>, read: impl Fn(Node<'db>) -> Option<T>) -> Option<T> {
    if let Some(found) = read(node.value) {
        return Some(found);
    }
    if node.value.kind() != SyntaxKind::MODULE {
        return None;
    }
    let module = ast::Module::cast(node.value)?;
    if module.item_list().is_some() {
        return None;
    }
    let def = cx.vsema.to_def(module)?;
    match def.definition_source_view(cx.db).value {
        ModuleSourceView::SourceFile(file) => read(file.syntax()),
        _ => None,
    }
}

/// The crate root of `krate`, as the parent of `node` that has no other.
fn crate_root<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>, krate: hir::Crate) -> Option<InFile<Node<'db>>> {
    let root = krate.root_module(cx.db).definition_source_view(cx.db).map(|it| it.node());
    (root != node).then_some(root)
}
