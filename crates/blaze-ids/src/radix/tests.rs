use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use super::*;

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

/// Says it has `self.0` threads and runs the jobs in turn: the parallel path on one thread.
struct Pretend(usize);

// SAFETY: as `Serial`'s.
unsafe impl Workers for Pretend {
    fn threads(&self) -> usize {
        self.0
    }

    fn run(&self, jobs: usize, job: &(dyn Fn(usize) + Sync)) {
        (0..jobs).for_each(job);
    }
}

/// `self.0` threads, each taking the next job until none is left.
struct Threads(usize);

// SAFETY: each index is taken once by one `fetch_add`; `thread::scope` joins every thread,
// and raises a panic of one, before it returns.
unsafe impl Workers for Threads {
    fn threads(&self) -> usize {
        self.0
    }

    fn run(&self, jobs: usize, job: &(dyn Fn(usize) + Sync)) {
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..self.0 {
                scope.spawn(|| {
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        if at >= jobs {
                            break;
                        }
                        job(at);
                    }
                });
            }
        });
    }
}

/// The shapes of input a sort meets: random, few distinct keys (where stability shows), only
/// some high bits set (skipped low digits), constant, in order, reversed, nearly in order.
fn keys(rng: &mut Rng, n: usize, shape: u64) -> Vec<u64> {
    let mut keys: Vec<u64> = match shape {
        0 => (0..n).map(|_| rng.next()).collect(),
        1 => (0..n).map(|_| rng.below(7)).collect(),
        2 => (0..n).map(|_| rng.below(1 << 12) << 40).collect(),
        3 => vec![rng.next(); n],
        4 => (0..n as u64).map(|it| it * 3).collect(),
        5 => (0..n as u64).rev().collect(),
        6 => (0..n).map(|_| rng.below(1 << 20) | (rng.below(4) << 62)).collect(),
        _ => {
            let mut it: Vec<u64> = (0..n as u64).collect();
            for _ in 0..n / 50 + 1 {
                let (a, b) = (rng.below(n as u64) as usize, rng.below(n as u64) as usize);
                it.swap(a, b);
            }
            it
        }
    };
    keys.truncate(n);
    keys
}

const SHAPES: u64 = 8;

fn sizes() -> Vec<usize> {
    let mut sizes: Vec<usize> = (0..=2 * SMALL + 3).collect();
    sizes.extend([200, 1000, 4097, WIDE - 1, WIDE, WIDE + 17]);
    sizes
}

fn pairs_of(keys: &[u64]) -> Vec<(u64, u32)> {
    keys.iter().enumerate().map(|(at, &key)| (key, at as u32)).collect()
}

fn stable(pairs: &[(u64, u32)]) -> Vec<(u64, u32)> {
    let mut it = pairs.to_vec();
    it.sort_by_key(|it| it.0);
    it
}

#[test]
fn pairs_sort_as_the_standard_stable_sort() {
    let mut rng = Rng(1);
    for n in sizes() {
        for shape in 0..SHAPES {
            let pairs = pairs_of(&keys(&mut rng, n, shape));
            let mut got = pairs.clone();
            sort_pairs(&mut got);
            assert_eq!(got, stable(&pairs), "n {n}, shape {shape}");
        }
    }
}

#[test]
fn keys_of_every_width_sort_as_the_standard_sort() {
    let mut rng = Rng(2);
    for n in sizes() {
        for shape in 0..SHAPES {
            let wide = keys(&mut rng, n, shape);
            macro_rules! check {
                ($($t:ty),*) => {$({
                    let mut got: Vec<$t> = wide.iter().map(|&it| it as $t).collect();
                    let mut want = got.clone();
                    want.sort_unstable();
                    sort(&mut got);
                    assert_eq!(got, want, "{} n {n}, shape {shape}", stringify!($t));
                })*};
            }
            check!(u8, u16, u32, u64, usize);
            let mut got: Vec<u128> =
                wide.iter().map(|&it| u128::from(it) << 64 | u128::from(it.rotate_left(17))).collect();
            let mut want = got.clone();
            want.sort_unstable();
            sort(&mut got);
            assert_eq!(got, want, "u128 n {n}, shape {shape}");
        }
    }
}

#[test]
fn sort_by_key_moves_owned_items_stably_and_keys_each_once() {
    let mut rng = Rng(3);
    for n in [0, 1, 2, 5, SMALL, SMALL + 1, 1000, WIDE + 3] {
        for shape in 0..SHAPES {
            let items: Vec<(u64, String)> = keys(&mut rng, n, shape)
                .into_iter()
                .enumerate()
                .map(|(at, key)| (key, format!("item {at}")))
                .collect();
            let mut want = items.clone();
            want.sort_by_key(|it| it.0);
            let mut got = items;
            let calls = AtomicUsize::new(0);
            sort_by_key(&mut got, |it| {
                calls.fetch_add(1, Ordering::Relaxed);
                it.0
            });
            assert_eq!(got, want, "n {n}, shape {shape}");
            assert_eq!(calls.into_inner(), if n < 2 { 0 } else { n }, "one key per item");
        }
    }
}

#[test]
fn copy_by_key_sorts_by_a_packed_tuple() {
    let mut rng = Rng(4);
    for n in [3, 100, 5000] {
        let items: Vec<(u32, i32, u16)> = (0..n)
            .map(|_| (rng.below(5) as u32, rng.next() as i32 % 9, rng.next() as u16))
            .collect();
        let mut want = items.clone();
        want.sort_by_key(|it| (it.0, it.1));
        let mut got = items;
        sort_copy_by_key(&mut got, |it| pack_32(it.0, signed_32(it.1)));
        assert_eq!(got, want, "n {n}");
    }
}

