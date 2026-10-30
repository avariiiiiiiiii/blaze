//! The toolchain's proc-macro server held against blaze's (docs/no-rustc.md, step J1): with
//! `BLAZE_PROC_MACRO_COMPARE` naming the server built from `vendor/proc-macro-srv` and
//! `BLAZE_PROC_MACRO_LIBS` a directory of the libraries blaze linked (named as rustc's, or
//! `lib<crate>` and the platform's suffix, with the list of their macros in them), each call the toolchain's server
//! expands with rustc's library of a crate is expanded again by that server with blaze's
//! library of the crate, given the same input, and the outputs, spans and all, are compared.

use std::{
    cell::Cell,
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use base_db::Db;
use hir_expand::proc_macro::{ProcMacro, ProcMacroCall, ProcMacroKind};
use paths::{AbsPath, AbsPathBuf};
use proc_macro_api::ProcMacroClient;
use rustc_hash::FxHashMap;

thread_local! {
    /// Set while blaze's server expands a call again, whose expander is wrapped as every
    /// other is: that expansion is neither kept nor compared.
    static COMPARING: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is expanding a call again for a comparison.
pub(crate) fn comparing() -> bool {
    COMPARING.with(Cell::get)
}

/// Whether this process compares the servers: then nothing is kept, so that each call is
/// expanded by both.
pub(crate) fn enabled() -> bool {
    std::env::var_os("BLAZE_PROC_MACRO_COMPARE").is_some()
}

struct Compare {
    server: ProcMacroClient,
    libs: PathBuf,
    /// The macros of blaze's library of each of rustc's, by the path of rustc's, or why
    /// there are none.
    loaded: Mutex<FxHashMap<AbsPathBuf, Result<Vec<ProcMacro>, String>>>,
}

fn compare() -> Option<&'static Compare> {
    static COMPARE: OnceLock<Option<Compare>> = OnceLock::new();
    COMPARE
        .get_or_init(|| {
            let server = std::env::var_os("BLAZE_PROC_MACRO_COMPARE")?;
            let libs = std::env::var_os("BLAZE_PROC_MACRO_LIBS")?;
            let server = AbsPathBuf::assert_utf8(std::path::absolute(server).ok()?);
            let none: [(&str, &Option<&str>); 0] = [];
            let server = match ProcMacroClient::spawn(&server, none, None, 1) {
                Ok(it) => it,
                Err(e) => {
                    eprintln!("proc macros: blaze's server {server} did not start: {e}");
                    return None;
                }
            };
            Some(Compare { server, libs: libs.into(), loaded: Mutex::default() })
        })
        .as_ref()
}

/// The crate rustc's library at `path` is of: cargo names it `lib<crate>-<hash>` and the
/// platform's suffix.
fn crate_of(path: &AbsPath) -> Option<&str> {
    let stem = path.file_stem()?;
    let stem = stem.strip_prefix("lib").unwrap_or(stem);
    Some(stem.rsplit_once('-').map_or(stem, |(name, _)| name))
}

impl Compare {
    /// blaze's library of rustc's at `dylib`, loaded into blaze's server: the one of the
    /// same file name, as blaze links it into a target directory where cargo asked rustc for
    /// it, which tells two versions of a crate apart, or else the one of its crate.
    fn macro_of(&self, dylib: &AbsPath, name: &str) -> Result<ProcMacro, String> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|it| it.into_inner());
        if !loaded.contains_key(dylib) {
            let macros = self.load(dylib);
            loaded.insert(dylib.to_path_buf(), macros); // alloc: setup: once per library
        }
        let macros = loaded[dylib].as_ref().map_err(Clone::clone)?;
        let found = macros.iter().find(|it| it.name.as_str() == name);
        found.cloned().ok_or_else(|| format!("blaze's library of {dylib} lists no `{name}`")) // alloc: error path
    }

    /// The macros of blaze's library of rustc's at `dylib`, loaded: once per library.
    fn load(&self, dylib: &AbsPath) -> Result<Vec<ProcMacro>, String> {
        let krate = crate_of(dylib).ok_or_else(|| format!("{dylib} names no crate"))?; // alloc: error path
        let file = format!("lib{krate}{}", std::env::consts::DLL_SUFFIX); // alloc: setup: once per library
        let same = dylib.file_name().map(|it| self.libs.join(it)).filter(|it| it.exists());
        let path = same.unwrap_or_else(|| self.libs.join(file));
        let path = std::path::absolute(path).map_err(|e| e.to_string())?; // alloc: error path
        if !path.exists() {
            return Err(format!("blaze linked no {}", path.display())); // alloc: error path
        }
        let path = AbsPathBuf::assert_utf8(path);
        load_cargo::load_proc_macro(&self.server, &path, &[]).map_err(|e| e.to_string()) // alloc: error path
    }
}

