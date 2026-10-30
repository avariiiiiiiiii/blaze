//! A store path as blaze's store keeps it: each file's bytes in pieces keyed by their own hash,
//! and one manifest per store path, keyed by the path and its NarHash, that lists its files and
//! the store paths it refers to. Both are lasting keys (`blaze_store::Store::insert_lasting`):
//! what they hold is named by the key alone, so every build of blaze finds them, and they
//! travel through the remote cache as any other entry does.

use std::{
    io::{self, Read, Write as _},
    os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, bail};
use blaze_format::cursor::{Put as _, Reader};
use blaze_store::Store;
use sha2::Digest as _;

use super::{
    StorePath,
    nar::{self, Sink},
    narinfo::{Sha256, base_name},
};

/// What the keys of the two kinds of entry start with. A change to how either is written is
/// another number here.
const MANIFEST: &[u8] = b"nix store path 1";
const PIECE: &[u8] = b"nix file piece 1";

/// A file is kept in pieces of this many bytes, the last one shorter: the most the store packs
/// compressed in one value.
pub const PIECE_MAX: usize = blaze_store::BLOCK_MAX;

/// How many bytes of pieces are read from the store at once when a path is laid out.
const BATCH: u64 = 64 << 20;

fn key(fields: &[&[u8]]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    hasher.finalize().into()
}

pub fn manifest_key(path: &StorePath) -> [u8; 32] {
    key(&[MANIFEST, path.path.as_bytes(), &path.nar_hash.0])
}

fn piece_key(bytes: &[u8]) -> [u8; 32] {
    key(&[PIECE, bytes])
}

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    File { executable: bool, size: u64, pieces: Vec<[u8; 32]> },
    Symlink { target: Vec<u8> },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// From the root, by names joined with `/`: empty for the root.
    pub path: Vec<u8>,
    pub kind: Kind,
}

/// What the store keeps of a store path besides its files' bytes.
#[derive(Debug, PartialEq, Eq)]
pub struct Manifest {
    pub of: StorePath,
    /// The store paths it refers to, itself too where it does.
    pub references: Vec<StorePath>,
    /// In the order of its NAR: a directory before what is in it.
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new(); // alloc: the value kept, once per store path
        let path = |out: &mut Vec<u8>, path: &StorePath| {
            out.put_bytes_u64(path.path.as_bytes());
            out.put_bytes(&path.nar_hash.0);
            out.put_u64(path.nar_size);
        };
        out.put_bytes_u64(MANIFEST);
        path(&mut out, &self.of);
        out.put_u64(self.references.len() as u64);
        for reference in &self.references {
            path(&mut out, reference);
        }
        out.put_u64(self.entries.len() as u64);
        for entry in &self.entries {
            out.put_bytes_u64(&entry.path);
            match &entry.kind {
                Kind::Directory => out.put_u8(0),
                Kind::File { executable, size, pieces } => {
                    out.put_u8(if *executable { 2 } else { 1 });
                    out.put_u64(*size);
                    out.put_u64(pieces.len() as u64);
                    pieces.iter().for_each(|it| out.put_bytes(it));
                }
                Kind::Symlink { target } => {
                    out.put_u8(3);
                    out.put_bytes_u64(target);
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Manifest> {
        let mut at = Reader::new(bytes);
        if at.bytes_u64().ok()? != MANIFEST {
            return None;
        }
        let of = store_path(&mut at)?;
        // A store path is at least its length, hash and size; an entry its length and kind.
        let references = (0..at.count_u64(48).ok()?).map(|_| store_path(&mut at)).collect::<Option<_>>()?;
        let count = at.count_u64(9).ok()?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            // alloc: the manifest owns its entries, read once per store path laid out
            let path = at.bytes_u64().ok()?.to_vec();
            let kind = match at.u8().ok()? {
                0 => Kind::Directory,
                kind @ (1 | 2) => {
                    let size = at.u64().ok()?;
                    let count = at.count_u64(32).ok()?;
                    let pieces = (0..count).map(|_| at.array::<32>().ok().copied());
                    let pieces = pieces.collect::<Option<Vec<[u8; 32]>>>()?;
                    Kind::File { executable: kind == 2, size, pieces }
                }
                3 => Kind::Symlink { target: at.bytes_u64().ok()?.to_vec() }, // alloc: as `path`
                _ => return None,
            };
            entries.push(Entry { path, kind });
        }
        at.is_done().then_some(Manifest { of, references, entries })
    }
}

fn store_path(at: &mut Reader<'_>) -> Option<StorePath> {
    let path = at.str_u64().ok()?.to_owned(); // alloc: the manifest owns its paths
    let nar_hash = Sha256(*at.array().ok()?);
    Some(StorePath { path, nar_hash, nar_size: at.u64().ok()? })
}

/// Puts what a NAR holds into the store as it is read: each file's pieces as they come, and
/// the list of its entries for the manifest, which is added once the NAR is known to be the one
/// signed ([`Keeping::finish`]).
pub struct Keeping<'a> {
    store: &'a Store,
    entries: Vec<Entry>,
    /// Every key added, to see that the store kept them all.
    pub added: Vec<[u8; 32]>,
}

impl<'a> Keeping<'a> {
    pub fn new(store: &'a Store) -> Keeping<'a> {
        Keeping { store, entries: Vec::new(), added: Vec::new() }
    }

    /// Adds the manifest of `of`, whose NAR this read, and which refers to `references`.
    pub fn finish(mut self, of: StorePath, references: Vec<StorePath>) -> Vec<[u8; 32]> {
        let key = manifest_key(&of);
        let manifest = Manifest { of, references, entries: self.entries };
        self.store.insert_lasting(&key, &manifest.encode());
        self.added.push(key);
        self.added
    }
}

impl Sink for Keeping<'_> {
    fn directory(&mut self, path: &[u8]) -> anyhow::Result<()> {
        self.entries.push(Entry { path: path.to_vec(), kind: Kind::Directory });
        Ok(())
    }

