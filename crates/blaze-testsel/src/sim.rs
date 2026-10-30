//! A deterministic simulation of record, edit and select (docs/v2/plan/lanes/fastcompile/
//! select.md, "Proof"). A program of functions, each version of which calls others (some only
//! under a bit of an input file it reads), reads or not, and fails or not, is edited at random;
//! after each edit every test is chosen for, then run for the truth. A test not run must pass
//! now, and a pass of it must have been kept at the same versions of what it executed and the
//! same input. A run that ends with its record incomplete keeps its pass under what the test
//! reaches.

use std::path::PathBuf;
use std::time::Duration;

use blaze_core::Traced;
use blaze_ids::{ContentId, rng::SplitMix64 as Rng};
use blaze_usercode::test::Attrs;

use crate::{
    record::Record,
    select::{
        Choice, Ran, Subject, choose, keep,
        tests::{Mem, World},
    },
    table::{
        Image, Version,
        tests::{bound, id},
    },
};

const FNS: u32 = 32;
/// Functions `0..SLOTS` are called through slots; the rest are in the unlogged code.
const SLOTS: u32 = 26;
const TESTS: u32 = 12;
const DATA: &str = "/ws/sim";

/// What a function does at a version: whom it calls, each under a bit of the input or always;
/// whether it reads the input; whether it fails.
struct Body {
    calls: Vec<(u32, Option<u8>)>,
    reads: bool,
    fails: bool,
}

fn body(n: u32, version: u32) -> Body {
    let mut rng = Rng::new((u64::from(n) << 32) | u64::from(version));
    rng.next_u64();
    let calls: Vec<(u32, Option<u8>)> = (0..rng.below(4))
        .map(|_| {
            let callee = rng.below(u64::from(FNS)) as u32;
            let bit = (rng.below(3) == 0).then(|| rng.below(8) as u8);
            (callee, bit)
        })
        .collect();
    let reads = rng.below(4) == 0 || calls.iter().any(|it| it.1.is_some());
    Body { calls, reads, fails: rng.below(29) == 0 }
}

#[derive(Clone)]
struct Program {
    versions: Vec<u32>,
    beside: u32,
}

struct Truth {
    /// Every function it executed, sorted.
    executed: Vec<u32>,
    read: bool,
    passed: bool,
}

impl Program {
    fn new() -> Program {
        Program { versions: vec![0; FNS as usize], beside: 0 }
    }

    fn image(&self) -> Image {
        let fns = (0..FNS).map(|n| Version { id: id(n), bound: bound(n, self.versions[n as usize]) }).collect();
        Image::new(fns, (0..SLOTS).collect(), ContentId::of("blaze v2 testsel sim beside 1", &self.beside.to_le_bytes())).unwrap()
    }

    fn run(&self, root: u32, data: u8) -> Truth {
        let mut seen = vec![false; FNS as usize];
        let mut stack = vec![root];
        let (mut read, mut passed) = (false, true);
        while let Some(n) = stack.pop() {
            if std::mem::replace(&mut seen[n as usize], true) {
                continue;
            }
            let body = body(n, self.versions[n as usize]);
            read |= body.reads;
            passed &= !body.fails;
            for (callee, bit) in body.calls {
                if bit.is_none_or(|bit| (data >> bit) & 1 == 1) {
                    stack.push(callee);
                }
            }
        }
        let executed = (0..FNS).filter(|&n| seen[n as usize]).collect();
        Truth { executed, read, passed }
    }

    /// Every function a test could execute at these versions, whatever it reads.
    fn reached(&self, root: u32) -> Vec<u32> {
        let mut seen = vec![false; FNS as usize];
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if !std::mem::replace(&mut seen[n as usize], true) {
                stack.extend(body(n, self.versions[n as usize]).calls.iter().map(|it| it.0));
            }
        }
        (0..FNS).filter(|&n| seen[n as usize]).collect()
    }
}

fn world(data: u8) -> World {
    World { files: [(PathBuf::from(DATA), [data; 32])].into(), env: Vec::new() }
}

fn traced(truth: &Truth, data: u8) -> Traced {
    match truth.read {
        true => Traced { files: vec![(PathBuf::from(DATA), Some([data; 32]))], ..Traced::default() },
        false => Traced::default(),
    }
}

fn record(truth: &Truth, whole: bool) -> Record {
    match whole {
        true => Record::of_hits(SLOTS, truth.executed.iter().copied().filter(|&n| n < SLOTS).collect()),
        false => Record::incomplete("killed"),
    }
}

/// A run, as the simulation remembers it to check a cached result against.
struct Past {
    /// Each function it executed, at its version then.
    executed: Vec<(u32, u32)>,
    unlogged: Vec<u32>,
    beside: u32,
    /// The input, when it read it.
    data: Option<u8>,
    passed: bool,
}

impl Past {
    fn of(program: &Program, truth: &Truth, data: u8) -> Past {
        Past {
            executed: truth.executed.iter().map(|&n| (n, program.versions[n as usize])).collect(),
            unlogged: program.versions[SLOTS as usize..].to_vec(),
            beside: program.beside,
            data: truth.read.then_some(data),
            passed: truth.passed,
        }
    }

