//! The regions only the ELF writer fills, from the placed image: `.interp`, the build ID,
//! the dynamic symbol table with its strings, GNU hash table and versions, `.rela.dyn`,
//! `.dynamic` and `.eh_frame_hdr`. A pure function of [`Placed`]: an incremental link asks
//! again and gets the same bytes for the same image, on any number of cores.

use std::collections::hash_map::Entry;

use blaze_codegen::Arch;
use blaze_ids::radix;
use gimli::UnwindSection;
use object::elf;
use rayon::prelude::*;
use rustc_hash::FxHashMap;

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

/// Items per rayon task: below this a split costs more than the work it shares.
pub(crate) const CHUNK: usize = 4096;

/// The first error in input order, so a failed link names the same place on any core count.
pub(crate) fn first_error(results: Vec<Result<(), LinkError>>) -> Result<(), LinkError> {
    results.into_iter().find_map(Result::err).map_or(Ok(()), Err)
}

/// Sorts relocations in their derived order: by addend, then stably by the fields before it.
/// Free when they are sorted already (the core hands over addresses in order).
fn sort_rela(items: &mut [Rela]) {
    if items.is_sorted() {
        return;
    }
    radix::sort_copy_by_key(items, |it| radix::signed_64(it.addend));
    radix::sort_copy_by_key(items, |it| radix::pack_64(it.at, radix::pack_32(it.r_type, it.sym)));
}

/// Every synthetic region, `.interp` first.
const SYNTHETIC: [&str; 10] =
    [INTERP, BUILD_ID, DYNSYM, DYNSTR, GNU_HASH, VERSYM, VERNEED, RELA_DYN, DYNAMIC, EH_FRAME_HDR];

