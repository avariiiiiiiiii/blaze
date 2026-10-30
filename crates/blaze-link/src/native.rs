//! Native inputs: C objects, archives and shared libraries, and the names they and Rust
//! code share.
//!
//! The rules are lld's: objects are loaded in link-line order, archive members are pulled for
//! what is strongly undefined wherever the archive is on the line (in rounds, each round's
//! members in link-line order), the first COMDAT group of a name wins, a strong definition
//! beats a weak one beats a common one, and sections nothing reachable refers to are dropped
//! (`--gc-sections`).
//!
//! What a stage pays is what changed. An input is read, checked and indexed once
//! ([`Parsed`]: its symbols interned, each section's referenced and defined symbols, its
//! groups and call frame records as columns), in parallel, and kept for the image's life in
//! [`Stage::objects`]. A resolution ([`Graph`]: what every name is, which members are pulled,
//! which groups win) and its collection ([`Marks`]) are kept in [`Resolved`] with how many of
//! Rust's pieces refer to each name, counted from the pieces made and removed since the last
//! stage, never by walking them. A stage then does one of three things:
//!
//! - nothing, when no native input or Rust definition changed and no name Rust refers to
//!   came or went in a way that changes what it resolves to;
//! - grows the kept resolution, when Rust refers to names it did not: they are resolved
//!   against the kept graph, and what they newly reach is marked and made, unless that needs
//!   an archive member, an alias, a special that keeps sections or a library the kept one
//!   does not have;
//! - builds the graph again from the kept columns (no input is read again) and compares it
//!   with the image's: only sections, frame records and commons that came or went, and
//!   symbols whose definition changed, are touched.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::collections::hash_map::Entry;
use std::hash::Hasher;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_ids::radix;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use smallvec::SmallVec;

use crate::cow::{CowList, CowVec, ListRef};
use crate::model::{
    Apply, Archive, Binding, Class, Data, Directive, Fixup, Import, Library, Name, Names, NativeFile, Object,
    SectionKind, Shared, Special, Sym, Symbol, SymbolDef, SymbolKind, Writer,
};
use crate::stage::{Content, Stage, error};
use crate::state::{Def, Input, PieceId, State, What};
use crate::symbols::{NameId, Obj, SymKey, Symbols, Wide};
use crate::unwind;
use crate::{Change, LinkError, NativeKind, OutputKind};

const NONE: u32 = u32::MAX;
const NONE64: u64 = u64::MAX;

#[inline]
fn pack(high: u32, low: u32) -> u64 {
    u64::from(high) << 32 | u64::from(low)
}

#[inline]
fn unpack(it: u64) -> (u32, u32) {
    ((it >> 32) as u32, it as u32)
}

fn hash(name: &str) -> u64 {
    let mut hasher = FxHasher::default();
    hasher.write(name.as_bytes());
    hasher.finish()
}

/// Sections kept whatever refers to them, by name, as lld keeps them.
fn kept_by_name(name: &str) -> bool {
    [".init", ".fini", ".ctors", ".dtors", ".jcr", ".preinit_array", ".init_array", ".fini_array"]
        .iter()
        .any(|it| name == *it || name.strip_prefix(it).is_some_and(|rest| rest.starts_with('.')))
        || name.starts_with(".note")
}

/// Names the core roots that an image need not define.
fn optional(name: &str) -> bool {
    name == "_init" || name == "_fini"
}

/// A symbol only its object sees: a local, or a section's own.
#[inline]
fn is_local(symbol: &Symbol) -> bool {
    symbol.binding == Binding::Local || symbol.kind == SymbolKind::Section
}

/// Whether a section of this kind is a piece of the image.
fn makes_piece(what: &SectionKind) -> bool {
    matches!(what, SectionKind::Alloc(..) | SectionKind::Unwind(_))
}

/// A bitset, copied on write by chunk: a stage that marks a few more sections copies the
/// words it writes.
#[derive(Clone, Default)]
pub(crate) struct Bits(CowVec<u64>);

impl Bits {
    #[inline]
    fn get(&self, at: usize) -> bool {
        self.0.get(at / 64).is_some_and(|word| word >> (at % 64) & 1 == 1)
    }

    /// Sets the bit; whether it was clear.
    fn set(&mut self, at: usize) -> bool {
        let (word, bit) = (at / 64, 1u64 << (at % 64));
        match self.0.get(word) {
            Some(it) if it & bit != 0 => false,
            Some(_) => {
                *self.0.get_mut(word).expect("in range") |= bit;
                true
            }
            None => {
                self.0.set(word, bit, || 0);
                true
            }
        }
    }
}

/// Names to `u32`s, open-addressed: a name is hashed once, compared only when the whole hash
/// matches, and the first value given a name keeps it. The names live with the caller (`eq`
/// compares a stored value's name with the one looked for).
#[derive(Clone, Default, Debug)]
pub(crate) struct Table {
    hashes: Vec<u64>,
    values: Vec<u32>,
    len: usize,
}

impl Table {
    fn with_capacity(len: usize) -> Table {
        let room = (len.max(4) * 2).next_power_of_two();
        Table { hashes: vec![0; room], values: vec![NONE; room], len: 0 }
    }

    /// Where a hash starts probing: its high bits, which FxHash mixes best.
    #[inline]
    fn start(&self, hash: u64) -> usize {
        (hash >> (64 - self.values.len().trailing_zeros())) as usize
    }

    fn find(&self, hash: u64, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        if self.values.is_empty() {
            return None;
        }
        let mask = self.values.len() - 1;
        let mut at = self.start(hash);
        loop {
            let value = self.values[at];
            if value == NONE {
                return None;
            }
            if self.hashes[at] == hash && eq(value) {
                return Some(value);
            }
            at = (at + 1) & mask;
        }
    }

    /// Gives the name `value` unless a value has it already: then that value.
    fn insert(&mut self, hash: u64, value: u32, mut eq: impl FnMut(u32) -> bool) -> Result<(), u32> {
        debug_assert_ne!(value, NONE);
        if (self.len + 1) * 2 > self.values.len() {
            self.grow();
        }
        let mask = self.values.len() - 1;
        let mut at = self.start(hash);
        loop {
            let it = self.values[at];
            if it == NONE {
                self.values[at] = value;
                self.hashes[at] = hash;
                self.len += 1;
                return Ok(());
            }
            if self.hashes[at] == hash && eq(it) {
                return Err(it);
            }
            at = (at + 1) & mask;
        }
    }

    fn grow(&mut self) {
        let room = (self.values.len() * 2).max(8);
        let old = std::mem::replace(self, Table { hashes: vec![0; room], values: vec![NONE; room], len: 0 });
        let mask = room - 1;
        for (hash, value) in old.hashes.into_iter().zip(old.values) {
            if value == NONE {
                continue;
            }
            let mut at = self.start(hash);
            while self.values[at] != NONE {
                at = (at + 1) & mask;
            }
            self.values[at] = value;
            self.hashes[at] = hash;
            self.len += 1;
        }
    }
}

/// An archive's index by name, the first member of a name keeping it.
fn archive_index(archive: &Archive) -> Table {
    let mut table = Table::with_capacity(archive.index.len());
    for (at, (name, _)) in archive.index.iter().enumerate() {
        let name = archive.name(*name);
        let _ = table.insert(hash(name), at as u32, |it| archive.name(archive.index[it as usize].0) == name);
    }
    table
}

/// The member an archive's index says defines `name`.
fn archive_find(table: &Table, archive: &Archive, name: &str) -> Option<u32> {
    table.find(hash(name), |it| archive.name(archive.index[it as usize].0) == name).map(|it| archive.index[it as usize].1)
}

/// Rows of `u32`s in one column.
#[derive(Default, Debug)]
struct Csr {
    at: Vec<u32>,
    items: Vec<u32>,
}

impl Csr {
    /// From `(row, item)` pairs: each row's items in the pairs' order.
    fn build(rows: usize, pairs: &[(u32, u32)]) -> Csr {
        let mut at = vec![0u32; rows + 1];
        for (row, _) in pairs {
            at[*row as usize + 1] += 1;
        }
        for row in 0..rows {
            at[row + 1] += at[row];
        }
        let mut next = at.clone();
        let mut items = vec![0; pairs.len()];
        for (row, item) in pairs {
            let slot = &mut next[*row as usize];
            items[*slot as usize] = *item;
            *slot += 1;
        }
        Csr { at, items }
    }

    #[inline]
    fn get(&self, row: usize) -> &[u32] {
        &self.items[self.at[row] as usize..self.at[row + 1] as usize]
    }
}

/// Checks every index and range a reader gave, once, so that the rest reads them in place.
/// `len` is the length of the slice the file was read from.
fn check_file(file: &NativeFile, len: usize) -> Result<(), String> {
    match file {
        NativeFile::Object(object) => check_object(object, len as u64),
        NativeFile::Archive(archive) => check_archive(archive, len),
        NativeFile::Shared(shared) => check_shared(shared),
    }
}

fn check_object(object: &Object, len: u64) -> Result<(), String> {
    let names = &object.names;
    let (sections, symbols) = (object.sections.len(), object.symbols.len());
    for (at, section) in object.sections.iter().enumerate() {
        let Some(name) = names.get(section.name) else {
            return Err(format!("section {at} has a name outside the object's names"));
        };
        if let Data::Input(range) = &section.data {
            if range.start > range.end || range.end > len {
                return Err(format!("{name}: bytes {range:?}, past the input's {len}"));
            }
        }
        let fixups = &section.fixups;
        if fixups.start > fixups.end || fixups.end as usize > object.fixups.len() {
            return Err(format!("{name}: relocations {fixups:?}, past the object's {}", object.fixups.len()));
        }
    }
    for (at, fixup) in object.fixups.iter().enumerate() {
        if fixup.symbol as usize >= symbols || fixup.minus.is_some_and(|it| it as usize >= symbols) {
            return Err(format!("relocation {at} refers to a symbol past the object's {symbols}"));
        }
    }
    for (at, symbol) in object.symbols.iter().enumerate() {
        let Some(name) = names.get(symbol.name) else {
            return Err(format!("symbol {at} has a name outside the object's names"));
        };
        match symbol.def {
            SymbolDef::Section { section, .. } if section as usize >= sections => {
                return Err(format!("`{name}` is in section {section}, past the object's {sections}"));
            }
            SymbolDef::Fallback { symbol: to, .. } if to as usize >= symbols => {
                return Err(format!("`{name}` falls back to symbol {to}, past the object's {symbols}"));
            }
            _ => {}
        }
    }
    for (at, group) in object.groups.iter().enumerate() {
        let Some(name) = names.get(group.name) else {
            return Err(format!("COMDAT group {at} has a name outside the object's names"));
        };
        let range = &group.sections;
        if range.start > range.end || range.end as usize > object.grouped.len() {
            return Err(format!("COMDAT group `{name}`: sections {range:?}, past the object's {}", object.grouped.len()));
        }
    }
    if let Some(it) = object.grouped.iter().find(|it| **it as usize >= sections) {
        return Err(format!("a COMDAT group has section {it}, past the object's {sections}"));
    }
    for (at, _) in &object.imports {
        let Some(symbol) = object.symbols.get(*at as usize) else {
            return Err(format!("a wasm import of symbol {at}, past the object's {symbols}"));
        };
        if symbol.def != SymbolDef::Undefined || symbol.kind != SymbolKind::Function {
            return Err(format!("a wasm import of `{}`, which is not an undefined function", names.at(symbol.name)));
        }
    }
    Ok(())
}

fn check_archive(archive: &Archive, len: usize) -> Result<(), String> {
    for (at, member) in archive.members.iter().enumerate() {
        let Some(name) = archive.names.get(member.name) else {
            return Err(format!("member {at} has a name outside the archive's names"));
        };
        if member.bytes.start > member.bytes.end || member.bytes.end > len {
            return Err(format!("member {name} past the end of the archive"));
        }
    }
    for (name, member) in &archive.index {
        let Some(name) = archive.names.get(*name) else {
            return Err("the archive's index has a name outside its names".to_owned());
        };
        if *member as usize >= archive.members.len() {
            return Err(format!("the archive's index puts `{name}` in member {member}, past its {}", archive.members.len()));
        }
    }
    Ok(())
}

fn check_shared(shared: &Shared) -> Result<(), String> {
    for symbol in &shared.symbols {
        let Some(name) = shared.names.get(symbol.name) else {
            return Err(format!("{}: a symbol with a name outside its names", shared.name));
        };
        if let Some(version) = symbol.version.filter(|it| *it as usize >= shared.versions.len()) {
            return Err(format!("{}: `{name}` has version {version}, past its {}", shared.name, shared.versions.len()));
        }
    }
    shared.more.iter().try_for_each(check_shared)
}

/// The bytes of each native input this stage, by its index in [`State::inputs`]: what native
/// sections and frame records borrow, so that they are never copied.
#[derive(Default)]
pub(crate) struct Pass<'a> {
    bytes: Vec<&'a [u8]>,
    filled: bool,
}

/// A native input as the image keeps it across stages, by [`Obj`].
#[derive(Clone)]
pub(crate) enum Read {
    Object(Arc<Parsed>),
    /// An import library's member: one DLL's symbols.
    Shared(Arc<NativeFile>),
    /// A searched archive's index (at member 0, which no object of an archive is).
    Index(Arc<Table>),
}

/// A record of a native `.eh_frame` section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Frame {
    section: u32,
    start: u32,
    end: u32,
    fde: bool,
    /// An FDE's CIE: its index among the object's frame records (NONE when the section has
    /// no record there).
    cie: u32,
    /// An FDE's function: the symbol its address (at 8) is relocated by, NONE for none.
    function: u32,
}

/// An object read, checked, interned and indexed once, kept for the image's life.
pub(crate) struct Parsed {
    obj: Obj,
    file: Arc<NativeFile>,
    /// Where the object starts in its input's bytes (an archive member's offset).
    base: usize,
    /// Each symbol's image symbol, then each section's, then each frame record's.
    syms: Vec<Sym>,
    sections_at: usize,
    frames_at: usize,
    /// Of each section, the symbols its relocations refer to (and subtract), each once.
    refs: Csr,
    /// Of each section, the symbols defined in it.
    defs: Csr,
    /// Of each section, its COMDAT group; NONE for none.
    group_of: Vec<u32>,
    /// Sections kept whatever refers to them.
    roots: Vec<u32>,
    /// Unwind sections and the symbol of their first relocation: what each describes.
    unwind: Vec<(u32, u32)>,
    /// `Class::Named` sections, by their names.
    named: Vec<(NameId, u32)>,
    frames: Vec<Frame>,
    /// Of each frame record, its relocations (indexes into `Object::fixups`).
    frame_fixups: Csr,
}

