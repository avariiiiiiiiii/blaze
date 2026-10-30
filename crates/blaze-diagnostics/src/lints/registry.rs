//! Every lint rustc knows, as its `LintStore` holds them: the builtin lints in the order rustc
//! registers them, their groups (those `register_builtins` adds, and `future_incompatible` and
//! the edition groups rustc builds from the lints), the renamed, removed and ignored names, the
//! internal `rustc::` lints of `-Z unstable-options`, and rustc's check of a lint name with its
//! suggestion of a similar one.
//!
//! Follows rustc's `compiler/rustc_lint_defs/src/{lib,builtin}.rs` (`declare_lint!`,
//! `FutureIncompatibleInfo`), every `declare_lint!` of `compiler/rustc_lint/src/`,
//! `compiler/rustc_lint/src/lib.rs` (`register_builtins`, `register_internals`),
//! `compiler/rustc_lint/src/context.rs` (`LintStore::register_lints`, `check_lint_name`,
//! `no_lint_suggestion`, `check_tool_name_for_backwards_compat`) and
//! `compiler/rustc_span/src/edit_distance.rs`, of rust-lang/rust
//! ad3d0bc141a02cf446e384136d250a1f6950fed5 (MIT or Apache-2.0); the pinned nightly is
//! `nightly-2026-10-27`.

pub mod edit_distance;
mod lints;
mod names;

use std::sync::OnceLock;

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use span::Edition;

/// A lint level, in rustc's order: where one is capped, the lower wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    Allow,
    Expect,
    Warn,
    ForceWarn,
    Deny,
    Forbid,
}

impl Level {
    /// As an attribute and rustc's notes name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Allow => "allow",
            Level::Expect => "expect",
            Level::Warn => "warn",
            Level::ForceWarn => "force-warn",
            Level::Deny => "deny",
            Level::Forbid => "forbid",
        }
    }

    /// The level an attribute named `name` sets: rustc's `Level::from_symbol`.
    pub fn of_attr(name: &str) -> Option<Level> {
        Some(match name {
            "allow" => Level::Allow,
            "expect" => Level::Expect,
            "warn" => Level::Warn,
            "deny" => Level::Deny,
            "forbid" => Level::Forbid,
            _ => return None,
        })
    }

    /// The level `--cap-lints` names: rustc's `Level::from_str`, which has no `expect` nor
    /// `force-warn`.
    pub fn of_cap(name: &str) -> Option<Level> {
        Some(match name {
            "allow" => Level::Allow,
            "warn" => Level::Warn,
            "deny" => Level::Deny,
            "forbid" => Level::Forbid,
            _ => return None,
        })
    }

    /// The flag that sets it. `expect` has none; rustc never asks for it.
    pub fn to_cmd_flag(self) -> &'static str {
        match self {
            Level::Allow => "-A",
            Level::Expect | Level::Warn => "-W",
            Level::ForceWarn => "--force-warn",
            Level::Deny => "-D",
            Level::Forbid => "-F",
        }
    }

    pub fn is_error(self) -> bool {
        matches!(self, Level::Deny | Level::Forbid)
    }
}

