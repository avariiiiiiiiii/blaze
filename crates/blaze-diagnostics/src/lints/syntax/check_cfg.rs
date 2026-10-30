//! `--check-cfg` ([`CheckCfg`]) and the lints of the cfgs a crate is built and checked with:
//! `explicit_builtin_cfgs_in_flags` on the `--cfg`s that set what rustc sets itself, and
//! `unexpected_cfgs` on every name and value a `cfg` evaluates that `--check-cfg` does not
//! expect (`unexpected`).
//!
//! Follows rustc's `compiler/rustc_interface/src/interface.rs` (`parse_cfg`, `parse_check_cfg`),
//! `compiler/rustc_session/src/config/cfg.rs` (`CheckCfg`, `ExpectedValues`, `fill_well_known`,
//! `disallow_cfgs`) and `compiler/rustc_session/src/errors.rs` (`UnexpectedBuiltinCfg`) (MIT or
//! Apache-2.0).

mod meta;
mod unexpected;
mod well_known;

use std::borrow::Cow;

use hir::{HirFileId, InFile};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{Edition, SyntaxKind, SyntaxNodePtr};

use self::{
    meta::{Item, Kind, LitKind, Parser},
    well_known::{DISALLOWED, DISALLOWED_PANIC, Start, WELL_KNOWN},
};
use super::{Emitted, Pass, Walk};
use crate::{
    cx::Cx,
    lints::CommandLine,
    lower::Phase,
    model::{Code, Diag, Level},
};

/// The names and values a crate's `cfg`s are checked against: rustc's `CheckCfg`, built from
/// the `--check-cfg` arguments in order ([`CheckCfg::add`]), with the well-known names and
/// values rustc adds to it once any is given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckCfg {
    /// Whether any `--check-cfg` is given: only then are `cfg`s checked.
    given: bool,
    /// Whether `cfg(any())` is given: any name is expected (`exhaustive_names` off).
    any_name: bool,
    /// The names given, in the order first given, each with its values.
    names: Vec<(Box<str>, Values)>,
    index: FxHashMap<Box<str>, usize>,
    /// The arguments that name an identifier the edition reserves after 2015, each with the
    /// first edition that refuses it.
    edition_keywords: Vec<(Box<str>, Edition)>,
    /// The `--cfg` arguments, in order, as given ([`CheckCfg::add_cfg`]).
    cfgs: Vec<Box<str>>,
    /// `-Z check-cfg-all-expected`: the lint lists every name and value expected, the
    /// well-known ones included.
    pub all_expected: bool,
    /// Whether the crate is built by cargo (rustc's `CARGO_CRATE_NAME` is set), which changes
    /// what the lint suggests.
    pub from_cargo: bool,
}

/// The values a name is expected with: rustc's `ExpectedValues`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Values {
    Any,
    /// `none`: the name is expected without a value. `values`: sorted, each once.
    Some { none: bool, values: Vec<Box<str>> },
}

const VISIT: &str = "visit <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more details";
const EXPECTED: &str = "expected `cfg(name, values(\"value1\", \"value2\", ... \"valueN\"))`";

/// rustc's fatal error on the `--check-cfg` argument `spec`, its notes after the message, a
/// line each.
fn invalid(spec: &str, notes: &[&str]) -> String {
    let mut out = format!("invalid `--check-cfg` argument: `{spec}`");
    for note in notes.iter().copied().chain([VISIT]) {
        out.push('\n');
        out.push_str(note);
    }
    out
}

/// rustc's fatal error on an argument of `spec` (`error!(in arg, ..)`).
fn invalid_in(spec: &str, arg: &Item<'_>, reason: &str) -> String {
    let printed = meta::to_string(arg);
    let what = match arg {
        Item::Lit(lit) => {
            let (article, descr) = lit.kind.describe();
            format!("`{printed}` is {article} {descr} literal")
        }
        Item::Meta(_) => format!("`{printed}` is invalid"),
    };
    invalid(spec, &[&what, reason])
}