impl Parsed {
    /// Indexes `file`, an object: `syms` its symbols' symbols, then its sections', then those
    /// of `records` (each `.eh_frame` section's, in order); `named` the names of its
    /// `Class::Named` sections, in order.
    fn new(
        obj: Obj,
        file: Arc<NativeFile>,
        base: usize,
        syms: Vec<Sym>,
        named: &[NameId],
        records: &[(u32, Vec<unwind::Native>)],
    ) -> Parsed {
        let NativeFile::Object(object) = &*file else { unreachable!("only objects are parsed") };
        let sections = object.sections.len();
        let mut pairs: Vec<(u32, u32)> = Vec::with_capacity(object.fixups.len());
        let mut row: SmallVec<[u32; 32]> = SmallVec::new();
        for (at, section) in object.sections.iter().enumerate() {
            row.clear();
            for fixup in object.section_fixups(section) {
                row.push(fixup.symbol);
                row.extend(fixup.minus);
            }
            row.sort_unstable();
            row.dedup();
            pairs.extend(row.iter().map(|it| (at as u32, *it)));
        }
        let refs = Csr::build(sections, &pairs);
        pairs.clear();
        for (at, symbol) in object.symbols.iter().enumerate() {
            if let SymbolDef::Section { section, .. } = symbol.def {
                pairs.push((section, at as u32));
            }
        }
        let defs = Csr::build(sections, &pairs);
        let mut group_of = vec![NONE; sections];
        for (at, group) in object.groups.iter().enumerate() {
            for section in object.group_sections(group) {
                let it = &mut group_of[*section as usize];
                if *it == NONE {
                    *it = at as u32;
                }
            }
        }
        let (mut roots, mut unwind, mut named_sections) = (Vec::new(), Vec::new(), Vec::new());
        let mut names = named.iter();
        for (at, section) in object.sections.iter().enumerate() {
            let at = at as u32;
            match &section.what {
                SectionKind::Dropped | SectionKind::EhFrame => continue,
                SectionKind::Unwind(_) => {
                    let first = object.section_fixups(section).iter().min_by_key(|it| it.at);
                    unwind.push((at, first.map_or(NONE, |it| it.symbol)));
                }
                SectionKind::Alloc(Class::Named { .. }, _) => {
                    named_sections.push((*names.next().expect("a name per named section"), at));
                }
                SectionKind::Alloc(..) => {}
            }
            let root = section.retain
                || kept_by_name(object.name(section.name))
                || matches!(section.what, SectionKind::Alloc(Class::InitArray | Class::FiniArray, _));
            if root {
                roots.push(at);
            }
        }
        let mut frames: Vec<Frame> = Vec::new();
        let mut fixup_pairs: Vec<(u32, u32)> = Vec::new();
        let mut by_at: Vec<(u64, u32)> = Vec::new();
        for (section, records) in records {
            let s = &object.sections[*section as usize];
            by_at.clear();
            by_at.extend((s.fixups.start..s.fixups.end).map(|it| (object.fixups[it as usize].at, it)));
            radix::sort_copy_by_key(&mut by_at, |&(at, it)| radix::pack_64(at, u64::from(it)));
            let first = frames.len();
            for record in records {
                let (start, end) = (record.range.start as u64, record.range.end as u64);
                let (lo, hi) = (by_at.partition_point(|it| it.0 < start), by_at.partition_point(|it| it.0 < end));
                let index = frames.len() as u32;
                let mut function = NONE;
                for &(at, fixup) in &by_at[lo..hi] {
                    fixup_pairs.push((index, fixup));
                    if at == start + 8 && function == NONE {
                        function = object.fixups[fixup as usize].symbol;
                    }
                }
                let cie = record.cie.map_or(NONE, |cie| {
                    frames[first..]
                        .binary_search_by_key(&(cie as u32), |it| it.start)
                        .map_or(NONE, |it| (first + it) as u32)
                });
                frames.push(Frame {
                    section: *section,
                    start: start as u32,
                    end: end as u32,
                    fde: record.cie.is_some(),
                    cie,
                    function,
                });
            }
        }
        let frame_fixups = Csr::build(frames.len(), &fixup_pairs);
        let sections_at = object.symbols.len();
        let frames_at = sections_at + sections;
        debug_assert_eq!(syms.len(), frames_at + frames.len());
        Parsed {
            obj,
            base,
            syms,
            sections_at,
            frames_at,
            refs,
            defs,
            group_of,
            roots,
            unwind,
            named: named_sections,
            frames,
            frame_fixups,
            file,
        }
    }

    fn object(&self) -> &Object {
        match &*self.file {
            NativeFile::Object(it) => it,
            _ => unreachable!("only objects are parsed"),
        }
    }

    #[inline]
    fn sym(&self, symbol: u32) -> Sym {
        self.syms[symbol as usize]
    }

    #[inline]
    fn section_sym(&self, section: u32) -> Sym {
        self.syms[self.sections_at + section as usize]
    }

    #[inline]
    fn frame_sym(&self, frame: u32) -> Sym {
        self.syms[self.frames_at + frame as usize]
    }
}

/// The keys of an object's symbols, sections and frame records, in [`Parsed::new`]'s order.
fn object_keys<'f>(obj: Obj, object: &'f Object, records: &[(u32, Vec<unwind::Native>)], out: &mut Vec<Wide<'f>>) {
    for (at, symbol) in object.symbols.iter().enumerate() {
        out.push(match (symbol.binding, symbol.kind, symbol.def) {
            (_, SymbolKind::Section, SymbolDef::Section { section, .. }) => Wide::Key(SymKey::Section(obj, section)),
            (Binding::Local, _, _) => Wide::Key(SymKey::Local(obj, at as u32)),
            _ => Wide::Name(object.name(symbol.name)),
        });
    }
    out.extend((0..object.sections.len() as u32).map(|it| Wide::Key(SymKey::Section(obj, it))));
    for (section, records) in records {
        out.extend(records.iter().map(|it| Wide::Key(SymKey::Frame(obj, *section, it.range.start as u32))));
    }
}

/// What the shared libraries of a link export: every library they stand for, flattened, and
/// of each name the first library's symbol. Built when the libraries change, not per stage.
#[derive(Default)]
pub(crate) struct Exports {
    roots: Vec<(Arc<NativeFile>, bool)>,
    libraries: Vec<Library>,
    /// Of each library, the first of its name (an import library's members of one DLL).
    canonical: Vec<u32>,
    names: Names,
    versions: Vec<String>,
    symbols: Vec<Export>,
    table: Table,
}

#[derive(Clone, Copy)]
struct Export {
    name: Name,
    /// Its library's canonical index.
    library: u32,
    /// Index into `Exports::versions`; NONE for none.
    version: u32,
    kind: SymbolKind,
    weak: bool,
}

impl Exports {
    fn build(roots: Vec<(Arc<NativeFile>, bool)>) -> Result<Exports, String> {
        let mut flat: Vec<(&Shared, bool)> = Vec::new();
        for (file, as_needed) in &roots {
            let NativeFile::Shared(shared) = &**file else { continue };
            let mut at = flat.len();
            flat.push((shared, *as_needed));
            while at < flat.len() {
                let (this, as_needed) = flat[at];
                flat.extend(this.more.iter().map(|it| (it, as_needed)));
                at += 1;
            }
        }
        let mut first: FxHashMap<&str, u32> = FxHashMap::default();
        let canonical: Vec<u32> =
            flat.iter().enumerate().map(|(at, (it, _))| *first.entry(it.name.as_str()).or_insert(at as u32)).collect();
        let libraries = flat.iter().map(|(it, as_needed)| Library { name: it.name.clone(), as_needed: *as_needed }).collect();
        let count: usize = flat.iter().map(|it| it.0.symbols.len()).sum();
        let mut names = Names::with_capacity(flat.iter().map(|it| it.0.names.bytes()).sum());
        let mut table = Table::with_capacity(count);
        let mut symbols: Vec<Export> = Vec::with_capacity(count);
        let mut versions: Vec<String> = Vec::new();
        let mut version_of: FxHashMap<&str, u32> = FxHashMap::default();
        for (at, (shared, _)) in flat.iter().enumerate() {
            for symbol in &shared.symbols {
                let name = shared.symbol_name(symbol);
                let hashed = hash(name);
                if table.find(hashed, |it| names.at(symbols[it as usize].name) == name).is_some() {
                    continue;
                }
                let version = match shared.version(symbol) {
                    None => NONE,
                    Some(version) => *version_of.entry(version).or_insert_with(|| {
                        versions.push(version.to_owned());
                        (versions.len() - 1) as u32
                    }),
                };
                let stored = names.push(name)?;
                let index = symbols.len() as u32;
                symbols.push(Export { name: stored, library: canonical[at], version, kind: symbol.kind, weak: symbol.weak });
                let _ = table.insert(hashed, index, |_| false);
            }
        }
        Ok(Exports { roots, libraries, canonical, names, versions, symbols, table })
    }

    fn find(&self, name: &str) -> Option<&Export> {
        self.table
            .find(hash(name), |it| self.names.at(self.symbols[it as usize].name) == name)
            .map(|it| &self.symbols[it as usize])
    }

    fn version(&self, export: &Export) -> Option<String> {
        (export.version != NONE).then(|| self.versions[export.version as usize].clone())
    }

    /// Whether these are the libraries it was built from.
    fn same(&self, roots: &[(Arc<NativeFile>, bool)]) -> bool {
        self.roots.len() == roots.len() && self.roots.iter().zip(roots).all(|(a, b)| Arc::ptr_eq(&a.0, &b.0) && a.1 == b.1)
    }
}

// A slot's flags.
/// Rust defines the name.
const RUST: u16 = 1;
/// Archives are searched for it: strongly undefined in a loaded object, a searching
/// fallback, a directive's, or a strong root.
const PULL: u16 = 2;
/// The link must define it: `/INCLUDE`, `/EXPORT`, what a taken fallback falls back to.
const REQUIRED: u16 = 4;
/// Collection starts from it, and it is resolved: the entry, exports, directives.
const ROOT: u16 = 8;
/// Referred to strongly whatever Rust does: the entry, exports.
const STRONG: u16 = 16;
/// Nothing defines it: it is an alias of its fallback.
const ALIAS: u16 = 32;
const QUEUED: u16 = 64;
/// Collection starts from it, but it is resolved only when something refers to it:
/// `_init`, `_fini`.
const LOOSE: u16 = 128;
/// Rust refers to it (a root, resolved).
const WANTED: u16 = 256;

/// A global of the graph: a name loaded objects mention, or Rust refers to or defines.
#[derive(Clone, Copy)]
struct Slot {
    sym: Sym,
    /// The winning definition, `pack(load, symbol)`; NONE64 for none.
    best: u64,
    /// Of `best`: 1 common, 2 weak, 3 strong.
    strength: u8,
    flags: u16,
    /// The first `SymbolDef::Fallback` of the name, `pack(load, symbol)`; NONE64 for none.
    fallback: u64,
}

/// An object of the graph.
struct Load {
    parsed: Arc<Parsed>,
    /// Its index in [`State::inputs`].
    input: u32,
    /// An archive member pulled for a name.
    pulled: bool,
    /// Where its sections and frame records start in the graph's flat numbering.
    sections: u32,
    frames: u32,
    /// Of each symbol, its slot; NONE for a local.
    slots: Vec<u32>,
}

static GRAPHS: AtomicU64 = AtomicU64::new(1);

/// A resolution of the natives: the objects loaded, what every global is, which COMDAT groups
/// won, and the indexes collection walks. Built when the natives or Rust's definitions
/// change, shared by the stages after.
pub(crate) struct Graph {
    /// A new number each time a graph is built (tests read it).
    #[cfg_attr(not(test), allow(dead_code))]
    id: u64,
    loads: Vec<Load>,
    by_obj: FxHashMap<Obj, u32>,
    slot_of: FxHashMap<Sym, u32>,
    slots: Vec<Slot>,
    /// Sections (flat) of a group that lost.
    discarded: Bits,
    sections: u32,
    frames: u32,
    /// Of each flat section, the unwind sections (flat) and FDEs (flat) describing it.
    unwinds: Csr,
    fdes: Csr,
    /// `Class::Named` sections (flat) by name, sorted: a name's are a run.
    named: Vec<(NameId, u32)>,
    /// Sections (flat) kept whatever refers to them.
    roots: Vec<u32>,
    /// The archives searched for names, in link-line order: input and index.
    archives: Vec<(u32, Arc<Table>)>,
    exports: Arc<Exports>,
    /// What the objects' `/EXPORT` directives export.
    asked: Arc<Vec<(String, String, bool)>>,
    /// wasm: what objects say a name is imported as, and the input saying it.
    wasm: FxHashMap<Sym, (u32, blaze_codegen::wasm::Import)>,
    entry: Option<Sym>,
}

impl Graph {
    fn new() -> Graph {
        Graph {
            id: GRAPHS.fetch_add(1, Ordering::Relaxed),
            loads: Vec::new(),
            by_obj: FxHashMap::default(),
            slot_of: FxHashMap::default(),
            slots: Vec::new(),
            discarded: Bits::default(),
            sections: 0,
            frames: 0,
            unwinds: Csr::build(0, &[]),
            fdes: Csr::build(0, &[]),
            named: Vec::new(),
            roots: Vec::new(),
            archives: Vec::new(),
            exports: Arc::default(),
            asked: Arc::default(),
            wasm: FxHashMap::default(),
            entry: None,
        }
    }

