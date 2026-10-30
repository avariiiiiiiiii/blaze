//! A field a struct expression or pattern names that its struct, union or variant lacks, or
//! has but not visible where it is named: E0560 (`S { x: 0 }`), E0559 (`E::V { x: 0 }`),
//! E0026 (`S { x }` as a pattern, the fields of a pattern in one error), and E0451 (a private
//! field named, the private fields of one expression or pattern in one error, given by
//! rustc's privacy pass). Also what its siblings share of a struct expression or pattern:
//! [`Record`], whichever syntax wrote it (an expression on the left of `=` is a pattern).
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_struct_fields`,
//! `report_unknown_field`, `available_field_names`, `name_series_display`),
//! `compiler/rustc_hir_typeck/src/pat.rs` (`check_struct_pat_fields`, `error_inexistent_fields`,
//! `error_tuple_variant_as_struct_pat`), `compiler/rustc_hir_typeck/src/fn_ctxt/mod.rs`
//! (`is_field_suggestable`), `compiler/rustc_privacy/src/lib.rs` (`NamePrivacyVisitor`,
//! `check_expanded_fields`, `emit_unreachable_field_error`), `compiler/rustc_privacy/src/
//! diagnostics.rs` (`FieldIsPrivate`) and `compiler/rustc_ast_lowering/src/expr.rs`
//! (`destructure_assign_mut`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use either::Either;
use hir::{HasAttrs as _, HasVisibility as _, HirFileId, InFile};
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange,
    ast::{self, HasName as _},
};

use crate::{
    cx::{Cx, item_range},
    lints::code_fact,
    lower::{
        Found, Phase,
        unresolved_field::{find_best_match_for_name, name_series_display},
    },
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::NoSuchField) -> Vec<Found> {
    let node = cx.node(d.field);
    let Some(record) = Record::of_field(node.file_id, &node.value) else { return vec![] };
    let Some(at) = record.fields.iter().position(|it| it.range == field_range(&node.value)) else {
        return vec![];
    };
    let variant = hir::Variant::from(d.variant);
    let found = match d.private {
        Some(_) => private_fields(cx, &record, at, variant),
        None if record.is_pat => inexistent_fields(cx, &record, at, variant),
        None => unknown_field(cx, &record, at, variant),
    };
    let Some((mut diag, phase)) = found else { return vec![] };
    code_fact(cx, &mut diag, InFile::new(record.file, syntax::SyntaxNodePtr::new(&record.node)));
    vec![Found::new(diag, phase)]
}

/// A struct expression or pattern as rustc reads it: `S { a: 1, b, ..base }` or `S { a, .. }`,
/// written as a pattern or as an expression on the left of `=`.
pub(crate) struct Record {
    pub(crate) file: HirFileId,
    /// The `RecordExpr` or `RecordPat`.
    pub(crate) node: SyntaxNode,
    pub(crate) path: Option<ast::Path>,
    pub(crate) fields: Vec<RecordField>,
    /// `..` or `..base`, and the base when there is one.
    pub(crate) rest: Option<(TextRange, Option<TextRange>)>,
    /// A pattern, or an expression that is one (destructuring assignment).
    pub(crate) is_pat: bool,
}

/// A field of a [`Record`].
pub(crate) struct RecordField {
    pub(crate) name: String,
    /// The name, or the binding of a shorthand field: rustc's `field.ident.span`.
    pub(crate) ident: TextRange,
    /// The field without its attributes: rustc's `field.span`.
    pub(crate) range: TextRange,
    /// The value, or the pattern, after the `:`, or the shorthand binding.
    pub(crate) value: Option<SyntaxNode>,
}

impl Record {
    pub(crate) fn of_field(
        file: HirFileId,
        field: &Either<ast::RecordExprField, ast::RecordPatField>,
    ) -> Option<Record> {
        match field {
            Either::Left(it) => Record::of(file, Either::Left(it.parent_record_lit())),
            Either::Right(it) => Record::of(file, Either::Right(it.parent_record_pat())),
        }
    }

