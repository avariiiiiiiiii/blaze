//! Where pieces go.
//!
//! A full link sorts each class's pieces by rank, then link-line order (native sections),
//! then symbol key, so the image is the same whatever the core count or the order changes
//! came in, and lays the classes out in the writer's order with a page boundary at each
//! change of protection. An incremental image keeps room in each region; an incremental
//! link puts new code in [`Class::Patch`] and new data in [`Class::PatchData`], never
//! reusing what an old piece had (a running program may still be in it, and a rollback
//! returns to it), and lays out again in place only the regions whose order the program
//! observes (named sections, init arrays).
//!
//! The sort is on a precomputed 32-byte key per piece (the import address table cell, rank
//! and link-line order, and an order-keeping prefix of the symbol's contents), radix sorted,
//! with only pieces whose keys tie compared by their symbols' contents. Classes are a dense
//! index and sort in parallel.

use blaze_ids::radix;
use rayon::prelude::*;
use rustc_hash::FxHashSet;

use crate::LinkError;
use crate::cow::{CowBytes, IdSet};
use crate::model::{Apply, Class, Counts, Region, Sym};
use crate::reloc::align_up;
use crate::stage::{Stage, error, function_of};
use crate::state::{ClassId, PieceId, PieceRef, Sort, Space, State, What, zeroed};
use crate::symbols::{SymKey, Symbols};

/// The patch regions' least room: what an edit session's new code and data may take.
const PATCH: u64 = 64 << 20;
/// The least room of a growing region of an incremental image.
const ROOM: u64 = 64 << 10;

/// Whether a class grows in place in an incremental link (else its new pieces go to a
/// patch region, or it never grows).
fn grows(class: &Class) -> bool {
    matches!(
        class,
        Class::EhFrame
            | Class::Got
            | Class::Plt
            | Class::Tdata
            | Class::Tbss
            | Class::ThreadVars
            | Class::Named { .. }
            | Class::InitArray
            | Class::FiniArray
            | Class::Synthetic(_)
    )
}

/// Whether the program observes the order of a class's pieces: it is laid out again whole.
fn ordered(class: &Class) -> bool {
    matches!(class, Class::Named { .. } | Class::InitArray | Class::FiniArray)
}

/// What [`Counts`] sums over pieces, kept across links so an incremental link counts only
/// what it changed.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Totals {
    pub absolute: u64,
    pub image: u64,
    pub got: u64,
    pub functions: u64,
    pub function_names: u64,
    pub fdes: u64,
    pub unwind: u64,
}

impl Totals {
    /// What one piece adds. The jump table's cells are counted from the slots, not here.
    pub fn of(symbols: &Symbols, state: &State, piece: PieceRef<'_>) -> Totals {
        let mut it = Totals {
            absolute: piece.fixups().iter().filter(|it| it.apply == Apply::Abs64).count() as u64,
            image: if piece.zero() { 0 } else { piece.size },
            ..Totals::default()
        };
        match piece.what() {
            What::Got { kind, .. } => {
                it.got += kind.cells();
                it.absolute += kind.cells();
            }
            What::Function(function) => {
                it.functions += 1;
                if let Some(name) = state.debug_name(state.function(function).id) {
                    it.function_names += symbols.str(name).len() as u64 + 1;
                }
            }
            What::Fde | What::NativeFrame { fde: true, .. } => it.fdes += 1,
            What::Native { unwind: true, .. } => it.unwind += piece.size,
            What::Native { .. } if matches!(piece.class(), Class::Text) => it.functions += 1,
            _ => {}
        }
        it
    }

    fn add(self, other: Totals) -> Totals {
        Totals {
            absolute: self.absolute + other.absolute,
            image: self.image + other.image,
            got: self.got + other.got,
            functions: self.functions + other.functions,
            function_names: self.function_names + other.function_names,
            fdes: self.fdes + other.fdes,
            unwind: self.unwind + other.unwind,
        }
    }

