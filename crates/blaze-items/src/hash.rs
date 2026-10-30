//! The four hashes of an item (docs/v2/plan/lanes/items.md section 6.1): its forms (`form`) with every
//! slot filled by the hash of what it refers to.
//!
//! - **interface**: the interface form, each ref by its interface hash.
//! - **layout** (of a type): the interface form and the target's data layout, each type it
//!   refers to by its layout hash and anything else (a trait in a bound) by its interface
//!   hash: a field's type changing size changes it, transitively.
//! - **code** (of an item with a body): its interface hash and its body form, each ref by its
//!   interface hash, so a callee's body edit leaves it as it was.
//! - **deep**: the interface and body forms, each ref by its deep hash, for what inlines.
//!
//! Items that refer to each other are hashed as one unit, as Unison does: the members of a
//! strongly connected component of the graph of refs are ordered by their content, each ref
//! inside it is the position of its target, and each member's hash is the unit's and its
//! position. Hashing is lazy and per item: a hash asks for the component of its item only,
//! found by Tarjan's algorithm over the refs of its own crate (a cycle never crosses crates,
//! dependencies being acyclic), and fills each ref outside the component by the tracked query
//! of that ref, which never reaches back. Each form is a tracked query, so an edit rewrites
//! the forms it touched and a hash that comes out equal stops what depends on it.
//!
//! A form that could not be written (the program is not one rustc accepts) gives no hash but
//! an [`Unhashable`] naming the item: never a hash salted by where the item is, which two
//! different edits could share.

use std::{alloc::Allocator, fmt};

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Frame, Tag, frame};
use hir_def::AdtId;
use hir_ty::{
    InferBodyId,
    db::{HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
};
use rustc_hash::FxHashMap;

use crate::{CodeHash, DeepHash, InterfaceHash, ItemHashes, ItemId, LayoutHash, form, key};

pub const INTERFACE_DOMAIN: &str = "blaze v2 interface hash 1";
pub const LAYOUT_DOMAIN: &str = "blaze v2 layout hash 1";
pub const CODE_DOMAIN: &str = "blaze v2 code hash 1";
pub const DEEP_DOMAIN: &str = "blaze v2 deep hash 1";
pub const CLOSURE_CODE_DOMAIN: &str = "blaze v2 closure code hash 1";
pub const COROUTINE_CODE_DOMAIN: &str = "blaze v2 coroutine code hash 1";

/// Why an item has no hash: a part of it is not of a program rustc accepts (an error type,
/// MIR that failed to lower, a macro without rules). A build stops at it, naming the item.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Unhashable {
    pub item: ItemId,
    pub what: &'static str,
}

impl Unhashable {
    /// The error a build stops with: the item by kind and path, and what could not be hashed.
    pub fn describe(&self, db: &Db) -> String {
        // alloc: the message a build stops with, written once at the edge.
        format!(
            "blaze-items: {:?} `{}` has no hash: {}",
            self.item.kind(),
            key::path(db, self.item),
            self.what
        )
    }
}

impl fmt::Display for Unhashable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?} has no hash: {}", self.item.kind(), self.item, self.what)
    }
}

impl std::error::Error for Unhashable {}

#[salsa::tracked(returns(ref))]
pub(crate) fn interface_form(db: &Db, id: ItemId) -> form::Sub {
    form::Cx::new(db).interface(id)
}

#[salsa::tracked(returns(ref))]
pub(crate) fn body_form(db: &Db, id: ItemId) -> Option<form::Sub> {
    form::Cx::new(db).body(id).map(|code| code.sub)
}

/// Which graph a hash is over, and so what its refs are filled with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Graph {
    Interface,
    Layout,
    Deep,
}

