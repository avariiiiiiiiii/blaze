//! Stopping every other thread of the process on Linux x86-64, for v1's writes over the
//! image ([`World`]).
//!
//! Each thread is sent a real-time signal of its own with `tgkill`. Its handler records where
//! the thread was (the `rip` the kernel saved), says it stopped, and waits on a futex until
//! the stopper lets it go; then it runs `cpuid`, which serializes, so that the thread fetches
//! the code written while it waited, as Intel asks of code another processor modified. The
//! threads are listed from `/proc/self/task` with `getdents64` into a buffer on the stack,
//! and again after each have stopped until no new one shows up, so one started meanwhile is
//! stopped too. Nothing allocates from the first signal sent to the last thread let go.
//!
//! A thread that doesn't stop within [`STOP_WITHIN`] (one that blocks the signal, or one in
//! a wait the kernel doesn't interrupt) makes the patch refused, with every thread let go.
//! Each round of stopping has a number: the handler of a signal from a round that ended
//! counts in no other ([`take`]), so the count the stopper waits for is of this round's
//! threads alone.

#![allow(unsafe_code)]

use std::{
    ffi::c_void,
    fs::File,
    os::fd::AsRawFd,
    sync::{
        Mutex,
        atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};

use crate::apply::{MOST_THREADS, STOP_WITHIN, World};

unsafe extern "C" {
    fn syscall(number: i64, ...) -> i64;
    fn sigaction(signal: i32, action: *const SigAction, old: *mut SigAction) -> i32;
    fn __libc_current_sigrtmin() -> i32;
    fn __libc_current_sigrtmax() -> i32;
    fn __errno_location() -> *mut i32;
}

const SYS_LSEEK: i64 = 8;
const SYS_GETTID: i64 = 186;
const SYS_FUTEX: i64 = 202;
const SYS_GETDENTS64: i64 = 217;
const SYS_TGKILL: i64 = 234;
const FUTEX_WAIT_PRIVATE: i32 = 128;
const FUTEX_WAKE_PRIVATE: i32 = 129;
const SEEK_SET: i32 = 0;
const ESRCH: i32 = 3;
const SA_SIGINFO: i32 = 4;
const SA_ONSTACK: i32 = 0x0800_0000;
const SA_RESTART: i32 = 0x1000_0000;
/// Where `ucontext_t` has `uc_mcontext.gregs[REG_RIP]` on x86-64: after `uc_flags`,
/// `uc_link` and the 24-byte `stack_t`, the 16th of the 8-byte registers.
const RIP_AT: usize = 8 + 8 + 24 + 16 * 8;

/// glibc's and musl's `struct sigaction` on x86-64.
#[repr(C)]
#[allow(dead_code)]
struct SigAction {
    handler: usize,
    mask: [u64; 16],
    flags: i32,
    restorer: usize,
}

/// The round the threads are held in, or 0 once they may go on: the futex they wait on.
static HOLD: AtomicU32 = AtomicU32::new(0);
/// The round's number in the high half and, in the low, how many threads took a slot of
/// [`PCS`], and how many stopped.
static CLAIMED: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);
/// Where each thread stopped, by the slot it took.
static PCS: [AtomicU64; MOST_THREADS] = [const { AtomicU64::new(0) }; MOST_THREADS];
/// The signal the handler is installed for, once.
static SIGNAL: AtomicI32 = AtomicI32::new(0);
/// One stopper at a time, with the last round's number.
static STOPPING: Mutex<u32> = Mutex::new(0);

/// Adds one to `counter` if it is of `round`: the count before.
fn take(counter: &AtomicU64, round: u32) -> Option<usize> {
    let mut now = counter.load(Ordering::Relaxed);
    loop {
        if (now >> 32) as u32 != round {
            return None;
        }
        match counter.compare_exchange_weak(now, now + 1, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return Some(now as u32 as usize),
            Err(seen) => now = seen,
        }
    }
}

