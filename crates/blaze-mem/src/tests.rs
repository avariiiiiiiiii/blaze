//! Each allocator against a model (`Vec`, `Box`, `Vec<Option<_>>`) on seeded random operations,
//! the properties its design promises (no move on growth, stale handles detected, rollback,
//! reuse), and its allocation budget with `blaze-alloc-count`: warm, none of them calls the
//! global allocator.

use std::{
    alloc::Layout,
    cell::Cell,
    rc::Rc,
    sync::{Arc, atomic::AtomicUsize, atomic::Ordering},
    thread,
};

use blaze_alloc_count::measure;

use crate::{
    AppendVec, Arena, Built, Column, Frame, Handle, MemError, Pool, Region, RegionVec, Revisions,
    Slab, Tag, chunks, frame, pool, revision::PublishError, scratch, tag,
};

/// xorshift64*: the tests' operations, the same on every run. Not `blaze_ids::rng`, since
/// `blaze-ids` depends on this crate.
struct Xorshift64Star(u64);

impl Xorshift64Star {
    fn new(seed: u64) -> Xorshift64Star {
        Xorshift64Star(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Counts its drops, to check each value is dropped exactly once.
struct Counted {
    value: u64,
    drops: Rc<Cell<usize>>,
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

// Region

#[test]
fn region_commits_on_growth_and_never_moves() {
    let region = Region::reserve(64 << 20, Tag::Other).expect("reserve");
    let base = region.base();
    assert_eq!(region.committed(), 0);
    region.ensure(100).expect("commit");
    assert!(region.committed() >= 100);
    let first = region.at(0).as_ptr();
    // SAFETY: the first 100 bytes are committed and this test owns them.
    unsafe { first.write_bytes(0xAB, 100) };
    region.ensure(32 << 20).expect("commit more");
    assert_eq!(region.base(), base, "growing never moves the base");
    // SAFETY: committed earlier, still committed, written above.
    assert_eq!(unsafe { first.read() }, 0xAB, "growing keeps what was written");
    assert!(region.committed() >= 32 << 20);
}

#[test]
fn region_past_its_reservation_is_an_error_naming_the_tag() {
    let region = Region::reserve(1 << 20, Tag::Link).expect("reserve");
    let error = region.ensure(region.reserved() + 1).expect_err("past the reservation");
    assert!(matches!(error, MemError::Exhausted { tag: Tag::Link, .. }), "{error}");
    assert!(error.to_string().starts_with("link: "), "{error}");
}

#[test]
fn region_decommit_after_gives_pages_back_and_commits_again() {
    let mut region = Region::reserve(16 << 20, Tag::Other).expect("reserve");
    region.ensure(8 << 20).expect("commit");
    region.decommit_after(1 << 20).expect("decommit");
    assert!(region.committed() < 8 << 20);
    region.ensure(4 << 20).expect("commit again");
    let at = region.at(3 << 20).as_ptr();
    // SAFETY: committed again just now.
    unsafe { at.write(7) };
    // SAFETY: as above.
    assert_eq!(unsafe { at.read() }, 7);
}

#[test]
fn regions_grow_from_many_threads_at_once() {
    let region = Arc::new(Region::reserve(256 << 20, Tag::Other).expect("reserve"));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let region = Arc::clone(&region);
            thread::spawn(move || {
                for step in 0..64 {
                    let len = (t * 64 + step + 1) * (256 << 10);
                    region.ensure(len).expect("commit");
                    let at = region.at(len - 1).as_ptr();
                    // SAFETY: the byte is committed; each thread writes the last byte of its
                    // own lengths, which other threads write too only with the same value.
                    unsafe { at.write(1) };
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("grower");
    }
    assert!(region.committed() >= 8 * 64 * (256 << 10));
}

// RegionVec

#[test]
fn region_vec_matches_vec_on_random_operations() {
    let mut rng = Xorshift64Star::new(1);
    let mut column = RegionVec::<u64>::new(1 << 20, Tag::Other).expect("reserve");
    let mut model = Vec::new();
    for _ in 0..200_000 {
        match rng.below(10) {
            0..=5 => {
                let value = rng.next();
                assert_eq!(column.push(value), model.len());
                model.push(value);
            }
            6 => assert_eq!(column.pop(), model.pop()),
            7 => {
                let len = rng.below(model.len() as u64 + 1) as usize;
                column.truncate(len);
                model.truncate(len);
            }
            8 => {
                let values: Vec<u64> = (0..rng.below(100)).map(|_| rng.next()).collect();
                column.extend_from_slice(&values);
                model.extend_from_slice(&values);
            }
            _ => {
                if !model.is_empty() {
                    let i = rng.below(model.len() as u64) as usize;
                    let value = rng.next();
                    column[i] = value;
                    model[i] = value;
                }
            }
        }
        assert_eq!(column.len(), model.len());
    }
    assert_eq!(column.as_slice(), model.as_slice());
}

#[test]
fn region_vec_addresses_are_stable_across_growth() {
    let mut column = RegionVec::<[u8; 24]>::new(1 << 22, Tag::Other).expect("reserve");
    column.push([1; 24]);
    let first = column.as_ptr_at(0).expect("pushed");
    for i in 0..1_000_000u32 {
        column.push([i as u8; 24]);
    }
    assert_eq!(column.as_ptr_at(0), Some(first), "growth never moves an element");
    assert_eq!(column[0], [1; 24]);
}

#[test]
fn region_vec_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let mut column = RegionVec::new(1000, Tag::Other).expect("reserve");
    for value in 0..100 {
        column.push(Counted { value, drops: Rc::clone(&drops) });
    }
    column.truncate(60);
    assert_eq!(drops.get(), 40);
    let last = column.pop().expect("one");
    assert_eq!(last.value, 59);
    drop(last);
    assert_eq!(drops.get(), 41);
    drop(column);
    assert_eq!(drops.get(), 100);
}

#[test]
fn region_vec_full_is_an_error() {
    let mut column = RegionVec::<u64>::new(8, Tag::Other).expect("reserve");
    let max = column.max_len();
    for i in 0..max {
        column.try_push(i as u64).expect("room");
    }
    assert!(matches!(column.try_push(0), Err(MemError::Exhausted { .. })));
}

#[test]
fn region_vec_push_allocates_nothing() {
    let mut column = RegionVec::<u64>::new(1 << 24, Tag::Other).expect("reserve");
    let (_, used) = measure(|| {
        for i in 0..1_000_000 {
            column.push(i);
        }
    });
    assert_eq!(used.allocations, 0, "a region grows by committing pages, not by the allocator");
}

// Column

#[test]
fn column_takes_pushes_from_many_threads_and_reads_them_in_place() {
    let column = Arc::new(Column::<u64>::new(1 << 20, Tag::Intern).expect("reserve"));
    let threads: Vec<_> = (0..8u64)
        .map(|t| {
            let column = Arc::clone(&column);
            thread::spawn(move || {
                let mut ids = Vec::with_capacity(10_000);
                for i in 0..10_000u64 {
                    let id = column.push(t << 32 | i).expect("room");
                    assert_eq!(column.get(id), Some(&(t << 32 | i)), "readable once pushed");
                    ids.push(id);
                }
                ids
            })
        })
        .collect();
    let mut seen = vec![false; 80_000];
    for thread in threads {
        for id in thread.join().expect("pusher") {
            assert!(!std::mem::replace(&mut seen[id as usize], true), "each index once");
        }
    }
    assert!(seen.iter().all(|&s| s));
    let mut model: Vec<u64> =
        (0..8u64).flat_map(|t| (0..10_000u64).map(move |i| t << 32 | i)).collect();
    let mut got: Vec<u64> = column.iter().map(|(_, &v)| v).collect();
    model.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, model);
    assert_eq!(column.get(90_000), None, "an index never pushed is not readable");
}

#[test]
fn column_reads_allocate_nothing() {
    let column = Column::<u64>::new(1 << 16, Tag::Intern).expect("reserve");
    for i in 0..10_000 {
        column.push(i).expect("room");
    }
    let (sum, used) = measure(|| (0..10_000u32).filter_map(|i| column.get(i)).sum::<u64>());
    assert_eq!(sum, (0..10_000).sum());
    assert_eq!(used.allocations, 0);
}

// AppendVec

#[test]
fn append_vec_matches_vec_and_never_moves() {
    let mut rng = Xorshift64Star::new(7);
    let column = AppendVec::<u64>::new(1 << 20, Tag::Vfs).expect("reserve");
    let mut model = Vec::new();
    for _ in 0..200_000 {
        let value = rng.next();
        assert_eq!(column.append(value) as usize, model.len());
        model.push(value);
    }
    let first = column.as_slice().as_ptr();
    for i in 0..100_000 {
        column.append(i);
        model.push(i);
    }
    assert_eq!(column.as_slice().as_ptr(), first, "growth never moves an element");
    assert_eq!(column.as_slice(), model.as_slice());
    assert_eq!(column.get(model.len() as u32), None);
}

#[test]
fn append_vec_readers_see_a_written_prefix_while_one_writer_appends() {
    let column = AppendVec::<[u64; 2]>::new(1 << 20, Tag::Vfs).expect("reserve");
    thread::scope(|scope| {
        let column = &column;
        let readers: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(move || {
                    let mut seen = 0;
                    while seen < 100_000 {
                        let published = column.as_slice();
                        assert!(published.len() >= seen, "the length only grows");
                        for (i, pair) in published.iter().enumerate().skip(seen) {
                            assert_eq!(*pair, [i as u64, !(i as u64)], "a published value is whole");
                        }
                        seen = published.len();
                    }
                })
            })
            .collect();
        for i in 0..100_000u64 {
            column.append([i, !i]);
        }
        for reader in readers {
            reader.join().expect("reader");
        }
    });
}

#[test]
fn append_vec_full_is_an_error_and_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let column = AppendVec::new(8, Tag::Vfs).expect("reserve");
    for value in 0..8 {
        column.try_append(Counted { value, drops: Rc::clone(&drops) }).expect("room");
    }
    let refused = column.try_append(Counted { value: 8, drops: Rc::clone(&drops) });
    assert!(matches!(refused, Err(MemError::Exhausted { tag: Tag::Vfs, .. })));
    assert_eq!(drops.get(), 1, "the refused value is dropped");
    drop(column);
    assert_eq!(drops.get(), 9);
}

#[test]
fn append_vec_appends_and_reads_allocate_nothing() {
    let column = AppendVec::<u64>::new(1 << 22, Tag::Vfs).expect("reserve");
    let (sum, used) = measure(|| {
        for i in 0..1_000_000 {
            column.append(i);
        }
        column.iter().sum::<u64>()
    });
    assert_eq!(sum, (0..1_000_000).sum());
    assert_eq!(used.allocations, 0);
}

// Frame

#[test]
fn frame_vec_matches_vec_and_grows_in_place() {
    let mut rng = Xorshift64Star::new(2);
    let frame = Frame::new(Tag::Infer).expect("reserve");
    let mut vec: Vec<u32, &Frame> = Vec::new_in(&frame);
    let mut model = Vec::new();
    vec.push(0);
    model.push(0);
    let first = vec.as_ptr();
    for _ in 0..100_000 {
        match rng.below(8) {
            0..=5 => {
                let value = rng.next() as u32;
                vec.push(value);
                model.push(value);
            }
            6 => assert_eq!(vec.pop(), model.pop()),
            _ => {
                vec.shrink_to_fit();
                model.shrink_to_fit();
            }
        }
    }
    assert_eq!(vec.as_slice(), model.as_slice());
    assert_eq!(vec.as_ptr(), first, "the frame's top allocation grows in place");
}

#[test]
fn frame_interleaved_vecs_keep_their_contents() {
    let frame = Frame::new(Tag::Infer).expect("reserve");
    let mut a: Vec<u64, &Frame> = Vec::new_in(&frame);
    let mut b: Vec<u8, &Frame> = Vec::new_in(&frame);
    for i in 0..50_000u64 {
        a.push(i * 3);
        b.push(i as u8);
    }
    assert!(a.iter().enumerate().all(|(i, &v)| v == i as u64 * 3));
    assert!(b.iter().enumerate().all(|(i, &v)| v == i as u8));
    let boxed = Box::new_in([7u64; 32], &frame);
    assert_eq!(*boxed, [7; 32]);
}

#[test]
fn frame_values_are_aligned_and_disjoint() {
    let frame = Frame::new(Tag::Mir).expect("reserve");
    let mut rng = Xorshift64Star::new(3);
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for _ in 0..10_000 {
        let size = rng.below(300) as usize;
        let align = 1 << rng.below(13);
        let ptr = frame.alloc_layout(Layout::from_size_align(size, align).expect("layout"));
        assert_eq!(ptr.as_ptr() as usize % align, 0);
        blocks.push((ptr.as_ptr() as usize, size));
    }
    blocks.sort_unstable();
    for pair in blocks.windows(2) {
        assert!(pair[0].0 + pair[0].1 <= pair[1].0, "blocks overlap");
    }
}

#[test]
fn frame_marker_rolls_back_and_reset_frees_all() {
    let mut frame = Frame::new(Tag::Codegen).expect("reserve");
    let kept = frame.alloc_slice_copy(&[1u8; 100]).as_ptr();
    let marker = frame.mark();
    let used = frame.used();
    for _ in 0..1000 {
        frame.alloc(0u64);
    }
    assert!(frame.used() > used);
    frame.reset_to(marker);
    assert_eq!(frame.used(), used);
    let again = frame.alloc(5u64) as *mut u64 as usize;
    assert!(again >= kept as usize + 100, "a rollback keeps what came before the marker");
    frame.reset();
    assert_eq!(frame.used(), 0);
    let text = frame.alloc_str("frame");
    assert_eq!(text, "frame");
}

#[test]
#[should_panic(expected = "a marker of another frame")]
fn frame_marker_of_another_frame_stops() {
    let one = Frame::new(Tag::Codegen).expect("reserve");
    let mut two = Frame::new(Tag::Codegen).expect("reserve");
    let marker = one.mark();
    two.reset_to(marker);
}

#[test]
fn frame_scope_frees_what_it_allocated() {
    let mut frame = Frame::new(Tag::Parse).expect("reserve");
    frame.alloc(1u64);
    let before = frame.used();
    let sum = frame.scope(|frame| {
        let values = frame.alloc_slice_fill_with(1000, |i| i as u64);
        values.iter().sum::<u64>()
    });
    assert_eq!(sum, (0..1000).sum());
    assert_eq!(frame.used(), before);
}

#[test]
fn frame_tasks_nest_with_frames_of_their_own() {
    frame::task(Tag::Infer, |outer| {
        let kept = outer.alloc_slice_copy(&[9u8; 64]);
        let inner_used = frame::task(Tag::Mir, |inner| {
            assert!(!std::ptr::eq(inner, outer), "a nested task gets another frame");
            inner.alloc_slice_copy(&[1u8; 4096]);
            inner.used()
        });
        assert!(inner_used >= 4096);
        assert_eq!(kept, &[9u8; 64], "an inner reset leaves the outer task's memory alone");
    });
    assert!(frame::spare_frames() >= 2, "both frames are kept warm for the next tasks");
    frame::task(Tag::Infer, |frame| assert_eq!(frame.used(), 0, "a task starts empty"));
}

#[test]
fn frame_task_counts_its_peak_to_its_tag() {
    let before = tag::snapshot().get(Tag::Expand).allocated;
    frame::task(Tag::Expand, |frame| {
        frame.alloc_slice_copy(&[0u8; 10_000]);
    });
    let after = tag::snapshot().get(Tag::Expand);
    assert!(after.allocated >= before + 10_000);
    assert!(after.task_peak >= 10_000);
}

#[test]
fn frame_past_its_reservation_is_an_alloc_error_not_a_crash() {
    use std::alloc::Allocator;
    let frame = Frame::with_reserve(1 << 20, Tag::Codegen).expect("reserve");
    let too_big = Layout::from_size_align(4 << 20, 8).expect("layout");
    assert!(frame.allocate(too_big).is_err());
    assert!(matches!(
        frame.try_alloc_layout(too_big),
        Err(MemError::Exhausted { tag: Tag::Codegen, .. })
    ));
}

#[test]
fn frame_task_allocates_nothing_once_warm() {
    let work = || {
        frame::task(Tag::Infer, |frame| {
            let mut types: Vec<u64, &Frame> = Vec::with_capacity_in(64, frame);
            for i in 0..100_000 {
                types.push(i);
            }
            let names = frame.alloc_str("a name");
            types.len() + names.len()
        })
    };
    work();
    let (_, used) = measure(work);
    assert_eq!(used.allocations, 0, "a warm task allocates only from its frame");
}

// Arena

#[test]
fn arena_values_from_many_threads_live_until_reset() {
    let mut arena = Arena::new(Tag::Query);
    thread::scope(|scope| {
        let arena = &arena;
        let handles: Vec<_> = (0..8u64)
            .map(|t| {
                scope.spawn(move || {
                    let local = arena.local();
                    let mut kept: Vec<&u64> = Vec::new();
                    for i in 0..20_000u64 {
                        kept.push(local.alloc(t * 1_000_000 + i));
                    }
                    let big = local.alloc_slice_copy(&vec![t as u8; 100_000]);
                    let huge = local.alloc_slice_copy(&vec![t as u16; 3 << 20]);
                    (kept, big, huge)
                })
            })
            .collect();
        for (t, handle) in handles.into_iter().enumerate() {
            let (kept, big, huge) = handle.join().expect("allocator");
            for (i, value) in kept.into_iter().enumerate() {
                assert_eq!(*value, t as u64 * 1_000_000 + i as u64);
            }
            assert!(big.iter().all(|&b| b == t as u8));
            assert!(huge.iter().all(|&h| h == t as u16));
        }
    });
    assert!(arena.held() > 0);
    arena.reset();
    assert_eq!(arena.held(), 0);
}

#[test]
fn arena_vec_matches_vec_and_keep_freezes_it() {
    let arena = Arena::new(Tag::Nameres);
    let local = arena.local();
    let mut rng = Xorshift64Star::new(4);
    let mut vec: Vec<u32, _> = Vec::new_in(&local);
    let mut model = Vec::new();
    for _ in 0..100_000 {
        if rng.below(5) == 0 {
            assert_eq!(vec.pop(), model.pop());
        } else {
            let value = rng.next() as u32;
            vec.push(value);
            model.push(value);
        }
    }
    let kept: &[u32] = local.keep(vec);
    drop(local);
    assert_eq!(kept, model.as_slice(), "a kept vector lives as long as its arena");
}

#[test]
fn arena_gives_chunks_back_and_takes_them_again_warm() {
    let mut first = Arena::new(Tag::Vfs);
    {
        let local = first.local();
        for i in 0..100_000u64 {
            local.alloc(i);
        }
    }
    let held = first.held();
    assert!(held > 0);
    first.reset();
    assert_eq!(first.held(), 0, "a reset gives every chunk back");
    // Purged chunks come back zeroed and are taken like warm ones.
    chunks::trim();
    let second = Arena::new(Tag::Vfs);
    let local = second.local();
    let values: Vec<&u64> = (0..100_000u64)
        .map(|i| {
            let value: &u64 = local.alloc(i);
            value
        })
        .collect();
    assert!(values.iter().enumerate().all(|(i, &&v)| v == i as u64));
    assert_eq!(second.held(), held, "the same work holds the same chunks");
    assert!(chunks::carved_bytes() >= held);
}

#[test]
fn arena_allocates_nothing_from_the_global_allocator() {
    let request = || {
        let arena = Arena::new(Tag::Query);
        let local = arena.local();
        let mut total = 0;
        for i in 0..50_000u64 {
            total += *local.alloc(i);
        }
        let mut list: Vec<u64, _> = Vec::new_in(&local);
        list.extend(0..10_000);
        total + list.len() as u64
    };
    request();
    let (_, used) = measure(request);
    assert_eq!(used.allocations, 0, "a request's memory comes from chunks, not the allocator");
}

// Revisions

struct Names;

impl Built for Names {
    type Value<'a> = &'a [&'a str];

    fn shorten<'s, 'l: 's>(value: &'s &'l [&'l str]) -> &'s &'s [&'s str] {
        value
    }
}

