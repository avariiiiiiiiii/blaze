//! Giving a database the images a store has ([`serve`]) and writing those it lacks
//! ([`store`]).
//!
//! The store is the driver's ([`ImageStore`]): the local store or the global cache, which
//! checks an image's content hash on every read, so the front end does not depend on either.

use std::{
    ops::Range,
    sync::{Arc, OnceLock},
};

use base_db::{Crate, Db};
use blaze_format::Aligned;
use blaze_ids::ContentId;
use hir_expand::{MacroCallKind, MacroDefKind, builtin::include_input_to_file_id};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    Lookup,
    deps::{
        DepImages, Image,
        image::{ImageRef, Part, PartBytes, write_image},
        key::keys,
        signatures,
    },
    nameres::{
        crate_def_map,
        frozen::{self, Frozen, Mode},
    },
};

/// Where images are kept, and the threads that read and write them: what the driver adapts the
/// store or the global cache and its pool to. `get` and `put` are called from those threads.
pub trait ImageStore: Sync {
    /// The image kept under `key`, its bytes checked against the content hash it was put
    /// with; `Err` when the store could not be read.
    fn get(&self, key: &ContentId) -> Result<Option<Arc<Aligned>>, String>;

    /// Keeps `image` under `key`. A second put of a key with other bytes is an error: a key
    /// names one image.
    fn put(&self, key: ContentId, image: Vec<u8>) -> Result<(), String>;

    /// Runs `task(at)` for each `at` in `0..count`, on every core the driver has: a crate's
    /// image is checked and encoded by one task, so [`serve`] and [`store`] take the larger of
    /// the work over the cores and the largest crate's. Running them in order on one thread
    /// gives the same result.
    // dyn: one call per batch of crates, its task called once per crate
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        (0..count).for_each(task);
    }
}

/// What [`serve`] did.
#[derive(Debug, Default)]
pub struct Served {
    /// The key of each crate read from an image or that can be: the [`eligible`] crates of
    /// those asked, and their dependencies.
    ///
    /// [`eligible`]: super::eligible
    pub keys: FxHashMap<Crate, ContentId>,
    /// The crates read from an image, in the order of `keys`' crates.
    pub hits: Vec<Crate>,
}

/// Why [`store`] wrote no image of a crate. Each is an input the key does not cover, so the
/// image would be served where the crate resolves otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Its def map depends on the macros of the session (`#[macro_export]` from a proc macro
    /// crate of the session): what they expand to is not in the key.
    SessionMacros,
    /// An item of it comes from a procedural macro, whose output may read files and
    /// variables the expansion server traces per expansion, which the key does not cover.
    ProcMacro { call: String },
    /// A module or an `include!` of it is a file outside the crate's source root, whose text
    /// the key does not cover (`OUT_DIR`, a `#[path]` out of the crate).
    OutsideSourceRoot { what: String },
    /// An encoding refused a part of it.
    Encode { part: Part, why: String },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::SessionMacros => f.write_str("its def map uses the session's macros"),
            Refused::ProcMacro { call } => write!(f, "an item comes from procedural macro {call}"),
            Refused::OutsideSourceRoot { what } => {
                write!(f, "{what} is outside the crate's source root")
            }
            Refused::Encode { part, why } => write!(f, "its {} do not encode: {why}", part.name()),
        }
    }
}

/// What [`store`] did.
#[derive(Debug, Default)]
pub struct Stored {
    /// The crates whose image was written.
    pub written: Vec<Crate>,
    /// The crates whose image was not, and why.
    pub refused: Vec<(Crate, Refused)>,
}

/// The name of `krate` in an error.
pub(crate) fn crate_name(db: &Db, krate: Crate) -> String {
    match &krate.extra_data(db).display_name {
        // alloc: error text
        Some(name) => name.to_string(),
        // alloc: error text
        None => format!("of root file {:?}", krate.data(db).root_file_id),
    }
}

