use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use blaze_core::{
    Address, Command, Completion, Conn, Done, Driver, Effect, Exit, FileName, IoError, Sandbox, Tag,
    Traced,
    sim::{self, Faults, Rng, Sim},
};
use blaze_ids::ContentId;

use super::*;
use crate::{
    confine,
    test::wire::{self, End, Frame, Frames, SOCKET},
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

/// A driver played by hand: it answers at once what needs no decision (writes, reads of the
/// records, sends, hang-ups, unlistens), and keeps the rest for the test to answer.
struct Fake {
    queue: Queue,
    now: Duration,
    timers: Vec<(Tag, Duration)>,
    /// The deadline and the grace timer of each run, by its id.
    deadlines: BTreeMap<u64, Tag>,
    graces: BTreeMap<u64, Tag>,
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
            deadlines: BTreeMap::new(),
            graces: BTreeMap::new(),
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
        // The runs and kills sent whose timers have not come yet: a server's timers follow the
        // send of its frames, in their order.
        let mut awaiting: std::collections::VecDeque<Frame> = std::collections::VecDeque::new();
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
                            if matches!(frame, Frame::Run { .. } | Frame::Kill { .. }) {
                                awaiting.push_back(frame.clone());
                            }
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
                    Effect::Timer { after, tag } => {
                        match awaiting.pop_front() {
                            Some(Frame::Run { id, .. }) => _ = self.deadlines.insert(id, tag),
                            Some(Frame::Kill { id }) => _ = self.graces.insert(id, tag),
                            _ => {}
                        }
                        self.timers.push((tag, after));
                    }
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
        self.say(conn, Frame::Ended { id, end, output: output.to_vec() });
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

    fn append(&mut self, b: usize, lines: &[Line]) {
        let text: String = lines.iter().map(Line::text).collect();
        self.records.get_mut(&name(&format!("req/1/r{b}"))).expect("the record").extend_from_slice(text.as_bytes());
    }

    fn fire(&mut self, tag: Tag) {
        let at = self.timers.iter().position(|it| it.0 == tag).expect("a timer");
        let (_, after) = self.timers.remove(at);
        self.now += after;
        self.complete(tag, Done::Fired);
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
    assert_eq!(fake.runs(conn), vec![(0, 1), (1, 2)]);
    assert!(fake.deadlines.contains_key(&0) && fake.deadlines.contains_key(&1));
    fake.append(0, &[
        Line::new(Seen::Read, "", "/lib/libc.so"),
        Line::new(Seen::Env, "", SOCKET),
        Line::new(Seen::Looked, "", "/root/req/1/t0"),
        Line::new(Seen::Read, "b", "/ws/b.txt"),
        Line::new(Seen::Read, "c", "/ws/c.txt"),
    ]);
    fake.now = Duration::from_millis(40);
    fake.end(conn, 0, End::Exited(0), b"b ok");
    let b = fake.result("b");
    assert_eq!((&b.outcome, b.output.as_slice(), b.attempts), (&Outcome::Passed, &b"b ok"[..], 1));
    assert_eq!(b.took, Duration::from_millis(40));
    assert_eq!(b.trace, Trace::Lines(vec![Line::new(Seen::Read, "", "/lib/libc.so"), Line::new(Seen::Read, "b", "/ws/b.txt")]));
    // A core is free: a runs.
    assert_eq!(fake.runs(conn), vec![(2, 0)]);
    fake.append(0, &[Line::new(Seen::Env, "c", "HOME")]);
    fake.end(conn, 1, End::Exited(101), b"c failed");
    let c = fake.result("c");
    assert_eq!(c.outcome, Outcome::Failed(Failure::Code(101)));
    let Trace::Lines(lines) = &c.trace else { panic!("{c:?}") };
    assert!(lines.contains(&Line::new(Seen::Env, "c", "HOME")) && lines.contains(&Line::new(Seen::Read, "c", "/ws/c.txt")));
    assert!(fake.frames(conn).is_empty());
    fake.end(conn, 2, End::Exited(0), b"");
    assert_eq!(fake.result("a").outcome, Outcome::Passed);
    // Nothing is left: the server is told to quit, and the queue is done once it has ended.
    assert_eq!(fake.frames(conn), vec![Frame::Quit]);
    assert!(!fake.queue.finished());
    fake.exit("/bin/lib", exited(Some(0), None, Some(Traced::default())));
    assert!(fake.queue.finished());
    assert_eq!(fake.results.len(), 3);
    // A deadline that fires after its test ended does nothing.
    let stale = fake.deadlines[&0];
    fake.fire(stale);
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
    assert_eq!(fake.runs(conn), vec![(0, 0), (1, 1)]);
    let deadline = fake.deadlines[&0];
    fake.fire(deadline);
    assert_eq!(fake.frames(conn), vec![Frame::Kill { id: 0 }]);
    fake.end(conn, 0, End::Signaled(9), b"partial");
    assert_eq!(fake.result("slow").outcome, Outcome::Failed(Failure::TimedOut(Duration::from_secs(1))));
    assert_eq!(fake.runs(conn), vec![(2, 2)]);
    fake.end(conn, 2, End::Exited(0), b"");
    let deadline = fake.deadlines[&1];
    fake.fire(deadline);
    assert_eq!(fake.frames(conn), vec![Frame::Kill { id: 1 }]);
    let grace = fake.graces[&1];
    fake.fire(grace);
    let server = fake.spawn_of("/bin/lib");
    assert_eq!(fake.kills, vec![server]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    let Outcome::Failed(Failure::Lost(why)) = &fake.result("stuck").outcome else { panic!("{:?}", fake.results) };
    assert!(why.contains("did not end") && why.contains("lib"), "{why}");
    assert!(fake.result("stuck").output.ends_with(b"server out"));
    assert!(fake.queue.finished());
}

#[test]
fn only_tests_named_flaky_run_again_and_report_every_attempt() {
    let mut flaky = job("flaky", 0);
    flaky.retries = 2;
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![flaky, job("solid", 0)])]);
    let conn = fake.hello(0, &["flaky", "solid"]);
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    fake.end(conn, 0, End::Exited(1), b"first\n");
    assert!(fake.results.is_empty());
    assert_eq!(fake.runs(conn), vec![(1, 0)]);
    fake.end(conn, 1, End::Exited(0), b"second\n");
    let flaky = fake.result("flaky");
    assert_eq!((&flaky.outcome, flaky.attempts), (&Outcome::Passed, 2));
    assert_eq!(flaky.output, b"--- attempt 1 ---\nfirst\n--- attempt 2 ---\nsecond\n");
    assert_eq!(fake.runs(conn), vec![(2, 1)]);
    fake.end(conn, 2, End::Exited(1), b"");
    let solid = fake.result("solid");
    assert_eq!((&solid.outcome, solid.attempts), (&Outcome::Failed(Failure::Code(1)), 1));
}

