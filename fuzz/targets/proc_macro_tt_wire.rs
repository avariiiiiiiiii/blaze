//! A token tree as its columns' bytes, the wire format the client is to read a proc-macro
//! server's (user code, in its sandbox) answers in (v2/requests/usercode.md,
//! "frontend-tokens"): `tt::TopSubtree::read_wire` (`crates/tt/src/storage/wire.rs`), with a
//! part callback that refuses what this side can't have sent.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Reading returns `Ok` or `Err` and asks for at most `blaze_fuzz::allowed(len)` bytes at
//!   once (the reader says it allocates a constant times its bytes).
//! - A tree read took whole words of the input, at most all of it, and `write_wire` of it is
//!   exactly the bytes it took.
//! - A tree read is the tree the builder makes of its token trees (`from_serialized`), so it
//!   compares and hashes as one built.
//! - A strict prefix of the bytes a tree took is refused as `Truncated`: one prefix a run, as
//!   long as the byte after the tree says (that many 256ths of it), or all but the last byte
//!   when nothing follows. One read of a prefix costs up to a read of the tree, so this is one
//!   extra read a run, and the engine's mutations of that byte reach every prefix.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use span::{
    Edition, EditionedFileId, ErasedFileAstId, FileId, Span, SpanAnchor, SyntaxContext, TextRange,
    TextSize,
};
use tt::{
    Delimiter, DelimiterKind, Ident, LitKind, Literal, Punct, Spacing, TopSubtree,
    TopSubtreeBuilder, WireError, WirePart,
};

#[path = "proc_macro_shared.rs"]
mod shared;

const TARGET: Target = Target {
    name: "proc_macro_tt_wire",
    check,
    seeds,
    dictionary: &[
        b"tt\x01\x00",
        // The root contexts of edition 2024 and 2015.
        b"\x00\xff\xff\xff",
        b"\x03\xff\xff\xff",
        // `WIDE` and `NARROW_LIMIT`.
        b"\xff\xff\xff\xff",
        b"\xff\xff\x00\x00",
        b"\x00\x00\x00\x00",
        b"\x01\x00\x00\x00",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const EDITIONS: [Edition; 4] =
    [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021, Edition::Edition2024];

/// A part as this side reads it: any AST id, a file id `EditionedFileId` can be, and only an
/// edition's root context, the only contexts a reader without a database has.
fn part(wire: WirePart) -> Result<(SpanAnchor, SyntaxContext), String> {
    let edition = (wire.file_id & EditionedFileId::EDITION_MASK) >> EditionedFileId::FILE_ID_BITS;
    if wire.file_id & EditionedFileId::RESERVED_MASK != 0 || edition > Edition::LATEST as u32 {
        // alloc: the error path; `read_wire`'s callback reports why as a `String`.
        return Err(format!("no file is {:#x}", wire.file_id));
    }
    let Some(ctx) = EDITIONS.map(SyntaxContext::root).into_iter().find(|it| it.into_u32() == wire.ctx)
    else {
        // alloc: the error path; `read_wire`'s callback reports why as a `String`.
        return Err(format!("context {} was not sent", wire.ctx));
    };
    let anchor = SpanAnchor {
        file_id: EditionedFileId::from_raw(wire.file_id),
        ast_id: ErasedFileAstId::from_raw(wire.ast_id),
    };
    Ok((anchor, ctx))
}

fn read(bytes: &[u8]) -> Result<(TopSubtree, usize), WireError> {
    let mut resolve = part;
    TopSubtree::read_wire(bytes, &mut resolve)
}

fn check(data: &[u8]) {
    let (most, read_whole) = blaze_fuzz::largest_allocation(|| read(data));
    let bound = blaze_fuzz::allowed(data.len());
    assert!(most <= bound, "reading a tree of {} bytes asked for {most} at once, past {bound}", data.len());
    let Ok((tree, used)) = read_whole else { return };

    assert!(used <= data.len() && used % 4 == 0, "a tree took {used} of {} bytes", data.len());
    let mut written = Vec::with_capacity(used);
    tree.write_wire(&mut written);
    assert_eq!(written, data[..used], "a tree read writes as the bytes it took: {tree:?}");

    // alloc: `from_serialized` takes its token trees as a `Vec`.
    let built = TopSubtree::from_serialized(tree.as_token_trees().iter_flat_tokens().collect());
    assert_eq!(built, tree, "a tree read is the tree the builder makes of its token trees");

    let cut = match data.get(used) {
        Some(&at) => used * usize::from(at) / 256,
        None => used - 1,
    };
    let refused = read(&data[..cut]);
    assert!(
        matches!(refused, Err(WireError::Truncated { .. })),
        "the first {cut} of the {used} bytes of a tree read as {refused:?}"
    );
}

/// A span of `edition`'s file 7, at `start` and `len` long, of AST id `ast` and the edition's
/// root context.
fn spanned(edition: Edition, ast: u32, start: u32, len: u32) -> Span {
    Span {
        range: TextRange::at(TextSize::new(start), TextSize::new(len)),
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(7), edition),
            ast_id: ErasedFileAstId::from_raw(ast),
        },
        ctx: SyntaxContext::root(edition),
    }
}

/// The shared fixture trees, and one of spans of every edition, AST ids, wide spans (as long
/// as a packed one can't be) and a literal of a long suffix.
fn seeds() -> Vec<Vec<u8>> {
    let mut parts = TopSubtreeBuilder::new(Delimiter {
        open: spanned(Edition::Edition2015, 0, 0, 1),
        close: spanned(Edition::Edition2024, 0, 200_000, 1),
        kind: DelimiterKind::Parenthesis,
    });
    for (at, edition) in EDITIONS.into_iter().enumerate() {
        let at = at as u32;
        parts.push(Ident::new("r#match", spanned(edition, at, 10 * at, 7)).into());
        parts.open(DelimiterKind::Bracket, spanned(edition, at + 1, 10 * at + 7, 1));
        let wide = spanned(edition, at, 0, 0x1_0000);
        parts.push(Punct { char: '\u{1F980}', spacing: Spacing::Joint, span: wide }.into());
        parts.close(spanned(edition, u32::MAX, 0xFFFF_0000, 0xFFFF));
    }
    let long = spanned(Edition::Edition2021, 9, 100, 0xFFFE);
    parts.push(Literal::new("1", long, LitKind::Integer, "u128_long_suffix").into());

    let mut trees = shared::trees();
    trees.push(parts.build());
    trees
        .iter()
        .map(|tree| {
            let mut bytes = Vec::new();
            tree.write_wire(&mut bytes);
            bytes
        })
        .collect()
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
