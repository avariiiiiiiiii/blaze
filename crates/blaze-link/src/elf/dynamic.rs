//! The regions only the ELF writer fills, from the placed image: `.interp`, the build ID,
//! the dynamic symbol table with its strings, GNU hash table and versions, `.rela.dyn`,
//! `.dynamic` and `.eh_frame_hdr`. A pure function of [`Placed`]: an incremental link asks
//! again and gets the same bytes for the same image.

use std::collections::HashMap;

use blaze_codegen::Arch;
use gimli::UnwindSection;
use object::elf;

use super::{
    BUILD_ID, DYNAMIC, DYNSTR, DYNSYM, EH_FRAME_HDR, Elf, GNU_HASH, INTERP, RELA_DYN, Segment, VERNEED,
    VERSYM, error,
};
use crate::model::{Class, Counts, Holds, Placed, Region, SymbolKind, Tls};
use crate::{LinkError, Output, OutputKind};

/// `.dynamic` entries besides `DT_NEEDED`: at most one of each tag [`Tables::of`] writes.
const DYNAMIC_FIXED: u64 = 25;
const SYM: u64 = 24;
const RELA: u64 = 24;

/// The synthetic regions of an output, in no particular order ([`super::Elf::order`] sorts).
fn names(output: &Output) -> Vec<&'static str> {
    let mut names = vec![BUILD_ID, DYNSYM, DYNSTR, GNU_HASH, VERSYM, VERNEED, RELA_DYN, DYNAMIC, EH_FRAME_HDR];
    if output.interpreter.is_some() {
        names.insert(0, INTERP);
    }
    names
}

fn soname(output: &Output) -> Option<&str> {
    match &output.kind {
        OutputKind::Shared { name, .. } => Some(name),
        OutputKind::Executable { .. } => None,
    }
}

/// How big each synthetic region may get, from the counts known before placement.
pub(crate) fn sizes(output: &Output, counts: &Counts) -> Vec<(Class, u64)> {
    let symbols = 1 + counts.imports + counts.exports;
    let soname = soname(output).map_or(0, |it| it.len() as u64 + 1);
    names(output)
        .into_iter()
        .map(|name| {
            let size = match name {
                INTERP => output.interpreter.as_ref().map_or(0, |it| it.len() as u64 + 1),
                BUILD_ID => NOTE_HEADER + 16,
                DYNSYM => SYM * symbols,
                DYNSTR => {
                    1 + counts.import_names
                        + 2 * counts.imports
                        + counts.export_names
                        + counts.exports
                        + counts.library_names
                        + counts.libraries
                        + soname
                }
                GNU_HASH => gnu_hash_len(counts.exports),
                VERSYM => 2 * symbols,
                VERNEED => 16 * (counts.libraries + counts.imports),
                RELA_DYN => RELA * (counts.absolute + counts.got),
                DYNAMIC => 16 * (counts.libraries + DYNAMIC_FIXED),
                EH_FRAME_HDR => 12 + 8 * counts.fdes,
                _ => unreachable!("a synthetic region `names` doesn't list"),
            };
            (Class::Synthetic(name), size)
        })
        .collect()
}

const NOTE_HEADER: u64 = 12 + 4;

/// Little-endian writes.
pub(crate) trait Put {
    fn u8(&mut self, value: u8);
    fn u16(&mut self, value: u16);
    fn u32(&mut self, value: u32);
    fn u64(&mut self, value: u64);
    fn align(&mut self, to: usize);
}

impl Put for Vec<u8> {
    fn u8(&mut self, value: u8) {
        self.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.extend_from_slice(&value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.extend_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.extend_from_slice(&value.to_le_bytes());
    }
    fn align(&mut self, to: usize) {
        self.resize(self.len().next_multiple_of(to), 0);
    }
}

/// A string table: `\0` first, each string once.
pub(crate) struct Strings {
    pub(crate) bytes: Vec<u8>,
    at: HashMap<String, u32>,
}

impl Strings {
    pub(crate) fn new() -> Strings {
        Strings { bytes: vec![0], at: HashMap::new() }
    }