/// Why a lint is future-incompatible: rustc's `FutureIncompatibilityReason`, with its issue or
/// the edition guide's page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// An error in a future release, in every edition.
    FutureReleaseError(u32),
    /// A change of meaning in a future release.
    FutureReleaseSemanticsChange(u32),
    /// As `FutureReleaseError`, said in the lint's own words.
    Custom(&'static str, u32),
    /// An error from the edition on.
    EditionError(Edition, &'static str),
    /// A change of meaning from the edition on.
    EditionSemanticsChange(Edition, &'static str),
    EditionAndFutureReleaseError(Edition, &'static str),
    EditionAndFutureReleaseSemanticsChange(Edition, &'static str),
}

impl Reason {
    /// The edition it is tied to, if any: such a lint is of that edition's compatibility group,
    /// any other of `future_incompatible`.
    pub fn edition(self) -> Option<Edition> {
        match self {
            Reason::EditionError(edition, _)
            | Reason::EditionSemanticsChange(edition, _)
            | Reason::EditionAndFutureReleaseError(edition, _)
            | Reason::EditionAndFutureReleaseSemanticsChange(edition, _) => Some(edition),
            Reason::FutureReleaseError(_)
            | Reason::FutureReleaseSemanticsChange(_)
            | Reason::Custom(..) => None,
        }
    }

    /// Where to read more: rustc's `FutureIncompatibilityReason::reference`.
    pub fn reference(self) -> String {
        match self {
            Reason::FutureReleaseError(issue)
            | Reason::FutureReleaseSemanticsChange(issue)
            | Reason::Custom(_, issue) => {
                format!("issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}>")
            }
            Reason::EditionError(edition, page)
            | Reason::EditionSemanticsChange(edition, page)
            | Reason::EditionAndFutureReleaseError(edition, page)
            | Reason::EditionAndFutureReleaseSemanticsChange(edition, page) => {
                format!(
                    "<https://doc.rust-lang.org/edition-guide/rust-{}/{page}.html>",
                    edition.number()
                )
            }
        }
    }

    /// What rustc warns of it, in a crate of edition `current`.
    pub fn explanation(self, current: Edition) -> String {
        match self {
            Reason::FutureReleaseError(_) => "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!".to_owned(),
            Reason::FutureReleaseSemanticsChange(_) => {
                "this will change its meaning in a future release!".to_owned()
            }
            Reason::EditionError(edition, _) => format!(
                "this is accepted in the current edition (Rust {}) but is a hard error in Rust {}!",
                current.number(),
                edition.number()
            ),
            Reason::EditionSemanticsChange(edition, _) => {
                format!("this changes meaning in Rust {}", edition.number())
            }
            Reason::EditionAndFutureReleaseError(edition, _) => format!(
                "this was previously accepted by the compiler but is being phased out; it will become a hard error in Rust {} and in a future release in all editions!",
                edition.number()
            ),
            Reason::EditionAndFutureReleaseSemanticsChange(edition, _) => format!(
                "this changes meaning in Rust {} and in a future release in all editions!",
                edition.number()
            ),
            Reason::Custom(text, _) => text.to_owned(),
        }
    }
}

/// rustc's `FutureIncompatibleInfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FutureIncompatible {
    pub reason: Reason,
    /// Whether rustc warns of the reason; off for a lint whose message says it already.
    pub explain_reason: bool,
    /// Whether it shows in cargo's future-incompatibility report of dependencies.
    pub report_in_deps: bool,
}

/// The feature a lint is unknown without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feature {
    pub name: &'static str,
    /// Its tracking issue, as `rustc_feature` has it.
    pub issue: Option<u32>,
}

/// A lint: rustc's `Lint`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lint {
    /// Lowercase, as attributes name it; a tool's with its tool: `rustc::bad_opt_access`.
    pub name: &'static str,
    pub default_level: Level,
    pub desc: &'static str,
    /// From this edition on, this level is its default.
    pub edition_level: Option<(Edition, Level)>,
    /// Whether it is given in the expansion of another crate's macro.
    pub report_in_external_macro: bool,
    pub future_incompatible: Option<FutureIncompatible>,
    pub feature_gate: Option<Feature>,
    /// Whether only the crate's own attributes set its level.
    pub crate_level_only: bool,
    /// Whether rustc runs it when it is allowed.
    pub eval_always: bool,
    /// Whether `-D warnings` leaves it a warning.
    pub ignore_deny_warnings: bool,
}

impl Lint {
    /// Its default level in a crate of edition `edition`: rustc's `Lint::default_level`.
    pub fn default_level(&self, edition: Edition) -> Level {
        match self.edition_level {
            Some((since, level)) if since <= edition => level,
            _ => self.default_level,
        }
    }

    const fn new(name: &'static str, default_level: Level, desc: &'static str) -> Lint {
        Lint {
            name,
            default_level,
            desc,
            edition_level: None,
            report_in_external_macro: false,
            future_incompatible: None,
            feature_gate: None,
            crate_level_only: false,
            eval_always: false,
            ignore_deny_warnings: false,
        }
    }

    const fn external(mut self) -> Lint {
        self.report_in_external_macro = true;
        self
    }

    const fn crate_level(mut self) -> Lint {
        self.crate_level_only = true;
        self
    }

    const fn eval_always(mut self) -> Lint {
        self.eval_always = true;
        self
    }

    const fn ignore_deny_warnings(mut self) -> Lint {
        self.ignore_deny_warnings = true;
        self
    }

    const fn edition(mut self, edition: Edition, level: Level) -> Lint {
        self.edition_level = Some((edition, level));
        self
    }

    const fn gate(mut self, name: &'static str, issue: Option<u32>) -> Lint {
        self.feature_gate = Some(Feature { name, issue });
        self
    }

    const fn fcw(mut self, reason: Reason) -> Lint {
        self.future_incompatible =
            Some(FutureIncompatible { reason, explain_reason: true, report_in_deps: false });
        self
    }

    /// `fcw` with `report_in_deps: true`.
    const fn fcw_deps(mut self, reason: Reason) -> Lint {
        self.future_incompatible =
            Some(FutureIncompatible { reason, explain_reason: true, report_in_deps: true });
        self
    }

    const fn unexplained(mut self) -> Lint {
        if let Some(it) = &mut self.future_incompatible {
            it.explain_reason = false;
        }
        self
    }
}

/// A lint group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: &'static str,
    pub lints: Vec<&'static Lint>,
    /// For an alias rustc keeps without saying so (`bad_style`): the group it stands for.
    pub alias_of: Option<&'static str>,
}

