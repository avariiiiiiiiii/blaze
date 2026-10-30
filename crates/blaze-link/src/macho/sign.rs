//! The ad-hoc code signature ld64 makes (`LC_CODE_SIGNATURE`, linker-signed): a blob of
//! blobs with one code directory, the SHA-256 of every 4 KiB page of the file before the
//! signature. macOS on arm64 runs no page without it, and checks every page it maps.

use super::sha256;

const SUPER_BLOB: u32 = 0xfade_0cc0;
const CODE_DIRECTORY: u32 = 0xfade_0c02;
/// The code directory's version with the executable segment's fields.
const VERSION: u32 = 0x0002_0400;
const ADHOC: u32 = 0x2;
const LINKER_SIGNED: u32 = 0x2_0000;
const EXECSEG_MAIN_BINARY: u64 = 0x1;
const HASH_SHA256: u8 = 2;
const HASH: usize = 32;
const PAGE_SHIFT: u8 = 12;
pub(super) const PAGE: usize = 1 << PAGE_SHIFT;
/// The super blob's header and its one index entry.
const BLOB_HEADER: usize = 12 + 8;
/// The code directory's fields up to its identifier.
const DIRECTORY_HEADER: usize = 88;

/// How long the signature of a file of `limit` bytes before it is.
pub(super) fn len(identifier: &str, limit: u64) -> u64 {
    let pages = (limit as usize).div_ceil(PAGE);
    (BLOB_HEADER + DIRECTORY_HEADER + identifier.len() + 1 + pages * HASH).next_multiple_of(16)
        as u64
}

/// The signature of `file` (every byte before the signature), whose `__TEXT` is `text_len`
/// bytes from `text_at`; `main` for an executable.
pub(super) fn signature(
    file: &[u8],
    identifier: &str,
    text_at: u64,
    text_len: u64,
    main: bool,
) -> Vec<u8> {
    let pages = file.len().div_ceil(PAGE);
    let hashes_at = DIRECTORY_HEADER + identifier.len() + 1;
    let directory_len = hashes_at + pages * HASH;
    let total = len(identifier, file.len() as u64) as usize;

    let mut out = Vec::with_capacity(total);
    let be32 = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_be_bytes());
    be32(&mut out, SUPER_BLOB);
    be32(&mut out, (BLOB_HEADER + directory_len) as u32);
    be32(&mut out, 1);
    // The code directory's slot, and where it is.
    be32(&mut out, 0);
    be32(&mut out, BLOB_HEADER as u32);

    be32(&mut out, CODE_DIRECTORY);
    be32(&mut out, directory_len as u32);
    be32(&mut out, VERSION);
    be32(&mut out, ADHOC | LINKER_SIGNED);
    be32(&mut out, hashes_at as u32);
    be32(&mut out, DIRECTORY_HEADER as u32);
    // No special slots: a linker's signature has no requirements or entitlements.
    be32(&mut out, 0);
    be32(&mut out, pages as u32);
    be32(&mut out, file.len() as u32);
    out.extend_from_slice(&[HASH as u8, HASH_SHA256, 0, PAGE_SHIFT]);
    // spare2, scatterOffset, teamOffset, spare3.
    for _ in 0..4 {
        be32(&mut out, 0);
    }
    // codeLimit64: the 32-bit limit says it.
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&text_at.to_be_bytes());
    out.extend_from_slice(&text_len.to_be_bytes());
    let flags = if main { EXECSEG_MAIN_BINARY } else { 0 };
    out.extend_from_slice(&flags.to_be_bytes());
    debug_assert_eq!(out.len(), BLOB_HEADER + DIRECTORY_HEADER);
    out.extend_from_slice(identifier.as_bytes());
    out.push(0);

    let hashes: Vec<[u8; 32]> = {
        use rayon::prelude::*;
        file.par_chunks(PAGE).map(sha256::digest).collect()
    };
    for hash in hashes {
        out.extend_from_slice(&hash);
    }
    out.resize(total, 0);
    out
}
