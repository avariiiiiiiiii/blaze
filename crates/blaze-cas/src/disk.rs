//! The store's bytes on disk (docs/v2-store.md, "On disk"): packs of records, the index table and
//! the head. Every number is little-endian, every record 16-byte aligned, so a pack is read in
//! place wherever it is mapped.

use blaze_ids::ContentId;

use crate::{Key, Kind, crc};

pub(crate) const ALIGN: u64 = 16;

pub(crate) const fn aligned(n: u64) -> u64 {
    (n + ALIGN - 1) & !(ALIGN - 1)
}

/// Every commit leaves each pack a whole number of blocks long, so every append starts and
/// ends on a block and can be written with `O_DIRECT`. The rest of a commit's last block is
/// zeros, which a record header never starts with.
pub(crate) const BLOCK: u64 = 4096;

pub(crate) const fn blocks(n: u64) -> u64 {
    (n + BLOCK - 1) & !(BLOCK - 1)
}

/// The domain of [`crate::Entry::content`]: what a value's bytes hash to.
pub(crate) const VALUE_DOMAIN: &str = "blaze v2 store value 1";
/// The domain of a chunk's key: a chunk is kept under the hash of its bytes.
pub(crate) const CHUNK_DOMAIN: &str = "blaze v2 store chunk 1";
pub(crate) const DICTIONARY_DOMAIN: &str = "blaze v2 store dictionary 1";

pub(crate) fn content_of(bytes: &[u8]) -> ContentId {
    ContentId::of(VALUE_DOMAIN, bytes)
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
}

pub(crate) fn id_at(bytes: &[u8], at: usize) -> ContentId {
    ContentId(bytes[at..at + 32].try_into().expect("32 bytes"))
}

// Packs.

pub(crate) const PACK_MAGIC: [u8; 8] = *b"BLZPACK\x03";
/// A pack starts with its magic and its kind; records follow.
pub(crate) const PACK_HEADER: u64 = 16;

pub(crate) fn pack_header(kind: Kind) -> [u8; PACK_HEADER as usize] {
    let mut out = [0; PACK_HEADER as usize];
    out[..8].copy_from_slice(&PACK_MAGIC);
    out[8] = kind.code();
    out
}

pub(crate) fn check_pack_header(bytes: &[u8], kind: Kind) -> Result<(), &'static str> {
    if bytes.len() < PACK_HEADER as usize || bytes[..8] != PACK_MAGIC {
        return Err("it does not start as a pack of this store's format does");
    }
    if bytes[8] != kind.code() || bytes[9..16].iter().any(|&it| it != 0) {
        return Err("its header names another kind than its head says");
    }
    Ok(())
}

/// The file of a pack, in its store's directory. Packs sit next to the head, so the directory
/// sync of the head's write makes a new pack's name durable with it.
pub(crate) fn pack_file(kind: Kind, number: u32) -> String {
    format!("pack-{}-{number:06}", kind.name())
}

pub(crate) fn index_file(slot: u8) -> String {
    format!("index-{slot}")
}

pub(crate) fn external_file(content: &ContentId) -> String {
    format!("file-{content:x}")
}

pub(crate) const HEAD_FILE: &str = "head";

/// How a record holds its value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Form {
    /// The value's bytes, as they are: read in place.
    Raw,
    /// zstd with the dictionary of its kind numbered by [`RecordHeader::dictionary`], or, for a
    /// long value ([`crate::cold::LONG_FROM`]), with no dictionary ([`NO_DICTIONARY`]) and
    /// long-distance matching.
    Zstd,
    /// A list of chunks ([`CHUNK_REF`] bytes each), each a value of its own, raw or LZ4.
    Chunked,
    /// The value is a file of its own, [`external_file`] of its content: placed by cloning it.
    External,
    /// A compression dictionary, listed in the head; not a value.
    Dictionary,
    /// LZ4 (a block, no frame): for chunks, which are copied out anyway when their value is
    /// assembled, so decompressing them costs little more than the copy.
    Lz4,
}

impl Form {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Form::Raw => 0,
            Form::Zstd => 1,
            Form::Chunked => 2,
            Form::External => 3,
            Form::Dictionary => 4,
            Form::Lz4 => 5,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Form> {
        match code {
            0 => Some(Form::Raw),
            1 => Some(Form::Zstd),
            2 => Some(Form::Chunked),
            3 => Some(Form::External),
            4 => Some(Form::Dictionary),
            5 => Some(Form::Lz4),
            _ => None,
        }
    }
}

pub(crate) const RECORD_HEADER: u64 = 96;
/// [`RecordHeader::dictionary`] of a value compressed without one.
pub(crate) const NO_DICTIONARY: u32 = u32::MAX;
const RECORD_MAGIC: [u8; 4] = *b"BLZR";
/// Where a record header keeps its CRC-32C, taken over the header with these bytes zero.
const RECORD_CHECK: std::ops::Range<usize> = 12..16;
/// A chunk in a chunked value's payload: its key and its length.
pub(crate) const CHUNK_REF: usize = 40;

