//! The ad-hoc code signature ld64 makes (`LC_CODE_SIGNATURE`, linker-signed): a blob of
//! blobs with one code directory, the SHA-256 of every 4 KiB page of the file before the
//! signature. macOS on arm64 runs no page without it, and checks every page it maps.
//!
//! The pages are hashed where they are, in the regions the core placed ([`View`]): nothing
//! assembles the file first. They are hashed in parallel, and an incremental link hashes only
//! the pages whose bytes differ from the last signature's ([`Cache`]).

#![forbid(unsafe_code)]

use blaze_ids::radix;
use rayon::prelude::*;

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
/// Pages one parallel task hashes: 256 KiB, so a task's work outweighs its scheduling.
const GROUP: usize = 64;

/// How long the signature of a file of `limit` bytes before it is.
pub(super) fn len(identifier: &str, limit: u64) -> u64 {
    let pages = (limit as usize).div_ceil(PAGE);
    (BLOB_HEADER + DIRECTORY_HEADER + identifier.len() + 1 + pages * HASH).next_multiple_of(16)
        as u64
}

/// The file before the signature, as the byte ranges it is made of: zero between them.
pub(super) struct View<'a> {
    /// By offset, not overlapping, none empty.
    parts: Vec<(usize, &'a [u8])>,
    len: usize,
}

