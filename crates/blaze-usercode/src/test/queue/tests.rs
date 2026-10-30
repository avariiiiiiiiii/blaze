use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use blaze_core::{
    Address, Command, Completion, Conn, Done, Driver, Effect, Exit, FileName, IoError, Sandbox, Tag,
    Traced,
    sim::{self, Faults, Rng, Sim},
};
use blaze_ids::ContentId;

use super::*;
use crate::{
    confine,
    test::wire::{self, End, Executed, Frame, Frames, SOCKET, UNRECORDED},
    trace::{Line, Seen},
};

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn config(jobs: usize) -> Config {
    Config {
        root: PathBuf::from("/root"),
        dir: name("req/1"),
        jobs,
        fail_fast: false,
        kill_grace: Duration::from_secs(5),
        tags: 1 << 40,
    }
}

fn job(name: &str, expected_ms: u64) -> Job {
    Job {
        name: name.into(),
        ignore: false,
        expected: Duration::from_millis(expected_ms),
        timeout: Duration::from_secs(60),
        retries: 0,
    }
}

fn binary(name: &str, jobs: Vec<Job>) -> Binary {
    Binary {
        id: ContentId::of("blaze v2 test binary 1", name.as_bytes()),
        name: name.into(),
        program: PathBuf::from(format!("/bin/{name}")),
        env: vec![("PATH".into(), "/usr/bin".into())],
        dir: "/ws".into(),
        read: vec!["/ws".into()],
        write: vec!["/tmp/t".into()],
        network: false,
        memory: None,
        mode: Mode::Process,
        jobs,
    }
}

fn exited(code: Option<i32>, signal: Option<i32>, traced: Option<Traced>) -> Done {
    Done::Exited(Box::new(Exit {
        code,
        signal,
        stdout: b"server out".to_vec(),
        stderr: Vec::new(),
        traced,
        peak_memory: 0,
    }))
}

fn lines(it: &[(Seen, &str)]) -> Trace {
    Trace::Lines(it.iter().copied().collect())
}

fn unrecorded() -> Executed {
    Executed::Not(UNRECORDED.to_owned())
}

/// The hello indices of `runs`.
fn indices(runs: &[(u64, u32)]) -> Vec<u32> {
    runs.iter().map(|it| it.1).collect()
}

/// A driver played by hand: it answers at once what needs no decision (writes, reads of the
/// records, sends, hang-ups, unlistens), and keeps the rest for the test to answer. Its clock
/// moves only by [`Fake::advance`], which fires the timers due on the way.
struct Fake {
    queue: Queue,
    now: Duration,
    /// Each timer set, when it fires, and how many sends had been asked for before it.
    timers: Vec<(Tag, Duration, usize)>,
    spawns: Vec<(Tag, Command)>,
    kills: Vec<Tag>,
    listens: Vec<(Tag, Address)>,
    recvs: Vec<(Tag, Conn)>,
    sent: Vec<(Conn, Frame)>,
    /// How many sends the queue asked for.
    sends: usize,
    hangups: Vec<Conn>,
    records: BTreeMap<FileName, Vec<u8>>,
    next_conn: u64,
    results: Vec<Finished>,
}

impl Fake {
    fn new(config: Config, binaries: Vec<Binary>) -> Fake {
        let mut effects = Vec::new();
        let queue = Queue::new(config, binaries, Duration::ZERO, &mut effects);
        let mut fake = Fake {
            queue,
            now: Duration::ZERO,
            timers: Vec::new(),
            spawns: Vec::new(),
            kills: Vec::new(),
            listens: Vec::new(),
            recvs: Vec::new(),
            sent: Vec::new(),
            sends: 0,
            hangups: Vec::new(),
            records: BTreeMap::new(),
            next_conn: 0,
            results: Vec::new(),
        };
        fake.pump(effects);
        fake
    }

    fn complete(&mut self, tag: Tag, done: Done) {
        assert!(self.queue.owns(tag));
        let mut effects = Vec::new();
        self.queue.complete(Completion { tag, done }, self.now, &mut effects);
        self.pump(effects);
    }

    fn cancel(&mut self) {
        let mut effects = Vec::new();
        self.queue.cancel(self.now, &mut effects);
        self.pump(effects);
    }

