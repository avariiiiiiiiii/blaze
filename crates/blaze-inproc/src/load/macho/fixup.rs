//! dyld's fixup opcodes and export trie, run over the image's writable segments as byte
//! buffers: the loader runs them over the private copies it mapped, the tests over plain
//! vectors, and both give the same bytes. Rebases first, then binds, the lazy table's too, all
//! at once (dyld's `-bind_at_load`). The opcodes are untrusted: a location outside a writable
//! segment, a bad opcode or more fixups than the segments have words is an error naming it.

#![forbid(unsafe_code)]

use super::parse::{MachO, S_THREAD_LOCAL_VARIABLE_POINTERS, Segment as Command};
use crate::Error;

const REBASE_TYPE_POINTER: u8 = 1;
const BIND_TYPE_POINTER: u8 = 1;
const BIND_SYMBOL_FLAGS_WEAK_IMPORT: u8 = 0x1;

const EXPORT_SYMBOL_FLAGS_KIND_MASK: u64 = 0x03;
const EXPORT_SYMBOL_FLAGS_KIND_REGULAR: u64 = 0x00;
const EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL: u64 = 0x01;
const EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE: u64 = 0x02;
const EXPORT_SYMBOL_FLAGS_REEXPORT: u64 = 0x08;
const EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER: u64 = 0x10;

/// One writable segment's bytes: its pages from `vmaddr` (before the slide).
pub(super) struct Segment<'m> {
    pub(super) vmaddr: u64,
    pub(super) bytes: &'m mut [u8],
}

/// The image's writable segments, which fixups write.
pub(super) struct Memory<'m> {
    segments: Vec<Segment<'m>>,
}

impl<'m> Memory<'m> {
    pub(super) fn new(segments: Vec<Segment<'m>>) -> Memory<'m> {
        Memory { segments }
    }

    fn find(&self, addr: u64, len: usize) -> Option<(usize, usize)> {
        self.segments.iter().enumerate().find_map(|(index, segment)| {
            let at = usize::try_from(addr.checked_sub(segment.vmaddr)?).ok()?;
            (at.checked_add(len)? <= segment.bytes.len()).then_some((index, at))
        })
    }

    pub(super) fn bytes(&self, addr: u64, len: u64) -> Option<&[u8]> {
        let len = usize::try_from(len).ok()?;
        let (index, at) = self.find(addr, len)?;
        Some(&self.segments[index].bytes[at..at + len])
    }

    pub(super) fn read(&self, addr: u64) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(addr, 8)?.try_into().ok()?))
    }

    pub(super) fn write(&mut self, addr: u64, value: u64) -> bool {
        let Some((index, at)) = self.find(addr, 8) else { return false };
        self.segments[index].bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
        true
    }

    pub(super) fn zero(&mut self, addr: u64, len: u64) -> bool {
        let Ok(len) = usize::try_from(len) else { return false };
        let Some((index, at)) = self.find(addr, len) else { return false };
        self.segments[index].bytes[at..at + len].fill(0);
        true
    }
}

/// A reader of an opcode table.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    table: &'static str,
}

impl<'a> Cursor<'a> {
    fn error(&self, what: &str) -> Error {
        Error::new(format!("{what} at byte {:#x} of the {}", self.at, self.table))
    }

    fn byte(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn uleb(&mut self) -> Result<u64, Error> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.byte().ok_or_else(|| self.error("a ULEB128 cut off"))?;
            let low = u64::from(byte & 0x7f);
            if shift >= 64 || (low << shift) >> shift != low {
                return Err(self.error("a ULEB128 past 64 bits"));
            }
            value |= low << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    fn sleb(&mut self) -> Result<i64, Error> {
        let mut value = 0i64;
        let mut shift = 0u32;
        loop {
            let byte = self.byte().ok_or_else(|| self.error("an SLEB128 cut off"))?;
            if shift >= 64 {
                return Err(self.error("an SLEB128 past 64 bits"));
            }
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
        }
    }

    fn string(&mut self) -> Result<&'a [u8], Error> {
        let rest = &self.bytes[self.at.min(self.bytes.len())..];
        let len = rest.iter().position(|&it| it == 0).ok_or_else(|| self.error("a symbol name not terminated"))?;
        self.at += len + 1;
        Ok(&rest[..len])
    }
}

/// Where the opcodes are: a segment and an offset in it, and how many more locations they may
/// name (every word of the writable segments once, over all tables, and one).
struct Place<'s> {
    segments: &'s [Command],
    segment: Option<usize>,
    offset: u64,
}

