//! What rustc's attribute parser says of the builtin attributes of each node as it parses
//! them, in the order it says it: for each attribute, its safety (`unsafe_attr_outside_unsafe`,
//! `unsafe_code`), what its parser says of its input (an empty list, an ill-formed input, a
//! second one where one is used, an unknown crate type, a derive helper named as a builtin
//! attribute), then its target (`unused_attributes` or `misplaced_diagnostic_attributes`: an
//! attribute where it does nothing; a crate-level attribute below the crate root).
//!
//! A parser takes an attribute, and so counts a second one as unused, when the attribute has
//! one of the forms of its template (a parser of no arguments takes any); the parsers that
//! lint their input themselves (`inline`, `link`, `ignore`, `used`, `macro_use`, `crate_type`,
//! `proc_macro_derive`, `repr`, the lists of `feature`, `target_feature`, `register_tool`)
//! are ported. Its errors (E0565, E0538, an unsafe attribute without `unsafe(..)` from the
//! edition it became unsafe, a target that is an error) are `diagnostics-items`' `checks/
//! attrs.rs`; `#[doc]`'s and `#[diagnostic::..]`'s own lints are `doc`'s and `diagnostic`'s;
//! `useless_deprecated` (`#[deprecated]` where it does nothing) is `diagnostics-lints-names`'.
//!
//! rustc lints no deprecated builtin attribute: `deprecated_attr` is removed
//! (`compiler/rustc_lint/src/lib.rs`).
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/interface.rs` (`parse_attribute_list`),
//! `safety.rs`, `target_checking.rs`, `context.rs` (`warn_unused_duplicate`,
//! `warn_empty_attribute`, `warn_ill_formed_attribute_input`), `attributes/mod.rs`
//! (`Single`, `Combine`), `attributes/{inline,link_attrs,test_attrs,codegen_attrs,
//! macro_attrs,crate_level,proc_macro_attrs,repr}.rs`, and the texts of `diagnostics.rs` and
//! `session_diagnostics.rs` (MIT or Apache-2.0).

use hir::InFile;
use rustc_hash::FxHashMap;
use syntax::{AstNode, NodeOrToken, SyntaxElement, SyntaxKind, SyntaxNode, T, TextRange, ast};

use super::{
    Args, Attr, AttrKind, Owner, Target, lint_at, parsed,
    table::{self, Allowed, Builtin, Kind, OnDuplicate, Policy, Safety, Targets},
};
use crate::{
    cx::{Cx, item_range},
    lints::{
        edit_distance::find_best_match_for_name_with_substrings,
        registry::LintId,
        syntax::{Emitted, Walk, from_expansion, source_fact},
    },
    model::{Applicability, Diag, SuggestionStyle},
};

const PREVIOUSLY_ACCEPTED: &str = "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!";

/// What the crate says that the attribute parser reads: its root file and its features.
pub(crate) struct Krate {
    root: Option<hir_expand::EditionedFileId>,
    root_name: String,
    fn_delegation: bool,
    stmt_expr_attributes: bool,
    extern_types: bool,
    pub(crate) fn_align: bool,
    pub(crate) static_align: bool,
}

impl Krate {
    pub(crate) fn of(cx: &Cx<'_, '_>, owners: &[Owner]) -> Krate {
        let root = cx.krate.root_module(cx.db).as_source_file_id(cx.db);
        let root_name = root.map(|it| cx.file_name(it.file_id(cx.db))).unwrap_or_default();
        let mut krate = Krate {
            root,
            root_name,
            fn_delegation: false,
            stmt_expr_attributes: false,
            extern_types: false,
            fn_align: false,
            static_align: false,
        };
        let crate_attrs = owners.iter().filter(|it| it.target == Target::Crate).flat_map(|it| &it.attrs);
        for attr in crate_attrs {
            let (true, Args::List(tt)) = (attr.is("feature"), attr.args()) else { continue };
            for item in items(&tt) {
                match word(&item).as_deref() {
                    Some("fn_delegation") => krate.fn_delegation = true,
                    Some("stmt_expr_attributes") => krate.stmt_expr_attributes = true,
                    Some("extern_types") => krate.extern_types = true,
                    Some("fn_align") => krate.fn_align = true,
                    Some("static_align") => krate.static_align = true,
                    _ => {}
                }
            }
        }
        krate
    }
}

/// What the parsers of one node have taken so far.
#[derive(Default)]
struct Seen {
    /// The attribute each single parser uses, by its path.
    single: FxHashMap<&'static [&'static str], InFile<TextRange>>,
    /// `#[used(compiler)]`, `#[used(linker)]` and `#[used]`.
    used: [Option<InFile<TextRange>>; 3],
    macro_use: MacroUse,
}

