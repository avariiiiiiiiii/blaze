//! The level of `dead_code` at a node, as the dead-code pass asks for it (`lint_level_spec_at_node`):
//! the `allow`, `expect`, `warn`, `deny` and `forbid` attributes from the crate root down to the
//! node, through the modules' declarations and the macro calls a node was expanded from, each
//! naming `dead_code`, a group with it or `warnings`; a `forbid` not lowered; then a `warn` that
//! `warnings` changes.
//!
//! What the attributes set is memoized at each node that has attributes and at each file's root
//! (what is set above it), as rustc's `ShallowLintLevelMap` keeps it per owner: a query walks up
//! only to the nearest node already seen, and each owner's attributes are read once.
//!
//! Follows `compiler/rustc_lint/src/levels.rs` (`LintLevelsBuilder::add`, `push`),
//! `compiler/rustc_middle/src/lint.rs` (`reveal_actual_level`) (MIT or Apache-2.0).

use std::cell::RefCell;

use hir::{HirFileId, InFile};
use hir_expand::MacroCallKind;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::registry::{self, Level, LintId, Named},
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

/// What the attributes set so far: of `dead_code`, and of `warnings`.
#[derive(Clone, Copy, Debug, Default)]
struct State {
    dead_code: Option<LevelPlus>,
    warnings: Option<LevelPlus>,
}

/// The states asked for: one FxHash probe per node with attributes walked through, and one per
/// file.
pub(super) struct Levels {
    /// After the attributes of each node that has some.
    owners: RefCell<FxHashMap<InFile<SyntaxNodePtr>, State>>,
    /// Above the root of each file: what its macro call or module declaration has.
    files: RefCell<FxHashMap<HirFileId, State>>,
}

impl Levels {
    pub(super) fn new() -> Levels {
        Levels { owners: RefCell::default(), files: RefCell::default() }
    }

    /// The level of `dead_code` at `node`.
    pub(super) fn at<'db>(&self, cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> LevelPlus {
        resolve(self.state(cx, node))
    }

    /// The level of `dead_code` at an impl a builtin derive of `adt` makes with no tokens:
    /// rustc's `create_derived_impl` puts it beside the ADT and copies the ADT's `allow`,
    /// `warn`, `deny` and `forbid` onto it (not `expect`, and not the ADT's other attributes).
    pub(super) fn at_derived<'db>(&self, cx: &Cx<'_, 'db>, adt: InFile<ast::Node<'db>>) -> LevelPlus {
        let mut state = match adt.value.parent() {
            Some(parent) => self.state(cx, adt.with_value(parent)),
            None => self.above_file(cx, adt.file_id, adt.value),
        };
        if let Some(owner) = ast::AnyHasAttrs::cast(adt.value) {
            apply(cx, adt.with_value(owner), ast::ptr(adt.value), &mut state, Copied::ToDerives);
        }
        resolve(state)
    }
}

/// `dead_code`'s level from what the attributes set: a `warn` that `warnings` changes.
fn resolve(state: State) -> LevelPlus {
    let level = state.dead_code.unwrap_or(LevelPlus { level: default_level(), expect: None });
    if level.level == Level::Warn
        && let Some(warnings) = state.warnings
        && warnings.level != Level::Warn
    {
        return warnings;
    }
    level
}

impl Levels {
    /// What is set at `node`: the state of the nearest memoized node above it, then the
    /// attributes of the nodes with attributes between, outermost first, each memoized.
    fn state<'db>(&self, cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> State {
        let mut pending: SmallVec<[(SyntaxNodePtr, ast::AnyHasAttrs<'db>); 4]> = SmallVec::new();
        let mut base = None;
        let mut top = node.value;
        for ancestor in node.value.ancestors() {
            if let Some(owner) = ast::AnyHasAttrs::cast(ancestor)
                && ast::attrs_including_inner(&owner).next().is_some()
            {
                let ptr = ast::ptr(ancestor);
                if let Some(&state) = self.owners.borrow().get(&node.with_value(ptr)) {
                    base = Some(state);
                    break;
                }
                pending.push((ptr, owner));
            }
            top = ancestor;
        }
        let mut state = match base {
            Some(it) => it,
            None => self.above_file(cx, node.file_id, top),
        };
        for (ptr, owner) in pending.into_iter().rev() {
            apply(cx, node.with_value(owner), ptr, &mut state, Copied::All);
            self.owners.borrow_mut().insert(node.with_value(ptr), state);
        }
        state
    }

    /// What is set above the root `top` of `file`.
    fn above_file<'db>(&self, cx: &Cx<'_, 'db>, file: HirFileId, top: ast::Node<'db>) -> State {
        if let Some(&state) = self.files.borrow().get(&file) {
            return state;
        }
        let state = match above(cx, file, top) {
            Some(node) => self.state(cx, node),
            None => State::default(),
        };
        self.files.borrow_mut().insert(file, state);
        state
    }
}

fn default_level() -> Level {
    LintId::DEAD_CODE.lint().default_level
}

/// Where the levels at the root `top` of `file` come from: the macro call it was expanded from
/// (for a derive's or an attribute's, what is around the item), or the declaration of the
/// module whose file it is.
fn above<'db>(cx: &Cx<'_, 'db>, file: HirFileId, top: ast::Node<'db>) -> Option<InFile<ast::Node<'db>>> {
    if let Some(call) = file.macro_file() {
        let node = call.call_node_view(cx.db);
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
    let module = cx.vsema.file_to_module_def(file.file_id(cx.db))?;
    if module.is_crate_root(cx.db) {
        return None;
    }
    let declaration = module.declaration_source_view(cx.db)?;
    Some(declaration.map(|it| it.syntax()))
}

/// Which of an owner's lint attributes count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Copied {
    All,
    /// Those a builtin derive copies onto its impl: all but `expect`.
    ToDerives,
}

/// The lint attributes of `owner` (whose node is `ptr`), in order, as they set `dead_code` and
/// `warnings`.
fn apply<'db>(
    cx: &Cx<'_, 'db>,
    owner: InFile<ast::AnyHasAttrs<'db>>,
    ptr: SyntaxNodePtr,
    state: &mut State,
    copied: Copied,
) {
    let file = owner.file_id.original_file(cx.db).file_id(cx.db);
    let ptr = owner.with_value(ptr);
    for (index, (attr, name)) in cx.vsema.lint_attrs(file, cx.krate, owner.value).enumerate() {
        let level = match attr {
            hir::LintAttr::Allow => Level::Allow,
            hir::LintAttr::Expect if copied == Copied::ToDerives => continue,
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
        Named::Lint(lint) => lint.id() == LintId::DEAD_CODE,
        Named::Group(group) => group.has(LintId::DEAD_CODE),
        Named::Renamed(new) => new != name && names_dead_code(new),
        _ => false,
    }
}
