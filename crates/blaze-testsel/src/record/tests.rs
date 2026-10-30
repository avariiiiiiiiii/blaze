use super::*;

/// splitmix64: a seed's sequence, the same on every machine.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The byte-at-a-time scan [`hits_of`] must equal.
fn reference(map: &[u8]) -> Vec<u32> {
    map.iter().enumerate().filter(|it| *it.1 != 0).map(|it| it.0 as u32).collect()
}

#[test]
fn the_word_scan_finds_what_the_byte_scan_finds() {
    let mut state = 7;
    for len in 0..200usize {
        for density in [0u64, 1, 8, 64, 255] {
            let map: Vec<u8> = (0..len)
                .map(|_| {
                    let roll = next(&mut state);
                    match roll % 256 < density {
                        // Any value but zero is a hit, the high bit alone included.
                        true => [1u8, 0x80, 0xff, 0x7f, 2][(roll >> 8) as usize % 5],
                        false => 0,
                    }
                })
                .collect();
            let mut out = vec![9];
            hits_of(&map, &mut out);
            assert_eq!(out[0], 9, "added to what was there");
            assert_eq!(out[1..], reference(&map)[..], "length {len}, density {density}");
        }
    }
}

#[test]
fn a_region_is_whole_only_when_armed_and_ended() {
    let mut region = header(20).to_vec();
    region.resize(HEADER + 20, 0);
    region[HEADER + 3] = 1;
    region[HEADER + 19] = 1;
    assert!(Record::of_region(&region, Ok(())).why().unwrap().contains("armed 0 of its 20"));
    assert!(mark_armed(&mut region));
    assert_eq!(Record::of_region(&region, Ok(())), Record::Complete { slots: 20, hits: vec![3, 19] });
    assert!(Record::of_region(&region, Err("ended by signal 9")).why().unwrap().contains("signal 9"));
    assert!(Record::of_region(&region[..HEADER + 10], Ok(())).why().is_some());
    assert!(Record::of_region(&region[..4], Ok(())).why().is_some());
    let mut other = region.clone();
    other[0] = b'x';
    assert!(Record::of_region(&other, Ok(())).why().unwrap().contains("magic"));
    assert!(!mark_armed(&mut other));
    assert!(!mark_armed(&mut [0u8; 3]));
}

#[test]
fn hits_out_of_order_or_range_are_no_record() {
    assert_eq!(Record::of_hits(4, vec![0, 3]), Record::Complete { slots: 4, hits: vec![0, 3] });
    assert_eq!(Record::of_hits(4, Vec::new()), Record::Complete { slots: 4, hits: Vec::new() });
    assert!(Record::of_hits(4, vec![4]).why().unwrap().contains("slot 4"));
    assert!(Record::of_hits(4, vec![2, 2]).why().unwrap().contains("out of order"));
    assert!(Record::of_hits(4, vec![3, 1]).why().is_some());
}
