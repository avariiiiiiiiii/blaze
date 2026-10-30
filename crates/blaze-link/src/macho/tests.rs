//! Images written from hand-made `Placed` values and read back with `object`'s Mach-O reader
//! and decoders of the opcodes, the trie and the signature written here.

use blaze_codegen::Arch;
use object::LittleEndian as LE;
use object::macho;
use object::read::macho::{MachHeader as _, Section as _, Segment as _};

use super::{core_name, linkedit, macho_name, sha256, sign};
use crate::model::{
    Bind, Class, Export, Function, GotEntry, Holds, Import, Library, Placed, Plan, Prot, Region,
    SectionKind, Special, SymbolKind, Tls, Writer,
};
use crate::output::{Debug, Platform};
use crate::{Format, Output, OutputKind};

fn output(arch: Arch, kind: OutputKind) -> Output {
    Output {
        format: Format::MachO,
        arch,
        kind,
        interpreter: None,
        platform: Some(Platform { platform: 1, minimum: 0x000e_0000, sdk: 0x000f_0000 }),
        incremental: false,
        debug: Debug::Lines,
        live: None,
        tags: 0,
        stack: None,
        windows_subsystem: false,
    }
}

fn executable(arch: Arch) -> Output {
    output(arch, OutputKind::Executable { entry: "main".to_owned() })
}

fn region(class: Class, addr: u64, size: u64, capacity: u64, prot: Prot) -> Region {
    Region { class, addr, capacity, size, prot, align: 8 }
}

/// The file as the core writes it: the regions at their offsets, then the writer's parts.
fn written(writer: &dyn Writer, placed: &Placed<'_>) -> Vec<u8> {
    let file = writer.file(placed).expect("the file");
    let mut bytes = vec![0u8; file.len as usize];
    for (index, region) in placed.regions.iter().enumerate() {
        if let Some(at) = writer.offset(placed, region) {
            let content = placed.bytes(index);
            bytes[at as usize..at as usize + content.len()].copy_from_slice(content);
        }
    }
    for (at, part) in &file.parts {
        bytes[*at as usize..*at as usize + part.len()].copy_from_slice(part);
    }
    bytes
}

fn uleb(bytes: &[u8], at: &mut usize) -> u64 {
    let (mut value, mut shift) = (0u64, 0);
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

fn sleb(bytes: &[u8], at: &mut usize) -> i64 {
    let (mut value, mut shift) = (0i64, 0);
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= i64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                value |= -1 << shift;
            }
            return value;
        }
    }
}

fn c_string(bytes: &[u8], at: &mut usize) -> Vec<u8> {
    let end = *at + bytes[*at..].iter().position(|it| *it == 0).unwrap();
    let name = bytes[*at..end].to_vec();
    *at = end + 1;
    name
}

/// What dyld does with rebase opcodes: the addresses it slides.
fn decode_rebases(stream: &[u8], segments: &[u64]) -> Vec<u64> {
    let (mut found, mut at, mut address) = (Vec::new(), 0, 0u64);
    while at < stream.len() {
        let byte = stream[at];
        at += 1;
        let immediate = u64::from(byte & macho::REBASE_IMMEDIATE_MASK);
        match byte & macho::REBASE_OPCODE_MASK {
            macho::REBASE_OPCODE_DONE => break,
            macho::REBASE_OPCODE_SET_TYPE_IMM => {
                assert_eq!(immediate, u64::from(macho::REBASE_TYPE_POINTER))
            }
            macho::REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                address = segments[immediate as usize] + uleb(stream, &mut at);
            }
            macho::REBASE_OPCODE_ADD_ADDR_ULEB => address += uleb(stream, &mut at),
            macho::REBASE_OPCODE_ADD_ADDR_IMM_SCALED => address += immediate * 8,
            macho::REBASE_OPCODE_DO_REBASE_IMM_TIMES => {
                for _ in 0..immediate {
                    found.push(address);
                    address += 8;
                }
            }
            macho::REBASE_OPCODE_DO_REBASE_ULEB_TIMES => {
                for _ in 0..uleb(stream, &mut at) {
                    found.push(address);
                    address += 8;
                }
            }
            other => panic!("rebase opcode {other:#x}"),
        }
    }
    found
}

/// What dyld does with bind opcodes: (address, ordinal, name, flags, addend).
fn decode_binds(stream: &[u8], segments: &[u64]) -> Vec<(u64, i64, Vec<u8>, u8, i64)> {
    let (mut found, mut at, mut address) = (Vec::new(), 0, 0u64);
    let (mut ordinal, mut name, mut flags, mut addend) = (0i64, Vec::new(), 0u8, 0i64);
    while at < stream.len() {
        let byte = stream[at];
        at += 1;
        let immediate = byte & macho::BIND_IMMEDIATE_MASK;
        match byte & macho::BIND_OPCODE_MASK {
            macho::BIND_OPCODE_DONE => break,
            macho::BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => ordinal = i64::from(immediate),
            macho::BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => ordinal = uleb(stream, &mut at) as i64,
            macho::BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => {
                ordinal = if immediate == 0 { 0 } else { i64::from((immediate | 0xf0) as i8) };
            }
            macho::BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
                flags = immediate;
                name = c_string(stream, &mut at);
            }
            macho::BIND_OPCODE_SET_TYPE_IMM => assert_eq!(immediate, macho::BIND_TYPE_POINTER),
            macho::BIND_OPCODE_SET_ADDEND_SLEB => addend = sleb(stream, &mut at),
            macho::BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                address = segments[immediate as usize] + uleb(stream, &mut at);
            }
            macho::BIND_OPCODE_ADD_ADDR_ULEB => address += uleb(stream, &mut at),
            macho::BIND_OPCODE_DO_BIND => {
                found.push((address, ordinal, name.clone(), flags, addend));
                address += 8;
            }
            other => panic!("bind opcode {other:#x}"),
        }
    }
    found.sort();
    found
}

/// Every export of a trie: (name, flags, address).
fn decode_trie(trie: &[u8]) -> Vec<(Vec<u8>, u64, u64)> {
    let mut found = Vec::new();
    let mut stack = vec![(0usize, Vec::new())];
    let mut seen = 0;
    while let Some((node, prefix)) = stack.pop() {
        seen += 1;
        assert!(seen < 10_000, "a cycle in the trie");
        let mut at = node;
        let info = uleb(trie, &mut at) as usize;
        let children_at = at + info;
        if info != 0 {
            let flags = uleb(trie, &mut at);
            let address = uleb(trie, &mut at);
            found.push((prefix.clone(), flags, address));
        }
        at = children_at;
        let children = trie[at];
        at += 1;
        for _ in 0..children {
            let label = c_string(trie, &mut at);
            let child = uleb(trie, &mut at) as usize;
            let mut name = prefix.clone();
            name.extend_from_slice(&label);
            stack.push((child, name));
        }
    }
    found.sort();
    found
}

/// A trie lookup as dyld does it, by walking edges.
fn trie_lookup(trie: &[u8], name: &[u8]) -> Option<u64> {
    let (mut node, mut rest) = (0usize, name);
    loop {
        let mut at = node;
        let info = uleb(trie, &mut at) as usize;
        if rest.is_empty() {
            if info == 0 {
                return None;
            }
            uleb(trie, &mut at);
            return Some(uleb(trie, &mut at));
        }
        at += info;
        let children = trie[at];
        at += 1;
        let mut next = None;
        for _ in 0..children {
            let label = c_string(trie, &mut at);
            let child = uleb(trie, &mut at) as usize;
            if rest.starts_with(&label) && next.is_none() {
                next = Some((child, label.len()));
            }
        }
        let (child, len) = next?;
        node = child;
        rest = &rest[len..];
    }
}

