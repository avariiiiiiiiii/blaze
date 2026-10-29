//! A deterministic [`Driver`]: one thread, a virtual clock and a seeded scheduler, so a state
//! machine's run under it is a function of the seed. It delays completions, delivers the ones
//! that are ready in a shuffled order, fails effects at a chosen rate, and [`Sim::crash`]
//! loses what was not synced, keeping a random prefix of it (a torn append), as a power cut
//! does. A failing seed replays exactly.
//!
//! Processes are the test's: [`Sim::program`] says what a program does when it is spawned, how
//! long it takes and what it ends with; a memory limit it passes kills it, as a cgroup does.
//! Sockets are in memory: a connect to an address listened on in the simulation makes both
//! ends, and [`Sim::dial`] is a peer from outside (an agent's client, a webhook) that the test
//! plays with [`Sim::peer_send`], [`Sim::peer_received`] and [`Sim::peer_hangup`].

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use crate::{
    Address, Command, Completion, Conn, Done, Driver, Effect, Exit, FileName, IoError, Meta, Tag,
};

/// What a simulated program does when spawned: how it ends, and how long it runs.
#[derive(Clone, Debug)]
pub struct Ran {
    pub exit: Exit,
    pub takes: Duration,
}

/// A simulated program: given the command it was spawned with and the simulation's random
/// numbers, what its run is.
pub type Program = Box<dyn FnMut(&Command, &mut Rng) -> Ran>;

/// The signal a kill or a passed memory limit ends a process with.
pub const KILLED: i32 = 9;

/// One end of a connection.
#[derive(Debug, Default)]
struct End {
    /// The other end, or `None` for a peer outside the simulation the test plays.
    peer: Option<Conn>,
    /// Sent to this end, not yet received.
    inbox: Vec<u8>,
    /// Sent by this end to a peer outside, not yet taken by the test.
    outbox: Vec<u8>,
    /// This end hung up.
    closed: bool,
    /// The other end hung up.
    peer_closed: bool,
}

/// SplitMix64: small, fast, and the same everywhere.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True once in `n` on average; never when `n` is 0.
    pub fn one_in(&mut self, n: u32) -> bool {
        n != 0 && self.below(u64::from(n)) == 0
    }

    /// The items of `items` in an order of the seed's.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for at in (1..items.len()).rev() {
            let other = self.below(at as u64 + 1) as usize;
            items.swap(at, other);
        }
    }
}

/// How unkind the simulated machine is.
#[derive(Clone, Debug)]
pub struct Faults {
    /// An effect fails once in this many (0: never). A failed append writes a random part of
    /// its bytes, as a short write does; a failed sync makes nothing durable; a failed write
    /// leaves the file as it was.
    pub fail_one_in: u32,
    /// Each completion is ready after a delay drawn up to this.
    pub max_latency: Duration,
    /// The file system cannot share blocks: every clone fails, as on ext4.
    pub no_clone: bool,
    /// A watch's change is reported as lost (the queue overflowed) once in this many.
    pub lose_change_one_in: u32,
}

impl Default for Faults {
    fn default() -> Faults {
        Faults {
            fail_one_in: 0,
            max_latency: Duration::from_micros(200),
            no_clone: false,
            lose_change_one_in: 0,
        }
    }
}

#[derive(Default, Clone, Debug)]
struct SimFile {
    /// What survives a crash.
    durable: Vec<u8>,
    /// Appended since the last sync that covered it; read as part of the file, lost (in part)
    /// by a crash.
    volatile: Vec<u8>,
}

#[derive(Debug)]
struct Pending {
    ready: Duration,
    effect: Effect,
    /// For a sync: how many volatile bytes the file had when it was submitted.
    covers: usize,
}

