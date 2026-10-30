//! A live build that compiles lazily (`blaze-lazy`): the walk describes every instance as a
//! whole build does, and every one that is not compiled ahead passes the same checks a whole
//! build's compile would put it through before the run starts, so a lazy build fails where a
//! whole build fails. Only the entry and the functions an initializer or finalizer section
//! names are compiled ahead; the rest wait for [`Build::compile_one`] on their first call.
//!
//! A function's checks are its front end's: describing it (its MIR monomorphized, its
//! constants evaluated) and building its IR (its layouts). A function whose template the store
//! has passed them in the build that kept it, under the same key, so the store's bytes are
//! read and its IR is not built again. A function whose IR is built for its check keeps its
//! shape, so its first call takes the memo's template without building it again. What only the
//! backend's emission finds (a backend's own failure, never a program's) is found on the
//! function's first call, or by the lazy runner's idle compiles, and named then.
//!
//! Folding (#1292) is a whole build's: a lazy function is compiled alone, as its own template.

use blaze_cas::Kind;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_mem::{Tag, frame};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    Build, Built, FnNode, FrontInstance, FrontStatic, Lookup, Out, Put, Sorted, StaticId, StaticNode, callee,
    error, stored_function, unkept_inline,
};
use crate::{
    Backend, CodegenError, CompiledFunction, InstanceKey, backend::Prepare, blob, plan::Mode, plan::Origin,
    query::keys, template::Template,
};

/// A function of a lazy build, as its walk reached it: what `blaze_lazy::plan::Plan` is made
/// of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Walked {
    pub key: InstanceKey,
    pub code: CodeHash,
    pub item: ItemKey,
}

/// Whether a static in `section` runs code at the start or the end of the process or of a
/// thread: an initializer or finalizer array, as each object format names it. The functions
/// such a static names are compiled ahead.
pub fn runs_at_start(section: &str) -> bool {
    let elf = |name: &str| section == name || section.strip_prefix(name).is_some_and(|it| it.starts_with('.'));
    elf(".init_array")
        || elf(".preinit_array")
        || elf(".fini_array")
        || elf(".ctors")
        || elf(".dtors")
        || section.contains("__mod_init_func")
        || section.contains("__mod_term_func")
        || section.starts_with(".CRT$X")
}

/// What a lazy run leaves for its build and its [`Built`].
#[derive(Default)]
pub(super) struct Lazily {
    pub(super) walked: Vec<Walked>,
    pub(super) entry: Vec<InstanceKey>,
    /// The shape of each function whose IR was built for its check.
    pub(super) shapes: FxHashMap<InstanceKey, ContentId>,
}

/// Where a compile's template came from.
enum Made {
    Memo,
    Compiled,
}

fn nothing() -> (InstanceKey, ItemKey) {
    (InstanceKey(ContentId::default()), ItemKey(ContentId::default()))
}

/// `key`'s node, if a lazy build may compile it: a function of the walk at a dev backend.
fn lazy_node<B: Prepare>(nodes: &Sorted<InstanceKey, FnNode<B>>, key: InstanceKey) -> Result<&FnNode<B>, CodegenError> {
    let Some(node) = nodes.get(&key) else {
        return Err(error(key, nothing().1, "compiled lazily, but no function of the build's walk"));
    };
    dev_only(key, node)?;
    Ok(node)
}

/// A lazy function is compiled alone on its first call, which a release unit can't be: its
/// imports are decided from the whole build's summaries.
fn dev_only<B: Prepare>(key: InstanceKey, node: &FnNode<B>) -> Result<(), CodegenError> {
    match node.spec.backend {
        Backend::Cranelift { .. } | Backend::Stencil => Ok(()),
        backend @ Backend::Llvm { .. } => Err(error(
            key,
            node.unit.item,
            // alloc: an error
            format!(
                "a lazy build compiles each function alone on its first call, and this one's first \
                 code is {backend:?}'s, whose imports need the whole build; a live build starts it \
                 at Cranelift"
            ),
        )),
    }
}

