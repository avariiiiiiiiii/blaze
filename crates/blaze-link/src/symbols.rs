//! The image's symbol table: append-only, sharded by the key's hash so that interning the
//! symbols of many pieces runs on every core, one shard per task.

use std::hash::{Hash, Hasher};

use blaze_codegen::InstanceKey;
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHasher};

use crate::model::Sym;
use crate::reloc::GotKind;

/// A native object: an input, and its member when the input is an archive (0 otherwise).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct Obj {
    pub native: ContentId,
    pub member: u32,
}

/// What a symbol is. Keys order deterministically: placement sorts pieces by them.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum SymKey {
    /// A function's code.
    Body(InstanceKey),
    /// A function's jump-table stub: what its callers call.
    Slot(InstanceKey),
    /// Data a function's code refers to, by its contents.
    Data(ContentId),
    Static(ItemKey, ContentId),
    /// The Mach-O TLV descriptor of a thread-local static.
    Tlv(ItemKey, ContentId),
    /// A name the image's pieces and native objects share: an extern, an export, a native
    /// global.
    Name(Box<str>),
    /// A native object's local symbol, by its index.
    Local(Obj, u32),
    /// A native object's section, by its index.
    Section(Obj, u32),
    /// The `core::panic::Location` of a span.
    Position(SpanRef),
    /// A source file's name, NUL-terminated, for the locations of its spans.
    FileName(Box<str>),
    /// A function's CIE (by its bytes and personality) and FDE.
    Cie(ContentId),
    Fde(InstanceKey),
    /// A function's Windows `UNWIND_INFO` and its language-specific data.
    WinInfo(InstanceKey),
    /// A native object's call frame record: its section, and the byte it starts at in it.
    Frame(Obj, u32, u32),
    /// A GOT cell of a symbol, of a kind.
    Got(GotKind, Box<SymKey>),
    /// A PLT stub of an import.
    Plt(Box<SymKey>),
    /// A tentative definition merged by name.
    Common(Box<str>),
    /// The cell holding the address of the current jump table.
    Generation,
    /// The jump table's cells.
    Cells,
    /// The jump table's stubs.
    Stubs,
    /// The live descriptor, and the path of the directory it names.
    Descriptor,
    LiveDir,
}

const SHARDS: usize = 16;
const SHIFT: u32 = 28;

#[derive(Clone, Default)]
struct Shard {
    map: FxHashMap<SymKey, u32>,
    keys: Vec<SymKey>,
}

#[derive(Clone)]
pub(crate) struct Symbols {
    shards: Vec<Shard>,
}

impl Default for Symbols {
    fn default() -> Self {
        Symbols { shards: vec![Shard::default(); SHARDS] }
    }
}

fn shard_of(key: &SymKey) -> usize {
    let mut hasher = FxHasher::default();
    key.hash(&mut hasher);
    (hasher.finish() >> 32) as usize % SHARDS
}

fn sym(shard: usize, local: u32) -> Sym {
    Sym(((shard as u32) << SHIFT) | local)
}

impl Symbols {
    pub fn intern(&mut self, key: SymKey) -> Sym {
        let shard = shard_of(&key);
        let it = &mut self.shards[shard];
        if let Some(local) = it.map.get(&key) {
            return sym(shard, *local);
        }
        let local = it.keys.len() as u32;
        assert!(local < 1 << SHIFT, "more than 2^28 symbols in one shard");
        it.keys.push(key.clone());
        it.map.insert(key, local);
        sym(shard, local)
    }

    /// Interns every key, the shards in parallel; the symbols in the keys' order.
    pub fn intern_all(&mut self, keys: Vec<SymKey>) -> Vec<Sym> {
        let mut by_shard: Vec<Vec<(usize, SymKey)>> = vec![Vec::new(); SHARDS];
        let count = keys.len();
        for (at, key) in keys.into_iter().enumerate() {
            by_shard[shard_of(&key)].push((at, key));
        }
        let found: Vec<Vec<(usize, Sym)>> = self
            .shards
            .par_iter_mut()
            .zip(by_shard)
            .enumerate()
            .map(|(shard, (it, keys))| {
                keys.into_iter()
                    .map(|(at, key)| {
                        let local = match it.map.get(&key) {
                            Some(local) => *local,
                            None => {
                                let local = it.keys.len() as u32;
                                assert!(local < 1 << SHIFT, "more than 2^28 symbols in one shard");
                                it.keys.push(key.clone());
                                it.map.insert(key, local);
                                local
                            }
                        };
                        (at, sym(shard, local))
                    })
                    .collect()
            })
            .collect();
        let mut out = vec![Sym(0); count];
        for (at, sym) in found.into_iter().flatten() {
            out[at] = sym;
        }
        out
    }

    pub fn find(&self, key: &SymKey) -> Option<Sym> {
        let shard = shard_of(key);
        self.shards[shard].map.get(key).map(|local| sym(shard, *local))
    }

    pub fn key(&self, sym: Sym) -> &SymKey {
        &self.shards[(sym.0 >> SHIFT) as usize].keys[(sym.0 & ((1 << SHIFT) - 1)) as usize]
    }

    /// Where a symbol's per-symbol state is kept: its shard and its index in it.
    pub fn split(sym: Sym) -> (usize, usize) {
        ((sym.0 >> SHIFT) as usize, (sym.0 & ((1 << SHIFT) - 1)) as usize)
    }

    pub const SHARDS: usize = SHARDS;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_idempotent_and_parallel_interning_agrees() {
        let mut symbols = Symbols::default();
        let a = symbols.intern(SymKey::Name("a".into()));
        assert_eq!(symbols.intern(SymKey::Name("a".into())), a);
        let keys: Vec<SymKey> = (0..500).map(|n| SymKey::Name(format!("n{n}").into())).collect();
        let syms = symbols.intern_all(keys.clone());
        let again = symbols.intern_all(keys.clone());
        assert_eq!(syms, again);
        for (key, sym) in keys.iter().zip(&syms) {
            assert_eq!(symbols.key(*sym), key);
            assert_eq!(symbols.find(key), Some(*sym));
        }
        assert_eq!(symbols.key(a), &SymKey::Name("a".into()));
        assert_eq!(symbols.find(&SymKey::Name("none".into())), None);
    }
}