    pub(crate) fn of(file: HirFileId, record: Either<ast::RecordExpr, ast::RecordPat>) -> Option<Record> {
        Some(match record {
            Either::Left(expr) => {
                let list = expr.record_expr_field_list()?;
                let fields = list
                    .fields()
                    .filter_map(|field| {
                        let name = field.field_name()?;
                        Some(RecordField {
                            name: name.text().to_string(),
                            ident: name.syntax().text_range(),
                            range: item_range(field.syntax()),
                            value: field.expr().map(|it| it.syntax().clone()),
                        })
                    })
                    .collect();
                let rest = list.dotdot_token().map(|dots| {
                    let base = list.spread().map(|it| it.syntax().text_range());
                    let end = base.map_or(dots.text_range().end(), |it| it.end());
                    (TextRange::new(dots.text_range().start(), end), base)
                });
                Record {
                    file,
                    is_pat: is_destructuring(expr.syntax()),
                    node: expr.syntax().clone(),
                    path: expr.path(),
                    fields,
                    rest,
                }
            }
            Either::Right(pat) => {
                let list = pat.record_pat_field_list()?;
                let fields = list
                    .fields()
                    .filter_map(|field| {
                        let name = field.field_name()?;
                        Some(RecordField {
                            name: name.to_string(),
                            ident: name.syntax().text_range(),
                            range: item_range(field.syntax()),
                            value: field.pat().map(|it| it.syntax().clone()),
                        })
                    })
                    .collect();
                let rest = list.rest_pat().map(|it| (it.syntax().text_range(), None));
                Record { file, is_pat: true, node: pat.syntax().clone(), path: pat.path(), fields, rest }
            }
        })
    }

    pub(crate) fn span(&self, cx: &Cx<'_, '_>, range: TextRange) -> Span {
        cx.span(InFile::new(self.file, range))
    }

    /// rustc's `qpath.span()`, or the whole when there is no path.
    pub(crate) fn path_range(&self) -> TextRange {
        self.path.as_ref().map_or(self.node.text_range(), |it| it.syntax().text_range())
    }

    pub(crate) fn module(&self, cx: &Cx<'_, '_>) -> Option<hir::Module> {
        cx.sema.scope(&self.node).map(|it| it.module())
    }

    /// The type of the expression or pattern, when the front end has it.
    pub(crate) fn ty<'db>(&self, cx: &Cx<'_, 'db>) -> Option<hir::Type<'db>> {
        let info = match ast::Expr::cast(self.node.clone()) {
            Some(expr) => cx.sema.type_of_expr(&expr),
            None => cx.sema.type_of_pat(&ast::Pat::cast(self.node.clone())?),
        };
        info.map(|it| it.original)
    }

    /// The names of the fields as rustc keeps them in `used_fields` (a repeated one once), with
    /// whether each is a repeat.
    pub(crate) fn repeats(&self) -> Vec<bool> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.fields.len());
        self.fields
            .iter()
            .map(|it| {
                let repeat = seen.contains(&it.name.as_str());
                if !repeat {
                    seen.push(&it.name);
                }
                repeat
            })
            .collect()
    }
}

/// The range of a field's node without its attributes, as [`RecordField::range`] has it.
fn field_range(field: &Either<ast::RecordExprField, ast::RecordPatField>) -> TextRange {
    match field {
        Either::Left(it) => item_range(it.syntax()),
        Either::Right(it) => item_range(it.syntax()),
    }
}