fn build_names<'a>(local: &crate::ArenaLocal<'a>, names: &[&str]) -> &'a [&'a str] {
    let names: Vec<&'a str> = names
        .iter()
        .map(|name| {
            let name: &'a str = local.alloc_str(name);
            name
        })
        .collect();
    local.alloc_slice_copy(&names)
}

/// One test, so no other test's task guard (the reclaim epoch is process-wide) holds a grace
/// these publishes and collections wait on.
#[test]
fn graces_revisions_publish_read_wait_for_readers_and_drop_once_and_limbo_waits() {
    publish_and_read_through_a_task_guard();
    wait_for_a_task_that_may_read_the_old_one();
    drop_each_value_once();
    limbo_keeps_a_retired_object_until_its_readers_end();
    limbo_matches_a_model_and_allocates_nothing_once_warm();
}

fn limbo_keeps_a_retired_object_until_its_readers_end() {
    let drops = Arc::new(AtomicUsize::new(0));
    let pool = Pool::<DropCount>::new(1024, Tag::Infer).expect("reserve");
    let mut cache = pool.cache();
    let mut limbo = pool::Limbo::new(&pool).expect("reserve");
    let first = cache.alloc(DropCount(Arc::clone(&drops)));
    let index = first.index();
    let reader = blaze_reclaim::enter();
    limbo.retire(first);
    assert_eq!(limbo.collect(), 0, "a task that began before the retirement may still read it");
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(reader);
    assert_eq!(limbo.collect(), 1);
    assert_eq!(drops.load(Ordering::Relaxed), 1, "dropped once its grace passed");
    assert!(limbo.is_empty());
    let second = cache.alloc(DropCount(Arc::clone(&drops)));
    limbo.retire(second);
    drop(limbo);
    assert_eq!(drops.load(Ordering::Relaxed), 2, "a dropped limbo drops what it held");
    drop(cache);
    let mut again = pool.cache();
    let reused = again.alloc(DropCount(Arc::clone(&drops)));
    assert!(reused.index() <= index + 2 * 128, "collected slots go back to the pool");
}

