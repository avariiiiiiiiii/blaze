//! Native inputs: C objects, archives and shared libraries, and the names they and Rust
//! code share.
//!
//! Every stage resolves from scratch, as lld would for the same inputs: objects are loaded
//! in link-line order, archive members are pulled for what is strongly undefined (lld's
//! rule: wherever the archive is on the line), the first COMDAT group of a name wins, a
//! strong definition beats a weak one beats a common one, and sections nothing reachable
//! refers to are dropped (`--gc-sections`). What it gives is then compared with the image:
//! only sections that came or went, and symbols whose definition changed, are touched.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use blaze_ids::radix;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::model::{
    Binding, Class, Directive, Fixup, Import, Library, NativeFile, Object, SectionKind, Shared, Special,
    Sym, SymbolDef, SymbolKind,
};
use crate::stage::{Stage, error};
use crate::state::{Def, Input, PieceId, Resolved, What};
use crate::symbols::{Obj, SymKey, Symbols};
use crate::unwind;
use crate::{Change, LinkError, NativeKind, OutputKind};

/// Symbols as one bit each, a bitset per symbol shard: insert and lookup O(1), and iteration
/// in ascending [`Sym`] order (shard, then place in it), the order of a `BTreeSet<Sym>`, also
/// between inserts. 1 bit per place of a shard up to its last member.
#[derive(Clone)]
struct SymBits {
    shards: Vec<Vec<u64>>,
}

impl Default for SymBits {
    fn default() -> Self {
        SymBits { shards: vec![Vec::new(); Symbols::SHARDS] }
    }
}

impl SymBits {
    /// Whether `sym` was not in yet.
    fn insert(&mut self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        let words = &mut self.shards[shard];
        if at / 64 >= words.len() {
            words.resize(at / 64 + 1, 0);
        }
        let bit = 1u64 << (at % 64);
        let new = words[at / 64] & bit == 0;
        words[at / 64] |= bit;
        new
    }

    fn contains(&self, sym: Sym) -> bool {
        let (shard, at) = Symbols::split(sym);
        self.shards[shard].get(at / 64).is_some_and(|word| word >> (at % 64) & 1 != 0)
    }

    fn extend(&mut self, syms: impl IntoIterator<Item = Sym>) {
        for sym in syms {
            self.insert(sym);
        }
    }

    fn iter(&self) -> impl Iterator<Item = Sym> + '_ {
        self.shards.iter().enumerate().flat_map(|(shard, words)| {
            words.iter().enumerate().flat_map(move |(word, &bits)| {
                let mut bits = bits;
                std::iter::from_fn(move || {
                    if bits == 0 {
                        return None;
                    }
                    let bit = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    Some(Symbols::join(shard, word * 64 + bit))
                })
            })
        })
    }
}

/// The names Rust pieces refer to, and of them those some reference to is strong.
#[derive(Default)]
struct Wanted {
    all: SymBits,
    strong: SymBits,
}

impl Wanted {
    /// `sym` wanted strongly, unless it is wanted already.
    fn or_strong(&mut self, sym: Sym) {
        if self.all.insert(sym) {
            self.strong.insert(sym);
        }
    }

    /// Whether every reference to `sym` is weak, if it is wanted.
    fn weak(&self, sym: Sym) -> Option<bool> {
        self.all.contains(sym).then(|| !self.strong.contains(sym))
    }
}

/// Sections kept whatever refers to them, by name, as lld keeps them.
fn kept_by_name(name: &str) -> bool {
    [".init", ".fini", ".ctors", ".dtors", ".jcr", ".preinit_array", ".init_array", ".fini_array"]
        .iter()
        .any(|it| name == *it || name.strip_prefix(it).is_some_and(|rest| rest.starts_with('.')))
        || name.starts_with(".note")
}

struct Loaded {
    obj: Obj,
    object: Arc<Object>,
    input: usize,
    /// Each symbol's image symbol.
    syms: Vec<Sym>,
}

/// Where a name is defined among the natives.
#[derive(Clone, Copy)]
enum Candidate {
    Section { load: usize, symbol: u32, weak: bool },
    Absolute { value: u64, tls: bool, weak: bool },
    Common,
}

/// What an archive member is.
enum Pulled {
    Object(Arc<Object>),
    /// An import library's member: one symbol of a DLL.
    Shared(Arc<Shared>),
}

/// Whether a candidate is in a section a COMDAT group of its name dropped.
fn dead(loads: &[Loaded], discarded: &FxHashSet<(usize, u32)>, candidate: &Candidate) -> bool {
    let Candidate::Section { load, symbol, .. } = candidate else { return false };
    matches!(
        loads[*load].object.symbols[*symbol as usize].def,
        SymbolDef::Section { section, .. } if discarded.contains(&(*load, section))
    )
}

/// The section and offset a global is defined at: its candidate's, or through the weak
/// externals it falls back to (`aliases`), the one it falls back to.
fn defined_in(
    loads: &[Loaded],
    candidates: &FxHashMap<Sym, Candidate>,
    aliases: &FxHashMap<Sym, (usize, u32)>,
    mut sym: Sym,
) -> Option<(usize, u32, u64)> {
    let at = |load: usize, symbol: u32| match loads[load].object.symbols[symbol as usize].def {
        SymbolDef::Section { section, value, .. } => Some((load, section, value)),
        _ => None,
    };
    for _ in 0..64 {
        match candidates.get(&sym) {
            Some(Candidate::Section { load, symbol, .. }) => return at(*load, *symbol),
            Some(_) => return None,
            None => {
                let (load, symbol) = *aliases.get(&sym)?;
                let it = &loads[load].object.symbols[symbol as usize];
                if it.binding == Binding::Local || it.kind == SymbolKind::Section {
                    return at(load, symbol);
                }
                sym = loads[load].syms[symbol as usize];
            }
        }
    }
    None
}