/// `MacroUseParser`'s state: whether a `#[macro_use]` without arguments was seen, the first
/// one, and those with arguments.
#[derive(Default)]
struct MacroUse {
    all: bool,
    first: Option<InFile<TextRange>>,
    specific: Vec<InFile<TextRange>>,
}

/// The lints attribute parsing gives of `owners`' builtin attributes.
pub(crate) fn lints(cx: &Cx<'_, '_>, _walk: &Walk<'_>, owners: &[Owner]) -> Vec<Emitted> {
    let krate = Krate::of(cx, owners);
    let mut emitted = Vec::new();
    let mut found = Vec::new();
    for owner in owners {
        let on_call = owner.target == Target::MacroCall;
        let mut seen = Seen::default();
        for (i, attr) in owner.attrs.iter().enumerate() {
            let AttrKind::Meta { .. } = &attr.kind else { continue };
            // A macro call's attributes are parsed one at a time, so none is a second one.
            if on_call {
                seen = Seen::default();
            }
            let path = attr.path();
            let segments: smallvec::SmallVec<[&str; 2]> = path.iter().map(String::as_str).collect();
            match table::builtin(&segments) {
                Some(builtin) => {
                    safety(cx, attr, builtin, &mut found);
                    if parses(attr, builtin) {
                        accept(cx, &krate, owner, attr, builtin, &mut seen, &mut found);
                        match builtin.targets {
                            Targets::ManuallyChecked if attr.is("repr") => {
                                repr_targets(cx, &krate, owner, attr, &mut found)
                            }
                            Targets::ManuallyChecked => {}
                            targets => check_target(cx, &krate, owner, attr, targets, "", &mut found),
                        }
                    }
                }
                None => {
                    // `check_attributes` skips an attribute a doc string follows.
                    if on_call
                        && !owner.attrs.get(i + 1).is_some_and(is_doc_str)
                        && let [name] = segments.as_slice()
                        && !matches!(*name, "cfg" | "cfg_attr")
                        && table::is_builtin_attr_name(name)
                    {
                        found.push(unused_builtin(cx, owner, attr, name));
                    }
                }
            }
            for mut diag in found.drain(..) {
                source_fact(cx, &mut diag, lint_at(owner));
                emitted.push(parsed(diag, owner));
            }
        }
    }
    emitted
}

/// Whether rustc's `ArgParser::from_attr_args` parses the attribute's arguments; when it does
/// not, it gives an error (`diagnostics-items`') and the attribute is not parsed further. A
/// `= value` must be an unsuffixed literal; a list must be in parentheses and of meta items, but
/// for `diagnostic::` attributes and `rustc_dummy`, whose lists are always taken.
fn parses(attr: &Attr, builtin: &Builtin) -> bool {
    match attr.args() {
        Args::Word => true,
        Args::NameValue(value) => is_unsuffixed_literal(value.as_ref()),
        Args::List(_) if matches!(builtin.path, ["diagnostic", ..] | ["rustc_dummy"]) => true,
        Args::List(tt) => {
            tt.left_delimiter_token().is_some_and(|it| it.kind() == T!['('])
                && items(&tt).iter().all(|item| is_meta_item(item))
        }
    }
}

/// rustc's `expr_to_lit` accepting `value`: a literal without a suffix.
fn is_unsuffixed_literal(value: Option<&ast::Expr>) -> bool {
    let Some(ast::Expr::Literal(literal)) = value else { return false };
    let token = literal.token();
    let text = token.text();
    match literal.kind() {
        ast::LiteralKind::IntNumber(it) => it.suffix().is_none(),
        ast::LiteralKind::FloatNumber(it) => it.suffix().is_none(),
        ast::LiteralKind::String(_) | ast::LiteralKind::ByteString(_) | ast::LiteralKind::CString(_) => {
            text.ends_with('"') || text.ends_with('#')
        }
        ast::LiteralKind::Char(_) | ast::LiteralKind::Byte(_) => text.ends_with('\''),
        ast::LiteralKind::Bool(_) => true,
    }
}

