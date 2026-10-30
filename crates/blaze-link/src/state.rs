//! An image's state: its pieces, where each is, what every symbol is defined as, and the
//! bytes of every region. Cloning it is cheap ([`CowVec`], shared buffers), so a stage
//! works on a copy and the state it started from stays whole: a stage that fails leaves
//! nothing behind, and a rollback is the state before.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use blaze_codegen::{InstanceKey, Line};
use blaze_ids::ContentId;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::cow::CowVec;
use crate::model::{Class, Fixup, Import, Library, NativeFile, Region, Special, Sym, Tls};
use crate::position::SpanTable;
use crate::reloc::GotKind;
use crate::symbols::{Obj, Symbols};
use crate::{NativeKind, Output};

pub(crate) type PieceId = u32;

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

/// One thing placed in a region: a function's code, a datum, a static, a native section, a
/// GOT cell, a call frame record.
#[derive(Clone, Debug)]
pub(crate) struct Piece {
    pub sym: Sym,
    /// The class it goes in in a full link.
    pub class: Class,
    pub rank: u32,
    /// Native pieces: the input's place on the link line, its member and section; others
    /// sort after them.
    pub order: [u32; 3],
    pub align: u64,
    pub size: u64,
    /// In a zeroed region: no bytes.
    pub zero: bool,
    /// Index into [`State::spaces`].
    pub region: u32,
    pub addr: u64,
    pub fixups: Arc<[Fixup]>,
    /// The fixups' targets it refers to weakly: 0 when nothing defines them.
    pub weak: Arc<[Sym]>,
    /// Symbols besides its own defined in it (a native section's), which move with it.
    pub defines: Arc<[Sym]>,
    pub what: What,
    /// What its last write left for the loader: offsets in it of slid addresses, and binds
    /// (offset, import, addend).
    pub rebase: Arc<[u64]>,
    pub binds: Arc<[(u64, u32, i64)]>,
}

#[derive(Clone, Debug)]
pub(crate) enum What {
    Function(Arc<FunctionInfo>),
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
    /// The jump table's cells and generation cell, its stubs, the live descriptor and its
    /// directory: made once per full link.
    Fixed,
}

#[derive(Clone, Debug)]
pub(crate) struct FunctionInfo {
    pub key: InstanceKey,
    pub fde: Option<Sym>,
    /// Windows: its `UNWIND_INFO` piece, and its handler and data.
    pub windows: Option<(Sym, Option<Sym>, Option<Sym>)>,
    pub lines: Vec<Line>,
}

/// A region of the image and its bytes (empty for a zeroed one).
#[derive(Clone, Debug)]
pub(crate) struct Space {
    pub region: Region,
    pub bytes: Arc<Vec<u8>>,
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

/// What resolving the native inputs and the names gave, and what it was computed from.
#[derive(Clone, Default)]
pub(crate) struct Resolved {
    pub imports: Vec<Import>,
    /// Each import's symbol.
    pub import_syms: Vec<Sym>,
    pub libraries: Vec<Library>,
    /// The native sections in the image.
    pub kept: FxHashSet<(Obj, u32)>,
    /// The native call frame records in the image: section and start.
    pub frames: FxHashSet<(Obj, u32, u32)>,
    /// The commons, by name: size and alignment.
    pub commons: BTreeMap<Box<str>, (u64, u64)>,
}

#[derive(Clone)]
pub(crate) struct State {
    pub generation: u64,
    pub pieces: CowVec<Option<Piece>>,
    /// Of each [`What::Shared`] piece, the fixups to it.
    pub users: CowVec<u32>,
    /// Per symbol shard ([`Symbols::split`]): each symbol's definition and the pieces whose
    /// fixups refer to it.
    pub defs: Vec<CowVec<Def>>,
    pub refs: Vec<CowVec<Vec<PieceId>>>,
    pub specials: Vec<Special>,
    pub spaces: Vec<Space>,
    /// The names Rust code defines (`#[no_mangle]`, `main`), each the symbol it is.
    pub names: Arc<BTreeMap<String, Sym>>,
    /// Every name a piece has referred to: what resolution looks at.
    pub seen: Arc<BTreeSet<Sym>>,
    pub debug_names: Arc<FxHashMap<InstanceKey, Arc<str>>>,
    pub spans: Arc<FxHashMap<ContentId, Arc<SpanTable>>>,
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
    /// What the image is across its generations: its first full link's.
    pub lineage: [u8; 16],
    pub laid_out: bool,
}

impl State {
    pub fn new() -> State {
        State {
            generation: blaze_live::patch::FIRST_GENERATION,
            pieces: CowVec::default(),
            users: CowVec::default(),
            defs: vec![CowVec::default(); Symbols::SHARDS],
            refs: vec![CowVec::default(); Symbols::SHARDS],
            specials: Vec::new(),
            spaces: Vec::new(),
            names: Arc::default(),
            seen: Arc::default(),
            debug_names: Arc::default(),
            spans: Arc::default(),
            inputs: Arc::default(),
            resolved: Arc::default(),
            slots: 0,
            slot_syms: CowVec::default(),
            capacity: 0,
            tls: None,
            file_len: 0,
            parts: Arc::default(),
            lineage: [0; 16],
            laid_out: false,
        }
    }

