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

/// What a file of `bytes` has: short, the same for the same bytes.
pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"f");
    hash.update(bytes);
    hash.finalize().iter().take(16).map(|it| format!("{it:02x}")).collect()
}
