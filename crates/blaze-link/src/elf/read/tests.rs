use blaze_codegen::Arch;
use object::write::{
    Comdat, Object as Builder, Relocation, SectionId, StandardSection, Symbol as Def, SymbolId,
    SymbolSection,
};
use object::{
    Architecture, BinaryFormat, ComdatKind, Endianness, RelocationFlags, SectionFlags,
    SectionKind as Kind, SymbolFlags, SymbolKind as Type, SymbolScope, elf,
};

use super::read;
use crate::NativeKind;
use crate::model::{
    Apply, Binding, Data, Group, NativeFile, NativeFixup, Object, Section, SectionKind, Symbol,
    SymbolDef, SymbolKind,
};

const SHARED: NativeKind = NativeKind::Shared { as_needed: false };
const ARCHIVE: NativeKind = NativeKind::Archive { whole: false };

fn object(result: Result<NativeFile, String>) -> Object {
    match result {
        Ok(NativeFile::Object(object)) => object,
        Ok(_) => panic!("read as something other than an object"),
        Err(error) => panic!("{error}"),
    }
}

fn error(result: Result<NativeFile, String>) -> String {
    match result {
        Ok(_) => panic!("read without an error"),
        Err(error) => error,
    }
}

fn alloc(name: &str, write: bool, exec: bool, tls: bool) -> SectionKind {
    let (class, rank) = super::super::section(name, write, exec, tls).unwrap();
    SectionKind::Alloc(class, rank)
}

fn builder(arch: Architecture) -> Builder<'static> {
    Builder::new(BinaryFormat::Elf, arch, Endianness::Little)
}

fn define(
    builder: &mut Builder<'_>,
    name: &str,
    section: SectionId,
    value: u64,
    size: u64,
    kind: Type,
    scope: SymbolScope,
    weak: bool,
) -> SymbolId {
    builder.add_symbol(Def {
        name: name.into(),
        value,
        size,
        kind,
        scope,
        weak,
        section: SymbolSection::Section(section),
        flags: SymbolFlags::None,
    })
}

fn undefined(builder: &mut Builder<'_>, name: &str, weak: bool) -> SymbolId {
    builder.add_symbol(Def {
        name: name.into(),
        value: 0,
        size: 0,
        kind: Type::Unknown,
        scope: SymbolScope::Dynamic,
        weak,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    })
}

fn relocate(builder: &mut Builder<'_>, section: SectionId, at: u64, symbol: SymbolId, addend: i64, r_type: u32) {
    builder
        .add_relocation(section, Relocation {
            offset: at,
            symbol,
            addend,
            flags: RelocationFlags::Elf { r_type },
        })
        .unwrap();
}

fn flags(builder: &mut Builder<'_>, section: SectionId, sh_flags: u32) {
    builder.section_mut(section).flags = SectionFlags::Elf { sh_flags: u64::from(sh_flags) };
}

fn section<'a>(object: &'a Object, name: &str) -> (u32, &'a Section) {
    let index = object.sections.iter().position(|it| it.name == name).unwrap_or_else(|| {
        panic!("no section {name}");
    });
    (index as u32, &object.sections[index])
}

fn symbol(object: &Object, name: &str) -> u32 {
    object.symbols.iter().position(|it| it.name == name).unwrap_or_else(|| {
        panic!("no symbol {name}");
    }) as u32
}

fn check(
    section: &Section,
    what: SectionKind,
    align: u64,
    data: Data,
    fixups: Vec<NativeFixup>,
    retain: bool,
) {
    assert_eq!(section.what, what, "{}", section.name);
    assert_eq!(section.align, align, "{}", section.name);
    assert_eq!(section.data, data, "{}", section.name);
    assert_eq!(section.fixups, fixups, "{}", section.name);
    assert_eq!(section.retain, retain, "{}", section.name);
}

fn fixup(at: u64, apply: Apply, symbol: u32, addend: i64) -> NativeFixup {
    NativeFixup { at, apply, symbol, addend, minus: None }
}

fn global(name: &str, def: SymbolDef, kind: SymbolKind) -> Symbol {
    Symbol { name: name.to_owned(), def, binding: Binding::Global, kind, hidden: false }
}

