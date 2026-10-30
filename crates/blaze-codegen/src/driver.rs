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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use blaze_cas::{Key, Kind};
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use rayon::prelude::*;

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
    template::{StaticTemplate, Template},
};

/// The store as a build reads it.
pub trait Lookup {
    /// The verified bytes under `key`; `Err` names why what is there can't be read.
    fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, String>;
}

impl Lookup for blaze_cas::Store {
    fn get(&self, key: &Key) -> Result<Option<Vec<u8>>, String> {
        match blaze_cas::Store::get(self, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes.to_vec())),
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
}

impl<B: Prepare> FnNode<B> {
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

/// One compile of a release phase: `name` is the key callers' places and summaries name it
/// by, `unit` what is compiled (another key for a function tiered up).
struct Job<'n, B: Prepare> {
    name: InstanceKey,
    unit: Unit,
    spec: &'n Spec,
    node: &'n FnNode<B>,
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

    /// A compile's side table under its content ID.
    fn side(&mut self, unit: &Unit, emitted: &Emitted) {
        match (&emitted.side, emitted.function.side) {
            (Some(bytes), Some(id)) => self.put(keys::side(&id), Kind::Debug, bytes.clone()),
            (None, None) => {}
            _ => self.errors.push(error(
                unit.instance.key(),
                unit.item,
                "the compile's side table and the function's record of it disagree",
            )),
        }
    }

    /// Reads one value, or records why it can't.
    fn read<T>(
        &mut self,
        store: &dyn Lookup,
        key: &Key,
        unit: &Unit,
        decode: impl FnOnce(&[u8]) -> Result<T, String>,
    ) -> Read<T> {
        match store.get(key) {
            Ok(Some(bytes)) => match decode(&bytes) {
                Ok(it) => Read::Hit(it),
                Err(why) => {
                    self.errors.push(error(unit.instance.key(), unit.item, format!("the store's {key:?}: {why}")));
                    Read::Failed
                }
            },
            Ok(None) => Read::Miss,
            Err(why) => {
                self.errors.push(error(unit.instance.key(), unit.item, format!("reading the store's {key:?}: {why}")));
                Read::Failed
            }
        }
    }

    /// The function of `template` for `key` with `refs`, into the build's functions.
    fn instantiate(&mut self, unit: &Unit, key: InstanceKey, template: &Template, refs: &[Target]) {
        match template.instantiate(key, refs) {
            Ok(function) => self.functions.push(function),
            Err(why) => self.errors.push(error(key, unit.item, why.to_string())),
        }
    }

    /// `function`'s template against `refs`, or the error naming why it has none.
    fn template_of(&mut self, unit: &Unit, function: &CompiledFunction, refs: &[Target]) -> Option<Template> {
        match Template::of(function, refs) {
            Ok(it) => Some(it),
            Err(why) => {
                self.errors.push(error(function.key, unit.item, why.to_string()));
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

/// One build's codegen.
pub struct Build<'a, B: Prepare> {
    backend: &'a B,
    frontend: &'a B::Frontend,
    policy: &'a Policy,
    memo: &'a Memo,
    nodes: BTreeMap<InstanceKey, FnNode<B>>,
    /// Functions the last run's statics refer to.
    static_callees: BTreeSet<InstanceKey>,
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
        Build { backend, frontend, policy, memo, nodes: BTreeMap::new(), static_callees: BTreeSet::new() }
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
            instance: instance.clone(),
            described,
            refs: Vec::new(),
        })
    }

    fn static_node(
        &self,
        value: &FrontStatic<B>,
        described: DescribedStatic<FrontInstance<B>, FrontStatic<B>>,
    ) -> Result<StaticNode<B>, CodegenError> {
        let choice = self.policy.choose(&described.origin, None).map_err(|why| {
            error(InstanceKey(described.value), described.item, why.to_string())
        })?;
        Ok(StaticNode { value: value.clone(), described, spec: choice.first, refs: Vec::new() })
    }

