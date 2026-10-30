#![allow(dead_code)]

use salsa::{Database, Storage};

use super::signal::Signal;
use super::sync::thread::{self, ThreadId};
use super::sync::{Arc, Mutex};

/// Various "knobs" and utilities used by tests to force
/// a certain behavior.
#[salsa::db]
pub(crate) trait KnobsDatabase: Database {
    /// Signal that we are entering stage `stage`.
    fn signal(&self, stage: usize);

    /// Wait until we reach stage `stage` (no-op if we have already reached that stage).
    fn wait_for(&self, stage: usize);
}

/// A database containing various "knobs" that can be used to customize how the queries
/// behave. One database is shared by `&` between the threads of a test; the knobs that
/// customize one thread are keyed by that thread.
#[salsa::db]
pub(crate) struct Knobs {
    storage: salsa::Storage<Self>,

    /// A kind of flexible barrier used to coordinate execution across
    /// threads to ensure we reach various weird states.
    pub(crate) signal: Arc<Signal>,

    /// When a thread listed here is about to block, send its signal.
    signal_on_will_block: Arc<Mutex<Vec<(ThreadId, usize)>>>,
}

impl Knobs {
    /// When the calling thread is about to block, send `stage`.
    pub fn signal_on_will_block(&self, stage: usize) {
        let id = thread::current().id();
        let mut knobs = self.signal_on_will_block.lock().unwrap();
        match knobs.iter_mut().find(|(thread, _)| *thread == id) {
            Some((_, old)) => *old = stage,
            None => knobs.push((id, stage)),
        }
    }
}

impl Default for Knobs {
    fn default() -> Self {
        let signal = <Arc<Signal>>::default();
        let signal_on_will_block = Arc::new(Mutex::new(Vec::<(ThreadId, usize)>::new()));

        Self {
            storage: Storage::new(Some(Box::new({
                let signal = signal.clone();
                let signal_on_will_block = signal_on_will_block.clone();
                move |event| {
                    if let salsa::EventKind::WillBlockOn { .. } = event.kind {
                        let stage = signal_on_will_block
                            .lock()
                            .unwrap()
                            .iter()
                            .find(|(thread, _)| *thread == event.thread_id)
                            .map_or(0, |&(_, stage)| stage);
                        signal.signal(stage);
                    }
                }
            }))),
            signal,
            signal_on_will_block,
        }
    }
}

#[salsa::db]
impl salsa::Database for Knobs {}

#[salsa::db]
impl KnobsDatabase for Knobs {
    fn signal(&self, stage: usize) {
        self.signal.signal(stage);
    }

    fn wait_for(&self, stage: usize) {
        self.signal.wait_for(stage);
    }
}