impl CheckCfg {
    /// Adds the `--check-cfg` argument `spec`, as rustc's `parse_check_cfg` does, or says why
    /// rustc refuses it: the message on the first line, a note on each after
    /// ([`error_diag`]). On an error nothing is added.
    pub fn add(&mut self, spec: &str) -> Result<(), String> {
        let expected = || invalid(spec, &[EXPECTED]);
        let toks = meta::lex(spec, Edition::Edition2015).ok_or_else(expected)?;
        let mut parser = Parser::new(&toks, Edition::Edition2015);
        let meta = parser.meta().filter(|_| parser.at_end()).ok_or_else(expected)?;
        let Kind::List(args) = &meta.kind else { return Err(expected()) };
        if !meta.has_name("cfg") {
            return Err(expected());
        }

        let mut names: Vec<&str> = Vec::new();
        let mut values: FxHashSet<Option<Cow<'_, str>>> = FxHashSet::default();
        let mut any_specified = false;
        let mut values_specified = false;
        let mut values_any_specified = false;
        for arg in args {
            match arg {
                Item::Meta(it) if matches!(it.kind, Kind::Word) && it.word().is_some() => {
                    if values_specified {
                        return Err(invalid(spec, &["`cfg()` names cannot be after values"]));
                    }
                    let ident = it.word().unwrap();
                    if ident.keyword {
                        return Err(invalid(spec, &["malformed `cfg` input, expected a valid identifier"]));
                    }
                    names.push(ident.name);
                }
                Item::Lit(lit) if lit.kind == LitKind::Bool => {
                    if values_specified {
                        return Err(invalid(spec, &["`cfg()` names cannot be after values"]));
                    }
                    names.push(lit.text);
                }
                Item::Meta(it) if it.has_name("any") && matches!(it.kind, Kind::List(_)) => {
                    if any_specified {
                        return Err(invalid(spec, &["`any()` cannot be specified multiple times"]));
                    }
                    any_specified = true;
                    if matches!(&it.kind, Kind::List(args) if !args.is_empty()) {
                        return Err(invalid_in(spec, arg, "`any()` takes no argument"));
                    }
                }
                Item::Meta(it) if it.has_name("values") && matches!(it.kind, Kind::List(_)) => {
                    if names.is_empty() {
                        return Err(invalid(spec, &["`values()` cannot be specified before the names"]));
                    } else if values_specified {
                        return Err(invalid(spec, &["`values()` cannot be specified multiple times"]));
                    }
                    values_specified = true;
                    let Kind::List(args) = &it.kind else { continue };
                    for arg in args {
                        match arg {
                            Item::Lit(lit) if lit.kind == LitKind::Str => {
                                let Some(value) = lit.str_value() else { return Err(expected()) };
                                values.insert(Some(value));
                            }
                            Item::Meta(it) if it.has_name("any") && matches!(it.kind, Kind::List(_)) => {
                                if values_any_specified {
                                    return Err(invalid_in(
                                        spec,
                                        arg,
                                        "`any()` in `values()` cannot be specified multiple times",
                                    ));
                                }
                                values_any_specified = true;
                                if matches!(&it.kind, Kind::List(args) if !args.is_empty()) {
                                    return Err(invalid_in(spec, arg, "`any()` in `values()` takes no argument"));
                                }
                            }
                            Item::Meta(it) if it.has_name("none") && matches!(it.kind, Kind::List(_)) => {
                                values.insert(None);
                                if matches!(&it.kind, Kind::List(args) if !args.is_empty()) {
                                    return Err(invalid_in(spec, arg, "`none()` in `values()` takes no argument"));
                                }
                            }
                            _ => {
                                return Err(invalid_in(
                                    spec,
                                    arg,
                                    "`values()` arguments must be string literals, `none()` or `any()`",
                                ));
                            }
                        }
                    }
                }
                _ => {
                    return Err(invalid_in(
                        spec,
                        arg,
                        "`cfg()` arguments must be simple identifiers, `any()` or `values(...)`",
                    ));
                }
            }
        }

        if !values_specified && !any_specified {
            values.insert(None);
        } else if !values.is_empty() && values_any_specified {
            return Err(invalid(
                spec,
                &["`values()` arguments cannot specify string literals and `any()` at the same time"],
            ));
        }
        if any_specified && !(names.is_empty() && values.is_empty() && !values_specified && !values_any_specified) {
            return Err(invalid(spec, &["`cfg(any())` can only be provided in isolation"]));
        }

        self.given = true;
        if let Some(since) = reserved_since(&toks) {
            self.edition_keywords.push((spec.into(), since));
        }
        if any_specified {
            self.any_name = true;
            return Ok(());
        }
        let none = values.contains(&None);
        let mut strings: Vec<&str> = values.iter().filter_map(|it| it.as_deref()).collect();
        strings.sort_unstable();
        for name in names {
            let at = match self.index.get(name) {
                Some(&at) => at,
                None => {
                    let values = match values_any_specified {
                        true => Values::Any,
                        false => Values::Some { none: false, values: Vec::new() },
                    };
                    self.index.insert(name.into(), self.names.len());
                    self.names.push((name.into(), values));
                    self.names.len() - 1
                }
            };
            let entry = &mut self.names[at].1;
            match entry {
                Values::Some { .. } if values_any_specified => *entry = Values::Any,
                Values::Some { none: has_none, values } => {
                    *has_none |= none;
                    for &value in &strings {
                        if let Err(at) = values.binary_search_by(|it| (**it).cmp(value)) {
                            values.insert(at, value.into());
                        }
                    }
                }
                Values::Any => {}
            }
        }
        Ok(())
    }

