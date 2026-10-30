//! The queue's timers (a test's deadline, a kill's grace, a whole binary's deadline): a
//! hierarchical timing wheel (Varghese and Lauck, "Hashed and hierarchical timing wheels"; the
//! Linux kernel's and tokio's), so setting, cancelling and firing a timer are O(1) whatever
//! how many tests run, and the driver is asked for one timer, at the wheel's next expiry, not
//! one per test.
//!
//! Time is in ticks (the queue's are milliseconds). Level `l` has 64 slots of `64^l` ticks;
//! a timer is in the level of the highest 6-bit digit its due tick and the wheel's `now`
//! differ in, at its due tick's digit there, so every timer of level `l` is due within the
//! `64^(l+1)` ticks `now` is in and after the `64^l` ticks `now` is in. The first level with
//! a timer has the next expiry, at its lowest occupied slot (one `trailing_zeros` of its
//! bitmap); there, a level-0 slot's timers fire and a higher slot's move down. Eleven levels
//! cover every `u64` tick.
//!
//! A timer is named by a handle that holds its generation, so a handle kept past its timer's
//! firing names nothing, even once its slot is another timer's.
//!
//! Cost: 22 bytes a timer plus its payload (SoA columns: due, two links, generation, place),
//! 2.8 KiB of heads, and the slab reused: a steady stream of timers allocates nothing.

const LEVELS: usize = 11;
const SLOTS: usize = 64;
/// The end of a list.
const NONE: u32 = u32::MAX;
/// The handle of no timer.
pub(super) const UNSET: u64 = u64::MAX;
/// The place of a timer in no slot: free, or fired.
const NOWHERE: u16 = u16::MAX;

#[derive(Debug)]
pub(super) struct Wheel<T> {
    /// Every timer due at or before this tick has fired.
    now: u64,
    heads: [[u32; SLOTS]; LEVELS],
    occupied: [u64; LEVELS],
    due: Vec<u64>,
    what: Vec<Option<T>>,
    next: Vec<u32>,
    prev: Vec<u32>,
    /// Bumped each time the timer is released: the high half of its handles.
    generation: Vec<u32>,
    /// Its level times 64 plus its slot, or [`NOWHERE`].
    place: Vec<u16>,
    /// Free timers, linked by `next`.
    free: u32,
    len: usize,
}

impl<T: Copy> Wheel<T> {
    pub(super) fn new(now: u64) -> Wheel<T> {
        Wheel {
            now,
            heads: [[NONE; SLOTS]; LEVELS],
            occupied: [0; LEVELS],
            due: Vec::new(),
            what: Vec::new(),
            next: Vec::new(),
            prev: Vec::new(),
            generation: Vec::new(),
            place: Vec::new(),
            free: NONE,
            len: 0,
        }
    }

    /// How many timers are set.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The timers made so far, set or free: what the slab holds.
    #[cfg(test)]
    pub(super) fn made(&self) -> usize {
        self.due.len()
    }

    /// A timer of `what` due at tick `due`, by its handle; one due by `now` fires at the next
    /// tick.
    pub(super) fn insert(&mut self, due: u64, what: T) -> u64 {
        let id = match self.free {
            NONE => {
                self.due.push(0);
                self.what.push(None);
                self.next.push(NONE);
                self.prev.push(NONE);
                self.generation.push(0);
                self.place.push(NOWHERE);
                (self.due.len() - 1) as u32
            }
            free => {
                self.free = self.next[free as usize];
                free
            }
        };
        self.due[id as usize] = due.max(self.now + 1);
        self.what[id as usize] = Some(what);
        self.len += 1;
        self.link(id);
        (u64::from(self.generation[id as usize]) << 32) | u64::from(id)
    }

    /// The timer of `handle` cancelled; one fired or cancelled already, or [`UNSET`], is
    /// nothing.
    pub(super) fn remove(&mut self, handle: u64) {
        let id = handle as u32;
        let at = id as usize;
        if handle == UNSET
            || at >= self.place.len()
            || self.what[at].is_none()
            || self.generation[at] != (handle >> 32) as u32
        {
            return;
        }
        if self.place[at] != NOWHERE {
            self.unlink(id);
        }
        self.release(id);
    }

    /// The tick of the next expiry: the next tick the wheel must be advanced to for a timer
    /// to fire or move down. `None` with no timer set.
    pub(super) fn next(&self) -> Option<u64> {
        for level in 0..LEVELS {
            let bits = self.occupied[level];
            if bits == 0 {
                continue;
            }
            let slot = bits.trailing_zeros() as u64;
            let shift = 6 * level as u32;
            let above = shift + 6;
            let base = match above {
                64.. => 0,
                _ => (self.now >> above) << above,
            };
            return Some(base | (slot << shift));
        }
        None
    }

