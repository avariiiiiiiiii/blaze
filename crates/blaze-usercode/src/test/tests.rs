use std::time::Duration;

use super::{
    FORK_SERVER, Failure, Finished, Outcome, Reported, Trace, report,
    wire::{End, Frame, Frames, LONGEST},
};

fn frames() -> Vec<Frame> {
    vec![
        Frame::Hello { version: 1, pid: 42, tests: vec!["a::b".into(), "ünï::cødé".into(), String::new()] },
        Frame::Ended { id: 7, end: End::Exited(0), output: b"ok\n".to_vec() },
        Frame::Ended { id: u64::MAX, end: End::Exited(-1), output: Vec::new() },
        Frame::Ended { id: 8, end: End::Signaled(9), output: vec![0, 255, 10] },
        Frame::Ended { id: 9, end: End::NotStarted("fork: no memory".into()), output: Vec::new() },
        Frame::Run { id: 3, index: 2 },
        Frame::Kill { id: 3 },
        Frame::Quit,
    ]
}

#[test]
fn every_frame_round_trips() {
    for frame in frames() {
        let bytes = frame.to_bytes();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4, "{frame:?}");
        assert_eq!(Frame::decode(&bytes[4..]), Ok(frame));
    }
}

#[test]
fn frames_come_whole_whatever_the_pieces() {
    let mut stream = Vec::new();
    for frame in frames() {
        frame.encode(&mut stream);
    }
    for piece in [1, 2, 3, 5, 7, 64, stream.len()] {
        let mut reader = Frames::new();
        let mut got = Vec::new();
        for chunk in stream.chunks(piece) {
            reader.push(chunk);
            while let Some(frame) = reader.pop().unwrap() {
                got.push(frame);
            }
        }
        assert_eq!(got, frames(), "pieces of {piece}");
        assert_eq!(reader.waiting(), 0);
    }
}

#[test]
fn a_frame_that_is_not_one_is_refused() {
    // Too long to be one: refused before its bytes come.
    let mut reader = Frames::new();
    reader.push(&(LONGEST + 1).to_le_bytes());
    assert!(reader.pop().is_err());
    // An unknown kind, bytes after the end, a body that ends early, a string not UTF-8.
    assert!(Frame::decode(&[77]).is_err());
    let mut quit = Frame::Quit.to_bytes()[4..].to_vec();
    quit.push(0);
    assert!(Frame::decode(&quit).is_err());
    let run = Frame::Run { id: 1, index: 2 }.to_bytes();
    assert!(Frame::decode(&run[4..run.len() - 1]).is_err());
    let mut hello = Frame::Hello { version: 1, pid: 1, tests: vec!["ab".into()] }.to_bytes()[4..].to_vec();
    let at = hello.len() - 1;
    hello[at] = 0xff;
    assert!(Frame::decode(&hello).is_err());
    assert!(Frame::decode(&[]).is_err());
    // A count past what the body holds ends early, never allocates for it.
    let mut lying = vec![1];
    lying.extend_from_slice(&1u32.to_le_bytes());
    lying.extend_from_slice(&1u32.to_le_bytes());
    lying.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(Frame::decode(&lying).is_err());
}

#[test]
fn the_harness_text_is_one_module_with_the_protocol_and_the_server() {
    assert!(FORK_SERVER.trim_start().starts_with("pub mod fork_server {"));
    assert!(FORK_SERVER.contains("pub const SOCKET: &str = \"__BLAZE_FORK_SERVER\";"));
    assert!(FORK_SERVER.contains("pub fn serve(tests: &'static [&'static super::test::TestDescAndFn], socket: ::std::ffi::OsString) -> !"));
    // Nothing of blaze is named in what the tested crate compiles.
    let code: Vec<&str> = FORK_SERVER.lines().filter(|it| !it.trim_start().starts_with("//")).collect();
    assert!(code.iter().all(|it| !it.contains("crate::") && !it.contains("blaze_")), "{code:#?}");
    // An inner doc comment only where a module starts: the protocol's, first.
    let after_open = FORK_SERVER.trim_start().trim_start_matches("pub mod fork_server {").trim_start();
    assert!(after_open.starts_with("//!"));
    let past_docs: String = after_open.lines().skip_while(|it| it.starts_with("//!")).collect::<Vec<_>>().join("\n");
    assert!(!past_docs.contains("//!"));
    // No test of blaze's in it, which the tested crate's harness would run as its own.
    // A child's end wakes the server (a pidfd, or a kqueue): it never waits on a clock.
    assert!(FORK_SERVER.contains("const PIDFD_OPEN: ::core::ffi::c_long = 434;"));
    assert!(FORK_SERVER.contains("fn kqueue() -> Int;"));
    assert!(FORK_SERVER.contains("let timeout = if now { 0 } else { -1 };"));
    assert!(!FORK_SERVER.contains("#[test]"));
    assert!(!FORK_SERVER.contains("#[cfg(test)]"));
    let (open, close) = FORK_SERVER.chars().fold((0, 0), |(open, close), it| match it {
        '{' => (open + 1, close),
        '}' => (open, close + 1),
        _ => (open, close),
    });
    assert_eq!(open, close);
}

fn finished(outcome: Outcome) -> Finished {
    Finished {
        binary: 0,
        test: Some("a::b".into()),
        outcome,
        attempts: 1,
        output: b"out".to_vec(),
        took: Duration::from_millis(1500),
        trace: Trace::Unknown("not traced".into()),
    }
}

#[test]
fn results_are_reported_as_the_api_has_them() {
    let passed = report("lib", &finished(Outcome::Passed)).unwrap();
    assert_eq!((passed.outcome, passed.millis, passed.name.as_str()), (Reported::Passed, 1500, "a::b"));
    assert_eq!(passed.output, b"out");
    let timed_out = report("lib", &finished(Outcome::Failed(Failure::TimedOut(Duration::from_secs(60))))).unwrap();
    assert_eq!(timed_out.outcome, Reported::Failed);
    assert!(String::from_utf8_lossy(&timed_out.output).contains("timeout"));
    let code = report("lib", &finished(Outcome::Failed(Failure::Code(1)))).unwrap();
    assert_eq!(code.output, b"out");
    assert_eq!(report("lib", &finished(Outcome::NotRun)), None);
    let mut whole = finished(Outcome::Passed);
    whole.test = None;
    assert_eq!(report("lib", &whole).unwrap().name, "");
}
