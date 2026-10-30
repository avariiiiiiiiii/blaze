//! E0004: a `match` that does not cover every value of its scrutinee.
//!
//! Follows rustc's `compiler/rustc_mir_build/src/thir/pattern/check_match.rs`
//! (`report_non_exhaustive_match`, `joined_uncovered_patterns`, `collect_special_tys`,
//! `report_adt_defined_here`, `maybe_point_at_variant`, `is_const_pat_that_looks_like_binding`),
//! `compiler/rustc_mir_build/src/diagnostics.rs` (`NonExhaustivePatternsTypeNotEmpty`,
//! `AdtDefinedHere`) and the printing of a witness in `compiler/rustc_pattern_analysis/src/
//! rustc.rs` (`print_witness_pat`) and `rustc/print.rs` (`write_struct_like`) (MIT or
//! Apache-2.0). hir gives the witnesses as rust-analyzer prints them (`hir-ide`'s
//! `missing_match_arms`): they are read back here against the scrutinee's type, and printed as
//! rustc prints them.

use blaze_items::{Fact, ItemId};
use hir::{Adt, HasSource, InFile, ModuleDef, StructKind, Type};
use hir_def::{Lookup as _, attrs::AttrFlags};
use syntax::{AstNode, SyntaxKind, SyntaxNode, TextRange, ast, ast::HasName};

use crate::{
    cx::{Cx, def_range, item_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::MissingMatchArms) -> Vec<Found> {
    let scrut = cx.node(d.scrutinee_expr);
    let sp = cx.span_of_ptr(d.scrutinee_expr);
    let ty = cx.sema.type_of_expr(&scrut.value).map(|it| it.adjusted());
    let match_expr = scrut.value.syntax().parent().and_then(ast::MatchExpr::cast);
    let arms: Vec<ast::MatchArm> =
        match_expr.as_ref().and_then(|it| it.match_arm_list()).map_or_else(Vec::new, |it| it.arms().collect());
    let braces = match_expr.as_ref().and_then(|it| braces_span(cx, &sp, scrut.with_value(it.syntax().text_range())));
    let text = cx.file_of(&sp).map(|it| cx.file_text(it));
    let mut diag = match (Uncovered::read(&d.uncovered_patterns), &ty) {
        (Some(Uncovered::NonEmpty), Some(ty)) => type_not_empty(cx, &sp, ty, braces, text),
        (Some(Uncovered::Witnesses { shown, count }), _) => {
            let witnesses = Witnesses::read(cx, &shown, count, ty.as_ref());
            let arms = Arms { file: scrut.file_id, arms: &arms, braces, text };
            not_covered(cx, &sp, ty.as_ref(), &witnesses, arms)
        }
        _ => {
            let mut diag =
                Diag::error(Some("E0004"), format!("non-exhaustive patterns: {}", d.uncovered_patterns), sp);
            if let Some(ty) = &ty {
                diag.note(format!("the matched value is of type `{}`", cx.ty(ty)));
            }
            diag
        }
    };
    if let Some(ty) = &ty {
        adt_fact(cx, &mut diag, ty);
    }
    code_fact(cx, &mut diag, d.scrutinee_expr.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Mir)]
}

