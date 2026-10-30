//! The functions of each workspace's program as a Flecs world, as its builds' records say them,
//! the snapshot of it a page is sent first, and Flecs's REST over it (docs/web.md).
//!
//! What it holds:
//!
//! - a workspace: `Workspace { root, build }`, unnamed; in it (Flecs's `Parent`) each crate the
//!   build put a function of, `Crate`, named as the crate, and in a crate its modules, `Module`,
//!   named as the module, one in another as in the crate;
//! - a function: `Function { key, name, how, ns, build }`, unnamed, in the module its path is
//!   in ([`module_path`]: a
//!   type's path is a module here, as on the page), or the crate when it is in none, and
//!   `Callees`, the functions it calls;
//! - a module's `Callees`, the other modules its functions call, and `(Calls, crate)` on a
//!   crate one of whose functions calls one of that crate's. A call that is gone stays until
//!   the program is written again. Only crates' calls are pairs: anim's 150 000 calls of
//!   functions would hold 266 MB as pairs that fragment and 1.9 GB as pairs that do not, and
//!   with its 20 000 calls of modules as pairs and `ChildOf` for `Parent`, the daemon held
//!   318 MB after anim's build and two edits, against 163 MB.
//!
//! A function placed allocates nothing once its crate and module are known: its name goes to
//! Flecs through a reused buffer, its module is one hash of its path, and a call to an id not
//! yet named waits in a chain of its `(object, id)` rather than in a list every record scans.
//!
//! One thread has the world: [`World`] is not `Send`. Flecs is reached only through
//! [`crate::flecs`], so this module is safe code.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use blaze_protocol::events::{Event, How, Record};
use rustc_hash::FxHashMap;

pub use crate::flecs::Answer;
use crate::flecs::{Entity, Flecs, Tag};

/// What a record did to a program.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// It is in the program as the page is told it: the page is sent it.
    Told,
    /// It began the program again: the page is sent it anew.
    Anew,
}

/// A Flecs world of programs.
pub struct World {
    flecs: Flecs,
    scratch: Scratch,
}

/// Buffers reused by every record, so placing a function allocates nothing.
#[derive(Default)]
struct Scratch {
    /// A module's key in [`Program::modules`], and where each of its segments' keys ends.
    path: Vec<u8>,
    ends: Vec<usize>,
    /// A module's segments, as byte ranges of the function's name.
    segments: Vec<(usize, usize)>,
    callees: Vec<Entity>,
    placed: Vec<u32>,
    calls: Vec<(u32, u32)>,
    /// Calls of the record to ids none of its object's records has named yet: the id, the caller.
    unnamed: Vec<(u32, u32)>,
    /// Calls the record named the callee of: when each waited, the caller and the callee.
    named: Vec<(u32, u32, u32)>,
    changed: Vec<u32>,
    changed_modules: Vec<Entity>,
}

/// No call waits after it.
const END: u32 = u32::MAX;

/// A call to an id not yet named, in its `(object, id)`'s chain: 8 bytes.
#[derive(Clone, Copy)]
struct Waiting {
    from: u32,
    next: u32,
}

const _: () = assert!(size_of::<Waiting>() == 8);

/// The program a workspace builds, and the records of its last build.
pub struct Program {
    root: PathBuf,
    entity: Entity,
    build: u64,
    /// One more each time what the snapshot says may have changed.
    changes: u64,
    crates: Vec<String>,
    /// Each crate's entity, by its index in `crates`.
    crates_of: Vec<Entity>,
    crate_of: FxHashMap<String, (u32, Entity)>,
    /// The crate named last: a record's functions are mostly of one crate.
    last_crate: u32,
    /// Each module by its crate's index (4 bytes, little-endian) and then `ff` and a segment
    /// for each of its segments: a function's module is one lookup of its path, and each
    /// module above it is a prefix of that.
    modules: FxHashMap<Box<[u8]>, Entity>,
    /// Each function, in the snapshot's order.
    nodes: Vec<Node>,
    by_key: FxHashMap<u64, u32>,
    /// The nodes of each object of the build under way, by their ids in it.
    ids: FxHashMap<u32, FxHashMap<u32, u32>>,
    /// The nodes each node calls.
    out: Vec<Vec<u32>>,
    edges: usize,
    /// The modules that call one another, and the crates.
    calls: rustc_hash::FxHashSet<(Entity, Entity)>,
    /// The modules each module calls, as its `Callees`.
    module_calls: FxHashMap<Entity, Vec<Entity>>,
    /// Calls of the build's objects to ids none of its records has named yet: the first of
    /// each `(object, id)` ([`key`]), chained through `waiting` in the order they came. One to
    /// code of another is never named.
    unnamed: FxHashMap<u64, u32>,
    waiting: Vec<Waiting>,
    /// The records of the last build, but for its functions, each as the page is sent it: a
    /// CBOR sequence, encoded once.
    told: Vec<u8>,
    told_count: usize,
    /// Where the record told last begins in `told`, and how many have been told in all.
    told_last: usize,
    appended: u64,
}

