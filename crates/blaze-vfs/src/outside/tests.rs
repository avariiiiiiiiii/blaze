use std::{path::Path, time::Duration};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, IoError, Tag,
    sim::{Faults, Sim},
};

use super::{Dirs, FLUSH, Outside, STAT, UNCONFIRMED, WATCH};

const TAGS: u64 = 1 << 40;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn path(it: &str) -> &Path {
    Path::new(it)
}

/// Runs the simulation until nothing is left to complete, handing every completion to `outside`.
fn pump(sim: &mut Sim, outside: &mut Outside<'_>, effects: &mut Vec<Effect>) {
    let mut done = Vec::new();
    loop {
        sim.submit(effects);
        if sim.poll(&mut done, Duration::ZERO) == 0 {
            return;
        }
        for completion in done.drain(..) {
            assert!(outside.owns(completion.tag), "{completion:?}");
            outside.complete(completion, effects);
        }
    }
}

/// A round run to its end.
fn round(sim: &mut Sim, outside: &mut Outside<'_>, effects: &mut Vec<Effect>) {
    outside.flush(effects);
    pump(sim, outside, effects);
    assert!(outside.flushed());
}

/// A simulation with `tc/lib/a.h` on its disk and a table wanting `/tc`, watched and confirmed.
fn confirmed(dirs: &Dirs) -> (Sim, Outside<'_>, Vec<Effect>) {
    let mut sim = Sim::new(1, Faults::default());
    sim.change(&name("tc/lib/a.h"), b"int a;");
    dirs.want(path("/tc"));
    let mut outside = Outside::new(dirs, "/", TAGS);
    let mut effects = Vec::new();
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.dir(0).since() < UNCONFIRMED, "the watch is confirmed");
    (sim, outside, effects)
}

#[test]
fn a_confirmed_watch_vouches_for_what_is_under_it() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    // A round with nothing changed keeps it.
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    // A generation from before the watch was confirmed is not vouched for.
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), dirs.dir(0).since() - 1));
}

#[test]
fn a_change_reported_after_a_stamp_stops_vouching() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    sim.change(&name("tc/lib/a.h"), b"int b;");
    pump(&mut sim, &mut outside, &mut effects);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/tc/lib/other.h"), generation), "the whole directory");
    // Read after the change was heard, a generation is vouched for again.
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
    // A change elsewhere is not one under it.
    let generation = dirs.generation();
    sim.change(&name("elsewhere/b.h"), b"int c;");
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
}

#[test]
fn a_watch_not_yet_confirmed_never_vouches() {
    let dirs = Dirs::new();
    let mut sim = Sim::new(2, Faults::default());
    sim.change(&name("tc/lib/a.h"), b"int a;");
    dirs.want(path("/tc"));
    let mut outside = Outside::new(&dirs, "/", TAGS);
    let mut effects = Vec::new();
    outside.flush(&mut effects);
    assert!(matches!(effects[..], [Effect::Watch { .. }, Effect::Stat { .. }]), "{effects:?}");
    // Nothing of the round is a flush: it is in at once, and the watch is not yet confirmed.
    assert!(outside.flushed());
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), u64::MAX - 1));
    pump(&mut sim, &mut outside, &mut effects);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_directory_not_there_is_confirmed_once_it_is() {
    let dirs = Dirs::new();
    let mut sim = Sim::new(3, Faults::default());
    dirs.want(path("/tc"));
    let mut outside = Outside::new(&dirs, "/", TAGS);
    let mut effects = Vec::new();
    round(&mut sim, &mut outside, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED, "a stat of nothing confirms nothing");
    sim.change(&name("tc/a.h"), b"int a;");
    pump(&mut sim, &mut outside, &mut effects);
    assert!(dirs.dir(0).since() < UNCONFIRMED, "the change under it asks again");
    assert!(dirs.unchanged_since(path("/tc/a.h"), dirs.generation()));
}

