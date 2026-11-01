//! The maintainer hook: what a maintainer sees of a write, a batch's one pass, a maintainer's
//! own writes, its state across writes and after a panic.
//!
//! Two test maintainers are linked into base-db's tests. They do nothing unless this thread's
//! probe is on, so the other tests' databases don't see them.

use std::{cell::RefCell, panic};

use blaze_dbsp::Work;
use blaze_query::db::Durability;
use span::Edition;
use triomphe::Arc;
use crate::{AbsPathBuf, FileId, FileSet, VfsPath};

use crate::{
    CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, Db, Env, SourceRoot, SourceRootId,
    TargetKind,
    maintain::{Maintained, Maintainer, Written, maintainers},
};

/// What a maintainer saw of one pass, owned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Seen {
    set: Vec<FileId>,
    added: Vec<FileId>,
    removed: Vec<FileId>,
    roots: Vec<SourceRootId>,
    crates: bool,
    included: usize,
}

impl Seen {
    fn of(written: &Written<'_>) -> Seen {
        Seen {
            set: written.set.to_vec(),
            added: written.added.to_vec(),
            removed: written.removed.to_vec(),
            roots: written.roots.to_vec(),
            crates: written.crates,
            included: written.included.len(),
        }
    }

    fn set(files: &[FileId]) -> Seen {
        Seen { set: files.to_vec(), ..Seen::default() }
    }
}

#[derive(Default)]
struct Probe {
    on: bool,
    /// What `first` saw, a pass each.
    seen: Vec<Seen>,
    /// The maintainers in the order they ran.
    ran: Vec<&'static str>,
    /// A file `first` writes itself in each run.
    own_write: Option<FileId>,
    /// The run of `first` (from 1) that panics.
    panic_on: Option<u32>,
    /// Whether `second` reports work for the crates of the graph.
    report: bool,
}

thread_local! {
    static PROBE: RefCell<Probe> = RefCell::new(Probe::default());
}

/// `first`'s state: how many times it ran since it was made.
#[derive(Default)]
struct Runs(u32);

/// `second`'s state.
#[derive(Default)]
struct Second(u32);

fn first(db: &mut Db, written: &Written<'_>) {
    let Some((own_write, panic_on)) = PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        probe.on.then(|| {
            probe.seen.push(Seen::of(written));
            probe.ran.push("first");
            (probe.own_write, probe.panic_on)
        })
    }) else {
        return;
    };
    let run = db.with_state(|_, runs: &mut Runs| {
        runs.0 += 1;
        runs.0
    });
    if let Some(file) = own_write {
        db.set_file_text(file, &format!("// written by a maintainer, run {run}\n"));
    }
    if panic_on == Some(run) {
        panic!("the test maintainer panics in run {run}");
    }
}

fn second(db: &mut Db, _: &Written<'_>) {
    let Some(report) = PROBE.with(|probe| {
        let mut probe = probe.borrow_mut();
        probe.on.then(|| {
            probe.ran.push("second");
            probe.report
        })
    }) else {
        return;
    };
    db.with_state(|_, second: &mut Second| second.0 += 1);
    if report {
        for krate in crate::all_crates(db).iter().copied() {
            db.report_work("test-second", krate, Work { ops: 1, slices: 2, ..Work::default() });
        }
    }
}

inventory::submit! { Maintainer { order: 20, name: "test-second", run: second } }
inventory::submit! { Maintainer { order: 10, name: "test-first", run: first } }

/// Runs `f` with this thread's probe on, and returns what `first` saw and the order the
/// maintainers ran in.
fn probed(own_write: Option<FileId>, panic_on: Option<u32>, f: impl FnOnce()) -> Probe {
    PROBE.with(|probe| {
        *probe.borrow_mut() = Probe { on: true, own_write, panic_on, ..Probe::default() };
    });
    let ran = panic::catch_unwind(panic::AssertUnwindSafe(f));
    let probe = PROBE.with(|probe| std::mem::take(&mut *probe.borrow_mut()));
    if let Err(panic) = ran {
        panic::resume_unwind(panic);
    }
    probe
}

fn file(n: u32) -> FileId {
    FileId::from_raw(n)
}

#[test]
fn maintainers_run_in_order() {
    let names: Vec<&str> = maintainers().iter().map(|it| it.name).collect();
    let at = |name| names.iter().position(|it| *it == name).unwrap();
    assert!(at("test-first") < at("test-second"), "{names:?}");
    let mut db = Db::for_tests();
    let probe = probed(None, None, || db.set_file_text(file(0), "fn a() {}\n"));
    assert_eq!(probe.ran, vec!["first", "second"]);
}

