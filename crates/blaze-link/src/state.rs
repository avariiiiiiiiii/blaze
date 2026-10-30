//! An image's state: its pieces, where each is, what every symbol is defined as, and the
//! bytes of every region. Cloning it is O(1) in the image's size: a reference count per column,
//! list arena and region, and one allocation (the regions' list, plus a name per
//! [`Class::Named`] region). So a stage works on a copy and the state it started from stays
//! whole: a stage that fails leaves nothing behind, and a rollback is the state before. A stage
//! copies only the tables, chunks and pages it writes ([`crate::cow`]).
//!
//! A piece is a row of columns: [`Hot`] (32 bytes: what placement, relocation and the writers
//! read of every piece), [`What`] (16), [`Sort`] (16), and its lists (fixups, weak targets,
//! symbols it defines, what its last write left for the loader) as [`ListRef`]s into shared
//! arenas. A symbol's definition is a 16-byte [`Def`]; the pieces referring to it are a list in
//! another arena, grown by doubling.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use blaze_codegen::{InstanceKey, Line};
use blaze_ids::{ContentId, radix};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::cow::{CowBytes, CowList, CowVec, ListRef};
use crate::model::{Class, Fixup, NativeFile, Region, Special, Sym, Tls};
pub(crate) use crate::native::Resolved;
use crate::position::Spans;
use crate::reloc::GotKind;
use crate::symbols::{IdNo, NameId, Obj, SHARDS, Symbols};
use crate::{NativeKind, Output};

pub(crate) type PieceId = u32;
pub(crate) type FuncId = u32;
/// What a write left for the loader to bind: offset in the piece, import, addend.
pub(crate) type Bind = (u64, u32, i64);

/// An index into the image's table of classes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct ClassId(pub u16);

/// What a symbol is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Def {
    None,
    /// At `offset` in a piece.
    Piece { piece: PieceId, offset: u64 },
    /// The image's import of this index.
    Import(u32),
    /// A value, not an address of the image: never slid. A weak symbol nothing defines is 0.
    Absolute(u64),
    /// An offset in the image's TLS block that is not a piece's (a native `Absolute`
    /// thread-local: the local-dynamic module base).
    TlsOffset(u64),
    /// Whatever another symbol is.
    Alias(Sym),
    /// The linker's: [`State::specials`] at this index.
    Special(u32),
}

const LIVE: u8 = 1;
const ZERO: u8 = 2;

/// What every pass reads of a piece.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Hot {
    pub addr: u64,
    pub size: u64,
    pub sym: Sym,
    /// Index into [`State::spaces`]; `u32::MAX` until placed.
    pub region: u32,
    /// The class it goes in in a full link.
    pub class: ClassId,
    align_log2: u8,
    flags: u8,
}

impl Hot {
    #[inline]
    pub fn align(&self) -> u64 {
        1 << self.align_log2
    }

    /// In a zeroed region: no bytes.
    #[inline]
    pub fn zero(&self) -> bool {
        self.flags & ZERO != 0
    }

    #[inline]
    pub fn live(&self) -> bool {
        self.flags & LIVE != 0
    }

    #[inline]
    pub fn set_zero(&mut self, zero: bool) {
        self.flags = if zero { self.flags | ZERO } else { self.flags & !ZERO };
    }

    #[inline]
    pub fn placed(&self) -> bool {
        self.region != u32::MAX
    }
}

/// One thing placed in a region: a function's code, a datum, a static, a native section, a
/// GOT cell, a call frame record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum What {
    /// [`State::function`] of this.
    Function(FuncId),
    /// Shared by what refers to it and gone with the last: read-only data merged by content,
    /// a panic location, a file name, a CIE. [`State::users`] counts the fixups to it.
    Shared,
    Static { thread_local: bool },
    Native { obj: Obj, section: u32, unwind: bool },
    /// A native call frame record; `fde` for an FDE (gone with what it describes).
    NativeFrame { obj: Obj, section: u32, fde: bool },
    Common,
    Got { kind: GotKind, target: Sym },
    Plt { target: Sym },
    Fde,
    WinInfo,
    /// A Mach-O TLV descriptor of a thread-local static.
    Tlv,
    /// The jump table's generation cell and stubs, the live descriptor and its directory:
    /// made once per full link.
    Fixed,
    /// The jump table's cells: no fixups. Entry `slot` holds the address of
    /// [`State::cell`] of it.
    Cells,
}

/// A full link's order within a class: rank, then for native pieces the input's place on the
/// link line, its member and section (others sort after them).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Sort {
    pub rank: u32,
    pub order: [u32; 3],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Lists {
    fixups: ListRef,
    /// The fixups' targets it refers to weakly: 0 when nothing defines them.
    weak: ListRef,
    /// Symbols besides its own defined in it (a native section's), which move with it.
    defines: ListRef,
}

/// What its last write left for the loader: offsets in it of slid addresses, and binds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Loader {
    rebase: ListRef,
    binds: ListRef,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FunctionInfo {
    pub key: InstanceKey,
    pub id: IdNo,
    pub fde: Option<Sym>,
    /// Windows: its `UNWIND_INFO` piece, and its handler and data.
    pub windows: Option<(Sym, Option<Sym>, Option<Sym>)>,
    /// [`State::lines`] of it.
    pub lines: ListRef,
}

