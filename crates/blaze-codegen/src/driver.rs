//! A build's codegen, one function instance at a time (docs/v2-codegen.md).
//!
//! 1. **Walk**: from the roots, the front end describes each instance and what it refers to,
//!    a level at a time on all cores; the policy gives each its settings and so its key.
//! 2. **Places**: each instance's references resolved to this build's targets (a callee
//!    reached through its slot or directly).
//! 3. **Dev** (Cranelift): a template from the store by instance key, else from the memo by
//!    shape (#1291), else compiled; folded groups first where the settings ask (#1292).
//! 4. **Release** (LLVM): every summary (stored, else from prepared IR), the imports of the
//!    whole build decided from them, then each compile by its imports' blob key.
//! 5. **Statics**, by their value's content and settings, never merged.
//! 6. Every template instantiated with this build's places, then code dedupe.
//!
//! The store is read through [`Lookup`] on the calling thread and written by the caller from
//! [`Built::puts`]: what the build compiled, its keys. A store entry that fails to read is an
//! error naming the unit, never a reason to compile again.

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::Arc,
};

use blaze_cas::{Key, Kind};
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use blaze_mem::{Tag, frame};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    CodegenError, CompiledFunction, Instance, InstanceKey, Static, Target, Unit,
    backend::{
        Address, Described, DescribedStatic, Emitted, FrontRef, Frontend, Import, Prepare, Shape,
        release_refs,
    },
    blob,
    dedupe::{Deduped, dedupe},
    fold::{self, Group, Masked},
    imports,
    memo::Memo,
    plan::{Origin, Policy},
    query::keys,
    settings::Spec,
    summary::{Imports, Summary},
    template::{StaticTemplate, Template, TemplateError},
};

/// The store as a build reads it.
pub trait Lookup {
    /// The verified bytes under `key`, borrowed from the store, read in place; `Err` names
    /// why what is there can't be read.
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String>;
}

impl Lookup for blaze_cas::Store {
    fn read(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        match blaze_cas::Store::get(self, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes)),
            // alloc: an error
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}

/// A value for the store.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Put {
    pub key: Key,
    pub kind: Kind,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    pub instances: usize,
    pub statics: usize,
    /// Templates read from the store.
    pub stored: usize,
    /// Templates the memo had: compiles saved up to callee renaming.
    pub memo: usize,
    pub compiled: usize,
    /// Functions that became thunks of a folded body.
    pub folded: usize,
    /// Imports of the release compiles that ran.
    pub imported: usize,
}

/// What a build gives the linker.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Built {
    /// By key, after dedupe.
    pub functions: Vec<CompiledFunction>,
    /// By item and instance.
    pub statics: Vec<Static>,
    pub roots: Vec<InstanceKey>,
    /// Functions outside code names, with their symbols.
    pub symbols: Vec<(InstanceKey, String)>,
    pub static_symbols: Vec<(ItemKey, ContentId, String)>,
    /// Functions reached through their slots.
    pub patchable: Vec<InstanceKey>,
    /// Functions of a live build whose code [`Build::tier_up`] replaces.
    pub tiered: Vec<InstanceKey>,
    pub deduped: Deduped,
    /// By key, each once.
    pub puts: Vec<Put>,
    pub stats: Stats,
}

/// What [`Build::tier_up`] gives: each tiered function's key, with the code for its slot.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TieredUp {
    pub replaced: Vec<(InstanceKey, CompiledFunction)>,
    pub puts: Vec<Put>,
    pub stats: Stats,
}

type FrontInstance<B> = <<B as Prepare>::Frontend as Frontend>::Instance;
type FrontStatic<B> = <<B as Prepare>::Frontend as Frontend>::Static;

struct FnNode<B: Prepare> {
    unit: Unit,
    spec: Spec,
    then: Option<Spec>,
    patchable: bool,
    instance: FrontInstance<B>,
    described: Described<FrontInstance<B>, FrontStatic<B>>,
    /// This build's targets of `described.refs`.
    refs: Vec<Target>,
    /// The functions its code builds in, at any depth, sorted.
    built_in: Vec<InstanceKey>,
}

pub const BUILT_IN_DOMAIN: &str = "blaze v2 built in 1";

/// What a unit's compiled code is kept under: its key, and the keys of the bodies it builds
/// in, as its own key takes callees by their interface.
fn stored(key: InstanceKey, built_in: &[InstanceKey]) -> InstanceKey {
    if built_in.is_empty() {
        return key;
    }
    let mut hasher = Hasher::new(BUILT_IN_DOMAIN);
    hasher.id(&key.0).update(&(built_in.len() as u64).to_le_bytes());
    for it in built_in {
        hasher.id(&it.0);
    }
    InstanceKey(hasher.finish())
}

impl<B: Prepare> FnNode<B> {
    /// What its code is kept under.
    fn stored(&self) -> InstanceKey {
        stored(self.unit.instance.key(), &self.built_in)
    }

    /// Whether code outside the build may call it, so nothing is assumed of its callers.
    fn external(&self) -> bool {
        self.patchable || self.then.is_some() || self.described.address == Address::Significant
    }
}

struct StaticNode<B: Prepare> {
    value: FrontStatic<B>,
    described: DescribedStatic<FrontInstance<B>, FrontStatic<B>>,
    spec: Spec,
    refs: Vec<Target>,
}

type StaticId = (ItemKey, ContentId);

/// A walk's nodes in key order, sorted once when the walk ends, each found by one hash of its
/// key: per entry its key, its value and a 4-byte slot of `at`.
struct Sorted<K, V> {
    keys: Vec<K>,
    values: Vec<V>,
    at: FxHashMap<K, u32>,
}

impl<K, V> Default for Sorted<K, V> {
    fn default() -> Self {
        // alloc: none, the lists are empty
        Sorted { keys: Vec::new(), values: Vec::new(), at: FxHashMap::default() }
    }
}

