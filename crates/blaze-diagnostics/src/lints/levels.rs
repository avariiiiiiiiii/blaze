//! The level of each lint found, as rustc decides it: the command line (`-A`, `-W`,
//! `--force-warn`, `-D`, `-F` in order, `--cap-lints`), the lint attributes on the node and
//! around it (through the macro call an expansion is of, the `mod` a file is of, to the
//! crate's), `cfg_attr`, `#![warn(warnings)]`, `forbid`, `expect`, `reason`; rustc's notes on
//! where a level came from and on a future-incompatible lint; what rustc says of the names in
//! the attributes and on the command line (unknown, renamed, removed, a tool's, unstable,
//! malformed, `forbid` overruled, only at crate level); the `#[expect]`s nothing fulfilled.
//!
//! - `attrs`: the lint attributes of a node as rustc reads them, and the node around it.
//! - `builder`: rustc's `LintLevelsBuilder`, as the early lint pass has it and as the HIR
//!   has it.
//!
//! Cost: a node with no attribute costs the walk a check of its kind. A lint found costs one
//! FxHash of its name to its lint, one of its node (or of each node out to the nearest one
//! seen), and a scan of the 2-byte lint ids of each set of levels from there out; a level is a
//! 12-byte `Spec`. Nothing is allocated for a lint unless it is shown.
//!
//! Follows rustc's `compiler/rustc_session/src/config.rs` (`get_cmd_lint_options`),
//! `compiler/rustc_middle/src/lint.rs` (`emit_lint_base`, `explain_lint_level_source`),
//! `compiler/rustc_lint/src/expect.rs` (`check_expectations`) and
//! `compiler/rustc_interface/src/passes.rs` (the pre-expansion lint pass before expansion, the
//! early one after resolution, the expectations checked after the late lints) (MIT or
//! Apache-2.0).

mod attrs;
mod builder;

use std::num::NonZeroU32;

use cfg::CfgOptions;
use hir::{HirFileId, InFile};
use hir_def::unstable_features::UnstableFeatures;
use rustc_hash::FxHashMap;
use syntax::{SyntaxNodePtr, TextRange, ast};

pub use crate::lints::registry::Level;

use self::builder::{Levels, When, owner_rank, root_file};
use crate::{
    cx::Cx,
    lints::{
        CheckCfg,
        registry::{self, Lint, LintId, Store},
        syntax::Walk,
    },
    lower::{Found, Phase},
    model::{self, Diag, MultiSpan, Span, Sublevel},
};

/// The lint flags rustc is given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandLine {
    /// `-A`, `-W`, `--force-warn`, `-D` and `-F`, in the order given: the lint, group or
    /// tool's lint, with `_` for `-`, and its level.
    pub opts: Vec<(String, Level)>,
    /// `--cap-lints`.
    pub cap: Option<Level>,
    /// `--check-cfg`.
    pub check_cfg: CheckCfg,
}

impl CommandLine {
    /// The lint flags among rustc's arguments `args`: `-A x`, `-Ax`, `--allow x`,
    /// `--allow=x` and so on, `--cap-lints` and `--check-cfg`. The other arguments are not
    /// lint flags and are passed by.
    pub fn parse(args: &[String]) -> Result<CommandLine, String> {
        let mut flags = CommandLine::default();
        let mut args = args.iter();
        let levels = [
            ("--allow", Level::Allow),
            ("--warn", Level::Warn),
            ("--force-warn", Level::ForceWarn),
            ("--deny", Level::Deny),
            ("--forbid", Level::Forbid),
            ("-A", Level::Allow),
            ("-W", Level::Warn),
            ("-D", Level::Deny),
            ("-F", Level::Forbid),
        ];
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| -> Result<Option<String>, String> {
                if arg == flag {
                    let next = args.next().ok_or_else(|| format!("Argument to option '{}' missing", flag.trim_start_matches('-')))?;
                    return Ok(Some(next.clone()));
                }
                let rest = arg.strip_prefix(flag);
                let rest = match flag.starts_with("--") {
                    true => rest.and_then(|it| it.strip_prefix('=')),
                    false => rest,
                };
                Ok(rest.map(str::to_owned))
            };
            if let Some(cap) = value("--cap-lints")? {
                if flags.cap.is_some() {
                    return Err("Option 'cap-lints' given more than once".to_owned());
                }
                flags.cap = Some(Level::of_cap(&cap).ok_or_else(|| format!("unknown lint level: `{cap}`"))?);
                continue;
            }
            if let Some(spec) = value("--check-cfg")? {
                flags.check_cfg.add(&spec)?;
                continue;
            }
            let mut found = None;
            for (flag, level) in levels {
                if let Some(name) = value(flag)? {
                    found = Some((name, level));
                    break;
                }
            }
            let Some((name, level)) = found else { continue };
            if name == "help" {
                return Err(format!("`{} help` asks for rustc's list of lints, which blaze does not write", level.to_cmd_flag()));
            }
            flags.opts.push((name.replace('-', "_"), level));
        }
        Ok(flags)
    }
}

