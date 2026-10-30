//! The linker end to end: x86-64 ELF images staged, committed through a simulated disk and
//! read back with `object`; incremental links, rollbacks, live patch messages, positions,
//! start/stop sections and C objects.

use std::collections::BTreeMap;

use blaze_codegen::{Arch, CompiledFunction, Covered, Data, Init, InstanceKey, Line, RelocKind, Relocs, Static, Target, Unwind};
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
        dev_run: None,
        exec_log: false,
        sanitize: blaze_codegen::settings::Sanitize::default(),
        rpaths: Vec::new(),
        triple: "x86_64-unknown-linux-gnu".into(),
        stack: None,
    }
}

fn live() -> Output {
    Output { live: Some(Live { dir: name("live"), path: "/tmp/blaze-link-test/live".into() }), ..output(true) }
}

fn function(n: u8, code: &[u8], relocs: Vec<blaze_codegen::Relocation>) -> CompiledFunction {
    CompiledFunction {
        key: key(n),
        arch: Arch::X86_64,
        align: 16,
        code: code.to_vec(),
        relocs: relocs.into(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    }
}

/// A 32-bit field relative to its end, at `at`.
fn rel32(at: u32, target: Target) -> blaze_codegen::Relocation {
    blaze_codegen::Relocation { at, kind: RelocKind::PcRel32, target, addend: -4 }
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
        &zerocopy::IntoBytes::as_bytes(&self.words[..])[..self.len]
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
fn bytes_gives_the_file_a_commit_writes_and_rolls_back_like_one() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut kept = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    let as_file = |bytes: &[u8]| {
        let mut file = Sparse::default();
        file.write(0, bytes);
        file
    };

    disk.link(&mut image, first(&start, &g)).unwrap();
    let staged = kept.stage(first(&start, &g)).unwrap();
    let len = staged.len();
    let full = kept.bytes(staged).unwrap();
    assert_eq!(full.len() as u64, len);
    assert!(as_file(&full).same(disk.file(TO)), "a full link's bytes are its file's");
    assert_eq!(kept.generation(), image.generation());

    disk.link(&mut image, edit(&g2)).unwrap();
    let edited = kept.stage(edit(&g2)).unwrap();
    let edited = kept.bytes(edited).unwrap();
    assert!(as_file(&edited).same(disk.file(TO)), "an incremental link's bytes are the whole file");

    let mut rolled = Disk::default();
    rolled.files.insert(name(TO), as_file(&edited));
    let mut effects = Vec::new();
    kept.roll_back(&mut effects).unwrap();
    assert_eq!(rolled.run(&mut kept, effects), Ok(1), "bytes keeps the history a commit keeps");
    assert!(rolled.file(TO).same(&as_file(&full)), "the rollback gives back the first link's file");
    let again = kept.stage(edit(&g2)).unwrap();
    assert!(kept.bytes(again).is_ok(), "the edit applies again after the rollback");

    let tables = kept.tables();
    let relocations = tables.iter().find(|it| it.0 == "relocation").expect("a relocation table");
    assert!(relocations.1 >= 1 && relocations.2 >= relocations.1, "{tables:?}");
}

#[test]
fn a_function_instrumented_for_pgo_is_refused_until_the_link_places_its_counters() {
    let (start, g) = program(Target::Function);
    let record = blaze_codegen::ProfileRecord { name: [7; 16], name_ref: 1, hash: 2, counters: 1, version: 10 };
    let counted = CompiledFunction { profile: Some(blaze_codegen::Profiled::Counted(record)), ..g.clone() };
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let error = image.stage(first(&start, &counted)).err().expect("an instrumented function is refused");
    assert_eq!(error.function, Some(key(2)));
    assert!(error.what.contains("-Cprofile-generate"), "{error}");

    let naming = function(2, &returns(1), vec![rel32(1, Target::ProfileCounters)]);
    let error = image.stage(first(&start, &naming)).err().expect("naming PGO counters is refused");
    assert_eq!(error.function, Some(key(2)));
}

#[test]
fn bytes_refuses_a_live_link_that_tells_running_programs() {
    let (start, g) = program(Target::Function);
    let g2 = function(3, &returns(2), Vec::new());
    let mut image = Image::new(name(TO), live()).unwrap();
    let staged = image.stage(first(&start, &g)).unwrap();
    assert!(image.bytes(staged).is_ok(), "the first link tells no program");
    let staged = image.stage(edit(&g2)).unwrap();
    let error = image.bytes(staged).unwrap_err();
    assert!(error.what.contains("tells running programs"), "{error}");
    assert_eq!(image.generation(), 1, "a refused link leaves the image as it was");
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

/// `lea rax, [rip + literal]; ret`, the literal its own data, and a call frame entry.
fn framed(n: u8) -> CompiledFunction {
    let mut it = function(n, &[0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3], vec![rel32(3, Target::Data(id(60)))]);
    it.data = vec![Data { id: id(60), align: 1, bytes: b"literal".to_vec(), relocs: Default::default() }];
    it.unwind = Some(Unwind::Dwarf {
        code_align: 1,
        data_align: -8,
        return_address: 16,
        cie: vec![0x0c, 7, 8, 0x90, 1],
        fde: vec![0x41, 0x0e, 0x10],
        personality: None,
        lsda: None,
    });
    it
}

/// Of each FDE in a call frame section at `base`, the address it covers from (`pcrel|sdata4`
/// at offset 8, as the core writes them), up to its zero length.
fn fde_starts(base: u64, bytes: &[u8]) -> Vec<u64> {
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= bytes.len() && word(at) != 0 {
        if word(at + 4) != 0 {
            let field = base + at as u64 + 8;
            out.push((field as i64 + i64::from(word(at + 8) as i32)) as u64);
        }
        at += 4 + word(at) as usize;
    }
    out
}

#[test]
fn a_patch_puts_its_frames_and_literals_in_the_patch_regions_and_ends_its_frames() {
    let (start, g) = program(Target::Function);
    let g2 = framed(3);
    let mut image = Image::new(name(TO), live()).unwrap();
    let mut disk = Disk::default();
    disk.link(&mut image, first(&start, &g)).unwrap();
    let a = Linked::of(disk.file(TO));
    let descriptor = Descriptor::decode(a.at(a.dynamic_symbol(blaze_live::patch::DESCRIPTOR), Descriptor::SIZE)).unwrap();
    let ([code, code_end], [data, data_end]) = (descriptor.patch_code, descriptor.patch_data);

    disk.link(&mut image, edit(&g2)).unwrap();
    let b = Linked::of(disk.file(TO));
    let new = b.symbol("g2");
    let literal = b.rel32(new + 3);
    assert!((data..data_end).contains(&literal), "the literal at {literal:#x}, in the patch data region");
    assert_eq!(b.at(literal, 7), b"literal");
    let message = disk.file(&format!("live/{}", message_file(1)));
    let bytes = &Linked { words: message.words(), len: message.len as usize };
    let Message::Patch(patch) = Message::decode(bytes.bytes()).unwrap() else { panic!("not a patch") };
    let [(fde, len)] = patch.frames[..] else { panic!("one FDE: {:?}", patch.frames) };
    assert!(code <= new && new < fde && fde + len + 4 <= code_end, "the FDE after the code, in the patch code region");
    assert_eq!(fde_starts(fde, b.at(fde, len as usize)), vec![new], "the FDE covers g2");
    let cie = fde + 4 - u64::from(b.u32_at(fde + 4));
    assert!(new < cie && cie < fde, "the new CIE in the patch code region, before the FDE");
    let written = |at: u64| patch.code.iter().find(|it| it.0 == at).map(|it| &it.1[..]);
    assert!(written(cie).is_some() && written(fde).is_some(), "the CIE and the FDE are code writes");
    assert_eq!(written(fde + len), Some(&[0u8; 4][..]), "the patch's frames end in their own zero length");
    let file = b.elf();
    for (at, bytes) in &patch.data {
        let section = file.sections().find(|it| (it.address()..it.address() + it.size()).contains(at)).unwrap();
        let object::SectionFlags::Elf { sh_flags } = section.flags() else { panic!("an ELF section") };
        assert!(sh_flags & u64::from(elf::SHF_WRITE) != 0, "{} bytes written at {at:#x}, in {}", bytes.len(), section.name().unwrap());
    }

    // A full link of the same functions puts the FDE in `.eh_frame`.
    let start = function(1, &CALLER, vec![rel32(1, Target::Function(key(3)))]);
    let mut full = Image::new(name(TO), live()).unwrap();
    let mut other = Disk::default();
    let change = Change {
        added: vec![&start, &g2],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into()), (key(3), "g2".into())],
        ..Change::default()
    };
    other.link(&mut full, change).unwrap();
    let c = Linked::of(other.file(TO));
    let file = c.elf();
    let eh_frame = file.section_by_name(".eh_frame").unwrap();
    assert_eq!(fde_starts(eh_frame.address(), eh_frame.data().unwrap()), vec![c.symbol("g2")]);
}

