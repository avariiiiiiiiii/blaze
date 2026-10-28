//! Rust's symbol names, as rustc writes them; reading them from objects is the engine's
//! (`crates/blaze-engine/src/symbols.rs`).

/// How long the symbol rustc made is at the start of `name`, a symbol LLVM may have put a
/// suffix after (`.0`, `.llvm.<hash>`): `None` for no symbol of Rust's, or the memory a
/// static points to (`_RNn`), or a thread-local's first value (`$tlv$init`).
pub fn rust_symbol_length(name: &str) -> Option<usize> {
    if name.contains('$') && !name.starts_with("_ZN") || name.starts_with("_RNn") {
        return None;
    }
    if name.starts_with("_R") {
        // v0 writes only letters, digits and `_`.
        return Some(name.find('.').unwrap_or(name.len()));
    }
    // The legacy mangling: `_ZN`, names each after its length, `E`. A name may have `.` in
    // it (`core..fmt..Debug`), and `$` escapes.
    let mut rest = name.strip_prefix("_ZN")?.as_bytes();
    let mut at = 3;
    loop {
        if rest.first() == Some(&b'E') {
            return Some(at + 1);
        }
        let digits = rest.iter().take_while(|it| it.is_ascii_digit()).count();
        let length: usize = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
        rest = rest.get(digits + length..)?;
        at += digits + length;
    }
}
