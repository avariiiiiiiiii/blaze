//! An ELF image built here in blaze-link's layout, relocated by the safe model into plain
//! buffers and loaded for real into this process, which must give the same bytes; what the
//! loader refuses; and mutated images, which must be errors or loads, never panics.

use std::ffi::{CStr, CString, c_void};
use std::sync::{Mutex, PoisonError};

use super::parse::{Elf, Machine};
use super::reloc::{self, Imports, Memory, Place, Segment};
use super::tls::{self, Loader};
use super::{Dl, Loaded, Source, link, place};
use crate::{Access, Error, sys};

/// blaze-link's AArch64 segment alignment, a multiple of every host page.
const P: u64 = 0x10000;

const DYNSTR: u64 = 0x300;
const SYMTAB: u64 = 0x400;
const VERSYM: u64 = 0x500;
const VERNEED: u64 = 0x540;
const RELA: u64 = 0x600;
const RELR: u64 = 0x780;
const EH_HDR: u64 = 0x900;
const EH: u64 = 0x910;
const READ_END: u64 = 0x914;
const F: u64 = P;
const RESOLVER: u64 = P + 0x10;
const GOT: u64 = 2 * P;
const INIT: u64 = 2 * P + 0x80;
const DYNAMIC: u64 = 2 * P + 0x100;
const DATA: u64 = 3 * P;
const TDATA: u64 = 3 * P + 0x100;
const DATA_FILE: u64 = 0x110;
const DATA_MEM: u64 = P + 0x100;
const FILE_LEN: u64 = 3 * P + 0x180;
const RESOLVED: u64 = 0x1234_5678;

const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_STRSZ: u64 = 10;
const DT_SYMENT: u64 = 11;
const DT_TEXTREL: u64 = 22;
const DT_INIT_ARRAY: u64 = 25;
const DT_INIT_ARRAYSZ: u64 = 27;
const DT_FLAGS: u64 = 30;
const DT_RELRSZ: u64 = 35;
const DT_RELR: u64 = 36;
const DT_RELRENT: u64 = 37;
const DT_VERSYM: u64 = 0x6fff_fff0;
const DT_FLAGS_1: u64 = 0x6fff_fffb;
const DT_VERNEED: u64 = 0x6fff_fffe;
const DT_VERNEEDNUM: u64 = 0x6fff_ffff;

/// The tests that load into this process patch its thread-local template: one at a time.
static LOADING: Mutex<()> = Mutex::new(());

struct Types {
    abs64: u32,
    glob_dat: u32,
    relative: u32,
    irelative: u32,
    tpoff: u32,
    dtpmod: u32,
    dtpoff: u32,
    tlsdesc: u32,
}

fn types(machine: Machine) -> Types {
    match machine {
        Machine::X86_64 => Types {
            abs64: 1,
            glob_dat: 6,
            relative: 8,
            irelative: 37,
            tpoff: 18,
            dtpmod: 16,
            dtpoff: 17,
            tlsdesc: 36,
        },
        Machine::Aarch64 => Types {
            abs64: 257,
            glob_dat: 1025,
            relative: 1027,
            irelative: 1032,
            tpoff: 1030,
            dtpmod: 1028,
            dtpoff: 1029,
            tlsdesc: 1031,
        },
    }
}

fn version_name(machine: Machine) -> &'static str {
    match machine {
        Machine::X86_64 => "GLIBC_2.2.5",
        Machine::Aarch64 => "GLIBC_2.17",
    }
}