    fn slot(&mut self, sym: Sym) -> u32 {
        let next = self.slots.len() as u32;
        match self.slot_of.entry(sym) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                it.insert(next);
                self.slots.push(Slot { sym, best: NONE64, strength: 0, flags: 0, fallback: NONE64 });
                next
            }
        }
    }

    #[inline]
    fn object(&self, load: u32) -> &Object {
        self.loads[load as usize].parsed.object()
    }

    #[inline]
    fn flat(&self, load: u32, section: u32) -> usize {
        (self.loads[load as usize].sections + section) as usize
    }

    fn unflat(&self, flat: u32) -> (u32, u32) {
        let load = self.loads.partition_point(|it| it.sections <= flat) - 1;
        (load as u32, flat - self.loads[load].sections)
    }

    fn unflat_frame(&self, flat: u32) -> (u32, u32) {
        let load = self.loads.partition_point(|it| it.frames <= flat) - 1;
        (load as u32, flat - self.loads[load].frames)
    }

    /// The winning definition, unless a later group's win dropped its section.
    fn best(&self, slot: u32) -> Option<(u32, u32)> {
        let it = self.slots[slot as usize].best;
        if it == NONE64 {
            return None;
        }
        let (load, symbol) = unpack(it);
        match self.object(load).symbols[symbol as usize].def {
            SymbolDef::Section { section, .. } if self.discarded.get(self.flat(load, section)) => None,
            _ => Some((load, symbol)),
        }
    }

    /// Whether a load's symbol is its name's winning definition.
    fn wins(&self, load: u32, symbol: u32) -> bool {
        let slot = self.loads[load as usize].slots[symbol as usize];
        slot != NONE && self.slots[slot as usize].best == pack(load, symbol) && self.best(slot).is_some()
    }

    /// Whether the natives or Rust define the name.
    fn defined(&self, slot: u32) -> bool {
        self.slots[slot as usize].flags & (RUST | ALIAS) != 0 || self.best(slot).is_some()
    }

    /// What a defined name is, as [`Outcome`] kinds.
    fn kind(&self, slot: u32) -> u8 {
        if self.slots[slot as usize].flags & RUST != 0 {
            return O_RUST;
        }
        match self.best(slot) {
            Some((load, symbol))
                if !self.loads[load as usize].pulled
                    && !matches!(self.object(load).symbols[symbol as usize].def, SymbolDef::Common { .. }) =>
            {
                O_FIXED
            }
            _ => O_PULLED,
        }
    }

    /// The section a load's symbol is defined in: its own for a local, else its name's.
    fn target(&self, load: u32, symbol: u32) -> Option<(u32, u32)> {
        let it = &self.object(load).symbols[symbol as usize];
        if is_local(it) {
            return match it.def {
                SymbolDef::Section { section, .. } => Some((load, section)),
                _ => None,
            };
        }
        self.home(self.loads[load as usize].slots[symbol as usize])
    }

    /// The section a name is defined in, through the fallbacks it is an alias of.
    fn home(&self, mut slot: u32) -> Option<(u32, u32)> {
        let at = |load: u32, symbol: u32| match self.object(load).symbols[symbol as usize].def {
            SymbolDef::Section { section, .. } => Some((load, section)),
            _ => None,
        };
        for _ in 0..64 {
            let it = &self.slots[slot as usize];
            if it.flags & RUST != 0 {
                return None;
            }
            if let Some((load, symbol)) = self.best(slot) {
                return at(load, symbol);
            }
            if it.flags & ALIAS == 0 {
                return None;
            }
            let (load, symbol) = unpack(it.fallback);
            if is_local(&self.object(load).symbols[symbol as usize]) {
                return at(load, symbol);
            }
            slot = self.loads[load as usize].slots[symbol as usize];
        }
        None
    }

    /// The indexes collection walks: describers, named sections, roots.
    fn index(&mut self) {
        let (mut unwinds, mut fdes, mut named, mut roots) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (load, it) in self.loads.iter().enumerate() {
            let load = load as u32;
            for (section, symbol) in &it.parsed.unwind {
                if *symbol == NONE {
                    continue;
                }
                if let Some((l, s)) = self.target(load, *symbol) {
                    unwinds.push((self.flat(l, s) as u32, it.sections + section));
                }
            }
            for (at, frame) in it.parsed.frames.iter().enumerate() {
                if !frame.fde || frame.function == NONE {
                    continue;
                }
                if let Some((l, s)) = self.target(load, frame.function) {
                    fdes.push((self.flat(l, s) as u32, it.frames + at as u32));
                }
            }
            named.extend(it.parsed.named.iter().map(|(name, section)| (*name, it.sections + section)));
            roots.extend(it.parsed.roots.iter().map(|section| it.sections + section));
        }
        radix::sort_copy_by_key(&mut named, |&(name, flat)| radix::pack_32(name.0, flat));
        self.unwinds = Csr::build(self.sections as usize, &unwinds);
        self.fdes = Csr::build(self.sections as usize, &fdes);
        self.named = named;
        self.roots = roots;
    }
}

// What a resolved name was ([`Resolved::outcomes`]): a kind in the low 3 bits, then flags.
/// Rust defines it.
const O_RUST: u8 = 1;
/// An object loaded whatever is undefined defines it.
const O_FIXED: u8 = 2;
/// A pulled member defines it, or it is an alias or a common.
const O_PULLED: u8 = 3;
/// An import.
const O_IMPORT: u8 = 4;
/// An import from an `--as-needed` library.
const O_NEEDED: u8 = 5;
/// A special, 0 for a weak name, or left undefined.
const O_OTHER: u8 = 6;
/// A special that keeps sections: `__start_X`, `__imp_X`.
const O_MARKS: u8 = 7;
const O_KIND: u8 = 7;
/// Every reference to it was weak.
const O_WEAK: u8 = 8;
/// Required, or strongly referred to other than by Rust.
const O_STRONG: u8 = 16;
/// Referred to or rooted other than by Rust.
const O_OTHERS: u8 = 32;

/// What a change of how Rust refers to one name asks of a stage.
#[derive(PartialEq, Eq, Debug)]
enum Next {
    Nothing,
    /// A name the kept resolution never resolved.
    Gained,
    Rebuild,
}

/// What a name's Rust references going from `before` to `after` (0 none, 1 weak only, 2
/// strong) asks, given what it resolved to.
fn settled(outcome: u8, before: u8, after: u8, full: bool) -> Next {
    let kind = outcome & O_KIND;
    if kind == 0 {
        return if after > 0 { Next::Gained } else { Next::Nothing };
    }
    let still = after > 0 || outcome & O_OTHERS != 0;
    let flips = (outcome & O_STRONG == 0 && after != 2) != (outcome & O_WEAK != 0);
    let rebuild = match kind {
        O_RUST => false,
        // A root went: what it kept may go.
        O_FIXED | O_MARKS => after == 0,
        // The member may no longer be pulled.
        O_PULLED => after < before,
        // A full link drops an import nothing refers to.
        O_IMPORT => if still { flips } else { full },
        O_NEEDED => !still || flips,
        _ => still && flips,
    };
    if rebuild { Next::Rebuild } else { Next::Nothing }
}

/// How many of Rust's pieces refer to each name, strongly and weakly, and what each counted
/// piece referred to, so a removed one is taken off whatever became of its lists.
#[derive(Clone, Default)]
struct Refs {
    /// By [`Symbols::dense`]: strong and weak references.
    counts: CowVec<[u32; 2]>,
    /// By piece: its names, each `sym | weak << 32`.
    of: CowVec<ListRef>,
    lists: CowList<u64>,
    /// Every name ever referred to, once.
    names: CowVec<Sym>,
    listed: Bits,
}

impl Refs {
    /// 0 when Rust does not refer to the name, 1 when only weakly, 2 when strongly.
    fn class(&self, sym: Sym) -> u8 {
        match self.counts.get(Symbols::dense(sym)) {
            Some([strong, _]) if *strong > 0 => 2,
            Some([_, weak]) if *weak > 0 => 1,
            _ => 0,
        }
    }

    fn count(&mut self, entry: u64, by: i32) {
        let sym = Sym(entry as u32);
        let at = Symbols::dense(sym);
        let mut it = self.counts.get(at).copied().unwrap_or([0; 2]);
        let weak = (entry >> 32) as usize;
        it[weak] = it[weak].wrapping_add_signed(by);
        self.counts.set(at, it, || [0; 2]);
        if self.listed.set(at) {
            self.names.push(sym);
        }
    }

    /// Copies the live lists into a fresh arena.
    fn compact(&mut self) {
        let mut lists = CowList::default();
        for id in 0..self.of.len() {
            let list = *self.of.get(id).expect("in range");
            if !list.is_empty() {
                let fresh = lists.push(self.lists.get(list));
                self.of.set(id, fresh, || ListRef::EMPTY);
            }
        }
        self.lists = lists;
    }
}

/// Whether a piece's references are Rust's, not the natives' or the linker's.
fn counted(what: What) -> bool {
    !matches!(
        what,
        What::Native { .. } | What::NativeFrame { .. } | What::Got { .. } | What::Plt { .. } | What::Common
    )
}

/// What collection kept, over a [`Graph`]'s flat numbering.
#[derive(Clone, Default)]
pub(crate) struct Marks {
    sections: Bits,
    frames: Bits,
    /// Runs of `Graph::named` kept whole, by their first entry.
    named: Bits,
    /// Of each slot, what its name keeps as a writer's special, once asked: UNKNOWN,
    /// NOTHING, `NAMED | run`, or the slot `__imp_X`'s X is.
    special: CowVec<u32>,
    /// Of each slot: 1 when kept native code refers to it only weakly, 2 strongly.
    native: CowVec<u8>,
}

const UNKNOWN: u32 = u32::MAX;
const NOTHING: u32 = u32::MAX - 1;
const NAMED: u32 = 1 << 31;

impl Marks {
    fn native_of(&self, slot: u32) -> u8 {
        self.native.get(slot as usize).copied().unwrap_or(0)
    }
}

/// What a name keeps as a writer's special: the `Class::Named` sections `__start_X` bounds
/// (`NAMED | their run`), the definition `__imp_X` is the cell of (X's slot), or NOTHING.
fn special_mark(graph: &Graph, symbols: &Symbols, writer: &dyn Writer, sym: Sym) -> u32 {
    let Some(name) = symbols.name_of(sym) else { return NOTHING };
    match writer.special(name) {
        Some(Special::Section { name, .. }) => symbols
            .find_name(&name)
            .and_then(|id| {
                let at = graph.named.partition_point(|it| it.0 < id);
                graph.named.get(at).is_some_and(|it| it.0 == id).then_some(NAMED | at as u32)
            })
            .unwrap_or(NOTHING),
        Some(Special::Got(target)) => {
            symbols.find_sym_name(&target).and_then(|it| graph.slot_of.get(&it).copied()).unwrap_or(NOTHING)
        }
        _ => NOTHING,
    }
}

/// A walk of collection: from roots, through relocations, groups and what describes a kept
/// section, marking what it reaches once.
struct Marker<'g> {
    graph: &'g Graph,
    marks: &'g mut Marks,
    symbols: &'g Symbols,
    writer: &'g dyn Writer,
    work: Vec<u64>,
    /// What this walk marked: `pack(load, section)`, `pack(load, frame)`.
    sections: Vec<u64>,
    frames: Vec<u64>,
}

impl<'g> Marker<'g> {
    fn new(graph: &'g Graph, marks: &'g mut Marks, symbols: &'g Symbols, writer: &'g dyn Writer) -> Marker<'g> {
        Marker { graph, marks, symbols, writer, work: Vec::new(), sections: Vec::new(), frames: Vec::new() }
    }

    fn push(&mut self, (load, section): (u32, u32)) {
        self.work.push(pack(load, section));
    }

    fn run(&mut self) {
        let graph = self.graph;
        while let Some(item) = self.work.pop() {
            let (load, section) = unpack(item);
            let flat = graph.flat(load, section);
            if graph.discarded.get(flat) || !self.marks.sections.set(flat) {
                continue;
            }
            self.sections.push(item);
            let parsed = &graph.loads[load as usize].parsed;
            let object = parsed.object();
            let group = parsed.group_of[section as usize];
            if group != NONE {
                for it in object.group_sections(&object.groups[group as usize]) {
                    self.work.push(pack(load, *it));
                }
            }
            for symbol in parsed.refs.get(section as usize) {
                self.symbol(load, *symbol);
            }
            for it in graph.unwinds.get(flat) {
                let at = graph.unflat(*it);
                self.push(at);
            }
            for it in graph.fdes.get(flat) {
                let (l, f) = graph.unflat_frame(*it);
                self.frame(l, f);
            }
        }
    }

    /// A relocation's symbol: its section, and what its name keeps as a special.
    fn symbol(&mut self, load: u32, symbol: u32) {
        let graph = self.graph;
        if let Some(at) = graph.target(load, symbol) {
            self.push(at);
        }
        let slot = graph.loads[load as usize].slots[symbol as usize];
        if slot != NONE {
            self.special(slot);
        }
    }

    /// A name: its definition's section, and what it keeps as a special.
    fn name(&mut self, slot: u32) {
        if let Some(at) = self.graph.home(slot) {
            self.push(at);
        }
        self.special(slot);
    }

    fn special(&mut self, slot: u32) {
        let mut it = self.marks.special.get(slot as usize).copied().unwrap_or(UNKNOWN);
        if it == UNKNOWN {
            it = special_mark(self.graph, self.symbols, self.writer, self.graph.slots[slot as usize].sym);
            self.marks.special.set(slot as usize, it, || UNKNOWN);
        }
        if it == NOTHING {
            return;
        }
        let graph = self.graph;
        if it & NAMED != 0 {
            let run = (it & !NAMED) as usize;
            if self.marks.named.set(run) {
                let name = graph.named[run].0;
                for (n, flat) in &graph.named[run..] {
                    if *n != name {
                        break;
                    }
                    let at = graph.unflat(*flat);
                    self.push(at);
                }
            }
        } else if let Some(at) = graph.home(it) {
            self.push(at);
        }
    }

    /// An FDE kept for what it describes, and its CIE.
    fn frame(&mut self, load: u32, frame: u32) {
        let graph = self.graph;
        let base = graph.loads[load as usize].frames;
        if !self.marks.frames.set((base + frame) as usize) {
            return;
        }
        self.frames.push(pack(load, frame));
        let cie = graph.loads[load as usize].parsed.frames[frame as usize].cie;
        if cie != NONE && self.marks.frames.set((base + cie) as usize) {
            self.frames.push(pack(load, cie));
            self.frame_fixups(load, cie);
        }
        self.frame_fixups(load, frame);
    }

    fn frame_fixups(&mut self, load: u32, frame: u32) {
        let parsed = &self.graph.loads[load as usize].parsed;
        let object = parsed.object();
        for fixup in parsed.frame_fixups.get(frame as usize) {
            self.symbol(load, object.fixups[*fixup as usize].symbol);
        }
    }
}