/// An x86-64 object with every kind of section and symbol the reader distinguishes.
fn x86_64() -> Vec<u8> {
    let mut b = builder(Architecture::X86_64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0x90; 64], 16);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 16], 8);
    let tbss = b.section_id(StandardSection::UninitializedTls);
    let inline = b.add_section(Vec::new(), b".text.inl".to_vec(), Kind::Text);
    b.append_section_data(inline, &[0xc3], 1);
    let keep = b.add_section(Vec::new(), b".data.keep".to_vec(), Kind::Data);
    b.append_section_data(keep, &[1, 2, 3, 4], 4);
    flags(&mut b, keep, elf::SHF_ALLOC | elf::SHF_WRITE | elf::SHF_GNU_RETAIN);
    let eh_frame = b.add_section(Vec::new(), b".eh_frame".to_vec(), Kind::Elf(elf::SHT_X86_64_UNWIND));
    b.append_section_data(eh_frame, &[0; 24], 8);
    flags(&mut b, eh_frame, elf::SHF_ALLOC);
    let debug = b.add_section(Vec::new(), b".debug_info".to_vec(), Kind::Debug);
    b.append_section_data(debug, &[0; 16], 1);

    let f = define(&mut b, "f", text, 0, 16, Type::Text, SymbolScope::Dynamic, false);
    let g = define(&mut b, "g", text, 16, 8, Type::Text, SymbolScope::Linkage, false);
    let local = define(&mut b, "local", text, 24, 4, Type::Text, SymbolScope::Compilation, false);
    let ext = undefined(&mut b, "ext", false);
    let weak = undefined(&mut b, "wk", true);
    let got = undefined(&mut b, "_GLOBAL_OFFSET_TABLE_", false);
    let tvar = b.add_symbol(Def {
        name: b"tvar".to_vec(),
        value: 0,
        size: 0,
        kind: Type::Tls,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    b.add_symbol_bss(tvar, tbss, 8, 8);
    let comm = b.add_common_symbol(
        Def {
            name: b"comm".to_vec(),
            value: 8,
            size: 0,
            kind: Type::Data,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        },
        16,
        8,
    );
    let inl = define(&mut b, "inl", inline, 0, 1, Type::Text, SymbolScope::Dynamic, false);
    b.add_comdat(Comdat { kind: ComdatKind::Any, symbol: inl, sections: vec![inline] });

    relocate(&mut b, text, 1, ext, -4, elf::R_X86_64_PLT32);
    relocate(&mut b, text, 6, ext, -4, elf::R_X86_64_REX_GOTPCRELX);
    relocate(&mut b, text, 11, g, -4, elf::R_X86_64_PC32);
    relocate(&mut b, text, 16, tvar, -4, elf::R_X86_64_TLSGD);
    relocate(&mut b, text, 21, tvar, -4, elf::R_X86_64_TLSLD);
    relocate(&mut b, text, 26, tvar, 0, elf::R_X86_64_DTPOFF32);
    relocate(&mut b, text, 30, tvar, -4, elf::R_X86_64_GOTTPOFF);
    relocate(&mut b, text, 34, tvar, 0, elf::R_X86_64_TPOFF32);
    relocate(&mut b, text, 38, got, 2, elf::R_X86_64_GOTPC32);
    relocate(&mut b, text, 42, f, 0, elf::R_X86_64_SIZE32);
    relocate(&mut b, text, 46, comm, 0, elf::R_X86_64_32S);
    relocate(&mut b, text, 50, weak, 0, elf::R_X86_64_PC16);
    relocate(&mut b, text, 52, local, 0, elf::R_X86_64_NONE);
    relocate(&mut b, text, 56, weak, -4, elf::R_X86_64_PLT32);
    relocate(&mut b, data, 0, f, 8, elf::R_X86_64_64);
    relocate(&mut b, data, 8, g, 0, elf::R_X86_64_GOTOFF64);
    let text_symbol = b.section_symbol(text);
    relocate(&mut b, eh_frame, 8, text_symbol, 0, elf::R_X86_64_PC32);
    // Debug sections are never relocated into the image, so a type the loaded code can't have
    // must not stop the read.
    relocate(&mut b, debug, 0, text_symbol, 0, elf::R_X86_64_GOT64);
    b.write().unwrap()
}

#[test]
fn reads_an_x86_64_object() {
    let bytes = x86_64();
    let object = object(read(Arch::X86_64, "t.o", &bytes, &NativeKind::Object));

    let (text, text_section) = section(&object, ".text");
    let (_, data_section) = section(&object, ".data");
    let (tbss, tbss_section) = section(&object, ".tbss");
    let (inline, inline_section) = section(&object, ".text.inl");
    let (_, keep_section) = section(&object, ".data.keep");
    let (_, eh_frame_section) = section(&object, ".eh_frame");

    assert_eq!(object.symbols[0], Symbol {
        name: String::new(),
        def: SymbolDef::Absolute(0),
        binding: Binding::Local,
        kind: SymbolKind::Other,
        hidden: false,
    });
    let f = symbol(&object, "f");
    let g = symbol(&object, "g");
    let local = symbol(&object, "local");
    let ext = symbol(&object, "ext");
    let weak = symbol(&object, "wk");
    let tvar = symbol(&object, "tvar");
    let comm = symbol(&object, "comm");
    let inl = symbol(&object, "inl");
    let at = |symbol: u32| &object.symbols[symbol as usize];
    assert_eq!(
        *at(f),
        global("f", SymbolDef::Section { section: text, value: 0, size: 16 }, SymbolKind::Function)
    );
    assert_eq!(*at(g), Symbol {
        hidden: true,
        ..global("g", SymbolDef::Section { section: text, value: 16, size: 8 }, SymbolKind::Function)
    });
    assert_eq!(*at(local), Symbol {
        binding: Binding::Local,
        ..global(
            "local",
            SymbolDef::Section { section: text, value: 24, size: 4 },
            SymbolKind::Function,
        )
    });
    assert_eq!(*at(ext), global("ext", SymbolDef::Undefined, SymbolKind::Other));
    assert_eq!(*at(weak), Symbol {
        binding: Binding::Weak,
        ..global("wk", SymbolDef::Undefined, SymbolKind::Other)
    });
    assert_eq!(
        *at(tvar),
        global("tvar", SymbolDef::Section { section: tbss, value: 0, size: 8 }, SymbolKind::Tls)
    );
    assert_eq!(*at(comm), global("comm", SymbolDef::Common { size: 16, align: 8 }, SymbolKind::Data));
    assert_eq!(
        *at(inl),
        global("inl", SymbolDef::Section { section: inline, value: 0, size: 1 }, SymbolKind::Function)
    );
    let text_symbol = object
        .symbols
        .iter()
        .position(|it| {
            it.kind == SymbolKind::Section
                && it.def == SymbolDef::Section { section: text, value: 0, size: 0 }
        })
        .expect("the section symbol of .text") as u32;
    assert_eq!(*at(text_symbol), Symbol {
        name: String::new(),
        def: SymbolDef::Section { section: text, value: 0, size: 0 },
        binding: Binding::Local,
        kind: SymbolKind::Section,
        hidden: false,
    });

    // The module symbol (TLSLD), _GLOBAL_OFFSET_TABLE_ (GOTPC32, GOTOFF64) and f's size
    // (SIZE32) follow the ELF symbols, in the order the relocations ask for them.
    let added = object.symbols.len() as u32 - 3;
    let (module, got, size) = (added, added + 1, added + 2);
    assert_eq!(*at(module), Symbol {
        name: String::new(),
        def: SymbolDef::Absolute(0),
        binding: Binding::Local,
        kind: SymbolKind::Tls,
        hidden: true,
    });
    assert_eq!(*at(got), global("_GLOBAL_OFFSET_TABLE_", SymbolDef::Undefined, SymbolKind::Other));
    assert_eq!(*at(size), Symbol {
        name: String::new(),
        def: SymbolDef::Absolute(16),
        binding: Binding::Local,
        kind: SymbolKind::Other,
        hidden: true,
    });

    check(
        text_section,
        alloc(".text", false, true, false),
        16,
        Data::Bytes(vec![0x90; 64]),
        vec![
            fixup(1, Apply::X86Branch32, ext, -4),
            fixup(6, Apply::X86GotPcRel32, ext, -4),
            fixup(11, Apply::Rel32, g, -4),
            fixup(16, Apply::X86TlsGd32, tvar, -4),
            fixup(21, Apply::X86TlsGd32, module, -4),
            fixup(26, Apply::DtpOff32, tvar, 0),
            fixup(30, Apply::X86GotTpOff32, tvar, -4),
            fixup(34, Apply::X86TpOff32, tvar, 0),
            fixup(38, Apply::Rel32, got, 2),
            fixup(42, Apply::Abs32, size, 0),
            fixup(46, Apply::Abs32S, comm, 0),
            fixup(50, Apply::Native(elf::R_X86_64_PC16), weak, 0),
            fixup(56, Apply::X86Branch32, weak, -4),
        ],
        false,
    );
    check(
        data_section,
        alloc(".data", true, false, false),
        8,
        Data::Bytes(vec![0; 16]),
        vec![fixup(0, Apply::Abs64, f, 8), NativeFixup {
            at: 8,
            apply: Apply::Delta64,
            symbol: g,
            addend: 0,
            minus: Some(got),
        }],
        false,
    );
    check(tbss_section, alloc(".tbss", true, false, true), 8, Data::Zero(8), Vec::new(), false);
    check(
        inline_section,
        alloc(".text.inl", false, true, false),
        1,
        Data::Bytes(vec![0xc3]),
        Vec::new(),
        false,
    );
    check(
        keep_section,
        alloc(".data.keep", true, false, false),
        4,
        Data::Bytes(vec![1, 2, 3, 4]),
        Vec::new(),
        true,
    );
    check(
        eh_frame_section,
        SectionKind::EhFrame,
        8,
        Data::Bytes(vec![0; 24]),
        vec![fixup(8, Apply::Rel32, text_symbol, 0)],
        false,
    );
    let loaded = [".text", ".data", ".tbss", ".text.inl", ".data.keep", ".eh_frame"];
    for section in &object.sections {
        if !loaded.contains(&section.name.as_str()) {
            assert_eq!(section.what, SectionKind::Dropped, "{}", section.name);
            assert_eq!(section.data, Data::Zero(0), "{}", section.name);
            assert!(section.fixups.is_empty(), "{}", section.name);
            assert!(!section.retain, "{}", section.name);
        }
    }
    assert_eq!(object.sections[0].what, SectionKind::Dropped);
    assert_eq!(section(&object, ".debug_info").1.what, SectionKind::Dropped);
    assert_eq!(section(&object, ".group").1.what, SectionKind::Dropped);
    assert_eq!(section(&object, ".symtab").1.what, SectionKind::Dropped);
    assert_eq!(section(&object, ".rela.text").1.what, SectionKind::Dropped);

    assert_eq!(object.groups, vec![Group { name: "inl".to_owned(), sections: vec![inline] }]);
    assert!(object.libraries.is_empty());
}

#[test]
fn reads_an_object_at_any_alignment() {
    let bytes = x86_64();
    let mut shifted = vec![0; bytes.len() + 1];
    shifted[1..].copy_from_slice(&bytes);
    let input = &shifted[1..];
    assert_ne!(input.as_ptr().align_offset(8), 0);
    let aligned = object(read(Arch::X86_64, "t.o", &bytes, &NativeKind::Object));
    let unaligned = object(read(Arch::X86_64, "t.o", input, &NativeKind::Object));
    assert_eq!(format!("{aligned:?}"), format!("{unaligned:?}"));
}

#[test]
fn reads_an_aarch64_object() {
    let mut b = builder(Architecture::Aarch64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 80], 4);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 8], 8);
    let tbss = b.section_id(StandardSection::UninitializedTls);
    let eh_frame = b.add_section(Vec::new(), b".eh_frame".to_vec(), Kind::Elf(elf::SHT_PROGBITS));
    b.append_section_data(eh_frame, &[0; 16], 8);
    flags(&mut b, eh_frame, elf::SHF_ALLOC);
    let f = define(&mut b, "f", text, 0, 80, Type::Text, SymbolScope::Dynamic, false);
    let ext = undefined(&mut b, "ext", false);
    let var = define(&mut b, "var", data, 0, 8, Type::Data, SymbolScope::Dynamic, false);
    let tvar = b.add_symbol(Def {
        name: b"tvar".to_vec(),
        value: 0,
        size: 0,
        kind: Type::Tls,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    b.add_symbol_bss(tvar, tbss, 8, 8);
    let relocations = [
        (0, ext, elf::R_AARCH64_CALL26),
        (4, ext, elf::R_AARCH64_JUMP26),
        (8, var, elf::R_AARCH64_ADR_PREL_PG_HI21),
        (12, var, elf::R_AARCH64_ADD_ABS_LO12_NC),
        (16, var, elf::R_AARCH64_LDST64_ABS_LO12_NC),
        (20, var, elf::R_AARCH64_LDST128_ABS_LO12_NC),
        (24, ext, elf::R_AARCH64_ADR_GOT_PAGE),
        (28, ext, elf::R_AARCH64_LD64_GOT_LO12_NC),
        (32, tvar, elf::R_AARCH64_TLSDESC_ADR_PAGE21),
        (36, tvar, elf::R_AARCH64_TLSDESC_LD64_LO12),
        (40, tvar, elf::R_AARCH64_TLSDESC_ADD_LO12),
        (44, tvar, elf::R_AARCH64_TLSDESC_CALL),
        (48, tvar, elf::R_AARCH64_TLSLE_ADD_TPREL_HI12),
        (52, tvar, elf::R_AARCH64_TLSLE_ADD_TPREL_LO12_NC),
        (56, var, elf::R_AARCH64_MOVW_UABS_G1_NC),
        (60, f, elf::R_AARCH64_CONDBR19),
        (64, f, elf::R_AARCH64_TSTBR14),
        (68, var, elf::R_AARCH64_LD_PREL_LO19),
        (72, var, elf::R_AARCH64_NONE),
        (76, var, elf::R_AARCH64_PREL16),
    ];
    for (at, symbol, r_type) in relocations {
        relocate(&mut b, text, at, symbol, 1, r_type);
    }
    relocate(&mut b, data, 0, f, 4, elf::R_AARCH64_ABS64);
    let text_symbol = b.section_symbol(text);
    relocate(&mut b, eh_frame, 8, text_symbol, 0, elf::R_AARCH64_PREL32);
    let bytes = b.write().unwrap();

    let object = object(read(Arch::Aarch64, "a.o", &bytes, &NativeKind::Object));
    let (text, text_section) = section(&object, ".text");
    let f = symbol(&object, "f");
    let ext = symbol(&object, "ext");
    let var = symbol(&object, "var");
    let tvar = symbol(&object, "tvar");
    check(
        text_section,
        alloc(".text", false, true, false),
        4,
        Data::Bytes(vec![0; 80]),
        vec![
            fixup(0, Apply::A64Call26, ext, 1),
            fixup(4, Apply::A64Jump26, ext, 1),
            fixup(8, Apply::A64Page21, var, 1),
            fixup(12, Apply::A64Lo12 { shift: 0 }, var, 1),
            fixup(16, Apply::A64Lo12 { shift: 3 }, var, 1),
            fixup(20, Apply::A64Lo12 { shift: 4 }, var, 1),
            fixup(24, Apply::A64GotPage21, ext, 1),
            fixup(28, Apply::A64GotLo12, ext, 1),
            fixup(32, Apply::A64TlsDescPage21, tvar, 1),
            fixup(36, Apply::A64TlsDescLo12, tvar, 1),
            fixup(40, Apply::A64TlsDescAdd12, tvar, 1),
            fixup(44, Apply::A64TlsDescCall, tvar, 1),
            fixup(48, Apply::A64TpHi12, tvar, 1),
            fixup(52, Apply::A64TpLo12, tvar, 1),
            fixup(56, Apply::A64MovW { group: 1 }, var, 1),
            fixup(60, Apply::A64CondBr19, f, 1),
            fixup(64, Apply::A64TstBr14, f, 1),
            fixup(68, Apply::Native(elf::R_AARCH64_LD_PREL_LO19), var, 1),
            fixup(76, Apply::Native(elf::R_AARCH64_PREL16), var, 1),
        ],
        false,
    );
    check(
        section(&object, ".data").1,
        alloc(".data", true, false, false),
        8,
        Data::Bytes(vec![0; 8]),
        vec![fixup(0, Apply::Abs64, f, 4)],
        false,
    );
    let text_symbol = object
        .symbols
        .iter()
        .position(|it| it.kind == SymbolKind::Section && it.def == SymbolDef::Section {
            section: text,
            value: 0,
            size: 0,
        })
        .unwrap() as u32;
    check(
        section(&object, ".eh_frame").1,
        SectionKind::EhFrame,
        8,
        Data::Bytes(vec![0; 16]),
        vec![fixup(8, Apply::Rel32, text_symbol, 0)],
        false,
    );
    assert_eq!(
        object.symbols[var as usize],
        global(
            "var",
            SymbolDef::Section { section: section(&object, ".data").0, value: 0, size: 8 },
            SymbolKind::Data,
        )
    );
    // No relocation of this object adds a symbol.
    assert_eq!(object.symbols.last().unwrap().name, "tvar");
}

fn with_text_relocation(r_type: u32) -> Vec<u8> {
    let mut b = builder(Architecture::X86_64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 16], 16);
    let ext = undefined(&mut b, "ext", false);
    relocate(&mut b, text, 0, ext, 0, r_type);
    b.write().unwrap()
}

