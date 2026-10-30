//! An assembled `asm!` block as the store keeps it (`crates/hir-ty-codegen/asm.rs`): a `u32`
//! code length, the code, then one 21-byte record per relocation (offset, kind, ELF type,
//! operand, addend). Read by `hir_ty_codegen::Assembled::from_bytes`, written by
//! `Assembled::to_bytes`. The bytes come from the store or another machine.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - `from_bytes` never panics or hangs;
//! - round trip: it gives `None`, or a value whose `to_bytes` is the input again;
//! - no allocation of the read is larger than [`bound`] of the input's length: the code is one
//!   copy of input bytes, and each 21-byte record one value of at most 32 bytes in a vector
//!   that at most doubles past what it holds.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use hir_ty_codegen::Assembled;

const TARGET: Target = Target {
    name: "codegen_asm",
    check,
    seeds,
    dictionary: &[
        b"\x00\x00\x00\x00",
        b"\x15\x00\x00\x00",
        b"\xff\xff\xff\xff",
        b"\x08",
        b"\x09",
        b"\x1e\x01\x00\x00",
        b"\x1f\x20\x03\xd5",
        b"\xe8\x00\x00\x00\x00",
    ],
    max_len: 1 << 12,
    budget: Duration::from_secs(300),
};

/// How long one relocation record is.
const RECORD: usize = 21;

/// The most one allocation of a read of `len` bytes may ask for: the code (at most `len`), or
/// the relocations' vector (under `64 * len / 21`, with room for its first four).
fn bound(len: usize) -> usize {
    4 * len + 1024
}

fn check(data: &[u8]) {
    let (largest, read) = blaze_fuzz::largest_allocation(|| Assembled::from_bytes(data));
    assert!(
        largest <= bound(data.len()),
        "allocation: reading {} bytes asked for {largest} bytes at once",
        data.len(),
    );
    if let Some(read) = read {
        assert!(read.to_bytes() == data, "round trip: a block read is not written as the bytes it was read from");
    }
}

/// A block of `code` and its relocations, each (offset, kind, ELF type, operand, addend), as
/// `to_bytes` lays it out.
fn block(code: &[u8], relocs: &[(u32, u8, u32, u32, i64)]) -> Vec<u8> {
    let mut out = (code.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(code);
    for &(offset, kind, elf_type, operand, addend) in relocs {
        out.extend_from_slice(&offset.to_le_bytes());
        out.push(kind);
        out.extend_from_slice(&elf_type.to_le_bytes());
        out.extend_from_slice(&operand.to_le_bytes());
        out.extend_from_slice(&addend.to_le_bytes());
    }
    out
}

/// The blocks `to_bytes` writes, each kind of relocation once.
fn valid() -> Vec<Vec<u8>> {
    // `R_AARCH64_LDST64_ABS_LO12_NC`, the one ELF type a stand-in kind keeps.
    const LDST64: u32 = 286;
    let nop = [0x1f, 0x20, 0x03, 0xd5];
    vec![
        block(&[], &[]),
        block(&nop, &[]),
        block(&[0xe8, 0, 0, 0, 0], &[(1, 1, 0, 0, -4)]),
        block(&[0x48, 0x8b, 0x05, 0, 0, 0, 0], &[(3, 2, 0, 1, -4)]),
        block(&[0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3], &[(3, 0, 0, 0, -4)]),
        block(&[0; 8], &[(0, 3, 0, 2, i64::MIN), (0, 3, 0, u32::MAX, i64::MAX)]),
        block(&[nop, nop, nop].concat(), &[(0, 5, 0, 0, 0), (4, 6, LDST64, 0, 8), (8, 4, 0, 1, 0)]),
        block(&[nop, nop].concat(), &[(0, 7, 0, 3, 0), (4, 8, 0, 3, 0)]),
        block(&nop, &[(u32::MAX, 4, 0, 0, 0)]),
    ]
}

/// The valid blocks, and each a byte short, a byte long, with a kind past the table and with a
/// code length past its end: what a damaged or a made-up entry looks like.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = valid();
    for bytes in valid() {
        out.push(bytes[..bytes.len() - 1].to_vec());
        out.push([&bytes[..], &[0]].concat());
        let mut long = bytes.clone();
        long[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        out.push(long);
        let code = u32::from_le_bytes(bytes[..4].try_into().expect("four bytes")) as usize;
        if bytes.len() >= 4 + code + RECORD {
            let mut kind = bytes.clone();
            kind[4 + code + 4] = 9;
            out.push(kind);
        }
    }
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use hir_ty_codegen::Assembled;

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    /// The hand-built blocks are what `to_bytes` writes, so they keep reaching the records.
    #[test]
    fn valid_blocks_read_back() {
        for bytes in super::valid() {
            let read = Assembled::from_bytes(&bytes).expect("a block laid out as `to_bytes` writes one");
            assert_eq!(read.to_bytes(), bytes);
        }
    }
}