/// The handler: records where the thread is and holds it while its round lasts. It calls
/// only what is async-signal-safe (atomics, `syscall`), and gives `errno` back as it was.
extern "C" fn held(_: i32, _: *mut c_void, context: *mut c_void) {
    // SAFETY: it takes nothing and returns this thread's `errno`, valid while it runs.
    let errno = unsafe { __errno_location() };
    // SAFETY: as above.
    let saved = unsafe { errno.read() };
    let round = HOLD.load(Ordering::Acquire);
    if round != 0
        && let Some(slot) = take(&CLAIMED, round)
    {
        // SAFETY: with SA_SIGINFO the third argument is the `ucontext_t` the kernel saved on
        // this thread's signal stack, which has the interrupted `rip` at `RIP_AT`.
        let pc = unsafe { context.cast::<u8>().add(RIP_AT).cast::<u64>().read_unaligned() };
        if let Some(at) = PCS.get(slot) {
            at.store(pc, Ordering::Relaxed);
        }
        take(&DONE, round);
        while HOLD.load(Ordering::Acquire) == round {
            // SAFETY: waits while `HOLD` is `round`; it is a static, and with no timeout the
            // call reads only it.
            unsafe { syscall(SYS_FUTEX, HOLD.as_ptr(), FUTEX_WAIT_PRIVATE, round, 0usize) };
        }
        serialize();
    }
    // SAFETY: as above.
    unsafe { errno.write(saved) };
}

/// Serializes the instruction stream: what another processor wrote is what is fetched next.
fn serialize() {
    // SAFETY: `cpuid` reads and writes registers alone; `rbx`, which LLVM keeps for itself,
    // is saved and given back around it.
    unsafe {
        std::arch::asm!(
            "mov {saved}, rbx",
            "cpuid",
            "mov rbx, {saved}",
            saved = out(reg) _,
            inout("eax") 0 => _,
            inout("ecx") 0 => _,
            out("edx") _,
            options(nostack, preserves_flags),
        )
    };
}

