#![cfg(feature = "inventory")]

//! `evict_values` and `evict_values_of` drop memoized values but keep their fingerprints and
//! edges: a later read computes the value again and backdates it when it is equal.

mod common;
use std::cell::Cell;

use common::LogDatabase;
use expect_test::expect;
use salsa::Setter;
use test_log::test;

thread_local! {
    static DROPS: Cell<usize> = const { Cell::new(0) };
}

fn drops() -> usize {
    DROPS.with(Cell::get)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Counted(u32);

impl Drop for Counted {
    fn drop(&mut self) {
        DROPS.with(|drops| drops.set(drops.get() + 1));
    }
}

#[salsa::input]
struct Source {
    #[returns(copy)]
    text: u32,
}

#[salsa::tracked(returns(ref), lru = 0)]
fn tens(db: &dyn LogDatabase, source: Source) -> Counted {
    db.push_log(format!("tens({})", source.text(db)));
    Counted(source.text(db) / 10)
}

#[salsa::tracked(returns(copy))]
fn reader(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log("reader".to_string());
    tens(db, source).0 + 1
}

#[test]
fn evict_values_drops_values_and_keeps_early_cutoff() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 12);
    let b = Source::new(&db, 40);

    assert_eq!(reader(&db, a), 2);
    assert_eq!(tens(&db, b).0, 4);
    db.clear_logs();

    let before = drops();
    assert_eq!(tens::evict_values(&mut db), 2);
    assert_eq!(drops() - before, 2);
    assert_eq!(tens::evict_values(&mut db), 0);

    // `reader` keeps its own value.
    assert_eq!(reader(&db, a), 2);
    db.assert_logs(expect!["[]"]);

    // An edit that leaves `tens` equal: it runs again, is backdated by fingerprint and `reader`
    // is kept.
    a.set_text(&mut db).to(13);
    assert_eq!(reader(&db, a), 2);
    db.assert_logs(expect![[r#"
        [
            "tens(13)",
        ]"#]]);

    // An evicted value whose inputs did not change is computed again on read.
    assert_eq!(tens(&db, b).0, 4);
    db.assert_logs(expect![[r#"
        [
            "tens(40)",
        ]"#]]);

    // An edit that changes `tens` still reaches `reader`.
    assert_eq!(tens::evict_values(&mut db), 2);
    a.set_text(&mut db).to(21);
    assert_eq!(reader(&db, a), 3);
    db.assert_logs(expect![[r#"
        [
            "tens(21)",
            "reader",
        ]"#]]);
}

#[test]
fn evict_values_of_drops_only_the_given_keys() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 12);
    let b = Source::new(&db, 40);

    assert_eq!(tens(&db, a).0, 1);
    assert_eq!(tens(&db, b).0, 4);
    db.clear_logs();

    let before = drops();
    assert_eq!(tens::evict_values_of(&mut db, [a]), 1);
    assert_eq!(drops() - before, 1);
    assert_eq!(tens::evict_values_of(&mut db, [a]), 0);

    assert_eq!(tens(&db, b).0, 4);
    db.assert_logs(expect!["[]"]);

    assert_eq!(tens(&db, a).0, 1);
    db.assert_logs(expect![[r#"
        [
            "tens(12)",
        ]"#]]);
}

#[salsa::tracked(returns(copy), lru = 0)]
fn sum(db: &dyn LogDatabase, left: Source, right: Source) -> u32 {
    db.push_log("sum".to_string());
    left.text(db) + right.text(db)
}

#[test]
fn evict_values_covers_interned_keys() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 1);
    let b = Source::new(&db, 2);

    assert_eq!(sum(&db, a, b), 3);
    assert_eq!(sum(&db, b, b), 4);
    db.clear_logs();

    assert_eq!(sum::evict_values(&mut db), 2);
    assert_eq!(sum(&db, a, b), 3);
    db.assert_logs(expect![[r#"
        [
            "sum",
        ]"#]]);
}
