//! The table switching on an image made in the test's own memory: a descriptor, a table
//! cell, a two-slot table of two functions, and buffers for the patch regions, with the
//! platform's writes done by [`Fake`].

#![allow(unsafe_code)]

use std::{
    collections::HashMap,
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_ids::rng::SplitMix64;

use super::{
    Outcome, hook, image,
    switch::{Dwarf, HOST_ARCH, KEPT, Layout, Memory, POOL, Span, Switch, Writes},
};
use crate::patch::{
    Answer, DESCRIPTOR_MAGIC, DESCRIPTOR_VERSION, Descriptor, FIRST_GENERATION, Message, Patch,
};

/// The slide of the fake image: every address it says is this much below where it is.
const SLIDE: u64 = 0x7_0000;
const ID: [u8; 16] = [0x42; 16];

extern "C-unwind" fn one() -> u32 {
    1
}

extern "C-unwind" fn two() -> u32 {
    2
}

#[inline(never)]
extern "C-unwind" fn boom() -> u32 {
    panic!("boom in a patch")
}

fn address(f: extern "C-unwind" fn() -> u32) -> u64 {
    f as usize as u64
}

fn unslid(address: u64) -> u64 {
    address.wrapping_sub(SLIDE)
}

#[derive(Default)]
struct Fake {
    /// Whether code writes are written: not when the region is a function's own code.
    write_code: bool,
    frames: Vec<u64>,
    functions: Vec<(u64, u64, u64)>,
    published: usize,
    /// Memory the fake image does not have: a range touching it is refused as a platform
    /// refuses an address outside the image.
    outside: Option<Range<u64>>,
}

impl Memory for Fake {
    const DWARF: Dwarf = Dwarf::Sections;
    const SEH: bool = true;

    fn cell(&self, cell: u64) -> u64 {
        // SAFETY: the cell is the `Program`'s, an 8-aligned `AtomicU64` that outlives the
        // switch.
        unsafe { image::cell(cell) }.load(Ordering::Acquire)
    }

    fn in_image(&self, range: &Range<u64>) -> Result<(), String> {
        match &self.outside {
            Some(outside) if range.start < outside.end && outside.start < range.end => {
                Err(format!("{:#x}..{:#x}, outside the image", range.start, range.end))
            }
            _ => Ok(()),
        }
    }

    fn bytes(&self, range: &Range<u64>) -> Result<&'static [u8], String> {
        Err(format!("{:#x}: the fake image has no read-only data", range.start))
    }

    fn loaders(&self, at: u64, slots: u64) -> &[u64] {
        // SAFETY: the table is the `Program`'s, 8-aligned and `slots` long (`Switch::new`
        // checked the cell points at it), and outlives the switch; nothing writes it.
        unsafe { image::words(at, slots as usize) }
    }

    fn code(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        if self.write_code {
            // SAFETY: every write the tests make is to a buffer of their `Program`, which
            // outlives the switch, and the switch checked it is in the region the descriptor
            // names.
            unsafe { writes.put_in_place() };
        }
        Ok(())
    }

    fn data(&mut self, writes: &Writes<'_>) -> Result<(), String> {
        // SAFETY: every data write the tests make is to a buffer of their `Program`, which
        // outlives the switch; the switch checks only that it misses the code region, the
        // cell, the loader's table and lasting memory, so the tests keep to the buffers.
        unsafe { writes.store_in_place() };
        Ok(())
    }

    fn frames(&mut self, starts: &[u64]) -> Result<(), String> {
        self.frames.extend_from_slice(starts);
        Ok(())
    }

    fn functions(&mut self, functions: &[(u64, u64, u64)]) -> Result<(), String> {
        self.functions.extend_from_slice(functions);
        Ok(())
    }

    fn publish(&mut self, cell: u64, table: u64) -> Result<(), String> {
        // SAFETY: the cell is the `Program`'s, an `AtomicU64` that outlives the switch.
        unsafe { AtomicU64::from_ptr(cell as *mut u64) }.store(table, Ordering::Release);
        self.published += 1;
        Ok(())
    }
}