    fn pump(&mut self, mut effects: Vec<Effect>) {
        while !effects.is_empty() {
            let mut answers = Vec::new();
            for effect in effects.drain(..) {
                match effect {
                    Effect::Write { file, bytes, tag } => {
                        self.records.insert(file, bytes);
                        answers.push(Completion { tag, done: Done::Written });
                    }
                    Effect::Read { file, at, len, tag } => {
                        let all = self.records.get(&file).cloned().unwrap_or_default();
                        let at = (at as usize).min(all.len());
                        let bytes = all[at..(at + len).min(all.len())].to_vec();
                        answers.push(Completion { tag, done: Done::Read(bytes) });
                    }
                    Effect::Send { conn, bytes, tag } => {
                        let mut frames = Frames::new();
                        frames.push(&bytes);
                        while let Some(frame) = frames.pop().unwrap() {
                            self.sent.push((conn, frame));
                        }
                        self.sends += 1;
                        answers.push(Completion { tag, done: Done::Sent });
                    }
                    Effect::Unlisten { tag } => {
                        self.listens.retain(|it| it.0 != tag);
                        answers.push(Completion { tag, done: Done::Unlistened });
                    }
                    Effect::Hangup { conn, tag } => {
                        self.hangups.push(conn);
                        answers.push(Completion { tag, done: Done::HungUp });
                    }
                    Effect::Timer { after, tag } => self.timers.push((tag, self.now + after, self.sends)),
                    Effect::Listen { at, tag } => self.listens.push((tag, at)),
                    Effect::Recv { conn, tag, .. } => self.recvs.push((tag, conn)),
                    Effect::Spawn { command, tag } => self.spawns.push((tag, *command)),
                    Effect::Kill { tag } => self.kills.push(tag),
                    other => panic!("the queue asked for {other:?}"),
                }
            }
            for answer in answers {
                self.queue.complete(answer, self.now, &mut effects);
            }
        }
        self.results.extend(self.queue.take());
    }

    /// The clock moved on by `by`, each timer due by then fired at its time, the earliest
    /// first.
    fn advance(&mut self, by: Duration) {
        let to = self.now + by;
        loop {
            let Some(at) = (0..self.timers.len()).filter(|&at| self.timers[at].1 <= to).min_by_key(|&at| self.timers[at].1) else {
                break;
            };
            let (tag, when, _) = self.timers.remove(at);
            self.now = self.now.max(when);
            self.complete(tag, Done::Fired);
        }
        self.now = to;
    }

    fn accept(&mut self, b: usize) -> Conn {
        let at = Address::Unix(name(&format!("req/1/t{b}")));
        let tag = self.listens.iter().find(|it| it.1 == at).expect("a listen").0;
        let conn = Conn(self.next_conn);
        self.next_conn += 1;
        self.complete(tag, Done::Accepted { conn });
        conn
    }

    fn say_bytes(&mut self, conn: Conn, bytes: Vec<u8>) {
        let at = self.recvs.iter().position(|it| it.1 == conn).expect("a receive on the connection");
        let (tag, _) = self.recvs.remove(at);
        self.complete(tag, Done::Read(bytes));
    }

    fn say(&mut self, conn: Conn, frame: Frame) {
        self.say_bytes(conn, frame.to_bytes());
    }

    fn hello(&mut self, b: usize, tests: &[&str]) -> Conn {
        let conn = self.accept(b);
        let tests = tests.iter().map(|it| (*it).to_owned()).collect();
        self.say(conn, Frame::Hello { version: wire::VERSION, pid: 1, tests });
        conn
    }

    fn end(&mut self, conn: Conn, id: u64, end: End, output: &[u8]) {
        self.end_with(conn, id, end, output, unrecorded());
    }

    fn end_with(&mut self, conn: Conn, id: u64, end: End, output: &[u8], executed: Executed) {
        self.say(conn, Frame::Ended { id, end, output: output.to_vec(), executed });
    }

    /// The frames sent to `conn` since the last call.
    fn frames(&mut self, conn: Conn) -> Vec<Frame> {
        let (to, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sent).into_iter().partition(|it| it.0 == conn);
        self.sent = rest;
        to.into_iter().map(|it| it.1).collect()
    }

    fn runs(&mut self, conn: Conn) -> Vec<(u64, u32)> {
        self.frames(conn)
            .into_iter()
            .filter_map(|it| match it {
                Frame::Run { id, index } => Some((id, index)),
                _ => None,
            })
            .collect()
    }

    /// The one run sent to `conn` since the last call, which must be of hello index `index`.
    fn run(&mut self, conn: Conn, index: u32) -> u64 {
        let runs = self.runs(conn);
        assert_eq!(indices(&runs), [index], "{runs:?}");
        runs[0].0
    }

    fn append(&mut self, b: usize, lines: &[Line]) {
        let text: String = lines.iter().map(Line::text).collect();
        self.records.get_mut(&name(&format!("req/1/r{b}"))).expect("the record").extend_from_slice(text.as_bytes());
    }

    fn spawn_of(&self, program: &str) -> Tag {
        self.spawns.iter().find(|it| it.1.program == PathBuf::from(program)).expect("a spawn").0
    }

    fn exit(&mut self, program: &str, done: Done) {
        let tag = self.spawn_of(program);
        self.spawns.retain(|it| it.0 != tag);
        self.complete(tag, done);
    }

    fn result(&self, test: &str) -> &Finished {
        self.results.iter().find(|it| it.test.as_deref() == Some(test)).unwrap_or_else(|| panic!("no result of {test}: {:?}", self.results))
    }
}

