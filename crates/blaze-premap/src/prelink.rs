//! The prelink: the image's pieces placed and relocated by blaze-link at a fixed base, as a
//! shared image whose undefined and weakly defined names are bound by the run
//! ([`Prelinked::binds`]), with every function, static and global name in a table.
//!
//! [`Prelink`] is the hook into blaze-link (`v2/requests/fastcompile.md`, fastcompile-premap
//! 1): blaze-link's stage runs it, in parallel as any full link is. [`build`] asks for it,
//! checks what came back against what was asked (every piece placed, segments page-aligned
//! and apart, every bind and init cell inside a writable segment) and encodes the image.

use blaze_codegen::{Arch, CompiledFunction, InstanceKey, Static, Target};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_link::premap as link;
use blaze_link::{Format, Native, Output, OutputKind};
use rayon::prelude::*;

use crate::PremapError;
use crate::image::{self, Meta, Pieces};
use crate::key::ImageKey;
use crate::table::{self, Def, Kind, Strength, SymbolId, TableView};

/// What the prelink links.
pub struct Request<'a> {
    /// The output it links as ([`output`]): a shared image, never incremental or live.
    pub output: &'a Output,
    /// The address of the image's first byte: it is never slid.
    pub base: u64,
    /// Where the run's loader keeps the program's thread-locals ([`blaze_inproc::TlsReserve`]):
    /// the image's are at `tls_bias + offset`, from offset 0 (from the thread pointer on ELF,
    /// from the start of the module's block on PE; 0 on Mach-O, whose descriptors hold the
    /// offset into the template).
    pub tls_bias: i64,
    pub functions: &'a [&'a CompiledFunction],
    pub statics: &'a [&'a Static],
    pub names: &'a [&'a (String, Target)],
    pub debug_names: &'a [&'a (InstanceKey, String)],
    pub natives: &'a [Native<'a>],
    pub spans: &'a [(ContentId, &'a [u8])],
    /// Codegen's digests of `functions`, `statics` and `debug_names`, in their order: kept in
    /// the image so a build checks its pieces against it with no hash of its own.
    pub function_digests: &'a [ContentId],
    pub static_digests: &'a [ContentId],
    pub debug_digests: &'a [ContentId],
}

/// blaze-link, linking a dependency image.
pub trait Prelink: Sync {
    type Placed: Placed;
    fn prelink(&self, request: &Request<'_>) -> Result<Self::Placed, PremapError>;
}

/// A prelink placed and relocated, its segments' bytes written only into the image's value,
/// so a cold prelink writes them once.
pub trait Placed {
    /// What the prelink says; its segments' `bytes` may be empty, [`Placed::len`] says how
    /// many it has.
    fn linked(&self) -> &Prelinked;
    /// How many bytes segment `at` has from its start; zeroes follow to its size.
    fn len(&self, at: usize) -> u64;
    /// Writes each segment's bytes at its offset of `out`: one offset per segment, increasing,
    /// each a multiple of the page and its segment inside `out`.
    fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), PremapError>;
}

/// A prelink whose segments hold their bytes: written by a copy of each.
impl Placed for Prelinked {
    fn linked(&self) -> &Prelinked {
        self
    }

    fn len(&self, at: usize) -> u64 {
        self.segments[at].bytes.len() as u64
    }

    fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), PremapError> {
        if offsets.len() != self.segments.len() {
            return Err(PremapError::of(format_args!("{} offsets for {} segments", offsets.len(), self.segments.len())));
        }
        let mut rest = out;
        let mut consumed = 0;
        for (segment, offset) in self.segments.iter().zip(offsets) {
            let len = segment.bytes.len();
            let fits = *offset >= consumed && offset.checked_add(len).is_some_and(|end| end - consumed <= rest.len());
            if !fits {
                return Err(PremapError::of(format_args!(
                    "the segment at {:#x} does not fit at offset {offset} of the image",
                    segment.addr
                )));
            }
            let (_, from) = std::mem::take(&mut rest).split_at_mut(offset - consumed);
            let (into, after) = from.split_at_mut(len);
            into.par_chunks_mut(COPY_PIECE)
                .zip(segment.bytes.par_chunks(COPY_PIECE))
                .for_each(|(into, from)| into.copy_from_slice(from));
            rest = after;
            consumed = offset + len;
        }
        Ok(())
    }
}

/// Segment bytes are copied in pieces of this size, in parallel.
const COPY_PIECE: usize = 1 << 20;

/// blaze-link's prelink ([`blaze_link::premap::prelink_placed`]), whose types mirror these
/// field for field.
#[derive(Clone, Copy, Default, Debug)]
pub struct Link;