fn spans() -> Vec<linkedit::Span> {
    vec![
        linkedit::Span { name: "__PAGEZERO", start: 0, end: 0x1_0000_0000, writable: false },
        linkedit::Span {
            name: "__TEXT",
            start: 0x1_0000_0000,
            end: 0x1_0000_8000,
            writable: false,
        },
        linkedit::Span {
            name: "__DATA",
            start: 0x1_0000_8000,
            end: 0x1_0001_0000,
            writable: true,
        },
    ]
}

#[test]
fn names_cross_the_edge_both_ways() {
    assert_eq!(core_name(b"_main").unwrap(), "main");
    assert_eq!(core_name(b"__mh_execute_header").unwrap(), "_mh_execute_header");
    assert_eq!(core_name(b"section$start$__DATA$__x").unwrap(), "\x01section$start$__DATA$__x");
    for name in ["main", "_mh_execute_header", "\x01section$end$__TEXT$__text", "_ZN3foo3barE"] {
        assert_eq!(core_name(macho_name(name).as_bytes()).unwrap(), name);
    }
    assert!(core_name(&[b'_', 0xff]).is_err());
    let mut names = crate::model::Names::default();
    let (fits, long) = ("l".repeat(255), "l".repeat(256));
    for raw in [&b"_main"[..], b"__mh_execute_header", b"section$start$__DATA$__x", b"_", b"x", b"", fits.as_bytes(), long.as_bytes()] {
        let name = super::push_core_name(&mut names, raw).unwrap();
        assert_eq!(names.at(name), core_name(raw).unwrap());
    }
    assert!(super::push_core_name(&mut names, &[b'_', 0xff]).is_err());
}

#[test]
fn uleb_and_sleb() {
    for value in [0u64, 1, 127, 128, 300, 1 << 35, u64::MAX] {
        let mut out = Vec::new();
        linkedit::uleb(&mut out, value);
        let mut at = 0;
        assert_eq!(uleb(&out, &mut at), value);
        assert_eq!(at, out.len());
    }
    for value in [0i64, 1, -1, 63, 64, -64, -65, 1 << 40, -(1 << 40), i64::MIN, i64::MAX] {
        let mut out = Vec::new();
        linkedit::sleb(&mut out, value);
        let mut at = 0;
        assert_eq!(sleb(&out, &mut at), value, "{value}");
        assert_eq!(at, out.len());
    }
}

#[test]
fn trie_finds_every_export() {
    let names: Vec<String> = ["_a", "_ab", "_abc", "_abd", "_b", "__mh_execute_header", "_main"]
        .iter()
        .map(|it| it.to_string())
        .chain((0..300).map(|it| format!("_sym{it}_{}", it * 7919 % 1000)))
        .collect();
    let mut exports: Vec<(Vec<u8>, u64, u64)> = (names.iter().enumerate())
        .map(|(nth, it)| (it.clone().into_bytes(), 0, 0x4000 + nth as u64 * 0x1234))
        .collect();
    let want = {
        let mut want = exports.clone();
        want.sort();
        want
    };
    let trie = linkedit::trie(&mut exports).unwrap();
    assert_eq!(trie.len() % 8, 0);
    assert_eq!(decode_trie(&trie), want);
    for (name, _, addr) in &want {
        assert_eq!(trie_lookup(&trie, name), Some(*addr), "{}", String::from_utf8_lossy(name));
    }
    assert_eq!(trie_lookup(&trie, b"_ac"), None);
    assert_eq!(trie_lookup(&trie, b"_"), None);
    let mut twice = vec![(b"_a".to_vec(), 0, 1), (b"_a".to_vec(), 0, 2)];
    assert!(linkedit::trie(&mut twice).is_err());
    assert!(linkedit::trie(&mut Vec::<(Vec<u8>, u64, u64)>::new()).unwrap().is_empty());
}

#[test]
fn rebases_in_runs() {
    let addrs = [
        0x1_0000_8000,
        0x1_0000_8008,
        0x1_0000_8010,
        0x1_0000_9000,
        0x1_0000_a000,
        0x1_0000_a008,
    ];
    let mut many: Vec<u64> = (0..40).map(|it| 0x1_0000_c000 + it * 8).collect();
    many.extend(addrs);
    let stream = linkedit::rebases(&many, &spans()).unwrap();
    let starts: Vec<u64> = spans().iter().map(|it| it.start).collect();
    let mut want = many.clone();
    want.sort();
    assert_eq!(decode_rebases(&stream, &starts), want);
    let text = linkedit::rebases(&[0x1_0000_4000], &spans()).unwrap_err();
    assert!(text.contains("__TEXT"), "{text}");
    assert!(linkedit::rebases(&[0x1_0000_8000, 0x1_0000_8000], &spans()).is_err());
    assert!(linkedit::rebases(&[0x2_0000_0000], &spans()).is_err());
}

#[test]
fn binds_decode_to_the_imports() {
    let bind = |addr, ordinal, name: &'static str, weak, addend| linkedit::Bind {
        addr,
        ordinal,
        name: name.as_bytes(),
        weak,
        addend,
    };
    let mut binds = vec![
        bind(0x1_0000_8000, 1, "_malloc", false, 0),
        bind(0x1_0000_8008, 1, "_free", false, 0),
        bind(0x1_0000_8010, 20, "_far", false, 0),
        bind(0x1_0000_8018, -2, "__tlv_bootstrap", false, 0),
        bind(0x1_0000_8020, -2, "_maybe", true, 0),
        bind(0x1_0000_8028, 1, "_malloc", false, 16),
        bind(0x1_0000_8030, 1, "_malloc", false, -8),
        bind(0x1_0000_9000, 1, "_malloc", false, 0),
    ];
    let mut want: Vec<(u64, i64, Vec<u8>, u8, i64)> = (binds.iter())
        .map(|it| {
            let flags = if it.weak { macho::BIND_SYMBOL_FLAGS_WEAK_IMPORT } else { 0 };
            (it.addr, it.ordinal, it.name.to_vec(), flags, it.addend)
        })
        .collect();
    want.sort();
    let stream = linkedit::binds(&mut binds, &spans()).unwrap();
    let starts: Vec<u64> = spans().iter().map(|it| it.start).collect();
    assert_eq!(decode_binds(&stream, &starts), want);
    let mut text = vec![bind(0x1_0000_4000, 1, "_x", false, 0)];
    assert!(linkedit::binds(&mut text, &spans()).is_err());
    let mut twice =
        vec![bind(0x1_0000_8000, 1, "_x", false, 0), bind(0x1_0000_8000, 2, "_y", false, 0)];
    assert!(linkedit::binds(&mut twice, &spans()).is_err());
}

#[test]
fn function_starts_are_distances() {
    let starts = linkedit::function_starts(0x1_0000_0000, &[0x1_0000_4010, 0x1_0000_4000, 0x1_0000_4400]);
    let mut at = 0;
    assert_eq!(uleb(&starts, &mut at), 0x4000);
    assert_eq!(uleb(&starts, &mut at), 0x10);
    assert_eq!(uleb(&starts, &mut at), 0x3f0);
    assert_eq!(uleb(&starts, &mut at), 0);
    assert!(linkedit::function_starts(0x1_0000_0000, &[]).is_empty());
}