/// `lea rax, [rip + array + addend]` of its own SanitizerCoverage array, then `ret`.
fn covered(n: u8, array: u32, addend: i64, coverage: Covered) -> CompiledFunction {
    let mut reloc = rel32(3, Target::Coverage(array));
    reloc.addend += addend;
    let mut it = function(n, &[0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3], vec![reloc]);
    it.coverage = Some(coverage);
    it
}

/// A section's address and size.
fn section_of(linked: &Linked, name: &str) -> (u64, u64) {
    let file = linked.elf();
    let it = file.section_by_name(name).unwrap_or_else(|| panic!("no section {name}"));
    (it.address(), it.size())
}

/// The `(pc, flags)` entries of `__sancov_pcs`.
fn pcs(linked: &Linked) -> Vec<(u64, u64)> {
    let (at, size) = section_of(linked, "__sancov_pcs");
    (0..size / 16).map(|n| (linked.u64_at(at + 16 * n), linked.u64_at(at + 16 * n + 8))).collect()
}

#[test]
fn coverage_arrays_and_pcs_are_laid_out_in_the_functions_order() {
    let start = covered(1, 1, 3, Covered { counters: 3, flags: 5, guards: 0, pcs: vec![(0, 1), (7, 0)] });
    let g = covered(2, 0, 1, Covered { counters: 2, flags: 2, guards: 8, pcs: vec![(0, 1), (3, 0), (7, 0)] });
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&g, &start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into()), (key(2), "g".into())],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let linked = Linked::of(disk.file(TO));
    let (s, g) = (linked.symbol("start"), linked.symbol("g"));
    assert!(s < g, "the functions in their keys' order");
    let (counters, counters_len) = section_of(&linked, "__sancov_cntrs");
    let (flags, flags_len) = section_of(&linked, "__sancov_bools");
    let (guards, guards_len) = section_of(&linked, "__sancov_guards");
    assert_eq!((counters_len, flags_len, guards_len), (5, 7, 8));
    assert_eq!(linked.rel32(s + 3), flags + 3, "start's `Coverage(1)` + 3: its flags, 3 in");
    assert_eq!(linked.rel32(g + 3), counters + 3 + 1, "g's counters after start's 3");
    assert_eq!(guards % 4, 0);
    assert_eq!(linked.at(counters, 5), [0; 5]);
    assert_eq!(linked.at(flags, 7), [0; 7]);
    assert_eq!(pcs(&linked), vec![(s, 1), (s + 7, 0), (g, 1), (g + 3, 0), (g + 7, 0)]);

    let none = covered(1, 2, 0, Covered { counters: 1, flags: 0, guards: 0, pcs: Vec::new() });
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let change = Change { added: vec![&none], names: vec![("_start".into(), Target::Direct(key(1)))], ..Change::default() };
    let error = image.stage(change).unwrap_err();
    assert!(error.what.contains("SanitizerCoverage array 2"), "an array the function has not: {error}");
}

