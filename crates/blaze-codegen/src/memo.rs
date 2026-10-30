//! The back-end memo (#1291): templates by the shape of what was compiled, so two instances
//! whose prepared IR is equal up to what their places hold compile once.
//!
//! Keyed by [`crate::backend::Shape::key`], which holds the settings, the architecture and the IR's canonical
//! bytes in full: BLAKE3 over all of it, so a hit is the same compile and not a near one.
//! Shared by every worker of a build and every build of the daemon.
//!
//! An append-only open-addressed table (docs/v2/plan/design/concurrency.md: a read takes no
//! lock and writes no shared memory). Each slot is a pointer to an immutable entry, published
//! by one compare-and-swap; a full probe window goes on to the next level, which has twice
//! the slots and is made on first use. A read gives `&Template` for as long as the memo is
//! borrowed: nothing is freed under `&self`, only by [`Memo::retain`], which takes `&mut self`.
//!
//! Unsafe for speed: the safe table, a `RwLock<HashMap>`, writes the lock's word on every
//! read, so each worker's lookup of every function moves one cache line between all cores.
//! Its tests check it against a map of first templates, and the model checker
//! (`cfg(blaze_check)`) runs its races.

#[cfg(not(blaze_check))]
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::{marker::PhantomData, ptr};

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use blaze_ids::ContentId;

use crate::template::Template;

/// Slots of level 0; level `n` has `FIRST << n`. Under the model checker a level is a few
/// slots, so a handful of puts spill and race to make the next level.
const FIRST: usize = if cfg!(blaze_check) { 2 } else { 1 << 10 };
const LEVELS: usize = 40;
/// Slots tried in a level before the next.
const PROBES: usize = if cfg!(blaze_check) { 2 } else { 16 };

struct Entry {
    shape: ContentId,
    template: Template,
}

type Level = [AtomicPtr<Entry>];

pub struct Memo {
    /// Each a `Box<Level>` of `FIRST << n` slots, leaked; null until first used.
    levels: [AtomicPtr<AtomicPtr<Entry>>; LEVELS],
    len: AtomicUsize,
    owns: PhantomData<Box<Entry>>,
}

const _: () = {
    const fn sync_and_send<T: Sync + Send>() {}
    sync_and_send::<Template>();
};

impl Default for Memo {
    fn default() -> Memo {
        Memo::new()
    }
}

/// Where a shape's probes start: its leading bits, which BLAKE3 makes uniform.
fn start(shape: &ContentId) -> usize {
    u64::from_le_bytes(shape.0[..8].try_into().expect("eight bytes")) as usize
}

fn slots(level: usize) -> usize {
    FIRST << level
}

impl Memo {
    pub fn new() -> Memo {
        Memo {
            levels: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            len: AtomicUsize::new(0),
            owns: PhantomData,
        }
    }

    /// Level `n`, if it was made.
    fn level(&self, n: usize) -> Option<&Level> {
        let first = self.levels[n].load(Ordering::Acquire);
        // SAFETY: a non-null level is a leaked `Box<[AtomicPtr<Entry>]>` of `slots(n)`
        // slots, published once by `level_or_make`'s CAS and freed only under `&mut self`.
        (!first.is_null()).then(|| unsafe { std::slice::from_raw_parts(first, slots(n)) })
    }

