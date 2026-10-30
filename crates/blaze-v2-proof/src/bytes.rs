//! Byte comparison for the oracle (docs/v2/plan/design/hardware.md): where two outputs first
//! differ, 64 bytes at a time with AVX-512BW, 32 with AVX2, 16 with SSE2 or NEON, four vectors
//! per loop and one flag test per four. The path is chosen at run time by CPU feature; every
//! path gives the same answer as [`Simd::Scalar`], so the choice is dispatch, never a different
//! result.
//!
//! Files are compared mapped, in place: no copy into a buffer, no read per megabyte. A short
//! tail is compared by one last vector that overlaps the bytes already found equal, so no path
//! drops to a byte loop except on inputs shorter than one vector.

use std::{fs::File, io, path::Path, sync::OnceLock};

/// A way to compare, in the order [`fastest`] prefers them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Simd {
    Avx512,
    Avx2,
    Sse2,
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
        if std::arch::is_x86_feature_detected!("sse2") {
            paths.push(Simd::Sse2);
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

/// The first of [`available`], detected once per process.
pub fn fastest() -> Simd {
    static FASTEST: OnceLock<Simd> = OnceLock::new();
    *FASTEST.get_or_init(|| available()[0])
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
        #[cfg(target_arch = "x86_64")]
        Simd::Sse2 => {
            assert!(std::arch::is_x86_feature_detected!("sse2"), "this CPU has no SSE2");
            // SAFETY: the CPU has SSE2, checked above.
            unsafe { sse2(a, b) }
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

/// The reference every vector path is tested against.
fn scalar(a: &[u8], b: &[u8], from: usize) -> Option<usize> {
    (from..a.len()).find(|&i| a[i] != b[i])
}

// Each vector path below takes `a` and `b` of one length `n`. It compares four vectors per
// iteration while `4 * W` bytes remain, then one per iteration, then the last `W` bytes of
// the input (overlapping bytes already found equal, so the first difference in it is the
// first of all); inputs shorter than `W` go to `scalar`, or a masked load on AVX-512.

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw")]
unsafe fn avx512(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{__m512i, _mm512_cmpneq_epi8_mask, _mm512_loadu_si512, _mm512_maskz_loadu_epi8};
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    #[inline]
    #[target_feature(enable = "avx512bw")]
    unsafe fn differ(pa: *const u8, pb: *const u8) -> u64 {
        // SAFETY: the caller has 64 bytes at each pointer; the loads are unaligned.
        unsafe { _mm512_cmpneq_epi8_mask(_mm512_loadu_si512(pa.cast::<__m512i>()), _mm512_loadu_si512(pb.cast::<__m512i>())) }
    }
    if n < 64 {
        let keep = (1u64 << n) - 1;
        // SAFETY: masked-off lanes are not read (AVX-512 masked loads touch only the lanes of
        // `keep`, the first `n` bytes of each slice), and both lanes read zero there.
        let differ = unsafe { _mm512_cmpneq_epi8_mask(_mm512_maskz_loadu_epi8(keep, pa.cast()), _mm512_maskz_loadu_epi8(keep, pb.cast())) };
        return (differ != 0).then(|| differ.trailing_zeros() as usize);
    }
    let mut at = 0;
    while at + 256 <= n {
        // SAFETY: `at + 256 <= n`, the length of both slices.
        let m = unsafe {
            [differ(pa.add(at), pb.add(at)), differ(pa.add(at + 64), pb.add(at + 64)),
             differ(pa.add(at + 128), pb.add(at + 128)), differ(pa.add(at + 192), pb.add(at + 192))]
        };
        if m[0] | m[1] | m[2] | m[3] != 0 {
            let k = m.iter().position(|&w| w != 0).expect("one of the four differs");
            return Some(at + 64 * k + m[k].trailing_zeros() as usize);
        }
        at += 256;
    }
    while at + 64 <= n {
        // SAFETY: `at + 64 <= n`.
        let m = unsafe { differ(pa.add(at), pb.add(at)) };
        if m != 0 {
            return Some(at + m.trailing_zeros() as usize);
        }
        at += 64;
    }
    if at < n {
        let last = n - 64;
        // SAFETY: `n >= 64`, so `last + 64 == n`.
        let m = unsafe { differ(pa.add(last), pb.add(last)) };
        if m != 0 {
            return Some(last + m.trailing_zeros() as usize);
        }
    }
    None
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{__m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    /// A bit per byte, set where the bytes are equal.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn same(pa: *const u8, pb: *const u8) -> u32 {
        // SAFETY: the caller has 32 bytes at each pointer; the loads are unaligned.
        unsafe {
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(pa.cast::<__m256i>()), _mm256_loadu_si256(pb.cast::<__m256i>())))
                as u32
        }
    }
    if n < 32 {
        return scalar(a, b, 0);
    }
    let mut at = 0;
    while at + 128 <= n {
        // SAFETY: `at + 128 <= n`, the length of both slices.
        let m = unsafe {
            [same(pa.add(at), pb.add(at)), same(pa.add(at + 32), pb.add(at + 32)),
             same(pa.add(at + 64), pb.add(at + 64)), same(pa.add(at + 96), pb.add(at + 96))]
        };
        if m[0] & m[1] & m[2] & m[3] != u32::MAX {
            let all = u128::from(m[0]) | (u128::from(m[1]) << 32) | (u128::from(m[2]) << 64) | (u128::from(m[3]) << 96);
            return Some(at + (!all).trailing_zeros() as usize);
        }
        at += 128;
    }
    while at + 32 <= n {
        // SAFETY: `at + 32 <= n`.
        let m = unsafe { same(pa.add(at), pb.add(at)) };
        if m != u32::MAX {
            return Some(at + (!m).trailing_zeros() as usize);
        }
        at += 32;
    }
    if at < n {
        let last = n - 32;
        // SAFETY: `n >= 32`, so `last + 32 == n`.
        let m = unsafe { same(pa.add(last), pb.add(last)) };
        if m != u32::MAX {
            return Some(last + (!m).trailing_zeros() as usize);
        }
    }
    None
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn sse2(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{__m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    /// A bit per byte in the low 16, set where the bytes are equal.
    #[inline]
    #[target_feature(enable = "sse2")]
    unsafe fn same(pa: *const u8, pb: *const u8) -> u16 {
        // SAFETY: the caller has 16 bytes at each pointer; the loads are unaligned.
        unsafe {
            _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_loadu_si128(pa.cast::<__m128i>()), _mm_loadu_si128(pb.cast::<__m128i>()))) as u16
        }
    }
    if n < 16 {
        return scalar(a, b, 0);
    }
    let mut at = 0;
    while at + 64 <= n {
        // SAFETY: `at + 64 <= n`, the length of both slices.
        let m = unsafe {
            [same(pa.add(at), pb.add(at)), same(pa.add(at + 16), pb.add(at + 16)),
             same(pa.add(at + 32), pb.add(at + 32)), same(pa.add(at + 48), pb.add(at + 48))]
        };
        if m[0] & m[1] & m[2] & m[3] != u16::MAX {
            let all = u64::from(m[0]) | (u64::from(m[1]) << 16) | (u64::from(m[2]) << 32) | (u64::from(m[3]) << 48);
            return Some(at + (!all).trailing_zeros() as usize);
        }
        at += 64;
    }
    while at + 16 <= n {
        // SAFETY: `at + 16 <= n`.
        let m = unsafe { same(pa.add(at), pb.add(at)) };
        if m != u16::MAX {
            return Some(at + (!m).trailing_zeros() as usize);
        }
        at += 16;
    }
    if at < n {
        let last = n - 16;
        // SAFETY: `n >= 16`, so `last + 16 == n`.
        let m = unsafe { same(pa.add(last), pb.add(last)) };
        if m != u16::MAX {
            return Some(last + (!m).trailing_zeros() as usize);
        }
    }
    None
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::aarch64::{
        uint8x16_t, vandq_u8, vceqq_u8, vget_lane_u64, vld1q_u8, vreinterpret_u64_u8, vreinterpretq_u16_u8, vshrn_n_u16,
    };
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn eq(pa: *const u8, pb: *const u8) -> uint8x16_t {
        // SAFETY: the caller has 16 bytes at each pointer; `vld1q_u8` has no alignment requirement.
        unsafe { vceqq_u8(vld1q_u8(pa), vld1q_u8(pb)) }
    }
    /// Four bits per byte (shift right and narrow), all set where the bytes are equal: the
    /// movemask NEON lacks, one instruction.
    #[inline]
    #[target_feature(enable = "neon")]
    fn same(eq: uint8x16_t) -> u64 {
        vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(eq))))
    }
    if n < 16 {
        return scalar(a, b, 0);
    }
    let mut at = 0;
    while at + 64 <= n {
        // SAFETY: `at + 64 <= n`, the length of both slices.
        let e = unsafe {
            [eq(pa.add(at), pb.add(at)), eq(pa.add(at + 16), pb.add(at + 16)),
             eq(pa.add(at + 32), pb.add(at + 32)), eq(pa.add(at + 48), pb.add(at + 48))]
        };
        if same(vandq_u8(vandq_u8(e[0], e[1]), vandq_u8(e[2], e[3]))) != u64::MAX {
            for (k, &e) in e.iter().enumerate() {
                let m = same(e);
                if m != u64::MAX {
                    return Some(at + 16 * k + (!m).trailing_zeros() as usize / 4);
                }
            }
            unreachable!("one of the four vectors differs");
        }
        at += 64;
    }
    while at + 16 <= n {
        // SAFETY: `at + 16 <= n`.
        let m = same(unsafe { eq(pa.add(at), pb.add(at)) });
        if m != u64::MAX {
            return Some(at + (!m).trailing_zeros() as usize / 4);
        }
        at += 16;
    }
    if at < n {
        let last = n - 16;
        // SAFETY: `n >= 16`, so `last + 16 == n`.
        let m = same(unsafe { eq(pa.add(last), pb.add(last)) });
        if m != u64::MAX {
            return Some(last + (!m).trailing_zeros() as usize / 4);
        }
    }
    None
}

/// A file's bytes, mapped read-only; an empty file maps nothing.
///
/// The file must not change while mapped: what is mapped here are the outputs and traces of
/// builds that have finished, which nothing writes while they are compared.
pub struct Mapped(Option<memmap2::Mmap>);

impl Mapped {
    pub fn open(path: &Path) -> io::Result<Mapped> {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Ok(Mapped(None));
        }
        // SAFETY: a read-only private map of a file nothing writes while it is mapped (the
        // type's contract above); `populate` faults it in at once, not a page per fault.
        let map = unsafe { memmap2::MmapOptions::new().populate().map(&file)? };
        Ok(Mapped(Some(map)))
    }

    pub fn bytes(&self) -> &[u8] {
        self.0.as_deref().unwrap_or(&[])
    }
}

/// Where two files first differ, or where the shorter ends; `None` when their bytes are the
/// same. Both are mapped and compared in place by [`fastest`].
pub fn first_file_difference(a: &Path, b: &Path) -> io::Result<Option<u64>> {
    let (a, b) = (Mapped::open(a)?, Mapped::open(b)?);
    Ok(first_difference(a.bytes(), b.bytes()).map(|at| at as u64))
}