impl<K: Copy + Ord + Hash, V> Sorted<K, V> {
    /// The entries of `map` by key. Each value moves twice whatever its size: only the
    /// permutation is sorted.
    fn of(map: FxHashMap<K, V>) -> Self {
        // alloc: per build, sized by its units
        let mut entries: Vec<Option<(K, V)>> = map.into_iter().map(Some).collect();
        assert!(entries.len() <= u32::MAX as usize, "more than 2^32 nodes");
        // alloc: per build, sized by its units
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        // Keys are unique: an unstable sort is the order.
        order.sort_unstable_by_key(|&at| entries[at as usize].as_ref().expect("not taken yet").0);
        let mut sorted = Sorted {
            keys: Vec::with_capacity(entries.len()),
            values: Vec::with_capacity(entries.len()),
            at: FxHashMap::with_capacity_and_hasher(entries.len(), Default::default()),
        };
        for at in order {
            let (key, value) = entries[at as usize].take().expect("each entry once");
            sorted.at.insert(key, sorted.keys.len() as u32);
            sorted.keys.push(key);
            sorted.values.push(value);
        }
        sorted
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.at.get(key).map(|&at| &self.values[at as usize])
    }

    fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.keys.iter().zip(&self.values)
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.keys.iter().zip(&mut self.values)
    }

    fn values(&self) -> std::slice::Iter<'_, V> {
        self.values.iter()
    }

    fn values_mut(&mut self) -> std::slice::IterMut<'_, V> {
        self.values.iter_mut()
    }
}

impl<K: Copy + Ord + Hash, V> std::ops::Index<&K> for Sorted<K, V> {
    type Output = V;

    fn index(&self, key: &K) -> &V {
        self.get(key).expect("a node of the walk")
    }
}

/// One compile of a release phase: `name` is the key callers' places and summaries name it
/// by, `unit` what is compiled (another key for a function tiered up).
struct Job<'n, B: Prepare> {
    name: InstanceKey,
    unit: Unit,
    spec: &'n Spec,
    node: &'n FnNode<B>,
}

/// A body `function` built in that none of `keys` is: code its store key does not hold.
fn unkept_inline<B: Prepare>(
    nodes: &Sorted<InstanceKey, FnNode<B>>,
    function: &CompiledFunction,
    keys: impl IntoIterator<Item = InstanceKey>,
) -> Option<blaze_items::CodeHash> {
    if function.inlined.is_empty() {
        return None;
    }
    let kept: HashSet<blaze_items::CodeHash> =
        // alloc: per compile that builds code in, sized by what it builds in
        keys.into_iter().filter_map(|it| nodes.get(&it)).map(|it| it.unit.instance.code).collect();
    function.inlined.iter().find(|it| !kept.contains(*it)).copied()
}

impl<B: Prepare> Job<'_, B> {
    fn stored(&self) -> InstanceKey {
        stored(self.unit.instance.key(), &self.node.built_in)
    }
}

fn error(unit: InstanceKey, item: ItemKey, what: impl Into<String>) -> CodegenError {
    CodegenError { unit, item, what: what.into() }
}

/// A store read: what is there, nothing, or what is there and can't be read, which is an
/// error of the build already recorded and never a reason to compile again.
enum Read<T> {
    Hit(T),
    Miss,
    Failed,
}

/// What a build collects as it goes.
#[derive(Default)]
struct Out {
    functions: Vec<CompiledFunction>,
    statics: Vec<Static>,
    puts: Vec<Put>,
    errors: Vec<CodegenError>,
    stats: Stats,
}

impl Out {
    fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) {
        self.puts.push(Put { key, kind, bytes });
    }

    fn template(&mut self, key: Key, template: &Template) {
        self.put(key, Kind::Code, blob::encode(template));
    }

    /// A compile's side table under its content ID: its bytes move to the put.
    fn side(&mut self, unit: &Unit, emitted: &mut Emitted) {
        match (emitted.side.take(), emitted.function.side) {
            (Some(bytes), Some(id)) => self.put(keys::side(&id), Kind::Debug, bytes),
            (None, None) => {}
            _ => self.errors.push(error(
                unit.instance.key(),
                unit.item,
                "the compile's side table and the function's record of it disagree",
            )),
        }
    }

    /// Reads one value in place, or records why it can't.
    fn read<T>(
        &mut self,
        store: &impl Lookup,
        key: &Key,
        unit: &Unit,
        decode: impl FnOnce(&[u8]) -> Result<T, String>,
    ) -> Read<T> {
        match store.read(key) {
            Ok(Some(bytes)) => match decode(bytes) {
                Ok(it) => Read::Hit(it),
                Err(why) => {
                    // alloc: an error
                    self.errors.push(error(unit.instance.key(), unit.item, format!("the store's {key:?}: {why}")));
                    Read::Failed
                }
            },
            Ok(None) => Read::Miss,
            Err(why) => {
                // alloc: an error
                self.errors.push(error(unit.instance.key(), unit.item, format!("reading the store's {key:?}: {why}")));
                Read::Failed
            }
        }
    }

    /// The function of `template` for `key` with `refs`, into the build's functions.
    fn instantiate(&mut self, unit: &Unit, key: InstanceKey, template: &Template, refs: &[Target]) {
        self.made(unit, key, template.instantiate(key, refs));
    }

    /// A function made for `key`, into the build's functions, or why it could not be made.
    fn made(&mut self, unit: &Unit, key: InstanceKey, made: Result<CompiledFunction, TemplateError>) {
        match made {
            Ok(function) => self.functions.push(function),
            // alloc: an error
            Err(why) => self.errors.push(error(key, unit.item, why.to_string())),
        }
    }

    /// A compile's template against `refs`, taking the compile's function over, or the error
    /// naming why it has none.
    fn template_of(&mut self, unit: &Unit, emitted: Emitted, refs: &[Target]) -> Option<Template> {
        let key = emitted.function.key;
        match Template::of_owned(emitted.function, refs, &emitted.extra) {
            Ok(it) => Some(it),
            Err(why) => {
                // alloc: an error
                self.errors.push(error(key, unit.item, why.to_string()));
                None
            }
        }
    }

    fn finish(mut self) -> (Vec<CompiledFunction>, Vec<Static>, Vec<Put>, Stats, Vec<CodegenError>) {
        self.puts.sort_by(|a, b| a.key.cmp(&b.key));
        self.puts.dedup_by(|a, b| a.key == b.key);
        self.errors.sort_by_key(|it| (it.unit, it.item));
        self.errors.dedup();
        (self.functions, self.statics, self.puts, self.stats, self.errors)
    }
}

