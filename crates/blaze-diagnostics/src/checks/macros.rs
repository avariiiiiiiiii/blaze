//! The errors of defining each `macro_rules!` of the crate, as rustc's expansion gives them
//! when it comes to the definition: mbe accepts definitions rustc refuses (no rules, an
//! unknown fragment specifier, a binding twice), so hir's `MacroDefError` does not have them
//! all. Each definition `cfg` keeps, in a file or in an expansion, in the order the walk comes
//! to it.
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs` (`compile_declarative_macro`,
//! called as `rustc_resolve`'s `define_macro` meets each definition) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{AstNode as _, ast};

use crate::{cx::Cx, lints::syntax::Walk, lower::Found, lower::macro_def_error};

pub(crate) fn check(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Found> {
    let mut found = Vec::new();
    for tree in &walk.trees {
        for node in &tree.nodes {
            if !ast::Macro::can_cast(node.kind()) {
                continue;
            }
            let Some(mac) = ast::Macro::cast(node.clone()) else { continue };
            found.extend(macro_def_error::check(cx, InFile::new(tree.file, &mac)));
        }
    }
    found
}
