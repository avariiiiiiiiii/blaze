//! rustc's `LintLevelsBuilder`: the level of each lint at each node, from the command line
//! down through the attributes of what is around the node, as the early lint pass has it
//! (`TopDown`: a `forbid` holds, and what overrules one is an error) or as the HIR has it
//! (`shallow_lint_levels_on`: the innermost attribute wins), with what the early one says of
//! the names in them and the `#[expect]`s the HIR one finds.
//!
//! Follows rustc's `compiler/rustc_lint/src/levels.rs` (`add_command_line`, `insert_spec`,
//! `add`, `check_gated_lint`), `compiler/rustc_middle/src/lint.rs` (`reveal_actual_level_spec`,
//! `LintExpectation`), `compiler/rustc_lint/src/lints.rs` and `compiler/rustc_lint/src/
//! errors.rs` (the texts) and `compiler/rustc_session/src/parse.rs`
//! (`add_feature_diagnostics_for_issue`) (MIT or Apache-2.0).

use std::{hash::BuildHasher, ops::Range};

use hashbrown::HashTable;
use hir::{HirFileId, InFile};
use intern::Symbol;
use rustc_hash::FxBuildHasher;
use syntax::{SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, ast};

use super::{
    ExpectId, Shared, Source, SourceId, Spec,
    attrs::{self, ItemKind, Read},
};
use crate::{
    lints::{
        registry::{self, Checked, Level, Lint, LintId},
        syntax::Walk,
    },
    lower::mutable_ref_binding::BUILT_ON,
    model::{Applicability, Code, Diag, MultiSpan, Span},
};

/// Where rustc's walk of the crate comes to a node: [`attrs::place`].
type Place = Vec<(u32, u32)>;

/// A diagnostic of lint levels, and when rustc gives it.
pub(super) struct Emitted {
    /// Whether rustc's pre-expansion lint pass gives it, which runs on the crate's root file
    /// as parsed: the command line's and the malformed attributes of that file. The rest come
    /// as the early lint pass comes to them.
    pub(super) pre: bool,
    /// Where the walk of the crate comes to the node; nothing for the command line's.
    pub(super) place: Place,
    pub(super) diag: Diag,
}

/// An `#[expect]` of one lint, a group or a tool's lint, as the HIR has it.
pub(super) struct Expectation {
    pub(super) id: ExpectId,
    /// Where the name is in the attribute.
    pub(super) key: Span,
    pub(super) reason: Option<Box<str>>,
    /// `#[expect(unfulfilled_lint_expectations)]`, which nothing fulfills.
    pub(super) is_unfulfilled: bool,
    /// The node the attribute is on: where the level of `unfulfilled_lint_expectations` is.
    pub(super) node: InFile<SyntaxNode>,
}

/// A set of levels: the command line's, or one node's attributes'.
type SetId = u32;

const COMMAND_LINE: SetId = 0;
const NO_SET: SetId = SetId::MAX;

/// The levels of a crate as one of rustc's builders has them.
///
/// The sets are CSR columns: set `s` gives the lints `ids[start[s]..start[s + 1]]` (to the
/// end for the last) the levels at the same indices of `specs`, inside set `parent[s]`. That
/// is 8 bytes a set and 14 a lint in one, against rustc's `FxIndexMap` per set; finding a
/// lint's level scans the 2-byte ids of each set from the node out, which are few (a set is
/// made only for a node with lint attributes) and short.
pub(super) struct Levels {
    /// rustc's `lint_added_lints`: the early lint pass's builder, which says what is wrong with
    /// the attributes and holds to a `forbid`; the HIR's does neither and has no lint attributes
    /// on generic parameters.
    early: bool,
    /// Whether the walk of the crate is on: what is said of the attributes is said then, as
    /// rustc's walk comes to them, and once.
    walking: bool,
    start: Vec<u32>,
    parent: Vec<SetId>,
    ids: Vec<LintId>,
    specs: Vec<Spec>,
    /// The set in force at each node seen, by one FxHash of the node: 28 bytes a node.
    at: HashTable<(InFile<SyntaxNodePtr>, SetId)>,
    /// The nodes between one asked of and the nearest seen, kept for its capacity.
    chain: Vec<(u64, InFile<SyntaxNodePtr>, InFile<SyntaxNode>)>,
    pub(super) emitted: Vec<Emitted>,
    pub(super) expectations: Vec<Expectation>,
}

