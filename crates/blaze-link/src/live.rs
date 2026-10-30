//! What a stage is to a running program: a patch it applies (new code and data, the slots
//! that changed, the frames to register), or why it must start again.
//!
//! The patch borrows what it sends: a piece's bytes are read in place from the new state's
//! pages (copied into the stage's arena only when they span written pages), its cells are
//! the dirty slots alone, and the message is encoded into one buffer of its exact length.

#![forbid(unsafe_code)]

use blaze_codegen::Arch;
use blaze_ids::radix;
use blaze_live::patch::{
    ARCH_AARCH64, ARCH_X86_64, DESCRIPTOR_MAGIC, DESCRIPTOR_VERSION, Descriptor, Message, PatchRef,
};

use crate::Format;
use crate::model::Class;
use crate::stage::{Content, Stage, error};
use crate::state::{PieceId, State, What};
use crate::symbols::SymKey;

/// The 4 zero bytes that end a `.eh_frame`, written after the frames a patch adds.
const EH_FRAME_END: [u8; 4] = [0; 4];

impl Stage<'_> {
    /// The live descriptor's bytes: where the table, the patch regions and the directory
    /// are, as linked.
    pub fn fill_descriptor(&mut self) -> Result<(), crate::LinkError> {
        let Some(live) = &self.output.live else { return Ok(()) };
        let missing = || error(None, "a live image laid out without its jump table");
        let at = |this: &Self, key: SymKey| -> Option<u64> {
            let (piece, offset) = this.new.home(this.symbols.find(key)?)?;
            Some(this.new.piece(piece)?.addr + offset)
        };
        let region = |this: &Self, class: Class| -> [u64; 2] {
            this.new
                .space_of(&class)
                .map(|it| &this.new.spaces[it].region)
                .map_or([0, 0], |it| [it.addr, it.addr + it.capacity])
        };
        let descriptor = Descriptor {
            magic: DESCRIPTOR_MAGIC,
            version: DESCRIPTOR_VERSION,
            arch: match self.output.arch {
                Arch::X86_64 => ARCH_X86_64,
                Arch::Aarch64 => ARCH_AARCH64,
                Arch::Wasm32 => return Err(error(None, "live patching of wasm")),
            },
            id: self.new.lineage,
            generation: self.new.generation,
            this: at(self, SymKey::Descriptor).ok_or_else(missing)?,
            table: at(self, SymKey::Generation).ok_or_else(missing)?,
            slots: u64::from(self.new.capacity),
            patch_code: region(self, Class::Patch),
            patch_data: region(self, Class::PatchData),
            dir: [at(self, SymKey::LiveDir).ok_or_else(missing)?, live.path.len() as u64],
        };
        let sym = self.symbols.find(SymKey::Descriptor).ok_or_else(missing)?;
        let (piece, _) = self.new.home(sym).ok_or_else(missing)?;
        let bytes = self.arena.alloc_slice_copy(&descriptor.encode());
        self.contents.insert(piece, Content::Bytes(bytes));
        self.write.insert(piece);
        Ok(())
    }

    /// The encoded message for a program running the image before this stage.
    pub fn message(&self) -> Vec<u8> {
        let id = self.new.lineage;
        let generation = self.new.generation;
        let restart = |why: String| Message::Restart { id, generation, why }.encode();
        if self.full {
            return restart("the image was linked again from scratch".to_owned());
        }
        let old = &self.old.resolved.imports;
        if let Some(new) = self.new.resolved.imports.get(old.len()..).and_then(|it| it.first()) {
            return restart(format!("it imports `{}`, which the loader binds only at start", new.name));
        }
        if self.old.tls != self.new.tls {
            return restart("its thread-local storage changed".to_owned());
        }
        let home = |key: SymKey| self.symbols.find(key).and_then(|it| self.new.home(it)).map(|it| it.0);
        let (cells, descriptor) = (home(SymKey::Cells), home(SymKey::Descriptor));
        let ids = self.write.sorted();
        let mut code: Vec<(u64, &[u8])> = Vec::with_capacity(ids.len());
        let mut data: Vec<(u64, &[u8])> = Vec::with_capacity(ids.len());
        let mut rebase: Vec<u64> = Vec::new();
        let mut slots: Vec<(u32, u64)> = Vec::new();
        let mut frames: Vec<(u64, u64)> = Vec::new();
        let mut windows: Vec<(u64, u64, u64)> = Vec::new();
        for id in ids {
            let Some(piece) = self.new.piece(id) else { continue };
            let space = &self.new.spaces[piece.region as usize];
            if Some(id) == descriptor || space.zero || piece.zero() {
                continue;
            }
            if Some(id) == cells {
                let mut dirty = self.dirty_cells.sorted();
                dirty.retain(|slot| u64::from(*slot) * 8 + 8 <= piece.size);
                slots.reserve(dirty.len());
                for slot in dirty {
                    let at = u64::from(slot) * 8;
                    let value = word(&self.new, id, at).expect("a placed cells piece");
                    if word(self.old, id, at) != Some(value) {
                        slots.push((slot, value));
                    }
                }
                continue;
            }
            match &space.region.class {
                Class::Patch => code.push((piece.addr, piece.bytes_in(self.arena))),
                Class::PatchData
                | Class::EhFrame
                | Class::Rodata
                | Class::RelRo
                | Class::Data
                | Class::Got
                | Class::Named { .. }
                | Class::Table => data.push((piece.addr, piece.bytes_in(self.arena))),
                Class::Synthetic(_) => continue,
                class => {
                    return restart(format!(
                        "{} changed in {class:?}, which a running program can't take",
                        self.symbols.describe(piece.sym)
                    ));
                }
            }
            rebase.extend(piece.rebase().iter().map(|at| piece.addr + at));
            if !piece.binds().is_empty() {
                return restart(format!(
                    "{} refers to an import the loader binds only at start",
                    self.symbols.describe(piece.sym)
                ));
            }
            if !self.fresh.contains(id) {
                continue;
            }
            match piece.what() {
                What::Fde | What::NativeFrame { fde: true, .. } => frames.push((piece.addr, piece.size)),
                What::Function(_) => {
                    let unwind = piece.function().and_then(|it| it.windows).and_then(|it| self.new.home(it.0));
                    if let Some((at, offset)) = unwind {
                        let addr = self.new.piece(at).map_or(0, |it| it.addr) + offset;
                        windows.push((piece.addr, piece.addr + piece.size, addr));
                    }
                }
                _ => {}
            }
        }
        if !frames.is_empty() && self.output.format == Format::Elf {
            let Some(space) = self.new.space_of(&Class::EhFrame) else {
                return restart("its new call frame information has no region".to_owned());
            };
            let region = &self.new.spaces[space].region;
            if region.size + 4 > region.capacity {
                return restart(format!("its call frame information fills its region ({} bytes)", region.capacity));
            }
            data.push((region.addr + region.size, &EH_FRAME_END));
        }
        radix::sort(&mut rebase);
        let patch = PatchRef {
            id,
            generation,
            base: self.old.generation,
            code: &code,
            data: &data,
            rebase: &rebase,
            cells: &slots,
            frames: &frames,
            windows: &windows,
            functions: self.functions,
            statics: self.statics,
        };
        let mut out = Vec::with_capacity(patch.encoded_len());
        patch.encode_into(&mut out);
        out
    }
}

/// The 8 bytes at `at` in a placed piece, as a little-endian word.
fn word(state: &State, id: PieceId, at: u64) -> Option<u64> {
    let piece = state.piece(id)?;
    let space = state.spaces.get(piece.region as usize)?;
    if space.zero || piece.zero() || at + 8 > piece.size {
        return None;
    }
    let mut out = [0; 8];
    space.bytes.copy_to((piece.addr - space.region.addr + at) as usize, &mut out);
    Some(u64::from_le_bytes(out))
}
