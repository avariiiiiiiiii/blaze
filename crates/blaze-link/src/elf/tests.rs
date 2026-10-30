use object::read::elf::{Dyn as _, ElfFile64, FileHeader as _, ProgramHeader as _, Rela as _, SectionHeader as _, Sym as _};
use object::{Endianness, SymbolIndex, elf};

use super::dynamic::{elf_hash, gnu_hash};
use super::image::section_name;
use super::{Elf, Segment, in_file, section, writer};
use crate::model::{
    Bind, Class, Counts, Export, Function, GotEntry, Holds, Import, Library, Placed, Plan, Region, Special, SymbolKind,
    Tls, Writer,
};
use crate::output::Debug;
use crate::{Format, Output, OutputKind};

fn output(arch: blaze_codegen::Arch, kind: OutputKind, interpreter: Option<&str>) -> Output {
    Output {
        format: Format::Elf,
        arch,
        kind,
        interpreter: interpreter.map(str::to_owned),
        platform: None,
        incremental: false,
        debug: Debug::None,
        live: None,
        tags: 0,
        stack: None,
    }
}

fn executable() -> Output {
    output(
        blaze_codegen::Arch::X86_64,
        OutputKind::Executable { entry: "_start".into() },
        Some("/lib64/ld-linux-x86-64.so.2"),
    )
}

/// An image placed as the core places one: classes in `Writer::order`, a page boundary at each
/// change of protection, each region aligned.
struct Fixture {
    writer: Elf,
    plan: Plan,
    regions: Vec<Region>,
    contents: Vec<Vec<u8>>,
    libraries: Vec<Library>,
    imports: Vec<Import>,
    got: Vec<GotEntry>,
    rebase: Vec<u64>,
    binds: Vec<Bind>,
    exports: Vec<Export>,
    functions: Vec<Function>,
    entry: Option<u64>,
    tls: Option<Tls>,
    debug: Vec<(&'static str, Vec<u8>)>,
}

impl Fixture {
    fn new(output: Output, pieces: &[(Class, u64, u64)], counts: &Counts) -> Fixture {
        let writer = Elf { output };
        let plan = writer.plan();
        let mut wanted = pieces.to_vec();
        for (class, size) in writer.synthetic(counts) {
            wanted.push((class, size, 8));
        }
        let mut classes: Vec<Class> = wanted.iter().map(|it| it.0.clone()).collect();
        writer.order(&mut classes);
        let mut regions = Vec::new();
        let mut addr = plan.base + plan.headers;
        let mut last = None;
        for class in classes {
            let (_, size, align) = wanted.iter().find(|it| it.0 == class).cloned().unwrap();
            let prot = writer.prot(&class);
            if last.is_some_and(|it| it != prot) {
                addr = addr.next_multiple_of(plan.page);
            }
            last = Some(prot);
            addr = addr.next_multiple_of(align);
            regions.push(Region { class, addr, capacity: size, size, prot, align });
            addr += size;
        }
        let contents =
            regions.iter().map(|it| if in_file(&it.class) { vec![0; it.size as usize] } else { Vec::new() }).collect();
        Fixture {
            writer,
            plan,
            regions,
            contents,
            libraries: Vec::new(),
            imports: Vec::new(),
            got: Vec::new(),
            rebase: Vec::new(),
            binds: Vec::new(),
            exports: Vec::new(),
            functions: Vec::new(),
            entry: None,
            tls: None,
            debug: Vec::new(),
        }
    }

    fn index(&self, class: &Class) -> usize {
        self.regions.iter().position(|it| &it.class == class).unwrap_or_else(|| panic!("no {class:?}"))
    }

    fn at(&self, class: &Class) -> u64 {
        self.regions[self.index(class)].addr
    }

