//! The file around the placed regions: the ELF header and program headers at its start, and
//! after the last region the symbol table, the debug sections and the section headers.
//!
//! [`file`] reads the synthetic regions' bytes from [`Placed::contents`], where the core puts
//! what [`crate::model::Writer::synthesize`] returned, so no table is built twice.

use object::elf;
use rayon::prelude::*;

use super::dynamic::{CHUNK, Put, Strings, Sym, info, numbered, shndx, value};
use super::{
    BUILD_ID, DYNAMIC, DYNSTR, DYNSYM, EH_FRAME_HDR, EHDR, Elf, GNU_HASH, INTERP, PHDR, PHDRS, PREINIT_ARRAY,
    RELA_DYN, Segment, VERNEED, VERSYM, error, in_file,
};
use crate::model::{Class, File, Placed, SymbolKind};
use crate::{LinkError, OutputKind};

const SYM: usize = 24;
const SHDR: usize = 64;

/// The bytes a symbol name takes in `.strtab`: none for an empty one (offset 0).
fn stored(name: &str) -> usize {
    if name.is_empty() { 0 } else { name.len() + 1 }
}

/// A run of `.strtab` laid out beforehand: names written in order, each at the next offset,
/// its terminator already 0.
struct Names<'b> {
    bytes: &'b mut [u8],
    at: usize,
    /// The `.strtab` offset of `bytes[0]`.
    offset: u32,
}

impl Names<'_> {
    fn add(&mut self, name: &str) -> u32 {
        if name.is_empty() {
            return 0;
        }
        self.bytes[self.at..self.at + name.len()].copy_from_slice(name.as_bytes());
        let offset = self.offset + self.at as u32;
        self.at += name.len() + 1;
        offset
    }
}

/// One `PT_LOAD`: from `vaddr`, its bytes in the file to `file_end`, in memory to `end`.
struct Load {
    segment: Segment,
    vaddr: u64,
    file_end: u64,
    end: u64,
    /// A region not in the file was placed: what follows can't be either.
    zeroed: bool,
}

