//! Watching a tree on macOS with FSEvents: one stream per watch, file-level events with no
//! latency, delivered on a serial dispatch queue of the stream's own and handed on as
//! [`Done::Changed`].
//!
//! A flush is a sync cookie, as watchman's: each stream also watches a directory of cookies of
//! its own, a flush writes a file there, and the flush is told once the stream hears of that
//! file. fseventsd reads the kernel's one queue of events in order and hands a stream its
//! events in that order, so by then every change made before the cookie has been heard.
//! `FSEventStreamFlushSync` is no barrier: it flushes what fseventsd holds back for latency,
//! not what the kernel queued that fseventsd has not read yet, and it blocks the caller.
//!
//! A journal ([`Stream::journal`]) is a stream started `sinceWhen` a saved event ID, which
//! fseventsd answers from its store of the volume's events before the live ones, ending the
//! history with `kFSEventStreamEventFlagHistoryDone`. Event IDs are the system's, in the order
//! events happened; a saved ID means something only while the volume's store has the UUID it
//! was saved with (`FSEventsCopyUUIDForDevice`) and the system's IDs have not passed below it
//! (wrapped).

use std::{
    ffi::{CStr, CString, c_char, c_void},
    io,
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::{Completion, Cursor, Done, FileName, Journaled, Lost, Tag};

type CFIndex = isize;
type CFRef = *const c_void;
type FSEventStreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

type Callback = extern "C" fn(
    stream: CFRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
);

#[repr(C)]
struct Context {
    version: CFIndex,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    describe: Option<extern "C" fn(*const c_void) -> CFRef>,
}

#[repr(C)]
struct CFArrayCallBacks {
    _opaque: [u8; 0],
}

/// CFUUID.h's `CFUUIDBytes`: sixteen `UInt8`s, returned by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes {
    bytes: [u8; 16],
}

const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;
const CREATE_FILE_EVENTS: u32 = 0x10;
const EVENT_MUST_SCAN_SUB_DIRS: u32 = 0x01;
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_KERNEL_DROPPED: u32 = 0x04;
const EVENT_IDS_WRAPPED: u32 = 0x08;
const EVENT_HISTORY_DONE: u32 = 0x10;
const EVENT_ROOT_CHANGED: u32 = 0x20;
const EVENT_MOUNT: u32 = 0x40;
const EVENT_UNMOUNT: u32 = 0x80;
/// Events after which the names given are not all that changed.
const LOSES: u32 = EVENT_MUST_SCAN_SUB_DIRS
    | EVENT_USER_DROPPED
    | EVENT_KERNEL_DROPPED
    | EVENT_ROOT_CHANGED
    | EVENT_MOUNT
    | EVENT_UNMOUNT;

// SAFETY: these signatures match CoreFoundation's CFString.h, CFArray.h and CFUUID.h (`CFRef`
// is `CFTypeRef` and `CFUUIDRef`, `CFIndex` is `isize`, `CFUUIDBytes` is laid out as its
// sixteen `UInt8` fields); a mismatch would be UB at every call.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFStringCreateWithFileSystemRepresentation(allocator: CFRef, path: *const c_char) -> CFRef;
    fn CFArrayCreate(
        allocator: CFRef,
        values: *const CFRef,
        count: CFIndex,
        callbacks: *const CFArrayCallBacks,
    ) -> CFRef;
    fn CFUUIDGetUUIDBytes(uuid: CFRef) -> CFUUIDBytes;
    fn CFRelease(object: CFRef);
}

// SAFETY: these signatures match FSEvents.h (`FSEventStreamEventId` is `UInt64`,
// `FSEventStreamCreateFlags` is `UInt32`, `dev_t` is `__darwin_dev_t`, an `int32_t`).
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFRef,
        callback: Callback,
        context: *const Context,
        paths: CFRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> CFRef;
}

// libdispatch, in libSystem.
// SAFETY: these signatures match dispatch/queue.h and dispatch/object.h.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attributes: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(queue: DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

/// What a stream tells of what it hears.
enum Mode {
    /// [`Done::Changed`], from when it started.
    Watch,
    /// [`Done::Journaled`], from a place in the volume's store.
    Journal(Journal),
}

/// A journal's place, read and moved on the stream's queue only.
struct Journal {
    /// The UUID of the watched directory's volume's store.
    volume: [u8; 16],
    /// The highest event ID told.
    last: AtomicU64,
    /// Whether `kFSEventStreamEventFlagHistoryDone` has been heard.
    caught_up: AtomicBool,
}

