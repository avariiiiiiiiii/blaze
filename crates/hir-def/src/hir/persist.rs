//! How HIR values are written into a [`blaze_format`] buffer (docs/v2/plan/lanes/format.md section 5),
//! for the results the database keeps on disk once they are cold: item trees, expression
//! stores, bodies and their source maps.
//!
//! A buffer is columns, each a [`Slice`] of plain records: one per arena ([`Node`]: what kind
//! of value each element is and where its fields begin) and one of words, [`WordsRec`], that
//! every value's fields are written in one after another, as its [`Code`] says. Each name or
//! other text is in the buffer once, as a [`Str`] in a table the words name it by; a
//! [`Symbol`] is written as its text and interned again when read.
//!
//! What salsa interns or tracks (a crate, a file, a macro call, a block, a syntax context) is
//! written as its dense index, which means something only to the database that made it: these
//! buffers are one run's state, the daemon's image of what it let go of ("IDs, two kinds":
//! a dense index may be kept in the image), never a key, a hash or the bytes of an output.

use std::mem::{offset_of, size_of};

use blaze_format::{FormatError, Slice, Str, Writer};
use cfg::{CfgAtom, CfgExpr};
use hir_expand::{
    HirFileId, InFile, MacroCallId,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::{Interned, Internable, Symbol};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use span::{Edition, ErasedFileAstId, FileAstId, Span, SpanAnchor, SyntaxContext};
use syntax::{TextRange, TextSize};
use thin_vec::ThinVec;

use crate::hir::ExprOrPatIdPacked;

/// `Code` for salsa's ids, as their bits: the database that wrote them is the one reading.
macro_rules! salsa_ids {
    ($($ty:ty),* $(,)?) => {$(
        impl $crate::hir::persist::Code for $ty {
            fn put(&self, w: &mut $crate::hir::persist::Words) {
                w.u64(::salsa::plumbing::AsId::as_id(self).as_bits());
            }

            fn get(
                r: &mut $crate::hir::persist::Read<'_>,
            ) -> Result<Self, ::blaze_format::FormatError> {
                Ok(<$ty as ::salsa::plumbing::FromId>::from_id(r.salsa_id()?))
            }
        }
    )*};
}
pub(crate) use salsa_ids;

blaze_format::record! {
    /// One element of an arena: which of its type's variants it is, and the word its fields
    /// begin at.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct Node: 1 {
        pub(crate) kind: u32,
        pub(crate) at: u32,
    }
}

blaze_format::record! {
    /// The words and the names of a buffer.
    #[derive(Clone, Copy)]
    pub(crate) struct WordsRec: 1 {
        pub(crate) words: Slice<u32>,
        pub(crate) names: Slice<Str>,
    }
}

/// A value written as words: at least one, so that a count read from a buffer is never more
/// than the words left.
pub(crate) trait Code: Sized {
    fn put(&self, w: &mut Words);
    fn get(r: &mut Read<'_>) -> Result<Self, FormatError>;
}

/// The words and names of a buffer being written.
#[derive(Default)]
pub(crate) struct Words {
    words: Vec<u32>,
    text: String,
    /// Each name's start in `text` and its length.
    names: Vec<(u32, u32)>,
    by_text: FxHashMap<Box<str>, u32>,
    by_symbol: FxHashMap<Symbol, u32>,
}

impl Words {
    pub(crate) fn new() -> Words {
        Words::default()
    }

    /// The word the next one written will be.
    pub(crate) fn at(&self) -> u32 {
        u32::try_from(self.words.len()).expect("more than 2^32 words of HIR in one buffer")
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.words.push(value);
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.u32(value as u32);
        self.u32((value >> 32) as u32);
    }

    pub(crate) fn u128(&mut self, value: u128) {
        self.u64(value as u64);
        self.u64((value >> 64) as u64);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.u32(value as u32);
    }

    /// A count of what follows.
    pub(crate) fn len(&mut self, len: usize) {
        self.u32(u32::try_from(len).expect("more than 2^32 values in one HIR list"));
    }

    pub(crate) fn str(&mut self, text: &str) {
        let index = self.name(text);
        self.u32(index);
    }

