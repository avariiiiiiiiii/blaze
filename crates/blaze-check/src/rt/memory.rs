//! Weak memory and races. Each atomic keeps its stores in modification order, as Miri's
//! `concurrency/weak_memory.rs` (a port of tsan11 and of "Dynamic Race Detection for C++11",
//! with Miri's C++20 changes), and a load may read any store coherence and the SC rules leave
//! it: where Miri picks one at random, every candidate here is a branch of the path. Each
//! `UnsafeCell`, and each atomic's non-atomic accesses (`get_mut`, `into_inner`), keeps its last
//! write and each thread's last read, checked against the accessing thread's clock.

use std::{collections::VecDeque, sync::atomic::Ordering};

use super::{At, Object, Runtime};
use crate::clock::{Clock, MAX_THREADS, Stamp, Tid};

/// Stores kept per atomic (Miri's `STORE_BUFFER_LIMIT`); older ones can no longer be read.
pub(crate) const HISTORY: usize = 128;

const UNSEEN: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub(crate) struct Store {
    value: u64,
    stamp: Stamp,
    /// What a load that reads it acquires.
    sync: Clock,
    /// SC, or happens before or is coherence-ordered before an SC store.
    seq_cst: bool,
    sc_loaded: bool,
    /// Each thread's clock at its first load of it.
    loaded: [u32; MAX_THREADS],
}

impl Store {
    fn initial(value: u64) -> Store {
        Store {
            value,
            stamp: Stamp::INIT,
            sync: Clock::ZERO,
            seq_cst: false,
            sc_loaded: false,
            loaded: [UNSEEN; MAX_THREADS],
        }
    }

    /// A load that happens before `clock` read it.
    fn read_before(&self, clock: &Clock) -> bool {
        self.loaded.iter().enumerate().any(|(thread, &at)| at != UNSEEN && at <= clock.get(thread))
    }
}

pub(crate) struct Atomic {
    pub(crate) stores: VecDeque<Store>,
    /// The last non-atomic write (its initial value, or `get_mut`), and where.
    plain: Stamp,
    plain_at: Option<At>,
    /// Each thread's last atomic access, and where.
    accessed: Clock,
    accessed_at: [Option<At>; MAX_THREADS],
    /// `get_mut` handed out the value: the next access starts the history again from it.
    pub(crate) dirty: bool,
}

pub(crate) struct Cell {
    write: Stamp,
    write_at: Option<At>,
    reads: Clock,
    reads_at: [Option<At>; MAX_THREADS],
}

impl Cell {
    pub(crate) fn new() -> Cell {
        Cell { write: Stamp::INIT, write_at: None, reads: Clock::ZERO, reads_at: [None; MAX_THREADS] }
    }
}

fn acquires(ord: Ordering) -> bool {
    matches!(ord, Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst)
}

fn releases(ord: Ordering) -> bool {
    matches!(ord, Ordering::Release | Ordering::AcqRel | Ordering::SeqCst)
}

fn place(at: Option<At>) -> String {
    at.map_or_else(|| "its creation".to_owned(), |at| at.to_string())
}

impl Runtime {
    /// The atomic `id` names, made with the value `initial` holds now if it names none.
    pub(crate) fn atomic(&mut self, id: &std::sync::atomic::AtomicU64, initial: u64) -> u32 {
        self.object(id, |rt| {
            let mut stores = rt.spare.pop().unwrap_or_default();
            stores.push_back(Store::initial(initial));
            Object::Atomic(Atomic {
                stores,
                plain: Stamp::INIT,
                plain_at: None,
                accessed: Clock::ZERO,
                accessed_at: [None; MAX_THREADS],
                dirty: false,
            })
        })
    }

    fn atomic_mut(&mut self, obj: u32) -> &mut Atomic {
        match &mut self.objects[obj as usize].obj {
            Object::Atomic(atomic) => atomic,
            _ => unreachable!("blaze-check: an atomic operation on another object"),
        }
    }

    /// Checks an atomic access against the last non-atomic write, records it, and starts the
    /// history again from `real` if `get_mut` changed the value.
    fn atomic_access(&mut self, obj: u32, thread: Tid, at: At, real: u64) -> Result<(), String> {
        let clock = self.threads[thread].clock;
        let atomic = self.atomic_mut(obj);
        if !atomic.plain.before(&clock) {
            let (writer, plain_at) = (atomic.plain.thread, atomic.plain_at);
            return Err(format!(
                "data race: an atomic access at {at} by thread {thread} races with a non-atomic \
                 write (`get_mut`) at {} by thread {writer}",
                place(plain_at)
            ));
        }
        if atomic.dirty {
            atomic.dirty = false;
            atomic.stores.clear();
            atomic.stores.push_back(Store::initial(real));
        }
        atomic.accessed.set(thread, clock.get(thread));
        atomic.accessed_at[thread] = Some(at);
        Ok(())
    }

