//! The errors of builtin attributes: what rustc's attribute parser gives as it lowers the AST
//! to HIR (`parse_attribute_list`: an attribute's safety, its arguments, what its parser says
//! of them, a second one where one is allowed, its target, a crate-level attribute elsewhere,
//! the attributes of a `where` predicate), what expansion's `check_attr` gives of the attributes
//! it leaves unparsed, and what `check_mod_attrs` gives of the parsed attributes.
//!
//! - `args`: an attribute's arguments (rustc's `ArgParser`) and the errors of reading them.
//! - `accept`: what a parser is given, and its errors of its input and its target.
//! - `cfg`: the errors of a `cfg` predicate a parser reads.
//! - `parsed`: what the parsers make of the attributes, as the checks after them read it.
//! - `parsers`: the parsers, one module per group of rustc's files.
//! - `check`: `check_mod_attrs`' errors.
//!
//! The lints of all of this are `lints::syntax::attrs`'; the feature gates of attributes
//! (E0658) are `checks::features::attributes`', which parsing calls where rustc gives them.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/interface.rs` (`parse_attribute_list`),
//! `compiler/rustc_attr_parsing/src/safety.rs` (`check_attribute_safety`),
//! `compiler/rustc_attr_parsing/src/target_checking.rs` (`check_invalid_crate_level_attr_item`,
//! `first_line_of_next_item`, `check_invalid_where_predicate_attrs`),
//! `compiler/rustc_attr_parsing/src/attributes/mod.rs` (`Single`, `OnDuplicate`),
//! `compiler/rustc_attr_parsing/src/attributes/transparency.rs` and the texts of
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs` and
//! `compiler/rustc_attr_parsing/src/diagnostics.rs` (MIT or Apache-2.0).

mod accept;
pub(in crate::checks) mod args;
mod cfg;
mod check;
pub(in crate::checks) mod parsed;
mod parsers;

use blaze_items::Fact;
use hir::InFile;
use syntax::{TextRange, TextSize};

use self::{
    accept::{Accept, Session},
    args::ArgParser,
    parsed::ParsedAttr,
    parsers::{Accepted, Finalize, GroupId, Module},
};
use crate::{
    checks::features,
    cx::Cx,
    lints::syntax::{
        Walk,
        attrs::{
            Attr, AttrKind, Owner, Target,
            table::{self, Kind, OnDuplicate, Safety, Targets},
        },
        source_fact,
    },
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// The errors attribute parsing gives of `owners`' attributes: as the AST is lowered, and for
/// a macro call's attributes, at expansion; with those of expansion's `check_attr`.
pub(crate) fn parsed(cx: &Cx<'_, '_>, _walk: &Walk<'_>, owners: &[Owner]) -> Vec<Found> {
    let session = Session::of(cx, owners);
    let mut found = Vec::new();
    for owner in owners {
        let mut errors = Vec::new();
        for attr in &owner.attrs {
            parsers::misc::validate(cx, attr, &mut errors);
        }
        push(cx, owner, Phase::Expand, errors, &mut found);
        let mut errors = Vec::new();
        lower(cx, &session, owner, true, &mut errors);
        let phase = match owner.target {
            Target::MacroCall => Phase::Expand,
            _ => Phase::Lower,
        };
        push(cx, owner, phase, errors, &mut found);
    }
    found
}

/// The errors `check_mod_attrs` gives of `owners`' parsed attributes, then those of the
/// stability pass, which reads what each definition inherits from its parents.
pub(crate) fn checked(cx: &Cx<'_, '_>, _walk: &Walk<'_>, owners: &[Owner]) -> Vec<Found> {
    let session = Session::of(cx, owners);
    let mut found = Vec::new();
    let mut all = Vec::with_capacity(owners.len());
    for owner in owners {
        if owner.target == Target::MacroCall {
            all.push(Vec::new());
            continue;
        }
        let parsed = lower(cx, &session, owner, false, &mut Vec::new());
        let mut errors = Vec::new();
        check::check(cx, &session, owner, &parsed, &mut errors);
        push(cx, owner, Phase::Items, errors, &mut found);
        all.push(parsed);
    }
    for (at, diag) in parsers::stability::crate_checks(cx, &session, owners, &all) {
        push(cx, &owners[at], Phase::Items, vec![diag], &mut found);
    }
    found
}

/// Pushes `owner`'s errors. Besides its source, each reads the `#![feature]`s of the crate
/// root, which the gates, the parsers and their targets look at.
fn push(cx: &Cx<'_, '_>, owner: &Owner, phase: Phase, errors: Vec<Diag>, found: &mut Vec<Found>) {
    let root = cx.module_key(cx.krate.root_module(cx.db));
    for mut diag in errors {
        source_fact(cx, &mut diag, owner.at());
        diag.fact(Fact::Signature(root), "the `#![feature]`s of the crate root");
        let phase = match (diag.fatal, owner.target) {
            (true, Target::Crate) => Phase::CrateAttrs,
            (true, _) => Phase::Expand,
            (false, _) => phase,
        };
        found.push(Found::new(diag, phase));
    }
}