    pub(crate) fn add(&mut self, string: &str) -> u32 {
        if string.is_empty() {
            return 0;
        }
        if let Some(at) = self.at.get(string) {
            return *at;
        }
        let at = self.bytes.len() as u32;
        self.bytes.extend_from_slice(string.as_bytes());
        self.bytes.push(0);
        self.at.insert(string.to_owned(), at);
        at
    }
}

/// One `Elf64_Sym`.
#[derive(Clone, Copy, Default)]
pub(crate) struct Sym {
    pub(crate) name: u32,
    pub(crate) info: u8,
    pub(crate) shndx: u16,
    pub(crate) value: u64,
    pub(crate) size: u64,
}

impl Sym {
    pub(crate) fn put(&self, out: &mut Vec<u8>) {
        out.u32(self.name);
        out.u8(self.info);
        out.u8(elf::STV_DEFAULT);
        out.u16(self.shndx);
        out.u64(self.value);
        out.u64(self.size);
    }
}

pub(crate) fn sym_type(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => elf::STT_FUNC,
        SymbolKind::Data => elf::STT_OBJECT,
        SymbolKind::Tls => elf::STT_TLS,
        SymbolKind::Section | SymbolKind::Other => elf::STT_NOTYPE,
    }
}

pub(crate) fn info(bind: u8, kind: SymbolKind) -> u8 {
    (bind << 4) | sym_type(kind)
}

/// The section header index of the region holding `addr`: one more than its index in
/// [`Placed::regions`], which [`super::image`] writes one section header each, in order.
pub(crate) fn section_of(regions: &[Region], addr: u64) -> Result<u16, LinkError> {
    let at = regions.partition_point(|it| it.addr <= addr);
    let Some(index) = at.checked_sub(1).filter(|&it| addr <= regions[it].addr + regions[it].capacity) else {
        return Ok(elf::SHN_ABS);
    };
    u16::try_from(index + 1)
        .ok()
        .filter(|&it| it < elf::SHN_LORESERVE)
        .ok_or_else(|| error(format!("{} regions: more sections than ELF numbers", regions.len())))
}

/// The value an exported or listed symbol has: thread-locals by their offset in the TLS
/// template, as the loader reads `STT_TLS` symbols.
pub(crate) fn value(placed: &Placed<'_>, name: &str, addr: u64, kind: SymbolKind) -> Result<u64, LinkError> {
    if kind != SymbolKind::Tls {
        return Ok(addr);
    }
    let tls = placed.tls.ok_or_else(|| error(format!("thread-local `{name}` in an image with no TLS")))?;
    addr.checked_sub(tls.addr)
        .filter(|&it| it <= tls.size)
        .ok_or_else(|| error(format!("thread-local `{name}` at {addr:#x} is outside the TLS template")))
}

/// The dynamic relocation types of an architecture.
struct Types {
    relative: u32,
    glob_dat: u32,
    abs64: u32,
    tpoff64: u32,
    dtpmod64: u32,
    dtpoff64: u32,
    tlsdesc: u32,
}

fn types(arch: Arch) -> Types {
    match arch {
        Arch::Aarch64 => Types {
            relative: elf::R_AARCH64_RELATIVE,
            glob_dat: elf::R_AARCH64_GLOB_DAT,
            abs64: elf::R_AARCH64_ABS64,
            tpoff64: elf::R_AARCH64_TLS_TPREL,
            dtpmod64: elf::R_AARCH64_TLS_DTPMOD,
            dtpoff64: elf::R_AARCH64_TLS_DTPREL,
            tlsdesc: elf::R_AARCH64_TLSDESC,
        },
        Arch::X86_64 | Arch::Wasm32 => Types {
            relative: elf::R_X86_64_RELATIVE,
            glob_dat: elf::R_X86_64_GLOB_DAT,
            abs64: elf::R_X86_64_64,
            tpoff64: elf::R_X86_64_TPOFF64,
            dtpmod64: elf::R_X86_64_DTPMOD64,
            dtpoff64: elf::R_X86_64_DTPOFF64,
            tlsdesc: elf::R_X86_64_TLSDESC,
        },
    }
}