/// What the callback reads, on the stream's queue.
struct Shared {
    tag: Tag,
    dir: FileName,
    /// The root with its symlinks resolved, as FSEvents names paths.
    root: PathBuf,
    /// The watched directory with its symlinks resolved, where it is one of its own.
    real_dir: PathBuf,
    /// This stream's cookies, symlinks resolved: watched with `real_dir`, named by the tag of
    /// the flush that wrote each.
    cookies: PathBuf,
    /// Each flush whose cookie the stream has not heard of yet.
    flushes: Mutex<Vec<Tag>>,
    /// `cookies`, kept to name a flush's cookie in without allocating ([`with_cookie`]).
    writing: Mutex<PathBuf>,
    /// The callback's buffers, kept for the stream's life: on its queue only, so never
    /// waited for.
    heard: Mutex<Heard>,
    send: Box<dyn Fn(Completion) + Send + Sync>,
    stopped: AtomicBool,
    mode: Mode,
}

/// The callback's buffers, grown once to their most and reused, so a batch that holds a
/// cookie allocates nothing (docs/v2/plan/design/memory.md: scratch reused per worker).
struct Heard {
    /// The cookies a batch named.
    cookies: Vec<u64>,
    /// The flushes those cookies tell.
    told: Vec<Tag>,
    /// `cookies`, to name a cookie in for its removal.
    path: PathBuf,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `n` in decimal in `digits`: a cookie's name, with no allocation.
fn decimal(mut n: u64, digits: &mut [u8; 20]) -> &[u8] {
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &digits[at..];
        }
    }
}

/// `dir` in a buffer with room for a cookie's name after it, for [`with_cookie`].
fn cookie_buffer(dir: &Path) -> PathBuf {
    // alloc: setup, once per stream.
    let mut buffer = PathBuf::with_capacity(dir.as_os_str().len() + 21);
    buffer.push(dir);
    buffer
}

/// Runs `f` on the path of the cookie of the flush `tag`, made in `dir`, the cookies'
/// directory: its name is pushed and popped again, so a buffer grown once allocates no more.
/// std names a path this short to the system from the stack.
fn with_cookie<R>(dir: &mut PathBuf, tag: Tag, f: impl FnOnce(&Path) -> R) -> R {
    let mut digits = [0; 20];
    dir.push(std::ffi::OsStr::from_bytes(decimal(tag.0, &mut digits)));
    let result = f(dir);
    dir.pop();
    result
}

/// Numbers each stream's directory of cookies in the process's.
static STREAMS: AtomicU64 = AtomicU64::new(0);

/// A directory of cookies for one stream, made empty: under the temporary directory, which
/// is the process's to write wherever the watched directory is.
fn cookies() -> io::Result<PathBuf> {
    let number = STREAMS.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        // alloc: setup, once per stream.
        .join(format!("blaze-fsevents-{}", std::process::id()))
        .join(number.to_string()); // alloc: setup, once per stream.
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::canonicalize(&dir)
}

/// Where a watch of `dir` under `root` looks, as FSEvents names paths.
struct Paths {
    root: PathBuf,
    /// The watched directory as the root names it, for errors.
    logical_dir: PathBuf,
    real_dir: PathBuf,
}

impl Paths {
    fn of(root: &Path, dir: &FileName) -> io::Result<Paths> {
        let real_root = std::fs::canonicalize(root).map_err(|error| {
            io::Error::new(error.kind(), format!("watching under {}: {error}", root.display()))
        })?;
        let logical_dir = real_root.join(dir.as_str());
        let real_dir = std::fs::canonicalize(&logical_dir).unwrap_or_else(|_| logical_dir.clone());
        Ok(Paths { root: real_root, logical_dir, real_dir })
    }
}

/// The UUID of the FSEvents store of the volume `path` is on: of its nearest ancestor that is
/// there, for a directory not made yet. A volume with no store (one fseventsd does not keep)
/// fails, naming the device.
fn store_of(path: &Path) -> io::Result<[u8; 16]> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = path
        .ancestors()
        .find_map(|it| std::fs::metadata(it).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "none of its directories is there"))?;
    // `st_dev` is an `int32_t` that `dev()` widened; narrowing gives it back.
    let device = meta.dev() as libc::dev_t;
    // SAFETY: no pointers; the result is a CFUUID retained for the caller, or null.
    let uuid = unsafe { FSEventsCopyUUIDForDevice(device) };
    if uuid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("the volume of device {device} keeps no FSEvents store, so no journal of it"),
        ));
    }
    // SAFETY: a CFUUID, alive until it is released below.
    let bytes = unsafe { CFUUIDGetUUIDBytes(uuid) };
    // SAFETY: retained by the copy above for this function, released once, not used after.
    unsafe { CFRelease(uuid) };
    Ok(bytes.bytes)
}

