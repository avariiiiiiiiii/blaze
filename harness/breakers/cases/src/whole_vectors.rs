//! An array of 16 bytes of lanes, and the result of a `simd_shuffle` of 16 bytes, are stored as
//! one vector, and a SIMD vector or array is copied in whole vectors: a value written in
//! words and read whole is a load the processor can't forward, which made sha2's SHA-NI rounds
//! (blaze's own `Digest::of`) run at a ninth of rustc's unoptimized speed. Every lane width
//! and kind goes through the packing.

use std::hint::black_box;

fn lanes() -> ([u8; 16], [u16; 8], [i32; 4], [f32; 4], [f64; 2], [bool; 16], [char; 4]) {
    let (a, b) = (black_box(0x9e_u8), black_box(-3.25f32));
    (
        [a, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, a.wrapping_mul(3)],
        [u16::from(a), 0x8000, 0xffff, 3, 4, 5, 6, 0x1234],
        [i32::MIN, -1, i32::from(a), i32::MAX],
        [b, -0.0, f32::INFINITY, f32::from_bits(0x7fc0_0001)],
        [f64::from(b), f64::MIN_POSITIVE],
        std::array::from_fn(|it| it % 3 == usize::from(a) % 3),
        ['a', char::from(a), '\u{10ffff}', 'é'],
    )
}

#[cfg(target_arch = "x86_64")]
fn shuffled(words: [i32; 4]) -> Vec<[i32; 4]> {
    use std::arch::x86_64::*;
    unsafe {
        let a: __m128i = std::mem::transmute(words);
        let b = _mm_set_epi32(40, 30, 20, 10);
        [
            _mm_shuffle_epi32::<0b00_01_10_11>(a),
            _mm_alignr_epi8::<4>(a, b),
            _mm_unpacklo_epi32(a, b),
            _mm_add_epi32(a, b),
            _mm_castps_si128(_mm_shuffle_ps::<0b01_00_11_10>(_mm_castsi128_ps(a), _mm_castsi128_ps(b))),
            black_box(in_c as Mixed)(7, a, 0.5, _mm_castsi128_ps(b), a, b, a, b, a, b, _mm_set1_epi32(-9)),
        ]
        .iter()
        .map(|it| std::mem::transmute(*it))
        .collect()
    }
}

/// Vectors in the C ABI, as wasmtime's libcalls take them: each in an XMM register until the
/// eight are taken (`x` takes one), the last two in memory.
#[cfg(target_arch = "x86_64")]
#[allow(improper_ctypes_definitions)]
type Mixed = extern "C" fn(
    i32,
    std::arch::x86_64::__m128i,
    f64,
    std::arch::x86_64::__m128,
    std::arch::x86_64::__m128i,
    std::arch::x86_64::__m128i,
    std::arch::x86_64::__m128i,
    std::arch::x86_64::__m128i,
    std::arch::x86_64::__m128i,
    std::arch::x86_64::__m128i,
    std::arch::x86_64::__m128i,
) -> std::arch::x86_64::__m128i;

#[cfg(target_arch = "x86_64")]
#[allow(improper_ctypes_definitions, clippy::too_many_arguments)]
extern "C" fn in_c(
    n: i32,
    a: std::arch::x86_64::__m128i,
    x: f64,
    b: std::arch::x86_64::__m128,
    c: std::arch::x86_64::__m128i,
    d: std::arch::x86_64::__m128i,
    e: std::arch::x86_64::__m128i,
    f: std::arch::x86_64::__m128i,
    g: std::arch::x86_64::__m128i,
    h: std::arch::x86_64::__m128i,
    i: std::arch::x86_64::__m128i,
) -> std::arch::x86_64::__m128i {
    use std::arch::x86_64::*;
    unsafe {
        let sum = [c, d, e, f, g, h].into_iter().fold(_mm_set1_epi32(n + x as i32), |it, v| _mm_add_epi32(it, v));
        _mm_xor_si128(_mm_mullo_epi16(sum, a), _mm_xor_si128(_mm_castps_si128(b), _mm_slli_epi32::<3>(i)))
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn shuffled(_: [i32; 4]) -> Vec<[i32; 4]> {
    Vec::new()
}

pub fn run() -> (String, Vec<[i32; 4]>) {
    let (bytes, halves, words, floats, doubles, flags, chars) = black_box(lanes());
    let bits = floats.map(f32::to_bits);
    let seen = format!("{bytes:?} {halves:?} {words:?} {bits:?} {doubles:?} {flags:?} {chars:?}");
    (seen, shuffled(words))
}