/// Whether `expr` is on the left of `=`, inside the tuples, arrays, tuple struct calls and
/// struct expressions there, which rustc lowers to a pattern (`destructure_assign`).
pub(crate) fn is_destructuring(expr: &SyntaxNode) -> bool {
    let mut node = expr.clone();
    while let Some(parent) = node.parent() {
        match parent.kind() {
            SyntaxKind::RECORD_EXPR_FIELD
            | SyntaxKind::RECORD_EXPR_FIELD_LIST
            | SyntaxKind::RECORD_EXPR
            | SyntaxKind::TUPLE_EXPR
            | SyntaxKind::ARRAY_EXPR
            | SyntaxKind::PAREN_EXPR
            | SyntaxKind::ARG_LIST => {}
            SyntaxKind::CALL_EXPR if node.kind() == SyntaxKind::ARG_LIST => {}
            SyntaxKind::BIN_EXPR => {
                let Some(bin) = ast::BinExpr::cast(parent) else { return false };
                return matches!(bin.op_kind(), Some(ast::BinaryOp::Assignment { op: None }))
                    && bin.lhs().is_some_and(|lhs| lhs.syntax() == &node);
            }
            _ => return false,
        }
        node = parent;
    }
    false
}

/// rustc's `AdtDef::variant_descr`.
pub(crate) fn variant_descr(variant: hir::Variant) -> &'static str {
    match variant {
        hir::Variant::Struct(_) => "struct",
        hir::Variant::Union(_) => "union",
        hir::Variant::EnumVariant(_) => "variant",
    }
}

/// rustc's `def_path_str` of a variant's `DefId`: `S`, or `E::V` of an enum's.
pub(crate) fn variant_path(cx: &Cx<'_, '_>, variant: hir::Variant) -> String {
    match variant {
        hir::Variant::EnumVariant(it) => cx.def_path(hir::ModuleDef::EnumVariant(it)),
        _ => cx.def_path(hir::ModuleDef::Adt(variant.adt(cx.db))),
    }
}

/// rustc's `is_field_suggestable`: visible from `module`, and of this crate or neither
/// `#[doc(hidden)]` nor unstable.
pub(crate) fn suggestable(cx: &Cx<'_, '_>, field: hir::Field, module: Option<hir::Module>) -> bool {
    let visible = module.is_none_or(|module| field.is_visible_from(cx.db, module));
    let local = field.parent_def(cx.db).module(cx.db).krate(cx.db) == cx.krate;
    let attrs = field.attrs(cx.db);
    visible && (local || (!attrs.is_doc_hidden() && !attrs.is_unstable()))
}

/// rustc's `listify` of names: "`a`", "`a` and `b`", "`a`, `b` and `c`".
pub(crate) fn listify(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => format!("`{only}`"),
        [others @ .., last] => {
            let others: Vec<String> = others.iter().map(|it| format!("`{it}`")).collect();
            format!("{} and `{last}`", others.join(", "))
        }
    }
}

fn adt_fact(cx: &Cx<'_, '_>, diag: &mut Diag, variant: hir::Variant) {
    let adt = variant.adt(cx.db);
    let name = variant_path(cx, variant);
    diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the fields of `{name}`"));
}