/// What a name stands for in rustc's lint store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Named {
    Lint(&'static Lint),
    Group(&'static Group),
    /// Renamed: the new name.
    Renamed(&'static str),
    /// Removed: why.
    Removed(&'static str),
    /// A rustdoc lint of old, which rustc takes and does nothing with.
    Ignored,
    Unknown,
}

/// The tools every crate has registered for lints and attributes (rustc_resolve's
/// `PREDEFINED_TOOLS`); `rustc` and `rustdoc` are tools of lints too, though not of attributes.
pub const PREDEFINED_TOOLS: &[&str] = &["clippy", "rustfmt", "diagnostic", "miri", "rust_analyzer"];

/// rustc's builtin lints, in the order it registers them.
pub fn lints() -> &'static [Lint] {
    lints::BUILTIN
}

/// The internal lints rustc registers under `-Z unstable-options`: `rustc::`.
pub fn internal_lints() -> &'static [Lint] {
    lints::INTERNAL
}

/// The builtin lint named `name`, lowercase, as rustc's store has it.
pub fn lint(name: &str) -> Option<&'static Lint> {
    index().lints.get(name).copied()
}

/// The builtin lint group named `name`, aliases included.
pub fn group(name: &str) -> Option<&'static Group> {
    groups().iter().find(|it| it.name == name)
}

/// rustc's lint groups, in the order it adds them: those it makes of the future-incompatible
/// lints as it registers them, those of `register_builtins`, then the alias `bad_style`.
pub fn groups() -> &'static [Group] {
    static GROUPS: OnceLock<Vec<Group>> = OnceLock::new();
    GROUPS.get_or_init(|| build_groups(lints::BUILTIN))
}

/// The internal group, `rustc::internal`.
pub fn internal_group() -> &'static Group {
    static GROUP: OnceLock<Group> = OnceLock::new();
    GROUP.get_or_init(|| Group {
        name: "rustc::internal",
        lints: names::INTERNAL_GROUP
            .iter()
            .map(|name| {
                lints::INTERNAL
                    .iter()
                    .find(|it| it.name == *name)
                    .unwrap_or_else(|| panic!("no internal lint `{name}`"))
            })
            .collect(),
        alias_of: None,
    })
}

/// What `name` stands for among rustc's builtin lints and groups.
pub fn named(name: &str) -> Named {
    if let Some(lint) = lint(name) {
        return Named::Lint(lint);
    }
    if let Some(group) = group(name) {
        return Named::Group(group);
    }
    match index().names.get(name) {
        Some(Entry::Renamed(new)) => Named::Renamed(new),
        Some(Entry::Removed(reason)) => Named::Removed(reason),
        Some(Entry::Ignored) => Named::Ignored,
        None => Named::Unknown,
    }
}

/// The renamed lints: the old name and the new.
pub fn renamed() -> &'static [(&'static str, &'static str)] {
    names::RENAMED
}

/// The removed lints and why.
pub fn removed() -> &'static [(&'static str, &'static str)] {
    names::REMOVED
}

/// The lint `name` is renamed to, following each renaming.
pub fn renamed_to(name: &str) -> Option<&'static str> {
    match index().names.get(name) {
        Some(Entry::Renamed(new)) => Some(new),
        _ => None,
    }
}

