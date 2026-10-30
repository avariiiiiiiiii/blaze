//! The fixture's macros built into this test against blaze's in-process bridge, as a reference
//! of their own: the same source the fixture's crate compiles, its `crate::proc_macro` the
//! bridge's (the test's root names it so).

use blaze_macro_wasm::native::Expander;

#[allow(dead_code, reason = "include_data and env_name would read the test's own process: not references")]
#[path = "ws/macros/src/imp.rs"]
mod imp;

/// The native reference of macro `name` of crate `krate`, where it has one.
pub(crate) fn reference(krate: &str, name: &str) -> Option<Expander> {
    if krate != "corpus_macros" {
        return None;
    }
    match name {
        "make_fn" => Some(Expander::One(imp::make_fn)),
        "Describe" => Some(Expander::One(imp::describe)),
        "label" => Some(Expander::Two(imp::label)),
        "boom" => Some(Expander::One(imp::boom)),
        _ => None,
    }
}