/// The function of instance `key` with `refs` in a stored template blob, read in place: its
/// lists are decoded into a frame and each part copied once, into the function. The outer
/// `Err` is bytes that are no template; the inner is the template's own.
fn stored_function(
    bytes: &[u8],
    key: InstanceKey,
    refs: &[Target],
) -> Result<Result<CompiledFunction, TemplateError>, String> {
    frame::task(Tag::Codegen, |frame| {
        // alloc: an error
        let view = blob::decode_view(bytes, frame).map_err(|e| e.to_string())?;
        Ok(view.instantiate(key, refs))
    })
}

/// One build's codegen.
pub struct Build<'a, B: Prepare> {
    backend: &'a B,
    frontend: &'a B::Frontend,
    policy: &'a Policy,
    memo: &'a Memo,
    nodes: Sorted<InstanceKey, FnNode<B>>,
    /// Functions the last run's statics refer to.
    static_callees: FxHashSet<InstanceKey>,
}

/// The function a target names, if it names one.
fn callee(target: &Target) -> Option<InstanceKey> {
    match target {
        Target::Function(key) | Target::Direct(key) => Some(*key),
        _ => None,
    }
}

impl<'a, B: Prepare> Build<'a, B> {
    pub fn new(backend: &'a B, frontend: &'a B::Frontend, policy: &'a Policy, memo: &'a Memo) -> Self {
        Build { backend, frontend, policy, memo, nodes: Sorted::default(), static_callees: FxHashSet::default() }
    }

    fn node(
        &self,
        instance: &FrontInstance<B>,
        described: Described<FrontInstance<B>, FrontStatic<B>>,
    ) -> Result<FnNode<B>, CodegenError> {
        let choice = self.policy.choose(&described.origin, described.overflow_checks).map_err(|why| {
            let unit = Instance {
                code: described.code,
                args: described.args,
                settings: crate::Settings(self.policy.id()),
            };
            // alloc: an error
            error(unit.key(), described.item, why.to_string())
        })?;
        let instance_of = Instance { code: described.code, args: described.args, settings: choice.first.settings() };
        let unit = Unit {
            instance: instance_of,
            backend: choice.first.backend,
            arch: self.policy.toolchain.arch,
            item: described.item,
        };
        Ok(FnNode {
            unit,
            spec: choice.first,
            then: choice.then,
            patchable: choice.patchable,
            // clone: per build, the walk's handle of the instance
            instance: instance.clone(),
            described,
            // alloc: none, empty until the walk resolves them
            refs: Vec::new(),
            // alloc: none, empty until the walk resolves them
            built_in: Vec::new(),
        })
    }

    fn static_node(
        &self,
        value: &FrontStatic<B>,
        described: DescribedStatic<FrontInstance<B>, FrontStatic<B>>,
    ) -> Result<StaticNode<B>, CodegenError> {
        let choice = self.policy.choose(&described.origin, None).map_err(|why| {
            // alloc: an error
            error(InstanceKey(described.value), described.item, why.to_string())
        })?;
        // clone: per build, the walk's handle of the static; its places are empty until resolved
        Ok(StaticNode { value: value.clone(), described, spec: choice.first, refs: Vec::new() })
    }

