//! Whether a program that takes edits runs; laying edits out for it is the engine's
//! (`crates/blaze-engine/src/live.rs`).

use blaze_live::format::{self, Process};

/// Whether the process a pid names runs.
pub use crate::raw::process::alive;

/// Whether any program that takes edits runs: a worker stopped while one does starts again
/// from the store, and its first build links the program again, which ends the program's
/// edits.
pub fn any_running() -> bool {
    let Some(root) = format::root() else { return false };
    let Ok(dirs) = std::fs::read_dir(root) else { return false };
    dirs.flatten().any(|dir| {
        let text = std::fs::read_to_string(dir.path().join("process")).unwrap_or_default();
        Process::decode(&text).is_some_and(|it| alive(it.pid))
    })
}
