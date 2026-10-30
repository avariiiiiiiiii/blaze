//! The image's dynamic relocations, applied to its writable segments as byte buffers: the
//! loader runs it over the private copies it mapped, the tests over plain vectors, and both
//! give the same bytes. `-z now`: every relocation at once, `RELR` and `RELATIVE` first, the
//! IFUNC resolvers last (a resolver may read what the others wrote), as glibc orders them.

#![forbid(unsafe_code)]

use std::ffi::CStr;
use std::ops::Range;

use super::parse::{Elf, Machine, RELA, SHN_ABS, STB_WEAK, STT_GNU_IFUNC, STT_TLS, u64_at};
use crate::Error;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    None,
    Relative,
    GlobDat,
    JumpSlot,
    Abs64,
    Irelative,
    TpOff,
    DtpMod,
    DtpOff,
    TlsDesc,
}

fn kind(machine: Machine, r_type: u32) -> Option<Kind> {
    Some(match (machine, r_type) {
        (_, 0) => Kind::None,
        (Machine::X86_64, 1) | (Machine::Aarch64, 257) => Kind::Abs64,
        (Machine::X86_64, 6) | (Machine::Aarch64, 1025) => Kind::GlobDat,
        (Machine::X86_64, 7) | (Machine::Aarch64, 1026) => Kind::JumpSlot,
        (Machine::X86_64, 8) | (Machine::Aarch64, 1027) => Kind::Relative,
        (Machine::X86_64, 16) | (Machine::Aarch64, 1028) => Kind::DtpMod,
        (Machine::X86_64, 17) | (Machine::Aarch64, 1029) => Kind::DtpOff,
        (Machine::X86_64, 18) | (Machine::Aarch64, 1030) => Kind::TpOff,
        (Machine::X86_64, 36) | (Machine::Aarch64, 1031) => Kind::TlsDesc,
        (Machine::X86_64, 37) | (Machine::Aarch64, 1032) => Kind::Irelative,
        _ => return None,
    })
}

fn type_name(machine: Machine, r_type: u32) -> String {
    let name = match (machine, kind(machine, r_type)) {
        (_, Some(Kind::None)) => "NONE",
        (Machine::X86_64, Some(Kind::Abs64)) => "R_X86_64_64",
        (Machine::X86_64, Some(Kind::GlobDat)) => "R_X86_64_GLOB_DAT",
        (Machine::X86_64, Some(Kind::JumpSlot)) => "R_X86_64_JUMP_SLOT",
        (Machine::X86_64, Some(Kind::Relative)) => "R_X86_64_RELATIVE",
        (Machine::X86_64, Some(Kind::DtpMod)) => "R_X86_64_DTPMOD64",
        (Machine::X86_64, Some(Kind::DtpOff)) => "R_X86_64_DTPOFF64",
        (Machine::X86_64, Some(Kind::TpOff)) => "R_X86_64_TPOFF64",
        (Machine::X86_64, Some(Kind::TlsDesc)) => "R_X86_64_TLSDESC",
        (Machine::X86_64, Some(Kind::Irelative)) => "R_X86_64_IRELATIVE",
        (Machine::Aarch64, Some(Kind::Abs64)) => "R_AARCH64_ABS64",
        (Machine::Aarch64, Some(Kind::GlobDat)) => "R_AARCH64_GLOB_DAT",
        (Machine::Aarch64, Some(Kind::JumpSlot)) => "R_AARCH64_JUMP_SLOT",
        (Machine::Aarch64, Some(Kind::Relative)) => "R_AARCH64_RELATIVE",
        (Machine::Aarch64, Some(Kind::DtpMod)) => "R_AARCH64_TLS_DTPMOD",
        (Machine::Aarch64, Some(Kind::DtpOff)) => "R_AARCH64_TLS_DTPREL",
        (Machine::Aarch64, Some(Kind::TpOff)) => "R_AARCH64_TLS_TPREL",
        (Machine::Aarch64, Some(Kind::TlsDesc)) => "R_AARCH64_TLSDESC",
        (Machine::Aarch64, Some(Kind::Irelative)) => "R_AARCH64_IRELATIVE",
        (_, None) => return format!("type {r_type}"),
    };
    name.to_owned()
}

