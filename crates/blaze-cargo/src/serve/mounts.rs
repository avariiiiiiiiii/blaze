//! Where each fetched source and the standard library are mounted (docs/v2/plan/lanes/cargo/notes.md
//! section 3): `at -> root`, kept durable in the VFS's journal as one head per mount, so a
//! restart recovers every source fetched before and fetches none of them again.

use std::collections::BTreeMap;

use blaze_ids::ContentId;
use blaze_units::query::{Mount, Origin};
use blaze_vfs::{CommitError, Op, OpError, Ticket, Vfs};

use crate::serve::work::Source;

/// What every mount's head is named by: `mount:<at>`, a NUL, then what `at` is under the root.
/// A NUL is in no path.
const PREFIX: &str = "mount:";

fn head_of(mount: &Mount) -> String {
    format!("{PREFIX}{}\0{}", mount.at, mount.under)
}

fn parse(head: &str) -> Option<(String, String)> {
    let (at, under) = head.strip_prefix(PREFIX)?.split_once('\0')?;
    Some((at.to_owned(), under.to_owned()))
}

/// Every source mounted, by `at`.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Mounts {
    by_at: BTreeMap<String, Mount>,
}

impl Mounts {
    /// The mounts a VFS recovered from its journal kept.
    pub fn recover(vfs: &Vfs) -> Mounts {
        let mut by_at = BTreeMap::new();
        for (head, root) in vfs.heads() {
            if let Some((at, under)) = parse(head) {
                by_at.insert(at.clone(), Mount { at, origin: Origin::Root(root), under });
            }
        }
        Mounts { by_at }
    }

    pub fn get(&self, at: &str) -> Option<&Mount> {
        self.by_at.get(at)
    }

    pub fn has(&self, at: &str) -> bool {
        self.by_at.contains_key(at)
    }

    /// Every mount, sorted by `at`.
    pub fn all(&self) -> impl Iterator<Item = &Mount> {
        self.by_at.values()
    }

    /// The roots the mounts hold, for a collection to keep.
    pub fn roots(&self) -> Vec<ContentId> {
        self.by_at
            .values()
            .filter_map(|it| match it.origin {
                Origin::Root(root) => Some(root),
                Origin::View => None,
            })
            .collect()
    }

    /// `source` put into the VFS as a root of its own and mounted at `at`, durably once the
    /// ticket is. A source mounted there before is replaced: a root is a function of its bytes,
    /// so a fetch of the same source gives the same root.
    pub fn mount(&mut self, vfs: &mut Vfs, at: &str, source: &Source) -> Result<(Mount, Ticket), String> {
        let ops: Vec<Op> = source
            .files
            .iter()
            .map(|file| Op::Write { path: file.path.clone(), bytes: file.bytes.clone(), mode: file.mode })
            .collect();
        let root = vfs
            .apply(vfs.empty(), &ops)
            .map_err(|error| format!("the files of the source for {at} don't make a tree: {}", op_error(&error)))?;
        let mount = Mount { at: at.to_owned(), origin: Origin::Root(root), under: source.under.clone() };
        let ticket = vfs.set_head(&head_of(&mount), root).map_err(|error| commit_error(at, &error))?;
        if let Some(old) = self.by_at.insert(at.to_owned(), mount.clone())
            && old.under != mount.under
        {
            _ = vfs.remove_head(&head_of(&old));
        }
        Ok((mount, ticket))
    }
}

fn op_error(error: &OpError) -> String {
    error.to_string()
}

fn commit_error(at: &str, error: &CommitError) -> String {
    format!("keeping the mount at {at}: {error}")
}

#[cfg(test)]
mod tests {
    use blaze_core::FileName;
    use blaze_vfs::Mode;

    use super::*;
    use crate::serve::work::SourceFile;

    fn source(files: &[(&str, &[u8])], under: &str) -> Source {
        Source {
            files: files
                .iter()
                .map(|(path, bytes)| SourceFile { path: (*path).to_owned(), bytes: bytes.to_vec(), mode: Mode::Regular })
                .collect(),
            under: under.to_owned(),
        }
    }

    #[test]
    fn a_mount_is_a_head_and_comes_back_from_the_heads() {
        let journal = FileName::new("journal").unwrap();
        let (mut vfs, _) = Vfs::recover(journal, 0, &[]);
        let mut mounts = Mounts::default();
        let at = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.219";
        let (mount, _) = mounts.mount(&mut vfs, at, &source(&[("Cargo.toml", b"[package]\n")], "")).unwrap();
        let index = "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/se/rd/serde";
        mounts.mount(&mut vfs, index, &source(&[("serde", b"{}\n")], "serde")).unwrap();
        let again = Mounts::recover(&vfs);
        assert_eq!(again, mounts);
        assert_eq!(again.get(at), Some(&mount));
        assert_eq!(again.get(index).unwrap().under, "serde");
        assert_eq!(again.roots().len(), 2);
    }

    #[test]
    fn the_same_bytes_are_the_same_root() {
        let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
        let mut mounts = Mounts::default();
        let files = source(&[("src/lib.rs", b"pub fn f() {}\n")], "");
        let (one, _) = mounts.mount(&mut vfs, "/a", &files).unwrap();
        let (two, _) = mounts.mount(&mut vfs, "/b", &files).unwrap();
        assert_eq!(one.origin, two.origin);
    }
}
