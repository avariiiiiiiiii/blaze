#![cfg(feature = "inventory")]

//! blaze issue #350: a value that recomputes equal after its durability dropped must not be
//! backdated, or a reader stays verified at the old, higher durability and a later edit of a
//! less durable input never reaches it.

#![forbid(unsafe_code)]

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

#[salsa::tracked(returns(copy))]
fn tens(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log(format!("tens({})", source.text(db)));
    source.text(db) / 10
}

#[salsa::tracked(returns(copy))]
fn reader(db: &dyn LogDatabase, source: Source) -> u32 {
    db.push_log("reader".to_string());
    tens(db, source) + 1
}

#[test]
fn reader_is_verified_again_after_a_durability_drop() {
    let mut db = common::LoggerDatabase::default();
    let a = Source::builder(12)
        .text_durability(Durability::HIGH)
        .new(&db);

    assert_eq!(reader(&db, a), 2);
    db.assert_logs(expect![[r#"
        [
            "reader",
            "tens(12)",
        ]"#]]);

    // `tens` is equal but now low durability: no backdate, so `reader` runs and takes the low
    // durability.
    a.set_text(&mut db).with_durability(Durability::LOW).to(13);
    assert_eq!(reader(&db, a), 2);
    db.assert_logs(expect![[r#"
        [
            "tens(13)",
            "reader",
        ]"#]]);

    // A low-durability edit must reach `reader`: it is verified again (deeply, so `tens` runs),
    // and as `tens` is equal at an unchanged durability it is backdated and `reader` is kept.
    a.set_text(&mut db).to(14);
    assert_eq!(reader(&db, a), 2);
    db.assert_logs(expect![[r#"
        [
            "tens(14)",
        ]"#]]);

    a.set_text(&mut db).to(27);
    assert_eq!(reader(&db, a), 3);
    db.assert_logs(expect![[r#"
        [
            "tens(27)",
            "reader",
        ]"#]]);
}

#[test]
fn equal_value_at_unchanged_durability_is_backdated_and_shallow_verified_later() {
    let mut db = common::LoggerDatabase::default();
    let high = Source::builder(12)
        .text_durability(Durability::HIGH)
        .new(&db);
    let low = Source::new(&db, 0);

    assert_eq!(reader(&db, high), 2);
    db.clear_logs();

    // A high-durability edit that leaves `tens` equal keeps `reader`.
    high.set_text(&mut db).with_durability(Durability::HIGH).to(15);
    assert_eq!(reader(&db, high), 2);
    db.assert_logs(expect![[r#"
        [
            "tens(15)",
        ]"#]]);

    // `reader` kept its high durability, so a low-durability edit elsewhere skips it entirely.
    low.set_text(&mut db).to(1);
    assert_eq!(reader(&db, high), 2);
    db.assert_logs(expect!["[]"]);
}
