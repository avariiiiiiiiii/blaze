use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
};

use proptest::prelude::*;

use super::*;

/// The spec: `blaze-link`'s `Csr::build` (`native.rs`) as it was before it moved here, the
/// plainest of the copies (count, prefix sum, place with the starts as cursors, shift back).
fn reference(rows: usize, pairs: &[(u32, u32)]) -> (Vec<u32>, Vec<u32>) {
    let mut at = vec![0u32; rows + 1];
    for (row, _) in pairs {
        at[*row as usize + 1] += 1;
    }
    for row in 0..rows {
        at[row + 1] += at[row];
    }
    let mut items = vec![0; pairs.len()];
    for (row, item) in pairs {
        let slot = &mut at[*row as usize];
        items[*slot as usize] = *item;
        *slot += 1;
    }
    for row in (1..=rows).rev() {
        at[row] = at[row - 1];
    }
    at[0] = 0;
    (at, items)
}

/// Rows and pairs with every row below them, some rows empty and some with many pairs.
fn rows_and_pairs() -> impl Strategy<Value = (usize, Vec<(u32, u32)>)> {
    (1usize..80).prop_flat_map(|rows| {
        (Just(rows), proptest::collection::vec((0..rows as u32, any::<u32>()), 0..400))
    })
}

proptest! {
    #[test]
    fn build_is_the_reference_counting_sort((rows, pairs) in rows_and_pairs()) {
        let csr = Csr::build(rows, pairs.iter().copied());
        let (starts, items) = reference(rows, &pairs);
        prop_assert_eq!(csr.starts(), &starts[..]);
        prop_assert_eq!(csr.items(), &items[..]);
        prop_assert_eq!(csr.rows(), rows);
        for row in 0..rows {
            let want: Vec<u32> = pairs.iter().filter(|it| it.0 as usize == row).map(|it| it.1).collect();
            prop_assert_eq!(csr.row(row), &want[..]);
            prop_assert_eq!(csr.get(row), Some(&want[..]));
        }
        prop_assert_eq!(csr.get(rows), None);
    }

    #[test]
    fn a_rebuild_is_a_build((rows, pairs) in rows_and_pairs(), (rows2, pairs2) in rows_and_pairs()) {
        let mut csr = Csr::build(rows, pairs.iter().copied());
        let mut cursors = Vec::new();
        csr.rebuild(rows2, pairs2.iter().copied(), &mut cursors);
        prop_assert_eq!(csr, Csr::build(rows2, pairs2.iter().copied()));
    }

    #[test]
    fn a_graph_is_its_edges_sorted((nodes, pairs) in rows_and_pairs()) {
        let edges: Vec<(u32, u32)> = pairs.iter().map(|&(s, t)| (s, t % nodes as u32)).collect();
        let graph = Csr::graph(nodes, edges.iter().copied());
        let mut sorted = edges.clone();
        sorted.sort_unstable();
        let (starts, _) = reference(nodes, &sorted);
        prop_assert_eq!(graph.starts(), &starts[..]);
        prop_assert!(graph.items().iter().eq(sorted.iter().map(|it| &it.1)));

        let mut set = graph;
        set.dedup_rows();
        sorted.dedup();
        let (starts, _) = reference(nodes, &sorted);
        prop_assert_eq!(set.starts(), &starts[..]);
        prop_assert!(set.items().iter().eq(sorted.iter().map(|it| &it.1)));
    }
}

#[test]
fn no_rows_and_no_pairs() {
    let none: Csr<u8> = Csr::new();
    assert_eq!((none.rows(), none.len(), none.starts()), (0, 0, &[][..]));
    assert_eq!(none.get(0), None);
    let empty = Csr::<u8>::build(0, std::iter::empty());
    assert_eq!((empty.rows(), empty.starts()), (0, &[0][..]));
    let blank = Csr::<u8>::build(3, std::iter::empty());
    assert_eq!(blank.starts(), &[0, 0, 0, 0]);
    assert_eq!(blank.row(2), &[] as &[u8]);
    let (starts, items) = blank.into_parts();
    assert_eq!(Csr::from_parts(starts, items).rows(), 3);
}

#[test]
fn items_that_are_not_copy_are_placed_once() {
    let words = ["b", "a", "c", "d"];
    let csr = Csr::build(2, words.iter().enumerate().map(|(at, it)| ((at % 2) as u32, it.to_string())));
    assert_eq!(csr.row(0), ["b", "c"]);
    assert_eq!(csr.row(1), ["a", "d"]);
}

#[test]
#[should_panic(expected = "is not below its 2 rows")]
fn a_row_past_the_rows_is_a_bug() {
    Csr::build(2, [(0, ()), (2, ())]);
}

#[test]
#[should_panic(expected = "not a prefix sum")]
fn parts_that_are_not_a_csr_are_refused() {
    Csr::from_parts(vec![0, 2, 1], vec![1u8]);
}

/// Pairs whose second walk moves one pair to another row: each would leave a place
/// unwritten, and each stops with a panic instead.
#[test]
fn a_second_walk_with_other_rows_panics() {
    for (first, second) in [(0u32, 1u32), (1, 0), (2, 0)] {
        let walks = Cell::new(0usize);
        let pairs = (0..6u32).map(|at| {
            if at == 0 {
                walks.set(walks.get() + 1);
            }
            let row = if at == 3 { if walks.get() == 1 { first } else { second } } else { at % 3 };
            (row, at)
        });
        let caught = catch_unwind(AssertUnwindSafe(|| Csr::build(3, pairs)));
        assert!(caught.is_err(), "{first} then {second}");
    }
    let walks = Cell::new(0usize);
    let shorter = (0..6u32)
        .inspect(|&at| {
            if at == 0 {
                walks.set(walks.get() + 1);
            }
        })
        .take_while(|&at| walks.get() == 1 || at < 5)
        .map(|at| (at % 2, at));
    assert!(catch_unwind(AssertUnwindSafe(|| Csr::build(2, shorter))).is_err());
}

#[test]
fn dedup_keeps_each_rows_first_of_a_run() {
    let mut csr = Csr::build(3, [(0, 1u32), (0, 1), (0, 2), (2, 2), (2, 2), (0, 1)]);
    csr.dedup_rows();
    assert_eq!(csr.row(0), [1, 2, 1]);
    assert_eq!(csr.row(1), [] as [u32; 0]);
    assert_eq!(csr.row(2), [2]);
    assert_eq!(csr.len(), 4);
}

/// Once its columns and cursors have the room, a rebuild (and a graph's) allocates nothing.
#[test]
fn a_warm_rebuild_allocates_nothing() {
    let pairs: Vec<(u32, u32)> = (0..10_000u32).map(|at| (at.wrapping_mul(2_654_435_761) % 500, at)).collect();
    let (mut csr, mut cursors) = (Csr::new(), Vec::new());
    csr.rebuild(500, pairs.iter().copied(), &mut cursors);
    let (mut graph, mut by_target) = (Csr::new(), Csr::new());
    graph.rebuild_graph(500, pairs.iter().map(|&(s, t)| (s, t % 500)), &mut by_target, &mut cursors);
    blaze_alloc_count::assert_at_most(0, "a warm CSR rebuild", || {
        for round in 0..20u32 {
            csr.rebuild(500, pairs.iter().map(|&(row, it)| ((row + round) % 500, it)), &mut cursors);
            graph.rebuild_graph(500, pairs.iter().map(|&(s, t)| (s, (t + round) % 500)), &mut by_target, &mut cursors);
        }
    });
    assert_eq!(csr.len(), pairs.len());
    assert_eq!(graph.len(), pairs.len());
}
