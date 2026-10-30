//! A [`Trace`] as a file (`blaze trace --out`, compared by the `blaze-v2-proof` binary): one
//! deterministic CBOR map of nine entries, a version and eight byte strings, one per column, each
//! element little-endian and of fixed width. Reading one is nine CBOR items and a length check per
//! column, where a derived encoding is an item per byte of every key and fingerprint (35 items a
//! key, 17 an edge).
//!
//! [`View`] reads the columns in place, the file mapped, after one check across cores that every
//! index is in bounds and the keys are sorted; [`Trace`]'s `Deserialize` copies them out of the
//! same view.

use blaze_ids::{ContentId, Fingerprint};
use blaze_query::{QueryKey, QueryKind};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{
    Bits, Trace,
    join::{self, Columns},
};

/// The schema of the file; 1 was the derived encoding, an item per byte.
pub const VERSION: u64 = 2;

/// A column's bytes, written as one CBOR byte string and read back borrowed.
#[derive(Clone, Copy)]
struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(self.0)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Bytes<'a> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Bytes<'a>, D::Error> {
        <&'a [u8]>::deserialize(from).map(Bytes)
    }
}

/// The file: `n` keys and `m` edges.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File<'a> {
    version: u64,
    /// `n` × 2 bytes: each key's kind.
    #[serde(borrow)]
    kinds: Bytes<'a>,
    /// `n` × 32 bytes: each key's input.
    #[serde(borrow)]
    inputs: Bytes<'a>,
    /// ⌈n / 64⌉ × 8 bytes each: [`Trace::has`] and [`Trace::ran`].
    #[serde(borrow)]
    has: Bytes<'a>,
    #[serde(borrow)]
    ran: Bytes<'a>,
    /// `n` × 16 bytes.
    #[serde(borrow)]
    fingerprints: Bytes<'a>,
    /// (`n` + 1) × 4 bytes, or none when there are no keys.
    #[serde(borrow)]
    edges_from: Bytes<'a>,
    /// `m` × 4 and `m` × 16 bytes.
    #[serde(borrow)]
    reads: Bytes<'a>,
    #[serde(borrow)]
    saw: Bytes<'a>,
}

/// A trace read in place: each accessor is one unaligned little-endian load, so the columns are
/// never copied, and a key is built from its 34 bytes only when it is compared or named.
#[derive(Clone, Copy)]
pub struct View<'a> {
    kinds: &'a [[u8; 2]],
    inputs: &'a [[u8; 32]],
    has: &'a [[u8; 8]],
    ran: &'a [[u8; 8]],
    fingerprints: &'a [[u8; 16]],
    edges_from: &'a [[u8; 4]],
    reads: &'a [[u8; 4]],
    saw: &'a [[u8; 16]],
}

fn exact<'a, const N: usize>(bytes: Bytes<'a>, what: &str) -> Result<&'a [[u8; N]], String> {
    let (whole, rest) = bytes.0.as_chunks::<N>();
    if rest.is_empty() {
        Ok(whole)
    } else {
        Err(format!("the trace's `{what}` has {} bytes, not a multiple of {N}", bytes.0.len()))
    }
}

fn bit(words: &[[u8; 8]], i: usize) -> bool {
    (u64::from_le_bytes(words[i / 64]) >> (i % 64)) & 1 == 1
}