const _: () = assert!(size_of::<(InFile<SyntaxNodePtr>, SetId)>() <= 28);

impl Levels {
    /// The builder with the command line's levels (rustc's `add_command_line`), and what the
    /// early one says of the names on it.
    pub(super) fn new(sh: &mut Shared<'_, '_, '_>, early: bool) -> Levels {
        let mut levels = Levels {
            early,
            walking: true,
            start: vec![0],
            parent: vec![NO_SET],
            ids: Vec::with_capacity(sh.cmd.opts.len()),
            specs: Vec::with_capacity(sh.cmd.opts.len()),
            at: HashTable::new(),
            chain: Vec::new(),
            emitted: Vec::new(),
            expectations: Vec::new(),
        };
        let cmd = sh.cmd;
        for (name, level) in &cmd.opts {
            levels.command_line(sh, name, *level);
        }
        levels
    }

    fn command_line(&mut self, sh: &mut Shared<'_, '_, '_>, name: &str, level: Level) {
        let (tool, only) = match name.split_once("::") {
            Some((tool, only)) => (Some(tool), only),
            None => (None, name),
        };
        if self.early {
            let requested = format!("requested on the command line with `{} {name}`", level.to_cmd_flag());
            if only == "warnings" && level == Level::ForceWarn {
                let mut diag = Diag::new(
                    crate::model::Level::Error,
                    Some(Code::Error("E0602".to_owned())),
                    "`warnings` lint group is not supported with ´--force-warn´",
                );
                diag.span = MultiSpan::default();
                self.push(true, Vec::new(), diag);
            }
            let lint = |name: &str, message: String| {
                Diag::new(crate::model::Level::Warning, Some(Code::Lint(name.to_owned())), message)
            };
            match sh.store.check(only, tool) {
                Checked::Renamed(new) => {
                    let mut diag = lint("renamed_and_removed_lints", format!("lint `{name}` has been renamed to `{new}`"));
                    diag.help(format!("use the new name `{new}`"));
                    diag.note(requested);
                    self.lint(sh, COMMAND_LINE, true, Vec::new(), diag, None);
                }
                Checked::Removed(reason) => {
                    let mut diag = lint("renamed_and_removed_lints", format!("lint `{name}` has been removed: {reason}"));
                    diag.note(requested);
                    self.lint(sh, COMMAND_LINE, true, Vec::new(), diag, None);
                }
                Checked::NoLint(suggestion) => {
                    let mut diag = lint("unknown_lints", format!("unknown lint: `{name}`"));
                    if let Some((replace, from_rustc)) = suggestion {
                        diag.help(match from_rustc {
                            true => format!("a lint with a similar name exists in `rustc` lints: `{replace}`"),
                            false => format!("did you mean: `{replace}`"),
                        });
                    }
                    diag.note(requested);
                    self.lint(sh, COMMAND_LINE, true, Vec::new(), diag, Some("E0602"));
                }
                Checked::Tool(_, Some(new)) => {
                    let mut diag = lint(
                        "renamed_and_removed_lints",
                        format!("lint name `{name}` is deprecated and may not have an effect in the future"),
                    );
                    diag.help(format!("change it to {new}"));
                    diag.note(requested);
                    self.lint(sh, COMMAND_LINE, true, Vec::new(), diag, None);
                }
                Checked::NoTool => {
                    let tool = tool.unwrap_or_default();
                    let mut diag = Diag::new(
                        crate::model::Level::Error,
                        Some(Code::Error("E0602".to_owned())),
                        format!("unknown lint tool: `{tool}`"),
                    );
                    diag.note(requested);
                    self.push(true, Vec::new(), diag);
                }
                Checked::Ok(_) | Checked::Tool(_, None) | Checked::MissingTool => {}
            }
        }
        let Some(lints) = sh.store.find_lints(name) else { return };
        let source = sh.source(Source::CommandLine { name: name.into(), level });
        for lint in lints {
            let id = LintId::of(lint);
            if self.own(COMMAND_LINE, id).is_some_and(|it| matches!(it.level, Level::ForceWarn | Level::Forbid)) {
                continue;
            }
            if self.gated(sh, COMMAND_LINE, lint, None, &mut Vec::new) {
                self.insert(COMMAND_LINE, id, Spec { level, expect: None, source });
            }
        }
    }