/// E0560 and E0559 (`report_unknown_field`).
fn unknown_field(cx: &Cx<'_, '_>, record: &Record, at: usize, variant: hir::Variant) -> Option<(Diag, Phase)> {
    let field = &record.fields[at];
    let ident = record.span(cx, field.ident);
    let kind_name = variant_descr(variant);
    let adt = variant.adt(cx.db);
    let ty = match record.ty(cx) {
        Some(ty) if !ty.contains_unknown() => cx.ty(&ty),
        _ => cx.def_path(hir::ModuleDef::Adt(adt)),
    };
    let variant_name = variant.name(cx.db).display(cx.db, cx.edition).to_string();
    let name = &field.name;
    let mut diag = match variant {
        hir::Variant::EnumVariant(_) => Diag::error(
            Some("E0559"),
            format!("{kind_name} `{ty}::{variant_name}` has no field named `{name}`"),
            ident.clone(),
        ),
        _ => Diag::error(Some("E0560"), format!("{kind_name} `{ty}` has no field named `{name}`"), ident.clone()),
    };
    let tuple = match variant {
        hir::Variant::Struct(it) => it.kind(cx.db) == hir::StructKind::Tuple,
        hir::Variant::EnumVariant(it) => it.kind(cx.db) == hir::StructKind::Tuple,
        hir::Variant::Union(_) => false,
    };
    if tuple {
        let inputs: Vec<String> =
            variant.fields(cx.db).iter().map(|it| format!("/* {} */", cx.ty(&it.ty(cx.db)))).collect();
        let fields = format!("({})", inputs.join(", "));
        let ident_span = variant_ident_span(cx, variant);
        let whole = record.span(cx, record.node.text_range());
        match variant {
            hir::Variant::EnumVariant(_) => {
                if let Some(span) = ident_span {
                    diag.span_label(span, format!("`{ty}::{variant_name}` defined here"));
                }
                diag.span_label(ident, "field does not exist");
                let from = record.path_range().end();
                diag.suggestion_verbose(
                    record.span(cx, TextRange::new(from, record.node.text_range().end())),
                    format!("`{ty}::{variant_name}` is a tuple {kind_name}, use the appropriate syntax"),
                    fields,
                    Applicability::HasPlaceholders,
                );
            }
            _ => {
                if let Some(span) = ident_span {
                    diag.span_label(span, format!("`{ty}` defined here"));
                }
                diag.span_label(ident, "field does not exist");
                diag.suggestion_verbose(
                    whole,
                    format!("`{ty}` is a tuple {kind_name}, use the appropriate syntax"),
                    format!("{ty}{fields}"),
                    Applicability::HasPlaceholders,
                );
            }
        }
    } else {
        let module = record.module(cx);
        let available: Vec<String> = variant
            .fields(cx.db)
            .into_iter()
            .filter(|it| {
                let field_name = it.name(cx.db);
                record.fields.iter().all(|skip| skip.name != field_name.as_str()) && suggestable(cx, *it, module)
            })
            .map(|it| it.name(cx.db).as_str().to_owned())
            .collect();
        let similar = find_best_match_for_name(&available, name, None)
            .filter(|it| !(name.parse::<usize>().is_ok() && it.parse::<usize>().is_ok()));
        if let Some(similar) = similar {
            diag.span_label(ident.clone(), "unknown field");
            diag.suggestion_verbose(ident, "a field with a similar name exists", similar, Applicability::MaybeIncorrect);
        } else {
            match variant {
                hir::Variant::EnumVariant(_) => {
                    diag.span_label(ident, format!("`{ty}::{variant_name}` does not have this field"))
                }
                _ => diag.span_label(ident, format!("`{ty}` does not have this field")),
            };
            match available.is_empty() {
                true => diag.note("all struct fields are already assigned"),
                false => diag.note(format!("available fields are: {}", name_series_display(&available))),
            };
        }
    }
    adt_fact(cx, &mut diag, variant);
    Some((diag, Phase::Bodies))
}

/// rustc's `def_ident_span` of a variant: the name of the struct, union or enum variant.
fn variant_ident_span(cx: &Cx<'_, '_>, variant: hir::Variant) -> Option<Span> {
    let name = match variant {
        hir::Variant::Struct(it) => cx.sema.source(it).and_then(|it| it.map(|it| it.name()).transpose()),
        hir::Variant::Union(it) => cx.sema.source(it).and_then(|it| it.map(|it| it.name()).transpose()),
        hir::Variant::EnumVariant(it) => cx.sema.source(it).and_then(|it| it.map(|it| it.name()).transpose()),
    }?;
    Some(cx.span(name.map(|it| it.syntax().text_range())))
}

/// What rustc's `check_struct_pat_fields` finds of a pattern's fields: those its variant lacks
/// (`inexistent_fields`), and those of the variant it does not mention, less one E0026
/// suggests in place of a field it lacks.
pub(crate) struct PatFields {
    /// Indices into [`Record::fields`].
    pub(crate) inexistent: Vec<usize>,
    pub(crate) unmentioned: Vec<hir::Field>,
    /// The name E0026 suggests for its last field.
    pub(crate) similar: Option<String>,
}

