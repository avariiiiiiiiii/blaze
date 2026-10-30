//! The table switching on an image made in the test's own memory: a descriptor, a table
//! cell, a two-slot table of two functions, and buffers for the patch regions, with the
//! platform's writes done by [`Fake`].

use std::{
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    Outcome, hook,
    switch::{Dwarf, HOST_ARCH, KEPT, Layout, Memory, Switch},
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
}

fn copy(writes: &[(u64, Vec<u8>)]) {
    for (at, bytes) in writes {
        // SAFETY: every write the tests make is to a buffer of their `Program`, which outlives
        // the switch, and the switch checked it is in the region the descriptor names.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), *at as *mut u8, bytes.len()) };
    }
}

impl Memory for Fake {
    const DWARF: Dwarf = Dwarf::Sections;
    const SEH: bool = true;

    fn code(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        if self.write_code {
            copy(writes);
        }
        Ok(())
    }

    fn data(&mut self, writes: &[(u64, Vec<u8>)]) -> Result<(), String> {
        copy(writes);
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
        let table: Box<[u64]> = vec![address(one), address(two)].into_boxed_slice();
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
        Switch::new(Fake { write_code, ..Fake::default() }, layout).expect("a switch")
    }

    /// Calls slot `slot` as code the linker made does: the cell, then the slot.
    fn call(&self, slot: usize) -> u32 {
        let table = self.cell.load(Ordering::Acquire) as *const u64;
        // SAFETY: the cell holds a table of two addresses of `extern "C-unwind" fn() -> u32`.
        let f = unsafe { *table.add(slot) };
        // SAFETY: as above.
        let f: extern "C-unwind" fn() -> u32 = unsafe { std::mem::transmute(f as usize) };
        f()
    }

    fn calls(&self) -> (u32, u32) {
        (self.call(0), self.call(1))
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

    // Code over a generation rolled back drops it, and the one applied on it.
    let mut third = patch(3, 0, vec![(1, unslid(address(one)))]);
    third.code = vec![(unslid(program.code_at(0)), vec![0x90; 8])];
    applied(&mut switch, third);
    assert_eq!(switch.kept(), vec![0, 3]);
    assert_eq!(program.calls(), (1, 1));
    let (outcome, _) = switch.handle(Message::RollBack { id: ID, to: 2 });
    assert!(
        matches!(&outcome, Outcome::Declined { generation: 2, why } if why.contains("not kept")),
        "{outcome}"
    );

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
        assert!(switch.retired() <= KEPT);
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