struct Node {
    entity: Entity,
    krate: u32,
    /// The module it is in, or its crate.
    module: Entity,
}

/// What Flecs's REST answered.
pub struct Answer {
    pub code: u16,
    pub kind: String,
    pub body: Vec<u8>,
}

impl World {
    pub fn new() -> Result<World, String> {
        // SAFETY: a world of its own, and NUL-terminated strings.
        unsafe {
            let world = ecs::ecs_init();
            if world.is_null() {
                return Err("Flecs made no world".into());
            }
            let code = CString::new(KINDS).map_err(|e| e.to_string())?;
            if ecs::ecs_script_run(
                world,
                c"blaze-web".as_ptr(),
                code.as_ptr(),
                std::ptr::null_mut(),
            ) != 0
            {
                ecs::ecs_fini(world);
                return Err("Flecs did not take the build page's kinds".into());
            }
            let rest = ecs::glue_rest_new(world);
            if rest.is_null() {
                ecs::ecs_fini(world);
                return Err("Flecs made no REST server".into());
            }
            let named = |name: &CStr| ecs::ecs_lookup(world, name.as_ptr());
            Ok(World {
                world,
                rest,
                workspace: named(c"Workspace"),
                function: named(c"Function"),
                callees: named(c"Callees"),
                krate: named(c"Crate"),
                module: named(c"Module"),
                calls: named(c"Calls"),
                scratch: Scratch::default(),
            })
        }
    }

    /// A program of the workspace at `root`, with no functions yet.
    pub fn program(&mut self, root: &Path) -> Program {
        let entity = self.workspace_of(root, 0);
        Program {
            root: root.to_owned(),
            entity,
            build: 0,
            changes: 0,
            crates: Vec::new(),
            crates_of: Vec::new(),
            crate_of: FxHashMap::default(),
            last_crate: 0,
            modules: FxHashMap::default(),
            nodes: Vec::new(),
            by_key: FxHashMap::default(),
            ids: FxHashMap::default(),
            out: Vec::new(),
            edges: 0,
            calls: Default::default(),
            module_calls: FxHashMap::default(),
            unnamed: FxHashMap::default(),
            waiting: Vec::new(),
            told: Vec::new(),
            told_count: 0,
            told_last: 0,
            appended: 0,
        }
    }

    /// Takes the program out of the world.
    pub fn forget(&mut self, program: Program) {
        // SAFETY: the world's thread; children go with the workspace.
        unsafe { ecs::ecs_delete(self.world, program.entity) };
    }

    /// Takes in `record`, of build `build` of the program's workspace.
    pub fn apply(&mut self, program: &mut Program, build: u64, record: &Record) -> Applied {
        match &record.event {
            Event::Began { .. } => {
                program.build = build;
                program.changes += 1;
                program.ids.clear();
                program.unnamed.clear();
                program.waiting.clear();
                program.told.clear();
                program.told_count = 0;
                program.tell(record);
                self.set_workspace(program);
                Applied::Told
            }
            Event::Functions { object, whole, functions } => {
                program.changes += 1;
                let anew = *whole && !program.ids.contains_key(object);
                if anew {
                    self.forget_functions(program);
                }
                self.placed(program, *object, functions);
                if anew { Applied::Anew } else { Applied::Told }
            }
            Event::Finished { .. } => {
                program.unnamed.clear();
                program.waiting.clear();
                program.tell(record);
                Applied::Told
            }
            _ => {
                program.tell(record);
                Applied::Told
            }
        }
    }

