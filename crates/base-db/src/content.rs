//! The content IDs of the front end's inputs (docs/v2/crates.md, "IDs"; docs/v2/plan/model.md):
//! every input carries the [`ContentId`] of what it is, so a result can be keyed
//! by the hashes of what it read rather than by revision, and two views with equal inputs share
//! it. Only content is hashed, never a `FileId`, `Crate`, `SourceRootId` or other dense index: a
//! file is named by its path, another input by its own content ID.

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
    let mut atoms: Vec<Vec<u8>> = Vec::new();
    for atom in cfg {
        let mut bytes = Vec::new();
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
        atoms.push(bytes);
    }
    atoms.sort_unstable();
    let mut hasher = Hasher::new(CFG_OPTIONS_DOMAIN);
    len(&mut hasher, atoms.len());
    for atom in &atoms {
        hasher.part(atom);
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
    key.extend_from_slice(path.to_string().as_bytes());
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
