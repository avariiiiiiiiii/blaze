//! One script of effects on every driver: they must end the same.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use crate::{
    Done, Driver, Effect, FileName, Tag, run_until_idle,
    sim::{Faults, Sim},
    threads::Threads,
};

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn dir(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-core-{what}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// What became of each step, by its tag. A clone is either done or refused for the file
/// system, which the drivers on disk may differ in; it reads back the same either way.
fn script(driver: &mut dyn Driver) -> BTreeMap<u64, String> {
    let mut seen = BTreeMap::new();
    let mut effects = vec![Effect::Write {
        file: name("store/a/value"),
        bytes: b"compiled".to_vec(),
        tag: Tag(1),
    }];
    run_until_idle(driver, &mut effects, |done, effects| {
        let shown = match &done.done {
            Done::Failed(why) if done.tag == Tag(3) && why.0.contains("does not copy") => {
                "refused".to_owned()
            }
            Done::Cloned if done.tag == Tag(3) => "refused".to_owned(),
            Done::Stat(Some(meta)) if meta.dir => "stat a directory".to_owned(),
            Done::Stat(Some(meta)) => format!("stat {} bytes", meta.len),
            // A directory's own length is the file system's.
            Done::Listed(found) => found
                .iter()
                .map(|(file, meta)| match meta.dir {
                    true => format!("{file}/"),
                    false => format!("{file} {}", meta.len),
                })
                .collect::<Vec<_>>()
                .join(", "),
            Done::Failed(_) => "failed".to_owned(),
            other => format!("{other:?}"),
        };
        seen.insert(done.tag.0, shown);
        let next = match done.tag.0 {
            1 => Effect::Write { file: name("store/a/value"), bytes: b"again".to_vec(), tag: Tag(2) },
            2 => Effect::Clone { from: name("store/a/value"), to: name("out/c"), tag: Tag(3) },
            3 => Effect::Link { from: name("store/a/value"), to: name("out/l"), tag: Tag(4) },
            4 => Effect::ReadAll { file: name("out/l"), tag: Tag(5) },
            5 => Effect::Stat { file: name("store/a/value"), tag: Tag(6) },
            6 => Effect::Stat { file: name("store/a"), tag: Tag(7) },
            7 => Effect::Stat { file: name("missing"), tag: Tag(8) },
            8 => Effect::Remove { file: name("out/l"), tag: Tag(9) },
            9 => Effect::Remove { file: name("out/l"), tag: Tag(10) },
            10 => Effect::ReadAll { file: name("out/l"), tag: Tag(11) },
            11 => Effect::Append { file: name("store/a/value"), bytes: b"+".to_vec(), tag: Tag(12) },
            12 => Effect::ReadAll { file: name("store/a/value"), tag: Tag(13) },
            13 => Effect::Link { from: name("missing"), to: name("out/m"), tag: Tag(14) },
            14 => Effect::Unwatch { tag: Tag(99) },
            99 => Effect::WriteAt {
                file: name("store/a/value"),
                at: 4,
                bytes: b"XYZ".to_vec(),
                tag: Tag(16),
            },
            16 => Effect::ReadAll { file: name("store/a/value"), tag: Tag(17) },
            17 => Effect::Mode { file: name("store/a/value"), mode: 0o755, tag: Tag(18) },
            18 => Effect::Rename {
                from: name("store/a/value"),
                to: name("store/a/moved"),
                tag: Tag(19),
            },
            19 => Effect::Stat { file: name("store/a/value"), tag: Tag(20) },
            20 => Effect::List { dir: name("store"), tag: Tag(21) },
            21 => Effect::WriteAt { file: name("missing"), at: 0, bytes: vec![1], tag: Tag(22) },
            22 => Effect::Rename { from: name("missing"), to: name("out/m"), tag: Tag(23) },
            23 => Effect::List { dir: name("missing"), tag: Tag(24) },
            _ => return,
        };
        effects.push(next);
    });
    seen
}

#[test]
fn every_driver_ends_the_script_the_same() {
    let mut sim = Sim::new(1, Faults::default());
    let simulated = script(&mut sim);
    let root = dir("threads");
    let threads = script(&mut Threads::new(&root, 2));
    assert_eq!(simulated, threads);
    #[cfg(target_os = "macos")]
    {
        let root = dir("kqueue");
        let kqueue = script(&mut crate::kqueue::Kqueue::new(&root, 2).unwrap());
        assert_eq!(simulated, kqueue);
    }
    assert_eq!(simulated[&5], format!("{:?}", Done::Read(b"again".to_vec())));
    assert_eq!(simulated[&13], format!("{:?}", Done::Read(b"again+".to_vec())));
    assert_eq!(simulated[&8], format!("{:?}", Done::Stat(None)));
    assert_eq!(simulated[&7], "stat a directory");
    assert_eq!(simulated[&14], "failed");
    assert_eq!(simulated[&17], format!("{:?}", Done::Read(b"agaiXYZ".to_vec())));
    assert_eq!(simulated[&20], format!("{:?}", Done::Stat(None)));
    assert_eq!(simulated[&21], "store/a/, store/a/moved 7");
    assert_eq!((&simulated[&22][..], &simulated[&23][..], &simulated[&24][..]), ("failed", "failed", "failed"));
    #[cfg(target_os = "linux")]
    {
        let root = dir("uring");
        let ring = script(&mut crate::uring::Uring::new(&root).unwrap());
        assert_eq!(simulated, ring);
    }
}

/// A file written under a watched directory is heard of, on the drivers that watch.
#[cfg(target_os = "linux")]
#[test]
fn watches_hear_a_file_written_under_them() {
    let run = |driver: &mut dyn Driver, root: Option<&std::path::Path>| {
        let mut effects = vec![
            Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) },
        ];
        run_until_idle(driver, &mut effects, |_, _| {});
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(7) }]);
        let mut effects = vec![
            Effect::Write { file: name("src/deep/lib.rs"), bytes: b"fn f() {}".to_vec(), tag: Tag(2) },
        ];
        let mut heard = Vec::new();
        let mut asked_end = false;
        let mut ended = false;
        // A write by someone else, as an editor's.
        if let Some(root) = root {
            std::fs::write(root.join("src/keep"), b"edited").unwrap();
        }
        let mut done = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ended && std::time::Instant::now() < deadline {
            driver.submit(&mut effects);
            driver.poll(&mut done, Duration::from_millis(50));
            for it in done.drain(..) {
                match it.done {
                    Done::Changed { files, .. } => heard.extend(files),
                    Done::Unwatched => ended = true,
                    Done::Failed(why) => panic!("{why}"),
                    _ => {}
                }
            }
            if !asked_end && heard.contains(&name("src/deep/lib.rs")) {
                asked_end = true;
                effects.push(Effect::Unwatch { tag: Tag(7) });
            }
        }
        assert!(ended, "the watch never ended; heard {heard:?}");
        assert_eq!(driver.in_flight(), 0);
        heard
    };
    let root = dir("watch-threads");
    let heard = run(&mut Threads::new(&root, 2), Some(&root));
    assert!(heard.contains(&name("src/keep")), "{heard:?}");
    let root = dir("watch-uring");
    let heard = run(&mut crate::uring::Uring::new(&root).unwrap(), Some(&root));
    assert!(heard.contains(&name("src/keep")), "{heard:?}");
}