fn limbo_matches_a_model_and_allocates_nothing_once_warm() {
    let pool = Pool::<[u64; 4]>::new(1 << 12, Tag::Infer).expect("reserve");
    let mut cache = pool.cache();
    let mut limbo = pool::Limbo::new(&pool).expect("reserve");
    let mut rng = Xorshift64Star::new(9);
    let mut round = |cache: &mut crate::Cache<'_, [u64; 4]>, limbo: &mut pool::Limbo<'_, [u64; 4]>| {
        let mut retired = 0;
        let mut collected = 0;
        for _ in 0..20_000 {
            if rng.below(3) == 0 {
                collected += limbo.collect();
            } else {
                limbo.retire(cache.alloc([rng.next(); 4]));
                retired += 1;
            }
        }
        collected += limbo.collect();
        assert_eq!(collected, retired, "with no reader every grace passes");
        assert!(limbo.is_empty());
    };
    round(&mut cache, &mut limbo);
    let (_, used) = measure(|| round(&mut cache, &mut limbo));
    assert_eq!(used.allocations, 0, "retiring and collecting reuse the limbo's column");
    assert!(pool.carved() <= 3 * 128 + 64, "collected slots are reused");
}

fn publish_and_read_through_a_task_guard() {
    let revisions = Revisions::<Names>::new(Tag::Vfs);
    {
        let guard = blaze_reclaim::enter();
        assert!(revisions.read(&guard).is_none(), "nothing before the first publish");
    }
    let one = revisions.publish(|local| build_names(local, &["a", "b"])).expect("publish");
    assert_eq!(one, 1);
    {
        let guard = blaze_reclaim::enter();
        let current = revisions.read(&guard).expect("published");
        assert_eq!(current.number, 1);
        assert_eq!(*current.value, ["a", "b"]);
    }
    let two = revisions.publish(|local| build_names(local, &["c"])).expect("publish");
    assert_eq!(two, 2);
    let three = revisions.publish(|local| build_names(local, &["d", "e", "f"])).expect("publish");
    assert_eq!(three, 3, "no task read revision 1, so its arena is reused at once");
    let guard = blaze_reclaim::enter();
    assert_eq!(*revisions.read(&guard).expect("published").value, ["d", "e", "f"]);
}

