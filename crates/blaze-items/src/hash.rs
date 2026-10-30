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

use std::fmt;

use blaze_ids::{ContentId, Hasher};
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
    pub fn describe(&self, db: &dyn HirDatabase) -> String {
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
pub(crate) fn interface_form(db: &dyn HirDatabase, id: ItemId) -> form::Sub {
    form::Cx::new(db).interface(id)
}

#[salsa::tracked(returns(ref))]
pub(crate) fn body_form(db: &dyn HirDatabase, id: ItemId) -> Option<form::Sub> {
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
    fn refs_into(self, db: &dyn HirDatabase, id: ItemId, out: &mut Vec<ItemId>) {
        out.extend_from_slice(&interface_form(db, id).refs);
        if self == Graph::Deep
            && let Some(body) = body_form(db, id)
        {
            out.extend_from_slice(&body.refs);
        }
    }

    /// Whether an edge to `to` can be inside a component of an item of `krate`: a ref to
    /// another crate never is, and the layout of a type depends on other types' layouts only.
    fn inside(self, db: &dyn HirDatabase, krate: base_db::Crate, to: ItemId) -> bool {
        (self != Graph::Layout || matches!(to, ItemId::Adt(_))) && to.krate(db) == krate
    }

    /// What `id` is apart from its refs.
    fn own(self, db: &dyn HirDatabase, id: ItemId) -> Result<ContentId, Unhashable> {
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
                    .part(format!("{:?}", target.arch).as_bytes());
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
    fn fill(self, db: &dyn HirDatabase, to: ItemId) -> Result<ContentId, Unhashable> {
        match (self, to) {
            (Graph::Interface, _) => interface_of(db, to),
            (Graph::Layout, ItemId::Adt(_)) => layout_of(db, to),
            (Graph::Layout, _) => interface_of(db, to),
            (Graph::Deep, _) => deep_of(db, to),
        }
    }
}

fn missed(sub: &form::Sub, item: ItemId) -> Result<(), Unhashable> {
    match sub.missed {
        Some(why) => Err(Unhashable { item, what: why.what() }),
        None => Ok(()),
    }
}

#[salsa::tracked(returns(copy))]
fn interface_of(db: &dyn HirDatabase, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Interface, id)
}

#[salsa::tracked(returns(copy))]
fn layout_of(db: &dyn HirDatabase, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Layout, id)
}

#[salsa::tracked(returns(copy))]
fn deep_of(db: &dyn HirDatabase, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Deep, id)
}

#[salsa::tracked(returns(copy))]
fn code_of(db: &dyn HirDatabase, id: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let Some(body) = body_form(db, id) else { return Ok(None) };
    let interface = interface_of(db, id)?;
    let mut h = Hasher::new(CODE_DOMAIN);
    h.id(&interface);
    Ok(Some(filled_into(db, &mut h, body, id)?))
}

/// A form's digest with each ref filled by its interface hash: what a fact that spells a form
/// (a closure's captures, a field's type) stands for.
pub(crate) fn filled(
    db: &dyn HirDatabase,
    domain: &'static str,
    sub: &form::Sub,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    filled_into(db, &mut Hasher::new(domain), sub, owner)
}

