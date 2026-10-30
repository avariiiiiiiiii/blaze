//! Byte comparison for the oracle (docs/v2/plan/hardware.md): where two outputs first
//! differ, 64 bytes at a time with AVX-512BW, 32 with AVX2, 16 with NEON. The path is chosen
//! at run time by CPU feature; every path gives the same answer as [`Simd::Scalar`], so the
//! choice is dispatch, never a different result.

use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

/// A way to compare, in the order [`fastest`] prefers them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Simd {
    Avx512,
    Avx2,
    Neon,
    Scalar,
}

/// The paths this CPU can run, fastest first; [`Simd::Scalar`] is always last.
pub fn available() -> Vec<Simd> {
    let mut paths = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512bw") {
            paths.push(Simd::Avx512);
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            paths.push(Simd::Avx2);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("neon") {
            paths.push(Simd::Neon);
        }
    }
    paths.push(Simd::Scalar);
    paths
}

pub fn fastest() -> Simd {
    available()[0]
}

/// The first offset at which `a` and `b` differ, or where the shorter ends; `None` when they
/// are equal.
pub fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    first_difference_with(fastest(), a, b)
}

/// [`first_difference`] by `path`, which must be one of [`available`]'s.
pub fn first_difference_with(path: Simd, a: &[u8], b: &[u8]) -> Option<usize> {
    let n = a.len().min(b.len());
    let longer = a.len() != b.len();
    let (a, b) = (&a[..n], &b[..n]);
    let found = match path {
        Simd::Scalar => scalar(a, b, 0),
        #[cfg(target_arch = "x86_64")]
        Simd::Avx512 => {
            assert!(std::arch::is_x86_feature_detected!("avx512bw"), "this CPU has no AVX-512BW");
            // SAFETY: the CPU has AVX-512BW, checked above.
            unsafe { avx512(a, b) }
        }
        #[cfg(target_arch = "x86_64")]
        Simd::Avx2 => {
            assert!(std::arch::is_x86_feature_detected!("avx2"), "this CPU has no AVX2");
            // SAFETY: the CPU has AVX2, checked above.
            unsafe { avx2(a, b) }
        }
        #[cfg(target_arch = "aarch64")]
        Simd::Neon => {
            assert!(std::arch::is_aarch64_feature_detected!("neon"), "this CPU has no NEON");
            // SAFETY: the CPU has NEON, checked above.
            unsafe { neon(a, b) }
        }
        #[allow(unreachable_patterns)]
        other => panic!("{other:?} is not a path of this architecture"),
    };
    found.or(longer.then_some(n))
}

fn scalar(a: &[u8], b: &[u8], from: usize) -> Option<usize> {
    (from..a.len()).find(|&i| a[i] != b[i])
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw")]
unsafe fn avx512(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{_mm512_cmpneq_epi8_mask, _mm512_loadu_si512};
    let mut at = 0;
    while at + 64 <= a.len() {
        // SAFETY: `at + 64 <= a.len() == b.len()`; the loads are unaligned.
        let differ = unsafe {
            _mm512_cmpneq_epi8_mask(
                _mm512_loadu_si512(a.as_ptr().add(at).cast()),
                _mm512_loadu_si512(b.as_ptr().add(at).cast()),
            )
        };
        if differ != 0 {
            return Some(at + differ.trailing_zeros() as usize);
        }
        at += 64;
    }
    scalar(a, b, at)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{_mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};
    let mut at = 0;
    while at + 32 <= a.len() {
        // SAFETY: `at + 32 <= a.len() == b.len()`; the loads are unaligned.
        let same = unsafe {
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(
                _mm256_loadu_si256(a.as_ptr().add(at).cast()),
                _mm256_loadu_si256(b.as_ptr().add(at).cast()),
            ))
        } as u32;
        if same != u32::MAX {
            return Some(at + (!same).trailing_zeros() as usize);
        }
        at += 32;
    }
    scalar(a, b, at)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::aarch64::{vceqq_u8, vld1q_u8, vminvq_u8};
    let mut at = 0;
    while at + 16 <= a.len() {
        // SAFETY: `at + 16 <= a.len() == b.len()`; `vld1q_u8` has no alignment requirement.
        let all_same = unsafe { vminvq_u8(vceqq_u8(vld1q_u8(a.as_ptr().add(at)), vld1q_u8(b.as_ptr().add(at)))) };
        if all_same != u8::MAX {
            return scalar(&a[..at + 16], &b[..at + 16], at);
        }
        at += 16;
    }
    scalar(a, b, at)
}

/// Where two files first differ, `None` when their bytes are the same. Equal lengths are
/// checked first, so files of other sizes are told apart without reading them.
pub fn first_file_difference(a: &Path, b: &Path) -> io::Result<Option<u64>> {
    let (mut fa, mut fb) = (File::open(a)?, File::open(b)?);
    let (la, lb) = (fa.metadata()?.len(), fb.metadata()?.len());
    let path = fastest();
    let (mut xa, mut xb) = (vec![0; 1 << 20], vec![0; 1 << 20]);
    let mut at = 0u64;
    loop {
        let (na, nb) = (fill(&mut fa, &mut xa)?, fill(&mut fb, &mut xb)?);
        if let Some(i) = first_difference_with(path, &xa[..na], &xb[..nb]) {
            return Ok(Some(at + i as u64));
        }
        if na == 0 {
            return Ok((la != lb).then_some(at));
        }
        at += na as u64;
    }
}

/// Reads until `buffer` is full or the file ends, so both files are compared at one offset.
fn fill(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}