    pub(crate) fn symbol(&mut self, symbol: &Symbol) {
        let index = match self.by_symbol.get(symbol) {
            Some(&index) => index,
            None => {
                let index = self.name(symbol.as_str());
                self.by_symbol.insert(symbol.clone(), index);
                index
            }
        };
        self.u32(index);
    }

    pub(crate) fn put<T: Code>(&mut self, value: &T) {
        value.put(self);
    }

    /// A count, then each of `values`.
    pub(crate) fn seq<'a, T: Code + 'a>(&mut self, values: impl ExactSizeIterator<Item = &'a T>) {
        self.len(values.len());
        values.for_each(|it| it.put(self));
    }

    fn name(&mut self, text: &str) -> u32 {
        if let Some(&index) = self.by_text.get(text) {
            return index;
        }
        let index = u32::try_from(self.names.len()).expect("more than 2^32 names in one buffer");
        let start = u32::try_from(self.text.len()).expect("more than 4 GiB of names in one buffer");
        let len = u32::try_from(text.len()).expect("a name of 4 GiB or more");
        self.text.push_str(text);
        self.names.push((start, len));
        self.by_text.insert(text.into(), index);
        index
    }

    /// Writes the text, the names and the words, for a [`WordsRec`] to point at
    /// ([`Placed::rec`]).
    pub(crate) fn place(&self, out: &mut Writer) -> Placed {
        let text_at = out.push_slice(self.text.as_bytes());
        let names_at = out.next::<Str>();
        let names: Vec<Str> = (self.names.iter().enumerate())
            .map(|(index, &(start, len))| {
                let field_at = names_at + (index * size_of::<Str>()) as u64;
                Str::between(field_at, text_at + u64::from(start), len as usize)
            })
            .collect();
        let pushed = out.push_slice(&names);
        debug_assert_eq!(pushed, names_at, "`Writer::next` is not where `push_slice` writes");
        let words_at = out.push_slice(&self.words);
        Placed {
            words: Column { at: words_at, len: self.at() },
            names: Column { at: names_at, len: names.len() as u32 },
        }
    }
}

/// Where a column of records was written and how many it holds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Column {
    pub(crate) at: u64,
    pub(crate) len: u32,
}

impl Column {
    /// Writes `values` as a column.
    pub(crate) fn push<T: blaze_format::Record>(out: &mut Writer, values: &[T]) -> Column {
        let len = u32::try_from(values.len()).expect("a column of more than 2^32 records");
        Column { at: out.push_slice(values), len }
    }

    /// The slice of this column for a field that is written at `field_at`.
    pub(crate) fn slice<T>(self, field_at: u64) -> Slice<T> {
        match self.len {
            0 => Slice::EMPTY,
            len => Slice::between(field_at, self.at, len as usize),
        }
    }
}

/// Where [`Words::place`] wrote the words and the names.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placed {
    words: Column,
    names: Column,
}

impl Placed {
    /// The record of the words and names, for one written at `at`.
    pub(crate) fn rec(self, at: u64) -> WordsRec {
        WordsRec {
            words: self.words.slice(at + offset_of!(WordsRec, words) as u64),
            names: self.names.slice(at + offset_of!(WordsRec, names) as u64),
        }
    }
}

/// Reads the words of a checked buffer, from a word on.
#[derive(Clone)]
pub(crate) struct Read<'a> {
    words: &'a [u32],
    names: &'a [Str],
    buffer: &'a [u8],
    next: usize,
}

