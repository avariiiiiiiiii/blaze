//! Stopping every other thread of the process on Windows x86-64, for v1's writes over the
//! image ([`World`]).
//!
//! `NtGetNextThread` walks the process's threads, each opened to suspend and read;
//! `SuspendThread` stops each and `GetThreadContext`, which returns once a thread running on
//! another core has stopped, says where. They are walked again after each have stopped
//! until no new one shows up, so one started meanwhile is stopped too, and `ResumeThread`
//! lets each go. A thread is let go from the kernel, where it was held, and fetches what was
//! written meanwhile once the writer called `FlushInstructionCache` over it. Nothing comes
//! from the heap while the threads are stopped (a thread may hold its lock): each call here
//! is a system call, and the handles and where the threads are go in room reserved before.

#![allow(unsafe_code)]

use std::{ffi::c_void, sync::Mutex};

use crate::apply::{MOST_THREADS, World};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtGetNextThread(
        process: *mut c_void,
        thread: *mut c_void,
        access: u32,
        attributes: u32,
        flags: u32,
        next: *mut *mut c_void,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn GetCurrentThreadId() -> u32;
    fn GetThreadId(thread: *mut c_void) -> u32;
    fn SuspendThread(thread: *mut c_void) -> u32;
    fn ResumeThread(thread: *mut c_void) -> u32;
    fn GetThreadContext(thread: *mut c_void, context: *mut Context) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

const THREAD_SUSPEND_RESUME: u32 = 0x0002;
const THREAD_GET_CONTEXT: u32 = 0x0008;
const THREAD_QUERY_LIMITED_INFORMATION: u32 = 0x0800;
const STATUS_NO_MORE_ENTRIES: i32 = 0x8000_001a_u32 as i32;
/// `CONTEXT_AMD64 | CONTEXT_CONTROL`: `Rip` and the rest of the control registers.
const CONTEXT_CONTROL: u32 = 0x0010_0001;
/// Where an x86-64 `CONTEXT` has `ContextFlags`, and `Rip`.
const FLAGS_AT: usize = 0x30;
const RIP_AT: usize = 0xf8;

/// An x86-64 `CONTEXT`: 1232 bytes, 16-aligned.
#[repr(C, align(16))]
struct Context([u8; 1232]);

/// One stopper at a time.
static STOPPING: Mutex<()> = Mutex::new(());

/// Why the threads were not all stopped, said once they are let go.
enum Stuck {
    Many,
    Walking(i32),
    Suspending(u32, u32),
    Context(u32, u32),
}

/// The threads of the process, walked and suspended.
pub(super) struct Threads {
    /// The threads suspended this round, each an open handle with its id, and where each
    /// is: room for [`MOST_THREADS`] of each, so a round allocates nothing.
    handles: Vec<usize>,
    ids: Vec<u32>,
    pcs: Vec<u64>,
}

impl Threads {
    pub(super) fn new() -> Threads {
        Threads {
            handles: Vec::with_capacity(MOST_THREADS),
            ids: Vec::with_capacity(MOST_THREADS),
            pcs: Vec::with_capacity(MOST_THREADS),
        }
    }

    /// Walks the threads and suspends each not suspended yet: how many it suspended.
    fn suspend_new(&mut self, me: u32) -> Result<usize, Stuck> {
        let access = THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_LIMITED_INFORMATION;
        // SAFETY: it takes nothing and returns this process's pseudo-handle, never closed.
        let process = unsafe { GetCurrentProcess() };
        let (mut previous, mut kept) = (std::ptr::null_mut(), false);
        let mut suspended = 0;
        let walked = loop {
            let mut next = std::ptr::null_mut();
            // SAFETY: `previous` is null or the handle the last call opened, still open; the
            // next thread's handle is opened into `next`.
            let status = unsafe { NtGetNextThread(process, previous, access, 0, 0, &mut next) };
            if !previous.is_null() && !kept {
                // SAFETY: the handle the last call opened, not kept: closed once.
                unsafe { CloseHandle(previous) };
            }
            previous = std::ptr::null_mut();
            if status == STATUS_NO_MORE_ENTRIES {
                break Ok(suspended);
            }
            if status != 0 {
                break Err(Stuck::Walking(status));
            }
            (previous, kept) = (next, false);
            // SAFETY: `next` is an open handle with THREAD_QUERY_LIMITED_INFORMATION.
            let id = unsafe { GetThreadId(next) };
            if id == me || self.ids.contains(&id) {
                continue;
            }
            if self.handles.len() == self.handles.capacity() {
                break Err(Stuck::Many);
            }
            // SAFETY: an open handle with THREAD_SUSPEND_RESUME, of a thread not this one.
            if unsafe { SuspendThread(next) } == u32::MAX {
                // SAFETY: it takes nothing.
                break Err(Stuck::Suspending(id, unsafe { GetLastError() }));
            }
            kept = true;
            self.handles.push(next as usize);
            self.ids.push(id);
            suspended += 1;
            let mut context = Context([0; 1232]);
            context.0[FLAGS_AT..FLAGS_AT + 4].copy_from_slice(&CONTEXT_CONTROL.to_le_bytes());
            // SAFETY: an open handle with THREAD_GET_CONTEXT of a suspended thread, and a
            // whole, aligned `CONTEXT` asking for the control registers.
            if unsafe { GetThreadContext(next, &mut context) } == 0 {
                // SAFETY: it takes nothing.
                break Err(Stuck::Context(id, unsafe { GetLastError() }));
            }
            let rip =
                u64::from_le_bytes(context.0[RIP_AT..RIP_AT + 8].try_into().expect("8 bytes"));
            self.pcs.push(rip);
        };
        if walked.is_err() && !previous.is_null() && !kept {
            // SAFETY: the handle the walk stopped at, not kept: closed once.
            unsafe { CloseHandle(previous) };
        }
        walked
    }
}

impl World for Threads {
    fn stopped<T>(&mut self, f: impl FnOnce(&[u64]) -> T) -> Result<T, String> {
        let _one = STOPPING.lock().unwrap_or_else(|it| it.into_inner());
        // SAFETY: it takes nothing.
        let me = unsafe { GetCurrentThreadId() };
        self.handles.clear();
        self.ids.clear();
        self.pcs.clear();
        let stopped = loop {
            match self.suspend_new(me) {
                Ok(0) => break Ok(()),
                Ok(_) => {}
                Err(why) => break Err(why),
            }
        };
        let stopped = stopped.map(|()| f(&self.pcs));
        for handle in self.handles.drain(..) {
            // SAFETY: a thread this round suspended once, let go once, by the handle kept
            // since; closed once.
            unsafe { ResumeThread(handle as *mut c_void) };
            // SAFETY: as above.
            unsafe { CloseHandle(handle as *mut c_void) };
        }
        stopped.map_err(|stuck| match stuck {
            Stuck::Many => format!("the program runs more than {MOST_THREADS} threads to stop"),
            Stuck::Walking(status) => {
                format!("walking the program's threads: NTSTATUS {status:#x}")
            }
            Stuck::Suspending(id, code) => format!("suspending thread {id}: error {code}"),
            Stuck::Context(id, code) => format!("reading where thread {id} is: error {code}"),
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

    /// Threads that spin are stopped where they are, and go on after.
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
        let mut threads = Threads::new();
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
