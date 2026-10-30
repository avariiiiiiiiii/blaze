//! The backend's tests (stencil.md, "Tests"): each stencil against `cranelift-interpreter`,
//! whole functions against Cranelift's code of them, the CLIF corpus, the library's
//! invariants, the encodings, unwinding through stencil code, and compile speed.
//!
//! What runs code (each stencil alone in a harness frame, whole functions, unwinding) runs on
//! the machine's own architecture under Unix ([`exec`]). Everything else, compiling for every
//! target the IR builder makes code for included, runs everywhere.

mod rng;
mod host;
mod semantics;
mod interp;

mod coverage;
mod enc;
mod library;
mod tier;

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod exec;
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod stencils;
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod funcs;
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod corpus;
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod unwind;
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod speed;
