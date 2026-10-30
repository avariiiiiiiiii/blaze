# Requests to frontend-tokens

## dedup: LitKind's wire number, once (from dedup-macros, 2026-10-29)

`LitKind` to and from its number 0 to 10 is written out in `crates/tt/src/storage.rs`
(`Node::literal` 144-156 and `Node::lit_kind` 166-178), in `hir-def`'s `nameres/frozen.rs`
(960-972 and 2289-2301, requested of db) and in both proc-macro wires (which move onto tt's
column bytes in your request to usercode). Design: `docs/v2/plan/lanes/dedup/macros.md`.

1. In `crates/tt/src/lib.rs`, after `pub enum LitKind`:

```rust
impl LitKind {
    /// This kind's number in tt's encodings (the column format, and hir-def's frozen trees), and
    /// its count of `#`s.
    #[inline]
    pub const fn code(self) -> (u8, u8) {
        match self {
            LitKind::Byte => (0, 0),
            LitKind::Char => (1, 0),
            LitKind::Integer => (2, 0),
            LitKind::Float => (3, 0),
            LitKind::Str => (4, 0),
            LitKind::StrRaw(hashes) => (5, hashes),
            LitKind::ByteStr => (6, 0),
            LitKind::ByteStrRaw(hashes) => (7, hashes),
            LitKind::CStr => (8, 0),
            LitKind::CStrRaw(hashes) => (9, hashes),
            LitKind::Err(()) => (10, 0),
        }
    }

    /// The kind [`LitKind::code`] numbers `code`, with `hashes` for a raw one; `None` past 10.
    #[inline]
    pub const fn from_code(code: u8, hashes: u8) -> Option<LitKind> {
        Some(match code {
            0 => LitKind::Byte,
            1 => LitKind::Char,
            2 => LitKind::Integer,
            3 => LitKind::Float,
            4 => LitKind::Str,
            5 => LitKind::StrRaw(hashes),
            6 => LitKind::ByteStr,
            7 => LitKind::ByteStrRaw(hashes),
            8 => LitKind::CStr,
            9 => LitKind::CStrRaw(hashes),
            10 => LitKind::Err(()),
            _ => return None,
        })
    }
}
```

2. `crates/tt/src/storage.rs`, `Node::literal`: replace the `let (code, hashes) = match kind { ... };`
   with `let (code, hashes) = kind.code();` and then `let (code, hashes) = (u32::from(code), hashes);`
   (the `head` expression shifts `code` as a `u32`).
   `Node::lit_kind`: replace its body with:

```rust
        let hashes = (self.head >> 6) as u8;
        // A node's code is one `literal` wrote, so it is at most 10; 11 to 15 read as `Err`, as
        // before.
        LitKind::from_code(((self.head >> 2) & 0xF) as u8, hashes).unwrap_or(LitKind::Err(()))
```

3. Tests in `crates/tt/src/storage/tests.rs`:
   - every kind, with 0, 1 and 255 hashes for the raw ones, round-trips through `code` and
     `from_code`;
   - `from_code(11..=255, _)` is `None`;
   - a `proptest` differential against the old `match`, copied into the test as the reference.
     This needs `proptest.workspace = true` under `[dev-dependencies]` in `crates/tt/Cargo.toml`;
     `proptest` is already a root workspace dependency.

> forwarded to frontend-tokens (`crates/tt/` is held by it, still running; told).
