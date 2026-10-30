//! The memory the tables live in: a key index against a map of the same rows across growth
//! past its region's commit steps, the hash columns against their rows, and each table's warm
//! lookup and append held to its allocation budget.

use std::collections::HashMap;

use blaze_alloc_count::assert_at_most;
use blaze_ids::ContentId;
use test_fixture::WithFixture;

use super::{items, single};
use crate::{
    CodeHash, DeepHash, Fact, FactSet, InterfaceHash, ItemHashes, ItemKey, ItemKind, LayoutHash,
    Unhashable, crate_hashes, item_key,
    key::{self, KeyIndex, Row},
    table::{Bits, HashColumns},
};

/// One crate's first 9 key bytes, and another's.
const PREFIX: [u8; 9] = [7, 1, 2, 3, 4, 5, 6, 7, 0];
const OTHER: [u8; 9] = [9, 9, 9, 9, 9, 9, 9, 9, 0x80];

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// `count` keys under `prefix`, the rest of each from `seed`.
fn keys(count: usize, seed: u64, prefix: [u8; 9]) -> Vec<ItemKey> {
    let mut state = seed | 1;
    (0..count)
        .map(|_| {
            let mut bytes = [0; 32];
            bytes[..9].copy_from_slice(&prefix);
            for chunk in bytes[9..].chunks_mut(8) {
                chunk.copy_from_slice(&next(&mut state).to_le_bytes()[..chunk.len()]);
            }
            ItemKey(ContentId(bytes))
        })
        .collect()
}

fn index_of(rows: &[Row<u32>]) -> KeyIndex<u32> {
    let mut column = key::key_rows("a test's index");
    column.extend_from_slice(rows);
    KeyIndex::new(column)
}

#[test]
fn a_key_index_answers_as_a_map_of_its_rows() {
    // 40-byte rows: 50 000 of them commit past a region's first steps.
    for count in [0, 1, 2, 3, 64, 1000, 50_000] {
        let present = keys(count, count as u64, PREFIX);
        let mut rows: Vec<Row<u32>> = Vec::new();
        let mut model = HashMap::new();
        for (at, &key) in present.iter().enumerate() {
            rows.push((key, at as u32));
            model.insert(key, at as u32);
        }
        // A key given twice with what it names once is one row.
        rows.extend(present.iter().step_by(7).map(|&key| (key, model[&key])));
        let index = index_of(&rows);
        assert_eq!(index.rows().len(), model.len(), "{count} keys");
        for (key, &value) in &model {
            assert_eq!(index.get(key), Some(value), "{key:?} of {count}");
        }
        for key in keys(1000, !(count as u64), PREFIX).iter().chain(&keys(100, 5, OTHER)) {
            assert_eq!(index.get(key), model.get(key).copied(), "{key:?} of {count}");
        }
    }
}

#[test]
fn a_key_index_of_two_crates_and_their_inner_keys_finds_each() {
    let mut rows: Vec<Row<u32>> = Vec::new();
    for (n, prefix) in [PREFIX, OTHER].into_iter().enumerate() {
        rows.extend(keys(500, n as u64 + 11, prefix).into_iter().map(|key| (key, n as u32)));
    }
    let index = index_of(&rows);
    for &(key, value) in &rows {
        assert_eq!(index.get(&key), Some(value), "{key:?}");
    }
    let equal = index_of(&rows.iter().rev().copied().collect::<Vec<_>>());
    assert_eq!(index, equal, "the rows' order is not part of an index");
}

#[test]
#[should_panic(expected = "two items with one key")]
fn a_key_index_refuses_a_key_naming_two_things() {
    let key = keys(1, 3, PREFIX)[0];
    index_of(&[(key, 1), (key, 2)]);
}

fn hashes(n: u8) -> ItemHashes {
    ItemHashes {
        kind: ItemKind::Struct,
        interface: InterfaceHash(ContentId([n; 32])),
        layout: (n % 2 == 0).then_some(LayoutHash(ContentId([n ^ 1; 32]))),
        code: (n % 3 == 0).then_some(CodeHash(ContentId([n ^ 2; 32]))),
        deep: DeepHash(ContentId([n ^ 3; 32])),
    }
}

