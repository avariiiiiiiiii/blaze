//! The one buffer a function is written into: its bytes, labels for places not written yet
//! (blocks, edges, trap stubs, constants), fixups resolved once every label is bound, and the
//! relocations the linker resolves. Reserved once per function from the sizes of the
//! stencils and the glue's bounds, and reused across functions: writing never allocates.

use cranelift_codegen::binemit;

use crate::{
    Error,
    code::{Name, Reloc},
    enc::{self, Form},
};

/// A place in the function, bound once.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Label(pub u32);

/// A field to be written with a label's place once it is bound: `S` is the label's offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixup {
    pub at: u32,
    pub form: Form,
    pub label: Label,
    pub addend: i64,
}

#[derive(Default)]
pub struct Buf {
    bytes: Vec<u8>,
    /// By label: its offset, or `u32::MAX` while unbound.
    labels: Vec<u32>,
    fixups: Vec<Fixup>,
    relocs: Vec<Reloc>,
}

impl Buf {
    pub fn new() -> Buf {
        Buf::default()
    }

    /// Empties it for the next function, keeping every allocation.
    pub fn clear(&mut self) {
        unimplemented!("fastcompile-stencil-patch")
    }

    /// Room for `bytes` more bytes, `labels` labels and `fixups` fixups: one reservation per
    /// function, from the sizes the patcher adds up.
    pub fn reserve(&mut self, bytes: usize, labels: usize, fixups: usize) {
        let _ = (bytes, labels, fixups);
        unimplemented!("fastcompile-stencil-patch")
    }

    /// The offset the next byte is written at.
    pub fn len(&self) -> u32 {
        unimplemented!("fastcompile-stencil-patch")
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> &[u8] {
        unimplemented!("fastcompile-stencil-patch")
    }

    /// The bytes from `at`, for patching what was just copied.
    pub fn bytes_mut(&mut self, at: u32) -> &mut [u8] {
        let _ = at;
        unimplemented!("fastcompile-stencil-patch")
    }

    pub fn put(&mut self, bytes: &[u8]) {
        let _ = bytes;
        unimplemented!("fastcompile-stencil-patch")
    }

    pub fn put1(&mut self, it: u8) {
        self.put(&[it]);
    }

    pub fn put2(&mut self, it: u16) {
        self.put(&it.to_le_bytes());
    }

    pub fn put4(&mut self, it: u32) {
        self.put(&it.to_le_bytes());
    }

    pub fn put8(&mut self, it: u64) {
        self.put(&it.to_le_bytes());
    }

    /// Pads to a multiple of `align` (a power of two) with `fill` repeated: `nop`s (x86-64
    /// `0x90`; AArch64 `d503201f`) in code, so a padded gap is never executed as anything else.
    pub fn align(&mut self, align: u32, fill: &[u8]) {
        let _ = (align, fill);
        unimplemented!("fastcompile-stencil-patch")
    }

    /// A new unbound label.
    pub fn label(&mut self) -> Label {
        unimplemented!("fastcompile-stencil-patch")
    }

    /// Binds `label` to the current offset. A label is bound once.
    pub fn bind(&mut self, label: Label) {
        let _ = label;
        unimplemented!("fastcompile-stencil-patch")
    }

    pub fn offset(&self, label: Label) -> Option<u32> {
        let _ = label;
        unimplemented!("fastcompile-stencil-patch")
    }

    /// Writes the field of `form` at `at` with `label`'s place plus `addend` when the label
    /// is bound ([`Buf::resolve`]); PC-relative forms take `S + A - P`.
    pub fn fixup(&mut self, at: u32, form: Form, label: Label, addend: i64) {
        let _ = (at, form, label, addend);
        unimplemented!("fastcompile-stencil-patch")
    }

    /// Writes the field of `form` at `at` with `value` now: a hole's immediate or offset.
    pub fn patch(&mut self, at: u32, form: Form, value: i64) -> Result<(), enc::Overflow> {
        let _ = (at, form, value);
        unimplemented!("fastcompile-stencil-patch")
    }

    /// A relocation for the linker.
    pub fn reloc(&mut self, at: u32, kind: binemit::Reloc, name: Name, addend: i64) {
        let _ = (at, kind, name, addend);
        unimplemented!("fastcompile-stencil-patch")
    }

    /// Writes every fixup, now that every label it names is bound. An unbound label or a
    /// value that does not fit its field is an error naming the form and the offset.
    pub fn resolve(&mut self) -> Result<(), Error> {
        unimplemented!("fastcompile-stencil-patch")
    }

    /// The bytes and relocations (sorted by offset), moved out; the buffer keeps its
    /// labels' and fixups' allocations for the next function. The caller gives the next
    /// function's buffer back its byte allocation by [`Buf::recycle`].
    pub fn take(&mut self) -> (Vec<u8>, Vec<Reloc>) {
        unimplemented!("fastcompile-stencil-patch")
    }

    /// Gives the buffer an allocation to write the next function into (one a consumer of a
    /// [`crate::Code`] is done with), so a steady stream of compiles allocates nothing.
    pub fn recycle(&mut self, bytes: Vec<u8>, relocs: Vec<Reloc>) {
        let _ = (bytes, relocs);
        unimplemented!("fastcompile-stencil-patch")
    }
}
