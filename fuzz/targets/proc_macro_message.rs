//! One message a proc-macro server (user code, in its sandbox) sends the client: the input is
//! the postcard bytes of a `BidirectionalMessage`, which the target puts in one COBS frame and
//! reads as the client reads a warm or a native server's answers and sub-requests,
//! `<BidirectionalMessage as Message>::read` (proc-macro-api, `bidirectional_protocol/msg.rs`).
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Reading returns `Ok` or `Err` and asks for at most `shared::bound(len)` bytes at once:
//!   `blaze_fuzz::allowed(len)`, with a hash table's reservation for a promised length
//!   (`shared::TABLE`, 2.25 MiB) in place of its megabyte.
//! - A message read is written (`Message::write`) as one frame, ended by its only 0, and that
//!   frame reads back to an equal message (`==`, whose hash tables compare in any order). It is
//!   told by writing it again: `Message::write` takes the message by value, and a second write
//!   and a `memcmp` cost less than a second read of the input and a `==`. Only where the frames
//!   differ (a hash table's entries in another order) are the two messages read again and
//!   compared.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use proc_macro_api::bidirectional_protocol::msg::BidirectionalMessage;

#[path = "proc_macro_shared.rs"]
mod shared;

const TARGET: Target = Target {
    name: "proc_macro_message",
    check,
    seeds,
    dictionary: &[
        // `Request` and `ExpandMacroWire`, then `Response` and each answer's variant, the last
        // `ExpandMacroWire` (`version::WIRE_TREES`).
        b"\x00\x04",
        b"\x01\x00",
        b"\x01\x01",
        b"\x01\x02",
        b"\x01\x03",
        b"\x01\x04",
        // `SubRequest`, then each sub-request's variant.
        b"\x02\x00",
        b"\x02\x05",
        b"\x02\x07",
        // `Ok`, `Err`, `None`, `Some`.
        b"\x00",
        b"\x01",
        // The longest `u32` and `u64` varints.
        b"\xff\xff\xff\xff\x0f",
        b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

fn check(data: &[u8]) {
    let frame = shared::cobs(data);
    let (most, read) = blaze_fuzz::largest_allocation(|| shared::read_one(&frame));
    let bound = shared::bound(data.len());
    assert!(most <= bound, "reading a message of {} bytes asked for {most} at once, past {bound}", data.len());
    let Ok(Some(message)) = read else { return };

    let written = shared::written(message);
    assert_eq!(
        written.iter().position(|it| *it == 0),
        Some(written.len() - 1),
        "a message is written as one frame, ended by its only 0: {written:?}"
    );
    let rewritten = shared::written(says(&written));
    if rewritten != written {
        assert_eq!(says(&written), says(&frame), "a message written reads back the same");
    }
}

/// The message `frame` reads as, which it does: it was read once already.
fn says(frame: &[u8]) -> BidirectionalMessage {
    match shared::read_one(frame) {
        Ok(Some(message)) => message,
        other => panic!("the frame {frame:?} reads as {other:?}"),
    }
}

/// The postcard bytes of every kind of message, as the crate writes them.
fn seeds() -> Vec<Vec<u8>> {
    shared::messages().into_iter().map(shared::raw).collect()
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