/// [`Link`]'s answer: blaze-link's placed prelink, which writes the segments itself.
pub struct LinkPlaced {
    placed: link::Placed,
    linked: Prelinked,
}

impl Placed for LinkPlaced {
    fn linked(&self) -> &Prelinked {
        &self.linked
    }

    fn len(&self, at: usize) -> u64 {
        self.placed.segments[at].len
    }

    fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), PremapError> {
        self.placed
            .write_into(out, offsets)
            .map_err(|error| PremapError::of(format_args!("writing the dependency image's segments: {error}")))
    }
}

impl Prelink for Link {
    type Placed = LinkPlaced;

    fn prelink(&self, request: &Request<'_>) -> Result<LinkPlaced, PremapError> {
        // alloc: once per image made: blaze-link's request takes the names as values
        let names: Vec<(String, Target)> = request.names.iter().map(|it| (*it).clone()).collect(); // clone: as above
        // alloc: once per image made, as above
        let debug_names: Vec<(InstanceKey, String)> = request.debug_names.iter().map(|it| (*it).clone()).collect(); // clone: as above
        let mut placed = link::prelink_placed(&link::Request {
            output: request.output,
            base: request.base,
            tls_bias: request.tls_bias,
            functions: request.functions,
            statics: request.statics,
            names: &names,
            debug_names: &debug_names,
            natives: request.natives,
            spans: request.spans,
        })
        .map_err(|error| PremapError::of(format_args!("the prelink of a dependency image: {error}")))?;
        let mut linked = from_link(std::mem::take(&mut placed.linked));
        let place = |it: &link::SegmentPlace| Segment {
            addr: it.addr,
            size: it.size,
            prot: prot_of_link(it.prot),
            bytes: Vec::new(), // alloc: none: the bytes are written by `write_into`, so the list stays empty
        };
        linked.segments = placed.segments.iter().map(place).collect(); // alloc: once per image made: its segments' places
        Ok(LinkPlaced { placed, linked })
    }
}

fn prot_of_link(prot: link::Prot) -> Prot {
    match prot {
        link::Prot::Code => Prot::Code,
        link::Prot::ReadOnly => Prot::ReadOnly,
        link::Prot::Data => Prot::Data,
    }
}

/// What blaze-link gave back, as premap's types.
fn from_link(linked: link::Prelinked) -> Prelinked {
    let entry = |it: link::InitEntry| InitEntry { rank: it.rank, order: it.order, tie: it.tie, at: it.at, count: it.count };
    let tls = |it: link::Tls| Tls { template: it.template, init: it.init, size: it.size, align: it.align };
    let at = |it: link::StaticAt| match it {
        link::StaticAt::Addr(addr) => StaticAt::Addr(addr),
        link::StaticAt::ThreadLocal(offset) => StaticAt::ThreadLocal(offset),
    };
    let bind = |it: link::BindKind| match it {
        link::BindKind::Addr => BindKind::Addr,
        link::BindKind::TpOff => BindKind::TpOff,
    };
    Prelinked {
        segments: Vec::new(), // alloc: none: the segments come from `Placed::segments`
        functions: linked.functions,
        statics: linked
            .statics
            .into_iter()
            .map(|it| PlacedStatic { item: it.item, instance: it.instance, at: at(it.at), size: it.size })
            .collect(), // alloc: once per image made, as above
        names: linked.names.into_iter().map(|it| PlacedName { name: it.name, def: def_of_link(it.def) }).collect(), // alloc: as above
        binds: linked
            .binds
            .into_iter()
            .map(|it| Bind { place: it.place, name: it.name, addend: it.addend, kind: bind(it.kind), weak: it.weak })
            .collect(), // alloc: once per image made, as above
        libraries: linked
            .libraries
            .into_iter()
            .map(|it| Library { name: it.name, order: it.order, as_needed: it.as_needed })
            .collect(), // alloc: once per image made, as above
        init: linked.init.into_iter().map(entry).collect(), // alloc: once per image made, as above
        fini: linked.fini.into_iter().map(entry).collect(), // alloc: once per image made, as above
        tls: linked.tls.map(tls),
        unwind: linked.unwind,
        thread_vars: linked.thread_vars,
        reached_directly: linked.reached_directly,
    }
}

/// blaze-link's definition of a name, as the table keeps it.
pub(crate) fn def_of_link(def: link::Def) -> Def {
    let kind = match def.kind {
        link::Kind::Function => Kind::Function,
        link::Kind::Data => Kind::Data,
        link::Kind::ThreadLocal => Kind::ThreadLocal,
    };
    let strength = match def.strength {
        link::Strength::Strong => Strength::Strong,
        link::Strength::Weak => Strength::Weak,
    };
    Def { value: def.value, size: def.size, kind, strength, order: def.order }
}

