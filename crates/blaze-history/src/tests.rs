use std::{
    collections::BTreeMap,
    sync::atomic::{
        AtomicBool,
        Ordering::{Acquire, Release},
    },
};

use blaze_graph::{
    Budget, Key, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId,
    admission::{Predict, TaskKind},
    machine::{NodeId, Outcome, Pool, Spec},
};
use blaze_protocol::events::NodeKind;

use crate::{
    Config, DecodeError, History,
    codec::{KIND_BYTES, MAGIC, RECORD_BYTES},
    micros,
    table::{self, RUNS, Record, WAYS, Writer, halves},
};

struct SplitMix(u64);

impl SplitMix {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    fn key(&mut self) -> Key {
        let mut bytes = [0; 32];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        Key(bytes)
    }
}

/// Keys of one bucket of any table: the same first eight bytes, then `n`.
fn same_bucket(n: u64) -> Key {
    let mut bytes = [7; 32];
    bytes[8..16].copy_from_slice(&n.to_le_bytes());
    Key(bytes)
}

fn config(slots: usize) -> Config {
    Config { slots, unseen_ns: 5_000, unseen_bytes: 1 << 20 }
}

const KIND: TaskKind = TaskKind(0x2001);

#[test]
fn a_run_time_is_whole_microseconds_rounded_up() {
    assert_eq!([micros(0), micros(1), micros(1000), micros(1001)], [0, 1, 1, 2]);
    assert_eq!(micros(u64::MAX), u32::MAX);
}

#[test]
fn a_record_predicts_the_median_of_its_last_runs() {
    let mut record = Record::first(KIND, [1, 2], 1, 10, 0);
    assert_eq!(record.ran_us(), 10);
    record.observe(2, 30, 0);
    assert_eq!(record.ran_us(), 20);
    record.observe(3, 1000, 0);
    assert_eq!(record.ran_us(), 30);
    record.observe(4, 20, 0);
    assert_eq!(record.ran_us(), 25);
    // The fifth run replaces the first: 30, 1000, 20, 21.
    record.observe(5, 21, 0);
    assert_eq!(record.runs, [21, 30, 1000, 20]);
    assert_eq!(record.ran_us(), 26);
    assert!(record.is_sound());
}

#[test]
fn a_record_peak_rises_at_once_and_comes_down_by_halves() {
    let mut record = Record::first(KIND, [1, 2], 1, 0, 1000);
    record.observe(2, 0, 200);
    assert_eq!(record.peak, 600);
    record.observe(3, 0, 200);
    assert_eq!(record.peak, 400);
    record.observe(4, 0, 5000);
    assert_eq!(record.peak, 5000);
    record.observe(5, 0, 5000);
    assert_eq!(record.peak, 5000);
}

#[test]
fn a_kind_mean_stays_between_its_runs() {
    let mut random = SplitMix(3);
    let mut history = History::new(config(16));
    let (mut least, mut most) = (u64::MAX, 0);
    for _ in 0..5000 {
        let ran = random.below(1 << 30);
        (least, most) = (least.min(ran), most.max(ran));
        history.observe(KIND, None, ran, 0);
        let mean = history.times().get(KIND).unwrap().mean_ns;
        assert!((least..=most).contains(&mean), "{mean} outside {least}..={most}");
    }
}

#[test]
fn a_key_is_predicted_by_its_record_and_an_unseen_one_by_its_kind() {
    let mut history = History::new(config(16));
    let (seen, unseen) = (same_bucket(1), same_bucket(2));
    let p = history.predict(KIND, Some(&seen));
    assert_eq!((p.cost, p.peak), (Some(5_000), 1 << 20), "a kind never seen");
    history.observe(KIND, Some(&seen), 400_000, 300);
    history.observe(KIND, None, 800_000, 900);
    let p = history.predict(KIND, Some(&seen));
    assert_eq!((p.cost, p.peak), (Some(400_000), 300));
    // The kind's mean moved an eighth of the way to the second run, its peak to the most.
    let p = history.predict(KIND, Some(&unseen));
    assert_eq!((p.cost, p.peak), (Some(450_000), 900));
    assert_eq!(history.predict(KIND, None), p);
    // Another kind with the same key has no record of it.
    let other = history.predict(TaskKind(9), Some(&seen));
    assert_eq!((other.cost, other.peak), (Some(5_000), 1 << 20));
}

