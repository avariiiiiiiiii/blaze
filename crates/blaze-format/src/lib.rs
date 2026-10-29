//! blaze v2's data format (docs/v2/format.md section 5): **the bytes in memory are the bytes on
//! disk**.
//!
//! A long-lived structure (a query's result, a pack of the store, the daemon's image) is a
//! tree of [`Record`]s in one byte buffer: no pointer, no `Arc`, aligned, little-endian, and
//! position-independent, each reference a [`Rel`] or a [`Slice`] counted from where it sits.
//! So a buffer is mmapped and read in place, and is the same bytes wherever it is mapped.
//!
//! A buffer starts with a [`Header`] naming its [`SchemaHash`]: a reader of another schema
//! refuses it by name ([`FormatError::Schema`]) and the daemon rebuilds what it held from the
//! store's results. A [`Persist`] type is one a query can return: written once into a
//! buffer, read in place as its [`Persist::Ref`].
//!
//! Spans are never in a record that is hashed: they sit in a side table, and a record names
//! one by [`SpanRef`].

use std::{fmt, marker::PhantomData};

use blaze_ids::ContentId;
use serde::{Deserialize, Serialize};

#[cfg(target_endian = "big")]
compile_error!("blaze's format is little-endian, as every target blaze runs on is");

/// The hash of a record type's layout: its fields' names, types, offsets and the schema hashes
/// of the records it holds. A change to any of them is a new schema.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct SchemaHash(pub ContentId);

/// A type whose value is its bytes.
///
/// # Safety
///
/// The implementor is `#[repr(C)]` (or a primitive) with no padding, no pointer, reference,
/// `Box` or `Arc` in it, and every bit pattern [`Record::check`] accepts is a valid value:
/// [`open`] hands out `&Self` over bytes it has only checked with it.
pub unsafe trait Record: Copy + 'static {
    const SCHEMA: SchemaHash;
    /// Whether `bytes`, `size_of::<Self>()` of them at an aligned address, are a valid value,
    /// the [`Rel`]s and [`Slice`]s in it within `buffer`.
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError>;
}

/// A reference to a `T` elsewhere in the same buffer: its offset from this field's own
/// address, in bytes. 0 is no `T`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Rel<T> {
    pub offset: i32,
    of: PhantomData<fn() -> T>,
}

/// `len` `T`s in a row elsewhere in the same buffer, starting `offset` bytes from this field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Slice<T> {
    pub offset: i32,
    pub len: u32,
    of: PhantomData<fn() -> T>,
}

/// UTF-8 bytes in the buffer.
pub type Str = Slice<u8>;

impl<T> Rel<T> {
    pub const NONE: Rel<T> = Rel { offset: 0, of: PhantomData };

    pub fn get<'a>(&'a self, buffer: &'a [u8]) -> Option<&'a T> {
        let _ = buffer;
        unimplemented!("v2: following a checked relative reference (v2-format)")
    }
}

impl<T> Slice<T> {
    pub fn get<'a>(&'a self, buffer: &'a [u8]) -> &'a [T] {
        let _ = buffer;
        unimplemented!("v2: following a checked relative slice (v2-format)")
    }
}

/// What a buffer starts with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Header {
    /// `b"BLAZEV2\0"`.
    pub magic: [u8; 8],
    pub schema: SchemaHash,
    /// The whole buffer's length, this header included.
    pub len: u64,
    /// Where the root record is, from the buffer's start.
    pub root: u64,
}

pub const MAGIC: [u8; 8] = *b"BLAZEV2\0";

/// The alignment every buffer is mapped and written at: records align to at most this.
pub const ALIGN: usize = 16;

/// The root `T` of `buffer`, after checking its header, its schema and every record reached
/// from the root.
pub fn open<T: Record>(buffer: &[u8]) -> Result<&T, FormatError> {
    let _ = buffer;
    unimplemented!("v2: opening a buffer, its header and its records checked (v2-format)")
}

/// Appends records to one buffer, aligned, and gives each a place for the [`Rel`]s of others.
pub struct Writer {
    pub bytes: Vec<u8>,
}

impl Writer {
    pub fn new(schema: SchemaHash) -> Writer {
        let _ = schema;
        unimplemented!("v2: a buffer with its header (v2-format)")
    }

    /// Where `value` was written, from the buffer's start.
    pub fn push<T: Record>(&mut self, value: &T) -> u64 {
        let _ = value;
        unimplemented!("v2: appending an aligned record (v2-format)")
    }

    pub fn push_slice<T: Record>(&mut self, values: &[T]) -> u64 {
        let _ = values;
        unimplemented!("v2: appending aligned records in a row (v2-format)")
    }

    /// The buffer, with `root` as its root record.
    pub fn finish(self, root: u64) -> Vec<u8> {
        let _ = root;
        unimplemented!("v2: closing a buffer (v2-format)")
    }
}

/// A value a query returns, or the store keeps: written once, read in place.
pub trait Persist: Send + Sync + 'static {
    const SCHEMA: SchemaHash;
    /// The value as read from a buffer, borrowing it.
    type Ref<'a>: Copy;
    /// Writes the value, returning where its root is.
    fn write(&self, out: &mut Writer) -> u64;
    fn read(buffer: &[u8]) -> Result<Self::Ref<'_>, FormatError>;
}

/// A span in a span side table (docs/v2/format.md section 5, "Spans"): the anchor is the
/// content ID of the item the span is relative to, `index` its place in that item's table.
/// Never hashed into code: a formatting edit changes the table, not the code that names it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(C)]
pub struct SpanRef {
    pub anchor: ContentId,
    pub index: u32,
}

/// Why bytes are not a value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatError {
    /// The buffer is of another schema: made by another blaze, or of another type.
    Schema { found: SchemaHash, wanted: SchemaHash },
    /// Not a buffer: a wrong magic, a length that is not the buffer's.
    Header(&'static str),
    /// A record at `at` that is not a valid value: `what` says why.
    Invalid { at: u64, what: &'static str },
    /// A reference at `at` that leaves the buffer or is misaligned.
    OutOfBounds { at: u64 },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Schema { found, wanted } => {
                write!(f, "a buffer of schema {} where {} was wanted", found.0, wanted.0)
            }
            FormatError::Header(what) => write!(f, "not a blaze buffer: {what}"),
            FormatError::Invalid { at, what } => write!(f, "invalid record at byte {at}: {what}"),
            FormatError::OutOfBounds { at } => {
                write!(f, "a reference at byte {at} leaves the buffer or is misaligned")
            }
        }
    }
}

impl std::error::Error for FormatError {}
