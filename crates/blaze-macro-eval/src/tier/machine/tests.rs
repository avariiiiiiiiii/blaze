use super::*;
use crate::tier::{
    load::{Call, Function, Image, Input},
    region::Bounds,
};

const SMALL: Bounds = Bounds { stack: 1 << 16, heap: 16 << 20 };

/// An image of one function `f` of 64 bytes: its call returning at 8 has a landing pad at 32,
/// its call returning at 16 none; its frame is 48 bytes.
fn image() -> Image {
    let f = Function {
        name: "f".to_owned(),
        bytes: vec![0; 64],
        align: 16,
        relocs: Vec::new(),
        entry: None,
        frame: 48,
        calls: vec![Call { ret: 8, landing: Some(32) }, Call { ret: 16, landing: None }],
        traps: vec![(20, ir::TrapCode::HEAP_OUT_OF_BOUNDS)],
    };
    Image::load(Input { functions: vec![f], blobs: Vec::new(), bounds: SMALL }).unwrap()
}

/// A stack of `words`, 16-aligned, and its address range.
fn stack(words: usize) -> (Vec<u128>, Range<u64>) {
    let memory = vec![0u128; words / 2 + 1];
    let start = memory.as_ptr() as u64;
    (memory, start..start + (words as u64) * 8)
}

fn put(stack: &Range<u64>, at: u64, words: [u64; 2]) {
    assert!(stack.contains(&at) && at + 16 <= stack.end);
    // SAFETY: inside the test's own stack buffer, 8-aligned, checked above.
    unsafe {
        ptr::write(at as *mut u64, words[0]);
        ptr::write((at + 8) as *mut u64, words[1]);
    }
}

#[test]
fn resume_is_the_routine_of_the_host() {
    let code = resume_code();
    if cfg!(target_arch = "x86_64") {
        assert_eq!(code.len(), 14);
        assert_eq!(&code[12..], &[0xff, 0xe7], "it ends in `jmp rdi`");
    } else if cfg!(target_arch = "aarch64") {
        assert_eq!(code.len(), 24);
        assert_eq!(u32::from_le_bytes(code[20..24].try_into().unwrap()), 0xd61f_0200, "it ends in `br x16`");
    }
}

#[test]
fn a_panic_lands_at_the_first_landing_pad_and_a_fault_at_the_thunk() {
    let image = image();
    let f = image.code(0).unwrap();
    let (_memory, range) = stack(64);
    let running = Running::new(&image, range.clone(), 0).unwrap();
    let thunk = Thunk { entry: 0, ret: 0x7000, landing: 0x7100, frame: 32 };
    running.add_thunk(thunk);
    // f's frame at `inner`, called by f again at `outer` (returning at 8, its pad), called by
    // the thunk at `top`.
    let (inner, outer, top) = (range.start + 32, range.start + 96, range.start + 256);
    put(&range, inner, [outer, f + 16]);
    put(&range, outer, [top, thunk.ret]);

    let pad = running.walk(f + 8, outer, Want::Pad).unwrap();
    assert_eq!(pad, Landing { pc: f + 32, sp: outer - 48, fp: outer });
    let none_here = running.walk(f + 16, inner, Want::Pad);
    assert_eq!(none_here, Some(Landing { pc: 0x7100, sp: top - 32, fp: top }), "no pad at 16: on to the thunk");
    let escape = running.walk(f + 8, outer, Want::Thunk).unwrap();
    assert_eq!(escape, Landing { pc: 0x7100, sp: top - 32, fp: top }, "an escape passes every pad");
}

#[test]
fn a_chain_that_does_not_rise_or_leaves_the_stack_lands_nowhere() {
    let image = image();
    let f = image.code(0).unwrap();
    let (_memory, range) = stack(64);
    let running = Running::new(&image, range.clone(), 0).unwrap();
    let at = range.start + 64;
    put(&range, at, [at - 16, f + 16]);
    assert_eq!(running.walk(f + 16, at, Want::Thunk), None, "a frame below the last");
    put(&range, at, [range.end + 64, f + 16]);
    assert_eq!(running.walk(f + 16, at, Want::Thunk), None, "a frame outside the stack");
    assert_eq!(running.walk(f + 16, at + 4, Want::Thunk), None, "a misaligned frame");
}