#[test]
fn signature_hashes_every_page() {
    let file: Vec<u8> = (0..3 * sign::PAGE + 100).map(|it| (it * 31 % 251) as u8).collect();
    let mut signature = Vec::new();
    sign::signature(&sign::View::whole(&file), "hello", 0, 0x4000, true, None, &mut signature);
    assert_eq!(signature.len() as u64, sign::len("hello", file.len() as u64));
    check_signature(&signature, &file, "hello");
}

/// The signature says the hash of every page of `file` and the identifier.
fn check_signature(signature: &[u8], file: &[u8], identifier: &str) {
    let be = |at: usize| u32::from_be_bytes(signature[at..at + 4].try_into().unwrap());
    assert_eq!(be(0), 0xfade_0cc0);
    assert_eq!(be(8), 1);
    assert_eq!(be(12), 0);
    let directory = be(16) as usize;
    let cd = |at: usize| be(directory + at);
    assert_eq!(cd(0), 0xfade_0c02);
    assert!(cd(8) >= 0x0002_0400);
    assert_eq!(cd(12) & 0x2, 0x2, "ad hoc");
    let (hashes, ident, special, slots, limit) =
        (cd(16) as usize, cd(20) as usize, cd(24), cd(28) as usize, cd(32) as usize);
    assert_eq!(special, 0);
    assert_eq!(limit, file.len());
    assert_eq!(&signature[directory + 36..directory + 40], &[32, 2, 0, 12]);
    let name = &signature[directory + ident..];
    let name = &name[..name.iter().position(|it| *it == 0).unwrap()];
    assert_eq!(name, identifier.as_bytes());
    assert_eq!(slots, file.len().div_ceil(4096));
    for (nth, page) in file.chunks(4096).enumerate() {
        let at = directory + hashes + nth * 32;
        assert_eq!(&signature[at..at + 32], &sha256::digest(page), "page {nth}");
    }
}

#[test]
fn wasm_and_no_platform_are_errors() {
    assert!(super::writer(&executable(Arch::Wasm32)).is_err());
    let mut output = executable(Arch::Aarch64);
    output.platform = None;
    assert!(super::writer(&output).is_err());
}

#[test]
fn plans() {
    let arm = super::writer(&executable(Arch::Aarch64)).unwrap().plan();
    assert_eq!((arm.base, arm.page, arm.tls_descriptors), (0x1_0000_0000, 0x4000, true));
    let x86 = super::writer(&executable(Arch::X86_64)).unwrap().plan();
    assert_eq!((x86.base, x86.page), (0x1_0000_0000, 0x1000));
    let shared = OutputKind::Shared { name: "@rpath/libx.dylib".to_owned(), exports: Vec::new() };
    assert_eq!(super::writer(&output(Arch::Aarch64, shared)).unwrap().plan().base, 0);
}

#[test]
fn classes_in_ld64_order() {
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let linkme = Class::Named { name: "__DATA,__linkme_X".to_owned(), write: true, exec: false };
    let mut classes = vec![
        Class::Bss,
        Class::Got,
        linkme.clone(),
        Class::EhFrame,
        Class::InitArray,
        Class::Tbss,
        Class::Text,
        Class::Synthetic(super::UNWIND_INFO),
        Class::Rodata,
        Class::Data,
        Class::Tdata,
        Class::Plt,
        super::compact_unwind(),
        Class::Table,
        Class::Patch,
        Class::ThreadVars,
        Class::RelRo,
    ];
    writer.order(&mut classes);
    assert_eq!(
        classes,
        vec![
            Class::Text,
            Class::Patch,
            Class::Plt,
            Class::Rodata,
            Class::Synthetic(super::UNWIND_INFO),
            Class::EhFrame,
            Class::Got,
            Class::InitArray,
            Class::RelRo,
            super::compact_unwind(),
            Class::Table,
            Class::Data,
            linkme,
            Class::ThreadVars,
            Class::Tdata,
            Class::Tbss,
            Class::Bss,
        ]
    );
    assert_eq!(writer.prot(&Class::Rodata), Prot::RX);
    assert_eq!(writer.prot(&Class::InitArray), Prot::R);
    assert_eq!(writer.prot(&Class::Got), Prot::R);
    assert_eq!(writer.prot(&Class::ThreadVars), Prot::RW);
}

#[test]
fn link_sections_and_specials() {
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let linkme = Class::Named { name: "__DATA,__linkme_X".to_owned(), write: true, exec: false };
    assert_eq!(
        writer.section("__DATA,__linkme_X,regular,no_dead_strip", false, false, false).unwrap(),
        (linkme.clone(), u32::MAX)
    );
    let section = |end| Some(Special::Section { name: "__DATA,__linkme_X".to_owned(), end });
    assert_eq!(writer.special("\x01section$start$__DATA$__linkme_X"), section(false));
    assert_eq!(writer.special("\x01section$end$__DATA$__linkme_X"), section(true));
    assert_eq!(writer.section("__DATA,__mod_init_func,mod_init_funcs", false, false, false).unwrap().0, Class::InitArray);
    assert_eq!(writer.section("__TEXT,__text,regular,pure_instructions", false, true, false).unwrap().0, Class::Text);
    assert_eq!(writer.section("__DATA,__data", true, false, false).unwrap().0, Class::Data);
    assert_eq!(writer.section("__DATA,__thread_data", true, false, true).unwrap().0, Class::Tdata);
    assert_eq!(writer.section("__DATA,__thread_bss,thread_local_zerofill", true, false, true).unwrap().0, Class::Tbss);
    for bad in ["nocomma", "__FOO,__bar", "__DATA,__x,bogus", "__DATA,__x,regular,bogus", "__DATA,__seventeen_bytes_", "__DATA,__nl,non_lazy_symbol_pointers"] {
        assert!(writer.section(bad, true, false, false).is_err(), "{bad}");
    }
    assert_eq!(writer.special("_mh_execute_header"), Some(Special::ImageBase));
    assert_eq!(writer.special("_mh_dylib_header"), None);
    assert_eq!(writer.special("__dso_handle"), Some(Special::ImageBase));
    assert_eq!(writer.special("__blaze_live"), Some(Special::LiveDescriptor));
    assert_eq!(writer.special("\x01section$start$__DATA$__mod_init_func"), Some(Special::Start(Class::InitArray)));
    assert_eq!(writer.special("\x01segment$start$__TEXT"), Some(Special::ImageBase));
    let regions = hello(executable(Arch::Aarch64)).regions;
    let segment = |name: &str| match writer.special(name) {
        Some(Special::Placed(at)) => at(&regions),
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(segment("\x01segment$end$__TEXT"), GOT);
    assert_eq!(segment("\x01segment$start$__DATA_CONST"), GOT);
    assert_eq!(segment("\x01segment$end$__DATA_CONST"), DATA);
    assert_eq!(segment("\x01segment$start$__DATA"), DATA);
    assert_eq!(segment("\x01segment$end$__DATA"), BASE + 0x10000);
    let no_data_const: Vec<Region> =
        regions.iter().filter(|it| it.prot != Prot::R).cloned().collect();
    let at = |name| match writer.special(name) {
        Some(Special::Placed(at)) => at(&no_data_const),
        other => panic!("{other:?}"),
    };
    assert_eq!((at("segment$start$__DATA_CONST"), at("segment$end$__DATA_CONST")), (GOT, GOT));
    assert_eq!(writer.special("\x01segment$start$__LINKEDIT"), None);
    assert_eq!(writer.special("malloc"), None);
    assert_eq!(writer.special("\x01section$start$__NOPE$__x"), None);

    assert_eq!(super::classify("__LD", "__compact_unwind", 0x0200_0000).unwrap(), SectionKind::Unwind(super::compact_unwind()));
    assert_eq!(super::classify("__TEXT", "__eh_frame", 0x6800_000b).unwrap(), SectionKind::EhFrame);
    assert_eq!(super::classify("__DWARF", "__debug_info", 0x0200_0000).unwrap(), SectionKind::Dropped);
    assert_eq!(super::classify("__TEXT", "__cstring", macho::S_CSTRING_LITERALS).unwrap(), SectionKind::Alloc(Class::Rodata, u32::MAX));
    assert_eq!(super::classify("__DATA", "__common", macho::S_ZEROFILL).unwrap(), SectionKind::Alloc(Class::Bss, u32::MAX));
    assert_eq!(super::classify("__DATA", "__thread_vars", macho::S_THREAD_LOCAL_VARIABLES).unwrap(), SectionKind::Alloc(Class::ThreadVars, u32::MAX));
    assert_eq!(super::classify("__DATA", "__const", macho::S_REGULAR).unwrap(), SectionKind::Alloc(Class::RelRo, u32::MAX));
    assert_eq!(super::classify("__DATA_CONST", "__const", macho::S_REGULAR).unwrap(), SectionKind::Alloc(Class::RelRo, u32::MAX));
}

/// What a test image is made of, owned, so a `Placed` can borrow it.
struct Parts {
    output: Output,
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

impl Parts {
    fn slices(&self) -> Vec<&[u8]> {
        self.contents.iter().map(Vec::as_slice).collect()
    }

    fn placed<'a>(&'a self, contents: &'a [&'a [u8]]) -> Placed<'a> {
        Placed {
            output: &self.output,
            name: "hello",
            plan: &self.plan,
            regions: &self.regions,
            contents,
            libraries: &self.libraries,
            imports: &self.imports,
            got: &self.got,
            rebase: &self.rebase,
            binds: &self.binds,
            exports: &self.exports,
            functions: &self.functions,
            entry: self.entry,
            init: None,
            fini: None,
            tls: self.tls,
            id: [7; 16],
            debug: &self.debug,
            symbol: &no_symbol,
        }
    }
}

