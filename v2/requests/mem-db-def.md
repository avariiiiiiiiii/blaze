# mem-db-def's requests

`mem-db-def` (child of `mem-db`, lane `mem`) moves `crates/base-db/`, `crates/hir-expand/`,
`crates/hir/` and `crates/hir-def/` (but format's files) onto `crates/blaze-mem`. Every path is
lane db's (children not DONE), so each change is written here for its owner to apply. The
contract it programs against is "The contract" in `docs/v2/plan/lanes/mem/db.md`; the plan and
the speed-of-light bounds are `docs/v2/plan/lanes/mem/db-def.md`.

This file holds the shared harness (H1 to H3, for all four crates) and base-db (B1 to B8). The
other parts are in the children's files, each written against H1 to H3:

| part | request file | owner told |
|---|---|---|
| def-map collection (`nameres.rs`, `nameres/` but `path_resolution.rs`) | `v2/requests/mem-db-def-collect.md` | `db-expand-names-next1`, `frontend-nameres-next1` |
| path resolution, scopes, `ItemScope`, `Resolver`, `visibility`, `find_path` | `v2/requests/mem-db-def-resolve.md` | `db-expand-names-next1`, `db-expand-next2` |
| the rest of hir-def (signatures, attrs, `lang_item`, import map, `macro_reach`, `deps/`) | `v2/requests/mem-db-def-items.md` | `db-expand-next2` |
| hir-expand | `v2/requests/mem-db-def-expand.md` | `db-expand-macros-next1` |
| hir | `v2/requests/mem-db-def-hir.md` | `db-next2` |

Programmed against two requested signatures: `blaze_mem::frame::current<R>(f: impl FnOnce(&Frame)
-> R) -> R` (mem-db's request 1 of lane mem) and `ContentId::of_many_into` (B3 below, of
`mem-format`). Line numbers are as read on 2026-10-29; the function named is the anchor.

---

## H1. Manifests (owners: `db-expand-inputs2-next1` for base-db, `db-expand-macros-next1` for hir-expand, `db-expand-next2` for hir-def, `db-next2` for hir)

Each of the four `Cargo.toml`s, in `[dependencies]` beside the local deps:

```toml
blaze-mem.workspace = true
hashbrown.workspace = true
```

and in `[dev-dependencies]` (base-db has it already):

```toml
blaze-alloc-count.workspace = true
```

Why: every change below names `blaze_mem::{frame, scratch, Tag, AppendVec, Column}`; frame maps
are `hashbrown::HashMap<K, V, FxBuildHasher, &Frame>`, whose `A: Allocator` needs hashbrown's
`nightly` feature (mem-db's root request; `hashbrown` is already a root workspace dependency,
`Cargo.toml:294`). Run `tools/v2/manifest_lint.py` after.

## H2. `#![feature(allocator_api)]` (same owners)

Each crate names `Vec<T, &Frame>`, so each `src/lib.rs` gets the feature beside its other crate
attributes:

- `crates/base-db/src/lib.rs:5-6`, before `#![forbid(unsafe_code)]`:
  ```rust
  #![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
  #![feature(allocator_api)]
  #![forbid(unsafe_code)]
  ```
- `crates/hir-expand/src/lib.rs:6-9`: `#![feature(allocator_api)]` after the `cfg_attr` line.
- `crates/hir/src/lib.rs:20-21`: `#![feature(allocator_api)]` after the `cfg_attr` line.
- `crates/hir-def/src/lib.rs:10`: `#![feature(allocator_api)]` after the `cfg_attr` line.

base-db and hir-expand keep `#![forbid(unsafe_code)]`: nothing here needs unsafe (blaze-mem's
API is safe).

## H3. One counting allocator per test binary (same owners)

A test binary has one global allocator, and every budget test of a crate shares it.

- **hir-def**: today `crates/hir-def/src/nameres/tests/paths.rs:745-805` installs its own
  (`#[global_allocator] static ALLOCATOR: alloc_count::Counting` and `mod alloc_count`, a copy
  of what `blaze-alloc-count` is). Delete those lines, and in `crates/hir-def/src/lib.rs`
  after `mod test_db;` (`:53-54`) add:
  ```rust
  #[cfg(test)]
  #[global_allocator]
  static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
  ```
  `resolved_path_allocates_nothing` (`paths.rs:723-743`) changes its count to the crate's:
  ```rust
  // before
  let (res, allocations) = alloc_count::count(resolve);
  assert_eq!(Res::from(res), Res::from(warm));
  assert_eq!(allocations, 0);
  // after
  let res = blaze_alloc_count::assert_at_most(0, "resolving a path on a warm def map", resolve);
  assert_eq!(Res::from(res), Res::from(warm));
  ```
- **hir-expand**: the same three lines at the top of `crates/hir-expand/src/lib.rs`'s
  `#[cfg(test)] mod tests` (`:1961`), as `#[global_allocator] static ALLOC: ...` inside it.
- **hir**: the same three lines in `crates/hir/src/lib.rs` after `mod tests;` (`:41-42`).
- **base-db**: has it (`crates/base-db/src/tests.rs:30-31`); `src/db/tests.rs` is in the same
  binary.

---

## base-db (owner: `db-expand-inputs2-next1`)

What base-db takes from the general-purpose allocator, before this change:

| what | when | after |
|---|---|---|
| a file's text `Arc<str>` (`files.rs` `write_text`, `:275`) and its `Reparse` slot (`reparse.rs:80`) | each text set | unchanged: an input's value has its own end (replaced by the next text), so it is owned (contract 4, lane mem's ruling), each at its exact size |
| the setters' temporaries: `changed` lists, `RootContent::refresh`'s three buffers and two result vectors, `hash_ranges`' input list, `of_many`'s three buffers, `set_view_root`'s plan (five vectors, a set) and write (four vectors, a set per root), `named::publish`'s two vectors | every keystroke (`set_file_text`) and every view set, changed or not | the setter's frame (B1, B2) and the hasher's scratch (B3): 0 |
| the view's base as an owned `VfsPath` key, and a `HashMap` remove and insert of its index | every `set_view_root`, even one that writes nothing | a scan of the few view bases, no key made (B2): 0 |
| `Column` pages, `Log` chunks, `Table` shard tables (`table.rs`) | the first ID in a page, a chunk, a table level | reserved regions tagged `Tag::Query` (B4) |
| `run_on_pool`'s `round` and `again` (`db.rs:381-382`) | every batch (each round of macro expansion, each check) | the worker's scratch (B5): 0 |
| `transitive_deps`, `transitive_rev_deps` (`input.rs:874`, `:896`): a worklist, a set, a list and, for reverse deps, the whole inverted crate graph | every call, from queries and every IDE request that asks | memoized queries (B6): a hit is 0 |
| `source_root_crates`' result (`lib.rs:497`) collected from a `Filter` (grown by doubling, then shrunk) | each miss | exact size from the frame (B7) |
| the setters' own state (`Writer`'s maps, `RootContent`'s per-file keys and bucket maps) | only when the file set grows | unchanged, owned: it grows with the file set and is edited in place by an edit |

The tag of every setter's task is `Tag::Parse` (contract 3: base-db's file inputs); of the
tables `Tag::Query` (contract 5).

### B1. Every setter is a task of its own (`files.rs`, `named.rs`, `content.rs`)

A setter runs outside any query (on `&mut Db`, from the driver or a test), so it opens its own
`blaze_mem::frame::task(Tag::Parse, |frame| ..)` and threads `frame` down. Nothing it puts in
the frame leaves it: what is kept (texts, slots, new source roots, `RootContent`'s keys) is
owned, what is returned (`ViewChange`) is copied out at its exact size.

`files.rs`, imports (`:11-36`): add
```rust
use blaze_mem::{Frame, Tag, frame};
```

`Files::set_appended_root_module` (`files.rs:195-213`), body after:
```rust
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            let input = self.file_text(file_id);
            if input.appended_root_module(&*db) == module {
                return;
            }
            let content = FileText::content_of(input.text(&*db), module);
            input.set_appended_root_module_field(db).with_durability(durability).to(module);
            input.set_content_field(db).with_durability(durability).to(InputId(content));
            let mut changed = Vec::new_in(frame);
            index_file(&mut writer, file_id, content, &mut changed);
            self.write_root_contents(&mut writer, db, &changed, frame);
        })
```

`Files::set_text` (`files.rs:216-233`), body after:
```rust
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            writer.view_ids.remove(&file_id);
            let appended =
                self.input_of(&writer, file_id).and_then(|it| it.appended_root_module(&*db));
            let file = ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes());
            let content = FileText::content_of_file(file, appended);
            self.write_text(&mut writer, db, file_id, Some((text, file, content)), durability);
            let mut changed = Vec::new_in(frame);
            index_file(&mut writer, file_id, content, &mut changed);
            self.write_root_contents(&mut writer, db, &changed, frame);
        })
```

`Files::write_root_contents` (`files.rs:308-326`) takes the frame and passes it on:
```rust
    pub(crate) fn write_root_contents(
        &self,
        writer: &mut Writer,
        db: &mut Db,
        roots: &[SourceRootId],
        frame: &Frame,
    ) {
        RootContent::refresh(&mut writer.root_contents, roots, frame);
        // ... the rest as it is
```

`Files::index_root` (`files.rs:330-360`) takes `frame: &Frame`; its `files` list goes in the
frame and each key is made at its exact size (B2's `path_key`):
```rust
        let mut files = Vec::with_capacity_in(root.len(), frame);
        // ... loop as it is, pushing (file, content::path_key(path), self.entry_content(db, file))
        RootContent::new(root.is_library, files, old.as_ref(), frame)
```

`Files::set_source_root_with_durability` (`files.rs:544-574`): the body inside
`frame::task(Tag::Parse, |frame| { .. })`, with `self.index_root(&mut writer, &*db,
source_root_id, &source_root, frame)`, `RootContent::refresh(&mut writer.root_contents,
&[source_root_id], frame)` and `writer.named.publish(db, frame)`.

`index_file` (`files.rs:649-663`):
```rust
pub(crate) fn index_file(
    writer: &mut Writer,
    file: FileId,
    content: ContentId,
    changed: &mut Vec<SourceRootId, &Frame>,
) {
```

`RootContent::new` (`files.rs:697-743`): `files: Vec<(FileId, Box<[u8]>, ContentId), &Frame>`
and `frame: &Frame`; `buckets` and `hash` are frame vectors, the path hashes a frame slice,
and the key is moved, not cloned, into `keys` (the bucket map takes the one clone):
```rust
    fn new(
        is_library: bool,
        files: Vec<(FileId, Box<[u8]>, ContentId), &Frame>,
        old: Option<&RootContent>,
        frame: &Frame,
    ) -> RootContent {
        let mut buckets = Vec::with_capacity_in(files.len(), frame);
        let mut hash = Vec::new_in(frame);
        for (at, (file, key, _)) in files.iter().enumerate() {
            match old.and_then(|old| old.keys.get(file)) {
                Some((bucket, known)) if **known == **key => buckets.push(*bucket),
                _ => {
                    buckets.push(0);
                    hash.push(at);
                }
            }
        }
        if !hash.is_empty() {
            let keys = frame.alloc_slice_from_iter(hash.iter().map(|&at| &*files[at].1));
            let hashes = frame.alloc_slice_fill_with(keys.len(), |_| ContentId::default());
            ContentId::of_many_into(SourceRoot::PATH_DOMAIN, keys, hashes);
            for (&at, path) in hash.iter().zip(&*hashes) {
                buckets[at] = RootContent::bucket(path);
            }
        }
        let mut content = RootContent::empty(is_library);
        content.keys.reserve(files.len());
        for ((file, key, id), bucket) in files.into_iter().zip(buckets) {
            // clone: the bucket map's copy of the key, kept with the root.
            content.buckets[bucket as usize].insert(key.clone(), id);
            content.keys.insert(file, (bucket, key));
        }
        // ... the `old` reuse as it is
```

`RootContent::refresh` (`files.rs:796-849`):
```rust
    pub(crate) fn refresh(
        contents: &mut FxHashMap<SourceRootId, RootContent>,
        roots: &[SourceRootId],
        frame: &Frame,
    ) {
        let mut bytes = Vec::new_in(frame);
        let mut ranges: Vec<Range<usize>, &Frame> = Vec::new_in(frame);
        let mut places = Vec::new_in(frame);
        // ... first loop as it is
        let hashes = content::hash_ranges(SourceRoot::BUCKET_DOMAIN, &bytes, &ranges, frame);
        for (&(root, bucket), &hash) in places.iter().zip(&*hashes) {
            // ... as it is
        }
        bytes.clear();
        ranges.clear();
        let mut refreshed = Vec::new_in(frame);
        // ... second loop as it is
        let hashes = content::hash_ranges(SourceRoot::DOMAIN, &bytes, &ranges, frame);
        for (root, &hash) in refreshed.into_iter().zip(&*hashes) {
            // ... as it is
        }
    }
```
`RootContent::bucket_bytes(&self, bucket, out: &mut Vec<u8>)` becomes
`out: &mut Vec<u8, &Frame>`; `FileText::content_bytes` (`lib.rs`, `out: &mut Vec<u8>`) and
`content::opt_bytes` / `content::frame` likewise take `&mut Vec<u8, &Frame>`.
`cfg_options_content` (`content.rs:28`) is the one other caller of `frame`: it is called once per
crate graph change (setup), so give it its own `frame::task(Tag::Parse, |frame| ..)` with
`bytes` and `atoms` as frame vectors rather than a second, global form of `frame`.

`content.rs`:
```rust
/// A path as content, at its exact size: whether it is on disk (0) or virtual (1), then its text.
pub(crate) fn path_key(path: &VfsPath) -> Box<[u8]> {
    blaze_mem::scratch::with_bytes(|key| {
        key.push(if path.as_path().is_some() { 0 } else { 1 });
        write!(key, "{path}").expect("writing to a Vec does not fail");
        Box::from(&key[..]) // alloc: the key a source root's content keeps
    })
}

/// [`path_key`] appended to `out`, for keys that are only hashed.
pub(crate) fn path_key_into(path: &VfsPath, out: &mut Vec<u8, &blaze_mem::Frame>) {
    out.push(if path.as_path().is_some() { 0 } else { 1 });
    write!(out, "{path}").expect("writing to a Vec does not fail");
}

/// `ContentId::of(domain, &buf[range])` for each range, in the frame: the domain's key derived
/// once for all of them, the inputs laid out in one buffer.
pub(crate) fn hash_ranges<'f>(
    domain: &'static str,
    buf: &[u8],
    ranges: &[Range<usize>],
    frame: &'f blaze_mem::Frame,
) -> &'f [ContentId] {
    let out = frame.alloc_slice_fill_with(ranges.len(), |_| ContentId::default());
    if !ranges.is_empty() {
        let inputs = frame.alloc_slice_from_iter(ranges.iter().map(|range| &buf[range.clone()]));
        ContentId::of_many_into(domain, inputs, out);
    }
    out
}
```
(`impl std::io::Write for Vec<u8, A>` holds for any `A: Allocator`, so `write!` into a frame
vector works.) The old `path_key` built `vec![..]` then grew it by `write!` and the caller
shrank it with `into_boxed_slice` (a doubling and a shrink per key); the new one writes into
the worker's byte buffer and copies once at the exact size.

`named.rs` `Named::publish(&mut self, db: &mut Db)` (`:138-189`) becomes
`publish(&mut self, db: &mut Db, frame: &Frame)`, its keys hashed from one frame buffer:
```rust
        let table = &self.table;
        let mut bytes = Vec::new_in(frame);
        let mut ranges: Vec<Range<usize>, &Frame> = Vec::with_capacity_in(self.pending.len(), frame);
        for &(_, at) in &self.pending {
            let start = bytes.len();
            content::path_key_into(&table.get(at).path, &mut bytes);
            ranges.push(start..bytes.len());
        }
        let ids = content::hash_ranges(SourceRoot::PATH_DOMAIN, &bytes, &ranges, frame);
        // ... `for ((hash, _), id) in self.pending.iter().zip(ids)` as it is
```
Its callers (`Files::set_source_root_with_durability`, `Files::write_view`) pass their frame.
The 64 bucket inputs made once per database stay as they are (setup).

### B2. `set_view_root` in its task, with no key made per call (`view.rs`, `files.rs`)

`Writer::views` (`files.rs:102`) becomes a list, since a database has one view base per
checkout and a scan of a handful compares paths without making one:
```rust
    /// The index of each view base's paths that [`Files::set_view_root`] keeps.
    pub(crate) views: Vec<(VfsPath, ViewIndex)>,
```
(`Writer::note_path`'s `for (base, index) in &mut self.views` and `self.views.clear()` read the
same on a `Vec` of pairs.)

`Files::set_view_root` (`view.rs:220-235`):
```rust
    pub fn set_view_root(
        &self,
        db: &mut Db,
        view: ViewRoot<'_>,
    ) -> Result<ViewChange, ViewError> {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            let found = writer.views.iter().position(|(base, _)| base.as_path() == Some(view.base));
            let (base, mut index) = match found {
                Some(at) => writer.views.swap_remove(at),
                None => {
                    // alloc: the base a view is first set with, kept for the next call.
                    let base = VfsPath::from(view.base.to_path_buf());
                    let index = ViewIndex::new(&base, &writer.by_path);
                    (base, index)
                }
            };
            let plan = self.plan_view(&writer, &mut index, &view, frame);
            writer.views.push((base, index));
            Ok(self.write_view(&mut writer, db, &view, plan?, frame))
        })
    }
```
(`swap_remove` then `push` keeps the list's capacity: no allocation.)

`Plan` and `Write` (`view.rs:194-212`):
```rust
struct Plan<'f> {
    writes: Vec<Write, &'f Frame>,
    added: Vec<(FileId, SourceRootId, VfsPath), &'f Frame>,
    removed: Vec<(SourceRootId, FileId), &'f Frame>,
    next_file: u32,
}
```

`Files::plan_view` (`view.rs:237-322`) takes `frame: &'f Frame` and returns
`Result<Plan<'f>, ViewError>`:
```rust
        let call = index.begin();
        let mut fresh: hashbrown::HashSet<&str, FxBuildHasher, &Frame> =
            hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
        let mut plan = Plan {
            writes: Vec::new_in(frame),
            added: Vec::new_in(frame),
            removed: Vec::new_in(frame),
            next_file: writer.next_file,
        };
        // ... the loop as it is (`VfsPath::from(view.base.join(file.path))` for a new file is
        // the path its source root keeps: owned)
        let bytes = frame.alloc_slice_from_iter(plan.writes.iter().map(|it| view.files[it.at].file.bytes));
        let ids = frame.alloc_slice_fill_with(bytes.len(), |_| ContentId::default());
        ContentId::of_many_into(blaze_vfs::FILE_DOMAIN, bytes, ids);
        for (write, &bytes) in plan.writes.iter().zip(&*ids) {
            // ... as it is
        }
        let mut built: Vec<Cow<'_, str>, &Frame> = Vec::with_capacity_in(view.built.len(), frame);
        built.extend(view.built.iter().filter_map(|built| match view.base.starts_with(built) {
            true => Some(Cow::Borrowed("")),
            false => Some(rel_key(built.strip_prefix(view.base)?.as_str())),
        }));
        // ... `index.unseen(call)` loop as it is
```
(`use rustc_hash::FxBuildHasher;` beside the `FxHashMap` import.) `rel_key` makes a `String`
only on Windows for a path with `\`; on every other platform it borrows.

`Files::write_view` (`view.rs:324-420`) takes `plan: Plan<'_>` and `frame: &Frame`:
```rust
        let mut change = ViewChange::default();
        let texts = frame.alloc_slice_from_iter(
            plan.writes.iter().map(|it| std::str::from_utf8(view.files[it.at].file.bytes).ok()),
        );
        let mut bytes = Vec::new_in(frame);
        let mut ranges = Vec::new_in(frame);
        for (write, text) in plan.writes.iter().zip(&*texts) {
            // ... as it is
        }
        let mut contents = content::hash_ranges(FileText::DOMAIN, &bytes, &ranges, frame).iter().copied();
        let mut changed_roots = Vec::new_in(frame);
        for (write, &text) in plan.writes.iter().zip(&*texts) {
            // ... as it is
        }
        // ... `set_file_source_root_in` loop as it is
        let mut roots: Vec<SourceRootId, &Frame> = Vec::with_capacity_in(plan.added.len() + plan.removed.len(), frame);
        roots.extend(plan.added.iter().map(|it| it.1).chain(plan.removed.iter().map(|it| it.0)));
        sort_roots(&mut roots);
        roots.dedup();
        self.write_root_files(writer, db, &plan, &roots, &mut changed_roots, frame);
        self.write_root_contents(writer, db, &changed_roots, frame);
        writer.named.publish(db, frame);
        // ... the `removed` and `writes` loops as they are
        writer.next_file = plan.next_file;
        // alloc: the caller's copy of the roots that changed; none when nothing did.
        change.roots = roots.to_vec();
        for list in [&mut change.set, &mut change.added, &mut change.removed, &mut change.no_text] {
            sort_files(list);
            list.dedup();
        }
        change
```
`radix::sort_copy_by_key` makes its scratch per call for two or more items; the two sorts use
the worker's kept buffers through `sort_copy_by_key_in` (`crates/blaze-radix/src/lib.rs:196`):
```rust
thread_local! {
    static ROOT_SORT: RefCell<Scratch<SourceRootId>> = const { RefCell::new(Scratch::new()) };
    static FILE_SORT: RefCell<Scratch<FileId>> = const { RefCell::new(Scratch::new()) };
}

fn sort_roots(roots: &mut [SourceRootId]) {
    ROOT_SORT.with_borrow_mut(|scratch| radix::sort_copy_by_key_in(roots, |it| it.0, scratch.take()));
}

fn sort_files(files: &mut [FileId]) {
    FILE_SORT.with_borrow_mut(|scratch| radix::sort_copy_by_key_in(files, |it| it.index(), scratch.take()));
}
```
(`use std::cell::RefCell; use blaze_mem::{Frame, Tag, frame, scratch::Scratch};`.)

`Files::write_root_files` (`view.rs:425-495`) takes `plan: &Plan<'_>`,
`changed: &mut Vec<SourceRootId, &Frame>` and `frame: &Frame`:
```rust
        let keys = frame.alloc_slice_from_iter(plan.added.iter().map(|(_, _, path)| content::path_key(path)));
        let key_bytes = frame.alloc_slice_from_iter(keys.iter().map(|key| &**key));
        let path_hashes = frame.alloc_slice_fill_with(key_bytes.len(), |_| ContentId::default());
        ContentId::of_many_into(SourceRoot::PATH_DOMAIN, key_bytes, path_hashes);
        let mut keys = frame.alloc_slice_from_iter(
            keys.iter_mut().zip(&*path_hashes).map(|(key, hash)| Some((mem::take(key), RootContent::bucket(hash)))),
        );
        for &root in roots {
            let input = self.source_root(root);
            let old = input.source_root(&*db);
            let mut gone: hashbrown::HashSet<FileId, FxBuildHasher, &Frame> =
                hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
            gone.extend(plan.removed.iter().filter(|it| it.0 == root).map(|it| it.1));
            // ... the rest as it is (`FileSet`, `SourceRoot::new_*`, `Arc::new` are the new
            // root's owned input)
```
`keys` holds owned `Box<[u8]>`s in frame slices: each is moved out (`take`) into its
`RootContent`, and a key left in a slice (none: every added file goes in one root) would be
leaked, not freed, when the frame resets: the existing `expect("an added file is put in one
root")` is what upholds it. (`mem::take` on a `Box<[u8]>` leaves the empty box, which does not
allocate.)

### B3. `ContentId::of_many_into` (owner: lane `format`; message `mem-format`)

`crates/blaze-ids/src/lib.rs:67` `of_many` returns a fresh `Vec` and
`crates/blaze-ids/src/many.rs:27-93` makes two more per call (`order`, `cvs`): three
allocations each time base-db hashes a batch, which is every keystroke (`refresh` hashes twice)
and every view set that writes. Add, beside `of_many` (kept for its other callers):

```rust
    /// [`ContentId::of_many`] into `out` (`out.len() == inputs.len()`), with its working
    /// buffers the calling thread's: allocates nothing once they have grown.
    pub fn of_many_into(domain: &'static str, inputs: &[&[u8]], out: &mut [ContentId]) {
        many::of_many_into(context_key(domain), inputs, out)
    }
```
and in `many.rs`, `of_many` becomes `let mut out = vec![..]; of_many_into(key, inputs, &mut
out); out`, with `of_many_into` the current body writing `out[..]`, `order` lent by
`blaze_mem::scratch::with_u32s` (it is `Vec<u32>` already; `resize(n, 0)` in place of `vec![0u32;
n]`) and `cvs` (`Vec<[u32; 8]>`) a `thread_local!` `RefCell<blaze_mem::scratch::Scratch<[u32; 8]>>`.
Needs `blaze-mem.workspace = true` in `crates/blaze-ids/Cargo.toml`. Budget test, in blaze-ids'
tests: after one warm call of 100 inputs, `assert_at_most(0, "of_many_into, warm", ..)` on the
second, and `of_many_into` equals `of_many` input for input on random lengths 0 to 3000 bytes.

### B4. base-db's tables on reserved regions (`table.rs`)

They are read on every query (lock-free, `docs/v2/plan/design/concurrency.md`) and grow only
when the file set or the notes grow; today each growth is a `Box` from the global allocator
(a 32 KiB page per 4096 IDs, a `Log` chunk, a shard table level). Each moves to a blaze-mem
region tagged `Tag::Query`, with the same read cost or less and no change to what a reader
sees:

- `table::Column<T>` (`:33-86`): a salsa input per dense ID, read with two acquire loads
  through a page directory. After, one `blaze_mem::AppendVec<AtomicU64>` reserved for
  `MAX + 1` words (64 MiB of address space, committed as IDs are used):
  ```rust
  pub(crate) struct Column<T> {
      words: AppendVec<AtomicU64>,
      of: PhantomData<fn() -> T>,
  }

  impl<T: AsId + FromId + Copy> Column<T> {
      pub(crate) const MAX: u32 = span::EditionedFileId::MAX_FILE_ID;

      pub(crate) fn new() -> Column<T> {
          let words = AppendVec::new(Self::MAX as usize + 1, Tag::Query)
              .unwrap_or_else(|error| panic!("{error}"));
          Column { words, of: PhantomData }
      }

      #[inline]
      pub(crate) fn get(&self, raw: u32) -> Option<T> {
          match self.words.get(raw)?.load(Ordering::Acquire) {
              0 => None,
              bits => Some(T::from_id(Id::from_bits(bits))),
          }
      }

      /// Sets (or with `None` clears) the input of `raw`. Called only by the setters, with the
      /// files' writer lock held: the one writer `AppendVec` asks for. Panics naming it past
      /// [`Column::MAX`].
      pub(crate) fn set(&self, raw: u32, value: Option<T>) {
          assert!(raw <= Self::MAX, "ID {raw} is past {}, the largest file ID the front end takes", Self::MAX);
          let bits = value.map_or(0, |it| it.as_id().as_bits());
          if bits == 0 && raw >= self.words.len() {
              return;
          }
          while self.words.len() <= raw {
              self.words.append(AtomicU64::new(0));
          }
          self.words.as_slice()[raw as usize].store(bits, Ordering::Release);
      }

      pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, T)> + '_ {
          self.words.iter().enumerate().filter_map(|(raw, word)| match word.load(Ordering::Acquire) {
              0 => None,
              bits => Some((raw as u32, T::from_id(Id::from_bits(bits)))),
          })
      }
  }
  ```
  A read is one acquire load of the length, a bounds compare and the word's acquire load, with
  no directory. File IDs are dense from 0 (`Files::unused_file_id` hands out the lowest free),
  so the words appended below an ID are the ones a page would have held. Every `set` caller
  holds `Files::lock` (`set_text`, `write_text`, `set_file_source_root_in`,
  `set_source_root_with_durability`): check that each still does after the db lanes' edits,
  since `AppendVec` stops on a second writer at once.
- `table::Log<T>` (`:107-139`): exactly `blaze_mem::Column<T>` (any number of appenders, one
  `fetch_add` to claim, a ready bit to publish, `&T` stable). After:
  ```rust
  pub(crate) struct Log<T>(blaze_mem::Column<T>);

  impl<T> Log<T> {
      /// At most `max` values.
      pub(crate) fn new(max: usize) -> Log<T> {
          Log(blaze_mem::Column::new(max, Tag::Query).unwrap_or_else(|error| panic!("{error}")))
      }

      pub(crate) fn push(&self, value: T) -> u32 {
          self.0.push(value).unwrap_or_else(|error| panic!("{error}"))
      }

      #[inline]
      pub(crate) fn get(&self, raw: u32) -> Option<&T> {
          self.0.get(raw)
      }

      /// The values appended, in order; one still being appended is left out.
      pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
          self.0.iter().map(|(_, value)| value)
      }
  }
  ```
  Sizes: `Files::views` `Log::new(1 << 16)`; each `Table`'s values `1 << 22` (four million
  includes, env reads or named paths: past any build; `MemError` names `Tag::Query` past it).
  `FIRST`, `CHUNKS` and `chunk_of` go.
- `table::Table<T>` (`:150-295`): its values are the `Log` above (`Table::new(max)` passes
  `max` on); a shard's levels (`tables: [OnceLock<Box<[AtomicU32]>>; TABLES]`) become
  `[OnceLock<AppendVec<AtomicU32>>; TABLES]`, each level its own exact reservation, filled with
  its zeros when made (growth only, with the shard's lock held):
  ```rust
  fn table(shard: &Shard, level: usize) -> &[AtomicU32] {
      shard.tables[level]
          .get_or_init(|| {
              let places = TABLE << level;
              let table = AppendVec::new(places, Tag::Query).unwrap_or_else(|error| panic!("{error}"));
              for _ in 0..places {
                  table.append(AtomicU32::new(0));
              }
              table
          })
          .as_slice()
  }
  ```
  `Shard` itself (`Box::new(Shard { .. })`, once per shard on its first insert) stays a box:
  64 bytes of locks and level, made once.
- `new_page` and `PAGE`, `PAGE_BITS` go.

`Files::default` (`files.rs:135-151`) passes the sizes:
`included: Table::new(1 << 22)` and the same for the other five tables, `views: Log::new(1 << 16)`;
`named::Named::default` (`named.rs:96-107`) `Arc::new(Table::new(1 << 22))`.

Address space per database: three columns at 64 MiB, seven tables at about 160 MiB each
(`(u64, T)` of at most 40 bytes times four million) plus their ready bits: about 1.3 GiB
reserved, nothing committed until used. Tests make many databases at once; at 47 bits of
address space that is room for 100 000 of them.

The test `a_table_finds_what_a_hash_map_does` (`tests.rs:422`) holds the new `Table` to the
same answers; add one for `Column` (set and clear random IDs below 100 000 with a `Vec` as the
reference; `get` and `iter` agree) and one for `Log` (four threads push 10 000 each; every
index is read back to its value, `iter` has 40 000 in index order).

### B5. `run_all`: each task in a frame of its own, tagged by its caller (`db.rs`)

A batch task's body runs outside salsa's executor: `hir/src/check.rs:118` checks modules in it,
and the collector's tasks (`nameres/collector.rs:146`, `:898`) ask queries from it. So each
task gets its own `frame::task`, which also keeps a task the pump runs on a waiting thread out
of the waiting caller's frame (the pump nests tasks strictly, `blaze_graph::pump`, so frames
stay a stack). The caller names the subsystem:

`Db::run_all` (`db.rs:278-296`):
```rust
    pub fn run_all<F: Fn(&Db, usize) + Sync>(&self, tag: Tag, count: usize, task: F) {
        if count == 0 {
            return;
        }
        let run = |db: &Db, at: usize| frame::task(tag, |_| task(db, at));
        match self.run {
            Run::Pool => run_on_pool(self, blaze_graph::exec::shared(), count, &run),
            Run::Here => match RUN_ORDER.get() {
                RunOrder::InOrder => (0..count).for_each(|at| run(self, at)),
                RunOrder::Reversed => (0..count).rev().for_each(|at| run(self, at)),
                RunOrder::Strided(stride) => {
                    let stride = stride.max(1);
                    (0..stride)
                        .flat_map(|first| (first..count).step_by(stride))
                        .for_each(|at| run(self, at));
                }
                RunOrder::LeftToReaders => {}
            },
        }
    }
```

`run_on_pool` (`db.rs:373-414`): `round` and `again` are the worker's `u32` buffers, so a batch
allocates nothing of its own (a job's box is blaze-graph's, `mem-core-plane`'s request on
`Job::erase`):
```rust
pub fn run_on_pool<F: Fn(&Db, usize) + Sync>(
    db: &Db,
    pool: &blaze_graph::pool::Pool,
    count: usize,
    task: &F,
) {
    let count = u32::try_from(count).expect("a batch of more than `u32::MAX` tasks");
    let near = blaze_graph::pool::group();
    let panicked = Mutex::new(None);
    let again = Mutex::new(scratch::lend_u32s());
    let mut round = scratch::lend_u32s();
    round.extend(0..count);
    while !round.is_empty() {
        pool.scope(|scope| {
            for &at in round.iter() {
                let (panicked, again) = (&panicked, &again);
                scope.submit(
                    near,
                    move || task(db, at as usize),
                    move |ran| {
                        // ... as it is, pushing `at` (a `u32`)
                    },
                );
            }
        });
        round.clear();
        std::mem::swap(&mut *round, &mut **again.lock().unwrap_or_else(|it| it.into_inner()));
        if panicked.lock().unwrap_or_else(|it| it.into_inner()).is_some() {
            break;
        }
    }
    if let Some(panic) = panicked.into_inner().unwrap_or_else(|it| it.into_inner()) {
        panic::resume_unwind(panic);
    }
}
```
(`use blaze_mem::{Tag, frame, scratch};` in `db.rs`; `Lent<Vec<u32>>` is `Send`, so the
`Mutex` is `Sync` as before.) `run_on_pool`'s own callers pass a task that is already framed:
`Db::run_all` above, and `crates/ide-db/src/lib.rs:319` (lane db's, if it is still live: it
wraps its task the same way, `frame::task(tag, ..)`, with the tag its `run_all` is given).

Callers pass their tag:
- `crates/hir-def/src/nameres/collector.rs:146` (`expand_all`): `db.run_all(Tag::Expand,
  calls.len(), &|db, at| ..)` (owner `db-expand-names-next1`).
- `crates/hir-def/src/nameres/collector.rs:898` (the preludes' def maps):
  `self.db.run_all(Tag::Nameres, prelude.len(), &|db, at| ..)` (same owner).
- `crates/hir/src/check.rs:118` (`check_modules`): `db.run_all(Tag::Infer, starts.len(),
  &|task_db, task| ..)`: a check is the body checks hir-ty runs (owner `db-next2`).
- `crates/base-db/src/db/tests.rs:44`, `:53`: `db.run_all(Tag::Query, ..)`; `tests.rs:698`,
  `:713` call `run_on_pool` with a bare task, which stays right (it is the pool's test).

### B6. `transitive_deps` and `transitive_rev_deps` memoized (`input.rs:868-922`)

Both rebuild everything per call (`vec![self]`, an `FxHashSet`, a `Vec`; for reverse deps an
`FxHashMap<Crate, Vec<Crate>>` of the whole graph) and are asked from queries
(`hir-def/src/deps/key.rs:42`, `hir-ty/src/method_resolution.rs:1170`,
`blaze-items/src/index.rs:506`) and from IDE requests (`hir/src/lib.rs:309`, `:314`,
`:4104`). As tracked queries over the crate input their hit is a memo read; a miss keeps its
temporaries in the frame and its result at its exact size:

```rust
impl Crate {
    /// The crate and every crate it depends on, directly or not, in the order a walk from it
    /// first reaches them.
    ///
    /// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
    pub fn transitive_deps(self, db: &Db) -> &[Crate] {
        transitive_deps(db, self)
    }

    /// The crate and every crate that depends on it, directly or not, in the order a walk from
    /// it first reaches them.
    ///
    /// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
    pub fn transitive_rev_deps(self, db: &Db) -> &[Crate] {
        transitive_rev_deps(db, self)
    }
}

#[salsa::tracked(returns(deref))]
fn transitive_deps(db: &Db, krate: Crate) -> Box<[Crate]> {
    blaze_mem::frame::current(|frame| {
        let mut worklist = Vec::new_in(frame);
        worklist.push(krate);
        let mut seen: hashbrown::HashSet<Crate, FxBuildHasher, &Frame> =
            hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
        let mut deps = Vec::new_in(frame);
        while let Some(krate) = worklist.pop() {
            if !seen.insert(krate) {
                continue;
            }
            deps.push(krate);
            worklist.extend(krate.data(db).dependencies.iter().map(|dep| dep.crate_id));
        }
        Box::from(&deps[..])
    })
}

#[salsa::tracked(returns(deref))]
fn transitive_rev_deps(db: &Db, krate: Crate) -> Box<[Crate]> {
    blaze_mem::frame::current(|frame| {
        let all = all_crates(db);
        // The reverse edges as CSR: for each crate (by its place in `all`), the crates that
        // depend on it.
        let mut place: hashbrown::HashMap<Crate, u32, FxBuildHasher, &Frame> =
            hashbrown::HashMap::with_capacity_and_hasher_in(all.len(), FxBuildHasher, frame);
        place.extend(all.iter().enumerate().map(|(at, &krate)| (krate, at as u32)));
        let mut starts = frame.alloc_slice_fill_with(all.len() + 1, |_| 0u32);
        for &from in all.iter() {
            for dep in &from.data(db).dependencies {
                starts[place[&dep.crate_id] as usize + 1] += 1;
            }
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        let mut next = frame.alloc_slice_copy(&starts[..all.len()]);
        let mut edges = frame.alloc_slice_fill_with(starts[all.len()] as usize, |_| krate);
        for &from in all.iter() {
            for dep in &from.data(db).dependencies {
                let to = place[&dep.crate_id] as usize;
                edges[next[to] as usize] = from;
                next[to] += 1;
            }
        }
        let mut seen = frame.alloc_slice_fill_with(all.len(), |_| false);
        let mut order = Vec::new_in(frame);
        let mut worklist = Vec::new_in(frame);
        worklist.push(krate);
        seen[place[&krate] as usize] = true;
        while let Some(krate) = worklist.pop() {
            order.push(krate);
            let at = place[&krate] as usize;
            for &rev_dep in &edges[starts[at] as usize..starts[at + 1] as usize] {
                let slot = &mut seen[place[&rev_dep] as usize];
                if !*slot {
                    *slot = true;
                    worklist.push(rev_dep);
                }
            }
        }
        Box::from(&order[..])
    })
}
```
(`use blaze_mem::Frame; use rustc_hash::FxBuildHasher;` in `input.rs`; `let (mut starts, mut
next, mut edges, mut seen)` are `&mut [_]`, so drop the `mut` on the bindings.)

Order: `transitive_deps` keeps its order (the same walk). `transitive_rev_deps` was the
iteration order of an `FxHashSet` (std's hash table layout), and is now the order of a walk
from the crate: deterministic and independent of any table's layout. Of its readers,
`blaze-items`' `dependents` feeds `impls_of`, which sorts by key; `hir/src/lib.rs:4104` extends
a list of impls in it (IDE output order, now stable across std versions); the others count it
or test membership.

Caller changes, `&[Crate]` in place of `Vec`/`Box`:
- `crates/hir-ty/src/method_resolution.rs:1170` (lane items):
  `krate.transitive_deps(db).iter().copied().filter(|krate| krate.data(db).origin.is_lang()).collect()`.
- `crates/blaze-items/src/index.rs:504-507` (lane items): `dependents` duplicates the memo;
  delete it and call `krate.transitive_rev_deps(db)` where it was called.
- `crates/hir/src/lib.rs:309` (`db-next2`): `self.id.transitive_rev_deps(db).iter().map(|&id| Crate { id })`;
  `:314`: `.transitive_deps(db).iter().filter_map(|&krate| hir_def::crate_notable_traits(db, krate))`;
  `:4104`: `for &krate in module.krate(db).transitive_rev_deps(db) {`.
- `crates/blaze-engine/src/frozen.rs:256` (lane codegen): `for &dep in krate.transitive_deps(db) {`.
- `crates/hir-def/src/deps/key.rs:42` and `crates/blaze-engine/src/outline.rs:530` read the same.

### B7. `source_root_crates` at its exact size (`lib.rs:497-513`)

```rust
    #[salsa::tracked(returns(deref))]
    pub fn source_root_crates<'db>(
        db: &'db Db,
        id: InternedSourceRootId<'db>,
    ) -> Box<[Crate]> {
        let crates = AllCrates::get(db).crates(db);
        let id = id.id(db);
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            found.extend(crates.iter().copied().filter(|&krate| {
                let root_file = krate.data(db).root_file_id;
                db.file_source_root(root_file).source_root_id(db) == id
            }));
            Box::from(&found[..])
        })
    }
```
`EditionedFileId::parse_errors` (`editioned_file_id.rs:43-50`) turns `errors()`'s list into a
box with `.into()`, which reallocates when the list has spare capacity; that list is
`syntax::Parse::errors`' (lane format's), so the exact size is theirs to give: noted to
`mem-format-tree`, nothing to change here.

### B8. The executor path (no change; what was checked)

base-db's query functions are `EditionedFileId::parse`, `is_library`, `parse_errors`
(`editioned_file_id.rs:26-50`), `line_index` (`lib.rs:241`), `source_root_crates` (`lib.rs:499`)
and, with B6, `transitive_deps` and `transitive_rev_deps`: all `#[salsa::tracked]` over `&Db`.
A call is the macro's `fn_ingredient(db).fetch(db, key)`
(`vendor/salsa/src/function/fetch.rs`): a hit returns the memo; a miss goes `fetch_cold`
(`fetch.rs:132`) → `self.execute` (`fetch.rs:197`; also `maybe_changed_after.rs:266`, a deep
verify that has to recompute) → `execute.rs:39` `execute` → `execute_maybe_iterate` (`:122`,
one call per fixpoint iteration) → `execute_query` (`:328`) → `C::execute(db, input)` (`:340`),
the one place a query function's body runs, which `mem-db-salsa` wraps in
`frame::task(C::TAG, ..)`. Nothing in base-db calls a query body any other way: no inner
function is called directly (`source_root_crates`' inner tracked function is reached only
through the outer wrapper), and `Reparse::parse`/`reparse::whole` run only inside `parse`
(and `assert_whole` in debug builds and tests, inside it). The one place base-db runs code
outside the executor is `run_all`'s tasks, which B5 frames; the setters run outside any query
and B1, B2 frame them.

### Budget tests (in `crates/base-db/src/tests.rs`, which installs `blaze_alloc_count::Counting`)

```rust
/// Sets each of `texts` in turn on `file`, `rounds` times: what a warm editor does.
fn type_into(db: &mut Db, file: FileId, texts: &[&str], rounds: usize) {
    for _ in 0..rounds {
        for text in texts {
            db.set_file_text(file, text);
        }
    }
}

#[test]
fn a_keystroke_allocates_only_its_text_and_slot() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    type_into(&mut db, files[1], &["pub fn f() {}", "pub fn fg() {}"], 4);
    let content = db.source_root(SourceRootId(0)).content(&db);
    blaze_alloc_count::assert_at_most(2, "a keystroke: the text's Arc and its Reparse slot", || {
        db.set_file_text(files[1], "pub fn f() {}")
    });
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), content);
    assert_eq!(db.file_text(files[1]).content(&db), FileText::content_of("pub fn f() {}", None));
}

#[test]
fn setting_an_unchanged_view_allocates_nothing() {
    let mut db = Db::for_tests();
    let (base, partition) = checkout(&mut db);
    let built = [base.join("target")];
    let files: Vec<ViewFile<'_>> =
        VIEW.iter().map(|&(path, bytes)| ViewFile { path, file: vfs_file(bytes) }).collect();
    let view = ViewRoot { base: &base, built: &built, partition: &partition, other: SourceRootId(1), files: &files };
    db.set_view_root(view).unwrap();
    db.set_view_root(view).unwrap();
    let revision = revision(&db);
    let change = blaze_alloc_count::assert_at_most(0, "a view set again as it was", || {
        db.set_view_root(view).unwrap()
    });
    assert_eq!(change, ViewChange::default());
    assert_eq!(revision(&db), revision);
}

#[test]
fn a_view_edit_allocates_only_its_text_slot_and_change() {
    let mut db = Db::for_tests();
    let (base, partition) = checkout(&mut db);
    let edited: &[(&str, &[u8])] =
        &[("src/lib.rs", b"mod a;"), ("src/a.rs", b"fn g() {}"), ("README.md", b"hello")];
    for _ in 0..4 {
        set_view(&mut db, &base, &partition, VIEW).unwrap();
        set_view(&mut db, &base, &partition, edited).unwrap();
    }
    let files: Vec<ViewFile<'_>> =
        VIEW.iter().map(|&(path, bytes)| ViewFile { path, file: vfs_file(bytes) }).collect();
    let view = ViewRoot { base: &base, built: &[], partition: &partition, other: SourceRootId(1), files: &files };
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    let change = blaze_alloc_count::assert_at_most(
        3,
        "a view with one file edited: the text's Arc, its Reparse slot and `ViewChange::set`",
        || db.set_view_root(view).unwrap(),
    );
    assert_eq!(change, ViewChange { set: vec![a], ..ViewChange::default() });
}

#[test]
fn a_batch_run_here_allocates_nothing_and_frames_each_task() {
    let db = Db::for_tests();
    let seen = std::sync::atomic::AtomicUsize::new(0);
    let task = |_: &Db, _: usize| {
        let tag = blaze_mem::frame::current(|frame| frame.tag());
        assert_eq!(tag, blaze_mem::Tag::Expand);
        seen.fetch_add(1, Ordering::Relaxed);
    };
    db.run_all(blaze_mem::Tag::Expand, 8, task);
    blaze_alloc_count::assert_at_most(0, "a batch of 8 run on the calling thread", || {
        db.run_all(blaze_mem::Tag::Expand, 8, task)
    });
    assert_eq!(seen.load(Ordering::Relaxed), 16);
}

#[test]
fn crate_deps_are_memo_hits() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], cfg(&[]), Env::default());
    let c = add(&mut graph, files[2], cfg(&[]), Env::default());
    depend(&mut graph, a, c, "c");
    let ids = graph.set_in_db(&mut db);
    let (a, c) = (ids[&a], ids[&c]);
    assert_eq!(a.transitive_deps(&db), &[a, c]);
    assert_eq!(c.transitive_rev_deps(&db), &[c, a]);
    blaze_alloc_count::assert_at_most(0, "transitive deps, warm", || a.transitive_deps(&db).len());
    blaze_alloc_count::assert_at_most(0, "transitive reverse deps, warm", || c.transitive_rev_deps(&db).len());
}
```
plus, beside `a_table_finds_what_a_hash_map_does`, the `Column` and `Log` tests of B4. The
existing `a_memo_hit_allocates_nothing` (`db/tests.rs:79`) holds the parse-level memo hit.

## dedup: LEB128 and seeded RNGs onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs`; its API is listed in `v2/requests/mem-backend-link.md` under
the same heading. The RNG is `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`).
`hir-def` already depends on `blaze-format`. In `crates/hir-def/src/nameres/frozen.rs`:

- `Enc::u32` becomes `pub fn u32(&mut self, it: u32) { blaze_format::leb::write_u64(&mut self.out, it.into()); }`.
- `Dec::u32` becomes
  ```rust
      pub fn u32(&mut self) -> R<u32> {
          blaze_format::leb::read_u32(self.bytes, &mut self.at).map_err(|error| error.to_string())
      }
  ```
  (it now also refuses a 5th byte carrying bits past 32, which the loop dropped).
