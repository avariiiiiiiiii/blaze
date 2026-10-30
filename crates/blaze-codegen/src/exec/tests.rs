use blaze_ids::ContentId;
use blaze_items::CodeHash;

use super::*;
use crate::{
    Target,
    driver::reach::{Node, Reach},
};

fn key(n: u8) -> InstanceKey {
    InstanceKey(ContentId([n; 32]))
}

fn id(n: u8) -> ContentId {
    ContentId([n + 100; 32])
}

fn ran(n: u8) -> ContentId {
    ContentId([n + 200; 32])
}

/// Functions 0, 1 and 2 by key, roots 0 and 2: 0 calls 1 through its slot, 2 calls nothing.
fn built() -> Built {
    let calls = [Target::Function(key(1))];
    let none: [Target; 0] = [];
    let marks = [ContentId([9; 32]); 2];
    let refs: [&[Target]; 3] = [&calls, &none, &none];
    let reach = Reach::new((0..3).map(|n| (key(n), Node { marks, refs: refs[n as usize] })), std::iter::empty());
    Built {
        roots: vec![key(0), key(2)],
        inputs: (0..3).map(|n| (key(n), CodeHash(ran(n)))).collect(),
        versions: (0..3).map(|n| (id(n), Ok(ran(n)))).collect(),
        reach,
        ..Built::default()
    }
}

fn slots(key: &InstanceKey) -> Option<u32> {
    match key.0.0[0] {
        0 => Some(2),
        1 => Some(0),
        _ => None,
    }
}

fn tests() -> Vec<(String, usize)> {
    vec![("calls".to_owned(), 0), ("alone".to_owned(), 1)]
}

#[test]
fn a_function_s_id_is_its_identity_with_its_settings() {
    let (identity, other) = (ContentId([1; 32]), ContentId([2; 32]));
    let (debug, release) = (Settings(ContentId([3; 32])), Settings(ContentId([4; 32])));
    assert_eq!(fn_id(identity, debug), fn_id(identity, debug));
    assert_ne!(fn_id(identity, debug), fn_id(identity, release));
    assert_ne!(fn_id(identity, debug), fn_id(other, debug));
}

#[test]
fn a_bound_is_of_the_items_its_references_bind_to() {
    let items = |key: &InstanceKey| (key.0.0[0] < 3).then(|| ItemKey(ContentId([key.0.0[0] + 50; 32])));
    let calls = |callee: u8| [Target::Function(key(callee)), Target::Extern("memcpy".to_owned())];
    let of = |refs: &[Target]| bound(&Ok(ran(0)), refs, items).unwrap();
    // Instance 1 and instance 2 here are of one item: a callee's own change is not the caller's.
    let same = |key: &InstanceKey| Some(ItemKey(ContentId([key.0.0[0].min(1); 32])));
    assert_eq!(
        bound(&Ok(ran(0)), &calls(1), same).unwrap(),
        bound(&Ok(ran(0)), &[Target::Direct(key(2)), Target::Extern("memcpy".to_owned())], same).unwrap()
    );
    assert_ne!(of(&calls(1)), of(&calls(2)), "another item called");
    assert_ne!(of(&calls(1)), of(&calls(1)[..1]), "a reference more");
    assert_ne!(of(&calls(1)), bound(&Ok(ran(1)), &calls(1), items).unwrap(), "its own code");
    assert_eq!(
        of(&[Target::Coverage(0)]),
        of(&[Target::Position(blaze_format::SpanRef { anchor: ContentId([4; 32]), index: 9 })])
    );
    assert_eq!(bound(&Err("no body".to_owned()), &calls(1), items), Err("no body".to_owned()));
    assert!(bound(&Ok(ran(0)), &calls(5), items).unwrap_err().contains("no instance of"));
}

#[test]
fn a_binary_records_what_it_ran_where_it_has_slots() {
    assert!(!records(Mode::Output));
    assert!(records(Mode::Live));
    assert!(records(Mode::Tiered));
}

