//! Generational slabs: a value's id is its slot and the slot's generation when it was put
//! there, so a lookup is one index and one compare, and an id kept past its value's removal
//! finds nothing (`slotmap`, `thunderdome`, a game engine's entity table).
//!
//! [`Slab`] hands out its own ids from a free list (last freed, first reused, so the slot is
//! still in cache), each packable into the low bits of a [`blaze_core::Tag`]. [`Table`] holds
//! values under ids handed out elsewhere (the session's `JobId`).

/// A value's place in a [`Slab`] or [`Table`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub slot: u32,
    pub generation: u32,
}

const _: () = assert!(std::mem::size_of::<Key>() == 8);

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

impl<T> Slot<T> {
    fn get(&self, generation: u32) -> Option<&T> {
        self.value.as_ref().filter(|_| self.generation == generation)
    }

    fn get_mut(&mut self, generation: u32) -> Option<&mut T> {
        self.value.as_mut().filter(|_| self.generation == generation)
    }
}

/// Values under ids it hands out: at most `2^slot_bits` at once, each id packing into
/// `slot_bits + generation_bits` bits. A generation wraps: an id is refused once its value is
/// removed until its slot has been reused `2^generation_bits` times.
pub struct Slab<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    len: u32,
    slot_bits: u32,
    generation_mask: u32,
}

impl<T> Slab<T> {
    pub fn new(slot_bits: u32, generation_bits: u32) -> Slab<T> {
        assert!(slot_bits <= 32 && generation_bits <= 32 && slot_bits + generation_bits <= 64);
        let generation_mask = if generation_bits == 32 { u32::MAX } else { (1 << generation_bits) - 1 };
        Slab { slots: Vec::new(), free: Vec::new(), len: 0, slot_bits, generation_mask }
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The id [`Slab::insert`] gives next; none once every slot is taken.
    pub fn next(&self) -> Option<Key> {
        if let Some(&slot) = self.free.last() {
            return Some(Key { slot, generation: self.slots[slot as usize].generation });
        }
        let slot = u32::try_from(self.slots.len()).ok()?;
        (u64::from(slot) < 1u64 << self.slot_bits).then_some(Key { slot, generation: 0 })
    }

    /// `value` in a free slot, its id; none, `value` given back, once every slot is taken.
    pub fn insert(&mut self, value: T) -> Result<Key, T> {
        let Some(key) = self.next() else { return Err(value) };
        match self.free.pop() {
            Some(slot) => self.slots[slot as usize].value = Some(value),
            None => self.slots.push(Slot { generation: 0, value: Some(value) }),
        }
        self.len += 1;
        Ok(key)
    }

    pub fn get(&self, key: Key) -> Option<&T> {
        self.slots.get(key.slot as usize)?.get(key.generation)
    }

    pub fn get_mut(&mut self, key: Key) -> Option<&mut T> {
        self.slots.get_mut(key.slot as usize)?.get_mut(key.generation)
    }

    /// The value of `key` taken out, its slot freed under the next generation.
    pub fn remove(&mut self, key: Key) -> Option<T> {
        let slot = self.slots.get_mut(key.slot as usize)?;
        slot.get(key.generation)?;
        let value = slot.value.take();
        slot.generation = slot.generation.wrapping_add(1) & self.generation_mask;
        self.free.push(key.slot);
        self.len -= 1;
        value
    }

    /// `key` as `slot | generation << slot_bits`.
    pub fn pack(&self, key: Key) -> u64 {
        u64::from(key.slot) | u64::from(key.generation) << self.slot_bits
    }

    /// The id [`Slab::pack`] made `packed` of; none for bits no id of this slab has.
    pub fn unpack(&self, packed: u64) -> Option<Key> {
        let slot_mask = (1u64 << self.slot_bits) - 1;
        let generation = packed >> self.slot_bits;
        if generation > u64::from(self.generation_mask) {
            return None;
        }
        Some(Key { slot: (packed & slot_mask) as u32, generation: generation as u32 })
    }

    /// Every value, by slot.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Key, &mut T)> {
        (0u32..).zip(&mut self.slots).filter_map(|(slot, it)| {
            let generation = it.generation;
            it.value.as_mut().map(|value| (Key { slot, generation }, value))
        })
    }
}

/// Values under ids handed out elsewhere: a value's slot is its id's, and the one who hands
/// them out puts a slot's next value in only once its last one is removed.
pub struct Table<T> {
    slots: Vec<Slot<T>>,
    len: u32,
}

impl<T> Table<T> {
    pub fn new() -> Table<T> {
        Table { slots: Vec::new(), len: 0 }
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `value` under `key`; given back while its slot holds another value, or holds a later
    /// generation than `key`'s (a stale id).
    pub fn put(&mut self, key: Key, value: T) -> Result<(), T> {
        let at = key.slot as usize;
        if at >= self.slots.len() {
            self.slots.resize_with(at + 1, || Slot { generation: 0, value: None });
        }
        let slot = &mut self.slots[at];
        if slot.value.is_some() || slot.generation > key.generation {
            return Err(value);
        }
        *slot = Slot { generation: key.generation, value: Some(value) };
        self.len += 1;
        Ok(())
    }

    pub fn get(&self, key: Key) -> Option<&T> {
        self.slots.get(key.slot as usize)?.get(key.generation)
    }

    pub fn get_mut(&mut self, key: Key) -> Option<&mut T> {
        self.slots.get_mut(key.slot as usize)?.get_mut(key.generation)
    }

    pub fn remove(&mut self, key: Key) -> Option<T> {
        let slot = self.slots.get_mut(key.slot as usize)?;
        slot.get(key.generation)?;
        self.len -= 1;
        slot.value.take()
    }

    /// Every value, by slot.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|it| it.value.as_ref())
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|it| it.value.as_mut())
    }
}

impl<T> Default for Table<T> {
    fn default() -> Table<T> {
        Table::new()
    }
}
