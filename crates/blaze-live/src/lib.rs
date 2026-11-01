//! Applies blaze's edits to a program while it runs.
//!
//! A program blaze-link linked calls every function a patch can replace through a slot of a
//! jump table, and [`Live::start`] readies it to take patches. The program runs from the
//! daemon's code memory (blaze-inproc), where each commit writes a patch's code; the program
//! asks the daemon for the patch's message over blaze-inproc's channel, writes its data, and
//! switches to it in one atomic store ([`slots`]). No thread is stopped and no code a thread
//! can be in is written over.
//!
//! [`Live::poll`] takes what came in since it was last called. Call it where nothing of the
//! program's is half done, such as between two frames: a function the calling thread is
//! running when its patch lands finishes as it was, and the next call runs the new code.
//!
//! Unsafe code is only in the modules that reach the kernel and the image's memory (`slots`'
//! platform, image and execution-log modules, and Windows' socket for [`edit`]), each allowing
//! it with its reason; the rest denies it. There is no safe version of them to pay for: std
//! maps no executable memory and gives no view of the image's words at the addresses the
//! linker placed them. A message is read in place by safe code only
//! ([`patch::Message::view`]), and the jump table's own words are `std` atomics at the
//! addresses the linker gave them.
//!
//! What a patch cannot do, it says, and the program has to be started again
//! ([`Outcome::Restart`]).
//!
//! Linux on x86-64 and AArch64, macOS on AArch64 and Windows on x86-64; elsewhere
//! [`Live::start`] says so.

#![deny(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The setting `BLAZE_<name>`.
fn setting(name: &str) -> Option<std::ffi::OsString> {
    // alloc: setup, read when the program asks where the daemon is
    std::env::var_os(format!("BLAZE_{name}"))
}

mod ask;
pub mod format;
pub mod patch;
pub mod slots;

#[cfg(all(windows, target_arch = "x86_64"))]
mod socket;

pub use slots::{Live, Outcome};

/// The counters that code the daemon compiled to count its branches adds to. That code finds
/// them by calling this, by name (`hir_ty_codegen::tier::COUNTERS`).
// SAFETY: the `blaze_live_` prefix is this crate's, so no other item of a program exports
// `blaze_live_counters`; a second one would fail the link or bind the daemon's code to the
// wrong counters.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn blaze_live_counters() -> *mut u64 {
    COUNTERS.as_ptr() as *mut u64
}

/// [`blaze_live_counters`]'s: an `AtomicU64` is a `u64` in memory. The daemon's code must add
/// to them atomically (relaxed is enough).
static COUNTERS: [std::sync::atomic::AtomicU64; format::COUNTERS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; format::COUNTERS];

/// What the daemon says of the tiers of this program: the functions it spends its time in, the
/// level each is compiled at, and what the daemon did of late. `None` while the daemon tiers
/// none of its functions; an error in a program the daemon did not start, or when the channel
/// to it fails. Each call asks the daemon over blaze-inproc's channel, so a program that shows
/// it asks a few times a second, not every frame.
pub fn tiers() -> Result<Option<format::Tiers>, String> {
    use patch::ask::{NO_TIERS, TIERS, TIERS_ASK};
    let attached = blaze_inproc::runtime::attach()
        // alloc: error path
        .map_err(|it| {
            format!("the tiers come only to a program blaze started on its code memory: {it}")
        })?;
    // alloc: the text, a few times a second
    let mut text = Vec::new();
    let kinds = ask::Kinds { ask: TIERS_ASK, piece: TIERS, none: NO_TIERS };
    if ask::fetch(attached, &kinds, "the tiers", u64::to_le_bytes, &mut text)?.is_none() {
        return Ok(None);
    }
    // alloc: error path
    let text =
        std::str::from_utf8(&text).map_err(|_| "the daemon's tiers are not UTF-8".to_owned())?;
    // alloc: error path
    format::Tiers::decode(text)
        .map(Some)
        .ok_or_else(|| "the daemon's tiers do not read as tiers".to_owned())
}

/// Hands the daemon the text of the source file at `path`, as an editor has it, without
/// writing it: the daemon builds from that text at once and a program that runs takes the
/// patch. The answer is the daemon's, as `blaze build` prints it: `ok patch=...
/// functions=N files=N ms=N`, `ok unchanged`, or why the edit is not built. The file itself
/// is the editor's to write when it likes; the daemon sees that it has that text already.
#[cfg(any(unix, all(windows, target_arch = "x86_64")))]
pub fn edit(path: &std::path::Path, text: &str) -> Result<String, String> {
    use std::io::{BufRead as _, Write as _};
    let socket = daemon_socket().ok_or("no home directory to find the daemon in")?;
    // alloc: error path
    let path = std::path::absolute(path).map_err(|it| format!("{}: {it}", path.display()))?;
    let path = path.to_str().ok_or("a path that is not UTF-8")?;
    let mut line = String::with_capacity(text.len() + path.len() + 8);
    line.push_str("edit\t");
    format::escape(path, &mut line);
    line.push('\t');
    format::escape(text, &mut line);
    line.push('\n');
    #[cfg(unix)]
    let stream = std::os::unix::net::UnixStream::connect(&socket);
    #[cfg(windows)]
    let stream = socket::Socket::connect(&socket);
    // alloc: error path
    let mut stream = stream.map_err(|it| format!("no daemon at {}: {it}", socket.display()))?;
    // alloc: error path
    stream.write_all(line.as_bytes()).map_err(|it| format!("sending the edit: {it}"))?;
    // alloc: the daemon's answer, handed to the caller: once per edit
    let mut reply = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut reply)
        // alloc: error path
        .map_err(|it| format!("reading the daemon's answer: {it}"))?;
    reply.truncate(reply.trim_end().len());
    if reply.is_empty() {
        // alloc: error path
        return Err("the daemon hung up without an answer".to_owned());
    }
    if reply.starts_with("ok") {
        return Ok(reply);
    }
    match reply.strip_prefix("error: ") {
        Some(why) => Err(format::unescape(why)),
        None => Err(reply),
    }
}

/// Where the daemon of this user listens, as blaze finds it: beside [`format::root`].
#[cfg(any(unix, all(windows, target_arch = "x86_64")))]
fn daemon_socket() -> Option<std::path::PathBuf> {
    Some(format::root()?.with_file_name("daemon.sock"))
}