    /// Every node of the crate with attributes, as rustc's walk comes to it. One without any
    /// costs a check of its kind and of its first child.
    pub(super) fn walk(&mut self, sh: &mut Shared<'_, '_, '_>, walk: &Walk<'_>) {
        self.walking = true;
        for tree in &walk.trees {
            for node in &tree.nodes {
                if attrs::may_own(node, self.early) {
                    self.set_at(sh, InFile::new(tree.file, node.clone()));
                }
            }
        }
        self.walking = false;
    }

    /// [`Levels::set_at`] of the node `at` points to, with no tree walked when a node at it was
    /// seen.
    pub(super) fn set_of(&mut self, sh: &mut Shared<'_, '_, '_>, at: InFile<SyntaxNodePtr>) -> SetId {
        let hash = FxBuildHasher.hash_one(&at);
        if let Some(&(_, set)) = self.at.find(hash, |(it, _)| *it == at) {
            return set;
        }
        let node = sh.cx.syntax(at);
        self.set_at(sh, node)
    }

    /// The set in force at `node`: of its attributes, else of the nearest node around it with
    /// any. Each node between it and the nearest one seen is hashed once.
    pub(super) fn set_at(&mut self, sh: &mut Shared<'_, '_, '_>, node: InFile<SyntaxNode>) -> SetId {
        let mut chain = std::mem::take(&mut self.chain);
        let mut next = Some(node);
        let mut set = COMMAND_LINE;
        while let Some(node) = next {
            let key = attrs::key(&node);
            let hash = FxBuildHasher.hash_one(&key);
            if let Some(&(_, known)) = self.at.find(hash, |(it, _)| *it == key) {
                set = known;
                break;
            }
            next = attrs::parent(sh.cx.sema, &node);
            chain.push((hash, key, node));
        }
        for (hash, key, node) in chain.drain(..).rev() {
            set = self.push_node(sh, set, &node);
            self.at.insert_unique(hash, (key, set), |(it, _)| FxBuildHasher.hash_one(it));
        }
        self.chain = chain;
        set
    }

    fn push_node(&mut self, sh: &mut Shared<'_, '_, '_>, parent: SetId, node: &InFile<SyntaxNode>) -> SetId {
        let Some(owner) = attrs::owner(&node.value, self.early) else { return parent };
        let reads = attrs::read(&owner, sh.cfg, sh.cx.edition);
        if reads.is_empty() {
            return parent;
        }
        let is_crate = node.value.kind() == SyntaxKind::SOURCE_FILE && node.file_id == sh.root;
        let set = SetId::try_from(self.start.len()).expect("fewer than 2^32 sets");
        self.start.push(u32::try_from(self.ids.len()).expect("fewer than 2^32 levels"));
        self.parent.push(parent);
        self.add(sh, set, node, reads, is_crate);
        if self.range(set).is_empty() {
            self.start.pop();
            self.parent.pop();
            return parent;
        }
        set
    }

