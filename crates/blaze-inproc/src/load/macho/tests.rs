//! A Mach-O image built here in blaze-link's layout, fixed up by the safe model into plain
//! buffers and loaded for real into this process, which must give the same bytes; opcode
//! tables encoded from random fixups, which must decode to the same fixups; what the loader
//! refuses; and mutated images, which must be errors or loads, never panics.

use std::ffi::CStr;
use std::sync::Mutex;

use super::fixup::{self, Bind, Export, Imports, Memory, Segment};
use super::parse::MachO;
use super::{Dl, UnwindSections, find_unwind_sections, functions, link, tlv};
use crate::channel::Channel;
use crate::sys::{self, Rights};
use crate::{Access, Error};

/// macOS AArch64's page, which blaze-link aligns segments to.
const P: u64 = 0x4000;
const FIRST: u64 = 0x1_0000_0000;
const TEXT_FN: u64 = 0x1000;
const EH_FRAME: u64 = 0x2000;
const GOT: u64 = P;
const INIT: u64 = P + 0x20;
const DATA: u64 = 2 * P;
const TLV_VARS: u64 = 2 * P + 0x20;
const TLV_DATA: u64 = 2 * P + 0x50;
const TLV_BSS: u64 = 2 * P + 0x58;
const BSS: u64 = 3 * P;
const LINKEDIT: u64 = 4 * P;
const IMAGE: u64 = 5 * P;
const TEMPLATE: u64 = 0x1122_3344_5566_7788;

/// `mov w0, #42` and `ret`.
const FORTY_TWO: [u8; 8] = [0x40, 0x05, 0x80, 0x52, 0xc0, 0x03, 0x5f, 0xd6];

const SEG_TEXT: u8 = 1;
const SEG_DATA_CONST: u8 = 2;
const SEG_DATA: u8 = 3;

const S_REGULAR: u32 = 0;
const S_ZEROFILL: u32 = 1;
const S_MOD_INIT_FUNC_POINTERS: u32 = 9;
const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;

