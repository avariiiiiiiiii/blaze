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
use blaze_link::{Format, Native, Output, OutputKind};

use crate::PremapError;
use crate::image::{self, Meta};
use crate::key::ImageKey;
use crate::table::{Def, Kind, Strength, SymbolId};

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
    pub names: &'a [(String, Target)],
    pub debug_names: &'a [(InstanceKey, String)],
    pub natives: &'a [Native<'a>],
    pub spans: &'a [(ContentId, &'a [u8])],
}

/// blaze-link, linking a dependency image.
pub trait Prelink: Sync {
    fn prelink(&self, request: &Request<'_>) -> Result<Prelinked, PremapError>;
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
/// link orders pieces by `rank` (`.init_array.NNNNN`'s priority; `u32::MAX` for none), then
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
            Err(PremapError::new("wasm has no dependency image: code is reached by index, not mapped".to_owned()))
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
        kind: OutputKind::Shared { name: key_name.to_owned(), exports: Vec::new() },
        interpreter: None,
        platform: dev.platform,
        incremental: false,
        debug: dev.debug,
        live: None,
        tags: dev.tags,
        stack: dev.stack,
        windows_subsystem: dev.windows_subsystem,
    }
}

/// The image of `request`, through `linker`, checked and encoded under `key`.
pub fn build(linker: &impl Prelink, request: &Request<'_>, key: ImageKey) -> Result<Vec<u8>, PremapError> {
    if request.output.incremental || request.output.live.is_some() {
        return Err(PremapError::new("a dependency image is linked whole: its output is neither incremental nor live".to_owned()));
    }
    let page = page(request.output.format, request.output.arch);
    if request.base % page != 0 || !IMAGES.contains(&request.base) {
        return Err(PremapError::new(format!(
            "the image's base {:#x} is not page-aligned in {:#x}..{:#x}",
            request.base, IMAGES.start, IMAGES.end
        )));
    }
    let linked = linker.prelink(request)?;
    check(request, &linked, page)?;

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
    let meta = Meta { key, format: request.output.format, arch: request.output.arch, base: request.base, page };
    image::encode(&meta, &linked, &entries)
}

/// What came back is what was asked, and maps.
fn check(request: &Request<'_>, linked: &Prelinked, page: u64) -> Result<(), PremapError> {
    let fail = |what: String| Err(PremapError::new(format!("the prelink of the dependency image {what}")));
    let mut end = request.base;
    for segment in &linked.segments {
        if segment.addr % page != 0 || segment.addr < end {
            return fail(format!("placed a segment at {:#x}, not page-aligned after {end:#x}", segment.addr));
        }
        if (segment.bytes.len() as u64) > segment.size || segment.size == 0 {
            return fail(format!(
                "made a segment at {:#x} of {} bytes in {}",
                segment.addr,
                segment.bytes.len(),
                segment.size
            ));
        }
        end = match segment.addr.checked_add(segment.size).filter(|end| *end <= IMAGES.end) {
            Some(end) => end,
            None => {
                return fail(format!(
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

    let mut placed: Vec<InstanceKey> = linked.functions.iter().map(|it| it.0).collect();
    placed.sort_unstable();
    for function in request.functions {
        if placed.binary_search(&function.key).is_err() {
            return fail(format!("placed no {:?}", function.key));
        }
    }
    for (key, addr, size) in &linked.functions {
        if !inside(*addr, (*size).max(1), false) {
            return fail(format!("placed {key:?} at {addr:#x}, outside its segments"));
        }
    }
    let mut statics: Vec<(ItemKey, ContentId)> = linked.statics.iter().map(|it| (it.item, it.instance)).collect();
    statics.sort_unstable();
    for value in request.statics {
        if statics.binary_search(&(value.item, value.instance)).is_err() {
            return fail(format!("placed no static {:?}", value.item));
        }
    }
    for bind in &linked.binds {
        if bind.place % 8 != 0 || !inside(bind.place, 8, true) {
            return fail(format!("binds `{}` at {:#x}, not an aligned cell of a writable segment", bind.name, bind.place));
        }
    }
    for entry in linked.init.iter().chain(&linked.fini) {
        if entry.at % 8 != 0 || !inside(entry.at, u64::from(entry.count) * 8, false) {
            return fail(format!("made an init or fini piece at {:#x} outside its segments", entry.at));
        }
    }
    if let Some(tls) = linked.tls {
        if tls.init > tls.size || !tls.align.is_power_of_two() || (tls.init > 0 && !inside(tls.template, tls.init, false)) {
            return fail(format!("made a thread-local template at {:#x} that is not one", tls.template));
        }
    }
    for (addr, len) in &linked.unwind {
        if !inside(*addr, *len, false) {
            return fail(format!("made call frame information at {addr:#x} outside its segments"));
        }
    }
    for (addr, len) in &linked.thread_vars {
        if addr % 8 != 0 || len % 24 != 0 || !inside(*addr, *len, true) {
            return fail(format!("made thread-local descriptors at {addr:#x} outside its writable segments"));
        }
    }
    Ok(())
}
