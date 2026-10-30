//! The packages blaze has native expanders of, each exactly as published, with the features
//! it is built with here and what it depends on: what a macro's crate and every package it
//! depends on must be, byte for byte, for its native expander to be used (`mod.rs`, `find`).
//!
//! Each is a copy of its `.crate` under `vendor/native-macros/<name>-<version>/` that blaze
//! builds as a library, not a proc macro: its `Cargo.toml` is blaze's, renaming the package
//! and making its dependencies the copies beside it and blaze's `proc_macro`
//! (`proc_macro/`), and a macro crate's `src/lib.rs` is the published one without the lines
//! of its `#[proc_macro_derive]`, `#[proc_macro_attribute]` and `#[proc_macro]` attributes.
//! The published bytes of both are under `.published/`, which [`files`](super::files) lists
//! in their place.

use hir_expand::proc_macro::ProcMacroKind;
use native_proc_macro::TokenStream;

use super::files;

/// A package as published and as blaze builds it.
pub(super) struct Package {
    pub(super) name: &'static str,
    pub(super) version: &'static str,
    /// Its directory under `vendor/native-macros/`.
    pub(super) dir: &'static str,
    pub(super) files: &'static [(&'static str, &'static [u8])],
    /// The features it is built with here: the union of what its dependents in this table ask.
    pub(super) features: &'static [&'static str],
    pub(super) build: Build,
    /// Each dependency, by the name the package's code knows it by, as an index of
    /// [`PACKAGES`], sorted by name.
    pub(super) deps: &'static [(&'static str, usize)],
}

/// What a package's build script sets for it to be compiled with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Build {
    /// Nothing, on blaze's pinned nightly: no build script, or quote's, which sets
    /// `no_diagnostic_namespace` only for a rustc older than 1.78.
    Nothing,
    /// What proc-macro2's probes find, which only its compiled code can tell
    /// (`mod.rs`, `proc_macro2_cfg`).
    ProcMacro2,
}

pub(super) const UNICODE_IDENT: usize = 0;
pub(super) const PROC_MACRO2: usize = 1;
pub(super) const QUOTE: usize = 2;
pub(super) const SYN_2: usize = 3;
pub(super) const SYN_3: usize = 4;
pub(super) const SERDE_DERIVE: usize = 5;
pub(super) const THISERROR_IMPL_1: usize = 6;
pub(super) const THISERROR_IMPL_2: usize = 7;
pub(super) const TOKIO_MACROS: usize = 8;

/// syn's features: serde_derive asks clone-impls, derive, parsing, printing and proc-macro,
/// thiserror-impl 2 the default ones, and tokio-macros full besides.
const SYN_3_FEATURES: &[&str] =
    &["clone-impls", "default", "derive", "full", "parsing", "printing", "proc-macro"];