/// The tests that load into this process: one at a time.
static LOADING: Mutex<()> = Mutex::new(());

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Rebase opcodes for `list` (segment, offset), using runs where the offsets have one stride.
fn encode_rebases(list: &[(u8, u64)]) -> Vec<u8> {
    let mut sorted = list.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = vec![0x10 | 1];
    let mut at: Option<(u8, u64)> = None;
    let mut index = 0;
    while index < sorted.len() {
        let (segment, offset) = sorted[index];
        match at {
            Some((current, place)) if current == segment && offset >= place => {
                let delta = offset - place;
                if delta != 0 && delta.is_multiple_of(8) && delta / 8 < 16 {
                    out.push(0x40 | (delta / 8) as u8);
                } else if delta != 0 {
                    out.push(0x30);
                    uleb(&mut out, delta);
                }
            }
            _ => {
                out.push(0x20 | segment);
                uleb(&mut out, offset);
            }
        }
        let same = |at: usize| sorted.get(at).is_some_and(|it| it.0 == segment);
        let stride = if same(index + 1) { sorted[index + 1].1 - offset } else { 0 };
        let mut end = index + 1;
        while same(end) && sorted[end].1 - sorted[end - 1].1 == stride {
            end += 1;
        }
        let count = (end - index) as u64;
        if count == 1 {
            if same(end) {
                out.push(0x70);
                uleb(&mut out, sorted[end].1 - offset - 8);
                at = Some((segment, sorted[end].1));
            } else {
                out.push(0x51);
                at = Some((segment, offset + 8));
            }
        } else if stride == 8 {
            if count < 16 {
                out.push(0x50 | count as u8);
            } else {
                out.push(0x60);
                uleb(&mut out, count);
            }
            at = Some((segment, offset + 8 * count));
        } else {
            out.push(0x80);
            uleb(&mut out, count);
            uleb(&mut out, stride - 8);
            at = Some((segment, offset + stride * count));
        }
        index = end;
    }
    out.push(0);
    out
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct BindSpec {
    segment: u8,
    offset: u64,
    ordinal: i64,
    name: Vec<u8>,
    weak: bool,
    addend: i64,
}

fn bind(segment: u8, offset: u64, ordinal: i64, name: &str) -> BindSpec {
    BindSpec { segment, offset, ordinal, name: name.as_bytes().to_vec(), weak: false, addend: 0 }
}

/// Bind opcodes for `list` in order; runs of one symbol at one stride as one opcode.
fn encode_binds(list: &[BindSpec], lazy: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < list.len() {
        let spec = &list[index];
        if spec.ordinal <= 0 {
            out.push(0x30 | (spec.ordinal as u8 & 0x0f));
        } else if spec.ordinal < 16 {
            out.push(0x10 | spec.ordinal as u8);
        } else {
            out.push(0x20);
            uleb(&mut out, spec.ordinal as u64);
        }
        out.push(0x40 | u8::from(spec.weak));
        out.extend_from_slice(&spec.name);
        out.push(0);
        out.push(0x51);
        out.push(0x60);
        sleb(&mut out, spec.addend);
        out.push(0x70 | spec.segment);
        uleb(&mut out, spec.offset);
        let alike = |at: usize| {
            !lazy
                && list.get(at).is_some_and(|it| {
                    BindSpec { offset: spec.offset, ..it.clone() } == *spec && it.offset > list[at - 1].offset
                })
        };
        let stride = if alike(index + 1) { list[index + 1].offset - spec.offset } else { 0 };
        let mut end = index + 1;
        while alike(end) && list[end].offset - list[end - 1].offset == stride {
            end += 1;
        }
        if end - index > 1 && stride >= 8 {
            out.push(0xc0);
            uleb(&mut out, (end - index) as u64);
            uleb(&mut out, stride - 8);
        } else {
            out.push(0x90);
            end = index + 1;
        }
        if lazy {
            out.push(0);
        }
        index = end;
    }
    if !lazy {
        out.push(0);
    }
    out
}

/// An export trie of a root with one edge per name.
fn encode_trie(exports: &[(&[u8], u64, u64)]) -> Vec<u8> {
    let mut out = vec![0, exports.len() as u8];
    let mut slots = Vec::with_capacity(exports.len());
    for (name, _, _) in exports {
        out.extend_from_slice(name);
        out.push(0);
        slots.push(out.len());
        out.extend_from_slice(&[0x80, 0x80, 0x00]);
    }
    for (index, &(_, flags, value)) in exports.iter().enumerate() {
        let at = out.len();
        let mut info = Vec::new();
        uleb(&mut info, flags);
        uleb(&mut info, value);
        out.push(info.len() as u8);
        out.extend_from_slice(&info);
        out.push(0);
        let slot = slots[index];
        out[slot] = (at & 0x7f) as u8 | 0x80;
        out[slot + 1] = ((at >> 7) & 0x7f) as u8 | 0x80;
        out[slot + 2] = (at >> 14) as u8;
    }
    out
}

/// What the test image is made of; each refusal test changes one thing.
#[derive(Clone)]
struct Spec {
    subtype: u32,
    flags: u32,
    linkedit_fileoff: u64,
    data_prot: u32,
    data_sections: Vec<([u8; 16], u64, u64, u32)>,
    extra: Vec<(u32, Vec<u8>)>,
    rebases: Vec<(u8, u64)>,
    binds: Vec<BindSpec>,
    lazy: Vec<BindSpec>,
    weak_bind: Vec<u8>,
    /// A segment of zero pages after `__LINKEDIT`, past the file.
    bss_after: bool,
}

fn name16(name: &str) -> [u8; 16] {
    let mut out = [0; 16];
    out[..name.len()].copy_from_slice(name.as_bytes());
    out
}

impl Default for Spec {
    fn default() -> Spec {
        let mut weak = bind(SEG_DATA_CONST, GOT + 16 - P, 1, "_blaze_no_such_symbol");
        weak.weak = true;
        Spec {
            subtype: 0,
            flags: 0x0020_0000 | 0x4 | 0x80 | 0x0080_0000,
            linkedit_fileoff: LINKEDIT,
            data_prot: 3,
            data_sections: vec![
                (name16("__data"), DATA, 0x20, S_REGULAR),
                (name16("__thread_vars"), TLV_VARS, 0x30, S_THREAD_LOCAL_VARIABLES),
                (name16("__thread_data"), TLV_DATA, 8, S_THREAD_LOCAL_REGULAR),
                (name16("__thread_bss"), TLV_BSS, 8, S_THREAD_LOCAL_ZEROFILL),
                (name16("__bss"), BSS, 0x40, S_ZEROFILL),
            ],
            extra: Vec::new(),
            rebases: vec![(SEG_DATA, 0), (SEG_DATA, 8), (SEG_DATA_CONST, INIT - P)],
            binds: vec![
                bind(SEG_DATA_CONST, GOT - P, 1, "_getpid"),
                bind(SEG_DATA_CONST, GOT + 8 - P, 0, "_answer"),
                weak,
                bind(SEG_DATA, TLV_VARS - DATA, 1, "__tlv_bootstrap"),
                bind(SEG_DATA, TLV_VARS + 24 - DATA, 1, "__tlv_bootstrap"),
            ],
            lazy: vec![bind(SEG_DATA, 0x10, 1, "_getppid")],
            weak_bind: Vec::new(),
            bss_after: false,
        }
    }
}

struct Commands {
    bytes: Vec<u8>,
    count: u32,
}

impl Commands {
    fn push(&mut self, cmd: u32, body: &[u8]) {
        let size = (8 + body.len()).next_multiple_of(8);
        self.bytes.extend_from_slice(&cmd.to_le_bytes());
        self.bytes.extend_from_slice(&(size as u32).to_le_bytes());
        self.bytes.extend_from_slice(body);
        self.bytes.resize(self.bytes.len() + size - 8 - body.len(), 0);
        self.count += 1;
    }

    #[allow(clippy::too_many_arguments)]
    fn segment(
        &mut self,
        name: &str,
        vmaddr: u64,
        vmsize: u64,
        fileoff: u64,
        filesize: u64,
        prot: u32,
        flags: u32,
        sections: &[([u8; 16], u64, u64, u32)],
    ) {
        let mut body = Vec::new();
        body.extend_from_slice(&name16(name));
        for value in [vmaddr, vmsize, fileoff, filesize] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        for value in [prot, prot, sections.len() as u32, flags] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        for &(section, offset, size, kind) in sections {
            body.extend_from_slice(&section);
            body.extend_from_slice(&name16(name));
            body.extend_from_slice(&(FIRST + offset).to_le_bytes());
            body.extend_from_slice(&size.to_le_bytes());
            let file = if matches!(kind, S_ZEROFILL | S_THREAD_LOCAL_ZEROFILL) { 0 } else { offset as u32 };
            for value in [file, 3, 0, 0, kind, 0, 0, 0] {
                body.extend_from_slice(&value.to_le_bytes());
            }
        }
        self.push(0x19, &body);
    }
}

fn put(bytes: &mut [u8], at: u64, value: u64) {
    bytes[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
}

fn get(bytes: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(bytes[at as usize..at as usize + 8].try_into().unwrap())
}

fn build(spec: &Spec) -> Vec<u8> {
    let mut linkedit = Vec::new();
    let mut table = |bytes: &[u8]| -> (u32, u32) {
        let at = LINKEDIT + linkedit.len() as u64;
        linkedit.extend_from_slice(bytes);
        linkedit.resize(linkedit.len().next_multiple_of(8), 0);
        (at as u32, bytes.len() as u32)
    };
    let rebase = table(&encode_rebases(&spec.rebases));
    let binds = table(&encode_binds(&spec.binds, false));
    let weak = table(&spec.weak_bind);
    let lazy = table(&encode_binds(&spec.lazy, true));
    let trie = table(&encode_trie(&[(b"_answer", 0, TEXT_FN), (b"_tlv", 1, TLV_VARS)]));
    let linkedit_len = (linkedit.len() as u64).next_multiple_of(8);

    let mut commands = Commands { bytes: Vec::new(), count: 0 };
    commands.segment("__PAGEZERO", 0, FIRST, 0, 0, 0, 0, &[]);
    commands.segment(
        "__TEXT",
        FIRST,
        P,
        0,
        P,
        5,
        0,
        &[(name16("__text"), TEXT_FN, 16, 0x8000_0400), (name16("__eh_frame"), EH_FRAME, 4, 0x6800_000b)],
    );
    commands.segment(
        "__DATA_CONST",
        FIRST + P,
        P,
        P,
        P,
        3,
        0x10,
        &[(name16("__got"), GOT, 24, 6), (name16("__mod_init_func"), INIT, 8, S_MOD_INIT_FUNC_POINTERS)],
    );
    commands.segment("__DATA", FIRST + DATA, 2 * P, DATA, P, spec.data_prot, 0, &spec.data_sections);
    commands.segment("__LINKEDIT", FIRST + LINKEDIT, P, spec.linkedit_fileoff, linkedit_len, 1, 0, &[]);
    if spec.bss_after {
        commands.segment("__BSS", FIRST + IMAGE, P, 0, 0, 3, 0, &[]);
    }
    let mut info = Vec::new();
    for (offset, size) in [rebase, binds, weak, lazy, trie] {
        info.extend_from_slice(&offset.to_le_bytes());
        info.extend_from_slice(&size.to_le_bytes());
    }
    commands.push(0x8000_0022, &info);
    let mut dylib = Vec::new();
    for value in [24u32, 2, 0x0001_0000, 0x0001_0000] {
        dylib.extend_from_slice(&value.to_le_bytes());
    }
    dylib.extend_from_slice(b"/usr/lib/libSystem.B.dylib\0");
    commands.push(0xc, &dylib);
    let mut main = Vec::new();
    main.extend_from_slice(&TEXT_FN.to_le_bytes());
    main.extend_from_slice(&0u64.to_le_bytes());
    commands.push(0x8000_0028, &main);
    for (cmd, body) in &spec.extra {
        commands.push(*cmd, body);
    }

    let mut image = vec![0u8; IMAGE as usize];
    for (index, value) in [0xfeed_facfu32, 0x0100_000c, spec.subtype, 2, commands.count, commands.bytes.len() as u32, spec.flags, 0]
        .into_iter()
        .enumerate()
    {
        image[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    image[32..32 + commands.bytes.len()].copy_from_slice(&commands.bytes);
    image[TEXT_FN as usize..TEXT_FN as usize + 8].copy_from_slice(&FORTY_TWO);
    put(&mut image, DATA, FIRST + TEXT_FN);
    put(&mut image, DATA + 8, FIRST + DATA + 8);
    put(&mut image, INIT, FIRST + TEXT_FN);
    put(&mut image, TLV_VARS + 16, 0);
    put(&mut image, TLV_VARS + 24 + 16, 8);
    put(&mut image, TLV_DATA, TEMPLATE);
    image[LINKEDIT as usize..LINKEDIT as usize + linkedit.len()].copy_from_slice(&linkedit);
    image
}

/// The image's writable segments as vectors, the file's bytes and zeros after.
fn model(macho: &MachO<'_>, image: &[u8]) -> Vec<(u64, Vec<u8>)> {
    macho
        .segments
        .iter()
        .filter(|it| it.writable())
        .map(|segment| {
            let mut bytes = vec![0u8; segment.vmsize.next_multiple_of(P) as usize];
            let start = (segment.vmaddr - macho.first) as usize;
            bytes[..segment.filesize as usize].copy_from_slice(&image[start..start + segment.filesize as usize]);
            (segment.vmaddr, bytes)
        })
        .collect()
}

fn memory(model: &mut [(u64, Vec<u8>)]) -> Memory<'_> {
    Memory::new(model.iter_mut().map(|(vmaddr, bytes)| Segment { vmaddr: *vmaddr, bytes }).collect())
}

/// Every symbol at an address made from its name and library, but `_missing`.
struct Fake;

impl Imports for Fake {
    fn library(&mut self, ordinal: usize, name: &[u8]) -> Result<Option<u64>, Error> {
        Ok((name != b"_missing").then(|| 0x7000_0000 + ordinal as u64 * 0x10_0000 + name.len() as u64))
    }

    fn flat(&mut self, name: &[u8]) -> Result<Option<u64>, Error> {
        Ok((name != b"_missing").then(|| 0x6000_0000 + name.len() as u64))
    }
}

#[test]
fn the_image_parses_as_laid_out() {
    let image = build(&Spec::default());
    let macho = MachO::parse(&image, P).unwrap();
    assert_eq!(macho.first, FIRST);
    assert_eq!(macho.span, IMAGE);
    assert_eq!(macho.entry, TEXT_FN);
    assert_eq!(macho.dylibs.len(), 1);
    assert_eq!(macho.dylibs[0].path.to_bytes(), b"/usr/lib/libSystem.B.dylib");
    let mapped: Vec<String> = macho.segments.iter().filter(|it| it.mapped).map(|it| it.name()).collect();
    assert_eq!(mapped, ["__TEXT", "__DATA_CONST", "__DATA", "__LINKEDIT"]);
    assert_eq!(fixup::export(macho.exports, b"_answer").unwrap(), Some(Export::Offset(TEXT_FN)));
    assert_eq!(fixup::export(macho.exports, b"_tlv").unwrap(), Some(Export::Offset(TLV_VARS)));
    assert_eq!(fixup::export(macho.exports, b"_answe").unwrap(), None);
    assert_eq!(fixup::export(macho.exports, b"_answers").unwrap(), None);
}

#[test]
fn the_model_fixes_up_as_dyld_would() {
    let image = build(&Spec::default());
    let macho = MachO::parse(&image, P).unwrap();
    let base = 0x2_0000_0000u64;
    let mut segments = model(&macho, &image);
    let mut memory = memory(&mut segments);
    fixup::apply(&macho, &mut memory, base, &mut Fake).unwrap();
    assert_eq!(memory.read(FIRST + DATA), Some(base + TEXT_FN));
    assert_eq!(memory.read(FIRST + DATA + 8), Some(base + DATA + 8));
    assert_eq!(memory.read(FIRST + INIT), Some(base + TEXT_FN));
    assert_eq!(memory.read(FIRST + GOT), Some(0x7000_0000 + 0x10_0000 + 7));
    assert_eq!(memory.read(FIRST + GOT + 8), Some(base + TEXT_FN));
    assert_eq!(memory.read(FIRST + GOT + 16), Some(0x7000_0000 + 0x10_0000 + 21));
    assert_eq!(memory.read(FIRST + DATA + 0x10), Some(0x7000_0000 + 0x10_0000 + 8));
    assert_eq!(functions(&macho, &memory, base, true).unwrap(), [base + TEXT_FN]);
    assert!(functions(&macho, &memory, base, false).unwrap().is_empty());
}

#[test]
fn rebases_round_trip() {
    let image = build(&Spec::default());
    let macho = MachO::parse(&image, P).unwrap();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..300 {
        let mut list = Vec::new();
        for _ in 0..rng.below(60) {
            let (segment, words) = if rng.below(3) == 0 { (SEG_DATA_CONST, P / 8) } else { (SEG_DATA, 2 * P / 8) };
            let start = rng.below(words);
            let stride = [1, 1, 2, 3, 17][rng.below(5) as usize];
            for step in 0..1 + rng.below(20) {
                let word = start + step * stride;
                if word < words {
                    list.push((segment, word * 8));
                }
            }
        }
        let opcodes = encode_rebases(&list);
        let mut got = Vec::new();
        let mut budget = fixup::budget(&macho);
        fixup::rebases(&opcodes, &macho.segments, &mut budget, |addr| {
            got.push(addr);
            Ok(())
        })
        .unwrap();
        let mut want: Vec<u64> = list.iter().map(|&(segment, offset)| macho.segments[segment as usize].vmaddr + offset).collect();
        want.sort_unstable();
        want.dedup();
        assert_eq!(got, want);
    }
}

#[test]
fn binds_round_trip_and_write_what_they_name() {
    let image = build(&Spec::default());
    let macho = MachO::parse(&image, P).unwrap();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let names = ["_a", "_bb", "_answer", "_missing"];
    for _ in 0..300 {
        let mut list: Vec<BindSpec> = Vec::new();
        let mut taken = std::collections::BTreeSet::new();
        for _ in 0..rng.below(30) {
            let ordinal = [1, 1, 0, -2][rng.below(4) as usize];
            let name = if ordinal == 0 { "_answer" } else { names[rng.below(4) as usize] };
            let spec = BindSpec {
                segment: SEG_DATA,
                offset: rng.below(2 * P / 8) * 8,
                ordinal,
                name: name.as_bytes().to_vec(),
                weak: rng.below(2) == 0,
                addend: [0, 8, -16][rng.below(3) as usize],
            };
            let stride = 8 * (1 + rng.below(3));
            for step in 0..1 + rng.below(4) {
                let offset = spec.offset + step * stride;
                if offset < 2 * P && taken.insert(offset) {
                    list.push(BindSpec { offset, ..spec.clone() });
                }
            }
        }
        list.retain(|it| it.name != b"_missing" || it.weak);
        let lazy = rng.below(2) == 0;
        let opcodes = encode_binds(&list, lazy);
        let mut got: Vec<BindSpec> = Vec::new();
        let mut budget = fixup::budget(&macho);
        fixup::binds(&opcodes, &macho.segments, lazy, &mut budget, |bind: &Bind<'_>| {
            got.push(BindSpec {
                segment: SEG_DATA,
                offset: bind.addr - (FIRST + DATA),
                ordinal: bind.ordinal,
                name: bind.name.to_vec(),
                weak: bind.weak,
                addend: bind.addend,
            });
            Ok(())
        })
        .unwrap();
        assert_eq!(got, list);

        let spec = Spec { binds: list.clone(), lazy: Vec::new(), rebases: Vec::new(), ..Spec::default() };
        let image = build(&spec);
        let macho = MachO::parse(&image, P).unwrap();
        let mut segments = model(&macho, &image);
        let mut memory = memory(&mut segments);
        let base = FIRST + 0x10_0000;
        fixup::apply(&macho, &mut memory, base, &mut Fake).unwrap();
        for spec in &list {
            let value = match (spec.ordinal, spec.name.as_slice()) {
                (_, b"_missing") => 0,
                (1, name) => 0x7000_0000 + 0x10_0000 + name.len() as u64,
                (_, b"_answer") => base + TEXT_FN,
                (_, name) => 0x6000_0000 + name.len() as u64,
            };
            assert_eq!(memory.read(FIRST + DATA + spec.offset), Some(value.wrapping_add(spec.addend as u64)), "{spec:?}");
        }
        drop(memory);
    }
}

#[test]
fn the_trie_finds_names_through_shared_prefixes() {
    #[rustfmt::skip]
    let trie: Vec<u8> = vec![
        // 0: the root, no terminal, one child "_" at 5.
        0, 1, b'_', 0, 5,
        // 5: "_", no terminal, children "answer" at 25, "tlv" at 30, "abs" at 36.
        0, 3, b'a', b'n', b's', b'w', b'e', b'r', 0, 25, b't', b'l', b'v', 0, 30, b'a', b'b', b's', 0, 36,
        // 25: regular, 0x1000.
        3, 0x00, 0x80, 0x20, 0,
        // 30: thread-local, 0x8020.
        4, 0x01, 0xa0, 0x80, 0x02, 0,
        // 36: absolute, 7.
        2, 0x02, 0x07, 0,
    ];
    assert_eq!(fixup::export(&trie, b"_answer").unwrap(), Some(Export::Offset(0x1000)));
    assert_eq!(fixup::export(&trie, b"_tlv").unwrap(), Some(Export::Offset(0x8020)));
    assert_eq!(fixup::export(&trie, b"_abs").unwrap(), Some(Export::Absolute(7)));
    assert_eq!(fixup::export(&trie, b"_").unwrap(), None);
    assert_eq!(fixup::export(&trie, b"_ab").unwrap(), None);
    assert_eq!(fixup::export(&trie, b"answer").unwrap(), None);
    let mut broken = trie.clone();
    broken[4] = 100;
    assert!(fixup::export(&broken, b"_answer").is_err());
    let mut reexport = trie;
    reexport[26] = 0x08;
    assert!(fixup::export(&reexport, b"_answer").unwrap_err().what.contains("re-exported"));
}

fn refused(spec: &Spec, needle: &str) {
    let image = build(spec);
    let error = match MachO::parse(&image, P) {
        Ok(macho) => {
            let mut segments = model(&macho, &image);
            let mut memory = memory(&mut segments);
            fixup::apply(&macho, &mut memory, FIRST, &mut Fake).unwrap_err()
        }
        Err(error) => error,
    };
    assert!(error.what.contains(needle), "{error} lacks {needle:?}");
}

#[test]
fn what_dyld_alone_does_is_refused() {
    refused(&Spec { linkedit_fileoff: LINKEDIT - P, ..Spec::default() }, "__LINKEDIT");
    refused(&Spec { subtype: 2, ..Spec::default() }, "arm64e");
    refused(&Spec { flags: 0x4, ..Spec::default() }, "MH_PIE");
    refused(&Spec { data_prot: 7, ..Spec::default() }, "writable and executable");
    refused(&Spec { extra: vec![(0x8000_0034, vec![0; 8])], ..Spec::default() }, "chained fixups");
    refused(&Spec { extra: vec![(0x8000_0099, vec![0; 8])], ..Spec::default() }, "0x80000099");
    let mut objc = Spec::default();
    objc.data_sections.push((name16("__objc_selrefs"), DATA + 0x60, 8, S_REGULAR));
    refused(&objc, "Objective-C");
    refused(&Spec { weak_bind: vec![0x11, 0], ..Spec::default() }, "weak bind");
    refused(&Spec { rebases: vec![(SEG_TEXT, 0)], ..Spec::default() }, "not writable");
    refused(&Spec { rebases: vec![(SEG_DATA, 2 * P)], ..Spec::default() }, "past segment");
    refused(&Spec { binds: vec![bind(SEG_DATA, 0, 1, "_missing")], ..Spec::default() }, "_missing");
    refused(&Spec { binds: vec![bind(SEG_DATA, 0, 2, "_getpid")], ..Spec::default() }, "library 2 of 1");
}

/// A rebase table that loops in place: a count of 2^40 at a skip that wraps back to the same
/// word.
#[test]
fn a_rebase_loop_is_cut_off_by_the_budget() {
    let image = build(&Spec::default());
    let macho = MachO::parse(&image, P).unwrap();
    let mut opcodes = vec![0x11, 0x20 | SEG_DATA, 0, 0x80];
    uleb(&mut opcodes, 1 << 40);
    uleb(&mut opcodes, u64::MAX - 7);
    opcodes.push(0);
    let mut budget = fixup::budget(&macho);
    let error = fixup::rebases(&opcodes, &macho.segments, &mut budget, |_| Ok(())).unwrap_err();
    assert!(error.what.contains("more fixups"), "{error}");
}

#[test]
fn mutated_images_are_errors_or_loads() {
    let image = build(&Spec::default());
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    for round in 0..3000 {
        let mut bytes = image.clone();
        for _ in 0..1 + rng.below(6) {
            let at = if rng.below(4) == 0 { LINKEDIT + rng.below(0x200) } else { rng.below(0x400) };
            bytes[at as usize] = match rng.below(3) {
                0 => rng.next() as u8,
                1 => bytes[at as usize] ^ (1 << rng.below(8)),
                _ => [0, 0xff, 0x80, 0x7f][rng.below(4) as usize],
            };
        }
        let len = if round % 10 == 0 { rng.below(IMAGE) as usize } else { bytes.len() };
        let Ok(macho) = MachO::parse(&bytes[..len], P) else { continue };
        if macho.segments.iter().filter(|it| it.writable()).map(|it| it.vmsize).sum::<u64>() > 64 << 20 {
            continue;
        }
        let mut segments = model(&macho, &bytes[..len]);
        let mut memory = memory(&mut segments);
        if fixup::apply(&macho, &mut memory, FIRST + 0x4000_0000, &mut Fake).is_ok() {
            let _ = functions(&macho, &memory, FIRST + 0x4000_0000, true);
            let _ = functions(&macho, &memory, FIRST + 0x4000_0000, false);
        }
        let _ = fixup::export(macho.exports, b"_answer");
        let _ = fixup::export(macho.exports, b"_tlv");
    }
}

fn pair() -> (Channel, Channel) {
    let mut fds = [0; 2];
    // SAFETY: `fds` is two writable ints for the new sockets.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) }, 0);
    (Channel::from_raw(i64::from(fds[0])).unwrap(), Channel::from_raw(i64::from(fds[1])).unwrap())
}

fn symbol(name: &CStr) -> u64 {
    // SAFETY: a NUL-terminated name looked up in the global scope.
    unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) as u64 }
}

