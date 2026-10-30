//! A stage: a [`Change`] applied to a copy of the image's state, placed and relocated.
//!
//! The order is fixed: what is removed goes first, then what is added is made into pieces
//! (in parallel, interning its symbols shard by shard), the native inputs and every name
//! are resolved ([`crate::native`]), GOT cells and PLT stubs made for what needs them, the
//! shared pieces counted and the unused swept, everything placed ([`crate::place`]), and
//! every piece that is new or refers to something that changed relocated, in parallel
//! ([`crate::write`]).

use std::borrow::Cow;
use std::sync::Arc;

use blaze_codegen::{Arch, CompiledFunction, Data, Init, InstanceKey, Reloc, Static, Target, Unwind};
use blaze_ids::Hasher;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::model::{Apply, Class, Fixup, Object, Plan, Sym, Writer};
use crate::position::{self, SpanTable};
use crate::reloc::{self, GotKind};
use crate::state::{Def, FunctionInfo, Piece, PieceId, State, What};
use crate::symbols::{Obj, SymKey, Symbols};
use crate::table;
use crate::unwind::{self, Field};
use crate::{Change, LinkError, Output};

const CIE_DOMAIN: &str = "blaze v2 link cie 1";

pub(crate) struct Stage<'a> {
    pub output: &'a Output,
    pub writer: &'a dyn Writer,
    pub plan: &'a Plan,
    pub symbols: &'a mut Symbols,
    pub objects: &'a mut FxHashMap<Obj, Arc<Object>>,
    pub old: &'a State,
    pub new: State,
    /// The image's file name.
    pub name: &'a str,
    /// The contents of pieces this stage made or rewrote.
    pub bytes: FxHashMap<PieceId, Cow<'a, [u8]>>,
    /// Pieces this stage made.
    pub fresh: FxHashSet<PieceId>,
    /// Pieces to relocate (with [`Stage::bytes`] or their bytes where they were).
    pub write: FxHashSet<PieceId>,
    /// Symbols whose definition changed: what refers to them is relocated again.
    pub changed: FxHashSet<Sym>,
    /// Pieces removed this stage, and what their fixups referred to.
    pub removed: Vec<(PieceId, Piece)>,
    /// Shared pieces that lost a user outside [`Stage::removed`], and symbols that gained
    /// one outside [`Stage::fresh`]: [`Stage::sweep`] counts them.
    pub touched: Vec<PieceId>,
    pub counted: Vec<Sym>,
    /// Whether everything is placed anew.
    pub full: bool,
    pub table: bool,
    pub functions: u64,
    pub statics: u64,
}

pub(crate) fn error(function: Option<InstanceKey>, what: impl Into<String>) -> LinkError {
    LinkError { function, what: what.into() }
}

impl<'a> Stage<'a> {
    pub fn sym(&mut self, key: SymKey) -> Sym {
        self.symbols.intern(key)
    }

    /// Defines `sym`, noting a change.
    pub fn define(&mut self, sym: Sym, def: Def) {
        if self.new.def(sym) != def {
            self.changed.insert(sym);
            self.new.set_def(sym, def);
        }
    }

    /// Adds a piece with its contents, defining its own symbol at its start.
    pub fn make(&mut self, piece: Piece, bytes: Cow<'a, [u8]>) -> PieceId {
        let sym = piece.sym;
        let id = self.new.add(piece);
        self.bytes.insert(id, bytes);
        self.fresh.insert(id);
        self.write.insert(id);
        self.define(sym, Def::Piece { piece: id, offset: 0 });
        id
    }

    /// Removes a piece, keeping what it referred to for the shared pieces' counts.
    pub fn unmake(&mut self, id: PieceId) {
        let Some(piece) = self.new.remove(id) else { return };
        for sym in std::iter::once(piece.sym).chain(piece.defines.iter().copied()) {
            self.changed.insert(sym);
        }
        self.fresh.remove(&id);
        self.write.remove(&id);
        self.bytes.remove(&id);
        self.removed.push((id, piece));
    }

    pub fn piece(
        &self,
        sym: Sym,
        class: Class,
        align: u64,
        size: u64,
        fixups: Vec<Fixup>,
        what: What,
    ) -> Piece {
        Piece {
            sym,
            class,
            rank: 0,
            order: [u32::MAX; 3],
            align: align.max(1),
            size,
            zero: false,
            region: u32::MAX,
            addr: 0,
            fixups: fixups.into(),
            weak: Arc::new([]),
            defines: Arc::new([]),
            what,
            rebase: Arc::new([]),
            binds: Arc::new([]),
        }
    }

