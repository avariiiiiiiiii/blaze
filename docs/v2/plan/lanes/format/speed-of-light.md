---
status: plan
summary: lane format's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane format: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component of lane format has a section here,
written by the agent that owns it. Unless a section says otherwise, figures are for one core of
the target machines (EPYC Turin, Graviton 5), from [hardware.md](../../design/hardware.md):
- about 3.5 GHz, so figures in ns use 3.5 GHz;
- an L1 hit in 4 to 5 cycles and L2 in about 14;
- streaming reads from L2 at about 32 bytes a cycle, and from DRAM at about 20 to 30 GB/s;
- about 2 instructions per cycle for branchy pointer code and about 4 for straight-line
  integer code.

None of this has been measured yet: nothing compiles until every lane is done. `proof`'s budgets
measure each bound once it does.

## The HIR expression store: `hir-def` `hir/`, `expr_store/` (speed-hir-store)

Layout and history: [speed-hir-store.md](speed-hir-store.md). Below, \(n\) is the number of HIR
nodes in a body (expressions, patterns, statements and type refs) and \(S\) the store's bytes.

### Lowering a body, `Body::of` (no source map)

Work: walk the body's syntax once and write one row per HIR node.
- **Bytes.** Each HIR node reads about 6 tree elements (4-byte words) and about 6 bytes of text,
  so about 30 bytes, and writes a 5-byte row, about 4 bytes of `extra` and list entries, and 4
  bytes on the list stack: about 45 bytes in all. The anchor's tree was just parsed or is kept
  per thread, so it is in L2. That is about 1.5 cycles a node at L2 bandwidth, and bytes are not
  the bound.
- **Instructions.** The minimum per node is about 5 to dispatch on the syntax kind, about 10 to
  find its children in the flat tree, about 5 to write the row, and about 3 for the list stack.
  Add a name interned for about one node in three (hash about 8 bytes and probe one table:
  about 25, so about 8 a node), and a hygiene lookup in the body's span map, which almost always
  has one context (about 5). That is about 40 instructions, at IPC 2 about **20 cycles, 5.7 ns
  a node**.
- **Fixed cost per body.** One salsa query and one exact allocation per non-empty column at
  `finish` (8 to 20 of them, about 30 ns each): about **1 µs**.
- **The bound** is \(5.7n\) ns + 1 µs per body. A build lowers bodies in parallel, one query
  each, so a crate graph's lowering is bound by total nodes over cores.
- **Where the design is.** The store side is at the bound: rows and CSR lists, per-thread
  pooled scratch with no allocation per node, and no hashing per node. Without a reader of the
  map, no source map is built, so no `AstPtr` and no table push either.
- **What stands in the way, in the walk** (estimated, to be measured first):
  1. Each `ast::*` step goes through a `SyntaxNode` handle. If a handle costs more than about
     10 instructions (a refcount, a parent pointer), it is the largest term. The fix is to
     lower from `(FlatTree, element index)` directly, with typed views over indices.
     Owners: speed-syntax (the views) and this lane (lowering).
  2. `check_cfg` runs for every expression, statement, field and parameter. Since 2026-10-29
     it is one test when there is no attribute (`lower.rs` `may_have_attrs`: is the first child
     node an attribute? The grammar puts every `AnyAttr*` first). Before, it walked every
     child, and for a block every statement, through `dyn` and `expand_cfg_attr`.
  3. Interning a name hashes the token text again. The lexer could carry each identifier's
     hash, or a per-thread front cache could answer repeated names in a body without touching
     the shared table. Owner: the interner (format).

  Once it compiles, the first step is `proof` measuring cycles per HIR node on the corpus.
  Anything above 22 cycles a node is a bug against this bound.

### Reading the store: `store.expr(e)`, `pat(p)`, `type_ref(t)`

- **Bound:** the bytes of the node, a 1-byte kind and a 4-byte data word in two columns, plus
  one `extra` read for a node of several words. That is 2 or 3 loads, L1 or L2 hits for a
  body being inferred: about 5 to 15 cycles.
- **Design:** exactly those loads and a `match` on the kind, so it is at the bound by
  construction. A whole-store scan reads \(5n\) bytes plus `extra`. The id-only scans
  (`expr_ids()`) read nothing, and borrowck's seven scans per body are filed to become one
  (`v2/requests/db.md`).

### `finish`: scratch to the store's columns

- **Bound:** write \(S\) once. Scratch is hot in L1 and L2, so the reads are nearly free, plus
  the allocations.
- **Design:** one copy per column, at the bound for \(S\). Since 2026-10-29 the `Copy`
  columns share one slab (`hir/col.rs`), so a body makes 1 allocation plus one per non-empty
  cold side table: about 4, not 8 to 20 (about 0.1 µs, not 0.5).
- **What stands in the way:** the side tables (names, literals) stay boxed, because their
  elements have destructors. Interning them into `u32` ids would let them join the slab.
  `TypeColumns` has its own slab. The path columns (`PathColumns`, since 2026-10-29 columns, not
  boxed values) are `Copy` but freeze to one box each, up to 12 a store, because lowering reads
  them back through the same views a frozen store has (`speed-hir-store.md`, "Still short").

### Encoding and decoding a store (`expr_store/persist.rs`)

- **Encode bound:** write \(S\) once, and hash it if its content id is taken. BLAKE3 costs
  about 0.6 cycles a byte with AVX-512 or NEON over large inputs.
