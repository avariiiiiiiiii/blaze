//! What a process of the daemon says of the build it does, as it does it: the records of
//! [`blaze_protocol::events`], in the file beside the process's socket that the daemon follows
//! ([`blaze_protocol::follow`]). A load, which comes before the process listens, is in it from
//! its first step. The engine says what only it knows (crates/blaze-engine/src/events.rs).

use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::{Mutex, atomic::Ordering},
    time::Duration,
};

use blaze_protocol::events::{Event, Record, Step};

/// The file this process says what it does in, once it has a socket.
static OUT: Mutex<Option<File>> = Mutex::new(None);

/// Has this process say what it does beside `socket` from now on.
pub fn said_beside(socket: &Path) {
    let path = blaze_protocol::follow::beside(socket, std::process::id());
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    if let Ok(file) = crate::socket::private_file(&mut options, &path) {
        *crate::lock(&OUT) = Some(file);
    }
}

/// Whether this process says anything: what it would say need not be worked out otherwise.
pub fn heard() -> bool {
    crate::lock(&OUT).is_some()
}

/// Says `event`. A process with no socket says nothing.
pub fn say(event: Event) {
    let mut out = crate::lock(&OUT);
    let Some(file) = out.as_mut() else { return };
    let record = Record { at_ns: blaze_protocol::follow::now_ns(), event };
    // One write of the whole frame: the daemon reads the file as it grows.
    if let Ok(item) = blaze_protocol::cbor::to_vec(&record) {
        let _ = blaze_protocol::write_frame(file, &item);
    }
}

/// The step the process is at, which the next one ends.
static AT: Mutex<Option<(Step, Option<String>)>> = Mutex::new(None);

/// Says the process is at `step`, of `of` when it is of one crate or program, and done
/// with the one before.
pub fn at(step: Step, of: Option<&str>) {
    let of = of.map(str::to_owned);
    if let Some((step, of)) = crate::lock(&AT).replace((step, of.clone())) {
        say(Event::Ended { step, of });
    }
    say(Event::Started { step, of });
}

/// Does `work` as `step` of `of`, and is at the step it was at before again after.
pub fn within<T>(step: Step, of: Option<&str>, work: impl FnOnce() -> T) -> T {
    let before = crate::lock(&AT).clone();
    at(step, of);
    let done = work();
    match before {
        Some((step, of)) => at(step, of.as_deref()),
        None => idle(),
    }
    done
}

/// Says the process is done with its step: it waits to be asked.
pub fn idle() {
    if let Some((step, of)) = crate::lock(&AT).take() {
        say(Event::Ended { step, of });
    }
}

/// How often a compile says how many functions it has compiled.
pub const COUNTED: Duration = Duration::from_millis(100);

/// The next id of a node of the build's graph ([`Event::Node`]) not said yet.
static NODES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The first of `count` ids of nodes no other node of the process has: whatever says nodes
/// takes them here, so the nodes of one graph and another's are told apart.
pub fn node_ids(count: usize) -> u32 {
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    NODES.fetch_add(count, std::sync::atomic::Ordering::Relaxed)
}

/// A count of what a step does, said every [`COUNTED`] while it goes on, with what is taking
/// longest among what is under way: the files a check reads, the crates names are resolved
/// of. Each worker thread has a slot it says what it works on in.
pub struct Counting {
    shared: Option<std::sync::Arc<Counted>>,
}

struct Counted {
    step: Step,
    total: u64,
    done: std::sync::atomic::AtomicU64,
    /// What each thread works on, and since when.
    slots: Vec<Mutex<Option<(String, std::time::Instant)>>>,
    /// The count and what took longest, as said last.
    said: Mutex<Option<(u64, Option<String>)>>,
}

impl Counted {
    /// Says how far the count is if that is not what was said last; with `over`, that
    /// nothing is under way any more.
    fn tick(&self, over: bool) {
        let done = self.done.load(Ordering::Relaxed);
        let longest = match over {
            true => None,
            false => {
                let slots = self.slots.iter().filter_map(|it| crate::lock(it).clone());
                slots.min_by_key(|it| it.1).map(|it| it.0)
            }
        };
        let now = Some((done, longest));
        let mut said = crate::lock(&self.said);
        if *said != now {
            let longest = now.as_ref().and_then(|it| it.1.clone());
            say(Event::Progress { step: self.step, done, total: self.total, longest });
            *said = now;
        }
    }
}

/// The counts going on, which one thread of the process says every [`COUNTED`]: a count
/// that ends in less starts no thread of its own.
static COUNTS: Mutex<Vec<std::sync::Weak<Counted>>> = Mutex::new(Vec::new());

/// The thread that says the [`COUNTS`], which waits while there are none.
fn ticker() -> &'static std::thread::Thread {
    static TICKER: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();
    TICKER.get_or_init(|| {
        let spawned = std::thread::Builder::new().name("counts".into()).spawn(|| {
            loop {
                let any = {
                    let mut counts = crate::lock(&COUNTS);
                    counts.retain(|it| it.strong_count() > 0);
                    // Said under the lock: a count dropped meanwhile says its last after.
                    counts
                        .iter()
                        .filter_map(std::sync::Weak::upgrade)
                        .for_each(|it| it.tick(false));
                    !counts.is_empty()
                };
                match any {
                    true => std::thread::park_timeout(COUNTED),
                    false => std::thread::park(),
                }
            }
        });
        match spawned {
            Ok(handle) => handle.thread().clone(),
            Err(_) => std::thread::current(),
        }
    })
}

impl Counting {
    /// Counts nothing.
    pub fn none() -> Counting {
        Counting { shared: None }
    }

    /// `slots` threads count toward `total` of `step` from now until this is dropped.
    pub fn new(step: Step, total: usize, slots: usize) -> Counting {
        if !heard() {
            return Counting::none();
        }
        let shared = std::sync::Arc::new(Counted {
            step,
            total: total as u64,
            done: Default::default(),
            slots: (0..slots).map(|_| Mutex::new(None)).collect(),
            said: Mutex::new(None),
        });
        let first = {
            let mut counts = crate::lock(&COUNTS);
            counts.push(std::sync::Arc::downgrade(&shared));
            counts.len() == 1
        };
        let ticker = ticker();
        if first {
            ticker.unpark();
        }
        Counting { shared: Some(shared) }
    }

    /// Thread `slot` works on `what` now; `what` is made only when anyone hears.
    pub fn on(&self, slot: usize, what: impl FnOnce() -> String) {
        if let Some(it) = self.shared.as_ref().and_then(|it| it.slots.get(slot)) {
            *crate::lock(it) = Some((what(), std::time::Instant::now()));
        }
    }

    /// Thread `slot` finished one.
    pub fn done(&self, slot: usize) {
        if let Some(shared) = &self.shared {
            shared.done.fetch_add(1, Ordering::Relaxed);
            if let Some(it) = shared.slots.get(slot) {
                *crate::lock(it) = None;
            }
        }
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            let this = std::sync::Arc::as_ptr(&shared);
            crate::lock(&COUNTS).retain(|it| it.as_ptr() != this);
            shared.tick(true);
        }
    }
}
