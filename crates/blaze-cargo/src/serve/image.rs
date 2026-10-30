//! The daemon's one image file (docs/v2/plan/design/restore.md, "The state is one mapped
//! image"): the database's sections, the VFS's, the plane's history and the symbols they name,
//! made in one builder and written in one write, then mapped back for the database to settle
//! onto.
//!
//! - An image is asked for after a cold build, by a snapshot and at a stop. One asked while
//!   another is being made is made once that one is mapped.
//! - The VFS writes its sections first. While its journal moves to another image
//!   ([`ImageError::Busy`]) nothing is written and the VFS is flushed; the image is tried again
//!   each turn. Busy when the VFS has nothing left to do is an error, and the old image stays.
//! - Once the write is renamed into place the VFS is told ([`Vfs::image_durable`]) and
//!   flushed, which rewrites its journal onto the image; the file is then mapped and opened,
//!   for [`Database::settle`].
//! - The plane's history is the predictor's bytes, a [`History`], in
//!   [`SectionId::PLANE_HISTORY`] under [`HISTORY`]. One of another blaze starts an empty
//!   history, which changes how work is ordered and admitted, never what is built; one that is
//!   corrupt or does not decode is an error.

#![forbid(unsafe_code)]

use std::{
    panic::resume_unwind,
    sync::{Arc, Mutex},
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::SchemaHash;
use blaze_history::{DecodeError, History};
use blaze_image::{Builder, Image, SectionId, Stable};
use blaze_query::Database;
use blaze_vfs::{ImageError, ImageMark, Vfs};

/// The schema of the plane's history section: [`History::encode`]'s bytes, by their version.
pub const HISTORY: SchemaHash =
    blaze_format::schema("blaze plane history 2: blaze_history::History::encode bytes, version blzhis01");

/// The plane's history in `image`, as `config` sizes it. An image written without one, or
/// with one of another blaze, gives an empty history and says so on stderr.
pub fn history<S: Stable>(image: &Image<S>, config: blaze_history::Config) -> Result<History, String> {
    let section = match image.section(SectionId::PLANE_HISTORY, HISTORY) {
        Ok(section) => section,
        Err(refused) => {
            eprintln!("blaze serve: {refused}; the plane's history starts empty");
            return Ok(History::new(config));
        }
    };
    let bytes = image.whole(&section).map_err(|corrupt| corrupt.to_string())?;
    match History::decode(bytes, config) {
        Ok(history) => Ok(history),
        Err(DecodeError::Version) => {
            eprintln!("blaze serve: the image's {} is of another blaze; it starts empty", SectionId::PLANE_HISTORY);
            Ok(History::new(config))
        }
        Err(error) => Err(format!("{}: {error}", SectionId::PLANE_HISTORY)),
    }
}

/// What an image is made of: the parts of the daemon that write its sections.
pub struct Parts<'a> {
    pub db: &'a Database,
    pub vfs: &'a mut Vfs,
    pub predict: &'a History,
    pub effects: &'a mut Vec<Effect>,
}

enum State {
    Idle,
    /// The VFS's journal is moving to another image: made once it can be.
    Waiting,
    Writing { mark: ImageMark },
    Mapping,
}

pub struct Images {
    file: FileName,
    write: Tag,
    map: Tag,
    state: State,
    /// Another image was asked while one was being made.
    again: bool,
    /// The last image made, mapped, until the database settles onto it.
    mapped: Option<Arc<Image<Mapping>>>,
    /// Why the last image asked was not made; cleared when one is asked again.
    failed: Option<String>,
}

impl Images {
    /// The image at `file`, written and mapped with the tags `tags` and `tags + 1`.
    pub fn new(file: FileName, tags: u64) -> Images {
        Images {
            file,
            write: Tag(tags),
            map: Tag(tags + 1),
            state: State::Idle,
            again: false,
            mapped: None,
            failed: None,
        }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        tag == self.write || tag == self.map
    }

    /// Asks for an image of the daemon as it is now.
    pub fn ask(&mut self, parts: Parts<'_>) {
        self.failed = None;
        match self.state {
            State::Idle => self.make(parts, false),
            State::Waiting | State::Writing { .. } | State::Mapping => self.again = true,
        }
    }

    /// Makes what waits: an image whose VFS sections could not be written yet, or one asked
    /// while another was made. `idle` is whether the driver has no effect in flight or queued.
    pub fn step(&mut self, parts: Parts<'_>, idle: bool) {
        match self.state {
            State::Waiting => self.make(parts, idle),
            State::Idle if self.again => {
                self.again = false;
                self.make(parts, false);
            }
            State::Idle | State::Writing { .. } | State::Mapping => {}
        }
    }

    fn make(&mut self, parts: Parts<'_>, idle: bool) {
        let mut builder = Builder::new();
        let mark = match parts.vfs.write_image(&mut builder) {
            Ok(mark) => mark,
            Err(ImageError::Busy) => {
                let before = parts.effects.len();
                parts.vfs.flush(parts.effects);
                if idle && parts.effects.len() == before {
                    return self.fail(format!("the daemon image /{}: {}", self.file, ImageError::Busy));
                }
                self.state = State::Waiting;
                return;
            }
            Err(error @ ImageError::Corrupt(_)) => return self.fail(format!("the daemon image /{}: {error}", self.file)),
        };
        let mut live = intern::Live::new();
        if let Err(error) = parts.db.write_image(&mut builder, &mut live) {
            return self.fail(format!("the database's sections of the daemon image /{}: {error}", self.file));
        }
        builder.add(SectionId::PLANE_HISTORY, HISTORY, &parts.predict.encode());
        intern::write_symbols(&live, &mut builder);
        match builder.finish_on(&on_pool) {
            Ok(bytes) => {
                // clone: the effect owns its name; once an image.
                parts.effects.push(Effect::Write { file: self.file.clone(), bytes, tag: self.write });
                self.state = State::Writing { mark };
            }
            Err(error) => self.fail(format!("the daemon image /{}: {error}", self.file)),
        }
    }