    fn file(
        &mut self,
        path: &[u8],
        executable: bool,
        size: u64,
        contents: &mut impl Read,
    ) -> anyhow::Result<()> {
        let mut pieces = Vec::with_capacity(size.div_ceil(PIECE_MAX as u64) as usize);
        let mut piece = vec![0; PIECE_MAX];
        let mut left = size;
        while left != 0 {
            let len = left.min(PIECE_MAX as u64) as usize;
            contents.read_exact(&mut piece[..len]).context("it ends in the middle of a file")?;
            let key = piece_key(&piece[..len]);
            self.store.insert_lasting(&key, &piece[..len]);
            self.added.push(key);
            pieces.push(key);
            left -= len as u64;
        }
        self.entries
            .push(Entry { path: path.to_vec(), kind: Kind::File { executable, size, pieces } });
        Ok(())
    }

    fn symlink(&mut self, path: &[u8], target: &[u8]) -> anyhow::Result<()> {
        let kind = Kind::Symlink { target: target.to_vec() };
        self.entries.push(Entry { path: path.to_vec(), kind });
        Ok(())
    }
}

/// A store path blaze's store has no copy of.
#[derive(Debug)]
pub struct Missing {
    pub path: String,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "blaze's store has no copy of {}, and no remote brought one. CI fills blaze's remote \
             cache with it (`blaze-engine closure fetch`, then `blaze-engine cache push REMOTE {}`), and \
             BLAZE_REMOTE names the remote it is taken from; blaze never takes it from \
             cache.nixos.org itself",
            self.path,
            super::PROJECT
        )
    }
}

impl std::error::Error for Missing {}

/// The manifest of `path`, from the store or through it from its remote.
pub fn manifest(store: &Store, path: &StorePath) -> anyhow::Result<Manifest> {
    let bytes = store
        .get_lasting(&manifest_key(path))
        .ok_or_else(|| Missing { path: path.path.clone() })?;
    let manifest = Manifest::decode(&bytes)
        .with_context(|| format!("blaze's store has a manifest of {} it cannot read", path.path))?;
    if manifest.of != *path {
        bail!(
            "blaze's store has the manifest of {} under the key of {}",
            manifest.of.path,
            path.path
        );
    }
    Ok(manifest)
}

/// Every store path `roots` refer to, themselves too, each once and sorted, from the store.
pub fn closure_of(store: &Store, roots: &[StorePath]) -> anyhow::Result<Vec<StorePath>> {
    let mut seen = std::collections::BTreeMap::new();
    let mut todo: Vec<StorePath> = roots.to_vec();
    while let Some(path) = todo.pop() {
        if seen.contains_key(&path.path) {
            continue;
        }
        let manifest = manifest(store, &path)?;
        todo.extend(manifest.references.into_iter().filter(|it| !seen.contains_key(&it.path)));
        seen.insert(path.path.clone(), path);
    }
    Ok(seen.into_values().collect())
}

/// Lays out `path` from the store as `<into>/<hash>-<name>`, unless it is there, and returns
/// where. It is written beside that, checked to be the NAR that was signed, made read-only and
/// only then moved into place, so what is there is whole. Processes that ask at once lay it
/// out once.
pub fn materialize(store: &Store, path: &StorePath, into: &Path) -> anyhow::Result<PathBuf> {
    let base = base_name(&path.path)?;
    let dir = into.join(base);
    if dir.symlink_metadata().is_ok() {
        return Ok(dir);
    }
    std::fs::create_dir_all(into)?;
    let lock = std::fs::File::create(into.join(format!(".{base}.lock")))?;
    lock.lock()?;
    if dir.symlink_metadata().is_ok() {
        return Ok(dir);
    }
    let manifest = manifest(store, path)?;
    let staging = into.join(format!(".{base}.unpacking"));
    remove(&staging)?;
    let laid_out = lay_out(store, &manifest, &staging).and_then(|()| {
        let (nar_hash, nar_size) = nar::hash_of(&staging)?;
        if (nar_hash, nar_size) != (path.nar_hash, path.nar_size) {
            bail!(
                "the copy of {} in blaze's store is not the one cache.nixos.org signed: laid out, \
                 its NAR is {nar_size} bytes of {nar_hash}, not {} of {}. Nothing was put in \
                 place; the store's entry of it is damaged",
                path.path,
                path.nar_size,
                path.nar_hash
            );
        }
        Ok(())
    });
    if let Err(e) = laid_out {
        _ = remove(&staging);
        return Err(e);
    }
    std::fs::rename(&staging, &dir)?;
    if dir.symlink_metadata()?.is_dir() {
        seal(&dir)?;
    }
    _ = std::fs::remove_file(into.join(format!(".{base}.lock")));
    Ok(dir)
}

