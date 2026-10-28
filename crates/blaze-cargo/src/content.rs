//! How a hash is written; crate identity by content is the engine's
//! (`crates/blaze-engine/src/content.rs`).

/// `bytes` in lower-case hex, two digits each: how a hash is written in a name or a key.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}
