use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_cas::{Corrupt, Key, Mismatch, Ticket};
use blaze_core::{Traced, Untraceable};
use blaze_ids::ContentId;
use blaze_items::CodeHash;

use super::*;
use crate::trace::{Inputs, Line, Seen, assemble};

/// The store, as a map that refuses to replace a value.
#[derive(Default)]
struct Mem {
    values: BTreeMap<Key, Vec<u8>>,
    puts: u64,
}

impl Passes for Mem {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, Corrupt> {
        Ok(self.values.get(key).map(Vec::as_slice))
    }

    fn put(&mut self, key: Key, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        match self.values.get(&key) {
            Some(kept) if *kept != bytes => {
                return Err(Mismatch {
                    key,
                    kept: ContentId::of("blaze v2 test mem 1", kept),
                    given: ContentId::of("blaze v2 test mem 1", &bytes),
                });
            }
            Some(_) => {}
            None => _ = self.values.insert(key, bytes),
        }
        self.puts += 1;
        Ok(Ticket(self.puts))
    }
}

/// The files and environment a test sees.
#[derive(Clone, Default)]
struct World {
    files: BTreeMap<PathBuf, [u8; 32]>,
    env: Vec<(OsString, OsString)>,
}

impl World {
    fn with(file: u8, foo: &str) -> World {
        World {
            files: [(PathBuf::from("/ws/data"), [file; 32])].into(),
            env: vec![("FOO".into(), foo.into()), ("PATH".into(), "/usr/bin".into())],
        }
    }
}

impl Inputs for World {
    type Error = String;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(self.files.get(path).copied())
    }

    fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
        Ok(None)
    }

    fn env(&mut self, name: &OsStr) -> Option<OsString> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.clone())
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        self.env.sort();
        &self.env
    }

    fn names(&mut self, _: &Path) -> Result<Option<Vec<(OsString, bool)>>, String> {
        Ok(None)
    }
}

fn code(n: u8) -> CodeHash {
    CodeHash(ContentId::of("blaze v2 test code 1", &[n]))
}

fn suite() -> Suite {
    Suite { package: "parser".into(), target: "lib".into(), context: ContentId::of("blaze v2 test context 1", b"x86_64") }
}

fn test(name: &str, reached: &[u8]) -> Test {
    Test { name: name.into(), reached: reached.iter().map(|it| code(*it)).collect(), attrs: Attrs::default(), outside: false }
}

/// What a test that read `/ws/data` and `$FOO` read, in `world`.
fn reads(world: &mut World) -> Traced {
    let lines = [
        Line::new(Seen::Read, "tests::a", "/ws/data"),
        Line::new(Seen::Env, "tests::a", "FOO"),
        Line::new(Seen::Read, "", "/ws/missing"),
    ];
    assemble(&lines, None, world).unwrap()
}

fn pass(store: &mut Mem, given: ContentId, test: &Test, traced: &Traced, took: u64) {
    let ran = Ran { passed: true, took: Duration::from_millis(took), traced: Some(traced) };
    record(store, given, test, &ran).unwrap();
}

fn decide(store: &Mem, given: ContentId, test: &Test, world: &mut World) -> Decision {
    select(store, given, test, false, world).unwrap()
}

#[test]
fn a_new_test_runs_and_an_unchanged_one_is_skipped() {
    let mut store = Mem::default();
    let mut world = World::with(1, "1");
    let given = suite().given();
    let a = test("tests::a", &[1, 2, 3]);
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Run { why: Why::New, expected: None });
    let traced = reads(&mut world);
    pass(&mut store, given, &a, &traced, 40);
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Skip { took: Some(Duration::from_millis(40)) });
    // Another worktree at the same code, with the same files: a test with the same name and
    // reach, built into another binary.
    let mut elsewhere = world.clone();
    let same = Test::of(crate::Reached { test: "tests::a".into(), binary: ContentId::of("blaze v2 test binary 1", b"other"), reached: vec![code(3), code(1), code(2), code(2)] }, Attrs::default(), false);
    assert_eq!(decide(&store, given, &same, &mut elsewhere), Decision::Skip { took: Some(Duration::from_millis(40)) });
    // The same test of another package is another test.
    let other = Suite { package: "lexer".into(), ..suite() };
    assert_eq!(decide(&store, other.given(), &a, &mut world), Decision::Run { why: Why::New, expected: None });
    // And every test runs when all are asked for.
    assert_eq!(select(&store, given, &a, true, &mut world).unwrap(), Decision::Run { why: Why::All, expected: Some(Duration::from_millis(40)) });
}

