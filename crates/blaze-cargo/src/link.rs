//! What the cargo drop-in reads of the links the engine makes (crates/blaze-engine/src/link.rs).

use std::path::Path;

/// What the stand-ins left in the executable at `exe` say: the functions blaze could not
/// compile that the program can reach.
pub fn stand_ins(exe: &Path) -> anyhow::Result<Vec<String>> {
    (crate::engine::get().stand_ins)(exe)
}
