use super::*;

const SMALL: Bounds = Bounds { stack: 1 << 16, heap: 16 << 20 };

fn function(name: &str, len: usize, relocs: Vec<CodeReloc>, entry: Option<Sig>) -> Function {
    Function {
        name: name.to_owned(),
        bytes: vec![0; len],
        align: 16,
        relocs,
        entry,
        frame: 16,
        calls: vec![Call { ret: 4, landing: Some(12) }],
        traps: vec![(8, ir::TrapCode::HEAP_OUT_OF_BOUNDS)],
    }
}

fn abs8(at: u32, to: To, addend: i64) -> CodeReloc {
    CodeReloc { at, kind: Kind::Abs8, to, addend }
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[test]
fn the_writer_resolves_as_a_static_linker_does() {
    let base = 0x10_0000u64;
    let mut code = vec![0u8; 64 + 2 * VENEER];
    let mut writer = Writer { code: &mut code, base, next_veneer: 64 };
    writer.relocate(0, Kind::Abs8, 0x1234, 8).unwrap();
    writer.relocate(8, Kind::X86PCRel4, base + 40, -4).unwrap();
    writer.relocate(12, Kind::X86CallPCRel4, base + 32, -4).unwrap();
    writer.relocate(16, Kind::X86CallPCRel4, base + (8 << 30), -4).unwrap();
    assert_eq!(word(&code, 0), 0x123c);
    assert_eq!(i32::from_le_bytes(code[8..12].try_into().unwrap()), 40 - 4 - 8);
    assert_eq!(i32::from_le_bytes(code[12..16].try_into().unwrap()), 32 - 4 - 12);
    let disp = i32::from_le_bytes(code[16..20].try_into().unwrap());
    assert_eq!(i64::from(disp), 64 - 4 - 16, "a call out of reach goes through the first veneer");
    let target = if cfg!(target_arch = "x86_64") { word(&code, 64 + 6) } else { word(&code, 64 + 8) };
    assert_eq!(target, base + (8 << 30), "the veneer holds the target");
}

#[test]
fn an_aarch64_call_patches_its_26_bits_and_keeps_the_opcode() {
    let base = 0x40_0000u64;
    let mut code = vec![0u8; 16 + VENEER];
    code[0..4].copy_from_slice(&0x9400_0000u32.to_le_bytes());
    let mut writer = Writer { code: &mut code, base, next_veneer: 16 };
    writer.relocate(0, Kind::Arm64Call, base + 8, 0).unwrap();
    assert_eq!(u32::from_le_bytes(code[0..4].try_into().unwrap()), 0x9400_0002);
    let mut writer = Writer { code: &mut code, base, next_veneer: 16 };
    assert!(writer.relocate(0, Kind::Arm64Call, base + 2, 0).is_err(), "an unaligned target");
}

#[test]
fn a_relocation_confined_code_never_has_is_an_error() {
    let mut code = vec![0u8; 16];
    let mut writer = Writer { code: &mut code, base: 0x1000, next_veneer: 16 };
    let what = writer.relocate(0, Kind::X86GOTPCRel4, 0x2000, 0).unwrap_err();
    assert!(what.contains("X86GOTPCRel4"), "{what}");
}

#[test]
fn a_load_places_the_code_the_table_and_the_image() {
    let sig = Sig(3);
    let caller = function(
        "caller",
        48,
        vec![
            abs8(0, To::Pointer(1), 0),
            abs8(8, To::Blob(0), 4),
            abs8(16, To::Table, 0),
            abs8(24, To::TableSize, 0),
            abs8(32, To::Guest(Layout::HEADER), 0),
        ],
        None,
    );
    let callee = function("callee", 32, Vec::new(), Some(sig));
    let blobs = vec![
        Blob { align: 8, init: Init::Bytes(vec![7; 16]), writable: false, relocs: vec![(8, To::Pointer(1), 0)] },
        Blob { align: 16, init: Init::Zeroed(32), writable: true, relocs: Vec::new() },
    ];
    let image = Image::load(Input { functions: vec![caller, callee], blobs, bounds: SMALL }).unwrap();

    assert_eq!(image.pointer(0), None);
    assert_eq!(image.pointer(1), Some(16));
    let callee_at = image.code(1).unwrap();
    assert_eq!(image.table().target(16, sig), Ok(callee_at), "the entry is the callee's code");
    assert_eq!(image.table().size(), 32);

    assert_eq!(&image.readonly[..8], &[7; 8]);
    assert_eq!(word(&image.readonly, 8), 16, "a pointer in data is its table pointer");
    assert_eq!(image.writable.len(), 32);
    assert!(image.writable.iter().all(|it| *it == 0));
    assert!(image.layout.readonly.end - image.layout.readonly.start >= 16);

    let (placed, name) = image.function_at(callee_at + 4).unwrap();
    assert_eq!((name, placed.start, placed.len, placed.frame), ("callee", callee_at, 32, 16));
    assert_eq!(placed.calls, [Call { ret: 4, landing: Some(12) }]);
    assert!(image.contains(callee_at));
    assert!(image.function_at(callee_at + 32).is_none(), "past the callee is padding or a veneer");
    let caller_at = image.code(0).unwrap();
    assert!(caller_at < callee_at && callee_at % 16 == 0);
}

#[test]
fn what_a_load_cant_resolve_is_named() {
    let taken = function("f", 16, vec![abs8(0, To::Pointer(1), 0)], None);
    let untaken = function("g", 16, Vec::new(), None);
    let Err(Stop(message)) = Image::load(Input { functions: vec![taken, untaken], blobs: Vec::new(), bounds: SMALL })
    else {
        panic!("the address of a function with no entry")
    };
    assert!(message.contains("`g`") && message.contains("no table entry"), "{message}");

    let past = function("h", 8, vec![abs8(4, To::Guest(0), 0)], None);
    let Err(Stop(message)) = Image::load(Input { functions: vec![past], blobs: Vec::new(), bounds: SMALL }) else {
        panic!("a relocation past the code")
    };
    assert!(message.contains("`h`") && message.contains("past its 8 bytes"), "{message}");

    let aligned = Blob { align: GRANULE * 2, init: Init::Zeroed(8), writable: true, relocs: Vec::new() };
    assert!(Image::load(Input { functions: Vec::new(), blobs: vec![aligned], bounds: SMALL }).is_err());
}
