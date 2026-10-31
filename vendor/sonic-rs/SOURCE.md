# sonic-rs

- Upstream: https://github.com/cloudwego/sonic-rs, crates.io `sonic-rs` 0.5.10 (sha256
  756586caeeb21126a57c17d1ef2b9386e2613ba784aae0f27104f5e9982335ec).
- Licence: Apache-2.0 (LICENSE), with the licences of the code it adapts in licenses/.
- Left out: Cargo.lock, Cargo.toml.orig, docs/, scripts/, profile/ and the repository's tool
  configs. The dev-dependencies stay in Cargo.toml, unused: the root manifest excludes it from
  the workspace, so its own tests are not built.

Changes, marked `blaze:` in src/parser.rs:

1. `Parser::skip_escaped_chars` checks that the four bytes after `\u` are hex digits. Upstream
   ate five bytes unchecked, so a string skipped with validation (`skip_one(true)`, what
   `deserialize_ignored_any` uses) such as `"\u"83d\ude00"` swallowed its closing quote and an
   invalid body decoded where serde_json refuses it (blaze-ci
   `webhook::tests::sonic_rs_takes_and_refuses_the_payloads_serde_json_did`, queue 01209). A
   surrogate pair is not checked when skipping, as serde_json's `ignore_escape` does not
   either. Report upstream.
