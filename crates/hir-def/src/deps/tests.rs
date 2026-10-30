use std::{sync::Arc, sync::Mutex};

use base_db::Crate;
use blaze_format::Aligned;
use blaze_ids::ContentId;
use rustc_hash::FxHashMap;
use test_fixture::WithFixture;

use super::*;
use crate::{nameres::frozen, test_db::TestDB};

const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib new_source_root:local
use lib::{Shown, shown, deep::deeper::f};
fn main() { shown!(); f(); }

//- /lib.rs crate:lib deps:core new_source_root:library
#[macro_export]
macro_rules! shown { () => { () } }
pub struct Shown;
pub enum E { A, B(u8) }
pub use E::*;
pub mod deep;
mod made { macro_rules! m { () => { pub fn made() {} } } m!(); }
pub use made::made;
use core::Missing;

//- /deep.rs
pub mod deeper { pub fn f() {} }
pub trait T { fn g(&self) -> u8; }
impl T for super::Shown { fn g(&self) -> u8 { 1 } }

//- /core.rs crate:core new_source_root:library
#[lang = "sized"]
pub trait Sized {}
#[rustc_builtin_macro]
pub macro derive {}
pub mod prelude { pub mod rust_2018 { pub use crate::derive; } }
"#;

/// The differences frozen bytes record are the process's: the tests that read them take turns.
pub(super) static TURN: Mutex<()> = Mutex::new(());

const BUILD: ContentId = ContentId([7; 32]);

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *base_db::all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
#[derive(Default)]
struct Memory {
    images: Mutex<FxHashMap<ContentId, Arc<Aligned>>>,
    gets: Mutex<usize>,
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<Arc<Aligned>>, String> {
        *self.gets.lock().unwrap() += 1;
        Ok(self.images.lock().unwrap().get(key).cloned())
    }