/// Reads each of `crates` that is [`eligible`] (and its dependencies) from the image `images`
/// keep under its key, by `build`, the blaze reading: `mode` says what is done with each
/// image. Replaces the frozen def maps and lang items `db` was given.
///
/// An image that does not read under its key is an error naming the crate and why, never a
/// crate resolved instead: the key covers what the image was made from and the store checked
/// the bytes, so such an image is a bug in the key or in an encoding.
///
/// [`eligible`]: super::eligible
pub fn serve(
    db: &mut Db,
    crates: &[Crate],
    build: ContentId,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
    mode: Mode,
) -> Result<Served, String> {
    let keys = keys(&*db, crates, build);
    // alloc: setup: once per session
    let mut order: Vec<(Crate, ContentId)> = keys.iter().map(|(&krate, &key)| (krate, key)).collect();
    order.sort_unstable_by_key(|&(_, key)| key);
    // alloc: setup: one slot per crate, once per session
    let found: Vec<OnceLock<Fetched>> = order.iter().map(|_| OnceLock::new()).collect();
    {
        let db = &*db;
        images.for_each(order.len(), &|at| {
            let (krate, key) = order[at];
            _ = found[at].set(fetch(db, krate, key, images));
        });
    }
    let mut served = FxHashMap::default();
    let mut maps = FxHashMap::default();
    let mut lang_items = FxHashMap::default();
    // alloc: setup: once per session
    let mut hits = Vec::with_capacity(order.len());
    for (&(krate, key), found) in order.iter().zip(found) {
        let found = found.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of crate {}", crate_name(db, krate))
        });
        let Some((bytes, parts)) = found? else { continue };
        let part = |part| PartBytes::of(&bytes, &parts, part).into();
        maps.insert(krate, Frozen { bytes: part(Part::DefMap), mode });
        lang_items.insert(krate, Frozen { bytes: part(Part::LangItems), mode });
        served.insert(krate, Image { key, bytes, parts, mode });
        hits.push(krate);
    }
    frozen::set(db, maps);
    frozen::set_lang_items(db, lang_items);
    set_images(db, served);
    Ok(Served { keys, hits })
}

/// An image found, and where its parts are in it.
type Fetched = Result<Option<(Arc<Aligned>, [Range<usize>; Part::ALL.len()])>, String>;

/// The image of `krate` under `key`, opened and its def map's files checked: one task of
/// [`serve`].
// dyn: the driver's store, one call per crate per session
fn fetch(db: &Db, krate: Crate, key: ContentId, images: &dyn ImageStore) -> Fetched {
    let name = || crate_name(db, krate);
    let Some(bytes) = images
        .get(&key)
        // alloc: error text
        .map_err(|why| format!("reading the image of crate {} (key {key:?}): {why}", name()))?
    else {
        return Ok(None);
    };
    let image = ImageRef::open(&bytes, key)
        // alloc: error text
        .map_err(|why| format!("the image of crate {} (key {key:?}): {why}", name()))?;
    if let Some(why) = frozen::unreadable(db, krate, image.part(Part::DefMap)) {
        // alloc: error text
        return Err(format!(
            "the image of crate {} (key {key:?}) names files that are not the crate's: \
             {why}; the key leaves out an input",
            name()
        ));
    }
    let parts = image.ranges(&bytes);
    Ok(Some((bytes, parts)))
}

fn set_images(db: &mut Db, images: FxHashMap<Crate, Image>) {
    use salsa::{Durability, Setter};
    // alloc: setup: the one input value, once per session
    let images = Arc::new(images);
    match DepImages::try_get(&*db) {
        Some(it) => {
            it.set_images(db).with_durability(Durability::HIGH).to(images);
        }
        None => {
            _ = DepImages::builder(images).durability(Durability::HIGH).new(&*db);
        }
    }
}