struct Phdr {
    p_type: u32,
    flags: u32,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

#[derive(Default)]
struct Shdr {
    name: u32,
    sh_type: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
}

impl Shdr {
    /// Its `Elf64_Shdr`.
    fn bytes(&self) -> [u8; SHDR] {
        let mut out = [0; SHDR];
        out[0..4].copy_from_slice(&self.name.to_le_bytes());
        out[4..8].copy_from_slice(&self.sh_type.to_le_bytes());
        out[8..16].copy_from_slice(&self.flags.to_le_bytes());
        out[16..24].copy_from_slice(&self.addr.to_le_bytes());
        out[24..32].copy_from_slice(&self.offset.to_le_bytes());
        out[32..40].copy_from_slice(&self.size.to_le_bytes());
        out[40..44].copy_from_slice(&self.link.to_le_bytes());
        out[44..48].copy_from_slice(&self.info.to_le_bytes());
        out[48..56].copy_from_slice(&self.align.to_le_bytes());
        out[56..64].copy_from_slice(&self.entsize.to_le_bytes());
        out
    }
}

pub(crate) fn section_name(class: &Class) -> &str {
    match class {
        Class::Text => ".text",
        Class::Patch => ".blaze.patch",
        Class::Stubs => ".blaze.stubs",
        Class::Plt => ".plt",
        Class::Veneers => ".blaze.veneers",
        Class::Rodata => ".rodata",
        Class::RelRo => ".data.rel.ro",
        Class::ThreadVars => ".blaze.thread_vars",
        Class::EhFrame => ".eh_frame",
        Class::Data => ".data",
        Class::Table => ".blaze.table",
        Class::Got => ".got",
        Class::InitArray => ".init_array",
        Class::FiniArray => ".fini_array",
        Class::PatchData => ".blaze.patch_data",
        Class::Named { name, .. } => name.as_str(),
        Class::Tdata => ".tdata",
        Class::Tbss => ".tbss",
        Class::Bss => ".bss",
        Class::Synthetic(name) => *name,
    }
}

/// The `Elf64_Verneed` entries of `.gnu.version_r`, its section's `sh_info`: the `vn_next` chain.
fn verneeds(bytes: &[u8]) -> u32 {
    let (mut count, mut at) = (0, 0usize);
    while let Some(next) = bytes.get(at + 12..at + 16) {
        count += 1;
        let next = u32::from_le_bytes(next.try_into().expect("4 bytes"));
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    count
}

pub(crate) fn file(writer: &Elf, placed: &Placed<'_>) -> Result<File, LinkError> {
    let plan = placed.plan;
    let regions = placed.regions;
    let entry = match &writer.output.kind {
        OutputKind::Executable { entry } => {
            Some(placed.entry.ok_or_else(|| error(format!("the executable's entry `{entry}` is not placed")))?)
        }
        OutputKind::Shared { .. } => placed.entry,
    };

    let mut loads: Vec<Load> = Vec::new();
    for region in regions {
        let segment = Segment::of(&region.class);
        let end = region.addr + region.capacity.max(region.size);
        let file = in_file(&region.class);
        match loads.last_mut() {
            Some(load) if load.segment == segment => {
                if file && load.zeroed {
                    return Err(error(format!(
                        "{:?} at {:#x} is in the file but follows zeroed memory in its segment",
                        region.class, region.addr
                    )));
                }
                if file {
                    load.file_end = end;
                }
                load.zeroed |= !file;
                load.end = end;
            }
            _ => {
                if loads.iter().any(|it| it.segment == segment) {
                    return Err(error(format!("{:?} at {:#x} splits the {segment:?} segment", region.class, region.addr)));
                }
                let vaddr = if loads.is_empty() { plan.base } else { region.addr };
                if loads.is_empty() && segment != Segment::Read {
                    return Err(error(format!("the image starts with {:?}, not the headers' segment", region.class)));
                }
                if vaddr % plan.page != 0 {
                    return Err(error(format!(
                        "the {segment:?} segment starts at {vaddr:#x}, not on a {:#x} page",
                        plan.page
                    )));
                }
                loads.push(Load { segment, vaddr, file_end: if file { end } else { vaddr }, end, zeroed: !file });
            }
        }
    }
    if loads.is_empty() {
        return Err(error("an image with no regions".into()));
    }

    // Each synthetic region's address and the bytes `synthesize` gave it.
    let synthetic = |name: &'static str| {
        placed.region(&Class::Synthetic(name)).map(|(index, it)| (it.addr, placed.bytes(index)))
    };
    let mut phdrs = vec![Phdr { p_type: elf::PT_PHDR, flags: 4, vaddr: plan.base + EHDR, filesz: 0, memsz: 0, align: 8 }];
    if let Some((addr, interp)) = synthetic(INTERP) {
        let size = interp.len() as u64;
        phdrs.push(Phdr { p_type: elf::PT_INTERP, flags: 4, vaddr: addr, filesz: size, memsz: size, align: 1 });
    }
    for load in &loads {
        phdrs.push(Phdr {
            p_type: elf::PT_LOAD,
            flags: load.segment.flags(),
            vaddr: load.vaddr,
            filesz: load.file_end - load.vaddr,
            memsz: load.end - load.vaddr,
            align: plan.page,
        });
    }
    if let Some((addr, dynamic)) = synthetic(DYNAMIC) {
        let size = dynamic.len() as u64;
        phdrs.push(Phdr { p_type: elf::PT_DYNAMIC, flags: 6, vaddr: addr, filesz: size, memsz: size, align: 8 });
    }
    if let Some(at) = loads.iter().position(|it| it.segment == Segment::Relro) {
        let load = &loads[at];
        // The loader protects whole pages: round up to the next segment's page so the last
        // one is protected too, but never past the mapping when nothing follows.
        let end = if at + 1 < loads.len() { load.end.next_multiple_of(plan.page) } else { load.end };
        let size = end - load.vaddr;
        phdrs.push(Phdr { p_type: elf::PT_GNU_RELRO, flags: 4, vaddr: load.vaddr, filesz: size, memsz: size, align: 1 });
    }
    if let (Some(_), Some((addr, hdr))) = (placed.region(&Class::EhFrame), synthetic(EH_FRAME_HDR)) {
        let size = hdr.len() as u64;
        phdrs.push(Phdr { p_type: elf::PT_GNU_EH_FRAME, flags: 4, vaddr: addr, filesz: size, memsz: size, align: 4 });
    }
    phdrs.push(Phdr { p_type: elf::PT_GNU_STACK, flags: 6, vaddr: 0, filesz: 0, memsz: 0, align: 0 });
    if let Some(tls) = placed.tls {
        phdrs.push(Phdr {
            p_type: elf::PT_TLS,
            flags: 4,
            vaddr: tls.addr,
            filesz: tls.init,
            memsz: tls.size,
            align: tls.align.max(1),
        });
    }
    if let Some((addr, note)) = synthetic(BUILD_ID) {
        let size = note.len() as u64;
        phdrs.push(Phdr { p_type: elf::PT_NOTE, flags: 4, vaddr: addr, filesz: size, memsz: size, align: 4 });
    }
    if phdrs.len() as u64 > PHDRS {
        return Err(error(format!("{} program headers, more than the {PHDRS} the headers have room for", phdrs.len())));
    }
    let phnum = phdrs.len() as u64;
    phdrs[0].filesz = phnum * PHDR;
    phdrs[0].memsz = phnum * PHDR;

    // Section headers: the null one, one per region in address order (`shndx` counts on it),
    // then those outside the address space.
    numbered(regions)?;
    let index_of = |class: &Class| regions.iter().position(|it| &it.class == class).map_or(0, |it| it as u32 + 1);
    let shdr_count = 1 + regions.len() + placed.debug.len() + 3;
    let mut names = Strings::with_capacity(
        regions.iter().map(|it| section_name(&it.class).len() + 1).sum::<usize>()
            + placed.debug.iter().map(|(name, _)| name.len() + 1).sum::<usize>()
            + ".symtab\0.strtab\0.shstrtab\0".len(),
        shdr_count,
    );
    let mut shdrs = Vec::with_capacity(shdr_count);
    shdrs.push(Shdr::default());
    for (index, region) in regions.iter().enumerate() {
        let class = &region.class;
        let segment = Segment::of(class);
        let mut flags = u64::from(elf::SHF_ALLOC);
        if segment.loader_writes() {
            flags |= u64::from(elf::SHF_WRITE);
        }
        if segment == Segment::Code {
            flags |= u64::from(elf::SHF_EXECINSTR);
        }
        if matches!(class, Class::Tdata | Class::Tbss) {
            flags |= u64::from(elf::SHF_TLS);
        }
        let mut shdr = Shdr {
            name: names.add(section_name(class)),
            sh_type: elf::SHT_PROGBITS,
            flags,
            addr: region.addr,
            offset: region.addr - plan.base,
            size: region.size,
            align: region.align.max(1),
            ..Shdr::default()
        };
        match class {
            Class::Tbss | Class::Bss => shdr.sh_type = elf::SHT_NOBITS,
            Class::InitArray => (shdr.sh_type, shdr.entsize) = (elf::SHT_INIT_ARRAY, 8),
            Class::FiniArray => (shdr.sh_type, shdr.entsize) = (elf::SHT_FINI_ARRAY, 8),
            Class::Named { name, .. } if name == PREINIT_ARRAY => {
                (shdr.sh_type, shdr.entsize) = (elf::SHT_PREINIT_ARRAY, 8)
            }
            Class::Got => shdr.entsize = 8,
            Class::EhFrame if writer.output.arch == blaze_codegen::Arch::X86_64 => {
                shdr.sh_type = elf::SHT_X86_64_UNWIND
            }
            Class::Synthetic(name) => {
                let bytes = placed.bytes(index);
                shdr.size = bytes.len() as u64;
                match *name {
                    BUILD_ID => shdr.sh_type = elf::SHT_NOTE,
                    DYNSYM => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_DYNSYM, 24);
                        (shdr.link, shdr.info) = (index_of(&Class::Synthetic(DYNSTR)), 1);
                    }
                    DYNSTR => shdr.sh_type = elf::SHT_STRTAB,
                    GNU_HASH => (shdr.sh_type, shdr.link) = (elf::SHT_GNU_HASH, index_of(&Class::Synthetic(DYNSYM))),
                    VERSYM => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_GNU_VERSYM, 2);
                        shdr.link = index_of(&Class::Synthetic(DYNSYM));
                    }
                    VERNEED => {
                        shdr.sh_type = elf::SHT_GNU_VERNEED;
                        (shdr.link, shdr.info) = (index_of(&Class::Synthetic(DYNSTR)), verneeds(bytes));
                    }
                    RELA_DYN => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_RELA, 24);
                        shdr.link = index_of(&Class::Synthetic(DYNSYM));
                    }
                    DYNAMIC => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_DYNAMIC, 16);
                        shdr.link = index_of(&Class::Synthetic(DYNSTR));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        shdrs.push(shdr);
    }

    // `.symtab`: the functions by name (locals, for backtraces and debuggers), then the
    // exports and the imports. Each name has its own place in `.strtab`, unshared as mold
    // writes it, so both tables are laid out by prefix sums over chunks of functions and
    // filled in parallel, straight into the file's tail.
    let chunks: Vec<(usize, usize)> = placed
        .functions
        .par_chunks(CHUNK)
        .map(|chunk| {
            chunk.iter().filter_map(|it| it.name.as_deref()).fold((0, 0), |(count, len), name| (count + 1, len + stored(name)))
        })
        .collect();
    let named: usize = chunks.iter().map(|it| it.0).sum();
    let function_names: usize = chunks.iter().map(|it| it.1).sum();
    let global_names = placed.exports.iter().map(|it| stored(&it.name)).sum::<usize>()
        + placed.imports.iter().map(|it| stored(&it.name)).sum::<usize>();
    let symbols = 1 + named + placed.exports.len() + placed.imports.len();
    let strtab_len = 1 + function_names + global_names;
    if u32::try_from(strtab_len).is_err() {
        return Err(error(format!("{strtab_len} bytes of symbol names, more than .strtab offsets reach")));
    }
    let first_global = 1 + named as u32;

    let tail = regions
        .iter()
        .filter(|it| in_file(&it.class))
        .map(|it| it.addr - plan.base + it.capacity.max(it.size))
        .fold(plan.headers, u64::max)
        .next_multiple_of(8);
    let mut len = 0usize;
    for (name, data) in placed.debug {
        shdrs.push(Shdr {
            name: names.add(name),
            sh_type: elf::SHT_PROGBITS,
            offset: tail + len as u64,
            size: data.len() as u64,
            align: 1,
            ..Shdr::default()
        });
        len += data.len();
    }
    let symtab_at = len.next_multiple_of(8);
    let strtab_at = symtab_at + symbols * SYM;
    let symtab_index = shdrs.len() as u32;
    shdrs.push(Shdr {
        name: names.add(".symtab"),
        sh_type: elf::SHT_SYMTAB,
        offset: tail + symtab_at as u64,
        size: (symbols * SYM) as u64,
        link: symtab_index + 1,
        info: first_global,
        align: 8,
        entsize: SYM as u64,
        ..Shdr::default()
    });
    shdrs.push(Shdr {
        name: names.add(".strtab"),
        sh_type: elf::SHT_STRTAB,
        offset: tail + strtab_at as u64,
        size: strtab_len as u64,
        align: 1,
        ..Shdr::default()
    });
    let shstrndx = shdrs.len();
    let shstrtab_name = names.add(".shstrtab");
    let shstrtab_at = strtab_at + strtab_len;
    shdrs.push(Shdr {
        name: shstrtab_name,
        sh_type: elf::SHT_STRTAB,
        offset: tail + shstrtab_at as u64,
        size: names.bytes.len() as u64,
        align: 1,
        ..Shdr::default()
    });
    if shdrs.len() >= usize::from(elf::SHN_LORESERVE) {
        return Err(error(format!("{} sections, more than ELF numbers", shdrs.len())));
    }
    let shoff_at = (shstrtab_at + names.bytes.len()).next_multiple_of(8);
    let shoff = tail + shoff_at as u64;

    let mut bytes = vec![0u8; shoff_at + shdrs.len() * SHDR];
    let mut at = 0;
    for (_, data) in placed.debug {
        bytes[at..at + data.len()].copy_from_slice(data);
        at += data.len();
    }
    {
        let (head, rest) = bytes.split_at_mut(strtab_at);
        let symtab = &mut head[symtab_at..];
        let strtab = &mut rest[..strtab_len];
        let (functions_syms, global_syms) = symtab.split_at_mut(SYM * (1 + named));
        let (function_strs, global_strs) = strtab.split_at_mut(1 + function_names);
        let (mut syms, mut strs) = (&mut functions_syms[SYM..], &mut function_strs[1..]);
        let mut jobs = Vec::with_capacity(chunks.len());
        let mut name_at = 1u32;
        for (chunk, &(count, len)) in placed.functions.chunks(CHUNK).zip(&chunks) {
            let (these_syms, rest) = std::mem::take(&mut syms).split_at_mut(SYM * count);
            syms = rest;
            let (these_strs, rest) = std::mem::take(&mut strs).split_at_mut(len);
            strs = rest;
            jobs.push((chunk, these_syms, these_strs, name_at));
            name_at += len as u32;
        }
        jobs.into_par_iter().for_each(|(chunk, syms, strs, name_at)| {
            let mut strings = Names { bytes: strs, at: 0, offset: name_at };
            let functions = chunk.iter().filter_map(|it| it.name.as_deref().map(|name| (it, name)));
            for ((function, name), out) in functions.zip(syms.chunks_exact_mut(SYM)) {
                let symbol = Sym {
                    name: strings.add(name),
                    info: info(elf::STB_LOCAL, SymbolKind::Function),
                    shndx: shndx(regions, function.addr),
                    value: function.addr,
                    size: function.size,
                };
                out.copy_from_slice(&symbol.bytes());
            }
        });

        let mut strings = Names { bytes: global_strs, at: 0, offset: name_at };
        let mut out = global_syms.chunks_exact_mut(SYM);
        for export in placed.exports {
            let symbol = Sym {
                name: strings.add(&export.name),
                info: info(elf::STB_GLOBAL, export.kind),
                shndx: shndx(regions, export.addr),
                value: value(placed, &export.name, export.addr, export.kind)?,
                size: export.size,
            };
            out.next().expect("a symbol counted").copy_from_slice(&symbol.bytes());
        }
        for import in placed.imports {
            let bind = if import.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
            let symbol = Sym { name: strings.add(&import.name), info: info(bind, import.kind), ..Sym::default() };
            out.next().expect("a symbol counted").copy_from_slice(&symbol.bytes());
        }
    }
    bytes[shstrtab_at..shstrtab_at + names.bytes.len()].copy_from_slice(&names.bytes);
    for (shdr, out) in shdrs.iter().zip(bytes[shoff_at..].chunks_exact_mut(SHDR)) {
        out.copy_from_slice(&shdr.bytes());
    }

    let mut head = Vec::with_capacity((EHDR + PHDR * phnum) as usize);
    head.extend_from_slice(&elf::ELFMAG);
    head.u8(elf::ELFCLASS64);
    head.u8(elf::ELFDATA2LSB);
    head.u8(elf::EV_CURRENT);
    head.u8(elf::ELFOSABI_NONE);
    head.resize(16, 0);
    head.u16(elf::ET_DYN);
    head.u16(writer.machine());
    head.u32(u32::from(elf::EV_CURRENT));
    head.u64(entry.unwrap_or(0));
    head.u64(EHDR);
    head.u64(shoff);
    head.u32(0);
    head.u16(EHDR as u16);
    head.u16(PHDR as u16);
    head.u16(phnum as u16);
    head.u16(64);
    head.u16(shdrs.len() as u16);
    head.u16(shstrndx as u16);
    for phdr in &phdrs {
        head.u32(phdr.p_type);
        head.u32(phdr.flags);
        head.u64(phdr.vaddr.saturating_sub(plan.base));
        head.u64(phdr.vaddr);
        head.u64(phdr.vaddr);
        head.u64(phdr.filesz);
        head.u64(phdr.memsz);
        head.u64(phdr.align);
    }
    debug_assert!(head.len() as u64 <= plan.headers);

    Ok(File { len: tail + bytes.len() as u64, parts: vec![(0, head), (tail, bytes)] })
}
