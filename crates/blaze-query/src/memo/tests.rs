use std::{
    sync::Barrier,
    time::{Duration, Instant},
};

use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Fingerprint};

use super::{MemoId, Slot, Store, value_row};
use crate::{Edge, QueryKey, QueryKind, Stored, segment::Span, table::EdgeRecord};

const SCHEMA: SchemaHash = SchemaHash(ContentId([0x57; 32]));
static ON_DISK: &[u8] = b"the bytes a cold pack holds";

fn key(n: u64) -> QueryKey {
    QueryKey { kind: QueryKind(0x2f00), input: ContentId::of("blaze v2 db memo test 1", &n.to_le_bytes()) }
}

fn edges(n: u64) -> Vec<EdgeRecord> {
    (0..3).map(|i| EdgeRecord::new(key(1000 + n * 3 + i), Fingerprint(u128::from(n) << 64 | u128::from(i)))).collect()
}

fn read(records: &[EdgeRecord]) -> Vec<Edge> {
    records.iter().map(EdgeRecord::edge).collect()
}

/// Waits until `done`, collecting: other tests' threads may still be in tasks that began
/// before what the store let go of was unlinked.
fn until(store: &Store, what: &str, mut done: impl FnMut(&Store) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let check = store.to_check();
        store.checked(check, |_| false);
        store.sweep();
        if done(store) {
            return;
        }
        assert!(Instant::now() < deadline, "{what} never happened");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A cold write names the value it copied by its row and that row's generation: once the row
/// is reused for another value of the memo, the stale word is refused.
#[test]
fn a_value_locations_generation_refuses_a_stale_swap() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let memo = store.make(key(0), Fingerprint(7), code, &[], b"first");
    let task = blaze_reclaim::enter();
    let (was, bytes) = store.in_memory(&task, memo).expect("in memory");
    assert_eq!(bytes, b"first");
    assert!(store.evict(&task, memo));
    assert_eq!(store.stored(&task, memo), Stored::Evicted);
    assert!(!store.evict(&task, memo), "evicted once");
    drop(task);
    until(&store, "the evicted value's grace", |store| store.held() == 0);

    let task = blaze_reclaim::enter();
    store.refill(&task, memo, b"again");
    let (now, bytes) = store.in_memory(&task, memo).expect("refilled");
    assert_eq!(bytes, b"again");
    assert_eq!(value_row(now), value_row(was), "the row past its grace is reused");
    assert_ne!(now, was, "under a generation of its own");
    // SAFETY: a static outlives the store.
    let stale = unsafe { store.to_disk(&task, memo, was, ON_DISK, ContentId([2; 32])) };
    assert!(!stale, "the word names a value the memo no longer has");
    assert_eq!(store.stored(&task, memo), Stored::Memory);
    assert_eq!(store.bytes(&task, memo), Some(&b"again"[..]));
    // SAFETY: as above.
    let fresh = unsafe { store.to_disk(&task, memo, now, ON_DISK, ContentId([2; 32])) };
    assert!(fresh);
    assert_eq!(store.stored(&task, memo), Stored::Disk(ContentId([2; 32])));
    assert_eq!(store.bytes(&task, memo), Some(ON_DISK));
    assert!(!store.evict(&task, memo), "a mapped value is not in memory");
}

/// A hit reads a value's bytes from the memo's own line: the copy there follows every swap of
/// the value's word, and is never used once the word names another value.
#[test]
fn a_memos_line_holds_where_its_current_value_is() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let memo = store.make(key(1), Fingerprint(8), code, &[], b"made");
    let task = blaze_reclaim::enter();
    let inline = |store: &Store| {
        store.hot(&task, memo).inline().map(|(bytes, len)| {
            // SAFETY: the copy names the value the word names, which this task holds.
            unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec()
        })
    };
    assert_eq!(inline(&store).as_deref(), Some(&b"made"[..]), "set when the memo is made");
    assert!(store.evict(&task, memo));
    assert_eq!(inline(&store), None, "no value, no copy");
    assert_eq!(store.bytes(&task, memo), None);
    store.refill(&task, memo, b"again");
    assert_eq!(inline(&store).as_deref(), Some(&b"again"[..]), "rewritten by the refill's swap");
    let (word, _) = store.in_memory(&task, memo).expect("refilled");
    // SAFETY: a static outlives the store.
    assert!(unsafe { store.to_disk(&task, memo, word, ON_DISK, ContentId([2; 32])) });
    assert_eq!(inline(&store).as_deref(), Some(ON_DISK), "and by the cold write's");
    assert_eq!(store.bytes(&task, memo), Some(ON_DISK));
}