fn wait_for_a_task_that_may_read_the_old_one() {
    let revisions = Revisions::<Names>::new(Tag::Vfs);
    revisions.publish(|local| build_names(local, &["one"])).expect("publish");
    let reader = blaze_reclaim::enter();
    let old = revisions.read(&reader).expect("published");
    assert_eq!(*old.value, ["one"]);
    revisions.publish(|local| build_names(local, &["two"])).expect("a second arena is free");
    // Revision 1's arena is next, but this thread's task may still read it.
    assert_eq!(
        revisions.publish(|local| build_names(local, &["three"])),
        Err(PublishError::Readers)
    );
    assert_eq!(*old.value, ["one"], "the old revision is intact while its reader's task lasts");
    drop(reader);
    assert_eq!(revisions.publish(|local| build_names(local, &["three"])), Ok(3));
}

struct Dropping;

impl Built for Dropping {
    type Value<'a> = DropCount;

    fn shorten<'s, 'l: 's>(value: &'s DropCount) -> &'s DropCount {
        value
    }
}

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn drop_each_value_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let revisions = Revisions::<Dropping>::new(Tag::Vfs);
    for _ in 0..5 {
        let drops = Arc::clone(&drops);
        revisions.publish(move |_| DropCount(drops)).expect("publish");
    }
    assert_eq!(drops.load(Ordering::Relaxed), 3, "each reused slot dropped its old value");
    drop(revisions);
    assert_eq!(drops.load(Ordering::Relaxed), 5);
}