/// rustc's `NonExhaustivePatternsTypeNotEmpty`: an empty match on a type that is not an enum
/// with variants.
fn type_not_empty<'db>(
    cx: &Cx<'_, 'db>,
    sp: &Span,
    ty: &Type<'db>,
    braces: Option<Span>,
    text: Option<&str>,
) -> Diag {
    let printed = cx.ty(ty);
    let mut diag =
        Diag::error(Some("E0004"), format!("non-exhaustive patterns: type `{printed}` is non-empty"), sp.clone());
    let peeled = ty.strip_references();
    if let Some(adt) = peeled.as_adt()
        && let Some(at) = adt_span(cx, adt, true)
    {
        let mut span = MultiSpan::of(at.clone());
        span.labels.push((at, String::new()));
        diag.span_note(span, format!("`{}` defined here", cx.ty(&peeled)));
    }
    match foreign_non_exhaustive(cx, ty) {
        true => diag.note(format!("the matched value is of type `{printed}`, which is marked as non-exhaustive")),
        false => diag.note(format!("the matched value is of type `{printed}`")),
    };
    match braces {
        Some(braces) => {
            let body = match text.map(|it| indentation_before(it, sp.lo)) {
                Some(indent) => format!(" {{\n{indent}    _ => todo!(),\n{indent}}}"),
                None => " { _ => todo!(), }".to_owned(),
            };
            diag.suggestion_verbose(
                braces,
                "ensure that all possible cases are being handled by adding a match arm with a wildcard pattern as shown",
                body,
                Applicability::HasPlaceholders,
            );
        }
        None => {
            diag.help("ensure that all possible cases are being handled by adding a match arm with a wildcard pattern");
        }
    }
    diag
}

/// The arms of a match, where its braces are and the text of its file.
struct Arms<'s> {
    file: hir::HirFileId,
    arms: &'s [ast::MatchArm],
    braces: Option<Span>,
    text: Option<&'s str>,
}

/// rustc's `report_non_exhaustive_match` past its empty case.
fn not_covered<'db>(
    cx: &Cx<'_, 'db>,
    sp: &Span,
    ty: Option<&Type<'db>>,
    witnesses: &Witnesses<'db>,
    arms: Arms<'_>,
) -> Diag {
    let joined = witnesses.joined();
    let mut diag = Diag::error(Some("E0004"), format!("non-exhaustive patterns: {joined} not covered"), sp.clone());
    diag.span_label(sp.clone(), format!("pattern{} {joined} not covered", plural(witnesses.count)));
    if let Some(ty) = ty
        && let Some(here) = DefinedHere::of(cx, ty, &witnesses.read, true)
    {
        diag.span_note(here.span(true), here.message());
    }
    if let Some(ty) = ty {
        diag.note(format!("the matched value is of type `{}`", cx.ty(ty)));
    }
    if !arms.arms.is_empty()
        && let Some(first) = witnesses.read.first()
    {
        let mut special: Vec<Type<'db>> = Vec::new();
        first.never_types(&mut special);
        for ty in &special {
            let printed = cx.ty(ty);
            match foreign_non_exhaustive(cx, ty) {
                true => diag.note(format!(
                    "`{printed}` is marked as non-exhaustive, so a wildcard `_` is necessary to match exhaustively"
                )),
                // A witness is `!` only where the type is empty and the place may hold an
                // invalid value: what rustc's `is_uninhabited` then says of it.
                false => diag.note(format!(
                    "`{printed}` is uninhabited but is not being matched by value, so a wildcard `_` is required"
                )),
            };
        }
    }
    for arm in arms.arms {
        if let Some(ast::Pat::IdentPat(pat)) = arm.pat()
            && let Some((name, def)) = const_that_looks_like_binding(cx, &pat)
        {
            let at = cx.span(InFile::new(arms.file, pat.syntax().text_range()));
            diag.span_label(
                at.clone(),
                format!(
                    "this pattern doesn't introduce a new catch-all binding, but rather pattern matches against the value of constant `{name}`"
                ),
            );
            diag.span_note(def, format!("constant `{name}` defined here"));
            diag.suggestion_verbose(
                at.shrink_to_hi(),
                "if you meant to introduce a binding, use a different name",
                "_var",
                Applicability::MaybeIncorrect,
            );
        }
    }

    let suggest_the_witnesses = witnesses.count < 4;
    let suggested_arm = match suggest_the_witnesses {
        true => format!("{} => todo!()", witnesses.printed.join(" | ")),
        false => "_ => todo!()".to_owned(),
    };
    let suggestion = arm_suggestion(cx, sp, &arms, &suggested_arm);
    let message = format!(
        "ensure that all possible cases are being handled by adding a match arm with a wildcard pattern{}{}",
        match witnesses.count > 1 && suggest_the_witnesses && suggestion.is_some() {
            true => ", a match arm with multiple or-patterns",
            false => "",
        },
        match (witnesses.count, suggestion.is_some()) {
            (1, true) => " or an explicit pattern as shown",
            (1, false) => " or an explicit pattern",
            (_, true) => " as shown, or multiple match arms",
            (_, false) => " or multiple match arms",
        },
    );
    match suggestion {
        Some((span, code)) => {
            diag.suggestion_verbose(span, message, code, Applicability::HasPlaceholders);
        }
        None => {
            diag.help(message);
        }
    }
    diag
}