/// A lint's level where it is: rustc's `LevelSpec`, as 12 bytes. What rustc keeps in it of
/// where the level came from is in [`Shared::sources`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Spec {
    level: Level,
    /// For `expect`, and `--force-warn` over one: the expectation a lint at it fulfills.
    expect: Option<ExpectId>,
    source: SourceId,
}

const _: () = assert!(size_of::<Spec>() == 12);

/// An `#[expect]`'s name, by where it is: an index into [`Shared::fulfilled`], from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ExpectId(NonZeroU32);

impl ExpectId {
    fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}

/// An index into [`Shared::sources`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SourceId(u32);

impl SourceId {
    /// rustc's `LintLevelSource::Default`, always the first.
    pub(super) const DEFAULT: SourceId = SourceId(0);
}

/// Where a level came from: rustc's `LintLevelSource`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Source {
    Default,
    /// An attribute: the name it sets the level of (the new one of a renamed lint, with its
    /// tool for a tool's), where the name is, and its `reason`.
    Node { name: Box<str>, span: Span, reason: Option<Box<str>> },
    /// The command line: the name given, with `_` for `-`, and the level of its flag.
    CommandLine { name: Box<str>, level: Level },
}

/// What deciding levels in a crate reads and what it finds along the way.
pub(super) struct Shared<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    cmd: &'a CommandLine,
    store: Store,
    features: &'a UnstableFeatures,
    cfg: &'a CfgOptions,
    /// The crate's root file.
    root: HirFileId,
    /// The level the first `-D warnings` or the like gives `warnings` on the command line.
    cmd_warnings: Option<Level>,
    /// The `warnings` lint.
    warnings: LintId,
    /// Where each level came from, by [`SourceId`]: one per name in an attribute or on the
    /// command line, not one per lint or per node.
    sources: Vec<Source>,
    /// Each `#[expect]`'s name by where it is, hashed once as both builders come to it.
    expects: FxHashMap<(HirFileId, TextRange), ExpectId>,
    /// Bit `i`: whether a lint at its level fulfilled the expectation of index `i`.
    fulfilled: Vec<u64>,
}

impl<'a, 'b, 'db> Shared<'a, 'b, 'db> {
    fn new(cx: &'a Cx<'b, 'db>, cmd: &'a CommandLine, cfg: &'a CfgOptions, root: HirFileId) -> Self {
        let registered_tools = ast::AnyHasAttrs::cast(cx.sema.parse_or_expand(root))
            .map(|it| attrs::registered_tools(&it, cfg, cx.edition))
            .unwrap_or_default();
        Shared {
            cx,
            cmd,
            store: Store { internal: false, registered_tools },
            features: UnstableFeatures::query(cx.db, cx.krate.base()),
            cfg,
            root,
            cmd_warnings: cmd.opts.iter().find(|(it, _)| it == "warnings").map(|&(_, it)| it),
            warnings: LintId::of(registry::lint("warnings").expect("a builtin lint")),
            sources: vec![Source::Default],
            expects: FxHashMap::default(),
            fulfilled: Vec::new(),
        }
    }

    pub(super) fn source(&mut self, source: Source) -> SourceId {
        let id = SourceId(u32::try_from(self.sources.len()).expect("fewer than 2^32 lint names"));
        self.sources.push(source);
        id
    }