pub(crate) fn pat_fields(cx: &Cx<'_, '_>, record: &Record, variant: hir::Variant) -> PatFields {
    let fields = variant.fields(cx.db);
    let names: Vec<hir::Name> = fields.iter().map(|it| it.name(cx.db)).collect();
    let repeats = record.repeats();
    let inexistent: Vec<usize> = (0..record.fields.len())
        .filter(|&i| !repeats[i] && !names.iter().any(|it| it.as_str() == record.fields[i].name))
        .collect();
    let mut unmentioned: Vec<hir::Field> = fields
        .iter()
        .zip(&names)
        .filter(|(_, name)| record.fields.iter().all(|it| it.name != name.as_str()))
        .map(|(field, _)| *field)
        .collect();
    let reported = !inexistent.is_empty() && inexistent.iter().all(|&i| record.fields[i].name != "_");
    let mut similar = None;
    if reported
        && let Some(&last) = inexistent.last()
        && let [only] = unmentioned.as_slice()
        && suggestable(cx, *only, record.module(cx))
    {
        let candidate = only.name(cx.db).as_str().to_owned();
        similar = find_best_match_for_name(std::slice::from_ref(&candidate), &record.fields[last].name, None);
        if similar.as_ref().is_some_and(|it| it.parse::<usize>().is_err()) {
            unmentioned.clear();
        }
    }
    PatFields { inexistent: if reported { inexistent } else { Vec::new() }, unmentioned, similar }
}

/// Whether rustc reports `pattern does not mention ...` (or its form for inaccessible fields)
/// of the pattern: a struct's or a variant's, without `..`, not mentioning a field.
pub(crate) fn has_unmentioned_error(record: &Record, variant: hir::Variant, found: &PatFields) -> bool {
    !matches!(variant, hir::Variant::Union(_)) && !found.unmentioned.is_empty() && record.rest.is_none()
}

/// E0769 (`error_tuple_variant_as_struct_pat`): a tuple struct or variant's pattern written
/// with names; rustc gives it in place of E0026 and E0027 when there is an E0027 to give.
pub(crate) fn tuple_variant_as_struct(cx: &Cx<'_, '_>, record: &Record, variant: hir::Variant) -> Option<Diag> {
    let tuple = match variant {
        hir::Variant::Struct(it) => it.kind(cx.db) == hir::StructKind::Tuple,
        hir::Variant::EnumVariant(it) => it.kind(cx.db) == hir::StructKind::Tuple,
        hir::Variant::Union(_) => false,
    };
    if !tuple {
        return None;
    }
    let numbered = !record.fields.is_empty() && record.fields.iter().all(|it| is_number(&it.name));
    if numbered {
        return None;
    }
    let path = record.path.as_ref()?;
    let written: String = path.syntax().text().to_string().split_whitespace().collect();
    let whole = record.span(cx, record.node.text_range());
    let mut diag = Diag::error(Some("E0769"), format!("tuple variant `{written}` written as struct variant"), whole);
    let fields = variant.fields(cx.db);
    let (sugg, applicability) = if record.fields.len() == fields.len() {
        let names: Vec<hir::Name> = fields.iter().map(|it| it.name(cx.db)).collect();
        let parts: Vec<String> = record
            .fields
            .iter()
            .map(|field| match names.iter().any(|it| it.as_str() == field.name) {
                true => "_".to_owned(),
                false => match field.value.as_ref().and_then(|it| cx.snippet(&record.span(cx, it.text_range()))) {
                    Some(text) => text.to_owned(),
                    None => field.value.as_ref().map_or_else(|| field.name.clone(), |it| it.text().to_string()),
                },
            })
            .collect();
        (parts.join(", "), Applicability::MachineApplicable)
    } else {
        (vec!["_"; fields.len()].join(", "), Applicability::MaybeIncorrect)
    };
    let from = path.syntax().text_range().end();
    diag.suggestion_verbose(
        record.span(cx, TextRange::new(from, record.node.text_range().end())),
        "use the tuple variant pattern syntax instead",
        format!("({sugg})"),
        applicability,
    );
    adt_fact(cx, &mut diag, variant);
    Some(diag)
}

