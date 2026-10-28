//! The words of a checker's socket, which the daemon asks; the checker is the engine's
//! (`crates/blaze-engine/src/check.rs`).

/// What a checker answers when the workspace changed in a way only loading it again takes
/// in: a source file came or went, a manifest changed, or the source of a build script or a
/// proc macro did, which run as they were built.
pub const RELOAD: &str = "reload";

/// What a lint request starts with, before its [`blaze_lints::serve::Ask`]; its answer starts
/// with it too, before the [`blaze_lints::serve::Answer`].
pub const LINT: &str = "lint\t";

/// The line a checker is asked with: the flags of the check that pick targets, then
/// `request`.
pub fn request(targets: &[String], request: &str) -> String {
    format!("{}\t{request}", serde_json::to_string(targets).unwrap_or_default())
}