fn filled_into(
    db: &dyn HirDatabase,
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
#[derive(Default)]
struct Batch {
    bytes: Vec<u8>,
    ends: Vec<usize>,
}

impl Batch {
    fn update(&mut self, bytes: &[u8]) -> &mut Batch {
        self.bytes.extend_from_slice(bytes);
        self
    }

    fn id(&mut self, id: &ContentId) -> &mut Batch {
        self.update(&id.0)
    }

    fn word(&mut self, n: usize) -> &mut Batch {
        self.update(&(n as u64).to_le_bytes())
    }

    /// Ends the preimage being written.
    fn end(&mut self) {
        self.ends.push(self.bytes.len());
    }

    /// Each ended preimage's hash, in order; the batch is left empty.
    fn hash(&mut self, domain: &'static str) -> Vec<ContentId> {
        let mut start = 0;
        let inputs: Vec<&[u8]> = self
            .ends
            .iter()
            .map(|&end| {
                let it = &self.bytes[start..end];
                start = end;
                it
            })
            .collect();
        let hashes = ContentId::of_many(domain, &inputs);
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
fn component_hash(db: &dyn HirDatabase, graph: Graph, root: ItemId) -> Result<ContentId, Unhashable> {
    let members = component(db, graph, root);
    let n = members.len();
    let domain = graph.domain();
    let position: FxHashMap<ItemId, usize> =
        members.iter().enumerate().map(|(at, &id)| (id, at)).collect();
    let owns = members.iter().map(|&member| graph.own(db, member)).collect::<Result<Vec<_>, _>>()?;
    // Every member's refs in one list, member `at`'s at `starts[at]..starts[at + 1]`, and what
    // each ref outside the component is filled with, `None` for one inside it.
    let mut starts = Vec::with_capacity(n + 1);
    let mut refs: Vec<ItemId> = Vec::new();
    starts.push(0);
    for &member in &members {
        graph.refs_into(db, member, &mut refs);
        starts.push(refs.len());
    }
    let fills = refs
        .iter()
        .map(|&to| match position.get(&to) {
            Some(_) => Ok(None),
            None => graph.fill(db, to).map(Some),
        })
        .collect::<Result<Vec<Option<ContentId>>, Unhashable>>()?;
    let of = |at: usize| starts[at]..starts[at + 1];

    let mut b = Batch::default();
    if n == 1 && fills.iter().all(Option::is_some) {
        b.update(b"item").id(&owns[0]);
        spell_fills(&mut b, &refs, &fills, |_| 0);
        return Ok(b.hash_one(domain));
    }

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
    let mut classes = distinct(&color);
    for _ in 0..n {
        for at in 0..n {
            b.update(b"refine").id(&color[at]);
            for to in &refs[of(at)] {
                if let Some(&inside) = position.get(to) {
                    b.id(&color[inside]);
                }
            }
            b.end();
        }
        let next = b.hash(domain);
        let next_classes = distinct(&next);
        color = next;
        if next_classes == classes {
            break;
        }
        classes = next_classes;
    }
    let keys: Vec<crate::ItemKey> = members.iter().map(|&id| key::item_key(db, id)).collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| color[a].0.cmp(&color[b].0).then(keys[a].cmp(&keys[b])));
    let mut rank = vec![0; n];
    for (at, &member) in order.iter().enumerate() {
        rank[member] = at;
    }

    b.update(b"cycle").word(n);
    for &at in &order {
        b.id(&owns[at]);
        spell_fills(&mut b, &refs[of(at)], &fills[of(at)], |to| rank[position[&to]]);
    }
    let whole = b.hash_one(domain);
    let mut b = Batch::default();
    b.update(b"member").id(&whole).word(rank[position[&root]]);
    Ok(b.hash_one(domain))
}

fn spell_fills(
    b: &mut Batch,
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

fn distinct(color: &[ContentId]) -> usize {
    let mut sorted: Vec<&ContentId> = color.iter().collect();
    sorted.sort_unstable_by_key(|it| it.0);
    sorted.dedup();
    sorted.len()
}

/// The strongly connected component of `root` in `graph` within its crate, by Tarjan's
/// algorithm (iterative: a chain of refs may be long). The edges of every frame on the work
/// stack are one list: a frame's are its tail when it is popped.
fn component(db: &dyn HirDatabase, graph: Graph, root: ItemId) -> Vec<ItemId> {
    struct Frame {
        node: ItemId,
        at: usize,
        start: usize,
        next: usize,
        end: usize,
    }
    let krate = root.krate(db);
    let mut index: FxHashMap<ItemId, usize> = FxHashMap::default();
    let mut low: Vec<usize> = Vec::new();
    let mut on_stack: Vec<bool> = Vec::new();
    let mut stack: Vec<ItemId> = Vec::new();
    let mut edges: Vec<ItemId> = Vec::new();
    let mut work: Vec<Frame> = Vec::new();
    let visit = |node: ItemId,
                 index: &mut FxHashMap<ItemId, usize>,
                 low: &mut Vec<usize>,
                 on_stack: &mut Vec<bool>,
                 stack: &mut Vec<ItemId>,
                 edges: &mut Vec<ItemId>,
                 work: &mut Vec<Frame>| {
        let at = low.len();
        index.insert(node, at);
        low.push(at);
        on_stack.push(true);
        stack.push(node);
        let start = edges.len();
        graph.refs_into(db, node, edges);
        let mut kept = start;
        for read in start..edges.len() {
            if graph.inside(db, krate, edges[read]) {
                edges[kept] = edges[read];
                kept += 1;
            }
        }
        edges.truncate(kept);
        work.push(Frame { node, at, start, next: start, end: kept });
    };
    visit(root, &mut index, &mut low, &mut on_stack, &mut stack, &mut edges, &mut work);
    let mut found = Vec::new();
    while let Some(frame) = work.last_mut() {
        if frame.next < frame.end {
            let to = edges[frame.next];
            frame.next += 1;
            let from = frame.at;
            match index.get(&to).copied() {
                None => visit(
                    to,
                    &mut index,
                    &mut low,
                    &mut on_stack,
                    &mut stack,
                    &mut edges,
                    &mut work,
                ),
                Some(at) if on_stack[at] => low[from] = low[from].min(at),
                Some(_) => {}
            }
            continue;
        }
        let Frame { node, at, start, .. } = work.pop().expect("the frame just looked at");
        edges.truncate(start);
        if let Some(parent) = work.last() {
            low[parent.at] = low[parent.at].min(low[at]);
        }
        if low[at] == at {
            let mut members = Vec::new();
            while let Some(member) = stack.pop() {
                on_stack[index[&member]] = false;
                members.push(member);
                if member == node {
                    break;
                }
            }
            if node == root {
                found = members;
            }
        }
    }
    found
}

/// The interface hash of an item.
pub fn interface_hash(db: &dyn HirDatabase, id: ItemId) -> Result<InterfaceHash, Unhashable> {
    interface_of(db, id).map(InterfaceHash)
}

/// The deep layout hash of a type.
pub fn layout_hash(db: &dyn HirDatabase, adt: AdtId) -> Result<LayoutHash, Unhashable> {
    layout_of(db, ItemId::Adt(adt)).map(LayoutHash)
}

/// The code hash of an item with a body, `None` for one without.
pub fn code_hash(db: &dyn HirDatabase, id: ItemId) -> Result<Option<CodeHash>, Unhashable> {
    Ok(code_of(db, id)?.map(CodeHash))
}

pub fn deep_hash(db: &dyn HirDatabase, id: ItemId) -> Result<DeepHash, Unhashable> {
    deep_of(db, id).map(DeepHash)
}

pub fn hashes(db: &dyn HirDatabase, id: ItemId) -> Result<ItemHashes, Unhashable> {
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
fn owner_of<'db>(db: &'db dyn HirDatabase, loc: InternedClosure<'db>) -> ItemId {
    match loc.owner {
        InferBodyId::DefWithBodyId(body) => ItemId::of_body(db, body),
        InferBodyId::AnonConstId(anon) => key::generic_def_item(anon.loc(db).owner.generic_def(db)),
    }
}

/// The code hash of one closure, for its codegen alone: its own MIR, signature and captures
/// (#906), each ref by its interface hash.
pub fn closure_code<'db>(
    db: &'db dyn HirDatabase,
    closure: InternedClosureId<'db>,
) -> Result<CodeHash, Unhashable> {
    closure_code_of(db, closure).map(CodeHash)
}

#[salsa::tracked(returns(copy))]
fn closure_code_of<'db>(
    db: &'db dyn HirDatabase,
    closure: InternedClosureId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).closure_code(closure).sub;
    filled(db, CLOSURE_CODE_DOMAIN, &sub, owner_of(db, closure.loc(db)))
}

/// The code hash of one coroutine (an `async` block or fn body, a `gen` block), with its
/// saved locals.
pub fn coroutine_code<'db>(
    db: &'db dyn HirDatabase,
    coroutine: InternedCoroutineId<'db>,
) -> Result<CodeHash, Unhashable> {
    coroutine_code_of(db, coroutine).map(CodeHash)
}

#[salsa::tracked(returns(copy))]
fn coroutine_code_of<'db>(
    db: &'db dyn HirDatabase,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).coroutine_code(coroutine).sub;
    filled(db, COROUTINE_CODE_DOMAIN, &sub, owner_of(db, coroutine.loc(db)))
}