#[test]
fn hash_columns_answer_as_a_map_of_their_rows() {
    let all = keys(300, 17, PREFIX);
    let (hashed, unhashable) = all.split_at(290);
    let mut rows: Vec<(ItemKey, ItemHashes)> =
        hashed.iter().enumerate().map(|(n, &key)| (key, hashes(n as u8))).collect();
    let model: HashMap<ItemKey, ItemHashes> = rows.iter().copied().collect();
    let item = items(&single("struct S;").0)[0];
    let mut missing: Vec<(ItemKey, Unhashable)> =
        unhashable.iter().map(|&key| (key, Unhashable { item, what: "a test's" })).collect();
    let columns = HashColumns::new(&mut rows, &mut missing);
    assert_eq!(columns.len(), model.len());
    assert!(columns.keys().windows(2).all(|it| it[0] < it[1]));
    for (&key, &it) in &model {
        assert_eq!(columns.get(key), Some(it), "{key:?}");
    }
    for &key in unhashable {
        assert_eq!(columns.get(key), None, "{key:?}");
    }
    assert_eq!(columns.unhashable().len(), unhashable.len());
    assert!(columns.unhashable().windows(2).all(|it| it[0].0 < it[1].0));
}

#[test]
fn a_warm_lookup_allocates_nothing() {
    let present = keys(10_000, 23, PREFIX);
    let rows: Vec<Row<u32>> = present.iter().enumerate().map(|(n, &k)| (k, n as u32)).collect();
    let index = index_of(&rows);
    let absent = keys(100, 29, PREFIX);
    assert_at_most(0, "a key index lookup", || {
        for key in present.iter().chain(&absent) {
            std::hint::black_box(index.get(key));
        }
    });

    let set: FactSet = present[..100].iter().map(|&key| Fact::Signature(key)).collect();
    assert_at_most(0, "a fact set lookup", || {
        for &key in &present[..200] {
            std::hint::black_box(set.contains(&Fact::Signature(key)));
        }
    });

    let bits = Bits::new((0..1000).map(|it| it % 5 == 0));
    assert_at_most(0, "a bitset's rank", || {
        for row in 0..1000 {
            std::hint::black_box((bits.get(row), bits.rank(row)));
        }
    });
}

#[test]
fn an_append_past_the_high_water_mark_allocates_nothing() {
    let rows = keys(20_000, 31, PREFIX);
    let mut column = key::key_rows::<Row<u32>>("a test's index");
    let committed = column.capacity();
    assert_at_most(0, "appending to a key index's rows", || {
        for (n, &key) in rows.iter().enumerate() {
            column.push((key, n as u32));
        }
    });
    assert!(column.capacity() > committed, "the appends committed past the first step");
    // The rows stay where they were written: one allocation, the directory.
    let index = assert_at_most(1, "indexing the rows", || KeyIndex::new(column));
    assert_eq!(index.rows().len(), rows.len());
}

#[test]
fn a_warm_resolve_and_crate_hashes_allocate_nothing() {
    let (db, _) = single(
        r#"
pub struct S { x: u32 }
pub enum E { A(S), B }
pub trait Tr { fn m(&self) -> u32 { 0 } }
impl Tr for S {}
pub fn f(s: &S) -> u32 { let c = |y: u32| y + s.x; fn inner() {} c(1) }
"#,
    );
    let ids = items(&db);
    let keys: Vec<ItemKey> = ids.iter().map(|&id| item_key(&db, id)).collect();
    for &key in &keys {
        assert!(key::resolve(&db, key).is_some(), "{key:?}");
    }
    let columns = crate_hashes(&db, db.test_crate());
    assert_at_most(0, "a warm resolve", || {
        for &key in &keys {
            std::hint::black_box(key::resolve(&db, key));
        }
    });
    assert_at_most(0, "a warm hash column lookup", || {
        for &key in &keys {
            std::hint::black_box(columns.get(key));
        }
    });
}