impl Place<'_> {
    fn set(&mut self, cursor: &Cursor<'_>, index: u8, offset: u64) -> Result<(), Error> {
        if usize::from(index) >= self.segments.len() {
            return Err(cursor.error(&format!("segment {index} of {}", self.segments.len())));
        }
        self.segment = Some(usize::from(index));
        self.offset = offset;
        Ok(())
    }

    /// The location the opcodes are at, which must be a word of a writable segment.
    fn address(&self, cursor: &Cursor<'_>) -> Result<u64, Error> {
        let segment = self.segment.map(|it| self.segments[it]).ok_or_else(|| cursor.error("a fixup before any segment"))?;
        if !segment.writable() {
            return Err(cursor.error(&format!("a fixup in segment {}, which is not writable", segment.name())));
        }
        if self.offset.checked_add(8).is_none_or(|end| end > segment.vmsize) {
            return Err(cursor.error(&format!("a fixup at {:#x} past segment {} of {:#x}", self.offset, segment.name(), segment.vmsize)));
        }
        Ok(segment.vmaddr + self.offset)
    }

    fn advance(&mut self, by: u64) {
        self.offset = self.offset.wrapping_add(by);
    }
}

/// The most locations the tables may name: a run of the opcodes can't loop past it.
pub(super) fn budget(macho: &MachO<'_>) -> u64 {
    macho.segments.iter().filter(|it| it.writable()).fold(1u64, |sum, it| sum.saturating_add(it.vmsize / 8))
}

fn spend(budget: &mut u64, cursor: &Cursor<'_>) -> Result<(), Error> {
    *budget = budget.checked_sub(1).ok_or_else(|| cursor.error("more fixups than the writable segments have words"))?;
    Ok(())
}

/// Each location the rebase opcodes name, by its address before the slide.
pub(super) fn rebases(
    opcodes: &[u8],
    segments: &[Command],
    budget: &mut u64,
    mut each: impl FnMut(u64) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut cursor = Cursor { bytes: opcodes, at: 0, table: "rebase opcodes" };
    let mut place = Place { segments, segment: None, offset: 0 };
    let mut kind = 0u8;
    let mut rebase = |place: &Place<'_>, cursor: &Cursor<'_>, kind: u8, budget: &mut u64| -> Result<(), Error> {
        spend(budget, cursor)?;
        if kind != REBASE_TYPE_POINTER {
            return Err(cursor.error(&format!("a rebase of type {kind}, not a pointer")));
        }
        each(place.address(cursor)?)
    };
    while let Some(byte) = cursor.byte() {
        let (opcode, immediate) = (byte & 0xf0, byte & 0x0f);
        match opcode {
            0x00 => break,
            0x10 => kind = immediate,
            0x20 => {
                let offset = cursor.uleb()?;
                place.set(&cursor, immediate, offset)?;
            }
            0x30 => place.advance(cursor.uleb()?),
            0x40 => place.advance(u64::from(immediate) * 8),
            0x50 => {
                for _ in 0..immediate {
                    rebase(&place, &cursor, kind, budget)?;
                    place.advance(8);
                }
            }
            0x60 => {
                let count = cursor.uleb()?;
                for _ in 0..count {
                    rebase(&place, &cursor, kind, budget)?;
                    place.advance(8);
                }
            }
            0x70 => {
                rebase(&place, &cursor, kind, budget)?;
                let skip = cursor.uleb()?;
                place.advance(skip.wrapping_add(8));
            }
            0x80 => {
                let count = cursor.uleb()?;
                let skip = cursor.uleb()?;
                for _ in 0..count {
                    rebase(&place, &cursor, kind, budget)?;
                    place.advance(skip.wrapping_add(8));
                }
            }
            _ => return Err(cursor.error(&format!("rebase opcode {byte:#04x}"))),
        }
    }
    Ok(())
}

/// What the bind opcodes set before a bind.
#[derive(Clone, Copy)]
struct Symbol<'a> {
    ordinal: i64,
    name: Option<&'a [u8]>,
    weak: bool,
    kind: u8,
    addend: i64,
}

/// One location a bind opcode names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Bind<'a> {
    /// Before the slide.
    pub(super) addr: u64,
    /// The library's 1-based index, or 0 (this image), -1 (the main executable, this image),
    /// -2 (the flat namespace), -3 (the weak definition lookup).
    pub(super) ordinal: i64,
    /// As the image names it, with its leading underscore.
    pub(super) name: &'a [u8],
    pub(super) weak: bool,
    pub(super) addend: i64,
}