    /// What Flecs's REST answers `method` of `path` (with its query), read only.
    pub fn rest(&mut self, method: &str, path: &str) -> Result<Answer, String> {
        let method = CString::new(method).map_err(|_| "a NUL in the method")?;
        let path = CString::new(path).map_err(|_| "a NUL in the path")?;
        let (mut body, mut len, mut kind) = (std::ptr::null_mut(), 0i32, std::ptr::null());
        // SAFETY: the world's thread, its server, NUL-terminated strings; the body is Flecs's
        // to free, once copied.
        unsafe {
            let code = ecs::glue_rest_request(
                self.rest,
                method.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                &mut body,
                &mut len,
                &mut kind,
            );
            let bytes = if body.is_null() || len <= 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(body.cast::<u8>(), len as usize).to_vec()
            };
            ecs::glue_free(body.cast::<c_void>());
            let kind = if kind.is_null() {
                "application/json".to_owned()
            } else {
                CStr::from_ptr(kind).to_string_lossy().into_owned()
            };
            let code = u16::try_from(code).map_err(|_| format!("Flecs answered {code}"))?;
            Ok(Answer { code, kind, body: bytes })
        }
    }

    /// The program in the layout of docs/web.md, "The snapshot".
    pub fn snapshot(&self, program: &Program) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + program.nodes.len() * 64 + program.edges * 8);
        out.extend(b"BLZW");
        out.extend(1u32.to_le_bytes());
        text(&mut out, program.root.to_string_lossy().as_bytes());
        out.extend(program.build.to_le_bytes());
        out.extend((program.crates.len() as u32).to_le_bytes());
        for krate in &program.crates {
            text(&mut out, krate.as_bytes());
        }
        out.extend((program.nodes.len() as u32).to_le_bytes());
        for node in &program.nodes {
            // SAFETY: the world's thread; every node has a Function.
            let function = unsafe {
                &*ecs::ecs_get_id(self.world, node.entity, self.function).cast::<Function>()
            };
            out.extend(function.key.to_le_bytes());
            out.extend(node.krate.to_le_bytes());
            out.push(function.how);
            out.extend(function.ns.to_le_bytes());
            out.extend(function.build.to_le_bytes());
            // SAFETY: Flecs's copy of the name, NUL-terminated.
            text(&mut out, unsafe { CStr::from_ptr(function.name) }.to_bytes());
        }
        out.extend((program.edges as u32).to_le_bytes());
        let mut sorted = Vec::new();
        for (from, calls) in program.out.iter().enumerate() {
            sorted.clear();
            sorted.extend_from_slice(calls);
            sorted.sort_unstable();
            for &to in &sorted {
                out.extend((from as u32).to_le_bytes());
                out.extend(to.to_le_bytes());
            }
        }
        out
    }

    fn workspace_of(&mut self, root: &Path, build: u64) -> Entity {
        // SAFETY: the world's thread.
        let entity = unsafe { ecs::ecs_new(self.world) };
        let root = c_text(&mut self.scratch.text, root.to_string_lossy().as_bytes());
        let value = Workspace { root, build };
        self.set(entity, self.workspace, &value);
        entity
    }

    fn set_workspace(&mut self, program: &Program) {
        let root = c_text(&mut self.scratch.text, program.root.to_string_lossy().as_bytes());
        let value = Workspace { root, build: program.build };
        self.set(program.entity, self.workspace, &value);
    }

    /// A build writes the whole program again: what it says is all of it.
    fn forget_functions(&mut self, program: &mut Program) {
        // SAFETY: the world's thread; children go with the workspace.
        unsafe { ecs::ecs_delete(self.world, program.entity) };
        program.entity = self.workspace_of(&program.root, program.build);
        program.crates.clear();
        program.crates_of.clear();
        program.crate_of.clear();
        program.last_crate = 0;
        program.modules.clear();
        program.nodes.clear();
        program.by_key.clear();
        // Another object's ids name nodes that are gone, as on the page, which starts over; that
        // it was said stays, so a whole record of it later is merged, not made anew again.
        for ids in program.ids.values_mut() {
            ids.clear();
        }
        program.out.clear();
        program.edges = 0;
        program.calls.clear();
        program.module_calls.clear();
        program.unnamed.clear();
        program.waiting.clear();
    }

    fn placed(
        &mut self,
        program: &mut Program,
        object: u32,
        functions: &[blaze_protocol::events::Function],
    ) {
        let mut placed = std::mem::take(&mut self.scratch.placed);
        placed.clear();
        placed.reserve(functions.len());
        for function in functions {
            let (krate, in_crate) = self.crate_of(program, &function.krate);
            let at = match program.by_key.get(&function.key) {
                Some(&at) => {
                    // What it calls is said again below.
                    program.nodes[at as usize].krate = krate;
                    program.edges -= program.out[at as usize].len();
                    program.out[at as usize].clear();
                    at
                }
                None => {
                    let parent =
                        self.module_of(program, krate, in_crate, &function.krate, &function.name);
                    // SAFETY: the world's thread.
                    let entity = unsafe { ecs::ecs_new(self.world) };
                    self.set_parent(entity, parent);
                    let at = program.nodes.len() as u32;
                    program.nodes.push(Node { entity, krate, module: parent });
                    program.out.push(Vec::new());
                    program.by_key.insert(function.key, at);
                    at
                }
            };
            let (how, ns) = match function.how {
                How::Compiled { ns } => (0u8, ns),
                How::Unit => (1, 0),
                How::Unchanged => (2, 0),
                How::Stored => (3, 0),
            };
            let name = c_text(&mut self.scratch.text, function.name.as_bytes());
            let value = Function { key: function.key, name, how, ns, build: program.build };
            self.set(program.nodes[at as usize].entity, self.function, &value);
            program.ids.entry(object).or_default().insert(function.id, at);
            placed.push(at);
        }
        let mut calls = std::mem::take(&mut self.scratch.calls);
        let mut unnamed = std::mem::take(&mut self.scratch.unnamed);
        let mut named = std::mem::take(&mut self.scratch.named);
        calls.clear();
        unnamed.clear();
        named.clear();
        let Some(ids) = program.ids.get(&object) else {
            self.scratch.placed = placed;
            self.scratch.calls = calls;
            self.scratch.unnamed = unnamed;
            self.scratch.named = named;
            return;
        };
        for (function, &from) in functions.iter().zip(&placed) {
            for &call in &function.calls {
                match ids.get(&call) {
                    Some(&to) => calls.push((from, to)),
                    None => unnamed.push((call, from)),
                }
            }
        }
        // The calls that waited for an id this record named, in the order they came: none of
        // this record's own waits, since every id it names is in `ids` already.
        for function in functions {
            let Some(mut at) = program.unnamed.remove(&key(object, function.id)) else {
                continue;
            };
            let to = ids[&function.id];
            while at != END {
                let waiting = program.waiting[at as usize];
                named.push((at, waiting.from, to));
                at = waiting.next;
            }
        }
        named.sort_unstable_by_key(|it| it.0);
        calls.extend(named.iter().map(|&(_, from, to)| (from, to)));
        for &(call, from) in &unnamed {
            let at = program.waiting.len() as u32;
            let first = program.unnamed.entry(key(object, call)).or_insert(END);
            program.waiting.push(Waiting { from, next: *first });
            *first = at;
        }
        let mut changed = std::mem::take(&mut self.scratch.changed);
        let mut changed_modules = std::mem::take(&mut self.scratch.changed_modules);
        changed.clear();
        changed.extend_from_slice(&placed);
        changed_modules.clear();
        for &(from, to) in &calls {
            let out = &mut program.out[from as usize];
            if out.contains(&to) {
                continue;
            }
            out.push(to);
            program.edges += 1;
            changed.push(from);
            let (from, to) = (&program.nodes[from as usize], &program.nodes[to as usize]);
            let crates =
                (program.crates_of[from.krate as usize], program.crates_of[to.krate as usize]);
            // A function at its crate's root is in no module: its calls are its crate's alone.
            let in_modules = from.module != crates.0 && to.module != crates.1;
            if in_modules
                && from.module != to.module
                && program.calls.insert((from.module, to.module))
            {
                program.module_calls.entry(from.module).or_default().push(to.module);
                changed_modules.push(from.module);
            }
            if crates.0 != crates.1 && program.calls.insert(crates) {
                // SAFETY: the world's thread.
                unsafe { ecs::ecs_add_id(self.world, crates.0, pair(self.calls, crates.1)) };
            }
        }
        changed.sort_unstable();
        changed.dedup();
        let mut callees = std::mem::take(&mut self.scratch.callees);
        for &at in &changed {
            callees.clear();
            let out = &program.out[at as usize];
            callees.extend(out.iter().map(|&to| program.nodes[to as usize].entity));
            self.set_callees(program.nodes[at as usize].entity, &callees);
        }
        changed_modules.sort_unstable();
        changed_modules.dedup();
        for &module in &changed_modules {
            self.set_callees(module, &program.module_calls[&module]);
        }
        self.scratch.placed = placed;
        self.scratch.calls = calls;
        self.scratch.unnamed = unnamed;
        self.scratch.named = named;
        self.scratch.changed = changed;
        self.scratch.changed_modules = changed_modules;
        self.scratch.callees = callees;
    }

    /// Sets `callees` as the `Callees` of `entity`: Flecs copies them.
    fn set_callees(&mut self, entity: Entity, callees: &[Entity]) {
        let vector = ecs::ecs_vec_t {
            array: callees.as_ptr().cast_mut().cast::<c_void>(),
            count: callees.len() as i32,
            size: callees.len() as i32,
        };
        self.set(entity, self.callees, &vector);
    }

    fn crate_of(&mut self, program: &mut Program, name: &str) -> (u32, Entity) {
        let last = program.last_crate as usize;
        if program.crates.get(last).is_some_and(|it| it == name) {
            return (last as u32, program.crates_of[last]);
        }
        if let Some(&it) = program.crate_of.get(name) {
            program.last_crate = it.0;
            return it;
        }
        let entity = self.named(program.entity, name, self.krate);
        let it = (program.crates.len() as u32, entity);
        program.crates.push(name.to_owned());
        program.crates_of.push(entity);
        program.crate_of.insert(name.to_owned(), it);
        program.last_crate = it.0;
        it
    }

    /// The module a function is in ([`module_path`]), under its crate, from the crate's name on.
    fn module_of(
        &mut self,
        program: &mut Program,
        krate: u32,
        in_crate: Entity,
        krate_name: &str,
        name: &str,
    ) -> Entity {
        let mut segments = std::mem::take(&mut self.scratch.segments);
        let mut path = std::mem::take(&mut self.scratch.path);
        let mut ends = std::mem::take(&mut self.scratch.ends);
        segments.clear();
        path.clear();
        ends.clear();
        let mut first = true;
        each_module(name, &mut |segment: &str| {
            if std::mem::take(&mut first) && segment == krate_name {
                return;
            }
            // Every segment is a slice of the name.
            let start = segment.as_ptr() as usize - name.as_ptr() as usize;
            segments.push((start, start + segment.len()));
        });
        path.extend(krate.to_le_bytes());
        for &(start, end) in &segments {
            path.push(0xff);
            path.extend_from_slice(&name.as_bytes()[start..end]);
            ends.push(path.len());
        }
        let found = match program.modules.get(&path[..]) {
            Some(&it) => Some(it),
            None if segments.is_empty() => Some(in_crate),
            None => None,
        };
        let at = found.unwrap_or_else(|| {
            let mut at = in_crate;
            for (&(start, end), &until) in segments.iter().zip(&ends) {
                at = match program.modules.get(&path[..until]) {
                    Some(&it) => it,
                    None => {
                        let it = self.named(at, &name[start..end], self.module);
                        program.modules.insert(path[..until].into(), it);
                        it
                    }
                };
            }
            at
        });
        self.scratch.segments = segments;
        self.scratch.path = path;
        self.scratch.ends = ends;
        at
    }

    fn named(&mut self, parent: Entity, name: &str, kind: Entity) -> Entity {
        // SAFETY: the world's thread.
        let entity = unsafe { ecs::ecs_new(self.world) };
        self.set_parent(entity, parent);
        let name = c_text(&mut self.scratch.text, name.as_bytes());
        // SAFETY: as above, and a NUL-terminated name Flecs copies.
        unsafe {
            ecs::ecs_set_name(self.world, entity, name);
            ecs::ecs_add_id(self.world, entity, kind);
        }
        entity
    }

    /// Puts `entity` in `parent` with Flecs's `Parent`, a `ChildOf` that does not fragment:
    /// with `(ChildOf, module)` the functions of each module are a table of their own, and anim
    /// has 5 000 modules.
    fn set_parent(&mut self, entity: Entity, parent: Entity) {
        // SAFETY: a constant Flecs sets once, before any world.
        let id = unsafe { ecs::FLECS_IDEcsParentID_ };
        self.set(entity, id, &Parent { value: parent });
    }

    /// Sets `value` as the component `id` of `entity`: Flecs copies it, its strings too.
    fn set<T>(&mut self, entity: Entity, id: Entity, value: &T) {
        // SAFETY: the world's thread, and `T` is laid out as the component (KINDS).
        unsafe {
            ecs::ecs_set_id(
                self.world,
                entity,
                id,
                size_of::<T>(),
                std::ptr::from_ref(value).cast::<c_void>(),
            )
        };
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // SAFETY: the world's thread, and nothing uses either after.
        unsafe {
            ecs::glue_rest_free(self.rest);
            ecs::ecs_fini(self.world);
        }
    }
}