pub struct Sim {
    rng: Rng,
    faults: Faults,
    now: Duration,
    files: BTreeMap<FileName, SimFile>,
    pending: Vec<Pending>,
    watches: BTreeMap<Tag, FileName>,
    /// Changes and ends of watches, accepts and ends of listens, and what sockets did, each
    /// ready at its time.
    told: Vec<(Duration, Completion)>,
    programs: BTreeMap<PathBuf, Program>,
    /// The run of each spawn in flight, decided when it was spawned.
    runs: BTreeMap<Tag, Exit>,
    listens: BTreeMap<Tag, Address>,
    ends: BTreeMap<Conn, End>,
    next_conn: u64,
    /// Receives waiting for bytes or a hang-up: tag, connection, at most.
    receiving: Vec<(Tag, Conn, usize)>,
}

impl Sim {
    pub fn new(seed: u64, faults: Faults) -> Sim {
        Sim {
            rng: Rng::new(seed),
            faults,
            now: Duration::ZERO,
            files: BTreeMap::new(),
            pending: Vec::new(),
            watches: BTreeMap::new(),
            told: Vec::new(),
            programs: BTreeMap::new(),
            runs: BTreeMap::new(),
            listens: BTreeMap::new(),
            ends: BTreeMap::new(),
            next_conn: 0,
            receiving: Vec::new(),
        }
    }

    /// What a spawn of `path` does. A spawn of a program not given fails, as one not on disk.
    pub fn program(
        &mut self,
        path: impl Into<PathBuf>,
        program: impl FnMut(&Command, &mut Rng) -> Ran + 'static,
    ) {
        self.programs.insert(path.into(), Box::new(program));
    }

    /// A peer outside the simulation connects to `at`: the listen there accepts the end this
    /// returns, which the test plays the other side of.
    pub fn dial(&mut self, at: &Address) -> Result<Conn, IoError> {
        let Some(listen) = self.listening_at(at) else {
            return Err(IoError(format!("{at:?}: nothing listens there: connection refused")));
        };
        let conn = self.new_end(None);
        self.tell(listen, Done::Accepted { conn });
        Ok(conn)
    }

    /// The peer outside sends `bytes` to the end `conn` of the simulation's.
    pub fn peer_send(&mut self, conn: Conn, bytes: &[u8]) {
        if let Some(end) = self.ends.get_mut(&conn)
            && end.peer.is_none()
            && !end.peer_closed
        {
            end.inbox.extend_from_slice(bytes);
            self.wake();
        }
    }

    /// What the end `conn` sent to the peer outside since the last time the test asked.
    pub fn peer_received(&mut self, conn: Conn) -> Vec<u8> {
        self.ends.get_mut(&conn).map(|end| std::mem::take(&mut end.outbox)).unwrap_or_default()
    }

    /// Whether the end `conn` has hung up on the peer outside.
    pub fn peer_hung_up_on(&self, conn: Conn) -> bool {
        self.ends.get(&conn).is_none_or(|end| end.closed)
    }

    /// The peer outside hangs up: a receive of `conn` then gets nothing.
    pub fn peer_hangup(&mut self, conn: Conn) {
        if let Some(end) = self.ends.get_mut(&conn)
            && end.peer.is_none()
        {
            end.peer_closed = true;
            self.wake();
        }
    }

    /// The machine loses power: every effect in flight is gone without a completion, every
    /// watch, process, listen and connection with it, and each file keeps what was synced plus
    /// a random prefix of what was appended after. A whole-file write that completed is kept
    /// whole; one in flight is not there.
    pub fn crash(&mut self) {
        self.pending.clear();
        self.watches.clear();
        self.told.clear();
        self.runs.clear();
        self.listens.clear();
        self.ends.clear();
        self.receiving.clear();
        for file in self.files.values_mut() {
            let kept = self.rng.below(file.volatile.len() as u64 + 1) as usize;
            file.durable.extend_from_slice(&file.volatile[..kept]);
            file.volatile.clear();
        }
    }

    /// What a file holds now, synced or not.
    pub fn contents(&self, file: &FileName) -> Vec<u8> {
        self.files
            .get(file)
            .map(|it| [it.durable.as_slice(), &it.volatile].concat())
            .unwrap_or_default()
    }

    /// Whether the file is there.
    pub fn exists(&self, file: &FileName) -> bool {
        self.files.contains_key(file)
    }