/// One writable segment's bytes: its pages from `vaddr`, of which the first `memsz` are the
/// segment's.
pub(super) struct Segment<'m> {
    pub(super) vaddr: u64,
    pub(super) memsz: u64,
    pub(super) bytes: &'m mut [u8],
}

/// The image's writable segments, which relocations write.
pub(super) struct Memory<'m> {
    segments: Vec<Segment<'m>>,
}

impl<'m> Memory<'m> {
    pub(super) fn new(segments: Vec<Segment<'m>>) -> Memory<'m> {
        Memory { segments }
    }

    fn span(segment: &Segment<'_>, at: u64, len: u64) -> Option<Range<usize>> {
        let offset = at.checked_sub(segment.vaddr)?;
        let end = offset.checked_add(len).filter(|&it| it <= segment.memsz && it <= segment.bytes.len() as u64)?;
        Some(offset as usize..end as usize)
    }

    /// `len` bytes at `at`, when one segment holds them all.
    pub(super) fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        self.segments.iter().find_map(|it| Memory::span(it, at, len).map(|range| &it.bytes[range]))
    }

    fn field(&mut self, at: u64, len: u64) -> Option<&mut [u8]> {
        self.segments.iter_mut().find_map(|it| Memory::span(it, at, len).map(|range| &mut it.bytes[range]))
    }

    pub(super) fn read(&self, at: u64) -> Option<u64> {
        self.bytes(at, 8).and_then(|it| u64_at(it, 0))
    }

    fn put(&mut self, at: u64, value: u64) -> bool {
        match self.field(at, 8) {
            Some(field) => {
                field.copy_from_slice(&value.to_le_bytes());
                true
            }
            None => false,
        }
    }

    #[cfg(test)]
    pub(super) fn segments(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.segments.iter().map(|it| (it.vaddr, &*it.bytes))
    }
}

/// Where the image is and what its thread-local relocations resolve to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Place {
    pub(super) base: u64,
    /// The image's thread-local block from the thread pointer: the loader's reserve
    /// ([`crate::TlsReserve::bias`]), the image's template at its start.
    pub(super) tp: i64,
    /// The loader's module ID and the reserve's offset in the loader's block, which is what
    /// `__tls_get_addr` is asked for a thread-local of the image.
    pub(super) module: u64,
    pub(super) dtp: u64,
    /// The static TLSDESC resolver: returns its descriptor's second word.
    pub(super) tlsdesc: u64,
}

/// Symbols from the system's libraries, and IFUNC resolvers run.
pub(super) trait Imports {
    /// The address of `name` (at `version`), or why there is none.
    fn find(&mut self, name: &CStr, version: Option<&CStr>) -> Result<u64, String>;
    /// What the image's resolver at `resolver` returns.
    fn ifunc(&mut self, resolver: u64) -> u64;
}

/// A symbol's value for an address relocation.
#[derive(Clone, Copy)]
enum Value {
    At(u64),
    /// The resolver of an IFUNC the image defines.
    Ifunc(u64),
}

struct Run<'r, 'a, I> {
    elf: &'r Elf<'a>,
    place: &'r Place,
    imports: &'r mut I,
    /// Resolved symbols by index: most are bound once, some many times.
    cache: Vec<Option<Value>>,
    /// (where, resolver, addend) of IFUNC results, written last.
    deferred: Vec<(u64, u64, u64)>,
}

/// Applies the image's relocations to `memory`.
pub(super) fn relocate(elf: &Elf<'_>, memory: &mut Memory<'_>, place: &Place, imports: &mut impl Imports) -> Result<(), Error> {
    let mut run = Run { elf, place, imports, cache: Vec::new(), deferred: Vec::new() };
    run.relr(memory)?;
    run.table(memory, elf.dynamic.rela.clone(), ".rela.dyn")?;
    run.table(memory, elf.dynamic.jmprel.clone(), ".rela.plt")?;
    for &(at, resolver, addend) in &run.deferred {
        let value = run.imports.ifunc(resolver).wrapping_add(addend);
        if !memory.put(at, value) {
            return Err(Error::new(format!("the IFUNC result at {at:#x} has no writable place")));
        }
    }
    Ok(())
}