impl Program {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn build(&self) -> u64 {
        self.build
    }

    /// One more each time what [`World::snapshot`] says of it may have changed.
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// The records of the last build, but for its functions, as the page is sent them: a
    /// CBOR sequence of [`Record`]s.
    pub fn told(&self) -> &[u8] {
        &self.told
    }

    /// How many records [`Program::told`] holds.
    pub fn told_count(&self) -> usize {
        self.told_count
    }

    /// How many records have been told in all, and the last of them, as the page is sent it.
    pub fn last_told(&self) -> (u64, &[u8]) {
        (self.appended, &self.told[self.told_last..])
    }

    pub fn functions(&self) -> usize {
        self.nodes.len()
    }

    pub fn calls(&self) -> usize {
        self.edges
    }

    fn tell(&mut self, record: &Record) {
        match blaze_protocol::cbor::to_vec(record) {
            Ok(item) => {
                self.told_last = self.told.len();
                self.told.extend_from_slice(&item);
                self.told_count += 1;
                self.appended += 1;
            }
            Err(error) => {
                eprintln!("build page: a record of {} does not encode: {error}", self.root.display())
            }
        }
    }
}

/// The chain of calls waiting for id `id` of object `object`.
fn key(object: u32, id: u32) -> u64 {
    (u64::from(object) << 32) | u64::from(id)
}