/// Where rustc suggests the missing arm, and its code: in the braces of an empty match, after
/// its only arm, or after its last when it is laid out as the one before it.
fn arm_suggestion(cx: &Cx<'_, '_>, sp: &Span, arms: &Arms<'_>, suggested_arm: &str) -> Option<(Span, String)> {
    let arm_span = |arm: &ast::MatchArm| cx.span(InFile::new(arms.file, arm_range(arm)));
    // rustc asks whether the body is a block to leave the comma out, which never holds: THIR
    // wraps every arm's body in a scope.
    match arms.arms {
        [] => {
            let braces = arms.braces.clone()?;
            let code = match arms.text.map(|it| indentation_before(it, sp.lo)) {
                Some(indent) => format!(" {{\n{indent}    {suggested_arm},\n{indent}}}"),
                None => format!(" {{ {suggested_arm}, }}"),
            };
            Some((braces, code))
        }
        [only] => {
            let only = arm_span(only);
            let pre = match arms.text {
                Some(text) if is_multiline(text, only.lo, extend_while(text, only.hi, |c| c.is_whitespace() || c == ',')) => {
                    format!("\n{}", indentation_before(text, only.lo))
                }
                _ => " ".to_owned(),
            };
            Some((only.shrink_to_hi(), format!(",{pre}{suggested_arm}")))
        }
        [.., prev, last] => {
            let (prev, last) = (arm_span(prev), arm_span(last));
            if prev.file != last.file || prev.expansion != last.expansion {
                return None;
            }
            let text = arms.text?;
            let spacing = match is_multiline(text, prev.hi, last.lo) {
                true => format!("\n{}", indentation_before(text, last.lo)),
                false => " ".to_owned(),
            };
            Some((last.shrink_to_hi(), format!(",{spacing}{suggested_arm}")))
        }
    }
}

/// rustc's span of a match arm: from its pattern, after its attributes, to the end of its body,
/// without the comma after it.
fn arm_range(arm: &ast::MatchArm) -> TextRange {
    let range = item_range(arm.syntax());
    let end = arm
        .syntax()
        .children_with_tokens()
        .filter(|it| !it.kind().is_trivia() && it.kind() != SyntaxKind::COMMA)
        .last()
        .map_or(range.end(), |it| it.text_range().end());
    TextRange::new(range.start(), end.max(range.start()))
}

/// The span after the scrutinee to the end of the match, when both are in the same place (the
/// braces with what is in them and the space before them).
fn braces_span(cx: &Cx<'_, '_>, sp: &Span, match_range: InFile<TextRange>) -> Option<Span> {
    let whole = cx.span(match_range);
    (whole.file == sp.file && whole.expansion == sp.expansion && sp.hi <= whole.hi)
        .then(|| Span { lo: sp.hi, hi: whole.hi, ..sp.clone() })
}

/// rustc's `SourceMap::indentation_before`: the white space that starts the line `at` is on.
pub(super) fn indentation_before(text: &str, at: u32) -> &str {
    let before = &text[..at as usize];
    let line = before.rsplit_once('\n').map_or(before, |(_, line)| line);
    line.split_once(|c: char| !c.is_whitespace()).map_or(line, |(indent, _)| indent)
}

/// rustc's `span_extend_while`: where the text from `hi` stops being what `f` takes.
fn extend_while(text: &str, hi: u32, f: impl Fn(char) -> bool) -> u32 {
    let rest = &text[hi as usize..];
    let len = rest.char_indices().find(|(_, c)| !f(*c)).map_or(rest.len(), |(at, _)| at);
    hi + len as u32
}

