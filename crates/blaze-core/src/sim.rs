//! A deterministic [`Driver`]: one thread, a virtual clock and a seeded scheduler, so a state
//! machine's run under it is a function of the seed. It delays completions, delivers the ones
//! that are ready in a shuffled order, fails effects at a chosen rate, and [`Sim::crash`]
//! loses what was not synced, keeping a random prefix of it (a torn append), as a power cut
//! does. A failing seed replays exactly.

use std::{collections::BTreeMap, time::Duration};

use crate::{Completion, Done, Driver, Effect, FileName, IoError, Meta, Tag};

/// SplitMix64: small, fast, and the same everywhere.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True once in `n` on average; never when `n` is 0.
    pub fn one_in(&mut self, n: u32) -> bool {
        n != 0 && self.below(u64::from(n)) == 0
    }

    /// The items of `items` in an order of the seed's.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for at in (1..items.len()).rev() {
            let other = self.below(at as u64 + 1) as usize;
            items.swap(at, other);
        }
    }
}

/// How unkind the simulated machine is.
#[derive(Clone, Debug)]
pub struct Faults {
    /// An effect fails once in this many (0: never). A failed append writes a random part of
    /// its bytes, as a short write does; a failed sync makes nothing durable; a failed write
    /// leaves the file as it was.
    pub fail_one_in: u32,
    /// Each completion is ready after a delay drawn up to this.
    pub max_latency: Duration,
    /// The file system cannot share blocks: every clone fails, as on ext4.
    pub no_clone: bool,
    /// A watch's change is reported as lost (the queue overflowed) once in this many.
    pub lose_change_one_in: u32,
}

impl Default for Faults {
    fn default() -> Faults {
        Faults {
            fail_one_in: 0,
            max_latency: Duration::from_micros(200),
            no_clone: false,
            lose_change_one_in: 0,
        }
    }
}

#[derive(Default, Clone, Debug)]
struct SimFile {
    /// What survives a crash.
    durable: Vec<u8>,
    /// Appended since the last sync that covered it; read as part of the file, lost (in part)
    /// by a crash.
    volatile: Vec<u8>,
}

#[derive(Debug)]
struct Pending {
    ready: Duration,
    effect: Effect,
    /// For a sync: how many volatile bytes the file had when it was submitted.
    covers: usize,
}

pub struct Sim {
    rng: Rng,
    faults: Faults,
    now: Duration,
    files: BTreeMap<FileName, SimFile>,
    pending: Vec<Pending>,
    watches: BTreeMap<Tag, FileName>,
    /// Changes and ends of watches, each ready at its time.
    told: Vec<(Duration, Completion)>,
}

impl Sim {
    pub fn new(seed: u64, faults: Faults) -> Sim {
        Sim {
            rng: Rng::new(seed),
            faults,
            now: Duration::ZERO,
            files: BTreeMap::new(),
            pending: Vec::new(),
            watches: BTreeMap::new(),
            told: Vec::new(),
        }
    }

    /// The machine loses power: every effect in flight is gone without a completion, every
    /// watch with it, and each file keeps what was synced plus a random prefix of what was
    /// appended after. A whole-file write that completed is kept whole; one in flight is not
    /// there.
    pub fn crash(&mut self) {
        self.pending.clear();
        self.watches.clear();
        self.told.clear();
        for file in self.files.values_mut() {
            let kept = self.rng.below(file.volatile.len() as u64 + 1) as usize;
            file.durable.extend_from_slice(&file.volatile[..kept]);
            file.volatile.clear();
        }
    }

    /// What a file holds now, synced or not.
    pub fn contents(&self, file: &FileName) -> Vec<u8> {
        self.files
            .get(file)
            .map(|it| [it.durable.as_slice(), &it.volatile].concat())
            .unwrap_or_default()
    }

    /// Whether the file is there.
    pub fn exists(&self, file: &FileName) -> bool {
        self.files.contains_key(file)
    }

    /// The file made to hold `bytes`, durably, by something outside the state machine (a
    /// person's editor): watches of it hear of it.
    pub fn change(&mut self, file: &FileName, bytes: &[u8]) {
        self.files.insert(file.clone(), SimFile { durable: bytes.to_vec(), volatile: Vec::new() });
        self.changed(file);
    }