impl<I: Imports> Run<'_, '_, I> {
    /// Why `len` bytes at `at` can't be written: the segment they are in.
    fn unwritable(&self, at: u64) -> String {
        match self.elf.load_at(at) {
            Some(load) if load.executable() => format!("{at:#x} is in {} (a text relocation)", load.describe()),
            Some(load) if load.writable() => format!("{at:#x} runs past the end of {}", load.describe()),
            Some(load) => format!("{at:#x} is in {}, which the loader does not write", load.describe()),
            None => format!("{at:#x} is in no segment"),
        }
    }

    fn store(&self, memory: &mut Memory<'_>, at: u64, value: u64) -> Result<(), String> {
        if memory.put(at, value) { Ok(()) } else { Err(self.unwritable(at)) }
    }

    /// `DT_RELR`: an even word is an address to slide, an odd one a bitmap of the 63 words
    /// after the last.
    fn relr(&mut self, memory: &mut Memory<'_>) -> Result<(), Error> {
        let range = self.elf.dynamic.relr.clone();
        if range.is_empty() {
            return Ok(());
        }
        let table = self.elf.file(range.start, range.end - range.start)?;
        let base = self.place.base;
        let slide = |memory: &mut Memory<'_>, at: u64, index: usize| -> Result<(), Error> {
            let value = memory.read(at).ok_or_else(|| {
                Error::new(format!("DT_RELR entry {index} slides the word at {at:#x}: {}", self.unwritable(at)))
            })?;
            memory.put(at, value.wrapping_add(base));
            Ok(())
        };
        let mut next = 0u64;
        for (index, word) in table.as_chunks::<8>().0.iter().enumerate() {
            let word = u64::from_le_bytes(*word);
            if word & 1 == 0 {
                slide(memory, word, index)?;
                next = word.wrapping_add(8);
                continue;
            }
            let mut bits = word >> 1;
            let mut at = next;
            while bits != 0 {
                if bits & 1 != 0 {
                    slide(memory, at, index)?;
                }
                bits >>= 1;
                at = at.wrapping_add(8);
            }
            next = next.wrapping_add(63 * 8);
        }
        Ok(())
    }

    fn table(&mut self, memory: &mut Memory<'_>, range: Range<u64>, table: &str) -> Result<(), Error> {
        if range.is_empty() {
            return Ok(());
        }
        let bytes = self.elf.file(range.start, range.end - range.start)?;
        let (entries, _) = bytes.as_chunks::<{ RELA as usize }>();
        for (index, entry) in entries.iter().enumerate() {
            let at = u64_at(entry, 0).unwrap_or_default();
            let info = u64_at(entry, 8).unwrap_or_default();
            let addend = u64_at(entry, 16).unwrap_or_default() as i64;
            self.one(memory, at, info, addend).map_err(|why| {
                Error::new(format!(
                    "relocation {index} of {table} ({}, symbol {}) at {at:#x}: {why}",
                    type_name(self.elf.machine, info as u32),
                    info >> 32
                ))
            })?;
        }
        Ok(())
    }

    fn one(&mut self, memory: &mut Memory<'_>, at: u64, info: u64, addend: i64) -> Result<(), String> {
        let r_type = info as u32;
        let sym = (info >> 32) as u32;
        let kind = kind(self.elf.machine, r_type).ok_or("a relocation type the loader does not apply")?;
        let base = self.place.base;
        let value = match kind {
            Kind::None => return Ok(()),
            Kind::Relative => base.wrapping_add(addend as u64),
            Kind::GlobDat | Kind::JumpSlot | Kind::Abs64 => {
                // glibc adds the addend of x86-64's GLOB_DAT and JUMP_SLOT nowhere (the psABI's
                // `S`), and of AArch64's everywhere (`S + A`).
                let add = match (kind, self.elf.machine) {
                    (Kind::Abs64, _) | (_, Machine::Aarch64) => addend as u64,
                    _ => 0,
                };
                match self.value(sym)? {
                    Value::At(value) => value.wrapping_add(add),
                    Value::Ifunc(resolver) => return self.defer(memory, at, resolver, add),
                }
            }
            Kind::Irelative => return self.defer(memory, at, base.wrapping_add(addend as u64), 0),
            Kind::TpOff => (self.place.tp as u64).wrapping_add(self.tls(sym, addend)?),
            Kind::DtpMod => {
                self.tls(sym, 0)?;
                self.place.module
            }
            Kind::DtpOff => self.place.dtp.wrapping_add(self.tls(sym, addend)?),
            Kind::TlsDesc => {
                let offset = (self.place.tp as u64).wrapping_add(self.tls(sym, addend)?);
                if memory.field(at, 16).is_none() {
                    return Err(self.unwritable(at));
                }
                memory.put(at, self.place.tlsdesc);
                memory.put(at + 8, offset);
                return Ok(());
            }
        };
        self.store(memory, at, value)
    }

    fn defer(&mut self, memory: &mut Memory<'_>, at: u64, resolver: u64, addend: u64) -> Result<(), String> {
        let image = resolver.wrapping_sub(self.place.base);
        if !self.elf.code(image, 1) {
            return Err(format!("the IFUNC resolver at {resolver:#x} is not in the image's code"));
        }
        if memory.field(at, 8).is_none() {
            return Err(self.unwritable(at));
        }
        self.deferred.push((at, resolver, addend));
        Ok(())
    }

    /// The value of symbol `index` for an address relocation.
    fn value(&mut self, index: u32) -> Result<Value, String> {
        if index == 0 {
            return Ok(Value::At(0));
        }
        if let Some(&Some(value)) = self.cache.get(index as usize) {
            return Ok(value);
        }
        let symbol = self.elf.symbol(index).map_err(|it| it.what)?;
        let name = || symbol.name.to_string_lossy();
        let value = if symbol.defined() {
            match symbol.kind {
                STT_TLS => return Err(format!("thread-local `{}` used as an address", name())),
                STT_GNU_IFUNC => Value::Ifunc(self.place.base.wrapping_add(symbol.value)),
                _ if symbol.shndx == SHN_ABS => Value::At(symbol.value),
                _ => Value::At(self.place.base.wrapping_add(symbol.value)),
            }
        } else {
            if symbol.kind == STT_TLS {
                return Err(imported_tls(&name()));
            }
            let version = self.elf.version(index).map_err(|it| it.what)?;
            match self.imports.find(symbol.name, version) {
                Ok(address) => Value::At(address),
                Err(_) if symbol.bind == STB_WEAK => Value::At(0),
                Err(why) => {
                    let version = version.map(|it| format!("@{}", it.to_string_lossy())).unwrap_or_default();
                    return Err(format!("`{}{version}` is in none of the libraries loaded: {why}", name()));
                }
            }
        };
        let slot = index as usize;
        if self.cache.len() <= slot {
            self.cache.resize(slot + 1, None);
        }
        self.cache[slot] = Some(value);
        Ok(value)
    }

    /// The offset in the image's thread-local block that symbol `index` plus `addend` is at.
    fn tls(&mut self, index: u32, addend: i64) -> Result<u64, String> {
        let tls = self.elf.tls.ok_or("a thread-local relocation in an image with no PT_TLS")?;
        let offset = if index == 0 {
            0
        } else {
            let symbol = self.elf.symbol(index).map_err(|it| it.what)?;
            if !symbol.defined() {
                return Err(imported_tls(&symbol.name.to_string_lossy()));
            }
            if symbol.kind != STT_TLS {
                return Err(format!("`{}` is not a thread-local", symbol.name.to_string_lossy()));
            }
            symbol.value
        };
        offset.checked_add_signed(addend).filter(|&it| it <= tls.memsz).ok_or_else(|| {
            format!("offset {offset:#x} plus {addend} is outside the image's {:#x}-byte thread-local block", tls.memsz)
        })
    }
}

fn imported_tls(name: &str) -> String {
    format!(
        "imported thread-local `{name}`: blaze's loader gives a program its own thread-locals only, in the \
         loader's static reserve"
    )
}