impl Shared {
    /// The name under the watched directory of a path FSEvents gave: `None` for one outside
    /// it, `Some(None)` for one under it that no [`FileName`] can name. `buffer` is room to
    /// write the name in, reused across the paths of a batch.
    fn name(&self, path: &Path, buffer: &mut String) -> Option<Option<FileName>> {
        if let Ok(rest) = path.strip_prefix(&self.real_dir) {
            return Some(match rest.as_os_str().is_empty() {
                true => Some(self.dir.clone()),
                false => rest.to_str().and_then(|rest| {
                    buffer.clear();
                    buffer.push_str(self.dir.as_str());
                    buffer.push('/');
                    buffer.push_str(rest);
                    FileName::new(buffer.as_str())
                }),
            });
        }
        let rest = path.strip_prefix(&self.root).ok()?;
        let Some(rest) = rest.to_str() else { return Some(None) };
        let name = FileName::new(rest)?;
        name.is_under(&self.dir).then_some(Some(name))
    }

    /// The cookie number of a path in this stream's cookies: `None` for one outside them,
    /// `Some(None)` for the directory itself or a name that is no cookie's.
    fn cookie(&self, path: &Path) -> Option<Option<u64>> {
        let rest = path.strip_prefix(&self.cookies).ok()?;
        Some(rest.to_str().and_then(|it| it.parse::<u64>().ok()))
    }
}

pub(crate) struct Stream {
    stream: FSEventStreamRef,
    queue: DispatchQueue,
    shared: *mut Shared,
}

// SAFETY: the stream and its queue are FSEvents' and libdispatch's objects, which may be stopped
// and released from any thread; `Shared` is read on the queue only, and freed once the queue has
// run its last callback.
unsafe impl Send for Stream {}

impl Stream {
    /// Watches `dir` under `root` (kFSEventStreamCreateFlagFileEvents | NoDefer |
    /// WatchRoot, latency 0, on a dispatch queue of its own), calling `send` with a
    /// `Completion { tag, done: Done::Changed { files, lost } }` for each batch, names
    /// relative to `root` as `FileName`s, each once; kFSEventStreamEventFlagMustScanSubDirs,
    /// UserDropped or KernelDropped make `lost: true`.
    pub(crate) fn start(
        root: &Path,
        dir: &FileName,
        tag: Tag,
        send: Box<dyn Fn(Completion) + Send + Sync>,
    ) -> io::Result<Stream> {
        let paths = Paths::of(root, dir)?;
        Stream::open(paths, dir, tag, send, SINCE_NOW, Mode::Watch, None)
    }

    /// The journal of `dir` under `root` from `since` ([`crate::Effect::Journal`]): a stream as
    /// [`Stream::start`]'s, calling `send` with `Done::Journaled` batches. From a cursor of
    /// this volume's store at or before the system's current event ID it starts `sinceWhen`
    /// that ID, its history told with `caught_up: false` until HistoryDone; from anything else
    /// it starts at the current ID, read before the stream is made so no event falls between,
    /// with a first batch sent before any other: empty, at that ID, and naming `dir` lost as
    /// [`Lost::Reset`] unless `since` was [`Cursor::None`].
    pub(crate) fn journal(
        root: &Path,
        dir: &FileName,
        since: Cursor,
        tag: Tag,
        send: Box<dyn Fn(Completion) + Send + Sync>,
    ) -> io::Result<Stream> {
        let paths = Paths::of(root, dir)?;
        let volume = store_of(&paths.real_dir).map_err(|error| {
            let at = paths.logical_dir.display();
            io::Error::new(error.kind(), format!("the journal of {at}: {error}"))
        })?;
        // SAFETY: no arguments; reads the system's last event ID.
        let now = unsafe { FSEventsGetCurrentEventId() };
        let resumed = match since {
            Cursor::FsEvents { event, volume: of } if of == volume && event <= now => Some(event),
            _ => None,
        };
        let first = match (resumed, since) {
            (Some(_), _) => None,
            (None, Cursor::None) => Some(Vec::new()),
            (None, _) => Some(vec![(dir.clone(), Lost::Reset)]),
        };
        let first = first.map(|lost| Journaled {
            files: Vec::new(),
            lost,
            cursor: Cursor::FsEvents { event: now, volume },
            caught_up: false,
        });
        let from = resumed.unwrap_or(now);
        let journal =
            Journal { volume, last: AtomicU64::new(from), caught_up: AtomicBool::new(false) };
        Stream::open(paths, dir, tag, send, from, Mode::Journal(journal), first)
    }

