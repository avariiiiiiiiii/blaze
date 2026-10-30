//! What the compiled tier answers of the calls [`Os::call`] gives back to the evaluator
//! ([`Answer::Evaluator`]), as `hir-ty-mir`'s thread shims (`mir/eval/shim/thread.rs`) answer
//! them for a run of one thread, which an expansion is:
//!
//! - the monotonic clocks (and the CPU-time ones, which the evaluator counts the same) read
//!   the evaluator's start, a machine up for a day, plus the run's virtual time, which no
//!   call of an expansion advances: so they are as deterministic as the evaluator's and are
//!   no input of the expansion's key;
//! - `gettid` is the evaluator's main thread's, 1;
//! - a `futex` wake wakes no one, and a wait returns `EAGAIN` when the word has changed, as
//!   the kernel's does; a wait on a word that holds the value would never return, since no
//!   other thread can change it, so it stops the expansion.

use super::{
    Call, Done, Os, Shim, fail, int, ok,
    target::{Errno, Sys, System, Target, Thread},
};
use crate::guest::{Guest, Stop, word};

const NANOS: u64 = 1_000_000_000;
/// The evaluator's `CLOCK_MONOTONIC` when a run starts.
const MONOTONIC_START_NS: u64 = 86_400 * NANOS;

/// Linux's `futex` operations, without `FUTEX_PRIVATE_FLAG` and `FUTEX_CLOCK_REALTIME`.
const FUTEX_WAIT: i64 = 0;
const FUTEX_WAKE: i64 = 1;
const FUTEX_WAIT_BITSET: i64 = 9;
const FUTEX_WAKE_BITSET: i64 = 10;
const FUTEX_CMD: i64 = !(128 | 256);

impl Os<'_> {
    /// Answers `shim`, a call [`Os::call`] gave back, as the evaluator's thread shims do.
    pub(crate) fn as_one_thread(&mut self, shim: Shim, g: &mut impl Guest, args: &[&[u8]], ret: &mut [u8]) -> Done {
        let Some(t) = shim.target else {
            // alloc: error path
            return Err(Stop(format!("calls `{:?}` on a target whose system blaze does not model", shim.call)));
        };
        match shim.call {
            Call::ClockGettime => clock_gettime(t, g, ret, (int(args, 0)?, word(args, 1)?)),
            Call::Syscall => match t.sys(int(args, 0)?) {
                Sys::Evaluator(Thread::Gettid) => ok(ret, 1),
                Sys::Evaluator(Thread::Futex) => futex(t, g, ret, &args[1..]),
                // alloc: error path
                other => Err(Stop(format!("makes the system call {other:?}, which only the evaluator answers"))),
            },
            // alloc: error path
            other => Err(Stop(format!("calls `{other:?}`, which only the evaluator answers"))),
        }
    }
}

fn clock_gettime(t: Target, g: &mut impl Guest, ret: &mut [u8], (clock, out): (i64, u64)) -> Done {
    let monotonic = match t.system {
        System::Linux => matches!(clock, 1..=4 | 6 | 7 | 9),
        System::Apple => matches!(clock, 4..=9 | 12 | 16),
    };
    if !monotonic {
        return fail(t, g, ret, Errno::Inval);
    }
    let mut timespec = [0; 16];
    timespec[..8].copy_from_slice(&(MONOTONIC_START_NS / NANOS).to_le_bytes());
    timespec[8..].copy_from_slice(&(MONOTONIC_START_NS % NANOS).to_le_bytes());
    g.write(out, &timespec)?;
    ok(ret, 0)
}

/// `syscall(SYS_futex, addr, op, value, ..)`.
fn futex(t: Target, g: &mut impl Guest, ret: &mut [u8], args: &[&[u8]]) -> Done {
    let (addr, op) = (word(args, 0)?, int(args, 1)? & FUTEX_CMD);
    match op {
        FUTEX_WAKE | FUTEX_WAKE_BITSET => ok(ret, 0),
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let value = word(args, 2)? as u32;
            let held = g.read(addr, 4)?;
            if u32::from_le_bytes([held[0], held[1], held[2], held[3]]) != value {
                return fail(t, g, ret, Errno::Again);
            }
            Err(Stop("waits on a futex that no other thread can wake: it would never return".to_owned())) // alloc: error path
        }
        // alloc: error path
        _ => Err(Stop(format!("makes the futex operation {op}, which blaze does not answer"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        guest::tests::Buffer,
        os::{Answer, target::Arch},
    };

    const LINUX: Target = Target { system: System::Linux, arch: Arch::X86_64 };

    fn memory() -> Buffer {
        Buffer { base: 0, bytes: vec![0; 64], errno: None }
    }

    fn futex_of(g: &mut Buffer, ret: &mut [u8], op: i64, value: u64) -> Done {
        let words = [8u64.to_le_bytes(), op.to_le_bytes(), value.to_le_bytes()];
        let args: Vec<&[u8]> = words.iter().map(|it| &it[..]).collect();
        futex(LINUX, g, ret, &args)
    }

    #[test]
    fn a_monotonic_clock_reads_the_evaluators_start() {
        let mut g = memory();
        let mut ret = [0; 4];
        clock_gettime(LINUX, &mut g, &mut ret, (1, 16)).unwrap();
        assert_eq!(ret, [0; 4]);
        assert_eq!(g.read(16, 8).unwrap(), &86_400u64.to_le_bytes());
        assert_eq!(g.read(24, 8).unwrap(), &[0; 8]);
        clock_gettime(LINUX, &mut g, &mut ret, (99, 16)).unwrap();
        assert_eq!(ret, [0xff; 4], "an unknown clock fails");
        assert_eq!(g.errno, Some(22));
    }

    #[test]
    fn a_futex_wakes_no_one_and_a_wait_never_hangs() {
        let mut g = memory();
        g.write(8, &5u32.to_le_bytes()).unwrap();
        let mut ret = [0; 8];
        assert_eq!(futex_of(&mut g, &mut ret, FUTEX_WAKE | 128, 1), Ok(Answer::Answered));
        assert_eq!(ret, [0; 8]);
        futex_of(&mut g, &mut ret, FUTEX_WAIT | 128, 4).unwrap();
        assert_eq!((ret, g.errno), ([0xff; 8], Some(11)), "EAGAIN: the word changed");
        let stop = futex_of(&mut g, &mut ret, FUTEX_WAIT_BITSET, 5).unwrap_err();
        assert!(stop.0.contains("never return"), "{stop}");
    }
}