impl Graph {
    fn domain(self) -> &'static str {
        match self {
            Graph::Interface => INTERFACE_DOMAIN,
            Graph::Layout => LAYOUT_DOMAIN,
            Graph::Deep => DEEP_DOMAIN,
        }
    }

    /// Appends the refs of `id` in this graph to `out`, in slot order.
    fn refs_into<A: Allocator>(self, db: &Db, id: ItemId, out: &mut Vec<ItemId, A>) {
        out.extend_from_slice(&interface_form(db, id).refs);
        if self == Graph::Deep
            && let Some(body) = body_form(db, id)
        {
            out.extend_from_slice(&body.refs);
        }
    }

    /// Whether an edge to `to` can be inside a component of an item of `krate`: a ref to
    /// another crate never is, and the layout of a type depends on other types' layouts only.
    fn inside(self, db: &Db, krate: base_db::Crate, to: ItemId) -> bool {
        (self != Graph::Layout || matches!(to, ItemId::Adt(_))) && to.krate(db) == krate
    }

    /// What `id` is apart from its refs.
    fn own(self, db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
        let interface = interface_form(db, id);
        missed(interface, id)?;
        match self {
            Graph::Interface => Ok(interface.digest),
            Graph::Layout => {
                let target = match &id.krate(db).workspace_data(db).target {
                    Ok(target) => target,
                    Err(_) => {
                        return Err(Unhashable { item: id, what: "its crate has no target" });
                    }
                };
                let mut h = Hasher::new(LAYOUT_DOMAIN);
                h.update(b"own")
                    .id(&interface.digest)
                    .part(target.data_layout.as_bytes())
                    .part(arch_name(target.arch).as_bytes());
                Ok(h.finish())
            }
            Graph::Deep => {
                let mut h = Hasher::new(DEEP_DOMAIN);
                h.update(b"own").id(&interface.digest);
                match body_form(db, id) {
                    Some(body) => {
                        missed(body, id)?;
                        h.update(&[1]).id(&body.digest)
                    }
                    None => h.update(&[0]),
                };
                Ok(h.finish())
            }
        }
    }

    /// The hash a ref outside the component is filled with.
    fn fill(self, db: &Db, to: ItemId) -> Result<ContentId, Unhashable> {
        match (self, to) {
            (Graph::Interface, _) => interface_of(db, to),
            (Graph::Layout, ItemId::Adt(_)) => layout_of(db, to),
            (Graph::Layout, _) => interface_of(db, to),
            (Graph::Deep, _) => deep_of(db, to),
        }
    }
}

/// The arch as a layout hash spells it: its `Debug` name, written out so no string is built.
fn arch_name(arch: base_db::target::Arch) -> &'static str {
    match arch {
        base_db::target::Arch::Wasm32 => "Wasm32",
        base_db::target::Arch::Wasm64 => "Wasm64",
        base_db::target::Arch::Other => "Other",
    }
}

fn missed(sub: &form::Sub, item: ItemId) -> Result<(), Unhashable> {
    match sub.missed {
        Some(why) => Err(Unhashable { item, what: why.what() }),
        None => Ok(()),
    }
}

#[salsa::tracked(returns(copy))]
fn interface_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Interface, id)
}

#[salsa::tracked(returns(copy))]
fn layout_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Layout, id)
}

#[salsa::tracked(returns(copy))]
fn deep_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Deep, id)
}

#[salsa::tracked(returns(copy))]
fn code_of(db: &Db, id: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let Some(body) = body_form(db, id) else { return Ok(None) };
    let interface = interface_of(db, id)?;
    let mut h = Hasher::new(CODE_DOMAIN);
    h.id(&interface);
    Ok(Some(filled_into(db, &mut h, body, id)?))
}

/// A form's digest with each ref filled by its interface hash: what a fact that spells a form
/// (a closure's captures, a field's type) stands for.
pub(crate) fn filled(
    db: &Db,
    domain: &'static str,
    sub: &form::Sub,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    filled_into(db, &mut Hasher::new(domain), sub, owner)
}

fn filled_into(
    db: &Db,
    h: &mut Hasher,
    sub: &form::Sub,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    missed(sub, owner)?;
    h.id(&sub.digest).update(&(sub.refs.len() as u64).to_le_bytes());
    for &to in sub.refs.iter() {
        h.id(&interface_of(db, to)?);
    }
    Ok(h.finish())
}

/// Preimages written back to back, each hashed alone in one batched call: bit for bit what
/// one [`Hasher`] per preimage gives, as `update` and `id` write the bytes a `Hasher` would.
struct Batch<'f> {
    bytes: Vec<u8, &'f Frame>,
    ends: Vec<usize, &'f Frame>,
}

impl<'f> Batch<'f> {
    fn new(frame: &'f Frame) -> Batch<'f> {
        Batch { bytes: Vec::new_in(frame), ends: Vec::new_in(frame) }
    }

    fn update(&mut self, bytes: &[u8]) -> &mut Batch<'f> {
        self.bytes.extend_from_slice(bytes);
        self
    }

