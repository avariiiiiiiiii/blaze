use std::collections::BTreeMap;

use super::*;

pub(crate) fn id(n: u32) -> FnId {
    FnId(ContentId::of("blaze v2 testsel test fn 1", &n.to_le_bytes()))
}

pub(crate) fn bound(n: u32, version: u32) -> Bound {
    Bound(ContentId::of("blaze v2 testsel test bound 1", &[n.to_le_bytes(), version.to_le_bytes()].concat()))
}

fn version(n: u32, v: u32) -> Version {
    Version { id: id(n), bound: bound(n, v) }
}

#[test]
fn the_lookup_finds_what_a_map_finds() {
    for count in [0u32, 1, 2, 7, 8, 9, 100, 1000] {
        let fns: Vec<Version> = (0..count).map(|n| version(n, n % 3)).collect();
        let image = Image::new(fns.clone(), Vec::new(), ContentId::default()).unwrap();
        let map: BTreeMap<FnId, Bound> = fns.iter().map(|it| (it.id, it.bound)).collect();
        for n in 0..count + 50 {
            assert_eq!(image.bound_of(&id(n)), map.get(&id(n)).copied(), "{n} of {count}");
        }
    }
}

#[test]
fn a_table_that_is_not_one_is_refused() {
    let fns = vec![version(0, 0), version(1, 0)];
    assert_eq!(Image::new(fns.clone(), vec![2], ContentId::default()).err(), Some(TableError::NoFunction { slot: 0, function: 2 }));
    assert_eq!(Image::new(fns.clone(), vec![1, 1], ContentId::default()).err(), Some(TableError::TwoSlots { function: 1 }));
    assert_eq!(Image::new(vec![version(0, 0), version(0, 1)], vec![], ContentId::default()).err(), Some(TableError::Twice(id(0))));
    let image = Image::new(fns, vec![1], ContentId::default()).unwrap();
    assert_eq!(image.reached(&[5]).err(), Some(TableError::NotInTable(5)));
    assert_eq!(image.executed(2, &[0]).err(), Some(TableError::OtherBinary { record: 2, table: 1 }));
}

#[test]
fn the_unlogged_hash_is_of_what_has_no_slot_in_any_order() {
    let beside = ContentId::of("blaze v2 testsel test image 1", b"deps");
    let a = Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0], beside).unwrap();
    let b = Image::new(vec![version(2, 0), version(0, 0), version(1, 0)], vec![1], beside).unwrap();
    assert_eq!(a.unlogged(), b.unlogged());
    // A logged function's edit leaves it; an unlogged one's, or the code beside, does not.
    let logged = Image::new(vec![version(0, 1), version(1, 0), version(2, 0)], vec![0], beside).unwrap();
    assert_eq!(a.unlogged(), logged.unlogged());
    let unlogged = Image::new(vec![version(0, 0), version(1, 1), version(2, 0)], vec![0], beside).unwrap();
    assert_ne!(a.unlogged(), unlogged.unlogged());
    let other = Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0], ContentId::default()).unwrap();
    assert_ne!(a.unlogged(), other.unlogged());
    // Moving a function behind a slot changes what is unlogged.
    let moved = Image::new(vec![version(0, 0), version(1, 0), version(2, 0)], vec![0, 1], beside).unwrap();
    assert_ne!(a.unlogged(), moved.unlogged());
}

#[test]
fn sets_are_sorted_round_trip_and_name_what_changed() {
    let fns: Vec<Version> = (0..10).map(|n| version(n, 0)).collect();
    let slots: Vec<u32> = (0..8).rev().collect();
    let image = Image::new(fns, slots, ContentId::default()).unwrap();
    let set = image.executed(8, &[0, 3, 7]).unwrap();
    assert_eq!(set.fns.len(), 3);
    assert!(set.fns.windows(2).all(|it| it[0] < it[1]));
    let mut want: Vec<Version> = [7, 4, 0].iter().map(|n| version(*n, 0)).collect();
    want.sort();
    assert_eq!(set.fns, want);
    let bytes = set.encode();
    let view = SetView::decode(&bytes).unwrap();
    assert_eq!(view.to_set(), set);
    assert_eq!(view.len(), 3);
    assert!(SetView::decode(&bytes[..bytes.len() - 1]).is_err());
    assert!(SetView::decode(b"blaze v2 fnset 2").is_err());
    assert_eq!(image.changed(&view), None);
    // The reached set leaves out what has no slot (8 and 9).
    assert_eq!(image.reached(&[9, 8, 7, 4, 0, 4]).unwrap(), set);

    let edited: Vec<Version> = (0..10).map(|n| version(n, u32::from(n == 4))).collect();
    let now = Image::new(edited, (0..8).rev().collect(), ContentId::default()).unwrap();
    assert_eq!(now.changed(&view), Some(Change::Changed(id(4))));
    let fewer: Vec<Version> = (0..10).filter(|n| *n != 7).map(|n| version(n, 0)).collect();
    let gone = Image::new(fewer, (0..7).collect(), ContentId::default()).unwrap();
    assert_eq!(gone.changed(&view), Some(Change::Gone(id(7))));
    let unlogged: Vec<Version> = (0..10).map(|n| version(n, u32::from(n == 9))).collect();
    let deps = Image::new(unlogged, (0..8).rev().collect(), ContentId::default()).unwrap();
    assert_eq!(deps.changed(&view), Some(Change::Unlogged));
}

#[test]
fn a_record_keys_by_what_it_executed_only_when_whole() {
    let image = Image::new(vec![version(0, 0), version(1, 0)], vec![0, 1], ContentId::default()).unwrap();
    let whole = Record::Complete { slots: 2, hits: vec![1] };
    assert_eq!(image.of_record(&whole), Some(Ok(Set::of(image.unlogged(), vec![version(1, 0)]))));
    assert_eq!(image.of_record(&Record::incomplete("killed")), None);
}