/// A file mapped, a process run with input, and a connection made, used and hung up: what
/// came of each, by step.
#[cfg(unix)]
fn processes_and_sockets(driver: &mut dyn Driver) -> Vec<String> {
    use crate::{Address, Command, Sandbox};
    let at = Address::Unix(name("run/daemon.sock"));
    let sh = Box::new(Command {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "cat; echo err >&2; exit 3".into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        dir: "/".into(),
        stdin: b"in".to_vec(),
        sandbox: Sandbox::Off,
        memory: None,
    });
    let mut seen = Vec::new();
    let mut effects = vec![
        Effect::Write { file: name("store/pack"), bytes: b"packed".to_vec(), tag: Tag(1) },
        Effect::Spawn { command: sh, tag: Tag(3) },
        Effect::Listen { at: at.clone(), tag: Tag(4) },
    ];
    let (mut client, mut server, mut got) = (None, None, Vec::new());
    let mut ended_after_hangup = false;
    let mut next = 100;
    run_until_idle(driver, &mut effects, |done, effects| {
        next += 1;
        match (done.tag.0, done.done) {
            (1, Done::Written) => {
                seen.push("written".to_owned());
                effects.push(Effect::Map { file: name("store/pack"), tag: Tag(2) });
                effects.push(Effect::Connect { at: at.clone(), tag: Tag(5) });
            }
            (2, Done::Mapped(it)) => seen.push(format!("mapped {:?}", it.bytes())),
            (3, Done::Exited(exit)) => seen.push(format!(
                "exited {:?} {:?} {:?} {:?}",
                exit.code,
                exit.signal,
                String::from_utf8_lossy(&exit.stdout),
                String::from_utf8_lossy(&exit.stderr)
            )),
            (4, Done::Accepted { conn }) => {
                server = Some(conn);
                effects.push(Effect::Recv { conn, most: 16, tag: Tag(next) });
            }
            (5, Done::Connected { conn }) => {
                client = Some(conn);
                effects.push(Effect::Send { conn, bytes: b"ping".to_vec(), tag: Tag(6) });
            }
            (6, Done::Sent) => seen.push("sent".to_owned()),
            (7, Done::HungUp) => seen.push("hung up".to_owned()),
            (4, Done::Unlistened) => seen.push("unlistened".to_owned()),
            (_, Done::Read(bytes)) if bytes.is_empty() => {
                ended_after_hangup = got.len() == 4;
                seen.push(format!("received {:?}, then the end", String::from_utf8_lossy(&got)));
                effects.push(Effect::Unlisten { tag: Tag(4) });
            }
            (_, Done::Read(bytes)) => {
                got.extend(bytes);
                let conn = server.expect("accepted");
                if got.len() == 4 {
                    effects.push(Effect::Hangup { conn: client.expect("connected"), tag: Tag(7) });
                }
                effects.push(Effect::Recv { conn, most: 16, tag: Tag(next) });
            }
            (tag, other) => seen.push(format!("{tag}: {other:?}")),
        }
    });
    assert!(ended_after_hangup, "{seen:?}");
    assert_eq!(driver.in_flight(), 0);
    seen.sort();
    seen
}