/// Whether rustc's meta item list parser takes `item`: a literal, or a path with nothing after
/// it, a parenthesized list or `= value`.
fn is_meta_item(item: &[SyntaxElement]) -> bool {
    let is_path_token = |it: &SyntaxElement| {
        matches!(it, NodeOrToken::Token(it) if matches!(it.kind(), SyntaxKind::IDENT | T![::] | T![self] | T![super] | T![crate] | T![Self]) || it.kind().is_keyword(span::Edition::CURRENT))
    };
    match item {
        [] => false,
        [NodeOrToken::Token(token)] if token.kind().is_literal() || matches!(token.kind(), T![true] | T![false]) => true,
        _ => {
            let path = item.iter().take_while(|it| is_path_token(it)).count();
            if path == 0 {
                return false;
            }
            match &item[path..] {
                [] => true,
                [NodeOrToken::Node(list)] => {
                    ast::TokenTree::cast(list.clone()).and_then(|it| it.left_delimiter_token()).is_some_and(|it| it.kind() == T!['('])
                }
                [NodeOrToken::Token(eq), value @ ..] => eq.kind() == T![=] && !value.is_empty(),
                _ => false,
            }
        }
    }
}

/// rustc's `doc_str`: a doc comment or `#[doc = "..."]`.
fn is_doc_str(attr: &Attr) -> bool {
    attr.is_doc_comment() || attr.is("doc") && matches!(attr.args(), Args::NameValue(value) if string(value.as_ref()).is_some())
}

/// rustc's `check_attribute_safety`: an attribute that must be `unsafe(..)` and is not,
/// before the edition it must be, and `unsafe_code` of every unsafe attribute.
fn safety(cx: &Cx<'_, '_>, attr: &Attr, builtin: &Builtin, found: &mut Vec<Diag>) {
    let Safety::Unsafe { note, unsafe_since } = builtin.safety else { return };
    let outside = cx.lint_may_fire(LintId::named("unsafe_attr_outside_unsafe"));
    let code = cx.lint_may_fire(LintId::named("unsafe_code"));
    if !outside && !code {
        return;
    }
    let inner = cx.span(attr.in_file(attr.inner_range()));
    if outside && !attr.is_unsafe() {
        let edition = attr.file.edition(cx.db);
        let is_error = unsafe_since.is_none_or(|since| edition >= since);
        if !is_error {
            let path = cx.span(attr.in_file(attr.path_range()));
            let mut diag = Diag::lint("unsafe_attr_outside_unsafe", "unsafe attribute used without unsafe", path.clone());
            diag.span_label(path, "usage of unsafe attribute");
            if !hides_wrapping(cx, attr) {
                diag.multipart_suggestion(
                    "wrap the attribute in `unsafe(...)`",
                    vec![(inner.shrink_to_lo(), "unsafe(".to_owned()), (inner.shrink_to_hi(), ")".to_owned())],
                    Applicability::MachineApplicable,
                );
            }
            found.push(diag);
        }
    }
    if !code {
        return;
    }
    let mut diag = Diag::lint("unsafe_code", format!("usage of the unsafe `{}` attribute", attr.name()), inner);
    diag.note(note);
    found.push(diag);
}

/// Whether rustc leaves out the suggestion to wrap the attribute in `unsafe(..)`: it is from an
/// expansion and its text is a macro call or a whole attribute (`safety.rs`'s
/// `not_from_proc_macro`).
fn hides_wrapping(cx: &Cx<'_, '_>, attr: &Attr) -> bool {
    let AttrKind::Meta { written, .. } = &attr.kind else { return false };
    if !from_expansion(cx, InFile::new(attr.file, written.syntax())) {
        return false;
    }
    let span = cx.span(attr.in_file(attr.inner_range()));
    let Some(snippet) = cx.snippet(&span) else { return false };
    let snippet: String = snippet.chars().filter(|it| !it.is_whitespace()).collect();
    snippet.contains("!(") || snippet.starts_with("#[") && snippet.ends_with(']')
}