// Pool

#[test]
fn pool_matches_box_on_random_operations() {
    let pool = Pool::<[u64; 4]>::new(1 << 16, Tag::Plane).expect("reserve");
    let mut cache = pool.cache();
    let mut rng = Xorshift64Star::new(5);
    let mut live = Vec::new();
    let mut model: Vec<Box<[u64; 4]>> = Vec::new();
    let mut peak = 0;
    for _ in 0..200_000 {
        match rng.below(6) {
            0..=2 => {
                let value = [rng.next(); 4];
                live.push(cache.alloc(value));
                model.push(Box::new(value));
                peak = peak.max(live.len());
            }
            3 if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                let pooled = live.swap_remove(i);
                let expected = model.swap_remove(i);
                assert_eq!(cache.free(pooled), *expected);
            }
            4 if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                drop(live.swap_remove(i));
                model.swap_remove(i);
            }
            _ if !live.is_empty() => {
                let i = rng.below(live.len() as u64) as usize;
                live[i][0] = rng.next();
                model[i][0] = live[i][0];
            }
            _ => {}
        }
    }
    let mut indices: Vec<u32> = live.iter().map(|p| p.index()).collect();
    indices.sort_unstable();
    indices.dedup();
    assert_eq!(indices.len(), live.len(), "live objects have distinct slots");
    for (pooled, boxed) in live.iter().zip(&model) {
        assert_eq!(**pooled, **boxed);
    }
    assert!(pool.carved() as usize <= peak + 2 * 128 + 64, "freed slots are reused");
}