#[test]
fn an_unknown_relocation_is_an_error() {
    let bytes = with_text_relocation(elf::R_X86_64_GOT64);
    let message = error(read(Arch::X86_64, "bad.o", &bytes, &NativeKind::Object));
    for part in ["bad.o", ".text", "R_X86_64_GOT64", "(27)", "large code model"] {
        assert!(message.contains(part), "{message}");
    }
    let bytes = with_text_relocation(elf::R_X86_64_GOTPC32_TLSDESC);
    let message = error(read(Arch::X86_64, "bad.o", &bytes, &NativeKind::Object));
    assert!(message.contains("R_X86_64_GOTPC32_TLSDESC"), "{message}");
    let bytes = with_text_relocation(250);
    let message = error(read(Arch::X86_64, "bad.o", &bytes, &NativeKind::Object));
    assert!(message.contains("bad.o") && message.contains("(250)"), "{message}");
}

#[test]
fn an_ifunc_is_an_error() {
    let mut b = builder(Architecture::X86_64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 16], 16);
    let resolver = define(&mut b, "resolve_me", text, 0, 16, Type::Text, SymbolScope::Dynamic, false);
    b.symbol_mut(resolver).flags = SymbolFlags::Elf {
        st_info: (elf::STB_GLOBAL << 4) | elf::STT_GNU_IFUNC,
        st_other: elf::STV_DEFAULT,
    };
    let bytes = b.write().unwrap();
    let message = error(read(Arch::X86_64, "i.o", &bytes, &NativeKind::Object));
    assert!(message.contains("i.o") && message.contains("resolve_me"), "{message}");
}

