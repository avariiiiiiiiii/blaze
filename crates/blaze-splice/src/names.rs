//! The names of an executable's symbols and imports, read once with the baseline and looked up
//! by every splice (docs/v2/plan/design/memory.md, "tables"): the bytes of every name in one
//! [`RegionVec`], the entries in another, and an open-addressed index of `u32` IDs over them,
//! all tagged [`Tag::Link`]. A name costs no allocation of its own, the table never moves, and
//! a lookup is one hash of the name, a probe or two and one compare of its bytes.
//!
//! The index is kept at most half full; past that it is built again twice as large from the
//! hashes the entries keep, into a region of its own, while the baseline is read.

use std::hash::BuildHasher as _;

use blaze_mem::{MemError, RegionVec, Tag};
use rustc_hash::FxBuildHasher;

/// The most bytes of names one table holds: an entry says where its name starts in a `u32`.
const NAME_BYTES: usize = u32::MAX as usize;

/// The most names one table holds.
const MAX_NAMES: usize = 1 << 25;

/// Index slots a table starts with at the least.
const MIN_SLOTS: usize = 16;

/// An empty slot of the index; any other is an entry's ID plus one.
const EMPTY: u32 = 0;

struct Entry<V> {
    hash: u64,
    start: u32,
    len: u32,
    value: V,
}

/// A map from names to `V`, for one owner.
pub(crate) struct Names<V> {
    bytes: RegionVec<u8>,
    entries: RegionVec<Entry<V>>,
    /// A power of two long, at most half full.
    slots: RegionVec<u32>,
}

fn hash(name: &[u8]) -> u64 {
    FxBuildHasher.hash_one(name)
}

/// `len` empty slots, in a region of their own.
fn empty_slots(len: usize) -> Result<RegionVec<u32>, MemError> {
    const ZEROS: [u32; 1024] = [EMPTY; 1024];
    let mut slots = RegionVec::new(len, Tag::Link)?;
    while slots.len() < len {
        let more = (len - slots.len()).min(ZEROS.len());
        slots.try_extend_from_slice(&ZEROS[..more])?;
    }
    Ok(slots)
}

impl<V> Names<V> {
    /// A table with index room for `expected` names before it is built again.
    pub(crate) fn with_capacity(expected: usize) -> Result<Names<V>, MemError> {
        let slots = expected
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or(MemError::TooLarge {
                tag: Tag::Link,
                what: "a table of names",
                wanted: expected,
            })?
            .max(MIN_SLOTS);
        Ok(Names {
            bytes: RegionVec::new(NAME_BYTES, Tag::Link)?,
            entries: RegionVec::new(MAX_NAMES, Tag::Link)?,
            slots: empty_slots(slots)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn name_of(&self, entry: &Entry<V>) -> &[u8] {
        let start = entry.start as usize;
        &self.bytes[start..start + entry.len as usize]
    }

    /// The ID of `name`, whose hash is `hash`, or the slot of the index it would go in.
    #[inline]
    fn find(&self, name: &[u8], hash: u64) -> Result<usize, usize> {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot == EMPTY {
                return Err(at);
            }
            let id = (slot - 1) as usize;
            let entry = &self.entries[id];
            if entry.hash == hash && self.name_of(entry) == name {
                return Ok(id);
            }
            at = (at + 1) & mask;
        }
    }

    #[inline]
    pub(crate) fn get(&self, name: &[u8]) -> Option<&V> {
        let id = self.find(name, hash(name)).ok()?;
        Some(&self.entries[id].value)
    }

    #[cfg(test)]
    #[inline]
    pub(crate) fn get_mut(&mut self, name: &[u8]) -> Option<&mut V> {
        let id = self.find(name, hash(name)).ok()?;
        Some(&mut self.entries[id].value)
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, name: &[u8]) -> bool {
        self.find(name, hash(name)).is_ok()
    }

    /// The value of `name`, and whether it was just put there: `value` when the table had no
    /// value for the name, which it then holds.
    pub(crate) fn get_or_insert(
        &mut self,
        name: &[u8],
        value: V,
    ) -> Result<(&mut V, bool), MemError> {
        self.get_or_insert_with(name, || value)
    }

    /// As [`Names::get_or_insert`], with the value made only when the name is new.
    pub(crate) fn get_or_insert_with(
        &mut self,
        name: &[u8],
        make: impl FnOnce() -> V,
    ) -> Result<(&mut V, bool), MemError> {
        let hash = hash(name);
        let mut at = match self.find(name, hash) {
            Ok(id) => return Ok((&mut self.entries[id].value, false)),
            Err(at) => at,
        };
        if (self.entries.len() + 1) * 2 > self.slots.len() {
            self.grow()?;
            at = match self.find(name, hash) {
                Ok(_) => unreachable!("building the index again keeps the names it had"),
                Err(at) => at,
            };
        }
        let too_large =
            |wanted| MemError::TooLarge { tag: Tag::Link, what: "the names of a table", wanted };
        let start = u32::try_from(self.bytes.len()).map_err(|_| too_large(self.bytes.len()))?;
        let len = u32::try_from(name.len()).map_err(|_| too_large(name.len()))?;
        self.bytes.try_extend_from_slice(name)?;
        let id = match self.entries.try_push(Entry { hash, start, len, value: make() }) {
            Ok(id) => id,
            Err(error) => {
                self.bytes.truncate(start as usize);
                return Err(error);
            }
        };
        self.slots[at] = id as u32 + 1;
        Ok((&mut self.entries[id].value, true))
    }

    /// Builds the index again twice as large.
    #[cold]
    fn grow(&mut self) -> Result<(), MemError> {
        let len = self.slots.len().checked_mul(2).ok_or(MemError::TooLarge {
            tag: Tag::Link,
            what: "the index of a table of names",
            wanted: self.slots.len(),
        })?;
        let mut slots = empty_slots(len)?;
        let mask = len - 1;
        for (id, entry) in self.entries.iter().enumerate() {
            let mut at = entry.hash as usize & mask;
            while slots[at] != EMPTY {
                at = (at + 1) & mask;
            }
            slots[at] = id as u32 + 1;
        }
        self.slots = slots;
        Ok(())
    }

    /// Every name with its value, in the order they went in.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u8], &V)> {
        self.entries.iter().map(|entry| (self.name_of(entry), &entry.value))
    }
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;

    use super::*;

    /// Names that share prefixes and lengths, as mangled ones do, and some that repeat.
    fn names(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|it| format!("_ZN5crate{}{}E", it % 97, "x".repeat(it % 13)).into_bytes())
            .collect()
    }