fn no_symbol(_: &str) -> Option<u64> {
    None
}

const BASE: u64 = 0x1_0000_0000;
const TEXT: u64 = BASE + 0x4000;
const GOT: u64 = BASE + 0x8000;
const INIT: u64 = GOT + 16;
const DATA: u64 = BASE + 0xc000;
const VARS: u64 = DATA + 16;

fn import(name: &str, library: Option<u32>, weak: bool) -> Import {
    Import { name: name.to_owned(), library, version: None, kind: SymbolKind::Function, weak, wasm: None }
}

/// An arm64 executable: code, a GOT of an import and a local, an initializer, data with a
/// pointer slid and one bound, a thread-local variable's descriptor, thread-locals and zeroed
/// data.
fn hello(output: Output) -> Parts {
    let writer = super::writer(&output).unwrap();
    let plan = writer.plan();
    let unwind = TEXT + 0x100;
    let regions = vec![
        region(Class::Text, TEXT, 8, 0x100, Prot::RX),
        region(Class::Synthetic(super::UNWIND_INFO), unwind, 12, 64, Prot::RX),
        region(Class::Got, GOT, 16, 16, Prot::R),
        region(Class::InitArray, INIT, 8, 8, Prot::R),
        region(Class::Data, DATA, 16, 16, Prot::RW),
        region(Class::ThreadVars, VARS, 24, 24, Prot::RW),
        region(Class::Tdata, VARS + 24, 8, 8, Prot::RW),
        region(Class::Tbss, VARS + 32, 8, 8, Prot::RW),
        region(Class::Bss, VARS + 40, 32, 32, Prot::RW),
    ];
    let contents = vec![
        vec![0x1f, 0x20, 0x03, 0xd5, 0xc0, 0x03, 0x5f, 0xd6],
        vec![1; 12],
        [0u64, TEXT].iter().flat_map(|it| it.to_le_bytes()).collect(),
        TEXT.to_le_bytes().to_vec(),
        [TEXT, 0].iter().flat_map(|it| it.to_le_bytes()).collect(),
        vec![0; 24],
        vec![9; 8],
        Vec::new(),
        Vec::new(),
    ];
    Parts {
        output,
        plan,
        regions,
        contents,
        libraries: vec![
            Library { name: "/usr/lib/libSystem.B.dylib".to_owned(), as_needed: false },
            Library { name: "/usr/lib/libunused.dylib".to_owned(), as_needed: true },
        ],
        imports: vec![
            import("free", Some(0), false),
            import("maybe", None, true),
            import("_tlv_bootstrap", Some(0), false),
        ],
        got: vec![
            GotEntry { addr: GOT, holds: Holds::Import(0) },
            GotEntry { addr: GOT + 8, holds: Holds::Local(TEXT) },
        ],
        rebase: vec![INIT, GOT + 8, DATA],
        binds: vec![
            Bind { addr: DATA + 8, import: 1, addend: 0 },
            Bind { addr: VARS, import: 2, addend: 0 },
        ],
        exports: vec![Export {
            name: "main".to_owned(),
            addr: TEXT,
            size: 8,
            kind: SymbolKind::Function,
        }],
        functions: vec![
            Function { addr: TEXT, size: 4, fde: None, windows: None, name: Some("main".to_owned()) },
            Function {
                addr: TEXT + 4,
                size: 4,
                fde: None,
                windows: None,
                name: Some("_ZN5hello6helper17h0123456789abcdefE".to_owned()),
            },
        ],
        entry: Some(TEXT),
        tls: Some(Tls { addr: VARS + 24, init: 8, size: 16, align: 8 }),
        debug: vec![(".debug_line", vec![1, 2, 3, 4]), (".debug_str_offsets", vec![5; 8])],
    }
}

/// What the load commands of a written image say.
#[derive(Default)]
struct Read {
    flags: u32,
    filetype: u32,
    segments: Vec<(String, u64, u64, u64, u64)>,
    sections: Vec<(String, String, u64, u64, u32)>,
    dylibs: Vec<(u32, String)>,
    id: Option<String>,
    dylinker: Option<String>,
    uuid: Option<[u8; 16]>,
    build: Option<(u32, u32, u32)>,
    entry: Option<u64>,
    info: Option<[(u32, u32); 5]>,
    symtab: Option<(u32, u32, u32, u32)>,
    dysymtab: Option<[u32; 6]>,
    indirect_at: Option<u32>,
    starts: Option<(u32, u32)>,
    in_code: Option<(u32, u32)>,
    signature: Option<(u32, u32)>,
}

fn name16(bytes: &[u8; 16]) -> String {
    let end = bytes.iter().position(|it| *it == 0).unwrap_or(16);
    String::from_utf8(bytes[..end].to_vec()).unwrap()
}