pub(crate) fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|it| it.is_ascii_digit())
}

/// E0026 (`error_inexistent_fields`), once for a pattern's fields, at its first.
fn inexistent_fields(cx: &Cx<'_, '_>, record: &Record, at: usize, variant: hir::Variant) -> Option<(Diag, Phase)> {
    let found = pat_fields(cx, record, variant);
    if found.inexistent.first() != Some(&at) {
        return None;
    }
    if has_unmentioned_error(record, variant, &found) && tuple_variant_as_struct(cx, record, variant).is_some() {
        return None;
    }
    let inexistent: Vec<&RecordField> = found.inexistent.iter().map(|&i| &record.fields[i]).collect();
    let (field_names, this, plural) = match inexistent.as_slice() {
        [only] => (format!("a field named `{}`", only.name), "this", ""),
        many => {
            let names: Vec<String> = many.iter().map(|it| format!("`{}`", it.name)).collect();
            (format!("fields named {}", names.join(", ")), "these", "s")
        }
    };
    let kind_name = variant_descr(variant);
    let path = variant_path(cx, variant);
    let mut spans = inexistent.iter().map(|it| record.span(cx, it.ident));
    let first = spans.next()?;
    let mut diag = Diag::error(Some("E0026"), format!("{kind_name} `{path}` does not have {field_names}"), first);
    for span in spans {
        diag.primary(span);
    }
    let last = inexistent.last()?;
    let last_span = record.span(cx, last.ident);
    diag.span_label(last_span.clone(), format!("{kind_name} `{path}` does not have {this} field{plural}"));
    let module = record.module(cx);
    let unmentioned = unmentioned_before_suggestion(cx, record, variant);
    if let [only] = unmentioned.as_slice()
        && suggestable(cx, *only, module)
    {
        let name = only.name(cx.db).as_str().to_owned();
        if let Some(similar) = &found.similar {
            diag.suggestion_verbose(
                last_span,
                "a field with a similar name exists",
                similar.clone(),
                Applicability::MaybeIncorrect,
            );
        } else if inexistent.len() == 1 && coerces(cx, record, last, *only) {
            diag.suggestion_with_style(
                vec![(last_span, name.clone())],
                format!("`{path}` has a field named `{name}`"),
                Applicability::MaybeIncorrect,
                SuggestionStyle::HideCodeInline,
            );
        }
    }
    adt_fact(cx, &mut diag, variant);
    Some((diag, Phase::Bodies))
}

/// The fields of the variant the pattern does not mention, before E0026 takes one as the one
/// meant.
fn unmentioned_before_suggestion(cx: &Cx<'_, '_>, record: &Record, variant: hir::Variant) -> Vec<hir::Field> {
    variant
        .fields(cx.db)
        .into_iter()
        .filter(|field| {
            let name = field.name(cx.db);
            record.fields.iter().all(|it| it.name != name.as_str())
        })
        .collect()
}

/// rustc's test before "`S` has a field named `x`": a field whose pattern is a literal or a
/// constant (`PatKind::Expr`) must have a type that coerces to the other field's.
fn coerces(cx: &Cx<'_, '_>, record: &Record, field: &RecordField, other: hir::Field) -> bool {
    let Some(value) = &field.value else { return true };
    let pat = match ast::Pat::cast(value.clone()) {
        Some(pat @ (ast::Pat::LiteralPat(_) | ast::Pat::PathPat(_) | ast::Pat::ConstBlockPat(_))) => pat,
        _ => return true,
    };
    let (Some(pat_ty), Some(record_ty)) = (cx.sema.type_of_pat(&pat), record.ty(cx)) else { return true };
    let Some((_, field_ty)) = record_ty.fields(cx.db).into_iter().find(|(it, _)| *it == other) else {
        return true;
    };
    pat_ty.original.could_coerce_to(cx.db, &field_ty)
}

