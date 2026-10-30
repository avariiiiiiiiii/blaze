//! Locks, condition variables and once cells, as loom's `rt/mutex.rs`, `rt/rwlock.rs` and
//! `rt/condvar.rs`: a thread that cannot take a lock is blocked at its operation until the lock
//! is released; taking one acquires what its last release released.

use super::{Kind, Object, Runtime, Wait};
use crate::clock::Clock;

pub(crate) struct Mutex {
    holder: Option<u8>,
    sync: Clock,
}

pub(crate) struct RwLock {
    /// A bit per reading thread.
    readers: u8,
    writer: Option<u8>,
    sync: Clock,
}

pub(crate) struct Condvar {
    /// Waiting threads, first come first notified.
    waiters: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnceState {
    Empty,
    Running(u8),
    Done,
}

pub(crate) struct Once {
    pub(crate) state: OnceState,
    /// The value, boxed by the cell that knows its type; null when it is the cell's own (set
    /// outside the model) or there is none.
    pub(crate) value: *mut (),
    sync: Clock,
}

/// What a once cell's operation found.
pub(crate) enum Found {
    /// Initialized: the value (null: the cell's own).
    Done(*mut ()),
    /// Not initialized, and the caller now initializes it.
    Run,
    /// Not initialized (a `get`).
    Empty,
}

impl Runtime {
    pub(crate) fn mutex(&mut self, id: &std::sync::atomic::AtomicU64) -> u32 {
        self.object(id, |_| Object::Mutex(Mutex { holder: None, sync: Clock::ZERO }))
    }

    pub(crate) fn rwlock(&mut self, id: &std::sync::atomic::AtomicU64) -> u32 {
        self.object(id, |_| {
            Object::RwLock(RwLock { readers: 0, writer: None, sync: Clock::ZERO })
        })
    }

    pub(crate) fn condvar(&mut self, id: &std::sync::atomic::AtomicU64) -> u32 {
        self.object(id, |_| Object::Condvar(Condvar { waiters: Vec::new() }))
    }

    pub(crate) fn once(&mut self, id: &std::sync::atomic::AtomicU64, set: bool) -> u32 {
        let state = if set { OnceState::Done } else { OnceState::Empty };
        self.object(id, |_| Object::Once(Once { state, value: std::ptr::null_mut(), sync: Clock::ZERO }))
    }

    fn lock_mut(&mut self, obj: u32) -> &mut Mutex {
        match &mut self.objects[obj as usize].obj {
            Object::Mutex(it) => it,
            _ => unreachable!("blaze-check: a lock operation on another object"),
        }
    }

    fn rw_mut(&mut self, obj: u32) -> &mut RwLock {
        match &mut self.objects[obj as usize].obj {
            Object::RwLock(it) => it,
            _ => unreachable!("blaze-check: a lock operation on another object"),
        }
    }

    pub(crate) fn once_mut(&mut self, obj: u32) -> &mut Once {
        match &mut self.objects[obj as usize].obj {
            Object::Once(it) => it,
            _ => unreachable!("blaze-check: a once operation on another object"),
        }
    }

    fn block_me(&mut self, obj: u32) {
        let me = self.current();
        self.threads[me].state = super::State::Blocked(Wait::Object(obj));
    }