    /// The stream of `paths.real_dir` and a directory of cookies from `since`, telling `first`
    /// before anything it hears.
    fn open(
        paths: Paths,
        dir: &FileName,
        tag: Tag,
        send: Box<dyn Fn(Completion) + Send + Sync>,
        since: u64,
        mode: Mode,
        first: Option<Journaled>,
    ) -> io::Result<Stream> {
        let Paths { root, logical_dir, real_dir } = paths;
        let c_path = |path: &Path| {
            CString::new(path.as_os_str().to_owned().into_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))
        };
        let path = c_path(&real_dir)?;
        let cookies = cookies().map_err(|error| {
            // alloc: an error path.
            io::Error::new(error.kind(), format!("making the cookies of a watch: {error}"))
        })?;
        let cookie_path = c_path(&cookies)?;
        let shared = Box::into_raw(Box::new(Shared {
            tag,
            dir: dir.clone(),
            root,
            real_dir,
            writing: Mutex::new(cookie_buffer(&cookies)),
            // alloc: setup, once per stream: the callback's buffers at the size a batch of a
            // few cookies needs.
            heard: Mutex::new(Heard {
                cookies: Vec::with_capacity(4),
                told: Vec::with_capacity(4),
                path: cookie_buffer(&cookies),
            }),
            cookies,
            flushes: Mutex::new(Vec::with_capacity(4)), // alloc: setup, once per stream.
            send,
            stopped: AtomicBool::new(false),
            mode,
        }));
        let failed = |why: String| {
            // SAFETY: made above and given to no stream that runs.
            let shared = unsafe { Box::from_raw(shared) };
            _ = std::fs::remove_dir_all(&shared.cookies);
            io::Error::other(format!("watching {}: {why}", logical_dir.display()))
        };
        // SAFETY: NUL-terminated paths; the strings are released below.
        let strings = [path.as_ptr(), cookie_path.as_ptr()]
            .map(|it| unsafe { CFStringCreateWithFileSystemRepresentation(std::ptr::null(), it) });
        let release = || {
            for string in strings.into_iter().filter(|it| !it.is_null()) {
                // SAFETY: made above, released once; an array holds its own reference.
                unsafe { CFRelease(string) };
            }
        };
        if strings.iter().any(|it| it.is_null()) {
            release();
            return Err(failed("CoreFoundation made no string of the path".into()));
        }
        // SAFETY: two CFStrings, retained by the array with the callbacks of CF types.
        let paths = unsafe {
            CFArrayCreate(std::ptr::null(), strings.as_ptr(), 2, &raw const kCFTypeArrayCallBacks)
        };
        release();
        if paths.is_null() {
            return Err(failed("CoreFoundation made no array of the path".into()));
        }
        let context = Context {
            version: 0,
            info: shared.cast(),
            retain: None,
            release: None,
            describe: None,
        };
        // SAFETY: the context is copied by the call; `shared` lives until the stream's queue
        // has run its last callback (`Drop`).
        let stream = unsafe {
            FSEventStreamCreate(
                std::ptr::null(),
                heard,
                &context,
                paths,
                since,
                0.0,
                CREATE_FILE_EVENTS | CREATE_NO_DEFER | CREATE_WATCH_ROOT,
            )
        };
        // SAFETY: the stream holds its own reference to the paths.
        unsafe { CFRelease(paths) };
        if stream.is_null() {
            return Err(failed("FSEventStreamCreate made no stream".into()));
        }
        // SAFETY: a NUL-terminated label; no attributes is a serial queue.
        let queue = unsafe { dispatch_queue_create(c"blaze-fsevents".as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            // SAFETY: the stream made above, never started.
            unsafe { FSEventStreamRelease(stream) };
            return Err(failed("libdispatch made no queue".into()));
        }
        // SAFETY: a stream not yet started and a queue of its own.
        unsafe { FSEventStreamSetDispatchQueue(stream, queue) };
        if let Some(first) = first {
            // SAFETY: made above; no callback reads it before the stream starts below.
            let shared = unsafe { &*shared };
            (shared.send)(Completion { tag, done: Done::Journaled(Box::new(first)) });
        }
        // SAFETY: the stream is scheduled on its queue.
        if unsafe { FSEventStreamStart(stream) } == 0 {
            // SAFETY: the stream did not start, so no callback runs during or after this.
            unsafe { FSEventStreamInvalidate(stream) };
            // SAFETY: the stream made above, released once and not used after.
            unsafe { FSEventStreamRelease(stream) };
            // SAFETY: the queue made above, released once and not used after.
            unsafe { dispatch_release(queue) };
            return Err(failed("FSEventStreamStart refused it".into()));
        }
        Ok(Stream { stream, queue, shared })
    }

