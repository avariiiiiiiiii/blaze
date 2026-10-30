//! What the resolve and its graph read of a package: one type for the load
//! (`load::summary`), blaze-metadata and blaze-tree, so a resolve's packages are handed on as
//! read, never copied into another crate's form.

use blaze_resolve::{InternedString, Summary};

/// What the resolve and its graph read of one package.
pub struct Read {
    pub summary: Summary,
    /// The crate name of its library, `-` as `_`.
    pub lib: Option<InternedString>,
    pub proc_macro_lib: bool,
    pub any_proc_macro: bool,
    /// Whether it has a build script, without which its build dependencies are not built.
    pub build_script: bool,
    /// The `required-features` of each of its binaries.
    pub bins: Vec<Vec<String>>,
    /// `package.license` and `package.repository`.
    pub license: Option<String>,
    pub repository: Option<String>,
}
