//! The linker end to end: x86-64 ELF images staged, committed through a simulated disk and
//! read back with `object`; incremental links, rollbacks, live patch messages, positions,
//! start/stop sections and C objects.

use std::collections::BTreeMap;

use blaze_codegen::{Arch, CompiledFunction, Init, InstanceKey, Line, Reloc, RelocKind, Static, Target};
use blaze_core::{Completion, Done, Effect, FileName, IoError};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_live::patch::{Descriptor, Message, message_file};
use object::read::elf::ElfFile64;
use object::write::{Object as Builder, Relocation, StandardSection, Symbol as Def, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, Object as _, ObjectSection as _, ObjectSymbol as _, RelocationFlags,
    SymbolFlags, SymbolKind, SymbolScope, elf,
};

use crate::{COLUMN, Change, Debug, Format, Image, LINE, Live, LinkError, Native, NativeKind, Output, OutputKind, Position, SpanTable};

const TO: &str = "out/app";
const PAGE: u64 = 4096;

fn id(n: u8) -> ContentId {
    ContentId([n; 32])
}

fn key(n: u8) -> InstanceKey {
    InstanceKey(id(n))
}

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn output(incremental: bool) -> Output {
    Output {
        format: Format::Elf,
        arch: Arch::X86_64,
        kind: OutputKind::Executable { entry: "_start".into() },
        interpreter: Some("/lib64/ld-linux-x86-64.so.2".into()),
        platform: None,
        incremental,
        debug: Debug::None,
        live: None,
        tags: 1 << 32,
        windows_subsystem: false,
        stack: None,
    }
}

fn live() -> Output {
    Output { live: Some(Live { dir: name("live"), path: "/tmp/blaze-link-test/live".into() }), ..output(true) }
}

fn function(n: u8, code: &[u8], relocs: Vec<Reloc>) -> CompiledFunction {
    CompiledFunction {
        key: key(n),
        arch: Arch::X86_64,
        align: 16,
        code: code.to_vec(),
        relocs,
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
    }
}

/// A 32-bit field relative to its end, at `at`.
fn rel32(at: u32, target: Target) -> Reloc {
    Reloc { at, kind: RelocKind::PcRel32, target, addend: -4 }
}

/// `call rel32; ret`.
const CALLER: [u8; 6] = [0xe8, 0, 0, 0, 0, 0xc3];

/// `mov eax, value; ret`.
fn returns(value: u8) -> [u8; 6] {
    [0xb8, value, 0, 0, 0, 0xc3]
}

/// `_start` (1) calling `g` (2) through `target`, and `g` returning 1.
fn program(target: fn(InstanceKey) -> Target) -> (CompiledFunction, CompiledFunction) {
    (function(1, &CALLER, vec![rel32(1, target(key(2)))]), function(2, &returns(1), Vec::new()))
}

fn first<'a>(start: &'a CompiledFunction, g: &'a CompiledFunction) -> Change<'a> {
    Change {
        added: vec![start, g],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into()), (key(2), "g".into())],
        ..Change::default()
    }
}

/// `g` (2) replaced by `g2` (3), which returns 2, in `g`'s slot.
fn edit(g2: &CompiledFunction) -> Change<'_> {
    Change {
        added: vec![g2],
        removed: vec![key(2)],
        slots: vec![(key(2), key(3))],
        debug_names: vec![(key(3), "g2".into())],
        ..Change::default()
    }
}

/// A file as pages, most of an incremental image being room nothing has written.
#[derive(Clone, Default, Debug)]
struct Sparse {
    len: u64,
    pages: BTreeMap<u64, Vec<u8>>,
}

impl Sparse {
    fn write(&mut self, at: u64, bytes: &[u8]) {
        let mut done = 0;
        while done < bytes.len() {
            let addr = at + done as u64;
            let page = self.pages.entry(addr / PAGE).or_insert_with(|| vec![0; PAGE as usize]);
            let from = (addr % PAGE) as usize;
            let n = (PAGE as usize - from).min(bytes.len() - done);
            page[from..from + n].copy_from_slice(&bytes[done..done + n]);
            done += n;
        }
        self.len = self.len.max(at + bytes.len() as u64);
    }

    fn truncate(&mut self, len: u64) {
        self.pages.retain(|page, _| page * PAGE < len);
        if let Some(last) = self.pages.get_mut(&(len / PAGE)) {
            last[(len % PAGE) as usize..].fill(0);
        }
        self.len = len;
    }