/// The synthetic regions of an output, in no particular order ([`super::Elf::order`] sorts):
/// `.interp` only with an interpreter.
fn names(output: &Output) -> &'static [&'static str] {
    match output.interpreter {
        Some(_) => &SYNTHETIC,
        None => &SYNTHETIC[1..],
    }
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
        .iter()
        .map(|&name| {
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
        .collect() // alloc: once per image, what `Writer::synthetic` returns
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

/// A string table: `\0` first, each string once. Keys borrow the image's names: one FxHash
/// probe (entry API) per string and no allocation per string.
pub(crate) struct Strings<'a> {
    pub(crate) bytes: Vec<u8>,
    at: FxHashMap<&'a str, u32>,
}

impl<'a> Strings<'a> {
    /// Room for `strings` strings of `bytes` bytes with their terminators.
    pub(crate) fn with_capacity(bytes: usize, strings: usize) -> Strings<'a> {
        let mut table = Vec::with_capacity(1 + bytes);
        table.push(0);
        Strings { bytes: table, at: FxHashMap::with_capacity_and_hasher(strings, Default::default()) }
    }

    pub(crate) fn add(&mut self, string: &'a str) -> u32 {
        if string.is_empty() {
            return 0;
        }
        let next = self.bytes.len() as u32;
        let at = *self.at.entry(string).or_insert(next);
        if at == next {
            self.bytes.extend_from_slice(string.as_bytes());
            self.bytes.push(0);
        }
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
    pub(crate) fn bytes(&self) -> [u8; SYM as usize] {
        let mut out = [0; SYM as usize];
        out[0..4].copy_from_slice(&self.name.to_le_bytes());
        out[4] = self.info;
        out[5] = elf::STV_DEFAULT;
        out[6..8].copy_from_slice(&self.shndx.to_le_bytes());
        out[8..16].copy_from_slice(&self.value.to_le_bytes());
        out[16..24].copy_from_slice(&self.size.to_le_bytes());
        out
    }

    pub(crate) fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bytes());
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

/// Whether every region's section header has an ordinary ELF number, as [`shndx`] needs.
pub(crate) fn numbered(regions: &[Region]) -> Result<(), LinkError> {
    if regions.len() + 1 >= usize::from(elf::SHN_LORESERVE) {
        return Err(error(format!("{} regions: more sections than ELF numbers", regions.len()))); // alloc: error path
    }
    Ok(())
}

/// The section header index of the region holding `addr`, once [`numbered`] held: one more
/// than its index in [`Placed::regions`], which [`super::image`] writes one section header
/// each, in order. A binary search over the regions.
pub(crate) fn shndx(regions: &[Region], addr: u64) -> u16 {
    let at = regions.partition_point(|it| it.addr <= addr);
    match at.checked_sub(1).filter(|&it| addr <= regions[it].addr + regions[it].capacity) {
        Some(index) => (index + 1) as u16,
        None => elf::SHN_ABS,
    }
}

/// The value an exported or listed symbol has: thread-locals by their offset in the TLS
/// template, as the loader reads `STT_TLS` symbols.
pub(crate) fn value(placed: &Placed<'_>, name: &str, addr: u64, kind: SymbolKind) -> Result<u64, LinkError> {
    if kind != SymbolKind::Tls {
        return Ok(addr);
    }
    let tls = placed.tls.ok_or_else(|| error(format!("thread-local `{name}` in an image with no TLS")))?; // alloc: error path
    addr.checked_sub(tls.addr)
        .filter(|&it| it <= tls.size)
        .ok_or_else(|| error(format!("thread-local `{name}` at {addr:#x} is outside the TLS template"))) // alloc: error path
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
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Rela {
    pub(crate) at: u64,
    pub(crate) r_type: u32,
    pub(crate) sym: u32,
    pub(crate) addend: i64,
}

const _: () = assert!(size_of::<Rela>() == RELA as usize);

impl Rela {
    fn bytes(&self) -> [u8; RELA as usize] {
        let mut out = [0; RELA as usize];
        out[0..8].copy_from_slice(&self.at.to_le_bytes());
        out[8..16].copy_from_slice(&((u64::from(self.sym) << 32) | u64::from(self.r_type)).to_le_bytes());
        out[16..24].copy_from_slice(&self.addend.to_le_bytes());
        out
    }
}

/// Every synthetic region's bytes.
pub(crate) struct Tables {
    pub(crate) interp: Option<Vec<u8>>,
    pub(crate) note: Vec<u8>,
    pub(crate) dynsym: Vec<u8>,
    pub(crate) dynstr: Vec<u8>,
    pub(crate) gnu_hash: Vec<u8>,
    pub(crate) versym: Vec<u8>,
    pub(crate) verneed: Vec<u8>,
    pub(crate) rela: Vec<u8>,
    pub(crate) dynamic: Vec<u8>,
    pub(crate) eh_frame_hdr: Vec<u8>,
}

impl Tables {
    pub(crate) fn of(writer: &Elf, placed: &Placed<'_>) -> Result<Tables, LinkError> {
        let output = &writer.output;
        let shared = matches!(output.kind, OutputKind::Shared { .. });
        let libraries = placed.libraries;
        let imports = placed.imports;
        let exports = placed.exports;

        // `--as-needed`: a library none of whose symbols are imported is left out. By library,
        // its place among the needed ones in load order, or NOT_NEEDED; an import marks its
        // library 0 until they are numbered.
        const NOT_NEEDED: u32 = u32::MAX;
        // alloc: once per image, 4 bytes per library
        let mut slot = vec![NOT_NEEDED; libraries.len()];
        for (at, import) in imports.iter().enumerate() {
            let Some(library) = import.library else { continue };
            let Some(flag) = slot.get_mut(library as usize) else {
                // alloc: error path
                return Err(error(format!("import {at} (`{}`) of library {library}, of no library", import.name)));
            };
            *flag = 0;
        }

        let string_bytes = libraries.iter().map(|it| it.name.len() + 1).sum::<usize>()
            + soname(output).map_or(0, |it| it.len() + 1)
            + imports.iter().map(|it| it.name.len() + 1 + it.version.as_ref().map_or(0, |it| it.len() + 1)).sum::<usize>()
            + exports.iter().map(|it| it.name.len() + 1).sum::<usize>();
        let mut strings = Strings::with_capacity(string_bytes, libraries.len() + 1 + 2 * imports.len() + exports.len());
        // By needed library: its name's offset in `.dynstr`.
        let mut needed_names = Vec::with_capacity(libraries.len());
        for (at, library) in libraries.iter().enumerate() {
            if library.as_needed && slot[at] == NOT_NEEDED {
                continue;
            }
            slot[at] = needed_names.len() as u32;
            needed_names.push(strings.add(&library.name));
        }
        let soname = soname(output).map(|it| strings.add(it));

        // Versions of imports, numbered from 2 in order of first use, one probe per import;
        // `.gnu.version` takes each import's as it is found. `needs` holds each version once
        // with its library's slot, then is sorted stably by slot: each library's versions are
        // a run of it, in the order they were numbered.
        let symbols = 1 + imports.len() + exports.len();
        let symoffset = 1 + imports.len() as u32;
        let mut versym = Vec::with_capacity(symbols * 2);
        versym.u16(elf::VER_NDX_LOCAL);
        let versioned = imports.iter().filter(|it| it.library.is_some() && it.version.is_some()).count();
        let mut needs: Vec<(u32, &str, u16)> = Vec::with_capacity(versioned);
        let mut version_index: FxHashMap<(u32, &str), u16> =
            FxHashMap::with_capacity_and_hasher(versioned, Default::default());
        let mut next = 2u16;
        for import in imports {
            let version = match (import.library, &import.version) {
                (Some(library), Some(version)) if slot[library as usize] != NOT_NEEDED => {
                    let slot = slot[library as usize];
                    match version_index.entry((slot, version.as_str())) {
                        Entry::Occupied(it) => *it.get(),
                        Entry::Vacant(it) => {
                            let index = next;
                            next = next
                                .checked_add(1)
                                .filter(|&it| it < elf::VERSYM_HIDDEN)
                                .ok_or_else(|| error("more symbol versions than ELF numbers".into()))?;
                            needs.push((slot, version.as_str(), index));
                            *it.insert(index)
                        }
                    }
                }
                _ => elf::VER_NDX_GLOBAL,
            };
            versym.u16(version);
        }
        versym.resize(symbols * 2, 0);
        for pair in versym[2 * symoffset as usize..].chunks_exact_mut(2) {
            pair.copy_from_slice(&elf::VER_NDX_GLOBAL.to_le_bytes());
        }
        radix::sort_copy_by_key(&mut needs, |it| it.0);

        // `.gnu.hash` as mold builds it: every hash once (in parallel when there are many),
        // then the exports in bucket order by a stable counting sort, the order a stable sort
        // by bucket gives, in two passes.
        let (buckets, mask_words) = gnu_hash_shape(exports.len() as u64);
        let mut hashes = Vec::with_capacity(exports.len());
        if exports.len() >= CHUNK {
            hashes.par_extend(exports.par_iter().with_min_len(CHUNK).map(|it| gnu_hash(it.name.as_bytes())));
        } else {
            hashes.extend(exports.iter().map(|it| gnu_hash(it.name.as_bytes())));
        }
        let bucket_of = |hash: u32| (hash % buckets as u32) as usize;
        // `ends[b + 1]` counts bucket `b`; summed, `ends[b]` is its start, which placing its
        // exports advances to its end.
        // alloc: once per image, 4 bytes per bucket
        let mut ends = vec![0u32; buckets as usize + 1];
        for &hash in &hashes {
            ends[bucket_of(hash) + 1] += 1;
        }
        for at in 1..ends.len() {
            ends[at] += ends[at - 1];
        }
        // alloc: once per image, 4 bytes per export
        let mut order = vec![0u32; exports.len()];
        for (at, &hash) in hashes.iter().enumerate() {
            let bucket = bucket_of(hash);
            order[ends[bucket] as usize] = at as u32;
            ends[bucket] += 1;
        }
        let bucket_start = |bucket: usize| if bucket == 0 { 0 } else { ends[bucket - 1] };

        // `.dynsym`: the null symbol, the imports, then the exports in bucket order.
        let mut dynsym = Vec::with_capacity(symbols * SYM as usize);
        Sym::default().put(&mut dynsym);
        for import in imports {
            let bind = if import.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
            Sym { name: strings.add(&import.name), info: info(bind, import.kind), ..Sym::default() }.put(&mut dynsym);
        }
        numbered(placed.regions)?;
        for &at in &order {
            let export = &exports[at as usize];
            Sym {
                name: strings.add(&export.name),
                info: info(elf::STB_GLOBAL, export.kind),
                shndx: shndx(placed.regions, export.addr),
                value: value(placed, &export.name, export.addr, export.kind)?,
                size: export.size,
            }
            .put(&mut dynsym);
        }

        let versioned_libraries = needs.chunk_by(|a, b| a.0 == b.0).count();
        let mut verneed = Vec::with_capacity(16 * (versioned_libraries + needs.len()));
        for (at, list) in needs.chunk_by(|a, b| a.0 == b.0).enumerate() {
            let last = at + 1 == versioned_libraries;
            verneed.u16(1);
            verneed.u16(list.len() as u16);
            verneed.u32(needed_names[list[0].0 as usize]);
            verneed.u32(16);
            verneed.u32(if last { 0 } else { 16 + 16 * list.len() as u32 });
            for (at, &(_, name, index)) in list.iter().enumerate() {
                verneed.u32(elf_hash(name.as_bytes()));
                verneed.u16(0);
                verneed.u16(index);
                verneed.u32(strings.add(name));
                verneed.u32(if at + 1 == list.len() { 0 } else { 16 });
            }
        }

        let mut gnu_hash = Vec::with_capacity(gnu_hash_len(exports.len() as u64) as usize);
        gnu_hash.u32(buckets as u32);
        gnu_hash.u32(symoffset);
        gnu_hash.u32(mask_words as u32);
        gnu_hash.u32(BLOOM_SHIFT);
        // The bloom filter, set in place.
        let bloom = gnu_hash.len();
        gnu_hash.resize(bloom + 8 * mask_words as usize, 0);
        for &hash in &hashes {
            let word = bloom + 8 * ((hash as u64 / 64) % mask_words) as usize;
            let bits = (1u64 << (hash % 64)) | (1u64 << ((hash >> BLOOM_SHIFT) % 64));
            let cell: &mut [u8; 8] = (&mut gnu_hash[word..word + 8]).try_into().expect("8 bytes");
            *cell = (u64::from_le_bytes(*cell) | bits).to_le_bytes();
        }
        for bucket in 0..buckets as usize {
            let (start, end) = (bucket_start(bucket), ends[bucket]);
            gnu_hash.u32(if start == end { 0 } else { symoffset + start });
        }
        for (at, &export) in order.iter().enumerate() {
            let hash = hashes[export as usize];
            let last = at as u32 + 1 == ends[bucket_of(hash)];
            gnu_hash.u32((hash & !1) | last as u32);
        }

        let (relocations, relative) = relocations(writer, placed, shared)?;
        // alloc: the output buffer, `.rela.dyn`'s bytes
        let mut rela = vec![0; relocations.len() * RELA as usize];
        rela.par_chunks_mut(RELA as usize * CHUNK).zip(relocations.par_chunks(CHUNK)).for_each(|(out, chunk)| {
            for (out, it) in out.chunks_exact_mut(RELA as usize).zip(chunk) {
                out.copy_from_slice(&it.bytes());
            }
        });

        let eh_frame_hdr = eh_frame_hdr(placed)?;

        let address = |name: &'static str| {
            placed
                .region(&Class::Synthetic(name))
                .map(|(_, it)| it.addr)
                .ok_or_else(|| error(format!("the image has no `{name}` region"))) // alloc: error path
        };
        let mut dynamic = Vec::with_capacity(16 * (needed_names.len() + DYNAMIC_FIXED as usize));
        let mut entry = |tag: u32, value: u64| {
            dynamic.u64(u64::from(tag));
            dynamic.u64(value);
        };
        for &name in &needed_names {
            entry(elf::DT_NEEDED, u64::from(name));
        }
        if let Some(soname) = soname {
            entry(elf::DT_SONAME, u64::from(soname));
        }
        let tpoff64 = types(output.arch).tpoff64;
        let static_tls = shared && relocations[relative..].iter().any(|it| it.r_type == tpoff64);
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
        if versioned_libraries != 0 {
            entry(elf::DT_VERSYM, address(VERSYM)?);
            entry(elf::DT_VERNEED, address(VERNEED)?);
            entry(elf::DT_VERNEEDNUM, versioned_libraries as u64);
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
            rela,
            dynamic,
            eh_frame_hdr,
        })
    }

    /// The bytes of the synthetic region `name`, moved out.
    fn take(&mut self, name: &str) -> Vec<u8> {
        std::mem::take(match name {
            INTERP => return self.interp.take().unwrap_or_default(),
            BUILD_ID => &mut self.note,
            DYNSYM => &mut self.dynsym,
            DYNSTR => &mut self.dynstr,
            GNU_HASH => &mut self.gnu_hash,
            VERSYM => &mut self.versym,
            VERNEED => &mut self.verneed,
            RELA_DYN => &mut self.rela,
            DYNAMIC => &mut self.dynamic,
            EH_FRAME_HDR => &mut self.eh_frame_hdr,
            _ => unreachable!("a synthetic region `names` doesn't list"),
        })
    }

    pub(crate) fn regions(mut self, output: &Output) -> Vec<(Class, Vec<u8>)> {
        // alloc: once per image, what `Writer::synthesize` returns
        names(output).iter().map(|&name| (Class::Synthetic(name), self.take(name))).collect()
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

/// Whether the loader may write `at`: the region holding it, or an error naming a text
/// relocation.
fn writable(regions: &[Region], at: u64, what: &str) -> Result<usize, LinkError> {
    let region = region_at(regions, at).ok_or_else(|| error(format!("{what} at {at:#x}, in no region of the image")))?; // alloc: error path
    let class = &regions[region].class;
    if !Segment::of(class).loader_writes() {
        // alloc: error path
        return Err(error(format!("{what} at {at:#x} is in {class:?}, which the loader can't write (a text relocation)")));
    }
    Ok(region)
}

/// `.rela.dyn`: `RELATIVE` for every slid address (sorted, first, counted by
/// `DT_RELACOUNT`), then what binds to imports and thread-locals, by address; and how many
/// are `RELATIVE`. One buffer of room for both: the slid addresses, most of the table, are
/// read into it in parallel, and the rest pushed after them and sorted in place.
fn relocations(writer: &Elf, placed: &Placed<'_>, shared: bool) -> Result<(Vec<Rela>, usize), LinkError> {
    let types = types(writer.output.arch);
    let import = |index: u32| -> Result<u32, LinkError> {
        if index as usize >= placed.imports.len() {
            return Err(error(format!("a dynamic relocation to import {index}, of no import"))); // alloc: error path
        }
        Ok(1 + index)
    };
    let regions = placed.regions;
    let relative_type = types.relative;
    let other_cells = placed.binds.len() + 2 * placed.got.len();
    let mut relative = Vec::with_capacity(placed.rebase.len() + other_cells);
    relative.resize(placed.rebase.len(), Rela::default());
    // alloc: once per image, one result per chunk of slid addresses
    let results: Vec<Result<(), LinkError>> = relative
        .par_chunks_mut(CHUNK)
        .zip(placed.rebase.par_chunks(CHUNK))
        .map(|(out, ats)| {
            for (out, &at) in out.iter_mut().zip(ats) {
                let region = writable(regions, at, "a slid address")?;
                let offset = (at - regions[region].addr) as usize;
                let value = placed
                    .bytes(region)
                    .get(offset..offset + 8)
                    .ok_or_else(|| error(format!("a slid address at {at:#x}, outside the bytes of its region")))?; // alloc: error path
                let value = u64::from_le_bytes(value.try_into().expect("8 bytes"));
                *out = Rela { at, r_type: relative_type, sym: 0, addend: value as i64 };
            }
            Ok(())
        })
        .collect();
    first_error(results)?;
    sort_rela(&mut relative);
    let count = relative.len();

    let other = &mut relative;
    let writable = |at: u64, what: &str| writable(regions, at, what);
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
                    // alloc: error path
                    error(format!("the GOT cell at {at:#x} holds a thread pointer offset in an image with no TLS"))
                })?;
                let addend = block_offset(writer.output.arch, tls, tp);
                if addend < 0 || addend as u64 > tls.size {
                    // alloc: error path
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
    sort_rela(&mut relative[count..]);
    Ok((relative, count))
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
        .map_err(|_| error(format!("{what}: {to:#x} is out of 32-bit reach of {from:#x}"))) // alloc: error path
}

/// `.eh_frame_hdr`: the binary search table of every FDE in [`Class::EhFrame`] (the
/// functions' and the native objects'), sorted by the address each covers. Encodings as
/// lld writes them: `pcrel|sdata4` to `.eh_frame`, a `udata4` count, `datarel|sdata4` pairs.
///
/// One pass over the section: each CIE is parsed once, where the walk meets it, and its FDEs
/// take it from there rather than parsing it again each.
fn eh_frame_hdr(placed: &Placed<'_>) -> Result<Vec<u8>, LinkError> {
    let hdr = placed
        .region(&Class::Synthetic(EH_FRAME_HDR))
        .map(|(_, it)| it.addr)
        .ok_or_else(|| error(format!("the image has no `{EH_FRAME_HDR}` region")))?; // alloc: error path
    let mut table = Vec::with_capacity(placed.functions.len());
    let eh_frame = placed.region(&Class::EhFrame);
    if let Some((index, region)) = eh_frame {
        let mut section = gimli::EhFrame::new(placed.bytes(index), gimli::LittleEndian);
        section.set_address_size(8);
        let bases = gimli::BaseAddresses::default().set_eh_frame(region.addr);
        let mut entries = section.entries(&bases);
        let bad = |it: gimli::Error| error(format!("the image's .eh_frame at {:#x}: {it}", region.addr)); // alloc: error path
        let mut cies = FxHashMap::default();
        while let Some(entry) = entries.next().map_err(bad)? {
            match entry {
                gimli::CieOrFde::Cie(cie) => {
                    cies.insert(cie.offset(), cie);
                }
                gimli::CieOrFde::Fde(partial) => {
                    let fde = partial
                        .parse(|section, bases, offset| match cies.get(&offset.0) {
                            // clone: a gimli CIE is plain data borrowing the section, no heap; `parse` takes it by value
                            Some(cie) => Ok(cie.clone()),
                            None => section.cie_from_offset(bases, offset),
                        })
                        .map_err(bad)?;
                    table.push((fde.initial_address(), region.addr + fde.offset() as u64));
                }
            }
        }
    }
    radix::sort_copy_by_key(&mut table, |&(pc, fde): &(u64, u64)| radix::pack_64(pc, fde));
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
    Ok(bytes)
}
