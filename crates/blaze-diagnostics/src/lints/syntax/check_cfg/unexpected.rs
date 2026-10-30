//! `unexpected_cfgs`: each name and value a `cfg` evaluates (`#[cfg]`, `#[cfg_attr]`, `cfg!`,
//! `cfg_select!`) that `--check-cfg` does not expect, as rustc parses each predicate: every
//! leaf of one that parses up to where it stops, the attributes of a node in order up to the
//! first `cfg` that removes it, each lint at the node rustc buffers it for.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/cfg.rs` (`parse_cfg_entry`,
//! `parse_cfg_entry_target`, `parse_name_value`), `compiler/rustc_attr_parsing/src/check_cfg.rs`
//! (`unexpected_cfg_name`, `unexpected_cfg_value`), `compiler/rustc_attr_parsing/src/diagnostics.rs`
//! (`UnexpectedCfgName`, `UnexpectedCfgValue` and their parts),
//! `compiler/rustc_attr_parsing/src/attributes/cfg_select.rs` (`parse_cfg_select`),
//! `compiler/rustc_parse/src/parser/cfg_select.rs` (`parse_delimited_token_tree`),
//! `compiler/rustc_builtin_macros/src/cfg.rs` (`parse_cfg`), `compiler/rustc_expand/src/config.rs`
//! (`StripUnconfigured`), `compiler/rustc_expand/src/expand.rs` (`assign_id!`: the
//! `lint_node_id` a lint is buffered at) and `compiler/rustc_span/src/lib.rs`
//! (`Span::in_external_macro`) (MIT or Apache-2.0).

use std::{borrow::Cow, cmp::Reverse};

use cfg::{CfgExpr, CfgOptions};
use hir::{HirFileId, InFile, MacroCallId};
use hir_expand::{MacroCallKind, MacroDefKind, builtin::BuiltinFnLikeExpander};
use smallvec::SmallVec;
use syntax::{
    AstNode, Edition, SyntaxKind,
    SyntaxKind::{
        ATTR, BANG, BLOCK_EXPR, COMMA, CONST_PARAM, FAT_ARROW, L_BRACK, L_CURLY, L_PAREN, LET_STMT,
        LIFETIME_PARAM, MACRO_CALL, MACRO_EXPR, MACRO_PAT, MACRO_TYPE, MATCH_ARM, PARAM, POUND,
        R_BRACK, R_CURLY, R_PAREN, RECORD_EXPR_FIELD, RECORD_FIELD, RECORD_PAT_FIELD, SELF_PARAM,
        TUPLE_FIELD, TYPE_PARAM, UNDERSCORE, VARIANT, WHERE_PRED,
    },
    SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasLoopBody as _},
};

use super::{
    CheckCfg, Expected, Set, ident, list,
    meta::{self, Item, Kind, LitKind, Parser, Tok},
};
use crate::{
    cx::Cx,
    lints::{
        edit_distance::find_best_match_for_name,
        syntax::{Emitted, Pass, Tree, Walk, source_fact},
    },
    lower::Phase,
    model::{Applicability, Diag, MultiSpan, Span, Sublevel},
};

/// rustc's `MAX_CHECK_CFG_NAMES_OR_VALUES`.
const MAX_LISTED: usize = 35;

const SEE_CARGO: &str = "see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration";
const SEE_RUSTC: &str =
    "see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration";

pub(super) fn lints(cx: &Cx<'_, '_>, check: &CheckCfg, walk: &Walk<'_>, out: &mut Vec<Emitted>) {
    let mut linter = Linter { cx, check, cfg: walk.cfg, out };
    for tree in &walk.trees {
        let edition = tree.file.edition(cx.db);
        let mut skipped: Vec<&SyntaxNode> = tree.off.iter().chain(&tree.replaced).collect();
        skipped.sort_by_key(|it| (it.text_range().start(), Reverse(it.text_range().end())));
        let mut skipped = skipped.into_iter().peekable();
        for node in &tree.nodes {
            while let Some(it) = skipped.next_if(|it| before(it, node)) {
                linter.owner(tree, edition, it);
            }
            linter.owner(tree, edition, node);
            if ast::CfgPredExpr::can_cast(node.kind()) || ast::CfgPredPat::can_cast(node.kind()) {
                linter.cfg_macro(tree, edition, node);
            }
        }
        for it in skipped {
            linter.owner(tree, edition, it);
        }
        if let Some(call) = tree.call
            && matches!(call.loc(cx.db).def.kind, MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::CfgSelect))
        {
            linter.cfg_select(call);
        }
    }
}

