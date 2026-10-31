//! Stopping every other thread of the task on macOS AArch64, for v1's writes over the image
//! ([`World`]).
//!
//! `task_threads` lists the threads, `thread_suspend` stops each (it returns once a thread
//! running on another core has stopped) and `thread_get_state` says where it is. They are
//! listed again after each have stopped until no new one shows up, so one started meanwhile
//! is stopped too, and `thread_resume` lets each go. A thread let go returns to user mode by
//! an exception return, which synchronizes its context: it fetches what was written while it
//! was stopped, once the writer invalidated the instruction cache over it
//! (`sys_icache_invalidate`, which ends in a barrier). Nothing is allocated in the process
//! while the threads are stopped: the kernel's list of threads is memory it maps, given back
//! with `vm_deallocate`, and every right to a thread's port is given back once.

#![allow(unsafe_code, reason = "suspends the process's other threads, and reads their pcs")]

use std::sync::Mutex;

use crate::apply::{MOST_THREADS, World};

unsafe extern "C" {
    fn mach_task_self() -> u32;
    fn mach_thread_self() -> u32;
    fn task_threads(task: u32, threads: *mut *mut u32, count: *mut u32) -> i32;
    fn thread_suspend(thread: u32) -> i32;
    fn thread_resume(thread: u32) -> i32;
    fn thread_get_state(thread: u32, flavor: i32, state: *mut u32, count: *mut u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn vm_deallocate(task: u32, address: usize, size: usize) -> i32;
}

const ARM_THREAD_STATE64: i32 = 6;
/// What `thread_suspend` says of a port whose thread has exited since `task_threads` listed
/// it: the port no longer converts to a thread.
const KERN_INVALID_ARGUMENT: i32 = 4;
const KERN_TERMINATED: i32 = 37;
const MACH_SEND_INVALID_DEST: i32 = 0x1000_0003;
/// What a pointer has of an address, without the bits pointer authentication signs it with.
const ADDRESS: u64 = (1 << 47) - 1;

#[repr(C)]
#[derive(Default)]
#[allow(dead_code)]
struct ThreadState {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    flags: u32,
}

/// One stopper at a time.
static STOPPING: Mutex<()> = Mutex::new(());

/// Why the threads were not all stopped, said once they are let go.
enum Stuck {
    Many,
    Listing(i32),
    Suspending(u32, i32),
    State(u32, i32),
}

/// The threads of the task, listed and suspended.
pub(super) struct Threads {
    /// The threads suspended this round, each with a right to its port, and where each is:
    /// room for [`MOST_THREADS`] of each, so a round allocates nothing.
    ports: Vec<u32>,
    pcs: Vec<u64>,
}

impl Threads {
    pub(super) fn new() -> Threads {
        Threads { ports: Vec::with_capacity(MOST_THREADS), pcs: Vec::with_capacity(MOST_THREADS) }
    }

    /// Lists the threads and suspends each not suspended yet: how many it suspended.
    fn suspend_new(&mut self, task: u32, me: u32) -> Result<usize, Stuck> {
        let (mut list, mut count) = (std::ptr::null_mut::<u32>(), 0u32);
        // SAFETY: `list` and `count` are what the call fills in: an array the kernel maps in
        // this task, and its length.
        let listed = unsafe { task_threads(task, &mut list, &mut count) };
        if listed != 0 {
            return Err(Stuck::Listing(listed));
        }
        let mut stuck = None;
        let mut suspended = 0;
        for nth in 0..count as usize {
            // SAFETY: the kernel gave `count` ports at `list`, read before it is given back.
            let port = unsafe { list.add(nth).read() };
            let keep = stuck.is_none()
                && port != me
                && !self.ports.contains(&port)
                && match self.suspend(port) {
                    Ok(kept) => kept,
                    Err(why) => {
                        stuck = Some(why);
                        // One suspended before its state could be read is kept, to let go.
                        self.ports.last() == Some(&port)
                    }
                };
            if keep {
                suspended += 1;
            } else {
                // SAFETY: a right `task_threads` gave, not kept: given back once.
                unsafe { mach_port_deallocate(task, port) };
            }
        }
        // SAFETY: the kernel's array of `count` ports, not read after this.
        unsafe { vm_deallocate(task, list as usize, count as usize * size_of::<u32>()) };
        match stuck {
            Some(why) => Err(why),
            None => Ok(suspended),
        }
    }

    /// Suspends `port` and reads where it is: whether it is kept, suspended, in `ports`.
    fn suspend(&mut self, port: u32) -> Result<bool, Stuck> {
        if self.ports.len() == self.ports.capacity() {
            return Err(Stuck::Many);
        }
        // SAFETY: `port` names a thread of this task, not this one; it returns once the
        // thread has stopped.
        let suspended = unsafe { thread_suspend(port) };
        if suspended == KERN_TERMINATED
            || suspended == KERN_INVALID_ARGUMENT
            || suspended == MACH_SEND_INVALID_DEST
        {
            return Ok(false);
        }
        if suspended != 0 {
            return Err(Stuck::Suspending(port, suspended));
        }
        self.ports.push(port);
        let mut state = ThreadState::default();
        let mut count = (size_of::<ThreadState>() / 4) as u32;
        // SAFETY: `state` is as large as `count` says, in `u32`s.
        let read = unsafe {
            thread_get_state(port, ARM_THREAD_STATE64, &raw mut state as *mut u32, &mut count)
        };
        if read != 0 {
            return Err(Stuck::State(port, read));
        }
        self.pcs.push(state.pc & ADDRESS);
        Ok(true)
    }
}

impl World for Threads {
    fn stopped<T>(&mut self, f: impl FnOnce(&[u64]) -> T) -> Result<T, String> {
        let _one = STOPPING.lock().unwrap_or_else(|it| it.into_inner());
        // SAFETY: they take nothing; this task's port needs no release, this thread's is
        // given back below.
        let (task, me) = unsafe { (mach_task_self(), mach_thread_self()) };
        self.ports.clear();
        self.pcs.clear();
        let stopped = loop {
            match self.suspend_new(task, me) {
                Ok(0) => break Ok(()),
                Ok(_) => {}
                Err(why) => break Err(why),
            }
        };
        let stopped = stopped.map(|()| f(&self.pcs));
        for port in self.ports.drain(..) {
            // SAFETY: a thread this round suspended once, let go once; the right to its port,
            // kept since `task_threads`, given back once.
            unsafe { thread_resume(port) };
            // SAFETY: as above.
            unsafe { mach_port_deallocate(task, port) };
        }
        // SAFETY: the right `mach_thread_self` gave above, given back once.
        unsafe { mach_port_deallocate(task, me) };
        stopped.map_err(|stuck| match stuck {
            // alloc: error path
            Stuck::Many => format!("the program runs more than {MOST_THREADS} threads to stop"),
            // alloc: error path
            Stuck::Listing(code) => format!("listing the program's threads: kern_return {code}"),
            Stuck::Suspending(port, code) => {
                // alloc: error path
                format!("suspending thread {port:#x}: kern_return {code}")
            }
            Stuck::State(port, code) => {
                // alloc: error path
                format!("reading where thread {port:#x} is: kern_return {code}")
            }
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