    pub fn arch(&self) -> Arch {
        self.output.arch
    }

    /// The symbol a codegen target is, and whether it is referred to weakly.
    fn target_key(&self, target: &Target) -> (SymKey, bool) {
        match target {
            Target::Function(key) if self.table => (SymKey::Slot(*key), false),
            Target::Function(key) | Target::Direct(key) => (SymKey::Body(*key), false),
            Target::Static { item, instance } => (SymKey::Static(*item, *instance), false),
            Target::Data(id) => (SymKey::Data(*id), false),
            Target::Extern(name) => (SymKey::Name(name.as_str().into()), false),
            Target::Weak(name) => (SymKey::Name(name.as_str().into()), true),
            Target::Position(span) => (SymKey::Position(*span), false),
        }
    }

    /// Applies everything `change` says to the copy of the state.
    pub fn apply(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        self.spans(change)?;
        if self.table && !self.new.laid_out {
            self.fixed(change.added.len() as u64)?;
        }
        for key in &change.removed {
            if let Some(sym) = self.symbols.find(&SymKey::Body(*key)) {
                if let Some((piece, _)) = self.new.home(sym) {
                    self.remove_function(piece);
                }
            }
        }
        for (item, instance) in &change.removed_statics {
            for key in [SymKey::Static(*item, *instance), SymKey::Tlv(*item, *instance)] {
                if let Some(sym) = self.symbols.find(&key) {
                    if let Def::Piece { piece, .. } = self.new.def(sym) {
                        self.unmake(piece);
                    }
                }
            }
        }
        if !change.debug_names.is_empty() {
            let names = Arc::make_mut(&mut self.new.debug_names);
            for (key, name) in &change.debug_names {
                names.insert(*key, name.as_str().into());
            }
        }
        self.functions(&change.added)?;
        for it in &change.statics {
            self.add_static(it)?;
        }
        let names = Arc::make_mut(&mut self.new.names);
        for name in &change.unnamed {
            names.remove(name);
        }
        let mut named = Vec::new();
        for (name, target) in &change.names {
            named.push((name.clone(), self.target_key(target).0));
        }
        for (name, key) in named {
            let sym = self.sym(key);
            Arc::make_mut(&mut self.new.names).insert(name, sym);
        }
        self.positions()?;
        let mut seen = Vec::new();
        for id in &self.fresh {
            let piece = self.new.piece(*id).expect("a fresh piece");
            for fixup in piece.fixups.iter() {
                if matches!(self.symbols.key(fixup.target), SymKey::Name(_)) {
                    seen.push(fixup.target);
                }
            }
        }
        for sym in self.new.names.keys().map(|it| SymKey::Name(it.as_str().into())).collect::<Vec<_>>() {
            seen.push(self.sym(sym));
        }
        if seen.iter().any(|it| !self.new.seen.contains(it)) {
            Arc::make_mut(&mut self.new.seen).extend(seen);
        }
        Ok(())
    }

