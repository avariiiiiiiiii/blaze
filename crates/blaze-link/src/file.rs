//! The end of a stage: the writer's synthetic regions and headers, then the byte ranges of
//! the file a commit writes, and which of the old file's ranges they overwrite (for a
//! rollback).
//!
//! A full link writes the file as runs of at most [`RUN`] bytes, built in parallel, which the
//! commit submits together. An incremental link writes what changed: each piece it wrote whose
//! bytes differ from before, each jump-table cell it set (8 bytes, not the cells array), the
//! synthetic regions' changed blocks and the changed headers, with ranges closer than [`GAP`]
//! merged into one write. Every range's bytes are copied once, from the regions' pages
//! straight into the write. The undo is only ranges: the state the history keeps holds the
//! old bytes, read when a rollback needs them ([`restore`]).

use std::borrow::Cow;

use blaze_ids::radix;
use rayon::prelude::*;
use smallvec::SmallVec;

use crate::arena::collect_in;
use crate::LinkError;
use crate::cow::CowBytes;
use crate::model::Class;
use crate::stage::{Stage, error};
use crate::state::{Space, State, What};

/// What a stage leaves for its commit.
pub(crate) struct Finished {
    pub state: State,
    pub full: bool,
    pub len: u64,
    /// Ranges of the file and their bytes, by offset, none overlapping.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// The ranges of the old file the writes overwrite, clipped to its length: empty for a
    /// full link, whose undo is the whole old file.
    pub undo: Vec<(u64, u64)>,
    pub old_len: u64,
    /// The encoded message for running programs.
    pub message: Option<Vec<u8>>,
}

/// The longest write of a full link: large enough to keep a device's queue full in few
/// writes, small enough that the writes of one file run side by side.
const RUN: u64 = 8 << 20;
/// Ranges of an incremental link closer than this are one write, the bytes between included.
const GAP: u64 = 4096;
/// The block a synthetic region is compared in.
const BLOCK: usize = 4096;

