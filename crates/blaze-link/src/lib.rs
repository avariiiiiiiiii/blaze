//! The parallel, in-place linker (docs/v2/plan/lanes/link.md; #275, #232).
//!
//! An [`Image`] is an executable (or a running program's code, for live patching) kept
//! linked. A [`Change`] says which functions are new or changed; each is placed in a
//! per-thread chunk of address space and written independently, relocations are applied in
//! parallel, and callers reach a function through its jump-table [`Slot`], so a function that
//! moved changes one slot. Nothing runs a half-patched image: [`Image::commit`] is the one
//! serial step, an atomic switch. A patch that panics is rolled back to the image before it.
//! Positions the program observes (`line!()`, `Location`) are filled here from the span side
//! table.

use blaze_codegen::{CompiledFunction, InstanceKey};
use blaze_core::{Effect, FileName};
use blaze_ids::ContentId;

/// A function's place in the jump table: stable for as long as the image lives.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Slot(pub u32);

/// What a link changes.
pub struct Change<'a> {
    pub added: Vec<&'a CompiledFunction>,
    pub removed: Vec<InstanceKey>,
    /// The span side tables the positions are filled from, by anchor.
    pub spans: Vec<(ContentId, &'a [u8])>,
}

/// The output of a link, made visible only by a commit.
pub struct Staged {
    _private: (),
}

/// Why a link failed: names the function and the relocation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LinkError {
    pub function: Option<InstanceKey>,
    pub what: String,
}

/// One linked executable, file or process.
pub struct Image {
    _private: (),
}

impl Image {
    /// A new image to be written at `to`, from scratch.
    pub fn new(to: FileName) -> Image {
        let _ = to;
        unimplemented!("v2: an empty image (v2-link)")
    }

    /// Places and relocates what `change` says, in parallel, into address space not yet
    /// visible.
    pub fn stage(&mut self, change: Change<'_>) -> Result<Staged, LinkError> {
        let _ = change;
        unimplemented!("v2: placing and relocating a change (v2-link)")
    }

    /// Makes `staged` the image, all at once, through effects (a rename of the file, or the
    /// running program's switch).
    pub fn commit(&mut self, staged: Staged, effects: &mut Vec<Effect>) {
        let _ = (staged, effects);
        unimplemented!("v2: the atomic commit (v2-link)")
    }

    /// Back to the image before the last commit: a patch that panicked.
    pub fn roll_back(&mut self, effects: &mut Vec<Effect>) {
        let _ = effects;
        unimplemented!("v2: rolling a patch back (v2-link)")
    }

    pub fn slot(&self, function: &InstanceKey) -> Option<Slot> {
        let _ = function;
        unimplemented!("v2: a function's slot (v2-link)")
    }
}