/// Binding 1 returns 42 in its first slot, 2 stops, 3 raises the exception at 0x1234; each
/// has two slots.
struct Shims;

impl Dispatch for Shims {
    fn slots(&self, id: u32) -> Option<usize> {
        (1..=3).contains(&id).then_some(2)
    }

    fn call(&mut self, _: &Running<'_>, id: u32, slots: &mut [[u8; 16]]) -> Result<(), Escape> {
        match id {
            1 => {
                slots[0][..8].copy_from_slice(&42u64.to_le_bytes());
                Ok(())
            }
            2 => Err(Escape::Stop(Stop("asked to stop".to_owned()))),
            _ => Err(Escape::Raise(0x1234)),
        }
    }
}

#[test]
fn a_shim_returns_stops_at_the_thunk_or_raises_to_the_first_pad() {
    let image = image();
    let f = image.code(0).unwrap();
    let (_memory, range) = stack(1 << 17);
    let running = Running::new(&image, range.clone(), 0).unwrap();
    let thunk = Thunk { entry: 0, ret: 0x7000, landing: 0x7100, frame: 32 };
    running.add_thunk(thunk);
    running.dispatch.set(Some(ptr::NonNull::from(Box::leak(Box::new(Shims)) as &mut dyn Dispatch)));
    // The stub's frame at `stub`, called from f at 8 (its pad), whose frame is at `caller`,
    // called by the thunk at `top`.
    let (stub, caller, top) = (range.end - 512, range.end - 256, range.end - 64);
    put(&range, stub, [caller, f + 8]);
    put(&range, caller, [top, thunk.ret]);
    let mut slots = [[0u8; 16]; 2];
    let at = slots.as_mut_ptr() as u64;

    assert_eq!(running.dispatched(1, at, stub), None);
    assert_eq!(&slots[0][..8], &42u64.to_le_bytes());

    let (landing, payload) = running.dispatched(3, at, stub).unwrap();
    assert_eq!((landing.pc, payload), (f + 32, [0x1234, 0]), "a raise lands at f's pad");
    assert!(running.take_escaped().is_none(), "the macro may catch it yet");

    let (landing, _) = running.dispatched(2, at, stub).unwrap();
    assert_eq!(landing, Landing { pc: 0x7100, sp: top - 32, fp: top });
    let Some(Escaped::Stop(Stop(why))) = running.take_escaped() else { panic!("no stop recorded") };
    assert_eq!(why, "asked to stop");

    let (_, _) = running.dispatched(9, at, stub).unwrap();
    let Some(Escaped::Stop(Stop(why))) = running.take_escaped() else { panic!("no stop recorded") };
    assert!(why.contains("binding 9"), "{why}");

    let deep = range.start + 1024;
    put(&range, deep, [caller, f + 16]);
    let (landing, _) = running.dispatched(1, at, deep).unwrap();
    assert_eq!(landing.pc, 0x7100, "too little stack left: stopped, not run");
    let Some(Escaped::Stop(Stop(why))) = running.take_escaped() else { panic!("no stop recorded") };
    assert!(why.contains("overflowed"), "{why}");
}

#[test]
fn a_fault_records_its_trap_and_lands_at_the_thunk() {
    let image = image();
    let f = image.code(0).unwrap();
    let (_memory, range) = stack(64);
    let running = Running::new(&image, range.clone(), 0).unwrap();
    running.add_thunk(Thunk { entry: 0, ret: 0x9000, landing: 0x9100, frame: 16 });
    let (fp, top) = (range.start + 64, range.start + 128);
    put(&range, fp, [top, 0x9000]);
    let landing = running.catch(libc::SIGSEGV, Regs { pc: f + 20, sp: fp - 48, fp, lr: 0 }).unwrap();
    assert_eq!(landing, Landing { pc: 0x9100, sp: top - 16, fp: top });
    let fault = running.take_fault().unwrap();
    assert_eq!(fault, Fault { signal: libc::SIGSEGV, pc: f + 20, trap: Some(ir::TrapCode::HEAP_OUT_OF_BOUNDS) });
    assert_eq!(running.take_fault(), None);
    assert!(running.catch(libc::SIGSEGV, Regs { pc: 0x10, sp: 0, fp, lr: 0 }).is_none(), "not the image's code");
}
