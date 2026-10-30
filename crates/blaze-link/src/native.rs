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
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::model::{
    Binding, Class, Fixup, Import, Library, NativeFile, Object, SectionKind, Shared, Special, Sym,
    SymbolDef, SymbolKind,
};
use crate::stage::{Stage, error};
use crate::state::{Def, Input, PieceId, Resolved, What};
use crate::symbols::{Obj, SymKey};
use crate::unwind;
use crate::{Change, LinkError, NativeKind, OutputKind};

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

impl<'a> Stage<'a> {
    /// The native inputs the change adds and removes.
    pub fn inputs(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        if change.natives.is_empty() && change.removed_natives.is_empty() {
            return Ok(());
        }
        let inputs = Arc::make_mut(&mut self.new.inputs);
        inputs.retain(|it| !change.removed_natives.contains(&it.id));
        for native in &change.natives {
            if let Some(it) = inputs.iter_mut().find(|it| it.id == native.id) {
                it.order = native.order;
                continue;
            }
            let file = self
                .writer
                .read(native.name, native.bytes, &native.kind)
                .map_err(|it| error(None, format!("{}: {it}", native.name)))?;
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

    fn member(&mut self, input: usize, member: u32) -> Result<Arc<Object>, LinkError> {
        let it = &self.new.inputs[input];
        let obj = Obj { native: it.id, member: member + 1 };
        if let Some(object) = self.objects.get(&obj) {
            return Ok(object.clone());
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
        let NativeFile::Object(object) = file else {
            return Err(error(None, format!("{name}: an archive member that is not an object")));
        };
        let object = Arc::new(object);
        self.objects.insert(obj, object.clone());
        Ok(object)
    }

    /// The names Rust pieces refer to, with whether every reference is weak.
    fn wanted(&self) -> BTreeMap<Sym, bool> {
        let mut out = BTreeMap::new();
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
                out.insert(*sym, weak);
            }
        }
        out
    }

    /// Loads, resolves, collects and places the natives, and defines every name.
    pub fn resolve(&mut self) -> Result<(), LinkError> {
        let mut wanted = self.wanted();
        let mut roots: Vec<Sym> = wanted.keys().copied().collect();
        let mut extra: Vec<String> = Vec::new();
        match &self.output.kind {
            OutputKind::Executable { entry } => extra.push(entry.clone()),
            OutputKind::Shared { exports, .. } => extra.extend(exports.iter().cloned()),
        }
        extra.extend(["_init".to_owned(), "_fini".to_owned()]);
        for name in &extra {
            let sym = self.sym(SymKey::Name(name.as_str().into()));
            roots.push(sym);
        }
        let entry_name = match &self.output.kind {
            OutputKind::Executable { entry } => Some(self.sym(SymKey::Name(entry.as_str().into()))),
            OutputKind::Shared { .. } => None,
        };
        if let Some(entry) = entry_name {
            wanted.entry(entry).or_insert(false);
        }
        if let OutputKind::Shared { exports, .. } = &self.output.kind {
            for name in exports.clone() {
                let sym = self.sym(SymKey::Name(name.as_str().into()));
                wanted.entry(sym).or_insert(false);
            }
        }

        let mut loads: Vec<Loaded> = Vec::new();
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
                            let object = self.member(at, member)?;
                            self.load(&mut loads, at, member + 1, object);
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
        let mut groups: FxHashMap<String, usize> = FxHashMap::default();
        let mut discarded: FxHashSet<(usize, u32)> = FxHashSet::default();
        let mut candidates: FxHashMap<Sym, Candidate> = FxHashMap::default();
        let mut commons: BTreeMap<Box<str>, (u64, u64)> = BTreeMap::new();
        let mut undefined_strong: BTreeSet<Sym> = BTreeSet::new();
        let mut pulled: FxHashSet<(usize, u32)> = FxHashSet::default();
        let mut done = 0;
        loop {
            while done < loads.len() {
                let load = &loads[done];
                for group in &load.object.groups {
                    if let Some(winner) = groups.get(&group.name) {
                        if *winner != done {
                            discarded.extend(group.sections.iter().map(|it| (done, *it)));
                        }
                    } else {
                        groups.insert(group.name.clone(), done);
                    }
                }
                for (at, symbol) in load.object.symbols.iter().enumerate() {
                    if symbol.binding == Binding::Local {
                        continue;
                    }
                    let sym = load.syms[at];
                    let weak = symbol.binding == Binding::Weak;
                    let candidate = match symbol.def {
                        SymbolDef::Undefined => {
                            if !weak {
                                undefined_strong.insert(sym);
                            }
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
                    match candidates.get(&sym) {
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
            for sym in wanted.iter().filter(|it| !*it.1).map(|it| *it.0).chain(undefined_strong.iter().copied()) {
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
                let object = self.member(input, member)?;
                self.load(&mut loads, input, member + 1, object);
            }
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
            match candidates.get(&loads[load].syms[symbol as usize])? {
                Candidate::Section { load, symbol, .. } => match loads[*load].object.symbols[*symbol as usize].def {
                    SymbolDef::Section { section, value, .. } => Some((*load, section, value)),
                    _ => None,
                },
                _ => None,
            }
        };

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
            if let Some(Candidate::Section { load, symbol, .. }) = candidates.get(&sym) {
                if let SymbolDef::Section { section, .. } = loads[*load].object.symbols[*symbol as usize].def {
                    work.push((*load, section));
                }
            }
            if let SymKey::Name(name) = this.symbols.key(sym) {
                if let Some(Special::Section { name: section, .. }) = this.writer.special(name) {
                    if named_done.insert(section.clone()) {
                        work.extend(specials_by_section.get(&section).into_iter().flatten().copied());
                    }
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
        sorted.sort();
        for (obj, section) in sorted {
            let load = by_obj[obj];
            self.native_section(&loads, load, *section)?;
        }
        let mut sorted: Vec<&(usize, u32, u32)> =
            kept_frames.iter().filter(|(l, s, a)| !old.frames.contains(&(loads[*l].obj, *s, *a))).collect();
        sorted.sort();
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
        sorted.sort_by_key(|it| it.0);
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
            syms.sort_unstable();
            syms.dedup();
            let piece = self.new.piece(id).expect("a live piece");
            if *piece.defines != *syms {
                self.new.piece_mut(id).expect("a live piece").defines = syms.into();
            }
        }

        // What is left: the linker's, a shared library's, weak, or an error.
        let mut names: BTreeSet<Sym> = wanted.keys().copied().collect();
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
        let mut libraries: Vec<(Library, &Shared)> = Vec::new();
        for input in inputs.iter() {
            if let NativeFile::Shared(shared) = &*input.file {
                let as_needed = matches!(input.kind, NativeKind::Shared { as_needed: true });
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
        }
        let mut exports: FxHashMap<&str, (usize, &crate::model::SharedSymbol)> = FxHashMap::default();
        for (at, (_, shared)) in libraries.iter().enumerate() {
            for it in &shared.symbols {
                exports.entry(it.name.as_str()).or_insert((at, it));
            }
        }
        let mut imports: Vec<(Sym, Import, usize)> = Vec::new();
        let executable = matches!(self.output.kind, OutputKind::Executable { .. });
        let mut specials = Vec::new();
        for sym in names {
            if defined.contains(&sym) {
                continue;
            }
            let SymKey::Name(name) = self.symbols.key(sym).clone() else { continue };
            let weak = wanted.get(&sym).copied().unwrap_or(true) && native_weak.get(&sym).copied().unwrap_or(true);
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
                    },
                    *library,
                ));
                continue;
            }
            if weak && executable {
                self.define(sym, Def::Absolute(0));
                continue;
            }
            if !executable {
                imports.push((
                    sym,
                    Import { name: name.to_string(), library: None, version: None, kind: SymbolKind::Other, weak },
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
            if !library.as_needed || used.contains(&at) {
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
        new.sort_by(|a, b| a.1.name.cmp(&b.1.name));
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
        if executable && entry.is_none() {
            let OutputKind::Executable { entry } = &self.output.kind else { unreachable!() };
            return Err(error(None, format!("the entry point `{entry}` is not defined")));
        }
        self.new.resolved = Arc::new(Resolved {
            imports: order.iter().map(|it| it.1.clone()).collect(),
            import_syms: order.iter().map(|it| it.0).collect(),
            libraries: needed,
            kept,
            frames: frame_keys,
            commons,
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
        weak.sort_unstable();
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
        fixups.sort_by_key(|f| f.at);
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
