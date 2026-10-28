//! The records of a build as a process writes them while it builds, for another to follow: each
//! a frame appended to a file beside the process's socket, named for the process so that one
//! started in another's place writes a file of its own. A record's time there is since the UNIX
//! epoch; whoever hands the records on counts it from when the build was asked for.

use std::{
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::events::Record;

/// The file the process `pid` that listens at `socket` says what it does in.
pub fn beside(socket: &Path, pid: u32) -> PathBuf {
    socket.with_file_name(format!("events.{pid}"))
}

/// Takes away what the processes that listened at `socket` before said.
pub fn forget_beside(socket: &Path) {
    let Some(dir) = socket.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("events.") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The file the process that listens in `dir` says what it does in: the one written last,
/// of a process that started in the place of another that has not quite gone.
pub fn written_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let written =
        entries.flatten().filter(|it| it.file_name().to_string_lossy().starts_with("events."));
    let written = written.filter_map(|it| Some((it.metadata().ok()?.modified().ok()?, it.path())));
    written.max().map(|(_, path)| path)
}

/// Nanoseconds since the UNIX epoch.
pub fn now_ns() -> u64 {
    let since = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
}

/// The records a process writes to a file, read as it writes them.
#[derive(Default)]
pub struct Follow {
    path: Option<PathBuf>,
    at: u64,
    /// Bytes read of a frame not written in full yet.
    partial: Vec<u8>,
}

impl Follow {
    /// The records written to `path` since the last read, from the end of what it had when
    /// it was first named: a process says what it did before the command asked too. A file
    /// other than the last is read from its start, as a process started in another's place.
    pub fn read(&mut self, path: &Path, from_end: bool) -> Vec<Record> {
        if self.path.as_deref() != Some(path) {
            let len = std::fs::metadata(path).map_or(0, |it| it.len());
            let first = self.path.is_none();
            self.path = Some(path.to_owned());
            self.at = if first && from_end { len } else { 0 };
            self.partial.clear();
        }
        let Ok(mut file) = File::open(path) else { return Vec::new() };
        if file.seek(SeekFrom::Start(self.at)).is_err() {
            return Vec::new();
        }
        let mut bytes = std::mem::take(&mut self.partial);
        let Ok(read) = file.read_to_end(&mut bytes) else { return Vec::new() };
        self.at += read as u64;
        let mut records = Vec::new();
        let mut rest = &bytes[..];
        while rest.len() >= 4 {
            let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            let Some(item) = rest.get(4..4 + len) else { break };
            match crate::cbor::from_slice::<Record>(item) {
                Ok(record) => records.push(record),
                Err(why) => eprintln!("{}: a record is not read: {why}", path.display()),
            }
            rest = &rest[4 + len..];
        }
        self.partial = rest.to_vec();
        records
    }
}

#[cfg(test)]
mod tests {
    use crate::events::{Event, Level, Step};

    #[test]
    fn records_are_followed_as_they_are_written() {
        let dir = std::env::temp_dir().join(format!("blaze-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.1");
        let frame = |event: Event| {
            let record = crate::events::Record { at_ns: 5, event };
            let item = crate::cbor::to_vec(&record).unwrap();
            let mut frame = Vec::new();
            crate::write_frame(&mut frame, &item).unwrap();
            frame
        };
        let before = frame(Event::Said { level: Level::Note, text: "before".to_owned() });
        std::fs::write(&path, &before).unwrap();
        let mut follow = super::Follow::default();
        assert!(follow.read(&path, true).is_empty(), "what was said before is not read");
        let started = frame(Event::Started { step: Step::Codegen, of: None });
        let ended = frame(Event::Ended { step: Step::Codegen, of: None });
        let mut written = before.clone();
        written.extend(&started);
        written.extend(&ended[..3]);
        std::fs::write(&path, &written).unwrap();
        let read = follow.read(&path, true);
        assert_eq!(read.len(), 1, "a frame half written waits for the rest");
        written.extend(&ended[3..]);
        std::fs::write(&path, &written).unwrap();
        let read = follow.read(&path, true);
        assert!(matches!(read[..], [crate::events::Record { event: Event::Ended { .. }, .. }]));
        // A process in the place of the last writes another file, read from its start.
        let other = dir.join("events.2");
        std::fs::write(&other, &before).unwrap();
        assert_eq!(follow.read(&other, true).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
