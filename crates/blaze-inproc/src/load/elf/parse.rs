//! The image's ELF header, program headers and dynamic section, read from untrusted bytes:
//! every offset and length is checked against the image and every failure is an error naming
//! the field. Views into the image are zero-copy slices of it.

#![forbid(unsafe_code)]

use std::ffi::CStr;
use std::ops::Range;

use crate::{Access, Error};

const EHDR: u64 = 64;
const PHDR: u64 = 56;
const SHDR: u64 = 64;
const SYM: u64 = 24;
pub(super) const RELA: u64 = 24;

const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

const PT_NULL: u32 = 0;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PT_SHLIB: u32 = 5;
const PT_PHDR: u32 = 6;
const PT_TLS: u32 = 7;
const PT_GNU_EH_FRAME: u32 = 0x6474_e550;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PT_GNU_RELRO: u32 = 0x6474_e552;
const PT_GNU_PROPERTY: u32 = 0x6474_e553;
const PT_GNU_SFRAME: u32 = 0x6474_e554;

const PF_X: u32 = 1;
const PF_W: u32 = 2;

pub(super) const STT_TLS: u8 = 6;
pub(super) const STT_GNU_IFUNC: u8 = 10;
pub(super) const STB_WEAK: u8 = 2;
pub(super) const SHN_UNDEF: u16 = 0;
pub(super) const SHN_ABS: u16 = 0xfff1;

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_PLTRELSZ: u64 = 2;
const DT_PLTGOT: u64 = 3;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_INIT: u64 = 12;
const DT_FINI: u64 = 13;
const DT_SONAME: u64 = 14;
const DT_RPATH: u64 = 15;
const DT_SYMBOLIC: u64 = 16;
const DT_REL: u64 = 17;
const DT_RELSZ: u64 = 18;
const DT_RELENT: u64 = 19;
const DT_PLTREL: u64 = 20;
const DT_DEBUG: u64 = 21;
const DT_TEXTREL: u64 = 22;
const DT_JMPREL: u64 = 23;
const DT_BIND_NOW: u64 = 24;
const DT_INIT_ARRAY: u64 = 25;
const DT_FINI_ARRAY: u64 = 26;
const DT_INIT_ARRAYSZ: u64 = 27;
const DT_FINI_ARRAYSZ: u64 = 28;
const DT_RUNPATH: u64 = 29;
const DT_FLAGS: u64 = 30;
const DT_PREINIT_ARRAY: u64 = 32;
const DT_PREINIT_ARRAYSZ: u64 = 33;
const DT_SYMTAB_SHNDX: u64 = 34;
const DT_RELRSZ: u64 = 35;
const DT_RELR: u64 = 36;
const DT_RELRENT: u64 = 37;
const DT_GNU_HASH: u64 = 0x6fff_fef5;
const DT_TLSDESC_PLT: u64 = 0x6fff_fef6;
const DT_TLSDESC_GOT: u64 = 0x6fff_fef7;
const DT_VERSYM: u64 = 0x6fff_fff0;
const DT_RELACOUNT: u64 = 0x6fff_fff9;
const DT_RELCOUNT: u64 = 0x6fff_fffa;
const DT_FLAGS_1: u64 = 0x6fff_fffb;
const DT_VERDEF: u64 = 0x6fff_fffc;
const DT_VERDEFNUM: u64 = 0x6fff_fffd;
const DT_VERNEED: u64 = 0x6fff_fffe;
const DT_VERNEEDNUM: u64 = 0x6fff_ffff;
const DT_AARCH64_BTI_PLT: u64 = 0x7000_0001;
const DT_AARCH64_PAC_PLT: u64 = 0x7000_0003;
const DT_AARCH64_VARIANT_PCS: u64 = 0x7000_0005;
const DT_AUXILIARY: u64 = 0x7fff_fffd;
const DT_FILTER: u64 = 0x7fff_ffff;

const DF_TEXTREL: u64 = 4;

const DW_EH_PE_OMIT: u8 = 0xff;

/// The architectures the loader runs: the image's has to be the host's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Machine {
    X86_64,
    Aarch64,
}

impl Machine {
    pub(super) fn host() -> Machine {
        if cfg!(target_arch = "aarch64") { Machine::Aarch64 } else { Machine::X86_64 }
    }

    pub(super) fn em(self) -> u16 {
        match self {
            Machine::X86_64 => EM_X86_64,
            Machine::Aarch64 => EM_AARCH64,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Machine::X86_64 => "x86-64",
            Machine::Aarch64 => "AArch64",
        }
    }
}

/// One `PT_LOAD`, its file offset its address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Load {
    /// Its program header's index, to name it.
    pub(super) index: u16,
    pub(super) vaddr: u64,
    pub(super) filesz: u64,
    pub(super) memsz: u64,
    pub(super) flags: u32,
}

impl Load {
    pub(super) fn executable(&self) -> bool {
        self.flags & PF_X != 0
    }

    pub(super) fn writable(&self) -> bool {
        self.flags & PF_W != 0
    }