- **Encode design:** each column is written as a `Slice`, which is a copy on little-endian
  targets. It is at the bound.
- **Decode bound:** unsafe.md requires bytes from outside the process to be hash-verified
  (0.6 cycles a byte) and bounds-checked before any view of them. The check (kinds below their
  count, list ranges inside their arrays) is about 1 cycle a 5-byte row, so about 0.2 cycles a
  byte. With views that borrow the buffer, the bound is about **0.8 cycles a byte**.
- **Decode design:** hash, then check in place, then copy each column out once. The copy adds
  about 0.1 cycles a byte plus one allocation per column, which is about 15% over the bound for
  a large store and more for a small one.
- **What stands in the way:** decode copies the columns, though since 2026-10-29 into one
  slab, not one allocation each. A store read in place from a crate image
  (`StoreBytes::InPlace`, most signature stores) could borrow its `Copy` columns instead.
  `Col` is the column type that can hold such a count, and `follow` already checks each
  `Slice`'s alignment. What's missing is the image's owner reaching `deps/signatures.rs` `read`
  as an `Arc` (frontend-deps-sigs, requested in `v2/requests/speed-hir-store.md`). Check also fuses into the hash
  pass, a 16 KiB block at a time, so the bytes are read from memory once.

### Expression scopes (`expr_store/scope.rs`)

- **Bound:** one pass over the expressions: read each kind (5 bytes), write its scope (4
  bytes), and push one entry per binding. About 10 instructions an expression, so 5 cycles.
- **Design:** one walk over the views into per-thread scratch columns, copied out exactly.
  Entries are grouped by scope, with a stable radix sort that is skipped when they are
  already grouped, which is every body without a `const` block inside a pattern. It is at the
  bound, estimated.

### An edit: what runs again

- **Bound:** for an edit inside body \(B\) of file \(F\), reparse what changed (speed-syntax),
  lower \(B\) again, and do \(O(1)\) for every other item of \(F\): compare a hash the parse
  carries.
- **Design:** a body is lowered from its own anchor's tree and is equal after any edit outside
  it, so nothing downstream reruns. But `body_syntax` runs for every body of \(F\), and each
  anchor is hashed whole (`tree_hash`, once per anchor per thread). So an edit costs
  \(O(|F|)\) hashing: \(|F| \times\) cycles a byte.
  - It was two SipHash-1-3 passes, about 3 cycles a byte: 1.2 M cycles, **340 µs**, for a
    400 KB file.
  - Since 2026-10-29 it is one BLAKE3 pass (`body.rs` `subtree_hash`): about 0.6 cycles a byte
    over a large anchor, so about **70 µs** for the same file. A single-function anchor under
    1 KiB gains little (one chunk, about 1.5 cycles a byte).
  - Signatures hash their trees the same way, filed in `v2/requests/speed-hir.md`.
- **What stands in the way:** every unchanged anchor is still hashed on every edit. The parse
  could carry each node's subtree hash, computed bottom-up while the tree is built and kept for
  unchanged subtrees by an incremental reparse. Then `tree_hash` is one load per anchor, and an
  edit's cost is the reparse plus one body. Owner: speed-syntax (the flat tree), filed in
  `v2/requests/format.md`. Until then an edit is \(O(|F|)\) and over its bound for large files.
- **Source maps** are built only when something reads them. An IDE body read with its map is
  lowered twice, once by `Body::of` and once with the map. That is IDE-only, and within 2× of
  the bound for that body.

## The lexer: `LexedStr::new` (`crates/parser/src/lexed_str.rs`, `lexed_str/scan.rs`) (format-lexer)

\(n\) is the text's bytes and \(T\) its tokens, trivia included. Rust source has about 3.5
bytes a token: the token column is sized for 3, and whitespace runs and identifiers are short.
The token mix is about 40% whitespace, 30% identifiers and keywords, 22% punctuation, and 8%
literals, lifetimes and comments. These are estimates; `proof` measures them on the corpus
first, and every figure below scales with them.

### The work, and the bound

The output is a 4-byte token (kind and length) per token, plus a 4-byte checkpoint per 16,
so 4.25 bytes a token. That is data.md's budget, and the parser reads it in this form.

- **Bytes.**
  - The text, read once: \(n\).
  - The classes, written once and read back: \(0.5n\) each way. They are 32 bytes per 64 of
    text; the text and its classes are still in L1 or L2 when the token loop reads them.
    They live in the worker's scratch buffer (`blaze_mem::scratch`, design/memory.md), which
    stays committed and warm from one lex to the next, so they cost no allocation and no
    page faults.
  - The tokens: \(4.25T \approx 1.2n\).

  That is about \(2.7n\) of distinct bytes. From L2, at about 32 bytes a cycle, it is
  **0.08 cycles a byte**. From DRAM (a file read for the first time), at 25 GB/s, it is
  \(2.7 / 25\) ns, so **0.38 cycles a byte**. Memory is not the bound.