#[test]
fn a_relink_that_drops_a_covered_function_drops_its_entries() {
    // `lea rax, [rip + counters]; call h; ret`
    let mut start = function(
        1,
        &[0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0, 0xc3],
        vec![rel32(3, Target::Coverage(0)), rel32(8, Target::Function(key(3)))],
    );
    start.coverage = Some(Covered { counters: 1, flags: 0, guards: 0, pcs: vec![(0, 1)] });
    let g = covered(2, 0, 0, Covered { counters: 2, flags: 0, guards: 0, pcs: vec![(0, 1), (7, 0)] });
    let h = covered(3, 0, 0, Covered { counters: 4, flags: 0, guards: 0, pcs: vec![(0, 1)] });
    let mut image = Image::new(name(TO), output(true)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start, &g, &h],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into()), (key(2), "g".into()), (key(3), "h".into())],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let a = Linked::of(disk.file(TO));
    let (s, g_at, h_at) = (a.symbol("start"), a.symbol("g"), a.symbol("h"));
    let (counters, len) = section_of(&a, "__sancov_cntrs");
    assert_eq!(len, 7);
    assert_eq!(a.rel32(h_at + 3), counters + 3, "h's counters after start's and g's");
    assert_eq!(pcs(&a), vec![(s, 1), (g_at, 1), (g_at + 7, 0), (h_at, 1)]);

    disk.link(&mut image, Change { removed: vec![key(2)], ..Change::default() }).unwrap();
    let b = Linked::of(disk.file(TO));
    assert_eq!(section_of(&b, "__sancov_cntrs"), (counters, 5), "g's counters are gone");
    assert_eq!(b.rel32(s + 3), counters);
    assert_eq!(b.rel32(h_at + 3), counters + 1, "h's counters moved down, and h was relocated to them");
    assert_eq!(pcs(&b), vec![(s, 1), (h_at, 1)], "g's PC entries are gone, the rest in order");
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
        init: Init::Bytes { bytes: vec![value; 8], relocs: Relocs::new() },
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: Some("linkme_ITEMS".into()),
        imports: Vec::new(),
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
    let object: std::sync::Arc<[u8]> = c_object().into();
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

/// What an incremental stage of a change to one function may allocate, whatever the image
/// holds: the state's copied chunk tables and pages, arena chunks, the column and lists it
/// rebuilds, and the writes it hands the commit.
const STAGE_BUDGET: u64 = 400;

/// What growing the image or the changed function may add to a stage's allocations: a few
/// buffers growing once more, never one per function or per fixup.
const GROWTH: u64 = 64;

