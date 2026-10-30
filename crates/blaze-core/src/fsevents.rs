//! Watching a tree on macOS with FSEvents: one stream per watch, file-level events with no
//! latency, delivered on a serial dispatch queue of the stream's own and handed on as
//! [`Done::Changed`].

use std::{
    collections::BTreeSet,
    ffi::{CStr, CString, c_char, c_void},
    io,
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{Completion, Done, FileName, Tag};

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

const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_WATCH_ROOT: u32 = 0x04;
const CREATE_FILE_EVENTS: u32 = 0x10;
const EVENT_MUST_SCAN_SUB_DIRS: u32 = 0x01;
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_KERNEL_DROPPED: u32 = 0x04;
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
    fn CFRelease(object: CFRef);
}

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
}

// libdispatch, in libSystem.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attributes: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(queue: DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: *mut c_void);
}

/// What the callback reads, on the stream's queue.
struct Shared {
    tag: Tag,
    dir: FileName,
    /// The root with its symlinks resolved, as FSEvents names paths.
    root: PathBuf,
    /// The watched directory with its symlinks resolved, where it is one of its own.
    real_dir: PathBuf,
    send: Box<dyn Fn(Completion) + Send + Sync>,
    stopped: AtomicBool,
}

impl Shared {
    /// The name under the watched directory of a path FSEvents gave: `None` for one outside
    /// it, `Some(None)` for one under it that no [`FileName`] can name.
    fn name(&self, path: &Path) -> Option<Option<FileName>> {
        if let Ok(rest) = path.strip_prefix(&self.real_dir) {
            return Some(match rest.as_os_str().is_empty() {
                true => Some(self.dir.clone()),
                false => {
                    rest.to_str().and_then(|rest| FileName::new(&format!("{}/{rest}", self.dir)))
                }
            });
        }
        let rest = path.strip_prefix(&self.root).ok()?;
        let Some(rest) = rest.to_str() else { return Some(None) };
        let name = FileName::new(rest)?;
        name.is_under(&self.dir).then_some(Some(name))
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
        let real_root = std::fs::canonicalize(root).map_err(|error| {
            io::Error::new(error.kind(), format!("watching under {}: {error}", root.display()))
        })?;
        let logical_dir = real_root.join(dir.as_str());
        let real_dir = std::fs::canonicalize(&logical_dir).unwrap_or_else(|_| logical_dir.clone());
        let path = CString::new(real_dir.clone().into_os_string().into_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
        let shared = Box::into_raw(Box::new(Shared {
            tag,
            dir: dir.clone(),
            root: real_root,
            real_dir,
            send,
            stopped: AtomicBool::new(false),
        }));
        let failed = |why: String| {
            // SAFETY: made above and given to no stream that runs.
            drop(unsafe { Box::from_raw(shared) });
            io::Error::other(format!("watching {}: {why}", logical_dir.display()))
        };
        // SAFETY: a NUL-terminated path; the string is released below.
        let string = unsafe { CFStringCreateWithFileSystemRepresentation(std::ptr::null(), path.as_ptr()) };
        if string.is_null() {
            return Err(failed("CoreFoundation made no string of the path".into()));
        }
        // SAFETY: one CFString, retained by the array with the callbacks of CF types.
        let paths =
            unsafe { CFArrayCreate(std::ptr::null(), &string, 1, &raw const kCFTypeArrayCallBacks) };
        // SAFETY: the array holds its own reference.
        unsafe { CFRelease(string) };
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
                SINCE_NOW,
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
        // SAFETY: the stream is scheduled on its queue.
        if unsafe { FSEventStreamStart(stream) } == 0 {
            // SAFETY: the stream did not start: no callback runs; both are released once.
            unsafe {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
            }
            return Err(failed("FSEventStreamStart refused it".into()));
        }
        Ok(Stream { stream, queue, shared })
    }

    /// Stops and releases it; no batch is sent once this returns. The caller sends the
    /// watch's `Done::Unwatched` after.
    pub(crate) fn stop(self) {
        drop(self);
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: `shared` lives until the end of this function.
        unsafe { (*self.shared).stopped.store(true, Ordering::Release) };
        let mut stream = self.stream;
        // SAFETY: the stream is stopped and invalidated on its own queue, so no callback runs
        // during it; the second sync runs after any callback queued before the invalidation.
        unsafe {
            dispatch_sync_f(self.queue, (&raw mut stream).cast(), end_on_queue);
            dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.shared));
        }
    }
}

extern "C" fn end_on_queue(stream: *mut c_void) {
    // SAFETY: the stream `Drop` passed, started and not yet released.
    unsafe {
        let stream = *stream.cast::<FSEventStreamRef>();
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
    }
}

extern "C" fn nothing(_: *mut c_void) {}

extern "C" fn heard(
    _stream: CFRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: the `Shared` of `Stream::start`, freed only after the queue's last callback.
    let shared = unsafe { &*info.cast::<Shared>() };
    if shared.stopped.load(Ordering::Acquire) {
        return;
    }
    let paths = paths.cast::<*const c_char>();
    let mut files = BTreeSet::new();
    let mut lost = false;
    for n in 0..count {
        // SAFETY: `count` flags and NUL-terminated paths, without kFSEventStreamCreateFlagUseCFTypes.
        let (flag, path) = unsafe { (*flags.add(n), CStr::from_ptr(*paths.add(n))) };
        lost |= flag & LOSES != 0;
        let path = Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
        match shared.name(path) {
            Some(Some(name)) => _ = files.insert(name),
            Some(None) => lost = true,
            None => {}
        }
    }
    if files.is_empty() && !lost {
        return;
    }
    let done = Done::Changed { files: files.into_iter().collect(), lost };
    (shared.send)(Completion { tag: shared.tag, done });
}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn a_file_written_under_the_watched_directory_is_heard_and_one_outside_is_not() {
        // Under the temporary directory, which is `/var/...` named and `/private/var/...` real.
        let root = std::env::temp_dir().join(format!("blaze-fsevents-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        let (send, heard) = mpsc::channel();
        let send = std::sync::Mutex::new(send);
        let dir = FileName::new("src").unwrap();
        let stream = Stream::start(
            &root,
            &dir,
            Tag(4),
            Box::new(move |completion| _ = send.lock().unwrap().send(completion)),
        )
        .unwrap();
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
}
