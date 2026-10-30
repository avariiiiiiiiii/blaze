//! A hierarchical timing wheel (Varghese and Lauck's, laid out as tokio's timer is): the timers
//! of a driver, keyed by a deadline in `u64` ticks (nanoseconds since the driver began).
//!
//! Eleven levels of 64 slots cover every `u64`: a timer sits at the level of the highest 6-bit
//! group in which its deadline differs from the wheel's time, in the slot that group names.
//! Inserting is a shift and a link, cancelling an unlink through its [`Handle`], and advancing
//! walks the occupied slots in deadline order through one bitmap per level, moving a higher
//! slot's timers down once time reaches it (each timer moves at most ten times). Timers of one
//! deadline fire in the order they were inserted. Cost: 24 bytes of links per timer beside its
//! value, 5.5 KiB of slot heads, and O(1) insert, cancel and step of an advance.

use std::time::Duration;

const BITS: u32 = 6;
const SLOTS: usize = 1 << BITS;
/// 11 groups of 6 bits cover 64.
const LEVELS: usize = 11;
const NIL: u32 = u32::MAX;

/// A tick count for `duration`: nanoseconds, the last tick for one past what `u64` holds
/// (some 584 years).
pub fn ticks(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Names a timer in the wheel until it fires or is cancelled; a stale one names nothing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Handle {
    index: u32,
    generation: u32,
}

/// A timer's place: its deadline and the links of its slot's list, in insertion order.
#[derive(Clone, Copy)]
struct Link {
    deadline: u64,
    prev: u32,
    next: u32,
    generation: u32,
    /// `level * SLOTS + slot`, or [`NIL`] while free (then `next` is the next free entry).
    slot: u32,
}

const _: () = assert!(size_of::<Link>() == 24);

#[derive(Clone, Copy)]
struct List {
    head: u32,
    tail: u32,
}

const EMPTY: List = List { head: NIL, tail: NIL };

pub struct Wheel<T> {
    /// Every timer due at or before this has fired.
    now: u64,
    /// Bit `slot` of level `level`: that slot's list is not empty.
    occupied: [u64; LEVELS],
    slots: Box<[List; LEVELS * SLOTS]>,
    links: Vec<Link>,
    values: Vec<Option<T>>,
    free: u32,
    len: usize,
    /// The soonest deadline, while known; recomputed after the timer that had it leaves.
    soonest: Option<u64>,
}

impl<T> Default for Wheel<T> {
    fn default() -> Wheel<T> {
        Wheel::new()
    }
}

impl<T> Wheel<T> {
    pub fn new() -> Wheel<T> {
        Wheel {
            now: 0,
            occupied: [0; LEVELS],
            slots: Box::new([EMPTY; LEVELS * SLOTS]),
            links: Vec::new(),
            values: Vec::new(),
            free: NIL,
            len: 0,
            soonest: None,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The wheel's time: what the last [`Wheel::advance`] went to.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// `value` due at `deadline`; one already passed is due at the next advance.
    pub fn insert(&mut self, deadline: u64, value: T) -> Handle {
        let deadline = deadline.max(self.now);
        let index = match self.free {
            NIL => {
                let index = u32::try_from(self.links.len()).ok().filter(|&it| it != NIL);
                let index = index.expect("fewer than 2^32 - 1 timers at once");
                self.links.push(Link { deadline, prev: NIL, next: NIL, generation: 0, slot: NIL });
                self.values.push(Some(value));
                index
            }
            index => {
                self.free = self.links[index as usize].next;
                self.links[index as usize].deadline = deadline;
                self.values[index as usize] = Some(value);
                index
            }
        };
        self.place(index);
        self.len += 1;
        self.soonest = match (self.len, self.soonest) {
            (1, _) => Some(deadline),
            (_, Some(soonest)) => Some(soonest.min(deadline)),
            (_, None) => None,
        };
        Handle { index, generation: self.links[index as usize].generation }
    }

    /// The timer's deadline, while it is in the wheel.
    pub fn deadline(&self, handle: Handle) -> Option<u64> {
        self.live(handle).then(|| self.links[handle.index as usize].deadline)
    }

    pub fn get_mut(&mut self, handle: Handle) -> Option<&mut T> {
        match self.live(handle) {
            true => self.values[handle.index as usize].as_mut(),
            false => None,
        }
    }

    /// The timer taken out before it fires; `None` once it has fired or been cancelled.
    pub fn cancel(&mut self, handle: Handle) -> Option<T> {
        if !self.live(handle) {
            return None;
        }
        let index = handle.index;
        let deadline = self.links[index as usize].deadline;
        self.unlink(index);
        if self.soonest == Some(deadline) {
            self.soonest = None;
        }
        Some(self.release(index))
    }

    /// The soonest deadline of a timer in the wheel.
    pub fn next_deadline(&mut self) -> Option<u64> {
        if self.len == 0 {
            return None;
        }
        if let Some(soonest) = self.soonest {
            return Some(soonest);
        }
        let (level, slot) = self.first().expect("a timer is in the wheel");
        let soonest = match level {
            // A slot of level 0 is one tick.
            0 => self.start(0, slot),
            _ => {
                let mut soonest = u64::MAX;
                let mut at = self.slots[level * SLOTS + slot].head;
                while at != NIL {
                    let link = &self.links[at as usize];
                    soonest = soonest.min(link.deadline);
                    at = link.next;
                }
                soonest
            }
        };
        self.soonest = Some(soonest);
        Some(soonest)
    }

    /// Time moved on to `to`: each timer due by then handed to `fire` with its deadline, in
    /// deadline order and, for one deadline, in the order they were inserted. A `to` behind the
    /// wheel's time fires nothing.
    pub fn advance(&mut self, to: u64, mut fire: impl FnMut(u64, T)) {
        while let Some((level, slot)) = self.first() {
            let start = self.start(level, slot);
            if start > to {
                break;
            }
            self.now = start;
            let list = std::mem::replace(&mut self.slots[level * SLOTS + slot], EMPTY);
            self.occupied[level] &= !(1 << slot);
            let mut at = list.head;
            while at != NIL {
                let next = self.links[at as usize].next;
                match level {
                    0 => {
                        let deadline = self.links[at as usize].deadline;
                        if self.soonest.is_some_and(|it| it <= deadline) {
                            self.soonest = None;
                        }
                        let value = self.release(at);
                        fire(deadline, value);
                    }
                    // Every level below is empty, so each goes to the tail of an empty list in
                    // the order it was inserted.
                    _ => self.place(at),
                }
                at = next;
            }
        }
        self.now = self.now.max(to);
    }

    /// Every timer taken out, in deadline order, the wheel's time left as it is.
    pub fn drain(&mut self, mut each: impl FnMut(u64, T)) {
        let now = self.now;
        self.advance(u64::MAX, &mut each);
        self.now = now;
    }

    fn live(&self, handle: Handle) -> bool {
        self.links
            .get(handle.index as usize)
            .is_some_and(|it| it.generation == handle.generation && it.slot != NIL)
    }

    /// The occupied slot of the lowest level: every timer of a lower level is due before any
    /// of a higher (they share the wheel's time above their level), and within a level a slot
    /// at or before the wheel's position holds nothing, so its lowest bit is the soonest.
    fn first(&self) -> Option<(usize, usize)> {
        let level = self.occupied.iter().position(|&it| it != 0)?;
        Some((level, self.occupied[level].trailing_zeros() as usize))
    }

    /// The first tick of `slot` of `level` in the wheel's current block of that level.
    fn start(&self, level: usize, slot: usize) -> u64 {
        let above = u64::MAX.checked_shl(BITS * (level as u32 + 1)).unwrap_or(0);
        (self.now & above) | ((slot as u64) << (BITS * level as u32))
    }

    /// Links `index` at the tail of the slot its deadline names from the wheel's time.
    fn place(&mut self, index: u32) {
        let deadline = self.links[index as usize].deadline;
        let differ = (self.now ^ deadline) | (SLOTS as u64 - 1);
        let level = ((63 - differ.leading_zeros()) / BITS) as usize;
        let slot = ((deadline >> (BITS * level as u32)) as usize) & (SLOTS - 1);
        let at = level * SLOTS + slot;
        let tail = self.slots[at].tail;
        let link = &mut self.links[index as usize];
        link.prev = tail;
        link.next = NIL;
        link.slot = at as u32;
        match tail {
            NIL => self.slots[at].head = index,
            tail => self.links[tail as usize].next = index,
        }
        self.slots[at].tail = index;
        self.occupied[level] |= 1 << slot;
    }

    fn unlink(&mut self, index: u32) {
        let Link { prev, next, slot, .. } = self.links[index as usize];
        let list = &mut self.slots[slot as usize];
        match prev {
            NIL => list.head = next,
            prev => self.links[prev as usize].next = next,
        }
        match next {
            NIL => list.tail = prev,
            next => self.links[next as usize].prev = prev,
        }
        if list.head == NIL {
            self.occupied[slot as usize / SLOTS] &= !(1 << (slot as usize % SLOTS));
        }
    }

    /// Frees an entry already out of its list, for the next insert.
    fn release(&mut self, index: u32) -> T {
        let link = &mut self.links[index as usize];
        link.slot = NIL;
        link.generation = link.generation.wrapping_add(1);
        link.next = self.free;
        self.free = index;
        self.len -= 1;
        self.values[index as usize].take().expect("a timer in the wheel has its value")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::Rng;

    /// The model: every timer with its deadline and insertion number, fired by sorting.
    #[derive(Default)]
    struct Model {
        now: u64,
        timers: Vec<(u64, u64, u32)>,
    }

    impl Model {
        fn advance(&mut self, to: u64) -> Vec<(u64, u32)> {
            self.now = self.now.max(to);
            let mut due: Vec<_> = self.timers.iter().copied().filter(|it| it.0 <= to).collect();
            self.timers.retain(|it| it.0 > to);
            due.sort_unstable();
            due.into_iter().map(|(deadline, _, value)| (deadline, value)).collect()
        }
    }

    fn deadline(rng: &mut Rng, now: u64) -> u64 {
        match rng.below(8) {
            0 => now,
            1 => now.saturating_sub(rng.below(100)),
            2 => u64::MAX - rng.below(3),
            3 => now.saturating_add(rng.below(4)),
            4 => now.saturating_add(rng.below(1 << 12)),
            5 => now.saturating_add(rng.below(1 << 30)),
            6 => now.saturating_add(rng.next_u64() >> rng.below(64)),
            _ => now.saturating_add(1 << (6 * rng.below(11))),
        }
    }

    /// Against the model on every seed: inserts (passed, now, near, far, at the end of time,
    /// on level boundaries, many on one deadline), cancels of live and stale handles, the
    /// soonest deadline, and advances small, large and backwards.
    #[test]
    fn fires_as_sorting_by_deadline_then_insertion_does() {
        for seed in 0..400 {
            let mut rng = Rng::new(seed);
            let mut wheel = Wheel::new();
            let mut model = Model::default();
            let mut handles: Vec<(Handle, u32)> = Vec::new();
            let mut inserted = 0u64;
            for _ in 0..600 {
                match rng.below(10) {
                    0..=4 => {
                        let at = deadline(&mut rng, model.now);
                        let value = inserted as u32;
                        let handle = wheel.insert(at, value);
                        assert_eq!(wheel.deadline(handle), Some(at.max(model.now)), "seed {seed}");
                        model.timers.push((at.max(model.now), inserted, value));
                        handles.push((handle, value));
                        inserted += 1;
                    }
                    5 | 6 if !handles.is_empty() => {
                        let (handle, value) =
                            handles.swap_remove(rng.below(handles.len() as u64) as usize);
                        let live = model.timers.iter().position(|it| it.2 == value);
                        assert_eq!(wheel.cancel(handle), live.map(|_| value), "seed {seed}");
                        if let Some(at) = live {
                            model.timers.remove(at);
                        }
                        assert_eq!(wheel.cancel(handle), None, "seed {seed}: cancelled twice");
                    }
                    7 => {
                        let soonest = model.timers.iter().map(|it| it.0).min();
                        assert_eq!(wheel.next_deadline(), soonest, "seed {seed}");
                    }
                    _ => {
                        let to = match rng.below(5) {
                            0 => model.now.saturating_sub(rng.below(10)),
                            1 => wheel.next_deadline().unwrap_or(model.now),
                            2 => u64::MAX,
                            _ => deadline(&mut rng, model.now),
                        };
                        let mut fired = Vec::new();
                        wheel.advance(to, |at, value| fired.push((at, value)));
                        assert_eq!(fired, model.advance(to), "seed {seed}");
                        assert_eq!(wheel.now(), model.now, "seed {seed}");
                    }
                }
                assert_eq!(wheel.len(), model.timers.len(), "seed {seed}");
            }
            let mut rest = Vec::new();
            wheel.drain(|at, value| rest.push((at, value)));
            assert_eq!(rest, model.advance(u64::MAX), "seed {seed}");
            assert!(wheel.is_empty());
        }
    }

    #[test]
    fn one_deadline_fires_in_insertion_order_across_levels() {
        let mut wheel = Wheel::new();
        let due = (1 << 40) + 5;
        for value in 0..41u32 {
            wheel.insert(due, value);
            // Closer each time, so the same deadline is placed from every level as the
            // earlier ones are moved down.
            wheel.advance(due - (1 << (40 - value)), |_, _| panic!("not due"));
        }
        let mut fired = Vec::new();
        wheel.advance(due, |at, value| fired.push((at, value)));
        assert_eq!(fired, (0..41).map(|it| (due, it)).collect::<Vec<_>>());
    }

    #[test]
    fn a_stale_handle_names_nothing_after_its_entry_is_reused() {
        let mut wheel = Wheel::new();
        let first = wheel.insert(10, "a");
        wheel.advance(10, |_, _| {});
        let second = wheel.insert(20, "b");
        assert_eq!((wheel.cancel(first), wheel.deadline(first)), (None, None));
        assert_eq!(wheel.get_mut(second).copied(), Some("b"));
        assert_eq!(wheel.cancel(second), Some("b"));
    }
}