/// The parts of an image a switch works on, in the test's memory.
struct Program {
    cell: Box<AtomicU64>,
    table: Box<[u64]>,
    code: Box<[u64]>,
    data: Box<[u64]>,
    /// The patch code region: `code`'s buffer, or a function's own code.
    region: Range<u64>,
}

impl Program {
    fn new() -> Program {
        Program::with_slots(2)
    }

    /// A program whose table has `slots` slots, of `one` and `two` in turn.
    fn with_slots(slots: usize) -> Program {
        let table: Box<[u64]> =
            (0..slots).map(|it| if it % 2 == 0 { address(one) } else { address(two) }).collect();
        let cell = Box::new(AtomicU64::new(table.as_ptr() as u64));
        let code = vec![0u64; 512].into_boxed_slice();
        let region = code.as_ptr() as u64..code.as_ptr() as u64 + 512 * 8;
        let data = vec![0u64; 64].into_boxed_slice();
        Program { cell, table, code, data, region }
    }

    fn code_at(&self, offset: u64) -> u64 {
        self.region.start + offset
    }

    fn data_at(&self, offset: u64) -> u64 {
        self.data.as_ptr() as u64 + offset
    }

    fn switch(&self, write_code: bool) -> Switch<Fake> {
        self.switch_at(write_code, FIRST_GENERATION)
    }

    /// A switch of the program, from its descriptor as the linker writes it for an image
    /// linked at `generation`.
    fn switch_at(&self, write_code: bool, generation: u64) -> Switch<Fake> {
        self.switch_with(Fake { write_code, ..Fake::default() }, generation).expect("a switch")
    }

    fn switch_with(&self, fake: Fake, generation: u64) -> Result<Switch<Fake>, String> {
        let mut bytes = vec![0u8; Descriptor::SIZE];
        let at = bytes.as_ptr() as u64;
        let descriptor = Descriptor {
            magic: DESCRIPTOR_MAGIC,
            version: DESCRIPTOR_VERSION,
            arch: HOST_ARCH,
            id: ID,
            generation,
            this: unslid(at),
            table: unslid(self.cell.as_ptr() as u64),
            slots: self.table.len() as u64,
            patch_code: [unslid(self.region.start), unslid(self.region.end)],
            patch_data: [unslid(self.data_at(0)), unslid(self.data_at(64 * 8))],
            dir: [0, 0],
        };
        bytes.copy_from_slice(&descriptor.encode());
        let decoded = Descriptor::decode(&bytes).expect("a descriptor");
        let layout = Layout::new(&decoded, at).expect("a layout");
        assert_eq!(layout.slide, SLIDE);
        Switch::new(fake, layout)
    }

    /// Calls slot `slot` as code the linker made does: the cell, then the slot.
    fn call(&self, slot: usize) -> u32 {
        let f = self.slots()[slot];
        // SAFETY: the table holds addresses of `extern "C-unwind" fn() -> u32`.
        let f: extern "C-unwind" fn() -> u32 = unsafe { std::mem::transmute(f as usize) };
        f()
    }

    fn calls(&self) -> (u32, u32) {
        (self.call(0), self.call(1))
    }

    /// Every slot of the table the cell points at.
    fn slots(&self) -> Vec<u64> {
        let table = self.cell.load(Ordering::Acquire) as *const u64;
        // SAFETY: the cell holds a table of `self.table.len()` addresses, 8-aligned, which
        // nothing writes while the test reads it.
        unsafe { std::slice::from_raw_parts(table, self.table.len()) }.to_vec()
    }
}

fn patch(generation: u64, base: u64, cells: Vec<(u32, u64)>) -> Patch {
    Patch { id: ID, generation, base, functions: cells.len() as u64, cells, ..Patch::default() }
}

fn applied(switch: &mut Switch<Fake>, patch: Patch) {
    let generation = patch.generation;
    let (outcome, answer) = switch.handle(Message::Patch(patch));
    assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome}");
    assert_eq!(answer, Some(Answer::Applied { generation }));
}

