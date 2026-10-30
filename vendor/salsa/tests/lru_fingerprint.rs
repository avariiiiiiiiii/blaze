#![cfg(feature = "inventory")]

//! A value an LRU cap dropped keeps its fingerprint: when an input edit makes it recompute to an
//! equal value it is backdated, so what read it doesn't run again, unless its durability dropped.

mod common;
use common::LogDatabase;
use expect_test::expect;
use salsa::{Durability, Setter};
use test_log::test;

#[salsa::input]
struct Source {
    #[returns(copy)]
    text: u32,
}

#[salsa::tracked(returns(copy), lru = 1)]
fn parse(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log(format!("parse({})", source.text(db)));
    source.text(db) / 10
}

#[salsa::tracked(returns(copy))]
fn check(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log("check".to_string());
    parse(db, source) + 1
}

#[test]
fn evicted_value_recomputed_equal_is_backdated() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 12);
    let b = Source::new(&db, 40);

    assert_eq!(check(&db, a), 2);
    // `parse(b)` is used after `parse(a)`, so the cap of 1 drops `parse(a)` at the next revision.
    assert_eq!(parse(&db, b), 4);
    db.assert_logs(expect![[r#"
        [
            "check",
            "parse(12)",
            "parse(40)",
        ]"#]]);

    a.set_text(&mut db).to(13);

    assert_eq!(check(&db, a), 2);
    db.assert_logs(expect![[r#"
        [
            "parse(13)",
        ]"#]]);

    // A recomputed value that differs is not backdated.
    a.set_text(&mut db).to(25);
    assert_eq!(check(&db, a), 3);
    db.assert_logs(expect![[r#"
        [
            "parse(25)",
            "check",
        ]"#]]);
}

#[test]
fn evicted_value_is_not_backdated_when_its_durability_drops() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::builder(12)
        .text_durability(Durability::HIGH)
        .new(&db);
    let b = Source::new(&db, 40);

    assert_eq!(check(&db, a), 2);
    assert_eq!(parse(&db, b), 4);
    db.clear_logs();

    // `parse(a)` was dropped and now computes the same value, but at a lower durability: `check`
    // must run again so that it is verified at that durability from now on.
    a.set_text(&mut db).with_durability(Durability::LOW).to(13);
    assert_eq!(check(&db, a), 2);
    db.assert_logs(expect![[r#"
        [
            "parse(13)",
            "check",
        ]"#]]);

    // Had `check` kept its high durability, this low-durability edit would not reach it.
    a.set_text(&mut db).to(31);
    assert_eq!(check(&db, a), 4);
    db.assert_logs(expect![[r#"
        [
            "parse(31)",
            "check",
        ]"#]]);
}

#[salsa::tracked(returns(copy), lru = 0)]
fn digits(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log(format!("digits({})", source.text(db)));
    source.text(db) / 10
}

#[salsa::tracked(returns(copy))]
fn report(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log("report".to_string());
    digits(db, source) * 100
}

/// Reads `digits` with its fingerprint: it is evicted (which fingerprints it) and computed again
/// (the new memo keeps the fingerprint), then `report` reads it.
fn read_with_fingerprint(db: &mut common::LoggerDatabase, source: Source) {
    let tens = source.text(&*db) / 10;
    assert_eq!(digits(&*db, source), tens);
    assert_eq!(digits::evict_values(db), 1);
    assert_eq!(report(&*db, source), tens * 100);
    db.clear_logs();
}

/// `report` read `digits` = 1 with its fingerprint. `digits` then changes to 2 and back to 1
/// while nobody reads `report`, each time computed again after an eviction, so each memo has a
/// fingerprint: the edge to `digits` recorded the fingerprint of 1, which `digits` has again, so
/// `report` is unchanged although `digits` changed after `report` was verified.
#[test]
fn reader_is_unchanged_when_its_input_has_the_fingerprint_it_read() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 10);
    read_with_fingerprint(&mut db, a);

    a.set_text(&mut db).to(20);
    assert_eq!(digits::evict_values(&mut db), 1);
    assert_eq!(digits(&db, a), 2);

    a.set_text(&mut db).to(11);
    assert_eq!(digits::evict_values(&mut db), 1);
    assert_eq!(report(&db, a), 100);
    db.assert_logs(expect![[r#"
        [
            "digits(20)",
            "digits(11)",
        ]"#]]);
}

/// Without an eviction in between, `digits` is compared by `Eq` and its new memo has no
/// fingerprint: `report` runs again, as it would without fingerprints.
#[test]
fn reader_compares_by_revision_when_its_input_has_no_fingerprint() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::new(&db, 10);
    read_with_fingerprint(&mut db, a);

    a.set_text(&mut db).to(20);
    assert_eq!(digits(&db, a), 2);

    a.set_text(&mut db).to(11);
    assert_eq!(report(&db, a), 100);
    db.assert_logs(expect![[r#"
        [
            "digits(20)",
            "digits(11)",
            "report",
        ]"#]]);
}

#[test]
fn fingerprint_edge_does_not_hide_a_durability_drop() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::builder(10)
        .text_durability(Durability::HIGH)
        .new(&db);
    read_with_fingerprint(&mut db, a);

    a.set_text(&mut db).with_durability(Durability::LOW).to(20);
    assert_eq!(digits::evict_values(&mut db), 1);
    assert_eq!(digits(&db, a), 2);

    // `digits` has the fingerprint `report` read, but it is now less durable than `report`.
    a.set_text(&mut db).to(11);
    assert_eq!(digits::evict_values(&mut db), 1);
    assert_eq!(report(&db, a), 100);
    db.assert_logs(expect![[r#"
        [
            "digits(20)",
            "digits(11)",
            "report",
        ]"#]]);

    a.set_text(&mut db).to(35);
    assert_eq!(report(&db, a), 300);
    db.assert_logs(expect![[r#"
        [
            "digits(35)",
            "report",
        ]"#]]);
}