fn read(file: &[u8]) -> Read {
    use object::read::macho::LoadCommandVariant as V;
    let header = macho::MachHeader64::<LE>::parse(file, 0).expect("a Mach-O header");
    let mut read = Read { flags: header.flags(LE), filetype: header.filetype(LE), ..Read::default() };
    let mut commands = header.load_commands(LE, file, 0).unwrap();
    while let Some(command) = commands.next().unwrap() {
        match command.variant().unwrap() {
            V::Segment64(segment, data) => {
                read.segments.push((
                    name16(segment.segname()),
                    segment.vmaddr(LE),
                    segment.vmsize(LE),
                    segment.fileoff(LE),
                    segment.filesize(LE),
                ));
                for section in segment.sections(LE, data).unwrap() {
                    read.sections.push((
                        name16(section.segname()),
                        name16(section.sectname()),
                        section.addr(LE),
                        section.size(LE),
                        section.flags(LE),
                    ));
                }
            }
            V::Dylib(dylib) => {
                let name = command.string(LE, dylib.dylib.name).unwrap();
                read.dylibs.push((command.cmd(), String::from_utf8(name.to_vec()).unwrap()));
            }
            V::IdDylib(dylib) => {
                let name = command.string(LE, dylib.dylib.name).unwrap();
                read.id = Some(String::from_utf8(name.to_vec()).unwrap());
            }
            V::LoadDylinker(dylinker) => {
                let name = command.string(LE, dylinker.name).unwrap();
                read.dylinker = Some(String::from_utf8(name.to_vec()).unwrap());
            }
            V::Uuid(uuid) => read.uuid = Some(uuid.uuid),
            V::BuildVersion(build) => {
                read.build = Some((build.platform.get(LE), build.minos.get(LE), build.sdk.get(LE)))
            }
            V::EntryPoint(entry) => read.entry = Some(entry.entryoff.get(LE)),
            V::DyldInfo(info) => {
                read.info = Some([
                    (info.rebase_off.get(LE), info.rebase_size.get(LE)),
                    (info.bind_off.get(LE), info.bind_size.get(LE)),
                    (info.weak_bind_off.get(LE), info.weak_bind_size.get(LE)),
                    (info.lazy_bind_off.get(LE), info.lazy_bind_size.get(LE)),
                    (info.export_off.get(LE), info.export_size.get(LE)),
                ])
            }
            V::Symtab(symtab) => {
                read.symtab = Some((
                    symtab.symoff.get(LE),
                    symtab.nsyms.get(LE),
                    symtab.stroff.get(LE),
                    symtab.strsize.get(LE),
                ))
            }
            V::Dysymtab(it) => {
                read.dysymtab = Some([
                    it.nlocalsym.get(LE),
                    it.iextdefsym.get(LE),
                    it.nextdefsym.get(LE),
                    it.iundefsym.get(LE),
                    it.nundefsym.get(LE),
                    it.nindirectsyms.get(LE),
                ]);
                read.indirect_at = Some(it.indirectsymoff.get(LE));
            }
            V::LinkeditData(data) => {
                let it = Some((data.dataoff.get(LE), data.datasize.get(LE)));
                match command.cmd() {
                    macho::LC_FUNCTION_STARTS => read.starts = it,
                    macho::LC_DATA_IN_CODE => read.in_code = it,
                    macho::LC_CODE_SIGNATURE => read.signature = it,
                    other => panic!("linkedit data command {other:#x}"),
                }
            }
            V::SourceVersion(_) => {}
            _ => panic!("a load command {:#x}", command.cmd()),
        }
    }
    read
}

fn range(file: &[u8], (at, len): (u32, u32)) -> &[u8] {
    &file[at as usize..(at + len) as usize]
}

/// (name, type, sect, desc, value) of every symbol.
fn symbols(file: &[u8], read: &Read) -> Vec<(String, u8, u8, u16, u64)> {
    let (at, count, strings, _) = read.symtab.unwrap();
    (0..count as usize)
        .map(|nth| {
            let entry = &file[at as usize + nth * 16..][..16];
            let strx = u32::from_le_bytes(entry[..4].try_into().unwrap()) as usize;
            let mut name_at = strings as usize + strx;
            let name = String::from_utf8(c_string(file, &mut name_at)).unwrap();
            let desc = u16::from_le_bytes(entry[6..8].try_into().unwrap());
            let value = u64::from_le_bytes(entry[8..16].try_into().unwrap());
            (name, entry[4], entry[5], desc, value)
        })
        .collect()
}