/// A cell or a loader's table the image doesn't have is refused before the switch reads it.
#[test]
fn a_table_outside_the_image_is_refused() {
    let program = Program::new();
    let cell = program.cell.as_ptr() as u64;
    let table = program.table.as_ptr() as u64;
    for (outside, named) in [(cell..cell + 8, cell), (table + 8..table + 9, table)] {
        let fake = Fake { outside: Some(outside), ..Fake::default() };
        let refused = program.switch_with(fake, FIRST_GENERATION).err().expect("refused");
        assert!(refused.contains(&format!("{named:#x}")), "{refused}");
    }
    let zero = Program::new();
    zero.cell.store(0, Ordering::Relaxed);
    let refused = zero.switch_with(Fake::default(), FIRST_GENERATION).err().expect("refused");
    assert!(refused.contains("holds 0x0"), "{refused}");
}

#[test]
fn patches_switch_the_table_and_roll_back() {
    let program = Program::new();
    let mut switch = program.switch(true);
    let image_table = program.cell.load(Ordering::Relaxed);
    assert_eq!(program.calls(), (1, 2));

    // An address to rebase, then an FDE and the zero length after it, in the code; a static
    // holding an address in the data.
    let mut code = vec![0u8; 32];
    code[..8].copy_from_slice(&unslid(address(one)).to_le_bytes());
    code[8..12].copy_from_slice(&12u32.to_le_bytes());
    code[12..16].copy_from_slice(&1u32.to_le_bytes());
    let mut first = patch(1, 0, vec![(0, unslid(address(two))), (1, unslid(address(one)))]);
    first.code = vec![(unslid(program.code_at(0)), code)];
    first.data =
        vec![(unslid(program.data_at(0)), unslid(program.data_at(8)).to_le_bytes().into())];
    first.rebase = vec![unslid(program.code_at(0)), unslid(program.data_at(0))];
    first.frames = vec![(unslid(program.code_at(8)), 16)];
    first.windows =
        vec![(unslid(program.code_at(0)), unslid(program.code_at(8)), unslid(program.code_at(24)))];
    first.statics = 1;
    let (outcome, answer) = switch.handle(Message::Patch(first));
    assert_eq!(outcome, Outcome::Applied { generation: 1, functions: 2, statics: 1 });
    assert_eq!(answer, Some(Answer::Applied { generation: 1 }));
    assert_eq!(program.calls(), (2, 1));
    assert_eq!(program.code[0], address(one));
    assert_eq!(program.data[0], program.data_at(8));
    assert_eq!(switch.memory.frames, vec![program.code_at(8)]);
    assert_eq!(
        switch.memory.functions,
        vec![(program.code_at(0), program.code_at(8), program.code_at(24))]
    );
    assert_eq!(switch.current(), 1);

    let (outcome, _) = switch.handle(Message::Patch(patch(2, 0, vec![(0, unslid(address(one)))])));
    assert!(
        matches!(&outcome, Outcome::Declined { generation: 2, why } if why.contains("is at 1")),
        "{outcome}"
    );
    assert_eq!(program.calls(), (2, 1));

    let mut second = patch(2, 1, vec![(1, unslid(address(two)))]);
    second.code = vec![(unslid(program.code_at(64)), vec![0xcc; 16])];
    applied(&mut switch, second);
    assert_eq!(program.calls(), (2, 2));
    assert_eq!(program.code[8], u64::from_le_bytes([0xcc; 8]));

    let mut over = patch(3, 2, vec![]);
    over.code = vec![(unslid(program.code_at(16)), vec![0; 8])];
    let (outcome, _) = switch.handle(Message::Patch(over));
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. } if why.contains("can be running")),
        "{outcome}"
    );

    let (outcome, answer) = switch.handle(Message::RollBack { id: ID, to: 1 });
    let why = "the linker took it back".to_owned();
    assert_eq!(outcome, Outcome::RolledBack { generation: 2, to: 1, why: why.clone() });
    assert_eq!(answer, Some(Answer::RolledBack { generation: 2, to: 1, why }));
    assert_eq!(program.calls(), (2, 1));
    switch.handle(Message::RollBack { id: ID, to: 0 });
    assert_eq!(program.calls(), (1, 2));
    assert_eq!(program.cell.load(Ordering::Relaxed), image_table);

    // Code rolled back stays: a thread can be in it still.
    let mut third = patch(3, 0, vec![(1, unslid(address(one)))]);
    third.code = vec![(unslid(program.code_at(0)), vec![0x90; 8])];
    let (outcome, _) = switch.handle(Message::Patch(third.clone()));
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. }
            if why.contains("generation 1's code") && why.contains("can be running")),
        "{outcome}"
    );
    assert_eq!(program.code[0], address(one));
    third.code = vec![(unslid(program.code_at(128)), vec![0x90; 8])];
    applied(&mut switch, third);
    assert_eq!(switch.kept(), vec![0, 1, 2, 3]);
    assert_eq!(program.calls(), (1, 1));
    switch.handle(Message::RollBack { id: ID, to: 2 });
    assert_eq!(program.calls(), (2, 2));
    switch.handle(Message::RollBack { id: ID, to: 3 });

    let restart = Message::Restart { id: ID, generation: 3, why: "a layout changed".into() };
    let (outcome, answer) = switch.handle(restart);
    assert_eq!(outcome, Outcome::Restart { generation: 3, why: "a layout changed".into() });
    assert_eq!(answer, None);
    let (outcome, _) = switch.handle(Message::RollBack { id: [0; 16], to: 0 });
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. } if why.contains("for image")),
        "{outcome}"
    );
    assert_eq!(program.calls(), (1, 1));
}

