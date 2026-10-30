use super::*;

const SMALL: Bounds = Bounds { stack: 1 << 20, heap: 64 << 20 };

fn region(writable: &[u8], readonly: &[u8]) -> Region {
    let layout = Layout::new(writable.len() as u32, readonly.len() as u32, SMALL).unwrap();
    let mut region = Region::new(layout).unwrap();
    region.install(writable, readonly).unwrap();
    region
}

#[test]
fn the_layout_is_ordered_aligned_and_guarded() {
    let layout = Layout::new(100, 70_000, Bounds::DEFAULT).unwrap();
    let g = GRANULE;
    assert_eq!(layout.writable, 2 * g..2 * g + 100);
    assert_eq!(layout.readonly.start, 3 * g);
    assert_eq!(layout.readonly.end, 3 * g + 70_000);
    // One granule of guard below the stack, past the read-only part's last granule.
    assert_eq!(layout.stack.start, 5 * g + g);
    assert_eq!(layout.stack.len() as u32, Bounds::DEFAULT.stack);
    assert_eq!(layout.heap.start, layout.stack.end);
    assert_eq!(layout.heap.len() as u32, Bounds::DEFAULT.heap);
    for part in [&layout.stack, &layout.heap] {
        assert_eq!(part.start % g, 0);
        assert_eq!(part.end % g, 0);
    }
    assert!(u64::from(layout.heap.end) <= SPAN);
}

#[test]
fn a_layout_past_four_gibibytes_is_refused() {
    let too_big = Bounds { stack: 8 << 20, heap: u32::MAX - (1 << 20) };
    let err = Layout::new(0, 0, too_big).unwrap_err();
    assert!(err.0.contains("4 GiB"), "{err}");
}

#[test]
fn the_base_is_aligned_to_the_span() {
    let region = region(&[1; 16], &[2; 16]);
    assert_eq!(region.base() % SPAN, 0);
}

#[test]
fn null_guards_and_unmapped_space_are_refused() {
    let region = region(&[0; 8], &[0; 8]);
    assert!(region.read(0, 1).is_err(), "null");
    assert!(region.read(u64::from(GRANULE) - 1, 2).is_err(), "straddles the null guard");
    let stack = &region.layout().stack;
    assert!(region.read(u64::from(stack.start) - 1, 1).is_err(), "the guard below the stack");
    assert!(region.read(u64::from(region.layout().heap.start), 1).is_err(), "heap not grown");
    assert!(region.read(SPAN - 8, 8).is_err(), "never mapped");
    assert!(region.read(SPAN, 1).is_err(), "past the span");
    assert!(region.read(u64::MAX, 2).is_err(), "wraps");
    assert_eq!(region.read(0, 0).unwrap(), &[] as &[u8], "an empty read is anywhere");
}

#[test]
fn constants_are_read_only_and_statics_writable() {
    let mut region = region(&[7; 32], &[9; 32]);
    let layout = region.layout().clone();
    assert_eq!(region.read(u64::from(layout.readonly.start), 32).unwrap(), &[9; 32]);
    assert!(region.write(u64::from(layout.readonly.start), &[1]).is_err());
    region.write(u64::from(layout.writable.start) + 4, &[1, 2]).unwrap();
    assert_eq!(region.read(u64::from(layout.writable.start) + 3, 4).unwrap(), &[7, 1, 2, 7]);
    // A write running past the statics' end is refused whole.
    assert!(region.write(u64::from(layout.writable.end) - 1, &[0, 0]).is_err());
}

#[test]
fn a_second_image_is_refused() {
    let mut region = region(&[0; 8], &[0; 8]);
    assert!(region.install(&[0; 8], &[0; 8]).is_err());
}