/// What an attribute's parser says of its input and of a second one.
fn accept(
    cx: &Cx<'_, '_>,
    krate: &Krate,
    owner: &Owner,
    attr: &Attr,
    builtin: &'static Builtin,
    seen: &mut Seen,
    found: &mut Vec<Diag>,
) {
    let this = attr.in_file(attr.range());
    let args = attr.args();
    let path = builtin.path;
    match (path, &args) {
        (["inline"], Args::NameValue(_)) => found.push(ill_formed(cx, attr, builtin, None)),
        (["link"], Args::NameValue(value)) if string(value.as_ref()).as_deref() == Some("dl") => {
            found.push(ill_formed(cx, attr, builtin, None))
        }
        (["ignore"], Args::NameValue(value)) if string(value.as_ref()).is_none() => {
            found.push(ill_formed(cx, attr, builtin, None))
        }
        (["ignore"], Args::List(tt)) => {
            let items = items(tt);
            let help = match items.as_slice() {
                [item] => path_of(item),
                _ => None,
            };
            found.push(ill_formed(cx, attr, builtin, help));
        }
        (["macro_use"], _) => macro_use(cx, attr, builtin, &args, &mut seen.macro_use, found),
        (["used"], _) => used(cx, attr, &args, &mut seen.used, found),
        (["repr"] | ["target_feature"] | ["force_target_feature"] | ["feature"] | ["register_tool"], Args::List(tt))
            if items(tt).is_empty() =>
        {
            if attr.is("repr") {
                let targets = Targets::AllowList(&[
                    Policy::Allow(Target::Struct),
                    Policy::Allow(Target::Enum),
                    Policy::Allow(Target::Union),
                    Policy::Warn(Target::MacroCall),
                ]);
                check_target(cx, krate, owner, attr, targets, "()", found);
            }
            found.push(empty_list(cx, attr, builtin, this));
        }
        (["crate_type"], Args::NameValue(value)) if owner.target == Target::Crate => {
            if let Some(diag) = unknown_crate_type(cx, attr, value.as_ref()) {
                found.push(diag);
            }
        }
        (["proc_macro_derive"], Args::List(tt)) => found.extend(ambiguous_helpers(cx, attr, tt)),
        _ => {}
    }
    let Kind::Single(on_duplicate) = builtin.kind else { return };
    if !takes(builtin, attr, &args) {
        return;
    }
    match seen.single.get(path) {
        Some(&used) => match on_duplicate {
            OnDuplicate::Warn => found.push(unused_duplicate(cx, used, this, false)),
            OnDuplicate::WarnButFutureError => found.push(unused_duplicate(cx, used, this, true)),
            OnDuplicate::Error | OnDuplicate::Ignore => {}
        },
        None => {
            seen.single.insert(path, this);
        }
    }
}

/// Whether a single parser converts the attribute, so that a second one is unused: a parser
/// of no arguments always does, the others when the input is of one of their forms, as
/// their conversions check.
fn takes(builtin: &Builtin, attr: &Attr, args: &Args) -> bool {
    let template = &builtin.template;
    let no_args = template.word && template.list.is_none() && template.one_of.is_empty() && template.name_value_str.is_none();
    if no_args {
        return true;
    }
    match args {
        Args::Word => template.word,
        Args::List(tt) => {
            let items = items(tt);
            template.list.is_some()
                || matches!(items.as_slice(), [item] if word(item).is_some_and(|it| template.one_of.contains(&it.as_str())))
        }
        Args::NameValue(value) => {
            template.name_value_str.is_some() && (string(value.as_ref()).is_some() || attr.is_doc_comment())
        }
    }
}

/// `MacroUseParser::ATTRIBUTES`: a second `#[macro_use]`, a `#[macro_use(..)]` with one without
/// arguments, an empty list.
fn macro_use(cx: &Cx<'_, '_>, attr: &Attr, builtin: &Builtin, args: &Args, state: &mut MacroUse, found: &mut Vec<Diag>) {
    let span = attr.in_file(attr.range());
    let first = *state.first.get_or_insert(span);
    match args {
        Args::Word if state.all => found.push(unused_duplicate(cx, first, span, false)),
        Args::Word => {
            state.all = true;
            state.first = Some(span);
            for specific in state.specific.drain(..) {
                found.push(unused_duplicate(cx, span, specific, false));
            }
        }
        Args::List(tt) if items(tt).is_empty() => {
            found.push(empty_list(cx, attr, builtin, attr.in_file(tt.syntax().text_range())));
        }
        Args::List(_) if state.all => found.push(unused_duplicate(cx, first, span, false)),
        Args::List(_) => state.specific.push(span),
        Args::NameValue(_) => {}
    }
}

/// `UsedParser::ATTRIBUTES`: one `#[used(compiler)]`, and one of `#[used(linker)]` and `#[used]`.
fn used(cx: &Cx<'_, '_>, attr: &Attr, args: &Args, used: &mut [Option<InFile<TextRange>>; 3], found: &mut Vec<Diag>) {
    const COMPILER: usize = 0;
    const LINKER: usize = 1;
    const DEFAULT: usize = 2;
    let by = match args {
        Args::Word => DEFAULT,
        Args::List(tt) => match items(tt).as_slice() {
            [item] => match word(item).as_deref() {
                Some("compiler") => COMPILER,
                Some("linker") => LINKER,
                _ => return,
            },
            _ => return,
        },
        Args::NameValue(_) => return,
    };
    let span = attr.in_file(attr.range());
    let other = match by {
        LINKER => used[DEFAULT],
        DEFAULT => used[LINKER],
        _ => None,
    };
    if let Some(prev) = other {
        found.push(unused_duplicate(cx, prev, span, false));
        return;
    }
    match used[by] {
        Some(prev) => found.push(unused_duplicate(cx, prev, span, false)),
        None => used[by] = Some(span),
    }
}

