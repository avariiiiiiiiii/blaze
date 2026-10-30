//! blaze v2's data format (docs/v2/plan/lanes/format.md section 5): **the bytes in memory are the bytes on
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
//! A reference always points at bytes written before the record that holds it: a [`Writer`]
//! writes children first and the root last. [`Record::check`] refuses any other reference, so
//! checking a buffer always ends, and costs what the records reached from the root are. Keep
//! columns of plain values ([`Slice`]s of integers): following one checks only its bounds.
//!
//! Spans are never in a record that is hashed: they sit in a side table ([`Spans`]), and a
//! record names one by [`SpanRef`].

mod schema;
pub mod tiers;

use std::{
    fmt,
    marker::PhantomData,
    mem::{align_of, offset_of, size_of, size_of_val},
    ops::Deref,
};

use blaze_ids::{ContentId, Fingerprint};
use serde::{Deserialize, Serialize};

pub use crate::schema::{Part, SCHEMA_DOMAIN, schema, schema_of};

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
/// [`open`] hands out `&Self` over bytes it has only checked with it. [`record!`] writes this
/// impl for a struct and asserts it has no padding.
pub unsafe trait Record: Copy + 'static {
    const SCHEMA: SchemaHash;
    /// Every `size_of::<Self>()` bytes are a valid value and hold no reference: `check`
    /// accepts anything at the right size and alignment, so a [`Slice`] of them is checked by
    /// its bounds alone, in O(1), however long it is.
    const PLAIN: bool = false;
    /// Whether `bytes`, `size_of::<Self>()` of them at an aligned address, are a valid value,
    /// the [`Rel`]s and [`Slice`]s in it within `buffer`.
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError>;
}

