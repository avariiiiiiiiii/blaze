//! The hashes a module's own records are made with: its digest, what it was built of, the answer
//! to a question of a span. Its machine code and verdicts are kept under `blaze_cas::Key`s
//! ([`crate::code_key`]), its expansions through [`blaze_usercode::index`] ([`crate::expand`]).

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

/// What a file of `bytes` has: short, the same for the same bytes ([`blaze_format::hex::encode_into`]
/// writes it).
pub(crate) fn digest_bytes(bytes: &[u8]) -> [u8; 16] {
    let digest = sha256_of(&[b"f", bytes]);
    digest[..16].try_into().expect("16 of 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_digest_is_the_first_half_of_its_sha256() {
        let mut said = String::new();
        blaze_format::hex::encode_into(&digest_bytes(b"x"), &mut said);
        let whole = Sha256::digest(b"fx");
        let want: String = whole.iter().take(16).map(|it| format!("{it:02x}")).collect();
        assert_eq!(said, want);
    }
}
