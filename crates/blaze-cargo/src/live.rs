//! Whether a program that takes edits runs; laying edits out for it is the engine's
//! (`crates/blaze-engine/src/live.rs`).

use blaze_live::format::{self, Process};

/// Whether any program that takes edits runs: a worker stopped while one does starts again
/// from the store, and its first build links the program again, which ends the program's
/// edits.
pub fn any_running() -> bool {
    let Some(root) = format::root() else { return false };
    let Ok(dirs) = std::fs::read_dir(root) else { return false };
    dirs.flatten().any(|dir| {
        let text = std::fs::read_to_string(dir.path().join("process")).unwrap_or_default();
        Process::decode(&text).is_some_and(|it| alive(it.pid))
    })
}

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: a signal of 0 is only a question.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::WAIT_TIMEOUT,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    // SAFETY: it takes no pointer, and the handle is closed by what owns it from here.
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    // SAFETY: the handle is open and nothing else owns it.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    // SAFETY: the handle is open; a wait of no time only asks whether the process ended.
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) == WAIT_TIMEOUT }
}

#[cfg(not(any(unix, windows)))]
pub fn alive(_: u32) -> bool {
    false
}