/// A table definition, as blaze-link takes it: the inverse of [`def_of_link`].
pub(crate) fn link_def(def: Def) -> link::Def {
    let kind = match def.kind {
        Kind::Function => link::Kind::Function,
        Kind::Data => link::Kind::Data,
        Kind::ThreadLocal => link::Kind::ThreadLocal,
    };
    let strength = match def.strength {
        Strength::Strong => link::Strength::Strong,
        Strength::Weak => link::Strength::Weak,
    };
    link::Def { value: def.value, size: def.size, kind, strength, order: def.order }
}

/// A run of the address space with one protection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Prot {
    /// Read and execute, shared by every run.
    Code,
    /// Read only, shared.
    ReadOnly,
    /// Read and write: each run gets its own copy of the page on its first write.
    Data,
}

/// A segment of the image: `bytes` from `addr`, then zeroes to `size`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Segment {
    pub addr: u64,
    pub size: u64,
    pub prot: Prot,
    pub bytes: Vec<u8>,
}

/// Where a static is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StaticAt {
    Addr(u64),
    /// A thread-local: its offset from the thread pointer.
    ThreadLocal(u64),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlacedStatic {
    pub item: ItemKey,
    pub instance: ContentId,
    pub at: StaticAt,
    pub size: u64,
}

/// A global name the image defines: Rust's (`#[no_mangle]`) and every global of a native
/// object in it, so a workspace reference finds it here and never pulls a second copy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlacedName {
    pub name: String,
    pub def: Def,
}

/// How a bound cell is written.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BindKind {
    /// The address of the symbol plus the addend, 64 bits.
    Addr,
    /// The symbol's offset from the thread pointer plus the addend, 64 bits.
    TpOff,
}

/// An 8-byte cell (a GOT entry, a pointer in data) of a writable segment the run writes
/// before anything runs: a name the image does not define, or defines only weakly (so a
/// strong definition in the workspace wins, as in a whole link).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Bind {
    pub place: u64,
    pub name: String,
    pub addend: i64,
    pub kind: BindKind,
    /// Referred to weakly: 0 when nothing defines it.
    pub weak: bool,
}

/// A shared library the image imports from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Library {
    /// What the loader finds it by (`DT_SONAME`, install name, DLL name).
    pub name: String,
    /// Its place on the link line.
    pub order: u32,
    /// Left out when none of its symbols is used.
    pub as_needed: bool,
}

/// One piece of an init or fini array: `count` 8-byte function pointers at `at`. A whole
/// link orders pieces by `rank` (`.init_array.NNNNN`'s priority; 65536 for none), then
/// by `order` (the input's place on the line, its member and its section; `u32::MAX` each
/// for Rust's pieces, which go after natives), then by `tie` (Rust's: the static's item and
/// instance); within a piece the pointers run in address order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InitEntry {
    pub rank: u32,
    pub order: [u32; 3],
    pub tie: [u8; 64],
    pub at: u64,
    pub count: u32,
}

impl InitEntry {
    pub fn sort_key(&self) -> (u32, [u32; 3], [u8; 64]) {
        (self.rank, self.order, self.tie)
    }

    /// The tie of a Rust static's piece.
    pub fn tie_of(item: &ItemKey, instance: &ContentId) -> [u8; 64] {
        let mut tie = [0; 64];
        tie[..32].copy_from_slice(&item.0.0);
        tie[32..].copy_from_slice(&instance.0);
        tie
    }
}

/// The thread-local template: initialized bytes at `template`, `init` long, then zeroes to
/// `size`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tls {
    pub template: u64,
    pub init: u64,
    pub size: u64,
    pub align: u64,
}

/// What blaze-link gives back.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Prelinked {
    /// In address order.
    pub segments: Vec<Segment>,
    pub functions: Vec<(InstanceKey, u64, u64)>,
    pub statics: Vec<PlacedStatic>,
    pub names: Vec<PlacedName>,
    pub binds: Vec<Bind>,
    pub libraries: Vec<Library>,
    pub init: Vec<InitEntry>,
    pub fini: Vec<InitEntry>,
    pub tls: Option<Tls>,
    /// Call frame information (`.eh_frame`, `__eh_frame`) to register, as address and length.
    pub unwind: Vec<(u64, u64)>,
    /// Mach-O: the thread-local descriptors (`__thread_vars`) the run's loader fills.
    pub thread_vars: Vec<(u64, u64)>,
    /// The names the image defines weakly and its own code reaches other than through a
    /// bind, so no definition beside the image may win over them. Sorted, each once.
    pub reached_directly: Vec<String>,
}

