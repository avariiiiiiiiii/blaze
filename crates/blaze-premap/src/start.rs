//! What a dev run does before its entry, so that the image and the workspace link behave as
//! one whole link: [`plan`].
//!
//! - **Bindings.** Each of the image's open cells ([`crate::prelink::Bind`]) takes what a
//!   whole link resolves its name to: a strong definition beats a weak one, of two weak ones
//!   the first on the link line wins, two strong ones are the whole link's error. Candidates
//!   are the workspace's definition and the image's own; with neither, the shared libraries
//!   in link-line order, looked up in the run process ([`Lookup`]), 0 for a weak reference.
//! - **Libraries** load in link-line order, each once: the image's and the workspace's lists
//!   merged by their place on the line. Each list holds what its link uses (an `as_needed`
//!   library neither uses is in neither), so the union is the whole link's list.
//! - **Thread-locals.** The run's loader keeps one area aside in every thread's static block
//!   ([`TlsReserve`]); a program's thread-locals are at `bias + offset` in it. The image's
//!   part is at offset 0, so the offsets its link computed alone hold in every run; the
//!   workspace's part follows it ([`workspace_tls_offset`]). On Mach-O the same offsets are
//!   into the one template the loader's descriptors point at.
//! - **Init and fini** pieces of both, merged in the whole link's order (rank, link-line
//!   order, tie), each piece's pointers in address order; fini runs last to first.

use blaze_inproc::TlsReserve;

use crate::PremapError;
use crate::image::View;
use crate::prelink::{BindKind, InitEntry, Library, Tls};
use crate::table::{Def, Kind, Strength, SymbolId};

/// One part of the program's thread-local area: the template's bytes at `template`, `init`
/// initialized and zeroes to `size`, copied to `offset` in the area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TlsPart {
    pub template: u64,
    pub init: u64,
    pub size: u64,
    pub offset: u64,
}

/// Where the workspace's thread-locals start in the area, after the image's. The workspace
/// link places them there (`v2/requests/fastcompile.md`, fastcompile-premap 2) and says where
/// it put them ([`WorkspaceLink::tls_offset`]); [`plan`] refuses a difference.
pub fn workspace_tls_offset(image: Option<&Tls>, align: u64) -> u64 {
    image.map_or(0, |it| it.size.next_multiple_of(align.max(1)))
}

/// The parts of the area, the image's first, checked against what the loader keeps aside.
pub fn tls_parts(
    reserve: &TlsReserve,
    image: Option<&Tls>,
    workspace: Option<(&Tls, u64)>,
) -> Result<Vec<TlsPart>, PremapError> {
    let mut parts = Vec::with_capacity(2);
    if let Some(tls) = image {
        parts.push((tls, 0));
    }
    if let Some((tls, offset)) = workspace {
        parts.push((tls, offset));
    }
    let mut out = Vec::with_capacity(parts.len());
    for (tls, offset) in parts {
        let end = offset.checked_add(tls.size);
        if tls.align > reserve.align || !offset.is_multiple_of(tls.align) || end.is_none_or(|end| end > reserve.size) {
            return Err(PremapError::new(format!(
                "the run's loader keeps {} bytes aligned to {} for thread-locals, and they need {} bytes \
                 aligned to {} at {offset}",
                reserve.size, reserve.align, tls.size, tls.align
            )));
        }
        out.push(TlsPart { template: tls.template, init: tls.init, size: tls.size, offset });
    }
    Ok(out)
}

/// What the workspace link made, as the run's start needs it (blaze-link's layout of the
/// workspace image, `v2/requests/fastcompile.md`, fastcompile-premap 1).
#[derive(Clone, Copy, Debug)]
pub struct WorkspaceLink<'a> {
    /// The global names it defines, sorted by name, each once.
    pub names: &'a [(String, Def)],
    pub init: &'a [InitEntry],
    pub fini: &'a [InitEntry],
    pub tls: Option<Tls>,
    /// Where its link put its thread-locals in the run's area.
    pub tls_offset: Option<u64>,
    /// Mach-O: its thread-local descriptors (`__thread_vars`), as address and length.
    pub thread_vars: &'a [(u64, u64)],
    pub libraries: &'a [Library],
    pub unwind: &'a [(u64, u64)],
}

/// A cell the run process binds itself: a name only a shared library defines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lookup<'a> {
    pub place: u64,
    pub name: &'a str,
    pub addend: i64,
    /// 0 when no library defines it; otherwise the run stops, naming it.
    pub weak: bool,
}

/// What the run process does, in this order, before its entry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Start<'a> {
    /// Loaded with every symbol global, in this order.
    pub libraries: Vec<&'a str>,
    /// 8-byte cells to write: `(address, value)`.
    pub writes: Vec<(u64, u64)>,
    pub lookups: Vec<Lookup<'a>>,
    /// The program's thread-local area, the image's part first.
    pub tls: Vec<TlsPart>,
    /// Mach-O: thread-local descriptors the loader fills (its thunk, a key, the offset kept).
    pub thread_vars: Vec<(u64, u64)>,
    /// Call frame information to register: address and length.
    pub unwind: Vec<(u64, u64)>,
    /// Cells holding the functions to call, in order, with `argc`, `argv` and `envp`.
    pub init: Vec<u64>,
    /// Cells holding the functions to call at exit, last to first.
    pub fini: Vec<u64>,
}