/// `invalid `crate_type` value`, with the stable crate type nearest it.
fn unknown_crate_type(cx: &Cx<'_, '_>, attr: &Attr, value: Option<&ast::Expr>) -> Option<Diag> {
    const KNOWN: &[&str] = &["bin", "dylib", "staticlib", "cdylib", "rlib", "lib", "proc-macro", "sdylib"];
    const STABLE: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro", "bin"];
    let value_node = value?;
    let text = string(Some(value_node))?;
    if KNOWN.contains(&text.as_str()) {
        return None;
    }
    let span = cx.span(attr.in_file(value_node.syntax().text_range()));
    let mut diag = Diag::lint("unknown_crate_types", "invalid `crate_type` value", span.clone());
    if let Some(candidate) = find_best_match_for_name_with_substrings(STABLE, &text, Some(5)) {
        diag.suggestion(span, "did you mean", format!("\"{candidate}\""), Applicability::MaybeIncorrect);
    }
    Some(diag)
}

/// `ambiguous_derive_helpers`: a helper of `#[proc_macro_derive(Trait, attributes(..))]` named
/// as a builtin attribute.
fn ambiguous_helpers(cx: &Cx<'_, '_>, attr: &Attr, tt: &ast::TokenTree) -> Vec<Diag> {
    let items = items(tt);
    let [trait_name, helpers] = items.as_slice() else { return Vec::new() };
    if word(trait_name).is_none() {
        return Vec::new();
    }
    let [NodeOrToken::Token(name), NodeOrToken::Node(list)] = helpers.as_slice() else { return Vec::new() };
    let Some(list) = ast::TokenTree::cast(list.clone()) else { return Vec::new() };
    if name.text() != "attributes" {
        return Vec::new();
    }
    let mut found = Vec::new();
    for helper in self::items(&list) {
        let [NodeOrToken::Token(ident)] = helper.as_slice() else { return found };
        if ident.kind() != SyntaxKind::IDENT {
            return found;
        }
        if table::is_builtin_attr_name(ident.text()) {
            let span = cx.span(attr.in_file(ident.text_range()));
            found.push(Diag::lint("ambiguous_derive_helpers", "there exists a built-in attribute with the same name", span));
        }
    }
    found
}