/// One `Elf64_Rela`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Rela {
    pub(crate) at: u64,
    pub(crate) r_type: u32,
    pub(crate) sym: u32,
    pub(crate) addend: i64,
}

/// Every synthetic region's bytes, and what the headers need to know of them.
pub(crate) struct Tables {
    pub(crate) interp: Option<Vec<u8>>,
    pub(crate) note: Vec<u8>,
    pub(crate) dynsym: Vec<u8>,
    pub(crate) dynstr: Vec<u8>,
    pub(crate) gnu_hash: Vec<u8>,
    pub(crate) versym: Vec<u8>,
    pub(crate) verneed: Vec<u8>,
    /// The `Elf64_Verneed` entries in `verneed`: its section's `sh_info`.
    pub(crate) verneeds: u32,
    pub(crate) rela: Vec<u8>,
    pub(crate) dynamic: Vec<u8>,
    pub(crate) eh_frame_hdr: Vec<u8>,
    /// Whether `.eh_frame_hdr` indexes an `.eh_frame`, so `PT_GNU_EH_FRAME` points at it.
    pub(crate) has_eh_frame: bool,
}

impl Tables {
    pub(crate) fn of(writer: &Elf, placed: &Placed<'_>) -> Result<Tables, LinkError> {
        let output = &writer.output;
        let shared = matches!(output.kind, OutputKind::Shared { .. });
        for (at, import) in placed.imports.iter().enumerate() {
            if let Some(library) = import.library
                && library as usize >= placed.libraries.len()
            {
                return Err(error(format!("import {at} (`{}`) of library {library}, of no library", import.name)));
            }
        }

        // `--as-needed`: a library none of whose symbols are imported is left out.
        let needed: Vec<usize> = (0..placed.libraries.len())
            .filter(|&it| {
                !placed.libraries[it].as_needed
                    || placed.imports.iter().any(|import| import.library == Some(it as u32))
            })
            .collect();

        let mut strings = Strings::new();
        let needed_names: Vec<u32> = needed.iter().map(|&it| strings.add(&placed.libraries[it].name)).collect();
        let soname = soname(output).map(|it| strings.add(it));

        // Versions of imports, numbered from 2 by library in load order, then first use.
        let mut versions: Vec<(usize, Vec<(String, u16)>)> = needed.iter().map(|&it| (it, Vec::new())).collect();
        let mut next = 2u16;
        let mut version_of = vec![elf::VER_NDX_GLOBAL; placed.imports.len()];
        for (at, import) in placed.imports.iter().enumerate() {
            let (Some(library), Some(version)) = (import.library, &import.version) else { continue };
            let Some((_, list)) = versions.iter_mut().find(|(it, _)| *it == library as usize) else { continue };
            version_of[at] = match list.iter().find(|(name, _)| name == version) {
                Some((_, index)) => *index,
                None => {
                    let index = next;
                    next = next
                        .checked_add(1)
                        .filter(|&it| it < elf::VERSYM_HIDDEN)
                        .ok_or_else(|| error("more symbol versions than ELF numbers".into()))?;
                    list.push((version.clone(), index));
                    index
                }
            };
        }
        versions.retain(|(_, list)| !list.is_empty());

        // `.dynsym`: the null symbol, the imports, then the exports in GNU hash bucket order.
        let mut symbols = vec![Sym::default()];
        for import in placed.imports {
            let bind = if import.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
            symbols.push(Sym { name: strings.add(&import.name), info: info(bind, import.kind), ..Sym::default() });
        }
        let symoffset = symbols.len() as u32;
        let (buckets, mask_words) = gnu_hash_shape(placed.exports.len() as u64);
        let mut exports: Vec<(u32, u32, usize)> = placed
            .exports
            .iter()
            .enumerate()
            .map(|(at, it)| {
                let hash = gnu_hash(it.name.as_bytes());
                (hash % buckets as u32, hash, at)
            })
            .collect();
        exports.sort_by_key(|&(bucket, _, _)| bucket);
        for &(_, _, at) in &exports {
            let export = &placed.exports[at];
            symbols.push(Sym {
                name: strings.add(&export.name),
                info: info(elf::STB_GLOBAL, export.kind),
                shndx: section_of(placed.regions, export.addr)?,
                value: value(placed, &export.name, export.addr, export.kind)?,
                size: export.size,
            });
        }
        let mut dynsym = Vec::with_capacity(symbols.len() * SYM as usize);
        for symbol in &symbols {
            symbol.put(&mut dynsym);
        }

        let mut versym = Vec::with_capacity(symbols.len() * 2);
        versym.u16(elf::VER_NDX_LOCAL);
        for version in &version_of {
            versym.u16(*version);
        }
        for _ in &exports {
            versym.u16(elf::VER_NDX_GLOBAL);
        }

        let mut verneed = Vec::new();
        for (at, (library, list)) in versions.iter().enumerate() {
            let last = at + 1 == versions.len();
            verneed.u16(1);
            verneed.u16(list.len() as u16);
            verneed.u32(strings.add(&placed.libraries[*library].name));
            verneed.u32(16);
            verneed.u32(if last { 0 } else { 16 + 16 * list.len() as u32 });
            for (at, (name, index)) in list.iter().enumerate() {
                verneed.u32(elf_hash(name.as_bytes()));
                verneed.u16(0);
                verneed.u16(*index);
                verneed.u32(strings.add(name));
                verneed.u32(if at + 1 == list.len() { 0 } else { 16 });
            }
        }

        let mut gnu_hash = Vec::with_capacity(gnu_hash_len(exports.len() as u64) as usize);
        gnu_hash.u32(buckets as u32);
        gnu_hash.u32(symoffset);
        gnu_hash.u32(mask_words as u32);
        gnu_hash.u32(BLOOM_SHIFT);
        let mut bloom = vec![0u64; mask_words as usize];
        for &(_, hash, _) in &exports {
            let word = (hash as u64 / 64) % mask_words;
            bloom[word as usize] |= (1 << (hash % 64)) | (1 << ((hash >> BLOOM_SHIFT) % 64));
        }
        for word in bloom {
            gnu_hash.u64(word);
        }
        let mut first = vec![0u32; buckets as usize];
        for (at, &(bucket, _, _)) in exports.iter().enumerate().rev() {
            first[bucket as usize] = symoffset + at as u32;
        }
        for it in first {
            gnu_hash.u32(it);
        }
        for (at, &(bucket, hash, _)) in exports.iter().enumerate() {
            let last = exports.get(at + 1).is_none_or(|next| next.0 != bucket);
            gnu_hash.u32((hash & !1) | last as u32);
        }

        let relocations = relocations(writer, placed, shared)?;
        let relative = relocations.iter().take_while(|it| it.r_type == types(output.arch).relative).count();
        let mut rela = Vec::with_capacity(relocations.len() * RELA as usize);
        for it in &relocations {
            rela.u64(it.at);
            rela.u64((u64::from(it.sym) << 32) | u64::from(it.r_type));
            rela.u64(it.addend as u64);
        }

        let eh = eh_frame_hdr(placed)?;

        let address = |name: &'static str| {
            placed
                .region(&Class::Synthetic(name))
                .map(|(_, it)| it.addr)
                .ok_or_else(|| error(format!("the image has no `{name}` region")))
        };
        let mut dynamic = Vec::new();
        let mut entry = |tag: u32, value: u64| {
            dynamic.u64(u64::from(tag));
            dynamic.u64(value);
        };
        for name in needed_names {
            entry(elf::DT_NEEDED, u64::from(name));
        }
        if let Some(soname) = soname {
            entry(elf::DT_SONAME, u64::from(soname));
        }
        let static_tls = shared && relocations.iter().any(|it| it.r_type == types(output.arch).tpoff64);
        let flags = elf::DF_BIND_NOW
            | if shared { elf::DF_SYMBOLIC } else { 0 }
            | if static_tls { elf::DF_STATIC_TLS } else { 0 };
        entry(elf::DT_FLAGS, u64::from(flags));
        let flags_1 = elf::DF_1_NOW | if shared { 0 } else { elf::DF_1_PIE };
        entry(elf::DT_FLAGS_1, u64::from(flags_1));
        if let Some(init) = placed.init {
            entry(elf::DT_INIT, init);
        }
        if let Some(fini) = placed.fini {
            entry(elf::DT_FINI, fini);
        }
        for (class, tag, size) in [
            (preinit(), elf::DT_PREINIT_ARRAY, elf::DT_PREINIT_ARRAYSZ),
            (Class::InitArray, elf::DT_INIT_ARRAY, elf::DT_INIT_ARRAYSZ),
            (Class::FiniArray, elf::DT_FINI_ARRAY, elf::DT_FINI_ARRAYSZ),
        ] {
            if let Some((_, region)) = placed.region(&class) {
                entry(tag, region.addr);
                entry(size, region.size);
            }
        }
        entry(elf::DT_GNU_HASH, address(GNU_HASH)?);
        entry(elf::DT_STRTAB, address(DYNSTR)?);
        entry(elf::DT_SYMTAB, address(DYNSYM)?);
        entry(elf::DT_STRSZ, strings.bytes.len() as u64);
        entry(elf::DT_SYMENT, SYM);
        if !shared {
            entry(elf::DT_DEBUG, 0);
        }
        if !relocations.is_empty() {
            entry(elf::DT_RELA, address(RELA_DYN)?);
            entry(elf::DT_RELASZ, rela.len() as u64);
            entry(elf::DT_RELAENT, RELA);
            entry(elf::DT_RELACOUNT, relative as u64);
        }
        if !versions.is_empty() {
            entry(elf::DT_VERSYM, address(VERSYM)?);
            entry(elf::DT_VERNEED, address(VERNEED)?);
            entry(elf::DT_VERNEEDNUM, versions.len() as u64);
        }
        entry(elf::DT_NULL, 0);

