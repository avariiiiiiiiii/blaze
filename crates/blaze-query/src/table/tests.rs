use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use blaze_ids::ContentId;

use super::{Column, Edit, Free, Ids, Map, Rows};
use crate::{QueryKey, QueryKind};

fn key(n: u64) -> QueryKey {
    QueryKey { kind: QueryKind(1), input: ContentId::of("blaze v2 db table test 1", &n.to_le_bytes()) }
}

/// Waits for what a map unlinked to be freed: other tests' threads may still be in tasks that
/// began before it was unlinked.
fn freed<V: Send + Sync + 'static>(map: &Map<V>) {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        map.sweep();
        if map.held() == 0 {
            return;
        }
        assert!(Instant::now() < until, "what the map unlinked was never freed");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn readers_see_every_published_entry_whole_while_tables_grow() {
    let map: Map<(u64, u64)> = Map::new();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for reader in 0..4u64 {
            let (map, done) = (&map, &done);
            scope.spawn(move || {
                while !done.load(Ordering::Acquire) {
                    let task = blaze_reclaim::enter();
                    for n in (reader..8 * 2048).step_by(97) {
                        if let Some(&(a, b)) = map.get(&task, &key(n)) {
                            assert_eq!((a, b), (n, !n), "an entry is read whole or not at all");
                        }
                    }
                }
            });
        }
        let writers: Vec<_> = (0..8u64)
            .map(|writer| {
                let map = &map;
                scope.spawn(move || {
                    for n in writer * 2048..(writer + 1) * 2048 {
                        let task = blaze_reclaim::enter();
                        assert_eq!(map.insert(&task, key(n), (n, !n)), &(n, !n));
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("a writer");
        }
        done.store(true, Ordering::Release);
    });
    let task = blaze_reclaim::enter();
    for n in 0..8 * 2048 {
        assert_eq!(map.get(&task, &key(n)), Some(&(n, !n)));
    }
    let mut count = 0;
    map.for_each(&task, |_, _| count += 1);
    assert_eq!(count, 8 * 2048);
    drop(task);
    freed(&map);
}

#[test]
fn a_replaced_entry_stays_readable_until_its_task_ends() {
    let map: Map<String> = Map::new();
    let task = blaze_reclaim::enter();
    map.insert(&task, key(1), "old".to_owned());
    let old = map.get(&task, &key(1)).expect("inserted");
    map.insert(&task, key(1), "new".to_owned());
    assert_eq!(old, "old", "a reader keeps what it loaded");
    assert_eq!(map.get(&task, &key(1)).map(String::as_str), Some("new"));
    map.sweep();
    assert_eq!(map.held(), 1, "the old entry waits for this task");
    drop(task);
    freed(&map);
}

/// A replaced entry's row is not handed out again while a task that could hold it lives, and
/// is once its grace has passed: entries are rows of one column, never a box each.
#[test]
fn a_retired_row_is_reused_only_after_its_grace() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    map.insert(&task, key(1), 1);
    let first = map.row(&task, &key(1)).expect("inserted");
    map.insert(&task, key(1), 2);
    let second = map.row(&task, &key(1)).expect("replaced");
    assert_ne!(first, second, "the old row may still be read");
    map.insert(&task, key(1), 3);
    map.sweep();
    let third = map.row(&task, &key(1)).expect("replaced");
    assert!(third != first && third != second, "rows {first} and {second} wait for this task");
    drop(task);
    freed(&map);

    let task = blaze_reclaim::enter();
    map.insert(&task, key(1), 4);
    let reused = map.row(&task, &key(1)).expect("replaced");
    assert!(reused == first || reused == second, "row {reused} is new, not one past its grace");
    assert_eq!(map.get(&task, &key(1)), Some(&4));
}

#[test]
fn rows_are_one_region_by_index_and_never_move() {
    let rows: Rows<u64> = Rows::new();
    let ids = Ids::new();
    let mut free = Free::default();
    let claimed: Vec<u32> = (0..10_000).map(|_| ids.claim(&mut free)).collect();
    assert_eq!(claimed, (0..10_000).collect::<Vec<u32>>(), "one writer's rows are dense");
    for &row in &claimed {
        // SAFETY: claimed just now by this thread alone and not yet published.
        unsafe { rows.put(row, u64::from(row) * 3) };
    }
    // SAFETY: each row was put above by this thread, and nothing drops it.
    let first = unsafe { rows.get(0) } as *const u64;
    for &row in &claimed {
        // SAFETY: as above.
        let at = unsafe { rows.get(row) };
        assert_eq!(*at, u64::from(row) * 3);
        assert_eq!(at as *const u64, first.wrapping_add(row as usize), "row {row} is base + index");
    }
    // SAFETY: as above.
    assert_eq!(unsafe { rows.get(0) } as *const u64, first, "growing never moved row 0");
}

/// A table read at numbers nobody claimed (the kinds) finds what was written and nothing else.
#[test]
fn a_row_never_written_reads_as_none() {
    let rows: Rows<u64> = Rows::sized(1 << 16);
    // SAFETY: no reader yet; the row holds no value.
    unsafe { rows.put(0x2f0c, 7) };
    // SAFETY: written above by this thread and never dropped.
    let read = |row| unsafe { rows.written(row) }.copied();
    assert_eq!(read(0x2f0c), Some(7));
    assert_eq!(read(0x2f0b), None, "below the row written");
    assert_eq!(read(0xfff0), None, "past the row written");
}

#[test]
fn get_or_insert_keeps_the_first_value() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    assert_eq!(map.get_or_insert_with(&task, key(1), || 1), &1);
    assert_eq!(map.get_or_insert_with(&task, key(1), || unreachable!("present")), &1);
    let winners: Vec<u64> = std::thread::scope(|scope| {
        let spawned: Vec<_> = (0..8u64)
            .map(|thread| {
                let map = &map;
                scope.spawn(move || *map.get_or_insert_with(&blaze_reclaim::enter(), key(2), || thread))
            })
            .collect();
        spawned.into_iter().map(|it| it.join().expect("a thread")).collect()
    });
    assert!(winners.iter().all(|it| *it == winners[0]), "one value won: {winners:?}");
}