fn is_multiline(text: &str, lo: u32, hi: u32) -> bool {
    text[lo as usize..hi as usize].contains('\n')
}

pub(super) fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// rustc's `is_const_pat_that_looks_like_binding`: a plain name that is a constant, not a new
/// binding; its name and the span of its definition.
pub(super) fn const_that_looks_like_binding(cx: &Cx<'_, '_>, pat: &ast::IdentPat) -> Option<(String, Span)> {
    if pat.ref_token().is_some()
        || pat.mut_token().is_some()
        || pat.pat().is_some()
        || !all_chars(pat.syntax(), |c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let ModuleDef::Const(konst) = cx.sema.resolve_bind_pat_to_const(pat)? else { return None };
    let name = konst.name(cx.db)?.as_str().to_owned();
    let source = konst.source(cx.db)?;
    let start = item_range(source.value.syntax()).start();
    let end = source.value.name().map_or(start, |it| it.syntax().text_range().end());
    Some((name, cx.span(source.with_value(TextRange::new(start, end.max(start))))))
}

/// Whether every character of `node`'s text is one `f` takes, read from its tokens in place.
pub(super) fn all_chars(node: &SyntaxNode, f: impl Fn(char) -> bool) -> bool {
    node.text().try_for_each_chunk(|chunk| if chunk.chars().all(&f) { Ok(()) } else { Err(()) }).is_ok()
}

/// rustc's `variant_list_has_applicable_non_exhaustive`: an ADT of another crate marked
/// `#[non_exhaustive]`.
fn foreign_non_exhaustive(cx: &Cx<'_, '_>, ty: &Type<'_>) -> bool {
    ty.as_adt().is_some_and(|adt| {
        adt.module(cx.db).krate(cx.db) != cx.krate
            && AttrFlags::query(cx.db, hir_def::AdtId::from(adt).into()).contains(AttrFlags::NON_EXHAUSTIVE)
    })
}

/// The fact of what a match on `ty` read of its ADT: its variants or fields.
pub(super) fn adt_fact(cx: &Cx<'_, '_>, diag: &mut Diag, ty: &Type<'_>) {
    let Some(adt) = ty.strip_references().as_adt() else { return };
    let what = match adt {
        Adt::Enum(_) => "variants",
        Adt::Struct(_) | Adt::Union(_) => "fields",
    };
    let path = cx.def_path(ModuleDef::Adt(adt));
    diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the {what} of `{path}`"));
}

/// Where rustc points at an ADT in a note: its name when it is the crate's own, else when
/// `non_local` its definition (`def_span`).
fn adt_span(cx: &Cx<'_, '_>, adt: Adt, non_local: bool) -> Option<Span> {
    let source = adt.source(cx.db)?;
    if adt.module(cx.db).krate(cx.db) == cx.krate {
        let name = source.value.name()?;
        return Some(cx.span(source.with_value(name.syntax().text_range())));
    }
    non_local.then(|| cx.span(source.with_value(def_range(source.value.syntax()))))
}

/// rustc's `AdtDefinedHere`: the ADT a match is on, with the variants its witnesses name.
pub(super) struct DefinedHere {
    at: Span,
    ty: String,
    variants: Vec<Span>,
}

impl DefinedHere {
    /// rustc's `report_adt_defined_here` of the first five witnesses.
    pub(super) fn of<'db>(
        cx: &Cx<'_, 'db>,
        ty: &Type<'db>,
        witnesses: &[Witness<'db>],
        non_local: bool,
    ) -> Option<DefinedHere> {
        let peeled = ty.strip_references();
        let adt = peeled.as_adt()?;
        let at = adt_span(cx, adt, non_local)?;
        let mut pointed = Vec::new();
        point_at_variants(adt, witnesses.iter().take(5), &mut pointed);
        let variants = pointed
            .into_iter()
            .filter_map(|variant| {
                let source = variant.source(cx.db)?;
                let name = source.value.name()?;
                Some(cx.span(source.with_value(name.syntax().text_range())))
            })
            .collect();
        Some(DefinedHere { at, ty: cx.ty(&peeled), variants })
    }

    /// Its spans; rustc's E0004 labels the ADT's own with an empty label, E0005 does not.
    pub(super) fn span(&self, empty_label: bool) -> MultiSpan {
        let mut span = MultiSpan::of(self.at.clone());
        if empty_label {
            span.labels.push((self.at.clone(), String::new()));
        }
        span.labels.extend(self.variants.iter().map(|it| (it.clone(), "not covered".to_owned())));
        span
    }

    pub(super) fn message(&self) -> String {
        format!("`{}` defined here", self.ty)
    }
}

/// rustc's `maybe_point_at_variant`: the variants of `adt` the witnesses name, first of each,
/// leaving out the fields of a variant of another enum and of one already named. Appended to
/// `out`; what a call has appended (`out[start..]`) is its `covered`, as in rustc, where each
/// call's list starts empty and takes in its fields' lists.
fn point_at_variants<'a, 'db: 'a>(
    adt: Adt,
    witnesses: impl Iterator<Item = &'a Witness<'db>>,
    out: &mut Vec<hir::EnumVariant>,
) {
    let start = out.len();
    for witness in witnesses {
        if let Witness::Adt { adt: of, variant: Some(variant), .. } = witness {
            if *of != adt || out[start..].contains(variant) {
                continue;
            }
            out.push(*variant);
        }
        point_at_variants(adt, witness.fields(), out);
    }
}

