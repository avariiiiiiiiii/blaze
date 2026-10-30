//! The image's Mach-O header and load commands, read from untrusted bytes: every offset and
//! length is checked against the image and every failure is an error naming the field. Views
//! into the image are zero-copy slices of it.
//!
//! What is accepted is what blaze-link writes for blaze's loader and dyld would run the same: a
//! position-independent arm64 `MH_EXECUTE` with `LC_MAIN`, its segments each at a file offset
//! equal to its address from the first one's (the one holding the header), fixups as dyld's
//! opcode tables (`LC_DYLD_INFO_ONLY`). What only dyld can do for an image (chained fixups,
//! interposing, Objective-C and Swift metadata, a re-exported library) is an error naming it.

#![forbid(unsafe_code)]

use std::ffi::CStr;
use std::ops::Range;

use crate::{Access, Error};

const MH_MAGIC_64: u32 = 0xfeed_facf;
const HEADER: usize = 32;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_SUBTYPE_MASK: u32 = 0x00ff_ffff;
const CPU_SUBTYPE_ARM64E: u32 = 2;
const MH_EXECUTE: u32 = 2;
const MH_PIE: u32 = 0x0020_0000;

const LC_REQ_DYLD: u32 = 0x8000_0000;
const LC_SEGMENT_64: u32 = 0x19;
const LC_UNIXTHREAD: u32 = 0x5;
const LC_LOAD_DYLIB: u32 = 0xc;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_DYLD_ENVIRONMENT: u32 = 0x27;
const LC_ENCRYPTION_INFO_64: u32 = 0x2c;
const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x22 | LC_REQ_DYLD;
const LC_LOAD_WEAK_DYLIB: u32 = 0x18 | LC_REQ_DYLD;
const LC_RPATH: u32 = 0x1c | LC_REQ_DYLD;
const LC_REEXPORT_DYLIB: u32 = 0x1f | LC_REQ_DYLD;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x23 | LC_REQ_DYLD;
const LC_MAIN: u32 = 0x28 | LC_REQ_DYLD;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x33 | LC_REQ_DYLD;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x34 | LC_REQ_DYLD;

const SEGMENT: usize = 72;
const SECTION: usize = 80;
const SG_READ_ONLY: u32 = 0x10;
const VM_PROT_READ: u32 = 1;
const VM_PROT_WRITE: u32 = 2;
const VM_PROT_EXECUTE: u32 = 4;

const SECTION_TYPE: u32 = 0xff;
pub(super) const S_ZEROFILL: u32 = 0x1;
pub(super) const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
pub(super) const S_MOD_TERM_FUNC_POINTERS: u32 = 0xa;
pub(super) const S_GB_ZEROFILL: u32 = 0xc;
const S_INTERPOSING: u32 = 0xd;
const S_DTRACE_DOF: u32 = 0xf;
const S_LAZY_DYLIB_SYMBOL_POINTERS: u32 = 0x10;
pub(super) const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
pub(super) const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
pub(super) const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
pub(super) const S_THREAD_LOCAL_VARIABLE_POINTERS: u32 = 0x14;
const S_THREAD_LOCAL_INIT_FUNCTION_POINTERS: u32 = 0x15;
pub(super) const S_INIT_FUNC_OFFSETS: u32 = 0x16;

pub(super) fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

pub(super) fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

fn name16(bytes: &[u8]) -> [u8; 16] {
    let mut name = [0; 16];
    name.copy_from_slice(&bytes[..16]);
    name
}