    fn sub(self, other: Totals) -> Totals {
        Totals {
            absolute: self.absolute - other.absolute,
            image: self.image - other.image,
            got: self.got - other.got,
            functions: self.functions - other.functions,
            function_names: self.function_names - other.function_names,
            fdes: self.fdes - other.fdes,
            unwind: self.unwind - other.unwind,
        }
    }

    /// Every live piece's, in parallel.
    pub fn all(symbols: &Symbols, state: &State) -> Totals {
        (0..state.piece_count())
            .into_par_iter()
            .with_min_len(4096)
            .filter_map(|id| state.piece(id))
            .map(|piece| Totals::of(symbols, state, piece))
            .reduce(Totals::default, Totals::add)
    }
}

/// A piece's place in a class's order: the key compared first, then the symbol's contents
/// when keys tie.
#[derive(Clone, Copy, Debug)]
struct Entry {
    key: [u64; 4],
    sym: Sym,
    id: PieceId,
}

/// No import address table cell: after those that are.
const NOT_IAT: u64 = u64::MAX;

impl Entry {
    fn of(symbols: &Symbols, piece: PieceRef<'_>, iat: u64) -> Entry {
        let Sort { rank, order } = piece.sort();
        Entry {
            key: [
                iat,
                u64::from(rank) << 32 | u64::from(order[0]),
                u64::from(order[1]) << 32 | u64::from(order[2]),
                prefix(symbols, piece.sym),
            ],
            sym: piece.sym,
            id: piece.id,
        }
    }
}

/// The first eight bytes of a symbol's place in [`Symbols::cmp`]: its variant, then the
/// start of what the variant compares first (an id's bytes, a name's bytes zero-padded).
/// Symbols with a smaller prefix are smaller; with equal ones, either.
fn prefix(symbols: &Symbols, sym: Sym) -> u64 {
    use SymKey::*;
    let key = symbols.key(sym);
    let tag: u64 = match key {
        Body(_) => 0,
        Slot(_) => 1,
        Data(_) => 2,
        Static(..) => 3,
        Tlv(..) => 4,
        Name(_) => 5,
        Local(..) => 6,
        Section(..) => 7,
        Position(..) => 8,
        FileName(_) => 9,
        Cie(_) => 10,
        Fde(_) => 11,
        WinInfo(_) => 12,
        Frame(..) => 13,
        Got(..) => 14,
        Plt(_) => 15,
        IatEnd(_) => 16,
        Common(_) => 17,
        Generation => 18,
        Cells => 19,
        Stubs => 20,
        Descriptor => 21,
        LiveDir => 22,
    };
    let seven = |bytes: &[u8]| {
        let mut out = [0u8; 8];
        let len = bytes.len().min(7);
        out[1..1 + len].copy_from_slice(&bytes[..len]);
        u64::from_be_bytes(out)
    };
    let rest = match key {
        Body(id) | Slot(id) | Data(id) | Cie(id) | Fde(id) | WinInfo(id) | Static(id, _) | Tlv(id, _)
        | Position(id, _) => seven(&symbols.content(id).0),
        Local(obj, _) | Section(obj, _) | Frame(obj, ..) => seven(&symbols.content(obj.native).0),
        Name(name) | FileName(name) | IatEnd(name) | Common(name) => seven(symbols.str(name).as_bytes()),
        Got(kind, _) => (kind as u64) << 48,
        Plt(_) | Generation | Cells | Stubs | Descriptor | LiveDir => 0,
    };
    tag << 56 | rest
}

/// Sorts entries by key, then ties by their symbols' contents: a total order, so the result
/// is the same whatever order they came in.
fn sort_entries(symbols: &Symbols, entries: &mut [Entry]) {
    by_key(entries);
    let mut start = 0;
    while start < entries.len() {
        let key = entries[start].key;
        let end = start + entries[start..].iter().take_while(|it| it.key == key).count();
        if end - start > 1 {
            entries[start..end].sort_unstable_by(|a, b| symbols.cmp(a.sym, b.sym));
        }
        start = end;
    }
}