    /// How it is mapped: code shared with the daemon's writable view (a patch is a store),
    /// writable data and RELRO as private copies, the rest shared read-only.
    pub(super) fn access(&self) -> Access {
        if self.executable() {
            Access::Execute
        } else if self.writable() {
            Access::Copy
        } else {
            Access::Read
        }
    }

    /// Checked at parse not to overflow.
    pub(super) fn end(&self) -> u64 {
        self.vaddr + self.memsz
    }

    pub(super) fn file_end(&self) -> u64 {
        self.vaddr + self.filesz
    }

    pub(super) fn describe(&self) -> String {
        let kind = match self.access() {
            Access::Execute => "executable",
            Access::Copy => "writable",
            Access::Read | Access::Shared => "read-only",
        };
        format!("the {kind} segment at {:#x} (program header {})", self.vaddr, self.index)
    }
}

/// The image's `PT_TLS`: its initialization image and block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Tls {
    pub(super) vaddr: u64,
    pub(super) filesz: u64,
    pub(super) memsz: u64,
    pub(super) align: u64,
}

/// An array of function addresses: `DT_INIT_ARRAY` and the like.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Array {
    pub(super) vaddr: u64,
    pub(super) count: u64,
}

/// What the dynamic section says, with the tables it names as views of the image.
#[derive(Default)]
pub(super) struct Dynamic<'a> {
    /// `DT_NEEDED`, as offsets in `strtab`, each checked to name a string.
    pub(super) needed: Vec<u32>,
    strtab: &'a [u8],
    /// From `DT_SYMTAB` to the end of its segment's file bytes: ELF gives no length.
    symtab: &'a [u8],
    /// From `DT_VERSYM` to the end of its segment's file bytes.
    versym: &'a [u8],
    /// `DT_VERNEED`'s versions: (index, name in `strtab`).
    versions: Vec<(u16, u32)>,
    pub(super) rela: Range<u64>,
    pub(super) jmprel: Range<u64>,
    pub(super) relr: Range<u64>,
    pub(super) init: Option<u64>,
    pub(super) fini: Option<u64>,
    pub(super) preinit_array: Option<Array>,
    pub(super) init_array: Option<Array>,
    pub(super) fini_array: Option<Array>,
}

/// One dynamic symbol.
#[derive(Clone, Copy, Debug)]
pub(super) struct Symbol<'a> {
    pub(super) name: &'a CStr,
    pub(super) bind: u8,
    pub(super) kind: u8,
    pub(super) shndx: u16,
    pub(super) value: u64,
}

impl Symbol<'_> {
    pub(super) fn defined(&self) -> bool {
        self.shndx != SHN_UNDEF
    }
}

/// A parsed image. Addresses are the image's own (its first `PT_LOAD` at 0); the loader adds
/// its base.
pub(super) struct Elf<'a> {
    bytes: &'a [u8],
    page: u64,
    pub(super) machine: Machine,
    pub(super) entry: u64,
    /// In address order, none sharing a page.
    pub(super) loads: Vec<Load>,
    /// The end of the last segment's memory, rounded up to a page.
    pub(super) span: u64,
    pub(super) tls: Option<Tls>,
    /// The pages made read-only once relocated, as glibc rounds them: both ends down.
    pub(super) relro: Option<Range<u64>>,
    /// `.eh_frame`, walked to its terminator.
    pub(super) eh_frame: Option<u64>,
    pub(super) dynamic: Dynamic<'a>,
}

fn bad(what: impl std::fmt::Display) -> Error {
    Error::new(format!("the ELF image: {what}"))
}

