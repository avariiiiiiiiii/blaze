//! Dependency crates read from images (docs/v2/plan/design/frontend.md item 5; #1293, #1294).
//!
//! A dependency (a crate whose origin is not local, all of whose dependencies are dependencies
//! too) is read from its *image*: one [`blaze_format`] buffer holding what the front end needs
//! of the crate without its source: its frozen `DefMap` and lang items (the portable encoding of
//! [`crate::nameres::frozen`]) and its items' signatures ([`signatures`]). The image is keyed by
//! every input of the crate ([`key`]) and kept by the store; [`serve`] gives a database the
//! images the store has, and [`store`] writes those it lacks once the crates are resolved.
//!
//! On a hit nothing of the crate is parsed until something asks for a body (codegen of a
//! generic or `#[inline]` function, a function not in the code cache, a const evaluation:
//! [`crate::nameres::demand`]), and that parses the file as an outline, whose lazy bodies are
//! parsed one at a time as they are walked into (`syntax::SourceFile::parse_outline`).
//! `blaze check` of the workspace checks its own crates, which are never dependencies, fully.
//!
//! An image is read in place: its parts are slices of the one buffer, each found by its tag
//! with no copy, and a signature is decoded when its item is asked for, alone.

mod image;
mod key;
mod serve;
pub(crate) mod signatures;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use base_db::{Crate, SourceDatabase};
use rustc_hash::FxHashMap;

pub use crate::deps::{
    image::{IMAGE_SCHEMA, ImageError, ImageRef, Part, write_image},
    key::{KEY_DOMAIN, PRODUCER, eligible, keys},
    serve::{ImageStore, Refused, Served, Stored, serve, store},
};
pub use crate::nameres::frozen::Mode;

/// An image a database was given: the buffer, which [`serve`] opened and checked against its
/// key, and what is done with it.
#[derive(Clone)]
pub struct Image {
    key: base_db::ContentId,
    bytes: Arc<blaze_format::Aligned>,
    mode: Mode,
}

impl Image {
    /// The image, read in place. [`serve`] checked the buffer when it made the image, so this
    /// opens it again without a hash.
    pub fn view(&self) -> ImageRef<'_> {
        ImageRef::open(&self.bytes, self.key)
            .unwrap_or_else(|why| panic!("a served image of key {:?} stopped reading: {why}", self.key))
    }

    pub fn key(&self) -> base_db::ContentId {
        self.key
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
}

/// Images compare by their key: the store gives the bytes of a key only after checking their
/// content hash, so equal keys are equal bytes.
impl PartialEq for Image {
    fn eq(&self, other: &Image) -> bool {
        self.key == other.key && self.mode == other.mode
    }
}

impl Eq for Image {}

impl std::hash::Hash for Image {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
        self.mode.hash(state);
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("key", &self.key)
            .field("len", &self.bytes.len())
            .field("mode", &self.mode)
            .finish()
    }
}

/// The images of the crates a database reads from them, set by [`serve`].
#[salsa::input(singleton, debug)]
pub struct DepImages {
    #[returns(ref)]
    pub images: Arc<FxHashMap<Crate, Image>>,
}

/// The image `krate` is read from, if it is.
pub(crate) fn image(db: &dyn SourceDatabase, krate: Crate) -> Option<ImageRef<'_>> {
    DepImages::try_get(db)?.images(db).get(&krate).map(Image::view)
}

/// What is done with the image of `krate`, if it has one: [`Mode::Serve`] reads it in place of
/// computing, [`Mode::Check`] computes as well and records a difference, [`Mode::Verify`]
/// writes what was read and records a difference from the bytes.
pub(crate) fn mode(db: &dyn SourceDatabase, krate: Crate) -> Option<Mode> {
    DepImages::try_get(db)?.images(db).get(&krate).map(Image::mode)
}