impl<'a> Read<'a> {
    /// The words of `rec`, which is in `buffer`, from the first.
    pub(crate) fn new(rec: &'a WordsRec, buffer: &'a [u8]) -> Read<'a> {
        Read { words: rec.words.get(buffer), names: rec.names.get(buffer), buffer, next: 0 }
    }

    /// The same words, from word `at`.
    pub(crate) fn from(&self, at: u32) -> Read<'a> {
        Read { next: at as usize, ..self.clone() }
    }

    /// Where the next word is in the buffer, for an error.
    fn here(&self) -> u64 {
        // An empty slice may point anywhere.
        let words = (self.words.as_ptr() as usize).saturating_sub(self.buffer.as_ptr() as usize);
        (words + self.next * size_of::<u32>()) as u64
    }

    pub(crate) fn invalid(&self, what: &'static str) -> FormatError {
        FormatError::Invalid { at: self.here(), what }
    }

    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        let word = *self.words.get(self.next).ok_or(FormatError::OutOfBounds { at: self.here() })?;
        self.next += 1;
        Ok(word)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, FormatError> {
        let low = self.u32()?;
        Ok(u64::from(low) | (u64::from(self.u32()?) << 32))
    }

    pub(crate) fn u128(&mut self) -> Result<u128, FormatError> {
        let low = self.u64()?;
        Ok(u128::from(low) | (u128::from(self.u64()?) << 64))
    }

    pub(crate) fn bool(&mut self) -> Result<bool, FormatError> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.invalid("a bool that is neither 0 nor 1")),
        }
    }

    /// A count of what follows, which is never more than the words left: each value is at
    /// least one.
    pub(crate) fn len(&mut self) -> Result<usize, FormatError> {
        let len = self.u32()? as usize;
        match len <= self.words.len().saturating_sub(self.next) {
            true => Ok(len),
            false => Err(self.invalid("a count of more values than there are words")),
        }
    }

    /// Which of `count` variants a value is.
    pub(crate) fn tag(&mut self, count: u32, what: &'static str) -> Result<u32, FormatError> {
        let tag = self.u32()?;
        match tag < count {
            true => Ok(tag),
            false => Err(self.invalid(what)),
        }
    }

    pub(crate) fn str(&mut self) -> Result<&'a str, FormatError> {
        let index = self.u32()? as usize;
        let name = self.names.get(index).ok_or_else(|| self.invalid("a name not in the table"))?;
        name.as_str(self.buffer)
    }

    pub(crate) fn symbol(&mut self) -> Result<Symbol, FormatError> {
        Ok(Symbol::intern(self.str()?))
    }

    pub(crate) fn get<T: Code>(&mut self) -> Result<T, FormatError> {
        T::get(self)
    }

    /// A count, then that many values.
    pub(crate) fn seq<T: Code, C: FromIterator<T>>(&mut self) -> Result<C, FormatError> {
        let len = self.len()?;
        (0..len).map(|_| T::get(self)).collect()
    }

    /// The id of something salsa made, as [`salsa_ids`] wrote it.
    pub(crate) fn salsa_id(&mut self) -> Result<salsa::Id, FormatError> {
        let bits = self.u64()?;
        match bits as u32 {
            0 => Err(self.invalid("a salsa id of index 0")),
            _ => Ok(salsa::Id::from_bits(bits)),
        }
    }
}

macro_rules! narrow {
    ($($ty:ty),*) => {$(
        impl Code for $ty {
            fn put(&self, w: &mut Words) {
                w.u32(u32::from(*self));
            }

            fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
                let word = r.u32()?;
                <$ty>::try_from(word).map_err(|_| r.invalid(concat!("a ", stringify!($ty), " out of range")))
            }
        }
    )*};
}
narrow!(u8, u16, u32);

impl Code for u64 {
    fn put(&self, w: &mut Words) {
        w.u64(*self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u64()
    }
}

impl Code for usize {
    fn put(&self, w: &mut Words) {
        w.u64(*self as u64);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let value = r.u64()?;
        usize::try_from(value).map_err(|_| r.invalid("a usize out of range"))
    }
}

impl Code for u128 {
    fn put(&self, w: &mut Words) {
        w.u128(*self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u128()
    }
}

impl Code for i128 {
    fn put(&self, w: &mut Words) {
        w.u128(*self as u128);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(r.u128()? as i128)
    }
}

impl Code for bool {
    fn put(&self, w: &mut Words) {
        w.bool(*self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.bool()
    }
}

impl Code for char {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(*self));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let word = r.u32()?;
        char::from_u32(word).ok_or_else(|| r.invalid("a char that is not a scalar value"))
    }
}

impl<T: Code> Code for Option<T> {
    fn put(&self, w: &mut Words) {
        match self {
            None => w.u32(0),
            Some(it) => {
                w.u32(1);
                it.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        match r.tag(2, "an option that is neither 0 nor 1")? {
            0 => Ok(None),
            _ => T::get(r).map(Some),
        }
    }
}

impl<T: Code> Code for Box<T> {
    fn put(&self, w: &mut Words) {
        (**self).put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        T::get(r).map(Box::new)
    }
}

impl<T: Code> Code for Box<[T]> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<T, _>()
    }
}

impl<T: Code> Code for Vec<T> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<T, _>()
    }
}