#[test]
fn a_write_runs_each_maintainer_once() {
    let mut db = Db::for_tests();
    let probe = probed(None, None, || db.set_file_text(file(0), "fn a() {}\n"));
    assert_eq!(probe.seen, vec![Seen::set(&[file(0)])]);
    // The same text again changes nothing, and runs nothing.
    let probe = probed(None, None, || db.set_file_text(file(0), "fn a() {}\n"));
    assert!(probe.seen.is_empty(), "{:?}", probe.seen);
}

#[test]
fn a_batch_runs_each_maintainer_once_over_all_it_wrote() {
    let mut db = Db::for_tests();
    let probe = probed(None, None, || {
        db.batch(|db| {
            db.set_file_text(file(2), "fn b() {}\n");
            db.set_file_text(file(1), "fn a() {}\n");
            db.batch(|db| db.set_file_text(file(2), "fn c() {}\n"));
            db.set_file_text_with_durability(file(3), "", Durability::LOW);
        })
    });
    assert_eq!(probe.seen, vec![Seen::set(&[file(1), file(2), file(3)])]);
    assert_eq!(probe.ran, vec!["first", "second"]);
}

#[test]
fn a_maintainers_own_writes_do_not_recurse() {
    let mut db = Db::for_tests();
    let probe = probed(Some(file(9)), None, || {
        db.set_file_text(file(0), "fn a() {}\n");
        db.set_file_text(file(0), "fn b() {}\n");
    });
    // One pass per outside write; the maintainer's own write of file 9 is in neither.
    assert_eq!(probe.seen, vec![Seen::set(&[file(0)]), Seen::set(&[file(0)])]);
    assert!(db.file_text(file(9)).text(&db).contains("run 2"));
}

#[test]
fn state_survives_across_writes() {
    let mut db = Db::for_tests();
    probed(None, None, || {
        for n in 0..3 {
            db.set_file_text(file(0), &format!("fn f{n}() {{}}\n"));
        }
    });
    assert_eq!(db.with_state(|_, runs: &mut Runs| runs.0), 3);
    assert_eq!(db.with_state(|_, second: &mut Second| second.0), 3);
}

#[test]
fn a_panic_in_a_maintainer_drops_the_state() {
    let mut db = Db::for_tests();
    let ran = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        probed(None, Some(2), || {
            db.set_file_text(file(0), "fn a() {}\n");
            db.set_file_text(file(0), "fn b() {}\n");
        })
    }));
    assert!(ran.is_err(), "the maintainer's panic goes on out of the write");
    // Every maintainer's state is gone: the next write maintains from empty, re-checking all.
    assert_eq!(db.with_state(|_, runs: &mut Runs| runs.0), 0);
    assert_eq!(db.with_state(|_, second: &mut Second| second.0), 0);
    let probe = probed(None, None, || db.set_file_text(file(1), "fn c() {}\n"));
    assert_eq!(probe.seen, vec![Seen { set: vec![file(1)], crates: true, ..Seen::default() }]);
    assert_eq!(db.with_state(|_, runs: &mut Runs| runs.0), 1);
}

#[test]
fn a_panic_in_with_state_drops_that_state() {
    let mut db = Db::for_tests();
    db.with_state(|_, runs: &mut Runs| runs.0 = 7);
    let ran = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        db.with_state(|_, runs: &mut Runs| {
            runs.0 += 1;
            panic!("in the state's call");
        })
    }));
    assert!(ran.is_err());
    assert_eq!(db.with_state(|_, runs: &mut Runs| runs.0), 0);
}

#[test]
#[should_panic(expected = "inside a call that has it out")]
fn a_state_is_out_once() {
    let mut db = Db::for_tests();
    db.with_state(|db, _: &mut Runs| db.with_state(|_, _: &mut Runs| ()));
}

#[test]
fn a_warm_state_lookup_allocates_nothing() {
    let mut db = Db::for_tests();
    db.with_state(|_, runs: &mut Runs| runs.0 += 1);
    let (_, used) = blaze_alloc_count::measure(|| db.with_state(|_, runs: &mut Runs| runs.0 += 1));
    assert_eq!(used.allocations, 0, "with_state of a state that exists: {used:?}");
}

