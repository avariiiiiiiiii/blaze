//! The mirror as an agent reads it: events in, Flecs REST queries out.

use std::{
    io::{Read as _, Write as _},
    net::TcpStream,
    time::{Duration, Instant},
};

use blaze_monitor::{
    Listen, Monitor,
    event::{Build, By, Compiled, Event, Live, Outcome, Picked, Test, Tests},
};

fn get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.split_once("\r\n\r\n").map_or(response.clone(), |it| it.1.to_owned())
}

/// Asks until the answer has `wanted`: the world applies events on its own thread.
fn until(port: u16, path: &str, wanted: &str) -> String {
    let asked = Instant::now();
    loop {
        let body = get(port, path);
        if body.contains(wanted) || asked.elapsed() > Duration::from_secs(10) {
            return body;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn events_are_entities_rest_serves() {
    let monitor = Monitor::start(Listen::Scan).unwrap();
    let port = monitor.port.expect("a free port");
    let at = 1_000_000_000_000;
    monitor.send(Event::Compiled(Compiled {
        workspace: "/w".to_owned(),
        binary: "/w/target/debug/app".to_owned(),
        krate: "app".to_owned(),
        kind: "patch".to_owned(),
        functions: 1,
        files: 1,
        why: String::new(),
        at_us: at + 500,
        took_us: 400,
    }));
    monitor.send(Event::Build(Build {
        workspace: "/w".to_owned(),
        command: "test".to_owned(),
        by: By::Daemon,
        why: String::new(),
        succeeded: true,
        pid: 7,
        at_us: at,
        took_us: 900,
    }));
    monitor.send(Event::Live(Live {
        executable: "/w/target/debug/app".to_owned(),
        pid: 4242,
        sequence: 1,
        outcome: "sent".to_owned(),
        why: String::new(),
        functions: 1,
        statics: 0,
        at_us: at + 600,
        took_us: 600,
    }));
    let test = |name: &str, picked, why: &str, outcome| Test {
        name: name.to_owned(),
        picked,
        why: why.to_owned(),
        path: String::new(),
        outcome,
        ended_us: 30,
    };
    monitor.send(Event::Tests(Tests {
        workspace: "/w".to_owned(),
        package: "app".to_owned(),
        kind: "lib".to_owned(),
        executable: "/w/target/debug/deps/app-1f2e".to_owned(),
        selection: "affected".to_owned(),
        total: 3,
        status: 101,
        at_us: at + 1000,
        took_us: 50,
        tests: vec![
            test("tests::adds", Picked::Changed, "", Outcome::Failed),
            test(
                "tests::reads_a_file",
                Picked::Always,
                "it reaches `std::fs::read`",
                Outcome::Passed,
            ),
            test("tests::untouched", Picked::Skipped, "", Outcome::NotRun),
        ],
    }));

    let failed = until(port, "/query?expr=blaze.Test%2Cblaze.Failed", "adds");
    assert!(failed.contains("\"name\":\"adds\""), "{failed}");
    assert!(!failed.contains("reads_a_file"), "{failed}");
    let always = until(port, "/query?expr=blaze.Test%2Cblaze.Always", "reads_a_file");
    assert!(always.contains("std::fs::read"), "{always}");
    let patch = until(port, "/query?expr=blaze.Patch%2C(blaze.Of%2C%24b)", "\"b\"");
    assert!(patch.contains("builds.b2"), "the patch is of the build that came after it: {patch}");
    let live = until(port, "/query?expr=blaze.LivePatch%2C(blaze.PatchedInto%2C%24p)", "4242");
    assert!(live.contains("processes.4242"), "{live}");
    let totals = until(port, "/entity/blaze/Totals", "live_sent");
    assert!(totals.contains("\"tests_failed\":1"), "{totals}");
    assert!(totals.contains("\"tests_skipped\":1"), "{totals}");
}