/// The lints of `repr`'s targets: each hint has its own (`parse_repr`'s `check_target`).
fn repr_targets(cx: &Cx<'_, '_>, krate: &Krate, owner: &Owner, attr: &Attr, found: &mut Vec<Diag>) {
    use Policy::{Allow, Warn};
    const DATA: &[Policy] =
        &[Allow(Target::Struct), Allow(Target::Enum), Allow(Target::Union), Warn(Target::MacroCall)];
    const PACKED: &[Policy] = &[Allow(Target::Struct), Allow(Target::Union), Warn(Target::MacroCall)];
    const SIMD: &[Policy] = &[Allow(Target::Struct)];
    const INT: &[Policy] = &[Allow(Target::Enum), Warn(Target::MacroCall)];
    const INTS: &[&str] = &["i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "i128", "u128", "isize", "usize"];
    let Args::List(tt) = attr.args() else { return };
    for item in items(&tt) {
        let Some(NodeOrToken::Token(hint)) = item.first() else { continue };
        let hint = hint.text();
        let (args, targets): (String, &[Policy]) = match hint {
            "align" => ("(align(...))".to_owned(), DATA),
            "packed" => ("(packed)".to_owned(), PACKED),
            "Rust" | "C" | "transparent" => (format!("({hint})"), DATA),
            "simd" => ("(simd)".to_owned(), SIMD),
            _ if INTS.contains(&hint) => (format!("({hint})"), INT),
            _ => continue,
        };
        check_target(cx, krate, owner, attr, Targets::AllowList(targets), &args, found);
    }
}

/// rustc's `check_target` of an attribute with arguments `args` (empty for most: those
/// whose target does not depend on them).
fn check_target(cx: &Cx<'_, '_>, krate: &Krate, owner: &Owner, attr: &Attr, targets: Targets, args: &str, found: &mut Vec<Diag>) {
    if let Some(warn) = targets.crate_level() {
        if warn && owner.target != Target::Crate {
            found.push(crate_level(cx, krate, owner, attr));
        }
        return;
    }
    if targets.is_allowed(owner.target) != Allowed::Warn {
        return;
    }
    let path = attr.path();
    let is_diagnostic = path.first().is_some_and(|it| it == "diagnostic");
    let lint = if attr.is("deprecated")
        && !matches!(
            owner.target,
            Target::Closure | Target::Expression | Target::Statement | Target::Arm | Target::MacroCall
        ) {
        return;
    } else if is_diagnostic {
        "misplaced_diagnostic_attributes"
    } else {
        "unused_attributes"
    };
    let name = attr.name();
    let range = if args.is_empty() { attr.path_range() } else { attr.inner_range() };
    let message = format!("the `{name}{args}` attribute cannot be used on {}", owner.target.plural_name());
    let mut diag = Diag::lint(lint, message, cx.span(attr.in_file(range)));
    diag.suggestion_with_style(
        vec![(cx.span(attr.in_file(attr.range())), String::new())],
        "remove the attribute",
        Applicability::MachineApplicable,
        SuggestionStyle::CompletelyHidden,
    );
    let (applied, only) = applied(krate, targets.allowed().collect(), owner.target);
    let only = if only { "only " } else { "" };
    diag.help(format!("the `{name}{args}` attribute can {only}be applied to {}", and_list(&applied)));
    match (path.as_slice(), owner.target) {
        ([it], Target::Static) if it == "link_name" => {
            let inner = cx.span(attr.in_file(attr.inner_range()));
            let mut parts = Vec::new();
            if !attr.is_unsafe() {
                parts.push((inner.shrink_to_lo(), "unsafe(".to_owned()));
            }
            parts.push((cx.span(attr.in_file(attr.path_range())), "export_name".to_owned()));
            if !attr.is_unsafe() {
                parts.push((inner.shrink_to_hi(), ")".to_owned()));
            }
            diag.multipart_suggestion("did you mean to use `#[export_name]`?", parts, Applicability::MaybeIncorrect);
        }
        ([it], Target::Fn | Target::Method(_)) if it == "repr" && args == "(align(...))" && krate.fn_align => {
            diag.help("use `#[rustc_align(...)]` instead");
        }
        ([it], Target::Static) if it == "repr" && args == "(align(...))" && krate.static_align => {
            diag.help("use `#[rustc_align_static(...)]` instead");
        }
        _ => {}
    }
    if !is_diagnostic {
        diag.warn(PREVIOUSLY_ACCEPTED);
    }
    if owner.target == Target::MacroCall {
        diag.note(
            "placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute",
        );
    }
    found.push(diag);
}

/// rustc's `check_crate_level` where being elsewhere is a lint: `InvalidAttrStyle`.
pub(crate) fn crate_level(cx: &Cx<'_, '_>, krate: &Krate, owner: &Owner, attr: &Attr) -> Diag {
    let name = attr.name();
    let message = match attr.inner {
        true => format!("the `#![{name}]` attribute can only be used at the crate root"),
        false => format!("crate-level attribute should be an inner attribute: add an exclamation mark: `#![{name}]`"),
    };
    let mut diag = Diag::lint("unused_attributes", message, cx.span(attr.in_file(attr.range())));
    if !attr.inner {
        let target = cx.span(InFile::new(owner.node.file_id, target_range(&owner.node.value)));
        diag.span_note(
            target,
            format!("this attribute does not have an `!`, which means it is applied to this {}", owner.target.name()),
        );
    }
    if attr.inner && krate.root.is_some_and(|root| attr.file.original_file(cx.db) != root) {
        diag.help(format!("the crate root is at `{}`", krate.root_name));
    }
    diag
}

/// rustc's span of what an attribute is applied to: the node without its attributes; a
/// macro call's ends at its arguments (`MacCall::span`), without a `;`.
fn target_range(node: &SyntaxNode) -> TextRange {
    let range = item_range(node);
    match ast::MacroCall::cast(node.clone()).and_then(|it| it.token_tree()) {
        Some(args) => TextRange::new(range.start(), args.syntax().text_range().end()),
        None => range,
    }
}

/// `UnusedBuiltinAttribute`: a builtin attribute no parser takes (a lint attribute) on a macro
/// call (`rustc_expand`'s `check_attributes`).
fn unused_builtin(cx: &Cx<'_, '_>, owner: &Owner, attr: &Attr, name: &str) -> Diag {
    let at = cx.span(attr.in_file(attr.range()));
    let mut diag = Diag::lint("unused_attributes", format!("unused attribute `{name}`"), at.clone());
    if let Some(path) = ast::MacroCall::cast(owner.node.value.clone()).and_then(|it| it.path()) {
        let text: String = path.syntax().text().to_string().chars().filter(|it| !it.is_whitespace()).collect();
        diag.span_note(
            cx.span(InFile::new(owner.node.file_id, path.syntax().text_range())),
            format!("the built-in attribute `{name}` will be ignored, since it's applied to the macro invocation `{text}`"),
        );
    }
    diag.suggestion_with_style(
        vec![(at, String::new())],
        "remove the attribute",
        Applicability::MachineApplicable,
        SuggestionStyle::CompletelyHidden,
    );
    diag
}

/// rustc's `allowed_targets_applied`: the targets an attribute can be applied to, those of
/// unstable features the crate has not enabled left out, similar ones named as a group, sorted;
/// with whether there is one.
pub(crate) fn applied(krate: &Krate, mut allowed: Vec<Target>, target: Target) -> (Vec<&'static str>, bool) {
    use super::target::MethodKind::{Inherent, Trait, TraitImpl};
    if !krate.fn_delegation {
        allowed.retain(|it| !matches!(it, Target::Delegation { .. }));
    }
    if !krate.stmt_expr_attributes {
        allowed.retain(|it| !matches!(it, Target::Expression | Target::Statement));
    }
    if !krate.extern_types {
        allowed.retain(|it| !matches!(it, Target::ForeignTy));
    }
    const FUNCTION_LIKE: &[Target] = &[
        Target::Fn,
        Target::Closure,
        Target::ForeignFn,
        Target::Method(Inherent),
        Target::Method(Trait { body: false }),
        Target::Method(Trait { body: true }),
        Target::Method(TraitImpl),
    ];
    const FUNCTION_WITH_BODY_LIKE: &[Target] = &[
        Target::Fn,
        Target::Closure,
        Target::Method(Inherent),
        Target::Method(Trait { body: true }),
        Target::Method(TraitImpl),
    ];
    const METHOD_LIKE: &[Target] = &[
        Target::Method(Inherent),
        Target::Method(Trait { body: false }),
        Target::Method(Trait { body: true }),
        Target::Method(TraitImpl),
    ];
    const IMPL_LIKE: &[Target] = &[Target::Impl { of_trait: false }, Target::Impl { of_trait: true }];
    const ADT_LIKE: &[Target] = &[Target::Struct, Target::Enum, Target::Union];
    let mut groups = Vec::new();
    for (group, name) in [
        (FUNCTION_LIKE, "functions"),
        (FUNCTION_WITH_BODY_LIKE, "functions with a body"),
        (METHOD_LIKE, "methods"),
        (IMPL_LIKE, "impl blocks"),
        (ADT_LIKE, "data types"),
    ] {
        if group.contains(&target) || allowed.iter().filter(|it| group.contains(it)).count() < 2 {
            continue;
        }
        allowed.retain(|it| !group.contains(it));
        groups.push(name);
    }
    let mut names: Vec<&'static str> = groups.into_iter().chain(allowed.iter().map(|it| it.plural_name())).collect();
    names.sort_unstable();
    names.dedup();
    let only = names.len() == 1;
    (names, only)
}

/// `UnusedDuplicate`: `unused` is a second attribute where `used` is used.
fn unused_duplicate(cx: &Cx<'_, '_>, used: InFile<TextRange>, unused: InFile<TextRange>, warning: bool) -> Diag {
    let this = cx.span(unused);
    let mut diag = Diag::lint("unused_attributes", "unused attribute", this.clone());
    diag.suggestion(this, "remove this attribute", "", Applicability::MachineApplicable);
    diag.span_note(cx.span(used), "attribute also specified here");
    if warning {
        diag.warn(PREVIOUSLY_ACCEPTED);
    }
    diag
}

/// `EmptyAttributeList` at `span`: the attribute, or for one valid without a list, the list.
fn empty_list(cx: &Cx<'_, '_>, attr: &Attr, builtin: &Builtin, span: InFile<TextRange>) -> Diag {
    let span = cx.span(span);
    let name = attr.name();
    let mut diag = Diag::lint("unused_attributes", "unused attribute", span.clone());
    let (note, remove) = match builtin.template.word {
        true => (format!("using `{name}` with an empty list is equivalent to not using a list at all"), "remove these parentheses"),
        false => (format!("using `{name}` with an empty list has no effect"), "remove this attribute"),
    };
    diag.note(note);
    diag.suggestion(span, remove, "", Applicability::MachineApplicable);
    diag
}

/// `IllFormedAttributeInput` of `ill_formed_attribute_input`, at what is between `#[` and `]`.
fn ill_formed(cx: &Cx<'_, '_>, attr: &Attr, builtin: &Builtin, help: Option<String>) -> Diag {
    let suggestions: Vec<String> =
        builtin.template.suggestions(&attr.name(), attr.is_unsafe()).into_iter().map(|it| format!("`{it}`")).collect();
    let list = and_list(&suggestions);
    let message = match suggestions.len() {
        1 => format!("attribute must be of the form {list}"),
        _ => format!("valid forms for the attribute are {list}"),
    };
    let mut diag = Diag::lint("ill_formed_attribute_input", message, cx.span(attr.in_file(attr.inner_range())));
    if let Some(lint) = help {
        diag.help(format!("if you meant to silence a warning, consider using #![allow({lint})] or #![expect({lint})]"));
    }
    diag
}

/// rustc's `StrListSepByAnd`: `a`, `a and b`, `a, b, and c`.
pub(crate) fn and_list<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [a, b] => format!("{} and {}", a.as_ref(), b.as_ref()),
        [init @ .., last] => {
            let mut out = String::new();
            for it in init {
                out.push_str(it.as_ref());
                out.push_str(", ");
            }
            out.push_str("and ");
            out.push_str(last.as_ref());
            out
        }
    }
}

