//! `non_snake_case` and `non_upper_case_globals` of the names hir's `IncorrectCase` says
//! nothing of (`crates/hir-ide/src/diagnostics/decl_check.rs`, lowered in
//! `lower/incorrect_case.rs`), so that each name rustc lints is linted once:
//!
//! - `IncorrectCase`: modules, functions and methods (not of trait impls, not `#[no_mangle]`
//!   of a foreign ABI), the bindings and parameters of functions' bodies, fields, constants and
//!   associated constants (not of trait impls), statics (not `extern`, not `#[no_mangle]`).
//! - here: the crate's name; lifetime parameters; const parameters; the parameters named in
//!   function pointer types; the bindings of the bodies of constants, statics and enum
//!   discriminants; a single-segment path pattern naming a constant ("constant in pattern");
//!   a trait's methods that are `#[no_mangle]` of a foreign ABI, which rustc lints and
//!   `IncorrectCase` passes over.
//!
//! `non_camel_case_types` is rustc's early lint pass's, linted with the syntax lints.
//!
//! Follows `compiler/rustc_lint/src/nonstandard_style.rs` (`NonSnakeCase`,
//! `NonUpperCaseGlobals`) and `compiler/rustc_lint/src/lints.rs` (`NonSnakeCaseDiag`,
//! `NonUpperCaseGlobal`) (MIT or Apache-2.0).

use base_db::TargetKind;
use blaze_items::Fact;
use hir::{InFile, ModuleDef, PathResolution};
use span::Edition;
use syntax::{
    AstNode, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasGenericArgs, HasName},
};

use super::walk::{self, Container, Item};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Code, Diag, Level, MultiSpan, Span, SuggestionStyle},
};

/// What a name's lint read: the item's signature or code, by where the name is.
type Read = Vec<(Fact, String)>;

/// rustc's `check_mod` of the crate root: the crate's name, unless every crate type it is
/// built as is an executable (#45127).
pub(super) fn krate(cx: &Cx<'_, '_>) -> Vec<Found> {
    let executable = matches!(
        cx.krate.target_kind(cx.db),
        TargetKind::Bin | TargetKind::Example | TargetKind::Test | TargetKind::Bench | TargetKind::BuildScript
    );
    if executable {
        return Vec::new();
    }
    let root = cx.krate.root_module(cx.db);
    let file = root.definition_source_file_id(cx.db);
    let Some(source_file) = ast::SourceFile::cast(cx.sema.parse_or_expand(file)) else { return Vec::new() };
    let at = InFile::new(file, SyntaxNodePtr::new(source_file.syntax()));
    let read: Read = vec![(Fact::Name(cx.module_key(root)), "the name of the crate".to_owned())];
    // `#![crate_name = ".."]`: the span of the name between its quotes.
    if let Some((ast::Meta::KeyValueMeta(kv), _)) = walk::attr(cx, &source_file, "crate_name")
        && let Some(ast::Expr::Literal(literal)) = kv.expr()
    {
        let token = literal.token();
        let text = token.text();
        let (Some(left), Some(right)) = (text.find('"'), text.rfind('"')) else { return Vec::new() };
        if right <= left {
            return Vec::new();
        }
        let start = token.text_range().start();
        let range = TextRange::new(start + TextSize::from(left as u32 + 1), start + TextSize::from(right as u32));
        let span = cx.span(InFile::new(file, range));
        return snake(cx, "crate", &text[left + 1..right], Some(span), at, &read).into_iter().collect();
    }
    // As cargo runs rustc, `--crate-name` names it, with no span.
    let Some(name) = cx.krate.display_name(cx.db) else { return Vec::new() };
    snake(cx, "crate", name.crate_name().symbol().as_str(), None, at, &read).into_iter().collect()
}