impl<'a> View<'a> {
    /// The trace `bytes` encode, checked: every column as long as the keys say, edge ranges in
    /// order and ending at the last edge, every edge reading a key of the trace, the keys strictly
    /// ascending. Nothing past the check can index out of bounds or join out of order.
    pub fn parse(bytes: &'a [u8]) -> Result<View<'a>, String> {
        let file: File<'a> = blaze_protocol::cbor::from_slice(bytes).map_err(|e| format!("the trace: {e}"))?;
        View::of(file)
    }

    fn of(file: File<'a>) -> Result<View<'a>, String> {
        if file.version != VERSION {
            return Err(format!("a trace of version {}; this reads version {VERSION}", file.version));
        }
        let view = View {
            kinds: exact(file.kinds, "kinds")?,
            inputs: exact(file.inputs, "inputs")?,
            has: exact(file.has, "has")?,
            ran: exact(file.ran, "ran")?,
            fingerprints: exact(file.fingerprints, "fingerprints")?,
            edges_from: exact(file.edges_from, "edges_from")?,
            reads: exact(file.reads, "reads")?,
            saw: exact(file.saw, "saw")?,
        };
        view.check()?;
        Ok(view)
    }

    fn from(&self, i: usize) -> usize {
        u32::from_le_bytes(self.edges_from[i]) as usize
    }

    fn check(&self) -> Result<(), String> {
        let (n, m) = (self.kinds.len(), self.reads.len());
        let words = n.div_ceil(64);
        let lengths = [
            ("inputs", self.inputs.len(), n),
            ("has", self.has.len(), words),
            ("ran", self.ran.len(), words),
            ("fingerprints", self.fingerprints.len(), n),
            ("saw", self.saw.len(), m),
        ];
        if let Some((what, got, want)) = lengths.into_iter().find(|&(_, got, want)| got != want) {
            return Err(format!("the trace has {n} keys and {m} edges, and {got} elements of `{what}`, not {want}"));
        }
        if n == 0 && self.edges_from.is_empty() {
            return if m == 0 { Ok(()) } else { Err(format!("the trace has no keys and {m} edges")) };
        }
        if self.edges_from.len() != n + 1 || self.from(0) != 0 || self.from(n) != m {
            return Err(format!("the trace's `edges_from` does not run from 0 to its {m} edges over its {n} keys"));
        }
        let pieces = join::pieces(n);
        let unsorted = join::split(n, pieces, |range| {
            range.into_iter().find(|&i| self.from(i) > self.from(i + 1) || (i > 0 && self.key(i - 1) >= self.key(i)))
        });
        if let Some(i) = unsorted.into_iter().flatten().next() {
            return Err(format!("the trace's key {i} is not above the one before it, or its edges end before they start"));
        }
        let highest = join::split(m, join::pieces(m), |range| {
            range.into_iter().map(|e| u32::from_le_bytes(self.reads[e])).max()
        });
        match highest.into_iter().flatten().max() {
            Some(read) if read as usize >= n => Err(format!("an edge of the trace reads key {read}; it has {n}")),
            _ => Ok(()),
        }
    }

    /// The trace as owned columns: one copy of each.
    pub fn to_trace(&self) -> Trace {
        let words = |column: &[[u8; 8]]| -> Bits { Bits(column.iter().map(|w| u64::from_le_bytes(*w)).collect()) };
        let fingerprints = |column: &[[u8; 16]]| -> Vec<Fingerprint> {
            column.iter().map(|f| Fingerprint(u128::from_le_bytes(*f))).collect()
        };
        let indices = |column: &[[u8; 4]]| -> Vec<u32> { column.iter().map(|w| u32::from_le_bytes(*w)).collect() };
        Trace {
            keys: (0..self.kinds.len()).map(|i| self.key(i)).collect(),
            has: words(self.has),
            ran: words(self.ran),
            fingerprints: fingerprints(self.fingerprints),
            edges_from: indices(self.edges_from),
            reads: indices(self.reads),
            saw: fingerprints(self.saw),
        }
    }
}

impl Columns for View<'_> {
    fn keys(&self) -> usize {
        self.kinds.len()
    }

    fn key(&self, i: usize) -> QueryKey {
        QueryKey { kind: QueryKind(u16::from_le_bytes(self.kinds[i])), input: ContentId(self.inputs[i]) }
    }

    fn has(&self, i: usize) -> bool {
        bit(self.has, i)
    }

    fn ran(&self, i: usize) -> bool {
        bit(self.ran, i)
    }

    fn fingerprint(&self, i: usize) -> Fingerprint {
        Fingerprint(u128::from_le_bytes(self.fingerprints[i]))
    }

    fn edges_of(&self, i: usize) -> std::ops::Range<usize> {
        self.from(i)..self.from(i + 1)
    }

    fn read(&self, e: usize) -> u32 {
        u32::from_le_bytes(self.reads[e])
    }

    fn saw(&self, e: usize) -> Fingerprint {
        Fingerprint(u128::from_le_bytes(self.saw[e]))
    }
}

impl Serialize for Trace {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        let n = self.keys.len();
        let mut kinds = Vec::with_capacity(2 * n);
        let mut inputs = Vec::with_capacity(32 * n);
        for key in &self.keys {
            kinds.extend_from_slice(&key.kind.0.to_le_bytes());
            inputs.extend_from_slice(&key.input.0);
        }
        let words = |bits: &Bits| bits.0.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let wide = |column: &[Fingerprint]| column.iter().flat_map(|f| f.0.to_le_bytes()).collect::<Vec<u8>>();
        let narrow = |column: &[u32]| column.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
        let (has, ran) = (words(&self.has), words(&self.ran));
        let (fingerprints, saw) = (wide(&self.fingerprints), wide(&self.saw));
        let (edges_from, reads) = (narrow(&self.edges_from), narrow(&self.reads));
        File {
            version: VERSION,
            kinds: Bytes(&kinds),
            inputs: Bytes(&inputs),
            has: Bytes(&has),
            ran: Bytes(&ran),
            fingerprints: Bytes(&fingerprints),
            edges_from: Bytes(&edges_from),
            reads: Bytes(&reads),
            saw: Bytes(&saw),
        }
        .serialize(to)
    }
}