    /// Records the `--cfg` argument `spec`, which [`parse_cfg`] reads with the crate's edition:
    /// `explicit_builtin_cfgs_in_flags` is on those that set what rustc sets itself.
    pub fn add_cfg(&mut self, spec: &str) {
        self.cfgs.push(spec.into());
    }

    /// Whether a name no `--check-cfg` gives is linted (`exhaustive_names`).
    fn exhaustive_names(&self) -> bool {
        self.given && !self.any_name
    }

    /// What `name` is expected with, the well-known values added (`fill_well_known`: a name
    /// given keeps what it was given, and gets the well-known values too).
    fn expected(&self, name: &str) -> Option<Expected<'_>> {
        let known = well_known(name);
        let user = self.index.get(name).map(|&it| &self.names[it].1);
        Some(match (user, known) {
            (Some(Values::Any), _) => Expected::Any,
            (Some(Values::Some { none, values }), known) => {
                Expected::Some(Set { none: *none, user: values, known: known.map_or(&[], |it| it.2) })
            }
            (None, Some((_, start, known))) => {
                Expected::Some(Set { none: *start == Start::NoValue, user: &[], known })
            }
            (None, None) => return None,
        })
    }

    /// Every name expected, in the order rustc interns them: the well-known ones (which rustc
    /// has from the start) by name, then those given, in the order given.
    fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = WELL_KNOWN.iter().map(|it| it.0).collect();
        names.sort_unstable();
        names.extend(self.names.iter().map(|it| &*it.0).filter(|it| well_known(it).is_none()));
        names
    }
}

/// What a name is expected with.
enum Expected<'a> {
    Any,
    Some(Set<'a>),
}

/// The values a name is expected with: those given and the well-known ones.
struct Set<'a> {
    none: bool,
    user: &'a [Box<str>],
    known: &'static [&'static str],
}

impl<'a> Set<'a> {
    fn contains(&self, value: Option<&str>) -> bool {
        match value {
            None => self.none,
            Some(value) => {
                self.user.binary_search_by(|it| (**it).cmp(value)).is_ok() || known_contains(self.known, value)
            }
        }
    }