/// The addresses dependency images and the workspace images beside them take: just above
/// where run processes map other code memory ([`blaze_inproc::BASES`]), so never in its way,
/// and below the top of a 39-bit address space, the smallest AArch64 Linux configures.
pub const IMAGES: std::ops::Range<u64> = blaze_inproc::BASES.end..0x80_0000_0000;

/// The address every image of an output is placed at: the start of [`IMAGES`], aligned to
/// 64 KiB, a multiple of every target's page. The workspace image follows it, within reach
/// of a call.
pub fn base(format: Format, arch: Arch) -> Result<u64, PremapError> {
    match (format, arch) {
        (Format::Wasm, _) | (_, Arch::Wasm32) => {
            Err(PremapError::of(format_args!("wasm has no dependency image: code is reached by index, not mapped")))
        }
        _ => Ok(IMAGES.start),
    }
}

/// The granule every segment is aligned to: the largest a system of the target maps a memory
/// object by (16 KiB pages on Apple silicon, 64 KiB pages on AArch64 Linux kernels built so,
/// Windows' 64 KiB allocation granularity for a view's address and offset).
pub fn page(format: Format, arch: Arch) -> u64 {
    match (format, arch) {
        (Format::MachO, Arch::Aarch64) => 16 << 10,
        (Format::Pe, _) | (_, Arch::Aarch64) => 64 << 10,
        _ => 4 << 10,
    }
}

/// The output a dependency image links as, from the dev run's: a shared image under the
/// key's name, of the same format, target, platform and debug info, with no room to grow and
/// no live patching (an image is replaced, never patched).
pub fn output(dev: &Output, key_name: &str) -> Output {
    Output {
        format: dev.format,
        arch: dev.arch,
        kind: OutputKind::Shared { name: key_name.to_owned(), exports: Vec::new() }, // alloc: setup, once per image made
        interpreter: None,
        platform: dev.platform,
        incremental: false,
        debug: dev.debug,
        live: None,
        tags: dev.tags,
        stack: dev.stack,
        windows_subsystem: dev.windows_subsystem,
        dev_run: None,
        triple: dev.triple.clone(), // clone: setup, once per image made: the output owns it
        exec_log: false,
        sanitize: dev.sanitize,
        rpaths: dev.rpaths.clone(), // clone: setup, once per image made: the output owns them
    }
}

/// The image of `request`, through `linker`, checked and encoded under `key`.
pub fn build(linker: &impl Prelink, request: &Request<'_>, key: ImageKey) -> Result<Vec<u8>, PremapError> {
    if request.output.incremental || request.output.live.is_some() {
        return Err(PremapError::of(format_args!(
            "a dependency image is linked whole: its output is neither incremental nor live"
        )));
    }
    let page = page(request.output.format, request.output.arch);
    if request.base % page != 0 || !IMAGES.contains(&request.base) {
        return Err(PremapError::of(format_args!(
            "the image's base {:#x} is not page-aligned in {:#x}..{:#x}",
            request.base, IMAGES.start, IMAGES.end
        )));
    }
    let placed = linker.prelink(request)?;
    check(request, &placed, page)?;
    let linked = placed.linked();

    let mut entries: Vec<(SymbolId, Def)> =
        Vec::with_capacity(linked.functions.len() + linked.statics.len() + linked.names.len());
    for (key, addr, size) in &linked.functions {
        let def = Def { value: *addr, size: *size, kind: Kind::Function, strength: Strength::Strong, order: u32::MAX };
        entries.push((SymbolId::function(key), def));
    }
    for placed in &linked.statics {
        let (value, kind) = match placed.at {
            StaticAt::Addr(addr) => (addr, Kind::Data),
            StaticAt::ThreadLocal(offset) => (offset, Kind::ThreadLocal),
        };
        let def = Def { value, size: placed.size, kind, strength: Strength::Strong, order: u32::MAX };
        entries.push((SymbolId::of_static(&placed.item, &placed.instance), def));
    }
    for name in &linked.names {
        entries.push((SymbolId::name(&name.name), name.def));
    }
    let mut table = table::build(&entries)?;
    let fail = |what: std::fmt::Arguments<'_>| Err(PremapError::of(format_args!("the prelink of the dependency image {what}")));
    let placed = |table: &Vec<u8>, id: &SymbolId| TableView::trusted(table).get(id).is_some();

    if request.function_digests.len() != request.functions.len()
        || request.static_digests.len() != request.statics.len()
        || request.debug_digests.len() != request.debug_names.len()
    {
        return Err(PremapError::of(format_args!(
            "the dependency image was asked with digests not one per function, static and debug name"
        )));
    }
    let mut contents = Vec::with_capacity(request.functions.len() + request.statics.len());
    for (function, digest) in request.functions.iter().zip(request.function_digests) {
        let id = SymbolId::function(&function.key);
        if !placed(&table, &id) {
            return fail(format_args!("placed no {:?}", function.key));
        }
        contents.push(*digest);
        table::set_content(&mut table, &id, contents.len() as u32)?;
    }
    let mut statics = Vec::with_capacity(request.statics.len());
    for (value, definition) in request.statics.iter().zip(request.static_digests) {
        let id = SymbolId::of_static(&value.item, &value.instance);
        if !placed(&table, &id) {
            return fail(format_args!("placed no static {:?}", value.item));
        }
        contents.push(*definition);
        table::set_content(&mut table, &id, contents.len() as u32)?;
        statics.push(((value.item, value.instance), *definition));
    }
    statics.sort_unstable_by_key(|it| it.0);
    for (name, _) in request.names {
        if !placed(&table, &SymbolId::name(name)) {
            return fail(format_args!("defined no `{name}`"));
        }
    }
    for (at, (function, _)) in request.debug_names.iter().enumerate() {
        table::set_debug(&mut table, &SymbolId::function(function), at as u32 + 1)?;
    }
    for name in &linked.reached_directly {
        table::set_reached_directly(&mut table, &SymbolId::name(name))?;
    }
    let meta = Meta { key, format: request.output.format, arch: request.output.arch, base: request.base, page };
    let pieces = Pieces {
        table: &table,
        contents: &contents,
        debug: request.debug_digests,
        statics: &statics,
        names: request.names.len() as u64,
    };
    image::encode(&meta, &placed, &pieces)
}