/// The items of an attribute's list, between its commas, each without trivia.
pub(crate) fn items(tt: &ast::TokenTree) -> Vec<Vec<SyntaxElement>> {
    let children: Vec<SyntaxElement> = tt.syntax().children_with_tokens().collect();
    let inside = match children.as_slice() {
        [_, inside @ .., _] => inside,
        _ => &[],
    };
    let mut items = vec![Vec::new()];
    for child in inside {
        match child {
            NodeOrToken::Token(token) if token.kind() == T![,] => items.push(Vec::new()),
            NodeOrToken::Token(token) if token.kind().is_trivia() => {}
            it => items.last_mut().expect("one item at least").push(it.clone()),
        }
    }
    items.retain(|it| !it.is_empty());
    items
}

/// The item when it is one word.
pub(crate) fn word(item: &[SyntaxElement]) -> Option<String> {
    match item {
        [NodeOrToken::Token(token)] if token.kind() == SyntaxKind::IDENT || token.kind().is_keyword(span::Edition::CURRENT) => {
            Some(token.text().to_owned())
        }
        _ => None,
    }
}

/// The item when it is a path with no arguments: `foo`, `clippy::bar`.
fn path_of(item: &[SyntaxElement]) -> Option<String> {
    let mut path = String::new();
    for element in item {
        let NodeOrToken::Token(token) = element else { return None };
        match token.kind() {
            SyntaxKind::IDENT | T![::] | T![:] => path.push_str(token.text()),
            _ => return None,
        }
    }
    (!path.is_empty()).then_some(path)
}