    /// Every timer due at or before tick `to`, in the order they fall due (those of one tick
    /// in no order), added to `fired`.
    pub(super) fn advance(&mut self, to: u64, fired: &mut Vec<T>) {
        while let Some(at) = self.next() {
            if at > to {
                break;
            }
            let level = (0..LEVELS).find(|it| self.occupied[*it] != 0).expect("an occupied level");
            let slot = self.occupied[level].trailing_zeros() as usize;
            self.now = at;
            let mut id = self.heads[level][slot];
            self.heads[level][slot] = NONE;
            self.occupied[level] &= !(1 << slot);
            while id != NONE {
                let next = self.next[id as usize];
                self.place[id as usize] = NOWHERE;
                match self.due[id as usize] <= self.now {
                    true => {
                        fired.push(self.what[id as usize].expect("a set timer"));
                        self.release(id);
                    }
                    false => self.link(id),
                }
                id = next;
            }
        }
        self.now = self.now.max(to);
    }

    fn link(&mut self, id: u32) {
        let due = self.due[id as usize];
        let differ = due ^ self.now;
        let level = ((63 - differ.leading_zeros()) / 6) as usize;
        let slot = ((due >> (6 * level as u32)) & 63) as usize;
        let head = self.heads[level][slot];
        self.next[id as usize] = head;
        self.prev[id as usize] = NONE;
        if head != NONE {
            self.prev[head as usize] = id;
        }
        self.heads[level][slot] = id;
        self.occupied[level] |= 1 << slot;
        self.place[id as usize] = (level * SLOTS + slot) as u16;
    }

    fn unlink(&mut self, id: u32) {
        let place = self.place[id as usize] as usize;
        let (level, slot) = (place / SLOTS, place % SLOTS);
        let (prev, next) = (self.prev[id as usize], self.next[id as usize]);
        match prev {
            NONE => self.heads[level][slot] = next,
            prev => self.next[prev as usize] = next,
        }
        if next != NONE {
            self.prev[next as usize] = prev;
        }
        if self.heads[level][slot] == NONE {
            self.occupied[level] &= !(1 << slot);
        }
        self.place[id as usize] = NOWHERE;
    }

    fn release(&mut self, id: u32) {
        self.what[id as usize] = None;
        self.generation[id as usize] = self.generation[id as usize].wrapping_add(1);
        self.next[id as usize] = self.free;
        self.free = id;
        self.len -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a plain list of timers, over seeded inserts, removes and advances by steps
    /// from a tick to an hour: the same timers fire at each advance, each once, none early,
    /// and the next expiry is never past the earliest timer due.
    #[test]
    fn the_wheel_fires_what_a_list_would() {
        for seed in 1..=40u64 {
            let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let mut next = move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let start = next() % (1 << 40);
            let mut wheel: Wheel<u64> = Wheel::new(start);
            let mut now = start;
            let mut set: Vec<(u64, u64, u64)> = Vec::new();
            let mut fired = Vec::new();
            for step in 0..3_000u64 {
                match next() % 4 {
                    0 | 1 => {
                        let after = match next() % 3 {
                            0 => next() % 64,
                            1 => next() % 70_000,
                            _ => next() % 4_000_000,
                        };
                        let id = wheel.insert(now + after, step);
                        set.push((id, (now + after).max(now + 1), step));
                    }
                    2 if !set.is_empty() => {
                        let at = (next() % set.len() as u64) as usize;
                        wheel.remove(set.swap_remove(at).0);
                    }
                    _ => {
                        let by = match next() % 3 {
                            0 => next() % 3,
                            1 => next() % 5_000,
                            _ => next() % 3_600_000,
                        };
                        now += by;
                        fired.clear();
                        wheel.advance(now, &mut fired);
                        fired.sort_unstable();
                        let mut expected: Vec<u64> = set.iter().filter(|it| it.1 <= now).map(|it| it.2).collect();
                        expected.sort_unstable();
                        assert_eq!(fired, expected, "seed {seed} step {step}");
                        set.retain(|it| it.1 > now);
                    }
                }
                assert_eq!(wheel.len(), set.len(), "seed {seed}");
                let earliest = set.iter().map(|it| it.1).min();
                match (wheel.next(), earliest) {
                    (Some(at), Some(due)) => assert!(at > now && at <= due, "seed {seed}: {at} {now} {due}"),
                    (None, None) => {}
                    other => panic!("seed {seed}: {other:?}"),
                }
            }
        }
    }

    /// Removing a timer twice, or one that fired, is nothing; and the slab is reused, so a
    /// steady stream of timers makes no more than are set at once.
    #[test]
    fn timers_are_reused_and_a_stale_remove_is_nothing() {
        let mut wheel: Wheel<u32> = Wheel::new(0);
        let mut fired = Vec::new();
        let mut now = 0;
        for n in 0..10_000u32 {
            let a = wheel.insert(now + 60_000, n);
            let b = wheel.insert(now + 5, n);
            wheel.remove(a);
            wheel.remove(a);
            now += 10;
            fired.clear();
            wheel.advance(now, &mut fired);
            assert_eq!(fired, [n]);
            // `b` fired: its handle names nothing, though its slot is the next `a`'s.
            wheel.remove(b);
            wheel.remove(UNSET);
            let c = wheel.insert(now + 1, n);
            wheel.remove(b);
            assert_eq!(wheel.len(), 1);
            wheel.remove(c);
        }
        assert_eq!(wheel.len(), 0);
        assert!(wheel.made() <= 2, "{}", wheel.made());
        assert_eq!(wheel.next(), None);
    }
}