    fn id(&mut self, id: &ContentId) -> &mut Batch<'f> {
        self.update(&id.0)
    }

    fn word(&mut self, n: usize) -> &mut Batch<'f> {
        self.update(&(n as u64).to_le_bytes())
    }

    /// Ends the preimage being written.
    fn end(&mut self) {
        self.ends.push(self.bytes.len());
    }

    /// Each ended preimage's hash, in order; the batch is left empty.
    fn hash(&mut self, domain: &'static str) -> Vec<ContentId> {
        let frame = *self.bytes.allocator();
        let mut inputs: Vec<&[u8], &Frame> = Vec::with_capacity_in(self.ends.len(), frame);
        let mut start = 0;
        for &end in self.ends.iter() {
            inputs.push(&self.bytes[start..end]);
            start = end;
        }
        // alloc: one hash per member, the colors a round refines, made by `of_many`.
        let hashes = ContentId::of_many(domain, &inputs);
        drop(inputs);
        self.bytes.clear();
        self.ends.clear();
        hashes
    }

    /// The hash of everything written, as one preimage.
    fn hash_one(&self, domain: &'static str) -> ContentId {
        ContentId::of(domain, &self.bytes)
    }
}

/// The hash of `root` in `graph`: of its component, as the module doc says.
fn component_hash(db: &Db, graph: Graph, root: ItemId) -> Result<ContentId, Unhashable> {
    let members = component(db, graph, root);
    if let [only] = members[..]
        && let Some(hash) = alone(db, graph, only)?
    {
        return Ok(hash);
    }
    let n = members.len();
    let domain = graph.domain();
    // alloc: what the members are spelled from is gathered while their refs are filled, and a
    // fill is the hash of another component, which recurses as deep as the graph of refs: a
    // frame (which nests a bounded depth) opens only once every fill is in.
    // Most components are one item, which needs no map to find a member's position.
    let position: FxHashMap<ItemId, usize> = match n {
        1 => FxHashMap::default(),
        // alloc: a cycle's members, as above.
        _ => members.iter().enumerate().map(|(at, &id)| (id, at)).collect(),
    };
    let inside = |to: ItemId| match n {
        1 => (to == root).then_some(0),
        _ => position.get(&to).copied(),
    };
    let owns = members.iter().map(|&member| graph.own(db, member)).collect::<Result<Vec<_>, _>>()?;
    // Every member's refs in one list, member `at`'s at `starts[at]..starts[at + 1]`, and what
    // each ref outside the component is filled with, `None` for one inside it.
    let mut starts = Vec::with_capacity(n + 1);
    // alloc: as above; one list for every member's refs.
    let mut refs: Vec<ItemId> = Vec::new();
    starts.push(0);
    for &member in &members {
        graph.refs_into(db, member, &mut refs);
        starts.push(refs.len());
    }
    let fills = refs
        .iter()
        .map(|&to| match inside(to) {
            Some(_) => Ok(None),
            None => graph.fill(db, to).map(Some),
        })
        .collect::<Result<Vec<Option<ContentId>>, Unhashable>>()?;
    let of = |at: usize| starts[at]..starts[at + 1];
    let root_at = inside(root).expect("the root is a member");

    let (whole, rank) = frame::task(Tag::Other, |frame| {
        let mut b = Batch::new(frame);
        // Order the members by content: their own content and what is outside, refined by the
        // content of the members they refer to until the classes stop splitting; members still
        // alike are ordered by key, which only matters where swapping them hashes alike. Each
        // round hashes every member in one batch.
        for at in 0..n {
            b.update(b"shape").id(&owns[at]);
            spell_fills(&mut b, &refs[of(at)], &fills[of(at)], |_| 0);
            b.end();
        }
        let mut color = b.hash(domain);
        let mut classes = distinct(&color, frame);
        for _ in 0..n {
            for at in 0..n {
                b.update(b"refine").id(&color[at]);
                for to in &refs[of(at)] {
                    if let Some(at) = inside(*to) {
                        b.id(&color[at]);
                    }
                }
                b.end();
            }
            let next = b.hash(domain);
            let next_classes = distinct(&next, frame);
            color = next;
            if next_classes == classes {
                break;
            }
            classes = next_classes;
        }
        let mut keys: Vec<crate::ItemKey, &Frame> = Vec::with_capacity_in(n, frame);
        keys.extend(members.iter().map(|&id| key::item_key(db, id)));
        let mut order: Vec<usize, &Frame> = Vec::with_capacity_in(n, frame);
        order.extend(0..n);
        order.sort_by(|&a, &b| color[a].0.cmp(&color[b].0).then(keys[a].cmp(&keys[b])));
        let mut rank: Vec<usize, &Frame> = Vec::with_capacity_in(n, frame);
        rank.resize(n, 0);
        for (at, &member) in order.iter().enumerate() {
            rank[member] = at;
        }

        b.update(b"cycle").word(n);
        for &at in order.iter() {
            b.id(&owns[at]);
            spell_fills(&mut b, &refs[of(at)], &fills[of(at)], |to| {
                rank[inside(to).expect("a ref without a fill is to a member")]
            });
        }
        (b.hash_one(domain), rank[root_at])
    });
    let mut h = Hasher::new(domain);
    h.update(b"member").id(&whole).update(&(rank as u64).to_le_bytes());
    Ok(h.finish())
}