/// Of each symbol, besides its definition: the pieces referring to it (a list with room to
/// grow), the first jump-table slot running it, and whether resolution looks at it.
#[derive(Clone, Copy, Debug)]
struct SymCold {
    refs: ListRef,
    room: u32,
    slot: u32,
    seen: bool,
}

const COLD: SymCold = SymCold { refs: ListRef::EMPTY, room: 0, slot: u32::MAX, seen: false };

/// Of each interned id: the debug name of the function it is, and the span table of the
/// anchor it is.
#[derive(Clone, Debug)]
struct IdCold {
    debug_name: Option<NameId>,
    spans: Option<Spans>,
}

const _: () = assert!(size_of::<Def>() == 16);
const _: () = assert!(size_of::<Hot>() == 32);
const _: () = assert!(size_of::<What>() == 16);
const _: () = assert!(size_of::<Sort>() == 16);
const _: () = assert!(size_of::<Lists>() == 36);
const _: () = assert!(size_of::<Loader>() == 24);
const _: () = assert!(size_of::<SymCold>() == 24);
const _: () = assert!(size_of::<FunctionInfo>() <= 88);

/// A piece to add: [`State::add`] copies its lists into the arenas.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NewPiece<'x> {
    pub sym: Sym,
    pub class: ClassId,
    pub rank: u32,
    pub order: [u32; 3],
    pub align: u64,
    pub size: u64,
    pub zero: bool,
    pub fixups: &'x [Fixup],
    pub weak: &'x [Sym],
    pub defines: &'x [Sym],
    pub what: What,
}

/// A piece as read: its [`Hot`] fields through `Deref`, the rest by method.
#[derive(Clone, Copy)]
pub(crate) struct PieceRef<'s> {
    pub id: PieceId,
    hot: &'s Hot,
    state: &'s State,
}

impl std::ops::Deref for PieceRef<'_> {
    type Target = Hot;

    fn deref(&self) -> &Hot {
        self.hot
    }
}

impl<'s> PieceRef<'s> {
    pub fn hot(self) -> &'s Hot {
        self.hot
    }

    pub fn class(self) -> &'s Class {
        self.state.class(self.hot.class)
    }

    pub fn what(self) -> What {
        *self.state.what.get(self.id as usize).expect("a piece's kind")
    }

    pub fn sort(self) -> Sort {
        *self.state.sort.get(self.id as usize).expect("a piece's order")
    }

    pub fn rank(self) -> u32 {
        self.sort().rank
    }

    pub fn order(self) -> [u32; 3] {
        self.sort().order
    }

    fn lists(self) -> Lists {
        *self.state.piece_lists.get(self.id as usize).expect("a piece's lists")
    }

    pub fn fixups(self) -> &'s [Fixup] {
        self.state.fixups.get(self.lists().fixups)
    }

    pub fn weak(self) -> &'s [Sym] {
        self.state.syms.get(self.lists().weak)
    }

    pub fn defines(self) -> &'s [Sym] {
        self.state.syms.get(self.lists().defines)
    }

    pub fn rebase(self) -> &'s [u64] {
        let loader = self.state.loader.get(self.id as usize).expect("a piece's loader lists");
        self.state.rebase.get(loader.rebase)
    }

    pub fn binds(self) -> &'s [Bind] {
        let loader = self.state.loader.get(self.id as usize).expect("a piece's loader lists");
        self.state.binds.get(loader.binds)
    }

    pub fn function(self) -> Option<&'s FunctionInfo> {
        match self.what() {
            What::Function(id) => Some(self.state.function(id)),
            _ => None,
        }
    }

    /// The bytes it has now in its region (none when zeroed or unplaced).
    pub fn bytes(self) -> Cow<'s, [u8]> {
        let Some(space) = self.state.spaces.get(self.hot.region as usize) else { return Cow::Borrowed(&[]) };
        if space.zero || self.hot.zero() {
            return Cow::Borrowed(&[]);
        }
        let at = (self.hot.addr - space.region.addr) as usize;
        space.bytes.read(at, self.hot.size as usize)
    }

    /// The same, borrowed, or copied into `arena` when it spans written pages.
    pub fn bytes_in<'b>(self, arena: &'b blaze_mem::Frame) -> &'b [u8]
    where
        's: 'b,
    {
        let Some(space) = self.state.spaces.get(self.hot.region as usize) else { return &[] };
        if space.zero || self.hot.zero() {
            return &[];
        }
        let (at, len) = ((self.hot.addr - space.region.addr) as usize, self.hot.size as usize);
        if let Some(it) = space.bytes.slice(at, len) {
            return it;
        }
        let out = arena.alloc_slice_fill_with(len, |_| 0);
        space.bytes.copy_to(at, out);
        out
    }
}

/// A region of the image and its bytes (empty for a zeroed one).
#[derive(Clone, Debug)]
pub(crate) struct Space {
    pub region: Region,
    pub bytes: CowBytes,
    pub zero: bool,
    /// Where it is in the file.
    pub offset: Option<u64>,
}

/// A native input of the image.
#[derive(Clone)]
pub(crate) struct Input {
    pub id: ContentId,
    pub name: Arc<str>,
    pub kind: NativeKind,
    pub order: u32,
    pub bytes: Arc<[u8]>,
    pub file: Arc<NativeFile>,
}

