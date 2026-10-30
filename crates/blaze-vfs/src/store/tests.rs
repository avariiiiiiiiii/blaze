//! The store against safe reference versions (docs/v2/plan/design/unsafe.md "Differential"): a
//! `Vec` for a column, an `FxHashMap` for the index, a `Vec<String>` for the names.

use std::sync::Arc;

use blaze_core::sim::Rng;
use blaze_ids::ContentId;
use rustc_hash::FxHashMap;

use super::{Building, Column, FIRST, Writer, chunk_of, chunk_start, run};

/// Seeded pushes and runs: every value reads back where the `Vec` has it, and every run reads
/// as one slice.
#[test]
fn a_column_reads_what_a_vec_holds() {
    for seed in 0..50 {
        let mut rng = Rng::new(seed);
        let column = Column::<u32>::new();
        let mut model: Vec<u32> = Vec::new();
        let mut runs = Vec::new();
        for _ in 0..400 {
            if rng.one_in(3) {
                let len = rng.below(FIRST as u64 * 3) as u32;
                let values: Vec<u32> = (0..len).map(|_| rng.below(1 << 30) as u32).collect();
                // SAFETY: this thread is the column's one writer.
                let start = unsafe { column.push_run(len, u32::MAX, values.iter().copied()) };
                assert!(start as usize >= model.len(), "seed {seed}");
                model.resize(start as usize, u32::MAX);
                model.extend(&values);
                runs.push((start, values));
            } else {
                let value = rng.below(1 << 30) as u32;
                // SAFETY: as above.
                let at = unsafe { column.push(value) };
                assert_eq!(at as usize, model.len(), "seed {seed}");
                model.push(value);
            }
        }
        assert_eq!(column.len() as usize, model.len(), "seed {seed}");
        for (at, value) in model.iter().enumerate() {
            assert_eq!(column.get(at as u32), value, "seed {seed}: {at}");
        }
        for (start, values) in runs {
            assert_eq!(column.slice(start, values.len() as u32), &values[..], "seed {seed}");
        }
    }
}

#[test]
fn chunks_tile_the_offsets_and_runs_fit_one() {
    let mut at = 0;
    for chunk in 0..12 {
        assert_eq!(chunk_start(chunk), at);
        for place in 0..FIRST << chunk {
            assert_eq!(chunk_of(at), (chunk, place));
            at += 1;
        }
    }
    let mut rng = Rng::new(7);
    for _ in 0..2000 {
        let (cursor, len) = (rng.below(1 << 24) as u32, rng.below(1 << 12) as u32);
        // The reference: the first offset at or after `cursor` whose chunk holds the run.
        let mut want = cursor as usize;
        while chunk_of(want).1 + len as usize > FIRST << chunk_of(want).0 {
            want += 1;
        }
        assert_eq!(run(cursor, len) as usize, want, "{cursor}+{len}");
    }
}

#[test]
fn a_column_drops_each_value_once() {
    let value: Arc<[u8]> = Arc::from(&b"x"[..]);
    let column = Column::new();
    let count = FIRST * 3 + 5;
    for _ in 0..count {
        // SAFETY: this thread is the column's one writer.
        unsafe { column.push(value.clone()) };
    }
    assert_eq!(Arc::strong_count(&value), count + 1);
    drop(column);
    assert_eq!(Arc::strong_count(&value), 1);
}

#[test]
#[should_panic(expected = "value 0 of a column of 0")]
fn a_read_past_what_is_written_panics() {
    let _ = Column::<u32>::new().get(0);
}

#[test]
#[should_panic(expected = "values 0..+2 of a column of 1")]
fn a_slice_past_what_is_written_panics() {
    let column = Column::<u8>::new();
    // SAFETY: this thread is the column's one writer.
    unsafe { column.push(1) };
    let _ = column.slice(0, 2);
}

#[test]
#[should_panic(expected = "a file has no entries")]
fn a_file_shape_has_no_entries() {
    let mut writer = Writer::new();
    let node = writer.file(ContentId::of("store test", b"f"), Arc::from(&b"f"[..]));
    let table = writer.table();
    let _ = table.entries(table.shape(node));
}