#[test]
fn a_program_starts_at_the_generation_it_was_linked_at() {
    let program = Program::new();
    let mut switch = program.switch_at(false, 7);
    assert_eq!(switch.current(), 7);
    let (outcome, _) = switch.handle(Message::Patch(patch(1, 0, vec![])));
    assert!(
        matches!(&outcome, Outcome::Declined { why, .. } if why.contains("where this program is at 7")),
        "{outcome}"
    );
    applied(&mut switch, patch(8, 7, vec![(0, unslid(address(two)))]));
    assert_eq!(program.calls(), (2, 2));
    switch.handle(Message::RollBack { id: ID, to: 7 });
    assert_eq!(program.calls(), (1, 2));
    assert_eq!(switch.kept(), vec![7, 8]);
}

#[test]
fn a_bad_patch_is_declined_before_anything_is_written() {
    let program = Program::new();
    let mut switch = program.switch(true);
    let table = program.cell.load(Ordering::Relaxed);
    let region = unslid(program.code_at(0));
    let with = |change: &dyn Fn(&mut Patch)| {
        let mut patch = patch(1, 0, vec![]);
        change(&mut patch);
        patch
    };
    let mut unterminated = vec![0u8; 16];
    unterminated[..4].copy_from_slice(&12u32.to_le_bytes());
    unterminated[4..8].copy_from_slice(&1u32.to_le_bytes());
    let cases: Vec<(Patch, &str)> = vec![
        (with(&|it| it.id = [0; 16]), "for image"),
        (patch(1, 1, vec![]), "where this program is at 0"),
        (patch(0, 0, vec![]), "counts up from its base"),
        (
            with(&|it| it.code = vec![(unslid(program.region.end), vec![1])]),
            "outside the patch code",
        ),
        (with(&|it| it.data = vec![(region, vec![1])]), "in the patch code region"),
        (
            with(&|it| it.data = vec![(unslid(program.cell.as_ptr() as u64), vec![0; 8])]),
            "the jump table's cell",
        ),
        (patch(1, 0, vec![(2, 0)]), "slot 2, where the table has 2"),
        (patch(1, 0, vec![(0, 0), (0, 0)]), "slot 0 set twice"),
        (with(&|it| it.rebase = vec![region]), "in none of the patch's writes"),
        (
            with(&|it| {
                it.code = vec![(region, vec![1; 8])];
                it.rebase = vec![region, region];
            }),
            "rebased twice",
        ),
        (with(&|it| it.code = vec![(region, vec![0; 16]), (region + 8, vec![0; 8])]), "two writes"),
        (
            with(&|it| {
                it.code = vec![(region, vec![0; 16])];
                it.frames = vec![(region, 16)];
            }),
            "its length says 4",
        ),
        (
            with(&|it| {
                it.code = vec![(region, unterminated.clone())];
                it.frames = vec![(region, 16)];
            }),
            "unwind entries reach",
        ),
        (with(&|it| it.windows = vec![(region, region, region)]), "a RUNTIME_FUNCTION"),
        (
            with(&|it| {
                it.code = vec![(region, vec![0, 0, 0, 0, 0, 0, 4, 0])];
                it.windows = vec![(region, region + 4, region + 4)];
            }),
            "runs past the patch's writes (12 bytes)",
        ),
        (
            with(&|it| it.data = vec![(unslid(program.table.as_ptr() as u64), vec![0; 8])]),
            "the loader's jump table",
        ),
    ];
    for (bad, says) in cases {
        let (outcome, answer) = switch.handle(Message::Patch(bad));
        match &outcome {
            Outcome::Declined { why, .. } => {
                assert!(why.contains(says), "{why:?} doesn't say {says:?}")
            }
            other => panic!("{other}, where it should say {says:?}"),
        }
        assert!(matches!(answer, Some(Answer::Declined { .. })), "{answer:?}");
        assert_eq!(program.cell.load(Ordering::Relaxed), table);
        assert_eq!(switch.memory.published, 0);
        assert_eq!(switch.current(), 0);
    }
    assert!(program.code.iter().chain(program.data.iter()).all(|it| *it == 0));
    assert!(switch.memory.frames.is_empty() && switch.memory.functions.is_empty());
}

