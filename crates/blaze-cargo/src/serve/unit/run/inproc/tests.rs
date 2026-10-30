use std::sync::Arc;

use blaze_inproc::frame::{Frame, Kind, kinds};

use super::{ARENA, CELLS_PER_FUNCTION, NO_PATCH, PATCH, PATCH_ASK, Patch, answer, dev_run};

fn ask(kind: Kind, body: &[u8]) -> Frame<'_> {
    Frame { kind, seq: 7, body }
}

fn at(generation: u64) -> [u8; 8] {
    generation.to_le_bytes()
}

#[test]
fn a_dev_executable_reserves_the_arena_and_only_a_tiered_run_cells() {
    let plain = dev_run(None);
    assert_eq!((plain.arena, plain.cells, plain.fixed), (ARENA, 0, 0));
    let tiered = dev_run(Some(1000));
    assert_eq!((tiered.arena, tiered.cells, tiered.fixed), (ARENA, 1000 * CELLS_PER_FUNCTION, 0));
    assert_eq!(dev_run(Some(u64::MAX)).cells, u64::MAX, "saturates, so the link refuses it by size");
}

#[test]
fn the_patch_kinds_are_blaze_lives() {
    for kind in [PATCH_ASK, PATCH, NO_PATCH] {
        assert!(kinds::LIVE.contains(&kind.0), "{kind:?}");
    }
    assert_eq!((PATCH_ASK.0, PATCH.0, NO_PATCH.0), (0x0300, 0x0301, 0x0302));
}

#[test]
fn a_run_behind_the_newest_patch_gets_it_and_one_at_it_gets_none() {
    let message: Arc<[u8]> = Arc::from(&b"patch 3"[..]);
    let patch = Patch { generation: 3, message: Some(message.clone()) };
    let (kind, body) = answer(&ask(PATCH_ASK, &at(2)), &patch).unwrap();
    assert_eq!((kind, &*body), (PATCH, &*message));
    let (kind, body) = answer(&ask(PATCH_ASK, &at(0)), &patch).unwrap();
    assert_eq!((kind, &*body), (PATCH, &*message), "several behind: the newest, which holds them all");
    for at_or_past in [3, 4] {
        let (kind, body) = answer(&ask(PATCH_ASK, &at(at_or_past)), &patch).unwrap();
        assert_eq!((kind, body.len()), (NO_PATCH, 0), "at {at_or_past}");
    }
    let (kind, _) = answer(&ask(PATCH_ASK, &at(0)), &Patch::default()).unwrap();
    assert_eq!(kind, NO_PATCH, "no patch since the first link");
}

#[test]
fn a_patch_longer_than_an_answer_is_refused_by_size() {
    let long: Arc<[u8]> = vec![0u8; blaze_inproc::ask::MOST_BODY + 1].into();
    let patch = Patch { generation: 1, message: Some(long) };
    let why = answer(&ask(PATCH_ASK, &at(0)), &patch).unwrap_err();
    assert!(why.contains("more than an answer holds"), "{why}");
    let fits: Arc<[u8]> = vec![1u8; blaze_inproc::ask::MOST_BODY].into();
    let patch = Patch { generation: 1, message: Some(fits) };
    assert_eq!(answer(&ask(PATCH_ASK, &at(0)), &patch).unwrap().0, PATCH);
}

#[test]
fn a_bad_patch_ask_and_other_kinds_are_refused_naming_them() {
    let patch = Patch::default();
    assert!(answer(&ask(PATCH_ASK, &[1, 2, 3]), &patch).is_err(), "short");
    assert!(answer(&ask(PATCH_ASK, &[0; 9]), &patch).is_err(), "trailing");
    let lazy = answer(&ask(Kind(kinds::LAZY.start), &[]), &patch).unwrap_err();
    assert!(lazy.contains("0x0100") && lazy.contains("lazy"), "{lazy}");
    let premap = answer(&ask(Kind(kinds::PREMAP.start), &[]), &patch).unwrap_err();
    assert!(premap.contains("0x0200"), "{premap}");
    let other = answer(&ask(Kind(0x0303), &[]), &patch).unwrap_err();
    assert!(other.contains("0x0303"), "{other}");
}
