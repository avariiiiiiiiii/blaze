//! What a task is expected to hold, for the plane's memory admission (docs/v2/plan/lanes/core.md section 4).
//!
//! Every task runs in a bump arena of its own, so what it held at most is measured, not
//! guessed. [`Estimates`] keeps, for each kind of task (a query kind), what tasks of it held,
//! and predicts the next one's peak from it: the [`crate::machine::Spec::memory`] a node is
//! added with. A task over its prediction is let finish and the estimate grows; nothing is
//! cut short or retried. The estimates are kept across runs, in the daemon's image
//! ([`Estimates::encode`]).
//!
//! The rule, integers only, the same observations giving the same estimates: a peak at or over
//! the prediction becomes the prediction at once. A peak under it counts toward a window of
//! [`WINDOW`] of them in a row; once the window is full the prediction comes down half the way
//! (rounded up) to the most of the window's peaks, and a new window begins. So the prediction is a high
//! quantile of what the kind held lately (the most of the last [`WINDOW`] peaks, approached by
//! halves), never under the last peak seen, never over the most ever seen.

#![forbid(unsafe_code)]

use std::fmt;

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

/// How many peaks under a prediction, in a row, bring it down a step.
pub const WINDOW: u32 = 64;

/// A kind of task whose tasks are alike in what they hold: a query kind's number
/// (`blaze_query::QueryKind`), or one of the plane's own (a link, a test run).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct TaskKind(pub u32);

/// What tasks of one kind held: 40 bytes.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Estimate {
    /// How many were measured.
    pub seen: u64,
    /// The most any of them held, in bytes.
    pub most: u64,
    /// A high quantile of what they held, in bytes: what a new task is predicted to hold.
    pub predicted: u64,
    /// How many peaks under `predicted` came in a row since it last moved: the window.
    #[serde(default)]
    pub below: u32,
    /// The most of the window's peaks.
    #[serde(default)]
    pub below_most: u64,
}

const _: () = assert!(size_of::<Estimate>() == 40);

impl Estimate {
    fn observe(&mut self, peak: u64) {
        self.seen = self.seen.saturating_add(1);
        self.most = self.most.max(peak);
        if peak >= self.predicted {
            self.predicted = peak;
            self.below = 0;
            self.below_most = 0;
            return;
        }
        self.below += 1;
        self.below_most = self.below_most.max(peak);
        if self.below >= WINDOW {
            self.predicted -= (self.predicted - self.below_most).div_ceil(2);
            self.below = 0;
            self.below_most = 0;
        }
    }

    /// Whether it is one [`Estimate::observe`] can make.
    fn is_sound(&self) -> bool {
        self.seen > 0
            && self.predicted <= self.most
            && self.below < WINDOW
            && ((self.below == 0 && self.below_most == 0)
                || (u64::from(self.below) <= self.seen && self.below_most < self.predicted))
    }
}

/// Each kind's [`Estimate`], learned as tasks finish. A predict or an observe is one Fx hash of
/// the kind's `u32` and one probe; an observe of a kind seen before allocates nothing. Kinds
/// are written in order of their numbers, by [`Estimates::encode`] and by serde alike.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Estimates {
    #[serde(with = "in_order")]
    pub by_kind: FxHashMap<TaskKind, Estimate>,
    /// What a task of a kind never measured is predicted to hold.
    pub unseen: u64,
}

/// [`Estimates::by_kind`] for serde, its kinds in order, so the same estimates give the same
/// bytes whatever order their kinds were first seen in.
mod in_order {
    use rustc_hash::FxHashMap;
    use serde::{Deserialize, Deserializer, Serializer};

    use super::{Estimate, TaskKind};

    pub(super) fn serialize<S: Serializer>(
        by_kind: &FxHashMap<TaskKind, Estimate>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(super::sorted(by_kind))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<FxHashMap<TaskKind, Estimate>, D::Error> {
        FxHashMap::deserialize(deserializer)
    }
}

/// The kinds of `by_kind` and their estimates, in order of the kinds' numbers.
fn sorted(by_kind: &FxHashMap<TaskKind, Estimate>) -> Vec<(TaskKind, Estimate)> {
    let mut kinds: Vec<(TaskKind, Estimate)> = by_kind.iter().map(|(&k, &e)| (k, e)).collect();
    kinds.sort_unstable_by_key(|it| it.0);
    kinds
}

/// The first bytes of [`Estimates::encode`]'s bytes, its version with them.
const MAGIC: &[u8; 8] = b"blzest01";
/// The bytes of one kind in [`Estimates::encode`]'s bytes.
const KIND_BYTES: usize = 4 + 4 + 8 * 4;

impl Estimates {
    /// No kind measured yet, each predicted to hold `unseen`.
    pub fn new(unseen: u64) -> Estimates {
        Estimates { by_kind: FxHashMap::default(), unseen }
    }