#[test]
fn the_last_generations_are_kept() {
    let program = Program::new();
    let mut switch = program.switch(false);
    for generation in 1..=100u64 {
        let target = if generation % 2 == 0 { address(one) } else { address(two) };
        applied(&mut switch, patch(generation, generation - 1, vec![(0, unslid(target))]));
        assert!(switch.pooled() <= POOL);
    }
    assert_eq!(program.call(0), 1);
    assert_eq!(switch.kept(), (101 - KEPT as u64..=100).collect::<Vec<_>>());
    let (outcome, _) = switch.handle(Message::RollBack { id: ID, to: 1 });
    assert!(matches!(outcome, Outcome::Declined { generation: 1, .. }), "{outcome}");
    switch.handle(Message::RollBack { id: ID, to: 99 });
    assert_eq!(program.call(0), 2);
    assert_eq!(switch.current(), 99);
}

#[test]
fn a_panic_is_tied_to_the_newest_generation_of_its_code() {
    let program = Program::new();
    let mut switch = program.switch(false);
    let mut first = patch(1, 0, vec![(0, unslid(address(two)))]);
    first.code = vec![(unslid(program.code_at(0)), vec![0; 64])];
    applied(&mut switch, first);
    let mut second = patch(2, 1, vec![]);
    second.code = vec![(unslid(program.code_at(64)), vec![0; 64])];
    applied(&mut switch, second);

    assert_eq!(switch.tie(&[program.code_at(10)]), Some((1, 0)));
    assert_eq!(switch.tie(&[program.code_at(70), program.code_at(10)]), Some((2, 1)));
    assert_eq!(switch.tie(&[program.code_at(10), program.code_at(127)]), Some((2, 1)));
    assert_eq!(switch.tie(&[program.code_at(128), address(one)]), None);
    assert_eq!(switch.tie(&[]), None);
    switch.roll_back(1).expect("generation 1 is kept");
    assert_eq!(switch.tie(&[program.code_at(70)]), None, "not what the program runs");
    assert_eq!(switch.tie(&[program.code_at(70), program.code_at(10)]), Some((1, 0)));
}