impl<T: Code> Code for ThinVec<T> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<T, _>()
    }
}

impl<A: smallvec::Array<Item: Code>> Code for SmallVec<A> {
    fn put(&self, w: &mut Words) {
        w.seq(self.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.seq::<A::Item, _>()
    }
}

impl<A: Code, B: Code> Code for (A, B) {
    fn put(&self, w: &mut Words) {
        self.0.put(w);
        self.1.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok((A::get(r)?, B::get(r)?))
    }
}

impl<A: Code, B: Code, C: Code> Code for (A, B, C) {
    fn put(&self, w: &mut Words) {
        self.0.put(w);
        self.1.put(w);
        self.2.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok((A::get(r)?, B::get(r)?, C::get(r)?))
    }
}

impl Code for Box<str> {
    fn put(&self, w: &mut Words) {
        w.str(self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.str().map(Box::from)
    }
}

impl Code for String {
    fn put(&self, w: &mut Words) {
        w.str(self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.str().map(String::from)
    }
}

impl Code for Symbol {
    fn put(&self, w: &mut Words) {
        w.symbol(self);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.symbol()
    }
}

impl Code for Name {
    fn put(&self, w: &mut Words) {
        w.symbol(self.symbol());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let symbol = r.symbol()?;
        match symbol.as_str().starts_with("r#") {
            true => Err(r.invalid("a name written with its `r#`")),
            false => Ok(Name::new_symbol_root(symbol)),
        }
    }
}

impl Code for PathKind {
    fn put(&self, w: &mut Words) {
        match *self {
            PathKind::Plain => w.u32(0),
            PathKind::Super(n) => {
                w.u32(1);
                w.u32(u32::from(n));
            }
            PathKind::Crate => w.u32(2),
            PathKind::Abs => w.u32(3),
            PathKind::DollarCrate(krate) => {
                w.u32(4);
                krate.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(5, "a path kind out of range")? {
            0 => PathKind::Plain,
            1 => PathKind::Super(r.get()?),
            2 => PathKind::Crate,
            3 => PathKind::Abs,
            _ => PathKind::DollarCrate(r.get()?),
        })
    }
}

impl Code for ModPath {
    fn put(&self, w: &mut Words) {
        self.kind.put(w);
        w.seq(self.segments().iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let kind = r.get()?;
        let segments: Vec<Name> = r.seq::<Name, _>()?;
        Ok(ModPath::from_segments(kind, segments))
    }
}

impl<T: Internable + Code> Code for Interned<T> {
    fn put(&self, w: &mut Words) {
        (**self).put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        T::get(r).map(Interned::new)
    }
}

impl Code for Edition {
    fn put(&self, w: &mut Words) {
        w.u32(*self as u32);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let edition = r.tag(Edition::LATEST as u32 + 1, "an edition out of range")?;
        Ok(Edition::from_u32(edition))
    }
}

/// The syntax context `raw` names, as `into_u32` gave it; `None` for `u32::MAX`, which no
/// context is.
pub(crate) fn syntax_context(raw: u32) -> Option<SyntaxContext> {
    if raw == u32::MAX {
        return None;
    }
    // SAFETY: `from_u32` needs a context of this database. `raw` is what `into_u32` gave in
    // the database that wrote the buffer, which is the one reading it (the module's docs):
    // bytes from outside this process reach a `read` only through
    // `blaze_format::read_verified`, which checks their content hash first, so a corrupted
    // buffer is refused before any context is decoded. `u32::MAX`, which `from_u32` would turn
    // into an overflow, is refused above.
    Some(unsafe { SyntaxContext::from_u32(raw) })
}

impl Code for SyntaxContext {
    fn put(&self, w: &mut Words) {
        w.u32(self.into_u32());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let raw = r.u32()?;
        syntax_context(raw).ok_or_else(|| r.invalid("a syntax context of u32::MAX"))
    }
}

impl Code for TextSize {
    fn put(&self, w: &mut Words) {
        w.u32(u32::from(*self));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u32().map(TextSize::from)
    }
}

impl Code for TextRange {
    fn put(&self, w: &mut Words) {
        self.start().put(w);
        self.end().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let (start, end): (TextSize, TextSize) = (r.get()?, r.get()?);
        match start <= end {
            true => Ok(TextRange::new(start, end)),
            false => Err(r.invalid("a range that ends before it starts")),
        }
    }
}

impl Code for ErasedFileAstId {
    fn put(&self, w: &mut Words) {
        w.u32(self.into_raw());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u32().map(ErasedFileAstId::from_raw)
    }
}

impl<N> Code for FileAstId<N> {
    fn put(&self, w: &mut Words) {
        self.erase().put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.get().map(FileAstId::from_erased_unchecked)
    }
}

impl Code for span::EditionedFileId {
    fn put(&self, w: &mut Words) {
        w.u32(self.as_u32());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let raw = r.u32()?;
        let edition = (raw & Self::EDITION_MASK) >> Self::FILE_ID_BITS;
        match raw & Self::RESERVED_MASK == 0 && edition <= Edition::LATEST as u32 {
            true => Ok(Self::from_raw(raw)),
            false => Err(r.invalid("a file id with reserved bits or an edition out of range")),
        }
    }
}

impl Code for SpanAnchor {
    fn put(&self, w: &mut Words) {
        self.file_id.put(w);
        self.ast_id.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(SpanAnchor { file_id: r.get()?, ast_id: r.get()? })
    }
}

impl Code for Span {
    fn put(&self, w: &mut Words) {
        self.range.put(w);
        self.anchor.put(w);
        self.ctx.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(Span { range: r.get()?, anchor: r.get()?, ctx: r.get()? })
    }
}

impl Code for CfgAtom {
    fn put(&self, w: &mut Words) {
        match self {
            CfgAtom::Flag(flag) => {
                w.u32(0);
                w.symbol(flag);
            }
            CfgAtom::KeyValue { key, value } => {
                w.u32(1);
                w.symbol(key);
                w.symbol(value);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a cfg atom out of range")? {
            0 => CfgAtom::Flag(r.symbol()?),
            _ => CfgAtom::KeyValue { key: r.symbol()?, value: r.symbol()? },
        })
    }
}

impl Code for CfgExpr {
    fn put(&self, w: &mut Words) {
        match self {
            CfgExpr::Invalid => w.u32(0),
            CfgExpr::Atom(atom) => {
                w.u32(1);
                atom.put(w);
            }
            CfgExpr::All(all) => {
                w.u32(2);
                all.put(w);
            }
            CfgExpr::Any(any) => {
                w.u32(3);
                any.put(w);
            }
            CfgExpr::Not(not) => {
                w.u32(4);
                not.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(5, "a cfg expression out of range")? {
            0 => CfgExpr::Invalid,
            1 => CfgExpr::Atom(r.get()?),
            2 => CfgExpr::All(r.get()?),
            3 => CfgExpr::Any(r.get()?),
            _ => CfgExpr::Not(r.get()?),
        })
    }
}

salsa_ids!(base_db::Crate, base_db::EditionedFileId, MacroCallId, crate::BlockId);

impl Code for HirFileId {
    fn put(&self, w: &mut Words) {
        match self {
            HirFileId::FileId(file) => {
                w.u32(0);
                file.put(w);
            }
            HirFileId::MacroFile(call) => {
                w.u32(1);
                call.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a file id that is neither a file nor a macro file")? {
            0 => HirFileId::FileId(r.get()?),
            _ => HirFileId::MacroFile(r.get()?),
        })
    }
}

impl<T: Code> Code for InFile<T> {
    fn put(&self, w: &mut Words) {
        self.file_id.put(w);
        self.value.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let file_id = r.get()?;
        Ok(InFile::new(file_id, r.get()?))
    }
}

impl<T> Code for la_arena::Idx<T> {
    fn put(&self, w: &mut Words) {
        w.u32(self.into_raw().into_u32());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(r.u32()?)))
    }
}

impl Code for ExprOrPatIdPacked {
    fn put(&self, w: &mut Words) {
        w.u32(self.0);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        r.u32().map(ExprOrPatIdPacked)
    }
}

/// Writes `node` for each value of `values` in turn, the value's fields as `put` writes them
/// and its kind as `kind` says, and returns the column of their [`Node`]s.
pub(crate) fn nodes<'a, T: 'a>(
    w: &mut Words,
    values: impl Iterator<Item = &'a T>,
    kind: impl Fn(&T) -> u32,
    mut put: impl FnMut(&T, &mut Words),
) -> Vec<Node> {
    values
        .map(|value| {
            let at = w.at();
            put(value, w);
            debug_assert!(w.at() > at, "a value written as no words");
            Node { kind: kind(value), at }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    blaze_format::record! {
        #[derive(Clone, Copy)]
        struct Root: 1 {
            words: WordsRec,
            first: u32,
        }
    }

    const SCHEMA: blaze_format::SchemaHash = <Root as blaze_format::Record>::SCHEMA;

    fn round_trip<T: Code + PartialEq + std::fmt::Debug>(value: T) {
        let mut w = Words::new();
        let first = w.at();
        value.put(&mut w);
        let mut out = Writer::new(SCHEMA);
        let placed = w.place(&mut out);
        let at = out.next::<Root>();
        let root = out.push(&Root { words: placed.rec(at + offset_of!(Root, words) as u64), first });
        let buffer = blaze_format::Aligned::copy_from(&out.finish(root));
        let root = blaze_format::open::<Root>(&buffer).expect("a buffer just written");
        let mut r = Read::new(&root.words, &buffer).from(root.first);
        assert_eq!(r.get::<T>().expect("a value just written"), value);
    }

    #[test]
    fn values_read_back_as_written() {
        round_trip(0u8);
        round_trip(u32::MAX);
        round_trip(u64::MAX - 1);
        round_trip(u128::MAX - 7);
        round_trip(i128::MIN + 3);
        round_trip('é');
        round_trip(Some(Box::<str>::from("a name")));
        round_trip(None::<u32>);
        round_trip(vec![String::from("x"), String::from("y"), String::from("x")]);
        round_trip((true, 7u16, Symbol::intern("sym")));
        round_trip(ModPath::from_segments(
            PathKind::Super(2),
            [Name::new_symbol_root(Symbol::intern("a")), Name::new_symbol_root(Symbol::intern("b"))],
        ));
        round_trip(CfgExpr::All(Box::new([
            CfgExpr::Atom(CfgAtom::Flag(Symbol::intern("test"))),
            CfgExpr::Not(Box::new(CfgExpr::Atom(CfgAtom::KeyValue {
                key: Symbol::intern("target_os"),
                value: Symbol::intern("linux"),
            }))),
        ])));
        round_trip(TextRange::new(3.into(), 9.into()));
        round_trip(SyntaxContext::root(Edition::Edition2021));
        round_trip(Edition::Edition2024);
    }

    #[test]
    fn a_count_past_the_words_is_refused() {
        let mut w = Words::new();
        w.u32(5);
        w.u32(1);
        let mut out = Writer::new(SCHEMA);
        let placed = w.place(&mut out);
        let at = out.next::<Root>();
        let root = out.push(&Root { words: placed.rec(at + offset_of!(Root, words) as u64), first: 0 });
        let buffer = blaze_format::Aligned::copy_from(&out.finish(root));
        let root = blaze_format::open::<Root>(&buffer).unwrap();
        assert!(Read::new(&root.words, &buffer).get::<Vec<u32>>().is_err());
    }
}