#[test]
fn a_flaky_test_that_fails_every_time_fails_after_its_retries() {
    let mut flaky = job("flaky", 0);
    flaky.retries = 1;
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![flaky])]);
    let conn = fake.hello(0, &["flaky"]);
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    fake.end(conn, 0, End::Signaled(6), b"");
    assert_eq!(fake.runs(conn), vec![(1, 0)]);
    fake.end(conn, 1, End::Exited(1), b"");
    let flaky = fake.result("flaky");
    assert_eq!((&flaky.outcome, flaky.attempts), (&Outcome::Failed(Failure::Code(1)), 2));
}

#[test]
fn fail_fast_stops_at_the_first_failure_and_no_fail_fast_runs_all() {
    for fail_fast in [true, false] {
        let config = Config { fail_fast, ..config(1) };
        let mut fake = Fake::new(config, vec![binary("lib", vec![job("a", 30), job("b", 20), job("c", 10)])]);
        let conn = fake.hello(0, &["a", "b", "c"]);
        assert_eq!(fake.runs(conn), vec![(0, 0)]);
        fake.end(conn, 0, End::Exited(1), b"");
        assert_eq!(fake.result("a").outcome, Outcome::Failed(Failure::Code(1)));
        if fail_fast {
            assert_eq!(fake.result("b").outcome, Outcome::NotRun);
            assert_eq!(fake.result("c").outcome, Outcome::NotRun);
            assert_eq!(fake.frames(conn), vec![Frame::Quit]);
        } else {
            assert_eq!(fake.runs(conn), vec![(1, 1)]);
            fake.end(conn, 1, End::Exited(0), b"");
            assert_eq!(fake.runs(conn), vec![(2, 2)]);
            fake.end(conn, 2, End::Exited(0), b"");
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
    assert_eq!(fake.runs(a), vec![(0, 0)]);
    let b = fake.hello(1, &["small", "mid"]);
    assert!(fake.runs(b).is_empty());
    fake.end(a, 0, End::Exited(0), b"");
    assert!(fake.runs(a).is_empty());
    // mid is index 1 of b's hello.
    assert_eq!(fake.runs(b), vec![(1, 1)]);
    fake.end(b, 1, End::Exited(0), b"");
    assert_eq!(fake.runs(b), vec![(2, 0)]);
    fake.end(b, 2, End::Exited(0), b"");
    assert_eq!(fake.frames(b), vec![Frame::Quit]);
    assert_eq!(fake.runs(a), vec![(3, 1)]);
    fake.end(a, 3, End::Exited(0), b"");
    assert_eq!(fake.frames(a), vec![Frame::Quit]);
    let order: Vec<&str> = fake.results.iter().map(|it| it.test.as_deref().unwrap()).collect();
    assert_eq!(order, ["big", "mid", "small", "tiny"]);
    assert_eq!((fake.results[1].binary, fake.results[3].binary), (1, 0));
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
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
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
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    fake.end(conn, 5, End::Exited(0), b"");
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
}

#[test]
fn a_record_that_is_not_one_leaves_what_was_read_unknown() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"Q\tnot\ta line\n");
    fake.end(conn, 0, End::Exited(0), b"");
    let a = fake.result("a");
    assert_eq!(a.outcome, Outcome::Passed);
    assert!(matches!(&a.trace, Trace::Unknown(why) if why.contains("req/1/r0")));
    assert_eq!(fake.runs(conn), vec![(1, 1)]);
    fake.end(conn, 1, End::Exited(0), b"");
    assert!(matches!(&fake.result("b").trace, Trace::Unknown(_)));
}

