//! Where pieces go.
//!
//! A full link sorts each class's pieces by rank, then link-line order (native sections),
//! then symbol key, so the image is the same whatever the core count or the order changes
//! came in, and lays the classes out in the writer's order with a page boundary at each
//! change of protection. An incremental image keeps room in each region; an incremental
//! link puts new code in [`Class::Patch`] and new data in [`Class::PatchData`], never
//! reusing what an old piece had (a running program may still be in it, and a rollback
//! returns to it), and lays out again in place only the regions whose order the program
//! observes (named sections, init arrays).

use std::sync::Arc;

use rustc_hash::FxHashSet;

use crate::model::{Class, Counts, Region};
use crate::reloc::align_up;
use crate::stage::{Stage, error};
use crate::state::{PieceId, Space, What, zeroed};
use crate::LinkError;

/// The patch regions' least room: what an edit session's new code and data may take.
const PATCH: u64 = 64 << 20;
/// The least room of a growing region of an incremental image.
const ROOM: u64 = 64 << 10;

/// Whether a class grows in place in an incremental link (else its new pieces go to a
/// patch region, or it never grows).
fn grows(class: &Class) -> bool {
    matches!(
        class,
        Class::EhFrame
            | Class::Got
            | Class::Plt
            | Class::Tdata
            | Class::Tbss
            | Class::ThreadVars
            | Class::Named { .. }
            | Class::InitArray
            | Class::FiniArray
            | Class::Synthetic(_)
    )
}

/// Whether the program observes the order of a class's pieces: it is laid out again whole.
fn ordered(class: &Class) -> bool {
    matches!(class, Class::Named { .. } | Class::InitArray | Class::FiniArray)
}