    /// The values, sorted, each once, `none` left out.
    fn values(&self) -> Vec<&'a str> {
        let mut values: Vec<&'a str> = self.user.iter().map(|it| &**it).chain(self.known.iter().copied()).collect();
        values.sort_unstable();
        values.dedup();
        values
    }
}

/// The well-known name `name`, if it is one.
fn well_known(name: &str) -> Option<&'static (&'static str, Start, &'static [&'static str])> {
    WELL_KNOWN.iter().find(|it| it.0 == name)
}

/// Whether the well-known values `known` have `value`: the long lists are sorted.
fn known_contains(known: &[&str], value: &str) -> bool {
    if known.len() > SORTED_FROM {
        known.binary_search(&value).is_ok()
    } else {
        known.contains(&value)
    }
}

/// The length from which the well-known value lists are sorted.
const SORTED_FROM: usize = 32;

/// The first edition that reserves an identifier `toks` names unraw, which rustc's parser then
/// refuses where it parses the argument.
fn reserved_since(toks: &[meta::Tok<'_>]) -> Option<Edition> {
    toks.iter()
        .filter(|it| it.kind == SyntaxKind::IDENT)
        .filter_map(|it| match it.text {
            "async" | "await" | "dyn" | "try" => Some(Edition::Edition2018),
            "gen" => Some(Edition::Edition2024),
            _ => None,
        })
        .min()
}

/// The `--cfg` argument `spec`: its name and value, as rustc's `parse_cfg` reads it, or
/// rustc's fatal error on it.
pub fn parse_cfg(spec: &str, edition: Edition) -> Result<(String, Option<String>), String> {
    let error = |reason: &str| Err(format!("invalid `--cfg` argument: `{spec}` ({reason})"));
    if let Some(toks) = meta::lex(spec, edition) {
        let mut parser = Parser::new(&toks, edition);
        if let Some(meta) = parser.meta()
            && parser.at_end()
        {
            if meta.root || meta.path.len() != 1 {
                return error("argument key must be an identifier");
            }
            match &meta.kind {
                Kind::List(_) => {}
                Kind::NameValue(lit) if lit.kind != LitKind::Str => {
                    return error("argument value must be a string");
                }
                Kind::NameValue(_) | Kind::Word => {
                    let ident = meta.path[0];
                    if ident.keyword {
                        return error("malformed `cfg` input, expected a valid identifier");
                    }
                    let value = match &meta.kind {
                        Kind::NameValue(lit) => lit.str_value().map(Cow::into_owned),
                        _ => None,
                    };
                    return Ok((ident.name.to_owned(), value));
                }
            }
        }
    }
    if spec.contains('=') && !spec.contains("=\"") && !spec.ends_with('"') {
        error(concat!(
            r#"expected `key` or `key="value"`, ensure escaping is appropriate"#,
            r#" for your shell, try 'key="value"' or key=\"value\""#
        ))
    } else {
        error(r#"expected `key` or `key="value"`"#)
    }
}

/// The fatal error `error` of [`CheckCfg::add`] or [`parse_cfg`] as rustc writes it: its first
/// line the message, each other a note.
pub fn error_diag(error: &str) -> Diag {
    let mut lines = error.split('\n');
    let mut diag = Diag::new(Level::Error, None, lines.next().unwrap_or_default());
    for note in lines {
        diag.note(note);
    }
    diag
}

/// `name` as rustc writes an identifier of `edition`: `r#` before one the edition reserves
/// (`Ident`'s `Display`, `is_raw_guess`).
fn ident(name: &str, edition: Edition) -> Cow<'_, str> {
    let raw = !matches!(name, "crate" | "self" | "Self" | "super")
        && SyntaxKind::from_keyword(name, edition).is_some();
    match raw {
        true => Cow::Owned(format!("r#{name}")),
        false => Cow::Borrowed(name),
    }
}

/// `items` as rustc lists them in a message (`DiagSymbolList`): "`a`", "`a` and `b`",
/// "`a`, `b`, and `c`".
fn list<S: AsRef<str>>(items: &[S]) -> String {
    let mut out = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(match items.len() {
                2 => " and ",
                n if i == n - 1 => ", and ",
                _ => ", ",
            });
        }
        out.push('`');
        out.push_str(item.as_ref());
        out.push('`');
    }
    out
}

