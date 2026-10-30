//! The program `blaze`: blaze's cargo, its client and its daemon ([`blaze_cargo::cli`]), and
//! what runs before it ([`main`]): the sandbox launcher, the stand-ins a build script runs, and
//! the release of blaze a workspace pins (`pin.rs`, `versions.rs`).

#![allow(clippy::print_stderr, reason = "a command line program: stderr is where it speaks")]
#![deny(unsafe_code)]

mod as_rustc;
mod pin;
mod program;
mod versions;

pub use program::main;

/// What the general allocator still serves (startup, configuration, rare large one-offs; the
/// bulk of a build comes from `blaze-mem`, which gives its pages back itself) wants the fastest
/// small-object allocator, mimalloc. Counted, so that the plane's `arena::measure` and
/// admission's `arena::held` see its bytes.
#[cfg(all(not(test), not(all(unix, feature = "heap-profile"))))]
#[global_allocator]
static ALLOC: blaze_graph::arena::Counting<mimalloc::MiMalloc> =
    blaze_graph::arena::Counting(mimalloc::MiMalloc);

/// `heap-profile` profiles jemalloc's heap, so jemalloc serves under it. Not on Windows.
#[cfg(all(unix, feature = "heap-profile", not(test)))]
#[global_allocator]
static ALLOC: blaze_graph::arena::Counting<tikv_jemallocator::Jemalloc> =
    blaze_graph::arena::Counting(tikv_jemallocator::Jemalloc);

/// The tests hold the hot paths to allocation budgets.
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