    #[test]
    fn it_is_a_map_of_names() {
        let mut model = FxHashMap::<Vec<u8>, u64>::default();
        let mut table = Names::<u64>::with_capacity(4).unwrap();
        for (nth, name) in names(5000).into_iter().enumerate() {
            let (value, new) = table.get_or_insert(&name, nth as u64).unwrap();
            let was = model.get(&name).copied();
            assert_eq!(new, was.is_none());
            match was {
                None => _ = model.insert(name.clone(), nth as u64),
                Some(was) => assert_eq!(*value, was),
            }
            if nth % 7 == 0 {
                *table.get_mut(&name).unwrap() += 1;
                *model.get_mut(&name).unwrap() += 1;
            }
        }
        assert_eq!(table.len(), model.len());
        for (name, value) in &model {
            assert_eq!(table.get(name), Some(value));
        }
        assert_eq!(table.iter().count(), model.len());
        for (name, value) in table.iter() {
            assert_eq!(model.get(name), Some(value));
        }
        assert!(!table.contains(b"_ZN5crate0E_not"));
        assert_eq!(table.get(b""), None);
        table.get_or_insert(b"", 7).unwrap();
        assert_eq!(table.get(b""), Some(&7));
    }

    #[test]
    fn a_value_is_made_only_for_a_new_name() {
        let mut table = Names::<u32>::with_capacity(0).unwrap();
        assert!(table.get_or_insert_with(b"a", || 1).unwrap().1);
        let (value, new) = table.get_or_insert_with(b"a", || unreachable!()).unwrap();
        assert_eq!((*value, new), (1, false));
    }

    #[test]
    fn a_lookup_allocates_nothing() {
        let names = names(20_000);
        let (table, used) = blaze_alloc_count::measure(|| {
            let mut table = Names::<u64>::with_capacity(16).unwrap();
            for (nth, name) in names.iter().enumerate() {
                table.get_or_insert(name, nth as u64).unwrap();
            }
            table
        });
        assert_eq!(used.allocations, 0, "a table of names is read into reserved regions");
        blaze_alloc_count::assert_at_most(0, "a lookup of a name", || {
            for name in &names {
                assert!(table.get(name).is_some());
            }
            assert!(table.get(b"_ZN3not5thereE").is_none());
        });
    }
}
