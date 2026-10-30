//! E0124: a field of a struct, union or enum variant declared twice.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/collect.rs` (`FieldUniquenessCheckContext`,
//! `lower_variant`) and its `FieldAlreadyDeclared::NotNested` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::{FieldSource, HasSource as _};
use rustc_hash::FxHashMap;
use syntax::AstNode;

use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase},
    model::{Diag, Span},
};

pub(super) fn check(cx: &Cx<'_, '_>, adts: &[hir::Adt]) -> Vec<Found> {
    let mut found = Vec::new();
    for &adt in adts {
        let variants: Vec<hir::Variant> = match adt {
            hir::Adt::Struct(it) => vec![it.into()],
            hir::Adt::Union(it) => vec![it.into()],
            hir::Adt::Enum(it) => it.variants(cx.db).into_iter().map(Into::into).collect(),
        };
        for variant in variants {
            found.extend(check_variant(cx, adt, variant));
        }
    }
    found
}

/// Each named field of `variant` whose name an earlier field has.
fn check_variant(cx: &Cx<'_, '_>, adt: hir::Adt, variant: hir::Variant) -> Vec<Found> {
    let mut seen: FxHashMap<String, Span> = FxHashMap::default();
    let mut found = Vec::new();
    for field in variant.fields(cx.db) {
        let Some(source) = field.source(cx.db) else { continue };
        let FieldSource::Named(node) = &source.value else { continue };
        let span = cx.span(source.with_value(item_range(node.syntax())));
        let name = field.name(cx.db).as_str().to_owned();
        let Some(prev) = seen.get(&name) else {
            seen.insert(name, span);
            continue;
        };
        let mut diag =
            Diag::error(Some("E0124"), format!("field `{name}` is already declared"), span.clone());
        diag.span_label(span, "field already declared");
        diag.span_label(prev.clone(), format!("`{name}` first declared here"));
        let what = format!("the fields of `{}`", cx.def_path(hir::ModuleDef::Adt(adt)));
        diag.fact(Fact::Layout(cx.key(ItemId::Adt(adt.into()))), what);
        found.push(Found::new(diag, Phase::Items));
    }
    found
}