#[test]
fn a_compressed_loaded_section_is_an_error() {
    let mut b = builder(Architecture::X86_64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 32], 16);
    flags(&mut b, text, elf::SHF_ALLOC | elf::SHF_EXECINSTR | elf::SHF_COMPRESSED);
    let bytes = b.write().unwrap();
    let message = error(read(Arch::X86_64, "z.o", &bytes, &NativeKind::Object));
    assert!(message.contains("z.o") && message.contains(".text"), "{message}");
}

#[test]
fn the_wrong_machine_or_kind_is_an_error() {
    let bytes = x86_64();
    let message = error(read(Arch::Aarch64, "t.o", &bytes, &NativeKind::Object));
    assert!(message.contains("t.o") && message.contains("x86-64"), "{message}");
    let message = error(read(Arch::X86_64, "t.o", &bytes, &SHARED));
    assert!(message.contains("t.o") && message.contains("shared"), "{message}");
    let message = error(read(Arch::X86_64, "t.o", &bytes, &ARCHIVE));
    assert!(message.contains("t.o") && message.contains("archive"), "{message}");
}

/// A GNU archive of `members`, with a `/` symbol table of `symbols` (each a name and the
/// member defining it) when given.
fn ar(symbols: Option<&[(&str, usize)]>, members: &[(&str, &[u8])]) -> Vec<u8> {
    fn header(out: &mut Vec<u8>, name: &str, size: usize) {
        let header = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644);
        assert_eq!(header.len(), 60);
        out.extend_from_slice(header.as_bytes());
    }
    let table = symbols.map(|symbols| {
        let names: usize = symbols.iter().map(|(name, _)| name.len() + 1).sum();
        (symbols, 4 + 4 * symbols.len() + names)
    });
    let mut offset = 8 + table.map_or(0, |(_, size)| 60 + size + size % 2);
    let mut offsets = Vec::new();
    for (_, bytes) in members {
        offsets.push(offset);
        offset += 60 + bytes.len() + bytes.len() % 2;
    }
    let mut out = b"!<arch>\n".to_vec();
    if let Some((symbols, size)) = table {
        header(&mut out, "/", size);
        out.extend_from_slice(&(symbols.len() as u32).to_be_bytes());
        for (_, member) in symbols {
            out.extend_from_slice(&(offsets[*member] as u32).to_be_bytes());
        }
        for (name, _) in symbols {
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        if size % 2 == 1 {
            out.push(b'\n');
        }
    }
    for (name, bytes) in members {
        header(&mut out, &format!("{name}/"), bytes.len());
        out.extend_from_slice(bytes);
        if bytes.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    out
}

fn members() -> (Vec<u8>, Vec<u8>) {
    let mut a = builder(Architecture::X86_64);
    let text = a.section_id(StandardSection::Text);
    a.append_section_data(text, &[0; 8], 16);
    define(&mut a, "alpha", text, 0, 8, Type::Text, SymbolScope::Dynamic, false);
    let beta = undefined(&mut a, "beta", false);
    relocate(&mut a, text, 1, beta, -4, elf::R_X86_64_PLT32);
    let mut b = builder(Architecture::X86_64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 8], 16);
    define(&mut b, "local_b", text, 0, 4, Type::Text, SymbolScope::Compilation, false);
    define(&mut b, "beta", text, 0, 4, Type::Text, SymbolScope::Dynamic, false);
    define(&mut b, "gamma", text, 4, 4, Type::Text, SymbolScope::Dynamic, true);
    (a.write().unwrap(), b.write().unwrap())
}