#[test]
fn a_full_bucket_drops_its_oldest_key() {
    let mut writer = Writer::new(16);
    for n in 0..WAYS as u64 {
        writer.observe(KIND, &same_bucket(n), 1, 1);
    }
    // Key 0 runs again, so key 1 is the oldest when a fifth comes.
    writer.observe(KIND, &same_bucket(0), 1, 1);
    writer.observe(KIND, &same_bucket(99), 1, 1);
    assert!(writer.get(KIND, &same_bucket(1)).is_none());
    for n in [0, 2, 3, 99] {
        assert!(writer.get(KIND, &same_bucket(n)).is_some(), "key {n}");
    }
    assert_eq!((writer.evicted(), writer.clock()), (1, 6));
    assert_eq!(writer.records().count(), WAYS);
}

/// A key is held exactly while it is one of the [`WAYS`] keys of its bucket observed most
/// lately, and its prediction is that of all its runs since it was last taken in.
#[derive(Default)]
struct Model {
    buckets: BTreeMap<usize, Vec<(TaskKind, [u64; 2], Vec<u32>, u64)>>,
}

impl Model {
    fn observe(&mut self, slots: usize, kind: TaskKind, key: &Key, ran_us: u32, peak: u64) {
        let halves = halves(key);
        let bucket = self.buckets.entry(table::bucket_in(slots, kind, halves)).or_default();
        let (runs, held) = match bucket.iter().position(|it| it.0 == kind && it.1 == halves) {
            Some(at) => {
                let (_, _, mut runs, held) = bucket.remove(at);
                runs.push(ran_us);
                (runs, if peak >= held { peak } else { held - (held - peak) / 2 })
            }
            None => (vec![ran_us], peak),
        };
        bucket.insert(0, (kind, halves, runs, held));
        bucket.truncate(WAYS);
    }

    fn get(&self, slots: usize, kind: TaskKind, key: &Key) -> Option<(u64, u64)> {
        let halves = halves(key);
        let bucket = self.buckets.get(&table::bucket_in(slots, kind, halves))?;
        let (_, _, runs, held) = bucket.iter().find(|it| it.0 == kind && it.1 == halves)?;
        let mut last: Vec<u64> = runs.iter().rev().take(RUNS).map(|&it| u64::from(it)).collect();
        last.sort_unstable();
        let n = last.len();
        let median = if n % 2 == 1 { last[n / 2] } else { (last[n / 2 - 1] + last[n / 2]).div_ceil(2) };
        Some((median, *held))
    }
}

#[test]
fn the_table_holds_what_the_model_says() {
    for seed in 0..100u64 {
        let mut random = SplitMix(seed);
        let slots = [WAYS, 16, 64][random.below(3) as usize];
        let keys: Vec<Key> = (0..1 + random.below(60)).map(|_| random.key()).collect();
        let mut writer = Writer::new(slots);
        let reader = writer.reader();
        let mut model = Model::default();
        for _ in 0..2000 {
            let kind = TaskKind(random.below(3) as u32);
            let key = &keys[random.below(keys.len() as u64) as usize];
            let (ran, peak) = (random.below(1 << 20) as u32, random.below(1 << 40));
            writer.observe(kind, key, ran, peak);
            model.observe(slots, kind, key, ran, peak);
            let kind = TaskKind(random.below(3) as u32);
            let key = &keys[random.below(keys.len() as u64) as usize];
            let got = writer.get(kind, key);
            assert_eq!(got, reader.get(kind, key), "seed {seed}: the reader reads what the writer does");
            assert_eq!(got.map(|it| (it.ran_us(), it.peak)), model.get(slots, kind, key), "seed {seed}");
        }
        assert!(writer.records().all(|(_, it)| it.is_sound()), "seed {seed}");
    }
}

/// Observations of a few kinds and keys, some keyless, some of kinds never seen again.
fn observed(seed: u64, slots: usize) -> History {
    let mut random = SplitMix(seed);
    let keys: Vec<Key> = (0..1 + random.below(300)).map(|_| random.key()).collect();
    let mut history = History::new(config(slots));
    for _ in 0..1 + random.below(3000) {
        let kind = TaskKind(random.below(6) as u32 * 0x1001);
        let key = (random.below(5) != 0).then(|| keys[random.below(keys.len() as u64) as usize]);
        history.observe(kind, key.as_ref(), random.below(1 << 34), random.below(1 << 36));
    }
    history
}

