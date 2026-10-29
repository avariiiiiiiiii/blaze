//! A def map is the same whatever the threads its macros expand on.

use super::*;
use crate::nameres::EXPANSION_THREADS;

/// The def map of the fixture's test crate, with its diagnostics, collected expanding each
/// round on `threads` threads.
fn collected_on(threads: usize, #[rust_analyzer::rust_fixture] ra_fixture: &str) -> String {
    let previous = EXPANSION_THREADS.replace(Some(threads));
    let db = TestDB::with_files(ra_fixture);
    let krate = db.fetch_test_crate();
    let def_map = crate_def_map(&db, krate);
    let diagnostics: Vec<String> =
        def_map.diagnostics().iter().map(|it| format!("{:?}", it.kind)).collect();
    let collected = format!("{}\n{diagnostics:#?}", def_map.dump(&db));
    EXPANSION_THREADS.set(previous);
    collected
}

fn check_same_on_any_threads(#[rust_analyzer::rust_fixture] ra_fixture: &str, has: &[&str]) {
    let serial = collected_on(1, ra_fixture);
    for item in has {
        assert!(serial.contains(item), "`{item}` is not in the def map:\n{serial}");
    }
    for threads in [2, 3, 4, 8, 32] {
        for _ in 0..3 {
            let parallel = collected_on(threads, ra_fixture);
            assert_eq!(serial, parallel, "the def map differs expanding on {threads} threads");
        }
    }
}

/// Each round's expansions define the macros the next round calls, in many modules at once.
const ROUNDS: &str = r#"
//- /lib.rs
macro_rules! items {
    ($($name:ident)*) => { $(pub struct $name;)* };
}

macro_rules! first {
    ($m:ident) => {
        macro_rules! second {
            ($n:ident) => {
                macro_rules! third {
                    ($o:ident) => { pub mod $o { items!(Three); } };
                }
                pub mod $n { items!(Two); }
            };
        }
        pub mod $m { items!(One); }
    };
}

first!(one);
second!(two);
third!(three);

macro_rules! module {
    ($name:ident, $($item:ident)*) => { pub mod $name { items!($($item)*); } };
}

module!(a, A1 A2 A3);
module!(b, B1 B2 B3);
module!(c, C1 C2 C3);
module!(d, D1 D2 D3);
module!(e, E1 E2 E3);
module!(f, F1 F2 F3);
module!(g, G1 G2 G3);
module!(h, H1 H2 H3);

pub use a::*;
pub use h::H1 as Last;

mod nested;

missing!();

//- /nested.rs
module!(i, I1);
module!(j, J1);
second!(k);
"#;

#[test]
fn rounds_of_macro_rules_collect_the_same_on_any_threads() {
    check_same_on_any_threads(
        ROUNDS,
        &[
            "crate::one",
            "crate::two",
            "crate::three",
            "Three",
            "crate::h",
            "Last",
            "crate::nested::k",
            "J1",
        ],
    );
}

#[test]
fn exported_macros_of_a_dependency_collect_the_same_on_any_threads() {
    check_same_on_any_threads(
        r#"
//- /main.rs crate:main deps:lib
use lib::*;

lib::define!(made);
made!(Made1);
made!(Made2);
lib::structs!(S1 S2 S3);
mod m {
    lib::structs!(M1);
    crate::made!(M2);
}

//- /lib.rs crate:lib
#[macro_export]
macro_rules! structs {
    ($($name:ident)*) => { $(pub struct $name;)* };
}

#[macro_export]
macro_rules! define {
    ($name:ident) => {
        #[macro_export]
        macro_rules! $name { ($item:ident) => { $crate::structs!($item); }; }
    };
}
"#,
        &["Made1", "Made2", "S3", "crate::m", "M1", "M2"],
    );
}

#[test]
fn attribute_and_derive_macros_collect_the_same_on_any_threads() {
    check_same_on_any_threads(
        r#"
//- proc_macros: identity, derive_identity
//- minicore: derive, clone, copy
//- /lib.rs
macro_rules! made {
    ($name:ident) => {
        #[proc_macros::identity]
        #[derive(proc_macros::DeriveIdentity, Clone, Copy)]
        pub struct $name;
    };
}

#[proc_macros::identity]
pub struct Plain;

#[proc_macros::identity]
pub mod inner {
    #[proc_macros::identity]
    pub fn f() {}
}

made!(X1);
made!(X2);
made!(X3);
made!(X4);

#[derive(Clone)]
pub enum Derived { A, B }
"#,
        &["Plain", "crate::inner", "X1", "X4", "Derived"],
    );
}