#[test]
fn executable_reads_back() {
    let parts = hello(executable(Arch::Aarch64));
    let writer = super::writer(&parts.output).unwrap();
    let slices = parts.slices();
    let placed = parts.placed(&slices);
    let file = written(&*writer, &placed);
    let read = read(&file);

    assert_eq!(read.filetype, macho::MH_EXECUTE);
    for flag in [macho::MH_PIE, macho::MH_DYLDLINK, macho::MH_TWOLEVEL, macho::MH_HAS_TLV_DESCRIPTORS] {
        assert_ne!(read.flags & flag, 0, "{flag:#x}");
    }
    let header = macho::MachHeader64::<LE>::parse(&*file, 0).unwrap();
    assert_eq!(header.cputype(LE), macho::CPU_TYPE_ARM64);

    let names: Vec<&str> = read.segments.iter().map(|it| it.0.as_str()).collect();
    assert_eq!(names, ["__PAGEZERO", "__TEXT", "__DATA_CONST", "__DATA", "__DWARF", "__LINKEDIT"]);
    assert_eq!(read.segments[0], ("__PAGEZERO".to_owned(), 0, BASE, 0, 0));
    assert_eq!(read.segments[1].1, BASE);
    assert_eq!(read.segments[1].3, 0);
    assert_eq!(read.segments[2].1, GOT);
    assert_eq!(read.segments[3].1, DATA);
    // `__DATA` has bytes up to its thread-locals and the rest zeroed.
    let data = &read.segments[3];
    assert!(data.4 < data.2 || data.4 == data.2);
    assert_eq!(data.3, DATA - BASE);
    let linkedit = read.segments.last().unwrap();
    assert_eq!(linkedit.3 % 0x4000, 0);
    assert_eq!(linkedit.3 + linkedit.4, file.len() as u64);

    let sections: Vec<(&str, &str, u64, u64)> = (read.sections.iter())
        .map(|it| (it.0.as_str(), it.1.as_str(), it.2, it.3))
        .collect();
    assert_eq!(
        sections[..9],
        [
            ("__TEXT", "__text", TEXT, 8),
            ("__TEXT", "__unwind_info", TEXT + 0x100, 12),
            ("__DATA_CONST", "__got", GOT, 16),
            ("__DATA_CONST", "__mod_init_func", INIT, 8),
            ("__DATA", "__data", DATA, 16),
            ("__DATA", "__thread_vars", VARS, 24),
            ("__DATA", "__thread_data", VARS + 24, 8),
            ("__DATA", "__thread_bss", VARS + 32, 8),
            ("__DATA", "__bss", VARS + 40, 32),
        ]
    );
    let flags: Vec<u32> = read.sections.iter().map(|it| it.4 & macho::SECTION_TYPE).collect();
    assert_eq!(
        flags[..9],
        [
            macho::S_REGULAR,
            macho::S_REGULAR,
            macho::S_NON_LAZY_SYMBOL_POINTERS,
            macho::S_MOD_INIT_FUNC_POINTERS,
            macho::S_REGULAR,
            macho::S_THREAD_LOCAL_VARIABLES,
            macho::S_THREAD_LOCAL_REGULAR,
            macho::S_THREAD_LOCAL_ZEROFILL,
            macho::S_ZEROFILL,
        ]
    );
    assert_eq!(&sections[9..], [("__DWARF", "__debug_line", linkedit.1 - 0x4000, 4), (
        "__DWARF",
        "__debug_str_offs",
        linkedit.1 - 0x4000 + 4,
        8
    )]);
    // Region bytes are where the sections say.
    assert_eq!(&file[0x4000..0x4008], &parts.contents[0][..]);
    assert_eq!(&file[(DATA - BASE) as usize..][..8], &TEXT.to_le_bytes());

    assert_eq!(read.dylinker.as_deref(), Some("/usr/lib/dyld"));
    assert_eq!(read.dylibs, [(macho::LC_LOAD_DYLIB, "/usr/lib/libSystem.B.dylib".to_owned())]);
    assert_eq!(read.uuid, Some([7; 16]));
    assert_eq!(read.build, Some((1, 0x000e_0000, 0x000f_0000)));
    assert_eq!(read.entry, Some(0x4000));

    let info = read.info.unwrap();
    let segments: Vec<u64> = read.segments.iter().map(|it| it.1).collect();
    let mut rebases = decode_rebases(range(&file, info[0]), &segments);
    rebases.sort();
    assert_eq!(rebases, [GOT + 8, INIT, DATA]);
    let binds = decode_binds(range(&file, info[1]), &segments);
    assert_eq!(binds, [
        (GOT, 1, b"_free".to_vec(), 0, 0),
        (DATA + 8, -2, b"_maybe".to_vec(), macho::BIND_SYMBOL_FLAGS_WEAK_IMPORT, 0),
        (VARS, 1, b"__tlv_bootstrap".to_vec(), 0, 0),
    ]);
    assert_eq!((info[2].1, info[3].1), (0, 0));
    let trie = range(&file, info[4]);
    assert_eq!(trie_lookup(trie, b"_main"), Some(0x4000));
    assert_eq!(trie_lookup(trie, b"__mh_execute_header"), Some(0));
    assert_eq!(decode_trie(trie).len(), 2);

    let symbols = symbols(&file, &read);
    assert_eq!(read.dysymtab.unwrap(), [1, 1, 2, 3, 3, 2]);
    assert_eq!(symbols[0].0, "__ZN5hello6helper17h0123456789abcdefE");
    assert_eq!((symbols[0].1, symbols[0].2, symbols[0].4), (macho::N_SECT, 1, TEXT + 4));
    assert_eq!(symbols[1].0, "__mh_execute_header");
    assert_eq!(symbols[1].3, macho::REFERENCED_DYNAMICALLY);
    assert_eq!((symbols[2].0.as_str(), symbols[2].1), ("_main", macho::N_SECT | macho::N_EXT));
    assert_eq!((symbols[3].0.as_str(), symbols[3].3 >> 8), ("__tlv_bootstrap", 1));
    assert_eq!((symbols[4].0.as_str(), symbols[4].3 >> 8), ("_free", 1));
    assert_eq!(symbols[5].0, "_maybe");
    assert_eq!(symbols[5].3 >> 8, u16::from(macho::DYNAMIC_LOOKUP_ORDINAL));
    assert_ne!(symbols[5].3 & macho::N_WEAK_REF, 0);
    let indirect = range(&file, (read.indirect_at.unwrap(), 8));
    assert_eq!(indirect, [4, 0, 0, 0, 0, 0, 0, 0x80].as_slice(), "_free, then a local");

    let starts = range(&file, read.starts.unwrap());
    let mut at = 0;
    assert_eq!((uleb(starts, &mut at), uleb(starts, &mut at), uleb(starts, &mut at)), (0x4000, 4, 0));
    assert_eq!(read.in_code.unwrap().1, 0);

    let (signed, len) = read.signature.unwrap();
    assert_eq!(signed % 16, 0);
    assert_eq!((signed + len) as usize, file.len());
    check_signature(range(&file, (signed, len)), &file[..signed as usize], "hello");

    // `object` reads it as the executable it is.
    use object::{Object as _, ObjectSection as _};
    let parsed = object::read::macho::MachOFile64::<LE>::parse(&*file).unwrap();
    assert_eq!(parsed.entry(), TEXT - BASE);
    assert_eq!(parsed.mach_uuid().unwrap(), Some([7; 16]));
    let text = parsed.section_by_name("__text").unwrap();
    assert_eq!(text.data().unwrap(), &parts.contents[0][..]);
}

#[test]
fn a_dylib_reads_back() {
    let shared = OutputKind::Shared { name: "@rpath/libhello.dylib".to_owned(), exports: Vec::new() };
    let mut parts = hello(output(Arch::X86_64, shared));
    let delta = BASE;
    for region in &mut parts.regions {
        region.addr -= delta;
    }
    for cell in &mut parts.got {
        cell.addr -= delta;
        if let Holds::Local(addr) = &mut cell.holds {
            *addr -= delta;
        }
    }
    for addr in &mut parts.rebase {
        *addr -= delta;
    }
    for bind in &mut parts.binds {
        bind.addr -= delta;
    }
    for function in &mut parts.functions {
        function.addr -= delta;
    }
    parts.exports[0].addr -= delta;
    parts.entry = None;
    parts.tls = parts.tls.map(|it| Tls { addr: it.addr - delta, ..it });
    parts.debug.clear();
    let writer = super::writer(&parts.output).unwrap();
    let slices = parts.slices();
    let placed = parts.placed(&slices);
    let file = written(&*writer, &placed);
    let read = read(&file);
    assert_eq!(read.filetype, macho::MH_DYLIB);
    assert_eq!(read.flags & macho::MH_PIE, 0);
    assert_eq!(read.id.as_deref(), Some("@rpath/libhello.dylib"));
    assert_eq!(read.dylinker, None);
    assert_eq!(read.entry, None);
    assert_eq!(read.segments[0].0, "__TEXT");
    assert_eq!(read.segments[0].1, 0);
    let header = macho::MachHeader64::<LE>::parse(&*file, 0).unwrap();
    assert_eq!(header.cputype(LE), macho::CPU_TYPE_X86_64);
    let trie = range(&file, read.info.unwrap()[4]);
    assert_eq!(decode_trie(trie), [(b"_main".to_vec(), 0, 0x4000)]);
    let (signed, len) = read.signature.unwrap();
    check_signature(range(&file, (signed, len)), &file[..signed as usize], "hello");
}

#[test]
fn weak_dylib_when_every_import_is_weak() {
    let mut parts = hello(executable(Arch::Aarch64));
    parts.imports[0].weak = true;
    let writer = super::writer(&parts.output).unwrap();
    let file = written(&*writer, &parts.placed(&parts.slices()));
    assert_eq!(read(&file).dylibs, [(macho::LC_LOAD_DYLIB, "/usr/lib/libSystem.B.dylib".to_owned())]);
    parts.imports[2].weak = true;
    let file = written(&*writer, &parts.placed(&parts.slices()));
    assert_eq!(read(&file).dylibs, [(macho::LC_LOAD_WEAK_DYLIB, "/usr/lib/libSystem.B.dylib".to_owned())]);
}