/// Every field is `Copy`, an `Arc`, or a copy-on-write container whose clone is a reference
/// count, except `spaces` (a few dozen regions): see the module's comment for the cost.
#[derive(Clone)]
pub(crate) struct State {
    pub generation: u64,
    /// Written only by resolution ([`crate::native`]), with `Arc::make_mut`.
    pub specials: Arc<Vec<Special>>,
    pub spaces: Vec<Space>,
    /// The names Rust code defines (`#[no_mangle]`, `main`), each the symbol it is.
    // BTreeMap: changed by every stage and walked in order by each native resolve.
    pub names: Arc<BTreeMap<NameId, Sym>>,
    /// wasm: what codegen's functions and statics say each name is imported as.
    pub wasm_imports: Arc<FxHashMap<NameId, blaze_codegen::wasm::Import>>,
    pub inputs: Arc<Vec<Input>>,
    pub resolved: Arc<Resolved>,
    /// The jump table: slots given out, and how many there are room for.
    pub slots: u32,
    /// Each slot's symbol, by its index.
    pub slot_syms: CowVec<Sym>,
    pub capacity: u32,
    pub tls: Option<Tls>,
    pub file_len: u64,
    pub parts: Arc<Vec<(u64, Vec<u8>)>>,
    /// The debug sections in the file ([`crate::model::File::debug`]): (file offset, index into
    /// [`crate::write::Lists::debug`]).
    pub debug: Arc<Vec<(u64, usize)>>,
    /// What the image exported, as the writer was given it: the next link's
    /// [`crate::model::Delta::same_names`] compares with it.
    pub exports: Arc<Vec<crate::model::Export>>,
    /// The [`crate::model::Placed::synthesis`] the writer last made this state's tables in.
    pub synthesis: u64,
    /// The 8-byte slots the writer encodes in the file ([`crate::model::Writer::encode`]), as
    /// (file offset, value), sorted by offset. The regions keep the plain values, which the
    /// next link and live patches read; only the file holds these.
    pub encoded: Arc<Vec<(u64, u64)>>,
    /// The writer's loader, GOT and function lists, kept sorted across links.
    pub lists: Arc<crate::write::Lists>,
    /// What the image is across its generations: its first full link's.
    pub lineage: [u8; 16],
    pub laid_out: bool,

    hot: CowVec<Hot>,
    what: CowVec<What>,
    sort: CowVec<Sort>,
    piece_lists: CowVec<Lists>,
    loader: CowVec<Loader>,
    /// Of each [`What::Shared`] piece, the fixups to it.
    users: CowVec<u32>,
    fixups: CowList<Fixup>,
    syms: CowList<Sym>,
    rebase: CowList<u64>,
    binds: CowList<Bind>,
    /// Per symbol shard ([`Symbols::split`]): inline, so a clone allocates nothing.
    defs: [CowVec<Def>; SHARDS],
    cold: [CowVec<SymCold>; SHARDS],
    ref_items: CowList<PieceId>,
    /// Every name a piece has referred to, in the order first seen.
    seen: CowVec<Sym>,
    classes: Arc<Vec<Class>>,
    functions: CowVec<FunctionInfo>,
    lines: CowList<Line>,
    /// Per id shard ([`Symbols::split_id`]).
    ids: [CowVec<IdCold>; SHARDS],
    /// Per slot: the body it runs, and the next slot running the same body.
    cells: CowVec<Sym>,
    slot_next: CowVec<u32>,
    /// The pieces removed, in order, across the state's lineage.
    removals: CowVec<PieceId>,
}

impl State {
    pub fn new() -> State {
        State {
            generation: blaze_live::patch::FIRST_GENERATION,
            specials: Arc::default(),
            // alloc: none (an empty `Vec`), once per image
            spaces: Vec::new(),
            names: Arc::default(),
            wasm_imports: Arc::default(),
            inputs: Arc::default(),
            resolved: Arc::default(),
            slots: 0,
            slot_syms: CowVec::default(),
            capacity: 0,
            tls: None,
            file_len: 0,
            parts: Arc::default(),
            debug: Arc::default(),
            exports: Arc::default(),
            synthesis: 0,
            encoded: Arc::default(),
            lineage: [0; 16],
            laid_out: false,
            hot: CowVec::default(),
            what: CowVec::default(),
            sort: CowVec::default(),
            piece_lists: CowVec::default(),
            lists: Arc::default(),
            loader: CowVec::default(),
            users: CowVec::default(),
            fixups: CowList::default(),
            syms: CowList::default(),
            rebase: CowList::default(),
            binds: CowList::default(),
            defs: std::array::from_fn(|_| CowVec::default()),
            cold: std::array::from_fn(|_| CowVec::default()),
            ref_items: CowList::default(),
            removals: CowVec::default(),
            seen: CowVec::default(),
            classes: Arc::default(),
            functions: CowVec::default(),
            lines: CowList::default(),
            ids: std::array::from_fn(|_| CowVec::default()),
            cells: CowVec::default(),
            slot_next: CowVec::default(),
        }
    }

    // Symbols.

    #[inline]
    pub fn def(&self, sym: Sym) -> Def {
        let (shard, at) = Symbols::split(sym);
        self.defs[shard].get(at).copied().unwrap_or(Def::None)
    }