/// Notes the names what a walk marked refers to; the slots whose note rose, from and to.
fn note_native(graph: &Graph, marks: &mut Marks, sections: &[u64], frames: &[u64], rose: &mut Vec<(u32, u8, u8)>) {
    let mut note = |marks: &mut Marks, load: u32, symbol: u32| {
        let it = &graph.loads[load as usize];
        let slot = it.slots[symbol as usize];
        if slot == NONE {
            return;
        }
        let level = if it.parsed.object().symbols[symbol as usize].binding == Binding::Weak { 1 } else { 2 };
        let was = marks.native_of(slot);
        if level > was {
            marks.native.set(slot as usize, level, || 0);
            rose.push((slot, was, level));
        }
    };
    for (load, section) in sections.iter().map(|it| unpack(*it)) {
        let parsed = &graph.loads[load as usize].parsed;
        for symbol in parsed.refs.get(section as usize) {
            note(marks, load, *symbol);
        }
    }
    for (load, frame) in frames.iter().map(|it| unpack(*it)) {
        let parsed = &graph.loads[load as usize].parsed;
        let object = parsed.object();
        for fixup in parsed.frame_fixups.get(frame as usize) {
            note(marks, load, object.fixups[*fixup as usize].symbol);
        }
    }
}

/// The symbols other than its own a kept section defines: its locals, and the globals it is
/// the winning definition of.
fn section_defines(graph: &Graph, load: u32, section: u32, out: &mut Vec<Sym>) {
    out.clear();
    let parsed = &graph.loads[load as usize].parsed;
    let object = parsed.object();
    let own = parsed.section_sym(section);
    for symbol in parsed.defs.get(section as usize) {
        let sym = parsed.sym(*symbol);
        if sym != own && (is_local(&object.symbols[*symbol as usize]) || graph.wins(load, *symbol)) {
            out.push(sym);
        }
    }
    out.sort_unstable();
    out.dedup();
}

/// What is left of an undefined name.
enum Decision {
    Special(Special),
    /// An import, and its library's canonical index (NONE for none).
    Import(Import, u32),
    /// 0: every reference is weak.
    Zero,
    /// Left undefined: an optional name.
    Skip,
}

/// What resolving the native inputs and the names gave, and what the next stage reuses.
#[derive(Clone, Default)]
pub(crate) struct Resolved {
    pub imports: Arc<Vec<Import>>,
    /// Each import's symbol.
    pub import_syms: Arc<Vec<Sym>>,
    pub libraries: Arc<Vec<Library>>,
    /// What the objects' `/EXPORT` directives export: the name, the symbol, and whether it
    /// is data.
    pub exports: Arc<Vec<(String, String, bool)>>,
    /// Whether an import is kept in place for an incremental link that no longer needs it.
    stale: bool,
    refs: Refs,
    /// The piece count and removals Rust's references were counted to.
    pieces: u32,
    removals: u64,
    /// What it was resolved from.
    inputs: Arc<Vec<Input>>,
    names: Arc<BTreeMap<NameId, Sym>>,
    wasm: Arc<FxHashMap<NameId, blaze_codegen::wasm::Import>>,
    graph: Option<Arc<Graph>>,
    marks: Marks,
    /// Of each name resolved, by [`Symbols::dense`]: `O_*` bits.
    outcomes: CowVec<u8>,
    /// The names `outcomes` holds.
    named: CowVec<Sym>,
    /// The names resolution defined.
    defined: CowVec<Sym>,
    /// Of each library of the graph's exports, its index in `libraries`; NONE when not needed.
    library_index: Arc<Vec<u32>>,
    /// The commons made: the name, size, alignment, sorted by name.
    commons: Arc<Vec<(Sym, u64, u64)>>,
}

impl Resolved {
    fn outcome(&self, sym: Sym) -> u8 {
        self.outcomes.get(Symbols::dense(sym)).copied().unwrap_or(0)
    }
}

/// A number that changes exactly when a stage built its resolution's graph again.
#[cfg(test)]
pub(crate) fn resolution_id(resolved: &Resolved) -> u64 {
    resolved.graph.as_ref().map_or(0, |it| it.id)
}

/// A resolution being built.
struct Build {
    graph: Graph,
    /// Slots to look for in the archives after the round.
    queue: Vec<u32>,
    pulled: FxHashSet<(u32, u32)>,
    /// COMDAT groups by name: the winner's `(load, group)` in `group_list`.
    groups: Table,
    group_list: Vec<(u32, u32)>,
    mismatches: FxHashMap<String, (String, u32)>,
    asked: FxHashSet<String>,
    shared: Vec<(Arc<NativeFile>, bool)>,
    /// Import libraries' members, in load order.
    members: Vec<Arc<NativeFile>>,
    commons: FxHashMap<u32, (u64, u64)>,
}

impl Build {
    fn new() -> Build {
        Build {
            graph: Graph::new(),
            queue: Vec::new(),
            pulled: FxHashSet::default(),
            groups: Table::default(),
            group_list: Vec::new(),
            mismatches: FxHashMap::default(),
            asked: FxHashSet::default(),
            shared: Vec::new(),
            members: Vec::new(),
            commons: FxHashMap::default(),
        }
    }

    fn slot(&mut self, sym: Sym) -> u32 {
        self.graph.slot(sym)
    }

    fn flag(&mut self, slot: u32, flags: u16) {
        let it = &mut self.graph.slots[slot as usize];
        it.flags |= flags;
        if flags & PULL != 0 && it.flags & QUEUED == 0 {
            it.flags |= QUEUED;
            self.queue.push(slot);
        }
    }

    /// Drops a group's sections.
    fn discard(&mut self, load: u32, group: u32) {
        let Graph { loads, discarded, .. } = &mut self.graph;
        let it = &loads[load as usize];
        let object = it.parsed.object();
        for section in object.group_sections(&object.groups[group as usize]) {
            discarded.set((it.sections + section) as usize);
        }
    }

    /// Weak externals left undefined become aliases of what they fall back to, which the
    /// link must then define; a chain back to itself is an error.
    fn aliases(&mut self, symbols: &Symbols) -> Result<(), LinkError> {
        let graph = &mut self.graph;
        for slot in 0..graph.slots.len() as u32 {
            let it = graph.slots[slot as usize];
            if it.fallback != NONE64 && it.flags & RUST == 0 && graph.best(slot).is_none() {
                graph.slots[slot as usize].flags |= ALIAS;
            }
        }
        for slot in 0..graph.slots.len() as u32 {
            if graph.slots[slot as usize].flags & ALIAS == 0 {
                continue;
            }
            let mut seen: SmallVec<[u32; 8]> = SmallVec::new();
            let mut at = slot;
            loop {
                let (load, symbol) = unpack(graph.slots[at as usize].fallback);
                if is_local(&graph.object(load).symbols[symbol as usize]) {
                    break;
                }
                let next = graph.loads[load as usize].slots[symbol as usize];
                if next == slot {
                    let name = symbols.name_of(graph.slots[slot as usize].sym).unwrap_or("?");
                    return Err(error(None, format!("the weak external `{name}` falls back to itself")));
                }
                if seen.contains(&next) || graph.slots[next as usize].flags & ALIAS == 0 {
                    break;
                }
                seen.push(next);
                at = next;
            }
            let (load, symbol) = unpack(graph.slots[slot as usize].fallback);
            let target = graph.loads[load as usize].slots[symbol as usize];
            if target != NONE {
                graph.slots[target as usize].flags |= REQUIRED;
            }
        }
        Ok(())
    }
}

/// A stage's scratch lists for making pieces.
#[derive(Default)]
struct Scratch {
    fixups: Vec<Fixup>,
    weak: Vec<Sym>,
    defines: Vec<Sym>,
}

impl<'a> Stage<'a> {
    /// The native inputs the change adds, removes and moves: each new one read and checked
    /// once, in parallel.
    pub fn inputs(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        if change.natives.is_empty() && change.removed_natives.is_empty() {
            return Ok(());
        }
        // An archive or library twice on the line is searched once (lld searches every
        // archive whatever its place); an object twice is an error.
        let mut given: FxHashMap<blaze_ids::ContentId, usize> =
            FxHashMap::with_capacity_and_hasher(change.natives.len(), Default::default());
        let mut natives = Vec::with_capacity(change.natives.len());
        for (at, native) in change.natives.iter().enumerate() {
            match given.entry(native.id) {
                Entry::Vacant(it) => {
                    it.insert(at);
                    natives.push(at);
                }
                Entry::Occupied(_) if matches!(native.kind, NativeKind::Object) => {
                    return Err(error(None, format!("{}: the object is on the link line twice", native.name)));
                }
                Entry::Occupied(_) => {}
            }
        }
        let removed: FxHashSet<blaze_ids::ContentId> = change.removed_natives.iter().copied().collect();
        let inputs = Arc::make_mut(&mut self.new.inputs);
        inputs.retain(|it| !removed.contains(&it.id));
        let have: FxHashMap<blaze_ids::ContentId, usize> = inputs.iter().enumerate().map(|(at, it)| (it.id, at)).collect();
        let mut fresh = Vec::new();
        for at in natives {
            let native = &change.natives[at];
            match have.get(&native.id) {
                Some(it) => inputs[*it].order = native.order,
                None => fresh.push(at),
            }
        }
        let writer = self.writer;
        let listed = &change.natives;
        let files: Vec<Result<NativeFile, LinkError>> = fresh
            .par_iter()
            .map(|at| {
                let native = &listed[*at];
                let file = writer.read(native.name, native.bytes, &native.kind).map_err(|it| error(None, format!("{}: {it}", native.name)))?;
                check_file(&file, native.bytes.len()).map_err(|it| error(None, format!("{}: {it}", native.name)))?;
                Ok(file)
            })
            .collect();
        let mut bytes: FxHashMap<blaze_ids::ContentId, &'a [u8]> = FxHashMap::default();
        for (at, file) in fresh.iter().zip(files) {
            let native = &change.natives[*at];
            inputs.push(Input {
                id: native.id,
                name: native.name.into(),
                kind: native.kind,
                order: native.order,
                bytes: native.bytes.into(),
                file: Arc::new(file?),
            });
            bytes.insert(native.id, native.bytes);
        }
        inputs.sort_by_key(|it| (it.order, it.id));
        let names: FxHashSet<&str> = inputs.iter().map(|it| &*it.name).collect();
        for input in inputs.iter() {
            let NativeFile::Shared(shared) = &*input.file else { continue };
            for wanted in &shared.inputs {
                let found = names.contains(wanted.as_str())
                    || inputs.iter().any(|it| {
                        it.name.len() > wanted.len()
                            && it.name.ends_with(wanted.as_str())
                            && it.name.as_bytes()[it.name.len() - wanted.len() - 1] == b'/'
                    });
                if !found {
                    return Err(error(None, format!("{} names {wanted}, which the link line does not have", input.name)));
                }
            }
        }
        let old: &'a State = self.old;
        for it in old.inputs.iter() {
            bytes.entry(it.id).or_insert(&it.bytes[..]);
        }
        self.native.bytes = inputs.iter().map(|it| bytes[&it.id]).collect();
        self.native.filled = true;
        if !removed.is_empty() {
            let gone: FxHashSet<u32> =
                removed.iter().filter(|it| !given.contains_key(*it)).filter_map(|it| self.symbols.find_id(it)).map(|it| it.0).collect();
            if !gone.is_empty() {
                self.objects.retain(|obj, _| !gone.contains(&obj.native.0));
            }
        }
        Ok(())
    }

    /// Each input's bytes, when [`Stage::inputs`] did not fill them: the image's.
    fn pass(&mut self) {
        if self.native.filled {
            return;
        }
        let old: &'a State = self.old;
        debug_assert!(Arc::ptr_eq(&old.inputs, &self.new.inputs));
        self.native.bytes = old.inputs.iter().map(|it| &it.bytes[..]).collect();
        self.native.filled = true;
    }