#[cfg(unix)]
#[test]
fn every_driver_runs_processes_and_sockets_the_same() {
    let mut sim = Sim::new(3, Faults::default());
    sim.program("/bin/sh", |command: &crate::Command, _: &mut crate::sim::Rng| crate::sim::Ran {
        exit: crate::Exit {
            code: Some(3),
            signal: None,
            stdout: command.stdin.clone(),
            stderr: b"err\n".to_vec(),
            traced: None,
            peak_memory: 0,
        },
        takes: Duration::from_millis(1),
    });
    let simulated = processes_and_sockets(&mut sim);
    assert_eq!(
        simulated,
        [
            "exited Some(3) None \"in\" \"err\\n\"",
            "hung up",
            "mapped [112, 97, 99, 107, 101, 100]",
            "received \"ping\", then the end",
            "sent",
            "unlistened",
            "written",
        ]
    );
    let root = dir("threads-processes");
    assert_eq!(simulated, processes_and_sockets(&mut Threads::new(&root, 2)));
    #[cfg(target_os = "linux")]
    {
        let root = dir("uring-processes");
        let ring = processes_and_sockets(&mut crate::uring::Uring::new(&root).unwrap());
        assert_eq!(simulated, ring);
    }
    #[cfg(target_os = "macos")]
    {
        let root = dir("kqueue-processes");
        let kqueue = processes_and_sockets(&mut crate::kqueue::Kqueue::new(&root, 2).unwrap());
        assert_eq!(simulated, kqueue);
    }
}