/// What rustc's `NonSnakeCase` and `NonUpperCaseGlobals` lint of `item` that `IncorrectCase`
/// does not, in the order rustc visits its nodes.
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
    let mut found = Vec::new();
    let def = super::item_id(cx, item);
    let read = |fact: fn(blaze_items::ItemKey) -> Fact, what: &str| -> Read {
        match &def {
            Some((id, name)) => vec![(fact(cx.key(*id)), format!("the {what} of `{name}`"))],
            None => Vec::new(),
        }
    };
    if let ast::Item::Fn(it) = &item.node.value
        && let Container::Trait(_) = item.container
        && !is_rust_abi(it.abi())
        && walk::attr(cx, it, "no_mangle").is_some()
        && let Some(name) = it.name()
    {
        let span = cx.span(item.node.with_value(name.syntax().text_range()));
        found.extend(snake(cx, "trait method", name.text(), Some(span), item.ptr(), &read(Fact::Name, "name")));
    }
    let bindings = matches!(&item.node.value, ast::Item::Const(_) | ast::Item::Static(_) | ast::Item::Enum(_));
    for node in &item.nodes {
        let at = node.as_ref().map(SyntaxNodePtr::new);
        if let Some(param) = ast::LifetimeParam::cast(node.value.clone())
            && let Some(lifetime) = param.lifetime()
        {
            let span = cx.span(node.with_value(lifetime.syntax().text_range()));
            found.extend(snake(cx, "lifetime", lifetime.text(), Some(span), at, &read(Fact::Signature, "signature")));
        } else if let Some(param) = ast::ConstParam::cast(node.value.clone())
            && let Some(name) = param.name()
        {
            let span = cx.span(node.with_value(name.syntax().text_range()));
            let usages = const_param_usages(cx, item, node.with_value(&param), name.text());
            let read = read(Fact::Signature, "signature");
            found.extend(upper(cx, "const parameter", name.text(), span, true, usages, at, &read));
        } else if let Some(ty) = ast::FnPtrType::cast(node.value.clone()) {
            for param in ty.param_list().into_iter().flat_map(|it| it.params()) {
                if let Some(ast::Pat::IdentPat(pat)) = param.pat()
                    && let Some(name) = pat.name()
                {
                    let span = cx.span(node.with_value(name.syntax().text_range()));
                    let at = node.with_value(SyntaxNodePtr::new(pat.syntax()));
                    found.extend(snake(cx, "variable", name.text(), Some(span), at, &read(Fact::Signature, "signature")));
                }
            }
        } else if let Some(pat) = ast::IdentPat::cast(node.value.clone()) {
            found.extend(ident_pat(cx, node.with_value(&pat), bindings, &read(Fact::Code, "code")));
        } else if let Some(pat) = ast::PathPat::cast(node.value.clone())
            && let Some(path) = pat.path()
            && path.qualifier().is_none()
            && let Some(segment) = path.segment()
            && segment.generic_arg_list().is_none()
            && let Some(name) = segment.name_ref()
            && let Some(PathResolution::Def(ModuleDef::Const(_))) = cx.sema.resolve_path(&path)
        {
            let span = cx.span(node.with_value(name.syntax().text_range()));
            let read = read(Fact::Code, "code");
            found.extend(upper(cx, "constant in pattern", name.text(), span, false, Vec::new(), at, &read));
        }
    }
    found
}

/// An identifier pattern: a constant it names, or a binding of a body `IncorrectCase` does
/// not look into (`bindings`: the item is a constant, a static or an enum's discriminants).
fn ident_pat(cx: &Cx<'_, '_>, pat: InFile<&ast::IdentPat>, bindings: bool, read: &Read) -> Option<Found> {
    let name = pat.value.name()?;
    let span = cx.span(pat.with_value(name.syntax().text_range()));
    let at = pat.map(|it| SyntaxNodePtr::new(it.syntax()));
    // The parameters of a function pointer type are linted with it.
    if pat.value.syntax().ancestors().any(|it| ast::FnPtrType::can_cast(it.kind())) {
        return None;
    }
    let plain = pat.value.pat().is_none() && pat.value.ref_token().is_none() && pat.value.mut_token().is_none();
    if plain && let Some(def) = cx.sema.resolve_bind_pat_to_const(pat.value) {
        return match def {
            ModuleDef::Const(_) => upper(cx, "constant in pattern", name.text(), span, false, Vec::new(), at, read),
            _ => None,
        };
    }
    if !bindings {
        return None;
    }
    // A shorthand field pattern names the field, not a new binding: the field is linted.
    let shorthand =
        pat.value.syntax().parent().and_then(ast::RecordPatField::cast).is_some_and(|it| it.name_ref().is_none());
    if shorthand {
        return None;
    }
    snake(cx, "variable", name.text(), Some(span), at, read)
}

