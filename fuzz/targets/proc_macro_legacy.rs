//! What a proc-macro server of the legacy protocol sends the client: the input is the bytes on
//! its stdout, lines of JSON, read one answer after another with
//! `<legacy_protocol::msg::Response as Message>::read` and one buffer, as `send_request`
//! (proc-macro-api, `legacy_protocol.rs`) reads them. A line that doesn't start with `{` is a
//! macro printing to stdout and is passed over; an empty line ends the answers.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - The stream ends (`Ok(None)`) after at most one read per byte: every read takes a line of at
//!   least one byte.
//! - Read in pieces (a byte at a time, two, uneven ones, large then small, or pieces the
//!   input's first bytes say; one of them a run, `shared::sizes`) it gives what it gives whole.
//! - Reading the stream whole asks for at most `blaze_fuzz::allowed(len)` bytes at once: the
//!   legacy answers have no hash table, and JSON promises no lengths.
//! - A last line with no newline, reached before any empty line, is read as the error that says
//!   so, never as an answer: it may be one cut short
//!   (`fuzz/corpus/proc_macro_legacy/api-version-check-with-no-newline.hex`).
//! - An answer read is written (`Message::write`) as one line, ended by its only newline, which
//!   reads back to an equal answer.

#![forbid(unsafe_code)]

use std::{io::BufRead, time::Duration};

use blaze_fuzzer::Target;
use proc_macro_api::{
    ProcMacroKind,
    legacy_protocol::msg::{
        ExpandMacroExtended, FlatTree, Message, PanicMessage, Response, ServerConfig,
        SpanDataIndexMap, SpanMode, serialize_span_data_index_map,
    },
    version,
};

#[path = "proc_macro_shared.rs"]
mod shared;

