#![cfg(feature = "inventory")]

//! `evict_values` and `evict_values_of` drop memoized values, fingerprinting each and keeping its
//! edges: a later read computes the value again and backdates it when it hashes the same. They
//! exist for every tracked function whose output is `Hash`, with or without `lru`.

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

#[salsa::tracked(returns(copy))]
fn halves(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log(format!("halves({})", source.text(db)));
    source.text(db) / 2
}

#[salsa::tracked(returns(copy))]
fn halves_reader(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log("halves_reader".to_string());
    halves(db, source) + 1
}

#[test]
fn function_without_lru_is_evicted_and_backdated() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 4);

    assert_eq!(halves_reader(&db, a), 3);
    db.clear_logs();

    assert_eq!(halves::evict_values(&mut db), 1);
    a.set_text(&mut db).to(5);
    assert_eq!(halves_reader(&db, a), 3);
    db.assert_logs(expect![[r#"
        [
            "halves(5)",
        ]"#]]);

    // The recomputed value is kept, and a change to it still reaches the reader.
    a.set_text(&mut db).to(6);
    assert_eq!(halves_reader(&db, a), 4);
    db.assert_logs(expect![[r#"
        [
            "halves(6)",
            "halves_reader",
        ]"#]]);
}

#[salsa::tracked(returns(copy), no_eq)]
fn halves_no_eq(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log(format!("halves_no_eq({})", source.text(db)));
    source.text(db) / 2
}

#[salsa::tracked(returns(copy))]
fn halves_no_eq_reader(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log("halves_no_eq_reader".to_string());
    halves_no_eq(db, source) + 1
}

#[test]
fn no_eq_function_is_evicted_without_a_fingerprint() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 4);

    assert_eq!(halves_no_eq_reader(&db, a), 3);
    db.clear_logs();

    assert_eq!(halves_no_eq::evict_values(&mut db), 1);
    a.set_text(&mut db).to(5);
    assert_eq!(halves_no_eq_reader(&db, a), 3);
    db.assert_logs(expect![[r#"
        [
            "halves_no_eq_reader",
            "halves_no_eq(5)",
        ]"#]]);
}