#[test]
fn a_line_being_written_waits_for_the_next_read() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"R\ta\t/ws/a\nR\tb\t/ws/");
    fake.end(conn, 0, End::Exited(0), b"");
    assert_eq!(fake.result("a").trace, Trace::Lines(vec![Line::new(Seen::Read, "a", "/ws/a")]));
    assert_eq!(fake.runs(conn), vec![(1, 1)]);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"b\n");
    fake.end(conn, 1, End::Exited(0), b"");
    assert_eq!(fake.result("b").trace, Trace::Lines(vec![Line::new(Seen::Read, "b", "/ws/b")]));
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
    assert!(fake.queue.finished());

    let mut fake = Fake::new(config(4), vec![old]);
    let (deadline, _) = fake.timers[0];
    fake.fire(deadline);
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/old")]);
    fake.exit("/bin/old", exited(None, Some(sim::KILLED), None));
    assert_eq!(fake.results[0].outcome, Outcome::Failed(Failure::TimedOut(Duration::from_secs(10))));
    assert!(matches!(&fake.results[0].trace, Trace::Unknown(_)));
}

#[test]
fn a_cancel_kills_what_runs_and_runs_nothing_more() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 10), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    fake.cancel();
    assert_eq!(fake.result("b").outcome, Outcome::NotRun);
    assert_eq!(fake.kills, vec![fake.spawn_of("/bin/lib")]);
    fake.exit("/bin/lib", exited(None, Some(9), None));
    assert_eq!(fake.result("a").outcome, Outcome::NotRun);
    assert!(fake.runs(conn).is_empty());
    assert!(fake.queue.finished());
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
/// every test ends once, with what it read, and the queue ends with every process.
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
                            let ended = Frame::Ended { id, end, output: test.as_bytes().to_vec() };
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
            assert_eq!(result.trace, Trace::Lines(vec![Line::new(Seen::Read, test, &format!("/ws/{test}"))]), "seed {seed}");
        }
    }
}

#[test]
fn what_one_call_says_to_a_server_goes_as_one_send_with_each_timer_after_it() {
    let mut fake = Fake::new(config(4), vec![binary("lib", vec![job("a", 4), job("b", 3), job("c", 2), job("d", 1)])]);
    let before = fake.sends;
    let conn = fake.hello(0, &["a", "b", "c", "d"]);
    assert_eq!(fake.sends, before + 1);
    assert_eq!(fake.runs(conn), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    for id in 0..4 {
        assert_eq!(fake.timers.iter().find(|it| it.0 == fake.deadlines[&id]).map(|it| it.1), Some(Duration::from_secs(60)));
    }
}

#[test]
fn a_line_of_every_scope_is_given_once_however_often_it_comes() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0), job("b", 0)])]);
    let conn = fake.hello(0, &["a", "b"]);
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    let libc = Line::new(Seen::Read, "", "/lib/libc.so");
    let looked = Line::new(Seen::Looked, "", "/lib/libc.so");
    fake.append(0, &[libc.clone(), libc.clone(), looked.clone(), Line::new(Seen::Read, "a", "/ws/a"), libc.clone()]);
    fake.end(conn, 0, End::Exited(0), b"");
    assert_eq!(fake.result("a").trace, Trace::Lines(vec![libc.clone(), looked.clone(), Line::new(Seen::Read, "a", "/ws/a")]));
    assert_eq!(fake.runs(conn), vec![(1, 1)]);
    fake.append(0, &[libc.clone(), Line::new(Seen::Read, "b", "/ws/b")]);
    fake.end(conn, 1, End::Exited(0), b"");
    assert_eq!(fake.result("b").trace, Trace::Lines(vec![libc, looked, Line::new(Seen::Read, "b", "/ws/b")]));
}

#[test]
fn a_line_that_is_not_utf_8_is_read_as_the_lossy_record_has_it() {
    let mut fake = Fake::new(config(1), vec![binary("lib", vec![job("a", 0)])]);
    let conn = fake.hello(0, &["a"]);
    assert_eq!(fake.runs(conn), vec![(0, 0)]);
    fake.records.get_mut(&name("req/1/r0")).unwrap().extend_from_slice(b"R\ta\t/ws/\xff\xfex\n");
    fake.end(conn, 0, End::Exited(0), b"");
    let text = String::from_utf8_lossy(b"R\ta\t/ws/\xff\xfex").into_owned();
    assert_eq!(fake.result("a").trace, Trace::Lines(vec![Line::parse(&text).unwrap()]));
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
