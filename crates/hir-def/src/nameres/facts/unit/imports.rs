//! A unit's imports and the worklist of their fixed point: the old collector's `Imports`, over
//! owned [`ImportOp`]s so a unit's state is a plain query value carried from round to round.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, hash_map::Entry},
    iter, mem,
};

use hir_expand::name::Name;
use rustc_hash::FxHashMap;

use crate::{
    ModuleId,
    item_scope::GlobId,
    item_tree::ImportKind,
    nameres::facts::ops::ImportOp,
    per_ns::PerNs,
    visibility::Visibility,
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum PartialResolvedImport {
    /// None of any namespaces is resolved
    Unresolved,
    /// One of namespaces is resolved
    Indeterminate(PerNs),
    /// All namespaces are resolved, OR it comes from other crate
    Resolved(PerNs),
}

impl PartialResolvedImport {
    pub(super) fn namespaces(self) -> PerNs {
        match self {
            PartialResolvedImport::Unresolved => PerNs::none(),
            PartialResolvedImport::Indeterminate(ns) | PartialResolvedImport::Resolved(ns) => ns,
        }
    }
}

/// One import of the unit and what its last attempt found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Directive {
    pub(super) op: ImportOp,
    pub(super) status: PartialResolvedImport,
}

/// Bits of a namespace mask: which of a name's namespaces a write touched, or a path read.
pub(super) const TYPES: u8 = 1;
pub(super) const VALUES: u8 = 2;
pub(super) const MACROS: u8 = 4;
pub(super) const ALL_NS: u8 = TYPES | VALUES | MACROS;

pub(super) fn written_ns(defs: &PerNs) -> u8 {
    defs.types.is_some() as u8 * TYPES
        | defs.values.is_some() as u8 * VALUES
        | defs.macros.is_some() as u8 * MACROS
}

/// A part of the state an import's resolution read that this unit can change. A read of a
/// module the unit does not hold waits on nothing: that module's scope is final for the round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum WaitOn {
    /// A name in the scope of a module of this unit.
    Slot(ModuleId, Name),
    /// What a path's first segment finds besides module scopes: a legacy macro, the extern
    /// prelude, the `macro_use` prelude.
    Env(Name),
}

/// One import waiting on one key, linked to the key's next waiter. 16 bytes.
#[derive(Clone, Copy, Debug)]
struct WaitEdge {
    import: u32,
    /// The attempt of `import` that waits; a later attempt makes the edge stale.
    generation: u32,
    next: u32,
    ns: u8,
}

const _: () = assert!(size_of::<WaitEdge>() == 16);

pub(super) const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImportList {
    Unresolved,
    Indeterminate,
    Resolved,
}

/// Where a pass of `resolve_imports` is. A woken import whose key is at or past the phase's
/// mark runs in this phase, as the loop over every import would have reached it after the
/// write; one before the mark runs in the next pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// Outside a pass: what is woken runs in the next one.
    Idle,
    /// Retrying unresolved imports, by index, from the given one on.
    Unresolved(u32),
    /// Retrying indeterminate imports, by the order they became so, from the given one on.
    Indeterminate(u32),
}

const PENDING: u8 = 1;
const DEFERRED: u8 = 2;