    pub fn set_def(&mut self, sym: Sym, def: Def) {
        let (shard, at) = Symbols::split(sym);
        self.defs[shard].set(at, def, || Def::None);
    }

    fn cold(&self, sym: Sym) -> SymCold {
        let (shard, at) = Symbols::split(sym);
        self.cold[shard].get(at).copied().unwrap_or(COLD)
    }

    fn cold_mut(&mut self, sym: Sym) -> &mut SymCold {
        let (shard, at) = Symbols::split(sym);
        if self.cold[shard].len() <= at {
            self.cold[shard].set(at, COLD, || COLD);
        }
        self.cold[shard].get_mut(at).expect("grown to it")
    }

    /// The piece a symbol is in, through aliases.
    pub fn home(&self, mut sym: Sym) -> Option<(PieceId, u64)> {
        for _ in 0..64 {
            match self.def(sym) {
                Def::Piece { piece, offset } => return Some((piece, offset)),
                Def::Alias(to) => sym = to,
                _ => return None,
            }
        }
        None
    }

    /// Where a symbol is in the image as placed: its piece's address plus its offset.
    pub fn addr_of(&self, sym: Sym) -> Option<u64> {
        let (piece, offset) = self.home(sym)?;
        Some(self.piece(piece)?.addr + offset)
    }

