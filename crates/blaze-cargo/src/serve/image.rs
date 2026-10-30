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
//!   history ([`Unlearned`]), which changes how work is ordered and admitted, never what is
//!   built; one that is corrupt or does not decode is an error ([`HistoryError`]).
//! - An image that is not made is an [`ImageFailed`]: the one before it stays on disk, and
//!   whoever asked (the cold build's settle, a snapshot, a stop) fails with it.

#![forbid(unsafe_code)]

use std::{
    fmt,
    panic::resume_unwind,
    sync::{Arc, Mutex},
};

use blaze_core::{Completion, Done, Effect, FileName, IoError, Mapping, Tag};
use blaze_format::SchemaHash;
use blaze_history::{DecodeError, History};
use blaze_image::{Builder, Corrupt, Image, OpenError, Refused, SectionId, Stable, TooLarge};
use blaze_query::{Database, DbError};
use blaze_vfs::{ImageError, ImageMark, Vfs};

/// The schema of the plane's history section: [`History::encode`]'s bytes, by their version.
pub const HISTORY: SchemaHash =
    blaze_format::schema("blaze plane history 2: blaze_history::History::encode bytes, version blzhis01");

/// Why the plane's history starts empty from an image: the image was written by another blaze.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unlearned {
    /// The image has no history section, or one of another schema.
    Refused(Refused),
    /// The section holds another version of [`History::encode`]'s bytes.
    Version,
}

impl fmt::Display for Unlearned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unlearned::Refused(refused) => write!(f, "{refused}"),
            Unlearned::Version => write!(f, "the image's {} is of another blaze", SectionId::PLANE_HISTORY),
        }
    }
}

/// Why the plane's history in an image can't be read: the daemon does not start.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HistoryError {
    Corrupt(Corrupt),
    Decode(DecodeError),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HistoryError::Corrupt(corrupt) => write!(f, "{corrupt}"),
            HistoryError::Decode(error) => write!(f, "{}: {error}", SectionId::PLANE_HISTORY),
        }
    }
}

/// The plane's history in `image`, as `config` sizes it, and why it starts empty when the image
/// was written without one or with one of another blaze.
pub fn history<S: Stable>(
    image: &Image<S>,
    config: blaze_history::Config,
) -> Result<(History, Option<Unlearned>), HistoryError> {
    let section = match image.section(SectionId::PLANE_HISTORY, HISTORY) {
        Ok(section) => section,
        Err(refused) => return Ok((History::new(config), Some(Unlearned::Refused(refused)))),
    };
    let bytes = image.whole(&section).map_err(HistoryError::Corrupt)?;
    match History::decode(bytes, config) {
        Ok(history) => Ok((history, None)),
        Err(DecodeError::Version) => Ok((History::new(config), Some(Unlearned::Version))),
        Err(error) => Err(HistoryError::Decode(error)),
    }
}

/// Why an image asked was not made.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unmade {
    /// The VFS's journal is moving to another image and nothing is left in flight to wait on.
    Busy,
    /// A group of the image the VFS was restored from failed its check.
    Vfs(Corrupt),
    Db(DbError),
    TooLarge(TooLarge),
    Write(IoError),
    Map(IoError),
    /// The image just written does not open.
    Open(OpenError),
}

/// An image of `file` not made, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImageFailed {
    pub file: FileName,
    pub why: Unmade,
}

impl fmt::Display for ImageFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = &self.file;
        match &self.why {
            Unmade::Busy => write!(f, "the daemon image /{file}: {}", ImageError::Busy),
            Unmade::Vfs(corrupt) => write!(f, "the daemon image /{file}: {}", ImageError::Corrupt(*corrupt)),
            Unmade::Db(error) => write!(f, "the database's sections of the daemon image /{file}: {error}"),
            Unmade::TooLarge(error) => write!(f, "the daemon image /{file}: {error}"),
            Unmade::Write(error) => write!(f, "writing the daemon image /{file}: {error}"),
            Unmade::Map(error) => write!(f, "mapping the daemon image /{file} just written: {error}"),
            Unmade::Open(error) => write!(f, "the daemon image /{file} just written: {error}"),
        }
    }
}

