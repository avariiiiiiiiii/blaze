//! Large copies into buffers that go straight to the kernel or the network and are not read
//! again soon, with non-temporal stores so they don't evict the cache. Every path leaves the
//! same bytes, so which one runs is dispatch, not a fallback.

/// From this many bytes on, [`append`] copies with non-temporal stores.
pub(crate) const STREAM_FROM: usize = 4 << 20;

/// Appends `src` to `dst`. From [`STREAM_FROM`] bytes on, with non-temporal stores, for a
/// copy that is not read again soon. Leaves `dst` exactly as `dst.extend_from_slice(src)`.
pub(crate) fn append(dst: &mut Vec<u8>, src: &[u8]) {
    if src.len() < STREAM_FROM {
        dst.extend_from_slice(src);
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx512f") {
        avx512(dst, src);
    } else {
        sse2(dst, src);
    }
    #[cfg(target_arch = "aarch64")]
    stnp(dst, src);
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    dst.extend_from_slice(src);
}

/// How many bytes to copy plainly before `to` is aligned to `width` (a power of two), and how
/// many after them fill whole `width`-byte stores.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn split(to: *mut u8, len: usize, width: usize) -> (usize, usize) {
    let head = (to.addr().wrapping_neg() & (width - 1)).min(len);
    (head, (len - head) / width * width)
}

#[cfg(target_arch = "x86_64")]
fn avx512(dst: &mut Vec<u8>, src: &[u8]) {
    assert!(is_x86_feature_detected!("avx512f"));
    dst.reserve(src.len());
    // SAFETY: the CPU has AVX-512F, asserted above. `reserve` left room for `src.len()` bytes
    // after the length, which `x86::avx512` writes all of, and `src` is borrowed shared while
    // `dst` is borrowed mutably, so they don't overlap.
    unsafe {
        x86::avx512(dst.as_mut_ptr().add(dst.len()), src);
        dst.set_len(dst.len() + src.len());
    }
}

#[cfg(target_arch = "x86_64")]
fn sse2(dst: &mut Vec<u8>, src: &[u8]) {
    dst.reserve(src.len());
    // SAFETY: `reserve` left room for `src.len()` bytes after the length, which `x86::sse2`
    // writes all of, and `src` is borrowed shared while `dst` is borrowed mutably, so they
    // don't overlap.
    unsafe {
        x86::sse2(dst.as_mut_ptr().add(dst.len()), src);
        dst.set_len(dst.len() + src.len());
    }
}

