//! The fence protocol checked exhaustively: every interleaving of the daemon retiring one range
//! and a few threads calling through its slot, quiescing, claiming slots, being born and dying,
//! at the grain of the single shared-memory accesses `fence.rs` and `runtime.rs` make.
//!
//! Sequentially consistent: every access to the epoch, `births` and the slots is `SeqCst`; the
//! entry's seqlock is one access here (a reader that sees it change skips it, which is what a
//! read of the grace before the entry was posted does), and the unlinking store is ordered
//! before the epoch's store by the fence in [`super::Fences::start`]. Two loads a function makes
//! with nothing between them that a thread could see (`holds`, the adopt test) are one step:
//! each outcome they give is the outcome of reading both at one of the two points.
//!
//! Checked over the whole graph of reachable states:
//! - safety: when the daemon frees the range, no thread is in it nor has loaded the old slot
//!   word and not yet made the call;
//! - liveness: from every reachable state, a state with the range freed is reachable (a fence
//!   passes once the threads run on);
//! - the check has teeth: the daemon frees while a thread is alive in some run, and each
//!   mutation of the protocol below is caught.

use std::collections::HashMap;
use std::hash::BuildHasherDefault;

const NONE: u8 = u8::MAX;
const MOST_THREADS: usize = 3;
const MOST_SLOTS: usize = 3;
/// The grace the range is posted under: the epoch starts at 1.
const GRACE: u8 = 1;

/// A change to the protocol the model must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mutation {
    None,
    /// The daemon trusts slots without counting them against the threads listed.
    NoCount,
    /// The epoch moves before the unlinking store.
    EarlyStart,
    /// A quiescent point reports the epoch whatever its stack holds.
    NoStack,
}