/// E0451 (`emit_unreachable_field_error`), once for an expression's or a pattern's private
/// fields, at the first.
fn private_fields(cx: &Cx<'_, '_>, record: &Record, at: usize, variant: hir::Variant) -> Option<(Diag, Phase)> {
    let adt = variant.adt(cx.db);
    if matches!(adt, hir::Adt::Enum(_)) {
        return None;
    }
    let module = record.module(cx)?;
    let fields = variant.fields(cx.db);
    let private = |name: &str| {
        fields.iter().find(|it| it.name(cx.db).as_str() == name).filter(|it| !it.is_visible_from(cx.db, module))
    };
    let expanded = record.rest.filter(|_| !record.is_pat);
    // (name, range, present, field)
    let mut failed: Vec<(String, TextRange, bool, hir::Field)> = Vec::new();
    match expanded {
        Some((rest, base)) => {
            for field in &fields {
                if field.is_visible_from(cx.db, module) {
                    continue;
                }
                let name = field.name(cx.db).as_str().to_owned();
                match record.fields.iter().find(|it| it.name == name) {
                    Some(present) => failed.push((name, present.range, true, *field)),
                    None => failed.push((name, base.unwrap_or(rest), false, *field)),
                }
            }
        }
        None => {
            for (field, repeat) in record.fields.iter().zip(record.repeats()) {
                if let Some(def) = private(&field.name).filter(|_| !repeat) {
                    failed.push((field.name.clone(), field.ident, true, *def));
                }
            }
        }
    }
    let first_present = failed.iter().find(|it| it.2)?;
    if first_present.1 != record.fields[at].ident && first_present.1 != record.fields[at].range {
        return None;
    }
    let names: Vec<&str> = failed.iter().map(|it| it.0.as_str()).collect();
    let (field_s, is_are) = if failed.len() == 1 { ("field", "is") } else { ("fields", "are") };
    let descr = variant_descr(variant);
    let path = cx.def_path(hir::ModuleDef::Adt(adt));
    let message = format!("{field_s} {} of {descr} `{path}` {is_are} private", listify(&names));
    let mut spans = failed.iter().map(|it| record.span(cx, it.1));
    let mut diag = Diag::error(Some("E0451"), message, spans.next()?);
    for span in spans {
        if !diag.span.primary.contains(&span) {
            diag.primary(span);
        }
    }
    let path_range = record.path_range();
    let (lo, hi) = match failed[0].1.start() >= path_range.end() {
        true => (path_range.end(), failed[0].1.start()),
        false => (failed[0].1.end(), path_range.start()),
    };
    let between = cx.snippet(&record.span(cx, TextRange::new(lo, hi.max(lo))));
    if between.is_some_and(|it| it.contains('\n')) {
        diag.span_label(record.span(cx, path_range), "in this type");
    }
    for (_, range, present, _) in &failed {
        if *present {
            diag.span_label(record.span(cx, *range), "private field");
        }
    }
    if let Some((rest, base)) = expanded {
        let rest_names: Vec<&str> = failed.iter().filter(|it| !it.2).map(|it| it.0.as_str()).collect();
        if !rest_names.is_empty() {
            let (field_s, is_are) = if rest_names.len() == 1 { ("field", "is") } else { ("fields", "are") };
            diag.span_label(
                record.span(cx, base.unwrap_or(rest)),
                format!("{field_s} {} {is_are} private", listify(&rest_names)),
            );
        }
    }
    let adt_name = adt.name(cx.db).display(cx.db, cx.edition).to_string();
    for (name, _, _, field) in &failed {
        diag.fact(
            Fact::Field { ty: cx.key(ItemId::Adt(adt.into())), field: field.index() as u32 },
            format!("the visibility of `{adt_name}::{name}`"),
        );
    }
    Some((diag, Phase::Late))
}