    /// The live pieces whose fixups refer to `sym`, each once, in the order each started to.
    pub fn refs(&self, sym: Sym) -> impl Iterator<Item = PieceId> + '_ {
        self.ref_items.get(self.cold(sym).refs).iter().copied().filter(|it| self.is_live(*it))
    }

    fn ref_push(&mut self, sym: Sym, id: PieceId) {
        let cold = *self.cold_mut(sym);
        let len = cold.refs.len();
        if len < cold.room as usize {
            let whole = ListRef { len: cold.room, ..cold.refs };
            self.ref_items.get_mut(whole)[len] = id;
            self.cold_mut(sym).refs.len += 1;
            return;
        }
        let room = (cold.room as usize * 2).max(4);
        let mut grown: SmallVec<[PieceId; 32]> = SmallVec::with_capacity(room);
        grown.extend_from_slice(self.ref_items.get(cold.refs));
        grown.push(id);
        grown.resize(room, PieceId::MAX);
        let list = self.ref_items.push(&grown);
        if cold.room > 0 {
            self.ref_items.free(ListRef { len: cold.room, ..cold.refs });
        }
        let it = self.cold_mut(sym);
        it.refs = ListRef { len: len as u32 + 1, ..list };
        it.room = room as u32;
    }

    /// Adds `id` to `sym`'s referrers unless it was the last added: while one piece's fixups
    /// are walked no other piece is added, so that finds every repeat.
    fn ref_push_once(&mut self, sym: Sym, id: PieceId) {
        if self.ref_items.get(self.cold(sym).refs).last() != Some(&id) {
            self.ref_push(sym, id);
        }
    }

    /// Adds `id` to the referrer lists of what the fixups in `list` refer to, once each.
    fn refer(&mut self, id: PieceId, list: ListRef) {
        for at in 0..list.len() {
            let fixup = self.fixups.get(list)[at];
            self.ref_push_once(fixup.target, id);
            if let Some(minus) = fixup.minus {
                self.ref_push_once(minus, id);
            }
        }
    }

    /// Takes `id` off the referrer lists of what the fixups in `list` refer to.
    fn unrefer(&mut self, id: PieceId, list: ListRef) {
        for at in 0..list.len() {
            let fixup = self.fixups.get(list)[at];
            self.ref_remove(fixup.target, id);
            if let Some(minus) = fixup.minus {
                self.ref_remove(minus, id);
            }
        }
    }

    fn ref_remove(&mut self, sym: Sym, id: PieceId) {
        let cold = self.cold(sym);
        if cold.refs.is_empty() {
            return;
        }
        let items = self.ref_items.get(cold.refs);
        let Some(at) = items.iter().position(|it| *it == id) else { return };
        let items = self.ref_items.get_mut(cold.refs);
        items.copy_within(at + 1.., at);
        self.cold_mut(sym).refs.len -= 1;
    }

    /// Every name a piece has referred to (what resolution looks at), in the order first seen.
    pub fn seen(&self) -> impl Iterator<Item = Sym> + '_ {
        self.seen.iter().copied()
    }

    pub fn is_seen(&self, sym: Sym) -> bool {
        self.cold(sym).seen
    }

    /// Marks a name as referred to; whether it was not before.
    pub fn see(&mut self, sym: Sym) -> bool {
        let cold = self.cold_mut(sym);
        if cold.seen {
            return false;
        }
        cold.seen = true;
        self.seen.push(sym);
        true
    }

    // Pieces.

    #[inline]
    fn is_live(&self, id: PieceId) -> bool {
        self.hot.get(id as usize).is_some_and(Hot::live)
    }

    pub fn piece_count(&self) -> u32 {
        self.hot.len() as u32
    }

    /// A live piece.
    #[inline]
    pub fn piece(&self, id: PieceId) -> Option<PieceRef<'_>> {
        let hot = self.hot.get(id as usize).filter(|it| it.live())?;
        Some(PieceRef { id, hot, state: self })
    }

    /// A piece live or removed: a removed piece's columns stay as they were.
    pub fn any(&self, id: PieceId) -> Option<PieceRef<'_>> {
        let hot = self.hot.get(id as usize)?;
        Some(PieceRef { id, hot, state: self })
    }

    pub fn hot_mut(&mut self, id: PieceId) -> &mut Hot {
        self.hot.get_mut(id as usize).expect("a piece")
    }

    pub fn live_pieces(&self) -> impl Iterator<Item = PieceRef<'_>> + '_ {
        self.hot.iter().enumerate().filter(|(_, it)| it.live()).map(|(id, hot)| PieceRef { id: id as PieceId, hot, state: self })
    }

    /// The symbols a piece's fixups refer to, once each: inline up to eight.
    pub fn targets(piece: PieceRef<'_>) -> SmallVec<[Sym; 8]> {
        let fixups = piece.fixups();
        let mut out: SmallVec<[Sym; 8]> = SmallVec::with_capacity(fixups.len() + fixups.iter().filter(|it| it.minus.is_some()).count());
        for fixup in fixups {
            out.push(fixup.target);
            out.extend(fixup.minus);
        }
        radix::sort_copy_by_key(&mut out, |it| it.0);
        out.dedup();
        out
    }

    /// Adds a piece, live and not placed, and its references; the caller defines its symbols.
    pub fn add(&mut self, piece: &NewPiece<'_>) -> PieceId {
        let id = self.hot.len() as PieceId;
        let align_log2 = piece.align.max(1).next_power_of_two().trailing_zeros() as u8;
        self.hot.push(Hot {
            addr: 0,
            size: piece.size,
            sym: piece.sym,
            region: u32::MAX,
            class: piece.class,
            align_log2,
            flags: LIVE | if piece.zero { ZERO } else { 0 },
        });
        self.what.push(piece.what);
        self.sort.push(Sort { rank: piece.rank, order: piece.order });
        let lists = Lists {
            fixups: self.fixups.push(piece.fixups),
            weak: self.syms.push(piece.weak),
            defines: self.syms.push(piece.defines),
        };
        self.piece_lists.push(lists);
        self.loader.push(Loader::default());
        self.users.push(0);
        self.refer(id, lists.fixups);
        id
    }

    /// Undefines `sym` when it is defined in piece `id`.
    fn undefine_in(&mut self, sym: Sym, id: PieceId) {
        if matches!(self.def(sym), Def::Piece { piece, .. } if piece == id) {
            self.set_def(sym, Def::None);
        }
    }

    /// Removes a piece, its symbols' definitions and its entries in the referrer lists of
    /// what it referred to. Its columns stay readable through [`State::any`].
    pub fn remove(&mut self, id: PieceId) -> bool {
        let Some(piece) = self.piece(id) else { return false };
        let own = piece.sym;
        let lines = piece.function().map(|it| it.lines);
        let lists = piece.lists();
        let loader = *self.loader.get(id as usize).expect("a piece's loader lists");
        self.hot_mut(id).flags &= !LIVE;
        self.removals.push(id);
        self.unrefer(id, lists.fixups);
        self.undefine_in(own, id);
        for at in 0..lists.defines.len() {
            let sym = self.syms.get(lists.defines)[at];
            self.undefine_in(sym, id);
        }
        self.fixups.free(lists.fixups);
        self.syms.free(lists.weak);
        self.syms.free(lists.defines);
        self.rebase.free(loader.rebase);
        self.binds.free(loader.binds);
        if let Some(lines) = lines {
            self.lines.free(lines);
        }
        true
    }

    /// How many pieces this state's lineage has removed.
    pub fn removals(&self) -> u64 {
        self.removals.len() as u64
    }

    /// The pieces removed after the first `mark` removals, in order.
    pub fn removed_since(&self, mark: u64) -> impl Iterator<Item = PieceId> + '_ {
        (mark as usize..self.removals.len()).map(|at| *self.removals.get(at).expect("in range"))
    }

    /// Replaces a live piece's fixups, keeping the referrer lists right.
    pub fn set_fixups(&mut self, id: PieceId, fixups: &[Fixup]) {
        let list = self.piece(id).expect("a live piece").lists().fixups;
        self.unrefer(id, list);
        self.fixups.free(list);
        let list = self.fixups.push(fixups);
        self.piece_lists.get_mut(id as usize).expect("a piece's lists").fixups = list;
        self.refer(id, list);
    }

    pub fn set_defines(&mut self, id: PieceId, defines: &[Sym]) {
        let old = self.piece_lists.get(id as usize).expect("a piece's lists").defines;
        self.syms.free(old);
        let list = self.syms.push(defines);
        self.piece_lists.get_mut(id as usize).expect("a piece's lists").defines = list;
    }

    pub fn set_weak(&mut self, id: PieceId, weak: &[Sym]) {
        let old = self.piece_lists.get(id as usize).expect("a piece's lists").weak;
        self.syms.free(old);
        let list = self.syms.push(weak);
        self.piece_lists.get_mut(id as usize).expect("a piece's lists").weak = list;
    }

    /// What a piece's write left for the loader.
    pub fn set_loader(&mut self, id: PieceId, rebase: &[u64], binds: &[Bind]) {
        let old = *self.loader.get(id as usize).expect("a piece's loader lists");
        if self.rebase.get(old.rebase) == rebase && self.binds.get(old.binds) == binds {
            return;
        }
        self.rebase.free(old.rebase);
        self.binds.free(old.binds);
        let new = Loader { rebase: self.rebase.push(rebase), binds: self.binds.push(binds) };
        *self.loader.get_mut(id as usize).expect("a piece's loader lists") = new;
    }

    /// Of a [`What::Shared`] piece, the fixups referring to it.
    pub fn users(&self, id: PieceId) -> u32 {
        self.users.get(id as usize).copied().unwrap_or(0)
    }

    pub fn users_mut(&mut self, id: PieceId) -> &mut u32 {
        self.users.get_mut(id as usize).expect("a piece's count")
    }

    /// Whether the list arenas are mostly garbage: then [`State::compact`] pays for itself.
    pub fn wants_compaction(&self) -> bool {
        let over = |garbage: usize, live: usize| garbage > live + (1 << 16);
        over(self.fixups.garbage(), self.fixups.live())
            || over(self.syms.garbage(), self.syms.live())
            || over(self.ref_items.garbage(), self.ref_items.live())
            || over(self.rebase.garbage(), self.rebase.live())
            || over(self.binds.garbage(), self.binds.live())
            || over(self.lines.garbage(), self.lines.live())
    }

    /// Copies every live list into fresh arenas and rebuilds the referrer lists, dropping
    /// what removed pieces left: O(image), done when the garbage outweighs the rest.
    pub fn compact(&mut self) {
        // clone: O(1), shares every chunk: the old lists are read while fresh ones are built
        let old = self.clone();
        self.fixups = CowList::default();
        self.syms = CowList::default();
        self.rebase = CowList::default();
        self.binds = CowList::default();
        self.ref_items = CowList::default();
        self.lines = CowList::default();
        for at in 0..self.functions.len() {
            self.functions.get_mut(at).expect("in range").lines = ListRef::EMPTY;
        }
        for shard in &mut self.cold {
            let len = shard.len();
            for at in 0..len {
                let it = shard.get(at).copied().expect("in range");
                if it.room > 0 {
                    *shard.get_mut(at).expect("in range") = SymCold { refs: ListRef::EMPTY, room: 0, ..it };
                }
            }
        }
        for id in 0..old.piece_count() {
            let piece = old.any(id).expect("in range");
            let (lists, loader) = if piece.live() {
                let lists = Lists {
                    fixups: self.fixups.push(piece.fixups()),
                    weak: self.syms.push(piece.weak()),
                    defines: self.syms.push(piece.defines()),
                };
                let loader = Loader { rebase: self.rebase.push(piece.rebase()), binds: self.binds.push(piece.binds()) };
                (lists, loader)
            } else {
                (Lists::default(), Loader::default())
            };
            *self.piece_lists.get_mut(id as usize).expect("in range") = lists;
            *self.loader.get_mut(id as usize).expect("in range") = loader;
            if piece.live() {
                self.refer(id, lists.fixups);
                if let What::Function(function) = piece.what() {
                    let lines = self.lines.push(old.lines(old.function(function)));
                    self.functions.get_mut(function as usize).expect("a function").lines = lines;
                }
            }
        }
    }

    // Classes.

    pub fn find_class(&self, class: &Class) -> Option<ClassId> {
        self.classes.iter().position(|it| it == class).map(|it| ClassId(it as u16))
    }

    pub fn class_id(&mut self, class: &Class) -> ClassId {
        if let Some(id) = self.find_class(class) {
            return id;
        }
        let classes = Arc::make_mut(&mut self.classes);
        let id = u16::try_from(classes.len()).expect("fewer than 2^16 classes");
        // clone: once per class the image gets (a few dozen for its life)
        classes.push(class.clone());
        ClassId(id)
    }

    pub fn class(&self, id: ClassId) -> &Class {
        &self.classes[id.0 as usize]
    }

    // Functions, debug names, span tables.

    pub fn add_function(
        &mut self,
        key: InstanceKey,
        id: IdNo,
        fde: Option<Sym>,
        windows: Option<(Sym, Option<Sym>, Option<Sym>)>,
        lines: &[Line],
    ) -> FuncId {
        let lines = self.lines.push(lines);
        self.functions.push(FunctionInfo { key, id, fde, windows, lines }) as FuncId
    }

    pub fn function(&self, id: FuncId) -> &FunctionInfo {
        self.functions.get(id as usize).expect("a function")
    }

    pub fn lines(&self, info: &FunctionInfo) -> &[Line] {
        self.lines.get(info.lines)
    }

    fn id_cold_mut(&mut self, id: IdNo) -> &mut IdCold {
        let (shard, at) = Symbols::split_id(id);
        let empty = || IdCold { debug_name: None, spans: None };
        if self.ids[shard].len() <= at {
            self.ids[shard].set(at, empty(), empty);
        }
        self.ids[shard].get_mut(at).expect("grown to it")
    }

    fn id_cold(&self, id: IdNo) -> Option<&IdCold> {
        let (shard, at) = Symbols::split_id(id);
        self.ids[shard].get(at)
    }

    /// The name debuggers and backtraces show for a function, by its interned key.
    pub fn debug_name(&self, function: IdNo) -> Option<NameId> {
        self.id_cold(function)?.debug_name
    }

    pub fn set_debug_name(&mut self, function: IdNo, name: NameId) {
        if self.debug_name(function) != Some(name) {
            self.id_cold_mut(function).debug_name = Some(name);
        }
    }

    pub fn span_table(&self, anchor: IdNo) -> Option<&Spans> {
        self.id_cold(anchor)?.spans.as_ref()
    }

    pub fn set_span_table(&mut self, anchor: IdNo, table: Spans) {
        self.id_cold_mut(anchor).spans = Some(table);
    }

    // Regions.

    /// The regions in one list, for a writer's [`crate::model::Placed::regions`].
    pub fn regions(&self) -> Vec<Region> {
        // alloc: once per write, a few dozen regions for the writers' slice
        self.spaces.iter().map(|it| it.region.clone()).collect()
    }

    pub fn space_of(&self, class: &Class) -> Option<usize> {
        self.spaces.iter().position(|it| &it.region.class == class)
    }

    /// The bytes a placed piece has now.
    pub fn bytes(&self, id: PieceId) -> Cow<'_, [u8]> {
        self.any(id).map_or(Cow::Borrowed(&[][..]), PieceRef::bytes)
    }

    /// Where a placed piece's bytes are: its region's bytes, the offset and the length; `None`
    /// for one with no bytes (zeroed or not placed).
    fn located(&self, id: PieceId) -> Option<(&CowBytes, usize, usize)> {
        let hot = self.hot.get(id as usize)?;
        let space = self.spaces.get(hot.region as usize)?;
        if space.zero || hot.zero() || hot.size == 0 {
            return None;
        }
        Some((&space.bytes, (hot.addr - space.region.addr) as usize, hot.size as usize))
    }

    /// Whether piece `id` has the same bytes here and in `other` ([`State::bytes`] of each,
    /// equal), compared in place without copying either.
    pub fn same_bytes(&self, other: &State, id: PieceId) -> bool {
        match (self.located(id), other.located(id)) {
            (None, None) => true,
            (Some((a, at, len)), Some((b, other_at, other_len))) => len == other_len && a.same(at, b, other_at, len),
            (Some(_), None) | (None, Some(_)) => false,
        }
    }

    // The jump table.

    /// Gives out the next slot, empty.
    pub fn new_slot(&mut self, sym: Sym) -> u32 {
        let slot = self.slots;
        self.slots += 1;
        self.slot_syms.push(sym);
        self.cells.push(crate::symbols::NO_SYM);
        self.slot_next.push(u32::MAX);
        slot
    }

    /// The body a slot runs.
    pub fn cell(&self, slot: u32) -> Option<Sym> {
        self.cells.get(slot as usize).copied().filter(|it| *it != crate::symbols::NO_SYM)
    }

    /// Puts a body in a slot (or empties it), keeping each body's list of slots.
    pub fn set_cell(&mut self, slot: u32, body: Option<Sym>) {
        let old = self.cell(slot);
        if old == body {
            return;
        }
        if let Some(old) = old {
            let next = *self.slot_next.get(slot as usize).expect("a slot");
            let head = self.cold(old).slot;
            if head == slot {
                self.cold_mut(old).slot = next;
            } else {
                let mut at = head;
                while at != u32::MAX {
                    let after = *self.slot_next.get(at as usize).expect("a slot");
                    if after == slot {
                        *self.slot_next.get_mut(at as usize).expect("a slot") = next;
                        break;
                    }
                    at = after;
                }
            }
        }
        *self.cells.get_mut(slot as usize).expect("a slot") = body.unwrap_or(crate::symbols::NO_SYM);
        let next = match body {
            Some(body) => {
                let cold = self.cold_mut(body);
                std::mem::replace(&mut cold.slot, slot)
            }
            None => u32::MAX,
        };
        *self.slot_next.get_mut(slot as usize).expect("a slot") = next;
    }

    /// The slots running a body.
    pub fn slots_of(&self, body: Sym) -> impl Iterator<Item = u32> + '_ {
        let mut at = self.cold(body).slot;
        std::iter::from_fn(move || {
            if at == u32::MAX {
                return None;
            }
            let slot = at;
            at = *self.slot_next.get(slot as usize).expect("a slot");
            Some(slot)
        })
    }

    /// Every slot given out that runs a body, in slot order.
    pub fn filled_cells(&self) -> impl Iterator<Item = (u32, Sym)> + '_ {
        self.cells
            .iter()
            .enumerate()
            .filter(|(_, it)| **it != crate::symbols::NO_SYM)
            .map(|(slot, it)| (slot as u32, *it))
    }
}