#[test]
fn a_changed_callee_reruns_and_names_the_first_changed_hash() {
    let mut store = Mem::default();
    let mut world = World::with(1, "1");
    let given = suite().given();
    let traced = reads(&mut world);
    pass(&mut store, given, &test("tests::a", &[1, 2, 3]), &traced, 10);
    let edited = test("tests::a", &[1, 5, 2, 4]);
    assert_eq!(
        decide(&store, given, &edited, &mut world),
        Decision::Run { why: Why::Changed(code(5)), expected: Some(Duration::from_millis(10)) }
    );
    let fewer = test("tests::a", &[1, 2]);
    assert_eq!(decide(&store, given, &fewer, &mut world), Decision::Run { why: Why::Dropped(code(3)), expected: Some(Duration::from_millis(10)) });
    // Its run passes: the edited reach is the one skipped now, and the explanation is
    // against it.
    pass(&mut store, given, &edited, &traced, 12);
    assert_eq!(decide(&store, given, &edited, &mut world), Decision::Skip { took: Some(Duration::from_millis(12)) });
    // Going back to the first code finds its pass, still kept.
    assert_eq!(decide(&store, given, &test("tests::a", &[1, 2, 3]), &mut world), Decision::Skip { took: Some(Duration::from_millis(12)) });
}

#[test]
fn attributes_are_part_of_a_test() {
    let mut store = Mem::default();
    let mut world = World::default();
    let given = suite().given();
    let a = test("tests::a", &[1]);
    pass(&mut store, given, &a, &Traced::default(), 1);
    let mut panicking = test("tests::a", &[1]);
    panicking.attrs.should_panic = ShouldPanic::WithMessage("boom".into());
    assert_eq!(decide(&store, given, &panicking, &mut world), Decision::Run { why: Why::Attributes, expected: Some(Duration::from_millis(1)) });
    let mut other = test("tests::a", &[1]);
    other.attrs.should_panic = ShouldPanic::WithMessage("bang".into());
    assert_ne!(panicking.identity(), other.identity());
    assert_ne!(a.identity(), test("tests::b", &[1]).identity());
}

#[test]
fn the_outside_world_always_runs() {
    let mut store = Mem::default();
    let mut world = World::default();
    let given = suite().given();
    let mut a = test("tests::net", &[1]);
    a.outside = true;
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Run { why: Why::Outside, expected: None });
    pass(&mut store, given, &a, &Traced::default(), 5);
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Run { why: Why::Outside, expected: Some(Duration::from_millis(5)) });
    // And no pass was kept for it: the same test not marked has none.
    a.outside = false;
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Run { why: Why::NoPass, expected: Some(Duration::from_millis(5)) });
}

#[test]
fn a_traced_input_that_changed_reruns_and_is_named() {
    let mut store = Mem::default();
    let given = suite().given();
    let a = test("tests::a", &[1, 2]);
    let mut world = World::with(1, "1");
    let traced = reads(&mut world);
    assert!(traced.files.iter().any(|it| it.0 == Path::new("/ws/missing") && it.1.is_none()));
    pass(&mut store, given, &a, &traced, 3);
    // A variable it did not read changes nothing.
    world.env.push(("UNREAD".into(), "x".into()));
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Skip { took: Some(Duration::from_millis(3)) });
    let mut edited = World::with(2, "1");
    assert_eq!(
        decide(&store, given, &a, &mut edited),
        Decision::Run { why: Why::Read(Input::File("/ws/data".into())), expected: Some(Duration::from_millis(3)) }
    );
    let mut set = World::with(1, "2");
    assert_eq!(decide(&store, given, &a, &mut set), Decision::Run { why: Why::Read(Input::Env("FOO".into())), expected: Some(Duration::from_millis(3)) });
    // A file it looked for and did not find, there now.
    let mut found = World::with(1, "1");
    found.files.insert("/ws/missing".into(), [9; 32]);
    assert_eq!(decide(&store, given, &a, &mut found), Decision::Run { why: Why::Read(Input::File("/ws/missing".into())), expected: Some(Duration::from_millis(3)) });
    // It passes with the new file: both passes are kept, each found by its values.
    let traced = reads(&mut edited);
    pass(&mut store, given, &a, &traced, 4);
    assert_eq!(decide(&store, given, &a, &mut edited), Decision::Skip { took: Some(Duration::from_millis(4)) });
    assert_eq!(decide(&store, given, &a, &mut World::with(1, "1")), Decision::Skip { took: Some(Duration::from_millis(4)) });
}

