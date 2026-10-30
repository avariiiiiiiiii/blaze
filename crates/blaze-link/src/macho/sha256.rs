//! SHA-256 of the pages a code signature hashes (FIPS 180-4), with the processor's SHA-256
//! instructions where it has them: tens of megabytes of a large debug build are hashed on
//! every link.

const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub(super) fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut state = INITIAL;
    let whole = bytes.len() / 64 * 64;
    compress(&mut state, &bytes[..whole]);
    // The rest, the bit 1, zeros and the length in bits, in one or two blocks.
    let rest = &bytes[whole..];
    let mut tail = [0u8; 128];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let len = if rest.len() < 56 { 64 } else { 128 };
    tail[len - 8..len].copy_from_slice(&(bytes.len() as u64 * 8).to_be_bytes());
    compress(&mut state, &tail[..len]);
    let mut out = [0u8; 32];
    for (word, at) in state.iter().zip(out.chunks_exact_mut(4)) {
        at.copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn compress(state: &mut [u32; 8], blocks: &[u8]) {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("sha2") {
        // SAFETY: the processor has the SHA-256 instructions.
        unsafe { aarch64::compress(state, blocks) };
        return;
    }
    portable(state, blocks);
}

fn portable(state: &mut [u32; 8], blocks: &[u8]) {
    for block in blocks.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16].wrapping_add(s0).wrapping_add(w[t - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for t in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(choice).wrapping_add(K[t]).wrapping_add(w[t]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            (h, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(add);
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use std::arch::aarch64::{
        uint32x4_t, vaddq_u32, vld1q_u8, vld1q_u32, vreinterpretq_u32_u8, vrev32q_u8,
        vsha256h2q_u32, vsha256hq_u32, vsha256su0q_u32, vsha256su1q_u32, vst1q_u32,
    };

    use super::K;

    /// # Safety
    ///
    /// The processor has the SHA-256 instructions.
    #[target_feature(enable = "sha2")]
    pub(super) unsafe fn compress(state: &mut [u32; 8], blocks: &[u8]) {
        // SAFETY: every load and store is of 16 bytes within `state`, `blocks` and `K`.
        unsafe {
            let mut abcd = vld1q_u32(state.as_ptr());
            let mut efgh = vld1q_u32(state.as_ptr().add(4));
            for block in blocks.chunks_exact(64) {
                let (abcd_was, efgh_was) = (abcd, efgh);
                let load =
                    |at: usize| vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block[at..].as_ptr())));
                let mut words: [uint32x4_t; 4] = [load(0), load(16), load(32), load(48)];
                for group in 0..16 {
                    let at = group % 4;
                    if group >= 4 {
                        let early = vsha256su0q_u32(words[at], words[(at + 1) % 4]);
                        words[at] =
                            vsha256su1q_u32(early, words[(at + 2) % 4], words[(at + 3) % 4]);
                    }
                    let mixed = vaddq_u32(words[at], vld1q_u32(K[group * 4..].as_ptr()));
                    let was = abcd;
                    abcd = vsha256hq_u32(abcd, efgh, mixed);
                    efgh = vsha256h2q_u32(efgh, was, mixed);
                }
                abcd = vaddq_u32(abcd, abcd_was);
                efgh = vaddq_u32(efgh, efgh_was);
            }
            vst1q_u32(state.as_mut_ptr(), abcd);
            vst1q_u32(state.as_mut_ptr().add(4), efgh);
        }
    }
}

#[cfg(test)]
mod tests {
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|it| format!("{it:02x}")).collect()
    }

    #[test]
    fn known_digests() {
        let cases: [(&[u8], &str); 4] = [
            (b"", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            (b"abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
                "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(hex(&super::digest(input)), want, "{}", String::from_utf8_lossy(input));
        }
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&super::digest(&million)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn instructions_as_portable() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let bytes: Vec<u8> = (0..70_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        for len in [0, 64, 128, 4096, 16384, 69_952] {
            let (mut fast, mut slow) = (super::INITIAL, super::INITIAL);
            super::compress(&mut fast, &bytes[..len]);
            super::portable(&mut slow, &bytes[..len]);
            assert_eq!(fast, slow, "{len} bytes");
        }
    }
}