    fn level_or_make(&self, n: usize) -> &Level {
        if let Some(level) = self.level(n) {
            return level;
        }
        let made: Box<Level> = (0..slots(n)).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
        let made = Box::into_raw(made) as *mut AtomicPtr<Entry>;
        match self.levels[n].compare_exchange(ptr::null_mut(), made, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {}
            // SAFETY: `made` was never published, so this is its only owner.
            Err(_) => drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(made, slots(n))) }),
        }
        self.level(n).expect("made or made by another")
    }

    /// The entry at `slot`, if one is there.
    fn entry(slot: &AtomicPtr<Entry>) -> Option<&Entry> {
        let it = slot.load(Ordering::Acquire);
        // SAFETY: a published entry is immutable and freed only under `&mut self`.
        (!it.is_null()).then(|| unsafe { &*it })
    }

    pub fn get(&self, shape: &ContentId) -> Option<&Template> {
        let start = start(shape);
        for n in 0..LEVELS {
            let level = self.level(n)?;
            let mask = slots(n) - 1;
            for probe in 0..PROBES {
                match Self::entry(&level[(start + probe) & mask]) {
                    Some(entry) if entry.shape == *shape => return Some(&entry.template),
                    Some(_) => {}
                    // Slots fill in probe order and are never emptied under `&self`: a shape
                    // that is anywhere is before the first empty slot of its window.
                    None => return None,
                }
            }
        }
        None
    }

    /// Keeps `template` as what `shape` compiles to, and gives what is kept: the first, if
    /// two workers compiled one shape at once. Both are the same compile; the first is kept
    /// so every instantiation of the shape in a build shares one template.
    pub fn put(&self, shape: ContentId, template: Template) -> &Template {
        let start = start(&shape);
        let ours = Box::into_raw(Box::new(Entry { shape, template }));
        for n in 0..LEVELS {
            let level = self.level_or_make(n);
            let mask = slots(n) - 1;
            for probe in 0..PROBES {
                let slot = &level[(start + probe) & mask];
                if let Some(entry) = Self::entry(slot) {
                    if entry.shape == shape {
                        // SAFETY: `ours` was never published.
                        drop(unsafe { Box::from_raw(ours) });
                        return &entry.template;
                    }
                    continue;
                }
                match slot.compare_exchange(ptr::null_mut(), ours, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        self.len.fetch_add(1, Ordering::Relaxed);
                        // SAFETY: published just now, and freed only under `&mut self`.
                        return unsafe { &(*ours).template };
                    }
                    Err(theirs) => {
                        // SAFETY: published by another put, as above.
                        let theirs = unsafe { &*theirs };
                        if theirs.shape == shape {
                            // SAFETY: `ours` was never published.
                            drop(unsafe { Box::from_raw(ours) });
                            return &theirs.template;
                        }
                    }
                }
            }
        }
        // SAFETY: `ours` was never published.
        drop(unsafe { Box::from_raw(ours) });
        panic!("the back-end memo is full: {LEVELS} levels of {PROBES} probes each")
    }

    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every entry, taken out of the table, which is left empty.
    fn drain(&mut self) -> Vec<Box<Entry>> {
        let mut entries = Vec::with_capacity(self.len());
        for n in 0..LEVELS {
            let first = std::mem::replace(self.levels[n].get_mut(), ptr::null_mut());
            if first.is_null() {
                continue;
            }
            // SAFETY: `&mut self`: no reader holds a level or an entry.
            let level = unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(first, slots(n))) };
            for mut slot in level.into_vec() {
                let it = std::mem::replace(slot.get_mut(), ptr::null_mut());
                if !it.is_null() {
                    // SAFETY: a non-null slot holds a `Box<Entry>` that `put` leaked and
                    // published once; `&mut self` means no reader holds it, and the slot was
                    // nulled just above, so this is its only owner (else a double free).
                    entries.push(unsafe { Box::from_raw(it) });
                }
            }
        }
        *self.len.get_mut() = 0;
        entries
    }

    /// Drops every template not of `kept`: what the daemon's collection keeps reachable. Runs
    /// between builds, when nothing borrows the memo.
    pub fn retain(&mut self, kept: impl Fn(&ContentId) -> bool) {
        for entry in self.drain() {
            if kept(&entry.shape) {
                let Entry { shape, template } = *entry;
                self.put(shape, template);
            }
        }
    }
}