/// A real panic, in a function standing for a generation's code, reached through its slot.
#[cfg(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
))]
#[test]
fn a_panic_in_a_generations_code_rolls_it_back() {
    hook::install();
    let mut program = Program::new();
    program.region = address(boom)..address(boom) + 256;
    let mut switch = program.switch(false);
    let mut first = patch(1, 0, vec![(0, unslid(address(boom)))]);
    first.code = vec![(unslid(address(boom)), vec![0; 256])];
    applied(&mut switch, first);

    let mut answers = Vec::new();
    let result = switch.guard(|| program.call(0), &mut |it| answers.push(it.clone()));
    let why = match result {
        Err(Outcome::RolledBack { generation: 1, to: 0, why }) => why,
        other => panic!("{other:?}"),
    };
    assert!(why.starts_with("panicked at ") && why.contains("tests.rs"), "{why}");
    assert!(why.ends_with(": boom in a patch"), "{why}");
    assert_eq!(answers, vec![Answer::RolledBack { generation: 1, to: 0, why }]);
    assert_eq!(switch.current(), 0);
    assert_eq!(program.call(0), 1);
    assert_eq!(switch.guard(|| program.call(0), &mut |_| {}), Ok(1));
}

#[test]
fn a_panic_elsewhere_is_resumed() {
    hook::install();
    let program = Program::new();
    let mut switch = program.switch(false);
    let mut first = patch(1, 0, vec![]);
    first.code = vec![(unslid(program.code_at(0)), vec![0; 64])];
    applied(&mut switch, first);

    let mut answers = Vec::new();
    let resumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        switch.guard(|| -> u32 { panic!("not in a patch") }, &mut |it| answers.push(it.clone()))
    }));
    let payload = resumed.expect_err("the panic is resumed");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"not in a patch"));
    assert!(answers.is_empty());
    assert_eq!(switch.current(), 1);
    assert_eq!(switch.guard(|| 7, &mut |_| {}), Ok(7));
}

#[test]
fn registered_unwind_entries_are_never_written_again() {
    let program = Program::new();
    let mut switch = program.switch(false);
    // An FDE of 16 bytes, then the zero length ending its run, in the data.
    let mut entries = vec![0u8; 20];
    entries[..4].copy_from_slice(&12u32.to_le_bytes());
    let mut first = patch(1, 0, vec![(0, unslid(address(two)))]);
    first.data = vec![(unslid(program.data_at(0)), entries)];
    first.frames = vec![(unslid(program.data_at(0)), 16)];
    applied(&mut switch, first);
    assert_eq!(switch.memory.frames, vec![program.data_at(0)]);

    for (at, len) in [(0, 8), (12, 8), (16, 4), (18, 8)] {
        let mut over = patch(2, 1, vec![]);
        over.data = vec![(unslid(program.data_at(at)), vec![1; len])];
        let (outcome, _) = switch.handle(Message::Patch(over));
        assert!(
            matches!(&outcome, Outcome::Declined { why, .. }
                if why.contains("generation 1's unwind entries")),
            "{at}: {outcome}"
        );
    }
    let mut after = patch(2, 1, vec![]);
    after.data = vec![(unslid(program.data_at(20)), vec![1; 4])];
    applied(&mut switch, after);

    // Rolled back, the generation's entries stay registered, and stay as they are.
    switch.roll_back(0).expect("generation 0 is kept");
    let mut over = patch(3, 0, vec![]);
    over.data = vec![(unslid(program.data_at(0)), vec![0; 20])];
    let (outcome, _) = switch.handle(Message::Patch(over));
    assert!(matches!(outcome, Outcome::Declined { .. }), "{outcome}");
    assert_eq!(program.data[0], 12);
}

/// The pool's tables, brought from one generation to another by undoing and redoing cells,
/// against a whole table of every generation, over patches and rollbacks that branch.
#[test]
fn the_pool_holds_what_a_whole_table_per_generation_would() {
    const SLOTS: usize = 16;
    let program = Program::with_slots(SLOTS);
    let mut switch = program.switch(false);
    let mut model = HashMap::from([(FIRST_GENERATION, program.table.to_vec())]);
    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let mut next = FIRST_GENERATION + 1;
    for _ in 0..2000 {
        if rng.one_in(3) {
            let kept = switch.kept();
            let to = kept[rng.index(kept.len())];
            switch.roll_back(to).expect("a kept generation");
        } else {
            let base = switch.current();
            let mut table = model[&base].clone();
            let mut cells = Vec::new();
            for slot in 0..SLOTS {
                if rng.one_in(4) {
                    let target = 0x10_0000 + rng.below(1 << 20) * 8;
                    table[slot] = target;
                    cells.push((slot as u32, unslid(target)));
                }
            }
            applied(&mut switch, patch(next, base, cells));
            model.insert(next, table);
            next += 1;
        }
        assert!(switch.pooled() <= POOL);
        assert_eq!(program.slots(), model[&switch.current()], "at {}", switch.current());
    }
    assert_eq!(switch.pooled(), POOL);
}