    /// The file made to hold `bytes`, durably, by something outside the state machine (a
    /// person's editor): watches of it hear of it.
    pub fn change(&mut self, file: &FileName, bytes: &[u8]) {
        self.files.insert(file.clone(), SimFile { durable: bytes.to_vec(), volatile: Vec::new() });
        self.changed(file);
    }

    /// The file removed by something outside the state machine.
    pub fn delete(&mut self, file: &FileName) {
        self.files.remove(file);
        self.changed(file);
    }

    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    fn delay(&mut self) -> Duration {
        Duration::from_nanos(self.rng.below(self.faults.max_latency.as_nanos() as u64 + 1))
    }

    /// Tells the watches of `file` it changed.
    fn changed(&mut self, file: &FileName) {
        let watching: Vec<Tag> = self
            .watches
            .iter()
            .filter(|(_, dir)| file.is_under(dir))
            .map(|(&tag, _)| tag)
            .collect();
        for tag in watching {
            let done = match self.rng.one_in(self.faults.lose_change_one_in) {
                true => Done::Changed { files: Vec::new(), lost: true },
                false => Done::Changed { files: vec![file.clone()], lost: false },
            };
            let at = self.now + self.delay();
            self.told.push((at, Completion { tag, done }));
        }
    }

    fn fail(&mut self, tag: Tag) -> Completion {
        Completion { tag, done: Done::Failed(IoError("injected by the simulation".into())) }
    }

    /// `done` of `tag`, ready after a delay, and after everything told of the same tag before.
    fn tell(&mut self, tag: Tag, done: Done) {
        let at = self.now + self.delay();
        let at = self
            .told
            .iter()
            .filter(|(_, it)| it.tag == tag)
            .map(|(at, _)| *at)
            .max()
            .map_or(at, |last| last.max(at));
        self.told.push((at, Completion { tag, done }));
    }

    fn listening_at(&self, at: &Address) -> Option<Tag> {
        self.listens.iter().find(|(_, it)| *it == at).map(|(&tag, _)| tag)
    }

    fn new_end(&mut self, peer: Option<Conn>) -> Conn {
        let conn = Conn(self.next_conn);
        self.next_conn += 1;
        self.ends.insert(conn, End { peer, ..End::default() });
        conn
    }

    /// Every waiting receive that can be answered now is: with what came, or with nothing once
    /// either end hung up.
    fn wake(&mut self) {
        let mut at = 0;
        while at < self.receiving.len() {
            let (tag, conn, most) = self.receiving[at];
            let end = self.ends.get_mut(&conn);
            let done = match end {
                None => Some(Done::Read(Vec::new())),
                Some(end) if !end.inbox.is_empty() => {
                    let n = most.min(end.inbox.len());
                    Some(Done::Read(end.inbox.drain(..n).collect()))
                }
                Some(end) if end.closed || end.peer_closed => Some(Done::Read(Vec::new())),
                Some(_) => None,
            };
            match done {
                Some(done) => {
                    self.receiving.remove(at);
                    self.tell(tag, done);
                }
                None => at += 1,
            }
        }
    }

    /// What a spawn of `command` ends with, decided now from its program.
    fn run(&mut self, command: &Command) -> Result<Ran, IoError> {
        let Some(program) = self.programs.get_mut(command.program.as_path()) else {
            return Err(IoError(format!(
                "starting {}: no such program in the simulation",
                command.program.display()
            )));
        };
        let mut ran = program(command, &mut self.rng);
        if let Some(limit) = command.memory
            && ran.exit.peak_memory > limit
        {
            // Killed as it passed its limit: by then it had run a part of its time.
            ran.takes = Duration::from_nanos(self.rng.below(ran.takes.as_nanos() as u64 + 1));
            ran.exit = Exit {
                code: None,
                signal: Some(KILLED),
                stdout: Vec::new(),
                stderr: Vec::new(),
                traced: None,
                peak_memory: limit,
            };
        }
        Ok(ran)
    }

