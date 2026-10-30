//! Lazy compilation, per function (docs/v2/plan/design/compile-time.md, change 1;
//! docs/v2/plan/lanes/fastcompile/lazy.md).
//!
//! A dev run or test compiles only the functions that execute. Every function is called
//! through its jump-table slot (blaze-link's stubs); a lazy function's slot starts at its
//! *entry*, a 16-byte stub of this crate's that hands its [`LazyId`] to one shared
//! *resolver* ([`stub`]). The resolver saves every register a call can carry a value in,
//! asks the daemon for the function's code through the run process's hook, restores them and
//! jumps to the code the daemon published in the slot. The next call goes through the slot
//! straight to the code.
//!
//! The daemon side is [`fetch::Fetcher`], a deterministic state machine: it maps a
//! [`LazyId`] to its instance, compiles it once however many threads ask (single flight),
//! places and publishes it, answers every asker, and meanwhile compiles what is likely to run
//! next at lower priority: the callees of what it compiled, and what history says the run
//! reaches. [`runner::Runner`] drives it on threads against the [`runner::Compiler`] and
//! [`runner::Placer`] the daemon gives it (codegen's memo and store first, then the
//! copy-and-patch backend; blaze-link's placement into blaze-inproc's code memory).
//!
//! [`plan::Plan`] says what is compiled ahead instead: what runs before `main` and the entry.
//! Every function's address is its slot's stub, compiled or not, so laziness changes no
//! address the program can see ([`plan`] says why each case is exact).
//!
//! Release builds and `blaze build` outputs are compiled whole: [`mode::allow`] refuses lazy
//! compilation for them, naming the profile or the output.

#![deny(unsafe_code)]

pub mod fetch;
pub mod mode;
pub mod plan;
pub mod runner;
pub mod session;
pub mod stub;
pub mod wire;

#[cfg(test)]
mod tests;

pub use mode::{Purpose, Refused, allow};
pub use plan::Plan;
pub use session::Session;

/// A lazy function's index among its session's lazy functions: what its entry hands the
/// resolver, and the index of every per-function column of the daemon's side.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LazyId(pub u32);

impl LazyId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The most lazy functions one session has: an entry hands its index to the resolver as a
/// sign-extended 32-bit immediate on x86-64, so it stays below 2^31.
pub const MOST_LAZY: u32 = 1 << 31;