    /// An atomic load: which store it reads is a branch (Miri's `fetch_store`).
    pub(crate) fn load(&mut self, obj: u32, ord: Ordering, at: At, real: u64) -> Result<u64, String> {
        let thread = self.current();
        self.atomic_access(obj, thread, at, real)?;
        let seq_cst = ord == Ordering::SeqCst;
        if seq_cst {
            let fence = self.last_sc_fence;
            self.threads[thread].read_seqcst.join(&fence);
        }
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.clear();
        let th = &self.threads[thread];
        let atomic = match &self.objects[obj as usize].obj {
            Object::Atomic(atomic) => atomic,
            _ => unreachable!("blaze-check: an atomic operation on another object"),
        };
        let newest = atomic.stores.len() - 1;
        let mut found_sc = false;
        for (index, store) in atomic.stores.iter().enumerate().rev() {
            let include = if seq_cst && store.seq_cst {
                // An SC load ignores all but the last SC store (stores not SC aren't affected).
                !std::mem::replace(&mut found_sc, true)
            } else {
                true
            };
            // A thread that yielded doesn't read again an old store it read before the yield,
            // so a loop that waits for a newer one ends (loom's `is_seen_before_yield`).
            let seen = store.loaded[thread];
            let stale = index != newest && seen != UNSEEN && th.last_yield.is_some_and(|it| seen <= it);
            if include && !stale {
                candidates.push(index as u16);
            }
            // Nothing earlier in modification order may be read past a store that:
            let hides = store.stamp.before(&th.clock) // happens before the load (CoWR)
                || store.read_before(&th.clock) // a load before this one read (CoRR)
                // an SC fence before the load saw (C++17 32.4 [atomics.order] 4)
                || (store.seq_cst && store.stamp.before(&th.write_seqcst))
                // was before an SC fence an SC load saw (paragraph 5)
                || (seq_cst && store.stamp.before(&th.read_seqcst))
                // an SC load read, for an SC load (C++20 coherence-ordered-before, Miri #2301)
                || (seq_cst && store.sc_loaded);
            if hides {
                break;
            }
        }
        let choice = self.path.branch_load(candidates.len()).map_err(|stuck| self.stuck(stuck));
        let index = choice.map(|choice| candidates[choice] as usize);
        self.candidates = candidates;
        let index = index?;
        let clock = self.threads[thread].clock;
        let atomic = self.atomic_mut(obj);
        let store = &mut atomic.stores[index];
        if store.loaded[thread] == UNSEEN {
            store.loaded[thread] = clock.get(thread);
        }
        store.sc_loaded |= seq_cst;
        let (value, sync) = (store.value, store.sync);
        let th = &mut self.threads[thread];
        match acquires(ord) {
            true => th.clock.join(&sync),
            false => th.fence_acquire.join(&sync),
        }
        Ok(value)
    }

    /// An atomic store, last in modification order (Miri's `store_impl`).
    pub(crate) fn store(&mut self, obj: u32, value: u64, ord: Ordering, at: At, real: u64) -> Result<(), String> {
        let thread = self.current();
        self.atomic_access(obj, thread, at, real)?;
        let th = &self.threads[thread];
        // C++20: a relaxed store ends a release sequence; it releases only what a release
        // fence before it did.
        let sync = if releases(ord) { th.clock } else { th.fence_release };
        self.push_store(obj, thread, value, sync, ord == Ordering::SeqCst);
        Ok(())
    }

    fn push_store(&mut self, obj: u32, thread: Tid, value: u64, sync: Clock, seq_cst: bool) {
        let clock = self.threads[thread].clock;
        if seq_cst {
            self.last_sc_write.set(thread, clock.get(thread));
        }
        let atomic = self.atomic_mut(obj);
        if seq_cst {
            // What happens before or is coherence-ordered before an SC store is SC for later
            // SC loads.
            for store in &mut atomic.stores {
                if store.stamp.before(&clock) || store.read_before(&clock) {
                    store.seq_cst = true;
                }
            }
        }
        if atomic.stores.len() == HISTORY {
            atomic.stores.pop_front();
        }
        atomic.stores.push_back(Store {
            value,
            stamp: Stamp { thread: thread as u8, at: clock.get(thread) },
            sync,
            seq_cst,
            sc_loaded: false,
            loaded: [UNSEEN; MAX_THREADS],
        });
    }