#[test]
fn a_rollback_to_a_generation_a_table_holds_writes_no_table() {
    let program = Program::new();
    let image = program.cell.load(Ordering::Relaxed);
    let mut switch = program.switch(false);
    applied(&mut switch, patch(1, 0, vec![(0, unslid(address(two)))]));
    let first = switch.table_of(1).expect("a table holds generation 1");
    applied(&mut switch, patch(2, 1, vec![(1, unslid(address(one)))]));
    let second = switch.table_of(2).expect("a table holds generation 2");
    assert_ne!(first, second);
    assert_eq!(switch.table_of(1), Some(first));
    assert_eq!(program.calls(), (2, 1));
    let pooled = switch.pooled();

    switch.roll_back(1).expect("generation 1 is kept");
    assert_eq!(program.cell.load(Ordering::Relaxed), first);
    assert_eq!(program.calls(), (2, 2));
    switch.roll_back(0).expect("generation 0 is kept");
    assert_eq!(program.cell.load(Ordering::Relaxed), image);
    assert_eq!(switch.table_of(0), Some(image));
    switch.roll_back(2).expect("generation 2 is kept");
    assert_eq!(program.cell.load(Ordering::Relaxed), second);
    assert_eq!(program.calls(), (2, 1));
    assert_eq!(switch.pooled(), pooled);
}

/// Before its switch, a patch stores its own cells and nothing else: the table it goes
/// into was brought to its base right after the switch before, however full the pool.
#[test]
fn a_patch_stores_only_its_own_cells_before_the_switch() {
    const SLOTS: usize = 64;
    let program = Program::with_slots(SLOTS);
    let mut switch = program.switch(false);
    let mut table = program.table.to_vec();
    for generation in 1..=4 * POOL as u64 {
        let cells: Vec<(u32, u64)> = (0..generation as u32 % 7 + 1)
            .map(|it| {
                let slot = (it * 9 + generation as u32) % SLOTS as u32;
                (slot, unslid(0x20_0000 + generation * 0x100 + u64::from(it) * 8))
            })
            .collect();
        for &(slot, target) in &cells {
            table[slot as usize] = target.wrapping_add(SLIDE);
        }
        let count = cells.len();
        applied(&mut switch, patch(generation, generation - 1, cells));
        assert_eq!(switch.stored(), count, "generation {generation}");
        assert_eq!(program.slots(), table, "generation {generation}");
    }
    assert_eq!(switch.pooled(), POOL);
}

/// `put` of every range against the bytes written whole and rebased one field at a time,
/// for ranges that cut spans and rebased fields anywhere.
#[test]
fn a_write_puts_any_part_of_its_spans_rebased() {
    const START: u64 = 0x1000;
    let message: Vec<u8> = (0..64u8).collect();
    let spans = [Span::new(START, 0, 24), Span::new(START + 30, 40, 16)];
    let rebase = [START + 4, START + 16, START + 36];
    let slide = 0x0123_4567_89ab_cdef;
    let writes = Writes::new(&message, &spans, &rebase, slide);

    let mut whole = [None::<u8>; 48];
    for span in &spans {
        for (offset, byte) in writes.bytes(span).iter().enumerate() {
            whole[(span.at - START) as usize + offset] = Some(*byte);
        }
    }
    for field in rebase {
        let at = (field - START) as usize;
        let bytes: Vec<u8> = whole[at..at + 8].iter().map(|it| it.expect("in a span")).collect();
        let value = u64::from_le_bytes(bytes.try_into().expect("8 bytes")).wrapping_add(slide);
        for (offset, byte) in value.to_le_bytes().into_iter().enumerate() {
            whole[at + offset] = Some(byte);
        }
    }
    for start in 0..whole.len() {
        for end in start..=whole.len() {
            let mut buffer = vec![0xee; end - start];
            let range = START + start as u64..START + end as u64;
            writes.fill(&range, &mut buffer);
            let expected: Vec<u8> = whole[start..end].iter().map(|it| it.unwrap_or(0xee)).collect();
            assert_eq!(buffer, expected, "{range:x?}");
        }
    }
}