    /// Stops and releases it; no batch is sent once this returns, and each flush not yet told
    /// is told failed. The caller sends the watch's `Done::Unwatched` after.
    pub(crate) fn stop(self) {
        drop(self);
    }

    /// Asks for a flush `tag`: its cookie is written, and once the stream hears of it
    /// `Completion { tag, done: Done::Flushed }` is sent, after every batch of changes made
    /// before (for a journal, after its history too). An error says the cookie could not be
    /// written, and nothing is sent.
    pub(crate) fn flush(&self, tag: Tag) -> io::Result<()> {
        // SAFETY: `shared` lives until this stream is dropped, which `&self` outlives.
        let shared = unsafe { &*self.shared };
        lock(&shared.flushes).push(tag);
        let written = with_cookie(&mut lock(&shared.writing), tag, |it| std::fs::write(it, b""));
        match written {
            Ok(()) => Ok(()),
            Err(error) => {
                // A part made may have been heard, and the flush told already.
                let mut flushes = lock(&shared.flushes);
                match flushes.iter().position(|it| *it == tag) {
                    Some(at) => {
                        flushes.swap_remove(at);
                        Err(error)
                    }
                    None => Ok(()),
                }
            }
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: `shared` lives until the end of this function.
        unsafe { (*self.shared).stopped.store(true, Ordering::Release) };
        let mut stream = self.stream;
        // SAFETY: the stream is stopped and invalidated on its own queue, so no callback runs
        // during it; `stream` outlives the call, which returns once the work has run.
        unsafe { dispatch_sync_f(self.queue, (&raw mut stream).cast(), end_on_queue) };
        // SAFETY: a serial queue: this runs after any callback queued before the invalidation.
        unsafe { dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing) };
        // SAFETY: invalidated, with no callback left to run; released once, not used after.
        unsafe { FSEventStreamRelease(self.stream) };
        // SAFETY: the queue has run its last work; released once, not used after.
        unsafe { dispatch_release(self.queue) };
        // SAFETY: made by `Box::into_raw` in `open`; no callback reads it any more.
        let shared = unsafe { Box::from_raw(self.shared) };
        for tag in std::mem::take(&mut *lock(&shared.flushes)) {
            let why = crate::IoError("the watch ended before the flush".into());
            (shared.send)(Completion { tag, done: Done::Failed(why) });
        }
        _ = std::fs::remove_dir_all(&shared.cookies);
    }
}

extern "C" fn end_on_queue(stream: *mut c_void) {
    // SAFETY: `Drop` passes a pointer to its copy of the stream, alive for this call.
    let stream = unsafe { *stream.cast::<FSEventStreamRef>() };
    // SAFETY: a stream started and not yet released, stopped on its own queue.
    unsafe { FSEventStreamStop(stream) };
    // SAFETY: as above; no callback is scheduled once this returns.
    unsafe { FSEventStreamInvalidate(stream) };
}

extern "C" fn nothing(_: *mut c_void) {}

/// A path FSEvents passed.
///
/// # Safety
///
/// `path` is a NUL-terminated string alive for `'a`.
unsafe fn path_of<'a>(path: *const c_char) -> &'a Path {
    // SAFETY: the caller's.
    let path = unsafe { CStr::from_ptr(path) };
    Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()))
}

