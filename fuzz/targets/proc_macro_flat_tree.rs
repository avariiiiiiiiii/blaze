//! The token trees a proc-macro server (user code, in its sandbox) gives back for an expansion.
//! The input's first byte picks the protocol version, one of those the client speaks
//! (`RUST_ANALYZER_SPAN_SUPPORT` to `CURRENT_API_VERSION`, `4 + byte % 5`); the rest is the
//! postcard bytes of an answer's `FlatTree` then its span table. The target puts them in an
//! `ExpandMacro` answer with nothing tracked and reads it as `bidirectional_protocol::expand`
//! does: `<BidirectionalMessage as Message>::read`, `deserialize_span_data_index_map` of the
//! table, then `FlatTree::to_subtree_resolved` (proc-macro-api, `legacy_protocol/msg/flat.rs`,
//! whose two readers the legacy protocol's answers go through too). Both return `Result`: a
//! refusal is the reader doing its job. At `WIRE_TREES` (8) a server may still answer
//! `ExpandMacro`, whose `FlatTree` reads as at 7; the wire's own trees are
//! `proc_macro_tt_wire`'s.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Reading the tree returns (`Ok` or `Err`), and asks for at most `bound(len)` bytes at once:
//!   `shared::bound(len)`, and four token trees per byte of the input, since a one-byte
//!   `token_tree` entry can name a literal read as a minus and a literal, in a `Vec` that may
//!   double.
//! - A tree read, written again as the server's writer does (`FlatTree::from_subtree`,
//!   `serialize_span_data_index_map`, `Message::write`), reads back as an `ExpandMacro` answer
//!   the reader accepts, and at a version with literal kinds (`EXTENDED_LEAF_DATA` on) as the same tree, unless the
//!   writer reshapes it: an invisible group around only parentheses of its own span (a
//!   passed-on expression's, which the writer leaves out), or a number whose text still starts
//!   with a minus (which the reader splits off again).

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use proc_macro_api::{
    bidirectional_protocol::msg::{BidirectionalMessage, ExpandMacroResponse, Response},
    legacy_protocol::msg::{
        FlatTree, SpanDataIndexMap, deserialize_span_data_index_map, serialize_span_data_index_map,
    },
    version,
};

#[path = "proc_macro_shared.rs"]
mod shared;