/// Each task's edges are copied once, with its memo, into its thread's edge segment: one
/// thread's memos are adjacent ranges of one segment, and two threads write two segments.
#[test]
fn a_tasks_edges_are_one_range_of_its_threads_segment() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let barrier = Barrier::new(2);
    let made: Vec<Vec<MemoId>> = std::thread::scope(|scope| {
        let spawned: Vec<_> = (0..2u64)
            .map(|thread| {
                let (store, barrier) = (&store, &barrier);
                scope.spawn(move || {
                    // Both threads alive at once, so each holds a lane of its own.
                    barrier.wait();
                    (0..50)
                        .map(|n| {
                            let n = thread * 50 + n;
                            store.make(key(n), Fingerprint(u128::from(n)), code, &edges(n), &n.to_le_bytes())
                        })
                        .collect::<Vec<MemoId>>()
                })
            })
            .collect();
        spawned.into_iter().map(|it| it.join().expect("a thread")).collect()
    });
    let task = blaze_reclaim::enter();
    for (thread, memos) in made.iter().enumerate() {
        for (at, &memo) in memos.iter().enumerate() {
            let n = thread as u64 * 50 + at as u64;
            assert_eq!(read(store.edges(&task, memo)), read(&edges(n)));
            assert_eq!(store.bytes(&task, memo), Some(&n.to_le_bytes()[..]));
            assert_eq!(store.key(&task, memo), key(n));
            assert_eq!(store.fingerprint(&task, memo), Fingerprint(u128::from(n)));
        }
        for pair in memos.windows(2) {
            let (a, b) = (store.edges(&task, pair[0]), store.edges(&task, pair[1]));
            assert_eq!(a.as_ptr_range().end, b.as_ptr(), "one thread's memos' edges are adjacent");
        }
    }
    assert_eq!(store.segments(), (2, 2, 0), "one edge and one value segment a thread");
}

/// Memos are freed a segment at a time: a segment stays while one memo in it lives, and is
/// freed whole once none does; a freed memo's row is reused.
#[test]
fn a_segment_is_freed_whole_once_nothing_in_it_is_held() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    // 168 bytes of edges each: the first 16 KiB segment holds 97, the second the rest.
    let memos: Vec<MemoId> = (0..200).map(|n| store.make(key(n), Fingerprint(1), code, &edges(n), &[])).collect();
    assert_eq!(store.segments().0, 2);
    let task = blaze_reclaim::enter();
    let first = store.edges(&task, memos[0]).as_ptr();
    let last_of_first = store.edges(&task, memos[96]).as_ptr();
    assert_eq!(last_of_first, first.wrapping_add(96 * 3), "memo 96 is the first segment's last");
    drop(task);

    let (kept, rest): (Vec<MemoId>, Vec<MemoId>) = memos.iter().partition(|&&memo| memo == memos[96]);
    store.unlinked(rest);
    until(&store, "freeing all but one memo", |store| store.dying() == 0);
    assert_eq!(store.segments().0, 2, "memo 96 holds the first segment; the second is open");

    store.unlinked(kept);
    until(&store, "freeing the last memo of the first segment", |store| store.segments().0 == 1);
    assert_eq!(store.dying(), 0);

    let again = store.make(key(0), Fingerprint(1), code, &edges(0), &[]);
    assert!(memos.contains(&again), "memo {again} is a new row, not a freed one");
}

#[test]
fn a_slot_holds_three_memos_in_itself_and_more_in_a_list() {
    let store = Store::new();
    let few = store.slot(None, &[4, 5, 6]);
    assert_eq!(few.memos(), &[4, 5, 6]);
    assert_eq!(few.key(), None);
    assert_eq!(store.segments().2, 0, "three memos are inline");
    let many = store.slot(Some(store.key_bytes(b"the key's bytes")), &[1, 2, 3, 4, 5]);
    assert_eq!(many.memos(), &[1, 2, 3, 4, 5]);
    assert_eq!(many.key(), Some(&b"the key's bytes"[..]));
    let shared = many.share_key();
    drop(many);
    assert_eq!(shared.as_ref().map(Span::bytes), Some(&b"the key's bytes"[..]), "a shared key outlives its slot");
    assert!(Slot::empty().memos().is_empty());
}

/// Reading a memo (its line, value, edges, key and where its value is) allocates nothing.
#[test]
fn a_store_read_allocates_nothing() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    let made = edges(4);
    let memo = store.make(key(4), Fingerprint(4), code, &made, b"read me");
    let task = blaze_reclaim::enter();
    let (fingerprint, inline, bytes, edge_len, at, stored) =
        blaze_alloc_count::assert_at_most(0, "a store read", || {
            (
                store.fingerprint(&task, memo),
                store.hot(&task, memo).inline().map(|(_, len)| len),
                store.bytes(&task, memo),
                store.edges(&task, memo).len(),
                store.key(&task, memo),
                store.stored(&task, memo),
            )
        });
    assert_eq!((fingerprint, inline, bytes, edge_len), (Fingerprint(4), Some(7), Some(&b"read me"[..]), 3));
    assert_eq!((at, stored), (key(4), Stored::Memory));
}

/// Once a thread has its lane, open segments and a block of rows, making a memo copies into
/// them and allocates nothing.
#[test]
fn a_memo_made_on_a_warm_thread_allocates_nothing() {
    let store = Store::new();
    let code = store.code(SCHEMA, ContentId([1; 32]));
    for n in 0..8 {
        store.make(key(n), Fingerprint(1), code, &edges(n), &n.to_le_bytes());
    }
    let made = edges(8);
    let memo = blaze_alloc_count::assert_at_most(0, "a memo made on a warm thread", || {
        store.make(key(8), Fingerprint(1), code, &made, &8u64.to_le_bytes())
    });
    let task = blaze_reclaim::enter();
    assert_eq!(read(store.edges(&task, memo)), read(&made));
    assert_eq!(store.bytes(&task, memo), Some(&8u64.to_le_bytes()[..]));
}
