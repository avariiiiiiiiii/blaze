//! E0618 "expected function, found `u32`": a call of something that is not callable, on the
//! callee, with "call expression requires function" on the call, the label on what the callee
//! names ("`x` has type `u32`", "`S` defined here"), the parentheses a unit struct or variant
//! does not take, the semicolon a call across lines was missing, the comma between array
//! elements, and the function a local shadows.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/callee.rs` (`report_invalid_callee`,
//! `maybe_suggest_bad_array_definition`) (MIT or Apache-2.0).

use hir::{HasSource as _, InFile, PathResolution};
use syntax::{
    AstNode, TextRange,
    ast::{self, HasArgList as _},
};

use super::binary_operator_cannot_be_applied::code_fact;
use crate::{
    cx::{Cx, def_range, signature_range},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::ExpectedFunction<'db>) -> Vec<Found> {
    let node = cx.node(d.call);
    let file = node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let call_span = cx.span_of_ptr(d.call);
    let Some(ast::Expr::CallExpr(call)) = node.value.clone().left() else {
        let ty = cx.ty(&d.found);
        let mut diag = Diag::error(Some("E0618"), format!("expected function, found `{ty}`"), call_span.clone());
        diag.span_label(call_span, "call expression requires function");
        return vec![Found::new(diag, Phase::Bodies)];
    };
    let callee = call.expr();
    let callee_range = callee.as_ref().map_or(call.syntax().text_range(), |it| it.syntax().text_range());
    let callee_span = at(callee_range);
    let args = call.arg_list().map(|it| it.args().count()).unwrap_or(0);
    let ty = cx.ty(&d.found);

    let callee_path = match &callee {
        Some(ast::Expr::PathExpr(it)) => it.path(),
        _ => None,
    };
    let inner_path = match &callee {
        Some(ast::Expr::CallExpr(inner)) => match inner.expr() {
            Some(ast::Expr::PathExpr(it)) => it.path(),
            _ => None,
        },
        _ => None,
    };
    let resolution = callee_path.as_ref().or(inner_path.as_ref()).and_then(|it| cx.sema.resolve_path(it));

    let unit = callee_path.as_ref().zip(resolution.as_ref()).and_then(|(path, res)| {
        let kind = match res {
            PathResolution::Def(hir::ModuleDef::Adt(hir::Adt::Struct(it)))
                if it.kind(cx.db) == hir::StructKind::Unit =>
            {
                "struct"
            }
            PathResolution::Def(hir::ModuleDef::EnumVariant(it)) if it.kind(cx.db) == hir::StructKind::Unit => {
                "enum variant"
            }
            _ => return None,
        };
        (args == 0).then(|| (kind, path_text(path)))
    });

    let found = match &unit {
        Some((kind, path)) => format!("{kind} `{path}`"),
        None => format!("`{ty}`"),
    };
    let mut diag = Diag::error(Some("E0618"), format!("expected function, found {found}"), callee_span.clone());

    if let Some((kind, path)) = &unit {
        diag.suggestion_verbose(
            at(TextRange::new(callee_range.end(), call.syntax().text_range().end())),
            format!("`{path}` is a unit {kind}, and does not take parentheses to be constructed"),
            "",
            Applicability::MachineApplicable,
        );
    }

    if let (Some(path), Some(PathResolution::Local(_))) = (&callee_path, &resolution)
        && path.qualifier().is_none()
        && let Some(name) = path.segment().and_then(|it| it.name_ref())
    {
        shadowed_functions(cx, name.text().as_str(), &mut diag);
    }

    if !bad_array(&call, &callee, &callee_span, &mut diag) {
        let rest = call.syntax().to_string();
        let start = usize::from(callee_range.end() - call.syntax().text_range().start());
        if rest.get(start..).is_some_and(|it| it.contains('\n')) {
            diag.suggestion(
                callee_span.shrink_to_hi(),
                "consider using a semicolon here to finish the statement",
                ";",
                Applicability::MaybeIncorrect,
            );
        }
        diag.span_label(call_span, "call expression requires function");
    }

    if let Some(res) = &resolution
        && let Some((span, label)) = defined_here(cx, res, &unit, inner_path.as_ref(), &ty)
    {
        diag.span_label(span, label);
    }
    code_fact(cx, d.call.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `qpath_to_string` of a path as written: its segments, without whitespace.
fn path_text(path: &ast::Path) -> String {
    path.syntax().text().to_string().split_whitespace().collect()
}

/// rustc's label on what the callee resolves to (`hir_res_span`).
fn defined_here<'db>(
    cx: &Cx<'_, 'db>,
    res: &PathResolution<'db>,
    unit: &Option<(&str, String)>,
    inner_path: Option<&ast::Path>,
    ty: &str,
) -> Option<(crate::model::Span, String)> {
    let span = match res {
        PathResolution::Local(local) => {
            let source = local.primary_source(cx.db);
            cx.span(source.source.map(|it| it.syntax().text_range()))
        }
        PathResolution::Def(def) => {
            let node = match def {
                hir::ModuleDef::EnumVariant(it) => it.source(cx.db)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Adt(hir::Adt::Struct(it)) => it.source(cx.db)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Adt(hir::Adt::Union(it)) => it.source(cx.db)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Adt(hir::Adt::Enum(it)) => it.source(cx.db)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Function(it) => it.source(cx.db)?.map(|it| signature_range(&it)),
                hir::ModuleDef::Const(it) => it.source(cx.db)?.map(|it| def_range(it.syntax())),
                hir::ModuleDef::Static(it) => it.source(cx.db)?.map(|it| def_range(it.syntax())),
                _ => return None,
            };
            cx.span(node)
        }
        _ => return None,
    };
    let label = if let Some((kind, path)) = unit {
        format!("{kind} `{path}` defined here")
    } else if let Some(inner) = inner_path {
        format!("`{}` defined here returns `{ty}`", path_text(inner))
    } else {
        match res {
            PathResolution::Local(local) => {
                format!("`{}` has type `{ty}`", local.name(cx.db).display(cx.db, cx.edition))
            }
            PathResolution::Def(
                def @ (hir::ModuleDef::EnumVariant(_)
                | hir::ModuleDef::Adt(hir::Adt::Struct(_))
                | hir::ModuleDef::Function(_)
                | hir::ModuleDef::Const(_)
                | hir::ModuleDef::Static(_)),
            ) => format!("`{}` defined here", cx.def_path(*def)),
            _ => format!("`{ty}` defined here"),
        }
    };
    Some((span, label))
}

/// rustc's label on each free function with the name of the local the callee is.
fn shadowed_functions(cx: &Cx<'_, '_>, name: &str, diag: &mut Diag) {
    for module in cx.krate.modules(cx.db) {
        for decl in module.declarations(cx.db) {
            let hir::ModuleDef::Function(function) = decl else { continue };
            if function.name(cx.db).as_str() != name {
                continue;
            }
            if let Some(source) = function.source(cx.db) {
                diag.span_label(
                    cx.span(source.map(|it| signature_range(&it))),
                    "this function of the same name is available here, but it's shadowed by the local binding",
                );
            }
        }
    }
}

/// rustc's `maybe_suggest_bad_array_definition`: `[(1, 2) (3, 4)]`, a tuple called with as many
/// arguments as it has elements, inside an array.
fn bad_array(
    call: &ast::CallExpr,
    callee: &Option<ast::Expr>,
    callee_span: &crate::model::Span,
    diag: &mut Diag,
) -> bool {
    let Some(ast::Expr::TupleExpr(tuple)) = callee else { return false };
    let in_array = call.syntax().parent().is_some_and(|it| ast::ArrayExpr::can_cast(it.kind()));
    let args = call.arg_list().map(|it| it.args().count()).unwrap_or(0);
    if !in_array || args != tuple.fields().count() {
        return false;
    }
    diag.suggestion(
        callee_span.shrink_to_hi(),
        "consider separating array elements with a comma",
        ",",
        Applicability::MaybeIncorrect,
    );
    true
}
