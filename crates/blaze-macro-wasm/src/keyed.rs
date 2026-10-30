//! The hashes a module's own records are made with: its digest, its machine code's key, whether
//! it keeps state, the answer to a question of a span. Its expansions are kept through
//! [`blaze_usercode::index`] ([`crate::expand`]).

use sha2::{Digest as _, Sha256};

/// A key of fields, each its length and its bytes.
pub(crate) struct Key(Sha256);

impl Key {
    pub(crate) fn new(kind: &str) -> Key {
        let mut key = Key(Sha256::new());
        key.field(kind.as_bytes());
        key
    }

    pub(crate) fn field(&mut self, bytes: &[u8]) -> &mut Key {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    pub(crate) fn done(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The SHA-256 of `parts` one after the other.
pub(crate) fn sha256_of(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
    }
    hash.finalize().into()
}

/// What a file of `bytes` has: short, the same for the same bytes ([`hex`] writes it).
pub(crate) fn digest_bytes(bytes: &[u8]) -> [u8; 16] {
    let digest = sha256_of(&[b"f", bytes]);
    digest[..16].try_into().expect("16 of 32 bytes")
}

const DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Appends `bytes` to `out` in lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8], out: &mut String) {
    out.reserve(2 * bytes.len());
    for &byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 15)] as char);
    }
}

/// Whether `text` is what [`hex`] writes of `bytes`.
pub(crate) fn is_hex(text: &str, bytes: &[u8]) -> bool {
    text.len() == 2 * bytes.len()
        && text.as_bytes().as_chunks::<2>().0.iter().zip(bytes).all(|(pair, &byte)| {
            *pair == [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 15)]]
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_written_as_its_hexadecimal() {
        let mut out = String::new();
        hex(&[0x00, 0x0f, 0xa5, 0xff], &mut out);
        assert_eq!(out, "000fa5ff");
        let mut said = String::new();
        hex(&digest_bytes(b"x"), &mut said);
        let whole = Sha256::digest(b"fx");
        let want: String = whole.iter().take(16).map(|it| format!("{it:02x}")).collect();
        assert_eq!(said, want);
        assert!(is_hex(&said, &digest_bytes(b"x")));
        assert!(!is_hex(&said.to_uppercase(), &digest_bytes(b"x")));
        assert!(!is_hex(&said[2..], &digest_bytes(b"x")));
        assert!(!is_hex(&said, &digest_bytes(b"y")));
    }
}