/// A key `key` never makes, for images of many functions.
fn wide_key(n: u32) -> InstanceKey {
    let mut bytes = [0xa5; 32];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    InstanceKey(ContentId(bytes))
}

/// Function `n`: `fixups` calls to `_start` (1), then `ret`.
fn calls(n: u8, fixups: u32) -> CompiledFunction {
    let mut code = Vec::with_capacity(fixups as usize * 5 + 1);
    let mut relocs = Vec::with_capacity(fixups as usize);
    for at in 0..fixups {
        relocs.push(rel32(at * 5 + 1, Target::Direct(key(1))));
        code.extend_from_slice(&CALLER[..5]);
    }
    code.push(0xc3);
    function(n, &code, relocs)
}

/// What staging a replacement of `g` allocates, in an incremental image of `_start`, `g` and
/// `fillers` functions nothing calls, `g` already replaced once so the structures a
/// replacement grows have grown. The replacement makes `fixups` calls. One worker thread, so
/// the per-thread count sees all the stage's work.
fn staged(fillers: u32, fixups: u32) -> blaze_alloc_count::Used {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    pool.install(|| {
        let (start, g) = program(Target::Function);
        let others: Vec<CompiledFunction> =
            (0..fillers).map(|n| CompiledFunction { key: wide_key(n), ..function(0, &returns(9), Vec::new()) }).collect();
        let mut change = first(&start, &g);
        change.added.extend(&others);
        let mut image = Image::new(name(TO), output(true)).unwrap();
        let mut disk = Disk::default();
        disk.link(&mut image, change).unwrap();
        let g2 = calls(3, fixups);
        disk.link(&mut image, edit(&g2)).unwrap();
        let g3 = calls(4, fixups);
        let next = Change { added: vec![&g3], removed: vec![key(3)], slots: vec![(key(2), key(4))], ..Change::default() };
        let (staged, used) = blaze_alloc_count::measure(|| image.stage(next));
        let mut effects = Vec::new();
        image.commit(staged.unwrap(), &mut effects).unwrap();
        disk.run(&mut image, effects).unwrap();
        assert_eq!(image.generation(), 3);
        used
    })
}

#[test]
fn an_incremental_stage_allocates_a_constant_whatever_the_image_holds() {
    let small = staged(4, 1);
    let large = staged(4096, 1);
    eprintln!("a one-function stage: {small:?} among 6 functions, {large:?} among 4098");
    for (used, what) in [(small, "6 functions"), (large, "4098 functions")] {
        assert!(used.allocations <= STAGE_BUDGET, "a one-function stage among {what}: {used:?}, budget {STAGE_BUDGET}");
    }
    assert!(
        large.allocations <= small.allocations + GROWTH,
        "a one-function stage allocates per function of the image: {small:?} among 6, {large:?} among 4098"
    );
}

#[test]
fn relocation_allocates_nothing_per_fixup() {
    // Below `Symbols`' parallel interning threshold, so both intern on this thread alike.
    let few = staged(4, 64);
    let many = staged(4, 2048);
    eprintln!("a one-function stage: {few:?} with 64 fixups, {many:?} with 2048");
    assert!(many.allocations <= STAGE_BUDGET, "a stage of 2048 fixups: {many:?}, budget {STAGE_BUDGET}");
    assert!(
        many.allocations <= few.allocations + GROWTH,
        "relocation allocates per fixup: {few:?} with 64 fixups, {many:?} with 2048"
    );
}

/// What staging a replacement of `g` allocates when `g` is covered, its arrays `pcs` bytes and
/// its PC table `pcs` entries, in an incremental image of `_start` and `g` (covered from the
/// first link, so the image has the sections), `g` already replaced once. One worker thread.
fn staged_covered(pcs: u32) -> blaze_alloc_count::Used {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    pool.install(|| {
        let covered = |n: u8| CompiledFunction {
            coverage: Some(Covered { counters: pcs, flags: 0, guards: 0, pcs: vec![(0, 0); pcs as usize] }),
            ..calls(n, 1)
        };
        let (start, _) = program(Target::Function);
        let g = covered(2);
        let mut image = Image::new(name(TO), output(true)).unwrap();
        let mut disk = Disk::default();
        disk.link(&mut image, first(&start, &g)).unwrap();
        let g2 = covered(3);
        disk.link(&mut image, edit(&g2)).unwrap();
        let g3 = covered(4);
        let next = Change { added: vec![&g3], removed: vec![key(3)], slots: vec![(key(2), key(4))], ..Change::default() };
        let (staged, used) = blaze_alloc_count::measure(|| image.stage(next));
        let mut effects = Vec::new();
        image.commit(staged.unwrap(), &mut effects).unwrap();
        disk.run(&mut image, effects).unwrap();
        assert_eq!(image.generation(), 3);
        used
    })
}