    /// The expectation of the `#[expect]` name at `range` of `file`, the same to both builders.
    pub(super) fn expect_id(&mut self, file: HirFileId, range: TextRange) -> ExpectId {
        let next = self.expects.len();
        *self.expects.entry((file, range)).or_insert_with(|| {
            let id = u32::try_from(next + 1).ok().and_then(NonZeroU32::new).expect("fewer than 2^32 expectations");
            ExpectId(id)
        })
    }

    fn fulfill(&mut self, id: ExpectId) {
        let (word, bit) = (id.index() / 64, id.index() % 64);
        if self.fulfilled.len() <= word {
            self.fulfilled.resize(word + 1, 0);
        }
        self.fulfilled[word] |= 1 << bit;
    }

    fn is_fulfilled(&self, id: ExpectId) -> bool {
        let (word, bit) = (id.index() / 64, id.index() % 64);
        self.fulfilled.get(word).is_some_and(|it| it & (1 << bit) != 0)
    }
    /// `diag` of `lint` at `spec` as rustc's `emit_lint_base` emits it: nothing when it is
    /// allowed or expected (the expectation fulfilled), or in another crate's macro and not of
    /// a lint given there; else at its level, with the future-incompatible warning and link
    /// and the notes on where its level came from.
    fn finish(&mut self, lint: &'static Lint, spec: Spec, mut diag: Diag) -> Option<Diag> {
        if spec.level == Level::Allow {
            return None;
        }
        if diag.span.primary.iter().any(attrs::external) {
            let incompatible = lint.future_incompatible.is_some_and(|it| it.reason.edition().is_none());
            if !incompatible && !lint.report_in_external_macro {
                return None;
            }
            diag.suggestions_disabled = true;
        }
        if let Some(expect) = spec.expect {
            self.fulfill(expect);
        }
        if spec.level == Level::Expect {
            return None;
        }
        diag.level = match spec.level.is_error() {
            true => model::Level::Error,
            false => model::Level::Warning,
        };
        if let Some(future) = lint.future_incompatible {
            if future.explain_reason {
                diag.warn(future.reason.explanation(self.cx.edition));
            }
            diag.note(format!("for more information, see {}", future.reason.reference()));
        }
        self.explain(lint, spec, &mut diag);
        Some(diag)
    }

    /// rustc's `explain_lint_level_source`.
    fn explain(&self, lint: &'static Lint, spec: Spec, diag: &mut Diag) {
        let level = spec.level;
        if level == Level::Allow {
            return;
        }
        let once = |diag: &mut Diag, sublevel: Sublevel, message: String| {
            diag.sub(sublevel, message, MultiSpan::default(), true);
        };
        let name = lint.name;
        let shown = level.as_str();
        match &self.sources[spec.source.0 as usize] {
            Source::Default => {
                let note = match self.store.group_of(lint) {
                    Some(group) => format!("`#[{shown}({name})]` (part of `#[{shown}({group})]`) on by default"),
                    None => format!("`#[{shown}({name})]` on by default"),
                };
                once(diag, Sublevel::Note, note);
            }
            Source::CommandLine { name: given, level: flagged } => {
                let flag = flagged.to_cmd_flag();
                let hyphens = name.replace('_', "-");
                if &**given == name {
                    once(diag, Sublevel::Note, format!("requested on the command line with `{flag} {hyphens}`"));
                } else {
                    let given = given.replace('_', "-");
                    once(diag, Sublevel::Note, format!("`{flag} {hyphens}` implied by `{flag} {given}`"));
                    if matches!(flagged, Level::Warn | Level::Deny) {
                        let help = match name == "dead_code" {
                            true => format!("to override `{flag} {given}` add `#[expect({name})]` or `#[allow({name})]`"),
                            false => format!("to override `{flag} {given}` add `#[allow({name})]`"),
                        };
                        once(diag, Sublevel::Help, help);
                    }
                }
            }
            Source::Node { name: attr, span, reason } => {
                if let Some(reason) = reason {
                    diag.note(&**reason);
                }
                diag.sub(Sublevel::Note, "the lint level is defined here", MultiSpan::of(span.clone()), true);
                if &**attr != name {
                    once(diag, Sublevel::Note, format!("`#[{shown}({name})]` implied by `#[{shown}({attr})]`"));
                }
            }
        }
        if let Some(warnings) = self.cmd_warnings
            && warnings >= Level::Deny
            && level < warnings
        {
            once(diag, Sublevel::Note, format!("the `{name}` lint ignores `-D warnings`"));
        }
    }
}