/// Checks the field of type `F` at `offset` in `record`, the bytes of a record in `buffer`:
/// what a [`Record::check`] calls for each of its fields.
pub fn check_field<F: Record>(
    record: &[u8],
    offset: usize,
    buffer: &[u8],
) -> Result<(), FormatError> {
    let at = (record.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
    let at = at + offset as u64;
    let Some(end) = offset.checked_add(size_of::<F>()).filter(|&end| end <= record.len()) else {
        return Err(FormatError::Invalid { at, what: "a field past the end of its record" });
    };
    let bytes = &record[offset..end];
    if !(bytes.as_ptr() as usize).is_multiple_of(align_of::<F>()) {
        return Err(FormatError::OutOfBounds { at });
    }
    F::check(bytes, buffer)
}

/// Whether `bytes` are where a `T` can be read: its size, at its alignment.
fn fits<T>(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
    let at = (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
    match bytes.len() == size_of::<T>() && (bytes.as_ptr() as usize).is_multiple_of(align_of::<T>())
    {
        true => Ok(()),
        false => Err(FormatError::Invalid { at, what: "a record of the wrong size or alignment" }),
    }
}

macro_rules! plain {
    ($($ty:ty => $name:literal),* $(,)?) => {$(
        // SAFETY: a primitive or a byte array, with no padding; every bit pattern is a value.
        unsafe impl Record for $ty {
            const SCHEMA: SchemaHash = schema($name);
            const PLAIN: bool = true;
            fn check(_: &[u8], _: &[u8]) -> Result<(), FormatError> {
                Ok(())
            }
        }
    )*};
}

plain! {
    u8 => "u8", u16 => "u16", u32 => "u32", u64 => "u64", u128 => "u128",
    i8 => "i8", i16 => "i16", i32 => "i32", i64 => "i64", i128 => "i128",
    ContentId => "ContentId", Fingerprint => "Fingerprint", SchemaHash => "SchemaHash",
}

// SAFETY: one byte, and `check` accepts only 0 and 1.
unsafe impl Record for bool {
    const SCHEMA: SchemaHash = schema("bool");
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        fits::<bool>(bytes, buffer)?;
        match bytes[0] {
            0 | 1 => Ok(()),
            _ => Err(FormatError::Invalid {
                at: (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64,
                what: "a bool that is neither 0 nor 1",
            }),
        }
    }
}

// SAFETY: an array has no padding between its elements, each checked as a `T`.
unsafe impl<T: Record, const N: usize> Record for [T; N] {
    const SCHEMA: SchemaHash =
        schema_of(&[Part::Str("array"), Part::U64(N as u64), Part::Schema(T::SCHEMA)]);
    const PLAIN: bool = T::PLAIN;
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        if T::PLAIN {
            return fits::<[T; N]>(bytes, buffer);
        }
        (0..N).try_for_each(|i| check_field::<T>(bytes, i * size_of::<T>(), buffer))
    }
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

/// The offset from a field at `field_at` to a target at `target_at`, both from the buffer's
/// start, the target written before the field.
fn offset(field_at: u64, target_at: u64) -> i32 {
    assert!(target_at <= field_at, "a reference to bytes written after it");
    i32::try_from(target_at as i64 - field_at as i64)
        .expect("a reference across more than 2 GiB of one buffer")
}

/// Where `field` is in `buffer`, from its start.
fn position<F>(field: &F, buffer: &[u8]) -> Result<u64, FormatError> {
    let (address, base) = (field as *const F as usize, buffer.as_ptr() as usize);
    let at = address.wrapping_sub(base);
    match address >= base && at.checked_add(size_of::<F>()).is_some_and(|end| end <= buffer.len())
    {
        true => Ok(at as u64),
        false => Err(FormatError::OutOfBounds { at: at as u64 }),
    }
}

/// Where `count` `T`s that a field at `field_at` points `offset` bytes away from start, from
/// the buffer's start: after the header, before the field, aligned for `T`.
fn target<T>(field_at: u64, offset: i32, count: u32, buffer: &[u8]) -> Result<usize, FormatError> {
    let bad = FormatError::OutOfBounds { at: field_at };
    let target = field_at as i64 + offset as i64;
    if target < HEADER_LEN as i64 {
        return Err(bad);
    }
    let target = target as u64;
    let len = (size_of::<T>() as u64).checked_mul(count as u64).ok_or(bad.clone())?;
    if target.checked_add(len).is_none_or(|end| end > field_at) {
        return Err(bad);
    }
    let address = (buffer.as_ptr() as usize).wrapping_add(target as usize);
    match address.is_multiple_of(align_of::<T>()) {
        true => Ok(target as usize),
        false => Err(bad),
    }
}

impl<T> Rel<T> {
    pub const NONE: Rel<T> = Rel { offset: 0, of: PhantomData };

    /// The reference a field at `field_at` holds to a `T` at `target_at`, both from the
    /// buffer's start: the `T` was written before the record holding the field.
    pub fn between(field_at: u64, target_at: u64) -> Rel<T> {
        assert!(target_at < field_at, "a reference to bytes written after it");
        Rel { offset: offset(field_at, target_at), of: PhantomData }
    }

    pub fn is_none(&self) -> bool {
        self.offset == 0
    }
}

impl<T: Record> Rel<T> {
    /// The `T` this points at, `None` for [`Rel::NONE`]. Panics when `buffer` is not the
    /// buffer this field is in, or the `T` is not a valid one there: a buffer from [`open`]
    /// was checked whole, so this checks again only what a reference itself holds.
    pub fn get<'a>(&'a self, buffer: &'a [u8]) -> Option<&'a T> {
        self.follow(buffer).unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<Option<&'a T>, FormatError> {
        if self.offset == 0 {
            return Ok(None);
        }
        let at = position(self, buffer)?;
        let target = target::<T>(at, self.offset, 1, buffer)?;
        let bytes = &buffer[target..target + size_of::<T>()];
        T::check(bytes, buffer)?;
        // SAFETY: `bytes` are in `buffer`, `size_of::<T>()` of them, aligned for `T`, and
        // `T::check` accepted them, which `Record`'s contract makes a valid `T`.
        Ok(Some(unsafe { &*bytes.as_ptr().cast::<T>() }))
    }
}

impl<T> Slice<T> {
    pub const EMPTY: Slice<T> = Slice { offset: 0, len: 0, of: PhantomData };

    /// The slice a field at `field_at` holds of `len` `T`s at `target_at`, both from the
    /// buffer's start: the `T`s were written before the record holding the field.
    pub fn between(field_at: u64, target_at: u64, len: usize) -> Slice<T> {
        let len = u32::try_from(len).expect("a slice of 2^32 values or more");
        Slice { offset: offset(field_at, target_at), len, of: PhantomData }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<T: Record> Slice<T> {
    /// The `T`s this points at. Panics as [`Rel::get`] does.
    pub fn get<'a>(&'a self, buffer: &'a [u8]) -> &'a [T] {
        self.follow(buffer).unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<&'a [T], FormatError> {
        if self.len == 0 {
            return Ok(&[]);
        }
        let at = position(self, buffer)?;
        let target = target::<T>(at, self.offset, self.len, buffer)?;
        let size = size_of::<T>();
        let bytes = &buffer[target..target + size * self.len as usize];
        if !T::PLAIN {
            (0..self.len as usize).try_for_each(|i| check_field::<T>(bytes, i * size, buffer))?;
        }
        // SAFETY: `bytes` are in `buffer`, `len` `T`s of them, aligned for `T` (the first is,
        // and a `T`'s size is a multiple of its alignment), and each was accepted by
        // `T::check` or is a `T::PLAIN` value, which `Record`'s contract makes a valid `T`.
        Ok(unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<T>(), self.len as usize) })
    }
}

impl Slice<u8> {
    /// The text this points at, refused when it is not UTF-8.
    pub fn as_str<'a>(&'a self, buffer: &'a [u8]) -> Result<&'a str, FormatError> {
        let bytes = self.follow(buffer)?;
        std::str::from_utf8(bytes).map_err(|_| FormatError::Invalid {
            at: (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64,
            what: "text that is not UTF-8",
        })
    }
}

// SAFETY: an `i32` and a zero-sized marker: every bit pattern is a `Rel`; `check` follows it
// and checks the `T` it reaches.
unsafe impl<T: Record> Record for Rel<T> {
    const SCHEMA: SchemaHash = schema_of(&[Part::Str("Rel"), Part::Schema(T::SCHEMA)]);
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        fits::<Rel<T>>(bytes, buffer)?;
        // SAFETY: `fits` checked size and alignment, and every bit pattern is a `Rel`.
        let rel = unsafe { &*bytes.as_ptr().cast::<Rel<T>>() };
        rel.follow(buffer).map(|_| ())
    }
}

// SAFETY: an `i32`, a `u32` and a zero-sized marker: every bit pattern is a `Slice`; `check`
// follows it and checks each `T` it reaches.
unsafe impl<T: Record> Record for Slice<T> {
    const SCHEMA: SchemaHash = schema_of(&[Part::Str("Slice"), Part::Schema(T::SCHEMA)]);
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        fits::<Slice<T>>(bytes, buffer)?;
        // SAFETY: `fits` checked size and alignment, and every bit pattern is a `Slice`.
        let slice = unsafe { &*bytes.as_ptr().cast::<Slice<T>>() };
        slice.follow(buffer).map(|_| ())
    }
}

/// Declares a `#[repr(C)]` struct of [`Record`] fields and implements [`Record`] for it: its
/// schema from its name, its version (the number after the name), and each field's name,
/// offset and schema; its check field by field. Fails to compile if the struct has padding:
/// order the fields by alignment, or spell the padding out as a `[u8; N]` field. The struct
/// must derive `Clone` and `Copy`.
///
/// ```ignore
/// blaze_format::record! {
///     #[derive(Clone, Copy, Debug)]
///     pub struct Node: 1 {
///         pub kind: u32,
///         pub children: Slice<u32>,
///     }
/// }
/// ```
#[macro_export]
macro_rules! record {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident: $version:literal {
            $( $(#[$field_meta:meta])* $field_vis:vis $field:ident: $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[repr(C)]
        $vis struct $name {
            $( $(#[$field_meta])* $field_vis $field: $ty, )*
        }

        const _: () = ::core::assert!(
            ::core::mem::size_of::<$name>() == 0 $( + ::core::mem::size_of::<$ty>() )*,
            ::core::concat!(::core::stringify!($name), " has padding: order its fields by alignment"),
        );

        // SAFETY: `#[repr(C)]` without padding (asserted above), each field a `Record` checked
        // by its own `check`.
        unsafe impl $crate::Record for $name {
            const SCHEMA: $crate::SchemaHash = $crate::schema_of(&[
                $crate::Part::Str(::core::stringify!($name)),
                $crate::Part::U64($version),
                $(
                    $crate::Part::Str(::core::stringify!($field)),
                    $crate::Part::U64(::core::mem::offset_of!($name, $field) as u64),
                    $crate::Part::Schema(<$ty as $crate::Record>::SCHEMA),
                )*
            ]);
            const PLAIN: bool = true $( && <$ty as $crate::Record>::PLAIN )*;

            fn check(
                bytes: &[u8],
                buffer: &[u8],
            ) -> ::core::result::Result<(), $crate::FormatError> {
                if <Self as $crate::Record>::PLAIN {
                    return ::core::result::Result::Ok(());
                }
                $(
                    $crate::check_field::<$ty>(
                        bytes,
                        ::core::mem::offset_of!($name, $field),
                        buffer,
                    )?;
                )*
                let _ = (bytes, buffer);
                ::core::result::Result::Ok(())
            }
        }
    };
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

/// How long a [`Header`] is: no record is in the first this many bytes of a buffer.
pub const HEADER_LEN: usize = size_of::<Header>();

const _: () = assert!(HEADER_LEN == 8 + 32 + 8 + 8, "the header has padding");

fn read_u64(buffer: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&buffer[at..at + 8]);
    u64::from_le_bytes(word)
}

/// The header of `buffer`, checked: its magic, its length, and that the buffer is at an
/// address aligned to [`ALIGN`], which every record in it relies on.
pub fn header(buffer: &[u8]) -> Result<Header, FormatError> {
    if buffer.len() < HEADER_LEN {
        return Err(FormatError::Header("shorter than a header"));
    }
    if !(buffer.as_ptr() as usize).is_multiple_of(ALIGN) {
        return Err(FormatError::Header("not at an address aligned to 16 bytes"));
    }
    let mut magic = [0; 8];
    magic.copy_from_slice(&buffer[..8]);
    if magic != MAGIC {
        return Err(FormatError::Header("a wrong magic"));
    }
    let mut schema = [0; 32];
    schema.copy_from_slice(&buffer[offset_of!(Header, schema)..offset_of!(Header, schema) + 32]);
    let header = Header {
        magic,
        schema: SchemaHash(ContentId(schema)),
        len: read_u64(buffer, offset_of!(Header, len)),
        root: read_u64(buffer, offset_of!(Header, root)),
    };
    if header.len != buffer.len() as u64 {
        return Err(FormatError::Header("a length that is not the buffer's"));
    }
    Ok(header)
}

/// The root `T` of `buffer`, after checking its header, its schema and every record reached
/// from the root.
pub fn open<T: Record>(buffer: &[u8]) -> Result<&T, FormatError> {
    open_as::<T>(buffer, T::SCHEMA)
}

/// The root `R` of a buffer of schema `schema`, checked as [`open`] checks: what a
/// [`Persist::read`] opens its buffer with, the buffer's schema being the `Persist` type's.
pub fn open_as<R: Record>(buffer: &[u8], schema: SchemaHash) -> Result<&R, FormatError> {
    let header = header(buffer)?;
    if header.schema != schema {
        return Err(FormatError::Schema { found: header.schema, wanted: schema });
    }
    let root = header.root;
    let end = root.checked_add(size_of::<R>() as u64).filter(|&end| end <= header.len);
    let Some(end) = end.filter(|_| root >= HEADER_LEN as u64 && root.is_multiple_of(align_of::<R>() as u64))
    else {
        return Err(FormatError::OutOfBounds { at: root });
    };
    let bytes = &buffer[root as usize..end as usize];
    R::check(bytes, buffer)?;
    // SAFETY: `bytes` are in `buffer`, `size_of::<R>()` of them, aligned for `R` (the buffer is
    // aligned to `ALIGN`, `root` to `R`), and `R::check` accepted them.
    Ok(unsafe { &*bytes.as_ptr().cast::<R>() })
}

/// The domain of a buffer's content ID ([`buffer_id`]).
pub const BUFFER_DOMAIN: &str = "blaze v2 buffer 1";

/// The content ID of a whole buffer, its header included: what the store and the daemon's
/// image key a buffer by, and [`open_verified`] checks it against.
pub fn buffer_id(buffer: &[u8]) -> ContentId {
    ContentId::of(BUFFER_DOMAIN, buffer)
}

/// [`open_as`] of bytes from outside this process (the store, a pack, the image, the
/// network), after checking they are the buffer `key` names. A decoder that trusts what a
/// record holds beyond its bounds (a salsa index, an interned handle) is only given bytes
/// opened this way.
pub fn open_verified<R: Record>(
    buffer: &[u8],
    schema: SchemaHash,
    key: ContentId,
) -> Result<&R, FormatError> {
    verify(buffer, key)?;
    open_as::<R>(buffer, schema)
}

/// [`Persist::read`] of bytes from outside this process, after checking they are the buffer
/// `key` names.
pub fn read_verified<P: Persist>(buffer: &[u8], key: ContentId) -> Result<P::Ref<'_>, FormatError> {
    verify(buffer, key)?;
    P::read(buffer)
}

fn verify(buffer: &[u8], key: ContentId) -> Result<(), FormatError> {
    let found = buffer_id(buffer);
    match found == key {
        true => Ok(()),
        false => Err(FormatError::Hash { found, wanted: key }),
    }
}

/// Appends records to one buffer, aligned, and gives each a place for the [`Rel`]s of others.
/// Padding between records is zeros, so equal values written in the same order are equal
/// bytes.
pub struct Writer {
    pub bytes: Vec<u8>,
}

fn align_up(at: usize, align: usize) -> usize {
    assert!(align <= ALIGN, "a record aligned to more than {ALIGN} bytes");
    at.next_multiple_of(align)
}

impl Writer {
    pub fn new(schema: SchemaHash) -> Writer {
        let mut bytes = Vec::with_capacity(4096);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&schema.0.0);
        bytes.extend_from_slice(&[0; 16]);
        debug_assert_eq!(bytes.len(), HEADER_LEN);
        Writer { bytes }
    }

    /// Where the next `T` pushed will be, from the buffer's start: what a record's [`Rel`]s
    /// and [`Slice`]s are made from before it is pushed.
    pub fn next<T: Record>(&self) -> u64 {
        align_up(self.bytes.len(), align_of::<T>()) as u64
    }

    fn pad(&mut self, align: usize) -> u64 {
        let at = align_up(self.bytes.len(), align);
        self.bytes.resize(at, 0);
        at as u64
    }

    /// Where `value` was written, from the buffer's start.
    pub fn push<T: Record>(&mut self, value: &T) -> u64 {
        self.push_slice(std::slice::from_ref(value))
    }

    pub fn push_slice<T: Record>(&mut self, values: &[T]) -> u64 {
        let at = self.pad(align_of::<T>());
        // SAFETY: a `Record` has no padding, so every byte of `values` is initialized.
        let bytes =
            unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) };
        self.bytes.extend_from_slice(bytes);
        at
    }

    /// Where `text` was written, for a [`Str`] of `text.len()` bytes.
    pub fn push_str(&mut self, text: &str) -> u64 {
        self.push_slice(text.as_bytes())
    }

    /// The buffer, with `root` as its root record.
    pub fn finish(mut self, root: u64) -> Vec<u8> {
        assert!(
            root >= HEADER_LEN as u64 && root < self.bytes.len() as u64,
            "the root is not a record of this buffer"
        );
        let len = self.pad(ALIGN);
        let at = offset_of!(Header, len);
        self.bytes[at..at + 8].copy_from_slice(&len.to_le_bytes());
        let at = offset_of!(Header, root);
        self.bytes[at..at + 8].copy_from_slice(&root.to_le_bytes());
        self.bytes
    }
}

/// A value a query returns, or the store keeps: written once, read in place.
pub trait Persist: Send + Sync + 'static {
    const SCHEMA: SchemaHash;
    /// The value as read from a buffer, borrowing it.
    type Ref<'a>: Copy;
    /// Writes the value, returning where its root is.
    fn write(&self, out: &mut Writer) -> u64;
    /// The value in `buffer`, checked as [`open_as`] checks. Bytes this process did not write
    /// are read through [`read_verified`], so a `read` may rely on the buffer being one some
    /// blaze wrote whole.
    fn read(buffer: &[u8]) -> Result<Self::Ref<'_>, FormatError>;
}