#[test]
fn pool_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let pool = Pool::<Counted>::new(1024, Tag::Plane).expect("reserve");
    let mut cache = pool.cache();
    let a = cache.alloc(Counted { value: 1, drops: Rc::clone(&drops) });
    let b = cache.alloc(Counted { value: 2, drops: Rc::clone(&drops) });
    let c = cache.alloc(Counted { value: 3, drops: Rc::clone(&drops) });
    cache.recycle(a);
    assert_eq!(drops.get(), 1);
    drop(b);
    assert_eq!(drops.get(), 2);
    let value = c.into_inner();
    assert_eq!(drops.get(), 2);
    assert_eq!(value.value, 3);
    drop(value);
    assert_eq!(drops.get(), 3);
}

#[test]
fn pool_caches_on_many_threads_share_slots_in_batches() {
    let pool = Pool::<u64>::new(1 << 20, Tag::Io).expect("reserve");
    thread::scope(|scope| {
        for t in 0..8u64 {
            let pool = &pool;
            scope.spawn(move || {
                let mut cache = pool.cache();
                let mut held = Vec::with_capacity(1000);
                for round in 0..200u64 {
                    for i in 0..1000 {
                        held.push(cache.alloc(t << 40 | round << 20 | i));
                    }
                    for (i, pooled) in held.drain(..).enumerate() {
                        assert_eq!(*pooled, t << 40 | round << 20 | i as u64);
                        if i % 2 == 0 {
                            cache.recycle(pooled);
                        } else {
                            drop(pooled);
                        }
                    }
                }
            });
        }
    });
    assert!(pool.carved() <= 8 * (1000 + 2 * 128) + 64 * 8, "slots are reused across threads");
}

