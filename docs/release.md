# Releases: canary and stable

blaze has two release channels, made of the same releases:

- **canary** (opt-in): every release, as soon as it is built. A release is cut from a main whose
  merge gate (`ci`) is green: the unit tests, the build, and the fast part of the smoke corpus
  below. It is published as a GitHub *prerelease*.
- **stable** (the default): a canary that then passed the heavy tests, unchanged. Promotion
  builds nothing: the archives users get on stable are the bytes that were tested as canary.

## Versions

A release is `v0.YYYYMMDD.N` (`0.YYYYMMDD.N` in Cargo.toml and `blaze --version`), N counting
the releases of that day (UTC) from 0. `.github/scripts/train_release.py` picks the number.

A version is one release on one tag, for good: **promotion flips that same release** from
prerelease to stable and makes it GitHub's *Latest*. No second tag is made and no asset is
uploaded again, so a pin to a version (`--version 0.20261028.0`, or a workspace's
`[workspace.metadata.blaze] version`) names the same bytes before and after promotion. What
moves is two pointers:

| channel | which release | how it is found |
|---|---|---|
| stable | the newest promoted release | GitHub's `releases/latest`, which never points at a prerelease |
| canary | the newest `v0.YYYYMMDD.N` release, prerelease or not | the repository's release feed (`releases.atom`) |

Canary is the newest release of either kind: every stable release was a canary first, so when
the newest canary is promoted the two channels name the same release.

A release older than this scheme (`v0.20261027.0`, the `nightly` build of 2026-10-20) was
published as a full release and counts as stable.

## Cutting a canary

`release-cut.yaml` (by hand, on main) or the release train's `release` job commits
`release: v0.YYYYMMDD.N` and tags it; `release.yaml` builds the archives on the tag for macOS
(Apple silicon) and Linux (x86-64, ARM64) and publishes them as a prerelease with `SHA256SUMS`
and the tag's CHANGELOG.md section as its notes. PGO is off for now (#870): the release is built
with `--no-pgo`.

## Promotion to stable

`promote.yaml` runs when a prerelease is published, and by hand for a given tag (to run again
after a flake, or after fixing the gate itself). Against the published archives, installed with
`install.sh --version <v>` as a user installs them, on macOS (Apple silicon) and Linux (x86-64
and ARM64):

- the install checks of install.yaml (the programs are there and run, `blaze new` + `blaze run`);
- the whole smoke corpus (below), dev and `--release`, `blaze run` and `blaze test`;
- the train's tiers that run against a release build.

When every job passes it marks the release stable and *Latest*, and appends to its notes the
line `Promoted to stable on <date> after <run URL>`. When one fails the release stays a canary
for good, and an issue is opened (or commented on, for the same failing case) with the release,
the case, the platform, the command to reproduce it and the run's link. A later canary that
passes is promoted over it; stable never moves backwards.

## The smoke corpus

`tests/smoke/` holds small programs that must build and run correctly with `blaze run` and
`blaze test`, in dev and `--release`, each with the output it must print. `harness/smoke.py`
runs them against a blaze (`BLAZE_BIN`, or `blaze` on PATH for an installed release).

- **std** (no dependencies; the merge gate runs these on every pull request, under two minutes):
  `HashMap` insert and iterate, `thread::spawn` + `join`, `thread_local!`, `Mutex`/`OnceLock`/
  atomic statics, `std::env` and `std::fs` basics.
- **crates** (from crates.io, at the versions their Cargo.lock pins): tokio (multi-threaded
  runtime, a TCP echo on localhost), a serde_json round trip, proc-macro derives (thiserror,
  clap's derive).
- **projects** (real repositories at pinned commits): ripgrep, indexable-inc/anim, and blaze
  itself.

The std cases are the merge gate's; all three groups are the promotion gate.

## Installing a channel

```sh
curl -fsSL https://ix.dev/blaze | sh                         # stable
curl -fsSL https://ix.dev/blaze | sh -s -- --channel canary  # the newest release
curl -fsSL https://ix.dev/blaze | sh -s -- --version 0.20261028.0
```

`BLAZE_CHANNEL=canary` is the same as `--channel canary`; `--version` wins over a channel. A
workspace's pin, `[workspace.metadata.blaze] version`, takes a version or `"stable"` /
`"canary"` too.
