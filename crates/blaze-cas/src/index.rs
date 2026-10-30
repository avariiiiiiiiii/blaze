//! The index table (docs/v2-store.md, "On disk"): an open-addressed table on keys, mapped and
//! read in place. A slot's number is the record's place in the store's flag columns, so it is
//! part of what `get` and `iter` give.

use crate::{
    Key, Kind,
    disk::{Form, Loc, id_at, u32_at, u64_at},
};

const INDEX_MAGIC: [u8; 8] = *b"BLZINDX\x03";
pub(crate) const INDEX_HEADER: usize = 64;
pub(crate) const SLOT: usize = 64;
const EMPTY: u32 = u32::MAX;
/// At most this share of an index's slots are full, so a lookup probes one or two lines.
const LOAD_NUMERATOR: u64 = 5;
const LOAD_DENOMINATOR: u64 = 8;
const LEAST_SLOTS: u64 = 1024;

/// An index table: a header, then a power of two of [`SLOT`]-byte slots, open-addressed with
/// linear probing from the key's first eight bytes (a BLAKE3 hash is uniform already).
pub(crate) fn build_index(entries: &[(Key, Loc)]) -> Vec<u8> {
    let mut slots = LEAST_SLOTS;
    while (entries.len() as u64) * LOAD_DENOMINATOR > slots * LOAD_NUMERATOR {
        slots *= 2;
    }
    let mut out = vec![0; INDEX_HEADER + slots as usize * SLOT];
    out[..8].copy_from_slice(&INDEX_MAGIC);
    out[8..12].copy_from_slice(&crate::FORMAT.to_le_bytes());
    out[16..24].copy_from_slice(&slots.to_le_bytes());
    out[24..32].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    let table = &mut out[INDEX_HEADER..];
    for at in 0..slots as usize {
        table[at * SLOT + 32..at * SLOT + 36].copy_from_slice(&EMPTY.to_le_bytes());
    }
    let mask = slots - 1;
    for (key, loc) in entries {
        let mut at = start(key) & mask;
        loop {
            let slot = &mut table[at as usize * SLOT..(at as usize + 1) * SLOT];
            if u32_at(slot, 32) == EMPTY {
                slot[..32].copy_from_slice(&key.0.0);
                slot[32..36].copy_from_slice(&loc.pack.to_le_bytes());
                slot[36] = loc.kind.code();
                slot[37] = loc.form.code();
                slot[40..48].copy_from_slice(&loc.offset.to_le_bytes());
                slot[48..56].copy_from_slice(&loc.stored.to_le_bytes());
                slot[56..64].copy_from_slice(&loc.raw.to_le_bytes());
                break;
            }
            at = (at + 1) & mask;
        }
    }
    out
}

fn start(key: &Key) -> u64 {
    u64_at(&key.0.0, 0)
}

/// A mapped index table, its header checked.
#[derive(Clone, Copy)]
pub(crate) struct Table<'a> {
    slots: &'a [u8],
    mask: u64,
    count: u64,
}

impl<'a> Table<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Result<Table<'a>, &'static str> {
        if bytes.len() < INDEX_HEADER || bytes[..8] != INDEX_MAGIC {
            return Err("it does not start as an index of this store's format does");
        }
        if u32_at(bytes, 8) != crate::FORMAT {
            return Err("it is of another format of the store");
        }
        let slots = u64_at(bytes, 16);
        let count = u64_at(bytes, 24);
        if !slots.is_power_of_two() || count >= slots {
            return Err("its header gives an impossible number of slots");
        }
        let len = slots.checked_mul(SLOT as u64).and_then(|it| it.checked_add(INDEX_HEADER as u64));
        if len != Some(bytes.len() as u64) {
            return Err("its length is not what its header says");
        }
        Ok(Table { slots: &bytes[INDEX_HEADER..], mask: slots - 1, count })
    }

    /// How many records it holds.
    pub(crate) fn count(&self) -> u64 {
        self.count
    }

    /// How many slots it has: every slot number is below this.
    pub(crate) fn slots(&self) -> u64 {
        self.mask + 1
    }

    fn slot(&self, at: u64) -> &'a [u8] {
        &self.slots[at as usize * SLOT..(at as usize + 1) * SLOT]
    }

    /// The slot of `key` and its record's place.
    pub(crate) fn get(&self, key: &Key) -> Option<(u64, Result<Loc, &'static str>)> {
        let mut at = start(key) & self.mask;
        for _ in 0..=self.mask {
            let slot = self.slot(at);
            if u32_at(slot, 32) == EMPTY {
                return None;
            }
            if slot[..32] == key.0.0 {
                return Some((at, decode_slot(slot)));
            }
            at = (at + 1) & self.mask;
        }
        None
    }

    /// Every full slot, in slot order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u64, Key, Result<Loc, &'static str>)> + 'a {
        let table = *self;
        (0..=self.mask).filter_map(move |at| {
            let slot = table.slot(at);
            (u32_at(slot, 32) != EMPTY).then(|| (at, Key(id_at(slot, 0)), decode_slot(slot)))
        })
    }
}

fn decode_slot(slot: &[u8]) -> Result<Loc, &'static str> {
    Ok(Loc {
        pack: u32_at(slot, 32),
        kind: Kind::from_code(slot[36]).ok_or("an index slot names no kind")?,
        form: Form::from_code(slot[37]).ok_or("an index slot names no form")?,
        offset: u64_at(slot, 40),
        stored: u64_at(slot, 48),
        raw: u64_at(slot, 56),
    })
}