/// The hash of `only`, a component of one item, when none of its refs is to itself, streamed
/// from its forms as [`component_hash`] would spell it; `None` for one that is.
fn alone(db: &Db, graph: Graph, only: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let own = graph.own(db, only)?;
    let interface: &[ItemId] = &interface_form(db, only).refs;
    let body: &[ItemId] = match graph {
        Graph::Deep => body_form(db, only).as_ref().map_or(&[][..], |it| &it.refs[..]),
        Graph::Interface | Graph::Layout => &[],
    };
    let refs = || interface.iter().chain(body).copied();
    if refs().any(|to| to == only) {
        return Ok(None);
    }
    let mut h = Hasher::new(graph.domain());
    h.update(b"item").id(&own).update(&((interface.len() + body.len()) as u64).to_le_bytes());
    for to in refs() {
        h.update(&[1]).id(&graph.fill(db, to)?);
    }
    Ok(Some(h.finish()))
}

fn spell_fills(
    b: &mut Batch<'_>,
    refs: &[ItemId],
    fills: &[Option<ContentId>],
    inside: impl Fn(ItemId) -> usize,
) {
    b.word(refs.len());
    for (&to, fill) in refs.iter().zip(fills) {
        match fill {
            Some(fill) => b.update(&[1]).id(fill),
            None => b.update(&[0]).word(inside(to)),
        };
    }
}

fn distinct(color: &[ContentId], frame: &Frame) -> usize {
    let mut sorted: Vec<&ContentId, &Frame> = Vec::with_capacity_in(color.len(), frame);
    sorted.extend(color.iter());
    sorted.sort_unstable_by_key(|it| it.0);
    sorted.dedup();
    sorted.len()
}

/// The strongly connected component of `root` in `graph` within its crate, by Tarjan's
/// algorithm (iterative: a chain of refs may be long). The edges of every frame on the work
/// stack are one list: a frame's are its tail when it is popped.
fn component(db: &Db, graph: Graph, root: ItemId) -> Vec<ItemId> {
    frame::task(Tag::Other, |frame| Tarjan::new(db, graph, root, frame).run(root))
}

/// A node being searched from: `edges[next..end]` are its edges still to follow, and
/// `edges[start..]` is its when it is popped.
struct Visit {
    node: ItemId,
    at: usize,
    start: usize,
    next: usize,
    end: usize,
}

struct Tarjan<'a, 'f> {
    db: &'a Db,
    graph: Graph,
    krate: base_db::Crate,
    // alloc: std's map takes no allocator; one per component searched.
    index: FxHashMap<ItemId, usize>,
    low: Vec<usize, &'f Frame>,
    on_stack: Vec<bool, &'f Frame>,
    stack: Vec<ItemId, &'f Frame>,
    edges: Vec<ItemId, &'f Frame>,
    work: Vec<Visit, &'f Frame>,
}

impl<'a, 'f> Tarjan<'a, 'f> {
    fn new(db: &'a Db, graph: Graph, root: ItemId, frame: &'f Frame) -> Self {
        Tarjan {
            db,
            graph,
            krate: root.krate(db),
            index: FxHashMap::default(),
            low: Vec::new_in(frame),
            on_stack: Vec::new_in(frame),
            stack: Vec::new_in(frame),
            edges: Vec::new_in(frame),
            work: Vec::new_in(frame),
        }
    }

