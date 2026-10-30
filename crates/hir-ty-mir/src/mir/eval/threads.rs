//! The threads of an evaluation. Each has its own code stack, stack region, thread-local
//! keys, `#[thread_local]` statics and panic being unwound. One runs at a time: the running
//! one's are the evaluator's own fields (`stack`, `code_stack`, ...), the others' wait in
//! [`Thread`], and the machine's `schedule` picks which runs at every terminator of the
//! outermost run.

use std::mem;

use hir_def::StaticId;
use rustc_hash::FxHashMap;

use super::{
    Address, Allocated, BasicBlockId, Evaluator, Interval, MirBody, MirEvalError, MirSpan,
    Result, Running, StackFrame, Then, TlsData,
    machine::{EvalThread, Machine},
    not_supported, stack_offset,
    unwind::Unwinding,
};

/// What a blocked thread waits for; the evaluator wakes it and finishes the call that blocked.
pub(super) enum Wait {
    /// `pthread_join`: `thread` to end. On waking, its return value (a pointer) is written to
    /// `retval` unless that is null, then `0` to the blocked call's destination.
    Join { thread: EvalThread, retval: Address },
    /// The futex word at `addr`: woken by a `wake` of `addr` whose bitset meets `bitset`
    /// (`u32::MAX` for a plain wait). On waking `woken` is written to the destination. When
    /// every thread waits and this one has a `timeout`, it wakes with those bytes instead (a
    /// timed wait times out only when nothing else can run, and the virtual clock moves to
    /// its deadline).
    Futex { addr: usize, bitset: u32, woken: Vec<u8>, timeout: Option<(u64, Vec<u8>)> },
}

/// The bytes of the stack region of each thread: thread `t`'s starts at `t` times this.
pub(super) fn thread_stack_span() -> usize {
    stack_offset() >> 4
}

/// How many threads the addresses of the stack have room for.
const MAX_THREADS: usize = 240;

/// A pthread key made by `pthread_key_create`, with its destructor (a function pointer's
/// bytes) if it has one; `None` once deleted.
pub(super) type TlsKey = Option<Option<usize>>;

pub(super) enum Run {
    /// Not started yet: it calls `start` with `arg` when first picked.
    Start { start: usize, arg: Address },
    Runnable,
    /// Waits on `wait`, to finish the call whose destination is `destination`. `since` orders
    /// the waiters of one futex, longest-waiting first.
    Blocked { wait: Wait, destination: Interval, since: u64 },
    /// Its start function returned and its exit destructors ran.
    Ended,
}

/// What a woken thread does as it goes on, in its own context: the machine sees the
/// synchronization, then the writes that finish the call it blocked in.
#[derive(Default)]
pub(super) struct Resume {
    /// The futex word it acquires (`Machine::acquire`).
    acquire: Option<u64>,
    /// The thread it joined (`Machine::join`).
    joined: Option<EvalThread>,
    /// A pointer to write (`retval` of a join), and the bytes of the call's result.
    pointer: Option<(Address, Address)>,
    result: Option<(Interval, Vec<u8>)>,
}

pub(super) struct Thread<'db> {
    pub(super) run: Run,
    /// While another thread runs: its frames, the block its top frame goes on at, its
    /// stack's bytes and allocations, and the rest of what the evaluator keeps of the
    /// running thread.
    frames: Vec<StackFrame<'db>>,
    block: BasicBlockId,
    stack: Vec<u8>,
    stack_allocations: Vec<Allocated>,
    tls: TlsData,
    statics: FxHashMap<StaticId, Address>,
    unwinding: Option<Unwinding<'db>>,
    running: Option<Running<'db>>,
    resume: Resume,
    /// Calls to make when its start function returns, last registered first.
    at_exit: Vec<(usize, Address)>,
    /// What its start function returned, once it did.
    result: Option<Address>,
    /// Where its start function's return value is written.
    result_slot: Interval,
    joined: bool,
    detached: bool,
    /// The body that made it and the call's span: its start's types are read in that body.
    origin: (&'db MirBody<'db>, MirSpan),
}

impl<'db> Thread<'db> {
    fn new(run: Run, result_slot: Interval, origin: (&'db MirBody<'db>, MirSpan)) -> Self {
        Thread {
            run,
            frames: Vec::new(),
            block: origin.0.start_block,
            stack: Vec::new(),
            stack_allocations: Vec::new(),
            tls: TlsData::default(),
            statics: FxHashMap::default(),
            unwinding: None,
            running: None,
            resume: Resume::default(),
            at_exit: Vec::new(),
            result: None,
            result_slot,
            joined: false,
            detached: false,
            origin,
        }
    }

