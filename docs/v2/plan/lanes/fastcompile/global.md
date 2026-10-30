---
status: plan
summary: blaze v2 fastcompile idea 8, one global cache (crates/blaze-global): which keys are public, how fetched values are trusted (Ed25519 records by a pinned publisher, then the content hash), the pipelined, batched, chunked HTTPS protocol, the cold build as one signed bundle, and the hooks other lanes are asked for.
date: 2026-10-29
---
# fastcompile-global: one global cache

Idea 8 of [compile-time.md](../../design/compile-time.md). What depends only on public inputs
(crates.io crate versions, std, sandboxed proc-macro and build-script runs, LLVM-optimized
functions) is computed once, by blaze's builders, and fetched by every machine instead of
built. The code is `crates/blaze-global/`; the boundaries are in [notes.md](notes.md).

## What is public (`public.rs`)

A store `Key` is a hash, so nothing in it says what it was made from. Publicness is judged where
a key is made: a `Rule` takes how the key was made (`QueryMade`, `RunMade`, `FunctionMade`, a
side table of a public function), makes the key again with its owner's own function
(`Key::of`, `blaze_usercode::index`, `blaze_codegen::query::keys`), and checks every input's
`Origin`. What passes is a `Public`, which no other code can make, and the client fetches and
publishes only those. The client asserts that every key the store asks it for was shown public.

- Public: a crates.io crate version by its lock checksum, the pinned toolchain's files, the
  builders' system image, and the outputs of public runs.
- Never public: a workspace member, a path or git dependency, another registry, a variable of
  the machine's environment read with a value, a run that read the whole environment or
  anything untraceable, or a dev (Cranelift) function.

`Secrets` holds the publisher's seed (raw and as hex) and every non-public environment value. A
value that contains any of them is refused before it is signed (`Client::publish`), and the
error names where the secret came from, never the secret.

## Trust (`sign.rs`)

A content hash proves the bytes but not that the key maps to them, and the cache is shared. Each
value has a `Record` (key, kind, content hash, length), signed with Ed25519 by a publisher key
this blaze pins (`sign::PINNED`). Ed25519 comes from `ring`, already a workspace dependency, so
no signature crate is added.

A fetched value enters the store only after two checks, in order:

1. The record's signature by a pinned key. Nothing is allocated for a value whose record fails
   it: the value is read past, so the chunk windows stay in step.
2. The bytes against the record's content hash.

A value that fails either check is answered `Done::Failed` with an error naming the key, so the
store reports `Fetched::Refused` and never uses it. It is never asked of anyone else.

Every count and length from the network is checked against what the body holds, or a limit,
before anything is allocated. A key's value never changes, so a replayed record is the same
record: there is no expiry or revocation to check.

## The protocol (`client.rs`, `server.rs`, `wire.rs`, `http.rs`, `tls.rs`)

HTTP/1.1 with `Content-Length` bodies, over rustls driven sans-IO through blaze-core's `Connect`,
`Send`, `Recv` and `Listen`.

- **Batching and pipelining.** One fetch names up to 256 keys of unknown length, or 4096 keys a
  manifest lists. A connection carries 4 requests at once, and the client opens up to 4
  connections while work is queued.
- **Response bounds.** A response takes its first value whatever its length, then further
  values while it stays under 64 MiB. Keys past that are answered *deferred* and asked again.
- **Chunked values.** Values travel as the store's FastCDC chunks. Each direction of a connection
  keeps the last 32 MiB of chunks, and a chunk seen again goes as a 9-byte reference, so
  near-equal values (the same crate rebuilt with one change) share bytes on the wire.
- **The client.** It is the store's `Remote`, so a fetched value arrives as the completion of
  `Store::fetch`, and the store re-checks the content hash as it does for any remote.
- **The server.** It keeps each value and, under `record_key`, its signed record, putting the
  value first. It checks a publish as a fetch is checked, and answers only once the puts are
  durable.

A malformed, oversized or unknown request is answered with its status and reason, and the
connection is closed.

## The cold build as a download (`bundle.rs`)

`Locked::parse` takes the lock file's crates.io packages that the build builds, with their
features. `BundleSpec` adds the toolchain, target and settings, and `BundleId` hashes all of it.
The builders publish every value of the cold build and then a `Manifest` with every record,
signed once. The server keeps a manifest only when it has the value and the identical record of
every entry.

`Download` asks for the manifest and checks its signature, then fetches everything listed that
the store lacks, in one `fetch_many`, spread across the connections. Each listed value is checked
against its listed record and its hash, with no per-value signature check.

## Tests (`tests.rs` and each module's own)

The simulation runs a client, the machine's store, and either a real server on a store of its
own or a server the test plays by hand. It covers:

- **Round trips:** fetch, deferral and re-asking, batching into 4 requests for 1000 keys, and
  publish.
- **Refusals:**
  - an unpinned key, a damaged signature, a content mismatch, or a record of another key;
  - damaged, truncated, reordered, short, oversized, chunked or non-HTTP responses;
  - unknown answers, piece counts past the body, oversized pieces, and hang-ups before and
    mid-response;
  - error statuses, and an unreachable cache.
- **Bit flips and cuts:** a flipped bit or a cut anywhere never gets a value kept with other
  bytes.
- **Secrets:** a value holding one is never sent.
- **Requests no client sends:** 405, 404, 413, 411 and 400.
- **Bundles:** the round trip (a key already present is not fetched), a missing bundle, a
  manifest by an unpinned key, a listed value with another record, and a manifest naming values
  the cache lacks.

The publicness rule is tested per input kind in `public.rs`.

## State of the art, matched or beaten

| system | what it does | here |
|---|---|---|
| Bazel remote cache, BuildBuddy, EngFlow (REAPI) | action-keyed CAS shared by a team; trust is "who can write" (auth) | the same action-keyed sharing, but trust is per value (a signature by a pinned builder key), so anyone may serve or mirror it and a write credential leaking can't poison anyone |
| sccache, ccache with a remote | compiler-invocation-keyed cache for a team | a key covers what rustc's command line misses (traced env, files read); sharing is global and limited to public inputs, decided by a checked rule rather than path heuristics |
| Nix binary caches (`narinfo` signatures, `trusted-public-keys`) | signed store paths from cache.nixos.org | the same model (Ed25519 by pinned keys, content-addressed values); beaten on granularity: per query, per build-script run and per optimized function, not per package |
| casync, desync, zsync | content-defined chunking for transferring trees | the store's own FastCDC chunks travel on the wire, so chunk identity is the store's, and nothing is re-chunked |
| rsync, zstd `--long` / dictionary windows | send what the peer lacks | a 32 MiB per-connection window both ends evict the same way, so references need no round trip |
| crates.io / docs.rs prebuilt artefacts (none for Rust builds today) | — | the cold build of a lock file's crates.io dependencies is one signed manifest and a parallel download |

## Hooks asked of other lanes

See `v2/requests/fastcompile.md`, `fastcompile-global` 1 to 4:

| lane | hook | why |
|---|---|---|
| store | make `content_of` and `chunk::chunks` public in `blaze-cas` | records name the store's own content hash, and the wire's pieces are the store's chunks |
| codegen | make `driver::stored` public | `Rule::function` remakes the release key |
| cargo | the daemon calls `Client::fetch` on a public miss and `Download` at cold start, and routes completions | the narrowest hook: the store's own `Remote` slot |
| ci | the builders' publish job, the publisher key as a secret, `sign::PINNED`, and the server with TLS | the publish side of the cache |