    fn page(&self, page: u64) -> &[u8] {
        static ZERO: [u8; PAGE as usize] = [0; PAGE as usize];
        self.pages.get(&page).map_or(&ZERO[..], |it| &it[..])
    }

    /// Whether two files have the same bytes.
    fn same(&self, other: &Sparse) -> bool {
        self.len == other.len
            && self.pages.keys().chain(other.pages.keys()).all(|page| self.page(*page) == other.page(*page))
    }

    /// The whole file, in words: `object` reads ELF from 8-aligned bytes only.
    fn words(&self) -> Vec<u64> {
        let mut words = vec![0u64; self.len.div_ceil(8) as usize];
        for (page, bytes) in &self.pages {
            let start = page * PAGE;
            if start >= self.len {
                continue;
            }
            let n = (self.len - start).min(PAGE) as usize;
            for (at, chunk) in bytes[..n].chunks(8).enumerate() {
                let mut word = [0; 8];
                word[..chunk.len()].copy_from_slice(chunk);
                words[(start / 8) as usize + at] = u64::from_ne_bytes(word);
            }
        }
        words
    }
}

/// The disk a commit's effects run on: completed in reverse order, as a driver may.
#[derive(Default)]
struct Disk {
    files: BTreeMap<FileName, Sparse>,
    modes: BTreeMap<FileName, u32>,
    fail_rename: bool,
    /// Each effect run: its kind and the bytes it wrote.
    log: Vec<(&'static str, usize)>,
}

impl Disk {
    fn done(&mut self, effect: &Effect) -> Done {
        let missing = |file: &FileName| Done::Failed(IoError(format!("{file}: not there")));
        match effect {
            Effect::Remove { file, .. } => {
                self.log.push(("remove", 0));
                self.files.remove(file);
                self.modes.remove(file);
                Done::Removed
            }
            Effect::Truncate { file, len, .. } => {
                self.log.push(("truncate", 0));
                self.files.entry(file.clone()).or_default().truncate(*len);
                Done::Truncated
            }
            Effect::Clone { from, to, .. } => {
                self.log.push(("clone", 0));
                let Some(it) = self.files.get(from).cloned() else { return missing(from) };
                self.files.insert(to.clone(), it);
                Done::Cloned
            }
            Effect::WriteAt { file, at, bytes, .. } => {
                self.log.push(("write at", bytes.len()));
                let Some(it) = self.files.get_mut(file) else { return missing(file) };
                it.write(*at, bytes);
                Done::Written
            }
            Effect::Sync { .. } => Done::Synced,
            Effect::Mode { file, mode, .. } => {
                self.modes.insert(file.clone(), *mode);
                Done::Moded
            }
            Effect::Rename { .. } if self.fail_rename => Done::Failed(IoError("no space left on the device".into())),
            Effect::Rename { from, to, .. } => {
                self.log.push(("rename", 0));
                let Some(it) = self.files.remove(from) else { return missing(from) };
                self.files.insert(to.clone(), it);
                match self.modes.remove(from) {
                    Some(mode) => self.modes.insert(to.clone(), mode),
                    None => self.modes.remove(to),
                };
                Done::Renamed
            }
            Effect::Write { file, bytes, .. } => {
                self.log.push(("write", bytes.len()));
                let mut it = Sparse::default();
                it.write(0, bytes);
                self.files.insert(file.clone(), it);
                Done::Written
            }
            other => panic!("the linker asked for {other:?}"),
        }
    }

    /// Runs a commit's or a rollback's effects to its end.
    fn run(&mut self, image: &mut Image, mut effects: Vec<Effect>) -> Result<u64, LinkError> {
        loop {
            let effect = effects.pop().expect("the commit ended or asked for more");
            let tag = effect.tag();
            assert!(image.owns(tag), "{effect:?} is not the image's");
            assert!(tag.0 >> 32 == 1, "{effect:?} has the tag {:#x}", tag.0);
            let done = self.done(&effect);
            let mut more = Vec::new();
            if let Some(result) = image.complete(&Completion { tag, done }, &mut more) {
                assert!(more.is_empty() && effects.is_empty(), "effects after the commit ended");
                return result;
            }
            effects.extend(more);
        }
    }