/// The path a function is in, from how a backtrace names it: its path but its last segment,
/// without generic arguments or closures, and of a method of `<T as Trait>` or `<T>`, `T`'s
/// path. As the page's `moduleOf` (assets/names.js), so the world's modules are the page's:
///
/// ```text
/// anim::scene::render                                  anim::scene
/// anim::main::{closure#0}                              anim
/// core::ptr::drop_in_place::<anim::Thing>              core::ptr
/// <alloc::vec::Vec<T> as core::ops::drop::Drop>::drop  alloc::vec::Vec
/// <&[u8] as core::fmt::Debug>::fmt                     (none: a slice has no path)
/// ```
pub fn module_path(name: &str) -> Vec<&str> {
    let mut out = Vec::new();
    each_module(name, &mut |it| out.push(it));
    out
}

/// Each segment of [`module_path`], in order, each a slice of `name`, with nothing allocated.
fn each_module<'a>(name: &'a str, f: &mut impl FnMut(&'a str)) {
    if name.starts_with('<') {
        if let Some(end) = closing(name, 0) {
            self_path(&name[1..end], f);
        }
        return;
    }
    // Every segment but the last.
    let mut held: Option<&'a str> = None;
    segments(name, &mut |it| {
        if let Some(before) = held.replace(it) {
            f(before);
        }
    });
}

