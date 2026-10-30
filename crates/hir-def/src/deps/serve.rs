//! Giving a database the images a store has ([`serve`]) and writing those it lacks
//! ([`store`]).
//!
//! The store is the driver's ([`ImageStore`]): the local store or the global cache, which
//! checks an image's content hash on every read, so the front end does not depend on either.

use std::sync::Arc;

use base_db::{Crate, SourceDatabase};
use blaze_format::Aligned;
use blaze_ids::ContentId;
use hir_expand::{MacroCallKind, MacroDefKind, builtin::include_input_to_file_id};
use rustc_hash::FxHashMap;

use crate::{
    Lookup,
    deps::{
        DepImages, Image,
        image::{ImageRef, Part, write_image},
        key::keys,
        signatures,
    },
    nameres::{
        crate_def_map,
        frozen::{self, Frozen, Mode},
    },
};

/// Where images are kept: what the driver adapts the store or the global cache to.
pub trait ImageStore {
    /// The image kept under `key`, its bytes checked against the content hash it was put
    /// with; `Err` when the store could not be read.
    fn get(&self, key: &ContentId) -> Result<Option<Arc<Aligned>>, String>;

    /// Keeps `image` under `key`. A second put of a key with other bytes is an error: a key
    /// names one image.
    fn put(&self, key: ContentId, image: Vec<u8>) -> Result<(), String>;
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
pub(crate) fn crate_name(db: &dyn SourceDatabase, krate: Crate) -> String {
    match &krate.extra_data(db).display_name {
        Some(name) => name.to_string(),
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
    db: &mut dyn SourceDatabase,
    crates: &[Crate],
    build: ContentId,
    images: &dyn ImageStore,
    mode: Mode,
) -> Result<Served, String> {
    let keys = keys(&*db, crates, build);
    let mut order: Vec<Crate> = keys.keys().copied().collect();
    order.sort_by_cached_key(|&krate| keys[&krate]);
    let mut served = FxHashMap::default();
    let mut maps = FxHashMap::default();
    let mut lang_items = FxHashMap::default();
    let mut hits = Vec::new();
    for krate in order {
        let key = keys[&krate];
        let name = || crate_name(&*db, krate);
        let Some(bytes) = images.get(&key).map_err(|why| {
            format!("reading the image of crate {} (key {key:?}): {why}", name())
        })?
        else {
            continue;
        };
        let image = ImageRef::open(&bytes, key)
            .map_err(|why| format!("the image of crate {} (key {key:?}): {why}", name()))?;
        let def_map = image.part(Part::DefMap);
        if let Some(why) = frozen::unreadable(&*db, krate, def_map) {
            return Err(format!(
                "the image of crate {} (key {key:?}) names files that are not the crate's: \
                 {why}; the key leaves out an input",
                name()
            ));
        }
        maps.insert(krate, Frozen { bytes: def_map.into(), mode });
        lang_items.insert(krate, Frozen { bytes: image.part(Part::LangItems).into(), mode });
        served.insert(krate, Image { key, bytes, mode });
        hits.push(krate);
    }
    frozen::set(db, maps);
    frozen::set_lang_items(db, lang_items);
    set_images(db, served);
    Ok(Served { keys, hits })
}

fn set_images(db: &mut dyn SourceDatabase, images: FxHashMap<Crate, Image>) {
    use salsa::{Durability, Setter};
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

/// Writes to `images` the image of each crate [`serve`] found none of, resolving it in `db`.
/// A crate whose image would not cover every input is refused, with why. An image the store
/// will not keep is an error naming the crate.
pub fn store(
    db: &dyn SourceDatabase,
    served: &Served,
    images: &dyn ImageStore,
) -> Result<Stored, String> {
    let mut order: Vec<Crate> =
        served.keys.keys().copied().filter(|krate| !served.hits.contains(krate)).collect();
    order.sort_by_cached_key(|&krate| served.keys[&krate]);
    let mut stored = Stored::default();
    for krate in order {
        let key = served.keys[&krate];
        let parts = match refusal(db, krate).map_or_else(|| encode(db, krate), Err) {
            Ok(parts) => parts,
            Err(why) => {
                stored.refused.push((krate, why));
                continue;
            }
        };
        let image = write_image(key, [&parts[0], &parts[1], &parts[2]]);
        images.put(key, image).map_err(|why| {
            format!("keeping the image of crate {} (key {key:?}): {why}", crate_name(db, krate))
        })?;
        stored.written.push(krate);
    }
    Ok(stored)
}

/// The parts of the image of `krate`, in the order of [`Part::ALL`].
fn encode(db: &dyn SourceDatabase, krate: Crate) -> Result<[Vec<u8>; 3], Refused> {
    let refused = |part| move |why| Refused::Encode { part, why };
    Ok([
        frozen::encode(db, krate).map_err(refused(Part::DefMap))?,
        frozen::encode_lang_items(db, krate).map_err(refused(Part::LangItems))?,
        signatures::encode(db, krate).map_err(refused(Part::Signatures))?,
    ])
}

/// Why the image of `krate` would leave out an input, if it would.
pub(super) fn refusal(db: &dyn SourceDatabase, krate: Crate) -> Option<Refused> {
    let def_map = crate_def_map(db, krate);
    if def_map.session_macros {
        return Some(Refused::SessionMacros);
    }
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    for (_, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id() {
            let file = file.file_id(db);
            if db.file_source_root(file).source_root_id(db) != root {
                return Some(Refused::OutsideSourceRoot { what: format!("module file {file:?}") });
            }
        }
        for call in module.scope(db).all_macro_calls() {
            let loc = call.lookup(db);
            match &loc.def.kind {
                MacroDefKind::ProcMacro(..) => {
                    let call = format!("{:?} of crate {}", call, crate_name(db, loc.def.krate));
                    return Some(Refused::ProcMacro { call });
                }
                MacroDefKind::BuiltInEager(_, expander)
                    if expander.is_include_like() && include_outside(db, call, root) =>
                {
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
    db: &dyn SourceDatabase,
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