#[test]
fn a_lost_event_stops_vouching() {
    let dirs = Dirs::new();
    let mut sim = Sim::new(4, Faults { lose_change_one_in: 1, ..Faults::default() });
    sim.change(&name("tc/lib/a.h"), b"int a;");
    dirs.want(path("/tc"));
    let mut outside = Outside::new(&dirs, "/", TAGS);
    let mut effects = Vec::new();
    round(&mut sim, &mut outside, &mut effects);
    let generation = dirs.generation();
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    sim.change(&name("tc/lib/a.h"), b"int b;");
    round(&mut sim, &mut outside, &mut effects);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/tc/x"), generation), "lost names nothing: all of it");
    // What was dropped may have been its own end: it is watched and confirmed again.
    assert!(dirs.dir(0).since() > generation);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn an_unwatch_stops_vouching_until_watched_again() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    outside.complete(Completion { tag: outside.tag(0, WATCH), done: Done::Unwatched }, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(effects.is_empty(), "made again by the next round, not at once: {effects:?}");
    round(&mut sim, &mut outside, &mut effects);
    assert!(dirs.dir(0).since() > generation, "confirmed again, after the break");
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_failed_watch_stops_vouching_and_is_asked_again_later() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    let failed = Done::Failed(IoError("injected".into()));
    outside.complete(Completion { tag: outside.tag(0, WATCH), done: failed }, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    // Backed off one round, then watched and confirmed again.
    for _ in 0..2 {
        round(&mut sim, &mut outside, &mut effects);
    }
    assert!(dirs.dir(0).since() > generation);
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_failed_flush_stops_vouching_and_ends_the_round() {
    let dirs = Dirs::new();
    let (_sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    outside.flush(&mut effects);
    let flush = outside.tag(0, FLUSH);
    assert_eq!(effects, vec![Effect::Flush { watch: outside.tag(0, WATCH), tag: flush }]);
    effects.clear();
    assert!(!outside.flushed());
    let failed = Done::Failed(IoError("injected".into()));
    outside.complete(Completion { tag: flush, done: failed }, &mut effects);
    assert!(outside.flushed());
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
}

#[test]
fn a_change_naming_the_directory_itself_watches_it_again() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    let replaced = Done::Changed { files: vec![name("tc")], lost: false };
    outside.complete(Completion { tag: outside.tag(0, WATCH), done: replaced }, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert_eq!(effects, vec![Effect::Unwatch { tag: outside.tag(0, WATCH) }]);
    pump(&mut sim, &mut outside, &mut effects);
    // Ended, then watched and stat'ed again at once.
    assert!(dirs.dir(0).since() > generation);
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
}

#[test]
fn a_change_made_before_a_flush_is_seen_once_it_completes() {
    for seed in 0..64 {
        let dirs = Dirs::new();
        let mut sim = Sim::new(seed, Faults { max_latency: Duration::from_millis(5), ..Faults::default() });
        sim.change(&name("tc/lib/a.h"), b"int a;");
        dirs.want(path("/tc"));
        let mut outside = Outside::new(&dirs, "/", TAGS);
        let mut effects = Vec::new();
        round(&mut sim, &mut outside, &mut effects);
        let generation = dirs.generation();
        // Changed on the disk; its event is still on its way.
        sim.change(&name("tc/lib/a.h"), b"int b;");
        outside.flush(&mut effects);
        assert!(!outside.flushed(), "seed {seed}");
        let mut done = Vec::new();
        while !outside.flushed() {
            sim.submit(&mut effects);
            assert!(sim.poll(&mut done, Duration::ZERO) > 0, "seed {seed}: the flush never came");
            for completion in done.drain(..) {
                outside.complete(completion, &mut effects);
            }
            assert!(
                outside.flushed() || !dirs.unchanged_since(path("/tc/lib/a.h"), generation),
                "seed {seed}: vouched while the round is out"
            );
        }
        assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation), "seed {seed}");
    }
}

#[test]
fn nothing_vouches_while_a_round_is_out() {
    let dirs = Dirs::new();
    let (mut sim, mut outside, mut effects) = confirmed(&dirs);
    let generation = dirs.generation();
    outside.flush(&mut effects);
    assert!(!outside.flushed());
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/tc/lib/a.h"), dirs.generation()));
    // A round asked while one is out: the flush still in flight covers less, and is asked again.
    sim.submit(&mut effects);
    outside.flush(&mut effects);
    assert!(effects.is_empty(), "one flush in flight at a time: {effects:?}");
    pump(&mut sim, &mut outside, &mut effects);
    assert!(outside.flushed());
    assert!(dirs.unchanged_since(path("/tc/lib/a.h"), generation));
}

#[test]
fn nothing_vouches_before_the_first_round_or_once_undriven() {
    let dirs = Dirs::new();
    dirs.want(path("/tc"));
    assert!(!dirs.unchanged_since(path("/tc/a.h"), dirs.generation()));
    let generation = {
        let (_sim, _outside, _effects) = confirmed(&dirs);
        let generation = dirs.generation();
        assert!(dirs.unchanged_since(path("/tc/a.h"), generation));
        generation
    };
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
    assert!(!dirs.unchanged_since(path("/tc/a.h"), generation));
}

#[test]
fn ancestors_are_found_and_only_whole_components() {
    let dirs = Dirs::new();
    let (_sim, _outside, _effects) = confirmed(&dirs);
    let generation = dirs.generation();
    for under in ["/tc", "/tc/", "/tc/lib", "/tc/lib/a.h", "/tc/lib/deep/er/x.h"] {
        assert!(dirs.unchanged_since(path(under), generation), "{under}");
    }
    for not in ["/tcx", "/tcx/a.h", "/t", "/", "/other/tc/a.h", ""] {
        assert!(!dirs.unchanged_since(path(not), generation), "{not}");
    }
}

#[test]
fn a_directory_the_root_cannot_name_is_never_watched() {
    let dirs = Dirs::new();
    dirs.want(path("/elsewhere/include"));
    dirs.want(path("/root/tc"));
    let mut sim = Sim::new(5, Faults::default());
    sim.change(&name("tc/a.h"), b"int a;");
    sim.change(&name("elsewhere/include/b.h"), b"int b;");
    let mut outside = Outside::new(&dirs, "/root", TAGS);
    let mut effects = Vec::new();
    outside.flush(&mut effects);
    assert_eq!(
        effects,
        vec![
            Effect::Watch { dir: name("tc"), tag: outside.tag(1, WATCH) },
            Effect::Stat { file: name("tc"), tag: outside.tag(1, STAT) },
        ]
    );
    pump(&mut sim, &mut outside, &mut effects);
    let generation = dirs.generation();
    assert!(dirs.unchanged_since(path("/root/tc/a.h"), generation));
    assert!(!dirs.unchanged_since(path("/elsewhere/include/b.h"), generation));
    round(&mut sim, &mut outside, &mut effects);
    assert_eq!(dirs.dir(0).since(), UNCONFIRMED);
}

#[test]
fn a_directory_is_wanted_once_and_found_past_growth() {
    let dirs = Dirs::new();
    dirs.want(path("/"));
    assert!(dirs.is_empty(), "the file system's root is not watched");
    dirs.want(path("/tc"));
    dirs.want(path("/tc/"));
    dirs.want(path("/tc/./"));
    assert_eq!(dirs.len(), 1);
    for at in 0..5000 {
        dirs.want(Path::new(&format!("/many/{at}")));
    }
    assert_eq!(dirs.len(), 5001);
    for at in 0..5000 {
        let wanted = format!("/many/{at}");
        let key = wanted.as_bytes();
        let dir = dirs.find(key, super::tag(key)).unwrap_or_else(|| panic!("{wanted}"));
        assert_eq!(dir.path(), Path::new(&wanted));
    }
    assert!(dirs.find(b"/many/5000", super::tag(b"/many/5000")).is_none());
    assert!(dirs.find(b"/tc", super::tag(b"/tc")).is_some());
}

#[cfg(not(blaze_check))] // many threads, long loops
#[test]
fn reads_race_with_wants_and_find_every_published_directory() {
    let dirs = Dirs::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for at in 0..2000 {
                dirs.want(Path::new(&format!("/race/{at}")));
            }
        });
        for _ in 0..2 {
            scope.spawn(|| {
                let mut seen = 0;
                while seen < 2000 {
                    let len = dirs.len();
                    for at in seen..len {
                        let wanted = format!("/race/{at}");
                        let key = wanted.as_bytes();
                        assert!(dirs.find(key, super::tag(key)).is_some(), "{wanted}");
                    }
                    seen = len;
                }
            });
        }
    });
}

