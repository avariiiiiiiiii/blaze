//! The stencil library's bytes: one little-endian blob per architecture variant, written by
//! the generator ([`Writer`]) and read in place by the patcher ([`Reader`]). Shared with the
//! generator (`build.rs` includes this file with `#[path]`); it uses nothing but `std`,
//! [`crate::shape`] and [`crate::enc`].
//!
//! ```text
//! Header   48 bytes: MAGIC, FORMAT, shape::VERSION, variant, counts and offsets, then id
//! Stencil  [stencils] 12 bytes: code offset u32, len u16, tail u8, holes u8, first hole u32
//! Hole     [holes]     8 bytes: at u16, form u8, kind u8, addend i32
//! Index    pilots [buckets] u16, then slots [slots] (key u32, stencil u32)
//! Code     bytes
//! Rodata   bytes
//! ```
//!
//! The index is a PTHash perfect hash from [`crate::shape::Shape::key`] to a stencil: key `k`
//! is in bucket `b = mix(k, 0) % buckets`, and its slot is `mix(k ^ pilot[b], 1) % slots`.

use std::fmt;

use crate::{enc::Form, shape::Shape};

pub const MAGIC: [u8; 8] = *b"BZSTENC1";
/// Bumped with any change to this layout.
pub const FORMAT: u32 = 1;
pub const HEADER: usize = 48 + 32;
pub const STENCIL: usize = 12;
pub const HOLE: usize = 8;

/// Which library: an architecture and the CPU baseline its stencils may use.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Variant {
    /// x86-64 baseline; `+cx16` only in the `i128` `atomic_cas` stencils.
    X86_64,
    /// AArch64 generic.
    Aarch64,
    /// AArch64 generic with `+lse` (Cranelift's `has_lse`, for `apple-m1`).
    Aarch64Lse,
}

impl Variant {
    pub const ALL: [Variant; 3] = [Variant::X86_64, Variant::Aarch64, Variant::Aarch64Lse];

    pub const fn from_u8(it: u8) -> Option<Variant> {
        if (it as usize) < Variant::ALL.len() { Some(Variant::ALL[it as usize]) } else { None }
    }

    /// The file the generator writes it to in `OUT_DIR`.
    pub const fn file(self) -> &'static str {
        match self {
            Variant::X86_64 => "x86_64.stencils",
            Variant::Aarch64 => "aarch64.stencils",
            Variant::Aarch64Lse => "aarch64_lse.stencils",
        }
    }
}

/// What a hole is filled with (stencil.md, "Holes").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum HoleKind {
    /// The frame offset of operand 0's, 1's or 2's slot.
    Slot0,
    Slot1,
    Slot2,
    /// The frame offset of result 0's or 1's slot.
    Result0,
    Result1,
    /// Operand 0's, 1's or 2's immediate.
    Imm0,
    Imm1,
    Imm2,
    /// The memory access's offset; for the stack ops, the stack slot's frame offset plus it.
    Offset,
    /// The code after this instruction's: the jump to the continuation.
    Next,
    /// The taken branch's edge.
    Target,
    /// The trap stub of the instruction's trap code.
    Trap,
    /// The instruction's 16-byte constant in the function's constant area.
    Const,
    /// The stencil's own read-only bytes, `addend` bytes into the library's rodata (the
    /// patcher places that range once per function and makes the hole's addend relative to
    /// it).
    Rodata,
}

impl HoleKind {
    pub const ALL: [HoleKind; 14] = [
        HoleKind::Slot0,
        HoleKind::Slot1,
        HoleKind::Slot2,
        HoleKind::Result0,
        HoleKind::Result1,
        HoleKind::Imm0,
        HoleKind::Imm1,
        HoleKind::Imm2,
        HoleKind::Offset,
        HoleKind::Next,
        HoleKind::Target,
        HoleKind::Trap,
        HoleKind::Const,
        HoleKind::Rodata,
    ];

    pub const fn from_u8(it: u8) -> Option<HoleKind> {
        if (it as usize) < HoleKind::ALL.len() { Some(HoleKind::ALL[it as usize]) } else { None }
    }

    /// The generator's name of its symbol (`hole.slot0`, ...), which the stencils' IR refers
    /// to and the object's relocations name.
    pub const fn symbol(self) -> &'static str {
        match self {
            HoleKind::Slot0 => "hole.slot0",
            HoleKind::Slot1 => "hole.slot1",
            HoleKind::Slot2 => "hole.slot2",
            HoleKind::Result0 => "hole.result0",
            HoleKind::Result1 => "hole.result1",
            HoleKind::Imm0 => "hole.imm0",
            HoleKind::Imm1 => "hole.imm1",
            HoleKind::Imm2 => "hole.imm2",
            HoleKind::Offset => "hole.offset",
            HoleKind::Next => "hole.next",
            HoleKind::Target => "hole.target",
            HoleKind::Trap => "hole.trap",
            HoleKind::Const => "hole.const",
            HoleKind::Rodata => "hole.rodata",
        }
    }
}

/// One hole: 8 bytes in the library.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hole {
    /// Offset in the stencil of its field (x86-64) or instruction (AArch64).
    pub at: u16,
    pub form: Form,
    pub kind: HoleKind,
    pub addend: i32,
}

/// One stencil, as the patcher reads it: borrowed from the library's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stencil<'a> {
    pub code: &'a [u8],
    /// Bytes at the end of `code` that are its jump to `Next`, left out when the next
    /// instruction's code follows directly; 0 when its last instruction is not that jump.
    pub tail: u8,
    holes: &'a [u8],
}

