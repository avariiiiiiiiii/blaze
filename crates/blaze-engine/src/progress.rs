//! What a worker or checker of the daemon is doing while a command waits for it: a status
//! word and what it is of (`Compiling` `app`), in a file beside the process's socket. The
//! daemon reads it as it waits and tells the command (`Reply::Progress`), which shows it: a
//! load and a first build take long enough that a command saying nothing looks stuck.

use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// The file this process says what it does in, once it has a socket.
static FILE: OnceLock<PathBuf> = OnceLock::new();

/// The file a process that listens at `socket` says what it does in.
pub(crate) fn beside(socket: &Path) -> PathBuf {
    socket.with_file_name("phase")
}

/// Has this process say what it does beside `socket` from now on.
pub(crate) fn said_beside(socket: &Path) {
    let _ = FILE.set(beside(socket));
}

/// Says the process is at `word` of `what`. A process with no socket says nothing.
pub(crate) fn phase(word: &str, what: &str) {
    let Some(file) = FILE.get() else { return };
    // Whole or not at all, for the daemon that reads it meanwhile.
    let written = file.with_extension(std::process::id().to_string());
    if std::fs::write(&written, format!("{word}\t{what}")).is_ok()
        && std::fs::rename(&written, file).is_err()
    {
        let _ = std::fs::remove_file(&written);
    }
}

/// Says the process has nothing under way: it waits to be asked.
pub(crate) fn idle() {
    if let Some(file) = FILE.get() {
        let _ = std::fs::remove_file(file);
    }
}

/// What the process that writes `file` says it is at, when it says anything.
pub(crate) fn read(file: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(file).ok()?;
    let (word, what) = text.split_once('\t')?;
    Some((word.to_owned(), what.to_owned()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_phase_is_read_as_written() {
        let dir = std::env::temp_dir().join(format!("blaze-progress-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = super::beside(&dir.join("sock"));
        assert_eq!(super::read(&file), None);
        std::fs::write(&file, "Compiling\tapp").unwrap();
        assert_eq!(super::read(&file), Some(("Compiling".to_owned(), "app".to_owned())));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