/// The diagnostics of `found` with the level of each lint decided, and what deciding them
/// says, each where rustc gives it: the command line's and the malformed attributes of the
/// root file as the pre-expansion lint pass runs, after the parser's errors; the rest of what
/// is said of the attributes as the early lint pass comes to them, before the early lints;
/// the `#[expect]`s nothing fulfilled after everything, unless `aborted`. When `aborted`,
/// what is of [`Phase::Late`] is dropped. What more than one of rustc's builders says is here
/// as many times, for `-Z deduplicate-diagnostics=no`: see [`builder::When`].
pub(crate) fn decide(cx: &Cx<'_, '_>, cmd: &CommandLine, found: Vec<Found>, aborted: bool) -> Vec<Diag> {
    let walk = Walk::of(cx);
    let root = root_file(cx).expect("a crate's root module is a file");
    let mut sh = Shared::new(cx, cmd, walk.cfg, root);
    let mut early = Levels::new(&mut sh, true, true);
    early.walk(&mut sh, &walk);
    let mut late = Levels::new(&mut sh, false, true);
    late.walk(&mut sh, &walk);

    let (mut pre, mut during): (Vec<_>, Vec<_>) =
        std::mem::take(&mut early.emitted).into_iter().partition(|it| it.when == When::Pre);
    pre.sort_by(|a, b| a.place.cmp(&b.place));
    during.sort_by(|a, b| a.place.cmp(&b.place));
    let hir = std::mem::take(&mut late.emitted);
    let extra = pre.len() + during.len() + hir.len();
    let mut pre = pre.into_iter().map(|it| it.diag);
    let mut during = during.into_iter().map(|it| it.diag);
    let mut hir = hir.into_iter().map(|it| it.diag);

    let mut found = found;
    if aborted {
        found.retain(|it| it.phase != Phase::Late);
    }
    let pre_at = found.iter().position(|it| it.phase != Phase::Parse || it.diag.is_lint()).unwrap_or(found.len());
    let early_at = found
        .iter()
        .position(|it| it.phase > Phase::Resolve || (it.phase == Phase::Resolve && it.diag.is_lint()))
        .unwrap_or(found.len());
    let hir_at = found.iter().position(|it| it.phase > Phase::Resolve).unwrap_or(found.len());
    let mut out = Vec::with_capacity(found.len() + extra + late.expectations.len());
    for (index, it) in found.into_iter().enumerate() {
        if index == pre_at {
            out.extend(pre.by_ref());
        }
        if index == early_at {
            out.extend(during.by_ref());
        }
        if index == hir_at {
            out.extend(hir.by_ref());
        }
        let lint = it.diag.lint_name().map(|name| {
            registry::declared(name).unwrap_or_else(|| panic!("`{name}` is not a lint of rustc's"))
        });
        let (Some(at), Some(lint)) = (it.lint_at, lint) else {
            out.push(it.diag);
            continue;
        };
        let levels = match it.phase <= Phase::Resolve {
            true => &mut early,
            false => &mut late,
        };
        let set = levels.set_of(&mut sh, at);
        let spec = levels.spec(&sh, lint, set);
        out.extend(sh.finish(lint, spec, it.diag));
    }
    out.extend(pre);
    out.extend(during);
    out.extend(hir);
    if aborted {
        return out;
    }

    let unfulfilled = registry::lint("unfulfilled_lint_expectations").expect("a builtin lint");
    let mut expectations: Vec<_> = std::mem::take(&mut late.expectations)
        .into_iter()
        .map(|it| ((owner_rank(&sh, &it.node), attrs::place(cx.sema, &it.node)), it))
        .collect();
    expectations.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, expectation) in expectations {
        if sh.is_fulfilled(expectation.id) {
            continue;
        }
        let mut diag = Diag::lint(unfulfilled.name, "this lint expectation is unfulfilled", expectation.key);
        if let Some(reason) = expectation.reason {
            diag.note(reason.into_string());
        }
        if expectation.is_unfulfilled {
            diag.note("the `unfulfilled_lint_expectations` lint can't be expected and will always produce this message");
        }
        let set = late.set_at(&mut sh, expectation.node);
        let spec = late.spec(&sh, unfulfilled, set);
        out.extend(sh.finish(unfulfilled, spec, diag));
    }
    out
}