    fn put(&mut self, class: &Class, offset: usize, bytes: &[u8]) {
        let index = self.index(class);
        self.contents[index][offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    fn with<R>(&self, then: impl FnOnce(&Placed<'_>) -> R) -> R {
        let contents: Vec<&[u8]> = self.contents.iter().map(Vec::as_slice).collect();
        then(&Placed {
            output: &self.writer.output,
            name: "image",
            plan: &self.plan,
            regions: &self.regions,
            contents: &contents,
            libraries: &self.libraries,
            imports: &self.imports,
            got: &self.got,
            rebase: &self.rebase,
            binds: &self.binds,
            exports: &self.exports,
            functions: &self.functions,
            entry: self.entry,
            tls: self.tls,
            id: *b"blaze build id!!",
            debug: &self.debug,
            init: None,
            fini: None,
        })
    }

    fn synthesize(&self) -> Result<Vec<(Class, Vec<u8>)>, crate::LinkError> {
        self.with(|placed| self.writer.synthesize(placed))
    }

    /// Synthesizes, then writes the file as the core does: regions at their offsets, then the
    /// writer's parts.
    fn link(&mut self) -> Vec<u8> {
        for (class, bytes) in self.synthesize().unwrap() {
            let index = self.index(&class);
            assert!(bytes.len() as u64 <= self.regions[index].capacity, "{class:?} outgrew the size it asked for");
            self.contents[index] = bytes;
        }
        for (class, bytes) in self.synthesize().unwrap() {
            assert_eq!(self.contents[self.index(&class)], bytes, "{class:?} is not a function of the image");
        }
        let (file, offsets) = self.with(|placed| {
            let offsets: Vec<Option<u64>> = self.regions.iter().map(|it| self.writer.offset(placed, it)).collect();
            (self.writer.file(placed).unwrap(), offsets)
        });
        assert_eq!(self.with(|placed| self.writer.file(placed).unwrap().parts), file.parts);
        let mut bytes = vec![0; file.len as usize];
        for ((region, contents), offset) in self.regions.iter().zip(&self.contents).zip(offsets) {
            if let Some(offset) = offset {
                assert_eq!(offset, region.addr, "a region's file offset is its address");
                bytes[offset as usize..][..contents.len()].copy_from_slice(contents);
            }
        }
        for (offset, part) in &file.parts {
            bytes[*offset as usize..][..part.len()].copy_from_slice(part);
        }
        bytes
    }
}

/// The workspace's `object` reads ELF only from 8-aligned bytes.
fn aligned(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks(8)
        .map(|it| {
            let mut word = [0; 8];
            word[..it.len()].copy_from_slice(it);
            u64::from_le_bytes(word)
        })
        .collect()
}

fn view(words: &[u64], len: usize) -> &[u8] {
    // SAFETY: `words` holds at least `len` initialized bytes, and u8 has no alignment.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), len) }
}

fn c_string(data: &[u8], at: u64) -> &str {
    let rest = &data[at as usize..];
    std::str::from_utf8(&rest[..rest.iter().position(|&it| it == 0).unwrap()]).unwrap()
}

fn eh_frame(fdes: &[(u64, u32)]) -> Vec<u8> {
    use gimli::write::{Address, CommonInformationEntry, EhFrame, EndianVec, FrameDescriptionEntry, FrameTable};
    let mut table = FrameTable::default();
    let encoding = gimli::Encoding { address_size: 8, format: gimli::Format::Dwarf32, version: 1 };
    let cie = table.add_cie(CommonInformationEntry::new(encoding, 1, -8, gimli::Register(16)));
    for &(addr, len) in fdes {
        table.add_fde(cie, FrameDescriptionEntry::new(Address::Constant(addr), len));
    }
    let mut section = EhFrame(EndianVec::new(gimli::LittleEndian));
    table.write_eh_frame(&mut section).unwrap();
    section.0.into_vec()
}

fn import(name: &str, library: Option<u32>, version: Option<&str>, weak: bool) -> Import {
    Import {
        name: name.into(),
        library,
        version: version.map(str::to_owned),
        kind: SymbolKind::Function,
        weak,
    }
}

#[test]
fn an_executable_reads_back() {
    let fdes = eh_frame(&[(0, 32), (0, 32)]).len() as u64;
    let counts = Counts {
        imports: 4,
        import_names: 72,
        libraries: 3,
        library_names: 32,
        got: 3,
        absolute: 9,
        exports: 2,
        export_names: 16,
        functions: 2,
        function_names: 10,
        fdes: 2,
        ..Counts::default()
    };
    let pieces = [
        (Class::Text, 64, 16),
        (Class::Rodata, 16, 8),
        (Class::EhFrame, fdes, 8),
        (Class::Data, 16, 8),
        (Class::RelRo, 8, 8),
        (Class::Got, 32, 8),
        (Class::InitArray, 8, 8),
        (Class::Tdata, 8, 8),
        (Class::Tbss, 16, 8),
        (Class::Bss, 32, 8),
    ];
    let mut image = Fixture::new(executable(), &pieces, &counts);
    let text = image.at(&Class::Text);
    let rodata = image.at(&Class::Rodata);
    let data = image.at(&Class::Data);
    let got = image.at(&Class::Got);
    let init = image.at(&Class::InitArray);
    let tdata = image.at(&Class::Tdata);
    let bss = image.at(&Class::Bss);
    image.put(&Class::Text, 0, &[0xcc; 64]);
    image.put(&Class::EhFrame, 0, &eh_frame(&[(text + 32, 32), (text, 32)]));
    image.put(&Class::Data, 0, &rodata.to_le_bytes());
    image.put(&Class::Got, 8, &rodata.to_le_bytes());
    image.put(&Class::InitArray, 0, &text.to_le_bytes());
    let relro = image.at(&Class::RelRo);
    image.put(&Class::RelRo, 0, &(text + 32).to_le_bytes());
    image.put(&Class::Tdata, 0, &7u64.to_le_bytes());
    image.libraries = vec![
        Library { name: "libc.so.6".into(), as_needed: true },
        Library { name: "libm.so.6".into(), as_needed: true },
        Library { name: "libunused.so".into(), as_needed: true },
    ];
    image.imports = vec![
        import("malloc", Some(0), Some("GLIBC_2.2.5"), false),
        import("realpath", Some(0), Some("GLIBC_2.3"), false),
        import("__cxa_thread_atexit_impl", None, None, true),
        import("cos", Some(1), Some("GLIBC_2.2.5"), false),
    ];
    image.got = vec![
        GotEntry { addr: got, holds: Holds::Import(0) },
        GotEntry { addr: got + 8, holds: Holds::Local(rodata) },
        GotEntry { addr: got + 16, holds: Holds::TlsGd { import: None, offset: 4 } },
    ];
    image.rebase = vec![data, got + 8, init, relro];
    image.binds = vec![Bind { addr: data + 8, import: 0, addend: 8 }];
    image.exports = vec![
        Export { name: "main".into(), addr: text, size: 32, kind: SymbolKind::Function },
        Export { name: "__blaze_live".into(), addr: rodata, size: 16, kind: SymbolKind::Data },
    ];
    image.functions = vec![
        Function { addr: text, size: 32, fde: None, windows: None, name: Some("main".into()) },
        Function { addr: text + 32, size: 32, fde: None, windows: None, name: Some("_start".into()) },
    ];
    image.entry = Some(text + 32);
    image.tls = Some(Tls { addr: tdata, init: 8, size: 24, align: 8 });
    image.debug = vec![(".debug_line", vec![1, 2, 3])];

    let bytes = image.link();
    let words = aligned(&bytes);
    let data_bytes = view(&words, bytes.len());
    let file = ElfFile64::<Endianness>::parse(data_bytes).unwrap();
    let endian = file.endian();
    let header = file.elf_header();
    assert_eq!(header.e_type(endian), elf::ET_DYN);
    assert_eq!(header.e_machine(endian), elf::EM_X86_64);
    assert_eq!(header.e_entry(endian), text + 32);

    // Segments: four loads, R, RX, relro, RW, each on a page, covering every region.
    let phdrs = file.elf_program_headers();
    let loads: Vec<_> = phdrs.iter().filter(|it| it.p_type(endian) == elf::PT_LOAD).collect();
    assert_eq!(loads.iter().map(|it| it.p_flags(endian)).collect::<Vec<_>>(), [4, 5, 6, 6]);
    for load in &loads {
        assert_eq!(load.p_offset(endian), load.p_vaddr(endian));
        assert_eq!(load.p_vaddr(endian) % 0x1000, 0);
        assert_eq!(load.p_align(endian), 0x1000);
    }
    assert_eq!(loads[0].p_vaddr(endian), 0);
    for region in &image.regions {
        let end = region.addr + region.size;
        let load = loads
            .iter()
            .find(|it| it.p_vaddr(endian) <= region.addr && end <= it.p_vaddr(endian) + it.p_memsz(endian))
            .unwrap_or_else(|| panic!("{:?} in no PT_LOAD", region.class));
        if in_file(&region.class) {
            assert!(end <= load.p_vaddr(endian) + load.p_filesz(endian), "{:?} past the file", region.class);
        }
        let flags = load.p_flags(endian);
        assert_eq!(flags, Segment::of(&region.class).flags(), "{:?}", region.class);
    }
    let phdr = |p_type| phdrs.iter().find(|it| it.p_type(endian) == p_type).unwrap();
    let interp = phdr(elf::PT_INTERP);
    assert_eq!(interp.data(endian, data_bytes).unwrap(), b"/lib64/ld-linux-x86-64.so.2\0");
    let relro = phdr(elf::PT_GNU_RELRO);
    for class in [Class::RelRo, Class::Got, Class::InitArray, Class::Synthetic(super::DYNAMIC)] {
        let addr = image.at(&class);
        assert!(relro.p_vaddr(endian) <= addr && addr < relro.p_vaddr(endian) + relro.p_memsz(endian));
    }
    assert!(relro.p_vaddr(endian) + relro.p_memsz(endian) <= data, "relro ends before the writable data");
    let tls = phdr(elf::PT_TLS);
    assert_eq!((tls.p_vaddr(endian), tls.p_filesz(endian), tls.p_memsz(endian)), (tdata, 8, 24));
    assert!(tdata < bss);
    let note = phdr(elf::PT_NOTE).data(endian, data_bytes).unwrap();
    assert_eq!(&note[12..16], b"GNU\0");
    assert_eq!(&note[16..], b"blaze build id!!");
    let hdr = image.at(&Class::Synthetic(super::EH_FRAME_HDR));
    assert_eq!(phdr(elf::PT_GNU_EH_FRAME).p_vaddr(endian), hdr);
    assert!(phdrs.iter().any(|it| it.p_type(endian) == elf::PT_GNU_STACK && it.p_flags(endian) == 6));

    // `.eh_frame_hdr`: both FDEs, sorted by the address they cover.
    let table = &bytes[hdr as usize..];
    assert_eq!(table[..4], [1, 0x1b, 0x03, 0x3b]);
    let word = |at: usize| i32::from_le_bytes(table[at..at + 4].try_into().unwrap()) as i64;
    assert_eq!(word(4) + hdr as i64 + 4, image.at(&Class::EhFrame) as i64);
    assert_eq!(word(8), 2);
    assert_eq!(word(12) + hdr as i64, text as i64);
    assert_eq!(word(20) + hdr as i64, text as i64 + 32);

    // `.dynamic`.
    let dynamic = phdr(elf::PT_DYNAMIC).dynamic(endian, data_bytes).unwrap().unwrap();
    let entries: Vec<(u64, u64)> = dynamic.iter().map(|it| (it.d_tag(endian), it.d_val(endian))).collect();
    let tag = |tag: u32| entries.iter().find(|it| it.0 == u64::from(tag)).map(|it| it.1);
    let strtab = tag(elf::DT_STRTAB).unwrap();
    let needed: Vec<&str> = entries
        .iter()
        .filter(|it| it.0 == u64::from(elf::DT_NEEDED))
        .map(|it| c_string(data_bytes, strtab + it.1))
        .collect();
    assert_eq!(needed, ["libc.so.6", "libm.so.6"], "an as-needed library nothing imports from is left out");
    assert_eq!(tag(elf::DT_FLAGS), Some(u64::from(elf::DF_BIND_NOW)));
    assert_eq!(tag(elf::DT_FLAGS_1), Some(u64::from(elf::DF_1_NOW | elf::DF_1_PIE)));
    assert_eq!(tag(elf::DT_DEBUG), Some(0));
    assert_eq!(tag(elf::DT_SONAME), None);
    assert_eq!(tag(elf::DT_INIT_ARRAY), Some(init));
    assert_eq!(tag(elf::DT_INIT_ARRAYSZ), Some(8));
    assert_eq!(tag(elf::DT_RELACOUNT), Some(4));
    assert_eq!(tag(elf::DT_VERNEEDNUM), Some(2));
    assert_eq!(tag(elf::DT_SYMTAB), Some(image.at(&Class::Synthetic(super::DYNSYM))));
    assert_eq!(tag(elf::DT_GNU_HASH), Some(image.at(&Class::Synthetic(super::GNU_HASH))));
    assert_eq!(entries.last(), Some(&(0, 0)));

    // `.rela.dyn`: the slid addresses first and sorted, then the binds by address.
    let sections = file.elf_section_table();
    let (_, rela) = sections.section_by_name(endian, b".rela.dyn").unwrap();
    let (relas, _) = rela.rela(endian, data_bytes).unwrap().unwrap();
    let relas: Vec<(u64, u32, u32, i64)> = relas
        .iter()
        .map(|it| (it.r_offset(endian), it.r_type(endian, false), it.r_sym(endian, false), it.r_addend(endian)))
        .collect();
    let mut relative = vec![
        (data, elf::R_X86_64_RELATIVE, 0, rodata as i64),
        (got + 8, elf::R_X86_64_RELATIVE, 0, rodata as i64),
        (init, elf::R_X86_64_RELATIVE, 0, text as i64),
        (relro, elf::R_X86_64_RELATIVE, 0, text as i64 + 32),
    ];
    relative.sort();
    let mut other = vec![
        (got, elf::R_X86_64_GLOB_DAT, 1, 0),
        (got + 16, elf::R_X86_64_DTPMOD64, 0, 0),
        (got + 24, elf::R_X86_64_DTPOFF64, 0, 4),
        (data + 8, elf::R_X86_64_64, 1, 8),
    ];
    other.sort();
    relative.extend(other);
    assert_eq!(relas, relative);

    // `.dynsym`, its versions and its hash table.
    let dynsyms = sections.symbols(endian, data_bytes, elf::SHT_DYNSYM).unwrap();
    let versions = sections.versions(endian, data_bytes).unwrap().unwrap();
    let version = |index: usize| {
        versions.version(versions.version_index(endian, SymbolIndex(index))).unwrap().map(|it| it.name().to_vec())
    };
    let name = |index: usize| dynsyms.symbol(SymbolIndex(index)).unwrap().name(endian, dynsyms.strings()).unwrap();
    assert_eq!(name(1), b"malloc");
    assert_eq!(version(1).as_deref(), Some(&b"GLIBC_2.2.5"[..]));
    assert_eq!(name(2), b"realpath");
    assert_eq!(version(2).as_deref(), Some(&b"GLIBC_2.3"[..]));
    assert_eq!(name(3), b"__cxa_thread_atexit_impl");
    assert_eq!(dynsyms.symbol(SymbolIndex(3)).unwrap().st_bind(), elf::STB_WEAK);
    assert_eq!(version(3), None);
    assert_eq!(version(4).as_deref(), Some(&b"GLIBC_2.2.5"[..]));
    let (hash, _) = sections.gnu_hash(endian, data_bytes).unwrap().unwrap();
    for export in &image.exports {
        let name = export.name.as_bytes();
        let (_, symbol) = hash
            .find(endian, name, elf::gnu_hash(name), None, &dynsyms, &versions)
            .unwrap_or_else(|| panic!("`{}` is not in the hash table", export.name));
        assert_eq!(symbol.st_value(endian), export.addr);
        assert!(!symbol.is_undefined(endian));
    }
    assert!(hash.find(endian, b"malloc", elf::gnu_hash(b"malloc"), None, &dynsyms, &versions).is_none());

    // `.symtab` names the functions; every region has its section header.
    let symtab = sections.symbols(endian, data_bytes, elf::SHT_SYMTAB).unwrap();
    let named: Vec<(Vec<u8>, u64)> = symtab
        .symbols()
        .iter()
        .filter(|it| it.st_type() == elf::STT_FUNC)
        .map(|it| (it.name(endian, symtab.strings()).unwrap().to_vec(), it.st_value(endian)))
        .collect();
    assert!(named.contains(&(b"main".to_vec(), text)));
    assert!(named.contains(&(b"_start".to_vec(), text + 32)));
    for region in &image.regions {
        let (_, shdr) = sections
            .section_by_name(endian, section_name(&region.class).as_bytes())
            .unwrap_or_else(|| panic!("no section for {:?}", region.class));
        assert_eq!(shdr.sh_addr(endian), region.addr);
    }
    let (_, line) = sections.section_by_name(endian, b".debug_line").unwrap();
    let at = line.sh_offset(endian) as usize;
    assert_eq!(bytes[at..at + 3], [1, 2, 3]);
}

#[test]
fn a_shared_library_exports_through_its_hash_table() {
    let names: Vec<String> = (0..100).map(|it| format!("blaze_export_{it}")).collect();
    let counts = Counts {
        exports: names.len() as u64,
        export_names: names.iter().map(|it| it.len() as u64).sum(),
        ..Counts::default()
    };
    let kind = OutputKind::Shared { name: "libblaze.so".into(), exports: names.clone() };
    let output = output(blaze_codegen::Arch::Aarch64, kind, None);
    let mut image = Fixture::new(output, &[(Class::Text, 400, 4), (Class::Data, 8, 8)], &counts);
    let text = image.at(&Class::Text);
    image.exports = names
        .iter()
        .enumerate()
        .map(|(at, name)| Export { name: name.clone(), addr: text + 4 * at as u64, size: 4, kind: SymbolKind::Function })
        .collect();

    let bytes = image.link();
    let words = aligned(&bytes);
    let data = view(&words, bytes.len());
    let file = ElfFile64::<Endianness>::parse(data).unwrap();
    let endian = file.endian();
    assert_eq!(file.elf_header().e_machine(endian), elf::EM_AARCH64);
    let phdrs = file.elf_program_headers();
    assert!(phdrs.iter().all(|it| it.p_type(endian) != elf::PT_INTERP));
    for load in phdrs.iter().filter(|it| it.p_type(endian) == elf::PT_LOAD) {
        assert_eq!(load.p_vaddr(endian) % 0x10000, 0);
        assert_eq!(load.p_align(endian), 0x10000);
    }
    let dynamic = phdrs.iter().find_map(|it| it.dynamic(endian, data).unwrap()).unwrap();
    let entries: Vec<(u64, u64)> = dynamic.iter().map(|it| (it.d_tag(endian), it.d_val(endian))).collect();
    let tag = |tag: u32| entries.iter().find(|it| it.0 == u64::from(tag)).map(|it| it.1);
    assert_eq!(c_string(data, tag(elf::DT_STRTAB).unwrap() + tag(elf::DT_SONAME).unwrap()), "libblaze.so");
    assert_eq!(tag(elf::DT_FLAGS), Some(u64::from(elf::DF_BIND_NOW | elf::DF_SYMBOLIC)));
    assert_eq!(tag(elf::DT_FLAGS_1), Some(u64::from(elf::DF_1_NOW)));
    assert_eq!(tag(elf::DT_DEBUG), None);
    assert_eq!(tag(elf::DT_RELA), None);

    let sections = file.elf_section_table();
    let dynsyms = sections.symbols(endian, data, elf::SHT_DYNSYM).unwrap();
    let versions = sections.versions(endian, data).unwrap().unwrap();
    let (hash, _) = sections.gnu_hash(endian, data).unwrap().unwrap();
    for export in &image.exports {
        let name = export.name.as_bytes();
        let (_, symbol) = hash
            .find(endian, name, elf::gnu_hash(name), None, &dynsyms, &versions)
            .unwrap_or_else(|| panic!("`{}` is not in the hash table", export.name));
        assert_eq!(symbol.st_value(endian), export.addr);
        assert_eq!(symbol.st_type(), elf::STT_FUNC);
    }
    assert!(hash.find(endian, b"blaze_export_100", elf::gnu_hash(b"blaze_export_100"), None, &dynsyms, &versions).is_none());
}

#[test]
fn a_pointer_in_read_only_data_is_a_text_relocation() {
    let mut image = Fixture::new(executable(), &[(Class::Text, 16, 16), (Class::Rodata, 8, 8)], &Counts {
        absolute: 1,
        ..Counts::default()
    });
    let rodata = image.at(&Class::Rodata);
    image.rebase = vec![rodata];
    image.entry = Some(image.at(&Class::Text));
    let error = image.synthesize().unwrap_err();
    assert!(error.what.contains("text relocation"), "{}", error.what);
}

#[test]
fn a_shared_librarys_initial_exec_cell_is_an_error() {
    let kind = OutputKind::Shared { name: "libx.so".into(), exports: Vec::new() };
    let output = output(blaze_codegen::Arch::X86_64, kind, None);
    let mut image = Fixture::new(output, &[(Class::Text, 16, 16), (Class::Got, 8, 8)], &Counts { got: 1, ..Counts::default() });
    let got = image.at(&Class::Got);
    image.got = vec![GotEntry { addr: got, holds: Holds::TpOff(-8) }];
    let error = image.synthesize().unwrap_err();
    assert!(error.what.contains("thread pointer offset"), "{}", error.what);
}

#[test]
fn hashes_are_the_elf_ones() {
    for name in [&b""[..], b"malloc", b"GLIBC_2.2.5", b"_ZN4core3fmt5write17h0123456789abcdefE"] {
        assert_eq!(gnu_hash(name), elf::gnu_hash(name));
        assert_eq!(elf_hash(name), elf::hash(name));
    }
}

#[test]
fn sections_are_placed_as_lld_places_them() {
    let named = |name: &str, write: bool, exec: bool| Class::Named { name: name.into(), write, exec };
    let cases = [
        (".text", false, true, false, Class::Text, 0),
        (".text.unlikely._ZN3foo3barE", false, true, false, Class::Text, 0),
        (".rodata.str1.1", false, false, false, Class::Rodata, 0),
        (".gcc_except_table._ZN3foo", false, false, false, Class::Rodata, 0),
        (".data", true, false, false, Class::Data, 0),
        (".data.rel.local", true, false, false, Class::Data, 0),
        (".data.rel.ro.local", true, false, false, Class::RelRo, 0),
        (".data.rel.ro", true, false, false, Class::RelRo, 0),
        (".bss.foo", true, false, false, Class::Bss, 0),
        (".tdata.x", true, false, true, Class::Tdata, 0),
        (".tbss.x", true, false, true, Class::Tbss, 0),
        (".init_array", true, false, false, Class::InitArray, 65536),
        (".init_array.00099", true, false, false, Class::InitArray, 99),
        (".ctors.65435", true, false, false, Class::InitArray, 100),
        (".ctors", true, false, false, Class::InitArray, 65536),
        (".fini_array.5", true, false, false, Class::FiniArray, 5),
        (".dtors", true, false, false, Class::FiniArray, 65536),
        (".preinit_array", true, false, false, named(".preinit_array", true, false), 0),
        ("linkme_ITEMS", true, false, false, named("linkme_ITEMS", true, false), 0),
        ("my_code", false, true, false, named("my_code", false, true), 0),
        (".init", false, true, false, named(".init", false, true), 0),
        (".textual", false, false, false, named(".textual", false, false), 0),
    ];
    for (name, write, exec, tls, class, rank) in cases {
        assert_eq!(section(name, write, exec, tls), Ok((class, rank)), "{name}");
    }
    assert!(section("rwx", true, true, false).is_err());
    assert!(section("", false, false, false).is_err());
}

#[test]
fn order_makes_four_segments() {
    let writer = Elf { output: executable() };
    let mut classes = vec![
        Class::Bss,
        Class::Got,
        Class::Text,
        Class::Synthetic(super::DYNAMIC),
        Class::Tbss,
        Class::Named { name: "linkme_ITEMS".into(), write: true, exec: false },
        Class::Synthetic(super::DYNSYM),
        Class::Rodata,
        Class::Tdata,
        Class::Data,
        Class::Synthetic(super::INTERP),
        Class::EhFrame,
        Class::InitArray,
        Class::Stubs,
        Class::Table,
        Class::RelRo,
    ];
    writer.order(&mut classes);
    let segments: Vec<Segment> = classes.iter().map(Segment::of).collect();
    assert!(segments.is_sorted(), "{classes:?}");
    assert_eq!(classes[0], Class::Synthetic(super::INTERP));
    assert_eq!(classes[classes.len() - 3..], [Class::Tdata, Class::Tbss, Class::Bss]);
    // A change of protection at each change of segment, and none inside one.
    for pair in classes.windows(2) {
        let same = Segment::of(&pair[0]) == Segment::of(&pair[1]);
        assert_eq!(same, writer.prot(&pair[0]) == writer.prot(&pair[1]), "{pair:?}");
    }
}

#[test]
fn specials() {
    let writer = Elf { output: executable() };
    assert_eq!(writer.special("__ehdr_start"), Some(Special::ImageBase));
    assert_eq!(writer.special("__dso_handle"), Some(Special::ImageBase));
    assert_eq!(writer.special("__blaze_live"), Some(Special::LiveDescriptor));
    assert_eq!(
        writer.special("__start_linkme_ITEMS"),
        Some(Special::Section { name: "linkme_ITEMS".into(), end: false })
    );
    assert_eq!(writer.special("__stop_linkme_ITEMS"), Some(Special::Section { name: "linkme_ITEMS".into(), end: true }));
    assert_eq!(writer.special("__start_.data"), None);
    assert_eq!(writer.special("__start_"), None);
    assert_eq!(writer.special("malloc"), None);

    let region = |class: Class, addr: u64, size: u64| {
        let prot = writer.prot(&class);
        Region { class, addr, capacity: size, size, prot, align: 8 }
    };
    let regions = [
        region(Class::Synthetic(super::DYNSYM), 0x300, 0x40),
        region(Class::Text, 0x1000, 0x100),
        region(Class::InitArray, 0x2000, 0x10),
        region(Class::Synthetic(super::DYNAMIC), 0x2010, 0x100),
        region(Class::Got, 0x2110, 0x20),
        region(Class::Data, 0x3000, 0x18),
        region(Class::Bss, 0x3018, 0x40),
    ];
    let at = |name: &str| match writer.special(name) {
        Some(Special::Placed(address)) => address(&regions),
        other => panic!("`{name}` is {other:?}"),
    };
    assert_eq!(at("__init_array_start"), 0x2000);
    assert_eq!(at("__init_array_end"), 0x2010);
    assert_eq!(at("__fini_array_start"), 0, "an absent array is empty at the image's start");
    assert_eq!(at("__fini_array_end"), 0);
    assert_eq!(at("_DYNAMIC"), 0x2010);
    assert_eq!(at("_GLOBAL_OFFSET_TABLE_"), 0x2110);
    assert_eq!(at("etext"), 0x1100);
    assert_eq!(at("_edata"), 0x3018);
    assert_eq!(at("__bss_start"), 0x3018);
    assert_eq!(at("_end"), 0x3058);
    assert_eq!(at("end"), 0x3058);
}

#[test]
fn wasm_is_not_elf() {
    let mut output = executable();
    output.arch = blaze_codegen::Arch::Wasm32;
    assert!(writer(&output).is_err());
    assert!(writer(&executable()).is_ok());
}

fn insn(word: u32) -> [u8; 4] {
    word.to_le_bytes()
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

#[test]
fn native_relocations() {
    use blaze_codegen::Arch::{Aarch64, X86_64};
    let apply = super::native::apply;

    let mut bytes = [0u8; 2];
    apply(X86_64, elf::R_X86_64_16, &mut bytes, 0x1000, 0xfff0, 0x0f).unwrap();
    assert_eq!(bytes, [0xff, 0xff]);
    assert!(apply(X86_64, elf::R_X86_64_16, &mut bytes, 0, 0x1_0000, 0).is_err());
    let mut bytes = [0u8; 1];
    apply(X86_64, elf::R_X86_64_PC8, &mut bytes, 0x1000, 0x1000, -2).unwrap();
    assert_eq!(bytes, [0xfe]);
    assert!(apply(X86_64, elf::R_X86_64_PC8, &mut bytes, 0x1000, 0x1080, 0).is_err());
    assert!(apply(X86_64, elf::R_X86_64_PC32, &mut [0; 4], 0, 0, 0).is_err(), "not a native type");

    // ADRP x0, page(S) with no range check.
    let mut bytes = insn(0x9000_0000);
    apply(Aarch64, elf::R_AARCH64_ADR_PREL_PG_HI21_NC, &mut bytes, 0x1_0000, 0x1_3456, 0).unwrap();
    let w = word(&bytes);
    let imm = ((w >> 29) & 3) | (((w >> 5) & 0x7_ffff) << 2);
    assert_eq!(imm, 3);
    assert_eq!(w & 0x9f00_001f, 0x9000_0000);

    // LDR x1, label: 19 bits of words.
    let mut bytes = insn(0x5800_0001);
    apply(Aarch64, elf::R_AARCH64_LD_PREL_LO19, &mut bytes, 0x1000, 0x900, 0).unwrap();
    assert_eq!((word(&bytes) >> 5) & 0x7_ffff, ((-0x700i32 >> 2) as u32) & 0x7_ffff);
    assert!(apply(Aarch64, elf::R_AARCH64_LD_PREL_LO19, &mut insn(0x5800_0001), 0x1000, 0x1002, 0).is_err());

    // A negative MOVZ becomes MOVN of the inverted bits; a MOVK keeps its opcode.
    let movz = 0xd280_0000u32;
    let mut bytes = insn(movz);
    apply(Aarch64, elf::R_AARCH64_MOVW_SABS_G0, &mut bytes, 0, 0, -2).unwrap();
    assert_eq!(word(&bytes), 0x9280_0000 | (1 << 5));
    let mut bytes = insn(movz);
    apply(Aarch64, elf::R_AARCH64_MOVW_SABS_G0, &mut bytes, 0, 0x1234, 0).unwrap();
    assert_eq!(word(&bytes), movz | (0x1234 << 5));
    assert!(apply(Aarch64, elf::R_AARCH64_MOVW_SABS_G0, &mut insn(movz), 0, 0x1_0000, 0).is_err());
    let movk = 0xf2a0_0000u32;
    let mut bytes = insn(movk);
    apply(Aarch64, elf::R_AARCH64_MOVW_PREL_G1_NC, &mut bytes, 0x1000, 0x1_2345_1000, 0).unwrap();
    assert_eq!(word(&bytes), movk | (0x2345 << 5));

    let mut bytes = [0u8; 2];
    apply(Aarch64, elf::R_AARCH64_PREL16, &mut bytes, 0x2000, 0x1000, 0).unwrap();
    assert_eq!(u16::from_le_bytes(bytes), (-0x1000i16) as u16);
}