const TARGET: Target = Target {
    name: "proc_macro_legacy",
    check,
    seeds,
    dictionary: &[
        b"\n",
        b"{\"ApiVersionCheck\":",
        b"{\"ListMacros\":",
        b"{\"ExpandMacro\":",
        b"{\"SetConfig\":",
        b"{\"ExpandMacroExtended\":",
        b"{\"ExpandMacroWire\":",
        b"{\"Ok\":",
        b"{\"Err\":",
        b"\"subtree\":[",
        b"\"literal\":[",
        b"\"punct\":[",
        b"\"ident\":[",
        b"\"token_tree\":[",
        b"\"text\":[",
        b"\"tree\":",
        b"\"span_data_table\":[",
        b"\"span_mode\":",
        b"\"RustAnalyzer\"",
        b"\"Id\"",
        b"\"CustomDerive\"",
        b"\"Attr\"",
        b"\"FuncLike\"",
        b"\"Bang\"",
        b"4294967295",
        b"\\u0000",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// What one read of the stream gave: an answer, or the error.
#[derive(Debug, PartialEq)]
enum Read {
    Answer(Response),
    Broken(String),
}

fn written(response: Response) -> Vec<u8> {
    // alloc: the line `Message::write` makes, which the check reads back.
    let mut line = Vec::new();
    response.write(&mut line).expect("an answer is written to memory");
    line
}

/// Every answer `reader` gives until the stream ends, read into one buffer.
// dyn: `Message::read` takes `&mut dyn BufRead`.
fn read_all(reader: &mut dyn BufRead, len: usize) -> Vec<Read> {
    // alloc: the one buffer the stream is read into, as the client keeps one.
    let mut buf = String::new();
    // alloc: the reads are what the check compares; a few doublings a run.
    let mut answers = Vec::new();
    while answers.len() <= len {
        match <Response as Message>::read(reader, &mut buf) {
            Ok(None) => return answers,
            Ok(Some(response)) => answers.push(Read::Answer(response)),
            // alloc: an error is compared by its text; `io::Error` has no `PartialEq`.
            Err(error) => answers.push(Read::Broken(error.to_string())),
        }
    }
    panic!("a stream of {len} bytes gave {} answers and has not ended", answers.len());
}

fn check(data: &[u8]) {
    let (most, whole) = blaze_fuzz::largest_allocation(|| {
        let mut reader = data;
        read_all(&mut reader, data.len())
    });
    let bound = blaze_fuzz::allowed(data.len());
    assert!(most <= bound, "reading a stream of {} bytes asked for {most} at once, past {bound}", data.len());

    let mut own = [0; 8];
    let sizes = shared::sizes(data, &mut own);
    let pieces = read_all(&mut shared::Pieces::new(data, sizes), data.len());
    assert_eq!(pieces, whole, "the stream read in pieces of {sizes:?} gives what it does whole");

    let unended = data.last().is_some_and(|it| *it != b'\n');
    let stopped = data.first() == Some(&b'\n') || data.windows(2).any(|it| it == b"\n\n");
    if unended && !stopped {
        assert!(
            matches!(whole.last(), Some(Read::Broken(why)) if why.contains("no newline")),
            "a last line with no newline is read as that error, not {:?}",
            whole.last()
        );
    }

    // `pieces` equals `whole`, so its answers are written and read back to be compared with
    // `whole`'s: `Message::write` takes the answer by value.
    // alloc: one buffer for every line read back; it grows only on a read.
    let mut buf = String::new();
    for (read, said) in pieces.into_iter().zip(&whole) {
        let Read::Answer(answer) = read else { continue };
        let line = written(answer);
        let text = || String::from_utf8_lossy(&line); // alloc: called only in a failing assert.
        assert_eq!(
            line.iter().position(|it| *it == b'\n'),
            Some(line.len() - 1),
            "an answer is written as one line, ended by its only newline: {}",
            text()
        );
        let mut reader = line.as_slice();
        match <Response as Message>::read(&mut reader, &mut buf) {
            Ok(Some(again)) => {
                assert_eq!(Read::Answer(again), *said, "an answer written reads back the same: {}", text())
            }
            other => panic!("the line {} reads back as {other:?}", text()),
        }
    }
}

/// Every kind of answer, with the token trees of the oldest and the newest version a server
/// may speak: `FlatTree`s, and at `WIRE_TREES` the wire's bytes.
fn responses() -> Vec<Response> {
    let mut responses = vec![
        Response::ApiVersionCheck(version::CURRENT_API_VERSION),
        Response::ListMacros(Ok(vec![
            ("Derive".to_owned(), ProcMacroKind::CustomDerive),
            ("attr".to_owned(), ProcMacroKind::Attr),
            ("bang".to_owned(), ProcMacroKind::Bang),
        ])),
        Response::ListMacros(Err("the library can't be loaded".to_owned())),
        Response::SetConfig(ServerConfig { span_mode: SpanMode::RustAnalyzer }),
        Response::ExpandMacro(Err(PanicMessage("panicked at \"ünï\"\n\u{1}".to_owned()))),
    ];
    for tree in shared::trees() {
        for version in [version::RUST_ANALYZER_SPAN_SUPPORT, version::CURRENT_API_VERSION] {
            let mut table = SpanDataIndexMap::default();
            let flat = FlatTree::from_subtree(tree.view(), version, &mut table);
            responses.push(Response::ExpandMacro(Ok(flat.clone())));
            responses.push(Response::ExpandMacroExtended(Ok(ExpandMacroExtended {
                tree: flat,
                span_data_table: serialize_span_data_index_map(&table),
            })));
        }
        let mut wire = Vec::new();
        tree.write_wire(&mut wire);
        responses.push(Response::ExpandMacroWire(Ok(wire)));
    }
    responses.push(Response::ExpandMacroWire(Err(PanicMessage("the macro panicked".to_owned()))));
    responses
}

/// Streams as servers write them: every answer in turn, a macro's prints to stdout between
/// them, an empty line before the last, an answer with no newline at the end (an error), and
/// lines that aren't JSON or UTF-8.
fn seeds() -> Vec<Vec<u8>> {
    let lines: Vec<Vec<u8>> = responses().into_iter().map(written).collect();
    let every = lines.concat();
    let mut printed = Vec::new();
    for line in &lines[..4] {
        printed.extend_from_slice(b"expanding...\n");
        printed.extend_from_slice(line);
    }
    let mut ended = lines[0].clone();
    ended.push(b'\n');
    ended.extend_from_slice(&lines[1]);
    let mut unended = lines[1].clone();
    unended.pop();
    let broken = b"{\"ApiVersionCheck\":\xff}\n{not json}\n{\"ApiVersionCheck\":4294967296}\n".to_vec();
    vec![every, printed, ended, unended, broken]
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