/// The level rustc's HIR levels give the lint `lint` at the node `at` (`lint_level_at_node`):
/// the attributes on it and around it, the command line `cmd`, `warnings` and `--cap-lints`.
/// For a pass that must know a level before it can say what to emit (`missing_unsafe`'s
/// `unsafe_op_in_unsafe_fn`). It reads the attributes of `at` and of each node out from it,
/// with no walk of the crate and nothing said of the names: call it once for a diagnostic,
/// not once for each node.
pub(crate) fn level_at(cx: &Cx<'_, '_>, cmd: &CommandLine, lint: &str, at: InFile<SyntaxNodePtr>) -> Level {
    let lint = registry::declared(lint).unwrap_or_else(|| panic!("`{lint}` is not a lint"));
    let root = root_file(cx).expect("a crate's root module is a file");
    let mut sh = Shared::new(cx, cmd, cx.krate.cfg(cx.db), root);
    let mut levels = Levels::new(&mut sh, false, false);
    let set = levels.set_of(&mut sh, at);
    levels.spec(&sh, lint, set).level
}

#[cfg(test)]
mod tests {
    use hir::Semantics;
    use syntax::AstNode;

    use super::*;

    /// [`level_at`] of `lint` at each function of the first workspace crate of `fixture`, by
    /// name, with the lint flags `args`.
    fn levels_of_fns(fixture: &str, args: &[&str], lint: &str) -> Vec<(String, Level)> {
        let checked = crate::tests::checked(fixture);
        let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
        let cmd = CommandLine::parse(&args).unwrap();
        let db = &checked.db;
        hir::attach_db(db, || {
            let sema = Semantics::new_dyn(db);
            let krate = hir::check::workspace_crates(db)[0];
            let cx = Cx::new(&sema, krate, &checked.names);
            let walk = Walk::of(&cx);
            let mut out = Vec::new();
            for tree in &walk.trees {
                for node in &tree.nodes {
                    let Some(name) = ast::Fn::cast(node.clone()).and_then(|it| it.name()) else { continue };
                    let at = InFile::new(tree.file, SyntaxNodePtr::new(node));
                    out.push((name.text().to_owned(), level_at(&cx, &cmd, lint, at)));
                }
            }
            out
        })
    }

    #[test]
    fn level_at_a_node_has_its_attributes_the_command_line_and_the_cap() {
        let fixture = r#"
//- /main.rs crate:main edition:2021
#![deny(unsafe_op_in_unsafe_fn)]
mod m {
    #[allow(unsafe_op_in_unsafe_fn)]
    pub fn allowed() {}
    pub fn denied() {}
}
#[expect(unsafe_op_in_unsafe_fn)]
fn expected() {}
fn main() {}
"#;
        let lint = "unsafe_op_in_unsafe_fn";
        let named = |levels: &[(&str, Level)]| -> Vec<(String, Level)> {
            levels.iter().map(|&(name, level)| (name.to_owned(), level)).collect()
        };
        assert_eq!(
            levels_of_fns(fixture, &[], lint),
            named(&[("allowed", Level::Allow), ("denied", Level::Deny), ("expected", Level::Expect), ("main", Level::Deny)])
        );
        assert_eq!(
            levels_of_fns(fixture, &["--cap-lints", "warn"], lint),
            named(&[("allowed", Level::Allow), ("denied", Level::Warn), ("expected", Level::Expect), ("main", Level::Warn)])
        );
        let plain = "//- /main.rs crate:main edition:2021\nfn main() {}\n";
        assert_eq!(levels_of_fns(plain, &[], lint), named(&[("main", Level::Allow)]));
        assert_eq!(levels_of_fns(plain, &["-F", "unsafe-op-in-unsafe-fn"], lint), named(&[("main", Level::Forbid)]));
    }
}