    /// Every instance and static the roots reach, each with its places resolved.
    fn walk(
        &self,
        roots: &[(FrontInstance<B>, Origin)],
        errors: &mut Vec<CodegenError>,
    ) -> (BTreeMap<InstanceKey, FnNode<B>>, BTreeMap<StaticId, StaticNode<B>>, Vec<InstanceKey>) {
        let mut nodes: BTreeMap<InstanceKey, FnNode<B>> = BTreeMap::new();
        let mut statics: BTreeMap<StaticId, StaticNode<B>> = BTreeMap::new();
        let mut asked: HashMap<(FrontInstance<B>, Origin), InstanceKey> = HashMap::new();
        let mut static_asked: HashMap<FrontStatic<B>, StaticId> = HashMap::new();
        let mut queued: HashSet<(FrontInstance<B>, Origin)> = HashSet::new();
        let mut static_queued: HashSet<FrontStatic<B>> = HashSet::new();
        let mut level: Vec<(FrontInstance<B>, Origin)> =
            roots.iter().filter(|it| queued.insert((*it).clone())).cloned().collect();
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
                        let asked = (callee.clone(), requester.clone());
                        if queued.insert(asked.clone()) {
                            next.push(asked);
                        }
                    }
                    FrontRef::Static(value) => {
                        if static_queued.insert(value.clone()) {
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
                .collect();
            let described_statics: Vec<_> = static_level
                .par_iter()
                .map(|value| self.frontend.describe_static(value).and_then(|it| self.static_node(value, it)))
                .collect();
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
                let origin = node.described.origin.clone();
                enqueue(&node.described.refs, &origin, &mut queued, &mut static_queued, &mut next, &mut next_statics);
                statics.entry(id).or_insert(node);
            }
            level = next;
            static_level = next_statics;
        }

        let through_slot: HashMap<InstanceKey, bool> =
            nodes.iter().map(|(key, node)| (*key, node.patchable || node.then.is_some())).collect();
        let resolve = |refs: &[FrontRef<FrontInstance<B>, FrontStatic<B>>], requester: &Origin| {
            refs.iter()
                .map(|it| match it {
                    FrontRef::Function(callee) => {
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
                    FrontRef::Extern(name) => Some(Target::Extern(name.clone())),
                    FrontRef::Weak(name) => Some(Target::Weak(name.clone())),
                })
                .collect::<Option<Vec<_>>>()
        };
        // An instance one of whose references failed has no places; that failure is already
        // an error of the build.
        for node in nodes.values_mut() {
            node.refs = resolve(&node.described.refs, &node.described.origin).unwrap_or_default();
        }
        for node in statics.values_mut() {
            node.refs = resolve(&node.described.refs, &node.described.origin).unwrap_or_default();
        }
        let roots = roots.iter().filter_map(|it| asked.get(it).copied()).collect();
        (nodes, statics, roots)
    }