#[test]
fn outcomes_say_what_became_of_a_patch() {
    let applied = Outcome::Applied { generation: 3, functions: 2, statics: 1 };
    assert_eq!(applied.to_string(), "generation 3 applied: 2 functions, 1 statics");
    let applied = Outcome::Applied { generation: 3, functions: 2, statics: 0 };
    assert_eq!(applied.to_string(), "generation 3 applied: 2 functions");
    let back = Outcome::RolledBack { generation: 3, to: 2, why: "panicked at a.rs:1:2: x".into() };
    assert_eq!(back.to_string(), "generation 3 rolled back to 2: panicked at a.rs:1:2: x");
    let restart = Outcome::Restart { generation: 3, why: "a layout changed".into() };
    assert_eq!(restart.to_string(), "restart needed at generation 3: a layout changed");
    let declined = Outcome::Declined { generation: 4, why: "no".into() };
    assert_eq!(declined.to_string(), "generation 4 not applied: no");
}

/// A warm patch's table work allocates nothing: the log's columns and the lasting ranges
/// are reserved regions, the pool is full and the scratch buffers are at their high-water
/// mark. Each patch sets one cell of a 16-slot table and is taken from encoded bytes, as
/// `Live::poll` takes a message from its kept buffer.
#[test]
fn a_warm_patch_allocates_nothing() {
    const SLOTS: u64 = 16;
    let program = Program::with_slots(SLOTS as usize);
    let mut switch = program.switch(false);
    let message = |generation: u64, base: u64| {
        let slot = (generation % SLOTS) as u32;
        let target = if generation % 2 == 0 { address(one) } else { address(two) };
        Message::Patch(patch(generation, base, vec![(slot, unslid(target))])).encode()
    };
    for _ in 0..POOL + KEPT + 8 {
        let base = switch.current();
        let cell = ((base % SLOTS) as u32, unslid(address(two)));
        applied(&mut switch, patch(base + 1, base, vec![cell]));
    }
    let first = switch.current();
    let messages: Vec<Vec<u8>> = (0..64).map(|it| message(first + it + 1, first + it)).collect();
    blaze_alloc_count::assert_at_most(0, "a warm patch's table work and its switch", || {
        for bytes in &messages {
            let view = Message::view(bytes).expect("an encoded message reads back");
            let (outcome, answer) = switch.take(&view);
            assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome}");
            assert!(answer.is_some());
        }
    });
    assert_eq!(switch.current(), first + 64);
}

/// The lasting ranges merge as `Vec::splice` would: against a model kept in a `Vec`.
#[test]
fn lasting_ranges_merge_as_a_sorted_list_would() {
    use super::switch::{Holds, Lasting};
    let mut lasting = Lasting::new().expect("reserved");
    let mut model: Vec<(Range<u64>, u64, Holds)> = Vec::new();
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    for number in 0..3000u64 {
        let seed = rng.next_u64();
        let start = seed % 4096 * 16;
        let range = start..start + 1 + seed % 64;
        let holds = if seed % 3 == 0 { Holds::Unwind } else { Holds::Code };
        lasting.add(range.clone(), number, holds);
        let at = model.partition_point(|it| it.0.end <= range.start);
        let (mut merged, mut kind, mut end) = (range, holds, at);
        while let Some((next, _, other)) = model.get(end).filter(|it| it.0.start < merged.end) {
            merged = merged.start.min(next.start)..merged.end.max(next.end);
            if *other == Holds::Code {
                kind = Holds::Code;
            }
            end += 1;
        }
        model.splice(at..end, [(merged, number, kind)]);
        assert_eq!(&lasting.ranges[..], &model[..]);
    }
}