#[test]
fn coverage_allocates_nothing_per_pc_entry() {
    let few = staged_covered(4);
    let many = staged_covered(2048);
    eprintln!("a covered one-function stage: {few:?} with 4 PC entries, {many:?} with 2048");
    assert!(many.allocations <= STAGE_BUDGET, "a covered stage of 2048 PC entries: {many:?}, budget {STAGE_BUDGET}");
    assert!(
        many.allocations <= few.allocations + GROWTH,
        "coverage allocates per PC entry: {few:?} with 4, {many:?} with 2048"
    );
}

#[test]
fn slot_and_generation_allocate_nothing() {
    let (start, g) = program(Target::Function);
    let mut image = Image::new(name(TO), output(true)).unwrap();
    Disk::default().link(&mut image, first(&start, &g)).unwrap();
    let slot = blaze_alloc_count::assert_at_most(0, "Image::slot", || image.slot(&key(2)));
    assert!(slot.is_some());
    let none = blaze_alloc_count::assert_at_most(0, "Image::slot of a function not linked", || image.slot(&key(9)));
    assert_eq!(none, None);
    let generation = blaze_alloc_count::assert_at_most(0, "Image::generation", || image.generation());
    assert_eq!(generation, 1);
}

mod ties;

mod counter;

mod lazy;

mod wasm_imports;

mod dev_run;

mod natives;

mod premap;

mod sanitize;

/// The state's columns, read and written as a stage does.
mod columns {
    use blaze_codegen::Line;
    use blaze_format::SpanRef;

    use super::{id, key};
    use crate::model::{Apply, Class, Fixup, Sym};
    use crate::state::{Def, NewPiece, PieceId, State, What};
    use crate::symbols::{SymKey, Symbols};

    fn fixup(at: u64, target: Sym) -> Fixup {
        Fixup { at, apply: Apply::Abs64, target, addend: 0, minus: None }
    }

    fn add(state: &mut State, sym: Sym, fixups: &[Fixup], what: What) -> PieceId {
        let class = state.class_id(&Class::Data);
        let piece = NewPiece {
            sym,
            class,
            rank: 0,
            order: [u32::MAX; 3],
            align: 8,
            size: 8 * fixups.len().max(1) as u64,
            zero: false,
            fixups,
            weak: &[],
            defines: &[],
            what,
        };
        let id = state.add(&piece);
        state.set_def(sym, Def::Piece { piece: id, offset: 0 });
        id
    }

    fn refs(state: &State, sym: Sym) -> Vec<PieceId> {
        state.refs(sym).collect()
    }

    #[test]
    fn dense_indexes_are_distinct_and_fit_the_column() {
        let mut symbols = Symbols::default();
        let names: Vec<String> = (0..20_000).map(|it| format!("name{it}")).collect();
        let keys: Vec<SymKey> = names.iter().map(|it| SymKey::Name(symbols.name(it))).collect();
        let syms = symbols.intern_all(&keys);
        let len = symbols.dense_len();
        let mut seen = vec![false; len];
        for sym in &syms {
            let at = Symbols::dense(*sym);
            assert!(at < len, "{at} of {len}");
            assert!(!std::mem::replace(&mut seen[at], true), "{sym:?} shares {at}");
        }
        assert!(len < syms.len() * 2, "the column of {} symbols is {len} long", syms.len());
    }

    #[test]
    fn a_clone_keeps_its_lists_when_the_original_rewrites_them() {
        let mut symbols = Symbols::default();
        let [a, b, c, d] = ["a", "b", "c", "d"].map(|it| symbols.sym_name(it));
        let mut old = State::new();
        let one = add(&mut old, a, &[fixup(0, c)], What::Shared);
        let two = add(&mut old, b, &[fixup(0, c), fixup(8, d)], What::Shared);
        let mut new = old.clone();
        new.set_fixups(one, &[fixup(0, d)]);
        let three = add(&mut new, c, &[fixup(0, a)], What::Shared);

        assert_eq!(old.piece(one).unwrap().fixups(), [fixup(0, c)]);
        assert_eq!(new.piece(one).unwrap().fixups(), [fixup(0, d)]);
        assert_eq!(new.piece(two).unwrap().fixups(), old.piece(two).unwrap().fixups());
        assert_eq!(old.piece_count(), 2);
        assert!(old.piece(three).is_none());
        assert_eq!((refs(&old, c), refs(&old, d)), (vec![one, two], vec![two]));
        assert_eq!((refs(&new, c), refs(&new, d), refs(&new, a)), (vec![two], vec![two, one], vec![three]));
    }