/// Where each byte of a state's file comes from: its regions in the file and the writer's
/// parts, each sorted by offset, and the slots the writer encodes, laid over the regions.
/// Parts are copied last, so they win where they overlap anything.
struct Layout<'s> {
    spaces: SmallVec<[(u64, u64, &'s CowBytes); 32]>,
    encoded: &'s [(u64, u64)],
    parts: SmallVec<[(u64, &'s [u8]); 16]>,
}

impl<'s> Layout<'s> {
    fn of(state: &'s State) -> Layout<'s> {
        let mut spaces = SmallVec::new();
        spaces.extend(state.spaces.iter().filter_map(|space| {
            let offset = space.offset.filter(|_| !space.zero)?;
            let len = space.region.size.min(space.bytes.len() as u64);
            (len > 0).then_some((offset, len, &space.bytes))
        }));
        radix::sort_by_key(&mut spaces, |it| it.0);
        let mut parts = SmallVec::new();
        parts.extend(state.parts.iter().filter(|it| !it.1.is_empty()).map(|(at, bytes)| (*at, &bytes[..])));
        radix::sort_copy_by_key(&mut parts, |it| it.0);
        Layout { spaces, encoded: &state.encoded, parts }
    }

    /// The file's bytes in `at..at + out.len()` copied into `out`, which is zeros.
    fn copy(&self, at: u64, out: &mut [u8]) {
        let end = at + out.len() as u64;
        let first = self.spaces.partition_point(|it| it.0 + it.1 <= at);
        for &(offset, len, bytes) in self.spaces[first..].iter().take_while(|it| it.0 < end) {
            let from = offset.max(at);
            let to = (offset + len).min(end);
            bytes.copy_to((from - offset) as usize, &mut out[(from - at) as usize..(to - at) as usize]);
        }
        let first = self.encoded.partition_point(|it| it.0 + 8 <= at);
        for &(offset, value) in self.encoded[first..].iter().take_while(|it| it.0 < end) {
            let from = offset.max(at);
            let to = (offset + 8).min(end);
            out[(from - at) as usize..(to - at) as usize]
                .copy_from_slice(&value.to_le_bytes()[(from - offset) as usize..(to - offset) as usize]);
        }
        let first = self.parts.partition_point(|it| it.0 + it.1.len() as u64 <= at);
        for &(offset, bytes) in self.parts[first..].iter().take_while(|it| it.0 < end) {
            let from = offset.max(at);
            let to = (offset + bytes.len() as u64).min(end);
            out[(from - at) as usize..(to - at) as usize]
                .copy_from_slice(&bytes[(from - offset) as usize..(to - offset) as usize]);
        }
    }

    fn bytes(&self, at: u64, len: u64) -> Vec<u8> {
        // alloc: one buffer per write, which the commit owns (`Finished::writes`)
        let mut out = vec![0; len as usize];
        self.copy(at, &mut out);
        out
    }

    /// Every range of the file anything is in, merged where closer than `gap`.
    fn extents(&self, gap: u64) -> Vec<(u64, u64)> {
        let mut all = Vec::with_capacity(self.spaces.len() + self.parts.len());
        all.extend(self.spaces.iter().map(|it| (it.0, it.1)));
        all.extend(self.parts.iter().map(|it| (it.0, it.1.len() as u64)));
        merge(all, gap)
    }
}

/// Sorts ranges and merges those that overlap or are closer than `gap`.
fn merge(mut ranges: Vec<(u64, u64)>, gap: u64) -> Vec<(u64, u64)> {
    ranges.retain(|it| it.1 > 0);
    radix::sort_copy_by_key(&mut ranges, |&(at, len)| radix::pack_64(at, len));
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (at, len) in ranges {
        match merged.last_mut() {
            Some(last) if at <= last.0 + last.1 + gap => last.1 = last.1.max(at + len - last.0),
            _ => merged.push((at, len)),
        }
    }
    merged
}

/// Splits ranges into runs of at most [`RUN`] bytes, cut at multiples of it.
fn runs(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(ranges.len());
    for &(mut at, len) in ranges {
        let end = at + len;
        while at < end {
            let cut = ((at / RUN) + 1) * RUN;
            let to = cut.min(end);
            out.push((at, to - at));
            at = to;
        }
    }
    out
}

/// The bytes of `ranges` of `layout`'s file, built in parallel.
fn fill(layout: &Layout<'_>, ranges: &[(u64, u64)]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::with_capacity(ranges.len());
    out.par_extend(ranges.par_iter().with_min_len(4).map(|&(at, len)| (at, layout.bytes(at, len))));
    out
}

/// Every byte of `state`'s file, as runs of at most [`RUN`] bytes. The gaps between regions
/// are zeros in a file just truncated, so runs span them up to a page.
pub(crate) fn whole(state: &State) -> Vec<(u64, Vec<u8>)> {
    let layout = Layout::of(state);
    fill(&layout, &runs(&layout.extents(GAP)))
}

/// The writes that put back `state`'s file where a commit overwrote `ranges` of it (the whole
/// file when it was a full link): a rollback's.
pub(crate) fn restore(state: &State, full: bool, ranges: &[(u64, u64)]) -> Vec<(u64, Vec<u8>)> {
    if full {
        return whole(state);
    }
    fill(&Layout::of(state), ranges)
}

/// A region's placed bytes, borrowed when its pages are.
fn contents(space: &Space) -> Cow<'_, [u8]> {
    if space.zero {
        return Cow::Borrowed(&[]);
    }
    let size = space.region.size.min(space.bytes.len() as u64) as usize;
    if size == space.bytes.len() { space.bytes.contiguous() } else { space.bytes.read(0, size) }
}

/// The file offsets of the slots a writer encoded at `slots`' addresses ([`Writer::encode`]),
/// `spaces` in address order and placed in the file. A slot out of order, overlapping
/// another, or not wholly inside a region in the file is an error naming its address.
///
/// [`Writer::encode`]: crate::model::Writer::encode
fn slots_in_file(spaces: &[Space], slots: &[(u64, u64)]) -> Result<Vec<(u64, u64)>, LinkError> {
    let mut out = Vec::with_capacity(slots.len());
    let mut at = 0;
    let mut next = 0;
    for &(addr, value) in slots {
        if addr < next {
            // alloc: error path
            return Err(error(None, format!("the writer encoded the slot at {addr:#x} out of order or overlapping another")));
        }
        next = addr.saturating_add(8);
        let end = |it: &Space| it.region.addr + it.region.size.min(it.bytes.len() as u64);
        while spaces.get(at).is_some_and(|it| end(it) <= addr) {
            at += 1;
        }
        let space = spaces
            .get(at)
            .filter(|it| it.region.addr <= addr && next <= end(it))
            // alloc: error path
            .ok_or_else(|| error(None, format!("the writer encoded a slot at {addr:#x}, outside every placed region")))?;
        let (Some(offset), false) = (space.offset, space.zero) else {
            // alloc: error path
            return Err(error(None, format!("the writer encoded a slot at {addr:#x}, in a region not in the file")));
        };
        out.push((offset + addr - space.region.addr, value));
    }
    Ok(out)
}

/// The 8-byte ranges whose encoded slot differs between `old` and `new` (each sorted by file
/// offset): changed, added or no longer encoded, the last only where the new file still is.
fn changed_slots(old: &[(u64, u64)], new: &[(u64, u64)], len: u64, ranges: &mut Vec<(u64, u64)>) {
    let (mut i, mut j) = (0, 0);
    loop {
        let at = match (old.get(i), new.get(j)) {
            (None, None) => break,
            (Some(a), None) => {
                i += 1;
                a.0
            }
            (None, Some(b)) => {
                j += 1;
                b.0
            }
            (Some(a), Some(b)) if a.0 < b.0 => {
                i += 1;
                a.0
            }
            (Some(a), Some(b)) if b.0 < a.0 => {
                j += 1;
                b.0
            }
            (Some(a), Some(b)) => {
                i += 1;
                j += 1;
                if a.1 == b.1 {
                    continue;
                }
                b.0
            }
        };
        if at + 8 <= len {
            ranges.push((at, 8));
        }
    }
}

impl<'a> Stage<'a> {
    pub fn finish(mut self) -> Result<Finished, LinkError> {
        self.update_lists()?;
        let mut parts = self.parts()?;
        let arena = self.arena;
        let contents = collect_in(self.new.spaces.iter().map(contents), arena);
        let synthetic = {
            let refs = collect_in(contents.iter().map(|it| &**it), arena);
            let symbol = |name: &str| self.address_of(&parts.regions, name);
            let placed = parts.placed(&self, &refs, &symbol);
            self.writer.synthesize(&placed)?
        };
        let mut homes = Vec::with_capacity_in(synthetic.len(), arena);
        for (class, bytes) in &synthetic {
            let at = self
                .new
                .space_of(class)
                // alloc: error path
                .ok_or_else(|| error(None, format!("the writer filled {class:?}, which the image has no region for")))?;
            let capacity = self.new.spaces[at].region.capacity;
            if bytes.len() as u64 > capacity {
                return Err(error(
                    None,
                    // alloc: error path
                    format!(
                        "{class:?} is {} bytes and has room for {capacity}: link the image from scratch",
                        bytes.len()
                    ),
                ));
            }
            parts.regions[at].size = bytes.len() as u64;
            homes.push(at);
        }
        let (offsets, file, encoded) = {
            let mut refs = collect_in(contents.iter().map(|it| &**it), arena);
            for (at, (_, bytes)) in homes.iter().zip(&synthetic) {
                refs[*at] = bytes.as_slice();
            }
            let symbol = |name: &str| self.address_of(&parts.regions, name);
            let placed = parts.placed(&self, &refs, &symbol);
            let offsets = collect_in(parts.regions.iter().map(|it| self.writer.offset(&placed, it)), arena);
            let encoded = self.writer.encode(&placed)?;
            (offsets, self.writer.file(&placed)?, encoded)
        };
        drop(contents);
        for (at, (_, bytes)) in homes.into_iter().zip(synthetic) {
            let space = &mut self.new.spaces[at];
            space.region.size = bytes.len() as u64;
            space.bytes = CowBytes::from_vec(bytes);
        }
        for (space, offset) in self.new.spaces.iter_mut().zip(offsets) {
            space.offset = if space.zero { None } else { offset };
        }
        self.new.encoded = if encoded.is_empty() {
            std::sync::Arc::default()
        } else {
            // alloc: once per stage whose writer encodes slots; the state shares them
            std::sync::Arc::new(slots_in_file(&self.new.spaces, &encoded)?)
        };
        self.new.file_len = file.len;
        // alloc: once per stage; the state shares the writer's parts
        self.new.parts = std::sync::Arc::new(file.parts);
        let message = self.output.live.is_some().then(|| self.message());
        let writes = if self.full { whole(&self.new) } else { self.dirty() };
        let (undo, old_len) = match (self.old.laid_out, self.full) {
            // alloc: none, an empty Vec holds no buffer
            (false, _) => (Vec::new(), 0),
            // alloc: none, an empty Vec holds no buffer
            (true, true) => (Vec::new(), self.old.file_len),
            (true, false) => {
                let len = self.old.file_len;
                let mut undo = Vec::with_capacity(writes.len());
                undo.extend(
                    writes.iter().filter(|(at, _)| *at < len).map(|(at, bytes)| (*at, (bytes.len() as u64).min(len - at))),
                );
                (undo, len)
            }
        };
        Ok(Finished { full: self.full, len: self.new.file_len, writes, undo, old_len, message, state: self.new })
    }

    /// The ranges of the file an incremental link changes, and their bytes.
    fn dirty(&self) -> Vec<(u64, Vec<u8>)> {
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(self.write.len() + self.new.spaces.len());
        for id in self.write.iter() {
            let Some(piece) = self.new.piece(id) else { continue };
            let Some(space) = self.new.spaces.get(piece.region as usize) else { continue };
            let (Some(offset), false, false) = (space.offset, space.zero, piece.zero()) else { continue };
            let at = offset + piece.addr - space.region.addr;
            if piece.what() == What::Cells {
                ranges.extend(self.dirty_cells.iter().map(|slot| (at + u64::from(slot) * 8, 8)));
                continue;
            }
            if piece.size == 0 {
                continue;
            }
            let same_place = !self.fresh.contains(id)
                && self.old.piece(id).is_some_and(|old| {
                    old.addr == piece.addr
                        && old.region == piece.region
                        && self.old.spaces.get(old.region as usize).is_some_and(|it| it.offset == space.offset)
                });
            if same_place && self.old.bytes(id) == self.new.bytes(id) {
                continue;
            }
            ranges.push((at, piece.size));
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
                        changed_blocks(offset, &contents(old), &contents(space), &mut ranges);
                    }
                }
                _ => ranges.push((offset, size.max(old.map_or(0, |it| it.region.size)))),
            }
        }
        if !std::sync::Arc::ptr_eq(&self.old.encoded, &self.new.encoded) {
            changed_slots(&self.old.encoded, &self.new.encoded, self.new.file_len, &mut ranges);
        }
        let mut old_parts = collect_in(self.old.parts.iter().map(|(at, bytes)| (*at, &bytes[..])), self.arena);
        radix::sort_copy_by_key(&mut old_parts, |it| it.0);
        for (offset, bytes) in self.new.parts.iter() {
            match old_parts.binary_search_by_key(offset, |it| it.0) {
                Ok(at) if old_parts[at].1 == &bytes[..] => {}
                Ok(at) => changed_blocks(*offset, old_parts[at].1, bytes, &mut ranges),
                Err(_) => ranges.push((*offset, bytes.len() as u64)),
            }
        }
        fill(&Layout::of(&self.new), &merge(ranges, GAP))
    }
}

