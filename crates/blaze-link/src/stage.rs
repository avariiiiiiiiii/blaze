//! A stage: a [`Change`] applied to a copy of the image's state, placed and relocated.
//!
//! The order is fixed: what is removed goes first, then what is added is made into pieces
//! (their symbols interned in one batch, shard by shard in parallel), the native inputs and
//! every name are resolved ([`crate::native`]), GOT cells and PLT stubs made for what needs
//! them, the shared pieces counted and the unused swept, everything placed
//! ([`crate::place`]), and every piece that is new or refers to something that changed
//! relocated, in parallel ([`crate::write`]).
//!
//! What a stage costs is what it touches: the sets below are bitsets over piece, slot and
//! symbol ids, the bytes of what it makes are borrowed from the change or allocated in its
//! arena (freed when the stage ends), and nothing walks every piece except a full link.

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_codegen::{Arch, CompiledFunction, Data, Init, InstanceKey, Reloc, Static, Target, Unwind};
use blaze_ids::{Hasher, radix};
use bumpalo::Bump;
use bumpalo::collections::Vec as BumpVec;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::cow::{IdSet, SymSet};
use crate::model::{Apply, Class, Fixup, Plan, Special, Sym, Writer};
use crate::position::{self, Spans};
use crate::reloc::{self, GotKind};
use crate::state::{ClassId, Def, NewPiece, PieceId, PieceRef, State, What};
use crate::symbols::{IdNo, Obj, SymKey, Symbols, Wide};
use crate::table;
use crate::unwind::{self, Field};
use crate::{Change, LinkError, Output};

const CIE_DOMAIN: &str = "blaze v2 link cie 1";

/// What a stage made or rewrote a piece as: bytes borrowed from the change or the stage's
/// arena, or zeros (a GOT cell, the cells array, a descriptor: nothing allocated).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Content<'a> {
    Bytes(&'a [u8]),
    Zero,
}

impl<'a> Content<'a> {
    /// Fills `out` (the piece's size): the bytes, zeros after them.
    pub fn copy_to(self, out: &mut [u8]) {
        match self {
            Content::Bytes(bytes) => {
                let n = bytes.len().min(out.len());
                out[..n].copy_from_slice(&bytes[..n]);
                out[n..].fill(0);
            }
            Content::Zero => out.fill(0),
        }
    }

    pub fn bytes(self) -> Option<&'a [u8]> {
        match self {
            Content::Bytes(bytes) => Some(bytes),
            Content::Zero => None,
        }
    }
}

/// The contents of the pieces a stage made (dense, by id from the first it made) or
/// rewrote (a few: positions, the descriptor, pieces a relayout moved).
pub(crate) struct Contents<'a> {
    first: PieceId,
    fresh: Vec<Option<Content<'a>>>,
    other: FxHashMap<PieceId, Content<'a>>,
}

impl<'a> Contents<'a> {
    pub fn new(first: PieceId) -> Contents<'a> {
        Contents { first, fresh: Vec::new(), other: FxHashMap::default() }
    }

    #[inline]
    pub fn get(&self, id: PieceId) -> Option<Content<'a>> {
        match id.checked_sub(self.first) {
            Some(at) => self.fresh.get(at as usize).copied().flatten(),
            None => self.other.get(&id).copied(),
        }
    }

    pub fn insert(&mut self, id: PieceId, content: Content<'a>) {
        match id.checked_sub(self.first) {
            Some(at) => {
                let at = at as usize;
                if self.fresh.len() <= at {
                    self.fresh.resize(at + 1, None);
                }
                self.fresh[at] = Some(content);
            }
            None => {
                self.other.insert(id, content);
            }
        }
    }

    pub fn remove(&mut self, id: PieceId) {
        match id.checked_sub(self.first) {
            Some(at) => {
                if let Some(it) = self.fresh.get_mut(at as usize) {
                    *it = None;
                }
            }
            None => {
                self.other.remove(&id);
            }
        }
    }

    pub fn contains(&self, id: PieceId) -> bool {
        self.get(id).is_some()
    }
}

pub(crate) struct Stage<'a> {
    pub output: &'a Output,
    pub writer: &'a dyn Writer,
    pub plan: &'a Plan,
    pub symbols: &'a mut Symbols,
    pub objects: &'a mut FxHashMap<Obj, crate::native::Read>,
    /// Each native input's bytes, borrowed for the stage ([`crate::native`]).
    pub native: crate::native::Pass<'a>,
    pub old: &'a State,
    pub new: State,
    /// The image's file name.
    pub name: &'a str,
    /// The stage's scratch: freed, all at once, when the stage ends.
    pub arena: &'a Bump,
    /// The contents of pieces this stage made or rewrote.
    pub contents: Contents<'a>,
    /// Pieces this stage made, still live.
    pub fresh: IdSet,
    /// Pieces to relocate (with [`Stage::contents`] or their bytes where they were).
    pub write: IdSet,
    /// Symbols whose definition changed: what refers to them is relocated again.
    pub changed: SymSet,
    /// Slots whose cell is written: put in this stage, or running a body that moved.
    pub dirty_cells: IdSet,
    /// Pieces removed this stage: [`State::any`] still reads them.
    pub removed: Vec<PieceId>,
    /// Slots whose function was removed: [`Stage::dangling`] fails if one is still empty
    /// and its slot still called.
    pub emptied: Vec<u32>,
    /// Shared pieces that lost a user outside [`Stage::removed`], and symbols that gained
    /// one outside [`Stage::fresh`]: [`Stage::sweep`] counts them.
    pub touched: Vec<PieceId>,
    pub counted: Vec<Sym>,
    /// Whether everything is placed anew.
    pub full: bool,
    pub table: bool,
    pub functions: u64,
    pub statics: u64,
    /// The classes [`Writer::own`] gave, asked once per stage.
    pub owned: Vec<(Class, ClassId, u32)>,
    /// Bytes reused for keys written out to be hashed.
    pub scratch: Vec<u8>,
    /// Whether [`Stage::spans`] replaced any span table.
    pub spans_changed: bool,
}

pub(crate) fn error(function: Option<InstanceKey>, what: impl Into<String>) -> LinkError {
    LinkError { function, what: what.into() }
}

/// The key a codegen target is, and whether it is referred to weakly.
fn wide(target: &Target, table: bool) -> (Wide<'_>, bool) {
    match target {
        Target::Function(key) if table => (Wide::Slot(key), false),
        Target::Function(key) | Target::Direct(key) => (Wide::Body(key), false),
        Target::Static { item, instance } => (Wide::Static(item, instance), false),
        Target::Data(id) => (Wide::Data(id), false),
        Target::Extern(name) => (Wide::Name(name), false),
        Target::Weak(name) => (Wide::Name(name), true),
        Target::Position(span) => (Wide::Position(span), false),
    }
}