/// Each location the bind opcodes name. `lazy`: the lazy table, whose entries each end with
/// `BIND_OPCODE_DONE`.
pub(super) fn binds<'a>(
    opcodes: &'a [u8],
    segments: &[Command],
    lazy: bool,
    budget: &mut u64,
    mut each: impl FnMut(&Bind<'a>) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut cursor = Cursor { bytes: opcodes, at: 0, table: if lazy { "lazy bind opcodes" } else { "bind opcodes" } };
    let mut place = Place { segments, segment: None, offset: 0 };
    let mut symbol = Symbol { ordinal: 0, name: None, weak: false, kind: 0, addend: 0 };
    let mut bind = |place: &Place<'_>, cursor: &Cursor<'a>, symbol: Symbol<'a>, budget: &mut u64| -> Result<(), Error> {
        spend(budget, cursor)?;
        if symbol.kind != BIND_TYPE_POINTER {
            return Err(cursor.error(&format!("a bind of type {}, not a pointer", symbol.kind)));
        }
        let name = symbol.name.filter(|it| !it.is_empty()).ok_or_else(|| cursor.error("a bind with no symbol"))?;
        each(&Bind { addr: place.address(cursor)?, ordinal: symbol.ordinal, name, weak: symbol.weak, addend: symbol.addend })
    };
    while let Some(byte) = cursor.byte() {
        let (opcode, immediate) = (byte & 0xf0, byte & 0x0f);
        match opcode {
            0x00 if lazy => {}
            0x00 => break,
            0x10 => symbol.ordinal = i64::from(immediate),
            0x20 => {
                let value = cursor.uleb()?;
                symbol.ordinal = i64::try_from(value).map_err(|_| cursor.error(&format!("library ordinal {value}")))?;
            }
            0x30 => symbol.ordinal = if immediate == 0 { 0 } else { i64::from((0xf0 | immediate) as i8) },
            0x40 => {
                symbol.weak = immediate & BIND_SYMBOL_FLAGS_WEAK_IMPORT != 0;
                symbol.name = Some(cursor.string()?);
            }
            0x50 => symbol.kind = immediate,
            0x60 => symbol.addend = cursor.sleb()?,
            0x70 => {
                let offset = cursor.uleb()?;
                place.set(&cursor, immediate, offset)?;
            }
            0x80 => place.advance(cursor.uleb()?),
            0x90 => {
                bind(&place, &cursor, symbol, budget)?;
                place.advance(8);
            }
            0xa0 => {
                bind(&place, &cursor, symbol, budget)?;
                let skip = cursor.uleb()?;
                place.advance(skip.wrapping_add(8));
            }
            0xb0 => {
                bind(&place, &cursor, symbol, budget)?;
                place.advance(u64::from(immediate) * 8 + 8);
            }
            0xc0 => {
                let count = cursor.uleb()?;
                let skip = cursor.uleb()?;
                for _ in 0..count {
                    bind(&place, &cursor, symbol, budget)?;
                    place.advance(skip.wrapping_add(8));
                }
            }
            0xd0 => return Err(cursor.error("threaded binds (BIND_OPCODE_THREADED), which are arm64e's")),
            _ => return Err(cursor.error(&format!("bind opcode {byte:#04x}"))),
        }
    }
    Ok(())
}

/// What the export trie says of a symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Export {
    /// At this offset from the image's start: a function or data, or a thread-local's
    /// descriptor.
    Offset(u64),
    Absolute(u64),
}

fn trie_error(at: usize, what: &str) -> Error {
    Error::new(format!("{what} at byte {at:#x} of the export trie"))
}

/// `name` (with its leading underscore) in the export trie `trie`.
pub(super) fn export(trie: &[u8], name: &[u8]) -> Result<Option<Export>, Error> {
    if trie.is_empty() {
        return Ok(None);
    }
    let mut node = 0usize;
    let mut rest = name;
    for _ in 0..=name.len() {
        let mut cursor = Cursor { bytes: trie, at: node, table: "export trie" };
        if node >= trie.len() {
            return Err(trie_error(node, "a node past the end"));
        }
        let terminal = usize::try_from(cursor.uleb()?).map_err(|_| trie_error(node, "a terminal size"))?;
        let children = cursor.at.checked_add(terminal).filter(|&it| it < trie.len()).ok_or_else(|| trie_error(node, "a terminal past the end"))?;
        if rest.is_empty() {
            if terminal == 0 {
                return Ok(None);
            }
            let flags = cursor.uleb()?;
            if flags & EXPORT_SYMBOL_FLAGS_REEXPORT != 0 {
                return Err(Error::new(format!("{} is re-exported from another library", String::from_utf8_lossy(name))));
            }
            if flags & EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER != 0 {
                return Err(Error::new(format!("{} has a resolver, which only dyld runs", String::from_utf8_lossy(name))));
            }
            let value = cursor.uleb()?;
            return match flags & EXPORT_SYMBOL_FLAGS_KIND_MASK {
                EXPORT_SYMBOL_FLAGS_KIND_REGULAR | EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL => Ok(Some(Export::Offset(value))),
                EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE => Ok(Some(Export::Absolute(value))),
                kind => Err(trie_error(node, &format!("an export of kind {kind}"))),
            };
        }
        cursor.at = children;
        let count = cursor.byte().ok_or_else(|| trie_error(children, "a child count past the end"))?;
        let mut next = None;
        for _ in 0..count {
            let label = cursor.string()?;
            let child = usize::try_from(cursor.uleb()?).map_err(|_| trie_error(cursor.at, "a child offset"))?;
            if !label.is_empty() && rest.starts_with(label) {
                next = Some((child, label.len()));
                break;
            }
        }
        let Some((child, consumed)) = next else { return Ok(None) };
        node = child;
        rest = &rest[consumed..];
    }
    Err(trie_error(node, "a path longer than the name"))
}