/// A parser group of a node, in the order its first attribute comes.
struct Group {
    module: Module,
    id: GroupId,
    /// Its first attribute; for a single parser, the one it uses once it has one.
    first: usize,
    kind: GroupKind,
}

enum GroupKind {
    /// A single parser, with the conversion it uses.
    Single(Option<parsed::Parsed>),
    /// A parser that keeps its attributes in its module's state.
    State,
}

/// rustc's `parse_attribute_list` of `owner`'s attributes: what they are, in the order rustc
/// keeps them, with the errors (when `emit`) in `errors`.
fn lower(cx: &Cx<'_, '_>, session: &Session, owner: &Owner, emit: bool, errors: &mut Vec<Diag>) -> Vec<ParsedAttr> {
    let mut quiet = Vec::new();
    let out: &mut Vec<Diag> = if emit { errors } else { &mut quiet };
    let mut node = parsers::Node::default();
    let mut groups: Vec<Group> = Vec::new();
    // The doc comments, `#[doc = ".."]` and the attributes no parser takes, which rustc keeps in
    // place: `where` predicates may have none of them.
    let mut in_place: Vec<usize> = Vec::new();
    for (index, attr) in owner.attrs.iter().enumerate() {
        let AttrKind::Meta { .. } = &attr.kind else {
            in_place.push(index);
            continue;
        };
        let path = attr.path();
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        let Some(builtin) = table::builtin(&segments) else {
            if emit && attr.is_unsafe() {
                out.push(not_unsafe(cx, attr));
            }
            if emit && owner.target == Target::Crate {
                out.extend(at_crate_level(cx, attr));
            }
            in_place.push(index);
            continue;
        };
        if emit {
            out.extend(safety(cx, attr, builtin.safety));
            if owner.target != Target::MacroCall {
                out.extend(features::attributes::gate(cx, &session.features, attr));
            }
        }
        let Some(args) = args::parse(cx, attr, &path, out) else { continue };
        if builtin.path == ["doc"]
            && let ArgParser::NameValue(nv) = &args
            && nv.value_as_str().is_some()
        {
            in_place.push(index);
            continue;
        }
        let Some(module) = parsers::module(builtin.path) else {
            panic!("no parser module takes the builtin attribute `{}`", attr.name())
        };
        let earlier = used_so_far(&groups);
        let mut accept = Accept::new(cx, session, owner, index, builtin, &earlier, emit, out);
        match parsers::accept(&mut node, module, &mut accept, &args) {
            Accepted::Single(converted) => {
                let id = GroupId::Single(builtin.path);
                let at = match groups.iter().position(|it| it.id == id) {
                    Some(at) => at,
                    None => {
                        groups.push(Group { module, id, first: index, kind: GroupKind::Single(None) });
                        groups.len() - 1
                    }
                };
                let group = &mut groups[at];
                if let Some(converted) = converted {
                    let GroupKind::Single(used) = &mut group.kind else { unreachable!("{id:?} is a single parser") };
                    if used.is_some() {
                        if emit
                            && let Kind::Single(on_duplicate) = builtin.kind
                            && let Some(diag) = duplicate(cx, owner, builtin.path, on_duplicate, group.first, index)
                        {
                            out.push(diag);
                        }
                    } else {
                        *used = Some(converted);
                        group.first = index;
                    }
                }
            }
            Accepted::Group(name) => {
                let id = GroupId::Named(name);
                if !groups.iter().any(|it| it.id == id) {
                    groups.push(Group { module, id, first: index, kind: GroupKind::State });
                }
            }
        }
        if emit
            && !matches!(builtin.targets, Targets::ManuallyChecked)
            && let Some(diag) = accept::target_error(cx, &session.krate, owner, attr, builtin.targets, "")
        {
            out.push(diag);
        }
    }
    let mut parsed = Vec::new();
    let mut deferred = Vec::new();
    let mut fx = Finalize::new(cx, session, owner, emit, out);
    for group in &mut groups {
        match &mut group.kind {
            GroupKind::Single(used) => {
                if let Some(made) = used.take() {
                    parsed.push(ParsedAttr { parsed: made, attr: group.first });
                    deferred.push((group.module, group.id, group.first));
                }
            }
            GroupKind::State => {
                let GroupId::Named(name) = group.id else { unreachable!("{:?} keeps a state", group.id) };
                if let Some(made) = parsers::finalize(&mut node, group.module, name, &mut fx) {
                    parsed.push(ParsedAttr { parsed: made, attr: group.first });
                }
                deferred.push((group.module, group.id, group.first));
            }
        }
    }
    for (module, id, first) in deferred {
        parsers::check(module, id, first, &mut fx, &parsed);
    }
    drop(fx);
    if emit && owner.target == Target::WherePredicate {
        let mut spans = in_place;
        if let Some(doc) = parsed.iter().find(|it| matches!(it.parsed, parsed::Parsed::Doc(_))) {
            spans.push(doc.attr);
        }
        if !spans.is_empty() {
            out.push(in_where(cx, owner, &spans));
        }
    }
    parsed
}