fn archive(result: Result<NativeFile, String>) -> crate::model::Archive {
    match result {
        Ok(NativeFile::Archive(archive)) => archive,
        Ok(_) => panic!("read as something other than an archive"),
        Err(error) => panic!("{error}"),
    }
}

#[test]
fn reads_an_archive_by_its_symbol_table() {
    let (a, b) = members();
    let symbols: &[(&str, usize)] = &[("alpha", 0), ("beta", 1), ("gamma", 1)];
    let bytes = ar(Some(symbols), &[("a.o", &a[..]), ("b.o", &b[..])]);
    let archive = archive(read(Arch::X86_64, "libab.a", &bytes, &ARCHIVE));
    let names: Vec<_> = archive.members.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["a.o", "b.o"]);
    assert_eq!(&bytes[archive.members[0].bytes.clone()], &a[..]);
    assert_eq!(&bytes[archive.members[1].bytes.clone()], &b[..]);
    assert_eq!(archive.index, vec![
        ("alpha".to_owned(), 0),
        ("beta".to_owned(), 1),
        ("gamma".to_owned(), 1)
    ]);
    // A member reads as an object from its range, at whatever alignment it has there.
    let member = object(read(
        Arch::X86_64,
        "libab.a(b.o)",
        &bytes[archive.members[1].bytes.clone()],
        &NativeKind::Object,
    ));
    symbol(&member, "gamma");
}