#[test]
fn tests_run_from_the_fork_server_longest_first_with_their_own_reads() {
    let mut fake = Fake::new(config(2), vec![binary("lib", vec![job("a", 10), job("b", 50), job("c", 30)])]);
    assert_eq!(fake.records.get(&name("req/1/r0")), Some(&Vec::new()));
    let command = &fake.spawns[0].1;
    assert!(command.env.contains(&(SOCKET.into(), "/root/req/1/t0".into())));
    assert!(command.env.contains(&(confine::LIVE.into(), "/root/req/1/r0".into())));
    assert!(command.env.contains(&("PATH".into(), "/usr/bin".into())));
    assert!(command.args.is_empty());
    let Sandbox::On { read, write, network, trace } = &command.sandbox else { panic!("{command:?}") };
    assert!(*trace && !*network);
    assert!(read.contains(&PathBuf::from("/root/req/1/t0")) && read.contains(&PathBuf::from("/ws")));
    assert_eq!(write, &vec![PathBuf::from("/tmp/t")]);
    let conn = fake.hello(0, &["a", "b", "c"]);
    // Two cores: b and c, the longest.
    let runs = fake.runs(conn);
    assert_eq!(indices(&runs), [1, 2]);
    let (b, c) = (runs[0].0, runs[1].0);
    // One driver timer, for the earlier deadline.
    assert_eq!(fake.timers.len(), 1);
    fake.append(0, &[
        Line::new(Seen::Read, "", "/lib/libc.so"),
        Line::new(Seen::Env, "", SOCKET),
        Line::new(Seen::Looked, "", "/root/req/1/t0"),
        Line::new(Seen::Read, "b", "/ws/b.txt"),
        Line::new(Seen::Read, "c", "/ws/c.txt"),
    ]);
    fake.advance(Duration::from_millis(40));
    fake.end(conn, b, End::Exited(0), b"b ok");
    let result = fake.result("b");
    assert_eq!((&result.outcome, result.output.as_slice(), result.attempts), (&Outcome::Passed, &b"b ok"[..], 1));
    assert_eq!(result.took, Duration::from_millis(40));
    assert_eq!(result.trace, lines(&[(Seen::Read, "/lib/libc.so"), (Seen::Read, "/ws/b.txt")]));
    assert_eq!(result.executed, unrecorded());
    // A core is free: a runs.
    let a = fake.run(conn, 0);
    fake.append(0, &[Line::new(Seen::Env, "c", "HOME")]);
    fake.end(conn, c, End::Exited(101), b"c failed");
    let result = fake.result("c");
    assert_eq!(result.outcome, Outcome::Failed(Failure::Code(101)));
    let Trace::Lines(read) = &result.trace else { panic!("{result:?}") };
    let read: Vec<(Seen, &str)> = read.iter().collect();
    assert!(read.contains(&(Seen::Env, "HOME")) && read.contains(&(Seen::Read, "/ws/c.txt")), "{read:?}");
    assert!(fake.frames(conn).is_empty());
    fake.end(conn, a, End::Exited(0), b"");
    assert_eq!(fake.result("a").outcome, Outcome::Passed);
    // Nothing is left: the server is told to quit, and the queue is done once it has ended.
    assert_eq!(fake.frames(conn), vec![Frame::Quit]);
    assert!(!fake.queue.finished());
    fake.exit("/bin/lib", exited(Some(0), None, Some(Traced::default())));
    assert!(fake.queue.finished());
    assert_eq!(fake.results.len(), 3);
    // The deadlines went with their tests: the timer still in flight does nothing.
    fake.advance(Duration::from_secs(120));
    assert!(fake.frames(conn).is_empty() && fake.kills.is_empty());
}

#[test]
fn a_test_past_its_timeout_is_killed_and_a_server_that_does_not_kill_it_is_killed() {
    let mut slow = job("slow", 0);
    slow.timeout = Duration::from_secs(1);
    let mut stuck = job("stuck", 0);
    stuck.timeout = Duration::from_secs(2);
    let mut fake = Fake::new(config(2), vec![binary("lib", vec![slow, stuck, job("later", 0)])]);
    let conn = fake.hello(0, &["slow", "stuck", "later"]);
    let runs = fake.runs(conn);
    assert_eq!(indices(&runs), [0, 1]);
    let (slow, stuck) = (runs[0].0, runs[1].0);
    // Never early.
    fake.advance(Duration::from_millis(999));
    assert!(fake.frames(conn).is_empty());
    fake.advance(Duration::from_millis(1));
    assert_eq!(fake.frames(conn), vec![Frame::Kill { id: slow }]);
    fake.end(conn, slow, End::Signaled(9), b"partial");
    assert_eq!(fake.result("slow").outcome, Outcome::Failed(Failure::TimedOut(Duration::from_secs(1))));
    let later = fake.run(conn, 2);
    fake.end(conn, later, End::Exited(0), b"");
    fake.advance(Duration::from_secs(1));
    assert_eq!(fake.frames(conn), vec![Frame::Kill { id: stuck }]);
    fake.advance(Duration::from_millis(4_999));
    assert!(fake.kills.is_empty());
    fake.advance(Duration::from_millis(1));
    let server = fake.spawn_of("/bin/lib");
    assert_eq!(fake.kills, vec![server]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("stuck").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("did not end") && why.contains("lib"), "{why}");
    assert!(fake.result("stuck").output.ends_with(b"server out"));
    assert_eq!(fake.result("stuck").executed, Executed::Not(why.clone()));
    assert!(fake.queue.finished());
}

