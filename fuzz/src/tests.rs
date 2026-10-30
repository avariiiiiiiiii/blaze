use crate::{
    Command, Options,
    corpus::{self, Corpus},
    features::{Seen, bucket},
    mutate::{MUTATIONS, Mutation, Mutator, Random, Recent},
    raw::{self, Coverage},
};

/// No comparisons seen, as in a build without trace-cmp.
struct NoRecent;

impl Recent for NoRecent {
    fn compare(&self, _: usize) -> (u64, u64, usize) {
        (0, 0, 0)
    }

    fn bytes(&self, _: usize) -> ([u8; raw::WORD], [u8; raw::WORD], usize) {
        ([0; raw::WORD], [0; raw::WORD], 0)
    }
}

/// One comparison in every slot: `0x11223344` against `0xdeadbeef`, and `MAGIC` against
/// `magic`.
struct OneRecent;

impl Recent for OneRecent {
    fn compare(&self, _: usize) -> (u64, u64, usize) {
        (0x1122_3344, 0xdead_beef, 4)
    }

    fn bytes(&self, _: usize) -> ([u8; raw::WORD], [u8; raw::WORD], usize) {
        let (mut a, mut b) = ([0; raw::WORD], [0; raw::WORD]);
        a[..5].copy_from_slice(b"MAGIC");
        b[..5].copy_from_slice(b"magic");
        (a, b, 5)
    }
}

#[test]
fn buckets_are_afls() {
    let expected = [(1, 0), (2, 1), (3, 2), (4, 3), (7, 3), (8, 4), (15, 4), (16, 5), (31, 5), (32, 6), (127, 6), (128, 7), (255, 7)];
    for (count, want) in expected {
        assert_eq!(bucket(count), want, "count {count}");
    }
}

#[test]
fn seen_counts_new_features_once() {
    let mut seen = Seen::new(1 << 10);
    assert_eq!(seen.merge(&[1, 64, 1000]), 3);
    assert_eq!(seen.merge(&[1, 64, 1000]), 0);
    assert_eq!(seen.merge(&[2, 64]), 1);
    assert_eq!(seen.count(), 4);
}

#[test]
fn drain_buckets_counters_masks_the_engines_and_clears() {
    // Counters past one word, so the tail is read too.
    let mut counters = vec![0u8; 19];
    counters[0] = 1;
    counters[3] = 3;
    counters[8] = 200;
    counters[9] = 5;
    counters[18] = 2;
    let coverage = Coverage::of(&mut counters, &[9]);
    let mut features = Vec::with_capacity(coverage.total() + raw::VALUE_BITS);
    coverage.drain(&mut features, false);
    features.sort_unstable();
    assert_eq!(features, vec![0 << 3, (3 << 3) | 2, (8 << 3) | 7, (18 << 3) | 1]);
    drop(coverage);
    assert!(counters.iter().all(|&c| c == 0), "drain clears every counter: {counters:?}");
}

#[test]
fn run_clears_first_and_reports_a_panic() {
    let mut counters = vec![7u8; 8];
    let coverage = Coverage::of(&mut counters, &[]);
    assert!(coverage.run(|_| {}, b"fine", 0));
    let mut features = Vec::with_capacity(coverage.total() + raw::VALUE_BITS);
    coverage.drain(&mut features, false);
    assert!(features.is_empty(), "run clears what came before it: {features:?}");
    assert!(!coverage.run(|data| assert!(data.is_empty(), "not empty"), b"x", 0));
    assert!(raw::current().is_none(), "no input is published between runs");
}

#[test]
fn a_build_without_coverage_says_which_flags() {
    match Coverage::new(&[]) {
        Err(e) => assert!(e.contains("-Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp"), "{e}"),
        Ok(coverage) => assert!(coverage.total() > 0),
    }
}

#[test]
fn mutations_stay_in_the_limit_and_repeat_with_the_seed() {
    let dictionary: &[&[u8]] = &[b"WORD", b"\xff\xff\xff\xff"];
    let start = b"header 1234 \x00\x01\x02\x03\x04\x05\x06\x07 tail".to_vec();
    let other = b"another input to splice".to_vec();
    let mut outputs = [Vec::new(), Vec::new()];
    for output in &mut outputs {
        let mut mutator = Mutator::new(7, dictionary, OneRecent);
        let mut data = start.clone();
        for _ in 0..5000 {
            mutator.mutate(&mut data, &other, 64);
            assert!(data.len() <= 64, "{} bytes", data.len());
            output.extend_from_slice(&data);
            if data.is_empty() {
                data = start.clone();
            }
        }
    }
    assert_eq!(outputs[0], outputs[1], "the same seed makes the same inputs");
}

