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