    fn link(&mut self, image: &mut Image, change: Change<'_>) -> Result<u64, LinkError> {
        let staged = image.stage(change)?;
        let mut effects = Vec::new();
        image.commit(staged, &mut effects)?;
        self.run(image, effects)
    }

    fn roll_back(&mut self, image: &mut Image) -> Result<u64, LinkError> {
        let mut effects = Vec::new();
        image.roll_back(&mut effects)?;
        self.run(image, effects)
    }

    fn file(&self, it: &str) -> &Sparse {
        self.files.get(&name(it)).unwrap_or_else(|| panic!("no file {it}"))
    }
}

/// A linked file read back.
struct Linked {
    words: Vec<u64>,
    len: usize,
}

impl Linked {
    fn of(file: &Sparse) -> Linked {
        Linked { words: file.words(), len: file.len as usize }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `words` holds `len.div_ceil(8)` initialized words, so at least `len` bytes,
        // and u8 has no alignment.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.len) }
    }

    fn elf(&self) -> ElfFile64<'_, Endianness> {
        ElfFile64::<Endianness>::parse(self.bytes()).unwrap()
    }

    /// A `.symtab` symbol's address.
    fn symbol(&self, name: &str) -> u64 {
        let elf = self.elf();
        let found = elf.symbols().find(|it| it.name().ok() == Some(name));
        found.unwrap_or_else(|| panic!("no symbol {name}")).address()
    }

    fn dynamic_symbol(&self, name: &str) -> u64 {
        let elf = self.elf();
        let found = elf.dynamic_symbols().find(|it| it.name().ok() == Some(name));
        found.unwrap_or_else(|| panic!("no dynamic symbol {name}")).address()
    }

    /// Bytes at an address: a region's file offset is its address.
    fn at(&self, addr: u64, len: usize) -> &[u8] {
        &self.bytes()[addr as usize..][..len]
    }

    fn u64_at(&self, addr: u64) -> u64 {
        u64::from_le_bytes(self.at(addr, 8).try_into().unwrap())
    }

    fn u32_at(&self, addr: u64) -> u32 {
        u32::from_le_bytes(self.at(addr, 4).try_into().unwrap())
    }

    /// What the 32-bit field at `field`, relative to its end, points at.
    fn rel32(&self, field: u64) -> u64 {
        let value = i32::from_le_bytes(self.at(field, 4).try_into().unwrap());
        (field as i64 + 4 + i64::from(value)) as u64
    }

    /// Where the x86-64 jump-table stub at `stub` jumps: through the cell holding the
    /// table's address, to the slot's entry of the table.
    fn through_stub(&self, stub: u64) -> u64 {
        assert_eq!(self.at(stub, 3), [0x4c, 0x8b, 0x1d], "mov r11, [rip + cell]");
        let cell = self.rel32(stub + 3);
        assert_eq!(self.at(stub + 7, 3), [0x41, 0xff, 0xa3], "jmp [r11 + slot]");
        let slot = u64::from(self.u32_at(stub + 10));
        self.u64_at(self.u64_at(cell) + slot)
    }
}

#[test]
fn a_full_link_is_an_executable_whose_calls_reach_their_callees() {
    let (start, g) = program(Target::Function);
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    assert_eq!(disk.link(&mut image, first(&start, &g)), Ok(1));
    assert_eq!(image.generation(), 1);
    assert_eq!(image.slot(&key(2)), None, "a release image calls directly");
    assert!(!disk.files.contains_key(&name("out/app.blaze-next")));
    assert_eq!(disk.modes.get(&name(TO)), Some(&0o755));

    let linked = Linked::of(disk.file(TO));
    let elf = linked.elf();
    let (s, g) = (linked.symbol("start"), linked.symbol("g"));
    assert_eq!(elf.entry(), s);
    assert_eq!(linked.at(s, 6)[0], 0xe8);
    assert_eq!(linked.rel32(s + 1), g);
    assert_eq!(linked.at(g, 6), returns(1));
    let interp = elf.section_by_name(".interp").unwrap();
    assert_eq!(interp.data().unwrap(), b"/lib64/ld-linux-x86-64.so.2\0");
}

#[test]
fn an_image_is_the_same_whatever_order_its_functions_came_in() {
    let (start, g) = program(Target::Function);
    let mut a = Image::new(name(TO), output(true)).unwrap();
    let mut b = Image::new(name(TO), output(true)).unwrap();
    let (mut one, mut two) = (Disk::default(), Disk::default());
    one.link(&mut a, first(&start, &g)).unwrap();
    let mut reversed = first(&start, &g);
    reversed.added.reverse();
    reversed.debug_names.reverse();
    two.link(&mut b, reversed).unwrap();
    assert!(one.file(TO).same(two.file(TO)));
}

