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
//! Follows rustc's `compiler/rustc_session/src/config.rs` (`get_cmd_lint_options`),
//! `compiler/rustc_middle/src/lint.rs` (`emit_lint_base`, `explain_lint_level_source`),
//! `compiler/rustc_lint/src/expect.rs` (`check_expectations`) and
//! `compiler/rustc_interface/src/passes.rs` (the pre-expansion lint pass before expansion, the
//! early one after resolution, the expectations checked after the late lints) (MIT or
//! Apache-2.0).

mod attrs;
mod builder;

use cfg::CfgOptions;
use hir::HirFileId;
use hir_def::unstable_features::UnstableFeatures;
use rustc_hash::FxHashSet;
use syntax::ast;

pub use crate::lints::registry::Level;

use self::builder::{Levels, owner_rank, root_file};
use crate::{
    cx::Cx,
    lints::{
        CheckCfg,
        registry::{self, Lint, Store},
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

/// A lint's level where it is: rustc's `LevelSpec`.
#[derive(Clone, Debug)]
pub(super) struct Spec {
    level: Level,
    /// For `expect`, and `--force-warn` over one: the expectation a lint at it fulfills, by
    /// where the name is in the `#[expect]`.
    expect: Option<Span>,
    source: Source,
}

/// Where a level came from: rustc's `LintLevelSource`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Source {
    Default,
    /// An attribute: the name it sets the level of (the new one of a renamed lint, with its
    /// tool for a tool's), where the name is, and its `reason`.
    Node { name: String, span: Span, reason: Option<String> },
    /// The command line: the name given, with `_` for `-`, and the level of its flag.
    CommandLine { name: String, level: Level },
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
    /// The expectations a lint at their level fulfilled.
    fulfilled: FxHashSet<Span>,
}

impl Shared<'_, '_, '_> {
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
        if let Some(expect) = &spec.expect {
            self.fulfilled.insert(expect.clone());
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
        self.explain(lint, &spec, &mut diag);
        Some(diag)
    }

    /// rustc's `explain_lint_level_source`.
    fn explain(&self, lint: &'static Lint, spec: &Spec, diag: &mut Diag) {
        let level = spec.level;
        if level == Level::Allow {
            return;
        }
        let once = |diag: &mut Diag, sublevel: Sublevel, message: String| {
            diag.sub(sublevel, message, MultiSpan::default(), true);
        };
        let name = lint.name;
        let shown = level.as_str();
        match &spec.source {
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
                if given == name {
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
                    diag.note(reason.clone());
                }
                diag.sub(Sublevel::Note, "the lint level is defined here", MultiSpan::of(span.clone()), true);
                if attr != name {
                    once(diag, Sublevel::Note, format!("`#[{shown}({name})]` implied by `#[{shown}({attr})]`"));
                }
            }
        }
        let warnings = self.cmd.opts.iter().find(|(it, _)| it == "warnings").map(|(_, it)| *it);
        if let Some(warnings) = warnings
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
/// what is of [`Phase::Late`] is dropped.
pub(crate) fn decide(cx: &Cx<'_, '_>, cmd: &CommandLine, found: Vec<Found>, aborted: bool) -> Vec<Diag> {
    let walk = Walk::of(cx);
    let root = root_file(cx).expect("a crate's root module is a file");
    let registered_tools = walk
        .tree(root)
        .and_then(|it| ast::AnyHasAttrs::cast(it.root.clone()))
        .map(|it| attrs::registered_tools(&it, walk.cfg, cx.edition))
        .unwrap_or_default();
    let mut sh = Shared {
        cx,
        cmd,
        store: Store { internal: false, registered_tools },
        features: UnstableFeatures::query(cx.db, cx.krate.base()),
        cfg: walk.cfg,
        root,
        fulfilled: FxHashSet::default(),
    };
    let mut early = Levels::new(&mut sh, true);
    early.walk(&mut sh, &walk);
    let mut late = Levels::new(&mut sh, false);
    late.walk(&mut sh, &walk);

    let (mut pre, mut during): (Vec<_>, Vec<_>) = std::mem::take(&mut early.emitted).into_iter().partition(|it| it.pre);
    pre.sort_by(|a, b| a.place.cmp(&b.place));
    during.sort_by(|a, b| a.place.cmp(&b.place));
    let mut pre = pre.into_iter().map(|it| it.diag);
    let mut during = during.into_iter().map(|it| it.diag);

    let found: Vec<Found> = match aborted {
        true => found.into_iter().filter(|it| it.phase != Phase::Late).collect(),
        false => found,
    };
    let pre_at = found.iter().position(|it| it.phase != Phase::Parse || it.diag.is_lint()).unwrap_or(found.len());
    let early_at = found
        .iter()
        .position(|it| it.phase > Phase::Resolve || (it.phase == Phase::Resolve && it.diag.is_lint()))
        .unwrap_or(found.len());
    let mut out = Vec::with_capacity(found.len());
    for (index, it) in found.into_iter().enumerate() {
        if index == pre_at {
            out.extend(pre.by_ref());
        }
        if index == early_at {
            out.extend(during.by_ref());
        }
        let name = it.diag.lint_name().map(str::to_owned);
        let (Some(at), Some(name)) = (it.lint_at, name) else {
            out.push(it.diag);
            continue;
        };
        let lint = registry::declared(&name)
            .unwrap_or_else(|| panic!("`{name}` is not a lint of rustc's, given as one at {at:?}"));
        let levels = match it.phase <= Phase::Resolve {
            true => &mut early,
            false => &mut late,
        };
        let set = levels.set_at(&mut sh, cx.syntax(at));
        let spec = levels.spec(&sh, lint, set);
        out.extend(sh.finish(lint, spec, it.diag));
    }
    out.extend(pre);
    out.extend(during);
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
        if sh.fulfilled.contains(&expectation.key) {
            continue;
        }
        let mut diag = Diag::lint(unfulfilled.name, "this lint expectation is unfulfilled", expectation.key);
        if let Some(reason) = expectation.reason {
            diag.note(reason);
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