#[test]
fn indexes_an_archive_without_a_symbol_table() {
    let (a, b) = members();
    let bytes = ar(None, &[("a.o", &a[..]), ("b.o", &b[..])]);
    let archive = archive(read(Arch::X86_64, "libab.a", &bytes, &ARCHIVE));
    assert_eq!(archive.members.len(), 2);
    assert_eq!(archive.index, vec![
        ("alpha".to_owned(), 0),
        ("beta".to_owned(), 1),
        ("gamma".to_owned(), 1)
    ]);
}

#[test]
fn a_thin_archive_is_an_error() {
    let message = error(read(Arch::X86_64, "thin.a", b"!<thin>\n", &ARCHIVE));
    assert!(message.contains("thin.a") && message.contains("thin"), "{message}");
}

fn shared(result: Result<NativeFile, String>) -> crate::model::Shared {
    match result {
        Ok(NativeFile::Shared(shared)) => shared,
        Ok(_) => panic!("read as something other than a shared library"),
        Err(error) => panic!("{error}"),
    }
}

#[test]
fn reads_a_linker_script() {
    let script = b"/* GNU ld script\n   two lines */\nOUTPUT_FORMAT(\"elf64-x86-64\", \"elf64-x86-64\",\n \"elf64-x86-64\")\nOUTPUT_ARCH(i386:x86-64)\nGROUP ( /lib/a.so.1, /usr/lib/b_nonshared.a  AS_NEEDED ( /lib64/ld.so.2 ) )\nINPUT(c.o)\n";
    let shared = shared(read(Arch::X86_64, "/usr/lib/libx.so", script, &SHARED));
    assert_eq!(shared.name, "libx.so");
    assert!(shared.symbols.is_empty() && shared.more.is_empty());
    assert_eq!(shared.inputs, ["/lib/a.so.1", "/usr/lib/b_nonshared.a", "/lib64/ld.so.2", "c.o"]);
}