    /// rustc's `add`: the attributes of one node.
    fn add(&mut self, sh: &mut Shared<'_, '_, '_>, set: SetId, node: &InFile<SyntaxNode>, reads: Vec<Read>, is_crate: bool) {
        let says = self.early && self.walking;
        let pre = node.file_id == sh.root;
        let cx = sh.cx;
        let file = node.file_id;
        let span = |range: TextRange| cx.span(InFile::new(file, range));
        let mut known: Option<Place> = None;
        let mut place = || known.get_or_insert_with(|| attrs::place(cx.sema, node)).clone();
        for read in reads {
            let (level, mut items) = match read {
                Read::Allows(name) => {
                    let lint = registry::lint(name).unwrap_or_else(|| panic!("no lint `{name}`"));
                    let spec = Spec { level: Level::Allow, expect: None, source: SourceId::DEFAULT };
                    self.insert(set, LintId::of(lint), spec);
                    continue;
                }
                Read::Level(level, items) => (level, items),
            };
            let tail = match items.last() {
                None => continue,
                Some(it) => match &it.kind {
                    ItemKind::Word(_) | ItemKind::Lit => Tail::Lint,
                    ItemKind::NameValue { string, value, .. } if it.kind.is_reason() => {
                        Tail::Reason(string.clone(), *value)
                    }
                    ItemKind::NameValue { .. } | ItemKind::List(_) => Tail::Bad(it.range),
                },
            };
            let mut reason: Option<Box<str>> = None;
            match tail {
                Tail::Lint => {}
                Tail::Reason(string, value) => {
                    match string {
                        Some(string) => reason = Some(string.into_boxed_str()),
                        None if says => {
                            let diag = malformed(span(value), "reason must be a string literal");
                            self.push(pre, place(), diag);
                        }
                        None => {}
                    }
                    items.pop();
                }
                Tail::Bad(range) => {
                    if says {
                        self.push(pre, place(), malformed(span(range), "bad attribute argument"));
                    }
                }
            }
            for item in &items {
                let ItemKind::Word(path) = &item.kind else {
                    if says {
                        let label = match item.kind.is_reason() {
                            true => "reason in lint attribute must come last",
                            false => "bad attribute argument",
                        };
                        self.push(pre, place(), malformed(span(item.range), label));
                    }
                    continue;
                };
                let at = span(item.range);
                let expect = (level == Level::Expect).then(|| sh.expect_id(file, item.range));
                let (tool, name) = match &path[..] {
                    [tool, rest @ ..] if !rest.is_empty() => (Some(tool), attrs::path_string(rest)),
                    _ => (None, attrs::path_string(path)),
                };
                let full = match tool {
                    Some(tool) => format!("{}::{name}", tool.name()),
                    None => name.clone().into_owned(),
                };
                let lint_diag = |lint: &str, message: String| Diag::lint(lint, message, at.clone());
                let (lints, named) = match sh.store.check(&name, tool.map(|it| it.name())) {
                    Checked::Ok(lints) => (lints, path.last().map_or("", |it| it.name()).to_owned()),
                    Checked::Tool(lints, None) => (lints, full),
                    Checked::Tool(lints, Some(new)) => {
                        if says {
                            let mut diag = lint_diag(
                                "renamed_and_removed_lints",
                                format!("lint name `{name}` is deprecated and may not have an effect in the future"),
                            );
                            diag.suggestion(at.clone(), "change it to", new.clone(), Applicability::MachineApplicable);
                            self.lint(sh, set, false, place(), diag, None);
                        }
                        (lints, new)
                    }
                    Checked::MissingTool => continue,
                    Checked::NoTool => {
                        if says {
                            let tool = tool.expect("a tool's lint");
                            let mut diag = Diag::error(
                                Some("E0710"),
                                format!("unknown tool name `{0}` found in scoped lint: `{0}::{name}`", tool.name()),
                                span(tool.range),
                            );
                            diag.help(format!("add `#![register_tool({})]` to the crate root", tool.name()));
                            self.push(pre, place(), diag);
                        }
                        continue;
                    }
                    Checked::Renamed(new) => {
                        if says {
                            let mut diag = lint_diag(
                                "renamed_and_removed_lints",
                                format!("lint `{full}` has been renamed to `{new}`"),
                            );
                            diag.suggestion(at.clone(), "use the new name", new, Applicability::MachineApplicable);
                            self.lint(sh, set, false, place(), diag, None);
                        }
                        match sh.store.check(new, None) {
                            Checked::Ok(lints) => (lints, new.to_owned()),
                            other => panic!("`{name}` is renamed to `{new}`, which is {other:?}"),
                        }
                    }
                    Checked::Removed(why) => {
                        if says {
                            let diag = lint_diag("renamed_and_removed_lints", format!("lint `{full}` has been removed: {why}"));
                            self.lint(sh, set, false, place(), diag, None);
                        }
                        continue;
                    }
                    Checked::NoLint(suggestion) => {
                        if says {
                            let mut diag = lint_diag("unknown_lints", format!("unknown lint: `{full}`"));
                            if let Some((replace, from_rustc)) = suggestion {
                                let message = match from_rustc {
                                    true => "a lint with a similar name exists in `rustc` lints",
                                    false => "did you mean",
                                };
                                diag.suggestion(at.clone(), message, replace, Applicability::MaybeIncorrect);
                            }
                            self.lint(sh, set, false, place(), diag, None);
                        }
                        continue;
                    }
                };
                let source =
                    sh.source(Source::Node { name: named.into_boxed_str(), span: at.clone(), reason: reason.clone() });
                for &lint in &lints {
                    if self.gated(sh, set, lint, Some(&at), &mut place) {
                        self.insert_spec(sh, set, lint, Spec { level, expect, source }, &mut place);
                    }
                }
                if let Some(id) = expect
                    && !self.early
                    && self.walking
                {
                    let is_unfulfilled = matches!(&lints[..], [only] if only.name == "unfulfilled_lint_expectations");
                    let reason = reason.clone();
                    self.expectations.push(Expectation { id, key: at, reason, is_unfulfilled, node: node.clone() });
                }
            }
        }
        if says && !is_crate {
            let range = self.range(set);
            let ignored = self.ids[range.clone()].iter().zip(&self.specs[range]).find_map(|(id, spec)| {
                match &sh.sources[spec.source.0 as usize] {
                    Source::Node { name, span, .. } if id.lint().crate_level_only => {
                        Some((spec.level, name.clone(), span.clone()))
                    }
                    _ => None,
                }
            });
            if let Some((level, name, at)) = ignored {
                let message = format!("{}({name}) is ignored unless specified at crate level", level.as_str());
                self.lint(sh, set, false, place(), Diag::lint("unused_attributes", message, at), None);
            }
        }
    }