impl Stage<'_> {
    /// The region an incremental link puts a new piece of `class` in.
    fn patch_class(&self, class: &Class) -> Class {
        if self.plan.indexed_code {
            return class.clone();
        }
        match class {
            Class::Text => Class::Patch,
            Class::Rodata | Class::RelRo | Class::Data | Class::Bss => Class::PatchData,
            other => other.clone(),
        }
    }

    pub fn counts(&self) -> Counts {
        let resolved = &self.new.resolved;
        let mut counts = Counts {
            imports: resolved.imports.len() as u64,
            import_names: resolved
                .imports
                .iter()
                .map(|it| it.name.len() as u64 + it.version.as_ref().map_or(0, |v| v.len() as u64) + 2)
                .sum(),
            libraries: resolved.libraries.len() as u64,
            library_names: resolved.libraries.iter().map(|it| it.name.len() as u64 + 1).sum(),
            ..Counts::default()
        };
        for (_, piece) in self.new.live_pieces() {
            counts.absolute +=
                piece.fixups.iter().filter(|it| it.apply == crate::model::Apply::Abs64).count() as u64;
            if !piece.zero {
                counts.image += piece.size;
            }
            match &piece.what {
                What::Got { kind, .. } => {
                    counts.got += kind.cells();
                    counts.absolute += kind.cells();
                }
                What::Function(info) => {
                    counts.functions += 1;
                    if let Some(name) = self.new.debug_names.get(&info.key) {
                        counts.function_names += name.len() as u64 + 1;
                    }
                }
                What::Fde | What::NativeFrame { fde: true, .. } => counts.fdes += 1,
                What::Native { unwind: true, .. } => counts.unwind += piece.size,
                What::Native { .. } => {
                    if matches!(piece.class, Class::Text) {
                        counts.functions += 1;
                    }
                }
                _ => {}
            }
        }
        let exports = self.exports();
        counts.exports = exports.len() as u64;
        counts.export_names = exports.iter().map(|it| it.len() as u64 + 1).sum();
        counts
    }

    /// The names the image exports.
    pub fn exports(&self) -> Vec<String> {
        let mut out = match &self.output.kind {
            crate::OutputKind::Shared { exports, .. } => exports.clone(),
            crate::OutputKind::Executable { .. } => Vec::new(),
        };
        if self.output.live.is_some() {
            out.push(blaze_live::patch::DESCRIPTOR.to_owned());
        }
        out
    }

    pub fn place(&mut self) -> Result<(), LinkError> {
        if self.full { self.lay_out() } else { self.allocate() }
    }

    /// Every piece placed anew.
    fn lay_out(&mut self) -> Result<(), LinkError> {
        let incremental = self.output.incremental;
        let mut by_class: std::collections::BTreeMap<Class, Vec<PieceId>> = Default::default();
        for (id, piece) in self.new.live_pieces() {
            by_class.entry(piece.class.clone()).or_default().push(id);
        }
        let mut sizes: std::collections::BTreeMap<Class, u64> = Default::default();
        for (class, size) in self.writer.synthetic(&self.counts()) {
            sizes.insert(class.clone(), size);
            by_class.entry(class).or_default();
        }
        if incremental && !self.plan.indexed_code {
            let mut more = vec![Class::Patch, Class::PatchData, Class::Got, Class::Plt, Class::EhFrame, Class::Tdata, Class::Tbss];
            if self.plan.tls_descriptors {
                more.push(Class::ThreadVars);
            }
            for class in more {
                by_class.entry(class).or_default();
            }
        }
        let mut classes: Vec<Class> = by_class.keys().cloned().collect();
        self.writer.order(&mut classes);
        let mut spaces = Vec::new();
        let mut addr = self.plan.base + self.plan.headers;
        let mut prot = None;
        let mut text_size = 0;
        let mut data_size = 0;
        for class in classes {
            let mut ids = by_class.remove(&class).unwrap_or_default();
            ids.sort_by(|a, b| {
                let (pa, pb) = (self.new.piece(*a).expect("live"), self.new.piece(*b).expect("live"));
                (pa.rank, pa.order, self.symbols.key(pa.sym)).cmp(&(pb.rank, pb.order, self.symbols.key(pb.sym)))
            });
            let this = self.writer.prot(&class);
            let indexed = self.plan.indexed_code && class == Class::Text;
            if !indexed && prot != Some(this) {
                addr = align_up(addr, self.plan.page);
                prot = Some(this);
            }
            let align = ids
                .iter()
                .map(|id| self.new.piece(*id).expect("live").align)
                .max()
                .unwrap_or(1)
                .max(if matches!(class, Class::Synthetic(_)) { 8 } else { 1 });
            let start = if indexed { 0 } else { align_up(addr, align) };
            let region = spaces.len() as u32;
            let mut size = 0;
            let zero = zeroed(&class);
            for id in &ids {
                let piece = self.new.piece_mut(*id).expect("live");
                size = align_up(size, piece.align);
                piece.addr = start + size;
                piece.region = region;
                piece.zero = zero;
                size += piece.size;
            }
            match &class {
                Class::Text => text_size += size,
                Class::Rodata | Class::RelRo | Class::Data | Class::Bss => data_size += size,
                _ => {}
            }
            let bound = sizes.get(&class).copied();
            let capacity = match (&class, incremental) {
                (Class::Patch, true) => align_up(text_size.max(PATCH), self.plan.page),
                (Class::PatchData, true) => align_up(data_size.max(PATCH), self.plan.page),
                (Class::Synthetic(_), true) => {
                    let bound = bound.unwrap_or(size);
                    align_up((bound * 2).max(bound + ROOM), 16)
                }
                (Class::Synthetic(_), false) => bound.unwrap_or(size),
                (class, true) if grows(class) => align_up((size * 2).max(size + ROOM), 16),
                _ => size,
            };
            spaces.push(Space {
                region: Region { class: class.clone(), addr: start, capacity, size, prot: this, align },
                bytes: Arc::new(if zero || matches!(class, Class::Synthetic(_)) {
                    Vec::new()
                } else {
                    vec![0; size as usize]
                }),
                zero,
                offset: None,
            });
            if matches!(class, Class::Synthetic(_)) {
                spaces.last_mut().expect("just pushed").region.size = 0;
            }
            if !indexed {
                addr = start + capacity;
            }
            self.write.extend(ids);
        }
        self.new.spaces = spaces;
        self.new.laid_out = true;
        self.tls();
        Ok(())
    }

    /// New pieces placed where an incremental image has room.
    fn allocate(&mut self) -> Result<(), LinkError> {
        for (class, bound) in self.writer.synthetic(&self.counts()) {
            let space = self.new.space_of(&class).ok_or_else(|| {
                error(None, format!("the image has no {class:?} region: link it from scratch"))
            })?;
            let capacity = self.new.spaces[space].region.capacity;
            if bound > capacity {
                return Err(error(
                    None,
                    format!("{class:?} may need {bound} bytes and has room for {capacity}: link the image from scratch"),
                ));
            }
        }
        let removed: FxHashSet<PieceId> = self.removed.iter().map(|it| it.0).collect();
        let mut relay: FxHashSet<usize> = FxHashSet::default();
        for (id, piece) in &self.removed {
            if self.old.piece(*id).is_some() && ordered(&piece.class) {
                relay.insert(piece.region as usize);
            }
        }
        let mut fresh: Vec<PieceId> = self
            .fresh
            .iter()
            .copied()
            .filter(|it| !removed.contains(it) && self.new.piece(*it).is_some_and(|p| p.region == u32::MAX))
            .collect();
        fresh.sort_by(|a, b| {
            let (pa, pb) = (self.new.piece(*a).expect("live"), self.new.piece(*b).expect("live"));
            (&pa.class, pa.rank, pa.order, self.symbols.key(pa.sym)).cmp(&(&pb.class, pb.rank, pb.order, self.symbols.key(pb.sym)))
        });
        for id in fresh {
            let class = self.new.piece(id).expect("live").class.clone();
            let target = self.patch_class(&class);
            let space = self.new.space_of(&target).ok_or_else(|| {
                error(
                    None,
                    format!(
                        "{} needs a {target:?} region, which the image was laid out without: link it from scratch",
                        crate::stage::describe(self.symbols.key(self.new.piece(id).expect("live").sym))
                    ),
                )
            })?;
            if ordered(&class) {
                relay.insert(space);
                let piece = self.new.piece_mut(id).expect("live");
                piece.region = space as u32;
                continue;
            }
            let region = &self.new.spaces[space].region;
            let zero = self.new.spaces[space].zero;
            let piece = self.new.piece(id).expect("live");
            let at = align_up(region.addr + region.size, piece.align) - region.addr;
            let end = at + piece.size;
            if end > region.capacity {
                return Err(error(
                    crate::stage::function_of(piece),
                    format!(
                        "{} does not fit in {target:?} ({} of {} bytes used): link the image from scratch",
                        crate::stage::describe(self.symbols.key(piece.sym)),
                        region.size,
                        region.capacity
                    ),
                ));
            }
            let addr = region.addr + at;
            self.new.spaces[space].region.size = end;
            let piece = self.new.piece_mut(id).expect("live");
            piece.addr = addr;
            piece.region = space as u32;
            piece.zero = zero;
        }
        let mut relay: Vec<usize> = relay.into_iter().collect();
        relay.sort_unstable();
        for space in relay {
            self.relay(space)?;
        }
        self.tls();
        Ok(())
    }

    /// Lays out an ordered region again in place: pieces that move are written again, and
    /// what refers to them.
    fn relay(&mut self, space: usize) -> Result<(), LinkError> {
        let class = self.new.spaces[space].region.class.clone();
        let mut ids: Vec<PieceId> = self
            .new
            .live_pieces()
            .filter(|(_, it)| it.region as usize == space)
            .map(|(id, _)| id)
            .collect();
        ids.sort_by(|a, b| {
            let (pa, pb) = (self.new.piece(*a).expect("live"), self.new.piece(*b).expect("live"));
            (pa.rank, pa.order, self.symbols.key(pa.sym)).cmp(&(pb.rank, pb.order, self.symbols.key(pb.sym)))
        });
        let start = self.new.spaces[space].region.addr;
        let capacity = self.new.spaces[space].region.capacity;
        let zero = self.new.spaces[space].zero;
        let mut size = 0;
        for id in ids {
            let piece = self.new.piece(id).expect("live");
            size = align_up(start + size, piece.align) - start;
            let addr = start + size;
            size += piece.size;
            if size > capacity {
                return Err(error(
                    None,
                    format!("{class:?} needs {size} bytes and has room for {capacity}: link the image from scratch"),
                ));
            }
            if piece.addr != addr || self.fresh.contains(&id) {
                let moved: Vec<_> = std::iter::once(piece.sym).chain(piece.defines.iter().copied()).collect();
                self.changed.extend(moved);
                if !self.bytes.contains_key(&id) && !self.fresh.contains(&id) {
                    let bytes = self.old.piece(id).map(|old| self.old.bytes(old).to_vec()).unwrap_or_default();
                    self.bytes.insert(id, std::borrow::Cow::Owned(bytes));
                }
                self.write.insert(id);
            }
            let piece = self.new.piece_mut(id).expect("live");
            piece.addr = addr;
            piece.zero = zero;
        }
        let old = self.new.spaces[space].region.size;
        self.new.spaces[space].region.size = size;
        if size < old && !zero {
            let bytes = Arc::make_mut(&mut self.new.spaces[space].bytes);
            bytes.truncate(size as usize);
        }
        Ok(())
    }

    /// Where the TLS template is: from the first thread-local region to the end of the
    /// last one's room, so that offsets stay as they are while it fills.
    fn tls(&mut self) {
        let tls: Vec<&Region> = self
            .new
            .spaces
            .iter()
            .map(|it| &it.region)
            .filter(|it| matches!(it.class, Class::Tdata | Class::Tbss))
            .collect();
        let (Some(first), Some(last)) = (tls.first(), tls.last()) else {
            self.new.tls = None;
            return;
        };
        let init = tls.iter().find(|it| it.class == Class::Tdata).map_or(0, |it| it.size);
        let align = tls.iter().map(|it| it.align).max().unwrap_or(1);
        self.new.tls = Some(crate::model::Tls {
            addr: first.addr,
            init,
            size: last.addr + last.capacity.max(last.size) - first.addr,
            align,
        });
    }

}