/// What hir says is not covered: its text read back (`hir-ide`'s `missing_match_arms`, rustc's
/// joined form of up to three witnesses as rust-analyzer prints them).
pub(super) enum Uncovered<'t> {
    /// "type `T` is non-empty": an empty match on a type that is not an enum with variants.
    NonEmpty,
    /// The first witnesses (three at most) and how many there are.
    Witnesses { shown: Vec<&'t str>, count: usize },
}

impl<'t> Uncovered<'t> {
    pub(super) fn read(text: &'t str) -> Option<Uncovered<'t>> {
        if text.starts_with("type `") && text.ends_with("` is non-empty") {
            return Some(Uncovered::NonEmpty);
        }
        // No witness has a backtick in it, so "` and " and "`, `" only ever join them.
        let list = text.strip_suffix(" not covered")?.strip_prefix('`')?;
        if let Some((head, more)) = list.rsplit_once("` and ")
            && let Some(more) = more.strip_suffix(" more")
        {
            let shown: Vec<&str> = head.split("`, `").collect();
            let count = shown.len() + more.parse::<usize>().ok()?;
            return Some(Uncovered::Witnesses { shown, count });
        }
        let list = list.strip_suffix('`')?;
        let mut shown: Vec<&str> = Vec::with_capacity(3);
        match list.rsplit_once("` and `") {
            Some((head, last)) => {
                shown.extend(head.split("`, `"));
                shown.push(last);
            }
            None => shown.push(list),
        }
        let count = shown.len();
        Some(Uncovered::Witnesses { shown, count })
    }
}

/// The witnesses of a match or a binding: read back where they could be, and printed as rustc
/// prints them (as rust-analyzer did where one could not be read back).
pub(super) struct Witnesses<'db> {
    pub(super) read: Vec<Witness<'db>>,
    pub(super) printed: Vec<String>,
    pub(super) count: usize,
}

