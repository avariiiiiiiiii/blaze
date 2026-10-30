//! Compile errors and warnings as rustc gives them (docs/v2/README.md section 18; #1522,
//! #1440, #780): computed from the front end's queries that found them, each with the facts
//! it read, and written as rustc and cargo write them. Every error rustc gives is given: a
//! program rustc rejects that blaze accepts is a bug of this crate or of the front end.
//!
//! - [`model`]: a diagnostic as data, rustc's `DiagInner`, with its facts.
//! - [`cx`]: what one is lowered with: spans as rustc gives them, files, facts.
//! - `lower`: each `hir::AnyDiagnostic` kind in rustc's words, one module per kind.
//! - `checks`: what rustc checks that the front end reports nothing of.
//! - [`lints`]: rustc's lints, their levels and the notes on where a level came from.
//! - [`print`]: rustc's printing of types and paths.
//! - [`emit`]: rustc's text (human, short, colour) and JSON, and its closing lines.
//! - [`check`]: a crate's diagnostics in the order rustc gives them, stopping where it stops.
//! - [`api`]: a diagnostic as the agent API sends it.

pub mod api;
pub mod check;
mod checks;
pub mod cx;
pub mod emit;
pub mod lints;
mod lower;
pub mod model;
pub mod print;
#[cfg(test)]
mod tests;

pub use check::{Options, check_crate};
pub use cx::{Cx, Files};
pub use lower::{Found, Phase};
pub use model::{Code, Diag, Level, Span};