#[test]
fn a_linker_script_searching_libraries_is_an_error() {
    let message = error(read(Arch::X86_64, "libx.so", b"GROUP ( -lfoo )", &SHARED));
    assert!(message.contains("libx.so") && message.contains("-lfoo"), "{message}");
    let message = error(read(Arch::X86_64, "libx.so", b"SEARCH_DIR(\"/usr/lib\")", &SHARED));
    assert!(message.contains("libx.so") && message.contains("SEARCH_DIR"), "{message}");
    let message = error(read(Arch::X86_64, "libx.so", b"GROUP ( a.so", &SHARED));
    assert!(message.contains("libx.so"), "{message}");
}

#[test]
fn a_linker_script_is_only_a_shared_library() {
    let message = error(read(Arch::X86_64, "x.o", b"GROUP ( a.so )", &NativeKind::Object));
    assert!(message.contains("x.o"), "{message}");
}

/// glibc's `libc.so`, a script; skipped where it is absent (macOS, other distributions).
#[test]
fn reads_the_system_libc_script() {
    for (arch, path, loader) in [
        (Arch::X86_64, "/usr/lib/x86_64-linux-gnu/libc.so", "ld-linux-x86-64.so.2"),
        (Arch::Aarch64, "/usr/lib/aarch64-linux-gnu/libc.so", "ld-linux-aarch64.so.1"),
    ] {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let shared = shared(read(arch, path, &bytes, &SHARED));
        assert_eq!(shared.name, "libc.so");
        let named = |end: &str| shared.inputs.iter().any(|input| input.ends_with(end));
        assert!(named("/libc.so.6"), "{:?}", shared.inputs);
        assert!(named("/libc_nonshared.a"), "{:?}", shared.inputs);
        assert!(named(loader), "{:?}", shared.inputs);
    }
}

