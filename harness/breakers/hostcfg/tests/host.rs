//! A proc macro built as a module for wasm32-wasip1 had its crates told that target, so
//! synstructure left out `decl_derive!` and the module had none of its derives (#828).

use hostcfg::Fields;

#[test]
fn derives_declared_with_synstructure() {
    assert_eq!(hostcfg::Pair::fields(), 2);
    assert_eq!(hostcfg::Shape::fields(), 5);
}

#[test]
fn macro_told_the_host() {
    assert_eq!(hostcfg::host(), ("not wasi", "not wasm"));
}