fn text16(name: &[u8; 16]) -> String {
    let len = name.iter().position(|&it| it == 0).unwrap_or(16);
    String::from_utf8_lossy(&name[..len]).into_owned()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Segment {
    pub(super) name: [u8; 16],
    pub(super) vmaddr: u64,
    pub(super) vmsize: u64,
    pub(super) fileoff: u64,
    pub(super) filesize: u64,
    pub(super) initprot: u32,
    pub(super) flags: u32,
    /// Whether it is part of the image's memory: not `__PAGEZERO` (below the first segment's
    /// address, no access, no bytes) or empty.
    pub(super) mapped: bool,
}

impl Segment {
    pub(super) fn name(&self) -> String {
        text16(&self.name)
    }

    /// How its pages are mapped; `None` leaves them reserved with no access.
    pub(super) fn access(&self) -> Option<Access> {
        if self.initprot & VM_PROT_EXECUTE != 0 {
            Some(Access::Execute)
        } else if self.initprot & VM_PROT_WRITE != 0 {
            Some(Access::Copy)
        } else if self.initprot & VM_PROT_READ != 0 {
            Some(Access::Read)
        } else {
            None
        }
    }

    pub(super) fn writable(&self) -> bool {
        self.mapped && self.access() == Some(Access::Copy)
    }

    /// `__DATA_CONST`: written by fixups, then read-only.
    pub(super) fn read_only_after_fixups(&self) -> bool {
        self.flags & SG_READ_ONLY != 0
    }

    pub(super) fn range(&self) -> Range<u64> {
        self.vmaddr..self.vmaddr + self.vmsize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Section {
    pub(super) name: [u8; 16],
    pub(super) segment: [u8; 16],
    pub(super) addr: u64,
    pub(super) size: u64,
    pub(super) align: u32,
    pub(super) flags: u32,
}

impl Section {
    pub(super) fn kind(&self) -> u32 {
        self.flags & SECTION_TYPE
    }

    pub(super) fn zerofill(&self) -> bool {
        matches!(self.kind(), S_ZEROFILL | S_GB_ZEROFILL | S_THREAD_LOCAL_ZEROFILL)
    }

    pub(super) fn name(&self) -> String {
        format!("{},{}", text16(&self.segment), text16(&self.name))
    }

    pub(super) fn range(&self) -> Range<u64> {
        self.addr..self.addr + self.size
    }

    pub(super) fn is(&self, segment: &str, name: &str) -> bool {
        text16(&self.segment) == segment && text16(&self.name) == name
    }
}

/// A library the image links, by `LC_LOAD_DYLIB` (or its weak and upward kinds), in the order
/// of the bind opcodes' ordinals.
#[derive(Clone, Copy, Debug)]
pub(super) struct Dylib<'a> {
    pub(super) path: &'a CStr,
    pub(super) weak: bool,
}

/// dyld's opcode tables, each a slice of the image.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Opcodes<'a> {
    pub(super) rebase: &'a [u8],
    pub(super) bind: &'a [u8],
    pub(super) weak_bind: &'a [u8],
    pub(super) lazy_bind: &'a [u8],
}

pub(super) struct MachO<'a> {
    bytes: &'a [u8],
    /// Every `LC_SEGMENT_64` in order: the fixup opcodes name them by index.
    pub(super) segments: Vec<Segment>,
    /// Every section, in order.
    pub(super) sections: Vec<Section>,
    pub(super) dylibs: Vec<Dylib<'a>>,
    pub(super) rpaths: Vec<&'a CStr>,
    pub(super) opcodes: Opcodes<'a>,
    pub(super) exports: &'a [u8],
    /// The address of the segment at file offset 0, which holds the header: offset 0 of the
    /// image object.
    pub(super) first: u64,
    /// The end of the image's memory, page-rounded, as an offset from `first`.
    pub(super) span: u64,
    /// `LC_MAIN`'s entry, as an offset from `first`, and the stack it asks for (0: the
    /// default).
    pub(super) entry: u64,
    pub(super) stack: u64,
}

fn slice<'a>(bytes: &'a [u8], offset: u32, size: u32, what: &str) -> Result<&'a [u8], Error> {
    if size == 0 {
        return Ok(&[]);
    }
    let start = offset as usize;
    bytes
        .get(start..start + size as usize)
        .ok_or_else(|| Error::new(format!("{what} at {offset:#x}, {size:#x} bytes, is past the image of {:#x}", bytes.len())))
}