#[derive(Clone, Copy, Debug)]
struct Config {
    /// Threads alive from the start (records `0..threads`).
    threads: usize,
    /// Thread 0 may start one more thread (the last record).
    spawn: bool,
    /// The started thread gets the ID of thread 1, which must be dead first.
    reuse: bool,
    /// A thread may die without giving its slot back.
    abrupt: bool,
    slots: usize,
    /// Calls through the slot each thread makes.
    calls: u8,
    /// Quiescent points each thread reaches.
    quiesces: u8,
    mutation: Mutation,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum SlotState {
    Free,
    Claiming,
    Online,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Slot {
    state: SlotState,
    tid: u8,
    seen: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Pc {
    Unborn,
    /// Between calls: may call, quiesce, start a thread or end.
    Top,
    /// Loaded the slot word, the call not made yet: no quiescent point is here.
    Called,
    /// In the function it called: may quiesce or return.
    Inside,
    QEpoch,
    QHolds,
    QScan,
    QCheck,
    QReport,
    QBegin,
    QAdopt(u8),
    QAdoptSeen(u8),
    QAdoptOnline(u8),
    QFree(u8),
    QTid(u8),
    QSeen(u8),
    QOnline(u8),
    /// Its thread-local's destructor: the slot given back.
    XRelease,
    /// Gone from the OS's list.
    XGone,
    Dead,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
struct Thread {
    pc: Pc,
    tid: u8,
    /// Where a quiescent point returns to: `Inside` or `Top`.
    inside: bool,
    /// Loaded the old slot word.
    old: bool,
    /// A frame of its stack is in the range.
    in_range: bool,
    epoch: u8,
    seen: u8,
    slot: u8,
    reported: u8,
    calls: u8,
    quiesces: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Part {
    State,
    Tid,
    Seen,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Dpc {
    Unlink,
    Post,
    Start,
    Births,
    ListBegin,
    ListEnd,
    Slot(u8, Part),
    Check,
    Count,
    Reap(u8),
    Decide,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Daemon {
    pc: Dpc,
    births: u8,
    /// The thread IDs listed, as a bitset.
    list: u8,
    counted: u8,
    oldest: u8,
    /// Slots of threads not listed, as a bitset.
    dead: u8,
    /// Whether the unlinking store was made (for `EarlyStart`, which moves it last).
    unlinked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct State {
    /// The slot word points at the new code.
    new: bool,
    epoch: u8,
    /// The table entry's grace: 0 until posted.
    grace: u8,
    births: u8,
    slots: [Slot; MOST_SLOTS],
    /// Live thread IDs, as the OS has them.
    alive: u8,
    /// While the daemon lists threads: IDs alive at some point of it, and IDs alive all of it.
    listing: bool,
    during: u8,
    throughout: u8,
    threads: [Thread; MOST_THREADS],
    daemon: Daemon,
}

fn bit(tid: u8) -> u8 {
    1 << tid
}

impl State {
    fn initial(config: &Config) -> State {
        let thread = |index: usize| Thread {
            pc: if index < config.threads { Pc::Top } else { Pc::Unborn },
            tid: if index < config.threads { index as u8 + 1 } else { 0 },
            inside: false,
            old: false,
            in_range: false,
            epoch: 0,
            seen: 0,
            slot: NONE,
            reported: 0,
            calls: config.calls,
            quiesces: config.quiesces,
        };
        let threads = [thread(0), thread(1), thread(2)];
        let alive = threads.iter().filter(|it| it.pc == Pc::Top).fold(0, |set, it| set | bit(it.tid));
        State {
            new: false,
            epoch: 1,
            grace: 0,
            births: 0,
            slots: [Slot { state: SlotState::Free, tid: 0, seen: 0 }; MOST_SLOTS],
            alive,
            listing: false,
            during: 0,
            throughout: 0,
            threads,
            daemon: Daemon {
                pc: if config.mutation == Mutation::EarlyStart { Dpc::Post } else { Dpc::Unlink },
                births: 0,
                list: 0,
                counted: 0,
                oldest: NONE,
                dead: 0,
                unlinked: false,
            },
        }
    }

    fn holds(&self, index: usize) -> bool {
        let thread = &self.threads[index];
        thread.slot != NONE && {
            let slot = &self.slots[thread.slot as usize];
            slot.state == SlotState::Online && slot.tid == thread.tid
        }
    }

    /// Whether a thread is in the range or about to be.
    fn exposed(&self) -> Option<usize> {
        self.threads.iter().position(|it| it.old || it.in_range)
    }
}

fn at(state: &mut State, index: usize) -> &mut Thread {
    &mut state.threads[index]
}

/// One step of thread `index`, every way it can go.
fn thread_steps(state: &State, index: usize, config: &Config, out: &mut Vec<State>) {
    let thread = state.threads[index];
    let mut next = *state;
    let finish = |next: &mut State| {
        let thread = &mut next.threads[index];
        thread.reported = if thread.seen == thread.epoch { thread.epoch } else { 0 };
        thread.pc = if thread.inside { Pc::Inside } else { Pc::Top };
    };
    match thread.pc {
        Pc::Unborn | Pc::Dead => {}
        Pc::Top => {
            if thread.calls > 0 {
                let mut call = *state;
                let it = at(&mut call, index);
                it.calls -= 1;
                it.old = !state.new;
                it.pc = Pc::Called;
                out.push(call);
            }
            if thread.quiesces > 0 {
                let mut quiesce = *state;
                let it = at(&mut quiesce, index);
                it.quiesces -= 1;
                it.inside = false;
                it.pc = Pc::QEpoch;
                out.push(quiesce);
            }
            let mut end = *state;
            at(&mut end, index).pc = Pc::XRelease;
            out.push(end);
            if config.abrupt {
                let mut die = *state;
                at(&mut die, index).pc = Pc::XGone;
                out.push(die);
            }
            let last = MOST_THREADS - 1;
            if index == 0
                && config.spawn
                && state.threads[last].pc == Pc::Unborn
                && (!config.reuse || state.threads[1].pc == Pc::Dead)
            {
                let mut spawn = *state;
                let tid = if config.reuse { state.threads[1].tid } else { last as u8 + 1 };
                let child = &mut spawn.threads[last];
                child.pc = Pc::Top;
                child.tid = tid;
                spawn.alive |= bit(tid);
                if spawn.listing {
                    spawn.during |= bit(tid);
                }
                out.push(spawn);
            }
        }
        Pc::Called => {
            let it = at(&mut next, index);
            it.in_range = it.old;
            it.old = false;
            it.pc = Pc::Inside;
            out.push(next);
        }
        Pc::Inside => {
            let mut back = *state;
            let it = at(&mut back, index);
            it.in_range = false;
            it.pc = Pc::Top;
            out.push(back);
            if thread.quiesces > 0 {
                let it = at(&mut next, index);
                it.quiesces -= 1;
                it.inside = true;
                it.pc = Pc::QEpoch;
                out.push(next);
            }
        }
        Pc::QEpoch => {
            let epoch = state.epoch;
            let it = at(&mut next, index);
            it.epoch = epoch;
            it.pc = if it.slot != NONE && it.reported == epoch { Pc::QHolds } else { Pc::QScan };
            out.push(next);
        }
        Pc::QHolds => {
            if state.holds(index) {
                let it = at(&mut next, index);
                it.pc = if it.inside { Pc::Inside } else { Pc::Top };
            } else {
                let it = at(&mut next, index);
                it.slot = NONE;
                it.pc = Pc::QScan;
            }
            out.push(next);
        }
        Pc::QScan => {
            let grace = state.grace;
            let it = at(&mut next, index);
            let frames = it.in_range && config.mutation != Mutation::NoStack;
            it.seen = if grace != 0 && grace < it.epoch && frames { grace } else { it.epoch };
            it.pc = Pc::QCheck;
            out.push(next);
        }
        Pc::QCheck => {
            let holds = state.holds(index);
            let it = at(&mut next, index);
            if holds {
                it.pc = Pc::QReport;
            } else {
                it.slot = NONE;
                it.pc = Pc::QBegin;
            }
            out.push(next);
        }
        Pc::QReport => {
            next.slots[thread.slot as usize].seen = thread.seen;
            finish(&mut next);
            out.push(next);
        }
        Pc::QBegin => {
            next.births += 1;
            at(&mut next, index).pc = Pc::QAdopt(0);
            out.push(next);
        }
        Pc::QAdopt(k) => {
            let k = k as usize;
            if k == config.slots {
                at(&mut next, index).pc = Pc::QFree(0);
            } else if state.slots[k].state == SlotState::Online && state.slots[k].tid == thread.tid {
                next.slots[k].state = SlotState::Claiming;
                at(&mut next, index).pc = Pc::QAdoptSeen(k as u8);
            } else {
                at(&mut next, index).pc = Pc::QAdopt(k as u8 + 1);
            }
            out.push(next);
        }
        Pc::QAdoptSeen(k) => {
            next.slots[k as usize].seen = thread.seen;
            at(&mut next, index).pc = Pc::QAdoptOnline(k);
            out.push(next);
        }
        Pc::QAdoptOnline(k) | Pc::QOnline(k) => {
            next.slots[k as usize].state = SlotState::Online;
            at(&mut next, index).slot = k;
            finish(&mut next);
            out.push(next);
        }
        Pc::QFree(k) => {
            let k = k as usize;
            if k == config.slots {
                at(&mut next, index).slot = NONE;
                finish(&mut next);
            } else if state.slots[k].state == SlotState::Free {
                next.slots[k].state = SlotState::Claiming;
                at(&mut next, index).pc = Pc::QTid(k as u8);
            } else {
                at(&mut next, index).pc = Pc::QFree(k as u8 + 1);
            }
            out.push(next);
        }
        Pc::QTid(k) => {
            next.slots[k as usize].tid = thread.tid;
            at(&mut next, index).pc = Pc::QSeen(k);
            out.push(next);
        }
        Pc::QSeen(k) => {
            next.slots[k as usize].seen = thread.seen;
            at(&mut next, index).pc = Pc::QOnline(k);
            out.push(next);
        }
        Pc::XRelease => {
            if thread.slot != NONE {
                next.slots[thread.slot as usize].state = SlotState::Free;
            }
            at(&mut next, index).pc = Pc::XGone;
            out.push(next);
        }
        Pc::XGone => {
            next.alive &= !bit(thread.tid);
            next.throughout &= !bit(thread.tid);
            at(&mut next, index).pc = Pc::Dead;
            out.push(next);
        }
    }
}

fn restart(daemon: &mut Daemon) {
    daemon.pc = Dpc::Births;
    daemon.list = 0;
    daemon.counted = 0;
    daemon.oldest = NONE;
    daemon.dead = 0;
}

/// One step of the daemon, every way it can go; `Err` names the thread exposed when it frees.
fn daemon_steps(state: &State, config: &Config, out: &mut Vec<State>) -> Result<(), usize> {
    let mut next = *state;
    match state.daemon.pc {
        Dpc::Unlink => {
            next.new = true;
            next.daemon.unlinked = true;
            next.daemon.pc = if config.mutation == Mutation::EarlyStart { Dpc::Births } else { Dpc::Post };
        }
        Dpc::Post => {
            next.grace = state.epoch;
            next.daemon.pc = Dpc::Start;
        }
        Dpc::Start => {
            next.epoch = state.epoch + 1;
            next.daemon.pc = if config.mutation == Mutation::EarlyStart { Dpc::Unlink } else { Dpc::Births };
        }
        Dpc::Births => {
            next.daemon.births = state.births;
            next.daemon.pc = Dpc::ListBegin;
        }
        Dpc::ListBegin => {
            next.listing = true;
            next.during = state.alive;
            next.throughout = state.alive;
            next.daemon.pc = Dpc::ListEnd;
        }
        Dpc::ListEnd => {
            let (must, may) = (state.throughout, state.during);
            let extra = may & !must;
            // Every list the OS could give: all IDs alive throughout, any of the others.
            let mut subset = extra;
            loop {
                let mut listed = *state;
                listed.listing = false;
                listed.during = 0;
                listed.throughout = 0;
                listed.daemon.list = must | subset;
                listed.daemon.pc = Dpc::Slot(0, Part::State);
                out.push(listed);
                if subset == 0 {
                    break;
                }
                subset = (subset - 1) & extra;
            }
            return Ok(());
        }
        Dpc::Slot(k, part) => {
            let index = k as usize;
            if index == config.slots {
                next.daemon.pc = Dpc::Check;
            } else {
                let slot = state.slots[index];
                match part {
                    Part::State => {
                        next.daemon.pc = if slot.state == SlotState::Online {
                            Dpc::Slot(k, Part::Tid)
                        } else {
                            Dpc::Slot(k + 1, Part::State)
                        };
                    }
                    Part::Tid => {
                        if next.daemon.list & bit(slot.tid) == 0 {
                            next.daemon.dead |= 1 << k;
                            next.daemon.pc = Dpc::Slot(k + 1, Part::State);
                        } else {
                            next.daemon.pc = Dpc::Slot(k, Part::Seen);
                        }
                    }
                    Part::Seen => {
                        next.daemon.counted += 1;
                        next.daemon.oldest = next.daemon.oldest.min(slot.seen);
                        next.daemon.pc = Dpc::Slot(k + 1, Part::State);
                    }
                }
            }
        }
        Dpc::Check => {
            if state.births != state.daemon.births {
                restart(&mut next.daemon);
            } else {
                next.daemon.pc = Dpc::Count;
            }
        }
        Dpc::Count => {
            let counted_all = u32::from(next.daemon.counted) == next.daemon.list.count_ones();
            if counted_all || config.mutation == Mutation::NoCount {
                next.daemon.pc = Dpc::Reap(0);
            } else {
                restart(&mut next.daemon);
            }
        }
        Dpc::Reap(k) => {
            let index = k as usize;
            if index == config.slots {
                next.daemon.pc = Dpc::Decide;
            } else {
                if state.daemon.dead & (1 << k) != 0 && state.slots[index].state == SlotState::Online {
                    next.slots[index].state = SlotState::Free;
                }
                next.daemon.pc = Dpc::Reap(k + 1);
            }
        }
        Dpc::Decide => {
            let ready = state.daemon.unlinked && state.grace != 0 && state.epoch > state.grace;
            if ready && state.daemon.oldest > GRACE {
                if let Some(thread) = state.exposed() {
                    return Err(thread);
                }
                next.daemon.pc = Dpc::Done;
            } else {
                restart(&mut next.daemon);
            }
        }
        Dpc::Done => return Ok(()),
    }
    out.push(next);
    Ok(())
}

/// The representative of `state`, exploring one state per class checks both properties:
/// - A dead thread's record is its ID alone: nothing steps it, a spawn reads only its ID, and
///   it died from `Top`, where it neither holds the old word nor is in the range.
/// - Renaming the threads, when every thread plays the same part (no spawn: thread `i` is ID
///   `i + 1` throughout). A thread ID only meets another in equality tests and as a bit of a
///   set, so renaming the records and every ID with them (the slots' IDs, the OS's list, the
///   daemon's list) gives a state that steps the same way, freeing exposes a thread in one
///   exactly when it does in the other, and the range is freed from one exactly when from the
///   other. Sorted by everything of a thread but its ID; ties only cost reduction.
fn canonical(mut state: State, config: &Config) -> State {
    for thread in &mut state.threads {
        if thread.pc == Pc::Dead {
            debug_assert!(!thread.old && !thread.in_range, "a thread died in the range");
            *thread = Thread {
                pc: Pc::Dead,
                tid: thread.tid,
                inside: false,
                old: false,
                in_range: false,
                epoch: 0,
                seen: 0,
                slot: NONE,
                reported: 0,
                calls: 0,
                quiesces: 0,
            };
        }
    }
    if config.spawn {
        return state;
    }
    let live = config.threads;
    let mut order = [0usize, 1, 2];
    order[..live].sort_by_key(|&it| Thread { tid: 0, ..state.threads[it] });
    if order[..live].iter().enumerate().all(|(at, &it)| at == it) {
        return state;
    }
    let mut rename = [0u8; MOST_THREADS + 1];
    for (to, &from) in order[..live].iter().enumerate() {
        rename[from + 1] = to as u8 + 1;
    }
    let set = |bits: u8| (1..=live).filter(|&tid| bits & bit(tid as u8) != 0).fold(0, |out, tid| out | bit(rename[tid]));
    let mut out = state;
    for (to, &from) in order[..live].iter().enumerate() {
        out.threads[to] = Thread { tid: to as u8 + 1, ..state.threads[from] };
    }
    for slot in &mut out.slots {
        slot.tid = rename[slot.tid as usize];
    }
    out.alive = set(state.alive);
    out.during = set(state.during);
    out.throughout = set(state.throughout);
    out.daemon.list = set(state.daemon.list);
    out
}

/// rustc's FxHash, for the explorer's table: a state is about a hundred one-byte writes,
/// which SipHash makes the explorer's whole cost.
#[derive(Default)]
struct Fx(u64);

impl Fx {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.add(u64::from(byte));
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    fn write_isize(&mut self, value: isize) {
        self.add(value as u64);
    }
}

struct Explored {
    states: usize,
    /// Done states with a thread still alive.
    freed_while_running: usize,
}

/// The most states one exploration holds (a few GiB with its edges): past it the config is too
/// big for a test, an error rather than a run of hours.
const MOST_STATES: usize = 1 << 24;

/// Explores every reachable state; `Err` is the trace to a violation.
fn explore(config: Config) -> Result<Explored, String> {
    let initial = canonical(State::initial(&config), &config);
    let mut index: HashMap<State, u32, BuildHasherDefault<Fx>> = HashMap::default();
    let mut states: Vec<State> = vec![initial];
    let mut parent: Vec<u32> = vec![u32::MAX];
    let mut edges: Vec<(u32, u32)> = Vec::new();
    index.insert(initial, 0);
    let mut stack = vec![0u32];
    let mut next = Vec::new();
    let trace = |states: &[State], parent: &[u32], mut at: u32| {
        let mut steps = Vec::new();
        while at != u32::MAX {
            steps.push(format!("{:?}", states[at as usize]));
            at = parent[at as usize];
        }
        steps.reverse();
        steps.join("\n")
    };
    while let Some(at) = stack.pop() {
        let state = states[at as usize];
        // Past the free nothing is checked: the daemon steps no more, and liveness asks only
        // that a freed state be reached.
        if state.daemon.pc == Dpc::Done {
            continue;
        }
        next.clear();
        for thread in 0..MOST_THREADS {
            thread_steps(&state, thread, &config, &mut next);
        }
        if let Err(thread) = daemon_steps(&state, &config, &mut next) {
            return Err(format!(
                "{config:?}: the range was freed while thread {thread} was in it or about to be:\n{}",
                trace(&states, &parent, at)
            ));
        }
        for successor in next.drain(..) {
            let successor = canonical(successor, &config);
            let to = *index.entry(successor).or_insert_with(|| {
                states.push(successor);
                parent.push(at);
                stack.push((states.len() - 1) as u32);
                (states.len() - 1) as u32
            });
            edges.push((at, to));
        }
        if states.len() > MOST_STATES {
            return Err(format!("{config:?}: past {MOST_STATES} states, too many to explore in a test"));
        }
    }

    // Liveness: every state reaches one where the range is freed (backwards from those).
    let count = states.len();
    let sources = blaze_radix::Csr::build(count, edges.iter().map(|&(source, to)| (to, source)));
    let mut reaches = vec![false; count];
    let mut work: Vec<u32> = Vec::new();
    for (at, state) in states.iter().enumerate() {
        if state.daemon.pc == Dpc::Done {
            reaches[at] = true;
            work.push(at as u32);
        }
    }
    while let Some(at) = work.pop() {
        for &source in sources.row(at as usize) {
            if !reaches[source as usize] {
                reaches[source as usize] = true;
                work.push(source);
            }
        }
    }
    if let Some(stuck) = reaches.iter().position(|it| !it) {
        return Err(format!(
            "{config:?}: a state from which the range is never freed:\n{}",
            trace(&states, &parent, stuck as u32)
        ));
    }
    let freed_while_running = states
        .iter()
        .filter(|it| it.daemon.pc == Dpc::Done && it.threads.iter().any(|t| !matches!(t.pc, Pc::Unborn | Pc::Dead)))
        .count();
    Ok(Explored { states: count, freed_while_running })
}

const BASE: Config = Config {
    threads: 2,
    spawn: false,
    reuse: false,
    abrupt: false,
    slots: 2,
    calls: 2,
    quiesces: 2,
    mutation: Mutation::None,
};

fn check(config: Config) {
    let explored = explore(config).unwrap_or_else(|it| panic!("{it}"));
    assert!(explored.states > 1000, "{config:?}: only {} states", explored.states);
    assert!(explored.freed_while_running > 0, "{config:?}: the range is never freed while a thread runs");
}

#[test]
fn two_threads() {
    check(BASE);
}

#[test]
fn a_thread_starts_while_the_daemon_looks() {
    check(Config { threads: 1, spawn: true, slots: 3, calls: 1, quiesces: 1, ..BASE });
}

#[test]
fn more_threads_than_slots() {
    check(Config { slots: 1, ..BASE });
}

#[test]
fn a_thread_dies_holding_its_slot_and_its_id_comes_back() {
    check(Config { spawn: true, reuse: true, abrupt: true, slots: 2, calls: 1, quiesces: 1, ..BASE });
}

#[test]
fn three_threads_two_slots() {
    check(Config { threads: 3, slots: 2, calls: 1, quiesces: 1, ..BASE });
}

fn caught(mutation: Mutation) {
    let config = Config { mutation, ..BASE };
    match explore(config) {
        Ok(_) => panic!("{mutation:?} was not caught"),
        Err(error) => assert!(!error.contains("too many to explore"), "{error}"),
    }
}

#[test]
fn trusting_slots_uncounted_is_caught() {
    caught(Mutation::NoCount);
}

#[test]
fn moving_the_epoch_before_the_unlink_is_caught() {
    caught(Mutation::EarlyStart);
}

#[test]
fn ignoring_the_stack_is_caught() {
    caught(Mutation::NoStack);
}