/// Writes the entries of `manifest` under `root`, read-only and dated as nix dates its store
/// (one second after 1970), the pieces of many files read from the store at once.
fn lay_out(store: &Store, manifest: &Manifest, root: &Path) -> anyhow::Result<()> {
    let at = |path: &[u8]| match path {
        [] => root.to_owned(),
        path => root.join(std::ffi::OsStr::from_bytes(path)),
    };
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let mut directories = Vec::new();
    let mut batch: Vec<&Entry> = Vec::new();
    let mut batched = 0;
    let write_batch = |batch: &mut Vec<&Entry>| -> anyhow::Result<()> {
        let keys: Vec<[u8; 32]> = batch
            .iter()
            .flat_map(|it| match &it.kind {
                Kind::File { pieces, .. } => pieces.as_slice(),
                _ => &[],
            })
            .copied()
            .collect();
        let mut values = store.get_many_lasting(&keys, threads).into_iter();
        for entry in batch.drain(..) {
            let Kind::File { executable, size, pieces } = &entry.kind else { continue };
            let file = at(&entry.path);
            let mut out = std::fs::File::create(&file)?;
            let mut written = 0;
            for _ in pieces {
                let piece = values.next().flatten().ok_or_else(|| Missing {
                    path: format!(
                        "{}/{} (a piece of it)",
                        manifest.of.path,
                        entry.path.escape_ascii()
                    ),
                })?;
                out.write_all(&piece)?;
                written += piece.len() as u64;
            }
            if written != *size {
                bail!(
                    "blaze's store has {written} bytes of {}/{}, which has {size}",
                    manifest.of.path,
                    entry.path.escape_ascii()
                );
            }
            out.set_modified(dated())?;
            let mode = if *executable { 0o555 } else { 0o444 };
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    };
    for entry in &manifest.entries {
        match &entry.kind {
            Kind::Directory => {
                std::fs::create_dir(at(&entry.path))?;
                directories.push(at(&entry.path));
            }
            Kind::Symlink { target } => {
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), at(&entry.path))?
            }
            Kind::File { size, .. } => {
                batch.push(entry);
                batched += size;
                if batched >= BATCH {
                    write_batch(&mut batch)?;
                    batched = 0;
                }
            }
        }
    }
    write_batch(&mut batch)?;
    // From the deepest, as nothing can be made in a directory that is read-only. The root stays
    // writable until it is renamed into place: macOS refuses to rename a directory it may not
    // write (its `..` changes), which Linux allows.
    for directory in directories.iter().rev().filter(|it| it.as_path() != root) {
        seal(directory)?;
    }
    Ok(())
}

fn seal(directory: &Path) -> io::Result<()> {
    std::fs::File::open(directory)?.set_modified(dated())?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o555))
}

fn dated() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1)
}

/// Removes the tree at `path`, read-only as a laid-out one is, if it is there.
pub fn remove(path: &Path) -> io::Result<()> {
    let Ok(meta) = path.symlink_metadata() else { return Ok(()) };
    if meta.is_dir() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        for entry in std::fs::read_dir(path)? {
            remove(&entry?.path())?;
        }
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_reads_back_as_it_was_written() {
        let path = |name: &str, byte: u8| StorePath {
            path: format!("/nix/store/{}-{name}", "a".repeat(32)),
            nar_hash: Sha256([byte; 32]),
            nar_size: u64::from(byte) * 1000,
        };
        let manifest = Manifest {
            of: path("x", 1),
            references: vec![path("x", 1), path("y", 2)],
            entries: vec![
                Entry { path: b"".to_vec(), kind: Kind::Directory },
                Entry {
                    path: b"bin".to_vec(),
                    kind: Kind::File { executable: true, size: 3, pieces: vec![[7; 32]] },
                },
                Entry { path: b"lib".to_vec(), kind: Kind::Symlink { target: b"/nix/x".to_vec() } },
            ],
        };
        let bytes = manifest.encode();
        assert_eq!(Manifest::decode(&bytes), Some(manifest));
        for cut in [1, 9, bytes.len() / 2, bytes.len() - 1] {
            assert_eq!(Manifest::decode(&bytes[..cut]), None, "cut at {cut}");
        }
        assert_eq!(Manifest::decode(&[&bytes[..], b"x"].concat()), None);
    }
}
