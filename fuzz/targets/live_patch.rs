//! What the daemon writes a v1 program, through blaze-live's `format::Patch::decode`, then
//! `blaze_live::fuzz_checks`, the checks each platform's `apply` makes of a decoded patch
//! before it maps or writes anything. The program trusts no byte of a patch: it comes from
//! another process. The checks run against a fixed image and room, at three slides.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Neither panics, on any bytes.
//! - `decode` asks for at most `blaze_fuzz::allowed(len)` bytes at once: no count is believed
//!   before its bytes are there.
//! - A patch `decode` accepts encodes to bytes of its `encoded_len` that decode to it again.
//! - Code the checks accept is laid out in whole pages within the process's room, and every
//!   write over the executable is inside the image.

#![forbid(unsafe_code)]

use std::{ops::Range, time::Duration};

use blaze_fuzzer::Target;
use blaze_live::format::{Body, Code, Patch, Process, Write};

const PAGE: u64 = 1 << 14;
const IMAGE: Range<u64> = 0x1_0000_0000..0x1_0100_0000;
const ROOM: Range<u64> = 0x2_0000_0000..0x2_0400_0000;

const TARGET: Target = Target {
    name: "live_patch",
    check,
    seeds,
    dictionary: &[
        b"BLAZELV1",
        b"\x00\x00\x00\x00\x01\x00\x00\x00",
        b"\x00\x00\x00\x00\x02\x00\x00\x00",
        b"\x00\x40\x00\x00\x00\x00\x00\x00",
        b"\x01\x00\x00\x00\x00\x00\x00\x00",
        b"\x00\x00\x00\x00\x00\x00\x00\x00",
    ],
    max_len: 1 << 17,
    budget: Duration::from_secs(300),
};

fn check(data: &[u8]) {
    let (most, decoded) = blaze_fuzz::largest_allocation(|| Patch::decode(data));
    assert!(
        most <= blaze_fuzz::allowed(data.len()),
        "decoding {} bytes asked for {most} at once",
        data.len()
    );
    let Ok(patch) = decoded else { return };
    let bytes = patch.encode();
    assert_eq!(bytes.len(), patch.encoded_len(), "a patch's encoded_len is its encoding's");
    assert_eq!(Patch::decode(&bytes).as_ref(), Ok(&patch), "a decoded patch encodes to itself");
    let Body::Code(code) = &patch.body else { return };
    let process = Process { pid: 1, uuid: patch.uuid, started: 0, next: ROOM.start, end: ROOM.end };
    for slide in [0, PAGE, 0u64.wrapping_sub(PAGE)] {
        let Ok((end, _)) = blaze_live::fuzz_checks(code, process, slide, IMAGE, PAGE) else {
            continue;
        };
        assert!(
            code.base >= ROOM.start && code.base <= end && end <= ROOM.end,
            "laid out in the room"
        );
        assert!(
            code.base.is_multiple_of(PAGE) && end.is_multiple_of(PAGE),
            "laid out in whole pages"
        );
        for write in &code.writes {
            let start = write.address.checked_add(slide);
            let last = start.and_then(|it| it.checked_add(write.bytes.len() as u64));
            assert!(
                start.is_some_and(|it| it >= IMAGE.start) && last.is_some_and(|it| it <= IMAGE.end),
                "a write in the image"
            );
        }
    }
}

/// Patches as the daemon writes them: one with code, unwind tables, pointers, writes over the
/// image and a static's first value, one that only puts back, a restart; and the ways bytes
/// are not one.
fn seeds() -> Vec<Vec<u8>> {
    let mut text = vec![0u8; PAGE as usize];
    text[8..16].copy_from_slice(&IMAGE.start.to_le_bytes());
    let code = Code {
        base: ROOM.start,
        text,
        data: vec![1; 64],
        frames: (0, 4),
        pointers: vec![8],
        writes: vec![
            Write {
                address: IMAGE.start + 0x100,
                bytes: vec![0xe9, 0, 0, 0, 0],
                pointers: Vec::new(),
            },
            Write {
                address: IMAGE.start + 0x8000,
                bytes: ROOM.start.to_le_bytes().to_vec(),
                pointers: vec![0],
            },
        ],
        redirected: 1,
        statics: 1,
        lasting: vec![Write { address: ROOM.start + PAGE, bytes: vec![0; 8], pointers: vec![0] }],
    };
    let patches = [
        Patch { uuid: [7; 16], sequence: 1, body: Body::Code(code) },
        Patch {
            uuid: [7; 16],
            sequence: 2,
            body: Body::Code(Code { base: ROOM.start, ..Code::default() }),
        },
        Patch { uuid: [7; 16], sequence: 3, body: Body::Restart("a layout changed".into()) },
    ];
    let mut seeds: Vec<Vec<u8>> = patches.iter().map(Patch::encode).collect();
    let whole = patches[0].encode();
    seeds.push(whole[..whole.len() - 3].to_vec());
    seeds.push([&whole[..], &[0]].concat());
    seeds.push([&b"BLAZELV0"[..], &whole[8..]].concat());
    seeds
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
