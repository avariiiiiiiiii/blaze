//! A reader hitting a stale interned value while a writer sweeps the same shard for one to
//! reuse: either the reader's ID is current and holds the fields it interned, or the writer
//! reused the slot first and the reader got a new ID; never a reused slot under an old ID.

use std::hash::{Hash, Hasher};

use salsa::plumbing::AsId;
use salsa::{Database, Setter};

use crate::Knobs;
use crate::sync::thread;

#[salsa::input]
struct Input {
    #[returns(copy)]
    key: u32,
}

/// One hash for every key, so the writer's miss sweeps the reader's shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, salsa::SalsaValue)]
struct SameShard(u32);

impl Hash for SameShard {
    fn hash<H: Hasher>(&self, state: &mut H) {
        0u8.hash(state);
    }
}

#[salsa::interned(revisions = 1)]
struct Item<'db> {
    #[returns(copy)]
    key: SameShard,
}

#[salsa::tracked(returns(copy))]
fn intern(db: &dyn Database, input: Input) -> Item<'_> {
    let key = input.key(db);
    let item = Item::new(db, SameShard(key));
    assert_eq!(
        item.key(db),
        SameShard(key),
        "an interned ID holds the fields it was interned with"
    );
    item
}

#[test]
fn reader_and_reuser_race() {
    crate::sync::check(|| {
        let mut db = Knobs::default();
        let reader = Input::new(&db, 0);
        let old = intern(&db, reader).as_id();

        // A new revision: with `revisions = 1`, the value is stale, and the next miss in its
        // shard may reuse it unless a reader validates it first.
        reader.set_key(&mut db).to(0);
        let writer = Input::new(&db, 1);

        let db = &db;
        let (read, written) = thread::scope(|scope| {
            let read = scope.spawn(move || intern(db, reader));
            let written = scope.spawn(move || intern(db, writer));
            (read.join().unwrap(), written.join().unwrap())
        });

        assert_eq!(read.key(db), SameShard(0));
        assert_eq!(written.key(db), SameShard(1));
        let (read, written) = (read.as_id(), written.as_id());
        assert_ne!(read, written);
        if read.index() == old.index() {
            // The reader kept the old value, under its old ID, so the writer could not reuse it.
            assert_eq!(read, old);
            assert_ne!(written.index(), old.index());
        } else {
            // The writer claimed the stale slot first, under a new generation.
            assert_eq!(written, old.next_generation().unwrap());
        }
    });
}
