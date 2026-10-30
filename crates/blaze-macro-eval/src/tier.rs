//! The compiled tier of proc macros (docs/v2/plan/design/compile-time.md, change 20; the design
//! is docs/v2/plan/lanes/fastcompile/macros-tier.md): a macro crate's code compiled by
//! copy-and-patch from the same MIR the evaluator runs, through hir-ty-codegen's Cranelift IR,
//! and run in blaze's process, isolated as a wasm engine isolates a module.
//!
//! - **Memory** ([`region`]): one 4 GiB reservation aligned to 4 GiB with guard pages; a guest
//!   address is an offset into it, and every load and store of the confined code is at
//!   `base + (address + offset) mod 2^32`. The macro's statics, shadow stack and heap
//!   ([`heap`]) live in it; the machine's stack, the code and the call table don't.
//! - **Indirect calls** ([`table`]): a function pointer is an offset into the image's table,
//!   and each call through one checks it and the entry's signature in the code.
//! - **Externals** ([`bind`]): every name outside the macro's code is bound to a shim (the same
//!   claims as the evaluator's run) or refuses the crate when it is compiled, naming the
//!   symbol.
//! - **Shims** reach the expansion's memory through [`memory::Memory`], the same [`Guest`]
//!   the evaluator's memory is, so `os.rs` and the server serve both tiers.
//!
//! **Running.** Compiled code reads and writes its region only while the runner holds the
//! region by `&mut`, on the thread that runs the expansion; a shim it calls gets the memory
//! back by `&mut` for the call. So every `&[u8]` the region lends is never written under it.
//!
//! **Which tier** ([`choose`]): a policy, never a failure. In a blaze that runs the compiled
//! tier ([`Policy::compiled`]), a macro crate runs compiled once it has run [`Policy::warm`]
//! times in the evaluator; one expansion runs wholly in one tier,
//! and a crate the policy picks that can't be compiled is an error naming the macro and what
//! it met, never a quiet stay in the evaluator.
//!
//! [`Guest`]: crate::guest::Guest

use blaze_ids::ContentId;

pub(crate) mod bind;
pub(crate) mod confine;
pub(crate) mod heap;
pub(crate) mod memory;
pub(crate) mod region;
pub(crate) mod table;

#[cfg(test)]
mod tests;

/// Which tier runs one expansion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tier {
    /// The MIR evaluator, every access checked.
    Evaluator,
    /// The macro crate's compiled image ([`run`]).
    Compiled,
}

/// When a macro crate runs compiled.
///
/// The threshold is the ski-rental rule stencil's `Tiering` uses: compile once what the
/// evaluator's slowness has cost equals what the compile costs. A derive of serde's size
/// reaches about 8,000 functions, which cost about 15 µs each to build IR for, confine, copy
/// and patch and load (120 ms of CPU, spread over the workers), once per version of the macro:
/// the image is kept in the store by code hash, and a crates.io macro's comes from the global
/// cache. The evaluator runs the same expansion one to two orders of magnitude slower than the
/// compiled code, 50 to 500 ms against 2 to 5 ms (macros-tier.md, "Speed of light"), so one
/// evaluated expansion has already cost what the compile does. A small macro (no syn, a few
/// hundred functions) compiles in a few milliseconds, so compiling it after one run wastes at
/// most that. Hence one run: the first expansion of a crate runs in the evaluator, which
/// checks every access on a real input, and the rest run compiled.
///
/// The policy never picks a tier this blaze can't run: [`Policy::compiled`] says whether it
/// has the compiled tier's confining pass, loader and runner.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Policy {
    /// Runs in the evaluator before a crate runs compiled.
    pub(crate) warm: u64,
    /// Whether this blaze runs the compiled tier. It doesn't yet: the loader and the runner
    /// (machine stack, shim stubs, unwinder, fault handler) are not written, so every
    /// expansion runs in the evaluator.
    pub(crate) compiled: bool,
}

impl Policy {
    pub(crate) const DEFAULT: Policy = Policy { warm: 1, compiled: false };

    /// The decision is the same for every code hash: by its runs alone.
    pub(crate) fn choose(&self, _code: ContentId, runs: u64) -> Tier {
        if self.compiled && runs >= self.warm { Tier::Compiled } else { Tier::Evaluator }
    }
}

/// The tier of the next expansion of the macro crate with code hash `code`, which has run
/// `runs` times before.
pub(crate) fn choose(code: ContentId, runs: u64) -> Tier {
    Policy::DEFAULT.choose(code, runs)
}

/// Runs `expansions` on the compiled image of the macro crate with code hash `code`, as
/// `run.rs`'s `evaluate` runs them on the evaluator: in order, each with its own server,
/// starting from the statics `preset`, giving what the last gave and what all of them did.
///
/// The image is made first: the crate's reached functions lowered to Cranelift IR, confined,
/// compiled by copy-and-patch and loaded. hir-ty-codegen hands out the IR and [`confine`]
/// confines it, but nothing loads the image or runs it on its own machine stack yet, so this
/// stops there, saying so. The policy never sends an expansion here while
/// [`Policy::compiled`] is false.
pub(crate) fn run(
    _db: &base_db::Db,
    _code: ContentId,
    _env: &base_db::Env,
    _preset: &[(hir_def::StaticId, Vec<u8>)],
    expansions: &[crate::run::Expansion<'_>],
) -> Result<crate::run::Ran, crate::guest::Stop> {
    if expansions.is_empty() {
        // alloc: error path
        return Err(crate::guest::Stop("was given nothing to expand".to_owned()));
    }
    const NO_RUNNER: &str = "can't run on the compiled tier: blaze has no loader for a confined image \
                             and no runner for its code yet";
    // alloc: error path
    Err(crate::guest::Stop(NO_RUNNER.to_owned()))
}