        let mut note = Vec::with_capacity((NOTE_HEADER + 16) as usize);
        note.u32(4);
        note.u32(16);
        note.u32(elf::NT_GNU_BUILD_ID);
        note.extend_from_slice(b"GNU\0");
        note.extend_from_slice(&placed.id);

        Ok(Tables {
            interp: output.interpreter.as_ref().map(|it| [it.as_bytes(), &[0]].concat()),
            note,
            dynsym,
            dynstr: strings.bytes,
            gnu_hash,
            versym,
            verneed,
            verneeds: versions.len() as u32,
            rela,
            dynamic,
            eh_frame_hdr: eh.0,
            has_eh_frame: eh.1,
        })
    }

    /// The bytes of the synthetic region `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&[u8]> {
        Some(match name {
            INTERP => self.interp.as_deref()?,
            BUILD_ID => &self.note,
            DYNSYM => &self.dynsym,
            DYNSTR => &self.dynstr,
            GNU_HASH => &self.gnu_hash,
            VERSYM => &self.versym,
            VERNEED => &self.verneed,
            RELA_DYN => &self.rela,
            DYNAMIC => &self.dynamic,
            EH_FRAME_HDR => &self.eh_frame_hdr,
            _ => return None,
        })
    }

    pub(crate) fn regions(self, output: &Output) -> Vec<(Class, Vec<u8>)> {
        names(output)
            .into_iter()
            .map(|name| (Class::Synthetic(name), self.get(name).unwrap_or_default().to_vec()))
            .collect()
    }
}