/// The paths in `item` naming the const parameter `param`, which the suggestion renames too.
fn const_param_usages(cx: &Cx<'_, '_>, item: &Item, param: InFile<&ast::ConstParam>, name: &str) -> Vec<Span> {
    let Some(def) = cx.sema.to_def2(param) else { return Vec::new() };
    let mut usages = Vec::new();
    for path in item.own::<ast::Path>() {
        if path.value.qualifier().is_some() {
            continue;
        }
        let Some(name_ref) = path.value.segment().and_then(|it| it.name_ref()) else { continue };
        if name_ref.text() != name {
            continue;
        }
        if let Some(PathResolution::ConstParam(it)) = cx.sema.resolve_path(&path.value)
            && it == def
        {
            usages.push(cx.span(path.with_value(name_ref.syntax().text_range())));
        }
    }
    usages
}

/// Whether a function of `abi` is of the Rust ABI: no `extern`, or `extern "Rust"`.
pub(super) fn is_rust_abi(abi: Option<ast::Abi>) -> bool {
    match abi {
        None => true,
        Some(abi) => abi.abi_string().is_some_and(|it| it.text().trim_matches('"') == "Rust"),
    }
}

/// rustc's `check_snake_case`: the lint of `name`, of the kind `sort`, when it is not snake
/// case. `span` is `None` for the crate's name given on the command line.
fn snake(
    cx: &Cx<'_, '_>,
    sort: &str,
    name: &str,
    span: Option<Span>,
    at: InFile<SyntaxNodePtr>,
    read: &Read,
) -> Option<Found> {
    if is_snake_case(name) {
        return None;
    }
    let message = format!("{sort} `{name}` should have a snake case name");
    let sc = to_snake_case(name);
    let mut diag = match &span {
        Some(span) => Diag::lint("non_snake_case", message, span.clone()),
        None => {
            let mut diag = Diag::new(Level::Warning, Some(Code::Lint("non_snake_case".to_owned())), message);
            diag.span = MultiSpan::default();
            diag
        }
    };
    match span {
        Some(span) if name == sc => {
            diag.span_label(span, "should have a snake_case name");
        }
        None if name == sc => {}
        None => {
            diag.help(format!("convert the identifier to snake case: `{sc}`"));
        }
        Some(span) if is_reserved(&sc, cx.edition) => {
            if can_be_raw(&sc) {
                diag.suggestion(
                    span,
                    "rename the identifier or convert it to a snake case raw identifier",
                    format!("r#{sc}"),
                    Applicability::MaybeIncorrect,
                );
            } else {
                diag.note(format!("`{sc}` cannot be used as a raw identifier"));
                diag.suggestion(span, "rename the identifier", "", Applicability::MaybeIncorrect);
            }
        }
        Some(span) => {
            diag.suggestion(span, "convert the identifier to snake case", sc, Applicability::MaybeIncorrect);
        }
    }
    for (fact, what) in read {
        diag.fact(*fact, what.clone());
    }
    Some(Found::lint(diag, Phase::Late, at))
}

/// rustc's `check_upper_case`: the lint of `name`, of the kind `sort`, when it is not upper
/// case; `can_change_usages` when it is not exported, and then the paths naming it renamed too.
#[allow(clippy::too_many_arguments)]
fn upper(
    cx: &Cx<'_, '_>,
    sort: &str,
    name: &str,
    span: Span,
    can_change_usages: bool,
    usages: Vec<Span>,
    at: InFile<SyntaxNodePtr>,
    read: &Read,
) -> Option<Found> {
    let _ = cx;
    if name.chars().all(|c| c.to_uppercase().eq([c])) {
        return None;
    }
    let uc = to_snake_case(name).to_uppercase();
    let message = format!("{sort} `{name}` should have an upper case name");
    let mut diag = Diag::lint("non_upper_case_globals", message, span.clone());
    if name != uc {
        let applicability = match can_change_usages {
            true => Applicability::MachineApplicable,
            false => Applicability::MaybeIncorrect,
        };
        diag.suggestion(span, "convert the identifier to upper case", uc.clone(), applicability);
        if can_change_usages {
            for usage in usages {
                diag.suggestion_with_style(
                    vec![(usage, uc.clone())],
                    "convert the identifier to upper case",
                    Applicability::MachineApplicable,
                    SuggestionStyle::CompletelyHidden,
                );
            }
        }
    } else {
        diag.span_label(span, "should have an UPPER_CASE name");
    }
    for (fact, what) in read {
        diag.fact(*fact, what.clone());
    }
    Some(Found::lint(diag, Phase::Late, at))
}