    /// The peak a task of `kind` is predicted to hold, in bytes.
    pub fn predict(&self, kind: TaskKind) -> u64 {
        match self.by_kind.get(&kind) {
            Some(estimate) if estimate.seen > 0 => estimate.predicted,
            _ => self.unseen,
        }
    }

    /// A task of `kind` finished having held `peak` bytes at most.
    pub fn observe(&mut self, kind: TaskKind, peak: u64) {
        self.by_kind.entry(kind).or_default().observe(peak);
    }

    /// The estimates as bytes for the daemon's image: the same estimates give the same bytes.
    /// Integers are little-endian: `MAGIC`, `unseen`, the count of kinds, then each kind in
    /// order: its number, `below`, `seen`, `most`, `predicted`, `below_most`.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(MAGIC.len() + 16 + self.by_kind.len() * KIND_BYTES);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&self.unseen.to_le_bytes());
        bytes.extend_from_slice(&(self.by_kind.len() as u64).to_le_bytes());
        for (kind, estimate) in sorted(&self.by_kind) {
            bytes.extend_from_slice(&kind.0.to_le_bytes());
            bytes.extend_from_slice(&estimate.below.to_le_bytes());
            for it in [estimate.seen, estimate.most, estimate.predicted, estimate.below_most] {
                bytes.extend_from_slice(&it.to_le_bytes());
            }
        }
        bytes
    }

    /// The estimates [`Estimates::encode`] gave `bytes`, or what is wrong with them: bytes of
    /// another version, cut short or with more after, kinds out of order, or an estimate no
    /// run of observations makes.
    pub fn decode(bytes: &[u8]) -> Result<Estimates, DecodeError> {
        let mut at = 0;
        let mut take = |len: usize| -> Result<&[u8], DecodeError> {
            let end = at + len;
            let it = bytes.get(at..end).ok_or(DecodeError::Short { at })?;
            at = end;
            Ok(it)
        };
        if take(MAGIC.len())? != MAGIC {
            return Err(DecodeError::Version);
        }
        let mut word = || -> Result<u64, DecodeError> {
            let it = take(8)?;
            Ok(u64::from_le_bytes(it.try_into().expect("8 bytes")))
        };
        let unseen = word()?;
        let count = word()?;
        let rest = bytes.len() - MAGIC.len() - 16;
        if count.checked_mul(KIND_BYTES as u64) != Some(rest as u64) {
            return Err(DecodeError::Length { count, bytes: bytes.len() });
        }
        let mut by_kind = FxHashMap::default();
        by_kind.reserve(count as usize);
        let mut last: Option<u32> = None;
        for chunk in bytes[MAGIC.len() + 16..].chunks_exact(KIND_BYTES) {
            let u32_at = |at: usize| u32::from_le_bytes(chunk[at..at + 4].try_into().expect("4"));
            let u64_at = |at: usize| u64::from_le_bytes(chunk[at..at + 8].try_into().expect("8"));
            let kind = u32_at(0);
            if last.is_some_and(|it| it >= kind) {
                return Err(DecodeError::Order { kind });
            }
            last = Some(kind);
            let estimate = Estimate {
                below: u32_at(4),
                seen: u64_at(8),
                most: u64_at(16),
                predicted: u64_at(24),
                below_most: u64_at(32),
            };
            if !estimate.is_sound() {
                return Err(DecodeError::Unsound { kind });
            }
            by_kind.insert(TaskKind(kind), estimate);
        }
        Ok(Estimates { by_kind, unseen })
    }
}

