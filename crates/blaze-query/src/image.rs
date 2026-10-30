//! The daemon image: every memo of the database in one file, mapped at start and read in
//! place, so a start is an mmap that pages in only what it touches (docs/v2/plan/lanes/db.md section 7).
//!
//! Layout, little-endian, every section at a 16-byte boundary:
//!
//! - a header as `blaze_format::Header` lays it out: `MAGIC`, the image's [`SchemaHash`], the
//!   file's length, where the root is;
//! - the root: ten `u64`s naming the sections (memos, edges, table, inputs, blob: where and how
//!   many);
//! - memos, [`MEMO_RECORD`] bytes each, sorted by key, a key's memos in a row;
//! - edges, [`EDGE_RECORD`] bytes each, a memo's in the order they were read;
//! - an open-addressed table of `u64`s, a power of two of them: 1 + the index of a key's first
//!   memo, 0 for empty, probed linearly from the key's slot;
//! - inputs, [`INPUT_RECORD`] bytes each: an input key and the path it names;
//! - the blob: values (each at a 16-byte boundary, read in place), keys' bytes, paths.
//!
//! The header, root and section bounds are checked when the image is opened; each record is
//! checked when it is first read, so a start reads nothing it does not use.

use std::sync::{Arc, Mutex};

use blaze_core::Mapping;
use blaze_format::{FormatError, MAGIC, SchemaHash};
use blaze_ids::{ContentId, Fingerprint};

use crate::{
    Input, QueryKey, QueryKind, kinds,
    table::{EDGE_RECORD, Edges, MemoData, Value},
};

pub(crate) const MEMO_RECORD: usize = 192;
pub(crate) const INPUT_RECORD: usize = 64;
const HEADER: usize = 56;
const ROOT_AT: usize = 64;
const ROOT: usize = 80;
const EVICTED: u64 = u64::MAX;

const LAYOUT: &str = "header(magic 8, schema 32, len 8, root 8) pad 64; \
root(memos at, memos, edges at, edges, table at, table len, inputs at, inputs, blob at, blob len: u64); \
memo(kind u16, pad 6, input 32, fingerprint 16, schema 32, producer 32, value id 32, \
edges from u32, edges u32, value at u64, value len u64 or max, key at u64, key len u64) 192; \
edge(kind u16, pad 6, input 32, saw 16) 56; table u64 1+memo; \
input(kind u16, pad 6, input 32, what u8, pad 7, path at u64, path len u64) 64; blob";

pub(crate) fn schema() -> SchemaHash {
    SchemaHash(ContentId::of("blaze v2 db image 1", LAYOUT.as_bytes()))
}

/// What one key contributes to an image.
pub(crate) struct Entry {
    pub(crate) key: QueryKey,
    pub(crate) memos: Vec<Arc<MemoData>>,
    pub(crate) key_bytes: Option<Arc<[u8]>>,
}

fn pad(out: &mut Vec<u8>, to: usize) {
    while out.len() % to != 0 {
        out.push(0);
    }
}