impl<'a> View<'a> {
    /// The file of `len` bytes holding `parts` at their offsets; an error naming the offsets
    /// when two overlap or one runs past `len`.
    pub(super) fn new(len: u64, mut parts: Vec<(u64, &'a [u8])>) -> Result<View<'a>, String> {
        parts.retain(|it| !it.1.is_empty());
        if !parts.is_sorted_by_key(|it| it.0) {
            radix::sort_copy_by_key(&mut parts, |it| it.0);
        }
        let mut end = 0;
        for &(at, bytes) in &parts {
            if at < end {
                return Err(format!("the file's bytes at {at:#x} overlap those before {end:#x}"));
            }
            end = at + bytes.len() as u64;
        }
        if end > len {
            return Err(format!("the file's bytes run to {end:#x}, past its {len:#x} signed bytes"));
        }
        let parts = parts.into_iter().map(|(at, bytes)| (at as usize, bytes)).collect();
        Ok(View { parts, len: len as usize })
    }

    /// The file of `bytes`.
    #[cfg(test)]
    pub(super) fn whole(bytes: &'a [u8]) -> View<'a> {
        View { parts: vec![(0, bytes)], len: bytes.len() }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Page `nth`: borrowed when one part holds all of it, else gathered into `buf`.
    fn page<'b>(&'b self, nth: usize, buf: &'b mut [u8; PAGE]) -> &'b [u8] {
        let start = nth * PAGE;
        let end = (start + PAGE).min(self.len);
        let first = self.parts.partition_point(|(at, bytes)| at + bytes.len() <= start);
        if let Some(&(at, bytes)) = self.parts.get(first)
            && at <= start
            && at + bytes.len() >= end
        {
            return &bytes[start - at..end - at];
        }
        let buf = &mut buf[..end - start];
        buf.fill(0);
        for &(at, bytes) in &self.parts[first..] {
            if at >= end {
                break;
            }
            let (from, to) = (at.max(start), (at + bytes.len()).min(end));
            buf[from - start..to - start].copy_from_slice(&bytes[from - at..to - at]);
        }
        buf
    }
}

/// The pages of the last signature and their hashes: a page whose bytes are the same is not
/// hashed again. Exact (bytes compared, not fingerprints), so a signature never depends on
/// what is cached. 4 KiB plus 32 bytes per page of the image.
#[derive(Default)]
pub(super) struct Cache {
    /// `pages * PAGE` bytes: each page as last signed, zero past `len`.
    pages: Vec<u8>,
    hashes: Vec<[u8; HASH]>,
    /// The length of the file last signed.
    len: usize,
}

/// The signature of `file` (every byte before the signature), whose `__TEXT` is `text_len`
/// bytes from `text_at`; `main` for an executable. Appended to `out`.
pub(super) fn signature(
    file: &View<'_>,
    identifier: &str,
    text_at: u64,
    text_len: u64,
    main: bool,
    cache: Option<&mut Cache>,
    out: &mut Vec<u8>,
) {
    let pages = file.len().div_ceil(PAGE);
    let hashes_at = DIRECTORY_HEADER + identifier.len() + 1;
    let directory_len = hashes_at + pages * HASH;
    let total = len(identifier, file.len() as u64) as usize;

    let start = out.len();
    out.reserve(total);
    let be32 = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_be_bytes());
    be32(out, SUPER_BLOB);
    be32(out, (BLOB_HEADER + directory_len) as u32);
    be32(out, 1);
    // The code directory's slot, and where it is.
    be32(out, 0);
    be32(out, BLOB_HEADER as u32);

    be32(out, CODE_DIRECTORY);
    be32(out, directory_len as u32);
    be32(out, VERSION);
    be32(out, ADHOC | LINKER_SIGNED);
    be32(out, hashes_at as u32);
    be32(out, DIRECTORY_HEADER as u32);
    // No special slots: a linker's signature has no requirements or entitlements.
    be32(out, 0);
    be32(out, pages as u32);
    be32(out, file.len() as u32);
    out.extend_from_slice(&[HASH as u8, HASH_SHA256, 0, PAGE_SHIFT]);
    // spare2, scatterOffset, teamOffset, spare3.
    for _ in 0..4 {
        be32(out, 0);
    }
    // codeLimit64: the 32-bit limit says it.
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&text_at.to_be_bytes());
    out.extend_from_slice(&text_len.to_be_bytes());
    let flags = if main { EXECSEG_MAIN_BINARY } else { 0 };
    out.extend_from_slice(&flags.to_be_bytes());
    debug_assert_eq!(out.len() - start, BLOB_HEADER + DIRECTORY_HEADER);
    out.extend_from_slice(identifier.as_bytes());
    out.push(0);

    let at = out.len();
    out.resize(start + total, 0);
    let slots = &mut out[at..at + pages * HASH];
    match cache {
        None => hash(file, slots),
        Some(cache) => hash_cached(file, slots, cache),
    }
}

fn hash(file: &View<'_>, slots: &mut [u8]) {
    slots.par_chunks_mut(HASH * GROUP).enumerate().for_each(|(group, slots)| {
        let mut buf = [0u8; PAGE];
        for (nth, slot) in slots.chunks_exact_mut(HASH).enumerate() {
            slot.copy_from_slice(&sha256::digest(file.page(group * GROUP + nth, &mut buf)));
        }
    });
}

fn hash_cached(file: &View<'_>, slots: &mut [u8], cache: &mut Cache) {
    let pages = slots.len() / HASH;
    let was = cache.len;
    cache.pages.resize(pages * PAGE, 0);
    cache.hashes.resize(pages, [0; HASH]);
    (slots.par_chunks_mut(HASH * GROUP))
        .zip(cache.pages.par_chunks_mut(PAGE * GROUP))
        .zip(cache.hashes.par_chunks_mut(GROUP))
        .enumerate()
        .for_each(|(group, ((slots, old), hashes))| {
            let mut buf = [0u8; PAGE];
            for (nth, slot) in slots.chunks_exact_mut(HASH).enumerate() {
                let page = group * GROUP + nth;
                let bytes = file.page(page, &mut buf);
                let old = &mut old[nth * PAGE..nth * PAGE + bytes.len()];
                let old_len = was.saturating_sub(page * PAGE).min(PAGE);
                if old_len != bytes.len() || old != bytes {
                    old.copy_from_slice(bytes);
                    hashes[nth] = sha256::digest(bytes);
                }
                slot.copy_from_slice(&hashes[nth]);
            }
        });
    cache.len = file.len();
}

#[cfg(test)]
mod tests {
    use super::{Cache, PAGE, View, signature};

    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut seed = seed | 1;
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    /// The file a view stands for, assembled.
    fn assembled(len: usize, parts: &[(u64, &[u8])]) -> Vec<u8> {
        let mut file = vec![0; len];
        for &(at, bytes) in parts {
            file[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        }
        file
    }

    #[test]
    fn a_view_signs_as_the_assembled_file() {
        let (a, b, c) = (noise(1, 100), noise(2, 3 * PAGE + 7), noise(3, PAGE));
        let len = 6 * PAGE + 300;
        // Parts within one page, across pages, page-aligned, and gaps of zeros between.
        let parts: Vec<(u64, &[u8])> = vec![
            (4 * PAGE as u64 + 10, &c[..]),
            (0, &a[..]),
            (PAGE as u64 - 3, &b[..]),
            (6 * PAGE as u64, &[][..]),
        ];
        let view = View::new(len as u64, parts.clone()).unwrap();
        let file = assembled(len, &parts);
        let (mut want, mut seen) = (Vec::new(), Vec::new());
        signature(&View::whole(&file), "x", 0, 0x4000, true, None, &mut want);
        signature(&view, "x", 0, 0x4000, true, None, &mut seen);
        assert_eq!(seen, want);
    }

    #[test]
    fn overlapping_or_long_parts_are_errors() {
        let (a, b) = ([1u8; 10], [2u8; 10]);
        assert!(View::new(100, vec![(0, &a[..]), (5, &b[..])]).is_err());
        assert!(View::new(15, vec![(10, &a[..])]).is_err());
    }

    #[test]
    fn the_cache_changes_nothing() {
        let mut cache = Cache::default();
        let mut file = noise(7, 70 * PAGE + 123);
        for round in 0..6u64 {
            match round {
                // A page changed, the file grown, shrunk, a page changed back, unchanged.
                1 => file[5 * PAGE + 9] ^= 0xff,
                2 => file.extend_from_slice(&noise(round, 3 * PAGE + 11)),
                3 => file.truncate(40 * PAGE + 5),
                4 => file[5 * PAGE + 9] ^= 0xff,
                _ => {}
            }
            let (mut want, mut seen) = (Vec::new(), Vec::new());
            signature(&View::whole(&file), "id", 0, 0x4000, false, None, &mut want);
            signature(&View::whole(&file), "id", 0, 0x4000, false, Some(&mut cache), &mut seen);
            assert_eq!(seen, want, "round {round}");
        }
    }
}
