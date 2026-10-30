use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use blaze_core::Traced;
use blaze_ids::ContentId;
use blaze_usercode::{
    test::{Lines, Trace},
    trace::{self, Inputs, Seen},
};

use super::traced::{Common, Splice};

/// Counts allocations for the budget tests here; the only global allocator of blaze-cargo's
/// tests.
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The machine a test reads: `/w/p{n}` is nothing for `n % 3 == 0`, a file for 1 and a
/// directory for 2; the environment is `V0` to `V3`.
struct World {
    env: Vec<(OsString, OsString)>,
}

impl World {
    fn new() -> World {
        World { env: (0..4).map(|it| (OsString::from(format!("V{it}")), OsString::from(format!("value {it}")))).collect() }
    }
}

fn kind(path: &Path) -> u8 {
    path.to_str().and_then(|it| it.strip_prefix("/w/p")).and_then(|it| it.parse::<u8>().ok()).map_or(0, |it| it % 3)
}

fn hash(what: &'static str, path: &Path) -> [u8; 32] {
    ContentId::of(what, path.as_os_str().as_encoded_bytes()).0
}

impl Inputs for World {
    type Error = Infallible;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Infallible> {
        Ok(match kind(path) {
            1 => Some(hash("file", path)),
            2 => Some(trace::DIRECTORY),
            _ => None,
        })
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Infallible> {
        Ok((kind(path) == 2).then(|| hash("listing", path)))
    }

    fn env(&mut self, name: &OsStr) -> Option<OsString> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.clone())
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        &self.env
    }

    fn names(&mut self, _: &Path) -> Result<Option<Vec<(OsString, bool)>>, Infallible> {
        Ok(None)
    }
}

/// splitmix64.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % n
    }
}

/// Lines of every kind over a few paths and names, so the two parts share many.
fn lines(rng: &mut Rng, count: u64) -> Vec<(Seen, String)> {
    (0..count)
        .map(|_| {
            let n = rng.below(12);
            match rng.below(10) {
                0 | 1 => (Seen::Read, format!("/w/p{n}")),
                2 => (Seen::Looked, format!("/w/p{n}")),
                3 | 4 => (Seen::Listed, format!("/w/p{n}")),
                5 if n == 0 => (Seen::Env, trace::ALL_ENV.to_owned()),
                5 | 6 => (Seen::Env, format!("V{}", n % 6)),
                7 => (Seen::Untraceable, ["clock", "random", "network", "descriptor", "other"][n as usize % 5].to_owned()),
                8 => (Seen::Incomplete, "a record cut short".to_owned()),
                _ => (Seen::RefusedWrite, format!("/w/p{n}")),
            }
        })
        .collect()
}

fn assembled(lines: &[(Seen, String)]) -> Traced {
    let Ok(traced) = trace::assemble_seen(lines.iter().map(|it| (it.0, it.1.as_str())), &mut World::new());
    traced
}

fn increasing<T: Ord>(list: &[T]) -> bool {
    list.windows(2).all(|it| it[0] < it[1])
}

/// A server's shared lines assembled once with each test's own spliced in are what one
/// assembly of all of them gives, as the store keeps it (each list strictly increasing); taken
/// out again, the shared part is as it was.
#[test]
fn a_splice_is_what_one_assembly_of_both_parts_gives() {
    let mut rng = Rng(7);
    let mut splice = Splice::default();
    for _ in 0..2000 {
        let count = rng.below(48);
        let all = lines(&mut rng, count);
        let cut = rng.below(all.len() as u64 + 1) as usize;
        let mut base = assembled(&all[..cut]);
        let before = base.clone();
        splice.put(&mut base, assembled(&all[cut..]));
        assert_eq!(base, assembled(&all), "{all:?} cut at {cut}");
        assert!(increasing(&base.files) && increasing(&base.dirs), "{base:?}");
        assert!(increasing(&base.env) && increasing(&base.untraceable), "{base:?}");
        splice.undo(&mut base);
        assert_eq!(base, before, "{all:?} cut at {cut}");
    }
}