/// What a record starts with. Its CRC-32C finds a torn append without reading the payload;
/// the payload is checked against `content` when it is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RecordHeader {
    pub kind: Kind,
    pub form: Form,
    pub dictionary: u32,
    /// Payload bytes after the header, padding not included.
    pub stored: u64,
    /// The value's length.
    pub raw: u64,
    pub key: Key,
    pub content: ContentId,
}

impl RecordHeader {
    pub(crate) fn encode(&self) -> [u8; RECORD_HEADER as usize] {
        let mut out = [0; RECORD_HEADER as usize];
        out[..4].copy_from_slice(&RECORD_MAGIC);
        out[4] = self.kind.code();
        out[5] = self.form.code();
        out[8..12].copy_from_slice(&self.dictionary.to_le_bytes());
        out[16..24].copy_from_slice(&self.stored.to_le_bytes());
        out[24..32].copy_from_slice(&self.raw.to_le_bytes());
        out[32..64].copy_from_slice(&self.key.0.0);
        out[64..96].copy_from_slice(&self.content.0);
        let check = crc::crc32c(&out);
        out[RECORD_CHECK].copy_from_slice(&check.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<RecordHeader, &'static str> {
        if bytes.len() < RECORD_HEADER as usize {
            return Err("its header is cut short");
        }
        let bytes = &bytes[..RECORD_HEADER as usize];
        if bytes[..4] != RECORD_MAGIC {
            return Err("its header has no record's magic");
        }
        let mut unchecked = [0; RECORD_HEADER as usize];
        unchecked.copy_from_slice(bytes);
        unchecked[RECORD_CHECK].fill(0);
        if u32_at(bytes, RECORD_CHECK.start) != crc::crc32c(&unchecked) {
            return Err("its header does not match its check");
        }
        let kind = Kind::from_code(bytes[4]).ok_or("its header names no kind")?;
        let form = Form::from_code(bytes[5]).ok_or("its header names no form")?;
        Ok(RecordHeader {
            kind,
            form,
            dictionary: u32_at(bytes, 8),
            stored: u64_at(bytes, 16),
            raw: u64_at(bytes, 24),
            key: Key(id_at(bytes, 32)),
            content: id_at(bytes, 64),
        })
    }

    /// The bytes the record takes in its pack, padding included.
    pub(crate) fn span(&self) -> u64 {
        aligned(RECORD_HEADER + self.stored)
    }
}

/// A whole record: header, payload, zeros to the next alignment.
pub(crate) fn record(header: &RecordHeader, payload: &[u8]) -> Vec<u8> {
    debug_assert_eq!(header.stored, payload.len() as u64);
    let mut out = Vec::with_capacity(header.span() as usize);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out.resize(header.span() as usize, 0);
    out
}

pub(crate) fn encode_chunks(chunks: &[(Key, u64)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunks.len() * CHUNK_REF);
    for (key, len) in chunks {
        out.extend_from_slice(&key.0.0);
        out.extend_from_slice(&len.to_le_bytes());
    }
    out
}

pub(crate) fn decode_chunks(payload: &[u8]) -> Result<Vec<(Key, u64)>, &'static str> {
    if payload.len() % CHUNK_REF != 0 {
        return Err("its list of chunks is not whole");
    }
    Ok(payload
        .chunks_exact(CHUNK_REF)
        .map(|it| (Key(id_at(it, 0)), u64_at(it, 32)))
        .collect())
}

// Where records are.

/// Where a record is, and enough of it to plan with: one slot of the index table, or an entry
/// of the in-memory delta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Loc {
    /// The pack's place in its store's head.
    pub pack: u32,
    pub kind: Kind,
    pub form: Form,
    pub offset: u64,
    pub stored: u64,
    pub raw: u64,
}

// The head.

const HEAD_MAGIC: [u8; 8] = *b"BLZHEAD\x03";
const HEAD_FIXED: usize = 8 + 4 + 4 + 8 + 8 + 8 + 4 + 4 + 8 + 4 + 4 + 4 + 4;
const PACK_REF: usize = 24;
const DICTIONARY_REF: usize = 16;
const EXTERNAL_REF: usize = 40;
const HEAD_CHECK: usize = 4;