fn preinit() -> Class {
    Class::Named { name: super::PREINIT_ARRAY.into(), write: true, exec: false }
}

/// The region of `regions` (in address order) holding `addr`, by its placed size.
fn region_at(regions: &[Region], addr: u64) -> Option<usize> {
    let at = regions.partition_point(|it| it.addr <= addr).checked_sub(1)?;
    (addr < regions[at].addr + regions[at].size.max(regions[at].capacity)).then_some(at)
}

/// `.rela.dyn`: `RELATIVE` for every slid address (sorted, first, counted by
/// `DT_RELACOUNT`), then what binds to imports and thread-locals, by address.
fn relocations(writer: &Elf, placed: &Placed<'_>, shared: bool) -> Result<Vec<Rela>, LinkError> {
    let types = types(writer.output.arch);
    let import = |index: u32| -> Result<u32, LinkError> {
        if index as usize >= placed.imports.len() {
            return Err(error(format!("a dynamic relocation to import {index}, of no import")));
        }
        Ok(1 + index)
    };
    let writable = |at: u64, what: &str| -> Result<usize, LinkError> {
        let region = region_at(placed.regions, at)
            .ok_or_else(|| error(format!("{what} at {at:#x}, in no region of the image")))?;
        let class = &placed.regions[region].class;
        if !Segment::of(class).loader_writes() {
            return Err(error(format!(
                "{what} at {at:#x} is in {class:?}, which the loader can't write (a text relocation)"
            )));
        }
        Ok(region)
    };

    let mut relative = Vec::with_capacity(placed.rebase.len());
    for &at in placed.rebase {
        let region = writable(at, "a slid address")?;
        let offset = (at - placed.regions[region].addr) as usize;
        let bytes = placed.bytes(region);
        let value = bytes
            .get(offset..offset + 8)
            .ok_or_else(|| error(format!("a slid address at {at:#x}, outside the bytes of its region")))?;
        let value = u64::from_le_bytes(value.try_into().expect("8 bytes"));
        relative.push(Rela { at, r_type: types.relative, sym: 0, addend: value as i64 });
    }
    relative.sort_unstable();

    let mut other = Vec::new();
    for bind in placed.binds {
        writable(bind.addr, "a bound address")?;
        other.push(Rela { at: bind.addr, r_type: types.abs64, sym: import(bind.import)?, addend: bind.addend });
    }
    for cell in placed.got {
        let at = cell.addr;
        writable(at, "a GOT cell")?;
        match &cell.holds {
            Holds::Local(_) => {}
            // A shared library's TLS block is placed when it is loaded: the loader adds the
            // block's offset from the thread pointer to the variable's offset in it.
            &Holds::TpOff(tp) if shared => {
                let tls = placed.tls.ok_or_else(|| {
                    error(format!("the GOT cell at {at:#x} holds a thread pointer offset in an image with no TLS"))
                })?;
                let addend = block_offset(writer.output.arch, tls, tp);
                if addend < 0 || addend as u64 > tls.size {
                    return Err(error(format!(
                        "the GOT cell at {at:#x} holds thread pointer offset {tp}, outside the TLS template"
                    )));
                }
                other.push(Rela { at, r_type: types.tpoff64, sym: 0, addend });
            }
            Holds::TpOff(_) => {}
            Holds::Import(index) => {
                other.push(Rela { at, r_type: types.glob_dat, sym: import(*index)?, addend: 0 })
            }
            Holds::ImportTpOff(index) => {
                other.push(Rela { at, r_type: types.tpoff64, sym: import(*index)?, addend: 0 })
            }
            Holds::TlsGd { import: index, offset } => {
                let sym = index.map(import).transpose()?.unwrap_or(0);
                other.push(Rela { at, r_type: types.dtpmod64, sym, addend: 0 });
                other.push(Rela { at: at + 8, r_type: types.dtpoff64, sym, addend: *offset as i64 });
            }
            Holds::TlsDesc { import: index, offset } => {
                let sym = index.map(import).transpose()?.unwrap_or(0);
                other.push(Rela { at, r_type: types.tlsdesc, sym, addend: *offset as i64 });
            }
        }
    }
    other.sort_unstable();
    relative.extend(other);
    Ok(relative)
}