- **Instructions.**
  - **Classification.** On AVX-512 a 64-byte block takes about 14 vector ops and 4 stores,
    and turning whitespace into token starts takes about 10 scalar ops. That is about 28
    instructions per 64 bytes, **0.44 a byte**. On NEON it is about 80 per 64 bytes, 1.25 a
    byte.
  - **Each token.** The least a lexer can do works a block at a time. A fifth bitmap marks
    the bytes that start a token needing more than its run: digits, `/`, `#`, quotes, bytes of
    0x80 or more, and the rest of ASCII that is not punctuation. That costs about 6 more
    vector ops per 64 bytes, so classification is about **0.55 instructions a byte**. Every
    token between two special starts is then known to be simple, so each one costs about 9
    instructions:
    - the bit scan, the cleared bit and the length from the last start (3);
    - the first byte and its kind from a table (2);
    - pack and store into a 64-entry local buffer (3);
    - the loop (1).

    The column's capacity and the checkpoints are paid once a block, about 20 instructions,
    which is 0.3 a byte.
  - **Identifiers.** Each also needs a keyword test. `SyntaxKind::from_keyword_in` reads the
    identifier in place with one 8-byte load and probes one table slot, about 19 instructions
    (format-grammar's "Keyword lookup" below). Averaged over all tokens that is
    \(0.3 \times 19 = 5.7\).
  - In all, about 14.7 instructions a token, so \(14.7 / 3.5 = 4.2\) a byte. Adding 0.3 for
    each block and 0.55 for classification gives **5.05 instructions a byte**.
- **The critical path.** The only chain carried from one token to the next is finding the next
  token's start. Clearing the lowest bit of a register (`bits &= bits - 1`) makes it **1 cycle
  a token**, about 0.3 cycles a byte, which does not bind. Finding the start with a load at the
  last token's end would make it about 10 cycles a token, which binds (see below).
- **The bound.** Instructions bind: 5.05 a byte at IPC 4 is **about 1.26 cycles a byte, 0.35
  ns a byte, about 2.9 GB/s a core** on Turin. On Graviton 5 (5.9 instructions a byte with
  NEON's classification) it is about 1.5 cycles a byte.
  - A 100 KB file is about **35 µs**.
  - Files lex in parallel, one per task, so a cold build's lexing is total bytes over cores.
    The rustc tree's 120 MB of `.rs` would take about 0.42 s on one core, and 4.4 ms on 96.
  - Interning identifiers is not the lexer's: it happens later, in lowering.

### Where the design is

The benchmark is simdjson. Stage 1 classifies 64 bytes at a time into bitmaps. Stage 2 walks
the structural bits with `trailing_zeros` and a cleared lowest bit, so the loop-carried chain
is one ALU op. Zig's tokenizer is the scalar bar.

As of 2026-10-29 the lexer has both stages:

- **Classification.** One vector pass gives four bitmaps:
  - identifier bytes;
  - whitespace, replaced in place by token starts (the first byte of an identifier or
    whitespace run, and every other byte);
  - `\n`;
  - `"` or `\`.
- **The token loop, `scan::Cursor` and `scan::token`.** The cursor keeps the current block's
  start bits in a register.
  - An identifier, whitespace or punctuation token takes three loads off the chain: its
    kind, its flags, and the byte after it. Add one `trailing_zeros`, one cleared bit and two
    branches that predict well. The branches are "needs special handling" (about 8% of
    tokens take it) and "the byte after may continue the token" (rare).
  - The kind is already the final `SyntaxKind`. Before, the fast path built a
    `rustc_lexer::TokenKind` that `extend_token` dispatched on again, and the next start was
    a load at the last token's end, the 10-cycle chain above.
- **Off the fast path, but still handled in `scan`:**
  - A line comment ends at the next `\n` bit.
  - A string ends at the next quote bit. If that bit is a `\`, the search skips it and the
    byte after it and goes on; the escapes are then checked once, on the contents alone, with
    the converter's `unescape_str`, and a string with an escape error goes to `rustc_lexer`, so
    its error text is unchanged.
  - A lifetime is `'` and an identifier run.
  - A plain decimal integer takes a byte or two of lookahead.

  Before, strings and lifetimes went to `rustc_lexer`, through a `Cursor`, UTF-8 decoding and,
  for a string, a second pass to unescape it.
- **The rest goes to `rustc_lexer`, one token each:** chars, raw, byte and C strings, strings
  with an escape error or a suffix, numbers with a base, fraction or suffix, block comments,
  and anything next to a byte of 0x80 or more. That is about 1% of tokens, at about 250
  instructions each: about 2.5 a token on average. `Cursor::seek` puts the cursor back after
  each.
- **Per token, the estimate is about 46 instructions against the minimum's 14.7.** The extra,
  about 31 instructions, is:
  - `LexedStr::push`'s work per token: the checkpoint test, the column's capacity, the
    running offset and the long-token test (7);
  - the per-token tests for special handling, for refilling the cursor's block and for the
    byte after the token (7);
  - the caller's test of the `Option` and of `IDENT` (3);
  - bounds checks (4);
  - the identifier's `&str` slice (1.2 on average);
  - address arithmetic (about 4);
  - the tokens `rustc_lexer` lexes (2.5).

  That is about 13.6 instructions a byte, **about 3.4 cycles a byte, 2.7× the bound**. Each
  term is an estimate good to about ±50%.

### What stands in the way, in the order to remove it once `proof` measures it

The estimate's error is larger than the gap between items, so the first measurement decides
the order. Anything above 1.4 cycles a byte on Turin (90% of the bound) is a bug against it.

1. **Per-token work that belongs once a block, about 17 instructions a token.** This is
   `push`'s bookkeeping, the per-token tests and the caller's tests. Fix: the block form
   above. Add the fifth bitmap (special starts) to the three vector kernels and the scalar
   reference, using simdjson's nibble-table lookup. Then, per block, walk the start bits that
   come before the first special start in a loop with no test but the loop's own. Write the
   packed tokens into a local buffer, and extend the column and add the checkpoints once. A
   special start drops to `scan::token` as today.
   - The long-token test moves to the block: a token longer than 4 MiB spans a block with no
     start.
   - The byte-after test goes away, because every byte that can continue a run (`#`, `"`,
     `'`, 0x80 and up) is itself a special start.

   Owner: format-lexer. What stands in the way: it rewrites the vector kernels, and the lanes
   are not building yet (common.md). The kernels must first pass the every-path-against-scalar
   test on AVX-512, AVX2 and NEON hardware, so this lands right after the first build. It is
   not stacked blind on top of the cursor change.
2. **The tokens still lexed by `rustc_lexer`, about 1% of tokens: about 2.5 instructions a
   token on average.** Escaped strings are now in `scan` (2026-10-29). Suffixed and floating
   numbers and chars are what is left. Fix: a scalar scan of the fraction, the exponent and
   the suffix in `scan::special`, and a char's one code point or escape. Owner: format-lexer,
   with the first measurement of the mix.
3. **The keyword test. Done (2026-10-29).** format-grammar replaced the perfect hash, which
   was about 60 instructions an identifier, not the 15 this section first assumed. The new
   probe reads 8 bytes in place, and `ident_kind` now calls `from_keyword_in` on the file's
   text, at about 19 instructions. The bound above already counts it.
4. **Bounds checks, about 4 instructions a token.** The classes' sentinel bits already stop
   every search at the end. Removing the remaining `bytes[at]` and `bytes.get(end)` checks
   would need an unsafe read, so it waits for a measurement that shows it matters
   (docs/v2/plan/design/unsafe.md: unsafe only for measured speed). The block form removes
   most of them anyway, because a start bit is in bounds by construction.

With 1 and 2 done, the estimate is about 19.2 instructions a token: the bound's 14.7 plus
about 4.5 for the slice, address arithmetic and bounds checks in the inner loop. Adding the
block's cost and classification, that is 6.3 a byte, about 1.6 cycles a byte, or 80% of the
bound. With 4 as well, the estimate is about 15.2 a token and 5.2 a byte, about 1.3 cycles a
byte, which is within 10% of the bound.

### `break 'a: loop {}` (`grammar/expressions/atom.rs`)

This adds one lookahead compare per `break`, which is free. The labeled value is parsed once
by the shared `labeled_expr`.

## The grammar: tokens to events (`crates/parser/src/grammar/`) (format-grammar)

\(t\) is a non-trivia token: the grammar never sees trivia. In real source, at about 3.5 bytes
a token and 60% of tokens non-trivia, there is one \(t\) per 5.8 bytes. The parser's test
trees (`test_data/parser/{ok,inline/ok}`, counted 2026-10-29) have 11,413 nodes for 10,855
\(t\): **1.05 nodes a token**. Those files are denser than real code, at 3.0 bytes a \(t\).

### The work, and the bound

The output is an 8-byte `Event`: one `Token` per \(t\), and a `Start` and a `Finish` per node.
- **Bytes.** The grammar reads the input word of each token it looks at, 4 bytes, and writes
  \(8 + 1.05 \times 16 \approx 25\) bytes of events per \(t\). With the `Vec`'s doubling
  copies, that is about 35 bytes, or about 1 cycle per \(t\) at L2's 32 bytes a cycle. Memory
  is not the bound.
- **Instructions.** The least an LL(3) parser can do per \(t\):
  - for the token, about 7: load its kind, dispatch on it (2), push the event (3), and advance;
  - for its 1.05 nodes, about 8 each: push the `Start` (3), then write the kind into it and
    push the `Finish` (5);
  - about 2 more kind tests (2 instructions each) where one dispatch does not decide.

  That is about **20 instructions per \(t\)**.
- **Branches.** The dispatch depends on the data. Even a parser whose branches predict well
  mispredicts about once in 8 tokens, at about 15 cycles each: about 2 cycles per \(t\). This
  is an estimate, which `proof` measures first.
- **The bound.** 20 instructions at IPC 2, plus the mispredicts, is **about 12 cycles, 3.4
  ns, per \(t\)**. That is about 2.1 cycles a byte, or about 1.7 GB/s a core. A 100 KB file
  is about 17,000 \(t\), so **about 60 µs**. Files parse in parallel, one per task, so a cold
  build is bound by total \(t\) over cores.

### Where the design is (estimated, ±50%, nothing measured)

The grammar is rust-analyzer's recursive descent: one call and several kind tests per
production. A kind test (`p.at`, `p.nth`, `p.current`) costs about 12 instructions, not 2:
- `Parser::nth`'s step counter, a `Cell` load, compare, branch and store (5);
- `Input::kind`'s `words.get` bounds check (2);
- `SyntaxKind::from`'s range assert (2);
- the mask and the compare (3).

The `Cell` store between two tests also stops the compiler from reusing the kind it just
loaded, so a chain of `p.at(X) || p.at(Y)` reloads it every time. With about 8 kind tests per
\(t\), that is about 95 instructions. Add about 15 for the token and its nodes, and about 8 per
node for the `Marker`'s `DropBomb`: building a `Cow` message, defusing it, and testing it on
drop. In all, about **120 instructions per \(t\), about 5× the bound**.

No valid input allocates. Every `format!`, `String` and `to_owned` in `grammar/` is on an
error path (checked 2026-10-29), including the new `PolarityAndModifiers` message.

### What stands in the way, in order

1. **The kind test's overhead, about 10 of its 12 instructions** (`parser.rs`, `input.rs`; not
   format-grammar's, filed in `v2/requests/format.md`, "format-grammar: the parser's speed of
   light"):
   - Count steps where a stuck loop must pass (`start`, `error`, `err_recover`, the
     `expect` failures), not in `nth`. Every grammar loop that fails to consume either starts
     a marker or reports an error, so the guard still catches it; a fuzz target over the
     grammar checks that.
   - Pad `Input`'s words with 4 `EOF` words, so a kind read at `pos + 3` needs no `get`.
   - Have `Input` hold kinds that are valid by construction, so `kind()` needs no range
     assert.

   Then a kind test is a load and a compare, and adjacent tests share the load. That leaves
   about 8 tests × 3 + 15 + 8 ≈ 47 instructions per \(t\), about 2.3× the bound.
2. **The `DropBomb` per marker, about 8 instructions a node.** A `Marker` of a `u32` and a
   `bool`, with its own `Drop` that asserts the bool, does the same check in one byte
   (`parser.rs`, same request).
3. **The production structure.** Most of what remains is calls and tests that one table-driven
   dispatch per token would merge. That is the postorder rewrite of `grammar/` that
   [speed-syntax.md](speed-syntax.md) describes, which also removes `event::process`. Do it
   after `proof` measures where the remaining cycles go: the ±50% on the estimate is larger
   than the gap between this item's options.

Applied 2026-10-29 in format-grammar's files:
- `paths::path_for_qualifier` read `p.nth(2)` (a full kind test) for every path segment before
  testing for `::`. It now tests `::` first, which saves about 12 instructions per path, about
  3 per \(t\).
- The new productions (`GuardPat`, `&pin`, the range ban, `!` bounds, fn-pointer params,
  `unsafe mod`, `cfg` literals) add at most one kind test where they are possible, and none
  elsewhere: `ban_pat_range_if_ambiguous` reads the operator `pattern_single_r` already had.

Applied 2026-10-29 in `parser.rs` and `input.rs` (requests-parser, items 1 and 2 of
"What stands in the way"), not measured. Nothing is built until every lane is done.
- **Step counter.** It is a plain `u32` in `Parser`, checked in `start` and `error` (which
  every failed `expect` and every path of `err_recover` go through) and in the `EOF` branches
  of `bump_any` and `bump_remap`, which consume nothing. The limit and the message are the
  same (`PARSER_STEP_LIMIT`, "the parser seems stuck"), and it is reset on every consumed token.
  A kind test no longer touches it, which saves 5 instructions a test. Because nothing stores
  between two tests, `p.at(X) || p.at(Y)` loads the word once.
- **Pad and valid kinds.** Every `Input`, and every `parser::Tokens` view (`Tokens::new`
  checks this), ends in `Tokens::PAD` = 4 words of an `EOF` token, and `len()` leaves them out.
  `kind`, `contextual_kind` and `is_joint` at `pos + n` are a load from the slice for
  `pos ≤ len` and `n ≤ 3`. A composite token reads `pos + n + 2` only after `pos + n` and
  `pos + n + 1` matched real kinds, so it never reads past the pad. A kind is the word's 10
  low bits transmuted, with no range assert. That is sound because `Input`'s words come only
  from `word(SyntaxKind, …)` plus flag bits above both kind fields, and `Tokens::new` checks
  both fields of every word once, a branch-free pass at memory bandwidth (about 0.1 cycle a
  word). This removes the `get` and its `EOF` select (about 1 instruction) and the assert (2).
- **Result.** A kind test is about 5 instructions: add the offset, the slice's bounds check
  (a compare and a never-taken branch, one fused µop), load, mask, compare. Before, it was
  about 12. With about 8 tests per \(t\) and the shared loads, that is about 8 × 4.5 + 15 ≈
  **51 instructions per \(t\), about 2.5× the bound** (it was about 5×). The bounds check is
  the 0.2× between this and the 2.3× projected above. Removing it needs `get_unchecked`, a
  release check that `pos ≤ len` on every bump (one compare per token instead of one per
  test), and `n ≤ 3` asserted in `nth_at`. That is about 7 µops per \(t\), for `proof` to
  weigh once it has measured.
- **Marker** (item 2 above): `Marker` is a `u32` and a `bool` with its own `Drop` that asserts
  it, with the same message. Completing or abandoning a marker stores `true`, and the
  compiler folds the drop's test away, so a node pays nothing for it. That is about 8
  instructions a node less, which is why the 51 has no term for it. `drop_bomb` is no longer
  a dependency.
- **Termination.** `tests.rs` `termination::every_entry_point_finishes_on_every_input` runs
  every `TopEntryPoint` (full and as an outline), every `PrefixEntryPoint` and
  `Reparser::top_level_item` over 2,000 token sequences (`BLAZE_FUZZ_RUNS` changes the
  count), with a deadline. The sequences are windows of the parser's sources, changed as
  `blaze-fuzz` changes bytes, and made-up sequences. It is a property test, not a fuzz
  target: `fuzz/Cargo.toml` was being edited by another lane, and `blaze-fuzz`'s global
  allocator can't share a test binary with the parser's counting one.

## Keyword lookup (`crates/parser/src/syntax_kind/keywords.rs`) (format-grammar)

The lexer looks up every identifier, about 30% of tokens (format-lexer's section, item 3).
- **Work:** read the identifier, at most 8 bytes for a strict keyword, probe one table entry,
  and compare it.
- **Bound:** about **12 instructions**: a length filter (2), one 8-byte load and a mask (3),
  a multiply-shift hash (3), two loads (2), and the compares (2).
- **Before (2026-10-29):** `blaze_phf`, a byte loop over the identifier's tail, three 64×64
  multiplies to fold it, a bucket and a pilot load, and a byte-by-byte compare. About 60
  instructions for a 5-byte identifier, with a branch per byte.
- **Now:** the key is the identifier as a zero-padded little-endian word, and a multiplier
  found at compile time sends every strict keyword to its own slot among 256. A `const`
  search finds it and fails the build if there is none, so no runtime path handles a missing
  one. A lookup is then one slot load, one entry load, and the window, length and edition
  compares, with no data-dependent branch.
  - `SyntaxKind::from_keyword_in(text, start, len, edition)` reads the window with one 8-byte
    load from the file text, about **19 instructions**.
  - `from_keyword(&str)` cannot read past the identifier, so it ORs four 2-byte loads at
    offsets set by the length, about **24 instructions**.
  - `tables_give_what_the_match_gave` checks both against the old `match`, for every keyword
    and every edition, with variants of every keyword: each byte flipped, each byte doubled,
    the whole doubled, a prefix, a suffix, upper case, and the identifier mid-text and at the
    end of the text.
- **What stands in the way of the last 7 instructions:**
  - The lexer calls `from_keyword_in` (format-lexer, 2026-10-29); it was requested of
    format-lexer (`lexed_str.rs:559`).
  - The slot holds an index into the entries, which costs one load and one bounds check.
    Entries stored in the slots directly (4 KB instead of about 1 KB) remove both, at the cost
    of more L1 footprint. Whether that pays is for `proof` to measure.
- Contextual keywords (up to 15 bytes, looked up only by the parser's callers) keep
  `blaze_phf`.

## Syntax validation (`crates/syntax/src/validation.rs`) (format-grammar)

- **Bound:** every node's kind read once, and one dispatch to the check for it, if it has
  one: about 4 instructions a node, plus what the checks do on the nodes they are for. The
  tree walk is syntax-tree's, so it is outside this bound.
- **Before:** 15 `can_cast` tests in a row on every node. Most nodes have no check, so they
  took all 15, about 30 instructions a node.
- **Now (2026-10-29):** one `match` on the kind, a jump table, at the bound. Each arm's type
  is a single kind; a `debug_assert` checks the kind against `AstNode::kind`.
- **What stands in the way:** the walk builds a `SyntaxNode` handle for every node, and a
  check casts with a cursor clone. Walking `(FlatTree, index)` directly is speed-syntax's
  typed views (`v2/requests/format.md`).

## The item tree: `hir-def` `item_tree*` (speed-hir-sol-items)

Layout and history: [speed-hir-items.md](speed-hir-items.md) (9 bytes an item plus a payload
record, a reused per-worker builder). The files are `frontend-deps`' while it builds dependency
images, so every change below is an exact edit in `v2/requests/frontend-deps.md`
("speed-hir-sol-items"), A to F. The file is frontend-deps-reparse's example
([frontend/speed-of-light.md](../frontend/speed-of-light.md), "frontend-deps-reparse"): \(I\) =
500 top-level items, 50 KB of text, about 10 KB of it signatures (bodies left out), \(A\) =
1 000 attributes, most of them doc comments, which the item tree skips. Cycle counts are
estimates counted from the code; nothing has been run.

### Lowering a file's outline (`lower_whole`, a warm builder)

Work: one row per item, from its signature's tokens. Bodies are never entered: a lazy `fn` body
is only tested for a leading `#!`.
- **Bytes.** Lowering reads about 10 KB of signatures and writes about 25 bytes an item (the
  9-byte row, its payload and its name), about 12 KB in all. At L2's 32 bytes a cycle that is
  about 700 cycles for the file, so bytes are not the bound.
- **Instructions, per item, at the minimum:**
  - about 15 to dispatch and find the name and visibility among the item's children;
  - about 30 to intern the name (hash about 8 bytes, probe one shard; the interner takes no
    lock on a hit);
  - about 25 for the ast id (one probe of `AstIdMap`);
  - about 15 to deduplicate the name within the item, and about 6 to write the row and payload;
  - about 10 to find that the item has no attributes.

  That is about 100 instructions, at IPC 2 about **50 cycles, 14 ns an item**. Each attribute
  the tree keeps adds a path interned (about 40 cycles); a skipped doc comment adds about 10.
- **Fixed cost per file:** one exact allocation per non-empty column at `finish`, at most 31
  (30 columns and, with edit C, the attribute index), about 30 ns each: about 1 µs.
- **The bound:** \(50I\) cycles + \(10A\) + 1 µs, so **about 11 µs** for the example file.
- **Where the code is.** About 170 cycles an item:
  - about 75 in `SyntaxNode` cursor scans: `name()`, `visibility()` and `attrs_with_doc()`
    each walk the item's children;
  - about 30 in attribute lowering's setup for an item with none (`collect_item_tree_attrs`);
  - about 30 to intern the name and 25 for the ast id;
  - about 10 for the three `Scoped` seals.

  Add, per file, a `radix::sort_copy_by_key` of the attribute owners at `finish`. Above its
  insertion-sort size it allocates a scratch the size of the owners and makes four passes. Yet
  the owners are already in order except the top level's, which is pushed first. In all that is
  about 100 000 cycles, **29 µs, about 38% of the bound**.
- **What stands in the way, and the exact change:**
  1. *The sort* (edit A): hold the top level's owner aside and push it last at `finish`. Every
     other owner is pushed right after its item's id is made, so they arrive in order, and an
     `assert!` in `add_attrs` checks it with one compare a push. This removes the allocation
     and the passes.
  2. *The scope table after a big item* (edit B): `Scoped::seal` drains an `FxIndexSet`, and a
     full drain clears the hash table, which writes every control byte at the table's
     high-water size. After one inline module with 1 000 names, every later sealed item on the
     worker writes about 1 KiB: about 32 cycles, about 20% of an item. With edit B, a table
     much larger than its entries is emptied one entry at a time instead. That is O(entries),
     with no allocation, and the capacity is kept.
  3. *The cursor scans* are the largest term. With frontend.md item 16's index views
     (frontend-tree) they are about 15 cycles. *The ast id* could be read by element index
     instead of a hash probe (span's `AstIdMap`, about 20 cycles).
  4. *Attribute setup* for an item with none: a first-child test, as the expression store's
     `may_have_attrs` does, saves about 25 cycles. It stays out of this round: inner attributes
     anywhere in a module's or an impl's list are collected today, and a first-child test must
     keep that.

  With 1 and 2 the code is about 160 cycles an item (40%). With 3 and 4, about 70 (70%). The
  remaining gap to 90% is interning: a per-thread front cache of recent names, which is the
  store section's item 3.

### A memo hit and the accessors

- **Memo hit** (`file_item_tree`, `block_item_tree_query`): salsa's lookup through the concrete
  `&Db`, a few loads and a revision compare, and no allocation (asserted:
  `items/tests.rs` `trees_asked_again_allocate_nothing`). Its cost is salsa's, lane db's.
- **Bound per accessor:** the bytes it returns, one or two loads from columns that a walk
  in item order streams through L1.
- **Where the code is.**
  - At the bound: `top_level_items`, `items`, `mod_kind`, `value_ns_ctor`, `use_tree` and
    `UseTree::kind`, `get_name`, `Index<NameId | PathId | RawVisibilityId>` and
    `import_alias`. Each is one or two bounds-checked loads, and a `Name` or `ModPath` comes back
    by reference or as a copy.
  - `index(id)`: three loads (kind, data word, ast id) and the payload, about 6 to 8 cycles.
    Packing the kind into the data word's top byte (data < 2^24 per kind) would make it 8 bytes
    and two loads, about 1 cycle less. That changes the persisted schema; it is listed for
    frontend-deps to weigh, not filed.
  - **`attrs(id)` is a binary search** over `attr_owners`: about 9 dependent probes for 500
    owners, with a mispredict on about half, so about 100 cycles. The def map collector asks it
    for every item: \(100I\) = 50 000 cycles, 14 µs a file, more than lowering the file's
    outline costs at its bound. **Edit C** makes it O(1) with a rank index in the style of
    the flat tree's node bits: per 64 items, one `u64` of which have attributes and the count
    before them (`AttrBlock`, 16 bytes per 64 items, 0.25 bytes an item, against the 4 bytes a
    per-item column would cost). The lookup is one load, a mask, a `popcnt` and the owner's
    load: about 10 cycles. The index is built from `attr_owners` at `finish`, at `assemble`
    and at `decode`, so the persisted bytes don't change.
  - `Use::expand`: for each leaf under a prefix, the joined path is interned, a `SmallVec`
    copy and one probe, about 40 cycles. The callback takes a `ModPath`, so this is the API's
    bound. A warm run allocates nothing (edit D asserts it).
- **Budgets** (edit D, `item_tree/tests.rs`): reading every item through every accessor is 0
  allocations, and `Use::expand` once its paths are interned is 0.

### Encoding and decoding (`persist.rs`)

- **Encode bound:** write the columns once, about 25 bytes an item plus the name table, then
  hash the buffer in the store (BLAKE3 at about 0.6 cycles a byte). That is about 15 KB, so
  about 10 000 cycles, 3 µs, for the example.
- **Encode design.** The columns are pushed as slices, at the bound. Three temporaries
  (kinds as `u8`, ast ids as `u32`, macro calls as records) are copied once more, about 5 bytes
  an item, plus 3 allocations, about 3%. The largest term is in `hir/persist.rs`, which is
  speed-hir-store's: `Words::name` allocates a `Box<str>` key for every distinct name, and both
  `name` and `symbol` look up and then insert, which hashes twice. At about 100 cycles a name
  that is 50 000 cycles, so **encode is about 5 times its bound**. Filed in
  `v2/requests/speed-hir-store.md`: one hash per name (the entry API, or a `hashbrown::HashTable`
  of indexes into `text`) and no allocation per name.
- **Decode bound:** verify the hash (the store's, 0.6 cycles a byte), check every id (about 1
  cycle a row), copy each column once (0.1 cycles a byte), and intern each distinct name once.
  That is about 1 cycle a byte plus 40 cycles a name, **about 10 µs** for the example.
- **Decode design.**
  - The `Copy` columns are one `memcpy` and one allocation each, and the checks are one pass,
    both at the bound.
  - Off the bound: `kinds` and `macro_calls` are collected into a `Result<Box<[_]>, _>`, whose
    size hint is 0. They grow by doubling and shrink at the end, about \(\log_2 n + 2\)
    allocations each. Edit E collects them at their exact length.
  - `Read::seq` (`hir/persist.rs`) has the same doubling for the names, paths, visibilities,
    attributes and `cfg`s. `Code for ModPath` reads its segments into a `Vec` first, one
    allocation per path and per attribute path. Both are filed for speed-hir-store: an
    exact-size iterator for `seq`, and a `SmallVec<[Name; 8]>` for the segments.
  - With E and those two changes, decoding allocates once per non-empty column (edit D
    asserts this). A read in place (`ItemTreeRef`, the top level, kinds, ast ids and children)
    is already 0.
- **What stands in the way of 90%:** the name table is still re-interned on every read. Reading
  it in place needs symbols persisted as stable ids: frontend-image's interner, filed by that
  lane.

### Lowering a macro expansion's tree

The same `lower_whole`, over the expansion's parse. Each span lookup is one binary search of
the expansion's span map, about 50 cycles for a visibility path, a macro call or an attribute;
a plain item has none. The bound and the code's distance from it are those of the outline
lowering above, plus that search. The expansion's parse costs more than its tree.

### An edit: one item's rows change

- **Bound.** Re-lowering the file's outline is the bound of the whole-file design: **about
  11 µs** for the example. Re-lowering one item and reusing the others' rows needs, per
  unchanged item, a hash the parse carries (one load and a compare, about 5 cycles) and a copy
  of its rows with ids moved (about 10 cycles). That is \(15I\) = 7 500 cycles + one item's
  lowering (about 0.1 µs), **about 2.2 µs**. Both reuse every downstream memo the same way: the
  tree comes out equal when only bodies changed, and only the edited item's rows differ after
  a signature edit (asserted: `tests.rs` `an_edit_of_one_item_changes_only_its_rows`).
- **Where the code is.** Source files go through the per-item queries
  (`items.rs`: `item_sources`, then `item_fragment` per item, then `assemble`). By
  frontend-deps-reparse's own count, an edit costs:
  - `item_sources` about 120 µs;
  - the changed item about 0.9 µs;
  - `assemble` about 45 µs;
  - plus about 100 cycles an item for salsa to verify each unchanged `item_fragment` memo,
    about 14 µs.

  That is **about 180 µs**: 6 times the whole outline re-lowered by today's code (29 µs), and
  16 times its bound (11 µs).
- **A cold build** pays the same per item without the reuse:
  - `item_sources`, about 850 cycles;
  - a new `item_fragment` memo, about 300;
  - `Ctx::for_item`'s extras, about 600 (a `RealSpanMap` of three allocations, a pairs `Vec`,
    and about 7 exact columns per fragment);
  - the lowering, about 170;
  - `assemble`, about 20.

  That is about 1 950 cycles an item: **about 11 times `lower_whole`**, and about 10 allocations
  an item where `lower_whole` makes 31 a file. For a 100 000-item workspace, that is about 55 ms
  of a core against about 5 ms.
- **What stands in the way, and what it would take.** Per-item salsa granularity has a floor
  of about 250 cycles an item per edit (a tracked struct matched and a memo verified). That
  alone is more than re-lowering the item, about 170 cycles, so the per-item queries can at
  best tie the whole-file lowering. They beat it only once neither the queries nor the hashing
  run per item:
  1. the parse carries each top-level item's subtree hash, which an incremental reparse keeps
     for unchanged items (frontend-deps-reparse's item 4 plan; speed-syntax);
  2. one query per file reuses rows by that hash: its tree keeps a per-top-level-item key
     column, and the last tree of each file is kept beside salsa, keyed by content. The key is
     every input lowering reads, the item hash, the ids, the anchors and the crate's `cfg` set,
     so reuse misses nothing (AGENTS.md's cache-key rule). A cold file, with no last tree, is
     lowered whole.

  That reaches about 2.2 µs an edit and `lower_whole`'s cost cold. Until it is built,
  `lower_whole` for source files is the faster of the two designs on both paths. It is filed
  for frontend-deps to decide (request F), since frontend.md item 14 is its lane's. Edit F also
  carries a small step that holds under either design: `hash_full` feeds the 16-byte
  `subtree_hash` for subtrees of 4 KiB or more, which the flat tree keeps across edits, instead
  of their text. That helps large `macro_rules!` and macro calls.
- **Budgets** (edit D): on a warm builder, lowering a file allocates at most once per non-empty
  column. An attribute with an input allocates its `Box<AttrInput>` too (hir-expand's; shared
  inputs are requested in `v2/requests/db-expand.md`), so the fixture has none.
