use std::{collections::HashMap, panic::AssertUnwindSafe};

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_image::{Builder, Image, SectionId};

use super::*;
use crate::order_key;

/// Compiled in: "a" twice, so the first is the one interning gives.
const STATICS: [&str; 3] = ["a", "b", "a"];
const SCHEMA: SchemaHash = SchemaHash(ContentId([7; 32]));
const ID: SectionId = SectionId::SYMBOLS;

fn shape() -> Shape {
    Shape {
        statics: STATICS.len() as u32,
        static_bytes: |id| STATICS[id as usize - 1].as_bytes(),
        key: order_key,
        align: 1,
        size: 1,
        text: true,
    }
}

/// A splitmix64 stream: seeded, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A short name from few letters, so many share an order key and a hash tag.
    fn name(&mut self) -> String {
        let len = self.below(12) as usize;
        (0..len).map(|_| ['a', 'b', 'é', '_', '0'][self.below(5) as usize]).collect()
    }
}

fn section_bytes(table: &Table, live: &Live) -> Vec<u8> {
    let mut builder = Builder::new();
    table.write(live, &mut builder.section(ID, SCHEMA));
    let image = Image::open(builder.finish().unwrap()).unwrap();
    let section = image.section(ID, SCHEMA).unwrap();
    image.whole(&section).unwrap().to_vec()
}

fn image_of(section: &[u8]) -> &'static Image<Vec<u8>> {
    let mut builder = Builder::new();
    builder.add(ID, SCHEMA, section);
    Box::leak(Box::new(Image::open(builder.finish().unwrap()).unwrap()))
}

fn reopen(table: &Table, live: &Live) -> Result<Table, RestoreError> {
    let image = image_of(&section_bytes(table, live));
    Table::restore(shape(), image, &image.section(ID, SCHEMA).unwrap(), &serial)
}

fn everything(table: &Table) -> Live {
    let mut live = Live::new();
    (1..table.end()).for_each(|id| live.mark_id(id));
    live
}

/// Every value of `model` reads back at its ID from `table`, and every other ID below `end`
/// names nothing.
fn check(table: &Table, model: &HashMap<u32, String>, end: u32) {
    for id in table.first..end {
        match model.get(&id) {
            Some(text) => {
                assert_eq!(table.get(id), Some(text.as_bytes()), "ID {id}");
                assert_eq!(table.find(text.as_bytes()), Some(id), "{text:?}");
                assert_eq!(table.intern(text.as_bytes()), id, "{text:?}");
                assert_eq!(table.key(id), order_key(text.as_bytes()));
            }
            None => assert_eq!(table.get(id), None, "ID {id} was freed"),
        }
    }
    assert_eq!(table.find(b"a"), Some(1));
    assert_eq!(table.find(b"b"), Some(2));
    assert_eq!(table.get(3), Some(&b"a"[..]));
    assert_eq!(table.get(0), None);
}

#[test]
fn compiled_in_values_are_their_ids() {
    let table = Table::new(shape());
    assert_eq!(table.intern(b"a"), 1, "a value compiled in twice is the first");
    assert_eq!(table.intern(b"b"), 2);
    assert_eq!(table.intern(b"c"), 4);
    assert_eq!(table.end(), 5);
    let restored = reopen(&table, &Live::new()).unwrap();
    assert_eq!(restored.intern(b"a"), 1);
    assert_eq!(restored.intern(b"b"), 2);
    assert_eq!(restored.get(4), None, "unmarked, so freed");
    assert_eq!(restored.intern(b"d"), 4, "a freed ID goes to the next new value");
}

#[test]
fn ids_are_equal_after_write_and_reopen() {
    let mut rng = Rng(0x1d5);
    let table = Table::new(shape());
    let mut model = HashMap::new();
    for _ in 0..3000 {
        let text = rng.name() + &rng.below(400).to_string();
        let id = table.intern(text.as_bytes());
        if id >= table.first {
            let previous = model.insert(id, text.clone());
            assert!(previous.is_none_or(|previous| previous == text), "ID {id} given twice");
        }
    }
    let mut current = table;
    for generation in 0..3 {
        let end = current.end();
        let next = reopen(&current, &everything(&current)).unwrap();
        check(&next, &model, end);
        let text = format!("made in generation {generation}");
        let id = next.intern(text.as_bytes());
        assert_eq!(id, end, "nothing was freed, so a new value takes the next ID");
        model.insert(id, text);
        current = next;
    }
}