fn put(image: &mut [u8], at: u64, bytes: &[u8]) {
    image[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
}

fn put16(image: &mut [u8], at: u64, value: u16) {
    put(image, at, &value.to_le_bytes());
}

fn put32(image: &mut [u8], at: u64, value: u32) {
    put(image, at, &value.to_le_bytes());
}

fn put64(image: &mut [u8], at: u64, value: u64) {
    put(image, at, &value.to_le_bytes());
}

fn get64(image: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(image[at as usize..at as usize + 8].try_into().expect("8 bytes"))
}

fn elf_hash(name: &[u8]) -> u32 {
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

/// The relocations of the built image, in `.rela.dyn` order: (where, type, symbol, addend).
fn relocations(machine: Machine) -> Vec<(u64, u32, u32, i64)> {
    let t = types(machine);
    vec![
        (GOT + 8, t.relative, 0, F as i64),
        (INIT, t.relative, 0, F as i64),
        (DATA + 32, t.relative, 0, 0x1234),
        (GOT, t.glob_dat, 1, 0),
        (GOT + 64, t.glob_dat, 2, 0),
        (DATA, t.abs64, 1, 16),
        (DATA + 16, t.abs64, 2, 5),
        (DATA + 24, t.abs64, 4, 2),
        (GOT + 16, t.tpoff, 0, 8),
        (GOT + 24, t.tpoff, 3, 0),
        (GOT + 32, t.dtpmod, 0, 0),
        (GOT + 40, t.dtpoff, 0, 4),
        (GOT + 48, t.tlsdesc, 0, 12),
        (DATA + 8, t.irelative, 0, RESOLVER as i64),
    ]
}

/// Where relocation `index`'s entry is in the built image.
fn rela(index: u64) -> u64 {
    RELA + index * 24
}

/// Where program header `index` is: 0 to 3 the `PT_LOAD`s (read, code, relro, data), 4
/// `PT_DYNAMIC`, 5 `PT_GNU_RELRO`, 6 `PT_GNU_EH_FRAME`, 7 `PT_GNU_STACK`, 8 `PT_TLS`.
fn phdr(index: u64) -> u64 {
    64 + index * 56
}

/// An image as blaze-link lays one out for blaze's loader: four `PT_LOAD`s each on a page,
/// file offsets equal to addresses, entry `f`, one import (`strlen`, versioned), one weak
/// import nothing defines, thread-locals and an IFUNC; with a `DT_RELR` table when `relr`.
fn build(machine: Machine, relr: bool) -> Vec<u8> {
    let mut image = vec![0u8; FILE_LEN as usize];
    let mut strings = vec![0u8];
    let mut add = |name: &str| {
        let at = strings.len() as u32;
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
        at
    };
    let libc = add("libc.so.6");
    let strlen = add("strlen");
    let absent = add("blaze_inproc_absent_weak");
    let tls_var = add("tls_var");
    let fdef = add("fdef");
    let version = add(version_name(machine));
    put(&mut image, DYNSTR, &strings);

    let mut symbol = |index: u64, name: u32, info: u8, shndx: u16, value: u64| {
        let at = SYMTAB + index * 24;
        put32(&mut image, at, name);
        image[at as usize + 4] = info;
        put16(&mut image, at + 6, shndx);
        put64(&mut image, at + 8, value);
    };
    symbol(1, strlen, 0x12, 0, 0);
    symbol(2, absent, 0x20, 0, 0);
    symbol(3, tls_var, 0x16, 9, 4);
    symbol(4, fdef, 0x12, 2, F);

    for (index, of) in [0u16, 2, 1, 1, 1].into_iter().enumerate() {
        put16(&mut image, VERSYM + 2 * index as u64, of);
    }
    put16(&mut image, VERNEED, 1);
    put16(&mut image, VERNEED + 2, 1);
    put32(&mut image, VERNEED + 4, libc);
    put32(&mut image, VERNEED + 8, 16);
    put32(&mut image, VERNEED + 12, 0);
    put32(&mut image, VERNEED + 16, elf_hash(version_name(machine).as_bytes()));
    put16(&mut image, VERNEED + 20, 0);
    put16(&mut image, VERNEED + 22, 2);
    put32(&mut image, VERNEED + 24, version);
    put32(&mut image, VERNEED + 28, 0);

    let relocations = relocations(machine);
    for (index, &(at, r_type, sym, addend)) in relocations.iter().enumerate() {
        let entry = rela(index as u64);
        put64(&mut image, entry, at);
        put64(&mut image, entry + 8, (u64::from(sym) << 32) | u64::from(r_type));
        put64(&mut image, entry + 16, addend as u64);
    }
    if relr {
        put64(&mut image, RELR, DATA + 48);
        put64(&mut image, RELR + 8, (0b101 << 1) | 1);
        for (at, value) in [(DATA + 48, 0x100), (DATA + 56, 0x200), (DATA + 64, 0x300), (DATA + 72, 0x400)] {
            put64(&mut image, at, value);
        }
    }

    put(&mut image, EH_HDR, &[1, 0x1b, 0x03, 0x3b]);
    put32(&mut image, EH_HDR + 4, (EH - (EH_HDR + 4)) as u32);
    put32(&mut image, EH_HDR + 8, 0);

    match machine {
        Machine::X86_64 => {
            put(&mut image, F, &[0xb8, 42, 0, 0, 0, 0xc3]);
            put(&mut image, RESOLVER, &[0xb8, 0x78, 0x56, 0x34, 0x12, 0xc3]);
        }
        Machine::Aarch64 => {
            put32(&mut image, F, 0x5280_0540);
            put32(&mut image, F + 4, 0xd65f_03c0);
            put32(&mut image, RESOLVER, 0x528a_cf00);
            put32(&mut image, RESOLVER + 4, 0x72a2_4680);
            put32(&mut image, RESOLVER + 8, 0xd65f_03c0);
        }
    }

    put64(&mut image, DATA + 40, 0xdead_beef);
    for index in 0..16u8 {
        image[(TDATA + u64::from(index)) as usize] = index + 1;
    }
    // What follows the last region in the file: the symbol table and the like, which the
    // loader must not show in `.bss`.
    image[(DATA + DATA_FILE) as usize..FILE_LEN as usize].fill(0xcc);

    let mut entries = vec![
        (DT_NEEDED, u64::from(libc)),
        (DT_STRTAB, DYNSTR),
        (DT_STRSZ, strings.len() as u64),
        (DT_SYMTAB, SYMTAB),
        (DT_SYMENT, 24),
        (DT_RELA, RELA),
        (DT_RELASZ, relocations.len() as u64 * 24),
        (DT_RELAENT, 24),
        (DT_INIT_ARRAY, INIT),
        (DT_INIT_ARRAYSZ, 8),
        (DT_VERSYM, VERSYM),
        (DT_VERNEED, VERNEED),
        (DT_VERNEEDNUM, 1),
        (DT_FLAGS, 8),
        (DT_FLAGS_1, 0x0800_0001),
    ];
    if relr {
        entries.extend([(DT_RELR, RELR), (DT_RELRSZ, 16), (DT_RELRENT, 8)]);
    }
    entries.push((0, 0));
    for (index, &(tag, value)) in entries.iter().enumerate() {
        put64(&mut image, DYNAMIC + 16 * index as u64, tag);
        put64(&mut image, DYNAMIC + 16 * index as u64 + 8, value);
    }
    let dynamic = entries.len() as u64 * 16;
    let relro = DYNAMIC + dynamic - 2 * P;

    put(&mut image, 0, b"\x7fELF\x02\x01\x01");
    put16(&mut image, 16, 3);
    put16(&mut image, 18, machine.em());
    put32(&mut image, 20, 1);
    put64(&mut image, 24, F);
    put64(&mut image, 32, 64);
    put16(&mut image, 52, 64);
    put16(&mut image, 54, 56);
    put16(&mut image, 56, 9);
    let headers: [(u32, u32, u64, u64, u64, u64); 9] = [
        (1, 4, 0, READ_END, READ_END, P),
        (1, 5, P, 0x20, 0x20, P),
        (1, 6, 2 * P, relro, relro, P),
        (1, 6, DATA, DATA_FILE, DATA_MEM, P),
        (2, 6, DYNAMIC, dynamic, dynamic, 8),
        (0x6474_e552, 4, 2 * P, P, P, 1),
        (0x6474_e550, 4, EH_HDR, 12, 12, 4),
        (0x6474_e551, 6, 0, 0, 0, 0),
        (7, 4, TDATA, 16, 32, 16),
    ];
    for (index, &(p_type, flags, vaddr, filesz, memsz, align)) in headers.iter().enumerate() {
        let at = phdr(index as u64);
        put32(&mut image, at, p_type);
        put32(&mut image, at + 4, flags);
        put64(&mut image, at + 8, vaddr);
        put64(&mut image, at + 16, vaddr);
        put64(&mut image, at + 24, vaddr);
        put64(&mut image, at + 32, filesz);
        put64(&mut image, at + 40, memsz);
        put64(&mut image, at + 48, align);
    }
    image
}

/// Imports from a table, IFUNC resolvers answered with a fixed value: the model's.
struct Table {
    strlen: u64,
    ifunc: u64,
}

impl Imports for Table {
    fn find(&mut self, name: &CStr, _: Option<&CStr>) -> Result<u64, String> {
        match name.to_bytes() {
            b"strlen" => Ok(self.strlen),
            _ => Err(format!("{name:?} is not in the table")),
        }
    }

    fn ifunc(&mut self, _: u64) -> u64 {
        self.ifunc
    }
}

fn test_place() -> Place {
    Place { base: 0x7f00_0000_0000, tp: -0x1000, module: 1, dtp: 0x40, tlsdesc: 0x5555_0000 }
}

/// The safe reference: the writable segments of `bytes` as plain buffers of whole pages,
/// relocated; `None` from the model for an image too big to buffer.
fn model(bytes: &[u8], place: &Place, table: &mut Table) -> Result<Vec<(u64, Vec<u8>)>, Error> {
    let page = sys::page();
    let elf = Elf::parse(bytes, page, Machine::host())?;
    let mut buffers = Vec::new();
    for load in elf.loads.iter().filter(|it| it.writable()) {
        let len = load.memsz.next_multiple_of(page);
        if len > 1 << 26 {
            return Err(Error::new("a segment too big for the model"));
        }
        let mut buffer = vec![0; len as usize];
        let file = elf.file(load.vaddr, load.filesz)?;
        buffer[..file.len()].copy_from_slice(file);
        buffers.push((load.vaddr, load.memsz, buffer));
    }
    {
        let segments = buffers
            .iter_mut()
            .map(|(vaddr, memsz, buffer)| Segment { vaddr: *vaddr, memsz: *memsz, bytes: buffer.as_mut_slice() })
            .collect();
        let mut memory = Memory::new(segments);
        reloc::relocate(&elf, &mut memory, place, table)?;
        assert_eq!(memory.segments().count(), elf.loads.iter().filter(|it| it.writable()).count());
    }
    Ok(buffers.into_iter().map(|(vaddr, _, buffer)| (vaddr, buffer)).collect())
}

fn word(buffers: &[(u64, Vec<u8>)], at: u64) -> u64 {
    let (vaddr, buffer) = buffers.iter().find(|(vaddr, buffer)| at >= *vaddr && at < vaddr + buffer.len() as u64).expect("mapped");
    get64(buffer, at - vaddr)
}

fn run_model(bytes: &[u8]) -> Result<Vec<(u64, Vec<u8>)>, Error> {
    model(bytes, &test_place(), &mut Table { strlen: 0x7e00_0000_1000, ifunc: RESOLVED })
}

fn refused(bytes: &[u8], needle: &str) {
    match run_model(bytes) {
        Ok(_) => panic!("loaded an image that has to be refused for {needle:?}"),
        Err(error) => assert!(error.what.contains(needle), "{needle:?} not in {:?}", error.what),
    }
}

#[test]
fn reads_blaze_links_layout() {
    let machine = Machine::host();
    let image = build(machine, false);
    let elf = Elf::parse(&image, sys::page(), machine).expect("parses");
    let accesses: Vec<Access> = elf.loads.iter().map(|it| it.access()).collect();
    assert_eq!(accesses, [Access::Read, Access::Execute, Access::Copy, Access::Copy]);
    assert_eq!(elf.entry, F);
    assert_eq!(elf.span, (DATA + DATA_MEM).next_multiple_of(sys::page()));
    let tls = elf.tls.expect("PT_TLS");
    assert_eq!((tls.vaddr, tls.filesz, tls.memsz, tls.align), (TDATA, 16, 32, 16));
    let relro = elf.relro.clone().expect("RELRO");
    assert_eq!(relro.start, 2 * P);
    assert!(relro.end > 2 * P && relro.end <= 3 * P);
    assert_eq!(elf.eh_frame, Some(EH));
    let needed: Vec<&CStr> = elf.dynamic.needed.iter().map(|&it| elf.string(it).expect("a name")).collect();
    assert_eq!(needed, [c"libc.so.6"]);
    let version = CString::new(version_name(machine)).expect("no NUL");
    assert_eq!(elf.version(1).expect("versioned"), Some(version.as_c_str()));
    assert_eq!(elf.version(2).expect("global"), None);
    assert_eq!(elf.symbol(3).expect("tls_var").value, 4);
    assert_eq!(elf.dynamic.init_array.map(|it| (it.vaddr, it.count)), Some((INIT, 1)));
}

#[test]
fn the_model_relocates_every_kind() {
    let place = test_place();
    let strlen = 0x7e00_0000_1000;
    let image = build(Machine::host(), true);
    let buffers = model(&image, &place, &mut Table { strlen, ifunc: RESOLVED }).expect("relocates");
    let base = place.base;
    let tp = place.tp as u64;
    assert_eq!(word(&buffers, GOT), strlen);
    assert_eq!(word(&buffers, GOT + 8), base + F);
    assert_eq!(word(&buffers, GOT + 16), tp.wrapping_add(8));
    assert_eq!(word(&buffers, GOT + 24), tp.wrapping_add(4));
    assert_eq!(word(&buffers, GOT + 32), place.module);
    assert_eq!(word(&buffers, GOT + 40), place.dtp + 4);
    assert_eq!(word(&buffers, GOT + 48), place.tlsdesc);
    assert_eq!(word(&buffers, GOT + 56), tp.wrapping_add(12));
    assert_eq!(word(&buffers, GOT + 64), 0, "a weak import nothing defines is null");
    assert_eq!(word(&buffers, INIT), base + F);
    assert_eq!(word(&buffers, DATA), strlen + 16);
    assert_eq!(word(&buffers, DATA + 8), RESOLVED);
    assert_eq!(word(&buffers, DATA + 16), 5);
    assert_eq!(word(&buffers, DATA + 24), base + F + 2);
    assert_eq!(word(&buffers, DATA + 32), base + 0x1234);
    assert_eq!(word(&buffers, DATA + 40), 0xdead_beef);
    assert_eq!(word(&buffers, DATA + 48), base + 0x100);
    assert_eq!(word(&buffers, DATA + 56), base + 0x200);
    assert_eq!(word(&buffers, DATA + 64), 0x300, "a clear bit of a RELR bitmap");
    assert_eq!(word(&buffers, DATA + 72), base + 0x400);
    let data = &buffers.iter().find(|it| it.0 == DATA).expect("data").1;
    let template: Vec<u8> = (1..=16).collect();
    assert_eq!(&data[0x100..0x110], &template[..]);
    assert!(data[DATA_FILE as usize..].iter().all(|&it| it == 0), "what follows the file bytes is zero");
}

#[test]
fn refuses_what_it_cannot_load_faithfully() {
    let machine = Machine::host();
    let good = build(machine, false);
    let edit = |change: &dyn Fn(&mut Vec<u8>)| {
        let mut image = good.clone();
        change(&mut image);
        image
    };
    let other = match machine {
        Machine::X86_64 => Machine::Aarch64,
        Machine::Aarch64 => Machine::X86_64,
    };
    refused(&edit(&|it| put16(it, 18, other.em())), "machine");
    refused(&edit(&|it| put16(it, 16, 2)), "ET_DYN");
    refused(&good[..40], "shorter than an ELF header");
    refused(&edit(&|it| put64(it, phdr(1) + 8, P + 0x1000)), "not its address");
    refused(
        &edit(&|it| {
            put64(it, phdr(1) + 8, P + 0x10);
            put64(it, phdr(1) + 16, P + 0x10);
        }),
        "does not start on a page",
    );
    refused(&edit(&|it| put32(it, phdr(1) + 4, 7)), "both writable and executable");
    refused(&edit(&|it| put32(it, phdr(7), 3)), "interpreter");
    refused(&edit(&|it| put32(it, phdr(7) + 4, 7)), "executable stack");
    refused(&edit(&|it| put64(it, phdr(1) + 32, FILE_LEN)), "past the image");
    refused(&edit(&|it| put16(it, SYMTAB + 3 * 24 + 6, 0)), "imported thread-local `tls_var`");
    refused(&edit(&|it| put32(it, rela(0) + 8, 0x7777)), "relocation type the loader does not apply");
    refused(&edit(&|it| put64(it, rela(0), F)), "text relocation");
    refused(&edit(&|it| put64(it, rela(0), DYNSTR)), "which the loader does not write");
    refused(&edit(&|it| put64(it, rela(0), DATA + DATA_MEM - 4)), "runs past the end");
    refused(&edit(&|it| put64(it, rela(13) + 16, GOT)), "IFUNC resolver");
    refused(
        &edit(&|it| {
            let name = get_name(it, "fdef");
            put32(it, SYMTAB + 24, name);
        }),
        "none of the libraries",
    );
    refused(&edit(&|it| put64(it, DYNAMIC + 13 * 16, DT_TEXTREL)), "DT_TEXTREL");
    refused(&edit(&|it| put64(it, DYNAMIC + 13 * 16, 0x6000_0000)), "dynamic tag 0x60000000");
    refused(&edit(&|it| put64(it, rela(9) + 16, 64)), "outside the image's");
    refused(
        &edit(&|it| {
            for at in (EH..P).step_by(8) {
                put(it, at, &[4, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
            }
        }),
        "no zero terminator",
    );
}

/// The offset of `name` in the built image's string table.
fn get_name(image: &[u8], name: &str) -> u32 {
    let table = &image[DYNSTR as usize..SYMTAB as usize];
    let needle = [name.as_bytes(), &[0]].concat();
    let at = table.windows(needle.len()).position(|it| it == needle).expect("the name is there");
    at as u32
}

#[test]
fn mutated_images_are_errors_or_loads_never_panics() {
    let mut image = build(Machine::host(), true);
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let hot = [(0, 0xa00), (2 * P, 0x200), (3 * P, 0x180), (0, FILE_LEN)];
    for _ in 0..3000 {
        let edits = 1 + next() % 6;
        let mut undo = Vec::with_capacity(edits as usize);
        for _ in 0..edits {
            let (start, len) = hot[(next() % hot.len() as u64) as usize];
            let at = (start + next() % len) as usize;
            let old = image[at];
            undo.push((at, old));
            image[at] = match next() % 4 {
                0 => 0,
                1 => 0xff,
                2 => old ^ (1 << (next() % 8)),
                _ => next() as u8,
            };
        }
        let _ = run_model(&image);
        for (at, old) in undo.into_iter().rev() {
            image[at] = old;
        }
    }
    for len in (0..image.len()).step_by(97) {
        let _ = run_model(&image[..len]);
    }
    run_model(&image).expect("every mutation was undone");
}

/// Segments mapped from a buffer: anonymous pages the bytes are copied into, then given the
/// access asked for.
struct Local {
    object: Vec<u8>,
}

impl Source for Local {
    fn len(&self) -> u64 {
        self.object.len() as u64
    }

    fn reserve(&mut self, len: u64, at: Option<u64>) -> Result<u64, Error> {
        let fixed = if at.is_some() { libc::MAP_FIXED_NOREPLACE } else { 0 };
        // SAFETY: a new anonymous mapping with no access, anywhere or only where nothing is.
        let mapped = unsafe {
            libc::mmap(
                at.unwrap_or(0) as *mut c_void,
                len as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | fixed,
                -1,
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(Error::new(format!("reserving: {}", std::io::Error::last_os_error())));
        }
        Ok(mapped as u64)
    }

    fn map(&mut self, offset: u64, len: u64, at: u64, access: Access) -> Result<u64, Error> {
        let end = offset.checked_add(len).filter(|&it| it <= self.len()).ok_or_else(|| Error::new("past the object"))?;
        // SAFETY: `at..at + len` is inside the reservation `reserve` made for this image (the
        // loader maps only there), which holds nothing else yet.
        let mapped = unsafe {
            libc::mmap(
                at as *mut c_void,
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if mapped == libc::MAP_FAILED || mapped as u64 != at {
            return Err(Error::new(format!("mapping: {}", std::io::Error::last_os_error())));
        }
        // SAFETY: the mapping just made, `len` bytes, writable, which nothing else refers to.
        unsafe { std::slice::from_raw_parts_mut(at as *mut u8, len as usize) }
            .copy_from_slice(&self.object[offset as usize..end as usize]);
        let protection = match access {
            Access::Execute => libc::PROT_READ | libc::PROT_EXEC,
            Access::Read => libc::PROT_READ,
            Access::Copy | Access::Shared => libc::PROT_READ | libc::PROT_WRITE,
        };
        // SAFETY: as above.
        if unsafe { libc::mprotect(at as *mut c_void, len as usize, protection) } != 0 {
            return Err(Error::new(format!("protecting: {}", std::io::Error::last_os_error())));
        }
        Ok(at)
    }
}

/// The permissions /proc/self/maps gives the page at `address`.
fn permissions(address: u64) -> String {
    let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(permissions)) = (fields.next(), fields.next()) else { continue };
        let Some((start, end)) = range.split_once('-') else { continue };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16)) else { continue };
        if address >= start && address < end {
            return permissions.to_owned();
        }
    }
    panic!("nothing is mapped at {address:#x}")
}

/// The calling thread's reserve's first `len` bytes.
fn reserve_bytes(len: usize) -> Vec<u8> {
    // SAFETY: the calling thread's reserve, `RESERVE_SIZE` bytes, live as long as the thread;
    // only read, and nothing writes it meanwhile (the tests that do hold `LOADING`).
    unsafe { std::slice::from_raw_parts(tls::here() as *const u8, len) }.to_vec()
}

/// Calls the TLS descriptor at `desc` as compiled code does.
fn call_tlsdesc(desc: u64) -> u64 {
    let out: u64;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `desc` is a descriptor the loader filled: its first word is the static resolver,
    // which reads the second word into `rax` and returns, touching nothing else.
    unsafe {
        std::arch::asm!("call qword ptr [rax]", inout("rax") desc => out, clobber_abi("C"));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as on x86-64, with the descriptor in `x0`.
    unsafe {
        std::arch::asm!("ldr x1, [x0]", "blr x1", inout("x0") desc => out, out("x1") _, clobber_abi("C"));
    }
    out
}

fn load_here(image: &[u8], object_len: u64) -> Result<Loaded, Error> {
    let image_len = (image.len() as u64).next_multiple_of(sys::page());
    let mut padded = image.to_vec();
    padded.resize(image_len as usize, 0);
    let mut object = padded.clone();
    object.resize(object_len.max(image_len) as usize, 0);
    let reserve = super::reserve()?;
    link(&mut Local { object }, &padded, image_len, None, reserve, &mut Dl)
}

#[test]
fn loads_into_this_process_as_the_model_relocates() {
    let _loading = LOADING.lock().unwrap_or_else(PoisonError::into_inner);
    let machine = Machine::host();
    let image = build(machine, true);
    let loaded = load_here(&image, 0).expect("loads");
    let base = loaded.base;
    assert_eq!(base % sys::page(), 0);

    let version = CString::new(version_name(machine)).expect("no NUL");
    let strlen = Dl.find(c"strlen", Some(version.as_c_str())).expect("libc has strlen");
    let loader = Loader::find().expect("the loader's block");
    let place = place(base, &loader);
    let buffers = model(&image, &place, &mut Table { strlen, ifunc: RESOLVED }).expect("the model relocates");
    for (vaddr, buffer) in &buffers {
        // SAFETY: the loader mapped the writable segment at `base + vaddr`, its pages readable,
        // and leaves it mapped for the life of the process.
        let real = unsafe { std::slice::from_raw_parts((base + vaddr) as *const u8, buffer.len()) };
        assert!(real == &buffer[..], "the segment at {vaddr:#x} differs from the model's");
    }

    // SAFETY: `F` is the image's `f`, `mov 42` and `ret`, in its code mapped executable.
    let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute((base + F) as usize) };
    assert_eq!(f(), 42);
    assert_eq!(word(&buffers, DATA + 8), RESOLVED, "the IFUNC resolver ran");
    assert_eq!(loaded.entry, base + F);
    assert_eq!(loaded.init_array, [base + F]);

    let template: Vec<u8> = (1..=16).chain([0; 16]).collect();
    assert_eq!(reserve_bytes(32), template, "this thread's reserve has the template");
    let later = std::thread::spawn(|| reserve_bytes(32)).join().expect("the thread ran");
    assert_eq!(later, template, "a thread started after the load starts with the template");
    let reserve = loader.reserve();
    assert_eq!(call_tlsdesc(base + GOT + 48), (reserve.bias as u64).wrapping_add(12));
    assert_eq!(word(&buffers, GOT + 16), (reserve.bias as u64).wrapping_add(8));

    assert_eq!(permissions(base + 2 * P), "r--p", "RELRO is read-only once relocated");
    assert_eq!(&permissions(base + F)[..3], "r-x");
    assert_eq!(&permissions(base + DATA)[..3], "rw-");
    let bss = base + (DATA + DATA_FILE).next_multiple_of(sys::page());
    assert_eq!(&permissions(bss)[..3], "rw-");
}

#[test]
fn maps_the_arena_after_the_image_and_refuses_bss_in_it() {
    let _loading = LOADING.lock().unwrap_or_else(PoisonError::into_inner);
    let page = sys::page();
    let image = build(Machine::host(), false);
    let short = (image.len() as u64).next_multiple_of(page);
    let error = load_here(&image, short + page).err().expect("bss over the arena");
    assert!(error.what.contains("arena"), "{}", error.what);

    let mut padded = image.clone();
    let image_len = (DATA + DATA_MEM).next_multiple_of(page);
    padded.resize(image_len as usize, 0);
    let loaded = load_here(&padded, image_len + 2 * page).expect("loads");
    assert_eq!(&permissions(loaded.base + image_len)[..3], "r-x");
    assert_eq!(&permissions(loaded.base + image_len + page)[..3], "r-x");
}

#[test]
fn refuses_thread_locals_past_the_reserve() {
    let reserve = super::reserve().expect("the reserve");
    let mut image = build(Machine::host(), false);
    put64(&mut image, phdr(8) + 40, reserve.size + 16);
    let error = load_here(&image, 0).err().expect("too many thread-locals");
    assert!(error.what.contains("reserve"), "{}", error.what);
    let mut image = build(Machine::host(), false);
    put64(&mut image, phdr(8) + 48, reserve.align * 2);
    let error = load_here(&image, 0).err().expect("thread-locals aligned past the reserve");
    assert!(error.what.contains("aligned to"), "{}", error.what);
}

#[test]
fn the_reserve_is_at_one_offset_from_every_thread() {
    let here = super::reserve().expect("the reserve");
    assert_eq!(here.size, crate::load::RESERVE_SIZE);
    assert_eq!(here.align, crate::load::RESERVE_ALIGN);
    let there = std::thread::spawn(|| super::reserve().expect("the reserve")).join().expect("the thread ran");
    assert_eq!(here, there);
    assert_eq!(tls::here() % crate::load::RESERVE_ALIGN, 0);
}