/// rustc's `is_snake_case` of `NonSnakeCase::check_snake_case`.
pub(super) fn is_snake_case(ident: &str) -> bool {
    if ident.is_empty() {
        return true;
    }
    let ident = ident.trim_start_matches('\'');
    let ident = ident.trim_matches('_');
    if ident.contains("__") {
        return false;
    }
    ident.chars().all(|c| c.to_lowercase().eq([c]))
}

/// rustc's `NonSnakeCase::to_snake_case`.
pub(super) fn to_snake_case(mut name: &str) -> String {
    let mut words = vec![];
    name = name.trim_start_matches(|c: char| {
        if c == '_' {
            words.push(String::new());
            true
        } else {
            false
        }
    });
    for s in name.split('_') {
        let mut last_upper = false;
        let mut buf = String::new();
        if s.is_empty() {
            continue;
        }
        for ch in s.chars() {
            if !buf.is_empty() && buf != "'" && (ch.is_uppercase() || is_titlecase(ch)) && !last_upper {
                words.push(buf.to_lowercase());
                buf = String::new();
            }
            last_upper = ch.is_uppercase() || is_titlecase(ch);
            buf.push(ch);
        }
        words.push(buf.to_lowercase());
    }
    words.join("_")
}

/// `char::is_titlecase`, which is unstable: the letters of Unicode's general category `Lt`.
fn is_titlecase(c: char) -> bool {
    matches!(
        c as u32,
        0x01c5 | 0x01c8 | 0x01cb | 0x01f2 | 0x1f88..=0x1f8f | 0x1f98..=0x1f9f | 0x1fa8..=0x1faf | 0x1fbc | 0x1fcc | 0x1ffc
    )
}

/// rustc's `Symbol::is_reserved`: a special identifier or a keyword of `edition`.
fn is_reserved(name: &str, edition: Edition) -> bool {
    const ALWAYS: &[&str] = &[
        "$crate", "{{root}}", "_", "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
        "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "abstract", "become",
        "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield",
    ];
    ALWAYS.contains(&name)
        || (matches!(name, "async" | "await" | "dyn" | "try") && edition >= Edition::Edition2018)
        || (name == "gen" && edition >= Edition::Edition2024)
}

/// rustc's `Symbol::can_be_raw`.
fn can_be_raw(name: &str) -> bool {
    !matches!(name, "" | "_" | "super" | "self" | "Self" | "crate" | "{{root}}" | "$crate")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_is_rustcs() {
        assert!(is_snake_case("foo_bar"));
        assert!(is_snake_case("_foo"));
        assert!(is_snake_case("'a"));
        assert!(is_snake_case("x86_64"));
        assert!(!is_snake_case("foo__bar"));
        assert!(!is_snake_case("FooBar"));
        assert!(!is_snake_case("'FooBar"));
        assert_eq!(to_snake_case("FooBar"), "foo_bar");
        assert_eq!(to_snake_case("'FooBar"), "'foo_bar");
        assert_eq!(to_snake_case("__FooBar"), "__foo_bar");
        assert_eq!(to_snake_case("HTTPServer"), "httpserver");
        assert_eq!(to_snake_case("NonSnakeCase"), "non_snake_case");
        assert_eq!(to_snake_case("x").to_uppercase(), "X");
    }

    #[test]
    fn reserved_names_follow_the_edition() {
        assert!(is_reserved("fn", Edition::Edition2015));
        assert!(!is_reserved("async", Edition::Edition2015));
        assert!(is_reserved("async", Edition::Edition2018));
        assert!(!is_reserved("gen", Edition::Edition2021));
        assert!(is_reserved("gen", Edition::Edition2024));
        assert!(can_be_raw("fn"));
        assert!(!can_be_raw("self"));
    }
}
