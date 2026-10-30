//! The content IDs of the front end's inputs (docs/v2/plan/reference/crates.md, "IDs"; docs/v2/plan/design/model.md):
//! every input carries the [`ContentId`] of what it is, so a result can be keyed
//! by the hashes of what it read rather than by revision, and two views with equal inputs share
//! it. Only content is hashed, never a `FileId`, `Crate`, `SourceRootId` or other dense index: a
//! file is named by its path, another input by its own content ID.

use std::{io::Write as _, ops::Range};

use blaze_ids::{ContentId, Hasher};
use cfg::{CfgAtom, CfgOptions};
use vfs::VfsPath;

/// A content ID as an input holds it. Only base-db makes one, from the value the input is set
/// with, so an input's ID and value never disagree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InputId(pub(crate) ContentId);

impl InputId {
    pub fn id(self) -> ContentId {
        self.0
    }
}

pub const CFG_OPTIONS_DOMAIN: &str = "blaze v2 cfg options 1";

/// The content ID of a set of cfg atoms: sorted by their bytes, so the order of the set does
/// not matter.
pub fn cfg_options_content(cfg: &CfgOptions) -> ContentId {
    // Every atom encoded into one buffer, then sorted as byte strings.
    let mut bytes = Vec::new();
    let mut atoms: Vec<Range<usize>> = Vec::new();
    for atom in cfg {
        let start = bytes.len();
        match atom {
            CfgAtom::Flag(name) => {
                bytes.push(0);
                frame(&mut bytes, name.as_str());
            }
            CfgAtom::KeyValue { key, value } => {
                bytes.push(1);
                frame(&mut bytes, key.as_str());
                frame(&mut bytes, value.as_str());
            }
        }
        atoms.push(start..bytes.len());
    }
    atoms.sort_unstable_by(|a, b| bytes[a.clone()].cmp(&bytes[b.clone()]));
    let mut hasher = Hasher::new(CFG_OPTIONS_DOMAIN);
    len(&mut hasher, atoms.len());
    for atom in atoms {
        hasher.part(&bytes[atom]);
    }
    hasher.finish()
}

fn frame(bytes: &mut Vec<u8>, part: &str) {
    bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
    bytes.extend_from_slice(part.as_bytes());
}

/// A path as content: whether it is on disk (0) or virtual (1), then its text.
pub(crate) fn path_key(path: &VfsPath) -> Vec<u8> {
    let mut key = vec![if path.as_path().is_some() { 0 } else { 1 }];
    write!(key, "{path}").expect("writing to a Vec does not fail");
    key
}

pub(crate) fn len(hasher: &mut Hasher, len: usize) {
    hasher.update(&(len as u64).to_le_bytes());
}

pub(crate) fn opt(hasher: &mut Hasher, part: Option<&str>) {
    match part {
        None => hasher.update(&[0]),
        Some(part) => hasher.update(&[1]).part(part.as_bytes()),
    };
}

/// What [`opt`] hashes, appended to `out`.
pub(crate) fn opt_bytes(out: &mut Vec<u8>, part: Option<&str>) {
    match part {
        None => out.push(0),
        Some(part) => {
            out.push(1);
            frame(out, part);
        }
    }
}

/// `ContentId::of(domain, &buf[range])` for each range, through [`ContentId::of_many`]: the
/// domain's key derived once for all of them, and the inputs laid out in one buffer.
pub(crate) fn hash_ranges(domain: &'static str, buf: &[u8], ranges: &[Range<usize>]) -> Vec<ContentId> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let inputs: Vec<&[u8]> = ranges.iter().map(|range| &buf[range.clone()]).collect();
    ContentId::of_many(domain, &inputs)
}