/// Whether `a` comes before `b` in a pre-order walk.
fn before(a: &SyntaxNode, b: &SyntaxNode) -> bool {
    let (a, b) = (a.text_range(), b.text_range());
    a.start() < b.start() || (a.start() == b.start() && a.end() > b.end())
}

struct Linter<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    check: &'a CheckCfg,
    cfg: &'a CfgOptions,
    out: &'a mut Vec<Emitted>,
}

/// A name, and maybe a value, a predicate tests (`CfgEntry::NameValue`).
struct Leaf<'t> {
    /// Without `r#`.
    name: Cow<'t, str>,
    raw: bool,
    name_range: TextRange,
    value: Option<(Cow<'t, str>, TextRange)>,
    range: TextRange,
}

/// Which predicate an attribute or macro has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    /// `cfg(predicate)`: the one item of a list.
    Cfg,
    /// `cfg_attr(predicate, ..)`, `cfg!(predicate)`: the first item, whatever follows it.
    First,
}

impl Linter<'_, '_, '_> {
    /// The `cfg`s and `cfg_attr`s of `node` as rustc expands them: in order, the attributes a
    /// `cfg_attr` that holds gives in its place, up to the first `cfg` that does not hold.
    fn owner(&mut self, tree: &Tree, edition: Edition, node: &SyntaxNode) {
        let Some(owner) = ast::AnyHasAttrs::cast(node.clone()) else { return };
        let mut attrs = ast::attrs_including_inner(&owner).peekable();
        if attrs.peek().is_none() {
            return;
        }
        let mut at = None;
        let mut stack: SmallVec<[ast::Meta; 2]> = SmallVec::new();
        'attrs: for attr in attrs {
            let Some(meta) = attr.meta() else { continue };
            stack.push(meta);
            'metas: while let Some(mut meta) = stack.pop() {
                while let ast::Meta::UnsafeMeta(it) = &meta {
                    let Some(inner) = it.meta() else { continue 'metas };
                    meta = inner;
                }
                match meta {
                    ast::Meta::CfgAttrMeta(it) => {
                        let Some(predicate) = it.cfg_predicate() else { continue };
                        predicate_leaves(it.syntax(), Form::First, edition, &mut |leaf| {
                            self.leaf(tree.file, node, edition, leaf, &mut at)
                        });
                        if self.cfg.check(&CfgExpr::parse_from_ast(predicate)) != Some(false) {
                            let len = stack.len();
                            stack.extend(it.metas());
                            stack[len..].reverse();
                        }
                    }
                    ast::Meta::CfgMeta(it) => {
                        predicate_leaves(it.syntax(), Form::Cfg, edition, &mut |leaf| {
                            self.leaf(tree.file, node, edition, leaf, &mut at)
                        });
                        if it
                            .cfg_predicate()
                            .is_some_and(|it| self.cfg.check(&CfgExpr::parse_from_ast(it)) == Some(false))
                        {
                            break 'attrs;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The predicate of `cfg!`, the root of its expansion.
    fn cfg_macro(&mut self, tree: &Tree, edition: Edition, node: &SyntaxNode) {
        let mut at = None;
        predicate_leaves(node, Form::First, edition, &mut |leaf| self.leaf(tree.file, node, edition, leaf, &mut at));
    }

    /// The predicates of the `cfg_select!` call `call`, every branch's, up to where its
    /// branches no longer parse.
    fn cfg_select(&mut self, call: MacroCallId) {
        let db = self.cx.db;
        let node = call.loc(db).to_node(db);
        let Some(tt) = ast::MacroCall::cast(node.value.clone()).and_then(|it| it.token_tree()) else { return };
        let tokens = tokens_of(tt.syntax());
        let toks = toks_of(&tokens);
        let [_, inner @ .., _] = &toks[..] else { return };
        let edition = node.file_id.edition(db);
        let mut parser = Parser::new(inner, edition);
        let mut leaves = Vec::new();
        'branches: while !parser.at_end() {
            while parser.eat(POUND) {
                parser.eat(BANG);
                if !skip_group(&mut parser) {
                    break 'branches;
                }
            }
            if !parser.eat(UNDERSCORE) {
                let Some(item) = parser.item() else { break };
                if entry(&item, &mut leaves).is_err() {
                    break;
                }
            }
            if !parser.eat(FAT_ARROW) {
                break;
            }
            if parser.peek().is_some_and(|it| it.kind == L_CURLY) {
                if !skip_group(&mut parser) {
                    break;
                }
                parser.eat(COMMA);
            } else {
                let mut depth = 0usize;
                while let Some(tok) = parser.peek() {
                    parser.bump();
                    match tok.kind {
                        L_PAREN | L_BRACK | L_CURLY => depth += 1,
                        R_PAREN | R_BRACK | R_CURLY => depth = depth.saturating_sub(1),
                        COMMA if depth == 0 => break,
                        _ => {}
                    }
                }
            }
        }
        let mut at = None;
        for leaf in &leaves {
            self.leaf(node.file_id, &node.value, edition, leaf, &mut at);
        }
    }

    /// The lint of `leaf` of a predicate of `owner` in `file`, if `--check-cfg` does not
    /// expect it, at `at`, the node rustc buffers the lints of `owner`'s predicates at.
    fn leaf(
        &mut self,
        file: HirFileId,
        owner: &SyntaxNode,
        edition: Edition,
        leaf: &Leaf<'_>,
        at: &mut Option<InFile<SyntaxNodePtr>>,
    ) {
        let mut diag = match self.check.expected(&leaf.name) {
            Some(Expected::Some(set)) if !set.contains(leaf.value.as_ref().map(|it| &*it.0)) => {
                self.value_lint(file, edition, leaf, &set)
            }
            None if self.check.exhaustive_names() => self.name_lint(file, edition, leaf),
            _ => return,
        };
        let at = *at.get_or_insert_with(|| lint_at(self.cx, file, owner));
        source_fact(self.cx, &mut diag, at);
        self.out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at));
    }

    fn span(&self, file: HirFileId, range: TextRange) -> Span {
        self.cx.span(InFile::new(file, range))
    }

    /// The edition of the token at `range`, as rustc has it of the token's span.
    fn edition_at(&self, file: HirFileId, range: TextRange, edition: Edition) -> Edition {
        match file.macro_file() {
            None => edition,
            Some(call) => call.expansion_span_map(self.cx.db).span_at(range.start()).ctx.edition(self.cx.db),
        }
    }

    /// Of the token at `range`: whether rustc's `in_external_macro` holds of its span, and the
    /// kind and path of the other crate's macro that made it (`outer_expn_data`), if one did.
    fn macro_of(&self, file: HirFileId, range: TextRange) -> (bool, Option<(&'static str, String)>) {
        let db = self.cx.db;
        let Some(call) = file.macro_file() else { return (false, None) };
        let Some(outer) = call.expansion_span_map(db).span_at(range.start()).ctx.outer_expn(db) else {
            return (false, None);
        };
        let loc = outer.loc(db);
        let local = hir::Crate::from(loc.def.krate) == self.cx.krate;
        let external = match loc.kind {
            MacroCallKind::FnLike { .. } => !local,
            _ => true,
        };
        if local {
            return (external, None);
        }
        let named = match loc.kind {
            MacroCallKind::FnLike { .. } => ast::MacroCall::cast(loc.to_node(db).value)
                .and_then(|it| it.path())
                .map(|path| ("macro", path_text(path.syntax()))),
            _ => self.cx.expansion_of(outer).map(|it| (it.kind.descr(), it.name)),
        };
        (external, named)
    }

    /// rustc's `unexpected_cfg_name`.
    fn name_lint(&self, file: HirFileId, edition: Edition, leaf: &Leaf<'_>) -> Diag {
        let (cx, check) = (self.cx, self.check);
        let name = &*leaf.name;
        let value = leaf.value.as_ref().map(|it| &*it.0);
        let name_span = self.span(file, leaf.name_range);
        let value_span = leaf.value.as_ref().map(|it| self.span(file, it.1));
        let token_edition = self.edition_at(file, leaf.name_range, edition);
        let (external, macro_help) = self.macro_of(file, leaf.name_range);
        let mut diag = Diag::lint(
            "unexpected_cfgs",
            format!("unexpected `cfg` condition name: `{}`", ident(name, cx.edition)),
            self.span(file, leaf.range),
        );
        let names = check.names();
        let mut is_feature = name == "feature";

        if is_feature && check.from_cargo {
            diag.help("consider defining some features in `Cargo.toml`");
        } else if let Some(value_span) = &value_span
            && name == "version"
        {
            diag.multipart_suggestion(
                "there is a similar config predicate: `version(\"..\")`",
                vec![
                    (name_span.shrink_to_hi().with_hi(value_span.lo), "(".to_owned()),
                    (value_span.shrink_to_hi(), ")".to_owned()),
                ],
                Applicability::MachineApplicable,
            );
        } else if value.is_none()
            && !leaf.raw
            && let Some(boolean) = miscapitalized_boolean(name)
        {
            diag.suggestion_verbose(
                name_span,
                format!(
                    "you may have meant to use `{boolean}` (notice the capitalization). Doing so makes this predicate evaluate to `{boolean}` unconditionally"
                ),
                boolean.to_string(),
                Applicability::MachineApplicable,
            );
        } else if let Some(best) = find_best_match_for_name(&names, name, None) {
            is_feature |= best == "feature";
            let best_ident = ident(best, token_edition);
            let similar = |diag: &mut Diag, span: Span, message: &str, code: String| {
                diag.suggestion(span, message, code, Applicability::MaybeIncorrect);
            };
            match check.expected(best) {
                Some(Expected::Some(set)) => {
                    let values = set.values();
                    let expected = |diag: &mut Diag| {
                        if !values.is_empty() {
                            diag.help(format!(
                                "expected values for `{}` are: {}",
                                ident(best, cx.edition),
                                list(&values)
                            ));
                        }
                    };
                    match (value, &value_span) {
                        (Some(value), Some(_)) if set.contains(Some(value)) => similar(
                            &mut diag,
                            name_span,
                            "there is a config with a similar name and value",
                            best_ident.into_owned(),
                        ),
                        (Some(_), Some(value_span)) if set.none => similar(
                            &mut diag,
                            name_span.to(value_span),
                            "there is a config with a similar name and no value",
                            best_ident.into_owned(),
                        ),
                        (Some(_), Some(value_span)) => {
                            let code = match values.first() {
                                Some(first) => format!("{best_ident} = \"{first}\""),
                                None => best_ident.into_owned(),
                            };
                            similar(
                                &mut diag,
                                name_span.to(value_span),
                                "there is a config with a similar name and different values",
                                code,
                            );
                            expected(&mut diag);
                        }
                        _ => {
                            similar(&mut diag, name_span, "there is a config with a similar name", best_ident.into_owned());
                            expected(&mut diag);
                        }
                    }
                }
                _ => similar(&mut diag, name_span, "there is a config with a similar name", best_ident.into_owned()),
            }
        } else {
            if value.is_none() {
                let similar_values: SmallVec<[&str; 3]> = names
                    .iter()
                    .copied()
                    .filter(|it| matches!(check.expected(it), Some(Expected::Some(set)) if set.contains(Some(name))))
                    .take(4)
                    .collect();
                if similar_values.len() <= 3 {
                    for cfg_name in similar_values {
                        diag.suggestion(
                            name_span.clone(),
                            "found config with similar value",
                            format!("{cfg_name} = \"{name}\""),
                            Applicability::MaybeIncorrect,
                        );
                    }
                }
            }
            let total = names.len();
            let mut shown: Vec<&str> = match check.all_expected {
                true => names,
                false => names.into_iter().filter(|it| super::well_known(it).is_none()).collect(),
            };
            shown.sort_unstable();
            let listed = if check.all_expected { shown.len() } else { shown.len().min(MAX_LISTED) };
            shown.truncate(listed);
            if !shown.is_empty() {
                let shown: Vec<Cow<'_, str>> = shown.iter().map(|it| ident(it, token_edition)).collect();
                let more = match total - listed {
                    0 => String::new(),
                    n => format!(" and {n} more"),
                };
                diag.sub(Sublevel::Help, format!("expected names are: {}{more}", list(&shown)), MultiSpan::default(), true);
            }
        }

        let check_cfg_arg = |escape: bool| to_check_cfg_arg(&ident(name, token_edition), value, escape);
        if check.from_cargo {
            macro_note(&mut diag, &macro_help);
            if !is_feature && !external {
                self.cargo_help(&mut diag, &check_cfg_arg);
            }
            diag.note(SEE_CARGO);
        } else {
            macro_note(&mut diag, &macro_help);
            diag.help(format!("to expect this configuration use `--check-cfg={}`", check_cfg_arg(false)));
            diag.note(SEE_RUSTC);
        }
        diag
    }

    /// rustc's `unexpected_cfg_value`.
    fn value_lint(&self, file: HirFileId, edition: Edition, leaf: &Leaf<'_>, set: &Set<'_>) -> Diag {
        let (cx, check) = (self.cx, self.check);
        let name = &*leaf.name;
        let value = leaf.value.as_ref().map(|it| &*it.0);
        let name_span = self.span(file, leaf.name_range);
        let value_span = leaf.value.as_ref().map(|it| self.span(file, it.1));
        let token_edition = self.edition_at(file, leaf.name_range, edition);
        let (external, macro_help) = self.macro_of(file, leaf.name_range);
        let message = match value {
            Some(value) => format!("unexpected `cfg` condition value: `{value}`"),
            None => "unexpected `cfg` condition value: (none)".to_owned(),
        };
        let mut diag = Diag::lint("unexpected_cfgs", message, self.span(file, leaf.range));
        let possibilities = set.values();
        let well_known = super::well_known(name).is_some();
        let renames: Vec<&str> = match value {
            Some(value) if well_known => {
                let mut renames: Vec<&str> = super::WELL_KNOWN
                    .iter()
                    .map(|it| it.0)
                    .filter(|it| matches!(check.expected(it), Some(Expected::Some(set)) if set.contains(Some(value))))
                    .collect();
                renames.sort_unstable();
                renames
            }
            _ => Vec::new(),
        };

        if let Some(value) = value
            && !renames.is_empty()
        {
            for rename in renames.iter().take(3) {
                diag.suggestion_verbose(
                    name_span.clone(),
                    format!("`{}` is an expected value for `{}`", ident(value, cx.edition), ident(rename, cx.edition)),
                    rename.to_string(),
                    Applicability::MaybeIncorrect,
                );
            }
        } else if !possibilities.is_empty() {
            let listed = if check.all_expected { possibilities.len() } else { possibilities.len().min(MAX_LISTED) };
            let more = match possibilities.len() - listed {
                0 => String::new(),
                n => format!(" and {n} more"),
            };
            diag.note(format!(
                "expected values for `{}` are: {}{}{more}",
                ident(name, cx.edition),
                if set.none { "(none), " } else { "" },
                list(&possibilities[..listed]),
            ));
            if let (Some(value), Some(value_span)) = (value, &value_span) {
                if let Some(best) = find_best_match_for_name(&possibilities, value, None) {
                    diag.suggestion(
                        value_span.clone(),
                        "there is a expected value with a similar name",
                        format!("\"{best}\""),
                        Applicability::MaybeIncorrect,
                    );
                }
            } else if let [first] = &possibilities[..] {
                diag.suggestion(
                    name_span.shrink_to_hi(),
                    "specify a config value",
                    format!(" = \"{first}\""),
                    Applicability::MaybeIncorrect,
                );
            }
        } else if set.none {
            diag.note(format!("no expected value for `{}`", ident(name, cx.edition)));
            if let Some(value_span) = &value_span {
                diag.suggestion(
                    name_span.shrink_to_hi().to(value_span),
                    "remove the value",
                    "",
                    Applicability::MaybeIncorrect,
                );
            }
        } else {
            diag.note(format!("no expected values for `{}`", ident(name, cx.edition)));
            let span = match &value_span {
                Some(value_span) => name_span.to(value_span),
                None => name_span.clone(),
            };
            diag.suggestion(span, "remove the condition", "", Applicability::MaybeIncorrect);
        }

        let check_cfg_arg = |escape: bool| to_check_cfg_arg(&ident(name, token_edition), value, escape);
        if check.from_cargo {
            if name == "feature" && !external {
                match value {
                    Some(value) => diag.help(format!(
                        "consider adding `{}` as a feature in `Cargo.toml`",
                        ident(value, cx.edition)
                    )),
                    None => diag.help("consider defining some features in `Cargo.toml`"),
                };
            } else if !well_known && !external {
                self.cargo_help(&mut diag, &check_cfg_arg);
            }
            macro_note(&mut diag, &macro_help);
            diag.note(SEE_CARGO);
        } else {
            if !well_known {
                diag.help(format!("to expect this configuration use `--check-cfg={}`", check_cfg_arg(false)));
            }
            macro_note(&mut diag, &macro_help);
            diag.note(SEE_RUSTC);
        }
        diag
    }

    /// rustc's `cargo_help_sub`: no `build.rs` way for a build script itself.
    fn cargo_help(&self, diag: &mut Diag, check_cfg_arg: &dyn Fn(bool) -> String) {
        diag.help("consider using a Cargo feature instead");
        diag.help(format!(
            "or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:\n [lints.rust]\n unexpected_cfgs = {{ level = \"warn\", check-cfg = ['{}'] }}",
            check_cfg_arg(false)
        ));
        let build_script = self
            .cx
            .krate
            .display_name(self.cx.db)
            .is_some_and(|it| it.crate_name().as_str() == "build_script_build");
        if !build_script {
            diag.help(format!(
                "or consider adding `println!(\"cargo::rustc-check-cfg={}\");` to the top of the `build.rs`",
                check_cfg_arg(true)
            ));
        }
    }
}

/// `UnexpectedCfgRustcMacroHelp` and `UnexpectedCfgCargoMacroHelp`.
fn macro_note(diag: &mut Diag, macro_help: &Option<(&'static str, String)>) {
    if let Some((kind, name)) = macro_help {
        diag.note(format!(
            "using a cfg inside a {kind} will use the cfgs from the destination crate and not the ones from the defining crate"
        ));
        diag.help(format!("try referring to `{name}` crate for guidance on how handle this unexpected cfg"));
    }
}

fn miscapitalized_boolean(name: &str) -> Option<bool> {
    if name.eq_ignore_ascii_case("false") {
        Some(false)
    } else if name.eq_ignore_ascii_case("true") {
        Some(true)
    } else {
        None
    }
}

/// rustc's `to_check_cfg_arg`: with `escape`, its quotes escaped for `println!` in a shell.
fn to_check_cfg_arg(name: &str, value: Option<&str>, escape: bool) -> String {
    let Some(value) = value else { return format!("cfg({name})") };
    let value = value.escape_debug().to_string();
    let values = match escape {
        true => format!("\\\"{}\\\"", value.replace('"', "\\\\\\\\\"")),
        false => format!("\"{value}\""),
    };
    format!("cfg({name}, values({values}))")
}

/// Each leaf of the predicate of the `cfg` attribute, `cfg_attr` attribute or `cfg!`
/// expansion `node`, as rustc parses it.
fn predicate_leaves(node: &SyntaxNode, form: Form, edition: Edition, each: &mut dyn FnMut(&Leaf<'_>)) {
    let tokens = tokens_of(node);
    let toks = toks_of(&tokens);
    let Some(open) = toks.iter().position(|it| it.kind == L_PAREN) else { return };
    let mut leaves = Vec::new();
    match form {
        Form::Cfg => {
            if open != 1 || toks.last().is_none_or(|it| it.kind != R_PAREN) {
                return;
            }
            let mut parser = Parser::new(&toks[open + 1..toks.len() - 1], edition);
            if let Some(items) = parser.items_to_end()
                && let [item] = &items[..]
            {
                let _ = entry(item, &mut leaves);
            }
        }
        Form::First => {
            if let Some(item) = Parser::new(&toks[open + 1..], edition).item() {
                let _ = entry(&item, &mut leaves);
            }
        }
    }
    for leaf in &leaves {
        each(leaf);
    }
}

/// rustc's `parse_cfg_entry`: the leaves of `item` up to where it stops, and whether it parsed.
fn entry<'t>(item: &Item<'t>, leaves: &mut Vec<Leaf<'t>>) -> Result<(), ()> {
    let meta = match item {
        Item::Lit(lit) => return if lit.kind == LitKind::Bool { Ok(()) } else { Err(()) },
        Item::Meta(meta) => meta,
    };
    match &meta.kind {
        Kind::List(list) => match meta.word().filter(|it| !it.keyword).map(|it| it.name) {
            Some("not") => match &list[..] {
                [single] => entry(single, leaves),
                _ => Err(()),
            },
            Some("any" | "all") => {
                for sub in list {
                    let _ = entry(sub, leaves);
                }
                Ok(())
            }
            Some("target") => {
                for sub in list {
                    let Item::Meta(sub) = sub else { continue };
                    let (Kind::NameValue(lit), Some(key)) = (&sub.kind, sub.word()) else { continue };
                    if key.keyword {
                        return Err(());
                    }
                    let name = Cow::Owned(format!("target_{}", key.name));
                    let _ = name_value(name, false, sub.range, Some(lit), sub.range, leaves);
                }
                Ok(())
            }
            Some("version") => Ok(()),
            _ => Err(()),
        },
        Kind::Word | Kind::NameValue(_) => {
            let Some(name) = meta.word().filter(|it| !it.keyword) else { return Err(()) };
            let value = match &meta.kind {
                Kind::NameValue(lit) => Some(lit),
                _ => None,
            };
            name_value(Cow::Borrowed(name.name), name.raw, meta.path_range, value, meta.range, leaves)
        }
    }
}

/// rustc's `parse_name_value`: the leaf, when its value is a string.
fn name_value<'t>(
    name: Cow<'t, str>,
    raw: bool,
    name_range: TextRange,
    value: Option<&meta::Lit<'t>>,
    range: TextRange,
    leaves: &mut Vec<Leaf<'t>>,
) -> Result<(), ()> {
    let value = match value {
        None => None,
        Some(lit) => Some((lit.str_value().ok_or(())?, lit.range)),
    };
    leaves.push(Leaf { name, raw, name_range, value, range });
    Ok(())
}

/// Skips the delimited group at the parser, which must start with an open delimiter.
fn skip_group(parser: &mut Parser<'_, '_>) -> bool {
    if !parser.peek().is_some_and(|it| matches!(it.kind, L_PAREN | L_BRACK | L_CURLY)) {
        return false;
    }
    let mut depth = 0usize;
    while let Some(tok) = parser.peek() {
        parser.bump();
        match tok.kind {
            L_PAREN | L_BRACK | L_CURLY => depth += 1,
            R_PAREN | R_BRACK | R_CURLY => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn tokens_of(node: &SyntaxNode) -> Vec<syntax::SyntaxToken> {
    node.descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| !it.kind().is_trivia())
        .collect()
}

fn toks_of(tokens: &[syntax::SyntaxToken]) -> Vec<Tok<'_>> {
    meta::glue(tokens.iter().map(|it| Tok { kind: it.kind(), text: it.text(), range: it.text_range() }).collect())
}

/// A path as rustc prints it: its tokens without what is between them.
fn path_text(path: &SyntaxNode) -> String {
    tokens_of(path).iter().map(|it| it.text()).collect()
}

/// The node rustc buffers a lint of `node`'s predicates at (`lint_node_id`): rustc evaluates
/// a node's `cfg`s before the node has an id, so the nearest node around it that has one,
/// through the `mod` item a file is of and the call an expansion is of.
fn lint_at(cx: &Cx<'_, '_>, file: HirFileId, node: &SyntaxNode) -> InFile<SyntaxNodePtr> {
    let db = cx.db;
    let (mut file, mut node) = (file, node.clone());
    loop {
        let mut top = node.clone();
        for ancestor in node.ancestors().skip(1) {
            if has_id(&ancestor) {
                return InFile::new(file, SyntaxNodePtr::new(&ancestor));
            }
            top = ancestor;
        }
        match file.macro_file() {
            Some(call) => {
                let called = call.loc(db).to_node(db);
                file = called.file_id;
                node = match called.value.kind() {
                    ATTR => called.value.parent().unwrap_or(called.value),
                    _ => called.value,
                };
            }
            None => {
                let declared = cx
                    .sema
                    .hir_file_to_module_def(file)
                    .filter(|it| !it.is_crate_root(db))
                    .and_then(|it| it.declaration_source(db));
                let Some(declared) = declared else { return InFile::new(file, SyntaxNodePtr::new(&top)) };
                if top != node {
                    return declared.map(|it| SyntaxNodePtr::new(it.syntax()));
                }
                file = declared.file_id;
                node = declared.value.syntax().clone();
            }
        }
    }
}

/// Whether rustc gives the node a `NodeId` it buffers lints at as it expands what is in it
/// (`assign_id!`).
fn has_id(node: &SyntaxNode) -> bool {
    let kind = node.kind();
    match kind {
        MACRO_CALL | MACRO_EXPR | MACRO_PAT | MACRO_TYPE => false,
        VARIANT | WHERE_PRED | RECORD_FIELD | TUPLE_FIELD | RECORD_PAT_FIELD | RECORD_EXPR_FIELD | PARAM
        | SELF_PARAM | TYPE_PARAM | LIFETIME_PARAM | CONST_PARAM | MATCH_ARM | LET_STMT => true,
        BLOCK_EXPR => !is_body(node),
        _ => is_item_expr_pat_or_type(kind),
    }
}

fn is_item_expr_pat_or_type(kind: SyntaxKind) -> bool {
    ast::Item::can_cast(kind) || ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind) || ast::Type::can_cast(kind)
}

/// Whether the block is a function's or loop's body or an `if`'s branch taken when it holds:
/// a `Block` in rustc's AST, not an expression.
fn is_body(block: &SyntaxNode) -> bool {
    let Some(parent) = block.parent() else { return false };
    let is = |it: Option<ast::BlockExpr>| it.is_some_and(|it| it.syntax() == block);
    if let Some(it) = ast::Fn::cast(parent.clone()) {
        return is(it.body());
    }
    if let Some(it) = ast::WhileExpr::cast(parent.clone()) {
        return is(it.loop_body());
    }
    if let Some(it) = ast::ForExpr::cast(parent.clone()) {
        return is(it.loop_body());
    }
    if let Some(it) = ast::LoopExpr::cast(parent.clone()) {
        return is(it.loop_body());
    }
    if let Some(it) = ast::IfExpr::cast(parent) {
        return is(it.then_branch());
    }
    false
}