#[test]
fn the_same_observations_give_the_same_predictions_and_bytes() {
    for seed in 0..40u64 {
        let (a, b) = (observed(seed, 64), observed(seed, 64));
        assert_eq!(a.encode(), b.encode(), "seed {seed}");
        let mut random = SplitMix(seed ^ 0xabc);
        for _ in 0..200 {
            let kind = TaskKind(random.below(6) as u32 * 0x1001);
            let key = random.key();
            assert_eq!(a.predict(kind, Some(&key)), b.predict(kind, Some(&key)), "seed {seed}");
        }
    }
}

#[test]
fn a_history_comes_back_from_its_bytes() {
    for seed in 0..40u64 {
        let history = observed(seed, 64);
        let bytes = history.encode();
        let back = History::decode(&bytes, config(64)).unwrap();
        assert_eq!(back.encode(), bytes, "seed {seed}");
        assert_eq!(back.estimates(), history.estimates(), "seed {seed}");
        assert_eq!(back.times(), history.times(), "seed {seed}");
        let records: Vec<_> = history.keys().records().collect();
        assert_eq!(back.keys().records().collect::<Vec<_>>(), records, "seed {seed}");
        for (_, record) in &records {
            let mut key = [0; 32];
            key[..8].copy_from_slice(&record.key[0].to_le_bytes());
            key[8..16].copy_from_slice(&record.key[1].to_le_bytes());
            let key = Key(key);
            assert_eq!(back.predict(record.kind, Some(&key)), history.predict(record.kind, Some(&key)));
        }
    }
}

#[test]
fn a_history_read_into_another_size_keeps_the_newest_of_each_bucket() {
    for seed in 0..40u64 {
        let history = observed(seed, 256);
        let bytes = history.encode();
        let bigger = History::decode(&bytes, config(1024)).unwrap();
        // Every record fits a table four times the size: all of them come back.
        let mut before: Vec<Record> = history.keys().records().map(|it| it.1).collect();
        let mut after: Vec<Record> = bigger.keys().records().map(|it| it.1).collect();
        before.sort_unstable_by_key(|it| it.stamp);
        after.sort_unstable_by_key(|it| it.stamp);
        assert_eq!(before, after, "seed {seed}");
        let smaller = History::decode(&bytes, config(16)).unwrap();
        assert!(smaller.keys().records().count() <= 16);
        // The newest record of each bucket of the smaller table is in it.
        let mut newest: BTreeMap<usize, Record> = BTreeMap::new();
        for record in before {
            newest.insert(table::bucket_in(16, record.kind, record.key), record);
        }
        for record in newest.values() {
            assert!(smaller.keys().records().any(|it| it.1 == *record), "seed {seed}: {record:?}");
        }
        assert_eq!(History::decode(&smaller.encode(), config(16)).unwrap().encode(), smaller.encode());
    }
}

#[test]
fn bad_bytes_are_refused_naming_what_is_wrong() {
    let history = observed(5, 64);
    let bytes = history.encode();
    let decode = |bytes: &[u8]| History::decode(bytes, config(64)).map(|_| ());
    assert_eq!(decode(b"blzhis00"), Err(DecodeError::Version));
    assert_eq!(decode(&bytes[..12]), Err(DecodeError::Short { at: 8 }));
    assert!(matches!(decode(&bytes[..bytes.len() - 1]), Err(DecodeError::Length { .. })));
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(matches!(decode(&longer), Err(DecodeError::Length { .. })));
    let mut slots = bytes.clone();
    slots[8..16].copy_from_slice(&48u64.to_le_bytes());
    assert_eq!(decode(&slots), Err(DecodeError::Slots(48)));

    let estimates = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let kinds_at = 40 + estimates + 8;
    let kinds = u64::from_le_bytes(bytes[kinds_at - 8..kinds_at].try_into().unwrap()) as usize;
    assert!(kinds >= 2);
    let mut order = bytes.clone();
    let first = order[kinds_at..kinds_at + 4].to_vec();
    order[kinds_at + KIND_BYTES..kinds_at + KIND_BYTES + 4].copy_from_slice(&first);
    assert!(matches!(decode(&order), Err(DecodeError::KindOrder { .. })));

    let records_at = kinds_at + kinds * KIND_BYTES + 8;
    assert!(bytes.len() >= records_at + 2 * RECORD_BYTES);
    // The first record's stamp past the clock.
    let mut late = bytes.clone();
    late[records_at + 16..records_at + 24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(matches!(decode(&late), Err(DecodeError::Unsound { .. })));
    // The first record's key moved to another bucket.
    let mut moved = bytes.clone();
    let key = u64::from_le_bytes(moved[records_at + 24..records_at + 32].try_into().unwrap());
    moved[records_at + 24..records_at + 32].copy_from_slice(&(key ^ 4).to_le_bytes());
    assert!(matches!(decode(&moved), Err(DecodeError::Bucket { .. })));
    // The second record in the first's slot.
    let mut twice = bytes.clone();
    let slot = twice[records_at..records_at + 4].to_vec();
    twice[records_at + RECORD_BYTES..records_at + RECORD_BYTES + 4].copy_from_slice(&slot);
    assert!(matches!(decode(&twice), Err(DecodeError::SlotOrder { .. })));
    // The first record's estimates are not the plane's bytes.
    let mut estimates_bad = bytes.clone();
    estimates_bad[40..48].copy_from_slice(b"blzest00");
    assert!(matches!(decode(&estimates_bad), Err(DecodeError::Estimates(_))));
    assert_eq!(&bytes[..8], MAGIC);
}

#[test]
fn readers_never_see_a_torn_record() {
    let mut writer = Writer::new(16);
    let reader = writer.reader();
    let key = Key([9; 32]);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..3 {
            let (reader, done) = (reader.clone(), &done);
            scope.spawn(move || {
                let mut last = 0;
                while !done.load(Acquire) {
                    let Some(record) = reader.get(KIND, &key) else { continue };
                    let seen = u64::from(record.seen);
                    assert_eq!((record.stamp, record.peak), (seen, seen), "{record:?}");
                    assert_eq!(record.runs[(record.seen as usize - 1) % RUNS], record.seen, "{record:?}");
                    assert!(seen >= last, "{seen} after {last}");
                    last = seen;
                }
            });
        }
        for n in 1..=200_000u32 {
            writer.observe(KIND, &key, n, u64::from(n));
        }
        done.store(true, Release);
    });
}

