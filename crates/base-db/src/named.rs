//! Every path any source root has named, across source roots: where
//! [`SourceDatabase::resolve_path`] looks when the anchor's own source root lacks a path. rustc
//! reads a `#[path]` module or an `include!` anywhere on disk, while v2 gives each package its
//! own source root, so a member's `#[path = "../shared/m.rs"]` names a file of another root.
//!
//! A lookup is tracked as reads of inputs, never a quiet miss and never a look at the disk:
//! - the [`PathBucket`] of the path, whose content ID is of the set of paths named in it, so a
//!   lookup that found nothing runs again once a source root names the path;
//! - on a hit, the file's [`FileSourceRootInput`](crate::FileSourceRootInput) and its source
//!   root's file set, so an edit of that root (the file taken out, or given another path)
//!   reruns the reader; the file's text is read by the reader itself.
//!
//! The table behind it is [`Table`]: a lookup is acquire loads, with no lock and nothing
//! written (docs/v2/plan/design/concurrency.md). Only the setters of [`Files`](crate::Files)
//! write it, with the database's `&mut`, so no query runs while it changes.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_ids::{ContentId, Hasher};
use salsa::{Durability, Setter};
use vfs::{FileId, VfsPath};

use crate::{
    InputId, SourceDatabase, SourceRoot, content,
    table::{Table, hash_of},
};

pub(crate) const BUCKETS: usize = 64;

/// A bucket's content ID is of the paths named in it: how many (8 bytes, little-endian), then
/// the lane-wise wrapping sum of their [`SourceRoot::PATH_DOMAIN`] IDs as four little-endian
/// `u64`s. A function of the set alone, in any order it was named, kept in `O(1)` a path.
pub const NAMED_PATHS_DOMAIN: &str = "blaze v2 named paths bucket 1";

/// The index's handles, set once when the first source root is set and never again: what it
/// holds is in its buckets' content IDs.
#[salsa::input(singleton, debug)]
pub(crate) struct NamedPaths {
    #[returns(ref)]
    table: PathTable,
    #[returns(ref)]
    buckets: Box<[PathBucket]>,
}

/// The paths of one of [`BUCKETS`], by their [`hash_of`]: see [`NAMED_PATHS_DOMAIN`].
#[salsa::input(debug)]
pub(crate) struct PathBucket {
    #[returns(copy)]
    content: InputId,
}

#[derive(Clone)]
pub(crate) struct PathTable(Arc<Table<NamedPath>>);

impl fmt::Debug for PathTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PathTable").finish_non_exhaustive()
    }
}

struct NamedPath {
    path: VfsPath,
    /// The file the path names, [`FileId::index`]: a path given another file keeps its entry.
    file: AtomicU32,
}

fn bucket_of(hash: u64) -> usize {
    // `Table` picks a shard by the top bits and a place by the low ones.
    (hash >> 52) as usize % BUCKETS
}

/// The setters' side of the index, in [`Writer`](crate::files::Writer).
pub(crate) struct Named {
    table: Arc<Table<NamedPath>>,
    input: Option<NamedPaths>,
    counts: [u64; BUCKETS],
    sums: [[u64; 4]; BUCKETS],
    /// Paths named since the last [`Named::publish`], with their [`hash_of`]: hashed for the
    /// buckets in one batch there.
    pending: Vec<(u64, VfsPath)>,
}

impl Default for Named {
    fn default() -> Named {
        Named {
            table: Arc::new(Table::new()),
            input: None,
            counts: [0; BUCKETS],
            sums: [[0; 4]; BUCKETS],
            pending: Vec::new(),
        }
    }
}

impl Named {
    /// A source root names `file` at `path`: a new path, or one given another file.
    pub(crate) fn note(&mut self, path: &VfsPath, file: FileId) {
        let hash = hash_of(path);
        let make = || NamedPath { path: path.clone(), file: AtomicU32::new(file.index()) };
        let (at, inserted) = self.table.insert(hash, |it| it.path == *path, make);
        if inserted {
            self.pending.push((hash, path.clone()));
        } else {
            self.table.get(at).file.store(file.index(), Ordering::Release);
        }
    }

    fn content(&self, bucket: usize) -> ContentId {
        let mut hasher = Hasher::new(NAMED_PATHS_DOMAIN);
        hasher.update(&self.counts[bucket].to_le_bytes());
        for lane in self.sums[bucket] {
            hasher.update(&lane.to_le_bytes());
        }
        hasher.finish()
    }

    /// Writes the content IDs of the buckets that gained paths, making the inputs the first
    /// time. Called by every setter that sets a source root's files, after it has.
    pub(crate) fn publish(&mut self, db: &mut dyn SourceDatabase) {
        if self.input.is_some() && self.pending.is_empty() {
            return;
        }
        let keys: Vec<Vec<u8>> = self.pending.iter().map(|(_, path)| content::path_key(path)).collect();
        let slices: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        let ids = match slices.is_empty() {
            true => Vec::new(),
            false => ContentId::of_many(SourceRoot::PATH_DOMAIN, &slices),
        };
        let mut dirty = 0u64;
        for ((hash, _), id) in self.pending.iter().zip(&ids) {
            let bucket = bucket_of(*hash);
            self.counts[bucket] += 1;
            for (lane, sum) in self.sums[bucket].iter_mut().enumerate() {
                let word: [u8; 8] = id.0[lane * 8..lane * 8 + 8].try_into().expect("8 bytes");
                *sum = sum.wrapping_add(u64::from_le_bytes(word));
            }
            dirty |= 1 << bucket;
        }
        self.pending.clear();
        let Some(input) = self.input else {
            let buckets: Box<[PathBucket]> = (0..BUCKETS)
                .map(|bucket| {
                    PathBucket::builder(InputId(self.content(bucket)))
                        .durability(Durability::MEDIUM)
                        .new(&*db)
                })
                .collect();
            let table = PathTable(self.table.clone());
            self.input =
                Some(NamedPaths::builder(table, buckets).durability(Durability::HIGH).new(&*db));
            return;
        };
        while dirty != 0 {
            let bucket = dirty.trailing_zeros() as usize;
            dirty &= dirty - 1;
            let content = InputId(self.content(bucket));
            let input = input.buckets(&*db)[bucket];
            if input.content(&*db) != content {
                input.set_content(db).with_durability(Durability::MEDIUM).to(content);
            }
        }
    }
}

/// The file a source root names at `path`, found through the index: see the module.
pub(crate) fn resolve<Db: ?Sized + SourceDatabase>(db: &Db, path: &VfsPath) -> Option<FileId> {
    let named = NamedPaths::try_get(db)?;
    let hash = hash_of(path);
    // The read a miss reruns on.
    let _ = named.buckets(db)[bucket_of(hash)].content(db);
    let table = &named.table(db).0;
    let at = table.find(hash, |it| it.path == *path)?;
    let file = FileId::from_raw(table.get(at).file.load(Ordering::Acquire));
    let root = db.file_source_root(file).source_root_id(db);
    let named_there = db.source_root(root).source_root_ref(db).file_for_path(path) == Some(&file);
    named_there.then_some(file)
}