fn build_groups(builtin: &'static [Lint]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut add = |groups: &mut Vec<Group>, name: &'static str, lint: &'static Lint| {
        match groups.iter_mut().find(|it| it.name == name) {
            Some(group) => group.lints.push(lint),
            None => groups.push(Group { name, lints: vec![lint], alias_of: None }),
        }
    };
    for lint in builtin {
        if let Some(future) = lint.future_incompatible {
            let name = match future.reason.edition() {
                Some(edition) => edition_group(edition),
                None => "future_incompatible",
            };
            add(&mut groups, name, lint);
        }
    }
    for (name, members) in names::GROUPS {
        let lints = members
            .iter()
            .map(|member| {
                builtin
                    .iter()
                    .find(|it| it.name == *member)
                    .unwrap_or_else(|| panic!("group `{name}` names no lint `{member}`"))
            })
            .collect();
        groups.push(Group { name, lints, alias_of: None });
    }
    for (alias, of) in names::ALIASES {
        let lints = groups.iter().find(|it| it.name == *of).map(|it| it.lints.clone());
        let lints = lints.unwrap_or_else(|| panic!("alias `{alias}` of no group `{of}`"));
        groups.push(Group { name: alias, lints, alias_of: Some(of) });
    }
    groups
}

/// The group rustc puts the lints of an edition in: `Edition::lint_name`.
pub fn edition_group(edition: Edition) -> &'static str {
    match edition {
        Edition::Edition2015 => "rust_2015_compatibility",
        Edition::Edition2018 => "rust_2018_compatibility",
        Edition::Edition2021 => "rust_2021_compatibility",
        Edition::Edition2024 => "rust_2024_compatibility",
    }
}

/// A name of rustc's store that is not a lint's nor a group's.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Renamed(&'static str),
    Removed(&'static str),
    Ignored,
}

struct Index {
    lints: FxHashMap<&'static str, &'static Lint>,
    names: FxHashMap<&'static str, Entry>,
}

fn index() -> &'static Index {
    static INDEX: OnceLock<Index> = OnceLock::new();
    INDEX.get_or_init(|| {
        let lints = lints::BUILTIN.iter().map(|it| (it.name, it)).collect();
        let mut names = FxHashMap::default();
        for name in names::IGNORED {
            names.insert(*name, Entry::Ignored);
        }
        for (old, new) in names::RENAMED {
            names.insert(*old, Entry::Renamed(new));
        }
        for (name, reason) in names::REMOVED {
            names.insert(*name, Entry::Removed(reason));
        }
        Index { lints, names }
    })
}

