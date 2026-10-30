---
status: plan
date: 2026-10-30
summary: no-fallback-tools (2026-10-30) - every fallback site in the store, query, cache, usercode, macro-eval, units, resolve, registry, vfs, diagnostics, fmt and the other non-link, non-front crates, classified error / rule / dead, with what each becomes.
---
# no-fallback: tools

The tools area of [no-fallback](../no-fallback.md): every crate under `crates/` except link's
(blaze-link, blaze-live, blaze-stencil, blaze-engine, blaze-cargo, blaze-build-script) and
front's (hir-def, hir-expand, hir, hir-ty, hir-ty-mir, hir-ty-codegen, hir-ty-matchck,
rustc_abi, mbe). Kinds are the lane's: **error** (becomes a typed error), **rule** (rustc's,
rustfmt's or cargo's own behavior, stays), **dead** (deleted). "done" marks a site changed in
the tree; "next" one classified and not yet changed.

## What changes what is built or whether a cache answers

| site | kind | what it does | becomes | state |
|---|---|---|---|---|
| `blaze-query/src/lib.rs` `Database::slot` | error | a key whose memos are in a corrupt image group got an empty slot, was computed again, and the group was only noted for the daemon to print | `slot` is `Result<Option<&Slot>, Corrupt>`; `resolve_held` and `add_memo` stop the run with it (`unreadable`, as `keep_input` did for files), so the request fails with `DbError::Corrupt` naming section and group, every time | done |
| `blaze-query/src/lib.rs` `Database::collect` (`slot_image_keys`) | error | after a collection every image key got an empty slot, so a corrupt key read before it would compute again afterwards | not run once a group is found corrupt (`ImageMap::first_corrupt`) | done |
| `blaze-query/src/lib.rs` `image_entries` / `write_image` | error | `for_each_key` skips a corrupt table group and `slot` errors were `continue`: the new image silently lacked what the group held | `write_image` is `Err(DbError::Corrupt)` while a group stands corrupt | done |
| `blaze-query/src/lib.rs` `learn_key`, `mark_edges`, `collect` roots, `settle`, `evict`, `memo`, `why_changed` | rule | callers of `slot` with no memo in memory to act on for a corrupt key | match `Ok(Some(..))`; the read noted the group and every request of the key fails with it | done |
| `blaze-query/src/lib.rs:416` `run_query` `Err(_) => Ok(None)` | rule | key bytes of another schema from an image are not decoded: the memo is unverifiable, its key computed under the new schema | stays: another schema is another key (the code number keys schema and producer), not a failed read | - |
| `blaze-query/src/tests.rs` `a_corrupt_group_fails_the_keys_in_it` | test | was `a_corrupt_group_drops_only_the_memos_in_it`, asserting the recompute | each key answers from the image or fails naming the group, again on a second ask, nothing reruns, `write_image` refuses | done |
| `blaze-cargo/src/serve/daemon.rs:654` | error | `eprintln!` "what it held was computed again" | link's: queued 01161 (now untrue; a corrupt image must fail, not start fresh) | queued |
| `blaze-cas/src/read.rs` `Look::kept` | error | a first record whose slot or header is not whole was passed over and an older tier's record of the key taken (a put then deduplicated against it and left the corrupt record in front) | the corrupt first record is no value: the put writes one that shadows it | done |
| `blaze-cas/src/remote.rs:995` `serve` | rule | a corrupt entry is answered as missing and pushed to `corrupt` | stays: the server has no valid value, and the corruption is reported to its owner | - |
| `blaze-cas/src/lib.rs:1172` settle `number.unwrap_or(NO_DICTIONARY)` | rule | a value is compressed without a dictionary when its kind has none whole | stays: zstd without a dictionary reads back the same bytes | - |
| `blaze-usercode/src/script.rs` `cargo_rule` | error | build-script output cargo refuses was kept in the store with a whole-package watch | `Result<CargoRule, blaze_build_script::Error>`; `Run::complete` fails with the new `RunError::Output { key, error }` and keeps nothing | done |
| `blaze-usercode/src/rule.rs` `CargoRule::package` | dead | only the fallback above used it | deleted | done |
| `blaze-macro-eval/src/code.rs:45` `path_for_file(..).unwrap_or_default()` | error | a file without a path hashed as `""` in the macro code-identity hash | `expect`: a source root lists only files it has paths of (`FileSet::iter` is its `paths` keys) | done |
| `blaze-macro-eval/src/expander.rs` `Library::saw` | error | a run leaving state where the store said the crate's macros keep none was an `eprintln!`, and the build went on with expansions run or reused as stateless | the expansion fails (`ProcMacroExpansionError::System` until hir-expand has a typed variant); the crate is told stateful first, so the next load uses a session | done |