#[test]
fn errors_name_what() {
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();

    let mut parts = hello(executable(Arch::Aarch64));
    parts.rebase.push(TEXT);
    let what = writer.file(&parts.placed(&parts.slices())).err().unwrap().what;
    assert!(what.contains("__TEXT"), "{what}");

    let mut parts = hello(executable(Arch::Aarch64));
    parts.got[1].holds = Holds::TpOff(8);
    assert!(writer.file(&parts.placed(&parts.slices())).is_err());

    let mut parts = hello(executable(Arch::Aarch64));
    parts.entry = None;
    assert!(writer.file(&parts.placed(&parts.slices())).is_err());

    let mut parts = hello(executable(Arch::Aarch64));
    parts.contents[7] = vec![1];
    assert!(writer.file(&parts.placed(&parts.slices())).is_err(), "bytes in a zeroed region");

    let mut parts = hello(executable(Arch::Aarch64));
    parts.libraries = (0..300)
        .map(|it| Library { name: format!("/usr/lib/lib{it}.dylib"), as_needed: false })
        .collect();
    assert!(writer.file(&parts.placed(&parts.slices())).is_err(), "too many libraries");

    assert!(writer.apply(1, &mut [0; 4], 0, 0, 0).is_err());
}

#[test]
fn reads_universal_slices() {
    // A universal file of two slices: the arm64 one is taken.
    let mut file = Vec::new();
    for field in [macho::FAT_MAGIC, 2] {
        file.extend_from_slice(&field.to_be_bytes());
    }
    for (cpu, offset) in [(macho::CPU_TYPE_X86_64, 64u32), (macho::CPU_TYPE_ARM64, 72)] {
        for field in [cpu, 0, offset, 8, 3] {
            file.extend_from_slice(&field.to_be_bytes());
        }
    }
    file.resize(64, 0);
    file.extend_from_slice(b"x86slice");
    file.extend_from_slice(b"armslice");
    let machine = super::MachO {
        output: executable(Arch::Aarch64),
        cpu: macho::CPU_TYPE_ARM64,
        subtype: 0,
        page: 0x4000,
        platform: Platform { platform: 1, minimum: 0, sdk: 0 },
        cache: None,
    };
    assert_eq!(machine.thin("fat", &file).unwrap(), (72, &b"armslice"[..]));
    let ppc = super::MachO { cpu: 18, ..machine };
    assert!(ppc.thin("fat", &file).is_err());
    assert_eq!(ppc.thin("thin", b"\xcf\xfa\xed\xfe....").unwrap().0, 0);
}

/// An arm64 object with code, data and C strings in atoms, a local and an undefined symbol,
/// and pointers to both.
fn an_object() -> Vec<u8> {
    use object::write::{Object as Builder, Relocation, StandardSection, Symbol as Def, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind as Type,
        SymbolScope,
    };
    let mut b = Builder::new(BinaryFormat::MachO, Architecture::Aarch64, Endianness::Little);
    b.set_subsections_via_symbols();
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0x1f, 0x20, 0x03, 0xd5].repeat(8), 4);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 16], 8);
    let strings = b.section_id(StandardSection::CString);
    b.append_section_data(strings, b"hello\0world\0", 1);
    let mut symbol = |name: &str, value: u64, size: u64, kind, scope, section| {
        b.add_symbol(Def {
            name: name.into(),
            value,
            size,
            kind,
            scope,
            weak: false,
            section,
            flags: SymbolFlags::None,
        })
    };
    let f = symbol("_f", 0, 16, Type::Text, SymbolScope::Linkage, SymbolSection::Section(text));
    symbol("_g", 16, 16, Type::Text, SymbolScope::Compilation, SymbolSection::Section(text));
    symbol("_d", 0, 16, Type::Data, SymbolScope::Linkage, SymbolSection::Section(data));
    let ext = symbol("_ext", 0, 0, Type::Unknown, SymbolScope::Dynamic, SymbolSection::Undefined);
    for (at, target) in [(0, f), (8, ext)] {
        let flags =
            RelocationFlags::MachO { r_type: macho::ARM64_RELOC_UNSIGNED, r_pcrel: false, r_length: 3 };
        b.add_relocation(data, Relocation { offset: at, symbol: target, addend: 0, flags }).unwrap();
    }
    b.write().unwrap()
}

fn native_object(read: Result<crate::model::NativeFile, String>) -> crate::model::Object {
    match read {
        Ok(crate::model::NativeFile::Object(object)) => object,
        Ok(_) => panic!("read as something other than an object"),
        Err(error) => panic!("{error}"),
    }
}

/// `a`, read from `a_input`, and `b`, from `b_input`, say the same through every accessor.
fn same_object(
    a: &crate::model::Object,
    a_input: &[u8],
    b: &crate::model::Object,
    b_input: &[u8],
) {
    assert_eq!(a.sections.len(), b.sections.len());
    for (x, y) in a.sections.iter().zip(&b.sections) {
        assert_eq!(a.name(x.name), b.name(y.name));
        assert_eq!((&x.what, x.align, x.retain), (&y.what, y.align, y.retain));
        assert_eq!(x.data.len(), y.data.len());
        assert_eq!(x.data.bytes(a_input).unwrap(), y.data.bytes(b_input).unwrap(), "{}", a.name(x.name));
        assert_eq!(a.section_fixups(x), b.section_fixups(y));
    }
    assert_eq!(a.symbols, b.symbols);
    assert_eq!(a.names, b.names);
    assert_eq!(a.libraries, b.libraries);
}

#[test]
fn objects_are_read_in_place_wherever_they_start() {
    use crate::model::Data;
    let bytes = an_object();
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let thin = native_object(writer.read("x.o", &bytes, &crate::NativeKind::Object));
    let mut held = 0;
    for section in &thin.sections {
        match &section.data {
            Data::Input(range) => {
                assert!(range.start <= range.end && range.end <= bytes.len() as u64);
                held += range.end - range.start;
            }
            Data::Zero(_) => {}
            Data::Bytes(_) => panic!("{}: copied, not a range of the input", thin.name(section.name)),
        }
    }
    assert_eq!(held, 32 + 16 + 12, "every byte of the code, the data and the strings");
    // An archive's member starts at any byte: every start of a word reads the same.
    let mut buffer = vec![0u8; bytes.len() + 8];
    for start in 0..8 {
        buffer[start..start + bytes.len()].copy_from_slice(&bytes);
        let input = &buffer[start..start + bytes.len()];
        let read = native_object(writer.read("x.o", input, &crate::NativeKind::Object));
        same_object(&thin, &bytes, &read, input);
        for (x, y) in thin.sections.iter().zip(&read.sections) {
            assert_eq!(x.data, y.data, "ranges of the slice read, wherever it is");
        }
    }
}