    fn stands(&self, program: &Program, data: u8) -> bool {
        self.passed
            && self.beside == program.beside
            && self.unlogged == program.versions[SLOTS as usize..]
            && self.executed.iter().all(|&(n, v)| program.versions[n as usize] == v)
            && self.data.is_none_or(|it| it == data)
    }
}

fn given() -> ContentId {
    ContentId::of("blaze v2 testsel sim given 1", b"sim")
}

#[test]
fn record_edit_select_never_skips_what_would_run_otherwise() {
    let attrs = Attrs::default();
    let names: Vec<String> = (0..TESTS).map(|t| format!("t{t}")).collect();
    let (mut skipped, mut ran) = (0u32, 0u32);
    for seed in 0..24u64 {
        let mut rng = Rng::new(seed);
        let mut program = Program::new();
        let mut data = rng.next_u64() as u8;
        let mut store = Mem::default();
        let mut pasts: Vec<Vec<Past>> = (0..TESTS).map(|_| Vec::new()).collect();
        for _ in 0..60 {
            let image = program.image();
            for t in 0..TESTS {
                let root = t % SLOTS;
                let reached = program.reached(root);
                let subject = Subject { name: &names[t as usize], attrs: &attrs, outside: false, reached: &reached };
                let truth = program.run(root, data);
                let choice = choose(&store, given(), &image, &subject, false, &mut world(data)).unwrap();
                match choice {
                    Choice::Cached(_) => {
                        skipped += 1;
                        assert!(truth.passed, "seed {seed}: test {t} skipped and would fail");
                        assert!(
                            pasts[t as usize].iter().any(|it| it.stands(&program, data)),
                            "seed {seed}: test {t} skipped with no pass at what it executes now"
                        );
                    }
                    Choice::Run { .. } => {
                        ran += 1;
                        let traced = traced(&truth, data);
                        let record = record(&truth, rng.below(6) != 0);
                        let it = Ran { passed: truth.passed, took: Duration::from_millis(1), traced: Some(&traced), record: &record };
                        keep(&mut store, given(), &image, &subject, &it).unwrap();
                        pasts[t as usize].push(Past::of(&program, &truth, data));
                    }
                }
            }
            match rng.below(10) {
                0 => data = rng.next_u64() as u8,
                1 => program.beside += 1,
                // Going back: code that was, a revert.
                2 => {
                    let n = rng.below(u64::from(FNS)) as usize;
                    program.versions[n] = program.versions[n].saturating_sub(1);
                }
                _ => {
                    let n = rng.below(u64::from(FNS)) as usize;
                    program.versions[n] += 1;
                }
            }
        }
    }
    assert!(skipped > ran / 4, "the simulation skips too little to say anything: {skipped} skipped, {ran} ran");
}

/// Over synthetic histories of one whole pass: the test runs again exactly when a function it
/// executed changed, the unlogged code changed, or it read the input and the input changed.
#[test]
fn a_test_runs_exactly_when_what_it_executed_or_read_changed() {
    let attrs = Attrs::default();
    let (mut cached, mut runs) = (0u32, 0u32);
    for seed in 0..600u64 {
        let mut rng = Rng::new(seed ^ 0x5eed);
        let mut program = Program::new();
        for version in &mut program.versions {
            *version = rng.below(3) as u32;
        }
        let data = rng.next_u64() as u8;
        let root = rng.below(u64::from(SLOTS)) as u32;
        let truth = program.run(root, data);
        if !truth.passed {
            continue;
        }
        let image = program.image();
        let reached = program.reached(root);
        let subject = Subject { name: "t", attrs: &attrs, outside: false, reached: &reached };
        let mut store = Mem::default();
        let traced = traced(&truth, data);
        let record = record(&truth, true);
        let ran = Ran { passed: true, took: Duration::from_millis(1), traced: Some(&traced), record: &record };
        keep(&mut store, given(), &image, &subject, &ran).unwrap();

        let before = program.clone();
        for _ in 0..1 + rng.below(3) {
            let n = rng.below(u64::from(FNS)) as usize;
            program.versions[n] += 1;
        }
        let now = if rng.below(4) == 0 { rng.next_u64() as u8 } else { data };
        let changed: Vec<u32> = (0..FNS).filter(|&n| program.versions[n as usize] != before.versions[n as usize]).collect();
        let should_run = changed.iter().any(|n| truth.executed.binary_search(n).is_ok())
            || changed.iter().any(|&n| n >= SLOTS)
            || (truth.read && now != data);
        let choice = choose(&store, given(), &program.image(), &subject, false, &mut world(now)).unwrap();
        let runs_now = matches!(choice, Choice::Run { .. });
        assert_eq!(runs_now, should_run, "seed {seed}: executed {:?}, changed {changed:?}, {choice:?}", truth.executed);
        match runs_now {
            true => runs += 1,
            false => cached += 1,
        }
    }
    assert!(cached > 50 && runs > 50, "{cached} cached, {runs} ran");
}