    /// Takes a completion of its own.
    pub fn complete(&mut self, vfs: &mut Vfs, completion: Completion, effects: &mut Vec<Effect>) {
        match (std::mem::replace(&mut self.state, State::Idle), completion.done) {
            (State::Writing { mark }, Done::Written) => {
                assert!(vfs.image_durable(mark.name), "the image written is the one the VFS staged last");
                vfs.flush(effects);
                // clone: as the write's.
                effects.push(Effect::Map { file: self.file.clone(), tag: self.map });
                self.state = State::Mapping;
            }
            (State::Writing { .. }, Done::Failed(error)) => {
                self.fail(format!("writing the daemon image /{}: {error}", self.file));
            }
            (State::Mapping, Done::Mapped(mapping)) => match Image::open(mapping) {
                Ok(image) => self.mapped = Some(Arc::new(image)),
                Err(error) => self.fail(format!("the daemon image /{} just written: {error}", self.file)),
            },
            (State::Mapping, Done::Failed(error)) => {
                self.fail(format!("mapping the daemon image /{} just written: {error}", self.file));
            }
            (_, done) => unreachable!("the daemon image's effect completed with {done:?}"),
        }
    }

    fn fail(&mut self, why: String) {
        eprintln!("blaze serve: {why}; the image before it stays");
        self.state = State::Idle;
        self.failed = Some(why);
    }

    /// Whether every image asked was made, or failed.
    pub fn done(&self) -> bool {
        matches!(self.state, State::Idle) && !self.again
    }

    /// Why the last image asked was not made.
    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// The last image made, for the database to settle onto.
    pub fn take_mapped(&mut self) -> Option<Arc<Image<Mapping>>> {
        self.mapped.take()
    }
}

/// [`blaze_image::Jobs`] on the process's worker pool: `job(0)` to `job(n - 1)`, back once all
/// ran; a job's panic is resumed here.
// dyn: the shape of `blaze_image::Jobs`; one call per section job of an image, each a whole section's work
pub fn on_pool(n: usize, job: &(dyn Fn(usize) + Sync)) {
    let panicked = Mutex::new(None);
    blaze_graph::exec::shared().scope(|scope| {
        for at in 0..n {
            let panicked = &panicked;
            scope.submit(None, move || job(at), move |ran| {
                if let Err(panic) = ran {
                    panicked.lock().unwrap_or_else(|it| it.into_inner()).get_or_insert(panic);
                }
            });
        }
    });
    if let Some(panic) = panicked.into_inner().unwrap_or_else(|it| it.into_inner()) {
        resume_unwind(panic);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn jobs_on_the_pool_each_run_once() {
        let ran: Vec<AtomicUsize> = (0..100).map(|_| AtomicUsize::new(0)).collect();
        on_pool(ran.len(), &|at| _ = ran[at].fetch_add(1, Ordering::Relaxed));
        assert!(ran.iter().all(|it| it.load(Ordering::Relaxed) == 1));
    }

    #[test]
    fn a_jobs_panic_is_the_callers() {
        let caught = std::panic::catch_unwind(|| on_pool(8, &|at| assert_ne!(at, 5, "job five")));
        assert!(caught.is_err());
    }

    fn with_history(schema: SchemaHash, bytes: &[u8]) -> Image<Vec<u8>> {
        let mut builder = Builder::new();
        builder.add(SectionId::PLANE_HISTORY, schema, bytes);
        Image::open(builder.finish().unwrap()).unwrap()
    }

    fn empty() -> Vec<u8> {
        History::new(blaze_history::Config::default()).encode()
    }

    #[test]
    fn the_plane_history_survives_an_image() {
        let mut learned = History::new(blaze_history::Config::default());
        let key = blaze_graph::plane::Key([7; 32]);
        for n in 0..50u64 {
            learned.observe(blaze_graph::admission::kinds::COMPILE, Some(&key), n * 1000, n << 10);
        }
        let image = with_history(HISTORY, &learned.encode());
        let restored = history(&image, blaze_history::Config::default()).unwrap();
        assert_eq!(restored.encode(), learned.encode());
    }

    #[test]
    fn a_history_of_another_blaze_or_none_starts_empty() {
        let config = blaze_history::Config::default();
        let none = Image::open(Builder::new().finish().unwrap()).unwrap();
        assert_eq!(history(&none, config).unwrap().encode(), empty());
        let estimates = with_history(blaze_graph::history::SCHEMA, b"blzest01");
        assert_eq!(history(&estimates, config).unwrap().encode(), empty());
        let older = with_history(HISTORY, b"blzhis00 and what followed");
        assert_eq!(history(&older, config).unwrap().encode(), empty());
    }

    #[test]
    fn a_history_that_does_not_decode_stops_the_daemon_naming_it() {
        let mut cut = empty();
        cut.truncate(cut.len() - 1);
        let error = history(&with_history(HISTORY, &cut), blaze_history::Config::default()).unwrap_err();
        assert!(error.contains("the plane's history"), "{error}");
    }
}
