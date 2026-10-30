#![cfg(all(feature = "inventory", not(feature = "shuttle")))]

//! Verifying memos by their edges with no claim (rustc's `try_mark_green`): a deep chain,
//! backdating, and many tasks verifying one chain at once give the same values and reruns as a
//! claimed check.

use std::sync::Mutex;

use salsa::Setter;

#[salsa::db]
trait LogDb: salsa::Database {
    fn log(&self, entry: String);
}

#[salsa::db]
#[derive(Default)]
struct Db {
    storage: salsa::Storage<Self>,
    log: Mutex<Vec<String>>,
}

impl Db {
    fn take_log(&self) -> Vec<String> {
        let mut log = std::mem::take(&mut *self.log.lock().unwrap());
        log.sort();
        log
    }
}

#[salsa::db]
impl salsa::Database for Db {}

#[salsa::db]
impl LogDb for Db {
    fn log(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }
}

#[salsa::input]
struct Chain {
    root: u32,
    other: u32,
}

/// Far deeper than the claim-free walk's inline stack (eight memos).
const LEN: u32 = 300;

#[salsa::tracked]
fn link(db: &dyn LogDb, chain: Chain, n: u32) -> u32 {
    db.log(format!("link {n:03}"));
    if n == 0 {
        chain.root(db) % 10
    } else {
        link(db, chain, n - 1) + 1
    }
}

fn all_links() -> Vec<String> {
    (0..=LEN).map(|n| format!("link {n:03}")).collect()
}

/// Running the chain nests `LEN` queries, deeper than a default thread's stack is sure to hold.
const STACK: usize = 64 << 20;

fn on_big_stack(f: impl FnOnce() + Send) {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn_scoped(scope, f)
            .unwrap()
            .join()
            .unwrap();
    });
}

#[test]
fn unrelated_change_verifies_a_deep_chain_without_running_it() {
    on_big_stack(unrelated_change_verifies_a_deep_chain_without_running_it_body);
}

fn unrelated_change_verifies_a_deep_chain_without_running_it_body() {
    let mut db = Db::default();
    let chain = Chain::new(&db, 3, 0);
    assert_eq!(link(&db, chain, LEN), 3 + LEN);
    assert_eq!(db.take_log(), all_links());

    for other in 1..4 {
        chain.set_other(&mut db).to(other);
        assert_eq!(link(&db, chain, LEN), 3 + LEN);
        assert_eq!(db.take_log(), Vec::<String>::new());
    }

    chain.set_root(&mut db).to(4);
    assert_eq!(link(&db, chain, LEN), 4 + LEN);
    assert_eq!(db.take_log(), all_links());
}

#[test]
fn a_backdated_link_stops_the_reruns() {
    on_big_stack(a_backdated_link_stops_the_reruns_body);
}

fn a_backdated_link_stops_the_reruns_body() {
    let mut db = Db::default();
    let chain = Chain::new(&db, 3, 0);
    assert_eq!(link(&db, chain, LEN), 3 + LEN);
    db.take_log();

    chain.set_root(&mut db).to(13);
    assert_eq!(link(&db, chain, LEN), 3 + LEN);
    assert_eq!(db.take_log(), vec!["link 000".to_owned()]);
}

#[test]
fn tasks_verifying_one_chain_agree_and_run_each_link_once() {
    on_big_stack(tasks_verifying_one_chain_agree_and_run_each_link_once_body);
}

fn tasks_verifying_one_chain_agree_and_run_each_link_once_body() {
    let mut db = Db::default();
    let chain = Chain::new(&db, 3, 0);
    assert_eq!(link(&db, chain, LEN), 3 + LEN);
    db.take_log();

    chain.set_other(&mut db).to(1);
    let db_ref = &db;
    let got: Vec<u32> = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                std::thread::Builder::new()
                    .stack_size(STACK)
                    .spawn_scoped(scope, move || link(db_ref, chain, LEN))
                    .unwrap()
            })
            .collect();
        tasks.into_iter().map(|task| task.join().unwrap()).collect()
    });
    assert_eq!(got, vec![3 + LEN; 8]);
    assert_eq!(db.take_log(), Vec::<String>::new());

    chain.set_root(&mut db).to(5);
    let db_ref = &db;
    let got: Vec<u32> = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..8)
            .map(|t| {
                std::thread::Builder::new()
                    .stack_size(STACK)
                    .spawn_scoped(scope, move || link(db_ref, chain, LEN - t * 7))
                    .unwrap()
            })
            .collect();
        tasks.into_iter().map(|task| task.join().unwrap()).collect()
    });
    let want: Vec<u32> = (0..8).map(|t| 5 + LEN - t * 7).collect();
    assert_eq!(got, want);
    assert_eq!(db.take_log(), all_links());
}