/// Pushes each `BLOCK` of `new`, at file offset `offset`, that differs from `old`, the bytes the
/// last link wrote there.
fn changed_blocks(offset: u64, old: &[u8], new: &[u8], ranges: &mut Vec<(u64, u64)>) {
    for start in (0..new.len()).step_by(BLOCK) {
        let end = (start + BLOCK).min(new.len());
        if old.get(start..end) != Some(&new[start..end]) {
            ranges.push((offset + start as u64, (end - start) as u64));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge_across_small_gaps_only() {
        let merged = merge(vec![(100, 8), (0, 10), (8, 4), (5000, 1), (100, 0), (9000, 10)], GAP);
        assert_eq!(merged, [(0, 108), (5000, 4010)]);
        assert_eq!(merge(vec![(0, 10), (10 + GAP + 1, 1)], GAP), [(0, 10), (10 + GAP + 1, 1)]);
        assert_eq!(merge(vec![(0, 10), (10 + GAP, 1)], GAP), [(0, 11 + GAP)]);
    }

    #[test]
    fn a_changed_part_rewrites_only_its_changed_blocks() {
        let old = vec![7u8; 3 * BLOCK];
        let mut new = old.clone();
        new[BLOCK + 5] = 0;
        new.extend_from_slice(&[1, 2, 3]);
        let mut ranges = Vec::new();
        changed_blocks(100, &old, &new, &mut ranges);
        assert_eq!(ranges, [(100 + BLOCK as u64, BLOCK as u64), (100 + 3 * BLOCK as u64, 3)]);
        ranges.clear();
        changed_blocks(0, &new, &old, &mut ranges);
        assert_eq!(ranges, [(BLOCK as u64, BLOCK as u64)], "a shorter part rewrites none past its end");
    }

    #[test]
    fn runs_cut_at_run_boundaries() {
        assert_eq!(runs(&[(10, 20)]), [(10, 20)]);
        assert_eq!(runs(&[(RUN - 4, 8)]), [(RUN - 4, 4), (RUN, 4)]);
        let long = runs(&[(1, 3 * RUN)]);
        assert_eq!(long.len(), 4);
        assert_eq!(long.iter().map(|it| it.1).sum::<u64>(), 3 * RUN);
        assert!(long.windows(2).all(|it| it[0].0 + it[0].1 == it[1].0));
        assert!(long[1..].iter().all(|it| it.0 % RUN == 0));
    }

    /// The layout's copy against a reference that writes every region then every part into
    /// one buffer of the whole file.
    #[test]
    fn layout_reads_what_a_whole_buffer_holds() {
        let regions: Vec<(u64, CowBytes)> = vec![
            (0x1000, CowBytes::from_vec((0..300u32).map(|it| it as u8).collect())),
            (0x1200, CowBytes::from_vec(vec![7; 0x900])),
            (0x4000, CowBytes::from_vec(vec![9; 10])),
        ];
        let parts: Vec<(u64, Vec<u8>)> = vec![(0, vec![1; 64]), (0x1100, vec![2; 4]), (0x5000, vec![3; 3])];
        let layout = Layout {
            spaces: regions.iter().map(|(at, bytes)| (*at, bytes.len() as u64, bytes)).collect(),
            encoded: &[],
            parts: parts.iter().map(|(at, bytes)| (*at, &bytes[..])).collect(),
        };
        let mut reference = vec![0u8; 0x5003];
        for (at, bytes) in &regions {
            let mut all = vec![0; bytes.len()];
            bytes.copy_to(0, &mut all);
            reference[*at as usize..*at as usize + all.len()].copy_from_slice(&all);
        }
        for (at, bytes) in &parts {
            reference[*at as usize..*at as usize + bytes.len()].copy_from_slice(bytes);
        }
        for (at, len) in [(0, 0x5003), (0x10f0, 0x20), (0x1fff, 2), (0x3000, 0x1001), (0x5002, 1), (0x5003, 5)] {
            let mut want = vec![0u8; len];
            let end = (at + len).min(reference.len());
            if at < end {
                want[..end - at].copy_from_slice(&reference[at..end]);
            }
            assert_eq!(layout.bytes(at as u64, len as u64), want, "{at:#x}+{len:#x}");
        }
        let extents = layout.extents(GAP);
        assert_eq!(extents, [(0, 0x1b00), (0x4000, 0x1003)]);
        let mut rebuilt = vec![0u8; 0x5003];
        for (at, bytes) in fill(&layout, &runs(&extents)) {
            rebuilt[at as usize..at as usize + bytes.len()].copy_from_slice(&bytes);
        }
        assert_eq!(rebuilt, reference);
    }
}
