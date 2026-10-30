//! Compression by tier (docs/v2/plan/data.md, "Compression follows the columns"). A column is
//! `u32`s; the store picks its tier by how recently it was read, and every tier gives back
//! exactly the values it was given.
//!
//! - **hot** ([`HotColumn`]): plain fixed-width values at the narrowest width their largest
//!   needs (`u8`, `u16` or `u32`, recorded in the column), mapped and read in place.
//! - **warm** ([`WarmColumn`], [`Packed`]): each value its offset from the column's minimum in
//!   as many bits as the range needs, packed into `u64`s: element `i` is a shift and a mask,
//!   read in place; [`WarmRef::unpack`] decodes a run of them with a loop specialized for the
//!   width, which the compiler vectorizes. The same bits on every machine.
//! - **cold** ([`encode_cold`]): a codec per column, the smallest of plain packing, runs (for
//!   tag columns), deltas (for sorted columns: CSR offsets, sorted relocations, keys) and a
//!   dictionary (for few distinct values), then zstd, with a dictionary per kind of column
//!   ([`ColdDictionary`]); decoded when read in.

use std::mem::offset_of;

use blaze_ids::ContentId;

use crate::{FormatError, Slice, Writer, record};

/// How wide a hot column's values are.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Width {
    U8,
    U16,
    U32,
}

impl Width {
    /// The narrowest width that holds every one of `values`.
    pub fn of(values: &[u32]) -> Width {
        match values.iter().copied().max().unwrap_or(0) {
            0..=0xff => Width::U8,
            0x100..=0xffff => Width::U16,
            _ => Width::U32,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            Width::U8 => 1,
            Width::U16 => 2,
            Width::U32 => 4,
        }
    }
}

record! {
    /// A hot column in a buffer: one of its slices holds the values, at the width
    /// [`Width::of`] gave them; the others are empty.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct HotColumn: 1 {
        u8s: Slice<u8>,
        u16s: Slice<u16>,
        u32s: Slice<u32>,
    }
}

/// A hot column's values written to a buffer, to be named by the record that holds the column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlacedHot {
    width: Width,
    at: u64,
    len: usize,
}

impl Writer {
    /// Writes `values` at their narrowest width.
    pub fn push_hot(&mut self, values: &[u32]) -> PlacedHot {
        let width = Width::of(values);
        let at = match width {
            Width::U8 => self.push_slice(&values.iter().map(|&it| it as u8).collect::<Vec<_>>()),
            Width::U16 => self.push_slice(&values.iter().map(|&it| it as u16).collect::<Vec<_>>()),
            Width::U32 => self.push_slice(values),
        };
        PlacedHot { width, at, len: values.len() }
    }

    /// Writes `values` bit-packed.
    pub fn push_warm(&mut self, values: &[u32]) -> PlacedWarm {
        let packed = Packed::pack(values);
        let at = self.push_slice(&packed.words);
        PlacedWarm { min: packed.min, bits: packed.bits, len: packed.len, at, words: packed.words.len() }
    }
}

impl HotColumn {
    /// The column a field at `field_at` holds, of the values `placed` wrote before it.
    pub fn new(field_at: u64, placed: PlacedHot) -> HotColumn {
        let field = |offset: usize| field_at + offset as u64;
        let mut column = HotColumn { u8s: Slice::EMPTY, u16s: Slice::EMPTY, u32s: Slice::EMPTY };
        match placed.width {
            Width::U8 => {
                column.u8s = Slice::between(field(offset_of!(HotColumn, u8s)), placed.at, placed.len)
            }
            Width::U16 => {
                column.u16s =
                    Slice::between(field(offset_of!(HotColumn, u16s)), placed.at, placed.len)
            }
            Width::U32 => {
                column.u32s =
                    Slice::between(field(offset_of!(HotColumn, u32s)), placed.at, placed.len)
            }
        }
        column
    }

    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<HotRef<'a>, FormatError> {
        let (u8s, u16s, u32s) =
            (self.u8s.follow(buffer)?, self.u16s.follow(buffer)?, self.u32s.follow(buffer)?);
        match (u8s.is_empty(), u16s.is_empty(), u32s.is_empty()) {
            (_, true, true) => Ok(HotRef::U8(u8s)),
            (true, false, true) => Ok(HotRef::U16(u16s)),
            (true, true, false) => Ok(HotRef::U32(u32s)),
            _ => Err(FormatError::Invalid {
                at: (self as *const HotColumn as usize).wrapping_sub(buffer.as_ptr() as usize)
                    as u64,
                what: "a hot column of more than one width",
            }),
        }
    }
}