fn le<const N: usize>(bytes: &[u8], at: u64) -> Option<[u8; N]> {
    let at = usize::try_from(at).ok()?;
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

pub(super) fn u16_at(bytes: &[u8], at: u64) -> Option<u16> {
    le(bytes, at).map(u16::from_le_bytes)
}

pub(super) fn u32_at(bytes: &[u8], at: u64) -> Option<u32> {
    le(bytes, at).map(u32::from_le_bytes)
}

pub(super) fn u64_at(bytes: &[u8], at: u64) -> Option<u64> {
    le(bytes, at).map(u64::from_le_bytes)
}

fn phdr_type(p_type: u32) -> String {
    match p_type {
        PT_LOAD => "PT_LOAD".into(),
        PT_DYNAMIC => "PT_DYNAMIC".into(),
        PT_INTERP => "PT_INTERP".into(),
        PT_NOTE => "PT_NOTE".into(),
        PT_PHDR => "PT_PHDR".into(),
        PT_TLS => "PT_TLS".into(),
        PT_GNU_EH_FRAME => "PT_GNU_EH_FRAME".into(),
        PT_GNU_STACK => "PT_GNU_STACK".into(),
        PT_GNU_RELRO => "PT_GNU_RELRO".into(),
        PT_GNU_PROPERTY => "PT_GNU_PROPERTY".into(),
        PT_GNU_SFRAME => "PT_GNU_SFRAME".into(),
        other => format!("type {other:#x}"),
    }
}

/// One program header as the file has it.
struct Phdr {
    index: u16,
    p_type: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

impl Phdr {
    fn read(bytes: &[u8], phoff: u64, index: u16) -> Result<Phdr, Error> {
        let at = phoff + u64::from(index) * PHDR;
        let field = |offset: u64| u64_at(bytes, at + offset);
        let missing = || bad(format!("program header {index} at {at:#x} is past the image's {:#x} bytes", bytes.len()));
        Ok(Phdr {
            index,
            p_type: u32_at(bytes, at).ok_or_else(missing)?,
            flags: u32_at(bytes, at + 4).ok_or_else(missing)?,
            offset: field(8).ok_or_else(missing)?,
            vaddr: field(16).ok_or_else(missing)?,
            filesz: field(32).ok_or_else(missing)?,
            memsz: field(40).ok_or_else(missing)?,
            align: field(48).ok_or_else(missing)?,
        })
    }

    fn what(&self) -> String {
        format!("program header {} ({}) at {:#x}", self.index, phdr_type(self.p_type), self.vaddr)
    }

    /// The checks every header with bytes in the file passes: in the file, at a file offset
    /// equal to its address (blaze-link's layout), its memory no shorter than its file bytes,
    /// and an alignment of 0 or a power of two.
    fn in_file(&self, len: u64) -> Result<(), Error> {
        if self.offset != self.vaddr {
            return Err(bad(format!(
                "{} has file offset {:#x}, not its address: blaze-link lays a region's file offset at its address",
                self.what(),
                self.offset
            )));
        }
        if self.offset.checked_add(self.filesz).is_none_or(|end| end > len) {
            return Err(bad(format!(
                "{} has {:#x} file bytes at {:#x}, past the image's {len:#x}",
                self.what(),
                self.filesz,
                self.offset
            )));
        }
        if self.filesz > self.memsz {
            return Err(bad(format!(
                "{} has {:#x} file bytes but {:#x} of memory",
                self.what(),
                self.filesz,
                self.memsz
            )));
        }
        if self.vaddr.checked_add(self.memsz).is_none() {
            return Err(bad(format!("{} has memory of {:#x} bytes, past the address space", self.what(), self.memsz)));
        }
        if self.align != 0 && !self.align.is_power_of_two() {
            return Err(bad(format!("{} is aligned to {:#x}, not a power of two", self.what(), self.align)));
        }
        Ok(())
    }
}

/// Sets a dynamic tag's value once.
fn once(slot: &mut Option<u64>, value: u64, name: &str) -> Result<(), Error> {
    if slot.replace(value).is_some() {
        return Err(bad(format!("{name} appears twice in the dynamic section")));
    }
    Ok(())
}

/// The dynamic tags the loader reads, as the section gives them.
#[derive(Default)]
struct Tags {
    strtab: Option<u64>,
    strsz: Option<u64>,
    symtab: Option<u64>,
    syment: Option<u64>,
    rela: Option<u64>,
    relasz: Option<u64>,
    relaent: Option<u64>,
    jmprel: Option<u64>,
    pltrelsz: Option<u64>,
    pltrel: Option<u64>,
    relr: Option<u64>,
    relrsz: Option<u64>,
    relrent: Option<u64>,
    init: Option<u64>,
    fini: Option<u64>,
    preinit_array: Option<u64>,
    preinit_arraysz: Option<u64>,
    init_array: Option<u64>,
    init_arraysz: Option<u64>,
    fini_array: Option<u64>,
    fini_arraysz: Option<u64>,
    versym: Option<u64>,
    verneed: Option<u64>,
    verneednum: Option<u64>,
}

impl<'a> Elf<'a> {
    /// Parses `bytes`, the image as the linker laid out its file, for a loader of pages of
    /// `page` bytes on `machine`.
    pub(super) fn parse(bytes: &'a [u8], page: u64, machine: Machine) -> Result<Elf<'a>, Error> {
        let len = bytes.len() as u64;
        if len < EHDR {
            return Err(bad(format!("{len} bytes, shorter than an ELF header")));
        }
        if bytes[..4] != *b"\x7fELF" {
            return Err(bad(format!("magic {:02x?}, not \\x7fELF", &bytes[..4])));
        }
        if bytes[4] != 2 {
            return Err(bad(format!("class {}, not ELFCLASS64", bytes[4])));
        }
        if bytes[5] != 1 {
            return Err(bad(format!("data encoding {}, not little-endian", bytes[5])));
        }
        if bytes[6] != 1 {
            return Err(bad(format!("identification version {}, not EV_CURRENT", bytes[6])));
        }
        if bytes[7] != 0 && bytes[7] != 3 {
            return Err(bad(format!("OS ABI {}, not System V or GNU", bytes[7])));
        }
        if bytes[8] != 0 {
            return Err(bad(format!("ABI version {}, not 0", bytes[8])));
        }
        let half = |at: u64| u16_at(bytes, at).unwrap_or_default();
        let word = |at: u64| u32_at(bytes, at).unwrap_or_default();
        let wide = |at: u64| u64_at(bytes, at).unwrap_or_default();
        let e_type = half(16);
        if e_type != ET_DYN {
            return Err(bad(format!("type {e_type}, not ET_DYN (a position-independent image)")));
        }
        let e_machine = half(18);
        if e_machine != machine.em() {
            return Err(bad(format!("machine {e_machine}, not this host's {} ({})", machine.name(), machine.em())));
        }
        let e_version = word(20);
        if e_version != 1 {
            return Err(bad(format!("version {e_version}, not EV_CURRENT")));
        }
        let entry = wide(24);
        let phoff = wide(32);
        let shoff = wide(40);
        let flags = word(48);
        if flags != 0 {
            return Err(bad(format!("flags {flags:#x}, which neither x86-64 nor AArch64 defines")));
        }
        let ehsize = half(52);
        if u64::from(ehsize) != EHDR {
            return Err(bad(format!("an ELF header of {ehsize} bytes, not {EHDR}")));
        }
        let phentsize = half(54);
        if u64::from(phentsize) != PHDR {
            return Err(bad(format!("program headers of {phentsize} bytes, not {PHDR}")));
        }
        let phnum = half(56);
        if phnum == 0xffff {
            return Err(bad("PN_XNUM program headers"));
        }
        if phoff.checked_add(u64::from(phnum) * PHDR).is_none_or(|end| end > len) {
            return Err(bad(format!("{phnum} program headers at {phoff:#x}, past the image's {len:#x} bytes")));
        }
        let shentsize = half(58);
        let shnum = half(60);
        let shstrndx = half(62);
        if shnum != 0 {
            if u64::from(shentsize) != SHDR {
                return Err(bad(format!("section headers of {shentsize} bytes, not {SHDR}")));
            }
            if shoff.checked_add(u64::from(shnum) * SHDR).is_none_or(|end| end > len) {
                return Err(bad(format!("{shnum} section headers at {shoff:#x}, past the image's {len:#x} bytes")));
            }
            if shstrndx >= shnum {
                return Err(bad(format!("section name table {shstrndx} of {shnum} sections")));
            }
        }

        let mut elf = Elf {
            bytes,
            page,
            machine,
            entry,
            loads: Vec::with_capacity(4),
            span: 0,
            tls: None,
            relro: None,
            eh_frame: None,
            dynamic: Dynamic::default(),
        };
        let (mut dynamic, mut relro, mut eh_frame_hdr) = (None, None, None);
        for index in 0..phnum {
            let phdr = Phdr::read(bytes, phoff, index)?;
            match phdr.p_type {
                PT_NULL => {}
                PT_LOAD => {
                    phdr.in_file(len)?;
                    if !phdr.vaddr.is_multiple_of(page) {
                        return Err(bad(format!("{} does not start on a page of {page:#x}", phdr.what())));
                    }
                    if phdr.memsz == 0 {
                        return Err(bad(format!("{} is empty", phdr.what())));
                    }
                    if phdr.flags & (PF_W | PF_X) == PF_W | PF_X {
                        return Err(bad(format!("{} is both writable and executable", phdr.what())));
                    }
                    if phdr.flags & PF_W == 0 && phdr.memsz > phdr.filesz {
                        return Err(bad(format!(
                            "{} is not writable but has zeroed memory past its file bytes, which a shared mapping can't give",
                            phdr.what()
                        )));
                    }
                    if phdr.vaddr.checked_add(phdr.memsz).and_then(|it| it.checked_next_multiple_of(page)).is_none() {
                        return Err(bad(format!("{} ends past the address space", phdr.what())));
                    }
                    elf.loads.push(Load {
                        index,
                        vaddr: phdr.vaddr,
                        filesz: phdr.filesz,
                        memsz: phdr.memsz,
                        flags: phdr.flags,
                    });
                }
                PT_INTERP => {
                    return Err(bad(format!(
                        "{} asks for the system's dynamic loader: an image blaze's loader runs is linked with no \
                         interpreter and with `main` as its entry",
                        phdr.what()
                    )));
                }
                PT_SHLIB => return Err(bad(format!("{}, which no ABI defines", phdr.what()))),
                PT_GNU_STACK => {
                    if phdr.flags & PF_X != 0 {
                        return Err(bad(format!(
                            "{} asks for an executable stack, which the loader's process does not have",
                            phdr.what()
                        )));
                    }
                }
                PT_DYNAMIC | PT_TLS | PT_GNU_RELRO | PT_GNU_EH_FRAME => {
                    phdr.in_file(len)?;
                    let slot = match phdr.p_type {
                        PT_DYNAMIC => &mut dynamic,
                        PT_GNU_RELRO => &mut relro,
                        PT_GNU_EH_FRAME => &mut eh_frame_hdr,
                        _ => {
                            if elf.tls.is_some() {
                                return Err(bad(format!("{} is the second PT_TLS", phdr.what())));
                            }
                            elf.tls = Some(Tls {
                                vaddr: phdr.vaddr,
                                filesz: phdr.filesz,
                                memsz: phdr.memsz,
                                align: phdr.align.max(1),
                            });
                            continue;
                        }
                    };
                    if slot.replace((phdr.vaddr, phdr.filesz, phdr.memsz)).is_some() {
                        return Err(bad(format!("{} appears twice", phdr.what())));
                    }
                }
                PT_NOTE | PT_PHDR | PT_GNU_PROPERTY | PT_GNU_SFRAME => phdr.in_file(len)?,
                _ => return Err(bad(format!("{}, which the loader does not know", phdr.what()))),
            }
        }

        if elf.loads.is_empty() {
            return Err(bad("no PT_LOAD"));
        }
        for pair in elf.loads.windows(2) {
            let end = pair[0].end().next_multiple_of(page);
            if pair[1].vaddr < end {
                return Err(bad(format!(
                    "{} starts at {:#x}, before the pages of {} end at {end:#x}",
                    pair[1].describe(),
                    pair[1].vaddr,
                    pair[0].describe()
                )));
            }
        }
        elf.span = elf.loads.iter().map(|it| it.end().next_multiple_of(page)).max().unwrap_or(0);

        if !elf.code(entry, 1) {
            return Err(bad(format!("the entry {entry:#x} is not in the file bytes of an executable segment")));
        }
        if let Some(tls) = elf.tls {
            elf.file(tls.vaddr, tls.filesz).map_err(|it| bad(format!("PT_TLS's initialization image: {}", it.what)))?;
        }
        if let Some((vaddr, _, memsz)) = relro {
            let start = vaddr - vaddr % page;
            let end = (vaddr + memsz) / page * page;
            if end > start {
                // blaze-link rounds RELRO up to the next segment's alignment, which may reach
                // past this segment's pages into the gap the reservation keeps unmapped.
                let load = elf
                    .loads
                    .iter()
                    .find(|it| it.writable() && start >= it.vaddr && start < it.end())
                    .ok_or_else(|| bad(format!("PT_GNU_RELRO {start:#x}..{end:#x} does not start in a writable segment")))?;
                elf.relro = Some(start..end.min(load.end().next_multiple_of(page)));
            }
        }
        if let Some((vaddr, filesz, _)) = eh_frame_hdr {
            elf.eh_frame = elf.eh_frame(vaddr, filesz)?;
        }
        if let Some((vaddr, filesz, _)) = dynamic {
            elf.dynamic = elf.dynamic(vaddr, filesz)?;
        }
        Ok(elf)
    }

    /// `len` bytes at `vaddr`, in the file bytes of one segment.
    pub(super) fn file(&self, vaddr: u64, len: u64) -> Result<&'a [u8], Error> {
        let bytes: &'a [u8] = self.bytes;
        let end = vaddr.checked_add(len);
        let inside = |load: &&Load| vaddr >= load.vaddr && end.is_some_and(|end| end <= load.file_end());
        match (self.loads.iter().find(inside), end) {
            (Some(_), Some(end)) => Ok(&bytes[vaddr as usize..end as usize]),
            _ => Err(bad(format!("{len:#x} bytes at {vaddr:#x} are not in the file bytes of any segment"))),
        }
    }

    /// From `vaddr` to the end of its segment's file bytes.
    fn file_from(&self, vaddr: u64) -> Result<&'a [u8], Error> {
        let bytes: &'a [u8] = self.bytes;
        let load = self.loads.iter().find(|it| vaddr >= it.vaddr && vaddr < it.file_end());
        match load {
            Some(load) => Ok(&bytes[vaddr as usize..load.file_end() as usize]),
            None => Err(bad(format!("{vaddr:#x} is not in the file bytes of any segment"))),
        }
    }

    /// The segment whose memory holds `vaddr`.
    pub(super) fn load_at(&self, vaddr: u64) -> Option<&Load> {
        self.loads.iter().find(|it| vaddr >= it.vaddr && vaddr < it.end())
    }

    /// Whether `len` bytes at `vaddr` are in the file bytes of an executable segment.
    pub(super) fn code(&self, vaddr: u64, len: u64) -> bool {
        let end = vaddr.checked_add(len);
        self.loads.iter().any(|it| it.executable() && vaddr >= it.vaddr && end.is_some_and(|end| end <= it.file_end()))
    }

    /// The string at `offset` of the dynamic string table.
    pub(super) fn string(&self, offset: u32) -> Result<&'a CStr, Error> {
        string(self.dynamic.strtab, offset)
    }

    pub(super) fn symbol(&self, index: u32) -> Result<Symbol<'a>, Error> {
        let at = u64::from(index) * SYM;
        let table = self.dynamic.symtab;
        let field = |offset: u64| u64_at(table, at + offset);
        let missing = || bad(format!("symbol {index} is past the file bytes of DT_SYMTAB's segment"));
        let name = u32_at(table, at).ok_or_else(missing)?;
        let info = le::<1>(table, at + 4).ok_or_else(missing)?[0];
        let shndx = u16_at(table, at + 6).ok_or_else(missing)?;
        let value = field(8).ok_or_else(missing)?;
        Ok(Symbol { name: self.string(name)?, bind: info >> 4, kind: info & 0xf, shndx, value })
    }

    /// The version symbol `index` is imported at (`DT_VERSYM`, `DT_VERNEED`), if any.
    pub(super) fn version(&self, index: u32) -> Result<Option<&'a CStr>, Error> {
        if self.dynamic.versym.is_empty() {
            return Ok(None);
        }
        let version = u16_at(self.dynamic.versym, u64::from(index) * 2)
            .ok_or_else(|| bad(format!("symbol {index} is past DT_VERSYM's segment")))?
            & 0x7fff;
        if version < 2 {
            return Ok(None);
        }
        match self.dynamic.versions.iter().find(|it| it.0 == version) {
            Some(&(_, name)) => self.string(name).map(Some),
            None => Err(bad(format!("symbol {index} has version {version}, which DT_VERNEED does not name"))),
        }
    }

    /// `.eh_frame`, from the `.eh_frame_hdr` at `hdr`, walked to its zero terminator within
    /// the mapped pages of its segment (where libgcc's `__register_frame` stops reading).
    fn eh_frame(&self, hdr: u64, size: u64) -> Result<Option<u64>, Error> {
        let what = |it: String| bad(format!("PT_GNU_EH_FRAME at {hdr:#x}: {it}"));
        let bytes = self.file(hdr, size).map_err(|it| what(it.what))?;
        if bytes.len() < 4 {
            return Err(what(format!("{} bytes, shorter than an .eh_frame_hdr", bytes.len())));
        }
        if bytes[0] != 1 {
            return Err(what(format!("version {}, not 1", bytes[0])));
        }
        let encoding = bytes[1];
        if encoding == DW_EH_PE_OMIT {
            return Ok(None);
        }
        let value = pointer(bytes, 4, encoding).ok_or_else(|| what(format!("eh_frame_ptr encoding {encoding:#04x}")))?;
        let eh_frame = match encoding & 0x70 {
            0x00 => value,
            0x10 => (hdr + 4).wrapping_add(value),
            0x30 => hdr.wrapping_add(value),
            _ => return Err(what(format!("eh_frame_ptr encoding {encoding:#04x}"))),
        };
        let load = self
            .loads
            .iter()
            .find(|it| eh_frame >= it.vaddr && eh_frame < it.file_end())
            .ok_or_else(|| what(format!(".eh_frame at {eh_frame:#x} is not in the file bytes of any segment")))?;
        if load.writable() {
            return Err(what(format!(".eh_frame at {eh_frame:#x} is in {}, not a read-only one", load.describe())));
        }
        let end = load.file_end().next_multiple_of(self.page).min(self.bytes.len() as u64);
        let mut at = eh_frame;
        loop {
            let length = u32_at(&self.bytes[..end as usize], at).ok_or_else(|| {
                what(format!(
                    ".eh_frame at {eh_frame:#x} has no zero terminator before {end:#x}, the end of its segment's \
                     pages: libgcc's __register_frame would read past them"
                ))
            })?;
            if length == 0 {
                return Ok(Some(eh_frame));
            }
            let (header, length) = if length == u32::MAX {
                let extended = u64_at(&self.bytes[..end as usize], at + 4)
                    .ok_or_else(|| what(format!("the record at {at:#x} has its 64-bit length past {end:#x}")))?;
                (12, extended)
            } else {
                (4, u64::from(length))
            };
            at = at
                .checked_add(header)
                .and_then(|it| it.checked_add(length))
                .filter(|&it| it <= end)
                .ok_or_else(|| what(format!("the record at {at:#x} of {length:#x} bytes runs past {end:#x}")))?;
        }
    }

    fn dynamic(&self, vaddr: u64, size: u64) -> Result<Dynamic<'a>, Error> {
        let table = self.file(vaddr, size).map_err(|it| bad(format!("PT_DYNAMIC: {}", it.what)))?;
        if !size.is_multiple_of(16) {
            return Err(bad(format!("PT_DYNAMIC of {size:#x} bytes, not whole entries")));
        }
        let mut tags = Tags::default();
        let mut needed = Vec::new();
        let mut ended = false;
        for entry in table.as_chunks::<16>().0 {
            let tag = u64_at(entry, 0).unwrap_or_default();
            let value = u64_at(entry, 8).unwrap_or_default();
            let slot = match tag {
                DT_NULL => {
                    ended = true;
                    break;
                }
                DT_NEEDED => {
                    needed.push(
                        u32::try_from(value).map_err(|_| bad(format!("DT_NEEDED names string {value:#x}")))?,
                    );
                    continue;
                }
                DT_STRTAB => (&mut tags.strtab, "DT_STRTAB"),
                DT_STRSZ => (&mut tags.strsz, "DT_STRSZ"),
                DT_SYMTAB => (&mut tags.symtab, "DT_SYMTAB"),
                DT_SYMENT => (&mut tags.syment, "DT_SYMENT"),
                DT_RELA => (&mut tags.rela, "DT_RELA"),
                DT_RELASZ => (&mut tags.relasz, "DT_RELASZ"),
                DT_RELAENT => (&mut tags.relaent, "DT_RELAENT"),
                DT_JMPREL => (&mut tags.jmprel, "DT_JMPREL"),
                DT_PLTRELSZ => (&mut tags.pltrelsz, "DT_PLTRELSZ"),
                DT_PLTREL => (&mut tags.pltrel, "DT_PLTREL"),
                DT_RELR => (&mut tags.relr, "DT_RELR"),
                DT_RELRSZ => (&mut tags.relrsz, "DT_RELRSZ"),
                DT_RELRENT => (&mut tags.relrent, "DT_RELRENT"),
                DT_INIT => (&mut tags.init, "DT_INIT"),
                DT_FINI => (&mut tags.fini, "DT_FINI"),
                DT_PREINIT_ARRAY => (&mut tags.preinit_array, "DT_PREINIT_ARRAY"),
                DT_PREINIT_ARRAYSZ => (&mut tags.preinit_arraysz, "DT_PREINIT_ARRAYSZ"),
                DT_INIT_ARRAY => (&mut tags.init_array, "DT_INIT_ARRAY"),
                DT_INIT_ARRAYSZ => (&mut tags.init_arraysz, "DT_INIT_ARRAYSZ"),
                DT_FINI_ARRAY => (&mut tags.fini_array, "DT_FINI_ARRAY"),
                DT_FINI_ARRAYSZ => (&mut tags.fini_arraysz, "DT_FINI_ARRAYSZ"),
                DT_VERSYM => (&mut tags.versym, "DT_VERSYM"),
                DT_VERNEED => (&mut tags.verneed, "DT_VERNEED"),
                DT_VERNEEDNUM => (&mut tags.verneednum, "DT_VERNEEDNUM"),
                DT_FLAGS => {
                    if value & DF_TEXTREL != 0 {
                        return Err(bad("DT_FLAGS has DF_TEXTREL: relocations into code"));
                    }
                    continue;
                }
                DT_TEXTREL => return Err(bad("DT_TEXTREL: relocations into code")),
                DT_REL | DT_RELSZ | DT_RELENT | DT_RELCOUNT => {
                    return Err(bad(format!("dynamic tag {tag}: REL relocations, which x86-64 and AArch64 images don't use")));
                }
                DT_RPATH | DT_RUNPATH => {
                    return Err(bad(
                        "DT_RPATH or DT_RUNPATH: blaze's loader opens DT_NEEDED libraries by the system's search \
                         path alone",
                    ));
                }
                DT_AUXILIARY | DT_FILTER => return Err(bad("a filter library (DT_AUXILIARY or DT_FILTER)")),
                DT_PLTGOT | DT_HASH | DT_SONAME | DT_SYMBOLIC | DT_DEBUG | DT_BIND_NOW | DT_SYMTAB_SHNDX
                | DT_GNU_HASH | DT_TLSDESC_PLT | DT_TLSDESC_GOT | DT_RELACOUNT | DT_FLAGS_1 | DT_VERDEF
                | DT_VERDEFNUM | DT_AARCH64_BTI_PLT | DT_AARCH64_PAC_PLT | DT_AARCH64_VARIANT_PCS => continue,
                other => return Err(bad(format!("dynamic tag {other:#x}, which the loader does not know"))),
            };
            once(slot.0, value, slot.1)?;
        }
        if !ended {
            return Err(bad("PT_DYNAMIC has no DT_NULL"));
        }

        let mut dynamic = Dynamic { needed, ..Dynamic::default() };
        match (tags.strtab, tags.strsz) {
            (Some(at), Some(size)) => dynamic.strtab = self.file(at, size).map_err(|it| bad(format!("DT_STRTAB: {}", it.what)))?,
            (None, None) => {}
            _ => return Err(bad("DT_STRTAB without DT_STRSZ, or the other way")),
        }
        if let Some(at) = tags.symtab {
            if tags.syment != Some(SYM) {
                return Err(bad(format!("DT_SYMENT {:?}, not {SYM}", tags.syment)));
            }
            if dynamic.strtab.is_empty() {
                return Err(bad("DT_SYMTAB with no DT_STRTAB"));
            }
            dynamic.symtab = self.file_from(at).map_err(|it| bad(format!("DT_SYMTAB: {}", it.what)))?;
        }
        dynamic.rela = self.relocations(tags.rela, tags.relasz, tags.relaent, RELA, "DT_RELA")?;
        if tags.jmprel.is_some() && tags.pltrel != Some(DT_RELA) {
            return Err(bad(format!("DT_PLTREL {:?}, not DT_RELA", tags.pltrel)));
        }
        dynamic.jmprel = self.relocations(tags.jmprel, tags.pltrelsz, Some(RELA), RELA, "DT_JMPREL")?;
        dynamic.relr = self.relocations(tags.relr, tags.relrsz, tags.relrent, 8, "DT_RELR")?;
        for (at, name) in [(tags.init, "DT_INIT"), (tags.fini, "DT_FINI")] {
            if let Some(at) = at
                && !self.code(at, 1)
            {
                return Err(bad(format!("{name} {at:#x} is not in the file bytes of an executable segment")));
            }
        }
        dynamic.init = tags.init;
        dynamic.fini = tags.fini;
        dynamic.preinit_array = self.array(tags.preinit_array, tags.preinit_arraysz, "DT_PREINIT_ARRAY")?;
        dynamic.init_array = self.array(tags.init_array, tags.init_arraysz, "DT_INIT_ARRAY")?;
        dynamic.fini_array = self.array(tags.fini_array, tags.fini_arraysz, "DT_FINI_ARRAY")?;
        if let Some(at) = tags.versym {
            dynamic.versym = self.file_from(at).map_err(|it| bad(format!("DT_VERSYM: {}", it.what)))?;
        }
        if let Some(at) = tags.verneed {
            let count = tags.verneednum.ok_or_else(|| bad("DT_VERNEED with no DT_VERNEEDNUM"))?;
            dynamic.versions = self.versions(at, count)?;
        }

        for &name in &dynamic.needed {
            string(dynamic.strtab, name).map_err(|it| bad(format!("DT_NEEDED: {}", it.what)))?;
        }
        for &(_, name) in &dynamic.versions {
            string(dynamic.strtab, name).map_err(|it| bad(format!("DT_VERNEED: {}", it.what)))?;
        }
        Ok(dynamic)
    }

    /// A relocation table's range, in the file bytes of one segment, of whole entries.
    fn relocations(
        &self,
        at: Option<u64>,
        size: Option<u64>,
        entry: Option<u64>,
        want: u64,
        name: &str,
    ) -> Result<Range<u64>, Error> {
        let Some(at) = at else { return Ok(0..0) };
        let size = size.ok_or_else(|| bad(format!("{name} with no size")))?;
        if entry != Some(want) {
            return Err(bad(format!("{name} entries of {entry:?} bytes, not {want}")));
        }
        if !size.is_multiple_of(want) {
            return Err(bad(format!("{name} of {size:#x} bytes, not whole entries of {want}")));
        }
        self.file(at, size).map_err(|it| bad(format!("{name}: {}", it.what)))?;
        Ok(at..at + size)
    }

    fn array(&self, at: Option<u64>, size: Option<u64>, name: &str) -> Result<Option<Array>, Error> {
        let (at, size) = match (at, size) {
            (None, None) => return Ok(None),
            (Some(at), Some(size)) => (at, size),
            _ => return Err(bad(format!("{name} without its size, or the other way"))),
        };
        if !size.is_multiple_of(8) {
            return Err(bad(format!("{name} of {size:#x} bytes, not whole addresses")));
        }
        let inside = self.load_at(at).is_some_and(|load| at.checked_add(size).is_some_and(|end| end <= load.end()));
        if size != 0 && !inside {
            return Err(bad(format!("{name} {at:#x} of {size:#x} bytes is not in one segment")));
        }
        Ok((size != 0).then_some(Array { vaddr: at, count: size / 8 }))
    }

    /// `DT_VERNEED`'s `count` entries and their versions.
    fn versions(&self, at: u64, count: u64) -> Result<Vec<(u16, u32)>, Error> {
        let table = self.file_from(at).map_err(|it| bad(format!("DT_VERNEED: {}", it.what)))?;
        let missing = |at: u64| bad(format!("DT_VERNEED's entry at {at:#x} is past its segment's file bytes"));
        let mut versions = Vec::new();
        let mut entry = 0u64;
        for _ in 0..count {
            let version = u16_at(table, entry).ok_or_else(|| missing(entry))?;
            if version != 1 {
                return Err(bad(format!("DT_VERNEED's entry at {entry:#x} has version {version}, not 1")));
            }
            let cnt = u16_at(table, entry + 2).ok_or_else(|| missing(entry))?;
            let aux = u32_at(table, entry + 8).ok_or_else(|| missing(entry))?;
            let next = u32_at(table, entry + 12).ok_or_else(|| missing(entry))?;
            let mut at = entry + u64::from(aux);
            for _ in 0..cnt {
                let other = u16_at(table, at + 6).ok_or_else(|| missing(at))?;
                let name = u32_at(table, at + 8).ok_or_else(|| missing(at))?;
                let following = u32_at(table, at + 12).ok_or_else(|| missing(at))?;
                versions.push((other & 0x7fff, name));
                if following == 0 {
                    break;
                }
                at += u64::from(following);
            }
            if next == 0 {
                break;
            }
            entry += u64::from(next);
        }
        Ok(versions)
    }
}

fn string(strtab: &[u8], offset: u32) -> Result<&CStr, Error> {
    strtab.get(offset as usize..).and_then(|it| CStr::from_bytes_until_nul(it).ok()).ok_or_else(|| {
        bad(format!("string {offset:#x} is not a NUL-terminated string of the {:#x}-byte DT_STRTAB", strtab.len()))
    })
}

/// A DWARF pointer encoded as `encoding` at `at` in `bytes`: the value formats an
/// `.eh_frame_hdr` uses.
fn pointer(bytes: &[u8], at: u64, encoding: u8) -> Option<u64> {
    if encoding & 0x80 != 0 {
        return None;
    }
    match encoding & 0x0f {
        0x00 | 0x04 | 0x0c => u64_at(bytes, at),
        0x03 => u32_at(bytes, at).map(u64::from),
        0x0b => u32_at(bytes, at).map(|it| it as i32 as i64 as u64),
        _ => None,
    }
}