/// The keys of a function's symbols, in the order [`Stage::add_function`] reads them: its
/// body, its relocations' targets, then each datum and its relocations' targets.
fn function_keys<'f>(function: &'f CompiledFunction, table: bool, out: &mut [Wide<'f>]) {
    let mut at = 0;
    let mut put = |key: Wide<'f>| {
        out[at] = key;
        at += 1;
    };
    put(Wide::Body(&function.key));
    for reloc in &function.relocs {
        put(wide(&reloc.target, table).0);
    }
    for data in &function.data {
        put(Wide::Data(&data.id));
        for reloc in &data.relocs {
            put(wide(&reloc.target, table).0);
        }
    }
}

fn key_count(function: &CompiledFunction) -> usize {
    1 + function.relocs.len() + function.data.iter().map(|it| 1 + it.relocs.len()).sum::<usize>()
}

impl<'a> Stage<'a> {
    pub fn sym(&mut self, key: SymKey) -> Sym {
        self.symbols.intern(key)
    }

    pub fn sym_name(&mut self, name: &str) -> Sym {
        self.symbols.sym_name(name)
    }

    fn target_sym(&mut self, target: &Target) -> Sym {
        let key = match wide(target, self.table).0 {
            Wide::Key(key) => key,
            Wide::Name(name) => SymKey::Name(self.symbols.name(name)),
            Wide::Body(key) => SymKey::Body(self.symbols.id(&key.0)),
            Wide::Slot(key) => SymKey::Slot(self.symbols.id(&key.0)),
            Wide::Data(id) => SymKey::Data(self.symbols.id(id)),
            Wide::Static(item, instance) => SymKey::Static(self.symbols.id(&item.0), self.symbols.id(instance)),
            Wide::Position(span) => SymKey::Position(self.symbols.id(&span.anchor), span.index),
        };
        self.symbols.intern(key)
    }

    /// The symbols of relocations' targets, interned in one batch.
    fn reloc_syms(&mut self, relocs: &[Reloc]) -> Vec<Sym> {
        let table = self.table;
        let keys: Vec<Wide<'_>> = relocs.iter().map(|it| wide(&it.target, table).0).collect();
        self.symbols.intern_wide(&keys)
    }

    /// Defines `sym`, noting a change.
    pub fn define(&mut self, sym: Sym, def: Def) {
        if self.new.def(sym) != def {
            self.changed.insert(sym);
            self.new.set_def(sym, def);
        }
    }

    /// Adds a piece with its contents, defining its own symbol at its start.
    pub fn make(&mut self, piece: &NewPiece<'_>, content: Content<'a>) -> PieceId {
        let id = self.new.add(piece);
        self.contents.insert(id, content);
        self.fresh.insert(id);
        self.write.insert(id);
        self.define(piece.sym, Def::Piece { piece: id, offset: 0 });
        id
    }

    /// Removes a piece; [`Stage::sweep`] counts what it referred to.
    pub fn unmake(&mut self, id: PieceId) {
        if !self.new.remove(id) {
            return;
        }
        let piece = self.new.any(id).expect("just removed");
        self.changed.insert(piece.sym);
        for sym in piece.defines() {
            self.changed.insert(*sym);
        }
        self.fresh.remove(id);
        self.write.remove(id);
        self.contents.remove(id);
        self.removed.push(id);
    }