/// A hot column read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotRef<'a> {
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
}

impl HotRef<'_> {
    pub fn width(&self) -> Width {
        match self {
            HotRef::U8(_) => Width::U8,
            HotRef::U16(_) => Width::U16,
            HotRef::U32(_) => Width::U32,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            HotRef::U8(it) => it.len(),
            HotRef::U16(it) => it.len(),
            HotRef::U32(it) => it.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, at: usize) -> u32 {
        match self {
            HotRef::U8(it) => it[at] as u32,
            HotRef::U16(it) => it[at] as u32,
            HotRef::U32(it) => it[at],
        }
    }

    pub fn to_vec(&self) -> Vec<u32> {
        match self {
            HotRef::U8(it) => it.iter().map(|&it| it as u32).collect(),
            HotRef::U16(it) => it.iter().map(|&it| it as u32).collect(),
            HotRef::U32(it) => it.to_vec(),
        }
    }
}

/// Values as offsets from their minimum, `bits` bits each, packed low bit first into `u64`s,
/// with one more word than they fill so a read of two words never leaves them. A column of 32
/// bits has 0 as its minimum, so `min` plus any `bits`-bit offset is a `u32`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Packed {
    pub min: u32,
    pub bits: u32,
    pub len: usize,
    pub words: Vec<u64>,
}

fn words_for(len: usize, bits: u32) -> usize {
    match bits {
        0 => 0,
        _ => (len * bits as usize).div_ceil(64) + 1,
    }
}

fn mask(bits: u32) -> u64 {
    (1u64 << bits) - 1
}

/// Whether a column of `bits`-bit offsets from `min` holds only `u32`s.
fn frame_fits(min: u32, bits: u32) -> bool {
    bits <= 32 && min as u64 + mask(bits) <= u32::MAX as u64
}

/// Element `at` of `bits`-bit values packed in `words`, before the minimum is added.
#[inline(always)]
fn unpacked(words: &[u64], bits: u32, at: usize) -> u32 {
    if bits == 0 {
        return 0;
    }
    let bit = at * bits as usize;
    let (word, shift) = (bit / 64, (bit % 64) as u32);
    let low = words[word] >> shift;
    let high = match shift + bits > 64 {
        true => words[word + 1] << (64 - shift),
        false => 0,
    };
    ((low | high) & mask(bits)) as u32
}

impl Packed {
    pub fn pack(values: &[u32]) -> Packed {
        let min = values.iter().copied().min().unwrap_or(0);
        let range = values.iter().copied().max().unwrap_or(0) - min;
        let bits = u32::BITS - range.leading_zeros();
        let min = match bits {
            32 => 0,
            _ => min,
        };
        let mut words = vec![0u64; words_for(values.len(), bits)];
        if bits > 0 {
            for (i, &value) in values.iter().enumerate() {
                let offset = (value - min) as u64;
                let bit = i * bits as usize;
                let (word, shift) = (bit / 64, (bit % 64) as u32);
                words[word] |= offset << shift;
                if shift + bits > 64 {
                    words[word + 1] |= offset >> (64 - shift);
                }
            }
        }
        Packed { min, bits, len: values.len(), words }
    }

