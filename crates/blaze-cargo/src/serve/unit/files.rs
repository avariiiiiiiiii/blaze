//! Where a unit's files are: every path cargo names is under a mount of the build
//! (`BuildConfig::mounts`: the view's checkout, each fetched source, the standard library), read
//! through the VFS's reader, never from the disk; and the name rustc gives a file in a
//! diagnostic, from the root of the workspace when it is below it.

#![forbid(unsafe_code)]

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use base_db::FileId;
use blaze_ids::ContentId;
use blaze_units::query::{Mount, Origin};
use blaze_vfs::{Bytes, Corrupt};

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
    /// Borrowed from `path` when the mount is a whole root, as the view's and most are.
    pub(crate) fn locate<'p>(&self, path: &'p str) -> Option<(ContentId, Cow<'p, str>)> {
        self.mounts.iter().find_map(|mount| {
            let rest = under(path, &mount.at)?;
            let root = match mount.origin {
                Origin::View => self.view,
                Origin::Root(id) => id,
            };
            let inner = match (mount.under.as_str(), rest) {
                ("", rest) => Cow::Borrowed(rest),
                (under, "") => Cow::Owned(under.to_owned()),
                (under, rest) => Cow::Owned(format!("{under}/{rest}")),
            };
            Some((root, inner))
        })
    }

    /// The bytes of the file at absolute `path`, with its content ID; `Err` for bytes of the
    /// mapped image that fail their check.
    pub(crate) fn read(&self, path: &str) -> Result<Option<(ContentId, blaze_vfs::Mode, Bytes)>, Corrupt> {
        let Some((root, inner)) = self.locate(path) else { return Ok(None) };
        self.reader.file(root, &inner)
    }

    /// Every file below the directory at absolute `dir`, by `/`-separated path under it.
    pub(crate) fn walk(&self, dir: &str) -> Result<Option<Vec<(String, Bytes)>>, Corrupt> {
        let Some((root, inner)) = self.locate(dir) else { return Ok(None) };
        let mut out = Vec::new();
        let mut pending = vec![String::new()];
        while let Some(at) = pending.pop() {
            let full = join(&inner, &at);
            let Some(names) = self.reader.list(root, &full) else { return Ok(None) };
            for (name, is_dir) in names {
                let rel = join(&at, &name);
                match is_dir {
                    true => pending.push(rel),
                    false => {
                        let Some((_, _, bytes)) = self.reader.file(root, &join(&inner, &rel))? else { return Ok(None) };
                        out.push((rel, bytes));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Some(out))
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

    /// The sources a diagnostic's snippets are read from, by the names [`Mounted::name`] gives;
    /// a file whose bytes fail their check gives no snippet.
    pub(crate) fn sources(&self) -> blaze_diagnostics::emit::Sources {
        let mounted = self.clone();
        blaze_diagnostics::emit::Sources::new(move |name| {
            let (_, _, bytes) = mounted.read(&mounted.absolute(name)).ok()??;
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
    pub(crate) paths: &'a HashMap<FileId, String>,
    pub(crate) mounted: &'a Mounted,
}

impl<'a> Names<'a> {
    /// The names of `paths`, the files of a task's reader.
    pub(crate) fn new(paths: &'a Arc<HashMap<FileId, String>>, mounted: &'a Mounted) -> Names<'a> {
        Names { paths, mounted }
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