impl<'de> Deserialize<'de> for Trace {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Trace, D::Error> {
        let file = File::deserialize(from)?;
        View::of(file).map(|view| view.to_trace()).map_err(de::Error::custom)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use blaze_ids::{ContentId, Fingerprint};
    use blaze_query::{Edge, QueryKey, QueryKind};

    use super::View;
    use crate::{Rng, Trace, TraceBuilder, join::Columns};

    /// A trace of `results` results over `keys` keys, each reading up to 6 of them, by `seed`.
    pub(crate) fn seeded(seed: u64, keys: u64, results: usize) -> Trace {
        let mut rng = Rng::new(seed);
        let key = |n: u64| QueryKey {
            kind: QueryKind(0x2000 + (n % 3) as u16),
            input: ContentId::of("blaze v2 proof wire test key 1", &n.to_le_bytes()),
        };
        let mut builder = TraceBuilder::default();
        for _ in 0..results {
            let edges: Vec<Edge> = (0..rng.below(7))
                .map(|_| Edge { read: key(rng.below(keys)), saw: Fingerprint(u128::from(rng.below(4))) })
                .collect();
            builder.insert(key(rng.below(keys)), Fingerprint(u128::from(rng.below(4))), &edges, rng.below(2) == 0);
        }
        builder.finish()
    }

    #[test]
    fn a_trace_round_trips_through_its_columns() {
        for seed in 0..40 {
            let trace = seeded(seed, 1 + seed * 7, (seed * 13) as usize);
            let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
            blaze_protocol::cbor::check(&bytes).unwrap();
            let back: Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
            assert_eq!(back, trace, "seed {seed}");
            let view = View::parse(&bytes).unwrap();
            assert_eq!(view.to_trace(), trace, "seed {seed}");
        }
    }

    #[test]
    fn the_view_answers_what_the_trace_does() {
        let trace = seeded(5, 300, 900);
        let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
        let view = View::parse(&bytes).unwrap();
        assert_eq!(view.keys(), trace.keys());
        for i in 0..trace.keys() {
            assert_eq!(
                (view.key(i), view.has(i), view.ran(i), view.fingerprint(i), view.edges_of(i)),
                (trace.key(i), trace.has(i), trace.ran(i), trace.fingerprint(i), trace.edges_of(i))
            );
        }
        for e in 0..trace.reads.len() {
            assert_eq!((view.read(e), view.saw(e)), (trace.read(e), trace.saw(e)));
        }
    }

    #[test]
    fn an_empty_trace_round_trips() {
        for trace in [Trace::default(), TraceBuilder::default().finish()] {
            let bytes = blaze_protocol::cbor::to_vec(&trace).unwrap();
            let back: Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
            assert_eq!(back, trace);
        }
    }

    #[test]
    fn a_trace_out_of_bounds_or_order_is_refused() {
        let good = seeded(9, 50, 200);
        let refused = |trace: &Trace, what: &str| {
            let bytes = blaze_protocol::cbor::to_vec(trace).unwrap();
            let said = View::parse(&bytes).err().unwrap_or_else(|| panic!("{what}: parsed"));
            assert!(said.contains(what), "{what}: {said}");
        };
        let mut reads = good.clone();
        reads.reads[0] = reads.keys.len() as u32;
        refused(&reads, "reads key");
        let mut order = good.clone();
        order.keys.swap(0, 1);
        refused(&order, "not above");
        let mut ends = good.clone();
        *ends.edges_from.last_mut().unwrap() += 1;
        refused(&ends, "edges_from");
        let mut short = good.clone();
        short.fingerprints.pop();
        refused(&short, "fingerprints");
        let mut backwards = good;
        let mid = backwards.edges_from.len() / 2;
        backwards.edges_from[mid] = backwards.edges_from[mid + 1] + 1;
        refused(&backwards, "end before");
    }
}