#[test]
fn reset_restores_statics_and_cells() {
    let mut region = region(&[5; 16], &[]);
    let layout = region.layout().clone();
    let sp = u64::from(Layout::HEADER + cell::SHADOW_SP);
    let errno = u64::from(Layout::HEADER + cell::ERRNO);
    region.write(u64::from(layout.writable.start), &[0; 16]).unwrap();
    region.write(sp, &0u64.to_le_bytes()).unwrap();
    region.write(errno, &5i32.to_le_bytes()).unwrap();
    region.reset(&[5; 16]).unwrap();
    assert_eq!(region.read(u64::from(layout.writable.start), 16).unwrap(), &[5; 16]);
    assert_eq!(region.read(sp, 8).unwrap(), &u64::from(layout.stack.end).to_le_bytes());
    let limit = u64::from(Layout::HEADER + cell::SHADOW_LIMIT);
    assert_eq!(region.read(limit, 8).unwrap(), &u64::from(layout.stack.start).to_le_bytes());
    assert_eq!(region.read(errno, 4).unwrap(), &[0; 4]);
    assert_eq!(region.read(u64::from(Layout::HEADER + cell::ZERO), 8).unwrap(), &[0; 8]);
}

#[test]
fn the_heap_maps_as_it_grows_and_reads_zeros_after_a_shrink() {
    let mut region = region(&[], &[]);
    let heap = u64::from(region.layout().heap.start);
    region.grow_heap(100).unwrap();
    assert!(region.committed() >= 100 && region.committed() % GRANULE == 0);
    region.write(heap + 99, &[3]).unwrap();
    assert!(region.read(heap + u64::from(region.committed()), 1).is_err());
    region.grow_heap(4 * GRANULE).unwrap();
    region.write(heap + u64::from(3 * GRANULE), &[4]).unwrap();
    region.shrink_heap(GRANULE).unwrap();
    assert_eq!(region.committed(), GRANULE);
    assert!(region.read(heap + u64::from(3 * GRANULE), 1).is_err());
    region.grow_heap(4 * GRANULE).unwrap();
    assert_eq!(region.read(heap + u64::from(3 * GRANULE), 1).unwrap(), &[0]);
    assert_eq!(region.read(heap + 99, 1).unwrap(), &[3], "the kept granule keeps its bytes");
}

#[test]
fn the_heap_stops_at_its_bound() {
    let mut region = region(&[], &[]);
    let bound = region.layout().heap.len() as u32;
    region.grow_heap(bound).unwrap();
    let err = region.grow_heap(bound + 1).unwrap_err();
    assert!(err.0.contains("heap"), "{err}");
}

/// The safe reference: each part as a vector, checked against the region over random
/// writes, fills and moves.
#[test]
fn the_region_matches_a_model_of_its_parts() {
    let writable: Vec<u8> = (0..300).map(|it| it as u8).collect();
    let mut region = region(&writable, &[0xaa; 50]);
    region.grow_heap(GRANULE).unwrap();
    let layout = region.layout().clone();
    let heap = region.committed() as usize;
    let parts = [
        (u64::from(layout.writable.start), layout.writable.len()),
        (u64::from(layout.stack.end) - 4096, 4096),
        (u64::from(layout.heap.start), heap),
    ];
    let mut model: Vec<Vec<u8>> = vec![writable.clone(), vec![0; 4096], vec![0; heap]];
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move |below: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % below as u64) as usize
    };
    for _ in 0..2_000 {
        let part = next(3);
        let (base, len) = parts[part];
        let at = next(len);
        let n = next(len - at + 8);
        let ok = at + n <= len;
        match next(3) {
            0 => {
                let bytes: Vec<u8> = (0..n).map(|_| next(256) as u8).collect();
                assert_eq!(region.write(base + at as u64, &bytes).is_ok(), ok || n == 0);
                if ok {
                    model[part][at..at + n].copy_from_slice(&bytes);
                }
            }
            1 => {
                let byte = next(256) as u8;
                assert_eq!(region.fill(base + at as u64, n, byte).is_ok(), ok || n == 0);
                if ok {
                    model[part][at..at + n].fill(byte);
                }
            }
            _ => {
                let to = next(len);
                let fits = ok && to + n <= len;
                let moved = region.copy_within(base + at as u64, base + to as u64, n).is_ok();
                assert_eq!(moved, fits || n == 0);
                if fits {
                    model[part].copy_within(at..at + n, to);
                }
            }
        }
    }
    for (part, (base, len)) in parts.iter().enumerate() {
        assert_eq!(region.read(*base, *len).unwrap(), &model[part][..]);
    }
}