fn spec(id: u32, deps: &[u32], key: Option<Key>, cost: u64, memory: u64) -> Spec {
    Spec {
        id: NodeId(id),
        kind: NodeKind::Codegen,
        name: format!("node {id}"),
        deps: deps.iter().map(|&it| NodeId(it)).collect(),
        pool: Pool::Work,
        cost,
        key,
        memory,
    }
}

/// `spec` with its cost and memory as `history` predicts them, as the daemon's plane adds it.
fn predicted(history: &History, id: u32, deps: &[u32], key: Key) -> Spec {
    let p = history.predict(KIND, Some(&key));
    spec(id, deps, Some(key), p.cost.expect("a history predicts every cost"), p.peak)
}

/// What the plane gives on opening one request of `specs` and closing it.
fn open(plane: &mut Plane, specs: Vec<Spec>) -> Vec<PlaneOutput> {
    let request = RequestId(1);
    let mut out = plane.step(PlaneInput::Open { at_ns: 0, request, weight: 1, keep_going: true }).unwrap();
    out.extend(plane.step(PlaneInput::Add { at_ns: 0, request, nodes: specs }).unwrap());
    out.extend(plane.step(PlaneInput::Close { at_ns: 0, request }).unwrap());
    out
}

fn runs(out: &[PlaneOutput]) -> Vec<NodeId> {
    out.iter().filter_map(|it| if let PlaneOutput::Run { id, .. } = it { Some(*id) } else { None }).collect()
}

/// The order a plane of one core runs `specs` in, each task done before the next starts.
fn order(specs: Vec<Spec>) -> Vec<NodeId> {
    let mut plane = Plane::new(PlaneConfig { io: 1, work: 1, budget: None, starve_after: 8 }).unwrap();
    let mut out = open(&mut plane, specs);
    let mut ran = Vec::new();
    for at in 1.. {
        if out.iter().any(|it| matches!(it, PlaneOutput::Finished { ok: true, .. })) {
            return ran;
        }
        let started = runs(&out);
        assert_eq!(started.len(), 1, "one core runs one task at a time: {started:?}");
        ran.push(started[0]);
        let done = PlaneInput::Done { at_ns: at, id: started[0], outcome: Outcome::Built, added: Vec::new(), group: None, peak: 0 };
        out = plane.step(done).unwrap();
    }
    unreachable!()
}