/// A sandbox asked for of a driver given nothing to make one is refused, and one given a
/// `Confine` runs what it says and hands back its record.
#[cfg(unix)]
#[test]
fn a_sandbox_is_made_by_the_confine_or_refused() {
    use std::sync::Arc;

    use crate::{Command, Confine, Confined, Sandbox, Traced};

    struct Shell;
    impl Confine for Shell {
        fn confine(&self, command: &Command, scratch: &std::path::Path) -> Result<Confined, String> {
            assert!(scratch.is_dir());
            let mut args = vec!["-c".into(), "printf confined; \"$0\" \"$@\"".into()];
            args.push(command.program.clone().into_os_string());
            args.extend(command.args.iter().cloned());
            Ok(Confined {
                program: "/bin/sh".into(),
                args,
                env: command.env.clone(),
                dir: command.dir.clone(),
            })
        }
        fn traced(&self, _: &Command, scratch: &std::path::Path) -> Result<Traced, String> {
            assert!(scratch.is_dir());
            let env = vec![("HOME".into(), None)];
            Ok(Traced { env, ..Traced::default() })
        }
    }
    let command = || {
        Box::new(Command {
            program: "/bin/echo".into(),
            args: vec![" ok".into()],
            env: Vec::new(),
            dir: "/".into(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: true },
            memory: None,
        })
    };
    let run = |driver: &mut dyn Driver| {
        let mut effects = vec![Effect::Spawn { command: command(), tag: Tag(1) }];
        let mut done = None;
        run_until_idle(driver, &mut effects, |it, _| done = Some(it.done));
        done.expect("the spawn ended")
    };
    let check = |root: &std::path::Path, refused: Done, confined: Done| {
        assert!(matches!(&refused, Done::Failed(why) if why.0.contains("Confine")), "{refused:?}");
        let Done::Exited(exit) = confined else { panic!("{confined:?}") };
        assert_eq!((exit.code, exit.stdout.as_slice()), (Some(0), &b"confined ok\n"[..]));
        assert_eq!(
            exit.traced.expect("traced").env,
            vec![(std::ffi::OsString::from("HOME"), None)],
        );
        assert!(!root.join(".blaze-spawn").read_dir().is_ok_and(|mut it| it.next().is_some()));
    };
    let root = dir("unconfined");
    check(
        &root,
        run(&mut Threads::new(&root, 1)),
        run(&mut Threads::new(&root, 1).confined(Arc::new(Shell))),
    );
    #[cfg(target_os = "linux")]
    {
        let root = dir("uring-unconfined");
        check(
            &root,
            run(&mut crate::uring::Uring::new(&root).unwrap()),
            run(&mut crate::uring::Uring::new(&root).unwrap().confined(Arc::new(Shell))),
        );
    }
    #[cfg(target_os = "macos")]
    {
        let root = dir("kqueue-unconfined");
        check(
            &root,
            run(&mut crate::kqueue::Kqueue::new(&root, 1).unwrap()),
            run(&mut crate::kqueue::Kqueue::new(&root, 1).unwrap().confined(Arc::new(Shell))),
        );
    }
}

#[test]
fn a_mapping_is_the_file() {
    let mut sim = Sim::new(7, Faults::default());
    sim.change(&name("store/pack"), b"packed");
    let mut effects = vec![Effect::Map { file: name("store/pack"), tag: Tag(1) }];
    let mut mapped = None;
    run_until_idle(&mut sim, &mut effects, |done, _| match done.done {
        Done::Mapped(it) => mapped = Some(it),
        other => panic!("{other:?}"),
    });
    assert_eq!(mapped.expect("mapped").bytes(), b"packed");
}
