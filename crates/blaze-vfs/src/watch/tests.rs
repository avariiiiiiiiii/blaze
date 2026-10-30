//! The watcher under the simulation, as each system's journal behaves across a restart
//! (docs/v2/plan/lanes/frontend/journal.md): what changed while blaze was down is found from
//! the saved cursor and column, a lost journal is named and looked at again only where it was,
//! and no file whose stamp did not move, nor one that only moved, is read.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use blaze_core::{
    Completion, Cursor, Done, Driver, Effect, FileName, Journaled, Lost, Mapping,
    sim::{Faults, Journal, Rng, Sim},
};

use super::{Watched, Watcher};
use crate::{Mode, Vfs, disk::Disks, section::Section};

const KINDS: [Journal; 3] = [Journal::None, Journal::FsEvents, Journal::Usn];
const TAGS: u64 = 1 << 40;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn ws(path: &str) -> FileName {
    name(&format!("ws/{path}"))
}

/// The watcher, its head, the simulated machine, and what the watcher asked of it.
struct Run {
    sim: Sim,
    vfs: Vfs,
    watcher: Watcher,
    effects: Vec<Effect>,
    /// What the disk holds, as the test made it.
    disk: BTreeMap<String, Vec<u8>>,
    /// Every file read, and every name stamped, since the last start.
    reads: Vec<String>,
    stamped: Vec<String>,
    out: Vec<Watched>,
}

impl Run {
    fn new(seed: u64, kind: Journal, files: &[(&str, &str)]) -> Run {
        let mut sim = Sim::new(seed, Faults::default());
        sim.set_journal(kind);
        let mut run = Run {
            sim,
            vfs: Vfs::recover(name("state/journal"), 0, &[]).0,
            watcher: Watcher::new("disk", name("ws"), TAGS),
            effects: Vec::new(),
            disk: BTreeMap::new(),
            reads: Vec::new(),
            stamped: Vec::new(),
            out: Vec::new(),
        };
        for (path, bytes) in files {
            run.write(path, bytes);
        }
        run.watcher.start(&run.vfs, &mut run.effects);
        run.pump();
        run
    }

    fn write(&mut self, path: &str, bytes: &str) {
        self.sim.change(&ws(path), bytes.as_bytes());
        self.disk.insert(path.into(), bytes.as_bytes().to_vec());
    }

    fn delete(&mut self, path: &str) {
        self.sim.delete(&ws(path));
        self.disk.remove(path);
    }

    fn rename(&mut self, from: &str, to: &str) {
        self.sim.rename(&ws(from), &ws(to));
        let bytes = self.disk.remove(from).expect("renamed what is there");
        self.disk.insert(to.into(), bytes);
    }

    /// Runs everything in flight, handing each completion to whichever asked for it.
    fn pump(&mut self) {
        let mut done = Vec::new();
        loop {
            for effect in &self.effects {
                match effect {
                    Effect::ReadAll { file, .. } => self.reads.push(file.as_str()["ws/".len()..].into()),
                    Effect::Stamp { files, .. } => self.stamped.extend(files.iter().map(str::to_owned)),
                    _ => {}
                }
            }
            self.sim.submit(&mut self.effects);
            if self.sim.poll(&mut done, Duration::ZERO) == 0 && self.effects.is_empty() {
                return;
            }
            for completion in done.drain(..) {
                if self.vfs.owns(completion.tag) {
                    self.vfs.complete(completion, &mut self.effects);
                } else {
                    assert!(self.watcher.owns(completion.tag), "{completion:?}");
                    let out = self.watcher.complete(&mut self.vfs, completion, &mut self.effects);
                    self.out.extend(out);
                }
            }
        }
    }

    /// blaze stops: the machine keeps its disk and journal, the watch is gone, and the column
    /// goes through the image's bytes (encoded, then read in place). `down` changes the disk
    /// while nothing watches; then a new watcher starts from what was saved.
    fn restart(&mut self, down: impl FnOnce(&mut Run)) {
        assert!(self.watcher.idle(), "stopped with looks in flight: {:?}", self.out);
        self.sim.crash();
        down(self);
        let mut bytes = Vec::new();
        self.vfs.disks.encode(&mut bytes);
        let len = bytes.len();
        let section = Section::new(Mapping(Arc::new(bytes)), 0..len).expect("a section");
        self.vfs.disks = Disks::open(section).expect("the column reopens");
        self.watcher = Watcher::new("disk", name("ws"), TAGS);
        (self.reads.clear(), self.stamped.clear(), self.out.clear());
        self.watcher.start(&self.vfs, &mut self.effects);
        self.pump();
    }

