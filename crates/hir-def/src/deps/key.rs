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

use base_db::{Db, Crate, ProcessEnv};
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
pub fn eligible(db: &Db, krate: Crate) -> bool {
    let mut memo = FxHashMap::default();
    walk(db, krate, &mut |_, _| ContentId::default(), &mut memo).is_some()
}

/// The key of each of `crates` that is [`eligible`], by `build`, the blaze that reads and
/// writes the images. A crate's key covers its dependencies' keys, so they are made too.
///
/// One walk of the crate graph: each crate is visited once, whether eligible and its key
/// decided together, so the work is the crates and their dependency edges, each edge a 32-byte
/// hash input.
pub fn keys(db: &Db, crates: &[Crate], build: ContentId) -> FxHashMap<Crate, ContentId> {
    let env = ProcessEnv::try_get(db).map(|it| it.content(db));
    let mut memo = FxHashMap::default();
    let mut hash = |krate, memo: &Memo| key(db, krate, build, env, memo);
    for &krate in crates {
        walk(db, krate, &mut hash, &mut memo);
    }
    // alloc: setup: the keys of a session, once
    memo.into_iter().filter_map(|(krate, key)| Some((krate, key?))).collect()
}

/// Each crate walked: its key, or `None` when it is not [`eligible`].
type Memo = FxHashMap<Crate, Option<ContentId>>;

/// `krate`'s key by `hash` of it and the memo holding its dependencies' keys, if it and every
/// crate it depends on is eligible, its dependencies' first. The crate graph has no cycles,
/// which [`Crate::transitive_deps`] relies on too, so the walk ends.
fn walk(
    db: &Db,
    krate: Crate,
    hash: &mut impl FnMut(Crate, &Memo) -> ContentId,
    memo: &mut Memo,
) -> Option<ContentId> {
    if let Some(&key) = memo.get(&krate) {
        return key;
    }
    let data = krate.data(db);
    let mut eligible = !data.origin.is_local();
    for dep in &data.dependencies {
        eligible &= walk(db, dep.crate_id, hash, memo).is_some();
    }
    let key = eligible.then(|| hash(krate, memo));
    memo.insert(krate, key);
    key
}

/// The key of `krate` from its inputs and its dependencies' keys in `memo`, in the order the
/// crate lists them: each is there, since [`walk`] found every dependency eligible.
fn key(db: &Db, krate: Crate, build: ContentId, env: Option<ContentId>, memo: &Memo) -> ContentId {
    let data = krate.data(db);
    let root = db.file_source_root(data.root_file_id).source_root_id(db);
    let mut hasher = Hasher::new(KEY_DOMAIN);
    hasher.id(&PRODUCER).id(&build).id(&krate.content(db)).id(&db.source_root(root).content(db));
    match env {
        Some(env) => hasher.update(&[1]).id(&env),
        None => hasher.update(&[0]),
    };
    hasher.update(&(data.dependencies.len() as u64).to_le_bytes());
    for dep in &data.dependencies {
        hasher.id(&memo[&dep.crate_id].expect("`walk` keys every dependency first"));
    }
    hasher.finish()
}
