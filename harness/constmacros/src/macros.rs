//! The macros of `shapes`, which blaze's const evaluator runs (docs/future/const-macros.md).

/// `impl Named for <the struct>`, whose `NAME` is the struct's name.
#[proc_macro_derive(Named)]
pub fn named(input: &str) -> String {
    let mut words = input.split_whitespace().skip_while(|it| *it != "struct");
    let name = words.nth(1).unwrap_or_default();
    format!("impl crate::Named for {name} {{ const NAME: &'static str = \"{name}\"; }}")
}