    /// rustc's `insert_spec`: `spec` of an attribute for `lint`, unless a `forbid` before it
    /// holds.
    fn insert_spec(
        &mut self,
        sh: &mut Shared<'_, '_, '_>,
        set: SetId,
        lint: &'static Lint,
        spec: Spec,
        place: &mut impl FnMut() -> Place,
    ) {
        let old = self.spec(sh, lint, set);
        if self.early && spec.level == Level::Deny && old.level == Level::Forbid {
            return;
        }
        if self.early && spec.level != Level::Forbid && old.level == Level::Forbid {
            let groups = match &sh.sources[old.source.0 as usize] {
                Source::Default => false,
                Source::Node { name, .. } | Source::CommandLine { name, .. } => sh.store.is_lint_group(name),
            };
            if self.walking
                && let Source::Node { name, span, .. } = &sh.sources[spec.source.0 as usize]
            {
                let message = format!("{}({name}) incompatible with previous forbid", spec.level.as_str());
                let mut diag = match groups {
                    true => Diag::lint("forbidden_lint_groups", message, span.clone()),
                    false => Diag::error(Some("E0453"), message, span.clone()),
                };
                diag.span_label(span.clone(), "overruled by previous forbid");
                match &sh.sources[old.source.0 as usize] {
                    Source::Default => {
                        diag.note(format!("`forbid` lint level is the default for {}", lint.name));
                    }
                    Source::Node { span, reason, .. } => {
                        diag.span_label(span.clone(), "`forbid` level set here");
                        if let Some(reason) = reason {
                            diag.note(&**reason);
                        }
                    }
                    Source::CommandLine { name, .. } => {
                        diag.note(format!("`forbid` lint level was set on command line (`-F {name}`)"));
                    }
                }
                match groups {
                    true => self.lint(sh, set, false, place(), diag, None),
                    false => self.push(false, place(), diag),
                }
            }
            if !groups {
                return;
            }
        }
        if spec.level == Level::Expect && lint.name == "unfulfilled_lint_expectations" {
            return;
        }
        let spec = match (old.level, spec.level) {
            (Level::ForceWarn, Level::Expect) => Spec { level: Level::ForceWarn, expect: spec.expect, source: old.source },
            (Level::ForceWarn, _) => Spec { level: Level::ForceWarn, expect: None, source: old.source },
            _ => spec,
        };
        self.insert(set, LintId::of(lint), spec);
    }