#[test]
fn every_mutation_applies_somewhere() {
    let dictionary: &[&[u8]] = &[b"WORD"];
    let mut mutator = Mutator::new(1, dictionary, OneRecent);
    for mutation in MUTATIONS {
        let mut applied = false;
        for _ in 0..100 {
            let mut data = b"some 42 bytes with MAGIC and \x44\x33\x22\x11".to_vec();
            let before = data.clone();
            if mutator.apply(mutation, &mut data, b"other", 128) {
                assert!(data.len() <= 128, "{mutation:?}");
                applied |= data != before || mutation == Mutation::ShuffleBytes;
            }
        }
        assert!(applied, "{mutation:?} never applied");
    }
}

#[test]
fn recent_operands_replace_what_was_compared() {
    let mut mutator = Mutator::new(3, &[], OneRecent);
    let mut replaced = false;
    for _ in 0..200 {
        let mut data = b"xxMAGICxx".to_vec();
        assert!(mutator.apply(Mutation::RecentBytes, &mut data, &[], 64));
        replaced |= data == b"xxmagicxx";
    }
    assert!(replaced, "MAGIC in the input becomes the operand it was compared with");
    let mut found = false;
    for _ in 0..200 {
        let mut data = vec![0, 0x44, 0x33, 0x22, 0x11, 0];
        assert!(mutator.apply(Mutation::RecentCompare, &mut data, &[], 64));
        found |= data == [0, 0xef, 0xbe, 0xad, 0xde, 0];
    }
    assert!(found, "a little-endian operand in the input becomes the other");
}

#[test]
fn nothing_recent_is_no_mutation() {
    let mut mutator = Mutator::new(5, &[], NoRecent);
    let mut data = b"abc".to_vec();
    assert!(!mutator.apply(Mutation::RecentCompare, &mut data, &[], 64));
    assert!(!mutator.apply(Mutation::RecentBytes, &mut data, &[], 64));
    assert!(!mutator.apply(Mutation::Dictionary, &mut data, &[], 64));
    assert_eq!(data, b"abc");
}

#[test]
fn random_stays_below_its_bound() {
    let mut random = Random::new(0);
    for bound in [1usize, 2, 3, 10, 255, 1 << 40] {
        for _ in 0..1000 {
            assert!(random.below(bound) < bound);
        }
    }
    assert_eq!(random.below(0), 0);
}

#[test]
fn hex_round_trips_and_refuses_what_is_not() {
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(corpus::from_hex(&corpus::to_hex(&bytes)).unwrap(), bytes);
    assert_eq!(corpus::from_hex("de ad\nbe\tef").unwrap(), [0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(corpus::from_hex("").unwrap(), Vec::<u8>::new());
    assert!(corpus::from_hex("abc").is_err());
    assert!(corpus::from_hex("zz").is_err());
}

#[test]
fn the_corpus_picks_every_input_and_favors_the_new() {
    let mut corpus = Corpus::default();
    corpus.add(b"a".to_vec(), 1);
    corpus.add(b"b".to_vec(), 1);
    corpus.add(b"c".to_vec(), 10);
    let mut random = Random::new(9);
    let mut counts = [0usize; 3];
    for _ in 0..10_000 {
        counts[(corpus.pick(&mut random)[0] - b'a') as usize] += 1;
    }
    assert!(counts.iter().all(|&n| n > 0), "{counts:?}");
    assert!(counts[2] > counts[0] + counts[1], "{counts:?}");
    assert_eq!((corpus.len(), corpus.bytes(), corpus.last()), (3, 3, &b"c"[..]));
}

#[test]
fn options_parse() {
    let parse = |args: &[&str]| Options::parse(args.iter().map(|it| it.to_string()));
    let fuzz = parse(&["fuzz", "--seconds", "5", "--seed", "3", "--no-value-profile"]).unwrap();
    assert!(matches!(fuzz.command, Command::Fuzz));
    assert_eq!((fuzz.seconds, fuzz.seed, fuzz.value_profile), (Some(5), Some(3), false));
    assert!(matches!(parse(&["run", "a.hex", "b.hex"]).unwrap().command, Command::Run(files) if files.len() == 2));
    assert!(matches!(parse(&["minimize", "a.hex"]).unwrap().command, Command::Minimize(_)));
    assert!(parse(&[]).is_err());
    assert!(parse(&["run"]).is_err());
    assert!(parse(&["fuzz", "a.hex"]).is_err());
    assert!(parse(&["fuzz", "--seconds"]).is_err());
    assert!(parse(&["fuzz", "--frobnicate"]).is_err());
}