    fn put(&self, key: ContentId, image: Vec<u8>) -> Result<(), String> {
        let mut images = self.images.lock().unwrap();
        match images.get(&key) {
            Some(kept) if **kept != *image => Err(format!("{key:?} kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                images.insert(key, Arc::new(Aligned::copy_from(&image)));
                Ok(())
            }
        }
    }
}

fn all(db: &TestDB) -> Vec<Crate> {
    base_db::all_crates(db).iter().copied().collect()
}

/// A store with the images of a fresh database of `fixture`.
fn stored(fixture: &str) -> (Memory, Stored) {
    let mut db = TestDB::with_files(fixture);
    let memory = Memory::default();
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    let stored = store(&db, &served, &memory).unwrap();
    (memory, stored)
}

fn names(db: &TestDB, crates: &[Crate]) -> Vec<String> {
    let mut names: Vec<String> = crates.iter().map(|&it| serve::crate_name(db, it)).collect();
    names.sort();
    names
}

#[test]
fn only_crates_outside_the_workspace_are_eligible() {
    let db = TestDB::with_files(FIXTURE);
    assert!(eligible(&db, crate_named(&db, "lib")));
    assert!(eligible(&db, crate_named(&db, "core")));
    assert!(!eligible(&db, crate_named(&db, "main")));
    let keys = keys(&db, &all(&db), BUILD);
    assert_eq!(names(&db, &keys.keys().copied().collect::<Vec<_>>()), ["core", "lib"]);
}

#[test]
fn a_library_on_a_member_is_not_eligible() {
    let db = TestDB::with_files(
        "//- /lib.rs crate:lib deps:member new_source_root:library\n\
         pub use member::X;\n\
         //- /member.rs crate:member new_source_root:local\n\
         pub struct X;\n",
    );
    assert!(!eligible(&db, crate_named(&db, "lib")));
}

fn key_of(fixture: &str, name: &str, build: ContentId) -> ContentId {
    let db = TestDB::with_files(fixture);
    let krate = crate_named(&db, name);
    keys(&db, &[krate], build)[&krate]
}

#[test]
fn keys_are_the_same_in_every_database() {
    assert_eq!(key_of(FIXTURE, "lib", BUILD), key_of(FIXTURE, "lib", BUILD));
}

#[test]
fn a_key_covers_every_input() {
    let lib = key_of(FIXTURE, "lib", BUILD);
    let core = key_of(FIXTURE, "core", BUILD);
    let changed = |fixture: String, name| assert_ne!(key_of(&fixture, name, BUILD), lib, "{fixture}");
    // A file of the crate that is not its root.
    changed(FIXTURE.replace("pub mod deeper", "pub mod other"), "lib");
    // A dependency's text, which the crate's content ID leaves out.
    changed(FIXTURE.replace("pub trait Sized {}", "pub trait Sized {}\npub struct More;"), "lib");
    // The crate's cfg and edition.
    changed(FIXTURE.replace("crate:lib deps:core", "crate:lib deps:core cfg:feature=x"), "lib");
    changed(FIXTURE.replace("crate:lib deps:core", "crate:lib deps:core edition:2015"), "lib");
    // The blaze reading.
    assert_ne!(key_of(FIXTURE, "lib", ContentId([8; 32])), lib);
    // A dependent's text changes nothing of its dependencies.
    assert_eq!(key_of(&FIXTURE.replace("pub mod deeper", "pub mod other"), "core", BUILD), core);
    // Nor does a member's.
    assert_eq!(key_of(&FIXTURE.replace("fn main()", "fn main2()"), "lib", BUILD), lib);
}

#[test]
fn an_image_reads_back_its_parts() {
    let key = ContentId([1; 32]);
    let parts: [&[u8]; 3] = [b"def map", b"", &[9; 33]];
    let bytes = Aligned::copy_from(&write_image(key, parts));
    let image = ImageRef::open(&bytes, key).unwrap();
    for (part, bytes) in Part::ALL.into_iter().zip(parts) {
        assert_eq!(image.part(part), bytes, "{part:?}");
        assert!(bytes.is_empty() || (image.part(part).as_ptr() as usize).is_multiple_of(16));
    }
    assert_eq!(write_image(key, parts), write_image(key, parts));
}

#[test]
fn an_image_under_another_key_does_not_read() {
    let bytes = Aligned::copy_from(&write_image(ContentId([1; 32]), [b"a", b"b", b"c"]));
    assert_eq!(
        ImageRef::open(&bytes, ContentId([2; 32])).err(),
        Some(ImageError::Key { found: ContentId([1; 32]), wanted: ContentId([2; 32]) })
    );
}

/// Every byte of an image flipped in turn: it reads, with each part inside the buffer, or it
/// does not, and never panics. The content hash, which catches every flip, is the store's.
#[test]
fn damaged_images_read_within_bounds_or_not_at_all() {
    let key = ContentId([1; 32]);
    let written = write_image(key, [b"def map", b"lang", b"signatures"]);
    let mut read = 0;
    for at in 0..written.len() {
        let mut bytes = written.clone();
        bytes[at] ^= 0xff;
        let bytes = Aligned::copy_from(&bytes);
        if let Ok(image) = ImageRef::open(&bytes, key) {
            read += 1;
            for part in Part::ALL {
                let part = image.part(part);
                assert!(part.is_empty() || bytes.as_ptr_range().contains(&part.as_ptr()), "{at}");
            }
        }
    }
    assert!(read < written.len(), "no flip was refused");
}

#[test]
fn every_dependency_is_stored_and_served() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, stored) = stored(FIXTURE);
    assert_eq!(stored.refused, Vec::new());
    let mut db = TestDB::with_files(FIXTURE);
    assert_eq!(names(&db, &stored.written), ["core", "lib"]);
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
    for name in ["core", "lib"] {
        let krate = crate_named(&db, name);
        assert_eq!(mode(&db, krate), Some(Mode::Serve));
        assert!(image(&db, krate).is_some());
    }
    assert_eq!(mode(&db, crate_named(&db, "main")), None);
    // Nothing more to store, and a second store of the same keys keeps the same bytes.
    let again = store(&db, &served, &memory).unwrap();
    assert_eq!((again.written.len(), again.refused.len()), (0, 0));
    assert_eq!(frozen::differences(), Vec::<String>::new());
}

/// The def maps (with their diagnostics) and lang items of a database that read every
/// dependency from its image, as the frozen encoding writes them, against a fresh
/// database's.
#[test]
fn a_served_dependency_resolves_as_a_fresh_one() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, _) = stored(FIXTURE);
    let fresh = TestDB::with_files(FIXTURE);
    for mode in [Mode::Serve, Mode::Verify, Mode::Check] {
        let mut db = TestDB::with_files(FIXTURE);
        let crates = all(&db);
        serve(&mut db, &crates, BUILD, &memory, mode).unwrap();
        for name in ["core", "lib", "main"] {
            let (krate, live) = (crate_named(&db, name), crate_named(&fresh, name));
            let def_map = crate::nameres::crate_def_map(&db, krate);
            let fresh_map = crate::nameres::crate_def_map(&fresh, live);
            assert_eq!(def_map.diagnostics.len(), fresh_map.diagnostics.len(), "{name} {mode:?}");
            assert_eq!(frozen::encode(&db, krate), frozen::encode(&fresh, live), "{name} {mode:?}");
            assert_eq!(
                frozen::encode_lang_items(&db, krate),
                frozen::encode_lang_items(&fresh, live),
                "{name} {mode:?}"
            );
        }
        assert_eq!(frozen::differences(), Vec::<String>::new(), "{mode:?}");
    }
}

#[test]
fn an_unresolved_import_of_a_dependency_is_still_reported() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, _) = stored(FIXTURE);
    let mut db = TestDB::with_files(FIXTURE);
    let crates = all(&db);
    serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let lib = crate::nameres::crate_def_map(&db, crate_named(&db, "lib"));
    assert!(!lib.diagnostics.is_empty(), "`use core::Missing` is unresolved");
}