#[test]
fn only_tests_named_flaky_run_again_and_report_every_attempt() {
    let mut flaky = job("flaky", 0);
    flaky.retries = 2;
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![flaky, job("solid", 0)])]);
    let conn = fake.hello(0, &["flaky", "solid"]);
    let first = fake.run(conn, 0);
    fake.end(conn, first, End::Exited(1), b"first\n");
    assert!(fake.results.is_empty());
    let second = fake.run(conn, 0);
    assert_ne!(first, second, "a retry is a new attempt, though its slot is reused");
    fake.end(conn, second, End::Exited(0), b"second\n");
    let flaky = fake.result("flaky");
    assert_eq!((&flaky.outcome, flaky.attempts), (&Outcome::Passed, 2));
    assert_eq!(flaky.output, b"--- attempt 1 ---\nfirst\n--- attempt 2 ---\nsecond\n");
    let solid = fake.run(conn, 1);
    fake.end(conn, solid, End::Exited(1), b"");
    let solid = fake.result("solid");
    assert_eq!((&solid.outcome, solid.attempts), (&Outcome::Failed(Failure::Code(1)), 1));
}

#[test]
fn a_flaky_test_that_fails_every_time_fails_after_its_retries() {
    let mut flaky = job("flaky", 0);
    flaky.retries = 1;
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![flaky, job("solid", 0)])]);
    let conn = fake.hello(0, &["flaky", "solid"]);
    let first = fake.run(conn, 0);
    fake.end(conn, first, End::Signaled(6), b"");
    let second = fake.run(conn, 0);
    fake.end(conn, second, End::Exited(1), b"");
    let flaky = fake.result("flaky");
    assert_eq!((&flaky.outcome, flaky.attempts), (&Outcome::Failed(Failure::Code(1)), 2));
    let solid = fake.run(conn, 1);
    fake.end(conn, solid, End::Exited(1), b"");
    let solid = fake.result("solid");
    assert_eq!((&solid.outcome, solid.attempts), (&Outcome::Failed(Failure::Code(1)), 1));
}

#[test]
fn fail_fast_stops_at_the_first_failure_and_no_fail_fast_runs_all() {
    for fail_fast in [true, false] {
        let config = Config { fail_fast, ..config(1) };
        let mut fake = Fake::new(config, vec![binary("lib", vec![job("a", 30), job("b", 20), job("c", 10)])]);
        let conn = fake.hello(0, &["a", "b", "c"]);
        let a = fake.run(conn, 0);
        fake.end(conn, a, End::Exited(1), b"");
        assert_eq!(fake.result("a").outcome, Outcome::Failed(Failure::Code(1)));
        if fail_fast {
            assert_eq!(fake.result("b").outcome, Outcome::NotRun);
            assert_eq!(fake.result("c").outcome, Outcome::NotRun);
            assert!(matches!(&fake.result("c").executed, Executed::Not(why) if why.contains("stopped")));
            assert_eq!(fake.frames(conn), vec![Frame::Quit]);
        } else {
            let b = fake.run(conn, 1);
            fake.end(conn, b, End::Exited(0), b"");
            let c = fake.run(conn, 2);
            fake.end(conn, c, End::Exited(0), b"");
            assert_eq!(fake.frames(conn), vec![Frame::Quit]);
        }
        fake.exit("/bin/lib", exited(Some(0), None, None));
        assert!(fake.queue.finished());
        assert_eq!(fake.results.len(), 3);
    }
}

#[test]
fn one_queue_over_every_binary_runs_the_longest_ready_test_first() {
    let mut fake = Fake::new(
        config(1),
        vec![binary("a", vec![job("big", 100), job("tiny", 1)]), binary("b", vec![job("mid", 50), job("small", 20)])],
    );
    let a = fake.hello(0, &["big", "tiny"]);
    let big = fake.run(a, 0);
    let b = fake.hello(1, &["small", "mid"]);
    assert!(fake.runs(b).is_empty());
    fake.end(a, big, End::Exited(0), b"");
    assert!(fake.runs(a).is_empty());
    // mid is index 1 of b's hello.
    let mid = fake.run(b, 1);
    fake.end(b, mid, End::Exited(0), b"");
    let small = fake.run(b, 0);
    fake.end(b, small, End::Exited(0), b"");
    assert_eq!(fake.frames(b), vec![Frame::Quit]);
    let tiny = fake.run(a, 1);
    fake.end(a, tiny, End::Exited(0), b"");
    assert_eq!(fake.frames(a), vec![Frame::Quit]);
    let order: Vec<&str> = fake.results.iter().map(|it| it.test.as_deref().unwrap()).collect();
    assert_eq!(order, ["big", "mid", "small", "tiny"]);
    assert_eq!((fake.results[1].binary, fake.results[3].binary), (1, 0));
}