/// A file the shared part listed and found nothing at and the own part read keeps the read's
/// value while lent, and its listing's again after.
#[test]
fn a_read_of_what_a_listing_found_nothing_at_stands() {
    let listed = [(Seen::Listed, "/w/p1".to_owned())];
    let read = [(Seen::Read, "/w/p1".to_owned())];
    let mut base = assembled(&listed);
    assert_eq!(base.files, vec![(PathBuf::from("/w/p1"), None)]);
    let mut splice = Splice::default();
    splice.put(&mut base, assembled(&read));
    assert_eq!(base, assembled(&[listed[0].clone(), read[0].clone()]));
    assert_eq!(base.files, vec![(PathBuf::from("/w/p1"), Some(hash("file", Path::new("/w/p1"))))]);
    splice.undo(&mut base);
    assert_eq!(base, assembled(&listed));
}

/// Once its lists have grown to a result's size, a splice and its undo allocate nothing.
#[test]
fn a_warm_splice_allocates_nothing() {
    let shared: Vec<(Seen, String)> = (0..2000).map(|it| (Seen::Read, format!("/lib/{it:05}"))).collect();
    let own: Vec<(Seen, String)> = [7, 900, 1999].iter().map(|it| (Seen::Read, format!("/lib/{it:05}x"))).collect();
    let mut base = assembled(&shared);
    let mut splice = Splice::default();
    splice.put(&mut base, assembled(&own));
    splice.undo(&mut base);
    let own = assembled(&own);
    let ((), used) = blaze_alloc_count::measure(|| {
        splice.put(&mut base, own);
        splice.undo(&mut base);
    });
    assert_eq!(used.allocations, 0, "a warm splice of 3 files into 2000");
    assert_eq!(base, assembled(&shared));
}

/// A test whose pass is kept is reported as the agent API marks a cached result: not run now,
/// and the pass that stands with how long it took and what it was kept by.
#[test]
fn a_kept_pass_is_reported_cached() {
    use blaze_api::{Basis, TestOutcome};

    let result = super::cached("tests/parse", "parse::nested", Some(std::time::Duration::from_micros(9_999)));
    assert_eq!((result.outcome, result.millis, result.output.len()), (TestOutcome::Unreached, 0, 0));
    assert_eq!((result.binary.as_str(), result.name.as_str()), ("tests/parse", "parse::nested"));
    let cached = result.cached.expect("a kept pass is cached");
    assert_eq!((cached.outcome, cached.millis), (TestOutcome::Passed, 9));
    assert!(matches!(cached.basis, Basis::Reached { why } if why == super::REACHED));
    assert_eq!(super::cached("a", "b", None).cached.map(|it| it.millis), Some(0));
}

fn of(lines: &[(Seen, String)]) -> Lines {
    lines.iter().map(|it| (it.0, it.1.as_str())).collect()
}

/// Through the cache, every kind of trace is what [`Trace::traced`] says it is, result after
/// result.
#[test]
fn a_result_is_traced_as_its_trace_says() {
    let mut rng = Rng(11);
    let mut common = Common::default();
    let mut world = World::new();
    for _ in 0..200 {
        let count = rng.below(32);
        let all = lines(&mut rng, count);
        let Ok(expected) = Trace::Lines(of(&all)).traced(&mut world);
        let Ok(got) = common.traced(Trace::Lines(of(&all)), &mut world, |it| it.cloned());
        assert_eq!(got, expected, "{all:?}");
    }
    let whole = assembled(&lines(&mut rng, 20));
    let Ok(got) = common.traced(Trace::Whole(whole.clone()), &mut world, |it| it.cloned());
    assert_eq!(got, Some(whole));
    let Ok(got) = common.traced(Trace::Unknown("the sandbox's record was cut".to_owned()), &mut world, |it| it.cloned());
    assert_eq!(got, None);
}