/// glibc's `libc.so.6`; skipped where it is absent.
#[test]
fn reads_the_system_libc() {
    for (arch, path, version) in [
        (Arch::X86_64, "/lib/x86_64-linux-gnu/libc.so.6", "GLIBC_2.3"),
        (Arch::Aarch64, "/lib/aarch64-linux-gnu/libc.so.6", "GLIBC_2.17"),
    ] {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let shared = shared(read(arch, path, &bytes, &SHARED));
        assert_eq!(shared.name, "libc.so.6");
        assert!(shared.more.is_empty() && shared.inputs.is_empty());
        // realpath@GLIBC_2.2.5 is hidden on x86-64: only the default version is importable.
        let realpath: Vec<_> = shared.symbols.iter().filter(|it| it.name == "realpath").collect();
        assert_eq!(realpath.len(), 1, "{realpath:?}");
        assert_eq!(realpath[0].version.as_deref(), Some(version));
        assert_eq!(realpath[0].kind, SymbolKind::Function);
        assert!(!realpath[0].weak);
        // An IFUNC of a shared library is the loader's to resolve: it reads as a function.
        assert!(
            shared.symbols.iter().any(|it| it.name == "memcpy" && it.kind == SymbolKind::Function),
            "memcpy"
        );
        assert!(shared.symbols.iter().all(|it| !it.name.is_empty()));
    }
}