/// Units of equal expected time keep their binary-then-job order, as the sorted set of
/// `(Reverse(expected), binary, job)` that the order replaced had them.
#[test]
fn ties_in_the_order_go_by_binary_then_job() {
    let mut fake = Fake::new(
        config(1),
        vec![binary("a", vec![job("x", 5), job("y", 9), job("z", 5)]), binary("b", vec![job("w", 5), job("v", 9)])],
    );
    let order: Vec<(u32, u32)> =
        fake.queue.order.iter().map(|&unit| (fake.queue.unit_bin[unit as usize], fake.queue.unit_job[unit as usize])).collect();
    assert_eq!(order, [(0, 1), (1, 1), (0, 0), (0, 2), (1, 0)]);
    for (at, &unit) in fake.queue.order.iter().enumerate() {
        assert_eq!(fake.queue.rank[unit as usize], at as u32);
    }
    let a = fake.hello(0, &["x", "y", "z"]);
    let b = fake.hello(1, &["w", "v"]);
    let y = fake.run(a, 1);
    fake.end(a, y, End::Exited(0), b"");
    let v = fake.run(b, 1);
    fake.end(b, v, End::Exited(0), b"");
    let x = fake.run(a, 0);
    fake.end(a, x, End::Exited(0), b"");
}

#[test]
fn a_binary_that_cannot_start_loses_its_tests_with_why() {
    let mut fake = Fake::new(config(4), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let spawn = fake.spawn_of("/bin/lib");
    fake.spawns.clear();
    fake.complete(spawn, Done::Failed(IoError("no user namespaces".into())));
    for test in ["a", "b"] {
        let Outcome::Failed(Failure::Lost(why)) = &fake.result(test).outcome else { panic!("{:?}", fake.results) };
        assert!(why.contains("could not be started in its sandbox: no user namespaces"), "{why}");
        assert_eq!(fake.result(test).trace, Trace::Unknown(why.clone()));
        assert_eq!(fake.result(test).executed, Executed::Not(why.clone()));
    }
    assert!(fake.listens.is_empty());
    assert!(fake.queue.finished());
}

#[test]
fn a_test_the_binary_does_not_have_is_lost() {
    let mut fake = Fake::new(config(4), vec![binary("lib", vec![job("a", 0), job("gone", 0)])]);
    let conn = fake.hello(0, &["a"]);
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("gone").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("no test named gone"), "{why}");
    fake.run(conn, 0);
}

#[test]
fn a_server_that_breaks_the_protocol_or_hangs_up_is_killed() {
    for case in 0..3 {
        let mut fake = Fake::new(config(2), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
        let conn = fake.hello(0, &["a", "b"]);
        assert_eq!(fake.runs(conn).len(), 2);
        let expected = match case {
            0 => {
                fake.say_bytes(conn, vec![1, 0, 0, 0, 77]);
                "broke the protocol"
            }
            1 => {
                fake.say(conn, Frame::Run { id: 0, index: 0 });
                "broke the protocol"
            }
            _ => {
                fake.say_bytes(conn, Vec::new());
                "closed its socket"
            }
        };
        let server = fake.spawn_of("/bin/lib");
        assert_eq!(fake.kills, vec![server]);
        fake.exit("/bin/lib", exited(None, Some(9), None));
        for test in ["a", "b"] {
            let Outcome::Failed(Failure::Lost(why)) = &fake.result(test).outcome else { panic!("{:?}", fake.results) };
            assert!(why.contains(expected), "{why}");
        }
        assert!(fake.queue.finished());
    }
}

#[test]
fn an_end_of_a_test_it_does_not_run_is_a_broken_protocol() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0)])]);
    let conn = fake.hello(0, &["a"]);
    let a = fake.run(conn, 0);
    fake.end(conn, a + 5, End::Exited(0), b"");
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
}

/// An attempt's id outlives it: once its slot is another's, an end of the old id is of no
/// test the server runs.
#[test]
fn an_end_of_an_attempt_already_ended_is_a_broken_protocol() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 1), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    fake.end(conn, a, End::Exited(0), b"");
    let b = fake.run(conn, 1);
    assert_eq!(a as u32, b as u32, "the slot is reused");
    assert_ne!(a, b);
    fake.end(conn, a, End::Exited(0), b"");
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    assert!(matches!(fake.result("b").outcome, Outcome::Failed(Failure::Lost(_))));
}

#[test]
fn a_record_that_is_not_one_leaves_what_was_read_unknown() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"Q\tnot\ta line\n");
    fake.end(conn, a, End::Exited(0), b"");
    let result = fake.result("a");
    assert_eq!(result.outcome, Outcome::Passed);
    assert!(matches!(&result.trace, Trace::Unknown(why) if why.contains("req/1/r0")));
    let b = fake.run(conn, 1);
    fake.end(conn, b, End::Exited(0), b"");
    assert!(matches!(&fake.result("b").trace, Trace::Unknown(_)));
}

#[test]
fn a_line_being_written_waits_for_the_next_read() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"R\ta\t/ws/a\nR\tb\t/ws/");
    fake.end(conn, a, End::Exited(0), b"");
    assert_eq!(fake.result("a").trace, lines(&[(Seen::Read, "/ws/a")]));
    let b = fake.run(conn, 1);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"b\n");
    fake.end(conn, b, End::Exited(0), b"");
    assert_eq!(fake.result("b").trace, lines(&[(Seen::Read, "/ws/b")]));
}

