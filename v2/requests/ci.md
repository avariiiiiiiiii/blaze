# Requests of lane ci

## 1. `crates/blaze-api/src/lib.rs`: the agent API's missing calls (schema 4)

Why: the brief and docs/v2/ci.md (#277) name calls step 0's `Request` lacks: open a view, what
an edit reaches, best-of-N, the production oracle, snapshots; and diagnostics "with their
facts". The types these name (`FactRead`, `Reach`, `ReachedItem`, `Ranked`, `Mismatch`) are
already in `lib.rs` (new public items, mine). Only the variants, the field and the number are
interface changes. Lane ci's session (`session.rs`) and blaze-ci are written against exactly
this; no other lane uses blaze-api.

```rust
pub const SCHEMA: u32 = 4;

pub struct Diagnostic {
    pub level: Level,
    pub code: Option<String>,
    pub message: String,
    pub spans: Vec<Span>,
    pub children: Vec<Diagnostic>,
    /// The facts the query that gave it read: what to change to make it go away, and why it
    /// came back after an edit.
    pub facts: Vec<FactRead>,
}

pub enum Request {
    // ... every existing variant as it is, then:
    /// Resolves a view to its root and keeps it open until [`Request::Close`] or the
    /// connection ends; answered with [`Response::Opened`].
    Open { view: ViewRef },
    Close { root: ContentId },
    /// What the change from `from` to `to` reaches (items, facts, functions compiled again,
    /// tests), building nothing; answered with [`Response::Reached`].
    Reach { from: ViewRef, to: ViewRef, targets: Targets },
    /// Each candidate's ops applied to `base` as a what-if, checked (and tested, with `test`),
    /// all at once: each candidate's responses as [`Response::Of`], then [`Response::Ranked`].
    BestOf { base: ViewRef, candidates: Vec<Vec<Op>>, targets: Targets, test: bool, filter: Vec<String> },
    /// The production oracle: the view built again from nothing and compared; answered with
    /// [`Response::Verified`].
    Verify { view: ViewRef, targets: Targets },
    /// The daemon's state made durable, before the VM snapshots itself; answered with
    /// [`Response::Snapshotted`] once it is.
    Snapshot,
}

pub enum Response {
    // ... every existing variant as it is, then:
    Opened { root: ContentId },
    Reached(Reach),
    /// A response of candidate `candidate` of a best-of-N.
    Of { candidate: u32, response: Box<Response> },
    /// Best first.
    Ranked(Vec<Ranked>),
    Verified { mismatches: Vec<Mismatch> },
    /// Every head's root, as made durable.
    Snapshotted { heads: Vec<(String, ContentId)> },
}
```

## 2. Root `Cargo.toml`: four workspace dependencies

Why: blaze-ci checks webhook signatures (HMAC-SHA256), signs the GitHub App's JWT (RS256) and
posts check runs over TLS. All four are in `Cargo.lock` already through blaze-cargo (ring
0.17.14, rustls 0.23.46, rustls-native-certs 0.8.5, base64 0.23.1), so nothing new is fetched.

```toml
base64 = "0.23"
ring = "0.17"
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
rustls-native-certs = "0.8"
```

## 3. `crates/blaze-vfs/src/lib.rs`: the empty root

Why: a CI head that was never written (the first push of a branch, the first PR) has no base
root to commit against: `Vfs::head` is `None`, and `commit` refuses an unknown base. blaze-next's
`Heads::root` gave the empty root for a head never written. Asked:

```rust
impl Vfs {
    /// The root of the empty directory: always known, the base of a head's first commit.
    pub fn empty(&self) -> ContentId;
}
```

Lane ci's session resolves `ViewRef::Head` of a head never written to `Vfs::empty()`
(blaze-next's rule: an unwritten head is the empty tree), so a client learns the base of a
head's first commit by `Open { view: Head(..) }`. The session calls `Vfs::empty` already: it
needs this to build.

## 4. Two arenas name an open view (for the coordinator to know)

`blaze_vfs::Vfs::open` and `blaze_query::Database::open` each return a `ViewId` (both
`Idx<View>`), from arenas of their own. The session opens a job's view in both and hands the
build both (`blaze_api::build::Opened`). If vfs and db agree on one arena, `Opened` loses a
field; nothing else changes.

## 5. The VFS has no symlinks or executable bit

A git tree's symlinks (mode 120000) and executable files (100755) have no `Op`. blaze-ci's
checkout refuses a tree with a symlink, naming the path (no-fallback), and writes an
executable as a plain file. A build script that runs a checked-in script, or a crate with a
symlinked file, needs `Op::Write` to carry a mode (lane vfs).

---
Applied by the coordinator, 16:46.