/// The run's start for `image` mapped beside `workspace`, in a loader that keeps `reserve`
/// for thread-locals.
pub fn plan<'a>(
    image: &View<'a>,
    workspace: &WorkspaceLink<'a>,
    reserve: &TlsReserve,
) -> Result<Start<'a>, PremapError> {
    if !workspace.names.is_sorted_by(|a, b| a.0 < b.0) {
        return Err(PremapError::new("the workspace link's names are not sorted, each once".to_owned()));
    }
    let image_tls = image.tls();

    let mut writes = Vec::new();
    let mut lookups = Vec::new();
    for bind in image.binds() {
        let theirs = workspace
            .names
            .binary_search_by(|it| it.0.as_str().cmp(bind.name))
            .ok()
            .map(|at| workspace.names[at].1);
        let own = image.get(&SymbolId::name(bind.name));
        let def = match (theirs, own) {
            (Some(a), Some(b)) if a.strength == Strength::Strong && b.strength == Strength::Strong => {
                return Err(PremapError::new(format!(
                    "`{}` is defined in the workspace and in a dependency, both strongly",
                    bind.name
                )));
            }
            (Some(a), Some(b)) => Some(match (a.strength, b.strength) {
                (Strength::Strong, _) => a,
                (_, Strength::Strong) => b,
                _ if b.order < a.order => b,
                _ => a,
            }),
            (one, other) => one.or(other),
        };
        match (def, bind.kind) {
            (Some(def), BindKind::Addr) if def.kind != Kind::ThreadLocal => {
                writes.push((bind.place, def.value.wrapping_add_signed(bind.addend)));
            }
            (Some(def), BindKind::TpOff) if def.kind == Kind::ThreadLocal => {
                writes.push((bind.place, def.value.wrapping_add_signed(bind.addend)));
            }
            (Some(_), _) => {
                return Err(PremapError::new(format!(
                    "`{}` is bound as {} and defined as {}",
                    bind.name,
                    if bind.kind == BindKind::TpOff { "a thread-local" } else { "an address" },
                    if bind.kind == BindKind::TpOff { "an address" } else { "a thread-local" },
                )));
            }
            (None, BindKind::TpOff) => {
                return Err(PremapError::new(format!(
                    "the dependency image reaches the thread-local `{}` of a shared library at a fixed \
                     offset, and a library the run loads has no fixed thread-local offset",
                    bind.name
                )));
            }
            (None, BindKind::Addr) => {
                lookups.push(Lookup { place: bind.place, name: bind.name, addend: bind.addend, weak: bind.weak });
            }
        }
    }

    let mut libraries: Vec<(u32, &'a str)> = image.libraries().map(|it| (it.order, it.name)).collect();
    libraries.extend(workspace.libraries.iter().map(|it| (it.order, it.name.as_str())));
    libraries.sort_by_key(|it| it.0);
    let mut seen = rustc_hash::FxHashSet::default();
    let libraries = libraries.into_iter().filter(|it| seen.insert(it.1)).map(|it| it.1).collect();

    let tls = match (workspace.tls, workspace.tls_offset) {
        (Some(tls), Some(offset)) => {
            let expected = workspace_tls_offset(image_tls.as_ref(), tls.align);
            if offset != expected {
                return Err(PremapError::new(format!(
                    "the workspace link put its thread-locals at {offset} in the run's area, and after \
                     the dependency image's they start at {expected}"
                )));
            }
            tls_parts(reserve, image_tls.as_ref(), Some((&tls, offset)))?
        }
        (None, None) => tls_parts(reserve, image_tls.as_ref(), None)?,
        _ => {
            return Err(PremapError::new(
                "the workspace link has a thread-local template and no place for it, or a place and none".to_owned(),
            ));
        }
    };

    let image_init: Vec<InitEntry> = image.init().collect();
    let image_fini: Vec<InitEntry> = image.fini().collect();
    let init = merged(&image_init, workspace.init)?;
    let fini = merged(&image_fini, workspace.fini)?;
    let mut unwind: Vec<(u64, u64)> = image.unwind().collect();
    unwind.extend_from_slice(workspace.unwind);
    let mut thread_vars: Vec<(u64, u64)> = image.thread_vars().collect();
    thread_vars.extend_from_slice(workspace.thread_vars);
    Ok(Start { libraries, writes, lookups, tls, thread_vars, unwind, init, fini })
}

/// The cells of both sides' pieces in a whole link's order.
fn merged(image: &[InitEntry], workspace: &[InitEntry]) -> Result<Vec<u64>, PremapError> {
    let mut all: Vec<&InitEntry> = image.iter().chain(workspace).collect();
    all.sort_by_key(|it| it.sort_key());
    if let Some(pair) = all.windows(2).find(|it| it[0].sort_key() == it[1].sort_key()) {
        return Err(PremapError::new(format!(
            "two init pieces, at {:#x} and {:#x}, have one place in a whole link's order",
            pair[0].at, pair[1].at
        )));
    }
    let mut cells = Vec::with_capacity(all.iter().map(|it| it.count as usize).sum());
    for entry in all {
        cells.extend((0..u64::from(entry.count)).map(|n| entry.at + n * 8));
    }
    Ok(cells)
}