    pub fn as_ref(&self) -> WarmRef<'_> {
        WarmRef { min: self.min, bits: self.bits, len: self.len, words: &self.words }
    }

    pub fn unpack(&self) -> Vec<u32> {
        self.as_ref().to_vec()
    }
}

record! {
    /// A warm column in a buffer: [`Packed`]'s fields, its words in the buffer.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct WarmColumn: 1 {
        min: u32,
        bits: u32,
        len: u32,
        words: Slice<u64>,
    }
}

/// A warm column's words written to a buffer, to be named by the record that holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlacedWarm {
    min: u32,
    bits: u32,
    len: usize,
    at: u64,
    words: usize,
}

impl WarmColumn {
    /// The column a field at `field_at` holds, of the words `placed` wrote before it.
    pub fn new(field_at: u64, placed: PlacedWarm) -> WarmColumn {
        let len = u32::try_from(placed.len).expect("a column of 2^32 values or more");
        let words = field_at + offset_of!(WarmColumn, words) as u64;
        WarmColumn {
            min: placed.min,
            bits: placed.bits,
            len,
            words: Slice::between(words, placed.at, placed.words),
        }
    }

    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<WarmRef<'a>, FormatError> {
        let words = self.words.follow(buffer)?;
        let at = (self as *const WarmColumn as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
        let fits = frame_fits(self.min, self.bits)
            && words.len() == words_for(self.len as usize, self.bits);
        match fits {
            true => Ok(WarmRef { min: self.min, bits: self.bits, len: self.len as usize, words }),
            false => Err(FormatError::Invalid { at, what: "a warm column whose words don't fit it" }),
        }
    }
}

/// A warm column read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WarmRef<'a> {
    pub min: u32,
    pub bits: u32,
    pub len: usize,
    pub words: &'a [u64],
}

macro_rules! unpack_by_width {
    ($bits:expr, $words:expr, $min:expr, $start:expr, $out:expr; $($n:literal)*) => {
        match $bits {
            $($n => unpack_fixed::<$n>($words, $min, $start, $out),)*
            other => unreachable!("a warm column of {other} bits"),
        }
    };
}

/// `out.len()` elements from `start`, with the width a constant so the loop's shifts and
/// masks are too.
fn unpack_fixed<const BITS: u32>(words: &[u64], min: u32, start: usize, out: &mut [u32]) {
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = min + unpacked(words, BITS, start + i);
    }
}

impl WarmRef<'_> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, at: usize) -> u32 {
        assert!(at < self.len, "element {at} of a column of {}", self.len);
        self.min + unpacked(self.words, self.bits, at)
    }

    /// Elements `start..start + out.len()` into `out`.
    pub fn unpack(&self, start: usize, out: &mut [u32]) {
        assert!(start + out.len() <= self.len, "past the end of a column of {}", self.len);
        unpack_by_width!(self.bits, self.words, self.min, start, out;
            0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32)
    }

    pub fn to_vec(&self) -> Vec<u32> {
        let mut out = vec![0; self.len];
        self.unpack(0, &mut out);
        out
    }
}

/// The codec of a cold column, its first byte before zstd.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Codec {
    /// Every value packed, as warm.
    Plain = 0,
    /// Runs of equal values: their values and their lengths, each packed.
    Runs = 1,
    /// A non-decreasing column: its first value and the differences, packed.
    Deltas = 2,
    /// Its distinct values, as deltas, and each value's index among them, packed.
    Dictionary = 3,
}

impl Codec {
    const ALL: [Codec; 4] = [Codec::Plain, Codec::Runs, Codec::Deltas, Codec::Dictionary];
}

/// A zstd dictionary for one kind of column, named by its content ID, which every cold column
/// made with it records: decoding with another dictionary is refused, not decoded wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColdDictionary {
    pub id: ContentId,
    pub bytes: Vec<u8>,
}