/// Each piece of `name` between the `::`s outside `<>`, `()`, `[]` and `{}`. Every byte it
/// looks for is ASCII, which no byte of a longer UTF-8 character is.
fn split_path<'a>(name: &'a str, f: &mut impl FnMut(&'a str)) {
    let bytes = name.as_bytes();
    let (mut depth, mut start, mut last, mut at) = (0usize, 0, 0u8, 0);
    while at < bytes.len() {
        let c = bytes[at];
        match c {
            b'<' | b'(' | b'[' | b'{' => depth += 1,
            b'>' if last != b'-' => depth = depth.saturating_sub(1),
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b':') => {
                f(&name[start..at]);
                at += 1;
                start = at + 1;
            }
            _ => {}
        }
        last = c;
        at += 1;
    }
    f(&name[start..]);
}

/// The byte of the `>` that closes the `<` at `open`.
fn closing(name: &str, open: usize) -> Option<usize> {
    let (mut depth, mut last) = (0usize, 0u8);
    for (at, &c) in name.as_bytes()[open..].iter().enumerate() {
        if c == b'<' {
            depth += 1;
        } else if c == b'>' && last != b'-' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(open + at);
            }
        }
        last = c;
    }
    None
}

/// The path of `T` in `T as Trait`, or of `T`.
fn self_path<'a>(inner: &'a str, f: &mut impl FnMut(&'a str)) {
    let (mut depth, mut last) = (0usize, 0u8);
    for (at, &c) in inner.as_bytes().iter().enumerate() {
        match c {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if last != b'-' => depth = depth.saturating_sub(1),
            b')' | b']' => depth = depth.saturating_sub(1),
            b' ' if depth == 0 && inner[at..].starts_with(" as ") => {
                return type_path(&inner[..at], f);
            }
            _ => {}
        }
        last = c;
    }
    type_path(inner, f)
}