/// For the API's error text, at the edge.
impl From<&ImageFailed> for String {
    fn from(failed: &ImageFailed) -> String {
        failed.to_string()
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
    failed: Option<ImageFailed>,
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
                    return self.fail(Unmade::Busy);
                }
                self.state = State::Waiting;
                return;
            }
            Err(ImageError::Corrupt(corrupt)) => return self.fail(Unmade::Vfs(corrupt)),
        };
        let mut live = intern::Live::new();
        if let Err(error) = parts.db.write_image(&mut builder, &mut live) {
            return self.fail(Unmade::Db(error));
        }
        builder.add(SectionId::PLANE_HISTORY, HISTORY, &parts.predict.encode());
        intern::write_symbols(&live, &mut builder);
        match builder.finish_on(&on_pool) {
            Ok(bytes) => {
                // clone: the effect owns its name; once an image.
                parts.effects.push(Effect::Write { file: self.file.clone(), bytes, tag: self.write });
                self.state = State::Writing { mark };
            }
            Err(error) => self.fail(Unmade::TooLarge(error)),
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
            (State::Writing { .. }, Done::Failed(error)) => self.fail(Unmade::Write(error)),
            (State::Mapping, Done::Mapped(mapping)) => match Image::open(mapping) {
                Ok(image) => self.mapped = Some(Arc::new(image)),
                Err(error) => self.fail(Unmade::Open(error)),
            },
            (State::Mapping, Done::Failed(error)) => self.fail(Unmade::Map(error)),
            (_, done) => unreachable!("the daemon image's effect completed with {done:?}"),
        }
    }

    fn fail(&mut self, why: Unmade) {
        self.state = State::Idle;
        // clone: a refcount, once a failed image.
        self.failed = Some(ImageFailed { file: self.file.clone(), why });
    }

    /// Whether every image asked was made, or failed.
    pub fn done(&self) -> bool {
        matches!(self.state, State::Idle) && !self.again
    }

    /// Why the last image asked was not made.
    pub fn failed(&self) -> Option<&ImageFailed> {
        self.failed.as_ref()
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
        let (restored, unlearned) = history(&image, blaze_history::Config::default()).unwrap();
        assert_eq!(restored.encode(), learned.encode());
        assert_eq!(unlearned, None);
    }

    #[test]
    fn a_history_of_another_blaze_or_none_starts_empty_saying_why() {
        let config = blaze_history::Config::default();
        let none = Image::open(Builder::new().finish().unwrap()).unwrap();
        let (restored, unlearned) = history(&none, config).unwrap();
        assert_eq!(restored.encode(), empty());
        assert!(matches!(unlearned, Some(Unlearned::Refused(Refused::Missing { .. }))), "{unlearned:?}");
        let estimates = with_history(blaze_graph::history::SCHEMA, b"blzest01");
        let (restored, unlearned) = history(&estimates, config).unwrap();
        assert_eq!(restored.encode(), empty());
        assert!(matches!(unlearned, Some(Unlearned::Refused(Refused::Schema { .. }))), "{unlearned:?}");
        let older = with_history(HISTORY, b"blzhis00 and what followed");
        let (restored, unlearned) = history(&older, config).unwrap();
        assert_eq!(restored.encode(), empty());
        assert_eq!(unlearned, Some(Unlearned::Version));
    }

    #[test]
    fn an_image_not_made_names_the_file_and_why() {
        let failed = ImageFailed {
            file: FileName::new("state/db/image").unwrap(),
            why: Unmade::Write(IoError("no space left on device".to_owned())),
        };
        let text = String::from(&failed);
        assert!(text.contains("writing the daemon image /state/db/image"), "{text}");
        assert!(text.contains("no space left"), "{text}");
    }

    #[test]
    fn a_history_that_does_not_decode_stops_the_daemon_naming_it() {
        let mut cut = empty();
        cut.truncate(cut.len() - 1);
        let Err(error) = history(&with_history(HISTORY, &cut), blaze_history::Config::default()) else {
            panic!("a cut history decoded")
        };
        assert!(matches!(error, HistoryError::Decode(_)), "{error:?}");
        assert!(error.to_string().contains("the plane's history"), "{error}");
    }
}