    /// Every instance and static the roots reach, each with its places resolved.
    fn walk(
        &self,
        roots: &[(FrontInstance<B>, Origin)],
        errors: &mut Vec<CodegenError>,
    ) -> (Sorted<InstanceKey, FnNode<B>>, Sorted<StaticId, StaticNode<B>>, Vec<InstanceKey>) {
        let mut nodes: FxHashMap<InstanceKey, FnNode<B>> = FxHashMap::default();
        let mut statics: FxHashMap<StaticId, StaticNode<B>> = FxHashMap::default();
        let mut asked: HashMap<(FrontInstance<B>, Origin), InstanceKey> = HashMap::new();
        let mut static_asked: HashMap<FrontStatic<B>, StaticId> = HashMap::new();
        let mut queued: HashSet<(FrontInstance<B>, Origin)> = HashSet::new();
        let mut static_queued: HashSet<FrontStatic<B>> = HashSet::new();
        let mut level: Vec<(FrontInstance<B>, Origin)> =
            // clone: per build, the walk's handles of its roots
            roots.iter().filter(|it| queued.insert((*it).clone())).cloned().collect();
        // alloc: per build, sized by its units
        let mut static_level: Vec<FrontStatic<B>> = Vec::new();
        let enqueue = |refs: &[FrontRef<FrontInstance<B>, FrontStatic<B>>],
                       requester: &Origin,
                       queued: &mut HashSet<(FrontInstance<B>, Origin)>,
                       static_queued: &mut HashSet<FrontStatic<B>>,
                       next: &mut Vec<(FrontInstance<B>, Origin)>,
                       next_statics: &mut Vec<FrontStatic<B>>| {
            for it in refs {
                match it {
                    FrontRef::Function(callee) => {
                        // clone: per build, the walk's handle of a callee and who asks for it
                        let asked = (callee.clone(), requester.clone());
                        // clone: per build, the walk's handle of a callee and who asks for it
                        if queued.insert(asked.clone()) {
                            next.push(asked);
                        }
                    }
                    FrontRef::Static(value) => {
                        // clone: per build, the walk's handle of a static
                        if static_queued.insert(value.clone()) {
                            // clone: per build, the walk's handle of a static
                            next_statics.push(value.clone());
                        }
                    }
                    FrontRef::Extern(_) | FrontRef::Weak(_) => {}
                }
            }
        };
        while !level.is_empty() || !static_level.is_empty() {
            let described: Vec<_> = level
                .par_iter()
                .map(|(instance, requester)| {
                    self.frontend.describe(instance, requester).and_then(|it| self.node(instance, it))
                })
                // alloc: per build, sized by its units
                .collect();
            let described_statics: Vec<_> = static_level
                .par_iter()
                .map(|value| self.frontend.describe_static(value).and_then(|it| self.static_node(value, it)))
                // alloc: per build, sized by its units
                .collect();
            // alloc: per build, sized by its units
            let (mut next, mut next_statics) = (Vec::new(), Vec::new());
            for (asked_for, node) in std::mem::take(&mut level).into_iter().zip(described) {
                let node = match node {
                    Ok(node) => node,
                    Err(why) => {
                        errors.push(why);
                        continue;
                    }
                };
                let key = node.unit.instance.key();
                asked.insert(asked_for, key);
                // clone: per build, the requester of what it refers to
                let origin = node.described.origin.clone();
                enqueue(&node.described.refs, &origin, &mut queued, &mut static_queued, &mut next, &mut next_statics);
                nodes.entry(key).or_insert(node);
            }
            for (value, node) in std::mem::take(&mut static_level).into_iter().zip(described_statics) {
                let node = match node {
                    Ok(node) => node,
                    Err(why) => {
                        errors.push(why);
                        continue;
                    }
                };
                let id = (node.described.item, node.described.instance);
                static_asked.insert(value, id);
                // clone: per build, the requester of what it refers to
                let origin = node.described.origin.clone();
                enqueue(&node.described.refs, &origin, &mut queued, &mut static_queued, &mut next, &mut next_statics);
                statics.entry(id).or_insert(node);
            }
            level = next;
            static_level = next_statics;
        }
        let (mut nodes, mut statics) = (Sorted::of(nodes), Sorted::of(statics));

        let through_slot: HashMap<InstanceKey, bool> =
            // alloc: per build, sized by its units
            nodes.iter().map(|(key, node)| (*key, node.patchable || node.then.is_some())).collect();
        let resolve = |refs: &[FrontRef<FrontInstance<B>, FrontStatic<B>>], requester: &Origin| {
            refs.iter()
                .map(|it| match it {
                    FrontRef::Function(callee) => {
                        // clone: per build, the walk's key of a callee and who asks for it
                        let key = asked.get(&(callee.clone(), requester.clone()))?;
                        Some(match through_slot.get(key) {
                            Some(true) => Target::Function(*key),
                            _ => Target::Direct(*key),
                        })
                    }
                    FrontRef::Static(value) => {
                        let (item, instance) = *static_asked.get(value)?;
                        Some(Target::Static { item, instance })
                    }
                    // clone: per build, the instance's own target, kept by its node
                    FrontRef::Extern(name) => Some(Target::Extern(name.clone())),
                    // clone: per build, the instance's own target, kept by its node
                    FrontRef::Weak(name) => Some(Target::Weak(name.clone())),
                })
                // alloc: per build, an instance's places, kept by its node
                .collect::<Option<Vec<_>>>()
        };
        // An instance one of whose references failed has no places; that failure is already
        // an error of the build.
        for node in nodes.values_mut() {
            node.refs = resolve(&node.described.refs, &node.described.origin).unwrap_or_default();
        }
        // What each builds in directly, then at any depth.
        let mut direct: FxHashMap<InstanceKey, Vec<InstanceKey>> =
            FxHashMap::with_capacity_and_hasher(nodes.len(), Default::default());
        for (key, node) in nodes.iter() {
            if node.refs.is_empty() && !node.described.refs.is_empty() {
                continue;
            }
            let mut callees = Vec::with_capacity(node.described.built_in.len());
            // A tiered callee is reached through its slot too, but tiering it up changes its
            // code, not its MIR: the copy built in stays right.
            for &at in &node.described.built_in {
                match node.refs.get(at as usize).and_then(callee) {
                    Some(it) if nodes.get(&it).is_some_and(|it| it.patchable) => errors.push(error(
                        *key,
                        node.unit.item,
                        // alloc: an error
                        format!("builds in {it:?}, which is patchable: an edit patches it alone"),
                    )),
                    Some(it) => callees.push(it),
                    None => errors.push(error(
                        *key,
                        node.unit.item,
                        // alloc: an error
                        format!("builds in place {at}, which is not a function among its {} references", node.refs.len()),
                    )),
                }
            }
            direct.insert(*key, callees);
        }
        // alloc: per build, reused across its units
        let (mut seen, mut queue) = (FxHashSet::default(), Vec::new());
        for (key, node) in nodes.iter_mut() {
            seen.clear();
            queue.extend(direct.get(key).into_iter().flatten().copied());
            while let Some(it) = queue.pop() {
                if it != *key && seen.insert(it) {
                    queue.extend(direct.get(&it).into_iter().flatten().copied());
                }
            }
            // alloc: per build, what a unit builds in, kept by its node
            node.built_in = seen.iter().copied().collect();
            node.built_in.sort_unstable();
        }
        for node in statics.values_mut() {
            node.refs = resolve(&node.described.refs, &node.described.origin).unwrap_or_default();
        }
        // alloc: per build, sized by its roots
        let roots = roots.iter().filter_map(|it| asked.get(it).copied()).collect();
        (nodes, statics, roots)
    }