#[test]
fn the_plane_runs_the_longest_path_of_history_first() {
    let (a, b, c) = (same_bucket(1), same_bucket(2), same_bucket(3));
    let mut history = History::new(config(64));
    // `a` then `b` take a millisecond together; `c` alone takes two.
    history.observe(KIND, Some(&a), 100_000, 0);
    history.observe(KIND, Some(&b), 900_000, 0);
    history.observe(KIND, Some(&c), 2_000_000, 0);
    let specs = vec![predicted(&history, 0, &[], a), predicted(&history, 1, &[0], b), predicted(&history, 2, &[], c)];
    assert_eq!(order(specs), [NodeId(2), NodeId(0), NodeId(1)]);
    // With the costs every node has without a history, the longer chain of nodes goes first.
    let flat = vec![spec(0, &[], Some(a), 1, 0), spec(1, &[0], Some(b), 1, 0), spec(2, &[], Some(c), 1, 0)];
    assert_eq!(order(flat), [NodeId(0), NodeId(1), NodeId(2)]);
}

/// Over random graphs and run times: a plane of one core given the history's costs runs the
/// nodes in the order the critical path of those run times says. Of the ready nodes, the one
/// with the longest path of predicted time from it to the end, its own included, runs first;
/// ties to the node added first.
#[test]
fn the_plane_orders_random_graphs_by_the_history_critical_path() {
    for seed in 0..200u64 {
        let mut random = SplitMix(seed);
        let n = 2 + random.below(30) as usize;
        let keys: Vec<Key> = (0..n).map(|_| random.key()).collect();
        let mut history = History::new(config(256));
        for key in &keys {
            for _ in 0..1 + random.below(5) {
                history.observe(KIND, Some(key), random.below(50) * 10_000, 0);
            }
        }
        let deps: Vec<Vec<u32>> = (0..n)
            .map(|i| (0..i as u32).filter(|_| random.below(4) == 0).collect())
            .collect();
        let specs: Vec<Spec> = (0..n).map(|i| predicted(&history, i as u32, &deps[i], keys[i])).collect();
        let cost: Vec<u64> = specs.iter().map(|it| it.cost).collect();
        let mut path = cost.clone();
        for i in (0..n).rev() {
            let after = (i + 1..n).filter(|&j| deps[j].contains(&(i as u32))).map(|j| path[j]).max();
            path[i] = cost[i] + after.unwrap_or(0);
        }
        let mut done = vec![false; n];
        let mut want = Vec::with_capacity(n);
        while want.len() < n {
            let ready = (0..n).filter(|&i| !done[i] && deps[i].iter().all(|&d| done[d as usize]));
            let next = ready.max_by_key(|&i| (path[i], std::cmp::Reverse(i))).unwrap();
            done[next] = true;
            want.push(NodeId(next as u32));
        }
        assert_eq!(order(specs), want, "seed {seed}");
    }
}

#[test]
fn admission_takes_each_key_peak_not_its_kind() {
    let (a, b, c, d) = (same_bucket(1), same_bucket(2), same_bucket(3), same_bucket(4));
    let mut history = History::new(config(64));
    history.observe(KIND, Some(&a), 1000, 80);
    history.observe(KIND, Some(&b), 1000, 80);
    history.observe(KIND, Some(&c), 1000, 30);
    history.observe(KIND, Some(&d), 1000, 30);
    assert_eq!(history.estimates().predict(KIND), 80);
    let plane = || {
        let budget = Some(Budget { bytes: 100, from: "the test".to_owned() });
        Plane::new(PlaneConfig { io: 1, work: 2, budget, starve_after: 8 }).unwrap()
    };
    // Two of 80 do not fit 100 at once; two of 30 do, which the kind's 80 would not let.
    let heavy = vec![predicted(&history, 0, &[], a), predicted(&history, 1, &[], b)];
    assert_eq!(runs(&open(&mut plane(), heavy)).len(), 1);
    let light = vec![predicted(&history, 0, &[], c), predicted(&history, 1, &[], d)];
    assert_eq!(runs(&open(&mut plane(), light)).len(), 2);
    let by_kind = history.estimates().predict(KIND);
    let light_by_kind = vec![spec(0, &[], Some(c), 1, by_kind), spec(1, &[], Some(d), 1, by_kind)];
    assert_eq!(runs(&open(&mut plane(), light_by_kind)).len(), 1);
}

#[test]
fn a_history_is_the_planes_predictor() {
    let key = same_bucket(1);
    let mut predict: Box<dyn Predict> = Box::new(History::new(config(16)));
    predict.observe(KIND, Some(key), 3_000_000, 4096);
    let p = predict.predict(KIND, Some(key));
    assert_eq!((p.cost, p.peak), (Some(3_000_000), 4096));
    let back = History::decode(&predict.encode(), config(16)).unwrap();
    assert_eq!(back.predict(KIND, Some(&key)), p);
}