    #[test]
    fn a_removed_piece_leaves_the_referrer_lists_and_its_definitions() {
        let mut symbols = Symbols::default();
        let [a, b, c, target] = ["a", "b", "c", "target"].map(|it| symbols.sym_name(it));
        let mut state = State::new();
        let pieces = [a, b, c].map(|sym| add(&mut state, sym, &[fixup(0, target)], What::Shared));
        assert_eq!(refs(&state, target), pieces);
        assert!(state.remove(pieces[1]));
        assert!(!state.remove(pieces[1]), "a piece is removed once");
        assert_eq!(refs(&state, target), [pieces[0], pieces[2]], "the rest keep their order");
        assert!(matches!(state.def(b), Def::None));
        assert!(state.piece(pieces[1]).is_none());
        assert_eq!(state.any(pieces[1]).unwrap().sym, b, "its columns are still readable");
        let again = add(&mut state, b, &[fixup(0, target)], What::Shared);
        assert_ne!(again, pieces[1], "piece ids are never reused");
        assert_eq!(refs(&state, target), [pieces[0], pieces[2], again]);
    }

    #[test]
    fn compaction_keeps_every_live_list_and_drops_the_rest() {
        let mut symbols = Symbols::default();
        let target = symbols.sym_name("target");
        let mut state = State::new();
        let lines = [Line { at: 4, span: SpanRef { anchor: id(9), index: 2 } }];
        let function = state.add_function(key(1), symbols.id(&id(1)), None, None, &lines);
        let body = symbols.sym_name("body");
        let kept = add(&mut state, body, &[fixup(0, target)], What::Function(function));
        let mut gone = Vec::new();
        for n in 0..2_000 {
            let sym = symbols.sym_name(&format!("gone{n}"));
            let fixups: Vec<Fixup> = (0..64).map(|at| fixup(at * 8, target)).collect();
            gone.push(add(&mut state, sym, &fixups, What::Shared));
        }
        for id in &gone {
            state.remove(*id);
        }
        assert!(state.wants_compaction(), "64 Ki dead fixups outweigh one live one");
        let before = state.clone();
        state.compact();
        assert!(!state.wants_compaction());
        assert_eq!(state.piece(kept).unwrap().fixups(), [fixup(0, target)]);
        assert_eq!(refs(&state, target), [kept]);
        let info = *state.piece(kept).unwrap().function().unwrap();
        let kept_lines: Vec<(u32, u32)> = state.lines(&info).iter().map(|it| (it.at, it.span.index)).collect();
        assert_eq!(kept_lines, [(4, 2)]);
        assert_eq!(before.piece(kept).unwrap().fixups(), [fixup(0, target)], "the clone is untouched");
        assert_eq!(state.piece_count(), before.piece_count(), "ids stay as they were");
    }

    #[test]
    fn cells_know_their_body_and_each_body_its_slots() {
        let mut symbols = Symbols::default();
        let [s0, s1, s2, f, g] = ["s0", "s1", "s2", "f", "g"].map(|it| symbols.sym_name(it));
        let mut state = State::new();
        let slots = [s0, s1, s2].map(|it| state.new_slot(it));
        assert_eq!(slots, [0, 1, 2]);
        state.set_cell(0, Some(f));
        state.set_cell(1, Some(f));
        state.set_cell(2, Some(g));
        let of = |state: &State, body| {
            let mut out: Vec<u32> = state.slots_of(body).collect();
            out.sort_unstable();
            out
        };
        assert_eq!((of(&state, f), of(&state, g)), (vec![0, 1], vec![2]));
        let old = state.clone();
        state.set_cell(1, Some(g));
        state.set_cell(0, None);
        assert_eq!((of(&state, f), of(&state, g)), (vec![], vec![1, 2]));
        assert_eq!(state.cell(0), None);
        assert_eq!(state.filled_cells().collect::<Vec<_>>(), [(1, g), (2, g)]);
        assert_eq!(old.filled_cells().collect::<Vec<_>>(), [(0, f), (1, f), (2, g)], "the clone is untouched");
        assert_eq!(of(&old, f), [0, 1]);
    }
}

/// Native resolution kept across stages: what a stage resolves again, and what it reuses.
mod resolution {
    use blaze_codegen::Target;
    use object::write::{Object as Builder, Relocation, Symbol as Def, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, Object as _, ObjectSection as _, RelocationFlags, SectionFlags,
        SectionKind, SymbolFlags, SymbolKind, SymbolScope, elf,
    };

    use super::{CALLER, Change, Disk, Image, Linked, Native, NativeKind, TO, function, id, key, name, output, returns};
    use crate::native::resolution_id;
    use crate::state::What;

    fn incremental() -> crate::Output {
        output(true)
    }