#[test]
fn an_incremental_link_puts_the_new_function_in_the_old_slot_and_writes_little() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &g)).unwrap();
    let before = disk.file(TO).clone();
    let a = Linked::of(&before);
    let (s, old) = (a.symbol("start"), a.symbol("g"));
    let stub = a.rel32(s + 1);
    assert_ne!(stub, old, "an incremental image calls through the stub");
    assert_eq!(a.through_stub(stub), old);
    let slot = image.slot(&key(2)).unwrap();

    disk.log.clear();
    assert_eq!(disk.link(&mut image, edit(&g2)), Ok(2));
    let kinds: Vec<&str> = disk.log.iter().map(|it| it.0).collect();
    assert!(kinds.contains(&"clone"), "an incremental link writes into a clone: {kinds:?}");
    let written: usize = disk.log.iter().map(|it| it.1).sum();
    assert!((written as u64) < disk.file(TO).len / 16, "{written} bytes of {}", disk.file(TO).len);
    assert_eq!(disk.modes.get(&name(TO)), Some(&0o755));

    let b = Linked::of(disk.file(TO));
    let new = b.symbol("g2");
    assert_ne!(new, old, "new code never goes where old code was");
    assert_eq!(b.at(s, 6), a.at(s, 6), "the caller is not rewritten");
    assert_eq!(b.through_stub(stub), new);
    assert_eq!(b.at(new, 6), returns(2));
    assert_eq!(b.at(old, 6), returns(1), "a running program may still be in the old code");
    assert_eq!(image.slot(&key(2)), Some(slot));
}

#[test]
fn a_rollback_gives_back_the_file_before_the_commit() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &g)).unwrap();
    let before = disk.file(TO).clone();
    disk.link(&mut image, edit(&g2)).unwrap();
    assert!(!disk.file(TO).same(&before));
    assert_eq!(disk.roll_back(&mut image), Ok(1));
    assert_eq!(image.generation(), 1);
    assert!(disk.file(TO).same(&before));
    assert!(disk.roll_back(&mut image).is_err(), "nothing before the first link");

    // The edit applies again after the rollback, at a generation never given out before.
    let generation = disk.link(&mut image, edit(&g2)).unwrap();
    assert!(generation > 2);
    let linked = Linked::of(disk.file(TO));
    assert_eq!(linked.through_stub(linked.rel32(linked.symbol("start") + 1)), linked.symbol("g2"));
}

#[test]
fn a_commit_that_fails_leaves_the_image_as_it_was() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &g)).unwrap();
    let before = disk.file(TO).clone();
    disk.fail_rename = true;
    let error = disk.link(&mut image, edit(&g2)).unwrap_err();
    assert!(error.what.contains("no space left"), "{error}");
    assert_eq!(image.generation(), 1);
    assert!(disk.file(TO).same(&before));

    disk.fail_rename = false;
    let generation = disk.link(&mut image, edit(&g2)).unwrap();
    assert_eq!(image.generation(), generation);
    let linked = Linked::of(disk.file(TO));
    assert_eq!(linked.through_stub(linked.rel32(linked.symbol("start") + 1)), linked.symbol("g2"));
}

#[test]
fn a_removed_function_still_called_through_its_slot_is_an_error() {
    let (start, g) = program(Target::Function);
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &g)).unwrap();
    let error = image.stage(Change { removed: vec![key(2)], ..Change::default() }).unwrap_err();
    assert!(error.what.contains("still calls it"), "{error}");
    assert_eq!(error.function, Some(key(1)), "the error names the caller");
    assert_eq!(image.generation(), 1);

    let g2 = function(3, &returns(2), Vec::new());
    let wrong = Change { added: vec![&g2], slots: vec![(key(2), key(4))], ..Change::default() };
    assert!(image.stage(wrong).is_err(), "a slot given a function that is not linked");
    assert_eq!(disk.link(&mut image, edit(&g2)), Ok(2));
}