/// Files and directories against a map from ID to node, through growth of the index and
/// through IDs whose probes start at one place.
#[test]
fn the_index_finds_what_a_map_holds() {
    let mut writer = Writer::new();
    let mut model: FxHashMap<ContentId, u32> = FxHashMap::default();
    for at in 0..5000u32 {
        let bytes: Arc<[u8]> = Arc::from(at.to_le_bytes().as_slice());
        let id = ContentId::of("store test", &bytes);
        let node = writer.file(id, bytes.clone());
        assert_eq!(*model.entry(id).or_insert(node), node);
        if at % 7 == 0 {
            assert_eq!(writer.file(id, bytes), node, "an ID added again is the same node");
        }
    }
    for at in 0..200u8 {
        let mut id = ContentId([9; 32]);
        id.0[31] = at;
        let node = writer.dir(id, &[(0, at as u32, 0)]);
        model.insert(id, node);
    }
    let table = writer.table();
    assert_eq!(table.len() as usize, model.len());
    for (id, &node) in &model {
        assert_eq!(table.find(id), Some(node));
        assert_eq!(table.id(node), id);
    }
    for at in 0..1000u32 {
        assert_eq!(table.find(&ContentId::of("store test absent", &at.to_le_bytes())), None);
    }
    let mut id = ContentId([9; 32]);
    id.0[31] = 200;
    assert_eq!(table.find(&id), None, "a missing ID in a full probe chain");
}

#[test]
fn a_directory_reads_its_entries_back() {
    let mut writer = Writer::new();
    let mut rng = Rng::new(3);
    let mut dirs = Vec::new();
    for at in 0..300u32 {
        let entries: Vec<(u32, u32, u8)> = (0..rng.below(FIRST as u64 * 2) as u32)
            .map(|k| (k, rng.below(1 << 20) as u32, rng.below(4) as u8))
            .collect();
        let node = writer.dir(ContentId::of("store test dir", &at.to_le_bytes()), &entries);
        dirs.push((node, entries));
    }
    let table = writer.table();
    for (node, want) in dirs {
        let entries = table.entries(table.shape(node));
        let got: Vec<(u32, u32, u8)> = (0..entries.len())
            .map(|k| (entries.names[k], entries.nodes[k], entries.modes[k]))
            .collect();
        assert_eq!(got, want);
    }
}

#[test]
fn names_read_back_as_interned() {
    let mut writer = Writer::new();
    let model: Vec<String> = (0..3000).map(|at| format!("{}é{at}", "n".repeat(at % 300))).collect();
    for (at, name) in model.iter().enumerate() {
        assert_eq!(writer.intern(name), at as u32);
    }
    let names = writer.names();
    assert_eq!(names.len() as usize, model.len());
    for (at, name) in model.iter().enumerate() {
        assert_eq!(names.str(at as u32), name);
        assert_eq!(names.get(at as u32), name.as_bytes());
    }
}

/// A reader's snapshot keeps the table it read through a replace, and the table is freed once
/// the snapshot's task ends.
#[test]
fn a_snapshot_keeps_its_table_through_a_replace() {
    let mut writer = Writer::new();
    let old = ContentId::of("store test", b"a");
    writer.file(old, Arc::from(&b"a"[..]));
    let shared = writer.shared();
    let snapshot = shared.read();
    let mut next = Building::new(1);
    let new = ContentId::of("store test", b"b");
    next.file(new, Arc::from(&b"b"[..]));
    assert_eq!(writer.replace(next), 1, "the old table waits for the snapshot's task");
    assert_eq!(snapshot.table().find(&old), Some(0));
    assert_eq!(snapshot.table().find(&new), None);
    drop(snapshot);
    // Other tests' readers may be in tasks begun before the replace; they end.
    while writer.reclaim() > 0 {
        std::thread::yield_now();
    }
    assert_eq!(shared.read().table().find(&new), Some(0));
    assert_eq!(shared.read().table().find(&old), None);
}

/// A reader on another thread looks for a directory while the writer adds it and its child:
/// under every interleaving and every value a load may read, it finds it whole or not at all.
#[cfg(blaze_check)]
#[test]
fn model_a_node_reads_whole_or_not_at_all() {
    use super::Table;

    blaze_check::model(|| {
        let table = Arc::new(Table::new(0));
        let file = ContentId::of("store model", b"f");
        let dir = ContentId::of("store model", b"d");
        let reader = {
            let table = table.clone();
            crate::sync::thread::spawn(move || {
                if let Some(node) = table.find(&dir) {
                    let entries = table.entries(table.shape(node));
                    assert_eq!(entries.len(), 1);
                    assert_eq!(table.id(entries.nodes[0]), &file);
                    assert_eq!(&table.blob(table.shape(entries.nodes[0]))[..], b"f");
                }
            })
        };
        // SAFETY: this thread is the table's one writer.
        let child = unsafe { table.add_file(file, Arc::from(&b"f"[..])) };
        // SAFETY: as above.
        unsafe { table.add_dir(dir, &[(0, child, 0)]) };
        reader.join().unwrap();
    });
}