#[test]
fn a_source_root_notes_its_files_coming_and_going() {
    let mut db = Db::for_tests();
    for n in 0..3 {
        db.set_file_text(file(n), "fn f() {}\n");
    }
    let root = |files: &[u32]| {
        let mut set = FileSet::default();
        for &n in files {
            set.insert(file(n), VfsPath::new_virtual_path(format!("/maintain/f{n}.rs")));
        }
        Arc::new(SourceRoot::new_local(set))
    };
    let probe = probed(None, None, || {
        db.set_source_root_with_durability(SourceRootId(0), root(&[0, 1]), Durability::MEDIUM)
    });
    assert_eq!(
        probe.seen,
        vec![Seen { added: vec![file(0), file(1)], roots: vec![SourceRootId(0)], ..Seen::default() }]
    );
    let probe = probed(None, None, || {
        db.set_source_root_with_durability(SourceRootId(0), root(&[1, 2]), Durability::MEDIUM)
    });
    assert_eq!(
        probe.seen,
        vec![Seen {
            added: vec![file(2)],
            removed: vec![file(0)],
            roots: vec![SourceRootId(0)],
            ..Seen::default()
        }]
    );
}

/// A source root of files 0 and 1, and a crate graph of one library rooted at file 0.
fn one_crate(db: &mut Db) -> CrateGraphBuilder {
    let mut set = FileSet::default();
    for n in 0..2 {
        db.set_file_text(file(n), "fn f() {}\n");
        db.set_file_source_root_with_durability(file(n), SourceRootId(0), Durability::MEDIUM);
        set.insert(file(n), VfsPath::new_virtual_path(format!("/maintain/c{n}.rs")));
    }
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
    let mut graph = CrateGraphBuilder::default();
    graph.add_crate_root(
        file(0),
        Edition::Edition2021,
        None,
        None,
        Default::default(),
        None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in maintain tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    );
    graph
}

#[test]
fn the_crate_graph_is_a_write_when_it_changes() {
    let mut db = Db::for_tests();
    let graph = one_crate(&mut db);
    let probe = probed(None, None, || _ = graph.clone().set_in_db(&mut db));
    assert_eq!(probe.seen, vec![Seen { crates: true, ..Seen::default() }]);
    let probe = probed(None, None, || _ = graph.set_in_db(&mut db));
    assert!(probe.seen.is_empty(), "an equal graph is no write: {:?}", probe.seen);
    let krate = crate::all_crates(&db)[0];
    let probe = probed(None, None, || krate.set_env(&mut db, Env::from_iter([("A".to_owned(), "1".to_owned())])));
    assert_eq!(probe.seen, vec![Seen { crates: true, ..Seen::default() }]);
    let probe = probed(None, None, || db.note_crates_changed());
    assert_eq!(probe.seen, vec![Seen { crates: true, ..Seen::default() }]);
}

#[test]
fn work_is_kept_per_pass_while_asked_for() {
    let mut db = Db::for_tests();
    let graph = one_crate(&mut db);
    graph.set_in_db(&mut db);
    let krate = crate::all_crates(&db)[0];
    let reporting = |f: &mut dyn FnMut()| {
        PROBE.with(|probe| {
            *probe.borrow_mut() = Probe { on: true, report: true, ..Probe::default() };
        });
        f();
        PROBE.with(|probe| probe.borrow_mut().on = false);
    };
    reporting(&mut || db.set_file_text(file(1), "fn g() {}\n"));
    let mut out = Vec::new();
    db.take_maintained(&mut out);
    assert!(out.is_empty(), "nothing is kept until asked for");
    db.keep_maintained(true);
    reporting(&mut || db.set_file_text(file(1), "fn h() {}\n"));
    reporting(&mut || {
        db.batch(|db| {
            db.set_file_text(file(1), "fn i() {}\n");
            db.set_file_text(file(0), "fn j() {}\n");
        })
    });
    let work = Work { ops: 1, slices: 2, ..Work::default() };
    db.take_maintained(&mut out);
    assert_eq!(
        out,
        vec![
            Maintained { write: 0, maintainer: "test-second", krate, work },
            Maintained { write: 1, maintainer: "test-second", krate, work },
        ]
    );
    out.clear();
    reporting(&mut || db.set_file_text(file(1), "fn k() {}\n"));
    db.take_maintained(&mut out);
    assert_eq!(out, vec![Maintained { write: 0, maintainer: "test-second", krate, work }]);
}