#[test]
fn slots_need_a_jump_table() {
    let (start, g) = program(Target::Direct);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut change = first(&start, &g);
    change.added.push(&g2);
    change.slots.push((key(2), key(3)));
    let error = image.stage(change).unwrap_err();
    assert!(error.what.contains("without a jump table"), "{error}");
}

#[test]
fn a_live_image_names_its_table_and_patches_say_what_changed() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), live()).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &g)).unwrap();
    assert!(!disk.files.keys().any(|it| it.as_str().starts_with("live/")), "nothing runs the first link");

    let a = Linked::of(disk.file(TO));
    let at = a.dynamic_symbol(blaze_live::patch::DESCRIPTOR);
    let descriptor = Descriptor::decode(a.at(at, Descriptor::SIZE)).unwrap();
    assert_eq!(descriptor.this, at);
    assert_eq!(descriptor.generation, 1);
    let stub = a.rel32(a.symbol("start") + 1);
    assert_eq!(descriptor.table, a.rel32(stub + 3), "the table cell the stubs load");
    assert!(descriptor.slots >= 2);
    let [code, end] = descriptor.patch_code;
    assert!(code < end);
    let [dir, len] = descriptor.dir;
    assert_eq!(a.at(dir, len as usize), b"/tmp/blaze-link-test/live");

    disk.link(&mut image, edit(&g2)).unwrap();
    let b = Linked::of(disk.file(TO));
    let new = b.symbol("g2");
    assert!((code..end).contains(&new));
    let message = disk.file(&format!("live/{}", message_file(1)));
    let words = message.words();
    let bytes = &Linked { words, len: message.len as usize };
    let Message::Patch(patch) = Message::decode(bytes.bytes()).unwrap() else { panic!("not a patch") };
    assert_eq!((patch.id, patch.base, patch.generation), (descriptor.id, 1, 2));
    assert_eq!(patch.code, vec![(new, returns(2).to_vec())]);
    let slot = image.slot(&key(2)).unwrap();
    assert!(patch.cells.contains(&(slot.0, new)), "{:?}", patch.cells);
    assert!(patch.frames.is_empty() && patch.windows.is_empty());
    let after = Descriptor::decode(b.at(at, Descriptor::SIZE)).unwrap();
    assert_eq!((after.id, after.generation), (descriptor.id, 2), "a program started now starts at 2");

    disk.roll_back(&mut image).unwrap();
    assert!(!disk.files.contains_key(&name(&format!("live/{}", message_file(1)))));
    let message = disk.file(&format!("live/{}", message_file(2)));
    let bytes = &Linked { words: message.words(), len: message.len as usize };
    assert_eq!(Message::decode(bytes.bytes()).unwrap(), Message::RollBack { id: descriptor.id, to: 1 });
}

#[test]
fn positions_are_filled_from_span_tables_and_a_reformat_rewrites_only_them() {
    let anchor = id(50);
    let table = |line| SpanTable {
        files: vec!["src/main.rs".into()],
        spans: vec![Position { file: 0, line: 1, column: 1 }, Position { file: 0, line, column: 9 }],
    };
    // `lea rax, [rip + location]; ret`
    let start = function(
        1,
        &[0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3],
        vec![rel32(3, Target::Position(SpanRef { anchor, index: 1 }))],
    );
    let spans = table(7).encode();
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        spans: vec![(anchor, &spans)],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let a = Linked::of(disk.file(TO));
    let s = a.symbol("start");
    let location = a.rel32(s + 3);
    let file = a.u64_at(location);
    assert_eq!(a.u64_at(location + 8), 11);
    assert_eq!(a.u32_at(location + LINE as u64), 7);
    assert_eq!(a.u32_at(location + COLUMN as u64), 9);
    assert_eq!(a.at(file, 12), b"src/main.rs\0");

    let spans = table(8).encode();
    disk.link(&mut image, Change { spans: vec![(anchor, &spans)], ..Change::default() }).unwrap();
    let b = Linked::of(disk.file(TO));
    assert_eq!(b.at(s, 8), a.at(s, 8), "no code is rewritten");
    assert_eq!(b.u32_at(location + LINE as u64), 8);
    assert_eq!(b.u64_at(location), file);

    let missing = function(4, &[0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3], vec![rel32(3, Target::Position(SpanRef { anchor: id(51), index: 0 }))]);
    let error = image.stage(Change { added: vec![&missing], ..Change::default() }).unwrap_err();
    assert!(error.what.contains("span table"), "{error}");
}