/// What rustc's `check_lint_name` finds of a name: `CheckLintNameResult`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checked {
    /// The lints it stands for: one, a group's, or none for an ignored name.
    Ok(Vec<&'static Lint>),
    /// Unknown, with a similar name if one is near and whether that one is rustc's own where
    /// a tool's was asked for.
    NoLint(Option<(String, bool)>),
    /// Of a tool rustc does not know.
    NoTool,
    Renamed(&'static str),
    Removed(&'static str),
    /// Of a tool, with the name to use instead when this one is deprecated.
    Tool(Vec<&'static Lint>, Option<String>),
    /// Of a tool that is not loaded: rustc says nothing of it.
    MissingTool,
}

/// rustc's lint store as one crate is checked with it: the builtin lints, the internal ones
/// under `-Z unstable-options`, and the tools of `#![register_tool]` and
/// `#![register_lint_tool]` (those of [`PREDEFINED_TOOLS`] always).
#[derive(Clone, Debug, Default)]
pub struct Store {
    pub internal: bool,
    pub registered_tools: Vec<String>,
}

impl Store {
    /// The lints registered, in rustc's order.
    pub fn lints(&self) -> impl Iterator<Item = &'static Lint> {
        let internal: &'static [Lint] = if self.internal { lints::INTERNAL } else { &[] };
        lints::BUILTIN.iter().chain(internal)
    }

    /// The groups, in rustc's order, aliases included.
    pub fn groups(&self) -> impl Iterator<Item = &'static Group> {
        let internal = self.internal.then(internal_group);
        groups().iter().chain(internal)
    }

    fn by_name(&self, name: &str) -> Option<&'static Lint> {
        lint(name).or_else(|| {
            self.internal.then(|| lints::INTERNAL.iter().find(|it| it.name == name)).flatten()
        })
    }

    fn group(&self, name: &str) -> Option<&'static Group> {
        self.groups().find(|it| it.name == name)
    }

    /// Whether `name` is a group's, or `warnings`: rustc's `is_lint_group`.
    pub fn is_lint_group(&self, name: &str) -> bool {
        name == "warnings" || self.group(name).is_some()
    }

    /// The lints `name` stands for: rustc's `find_lints`.
    pub fn find_lints(&self, name: &str) -> Option<Vec<&'static Lint>> {
        if let Some(lint) = self.by_name(name) {
            return Some(vec![lint]);
        }
        match index().names.get(name) {
            Some(Entry::Renamed(new)) => return Some(vec![self.by_name(new)?]),
            Some(Entry::Removed(_)) => return None,
            Some(Entry::Ignored) => return Some(Vec::new()),
            None => {}
        }
        self.group(name).map(|it| it.lints.clone())
    }

    /// rustc's `check_lint_name` of `name`, written with `tool::` before it when `tool` is
    /// given.
    pub fn check(&self, name: &str, tool: Option<&str>) -> Checked {
        if let Some(tool) = tool
            && tool != "rustc"
            && tool != "rustdoc"
            && !PREDEFINED_TOOLS.contains(&tool)
            && !self.registered_tools.iter().any(|it| it == tool)
        {
            return Checked::NoTool;
        }
        let complete = match tool {
            Some(tool) => format!("{tool}::{name}"),
            None => name.to_owned(),
        };
        if let Some(tool) = tool {
            if let Some(lint) = self.by_name(&complete) {
                return Checked::Tool(vec![lint], None);
            }
            if index().names.get(complete.as_str()).is_none() {
                return match self.group(&complete) {
                    // Every alias of rustc's is a quiet one: none is said to be deprecated.
                    Some(group) => Checked::Tool(group.lints.clone(), None),
                    None => {
                        let prefix = format!("{tool}::");
                        if self.lints().any(|it| it.name.starts_with(&prefix)) {
                            self.no_lint_suggestion(&complete, tool)
                        } else {
                            Checked::MissingTool
                        }
                    }
                };
            }
        }
        if let Some(lint) = self.by_name(&complete) {
            return Checked::Ok(vec![lint]);
        }
        match index().names.get(complete.as_str()) {
            Some(Entry::Renamed(new)) => Checked::Renamed(new),
            Some(Entry::Removed(reason)) => Checked::Removed(reason),
            Some(Entry::Ignored) => Checked::Ok(Vec::new()),
            None => match self.group(&complete) {
                Some(group) => Checked::Ok(group.lints.clone()),
                None => self.backwards_compat(&complete, "clippy"),
            },
        }
    }

    /// rustc's `check_tool_name_for_backwards_compat`: a name of clippy's written without
    /// `clippy::`, which rustc knows when clippy is loaded and this store never is.
    fn backwards_compat(&self, name: &str, tool: &str) -> Checked {
        let complete = format!("{tool}::{name}");
        if let Some(lint) = self.by_name(&complete) {
            return Checked::Tool(vec![lint], Some(complete));
        }
        if index().names.contains_key(complete.as_str()) {
            return Checked::NoLint(None);
        }
        match self.group(&complete) {
            Some(group) => Checked::Tool(group.lints.clone(), Some(complete)),
            None => self.no_lint_suggestion(name, tool),
        }
    }

    /// rustc's `no_lint_suggestion`: the name in lowercase when that is a lint's, else the
    /// nearest name of a group (aliases left out) or a lint.
    fn no_lint_suggestion(&self, name: &str, tool: &str) -> Checked {
        let lower = name.to_lowercase();
        if name.chars().any(char::is_uppercase) && self.find_lints(&lower).is_some() {
            return Checked::NoLint(Some((lower, false)));
        }
        let mut groups: Vec<&str> =
            self.groups().filter(|it| it.alias_of.is_none()).map(|it| it.name).collect();
        groups.sort_unstable();
        let names: Vec<&str> = groups.into_iter().chain(self.lints().map(|it| it.name)).collect();
        let mut lookups = vec![lower.as_str()];
        if let Some(last) = lower.split("::").last() {
            lookups.push(last);
        }
        let found = edit_distance::find_best_match_for_names(&names, &lookups, None);
        let suggestion = found.map(|it| {
            let is_rustc = lower.contains("::") && !it.starts_with(tool);
            (it.to_owned(), is_rustc)
        });
        Checked::NoLint(suggestion)
    }

    /// The first group, aliases left out, that has `lint`: what rustc's note of a default
    /// level names it part of.
    pub fn group_of(&self, lint: &Lint) -> Option<&'static str> {
        self.groups()
            .filter(|it| it.alias_of.is_none())
            .find(|it| it.lints.iter().any(|it| it.name == lint.name))
            .map(|it| it.name)
    }
}

#[cfg(test)]
mod tests;