    /// Before `lock`: blocks the thread while another holds it.
    pub(crate) fn mutex_wait(&mut self, obj: u32) -> Result<(), String> {
        let me = self.current();
        match self.lock_mut(obj).holder {
            Some(holder) if holder as usize == me => {
                Err(format!("{} locks a mutex it holds: a deadlock", self.describe(me)))
            }
            Some(_) => {
                self.block_me(obj);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Takes the mutex if it is free.
    pub(crate) fn mutex_take(&mut self, obj: u32) -> bool {
        let me = self.current();
        let lock = self.lock_mut(obj);
        if lock.holder.is_some() {
            return false;
        }
        lock.holder = Some(me as u8);
        let sync = lock.sync;
        self.threads[me].clock.join(&sync);
        self.block_others(obj, |kind| kind == Kind::Write);
        true
    }

    pub(crate) fn mutex_release(&mut self, obj: u32) {
        let me = self.tick();
        let clock = self.threads[me].clock;
        let lock = self.lock_mut(obj);
        lock.holder = None;
        lock.sync.join(&clock);
        self.wake(Wait::Object(obj));
    }

    /// Before `read` (`write: false`) or `write`: blocks the thread while it cannot take it.
    pub(crate) fn rw_wait(&mut self, obj: u32, write: bool) -> Result<(), String> {
        let me = self.current();
        let lock = self.rw_mut(obj);
        if lock.writer == Some(me as u8) || (write && lock.readers & (1 << me) != 0) {
            return Err(format!("{} takes a lock it holds: a deadlock", self.describe(me)));
        }
        if lock.writer.is_some() || (write && lock.readers != 0) {
            self.block_me(obj);
        }
        Ok(())
    }

    pub(crate) fn rw_take(&mut self, obj: u32, write: bool) -> bool {
        let me = self.current();
        let lock = self.rw_mut(obj);
        if lock.writer.is_some() || (write && lock.readers != 0) {
            return false;
        }
        match write {
            true => lock.writer = Some(me as u8),
            false => lock.readers |= 1 << me,
        }
        let sync = lock.sync;
        self.threads[me].clock.join(&sync);
        match write {
            true => self.block_others(obj, |kind| matches!(kind, Kind::Read | Kind::Write)),
            false => self.block_others(obj, |kind| kind == Kind::Write),
        }
        true
    }

    pub(crate) fn rw_release(&mut self, obj: u32, write: bool) {
        let me = self.tick();
        let clock = self.threads[me].clock;
        let lock = self.rw_mut(obj);
        lock.sync.join(&clock);
        match write {
            true => lock.writer = None,
            false => lock.readers &= !(1 << me),
        }
        if lock.writer.is_none() && lock.readers == 0 {
            self.wake(Wait::Object(obj));
        }
    }

    pub(crate) fn condvar_enqueue(&mut self, obj: u32) {
        let me = self.current() as u8;
        match &mut self.objects[obj as usize].obj {
            Object::Condvar(it) => it.waiters.push(me),
            _ => unreachable!("blaze-check: a condvar operation on another object"),
        }
    }

    pub(crate) fn condvar_notify(&mut self, obj: u32, all: bool) {
        let woken: Vec<u8> = match &mut self.objects[obj as usize].obj {
            Object::Condvar(it) if all => std::mem::take(&mut it.waiters),
            Object::Condvar(it) if !it.waiters.is_empty() => vec![it.waiters.remove(0)],
            Object::Condvar(_) => Vec::new(),
            _ => unreachable!("blaze-check: a condvar operation on another object"),
        };
        for thread in woken {
            let th = &mut self.threads[thread as usize];
            if th.state == super::State::Blocked(Wait::Notify(obj)) {
                th.state = super::State::Runnable;
            }
        }
    }

    /// Before an initialization of a once cell: blocks the thread while another initializes it.
    pub(crate) fn once_wait(&mut self, obj: u32) -> Result<(), String> {
        let me = self.current();
        match self.once_mut(obj).state {
            OnceState::Running(by) if by as usize == me => Err(format!(
                "{} initializes a once cell from inside its own initialization: a deadlock",
                self.describe(me)
            )),
            OnceState::Running(_) => {
                self.block_me(obj);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A look at a once cell: initialized, or, with `init`, the caller's to initialize.
    pub(crate) fn once_look(&mut self, obj: u32, init: bool) -> Found {
        let me = self.current();
        let once = self.once_mut(obj);
        match once.state {
            OnceState::Done => {
                let (value, sync) = (once.value, once.sync);
                self.threads[me].clock.join(&sync);
                Found::Done(value)
            }
            OnceState::Empty if init => {
                once.state = OnceState::Running(me as u8);
                self.block_others(obj, |kind| kind == Kind::Write);
                Found::Run
            }
            OnceState::Empty | OnceState::Running(_) => Found::Empty,
        }
    }

    /// The caller's initialization ended with `value` (boxed by the cell).
    pub(crate) fn once_done(&mut self, obj: u32, value: *mut ()) {
        let me = self.tick();
        let clock = self.threads[me].clock;
        let once = self.once_mut(obj);
        once.state = OnceState::Done;
        once.value = value;
        once.sync = clock;
        self.wake(Wait::Object(obj));
    }
}