/// Every import of the unit, and the queues of the fixed point over them. An import is a `u32`
/// index into the columns; it moves between lists by its `list` entry only.
#[derive(Clone, Debug)]
pub(super) struct Imports {
    pub(super) directives: Vec<Directive>,
    pub(super) list: Vec<ImportList>,
    /// Bumped at each attempt to resolve the import.
    generation: Vec<u32>,
    /// `PENDING` and `DEFERRED` bits.
    queued: Vec<u8>,
    /// Position in `indeterminate`, once the import was indeterminate.
    pub(super) seq: Vec<u32>,
    /// The imports that were indeterminate, in the order they became so, with what each last
    /// resolved to. One resolved since stays where it is; `list` says it is done.
    pub(super) indeterminate: Vec<(u32, PerNs)>,
    /// Plain imports by the module and the name they bind, in index order: head and tail of a
    /// list linked through `next_named`.
    named: FxHashMap<(ModuleId, Name), (u32, u32)>,
    next_named: Vec<u32>,
    pub(super) phase: Phase,
    pending_unresolved: BinaryHeap<Reverse<u32>>,
    deferred_unresolved: Vec<u32>,
    pending_indeterminate: BinaryHeap<Reverse<u32>>,
    deferred_indeterminate: Vec<u32>,
    /// Head of each key's list of waiting imports in `edges`.
    waits: FxHashMap<WaitOn, u32>,
    edges: Vec<WaitEdge>,
    /// Head of the edges unlinked from every key, reused before `edges` grows.
    free_edge: u32,
    /// The imports, with the attempt, that read something without a key (a module's children,
    /// all its declarations): any write wakes them.
    any_waiters: Vec<(u32, u32)>,
    /// Scratch: the keys of the attempt being registered.
    pub(super) keys: Vec<(WaitOn, u8)>,
    /// Scratch: whether the attempt being registered read something without a key.
    pub(super) keys_any: bool,
    /// Scratch: the imports a write woke.
    woken: Vec<u32>,
}

impl Default for Imports {
    fn default() -> Self {
        Imports::new()
    }
}

impl Imports {
    pub(super) fn new() -> Self {
        Imports {
            directives: Vec::new(),
            list: Vec::new(),
            generation: Vec::new(),
            queued: Vec::new(),
            seq: Vec::new(),
            indeterminate: Vec::new(),
            named: FxHashMap::default(),
            next_named: Vec::new(),
            phase: Phase::Idle,
            pending_unresolved: BinaryHeap::new(),
            deferred_unresolved: Vec::new(),
            pending_indeterminate: BinaryHeap::new(),
            deferred_indeterminate: Vec::new(),
            waits: FxHashMap::default(),
            edges: Vec::new(),
            free_edge: NONE,
            any_waiters: Vec::new(),
            keys: Vec::new(),
            keys_any: false,
            woken: Vec::new(),
        }
    }

    pub(super) fn len(&self) -> u32 {
        self.directives.len() as u32
    }

    pub(super) fn push(&mut self, op: ImportOp) {
        let at = self.directives.len() as u32;
        if op.source.kind == ImportKind::Plain
            && let Some(name) = op.bound_name()
        {
            match self.named.entry((op.module, name)) {
                Entry::Occupied(mut it) => {
                    let (_, tail) = it.get_mut();
                    self.next_named[*tail as usize] = at;
                    *tail = at;
                }
                Entry::Vacant(it) => {
                    it.insert((at, at));
                }
            }
        }
        self.next_named.push(NONE);
        self.directives.push(Directive { op, status: PartialResolvedImport::Unresolved });
        self.list.push(ImportList::Unresolved);
        self.generation.push(0);
        self.queued.push(0);
        self.seq.push(NONE);
        self.wake(at);
    }