/// A type's path without its generic arguments, or none for a type that has none: a reference
/// or pointer is its pointee's, a slice, tuple or `fn` pointer has none.
fn type_path<'a>(ty: &'a str, f: &mut impl FnMut(&'a str)) {
    let mut ty = ty.trim();
    loop {
        let before = ty;
        for prefix in ["&mut ", "&", "*const ", "*mut ", "dyn "] {
            ty = ty.strip_prefix(prefix).unwrap_or(ty).trim_start();
        }
        if let Some(rest) = ty.strip_prefix('\'') {
            ty = rest.split_once(' ').map_or(ty, |(_, rest)| rest.trim_start());
        }
        if ty == before {
            break;
        }
    }
    if ty.starts_with('<') {
        if let Some(end) = closing(ty, 0) {
            self_path(&ty[1..end], f);
        }
        return;
    }
    let fn_pointer =
        ty.starts_with("fn(") || ty.starts_with("unsafe ") || ty.starts_with("extern ");
    if fn_pointer || !ty.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return;
    }
    segments(ty, f)
}

/// The named segments of a path: no `::<…>`, generic arguments or `{closure#0}`.
fn segments<'a>(path: &'a str, f: &mut impl FnMut(&'a str)) {
    split_path(path, &mut |it: &'a str| {
        if !it.is_empty() && !it.starts_with('<') && !it.starts_with('{') {
            f(it.find('<').map_or(it, |lt| &it[..lt]));
        }
    });
}

fn pair(relationship: Entity, target: Entity) -> ecs::ecs_id_t {
    ecs::ECS_PAIR | ((relationship & 0xFFFF_FFFF) << 32) | (target & 0xFFFF_FFFF)
}

fn text(out: &mut Vec<u8>, text: &[u8]) {
    out.extend((text.len() as u32).to_le_bytes());
    out.extend(text);
}

/// `text` without its NULs and with one at its end, in `into`, for Flecs to copy: valid until
/// `into` is next changed.
fn c_text(into: &mut Vec<u8>, text: &[u8]) -> *const c_char {
    into.clear();
    if text.contains(&0) {
        into.extend(text.iter().copied().filter(|&it| it != 0));
    } else {
        into.extend_from_slice(text);
    }
    into.push(0);
    into.as_ptr().cast()
}