/// Symbols from the image's libraries and the flat namespace, by the image's names (leading
/// underscore and all). `None`: not found, or the library is a weak one that is absent.
pub(super) trait Imports {
    fn library(&mut self, ordinal: usize, name: &[u8]) -> Result<Option<u64>, Error>;
    fn flat(&mut self, name: &[u8]) -> Result<Option<u64>, Error>;
}

/// The value a bind writes, and whether it is this image's own.
fn resolve(macho: &MachO<'_>, base: u64, imports: &mut impl Imports, bind: &Bind<'_>) -> Result<(Option<u64>, bool), Error> {
    let own = |name: &[u8]| -> Result<Option<u64>, Error> {
        Ok(export(macho.exports, name)?.map(|it| match it {
            Export::Offset(offset) => base.wrapping_add(offset),
            Export::Absolute(value) => value,
        }))
    };
    Ok(match bind.ordinal {
        ordinal @ 1.. => {
            let ordinal = ordinal as usize;
            if ordinal > macho.dylibs.len() {
                return Err(Error::new(format!(
                    "a bind of {} to library {ordinal} of {}",
                    String::from_utf8_lossy(bind.name),
                    macho.dylibs.len()
                )));
            }
            (imports.library(ordinal, bind.name)?, false)
        }
        0 | -1 => (own(bind.name)?, true),
        -2 | -3 => match own(bind.name)? {
            Some(value) => (Some(value), true),
            None => (imports.flat(bind.name)?, false),
        },
        ordinal => return Err(Error::new(format!("a bind of {} to ordinal {ordinal}", String::from_utf8_lossy(bind.name)))),
    })
}

/// Applies every rebase and bind of the image at `base` (its first segment's address
/// `macho.first` slid there) to `memory`.
pub(super) fn apply(macho: &MachO<'_>, memory: &mut Memory<'_>, base: u64, imports: &mut impl Imports) -> Result<(), Error> {
    if !macho.opcodes.weak_bind.is_empty() {
        return Err(Error::new(
            "weak bind opcodes: weak definitions coalesced across images, which blaze-link does not write and only dyld runs",
        ));
    }
    let slide = base.wrapping_sub(macho.first);
    let mut budget = budget(macho);
    rebases(macho.opcodes.rebase, &macho.segments, &mut budget, |addr| {
        let value = memory.read(addr).ok_or_else(|| Error::new(format!("a rebase at {addr:#x} outside the image's data")))?;
        // `read` found the word, so `write` does.
        memory.write(addr, value.wrapping_add(slide));
        Ok(())
    })?;
    let thread_pointers: Vec<_> = macho.sections_of(S_THREAD_LOCAL_VARIABLE_POINTERS).map(|it| it.range()).collect();
    let mut last: Option<(i64, &[u8], Option<u64>, bool)> = None;
    for (table, lazy) in [(macho.opcodes.bind, false), (macho.opcodes.lazy_bind, true)] {
        binds(table, &macho.segments, lazy, &mut budget, |bind| {
            let (found, own) = match last {
                Some((ordinal, name, found, own)) if ordinal == bind.ordinal && name == bind.name => (found, own),
                _ => {
                    let (found, own) = resolve(macho, base, imports, bind)?;
                    last = Some((bind.ordinal, bind.name, found, own));
                    (found, own)
                }
            };
            let symbol = || String::from_utf8_lossy(bind.name).into_owned();
            let value = match found {
                Some(value) => value,
                None if bind.weak => 0,
                None => return Err(Error::new(format!("symbol {} (library ordinal {}) is not found", symbol(), bind.ordinal))),
            };
            if !own && found.is_some() && thread_pointers.iter().any(|it| it.contains(&bind.addr)) {
                return Err(Error::new(format!(
                    "thread-local {} imported from a library: dlsym gives a thread's variable, not its descriptor",
                    symbol()
                )));
            }
            if !memory.write(bind.addr, value.wrapping_add(bind.addend as u64)) {
                return Err(Error::new(format!("a bind of {} at {:#x} outside the image's data", symbol(), bind.addr)));
            }
            Ok(())
        })?;
    }
    Ok(())
}
