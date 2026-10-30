//! Items' long-lived data as columns (docs/v2/plan/design/data.md): a crate's item hashes one
//! column per hash kind, and facts as sorted `(kind, subject, object)` triples.
//!
//! A column is read alone: finding a key reads the key column only, and a fact range scan
//! reads the kind and subject columns only. Hashes that only some items have (a layout, a
//! code hash) sit densely in their own column, found through a presence bitset by rank.

use std::cmp::Ordering;

use base_db::Crate;
use blaze_ids::ContentId;
use blaze_query::QueryKind;
use hir_ty::db::HirDatabase;

use crate::{
    CodeHash, DeepHash, Fact, InterfaceHash, ItemHashes, ItemKey, ItemKind, LayoutHash, NameHash,
    SimplifiedType, Unhashable, hash, key, kinds,
};

const _: () = {
    assert!(size_of::<ItemKey>() == 32);
    assert!(size_of::<InterfaceHash>() == 32);
    assert!(size_of::<LayoutHash>() == 32);
    assert!(size_of::<CodeHash>() == 32);
    assert!(size_of::<DeepHash>() == 32);
    assert!(size_of::<ItemKind>() == 1);
};

/// One bit per row, with the count of set bits before each word, so a row's rank (how many
/// rows before it are set: where its value sits in a column of the set rows alone) is one
/// popcount.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Bits {
    words: Box<[u64]>,
    before: Box<[u32]>,
    len: u32,
}

impl Bits {
    pub fn new(bits: impl IntoIterator<Item = bool>) -> Bits {
        let mut words: Vec<u64> = Vec::new();
        let mut len = 0usize;
        for bit in bits {
            if len % 64 == 0 {
                words.push(0);
            }
            if bit {
                words[len / 64] |= 1 << (len % 64);
            }
            len += 1;
        }
        let mut before = Vec::with_capacity(words.len());
        let mut count = 0u32;
        for word in &words {
            before.push(count);
            count += word.count_ones();
        }
        let len = u32::try_from(len).expect("blaze-items: more than u32::MAX rows in a bitset");
        Bits { words: words.into(), before: before.into(), len }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[track_caller]
    pub fn get(&self, row: usize) -> bool {
        assert!(row < self.len(), "blaze-items: row {row} of a bitset of {}", self.len);
        self.words[row / 64] >> (row % 64) & 1 == 1
    }

    /// How many rows before `row` are set.
    #[track_caller]
    pub fn rank(&self, row: usize) -> usize {
        assert!(row < self.len(), "blaze-items: row {row} of a bitset of {}", self.len);
        let word = row / 64;
        let below = self.words[word] & ((1u64 << (row % 64)) - 1);
        self.before[word] as usize + below.count_ones() as usize
    }

    /// How many rows are set.
    pub fn count(&self) -> usize {
        match (self.before.last(), self.words.last()) {
            (Some(&before), Some(word)) => before as usize + word.count_ones() as usize,
            _ => 0,
        }
    }
}

/// The hashes of a set of items, sorted by key, one column per hash kind. The items that have
/// no hash are in a side table, by key with why.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HashColumns {
    keys: Box<[ItemKey]>,
    kinds: Box<[ItemKind]>,
    interface: Box<[InterfaceHash]>,
    deep: Box<[DeepHash]>,
    has_layout: Bits,
    layout: Box<[LayoutHash]>,
    has_code: Bits,
    code: Box<[CodeHash]>,
    unhashable: Box<[(ItemKey, Unhashable)]>,
}