    /// The file removed by something outside the state machine.
    pub fn delete(&mut self, file: &FileName) {
        self.files.remove(file);
        self.changed(file);
    }

    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    fn delay(&mut self) -> Duration {
        Duration::from_nanos(self.rng.below(self.faults.max_latency.as_nanos() as u64 + 1))
    }

    /// Tells the watches of `file` it changed.
    fn changed(&mut self, file: &FileName) {
        let watching: Vec<Tag> = self
            .watches
            .iter()
            .filter(|(_, dir)| file.is_under(dir))
            .map(|(&tag, _)| tag)
            .collect();
        for tag in watching {
            let done = match self.rng.one_in(self.faults.lose_change_one_in) {
                true => Done::Changed { files: Vec::new(), lost: true },
                false => Done::Changed { files: vec![file.clone()], lost: false },
            };
            let at = self.now + self.delay();
            self.told.push((at, Completion { tag, done }));
        }
    }

    fn fail(&mut self, tag: Tag) -> Completion {
        Completion { tag, done: Done::Failed(IoError("injected by the simulation".into())) }
    }

    fn complete(&mut self, pending: Pending) -> Completion {
        let tag = pending.effect.tag();
        if !matches!(pending.effect, Effect::Timer { .. })
            && self.rng.one_in(self.faults.fail_one_in)
        {
            if let Effect::Append { file, bytes, .. } = &pending.effect {
                let torn = self.rng.below(bytes.len() as u64) as usize;
                self.files
                    .entry(file.clone())
                    .or_default()
                    .volatile
                    .extend_from_slice(&bytes[..torn]);
                if torn > 0 {
                    self.changed(&file.clone());
                }
            }
            return self.fail(tag);
        }
        let done = match pending.effect {
            Effect::ReadAll { file, .. } => Done::Read(self.contents(&file)),
            Effect::Read { file, at, len, .. } => {
                let all = self.contents(&file);
                let at = (at as usize).min(all.len());
                Done::Read(all[at..(at + len).min(all.len())].to_vec())
            }
            Effect::Append { file, bytes, .. } => {
                self.files.entry(file.clone()).or_default().volatile.extend_from_slice(&bytes);
                self.changed(&file);
                Done::Appended
            }
            Effect::Sync { file, .. } => {
                let file = self.files.entry(file).or_default();
                let covered = pending.covers.min(file.volatile.len());
                file.durable.extend(file.volatile.drain(..covered));
                Done::Synced
            }
            // Simpler than a disk: the cut is not undone by a crash before the next sync.
            Effect::Truncate { file, len, .. } => {
                let sim = self.files.entry(file.clone()).or_default();
                let len = len as usize;
                if len <= sim.durable.len() {
                    sim.durable.truncate(len);
                    sim.volatile.clear();
                } else {
                    sim.volatile.truncate(len - sim.durable.len());
                }
                self.changed(&file);
                Done::Truncated
            }
            Effect::Write { file, bytes, .. } => {
                self.files.insert(file.clone(), SimFile { durable: bytes, volatile: Vec::new() });
                self.changed(&file);
                Done::Written
            }
            Effect::Clone { from, to, .. } | Effect::Link { from, to, .. }
                if !self.files.contains_key(&from) =>
            {
                let _ = to;
                Done::Failed(IoError(format!("{from}: no such file")))
            }
            Effect::Clone { .. } if self.faults.no_clone => Done::Failed(IoError(
                "the simulated file system cannot share blocks; blaze does not copy instead"
                    .into(),
            )),
            Effect::Clone { to, .. } | Effect::Link { to, .. } if self.files.contains_key(&to) => {
                Done::Failed(IoError(format!("{to}: already there")))
            }
            Effect::Clone { from, to, .. } => {
                let bytes = self.contents(&from);
                self.files.insert(to.clone(), SimFile { durable: bytes, volatile: Vec::new() });
                self.changed(&to);
                Done::Cloned
            }
            Effect::Link { from, to, .. } => {
                let bytes = self.contents(&from);
                self.files.insert(to.clone(), SimFile { durable: bytes, volatile: Vec::new() });
                self.changed(&to);
                Done::Linked
            }
            Effect::Stat { file, .. } => Done::Stat(match self.files.get(&file) {
                Some(it) => Some(Meta {
                    len: (it.durable.len() + it.volatile.len()) as u64,
                    dir: false,
                    modified_ns: 0,
                }),
                None if self.files.keys().any(|it| it != &file && it.is_under(&file)) => {
                    Some(Meta { len: 0, dir: true, modified_ns: 0 })
                }
                None => None,
            }),
            Effect::Remove { file, .. } => {
                if self.files.remove(&file).is_some() {
                    self.changed(&file);
                }
                Done::Removed
            }
            Effect::Timer { .. } => Done::Fired,
            Effect::Watch { .. } | Effect::Unwatch { .. } => {
                unreachable!("watches are not pending effects")
            }
        };
        Completion { tag, done }
    }
}

