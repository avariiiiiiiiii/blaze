//! The image's symbol table: what the workspace link resolves against.
//!
//! Every symbol is a 32-byte ID ([`SymbolId`]): a function by its instance key, a static by
//! its item and instance, a name by its bytes, each in its own hash domain. IDs are BLAKE3
//! outputs, so their first 8 bytes index an open-addressed table directly, with no further
//! hashing: a power-of-two count of 64-byte slots (one cache line each), at most half full,
//! linear probing. A lookup is one cache miss and a 32-byte compare, usually; a miss ends at
//! the first empty slot. The table is read in place from the image's bytes ([`TableView`]),
//! checked once when the image is opened.

use blaze_codegen::InstanceKey;
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;

use crate::PremapError;

pub const STATIC_SYMBOL_DOMAIN: &str = "blaze v2 premap symbol static 1";
pub const NAME_SYMBOL_DOMAIN: &str = "blaze v2 premap symbol name 1";

/// Bytes per slot: id 0..32, value 32..40, size 40..48, order 48..52, kind 52, strength 53.
pub const SLOT: usize = 64;

/// A symbol's ID in the table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SymbolId(pub ContentId);

impl SymbolId {
    pub fn function(key: &InstanceKey) -> SymbolId {
        SymbolId(key.0)
    }

    pub fn of_static(item: &ItemKey, instance: &ContentId) -> SymbolId {
        SymbolId(Hasher::new(STATIC_SYMBOL_DOMAIN).id(&item.0).id(instance).finish())
    }

    pub fn name(name: &str) -> SymbolId {
        SymbolId(Hasher::new(NAME_SYMBOL_DOMAIN).part(name.as_bytes()).finish())
    }
}

/// What a symbol is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Function,
    /// A static or a native datum: `value` is its address.
    Data,
    /// A thread-local: `value` is its offset from the thread pointer, as the image's code
    /// reaches it ([`crate::start::TlsBlock`]: the image's block is the first).
    ThreadLocal,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Strength {
    Strong,
    /// A weak definition: a strong one elsewhere wins; of two weak ones the first on the
    /// link line ([`Def::order`]).
    Weak,
}

/// A symbol's definition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Def {
    pub value: u64,
    pub size: u64,
    pub kind: Kind,
    pub strength: Strength,
    /// Its input's place on the link line: `u32::MAX` for Rust's own pieces, which sort after
    /// every native.
    pub order: u32,
}

/// Builds a table of `entries`: an error when an ID is twice in it, or is all zero (the
/// empty slot).
pub fn build(entries: &[(SymbolId, Def)]) -> Result<Vec<u8>, PremapError> {
    let slots = (entries.len() * 2).next_power_of_two().max(16);
    let mask = slots - 1;
    let mut out = vec![0u8; slots * SLOT];
    for (id, def) in entries {
        if id.0.0 == [0; 32] {
            return Err(PremapError::new("a symbol of the image has the empty ID".to_owned()));
        }
        let mut at = start(&id.0) as usize & mask;
        loop {
            let slot = &mut out[at * SLOT..(at + 1) * SLOT];
            if slot[..32] == [0; 32] {
                slot[..32].copy_from_slice(&id.0.0);
                slot[32..40].copy_from_slice(&def.value.to_le_bytes());
                slot[40..48].copy_from_slice(&def.size.to_le_bytes());
                slot[48..52].copy_from_slice(&def.order.to_le_bytes());
                slot[52] = kind_tag(def.kind);
                slot[53] = strength_tag(def.strength);
                break;
            }
            if slot[..32] == id.0.0 {
                return Err(PremapError::new(format!("the symbol {:?} is in the image twice", id.0)));
            }
            at = (at + 1) & mask;
        }
    }
    Ok(out)
}

/// A table read in place.
#[derive(Clone, Copy)]
pub struct TableView<'a> {
    bytes: &'a [u8],
    mask: usize,
}

