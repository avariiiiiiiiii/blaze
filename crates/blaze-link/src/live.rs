//! What a stage is to a running program: a patch it applies (new code and data, the slots
//! that changed, the frames to register), or why it must start again.

use blaze_live::patch::{Descriptor, Message, Patch, ARCH_AARCH64, ARCH_X86_64, DESCRIPTOR_MAGIC, DESCRIPTOR_VERSION};

use crate::model::Class;
use crate::stage::{Stage, describe};
use crate::state::What;
use crate::symbols::SymKey;
use blaze_codegen::Arch;

impl Stage<'_> {
    /// The live descriptor's bytes: where the table, the patch regions and the directory
    /// are, as linked.
    pub fn fill_descriptor(&mut self) -> Result<(), crate::LinkError> {
        let Some(live) = &self.output.live else { return Ok(()) };
        let at = |this: &Self, key: SymKey| -> Option<u64> {
            let (piece, offset) = this.new.home(this.symbols.find(&key)?)?;
            Some(this.new.piece(piece)?.addr + offset)
        };
        let region = |this: &Self, class: Class| -> [u64; 2] {
            this.new
                .space_of(&class)
                .map(|it| &this.new.spaces[it].region)
                .map_or([0, 0], |it| [it.addr, it.addr + it.capacity])
        };
        let missing = || crate::stage::error(None, "a live image laid out without its jump table");
        let descriptor = Descriptor {
            magic: DESCRIPTOR_MAGIC,
            version: DESCRIPTOR_VERSION,
            arch: match self.output.arch {
                Arch::X86_64 => ARCH_X86_64,
                Arch::Aarch64 => ARCH_AARCH64,
                Arch::Wasm32 => return Err(crate::stage::error(None, "live patching of wasm")),
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
        let sym = self.symbols.find(&SymKey::Descriptor).ok_or_else(missing)?;
        let (piece, _) = self.new.home(sym).ok_or_else(missing)?;
        self.bytes.insert(piece, std::borrow::Cow::Owned(descriptor.encode()));
        self.write.insert(piece);
        Ok(())
    }

    /// What a program running the image before this stage is told.
    pub fn message(&self) -> Message {
        let id = self.new.lineage;
        let generation = self.new.generation;
        let restart = |why: String| Message::Restart { id, generation, why };
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
        let mut patch = Patch {
            id,
            generation,
            base: self.old.generation,
            functions: self.functions,
            statics: self.statics,
            ..Patch::default()
        };
        let cells = self.symbols.find(&SymKey::Cells).and_then(|it| self.new.home(it)).map(|it| it.0);
        let descriptor = self.symbols.find(&SymKey::Descriptor).and_then(|it| self.new.home(it)).map(|it| it.0);
        let mut ids: Vec<_> = self.write.iter().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let Some(piece) = self.new.piece(id) else { continue };
            let space = &self.new.spaces[piece.region as usize];
            if Some(id) == descriptor || space.zero || piece.zero {
                continue;
            }
            if Some(id) == cells {
                let old = self.old.piece(id).map(|it| self.old.bytes(it)).unwrap_or(&[]);
                let new = self.new.bytes(piece);
                for (slot, cell) in new.chunks_exact(8).enumerate() {
                    if old.get(slot * 8..slot * 8 + 8) != Some(cell) {
                        let value = u64::from_le_bytes(cell.try_into().expect("8 bytes"));
                        patch.cells.push((slot as u32, value));
                    }
                }
                continue;
            }
            let bytes = self.new.bytes(piece).to_vec();
            match &space.region.class {
                Class::Patch => patch.code.push((piece.addr, bytes)),
                Class::PatchData
                | Class::EhFrame
                | Class::Rodata
                | Class::RelRo
                | Class::Data
                | Class::Got
                | Class::Named { .. }
                | Class::Table => patch.data.push((piece.addr, bytes)),
                Class::Synthetic(_) => continue,
                class => {
                    return restart(format!(
                        "{} changed in {class:?}, which a running program can't take",
                        describe(self.symbols.key(piece.sym))
                    ));
                }
            }
            patch.rebase.extend(piece.rebase.iter().map(|at| piece.addr + at));
            if !piece.binds.is_empty() {
                return restart(format!(
                    "{} refers to an import the loader binds only at start",
                    describe(self.symbols.key(piece.sym))
                ));
            }
            if self.fresh.contains(&id) {
                match &piece.what {
                    What::Fde | What::NativeFrame { fde: true, .. } => patch.frames.push((piece.addr, piece.size)),
                    What::Function(info) => {
                        if let Some((unwind, _, _)) = info.windows {
                            if let Some((p, offset)) = self.new.home(unwind) {
                                let addr = self.new.piece(p).map_or(0, |it| it.addr) + offset;
                                patch.windows.push((piece.addr, piece.addr + piece.size, addr));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        patch.rebase.sort_unstable();
        Message::Patch(patch)
    }
}
