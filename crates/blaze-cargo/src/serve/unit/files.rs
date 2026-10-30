//! Where a unit's files are: every path cargo names is under a mount of the build
//! (`BuildConfig::mounts`: the view's checkout, each fetched source, the standard library), read
//! through the VFS's reader, never from the disk; and the name rustc gives a file in a
//! diagnostic, from the root of the workspace when it is below it.

use std::{collections::HashMap, sync::Arc};

use base_db::FileId;
use blaze_ids::ContentId;
use blaze_units::query::{Mount, Origin};
use load_cargo::v2::Lowered;

/// The mounts of a build, to find a path's file in.
#[derive(Clone)]
pub(crate) struct Mounted {
    reader: blaze_vfs::Reader,
    view: ContentId,
    /// Longest `at` first, so the first that holds a path is its mount.
    mounts: Vec<Mount>,
    /// The workspace's root, which names are relative to.
    root: String,
}

impl Mounted {
    pub(crate) fn new(reader: blaze_vfs::Reader, view: ContentId, mounts: &[Mount], root: &str) -> Mounted {
        let mut mounts = mounts.to_vec();
        mounts.sort_by(|a, b| b.at.len().cmp(&a.at.len()).then_with(|| a.at.cmp(&b.at)));
        Mounted { reader, view, mounts, root: root.trim_end_matches('/').to_owned() }
    }

    /// The root and path in it of absolute `path`: the mount with the longest `at` holding it.
    pub(crate) fn locate(&self, path: &str) -> Option<(ContentId, String)> {
        self.mounts.iter().find_map(|mount| {
            let rest = under(path, &mount.at)?;
            let root = match mount.origin {
                Origin::View => self.view,
                Origin::Root(id) => id,
            };
            let inner = match (mount.under.as_str(), rest) {
                ("", rest) => rest.to_owned(),
                (under, "") => under.to_owned(),
                (under, rest) => format!("{under}/{rest}"),
            };
            Some((root, inner))
        })
    }

    /// The bytes of the file at absolute `path`, with its content ID.
    pub(crate) fn read(&self, path: &str) -> Option<(ContentId, blaze_vfs::Mode, Arc<[u8]>)> {
        let (root, inner) = self.locate(path)?;
        self.reader.file(root, &inner)
    }

    /// Every file below the directory at absolute `dir`, by `/`-separated path under it.
    pub(crate) fn walk(&self, dir: &str) -> Option<Vec<(String, Arc<[u8]>)>> {
        let (root, inner) = self.locate(dir)?;
        let mut out = Vec::new();
        let mut pending = vec![String::new()];
        while let Some(at) = pending.pop() {
            let full = join(&inner, &at);
            for (name, is_dir) in self.reader.list(root, &full)? {
                let rel = join(&at, &name);
                match is_dir {
                    true => pending.push(rel),
                    false => {
                        let (_, _, bytes) = self.reader.file(root, &join(&inner, &rel))?;
                        out.push((rel, bytes));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Some(out)
    }

    /// The absolute path of a name rustc gave: relative ones are from the workspace's root.
    pub(crate) fn absolute(&self, name: &str) -> String {
        match name.starts_with('/') {
            true => name.to_owned(),
            false => format!("{}/{name}", self.root),
        }
    }

    /// The name rustc gives absolute `path` in a diagnostic: from the workspace's root when it
    /// is below it, as cargo runs rustc there, else its whole path.
    pub(crate) fn name(&self, path: &str) -> String {
        match under(path, &self.root) {
            Some(rest) if !rest.is_empty() => rest.to_owned(),
            _ => path.to_owned(),
        }
    }

    /// The sources a diagnostic's snippets are read from, by the names [`Mounted::name`] gives.
    pub(crate) fn sources(&self) -> blaze_diagnostics::emit::Sources {
        let mounted = self.clone();
        blaze_diagnostics::emit::Sources::new(move |name| {
            let (_, _, bytes) = mounted.read(&mounted.absolute(name))?;
            String::from_utf8(bytes.to_vec()).ok()
        })
    }
}

/// What of `path` is below `dir`: `""` for `dir` itself.
pub(crate) fn under<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    let dir = dir.trim_end_matches('/');
    let rest = path.strip_prefix(dir)?;
    match rest.strip_prefix('/') {
        Some(rest) => Some(rest),
        None => rest.is_empty().then_some(""),
    }
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{a}/{b}"),
    }
}

/// The names of the front end's files, as rustc gives them: what the diagnostics and the code
/// generator (`file!()`, a panic's location) print.
pub(crate) struct Names<'a> {
    pub(crate) paths: Arc<HashMap<FileId, String>>,
    pub(crate) mounted: &'a Mounted,
}

impl<'a> Names<'a> {
    pub(crate) fn new(lowered: &Lowered, mounted: &'a Mounted) -> Names<'a> {
        Names { paths: lowered.paths(), mounted }
    }
}

impl blaze_diagnostics::Files for Names<'_> {
    fn name(&self, file: FileId) -> String {
        match self.paths.get(&file) {
            Some(path) => self.mounted.name(path),
            None => format!("<file {}>", file.index()),
        }
    }
}
