# Requests of lane `usercode-wire`

From `usercode-wire` (a child of `usercode`), which did `v2/requests/usercode.md`
`## frontend-tokens` items 1-2 and `## From safety-fuzz-procmacro (2026-10-29)` items 1-6 and 8
in `crates/proc-macro-api/` and `crates/proc-macro-srv/`. Nothing here was built or tested
(the lane may not); each request says what it rests on.

## 1. load-cargo: `answer()` refuses a server's ids with errors, not panics (`crates/load-cargo/src/lib.rs`)

From safety-fuzz-procmacro item 7, forwarded because `load-cargo` is not this lane's.

What: `answer` (line 950) builds `FileId::from_raw`, `EditionedFileId::from_raw`,
`TextRange::new` and `SyntaxContext::from_u32` from the numbers in a server's `SubRequest`.
Each panics on some input (`TextRange::new` on `start > end`, `EditionedFileId::from_raw` on the
reserved bit or an edition past the latest, `from_u32` on `u32::MAX`). Answer those inputs with
`SubResponse::Cancel { reason }` naming the field and its value, in place of the panic.
`proc_macro_api::wire::raw_part(tt::WirePart { file_id, ast_id, ctx })` is public now and
does exactly the id checks, returning `Result<(SpanAnchor, SyntaxContext), String>`, so
`SpanSource`, `SpanParent` and `SpanJoin` can take their anchor and context from it. The range
still needs `start <= end` checked by hand. `LocalFilePath` and `FilePath` take a bare `FileId`,
and `FileId::from_raw` asserts the id is at most `FileId::MAX`, so check that too.

Why: the server is user code in a sandbox. `run_conversation`'s `catch_unwind` catches these
panics today, but a panic is no answer: the error should name what the server sent.

## 2. safety-fuzz (`fuzz/targets/proc_macro_flat_tree.rs` and the other `proc_macro_*` targets)

What changed in `crates/proc-macro-api/`, which the targets read:

1. `deserialize_span_data_index_map(&[u32]) -> Result<SpanDataIndexMap, String>` and
   `FlatTree::to_subtree_resolved(self, version, &table) -> Result<tt::TopSubtree, String>`.
   `decode` (lines 81-82) needs `.ok()?` on both calls. `to_tokenstream_unresolved` and
   `to_tokenstream_resolved` (in-rust-tree) return `Result` too.
2. The reader now checks what the writer guarantees: subtree ranges consecutive in the writer's
   breadth-first order, the k-th subtree named once, by an earlier one, as index k + 1; every
   index in bounds; kinds, spacing, chars and rawness valid; a suffix no longer than a token
   tree holds (256 KiB). It builds in preorder with a stack of its own, so the work is linear
   in the input. A span id past the table and a table naming one span twice are errors. Each
   input of the item 1-3 table is a unit test in `legacy_protocol/msg/flat.rs` `mod tests`,
   which must return an error. So `bound(len)` can likely come down, since nothing is built
   twice any more.
3. Every message type in `bidirectional_protocol/msg.rs` and `legacy_protocol/msg.rs`, and
   `FlatTree`, derives `PartialEq, Eq` (item 8). `canonical` and the `Debug` comparisons can
   go.
4. Frames: `transport::LONGEST` (1 GiB) is the longest frame for both transports, past which
   reading fails with "a frame of more than … bytes". A postcard stream that ends inside a
   frame (no terminating 0) is an error, not `None`, and so is a final JSON line with no
   newline (`api-version-check-with-no-newline.hex` is now an error). Bytes after the message
   inside a frame are an error in both formats. Postcard frames are unstuffed by
   `transport::postcard::decode` directly, not `postcard::from_bytes_cobs`. JSON keeps
   serde_json's recursion limit (the `unbounded_depth` feature is gone), so a JSON message
   nested deeper than 128 is an error.
5. `warm::LinkReader` is `pub` with `pub fn new(link: Arc<dyn Link>) -> LinkReader` (item 5).
6. A new protocol version, `version::WIRE_TREES` = 8 = `CURRENT_API_VERSION`. It adds
   `Request::ExpandMacroWire` and `Response::ExpandMacroWire` at the end of both protocols'
   enums, so the postcard tags of the older variants are unchanged. The trees in them are
   `tt::TopSubtree::write_wire` bytes (`proc_macro_tt_wire` fuzzes `read_wire`). `VERSIONS` in
   `proc_macro_flat_tree.rs` now includes 8, where a `FlatTree` reads as at 7. A target for the
   client's side of version 8 would read `Response::ExpandMacroWire` and call
   `proc_macro_api::wire::read_tree` (crate-private now; ask if you want it public).

## 3. usercode (`crates/blaze-expand/src/lib.rs`): `read` takes the `Result`

What: line 379, `Ok(flat.to_subtree_resolved(CURRENT_API_VERSION, &table))` becomes
`flat.to_subtree_resolved(CURRENT_API_VERSION, &table).map_err(|error| format!("a kept expansion: {error}"))`.
`CURRENT_API_VERSION` is now 8 (was 7). The kept expansion's `FlatTree` reads the same at 8 as
at 7, but the run key's `"abi"` part (line 354) changes, so kept expansions are rebuilt once.

Why: `to_subtree_resolved` returns `Result` (safety-fuzz-procmacro item 1). Without this
change `blaze-expand` does not build.

## 4. coordinator: the proc-macro server blaze runs speaks version 8 (`vendor/proc-macro-srv/`)

What: the server blaze starts is `vendor/proc-macro-srv/crates/proc-macro-srv-cli`, on its own
vendored copy of `proc-macro-api` (version 6). The client negotiates, so it keeps sending
`FlatTree` JSON to it, which works but leaves the wire format unused. To use the wire format,
either build the CLI on `crates/proc-macro-api` and `crates/proc-macro-srv`, or port the
handling into the vendored copy. Handling `ExpandMacroWire` means: read `trees` with
`proc_macro_srv::TokenStream::read_wire(bytes, &mut proc_macro_api::wire::raw_part, join)`
(the attribute starts at the byte count the first read returns), take the three sites with
`WireSpan::resolve(&mut raw_part)`, write the output with `TokenStream::write_wire(call_site,
&mut out)`, answer `Response::ExpandMacroWire` (with tracked env vars and paths in the
bidirectional protocol's `ExpandMacroWireResponse`), and report `CURRENT_API_VERSION` 8.

Why: frontend-tokens item 2. Until then, expansions pay for the JSON `FlatTree` both ways.

## 5. frontend-tokens (`crates/tt/`): two small exports (optional)

What: (a) a `pub const` for the longest literal suffix a node holds (`u32::MAX >> 14` bytes),
which `proc-macro-api` and `proc-macro-srv` restate as `LONGEST_SUFFIX` so they can refuse a
long suffix before `Node::literal` asserts. (b) `SubtreeView::write_wire`, or `write_wire` with
fragment wrappers left out. `proc_macro_api::wire::write_tree` rebuilds the tree through
`TopSubtreeBuilder` because `write_wire` exists only on a whole `TopSubtree`, and the
parentheses of a passed-on fragment must not reach a proc macro. That costs one rebuild per
request.

Why: (a) keeps the two crates' bound in step with tt's. (b) removes the copy that remains per
request.