/// The NUL-terminated string at `offset` of a load command, within it.
fn command_string<'a>(command: &'a [u8], offset: u32, what: &str) -> Result<&'a CStr, Error> {
    command
        .get(offset as usize..)
        .filter(|_| offset as usize >= 12)
        .and_then(|rest| CStr::from_bytes_until_nul(rest).ok())
        .ok_or_else(|| Error::new(format!("{what}: a name at {offset:#x} not terminated within its command")))
}

fn objc_or_swift(section: &Section) -> bool {
    let name = text16(&section.name);
    (name.starts_with("__objc_") && name != "__objc_imageinfo") || name.starts_with("__swift5_")
}

impl<'a> MachO<'a> {
    /// The image in `bytes`, which are its first `image_len` bytes; `page` is the mapping
    /// granule every segment's address and offset must be a multiple of.
    pub(super) fn parse(bytes: &'a [u8], page: u64) -> Result<MachO<'a>, Error> {
        let field = |at: usize| u32_at(bytes, at).ok_or_else(|| Error::new(format!("an image of {} bytes", bytes.len())));
        if field(0)? != MH_MAGIC_64 {
            return Err(Error::new(format!("an image of magic {:#x}, not a 64-bit Mach-O", field(0)?)));
        }
        let (cputype, subtype, filetype) = (field(4)?, field(8)? & CPU_SUBTYPE_MASK, field(12)?);
        if cputype != CPU_TYPE_ARM64 || subtype == CPU_SUBTYPE_ARM64E {
            return Err(Error::new(format!(
                "a Mach-O of CPU type {cputype:#x} subtype {subtype:#x}, not arm64 (arm64e's signed pointers are not loaded)"
            )));
        }
        if filetype != MH_EXECUTE {
            return Err(Error::new(format!("a Mach-O of file type {filetype}, not MH_EXECUTE")));
        }
        let (ncmds, sizeofcmds, flags) = (field(16)?, field(20)?, field(24)?);
        if flags & MH_PIE == 0 {
            return Err(Error::new("a Mach-O without MH_PIE: blaze's loader places every image where it maps it"));
        }
        let commands = bytes
            .get(HEADER..HEADER + sizeofcmds as usize)
            .ok_or_else(|| Error::new(format!("load commands of {sizeofcmds:#x} bytes past the image of {:#x}", bytes.len())))?;

        let mut out = MachO {
            bytes,
            segments: Vec::new(),
            sections: Vec::new(),
            dylibs: Vec::new(),
            rpaths: Vec::new(),
            opcodes: Opcodes::default(),
            exports: &[],
            first: 0,
            span: 0,
            entry: 0,
            stack: 0,
        };
        let mut info = false;
        let mut main = None;
        let mut at = 0usize;
        for index in 0..ncmds {
            let what = format!("load command {index}");
            let (cmd, size) = match (u32_at(commands, at), u32_at(commands, at + 4)) {
                (Some(cmd), Some(size)) => (cmd, size as usize),
                _ => return Err(Error::new(format!("{what} at {:#x} is past sizeofcmds {sizeofcmds:#x}", HEADER + at))),
            };
            if size < 8 || !size.is_multiple_of(8) || at + size > commands.len() {
                return Err(Error::new(format!("{what} (cmd {cmd:#x}) of {size:#x} bytes at {:#x}", HEADER + at)));
            }
            let command = &commands[at..at + size];
            let word = |offset: usize| {
                u32_at(command, offset).ok_or_else(|| Error::new(format!("{what} (cmd {cmd:#x}) of {size} bytes is short")))
            };
            match cmd {
                LC_SEGMENT_64 => out.segment(command, &what)?,
                LC_DYLD_INFO | LC_DYLD_INFO_ONLY => {
                    if info {
                        return Err(Error::new("two LC_DYLD_INFO commands"));
                    }
                    info = true;
                    out.opcodes = Opcodes {
                        rebase: slice(bytes, word(8)?, word(12)?, "the rebase opcodes")?,
                        bind: slice(bytes, word(16)?, word(20)?, "the bind opcodes")?,
                        weak_bind: slice(bytes, word(24)?, word(28)?, "the weak bind opcodes")?,
                        lazy_bind: slice(bytes, word(32)?, word(36)?, "the lazy bind opcodes")?,
                    };
                    let export = slice(bytes, word(40)?, word(44)?, "the export trie")?;
                    if !export.is_empty() {
                        if !out.exports.is_empty() {
                            return Err(Error::new("two export tries"));
                        }
                        out.exports = export;
                    }
                }
                LC_DYLD_EXPORTS_TRIE => {
                    let export = slice(bytes, word(8)?, word(12)?, "the export trie")?;
                    if !out.exports.is_empty() && !export.is_empty() {
                        return Err(Error::new("two export tries"));
                    }
                    if !export.is_empty() {
                        out.exports = export;
                    }
                }
                LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_LOAD_UPWARD_DYLIB => {
                    let path = command_string(command, word(8)?, &what)?;
                    out.dylibs.push(Dylib { path, weak: cmd == LC_LOAD_WEAK_DYLIB });
                }
                LC_RPATH => out.rpaths.push(command_string(command, word(8)?, &what)?),
                LC_MAIN => {
                    if main.is_some() {
                        return Err(Error::new("two LC_MAIN commands"));
                    }
                    let entry = u64_at(command, 8).ok_or_else(|| Error::new("a short LC_MAIN"))?;
                    let stack = u64_at(command, 16).ok_or_else(|| Error::new("a short LC_MAIN"))?;
                    main = Some((entry, stack));
                }
                LC_ENCRYPTION_INFO_64 => {
                    if word(16)? != 0 {
                        return Err(Error::new("an encrypted image (LC_ENCRYPTION_INFO_64 with a cryptid)"));
                    }
                }
                LC_DYLD_CHAINED_FIXUPS => {
                    return Err(Error::new("chained fixups (LC_DYLD_CHAINED_FIXUPS): blaze-link writes opcode tables"));
                }
                LC_UNIXTHREAD => return Err(Error::new("LC_UNIXTHREAD: an image without LC_MAIN")),
                LC_REEXPORT_DYLIB => {
                    return Err(Error::new("LC_REEXPORT_DYLIB in an executable, whose exports only dyld would merge"));
                }
                LC_LAZY_LOAD_DYLIB => return Err(Error::new("LC_LAZY_LOAD_DYLIB, which dyld no longer loads")),
                LC_DYLD_ENVIRONMENT => {
                    return Err(Error::new("LC_DYLD_ENVIRONMENT: dyld settings in the image, which only dyld applies"));
                }
                cmd if cmd & LC_REQ_DYLD != 0 => {
                    return Err(Error::new(format!("load command {cmd:#x}, which dyld requires and this loader does not know")));
                }
                _ => {}
            }
            at += size;
        }
        if !info {
            return Err(Error::new("no LC_DYLD_INFO_ONLY: the image has no fixup opcodes"));
        }
        let (entry, stack) = main.ok_or_else(|| Error::new("no LC_MAIN"))?;
        out.layout(page, HEADER + sizeofcmds as usize)?;
        let text = out.first;
        if !out.in_code(text.wrapping_add(entry)) || entry >= out.span {
            return Err(Error::new(format!("LC_MAIN's entry {entry:#x} is not in the image's code")));
        }
        out.entry = entry;
        out.stack = stack;
        Ok(out)
    }

    fn segment(&mut self, command: &[u8], what: &str) -> Result<(), Error> {
        let short = || Error::new(format!("{what}: a short LC_SEGMENT_64"));
        if command.len() < SEGMENT {
            return Err(short());
        }
        let segment = Segment {
            name: name16(&command[8..24]),
            vmaddr: u64_at(command, 24).ok_or_else(short)?,
            vmsize: u64_at(command, 32).ok_or_else(short)?,
            fileoff: u64_at(command, 40).ok_or_else(short)?,
            filesize: u64_at(command, 48).ok_or_else(short)?,
            initprot: u32_at(command, 60).ok_or_else(short)?,
            flags: u32_at(command, 68).ok_or_else(short)?,
            mapped: false,
        };
        let nsects = u32_at(command, 64).ok_or_else(short)? as usize;
        if command.len() < SEGMENT + nsects * SECTION {
            return Err(Error::new(format!("{what}: segment {} of {nsects} sections in {} bytes", segment.name(), command.len())));
        }
        if segment.vmaddr.checked_add(segment.vmsize).is_none() || segment.fileoff.checked_add(segment.filesize).is_none() {
            return Err(Error::new(format!("segment {} wraps the address space", segment.name())));
        }
        for index in 0..nsects {
            let at = SEGMENT + index * SECTION;
            let section = Section {
                name: name16(&command[at..at + 16]),
                segment: name16(&command[at + 16..at + 32]),
                addr: u64_at(command, at + 32).ok_or_else(short)?,
                size: u64_at(command, at + 40).ok_or_else(short)?,
                align: u32_at(command, at + 52).ok_or_else(short)?,
                flags: u32_at(command, at + 64).ok_or_else(short)?,
            };
            let offset = u32_at(command, at + 48).ok_or_else(short)?;
            let name = section.name();
            if section.segment != segment.name {
                return Err(Error::new(format!("section {name} in segment {}", segment.name())));
            }
            let end = section.addr.checked_add(section.size);
            if section.addr < segment.vmaddr || end.is_none_or(|end| end > segment.vmaddr + segment.vmsize) {
                return Err(Error::new(format!("section {name} at {:#x}, {:#x} bytes, is outside its segment", section.addr, section.size)));
            }
            if !section.zerofill() && section.size != 0 {
                let into = section.addr - segment.vmaddr;
                if u64::from(offset).checked_sub(segment.fileoff) != Some(into) || into + section.size > segment.filesize {
                    return Err(Error::new(format!(
                        "section {name} at file offset {offset:#x}, not its segment's {:#x} bytes in",
                        into
                    )));
                }
            }
            match section.kind() {
                S_INTERPOSING => return Err(Error::new(format!("section {name} interposes, which only dyld does"))),
                S_DTRACE_DOF => return Err(Error::new(format!("section {name} has DTrace probes, which only dyld registers"))),
                S_LAZY_DYLIB_SYMBOL_POINTERS => return Err(Error::new(format!("section {name} of lazily loaded libraries"))),
                S_THREAD_LOCAL_INIT_FUNCTION_POINTERS if section.size != 0 => {
                    return Err(Error::new(format!("section {name}: thread-local initializers, which dyld no longer runs")));
                }
                _ => {}
            }
            if section.align > 12 {
                return Err(Error::new(format!("section {name} aligned to 2^{}", section.align)));
            }
            if objc_or_swift(&section) {
                return Err(Error::new(format!(
                    "section {name}: Objective-C or Swift metadata, which only dyld registers with their runtimes"
                )));
            }
            self.sections.push(section);
        }
        self.segments.push(segment);
        Ok(())
    }

    /// Checks the segments lie as blaze's loader maps them, from the one at file offset 0.
    fn layout(&mut self, page: u64, headers: usize) -> Result<(), Error> {
        let mut first = None;
        for segment in &self.segments {
            if segment.fileoff == 0 && segment.filesize != 0 {
                if first.is_some() {
                    return Err(Error::new(format!("two segments at file offset 0, the second {}", segment.name())));
                }
                first = Some(*segment);
            }
        }
        let first = first.ok_or_else(|| Error::new("no segment at file offset 0 holds the header"))?;
        if first.access() != Some(Access::Execute) || first.filesize < headers as u64 {
            return Err(Error::new(format!("segment {} holds the header but is not code of it all", first.name())));
        }
        self.first = first.vmaddr;
        let limit = self.bytes.len() as u64;
        let mut ranges: Vec<(u64, u64, String)> = Vec::new();
        for segment in &mut self.segments {
            let name = segment.name();
            if segment.vmsize == 0 {
                continue;
            }
            if segment.vmaddr < first.vmaddr {
                if segment.initprot != 0 || segment.filesize != 0 {
                    return Err(Error::new(format!("segment {name} at {:#x}, below {}", segment.vmaddr, first.name())));
                }
                if segment.vmaddr.saturating_add(segment.vmsize) > first.vmaddr {
                    return Err(Error::new(format!("segment {name} runs into {}", first.name())));
                }
                continue;
            }
            let offset = segment.vmaddr - first.vmaddr;
            if !segment.vmaddr.is_multiple_of(page) || !first.vmaddr.is_multiple_of(page) {
                return Err(Error::new(format!("segment {name} at {:#x}: not on a page of {page:#x}", segment.vmaddr)));
            }
            if segment.filesize != 0 && segment.fileoff != offset {
                return Err(Error::new(format!(
                    "segment {name} is at file offset {:#x} where its address puts it at {offset:#x}: blaze's loader \
                     maps an image whose every segment's file offset is its address from {}'s",
                    segment.fileoff,
                    first.name()
                )));
            }
            if segment.filesize > segment.vmsize || offset.checked_add(segment.filesize).is_none_or(|end| end > limit) {
                return Err(Error::new(format!(
                    "segment {name} of {:#x} file bytes at {offset:#x} in {:#x} of memory, an image of {limit:#x}",
                    segment.filesize, segment.vmsize
                )));
            }
            let prot = segment.initprot & (VM_PROT_WRITE | VM_PROT_EXECUTE);
            if prot == VM_PROT_WRITE | VM_PROT_EXECUTE {
                return Err(Error::new(format!("segment {name} is writable and executable")));
            }
            if segment.access() != Some(Access::Copy) && segment.vmsize > segment.filesize.next_multiple_of(page) {
                return Err(Error::new(format!("segment {name} has zero pages but is not writable")));
            }
            let end = (offset + segment.vmsize)
                .checked_next_multiple_of(page)
                .ok_or_else(|| Error::new(format!("segment {name} ends past the address space")))?;
            ranges.push((offset, end, name));
            segment.mapped = true;
            self.span = self.span.max(end);
        }
        ranges.sort();
        for pair in ranges.windows(2) {
            if pair[1].0 < pair[0].1 {
                return Err(Error::new(format!("segments {} and {} overlap", pair[0].2, pair[1].2)));
            }
        }
        for section in &self.sections {
            let segment = self.segments.iter().find(|it| it.name == section.segment && it.range().contains(&section.addr));
            if section.size != 0 && !segment.is_some_and(|it| it.mapped) {
                return Err(Error::new(format!("section {} is in no segment of the image's memory", section.name())));
            }
        }
        Ok(())
    }

    /// The image's bytes of `len` at address `addr` (before the slide), in a segment's file part.
    pub(super) fn file(&self, addr: u64, len: u64) -> Option<&'a [u8]> {
        let segment = self.segments.iter().find(|it| it.mapped && it.range().contains(&addr))?;
        let into = addr - segment.vmaddr;
        if into.checked_add(len)? > segment.filesize {
            return None;
        }
        let start = usize::try_from(segment.fileoff + into).ok()?;
        self.bytes.get(start..start.checked_add(usize::try_from(len).ok()?)?)
    }

    /// Whether `addr` (before the slide) is in an executable segment.
    pub(super) fn in_code(&self, addr: u64) -> bool {
        self.segments.iter().any(|it| it.mapped && it.access() == Some(Access::Execute) && it.range().contains(&addr))
    }

    /// The executable segments' address ranges, before the slide.
    pub(super) fn code(&self) -> impl Iterator<Item = Range<u64>> + '_ {
        self.segments.iter().filter(|it| it.mapped && it.access() == Some(Access::Execute)).map(Segment::range)
    }

    pub(super) fn sections_of(&self, kind: u32) -> impl Iterator<Item = &Section> + '_ {
        self.sections.iter().filter(move |it| it.kind() == kind && it.size != 0)
    }

    pub(super) fn section(&self, segment: &str, name: &str) -> Option<&Section> {
        self.sections.iter().find(|it| it.is(segment, name) && it.size != 0)
    }
}