extern "C" fn heard(
    _stream: CFRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    // SAFETY: the `Shared` of `Stream::open`, freed only after the queue's last callback.
    let shared = unsafe { &*info.cast::<Shared>() };
    if shared.stopped.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: FSEvents passes `count` flags, alive for the callback.
    let flags = unsafe { std::slice::from_raw_parts(flags, count) };
    // SAFETY: and `count` C strings, without kFSEventStreamCreateFlagUseCFTypes.
    let paths = unsafe { std::slice::from_raw_parts(paths.cast::<*const c_char>(), count) };
    // SAFETY: and `count` event IDs.
    let ids = unsafe { std::slice::from_raw_parts(ids, count) };
    let mut heard = lock(&shared.heard);
    let Heard { cookies, told, path } = &mut *heard;
    cookies.clear();
    match &shared.mode {
        Mode::Watch => changed(shared, flags, paths, cookies),
        Mode::Journal(journal) => journaled(shared, journal, flags, paths, ids, cookies),
    }
    if !cookies.is_empty() {
        tell_flushes(shared, cookies, told, path);
    }
}

/// A watch's batch, as [`Done::Changed`].
fn changed(shared: &Shared, flags: &[u32], paths: &[*const c_char], cookies: &mut Vec<u64>) {
    let mut files = Vec::with_capacity(flags.len());
    let mut buffer = String::new();
    let mut lost = false;
    for (&flag, &path) in flags.iter().zip(paths) {
        lost |= flag & LOSES != 0;
        // SAFETY: a path FSEvents passed, alive for the callback.
        let path = unsafe { path_of(path) };
        if let Some(cookie) = shared.cookie(path) {
            cookies.extend(cookie);
            continue;
        }
        match shared.name(path, &mut buffer) {
            Some(Some(name)) => files.push(name),
            Some(None) => lost = true,
            None => {}
        }
    }
    if !files.is_empty() || lost {
        files.sort_unstable();
        files.dedup();
        let done = Done::Changed { files, lost };
        (shared.send)(Completion { tag: shared.tag, done });
    }
}

/// `name` lost for `why`, once.
fn lose(lost: &mut Vec<(FileName, Lost)>, name: &FileName, why: Lost) {
    if !lost.iter().any(|(it, was)| it == name && *was == why) {
        lost.push((name.clone(), why));
    }
}

/// A journal's batch, as [`Done::Journaled`]: sent when it names anything, moves the cursor or
/// ends the history.
fn journaled(
    shared: &Shared,
    journal: &Journal,
    flags: &[u32],
    paths: &[*const c_char],
    ids: &[u64],
    cookies: &mut Vec<u64>,
) {
    let mut files = Vec::with_capacity(flags.len());
    let mut lost = Vec::new();
    let mut buffer = String::new();
    let before = journal.last.load(Ordering::Relaxed);
    let was_caught_up = journal.caught_up.load(Ordering::Relaxed);
    let (mut last, mut caught_up) = (before, was_caught_up);
    let dir = &shared.dir;
    for ((&flag, &path), &id) in flags.iter().zip(paths).zip(ids) {
        // Its path is to be ignored, and its ID is no event's of the watched paths.
        if flag & EVENT_HISTORY_DONE != 0 {
            caught_up = true;
            continue;
        }
        if flag & EVENT_IDS_WRAPPED != 0 {
            last = id;
            lose(&mut lost, dir, Lost::Reset);
            continue;
        }
        // RootChanged carries ID 0.
        last = last.max(id);
        if flag & (EVENT_USER_DROPPED | EVENT_KERNEL_DROPPED) != 0 {
            lose(&mut lost, dir, Lost::Dropped);
            continue;
        }
        // SAFETY: a path FSEvents passed, alive for the callback.
        let path = unsafe { path_of(path) };
        if let Some(cookie) = shared.cookie(path) {
            // A history may hold the cookies of an earlier stream that had this directory;
            // a flush is told only by one written once the history is told.
            if caught_up {
                cookies.extend(cookie);
            }
            continue;
        }
        if flag & (EVENT_ROOT_CHANGED | EVENT_MOUNT | EVENT_UNMOUNT) != 0 {
            lose(&mut lost, dir, Lost::RootChanged);
            continue;
        }
        let name = shared.name(path, &mut buffer);
        if flag & EVENT_MUST_SCAN_SUB_DIRS != 0 {
            let name = match name {
                Some(Some(name)) => name,
                Some(None) => dir.clone(),
                None if shared.real_dir.starts_with(path) => dir.clone(),
                None => continue,
            };
            lose(&mut lost, &name, Lost::Rescan);
            continue;
        }
        match name {
            Some(Some(name)) => files.push(name),
            Some(None) => lose(&mut lost, dir, Lost::Rescan),
            None => {}
        }
    }
    if files.is_empty() && lost.is_empty() && last == before && caught_up == was_caught_up {
        return;
    }
    files.sort_unstable();
    files.dedup();
    lost.sort_by(|a, b| a.0.cmp(&b.0));
    journal.last.store(last, Ordering::Relaxed);
    journal.caught_up.store(caught_up, Ordering::Relaxed);
    let cursor = Cursor::FsEvents { event: last, volume: journal.volume };
    let done = Done::Journaled(Box::new(Journaled { files, lost, cursor, caught_up }));
    (shared.send)(Completion { tag: shared.tag, done });
}