impl<'a> Stencil<'a> {
    pub fn holes(&self) -> impl ExactSizeIterator<Item = Hole> + 'a {
        let holes: &'a [u8] = self.holes;
        holes.chunks_exact(HOLE).map(decode_hole)
    }

    /// Bytes copied when the next instruction follows directly.
    pub fn body(&self) -> &'a [u8] {
        let code: &'a [u8] = self.code;
        &code[..code.len() - usize::from(self.tail)]
    }
}

/// A hole from its 8 bytes, which [`Reader::new`] checked.
fn decode_hole(bytes: &[u8]) -> Hole {
    let _ = bytes;
    unimplemented!("fastcompile-stencil-gen")
}

/// Why a library's bytes were refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatError {
    Magic,
    /// The format or shape version it was written with, and ours.
    Version { found: (u32, u32), ours: (u32, u32) },
    Variant(u8),
    /// A count or offset out of the bytes, or a hole out of its stencil, by what it is.
    Bounds(&'static str),
    Form(u8),
    Kind(u8),
    /// The writer was given a shape twice, or one [`Shape::exists`] refuses.
    Shape(u32),
    /// The writer's index found no pilot for a bucket (it retries with more buckets first).
    Index,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Magic => f.write_str("not a stencil library"),
            FormatError::Version { found, ours } => write!(
                f,
                "a library of format {} and shapes {}, not {} and {}",
                found.0, found.1, ours.0, ours.1
            ),
            FormatError::Variant(it) => write!(f, "variant {it} is none"),
            FormatError::Bounds(what) => write!(f, "{what} is out of bounds"),
            FormatError::Form(it) => write!(f, "hole form {it} is none"),
            FormatError::Kind(it) => write!(f, "hole kind {it} is none"),
            FormatError::Shape(key) => match Shape::from_key(*key) {
                Some(shape) => write!(f, "shape {shape} twice or not of the set"),
                None => write!(f, "shape key {key:#x} is no shape"),
            },
            FormatError::Index => f.write_str("no perfect index was found"),
        }
    }
}

/// A library read in place: every count, offset and hole checked once by [`Reader::new`],
/// then read without checks beyond slice bounds.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Checks the header (magic, [`FORMAT`], [`crate::shape::VERSION`]), that every column is
    /// in the bytes, and that every hole is a known form and kind inside its stencil.
    pub fn new(bytes: &'a [u8]) -> Result<Reader<'a>, FormatError> {
        let _ = bytes;
        unimplemented!("fastcompile-stencil-gen")
    }

    pub fn variant(&self) -> Variant {
        unimplemented!("fastcompile-stencil-gen")
    }

    /// The BLAKE3 hash of the bytes with this field zeroed, which the writer computed.
    pub fn id(&self) -> [u8; 32] {
        unimplemented!("fastcompile-stencil-gen")
    }

    pub fn len(&self) -> u32 {
        unimplemented!("fastcompile-stencil-gen")
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The stencil of `key`: two multiplies, a pilot and a compare.
    pub fn find(&self, key: u32) -> Option<u32> {
        let _ = key;
        unimplemented!("fastcompile-stencil-gen")
    }

    /// Stencil `index`, below [`Reader::len`].
    pub fn stencil(&self, index: u32) -> Stencil<'a> {
        let _ = index;
        unimplemented!("fastcompile-stencil-gen")
    }

    /// The key stencil `index` was written for, from the index's slots.
    pub fn key(&self, index: u32) -> u32 {
        let _ = index;
        unimplemented!("fastcompile-stencil-gen")
    }

    /// The library's read-only bytes, which `Rodata` holes' addends are offsets into.
    pub fn rodata(&self) -> &'a [u8] {
        unimplemented!("fastcompile-stencil-gen")
    }
}

/// Builds a library: the generator pushes each stencil in [`crate::shape::each`]'s order.
pub struct Writer {
    variant: Variant,
    keys: Vec<u32>,
    stencils: Vec<u8>,
    holes: Vec<u8>,
    code: Vec<u8>,
    rodata: Vec<u8>,
}

impl Writer {
    pub fn new(variant: Variant) -> Writer {
        Writer {
            variant,
            keys: Vec::new(),
            stencils: Vec::new(),
            holes: Vec::new(),
            code: Vec::new(),
            rodata: Vec::new(),
        }
    }

    /// Appends the read-only bytes a stencil refers to (LLVM's constant pool), aligned to
    /// `align`; the offset its `Rodata` holes' addends are made relative to.
    pub fn rodata(&mut self, bytes: &[u8], align: u32) -> u32 {
        let _ = (bytes, align);
        unimplemented!("fastcompile-stencil-gen")
    }

    /// Appends one stencil; its index.
    pub fn push(
        &mut self,
        shape: Shape,
        code: &[u8],
        tail: u8,
        holes: &[Hole],
    ) -> Result<u32, FormatError> {
        let _ = (shape, code, tail, holes, &self.variant, &self.keys);
        let _ = (&self.stencils, &self.holes, &self.code, &self.rodata);
        unimplemented!("fastcompile-stencil-gen")
    }

    /// The library's bytes, with its perfect index and `hash` (BLAKE3, given by the caller,
    /// which has the dependency) of the bytes as its id.
    pub fn finish(self, hash: impl FnOnce(&[u8]) -> [u8; 32]) -> Result<Vec<u8>, FormatError> {
        let _ = hash;
        unimplemented!("fastcompile-stencil-gen")
    }
}