impl Driver for Sim {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::Watch { dir, tag } => {
                    self.watches.insert(tag, dir);
                    continue;
                }
                Effect::Unwatch { tag } => {
                    let done = match self.watches.remove(&tag) {
                        Some(_) => Done::Unwatched,
                        None => Done::Failed(IoError(format!("nothing is watched as {tag:?}"))),
                    };
                    let at = self.now + self.delay();
                    // A change told before the end is heard before it.
                    let at = self
                        .told
                        .iter()
                        .filter(|(_, it)| it.tag == tag)
                        .map(|(at, _)| *at)
                        .max()
                        .map_or(at, |last| last.max(at));
                    self.told.push((at, Completion { tag, done }));
                    continue;
                }
                _ => {}
            }
            let delay = match &effect {
                Effect::Timer { after, .. } => *after,
                _ => self.delay(),
            };
            let covers = match &effect {
                Effect::Sync { file, .. } => self.files.get(file).map_or(0, |it| it.volatile.len()),
                _ => 0,
            };
            self.pending.push(Pending { ready: self.now + delay, effect, covers });
        }
    }

    /// Never waits: time jumps to the first completion when none is ready.
    fn poll(&mut self, out: &mut Vec<Completion>, _wait: Duration) -> usize {
        let first = self.pending.iter().map(|it| it.ready).chain(self.told.iter().map(|it| it.0));
        let Some(first) = first.min() else { return 0 };
        self.now = self.now.max(first);
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].ready <= self.now {
                ready.push(self.pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        // `swap_remove` already disturbed the order; shuffle so it is the seed's, not the vec's.
        self.rng.shuffle(&mut ready);
        let before = out.len();
        for pending in ready {
            let completion = self.complete(pending);
            out.push(completion);
        }
        // What watches were told, in the order they were told: a watch's end comes last.
        let mut index = 0;
        while index < self.told.len() {
            if self.told[index].0 <= self.now {
                out.push(self.told.remove(index).1);
            } else {
                index += 1;
            }
        }
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        let ending = self.told.iter().filter(|(_, it)| !matches!(it.done, Done::Changed { .. }));
        self.pending.len() + self.watches.len() + ending.count()
    }

    fn now(&self) -> Duration {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_until_idle;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    #[test]
    fn a_crash_keeps_what_was_synced_and_a_prefix_of_the_rest() {
        for seed in 0..200 {
            let mut sim = Sim::new(seed, Faults::default());
            let file = name("log");
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: b"synced".to_vec(), tag: Tag(0) }];
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(1) });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Append {
                file: file.clone(),
                bytes: b"-lost".to_vec(),
                tag: Tag(2),
            });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            sim.crash();
            let left = sim.contents(&file);
            assert!(left.starts_with(b"synced") && b"synced-lost".starts_with(&left), "{left:?}");
        }
    }

    #[test]
    fn the_same_seed_is_the_same_run() {
        let run = |seed| {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 4, ..Faults::default() });
            let mut effects: Vec<Effect> = (0..32)
                .map(|n| Effect::Append { file: name("f"), bytes: vec![n as u8], tag: Tag(n) })
                .collect();
            let mut order = Vec::new();
            run_until_idle(&mut sim, &mut effects, |done, _| order.push(done));
            (order, sim.contents(&name("f")))
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    #[test]
    fn a_sync_covers_only_what_was_appended_before_it() {
        let mut sim = Sim::new(
            1,
            Faults { fail_one_in: 0, max_latency: Duration::ZERO, ..Faults::default() },
        );
        let file = name("log");
        let mut effects =
            vec![Effect::Append { file: file.clone(), bytes: b"a".to_vec(), tag: Tag(0) }];
        run_until_idle(&mut sim, &mut effects, |_, _| {});
        sim.submit(&mut vec![Effect::Sync { file: file.clone(), tag: Tag(1) }]);
        // Appended after the sync was asked for, completed before it.
        sim.files.get_mut(&file).unwrap().volatile.extend_from_slice(b"b");
        let mut out = Vec::new();
        sim.poll(&mut out, Duration::ZERO);
        let file = &sim.files[&file];
        assert_eq!((file.durable.as_slice(), file.volatile.as_slice()), (&b"a"[..], &b"b"[..]));
    }

    /// A whole-file write is all or nothing over a crash at any point, on every seed.
    #[test]
    fn a_write_is_whole_or_not_there_after_a_crash() {
        for seed in 0..2_000 {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 5, ..Faults::default() });
            let file = name("store/index");
            sim.change(&file, b"old contents");
            sim.submit(&mut vec![Effect::Write {
                file: file.clone(),
                bytes: b"new".to_vec(),
                tag: Tag(1),
            }]);
            let mut out = Vec::new();
            if sim.rng().one_in(2) {
                sim.poll(&mut out, Duration::ZERO);
            }
            let acknowledged = out.iter().any(|it| it.done == Done::Written);
            sim.crash();
            let left = sim.contents(&file);
            match acknowledged {
                true => assert_eq!(left, b"new", "seed {seed}"),
                false => assert!(left == b"new" || left == b"old contents", "seed {seed}"),
            }
        }
    }

    #[test]
    fn watches_hear_changes_under_their_directory_then_their_end() {
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults { lose_change_one_in: 7, ..Faults::default() });
            let mut effects = vec![Effect::Watch { dir: name("src"), tag: Tag(9) }];
            sim.submit(&mut effects);
            sim.change(&name("src/lib.rs"), b"fn main() {}");
            sim.change(&name("target/out"), b"not watched");
            effects.push(Effect::Write { file: name("src/a/b.rs"), bytes: vec![1], tag: Tag(1) });
            effects.push(Effect::Unwatch { tag: Tag(9) });
            let mut heard = Vec::new();
            let mut lost = false;
            let mut ended = false;
            run_until_idle(&mut sim, &mut effects, |done, _| match done.done {
                Done::Changed { files, lost: was_lost } => {
                    assert!(!ended, "seed {seed}: a change after the end");
                    lost |= was_lost;
                    heard.extend(files);
                }
                Done::Unwatched => ended = true,
                _ => {}
            });
            assert!(ended, "seed {seed}");
            assert!(!heard.contains(&name("target/out")), "seed {seed}");
            if !lost {
                assert!(heard.contains(&name("src/lib.rs")), "seed {seed}: {heard:?}");
            }
        }
    }

    #[test]
    fn a_file_system_that_cannot_share_blocks_refuses_a_clone() {
        let mut sim = Sim::new(3, Faults { no_clone: true, ..Faults::default() });
        sim.change(&name("store/v"), b"code");
        let mut effects = vec![
            Effect::Clone { from: name("store/v"), to: name("target/a"), tag: Tag(1) },
            Effect::Link { from: name("store/v"), to: name("target/b"), tag: Tag(2) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done));
        assert!(matches!(&seen[&Tag(1)], Done::Failed(why) if why.0.contains("does not copy")));
        assert_eq!(seen[&Tag(2)], Done::Linked);
        assert!(!sim.exists(&name("target/a")));
        assert_eq!(sim.contents(&name("target/b")), b"code");
    }
}