#[test]
fn the_parallel_path_is_the_same_on_any_thread_count() {
    let mut rng = Rng(5);
    for n in [PARALLEL, PARALLEL + 12_345, 250_000] {
        for shape in [0, 1, 2, 5, 6, 7] {
            let pairs = pairs_of(&keys(&mut rng, n, shape));
            let want = stable(&pairs);
            for workers in [&Pretend(2) as &dyn Workers, &Pretend(7), &Pretend(64), &Threads(3), &Threads(8)]
            {
                let mut got = pairs.clone();
                on(workers).sort_pairs(&mut got);
                assert_eq!(got, want, "n {n}, shape {shape}, {} threads", workers.threads());
            }
            let mut got: Vec<u64> = pairs.iter().map(|it| it.0).collect();
            on(&Threads(4)).sort(&mut got);
            assert!(got.iter().copied().eq(want.iter().map(|it| it.0)), "keys, n {n}, shape {shape}");
        }
    }
    let mut got = vec![7u32; PARALLEL];
    on(&Threads(4)).sort(&mut got);
    assert!(got.iter().all(|&it| it == 7));
}

#[test]
fn one_thread_takes_the_serial_path_from_parallel_sizes() {
    let mut rng = Rng(6);
    let pairs = pairs_of(&keys(&mut rng, PARALLEL + 1, 0));
    let mut got = pairs.clone();
    on(&Serial).sort_pairs(&mut got);
    assert_eq!(got, stable(&pairs));
}

static ASKED: AtomicUsize = AtomicUsize::new(0);

fn three_threads(with: &mut dyn FnMut(&dyn Workers)) -> bool {
    ASKED.fetch_add(1, Ordering::Relaxed);
    with(&Threads(3));
    true
}

/// The only test that sorts [`PARALLEL`] elements without `on`, so the count is its own.
#[test]
fn a_large_sort_runs_on_the_ambient_workers() {
    set_ambient(three_threads);
    let pairs = pairs_of(&keys(&mut Rng(8), PARALLEL + 5, 0));
    let mut got = pairs.clone();
    sort_pairs(&mut got);
    assert_eq!(got, stable(&pairs));
    assert_eq!(ASKED.load(Ordering::Relaxed), 1);
    let mut small = pairs[..PARALLEL - 1].to_vec();
    sort_pairs(&mut small);
    assert_eq!(small, stable(&pairs[..PARALLEL - 1]));
    assert_eq!(ASKED.load(Ordering::Relaxed), 1, "below PARALLEL no hook is asked");
}

#[test]
fn digits_every_key_shares_are_skipped() {
    let shifts = |plan: Plan| plan.shifts().to_vec();
    assert_eq!(shifts(plan(0u64, 1000)), Vec::<u32>::new());
    assert_eq!(shifts(plan(0xFFu64 << 40, 1000)), [40]);
    assert_eq!(shifts(plan(0x1_0000_0001u64, 1000)), [0, 32]);
    assert_eq!(shifts(plan(u32::MAX, 1000)), [0, 8, 16, 24], "8 bits below WIDE");
    assert_eq!(shifts(plan(u32::MAX, WIDE)), [0, 11, 22], "11 bits from WIDE, a pass fewer");
    assert_eq!(shifts(plan(0xFFFFu32, WIDE)), [0, 8], "two passes of 8 bits, as many of 11");
    assert_eq!(plan(u64::MAX, WIDE).len, 6);
    assert_eq!(plan(u128::MAX, 10).len, 16);
}

#[test]
fn order_helpers_keep_order() {
    let mut rng = Rng(7);
    for _ in 0..10_000 {
        let (a, b) = (rng.next() as i64, rng.next() as i64);
        assert_eq!(a.cmp(&b), signed_64(a).cmp(&signed_64(b)));
        let (c, d) = (a as i32, b as i32);
        assert_eq!(c.cmp(&d), signed_32(c).cmp(&signed_32(d)));
        let (e, f) = (rng.below(3) as u32, rng.below(3) as u32);
        assert_eq!((e, c as u32).cmp(&(f, d as u32)), pack_32(e, c as u32).cmp(&pack_32(f, d as u32)));
        let (g, h) = (rng.below(3), rng.below(3));
        assert_eq!((g, a as u64).cmp(&(h, b as u64)), pack_64(g, a as u64).cmp(&pack_64(h, b as u64)));
    }
    for edge in [i64::MIN, -1, 0, 1, i64::MAX] {
        assert!(signed_64(edge) <= signed_64(i64::MAX) && signed_64(edge) >= signed_64(i64::MIN));
    }
}

/// A key that changes as it is read is a caller's bug: the sort panics or leaves some
/// permutation of the items, never anything else.
#[test]
fn a_key_that_changes_between_reads_panics_or_permutes() {
    for (n, workers) in [(5000, &Serial as &dyn Workers), (PARALLEL + 1, &Serial), (PARALLEL + 1, &Pretend(4))]
    {
        let items: Vec<u64> = (0..n as u64).rev().collect();
        let calls = AtomicU64::new(0);
        let mut got = items.clone();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            on(workers).sort_copy_by_key(&mut got, |it| *it ^ (calls.fetch_add(1, Ordering::Relaxed) & 0x0F0F));
        }));
        got.sort_unstable();
        let mut want = items;
        want.sort_unstable();
        assert_eq!(got, want, "n {n}, {} threads", workers.threads());
    }
}