    fn head(&self) -> BTreeMap<String, Vec<u8>> {
        let Some(id) = self.vfs.head("disk") else { return BTreeMap::new() };
        let tree = &self.vfs.tree;
        let root = self.vfs.root(id).expect("a head's root");
        tree.files(root, Mode::Regular, "")
            .into_iter()
            .map(|(path, node, _)| {
                (path, tree.bytes(node).expect("not corrupt").expect("a file").to_vec())
            })
            .collect()
    }

    fn reads(&self) -> BTreeSet<&str> {
        self.reads.iter().map(String::as_str).collect()
    }

    fn lost(&self) -> Vec<(&str, Lost)> {
        let lost = self.out.iter().filter_map(|it| match it {
            Watched::Lost { path, why } => Some((path.as_str(), *why)),
            _ => None,
        });
        lost.collect()
    }

    fn check(&self, what: &str) {
        assert_eq!(self.head(), self.disk, "{what}: the head is the disk");
        assert!(self.watcher.idle(), "{what}: {:?}", self.out);
        let odd = self.out.iter().find(|it| {
            !matches!(it, Watched::Committed(_) | Watched::Scanned | Watched::Lost { .. })
        });
        assert!(odd.is_none(), "{what}: {odd:?} in {:?}", self.out);
    }
}

const TREE: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]"),
    ("src/lib.rs", "pub mod a;"),
    ("src/a.rs", "pub fn a() {}"),
    ("src/old/one.rs", "1"),
    ("src/old/two.rs", "2"),
    ("src/old/deep/three.rs", "3"),
    ("tests/t.rs", "#[test] fn t() {}"),
    ("README", "read me"),
];

#[test]
fn a_first_start_reads_the_directory_once() {
    for kind in KINDS {
        let run = Run::new(1, kind, TREE);
        run.check(&format!("{kind:?}"));
        let all: BTreeSet<&str> = TREE.iter().map(|it| it.0).collect();
        assert_eq!(run.reads(), all, "{kind:?}");
        assert!(run.out.contains(&Watched::Scanned), "{kind:?}");
    }
}

#[test]
fn a_restart_with_nothing_changed_reads_and_commits_nothing() {
    for kind in KINDS {
        for seed in 0..20 {
            let mut run = Run::new(seed, kind, TREE);
            let head = run.vfs.head("disk");
            run.restart(|_| {});
            let what = format!("{kind:?} seed {seed}");
            run.check(&what);
            assert_eq!(run.reads, Vec::<String>::new(), "{what}");
            assert_eq!(run.vfs.head("disk"), head, "{what}");
            assert!(!run.out.iter().any(|it| matches!(it, Watched::Committed(_))), "{what}");
            match kind {
                // No journal: one sweep of stamps, of every tracked name and nothing else.
                Journal::None => {
                    let swept: BTreeSet<&str> = run.stamped.iter().map(String::as_str).collect();
                    assert!(swept.contains("ws/src/old/deep/three.rs"), "{what}: {swept:?}");
                    assert!(swept.iter().all(|it| *it == "ws" || it.starts_with("ws/")), "{what}");
                    assert_eq!(swept.len(), run.stamped.len(), "{what}: each name once");
                }
                // A journal: nothing is looked at.
                _ => assert_eq!(run.stamped, Vec::<String>::new(), "{what}"),
            }
        }
    }
}

#[test]
fn what_changed_while_down_is_read_and_what_moved_is_not() {
    for kind in KINDS {
        for seed in 0..20 {
            let mut run = Run::new(seed, kind, TREE);
            run.restart(|run| {
                run.write("src/a.rs", "pub fn a() { changed() }");
                run.write("src/new/b.rs", "pub fn b() {}");
                run.delete("tests/t.rs");
                run.rename("README", "README.md");
                // A directory renamed: each of its files keeps its inode and mtime.
                run.rename("src/old/one.rs", "src/moved/one.rs");
                run.rename("src/old/two.rs", "src/moved/two.rs");
                run.rename("src/old/deep/three.rs", "src/moved/deep/three.rs");
                run.sim.chmod(&ws("Cargo.toml"), 0o600);
            });
            let what = format!("{kind:?} seed {seed}");
            run.check(&what);
            // A chmod moves the stamp, so the file is read; its bytes are the head's, so it
            // makes no op.
            let want = BTreeSet::from(["src/a.rs", "src/new/b.rs", "Cargo.toml"]);
            assert_eq!(run.reads(), want, "{what}");
            assert!(run.lost().is_empty(), "{what}: {:?}", run.lost());
            // A journal names what changed: nothing it did not name is stamped.
            if kind != Journal::None {
                assert!(!run.stamped.iter().any(|it| it == "ws/src/lib.rs"), "{what}: {:?}", run.stamped);
            }
            let commits = run.out.iter().filter(|it| matches!(it, Watched::Committed(_))).count();
            assert_eq!(commits, 1, "{what}: what changed while down is one transaction");
        }
    }
}

