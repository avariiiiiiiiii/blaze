//! The end of a stage: the writer's synthetic regions and headers, then the byte ranges of
//! the file a commit writes, and what was there before (for a rollback).

use std::sync::Arc;

use crate::model::Class;
use crate::stage::{Stage, error};
use crate::state::State;
use crate::LinkError;

/// What a stage leaves for its commit.
pub(crate) struct Finished {
    pub state: State,
    pub full: bool,
    pub len: u64,
    /// Ranges of the file and their bytes.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// The same ranges as the file had them before, and its length.
    pub undo: Vec<(u64, Vec<u8>)>,
    pub old_len: u64,
    /// The message for running programs.
    pub message: Option<blaze_live::patch::Message>,
}

const BLOCK: usize = 4096;

/// The bytes of `state`'s file in `at..at + len`: zero where nothing is.
pub(crate) fn file_bytes(state: &State, at: u64, len: u64) -> Vec<u8> {
    let mut out = vec![0; len as usize];
    let end = at + len;
    let mut copy = |offset: u64, bytes: &[u8]| {
        let from = offset.max(at);
        let to = (offset + bytes.len() as u64).min(end);
        if from < to {
            out[(from - at) as usize..(to - at) as usize]
                .copy_from_slice(&bytes[(from - offset) as usize..(to - offset) as usize]);
        }
    };
    for space in &state.spaces {
        if let (Some(offset), false) = (space.offset, space.zero) {
            copy(offset, &space.bytes[..(space.region.size as usize).min(space.bytes.len())]);
        }
    }
    for (offset, bytes) in state.parts.iter() {
        copy(*offset, bytes);
    }
    out
}

/// Every byte of `state`'s file, as writes.
pub(crate) fn whole(state: &State) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    for space in &state.spaces {
        if let (Some(offset), false) = (space.offset, space.zero) {
            let size = (space.region.size as usize).min(space.bytes.len());
            if size > 0 {
                out.push((offset, space.bytes[..size].to_vec()));
            }
        }
    }
    out.extend(state.parts.iter().cloned());
    out
}

impl<'a> Stage<'a> {
    pub fn finish(mut self) -> Result<Finished, LinkError> {
        let mut parts = self.parts()?;
        let synthetic = {
            let contents: Vec<&[u8]> = self.new.spaces.iter().map(|it| self.contents(it)).collect();
            let symbol = |name: &str| self.address_of(&parts.regions, name);
            let placed = parts.placed(&self, &contents, &symbol);
            self.writer.synthesize(&placed)?
        };
        for (class, bytes) in synthetic {
            let at = self
                .new
                .space_of(&class)
                .ok_or_else(|| error(None, format!("the writer filled {class:?}, which the image has no region for")))?;
            let space = &mut self.new.spaces[at];
            if bytes.len() as u64 > space.region.capacity {
                return Err(error(
                    None,
                    format!(
                        "{class:?} is {} bytes and has room for {}: link the image from scratch",
                        bytes.len(),
                        space.region.capacity
                    ),
                ));
            }
            space.region.size = bytes.len() as u64;
            space.bytes = Arc::new(bytes);
        }
        parts.regions = self.new.regions();
        let (offsets, file) = {
            let contents: Vec<&[u8]> = self.new.spaces.iter().map(|it| self.contents(it)).collect();
            let symbol = |name: &str| self.address_of(&parts.regions, name);
            let placed = parts.placed(&self, &contents, &symbol);
            let offsets: Vec<Option<u64>> =
                self.new.spaces.iter().map(|it| self.writer.offset(&placed, &it.region)).collect();
            (offsets, self.writer.file(&placed)?)
        };
        for (space, offset) in self.new.spaces.iter_mut().zip(offsets) {
            space.offset = if space.zero { None } else { offset };
        }
        self.new.file_len = file.len;
        self.new.parts = Arc::new(file.parts);
        let message = self.output.live.is_some().then(|| self.message());
        let writes = if self.full { whole(&self.new) } else { self.dirty() };
        let (undo, old_len) = if self.old.laid_out {
            if self.full {
                (whole(self.old), self.old.file_len)
            } else {
                let len = self.old.file_len;
                let undo = writes
                    .iter()
                    .filter(|(at, _)| *at < len)
                    .map(|(at, bytes)| (*at, file_bytes(self.old, *at, (bytes.len() as u64).min(len - at))))
                    .collect();
                (undo, len)
            }
        } else {
            (Vec::new(), 0)
        };
        Ok(Finished { full: self.full, len: self.new.file_len, writes, undo, old_len, message, state: self.new })
    }

    fn contents<'s>(&self, space: &'s crate::state::Space) -> &'s [u8] {
        if space.zero {
            return &[];
        }
        &space.bytes[..(space.region.size as usize).min(space.bytes.len())]
    }

    /// The ranges of the file an incremental link changes.
    fn dirty(&self) -> Vec<(u64, Vec<u8>)> {
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for id in &self.write {
            let Some(piece) = self.new.piece(*id) else { continue };
            let space = &self.new.spaces[piece.region as usize];
            if let (Some(offset), false, false) = (space.offset, space.zero, piece.zero) {
                if piece.size > 0 {
                    ranges.push((offset + piece.addr - space.region.addr, piece.size));
                }
            }
        }
        for (at, space) in self.new.spaces.iter().enumerate() {
            let Some(offset) = space.offset else { continue };
            let old = self.old.spaces.get(at).filter(|it| it.region.class == space.region.class);
            let size = space.region.size;
            match old {
                Some(old) if old.offset == Some(offset) => {
                    if old.region.size > size {
                        ranges.push((offset + size, old.region.size - size));
                    }
                    if matches!(space.region.class, Class::Synthetic(_)) {
                        let (a, b) = (self.contents(old), self.contents(space));
                        for start in (0..b.len()).step_by(BLOCK) {
                            let end = (start + BLOCK).min(b.len());
                            if a.get(start..end) != Some(&b[start..end]) {
                                ranges.push((offset + start as u64, (end - start) as u64));
                            }
                        }
                    }
                }
                _ => ranges.push((offset, size.max(old.map_or(0, |it| it.region.size)))),
            }
        }
        for (offset, bytes) in self.new.parts.iter() {
            if !self.old.parts.iter().any(|(o, b)| o == offset && b == bytes) {
                ranges.push((*offset, bytes.len() as u64));
            }
        }
        ranges.retain(|it| it.1 > 0);
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (at, len) in ranges {
            match merged.last_mut() {
                Some(last) if at <= last.0 + last.1 => last.1 = last.1.max(at + len - last.0),
                _ => merged.push((at, len)),
            }
        }
        merged.into_iter().map(|(at, len)| (at, file_bytes(&self.new, at, len))).collect()
    }
}