    pub fn def(&self, sym: Sym) -> Def {
        let (shard, at) = Symbols::split(sym);
        self.defs[shard].get(at).copied().unwrap_or(Def::None)
    }

    pub fn set_def(&mut self, sym: Sym, def: Def) {
        let (shard, at) = Symbols::split(sym);
        self.defs[shard].set(at, def, || Def::None);
    }

    pub fn refs(&self, sym: Sym) -> &[PieceId] {
        let (shard, at) = Symbols::split(sym);
        self.refs[shard].get(at).map_or(&[], |it| it.as_slice())
    }

    fn refs_mut(&mut self, sym: Sym) -> &mut Vec<PieceId> {
        let (shard, at) = Symbols::split(sym);
        if self.refs[shard].len() <= at {
            self.refs[shard].set(at, Vec::new(), Vec::new);
        }
        self.refs[shard].get_mut(at).expect("grown to it")
    }

    pub fn piece(&self, id: PieceId) -> Option<&Piece> {
        self.pieces.get(id as usize).and_then(Option::as_ref)
    }

    pub fn piece_mut(&mut self, id: PieceId) -> Option<&mut Piece> {
        self.pieces.get_mut(id as usize).and_then(Option::as_mut)
    }

    /// The symbols a piece's fixups refer to, once each.
    pub fn targets(piece: &Piece) -> Vec<Sym> {
        let mut out: Vec<Sym> =
            piece.fixups.iter().flat_map(|it| std::iter::once(it.target).chain(it.minus)).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Adds a piece and its references; the caller defines its symbols.
    pub fn add(&mut self, piece: Piece) -> PieceId {
        let id = self.pieces.len() as PieceId;
        for target in Self::targets(&piece) {
            self.refs_mut(target).push(id);
        }
        self.pieces.push(Some(piece));
        self.users.push(0);
        id
    }

    /// Removes a piece, its references and its symbols' definitions.
    pub fn remove(&mut self, id: PieceId) -> Option<Piece> {
        let piece = self.pieces.get_mut(id as usize)?.take()?;
        for target in Self::targets(&piece) {
            self.refs_mut(target).retain(|it| *it != id);
        }
        for sym in std::iter::once(piece.sym).chain(piece.defines.iter().copied()) {
            if matches!(self.def(sym), Def::Piece { piece, .. } if piece == id) {
                self.set_def(sym, Def::None);
            }
        }
        Some(piece)
    }

    /// Replaces a piece's fixups, keeping the reverse index right.
    pub fn set_fixups(&mut self, id: PieceId, fixups: Arc<[Fixup]>) {
        let old = Self::targets(self.piece(id).expect("a live piece"));
        for target in old {
            self.refs_mut(target).retain(|it| *it != id);
        }
        let piece = self.piece_mut(id).expect("a live piece");
        piece.fixups = fixups;
        let new = Self::targets(piece);
        for target in new {
            self.refs_mut(target).push(id);
        }
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

    pub fn live_pieces(&self) -> impl Iterator<Item = (PieceId, &Piece)> {
        self.pieces.iter().enumerate().filter_map(|(id, it)| it.as_ref().map(|it| (id as PieceId, it)))
    }

    pub fn regions(&self) -> Vec<Region> {
        self.spaces.iter().map(|it| it.region.clone()).collect()
    }

    pub fn space_of(&self, class: &Class) -> Option<usize> {
        self.spaces.iter().position(|it| &it.region.class == class)
    }

    /// The bytes a placed piece has now.
    pub fn bytes(&self, piece: &Piece) -> &[u8] {
        let space = &self.spaces[piece.region as usize];
        if space.zero || piece.zero {
            return &[];
        }
        let at = (piece.addr - space.region.addr) as usize;
        &space.bytes[at..at + piece.size as usize]
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