pub(super) static PACKAGES: [Package; 9] = [
    Package {
        name: "unicode-ident",
        version: "1.0.27",
        dir: "unicode-ident-1.0.27",
        files: files::UNICODE_IDENT,
        features: &[],
        build: Build::Nothing,
        deps: &[],
    },
    Package {
        name: "proc-macro2",
        version: "1.0.107",
        dir: "proc-macro2-1.0.107",
        files: files::PROC_MACRO2,
        features: &["default", "proc-macro"],
        build: Build::ProcMacro2,
        deps: &[("unicode_ident", UNICODE_IDENT)],
    },
    Package {
        name: "quote",
        version: "1.0.47",
        dir: "quote-1.0.47",
        files: files::QUOTE,
        features: &["default", "proc-macro"],
        build: Build::Nothing,
        deps: &[("proc_macro2", PROC_MACRO2)],
    },
    Package {
        name: "syn",
        version: "2.0.119",
        dir: "syn-2.0.119",
        files: files::SYN_2,
        features: &["clone-impls", "default", "derive", "parsing", "printing", "proc-macro"],
        build: Build::Nothing,
        deps: &[("proc_macro2", PROC_MACRO2), ("quote", QUOTE), ("unicode_ident", UNICODE_IDENT)],
    },
    Package {
        name: "syn",
        version: "3.0.9",
        dir: "syn-3.0.9",
        files: files::SYN_3,
        features: SYN_3_FEATURES,
        build: Build::Nothing,
        deps: &[("proc_macro2", PROC_MACRO2), ("quote", QUOTE), ("unicode_ident", UNICODE_IDENT)],
    },
    Package {
        name: "serde_derive",
        version: "1.0.229",
        dir: "serde_derive-1.0.229",
        files: files::SERDE_DERIVE,
        features: &["default"],
        build: Build::Nothing,
        deps: &[("proc_macro2", PROC_MACRO2), ("quote", QUOTE), ("syn", SYN_3)],
    },
    Package {
        name: "thiserror-impl",
        version: "1.0.69",
        dir: "thiserror-impl-1.0.69",
        files: files::THISERROR_IMPL_1,
        features: &[],
        build: Build::Nothing,
        deps: &[("proc_macro2", PROC_MACRO2), ("quote", QUOTE), ("syn", SYN_2)],
    },
    Package {
        name: "thiserror-impl",
        version: "2.0.21",
        dir: "thiserror-impl-2.0.21",
        files: files::THISERROR_IMPL_2,
        features: &[],
        build: Build::Nothing,
        deps: &[("proc_macro2", PROC_MACRO2), ("quote", QUOTE), ("syn", SYN_3)],
    },
    Package {
        name: "tokio-macros",
        version: "2.7.2",
        dir: "tokio-macros-2.7.2",
        files: files::TOKIO_MACROS,
        features: &[],
        build: Build::Nothing,
        deps: &[("proc_macro2", PROC_MACRO2), ("quote", QUOTE), ("syn", SYN_3)],
    },
];

/// A macro's function in its crate, as built here.
#[derive(Clone, Copy)]
pub(super) enum Expander {
    /// A derive's or a function-like macro's.
    One(fn(TokenStream) -> TokenStream),
    /// An attribute's.
    Two(fn(TokenStream, TokenStream) -> TokenStream),
}

/// A macro of a crate of [`ROOTS`]: its name and kind as the crate's proc-macro attribute
/// declares them, and its function.
pub(super) struct Entry {
    pub(super) name: &'static str,
    pub(super) kind: ProcMacroKind,
    pub(super) expander: Expander,
}

/// A proc-macro crate of [`PACKAGES`] and every macro it declares.
pub(super) struct Root {
    pub(super) package: usize,
    pub(super) macros: &'static [Entry],
}

const fn derive(name: &'static str, f: fn(TokenStream) -> TokenStream) -> Entry {
    Entry { name, kind: ProcMacroKind::CustomDerive, expander: Expander::One(f) }
}

const fn attribute(
    name: &'static str,
    f: fn(TokenStream, TokenStream) -> TokenStream,
) -> Entry {
    Entry { name, kind: ProcMacroKind::Attr, expander: Expander::Two(f) }
}

const fn bang(name: &'static str, f: fn(TokenStream) -> TokenStream) -> Entry {
    Entry { name, kind: ProcMacroKind::Bang, expander: Expander::One(f) }
}

pub(super) static ROOTS: [Root; 4] = [
    Root {
        package: SERDE_DERIVE,
        macros: &[
            derive("Serialize", native_serde_derive::derive_serialize),
            derive("Deserialize", native_serde_derive::derive_deserialize),
        ],
    },
    Root {
        package: THISERROR_IMPL_1,
        macros: &[derive("Error", native_thiserror_impl_1::derive_error)],
    },
    Root {
        package: THISERROR_IMPL_2,
        macros: &[derive("Error", native_thiserror_impl_2::derive_error)],
    },
    Root {
        package: TOKIO_MACROS,
        macros: &[
            attribute("main", native_tokio_macros::main),
            attribute("main_rt", native_tokio_macros::main_rt),
            attribute("test", native_tokio_macros::test),
            attribute("test_rt", native_tokio_macros::test_rt),
            attribute("main_fail", native_tokio_macros::main_fail),
            attribute("test_fail", native_tokio_macros::test_fail),
            bang(
                "select_priv_declare_output_enum",
                native_tokio_macros::select_priv_declare_output_enum,
            ),
            bang("select_priv_clean_pattern", native_tokio_macros::select_priv_clean_pattern),
        ],
    },
];