    /// A read-modify-write, or a compare-exchange: it reads the newest store. `f` gives the
    /// value to write, or `None` (a failed compare-exchange: a load with `failure`).
    pub(crate) fn rmw(
        &mut self,
        obj: u32,
        success: Ordering,
        failure: Ordering,
        at: At,
        real: u64,
        f: impl FnOnce(u64) -> Option<u64>,
    ) -> Result<Result<u64, u64>, String> {
        let thread = self.current();
        self.atomic_access(obj, thread, at, real)?;
        let clock = self.threads[thread].clock;
        let atomic = self.atomic_mut(obj);
        let newest = atomic.stores.back_mut().expect("an atomic has a store");
        let old = newest.value;
        let new = f(old);
        let ord = if new.is_some() { success } else { failure };
        let seq_cst = ord == Ordering::SeqCst;
        if newest.loaded[thread] == UNSEEN {
            newest.loaded[thread] = clock.get(thread);
        }
        newest.sc_loaded |= seq_cst;
        let sync = newest.sync;
        if seq_cst {
            let fence = self.last_sc_fence;
            self.threads[thread].read_seqcst.join(&fence);
        }
        let th = &mut self.threads[thread];
        match acquires(ord) {
            true => th.clock.join(&sync),
            false => th.fence_acquire.join(&sync),
        }
        let Some(new) = new else { return Ok(Err(old)) };
        // A release sequence goes on through an RMW: it releases what the store it replaces
        // did, and its own.
        let mut released = sync;
        released.join(if releases(success) { &th.clock } else { &th.fence_release });
        self.push_store(obj, thread, new, released, seq_cst);
        Ok(Ok(old))
    }

    /// A fence (Miri's `atomic_fence`). An SC fence is an AcqRel RMW of one global clock.
    pub(crate) fn fence(&mut self, ord: Ordering) {
        let thread = self.current();
        let th = &mut self.threads[thread];
        if ord != Ordering::Release {
            let acquired = th.fence_acquire;
            th.clock.join(&acquired);
        }
        if ord == Ordering::SeqCst {
            self.last_sc_fence.join(&th.clock);
            th.clock.join(&self.last_sc_fence);
            th.write_seqcst.join(&self.last_sc_write);
        }
        if ord != Ordering::Acquire {
            th.fence_release = th.clock;
        }
    }

    /// `get_mut`: a non-atomic write, which every atomic access must happen before. Gives the
    /// newest value, which the caller writes where `&mut` points.
    pub(crate) fn plain_write(&mut self, obj: u32, at: At) -> Result<u64, String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        let atomic = self.atomic_mut(obj);
        if let Some(other) = clock.behind(&atomic.accessed) {
            return Err(format!(
                "data race: a non-atomic write (`get_mut`) at {at} by thread {thread} races with an \
                 atomic access at {} by thread {other}",
                place(atomic.accessed_at[other])
            ));
        }
        atomic.plain = Stamp { thread: thread as u8, at: clock.get(thread) };
        atomic.plain_at = Some(at);
        atomic.dirty = true;
        Ok(atomic.stores.back().expect("an atomic has a store").value)
    }

    /// `into_inner`: a non-atomic read of the newest value, which every store must happen
    /// before.
    pub(crate) fn plain_read(&mut self, obj: u32, at: At, real: u64) -> Result<u64, String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        let atomic = self.atomic_mut(obj);
        if atomic.dirty {
            return Ok(real);
        }
        let newest = atomic.stores.back().expect("an atomic has a store");
        if !newest.stamp.before(&clock) {
            return Err(format!(
                "data race: a non-atomic read (`into_inner`) at {at} by thread {thread} races \
                 with an atomic store by thread {}",
                newest.stamp.thread
            ));
        }
        Ok(newest.value)
    }

    fn cell_mut(&mut self, obj: u32) -> &mut Cell {
        match &mut self.objects[obj as usize].obj {
            Object::Cell(cell) => cell,
            _ => unreachable!("blaze-check: a cell access to another object"),
        }
    }

    /// A read of an `UnsafeCell`: the last write must happen before it.
    pub(crate) fn cell_read(&mut self, obj: u32, at: At) -> Result<(), String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        let cell = self.cell_mut(obj);
        if !cell.write.before(&clock) {
            return Err(format!(
                "data race on an `UnsafeCell`: a read at {at} by thread {thread} races with the \
                 write at {} by thread {}",
                place(cell.write_at),
                cell.write.thread
            ));
        }
        cell.reads.set(thread, clock.get(thread));
        cell.reads_at[thread] = Some(at);
        Ok(())
    }

    /// A write of an `UnsafeCell`: the last write and every read must happen before it.
    pub(crate) fn cell_write(&mut self, obj: u32, at: At) -> Result<(), String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        let cell = self.cell_mut(obj);
        if !cell.write.before(&clock) {
            return Err(format!(
                "data race on an `UnsafeCell`: a write at {at} by thread {thread} races with the \
                 write at {} by thread {}",
                place(cell.write_at),
                cell.write.thread
            ));
        }
        if let Some(other) = clock.behind(&cell.reads) {
            return Err(format!(
                "data race on an `UnsafeCell`: a write at {at} by thread {thread} races with the \
                 read at {} by thread {other}",
                place(cell.reads_at[other])
            ));
        }
        cell.write = Stamp { thread: thread as u8, at: clock.get(thread) };
        cell.write_at = Some(at);
        Ok(())
    }
}