#[cfg(target_arch = "aarch64")]
fn stnp(dst: &mut Vec<u8>, src: &[u8]) {
    dst.reserve(src.len());
    // SAFETY: `reserve` left room for `src.len()` bytes after the length, which `arm::stnp`
    // writes all of, and `src` is borrowed shared while `dst` is borrowed mutably, so they
    // don't overlap.
    unsafe {
        arm::stnp(dst.as_mut_ptr().add(dst.len()), src);
        dst.set_len(dst.len() + src.len());
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::{
        _mm_loadu_si128, _mm_sfence, _mm_stream_si128, _mm512_loadu_si512, _mm512_stream_si512,
    };
    use std::ptr;

    use super::split;

    /// Copies `from` to `to`, the 64-byte-aligned middle with non-temporal stores.
    ///
    /// # Safety
    ///
    /// The CPU has AVX-512F; `to` is valid for writes of `from.len()` bytes and doesn't overlap
    /// `from`.
    #[target_feature(enable = "avx512f")]
    pub(super) unsafe fn avx512(to: *mut u8, from: &[u8]) {
        let (head, body) = split(to, from.len(), 64);
        let src = from.as_ptr();
        // SAFETY: every offset is below `from.len()`, for which both pointers are valid (the
        // caller's contract), and each store is at `to + head + 64k`, 64-byte aligned by
        // `split`. The non-temporal stores are fenced before anything else touches `to`.
        unsafe {
            ptr::copy_nonoverlapping(src, to, head);
            let mut at = head;
            while at < head + body {
                let v = _mm512_loadu_si512(src.add(at).cast());
                _mm512_stream_si512(to.add(at).cast(), v);
                at += 64;
            }
            _mm_sfence();
            ptr::copy_nonoverlapping(src.add(at), to.add(at), from.len() - at);
        }
    }

    /// Copies `from` to `to`, the 16-byte-aligned middle with non-temporal stores.
    ///
    /// # Safety
    ///
    /// `to` is valid for writes of `from.len()` bytes and doesn't overlap `from`.
    pub(super) unsafe fn sse2(to: *mut u8, from: &[u8]) {
        let (head, body) = split(to, from.len(), 16);
        let src = from.as_ptr();
        // SAFETY: SSE2 is baseline on x86_64. Every offset is below `from.len()`, for which
        // both pointers are valid (the caller's contract), and each store is at
        // `to + head + 16k`, 16-byte aligned by `split`. The non-temporal stores are fenced
        // before anything else touches `to`.
        unsafe {
            ptr::copy_nonoverlapping(src, to, head);
            let mut at = head;
            while at < head + body {
                let v = _mm_loadu_si128(src.add(at).cast());
                _mm_stream_si128(to.add(at).cast(), v);
                at += 16;
            }
            _mm_sfence();
            ptr::copy_nonoverlapping(src.add(at), to.add(at), from.len() - at);
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod arm {
    use std::arch::asm;
    use std::ptr;

    use super::split;

    /// Copies `from` to `to`, the 64-byte-aligned middle with non-temporal pair stores. No
    /// barrier: the bytes are only read later through `to`, on this thread or after whatever
    /// synchronization hands `to` over, which orders them like any other store.
    ///
    /// # Safety
    ///
    /// `to` is valid for writes of `from.len()` bytes and doesn't overlap `from`.
    pub(super) unsafe fn stnp(to: *mut u8, from: &[u8]) {
        let (head, body) = split(to, from.len(), 64);
        let src = from.as_ptr();
        // SAFETY: NEON is baseline on aarch64. Every offset is below `from.len()`, for which
        // both pointers are valid (the caller's contract): each `asm!` reads and writes the 64
        // bytes at `at`, and `at + 64 <= head + body <= from.len()`.
        unsafe {
            ptr::copy_nonoverlapping(src, to, head);
            let mut at = head;
            while at < head + body {
                asm!(
                    "ldp {a:q}, {b:q}, [{src}]",
                    "ldp {c:q}, {d:q}, [{src}, #32]",
                    "stnp {a:q}, {b:q}, [{dst}]",
                    "stnp {c:q}, {d:q}, [{dst}, #32]",
                    src = in(reg) src.add(at),
                    dst = in(reg) to.add(at),
                    a = out(vreg) _,
                    b = out(vreg) _,
                    c = out(vreg) _,
                    d = out(vreg) _,
                    options(nostack, preserves_flags),
                );
                at += 64;
            }
            ptr::copy_nonoverlapping(src.add(at), to.add(at), from.len() - at);
        }
    }
}

#[cfg(test)]
fn plain(dst: &mut Vec<u8>, src: &[u8]) {
    dst.extend_from_slice(src);
}

/// Every path this CPU can run, the plain copy first, for the differential tests. Each
/// streams whatever the length; only [`append`] applies [`STREAM_FROM`].
#[cfg(test)]
pub(crate) fn paths() -> Vec<(&'static str, fn(&mut Vec<u8>, &[u8]))> {
    let mut out: Vec<(&'static str, fn(&mut Vec<u8>, &[u8]))> = vec![("extend_from_slice", plain)];
    #[cfg(target_arch = "x86_64")]
    {
        out.push(("sse2", sse2));
        if is_x86_feature_detected!("avx512f") {
            out.push(("avx512f", avx512));
        }
    }
    #[cfg(target_arch = "aarch64")]
    out.push(("stnp", stnp));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = blaze_core::sim::Rng::new(seed);
        (0..len).map(|_| rng.next_u64() as u8).collect()
    }

    /// `copy` of `src` onto a `dst` holding `prefix`, with `spare` bytes of capacity beyond
    /// what the copy needs, against `extend_from_slice`.
    fn check(name: &str, copy: fn(&mut Vec<u8>, &[u8]), prefix: &[u8], src: &[u8], spare: usize) {
        let mut expected = prefix.to_vec();
        expected.extend_from_slice(src);
        let mut got = Vec::with_capacity(prefix.len() + src.len() + spare);
        got.extend_from_slice(prefix);
        copy(&mut got, src);
        assert!(
            got == expected,
            "{name}: {} onto {} with {spare} spare",
            src.len(),
            prefix.len()
        );
    }

    #[test]
    fn every_path_copies_short_inputs() {
        let buffer = noise(1100, 1);
        let prefix = noise(70, 2);
        let mut copies = paths();
        copies.push(("append", append));
        for (name, copy) in copies {
            for len in (0..=300).chain([511, 512, 513, 1000, 1023, 1024, 1025]) {
                for offset in 0..16 {
                    for kept in [0, 1, 7, 16, 33, 64, 70] {
                        for spare in [0, 9] {
                            let src = &buffer[offset..offset + len];
                            check(name, copy, &prefix[..kept], src, spare);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_path_copies_around_the_threshold() {
        let buffer = noise(STREAM_FROM + 64 + 16, 3);
        let prefix = noise(70, 4);
        let mut copies = paths();
        copies.push(("append", append));
        for (name, copy) in copies {
            for len in [STREAM_FROM - 1, STREAM_FROM, STREAM_FROM + 1, STREAM_FROM + 63] {
                for offset in [0, 13] {
                    for kept in [0, 5, 64] {
                        let src = &buffer[offset..offset + len];
                        check(name, copy, &prefix[..kept], src, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn append_onto_an_empty_vec_is_the_bytes() {
        let src = noise(STREAM_FROM + 3, 5);
        let mut dst = Vec::new();
        append(&mut dst, &src);
        assert!(dst == src);
    }
}
