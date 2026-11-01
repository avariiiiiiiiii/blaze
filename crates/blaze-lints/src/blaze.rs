//! blaze's own lints, `blaze::`: what costs in hot code, the rules `tools/v2/hot_lint.py`
//! checks by pattern, from types and resolutions instead
//! ([docs/v2/plan/lanes/lints/blaze.md](../../../docs/v2/plan/lanes/lints/blaze.md)).
//!
//! The walk calls [`check_node`] on every node. A path type is checked where it is written
//! (`std_hashmap`'s type form); the calls of a body are checked once, from its root, over the
//! body's HIR, so each sees what inference resolved: the callee and its `Self`, the value's
//! type, the allocator reach of the instance called. A finding is placed at its expression, or
//! at the outermost macro call that wrote it (`format!`, `vec!`), and dropped where its
//! exemption comment is.

mod alloc;
mod clone;
mod dispatch;
mod exempt;
mod fiber;
mod hashmap;
mod parallel;
#[cfg(test)]
mod tests;

use base_db::Db;
use hir_def::{DefWithBodyId, FunctionId, HasModule as _, TraitId, hir::Expr};
use hir_ty::{ParamEnvAndCrate, db::HirDatabase as _, next_solver::DbInterner};
use intern::sym;
use syntax::{NodeOrToken, TextRange, view as ast};

use ast::AstNode as _;

use crate::check::Cx;

macro_rules! docs {
    () => {
        "https://github.com/indexable-inc/blaze/blob/main/docs/v2/plan/lanes/lints/blaze.md"
    };
}

macro_rules! parallel_docs {
    () => {
        "https://github.com/indexable-inc/blaze/blob/main/docs/v2/plan/lanes/lints/parallel.md"
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Lint {
    HotClone,
    HotAlloc,
    HotDyn,
    StdHashmap,
    Parallelizable,
    SerialDependence,
    FiberBoundary,
    HotAllocCall,
}

/// The group `blaze::hot`: every lint of hot code, SipHash maps included as `hot_lint.py`
/// checks them.
pub(crate) const HOT: &[Lint] =
    &[Lint::HotClone, Lint::HotAlloc, Lint::HotAllocCall, Lint::HotDyn, Lint::StdHashmap];

impl Lint {
    /// In discriminant order.
    pub(crate) const ALL: &[Lint] = &[
        Lint::HotClone,
        Lint::HotAlloc,
        Lint::HotDyn,
        Lint::StdHashmap,
        Lint::Parallelizable,
        Lint::SerialDependence,
        Lint::FiberBoundary,
        Lint::HotAllocCall,
    ];

    /// The name after `blaze::`.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Lint::HotClone => "hot_clone",
            Lint::HotAlloc => "hot_alloc",
            Lint::HotDyn => "hot_dyn",
            Lint::StdHashmap => "std_hashmap",
            Lint::Parallelizable => "parallelizable",
            Lint::SerialDependence => "serial_dependence",
            Lint::FiberBoundary => "fiber_boundary",
            Lint::HotAllocCall => "hot_alloc_call",
        }
    }

    /// The lint's bit in [`crate::lints::On`], above every clippy lint's.
    pub(crate) const fn bit(self) -> crate::lints::On {
        1 << (48 + self as u32)
    }

    /// Where the lint is described, for the help line of each finding.
    pub(crate) const fn docs_url(self) -> &'static str {
        match self {
            Lint::HotClone => concat!(docs!(), "#hot_clone"),
            Lint::HotAlloc => concat!(docs!(), "#hot_alloc"),
            Lint::HotAllocCall => concat!(docs!(), "#hot_alloc_call"),
            Lint::HotDyn => concat!(docs!(), "#hot_dyn"),
            Lint::StdHashmap => concat!(docs!(), "#std_hashmap"),
            Lint::Parallelizable => concat!(parallel_docs!(), "#parallelizable"),
            Lint::SerialDependence => concat!(parallel_docs!(), "#serial_dependence"),
            Lint::FiberBoundary => {
                "https://github.com/indexable-inc/blaze/blob/main/docs/v2/plan/lanes/may-suspend/fiber-boundary.md"
            }
        }
    }

    /// The help the finding ends with, past the lint's own.
    const fn help(self) -> &'static str {
        match self {
            Lint::HotClone => "borrow it, copy an ID, or make the type not `Clone`",
            Lint::HotAlloc => {
                "write into a buffer the caller keeps, an arena, or intern it; format only at the edge"
            }
            Lint::HotAllocCall => {
                "keep the allocation out of what the hot path calls, or say why it is needed there with `// alloc: <why>`"
            }
            Lint::HotDyn => "use a concrete type, a generic, or a `match` on a tag",
            Lint::StdHashmap => "use `rustc_hash::FxHashMap` (or `FxHashSet`), or index by content hash",
            Lint::Parallelizable => "run the iterations with rayon's parallel iterators or blaze's pool",
            Lint::SerialDependence => {
                "make the dependence a reduction to run the iterations in parallel, or keep it serial"
            }
            Lint::FiberBoundary => {
                "go through `blaze_graph::sched`'s own API (`sched::wait`, `sched::carry`, ...): fibers are the scheduler's alone"
            }
        }
    }
}