impl<'a> TableView<'a> {
    /// Checks every slot once: a power-of-two count of whole slots, every kind and strength
    /// one there is, every ID in the slot a probe from its start reaches without passing an
    /// empty one.
    pub fn parse(bytes: &'a [u8]) -> Result<TableView<'a>, PremapError> {
        let bad = |what: String| Err(PremapError::new(format!("the image's symbol table {what}")));
        if bytes.len() % SLOT != 0 {
            return bad(format!("is {} bytes, not whole slots", bytes.len()));
        }
        let slots = bytes.len() / SLOT;
        if !slots.is_power_of_two() || slots < 16 {
            return bad(format!("has {slots} slots, not a power of two of at least 16"));
        }
        let view = TableView { bytes, mask: slots - 1 };
        let mut used = 0usize;
        for at in 0..slots {
            let slot = &bytes[at * SLOT..(at + 1) * SLOT];
            if slot[..32] == [0; 32] {
                if slot[32..] != [0; 32] {
                    return bad(format!("has an empty slot {at} with bytes in it"));
                }
                continue;
            }
            used += 1;
            if kind_of(slot[52]).is_none() || strength_of(slot[53]).is_none() || slot[54..] != [0; 10] {
                return bad(format!("has slot {at} of no kind there is"));
            }
            let id: [u8; 32] = slot[..32].try_into().expect("32 bytes");
            if view.find(&ContentId(id)) != Some(at) {
                return bad(format!("has slot {at} where no probe for its ID reaches"));
            }
        }
        if used * 2 > slots {
            return bad(format!("is more than half full ({used} of {slots})"));
        }
        Ok(view)
    }

    /// A table [`TableView::parse`] accepted before, with no checks.
    pub(crate) fn trusted(bytes: &'a [u8]) -> TableView<'a> {
        TableView { bytes, mask: bytes.len() / SLOT - 1 }
    }

    pub fn get(&self, id: &SymbolId) -> Option<Def> {
        let at = self.find(&id.0)?;
        let slot = &self.bytes[at * SLOT..(at + 1) * SLOT];
        Some(Def {
            value: u64::from_le_bytes(slot[32..40].try_into().expect("8 bytes")),
            size: u64::from_le_bytes(slot[40..48].try_into().expect("8 bytes")),
            order: u32::from_le_bytes(slot[48..52].try_into().expect("4 bytes")),
            kind: kind_of(slot[52]).expect("checked by parse"),
            strength: strength_of(slot[53]).expect("checked by parse"),
        })
    }

    /// Every symbol: for tests and for the run's bindings, never on a lookup path.
    pub fn iter(&self) -> impl Iterator<Item = (SymbolId, Def)> + '_ {
        (0..=self.mask).filter_map(move |at| {
            let slot = &self.bytes[at * SLOT..(at + 1) * SLOT];
            if slot[..32] == [0; 32] {
                return None;
            }
            let id = SymbolId(ContentId(slot[..32].try_into().expect("32 bytes")));
            Some((id, self.get(&id).expect("in the table")))
        })
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn find(&self, id: &ContentId) -> Option<usize> {
        let mut at = start(id) as usize & self.mask;
        for _ in 0..=self.mask {
            let slot = &self.bytes[at * SLOT..at * SLOT + 32];
            if slot == id.0 {
                return Some(at);
            }
            if slot == [0; 32] {
                return None;
            }
            at = (at + 1) & self.mask;
        }
        None
    }
}

fn start(id: &ContentId) -> u64 {
    u64::from_le_bytes(id.0[..8].try_into().expect("8 bytes"))
}

fn kind_tag(kind: Kind) -> u8 {
    match kind {
        Kind::Function => 1,
        Kind::Data => 2,
        Kind::ThreadLocal => 3,
    }
}

fn kind_of(tag: u8) -> Option<Kind> {
    match tag {
        1 => Some(Kind::Function),
        2 => Some(Kind::Data),
        3 => Some(Kind::ThreadLocal),
        _ => None,
    }
}

fn strength_tag(strength: Strength) -> u8 {
    match strength {
        Strength::Strong => 1,
        Strength::Weak => 2,
    }
}

fn strength_of(tag: u8) -> Option<Strength> {
    match tag {
        1 => Some(Strength::Strong),
        2 => Some(Strength::Weak),
        _ => None,
    }
}