/// `key`'s function from the store's template, if it has one.
fn stored_one<B: Prepare, L: Lookup + ?Sized>(
    node: &FnNode<B>,
    key: InstanceKey,
    store: &L,
) -> Result<Option<CompiledFunction>, CodegenError> {
    let at = keys::template(&node.stored());
    match store.read(&at) {
        Ok(None) => Ok(None),
        Ok(Some(bytes)) => match stored_function(bytes, key, &node.refs) {
            Ok(Ok(function)) => Ok(Some(function)),
            // alloc: an error
            Ok(Err(why)) => Err(error(key, node.unit.item, why.to_string())),
            // alloc: an error
            Err(why) => Err(error(key, node.unit.item, format!("the store's {at:?}: {why}"))),
        },
        // alloc: an error
        Err(why) => Err(error(key, node.unit.item, format!("reading the store's {at:?}: {why}"))),
    }
}

/// `key`'s function from the memo's `template`, with the template's [`Put`] under its key.
fn memoized<B: Prepare>(
    node: &FnNode<B>,
    key: InstanceKey,
    template: &Template,
) -> Result<(CompiledFunction, Vec<Put>), CodegenError> {
    // alloc: an error
    let function = template.instantiate(key, &node.refs).map_err(|why| error(key, node.unit.item, why.to_string()))?;
    let at = keys::template(&node.stored());
    // alloc: the store's value of a memo hit, once a function
    Ok((function, vec![Put { key: at, kind: Kind::Code, bytes: blob::encode(template) }]))
}