/// Writes to `images` the image of each crate [`serve`] found none of, resolving it in `db`,
/// one task a crate ([`ImageStore::for_each`]). A crate whose image would not cover every
/// input is refused, with why. An image the store will not keep is an error naming the crate.
// dyn: the driver's store, one call per crate per session
pub fn store(db: &Db, served: &Served, images: &dyn ImageStore) -> Result<Stored, String> {
    // alloc: setup: once per session
    let hits: FxHashSet<Crate> = served.hits.iter().copied().collect();
    // alloc: setup: once per session
    let mut order: Vec<(Crate, ContentId)> = (served.keys.iter())
        .filter(|(krate, _)| !hits.contains(krate))
        .map(|(&krate, &key)| (krate, key))
        // alloc: setup: once per session
        .collect();
    order.sort_unstable_by_key(|&(_, key)| key);
    let made: Vec<OnceLock<Result<Option<Refused>, String>>> =
        // alloc: setup: one slot per crate, once per session
        order.iter().map(|_| OnceLock::new()).collect();
    images.for_each(order.len(), &|at| {
        let (krate, key) = order[at];
        _ = made[at].set(make(db, krate, key, images));
    });
    let mut stored = Stored::default();
    for (&(krate, _), made) in order.iter().zip(made) {
        let made = made.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of crate {}", crate_name(db, krate))
        });
        match made? {
            None => stored.written.push(krate),
            Some(why) => stored.refused.push((krate, why)),
        }
    }
    Ok(stored)
}

/// Writes the image of `krate` under `key`, or says why it is refused: one task of [`store`].
fn make(
    db: &Db,
    krate: Crate,
    key: ContentId,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
) -> Result<Option<Refused>, String> {
    let parts = match refusal(db, krate).map_or_else(|| encode(db, krate), Err) {
        Ok(parts) => parts,
        Err(why) => return Ok(Some(why)),
    };
    let image = write_image(key, [&parts[0], &parts[1], &parts[2]]);
    drop(parts);
    images.put(key, image).map_err(|why| {
        // alloc: error text
        format!("keeping the image of crate {} (key {key:?}): {why}", crate_name(db, krate))
    })?;
    Ok(None)
}

/// The parts of the image of `krate`, in the order of [`Part::ALL`].
fn encode(db: &Db, krate: Crate) -> Result<[Vec<u8>; 3], Refused> {
    let refused = |part| move |why| Refused::Encode { part, why };
    Ok([
        frozen::encode(db, krate).map_err(refused(Part::DefMap))?,
        frozen::encode_lang_items(db, krate).map_err(refused(Part::LangItems))?,
        signatures::encode(db, krate).map_err(refused(Part::Signatures))?,
    ])
}

/// Why the image of `krate` would leave out an input, if it would.
pub(super) fn refusal(db: &Db, krate: Crate) -> Option<Refused> {
    let def_map = crate_def_map(db, krate);
    if def_map.session_macros {
        return Some(Refused::SessionMacros);
    }
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    for (_, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id() {
            let file = file.file_id(db);
            if db.file_source_root(file).source_root_id(db) != root {
                // alloc: why a crate is refused, once per refused crate
                return Some(Refused::OutsideSourceRoot { what: format!("module file {file:?}") });
            }
        }
        for call in module.scope(db).all_macro_calls() {
            let loc = call.lookup(db);
            match &loc.def.kind {
                MacroDefKind::ProcMacro(..) => {
                    // alloc: why a crate is refused, once per refused crate
                    let call = format!("{:?} of crate {}", call, crate_name(db, loc.def.krate));
                    return Some(Refused::ProcMacro { call });
                }
                MacroDefKind::BuiltInEager(_, expander)
                    if expander.is_include_like() && include_outside(db, call, root) =>
                {
                    // alloc: why a crate is refused, once per refused crate
                    return Some(Refused::OutsideSourceRoot { what: format!("include {call:?}") });
                }
                _ => {}
            }
        }
    }
    None
}

/// Whether the file the eager `include!`, `include_str!` or `include_bytes!` `call` reads is
/// outside the source root `root`, or not known: its eager argument, expanded, is the path.
fn include_outside(
    db: &Db,
    call: hir_expand::MacroCallId,
    root: base_db::SourceRootId,
) -> bool {
    let MacroCallKind::FnLike { eager: Some(eager), .. } = &call.lookup(db).kind else {
        return true;
    };
    match include_input_to_file_id(db, call, &eager.arg) {
        Ok(file) => db.file_source_root(file.file_id(db)).source_root_id(db) != root,
        Err(_) => true,
    }
}