/// `value` as a buffer of its own.
pub fn to_bytes<P: Persist>(value: &P) -> Vec<u8> {
    let mut out = Writer::new(P::SCHEMA);
    let root = value.write(&mut out);
    out.finish(root)
}

#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; ALIGN]);

/// Owned bytes at an address aligned to [`ALIGN`]: what a buffer is read from when it is not
/// mmapped (a `Vec<u8>` is aligned to 1).
#[derive(Clone)]
pub struct Aligned {
    blocks: Vec<Block>,
    len: usize,
}

impl Aligned {
    pub fn copy_from(bytes: &[u8]) -> Aligned {
        let mut blocks = vec![Block([0; ALIGN]); bytes.len().div_ceil(ALIGN)];
        for (block, chunk) in blocks.iter_mut().zip(bytes.chunks(ALIGN)) {
            block.0[..chunk.len()].copy_from_slice(chunk);
        }
        Aligned { blocks, len: bytes.len() }
    }
}

impl Deref for Aligned {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `blocks` holds at least `len` initialized bytes, `Block` being 16 bytes
        // without padding.
        unsafe { std::slice::from_raw_parts(self.blocks.as_ptr().cast::<u8>(), self.len) }
    }
}

impl fmt::Debug for Aligned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Aligned").field("len", &self.len).finish()
    }
}

