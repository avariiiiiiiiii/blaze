use std::ffi::CString;
use std::ops::Range;

use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use super::parse::{self, Symbol, Tls};
use super::*;

const BASE: u64 = 0x1_4000_0000;
const TEXT: u64 = 0x1000;
const GRANULE: u64 = 0x10000;
const OPTIONAL: usize = 0x58;
const SECTIONS: usize = OPTIONAL + 240;
const REL: u16 = 0xa000;

/// Where the sample's sections are when its data starts at `start`.
#[derive(Clone, Copy)]
struct Layout {
    rdata: u64,
    data: u64,
    pdata: u64,
    reloc: u64,
    size: u64,
}

impl Layout {
    fn at(start: u64) -> Layout {
        Layout { rdata: start, data: start + 0x1000, pdata: start + 0x3000, reloc: start + 0x4000, size: start + 0x5000 }
    }
}

fn put(image: &mut [u8], at: u64, bytes: &[u8]) {
    image[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
}

fn put16(image: &mut [u8], at: u64, value: u16) {
    put(image, at, &value.to_le_bytes());
}

fn put32(image: &mut [u8], at: u64, value: u64) {
    put(image, at, &(value as u32).to_le_bytes());
}

fn put64(image: &mut [u8], at: u64, value: u64) {
    put(image, at, &value.to_le_bytes());
}

fn le16(image: &[u8], at: u64) -> u16 {
    u16::from_le_bytes(image[at as usize..at as usize + 2].try_into().unwrap())
}

fn le32(image: &[u8], at: u64) -> u64 {
    u64::from(u32::from_le_bytes(image[at as usize..at as usize + 4].try_into().unwrap()))
}

fn le64(image: &[u8], at: u64) -> u64 {
    u64::from_le_bytes(image[at as usize..at as usize + 8].try_into().unwrap())
}

fn section(image: &mut [u8], index: usize, name: &[u8], rva: u64, virtual_len: u64, raw_len: u64, flags: u64) {
    let at = (SECTIONS + 40 * index) as u64;
    put(image, at, name);
    put32(image, at + 8, virtual_len);
    put32(image, at + 12, rva);
    put32(image, at + 16, raw_len);
    put32(image, at + 20, rva);
    put32(image, at + 36, flags);
}

fn directory(image: &mut [u8], index: usize, rva: u64, size: u64) {
    put32(image, (OPTIONAL + 112 + 8 * index) as u64, rva);
    put32(image, (OPTIONAL + 116 + 8 * index) as u64, size);
}

/// An executable as blaze-link lays one out, its data from `start` on: code, read-only data
/// with imports from kernel32 and ntdll, a TLS directory and a callback; data with pointers to
/// rebase, the address slots, the TLS template and index, and garbage past its raw bytes; a
/// function table; base relocations.
fn sample(start: u64) -> Vec<u8> {
    let Layout { rdata: r, data: w, pdata: p, reloc, size } = Layout::at(start);
    let image = &mut vec![0u8; size.next_multiple_of(GRANULE) as usize];
    put(image, 0, b"MZ");
    put32(image, 0x3c, 0x40);
    put(image, 0x40, b"PE\0\0");
    put16(image, 0x44, 0x8664);
    put16(image, 0x46, 5);
    put16(image, 0x54, 240);
    put16(image, 0x56, 0x22);
    let o = OPTIONAL as u64;
    put16(image, o, 0x20b);
    put32(image, o + 16, TEXT);
    put64(image, o + 24, BASE);
    put32(image, o + 32, 0x1000);
    put32(image, o + 36, 0x200);
    put32(image, o + 56, size);
    put32(image, o + 60, 0x400);
    put64(image, o + 72, 0x10_0000);
    put32(image, o + 108, 16);
    section(image, 0, b".text\0\0\0", TEXT, 0x200, 0x200, 0x6000_0020);
    section(image, 1, b".rdata\0\0", r, 0x1000, 0x1000, 0x4000_0040);
    section(image, 2, b".data\0\0\0", w, 0x1800, 0x1000, 0xc000_0040);
    section(image, 3, b".pdata\0\0", p, 0x18, 0x200, 0x4000_0040);
    section(image, 4, b".reloc\0\0", reloc, 0x1c, 0x200, 0x4200_0040);
    image[TEXT as usize..TEXT as usize + 0x200].fill(0xcc);

    // Imports: descriptors, lookup tables, names, address slots (filled as the lookup tables).
    for (index, (lookup, name, slots)) in [(r + 0x100, r + 0x300, w + 0x100), (r + 0x140, r + 0x320, w + 0x140)].into_iter().enumerate() {
        let at = r + 20 * index as u64;
        put32(image, at, lookup);
        put32(image, at + 12, name);
        put32(image, at + 16, slots);
    }
    for (lookup, slots, names) in [(r + 0x100, w + 0x100, &[r + 0x200, r + 0x220][..]), (r + 0x140, w + 0x140, &[r + 0x240])] {
        for (index, name) in names.iter().enumerate() {
            put64(image, lookup + 8 * index as u64, *name);
            put64(image, slots + 8 * index as u64, *name);
        }
    }
    put(image, r + 0x202, b"GetCurrentProcessId\0");
    put(image, r + 0x222, b"RtlPcToFileHeader\0");
    put(image, r + 0x242, b"RtlGetVersion\0");
    put(image, r + 0x300, b"KERNEL32.dll\0");
    put(image, r + 0x320, b"ntdll.dll\0");
    directory(image, 1, r, 60);
    directory(image, 12, w + 0x100, 0x58);

    // Thread-locals: 0x20 bytes of template (a pointer to rebase among them), 0x10 of zeros.
    put64(image, r + 0x400, BASE + w + 0x400);
    put64(image, r + 0x408, BASE + w + 0x420);
    put64(image, r + 0x410, BASE + w + 0x500);
    put64(image, r + 0x418, BASE + r + 0x440);
    put32(image, r + 0x420, 0x10);
    put32(image, r + 0x424, 0x0030_0000);
    put64(image, r + 0x440, BASE + TEXT + 0x10);
    for index in 0..0x20u8 {
        image[(w + 0x400) as usize + usize::from(index)] = index + 1;
    }
    put64(image, w + 0x408, BASE + w + 0x10);
    directory(image, 9, r + 0x400, 40);

    // Data to rebase, and garbage past the raw bytes, which the loader zeroes.
    put64(image, w, BASE + w + 0x10);
    put64(image, w + 8, BASE + TEXT);
    put(image, w + 0x1100, b"garbage");

    // Two functions, both unwound by the same information.
    for (index, (begin, end)) in [(TEXT, TEXT + 0x10), (TEXT + 0x10, TEXT + 0x20)].into_iter().enumerate() {
        let at = p + 12 * index as u64;
        put32(image, at, begin);
        put32(image, at + 4, end);
        put32(image, at + 8, r + 0x600);
    }
    directory(image, 3, p, 24);

    // Base relocations: a block for the data page, one for the read-only data page.
    put32(image, reloc, w);
    put32(image, reloc + 4, 16);
    for (index, entry) in [REL, REL | 0x008, REL | 0x408, 0].into_iter().enumerate() {
        put16(image, reloc + 8 + 2 * index as u64, entry);
    }
    put32(image, reloc + 16, r);
    put32(image, reloc + 20, 12);
    put16(image, reloc + 24, REL | 0x440);
    put16(image, reloc + 26, 0);
    directory(image, 5, reloc, 28);
    std::mem::take(image)
}

fn reference_directory(image: &[u8], index: usize) -> (u64, u64) {
    (le32(image, (OPTIONAL + 112 + 8 * index) as u64), le32(image, (OPTIONAL + 116 + 8 * index) as u64))
}

fn reference_relocations(image: &[u8]) -> Vec<u64> {
    let (mut at, size) = reference_directory(image, 5);
    let end = at + size;
    let mut out = Vec::new();
    while at < end {
        let (page, size) = (le32(image, at), le32(image, at + 4));
        for entry in (at + 8..at + size).step_by(2) {
            let entry = le16(image, entry);
            if entry >> 12 == 10 {
                out.push(page + u64::from(entry & 0xfff));
            }
        }
        at += size;
    }
    out
}

#[derive(PartialEq, Eq, Debug)]
enum Named {
    Name(String),
    Ordinal(u16),
}

fn c_string(image: &[u8], at: u64) -> String {
    let bytes = &image[at as usize..];
    String::from_utf8(bytes[..bytes.iter().position(|it| *it == 0).unwrap()].to_vec()).unwrap()
}

fn reference_imports(image: &[u8]) -> Vec<(String, u64, Named)> {
    let (mut at, _) = reference_directory(image, 1);
    let mut out = Vec::new();
    while le32(image, at + 12) != 0 {
        let library = c_string(image, le32(image, at + 12));
        let (lookup, slots) = (le32(image, at), le32(image, at + 16));
        for index in 0.. {
            let thunk = le64(image, lookup + 8 * index);
            if thunk == 0 {
                break;
            }
            let symbol = if thunk >> 63 == 1 { Named::Ordinal(thunk as u16) } else { Named::Name(c_string(image, thunk + 2)) };
            out.push((library.clone(), slots + 8 * index, symbol));
        }
        at += 20;
    }
    out
}

fn walked_imports(headers: &parse::Headers<'_>) -> Vec<(String, u64, Named)> {
    let mut out = Vec::new();
    for library in parse::imports(headers).unwrap() {
        let name = String::from_utf8(library.name.to_vec()).unwrap();
        for symbol in library.symbols(headers) {
            let (slot, symbol) = symbol.unwrap();
            let symbol = match symbol {
                Symbol::Name(it) => Named::Name(String::from_utf8(it.to_vec()).unwrap()),
                Symbol::Ordinal(it) => Named::Ordinal(it),
            };
            out.push((name.clone(), slot, symbol));
        }
    }
    out
}

fn error(result: Result<impl std::fmt::Debug, Error>) -> String {
    result.expect_err("an error").to_string()
}

#[test]
fn the_sample_parses() {
    let image = sample(GRANULE);
    let layout = Layout::at(GRANULE);
    let headers = parse::parse(&image).unwrap();
    assert_eq!((headers.image_base, headers.size_of_image, headers.entry), (BASE, layout.size, TEXT));
    assert!(headers.relocatable);
    assert_eq!(headers.sections.len(), 5);
    assert_eq!(headers.code_end(GRANULE).unwrap(), GRANULE);
    assert_eq!(
        parse::tls(&headers).unwrap(),
        Some(Tls {
            start: layout.data + 0x400,
            init: 0x20,
            zero: 0x10,
            index: layout.data + 0x500,
            callbacks: vec![TEXT + 0x10],
            align: 4,
        })
    );
    assert_eq!(parse::functions(&headers, GRANULE).unwrap(), Some((layout.pdata, 2)));
}

#[test]
fn the_walkers_agree_with_the_reference() {
    let image = sample(GRANULE);
    let headers = parse::parse(&image).unwrap();
    let relocations: Vec<u64> = parse::relocations(&headers).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(relocations, reference_relocations(&image));
    assert_eq!(relocations.len(), 4);
    assert_eq!(walked_imports(&headers), reference_imports(&image));

    let mut by_ordinal = image.clone();
    let layout = Layout::at(GRANULE);
    put64(&mut by_ordinal, layout.rdata + 0x140, (1 << 63) | 5);
    let headers = parse::parse(&by_ordinal).unwrap();
    let walked = walked_imports(&headers);
    assert_eq!(walked, reference_imports(&by_ordinal));
    assert_eq!(walked[2].2, Named::Ordinal(5));
}

#[test]
fn relocating_writes_what_the_reference_does() {
    let image = sample(GRANULE);
    let headers = parse::parse(&image).unwrap();
    let mut memory = image.clone();
    let at = memory.as_mut_ptr() as u64;
    relocate(&headers, at).unwrap();
    let delta = at.wrapping_sub(BASE);
    let mut want = image.clone();
    for rva in reference_relocations(&image) {
        let value = le64(&want, rva).wrapping_add(delta);
        put64(&mut want, rva, value);
    }
    assert_eq!(memory, want);
}

#[test]
fn a_relocation_into_code_is_refused() {
    let mut image = sample(GRANULE);
    put32(&mut image, Layout::at(GRANULE).reloc, TEXT);
    let headers = parse::parse(&image).unwrap();
    let mut memory = image.clone();
    let message = error(relocate(&headers, memory.as_mut_ptr() as u64));
    assert!(message.contains(".text") && message.contains("blaze-link"), "{message}");
}

#[test]
fn data_past_its_raw_bytes_is_zeroed() {
    let image = sample(GRANULE);
    let headers = parse::parse(&image).unwrap();
    let mut memory = image.clone();
    zero_tails(&headers, memory.as_mut_ptr() as u64);
    let data = Layout::at(GRANULE).data as usize;
    assert!(memory[data + 0x1000..data + 0x2000].iter().all(|it| *it == 0));
    assert_eq!(memory[..data + 0x1000], image[..data + 0x1000]);
    assert_eq!(memory[data + 0x2000..], image[data + 0x2000..]);
}

fn wide_name(name: &str) -> Vec<u16> {
    name.encode_utf16().chain([0]).collect()
}

fn system(library: &str, symbol: &str) -> u64 {
    let library = wide_name(library);
    let symbol = CString::new(symbol).unwrap();
    // SAFETY: NUL-terminated names; the libraries are loaded in every process.
    let function = unsafe { GetProcAddress(GetModuleHandleW(library.as_ptr()), symbol.as_ptr().cast()) };
    function.expect("the symbol") as usize as u64
}

#[test]
fn linking_fills_the_slots_the_reference_names() {
    let image = sample(GRANULE);
    let headers = parse::parse(&image).unwrap();
    let libraries = parse::imports(&headers).unwrap();
    let mut memory = image.clone();
    let at = memory.as_mut_ptr() as u64;
    system_pc_to_file_header().unwrap();
    assert_eq!(link(&headers, &libraries, at).unwrap().len(), 2);
    let ours = pc_to_file_header as PcToFileHeader as usize as u64;
    for (library, slot, symbol) in reference_imports(&image) {
        let Named::Name(symbol) = symbol else { unreachable!("the sample imports by name") };
        let want = if symbol == "RtlPcToFileHeader" { ours } else { system(&library, &symbol) };
        assert_eq!(le64(&memory, slot), want, "{symbol} in {library}");
    }
}

#[test]
fn a_missing_import_is_named() {
    let mut image = sample(GRANULE);
    put(&mut image, Layout::at(GRANULE).rdata + 0x242, b"NoSuchFunction\0");
    let headers = parse::parse(&image).unwrap();
    let libraries = parse::imports(&headers).unwrap();
    let mut memory = image.clone();
    let message = error(link(&headers, &libraries, memory.as_mut_ptr() as u64));
    assert!(message.contains("NoSuchFunction") && message.contains("ntdll.dll"), "{message}");
}

#[test]
fn the_system_finds_its_modules_and_the_wrapper_the_program() {
    system_pc_to_file_header().unwrap();
    let kernel32 = wide_name("kernel32.dll");
    // SAFETY: a NUL-terminated name.
    let module = unsafe { GetModuleHandleW(kernel32.as_ptr()) };
    let pc = system("kernel32.dll", "GetCurrentProcessId");
    let mut base = std::ptr::null_mut();
    // SAFETY: a pc and an out pointer, as `RtlPcToFileHeader` takes them.
    assert_eq!(unsafe { pc_to_file_header(pc as *const c_void, &mut base) }, module);
    assert_eq!(base, module);

    static PRETEND: [u8; 256] = [0; 256];
    let start = PRETEND.as_ptr() as u64;
    let _ = IMAGE.set((start, 256));
    if IMAGE.get() == Some(&(start, 256)) {
        // SAFETY: as above.
        assert_eq!(unsafe { pc_to_file_header((start + 100) as *const c_void, &mut base) } as u64, start);
        assert_eq!(base as u64, start);
    }
}

#[test]
fn a_four_kib_layout_is_refused_naming_what_blaze_link_must_do() {
    let image = sample(0x2000);
    let headers = parse::parse(&image).unwrap();
    let message = error(headers.code_end(GRANULE));
    assert!(message.contains(".rdata") && message.contains("blaze-link") && message.contains("0x10000"), "{message}");
}

fn field(index: usize, offset: u64) -> u64 {
    (SECTIONS + 40 * index) as u64 + offset
}

#[test]
fn malformed_headers_are_refused() {
    let cases: [(&str, fn(&mut Vec<u8>)); 9] = [
        ("PointerToRawData", |it| put32(it, field(1, 20), GRANULE + 0x200)),
        ("writable and executable", |it| put32(it, field(0, 36), 0xe000_0020)),
        ("DLLs", |it| put16(it, 0x56, 0x2022)),
        ("delay-load", |it| directory(it, 13, GRANULE, 64)),
        ("machine", |it| put16(it, 0x44, 0x14c)),
        ("zero-fills", |it| put32(it, field(0, 16), 0x100)),
        ("no section of code", |it| put32(it, (OPTIONAL + 16) as u64, GRANULE)),
        ("over what ends", |it| put32(it, field(2, 12), GRANULE + 0x800)),
        ("past the end", |it| it.truncate(0x3e)),
    ];
    for (want, break_it) in cases {
        let mut image = sample(GRANULE);
        break_it(&mut image);
        let message = error(parse::parse(&image));
        assert!(message.contains(want), "{want}: {message}");
    }
}

#[test]
fn malformed_tables_are_refused() {
    let layout = Layout::at(GRANULE);
    let mut image = sample(GRANULE);
    put32(&mut image, layout.pdata + 12, TEXT + 0x8);
    let headers = parse::parse(&image).unwrap();
    assert!(error(parse::functions(&headers, GRANULE)).contains("entry 1"));

    let mut image = sample(GRANULE);
    put16(&mut image, layout.reloc + 8, 0x3000);
    let headers = parse::parse(&image).unwrap();
    let walked: Vec<_> = parse::relocations(&headers).unwrap().collect();
    assert_eq!(walked.len(), 1);
    assert!(walked[0].as_ref().unwrap_err().to_string().contains("type 3"));

    let mut image = sample(GRANULE);
    put64(&mut image, layout.rdata + 0x440, BASE + layout.rdata);
    let headers = parse::parse(&image).unwrap();
    assert!(error(parse::tls(&headers)).contains("TLS callback"));

    let mut image = sample(GRANULE);
    put64(&mut image, layout.rdata + 0x410, BASE + TEXT);
    let headers = parse::parse(&image).unwrap();
    assert!(error(parse::tls(&headers)).contains("TLS index"));
}

#[test]
fn templates_larger_than_the_reserve_are_refused() {
    let reserve = TlsReserve { bias: 0x40, size: RESERVE_SIZE, align: ALIGN };
    let tls = |init, zero, align| Tls { start: 0, init, zero, index: 0, callbacks: Vec::new(), align };
    assert!(fits(&tls(0x100, 0x100, 16), reserve).is_ok());
    assert!(fits(&tls(RESERVE_SIZE, 0, 8), reserve).is_ok());
    assert!(error(fits(&tls(RESERVE_SIZE, 1, 8), reserve)).contains("reserve"));
    assert!(fits(&tls(8, 0, 32), reserve).is_err());
}

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Every walker over `bytes`, their errors dropped: none may panic.
fn walk_all(bytes: &[u8]) {
    let Ok(headers) = parse::parse(bytes) else { return };
    let code_end = headers.code_end(GRANULE).unwrap_or(headers.size_of_image);
    let _ = parse::tls(&headers);
    let _ = parse::functions(&headers, code_end);
    if let Ok(libraries) = parse::imports(&headers) {
        for library in libraries {
            library.symbols(&headers).for_each(drop);
        }
    }
    if let Ok(relocations) = parse::relocations(&headers) {
        relocations.for_each(drop);
    }
    for section in &headers.sections {
        let _ = headers.section_of(section.rva, section.extent);
        let _ = headers.raw(section.rva, section.raw_len);
    }
}

#[test]
fn mutated_images_never_panic() {
    let mut image = sample(GRANULE);
    let layout = Layout::at(GRANULE);
    let regions: [Range<u64>; 5] = [
        0..0x400,
        layout.rdata..layout.rdata + 0x460,
        layout.data..layout.data + 0x520,
        layout.pdata..layout.pdata + 0x18,
        layout.reloc..layout.reloc + 0x1c,
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..20_000 {
        let mut undo = Vec::new();
        for _ in 0..1 + rng(&mut state) % 8 {
            let region = &regions[(rng(&mut state) % regions.len() as u64) as usize];
            let at = (region.start + rng(&mut state) % (region.end - region.start)) as usize;
            undo.push((at, image[at]));
            let random = rng(&mut state);
            image[at] = match random % 4 {
                0 => (random >> 8) as u8,
                1 => 0,
                2 => 0xff,
                _ => image[at] ^ (1 << ((random >> 8) % 8)),
            };
        }
        let len = match rng(&mut state) % 8 {
            0 => (rng(&mut state) % image.len() as u64) as usize,
            _ => image.len(),
        };
        walk_all(&image[..len]);
        for (at, old) in undo.into_iter().rev() {
            image[at] = old;
        }
    }
    assert_eq!(image, sample(GRANULE));
}

#[test]
fn the_first_argument_is_the_loaders_path() {
    let cases = [
        (r#""C:\blaze\run.exe" prog a b"#, "prog a b"),
        (r"C:\blaze\run.exe prog", "prog"),
        ("\"C:\\with space\\run.exe\" \t x", "x"),
        (r"run.exe", ""),
        (r#""unterminated"#, ""),
        ("run.exe\tprog", "prog"),
        ("", ""),
    ];
    for (line, rest) in cases {
        let wide: Vec<u16> = line.encode_utf16().collect();
        assert_eq!(&line[parse::after_first(line.as_bytes())..], rest, "{line}");
        assert_eq!(String::from_utf16(&wide[parse::after_first(&wide)..]).unwrap(), rest, "{line}");
    }
}

#[test]
fn dropping_the_first_argument_keeps_the_terminator() {
    let mut line: Vec<u16> = "\"C:\\run.exe\" prog --flag".encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated string this test owns.
    unsafe { drop_first(line.as_mut_ptr()) };
    let end = line.iter().position(|it| *it == 0).unwrap();
    assert_eq!(String::from_utf16(&line[..end]).unwrap(), "prog --flag");
}

#[test]
fn the_reserve_is_in_every_threads_block() {
    let here = reserve().unwrap();
    assert_eq!((here.size, here.align), (RESERVE_SIZE, ALIGN));
    assert!(here.bias >= 0);
    let there = std::thread::spawn(|| reserve().unwrap()).join().unwrap();
    assert_eq!(here, there);
}

fn reserve_bytes(len: usize) -> Vec<u8> {
    // SAFETY: the calling thread's reserve, `RESERVE_SIZE` bytes that live as long as the
    // thread; only read, and only `thread_locals_reach_every_thread` writes it.
    RESERVE.with(|it| unsafe { std::slice::from_raw_parts(it.0.get() as *const u8, len) }.to_vec())
}

#[test]
fn thread_locals_reach_every_thread() {
    let image = sample(GRANULE);
    let headers = parse::parse(&image).unwrap();
    let tls = parse::tls(&headers).unwrap().unwrap();
    let mut memory = image.clone();
    let at = memory.as_mut_ptr() as u64;
    thread_locals(&tls, at, &Here::find().unwrap()).unwrap();
    assert_eq!(le32(&memory, tls.index), u64::from(tls_index()));
    let mut want = memory[tls.start as usize..(tls.start + tls.init) as usize].to_vec();
    want.resize(tls.len() as usize, 0);
    assert_eq!(reserve_bytes(want.len()), want);
    let len = want.len();
    assert_eq!(std::thread::spawn(move || reserve_bytes(len)).join().unwrap(), want);
}