impl Drop for Memo {
    fn drop(&mut self) {
        drop(self.drain());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Arch;

    fn template(byte: u8) -> Template {
        Template {
            arch: Arch::Aarch64,
            align: 4,
            code: vec![byte; 4],
            relocs: crate::Relocs::new(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            refs: 0,
            externs: Vec::new(),
            coverage: None,
            profile: None,
        }
    }

    fn shape(n: u64) -> ContentId {
        let mut id = ContentId([0; 32]);
        id.0[..8].copy_from_slice(&n.to_le_bytes());
        id.0[8..16].copy_from_slice(&(n ^ 0x9e37_79b9_7f4a_7c15).to_le_bytes());
        id
    }

    #[test]
    fn the_first_template_of_a_shape_is_kept() {
        let memo = Memo::new();
        let shape = ContentId([5; 32]);
        assert!(memo.get(&shape).is_none());
        assert_eq!(*memo.put(shape, template(1)), template(1));
        assert_eq!(*memo.put(shape, template(2)), template(1));
        assert_eq!(*memo.get(&shape).unwrap(), template(1));
        assert_eq!(memo.len(), 1);
    }

    #[test]
    fn collection_keeps_what_it_is_told() {
        let mut memo = Memo::new();
        memo.put(ContentId([1; 32]), template(1));
        memo.put(ContentId([2; 32]), template(2));
        memo.retain(&|shape| shape.0[0] == 2);
        assert!(memo.get(&ContentId([1; 32])).is_none());
        assert_eq!(*memo.get(&ContentId([2; 32])).unwrap(), template(2));
        assert_eq!(memo.len(), 1);
    }

    #[test]
    fn shapes_that_start_at_one_slot_spill_into_later_levels() {
        let memo = Memo::new();
        // Equal leading bits: every one probes the same window of every level.
        let crowded: Vec<ContentId> = (0..(PROBES * 3) as u8)
            .map(|n| {
                let mut id = ContentId([7; 32]);
                id.0[31] = n;
                id
            })
            .collect();
        for (n, shape) in crowded.iter().enumerate() {
            memo.put(*shape, template(n as u8));
        }
        for (n, shape) in crowded.iter().enumerate() {
            assert_eq!(*memo.get(shape).unwrap(), template(n as u8));
        }
        assert!(memo.level(2).is_some());
        assert_eq!(memo.len(), crowded.len());
    }

    /// The memo against a map that keeps the first template of a shape: the same answers for
    /// a run of puts, gets and collections, crowded into few windows so most spill.
    #[test]
    fn the_memo_answers_as_a_map_of_first_templates() {
        let mut memo = Memo::new();
        let mut reference: std::collections::HashMap<ContentId, Template> = Default::default();
        let mut rng = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let pick = |n: u64| {
            let mut id = ContentId([0; 32]);
            id.0[..8].copy_from_slice(&(n % 8).to_le_bytes());
            id.0[31] = (n % 31) as u8;
            id
        };
        for _ in 0..4000 {
            let (op, n) = (next() % 16, next());
            let shape = pick(n);
            match op {
                0 => {
                    let kept = (next() % 4) as u8;
                    memo.retain(&|it| it.0[31] % 4 != kept);
                    reference.retain(|it, _| it.0[31] % 4 != kept);
                }
                1..8 => {
                    let template = template(n as u8);
                    let want = reference.entry(shape).or_insert(template.clone()).clone();
                    assert_eq!(*memo.put(shape, template), want);
                }
                _ => assert_eq!(memo.get(&shape), reference.get(&shape)),
            }
            assert_eq!(memo.len(), reference.len());
        }
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn workers_putting_at_once_keep_one_template_per_shape() {
        let memo = Memo::new();
        let shapes: Vec<ContentId> = (0..4096).map(shape).collect();
        std::thread::scope(|scope| {
            for worker in 0..8u8 {
                let (memo, shapes) = (&memo, &shapes);
                scope.spawn(move || {
                    for it in shapes {
                        let kept = memo.put(*it, template(worker));
                        assert_eq!(memo.get(it).unwrap() as *const Template, kept as *const Template);
                    }
                });
            }
        });
        assert_eq!(memo.len(), shapes.len());
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-codegen
/// --release model_`): puts racing on one shape keep one template, puts crowding one window
/// race to make the next level, and a get racing a put sees nothing or what the put kept.
#[cfg(all(test, blaze_check))]
mod model {
    use std::sync::Arc;

    use blaze_check::thread;

    use super::*;
    use crate::Arch;

    fn template(byte: u8) -> Template {
        Template {
            arch: Arch::Aarch64,
            align: 4,
            code: vec![byte; 4],
            relocs: crate::Relocs::new(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            refs: 0,
            externs: Vec::new(),
            coverage: None,
            profile: None,
        }
    }

    /// Shapes that start their probes at one slot, told apart by `n`.
    fn crowded(n: u8) -> ContentId {
        let mut id = ContentId([0; 32]);
        id.0[31] = n;
        id
    }

    fn kept_at(template: &Template) -> usize {
        std::ptr::from_ref(template).addr()
    }

    #[test]
    fn model_puts_of_one_shape_keep_one_template() {
        blaze_check::model(|| {
            let memo = Arc::new(Memo::new());
            let other = {
                let memo = memo.clone();
                thread::spawn(move || kept_at(memo.put(crowded(0), template(1))))
            };
            let kept = memo.put(crowded(0), template(2));
            assert_eq!(kept_at(kept), other.join().unwrap(), "one template kept");
            assert!(*kept == template(1) || *kept == template(2));
            assert_eq!(memo.get(&crowded(0)).map(kept_at), Some(kept_at(kept)));
            assert_eq!(memo.len(), 1);
        });
    }

    #[test]
    fn model_crowded_puts_race_to_make_the_next_level() {
        blaze_check::model(|| {
            let memo = Arc::new(Memo::new());
            let other = {
                let memo = memo.clone();
                thread::spawn(move || {
                    for n in [0, 1] {
                        assert_eq!(*memo.put(crowded(n), template(n)), template(n));
                    }
                })
            };
            assert_eq!(*memo.put(crowded(2), template(2)), template(2));
            other.join().unwrap();
            for n in 0..3 {
                assert_eq!(*memo.get(&crowded(n)).unwrap(), template(n));
            }
            assert_eq!(memo.len(), 3);
            assert!(memo.level(1).is_some(), "three shapes of one window spill");
        });
    }

    #[test]
    fn model_a_get_sees_nothing_or_what_a_put_kept() {
        blaze_check::model(|| {
            let memo = Arc::new(Memo::new());
            memo.put(crowded(0), template(0));
            memo.put(crowded(1), template(1));
            let other = {
                let memo = memo.clone();
                thread::spawn(move || kept_at(memo.put(crowded(2), template(2))))
            };
            let seen = memo.get(&crowded(2)).map(kept_at);
            let kept = other.join().unwrap();
            assert!(seen.is_none_or(|it| it == kept), "a get reads a published entry");
            assert_eq!(memo.get(&crowded(2)).map(kept_at), Some(kept));
        });
    }
}