const DICTIONARY_DOMAIN: &str = "blaze v2 zstd dictionary 1";
const COLD_VERSION: u8 = 1;
const COLD_HEADER: usize = 1 + 32 + 8;
/// zstd's level for cold columns: written once in the background, read on every page-in.
const LEVEL: i32 = 9;

impl ColdDictionary {
    pub fn new(bytes: Vec<u8>) -> ColdDictionary {
        ColdDictionary { id: ContentId::of(DICTIONARY_DOMAIN, &bytes), bytes }
    }

    /// A dictionary of at most `max` bytes trained on `samples`, cold columns of one kind
    /// before zstd ([`raw_cold`]).
    pub fn train(samples: &[Vec<u8>], max: usize) -> Result<ColdDictionary, FormatError> {
        zstd::dict::from_samples(samples, max)
            .map(ColdDictionary::new)
            .map_err(|_| FormatError::Invalid { at: 0, what: "zstd could not train a dictionary" })
    }
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_packed(out: &mut Vec<u8>, values: &[u32]) {
    let packed = Packed::pack(values);
    put_varint(out, packed.min as u64);
    out.push(packed.bits as u8);
    put_varint(out, packed.len as u64);
    // The word past the last is never read but by a value spanning two words, so it is
    // left out here and added back when read.
    let used = packed.words.len().saturating_sub(1);
    for word in &packed.words[..used] {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

const CORRUPT: FormatError = FormatError::Invalid { at: 0, what: "a cold column that does not decode" };

/// The most values a column holds, as a [`Slice`]'s length does: a cold column claiming more
/// is refused before anything is allocated for it. A run or a constant column takes a few bytes
/// whatever its length, so its bytes can't bound it.
const MOST_VALUES: u64 = u32::MAX as u64;

struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn byte(&mut self) -> Result<u8, FormatError> {
        let byte = *self.bytes.get(self.at).ok_or(CORRUPT)?;
        self.at += 1;
        Ok(byte)
    }

    fn varint(&mut self) -> Result<u64, FormatError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(CORRUPT)
    }

    fn u32(&mut self) -> Result<u32, FormatError> {
        u32::try_from(self.varint()?).map_err(|_| CORRUPT)
    }

    /// A packed column of at most `most` values.
    fn packed(&mut self, most: usize) -> Result<Vec<u32>, FormatError> {
        let min = self.u32()?;
        let bits = self.byte()? as u32;
        let len = usize::try_from(self.varint()?).map_err(|_| CORRUPT)?;
        if len > most || !frame_fits(min, bits) {
            return Err(CORRUPT);
        }
        let stored = words_for(len, bits).saturating_sub(1);
        if stored.checked_mul(8).is_none_or(|need| need > self.bytes.len() - self.at) {
            return Err(CORRUPT);
        }
        let mut words = Vec::with_capacity(stored + 1);
        for _ in 0..stored {
            let end = self.at.checked_add(8).filter(|&end| end <= self.bytes.len()).ok_or(CORRUPT)?;
            let mut word = [0; 8];
            word.copy_from_slice(&self.bytes[self.at..end]);
            words.push(u64::from_le_bytes(word));
            self.at = end;
        }
        if bits > 0 {
            words.push(0);
        }
        Ok(WarmRef { min, bits, len, words: &words }.to_vec())
    }
}

/// `values` in `codec`, before zstd; `None` when the codec can't hold them (deltas of a
/// column that decreases).
fn encode_raw(values: &[u32], codec: Codec) -> Option<Vec<u8>> {
    assert!(values.len() as u64 <= MOST_VALUES, "a column of {} values", values.len());
    let mut out = vec![codec as u8];
    put_varint(&mut out, values.len() as u64);
    match codec {
        Codec::Plain => put_packed(&mut out, values),
        Codec::Runs => {
            let (mut runs, mut lengths) = (Vec::new(), Vec::new());
            for &value in values {
                match runs.last() {
                    Some(&last) if last == value => *lengths.last_mut().unwrap() += 1,
                    _ => {
                        runs.push(value);
                        lengths.push(1u32);
                    }
                }
            }
            put_packed(&mut out, &runs);
            put_packed(&mut out, &lengths);
        }
        Codec::Deltas => {
            if values.windows(2).any(|pair| pair[1] < pair[0]) {
                return None;
            }
            let deltas: Vec<u32> = values.windows(2).map(|pair| pair[1] - pair[0]).collect();
            put_varint(&mut out, values.first().copied().unwrap_or(0) as u64);
            put_packed(&mut out, &deltas);
        }
        Codec::Dictionary => {
            let mut distinct = values.to_vec();
            distinct.sort_unstable();
            distinct.dedup();
            let deltas: Vec<u32> = distinct.windows(2).map(|pair| pair[1] - pair[0]).collect();
            let indices: Vec<u32> = values
                .iter()
                .map(|value| distinct.binary_search(value).expect("a value among the distinct") as u32)
                .collect();
            put_varint(&mut out, distinct.len() as u64);
            put_varint(&mut out, distinct.first().copied().unwrap_or(0) as u64);
            put_packed(&mut out, &deltas);
            put_packed(&mut out, &indices);
        }
    }
    Some(out)
}

fn decode_raw(raw: &[u8]) -> Result<Vec<u32>, FormatError> {
    let mut bytes = Bytes { bytes: raw, at: 0 };
    let codec = bytes.byte()?;
    let len = bytes.varint()?;
    if len > MOST_VALUES {
        return Err(CORRUPT);
    }
    let (len, most) = (len as usize, len as usize);
    let values = match Codec::ALL.get(codec as usize).copied().ok_or(CORRUPT)? {
        Codec::Plain => bytes.packed(most)?,
        Codec::Runs => {
            let runs = bytes.packed(most)?;
            let lengths = bytes.packed(most)?;
            if runs.len() != lengths.len()
                || lengths.iter().map(|&it| it as u64).sum::<u64>() != len as u64
            {
                return Err(CORRUPT);
            }
            let mut values = Vec::with_capacity(len);
            for (&value, &length) in runs.iter().zip(&lengths) {
                values.extend(std::iter::repeat_n(value, length as usize));
            }
            values
        }
        Codec::Deltas => {
            let first = bytes.u32()?;
            let deltas = bytes.packed(most.saturating_sub(1))?;
            let mut values = Vec::with_capacity(deltas.len() + 1);
            if len > 0 {
                values.push(first);
            }
            for delta in deltas {
                let next = values.last().copied().unwrap_or(first).checked_add(delta).ok_or(CORRUPT)?;
                values.push(next);
            }
            values
        }
        Codec::Dictionary => {
            let count = usize::try_from(bytes.varint()?).map_err(|_| CORRUPT)?;
            if count > len {
                return Err(CORRUPT);
            }
            let first = bytes.u32()?;
            let deltas = bytes.packed(count.saturating_sub(1))?;
            let mut distinct = Vec::with_capacity(count);
            if count > 0 {
                distinct.push(first);
            }
            for delta in deltas {
                let next = distinct.last().copied().unwrap_or(first).checked_add(delta).ok_or(CORRUPT)?;
                distinct.push(next);
            }
            if distinct.len() != count {
                return Err(CORRUPT);
            }
            let indices = bytes.packed(most)?;
            indices
                .iter()
                .map(|&index| distinct.get(index as usize).copied().ok_or(CORRUPT))
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    match values.len() == len && bytes.at == raw.len() {
        true => Ok(values),
        false => Err(CORRUPT),
    }
}

/// `values` in the smallest codec before zstd (the first of [`Codec`]'s order among equals):
/// what a [`ColdDictionary`] is trained on.
pub fn raw_cold(values: &[u32]) -> Vec<u8> {
    Codec::ALL
        .iter()
        .filter_map(|&codec| encode_raw(values, codec))
        .min_by_key(|raw| raw.len())
        .expect("plain packing holds any column")
}

/// The codec [`encode_cold`] picks for `values`.
pub fn cold_codec(values: &[u32]) -> Codec {
    Codec::ALL[raw_cold(values)[0] as usize]
}

/// `values` as a cold column: the version, the dictionary's content ID (zeros for none), the
/// length before zstd, then zstd's frame of the smallest codec's bytes.
pub fn encode_cold(values: &[u32], dictionary: Option<&ColdDictionary>) -> Vec<u8> {
    compress(&raw_cold(values), dictionary)
}

pub fn decode_cold(bytes: &[u8], dictionary: Option<&ColdDictionary>) -> Result<Vec<u32>, FormatError> {
    decode_raw(&decompress(bytes, dictionary)?)
}

/// Bytes (text, a column of another type already laid out) as a cold column: zstd alone.
pub fn encode_cold_bytes(bytes: &[u8], dictionary: Option<&ColdDictionary>) -> Vec<u8> {
    compress(bytes, dictionary)
}

pub fn decode_cold_bytes(
    bytes: &[u8],
    dictionary: Option<&ColdDictionary>,
) -> Result<Vec<u8>, FormatError> {
    decompress(bytes, dictionary)
}

fn compress(raw: &[u8], dictionary: Option<&ColdDictionary>) -> Vec<u8> {
    let frame = match dictionary {
        Some(dictionary) => zstd::bulk::Compressor::with_dictionary(LEVEL, &dictionary.bytes)
            .and_then(|mut compressor| compressor.compress(raw)),
        None => zstd::bulk::compress(raw, LEVEL),
    }
    .unwrap_or_else(|error| panic!("zstd failed to compress {} bytes: {error}", raw.len()));
    let mut out = Vec::with_capacity(COLD_HEADER + frame.len());
    out.push(COLD_VERSION);
    out.extend_from_slice(&dictionary.map_or([0; 32], |it| it.id.0));
    out.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    out.extend_from_slice(&frame);
    out
}

/// The most a cold column may say it holds before zstd: no column is near it, and a corrupt
/// length is refused before anything that size is allocated.
const MOST_RAW: u64 = 1 << 32;

fn decompress(bytes: &[u8], dictionary: Option<&ColdDictionary>) -> Result<Vec<u8>, FormatError> {
    if bytes.len() < COLD_HEADER {
        return Err(FormatError::Header("a cold column shorter than its header"));
    }
    if bytes[0] != COLD_VERSION {
        return Err(FormatError::Header("a cold column of another version"));
    }
    let named = &bytes[1..33];
    match dictionary {
        Some(dictionary) if named != dictionary.id.0 => {
            return Err(FormatError::Header("a cold column made with another dictionary"));
        }
        None if named != [0; 32] => {
            return Err(FormatError::Header("a cold column made with a dictionary, read without"));
        }
        _ => {}
    }
    let mut len = [0; 8];
    len.copy_from_slice(&bytes[33..41]);
    let len = u64::from_le_bytes(len);
    if len > MOST_RAW {
        return Err(CORRUPT);
    }
    let frame = &bytes[COLD_HEADER..];
    let raw = match dictionary {
        Some(dictionary) => zstd::bulk::Decompressor::with_dictionary(&dictionary.bytes)
            .and_then(|mut decompressor| decompressor.decompress(frame, len as usize)),
        None => zstd::bulk::decompress(frame, len as usize),
    }
    .map_err(|_| CORRUPT)?;
    match raw.len() as u64 == len {
        true => Ok(raw),
        false => Err(CORRUPT),
    }
}

const _: () = assert!(size_of::<HotColumn>() == 24 && size_of::<WarmColumn>() == 20);

#[cfg(test)]
mod tests;