#[test]
fn gc_never_renumbers_a_live_value() {
    let mut rng = Rng(0x6c);
    let mut current = Table::new(shape());
    let mut model: HashMap<u32, String> = HashMap::new();
    for generation in 0..8 {
        for _ in 0..rng.below(600) {
            let text = format!("{}{generation}", rng.name());
            let id = current.intern(text.as_bytes());
            if id >= current.first {
                model.insert(id, text);
            }
        }
        let mut live = Live::new();
        model.retain(|&id, _| {
            let keep = rng.below(3) != 0;
            if keep {
                live.mark_id(id);
            }
            keep
        });
        let end = current.end();
        let next = reopen(&current, &live).unwrap();
        check(&next, &model, end);

        // A new value takes a freed ID first, lowest first, then the next: never a live one.
        let mut freed: Vec<u32> = (next.first..end).filter(|id| !model.contains_key(id)).collect();
        freed.sort_unstable();
        for (at, &expect) in freed.iter().chain([end, end + 1].iter()).enumerate() {
            let text = format!("new {generation} {at}");
            let id = next.intern(text.as_bytes());
            assert_eq!(id, expect);
            assert!(model.insert(id, text).is_none(), "ID {id} was live");
        }
        current = next;
    }
}

#[test]
fn a_freed_value_made_again_takes_a_new_id() {
    let table = Table::new(shape());
    let (kept, dropped) = (table.intern(b"kept"), table.intern(b"dropped"));
    let mut live = Live::new();
    live.mark_id(kept);
    let restored = reopen(&table, &live).unwrap();
    assert_eq!(restored.find(b"dropped"), None);
    assert_eq!(restored.find(b"kept"), Some(kept));
    let again = restored.intern(b"dropped");
    assert_eq!(again, dropped, "the only freed ID, so the first new value's");
    assert_eq!(restored.intern(b"third"), restored.end() - 1);
}

#[test]
fn a_lost_id_panics() {
    let table = Table::new(shape());
    let dropped = table.intern(b"dropped");
    let restored = reopen(&table, &Live::new()).unwrap();
    let lost = std::panic::catch_unwind(AssertUnwindSafe(|| restored.bytes(dropped)));
    assert!(lost.is_err(), "reading a freed ID is a bug, never a quiet empty value");
    let lost = std::panic::catch_unwind(AssertUnwindSafe(|| restored.key(dropped)));
    assert!(lost.is_err());
}