const TARGET: Target = Target {
    name: "proc_macro_flat_tree",
    check,
    seeds,
    dictionary: &[
        // Tags of `token_tree` entries: a subtree, a literal, a punct and an ident.
        b"\x00",
        b"\x01",
        b"\x02",
        b"\x03",
        // `!0`, the literal without a suffix, and the subtree's unset range.
        b"\xff\xff\xff\xff\x0f",
        // Delimiter kinds and literal kinds past the last.
        b"\x04",
        b"\x0b",
        // A surrogate, which is no `char`.
        b"\x80\xb0\x03",
        // A span table's entry: file 0, ast id 0, `0..1`, context 0.
        b"\x00\x00\x00\x01\x00",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

/// The postcard bytes of an `ExpandMacro` answer up to its tree: `Response`, `ExpandMacro`, `Ok`.
const ANSWER: [u8; 3] = [1, 3, 0];
/// After its span table: no tracked variables and no tracked paths.
const UNTRACKED: [u8; 2] = [0, 0];

/// `RUST_ANALYZER_SPAN_SUPPORT` (4) to `WIRE_TREES` (8): five. The corpus's version bytes are 0
/// and 3, which pick 4 and 7 whether there are four versions or five.
const VERSIONS: u32 = version::CURRENT_API_VERSION - version::RUST_ANALYZER_SPAN_SUPPORT + 1;
const _: () = assert!(version::CURRENT_API_VERSION >= version::WIRE_TREES);

/// The reader builds the tree once, in preorder, into a `TopSubtreeBuilder` with room for one
/// node and span per `token_tree` entry. That linear build doesn't lower the bound: its reason
/// was never a second build but the minus a numeric literal's text splits off, two nodes for
/// one entry, so the builder's `spans` (each smaller than a `tt::TokenTree`) may double past
/// that room.
fn bound(len: usize) -> usize {
    shared::bound(len) + 4 * size_of::<tt::TokenTree>() * len
}

/// The tree of the answer in `frame` as the client reads it, `None` for a frame that is no
/// `ExpandMacro` answer or whose table or tree the client refuses.
fn decode(frame: &[u8], version: u32) -> Option<tt::TopSubtree> {
    let Ok(Some(BidirectionalMessage::Response(Response::ExpandMacro(Ok(answer))))) =
        shared::read_one(frame)
    else {
        return None;
    };
    let table = deserialize_span_data_index_map(&answer.span_data_table).ok()?;
    FlatTree::to_subtree_resolved(answer.tree, version, &table).ok()
}

/// The frame of the answer a server's writer makes of `tree`.
fn encode(tree: &tt::TopSubtree, version: u32) -> Vec<u8> {
    let mut table = SpanDataIndexMap::default();
    let flat = FlatTree::from_subtree(tree.view(), version, &mut table);
    shared::written(BidirectionalMessage::Response(Response::ExpandMacro(Ok(ExpandMacroResponse {
        tree: flat,
        span_data_table: serialize_span_data_index_map(&table),
        tracked_env_vars: Default::default(),
        tracked_paths: Default::default(),
    }))))
}

/// Whether the writer writes `tree` as another tree (see the invariants).
fn reshaped(tree: &tt::TopSubtree) -> bool {
    // alloc: only called when a tree read back differs, the edge before a crash report.
    let tokens: Vec<tt::TokenTree> = tree.as_token_trees().iter_flat_tokens().collect();
    tokens.iter().enumerate().any(|(at, token)| match token {
        tt::TokenTree::Subtree(outer) => {
            outer.delimiter.kind == tt::DelimiterKind::Invisible
                && matches!(tokens.get(at + 1), Some(tt::TokenTree::Subtree(inner))
                    if inner.delimiter.kind == tt::DelimiterKind::Parenthesis
                        && inner.delimiter.open == outer.delimiter.open
                        && inner.delimiter.close == outer.delimiter.open
                        && inner.len + 1 == outer.len)
        }
        tt::TokenTree::Leaf(tt::Leaf::Literal(literal)) => {
            matches!(literal.kind, tt::LitKind::Integer | tt::LitKind::Float)
                && literal.text().starts_with('-')
        }
        tt::TokenTree::Leaf(_) => false,
    })
}

fn check(data: &[u8]) {
    let Some((&pick, bytes)) = data.split_first() else { return };
    let version = version::RUST_ANALYZER_SPAN_SUPPORT + u32::from(pick) % VERSIONS;
    let frame = shared::cobs(&[&ANSWER[..], bytes, &UNTRACKED[..]].concat());

    let (most, tree) = blaze_fuzz::largest_allocation(|| decode(&frame, version));
    let bound = bound(data.len());
    assert!(most <= bound, "reading a tree of {} bytes asked for {most} at once, past {bound}", data.len());
    let Some(tree) = tree else { return };

    let again = decode(&encode(&tree, version), version)
        .unwrap_or_else(|| panic!("a tree written at version {version} reads back refused: {tree:?}"));
    if again != tree && version >= version::EXTENDED_LEAF_DATA && !reshaped(&tree) {
        assert_eq!(again, tree, "a tree written and read back at version {version} is the same");
    }
}

/// Every fixture tree, written at each version the client speaks.
fn seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    for tree in shared::trees() {
        for pick in 0..VERSIONS {
            let version = version::RUST_ANALYZER_SPAN_SUPPORT + pick;
            let raw = shared::raw(shared::read_one(&encode(&tree, version)).unwrap().unwrap());
            assert!(
                raw.starts_with(&ANSWER) && raw.ends_with(&UNTRACKED),
                "an answer with nothing tracked is its tree and table between `ANSWER` and `UNTRACKED`"
            );
            let mut seed = vec![pick as u8];
            seed.extend_from_slice(&raw[ANSWER.len()..raw.len() - UNTRACKED.len()]);
            seeds.push(seed);
        }
    }
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