/// Every bit of [`Lint::ALL`].
const ANY: crate::lints::On = {
    let mut bits = 0;
    let mut i = 0;
    while i < Lint::ALL.len() {
        bits |= Lint::ALL[i].bit();
        i += 1;
    }
    bits
};

pub(crate) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_any(ANY) {
        return;
    }
    if let Some(ty) = ast::PathType::cast(node)
        && cx.on_blaze(Lint::StdHashmap)
        && let Some(message) = hashmap::check_type(cx, ty)
        && let Some(range) = cx.range(node)
        && !exempt::exempt(cx.text(), range, Lint::StdHashmap)
    {
        let span = cx.span(range);
        cx.blaze(Lint::StdHashmap, node, span, message)
            .help(Lint::StdHashmap.help())
            .help(format!("for further information visit {}", Lint::StdHashmap.docs_url()));
    }
    // An `extern crate` is placed at the name it writes: the item's own range starts at the
    // comments the parser attaches to it.
    if cx.on_blaze(Lint::FiberBoundary)
        && let Some(message) = fiber::check(cx, node)
        && let Some(range) = cx.range(
            ast::ExternCrate::cast(node).and_then(|it| it.name_ref()).map_or(node, |it| it.syntax()),
        )
    {
        let span = cx.span(range);
        cx.blaze(Lint::FiberBoundary, node, span, message)
            .help(Lint::FiberBoundary.help())
            .help(format!("for further information visit {}", Lint::FiberBoundary.docs_url()));
    }
    if node == cx.root() && cx.in_file() {
        check_body(cx);
    }
}

/// What a body's walk knows: the database, the body's environment, and the items its lints
/// compare against.
struct Known<'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    env: ParamEnvAndCrate<'db>,
    target: hir::DisplayTarget,
    /// `Clone` and its `clone`.
    clone: Option<(TraitId, FunctionId)>,
    std: hashmap::Std,
    alloc: alloc::Std,
}

impl<'db> Known<'db> {
    fn new(cx: &Cx<'db, '_>, def: DefWithBodyId) -> Known<'db> {
        let db = cx.db();
        let clone = cx.lang_trait(hir::LangItem::Clone).and_then(|it| {
            let id = TraitId::from(it);
            let clone_fn = id.trait_items(db).method_by_name(&hir::Name::new_symbol_root(sym::clone));
            Some((id, clone_fn?))
        });
        Known {
            db,
            interner: DbInterner::new_no_crate(db),
            env: ParamEnvAndCrate {
                param_env: db.trait_environment(def.generic_def(db)),
                krate: def.krate(db),
            },
            target: cx.krate().to_display_target(db),
            clone,
            std: hashmap::Std::new(cx),
            alloc: alloc::Std::new(cx),
        }
    }
}

/// One finding of a body, before it is reported.
struct Finding {
    lint: Lint,
    range: TextRange,
    message: String,
    note: Option<String>,
}