#[test]
fn a_table_has_every_function_each_slot_s_and_each_test_s() {
    let beside = ContentId([7; 32]);
    let table = ExecTable::new(&built(), 4, slots, beside, tests()).unwrap();
    assert_eq!(table.fns, [(id(0), ran(0)), (id(1), ran(1)), (id(2), ran(2))]);
    assert_eq!(table.slot_fn, [1, ExecTable::EMPTY, 0, ExecTable::EMPTY]);
    assert_eq!(table.beside, beside);
    assert_eq!(table.tests, [("calls".to_owned(), vec![0, 1]), ("alone".to_owned(), vec![2])]);
}

#[test]
fn a_table_is_in_identity_order_and_says_each_place_in_it() {
    // 1000 functions whose `FnId`s run against their keys: function `n` is `999 - n`th.
    let numbered = |n: u32, byte: usize| {
        let mut bytes = [0u8; 32];
        bytes[byte..byte + 4].copy_from_slice(&n.to_be_bytes());
        ContentId(bytes)
    };
    let all: Vec<Target> = (0..1000).map(|n| Target::Direct(InstanceKey(numbered(n, 0)))).collect();
    let one = [Target::Function(InstanceKey(numbered(1, 0)))];
    let none: [Target; 0] = [];
    let marks = [ContentId([9; 32]); 2];
    let reach = Reach::new(
        (0..1000).map(|n| {
            let refs = match n {
                0 => &one[..],
                999 => &all[..],
                _ => &none[..],
            };
            (InstanceKey(numbered(n, 0)), Node { marks, refs })
        }),
        std::iter::empty(),
    );
    let built = Built {
        roots: vec![InstanceKey(numbered(0, 0)), InstanceKey(numbered(999, 0))],
        inputs: (0..1000).map(|n| (InstanceKey(numbered(n, 0)), CodeHash(numbered(n, 8)))).collect(),
        versions: (0..1000).map(|n| (numbered(999 - n, 4), Ok(numbered(n, 8)))).collect(),
        reach,
        ..Built::default()
    };
    let slot_of = |key: &InstanceKey| (key.0 == numbered(0, 0)).then_some(1);
    let tests = [("few".to_owned(), 0), ("every".to_owned(), 1)];
    let table = ExecTable::new(&built, 2, slot_of, ContentId::default(), tests).unwrap();
    let ids: Vec<ContentId> = table.fns.iter().map(|it| it.0).collect();
    assert_eq!(ids, (0..1000).map(|n| numbered(n, 4)).collect::<Vec<_>>());
    assert_eq!(table.fns[999].1, numbered(0, 8), "function 0 is last");
    assert_eq!(table.slot_fn, [ExecTable::EMPTY, 999]);
    // A sort for the test reaching two of 1000, the bits for the one reaching all.
    assert_eq!(table.tests[0].1, [998, 999]);
    assert_eq!(table.tests[1].1, (0..1000).collect::<Vec<u32>>());
}

#[test]
fn a_table_is_refused_when_it_would_not_say_what_ran() {
    let zero = ContentId::default();

    let mut unbound = built();
    unbound.versions[1].1 = Err("no body".to_owned());
    assert_eq!(
        ExecTable::new(&unbound, 4, slots, zero, tests()),
        Err(ExecError::Unbound { function: key(1), why: "no body".to_owned() })
    );

    let mut twice = built();
    twice.versions[2].0 = id(0);
    assert_eq!(
        ExecTable::new(&twice, 4, slots, zero, tests()),
        Err(ExecError::Twice { function: key(2), other: key(0) })
    );

    assert_eq!(
        ExecTable::new(&built(), 2, slots, zero, tests()),
        Err(ExecError::PastTable { function: key(0), slot: 2, slots: 2 })
    );

    assert_eq!(
        ExecTable::new(&built(), 4, |key| (key.0.0[0] < 2).then_some(1), zero, tests()),
        Err(ExecError::TwoInSlot { slot: 1, function: key(1), other: key(0) })
    );

    assert_eq!(
        ExecTable::new(&built(), 4, slots, zero, [("gone".to_owned(), 2)]),
        Err(ExecError::NoRoot { test: "gone".to_owned(), root: 2 })
    );
}
