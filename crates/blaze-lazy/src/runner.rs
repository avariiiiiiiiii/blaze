//! The daemon's side of a lazy run, on threads: [`crate::fetch::Fetcher`] driven against the
//! compiler, the placer and the channel the daemon gives it.
//!
//! - [`Compiler`] compiles one instance: codegen's memo and the store first, the
//!   copy-and-patch backend (blaze-stencil) on a miss in dev and test runs.
//! - [`Placer`] places one compiled function into the run process's code memory with its
//!   relocations resolved (a callee not compiled yet is reached through its slot, whose stub
//!   is placed), then publishes its slot: one release store of the slot's word after the code
//!   is written and visible to the process's instruction fetch (blaze-link into
//!   blaze-inproc's code memory, whose publish does the cache maintenance).
//! - [`Answers`] sends a frame to a run process and stops one; [`Runs`] is blaze-inproc's, and
//!   [`Runner::serve`] reads one run process's asks.
//!
//! Workers take compiles from the fetcher, compile with no lock held, place one at a time,
//! and send the answers after the fetcher's lock is let go. A compile that panics or fails
//! answers `Failed` naming the function; a run that called it is stopped with that error,
//! never left to run something else. The fetcher's lock ([`Shared::state`]) is held only to
//! take or record a compile, never across one; the placer's only across one placement.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use blaze_codegen::{CompiledFunction, InstanceKey};
use blaze_inproc::ask::{self, Reader, Sender};
use blaze_inproc::frame::Kind;
use blaze_items::{CodeHash, ItemKey};
use rustc_hash::FxHashMap;

use crate::{
    LazyId,
    fetch::{Action, Fetcher, Outcome, Priority, Waiter},
    session::Session,
    wire::{self, Frame, WireError},
};

/// Compiles one instance of the build.
pub trait Compiler: Send + Sync + 'static {
    fn compile(&self, key: InstanceKey) -> Result<CompiledFunction, String>;
}

/// Places one function in the running image and publishes its slot; the address it runs at.
pub trait Placer: Send + 'static {
    fn place(&mut self, function: &CompiledFunction) -> Result<u64, String>;
}

/// The run processes' asks.
pub trait Answers: Send + Sync + 'static {
    /// Sends `body` to `client` as a blaze-inproc frame of `kind`, answering the frame of
    /// sequence number `seq`.
    fn send(&self, client: u32, seq: u32, kind: Kind, body: &[u8]);
    /// The run called `function` (its instance and item; none for an id the session doesn't
    /// have), which can't be compiled: stop it with `why`.
    fn stop(&self, function: Option<(InstanceKey, ItemKey)>, why: &str);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Compiles at once.
    pub workers: u32,
    /// Whether every lazy function nothing asked for is compiled at [`Priority::Idle`] once
    /// nothing else is queued: idle cores' work, and how a failing function nobody called is
    /// found ([`Runner::close`]).
    pub idle: bool,
}

struct State {
    fetcher: Fetcher,
    closing: bool,
    idle_queued: bool,
}

struct Shared<C, P, A> {
    session: Session,
    /// The fetcher's state: held only to take or record a compile, never across one.
    state: Mutex<State>,
    wake: Condvar,
    compiler: C,
    /// Held across one placement: the image is placed into one function at a time.
    placer: Mutex<P>,
    answers: A,
    idle: bool,
}

/// One lazy run's daemon side.
pub struct Runner<C, P, A> {
    shared: Arc<Shared<C, P, A>>,
    threads: Vec<JoinHandle<()>>,
}