#[test]
fn a_file_renamed_over_another_is_not_read() {
    for kind in KINDS {
        let mut run = Run::new(3, kind, TREE);
        run.restart(|run| run.rename("src/a.rs", "src/lib.rs"));
        let what = format!("{kind:?}");
        run.check(&what);
        assert_eq!(run.reads, Vec::<String>::new(), "{what}");
        assert_eq!(run.head()["src/lib.rs"], b"pub fn a() {}", "{what}");
    }
}

#[test]
fn a_lost_journal_is_named_and_only_its_subtree_is_swept() {
    for kind in [Journal::FsEvents, Journal::Usn] {
        for seed in 0..10 {
            let what = format!("{kind:?} seed {seed}");
            // The journal reset (FSEvents' store made again, the USN journal deleted): the
            // whole directory is swept by stamps, and only what changed is read.
            let mut run = Run::new(seed, kind, TREE);
            run.restart(|run| {
                run.write("src/lib.rs", "pub mod a; pub mod b;");
                run.sim.reset_journal();
            });
            run.check(&what);
            assert_eq!(run.lost(), [("", Lost::Reset)], "{what}");
            assert_eq!(run.reads(), BTreeSet::from(["src/lib.rs"]), "{what}");

            // The journal no longer reaches the cursor.
            run.restart(|run| {
                run.delete("src/a.rs");
                run.sim.truncate_journal();
                run.write("README", "read me again");
            });
            run.check(&what);
            assert_eq!(run.lost(), [("", Lost::Truncated)], "{what}");
            assert_eq!(run.reads(), BTreeSet::from(["README"]), "{what}");

            // The system says to look at one subtree again: nothing outside it is stamped.
            run.restart(|run| {
                run.write("src/old/deep/three.rs", "three");
                run.sim.lose_changes(&ws("src/old"), Lost::Rescan);
            });
            run.check(&what);
            assert!(run.lost().contains(&("src/old", Lost::Rescan)), "{what}: {:?}", run.lost());
            assert_eq!(run.reads(), BTreeSet::from(["src/old/deep/three.rs"]), "{what}");
            let outside = run.stamped.iter().find(|it| !it.starts_with("ws/src/old"));
            assert_eq!(outside, None, "{what}: stamped outside the lost subtree");
        }
    }
}

#[test]
fn with_no_journal_a_saved_cursor_is_named_lost_and_swept() {
    let mut run = Run::new(5, Journal::FsEvents, TREE);
    // The same disk, now on a system with no journal across a restart.
    run.restart(|run| {
        run.sim.set_journal(Journal::None);
        run.write("src/a.rs", "pub fn a() { linux() }");
    });
    run.check("none");
    assert_eq!(run.lost(), [("", Lost::NoJournal)]);
    assert_eq!(run.reads(), BTreeSet::from(["src/a.rs"]));
}

/// [`Lost::Unnamed`], as NTFS's unprivileged read tells it: the directory alone is listed and
/// diffed against what was in it. Nothing is stamped; only the new file is read, and the
/// renamed one is moved.
#[test]
fn an_unnamed_change_lists_its_directory_alone() {
    let mut run = Run::new(7, Journal::Usn, TREE);
    let cursor = run.vfs.disks.get("disk").expect("a disk").cursor;
    run.watcher.stop(&mut run.effects);
    run.pump();
    assert!(run.out.contains(&Watched::Ended(None)));
    (run.reads.clear(), run.stamped.clear(), run.out.clear());
    // Changes no watch hears.
    run.delete("src/lib.rs");
    run.write("src/c.rs", "pub fn c() {}");
    run.rename("src/a.rs", "src/b.rs");
    run.delete("src/old/one.rs");
    run.delete("src/old/two.rs");
    run.delete("src/old/deep/three.rs");
    let told = Journaled {
        files: Vec::new(),
        lost: vec![(ws("src"), Lost::Unnamed)],
        cursor,
        caught_up: true,
    };
    let journal = Completion { tag: blaze_core::Tag(TAGS), done: Done::Journaled(Box::new(told)) };
    let out = run.watcher.complete(&mut run.vfs, journal, &mut run.effects);
    run.out.extend(out);
    assert!(
        matches!(&run.effects[..], [Effect::Entries { dir, .. }] if *dir == ws("src")),
        "{:?}",
        run.effects
    );
    run.pump();
    run.check("unnamed");
    assert_eq!(run.lost(), [("src", Lost::Unnamed)]);
    assert_eq!(run.stamped, Vec::<String>::new());
    assert_eq!(run.reads(), BTreeSet::from(["src/c.rs"]));
    assert!(!run.head().keys().any(|it| it.starts_with("src/old")));
}