/// The offset in the TLS template of the thread-local at `tp` from the thread pointer: the
/// inverse of [`crate::reloc::tp_offset`].
fn block_offset(arch: Arch, tls: Tls, tp: i64) -> i64 {
    let align = tls.align.max(1);
    match arch {
        Arch::Aarch64 => tp - 16u64.next_multiple_of(align) as i64,
        Arch::X86_64 | Arch::Wasm32 => tp + tls.size.next_multiple_of(align) as i64,
    }
}

const BLOOM_SHIFT: u32 = 26;

pub(super) fn gnu_hash(name: &[u8]) -> u32 {
    name.iter().fold(5381u32, |hash, &it| hash.wrapping_mul(33).wrapping_add(u32::from(it)))
}

/// The SysV ELF hash, of version names in `.gnu.version_r`.
pub(super) fn elf_hash(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &it in name {
        hash = (hash << 4).wrapping_add(u32::from(it));
        let high = hash & 0xf000_0000;
        if high != 0 {
            hash ^= high >> 24;
        }
        hash &= !high;
    }
    hash
}

/// lld's shape for `n` exports: a bucket per 4, a bloom filter of 12 bits per symbol in
/// a power of two of 64-bit words.
fn gnu_hash_shape(n: u64) -> (u64, u64) {
    ((n / 4).max(1), (n * 12 / 64 + 1).next_power_of_two())
}

