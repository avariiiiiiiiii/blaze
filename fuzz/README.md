# blaze-fuzzer

Coverage-guided fuzz targets for every parser in blaze that reads bytes nobody vouches for: the
store's packs and the remote protocol, the agent API's CBOR, the linker's object files and
archives, the VFS journal, and wasm proc macros. The engine is blaze's own (`src/`), so no
libFuzzer and no rustc are involved.

## Running a target

A fuzz build needs AddressSanitizer and SanitizerCoverage, in rustc's flag names, for every
crate of the build:

```sh
export RUSTFLAGS="-Zsanitizer=address -Csanitizer-coverage-level=3 \
  -Zsanitizer-coverage=inline-8bit-counters,pc-table,trace-cmp -Cdebug-assertions"
blaze build --release -p blaze-fuzzer --bin cas_pack
target/release/cas_pack fuzz                  # for the target's budget
target/release/cas_pack fuzz --seconds 60 --seed 1
```

A binary built without those flags refuses to `fuzz` and names the missing flag; it does not
run without coverage instead. Every target binary takes these commands:

| command | what it does |
|---|---|
| `fuzz [--seconds N] [--runs N] [--seed N] [--max-len N] [--timeout-ms N] [--no-value-profile] [--out DIR]` | fuzzes until the budget (the target's `budget`, or `--seconds`) or `--runs` is spent, or an input fails |
| `run FILE...` | runs each input once, catching nothing: a failure ends the process as it would anywhere |
| `minimize FILE [--seconds N] [--timeout-ms N]` | finds the smallest input that still fails the same way (a failure, or a timeout) |
| `seeds` | runs the seed corpus and its deterministic changes, as the `#[test]` does |

What a run keeps goes to `fuzz/out/<target>/` (ignored by git): `corpus/` (inputs that reached
something new, read again by the next run), `crash-<hash>.hex`, `timeout-<hash>.hex` (an input
that ran longer than `--timeout-ms`, 10 s by default) and `minimized-<hash>.hex`. Every file is
the input in hexadecimal, as the seed corpus is. The engine stops at the first failure and
prints the commands that replay and minimize it.

AddressSanitizer runs with `abort_on_error=1`, `allocator_may_return_null=0` and
`max_allocation_size_mb=2048` (one allocation past 2 GiB is a finding), and without the leak
check; `ASAN_OPTIONS` overrides these.

## The seed corpus, and the plain test

`fuzz/corpus/<target>/*.hex` holds each target's seeds, one input per file in hexadecimal with
whitespace ignored (what `crates/blaze-fuzz`'s `inputs` reads). Each target also builds valid
inputs through the writer side of its API (`Target::seeds`). Every target's `#[test]`, run in
the merge gate without coverage by `blaze test -p blaze-fuzzer`, feeds `check` the corpus files,
the built seeds and 20000 deterministic changes of them (`BLAZE_FUZZ_RUNS` changes the count).
To keep an input a fuzz run found, copy it from `fuzz/out/<target>/` into the target's corpus.

## The engine

- **Coverage**: SanitizerCoverage's inline 8-bit counters, one per block, registered by
  `__sanitizer_cov_8bit_counters_init`, and its PC table (`__sanitizer_cov_pcs_init`). A run's
  features are each counter it reached, in AFL's buckets of hit counts (1, 2, 3, 4-7, 8-15,
  16-31, 32-127, 128+), and with the value profile the comparisons it made: per site, the
  Hamming distance of the operands and the leading zeros of their difference, as libFuzzer's
  `-use_value_profile` (`__sanitizer_cov_trace_cmp*`, `_switch`, `_div*`, `_gep`). An input is
  kept when a feature is new. The engine's own code is instrumented too; the PC table tells
  which counters are its callbacks', and those are dropped.
- **Mutations**: libFuzzer's: erase, insert and repeat bytes, change a byte or a bit, shuffle,
  change an integer in ASCII or in binary (either byte order), copy or insert a part, cross over
  and splice two corpus inputs, words from the target's dictionary, and operands of recent
  comparisons (2, 4 and 8 byte compares, and `memcmp`/`bcmp` operands through AddressSanitizer's
  `__sanitizer_weak_hook_memcmp`): where one operand is in the input, the other replaces it.
  One to five of them are stacked on a corpus input picked with weight for the features it was
  first to reach, newer inputs weighted more.
- **Failures**: a panic, or an AddressSanitizer report (`__asan_on_error`), writes the input
  being run; a watchdog writes one that runs past the timeout and ends the process. `minimize`
  runs each try in a child process (`<target> run`), cutting chunks of halving size and then
  zeroing bytes while the failure stays the same.

## Targets

Each target's file (`targets/<target>.rs`) begins with the invariants it checks.

| target | parser | checks |
|---|---|---|
| `cas_pack` | blaze-cas packs: header, the record walk at open, reads | read never panics or hangs; no allocation past a bound the disk's length implies; a verified read of damaged bytes is `Corrupt`/`Mismatch`, never other bytes; values put, flushed and settled read back the same |
| `cas_head` | blaze-cas `Head::decode`, and what open maps by it | as `cas_pack`, for heads written by the store and damaged |
| `cas_index` | blaze-cas pack index tables: header, control column, slots, lookups, scrub | as `cas_pack`, for index tables built as the store lays them out and damaged |
| `cas_entry` | one blaze-cas record and its payload in each form (raw, chunked, zstd, LZ4, external) | as `cas_pack`; lengths the bytes can't hold are refused |
| `cas_cold` | the cold tier: zstd with and without a dictionary, LZ4, dictionary records loaded at open | as `cas_pack`; a frame naming 2^40 bytes is refused, not allocated |
| `cas_remote` | the remote protocol: `Answer::decode`, the `Client`'s frames into `Store::complete`, the `Server`'s asks | decode then encode is the identity; every fetch is answered once, as the frame says; allocation bounded by what was received; a server's answers are `Answer::of` in order |
| `api_cbor` | blaze-protocol's deterministic CBOR: `cbor::from_slice`, `cbor::check`, `cbor::reference` | `check` allocates nothing; decoding is bounded; bytes decode exactly when `check` passes them (what decodes is canonical); the fast and reference decoders agree on every input and every shape; a decoded item encodes back to exactly the input |
| `api_cbor_encode` | the CBOR encoders, driven by a program in the input | `to_vec`, a kept `Writer`, `append` and the reference give the same bytes or error; the output is canonical and decodes back to itself; frames round-trip |
| `api_frame` | blaze-protocol's `read_frame`, `read_frame_into`, `receive`, from a slice and a reader that trickles and is interrupted | exactly the frames a plain walk finds, then the error the walk says; bounded; `write_frame` writes the stream back |
| `api_frames` | blaze-api `frames::Frames`, fed whole, a byte at a time and in pieces | the same items however the bytes arrive; past `LONGEST_REQUEST` it stays broken with the exact error; bounded |
| `api_envelope` | blaze-api `frames::decode` of `Envelope<Request>` (the daemon) and `Envelope<Response>` (the client), `frames::encode` | bounded; fast and reference decoders agree; what decodes is canonical; `Ok` exactly in this `SCHEMA`, otherwise the exact refusal; encode then decode gives the envelope back |
| `api_http_request` | blaze-api `http::RequestParser`, whole and byte by byte | the same requests and errors either way; a failed parser stays failed; bounded; heads well formed; a request written back parses to itself |
| `api_http_response` | blaze-api `http::ResponseParser` and `http::read_response` | as `api_http_request`, for the client's side |
| `link_elf_object`, `link_elf_archive`, `link_elf_shared` | blaze-link's ELF reader: relocatable objects, `ar` archives, shared objects and linker scripts (`blaze_link::fuzz_read`) | never panics (no read past the input: ASan in the fuzz build, the slice's bounds check in the plain test); bounded; reads the same at any alignment; an accepted input stays accepted with bytes appended (nothing past what it names is read); archives read alike with and without `whole`, shared objects with and without `as_needed` |
| `link_macho_object`, `link_macho_archive`, `link_macho_dylib`, `link_macho_trie`, `link_macho_tbd` | blaze-link's Mach-O reader: objects (thin and fat), archives, dylibs and stubs, export tries alone, `.tbd` text | as the ELF targets; an export trie reads alike through `LC_DYLD_INFO_ONLY` and `LC_DYLD_EXPORTS_TRIE` |
| `link_coff_object`, `link_coff_archive`, `link_coff_import` | blaze-link's PE/COFF reader: objects and short imports, `lib` archives, import libraries | as the ELF targets |
| `link_wasm_object`, `link_wasm_archive` | blaze-link's wasm object and archive reader | as the ELF targets |
| `vfs_journal_bytes` | `Vfs::recover` of any bytes | never panics; bounded; records and skipped bytes fit the input; every recovered head is a root the tree has, whose files list, read and follow; recovering twice gives the same; one more move settles durably and recovers exactly |
| `vfs_journal_records` | `Vfs::recover` of records framed with good checksums (forged directories, heads, removals, sequence numbers) | as `vfs_journal_bytes`, past the checksum |
| `vfs_journal_model` | a program of commits on a simulated disk, cut, torn or damaged where the input says, then recovered | recovery gives the state after a prefix of the committed transactions (the whole sequence when nothing was damaged), and per head nothing that was not committed |
| `macro_wasm_validate` | any bytes as a wasm proc-macro module file, through `Wasm::at` | validation and loading never panic; bounded |
| `macro_wasm_host` | a fixed guest that calls every host and WASI import with pointers, lengths and numbers from the input | the host never panics and never reads or writes outside the guest's memory (every guest pointer and length is checked); bounded |
| `macro_wasm_wire` | a fixed guest that outputs the input, decoded as the token-tree wire | never panics; bounded; at most two trees per 15 bytes |
| `macro_wasm_lex` | `blaze_macro_wasm::tokens`, the host's lexer for `ts_from_str` | never panics; bounded; at most 2 trees per byte, plus one |
| `usercode_wire` | blaze-usercode's fork-server frames (`test::wire::Frames`, `Frame::decode`), what a test binary writes | pushed whole, a byte at a time, as `needed` asks and in the input's pieces, `pop` gives a plain walk's frames to the first error; a length past `LONGEST` is an error, never an allocation; decode is canonical; bounded |
| `usercode_script_output` | `blaze_build_script::output::parse`, a build script's stdout | reads as its directives alone, by cargo's rule, and as its lines one at a time; kept texts are one line and never name a moved `OUT_DIR`; bounded |
| `usercode_script_elide` | `blaze_build_script::elide`, a build script's source and its lexer | the lexer's tokens are the source's own text in order; what is elided verifies against itself and refuses anything appended; bounded |
| `usercode_script_verify` | `blaze_build_script::verify` of a run's output against the elided one | `Ok` exactly when they are equal, else the error naming the first differing line; bounded |
| `usercode_trace_record`, `usercode_trace_report` | the sandbox trace's lines (`trace::parse`) and reports (`trace::decode`) | agree with a model of the grammar; round trip; reading in pieces equals reading whole; bounded |
| `usercode_index_shape`, `usercode_run_value` | `index::Shape::decode`, `run::Value::decode` | decode then encode is the identity; bounded |
| `codegen_asm` | `hir_ty_codegen::Assembled::from_bytes`, assembled code kept in the store | `None`, or `to_bytes` is the input; at most `4 * len + 1 KiB` allocated at once |
| `hir_item_tree`, `hir_expr_store`, `hir_body`, `hir_store_source_map`, `hir_body_source_map` | `Persist::read` then `decode()` of the persisted item tree, expression store, body and their source maps | `Err`, or a value written back the same (for column edits of a valid buffer, exactly the input); decoded syntax contexts are compared and dropped, never given to a database; bounded |

Several seed corpora hold inputs that show a bug their owner has not fixed yet (each listed in
the owner's `v2/requests/<owner>.md`); those targets' `#[test]`s fail until it is fixed.