    /// rustc's `check_gated_lint`: whether `lint` is not behind a feature the crate lacks; the
    /// early builder says so of one that is.
    fn gated(
        &mut self,
        sh: &mut Shared<'_, '_, '_>,
        set: SetId,
        lint: &'static Lint,
        at: Option<&Span>,
        place: &mut impl FnMut() -> Place,
    ) -> bool {
        let Some(feature) = lint.feature_gate else { return true };
        if sh.features.is_enabled(&Symbol::intern(feature.name)) {
            return true;
        }
        if self.early && self.walking {
            let mut diag = Diag::new(
                crate::model::Level::Warning,
                Some(Code::Lint("unknown_lints".to_owned())),
                format!("unknown lint: `{}`", lint.name),
            );
            if let Some(at) = at {
                diag.span = MultiSpan::of(at.clone());
            }
            diag.note(format!("the `{}` lint is unstable", lint.name));
            if let Some(issue) = feature.issue {
                diag.note(format!("see issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}> for more information"));
            }
            diag.help(match at {
                None => format!("add `-Zcrate-attr=\"feature({})\"` to the command-line options to enable", feature.name),
                Some(_) => format!("add `#![feature({})]` to the crate attributes to enable", feature.name),
            });
            diag.note(format!("this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"));
            self.lint(sh, set, false, place(), diag, None);
        }
        false
    }

    /// `diag` of a lint at its level in `set` as it is now (rustc's `emit_span_lint` from the
    /// builder), with `code` for its code when it has one of its own.
    fn lint(&mut self, sh: &mut Shared<'_, '_, '_>, set: SetId, pre: bool, place: Place, diag: Diag, code: Option<&str>) {
        let name = diag.lint_name().expect("a lint's diagnostic");
        let lint = registry::lint(name).unwrap_or_else(|| panic!("no lint `{name}`"));
        let spec = self.spec(sh, lint, set);
        if let Some(mut diag) = sh.finish(lint, spec, diag) {
            if let Some(code) = code {
                diag.code = Some(Code::Error(code.to_owned()));
            }
            self.push(pre, place, diag);
        }
    }

    fn push(&mut self, pre: bool, place: Place, diag: Diag) {
        self.emitted.push(Emitted { pre, place, diag });
    }

    /// The level of `lint` in `set`: rustc's `reveal_actual_level_spec`, with `warnings` and
    /// `--cap-lints`.
    pub(super) fn spec(&self, sh: &Shared<'_, '_, '_>, lint: &'static Lint, set: SetId) -> Spec {
        let mut spec = self.raw(LintId::of(lint), set).unwrap_or(Spec {
            level: lint.default_level(sh.cx.edition),
            expect: None,
            source: SourceId::DEFAULT,
        });
        if spec.level == Level::Warn
            && let Some(warnings) = self.raw(sh.warnings, set)
        {
            let respect = match warnings.level {
                Level::Warn | Level::ForceWarn => false,
                Level::Deny | Level::Forbid => !lint.ignore_deny_warnings,
                Level::Allow | Level::Expect => true,
            };
            if respect {
                spec = warnings;
            }
        }
        // Only `--force-warn` gives `ForceWarn`, which keeps its source over an attribute's
        // level: a `ForceWarn` is a command-line one, which the cap leaves.
        if spec.level != Level::ForceWarn
            && let Some(cap) = sh.cmd.cap
        {
            spec.level = spec.level.min(cap);
        }
        spec
    }