    /// Builds the IR of each of `keys`, on all cores.
    fn prepare_all(
        &self,
        nodes: &BTreeMap<InstanceKey, FnNode<B>>,
        keys: &[InstanceKey],
    ) -> Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)> {
        keys.par_iter()
            .map(|key| {
                let node = &nodes[key];
                let made = self.backend.prepare(self.frontend, &node.unit, &node.spec, &node.instance, &node.refs);
                (*key, made)
            })
            .collect()
    }

    /// Compiles everything `roots` reach. Each root is an instance and whose code asks for it.
    pub fn run(
        &mut self,
        roots: &[(FrontInstance<B>, Origin)],
        store: &dyn Lookup,
    ) -> Result<Built, Vec<CodegenError>> {
        let mut out = Out::default();
        let (nodes, statics, root_keys) = self.walk(roots, &mut out.errors);
        if !out.errors.is_empty() {
            return Err(out.finish().4);
        }
        out.stats.instances = nodes.len();
        out.stats.statics = statics.len();
        self.static_callees = statics.values().flat_map(|it| it.refs.iter().filter_map(callee)).collect();

        self.dev(&nodes, store, &mut out);
        let release: Vec<Job<'_, B>> = nodes
            .iter()
            .filter(|(_, node)| node.spec.backend != crate::Backend::Cranelift)
            .map(|(key, node)| Job { name: *key, unit: node.unit.clone(), spec: &node.spec, node })
            .collect();
        self.release(&nodes, &release, store, &mut out);
        self.statics(&statics, store, &mut out);

        let mergeable = |key: &InstanceKey| match nodes.get(key) {
            Some(node) => !node.external(),
            None => true,
        };
        let deduped = dedupe(&mut out.functions, &mergeable);
        out.statics.sort_by_key(|it| (it.item, it.instance));

        let built_symbols = |node: &FnNode<B>| node.described.symbol.clone();
        let symbols = nodes
            .iter()
            .filter_map(|(key, node)| Some((*key, built_symbols(node)?)))
            .collect();
        let static_symbols = statics
            .iter()
            .filter_map(|((item, instance), node)| Some((*item, *instance, node.described.symbol.clone()?)))
            .collect();
        let patchable = nodes.iter().filter(|it| it.1.patchable).map(|it| *it.0).collect();
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
    pub fn tier_up(&self, store: &dyn Lookup) -> Result<TieredUp, Vec<CodegenError>> {
        let mut out = Out::default();
        let jobs: Vec<Job<'_, B>> = self
            .nodes
            .iter()
            .filter_map(|(key, node)| {
                let spec = node.then.as_ref()?;
                let instance = Instance { settings: spec.settings(), ..node.unit.instance };
                let unit = Unit { instance, backend: spec.backend, ..node.unit.clone() };
                Some(Job { name: *key, unit, spec, node })
            })
            .collect();
        let names: Vec<(InstanceKey, InstanceKey)> =
            jobs.iter().map(|job| (job.name, job.unit.instance.key())).collect();
        self.release(&self.nodes, &jobs, store, &mut out);
        let (functions, _, puts, stats, errors) = out.finish();
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut by_key: HashMap<InstanceKey, CompiledFunction> =
            functions.into_iter().map(|it| (it.key, it)).collect();
        let replaced = names
            .into_iter()
            .filter_map(|(name, key)| Some((name, by_key.remove(&key)?)))
            .collect();
        Ok(TieredUp { replaced, puts, stats })
    }

    fn dev(&self, nodes: &BTreeMap<InstanceKey, FnNode<B>>, store: &dyn Lookup, out: &mut Out) {
        let dev: Vec<InstanceKey> = nodes
            .iter()
            .filter(|(_, node)| node.spec.backend == crate::Backend::Cranelift)
            .map(|(key, _)| *key)
            .collect();
        let mut prepared: HashMap<InstanceKey, (Shape, B::Prepared)> = HashMap::new();

        // Folding: every candidate's masked shape, from the store or from its IR.
        let mut masked: BTreeMap<InstanceKey, Option<Masked>> = BTreeMap::new();
        let mut unmasked = Vec::new();
        for key in dev.iter().filter(|key| nodes[*key].spec.fold) {
            let unit = &nodes[key].unit;
            let read = out.read(store, &keys::masked(key), unit, |it| blob::decode_masked(it).map_err(|e| e.to_string()));
            match read {
                Read::Hit(it) => {
                    masked.insert(*key, it);
                }
                Read::Miss => unmasked.push(*key),
                Read::Failed => {}
            }
        }
        for (key, made) in self.prepare_all(nodes, &unmasked) {
            match made {
                Ok((shape, made)) => {
                    let it = self.backend.masked(&nodes[&key].unit, &made);
                    out.put(keys::masked(&key), Kind::Query, blob::encode_masked(it.as_ref()));
                    masked.insert(key, it);
                    prepared.insert(key, (shape, made));
                }
                Err(why) => out.errors.push(why),
            }
        }
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
                    format!("its masked IR names place {at} of its {places} references"),
                )),
                None => candidates.push((*key, node.unit.instance.settings, it, node.refs.as_slice())),
            }
        }
        let groups = fold::groups(&candidates, &fold::Limits::default());
        let grouped: HashSet<InstanceKey> = groups.iter().flat_map(|it| it.members.iter().copied()).collect();

        // Every other dev unit: its template from the store, the memo, or a compile.
        let mut missing = Vec::new();
        for key in dev.iter().filter(|key| !grouped.contains(*key)) {
            let node = &nodes[key];
            let read = out.read(store, &keys::template(key), &node.unit, |it| blob::decode(it).map_err(|e| e.to_string()));
            match read {
                Read::Hit(template) => {
                    out.stats.stored += 1;
                    prepared.remove(key);
                    out.instantiate(&node.unit, *key, &template, &node.refs);
                }
                Read::Miss => missing.push(*key),
                Read::Failed => {
                    prepared.remove(key);
                }
            }
        }
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
        let mut to_emit = Vec::new();
        let mut first_of: HashMap<ContentId, InstanceKey> = HashMap::new();
        let mut waiting: Vec<(InstanceKey, ContentId)> = Vec::new();
        for key in missing {
            let Some((shape, made)) = prepared.remove(&key) else { continue };
            let node = &nodes[&key];
            let shape = shape.key(&node.unit);
            match self.memo.get(&shape) {
                Some(template) => {
                    out.stats.memo += 1;
                    out.template(keys::template(&key), &template);
                    out.instantiate(&node.unit, key, &template, &node.refs);
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
            .collect();
        for (key, shape, emitted) in emitted {
            let node = &nodes[&key];
            let emitted = match emitted {
                Ok(it) if it.function.key == key => it,
                Ok(it) => {
                    out.errors.push(error(key, node.unit.item, format!("compiled as {:?}", it.function.key)));
                    continue;
                }
                Err(why) => {
                    out.errors.push(why);
                    continue;
                }
            };
            out.stats.compiled += 1;
            out.side(&node.unit, &emitted);
            let Some(template) = out.template_of(&node.unit, &emitted.function, &node.refs) else { continue };
            let kept = self.memo.put(shape, template);
            out.template(keys::template(&key), &kept);
            out.instantiate(&node.unit, key, &kept, &node.refs);
        }
        for (key, shape) in waiting {
            let node = &nodes[&key];
            match self.memo.get(&shape) {
                Some(template) => {
                    out.stats.memo += 1;
                    out.template(keys::template(&key), &template);
                    out.instantiate(&node.unit, key, &template, &node.refs);
                }
                None => out.errors.push(error(
                    key,
                    node.unit.item,
                    format!("not compiled: the compile of its shape, by {:?}, failed", first_of[&shape]),
                )),
            }
        }

        self.folded(nodes, &groups, prepared, store, out);
    }

    /// Each group's body and thunks, from the store or merged from the members' IR.
    fn folded(
        &self,
        nodes: &BTreeMap<InstanceKey, FnNode<B>>,
        groups: &[Group],
        mut prepared: HashMap<InstanceKey, (Shape, B::Prepared)>,
        store: &dyn Lookup,
        out: &mut Out,
    ) {
        let thunk_refs = |group: &Group, member: &InstanceKey| group.thunk_refs(&nodes[member].refs);
        let mut merging = Vec::new();
        for group in groups {
            let key = group.key();
            let first = &nodes[&group.members[0]];
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
            .collect();
        for (key, made) in self.prepare_all(nodes, &unprepared) {
            match made {
                Ok(made) => {
                    prepared.insert(key, made);
                }
                Err(why) => out.errors.push(why),
            }
        }
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
                .collect();
            // A member that failed to prepare is already an error of the build.
            if let Some(members) = members {
                jobs.push((group, members));
            }
        }
        let merged: Vec<_> =
            jobs.into_par_iter().map(|(group, members)| (group, self.backend.merge(group, members))).collect();
        for (group, merged) in merged {
            let key = group.key();
            let first = &nodes[&group.members[0]];
            let merged = match merged {
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
            out.side(&first.unit, &merged.body);
            if let Some(body) = out.template_of(&first.unit, &merged.body.function, &first.refs) {
                out.template(keys::folded(&key), &body);
                out.instantiate(&first.unit, key, &body, &first.refs);
            }
            for (member, thunk) in group.members.iter().zip(&merged.thunks) {
                let node = &nodes[member];
                if thunk.function.key != *member {
                    out.errors.push(error(*member, node.unit.item, "a thunk of another member"));
                    continue;
                }
                out.side(&node.unit, thunk);
                let refs = thunk_refs(group, member);
                if let Some(template) = out.template_of(&node.unit, &thunk.function, &refs) {
                    out.template(keys::thunk(&key, member), &template);
                    out.instantiate(&node.unit, *member, &template, &refs);
                }
            }
        }
    }

    fn release(
        &self,
        nodes: &BTreeMap<InstanceKey, FnNode<B>>,
        jobs: &[Job<'_, B>],
        store: &dyn Lookup,
        out: &mut Out,
    ) {
        if jobs.is_empty() {
            return;
        }
        let by_name: HashMap<InstanceKey, &Job<'_, B>> = jobs.iter().map(|job| (job.name, job)).collect();
        let prepare = |names: &[InstanceKey]| -> Vec<(InstanceKey, Result<(Shape, B::Prepared), CodegenError>)> {
            names
                .par_iter()
                .map(|name| {
                    let job = by_name[name];
                    let made = self.backend.prepare(self.frontend, &job.unit, job.spec, &job.node.instance, &job.node.refs);
                    (*name, made)
                })
                .collect()
        };

        // Every summary: stored, else from the IR, which is kept for the compile.
        let mut summaries: BTreeMap<InstanceKey, Summary> = BTreeMap::new();
        let mut prepared: HashMap<InstanceKey, B::Prepared> = HashMap::new();
        let mut exports: HashMap<InstanceKey, B::Export> = HashMap::new();
        let mut unsummarized = Vec::new();
        for job in jobs {
            let read = out.read(store, &keys::summary(&job.unit.instance.key()), &job.unit, |it| {
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
                    let summarized = made.and_then(|(_, made)| {
                        let (summary, export) = self.backend.summarize(&job.unit, job.spec, &made, &job.node.refs)?;
                        Ok((summary, export, made))
                    });
                    (name, summarized)
                })
                .collect::<Vec<_>>()
        };
        for (name, summarized) in summarize(prepare(&unsummarized)) {
            let job = by_name[&name];
            match summarized {
                Ok((summary, export, made)) => {
                    let bytes = serde_json::to_vec(&summary).expect("a summary is plain data");
                    out.put(keys::summary(&job.unit.instance.key()), Kind::Query, bytes);
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
        let outside: BTreeSet<InstanceKey> = jobs
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
            .collect();
        imports::close(&mut summaries, &outside);
        let decided = match imports::decide(&summaries, &imports::Limits::default()) {
            Ok(it) => it,
            Err(why) => {
                let job = &jobs[0];
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
        let mut missing = Vec::new();
        for job in jobs {
            let imports = imports_of(&job.name);
            let imported: Vec<&[Target]> =
                imports.imported.iter().map(|it| by_name[it].node.refs.as_slice()).collect();
            let refs = release_refs(&job.node.refs, &imported);
            let key = job.unit.instance.key();
            let blob_key = keys::release(&imports.blob_key(key));
            match out.read(store, &blob_key, &job.unit, |it| blob::decode(it).map_err(|e| e.to_string())) {
                Read::Hit(template) => {
                    out.stats.stored += 1;
                    out.instantiate(&job.unit, key, &template, &refs);
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
                    .collect();
                let emitted = match list {
                    Some(list) => {
                        self.backend.emit(&job.unit, job.spec, made, &refs, Some((&imports, list.as_slice())))
                    }
                    None => Err(error(job.unit.instance.key(), job.unit.item, "an import whose export was not made")),
                };
                (name, imports, refs, blob_key, emitted)
            })
            .collect();
        for (name, imports, refs, blob_key, emitted) in emitted {
            let job = by_name[&name];
            let key = job.unit.instance.key();
            let emitted = match emitted {
                Ok(it) if it.function.key == key => it,
                Ok(it) => {
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
            out.side(&job.unit, &emitted);
            if let Some(template) = out.template_of(&job.unit, &emitted.function, &refs) {
                out.template(blob_key, &template);
                out.instantiate(&job.unit, key, &template, &refs);
            }
        }
    }

    fn statics(&self, statics: &BTreeMap<StaticId, StaticNode<B>>, store: &dyn Lookup, out: &mut Out) {
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
        let mut missing = Vec::new();
        for (id, node) in statics {
            let key = keys::static_value(&definition(&node.described), &node.spec.settings());
            let unit = unit_of(node);
            match out.read(store, &key, &unit, |it| blob::decode_static(it).map_err(|e| e.to_string())) {
                Read::Hit(template) => {
                    out.stats.stored += 1;
                    match template.instantiate(id.0, id.1, &node.refs) {
                        Ok(value) => out.statics.push(value),
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
    Arc::new(Memo::new())
}

#[cfg(test)]
mod tests;