fn check_body(cx: &mut Cx<'_, '_>) {
    parallel::check(cx);
    let on = |lint| cx.on_blaze(lint);
    let (on_clone, on_alloc, on_dyn, on_map) =
        (on(Lint::HotClone), on(Lint::HotAlloc), on(Lint::HotDyn), on(Lint::StdHashmap));
    let on_alloc_call = on(Lint::HotAllocCall);
    if !(on_clone || on_alloc || on_alloc_call || on_dyn || on_map) {
        return;
    }
    let mut found: Vec<Finding> = Vec::new();
    {
        let Some(analysis) = cx.analysis() else { return };
        let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
        let k = Known::new(cx, def);
        let db = k.db;
        let body = analysis.body();
        let infer = analysis.infer();
        let text = cx.text();
        let mut push = |lint, range, message, note| {
            if exempt::exempt(text, range, lint)
                || found.iter().any(|it: &Finding| it.lint == lint && it.range == range)
            {
                return;
            }
            found.push(Finding { lint, range, message, note });
        };
        for (id, expr) in body.exprs() {
            let (first, turbofish) = match expr {
                Expr::MethodCall { receiver, generic_args, .. } => {
                    (Some(receiver), generic_args.is_some())
                }
                Expr::Call { args, .. } => (args.first().copied(), false),
                _ => continue,
            };
            let Some(range) = analysis.expr_range(id) else { continue };
            let Some((callee, args)) = alloc::written_callee(infer, body, id) else { continue };
            let is_clone = clone::is_clone(&k, callee);
            if on_clone
                && is_clone
                && let Some(first) = first
            {
                let receiver = infer
                    .type_of_expr_with_adjust(first)
                    .unwrap_or_else(|| infer.expr_ty(first))
                    .strip_reference();
                if let Some((message, note)) = clone::finding(&k, &analysis.ty(receiver)) {
                    push(Lint::HotClone, range, message, note);
                }
            }
            if on_dyn && let Some(message) = dispatch::finding(&k, callee, args) {
                push(Lint::HotDyn, range, message, None);
            }
            let reaches = |through_workspace| {
                !is_clone
                    && alloc::instance_reaches(
                        db,
                        k.env,
                        alloc::resolve(db, k.env, callee, args),
                        through_workspace,
                    )
                    && !alloc::grows_kept_buffer(db, body, id, callee)
            };
            let call = || match macro_name(cx.snippet(range)) {
                Some(called) => format!("this `{called}!`"),
                None => format!("this call to `{}`", hir::Function::from(callee).name(db).as_str()),
            };
            if on_alloc && reaches(false) {
                push(Lint::HotAlloc, range, format!("{} reaches the global allocator", call()), None);
            } else if on_alloc_call && reaches(true) && !reaches(false) {
                let message = format!("{} reaches the global allocator through the workspace's own code", call());
                let note = "`hot_alloc` finds the allocation in the function of the workspace that \
                            makes it; a hot function promises that nothing it calls allocates";
                push(Lint::HotAllocCall, range, message, Some(note.to_owned()));
            } else if on_alloc
                && !is_clone
                && let Some(made) = alloc::makes_global(&k, callee, infer.expr_ty(id))
            {
                let made = hir::Adt::from(made).name(db);
                let message = format!("{} makes a `{}` of the global allocator", call(), made.as_str());
                let note = "it allocates from the global heap once it grows: name its allocator \
                            (`Vec::new_in(alloc)`), grow a buffer the caller keeps, or say why it \
                            needs the global heap with `// alloc: <why>`";
                push(Lint::HotAlloc, range, message, Some(note.to_owned()));
            }
            if on_map
                && let Some(message) =
                    hashmap::check_construct(&k, callee, infer.expr_ty(id), turbofish)
            {
                push(Lint::StdHashmap, range, message, None);
            }
        }
    }
    let root = cx.root();
    for finding in found {
        let at = match root.text_range().contains_range(finding.range) {
            true => match root.covering_element(finding.range) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => it.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(finding.range);
        let diagnostic = cx.blaze(finding.lint, at, span, finding.message);
        if let Some(note) = finding.note {
            diagnostic.note(note);
        }
        diagnostic
            .help(finding.lint.help())
            .help(format!("for further information visit {}", finding.lint.docs_url()));
    }
}

/// The name of the macro `snippet` calls, when it is a macro call (`format!(..)`, `vec![..]`).
fn macro_name(snippet: &str) -> Option<&str> {
    let end = snippet
        .find(|it: char| !(it.is_alphanumeric() || it == '_' || it == ':'))
        .filter(|&it| it > 0)?;
    let called = &snippet[..end];
    snippet[end..].starts_with('!').then(|| called.rsplit("::").next().unwrap_or(called))
}