/// The word at `at` in this process, which the test mapped readable.
fn word(at: u64) -> u64 {
    // SAFETY: every caller names a word of the image `link` mapped readable in this process,
    // for the rest of it.
    unsafe { (at as *const u64).read() }
}

#[test]
fn a_loaded_image_is_the_model_mapped() {
    let _one = LOADING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(sys::page(), P, "this test lays the image out for 16 KiB pages");
    let image = build(&Spec::default());
    let object = sys::Object::create("macho-test", IMAGE + P).unwrap();
    let mut view = object.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..IMAGE as usize).unwrap().copy_from_slice(&image);
    view.bytes_mut(IMAGE as usize..IMAGE as usize + 8).unwrap().copy_from_slice(&FORTY_TWO);
    view.publish(IMAGE as usize..IMAGE as usize + 8);
    let granted = sys::granted(&object, Rights::Code).unwrap();
    let (mut channel, _other) = pair();

    let loaded = link(&granted, &mut channel, &image, IMAGE, Some(LOADED_AT)).unwrap();
    let base = loaded.base;
    assert_eq!(base, LOADED_AT);
    assert_eq!(loaded.entry, base + TEXT_FN);
    assert_eq!(loaded.initializers, [base + TEXT_FN]);

    // SAFETY: the image's `__text` holds `mov w0, #42; ret`, mapped executable at `base +
    // TEXT_FN`, and the arena the same at `base + IMAGE`.
    let (text, arena): (extern "C" fn() -> u32, extern "C" fn() -> u32) =
        unsafe { (std::mem::transmute((base + TEXT_FN) as usize), std::mem::transmute((base + IMAGE) as usize)) };
    assert_eq!(text(), 42);
    assert_eq!(arena(), 42);

    let macho = MachO::parse(&image, P).unwrap();
    let mut segments = model(&macho, &image);
    let mut model_memory = memory(&mut segments);
    let mut dl = Dl::open(&macho).unwrap();
    fixup::apply(&macho, &mut model_memory, base, &mut dl).unwrap();
    for segment in macho.segments.iter().filter(|it| it.writable()) {
        for at in (segment.vmaddr..segment.vmaddr + segment.vmsize).step_by(8) {
            let real = word(at - FIRST + base);
            let descriptor = (FIRST + TLV_VARS..FIRST + TLV_VARS + 0x30).contains(&at) && (at - FIRST - TLV_VARS) % 24 != 16;
            if !descriptor {
                assert_eq!(real, model_memory.read(at).unwrap(), "the word at {:#x}", at - FIRST);
            }
        }
    }
    drop(model_memory);
    assert_eq!(word(base + GOT), symbol(c"getpid"));
    assert_eq!(word(base + GOT + 8), base + TEXT_FN);
    assert_eq!(word(base + GOT + 16), 0);
    assert_eq!(word(base + DATA), base + TEXT_FN);
    assert_eq!(word(base + DATA + 0x10), symbol(c"getppid"));
    assert_eq!(word(base + BSS), 0);
    // SAFETY: `__bss` is the reservation's zero pages, made readable and writable.
    unsafe { ((base + BSS) as *mut u64).write(5) };
    assert_eq!(word(base + BSS), 5);

    assert_eq!(word(base + TLV_VARS), tlv::thunk());
    assert_eq!(word(base + TLV_VARS + 24), tlv::thunk());
    // SAFETY: the thunk takes a descriptor in x0 and returns the variable's address there,
    // keeping every other register a C callee may change: callable as a C function.
    let get: extern "C" fn(u64) -> *mut u64 = unsafe { std::mem::transmute(tlv::thunk() as usize) };
    let (data, bss) = (get(base + TLV_VARS), get(base + TLV_VARS + 24));
    assert_eq!(bss as u64, data as u64 + 8);
    // SAFETY: both are this thread's block for the image, 16 bytes from `data`.
    unsafe {
        assert_eq!((data.read(), bss.read()), (TEMPLATE, 0));
        data.write(1);
    }
    assert_eq!(get(base + TLV_VARS), data, "a thread's block is made once");
    let (descriptor, theirs) = (base + TLV_VARS, data as u64);
    let other = std::thread::spawn(move || {
        let block = get(descriptor);
        // SAFETY: the new thread's own block, made from the template.
        (block as u64, unsafe { block.read() })
    })
    .join()
    .unwrap();
    assert_ne!(other.0, theirs);
    assert_eq!(other.1, TEMPLATE);

    let mut info = UnwindSections {
        dso_base: 0,
        dwarf_section: 0,
        dwarf_section_length: 0,
        compact_unwind_section: 0,
        compact_unwind_section_length: 0,
    };
    assert_eq!(find_unwind_sections((base + TEXT_FN) as usize, &raw mut info), 1);
    assert_eq!((info.dso_base, info.dwarf_section, info.dwarf_section_length), (base as usize, (base + EH_FRAME) as usize, 4));
    assert_eq!(find_unwind_sections((base + IMAGE) as usize, &raw mut info), 0, "the arena registers its own");
    drop(view);
}