/// What a closed runner found: the functions that failed to compile, asked for or not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Closed {
    pub failures: Vec<(InstanceKey, ItemKey, String)>,
    pub compiles: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<C: Compiler, P: Placer, A: Answers> Runner<C, P, A> {
    /// Starts `config.workers` threads fetching `session`'s functions, `first` queued.
    pub fn start(
        session: Session,
        first: &[(LazyId, Priority)],
        compiler: C,
        placer: P,
        answers: A,
        config: Config,
    ) -> Result<Runner<C, P, A>, String> {
        let workers = config.workers.max(1);
        let mut fetcher = Fetcher::new(session.plan().len(), workers);
        for &(id, priority) in first {
            fetcher.want(id, priority);
        }
        let shared = Arc::new(Shared {
            session,
            state: Mutex::new(State { fetcher, closing: false, idle_queued: false }),
            wake: Condvar::new(),
            compiler,
            placer: Mutex::new(placer),
            answers,
            idle: config.idle,
        });
        let mut threads = Vec::with_capacity(workers as usize);
        for n in 0..workers {
            let worker = Arc::clone(&shared);
            let spawned =
                std::thread::Builder::new().name(format!("blaze-lazy-{n}")).spawn(move || worker.work());
            match spawned {
                Ok(thread) => threads.push(thread),
                Err(error) => {
                    drop(Runner { shared, threads });
                    return Err(format!("starting a lazy compile thread: {error}"));
                }
            }
        }
        Ok(Runner { shared, threads })
    }

    /// Reads `client`'s asks from `reader` until it ends or sends what is not an ask: why, for
    /// the daemon to stop the run with when it still runs. Blocking: on a thread of the run's.
    pub fn serve(&self, client: u32, reader: &mut Reader) -> String {
        loop {
            let frame = match reader.recv() {
                Ok(frame) => frame,
                Err(error) => return error.to_string(),
            };
            if let Err(error) = self.frame(client, frame.kind, frame.seq, frame.body) {
                return format!("process {client} asked what is not a lazy function's code: {error}");
            }
        }
    }

    /// A blaze-inproc frame of `kind` and sequence number `seq` from `client`'s run process,
    /// with body `bytes`. A frame that is not an ask is an error of the channel, for the
    /// daemon to stop the run with.
    pub fn frame(&self, client: u32, kind: Kind, seq: u32, bytes: &[u8]) -> Result<(), WireError> {
        let frame = Frame::decode(bytes)?;
        if wire::channel_kind(&frame) != kind {
            return Err(WireError::Kind(u32::from(kind.0)));
        }
        match frame {
            Frame::Ask { request, lazy } => {
                self.shared.ask(Waiter { client, seq, request }, lazy);
                Ok(())
            }
            Frame::Ready { .. } | Frame::Failed { .. } => Err(WireError::Unasked),
        }
    }

    /// Queues `ids` at `priority`: the daemon's prefetches, such as an agent's declared intent.
    pub fn want(&self, ids: &[LazyId], priority: Priority) {
        let mut state = lock(&self.shared.state);
        for &id in ids {
            state.fetcher.want(id, priority);
        }
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Where `key`'s code was published, once it was.
    pub fn target(&self, key: &InstanceKey) -> Option<u64> {
        let id = self.shared.session.plan().id(key)?;
        lock(&self.shared.state).fetcher.target(id)
    }

    /// The ids of the lazy functions whose code is among `reached`.
    pub fn reached(&self, reached: &[CodeHash]) -> Vec<LazyId> {
        let mut out = Vec::new();
        self.shared.session.plan().reached(reached, &mut out);
        out
    }

    /// Stops the workers once the compiles running end, and says what failed.
    pub fn close(mut self) -> Closed {
        lock(&self.shared.state).closing = true;
        self.shared.wake.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let state = lock(&self.shared.state);
        let plan = self.shared.session.plan();
        Closed {
            failures: state
                .fetcher
                .failures()
                .map(|(id, why)| (plan.key(id), plan.item(id), why.to_owned()))
                .collect(),
            compiles: state.fetcher.compiles(),
        }
    }
}

impl<C, P, A> Drop for Runner<C, P, A> {
    fn drop(&mut self) {
        if self.threads.is_empty() {
            return;
        }
        lock(&self.shared.state).closing = true;
        self.shared.wake.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl<C: Compiler, P: Placer, A: Answers> Shared<C, P, A> {
    fn ask(&self, to: Waiter, lazy: u32) {
        let mut state = lock(&self.state);
        if let Some(target) = state.fetcher.target(LazyId(lazy)) {
            drop(state);
            self.answers.send(to.client, to.seq, wire::READY_KIND, &wire::ready(to.request, lazy, target));
            return;
        }
        let mut actions = Vec::new();
        state.fetcher.ask(to, lazy, &mut actions);
        drop(state);
        self.wake.notify_all();
        self.act(&actions, &mut Vec::with_capacity(64));
    }

    fn work(&self) {
        let mut actions = Vec::new();
        let mut callees = Vec::new();
        let mut frame = Vec::with_capacity(64);
        loop {
            let (id, _) = {
                let mut state = lock(&self.state);
                loop {
                    if state.closing {
                        return;
                    }
                    if let Some(job) = state.fetcher.next() {
                        break job;
                    }
                    if self.idle && !state.idle_queued && state.fetcher.idle() {
                        state.idle_queued = true;
                        for id in 0..state.fetcher.len() {
                            state.fetcher.want(LazyId(id), Priority::Idle);
                        }
                        continue;
                    }
                    state = self.wake.wait(state).unwrap_or_else(PoisonError::into_inner);
                }
            };
            let placed = self.compile_and_place(id);
            callees.clear();
            if let Ok((_, function)) = &placed {
                self.session.plan().callees(function, &mut callees);
            }
            actions.clear();
            let mut state = lock(&self.state);
            let result = placed.as_ref().map(|it| it.0).map_err(String::as_str);
            state.fetcher.done(id, result, &mut actions);
            for &callee in &callees {
                state.fetcher.want(callee, Priority::Callee);
            }
            drop(state);
            self.wake.notify_all();
            self.act(&actions, &mut frame);
        }
    }

    /// `id`'s code, compiled, placed and published: where it runs and the function, or why
    /// not, naming it.
    fn compile_and_place(&self, id: LazyId) -> Result<(u64, CompiledFunction), String> {
        let plan = self.session.plan();
        let (key, item) = (plan.key(id), plan.item(id));
        let named = |why: &str| format!("compiling {key:?} (item {item:?}) on its first call: {why}");
        let function = match catch_unwind(AssertUnwindSafe(|| self.compiler.compile(key))) {
            Ok(Ok(function)) if function.key == key => function,
            Ok(Ok(function)) => return Err(named(&format!("the compiler gave {:?}", function.key))),
            Ok(Err(why)) => return Err(named(&why)),
            Err(payload) => return Err(named(&format!("the compiler panicked: {}", panic_text(&*payload)))),
        };
        let placed = catch_unwind(AssertUnwindSafe(|| lock(&self.placer).place(&function)));
        match placed {
            Ok(Ok(0)) => Err(named("placed at address 0")),
            Ok(Ok(target)) => Ok((target, function)),
            Ok(Err(why)) => Err(named(&format!("placing it: {why}"))),
            Err(payload) => Err(named(&format!("placing it panicked: {}", panic_text(&*payload)))),
        }
    }

    fn act(&self, actions: &[Action], frame: &mut Vec<u8>) {
        let plan = self.session.plan();
        for action in actions {
            match action {
                Action::Answer { to, lazy, outcome } => {
                    frame.clear();
                    let answer = match outcome {
                        Outcome::Ready(target) => Frame::Ready { request: to.request, lazy: *lazy, target: *target },
                        Outcome::Failed(why) => Frame::Failed { request: to.request, lazy: *lazy, why: fit(why) },
                    };
                    answer.encode(frame);
                    self.answers.send(to.client, to.seq, wire::channel_kind(&answer), frame);
                }
                Action::Stop { lazy, why } => {
                    let function = (*lazy < plan.len()).then(|| (plan.key(LazyId(*lazy)), plan.item(LazyId(*lazy))));
                    self.answers.stop(function, why);
                }
            }
        }
    }
}

/// The most of `why` an answer carries: what fits in an ask's slot, cut at a character. The
/// whole of it is the fetcher's ([`Runner::close`]).
pub(crate) fn fit(why: &str) -> &str {
    let most = ask::MOST_BODY - wire::FAILED_LEN;
    if why.len() <= most {
        return why;
    }
    let end = (0..=most).rev().find(|it| why.is_char_boundary(*it)).unwrap_or(0);
    &why[..end]
}

/// The [`Answers`] of one runner's run processes, which blaze-inproc started: each one's
/// [`Sender`], by process ID, its client. A run whose answer can't be sent, or that called a
/// function that can't be compiled, is killed, that its error.
#[derive(Default)]
pub struct Runs {
    senders: Mutex<FxHashMap<u32, Sender>>,
}

impl Runs {
    /// Answers `sender`'s process from now on: its client.
    pub fn add(&self, sender: Sender) -> u32 {
        let client = sender.pid();
        lock(&self.senders).insert(client, sender);
        client
    }

    /// Once `client`'s process ended.
    pub fn remove(&self, client: u32) {
        lock(&self.senders).remove(&client);
    }
}

fn kill(sender: &Sender, why: &str) {
    if let Err(error) = sender.kill(why) {
        eprintln!("blaze-lazy: {error}");
    }
}

impl Answers for Runs {
    fn send(&self, client: u32, seq: u32, kind: Kind, body: &[u8]) {
        let Some(sender) = lock(&self.senders).get(&client).cloned() else { return };
        if let Err(error) = sender.send(kind, seq, body) {
            kill(&sender, &format!("answering its ask for a lazy function's code: {error}"));
        }
    }

    /// Kills every run process it answers: they run one program, which called the function.
    fn stop(&self, _: Option<(InstanceKey, ItemKey)>, why: &str) {
        let senders: Vec<Sender> = lock(&self.senders).values().cloned().collect();
        for sender in &senders {
            kill(sender, why);
        }
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    match payload.downcast_ref::<&str>() {
        Some(text) => (*text).to_owned(),
        None => match payload.downcast_ref::<String>() {
            Some(text) => text.clone(),
            None => "a payload that is not text".to_owned(),
        },
    }
}