#[test]
fn pool_full_is_an_error() {
    let pool = Pool::<u64>::new(10, Tag::Io).expect("reserve");
    let mut cache = pool.cache();
    let held: Vec<_> = (0..10).map(|i| cache.alloc(i)).collect();
    assert!(matches!(cache.try_alloc(10), Err(MemError::Exhausted { tag: Tag::Io, .. })));
    drop(held);
}

#[test]
fn pool_allocates_nothing_once_warm() {
    let pool = Pool::<[u64; 8]>::new(1 << 16, Tag::Plane).expect("reserve");
    let mut cache = pool.cache();
    let mut held = Vec::with_capacity(10_000);
    let mut round = || {
        for i in 0..10_000 {
            held.push(cache.alloc([i; 8]));
        }
        for pooled in held.drain(..) {
            cache.recycle(pooled);
        }
    };
    round();
    let (_, used) = measure(&mut round);
    assert_eq!(used.allocations, 0, "a warm pool recycles its slots");
}

// Slab and handles

#[test]
fn slab_matches_a_model_and_detects_stale_handles() {
    let mut slab = Slab::<u64>::new(1 << 16, Tag::Io).expect("reserve");
    let mut model: Vec<(Handle<u64>, u64)> = Vec::new();
    let mut stale: Vec<Handle<u64>> = Vec::new();
    let mut rng = Xorshift64Star::new(6);
    for _ in 0..200_000 {
        match rng.below(5) {
            0..=1 => {
                let value = rng.next();
                let handle = slab.insert(value);
                model.push((handle, value));
            }
            2 if !model.is_empty() => {
                let i = rng.below(model.len() as u64) as usize;
                let (handle, value) = model.swap_remove(i);
                assert_eq!(slab.remove(handle), Some(value));
                assert_eq!(slab.remove(handle), None, "a handle is stale once removed");
                stale.push(handle);
            }
            3 if !model.is_empty() => {
                let i = rng.below(model.len() as u64) as usize;
                let value = rng.next();
                *slab.get_mut(model[i].0).expect("live") = value;
                model[i].1 = value;
            }
            _ if !stale.is_empty() => {
                let handle = stale[rng.below(stale.len() as u64) as usize];
                assert_eq!(slab.get(handle), None, "a stale handle never reads a new value");
            }
            _ => {}
        }
    }
    assert_eq!(slab.len(), model.len());
    for &(handle, value) in &model {
        assert_eq!(slab.get(handle), Some(&value));
    }
    let mut listed: Vec<_> = slab.iter().map(|(h, &v)| (h, v)).collect();
    listed.sort_unstable_by_key(|&(h, _)| h);
    model.sort_unstable_by_key(|&(h, _)| h);
    assert_eq!(listed, model);
    assert!(slab.slots() <= 1 << 16);
}

#[test]
fn slab_reuses_slots_with_new_generations() {
    let mut slab = Slab::<&str>::new(16, Tag::Io).expect("reserve");
    let a = slab.insert("a");
    slab.remove(a);
    let b = slab.insert("b");
    assert_eq!(a.index(), b.index(), "the freed slot is reused first");
    assert_ne!(a.generation(), b.generation());
    assert_eq!(slab.get(a), None);
    assert_eq!(slab.get(b), Some(&"b"));
    assert_eq!(Handle::<&str>::from_bits(b.to_bits()), b);
    let c = slab.insert_with(|handle| if handle.index() == 1 { "c at 1" } else { "c" });
    assert_eq!(slab.get(c), Some(&"c at 1"));
    slab.clear();
    assert!(slab.is_empty());
    assert_eq!(slab.get(b), None);
    assert_eq!(slab.get(c), None);
}

#[test]
fn slab_drops_each_value_once() {
    let drops = Rc::new(Cell::new(0));
    let mut slab = Slab::new(64, Tag::Io).expect("reserve");
    let handles: Vec<_> =
        (0..10).map(|value| slab.insert(Counted { value, drops: Rc::clone(&drops) })).collect();
    drop(slab.remove(handles[3]));
    assert_eq!(drops.get(), 1);
    drop(slab);
    assert_eq!(drops.get(), 10);
}

