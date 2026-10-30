use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use blaze_api::http;
use blaze_ids::ContentId;
use blaze_vfs::Op;

use super::{
    Post, batches, listen, load, post, save,
    listen::{Listener, Seen},
};
use crate::{
    CheckRun, CheckStatus, Checkout, Event,
    config::Secret,
    github::{Checks, GitHubError},
    service::{Held, Input, RunId, State},
    snapshot::Kept,
};

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("blaze-ci-runtime-{}-{name}", std::process::id()))
}

fn cost(op: &Op) -> usize {
    match op {
        Op::Write { path, bytes } | Op::Edit { path, bytes, .. } => path.len() + bytes.len() + super::OP_OVERHEAD,
        Op::Delete { path } => path.len() + super::OP_OVERHEAD,
        Op::Rename { from, to } => from.len() + to.len() + super::OP_OVERHEAD,
    }
}

#[test]
fn no_ops_are_one_empty_batch_and_small_ops_one_batch() {
    assert_eq!(batches(&[], 1000).unwrap(), vec![Vec::<Op>::new()]);
    let ops = vec![
        Op::Write { path: "a".to_owned(), bytes: vec![1; 10] },
        Op::Delete { path: "b".to_owned() },
        Op::Rename { from: "c".to_owned(), to: "d".to_owned() },
    ];
    assert_eq!(batches(&ops, 1000).unwrap(), vec![ops]);
}

#[test]
fn batches_stay_under_the_bound_and_keep_the_order() {
    let ops: Vec<Op> = (0..50).map(|n| Op::Write { path: format!("f{n:02}"), bytes: vec![n; 100] }).collect();
    let cut = batches(&ops, 500).unwrap();
    assert!(cut.len() > 1);
    for batch in &cut {
        assert!(!batch.is_empty());
        assert!(batch.iter().map(cost).sum::<usize>() <= 500);
    }
    assert_eq!(cut.concat(), ops);
}

