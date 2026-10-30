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
//! - **Thread-locals.** One static block: the image's part first (nearest the thread pointer,
//!   so the offsets its link computed alone hold), the workspace's after ([`TlsBlock`]).
//! - **Init and fini** pieces of both, merged in the whole link's order (rank, link-line
//!   order, tie), each piece's pointers in address order; fini runs last to first.

use blaze_codegen::Arch;
use blaze_link::Format;

use crate::PremapError;
use crate::image::View;
use crate::prelink::{BindKind, InitEntry, Library, Tls};
use crate::table::{Def, Kind, Strength, SymbolId};

/// How a target lays out the static thread-local block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    /// Below the thread pointer, the first module's block nearest it (x86-64 ELF, variant II).
    Below,
    /// Above the thread pointer after a control block of `tcb` bytes, the first module's
    /// block first (AArch64 ELF, variant I, 16 bytes); Mach-O's TLV and PE's `.tls`, whose
    /// offsets are from the start of the template (0).
    Above { tcb: u64 },
}

pub fn variant(format: Format, arch: Arch) -> Result<Variant, PremapError> {
    match (format, arch) {
        (Format::Elf, Arch::X86_64) => Ok(Variant::Below),
        (Format::Elf, Arch::Aarch64) => Ok(Variant::Above { tcb: 16 }),
        (Format::MachO | Format::Pe, Arch::X86_64 | Arch::Aarch64) => Ok(Variant::Above { tcb: 0 }),
        (format, arch) => Err(PremapError::new(format!("no dependency image for {format:?} on {arch:?}"))),
    }
}

/// One part of the block: the template's bytes at `template`, `init` initialized and zeroes
/// to `size`, copied to `offset` from the thread pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TlsPart {
    pub template: u64,
    pub init: u64,
    pub size: u64,
    pub offset: i64,
}

/// The run's static thread-local block.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TlsBlock {
    pub variant: Variant,
    /// The image's part first.
    pub parts: Vec<TlsPart>,
    /// The bytes from the lowest part's start to the highest part's end.
    pub size: u64,
    /// The thread pointer's alignment.
    pub align: u64,
}

/// Where the image's thread-locals start from the thread pointer: as its link computed them,
/// alone, as the first module.
pub fn image_tls_start(variant: Variant, image: &Tls) -> i64 {
    match variant {
        Variant::Below => -(image.size.next_multiple_of(image.align) as i64),
        Variant::Above { tcb } => tcb.next_multiple_of(image.align) as i64,
    }
}

/// Where the workspace's thread-locals start from the thread pointer, after the image's.
/// The workspace link computes the same (`v2/requests/fastcompile.md`, fastcompile-premap 2)
/// and says what it got ([`WorkspaceLink::tls_start`]); [`plan`] refuses a difference.
pub fn workspace_tls_start(variant: Variant, image: Option<&Tls>, size: u64, align: u64) -> i64 {
    match (variant, image) {
        (Variant::Below, None) => -(size.next_multiple_of(align) as i64),
        (Variant::Below, Some(image)) => {
            let end = image_tls_start(variant, image);
            let start = end - size as i64;
            start - start.rem_euclid(align as i64)
        }
        (Variant::Above { tcb }, None) => tcb.next_multiple_of(align) as i64,
        (Variant::Above { .. }, Some(image)) => {
            let end = image_tls_start(variant, image) as u64 + image.size;
            end.next_multiple_of(align) as i64
        }
    }
}

pub fn tls_block(variant: Variant, image: Option<&Tls>, workspace: Option<(&Tls, i64)>) -> TlsBlock {
    let mut parts = Vec::with_capacity(2);
    let mut align = 1;
    if let Some(image) = image {
        let offset = image_tls_start(variant, image);
        parts.push(TlsPart { template: image.template, init: image.init, size: image.size, offset });
        align = align.max(image.align);
    }
    if let Some((tls, offset)) = workspace {
        parts.push(TlsPart { template: tls.template, init: tls.init, size: tls.size, offset });
        align = align.max(tls.align);
    }
    let low = parts.iter().map(|it| it.offset).min().unwrap_or(0);
    let high = parts.iter().map(|it| it.offset + it.size as i64).max().unwrap_or(0);
    TlsBlock { variant, parts, size: (high - low) as u64, align }
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
    /// Where its link put its thread-locals from the thread pointer.
    pub tls_start: Option<i64>,
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
    pub tls: TlsBlock,
    /// Call frame information to register: address and length.
    pub unwind: Vec<(u64, u64)>,
    /// Cells holding the functions to call, in order, with `argc`, `argv` and `envp`.
    pub init: Vec<u64>,
    /// Cells holding the functions to call at exit, last to first.
    pub fini: Vec<u64>,
}

/// The run's start for `image` mapped beside `workspace`.
pub fn plan<'a>(image: &View<'a>, workspace: &WorkspaceLink<'a>) -> Result<Start<'a>, PremapError> {
    let meta = image.meta();
    let variant = variant(meta.format, meta.arch)?;
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

    let tls = match (workspace.tls, workspace.tls_start) {
        (Some(tls), Some(start)) => {
            let expected = workspace_tls_start(variant, image_tls.as_ref(), tls.size, tls.align);
            if start != expected {
                return Err(PremapError::new(format!(
                    "the workspace link put its thread-locals at {start} from the thread pointer, and \
                     after the dependency image's they start at {expected}"
                )));
            }
            tls_block(variant, image_tls.as_ref(), Some((&tls, start)))
        }
        (None, None) => tls_block(variant, image_tls.as_ref(), None),
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
    Ok(Start { libraries, writes, lookups, tls, unwind, init, fini })
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
