//! Macros whose crates are told the target they run on: rustc builds a proc macro for the
//! host, blaze builds it as a module for wasm32-wasip1.

use quote::quote;
use synstructure::decl_derive;

// synstructure declares its `decl_derive!` only where it takes `proc_macro` to work, which is
// not wasm32 with `target_os = "wasi"`: as hir-ty's `TypeVisitable` and ICU's `Yokeable` are.
decl_derive!([Fields] => fields_derive);

fn fields_derive(s: synstructure::Structure<'_>) -> proc_macro2::TokenStream {
    let count = s.variants().iter().map(|it| it.bindings().len()).sum::<usize>();
    s.gen_impl(quote! {
        gen impl crate::Fields for @Self {
            fn fields() -> usize {
                #count
            }
        }
    })
}

/// `(target_os, target_family)` as the macro's own crate is told them.
#[proc_macro]
pub fn host(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let os = if cfg!(target_os = "wasi") { "wasi" } else { "not wasi" };
    let family = if cfg!(target_family = "wasm") { "wasm" } else { "not wasm" };
    quote!((#os, #family)).into()
}