#[test]
fn slab_allocates_nothing_once_warm() {
    let mut slab = Slab::<[u64; 4]>::new(1 << 16, Tag::Io).expect("reserve");
    let mut handles = Vec::with_capacity(10_000);
    let mut round = |slab: &mut Slab<[u64; 4]>| {
        for i in 0..10_000 {
            handles.push(slab.insert([i; 4]));
        }
        for handle in handles.drain(..) {
            assert!(slab.get(handle).is_some());
            slab.remove(handle);
        }
    };
    round(&mut slab);
    let (_, used) = measure(|| round(&mut slab));
    assert_eq!(used.allocations, 0, "insert, lookup and remove reuse the slab's slots");
}

// Tags

#[test]
fn tag_counters_track_live_high_water_and_rate() {
    // Only this test touches `Tag::Api`.
    let before = tag::snapshot().get(Tag::Api);
    tag::add(Tag::Api, 1000);
    tag::add(Tag::Api, 500);
    tag::sub(Tag::Api, 1200);
    tag::allocated(Tag::Api, 4000);
    let after = tag::snapshot().get(Tag::Api);
    assert_eq!(after.live, before.live + 300);
    assert!(after.high_water >= before.live + 1500);
    assert_eq!(after.allocated, before.allocated + 4000);

    tag::set_budget(Tag::Api, after.live + 10);
    assert!(!tag::over_budget(Tag::Api));
    assert_eq!(tag::headroom(Tag::Api), 10);
    tag::add(Tag::Api, 11);
    assert!(tag::over_budget(Tag::Api));
    tag::sub(Tag::Api, 311);
    tag::set_budget(Tag::Api, u64::MAX);

    let mut rate = tag::Rate::new();
    let first = rate.sample(1_000_000_000, &tag::snapshot());
    assert_eq!(first[Tag::Api.index()], 0, "no rate from one sample");
    tag::allocated(Tag::Api, 2000);
    let second = rate.sample(1_500_000_000, &tag::snapshot());
    assert_eq!(second[Tag::Api.index()], 4000, "2000 bytes in half a second");
}

#[test]
fn tag_names_round_trip() {
    for tag in Tag::ALL {
        assert_eq!(Tag::from_name(tag.name()), Some(tag));
    }
    assert_eq!(Tag::from_name("nonsense"), None);
}

// Scratch

#[test]
fn scratch_lends_distinct_buffers_when_nested_and_keeps_their_capacity() {
    scratch::with_bytes(|outer| {
        outer.extend_from_slice(b"outer");
        scratch::with_bytes(|inner| {
            assert!(inner.is_empty(), "a nested call gets its own buffer");
            inner.extend_from_slice(&[0; 4096]);
        });
        assert_eq!(outer.as_slice(), b"outer");
    });
    scratch::with_bytes(|buffer| {
        assert!(buffer.is_empty());
        assert!(buffer.capacity() >= 5, "the thread's buffer keeps what it grew to");
    });
}

#[test]
fn scratch_lent_buffers_return_to_the_shelf_of_the_thread_that_drops_them() {
    let mut lent = scratch::lend_u32s();
    assert!(lent.is_empty());
    lent.extend(0..1000);
    let grown = lent.capacity();
    let built = thread::spawn(move || {
        let lent = lent;
        assert_eq!(lent.len(), 1000, "a lent buffer moves with its owner");
        drop(lent);
        scratch::with_u32s(|ids| (ids.is_empty(), ids.capacity()))
    })
    .join()
    .expect("reader");
    assert_eq!(built, (true, grown), "the dropping thread's shelf keeps it, emptied");
    let kept = scratch::lend_bytes().into_inner();
    assert!(kept.is_empty());
}

#[test]
fn scratch_paths_are_lent_empty_and_kept_warm() {
    scratch::with_path(|path| {
        path.push("/tmp");
        path.push("blaze");
        assert!(path.ends_with("blaze"));
    });
    let (_, used) = measure(|| {
        scratch::with_path(|path| {
            assert!(path.as_os_str().is_empty());
            path.push("/tmp");
            path.push("blaze");
        })
    });
    assert_eq!(used.allocations, 0, "a warm path buffer is reused");
}

#[test]
fn scratch_lent_allocates_nothing_once_warm() {
    // Locals drop in reverse, so each buffer goes back where it was taken from.
    let work = || {
        let mut tokens = scratch::lend_u32s();
        let mut checkpoints = scratch::lend_u32s();
        tokens.extend(0..10_000);
        checkpoints.extend(0..1_000);
        tokens.len() + checkpoints.len()
    };
    work();
    let (_, used) = measure(work);
    assert_eq!(used.allocations, 0, "two lent columns come back warm");
}

#[test]
fn scratch_allocates_nothing_once_warm() {
    let work = || {
        scratch::with_u32s(|ids| {
            ids.extend(0..10_000);
            scratch::with_string(|text| {
                use std::fmt::Write as _;
                let _ = write!(text, "{} ids", ids.len());
                text.len()
            })
        })
    };
    work();
    let (_, used) = measure(work);
    assert_eq!(used.allocations, 0, "a warm scratch buffer is reused, not reallocated");
}
