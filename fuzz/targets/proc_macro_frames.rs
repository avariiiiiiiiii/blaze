//! The stream of frames a proc-macro server (user code, in its sandbox) sends the client: the
//! input is the bytes on the wire, COBS frames each ended by a 0, read one message after another
//! with `<BidirectionalMessage as Message>::read` and one buffer, as `run_conversation`
//! (proc-macro-api, `bidirectional_protocol.rs`) reads a native server's stdout, and through
//! `warm::LinkReader` as it reads a warm server's link.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - The stream ends (`Ok(None)`) after at most one read per byte: every read takes a frame of
//!   at least one byte.
//! - Read whole, the stream gives what a plain walk of it gives: each frame up to and with its
//!   0, then what is left after the last 0, each read on its own, in order (a message, or the
//!   error its bytes are).
//! - A stream that doesn't end in a 0 ends inside a frame, and its last read is the error that
//!   says so, never a message or the stream's end.
//! - Read through a `LinkReader` whose link gives the stream in pieces (a byte at a time, two,
//!   uneven ones, large then small, or pieces the input's first bytes say; one of them a run,
//!   `shared::sizes`) it gives the same.
//! - Reading the stream whole asks for at most `shared::bound(len)` bytes at once.

#![forbid(unsafe_code)]

use std::{
    io::{self, BufRead},
    sync::mpsc,
    time::Duration,
};

use blaze_core::{Completion, Conn, Done, Effect, IoError};
use blaze_fuzzer::Target;
use proc_macro_api::{
    bidirectional_protocol::msg::BidirectionalMessage,
    legacy_protocol::msg::Message,
    warm::{Bridge, LinkReader},
};

#[path = "proc_macro_shared.rs"]
mod shared;

const TARGET: Target = Target {
    name: "proc_macro_frames",
    check,
    seeds,
    dictionary: &[b"\x00", b"\x01\x00", b"\x04\x01\x01\x07\x00", b"\xff", b"\x0a"],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// What one read of the stream gave.
#[derive(Debug, PartialEq)]
enum Read {
    Message(BidirectionalMessage),
    Broken(String),
}

fn outcome(read: io::Result<Option<BidirectionalMessage>>) -> Option<Read> {
    match read {
        Ok(None) => None,
        Ok(Some(message)) => Some(Read::Message(message)),
        // alloc: an error is compared by its text; `io::Error` has no `PartialEq`.
        Err(error) => Some(Read::Broken(error.to_string())),
    }
}

/// Every frame a [`LinkReader`] gives of a server's link that hands out `stream` in pieces of
/// `sizes` in turn: a thread plays the daemon's driver, answering each [`Bridge`] receive with
/// the next piece appended to its buffer. Only the reading half is used.
fn trickled(stream: &[u8], sizes: &[usize]) -> Vec<Read> {
    let (woken, wakes) = mpsc::channel::<()>();
    // dyn: `Bridge::new` takes the driver's waker boxed, once a run.
    let (mut bridge, link) = Bridge::new(Conn(1), 0, Box::new(move || {
        _ = woken.send(());
    }));
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let (mut at, mut piece) = (0, 0);
            // alloc: the driver's effects, one in flight at a time.
            let mut effects = Vec::new();
            // The link, and the waker with it, drop once the reader is done: then this ends.
            while wakes.recv().is_ok() {
                bridge.effects(&mut effects);
                for effect in effects.drain(..) {
                    let tag = effect.tag();
                    let done = match effect {
                        Effect::Recv { most, mut into, .. } => {
                            let end = (at + sizes[piece % sizes.len()].min(most)).min(stream.len());
                            piece += 1;
                            into.extend_from_slice(&stream[at..end]);
                            at = end;
                            Done::Read(into)
                        }
                        Effect::Send { mut bytes, .. } => {
                            bytes.clear();
                            // alloc: error path, a send the target's link refuses.
                            let error = IoError("the target's link is only read".to_owned());
                            Done::Broken { buffer: bytes, error }
                        }
                        other => panic!("a bridge asks to send and receive, not {other:?}"),
                    };
                    bridge.complete(Completion { tag, done });
                }
            }
        });
        read_all(&mut LinkReader::new(link), stream.len())
    })
}

/// Every frame `reader` gives until the stream ends, read into one buffer.
// dyn: `Message::read` takes `&mut dyn BufRead`.
fn read_all(reader: &mut dyn BufRead, len: usize) -> Vec<Read> {
    // alloc: the one buffer the stream is read into, as the client keeps one.
    let mut buf = Vec::new();
    // alloc: the reads are what the check compares; a few doublings a run.
    let mut frames = Vec::new();
    while frames.len() <= len {
        match outcome(<BidirectionalMessage as Message>::read(reader, &mut buf)) {
            Some(frame) => frames.push(frame),
            None => return frames,
        }
    }
    panic!("a stream of {len} bytes gave {} frames and has not ended", frames.len());
}

/// The frames a plain walk of `stream` finds, each read on its own.
fn walk(stream: &[u8]) -> Vec<Read> {
    stream
        .split_inclusive(|it| *it == 0)
        .map(|frame| {
            outcome(shared::read_one(frame))
                .unwrap_or_else(|| panic!("a frame of {} bytes read as the stream's end", frame.len()))
        })
        .collect() // alloc: the reference the reads are compared with.
}

fn check(data: &[u8]) {
    let expected = walk(data);

    let (most, whole) = blaze_fuzz::largest_allocation(|| {
        let mut reader = data;
        read_all(&mut reader, data.len())
    });
    let bound = shared::bound(data.len());
    assert!(most <= bound, "reading a stream of {} bytes asked for {most} at once, past {bound}", data.len());
    assert_eq!(whole, expected, "the stream read whole gives its frames, each as read on its own");
    if data.last().is_some_and(|it| *it != 0) {
        assert!(
            matches!(whole.last(), Some(Read::Broken(why)) if why.contains("no terminating zero")),
            "a stream ending inside a frame ends with that error, not {:?}",
            whole.last()
        );
    }

    let mut own = [0; 8];
    let sizes = shared::sizes(data, &mut own);
    let pieces = trickled(data, sizes);
    assert_eq!(pieces, expected, "the stream read from a link in pieces of {sizes:?} gives what it does whole");
}

/// Streams as servers write them: every kind of message in turn, a conversation's sub-request
/// then its answer, a stream torn in its last frame (an error), empty frames, and a server's
/// print to stdout in front of its frames.
fn seeds() -> Vec<Vec<u8>> {
    let every: Vec<u8> = shared::messages().into_iter().flat_map(shared::written).collect();
    let conversation: Vec<u8> = shared::messages()
        .into_iter()
        .filter(|it| {
            matches!(it, BidirectionalMessage::SubRequest(_) | BidirectionalMessage::Response(_))
        })
        .take(8)
        .flat_map(shared::written)
        .collect();
    let mut torn = conversation.clone();
    torn.truncate(torn.len() - 2);
    let mut printed = b"debug: expanding\n".to_vec();
    printed.extend(&conversation);
    vec![every, conversation, torn, vec![0, 0, 1, 0], printed]
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