#[test]
fn a_package_that_opts_out_runs_whole_as_libtest_s_threads() {
    let mut ignored = job("y", 5);
    ignored.ignore = true;
    let mut old = binary("old", vec![job("x", 5), ignored]);
    old.mode = Mode::Threads { args: vec!["--nocapture".into()], timeout: Duration::from_secs(10) };
    let mut fake = Fake::new(config(4), vec![old.clone()]);
    assert!(fake.records.is_empty() && fake.listens.is_empty());
    let command = &fake.spawns[0].1;
    let args: Vec<String> = command.args.iter().map(|it| it.to_string_lossy().into_owned()).collect();
    assert_eq!(args, ["x", "y", "--exact", "--test-threads=2", "--include-ignored", "--nocapture"]);
    assert!(!command.env.iter().any(|it| it.0 == SOCKET));
    assert!(matches!(command.sandbox, Sandbox::On { trace: true, .. }));
    let traced = Traced { env: vec![("HOME".into(), Some("/h".into()))], ..Traced::default() };
    fake.exit("/bin/old", exited(Some(0), None, Some(traced.clone())));
    let whole = &fake.results[0];
    assert_eq!((whole.test.as_ref(), &whole.outcome, &whole.trace), (None, &Outcome::Passed, &Trace::Whole(traced)));
    assert_eq!(whole.executed, Executed::Not(WHOLE_UNRECORDED.to_owned()));
    assert!(fake.queue.finished());

    let mut fake = Fake::new(config(4), vec![old]);
    fake.advance(Duration::from_millis(9_999));
    assert!(fake.kills.is_empty());
    fake.advance(Duration::from_millis(1));
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/old")]);
    fake.exit("/bin/old", exited(None, Some(sim::KILLED), None));
    assert_eq!(fake.results[0].outcome, Outcome::Failed(Failure::TimedOut(Duration::from_secs(10))));
    assert!(matches!(&fake.results[0].trace, Trace::Unknown(_)));
}

#[test]
fn a_cancel_kills_what_runs_and_runs_nothing_more() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 10), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    fake.run(conn, 0);
    fake.cancel();
    assert_eq!(fake.result("b").outcome, Outcome::NotRun);
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    assert_eq!(fake.result("a").outcome, Outcome::NotRun);
    assert!(fake.runs(conn).is_empty());
    assert!(fake.queue.finished());
}

