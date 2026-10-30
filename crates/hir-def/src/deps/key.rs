//! The key of a dependency's image: a hash of every input its def map, lang items and
//! signatures are made from.
//!
//! - [`Crate::content`]: the root file's path, edition, cfg, features (as cfg), env, crate
//!   attributes, target, toolchain, and each dependency's own content ID.
//! - The content of the source root of the crate's root file: each file's path and text.
//! - [`ProcessEnv::content`]: the variables `env!` reads when the crate's env has no value.
//! - The key of each dependency, in the order the crate lists them: its files' texts, which
//!   [`Crate::content`] leaves out.
//! - `build`, which names the blaze that reads and writes the image (its front end decides
//!   what a crate resolves to), and [`PRODUCER`], the version of each encoding in the image.
//!
//! A file of the crate outside its source root (a `#[path]` out of the crate, an `include!`
//! of `OUT_DIR`) is not in the key: the frozen def map names it with its text's hash, which
//! [`crate::nameres::frozen::unreadable`] checks before the image is used.

use base_db::{Crate, ProcessEnv, SourceDatabase};
use blaze_ids::{ContentId, Hasher};
use rustc_hash::FxHashMap;

use crate::{deps::image::IMAGE_SCHEMA, deps::signatures, nameres::frozen};

/// The domain of an image key.
pub const KEY_DOMAIN: &str = "blaze v2 dependency image key 1";

/// What makes the images: the version of each encoding in them, so an image of another
/// encoding is never found.
pub static PRODUCER: std::sync::LazyLock<ContentId> = std::sync::LazyLock::new(|| {
    let mut hasher = Hasher::new(KEY_DOMAIN);
    hasher.part(b"producer");
    hasher.id(&IMAGE_SCHEMA.0);
    hasher.part(frozen::ENCODING.as_bytes());
    hasher.id(&signatures::SCHEMA.0);
    hasher.finish()
});

/// Whether `krate` is read from an image: a crate no workspace member, none of whose
/// dependencies is one either (a member's def map changes with every edit, so nothing that
/// names it is kept).
pub fn eligible(db: &dyn SourceDatabase, krate: Crate) -> bool {
    !krate.data(db).origin.is_local()
        && krate.transitive_deps(db).iter().all(|dep| !dep.data(db).origin.is_local())
}

/// The key of each of `crates` that is [`eligible`], by `build`, the blaze that reads and
/// writes the images. A crate's key covers its dependencies' keys, so they are made too.
pub fn keys(
    db: &dyn SourceDatabase,
    crates: &[Crate],
    build: ContentId,
) -> FxHashMap<Crate, ContentId> {
    let env = ProcessEnv::try_get(db).map(|it| it.content(db));
    let mut done = FxHashMap::default();
    for &krate in crates {
        if eligible(db, krate) {
            key(db, krate, build, env, &mut done);
        }
    }
    done
}

/// The key of `krate`, an [`eligible`] crate, its dependencies' first. The crate graph has no
/// cycles, which [`Crate::transitive_deps`] relies on too, so the walk ends.
fn key(
    db: &dyn SourceDatabase,
    krate: Crate,
    build: ContentId,
    env: Option<ContentId>,
    done: &mut FxHashMap<Crate, ContentId>,
) -> ContentId {
    if let Some(&key) = done.get(&krate) {
        return key;
    }
    let data = krate.data(db);
    let deps: Vec<ContentId> =
        data.dependencies.iter().map(|dep| key(db, dep.crate_id, build, env, done)).collect();
    let root = db.file_source_root(data.root_file_id).source_root_id(db);
    let mut hasher = Hasher::new(KEY_DOMAIN);
    hasher.id(&PRODUCER).id(&build).id(&krate.content(db)).id(&db.source_root(root).content(db));
    match env {
        Some(env) => hasher.update(&[1]).id(&env),
        None => hasher.update(&[0]),
    };
    hasher.update(&(deps.len() as u64).to_le_bytes());
    for dep in &deps {
        hasher.id(dep);
    }
    let key = hasher.finish();
    done.insert(krate, key);
    key
}