#[test]
fn start_and_stop_symbols_bound_a_named_section() {
    let item = |n: u8, value: u8| Static {
        item: ItemKey(id(n)),
        instance: id(n + 100),
        align: 8,
        init: Init::Bytes { bytes: vec![value; 8], relocs: Vec::new() },
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: Some("linkme_ITEMS".into()),
    };
    let (one, two) = (item(10, 1), item(11, 2));
    // `lea rax, [rip + start]; lea rcx, [rip + stop]; ret`
    let code = [0x48, 0x8d, 0x05, 0, 0, 0, 0, 0x48, 0x8d, 0x0d, 0, 0, 0, 0, 0xc3];
    let start = function(
        1,
        &code,
        vec![
            rel32(3, Target::Extern("__start_linkme_ITEMS".into())),
            rel32(10, Target::Extern("__stop_linkme_ITEMS".into())),
        ],
    );
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        statics: vec![&one, &two],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let s = linked.symbol("start");
    let (begin, end) = (linked.rel32(s + 3), linked.rel32(s + 10));
    assert_eq!(end - begin, 16);
    assert_eq!(linked.at(begin, 16), [[1; 8], [2; 8]].concat());
    let elf = linked.elf();
    let section = elf.section_by_name("linkme_ITEMS").unwrap();
    assert_eq!((section.address(), section.size()), (begin, 16));

    let code = [0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3];
    let lonely = function(1, &code, vec![rel32(3, Target::Extern("__start_missing".into()))]);
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let change = Change {
        added: vec![&lonely],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        ..Change::default()
    };
    let error = image.stage(change).unwrap_err();
    assert!(error.what.contains("section `missing`"), "{error}");
}

/// An x86-64 ELF object defining `c_helper`, which calls `rust_fn`.
fn c_object() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = b.section_id(StandardSection::Text);
    let at = b.append_section_data(text, &CALLER, 16);
    b.add_symbol(Def {
        name: "c_helper".into(),
        value: at,
        size: 6,
        kind: SymbolKind::Text,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    let rust = b.add_symbol(Def {
        name: "rust_fn".into(),
        value: 0,
        size: 0,
        kind: SymbolKind::Unknown,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    b.add_relocation(text, Relocation {
        offset: at + 1,
        symbol: rust,
        addend: -4,
        flags: RelocationFlags::Elf { r_type: elf::R_X86_64_PLT32 },
    })
    .unwrap();
    b.write().unwrap()
}

#[test]
fn a_c_object_and_rust_code_call_each_other_by_name() {
    let object = c_object();
    let start = function(1, &CALLER, vec![rel32(1, Target::Extern("c_helper".into()))]);
    let rust = function(2, &returns(3), Vec::new());
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start, &rust],
        names: vec![("_start".into(), Target::Direct(key(1))), ("rust_fn".into(), Target::Direct(key(2)))],
        debug_names: vec![(key(1), "start".into()), (key(2), "rust".into())],
        natives: vec![Native { id: id(60), name: "helper.o", bytes: &object, kind: NativeKind::Object, order: 0 }],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let helper = linked.rel32(linked.symbol("start") + 1);
    assert_eq!(linked.at(helper, 6)[0], 0xe8, "c_helper's code");
    assert_eq!(linked.rel32(helper + 1), linked.symbol("rust"));

    let mut image = Image::new(name(TO), output(false)).unwrap();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        natives: vec![Native { id: id(60), name: "helper.o", bytes: &object, kind: NativeKind::Object, order: 0 }],
        ..Change::default()
    };
    let error = image.stage(change).unwrap_err();
    assert!(error.what.contains("undefined symbol `rust_fn`") && error.what.contains("helper.o"), "{error}");
}

#[test]
fn line_tables_are_written_from_the_span_tables() {
    let anchor = id(70);
    let spans = SpanTable { files: vec!["src/lib.rs".into()], spans: vec![Position { file: 0, line: 4, column: 1 }] }.encode();
    let mut start = function(1, &returns(0), Vec::new());
    start.lines = vec![Line { at: 0, span: SpanRef { anchor, index: 0 } }];
    let mut image = Image::new(name(TO), Output { debug: Debug::Lines, ..output(false) }).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        spans: vec![(anchor, &spans)],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let elf = linked.elf();
    let lines = elf.section_by_name(".debug_line").unwrap();
    assert!(!lines.data().unwrap().is_empty());
}