impl HashColumns {
    /// The columns of `rows`, which may come in any order; a key given twice is a bug in the
    /// caller.
    pub fn new(
        mut rows: Vec<(ItemKey, ItemHashes)>,
        mut unhashable: Vec<(ItemKey, Unhashable)>,
    ) -> HashColumns {
        rows.sort_unstable_by_key(|&(key, _)| key);
        unhashable.sort_unstable_by_key(|&(key, _)| key);
        for pair in rows.windows(2) {
            assert_ne!(pair[0].0, pair[1].0, "blaze-items: two rows of one key");
        }
        HashColumns {
            keys: rows.iter().map(|&(key, _)| key).collect(),
            kinds: rows.iter().map(|(_, it)| it.kind).collect(),
            interface: rows.iter().map(|(_, it)| it.interface).collect(),
            deep: rows.iter().map(|(_, it)| it.deep).collect(),
            has_layout: Bits::new(rows.iter().map(|(_, it)| it.layout.is_some())),
            layout: rows.iter().filter_map(|(_, it)| it.layout).collect(),
            has_code: Bits::new(rows.iter().map(|(_, it)| it.code.is_some())),
            code: rows.iter().filter_map(|(_, it)| it.code).collect(),
            unhashable: unhashable.into(),
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn keys(&self) -> &[ItemKey] {
        &self.keys
    }

    /// The row of `key`, if it has hashes.
    pub fn find(&self, key: ItemKey) -> Option<usize> {
        self.keys.binary_search(&key).ok()
    }

    pub fn get(&self, key: ItemKey) -> Option<ItemHashes> {
        self.find(key).map(|row| self.row(row))
    }

    #[track_caller]
    pub fn row(&self, row: usize) -> ItemHashes {
        ItemHashes {
            kind: self.kinds[row],
            interface: self.interface[row],
            layout: self.has_layout.get(row).then(|| self.layout[self.has_layout.rank(row)]),
            code: self.has_code.get(row).then(|| self.code[self.has_code.rank(row)]),
            deep: self.deep[row],
        }
    }

    /// The items that have no hash, sorted by key.
    pub fn unhashable(&self) -> &[(ItemKey, Unhashable)] {
        &self.unhashable
    }

    /// The keys whose hashes differ from `old` to `self`: added, removed or changed, sorted;
    /// one merge over both key columns.
    pub fn changed_since(&self, old: &HashColumns) -> Vec<ItemKey> {
        let (mut a, mut b) = (0, 0);
        let mut changed = Vec::new();
        while a < old.len() || b < self.len() {
            let order = match (old.keys.get(a), self.keys.get(b)) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => Ordering::Less,
                (None, _) => Ordering::Greater,
            };
            match order {
                Ordering::Less => {
                    changed.push(old.keys[a]);
                    a += 1;
                }
                Ordering::Greater => {
                    changed.push(self.keys[b]);
                    b += 1;
                }
                Ordering::Equal => {
                    if old.row(a) != self.row(b) {
                        changed.push(self.keys[b]);
                    }
                    a += 1;
                    b += 1;
                }
            }
        }
        changed
    }
}

/// The hashes of every item of `krate` with an [`crate::ItemId`], bodies' items included, in
/// one pass: what the store keeps of a crate and diffs against the last build.
#[salsa::tracked(returns(ref))]
pub fn crate_hashes(db: &dyn HirDatabase, krate: Crate) -> HashColumns {
    let mut rows = Vec::new();
    let mut unhashable = Vec::new();
    for id in key::crate_item_ids(db, krate) {
        let key = key::item_key(db, id);
        match hash::hashes(db, id) {
            Ok(it) => rows.push((key, it)),
            Err(it) => unhashable.push((key, it)),
        }
    }
    HashColumns::new(rows, unhashable)
}

const NO_OBJECT: u32 = u32::MAX;

/// One fact: its kind's offset from [`kinds::SIGNATURE`], a subject and an object, each a
/// position in the set's dictionary of ids, except a field's object, which is its position.
pub const FACT_ROW_BYTES: usize = size_of::<u8>() + 2 * size_of::<u32>();

const _: () = assert!(FACT_ROW_BYTES == 9);

/// The facts a query read, distinct and sorted by `(kind, subject, object)`, as three columns
/// over a sorted dictionary of the ids they name: every fact of one kind about one subject is
/// one range, found by binary search.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FactSet {
    ids: Box<[ContentId]>,
    kinds: Box<[u8]>,
    subjects: Box<[u32]>,
    objects: Box<[u32]>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Object {
    None,
    Field(u32),
    Id(ContentId),
}

fn tag(kind: QueryKind) -> u8 {
    u8::try_from(kind.0 - kinds::SIGNATURE.0).expect("blaze-items: a fact kind past u8")
}

fn split(fact: &Fact) -> (u8, ContentId, Object) {
    let tag = tag(fact.kind());
    match *fact {
        Fact::Signature(it)
        | Fact::Layout(it)
        | Fact::Code(it)
        | Fact::ConstValue(it)
        | Fact::Name(it)
        | Fact::Captures(it)
        | Fact::AutoTraits(it)
        | Fact::Coherence(it)
        | Fact::Specializes(it)
        | Fact::DropCheck(it)
        | Fact::Derive(it) => (tag, it.0, Object::None),
        Fact::Field { ty, field } => (tag, ty.0, Object::Field(field)),
        Fact::Method { self_ty, name } => (tag, self_ty.0, Object::Id(name.0)),
        Fact::Implements { trait_, self_ty } | Fact::Vtable { trait_, self_ty } => {
            (tag, trait_.0, Object::Id(self_ty.0))
        }
        Fact::Resolves { scope, name } => (tag, scope.0, Object::Id(name.0)),
    }
}

fn join(tag: u8, subject: ContentId, object: Object) -> Fact {
    let item = ItemKey(subject);
    let id = || match object {
        Object::Id(it) => it,
        Object::None | Object::Field(_) => unreachable!("fact kind {tag} without an id object"),
    };
    match QueryKind(kinds::SIGNATURE.0 + u16::from(tag)) {
        kinds::SIGNATURE => Fact::Signature(item),
        kinds::LAYOUT => Fact::Layout(item),
        kinds::FIELD => match object {
            Object::Field(field) => Fact::Field { ty: item, field },
            Object::None | Object::Id(_) => unreachable!("a field fact without a field"),
        },
        kinds::METHOD => Fact::Method { self_ty: SimplifiedType(subject), name: NameHash(id()) },
        kinds::IMPLEMENTS => Fact::Implements { trait_: item, self_ty: SimplifiedType(id()) },
        kinds::RESOLVES => Fact::Resolves { scope: item, name: NameHash(id()) },
        kinds::CODE => Fact::Code(item),
        kinds::CONST_VALUE => Fact::ConstValue(item),
        kinds::NAME => Fact::Name(item),
        kinds::CAPTURES => Fact::Captures(item),
        kinds::AUTO_TRAITS => Fact::AutoTraits(item),
        kinds::COHERENCE => Fact::Coherence(item),
        kinds::SPECIALIZES => Fact::Specializes(item),
        kinds::DROP_CHECK => Fact::DropCheck(item),
        kinds::VTABLE => Fact::Vtable { trait_: item, self_ty: SimplifiedType(id()) },
        kinds::DERIVE => Fact::Derive(item),
        other => unreachable!("{other:?} is no fact kind"),
    }
}

impl FromIterator<Fact> for FactSet {
    fn from_iter<I: IntoIterator<Item = Fact>>(facts: I) -> FactSet {
        let mut split: Vec<(u8, ContentId, Object)> = facts.into_iter().map(|it| split(&it)).collect();
        let mut ids: Vec<ContentId> = Vec::with_capacity(split.len() * 2);
        for &(_, subject, object) in &split {
            ids.push(subject);
            if let Object::Id(it) = object {
                ids.push(it);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        assert!(ids.len() < NO_OBJECT as usize, "blaze-items: more than u32::MAX ids in a fact set");
        let at = |id: ContentId| ids.binary_search(&id).expect("every id is in the dictionary") as u32;
        // Positions in a sorted dictionary order as the ids do, so sorting the encoded rows
        // sorts the facts by content.
        split.sort_unstable();
        split.dedup();
        let kinds = split.iter().map(|&(tag, _, _)| tag).collect();
        let subjects = split.iter().map(|&(_, subject, _)| at(subject)).collect();
        let objects = split
            .iter()
            .map(|&(_, _, object)| match object {
                Object::None => NO_OBJECT,
                Object::Field(field) => field,
                Object::Id(it) => at(it),
            })
            .collect();
        FactSet { ids: ids.into(), kinds, subjects, objects }
    }
}

impl FactSet {
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    fn fact(&self, row: usize) -> Fact {
        let tag = self.kinds[row];
        let subject = self.ids[self.subjects[row] as usize];
        let object = match (QueryKind(kinds::SIGNATURE.0 + u16::from(tag)), self.objects[row]) {
            (kinds::FIELD, field) => Object::Field(field),
            (_, NO_OBJECT) => Object::None,
            (_, at) => Object::Id(self.ids[at as usize]),
        };
        join(tag, subject, object)
    }

    /// Every fact, sorted.
    pub fn iter(&self) -> impl Iterator<Item = Fact> + '_ {
        (0..self.len()).map(|row| self.fact(row))
    }

    /// The rows of facts of `kind` about `subject` (a fact's first id: the item, the
    /// simplified self type of a method, the trait of an impl question, the scope of a name).
    fn range(&self, kind: QueryKind, subject: ContentId) -> std::ops::Range<usize> {
        let Ok(subject) = self.ids.binary_search(&subject) else { return 0..0 };
        let at = (tag(kind), subject as u32);
        let start = self.partition(|row| row < at);
        let end = self.partition(|row| row <= at);
        start..end
    }

    fn partition(&self, before: impl Fn((u8, u32)) -> bool) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let mid = low + (high - low) / 2;
            if before((self.kinds[mid], self.subjects[mid])) {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    /// Every fact of `kind` about `subject`: what to re-check when that subject changes.
    pub fn about(&self, kind: QueryKind, subject: ContentId) -> impl Iterator<Item = Fact> + '_ {
        self.range(kind, subject).map(|row| self.fact(row))
    }

    pub fn contains(&self, fact: &Fact) -> bool {
        let (_, subject, _) = split(fact);
        self.range(fact.kind(), subject).any(|row| self.fact(row) == *fact)
    }
}
