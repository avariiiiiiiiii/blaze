//! Tier-up from tier 0 (stencil.md, "Tier-up"; compile-time.md, change 13). Tier 0 code
//! counts into its function's two counter cells, reached through one relocation
//! ([`crate::code::Name::Counter`], `Target::Counter` of its instance): the prologue adds one
//! to [`CALLS`], each back edge one to [`BACK_EDGES`]. The adds are plain read-modify-writes:
//! a count lost to a race is fine for a heuristic, and an aligned 8-byte store never tears.
//!
//! [`Tiering`] is a deterministic state machine over per-function columns: given snapshots of
//! the cells it says which functions to recompile at which tier, and [`Recompile`] is the
//! daemon's hook that does it (compile the unit at `spec.at(backend)`, place it, publish its
//! slot, retire the old code behind blaze-inproc's fence).

use blaze_codegen::Backend;

/// Bytes of one function's cells: two `u64`s, 8-aligned.
pub const CELLS: u32 = 16;
/// The addend of the calls cell from the cells' address.
pub const CALLS: i64 = 0;
/// The addend of the back-edges cell.
pub const BACK_EDGES: i64 = 8;

/// The tiers of a dev or test run's function.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Tier {
    /// Copy-and-patch: every function's first code.
    Stencil,
    /// Cranelift: warm functions.
    Cranelift,
    /// LLVM at full optimization: hot functions.
    Llvm,
}

impl Tier {
    /// The backend that compiles it; `None` for tier 0, which is this crate's.
    pub fn backend(self) -> Option<Backend> {
        match self {
            Tier::Stencil => None,
            Tier::Cranelift => Some(Backend::Cranelift),
            Tier::Llvm => Some(Backend::Llvm { opt: 3 }),
        }
    }
}

/// When a function moves up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    /// Weighted count at which tier 0 code is recompiled with Cranelift.
    pub warm: u64,
    /// Weighted count at which Cranelift code is recompiled with LLVM.
    pub hot: u64,
    /// What one back edge weighs against one call.
    pub back_edge_weight: u64,
}

impl Default for Policy {
    fn default() -> Policy {
        unimplemented!("fastcompile-stencil-tier: thresholds, argued in stencil.md")
    }
}

/// One snapshot of a function's cells.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Counts {
    pub calls: u64,
    pub back_edges: u64,
}

/// A recompile [`Tiering`] asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Promote {
    /// The function's index among the run's tiered functions.
    pub function: u32,
    pub to: Tier,
}

/// The tiering state of one run: one column entry per function.
pub struct Tiering {
    policy: Policy,
}

impl Tiering {
    /// `functions` functions, all at [`Tier::Stencil`].
    pub fn new(functions: u32, policy: Policy) -> Tiering {
        let _ = functions;
        let _ = policy;
        unimplemented!("fastcompile-stencil-tier")
    }

    pub fn tier(&self, function: u32) -> Tier {
        let _ = (function, &self.policy);
        unimplemented!("fastcompile-stencil-tier")
    }

    /// Takes a snapshot of `function`'s cells; pushes to `out` the recompile it now earns,
    /// if any and if none is in flight for it.
    pub fn observe(&mut self, function: u32, counts: Counts, out: &mut Vec<Promote>) {
        let _ = (function, counts, out);
        unimplemented!("fastcompile-stencil-tier")
    }

    /// The recompile of `function` at `tier` is published: it is at `tier` now.
    pub fn landed(&mut self, function: u32, tier: Tier) {
        let _ = (function, tier);
        unimplemented!("fastcompile-stencil-tier")
    }

    /// The recompile of `function` at `tier` failed: it stays where it is and is not asked
    /// for again in this run (the failure is reported by the caller, never hidden).
    pub fn failed(&mut self, function: u32, tier: Tier) {
        let _ = (function, tier);
        unimplemented!("fastcompile-stencil-tier")
    }
}

/// The daemon's side of a promotion.
pub trait Recompile: Sync {
    /// Compiles `function` with `backend` and publishes it in its slot.
    fn recompile(&self, function: u32, backend: Backend) -> Result<(), String>;
}