#[test]
fn an_edit_to_a_dependency_misses_its_image_and_its_dependents() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, _) = stored(FIXTURE);
    let edited = FIXTURE.replace("pub trait Sized {}", "pub trait Sized {}\npub struct More;");
    let mut db = TestDB::with_files(&edited);
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(served.hits, Vec::new());
    let stored = store(&db, &served, &memory).unwrap();
    assert_eq!(names(&db, &stored.written), ["core", "lib"]);
}

#[test]
fn an_image_written_under_another_key_is_an_error() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, _) = stored(FIXTURE);
    let db = TestDB::with_files(FIXTURE);
    let lib = crate_named(&db, "lib");
    let key = keys(&db, &[lib], BUILD)[&lib];
    let other = ContentId([3; 32]);
    let image = memory.images.lock().unwrap()[&key].clone();
    memory.images.lock().unwrap().insert(other, image.clone());
    memory.images.lock().unwrap().insert(key, Arc::new(Aligned::copy_from(&write_image(
        other,
        [b"", b"", b""],
    ))));
    let mut db = TestDB::with_files(FIXTURE);
    let crates = all(&db);
    let error = serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap_err();
    assert!(error.contains("the image of crate lib"), "{error}");
    assert!(error.contains("written under key"), "{error}");
}

#[test]
fn a_crate_with_an_item_from_a_proc_macro_is_refused() {
    let db = TestDB::with_files(
        "//- proc_macros: identity\n\
         //- /lib.rs\n\
         pub mod inner { #[proc_macros::identity] pub fn f() {} }\n",
    );
    let refused: Vec<Refused> =
        all(&db).into_iter().filter_map(|krate| serve::refusal(&db, krate)).collect();
    assert!(
        matches!(refused.as_slice(), [Refused::ProcMacro { call }] if call.contains("proc_macros")),
        "{refused:?}"
    );
}

#[test]
fn a_crate_with_no_outside_input_is_not_refused() {
    let db = TestDB::with_files(FIXTURE);
    for name in ["core", "lib"] {
        assert_eq!(serve::refusal(&db, crate_named(&db, name)), None, "{name}");
    }
}

#[test]
fn serving_asks_the_store_once_a_crate() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, _) = stored(FIXTURE);
    *memory.gets.lock().unwrap() = 0;
    let mut db = TestDB::with_files(FIXTURE);
    let crates = all(&db);
    serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    assert_eq!(*memory.gets.lock().unwrap(), 2);
}

/// [`Memory`] whose tasks run on four threads, each taking every fourth.
#[derive(Default)]
struct Threads(Memory);

impl ImageStore for Threads {
    fn get(&self, key: &ContentId) -> Result<Option<Arc<Aligned>>, String> {
        self.0.get(key)
    }

    fn put(&self, key: ContentId, image: Vec<u8>) -> Result<(), String> {
        self.0.put(key, image)
    }

    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        std::thread::scope(|scope| {
            for first in 0..4 {
                scope.spawn(move || (first..count).step_by(4).for_each(task));
            }
        });
    }
}

#[test]
fn tasks_on_threads_store_and_serve_as_in_order() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, _) = stored(FIXTURE);
    let threads = Threads::default();
    let mut db = TestDB::with_files(FIXTURE);
    let crates = all(&db);
    let served = serve(&mut db, &crates, BUILD, &threads, Mode::Serve).unwrap();
    let stored = store(&db, &served, &threads).unwrap();
    assert_eq!(names(&db, &stored.written), ["core", "lib"]);
    assert_eq!(contents(&threads.0), contents(&memory));
    let mut db = TestDB::with_files(FIXTURE);
    let served = serve(&mut db, &crates, BUILD, &threads, Mode::Serve).unwrap();
    assert_eq!(names(&db, &served.hits), ["core", "lib"]);
}

/// A served image's parts are slices of the buffer the store gave, not copies.
#[test]
fn served_parts_are_the_store_s_bytes() {
    let _turn = TURN.lock().unwrap_or_else(|it| it.into_inner());
    let (memory, _) = stored(FIXTURE);
    let mut db = TestDB::with_files(FIXTURE);
    let crates = all(&db);
    serve(&mut db, &crates, BUILD, &memory, Mode::Serve).unwrap();
    let lib = crate_named(&db, "lib");
    let key = keys(&db, &[lib], BUILD)[&lib];
    let kept = memory.images.lock().unwrap()[&key].clone();
    let part = image(&db, lib).unwrap().part(Part::Signatures);
    assert!(kept.as_ptr_range().contains(&part.as_ptr()) || part.is_empty());
}

/// What `memory` keeps, by key.
fn contents(memory: &Memory) -> Vec<(ContentId, Vec<u8>)> {
    let mut kept: Vec<_> =
        memory.images.lock().unwrap().iter().map(|(&key, bytes)| (key, bytes.to_vec())).collect();
    kept.sort();
    kept
}