#[test]
fn a_failure_or_what_cannot_be_kept_leaves_no_pass() {
    let mut store = Mem::default();
    let mut world = World::default();
    let given = suite().given();
    let a = test("tests::a", &[1]);
    let failed = Ran { passed: false, took: Duration::from_millis(9), traced: Some(&Traced::default()) };
    record(&mut store, given, &a, &failed).unwrap();
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Run { why: Why::NoPass, expected: Some(Duration::from_millis(9)) });
    let network = Traced { untraceable: vec![Untraceable::Network], ..Traced::default() };
    pass(&mut store, given, &a, &network, 2);
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Run { why: Why::NoPass, expected: Some(Duration::from_millis(2)) });
    let unknown = Ran { passed: true, took: Duration::from_millis(3), traced: None };
    record(&mut store, given, &a, &unknown).unwrap();
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Run { why: Why::NoPass, expected: Some(Duration::from_millis(3)) });
    // The clock and random numbers are kept: libtest's result of such a test is taken too.
    let clock = Traced { untraceable: vec![Untraceable::Clock, Untraceable::Random], ..Traced::default() };
    assert!(keepable(&clock));
    pass(&mut store, given, &a, &clock, 4);
    assert_eq!(decide(&store, given, &a, &mut world), Decision::Skip { took: Some(Duration::from_millis(4)) });
}

#[test]
fn the_history_s_end_is_its_latest_entry() {
    let mut store = Mem::default();
    let mut world = World::default();
    let given = suite().given();
    for n in 0..37u8 {
        let a = test("tests::a", &[n]);
        pass(&mut store, given, &a, &Traced::default(), u64::from(n));
        assert_eq!(last(&store, given, "tests::a").unwrap().unwrap().0, u32::from(n));
        assert_eq!(decide(&store, given, &test("tests::a", &[n, 200]), &mut world), Decision::Run { why: Why::Changed(code(200)), expected: Some(Duration::from_millis(u64::from(n))) });
    }
    assert!(last(&store, given, "tests::b").unwrap().is_none());
}

#[test]
fn a_history_entry_round_trips() {
    let traced = Traced { env: vec![("FOO".into(), Some("1".into()))], ..Traced::default() };
    let report = trace::encode(&Report::Ran(Some(traced.clone())));
    let entry = Entry {
        passed: true,
        kept: true,
        attrs: Attrs::default().id(),
        reached: Key(ContentId::of("blaze v2 test 1", b"set")),
        took: Duration::from_micros(1234),
        traced: &report,
    };
    let bytes = entry.encode();
    assert_eq!(Entry::decode(&bytes), Ok(entry));
    assert_eq!(entry.traced(), Ok(Some(traced)));
    let bare = Entry { traced: &[], kept: false, ..entry };
    let bytes = bare.encode();
    assert_eq!(Entry::decode(&bytes), Ok(bare));
    assert_eq!(bare.traced(), Ok(None));
    assert!(Entry::decode(&bytes[..bytes.len() - 1]).is_err());
    // What a kept pass read is checked when it is read, to explain a rerun: a report with no
    // record is not one.
    let empty = trace::encode(&Report::Ran(None));
    assert!(Entry { traced: &empty, ..entry }.traced().is_err());
    let set = [code(1), code(2)];
    assert_eq!(decode_reached(&encode_reached(&set)), Ok(set.to_vec()));
    assert!(decode_reached(&encode_reached(&set)[..40]).is_err());
}

/// A record not in normal order keeps what its normal order keeps: the order is fixed before
/// anything is keyed, whether the record came normal (and is borrowed) or not (and is sorted).
#[test]
fn a_pass_is_kept_by_what_it_read_in_normal_order() {
    let mut world = World::with(1, "1");
    let given = suite().given();
    let a = test("tests::a", &[1]);
    let normal = reads(&mut world);
    assert!(trace::normal(&normal));
    let mut shuffled = normal.clone();
    shuffled.files.reverse();
    shuffled.files.push(shuffled.files[0].clone());
    assert!(!trace::normal(&shuffled));
    let (mut one, mut two) = (Mem::default(), Mem::default());
    pass(&mut one, given, &a, &normal, 1);
    pass(&mut two, given, &a, &shuffled, 1);
    assert_eq!(one.values, two.values);
    assert_eq!(decide(&two, given, &a, &mut world), Decision::Skip { took: Some(Duration::from_millis(1)) });
}

#[test]
fn a_binary_run_whole_is_one_test_of_all_of_them() {
    let mut store = Mem::default();
    let mut world = World::default();
    let suite = suite();
    let made = || vec![test("a", &[1, 2]), test("b", &[2, 3])];
    let tests = made();
    let given = suite.given_whole(&tests);
    let whole = Test::whole(&tests);
    assert_eq!(whole.reached, vec![code(1), code(2), code(3)]);
    assert_ne!(given, suite.given());
    assert_eq!(decide(&store, given, &whole, &mut world), Decision::Run { why: Why::New, expected: None });
    pass(&mut store, given, &whole, &Traced::default(), 100);
    assert_eq!(decide(&store, given, &whole, &mut world), Decision::Skip { took: Some(Duration::from_millis(100)) });
    // A test marked ignored makes it another run.
    let mut marked = made();
    marked[1].attrs.ignore = true;
    assert_ne!(suite.given_whole(&marked), given);
    // The order they are named in does not.
    let reversed: Vec<Test> = made().into_iter().rev().collect();
    assert_eq!(suite.given_whole(&reversed), given);
    let mut outside = made();
    outside[0].outside = true;
    assert!(Test::whole(&outside).outside);
}