/// Where `a_loaded_image_is_the_model_mapped` maps its image: in [`crate::BASES`], where code
/// memories are, so nothing of this process is there; its mappings stay for the process.
const LOADED_AT: u64 = crate::BASES.start + (1 << 32);

/// A base something of this process holds already is an error, and what holds it is untouched:
/// the loader never maps elsewhere nor over it.
#[test]
fn a_taken_base_is_refused() {
    let _one = LOADING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let image = build(&Spec::default());
    let object = sys::Object::create("macho-taken", IMAGE + P).unwrap();
    let mut view = object.map_here(Access::Shared).unwrap();
    view.bytes_mut(0..IMAGE as usize).unwrap().copy_from_slice(&image);
    let granted = sys::granted(&object, Rights::Code).unwrap();
    let (mut channel, _other) = pair();
    let error = match link(&granted, &mut channel, &image, IMAGE, Some(view.address())) {
        Ok(_) => panic!("an image loaded over the daemon's own view"),
        Err(error) => error,
    };
    assert!(error.what.contains("are taken"), "{error}");
    assert_eq!(view.bytes(0..IMAGE as usize).unwrap(), &image[..], "the view is untouched");
}

/// Zero pages past the file are the reservation's own with no arena, and an error with one,
/// whose code would be there.
#[test]
fn zero_pages_past_the_file_take_the_arena_s_place() {
    let _one = LOADING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let spec = Spec { bss_after: true, ..Spec::default() };
    let image = build(&spec);
    let (mut channel, _other) = pair();

    let with_arena = sys::Object::create("macho-arena", IMAGE + P).unwrap();
    with_arena.map_here(Access::Shared).unwrap().bytes_mut(0..IMAGE as usize).unwrap().copy_from_slice(&image);
    let granted = sys::granted(&with_arena, Rights::Code).unwrap();
    let error = match link(&granted, &mut channel, &image, IMAGE, None) {
        Ok(_) => panic!("zero pages over the arena loaded"),
        Err(error) => error,
    };
    assert!(error.what.contains("image part"), "{error}");

    let alone = sys::Object::create("macho-alone", IMAGE).unwrap();
    alone.map_here(Access::Shared).unwrap().bytes_mut(0..IMAGE as usize).unwrap().copy_from_slice(&image);
    let granted = sys::granted(&alone, Rights::Code).unwrap();
    let loaded = link(&granted, &mut channel, &image, IMAGE, None).unwrap();
    assert_ne!(loaded.base, 0);
    assert_eq!(word(loaded.base + IMAGE), 0);
    // SAFETY: `__BSS`, the reservation's zero pages made readable and writable.
    unsafe { ((loaded.base + IMAGE) as *mut u64).write(3) };
    assert_eq!(word(loaded.base + IMAGE), 3);
}