    fn complete(&mut self, pending: Pending) -> Completion {
        let tag = pending.effect.tag();
        if !matches!(pending.effect, Effect::Timer { .. })
            && self.rng.one_in(self.faults.fail_one_in)
        {
            if let Effect::Spawn { .. } = &pending.effect {
                self.runs.remove(&tag);
            }
            if let Effect::Append { file, bytes, .. } = &pending.effect {
                let torn = self.rng.below(bytes.len() as u64) as usize;
                self.files
                    .entry(file.clone())
                    .or_default()
                    .volatile
                    .extend_from_slice(&bytes[..torn]);
                if torn > 0 {
                    self.changed(&file.clone());
                }
            }
            return self.fail(tag);
        }
        let done = match pending.effect {
            Effect::ReadAll { file, .. } => Done::Read(self.contents(&file)),
            Effect::Read { file, at, len, .. } => {
                let all = self.contents(&file);
                let at = (at as usize).min(all.len());
                Done::Read(all[at..(at + len).min(all.len())].to_vec())
            }
            Effect::Append { file, bytes, .. } => {
                self.files.entry(file.clone()).or_default().volatile.extend_from_slice(&bytes);
                self.changed(&file);
                Done::Appended
            }
            Effect::Sync { file, .. } => {
                let file = self.files.entry(file).or_default();
                let covered = pending.covers.min(file.volatile.len());
                file.durable.extend(file.volatile.drain(..covered));
                Done::Synced
            }
            // Simpler than a disk: the cut is not undone by a crash before the next sync.
            Effect::Truncate { file, len, .. } => {
                let sim = self.files.entry(file.clone()).or_default();
                let len = len as usize;
                if len <= sim.durable.len() {
                    sim.durable.truncate(len);
                    sim.volatile.clear();
                } else {
                    sim.volatile.truncate(len - sim.durable.len());
                }
                self.changed(&file);
                Done::Truncated
            }
            Effect::Write { file, bytes, .. } => {
                self.files.insert(file.clone(), SimFile { durable: bytes, volatile: Vec::new() });
                self.changed(&file);
                Done::Written
            }
            Effect::Clone { from, to, .. } | Effect::Link { from, to, .. }
                if !self.files.contains_key(&from) =>
            {
                let _ = to;
                Done::Failed(IoError(format!("{from}: no such file")))
            }
            Effect::Clone { .. } if self.faults.no_clone => Done::Failed(IoError(
                "the simulated file system cannot share blocks; blaze does not copy instead"
                    .into(),
            )),
            Effect::Clone { to, .. } | Effect::Link { to, .. } if self.files.contains_key(&to) => {
                Done::Failed(IoError(format!("{to}: already there")))
            }
            Effect::Clone { from, to, .. } => {
                let bytes = self.contents(&from);
                self.files.insert(to.clone(), SimFile { durable: bytes, volatile: Vec::new() });
                self.changed(&to);
                Done::Cloned
            }
            Effect::Link { from, to, .. } => {
                let bytes = self.contents(&from);
                self.files.insert(to.clone(), SimFile { durable: bytes, volatile: Vec::new() });
                self.changed(&to);
                Done::Linked
            }
            Effect::Stat { file, .. } => Done::Stat(match self.files.get(&file) {
                Some(it) => Some(Meta {
                    len: (it.durable.len() + it.volatile.len()) as u64,
                    dir: false,
                    modified_ns: 0,
                }),
                None if self.files.keys().any(|it| it != &file && it.is_under(&file)) => {
                    Some(Meta { len: 0, dir: true, modified_ns: 0 })
                }
                None => None,
            }),
            Effect::Remove { file, .. } => {
                if self.files.remove(&file).is_some() {
                    self.changed(&file);
                }
                Done::Removed
            }
            Effect::Timer { .. } => Done::Fired,
            Effect::Map { file, .. } => {
                Done::Mapped(crate::Mapping(std::sync::Arc::new(self.contents(&file))))
            }
            Effect::Spawn { .. } => match self.runs.remove(&tag) {
                Some(exit) => Done::Exited(Box::new(exit)),
                None => unreachable!("a spawn in flight has its run"),
            },
            Effect::Watch { .. }
            | Effect::Unwatch { .. }
            | Effect::Kill { .. }
            | Effect::Listen { .. }
            | Effect::Unlisten { .. }
            | Effect::Connect { .. }
            | Effect::Recv { .. }
            | Effect::Send { .. }
            | Effect::Hangup { .. } => {
                unreachable!("watches, kills and sockets are not pending effects")
            }
        };
        Completion { tag, done }
    }
}