/// A span in a span side table (docs/v2/plan/lanes/format.md section 5, "Spans"): the anchor is the
/// content ID of the item the span is relative to, `index` its place in that item's table.
/// Never hashed into code: a formatting edit changes the table, not the code that names it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(C)]
pub struct SpanRef {
    pub anchor: ContentId,
    pub index: u32,
}

const _: () = assert!(size_of::<SpanRef>() == 32 + 4, "a SpanRef has padding");

// SAFETY: `#[repr(C)]` without padding (asserted above), of two fields every bit pattern of
// which is a value.
unsafe impl Record for SpanRef {
    const SCHEMA: SchemaHash = schema_of(&[
        Part::Str("SpanRef"),
        Part::U64(1),
        Part::Str("anchor"),
        Part::U64(offset_of!(SpanRef, anchor) as u64),
        Part::Schema(<ContentId as Record>::SCHEMA),
        Part::Str("index"),
        Part::U64(offset_of!(SpanRef, index) as u64),
        Part::Schema(<u32 as Record>::SCHEMA),
    ]);
    const PLAIN: bool = true;
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        fits::<SpanRef>(bytes, buffer)
    }
}

record! {
    /// A span of an anchor's span table: bytes from the anchor's start, so an edit that moves
    /// the anchor in its file moves none of its spans.
    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct RelSpan: 1 {
        pub start: u32,
        pub len: u32,
    }
}