#[test]
fn a_restored_table_interns_from_many_threads() {
    let table = Table::new(shape());
    let ids: Vec<u32> = (0..2000).map(|n| table.intern(format!("old {n}").as_bytes())).collect();
    let mut live = Live::new();
    ids.iter().step_by(2).for_each(|&id| live.mark_id(id));
    let restored = reopen(&table, &live).unwrap();
    let seen: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8u32)
            .map(|worker| {
                let restored = &restored;
                scope.spawn(move || {
                    (0..3000u32)
                        .map(|n| (n + worker * 331) % 3000)
                        .map(|n| restored.intern(format!("new {n}").as_bytes()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers.into_iter().map(|it| it.join().unwrap()).collect()
    });
    let mut owner = HashMap::new();
    for (worker, ids) in seen.iter().enumerate() {
        for (i, &id) in ids.iter().enumerate() {
            let n = (i as u32 + worker as u32 * 331) % 3000;
            assert_eq!(*owner.entry(id).or_insert(n), n, "ID {id} given to two values");
            assert!(!live.contains(id), "ID {id} is live");
            assert_eq!(restored.get(id), Some(format!("new {n}").as_bytes()));
        }
    }
    for &id in ids.iter().step_by(2) {
        assert!(restored.get(id).is_some());
    }
}

#[test]
fn a_corrupt_table_is_dropped_whole() {
    let table = Table::new(shape());
    (0..500).for_each(|n| _ = table.intern(format!("value {n}").as_bytes()));
    let mut builder = Builder::new();
    table.write(&everything(&table), &mut builder.section(ID, SCHEMA));
    let mut file = builder.finish().unwrap();
    let image = Image::open(file.clone()).unwrap();
    let section = image.section(ID, SCHEMA).unwrap();
    // The last byte of the section's values, far from its root.
    file[(section.at + section.len - 1) as usize] ^= 0x20;
    let image: &'static Image<Vec<u8>> = Box::leak(Box::new(Image::open(file).unwrap()));
    match Table::restore(shape(), image, &section, &serial) {
        Err(RestoreError::Corrupt(corrupt)) => assert_eq!(corrupt.section, ID),
        other => panic!("expected a corrupt section, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn another_shape_is_refused() {
    let section = section_bytes(&Table::new(shape()), &Live::new());
    let image = image_of(&section);
    let other = Shape { statics: 2, ..shape() };
    let refused = Table::restore(other, image, &image.section(ID, SCHEMA).unwrap(), &serial);
    assert!(matches!(refused, Err(RestoreError::Invalid { section: ID, .. })));
}

/// Whatever bytes a section holds, restoring it is an error or a table whose every read is in
/// bounds and whose interning stays sound: the checks of `validate` leave nothing for a read to
/// trip on. (That each value is reachable in the index is the CRC's to guard, not these.)
#[test]
fn a_restore_never_accepts_a_table_it_cannot_read() {
    let mut rng = Rng(0xbad);
    let table = Table::new(shape());
    (0..40).for_each(|_| _ = table.intern(rng.name().as_bytes()));
    let mut live = Live::new();
    (table.first..table.end()).filter(|id| id % 3 != 0).for_each(|id| live.mark_id(id));
    let section = section_bytes(&table, &live);
    let (mut accepted, mut refused) = (0, 0);
    for _ in 0..4000 {
        let mut bytes = section.clone();
        for _ in 0..1 + rng.below(3) {
            // Mostly the root, where one bit moves a whole column.
            let at = match rng.below(2) {
                0 => rng.below(size_of::<Root>() as u64),
                _ => rng.below(bytes.len() as u64),
            } as usize;
            bytes[at] ^= 1 << rng.below(8);
        }
        let image = image_of(&bytes);
        match Table::restore(shape(), image, &image.section(ID, SCHEMA).unwrap(), &serial) {
            Ok(restored) => {
                accepted += 1;
                for id in 0..restored.end() + 2 {
                    if let Some(value) = restored.get(id) {
                        assert!(std::str::from_utf8(value).is_ok());
                        let _ = restored.key(id);
                        let again = restored.intern(value);
                        assert_eq!(restored.get(again), Some(value));
                    }
                }
                let id = restored.intern(b"made after a strange restore");
                assert!(restored.get(id).is_some());
            }
            Err(RestoreError::Invalid { .. } | RestoreError::Corrupt(_)) => refused += 1,
            Err(other) => panic!("{other}"),
        }
    }
    assert!(accepted > 0 && refused > 0, "{accepted} accepted, {refused} refused");
}

#[test]
fn live_sets() {
    let mut live = Live::new();
    assert!(!live.contains(0) && !live.contains(1_000_000));
    for id in [1, 63, 64, 65, 4095, 1_000_000] {
        live.mark_id(id);
        live.mark_id(id);
    }
    assert_eq!(live.count(), 6);
    assert!(live.contains(64) && live.contains(1_000_000) && !live.contains(66));
}

/// The index's hash is the image's format: it may not depend on the process, the build or
/// the platform, so it is checked against itself on fixed inputs from two computations.
#[test]
fn value_hash_is_deterministic_and_spreads() {
    let names: Vec<Vec<u8>> = (0..20_000u32).map(|n| format!("name {n}").into_bytes()).collect();
    let hashes: Vec<u64> = names.iter().map(|name| value_hash(name)).collect();
    assert_eq!(hashes, names.iter().map(|name| value_hash(&name.clone())).collect::<Vec<_>>());
    let mut distinct = hashes.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), hashes.len());
    // Every tag value is used, and the low bits (the probe start) spread over 1024 groups.
    let mut tags = [0u32; 128];
    let mut starts = vec![0u32; 1024];
    for &hash in &hashes {
        tags[(tag(hash) & 0x7f) as usize] += 1;
        starts[hash as usize & 1023] += 1;
    }
    assert!(tags.iter().all(|&n| n > 0));
    assert!(starts.iter().all(|&n| n < 60), "{:?}", starts.iter().max());
    assert_ne!(value_hash(b""), value_hash(b"\0"));
    assert_ne!(value_hash(b"ab"), value_hash(b"ba"));
}

/// Four workers taking every fourth job, in no fixed order.
fn four_threads(n: usize, job: &(dyn Fn(usize) + Sync)) {
    std::thread::scope(|scope| {
        for worker in 0..4 {
            scope.spawn(move || (worker..n).rev().step_by(4).for_each(job));
        }
    });
}

/// A restore split across workers gives the table and the error one core gives: the first
/// corrupt group, wherever the workers were.
#[test]
fn a_parallel_restore_is_the_serial_one() {
    let table = Table::new(shape());
    (0..200_000u32).for_each(|n| _ = table.intern(format!("symbol number {n}").as_bytes()));
    let mut builder = Builder::new();
    table.write(&everything(&table), &mut builder.section(ID, SCHEMA));
    let file = builder.finish().unwrap();
    let image: &'static Image<Vec<u8>> = Box::leak(Box::new(Image::open(file.clone()).unwrap()));
    let section = image.section(ID, SCHEMA).unwrap();
    assert!(section.groups as usize > 4 * CHUNK_GROUPS, "{} groups", section.groups);
    let parallel = Table::restore(shape(), image, &section, &four_threads).unwrap();
    for id in (parallel.first..parallel.end()).step_by(997) {
        assert_eq!(parallel.get(id), table.get(id));
    }

    let mut corrupt = file;
    let group_len = image.group_len() as usize;
    for group in [3, section.groups as usize - 2] {
        corrupt[section.at as usize + group * group_len + 17] ^= 0x40;
    }
    let serial_error = {
        let image: &'static Image<Vec<u8>> = Box::leak(Box::new(Image::open(corrupt.clone()).unwrap()));
        Table::restore(shape(), image, &section, &serial).err().unwrap()
    };
    for _ in 0..8 {
        let image: &'static Image<Vec<u8>> = Box::leak(Box::new(Image::open(corrupt.clone()).unwrap()));
        let error = Table::restore(shape(), image, &section, &four_threads).err().unwrap();
        assert_eq!(error, serial_error);
    }
    assert!(matches!(serial_error, RestoreError::Corrupt(Corrupt { group: 3, .. })), "{serial_error}");
}