/// The index table a head names: which of the two slots, its length and its CRC-32C.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct IndexRef {
    pub slot: u8,
    pub len: u64,
    pub check: u32,
    pub count: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PackRef {
    pub kind: Kind,
    pub number: u32,
    /// Its bytes that are the store's: anything after is a torn append, cut before the next.
    pub committed: u64,
    /// How far the index table covers it; records from here to `committed` are found by
    /// reading their headers when the store opens.
    pub indexed: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DictionaryRef {
    pub kind: Kind,
    pub pack: u32,
    pub offset: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ExternalRef {
    pub content: ContentId,
    pub len: u64,
}

/// What a store is: written whole and at once (`Effect::Write`) after everything it names is
/// durable, so a crash leaves the old head or the new, each naming only whole bytes.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(crate) struct Head {
    pub generation: u64,
    pub index: Option<IndexRef>,
    pub packs: Vec<PackRef>,
    pub dictionaries: Vec<DictionaryRef>,
    pub externals: Vec<ExternalRef>,
}

impl Head {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            HEAD_FIXED
                + self.packs.len() * PACK_REF
                + self.dictionaries.len() * DICTIONARY_REF
                + self.externals.len() * EXTERNAL_REF
                + HEAD_CHECK,
        );
        out.extend_from_slice(&HEAD_MAGIC);
        out.extend_from_slice(&crate::FORMAT.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        let index = self.index.unwrap_or(IndexRef {
            slot: u8::MAX,
            len: 0,
            check: 0,
            count: 0,
        });
        out.extend_from_slice(&u64::from(index.slot).to_le_bytes());
        out.extend_from_slice(&index.len.to_le_bytes());
        out.extend_from_slice(&index.check.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&index.count.to_le_bytes());
        out.extend_from_slice(&(self.packs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.dictionaries.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.externals.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        debug_assert_eq!(out.len(), HEAD_FIXED);
        for pack in &self.packs {
            out.push(pack.kind.code());
            out.extend_from_slice(&[0; 3]);
            out.extend_from_slice(&pack.number.to_le_bytes());
            out.extend_from_slice(&pack.committed.to_le_bytes());
            out.extend_from_slice(&pack.indexed.to_le_bytes());
        }
        for dictionary in &self.dictionaries {
            out.push(dictionary.kind.code());
            out.extend_from_slice(&[0; 3]);
            out.extend_from_slice(&dictionary.pack.to_le_bytes());
            out.extend_from_slice(&dictionary.offset.to_le_bytes());
        }
        for external in &self.externals {
            out.extend_from_slice(&external.content.0);
            out.extend_from_slice(&external.len.to_le_bytes());
        }
        let check = crc::crc32c(&out);
        out.extend_from_slice(&check.to_le_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Head, String> {
        if bytes.len() < HEAD_FIXED + HEAD_CHECK || bytes[..8] != HEAD_MAGIC {
            return Err("it is not the head of a store of this format".into());
        }
        let format = u32_at(bytes, 8);
        if format != crate::FORMAT {
            return Err(format!(
                "it is of store format {format}, not {}: remove the directory to start a new \
                 store there",
                crate::FORMAT
            ));
        }
        let (body, check) = bytes.split_at(bytes.len() - HEAD_CHECK);
        if crc::crc32c(body).to_le_bytes() != check {
            return Err("it does not match its check".into());
        }
        let generation = u64_at(body, 16);
        let slot = u64_at(body, 24);
        let index = match slot {
            0 | 1 => Some(IndexRef {
                slot: slot as u8,
                len: u64_at(body, 32),
                check: u32_at(body, 40),
                count: u64_at(body, 48),
            }),
            s if s == u64::from(u8::MAX) => None,
            _ => return Err("it names an index slot there is not".into()),
        };
        let packs = u32_at(body, 56) as usize;
        let dictionaries = u32_at(body, 60) as usize;
        let externals = u32_at(body, 64) as usize;
        let want = packs
            .checked_mul(PACK_REF)
            .zip(dictionaries.checked_mul(DICTIONARY_REF))
            .zip(externals.checked_mul(EXTERNAL_REF))
            .and_then(|((a, b), c)| a.checked_add(b)?.checked_add(c)?.checked_add(HEAD_FIXED));
        if want != Some(body.len()) {
            return Err("its length is not what its counts say".into());
        }
        let mut at = HEAD_FIXED;
        let mut head = Head { generation, index, ..Head::default() };
        for _ in 0..packs {
            let kind = Kind::from_code(body[at]).ok_or("a pack of no kind")?;
            head.packs.push(PackRef {
                kind,
                number: u32_at(body, at + 4),
                committed: u64_at(body, at + 8),
                indexed: u64_at(body, at + 16),
            });
            at += PACK_REF;
        }
        for _ in 0..dictionaries {
            let kind = Kind::from_code(body[at]).ok_or("a dictionary of no kind")?;
            let pack = u32_at(body, at + 4);
            if pack as usize >= head.packs.len() {
                return Err("a dictionary in a pack it does not have".into());
            }
            head.dictionaries.push(DictionaryRef { kind, pack, offset: u64_at(body, at + 8) });
            at += DICTIONARY_REF;
        }
        for _ in 0..externals {
            head.externals.push(ExternalRef { content: id_at(body, at), len: u64_at(body, at + 32) });
            at += EXTERNAL_REF;
        }
        for pack in &head.packs {
            if pack.indexed > pack.committed || pack.committed < PACK_HEADER || pack.committed % BLOCK != 0 {
                return Err(format!("pack {} has impossible lengths", pack_file(pack.kind, pack.number)));
            }
        }
        Ok(head)
    }
}