    /// The imports still unresolved, in order.
    pub(super) fn unresolved(&self) -> impl Iterator<Item = &Directive> + '_ {
        self.directives
            .iter()
            .zip(&self.list)
            .filter(|(_, list)| **list == ImportList::Unresolved)
            .map(|(directive, _)| directive)
    }

    /// The unresolved plain imports of `module` that bind `name`, in order.
    pub(super) fn unresolved_named(
        &self,
        module: ModuleId,
        name: Name,
    ) -> impl Iterator<Item = &Directive> + '_ {
        let mut at = self.named.get(&(module, name)).map_or(NONE, |&(head, _)| head);
        iter::from_fn(move || {
            while at != NONE {
                let this = at as usize;
                at = self.next_named[this];
                if self.list[this] == ImportList::Unresolved {
                    return Some(&self.directives[this]);
                }
            }
            None
        })
    }

    /// Queues import `at` to be retried: in this pass if its phase has not reached it yet,
    /// else in the next.
    pub(super) fn wake(&mut self, at: u32) {
        let i = at as usize;
        let unresolved = match self.list[i] {
            ImportList::Resolved => return,
            ImportList::Unresolved => true,
            ImportList::Indeterminate => false,
        };
        let key = if unresolved { at } else { self.seq[i] };
        let now = match (self.phase, unresolved) {
            (Phase::Idle, _) | (Phase::Unresolved(_), false) => true,
            (Phase::Unresolved(from), true) | (Phase::Indeterminate(from), false) => key >= from,
            (Phase::Indeterminate(_), true) => false,
        };
        if now {
            if self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                match unresolved {
                    true => self.pending_unresolved.push(Reverse(key)),
                    false => self.pending_indeterminate.push(Reverse(key)),
                }
            }
        } else if self.queued[i] & DEFERRED == 0 {
            self.queued[i] |= DEFERRED;
            match unresolved {
                true => self.deferred_unresolved.push(key),
                false => self.deferred_indeterminate.push(key),
            }
        }
    }

    pub(super) fn begin_pass(&mut self) {
        while let Some(at) = self.deferred_unresolved.pop() {
            let i = at as usize;
            self.queued[i] &= !DEFERRED;
            if self.list[i] == ImportList::Unresolved && self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                self.pending_unresolved.push(Reverse(at));
            }
        }
        while let Some(seq) = self.deferred_indeterminate.pop() {
            let i = self.indeterminate[seq as usize].0 as usize;
            self.queued[i] &= !DEFERRED;
            if self.list[i] == ImportList::Indeterminate && self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                self.pending_indeterminate.push(Reverse(seq));
            }
        }
        self.phase = Phase::Unresolved(0);
    }

    /// The next unresolved import of this pass, by index.
    pub(super) fn next_unresolved(&mut self) -> Option<u32> {
        while let Some(Reverse(at)) = self.pending_unresolved.pop() {
            let i = at as usize;
            self.queued[i] &= !PENDING;
            self.phase = Phase::Unresolved(at + 1);
            if self.list[i] == ImportList::Unresolved {
                return Some(at);
            }
        }
        None
    }

    /// The next indeterminate import of this pass, by the order they became so.
    pub(super) fn next_indeterminate(&mut self) -> Option<u32> {
        while let Some(Reverse(seq)) = self.pending_indeterminate.pop() {
            let i = self.indeterminate[seq as usize].0 as usize;
            self.queued[i] &= !PENDING;
            self.phase = Phase::Indeterminate(seq + 1);
            if self.list[i] == ImportList::Indeterminate {
                return Some(seq);
            }
        }
        None
    }

    /// Starts a new attempt of import `at`: what its last one waited on no longer wakes it.
    pub(super) fn attempt(&mut self, at: u32) {
        let generation = &mut self.generation[at as usize];
        *generation = generation.wrapping_add(1);
    }

    /// Makes import `at`'s current attempt wait on `self.keys` (and on any write if
    /// `self.keys_any`), and clears them.
    pub(super) fn wait(&mut self, at: u32) {
        let generation = self.generation[at as usize];
        if mem::take(&mut self.keys_any) {
            self.any_waiters.push((at, generation));
        }
        for k in 0..self.keys.len() {
            let (key, ns) = self.keys[k];
            let edge = WaitEdge { import: at, generation, next: NONE, ns };
            let slot = match self.free_edge {
                NONE => {
                    self.edges.push(edge);
                    self.edges.len() as u32 - 1
                }
                free => {
                    self.free_edge = self.edges[free as usize].next;
                    self.edges[free as usize] = edge;
                    free
                }
            };
            let head = self.waits.entry(key).or_insert(NONE);
            self.edges[slot as usize].next = *head;
            *head = slot;
        }
        self.keys.clear();
    }

    /// Wakes the current waiters of `key` that read one of the namespaces `ns`, and drops them
    /// and the stale ones from its list.
    fn wake_on(&mut self, key: &WaitOn, ns: u8) {
        let Imports { waits, edges, generation, free_edge, woken, .. } = self;
        let Some(head) = waits.get_mut(key) else { return };
        let mut prev = NONE;
        let mut cur = *head;
        while cur != NONE {
            let edge = edges[cur as usize];
            let stale = generation[edge.import as usize] != edge.generation;
            let hit = !stale && edge.ns & ns != 0;
            if stale || hit {
                if hit {
                    woken.push(edge.import);
                }
                match prev {
                    NONE => *head = edge.next,
                    prev => edges[prev as usize].next = edge.next,
                }
                edges[cur as usize].next = *free_edge;
                *free_edge = cur;
            } else {
                prev = cur;
            }
            cur = edge.next;
        }
        while let Some(at) = self.woken.pop() {
            self.wake(at);
        }
    }

    /// Wakes the imports whose attempt waits on any write; stale ones are dropped.
    fn wake_any(&mut self) {
        let mut waiters = mem::take(&mut self.any_waiters);
        for (at, generation) in waiters.drain(..) {
            if self.generation[at as usize] == generation {
                self.wake(at);
            }
        }
        self.any_waiters = waiters;
    }

    /// A write of the namespaces `ns` of `name` in the scope of `module`.
    pub(super) fn wake_slot(&mut self, module: ModuleId, name: Name, ns: u8) {
        if ns == 0 {
            return;
        }
        if !self.any_waiters.is_empty() {
            self.wake_any();
        }
        if !self.waits.is_empty() {
            self.wake_on(&WaitOn::Slot(module, name), ns);
        }
    }

    /// A new legacy macro, extern prelude or `macro_use` prelude entry named `name`.
    pub(super) fn wake_env(&mut self, name: Name, ns: u8) {
        if !self.any_waiters.is_empty() {
            self.wake_any();
        }
        if !self.waits.is_empty() {
            self.wake_on(&WaitOn::Env(name), ns);
        }
    }

    /// A change every path may have read: the std prelude module, a module's children, or,
    /// when a unit carries on into a new round, any module of another component.
    pub(super) fn wake_all(&mut self) {
        for at in 0..self.directives.len() as u32 {
            self.wake(at);
        }
    }
}