/// What the fork server read of a test's region comes with its result, as it was sent.
#[test]
fn what_a_test_executed_comes_with_its_result() {
    let mut fake = Fake::new(config(2), vec![binary("lib", vec![job("a", 2), job("b", 1)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let runs = fake.runs(conn);
    let hits = Executed::Hits { slots: 64, hits: vec![3, 9, 63] };
    fake.end_with(conn, runs[0].0, End::Exited(0), b"", hits.clone());
    fake.end_with(conn, runs[1].0, End::Exited(1), b"", Executed::Not("its region was not armed".to_owned()));
    assert_eq!(fake.result("a").executed, hits);
    assert_eq!(fake.result("b").executed, Executed::Not("its region was not armed".to_owned()));
}

/// The fork server of a simulated binary, played by the test from outside the simulation:
/// it says hello, notes each test's reads in the record and answers each run.
struct Played {
    tests: Vec<String>,
    conn: Option<Conn>,
    frames: Frames,
    quit: bool,
}

/// Two binaries on two cores under the simulation's reordering and latency, on many seeds:
/// every test ends once, with what it read and executed, and the queue ends with every
/// process.
#[test]
fn the_queue_under_a_simulated_driver() {
    for seed in 0..64 {
        let mut sim = Sim::new(seed, Faults::default());
        for program in ["/bin/one", "/bin/two"] {
            sim.program(program, |_: &Command, _: &mut Rng| sim::Ran {
                exit: Exit { code: Some(0), signal: None, stdout: Vec::new(), stderr: Vec::new(), traced: Some(Traced::default()), peak_memory: 0 },
                takes: Duration::from_secs(3600),
            });
        }
        let mut one = binary("one", vec![job("a", 30), job("fails", 20), job("c", 10)]);
        one.jobs[1].retries = 0;
        let two = binary("two", vec![job("d", 50), job("e", 5)]);
        let mut effects = Vec::new();
        let mut queue = Queue::new(config(2), vec![one, two], sim.now(), &mut effects);
        let mut played: Vec<Played> = [vec!["a", "fails", "c"], vec!["d", "e"]]
            .into_iter()
            .map(|tests| Played { tests: tests.into_iter().map(str::to_owned).collect(), conn: None, frames: Frames::new(), quit: false })
            .collect();
        let mut results = Vec::new();
        let mut out = Vec::new();
        let mut rounds = 0;
        while !queue.finished() {
            rounds += 1;
            assert!(rounds < 100_000, "seed {seed}: the queue never ended");
            sim.submit(&mut effects);
            sim.poll(&mut out, Duration::ZERO);
            for completion in out.drain(..) {
                assert!(queue.owns(completion.tag), "seed {seed}");
                queue.complete(completion, sim.now(), &mut effects);
            }
            results.extend(queue.take());
            for (b, server) in played.iter_mut().enumerate() {
                let socket = Address::Unix(name(&format!("req/1/t{b}")));
                let Some(conn) = server.conn.or_else(|| sim.dial(&socket).ok()) else { continue };
                if server.conn.is_none() {
                    server.conn = Some(conn);
                    let hello = Frame::Hello { version: wire::VERSION, pid: 7, tests: server.tests.clone() };
                    sim.peer_send(conn, &hello.to_bytes());
                }
                server.frames.push(&sim.peer_received(conn));
                while let Some(frame) = server.frames.pop().unwrap() {
                    match frame {
                        Frame::Run { id, index } => {
                            let test = &server.tests[index as usize];
                            let record = name(&format!("req/1/r{b}"));
                            let mut bytes = sim.contents(&record);
                            bytes.extend_from_slice(Line::new(Seen::Read, test, &format!("/ws/{test}")).text().as_bytes());
                            sim.change(&record, &bytes);
                            let end = match test.as_str() {
                                "fails" => End::Exited(101),
                                _ => End::Exited(0),
                            };
                            let executed = Executed::Hits { slots: 16, hits: vec![index] };
                            let ended = Frame::Ended { id, end, output: test.as_bytes().to_vec(), executed };
                            sim.peer_send(conn, &ended.to_bytes());
                        }
                        Frame::Quit => {
                            server.quit = true;
                            sim.peer_hangup(conn);
                        }
                        other => panic!("seed {seed}: blaze sent {other:?}"),
                    }
                }
            }
        }
        assert!(played.iter().all(|it| it.quit), "seed {seed}");
        let mut names: Vec<&str> = results.iter().map(|it| it.test.as_deref().unwrap()).collect();
        names.sort_unstable();
        assert_eq!(names, ["a", "c", "d", "e", "fails"], "seed {seed}");
        for result in &results {
            let test = result.test.as_deref().unwrap();
            let expected = match test {
                "fails" => Outcome::Failed(Failure::Code(101)),
                _ => Outcome::Passed,
            };
            assert_eq!(result.outcome, expected, "seed {seed}");
            assert_eq!(result.output, test.as_bytes(), "seed {seed}");
            assert_eq!(result.trace, lines(&[(Seen::Read, &format!("/ws/{test}"))]), "seed {seed}");
            let index = played[result.binary].tests.iter().position(|it| it == test).unwrap() as u32;
            assert_eq!(result.executed, Executed::Hits { slots: 16, hits: vec![index] }, "seed {seed}");
        }
    }
}

#[test]
fn what_one_call_says_to_a_server_goes_as_one_send_with_the_timer_after_it() {
    let mut fake = Fake::new(config(4), vec![binary("lib", vec![job("a", 4), job("b", 3), job("c", 2), job("d", 1)])]);
    let before = fake.sends;
    let conn = fake.hello(0, &["a", "b", "c", "d"]);
    assert_eq!(fake.sends, before + 1);
    let runs = fake.runs(conn);
    assert_eq!(indices(&runs), [0, 1, 2, 3]);
    // One driver timer for the four deadlines, asked for after the send.
    assert_eq!(fake.timers.len(), 1);
    assert_eq!(fake.timers[0].2, before + 1);
    assert!(fake.timers[0].1 <= Duration::from_secs(60));
    fake.advance(Duration::from_millis(59_999));
    assert!(fake.frames(conn).is_empty());
    fake.advance(Duration::from_millis(1));
    let mut killed: Vec<u64> = fake
        .frames(conn)
        .into_iter()
        .map(|it| match it {
            Frame::Kill { id } => id,
            other => panic!("{other:?}"),
        })
        .collect();
    killed.sort_unstable();
    let mut ran: Vec<u64> = runs.iter().map(|it| it.0).collect();
    ran.sort_unstable();
    assert_eq!(killed, ran);
    assert_eq!(fake.sends, before + 2, "the four kills of one call go as one send");
}

#[test]
fn a_line_of_every_scope_is_given_once_however_often_it_comes() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    let a = fake.run(conn, 0);
    let libc = Line::new(Seen::Read, "", "/lib/libc.so");
    let looked = Line::new(Seen::Looked, "", "/lib/libc.so");
    fake.append(0, &[libc.clone(), libc.clone(), looked.clone(), Line::new(Seen::Read, "a", "/ws/a"), libc.clone()]);
    fake.end(conn, a, End::Exited(0), b"");
    let common = [(Seen::Read, "/lib/libc.so"), (Seen::Looked, "/lib/libc.so")];
    assert_eq!(fake.result("a").trace, lines(&[common[0], common[1], (Seen::Read, "/ws/a")]));
    let b = fake.run(conn, 1);
    fake.append(0, &[libc, Line::new(Seen::Read, "b", "/ws/b")]);
    fake.end(conn, b, End::Exited(0), b"");
    assert_eq!(fake.result("b").trace, lines(&[common[0], common[1], (Seen::Read, "/ws/b")]));
}

#[test]
fn a_line_that_is_not_utf_8_is_read_as_the_lossy_record_has_it() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0)])]);
    let conn = fake.hello(0, &["a"]);
    let a = fake.run(conn, 0);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"R\ta\t/ws/\xff\xfex\n");
    fake.end(conn, a, End::Exited(0), b"");
    let text = String::from_utf8_lossy(b"R\ta\t/ws/\xff\xfex").into_owned();
    let line = Line::parse(&text).unwrap();
    assert_eq!(fake.result("a").trace, lines(&[(line.seen, &line.what)]));
}