    /// A section's bytes, or `within` them: borrowed from the input, or copied to the arena
    /// when the reader made them; none for zeros.
    fn content(&self, data: &Data, input: u32, base: usize, within: Range<usize>) -> Content<'a> {
        match data {
            Data::Input(range) => {
                let bytes: &'a [u8] = self.native.bytes[input as usize];
                let start = base + range.start as usize;
                Content::Bytes(&bytes[start + within.start..start + within.end])
            }
            Data::Bytes(bytes) => {
                let arena: &'a bumpalo::Bump = self.arena;
                Content::Bytes(arena.alloc_slice_copy(&bytes[within]))
            }
            Data::Zero(_) => Content::Zero,
        }
    }

    fn piece_of(&self, sym: Sym) -> Option<PieceId> {
        match self.new.def(sym) {
            Def::Piece { piece, .. } if self.new.piece(piece).is_some() => Some(piece),
            _ => None,
        }
    }

    /// The objects and import libraries at `wanted` (input, and 0 for the input itself or
    /// member + 1): kept ones as they are, the rest read, checked, interned and indexed, the
    /// reading and indexing in parallel.
    fn read(&mut self, wanted: &[(u32, u32)]) -> Result<Vec<Read>, LinkError> {
        let mut objs = Vec::with_capacity(wanted.len());
        for (input, member) in wanted {
            let id = self.new.inputs[*input as usize].id;
            objs.push(self.symbols.obj(&id, *member));
        }
        let mut out: Vec<Option<Read>> = objs.iter().map(|it| self.objects.get(it).cloned()).collect();
        let misses: Vec<usize> = (0..wanted.len()).filter(|it| out[*it].is_none()).collect();
        if !misses.is_empty() {
            let fresh = self.read_misses(wanted, &objs, &misses)?;
            for (at, read) in misses.into_iter().zip(fresh) {
                self.objects.insert(objs[at], read.clone());
                out[at] = Some(read);
            }
        }
        Ok(out.into_iter().map(|it| it.expect("read")).collect())
    }

    fn read_misses(&mut self, wanted: &[(u32, u32)], objs: &[Obj], misses: &[usize]) -> Result<Vec<Read>, LinkError> {
        struct Got {
            file: Arc<NativeFile>,
            base: usize,
            records: Vec<(u32, Vec<unwind::Native>)>,
        }
        let writer = self.writer;
        let inputs = self.new.inputs.clone();
        let bytes: &[&'a [u8]] = &self.native.bytes;
        let got: Vec<Result<Got, LinkError>> = misses
            .par_iter()
            .map(|at| {
                let (input, member) = wanted[*at];
                let it = &inputs[input as usize];
                let all = bytes[input as usize];
                let (file, base, name) = if member == 0 {
                    (it.file.clone(), 0, None)
                } else {
                    let NativeFile::Archive(archive) = &*it.file else { unreachable!("a member of an archive") };
                    let entry = &archive.members[member as usize - 1];
                    let name = format!("{}({})", it.name, archive.name(entry.name));
                    let slice = &all[entry.bytes.clone()];
                    let file = writer.read(&name, slice, &NativeKind::Object).map_err(|what| error(None, format!("{name}: {what}")))?;
                    check_file(&file, slice.len()).map_err(|what| error(None, format!("{name}: {what}")))?;
                    if matches!(file, NativeFile::Archive(_)) {
                        return Err(error(None, format!("{name}: an archive in an archive")));
                    }
                    (Arc::new(file), entry.bytes.start, Some(name))
                };
                let mut records = Vec::new();
                if let NativeFile::Object(object) = &*file {
                    for (at, section) in object.sections.iter().enumerate() {
                        if section.what != SectionKind::EhFrame {
                            continue;
                        }
                        let data = match &section.data {
                            Data::Input(range) => &all[base + range.start as usize..base + range.end as usize],
                            Data::Bytes(bytes) => &bytes[..],
                            Data::Zero(_) => continue,
                        };
                        let split = unwind::split(data).map_err(|what| {
                            let name = name.as_deref().unwrap_or(&*it.name);
                            error(None, format!("{name}: {}: {what}", object.name(section.name)))
                        })?;
                        records.push((at as u32, split));
                    }
                }
                Ok(Got { file, base, records })
            })
            .collect();
        let got: Vec<Got> = got.into_iter().collect::<Result<_, _>>()?;
        // One batch for every object's symbols, sections and frame records, and one for the
        // names of its named sections.
        let mut starts = Vec::with_capacity(got.len() + 1);
        let syms;
        let name_ids;
        {
            let mut keys: Vec<Wide<'_>> = Vec::new();
            let mut names: Vec<&str> = Vec::new();
            for (it, at) in got.iter().zip(misses) {
                starts.push((keys.len(), names.len()));
                let NativeFile::Object(object) = &*it.file else { continue };
                object_keys(objs[*at], object, &it.records, &mut keys);
                for section in &object.sections {
                    if matches!(section.what, SectionKind::Alloc(Class::Named { .. }, _)) {
                        names.push(object.name(section.name));
                    }
                }
            }
            starts.push((keys.len(), names.len()));
            syms = self.symbols.intern_wide(&keys);
            name_ids = self.symbols.names_all(&names);
        }
        let reads: Vec<Read> = got
            .into_par_iter()
            .zip(misses.par_iter())
            .enumerate()
            .map(|(n, (it, at))| {
                if matches!(*it.file, NativeFile::Shared(_)) {
                    return Read::Shared(it.file);
                }
                let ((keys, names), (keys_end, names_end)) = (starts[n], starts[n + 1]);
                let parsed = Parsed::new(
                    objs[*at],
                    it.file,
                    it.base,
                    syms[keys..keys_end].to_vec(),
                    &name_ids[names..names_end],
                    &it.records,
                );
                Read::Object(Arc::new(parsed))
            })
            .collect();
        Ok(reads)
    }

    /// The indexes of the searched archives, kept ones as they are, the rest built in
    /// parallel.
    fn indexes(&mut self, inputs: &[Input], lazy: &[u32]) -> Vec<(u32, Arc<Table>)> {
        let objs: Vec<Obj> = lazy.iter().map(|it| self.symbols.obj(&inputs[*it as usize].id, 0)).collect();
        let kept: Vec<Option<Arc<Table>>> = objs
            .iter()
            .map(|obj| match self.objects.get(obj) {
                Some(Read::Index(it)) => Some(it.clone()),
                _ => None,
            })
            .collect();
        let built: Vec<Arc<Table>> = lazy
            .par_iter()
            .zip(kept.into_par_iter())
            .map(|(input, kept)| {
                kept.unwrap_or_else(|| {
                    let NativeFile::Archive(archive) = &*inputs[*input as usize].file else {
                        unreachable!("searched inputs are archives")
                    };
                    Arc::new(archive_index(archive))
                })
            })
            .collect();
        for (obj, table) in objs.iter().zip(&built) {
            self.objects.entry(*obj).or_insert_with(|| Read::Index(table.clone()));
        }
        lazy.iter().copied().zip(built).collect()
    }

    /// Adds an object to the graph: its groups, directives, imports and globals, in lld's
    /// order of precedence (the graph's loads are in link-line order, then pull rounds).
    fn register(&mut self, b: &mut Build, parsed: Arc<Parsed>, input: u32, pulled: bool) -> Result<(), LinkError> {
        let object = parsed.object();
        let load = b.graph.loads.len() as u32;
        let mut slots = vec![NONE; object.symbols.len()];
        for (at, symbol) in object.symbols.iter().enumerate() {
            if !is_local(symbol) {
                slots[at] = b.slot(parsed.sym(at as u32));
            }
        }
        let (sections, frames) = (b.graph.sections, b.graph.frames);
        b.graph.sections += object.sections.len() as u32;
        b.graph.frames += parsed.frames.len() as u32;
        b.graph.by_obj.insert(parsed.obj, load);
        b.graph.loads.push(Load { parsed: parsed.clone(), input, pulled, sections, frames, slots });
        let name_of = |this: &Self, input: u32| this.new.inputs[input as usize].name.clone();

        let size = |graph: &Graph, load: u32, group: u32| -> u64 {
            let object = graph.object(load);
            object.group_sections(&object.groups[group as usize]).first().map_or(0, |it| object.sections[*it as usize].data.len())
        };
        for (index, group) in object.groups.iter().enumerate() {
            let index = index as u32;
            let name = object.name(group.name);
            let entry = b.group_list.len() as u32;
            let (graph, list) = (&b.graph, &b.group_list);
            let found = b.groups.insert(hash(name), entry, |it| {
                let (l, g) = list[it as usize];
                let o = graph.object(l);
                o.name(o.groups[g as usize].name) == name
            });
            match found {
                Ok(()) => b.group_list.push((load, index)),
                Err(winner) => {
                    let (wl, wg) = b.group_list[winner as usize];
                    if wl == load {
                        continue;
                    }
                    if group.largest && size(&b.graph, load, index) > size(&b.graph, wl, wg) {
                        b.discard(wl, wg);
                        b.group_list[winner as usize] = (load, index);
                    } else {
                        b.discard(load, index);
                    }
                }
            }
        }

        for directive in &object.directives {
            match directive {
                Directive::Include(name) => {
                    let slot = b.slot(self.symbols.sym_name(name));
                    b.flag(slot, PULL | REQUIRED | ROOT);
                }
                Directive::Export { name, symbol, data } => {
                    let slot = b.slot(self.symbols.sym_name(symbol));
                    b.flag(slot, PULL | REQUIRED | ROOT);
                    if b.asked.insert(name.clone()) {
                        Arc::make_mut(&mut b.graph.asked).push((name.clone(), symbol.clone(), *data));
                    }
                }
                Directive::Mismatch { key, value } => match b.mismatches.get(key) {
                    Some((first, by)) if first != value => {
                        return Err(error(
                            None,
                            format!(
                                "`{key}` is `{first}` in {} and `{value}` in {} (/FAILIFMISMATCH)",
                                name_of(self, *by),
                                name_of(self, input)
                            ),
                        ));
                    }
                    Some(_) => {}
                    None => {
                        b.mismatches.insert(key.clone(), (value.clone(), input));
                    }
                },
                Directive::NoDefaultLib(_) => {}
            }
        }

        for (at, import) in &object.imports {
            let sym = parsed.sym(*at);
            match b.graph.wasm.get(&sym) {
                Some((by, known)) if known != import => {
                    let name = object.name(object.symbols[*at as usize].name);
                    return Err(error(
                        None,
                        format!("`{name}` is imported as {known:?} by {} and as {import:?} by {}", name_of(self, *by), name_of(self, input)),
                    ));
                }
                Some(_) => {}
                None => {
                    b.graph.wasm.insert(sym, (input, import.clone()));
                }
            }
        }

        for (at, symbol) in object.symbols.iter().enumerate() {
            if is_local(symbol) {
                continue;
            }
            let slot = b.graph.loads[load as usize].slots[at];
            let weak = symbol.binding == Binding::Weak;
            let strength = match symbol.def {
                SymbolDef::Undefined => {
                    if !weak {
                        b.flag(slot, PULL);
                    }
                    continue;
                }
                SymbolDef::Fallback { search, .. } => {
                    if search {
                        b.flag(slot, PULL);
                    }
                    let it = &mut b.graph.slots[slot as usize];
                    if it.fallback == NONE64 {
                        it.fallback = pack(load, at as u32);
                    }
                    continue;
                }
                SymbolDef::Section { section, .. } => {
                    if b.graph.discarded.get(b.graph.flat(load, section)) {
                        continue;
                    }
                    if weak { 2 } else { 3 }
                }
                SymbolDef::Absolute(_) => {
                    if weak {
                        2
                    } else {
                        3
                    }
                }
                SymbolDef::Common { size, align } => {
                    let it = b.commons.entry(slot).or_insert((0, 1));
                    it.0 = it.0.max(size);
                    it.1 = it.1.max(align);
                    1
                }
            };
            let it = b.graph.slots[slot as usize];
            if it.flags & RUST != 0 {
                if strength == 3 {
                    return Err(error(
                        None,
                        format!(
                            "duplicate symbol `{}`: Rust code defines it, and so does {}",
                            object.name(symbol.name),
                            name_of(self, input)
                        ),
                    ));
                }
                continue;
            }
            let take = match b.graph.best(slot) {
                None => true,
                Some(_) if strength > it.strength => true,
                Some((l, i)) if strength == 3 && it.strength == 3 => {
                    let first = match b.graph.object(l).symbols[i as usize].def {
                        SymbolDef::Section { .. } => name_of(self, b.graph.loads[l as usize].input).to_string(),
                        _ => "an absolute definition".to_owned(),
                    };
                    return Err(error(
                        None,
                        format!("duplicate symbol `{}`: in {first} and in {}", object.name(symbol.name), name_of(self, input)),
                    ));
                }
                Some(_) => false,
            };
            if take {
                let it = &mut b.graph.slots[slot as usize];
                it.best = pack(load, at as u32);
                it.strength = strength;
            }
        }
        Ok(())
    }

    /// What is left of a name nothing defines: the linker's, a shared library's, 0, or an
    /// error naming it and who refers to it.
    fn decide(&self, graph: &Graph, sym: Sym, weak: bool) -> Result<Decision, LinkError> {
        let Some(name) = self.symbols.name_of(sym) else { return Ok(Decision::Skip) };
        if let Some(special) = self.writer.special(name) {
            return Ok(Decision::Special(special));
        }
        if let Some(export) = graph.exports.find(name) {
            let import = Import {
                name: name.to_owned(),
                library: None,
                version: graph.exports.version(export),
                kind: export.kind,
                weak: weak || export.weak,
                wasm: None,
            };
            return Ok(Decision::Import(import, export.library));
        }
        let executable = matches!(self.output.kind, OutputKind::Executable { .. });
        if self.output.format == crate::Format::Wasm {
            // rustc links wasm with `--allow-undefined`: a function something says the
            // import of is imported; a weak one is null.
            let native = graph.wasm.get(&sym);
            let rust = match self.symbols.key(sym) {
                SymKey::Name(id) => self.new.wasm_imports.get(&id),
                _ => None,
            };
            if let (Some((by, a)), Some(b)) = (native, rust) {
                if a != b {
                    return Err(error(
                        None,
                        format!("`{name}` is imported as {a:?} by {} and as {b:?} by Rust code", self.new.inputs[*by as usize].name),
                    ));
                }
            }
            return match native.map(|it| &it.1).or(rust) {
                Some(it) if !weak => Ok(Decision::Import(
                    Import {
                        name: name.to_owned(),
                        library: None,
                        version: None,
                        kind: SymbolKind::Function,
                        weak: false,
                        wasm: Some(it.clone()),
                    },
                    NONE,
                )),
                _ if weak => Ok(Decision::Zero),
                _ => Err(error(
                    None,
                    format!("undefined symbol `{name}`{}, which no input says the wasm import of", self.referrer(sym)),
                )),
            };
        }
        if weak && executable {
            return Ok(Decision::Zero);
        }
        if !executable {
            let import = Import { name: name.to_owned(), library: None, version: None, kind: SymbolKind::Other, weak, wasm: None };
            return Ok(Decision::Import(import, NONE));
        }
        if optional(name) {
            return Ok(Decision::Skip);
        }
        Err(error(None, format!("undefined symbol `{name}`{}", self.referrer(sym))))
    }

    /// Who refers to an undefined symbol, for its error.
    fn referrer(&self, sym: Sym) -> String {
        let Some(piece) = self.new.refs(sym).find_map(|it| self.new.piece(it)) else {
            return String::new();
        };
        match piece.what() {
            What::Native { obj, .. } | What::NativeFrame { obj, .. } => {
                let id = self.symbols.content(obj.native);
                let name = self.new.inputs.iter().find(|it| it.id == *id).map_or("?", |it| &*it.name);
                format!(", referred to by {name}")
            }
            _ => format!(", referred to by {}", self.symbols.describe(piece.sym)),
        }
    }

    /// Makes a kept section's piece, its bytes borrowed from its input.
    fn make_section(&mut self, graph: &Graph, load: u32, section: u32, scratch: &mut Scratch) -> Result<(), LinkError> {
        let it = &graph.loads[load as usize];
        let parsed = &it.parsed;
        let object = parsed.object();
        let s = &object.sections[section as usize];
        let (class, rank, unwind) = match &s.what {
            SectionKind::Alloc(class, rank) => (class, *rank, false),
            SectionKind::Unwind(class) => (class, 0, true),
            SectionKind::Dropped | SectionKind::EhFrame => return Ok(()),
        };
        let zeroed = crate::state::zeroed(class);
        if zeroed && !matches!(s.data, Data::Zero(_)) {
            let name = &self.new.inputs[it.input as usize].name;
            return Err(error(None, format!("{name}: {} has bytes, in a zeroed section", object.name(s.name))));
        }
        scratch.fixups.clear();
        scratch.weak.clear();
        for f in object.section_fixups(s) {
            let target = parsed.sym(f.symbol);
            let symbol = &object.symbols[f.symbol as usize];
            if symbol.binding == Binding::Weak && symbol.def == SymbolDef::Undefined {
                scratch.weak.push(target);
            }
            scratch.fixups.push(Fixup { at: f.at, apply: f.apply, target, addend: f.addend, minus: f.minus.map(|m| parsed.sym(m)) });
        }
        scratch.weak.sort_unstable();
        scratch.weak.dedup();
        section_defines(graph, load, section, &mut scratch.defines);
        let content =
            if zeroed { Content::Zero } else { self.content(&s.data, it.input, parsed.base, 0..s.data.len() as usize) };
        let order = self.new.inputs[it.input as usize].order;
        let what = What::Native { obj: parsed.obj, section, unwind };
        let mut piece = self.piece(parsed.section_sym(section), class, s.align, s.data.len(), &scratch.fixups, what);
        piece.rank = rank;
        piece.order = [order, parsed.obj.member, section];
        piece.zero = zeroed;
        piece.weak = scratch.weak.as_slice();
        piece.defines = scratch.defines.as_slice();
        self.make(&piece, content);
        Ok(())
    }

    /// Makes a kept frame record's piece: an FDE's CIE pointer is a fixup to its CIE's piece.
    fn make_frame(&mut self, graph: &Graph, load: u32, frame: u32, fixups: &mut Vec<Fixup>) -> Result<(), LinkError> {
        let it = &graph.loads[load as usize];
        let parsed = &it.parsed;
        let object = parsed.object();
        let record = parsed.frames[frame as usize];
        let s = &object.sections[record.section as usize];
        let (start, end) = (u64::from(record.start), u64::from(record.end));
        let name = || self.new.inputs[it.input as usize].name.clone();
        if (end - start) % 4 != 0 {
            return Err(error(None, format!("{}: {}: a frame record not a multiple of 4 long", name(), object.name(s.name))));
        }
        if record.fde && record.cie == NONE {
            return Err(error(None, format!("{}: {}: an FDE at {start:#x} whose CIE is no record", name(), object.name(s.name))));
        }
        let sym = parsed.frame_sym(frame);
        fixups.clear();
        for at in parsed.frame_fixups.get(frame as usize) {
            let f = &object.fixups[*at as usize];
            fixups.push(Fixup {
                at: f.at - start,
                apply: f.apply,
                target: parsed.sym(f.symbol),
                addend: f.addend,
                minus: f.minus.map(|m| parsed.sym(m)),
            });
        }
        if record.fde {
            let cie = parsed.frame_sym(record.cie);
            fixups.retain(|f| f.at != 4);
            fixups.push(Fixup { at: 4, apply: Apply::Delta32, target: sym, addend: 4, minus: Some(cie) });
        }
        fixups.sort_by_key(|f| f.at);
        let content = self.content(&s.data, it.input, parsed.base, start as usize..end as usize);
        let order = self.new.inputs[it.input as usize].order;
        let what = What::NativeFrame { obj: parsed.obj, section: record.section, fde: record.fde };
        let mut piece = self.piece(sym, &Class::EhFrame, 4, end - start, fixups.as_slice(), what);
        piece.rank = u32::from(record.fde);
        piece.order = [order, parsed.obj.member, record.section];
        self.make(&piece, content);
        Ok(())
    }

    fn make_common(&mut self, name: Sym, size: u64, align: u64) {
        let SymKey::Name(id) = self.symbols.key(name) else { return };
        let common = self.sym(SymKey::Common(id));
        let mut piece = self.piece(common, &Class::Bss, align, size, &[], What::Common);
        piece.zero = true;
        self.make(&piece, Content::Zero);
    }

    fn unmake_common(&mut self, name: Sym) {
        let SymKey::Name(id) = self.symbols.key(name) else { return };
        if let Some(piece) = self.symbols.find(SymKey::Common(id)).and_then(|it| self.piece_of(it)) {
            self.unmake(piece);
        }
    }

    /// Defines the symbols a newly kept section holds: its locals, the globals it wins.
    fn define_in(&mut self, graph: &Graph, load: u32, section: u32) {
        let parsed = &graph.loads[load as usize].parsed;
        let Some(piece) = self.piece_of(parsed.section_sym(section)) else { return };
        let object = parsed.object();
        for symbol in parsed.defs.get(section as usize) {
            let s = &object.symbols[*symbol as usize];
            let sym = parsed.sym(*symbol);
            if sym == parsed.section_sym(section) || !(is_local(s) || graph.wins(load, *symbol)) {
                continue;
            }
            if let SymbolDef::Section { value, .. } = s.def {
                self.define(sym, Def::Piece { piece, offset: value });
            }
        }
    }

    /// Counts Rust's references to names from the pieces made and removed since `old`: the
    /// counts, and the names whose class (none, weak, strong) changed, from and to.
    fn references(&self, old: &Resolved) -> (Refs, Vec<(Sym, u8, u8)>) {
        let mut refs = old.refs.clone();
        let mut touched: Vec<Sym> = Vec::new();
        for id in self.new.removed_since(old.removals) {
            let Some(list) = refs.of.get(id as usize).copied().filter(|it| !it.is_empty()) else { continue };
            let entries: SmallVec<[u64; 16]> = refs.lists.get(list).iter().copied().collect();
            for entry in entries {
                refs.count(entry, -1);
                touched.push(Sym(entry as u32));
            }
            refs.lists.free(list);
            refs.of.set(id as usize, ListRef::EMPTY, || ListRef::EMPTY);
        }
        let mut entries: SmallVec<[u64; 16]> = SmallVec::new();
        for id in old.pieces..self.new.piece_count() {
            let Some(piece) = self.new.piece(id) else { continue };
            if !counted(piece.what()) {
                continue;
            }
            entries.clear();
            for fixup in piece.fixups() {
                if matches!(self.symbols.key(fixup.target), SymKey::Name(_)) {
                    let weak = piece.weak().contains(&fixup.target);
                    entries.push(u64::from(fixup.target.0) | u64::from(weak) << 32);
                }
            }
            if entries.is_empty() {
                continue;
            }
            entries.sort_unstable();
            entries.dedup();
            for entry in &entries {
                refs.count(*entry, 1);
                touched.push(Sym(*entry as u32));
            }
            let list = refs.lists.push(&entries);
            refs.of.set(id as usize, list, || ListRef::EMPTY);
        }
        if refs.lists.garbage() > refs.lists.live() + (1 << 16) {
            refs.compact();
        }
        radix::sort_copy_by_key(&mut touched, |it| it.0);
        touched.dedup();
        let changes = touched
            .into_iter()
            .filter_map(|sym| {
                let (before, after) = (old.refs.class(sym), refs.class(sym));
                (before != after).then_some((sym, before, after))
            })
            .collect();
        (refs, changes)
    }

    /// Resolves the natives and the names, reusing what the image's resolution can.
    pub fn resolve(&mut self) -> Result<(), LinkError> {
        let old = self.new.resolved.clone();
        let (refs, changes) = self.references(&old);
        let same = old.graph.is_some()
            && Arc::ptr_eq(&old.inputs, &self.new.inputs)
            && (Arc::ptr_eq(&old.names, &self.new.names) || *old.names == *self.new.names)
            && (Arc::ptr_eq(&old.wasm, &self.new.wasm_imports) || *old.wasm == *self.new.wasm_imports)
            && !(self.full && old.stale);
        let mut gained = Vec::new();
        let mut rebuild = !same;
        if same {
            for (sym, before, after) in &changes {
                match settled(old.outcome(*sym), *before, *after, self.full) {
                    Next::Nothing => {}
                    Next::Gained => gained.push(*sym),
                    Next::Rebuild => {
                        rebuild = true;
                        break;
                    }
                }
            }
        }
        let grown = match (rebuild, gained.is_empty()) {
            (true, _) => None,
            (false, true) => Some((*old).clone()),
            (false, false) => self.grow(&old, &refs, &gained)?,
        };
        let mut next = match grown {
            Some(it) => it,
            None => self.rebuild(&old, &refs)?,
        };
        next.refs = refs;
        next.pieces = self.new.piece_count();
        next.removals = self.new.removals();
        self.new.resolved = Arc::new(next);
        Ok(())
    }

    /// Resolves names Rust newly refers to against the kept graph, and makes what they newly
    /// reach; `None` when that needs what only building the graph again gives.
    fn grow(&mut self, old: &Resolved, refs: &Refs, gained: &[Sym]) -> Result<Option<Resolved>, LinkError> {
        let Some(graph) = old.graph.clone() else { return Ok(None) };
        let mut marks = old.marks.clone();
        let mut outcomes: Vec<(Sym, u8)> = Vec::new();
        let mut undefined: Vec<Sym> = Vec::new();
        let (sections, frames) = {
            let symbols: &Symbols = &*self.symbols;
            let mut marker = Marker::new(&graph, &mut marks, symbols, self.writer);
            for &sym in gained {
                // A gained name is new to the names: nothing but Rust refers to it.
                let weak = if refs.class(sym) == 2 { 0 } else { O_WEAK };
                let rust = match symbols.key(sym) {
                    SymKey::Name(id) => self.new.names.contains_key(&id),
                    _ => false,
                };
                if rust {
                    outcomes.push((sym, O_RUST | weak));
                    continue;
                }
                let slot = graph.slot_of.get(&sym).copied();
                if slot.is_some_and(|it| graph.slots[it as usize].flags & ALIAS != 0) {
                    return Ok(None);
                }
                if let Some(slot) = slot.filter(|it| graph.best(*it).is_some()) {
                    marker.name(slot);
                    outcomes.push((sym, graph.kind(slot) | weak));
                    continue;
                }
                if special_mark(&graph, symbols, self.writer, sym) != NOTHING {
                    return Ok(None);
                }
                if refs.class(sym) == 2 {
                    let name = symbols.name_of(sym).unwrap_or("");
                    let pulls = graph.archives.iter().any(|(input, table)| match &*self.new.inputs[*input as usize].file {
                        NativeFile::Archive(archive) => archive_find(table, archive, name).is_some(),
                        _ => false,
                    });
                    if pulls {
                        return Ok(None);
                    }
                }
                undefined.push(sym);
            }
            marker.run();
            (marker.sections, marker.frames)
        };
        let mut rose = Vec::new();
        note_native(&graph, &mut marks, &sections, &frames, &mut rose);
        for (slot, _, level) in rose {
            let it = &graph.slots[slot as usize];
            let outcome = old.outcome(it.sym);
            let strong = if level == 2 { O_STRONG } else { 0 };
            if outcome & O_KIND == 0 {
                if graph.defined(slot) {
                    let weak = if level == 2 || refs.class(it.sym) == 2 { 0 } else { O_WEAK };
                    outcomes.push((it.sym, graph.kind(slot) | weak | strong | O_OTHERS));
                } else {
                    undefined.push(it.sym);
                }
                continue;
            }
            if !graph.defined(slot) && level == 2 && outcome & O_WEAK != 0 {
                return Ok(None);
            }
            outcomes.push((it.sym, outcome | strong | O_OTHERS));
        }

        radix::sort_copy_by_key(&mut undefined, |it| it.0);
        undefined.dedup();
        let mut decisions = Vec::with_capacity(undefined.len());
        for sym in undefined {
            let slot = graph.slot_of.get(&sym).copied();
            let flags = slot.map_or(0, |it| graph.slots[it as usize].flags);
            let native = slot.map_or(0, |it| marks.native_of(it));
            let strong = flags & (REQUIRED | STRONG) != 0 || native == 2;
            let others = strong || flags & ROOT != 0 || native > 0;
            let weak = !strong && refs.class(sym) != 2;
            let bits = if weak { O_WEAK } else { 0 } | if strong { O_STRONG } else { 0 } | if others { O_OTHERS } else { 0 };
            let decision = self.decide(&graph, sym, weak)?;
            let kind = match &decision {
                Decision::Special(Special::Section { .. } | Special::Got(_)) => return Ok(None),
                Decision::Special(_) | Decision::Zero | Decision::Skip => O_OTHER,
                Decision::Import(_, library) if *library == NONE => O_IMPORT,
                Decision::Import(_, library) if old.library_index[*library as usize] == NONE => return Ok(None),
                Decision::Import(_, library) if graph.exports.libraries[*library as usize].as_needed => O_NEEDED,
                Decision::Import(..) => O_IMPORT,
            };
            decisions.push((sym, decision, kind | bits));
        }

        // Nothing below gives up: make and define what was reached and decided.
        self.pass();
        let mut scratch = Scratch::default();
        let mut sections = sections;
        radix::sort(&mut sections);
        for (load, section) in sections.iter().map(|it| unpack(*it)) {
            let parsed = &graph.loads[load as usize].parsed;
            if !makes_piece(&parsed.object().sections[section as usize].what) || self.piece_of(parsed.section_sym(section)).is_some() {
                continue;
            }
            self.make_section(&graph, load, section, &mut scratch)?;
            self.define_in(&graph, load, section);
        }
        let mut frames = frames;
        frames.sort_unstable();
        for (load, frame) in frames.iter().map(|it| unpack(*it)) {
            if self.piece_of(graph.loads[load as usize].parsed.frame_sym(frame)).is_none() {
                self.make_frame(&graph, load, frame, &mut scratch.fixups)?;
            }
        }
        let mut next = old.clone();
        let mut imports: Vec<(Sym, Import)> = Vec::new();
        for (sym, decision, outcome) in decisions {
            match decision {
                Decision::Special(special) => {
                    let at = self.new.specials.len() as u32;
                    self.new.specials.push(special);
                    self.define(sym, Def::Special(at));
                    next.defined.push(sym);
                }
                Decision::Import(mut import, library) => {
                    import.library = (library != NONE).then(|| old.library_index[library as usize]);
                    imports.push((sym, import));
                }
                Decision::Zero => {
                    self.define(sym, Def::Absolute(0));
                    next.defined.push(sym);
                }
                Decision::Skip => {}
            }
            outcomes.push((sym, outcome));
        }
        if !imports.is_empty() {
            imports.sort_by(|a, b| a.1.name.cmp(&b.1.name));
            let list = Arc::make_mut(&mut next.imports);
            let syms = Arc::make_mut(&mut next.import_syms);
            for (sym, import) in imports {
                // An import kept in place for an earlier link is taken up again.
                let at = match syms.iter().position(|it| *it == sym) {
                    Some(at) => {
                        list[at] = import;
                        at
                    }
                    None => {
                        syms.push(sym);
                        list.push(import);
                        syms.len() - 1
                    }
                };
                self.define(sym, Def::Import(at as u32));
                next.defined.push(sym);
            }
        }
        for (sym, outcome) in outcomes {
            if next.outcome(sym) & O_KIND == 0 {
                next.named.push(sym);
            }
            next.outcomes.set(Symbols::dense(sym), outcome, || 0);
        }
        next.marks = marks;
        Ok(Some(next))
    }

    /// Builds the graph again from the kept objects and makes the image's natives and names
    /// what it gives, touching only what differs.
    fn rebuild(&mut self, old: &Resolved, refs: &Refs) -> Result<Resolved, LinkError> {
        self.pass();
        let output = self.output;
        let inputs = self.new.inputs.clone();
        let names = self.new.names.clone();
        let mut b = Build::new();
        let rust: Vec<(Sym, Sym)> = names.iter().map(|(name, target)| (self.symbols.intern(SymKey::Name(*name)), *target)).collect();
        for (sym, _) in &rust {
            let slot = b.slot(*sym);
            b.flag(slot, RUST);
        }
        for sym in refs.names.iter() {
            match refs.class(*sym) {
                0 => {}
                1 => {
                    let slot = b.slot(*sym);
                    b.flag(slot, WANTED);
                }
                _ => {
                    let slot = b.slot(*sym);
                    b.flag(slot, WANTED | PULL);
                }
            }
        }
        let entry = match &output.kind {
            OutputKind::Executable { entry } => Some(self.symbols.sym_name(entry)),
            OutputKind::Shared { .. } => self.writer.shared_entry().map(|it| self.symbols.sym_name(it)),
        };
        if let Some(sym) = entry {
            let slot = b.slot(sym);
            b.flag(slot, ROOT | STRONG | PULL);
        }
        b.graph.entry = entry;
        if let OutputKind::Shared { exports, .. } = &output.kind {
            for name in exports {
                let slot = b.slot(self.symbols.sym_name(name));
                b.flag(slot, ROOT | STRONG | PULL);
            }
        }
        for name in ["_init", "_fini"] {
            let slot = b.slot(self.symbols.sym_name(name));
            b.flag(slot, LOOSE);
        }

        // Load: the line's objects and whole archives, then members for what is strongly
        // undefined, round by round.
        let mut round: Vec<(u32, u32)> = Vec::new();
        let mut lazy: Vec<u32> = Vec::new();
        for (at, input) in inputs.iter().enumerate() {
            let at = at as u32;
            match &*input.file {
                NativeFile::Object(_) => round.push((at, 0)),
                NativeFile::Archive(archive) if matches!(input.kind, NativeKind::Archive { whole: true }) => {
                    round.extend((1..=archive.members.len() as u32).map(|member| (at, member)));
                }
                NativeFile::Archive(_) => lazy.push(at),
                NativeFile::Shared(_) => {
                    b.shared.push((input.file.clone(), matches!(input.kind, NativeKind::Shared { as_needed: true })));
                }
            }
        }
        let archives = self.indexes(&inputs, &lazy);
        let mut pulled = false;
        loop {
            let reads = self.read(&round)?;
            for (&(input, _), read) in round.iter().zip(reads) {
                match read {
                    Read::Object(parsed) => self.register(&mut b, parsed, input, pulled)?,
                    Read::Shared(file) => b.members.push(file),
                    Read::Index(_) => {}
                }
            }
            let mut next: Vec<(u32, u32)> = Vec::new();
            for slot in std::mem::take(&mut b.queue) {
                if b.graph.slots[slot as usize].flags & RUST != 0 || b.graph.best(slot).is_some() {
                    continue;
                }
                let Some(name) = self.symbols.name_of(b.graph.slots[slot as usize].sym) else { continue };
                for (input, table) in &archives {
                    let NativeFile::Archive(archive) = &*inputs[*input as usize].file else { continue };
                    if let Some(member) = archive_find(table, archive, name) {
                        if b.pulled.insert((*input, member)) {
                            next.push((*input, member + 1));
                        }
                        break;
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            next.sort_unstable();
            round = next;
            pulled = true;
        }
        b.aliases(&*self.symbols)?;
        let mut commons: Vec<(Sym, u64, u64)> = Vec::new();
        for (slot, (size, align)) in &b.commons {
            let Some((load, symbol)) = b.graph.best(*slot) else { continue };
            if matches!(b.graph.object(load).symbols[symbol as usize].def, SymbolDef::Common { .. }) {
                commons.push((b.graph.slots[*slot as usize].sym, *size, *align));
            }
        }
        commons.sort_unstable_by_key(|it| it.0);
        let mut shared = std::mem::take(&mut b.shared);
        shared.extend(b.members.drain(..).map(|it| (it, false)));
        b.graph.exports = match old.graph.as_ref().map(|it| &it.exports) {
            Some(it) if it.same(&shared) => it.clone(),
            _ => Arc::new(Exports::build(shared).map_err(|what| error(None, what))?),
        };
        b.graph.archives = archives;
        b.graph.index();
        let graph = Arc::new(b.graph);

        // Collect.
        let mut marks = Marks::default();
        let (sections, frames) = {
            let mut marker = Marker::new(&graph, &mut marks, &*self.symbols, self.writer);
            for slot in 0..graph.slots.len() as u32 {
                if graph.slots[slot as usize].flags & (WANTED | ROOT | LOOSE) != 0 {
                    marker.name(slot);
                }
            }
            for flat in &graph.roots {
                let at = graph.unflat(*flat);
                marker.push(at);
            }
            marker.run();
            (marker.sections, marker.frames)
        };
        note_native(&graph, &mut marks, &sections, &frames, &mut Vec::new());

        // What went: sections, frame records and commons the image has and the graph does not
        // keep. Loads whose sections came or went, or whose globals' winners changed, have
        // their symbols defined again; the rest are as they were.
        let mut affected = Bits::default();
        let mut gone: Vec<Sym> = Vec::new();
        if let Some(was) = &old.graph {
            for (l, it) in was.loads.iter().enumerate() {
                let now = graph.by_obj.get(&it.parsed.obj).copied();
                for s in 0..it.parsed.object().sections.len() as u32 {
                    if old.marks.sections.get(was.flat(l as u32, s)) && !now.is_some_and(|n| marks.sections.get(graph.flat(n, s))) {
                        gone.push(it.parsed.section_sym(s));
                        if let Some(n) = now {
                            affected.set(n as usize);
                        }
                    }
                }
                for f in 0..it.parsed.frames.len() as u32 {
                    let kept = now.is_some_and(|n| marks.frames.get((graph.loads[n as usize].frames + f) as usize));
                    if old.marks.frames.get((it.frames + f) as usize) && !kept {
                        gone.push(it.parsed.frame_sym(f));
                    }
                }
            }
        }
        for sym in gone {
            if let Some(piece) = self.piece_of(sym) {
                self.unmake(piece);
            }
        }
        let mut fresh_commons = Vec::new();
        {
            let (was, now) = (&old.commons[..], &commons[..]);
            let (mut i, mut j) = (0, 0);
            loop {
                match (was.get(i), now.get(j)) {
                    (Some(x), Some(y)) if x.0 == y.0 => {
                        if x != y {
                            self.unmake_common(x.0);
                            fresh_commons.push(*y);
                        }
                        i += 1;
                        j += 1;
                    }
                    (Some(x), Some(y)) if x.0 < y.0 => {
                        self.unmake_common(x.0);
                        i += 1;
                    }
                    (Some(x), None) => {
                        self.unmake_common(x.0);
                        i += 1;
                    }
                    (_, Some(y)) => {
                        fresh_commons.push(*y);
                        j += 1;
                    }
                    (None, None) => break,
                }
            }
        }

        // What came.
        let mut scratch = Scratch::default();
        for (load, it) in graph.loads.iter().enumerate() {
            let load = load as u32;
            let object = it.parsed.object();
            for s in 0..object.sections.len() as u32 {
                if !marks.sections.get(graph.flat(load, s)) || !makes_piece(&object.sections[s as usize].what) {
                    continue;
                }
                if self.piece_of(it.parsed.section_sym(s)).is_none() {
                    self.make_section(&graph, load, s, &mut scratch)?;
                    affected.set(load as usize);
                }
            }
            for f in 0..it.parsed.frames.len() as u32 {
                if marks.frames.get((it.frames + f) as usize) && self.piece_of(it.parsed.frame_sym(f)).is_none() {
                    self.make_frame(&graph, load, f, &mut scratch.fixups)?;
                }
            }
        }
        for (name, size, align) in fresh_commons {
            self.make_common(name, size, align);
        }

        // Every symbol's definition.
        for (load, it) in graph.loads.iter().enumerate() {
            if affected.get(load) {
                continue;
            }
            let was = old.graph.as_ref().and_then(|was| Some((was, *was.by_obj.get(&it.parsed.obj)?)));
            let changed = match was {
                None => true,
                Some((was, w)) => (0..it.slots.len() as u32)
                    .any(|i| it.slots[i as usize] != NONE && graph.wins(load as u32, i) != was.wins(w, i)),
            };
            if changed {
                affected.set(load);
            }
        }
        for (load, it) in graph.loads.iter().enumerate() {
            if !affected.get(load) {
                continue;
            }
            let load = load as u32;
            let parsed = &it.parsed;
            let object = parsed.object();
            for (at, symbol) in object.symbols.iter().enumerate() {
                if !is_local(symbol) {
                    continue;
                }
                let sym = parsed.sym(at as u32);
                let def = match symbol.def {
                    SymbolDef::Section { section, value, .. } => {
                        if sym == parsed.section_sym(section) {
                            continue;
                        }
                        self.piece_of(parsed.section_sym(section)).map_or(Def::None, |piece| Def::Piece { piece, offset: value })
                    }
                    SymbolDef::Absolute(value) if symbol.kind == SymbolKind::Tls => Def::TlsOffset(value),
                    SymbolDef::Absolute(value) => Def::Absolute(value),
                    _ => Def::None,
                };
                self.define(sym, def);
            }
            for s in 0..object.sections.len() as u32 {
                let Some(piece) = self.piece_of(parsed.section_sym(s)) else { continue };
                section_defines(&graph, load, s, &mut scratch.defines);
                if self.new.piece(piece).is_some_and(|it| it.defines() != scratch.defines.as_slice()) {
                    self.new.set_defines(piece, &scratch.defines);
                }
            }
        }
        let mut defined: Vec<Sym> = Vec::with_capacity(graph.slots.len());
        for slot in 0..graph.slots.len() as u32 {
            let it = graph.slots[slot as usize];
            if it.flags & RUST != 0 {
                continue;
            }
            let def = if let Some((load, symbol)) = graph.best(slot) {
                let parsed = &graph.loads[load as usize].parsed;
                let s = &parsed.object().symbols[symbol as usize];
                match s.def {
                    SymbolDef::Section { section, value, .. } => {
                        self.piece_of(parsed.section_sym(section)).map_or(Def::None, |piece| Def::Piece { piece, offset: value })
                    }
                    SymbolDef::Absolute(value) if s.kind == SymbolKind::Tls => Def::TlsOffset(value),
                    SymbolDef::Absolute(value) => Def::Absolute(value),
                    SymbolDef::Common { .. } => {
                        let SymKey::Name(name) = self.symbols.key(it.sym) else { continue };
                        Def::Alias(self.sym(SymKey::Common(name)))
                    }
                    SymbolDef::Undefined | SymbolDef::Fallback { .. } => continue,
                }
            } else if it.flags & ALIAS != 0 {
                let (load, symbol) = unpack(it.fallback);
                Def::Alias(graph.loads[load as usize].parsed.sym(symbol))
            } else {
                continue;
            };
            self.define(it.sym, def);
            defined.push(it.sym);
        }
        for (sym, target) in &rust {
            self.define(*sym, Def::Alias(*target));
            defined.push(*sym);
        }
        let live = output.live.is_some().then(|| self.symbols.sym_name(blaze_live::patch::DESCRIPTOR));
        if let Some(live) = live {
            let descriptor = self.sym(SymKey::Descriptor);
            self.define(live, Def::Alias(descriptor));
            defined.push(live);
        }

        // What is left: the linker's, a shared library's, weak, or an error. `__imp_X` is
        // X's GOT cell: X is resolved as a strong reference is.
        let kind_of = |sym: Sym| -> Option<u8> {
            if Some(sym) == live {
                return Some(O_RUST);
            }
            let slot = *graph.slot_of.get(&sym)?;
            graph.defined(slot).then(|| graph.kind(slot))
        };
        let mut wanted: Vec<Sym> = Vec::new();
        for (slot, it) in graph.slots.iter().enumerate() {
            if it.flags & (ROOT | REQUIRED) != 0 || marks.native_of(slot as u32) > 0 || refs.class(it.sym) > 0 {
                wanted.push(it.sym);
            }
        }
        let mut cells: FxHashSet<Sym> = FxHashSet::default();
        for sym in &wanted {
            if kind_of(*sym).is_some() {
                continue;
            }
            let special = self.symbols.name_of(*sym).and_then(|it| self.writer.special(it));
            if let Some(Special::Got(target)) = special {
                cells.insert(self.symbols.sym_name(&target));
            }
        }
        wanted.extend(cells.iter().copied());
        wanted.sort_unstable();
        wanted.dedup();
        let mut outcomes: Vec<(Sym, u8)> = Vec::with_capacity(wanted.len());
        let mut imports: Vec<(Sym, Import, u32)> = Vec::new();
        let mut specials: Vec<(Sym, Special)> = Vec::new();
        for sym in wanted {
            let slot = graph.slot_of.get(&sym).copied();
            let flags = slot.map_or(0, |it| graph.slots[it as usize].flags);
            let native = slot.map_or(0, |it| marks.native_of(it));
            let strong = flags & (REQUIRED | STRONG) != 0 || native == 2 || cells.contains(&sym);
            let others = strong || flags & ROOT != 0 || native > 0;
            let weak = !strong && refs.class(sym) != 2;
            let bits = if weak { O_WEAK } else { 0 } | if strong { O_STRONG } else { 0 } | if others { O_OTHERS } else { 0 };
            let kind = match kind_of(sym) {
                Some(kind) => kind,
                None => match self.decide(&graph, sym, weak)? {
                    Decision::Special(special) => {
                        let kind = if matches!(special, Special::Section { .. } | Special::Got(_)) { O_MARKS } else { O_OTHER };
                        specials.push((sym, special));
                        kind
                    }
                    Decision::Import(import, library) => {
                        let needed = library != NONE && graph.exports.libraries[library as usize].as_needed;
                        imports.push((sym, import, library));
                        if needed { O_NEEDED } else { O_IMPORT }
                    }
                    Decision::Zero => {
                        self.define(sym, Def::Absolute(0));
                        defined.push(sym);
                        O_OTHER
                    }
                    Decision::Skip => O_OTHER,
                },
            };
            outcomes.push((sym, kind | bits));
        }
        self.new.specials.clear();
        for (sym, special) in specials {
            let at = self.new.specials.len() as u32;
            self.new.specials.push(special);
            self.define(sym, Def::Special(at));
            defined.push(sym);
        }

        // The libraries the image needs, and its imports: kept in their order across
        // incremental links so that no bind moves.
        let exports = &graph.exports;
        let mut used = vec![false; exports.libraries.len()];
        for (_, _, library) in &imports {
            if *library != NONE {
                used[*library as usize] = true;
            }
        }
        let mut index = vec![NONE; exports.libraries.len()];
        let mut needed: Vec<Library> = Vec::new();
        for (at, library) in exports.libraries.iter().enumerate() {
            if exports.canonical[at] == at as u32 && (!library.as_needed || used[at]) {
                index[at] = needed.len() as u32;
                needed.push(library.clone());
            }
        }
        let mut current: FxHashMap<Sym, Import> = FxHashMap::with_capacity_and_hasher(imports.len(), Default::default());
        for (sym, mut import, library) in imports {
            import.library = (library != NONE).then(|| index[library as usize]).filter(|it| *it != NONE);
            current.insert(sym, import);
        }
        // An import an incremental link no longer needs keeps its place (nothing binds to it),
        // its library looked up again by name.
        let mut order: Vec<(Sym, Import, bool)> = Vec::with_capacity(current.len() + old.imports.len());
        if !self.full {
            for (sym, import) in old.import_syms.iter().zip(old.imports.iter()) {
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
        let stale = order.iter().any(|it| !it.2);
        for (at, (sym, _, active)) in order.iter().enumerate() {
            if *active {
                self.define(*sym, Def::Import(at as u32));
                defined.push(*sym);
            }
        }
        let (import_syms, imports): (Vec<Sym>, Vec<Import>) = order.into_iter().map(|(sym, import, _)| (sym, import)).unzip();

        // A name the image's resolution defined and this one does not is undefined now.
        let mut now = Bits::default();
        for sym in &defined {
            now.set(Symbols::dense(*sym));
        }
        for sym in old.defined.iter() {
            if !now.get(Symbols::dense(*sym)) {
                self.define(*sym, Def::None);
            }
        }
        if let Some(entry) = graph.entry {
            if self.new.def(entry) == Def::None {
                return Err(error(None, format!("the entry point {} is not defined", self.symbols.describe(entry))));
            }
        }

        let mut table = old.outcomes.clone();
        for sym in old.named.iter() {
            table.set(Symbols::dense(*sym), 0, || 0);
        }
        let mut named = CowVec::default();
        for (sym, outcome) in outcomes {
            table.set(Symbols::dense(sym), outcome, || 0);
            named.push(sym);
        }
        let mut list = CowVec::default();
        for sym in defined {
            list.push(sym);
        }
        // The same lists keep their identity: the writer rebinds only when they change.
        Ok(Resolved {
            imports: if *old.imports == imports { old.imports.clone() } else { Arc::new(imports) },
            import_syms: if *old.import_syms == import_syms { old.import_syms.clone() } else { Arc::new(import_syms) },
            libraries: if *old.libraries == needed { old.libraries.clone() } else { Arc::new(needed) },
            exports: graph.asked.clone(),
            stale,
            refs: Refs::default(),
            pieces: 0,
            removals: 0,
            inputs: self.new.inputs.clone(),
            names: self.new.names.clone(),
            wasm: self.new.wasm_imports.clone(),
            library_index: Arc::new(index),
            commons: Arc::new(commons),
            marks,
            outcomes: table,
            named,
            defined: list,
            graph: Some(graph),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Group, Section};
    use crate::symbols::IdNo;

    fn names(of: &[&str]) -> (Names, Vec<Name>) {
        let mut names = Names::default();
        let list = of.iter().map(|it| names.push(it).unwrap()).collect();
        (names, list)
    }

    fn symbol(name: Name, def: SymbolDef, binding: Binding, kind: SymbolKind) -> Symbol {
        Symbol { name, def, binding, kind, hidden: false }
    }

    fn section(name: Name, what: SectionKind, data: Data, fixups: Range<u32>) -> Section {
        Section { name, what, align: 1, data, fixups, retain: false }
    }

    #[test]
    fn a_table_keeps_the_first_value_of_a_name_and_finds_it_after_growing() {
        let keys: Vec<String> = (0..1000).map(|it| format!("name{it}")).collect();
        let mut table = Table::with_capacity(2);
        for (at, key) in keys.iter().enumerate() {
            let eq = |it: u32| keys[it as usize % keys.len()] == *key;
            assert_eq!(table.insert(hash(key), at as u32, eq), Ok(()));
        }
        for (at, key) in keys.iter().enumerate() {
            assert_eq!(table.insert(hash(key), (at + keys.len()) as u32, |it| keys[it as usize % keys.len()] == *key), Err(at as u32));
            assert_eq!(table.find(hash(key), |it| keys[it as usize % keys.len()] == *key), Some(at as u32));
        }
        assert_eq!(table.find(hash("absent"), |_| true), None);
        assert_eq!(Table::default().find(hash("x"), |_| true), None);
    }

    #[test]
    fn bits_set_once_across_words_and_copies() {
        let mut bits = Bits::default();
        assert!(bits.set(3));
        assert!(!bits.set(3));
        assert!(bits.set(64 * 2000 + 5));
        let copy = bits.clone();
        assert!(bits.set(7));
        assert!(bits.get(3) && bits.get(7) && bits.get(64 * 2000 + 5));
        assert!(!bits.get(4) && !bits.get(64 * 3000));
        assert!(!copy.get(7), "a copy does not see later writes");
    }

    #[test]
    fn rows_keep_their_items_in_order() {
        let csr = Csr::build(4, &[(2, 7), (0, 1), (2, 3), (3, 9)]);
        assert_eq!(csr.get(0), &[1]);
        assert_eq!(csr.get(1), &[] as &[u32]);
        assert_eq!(csr.get(2), &[7, 3]);
        assert_eq!(csr.get(3), &[9]);
    }

    #[test]
    fn sections_kept_by_name_are_lld_s() {
        for it in [".init", ".init_array", ".init_array.5", ".fini_array.00100", ".ctors", ".note.GNU-stack", ".jcr"] {
            assert!(kept_by_name(it), "{it}");
        }
        for it in [".initx", ".text.init", ".data", ".init_arrayx"] {
            assert!(!kept_by_name(it), "{it}");
        }
    }

    #[test]
    fn a_reader_s_ranges_and_indexes_are_checked() {
        let (names, list) = names(&[".text", "f"]);
        let good = Object {
            sections: vec![section(list[0], SectionKind::Alloc(Class::Text, 0), Data::Input(0..16), 0..1)],
            symbols: vec![symbol(list[1], SymbolDef::Section { section: 0, value: 0, size: 4 }, Binding::Global, SymbolKind::Function)],
            fixups: vec![crate::model::NativeFixup { at: 0, apply: Apply::Abs64, symbol: 0, addend: 0, minus: None }],
            names: names.clone(),
            ..Object::default()
        };
        assert_eq!(check_object(&good, 16), Ok(()));
        assert!(check_object(&good, 15).unwrap_err().contains("past the input's 15"));

        let mut bad = good.clone();
        bad.fixups[0].symbol = 1;
        assert!(check_object(&bad, 16).unwrap_err().contains("past the object's 1"));

        let mut bad = good.clone();
        bad.sections[0].fixups = 0..2;
        assert!(check_object(&bad, 16).unwrap_err().contains("relocations"));

        let mut bad = good.clone();
        bad.symbols[0].def = SymbolDef::Section { section: 1, value: 0, size: 0 };
        assert!(check_object(&bad, 16).unwrap_err().contains("past the object's 1"));

        let mut bad = good.clone();
        bad.groups = vec![Group { name: list[1], sections: 0..1, largest: false }];
        bad.grouped = vec![4];
        assert!(check_object(&bad, 16).unwrap_err().contains("COMDAT"));

        let mut foreign = Names::default();
        foreign.push("a much longer name than any of these").unwrap();
        let mut bad = good;
        bad.symbols[0].name = foreign.push("x").unwrap();
        assert!(check_object(&bad, 16).unwrap_err().contains("outside"));
    }

    #[test]
    fn an_object_is_indexed_once_with_its_frame_records() {
        let cie = unwind::cie(1, -8, 16, &[], false).unwrap();
        let fde = unwind::fde(4, &[], false, false).unwrap();
        let fde_at = cie.bytes.len();
        let mut frame = cie.bytes.clone();
        frame.extend_from_slice(&fde.bytes);
        let back = (fde_at + 4) as u32;
        frame[fde_at + 4..fde_at + 8].copy_from_slice(&back.to_le_bytes());

        let (names, list) = names(&[".text", ".eh_frame", "f", "g", ".init_array", "my_section"]);
        let fixup = |at: u64, symbol: u32| crate::model::NativeFixup { at, apply: Apply::Abs64, symbol, addend: 0, minus: None };
        let object = Object {
            sections: vec![
                section(list[0], SectionKind::Alloc(Class::Text, 0), Data::Bytes(vec![0x90; 8]), 0..1),
                section(list[1], SectionKind::EhFrame, Data::Bytes(frame.clone()), 1..2),
                section(list[4], SectionKind::Alloc(Class::InitArray, 0), Data::Zero(8), 2..2),
                section(
                    list[5],
                    SectionKind::Alloc(Class::Named { name: "my_section".into(), write: true, exec: false }, 0),
                    Data::Zero(8),
                    2..2,
                ),
            ],
            symbols: vec![
                symbol(list[2], SymbolDef::Section { section: 0, value: 0, size: 4 }, Binding::Global, SymbolKind::Function),
                symbol(list[3], SymbolDef::Undefined, Binding::Weak, SymbolKind::Function),
            ],
            fixups: vec![fixup(4, 1), fixup(fde_at as u64 + 8, 0)],
            names,
            ..Object::default()
        };
        let records = vec![(1, unwind::split(&frame).unwrap())];
        assert_eq!(records[0].1.len(), 2);
        let syms: Vec<Sym> = (0..2 + 4 + 2).map(Sym).collect();
        let obj = Obj { native: IdNo(0), member: 0 };
        let parsed = Parsed::new(obj, Arc::new(NativeFile::Object(object)), 0, syms, &[NameId(9)], &records);

        assert_eq!(parsed.refs.get(0), &[1], "the text refers to `g`");
        assert_eq!(parsed.refs.get(1), &[0], "the frame records refer to `f`");
        assert_eq!(parsed.defs.get(0), &[0]);
        assert_eq!(parsed.roots, vec![2], "the init array is kept whatever refers to it");
        assert_eq!(parsed.named, vec![(NameId(9), 3)]);
        assert_eq!(parsed.section_sym(1), Sym(3));
        assert_eq!(parsed.frames.len(), 2);
        let (cie, fde) = (parsed.frames[0], parsed.frames[1]);
        assert!(!cie.fde && cie.cie == NONE && cie.start == 0);
        assert!(fde.fde && fde.cie == 0 && fde.start == fde_at as u32 && fde.function == 0);
        assert_eq!(parsed.frame_fixups.get(0), &[] as &[u32]);
        assert_eq!(parsed.frame_fixups.get(1), &[1]);
        assert_eq!(parsed.frame_sym(1), Sym(7));
    }

    #[test]
    fn what_a_change_of_rust_s_references_asks() {
        let defined = O_FIXED | O_STRONG;
        assert_eq!(settled(0, 0, 2, false), Next::Gained);
        assert_eq!(settled(0, 0, 0, false), Next::Nothing);
        assert_eq!(settled(O_RUST, 2, 0, true), Next::Nothing);
        assert_eq!(settled(defined, 2, 0, false), Next::Rebuild, "a root went");
        assert_eq!(settled(defined, 1, 2, false), Next::Nothing);
        assert_eq!(settled(O_PULLED, 2, 1, false), Next::Rebuild, "the member may go");
        assert_eq!(settled(O_PULLED, 1, 2, false), Next::Nothing);
        assert_eq!(settled(O_IMPORT, 2, 0, false), Next::Nothing, "an incremental link keeps it in place");
        assert_eq!(settled(O_IMPORT, 2, 0, true), Next::Rebuild, "a full link drops it");
        assert_eq!(settled(O_IMPORT, 2, 1, false), Next::Rebuild, "it becomes weak");
        assert_eq!(settled(O_IMPORT | O_STRONG | O_OTHERS, 2, 1, false), Next::Nothing, "still strong");
        assert_eq!(settled(O_IMPORT | O_WEAK | O_OTHERS, 0, 2, false), Next::Rebuild, "it becomes strong");
        assert_eq!(settled(O_NEEDED, 2, 0, false), Next::Rebuild, "its library may go");
        assert_eq!(settled(O_OTHER | O_WEAK, 1, 0, true), Next::Nothing);
        assert_eq!(settled(O_MARKS, 2, 0, false), Next::Rebuild);
    }
}
