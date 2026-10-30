//! A segment of a signature's path naming an item the place can't see: E0603 "{descr} `x` is
//! private", with the item's head. Only items declared in the module the segment is looked up
//! in; one reached through a private import is rustc's import chain, which needs hir to say
//! which names of a scope are imports.
//!
//! Follows rustc's `compiler/rustc_resolve/src/diagnostics/impls.rs` (`report_privacy_error`)
//! and `diagnostics/mod.rs` (`IsPrivate`, `NoteAndRefersToTheItemDefinedHere`) (MIT or
//! Apache-2.0).

use hir::{HasVisibility as _, InFile, ModuleDef, PathResolution};
use syntax::ast;

use super::{
    Signature,
    failure::Segment,
    paths::{def_descr, def_span},
};
use crate::{cx::Cx, model::Diag};

/// The E0603 of the first segment after the first whose item `current` can't see; `prefixes`
/// and `segments` are the path's prefixes and segments, one for one.
pub(super) fn check(
    cx: &Cx<'_, '_>,
    signature: &Signature,
    prefixes: &[ast::Path],
    segments: &[Segment],
    current: hir::Module,
) -> Option<Diag> {
    let file = signature.node.file_id;
    let mut parent: Option<ModuleDef> = None;
    for (prefix, segment) in prefixes.iter().zip(segments) {
        let def = match cx.sema.resolve_path(prefix)? {
            PathResolution::Def(def) => def,
            _ => return None,
        };
        if let Some(ModuleDef::Module(module)) = parent
            && !def.is_visible_from(cx.db, current)
            && module.declarations(cx.db).contains(&def)
        {
            let descr = def_descr(cx, def)?;
            let span = cx.span(InFile::new(file, segment.range));
            let mut diag = Diag::error(Some("E0603"), format!("{descr} `{}` is private", segment.name), span.clone());
            diag.span_label(span, format!("private {descr}"));
            if let Some(at) = def_span(cx, def) {
                diag.span_note(at, format!("the {descr} `{}` is defined here", segment.name));
            }
            return Some(diag);
        }
        parent = Some(def);
    }
    None
}