#[test]
fn removed_entries_leave_tombs_reused_and_dropped_by_growth() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    for n in 0..1000 {
        map.insert(&task, key(n), n);
    }
    map.retain(&task, |_, value| value % 2 == 0);
    for n in 0..1000 {
        assert_eq!(map.get(&task, &key(n)).copied(), (n % 2 == 0).then_some(n));
    }
    for _ in 0..4 {
        for n in (1..1000).step_by(2) {
            map.insert(&task, key(n), n);
        }
        map.retain(&task, |_, value| value % 2 == 0);
    }
    let mut live = 0;
    map.for_each(&task, |_, value| {
        assert_eq!(value % 2, 0);
        live += 1;
    });
    assert_eq!(live, 500, "the evens, each once, and no odd kept");
    map.edit_all(&task, |_, value| if *value < 10 { Edit::Replace(value + 1000) } else { Edit::Keep });
    assert_eq!(map.get(&task, &key(4)), Some(&1004));
    drop(task);
    freed(&map);
}

#[test]
fn an_update_sees_the_entry_it_replaces() {
    let map: Map<Vec<u64>> = Map::new();
    let task = blaze_reclaim::enter();
    std::thread::scope(|scope| {
        for thread in 0..8u64 {
            let map = &map;
            scope.spawn(move || {
                let task = blaze_reclaim::enter();
                for n in 0..100 {
                    map.update(&task, key(0), |old| {
                        let mut new = old.cloned().unwrap_or_default();
                        new.push(thread * 100 + n);
                        Some(new)
                    });
                }
            });
        }
    });
    let mut all = map.get(&task, &key(0)).expect("updated").clone();
    all.sort_unstable();
    assert_eq!(all, (0..800).collect::<Vec<u64>>(), "no update was lost");
    map.update(&task, key(0), |_| None);
    assert_eq!(map.get(&task, &key(0)).map(Vec::len), Some(800), "`None` leaves it");
}

#[test]
fn a_column_publishes_takes_and_restores_by_index() {
    let mut column: Column<String> = Column::new();
    let indices: Vec<u32> = std::thread::scope(|scope| {
        let spawned: Vec<_> = (0..4)
            .map(|thread| {
                let column = &column;
                scope.spawn(move || {
                    (0..100).map(|n| column.push(|index| format!("{thread}:{n}:{index}"))).collect::<Vec<_>>()
                })
            })
            .collect();
        spawned.into_iter().flat_map(|it| it.join().expect("a thread")).collect()
    });
    let task = blaze_reclaim::enter();
    for index in &indices {
        assert!(column.get(&task, *index).expect("pushed").ends_with(&format!(":{index}")));
    }
    let taken = column.take(7).expect("pushed");
    assert!(column.get(&task, 7).is_none());
    assert!(column.take(7).is_none(), "taken once");
    drop(task);
    drop(taken);
    let (next, values) = column.take_all();
    assert_eq!((next, values.len()), (400, 399));
    column.restore(next, values, |value| value.rsplit(':').next().and_then(|it| it.parse().ok()).expect("an index"));
    assert_eq!(column.values_mut().count(), 399);
    assert_eq!(column.push(|index| index.to_string()), 400);
}

/// A lookup, hit or miss, allocates nothing.
#[test]
fn a_map_lookup_allocates_nothing() {
    let map: Map<u64> = Map::new();
    let task = blaze_reclaim::enter();
    for n in 0..256 {
        map.insert(&task, key(n), n);
    }
    let found = blaze_alloc_count::assert_at_most(0, "a map lookup", || {
        (0..512).filter(|&n| map.get(&task, &key(n)) == Some(&n)).count()
    });
    assert_eq!(found, 256);
    drop(task);
    freed(&map);
}