/// Why bytes are not [`Estimates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not of this version of [`Estimates::encode`].
    Version,
    Short { at: usize },
    /// Not as many bytes as `count` kinds take.
    Length { count: u64, bytes: usize },
    /// A kind not after the one before it.
    Order { kind: u32 },
    /// An estimate no observations make.
    Unsound { kind: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Version => write!(f, "memory estimates of another version"),
            DecodeError::Short { at } => write!(f, "memory estimates cut short at byte {at}"),
            DecodeError::Length { count, bytes } => {
                write!(f, "memory estimates of {count} kinds in {bytes} bytes")
            }
            DecodeError::Order { kind } => {
                write!(f, "memory estimates with kind {kind} out of order")
            }
            DecodeError::Unsound { kind } => {
                write!(f, "memory estimate of kind {kind} that no observations make")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// A task's arena: where everything the task allocates while it runs goes, so its peak is
/// measured and its results, once written into the zero-copy format, outlive it.
pub trait Arena {
    /// The most bytes it held at once.
    fn peak(&self) -> u64;
    /// The bytes it holds now.
    fn held(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

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
    }

    /// A peak of one of a few sizes, now and then far bigger.
    fn peak(random: &mut SplitMix) -> u64 {
        match random.below(20) {
            0 => random.below(1 << 34),
            1..=3 => 0,
            _ => (1 + random.below(8)) << (10 + random.below(16)),
        }
    }

    #[test]
    fn a_kind_never_measured_is_unseen() {
        let mut estimates = Estimates::new(7 << 20);
        assert_eq!(estimates.predict(TaskKind(3)), 7 << 20);
        estimates.observe(TaskKind(3), 100);
        assert_eq!(estimates.predict(TaskKind(3)), 100);
        assert_eq!(estimates.predict(TaskKind(4)), 7 << 20);
    }

    #[test]
    fn a_peak_over_the_prediction_is_the_prediction_at_once() {
        let mut estimates = Estimates::new(0);
        let kind = TaskKind(1);
        estimates.observe(kind, 1000);
        estimates.observe(kind, 10);
        assert_eq!(estimates.predict(kind), 1000);
        estimates.observe(kind, 5000);
        assert_eq!(estimates.predict(kind), 5000);
        let estimate = estimates.by_kind[&kind];
        assert_eq!((estimate.seen, estimate.most, estimate.below), (3, 5000, 0));
    }

    #[test]
    fn smaller_peaks_bring_it_down_by_halves_a_window_at_a_time() {
        let mut estimates = Estimates::new(0);
        let kind = TaskKind(1);
        estimates.observe(kind, 1 << 20);
        for _ in 0..WINDOW - 1 {
            estimates.observe(kind, 1024);
            assert_eq!(estimates.predict(kind), 1 << 20);
        }
        estimates.observe(kind, 1024);
        assert_eq!(estimates.predict(kind), 1024 + ((1 << 20) - 1024) / 2);
        let mut windows = 1;
        while estimates.predict(kind) > 1024 {
            for _ in 0..WINDOW {
                estimates.observe(kind, 1024);
            }
            windows += 1;
        }
        assert_eq!(estimates.predict(kind), 1024);
        assert!(windows <= 21, "{windows} windows");
        assert_eq!(estimates.by_kind[&kind].most, 1 << 20);
    }

    /// Over random runs of peaks of many kinds, after every observation: the prediction is not
    /// under the peak just seen nor over the most seen, it comes down only at the end of a full
    /// window of smaller peaks and never under the most of them, and it went up only to a peak.
    #[test]
    fn random_peaks_keep_the_rule() {
        for seed in 0..200u64 {
            let mut random = SplitMix(seed);
            let mut estimates = Estimates::new(random.below(1 << 30));
            let mut smaller_in_a_row = Windows::new();
            for _ in 0..2000 {
                let kind = TaskKind(random.below(4) as u32);
                let peak = peak(&mut random);
                let before = estimates.by_kind.get(&kind).copied();
                estimates.observe(kind, peak);
                let after = estimates.by_kind[&kind];
                let predicted = estimates.predict(kind);
                assert_eq!(predicted, after.predicted);
                assert!(predicted >= peak, "seed {seed}: {predicted} under the last peak {peak}");
                assert!(predicted <= after.most, "seed {seed}: {predicted} over {}", after.most);
                assert!(after.is_sound(), "seed {seed}: {after:?}");
                let window = smaller_in_a_row.entry(kind).or_default();
                match before {
                    Some(before) if peak < before.predicted => {
                        window.0 += 1;
                        window.1 = window.1.max(peak);
                        if predicted < before.predicted {
                            assert_eq!(window.0, WINDOW, "seed {seed}: down after {}", window.0);
                            assert!(predicted >= window.1, "seed {seed}");
                            *window = (0, 0);
                        } else {
                            assert_eq!(predicted, before.predicted, "seed {seed}");
                        }
                    }
                    _ => {
                        assert_eq!(predicted, peak, "seed {seed}");
                        *window = (0, 0);
                    }
                }
            }
        }
    }

    /// Per kind: the smaller peaks in a row since its prediction moved, and the most of them.
    type Windows = BTreeMap<TaskKind, (u32, u64)>;

    #[test]
    fn the_same_observations_give_the_same_estimates_and_bytes() {
        for seed in 0..50u64 {
            let run = || {
                let mut random = SplitMix(seed);
                let mut estimates = Estimates::new(1 << 20);
                for _ in 0..1000 {
                    let kind = TaskKind(random.below(6) as u32);
                    estimates.observe(kind, peak(&mut random));
                }
                estimates
            };
            let (a, b) = (run(), run());
            assert_eq!(a, b, "seed {seed}");
            assert_eq!(a.encode(), b.encode(), "seed {seed}");
        }
    }

    /// The bytes [`Estimates::encode`] gave while the kinds were a `BTreeMap`: kinds in order.
    fn encoded(model: &BTreeMap<TaskKind, Estimate>, unseen: u64) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&unseen.to_le_bytes());
        bytes.extend_from_slice(&(model.len() as u64).to_le_bytes());
        for (kind, estimate) in model {
            bytes.extend_from_slice(&kind.0.to_le_bytes());
            bytes.extend_from_slice(&estimate.below.to_le_bytes());
            for it in [estimate.seen, estimate.most, estimate.predicted, estimate.below_most] {
                bytes.extend_from_slice(&it.to_le_bytes());
            }
        }
        bytes
    }

    /// The hashed table predicts what the ordered map it replaced predicted, after every
    /// observation of kinds far apart (query kinds, the daemon's own, `u32::MAX`), and its
    /// bytes are the map's.
    #[test]
    fn the_hashed_table_says_what_the_ordered_map_said() {
        let kinds = [0, 1, 7, 300, 0x6000, 0x1_0001, 0x1_0004, u32::MAX];
        for seed in 0..100u64 {
            let mut random = SplitMix(seed);
            let unseen = random.below(1 << 24);
            let mut estimates = Estimates::new(unseen);
            let mut model: BTreeMap<TaskKind, Estimate> = BTreeMap::new();
            for _ in 0..1500 {
                let kind = TaskKind(kinds[random.below(kinds.len() as u64) as usize]);
                let peak = peak(&mut random);
                estimates.observe(kind, peak);
                model.entry(kind).or_default().observe(peak);
                let asked = TaskKind(kinds[random.below(kinds.len() as u64) as usize]);
                let want = model.get(&asked).filter(|it| it.seen > 0).map_or(unseen, |it| it.predicted);
                assert_eq!(estimates.predict(asked), want, "seed {seed}");
            }
            assert_eq!(estimates.by_kind.len(), model.len(), "seed {seed}");
            assert_eq!(estimates.encode(), encoded(&model, unseen), "seed {seed}");
        }
    }

    /// The same estimates, their kinds first seen in another order, give the same bytes, by
    /// [`Estimates::encode`] and by serde, and come back from both.
    #[test]
    fn the_order_kinds_came_in_changes_no_byte() {
        for seed in 0..50u64 {
            let mut random = SplitMix(seed);
            let runs: Vec<(TaskKind, Vec<u64>)> = (0..1 + random.below(40))
                .map(|kind| {
                    let kind = TaskKind((kind as u32).wrapping_mul(0x9e37_79b9));
                    (kind, (0..1 + random.below(200)).map(|_| peak(&mut random)).collect())
                })
                .collect();
            let mut forward = Estimates::new(5);
            for (kind, peaks) in &runs {
                for &peak in peaks {
                    forward.observe(*kind, peak);
                }
            }
            let mut backward = Estimates::new(5);
            for (kind, peaks) in runs.iter().rev() {
                for &peak in peaks {
                    backward.observe(*kind, peak);
                }
            }
            assert_eq!(forward, backward, "seed {seed}");
            assert_eq!(forward.encode(), backward.encode(), "seed {seed}");
            let json = serde_json::to_vec(&forward).unwrap();
            assert_eq!(json, serde_json::to_vec(&backward).unwrap(), "seed {seed}");
            assert_eq!(serde_json::from_slice::<Estimates>(&json).unwrap(), forward, "seed {seed}");
            assert_eq!(Estimates::decode(&forward.encode()), Ok(forward), "seed {seed}");
        }
    }

    #[test]
    fn estimates_come_back_from_their_bytes_and_bad_bytes_are_refused() {
        let mut random = SplitMix(9);
        let mut estimates = Estimates::new(123);
        for _ in 0..500 {
            estimates.observe(TaskKind(random.below(5) as u32), peak(&mut random));
        }
        let bytes = estimates.encode();
        assert_eq!(Estimates::decode(&bytes), Ok(estimates.clone()));
        assert_eq!(Estimates::decode(&Estimates::new(5).encode()), Ok(Estimates::new(5)));
        assert_eq!(Estimates::decode(b"blzest00"), Err(DecodeError::Version));
        assert_eq!(Estimates::decode(&bytes[..12]), Err(DecodeError::Short { at: 8 }));
        assert!(matches!(
            Estimates::decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Length { .. })
        ));
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(matches!(Estimates::decode(&longer), Err(DecodeError::Length { .. })));
        // The predicted bytes of the first kind made more than the most.
        let mut unsound = bytes.clone();
        let first = MAGIC.len() + 16;
        unsound[first + 24..first + 32].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(Estimates::decode(&unsound), Err(DecodeError::Unsound { .. })));
        // The second kind's number made the first's.
        let mut order = bytes;
        let kind = order[first..first + 4].to_vec();
        order[first + KIND_BYTES..first + KIND_BYTES + 4].copy_from_slice(&kind);
        assert!(matches!(Estimates::decode(&order), Err(DecodeError::Order { .. })));
    }
}