/// A check on another thread while the driver adds a directory, confirms it and ends a round:
/// under every interleaving and every value a load may read, it finds the directory whole or
/// not at all, and vouches only for a generation read after the confirmation.
#[cfg(blaze_check)]
#[test]
fn model_a_check_sees_a_directory_whole_and_vouches_only_after_it_is_confirmed() {
    use std::sync::Arc;

    blaze_check::model(|| {
        let dirs = Arc::new(Dirs::new());
        let reader = {
            let dirs = dirs.clone();
            crate::sync::thread::spawn(move || {
                let generation = dirs.generation();
                if let Some(dir) = dirs.find(b"/tc", super::tag(b"/tc")) {
                    assert_eq!(dir.path(), Path::new("/tc"));
                }
                if dirs.unchanged_since(path("/tc/a.h"), generation) {
                    assert!(generation >= 1, "vouched for a generation before the confirmation");
                }
            })
        };
        dirs.want(path("/tc"));
        dirs.dir(0).since.store(dirs.bump(), crate::sync::atomic::Ordering::Release);
        dirs.settle(true);
        reader.join().unwrap();
    });
}

#[test]
fn the_tags_are_its_own() {
    let dirs = Dirs::new();
    let outside = Outside::new(&dirs, "/", TAGS);
    assert!(outside.owns(Tag(TAGS)));
    assert!(outside.owns(Tag(TAGS + super::TAGS - 1)));
    assert!(!outside.owns(Tag(TAGS - 1)));
    assert!(!outside.owns(Tag(TAGS + super::TAGS)));
}

#[test]
#[should_panic(expected = "another Outside drives this table")]
fn one_driver_per_table() {
    let dirs = Dirs::new();
    let _one = Outside::new(&dirs, "/", TAGS);
    let _two = Outside::new(&dirs, "/", 2 * TAGS);
}