    /// The live descriptor and the path it names, filled once the image is placed.
    pub fn descriptor(&mut self) {
        let Some(live) = &self.output.live else { return };
        let path = live.path.clone().into_bytes();
        let dir = self.sym(SymKey::LiveDir);
        if self.new.home(dir).is_none() {
            let piece = self.piece(dir, Class::Rodata, 1, path.len() as u64, Vec::new(), What::Fixed);
            self.make(piece, Cow::Owned(path));
        }
        let descriptor = self.sym(SymKey::Descriptor);
        if self.new.home(descriptor).is_none() {
            let size = blaze_live::patch::Descriptor::SIZE as u64;
            let piece = self.piece(descriptor, Class::Rodata, 8, size, Vec::new(), What::Fixed);
            self.make(piece, Cow::Owned(vec![0; size as usize]));
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
        let piece = self.piece(
            generation,
            Class::Table,
            8,
            8,
            vec![Fixup { at: 0, apply: Apply::Abs64, target: cells, addend: 0, minus: None }],
            What::Fixed,
        );
        self.make(piece, Cow::Owned(vec![0; 8]));
        let mut piece = self.piece(cells, Class::Table, 8, u64::from(capacity) * 8, Vec::new(), What::Fixed);
        piece.rank = 1;
        self.make(piece, Cow::Owned(vec![0; capacity as usize * 8]));
        let size = table::stub_size(self.arch());
        let mut code = Vec::with_capacity((u64::from(capacity) * size) as usize);
        let mut fixups = Vec::new();
        for slot in 0..capacity {
            let (bytes, stub) = table::stub(self.arch(), slot);
            let at = code.len() as u64;
            code.extend_from_slice(&bytes);
            fixups.extend(stub.into_iter().map(|it| Fixup {
                at: at + it.at,
                apply: it.apply,
                target: generation,
                addend: it.addend,
                minus: None,
            }));
        }
        let stubs = self.sym(SymKey::Stubs);
        let piece = self.piece(stubs, Class::Stubs, 16, code.len() as u64, fixups, What::Fixed);
        self.make(piece, Cow::Owned(code));
        Ok(())
    }

    fn remove_function(&mut self, piece: PieceId) {
        let Some(What::Function(info)) = self.new.piece(piece).map(|it| it.what.clone()) else {
            return;
        };
        self.unmake(piece);
        for sym in info.fde.into_iter().chain(info.windows.map(|it| it.0)) {
            if let Def::Piece { piece, .. } = self.new.def(sym) {
                self.unmake(piece);
            }
        }
        if self.table {
            if let Some(slot) = self.symbols.find(&SymKey::Slot(info.key)) {
                self.cell(slot, None);
            }
        }
    }

    /// Points a slot's cell at a body, or leaves it as it is (`None`).
    fn cell(&mut self, slot: Sym, body: Option<Sym>) {
        let Some((_, offset)) = self.new.home(slot) else { return };
        let index = offset / table::stub_size(self.arch());
        let cells = self.symbols.find(&SymKey::Cells).expect("the table was made");
        let Def::Piece { piece, .. } = self.new.def(cells) else { return };
        let at = index * 8;
        let mut fixups: Vec<Fixup> =
            self.new.piece(piece).expect("the cells").fixups.iter().filter(|it| it.at != at).copied().collect();
        if let Some(body) = body {
            fixups.push(Fixup { at, apply: Apply::Abs64, target: body, addend: 0, minus: None });
            fixups.sort_by_key(|it| it.at);
        }
        self.new.set_fixups(piece, fixups.into());
        self.write.insert(piece);
    }

    /// The functions added, made into pieces: their symbols interned in parallel.
    fn functions(&mut self, added: &[&'a CompiledFunction]) -> Result<(), LinkError> {
        let mut seen = FxHashSet::default();
        let mut todo = Vec::new();
        for function in added {
            if function.arch != self.arch() {
                return Err(error(
                    Some(function.key),
                    format!("compiled for {:?}, linked into an image for {:?}", function.arch, self.arch()),
                ));
            }
            if !seen.insert(function.key) {
                return Err(error(Some(function.key), "added twice in one change"));
            }
            let body = self.symbols.find(&SymKey::Body(function.key));
            if body.and_then(|it| self.new.home(it)).is_some() {
                continue;
            }
            todo.push(*function);
        }
        let keys: Vec<Vec<SymKey>> = todo
            .par_iter()
            .map(|function| {
                let mut keys = vec![SymKey::Body(function.key)];
                keys.extend(function.relocs.iter().map(|it| self.target_key(&it.target).0));
                for data in &function.data {
                    keys.push(SymKey::Data(data.id));
                    keys.extend(data.relocs.iter().map(|it| self.target_key(&it.target).0));
                }
                keys
            })
            .collect();
        let counts: Vec<usize> = keys.iter().map(Vec::len).collect();
        let syms = self.symbols.intern_all(keys.into_iter().flatten().collect());
        let mut at = 0;
        for (function, count) in todo.iter().zip(counts) {
            let syms = &syms[at..at + count];
            at += count;
            self.add_function(function, syms)?;
        }
        self.functions += todo.len() as u64;
        Ok(())
    }

    fn fixups(
        &self,
        relocs: &[Reloc],
        syms: &[Sym],
        function: Option<InstanceKey>,
    ) -> Result<(Vec<Fixup>, Vec<Sym>), LinkError> {
        let mut fixups = Vec::with_capacity(relocs.len());
        let mut weak = Vec::new();
        for (reloc, sym) in relocs.iter().zip(syms) {
            let apply = reloc::of_kind(reloc.kind, self.arch()).map_err(|it| error(function, it))?;
            if self.target_key(&reloc.target).1 {
                weak.push(*sym);
            }
            fixups.push(Fixup { at: u64::from(reloc.at), apply, target: *sym, addend: reloc.addend, minus: None });
        }
        weak.sort_unstable();
        weak.dedup();
        Ok((fixups, weak))
    }

    fn add_function(&mut self, function: &'a CompiledFunction, syms: &[Sym]) -> Result<(), LinkError> {
        let key = Some(function.key);
        let body = syms[0];
        let (fixups, weak) = self.fixups(&function.relocs, &syms[1..=function.relocs.len()], key)?;
        let mut at = 1 + function.relocs.len();
        for data in &function.data {
            let sym = syms[at];
            let relocs = &syms[at + 1..at + 1 + data.relocs.len()];
            at += 1 + data.relocs.len();
            self.add_data(data, sym, relocs, key)?;
        }
        let mut info = FunctionInfo { key: function.key, fde: None, windows: None, lines: function.lines.clone() };
        match &function.unwind {
            Some(Unwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
                let personality = personality.as_ref().map(|it| self.target_key(it).0).map(|it| self.sym(it));
                let mut hasher = Hasher::new(CIE_DOMAIN);
                hasher
                    .part(&code_align.to_le_bytes())
                    .part(&data_align.to_le_bytes())
                    .part(&return_address.to_le_bytes())
                    .part(cie);
                if let Some(personality) = personality {
                    hasher.part(format!("{:?}", self.symbols.key(personality)).as_bytes());
                }
                let cie_sym = self.sym(SymKey::Cie(hasher.finish()));
                if self.new.home(cie_sym).is_none() {
                    let record = unwind::cie(*code_align, *data_align, *return_address, cie, personality.is_some())
                        .map_err(|it| error(key, it))?;
                    let fixups = record
                        .fixups
                        .iter()
                        .map(|(at, _)| Fixup {
                            at: *at,
                            apply: Apply::GotRel32,
                            target: personality.expect("a personality field"),
                            addend: 0,
                            minus: None,
                        })
                        .collect();
                    let piece = self.piece(cie_sym, Class::EhFrame, 4, record.bytes.len() as u64, fixups, What::Shared);
                    self.make(piece, Cow::Owned(record.bytes));
                }
                let lsda = match lsda {
                    Some(id) => {
                        if !function.data.iter().any(|it| it.id == *id) {
                            return Err(error(key, "its LSDA is not one of its data"));
                        }
                        Some(self.sym(SymKey::Data(*id)))
                    }
                    None => None,
                };
                let record = unwind::fde(function.code.len() as u64, fde, personality.is_some(), lsda.is_some())
                    .map_err(|it| error(key, it))?;
                let fde_sym = self.sym(SymKey::Fde(function.key));
                let fixups = record
                    .fixups
                    .iter()
                    .map(|(at, field)| match field {
                        Field::CiePointer => {
                            Fixup { at: *at, apply: Apply::Delta32, target: fde_sym, addend: *at as i64, minus: Some(cie_sym) }
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
                    })
                    .collect();
                let mut piece = self.piece(fde_sym, Class::EhFrame, 4, record.bytes.len() as u64, fixups, What::Fde);
                piece.rank = 1;
                self.make(piece, Cow::Owned(record.bytes));
                info.fde = Some(fde_sym);
            }
            Some(Unwind::Windows { info: bytes, handler, lsda }) => {
                let handler = handler.as_ref().map(|it| self.target_key(it).0).map(|it| self.sym(it));
                let lsda = lsda.map(|id| self.sym(SymKey::Data(id)));
                let mut bytes = bytes.clone();
                while bytes.len() % 4 != 0 {
                    bytes.push(0);
                }
                let mut fixups = Vec::new();
                for target in handler.into_iter().chain(lsda) {
                    fixups.push(Fixup {
                        at: bytes.len() as u64,
                        apply: Apply::ImageRel32,
                        target,
                        addend: 0,
                        minus: None,
                    });
                    bytes.extend_from_slice(&[0; 4]);
                }
                let sym = self.sym(SymKey::WinInfo(function.key));
                let piece = self.piece(sym, Class::Rodata, 4, bytes.len() as u64, fixups, What::WinInfo);
                self.make(piece, Cow::Owned(bytes));
                info.windows = Some((sym, handler, lsda));
            }
            None => {}
        }
        let mut piece = self.piece(
            body,
            Class::Text,
            u64::from(function.align),
            function.code.len() as u64,
            fixups,
            What::Function(Arc::new(info)),
        );
        piece.weak = weak.into();
        self.make(piece, Cow::Borrowed(&function.code));
        if self.table {
            let slot = self.sym(SymKey::Slot(function.key));
            if self.new.home(slot).is_none() {
                self.new_slot(slot, function.key)?;
            }
            self.cell(slot, Some(body));
        }
        Ok(())
    }

    fn new_slot(&mut self, slot: Sym, function: InstanceKey) -> Result<(), LinkError> {
        if self.new.slots >= self.new.capacity {
            return Err(error(
                Some(function),
                format!(
                    "the jump table is full ({} slots): link the image from scratch",
                    self.new.capacity
                ),
            ));
        }
        let index = self.new.slots;
        self.new.slots += 1;
        let stubs = self.symbols.find(&SymKey::Stubs).expect("the table was made");
        let Def::Piece { piece, .. } = self.new.def(stubs) else {
            return Err(error(Some(function), "the jump table's stubs are gone"));
        };
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
        let (fixups, weak) = self.fixups(&data.relocs, relocs, function)?;
        let class = if fixups.is_empty() { Class::Rodata } else { Class::RelRo };
        let mut piece =
            self.piece(sym, class, u64::from(data.align), data.bytes.len() as u64, fixups, What::Shared);
        piece.weak = weak.into();
        self.make(piece, Cow::Borrowed(&data.bytes));
        Ok(())
    }

    fn add_static(&mut self, it: &'a Static) -> Result<(), LinkError> {
        let sym = self.sym(SymKey::Static(it.item, it.instance));
        if let Def::Piece { piece, .. } = self.new.def(sym) {
            self.unmake(piece);
        }
        for data in &it.data {
            let data_sym = self.sym(SymKey::Data(data.id));
            let keys: Vec<SymKey> = data.relocs.iter().map(|r| self.target_key(&r.target).0).collect();
            let relocs = self.symbols.intern_all(keys);
            self.add_data(data, data_sym, &relocs, None)?;
        }
        let (bytes, relocs, size, zero): (Cow<'a, [u8]>, &[Reloc], u64, bool) = match &it.init {
            Init::Bytes { bytes, relocs } => (Cow::Borrowed(bytes), relocs, bytes.len() as u64, false),
            Init::Zeroed { size } => (Cow::Owned(Vec::new()), &[], *size, true),
        };
        let keys: Vec<SymKey> = relocs.iter().map(|r| self.target_key(&r.target).0).collect();
        let syms = self.symbols.intern_all(keys);
        let (fixups, weak) = self.fixups(relocs, &syms, None)?;
        let static_error = |what: String| error(None, format!("the static {:?}: {what}", it.item));
        let (class, rank) = match &it.section {
            Some(name) => self
                .writer
                .section(name, it.mutable || !fixups.is_empty(), false, it.thread_local)
                .map_err(static_error)?,
            None => (
                match (it.thread_local, it.mutable, zero, fixups.is_empty()) {
                    (true, _, false, _) => Class::Tdata,
                    (true, _, true, _) => Class::Tbss,
                    (false, true, true, _) => Class::Bss,
                    (false, true, false, _) => Class::Data,
                    (false, false, _, true) => Class::Rodata,
                    (false, false, _, false) => Class::RelRo,
                },
                0,
            ),
        };
        let zeroed = crate::state::zeroed(&class);
        let bytes = match (zero, zeroed) {
            (true, false) => Cow::Owned(vec![0; size as usize]),
            (false, true) => {
                return Err(static_error(format!("initialized bytes in the zeroed section {class:?}")));
            }
            _ => bytes,
        };
        let mut piece = self.piece(
            sym,
            class,
            u64::from(it.align),
            size,
            fixups,
            What::Static { thread_local: it.thread_local },
        );
        piece.rank = rank;
        piece.zero = zeroed;
        piece.weak = weak.into();
        self.make(piece, bytes);
        if it.thread_local && self.plan.tls_descriptors {
            let tlv = self.sym(SymKey::Tlv(it.item, it.instance));
            if let Def::Piece { piece, .. } = self.new.def(tlv) {
                self.unmake(piece);
            }
            let bootstrap = self.sym(SymKey::Name("_tlv_bootstrap".into()));
            let fixups = vec![
                Fixup { at: 0, apply: Apply::Abs64, target: bootstrap, addend: 0, minus: None },
                Fixup { at: 16, apply: Apply::DtpOff64, target: sym, addend: 0, minus: None },
            ];
            let piece = self.piece(tlv, Class::ThreadVars, 8, 24, fixups, What::Tlv);
            self.make(piece, Cow::Owned(vec![0; 24]));
        }
        self.statics += 1;
        Ok(())
    }

    /// The span tables the change carries, decoded; the panic locations of a changed one
    /// rewritten in place.
    fn spans(&mut self, change: &Change<'a>) -> Result<(), LinkError> {
        if change.spans.is_empty() {
            return Ok(());
        }
        let mut changed = FxHashSet::default();
        for (anchor, bytes) in &change.spans {
            let table = SpanTable::decode(bytes)
                .map_err(|it| error(None, format!("the span table of {anchor:?}: {it}")))?;
            if self.new.spans.get(anchor).is_some_and(|it| **it == table) {
                continue;
            }
            Arc::make_mut(&mut self.new.spans).insert(*anchor, Arc::new(table));
            changed.insert(*anchor);
        }
        if changed.is_empty() {
            return Ok(());
        }
        let locations: Vec<(PieceId, blaze_format::SpanRef)> = self
            .new
            .live_pieces()
            .filter_map(|(id, piece)| match self.symbols.key(piece.sym) {
                SymKey::Position(span) if changed.contains(&span.anchor) => Some((id, *span)),
                _ => None,
            })
            .collect();
        for (id, span) in locations {
            let (bytes, file) = self.location(span)?;
            let fixups = vec![Fixup { at: 0, apply: Apply::Abs64, target: file, addend: 0, minus: None }];
            self.retarget(id, fixups);
            self.bytes.insert(id, Cow::Owned(bytes));
            self.write.insert(id);
        }
        Ok(())
    }

    /// Replaces the fixups of a piece that was there before the stage, counting the shared
    /// pieces it stops and starts referring to.
    fn retarget(&mut self, id: PieceId, fixups: Vec<Fixup>) {
        let before = State::targets(self.new.piece(id).expect("a live piece"));
        self.new.set_fixups(id, fixups.into());
        let after = State::targets(self.new.piece(id).expect("a live piece"));
        for target in before.iter().filter(|it| !after.contains(it)) {
            if let Some(shared) = self.shared(*target) {
                let users = self.new.users.get_mut(shared as usize).expect("a piece's count");
                *users = users.saturating_sub(1);
                self.touched.push(shared);
            }
        }
        for target in after.iter().filter(|it| !before.contains(it)) {
            self.counted.push(*target);
        }
    }

    /// A span's `Location` record and the symbol of its file name, made when missing.
    fn location(&mut self, span: blaze_format::SpanRef) -> Result<(Vec<u8>, Sym), LinkError> {
        let table = self.new.spans.get(&span.anchor).cloned().ok_or_else(|| {
            error(None, format!("a position in {:?}, whose span table the link was not given", span.anchor))
        })?;
        let at = table.spans.get(span.index as usize).ok_or_else(|| {
            error(None, format!("span {} of {:?}, whose table has {}", span.index, span.anchor, table.spans.len()))
        })?;
        let name = &table.files[at.file as usize];
        let file = self.sym(SymKey::FileName(name.as_str().into()));
        if self.new.home(file).is_none() {
            let bytes = position::file_name(name);
            let piece = self.piece(file, Class::Rodata, 1, bytes.len() as u64, Vec::new(), What::Shared);
            self.make(piece, Cow::Owned(bytes));
        }
        Ok((position::location(name.len(), *at), file))
    }

    /// The `Location` records the fresh pieces refer to and that don't exist yet.
    fn positions(&mut self) -> Result<(), LinkError> {
        let mut wanted = Vec::new();
        for id in &self.fresh {
            let piece = self.new.piece(*id).expect("a fresh piece");
            for fixup in piece.fixups.iter() {
                if let SymKey::Position(span) = self.symbols.key(fixup.target) {
                    wanted.push((fixup.target, *span));
                }
            }
        }
        wanted.sort_unstable_by_key(|it| it.0);
        wanted.dedup();
        for (sym, span) in wanted {
            if self.new.home(sym).is_some() {
                continue;
            }
            let (bytes, file) = self.location(span)?;
            let fixups = vec![Fixup { at: 0, apply: Apply::Abs64, target: file, addend: 0, minus: None }];
            let piece = self.piece(
                sym,
                Class::RelRo,
                position::LOCATION_ALIGN,
                position::LOCATION_SIZE,
                fixups,
                What::Shared,
            );
            self.make(piece, Cow::Owned(bytes));
        }
        Ok(())
    }

    /// Counts the fixups to shared pieces that this stage added and removed, and removes the
    /// shared pieces nothing refers to any more.
    pub fn sweep(&mut self) {
        let mut touched = std::mem::take(&mut self.touched);
        let mut fresh: Vec<PieceId> = self.fresh.iter().copied().collect();
        fresh.sort_unstable();
        let mut targets: Vec<Sym> = std::mem::take(&mut self.counted);
        for id in &fresh {
            let piece = self.new.piece(*id).expect("a fresh piece");
            targets.extend(State::targets(piece));
            if matches!(piece.what, What::Shared) {
                touched.push(*id);
            }
        }
        for target in targets {
            if let Some(shared) = self.shared(target) {
                *self.new.users.get_mut(shared as usize).expect("a piece's count") += 1;
            }
        }
        let counted: FxHashSet<PieceId> = fresh.into_iter().collect();
        let mut at = 0;
        while at < self.removed.len() || !touched.is_empty() {
            while at < self.removed.len() {
                let (id, piece) = &self.removed[at];
                at += 1;
                if self.old.piece(*id).is_none() && !counted.contains(id) {
                    continue;
                }
                for target in State::targets(piece) {
                    if let Some(shared) = self.shared(target).or_else(|| self.old_shared(target)) {
                        if let Some(users) = self.new.users.get_mut(shared as usize) {
                            *users = users.saturating_sub(1);
                        }
                        touched.push(shared);
                    }
                }
            }
            touched.sort_unstable();
            touched.dedup();
            for id in std::mem::take(&mut touched) {
                let live = self.new.piece(id).is_some_and(|it| matches!(it.what, What::Shared));
                if live && self.new.users.get(id as usize) == Some(&0) {
                    self.unmake(id);
                }
            }
        }
    }

    /// The shared piece a symbol is at the start of, in the new state.
    fn shared(&self, sym: Sym) -> Option<PieceId> {
        match self.new.def(sym) {
            Def::Piece { piece, offset: 0 } => {
                matches!(self.new.piece(piece)?.what, What::Shared).then_some(piece)
            }
            _ => None,
        }
    }

    /// The same, as it was before the stage: a removed piece's references were counted then.
    fn old_shared(&self, sym: Sym) -> Option<PieceId> {
        match self.old.def(sym) {
            Def::Piece { piece, offset: 0 } => {
                matches!(self.old.piece(piece)?.what, What::Shared).then_some(piece)
            }
            _ => None,
        }
    }

    /// The GOT cells and PLT stubs the pieces to write need, made when missing.
    pub fn needs(&mut self) -> Result<(), LinkError> {
        let mut cells: Vec<(GotKind, Sym)> = Vec::new();
        let mut plt: Vec<Sym> = Vec::new();
        for id in &self.write {
            let Some(piece) = self.new.piece(*id) else { continue };
            for fixup in piece.fixups.iter() {
                if let Some(kind) = reloc::got_kind(fixup.apply) {
                    cells.push((kind, self.got_target(fixup)));
                } else if reloc::is_branch(fixup.apply) && self.imported(fixup.target) {
                    plt.push(fixup.target);
                }
            }
        }
        plt.sort_unstable();
        plt.dedup();
        for target in &plt {
            cells.push((GotKind::Address, *target));
        }
        cells.sort_unstable();
        cells.dedup();
        for (kind, target) in cells {
            let key = SymKey::Got(kind, Box::new(self.symbols.key(target).clone()));
            let sym = self.sym(key);
            if self.new.home(sym).is_some() {
                continue;
            }
            let size = kind.cells() * 8;
            let fixups = self.got_fixups(kind, target);
            let mut piece = self.piece(sym, Class::Got, 8, size, fixups, What::Got { kind, target });
            piece.order = [u32::MAX, 0, 0];
            self.make(piece, Cow::Owned(vec![0; size as usize]));
        }
        for target in plt {
            let key = SymKey::Plt(Box::new(self.symbols.key(target).clone()));
            let sym = self.sym(key);
            if self.new.home(sym).is_some() {
                continue;
            }
            let (code, stub) = table::plt(self.arch());
            let fixups = stub
                .into_iter()
                .map(|it| Fixup { at: it.at, apply: it.apply, target, addend: it.addend, minus: None })
                .map(|mut it| {
                    it.apply = match it.apply {
                        Apply::Rel32 => Apply::X86GotPcRel32,
                        Apply::A64Page21 => Apply::A64GotPage21,
                        Apply::A64Lo12 { .. } => Apply::A64GotLo12,
                        other => other,
                    };
                    it
                })
                .collect();
            let piece = self.piece(sym, Class::Plt, 16, code.len() as u64, fixups, What::Plt { target });
            self.make(piece, Cow::Owned(code));
        }
        Ok(())
    }

    /// What a GOT cell of a fixup holds the address of: a thread-local static's TLV
    /// descriptor for the TLV kinds, else the target.
    pub fn got_target(&self, fixup: &Fixup) -> Sym {
        if matches!(fixup.apply, Apply::TlvPage21 | Apply::TlvLo12 | Apply::TlvRel32) {
            if let SymKey::Static(item, instance) = self.symbols.key(fixup.target) {
                if let Some(tlv) = self.symbols.find(&SymKey::Tlv(*item, *instance)) {
                    return tlv;
                }
            }
        }
        fixup.target
    }

    fn imported(&self, mut sym: Sym) -> bool {
        for _ in 0..64 {
            match self.new.def(sym) {
                Def::Import(_) => return true,
                Def::Alias(to) => sym = to,
                _ => return false,
            }
        }
        false
    }

    /// A GOT cell's fixups: the core writes what it can; an import's cells are the loader's.
    fn got_fixups(&self, kind: GotKind, target: Sym) -> Vec<Fixup> {
        if self.imported(target) {
            return Vec::new();
        }
        let fixup = |at, apply| Fixup { at, apply, target, addend: 0, minus: None };
        match kind {
            GotKind::Address => vec![fixup(0, Apply::Abs64)],
            GotKind::TpOff => vec![fixup(0, Apply::TpOff64)],
            GotKind::TlsGd => vec![fixup(8, Apply::DtpOff64)],
            GotKind::TlsDesc => Vec::new(),
        }
    }

    /// Rewrites every piece referring to a symbol whose definition changed.
    pub fn referrers(&mut self) {
        let changed: Vec<Sym> = self.changed.iter().copied().collect();
        for sym in changed {
            for id in self.new.refs(sym).to_vec() {
                if self.new.piece(id).is_some() {
                    self.write.insert(id);
                }
            }
        }
    }

    /// Fails when a removed function or static is still referred to.
    pub fn dangling(&self) -> Result<(), LinkError> {
        for (_, piece) in &self.removed {
            for sym in std::iter::once(piece.sym).chain(piece.defines.iter().copied()) {
                if self.new.home(sym).is_some() || !matches!(self.new.def(sym), Def::None) {
                    continue;
                }
                let users: Vec<PieceId> =
                    self.new.refs(sym).iter().copied().filter(|it| self.new.piece(*it).is_some()).collect();
                if let Some(user) = users.first() {
                    let user = self.new.piece(*user).expect("a live piece");
                    return Err(error(
                        function_of(user),
                        format!(
                            "{} is removed, and {} still refers to it",
                            describe(self.symbols.key(sym)),
                            describe(self.symbols.key(user.sym)),
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn function_of(piece: &Piece) -> Option<InstanceKey> {
    match &piece.what {
        What::Function(info) => Some(info.key),
        _ => None,
    }
}

/// A symbol as an error names it.
pub(crate) fn describe(key: &SymKey) -> String {
    match key {
        SymKey::Body(key) | SymKey::Slot(key) => format!("the function {key:?}"),
        SymKey::Static(item, instance) => format!("the static {item:?} ({instance:?})"),
        SymKey::Name(name) => format!("`{name}`"),
        SymKey::Data(id) => format!("the data {id:?}"),
        other => format!("{other:?}"),
    }
}