impl<'db> Witnesses<'db> {
    pub(super) fn read(cx: &Cx<'_, 'db>, shown: &[&str], count: usize, ty: Option<&Type<'db>>) -> Witnesses<'db> {
        let mut read = Vec::with_capacity(shown.len());
        let mut printed = Vec::with_capacity(shown.len());
        for text in shown {
            match ty.and_then(|ty| Witness::read(cx, text, ty)) {
                Some(witness) => {
                    let mut out = String::with_capacity(text.len() + 8);
                    witness.print(cx, &mut out);
                    printed.push(out);
                    read.push(witness);
                }
                None => printed.push((*text).to_owned()),
            }
        }
        Witnesses { read, printed, count }
    }

    /// rustc's `joined_uncovered_patterns`.
    pub(super) fn joined(&self) -> String {
        match &self.printed[..] {
            [] => String::new(),
            [only] if self.count == 1 => format!("`{only}`"),
            [head @ .., last] if self.count == self.printed.len() => {
                format!("`{}` and `{last}`", head.join("`, `"))
            }
            shown => format!("`{}` and {} more", shown.join("`, `"), self.count - shown.len()),
        }
    }
}

/// A witness, as rustc's `WitnessPat` has it as far as diagnostics print it.
pub(super) enum Witness<'db> {
    Wild,
    /// rustc's `Never` constructor: a value of an empty type in a place that may hold an
    /// invalid one.
    Never(Type<'db>),
    Bool(bool),
    Ref { mutable: bool, inner: Box<Witness<'db>> },
    Tuple(Vec<Witness<'db>>),
    Adt {
        adt: Adt,
        variant: Option<hir::EnumVariant>,
        kind: StructKind,
        arity: usize,
        /// For a braced one, the fields rust-analyzer printed (those not `_`); else all of them.
        fields: Vec<(hir::Field, Witness<'db>)>,
    },
}

impl<'db> Witness<'db> {
    /// `text`, as `hir-ide`'s `HirDisplay for Pat` printed a witness of `ty`.
    pub(super) fn read(cx: &Cx<'_, 'db>, text: &str, ty: &Type<'db>) -> Option<Witness<'db>> {
        let mut reader = Reader { cx, text, at: 0 };
        let witness = reader.witness(ty)?;
        (reader.at == text.len()).then_some(witness)
    }

    fn fields(&self) -> Fields<'_, 'db> {
        match self {
            Witness::Ref { inner, .. } => Fields::Witnesses(std::slice::from_ref(&**inner).iter()),
            Witness::Tuple(fields) => Fields::Witnesses(fields.iter()),
            Witness::Adt { fields, .. } => Fields::Named(fields.iter()),
            Witness::Wild | Witness::Never(_) | Witness::Bool(_) => Fields::Witnesses(Default::default()),
        }
    }

    /// rustc's `would_print_as_wildcard`.
    fn is_wild(&self) -> bool {
        matches!(self, Witness::Wild | Witness::Never(_))
    }

    /// The types of its `!`s, first of each, in order: rustc's `collect_special_tys` of what
    /// rust-analyzer's printing keeps.
    fn never_types(&self, out: &mut Vec<Type<'db>>) {
        if let Witness::Never(ty) = self
            && !out.contains(ty)
        {
            out.push(ty.clone());
        }
        for field in self.fields() {
            field.never_types(out);
        }
    }

    /// rustc's `print_witness_pat`.
    pub(super) fn print(&self, cx: &Cx<'_, 'db>, out: &mut String) {
        match self {
            Witness::Wild | Witness::Never(_) => out.push('_'),
            Witness::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Witness::Ref { mutable, inner } => {
                out.push_str(if *mutable { "&mut " } else { "&" });
                inner.print(cx, out);
            }
            Witness::Tuple(fields) => {
                out.push('(');
                for (at, field) in fields.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    field.print(cx, out);
                }
                if fields.len() == 1 {
                    out.push(',');
                }
                out.push(')');
            }
            Witness::Adt { adt, variant, kind, arity, fields } => {
                match variant {
                    Some(variant) => {
                        let name = variant.name(cx.db);
                        if !is_option_or_result(cx, *adt) {
                            out.push_str(&cx.def_path(ModuleDef::Adt(*adt)));
                            out.push_str("::");
                        }
                        out.push_str(name.as_str());
                    }
                    None => out.push_str(&cx.def_path(ModuleDef::Adt(*adt))),
                }
                match kind {
                    StructKind::Record => {
                        out.push_str(" { ");
                        let mut printed = 0;
                        for (field, witness) in fields {
                            if witness.is_wild() {
                                continue;
                            }
                            if printed > 0 {
                                out.push_str(", ");
                            }
                            out.push_str(field.name(cx.db).as_str());
                            out.push_str(": ");
                            witness.print(cx, out);
                            printed += 1;
                        }
                        let union = matches!(adt, Adt::Union(_));
                        if printed < *arity && (!union || printed == 0) {
                            out.push_str(if printed > 0 { ", .." } else { ".." });
                        }
                        out.push_str(" }");
                    }
                    StructKind::Tuple | StructKind::Unit if *arity != 0 => {
                        out.push('(');
                        for (at, (_, witness)) in fields.iter().enumerate() {
                            if at > 0 {
                                out.push_str(", ");
                            }
                            witness.print(cx, out);
                        }
                        out.push(')');
                    }
                    StructKind::Tuple | StructKind::Unit => {}
                }
            }
        }
    }
}

/// The fields of a [`Witness`], in order.
enum Fields<'w, 'db> {
    Witnesses(std::slice::Iter<'w, Witness<'db>>),
    Named(std::slice::Iter<'w, (hir::Field, Witness<'db>)>),
}

impl<'w, 'db> Iterator for Fields<'w, 'db> {
    type Item = &'w Witness<'db>;

    #[inline]
    fn next(&mut self) -> Option<&'w Witness<'db>> {
        match self {
            Fields::Witnesses(it) => it.next(),
            Fields::Named(it) => it.next().map(|(_, it)| it),
        }
    }
}

/// rustc names the variants of `Option` and `Result` (its diagnostic items) without their enum:
/// here by their lang items.
fn is_option_or_result(cx: &Cx<'_, '_>, adt: Adt) -> bool {
    let hir_def::AdtId::EnumId(id) = hir_def::AdtId::from(adt) else { return false };
    let lang = hir_def::lang_item::lang_items(cx.db, cx.krate.base());
    lang.Option == Some(id) || lang.ResultOk.is_some_and(|it| it.lookup(cx.db).parent == id)
}

/// Reads rust-analyzer's printing of a witness back, the type of each part known from where it
/// is: `_`, `!`, `true`, `&p`, `&mut p`, `(p, q)`, `Name`, `Name(p, q)`, `Name { f: p, .. }`.
struct Reader<'t, 'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    text: &'t str,
    at: usize,
}

impl<'t, 'db> Reader<'t, '_, '_, 'db> {
    fn eat(&mut self, s: &str) -> bool {
        let found = self.text[self.at..].starts_with(s);
        if found {
            self.at += s.len();
        }
        found
    }

    fn ident(&mut self) -> Option<&'t str> {
        let rest: &'t str = &self.text[self.at..];
        let start = if rest.starts_with("r#") { 2 } else { 0 };
        let len = rest[start..].find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len() - start);
        if len == 0 {
            return None;
        }
        self.at += start + len;
        Some(&rest[start..start + len])
    }