/// Whether a class's regions are zeroed at load rather than read from the file.
pub(crate) fn zeroed(class: &Class) -> bool {
    matches!(class, Class::Bss | Class::Tbss)
}

/// Whether a format reaches functions by address through stubs (not wasm's indexes).
pub(crate) fn has_table(output: &Output, indexed_code: bool) -> bool {
    output.incremental && !indexed_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Apply, Prot, SectionName};

    fn fixup(target: Sym, minus: Option<Sym>) -> Fixup {
        Fixup { at: 0, apply: Apply::Abs64, target, addend: 0, minus }
    }

    fn piece(sym: Sym, class: ClassId, fixups: &[Fixup]) -> NewPiece<'_> {
        NewPiece {
            sym,
            class,
            rank: 0,
            order: [0; 3],
            align: 8,
            size: 16,
            zero: false,
            fixups,
            weak: &[],
            defines: &[],
            what: What::Shared,
        }
    }

    fn space(class: Class, addr: u64, bytes: Vec<u8>) -> Space {
        let size = bytes.len() as u64;
        Space {
            region: Region { class, addr, capacity: size, size, prot: Prot::R, align: 16 },
            bytes: CowBytes::from_vec(bytes),
            zero: false,
            offset: None,
        }
    }

    /// A state of `pieces` pieces over every symbol shard, each referring to up to seven of 64
    /// symbols, in a code region and a named one.
    fn image(pieces: u32) -> State {
        let mut state = State::new();
        let class = state.class_id(&Class::Text);
        let fixups: Vec<Fixup> = (0..64).map(|it| fixup(Symbols::join(it % SHARDS, it), None)).collect();
        for n in 0..pieces {
            let sym = Symbols::join(n as usize % SHARDS, 100 + n as usize);
            let id = state.add(&piece(sym, class, &fixups[(n % 57) as usize..(n % 57 + n % 8) as usize]));
            state.set_def(sym, Def::Piece { piece: id, offset: 0 });
        }
        state.spaces.push(space(Class::Text, 0x1000, vec![0x90; 3 * CowBytes::PAGE]));
        let named = Class::Named { name: SectionName::new("linkme_ITEMS"), write: false, exec: false };
        state.spaces.push(space(named, 0x10000, vec![0; 64]));
        Arc::make_mut(&mut state.specials).push(Special::Absolute(1));
        state
    }

    /// What `Image::stage` pays to stage on a copy, whatever the image's size: the spaces list.
    /// Every column, arena and region is shared, a `Class::Named` region's read name too.
    #[test]
    fn a_clone_allocates_the_regions_list_only() {
        for pieces in [10, 5000] {
            let state = image(pieces);
            let (copy, used) = blaze_alloc_count::measure(|| state.clone());
            assert!(used.allocations <= 1, "{pieces} pieces: {used:?}");
            assert_eq!(copy.piece_count(), pieces);
            assert!(Arc::ptr_eq(&copy.specials, &state.specials));
        }
    }

    #[test]
    fn lookups_allocate_nothing() {
        let state = image(5000);
        let sym = Symbols::join(7, 107);
        blaze_alloc_count::assert_at_most(0, "a symbol's definition, home, piece and lists", || {
            let (piece, _) = state.home(sym).expect("defined");
            assert_eq!(state.def(sym), Def::Piece { piece, offset: 0 });
            let piece = state.piece(piece).expect("live");
            assert_eq!(piece.sym, sym);
            assert_eq!(piece.fixups().len(), 7);
            assert!(state.refs(Symbols::join(10, 10)).next().is_some());
            assert!(state.same_bytes(&state, piece.id));
        });
    }

    /// A piece naming a symbol twice (and as the subtracted one) is its referrer once, and
    /// leaves every list it was in when removed or given other fixups.
    #[test]
    fn a_piece_refers_to_each_symbol_once() {
        let mut state = State::new();
        let class = state.class_id(&Class::Text);
        let (a, b, c) = (Symbols::join(1, 0), Symbols::join(2, 0), Symbols::join(3, 0));
        let first = state.add(&piece(Symbols::join(4, 0), class, &[fixup(a, Some(b)), fixup(b, None), fixup(a, Some(a))]));
        let second = state.add(&piece(Symbols::join(4, 1), class, &[fixup(a, None)]));
        let refs = |state: &State, sym: Sym| state.refs(sym).collect::<Vec<_>>();
        assert_eq!(refs(&state, a), vec![first, second]);
        assert_eq!(refs(&state, b), vec![first]);
        state.set_fixups(first, &[fixup(c, None), fixup(c, None)]);
        assert_eq!(refs(&state, a), vec![second]);
        assert_eq!(refs(&state, b), Vec::<PieceId>::new());
        assert_eq!(refs(&state, c), vec![first]);
        assert_eq!(&*State::targets(state.piece(first).unwrap()), &[c]);
        state.set_def(Symbols::join(4, 0), Def::Piece { piece: first, offset: 0 });
        assert!(state.remove(first));
        assert_eq!(refs(&state, c), Vec::<PieceId>::new());
        assert_eq!(state.def(Symbols::join(4, 0)), Def::None);
        state.compact();
        assert_eq!(refs(&state, a), vec![second]);
    }

    /// The comparison in place agrees with comparing the copies `State::bytes` gives.
    #[test]
    fn same_bytes_is_bytes_compared() {
        let mut old = image(3);
        let page = CowBytes::PAGE as u64;
        let places = [(0, 0x1000), (1, 0x1000 + page - 8), (2, 0x1000 + 2 * page)];
        for (id, addr) in places {
            let hot = old.hot_mut(id);
            hot.region = 0;
            hot.addr = addr;
        }
        let mut new = old.clone();
        new.spaces[0].bytes.write((page - 4) as usize, &[1, 2]);
        for id in 0..3 {
            assert_eq!(new.same_bytes(&old, id), new.bytes(id) == old.bytes(id), "piece {id}");
        }
        assert!(new.same_bytes(&old, 0) && !new.same_bytes(&old, 1) && new.same_bytes(&old, 2));
        new.hot_mut(2).set_zero(true);
        assert!(!new.same_bytes(&old, 2));
    }
}