/// The lints of the crate's `--cfg`s and `cfg`s, in the order rustc buffers them; or, when
/// rustc refuses a `--cfg` or `--check-cfg` argument for the crate's edition, its fatal error
/// alone.
pub(crate) fn lints(cx: &Cx<'_, '_>, cmd: &CommandLine, walk: &Walk<'_>) -> Vec<Emitted> {
    let mut emitted = Vec::new();
    let check = &cmd.check_cfg;
    let mut cfgs: Vec<(String, Option<String>)> = Vec::new();
    for spec in &check.cfgs {
        match parse_cfg(spec, cx.edition) {
            Ok(cfg) => cfgs.push(cfg),
            Err(error) => return vec![Emitted::unplaced(error_diag(&error), Phase::Parse, Pass::Buffered)],
        }
    }
    if let Some((spec, _)) = check.edition_keywords.iter().find(|it| cx.edition >= it.1) {
        let error = invalid(spec, &[EXPECTED]);
        return vec![Emitted::unplaced(error_diag(&error), Phase::Parse, Pass::Buffered)];
    }
    let Some(root) = cx.krate.root_module(cx.db).as_source_file_id(cx.db) else { return emitted };
    let Some(root) = walk.tree(HirFileId::from(root)) else { return emitted };
    let krate = root.ptr(&root.root);
    builtin(cx, &cfgs, krate, &mut emitted);
    if check.given {
        unexpected::lints(cx, check, walk, &mut emitted);
    }
    emitted
}

/// `explicit_builtin_cfgs_in_flags` on each `--cfg` that sets a cfg rustc sets itself
/// (`disallow_cfgs`), each once, at the crate.
fn builtin(cx: &Cx<'_, '_>, cfgs: &[(String, Option<String>)], krate: InFile<SyntaxNodePtr>, out: &mut Vec<Emitted>) {
    let mut seen: FxHashSet<(&str, Option<&str>)> = FxHashSet::default();
    for (name, value) in cfgs {
        if !seen.insert((name, value.as_deref())) {
            continue;
        }
        let flag = match name.as_str() {
            "panic" => value.as_deref().filter(|it| DISALLOWED_PANIC.contains(it)).map(|_| "-C panic"),
            name => DISALLOWED
                .iter()
                .find(|it| it.0 == name && it.1.is_none_or(|with_value| with_value == value.is_some()))
                .map(|it| it.2),
        };
        let Some(flag) = flag else { continue };
        let cfg = match value {
            Some(value) => format!(r#"{name}="{value}""#),
            None => name.clone(),
        };
        let mut diag = Diag::new(
            Level::Warning,
            Some(Code::Lint("explicit_builtin_cfgs_in_flags".to_owned())),
            format!("unexpected `--cfg {cfg}` flag"),
        );
        diag.note(format!(
            "config `{}` is only supposed to be controlled by `{flag}`",
            ident(name, cx.edition)
        ));
        diag.note("manually setting a built-in cfg can and does create incoherent behaviors");
        out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, krate));
    }
}

#[cfg(test)]
mod tests {
    use super::{SORTED_FROM, WELL_KNOWN};

    #[test]
    fn long_well_known_value_lists_are_sorted() {
        for (name, _, values) in WELL_KNOWN {
            if values.len() > SORTED_FROM {
                assert!(values.is_sorted(), "the values of `{name}` are not sorted");
            }
        }
    }
}