#[test]
fn universal_objects_name_bytes_of_the_whole_file() {
    use crate::model::Data;
    let bytes = an_object();
    let at = 0x4000u32;
    let mut fat = Vec::new();
    for field in [macho::FAT_MAGIC, 2] {
        fat.extend_from_slice(&field.to_be_bytes());
    }
    for (cpu, offset, size, align) in
        [(macho::CPU_TYPE_X86_64, 0x1000, 8, 12), (macho::CPU_TYPE_ARM64, at, bytes.len() as u32, 14)]
    {
        for field in [cpu, 0, offset, size, align] {
            fat.extend_from_slice(&field.to_be_bytes());
        }
    }
    fat.resize(0x1000, 0);
    fat.extend_from_slice(b"x86slice");
    fat.resize(at as usize, 0);
    fat.extend_from_slice(&bytes);

    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let thin = native_object(writer.read("x.o", &bytes, &crate::NativeKind::Object));
    let read = native_object(writer.read("fat.o", &fat, &crate::NativeKind::Object));
    same_object(&thin, &bytes, &read, &fat);
    for (x, y) in thin.sections.iter().zip(&read.sections) {
        match (&x.data, &y.data) {
            (Data::Input(x), Data::Input(y)) => {
                assert_eq!(*y, x.start + u64::from(at)..x.end + u64::from(at));
            }
            (x, y) => assert_eq!(x, y),
        }
    }
}

/// The trie as it was first built, one name at a time with an edge label copied per edge:
/// the reference the one-pass builder must match byte for byte.
fn trie_by_insertion(exports: &mut [(Vec<u8>, u64, u64)]) -> Vec<u8> {
    #[derive(Default)]
    struct Node {
        edges: Vec<(Vec<u8>, usize)>,
        terminal: Option<(u64, u64)>,
        offset: u64,
    }
    let uleb_len = |value: u64| u64::from((64 - value.leading_zeros()).max(1).div_ceil(7));
    let info_len = |node: &Node| node.terminal.map_or(0, |(f, a)| uleb_len(f) + uleb_len(a));
    exports.sort_unstable();
    let mut nodes = vec![Node::default()];
    for (name, flags, addr) in exports.iter() {
        let (mut node, mut name) = (0, &name[..]);
        loop {
            if name.is_empty() {
                nodes[node].terminal = Some((*flags, *addr));
                break;
            }
            let Some(edge) = nodes[node].edges.iter().position(|it| it.0[0] == name[0]) else {
                nodes.push(Node { terminal: Some((*flags, *addr)), ..Node::default() });
                let child = nodes.len() - 1;
                nodes[node].edges.push((name.to_vec(), child));
                break;
            };
            let (label, child) = nodes[node].edges[edge].clone();
            let common = label.iter().zip(name).take_while(|(a, b)| a == b).count();
            if common < label.len() {
                nodes.push(Node { edges: vec![(label[common..].to_vec(), child)], ..Node::default() });
                let middle = nodes.len() - 1;
                nodes[node].edges[edge] = (label[..common].to_vec(), middle);
                node = middle;
            } else {
                node = child;
            }
            name = &name[common..];
        }
    }
    for node in &mut nodes {
        node.edges.sort_unstable();
    }
    let mut order = Vec::new();
    let mut stack = vec![0];
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(nodes[node].edges.iter().rev().map(|it| it.1));
    }
    loop {
        let (mut offset, mut moved) = (0, false);
        for &node in &order {
            moved |= nodes[node].offset != offset;
            nodes[node].offset = offset;
            let info = info_len(&nodes[node]);
            let mut len = uleb_len(info) + info + 1;
            for (label, child) in &nodes[node].edges {
                len += label.len() as u64 + 1 + uleb_len(nodes[*child].offset);
            }
            offset += len;
        }
        if !moved {
            break;
        }
    }
    let mut out = Vec::new();
    for &node in &order {
        let node = &nodes[node];
        linkedit::uleb(&mut out, info_len(node));
        if let Some((flags, addr)) = node.terminal {
            linkedit::uleb(&mut out, flags);
            linkedit::uleb(&mut out, addr);
        }
        out.push(node.edges.len() as u8);
        for (label, child) in &node.edges {
            out.extend_from_slice(label);
            out.push(0);
            linkedit::uleb(&mut out, nodes[*child].offset);
        }
    }
    out.resize(out.len().next_multiple_of(8), 0);
    out
}

#[test]
fn trie_is_the_insertion_trie() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..200 {
        // Names over a small alphabet, so prefixes are shared, split and ended often.
        let count = 1 + (next() % 300) as usize;
        let mut names = std::collections::BTreeSet::new();
        while names.len() < count {
            let len = 1 + (next() % 12) as usize;
            names.insert((0..len).map(|_| b"_abc$Z"[(next() % 6) as usize]).collect::<Vec<u8>>());
        }
        let mut exports: Vec<(Vec<u8>, u64, u64)> = (names.into_iter())
            .map(|name| (name, next() % 3, next() % (1 << (next() % 40))))
            .collect();
        // Out of order, as the builder must sort them itself.
        exports.reverse();
        let want = trie_by_insertion(&mut exports.clone());
        assert_eq!(linkedit::trie(&mut exports).unwrap(), want, "round {round}");
        let mut borrowed: Vec<(&[u8], u64, u64)> =
            exports.iter().map(|it| (&it.0[..], it.1, it.2)).collect();
        assert_eq!(linkedit::trie(&mut borrowed).unwrap(), want, "round {round}, borrowed");
    }
}

#[test]
fn names_sort_as_their_mach_o_spelling() {
    let names = ["main", "\x01main", "\x01_main", "_main", "\x01", "a", "\x01a", "\x01`", "Z", "\x01Z"];
    for a in names {
        for b in names {
            let want = macho_name(a).into_bytes().cmp(&macho_name(b).into_bytes());
            assert_eq!(super::macho_order(a, b), want, "{a:?} {b:?}");
        }
    }
}

#[test]
fn rebases_and_starts_sorted_or_not_are_the_same() {
    let sorted: Vec<u64> = (0..50).map(|it| 0x1_0000_8000 + it * 8 + (it / 7) * 0x40).collect();
    let mut shuffled = sorted.clone();
    shuffled.reverse();
    shuffled.swap(3, 30);
    assert_eq!(linkedit::rebases(&sorted, &spans()).unwrap(), linkedit::rebases(&shuffled, &spans()).unwrap());
    assert_eq!(
        linkedit::function_starts(0x1_0000_0000, &sorted),
        linkedit::function_starts(0x1_0000_0000, &shuffled)
    );
    let mut twice = sorted.clone();
    twice.push(sorted[10]);
    assert_eq!(linkedit::function_starts(0x1_0000_0000, &twice), linkedit::function_starts(0x1_0000_0000, &sorted));
}

/// A writer that keeps the last signature's pages writes the same file as one that doesn't,
/// link after link, as the code and the data change.
#[test]
fn the_signing_cache_writes_the_same_file() {
    let mut incremental = executable(Arch::Aarch64);
    incremental.incremental = true;
    let cached = super::writer(&incremental).unwrap();
    let mut parts = hello(executable(Arch::Aarch64));
    for round in 0..4u8 {
        match round {
            1 => parts.contents[0][0] ^= 0x10,
            2 => parts.contents[4][3] = 0x42,
            3 => parts.exports[0].name = "main2".to_owned(),
            _ => {}
        }
        let fresh = super::writer(&parts.output).unwrap();
        let slices = parts.slices();
        let placed = parts.placed(&slices);
        let file = written(&*cached, &placed);
        assert_eq!(file, written(&*fresh, &placed), "round {round}");
        let read = read(&file);
        let (at, len) = read.signature.unwrap();
        check_signature(range(&file, (at, len)), &file[..at as usize], "hello");
    }
}