#[test]
fn filters_and_ignored_tests_as_libtest_has_them() {
    let mut ignored = test("parse::slow", &[1]);
    ignored.attrs.ignore = true;
    let quick = test("parse::quick", &[1]);
    let all = Filter::default();
    assert_eq!(wanted(&quick, &all), Wanted::Yes);
    assert_eq!(wanted(&ignored, &all), Wanted::Ignored);
    let only = Filter { ignored: Ignored::Only, ..Filter::default() };
    assert_eq!((wanted(&quick, &only), wanted(&ignored, &only)), (Wanted::Filtered, Wanted::Yes));
    let include = Filter { ignored: Ignored::Include, ..Filter::default() };
    assert_eq!(wanted(&ignored, &include), Wanted::Yes);
    let named = Filter { names: vec!["quick".into()], ..Filter::default() };
    assert_eq!((wanted(&quick, &named), wanted(&ignored, &named)), (Wanted::Yes, Wanted::Filtered));
    let exact = Filter { names: vec!["quick".into()], exact: true, ..Filter::default() };
    assert_eq!(wanted(&quick, &exact), Wanted::Filtered);
    let skip = Filter { skip: vec!["parse::".into()], ..Filter::default() };
    assert_eq!(wanted(&quick, &skip), Wanted::Filtered);
}

#[test]
fn reasons_read_as_explanations() {
    let names = |hash: &CodeHash| (*hash == code(1)).then(|| "parser::Token::new".to_owned());
    assert_eq!(Why::Changed(code(1)).describe(&names), "reaches parser::Token::new, changed");
    assert!(Why::Changed(code(2)).describe(&names).starts_with("reaches code "));
    assert_eq!(Why::New.to_string(), "new");
    assert_eq!(Why::NoPass.to_string(), "no pass kept");
    assert_eq!(Why::Outside.to_string(), "outside world");
    assert_eq!(Why::Read(Input::Env("FOO".into())).to_string(), "read $FOO, changed");
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Seeded sets of every size around the radix sort's threshold, with repeats and with hashes
/// that share their first four bytes (the runs sorted by the whole hash).
fn seeded_sets() -> Vec<Vec<CodeHash>> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut sets = Vec::new();
    for len in (0..200).chain([1_000, 4_096, 10_000]) {
        let mut set: Vec<CodeHash> = Vec::with_capacity(len);
        for _ in 0..len {
            let mut bytes = [0u8; 32];
            for chunk in bytes.chunks_mut(8) {
                chunk.copy_from_slice(&xorshift(&mut state).to_le_bytes());
            }
            match xorshift(&mut state) % 8 {
                0 if !set.is_empty() => {
                    let at = (xorshift(&mut state) % set.len() as u64) as usize;
                    set.push(set[at]);
                    continue;
                }
                1 if !set.is_empty() => {
                    let at = (xorshift(&mut state) % set.len() as u64) as usize;
                    bytes[..4].copy_from_slice(&set[at].0.0[..4]);
                }
                2 => bytes[..4].fill(0),
                _ => {}
            }
            set.push(CodeHash(ContentId(bytes)));
        }
        sets.push(set);
    }
    sets
}

#[test]
fn the_reached_set_sorts_as_a_comparison_sort_does() {
    for hashes in seeded_sets() {
        let mut expected = hashes.clone();
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(sorted(&hashes), expected, "{} hashes", hashes.len());
    }
}

/// The identity hashes the set in one update; the bytes are those of a hash at a time, so
/// every kept key stays.
#[test]
fn the_identity_is_the_hash_of_each_reached_hash_in_order() {
    for reached in seeded_sets() {
        let test = Test { name: "tests::a".into(), reached, attrs: Attrs::default(), outside: false };
        let mut set = test.reached.clone();
        set.sort_unstable();
        set.dedup();
        let mut hasher = Hasher::new("blaze v2 test identity 1");
        hasher.part(test.name.as_bytes()).id(&test.attrs.id());
        hasher.update(&(set.len() as u64).to_le_bytes());
        for hash in &set {
            hasher.id(&hash.0);
        }
        assert_eq!(test.identity(), hasher.finish(), "{} hashes", test.reached.len());
    }
}
