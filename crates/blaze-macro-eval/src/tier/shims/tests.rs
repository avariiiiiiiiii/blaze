use base_db::Env;

use super::*;
use crate::tier::region::Bounds;

const SMALL: Bounds = Bounds { stack: 1 << 16, heap: 16 << 20 };

struct NoBridge;

impl Bridge for NoBridge {
    fn call(&mut self, _: &mut Shims<'_, '_>, _: &Running<'_>, _: &mut [[u8; 16]]) -> Result<(), Escape> {
        Err(Stop("no bridge in this test".to_owned()).into())
    }
}

fn words(given: &[u64]) -> [u64; MOST_PARAMS] {
    let mut out = [0; MOST_PARAMS];
    out[..given.len()].copy_from_slice(given);
    out
}

/// Runs `own` of `args`, giving its result's word.
fn run(shims: &mut Shims<'_, '_>, own: Own, args: &[u64]) -> Result<u64, Escape> {
    let mut ret = [0xaa; 16];
    shims.own(own, &words(args), &mut ret)?;
    assert_eq!(&ret[8..], &[0; 8], "the slot's high half is cleared");
    Ok(u64::from_le_bytes(ret[..8].try_into().unwrap()))
}

#[test]
fn the_allocators_and_memory_functions_work_on_the_region() {
    let env = Env::default();
    let mut os = Os::new(&env);
    let mut memory = Memory::new(&[], &[], SMALL).unwrap();
    let mut bridge = NoBridge;
    let mut shims = Shims::new(&[], &mut memory, &mut os, &mut bridge);

    let a = run(&mut shims, Own::Alloc, &[32, 8]).unwrap();
    let b = run(&mut shims, Own::Malloc, &[32]).unwrap();
    assert_ne!(a, b);
    shims.memory().write(a, b"hello, world\0").unwrap();
    assert_eq!(run(&mut shims, Own::Strlen, &[a]).unwrap(), 12);
    assert_eq!(run(&mut shims, Own::Memcpy, &[b, a, 13]).unwrap(), b);
    assert_eq!(run(&mut shims, Own::Memcmp, &[a, b, 13]).unwrap(), 0);
    assert_eq!(run(&mut shims, Own::Memchr, &[a, u64::from(b'o'), 12]).unwrap(), a + 4);
    assert_eq!(run(&mut shims, Own::Memrchr, &[a, u64::from(b'o'), 12]).unwrap(), a + 8);
    assert_eq!(run(&mut shims, Own::Memchr, &[a, u64::from(b'z'), 12]).unwrap(), 0);
    assert_eq!(run(&mut shims, Own::Memset, &[b, u64::from(b'J'), 1]).unwrap(), b);
    let less = run(&mut shims, Own::Memcmp, &[b, a, 13]).unwrap() as u32 as i32;
    assert!(less < 0, "'J' sorts before 'h'");

    let zeroed = run(&mut shims, Own::Calloc, &[4, 8]).unwrap();
    assert_eq!(shims.memory().read(zeroed, 32).unwrap(), &[0; 32]);
    assert_eq!(run(&mut shims, Own::Calloc, &[u64::MAX, 2]).unwrap(), 0, "an overflowing calloc is null");
    let grown = run(&mut shims, Own::CRealloc, &[b, 4096]).unwrap();
    assert_eq!(shims.memory().read(grown, 1).unwrap(), b"J");
    run(&mut shims, Own::Free, &[grown]).unwrap();
    run(&mut shims, Own::Free, &[0]).unwrap();
    run(&mut shims, Own::Dealloc, &[a, 32, 8]).unwrap();

    let out = run(&mut shims, Own::Malloc, &[8]).unwrap();
    assert_eq!(run(&mut shims, Own::PosixMemalign, &[out, 64, 100]).unwrap(), 0);
    assert!(shims.memory().read_u64(out).unwrap().is_multiple_of(64));
    assert_eq!(run(&mut shims, Own::Errno, &[]).unwrap(), shims.memory().errno_address());
}

#[test]
fn what_a_shim_cant_do_stops_and_a_raise_goes_up_the_frames() {
    let env = Env::default();
    let mut os = Os::new(&env);
    let mut memory = Memory::new(&[], &[], SMALL).unwrap();
    let mut bridge = NoBridge;
    let mut shims = Shims::new(&[], &mut memory, &mut os, &mut bridge);

    let Err(Escape::Stop(stop)) = run(&mut shims, Own::Memcpy, &[16, 0, SPAN + 1]) else { panic!("copied past memory") };
    assert!(stop.0.contains("past its memory"), "{stop}");
    let Err(Escape::Stop(stop)) = run(&mut shims, Own::Abort, &[]) else { panic!("abort returned") };
    assert_eq!(stop.0, "called `abort`");
    let Err(Escape::Stop(stop)) = run(&mut shims, Own::AllocError, &[1 << 40, 8]) else { panic!("no stop") };
    assert!(stop.0.contains("1099511627776 bytes"), "{stop}");
    let Err(Escape::Raise(at)) = run(&mut shims, Own::RaiseException, &[0x4000]) else { panic!("no raise") };
    assert_eq!(at, 0x4000);
    let Err(Escape::Raise(at)) = run(&mut shims, Own::UnwindResume, &[0x4010]) else { panic!("no resume") };
    assert_eq!(at, 0x4010);
    assert_eq!(run(&mut shims, Own::Backtrace, &[0, 0]).unwrap(), URC_END_OF_STACK);

    let text = run(&mut shims, Own::Malloc, &[8]).unwrap();
    assert_eq!(run(&mut shims, Own::Write, &[2, text, 8]).unwrap(), 8, "stderr takes it all");
    let Err(Escape::Stop(stop)) = run(&mut shims, Own::Write, &[3, text, 8]) else { panic!("wrote to fd 3") };
    assert!(stop.0.contains("descriptor 3"), "{stop}");
}

#[test]
fn a_stub_has_a_slot_per_parameter_and_vmctx() {
    let stub = |params: &[u8], ret| Stub { bound: Bound::Unshimmed("f".to_owned()), params: params.to_vec(), ret };
    assert_eq!(stub(&[], 0).slots(), 1);
    assert_eq!(stub(&[8, 4], 4).slots(), 3);
    assert_eq!(stub(&[], 8).slots(), 1);
}
