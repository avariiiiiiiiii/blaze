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
  2. `check_cfg` runs for every expression, statement, field and parameter. It must be one
     test (does the node have an attribute child?) when there is no attribute. That is in
     `lower.rs` (speed-hir-store).
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
- **Design:** one copy per column (`take_exact`), at the bound for \(S\). For a body of a few
  dozen nodes, the 8 to 20 allocations (about 0.5 µs) are more than the copy.
- **What stands in the way:** an allocation per column. The fix is one allocation per store,
  with every column a slice of it, which brings 8 to 20 allocations down to 1. That is the
  next change to `HirColumns` (speed-hir-store, after `speed-hir-store-stmts`, which holds the
  file).

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
- **What stands in the way:** the columns are owned `Box<[T]>`. Columns that are either owned
  or borrowed from the verified, aligned buffer (an `Arc` of the mapped pack) would remove the
  copy. That needs `blaze_format::Slice` to guarantee the alignment of its elements (format),
  and the store's column type to hold either (speed-hir-store). Check also fuses into the hash
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
  - **Identifiers.** Each also needs a keyword test: a length filter and the perfect hash in
    `syntax_kind/keywords.rs`, about 15 instructions. Averaged over all tokens that is
    \(0.3 \times 15 = 4.5\).
  - In all, about 13.5 instructions a token, so \(13.5 / 3.5 = 3.9\) a byte. Adding 0.3 for
    each block and 0.55 for classification gives **4.7 instructions a byte**.
- **The critical path.** The only chain carried from one token to the next is finding the next
  token's start. Clearing the lowest bit of a register (`bits &= bits - 1`) makes it **1 cycle
  a token**, about 0.3 cycles a byte, which does not bind. Finding the start with a load at the
  last token's end would make it about 10 cycles a token, which binds (see below).
- **The bound.** Instructions bind: 4.7 a byte at IPC 4 is **about 1.2 cycles a byte, 0.33
  ns a byte, about 3 GB/s a core** on Turin. On Graviton 5 (5.6 instructions a byte with
  NEON's classification) it is about 1.4 cycles a byte.
  - A 100 KB file is about **33 µs**.
  - Files lex in parallel, one per task, so a cold build's lexing is total bytes over cores.
    The rustc tree's 120 MB of `.rs` would take about 0.4 s on one core, and 4 ms on 96.
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
- **Per token, the estimate is about 45 instructions against the minimum's 13.5.** The extra,
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

  That is about 13.3 instructions a byte, **about 3.3 cycles a byte, 2.8× the bound**. Each
  term is an estimate good to about ±50%.

### What stands in the way, in the order to remove it once `proof` measures it

The estimate's error is larger than the gap between items, so the first measurement decides
the order. Anything above 1.33 cycles a byte on Turin (90% of the bound) is a bug against it.

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
3. **The keyword test, about 15 instructions an identifier, 4.5 a token.** A branch-free probe
   would take about 8: a key built from the length and the first two bytes indexes the table,
   then one masked 8-byte compare. That is `syntax_kind/keywords.rs` (format-grammar); filed
   in `v2/requests/format-lexer.md`. It takes the bound itself from 13.5 to about 11.5
   instructions a token.
4. **Bounds checks, about 4 instructions a token.** The classes' sentinel bits already stop
   every search at the end. Removing the remaining `bytes[at]` and `bytes.get(end)` checks
   would need an unsafe read, so it waits for a measurement that shows it matters
   (docs/v2/plan/design/unsafe.md: unsafe only for measured speed). The block form removes
   most of them anyway, because a start bit is in bounds by construction.

With 1 and 2 done, the estimate is about 18 instructions a token: the bound's 13.5 plus about
4.5 for the slice, address arithmetic and bounds checks in the inner loop. Adding the block's
cost and classification, that is 6.0 a byte, about 1.5 cycles a byte, or 80% of the bound.
With 4 as well, the estimate is about 14.5 a token and 5.0 a byte, about 1.25 cycles a byte,
which is within 10% of the bound. Item 3 lowers the bound and the design alike.

### `break 'a: loop {}` (`grammar/expressions/atom.rs`)

This adds one lookahead compare per `break`, which is free. The labeled value is parsed once
by the shared `labeled_expr`.