/// What the single parsers before an attribute made of theirs, for a parser that reads its
/// node's other attributes as it parses.
fn used_so_far(groups: &[Group]) -> Vec<ParsedAttr> {
    groups
        .iter()
        .filter_map(|it| match &it.kind {
            GroupKind::Single(Some(parsed)) => Some(ParsedAttr { parsed: parsed.clone(), attr: it.first }),
            _ => None,
        })
        .collect()
}

/// What a second attribute of a single parser is, where the first is used: `UnusedMultiple`,
/// or `transparency.rs`'s own error (the lints are `lints::syntax::attrs`').
fn duplicate(
    cx: &Cx<'_, '_>,
    owner: &Owner,
    path: &[&str],
    on_duplicate: OnDuplicate,
    used: usize,
    unused: usize,
) -> Option<Diag> {
    let used_attr = &owner.attrs[used];
    let unused_attr = &owner.attrs[unused];
    let used_span = cx.span(used_attr.in_file(used_attr.range()));
    let unused_span = cx.span(unused_attr.in_file(unused_attr.range()));
    if path == ["rustc_macro_transparency"] {
        let mut diag = Diag::error(None, "multiple macro transparency attributes", used_span);
        diag.primary(unused_span);
        return Some(diag);
    }
    if on_duplicate != OnDuplicate::Error {
        return None;
    }
    let mut diag = Diag::error(None, format!("multiple `{}` attributes", path.join("..")), unused_span.clone());
    diag.suggestion(unused_span, "remove this attribute", "", Applicability::MachineApplicable);
    diag.span_note(used_span, "attribute also specified here");
    Some(diag)
}

/// The errors of rustc's `check_attribute_safety`: an unsafe attribute without `unsafe(..)`
/// from the edition it must have one, `unsafe(..)` around one that is not unsafe.
fn safety(cx: &Cx<'_, '_>, attr: &Attr, expected: Safety) -> Option<Diag> {
    match expected {
        Safety::Unsafe { unsafe_since, .. } if !attr.is_unsafe() => {
            if unsafe_since.is_some_and(|since| attr.file.edition(cx.db) < since) {
                return None;
            }
            let path = cx.span(attr.in_file(attr.path_range()));
            let mut diag = Diag::error(None, "unsafe attribute used without unsafe", path.clone());
            diag.span_label(path, "usage of unsafe attribute");
            if !hides_wrapping(cx, attr) {
                let inner = cx.span(attr.in_file(attr.inner_range()));
                diag.multipart_suggestion(
                    "wrap the attribute in `unsafe(...)`",
                    vec![(inner.shrink_to_lo(), "unsafe(".to_owned()), (inner.shrink_to_hi(), ")".to_owned())],
                    Applicability::MachineApplicable,
                );
            }
            Some(diag)
        }
        Safety::Normal if attr.is_unsafe() => Some(not_unsafe(cx, attr)),
        _ => None,
    }
}

/// `InvalidAttrUnsafe`: `unsafe(..)` around an attribute that is not unsafe.
fn not_unsafe(cx: &Cx<'_, '_>, attr: &Attr) -> Diag {
    let AttrKind::Meta { unsafety: Some(unsafety), .. } = &attr.kind else {
        unreachable!("`{}` is written `unsafe(..)`", attr.name())
    };
    let keyword = unsafety
        .unsafe_token()
        .map(|it| it.text_range())
        .unwrap_or_else(|| TextRange::at(syntax::AstNode::syntax(unsafety).text_range().start(), TextSize::from(6)));
    let span = cx.span(attr.in_file(keyword));
    let mut diag = Diag::error(None, format!("`{}` is not an unsafe attribute", attr.name()), span.clone());
    diag.span_label(span, "this is not an unsafe attribute");
    diag.note("extraneous unsafe is not allowed in attributes");
    diag
}

