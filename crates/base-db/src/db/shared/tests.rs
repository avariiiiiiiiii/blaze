use std::{
    panic::AssertUnwindSafe,
    sync::mpsc,
    time::{Duration, Instant},
};

use salsa::{Cancelled, Database as _};
use span::FileId;

use super::Shared;
use crate::{Db, FileText};

#[salsa::tracked]
fn text_len(db: &Db, text: FileText) -> u32 {
    text.text(db).len() as u32
}

fn shared_with(text: &str) -> (Shared, FileId) {
    let mut db = Db::for_tests();
    let file = FileId::from_raw(0);
    db.set_file_text(file, text);
    (Shared::new(db), file)
}

#[test]
fn a_write_stops_a_held_reader_and_waits_for_it_to_let_go() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    let text = shared.file_text(file);
    let reader = shared.reader();
    let (started, has_started) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let ran = Cancelled::catch(AssertUnwindSafe(|| {
            started.send(()).unwrap();
            while Instant::now() < deadline {
                reader.unwind_if_revision_cancelled();
                assert_eq!(text_len(&reader, text), 13);
                std::thread::yield_now();
            }
        }));
        drop(reader);
        ran
    });
    has_started.recv().unwrap();
    shared.write().set_file_text(file, "fn other() {}\n");
    assert!(matches!(thread.join().unwrap(), Err(Cancelled::PendingWrite)));
    assert!(!shared.readers_stopped(), "the write clears the stop");
    assert_eq!(text_len(&shared, shared.file_text(file)), 14);
}

#[test]
fn a_write_with_no_reader_left_neither_waits_nor_stops() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    drop(shared.reader());
    shared.write().set_file_text(file, "fn other() {}\n");
    assert!(!shared.readers_stopped());
    assert_eq!(text_len(&shared, shared.file_text(file)), 14);
}

#[test]
fn idle_is_the_database_only_while_no_reader_holds_it_and_stops_none() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    let text = shared.file_text(file);
    let reader = shared.reader();
    assert!(shared.idle().is_none());
    assert!(!shared.readers_stopped(), "a reader goes on");
    assert_eq!(text_len(&reader, text), 13);
    drop(reader);
    shared.idle().expect("no reader is left").set_file_text(file, "fn other() {}\n");
    assert_eq!(text_len(&shared, shared.file_text(file)), 14);
}

#[test]
fn a_stop_holds_until_the_next_write() {
    let (mut shared, file) = shared_with("fn main() {}\n");
    let text = shared.file_text(file);
    shared.stop_readers();
    assert!(shared.readers_stopped());
    let stopped = Cancelled::catch(AssertUnwindSafe(|| text_len(&shared, text)));
    assert!(matches!(stopped, Err(Cancelled::PendingWrite)));
    let reader = shared.reader();
    let stopped = Cancelled::catch(AssertUnwindSafe(|| text_len(&reader, text)));
    assert!(matches!(stopped, Err(Cancelled::PendingWrite)), "a reader made after the stop");
    drop(reader);
    let db = shared.into_inner();
    assert!(!db.readers_stopped());
    assert_eq!(text_len(&db, text), 13);
}