fn slot_of(key: &QueryKey, mask: usize) -> usize {
    let mut low = [0; 8];
    low.copy_from_slice(&key.input.0[..8]);
    let mixed = u64::from_le_bytes(low) ^ (u64::from(key.kind.0)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (mixed as usize) & mask
}

fn put_key(out: &mut Vec<u8>, key: &QueryKey) {
    out.extend_from_slice(&key.kind.0.to_le_bytes());
    out.extend_from_slice(&[0; 6]);
    out.extend_from_slice(&key.input.0);
}

/// The image of `entries` (sorted by key, each key's memos in a fixed order) and `inputs`.
pub(crate) fn write(entries: &[Entry], inputs: &[(QueryKey, Input)]) -> Vec<u8> {
    let memo_count: usize = entries.iter().map(|entry| entry.memos.len()).sum();
    let edge_count: usize =
        entries.iter().flat_map(|entry| entry.memos.iter()).map(|memo| memo.edges.len()).sum();
    let table_len = (entries.len() * 2).next_power_of_two().max(2);

    let memos_at = ROOT_AT + ROOT;
    let edges_at = memos_at + memo_count * MEMO_RECORD;
    let table_at = (edges_at + edge_count * EDGE_RECORD).next_multiple_of(16);
    let inputs_at = table_at + table_len * 8;
    let blob_at = (inputs_at + inputs.len() * INPUT_RECORD).next_multiple_of(16);

    let mut blob = Vec::new();
    let mut memos = Vec::with_capacity(memo_count * MEMO_RECORD);
    let mut edges = Vec::with_capacity(edge_count * EDGE_RECORD);
    let mut table = vec![0u64; table_len];
    let mut first_edge = 0u32;
    let mut index = 0u64;
    for entry in entries {
        let (key_at, key_len) = match &entry.key_bytes {
            Some(bytes) => {
                let at = blob_at + blob.len();
                blob.extend_from_slice(bytes);
                (at as u64, bytes.len() as u64)
            }
            None => (0, 0),
        };
        if !entry.memos.is_empty() {
            let mask = table_len - 1;
            let mut slot = slot_of(&entry.key, mask);
            while table[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            table[slot] = index + 1;
        }
        for memo in &entry.memos {
            put_key(&mut memos, &memo.key);
            memos.extend_from_slice(&memo.fingerprint.0.to_le_bytes());
            memos.extend_from_slice(&memo.schema.0.0);
            memos.extend_from_slice(&memo.producer.0);
            let (value_id, value_at, value_len) = match memo.bytes() {
                Some((pointer, len)) => {
                    // SAFETY: `MemoData::bytes` points at `len` bytes that live as long as the
                    // database, which outlives this call.
                    let bytes = unsafe { std::slice::from_raw_parts(pointer, len) };
                    pad(&mut blob, 16);
                    let at = blob_at + blob.len();
                    blob.extend_from_slice(bytes);
                    (crate::value_id(bytes), at as u64, len as u64)
                }
                None => (ContentId::default(), 0, EVICTED),
            };
            memos.extend_from_slice(&value_id.0);
            memos.extend_from_slice(&first_edge.to_le_bytes());
            memos.extend_from_slice(&(memo.edges.len() as u32).to_le_bytes());
            memos.extend_from_slice(&value_at.to_le_bytes());
            memos.extend_from_slice(&value_len.to_le_bytes());
            memos.extend_from_slice(&key_at.to_le_bytes());
            memos.extend_from_slice(&key_len.to_le_bytes());
            for edge in memo.edges.iter() {
                put_key(&mut edges, &edge.read);
                edges.extend_from_slice(&edge.saw.0.to_le_bytes());
            }
            first_edge += memo.edges.len() as u32;
            index += 1;
        }
    }
    let mut input_records = Vec::with_capacity(inputs.len() * INPUT_RECORD);
    for (key, input) in inputs {
        let (what, path) = match input {
            Input::File(path) => (0u8, path.as_str()),
            Input::Dir(path) => (1, path.as_str()),
            Input::Config => (2, ""),
        };
        let at = blob_at + blob.len();
        blob.extend_from_slice(path.as_bytes());
        put_key(&mut input_records, key);
        input_records.push(what);
        input_records.extend_from_slice(&[0; 7]);
        input_records.extend_from_slice(&(at as u64).to_le_bytes());
        input_records.extend_from_slice(&(path.len() as u64).to_le_bytes());
    }

    let len = blob_at + blob.len();
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&schema().0.0);
    out.extend_from_slice(&(len as u64).to_le_bytes());
    out.extend_from_slice(&(ROOT_AT as u64).to_le_bytes());
    debug_assert_eq!(out.len(), HEADER);
    pad(&mut out, 16);
    for word in [
        memos_at,
        memo_count,
        edges_at,
        edge_count,
        table_at,
        table_len,
        inputs_at,
        inputs.len(),
        blob_at,
        blob.len(),
    ] {
        out.extend_from_slice(&(word as u64).to_le_bytes());
    }
    debug_assert_eq!(out.len(), memos_at);
    out.extend_from_slice(&memos);
    out.extend_from_slice(&edges);
    pad(&mut out, 16);
    debug_assert_eq!(out.len(), table_at);
    for slot in table {
        out.extend_from_slice(&slot.to_le_bytes());
    }
    out.extend_from_slice(&input_records);
    pad(&mut out, 16);
    debug_assert_eq!(out.len(), blob_at);
    out.extend_from_slice(&blob);
    out
}

/// A mapped image, its sections checked.
pub(crate) struct ImageMap {
    mapping: Mapping,
    memos_at: usize,
    memo_count: usize,
    edges_at: usize,
    edge_count: usize,
    table_at: usize,
    table_len: usize,
    inputs_at: usize,
    input_count: usize,
    blob_at: usize,
    blob_len: usize,
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, FormatError> {
    let word = bytes.get(at..at + 8).ok_or(FormatError::OutOfBounds { at: at as u64 })?;
    Ok(u64::from_le_bytes(word.try_into().expect("8 bytes")))
}

fn id_at(bytes: &[u8], at: usize) -> ContentId {
    ContentId(bytes[at..at + 32].try_into().expect("32 bytes"))
}

fn key_at(bytes: &[u8], at: usize) -> QueryKey {
    QueryKey { kind: QueryKind(u16::from_le_bytes([bytes[at], bytes[at + 1]])), input: id_at(bytes, at + 8) }
}

fn size(word: u64, at: usize) -> Result<usize, FormatError> {
    usize::try_from(word).map_err(|_| FormatError::OutOfBounds { at: at as u64 })
}

impl ImageMap {
    pub(crate) fn open(mapping: Mapping) -> Result<ImageMap, FormatError> {
        let bytes = mapping.bytes();
        if bytes.len() < ROOT_AT + ROOT {
            return Err(FormatError::Header("shorter than an image's header and root"));
        }
        if bytes[..8] != MAGIC {
            return Err(FormatError::Header("not an image: wrong magic"));
        }
        let found = SchemaHash(id_at(bytes, 8));
        if found != schema() {
            return Err(FormatError::Schema { found, wanted: schema() });
        }
        if u64_at(bytes, 40)? != bytes.len() as u64 {
            return Err(FormatError::Header("the image's length is not the file's"));
        }
        if u64_at(bytes, 48)? != ROOT_AT as u64 {
            return Err(FormatError::Invalid { at: 48, what: "the root is not where images keep it" });
        }
        let mut words = [0usize; 10];
        for (index, word) in words.iter_mut().enumerate() {
            let at = ROOT_AT + index * 8;
            *word = size(u64_at(bytes, at)?, at)?;
        }
        let [memos_at, memo_count, edges_at, edge_count, table_at, table_len, inputs_at, input_count, blob_at, blob_len] =
            words;
        let within = |at: usize, count: usize, each: usize, field: usize| {
            let end = count.checked_mul(each).and_then(|len| len.checked_add(at));
            match end {
                Some(end) if end <= bytes.len() => Ok(()),
                _ => Err(FormatError::OutOfBounds { at: (ROOT_AT + field * 8) as u64 }),
            }
        };
        within(memos_at, memo_count, MEMO_RECORD, 0)?;
        within(edges_at, edge_count, EDGE_RECORD, 2)?;
        within(table_at, table_len, 8, 4)?;
        within(inputs_at, input_count, INPUT_RECORD, 6)?;
        within(blob_at, blob_len, 1, 8)?;
        if !table_len.is_power_of_two() || table_at % 8 != 0 || blob_at % 16 != 0 {
            return Err(FormatError::Invalid { at: ROOT_AT as u64, what: "a section misaligned" });
        }
        Ok(ImageMap {
            mapping,
            memos_at,
            memo_count,
            edges_at,
            edge_count,
            table_at,
            table_len,
            inputs_at,
            input_count,
            blob_at,
            blob_len,
        })
    }

    fn bytes(&self) -> &[u8] {
        self.mapping.bytes()
    }

    fn table(&self, slot: usize) -> u64 {
        u64_at(self.bytes(), self.table_at + slot * 8).expect("checked at open")
    }

    fn memo_key(&self, index: usize) -> QueryKey {
        key_at(self.bytes(), self.memos_at + index * MEMO_RECORD)
    }

    /// The indices of `key`'s memos.
    pub(crate) fn find(&self, key: &QueryKey) -> Option<std::ops::Range<usize>> {
        let mask = self.table_len - 1;
        let mut slot = slot_of(key, mask);
        for _ in 0..self.table_len {
            let first = self.table(slot);
            if first == 0 {
                return None;
            }
            let first = self.checked_memo(first - 1);
            if self.memo_key(first) == *key {
                let mut end = first + 1;
                while end < self.memo_count && self.memo_key(end) == *key {
                    end += 1;
                }
                return Some(first..end);
            }
            slot = (slot + 1) & mask;
        }
        None
    }

    fn checked_memo(&self, index: u64) -> usize {
        match usize::try_from(index) {
            Ok(index) if index < self.memo_count => index,
            _ => panic!("the daemon image is corrupt: its table names memo {index} of {}", self.memo_count),
        }
    }

    /// Every key with memos in the image.
    pub(crate) fn keys(&self) -> Vec<QueryKey> {
        (0..self.table_len)
            .map(|slot| self.table(slot))
            .filter(|first| *first != 0)
            .map(|first| self.memo_key(self.checked_memo(first - 1)))
            .collect()
    }

    fn blob_range(&self, at: u64, len: u64, what: &str) -> (usize, usize) {
        let ok = usize::try_from(at).ok().zip(usize::try_from(len).ok()).filter(|(at, len)| {
            *at >= self.blob_at && at.checked_add(*len).is_some_and(|end| end <= self.blob_at + self.blob_len)
        });
        ok.unwrap_or_else(|| panic!("the daemon image is corrupt: {what} at {at}+{len} leaves the blob"))
    }

    /// The memo at `index`, read in place: its edges and value stay in the mapping.
    pub(crate) fn memo(&self, index: usize) -> MemoData {
        let bytes = self.bytes();
        let at = self.memos_at + index * MEMO_RECORD;
        let record = &bytes[at..at + MEMO_RECORD];
        let key = key_at(record, 0);
        let fingerprint = Fingerprint(u128::from_le_bytes(record[40..56].try_into().expect("16")));
        let schema = SchemaHash(id_at(record, 56));
        let producer = id_at(record, 88);
        let value_id = id_at(record, 120);
        let first_edge = u32::from_le_bytes(record[152..156].try_into().expect("4")) as usize;
        let edge_len = u32::from_le_bytes(record[156..160].try_into().expect("4")) as usize;
        if first_edge.checked_add(edge_len).is_none_or(|end| end > self.edge_count) {
            panic!("the daemon image is corrupt: memo {index}'s edges leave the edge section");
        }
        let value_at = u64::from_le_bytes(record[160..168].try_into().expect("8"));
        let value_len = u64::from_le_bytes(record[168..176].try_into().expect("8"));
        let value = if value_len == EVICTED {
            Value::Evicted
        } else {
            let (at, len) = self.blob_range(value_at, value_len, "a value");
            if at % 16 != 0 {
                panic!("the daemon image is corrupt: memo {index}'s value is misaligned");
            }
            Value::Mapped { mapping: self.mapping.clone(), at, len, id: value_id }
        };
        MemoData {
            key,
            fingerprint,
            schema,
            producer,
            edges: Edges::Mapped {
                image: self.mapping.clone(),
                at: self.edges_at + first_edge * EDGE_RECORD,
                len: edge_len,
            },
            value: Mutex::new(value),
        }
    }

    /// The bytes of the key of the memo at `index`, if it was kept.
    pub(crate) fn key_bytes(&self, index: usize) -> Option<Arc<[u8]>> {
        let record = &self.bytes()[self.memos_at + index * MEMO_RECORD..][..MEMO_RECORD];
        let at = u64::from_le_bytes(record[176..184].try_into().expect("8"));
        let len = u64::from_le_bytes(record[184..192].try_into().expect("8"));
        if len == 0 {
            return None;
        }
        let (at, len) = self.blob_range(at, len, "a key");
        Some(Arc::from(&self.bytes()[at..at + len]))
    }

    pub(crate) fn inputs(&self) -> Vec<(QueryKey, Input)> {
        let bytes = self.bytes();
        (0..self.input_count)
            .map(|index| {
                let record = &bytes[self.inputs_at + index * INPUT_RECORD..][..INPUT_RECORD];
                let key = key_at(record, 0);
                let at = u64::from_le_bytes(record[48..56].try_into().expect("8"));
                let len = u64::from_le_bytes(record[56..64].try_into().expect("8"));
                let (at, len) = self.blob_range(at, len, "a path");
                let path = std::str::from_utf8(&bytes[at..at + len])
                    .unwrap_or_else(|_| panic!("the daemon image is corrupt: input {index}'s path"))
                    .to_owned();
                let input = match (record[40], key.kind) {
                    (0, kinds::FILE) => Input::File(path),
                    (1, kinds::DIR) => Input::Dir(path),
                    (2, kinds::CONFIG) => Input::Config,
                    (what, kind) => panic!(
                        "the daemon image is corrupt: input {index} is a {what} of kind {}",
                        kind.0
                    ),
                };
                (key, input)
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) fn encode_edge(edge: &crate::Edge) -> Vec<u8> {
    let mut out = Vec::new();
    put_key(&mut out, &edge.read);
    out.extend_from_slice(&edge.saw.0.to_le_bytes());
    out
}