#[test]
fn a_file_too_large_for_one_request_is_written_then_appended() {
    let bytes: Vec<u8> = (0..2500u32).map(|n| n as u8).collect();
    let ops = vec![
        Op::Delete { path: "gone".to_owned() },
        Op::Write { path: "big".to_owned(), bytes: bytes.clone() },
        Op::Write { path: "small".to_owned(), bytes: vec![7] },
    ];
    let cut = batches(&ops, 1000).unwrap();
    let flat = cut.concat();
    assert_eq!(flat[0], Op::Delete { path: "gone".to_owned() });
    let mut rebuilt = Vec::new();
    for op in &flat[1..flat.len() - 1] {
        match op {
            Op::Write { path, bytes } => {
                assert_eq!(path, "big");
                assert!(rebuilt.is_empty());
                rebuilt.extend_from_slice(bytes);
            }
            Op::Edit { path, at, len, bytes } => {
                assert_eq!(path, "big");
                assert_eq!((*at, *len), (rebuilt.len() as u64, 0));
                rebuilt.extend_from_slice(bytes);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(rebuilt, bytes);
    assert_eq!(flat.last(), Some(&Op::Write { path: "small".to_owned(), bytes: vec![7] }));
    for batch in &cut {
        assert!(batch.iter().map(cost).sum::<usize>() <= 1000, "{batch:?}");
    }
}

#[test]
fn a_path_longer_than_a_request_is_refused_naming_it() {
    let ops = vec![Op::Write { path: "p".repeat(2000), bytes: vec![0; 10] }];
    assert!(batches(&ops, 1000).unwrap_err().contains("ppp"));
}

#[test]
fn the_state_file_round_trips_and_is_empty_at_first() {
    let path = temp("state.json");
    let _ = std::fs::remove_file(&path);
    assert_eq!(load(&path).unwrap(), State::default());
    let mut state = State::default();
    state.checkouts.insert(
        "branch/main".to_owned(),
        Held { checkout: Checkout { commit: "c".to_owned(), root: ContentId([9; 32]) }, at: 4 },
    );
    state.snapshots.push(Kept { id: "s".to_owned(), name: "n".to_owned(), at: 5 });
    save(&path, &state).unwrap();
    assert_eq!(load(&path).unwrap(), state);
    std::fs::write(&path, "{\"checkouts\": {}, \"snapshots\": [], \"other\": 1}").unwrap();
    let error = load(&path).unwrap_err();
    assert!(error.contains("other") && error.contains("state"), "{error}");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn the_oldest_delivery_ids_are_forgotten() {
    let mut seen = Seen::default();
    assert!(seen.first("a"));
    assert!(!seen.first("a"));
    for n in 0..listen::REMEMBERED {
        assert!(seen.first(&n.to_string()));
    }
    assert!(seen.first("a"), "forgotten after REMEMBERED others");
    assert!(!seen.first(&(listen::REMEMBERED - 1).to_string()));
}

/// Fails the first `failing` calls.
struct Flaky {
    failing: u32,
    calls: u32,
}

impl Checks for Flaky {
    fn create(&mut self, _: &CheckRun) -> Result<u64, GitHubError> {
        self.calls += 1;
        if self.calls <= self.failing {
            return Err(GitHubError { what: "502".to_owned(), status: Some(502) });
        }
        Ok(77)
    }

    fn update(&mut self, _: u64, run: &CheckRun) -> Result<(), GitHubError> {
        self.create(run).map(drop)
    }
}

fn run_check() -> CheckRun {
    CheckRun { name: "blaze/check".to_owned(), commit: "c".to_owned(), status: CheckStatus::Queued, summary: String::new() }
}

#[test]
fn github_errors_are_retried_five_times() {
    for (failing, created) in [(0, true), (5, true), (6, false)] {
        let github = Arc::new(Mutex::new(Flaky { failing, calls: 0 }));
        let (posts, receive) = mpsc::channel();
        let (service, inputs) = mpsc::channel();
        posts.send(Post::Create { run: RunId(3), check: run_check() }).unwrap();
        drop(posts);
        post(github.clone(), receive, service, Duration::ZERO);
        let Ok(Input::Created { run, result }) = inputs.recv() else { panic!("nothing created") };
        assert_eq!(run, RunId(3));
        assert_eq!(result.is_ok(), created, "{failing}: {result:?}");
        if let Err(what) = result {
            assert!(what.contains("blaze/check") && what.contains("6 times"), "{what}");
        }
        assert_eq!(github.lock().unwrap().calls, (failing + 1).min(6));
    }
}

fn sign(secret: &[u8], body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={hex}")
}

fn deliver(at: std::net::SocketAddr, id: &str, signature: &str, body: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(at).unwrap();
    let headers = [
        ("X-GitHub-Event", "push"),
        ("X-GitHub-Delivery", id),
        ("X-Hub-Signature-256", signature),
        ("Content-Type", "application/json"),
    ];
    stream.write_all(&http::request("POST", "localhost", "/", &headers, body)).unwrap();
    let response = http::read_response(&mut stream, 1 << 20).unwrap();
    (response.status, String::from_utf8_lossy(&response.body).into_owned())
}

#[test]
fn needs_loopback_a_delivery_is_checked_answered_and_handed_on_once() {
    let path = temp("webhook-secret");
    std::fs::write(&path, "It's a Secret to Everybody\n").unwrap();
    let secret = Arc::new(Secret::read(&path).unwrap());
    std::fs::remove_file(&path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let at = listener.local_addr().unwrap();
    let (service, inputs) = mpsc::channel();
    let shared = Arc::new(Listener::new(secret.clone(), "o/r".to_owned(), service));
    thread::spawn(move || listen::serve(listener, shared));

    let commit = "0123456789abcdef0123456789abcdef01234567";
    let body = format!(
        r#"{{"ref":"refs/heads/main","after":"{commit}","repository":{{"full_name":"o/r"}}}}"#
    );
    let signature = sign(secret.expose(), body.as_bytes());
    assert_eq!(deliver(at, "d1", &signature, body.as_bytes()).0, 202);
    assert_eq!(
        inputs.recv_timeout(Duration::from_secs(5)).unwrap(),
        Input::Event(Event::Push { branch: "main".to_owned(), commit: commit.to_owned() })
    );
    let (status, said) = deliver(at, "d1", &signature, body.as_bytes());
    assert_eq!(status, 202);
    assert!(said.contains("already"), "{said}");

    let (status, _) = deliver(at, "d2", &sign(b"another secret", body.as_bytes()), body.as_bytes());
    assert_eq!(status, 401);

    let other = body.replace("o/r", "x/y");
    let (status, said) = deliver(at, "d3", &sign(secret.expose(), other.as_bytes()), other.as_bytes());
    assert_eq!(status, 403);
    assert!(said.contains("x/y"), "{said}");
    assert!(inputs.recv_timeout(Duration::from_millis(200)).is_err(), "only the first was handed on");
}