    /// The bytes of the stack of a thread that doesn't run.
    pub(super) fn stack(&self) -> &Vec<u8> {
        &self.stack
    }

    pub(super) fn stack_mut(&mut self) -> &mut Vec<u8> {
        &mut self.stack
    }

    pub(super) fn stack_allocations(&self) -> &[Allocated] {
        &self.stack_allocations
    }
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// The running thread.
    pub(super) fn thread(&self) -> EvalThread {
        self.thread
    }

    /// Where the running thread's stack region starts, in the stack's offsets.
    pub(super) fn stack_base(&self) -> usize {
        self.thread as usize * thread_stack_span()
    }

    /// A new thread calling `start` (the bytes of a function pointer, `extern "C" fn(*mut u8)
    /// -> *mut u8`) with `arg`; it first runs when the scheduler picks it. Tells the machine.
    pub(super) fn spawn_thread(
        &mut self,
        start: usize,
        arg: Address,
        span: MirSpan,
    ) -> Result<'db, EvalThread> {
        let Some(origin) = self.body_running else {
            not_supported!("spawning a thread outside of a function's body");
        };
        if self.threads.is_empty() {
            // The main thread, which runs: its state is the evaluator's own fields.
            let slot = Interval::new(Address::invalid(1), 0);
            self.threads.push(Thread::new(Run::Runnable, slot, (origin, span)));
        }
        if self.threads.len() >= MAX_THREADS {
            not_supported!("more than {MAX_THREADS} threads");
        }
        let id = self.threads.len() as EvalThread;
        let size = self.ptr_size();
        let result_slot = Interval::new(self.heap_allocate(size, size)?, size);
        self.machine.spawn(id)?;
        self.threads.push(Thread::new(Run::Start { start, arg }, result_slot, (origin, span)));
        Ok(id)
    }

    /// Blocks the running thread on `wait` once the shim returns (the shim writes nothing to
    /// the destination): its call completes when it wakes. A wait that is already satisfied
    /// (`Join` of an ended thread) completes at once.
    pub(super) fn block_on(&mut self, wait: Wait) {
        self.waiting = Some(wait);
    }

    /// Wakes up to `count` threads waiting on the futex `addr` whose bitset meets `bitset`,
    /// longest-waiting first; how many woke. Tells the machine (`release`, then `acquire` as
    /// each woken one goes on).
    pub(super) fn wake(&mut self, addr: usize, bitset: u32, count: usize) -> Result<'db, usize> {
        self.machine.release(addr as u64)?;
        let mut waiters: Vec<(u64, usize)> = self
            .threads
            .iter()
            .enumerate()
            .filter_map(|(i, thread)| match &thread.run {
                Run::Blocked { wait: Wait::Futex { addr: at, bitset: bits, .. }, since, .. }
                    if *at == addr && bits & bitset != 0 =>
                {
                    Some((*since, i))
                }
                _ => None,
            })
            .collect();
        waiters.sort_unstable();
        let woken = waiters.len().min(count);
        for &(_, i) in &waiters[..woken] {
            self.wake_thread(i, false);
        }
        Ok(woken)
    }

    /// `pthread_detach`: nobody joins it; an error if it was joined or detached.
    pub(super) fn detach_thread(&mut self, thread: EvalThread) -> Result<'db, ()> {
        if !self.joinable(thread) {
            return Err(MirEvalError::UndefinedBehavior(format!(
                "detaching thread {thread}, which was joined or detached, or never made"
            )));
        }
        self.machine.detach(thread)?;
        self.threads[thread as usize].detached = true;
        Ok(())
    }

    /// Whether `thread` was made and is not yet joined or detached.
    pub(super) fn joinable(&self, thread: EvalThread) -> bool {
        match self.threads.get(thread as usize) {
            Some(it) => !it.joined && !it.detached,
            // The main thread of a run that made no other.
            None => thread == 0 && self.threads.is_empty(),
        }
    }

    /// `sched_yield`: the machine's `yield_now`, then a switch where the call ends, to another
    /// thread if one can run.
    pub(super) fn yield_thread(&mut self) {
        self.machine.yield_now();
        self.yielded = true;
    }

    /// A new pthread key, with the destructor `dtor` (a function pointer's bytes, `unsafe
    /// extern "C" fn(*mut u8)`) if it has one.
    pub(super) fn tls_create_key(&mut self, dtor: Option<usize>) -> usize {
        self.tls_keys.push(Some(dtor));
        self.tls_keys.len() - 1
    }

    pub(super) fn tls_delete_key(&mut self, key: usize) -> Result<'db, ()> {
        match self.tls_keys.get_mut(key) {
            Some(it @ Some(_)) => {
                *it = None;
                Ok(())
            }
            _ => Err(MirEvalError::UndefinedBehavior(format!("deleting invalid tls key {key}"))),
        }
    }

    /// The running thread's value of `key`.
    pub(super) fn tls_get(&self, key: usize) -> Result<'db, u128> {
        self.check_tls_key(key)?;
        Ok(self.thread_local_storage.keys.get(key).copied().unwrap_or(0))
    }

    pub(super) fn tls_set(&mut self, key: usize, value: u128) -> Result<'db, ()> {
        self.check_tls_key(key)?;
        let keys = &mut self.thread_local_storage.keys;
        if keys.len() <= key {
            keys.resize(key + 1, 0);
        }
        keys[key] = value;
        Ok(())
    }

    fn check_tls_key(&self, key: usize) -> Result<'db, ()> {
        match self.tls_keys.get(key) {
            Some(Some(_)) => Ok(()),
            _ => Err(MirEvalError::UndefinedBehavior(format!("using invalid tls key {key}"))),
        }
    }

    /// A destructor to call (`func`: a function pointer's bytes, `unsafe extern "C"
    /// fn(*mut u8)`) with `arg` when the running thread's start function returns, last
    /// registered first. The main thread's are not run: the run ends when it returns.
    pub(super) fn at_thread_exit(&mut self, func: usize, arg: Address) {
        self.at_exit.push((func, arg));
    }

    /// The stack region of `thread`: its lowest address and its size.
    pub(super) fn thread_stack(&self, thread: EvalThread) -> (usize, usize) {
        let span = thread_stack_span();
        (Address::stack(thread as usize * span).to_usize(), span)
    }

    /// The virtual clock, in nanoseconds since the run started.
    pub(super) fn now(&self) -> u64 {
        self.clock
    }

    /// Advances the virtual clock by `nanos` (`nanosleep`).
    pub(super) fn sleep(&mut self, nanos: u64) {
        self.clock = self.clock.saturating_add(nanos);
        self.yield_thread();
    }

    /// Whether the running thread may switch: there are others, and the evaluator is in its
    /// outermost run (a nested one is a Rust call of its own, which can't be suspended).
    pub(super) fn may_switch(&self) -> bool {
        self.threads.len() > 1 && self.nesting == 1
    }

    /// At a terminator: the thread the machine picks to run next, if not the running one.
    pub(super) fn pick_at_terminator(&mut self) -> Result<'db, Option<EvalThread>> {
        if !self.may_switch() {
            self.yielded = false;
            return Ok(None);
        }
        let yielded = mem::take(&mut self.yielded);
        let mut runnable = self.runnable();
        if yielded && runnable.len() > 1 {
            runnable.retain(|&it| it != self.thread);
        }
        let next = self.machine.schedule(self.thread, &runnable)?;
        if next == self.thread {
            return Ok(None);
        }
        if !runnable.contains(&next) {
            return Err(MirEvalError::InternalError(
                format!("the machine scheduled thread {next}, which can't run").into(),
            ));
        }
        Ok(Some(next))
    }

    /// The threads that can run: the running one, those runnable or not started.
    fn runnable(&self) -> Vec<EvalThread> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(i, thread)| {
                *i as EvalThread == self.thread
                    || matches!(thread.run, Run::Runnable | Run::Start { .. })
            })
            .map(|(i, _)| i as EvalThread)
            .collect()
    }

    /// The call that just returned asked to block on `wait` (`block_on`): finished at once if
    /// the wait is satisfied already, else the running thread blocks. Whether it blocked.
    pub(super) fn block_call(&mut self, wait: Wait, destination: Interval) -> Result<'db, bool> {
        if let Wait::Join { thread, retval } = wait {
            let Some(joined) = self.threads.get(thread as usize) else {
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "joining thread {thread}, which was never made"
                )));
            };
            if joined.joined || joined.detached || thread == self.thread {
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "joining thread {thread}, which is the running one, or joined or detached"
                )));
            }
            self.threads[thread as usize].joined = true;
            if let Run::Ended = self.threads[thread as usize].run {
                self.machine.join(thread)?;
                let result = self.threads[thread as usize].result;
                self.finish_join(retval, result, destination)?;
                return Ok(false);
            }
        }
        if self.nesting > 1 {
            let name = self.running_name();
            not_supported!("blocking in {name}, which the evaluator runs nested");
        }
        let others_run = self.threads.iter().enumerate().any(|(i, thread)| {
            i != self.thread as usize && matches!(thread.run, Run::Runnable | Run::Start { .. })
        });
        if !others_run && let Wait::Futex { timeout: Some((deadline, bytes)), .. } = &wait {
            // Nothing else can run, so the wait times out now.
            self.clock = self.clock.max(*deadline);
            destination.write_from_bytes(self, &bytes[..destination.size.min(bytes.len())])?;
            return Ok(false);
        }
        if self.threads.len() <= 1 {
            return Err(MirEvalError::UndefinedBehavior(
                "deadlock: the only thread blocks".to_owned(),
            ));
        }
        self.wait_counter += 1;
        let since = self.wait_counter;
        let me = self.thread as usize;
        self.threads[me].run = Run::Blocked { wait, destination, since };
        Ok(true)
    }

    fn finish_join(
        &mut self,
        retval: Address,
        result: Option<Address>,
        destination: Interval,
    ) -> Result<'db, ()> {
        if retval.to_usize() != 0 {
            let result = result.unwrap_or(Address::invalid(0));
            self.write_pointer(retval, result)?;
        }
        destination.write_from_bytes(self, &0i32.to_le_bytes()[..destination.size])
    }

    /// Makes the blocked thread `i` runnable: its call finishes as it goes on, with its wait's
    /// result (`timed_out`: the timeout's).
    fn wake_thread(&mut self, i: usize, timed_out: bool) {
        let thread = &mut self.threads[i];
        let Run::Blocked { wait, destination, .. } = mem::replace(&mut thread.run, Run::Runnable)
        else {
            return;
        };
        match wait {
            Wait::Futex { addr, woken, timeout, .. } => {
                let bytes = match (timed_out, timeout) {
                    (true, Some((_, bytes))) => bytes,
                    _ => {
                        thread.resume.acquire = Some(addr as u64);
                        woken
                    }
                };
                thread.resume.result = Some((destination, bytes));
            }
            Wait::Join { thread: joined, retval } => {
                thread.resume.joined = Some(joined);
                if retval.to_usize() != 0 {
                    let result = self.threads[joined as usize].result;
                    let thread = &mut self.threads[i];
                    thread.resume.pointer = Some((retval, result.unwrap_or(Address::invalid(0))));
                }
                self.threads[i].resume.result = Some((destination, 0i32.to_le_bytes().to_vec()));
            }
        }
    }

    /// The running thread's start function (or an exit destructor) returned, with its return
    /// place `returned`: the next exit destructor runs, else the thread ends, wakes its
    /// joiners and another thread runs. Where the thread to run goes on.
    pub(super) fn thread_returned(&mut self, returned: Interval) -> Result<'db, BasicBlockId> {
        let me = self.thread as usize;
        if self.threads[me].result.is_none() {
            let result = self.read_pointer(returned.addr)?;
            self.threads[me].result = Some(result);
        }
        if let Some(block) = self.next_exit_call()? {
            return Ok(block);
        }
        self.machine.exit()?;
        self.threads[me].run = Run::Ended;
        let joiners: Vec<usize> = (0..self.threads.len())
            .filter(|&i| {
                matches!(
                    self.threads[i].run,
                    Run::Blocked { wait: Wait::Join { thread, .. }, .. } if thread as usize == me
                )
            })
            .collect();
        for i in joiners {
            self.wake_thread(i, false);
        }
        let block = self.threads[me].origin.0.start_block;
        self.switch_thread(None, block)
    }

    /// The next exit call of the running thread, made a frame: an `at_thread_exit`
    /// destructor, else a pthread key's destructor of a key whose value is not null (which
    /// is set to null first, as pthread does). Where it starts, if there is one.
    fn next_exit_call(&mut self) -> Result<'db, Option<BasicBlockId>> {
        let next = match self.at_exit.pop() {
            Some(it) => Some(it),
            None => self.next_key_destructor()?,
        };
        let Some((func, arg)) = next else { return Ok(None) };
        let me = self.thread as usize;
        let (body, span) = self.threads[me].origin;
        let slot = self.threads[me].result_slot;
        let frame = self.call_fn_pointer_word(func, &[arg], slot, body, span, Then::Thread)?;
        Ok(frame.map(|frame| {
            let block = frame.locals.body.start_block;
            self.code_stack.push(frame);
            block
        }))
    }

    fn next_key_destructor(&mut self) -> Result<'db, Option<(usize, Address)>> {
        for key in 0..self.tls_keys.len() {
            let Some(Some(dtor)) = self.tls_keys[key] else { continue };
            let value = self.thread_local_storage.keys.get(key).copied().unwrap_or(0);
            if value == 0 {
                continue;
            }
            self.thread_local_storage.keys[key] = 0;
            return Ok(Some((dtor, Address::from_usize(value as usize))));
        }
        Ok(None)
    }

    /// Switches to another thread: `next` if the machine picked it, else (the running thread
    /// blocked or ended) the one the machine picks of those that can run. The running thread,
    /// unless it ended, goes on later at `block` with its frames on [`Self::code_stack`].
    /// Where the thread switched to goes on.
    pub(super) fn switch_thread(
        &mut self,
        next: Option<EvalThread>,
        block: BasicBlockId,
    ) -> Result<'db, BasicBlockId> {
        let me = self.thread as usize;
        let next = match next {
            Some(it) => it,
            None => self.pick_after_block()?,
        };
        self.park(me, block);
        self.unpark(next as usize)
    }

    /// Of the threads that can run, the one the machine picks; when none can, the timed wait
    /// that ends first times out, and with none of those either the program deadlocked.
    fn pick_after_block(&mut self) -> Result<'db, EvalThread> {
        let runnable: Vec<EvalThread> = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, thread)| matches!(thread.run, Run::Runnable | Run::Start { .. }))
            .map(|(i, _)| i as EvalThread)
            .collect();
        if !runnable.is_empty() {
            let running = self.thread;
            return Ok(self.machine.schedule(running, &runnable)?);
        }
        let timed = self
            .threads
            .iter()
            .enumerate()
            .filter_map(|(i, thread)| match &thread.run {
                Run::Blocked { wait: Wait::Futex { timeout: Some((deadline, _)), .. }, since, .. } => {
                    Some((*deadline, *since, i))
                }
                _ => None,
            })
            .min();
        let Some((deadline, _, i)) = timed else {
            return Err(MirEvalError::UndefinedBehavior(
                "deadlock: every thread is blocked".to_owned(),
            ));
        };
        self.clock = self.clock.max(deadline);
        self.wake_thread(i, true);
        Ok(i as EvalThread)
    }

    /// Moves the running thread's state into its entry (all of it at once, so the evaluator's
    /// own fields are then the next thread's to fill).
    fn park(&mut self, me: usize, block: BasicBlockId) {
        let thread = &mut self.threads[me];
        thread.frames = mem::take(&mut self.code_stack);
        thread.block = block;
        thread.stack = mem::take(&mut self.stack);
        thread.stack_allocations = mem::take(&mut self.stack_allocations);
        thread.tls = mem::take(&mut self.thread_local_storage);
        thread.statics = mem::take(&mut self.thread_statics);
        thread.unwinding = self.unwinding.take();
        thread.running = self.running.take();
        thread.at_exit = mem::take(&mut self.at_exit);
    }

    /// Makes thread `i` the running one, starting it if it is new. Where it goes on.
    fn unpark(&mut self, i: usize) -> Result<'db, BasicBlockId> {
        self.thread = i as EvalThread;
        let thread = &mut self.threads[i];
        self.code_stack = mem::take(&mut thread.frames);
        self.stack = mem::take(&mut thread.stack);
        self.stack_allocations = mem::take(&mut thread.stack_allocations);
        self.thread_local_storage = mem::take(&mut thread.tls);
        self.thread_statics = mem::take(&mut thread.statics);
        self.unwinding = thread.unwinding.take();
        self.running = thread.running.take();
        self.at_exit = mem::take(&mut thread.at_exit);
        let block = thread.block;
        let resume = mem::take(&mut thread.resume);
        if let Run::Start { start, arg } = mem::replace(&mut thread.run, Run::Runnable) {
            let (body, span) = thread.origin;
            let slot = thread.result_slot;
            self.stack.push(0);
            let Some(frame) = self.call_fn_pointer_word(start, &[arg], slot, body, span, Then::Thread)?
            else {
                not_supported!("a thread whose start function is a shim");
            };
            let block = frame.locals.body.start_block;
            self.code_stack.push(frame);
            return Ok(block);
        }
        if let Some(addr) = resume.acquire {
            self.machine.acquire(addr)?;
        }
        if let Some(joined) = resume.joined {
            self.machine.join(joined)?;
        }
        if let Some((at, to)) = resume.pointer {
            self.write_pointer(at, to)?;
        }
        if let Some((destination, bytes)) = resume.result {
            destination.write_from_bytes(self, &bytes[..destination.size.min(bytes.len())])?;
        }
        Ok(block)
    }

    /// What the running frame is, for errors.
    fn running_name(&self) -> String {
        match self.running.and_then(|(callee, _)| callee) {
            Some(func) => {
                hir_def::signatures::FunctionSignature::of(self.db, func).name.as_str().to_owned()
            }
            None => "a closure or shim".to_owned(),
        }
    }
}