    fn range(&self, set: SetId) -> Range<usize> {
        let set = set as usize;
        let end = self.start.get(set + 1).map_or(self.ids.len(), |&it| it as usize);
        self.start[set] as usize..end
    }

    fn raw(&self, id: LintId, set: SetId) -> Option<Spec> {
        let mut at = set;
        while at != NO_SET {
            if let Some(spec) = self.own(at, id) {
                return Some(spec);
            }
            at = self.parent[at as usize];
        }
        None
    }

    fn own(&self, set: SetId, id: LintId) -> Option<Spec> {
        let range = self.range(set);
        let start = range.start;
        self.ids[range].iter().position(|&it| it == id).map(|it| self.specs[start + it])
    }

    /// Sets `id` at `spec` in `set`, which is the last set.
    fn insert(&mut self, set: SetId, id: LintId, spec: Spec) {
        debug_assert_eq!(set as usize, self.start.len() - 1, "levels are set in the last set only");
        let range = self.range(set);
        let start = range.start;
        match self.ids[range].iter().position(|&it| it == id) {
            Some(at) => self.specs[start + at] = spec,
            None => {
                self.ids.push(id);
                self.specs.push(spec);
            }
        }
    }
}

/// The last element of a lint attribute's list, which may be its `reason`.
enum Tail {
    Lint,
    /// `reason = ..`: the string, if it is one, and where the value is.
    Reason(Option<String>, TextRange),
    /// A `name = value` or `name(..)` that is not a reason.
    Bad(TextRange),
}

/// E0452 "malformed lint attribute input" with its label.
fn malformed(at: Span, label: &str) -> Diag {
    let mut diag = Diag::error(Some("E0452"), "malformed lint attribute input", at.clone());
    diag.span_label(at, label);
    diag
}

/// Where the HIR has the `#[expect]`s of `node`, for the order rustc checks them in
/// (`ModuleItems::owners`): the crate, then free items, trait items, impl items and foreign
/// items, each in order; the owner is the item `node` is in.
pub(super) fn owner_rank(sh: &Shared<'_, '_, '_>, node: &InFile<SyntaxNode>) -> (u8, Place) {
    let sema = sh.cx.sema;
    let mut at = node.clone();
    loop {
        let kind = at.value.kind();
        if kind == SyntaxKind::SOURCE_FILE && at.file_id == sh.root {
            return (0, attrs::place(sema, &at));
        }
        let up = attrs::parent(sema, &at);
        if ast::Item::can_cast(kind) && kind != SyntaxKind::MACRO_CALL {
            let around = up.as_ref().map(|it| it.value.kind());
            let rank = match around {
                Some(SyntaxKind::ASSOC_ITEM_LIST) => {
                    match up.as_ref().and_then(|it| attrs::parent(sema, it)).map(|it| it.value.kind()) {
                        Some(SyntaxKind::TRAIT) => 2,
                        _ => 3,
                    }
                }
                Some(SyntaxKind::EXTERN_ITEM_LIST) => 4,
                _ => 1,
            };
            return (rank, attrs::place(sema, &at));
        }
        match up {
            Some(up) => at = up,
            None => return (0, attrs::place(sema, &at)),
        }
    }
}

/// The crate's root file, which is the root module's.
pub(super) fn root_file(cx: &crate::cx::Cx<'_, '_>) -> Option<HirFileId> {
    cx.krate.root_module(cx.db).as_source_file_id(cx.db).map(HirFileId::from)
}