    fn witness(&mut self, ty: &Type<'db>) -> Option<Witness<'db>> {
        let db = self.cx.db;
        if self.eat("&") {
            let mutable = self.eat("mut ");
            let (inner, _) = ty.as_reference()?;
            return Some(Witness::Ref { mutable, inner: Box::new(self.witness(&inner)?) });
        }
        if self.eat("!") {
            return Some(Witness::Never(ty.clone()));
        }
        if self.eat("(") {
            let tys = ty.tuple_fields(db);
            let mut fields = Vec::with_capacity(tys.len());
            for (at, ty) in tys.iter().enumerate() {
                if at > 0 && !self.eat(", ") {
                    return None;
                }
                fields.push(self.witness(ty)?);
            }
            self.eat(",");
            return self.eat(")").then_some(Witness::Tuple(fields));
        }
        let name = self.ident()?;
        match name {
            "_" => return Some(Witness::Wild),
            "true" | "false" if ty.is_bool() => return Some(Witness::Bool(name == "true")),
            _ => {}
        }
        let adt = ty.as_adt()?;
        let (variant, all, kind) = match adt {
            Adt::Enum(it) => {
                let variant = it.variants(db).into_iter().find(|it| it.name(db).as_str() == name)?;
                (Some(variant), variant.fields(db), variant.kind(db))
            }
            Adt::Struct(it) => (None, it.fields(db), it.kind(db)),
            Adt::Union(it) => (None, it.fields(db), it.kind(db)),
        };
        let field_ty = |field: &hir::Field| field.ty(db).instantiate_type_params(db, ty.type_arguments());
        let mut fields = Vec::with_capacity(all.len());
        match kind {
            StructKind::Record => {
                if !self.eat(" { ") {
                    return None;
                }
                if !self.eat(" }") {
                    loop {
                        if self.eat("..") {
                            break;
                        }
                        let name = self.ident()?;
                        let field = *all.iter().find(|it| it.name(db).as_str() == name)?;
                        if !self.eat(": ") {
                            return None;
                        }
                        fields.push((field, self.witness(&field_ty(&field))?));
                        if !self.eat(", ") {
                            break;
                        }
                    }
                    if !self.eat(" }") {
                        return None;
                    }
                }
            }
            StructKind::Tuple | StructKind::Unit if !all.is_empty() => {
                if !self.eat("(") {
                    return None;
                }
                for (at, field) in all.iter().enumerate() {
                    if at > 0 && !self.eat(", ") {
                        return None;
                    }
                    fields.push((*field, self.witness(&field_ty(field))?));
                }
                if !self.eat(")") {
                    return None;
                }
            }
            StructKind::Tuple | StructKind::Unit => {}
        }
        Some(Witness::Adt { adt, variant, kind, arity: all.len(), fields })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Option<(Vec<&str>, usize)> {
        match Uncovered::read(text)? {
            Uncovered::NonEmpty => None,
            Uncovered::Witnesses { shown, count } => Some((shown, count)),
        }
    }

    #[test]
    fn the_uncovered_text_is_read_back_to_its_witnesses() {
        assert_eq!(read("`None` not covered"), Some((vec!["None"], 1)));
        assert_eq!(read("`A` and `B(_)` not covered"), Some((vec!["A", "B(_)"], 2)));
        assert_eq!(read("`A`, `S { a: and, .. }` and `C` not covered"), Some((vec!["A", "S { a: and, .. }", "C"], 3)));
        assert_eq!(read("`A`, `B`, `C` and 8 more not covered"), Some((vec!["A", "B", "C"], 11)));
        assert!(matches!(Uncovered::read("type `&A` is non-empty"), Some(Uncovered::NonEmpty)));
        assert!(Uncovered::read("something else").is_none());
    }

    #[test]
    fn the_indentation_is_that_of_the_line() {
        let text = "fn main() {\n    match x {\n        A => {}\n    }\n}\n";
        let at = text.find("A =>").unwrap() as u32;
        assert_eq!(indentation_before(text, at), "        ");
        assert_eq!(extend_while(text, at + 7, |c| c.is_whitespace() || c == ','), text.find("    }").unwrap() as u32 + 4);
    }

    #[test]
    fn a_witness_gives_its_fields_in_order() {
        let tuple = Witness::Tuple(vec![Witness::Bool(true), Witness::Wild, Witness::Bool(false)]);
        let fields: Vec<_> = tuple.fields().collect();
        assert!(matches!(fields[..], [Witness::Bool(true), Witness::Wild, Witness::Bool(false)]));
        let reference = Witness::Ref { mutable: false, inner: Box::new(Witness::Bool(true)) };
        assert!(matches!(reference.fields().collect::<Vec<_>>()[..], [Witness::Bool(true)]));
        assert_eq!(Witness::Wild.fields().count(), 0);
        assert_eq!(Witness::Bool(true).fields().count(), 0);
    }
}