/// What came back is what was asked, and maps.
fn check(request: &Request<'_>, placed: &impl Placed, page: u64) -> Result<(), PremapError> {
    let fail = |what: std::fmt::Arguments<'_>| Err(PremapError::of(format_args!("the prelink of the dependency image {what}")));
    let linked = placed.linked();
    let mut end = request.base;
    for (at, segment) in linked.segments.iter().enumerate() {
        if segment.addr % page != 0 || segment.addr < end {
            return fail(format_args!("placed a segment at {:#x}, not page-aligned after {end:#x}", segment.addr));
        }
        if placed.len(at) > segment.size || segment.size == 0 {
            return fail(format_args!("made a segment at {:#x} of {} bytes in {}", segment.addr, placed.len(at), segment.size));
        }
        end = match segment.addr.checked_add(segment.size).filter(|end| *end <= IMAGES.end) {
            Some(end) => end,
            None => {
                return fail(format_args!(
                    "made a segment at {:#x} reaching past {:#x}, where run processes map images",
                    segment.addr, IMAGES.end
                ));
            }
        };
    }
    let inside = |addr: u64, len: u64, writable: bool| {
        linked.segments.iter().any(|it| {
            addr >= it.addr && addr.saturating_add(len) <= it.addr + it.size && (!writable || it.prot == Prot::Data)
        })
    };

    for (key, addr, size) in &linked.functions {
        if !inside(*addr, (*size).max(1), false) {
            return fail(format_args!("placed {key:?} at {addr:#x}, outside its segments"));
        }
    }
    for bind in &linked.binds {
        if bind.place % 8 != 0 || !inside(bind.place, 8, true) {
            return fail(format_args!("binds `{}` at {:#x}, not an aligned cell of a writable segment", bind.name, bind.place));
        }
    }
    for entry in linked.init.iter().chain(&linked.fini) {
        if entry.at % 8 != 0 || !inside(entry.at, u64::from(entry.count) * 8, false) {
            return fail(format_args!("made an init or fini piece at {:#x} outside its segments", entry.at));
        }
    }
    if let Some(tls) = linked.tls {
        if tls.init > tls.size || !tls.align.is_power_of_two() || (tls.init > 0 && !inside(tls.template, tls.init, false)) {
            return fail(format_args!("made a thread-local template at {:#x} that is not one", tls.template));
        }
    }
    for (addr, len) in &linked.unwind {
        if !inside(*addr, *len, false) {
            return fail(format_args!("made call frame information at {addr:#x} outside its segments"));
        }
    }
    for (addr, len) in &linked.thread_vars {
        if addr % 8 != 0 || len % 24 != 0 || !inside(*addr, *len, true) {
            return fail(format_args!("made thread-local descriptors at {addr:#x} outside its writable segments"));
        }
    }
    Ok(())
}