    /// A piece's fields, with rank 0, the order of codegen's pieces and not zeroed.
    pub fn piece<'x>(
        &mut self,
        sym: Sym,
        class: &Class,
        align: u64,
        size: u64,
        fixups: &'x [Fixup],
        what: What,
    ) -> NewPiece<'x> {
        let class = self.new.class_id(class);
        Self::piece_in(sym, class, align, size, fixups, what)
    }

    fn piece_in<'x>(sym: Sym, class: ClassId, align: u64, size: u64, fixups: &'x [Fixup], what: What) -> NewPiece<'x> {
        NewPiece {
            sym,
            class,
            rank: 0,
            order: [u32::MAX; 3],
            align: align.max(1),
            size,
            zero: false,
            fixups,
            weak: &[],
            defines: &[],
            what,
        }
    }

    /// The class and rank the writer gives codegen's pieces of `class`.
    fn own(&mut self, class: Class) -> (ClassId, u32) {
        if let Some((_, id, rank)) = self.owned.iter().find(|it| it.0 == class) {
            return (*id, *rank);
        }
        let (to, rank) = self.writer.own(&class);
        let id = self.new.class_id(&to);
        self.owned.push((class, id, rank));
        (id, rank)
    }

    pub fn arch(&self) -> Arch {
        self.output.arch
    }

    fn fixup_slice(&self, fixups: impl IntoIterator<Item = Fixup>) -> &'a [Fixup] {
        BumpVec::from_iter_in(fixups, self.arena).into_bump_slice()
    }

    /// Applies everything `change` says to the copy of the state.
    pub fn apply(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        self.spans(change)?;
        if self.table && !self.new.laid_out {
            self.fixed(change.added.len() as u64)?;
        }
        for key in &change.removed {
            if let Some(sym) = self.symbols.find_body(key) {
                if let Some((piece, _)) = self.new.home(sym) {
                    self.remove_function(piece);
                }
            }
        }
        for (item, instance) in &change.removed_statics {
            let (Some(item), Some(instance)) = (self.symbols.find_id(&item.0), self.symbols.find_id(instance)) else {
                continue;
            };
            for key in [SymKey::Static(item, instance), SymKey::Tlv(item, instance)] {
                if let Some(sym) = self.symbols.find(key) {
                    if let Def::Piece { piece, .. } = self.new.def(sym) {
                        self.unmake(piece);
                    }
                }
            }
        }
        for (key, name) in &change.debug_names {
            let (id, name) = (self.symbols.id(&key.0), self.symbols.name(name));
            self.new.set_debug_name(id, name);
        }
        self.functions(&change.added)?;
        self.slots(&change.slots)?;
        for it in &change.statics {
            self.add_static(it)?;
        }
        let mut named = Vec::with_capacity(change.names.len());
        for (name, target) in &change.names {
            let name = self.symbols.name(name);
            let sym = self.target_sym(target);
            named.push((name, sym));
        }
        let symbols = &*self.symbols;
        let names = &self.new.names;
        let differs = named.iter().any(|(name, sym)| names.get(name) != Some(sym))
            || change.unnamed.iter().any(|it| symbols.find_name(it).is_some_and(|it| names.contains_key(&it)));
        if differs {
            let names = Arc::make_mut(&mut self.new.names);
            for name in &change.unnamed {
                if let Some(name) = symbols.find_name(name) {
                    names.remove(&name);
                }
            }
            for (name, sym) in &named {
                names.insert(*name, *sym);
            }
        }
        self.positions()?;
        let mut seen: Vec<Sym> = Vec::new();
        for id in self.fresh.iter() {
            let piece = self.new.piece(id).expect("a fresh piece");
            seen.extend(
                piece.fixups().iter().map(|it| it.target).filter(|it| matches!(self.symbols.key(*it), SymKey::Name(_))),
            );
        }
        for (name, _) in named {
            seen.push(self.sym(SymKey::Name(name)));
        }
        for sym in seen {
            self.new.see(sym);
        }
        Ok(())
    }

    /// The live descriptor and the path it names, filled once the image is placed.
    pub fn descriptor(&mut self) {
        let Some(live) = &self.output.live else { return };
        let path: &'a [u8] = live.path.as_bytes();
        let dir = self.sym(SymKey::LiveDir);
        if self.new.home(dir).is_none() {
            let piece = self.piece(dir, &Class::Rodata, 1, path.len() as u64, &[], What::Fixed);
            self.make(&piece, Content::Bytes(path));
        }
        let descriptor = self.sym(SymKey::Descriptor);
        if self.new.home(descriptor).is_none() {
            let size = blaze_live::patch::Descriptor::SIZE as u64;
            let piece = self.piece(descriptor, &Class::Rodata, 8, size, &[], What::Fixed);
            self.make(&piece, Content::Zero);
        }
    }

    /// The jump table's generation cell, cells and stubs, with room for `functions` and
    /// what incremental links add.
    fn fixed(&mut self, functions: u64) -> Result<(), LinkError> {
        let capacity = table::capacity(functions, true);
        let capacity = u32::try_from(capacity).map_err(|_| error(None, "a jump table past 2^32 slots"))?;
        self.new.capacity = capacity;
        let cells = self.sym(SymKey::Cells);
        let generation = self.sym(SymKey::Generation);
        let fixups = self.fixup_slice([Fixup { at: 0, apply: Apply::Abs64, target: cells, addend: 0, minus: None }]);
        let piece = self.piece(generation, &Class::Table, 8, 8, fixups, What::Fixed);
        self.make(&piece, Content::Zero);
        let mut piece = self.piece(cells, &Class::Table, 8, u64::from(capacity) * 8, &[], What::Cells);
        piece.rank = 1;
        self.make(&piece, Content::Zero);
        let arch = self.arch();
        let size = table::stub_size(arch) as usize;
        let code: &'a mut [u8] = self.arena.alloc_slice_fill_copy(size * capacity as usize, 0);
        if size > 0 {
            code.par_chunks_mut(size).with_min_len(4096).enumerate().for_each(|(slot, out)| {
                table::write_stub(arch, slot as u32, out);
            });
        }
        let stub = table::stub_fixups(arch);
        let fixups = self.fixup_slice((0..u64::from(capacity)).flat_map(|slot| {
            stub.iter().map(move |it| Fixup {
                at: slot * size as u64 + it.at,
                apply: it.apply,
                target: generation,
                addend: it.addend,
                minus: None,
            })
        }));
        let stubs = self.sym(SymKey::Stubs);
        let piece = self.piece(stubs, &Class::Stubs, 16, code.len() as u64, fixups, What::Fixed);
        self.make(&piece, Content::Bytes(code));
        Ok(())
    }

    fn remove_function(&mut self, piece: PieceId) {
        let Some((body, info)) = self.new.piece(piece).and_then(|it| Some((it.sym, *it.function()?))) else {
            return;
        };
        self.unmake(piece);
        for sym in info.fde.into_iter().chain(info.windows.map(|it| it.0)) {
            if let Def::Piece { piece, .. } = self.new.def(sym) {
                self.unmake(piece);
            }
        }
        if self.table {
            self.unslot(body);
        }
    }

    /// The jump table's cells piece.
    fn cells_piece(&self) -> Option<PieceId> {
        match self.new.def(self.symbols.find(SymKey::Cells)?) {
            Def::Piece { piece, .. } => Some(piece),
            _ => None,
        }
    }

    /// Points a slot's cell at a body.
    fn cell(&mut self, slot: Sym, body: Sym) {
        let Some((_, offset)) = self.new.home(slot) else { return };
        let index = (offset / table::stub_size(self.arch())) as u32;
        let Some(cells) = self.cells_piece() else { return };
        self.new.set_cell(index, Some(body));
        self.dirty_cells.insert(index);
        self.write.insert(cells);
    }

    /// Empties every cell holding `body`, which is gone: the cell keeps its bytes (a running
    /// program may still be in the old code) until a function is put in its slot.
    fn unslot(&mut self, body: Sym) {
        let slots: Vec<u32> = self.new.slots_of(body).collect();
        for slot in slots {
            self.new.set_cell(slot, None);
            self.dirty_cells.remove(slot);
            self.emptied.push(slot);
        }
    }

    /// Puts each function in the slot `change` names for it.
    fn slots(&mut self, slots: &[(InstanceKey, InstanceKey)]) -> Result<(), LinkError> {
        if slots.is_empty() {
            return Ok(());
        }
        if !self.table {
            let (slot, _) = slots[0];
            return Err(error(
                Some(slot),
                "a function put in another's slot, in an image linked without a jump table",
            ));
        }
        for (name, function) in slots {
            let body = self
                .symbols
                .find_body(function)
                .filter(|it| self.new.home(*it).is_some())
                .ok_or_else(|| {
                    error(Some(*function), format!("put in the slot of {name:?}, and neither added nor linked"))
                })?;
            let id = self.symbols.id(&name.0);
            let slot = self.sym(SymKey::Slot(id));
            if self.new.home(slot).is_none() {
                self.new_slot(slot, *name)?;
            }
            self.cell(slot, body);
        }
        Ok(())
    }

    /// The functions added, made into pieces: their keys built in parallel and interned in
    /// one batch.
    fn functions(&mut self, added: &[&'a CompiledFunction]) -> Result<(), LinkError> {
        let mut seen: FxHashSet<&InstanceKey> = FxHashSet::default();
        seen.reserve(added.len());
        let mut todo: Vec<&'a CompiledFunction> = Vec::with_capacity(added.len());
        for function in added {
            if function.arch != self.arch() {
                return Err(error(
                    Some(function.key),
                    format!("compiled for {:?}, linked into an image for {:?}", function.arch, self.arch()),
                ));
            }
            if !seen.insert(&function.key) {
                return Err(error(Some(function.key), "added twice in one change"));
            }
            let body = self.symbols.find_body(&function.key);
            if body.and_then(|it| self.new.home(it)).is_some() {
                continue;
            }
            todo.push(function);
        }
        let counts: Vec<usize> = todo.iter().map(|it| key_count(it)).collect();
        let mut keys: Vec<Wide<'a>> = vec![Wide::Key(SymKey::Generation); counts.iter().sum()];
        let mut parts: Vec<&mut [Wide<'a>]> = Vec::with_capacity(todo.len());
        let mut rest = &mut keys[..];
        for count in &counts {
            let (part, after) = rest.split_at_mut(*count);
            parts.push(part);
            rest = after;
        }
        let table = self.table;
        parts.into_par_iter().zip(todo.par_iter()).for_each(|(out, function)| function_keys(*function, table, out));
        let syms = self.symbols.intern_wide(&keys);
        let mut at = 0;
        for (function, count) in todo.iter().zip(counts) {
            self.add_function(function, &syms[at..at + count])?;
            at += count;
        }
        self.functions += todo.len() as u64;
        Ok(())
    }

    fn fixups(
        &self,
        relocs: &[Reloc],
        syms: &[Sym],
        function: Option<InstanceKey>,
        in_code: bool,
    ) -> Result<(&'a [Fixup], &'a [Sym]), LinkError> {
        let mut fixups = BumpVec::with_capacity_in(relocs.len(), self.arena);
        let mut weak = BumpVec::new_in(self.arena);
        for (reloc, sym) in relocs.iter().zip(syms) {
            let apply = reloc::of_kind(reloc.kind, self.output.format, self.arch(), in_code).map_err(|it| error(function, it))?;
            if matches!(reloc.target, Target::Weak(_)) {
                weak.push(*sym);
            }
            fixups.push(Fixup { at: u64::from(reloc.at), apply, target: *sym, addend: reloc.addend, minus: None });
        }
        weak.sort_unstable();
        weak.dedup();
        Ok((fixups.into_bump_slice(), weak.into_bump_slice()))
    }

    fn add_function(&mut self, function: &'a CompiledFunction, syms: &[Sym]) -> Result<(), LinkError> {
        let key = Some(function.key);
        let body = syms[0];
        let (fixups, weak) = self.fixups(&function.relocs, &syms[1..=function.relocs.len()], key, true)?;
        if self.arch() == Arch::Wasm32 && wasm_imports_in(&function.code).map_err(|it| error(key, it))? {
            let (code, _) = blaze_codegen::wasm::Code::decode(&function.code)
                .map_err(|it| error(key, format!("its wasm header: {it}")))?;
            self.wasm_imports(&code.imports);
        }
        let mut at = 1 + function.relocs.len();
        for data in &function.data {
            let sym = syms[at];
            let relocs = &syms[at + 1..at + 1 + data.relocs.len()];
            at += 1 + data.relocs.len();
            self.add_data(data, sym, relocs, key)?;
        }
        let mut fde_sym = None;
        let mut windows = None;
        match &function.unwind {
            Some(Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
                let personality = personality.as_ref().map(|it| self.target_sym(it));
                let mut hasher = Hasher::new(CIE_DOMAIN);
                hasher
                    .part(&code_align.to_le_bytes())
                    .part(&data_align.to_le_bytes())
                    .part(&return_address.to_le_bytes())
                    .part(cie);
                if let Some(personality) = personality {
                    self.scratch.clear();
                    self.symbols.write_key(personality, &mut self.scratch);
                    hasher.part(&self.scratch);
                }
                let cie_id = self.symbols.id(&hasher.finish());
                let cie_sym = self.sym(SymKey::Cie(cie_id));
                if self.new.home(cie_sym).is_none() {
                    let record = unwind::cie(*code_align, *data_align, *return_address, cie, personality.is_some())
                        .map_err(|it| error(key, it))?;
                    let fixups = self.fixup_slice(record.fixups.iter().map(|(at, _)| Fixup {
                        at: *at,
                        apply: Apply::GotRel32,
                        target: personality.expect("a personality field"),
                        addend: 0,
                        minus: None,
                    }));
                    let bytes: &'a [u8] = self.arena.alloc_slice_copy(&record.bytes);
                    let piece = self.piece(cie_sym, &Class::EhFrame, 4, bytes.len() as u64, fixups, What::Shared);
                    self.make(&piece, Content::Bytes(bytes));
                }
                let lsda = match lsda {
                    Some(id) => {
                        if !function.data.iter().any(|it| it.id == *id) {
                            return Err(error(key, "its LSDA is not one of its data"));
                        }
                        let id = self.symbols.id(id);
                        Some(self.sym(SymKey::Data(id)))
                    }
                    None => None,
                };
                let record = unwind::fde(function.code.len() as u64, fde, personality.is_some(), lsda.is_some())
                    .map_err(|it| error(key, it))?;
                let fde_key = self.symbols.id(&function.key.0);
                let sym = self.sym(SymKey::Fde(fde_key));
                let fixups = self.fixup_slice(record.fixups.iter().map(|(at, field)| match field {
                    Field::CiePointer => {
                        Fixup { at: *at, apply: Apply::Delta32, target: sym, addend: *at as i64, minus: Some(cie_sym) }
                    }
                    Field::Function => Fixup { at: *at, apply: Apply::Rel32, target: body, addend: 0, minus: None },
                    Field::Lsda => Fixup {
                        at: *at,
                        apply: Apply::Rel32,
                        target: lsda.expect("an LSDA field"),
                        addend: 0,
                        minus: None,
                    },
                    Field::Personality => unreachable!("a CIE's field"),
                }));
                let bytes: &'a [u8] = self.arena.alloc_slice_copy(&record.bytes);
                let mut piece = self.piece(sym, &Class::EhFrame, 4, bytes.len() as u64, fixups, What::Fde);
                piece.rank = 1;
                self.make(&piece, Content::Bytes(bytes));
                fde_sym = Some(sym);
            }
            Some(Unwind::Windows { info: bytes, handler, lsda }) => {
                let handler = handler.as_ref().map(|it| self.target_sym(it));
                let lsda = lsda.map(|id| {
                    let id = self.symbols.id(&id);
                    self.sym(SymKey::Data(id))
                });
                let padded = bytes.len().next_multiple_of(4);
                let targets = handler.into_iter().chain(lsda);
                let size = padded + 4 * targets.clone().count();
                let out: &'a mut [u8] = self.arena.alloc_slice_fill_copy(size, 0);
                out[..bytes.len()].copy_from_slice(bytes);
                let fixups = self.fixup_slice(targets.enumerate().map(|(n, target)| Fixup {
                    at: (padded + 4 * n) as u64,
                    apply: Apply::ImageRel32,
                    target,
                    addend: 0,
                    minus: None,
                }));
                let info_key = self.symbols.id(&function.key.0);
                let sym = self.sym(SymKey::WinInfo(info_key));
                let piece = self.piece(sym, &Class::Rodata, 4, size as u64, fixups, What::WinInfo);
                self.make(&piece, Content::Bytes(out));
                windows = Some((sym, handler, lsda));
            }
            None => {}
        }
        let id = self.symbols.id(&function.key.0);
        let info = self.new.add_function(function.key, id, fde_sym, windows, &function.lines);
        let (class, rank) = self.own(Class::Text);
        let mut piece = Self::piece_in(
            body,
            class,
            u64::from(function.align),
            function.code.len() as u64,
            fixups,
            What::Function(info),
        );
        piece.rank = rank;
        piece.weak = weak;
        self.make(&piece, Content::Bytes(&function.code));
        if self.table {
            let slot = self.sym(SymKey::Slot(id));
            if self.new.home(slot).is_none() {
                self.new_slot(slot, function.key)?;
            }
            self.cell(slot, body);
        }
        Ok(())
    }

    fn new_slot(&mut self, slot: Sym, function: InstanceKey) -> Result<(), LinkError> {
        if self.new.slots >= self.new.capacity {
            return Err(error(
                Some(function),
                format!("the jump table is full ({} slots): link the image from scratch", self.new.capacity),
            ));
        }
        let stubs = self.symbols.find(SymKey::Stubs).expect("the table was made");
        let Def::Piece { piece, .. } = self.new.def(stubs) else {
            return Err(error(Some(function), "the jump table's stubs are gone"));
        };
        let index = self.new.new_slot(slot);
        let offset = u64::from(index) * table::stub_size(self.arch());
        self.define(slot, Def::Piece { piece, offset });
        Ok(())
    }

    fn add_data(
        &mut self,
        data: &'a Data,
        sym: Sym,
        relocs: &[Sym],
        function: Option<InstanceKey>,
    ) -> Result<(), LinkError> {
        if self.new.home(sym).is_some() {
            return Ok(());
        }
        let (fixups, weak) = self.fixups(&data.relocs, relocs, function, false)?;
        let (class, rank) = self.own(if fixups.is_empty() { Class::Rodata } else { Class::RelRo });
        let mut piece =
            Self::piece_in(sym, class, u64::from(data.align), data.bytes.len() as u64, fixups, What::Shared);
        piece.rank = rank;
        piece.weak = weak;
        self.make(&piece, Content::Bytes(&data.bytes));
        Ok(())
    }

    fn add_static(&mut self, it: &'a Static) -> Result<(), LinkError> {
        let (item, instance) = (self.symbols.id(&it.item.0), self.symbols.id(&it.instance));
        let sym = self.sym(SymKey::Static(item, instance));
        if let Def::Piece { piece, .. } = self.new.def(sym) {
            self.unmake(piece);
        }
        for data in &it.data {
            let id = self.symbols.id(&data.id);
            let data_sym = self.sym(SymKey::Data(id));
            let relocs = self.reloc_syms(&data.relocs);
            self.add_data(data, data_sym, &relocs, None)?;
        }
        let (bytes, relocs, size, zero): (&'a [u8], &'a [Reloc], u64, bool) = match &it.init {
            Init::Bytes { bytes, relocs } => (&bytes[..], &relocs[..], bytes.len() as u64, false),
            Init::Zeroed { size } => (&[][..], &[][..], *size, true),
        };
        let syms = self.reloc_syms(relocs);
        let (fixups, weak) = self.fixups(relocs, &syms, None, false)?;
        if self.arch() == Arch::Wasm32 {
            self.wasm_imports(&it.imports);
        }
        let static_error = |what: String| error(None, format!("the static {:?}: {what}", it.item));
        let (class, rank) = match &it.section {
            Some(name) => {
                let (class, rank) = self
                    .writer
                    .section(name, it.mutable || !fixups.is_empty(), false, it.thread_local)
                    .map_err(static_error)?;
                (self.new.class_id(&class), rank)
            }
            None => self.own(match (it.thread_local, it.mutable, zero, fixups.is_empty()) {
                (true, _, false, _) => Class::Tdata,
                (true, _, true, _) => Class::Tbss,
                (false, true, true, _) => Class::Bss,
                (false, true, false, _) => Class::Data,
                (false, false, _, true) => Class::Rodata,
                (false, false, _, false) => Class::RelRo,
            }),
        };
        let zeroed = crate::state::zeroed(self.new.class(class));
        let content = match (zero, zeroed) {
            (false, true) => {
                let class = self.new.class(class).clone();
                return Err(static_error(format!("initialized bytes in the zeroed section {class:?}")));
            }
            (false, false) => Content::Bytes(bytes),
            (true, _) => Content::Zero,
        };
        let mut piece =
            Self::piece_in(sym, class, u64::from(it.align), size, fixups, What::Static { thread_local: it.thread_local });
        piece.rank = rank;
        piece.zero = zeroed;
        piece.weak = weak;
        self.make(&piece, content);
        if it.thread_local && self.plan.tls_descriptors {
            let tlv = self.sym(SymKey::Tlv(item, instance));
            if let Def::Piece { piece, .. } = self.new.def(tlv) {
                self.unmake(piece);
            }
            let bootstrap = self.sym_name("_tlv_bootstrap");
            let fixups = self.fixup_slice([
                Fixup { at: 0, apply: Apply::Abs64, target: bootstrap, addend: 0, minus: None },
                Fixup { at: 16, apply: Apply::DtpOff64, target: sym, addend: 0, minus: None },
            ]);
            let piece = self.piece(tlv, &Class::ThreadVars, 8, 24, fixups, What::Tlv);
            self.make(&piece, Content::Zero);
        }
        self.statics += 1;
        Ok(())
    }

    /// The span tables the change carries, checked and kept once each; the panic locations
    /// of a changed one rewritten in place. Finding them costs the changed table's spans.
    fn spans(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        if change.spans.is_empty() {
            return Ok(());
        }
        let mut changed: Vec<(IdNo, u32)> = Vec::new();
        for (anchor, bytes) in &change.spans {
            let id = self.symbols.id(anchor);
            let before = match self.new.span_table(id) {
                Some(it) if it.is(bytes) => continue,
                Some(it) => it.len(),
                None => 0,
            };
            let spans = Spans::new(bytes).map_err(|it| error(None, format!("the span table of {anchor:?}: {it}")))?;
            let len = before.max(spans.len());
            self.new.set_span_table(id, spans);
            self.spans_changed = true;
            changed.push((id, len));
        }
        for (anchor, len) in changed {
            for index in 0..len {
                let Some(sym) = self.symbols.find(SymKey::Position(anchor, index)) else { continue };
                let Some(id) = self.new.home(sym).map(|it| it.0).filter(|it| self.new.piece(*it).is_some()) else {
                    continue;
                };
                let (bytes, file) = self.location(anchor, index)?;
                let fixups = [Fixup { at: 0, apply: Apply::Abs64, target: file, addend: 0, minus: None }];
                self.retarget(id, &fixups);
                self.contents.insert(id, Content::Bytes(bytes));
                self.write.insert(id);
            }
        }
        Ok(())
    }

    /// Replaces the fixups of a piece that was there before the stage, counting the shared
    /// pieces it stops and starts referring to.
    fn retarget(&mut self, id: PieceId, fixups: &[Fixup]) {
        let before = State::targets(self.new.piece(id).expect("a live piece"));
        self.new.set_fixups(id, fixups);
        let after = State::targets(self.new.piece(id).expect("a live piece"));
        for target in before.iter().filter(|it| !after.contains(it)) {
            if let Some(shared) = self.shared(*target) {
                let users = self.new.users_mut(shared);
                *users = users.saturating_sub(1);
                self.touched.push(shared);
            }
        }
        for target in after.iter().filter(|it| !before.contains(it)) {
            self.counted.push(*target);
        }
    }

    /// A span's `Location` record and the symbol of its file name, made when missing.
    fn location(&mut self, anchor: IdNo, index: u32) -> Result<(&'a [u8], Sym), LinkError> {
        let arena = self.arena;
        let Some(table) = self.new.span_table(anchor) else {
            let anchor = self.symbols.content(anchor);
            return Err(error(None, format!("a position in {anchor:?}, whose span table the link was not given")));
        };
        let Some(at) = table.span(index) else {
            let len = table.len();
            let anchor = self.symbols.content(anchor);
            return Err(error(None, format!("span {index} of {anchor:?}, whose table has {len}")));
        };
        let name = table.file(at.file);
        let record: &'a [u8] = arena.alloc_slice_copy(&position::location(name.len(), at));
        let file_name = self.symbols.name(name);
        let file = self.symbols.intern(SymKey::FileName(file_name));
        let bytes: Option<&'a [u8]> = self.new.home(file).is_none().then(|| {
            let out = arena.alloc_slice_fill_copy(name.len() + 1, 0);
            out[..name.len()].copy_from_slice(name.as_bytes());
            &*out
        });
        if let Some(bytes) = bytes {
            let piece = self.piece(file, &Class::Rodata, 1, bytes.len() as u64, &[], What::Shared);
            self.make(&piece, Content::Bytes(bytes));
        }
        Ok((record, file))
    }

    /// The `Location` records the fresh pieces refer to and that don't exist yet.
    fn positions(&mut self) -> Result<(), LinkError> {
        let mut wanted: Vec<(Sym, IdNo, u32)> = Vec::new();
        for id in self.fresh.iter() {
            let piece = self.new.piece(id).expect("a fresh piece");
            for fixup in piece.fixups() {
                if let SymKey::Position(anchor, index) = self.symbols.key(fixup.target) {
                    wanted.push((fixup.target, anchor, index));
                }
            }
        }
        wanted.sort_unstable_by_key(|it| it.0);
        wanted.dedup_by_key(|it| it.0);
        let class = self.new.class_id(&Class::RelRo);
        for (sym, anchor, index) in wanted {
            if self.new.home(sym).is_some() {
                continue;
            }
            let (bytes, file) = self.location(anchor, index)?;
            let fixups = self.fixup_slice([Fixup { at: 0, apply: Apply::Abs64, target: file, addend: 0, minus: None }]);
            let piece = Self::piece_in(
                sym,
                class,
                position::LOCATION_ALIGN,
                position::LOCATION_SIZE,
                fixups,
                What::Shared,
            );
            self.make(&piece, Content::Bytes(bytes));
        }
        Ok(())
    }

    /// Counts the fixups to shared pieces that this stage added and removed, and removes the
    /// shared pieces nothing refers to any more.
    pub fn sweep(&mut self) {
        let mut touched = std::mem::take(&mut self.touched);
        let fresh = self.fresh.sorted();
        let mut targets: Vec<Sym> = std::mem::take(&mut self.counted);
        for id in &fresh {
            let piece = self.new.piece(*id).expect("a fresh piece");
            targets.extend(State::targets(piece));
            if matches!(piece.what(), What::Shared) {
                touched.push(*id);
            }
        }
        for target in targets {
            if let Some(shared) = self.shared(target) {
                *self.new.users_mut(shared) += 1;
            }
        }
        let mut at = 0;
        while at < self.removed.len() || !touched.is_empty() {
            while at < self.removed.len() {
                let id = self.removed[at];
                at += 1;
                if self.old.piece(id).is_none() && fresh.binary_search(&id).is_err() {
                    continue;
                }
                let targets = State::targets(self.new.any(id).expect("a removed piece"));
                for target in targets {
                    if let Some(shared) = self.shared(target).or_else(|| self.old_shared(target)) {
                        if shared < self.new.piece_count() {
                            let users = self.new.users_mut(shared);
                            *users = users.saturating_sub(1);
                        }
                        touched.push(shared);
                    }
                }
            }
            touched.sort_unstable();
            touched.dedup();
            for id in std::mem::take(&mut touched) {
                let live = self.new.piece(id).is_some_and(|it| matches!(it.what(), What::Shared));
                if live && self.new.users(id) == 0 {
                    self.unmake(id);
                }
            }
        }
    }

    /// The shared piece a symbol is at the start of, in the new state.
    fn shared(&self, sym: Sym) -> Option<PieceId> {
        match self.new.def(sym) {
            Def::Piece { piece, offset: 0 } => matches!(self.new.piece(piece)?.what(), What::Shared).then_some(piece),
            _ => None,
        }
    }

    /// The same, as it was before the stage: a removed piece's references were counted then.
    fn old_shared(&self, sym: Sym) -> Option<PieceId> {
        match self.old.def(sym) {
            Def::Piece { piece, offset: 0 } => matches!(self.old.piece(piece)?.what(), What::Shared).then_some(piece),
            _ => None,
        }
    }

    /// The GOT cell of a target, if one was made.
    pub fn got_cell(&self, kind: GotKind, target: Sym) -> Option<Sym> {
        self.symbols.find(SymKey::Got(kind, target))
    }

    /// The PLT stub of a target, if one was made.
    pub fn plt_stub(&self, target: Sym) -> Option<Sym> {
        self.symbols.find(SymKey::Plt(target))
    }

    /// The GOT cells and PLT stubs the pieces to write need, made when missing.
    pub fn needs(&mut self) -> Result<(), LinkError> {
        let mut cells: Vec<(GotKind, Sym)> = Vec::new();
        let mut plt: Vec<Sym> = Vec::new();
        let mut got_names: Vec<String> = Vec::new();
        for id in self.write.iter() {
            let Some(piece) = self.new.piece(id) else { continue };
            for fixup in piece.fixups() {
                if let Some(kind) = reloc::got_kind(fixup.apply) {
                    cells.push((kind, self.got_target(fixup)));
                } else if self.through_plt(fixup) {
                    plt.push(fixup.target);
                } else if let Some(Special::Got(name)) = self.special_of(fixup.target) {
                    got_names.push(name.clone());
                }
            }
        }
        for name in got_names {
            let target = self.sym_name(&name);
            cells.push((GotKind::Address, target));
        }
        radix::sort_copy_by_key(&mut plt, |it| it.0);
        plt.dedup();
        cells.extend(plt.iter().map(|it| (GotKind::Address, *it)));
        radix::sort_copy_by_key(&mut cells, |&(kind, sym)| radix::pack_32(kind as u32, sym.0));
        cells.dedup();
        let got = self.new.class_id(&Class::Got);
        for (kind, target) in cells {
            let sym = self.sym(SymKey::Got(kind, target));
            if self.new.home(sym).is_some() {
                continue;
            }
            let size = kind.cells() * 8;
            let fixups = self.got_fixups(kind, target);
            let mut piece = Self::piece_in(sym, got, 8, size, fixups, What::Got { kind, target });
            piece.order = [u32::MAX, 0, 0];
            self.make(&piece, Content::Zero);
        }
        let arch = self.arch();
        let class = self.new.class_id(&Class::Plt);
        for target in plt {
            let sym = self.sym(SymKey::Plt(target));
            if self.new.home(sym).is_some() {
                continue;
            }
            let (code, stub) = table::plt(arch);
            let fixups = self.fixup_slice(stub.iter().map(|it| Fixup {
                at: it.at,
                apply: match it.apply {
                    Apply::Rel32 => Apply::X86GotPcRel32,
                    Apply::A64Page21 => Apply::A64GotPage21,
                    Apply::A64Lo12 { .. } => Apply::A64GotLo12,
                    other => other,
                },
                target,
                addend: it.addend,
                minus: None,
            }));
            let piece = Self::piece_in(sym, class, 16, code.len() as u64, fixups, What::Plt { target });
            self.make(&piece, Content::Bytes(code));
        }
        if self.plan.got_by_library {
            self.iat_ends();
        }
        Ok(())
    }

    /// The zero cell after each needed DLL's import address table cells, which the GOT's
    /// layout puts after them ([`Stage::iat_end`]); those of DLLs no longer needed removed.
    /// The cells there are are the last resolution's libraries' ([`Stage::old`]).
    fn iat_ends(&mut self) {
        let mut names: Vec<crate::symbols::NameId> =
            self.new.resolved.libraries.iter().map(|it| self.symbols.name(&it.name)).collect();
        for library in self.old.resolved.libraries.iter() {
            let Some(name) = self.symbols.find_name(&library.name) else { continue };
            if names.contains(&name) {
                continue;
            }
            if let Some(Def::Piece { piece, .. }) = self.symbols.find(SymKey::IatEnd(name)).map(|it| self.new.def(it)) {
                self.unmake(piece);
            }
        }
        names.sort_by(|a, b| self.symbols.str(*a).cmp(self.symbols.str(*b)));
        names.dedup();
        let got = self.new.class_id(&Class::Got);
        for name in names {
            let sym = self.sym(SymKey::IatEnd(name));
            if self.new.home(sym).is_some() {
                continue;
            }
            let mut piece = Self::piece_in(sym, got, 8, 8, &[], What::Fixed);
            piece.order = [u32::MAX, 0, 0];
            self.make(&piece, Content::Zero);
        }
    }

    /// Of an import address table's zero cell, the index of its DLL in the image's libraries.
    pub fn iat_end(&self, sym: Sym) -> Option<u32> {
        let SymKey::IatEnd(name) = self.symbols.key(sym) else { return None };
        let name = self.symbols.str(name);
        self.new.resolved.libraries.iter().position(|it| it.name == name).map(|it| it as u32)
    }

    /// What a GOT cell of a fixup holds the address of: a thread-local static's TLV
    /// descriptor for the TLV kinds, else the target.
    pub fn got_target(&self, fixup: &Fixup) -> Sym {
        crate::write::got_target(&self.symbols, fixup)
    }

    fn imported(&self, sym: Sym) -> bool {
        self.import_of(sym).is_some()
    }

    /// The import a symbol is, through its aliases.
    pub fn import_of(&self, mut sym: Sym) -> Option<u32> {
        for _ in 0..64 {
            match self.new.def(sym) {
                Def::Import(at) => return Some(at),
                Def::Alias(to) => sym = to,
                _ => return None,
            }
        }
        None
    }

    /// The linker's symbol a symbol is, through its aliases.
    pub fn special_of(&self, mut sym: Sym) -> Option<&Special> {
        for _ in 0..64 {
            match self.new.def(sym) {
                Def::Special(at) => return self.new.specials.get(at as usize),
                Def::Alias(to) => sym = to,
                _ => return None,
            }
        }
        None
    }

    /// Whether a fixup reaches its target at the target's PLT stub: a branch to an import,
    /// and where imports are reached only through their GOT cells (PE), any reference to an
    /// imported function that is not through its cell, as link.exe's import thunk.
    pub fn through_plt(&self, fixup: &Fixup) -> bool {
        let Some(at) = self.import_of(fixup.target) else { return false };
        if reloc::is_branch(fixup.apply) {
            return true;
        }
        self.plan.got_by_library
            && matches!(fixup.apply, Apply::Abs64 | Apply::ImageRel32 | Apply::Rel32)
            && self.new.resolved.imports.get(at as usize).is_some_and(|it| it.kind == crate::model::SymbolKind::Function)
    }

    /// Records what codegen says names are imported as on wasm; the map is copied only when
    /// one is new or different.
    fn wasm_imports(&mut self, imports: &[(String, blaze_codegen::wasm::Import)]) {
        for (name, import) in imports {
            let name = self.symbols.name(name);
            if self.new.wasm_imports.get(&name) != Some(import) {
                Arc::make_mut(&mut self.new.wasm_imports).insert(name, import.clone());
            }
        }
    }

    /// A GOT cell's fixups: the core writes what it can; an import's cells are the loader's.
    fn got_fixups(&self, kind: GotKind, target: Sym) -> &'a [Fixup] {
        if self.imported(target) {
            return &[];
        }
        let fixup = |at, apply| Fixup { at, apply, target, addend: 0, minus: None };
        match kind {
            GotKind::Address => self.fixup_slice([fixup(0, Apply::Abs64)]),
            GotKind::TpOff => self.fixup_slice([fixup(0, Apply::TpOff64)]),
            GotKind::TlsGd => self.fixup_slice([fixup(8, Apply::DtpOff64)]),
            GotKind::TlsDesc => &[],
        }
    }

    /// Rewrites every piece referring to a symbol whose definition changed, and every cell
    /// running a body that moved. Costs the changed symbols' referrers.
    pub fn referrers(&mut self) {
        for sym in self.changed.iter() {
            for id in self.new.refs(sym) {
                self.write.insert(id);
            }
            for slot in self.new.slots_of(sym) {
                self.dirty_cells.insert(slot);
            }
        }
        if !self.dirty_cells.is_empty() {
            if let Some(cells) = self.cells_piece() {
                self.write.insert(cells);
            }
        }
    }

    /// Fails when a removed function or static is still referred to, directly or through
    /// its slot.
    pub fn dangling(&self) -> Result<(), LinkError> {
        for slot in &self.emptied {
            if self.new.cell(*slot).is_some() {
                continue;
            }
            let Some(slot_sym) = self.new.slot_syms.get(*slot as usize).copied() else { continue };
            if let Some(caller) = self.new.refs(slot_sym).find_map(|it| self.new.piece(it)) {
                return Err(error(
                    function_of(caller),
                    format!(
                        "{} is removed with nothing put in its slot, and {} still calls it",
                        self.symbols.describe(slot_sym),
                        self.symbols.describe(caller.sym),
                    ),
                ));
            }
        }
        for id in &self.removed {
            let piece = self.new.any(*id).expect("a removed piece");
            for sym in std::iter::once(piece.sym).chain(piece.defines().iter().copied()) {
                if self.new.home(sym).is_some() || !matches!(self.new.def(sym), Def::None) {
                    continue;
                }
                if let Some(user) = self.new.refs(sym).find_map(|it| self.new.piece(it)) {
                    return Err(error(
                        function_of(user),
                        format!(
                            "{} is removed, and {} still refers to it",
                            self.symbols.describe(sym),
                            self.symbols.describe(user.sym),
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn function_of(piece: PieceRef<'_>) -> Option<InstanceKey> {
    piece.function().map(|it| it.key)
}

/// Reads a LEB128 at `*at`, moving past it.
fn uleb(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("wasm: a truncated code header")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("wasm: a LEB128 past 64 bits in a code header".to_owned())
}

/// Moves `*at` past a function type (`0x60`, the parameters, the results).
fn skip_signature(bytes: &[u8], at: &mut usize) -> Result<(), String> {
    if bytes.get(*at) != Some(&0x60) {
        return Err("wasm: a type that is not a function type".to_owned());
    }
    *at += 1;
    for _ in 0..2 {
        let n = usize::try_from(uleb(bytes, at)?).map_err(|_| "wasm: a truncated code header")?;
        *at = at.checked_add(n).filter(|it| *it <= bytes.len()).ok_or("wasm: a truncated code header")?;
    }
    Ok(())
}

/// Whether a wasm function's code header ([`blaze_codegen::wasm::Code`]) lists imports:
/// read past its type, types, type fixups and stack pointer fixups without decoding them,
/// so a function without imports allocates nothing.
fn wasm_imports_in(code: &[u8]) -> Result<bool, String> {
    let mut at = 0;
    let len = usize::try_from(uleb(code, &mut at)?).map_err(|_| "wasm: a truncated code header")?;
    let end = at.checked_add(len).filter(|it| *it <= code.len()).ok_or("wasm: a truncated code header")?;
    let header = &code[..end];
    skip_signature(header, &mut at)?;
    for _ in 0..uleb(header, &mut at)? {
        skip_signature(header, &mut at)?;
    }
    for _ in 0..uleb(header, &mut at)? {
        uleb(header, &mut at)?;
        uleb(header, &mut at)?;
    }
    for _ in 0..uleb(header, &mut at)? {
        uleb(header, &mut at)?;
    }
    Ok(uleb(header, &mut at)? > 0)
}

#[cfg(test)]
mod tests {
    use blaze_codegen::wasm::{Code, Import, Signature, ValType};

    use super::*;

    /// The header scan agrees with the full decode on whether there are imports.
    #[test]
    fn the_wasm_header_scan_finds_imports_as_the_decoder_does() {
        let sig = Signature { params: vec![ValType::I32, ValType::I64], results: vec![ValType::F64] };
        let bare = Code {
            ty: sig.clone(),
            types: vec![sig.clone(), Signature::default()],
            type_fixups: vec![(3, 0), (300, 1)],
            stack_pointer: vec![1, 70_000],
            imports: Vec::new(),
            export: Some("f".to_owned()),
        };
        let with = Code {
            imports: vec![("puts".to_owned(), Import { module: "env".to_owned(), field: "puts".to_owned(), ty: sig })],
            ..bare.clone()
        };
        for code in [&bare, &with] {
            let (bytes, _) = code.encode(&[0x0b]);
            let (decoded, _) = Code::decode(&bytes).unwrap();
            assert_eq!(wasm_imports_in(&bytes), Ok(!decoded.imports.is_empty()));
            for cut in 0..bytes.len() - 1 {
                let _ = wasm_imports_in(&bytes[..cut]);
            }
        }
    }

    #[test]
    fn contents_are_dense_for_fresh_pieces_and_mapped_for_others() {
        let mut contents = Contents::new(10);
        contents.insert(12, Content::Zero);
        contents.insert(3, Content::Bytes(b"abc"));
        assert!(matches!(contents.get(12), Some(Content::Zero)));
        assert!(contents.get(11).is_none());
        assert_eq!(contents.get(3).and_then(Content::bytes), Some(&b"abc"[..]));
        contents.remove(12);
        contents.remove(3);
        assert!(!contents.contains(12) && !contents.contains(3));
        let mut out = [9u8; 5];
        Content::Bytes(b"ab").copy_to(&mut out);
        assert_eq!(out, [b'a', b'b', 0, 0, 0]);
    }
}