impl Driver for Sim {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::Watch { dir, tag } => {
                    self.watches.insert(tag, dir);
                    continue;
                }
                Effect::Unwatch { tag } => {
                    let done = match self.watches.remove(&tag) {
                        Some(_) => Done::Unwatched,
                        None => Done::Failed(IoError(format!("nothing is watched as {tag:?}"))),
                    };
                    let at = self.now + self.delay();
                    // A change told before the end is heard before it.
                    let at = self
                        .told
                        .iter()
                        .filter(|(_, it)| it.tag == tag)
                        .map(|(at, _)| *at)
                        .max()
                        .map_or(at, |last| last.max(at));
                    self.told.push((at, Completion { tag, done }));
                    continue;
                }
                Effect::Kill { tag } => {
                    if let Some(pending) = self.pending.iter_mut().find(|it| {
                        matches!(it.effect, Effect::Spawn { tag: spawn, .. } if spawn == tag)
                    }) && let Some(exit) = self.runs.get_mut(&tag)
                    {
                        let now = self.now;
                        let delay = Duration::from_nanos(
                            self.rng.below(self.faults.max_latency.as_nanos() as u64 + 1),
                        );
                        pending.ready = pending.ready.min(now + delay);
                        exit.code = None;
                        exit.signal = Some(KILLED);
                    }
                    continue;
                }
                Effect::Listen { at, tag } => {
                    match self.listening_at(&at) {
                        Some(_) => self.tell(
                            tag,
                            Done::Failed(IoError(format!("{at:?}: another listen has it"))),
                        ),
                        None if self.rng.one_in(self.faults.fail_one_in) => {
                            let done = self.fail(tag).done;
                            self.tell(tag, done);
                        }
                        None => _ = self.listens.insert(tag, at),
                    }
                    continue;
                }
                Effect::Unlisten { tag } => {
                    let done = match self.listens.remove(&tag) {
                        Some(_) => Done::Unlistened,
                        None => Done::Failed(IoError(format!("nothing is listened on as {tag:?}"))),
                    };
                    self.tell(tag, done);
                    continue;
                }
                Effect::Connect { at, tag } => {
                    let done = match self.listening_at(&at) {
                        _ if self.rng.one_in(self.faults.fail_one_in) => self.fail(tag).done,
                        None => Done::Failed(IoError(format!(
                            "{at:?}: nothing listens there: connection refused"
                        ))),
                        Some(listen) => {
                            let mine = self.new_end(None);
                            let theirs = self.new_end(Some(mine));
                            self.ends.get_mut(&mine).expect("just made").peer = Some(theirs);
                            self.tell(listen, Done::Accepted { conn: theirs });
                            Done::Connected { conn: mine }
                        }
                    };
                    self.tell(tag, done);
                    continue;
                }
                Effect::Recv { conn, most, tag } => {
                    match self.ends.get(&conn) {
                        Some(_) => {
                            self.receiving.push((tag, conn, most));
                            self.wake();
                        }
                        None => self.tell(
                            tag,
                            Done::Failed(IoError(format!("no connection {conn:?}"))),
                        ),
                    }
                    continue;
                }
                Effect::Send { conn, bytes, tag } => {
                    let done = match self.ends.get(&conn).map(|end| (end.closed, end.peer_closed, end.peer)) {
                        None => Done::Failed(IoError(format!("no connection {conn:?}"))),
                        Some((true, _, _) | (_, true, _)) => Done::Failed(IoError(format!(
                            "{conn:?}: the connection is hung up: broken pipe"
                        ))),
                        Some((_, _, None)) => {
                            let end = self.ends.get_mut(&conn).expect("looked up");
                            end.outbox.extend_from_slice(&bytes);
                            Done::Sent
                        }
                        Some((_, _, Some(peer))) => {
                            if let Some(peer) = self.ends.get_mut(&peer) {
                                peer.inbox.extend_from_slice(&bytes);
                            }
                            Done::Sent
                        }
                    };
                    self.tell(tag, done);
                    self.wake();
                    continue;
                }
                Effect::Hangup { conn, tag } => {
                    let peer = self.ends.get_mut(&conn).map(|end| {
                        end.closed = true;
                        end.peer
                    });
                    let done = match peer {
                        Some(peer) => {
                            if let Some(peer) = peer.and_then(|it| self.ends.get_mut(&it)) {
                                peer.peer_closed = true;
                            }
                            Done::HungUp
                        }
                        None => Done::Failed(IoError(format!("no connection {conn:?}"))),
                    };
                    self.wake();
                    self.tell(tag, done);
                    continue;
                }
                Effect::Spawn { ref command, tag } => match self.run(command) {
                    Ok(ran) => {
                        self.runs.insert(tag, ran.exit);
                        let ready = self.now + ran.takes + self.delay();
                        self.pending.push(Pending { ready, effect, covers: 0 });
                        continue;
                    }
                    Err(why) => {
                        self.tell(tag, Done::Failed(why));
                        continue;
                    }
                },
                _ => {}
            }
            let delay = match &effect {
                Effect::Timer { after, .. } => *after,
                _ => self.delay(),
            };
            let covers = match &effect {
                Effect::Sync { file, .. } => self.files.get(file).map_or(0, |it| it.volatile.len()),
                _ => 0,
            };
            self.pending.push(Pending { ready: self.now + delay, effect, covers });
        }
    }

    /// Never waits: time jumps to the first completion when none is ready.
    fn poll(&mut self, out: &mut Vec<Completion>, _wait: Duration) -> usize {
        let first = self.pending.iter().map(|it| it.ready).chain(self.told.iter().map(|it| it.0));
        let Some(first) = first.min() else { return 0 };
        self.now = self.now.max(first);
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].ready <= self.now {
                ready.push(self.pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        // `swap_remove` already disturbed the order; shuffle so it is the seed's, not the vec's.
        self.rng.shuffle(&mut ready);
        let before = out.len();
        for pending in ready {
            let completion = self.complete(pending);
            out.push(completion);
        }
        // What watches were told, in the order they were told: a watch's end comes last.
        let mut index = 0;
        while index < self.told.len() {
            if self.told[index].0 <= self.now {
                out.push(self.told.remove(index).1);
            } else {
                index += 1;
            }
        }
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        let ending = self.told.iter().filter(|(_, it)| {
            !matches!(it.done, Done::Changed { .. } | Done::Accepted { .. })
        });
        self.pending.len()
            + self.watches.len()
            + self.listens.len()
            + self.receiving.len()
            + ending.count()
    }

    fn now(&self) -> Duration {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_until_idle;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    #[test]
    fn a_crash_keeps_what_was_synced_and_a_prefix_of_the_rest() {
        for seed in 0..200 {
            let mut sim = Sim::new(seed, Faults::default());
            let file = name("log");
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: b"synced".to_vec(), tag: Tag(0) }];
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(1) });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Append {
                file: file.clone(),
                bytes: b"-lost".to_vec(),
                tag: Tag(2),
            });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            sim.crash();
            let left = sim.contents(&file);
            assert!(left.starts_with(b"synced") && b"synced-lost".starts_with(&left), "{left:?}");
        }
    }

    #[test]
    fn the_same_seed_is_the_same_run() {
        let run = |seed| {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 4, ..Faults::default() });
            let mut effects: Vec<Effect> = (0..32)
                .map(|n| Effect::Append { file: name("f"), bytes: vec![n as u8], tag: Tag(n) })
                .collect();
            let mut order = Vec::new();
            run_until_idle(&mut sim, &mut effects, |done, _| order.push(done));
            (order, sim.contents(&name("f")))
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    #[test]
    fn a_sync_covers_only_what_was_appended_before_it() {
        let mut sim = Sim::new(
            1,
            Faults { fail_one_in: 0, max_latency: Duration::ZERO, ..Faults::default() },
        );
        let file = name("log");
        let mut effects =
            vec![Effect::Append { file: file.clone(), bytes: b"a".to_vec(), tag: Tag(0) }];
        run_until_idle(&mut sim, &mut effects, |_, _| {});
        sim.submit(&mut vec![Effect::Sync { file: file.clone(), tag: Tag(1) }]);
        // Appended after the sync was asked for, completed before it.
        sim.files.get_mut(&file).unwrap().volatile.extend_from_slice(b"b");
        let mut out = Vec::new();
        sim.poll(&mut out, Duration::ZERO);
        let file = &sim.files[&file];
        assert_eq!((file.durable.as_slice(), file.volatile.as_slice()), (&b"a"[..], &b"b"[..]));
    }

    /// A whole-file write is all or nothing over a crash at any point, on every seed.
    #[test]
    fn a_write_is_whole_or_not_there_after_a_crash() {
        for seed in 0..2_000 {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 5, ..Faults::default() });
            let file = name("store/index");
            sim.change(&file, b"old contents");
            sim.submit(&mut vec![Effect::Write {
                file: file.clone(),
                bytes: b"new".to_vec(),
                tag: Tag(1),
            }]);
            let mut out = Vec::new();
            if sim.rng().one_in(2) {
                sim.poll(&mut out, Duration::ZERO);
            }
            let acknowledged = out.iter().any(|it| it.done == Done::Written);
            sim.crash();
            let left = sim.contents(&file);
            match acknowledged {
                true => assert_eq!(left, b"new", "seed {seed}"),
                false => assert!(left == b"new" || left == b"old contents", "seed {seed}"),
            }
        }
    }

    #[test]
    fn watches_hear_changes_under_their_directory_then_their_end() {
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults { lose_change_one_in: 7, ..Faults::default() });
            let mut effects = vec![Effect::Watch { dir: name("src"), tag: Tag(9) }];
            sim.submit(&mut effects);
            sim.change(&name("src/lib.rs"), b"fn main() {}");
            sim.change(&name("target/out"), b"not watched");
            effects.push(Effect::Write { file: name("src/a/b.rs"), bytes: vec![1], tag: Tag(1) });
            effects.push(Effect::Unwatch { tag: Tag(9) });
            let mut heard = Vec::new();
            let mut lost = false;
            let mut ended = false;
            run_until_idle(&mut sim, &mut effects, |done, _| match done.done {
                Done::Changed { files, lost: was_lost } => {
                    assert!(!ended, "seed {seed}: a change after the end");
                    lost |= was_lost;
                    heard.extend(files);
                }
                Done::Unwatched => ended = true,
                _ => {}
            });
            assert!(ended, "seed {seed}");
            assert!(!heard.contains(&name("target/out")), "seed {seed}");
            if !lost {
                assert!(heard.contains(&name("src/lib.rs")), "seed {seed}: {heard:?}");
            }
        }
    }

    #[test]
    fn a_file_system_that_cannot_share_blocks_refuses_a_clone() {
        let mut sim = Sim::new(3, Faults { no_clone: true, ..Faults::default() });
        sim.change(&name("store/v"), b"code");
        let mut effects = vec![
            Effect::Clone { from: name("store/v"), to: name("target/a"), tag: Tag(1) },
            Effect::Link { from: name("store/v"), to: name("target/b"), tag: Tag(2) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done));
        assert!(matches!(&seen[&Tag(1)], Done::Failed(why) if why.0.contains("does not copy")));
        assert_eq!(seen[&Tag(2)], Done::Linked);
        assert!(!sim.exists(&name("target/a")));
        assert_eq!(sim.contents(&name("target/b")), b"code");
    }
}