    fn visit(&mut self, node: ItemId) {
        let at = self.low.len();
        self.index.insert(node, at);
        self.low.push(at);
        self.on_stack.push(true);
        self.stack.push(node);
        let start = self.edges.len();
        self.graph.refs_into(self.db, node, &mut self.edges);
        let mut kept = start;
        for read in start..self.edges.len() {
            if self.graph.inside(self.db, self.krate, self.edges[read]) {
                self.edges[kept] = self.edges[read];
                kept += 1;
            }
        }
        self.edges.truncate(kept);
        self.work.push(Visit { node, at, start, next: start, end: kept });
    }

    /// The members of `root`'s component, in the order they leave the stack.
    fn run(mut self, root: ItemId) -> Vec<ItemId> {
        self.visit(root);
        // alloc: the component outlives the search's frame.
        let mut found = Vec::new();
        while let Some(visit) = self.work.last_mut() {
            if visit.next < visit.end {
                let to = self.edges[visit.next];
                visit.next += 1;
                let from = visit.at;
                match self.index.get(&to).copied() {
                    None => self.visit(to),
                    Some(at) if self.on_stack[at] => self.low[from] = self.low[from].min(at),
                    Some(_) => {}
                }
                continue;
            }
            let Visit { node, at, start, .. } = self.work.pop().expect("the node just looked at");
            self.edges.truncate(start);
            if let Some(parent) = self.work.last() {
                self.low[parent.at] = self.low[parent.at].min(self.low[at]);
            }
            if self.low[at] == at {
                let first = self
                    .stack
                    .iter()
                    .rposition(|&member| member == node)
                    .expect("a node stays on the stack until its component leaves it");
                if node == root {
                    found.extend(self.stack[first..].iter().rev());
                }
                for member in self.stack.drain(first..) {
                    self.on_stack[self.index[&member]] = false;
                }
            }
        }
        found
    }
}

/// The interface hash of an item.
pub fn interface_hash(db: &Db, id: ItemId) -> Result<InterfaceHash, Unhashable> {
    interface_of(db, id).map(InterfaceHash)
}

/// The deep layout hash of a type.
pub fn layout_hash(db: &Db, adt: AdtId) -> Result<LayoutHash, Unhashable> {
    layout_of(db, ItemId::Adt(adt)).map(LayoutHash)
}

/// The code hash of an item with a body, `None` for one without.
pub fn code_hash(db: &Db, id: ItemId) -> Result<Option<CodeHash>, Unhashable> {
    Ok(code_of(db, id)?.map(CodeHash))
}

pub fn deep_hash(db: &Db, id: ItemId) -> Result<DeepHash, Unhashable> {
    deep_of(db, id).map(DeepHash)
}

pub fn hashes(db: &Db, id: ItemId) -> Result<ItemHashes, Unhashable> {
    Ok(ItemHashes {
        kind: id.kind(),
        interface: interface_hash(db, id)?,
        layout: match id {
            ItemId::Adt(adt) => Some(layout_hash(db, adt)?),
            _ => None,
        },
        code: code_hash(db, id)?,
        deep: deep_hash(db, id)?,
    })
}

/// The item a closure or coroutine is written in.
fn owner_of<'db>(db: &'db Db, loc: InternedClosure<'db>) -> ItemId {
    match loc.owner {
        InferBodyId::DefWithBodyId(body) => ItemId::of_body(db, body),
        InferBodyId::AnonConstId(anon) => key::generic_def_item(anon.loc(db).owner.generic_def(db)),
    }
}

/// The code hash of one closure, for its codegen alone: its own MIR, signature and captures
/// (#906), each ref by its interface hash.
pub fn closure_code<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Result<CodeHash, Unhashable> {
    closure_code_of(db, closure).map(CodeHash)
}

#[salsa::tracked(returns(copy))]
fn closure_code_of<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).closure_code(closure).sub;
    filled(db, CLOSURE_CODE_DOMAIN, &sub, owner_of(db, closure.loc(db)))
}

/// The code hash of one coroutine (an `async` block or fn body, a `gen` block), with its
/// saved locals.
pub fn coroutine_code<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<CodeHash, Unhashable> {
    coroutine_code_of(db, coroutine).map(CodeHash)
}

#[salsa::tracked(returns(copy))]
fn coroutine_code_of<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).coroutine_code(coroutine).sub;
    filled(db, COROUTINE_CODE_DOMAIN, &sub, owner_of(db, coroutine.loc(db)))
}