    /// An ELF object: each function its own `.text.<name>` section, returning `value`; each
    /// constructor an `.init_array` entry pointing at one of them.
    fn object(functions: &[(&str, u8)], constructors: &[&str]) -> Vec<u8> {
        let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let mut symbols = Vec::new();
        for (it, value) in functions {
            let text = b.add_section(Vec::new(), format!(".text.{it}").into_bytes(), SectionKind::Text);
            let at = b.append_section_data(text, &returns(*value), 16);
            let sym = b.add_symbol(Def {
                name: it.as_bytes().to_vec(),
                value: at,
                size: 6,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
            symbols.push((*it, sym));
        }
        for it in constructors {
            let init = b.add_section(Vec::new(), b".init_array".to_vec(), SectionKind::Elf(elf::SHT_INIT_ARRAY));
            b.section_mut(init).flags = SectionFlags::Elf { sh_flags: u64::from(elf::SHF_ALLOC | elf::SHF_WRITE) };
            let at = b.append_section_data(init, &[0; 8], 8);
            let symbol = symbols.iter().find(|(name, _)| name == it).expect("a constructor's function").1;
            b.add_relocation(init, Relocation {
                offset: at,
                symbol,
                addend: 0,
                flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
            })
            .unwrap();
        }
        b.write().unwrap()
    }

    /// A GNU archive of `members` (name, bytes, the symbols it defines), with its symbol index.
    fn archive(members: &[(&str, &[u8], &[&str])]) -> Vec<u8> {
        let header = |name: &str, size: usize| format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644);
        let names: Vec<&str> = members.iter().flat_map(|it| it.2.iter().copied()).collect();
        let index_len = 4 + 4 * names.len() + names.iter().map(|it| it.len() + 1).sum::<usize>();
        let mut offsets = Vec::new();
        let mut at = 8 + 60 + index_len.next_multiple_of(2);
        for (_, bytes, defines) in members {
            offsets.extend(defines.iter().map(|_| at as u32));
            at += 60 + bytes.len().next_multiple_of(2);
        }
        let mut out = b"!<arch>\n".to_vec();
        out.extend_from_slice(header("/", index_len).as_bytes());
        out.extend_from_slice(&(names.len() as u32).to_be_bytes());
        for offset in offsets {
            out.extend_from_slice(&offset.to_be_bytes());
        }
        for it in &names {
            out.extend_from_slice(it.as_bytes());
            out.push(0);
        }
        if out.len() % 2 != 0 {
            out.push(b'\n');
        }
        for (member, bytes, _) in members {
            out.extend_from_slice(header(&format!("{member}/"), bytes.len()).as_bytes());
            out.extend_from_slice(bytes);
            if out.len() % 2 != 0 {
                out.push(b'\n');
            }
        }
        out
    }

    fn native<'a>(n: u8, file: &'a str, bytes: &'a std::sync::Arc<[u8]>, kind: NativeKind) -> Native<'a> {
        Native { id: id(n), name: file, bytes, kind, order: u32::from(n) }
    }

    /// The live native sections of the image.
    fn sections(image: &Image) -> usize {
        image.state.live_pieces().filter(|it| matches!(it.what(), What::Native { .. })).count()
    }

    #[test]
    fn a_body_edit_resolves_nothing() {
        let object: std::sync::Arc<[u8]> = super::c_object().into();
        let start = function(1, &CALLER, vec![super::rel32(1, Target::Extern("c_helper".into()))]);
        let (rust, rust2) = (function(2, &returns(3), Vec::new()), function(3, &returns(4), Vec::new()));
        let mut image = Image::new(name(TO), incremental()).unwrap();
        let mut disk = Disk::default();
        let change = Change {
            added: vec![&start, &rust],
            names: vec![("_start".into(), Target::Direct(key(1))), ("rust_fn".into(), Target::Function(key(2)))],
            debug_names: vec![(key(1), "start".into()), (key(2), "rust".into())],
            natives: vec![native(60, "helper.o", &object, NativeKind::Object)],
            ..Change::default()
        };
        disk.link(&mut image, change).unwrap();
        let before = resolution_id(&image.state.resolved);
        let edit = Change {
            added: vec![&rust2],
            removed: vec![key(2)],
            slots: vec![(key(2), key(3))],
            debug_names: vec![(key(3), "rust2".into())],
            ..Change::default()
        };
        disk.link(&mut image, edit).unwrap();
        assert_eq!(resolution_id(&image.state.resolved), before, "an edit of a body resolves nothing");
        let linked = Linked::of(disk.file(TO));
        let helper = linked.rel32(linked.symbol("start") + 1);
        assert_eq!(linked.through_stub(linked.rel32(helper + 1)), linked.symbol("rust2"));
    }

    /// (b)'s two links: `start` calling `c_one`, then `more` calling `c_two` too.
    fn grow(image: &mut Image, disk: &mut Disk, object: &std::sync::Arc<[u8]>) {
        let start = function(1, &CALLER, vec![super::rel32(1, Target::Extern("c_one".into()))]);
        let change = Change {
            added: vec![&start],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![(key(1), "start".into())],
            natives: vec![native(61, "two.o", object, NativeKind::Object)],
            ..Change::default()
        };
        disk.link(image, change).unwrap();
        let more = function(4, &CALLER, vec![super::rel32(1, Target::Extern("c_two".into()))]);
        let change = Change { added: vec![&more], debug_names: vec![(key(4), "more".into())], ..Change::default() };
        disk.link(image, change).unwrap();
    }

    #[test]
    fn a_new_reference_to_a_loaded_name_grows_the_image_and_dropping_it_collects() {
        let object: std::sync::Arc<[u8]> = object(&[("c_one", 1), ("c_two", 2)], &[]).into();
        let mut image = Image::new(name(TO), incremental()).unwrap();
        let mut disk = Disk::default();
        let start = function(1, &CALLER, vec![super::rel32(1, Target::Extern("c_one".into()))]);
        let change = Change {
            added: vec![&start],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![(key(1), "start".into())],
            natives: vec![native(61, "two.o", &object, NativeKind::Object)],
            ..Change::default()
        };
        disk.link(&mut image, change).unwrap();
        let (first, alone) = (resolution_id(&image.state.resolved), sections(&image));
        let a = Linked::of(disk.file(TO));
        let one = a.rel32(a.symbol("start") + 1);
        assert_eq!(a.at(one, 6), returns(1));

        let more = function(4, &CALLER, vec![super::rel32(1, Target::Extern("c_two".into()))]);
        let change = Change { added: vec![&more], debug_names: vec![(key(4), "more".into())], ..Change::default() };
        disk.link(&mut image, change).unwrap();
        let b = Linked::of(disk.file(TO));
        let two = b.rel32(b.symbol("more") + 1);
        assert_eq!(b.at(two, 6), returns(2), "c_two's section is in the image");
        assert_eq!(b.rel32(b.symbol("start") + 1), one, "c_one's section did not move");
        assert_eq!(resolution_id(&image.state.resolved), first, "a loaded name grows, not rebuilds");
        assert_eq!(sections(&image), alone + 1);

        disk.link(&mut image, Change { removed: vec![key(4)], ..Change::default() }).unwrap();
        assert_ne!(resolution_id(&image.state.resolved), first, "dropping the last reference rebuilds");
        assert_eq!(sections(&image), alone, "c_two's section is collected");
    }

    #[test]
    fn an_archive_member_is_pulled_only_while_something_needs_it() {
        let a = object(&[("need_a", 1)], &["need_a"]);
        let b = object(&[("need_b", 2)], &["need_b"]);
        let library: std::sync::Arc<[u8]> = archive(&[("a.o", &a, &["need_a"]), ("b.o", &b, &["need_b"])]).into();
        let mut image = Image::new(name(TO), incremental()).unwrap();
        let mut disk = Disk::default();
        let start = function(1, &CALLER, vec![super::rel32(1, Target::Extern("need_a".into()))]);
        let change = Change {
            added: vec![&start],
            names: vec![("_start".into(), Target::Direct(key(1)))],
            debug_names: vec![(key(1), "start".into())],
            natives: vec![native(62, "libneed.a", &library, NativeKind::Archive { whole: false })],
            ..Change::default()
        };
        disk.link(&mut image, change).unwrap();
        let constructors = |linked: &Linked| -> Vec<u64> {
            let elf = linked.elf();
            let Some(init) = elf.section_by_name(".init_array") else { return Vec::new() };
            let data = linked.at(init.address(), init.size() as usize);
            data.chunks_exact(8).map(|it| u64::from_le_bytes(it.try_into().unwrap())).filter(|it| *it != 0).collect()
        };
        let first = Linked::of(disk.file(TO));
        let need_a = first.rel32(first.symbol("start") + 1);
        assert_eq!(first.at(need_a, 6), returns(1));
        assert_eq!(constructors(&first), [need_a], "only a.o is pulled");

        let start2 = function(5, &CALLER, vec![super::rel32(1, Target::Extern("need_b".into()))]);
        let change = Change {
            added: vec![&start2],
            removed: vec![key(1)],
            names: vec![("_start".into(), Target::Direct(key(5)))],
            debug_names: vec![(key(5), "start2".into())],
            ..Change::default()
        };
        disk.link(&mut image, change).unwrap();
        let second = Linked::of(disk.file(TO));
        let need_b = second.rel32(second.symbol("start2") + 1);
        assert_eq!(second.at(need_b, 6), returns(2));
        assert_eq!(constructors(&second), [need_b], "a stale member's constructor must not run");
    }

    #[test]
    fn resolution_is_the_same_on_any_number_of_threads() {
        let object: std::sync::Arc<[u8]> = object(&[("c_one", 1), ("c_two", 2)], &[]).into();
        let run = |threads: usize| {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            pool.install(|| {
                let mut image = Image::new(name(TO), incremental()).unwrap();
                let mut disk = Disk::default();
                grow(&mut image, &mut disk, &object);
                disk.file(TO).clone()
            })
        };
        assert!(run(1).same(&run(4)));
    }
}