/// The value of a string literal.
pub(crate) fn string(value: Option<&ast::Expr>) -> Option<String> {
    let ast::Expr::Literal(literal) = value? else { return None };
    match literal.kind() {
        ast::LiteralKind::String(it) => it.value().ok().map(|it| it.into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{super::target::MethodKind, *};

    #[test]
    fn a_list_is_joined_with_and() {
        assert_eq!(and_list(&["`a`"]), "`a`");
        assert_eq!(and_list(&["`a`", "`b`"]), "`a` and `b`");
        assert_eq!(and_list(&["`a`", "`b`", "`c`"]), "`a`, `b`, and `c`");
    }

    #[test]
    fn similar_targets_are_named_as_a_group() {
        let krate = Krate {
            root: None,
            root_name: String::new(),
            fn_delegation: false,
            stmt_expr_attributes: false,
            extern_types: false,
            fn_align: false,
            static_align: false,
        };
        let allowed = vec![
            Target::Fn,
            Target::Method(MethodKind::Inherent),
            Target::Method(MethodKind::TraitImpl),
            Target::Static,
            Target::Expression,
        ];
        let (names, only) = applied(&krate, allowed, Target::Struct);
        assert_eq!(names, ["functions", "methods", "statics"]);
        assert!(!only);
    }

    #[test]
    fn the_builtin_table_is_sorted_and_finds_paths() {
        assert!(table::builtin(&["inline"]).is_some());
        assert!(table::builtin(&["diagnostic", "on_unimplemented"]).is_some());
        assert!(table::builtin(&["no_such_attribute"]).is_none());
        assert!(table::is_builtin_attr_name("must_use"));
        assert!(!table::is_builtin_attr_name("serde"));
        let inline = table::builtin(&["inline"]).unwrap();
        assert_eq!(inline.template.suggestions("inline", false), ["inline", "inline(always)", "inline(never)"]);
    }
}