/// Expands the call again with blaze's server and library when asked to, and counts whether
/// it gave what the toolchain's did.
pub(crate) fn again(
    dylib: &AbsPath,
    name: &str,
    kind: ProcMacroKind,
    db: &Db,
    call: ProcMacroCall<'_>,
    toolchain: &Result<tt::TopSubtree, hir_expand::proc_macro::ProcMacroExpansionError>,
) {
    let Some(compare) = compare() else { return };
    let mac = match compare.macro_of(dylib, name) {
        Ok(it) => it,
        Err(why) => {
            COUNTS.unloaded.fetch_add(1, Ordering::Relaxed);
            noted(dylib, name, NOT_COMPARED, &format!("not compared: {why}")); // alloc: error path
            return;
        }
    };
    COMPARING.with(|it| it.set(true));
    let ours = mac.expander.expand(db, call);
    COMPARING.with(|it| it.set(false));
    COUNTS.compared[crate::kind_byte(kind) as usize].fetch_add(1, Ordering::Relaxed);
    let same = match (toolchain, &ours) {
        (Ok(theirs), Ok(ours)) => theirs == ours,
        (Err(theirs), Err(ours)) => {
            COUNTS.failed.fetch_add(1, Ordering::Relaxed);
            format!("{theirs:?}") == format!("{ours:?}") // alloc: error path: both servers failed
        }
        _ => false,
    };
    if !same {
        COUNTS.differed.fetch_add(1, Ordering::Relaxed);
        let said = |it: &Result<tt::TopSubtree, _>| match it {
            Ok(tree) => format!("{tree:?}"), // alloc: error path: the servers differ
            Err(e) => format!("error: {e:?}"), // alloc: error path: the servers differ
        };
        noted(
            dylib,
            name,
            OTHERWISE,
            // alloc: error path: the servers differ
            &format!(
                "blaze's server and library expanded otherwise than the toolchain's:\n\
                 toolchain's: {}\nblaze's:     {}",
                said(toolchain),
                said(&ours)
            ),
        );
    }
}

const OTHERWISE: usize = 0;
const NOT_COMPARED: usize = 1;

/// The calls of each macro, `crate::name`, expanded otherwise and not compared.
static NAMED: Mutex<BTreeMap<String, [u64; 2]>> = Mutex::new(BTreeMap::new());

/// Counts a call of the macro `name` of the library `dylib` as `which`, and says `what` of
/// the first.
fn noted(dylib: &AbsPath, name: &str, which: usize, what: &str) {
    let krate = crate_of(dylib).unwrap_or("?");
    let mut named = NAMED.lock().unwrap_or_else(|it| it.into_inner());
    let calls = named.entry(format!("{krate}::{name}")).or_default(); // alloc: error path: a call that differed or was not compared
    calls[which] += 1;
    if calls[which] == 1 {
        eprintln!("proc macros: `{krate}::{name}`: {what}");
    }
}

struct Counts {
    compared: [AtomicU64; 3],
    differed: AtomicU64,
    /// Calls both servers failed to expand, the same way or not.
    failed: AtomicU64,
    unloaded: AtomicU64,
}

static COUNTS: Counts = Counts {
    compared: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
    differed: AtomicU64::new(0),
    failed: AtomicU64::new(0),
    unloaded: AtomicU64::new(0),
};

/// What the comparison found so far, for the report: `None` when there is none.
pub(crate) fn report() -> Option<String> {
    use std::fmt::Write as _;

    compare()?;
    let compared = COUNTS.compared.each_ref().map(|it| it.load(Ordering::Relaxed));
    let mut said = String::new(); // alloc: the edge: the daemon's log
    _ = write!(said, "compared with blaze's server: {} calls (", compared.iter().sum::<u64>());
    for (at, (what, calls)) in ["derives", "attributes", "function-like"].into_iter().zip(compared).enumerate() {
        _ = write!(said, "{}{calls} {what}", if at == 0 { "" } else { ", " });
    }
    _ = write!(
        said,
        "), {} expanded otherwise, {} failed by both, {} not compared",
        COUNTS.differed.load(Ordering::Relaxed),
        COUNTS.failed.load(Ordering::Relaxed),
        COUNTS.unloaded.load(Ordering::Relaxed),
    );
    let named = NAMED.lock().unwrap_or_else(|it| it.into_inner());
    for (which, what) in [(OTHERWISE, "expanded otherwise"), (NOT_COMPARED, "not compared")] {
        let mut macros = named.iter().filter(|(_, calls)| calls[which] != 0).peekable();
        if macros.peek().is_none() {
            continue;
        }
        _ = write!(said, " ({what}: ");
        for (at, (label, calls)) in macros.enumerate() {
            _ = write!(said, "{}{label} {}", if at == 0 { "" } else { ", " }, calls[which]);
        }
        said.push(')');
    }
    Some(said)
}
