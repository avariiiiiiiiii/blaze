//! What a worker or checker of the daemon says of the build it does, as it does it: the
//! records of [`blaze_protocol::events`], in the file beside the process's socket that the
//! daemon follows ([`blaze_protocol::follow`]). A load, which comes before the process
//! listens, is in it from its first step.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub(crate) use blaze_protocol::events::Step;
use blaze_protocol::events::{Cause, CrateState, Event, Fns, Record};
use rustc_hash::FxHashMap;

/// The file this process says what it does in, once it has a socket.
static OUT: Mutex<Option<File>> = Mutex::new(None);

/// Has this process say what it does beside `socket` from now on.
pub(crate) fn said_beside(socket: &Path) {
    let path = blaze_protocol::follow::beside(socket, std::process::id());
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    if let Ok(file) = crate::socket::private_file(&mut options, &path) {
        *crate::lock(&OUT) = Some(file);
    }
}

/// Says `event`. A process with no socket says nothing.
pub(crate) fn say(event: Event) {
    let mut out = crate::lock(&OUT);
    let Some(file) = out.as_mut() else { return };
    let record = Record { at_ns: blaze_protocol::follow::now_ns(), event };
    // One write of the whole frame: the daemon reads the file as it grows.
    if let Ok(item) = blaze_protocol::cbor::to_vec(&record) {
        let _ = blaze_protocol::write_frame(file, &item);
    }
}

/// The step the process is at, which the next one ends.
static AT: Mutex<Option<(Step, Option<String>)>> = Mutex::new(None);

/// Says the process is at `step`, of `of` when it is of one crate or program, and done
/// with the one before.
pub(crate) fn at(step: Step, of: Option<&str>) {
    let of = of.map(str::to_owned);
    if let Some((step, of)) = crate::lock(&AT).replace((step, of.clone())) {
        say(Event::Ended { step, of });
    }
    say(Event::Started { step, of });
}

/// Says the process is done with its step: it waits to be asked.
pub(crate) fn idle() {
    if let Some((step, of)) = crate::lock(&AT).take() {
        say(Event::Ended { step, of });
    }
}

/// How often a compile says how many functions it has compiled.
const COUNTED: Duration = Duration::from_millis(100);

/// The compile of the functions of `krate`, from now until this is dropped: a
/// [`Step::Codegen`], and every [`COUNTED`] the functions compiled so far.
pub(crate) struct Compiling {
    counting: Option<(std::sync::Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

pub(crate) fn compiling(krate: &str) -> Compiling {
    at(Step::Codegen, Some(krate));
    if crate::lock(&OUT).is_none() {
        return Compiling { counting: None };
    }
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let counting = {
        let (done, name) = (done.clone(), krate.to_owned());
        let from = hir_ty_codegen::compiled_so_far();
        std::thread::spawn(move || {
            let mut said = 0;
            while !done.load(Ordering::Relaxed) {
                std::thread::park_timeout(COUNTED);
                let compiled = (hir_ty_codegen::compiled_so_far() - from) as u64;
                if compiled != said {
                    said = compiled;
                    let fns = Fns { total: 0, done: compiled, kept: 0 };
                    say(Event::Crate { name: name.clone(), state: CrateState::Running, fns });
                }
            }
        })
    };
    Compiling { counting: Some((done, counting)) }
}

impl Drop for Compiling {
    fn drop(&mut self) {
        if let Some((done, counting)) = self.counting.take() {
            done.store(true, Ordering::Relaxed);
            counting.thread().unpark();
            let _ = counting.join();
        }
    }
}

/// Says what the build of `krate` came to, by the codegen's count, and of a build of an
/// edit, of the files `edited`, which functions it compiled again and why.
pub(crate) fn built(
    db: &dyn hir::db::HirDatabase,
    krate: &str,
    stats: &hir_ty_codegen::BuildStats,
    edited: Option<&FxHashMap<vfs::FileId, PathBuf>>,
) {
    let (recompiled, all) = hir_ty_codegen::take_recompiled(db);
    if let Some(edited) = edited {
        let count = (all - recompiled.len()) as u64;
        for it in recompiled {
            let cause = match edited.get(&it.file) {
                Some(file) => Cause::Edited { file: file.clone() },
                None => Cause::Uses,
            };
            say(Event::Rebuilt { name: it.name, krate: it.krate, cause });
        }
        if count > 0 {
            say(Event::Unlisted { count });
        }
    }
    let (bodies, reused) = (stats.bodies as u64, stats.reused as u64);
    let fns = Fns { total: bodies, done: bodies, kept: reused };
    let state = if stats.compiled == 0 { CrateState::UpToDate } else { CrateState::Built };
    say(Event::Crate { name: krate.to_owned(), state, fns });
}
