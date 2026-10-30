use super::*;
use crate::tier::region::{Bounds, Layout};

fn memory(heap: u32) -> (Region, Heap) {
    let layout = Layout::new(0, 0, Bounds { stack: 1 << 16, heap }).unwrap();
    let heap = Heap::new(layout.heap.clone());
    let mut region = Region::new(layout).unwrap();
    region.install(&[], &[]).unwrap();
    (region, heap)
}

/// A deterministic stream of sizes and alignments shaped like a macro's: mostly small
/// vectors and boxes, some strings that grow, a few large buffers.
struct Rng(u64);

impl Rng {
    fn next(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below
    }

    fn layout(&mut self) -> (u64, u64) {
        let size = match self.next(10) {
            0..=6 => 1 + self.next(64),
            7 | 8 => 1 + self.next(4096),
            _ => 1 + self.next(300_000),
        };
        (size, 1 << self.next(5))
    }
}

#[test]
fn blocks_are_aligned_inside_the_heap_and_disjoint() {
    let (mut region, mut heap) = memory(256 << 20);
    let bound = region.layout().heap.clone();
    let mut rng = Rng(7);
    let mut live: Vec<(u64, u64, u64)> = Vec::new();
    for _ in 0..5_000 {
        if !live.is_empty() && rng.next(3) == 0 {
            let (at, size, align) = live.swap_remove(rng.next(live.len() as u64) as usize);
            heap.dealloc(at, size, align).unwrap();
            continue;
        }
        let (size, align) = rng.layout();
        let at = u64::from(heap.alloc(&mut region, size, align).unwrap());
        assert!(at.is_multiple_of(align), "{at:#x} for align {align}");
        assert!(u64::from(bound.start) <= at && at + size <= u64::from(bound.end));
        region.write(at, &vec![0xee; size as usize]).unwrap();
        live.push((at, size, align));
    }
    live.sort_unstable();
    for pair in live.windows(2) {
        assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:x?} overlap");
    }
}

#[test]
fn the_last_block_freed_is_reused_first() {
    let (mut region, mut heap) = memory(16 << 20);
    let a = heap.alloc(&mut region, 24, 8).unwrap();
    let b = heap.alloc(&mut region, 24, 8).unwrap();
    assert_eq!(b, a + 32, "one class, carved in order");
    heap.dealloc(u64::from(a), 24, 8).unwrap();
    heap.dealloc(u64::from(b), 24, 8).unwrap();
    assert_eq!(heap.alloc(&mut region, 30, 4).unwrap(), b);
    assert_eq!(heap.alloc(&mut region, 17, 1).unwrap(), a);
    let big = heap.alloc(&mut region, 100_000, 8).unwrap();
    heap.dealloc(u64::from(big), 100_000, 8).unwrap();
    assert_eq!(heap.alloc(&mut region, 120_000, 16).unwrap(), big, "the same two granules");
}

#[test]
fn the_same_calls_give_the_same_addresses() {
    let run = || {
        let (mut region, mut heap) = memory(64 << 20);
        let mut rng = Rng(42);
        let mut out = Vec::new();
        let mut live = Vec::new();
        for _ in 0..2_000 {
            if !live.is_empty() && rng.next(2) == 0 {
                let (at, size, align) = live.swap_remove(rng.next(live.len() as u64) as usize);
                heap.dealloc(at, size, align).unwrap();
            } else {
                let (size, align) = rng.layout();
                let at = u64::from(heap.alloc(&mut region, size, align).unwrap());
                out.push(at - u64::from(region.layout().heap.start));
                live.push((at, size, align));
            }
        }
        out
    };
    assert_eq!(run(), run());
}

#[test]
fn zeroed_blocks_are_zero_after_reuse_and_realloc_keeps_bytes() {
    let (mut region, mut heap) = memory(16 << 20);
    let a = heap.alloc(&mut region, 64, 8).unwrap();
    region.write(u64::from(a), &[0xff; 64]).unwrap();
    heap.dealloc(u64::from(a), 64, 8).unwrap();
    let z = heap.alloc_zeroed(&mut region, 64, 8).unwrap();
    assert_eq!(z, a);
    assert_eq!(region.read(u64::from(z), 64).unwrap(), &[0; 64]);

    let bytes: Vec<u8> = (0..100).collect();
    region.write(u64::from(z), &bytes[..64]).unwrap();
    let same = heap.realloc(&mut region, u64::from(z), 64, 8, 60).unwrap();
    assert_eq!(same, z, "one class: in place");
    let moved = heap.realloc(&mut region, u64::from(z), 60, 8, 5_000).unwrap();
    assert_ne!(moved, z);
    assert_eq!(region.read(u64::from(moved), 60).unwrap(), &bytes[..60]);
}

#[test]
fn the_heap_stops_at_its_bound() {
    let (mut region, mut heap) = memory(1 << 20);
    let err = heap.alloc(&mut region, 2 << 20, 8).unwrap_err();
    assert!(err.0.contains("bytes of heap"), "{err}");
    for _ in 0..16 {
        heap.alloc(&mut region, 60_000, 8).unwrap();
    }
    assert!(heap.alloc(&mut region, 60_000, 8).is_err(), "16 granules are 1 MiB");
}

#[test]
fn what_was_never_given_is_refused() {
    let (mut region, mut heap) = memory(16 << 20);
    let a = heap.alloc(&mut region, 32, 8).unwrap();
    assert!(heap.dealloc(u64::from(a) + 8, 32, 8).is_err(), "misaligned for its class");
    assert!(heap.dealloc(0x10, 32, 8).is_err(), "below the heap");
    assert!(heap.dealloc(u64::from(a) + (1 << 20), 32, 8).is_err(), "above what was handed out");
    assert!(heap.alloc(&mut region, 8, 3).is_err(), "alignment not a power of two");
    assert!(heap.c_free(u64::from(a)).is_err(), "not malloc's");
    heap.c_free(0).unwrap();
}

#[test]
fn malloc_keeps_sizes_for_free_and_realloc() {
    let (mut region, mut heap) = memory(16 << 20);
    let a = heap.c_alloc(&mut region, 10, 16, false).unwrap();
    region.write(u64::from(a), b"0123456789").unwrap();
    let b = heap.c_realloc(&mut region, u64::from(a), 1000).unwrap();
    assert_eq!(region.read(u64::from(b), 10).unwrap(), b"0123456789");
    assert!(heap.c_free(u64::from(a)).is_err(), "moved");
    heap.c_free(u64::from(b)).unwrap();
    let c = heap.c_realloc(&mut region, 0, 8).unwrap();
    heap.c_free(u64::from(c)).unwrap();
}

#[test]
fn a_warm_expansion_allocates_nothing_on_the_host() {
    let (mut region, mut heap) = memory(64 << 20);
    let expansion = |region: &mut Region, heap: &mut Heap| {
        let mut rng = Rng(3);
        let mut live = [(0u64, 0u64, 0u64); 64];
        for slot in &mut live {
            let (size, align) = rng.layout();
            *slot = (u64::from(heap.alloc(region, size, align).unwrap()), size, align);
        }
        for (at, size, align) in live {
            heap.dealloc(at, size, align).unwrap();
        }
        heap.reset();
    };
    expansion(&mut region, &mut heap);
    let ((), used) = blaze_alloc_count::measure(|| expansion(&mut region, &mut heap));
    assert_eq!(used.allocations, 0, "a warm heap allocates no host memory");
}
