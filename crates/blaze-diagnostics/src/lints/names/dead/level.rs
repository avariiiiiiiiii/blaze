//! The level of `dead_code` at a node, as the dead-code pass asks for it (`lint_level_spec_at_node`):
//! the `allow`, `expect`, `warn`, `deny` and `forbid` attributes from the crate root down to the
//! node, through the modules' declarations and the macro calls a node was expanded from, each
//! naming `dead_code`, a group with it or `warnings`; a `forbid` not lowered; then a `warn` that
//! `warnings` changes.
//!
//! Follows `compiler/rustc_lint/src/levels.rs` (`LintLevelsBuilder::add`, `push`),
//! `compiler/rustc_middle/src/lint.rs` (`reveal_actual_level`) (MIT or Apache-2.0).

use std::cell::RefCell;

use hir::{HirFileId, InFile};
use hir_expand::MacroCallKind;
use rustc_hash::FxHashMap;
use syntax::{AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, ast};

use crate::{
    cx::Cx,
    lints::registry::{self, Level, Named},
};

/// The `#[expect]` a level comes from: the node of the attribute's owner and the lint's place
/// among the lints its attributes name.
pub(super) type ExpectId = (InFile<SyntaxNodePtr>, u32);

/// rustc's level and the expectation it fulfills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LevelPlus {
    pub(super) level: Level,
    pub(super) expect: Option<ExpectId>,
}

impl LevelPlus {
    pub(super) fn is_allow_or_expect(self) -> bool {
        matches!(self.level, Level::Allow | Level::Expect)
    }
}

/// The levels asked for, each node once.
pub(super) struct Levels {
    at: RefCell<FxHashMap<InFile<SyntaxNodePtr>, LevelPlus>>,
}

impl Levels {
    pub(super) fn new() -> Levels {
        Levels { at: RefCell::new(FxHashMap::default()) }
    }

    /// The level of `dead_code` at `node`.
    pub(super) fn at(&self, cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> LevelPlus {
        let key = node.as_ref().map(SyntaxNodePtr::new);
        if let Some(it) = self.at.borrow().get(&key) {
            return *it;
        }
        let level = level_at(cx, node);
        self.at.borrow_mut().insert(key, level);
        level
    }
}

/// What the attributes set so far: of `dead_code`, and of `warnings`.
struct State {
    dead_code: Option<LevelPlus>,
    warnings: Option<LevelPlus>,
}

fn level_at(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> LevelPlus {
    let chain = chain(cx, node);
    let mut state = State { dead_code: None, warnings: None };
    for owner in chain.iter().rev() {
        apply(cx, owner, &mut state);
    }
    let level = state.dead_code.unwrap_or(LevelPlus { level: default_level(), expect: None });
    if level.level == Level::Warn
        && let Some(warnings) = state.warnings
        && warnings.level != Level::Warn
    {
        return warnings;
    }
    level
}

fn default_level() -> Level {
    registry::lint("dead_code").map_or(Level::Warn, |it| it.default_level)
}

/// The nodes whose attributes set the level at `node`, innermost first: `node`, its ancestors,
/// the calls of the macros it was expanded from, the declarations of the modules it is in.
fn chain(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Vec<InFile<ast::AnyHasAttrs>> {
    let mut chain = Vec::new();
    let mut at = Some(node.clone());
    while let Some(node) = at.take() {
        let mut top = node.value.clone();
        for ancestor in node.value.ancestors() {
            if let Some(owner) = ast::AnyHasAttrs::cast(ancestor.clone())
                && ast::attrs_including_inner(&owner).next().is_some()
            {
                chain.push(node.with_value(owner));
            }
            top = ancestor;
        }
        at = above(cx, node.file_id, &top);
    }
    chain
}

/// Where the levels at the root `top` of `file` come from: the macro call it was expanded from
/// (for a derive's or an attribute's, what is around the item), or the declaration of the
/// module whose file it is.
fn above(cx: &Cx<'_, '_>, file: HirFileId, top: &SyntaxNode) -> Option<InFile<SyntaxNode>> {
    if let Some(call) = file.macro_file() {
        let node = call.call_node(cx.db);
        return match call.loc(cx.db).kind {
            MacroCallKind::FnLike { .. } => Some(node),
            MacroCallKind::Derive { .. } | MacroCallKind::Attr { .. } => {
                let parent = node.value.parent()?;
                Some(node.with_value(parent))
            }
        };
    }
    let file = file.file_id()?;
    if top.kind() != SyntaxKind::SOURCE_FILE {
        return None;
    }
    let module = cx.sema.file_to_module_def(file.file_id(cx.db))?;
    if module.is_crate_root(cx.db) {
        return None;
    }
    let declaration = module.declaration_source(cx.db)?;
    Some(declaration.map(|it| it.syntax().clone()))
}

/// The lint attributes of `owner`, in order, as they set `dead_code` and `warnings`.
fn apply(cx: &Cx<'_, '_>, owner: &InFile<ast::AnyHasAttrs>, state: &mut State) {
    let file = owner.file_id.original_file(cx.db).file_id(cx.db);
    let ptr = owner.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
    for (index, (attr, name)) in cx.sema.lint_attrs(file, cx.krate, owner.value.clone()).enumerate() {
        let level = match attr {
            hir::LintAttr::Allow => Level::Allow,
            hir::LintAttr::Expect => Level::Expect,
            hir::LintAttr::Warn => Level::Warn,
            hir::LintAttr::Deny => Level::Deny,
            hir::LintAttr::Forbid => Level::Forbid,
        };
        let set = LevelPlus { level, expect: (level == Level::Expect).then_some((ptr, index as u32)) };
        if name == "warnings" {
            set_level(&mut state.warnings, set);
            continue;
        }
        if names_dead_code(&name) {
            set_level(&mut state.dead_code, set);
        }
    }
}

/// rustc's `insert_spec`: a level under a `forbid` stays `forbid`.
fn set_level(slot: &mut Option<LevelPlus>, set: LevelPlus) {
    if slot.is_some_and(|it| it.level == Level::Forbid) {
        return;
    }
    *slot = Some(set);
}

/// Whether an attribute's `name` names `dead_code`: itself, a group with it, or a name that was
/// renamed to one of those.
fn names_dead_code(name: &str) -> bool {
    match registry::named(name) {
        Named::Lint(lint) => lint.name == "dead_code",
        Named::Group(group) => group.lints.iter().any(|it| it.name == "dead_code"),
        Named::Renamed(new) => new != name && names_dead_code(new),
        _ => false,
    }
}