/// Tells each flush whose cookie was heard, through the callback's buffers.
fn tell_flushes(shared: &Shared, cookies: &[u64], told: &mut Vec<Tag>, path: &mut PathBuf) {
    told.clear();
    lock(&shared.flushes).retain(|tag| {
        let heard = cookies.contains(&tag.0);
        if heard {
            told.push(*tag);
        }
        !heard
    });
    for &tag in told.iter() {
        // Its removal is heard too, of a flush told already: nothing.
        _ = with_cookie(path, tag, |it| std::fs::remove_file(it));
        (shared.send)(Completion { tag, done: Done::Flushed });
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::*;

    /// A root under the temporary directory, which is `/var/...` named and `/private/var/...`
    /// real, with `src` and `target` made in it.
    fn root(what: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("blaze-fsevents-{what}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        root
    }

    type Heard = mpsc::Receiver<Completion>;

    fn sender() -> (Box<dyn Fn(Completion) + Send + Sync>, Heard) {
        let (send, heard) = mpsc::channel();
        let send = std::sync::Mutex::new(send);
        (Box::new(move |completion| _ = send.lock().unwrap().send(completion)), heard)
    }

    fn journaled(completion: Completion, tag: Tag) -> Journaled {
        assert_eq!(completion.tag, tag);
        match completion.done {
            Done::Journaled(it) => *it,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_file_written_under_the_watched_directory_is_heard_and_one_outside_is_not() {
        let root = root("watch");
        let (send, heard) = sender();
        let dir = FileName::new("src").unwrap();
        let stream = Stream::start(&root, &dir, Tag(4), send).unwrap();
        let mut files = BTreeSet::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        let wanted = FileName::new("src/deep/lib.rs").unwrap();
        while !files.contains(&wanted) && Instant::now() < deadline {
            std::fs::write(root.join("target/out"), b"not watched").unwrap();
            std::fs::create_dir_all(root.join("src/deep")).unwrap();
            std::fs::write(root.join("src/deep/lib.rs"), b"fn f() {}").unwrap();
            while let Ok(completion) = heard.recv_timeout(Duration::from_millis(100)) {
                assert_eq!(completion.tag, Tag(4));
                let Done::Changed { files: batch, .. } = completion.done else {
                    panic!("{:?}", completion.done)
                };
                let unique: BTreeSet<_> = batch.iter().cloned().collect();
                assert_eq!(unique.len(), batch.len(), "a name twice in one batch: {batch:?}");
                files.extend(batch);
            }
        }
        stream.stop();
        assert!(files.contains(&wanted), "{files:?}");
        assert!(files.iter().all(|it| it.is_under(&dir)), "{files:?}");
        // Nothing is sent once it has stopped.
        std::fs::write(root.join("src/after"), b"late").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        while let Ok(completion) = heard.try_recv() {
            let Done::Changed { files, .. } = completion.done else { panic!() };
            assert!(!files.contains(&FileName::new("src/after").unwrap()), "{files:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Takes batches until one is caught up, or the deadline: every one of them.
    fn until_caught_up(heard: &Heard, tag: Tag) -> Vec<Journaled> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut batches = Vec::new();
        while Instant::now() < deadline {
            let Ok(completion) = heard.recv_timeout(Duration::from_millis(100)) else { continue };
            let batch = journaled(completion, tag);
            let caught_up = batch.caught_up;
            batches.push(batch);
            if caught_up {
                return batches;
            }
        }
        panic!("never caught up: {batches:?}")
    }

    fn event(cursor: Cursor) -> (u64, [u8; 16]) {
        match cursor {
            Cursor::FsEvents { event, volume } => (event, volume),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_journal_resumes_from_its_cursor_with_what_changed_while_it_was_stopped() {
        let root = root("journal");
        let dir = FileName::new("src").unwrap();
        let (send, heard) = sender();
        let stream = Stream::journal(&root, &dir, Cursor::None, Tag(1), send).unwrap();
        // The first batch is there at once, before any the stream hears.
        let first = journaled(heard.try_recv().expect("the first batch, at once"), Tag(1));
        assert_eq!((first.files.len(), first.lost.len(), first.caught_up), (0, 0, false));
        let (start, volume) = event(first.cursor);
        let mut last = until_caught_up(&heard, Tag(1)).last().unwrap().cursor;
        // A change heard live moves the cursor past it.
        let before = FileName::new("src/before.rs").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut heard_before = false;
        while !heard_before && Instant::now() < deadline {
            std::fs::write(root.join("src/before.rs"), b"fn f() {}").unwrap();
            while let Ok(completion) = heard.recv_timeout(Duration::from_millis(100)) {
                let batch = journaled(completion, Tag(1));
                assert!(batch.caught_up);
                assert!(batch.files.iter().all(|it| it.is_under(&dir)), "{batch:?}");
                assert!(event(batch.cursor).0 >= event(last).0);
                heard_before |= batch.files.contains(&before);
                last = batch.cursor;
            }
        }
        assert!(heard_before);
        assert!(event(last).0 > start);
        assert_eq!(event(last).1, volume);
        stream.stop();
        // Written while nothing listens, then given time to reach fseventsd's store.
        std::fs::create_dir_all(root.join("src/down")).unwrap();
        std::fs::write(root.join("src/down/lib.rs"), b"fn g() {}").unwrap();
        std::fs::write(root.join("target/out"), b"not watched").unwrap();
        std::thread::sleep(Duration::from_secs(1));
        let (send, heard) = sender();
        let stream = Stream::journal(&root, &dir, last, Tag(2), send).unwrap();
        let batches = until_caught_up(&heard, Tag(2));
        stream.stop();
        // The history, told up to and with the batch of HistoryDone, which may hold some of it.
        let (done, history) = batches.split_last().unwrap();
        assert!(done.caught_up && history.iter().all(|it| !it.caught_up), "{batches:?}");
        let down = FileName::new("src/down/lib.rs").unwrap();
        let told: BTreeSet<_> = batches.iter().flat_map(|it| it.files.iter().cloned()).collect();
        assert!(told.contains(&down), "{batches:?}");
        assert!(!told.contains(&before), "told again from before the cursor: {batches:?}");
        assert!(batches.iter().all(|it| it.lost.is_empty()), "{batches:?}");
        assert!(batches.iter().flat_map(|it| &it.files).all(|it| it.is_under(&dir)), "{batches:?}");
        assert!(batches.iter().all(|it| event(it.cursor).0 >= event(last).0));
        for batch in &batches {
            let mut sorted = batch.files.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted, batch.files);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_cursor_of_another_store_or_past_now_is_lost_as_reset() {
        let root = root("reset");
        let dir = FileName::new("src").unwrap();
        let (send, heard) = sender();
        let stream = Stream::journal(&root, &dir, Cursor::None, Tag(1), send).unwrap();
        let (now, volume) = event(journaled(heard.try_recv().unwrap(), Tag(1)).cursor);
        stream.stop();
        let cursors = [
            Cursor::FsEvents { event: 1, volume: [0xab; 16] },
            Cursor::FsEvents { event: u64::MAX - 1, volume },
            Cursor::Usn { volume: 1, journal: 2, next: 3 },
            Cursor::Sim { journal: 1, next: 0 },
        ];
        for (n, since) in cursors.into_iter().enumerate() {
            let tag = Tag(10 + n as u64);
            let (send, heard) = sender();
            let stream = Stream::journal(&root, &dir, since, tag, send).unwrap();
            let first = journaled(heard.try_recv().expect("the first batch, at once"), tag);
            stream.stop();
            assert_eq!(first.lost, [(dir.clone(), Lost::Reset)], "{since:?}");
            assert!(first.files.is_empty() && !first.caught_up);
            let (event, of) = event(first.cursor);
            assert!(event >= now && event < u64::MAX - 1, "{since:?}: {event}");
            assert_eq!(of, volume);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