/// One importer of a source, linked to the source's next importer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ImporterEdge<K, T> {
    pub(super) importer: K,
    pub(super) data: T,
    pub(super) next: u32,
}

/// The importers of each source, each list in the order its importers came: one array of edges,
/// linked per source. An edge is appended without moving the others, so a list is read in place
/// while its importers are visited.
#[derive(Clone, Debug)]
pub(super) struct ImporterLists<K, T> {
    edges: Vec<ImporterEdge<K, T>>,
    /// Head and tail of each source's list.
    heads: FxHashMap<K, (u32, u32)>,
    /// The edge of each `(source, importer)`.
    by_pair: FxHashMap<(K, K), u32>,
}

impl<K, T> Default for ImporterLists<K, T> {
    fn default() -> Self {
        ImporterLists {
            edges: Vec::new(),
            heads: FxHashMap::default(),
            by_pair: FxHashMap::default(),
        }
    }
}

impl<K: Copy + Eq + std::hash::Hash, T: Copy> ImporterLists<K, T> {
    /// Adds `importer` to the importers of `source`, or, if it is one already, lets `merge`
    /// update the data it has.
    pub(super) fn add(&mut self, source: K, importer: K, data: T, merge: impl FnOnce(&mut T)) {
        match self.by_pair.entry((source, importer)) {
            Entry::Occupied(it) => merge(&mut self.edges[*it.get() as usize].data),
            Entry::Vacant(it) => {
                let edge = self.edges.len() as u32;
                it.insert(edge);
                self.edges.push(ImporterEdge { importer, data, next: NONE });
                match self.heads.entry(source) {
                    Entry::Occupied(mut it) => {
                        let (_, tail) = it.get_mut();
                        self.edges[*tail as usize].next = edge;
                        *tail = edge;
                    }
                    Entry::Vacant(it) => {
                        it.insert((edge, edge));
                    }
                }
            }
        }
    }

    /// The first edge of `source`'s list, or `NONE`.
    pub(super) fn first(&self, source: K) -> u32 {
        self.heads.get(&source).map_or(NONE, |&(head, _)| head)
    }

    pub(super) fn edge(&self, at: u32) -> ImporterEdge<K, T> {
        self.edges[at as usize]
    }
}

/// The glob imports into the unit's modules: for each source module, the modules that
/// glob-import it, with the visibility and the glob. A source may be a module of another
/// component, whose scope the next round pulls again.
pub(super) type GlobEdges = ImporterLists<ModuleId, (Visibility, GlobId)>;