impl<B: Prepare> Build<'_, B> {
    /// [`Build::run_with_statics`] for a live build that compiles lazily (`later`): every
    /// instance the roots reach is walked and checked, the statics are compiled, and of the
    /// functions only the entry (the roots at the places `entry` gives) and what a static in
    /// an initializer or finalizer section names ([`runs_at_start`]). [`Built::walked`] lists
    /// every function in the walk's order, [`Built::entry`] the entry's keys; the rest are
    /// compiled by [`Build::compile_one`]. A build whose output is a file is compiled whole,
    /// so with [`Mode::Output`] it is an error.
    pub fn run_later(
        &mut self,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        entry: &[usize],
        store: &impl Lookup,
    ) -> Result<Built, Vec<CodegenError>> {
        if self.policy.mode == Mode::Output {
            let (unit, item) = nothing();
            let why = "a lazy build (`later`) of a build whose output is a file, which is compiled whole";
            return Err(vec![error(unit, item, why)]); // alloc: an error
        }
        self.run_as(roots, static_roots, Some(entry), store)
    }

    /// One function of the last run's walk, as a lazy run asks for it: the store's template,
    /// else the memo's by its shape, else compiled at its dev backend. What it compiles is not
    /// kept: [`Build::compile_kept`] gives the store's values too.
    pub fn compile_one<L: Lookup + ?Sized>(&self, key: InstanceKey, store: &L) -> Result<CompiledFunction, CodegenError> {
        self.compile_kept(key, store).map(|it| it.0)
    }

    /// [`Build::compile_one`], with what the caller puts in the store: the template under the
    /// function's key and its side table, none for a template the store had.
    pub fn compile_kept<L: Lookup + ?Sized>(
        &self,
        key: InstanceKey,
        store: &L,
    ) -> Result<(CompiledFunction, Vec<Put>), CodegenError> {
        let node = lazy_node(&self.nodes, key)?;
        if let Some(function) = stored_one(node, key, store)? {
            // alloc: none, empty
            return Ok((function, Vec::new()));
        }
        self.fresh(&self.nodes, &self.shapes, node, key).map(|(function, puts, _)| (function, puts))
    }

    /// The memo's function for `key`, as [`Build::compile_kept`] would give it from the memo:
    /// the instance and its template's [`Put`]. `None` when `key` is no function of the last
    /// run's walk at a dev backend, its check recorded no shape (it was compiled ahead or the
    /// store had it), or the memo has no template of that shape. A probe for the thread that
    /// reads a lazy run's asks: it reads no store, builds no IR and takes no lock but the
    /// memo's. An `Err` is the instantiation's, which `compile_kept` gives too.
    pub fn memo_hit(&self, key: InstanceKey) -> Option<Result<(CompiledFunction, Vec<Put>), CodegenError>> {
        let node = lazy_node(&self.nodes, key).ok()?;
        self.recorded_hit(&self.shapes, node, key)
    }

    /// The memo's template of the shape `key`'s check recorded, instantiated.
    fn recorded_hit(
        &self,
        shapes: &FxHashMap<InstanceKey, ContentId>,
        node: &FnNode<B>,
        key: InstanceKey,
    ) -> Option<Result<(CompiledFunction, Vec<Put>), CodegenError>> {
        let template = self.memo.get(shapes.get(&key)?)?;
        Some(memoized(node, key, template))
    }

    /// `key`'s function where the store has none: the memo's by its shape (the one its check
    /// recorded, else its IR's), else compiled.
    fn fresh(
        &self,
        nodes: &Sorted<InstanceKey, FnNode<B>>,
        shapes: &FxHashMap<InstanceKey, ContentId>,
        node: &FnNode<B>,
        key: InstanceKey,
    ) -> Result<(CompiledFunction, Vec<Put>, Made), CodegenError> {
        let item = node.unit.item;
        let at = keys::template(&node.stored());
        let from_memo = |made: Result<(CompiledFunction, Vec<Put>), CodegenError>| made.map(|(f, puts)| (f, puts, Made::Memo));
        if let Some(hit) = self.recorded_hit(shapes, node, key) {
            return from_memo(hit);
        }
        let (shape, made) = self.backend.prepare(self.frontend, &node.unit, &node.spec, &node.instance, &node.refs)?;
        let shape = shape.key();
        if let Some(template) = self.memo.get(&shape) {
            return from_memo(memoized(node, key, template));
        }
        let emitted = self.backend.emit(&node.unit, &node.spec, made, &node.refs, None)?;
        if emitted.function.key != key {
            // alloc: an error
            return Err(error(key, item, format!("compiled as {:?}", emitted.function.key)));
        }
        if let Some(it) = unkept_inline(nodes, &emitted.function, node.built_in.iter().copied()) {
            // alloc: an error
            return Err(error(key, item, format!("built in {it:?}, which its key does not hold")));
        }
        // alloc: the store's values of a compile: its template and its side table
        let mut puts = Vec::with_capacity(2);
        match (emitted.side, emitted.function.side) {
            (Some(bytes), Some(id)) => puts.push(Put { key: keys::side(&id), kind: Kind::Debug, bytes }),
            (None, None) => {}
            _ => return Err(error(key, item, "the compile's side table and the function's record of it disagree")),
        }
        let template = Template::of_owned(emitted.function, &node.refs, &emitted.extra)
            // alloc: an error
            .map_err(|why| error(key, item, why.to_string()))?;
        let kept = self.memo.put(shape, template);
        puts.push(Put { key: at, kind: Kind::Code, bytes: blob::encode(kept) });
        // alloc: an error
        let function = kept.instantiate(key, &node.refs).map_err(|why| error(key, item, why.to_string()))?;
        Ok((function, puts, Made::Compiled))
    }

    /// A lazy run's functions, once the walk ended with no error: each checked, the entry's
    /// and the start sections' compiled into `out`. `order` is every function's key in the
    /// walk's order; `root_keys` the roots', in their order.
    pub(super) fn later(
        &self,
        nodes: &Sorted<InstanceKey, FnNode<B>>,
        statics: &Sorted<StaticId, StaticNode<B>>,
        root_keys: &[InstanceKey],
        entry: &[usize],
        order: &[InstanceKey],
        store: &impl Lookup,
        out: &mut Out,
    ) -> Lazily {
        let mut entry_keys = Vec::with_capacity(entry.len());
        for &at in entry {
            match root_keys.get(at) {
                Some(key) => entry_keys.push(*key),
                None => {
                    let (unit, item) = nothing();
                    // alloc: an error
                    out.errors.push(error(unit, item, format!("the entry is root {at}, of {} roots", root_keys.len())));
                }
            }
        }
        for (key, node) in nodes.iter() {
            if let Err(why) = dev_only(*key, node) {
                out.errors.push(why);
            }
        }
        if !out.errors.is_empty() {
            return Lazily::default();
        }

        let started = statics
            .values()
            .filter(|it| it.described.section.as_deref().is_some_and(runs_at_start))
            .flat_map(|it| it.refs.iter().filter_map(callee));
        // alloc: per build, its entry and start functions
        let ahead: FxHashSet<InstanceKey> = entry_keys.iter().copied().chain(started).collect();
        // alloc: per build, its entry and start functions
        let mut ahead: Vec<InstanceKey> = ahead.into_iter().collect();
        ahead.sort_unstable();

        // The checks of every function compiled later: a template the store has, else its IR.
        // The store is read on this thread, its bytes decoded in place on all cores.
        // alloc: per build, sized by its units
        let (mut unchecked, mut stored) = (Vec::new(), Vec::new());
        for key in order.iter().filter(|it| ahead.binary_search(*it).is_err()) {
            let node = &nodes[key];
            let at = keys::template(&node.stored());
            match store.read(&at) {
                Ok(Some(bytes)) => stored.push((*key, node, at, bytes)),
                Ok(None) => unchecked.push(*key),
                // alloc: an error
                Err(why) => out.errors.push(error(*key, node.unit.item, format!("reading the store's {at:?}: {why}"))),
            }
        }
        let unread: Vec<CodegenError> = stored
            .par_iter()
            .filter_map(|(key, node, at, bytes)| {
                let why = frame::task(Tag::Codegen, |frame| blob::decode_view(bytes, frame).err())?;
                // alloc: an error
                Some(error(*key, node.unit.item, format!("the store's {at:?}: {why}")))
            })
            // alloc: none but for the errors
            .collect();
        out.errors.extend(unread);
        let checked: Vec<(InstanceKey, Result<ContentId, CodegenError>)> =
            self.until_stopped(unchecked.par_iter(), &mut out.errors, |key| {
                let node = &nodes[key];
                let made = self.backend.prepare(self.frontend, &node.unit, &node.spec, &node.instance, &node.refs);
                // The IR is let go here, on the thread that built it: only its shape is kept.
                (*key, made.map(|(shape, _)| shape.key()))
            });
        let mut shapes = FxHashMap::with_capacity_and_hasher(checked.len(), Default::default());
        for (key, shape) in checked {
            match shape {
                Ok(shape) => {
                    shapes.insert(key, shape);
                }
                Err(why) => out.errors.push(why),
            }
        }

        // The functions compiled ahead: the store's on this thread, the rest on all cores.
        // alloc: per build, its entry and start functions
        let mut missing = Vec::new();
        for key in &ahead {
            let read = lazy_node(nodes, *key).and_then(|node| Ok((node, stored_one(node, *key, store)?)));
            match read {
                Ok((node, Some(function))) => {
                    out.stats.stored += 1;
                    out.push(node.unit.instance.code, function);
                }
                Ok((node, None)) => missing.push((*key, node)),
                Err(why) => out.errors.push(why),
            }
        }
        let compiled = self.until_stopped(missing.par_iter(), &mut out.errors, |(key, node)| {
            (node.unit.instance.code, self.fresh(nodes, &shapes, node, *key))
        });
        for (code, made) in compiled {
            match made {
                Ok((function, puts, how)) => {
                    match how {
                        Made::Memo => out.stats.memo += 1,
                        Made::Compiled => out.stats.compiled += 1,
                    }
                    out.push(code, function);
                    out.puts.extend(puts);
                }
                Err(why) => out.errors.push(why),
            }
        }

        let walked = order
            .iter()
            .map(|key| {
                let node = &nodes[key];
                Walked { key: *key, code: node.unit.instance.code, item: node.unit.item }
            })
            // alloc: per build, what it gives the lazy runner
            .collect();
        Lazily { walked, entry: entry_keys, shapes }
    }
}