/// Entries by key, stably: the 32-byte key's low half, then its high half (least significant
/// first, so the second pass keeps the first's order among equal high halves).
fn by_key(entries: &mut [Entry]) {
    radix::sort_copy_by_key(entries, |it| radix::pack_64(it.key[2], it.key[3]));
    radix::sort_copy_by_key(entries, |it| radix::pack_64(it.key[0], it.key[1]));
}

impl Stage<'_> {
    /// The region an incremental link puts a new piece of `class` in.
    fn patch_class(&self, class: &Class) -> Class {
        if self.plan.indexed_code {
            return class.clone();
        }
        match class {
            Class::Text => Class::Patch,
            Class::Rodata | Class::RelRo | Class::Data | Class::Bss => Class::PatchData,
            other => other.clone(),
        }
    }

    /// What the piece sums of the image are: from every piece in a full link, from the last
    /// link's and what this one wrote, made or removed in an incremental one.
    pub fn totals(&self) -> Totals {
        if self.full || !self.old.laid_out {
            return Totals::all(self.symbols, &self.new);
        }
        let mut seen = IdSet::default();
        let mut add = Totals::default();
        let mut sub = Totals::default();
        let ids = self.write.iter().chain(self.fresh.iter()).chain(self.removed.iter().copied());
        for id in ids {
            if !seen.insert(id) {
                continue;
            }
            if let Some(piece) = self.new.piece(id) {
                add = add.add(Totals::of(self.symbols, &self.new, piece));
            }
            if let Some(piece) = self.old.piece(id) {
                sub = sub.add(Totals::of(self.symbols, self.old, piece));
            }
        }
        self.old.lists.totals.add(add).sub(sub)
    }

    pub fn counts(&self) -> Counts {
        let resolved = &self.new.resolved;
        let totals = self.totals();
        let exports = self.exports();
        Counts {
            imports: resolved.imports.len() as u64,
            import_names: resolved
                .imports
                .iter()
                .map(|it| it.name.len() as u64 + it.version.as_ref().map_or(0, |v| v.len() as u64) + 2)
                .sum(),
            libraries: resolved.libraries.len() as u64,
            library_names: resolved.libraries.iter().map(|it| it.name.len() as u64 + 1).sum(),
            got: totals.got,
            absolute: totals.absolute + u64::from(self.new.slots),
            exports: exports.len() as u64,
            export_names: exports.iter().map(|it| it.0.len() as u64 + 1).sum(),
            functions: totals.functions,
            function_names: totals.function_names,
            image: totals.image,
            fdes: totals.fdes,
            unwind: totals.unwind,
        }
    }

    /// What the image exports: the name, the symbol it is, and whether it is data whatever
    /// it is defined in (`/EXPORT:name=symbol,DATA`).
    pub fn exports(&self) -> Vec<(&str, &str, bool)> {
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        let mut out: Vec<(&str, &str, bool)> = Vec::new();
        if let crate::OutputKind::Shared { exports, .. } = &self.output.kind {
            out.reserve(exports.len());
            for it in exports {
                seen.insert(it);
                out.push((it, it, false));
            }
        }
        for (name, symbol, data) in self.new.resolved.exports.iter() {
            if seen.insert(name) {
                out.push((name, symbol, *data));
            }
        }
        if self.output.live.is_some() {
            let name = blaze_live::patch::DESCRIPTOR;
            out.push((name, name, true));
        }
        out
    }

    pub fn place(&mut self) -> Result<(), LinkError> {
        if self.full { self.lay_out() } else { self.allocate() }
    }

    /// Whether a class is laid out again whole when it changes: as [`ordered`], and the GOT
    /// of an image whose GOT is its import address table, each DLL's cells together.
    fn ordered(&self, class: &Class) -> bool {
        ordered(class) || (self.plan.got_by_library && *class == Class::Got)
    }

    /// Where a GOT cell is in an image whose GOT is its import address table: the DLL, then
    /// the import (each DLL's zero cell after its imports' cells), as one key.
    fn iat_key(&self, piece: PieceRef<'_>) -> u64 {
        if let Some(library) = self.iat_end(piece.sym) {
            return u64::from(library) << 32 | u64::from(u32::MAX);
        }
        let What::Got { kind: crate::reloc::GotKind::Address, target } = piece.what() else { return NOT_IAT };
        let Some(at) = self.import_of(target) else { return NOT_IAT };
        let Some(import) = self.new.resolved.imports.get(at as usize) else { return NOT_IAT };
        u64::from(import.library.unwrap_or(u32::MAX)) << 32 | u64::from(at)
    }

    /// The pieces of `class` in the order they are laid out in: import address table cells
    /// first, by DLL and import; then by rank, link-line order and symbol.
    fn sort_pieces(&self, class: &Class, ids: &[PieceId]) -> Vec<PieceId> {
        let iat = self.plan.got_by_library && *class == Class::Got;
        let mut entries: Vec<Entry> = ids
            .iter()
            .map(|id| {
                let piece = self.new.piece(*id).expect("live");
                Entry::of(self.symbols, piece, if iat { self.iat_key(piece) } else { NOT_IAT })
            })
            .collect();
        sort_entries(self.symbols, &mut entries);
        entries.into_iter().map(|it| it.id).collect()
    }

    /// Places `ids` in this order from `start`: each piece's address, and the region's size.
    fn put(&mut self, start: u64, ids: &[PieceId], region: u32, zero: bool) -> u64 {
        let mut size = 0;
        for id in ids {
            let hot = self.new.hot_mut(*id);
            size = align_up(start + size, hot.align()) - start;
            hot.addr = start + size;
            hot.region = region;
            hot.set_zero(zero);
            size += hot.size;
        }
        size
    }

    /// Every piece placed anew.
    fn lay_out(&mut self) -> Result<(), LinkError> {
        let incremental = self.output.incremental;
        let synthetic = self.writer.synthetic(&self.counts());
        let mut extra: Vec<Class> = synthetic.iter().map(|it| it.0.clone()).collect();
        if incremental && !self.plan.indexed_code {
            extra.extend([Class::Patch, Class::PatchData, Class::Got, Class::Plt, Class::EhFrame, Class::Tdata, Class::Tbss]);
            if self.plan.tls_descriptors {
                extra.push(Class::ThreadVars);
            }
        }
        let extra: Vec<u16> = extra.iter().map(|it| self.new.class_id(it).0).collect();
        let mut groups: Vec<Vec<PieceId>> = Vec::new();
        let mut present: Vec<bool> = Vec::new();
        let mark = |groups: &mut Vec<Vec<PieceId>>, present: &mut Vec<bool>, class: u16| {
            let at = class as usize;
            if groups.len() <= at {
                groups.resize_with(at + 1, Vec::new);
                present.resize(at + 1, false);
            }
            present[at] = true;
        };
        for piece in self.new.live_pieces() {
            mark(&mut groups, &mut present, piece.class.0);
            groups[piece.class.0 as usize].push(piece.id);
        }
        for class in extra {
            mark(&mut groups, &mut present, class);
        }
        let iat = self.plan.got_by_library.then(|| self.new.find_class(&Class::Got)).flatten();
        let mut sorted: Vec<Vec<PieceId>> = {
            let symbols: &Symbols = self.symbols;
            let state = &self.new;
            groups
                .par_iter()
                .enumerate()
                .map(|(class, ids)| {
                    if iat.is_some_and(|it| it.0 as usize == class) {
                        return Vec::new();
                    }
                    let mut entries: Vec<Entry> =
                        ids.iter().map(|id| Entry::of(symbols, state.piece(*id).expect("live"), NOT_IAT)).collect();
                    sort_entries(symbols, &mut entries);
                    entries.into_iter().map(|it| it.id).collect()
                })
                .collect()
        };
        if let Some(iat) = iat {
            if let Some(ids) = groups.get(iat.0 as usize) {
                sorted[iat.0 as usize] = self.sort_pieces(&Class::Got, ids);
            }
        }
        drop(groups);
        let mut classes: Vec<Class> = (0..present.len())
            .filter(|it| present[*it])
            .map(|it| self.new.class(ClassId(it as u16)).clone())
            .collect();
        self.writer.order(&mut classes);
        let mut spaces = Vec::with_capacity(classes.len());
        let mut addr = self.plan.base + self.plan.headers;
        let mut code_addr = 0;
        let mut prot = None;
        let mut text_size = 0;
        let mut data_size = 0;
        for class in classes {
            let id = self.new.find_class(&class).expect("a class of the image");
            let ids = std::mem::take(&mut sorted[id.0 as usize]);
            let this = self.writer.prot(&class);
            let indexed = self.plan.indexed_code && this.exec;
            if !indexed && prot != Some(this) {
                addr = align_up(addr, self.plan.page);
                prot = Some(this);
            }
            let align = ids
                .iter()
                .map(|id| self.new.piece(*id).expect("live").align())
                .max()
                .unwrap_or(1)
                .max(if matches!(class, Class::Synthetic(_)) { 8 } else { 1 });
            let start = if indexed { code_addr } else { align_up(addr, align) };
            let zero = zeroed(&class);
            let size = self.put(start, &ids, spaces.len() as u32, zero);
            match &class {
                Class::Text => text_size += size,
                Class::Rodata | Class::RelRo | Class::Data | Class::Bss => data_size += size,
                _ => {}
            }
            let bound = synthetic.iter().find(|it| it.0 == class).map(|it| it.1);
            let capacity = match (&class, incremental) {
                (Class::Patch, true) => align_up(text_size.max(PATCH), self.plan.page),
                (Class::PatchData, true) => align_up(data_size.max(PATCH), self.plan.page),
                (Class::Synthetic(_), true) => {
                    let bound = bound.unwrap_or(size);
                    align_up((bound * 2).max(bound + ROOM), 16)
                }
                (Class::Synthetic(_), false) => bound.unwrap_or(size),
                (class, true) if grows(class) => align_up((size * 2).max(size + ROOM), 16),
                _ => size,
            };
            let synthetic = matches!(class, Class::Synthetic(_));
            spaces.push(Space {
                region: Region { class, addr: start, capacity, size: if synthetic { 0 } else { size }, prot: this, align },
                bytes: CowBytes::default(),
                zero,
                offset: None,
            });
            if indexed {
                code_addr = start + capacity;
            } else {
                addr = start + capacity;
            }
            self.write.extend(ids);
        }
        self.new.spaces = spaces;
        self.new.laid_out = true;
        self.tls();
        Ok(())
    }

    /// New pieces placed where an incremental image has room.
    fn allocate(&mut self) -> Result<(), LinkError> {
        for (class, bound) in self.writer.synthetic(&self.counts()) {
            let space = self
                .new
                .space_of(&class)
                .ok_or_else(|| error(None, format!("the image has no {class:?} region: link it from scratch")))?;
            let capacity = self.new.spaces[space].region.capacity;
            if bound > capacity {
                return Err(error(
                    None,
                    format!("{class:?} may need {bound} bytes and has room for {capacity}: link the image from scratch"),
                ));
            }
        }
        let mut relay: Vec<usize> = Vec::new();
        for id in &self.removed {
            if let Some(old) = self.old.piece(*id) {
                if self.ordered(old.class()) {
                    relay.push(old.region as usize);
                }
            }
        }
        let mut by_class: Vec<(ClassId, Vec<PieceId>)> = Vec::new();
        for id in self.fresh.iter() {
            let Some(piece) = self.new.piece(id) else { continue };
            if piece.placed() {
                continue;
            }
            match by_class.iter_mut().find(|it| it.0 == piece.class) {
                Some(group) => group.1.push(id),
                None => by_class.push((piece.class, vec![id])),
            }
        }
        by_class.sort_by(|a, b| self.new.class(a.0).cmp(self.new.class(b.0)));
        for (class_id, ids) in by_class {
            let class = self.new.class(class_id).clone();
            let mut entries: Vec<Entry> =
                ids.iter().map(|id| Entry::of(self.symbols, self.new.piece(*id).expect("live"), NOT_IAT)).collect();
            sort_entries(self.symbols, &mut entries);
            let target = self.patch_class(&class);
            let ordered = self.ordered(&class);
            for Entry { id, sym, .. } in entries {
                let space = self.new.space_of(&target).ok_or_else(|| {
                    error(
                        None,
                        format!(
                            "{} needs a {target:?} region, which the image was laid out without: link it from scratch",
                            self.symbols.describe(sym)
                        ),
                    )
                })?;
                if ordered {
                    relay.push(space);
                    self.new.hot_mut(id).region = space as u32;
                    continue;
                }
                let region = &self.new.spaces[space].region;
                let zero = self.new.spaces[space].zero;
                let piece = self.new.piece(id).expect("live");
                let at = align_up(region.addr + region.size, piece.align()) - region.addr;
                let end = at + piece.size;
                if end > region.capacity {
                    return Err(error(
                        function_of(&self.new, piece),
                        format!(
                            "{} does not fit in {target:?} ({} of {} bytes used): link the image from scratch",
                            self.symbols.describe(sym),
                            region.size,
                            region.capacity
                        ),
                    ));
                }
                let addr = region.addr + at;
                self.new.spaces[space].region.size = end;
                let hot = self.new.hot_mut(id);
                hot.addr = addr;
                hot.region = space as u32;
                hot.set_zero(zero);
            }
        }
        relay.sort_unstable();
        relay.dedup();
        if relay.is_empty() {
            self.tls();
            return Ok(());
        }
        let mut members: Vec<Vec<PieceId>> = vec![Vec::new(); relay.len()];
        for piece in self.new.live_pieces() {
            if let Ok(at) = relay.binary_search(&(piece.region as usize)) {
                members[at].push(piece.id);
            }
        }
        for (space, ids) in relay.into_iter().zip(members) {
            self.relay(space, &ids)?;
        }
        self.tls();
        Ok(())
    }

    /// Lays out an ordered region again in place, its pieces `ids`: pieces that move are
    /// written again (from their old bytes, which relocation reads for a moved piece), and
    /// what refers to them.
    fn relay(&mut self, space: usize, ids: &[PieceId]) -> Result<(), LinkError> {
        let class = self.new.spaces[space].region.class.clone();
        let ids = self.sort_pieces(&class, ids);
        let start = self.new.spaces[space].region.addr;
        let capacity = self.new.spaces[space].region.capacity;
        let zero = self.new.spaces[space].zero;
        let mut size = 0;
        let mut moved: Vec<(PieceId, u64)> = Vec::new();
        for id in &ids {
            let piece = self.new.piece(*id).expect("live");
            size = align_up(start + size, piece.align()) - start;
            let addr = start + size;
            if piece.addr != addr || self.fresh.contains(*id) {
                moved.push((*id, addr));
            }
            size += piece.size;
        }
        if size > capacity {
            return Err(error(
                None,
                format!("{class:?} needs {size} bytes and has room for {capacity}: link the image from scratch"),
            ));
        }
        for (id, addr) in moved {
            let piece = self.new.piece(id).expect("live");
            self.changed.insert(piece.sym);
            for sym in piece.defines() {
                self.changed.insert(*sym);
            }
            self.write.insert(id);
            let hot = self.new.hot_mut(id);
            hot.addr = addr;
            hot.set_zero(zero);
        }
        let old = self.new.spaces[space].region.size;
        self.new.spaces[space].region.size = size;
        if size < old && !zero {
            self.new.spaces[space].bytes.resize(size as usize);
        }
        Ok(())
    }

    /// Where the TLS template is: from the first thread-local region to the end of the
    /// last one's room, so that offsets stay as they are while it fills.
    fn tls(&mut self) {
        let tls: Vec<&Region> = self
            .new
            .spaces
            .iter()
            .map(|it| &it.region)
            .filter(|it| matches!(it.class, Class::Tdata | Class::Tbss))
            .collect();
        let (Some(first), Some(last)) = (tls.first(), tls.last()) else {
            self.new.tls = None;
            return;
        };
        let init = tls.iter().find(|it| it.class == Class::Tdata).map_or(0, |it| it.size);
        let align = tls.iter().map(|it| it.align).max().unwrap_or(1);
        self.new.tls = Some(crate::model::Tls {
            addr: first.addr,
            init,
            size: last.addr + last.capacity.max(last.size) - first.addr,
            align,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_ids::ContentId;

    fn entry(key: [u64; 4], sym: u32, id: PieceId) -> Entry {
        Entry { key, sym: Sym(sym), id }
    }

    /// The radix sort against a comparison sort, over keys where some words are constant,
    /// some bytes vary, and many keys tie (stability keeps ties in input order).
    #[test]
    fn radix_sort_agrees_with_a_comparison_sort() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for len in [0usize, 1, 2, 300, 5000] {
            let entries: Vec<Entry> = (0..len)
                .map(|at| {
                    let a = next();
                    let iat = if a & (1 << 20) == 0 { NOT_IAT } else { a >> 62 };
                    entry([iat, (a & 3) << 32 | (a >> 60), 7 + ((a >> 40) & 1), a & 0xff00_00ff_0000], at as u32, at as u32)
                })
                .collect();
            let mut want = entries.clone();
            want.sort_by_key(|it| it.key);
            let mut got = entries;
            by_key(&mut got);
            let ids = |it: &[Entry]| it.iter().map(|e| e.id).collect::<Vec<_>>();
            assert_eq!(ids(&got), ids(&want), "{len} entries");
        }
    }

    /// A symbol's prefix never orders it against another the other way from their contents.
    #[test]
    fn prefixes_keep_the_content_order() {
        let mut symbols = Symbols::default();
        let mut syms = Vec::new();
        for name in ["", "a", "a\0", "a\0b", "ab", "abcdefgh", "abcdefgi", "abcdefg", "b", "zz", "\u{ff}"] {
            syms.push(symbols.sym_name(name));
        }
        for n in 0..40u32 {
            let id = ContentId::of("blaze v2 link place test", &n.to_le_bytes());
            let id = symbols.id(&id);
            syms.push(symbols.intern(SymKey::Body(id)));
            syms.push(symbols.intern(SymKey::Data(id)));
            syms.push(symbols.intern(SymKey::Fde(id)));
        }
        syms.push(symbols.intern(SymKey::Cells));
        syms.push(symbols.intern(SymKey::Stubs));
        for &a in &syms {
            for &b in &syms {
                let (pa, pb) = (prefix(&symbols, a), prefix(&symbols, b));
                if pa < pb {
                    assert_eq!(symbols.cmp(a, b), std::cmp::Ordering::Less, "{} {}", symbols.describe(a), symbols.describe(b));
                }
            }
        }
        let mut entries: Vec<Entry> =
            syms.iter().rev().enumerate().map(|(at, sym)| entry([1, 2, 3, prefix(&symbols, *sym)], sym.0, at as u32)).collect();
        let mut many = entries.clone();
        while many.len() <= radix::SMALL {
            let base = many.len() as u32;
            many.extend(entries.iter().map(|it| Entry { id: it.id + base, ..*it }));
        }
        sort_entries(&symbols, &mut entries);
        assert!(entries.windows(2).all(|it| symbols.cmp(it[0].sym, it[1].sym) == std::cmp::Ordering::Less));
        sort_entries(&symbols, &mut many);
        assert!(many.windows(2).all(|it| symbols.cmp(it[0].sym, it[1].sym) != std::cmp::Ordering::Greater));
    }
}