/// The lines a test is given go to [`trace::assemble`] as the lines it was: scope dropped,
/// which `assemble` reads as of every scope.
#[test]
fn lines_are_read_back_as_they_were_pushed() {
    let given = [(Seen::Read, "/a"), (Seen::Env, "HOME"), (Seen::Read, ""), (Seen::Listed, "/é/b")];
    let lines: Lines = given.iter().copied().collect();
    assert_eq!(lines.len(), 4);
    assert!(!lines.is_empty() && Lines::new().is_empty());
    assert_eq!(lines.iter().collect::<Vec<_>>(), given);
}

/// The queue's borrowed reading of a record's line against [`Line::parse`], over seeded lines
/// of the characters that matter (letters, tabs, a scope, a multi-byte character).
#[test]
fn a_line_is_read_in_place_as_line_parse_reads_it() {
    let alphabet = ['R', 'L', 'D', 'E', 'T', 'W', 'N', 'X', 'Q', '\t', 'a', '/', 'é', '*'];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..20_000 {
        let len = (next() % 12) as usize;
        let text: String = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
        let read = record_line(&text).map(|(seen, scope, what)| Line { seen, scope: scope.to_owned(), what: what.to_owned() });
        assert_eq!(read, Line::parse(&text), "{text:?}");
    }
}

/// [`SEEN`] is [`Seen`] in its declared order, so a packed line reads back its kind.
#[test]
fn a_packed_line_reads_back_its_kind() {
    for (at, &seen) in SEEN.iter().enumerate() {
        assert_eq!(seen as usize, at);
        assert_eq!(unpack(pack(names::MOST - 1, seen)), (names::MOST - 1, seen));
        assert_eq!(Seen::of(&Line::new(seen, "", "").text()[..1]), Some(seen));
        assert_eq!(record_line(&format!("{}\t\tx", &Line::new(seen, "", "").text()[..1])), Some((seen, "", "x")));
    }
}

/// The bitsets against a sorted set, over seeded sets, clears and firsts.
#[test]
fn bits_are_a_set() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for len in [0usize, 1, 63, 64, 65, 200] {
        let mut bits = Bits::full(len);
        let mut set: BTreeSet<u32> = (0..len as u32).collect();
        assert_eq!(bits.ones().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
        if len == 0 {
            assert_eq!(bits.first(), None);
            continue;
        }
        for _ in 0..2_000 {
            let at = (next() % len as u64) as u32;
            match next() % 3 {
                0 => {
                    bits.set(at);
                    set.insert(at);
                }
                1 => {
                    bits.clear(at);
                    set.remove(&at);
                }
                _ => {
                    let first = bits.first();
                    assert_eq!(first, set.first().copied());
                    if let Some(first) = first {
                        bits.clear(first);
                        set.remove(&first);
                    }
                }
            }
            assert_eq!(bits.has(at), set.contains(&at));
        }
        assert_eq!(bits.ones().collect::<Vec<_>>(), set.iter().copied().collect::<Vec<_>>());
        bits.clear_all();
        assert_eq!((bits.first(), bits.ones().next()), (None, None));
        let mut empty = Bits::empty(len);
        assert_eq!(empty.first(), None);
    }
}

/// Many tests through few cores: the attempt slab, the tag slab, the wheel and each server's
/// lists stay at what the cores need, however many tests run.
#[test]
fn the_steady_state_reuses_its_slabs() {
    let jobs: Vec<Job> = (0..600).map(|n| job(&format!("t{n}"), 600 - n)).collect();
    let names: Vec<String> = jobs.iter().map(|it| it.name.clone()).collect();
    let mut fake = Fake::new(config(4), vec![binary("lib", jobs)]);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let conn = fake.hello(0, &names);
    let mut ended = 0;
    let mut high = None;
    while ended < 600 {
        let runs = fake.runs(conn);
        assert!(!runs.is_empty(), "nothing runs after {ended}");
        for (id, index) in runs {
            fake.append(0, &[Line::new(Seen::Read, "", "/lib/libc.so"), Line::new(Seen::Read, names[index as usize], "/ws/x")]);
            fake.advance(Duration::from_millis(3));
            fake.end(conn, id, End::Exited(0), b"ok");
            ended += 1;
        }
        let queue = &fake.queue;
        let server = queue.bins[0].server.as_ref().unwrap();
        let now = (queue.attempts.len(), queue.tags.len(), queue.wheel.made(), server.waiting.capacity(), server.next.capacity());
        if ended == 100 {
            high = Some(now);
        }
        if let Some(high) = high {
            assert_eq!(now, high, "after {ended} tests");
        }
        assert!(queue.attempts.len() <= 4 && queue.wheel.made() <= 8, "{now:?}");
    }
    assert_eq!(fake.results.len(), 600);
    assert!(fake.results.iter().all(|it| it.outcome == Outcome::Passed));
    let server = fake.queue.bins[0].server.as_ref().unwrap();
    // The line of every scope is kept once; every name once (blaze's four, then two).
    assert_eq!(server.common.len(), 1);
    assert_eq!(server.names.len(), 6);
}
