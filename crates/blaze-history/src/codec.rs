//! A [`History`] as bytes for the daemon's image: the same history gives the same bytes, and
//! bytes read back predict what the history they came from predicted.
//!
//! Little-endian: [`MAGIC`]; the table's slots, its clock and the records it dropped; the
//! kinds' peaks as [`Estimates::encode`] gives them, after their length; the kinds' times in
//! order of their numbers, [`KIND_BYTES`] each (kind, 4 zero bytes, seen, mean); then the
//! records in slot order, [`RECORD_BYTES`] each (slot, kind, seen, 4 zero bytes, stamp, the
//! key's halves, peak, the four runs).

use std::fmt;

use blaze_format::cursor::{self, Put as _, Reader};
use blaze_graph::admission::{self, Estimates, TaskKind};
use blaze_ids::radix;

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

pub(crate) fn encode(history: &History) -> Vec<u8> {
    let estimates = history.estimates.encode();
    let kinds = history.times.sorted();
    let records: Vec<(usize, Record)> = history.keys.records().collect();
    let mut out = Vec::with_capacity(
        MAGIC.len() + 32 + estimates.len() + 8 + kinds.len() * KIND_BYTES + 8 + records.len() * RECORD_BYTES,
    );
    out.put_bytes(MAGIC);
    out.put_u64(history.keys.slots() as u64);
    out.put_u64(history.keys.clock());
    out.put_u64(history.keys.evicted());
    out.put_bytes_u64(&estimates);
    out.put_u64(kinds.len() as u64);
    for (kind, time) in kinds {
        out.put_u32(kind.0);
        out.put_u32(0);
        out.put_u64(time.seen);
        out.put_u64(time.mean_ns);
    }
    out.put_u64(records.len() as u64);
    for (slot, record) in records {
        for half in [slot as u32, record.kind.0, record.seen, 0] {
            out.put_u32(half);
        }
        for word in [record.stamp, record.key[0], record.key[1], record.peak] {
            out.put_u64(word);
        }
        for run in record.runs {
            out.put_u32(run);
        }
    }
    out
}

impl From<cursor::Error> for DecodeError {
    fn from(error: cursor::Error) -> DecodeError {
        DecodeError::Short { at: error.at }
    }
}

pub(crate) fn decode(bytes: &[u8], config: &Config) -> Result<History, DecodeError> {
    let mut read = Reader::new(bytes);
    if read.array::<8>()? != MAGIC {
        return Err(DecodeError::Version);
    }
    let slots = read.u64()?;
    if !slots.is_power_of_two() || slots < WAYS as u64 || slots > MOST_SLOTS {
        return Err(DecodeError::Slots(slots));
    }
    let slots = slots as usize;
    let (clock, evicted) = (read.u64()?, read.u64()?);
    let mut estimates = Estimates::decode(read.bytes_u64()?).map_err(DecodeError::Estimates)?;
    estimates.unseen = config.unseen_bytes;

    let kind_count = read.count_u64(KIND_BYTES)?;
    let mut kinds = Vec::with_capacity(kind_count);
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
    radix::sort_copy_by_key(&mut stamps, |&(stamp, slot)| radix::pack_64(stamp, slot as u64));
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