/// Installs [`held`] for the highest real-time signal nothing else handles, once.
fn install() -> Result<i32, String> {
    let _one = STOPPING.lock().unwrap_or_else(|it| it.into_inner());
    let installed = SIGNAL.load(Ordering::Acquire);
    if installed != 0 {
        return Ok(installed);
    }
    // SAFETY: they take nothing and say which real-time signals the C library leaves free.
    let (low, high) = unsafe { (__libc_current_sigrtmin(), __libc_current_sigrtmax()) };
    for signal in (low..=high).rev() {
        let mut old = SigAction { handler: 0, mask: [0; 16], flags: 0, restorer: 0 };
        // SAFETY: reads the signal's action into `old`, changing nothing.
        if unsafe { sigaction(signal, std::ptr::null(), &mut old) } != 0 || old.handler != 0 {
            continue;
        }
        let action = SigAction {
            handler: held as extern "C" fn(i32, *mut c_void, *mut c_void) as usize,
            mask: [!0; 16],
            flags: SA_SIGINFO | SA_ONSTACK | SA_RESTART,
            restorer: 0,
        };
        // SAFETY: `held` is a handler of the SA_SIGINFO kind, which only uses what is
        // async-signal-safe; the signal was handled by nothing (the default, read above),
        // and every signal is blocked while it runs.
        if unsafe { sigaction(signal, &action, std::ptr::null_mut()) } == 0 {
            SIGNAL.store(signal, Ordering::Release);
            return Ok(signal);
        }
    }
    Err(format!(
        "no real-time signal ({low}..={high}) is free to stop the program's threads with, which \
         live edits write over their code with"
    ))
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Why the threads were not all stopped, said once they are let go.
enum Stuck {
    Many,
    Listing(i32),
    Signal(i32, i32),
    Slow(usize, usize),
}

/// The threads of the process, listed and stopped by signals.
pub(super) struct Threads {
    task: File,
    signal: i32,
    pid: i32,
    me: i32,
    /// The threads of this round, and where the stopped ones are: room for [`MOST_THREADS`]
    /// of each, so a round allocates nothing.
    tids: Vec<i32>,
    pcs: Vec<u64>,
}

impl Threads {
    pub(super) fn new() -> Result<Threads, String> {
        let signal = install()?;
        let task = File::open("/proc/self/task")
            .map_err(|it| format!("listing the program's threads in /proc/self/task: {it}"))?;
        let pid = std::process::id() as i32;
        Ok(Threads {
            task,
            signal,
            pid,
            me: 0,
            tids: Vec::with_capacity(MOST_THREADS),
            pcs: Vec::with_capacity(MOST_THREADS),
        })
    }

    /// Lists the threads and signals each not signalled yet this round: how many it signalled.
    fn signal_new(&mut self) -> Result<usize, Stuck> {
        let fd = self.task.as_raw_fd();
        // SAFETY: rewinds this one's directory, which it holds open.
        if unsafe { syscall(SYS_LSEEK, fd, 0i64, SEEK_SET) } < 0 {
            return Err(Stuck::Listing(errno()));
        }
        let mut buffer = [0u8; 4096];
        let mut signalled = 0;
        loop {
            // SAFETY: `buffer` has room for `buffer.len()` bytes of entries.
            let read = unsafe { syscall(SYS_GETDENTS64, fd, buffer.as_mut_ptr(), buffer.len()) };
            let Ok(read) = usize::try_from(read) else { return Err(Stuck::Listing(errno())) };
            if read == 0 {
                return Ok(signalled);
            }
            let mut at = 0;
            while at + 19 < read {
                let len = usize::from(u16::from_ne_bytes([buffer[at + 16], buffer[at + 17]]));
                if len == 0 || at + len > read {
                    return Err(Stuck::Listing(0));
                }
                let name = &buffer[at + 19..at + len];
                at += len;
                let Some(tid) = tid_of(name) else { continue };
                if tid == self.me || self.tids.contains(&tid) {
                    continue;
                }
                if self.tids.len() == self.tids.capacity() {
                    return Err(Stuck::Many);
                }
                self.tids.push(tid);
                // SAFETY: sends the signal `held` handles to a thread of this process.
                if unsafe { syscall(SYS_TGKILL, self.pid, tid, self.signal) } == 0 {
                    signalled += 1;
                } else if errno() != ESRCH {
                    return Err(Stuck::Signal(tid, errno()));
                }
            }
        }
    }

    /// Stops every thread but this one: how many stopped.
    fn stop(&mut self) -> Result<usize, Stuck> {
        let deadline = Instant::now() + STOP_WITHIN;
        let mut expected = 0;
        loop {
            let new = self.signal_new()?;
            expected += new;
            loop {
                let done = DONE.load(Ordering::Acquire) as u32 as usize;
                if done >= expected {
                    break;
                }
                if Instant::now() > deadline {
                    return Err(Stuck::Slow(expected, done));
                }
                std::thread::yield_now();
            }
            if new == 0 {
                return Ok(expected);
            }
        }
    }
}

/// The thread id a directory entry of `/proc/self/task` names, up to its zero.
fn tid_of(name: &[u8]) -> Option<i32> {
    let name = &name[..name.iter().position(|it| *it == 0)?];
    if name.is_empty() || !name.iter().all(u8::is_ascii_digit) {
        return None;
    }
    name.iter()
        .try_fold(0i32, |tid, digit| tid.checked_mul(10)?.checked_add(i32::from(digit - b'0')))
}

impl World for Threads {
    fn stopped<T>(&mut self, f: impl FnOnce(&[u64]) -> T) -> Result<T, String> {
        let mut last = STOPPING.lock().unwrap_or_else(|it| it.into_inner());
        let round = last.wrapping_add(1).max(1);
        *last = round;
        // SAFETY: it takes nothing and returns this thread's id.
        self.me = unsafe { syscall(SYS_GETTID) } as i32;
        self.tids.clear();
        self.pcs.clear();
        CLAIMED.store(u64::from(round) << 32, Ordering::Relaxed);
        DONE.store(u64::from(round) << 32, Ordering::Relaxed);
        HOLD.store(round, Ordering::Release);
        let stopped = self.stop().map(|count| {
            for at in &PCS[..count.min(MOST_THREADS)] {
                self.pcs.push(at.load(Ordering::Relaxed));
            }
            f(&self.pcs)
        });
        HOLD.store(0, Ordering::Release);
        // SAFETY: wakes every thread waiting on `HOLD`, a static.
        unsafe { syscall(SYS_FUTEX, HOLD.as_ptr(), FUTEX_WAKE_PRIVATE, i32::MAX) };
        stopped.map_err(|stuck| match stuck {
            Stuck::Many => format!("the program runs more than {MOST_THREADS} threads to stop"),
            Stuck::Listing(code) => format!("listing the program's threads: error {code}"),
            Stuck::Signal(tid, code) => format!("stopping thread {tid}: error {code}"),
            Stuck::Slow(expected, done) => format!(
                "{} of the program's {expected} other threads did not stop within {STOP_WITHIN:?} \
                 (one blocks signal {}, or waits where the kernel doesn't interrupt it)",
                expected - done,
                self.signal
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use super::*;

    #[test]
    fn thread_ids_are_read_from_entry_names() {
        assert_eq!(tid_of(b"1234\0\0\0"), Some(1234));
        assert_eq!(tid_of(b".\0"), None);
        assert_eq!(tid_of(b"..\0"), None);
        assert_eq!(tid_of(b"99999999999\0"), None);
        assert_eq!(tid_of(b"12"), None);
    }

    /// A round from before counts in no other.
    #[test]
    fn a_count_of_another_round_is_not_taken() {
        let counter = AtomicU64::new(7 << 32 | 3);
        assert_eq!(take(&counter, 7), Some(3));
        assert_eq!(take(&counter, 6), None);
        assert_eq!(counter.load(Ordering::Relaxed), 7 << 32 | 4);
    }

    /// Threads that spin are stopped where they are, each counted once, and go on after.
    #[test]
    fn every_other_thread_stops_and_goes_on() {
        let go = Arc::new(AtomicBool::new(true));
        let counts: Arc<Vec<AtomicU64>> = Arc::new((0..4).map(|_| AtomicU64::new(0)).collect());
        let spinning: Vec<_> = (0..4)
            .map(|nth| {
                let (go, counts) = (go.clone(), counts.clone());
                std::thread::spawn(move || {
                    while go.load(Ordering::Relaxed) {
                        counts[nth].fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        let mut threads = Threads::new().expect("a signal to stop threads with");
        for _ in 0..3 {
            // Nothing allocates while the threads, the test harness's among them, are stopped.
            let (pcs, still) = threads
                .stopped(|pcs| {
                    let before: [u64; 4] =
                        std::array::from_fn(|n| counts[n].load(Ordering::Relaxed));
                    for _ in 0..10_000 {
                        std::hint::spin_loop();
                    }
                    let after: [u64; 4] =
                        std::array::from_fn(|n| counts[n].load(Ordering::Relaxed));
                    (pcs.len(), before == after)
                })
                .expect("stopped");
            assert!(pcs >= 4, "{pcs} threads stopped");
            assert!(still, "a thread ran while the others were stopped");
        }
        let before: u64 = counts.iter().map(|it| it.load(Ordering::Relaxed)).sum();
        while counts.iter().map(|it| it.load(Ordering::Relaxed)).sum::<u64>() == before {
            std::hint::spin_loop();
        }
        go.store(false, Ordering::Relaxed);
        for thread in spinning {
            thread.join().expect("joined");
        }
    }
}
