//! The file around the placed regions: the ELF header and program headers at its start, and
//! after the last region the symbol table, the debug sections and the section headers.

use std::borrow::Cow;

use object::elf;

use super::dynamic::{Put, Strings, Sym, Tables, info, section_of, value};
use super::{
    BUILD_ID, DYNAMIC, DYNSTR, DYNSYM, EH_FRAME_HDR, EHDR, Elf, GNU_HASH, INTERP, PHDR, PHDRS, PREINIT_ARRAY,
    RELA_DYN, Segment, VERNEED, VERSYM, error, in_file,
};
use crate::model::{Class, File, Placed, SymbolKind};
use crate::{LinkError, OutputKind};

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

pub(crate) fn section_name(class: &Class) -> Cow<'_, str> {
    Cow::Borrowed(match class {
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
        Class::Named { name, .. } => return Cow::Borrowed(name.as_str()),
        Class::Tdata => ".tdata",
        Class::Tbss => ".tbss",
        Class::Bss => ".bss",
        Class::Synthetic(name) => *name,
    })
}

pub(crate) fn file(writer: &Elf, placed: &Placed<'_>) -> Result<File, LinkError> {
    let tables = Tables::of(writer, placed)?;
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

    let synthetic = |name: &'static str| placed.region(&Class::Synthetic(name)).map(|(_, it)| it.addr);
    let mut phdrs = vec![Phdr { p_type: elf::PT_PHDR, flags: 4, vaddr: plan.base + EHDR, filesz: 0, memsz: 0, align: 8 }];
    if let (Some(interp), Some(addr)) = (&tables.interp, synthetic(INTERP)) {
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
    if let Some(addr) = synthetic(DYNAMIC) {
        let size = tables.dynamic.len() as u64;
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
    if let (true, Some(addr)) = (tables.has_eh_frame, synthetic(EH_FRAME_HDR)) {
        let size = tables.eh_frame_hdr.len() as u64;
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
    if let Some(addr) = synthetic(BUILD_ID) {
        let size = tables.note.len() as u64;
        phdrs.push(Phdr { p_type: elf::PT_NOTE, flags: 4, vaddr: addr, filesz: size, memsz: size, align: 4 });
    }
    if phdrs.len() as u64 > PHDRS {
        return Err(error(format!("{} program headers, more than the {PHDRS} the headers have room for", phdrs.len())));
    }
    let phnum = phdrs.len() as u64;
    phdrs[0].filesz = phnum * PHDR;
    phdrs[0].memsz = phnum * PHDR;

    // Section headers: the null one, one per region in address order (`section_of` counts on
    // it), then those outside the address space.
    let index_of = |class: &Class| regions.iter().position(|it| &it.class == class).map_or(0, |it| it as u32 + 1);
    let mut names = Strings::new();
    let mut shdrs = vec![Shdr::default()];
    for region in regions {
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
            name: names.add(&section_name(class)),
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
                shdr.size = tables.get(name).map_or(0, |it| it.len() as u64);
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
                        (shdr.link, shdr.info) = (index_of(&Class::Synthetic(DYNSTR)), tables.verneeds);
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
    // exports and the imports.
    let mut strtab = Strings::new();
    let mut symtab = vec![Sym::default()];
    for function in placed.functions {
        let Some(name) = &function.name else { continue };
        symtab.push(Sym {
            name: strtab.add(name),
            info: info(elf::STB_LOCAL, SymbolKind::Function),
            shndx: section_of(regions, function.addr)?,
            value: function.addr,
            size: function.size,
        });
    }
    let first_global = symtab.len() as u32;
    for export in placed.exports {
        symtab.push(Sym {
            name: strtab.add(&export.name),
            info: info(elf::STB_GLOBAL, export.kind),
            shndx: section_of(regions, export.addr)?,
            value: value(placed, &export.name, export.addr, export.kind)?,
            size: export.size,
        });
    }
    for import in placed.imports {
        let bind = if import.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
        symtab.push(Sym { name: strtab.add(&import.name), info: info(bind, import.kind), ..Sym::default() });
    }

    let tail = regions
        .iter()
        .filter(|it| in_file(&it.class))
        .map(|it| it.addr - plan.base + it.capacity.max(it.size))
        .fold(plan.headers, u64::max)
        .next_multiple_of(8);
    let mut bytes = Vec::new();
    let put = |bytes: &mut Vec<u8>, align: usize, data: &[u8]| {
        bytes.align(align);
        let offset = tail + bytes.len() as u64;
        bytes.extend_from_slice(data);
        offset
    };
    for (name, data) in placed.debug {
        let offset = put(&mut bytes, 1, data);
        shdrs.push(Shdr {
            name: names.add(name),
            sh_type: elf::SHT_PROGBITS,
            offset,
            size: data.len() as u64,
            align: 1,
            ..Shdr::default()
        });
    }
    let mut symbols = Vec::with_capacity(symtab.len() * 24);
    for symbol in &symtab {
        symbol.put(&mut symbols);
    }
    let symtab_index = shdrs.len() as u32;
    let offset = put(&mut bytes, 8, &symbols);
    shdrs.push(Shdr {
        name: names.add(".symtab"),
        sh_type: elf::SHT_SYMTAB,
        offset,
        size: symbols.len() as u64,
        link: symtab_index + 1,
        info: first_global,
        align: 8,
        entsize: 24,
        ..Shdr::default()
    });
    let offset = put(&mut bytes, 1, &strtab.bytes);
    shdrs.push(Shdr {
        name: names.add(".strtab"),
        sh_type: elf::SHT_STRTAB,
        offset,
        size: strtab.bytes.len() as u64,
        align: 1,
        ..Shdr::default()
    });
    let shstrndx = shdrs.len();
    let shstrtab_name = names.add(".shstrtab");
    let offset = put(&mut bytes, 1, &names.bytes);
    shdrs.push(Shdr {
        name: shstrtab_name,
        sh_type: elf::SHT_STRTAB,
        offset,
        size: names.bytes.len() as u64,
        align: 1,
        ..Shdr::default()
    });
    if shdrs.len() >= usize::from(elf::SHN_LORESERVE) {
        return Err(error(format!("{} sections, more than ELF numbers", shdrs.len())));
    }
    bytes.align(8);
    let shoff = tail + bytes.len() as u64;
    for shdr in &shdrs {
        bytes.u32(shdr.name);
        bytes.u32(shdr.sh_type);
        bytes.u64(shdr.flags);
        bytes.u64(shdr.addr);
        bytes.u64(shdr.offset);
        bytes.u64(shdr.size);
        bytes.u32(shdr.link);
        bytes.u32(shdr.info);
        bytes.u64(shdr.align);
        bytes.u64(shdr.entsize);
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
