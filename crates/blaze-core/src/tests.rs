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
    assert_eq!(simulated[&5], format!("{:?}", Done::Read(b"again".to_vec())));
    assert_eq!(simulated[&13], format!("{:?}", Done::Read(b"again+".to_vec())));
    assert_eq!(simulated[&8], format!("{:?}", Done::Stat(None)));
    assert_eq!(simulated[&7], "stat a directory");
    assert_eq!(simulated[&14], "failed");
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
