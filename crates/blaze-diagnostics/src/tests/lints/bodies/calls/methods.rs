//! rustc's UI tests of `NoopMethodCall`, `MapUnitFn`, `NonPanicFmt` and `AsmLabels`, each compiled
//! against a `core` and a `std` of the items it uses, written as `library/core` and
//! `library/std` write them, their `#[lang]`s and `#[rustc_diagnostic_item]`s included: these
//! lints read diagnostic items minicore does not have (`Clone`, `noop_method_clone`,
//! `IteratorMap`, `std_panic_2015_macro`, ..), and the macros of edition 2015's `panic!`.

mod asm_labels;
mod map_unit_fn;
mod non_fmt_panic;
mod noop_method_call;

use super::{Ui, compiletest};

/// What every test's `core` starts with: `marker`'s `Sized` and `Copy`, `clone`'s `Clone` with
/// `&T`'s no-op `clone`, the primitives' impls of both, and the `derive` attribute.
const CORE: &str = r#"#![no_core]
#![rustc_coherence_is_core]
#![feature(no_core, lang_items, rustc_attrs, decl_macro, fundamental, auto_traits, negative_impls)]

pub mod marker {
    #[lang = "pointee_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait PointeeSized {}

    #[lang = "meta_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait MetaSized: PointeeSized {}

    #[lang = "sized"]
    #[rustc_diagnostic_item = "Sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait Sized: MetaSized {}

    #[rustc_diagnostic_item = "Send"]
    pub unsafe auto trait Send {}
    impl<T: PointeeSized> !Send for *const T {}
    impl<T: PointeeSized> !Send for *mut T {}
    unsafe impl<T: Sync + PointeeSized> Send for &T {}
    unsafe impl<T: Send + PointeeSized> Send for &mut T {}

    #[rustc_diagnostic_item = "Sync"]
    #[lang = "sync"]
    pub unsafe auto trait Sync {}
    impl<T: PointeeSized> !Sync for *const T {}
    impl<T: PointeeSized> !Sync for *mut T {}

    #[rustc_unsafe_specialization_marker]
    #[rustc_diagnostic_item = "Copy"]
    #[lang = "copy"]
    pub trait Copy: Clone {}

    #[rustc_builtin_macro]
    pub macro Copy($item:item) {}

    use crate::clone::Clone;

    macro_rules! impl_copy {
        ($($t:ty)*) => { $(impl Copy for $t {})* };
    }
    impl_copy! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 f32 f64 bool char ! }
    impl<T: PointeeSized> Copy for *const T {}
    impl<T: PointeeSized> Copy for *mut T {}
    impl<T: PointeeSized> Copy for &T {}
}

pub mod clone {
    use crate::marker::{PointeeSized, Sized};

    #[lang = "clone"]
    #[rustc_diagnostic_item = "Clone"]
    pub trait Clone: Sized {
        fn clone(&self) -> Self;
    }

    #[rustc_builtin_macro]
    pub macro Clone($item:item) {}

    macro_rules! impl_clone {
        ($($t:ty)*) => {
            $(impl Clone for $t {
                fn clone(&self) -> Self {
                    *self
                }
            })*
        };
    }
    impl_clone! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 f32 f64 bool char ! }

    impl<T: PointeeSized> Clone for *const T {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<T: PointeeSized> Clone for *mut T {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<T: PointeeSized> Clone for &T {
        #[rustc_diagnostic_item = "noop_method_clone"]
        fn clone(&self) -> Self {
            *self
        }
    }
}

pub mod macros {
    pub mod builtin {
        #[rustc_builtin_macro]
        pub macro derive($item:item) {}
    }
}
"#;

/// The crates a UI test is compiled against.
#[derive(Default)]
pub(crate) struct Library<'a> {
    /// The modules of `core` the test uses beyond [`CORE`]'s, as core writes them.
    pub(crate) core: &'a str,
    /// What core's prelude has beyond `Sized`, `Send`, `Sync`, `Copy`, `Clone` and `derive`:
    /// `pub use crate::..;` lines.
    pub(crate) core_prelude: &'a str,
    /// The modules of `std` the test uses, as std writes them; `core` is its dependency.
    pub(crate) std: &'a str,
    /// What std's prelude has beyond core's: `pub use crate::..;` lines.
    pub(crate) std_prelude: &'a str,
    /// The crates of the test's `//@ aux-build`s, `(crate name, edition, source)`, each
    /// depending on `std` and `core`; the test's crate depends on each.
    pub(crate) aux: &'a [(&'a str, &'a str, &'a str)],
}

/// The prelude module of each edition: `uses` and, in std's, all of core's first.
fn prelude(of_core: bool, uses: &str) -> String {
    let mut out = String::from("pub mod prelude {\n");
    for edition in ["rust_2015", "rust_2018", "rust_2021", "rust_2024"] {
        out.push_str(&format!("    pub mod {edition} {{\n"));
        if of_core {
            out.push_str(
                "        pub use crate::marker::{Copy, Send, Sized, Sync};\n\
                 \x20       pub use crate::clone::Clone;\n\
                 \x20       pub use crate::macros::builtin::derive;\n",
            );
        } else {
            out.push_str(&format!("        pub use core::prelude::{edition}::*;\n"));
        }
        out.push_str(uses);
        out.push_str("\n    }\n");
    }
    out.push_str("}\n");
    out
}

/// What rustc writes to stderr compiling the UI test `ui` against `library`: its `.stderr`.
/// `ui`'s own `core` and `std` are minicore's, which these tests do not use.
pub(crate) fn ui_library(ui: Ui<'_>, library: &Library<'_>) -> String {
    assert!(ui.core.is_empty() && ui.std.is_none(), "the library is `library`'s");
    let mut fixture = format!(
        "//- /core.rs crate:core edition:2024\n{CORE}{}\n{}\
         //- /std.rs crate:std edition:2024 deps:core\n{}\n{}",
        library.core,
        prelude(true, library.core_prelude),
        library.std,
        prelude(false, library.std_prelude),
    );
    let mut deps = String::from("std,core");
    for (name, edition, source) in library.aux {
        fixture.push_str(&format!("//- /{name}.rs crate:{name} edition:{edition} deps:std,core\n{source}"));
        if !source.ends_with('\n') {
            fixture.push('\n');
        }
        deps.push(',');
        deps.push_str(name);
    }
    let meta = if ui.meta.is_empty() { String::new() } else { format!(" {}", ui.meta) };
    fixture.push_str(&format!("//- /{} crate:main edition:{} deps:{deps}{meta}\n{}", ui.name, ui.edition, ui.source));
    let stderr = crate::tests::checked_with(&fixture, "$DIR/", &compiletest(ui.test, ui.flags))
        .stderr_with(crate::emit::Style { ui_testing: true, ..crate::emit::Style::default() });
    crate::tests::normalized(&stderr)
}