| `blaze-usercode/src/trace.rs:446` `untraceable_of(what).unwrap_or(Untraceable::Descriptor)` | error | a name the sandbox wrote that this blaze doesn't know is taken as a descriptor read | the trace's error, naming the line, once `assemble_seen`'s error carries one (its error is the caller's `I::Error` now); conservative meanwhile (the run reruns as cargo would) | next |
| `blaze-usercode/src/inputs.rs:296` | rule | a file whose stat after hashing fails or changed is hashed again at its next check | stays: the hash given is the bytes read, only its memo is skipped | - |
| `blaze-usercode/src/index.rs:471`, `test/select.rs:617,656`, `blaze-testsel/src/select.rs:435` | rule | a put that finds another value at a probe slot takes the next slot | stays: open addressing in the store, bounded by the table | - |
| `blaze-units/src/read.rs:194` `patch_tables` | error | a `[patch.<registry-name>]` table (cargo looks the name up in `[registries]`) was skipped: the patch silently not applied | an error naming the table; looking the name up in the config's registries is the follow-up | done |
| `blaze-units/src/load/resolve.rs:74,83` | error | the root `Cargo.toml` and `Cargo.lock` were decoded lossily, where cargo refuses bytes that are not UTF-8 | `String::from_utf8`, its error the load's | done |
| `blaze-units/src/load/manifest.rs:123` `Parsed::Unparsed` | rule | the query keeps only that a manifest does not parse | stays: `Files::manifest` parses again for the error, which the load reports | - |
| `blaze-units/src/index.rs:320`, `blaze-registry/src/index.rs:102,107` `parse_line(..).ok()` | rule | an index line that does not parse is left out | stays: cargo's `Summaries` skips such lines (`IndexSummary` parse errors are logged and the line skipped) | - |
| `blaze-units/src/load/source/tree.rs:217` | rule | an unreadable manifest at the package's own directory, walking up for `[workspace]` | stays: the package's table was read by the caller already; any other directory's error is returned | - |
| `blaze-registry/src/registry.rs:282,315` | rule | cargo's index cache unreadable or of another version is read from the index again; a cache write that fails is a warning | stays: cargo's `Summaries::parse`, over the cache cargo shares | - |
| `blaze-registry/src/download.rs:163` | rule | a `.crate` in cargo's cache that is not of its sha256 (cut short) is removed and downloaded again | stays: cargo's cache, not blaze's store; the bytes used are checked against the index's sha256 either way | - |
| `blaze-registry/src/download.rs` `keep_crate_file` | error | with `keep_crate_files` asked for, a `.crate` that could not be written into cargo's cache was a warning | the download's error | done |
| `blaze-registry/src/git.rs:118,150,445,616`, `sparse.rs:148` | rule | fetch then re-clone, the GitHub fast path's error ignored, a submodule URL joined by text, a malformed nested manifest skipped, a cached `config.json` that does not decode fetched again | stays: each is cargo's `GitRemote::checkout`, `fetch`, `update_submodules`, `read_nested_packages`, `RemoteRegistry::config` | - |
| `blaze-resolve/src/workspace_resolve.rs:525` | rule | a query error while explaining why a patch matched nothing is a warning and no candidates | stays: cargo's `summary_for_patch`, on a path that ends in an error | - |
| `blaze-vfs/src/lib.rs:333,352`, `watch.rs:416,1028` `heads.get(..).unwrap_or(empty)` | rule | a head with no commit yet is at the empty tree | stays: the first commit to a head makes it | - |
| `blaze-vfs/src/image.rs:209-222` `Vfs::restore` | error | an image that does not open or that the journal does not start from, or whose disks section is refused, is passed over: the journal is loaded alone (same tree, slower), the reason only in `Recovered::image` | the daemon's to refuse (link's `serve/daemon.rs` reads `ImageUse`): queued 01172 | queued |

## Diagnostics, formatter and the rest

| site | kind | what it does | becomes | state |
|---|---|---|---|---|
| `blaze-lints/src/check.rs` `Driver::run` | error | a module file the caller's `NameOf` does not name was skipped whole: none of its lints ran | `check` is `Result<Vec<Found>, diag::Unnamed>`; the file is the error | done |
| `blaze-lints/src/diag.rs` `place_diag` / `place` | error | a diagnostic with a span in a file not named was dropped | `Result<(), Unnamed>` naming the file; `take` passes it up | done |
| `blaze-diagnostics/src/lower/parse.rs:113`, `parse/crate_attr.rs:56` `errors.unwrap_or_default()` | rule | `parse_errors` is `None` for a file without errors | stays: a true optional | - |
| `blaze-diagnostics` body lints returning early on `tainted()` / `mir_tainted()` (`panics.rs:86`, `unsafety.rs:48`, `patterns.rs:42`, `liveness.rs:74`, `calls/statements.rs:541`, `reachable.rs:38`) | rule | a body with errors is not linted | stays: rustc's `tainted_by_errors` | - |
| `blaze-diagnostics` `normalize(ty).unwrap_or(ty)` (`significant_drop.rs:300`, `values/invalid.rs:180`) | rule | a type that does not normalize is linted as written | stays: rustc's `try_normalize_erasing_regions(..).unwrap_or(ty)` | - |
| `blaze-diagnostics` layout `.ok()` (`externs.rs:205,340`, `statics.rs:65`, `types/pointers.rs:95`, `variants.rs:24`) | rule | a type whose layout errs is not linted for size or FFI | stays: rustc's `cx.layout_of(ty)` is `let Ok(..) else return` / `is_ok_and` in the same lints | - |
| `blaze-diagnostics` `expr_adjustment(..).unwrap_or_default()`, `blaze-lints` `adjustments(..).unwrap_or_default()` | rule | an expression without adjustments | stays: a true optional (`None` is none) | - |