/// Whether rustc leaves out the suggestion to wrap the attribute in `unsafe(..)`: it is from an
/// expansion and its text is a macro call or a whole attribute (`not_from_proc_macro`).
fn hides_wrapping(cx: &Cx<'_, '_>, attr: &Attr) -> bool {
    let AttrKind::Meta { written, .. } = &attr.kind else { return false };
    if !crate::lints::syntax::from_expansion(cx, InFile::new(attr.file, syntax::AstNode::syntax(written))) {
        return false;
    }
    let span = cx.span(attr.in_file(attr.inner_range()));
    let Some(snippet) = cx.snippet(&span) else { return false };
    let snippet: String = snippet.chars().filter(|it| !it.is_whitespace()).collect();
    snippet.contains("!(") || snippet.starts_with("#[") && snippet.ends_with(']')
}

/// `InvalidAttrAtCrateLevel`: `#![derive]`, `#![test]`, `#![test_case]`,
/// `#![global_allocator]` or `#![bench]`, which name attribute macros that apply to items.
fn at_crate_level(cx: &Cx<'_, '_>, attr: &Attr) -> Option<Diag> {
    const NAMES: &[&str] = &["derive", "test", "test_case", "global_allocator", "bench"];
    let name = NAMES.iter().find(|it| attr.is(it))?;
    let range = attr.range();
    let span = cx.span(attr.in_file(range));
    let mut diag = Diag::error(None, format!("`{name}` attribute cannot be used at crate level"), span.clone());
    let pound = TextRange::new(range.start(), attr.inner_range().start());
    diag.suggestion_verbose(cx.span(attr.in_file(pound)), "perhaps you meant to use an outer attribute", "#[", Applicability::MachineApplicable);
    if let Some(file) = cx.file_of(&span)
        && let Some(next) = first_line_of_next_item(cx.file_text(file), range.end())
    {
        diag.span_label(cx.span_in_file(file, next), "the inner attribute doesn't annotate this item");
    }
    Some(diag)
}

/// rustc's `first_line_of_next_item`: the first line of what follows `end` that is not blank,
/// a comment or an attribute (up to its first `]`).
fn first_line_of_next_item(text: &str, end: TextSize) -> Option<TextRange> {
    let start = usize::from(end);
    let mut source = text.get(start..)?;
    loop {
        let first = source.chars().next()?;
        if first.is_whitespace() {
            let at = source.find(|c: char| !c.is_whitespace())?;
            source = &source[at..];
        } else if source.starts_with("//") {
            let at = source.find('\n')?;
            source = &source[at + 1..];
        } else if source.starts_with("/*") {
            let at = source.find("*/")?;
            source = &source[at + 2..];
        } else if first == '#' {
            let at = source.find(']')?;
            source = &source[at + 1..];
        } else {
            let lo = text.len() - source.len();
            let line = source.split('\n').next().map(str::trim_end)?;
            let lo = TextSize::try_from(lo).ok()?;
            return Some(TextRange::at(lo, TextSize::try_from(line.len()).ok()?));
        }
    }
}

/// `UnsupportedAttributesInWhere`, at the attributes `indices` of a `where` predicate.
fn in_where(cx: &Cx<'_, '_>, owner: &Owner, indices: &[usize]) -> Diag {
    let mut spans = indices.iter().map(|&it| {
        let attr = &owner.attrs[it];
        cx.span(attr.in_file(attr.range()))
    });
    let Some(first) = spans.next() else { unreachable!("a `where` predicate's attributes are given") };
    let mut diag = Diag::error(None, "most attributes are not supported in `where` clauses", first);
    for span in spans {
        diag.primary(span);
    }
    diag.help("only `#[cfg]` and `#[cfg_attr]` are supported");
    diag
}

#[cfg(test)]
mod tests {
    use syntax::TextSize;

    use super::first_line_of_next_item;

    #[test]
    fn the_next_item_is_after_comments_and_attributes() {
        let text = "#![derive(Copy)]\n// a\n/* b */\n#[inline]\nfn main() {}   \nfn g() {}\n";
        let end = TextSize::from(16);
        let found = first_line_of_next_item(text, end).map(|it| &text[it]);
        assert_eq!(found, Some("fn main() {}"));
        assert_eq!(first_line_of_next_item("#![test]", TextSize::from(8)), None);
    }
}