fn gnu_hash_len(n: u64) -> u64 {
    let (buckets, mask_words) = gnu_hash_shape(n);
    16 + 8 * mask_words + 4 * buckets + 4 * n
}

fn rel32(to: u64, from: u64, what: &str) -> Result<u32, LinkError> {
    i32::try_from(to.wrapping_sub(from) as i64)
        .map(|it| it as u32)
        .map_err(|_| error(format!("{what}: {to:#x} is out of 32-bit reach of {from:#x}")))
}

/// `.eh_frame_hdr`: the binary search table of every FDE in [`Class::EhFrame`] (the
/// functions' and the native objects'), sorted by the address each covers. Encodings as
/// lld writes them: `pcrel|sdata4` to `.eh_frame`, a `udata4` count, `datarel|sdata4` pairs.
fn eh_frame_hdr(placed: &Placed<'_>) -> Result<(Vec<u8>, bool), LinkError> {
    let hdr = placed
        .region(&Class::Synthetic(EH_FRAME_HDR))
        .map(|(_, it)| it.addr)
        .ok_or_else(|| error(format!("the image has no `{EH_FRAME_HDR}` region")))?;
    let mut table = Vec::new();
    let eh_frame = placed.region(&Class::EhFrame);
    if let Some((index, region)) = eh_frame {
        let mut section = gimli::EhFrame::new(placed.bytes(index), gimli::LittleEndian);
        section.set_address_size(8);
        let bases = gimli::BaseAddresses::default().set_eh_frame(region.addr);
        let mut entries = section.entries(&bases);
        let bad = |it: gimli::Error| error(format!("the image's .eh_frame at {:#x}: {it}", region.addr));
        while let Some(entry) = entries.next().map_err(bad)? {
            if let gimli::CieOrFde::Fde(partial) = entry {
                let fde = partial.parse(gimli::EhFrame::cie_from_offset).map_err(bad)?;
                table.push((fde.initial_address(), region.addr + fde.offset() as u64));
            }
        }
    }
    table.sort_unstable();
    let mut bytes = Vec::with_capacity(12 + 8 * table.len());
    bytes.u8(1);
    bytes.u8(gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0);
    bytes.u8(gimli::DW_EH_PE_udata4.0);
    bytes.u8(gimli::DW_EH_PE_datarel.0 | gimli::DW_EH_PE_sdata4.0);
    let eh_frame_addr = eh_frame.map_or(hdr + 4, |(_, it)| it.addr);
    bytes.u32(rel32(eh_frame_addr, hdr + 4, ".eh_frame from .eh_frame_hdr")?);
    bytes.u32(u32::try_from(table.len()).map_err(|_| error("more FDEs than .eh_frame_hdr counts".into()))?);
    for (pc, fde) in table {
        bytes.u32(rel32(pc, hdr, "a function from .eh_frame_hdr")?);
        bytes.u32(rel32(fde, hdr, "an FDE from .eh_frame_hdr")?);
    }
    Ok((bytes, eh_frame.is_some()))
}