    /// Builds the IR of each of `keys`, on all cores.
    fn prepare_all(
        &self,
        nodes: &Sorted<InstanceKey, FnNode<B>>,
        keys: &[InstanceKey],
    ) -> Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)> {
        keys.par_iter()
            .map(|key| {
                let node = &nodes[key];
                let made = self.backend.prepare(self.frontend, &node.unit, &node.spec, &node.instance, &node.refs);
                (*key, made)
            })
            // alloc: per build, sized by its units
            .collect()
    }

    /// Compiles everything `roots` reach. Each root is an instance and whose code asks for it.
    pub fn run(
        &mut self,
        roots: &[(FrontInstance<B>, Origin)],
        store: &impl Lookup,
    ) -> Result<Built, Vec<CodegenError>> {
        let mut out = Out::default();
        let (nodes, statics, root_keys) = self.walk(roots, &mut out.errors);
        if !out.errors.is_empty() {
            return Err(out.finish().4);
        }
        out.stats.instances = nodes.len();
        out.stats.statics = statics.len();
        // alloc: per build, sized by its statics' references
        self.static_callees = statics.values().flat_map(|it| it.refs.iter().filter_map(callee)).collect();

        self.dev(&nodes, store, &mut out);
        let release: Vec<Job<'_, B>> = nodes
            .iter()
            .filter(|(_, node)| !node.spec.backend.is_cranelift())
            // clone: per build, a release job's unit (plain IDs)
            .map(|(key, node)| Job { name: *key, unit: node.unit.clone(), spec: &node.spec, node })
            // alloc: per build, sized by its units
            .collect();
        self.release(&nodes, &release, store, &mut out);
        self.statics(&statics, store, &mut out);

        let mergeable = |key: &InstanceKey| match nodes.get(key) {
            Some(node) => !node.external(),
            None => true,
        };
        let deduped = dedupe(&mut out.functions, &mergeable);
        out.statics.sort_by_key(|it| (it.item, it.instance));

        // clone: per build, the symbols the build gives the linker
        let built_symbols = |node: &FnNode<B>| node.described.symbol.clone();
        let symbols = nodes
            .iter()
            .filter_map(|(key, node)| Some((*key, built_symbols(node)?)))
            // alloc: per build, what it gives the linker
            .collect();
        let static_symbols = statics
            .iter()
            // clone: per build, the symbols the build gives the linker
            .filter_map(|((item, instance), node)| Some((*item, *instance, node.described.symbol.clone()?)))
            // alloc: per build, what it gives the linker
            .collect();
        // alloc: per build, what it gives the linker
        let patchable = nodes.iter().filter(|it| it.1.patchable).map(|it| *it.0).collect();
        // alloc: per build, what it gives the linker
        let tiered = nodes.iter().filter(|it| it.1.then.is_some()).map(|it| *it.0).collect();
        let (functions, compiled_statics, puts, stats, errors) = out.finish();
        self.nodes = nodes;
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(Built {
            functions,
            statics: compiled_statics,
            roots: root_keys,
            symbols,
            static_symbols,
            patchable,
            tiered,
            deduped,
            puts,
            stats,
        })
    }

    /// A live build's tiered functions at their last tier (#807): LLVM's code for the slots
    /// the build filled with Cranelift's. Nothing patchable is imported, as an edit replaces
    /// it alone.
    pub fn tier_up(&self, store: &impl Lookup) -> Result<TieredUp, Vec<CodegenError>> {
        let mut out = Out::default();
        let jobs: Vec<Job<'_, B>> = self
            .nodes
            .iter()
            .filter_map(|(key, node)| {
                let spec = node.then.as_ref()?;
                let instance = Instance { settings: spec.settings(), ..node.unit.instance };
                // clone: per tier-up, a job's unit (plain IDs)
                let unit = Unit { instance, backend: spec.backend, ..node.unit.clone() };
                Some(Job { name: *key, unit, spec, node })
            })
            // alloc: per tier-up, sized by its units
            .collect();
        let names: Vec<(InstanceKey, InstanceKey)> =
            // alloc: per tier-up, sized by its units
            jobs.iter().map(|job| (job.name, job.unit.instance.key())).collect();
        self.release(&self.nodes, &jobs, store, &mut out);
        let (functions, _, puts, stats, errors) = out.finish();
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut by_key: HashMap<InstanceKey, CompiledFunction> =
            // alloc: per tier-up, sized by its units
            functions.into_iter().map(|it| (it.key, it)).collect();
        let replaced = names
            .into_iter()
            .filter_map(|(name, key)| Some((name, by_key.remove(&key)?)))
            // alloc: per tier-up, sized by its units
            .collect();
        Ok(TieredUp { replaced, puts, stats })
    }

    fn dev(&self, nodes: &Sorted<InstanceKey, FnNode<B>>, store: &impl Lookup, out: &mut Out) {
        let dev: Vec<InstanceKey> = nodes
            .iter()
            .filter(|(_, node)| node.spec.backend.is_cranelift())
            .map(|(key, _)| *key)
            // alloc: per build, sized by its units
            .collect();
        let mut prepared: HashMap<InstanceKey, (Shape, B::Prepared)> = HashMap::new();

        // Folding: every candidate's masked shape, from the store or from its IR.
        // Each dev key at most once, from the store or from its IR; sorted by key below.
        // alloc: per build, sized by its units
        let mut masked: Vec<(InstanceKey, Option<Masked>)> = Vec::new();
        // alloc: per build, sized by its units
        let mut unmasked = Vec::new();
        for key in dev.iter().filter(|key| nodes[*key].spec.fold) {
            let unit = &nodes[key].unit;
            // alloc: an error
            let read = out.read(store, &keys::masked(&nodes[key].stored()), unit, |it| blob::decode_masked(it).map_err(|e| e.to_string()));
            match read {
                Read::Hit(it) => {
                    masked.push((*key, it));
                }
                Read::Miss => unmasked.push(*key),
                Read::Failed => {}
            }
        }
        for (key, made) in self.prepare_all(nodes, &unmasked) {
            match made {
                Ok((shape, made)) => {
                    let it = self.backend.masked(&nodes[&key].unit, &made, &nodes[&key].refs);
                    out.put(keys::masked(&nodes[&key].stored()), Kind::Query, blob::encode_masked(it.as_ref()));
                    masked.push((key, it));
                    prepared.insert(key, (shape, made));
                }
                Err(why) => out.errors.push(why),
            }
        }
        masked.sort_unstable_by_key(|it| it.0);
        // alloc: per build, sized by its units
        let mut candidates = Vec::new();
        for (key, it) in &masked {
            let (node, Some(it)) = (&nodes[key], it) else { continue };
            let places = node.refs.len();
            let beyond = it.constants.iter().find_map(|constant| match *constant {
                fold::Constant::Place(at) if at as usize >= places => Some(at),
                _ => None,
            });
            match beyond {
                Some(at) => out.errors.push(error(
                    *key,
                    node.unit.item,
                    // alloc: an error
                    format!("its masked IR names place {at} of its {places} references"),
                )),
                None => candidates.push((*key, node.unit.instance.settings, it, node.refs.as_slice())),
            }
        }
        let groups = fold::groups(&candidates, &fold::Limits::default());
        // alloc: per build, sized by its units
        let grouped: HashSet<InstanceKey> = groups.iter().flat_map(|it| it.members.iter().copied()).collect();

        // Every other dev unit: its template from the store, the memo, or a compile.
        // alloc: per build, sized by its units
        let mut missing = Vec::new();
        for key in dev.iter().filter(|key| !grouped.contains(*key)) {
            let node = &nodes[key];
            let read = out.read(store, &keys::template(&node.stored()), &node.unit, |it| {
                stored_function(it, *key, &node.refs)
            });
            match read {
                Read::Hit(made) => {
                    out.stats.stored += 1;
                    prepared.remove(key);
                    out.made(&node.unit, *key, made);
                }
                Read::Miss => missing.push(*key),
                Read::Failed => {
                    prepared.remove(key);
                }
            }
        }
        // alloc: per build, sized by its units
        let unprepared: Vec<InstanceKey> = missing.iter().filter(|it| !prepared.contains_key(*it)).copied().collect();
        for (key, made) in self.prepare_all(nodes, &unprepared) {
            match made {
                Ok(made) => {
                    prepared.insert(key, made);
                }
                Err(why) => out.errors.push(why),
            }
        }
        // One compile per shape: the first of a shape compiles, the rest wait for its template.
        // alloc: per build, sized by its units
        let mut to_emit = Vec::new();
        let mut first_of: HashMap<ContentId, InstanceKey> = HashMap::new();
        // alloc: per build, sized by its units
        let mut waiting: Vec<(InstanceKey, ContentId)> = Vec::new();
        for key in missing {
            let Some((shape, made)) = prepared.remove(&key) else { continue };
            let node = &nodes[&key];
            let shape = shape.key();
            match self.memo.get(&shape) {
                Some(template) => {
                    out.stats.memo += 1;
                    out.template(keys::template(&node.stored()), template);
                    out.instantiate(&node.unit, key, template, &node.refs);
                }
                None if first_of.contains_key(&shape) => waiting.push((key, shape)),
                None => {
                    first_of.insert(shape, key);
                    to_emit.push((key, shape, made));
                }
            }
        }
        let emitted: Vec<_> = to_emit
            .into_par_iter()
            .map(|(key, shape, made)| {
                let node = &nodes[&key];
                (key, shape, self.backend.emit(&node.unit, &node.spec, made, &node.refs, None))
            })
            // alloc: per build, sized by its units
            .collect();
        for (key, shape, emitted) in emitted {
            let node = &nodes[&key];
            let mut emitted = match emitted {
                Ok(it) if it.function.key == key => it,
                Ok(it) => {
                    // alloc: an error
                    out.errors.push(error(key, node.unit.item, format!("compiled as {:?}", it.function.key)));
                    continue;
                }
                Err(why) => {
                    out.errors.push(why);
                    continue;
                }
            };
            if let Some(it) = unkept_inline(nodes, &emitted.function, node.built_in.iter().copied()) {
                // alloc: an error
                out.errors.push(error(key, node.unit.item, format!("built in {it:?}, which its key does not hold")));
                continue;
            }
            out.stats.compiled += 1;
            out.side(&node.unit, &mut emitted);
            let Some(template) = out.template_of(&node.unit, emitted, &node.refs) else { continue };
            let kept = self.memo.put(shape, template);
            out.template(keys::template(&node.stored()), kept);
            out.instantiate(&node.unit, key, kept, &node.refs);
        }
        for (key, shape) in waiting {
            let node = &nodes[&key];
            match self.memo.get(&shape) {
                Some(template) => {
                    out.stats.memo += 1;
                    out.template(keys::template(&node.stored()), template);
                    out.instantiate(&node.unit, key, template, &node.refs);
                }
                None => out.errors.push(error(
                    key,
                    node.unit.item,
                    // alloc: an error
                    format!("not compiled: the compile of its shape, by {:?}, failed", first_of[&shape]),
                )),
            }
        }

        self.folded(nodes, &groups, prepared, store, out);
    }

    /// Each group's body and thunks, from the store or merged from the members' IR.
    fn folded(
        &self,
        nodes: &Sorted<InstanceKey, FnNode<B>>,
        groups: &[Group],
        mut prepared: HashMap<InstanceKey, (Shape, B::Prepared)>,
        store: &impl Lookup,
        out: &mut Out,
    ) {
        let thunk_refs = |group: &Group, member: &InstanceKey| group.thunk_refs(&nodes[member].refs);
        // alloc: per build, sized by its groups
        let mut merging = Vec::new();
        for group in groups {
            let key = group.key();
            let first = &nodes[&group.members[0]];
            // alloc: an error
            let decode = |it: &[u8]| blob::decode(it).map_err(|e| e.to_string());
            let body = out.read(store, &keys::folded(&key), &first.unit, decode);
            let mut thunks = Vec::with_capacity(group.members.len());
            let mut failed = matches!(body, Read::Failed);
            for member in &group.members {
                match out.read(store, &keys::thunk(&key, member), &nodes[member].unit, decode) {
                    Read::Hit(it) => thunks.push(it),
                    Read::Miss => {}
                    Read::Failed => failed = true,
                }
            }
            match body {
                _ if failed => {}
                Read::Hit(body) if thunks.len() == group.members.len() => {
                    out.stats.stored += 1 + thunks.len();
                    out.stats.folded += group.members.len();
                    out.instantiate(&first.unit, key, &body, &first.refs);
                    for (member, thunk) in group.members.iter().zip(&thunks) {
                        out.instantiate(&nodes[member].unit, *member, thunk, &thunk_refs(group, member));
                    }
                }
                _ => merging.push(group),
            }
        }
        let unprepared: Vec<InstanceKey> = merging
            .iter()
            .flat_map(|group| group.members.iter())
            .filter(|it| !prepared.contains_key(*it))
            .copied()
            // alloc: per build, sized by its groups
            .collect();
        for (key, made) in self.prepare_all(nodes, &unprepared) {
            match made {
                Ok(made) => {
                    prepared.insert(key, made);
                }
                Err(why) => out.errors.push(why),
            }
        }
        // alloc: per build, sized by its groups
        let mut jobs = Vec::new();
        for group in merging {
            let members: Option<Vec<_>> = group
                .members
                .iter()
                .map(|member| {
                    let node = &nodes[member];
                    let (_, made) = prepared.remove(member)?;
                    Some((&node.unit, &node.spec, made, node.refs.as_slice()))
                })
                // alloc: per merge, the members the backend takes
                .collect();
            // A member that failed to prepare is already an error of the build.
            if let Some(members) = members {
                jobs.push((group, members));
            }
        }
        let merged: Vec<_> =
            // alloc: per build, sized by its groups
            jobs.into_par_iter().map(|(group, members)| (group, self.backend.merge(group, members))).collect();
        for (group, merged) in merged {
            let key = group.key();
            let first = &nodes[&group.members[0]];
            let mut merged = match merged {
                Ok(it) => it,
                Err(why) => {
                    out.errors.push(why);
                    continue;
                }
            };
            if merged.body.function.key != key || merged.thunks.len() != group.members.len() {
                out.errors.push(error(key, first.unit.item, "a merge that is not of its group"));
                continue;
            }
            out.stats.compiled += 1 + merged.thunks.len();
            out.stats.folded += group.members.len();
            out.side(&first.unit, &mut merged.body);
            if let Some(body) = out.template_of(&first.unit, merged.body, &first.refs) {
                out.template(keys::folded(&key), &body);
                out.instantiate(&first.unit, key, &body, &first.refs);
            }
            for (member, mut thunk) in group.members.iter().zip(merged.thunks) {
                let node = &nodes[member];
                if thunk.function.key != *member {
                    out.errors.push(error(*member, node.unit.item, "a thunk of another member"));
                    continue;
                }
                out.side(&node.unit, &mut thunk);
                let refs = thunk_refs(group, member);
                if let Some(template) = out.template_of(&node.unit, thunk, &refs) {
                    out.template(keys::thunk(&key, member), &template);
                    out.instantiate(&node.unit, *member, &template, &refs);
                }
            }
        }
    }

    fn release(
        &self,
        nodes: &Sorted<InstanceKey, FnNode<B>>,
        jobs: &[Job<'_, B>],
        store: &impl Lookup,
        out: &mut Out,
    ) {
        if jobs.is_empty() {
            return;
        }
        // alloc: per build, sized by its units
        let by_name: HashMap<InstanceKey, &Job<'_, B>> = jobs.iter().map(|job| (job.name, job)).collect();
        let prepare = |names: &[InstanceKey]| -> Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)> {
            names
                .par_iter()
                .map(|name| {
                    let job = by_name[name];
                    let made = self.backend.prepare(self.frontend, &job.unit, job.spec, &job.node.instance, &job.node.refs);
                    (*name, made)
                })
                // alloc: per build, sized by its units
                .collect()
        };

        // Every summary: stored, else from the IR, which is kept for the compile.
        // Sorted by key once, in `imports::decide`.
        let mut summaries: FxHashMap<InstanceKey, Summary> =
            FxHashMap::with_capacity_and_hasher(jobs.len(), Default::default());
        let mut prepared: HashMap<InstanceKey, B::Prepared> = HashMap::new();
        let mut exports: HashMap<InstanceKey, B::Export> = HashMap::new();
        // alloc: per build, sized by its units
        let mut unsummarized = Vec::new();
        for job in jobs {
            let read = out.read(store, &keys::summary(&job.stored()), &job.unit, |it| {
                // alloc: an error
                serde_json::from_slice::<Summary>(it).map_err(|e| e.to_string())
            });
            match read {
                Read::Hit(summary) => {
                    summaries.insert(job.name, summary);
                }
                Read::Miss => unsummarized.push(job.name),
                Read::Failed => {}
            }
        }
        let summarize = |made: Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)>| {
            made.into_par_iter()
                .map(|(name, made)| {
                    let job = by_name[&name];
                    let summarized = made.and_then(|(_, mut made)| {
                        let (summary, export) =
                            self.backend.summarize(&job.unit, job.spec, &mut made, &job.node.refs)?;
                        Ok((summary, export, made))
                    });
                    (name, summarized)
                })
                // alloc: per build, sized by its units
                .collect::<Vec<_>>()
        };
        for (name, summarized) in summarize(prepare(&unsummarized)) {
            let job = by_name[&name];
            match summarized {
                Ok((summary, export, made)) => {
                    let bytes = serde_json::to_vec(&summary).expect("a summary is plain data");
                    out.put(keys::summary(&job.stored()), Kind::Query, bytes);
                    summaries.insert(name, summary);
                    exports.insert(name, export);
                    prepared.insert(name, made);
                }
                Err(why) => out.errors.push(why),
            }
        }
        // What only this build knows of a function: whether code outside it calls it, or
        // anything without a summary (another phase's unit, a static's value) refers to it.
        for (name, summary) in &mut summaries {
            summary.key = *name;
        }
        let outside: FxHashSet<InstanceKey> = jobs
            .iter()
            .filter(|job| job.node.external())
            .map(|job| job.name)
            .chain(
                nodes
                    .iter()
                    .filter(|(key, _)| !by_name.contains_key(*key))
                    .flat_map(|(_, node)| node.refs.iter().filter_map(callee)),
            )
            .chain(self.static_callees.iter().copied())
            // alloc: per build, sized by its units
            .collect();
        imports::close(&mut summaries, &outside);
        let decided = match imports::decide(&summaries, &imports::Limits::default()) {
            Ok(it) => it,
            Err(why) => {
                let job = &jobs[0];
                // alloc: an error
                out.errors.push(error(job.name, job.unit.item, format!("deciding the imports: {why}")));
                return;
            }
        };
        let patchable = |key: &InstanceKey| nodes.get(key).is_some_and(|node| node.patchable);
        let imports_of = |name: &InstanceKey| -> Imports {
            let mut imports = decided.get(name).cloned().unwrap_or_default();
            imports.imported.retain(|it| !patchable(it) && by_name.contains_key(it));
            imports.callees.retain(|it| !patchable(&it.0));
            imports.dead.retain(|it| !patchable(&it.0));
            imports
        };

        // Each compile by its blob key: stored, else compiled with its imports' exports.
        // alloc: per build, sized by its units
        let mut missing = Vec::new();
        for job in jobs {
            let imports = imports_of(&job.name);
            let imported: Vec<&[Target]> =
                // alloc: per release unit, sized by its imports
                imports.imported.iter().map(|it| by_name[it].node.refs.as_slice()).collect();
            let refs = release_refs(&job.node.refs, &imported);
            let key = job.unit.instance.key();
            let blob_key = keys::release(&imports.blob_key(job.stored()));
            match out.read(store, &blob_key, &job.unit, |it| stored_function(it, key, &refs)) {
                Read::Hit(made) => {
                    out.stats.stored += 1;
                    out.made(&job.unit, key, made);
                }
                Read::Miss => missing.push((job.name, imports, refs, blob_key)),
                Read::Failed => {}
            }
        }
        let mut needed: Vec<InstanceKey> = missing
            .iter()
            .flat_map(|(name, imports, _, _)| std::iter::once(name).chain(&imports.imported))
            .filter(|it| !prepared.contains_key(*it) && !exports.contains_key(*it))
            .copied()
            // alloc: per build, sized by its units
            .collect();
        needed.sort();
        needed.dedup();
        for (name, summarized) in summarize(prepare(&needed)) {
            match summarized {
                Ok((_, export, made)) => {
                    exports.insert(name, export);
                    prepared.insert(name, made);
                }
                Err(why) => out.errors.push(why),
            }
        }
        let exports = &exports;
        // alloc: per build, sized by its units
        let mut compiles = Vec::new();
        for (name, imports, refs, blob_key) in missing {
            if let Some(made) = prepared.remove(&name) {
                compiles.push((name, imports, refs, blob_key, made));
            }
        }
        let emitted: Vec<_> = compiles
            .into_par_iter()
            .map(|(name, imports, refs, blob_key, made)| {
                let job = by_name[&name];
                let list: Option<Vec<Import<'_, B::Export>>> = imports
                    .imported
                    .iter()
                    .map(|it| Some(Import { key: *it, export: exports.get(it)?, refs: &by_name[it].node.refs }))
                    // alloc: per release compile, sized by its imports
                    .collect();
                let emitted = match list {
                    Some(list) => {
                        self.backend.emit(&job.unit, job.spec, made, &refs, Some((&imports, list.as_slice())))
                    }
                    None => Err(error(job.unit.instance.key(), job.unit.item, "an import whose export was not made")),
                };
                (name, imports, refs, blob_key, emitted)
            })
            // alloc: per build, sized by its units
            .collect();
        for (name, imports, refs, blob_key, emitted) in emitted {
            let job = by_name[&name];
            let key = job.unit.instance.key();
            let mut emitted = match emitted {
                Ok(it) if it.function.key == key => it,
                Ok(it) => {
                    // alloc: an error
                    out.errors.push(error(key, job.unit.item, format!("compiled as {:?}", it.function.key)));
                    continue;
                }
                Err(why) => {
                    out.errors.push(why);
                    continue;
                }
            };
            out.stats.compiled += 1;
            out.stats.imported += imports.imported.len();
            out.side(&job.unit, &mut emitted);
            if let Some(template) = out.template_of(&job.unit, emitted, &refs) {
                out.template(blob_key, &template);
                out.instantiate(&job.unit, key, &template, &refs);
            }
        }
    }

    fn statics(&self, statics: &Sorted<StaticId, StaticNode<B>>, store: &impl Lookup, out: &mut Out) {
        let unit_of = |node: &StaticNode<B>| Unit {
            instance: Instance {
                code: blaze_items::CodeHash(node.described.value),
                args: node.described.instance,
                settings: node.spec.settings(),
            },
            backend: node.spec.backend,
            arch: self.policy.toolchain.arch,
            item: node.described.item,
        };
        // alloc: per build, sized by its statics
        let mut missing = Vec::new();
        for (id, node) in statics.iter() {
            let key = keys::static_value(&definition(&node.described), &node.spec.settings());
            let unit = unit_of(node);
            // alloc: an error
            match out.read(store, &key, &unit, |it| blob::decode_static(it).map_err(|e| e.to_string())) {
                Read::Hit(template) => {
                    out.stats.stored += 1;
                    match template.instantiate(id.0, id.1, &node.refs) {
                        Ok(value) => out.statics.push(value),
                        // alloc: an error
                        Err(why) => out.errors.push(error(unit.instance.key(), id.0, why.to_string())),
                    }
                }
                Read::Miss => missing.push((id, node, key)),
                Read::Failed => {}
            }
        }
        let compiled: Vec<_> = missing
            .into_par_iter()
            .map(|(id, node, key)| {
                let made = self.backend.compile_static(self.frontend, &node.spec, &node.value, &node.refs);
                (id, node, key, made)
            })
            // alloc: per build, sized by its statics
            .collect();
        for (id, node, key, made) in compiled {
            let unit = unit_of(node);
            let described = &node.described;
            let value = match made {
                Ok(it) if (it.item, it.instance) != *id => {
                    out.errors.push(error(unit.instance.key(), id.0, "compiled as another static"));
                    continue;
                }
                Ok(it)
                    if (it.mutable, it.thread_local, &it.section)
                        != (described.mutable, described.thread_local, &described.section) =>
                {
                    out.errors.push(error(
                        unit.instance.key(),
                        id.0,
                        "compiled with another mutability, thread-locality or section than the front end described",
                    ));
                    continue;
                }
                Ok(it) => it,
                Err(why) => {
                    out.errors.push(why);
                    continue;
                }
            };
            out.stats.compiled += 1;
            match StaticTemplate::of(&value, &node.refs) {
                Ok(template) => {
                    out.put(key, Kind::Code, blob::encode_static(&template));
                    out.statics.push(value);
                }
                // alloc: an error
                Err(why) => out.errors.push(error(unit.instance.key(), id.0, why.to_string())),
            }
        }
    }
}

pub const STATIC_DEFINITION_DOMAIN: &str = "blaze v2 static definition 1";

/// Everything a static's compiled definition is made of besides which static it is: what
/// its template is kept under.
fn definition<I, S>(described: &DescribedStatic<I, S>) -> ContentId {
    let mut hasher = Hasher::new(STATIC_DEFINITION_DOMAIN);
    hasher.id(&described.value).update(&[described.mutable as u8, described.thread_local as u8]);
    match &described.section {
        Some(section) => hasher.update(&[1]).part(section.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.finish()
}

/// The memo shared by every build of a daemon.
pub fn shared_memo() -> Arc<Memo> {
    // alloc: once a daemon
    Arc::new(Memo::new())
}

#[cfg(test)]
mod tests;