impl<'a> Stage<'a> {
    /// The native inputs the change adds and removes.
    pub fn inputs(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        if change.natives.is_empty() && change.removed_natives.is_empty() {
            return Ok(());
        }
        let inputs = Arc::make_mut(&mut self.new.inputs);
        inputs.retain(|it| !change.removed_natives.contains(&it.id));
        let mut fresh = Vec::new();
        for native in &change.natives {
            match inputs.iter_mut().find(|it| it.id == native.id) {
                Some(it) => it.order = native.order,
                None => fresh.push(native),
            }
        }
        let writer = self.writer;
        let files: Vec<Result<NativeFile, LinkError>> = fresh
            .par_iter()
            .map(|native| {
                writer
                    .read(native.name, native.bytes, &native.kind)
                    .map_err(|it| error(None, format!("{}: {it}", native.name)))
            })
            .collect();
        for (native, file) in fresh.into_iter().zip(files) {
            let file = file?;
            inputs.push(Input {
                id: native.id,
                name: native.name.into(),
                kind: native.kind,
                order: native.order,
                bytes: native.bytes.into(),
                file: Arc::new(file),
            });
        }
        inputs.sort_by_key(|it| (it.order, it.id));
        let names: FxHashSet<&str> = inputs.iter().map(|it| &*it.name).collect();
        for input in inputs.iter() {
            if let NativeFile::Shared(shared) = &*input.file {
                for wanted in &shared.inputs {
                    if !names.contains(wanted.as_str()) && !names.iter().any(|it| it.ends_with(&format!("/{wanted}"))) {
                        return Err(error(
                            None,
                            format!("{} names {wanted}, which the link line does not have", input.name),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn load(&mut self, loads: &mut Vec<Loaded>, input: usize, member: u32, object: Arc<Object>) {
        let obj = Obj { native: self.new.inputs[input].id, member };
        let keys: Vec<SymKey> = object
            .symbols
            .iter()
            .enumerate()
            .map(|(at, symbol)| match (symbol.binding, symbol.kind, symbol.def) {
                (_, SymbolKind::Section, SymbolDef::Section { section, .. }) => SymKey::Section(obj, section),
                (Binding::Local, _, _) => SymKey::Local(obj, at as u32),
                _ => SymKey::Name(symbol.name.as_str().into()),
            })
            .collect();
        let syms = self.symbols.intern_all(keys);
        loads.push(Loaded { obj, object, input, syms });
    }

    fn member(&mut self, input: usize, member: u32) -> Result<Pulled, LinkError> {
        let it = &self.new.inputs[input];
        let obj = Obj { native: it.id, member: member + 1 };
        if let Some(object) = self.objects.get(&obj) {
            return Ok(Pulled::Object(object.clone()));
        }
        let NativeFile::Archive(archive) = &*it.file else { unreachable!("a member of an archive") };
        let entry = &archive.members[member as usize];
        let bytes = it.bytes.get(entry.bytes.clone()).ok_or_else(|| {
            error(None, format!("{}: member {} past the end of the archive", it.name, entry.name))
        })?;
        let name = format!("{}({})", it.name, entry.name);
        let file = self
            .writer
            .read(&name, bytes, &NativeKind::Object)
            .map_err(|what| error(None, format!("{name}: {what}")))?;
        match file {
            NativeFile::Object(object) => {
                let object = Arc::new(object);
                self.objects.insert(obj, object.clone());
                Ok(Pulled::Object(object))
            }
            NativeFile::Shared(shared) => Ok(Pulled::Shared(Arc::new(shared))),
            NativeFile::Archive(_) => Err(error(None, format!("{name}: an archive in an archive"))),
        }
    }

    /// Loads a member pulled from an archive: an object, or an import library's member,
    /// which adds its DLL's symbol to the shared libraries.
    fn pull(
        &mut self,
        loads: &mut Vec<Loaded>,
        shared: &mut Vec<Arc<Shared>>,
        input: usize,
        member: u32,
    ) -> Result<(), LinkError> {
        match self.member(input, member)? {
            Pulled::Object(object) => self.load(loads, input, member + 1, object),
            Pulled::Shared(it) => shared.push(it),
        }
        Ok(())
    }

    /// The names Rust pieces refer to, with whether every reference is weak.
    fn wanted(&self) -> Wanted {
        let mut out = Wanted::default();
        for sym in self.new.seen.iter() {
            let mut weak = true;
            let mut any = false;
            for id in self.new.refs(*sym) {
                let Some(piece) = self.new.piece(*id) else { continue };
                if matches!(piece.what, What::Native { .. } | What::NativeFrame { .. } | What::Got { .. } | What::Plt { .. }) {
                    continue;
                }
                any = true;
                weak &= piece.weak.contains(sym);
            }
            if any {
                out.all.insert(*sym);
                if !weak {
                    out.strong.insert(*sym);
                }
            }
        }
        out
    }

    /// Loads, resolves, collects and places the natives, and defines every name.
    pub fn resolve(&mut self) -> Result<(), LinkError> {
        let mut wanted = self.wanted();
        let mut roots: Vec<Sym> = wanted.all.iter().collect();
        let mut extra: Vec<String> = Vec::new();
        let entry = match &self.output.kind {
            OutputKind::Executable { entry } => Some(entry.clone()),
            OutputKind::Shared { .. } => self.writer.shared_entry().map(str::to_owned),
        };
        extra.extend(entry.clone());
        if let OutputKind::Shared { exports, .. } = &self.output.kind {
            extra.extend(exports.iter().cloned());
        }
        extra.extend(["_init".to_owned(), "_fini".to_owned()]);
        for name in &extra {
            let sym = self.sym(SymKey::Name(name.as_str().into()));
            roots.push(sym);
        }
        let entry_name = entry.as_ref().map(|it| self.sym(SymKey::Name(it.as_str().into())));
        if let Some(entry) = entry_name {
            wanted.or_strong(entry);
        }
        if let OutputKind::Shared { exports, .. } = &self.output.kind {
            for name in exports.clone() {
                let sym = self.sym(SymKey::Name(name.as_str().into()));
                wanted.or_strong(sym);
            }
        }

        let mut loads: Vec<Loaded> = Vec::new();
        let mut pulled_shared: Vec<Arc<Shared>> = Vec::new();
        let mut archives: Vec<(usize, FxHashMap<&str, u32>)> = Vec::new();
        let inputs = self.new.inputs.clone();
        for (at, input) in inputs.iter().enumerate() {
            match &*input.file {
                NativeFile::Object(object) => {
                    let object = self
                        .objects
                        .entry(Obj { native: input.id, member: 0 })
                        .or_insert_with(|| Arc::new(object.clone()))
                        .clone();
                    self.load(&mut loads, at, 0, object);
                }
                NativeFile::Archive(archive) => {
                    if matches!(input.kind, NativeKind::Archive { whole: true }) {
                        for member in 0..archive.members.len() as u32 {
                            self.pull(&mut loads, &mut pulled_shared, at, member)?;
                        }
                    } else {
                        let mut index = FxHashMap::default();
                        for (name, member) in &archive.index {
                            index.entry(name.as_str()).or_insert(*member);
                        }
                        archives.push((at, index));
                    }
                }
                NativeFile::Shared(_) => {}
            }
        }

        let mut rust_names: FxHashMap<Sym, Sym> = FxHashMap::default();
        for (name, target) in self.new.names.clone().iter() {
            let sym = self.sym(SymKey::Name(name.as_str().into()));
            rust_names.insert(sym, *target);
        }

        // COMDAT, candidates and archive pulling, to a fixed point.
        let mut groups: FxHashMap<String, (usize, usize)> = FxHashMap::default();
        let mut discarded: FxHashSet<(usize, u32)> = FxHashSet::default();
        let mut candidates: FxHashMap<Sym, Candidate> = FxHashMap::default();
        // BTreeMap: walked in name order where it makes the commons' symbols.
        let mut commons: BTreeMap<Box<str>, (u64, u64)> = BTreeMap::new();
        let mut undefined_strong = SymBits::default();
        let mut pulled: FxHashSet<(usize, u32)> = FxHashSet::default();
        // Sorted by symbol once, where the aliases are made.
        let mut fallbacks: FxHashMap<Sym, (usize, u32)> = FxHashMap::default();
        // Names the link must define whatever refers to them: `/INCLUDE`, `/EXPORT`, what
        // weak externals fall back to, `__imp_X`'s `X`.
        let mut required: FxHashSet<Sym> = FxHashSet::default();
        let mut exports_asked: Vec<(String, String, bool)> = Vec::new();
        let mut mismatches: FxHashMap<String, (String, usize)> = FxHashMap::default();
        let mut native_imports: FxHashMap<Sym, blaze_codegen::wasm::Import> = FxHashMap::default();
        let mut done = 0;
        loop {
            while done < loads.len() {
                let load = &loads[done];
                for (index, group) in load.object.groups.iter().enumerate() {
                    let size = |load: &Loaded, group: &crate::model::Group| {
                        group.sections.first().map_or(0, |it| load.object.sections[*it as usize].data.len())
                    };
                    match groups.get(&group.name).copied() {
                        None => {
                            groups.insert(group.name.clone(), (done, index));
                        }
                        Some((winner, _)) if winner == done => {}
                        Some((winner, at)) => {
                            let old = &loads[winner].object.groups[at];
                            if group.largest && size(load, group) > size(&loads[winner], old) {
                                discarded.extend(old.sections.iter().map(|it| (winner, *it)));
                                groups.insert(group.name.clone(), (done, index));
                            } else {
                                discarded.extend(group.sections.iter().map(|it| (done, *it)));
                            }
                        }
                    }
                }
                for directive in &load.object.directives {
                    match directive {
                        Directive::Include(name) => {
                            let sym = self.sym(SymKey::Name(name.as_str().into()));
                            undefined_strong.insert(sym);
                            required.insert(sym);
                            roots.push(sym);
                        }
                        Directive::Export { name, symbol, data } => {
                            let sym = self.sym(SymKey::Name(symbol.as_str().into()));
                            undefined_strong.insert(sym);
                            required.insert(sym);
                            roots.push(sym);
                            if !exports_asked.iter().any(|it| it.0 == *name) {
                                exports_asked.push((name.clone(), symbol.clone(), *data));
                            }
                        }
                        Directive::Mismatch { key, value } => match mismatches.get(key) {
                            Some((first, by)) if first != value => {
                                return Err(error(
                                    None,
                                    format!(
                                        "`{key}` is `{first}` in {} and `{value}` in {} (/FAILIFMISMATCH)",
                                        self.new.inputs[loads[*by].input].name,
                                        self.new.inputs[load.input].name
                                    ),
                                ));
                            }
                            Some(_) => {}
                            None => {
                                mismatches.insert(key.clone(), (value.clone(), done));
                            }
                        },
                        Directive::NoDefaultLib(_) => {}
                    }
                }
                for (at, symbol) in load.object.symbols.iter().enumerate() {
                    if symbol.binding == Binding::Local {
                        continue;
                    }
                    let sym = load.syms[at];
                    let weak = symbol.binding == Binding::Weak;
                    if let Some(import) = &symbol.import {
                        native_imports.entry(sym).or_insert_with(|| import.clone());
                    }
                    let candidate = match symbol.def {
                        SymbolDef::Undefined => {
                            if !weak {
                                undefined_strong.insert(sym);
                            }
                            continue;
                        }
                        SymbolDef::Fallback { symbol, search } => {
                            if search {
                                undefined_strong.insert(sym);
                            }
                            fallbacks.entry(sym).or_insert((done, symbol));
                            continue;
                        }
                        SymbolDef::Section { section, .. } => {
                            if discarded.contains(&(done, section)) {
                                continue;
                            }
                            Candidate::Section { load: done, symbol: at as u32, weak }
                        }
                        SymbolDef::Absolute(value) => Candidate::Absolute { value, tls: symbol.kind == SymbolKind::Tls, weak },
                        SymbolDef::Common { size, align } => {
                            let it = commons.entry(symbol.name.as_str().into()).or_insert((0, 1));
                            it.0 = it.0.max(size);
                            it.1 = it.1.max(align);
                            Candidate::Common
                        }
                    };
                    if rust_names.contains_key(&sym) {
                        if !weak && !matches!(candidate, Candidate::Common) {
                            let input = &self.new.inputs[load.input];
                            return Err(error(
                                None,
                                format!(
                                    "duplicate symbol `{}`: Rust code defines it, and so does {}",
                                    symbol.name, input.name
                                ),
                            ));
                        }
                        continue;
                    }
                    let strength = |it: &Candidate| match it {
                        Candidate::Section { weak: false, .. } | Candidate::Absolute { weak: false, .. } => 2,
                        Candidate::Section { .. } | Candidate::Absolute { .. } => 1,
                        Candidate::Common => 0,
                    };
                    match candidates.get(&sym).filter(|it| !dead(&loads, &discarded, it)) {
                        None => {
                            candidates.insert(sym, candidate);
                        }
                        Some(old) if strength(&candidate) > strength(old) => {
                            candidates.insert(sym, candidate);
                        }
                        Some(old) if strength(&candidate) == 2 && strength(old) == 2 => {
                            let first = match old {
                                Candidate::Section { load, .. } => self.new.inputs[loads[*load].input].name.to_string(),
                                _ => "an absolute definition".to_owned(),
                            };
                            return Err(error(
                                None,
                                format!(
                                    "duplicate symbol `{}`: in {first} and in {}",
                                    symbol.name, self.new.inputs[load.input].name
                                ),
                            ));
                        }
                        Some(_) => {}
                    }
                }
                done += 1;
            }
            let mut pull = Vec::new();
            for sym in wanted.strong.iter().chain(undefined_strong.iter()) {
                if candidates.contains_key(&sym) || rust_names.contains_key(&sym) {
                    continue;
                }
                let SymKey::Name(name) = self.symbols.key(sym) else { continue };
                for (input, index) in &archives {
                    if let Some(member) = index.get(&**name) {
                        if pulled.insert((*input, *member)) {
                            pull.push((*input, *member));
                        }
                        break;
                    }
                }
            }
            if pull.is_empty() {
                break;
            }
            for (input, member) in pull {
                self.pull(&mut loads, &mut pulled_shared, input, member)?;
            }
        }
        candidates.retain(|_, it| !dead(&loads, &discarded, it));

        // Weak externals the link left undefined: each is the symbol it falls back to.
        let mut aliases: FxHashMap<Sym, (usize, u32)> = FxHashMap::default();
        let mut fallbacks: Vec<(Sym, (usize, u32))> = fallbacks.into_iter().collect();
        radix::sort_copy_by_key(&mut fallbacks, |it| it.0.0);
        for (sym, (load, symbol)) in &fallbacks {
            if !candidates.contains_key(sym) && !rust_names.contains_key(sym) {
                aliases.insert(*sym, (*load, *symbol));
            }
        }
        for (sym, (load, symbol)) in &aliases {
            let mut seen = FxHashSet::default();
            let mut at = (*load, *symbol);
            loop {
                let it = &loads[at.0].object.symbols[at.1 as usize];
                let next = loads[at.0].syms[at.1 as usize];
                if it.binding == Binding::Local || it.kind == SymbolKind::Section || !seen.insert(next) {
                    if seen.contains(sym) {
                        let SymKey::Name(name) = self.symbols.key(*sym) else { unreachable!("a global") };
                        return Err(error(None, format!("the weak external `{name}` falls back to itself")));
                    }
                    break;
                }
                match aliases.get(&next) {
                    Some(then) => at = *then,
                    None => break,
                }
            }
            required.insert(loads[*load].syms[*symbol as usize]);
        }

        // What every loaded symbol is defined in: (load, section, value).
        let section_of = |loads: &[Loaded], candidates: &FxHashMap<Sym, Candidate>, load: usize, symbol: u32| -> Option<(usize, u32, u64)> {
            let it = &loads[load].object.symbols[symbol as usize];
            if it.binding == Binding::Local || it.kind == SymbolKind::Section {
                return match it.def {
                    SymbolDef::Section { section, value, .. } => Some((load, section, value)),
                    _ => None,
                };
            }
            defined_in(loads, candidates, &aliases, loads[load].syms[symbol as usize])
        };
        // A kept COMDAT group lives or goes whole: each section's group's sections.
        let mut group_of: FxHashMap<(usize, u32), (usize, usize)> = FxHashMap::default();
        for (load, it) in loads.iter().enumerate() {
            for (index, group) in it.object.groups.iter().enumerate() {
                for section in &group.sections {
                    if !discarded.contains(&(load, *section)) {
                        group_of.insert((load, *section), (load, index));
                    }
                }
            }
        }

        // Collection: mark from the roots.
        let specials_by_section: FxHashMap<String, Vec<(usize, u32)>> = {
            let mut out: FxHashMap<String, Vec<(usize, u32)>> = FxHashMap::default();
            for (load, it) in loads.iter().enumerate() {
                for (at, section) in it.object.sections.iter().enumerate() {
                    if matches!(section.what, SectionKind::Alloc(Class::Named { .. }, _)) {
                        out.entry(section.name.clone()).or_default().push((load, at as u32));
                    }
                }
            }
            out
        };
        let mut marked: FxHashSet<(usize, u32)> = FxHashSet::default();
        let mut work: Vec<(usize, u32)> = Vec::new();
        let mut named_done: FxHashSet<String> = FxHashSet::default();
        let mark_name = |this: &Self, sym: Sym, work: &mut Vec<(usize, u32)>, named_done: &mut FxHashSet<String>| {
            if let Some((load, section, _)) = defined_in(&loads, &candidates, &aliases, sym) {
                work.push((load, section));
            }
            if let SymKey::Name(name) = this.symbols.key(sym) {
                match this.writer.special(name) {
                    Some(Special::Section { name: section, .. }) => {
                        if named_done.insert(section.clone()) {
                            work.extend(specials_by_section.get(&section).into_iter().flatten().copied());
                        }
                    }
                    Some(Special::Got(target)) => {
                        let target = this.symbols.find(&SymKey::Name(target.as_str().into()));
                        if let Some((load, section, _)) =
                            target.and_then(|it| defined_in(&loads, &candidates, &aliases, it))
                        {
                            work.push((load, section));
                        }
                    }
                    _ => {}
                }
            }
        };
        for sym in &roots {
            mark_name(self, *sym, &mut work, &mut named_done);
        }
        for (load, it) in loads.iter().enumerate() {
            for (at, section) in it.object.sections.iter().enumerate() {
                if discarded.contains(&(load, at as u32)) {
                    continue;
                }
                let root = section.retain
                    || kept_by_name(&section.name)
                    || matches!(section.what, SectionKind::Alloc(Class::InitArray | Class::FiniArray, _));
                if root && !matches!(section.what, SectionKind::Dropped | SectionKind::EhFrame) {
                    work.push((load, at as u32));
                }
            }
        }
        let mut frames: Vec<(usize, u32, Vec<unwind::Native>)> = Vec::new();
        for (load, it) in loads.iter().enumerate() {
            for (at, section) in it.object.sections.iter().enumerate() {
                if section.what == SectionKind::EhFrame {
                    let crate::model::Data::Bytes(bytes) = &section.data else { continue };
                    let records = unwind::split(bytes).map_err(|what| {
                        error(None, format!("{}: {}: {what}", self.new.inputs[it.input].name, section.name))
                    })?;
                    frames.push((load, at as u32, records));
                }
            }
        }
        let mut kept_frames: FxHashSet<(usize, u32, u32)> = FxHashSet::default();
        loop {
            while let Some((load, section)) = work.pop() {
                if discarded.contains(&(load, section)) || !marked.insert((load, section)) {
                    continue;
                }
                if let Some((owner, index)) = group_of.get(&(load, section)) {
                    work.extend(loads[*owner].object.groups[*index].sections.iter().map(|it| (*owner, *it)));
                }
                let it = &loads[load].object.sections[section as usize];
                for fixup in &it.fixups {
                    for symbol in std::iter::once(fixup.symbol).chain(fixup.minus) {
                        if let Some((load, section, _)) = section_of(&loads, &candidates, load, symbol) {
                            work.push((load, section));
                        }
                        let sym = loads[load].syms[symbol as usize];
                        mark_name(self, sym, &mut work, &mut named_done);
                    }
                }
            }
            // Unwind sections and FDEs are kept for what they describe.
            for (load, it) in loads.iter().enumerate() {
                for (at, section) in it.object.sections.iter().enumerate() {
                    if !matches!(section.what, SectionKind::Unwind(_)) || marked.contains(&(load, at as u32)) {
                        continue;
                    }
                    let first = section.fixups.iter().min_by_key(|it| it.at);
                    let describes = first.and_then(|it| section_of(&loads, &candidates, load, it.symbol));
                    if describes.is_some_and(|(l, s, _)| marked.contains(&(l, s))) {
                        work.push((load, at as u32));
                    }
                }
            }
            for (load, section, records) in &frames {
                let it = &loads[*load].object.sections[*section as usize];
                for record in records.iter().filter(|it| it.cie.is_some()) {
                    let start = record.range.start as u32;
                    if kept_frames.contains(&(*load, *section, start)) {
                        continue;
                    }
                    let function = it.fixups.iter().find(|f| f.at == u64::from(start) + 8);
                    let describes = function.and_then(|f| section_of(&loads, &candidates, *load, f.symbol));
                    if !describes.is_some_and(|(l, s, _)| marked.contains(&(l, s))) {
                        continue;
                    }
                    kept_frames.insert((*load, *section, start));
                    let cie = record.cie.expect("an FDE") as u32;
                    kept_frames.insert((*load, *section, cie));
                    let in_records = |at: u64, range: &std::ops::Range<usize>| {
                        at >= range.start as u64 && at < range.end as u64
                    };
                    let cie_range = records.iter().find(|r| r.range.start as u32 == cie).map(|r| r.range.clone());
                    for fixup in &it.fixups {
                        let ours = in_records(fixup.at, &record.range)
                            || cie_range.as_ref().is_some_and(|r| in_records(fixup.at, r));
                        if ours {
                            if let Some((l, s, _)) = section_of(&loads, &candidates, *load, fixup.symbol) {
                                work.push((l, s));
                            }
                            let sym = loads[*load].syms[fixup.symbol as usize];
                            mark_name(self, sym, &mut work, &mut named_done);
                        }
                    }
                }
            }
            if work.is_empty() {
                break;
            }
        }

        // The image's natives: sections and frame records that came, and went.
        let kept: FxHashSet<(Obj, u32)> = marked
            .iter()
            .filter(|(load, section)| {
                !matches!(loads[*load].object.sections[*section as usize].what, SectionKind::Dropped | SectionKind::EhFrame)
            })
            .map(|(load, section)| (loads[*load].obj, *section))
            .collect();
        let frame_keys: FxHashSet<(Obj, u32, u32)> =
            kept_frames.iter().map(|(load, section, at)| (loads[*load].obj, *section, *at)).collect();
        let old = self.new.resolved.clone();
        for (obj, section) in old.kept.iter().filter(|it| !kept.contains(*it)) {
            if let Some(sym) = self.symbols.find(&SymKey::Section(*obj, *section)) {
                if let Def::Piece { piece, .. } = self.new.def(sym) {
                    self.unmake(piece);
                }
            }
        }
        for (obj, section, at) in old.frames.iter().filter(|it| !frame_keys.contains(*it)) {
            if let Some(sym) = self.symbols.find(&SymKey::Frame(*obj, *section, *at)) {
                if let Def::Piece { piece, .. } = self.new.def(sym) {
                    self.unmake(piece);
                }
            }
        }
        let by_obj: FxHashMap<Obj, usize> = loads.iter().enumerate().map(|(at, it)| (it.obj, at)).collect();
        let mut sorted: Vec<&(Obj, u32)> = kept.iter().filter(|it| !old.kept.contains(*it)).collect();
        radix::sort_copy_by_key(&mut sorted, |&&(obj, section)| {
            (u128::from(obj.native.0) << 64) | (u128::from(obj.member) << 32) | u128::from(section)
        });
        for (obj, section) in sorted {
            let load = by_obj[obj];
            self.native_section(&loads, load, *section)?;
        }
        let mut sorted: Vec<&(usize, u32, u32)> =
            kept_frames.iter().filter(|(l, s, a)| !old.frames.contains(&(loads[*l].obj, *s, *a))).collect();
        radix::sort_copy_by_key(&mut sorted, |&&(load, section, at)| {
            (u128::from(load as u64) << 64) | (u128::from(section) << 32) | u128::from(at)
        });
        for (load, section, at) in sorted {
            let records = &frames.iter().find(|it| it.0 == *load && it.1 == *section).expect("split").2;
            let record = records.iter().find(|it| it.range.start as u32 == *at).expect("a record");
            self.native_frame(&loads, *load, *section, record)?;
        }

        // Every loaded symbol's definition.
        let piece_of = |this: &Self, obj: Obj, section: u32| -> Option<PieceId> {
            match this.new.def(this.symbols.find(&SymKey::Section(obj, section))?) {
                Def::Piece { piece, .. } => Some(piece),
                _ => None,
            }
        };
        let mut defined: FxHashSet<Sym> = FxHashSet::default();
        for it in &loads {
            for (at, symbol) in it.object.symbols.iter().enumerate() {
                let sym = it.syms[at];
                let local = symbol.binding == Binding::Local || symbol.kind == SymbolKind::Section;
                if !local {
                    continue;
                }
                let def = match symbol.def {
                    SymbolDef::Section { section, value, .. } => match piece_of(self, it.obj, section) {
                        Some(piece) => Def::Piece { piece, offset: value },
                        None => Def::None,
                    },
                    SymbolDef::Absolute(value) if symbol.kind == SymbolKind::Tls => Def::TlsOffset(value),
                    SymbolDef::Absolute(value) => Def::Absolute(value),
                    _ => Def::None,
                };
                self.define(sym, def);
            }
        }
        let mut sorted: Vec<(Sym, Candidate)> = candidates.iter().map(|(s, c)| (*s, *c)).collect();
        radix::sort_by_key(&mut sorted, |it| it.0.0);
        for (sym, candidate) in sorted {
            let def = match candidate {
                Candidate::Section { load, symbol, .. } => {
                    let it = &loads[load];
                    match it.object.symbols[symbol as usize].def {
                        SymbolDef::Section { section, value, .. } => match piece_of(self, it.obj, section) {
                            Some(piece) => Def::Piece { piece, offset: value },
                            None => Def::None,
                        },
                        _ => Def::None,
                    }
                }
                Candidate::Absolute { value, tls: true, .. } => Def::TlsOffset(value),
                Candidate::Absolute { value, .. } => Def::Absolute(value),
                Candidate::Common => continue,
            };
            defined.insert(sym);
            self.define(sym, def);
        }
        let old_commons: Vec<Box<str>> = old.commons.keys().filter(|it| !commons.contains_key(*it)).cloned().collect();
        for name in old_commons {
            if let Some(sym) = self.symbols.find(&SymKey::Common(name)) {
                if let Def::Piece { piece, .. } = self.new.def(sym) {
                    self.unmake(piece);
                }
            }
        }
        for (name, (size, align)) in &commons {
            let sym = self.sym(SymKey::Name(name.clone()));
            if !matches!(candidates.get(&sym), Some(Candidate::Common)) {
                continue;
            }
            let common = self.sym(SymKey::Common(name.clone()));
            let same = old.commons.get(name) == Some(&(*size, *align));
            if !same {
                if let Def::Piece { piece, .. } = self.new.def(common) {
                    self.unmake(piece);
                }
                let mut piece = self.piece(common, Class::Bss, *align, *size, Vec::new(), What::Common);
                piece.zero = true;
                self.make(piece, Cow::Owned(Vec::new()));
            }
            defined.insert(sym);
            self.define(sym, Def::Alias(common));
        }
        for (sym, target) in &rust_names {
            defined.insert(*sym);
            self.define(*sym, Def::Alias(*target));
        }
        let mut sorted: Vec<(Sym, (usize, u32))> = aliases.iter().map(|(s, at)| (*s, *at)).collect();
        radix::sort_copy_by_key(&mut sorted, |it| it.0.0);
        for (sym, (load, symbol)) in sorted {
            defined.insert(sym);
            self.define(sym, Def::Alias(loads[load].syms[symbol as usize]));
        }
        let mut defines: FxHashMap<PieceId, Vec<Sym>> = FxHashMap::default();
        for it in &loads {
            for (at, sym) in it.syms.iter().enumerate() {
                if let SymbolDef::Section { section, .. } = it.object.symbols[at].def {
                    if let (Some(home), Def::Piece { piece, .. }) = (piece_of(self, it.obj, section), self.new.def(*sym)) {
                        if home == piece && self.new.piece(piece).is_some_and(|p| p.sym != *sym) {
                            defines.entry(piece).or_default().push(*sym);
                        }
                    }
                }
            }
        }
        let native: Vec<PieceId> = self
            .new
            .live_pieces()
            .filter(|(_, it)| matches!(it.what, What::Native { .. }))
            .map(|(id, _)| id)
            .collect();
        for id in native {
            let mut syms = defines.remove(&id).unwrap_or_default();
            radix::sort_copy_by_key(&mut syms, |it| it.0);
            syms.dedup();
            let piece = self.new.piece(id).expect("a live piece");
            if *piece.defines != *syms {
                self.new.piece_mut(id).expect("a live piece").defines = syms.into();
            }
        }

        // What is left: the linker's, a shared library's, weak, or an error.
        let mut names = wanted.all.clone();
        let mut native_weak: FxHashMap<Sym, bool> = FxHashMap::default();
        for (load, it) in loads.iter().enumerate() {
            for (at, section) in it.object.sections.iter().enumerate() {
                if !marked.contains(&(load, at as u32)) {
                    continue;
                }
                for fixup in &section.fixups {
                    for symbol in std::iter::once(fixup.symbol).chain(fixup.minus) {
                        let s = &it.object.symbols[symbol as usize];
                        if s.binding != Binding::Local && s.kind != SymbolKind::Section {
                            let sym = it.syms[symbol as usize];
                            names.insert(sym);
                            let weak = native_weak.entry(sym).or_insert(true);
                            *weak &= s.binding == Binding::Weak;
                        }
                    }
                }
            }
        }
        if self.output.live.is_some() {
            let live = self.sym(SymKey::Name(blaze_live::patch::DESCRIPTOR.into()));
            let descriptor = self.sym(SymKey::Descriptor);
            defined.insert(live);
            self.define(live, Def::Alias(descriptor));
        }
        for sym in &roots {
            if extra_is_optional(self.symbols.key(*sym)) {
                continue;
            }
            names.insert(*sym);
        }
        names.extend(required.iter().copied());
        // `__imp_X` is X's GOT cell: X is resolved as a strong reference is.
        let got_targets: Vec<Box<str>> = names
            .iter()
            .filter(|it| !defined.contains(*it))
            .filter_map(|it| match self.symbols.key(*it) {
                SymKey::Name(name) => match self.writer.special(name) {
                    Some(Special::Got(target)) => Some(target.into_boxed_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        for target in got_targets {
            let sym = self.sym(SymKey::Name(target));
            required.insert(sym);
            names.insert(sym);
        }
        let mut libraries: Vec<(Library, &Shared)> = Vec::new();
        let shared_inputs = inputs.iter().filter_map(|input| match &*input.file {
            NativeFile::Shared(shared) => Some((shared, matches!(input.kind, NativeKind::Shared { as_needed: true }))),
            _ => None,
        });
        for (shared, as_needed) in shared_inputs.chain(pulled_shared.iter().map(|it| (&**it, false))) {
            let mut all: Vec<&Shared> = vec![shared];
            let mut at = 0;
            while at < all.len() {
                let this: &Shared = all[at];
                all.extend(this.more.iter());
                at += 1;
            }
            for it in all {
                libraries.push((Library { name: it.name.clone(), as_needed }, it));
            }
        }
        // Libraries of one name are one library (an import library's members of one DLL).
        let mut first_of: FxHashMap<&str, usize> = FxHashMap::default();
        let canonical: Vec<usize> =
            (0..libraries.len()).map(|at| *first_of.entry(libraries[at].0.name.as_str()).or_insert(at)).collect();
        let mut exports: FxHashMap<&str, (usize, &crate::model::SharedSymbol)> = FxHashMap::default();
        for (at, (_, shared)) in libraries.iter().enumerate() {
            for it in &shared.symbols {
                exports.entry(it.name.as_str()).or_insert((canonical[at], it));
            }
        }
        let mut imports: Vec<(Sym, Import, usize)> = Vec::new();
        let executable = matches!(self.output.kind, OutputKind::Executable { .. });
        let mut specials = Vec::new();
        for sym in names.iter() {
            if defined.contains(&sym) {
                continue;
            }
            let SymKey::Name(name) = self.symbols.key(sym).clone() else { continue };
            let weak = !required.contains(&sym)
                && wanted.weak(sym).unwrap_or(true)
                && native_weak.get(&sym).copied().unwrap_or(true);
            if let Some(special) = self.writer.special(&name) {
                specials.push((sym, special));
                continue;
            }
            if let Some((library, it)) = exports.get(&*name) {
                imports.push((
                    sym,
                    Import {
                        name: name.to_string(),
                        library: Some(*library as u32),
                        version: it.version.clone(),
                        kind: it.kind,
                        weak: weak || it.weak,
                        wasm: None,
                    },
                    *library,
                ));
                continue;
            }
            if self.output.format == crate::Format::Wasm {
                // rustc links wasm with `--allow-undefined`: a function something says the
                // import of is imported; a weak one is null.
                let import = native_imports.get(&sym).or_else(|| self.new.wasm_imports.get(&*name));
                match import {
                    Some(it) if !weak => {
                        let import = Import {
                            name: name.to_string(),
                            library: None,
                            version: None,
                            kind: SymbolKind::Function,
                            weak: false,
                            wasm: Some(it.clone()),
                        };
                        imports.push((sym, import, usize::MAX));
                    }
                    _ if weak => self.define(sym, Def::Absolute(0)),
                    _ => {
                        return Err(error(
                            None,
                            format!(
                                "undefined symbol `{name}`{}, which no input says the wasm import of",
                                self.referrer(sym)
                            ),
                        ));
                    }
                }
                continue;
            }
            if weak && executable {
                self.define(sym, Def::Absolute(0));
                continue;
            }
            if !executable {
                imports.push((
                    sym,
                    Import {
                        name: name.to_string(),
                        library: None,
                        version: None,
                        kind: SymbolKind::Other,
                        weak,
                        wasm: None,
                    },
                    usize::MAX,
                ));
                continue;
            }
            if extra_is_optional(&SymKey::Name(name.clone())) {
                continue;
            }
            return Err(error(None, format!("undefined symbol `{name}`{}", self.referrer(sym))));
        }
        self.new.specials.clear();
        for (sym, special) in specials {
            let at = self.new.specials.len() as u32;
            self.new.specials.push(special);
            self.define(sym, Def::Special(at));
        }

        // The libraries the image needs, and its imports: kept in their order across
        // incremental links so that no bind moves.
        let used: FxHashSet<usize> = imports.iter().map(|it| it.2).collect();
        let mut index = vec![None; libraries.len()];
        let mut needed = Vec::new();
        for (at, (library, _)) in libraries.iter().enumerate() {
            if canonical[at] == at && (!library.as_needed || used.contains(&at)) {
                index[at] = Some(needed.len() as u32);
                needed.push(library.clone());
            }
        }
        for it in &mut imports {
            it.1.library = it.1.library.and_then(|library| index[library as usize]);
        }
        // An import an incremental link no longer needs keeps its place (nothing binds to it),
        // its library looked up again by name.
        let mut order: Vec<(Sym, Import, bool)> = Vec::new();
        let mut current: FxHashMap<Sym, Import> = imports.into_iter().map(|it| (it.0, it.1)).collect();
        if !self.full {
            for (sym, import) in old.import_syms.iter().zip(&old.imports) {
                match current.remove(sym) {
                    Some(it) => order.push((*sym, it, true)),
                    None => {
                        let mut stale = import.clone();
                        stale.library = import.library.and_then(|library| {
                            let name = &old.libraries.get(library as usize)?.name;
                            needed.iter().position(|it| &it.name == name).map(|it| it as u32)
                        });
                        stale.weak |= stale.library.is_none();
                        order.push((*sym, stale, false));
                    }
                }
            }
        }
        let mut new: Vec<(Sym, Import)> = current.into_iter().collect();
        blaze_ids::names::sort_by_name(&mut new, |it| it.1.name.as_bytes());
        order.extend(new.into_iter().map(|(sym, import)| (sym, import, true)));
        for (at, (sym, _, active)) in order.iter().enumerate() {
            if *active {
                self.define(*sym, Def::Import(at as u32));
            }
        }
        let entry = entry_name.and_then(|it| match self.new.def(it) {
            Def::None => None,
            _ => Some(it),
        });
        if let (Some(name), None) = (&entry_name, entry) {
            return Err(error(None, format!("the entry point {} is not defined", crate::stage::describe(self.symbols.key(*name)))));
        }
        self.new.resolved = Arc::new(Resolved {
            imports: order.iter().map(|it| it.1.clone()).collect(),
            import_syms: order.iter().map(|it| it.0).collect(),
            libraries: needed,
            kept,
            frames: frame_keys,
            commons,
            exports: exports_asked,
        });
        Ok(())
    }

    /// Who refers to an undefined symbol, for its error.
    fn referrer(&self, sym: Sym) -> String {
        let Some(piece) = self.new.refs(sym).iter().find_map(|it| self.new.piece(*it)) else {
            return String::new();
        };
        match (&piece.what, self.symbols.key(piece.sym)) {
            (What::Native { obj, .. }, SymKey::Section(_, _)) => {
                let name = self.new.inputs.iter().find(|it| it.id == obj.native).map_or("?", |it| &it.name);
                format!(", referred to by {name}")
            }
            (_, key) => format!(", referred to by {}", crate::stage::describe(key)),
        }
    }

    fn native_section(&mut self, loads: &[Loaded], load: usize, section: u32) -> Result<(), LinkError> {
        let it = &loads[load];
        let (name, order) = (self.new.inputs[it.input].name.clone(), self.new.inputs[it.input].order);
        let s = &it.object.sections[section as usize];
        let (class, rank, unwind) = match &s.what {
            SectionKind::Alloc(class, rank) => (class.clone(), *rank, false),
            SectionKind::Unwind(class) => (class.clone(), 0, true),
            SectionKind::Dropped | SectionKind::EhFrame => return Ok(()),
        };
        let mut weak = Vec::new();
        let fixups: Vec<Fixup> = s
            .fixups
            .iter()
            .map(|f| {
                let target = it.syms[f.symbol as usize];
                if it.object.symbols[f.symbol as usize].binding == Binding::Weak
                    && it.object.symbols[f.symbol as usize].def == SymbolDef::Undefined
                {
                    weak.push(target);
                }
                Fixup { at: f.at, apply: f.apply, target, addend: f.addend, minus: f.minus.map(|m| it.syms[m as usize]) }
            })
            .collect();
        radix::sort_copy_by_key(&mut weak, |it: &Sym| it.0);
        weak.dedup();
        let sym = self.sym(SymKey::Section(it.obj, section));
        let zero = matches!(s.data, crate::model::Data::Zero(_));
        let zeroed = crate::state::zeroed(&class);
        if !zero && zeroed {
            return Err(error(None, format!("{name}: {} has bytes, in a zeroed section", s.name)));
        }
        let bytes: Cow<'a, [u8]> = match &s.data {
            crate::model::Data::Bytes(bytes) => Cow::Owned(bytes.clone()),
            crate::model::Data::Zero(size) if !zeroed => Cow::Owned(vec![0; *size as usize]),
            crate::model::Data::Zero(_) => Cow::Owned(Vec::new()),
        };
        let mut piece = self.piece(sym, class, s.align, s.data.len(), fixups, What::Native { obj: it.obj, section, unwind });
        piece.rank = rank;
        piece.order = [order, it.obj.member, section];
        piece.zero = zeroed;
        piece.weak = weak.into();
        self.make(piece, bytes);
        Ok(())
    }

    fn native_frame(
        &mut self,
        loads: &[Loaded],
        load: usize,
        section: u32,
        record: &unwind::Native,
    ) -> Result<(), LinkError> {
        let it = &loads[load];
        let (name, order) = (self.new.inputs[it.input].name.clone(), self.new.inputs[it.input].order);
        let s = &it.object.sections[section as usize];
        let crate::model::Data::Bytes(bytes) = &s.data else { unreachable!("split from bytes") };
        let start = record.range.start as u64;
        let end = record.range.end as u64;
        let sym = self.sym(SymKey::Frame(it.obj, section, start as u32));
        let mut fixups: Vec<Fixup> = s
            .fixups
            .iter()
            .filter(|f| f.at >= start && f.at < end)
            .map(|f| Fixup {
                at: f.at - start,
                apply: f.apply,
                target: it.syms[f.symbol as usize],
                addend: f.addend,
                minus: f.minus.map(|m| it.syms[m as usize]),
            })
            .collect();
        let fde = record.cie.is_some();
        if let Some(cie) = record.cie {
            let cie = self.sym(SymKey::Frame(it.obj, section, cie as u32));
            fixups.retain(|f| f.at != 4);
            fixups.push(Fixup { at: 4, apply: crate::model::Apply::Delta32, target: sym, addend: 4, minus: Some(cie) });
        }
        radix::sort_copy_by_key(&mut fixups, |f| f.at);
        let mut piece = self.piece(
            sym,
            Class::EhFrame,
            4,
            end - start,
            fixups,
            What::NativeFrame { obj: it.obj, section, fde },
        );
        piece.rank = if fde { 1 } else { 0 };
        piece.order = [order, it.obj.member, section];
        let bytes = bytes[record.range.clone()].to_vec();
        if bytes.len() % 4 != 0 {
            return Err(error(None, format!("{name}: {}: a frame record not a multiple of 4 long", s.name)));
        }
        self.make(piece, Cow::Owned(bytes));
        Ok(())
    }
}

/// Names the core asks for that an image need not define.
fn extra_is_optional(key: &SymKey) -> bool {
    matches!(key, SymKey::Name(name) if &**name == "_init" || &**name == "_fini")
}

#[cfg(test)]
mod sym_bits {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn the_bits_answer_as_a_sorted_set_does_between_inserts() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let (mut bits, mut set) = (SymBits::default(), BTreeSet::new());
        for round in 0..4_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let shard = (seed % Symbols::SHARDS as u64) as usize;
            let at = match round % 3 {
                0 => (seed >> 8) as usize % 200,
                1 => (seed >> 8) as usize % 5_000,
                _ => 63 + 64 * ((seed >> 8) as usize % 4),
            };
            let sym = Symbols::join(shard, at);
            assert_eq!(bits.insert(sym), set.insert(sym));
            let probe = Symbols::join(shard, (seed >> 20) as usize % 6_000);
            assert_eq!(bits.contains(probe), set.contains(&probe));
            if round % 97 == 0 {
                assert_eq!(bits.iter().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
            }
        }
        assert_eq!(bits.iter().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
        let mut extended = SymBits::default();
        extended.extend(set.iter().rev().copied());
        assert_eq!(extended.iter().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
    }

    #[test]
    fn wanted_is_strong_unless_wanted_already() {
        let (weak, strong, absent) = (Symbols::join(3, 9), Symbols::join(0, 70), Symbols::join(63, 1));
        let mut wanted = Wanted::default();
        wanted.all.insert(weak);
        wanted.or_strong(weak);
        wanted.or_strong(strong);
        assert_eq!(wanted.weak(weak), Some(true));
        assert_eq!(wanted.weak(strong), Some(false));
        assert_eq!(wanted.weak(absent), None);
        assert_eq!(wanted.strong.iter().collect::<Vec<_>>(), vec![strong]);
        assert_eq!(wanted.all.iter().collect::<Vec<_>>(), vec![strong, weak]);
    }
}