record! {
    #[derive(Clone, Copy)]
    struct SpansRoot: 1 {
        anchor: ContentId,
        spans: Slice<RelSpan>,
    }
}

/// The span side table of one anchor: what a [`SpanRef`] of that anchor indexes.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Spans {
    pub anchor: ContentId,
    pub spans: Vec<RelSpan>,
}

impl Spans {
    pub fn new(anchor: ContentId) -> Spans {
        Spans { anchor, spans: Vec::new() }
    }

    /// Adds a span `len` bytes long, `start` bytes from the anchor's start.
    pub fn push(&mut self, start: u32, len: u32) -> SpanRef {
        let index = u32::try_from(self.spans.len()).expect("an anchor of 2^32 spans");
        self.spans.push(RelSpan { start, len });
        SpanRef { anchor: self.anchor, index }
    }

    /// The span `span` names, `None` for a span of another anchor or past this table.
    pub fn get(&self, span: SpanRef) -> Option<RelSpan> {
        match span.anchor == self.anchor {
            true => self.spans.get(span.index as usize).copied(),
            false => None,
        }
    }
}

/// [`Spans`] read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpansRef<'a> {
    pub anchor: ContentId,
    pub spans: &'a [RelSpan],
}

impl SpansRef<'_> {
    pub fn get(&self, span: SpanRef) -> Option<RelSpan> {
        match span.anchor == self.anchor {
            true => self.spans.get(span.index as usize).copied(),
            false => None,
        }
    }

    pub fn to_spans(&self) -> Spans {
        Spans { anchor: self.anchor, spans: self.spans.to_vec() }
    }
}

impl Persist for Spans {
    const SCHEMA: SchemaHash =
        schema_of(&[Part::Str("Spans"), Part::U64(1), Part::Schema(SpansRoot::SCHEMA)]);
    type Ref<'a> = SpansRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let spans = out.push_slice(&self.spans);
        let at = out.next::<SpansRoot>();
        let field = at + offset_of!(SpansRoot, spans) as u64;
        let root = SpansRoot {
            anchor: self.anchor,
            spans: Slice::between(field, spans, self.spans.len()),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        at
    }

    fn read(buffer: &[u8]) -> Result<SpansRef<'_>, FormatError> {
        let root = open_as::<SpansRoot>(buffer, Self::SCHEMA)?;
        Ok(SpansRef { anchor: root.anchor, spans: root.spans.follow(buffer)? })
    }
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
    /// Bytes that are not the buffer their key names ([`open_verified`]).
    Hash { found: ContentId, wanted: ContentId },
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
            FormatError::Hash { found, wanted } => {
                write!(f, "a buffer of content {found} where {wanted} was wanted")
            }
        }
    }
}

impl std::error::Error for FormatError {}

#[cfg(test)]
mod tests;