#[test]
fn a_cursor_is_saved_only_with_what_it_covers() {
    for kind in [Journal::FsEvents, Journal::Usn] {
        let mut run = Run::new(9, kind, TREE);
        let first = run.vfs.disks.get("disk").expect("a disk").cursor;
        assert!(matches!(first, Cursor::Sim { .. }), "{kind:?}: {first:?}");
        run.write("src/live.rs", "live");
        run.pump();
        run.check(&format!("{kind:?} live"));
        let after = run.vfs.disks.get("disk").expect("a disk").cursor;
        assert_ne!(after, first, "{kind:?}: a committed change moves the cursor");
        // A restart from it hears nothing again.
        run.restart(|_| {});
        assert_eq!(run.reads, Vec::<String>::new(), "{kind:?}");
    }
}

/// Random edits, deletes, renames and chmods while blaze is down, for each system, over many
/// seeds and several restarts: the head is always the disk, and a file is read only when it
/// was written or chmodded while down, or renamed onto a path that had a file.
#[test]
fn the_head_follows_the_disk_across_restarts() {
    const PATHS: &[&str] = &["a", "b", "d/x", "d/y", "d/e/z", "f/g/h", "k"];
    for kind in KINDS {
        for seed in 0..100 {
            let mut rng = Rng::new(seed ^ 0x6a75_726e);
            let mut run = Run::new(seed, kind, &[("keep", "k"), ("d/x", "x")]);
            for round in 0..4 {
                let mut touched = BTreeSet::new();
                let mut steps = Vec::new();
                let mut disk = run.disk.clone();
                for step in 0..1 + rng.below(8) {
                    let path = PATHS[rng.below(PATHS.len() as u64) as usize];
                    let there = disk.contains_key(path);
                    let free = PATHS.iter().find(|it| {
                        !disk.contains_key(**it)
                            && !disk.keys().any(|k| k.starts_with(&format!("{it}/")) || it.starts_with(&format!("{k}/")))
                    });
                    match rng.below(4) {
                        0 if there => {
                            disk.remove(path);
                            steps.push((0, path.to_owned(), String::new()));
                        }
                        1 if there && free.is_some() => {
                            let to = free.expect("checked").to_string();
                            let bytes = disk.remove(path).expect("there");
                            disk.insert(to.clone(), bytes);
                            // A file written or chmodded before its rename is read where it went;
                            // so is one renamed onto a path that had a file at the start (its
                            // change time moved, as it does for a file moved away and back).
                            if touched.remove(path) || run.disk.contains_key(&to) {
                                touched.insert(to.clone());
                            }
                            steps.push((1, path.to_owned(), to));
                        }
                        2 if there => {
                            touched.insert(path.to_owned());
                            steps.push((2, path.to_owned(), String::new()));
                        }
                        _ if disk.keys().any(|k| path.starts_with(&format!("{k}/")) || k.starts_with(&format!("{path}/"))) => {}
                        _ => {
                            let bytes = format!("{seed}:{round}:{step}");
                            disk.insert(path.into(), bytes.clone().into_bytes());
                            touched.insert(path.to_owned());
                            steps.push((3, path.to_owned(), bytes));
                        }
                    }
                }
                run.restart(|run| {
                    for (op, path, arg) in steps {
                        match op {
                            0 => run.delete(&path),
                            1 => run.rename(&path, &arg),
                            2 => run.sim.chmod(&ws(&path), 0o600 + (round as u32 % 2)),
                            _ => run.write(&path, &arg),
                        }
                    }
                });
                let what = format!("{kind:?} seed {seed} round {round}");
                run.check(&what);
                let extra: Vec<&str> =
                    run.reads().into_iter().filter(|it| !touched.contains(*it)).collect();
                assert_eq!(extra, Vec::<&str>::new(), "{what}: read but not changed");
            }
        }
    }
}
