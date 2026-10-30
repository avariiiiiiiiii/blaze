//! A [`History`] as bytes for the daemon's image: the same history gives the same bytes, and
//! bytes read back predict what the history they came from predicted.
//!
//! Little-endian: [`MAGIC`]; the table's slots, its clock and the records it dropped; the
//! kinds' peaks as [`Estimates::encode`] gives them, after their length; the kinds' times in
//! order of their numbers, [`KIND_BYTES`] each (kind, 4 zero bytes, seen, mean); then the
//! records in slot order, [`RECORD_BYTES`] each (slot, kind, seen, 4 zero bytes, stamp, the
//! key's halves, peak, the four runs).

use std::fmt;

use blaze_graph::admission::{self, Estimates, TaskKind};

use crate::{
    Config, History,
    kinds::{Time, Times},
    table::{self, RUNS, Record, WAYS, Writer},
};

pub const MAGIC: &[u8; 8] = b"blzhis01";
pub const KIND_BYTES: usize = 24;
pub const RECORD_BYTES: usize = 64;
/// The most slots bytes may say a table had: 1 GiB of records. More is not a table this
/// blaze makes.
pub const MOST_SLOTS: u64 = 1 << 24;

/// Why bytes are not a [`History`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not of this version of [`History::encode`].
    Version,
    Short { at: usize },
    /// The kinds' peaks are not [`Estimates`]' bytes.
    Estimates(admission::DecodeError),
    /// Not as many bytes as `count` records take after the kinds.
    Length { count: u64, bytes: usize },
    /// A table of a size no history has.
    Slots(u64),
    /// A kind not after the one before it.
    KindOrder { kind: u32 },
    /// A kind's times no observations make.
    KindUnsound { kind: u32 },
    /// A record's slot not after the one before it, or past the table.
    SlotOrder { slot: u32 },
    /// A record in a slot not of its key's bucket.
    Bucket { slot: u32 },
    /// A record no observations make: unseen, stamped after the clock or with another's
    /// stamp, or a run where no run reached.
    Unsound { slot: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Version => write!(f, "a task history of another version"),
            DecodeError::Short { at } => write!(f, "a task history cut short at byte {at}"),
            DecodeError::Estimates(error) => write!(f, "a task history's memory estimates: {error}"),
            DecodeError::Length { count, bytes } => {
                write!(f, "a task history of {count} records in {bytes} bytes")
            }
            DecodeError::Slots(slots) => write!(f, "a task history of a table of {slots} slots"),
            DecodeError::KindOrder { kind } => write!(f, "a task history with kind {kind} out of order"),
            DecodeError::KindUnsound { kind } => {
                write!(f, "a task history with times of kind {kind} that no runs make")
            }
            DecodeError::SlotOrder { slot } => write!(f, "a task history with slot {slot} out of order"),
            DecodeError::Bucket { slot } => {
                write!(f, "a task history with the record in slot {slot} out of its bucket")
            }
            DecodeError::Unsound { slot } => {
                write!(f, "a task history with a record in slot {slot} that no runs make")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

fn put(out: &mut Vec<u8>, words: &[u64]) {
    for word in words {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

pub(crate) fn encode(history: &History) -> Vec<u8> {
    let estimates = history.estimates.encode();
    let kinds = history.times.sorted();
    let records: Vec<(usize, Record)> = history.keys.records().collect();
    let mut out = Vec::with_capacity(
        MAGIC.len() + 32 + estimates.len() + 8 + kinds.len() * KIND_BYTES + 8 + records.len() * RECORD_BYTES,
    );
    out.extend_from_slice(MAGIC);
    put(&mut out, &[history.keys.slots() as u64, history.keys.clock(), history.keys.evicted()]);
    put(&mut out, &[estimates.len() as u64]);
    out.extend_from_slice(&estimates);
    put(&mut out, &[kinds.len() as u64]);
    for (kind, time) in kinds {
        out.extend_from_slice(&kind.0.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        put(&mut out, &[time.seen, time.mean_ns]);
    }
    put(&mut out, &[records.len() as u64]);
    for (slot, record) in records {
        for half in [slot as u32, record.kind.0, record.seen, 0] {
            out.extend_from_slice(&half.to_le_bytes());
        }
        put(&mut out, &[record.stamp, record.key[0], record.key[1], record.peak]);
        for run in record.runs {
            out.extend_from_slice(&run.to_le_bytes());
        }
    }
    out
}

/// Reads bytes in order, each read bounds-checked.
struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let it = self
            .at
            .checked_add(len)
            .and_then(|end| self.bytes.get(self.at..end))
            .ok_or(DecodeError::Short { at: self.at })?;
        self.at += len;
        Ok(it)
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }
}

pub(crate) fn decode(bytes: &[u8], config: &Config) -> Result<History, DecodeError> {
    let mut read = Bytes { bytes, at: 0 };
    if read.take(MAGIC.len())? != MAGIC {
        return Err(DecodeError::Version);
    }
    let slots = read.u64()?;
    if !slots.is_power_of_two() || slots < WAYS as u64 || slots > MOST_SLOTS {
        return Err(DecodeError::Slots(slots));
    }
    let slots = slots as usize;
    let (clock, evicted) = (read.u64()?, read.u64()?);
    let estimates_len = usize::try_from(read.u64()?).map_err(|_| DecodeError::Short { at: read.at })?;
    let mut estimates = Estimates::decode(read.take(estimates_len)?).map_err(DecodeError::Estimates)?;
    estimates.unseen = config.unseen_bytes;

    let kind_count = read.u64()?;
    if kind_count.checked_mul(KIND_BYTES as u64).is_none_or(|it| it > read.left() as u64) {
        return Err(DecodeError::Short { at: read.at });
    }
    let mut kinds = Vec::with_capacity(kind_count as usize);
    let mut last: Option<u32> = None;
    for _ in 0..kind_count {
        let kind = read.u32()?;
        let pad = read.u32()?;
        let time = Time { seen: read.u64()?, mean_ns: read.u64()? };
        if last.is_some_and(|it| it >= kind) {
            return Err(DecodeError::KindOrder { kind });
        }
        last = Some(kind);
        if pad != 0 || time.seen == 0 {
            return Err(DecodeError::KindUnsound { kind });
        }
        kinds.push((TaskKind(kind), time));
    }

    let count = read.u64()?;
    if count.checked_mul(RECORD_BYTES as u64) != Some(read.left() as u64) {
        return Err(DecodeError::Length { count, bytes: bytes.len() });
    }
    let mut records = Vec::with_capacity(count as usize);
    let mut last: Option<u32> = None;
    for _ in 0..count {
        let slot = read.u32()?;
        let kind = TaskKind(read.u32()?);
        let seen = read.u32()?;
        let pad = read.u32()?;
        let (stamp, key, peak) = (read.u64()?, [read.u64()?, read.u64()?], read.u64()?);
        let mut runs = [0; RUNS];
        for run in &mut runs {
            *run = read.u32()?;
        }
        if last.is_some_and(|it| it >= slot) || slot as usize >= slots {
            return Err(DecodeError::SlotOrder { slot });
        }
        last = Some(slot);
        let record = Record { kind, key, seen, stamp, peak, runs };
        if table::bucket_in(slots, kind, key) != slot as usize - slot as usize % WAYS {
            return Err(DecodeError::Bucket { slot });
        }
        if pad != 0 || !record.is_sound() || stamp > clock {
            return Err(DecodeError::Unsound { slot });
        }
        records.push((slot as usize, record));
    }
    let mut stamps: Vec<(u64, usize)> = records.iter().map(|(slot, it)| (it.stamp, *slot)).collect();
    stamps.sort_unstable();
    if let Some(pair) = stamps.windows(2).find(|it| it[0].0 == it[1].0) {
        return Err(DecodeError::Unsound { slot: pair[1].1 as u32 });
    }

    let mut keys = Writer::new(config.slots);
    keys.set_counts(clock, evicted);
    if config.slots == slots {
        for (slot, record) in &records {
            keys.put_at(*slot, record);
        }
    } else {
        // Oldest first, so where a bucket of the new size is full the newest stay.
        for &(_, slot) in &stamps {
            let record = records.binary_search_by_key(&slot, |it| it.0).map(|at| records[at].1).expect("a record's slot");
            keys.place(&record);
        }
    }
    Ok(History { estimates, times: Times::from_sorted(kinds), keys, unseen_ns: config.unseen_ns })
}
