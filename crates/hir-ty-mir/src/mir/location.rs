//! Where code is written, as rustc counts it: the line, column and file that `line!()`,
//! `column!()` and `file!()` expand to, which rust-analyzer expands to placeholders, and that
//! `#[track_caller]` and panics report. The evaluator of constants and the code generator
//! both read them.

use salsa::Database as _;
use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, ItemContainerId, Lookup, attrs::AttrFlags, expr_store::Body,
    hir::ExprId, signatures::FunctionSignature,
};
use hir_expand::{
    FileRange, HirFileId, InFile, MacroDefKind,
    builtin::{BuiltinFnLikeExpander, EagerExpander},
};
use rustc_hash::FxHashMap;
use syntax::{AstNode, AstToken, SyntaxElement, SyntaxKind, SyntaxNode, TextRange, TextSize, ast};

use crate::{
    InferenceResult,
    db::HirDatabase,
    mir::{MirSpan, is_builtin_index},
};

/// The range of an item or a macro call without the attributes and comments on top of it,
/// which rustc's span of it leaves out.
pub fn written_range(node: &SyntaxNode) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| {
            !matches!(it.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
                && !it.kind().is_trivia()
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// Whether lowering `body` met a call of `line!`, `column!`, `file!` or `concat!`: only then
/// can a literal of it be what one of them expanded to, and only then is its source map read.
fn calls_location_macros(db: &Db, body: &Body) -> bool {
    body.macro_calls().iter().any(|call| {
        matches!(
            call.loc(db).def.kind,
            MacroDefKind::BuiltIn(
                _,
                BuiltinFnLikeExpander::Line
                    | BuiltinFnLikeExpander::Column
                    | BuiltinFnLikeExpander::File
            ) | MacroDefKind::BuiltInEager(_, EagerExpander::Concat)
        )
    })
}

/// The literals of a body that `line!()`, `column!()` or `file!()` expanded to. The source map
/// changes with every edit above the body, and this keeps code without such literals from
/// changing with it.
#[salsa::tracked(returns(ref))]
pub fn location_macros<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
) -> FxHashMap<ExprId, (BuiltinFnLikeExpander, HirFileId)> {
    let body = Body::of(db, owner);
    if !calls_location_macros(db, body) {
        return FxHashMap::default();
    }
    let source_map = Body::unmoved_source_map(db, owner);
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let MacroDefKind::BuiltIn(_, expander) = file_id.macro_file()?.loc(db).def.kind else {
                return None;
            };
            matches!(
                expander,
                BuiltinFnLikeExpander::Line
                    | BuiltinFnLikeExpander::Column
                    | BuiltinFnLikeExpander::File
            )
            .then_some((id, (expander, file_id)))
        })
        // alloc: the query's value, of a body that called a location macro
        .collect()
}

/// The literals of a body that `concat!` made of arguments with `line!()`, `column!()` or
/// `file!()` among them, each with the expansion it is in.
#[salsa::tracked(returns(ref))]
pub fn located_concats<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
) -> FxHashMap<ExprId, HirFileId> {
    let body = Body::of(db, owner);
    if !calls_location_macros(db, body) {
        return FxHashMap::default();
    }
    let source_map = Body::unmoved_source_map(db, owner);
    body.exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Literal(_)))
        .filter_map(|(id, _)| {
            let file_id = source_map.expr_syntax(id).ok()?.file_id;
            let loc = file_id.macro_file()?.loc(db);
            let concat =
                matches!(loc.def.kind, MacroDefKind::BuiltInEager(_, EagerExpander::Concat));
            (concat && names_location(&loc.to_node(db).value)).then_some((id, file_id))
        })
        // alloc: the query's value, of a body that called a location macro
        .collect()
}

/// What `line!()`, `column!()`, `file!()` or a `concat!` of them is where it is written.
pub enum Placed {
    Number(u32),
    Text(String),
}

// dyn: the build's rule, told once a build and called once per file (`file_source_path`)
type SourcePaths = dyn Fn(&Db, base_db::FileId) -> Option<String> + Send + Sync;

/// How a build names files, told by [`name_sources`]. Two rules with the same `key` name every
/// file the same: the key is all the rule depends on besides what it reads of the database.
#[derive(Clone)]
pub struct NamingRule {
    key: Box<str>,
    paths: std::sync::Arc<SourcePaths>,
}

impl std::fmt::Debug for NamingRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("NamingRule").field(&self.key).finish()
    }
}

/// What the build calls each file, for a constant with `file!()` in its value: the evaluator
/// has no build to ask, and a constant's value depends on it, so it is an input.
#[salsa::input(singleton, debug)]
pub struct SourceNames {
    #[returns(ref)]
    pub rule: NamingRule,
}

/// Tells the evaluator of constants what the build calls each file: `Symbols::source_path` in
/// blaze. `key` names everything `paths` depends on besides the database (the checkout and its
/// base, say); telling the same key again changes nothing, so no constant is evaluated again.
pub fn name_sources(
    db: &mut Db,
    key: &str,
    paths: impl Fn(&Db, base_db::FileId) -> Option<String> + Send + Sync + 'static,
) {
    // alloc: once per naming rule a build tells
    let rule = NamingRule { key: key.into(), paths: std::sync::Arc::new(paths) };
    match SourceNames::try_get(db) {
        Some(names) if names.rule(db).key == rule.key => {}
        Some(names) => {
            names.set_rule(db).with_durability(salsa::Durability::HIGH).to(rule);
        }
        None => {
            _ = SourceNames::builder(rule).durability(salsa::Durability::HIGH).new(db);
            // Reading a singleton that does not exist yet is not tracked: what read it before
            // reported an untracked read (`file_source_path`), which a new revision makes stale.
            db.synthetic_write(salsa::Durability::LOW);
        }
    }
}

/// What the build calls `file`, as `file!()` and `Location::file` say: none when the
/// evaluator was not told ([`name_sources`]).
pub fn source_path<'db>(db: &'db Db, file: base_db::FileId) -> Option<&'db str> {
    file_source_path(db, db.file_text(file))
}

#[salsa::tracked(returns(as_deref))]
fn file_source_path<'db>(db: &'db Db, file: base_db::FileText) -> Option<String> {
    let Some(names) = SourceNames::try_get(db) else {
        db.report_untracked_read();
        return None;
    };
    (names.rule(db).paths)(db, file.file_id(db))
}

/// The value of the literal `expr` of `owner` when it is what `line!()`, `column!()`,
/// `file!()` or a `concat!` of them expanded to: rust-analyzer expands them to placeholders,
/// and a constant is evaluated from what it expanded them to.
pub fn placed(db: &Db, owner: DefWithBodyId, expr: ExprId) -> Option<Placed> {
    let path = |file| source_path(db, file);
    if let Some(file_id) = located_concats(db, owner).get(&expr) {
        use std::fmt::Write;
        let call = file_id.macro_file()?.loc(db).to_node(db);
        // alloc: the value of the `concat!`, made only where one of a place is evaluated
        let mut text = String::new();
        for part in concat_parts(&call.value)? {
            match part {
                ConcatPart::Text(part) => text.push_str(&part),
                ConcatPart::Location(expander, range) => {
                    let (file, line, col) = macro_location_of(db, call.file_id, range)?;
                    match expander {
                        BuiltinFnLikeExpander::Line => _ = write!(text, "{line}"),
                        BuiltinFnLikeExpander::Column => _ = write!(text, "{col}"),
                        _ => text.push_str(path(file)?),
                    }
                }
            }
        }
        return Some(Placed::Text(text));
    }
    let &(expander, file_id) = location_macros(db, owner).get(&expr)?;
    let site = written_call(db, file_id)?;
    let range = InFile::new(site.file_id, written_range(&site.value));
    let FileRange { file_id, range } = range.original_node_file_range_rooted(db);
    let (file, line, col) = macro_location_of(db, file_id.into(), range)?;
    Some(match expander {
        BuiltinFnLikeExpander::Line => Placed::Number(line),
        BuiltinFnLikeExpander::Column => Placed::Number(col),
        // alloc: the value of `file!()`, made only where one is evaluated
        _ => Placed::Text(path(file)?.to_owned()),
    })
}

/// The macro of `name` that expands to a place in the source.
fn location_expander(name: &str) -> Option<BuiltinFnLikeExpander> {
    match name {
        "line" => Some(BuiltinFnLikeExpander::Line),
        "column" => Some(BuiltinFnLikeExpander::Column),
        "file" => Some(BuiltinFnLikeExpander::File),
        _ => None,
    }
}

/// Whether a macro call is passed a call of `line!`, `column!` or `file!`.
fn names_location(call: &SyntaxNode) -> bool {
    call.descendants_with_tokens().filter_map(|it| it.into_token()).any(|token| {
        token.kind() == SyntaxKind::IDENT
            && location_expander(token.text()).is_some()
            && token
                .next_token()
                .and_then(|next| syntax::algo::skip_trivia_token(next, syntax::Direction::Next))
                .is_some_and(|next| next.kind() == SyntaxKind::BANG)
    })
}

/// An argument of a `concat!`.
pub enum ConcatPart {
    Text(String),
    /// A call of `line!`, `column!` or `file!`, and where it is.
    Location(BuiltinFnLikeExpander, TextRange),
}

/// The arguments of a `concat!` call, when each is a literal or a call of `line!`, `column!`
/// or `file!`.
pub fn concat_parts(call: &SyntaxNode) -> Option<Vec<ConcatPart>> {
    // clone: a syntax node handle (a refcount)
    let tree = ast::MacroCall::cast(call.clone())?.token_tree()?;
    let mut inside: Vec<SyntaxElement> =
        // alloc: the call's arguments, read only where a `concat!` of a place is evaluated
        tree.syntax().children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    inside.pop()?;
    let inside = inside.get(1..)?;
    inside
        .split(|it| it.kind() == SyntaxKind::COMMA)
        .filter(|argument| !argument.is_empty())
        .map(concat_part)
        // alloc: the parts, made only where a `concat!` of a place is evaluated
        .collect()
}

fn concat_part(argument: &[SyntaxElement]) -> Option<ConcatPart> {
    if let [SyntaxElement::Token(token)] = argument {
        let text = match token.kind() {
            // clone: a token handle; alloc: the part's text
            SyntaxKind::STRING => ast::String::cast(token.clone())?.value().ok()?.into_owned(),
            SyntaxKind::INT_NUMBER => {
                // clone: a token handle; alloc: the part's text
                ast::IntNumber::cast(token.clone())?.value().ok()?.to_string()
            }
            // clone: a token handle; alloc: the part's text
            SyntaxKind::CHAR => ast::Char::cast(token.clone())?.value().ok()?.to_string(),
            // alloc: the part's text
            SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW => token.text().to_owned(),
            _ => return None,
        };
        return Some(ConcatPart::Text(text));
    }
    let [.., SyntaxElement::Token(name), bang, SyntaxElement::Node(arguments)] = argument else {
        return None;
    };
    let expander = location_expander(name.text())?;
    let empty = arguments.children_with_tokens().filter(|it| !it.kind().is_trivia()).count() == 2;
    (bang.kind() == SyntaxKind::BANG && empty).then(|| {
        let range = argument[0].text_range().cover(arguments.text_range());
        ConcatPart::Location(expander, range)
    })
}

/// The macro call an expansion comes from that is itself written out, in the source or in
/// what a macro was passed, and not by another macro.
pub fn written_call(db: &Db, file_id: HirFileId) -> Option<InFile<SyntaxNode>> {
    let mut call = file_id.macro_file()?.loc(db).to_node(db);
    while let Some(outer) = call.file_id.macro_file() {
        let start = TextRange::empty(call.value.text_range().start());
        if call.file_id.span_map(db).span_for_range(start).ctx.is_root() {
            break;
        }
        call = outer.loc(db).to_node(db);
    }
    Some(call)
}

/// File, line and column of where `range` of `file_id` begins as rustc reports it (the column
/// in display width, as [`line_and_column`] gives it).
pub fn location_of(
    db: &Db,
    file_id: HirFileId,
    range: TextRange,
) -> Option<(base_db::FileId, u32, u32)> {
    line_and_column(db, reported_file_range(db, file_id, range))
}

/// [`location_of`] as `line!()` and `column!()` expand to it: the column is rustc's
/// `lookup_char_pos(..).col + 1`, in chars (a tab is one), not the display width.
pub fn macro_location_of(
    db: &Db,
    file_id: HirFileId,
    range: TextRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let FileRange { file_id, range } = reported_file_range(db, file_id, range);
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let place = base_db::line_index(db, text).place(range.start())?;
    Some((file_id, place.line(), place.char_column(text.text(db))?))
}

/// Where in a file rustc reports `range` of `file_id`: what a macro writes itself from where
/// the outermost macro is called, and what it is passed at its own place.
fn reported_file_range(db: &Db, file_id: HirFileId, range: TextRange) -> FileRange {
    let own_tokens = file_id.macro_file().is_some()
        && !file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root();
    match written_call(db, file_id) {
        Some(call) if own_tokens => InFile::new(call.file_id, written_range(&call.value))
            .original_node_file_range_rooted(db),
        _ => InFile::new(file_id, range).original_node_file_range_rooted(db),
    }
}

/// File, line and column of where a range of a file begins: rustc's line, and its
/// `col_display + 1` (what `Location::caller` and debug info are given).
pub fn line_and_column(
    db: &Db,
    FileRange { file_id, range }: FileRange,
) -> Option<(base_db::FileId, u32, u32)> {
    let file_id = file_id.file_id(db);
    let text = db.file_text(file_id);
    let place = base_db::line_index(db, text).place(range.start())?;
    Some((file_id, place.line(), place.display_column(text.text(db))?))
}

/// File, line and column of a place in a body, as rustc reports them, found from where the
/// body is in its file: the code generator also finds them from the body alone.
#[salsa::tracked]
pub fn source_location_in_file<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(base_db::FileId, u32, u32)> {
    let (file_id, range) = source_range(db, owner, span)?;
    location_of(db, file_id, range)
}

/// The file a place in a body is written in, which may be what a macro expands to, and
/// where in it rustc has the place begin.
pub fn source_range<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
    span: MirSpan,
) -> Option<(HirFileId, TextRange)> {
    let source_map = &Body::with_source_map(db, owner).1;
    let source = match span {
        MirSpan::ExprId(expr) => source_map.expr_syntax(expr).ok()?,
        // The block an `async fn` is taken to wrap its body in is not written anywhere, and
        // ends where the body does.
        MirSpan::ExprEnd(expr) => {
            match (source_map.expr_syntax(expr), Body::of(db, owner).expr(expr)) {
                (Ok(written), _) => written,
                (Err(_), hir_def::hir::Expr::Block { tail: Some(tail), .. }) => {
                    source_map.expr_syntax(tail).ok()?
                }
                (Err(_), _) => return None,
            }
        }
        MirSpan::PatId(pat) => source_map.pat_syntax(pat).ok()?,
        MirSpan::BindingId(_) | MirSpan::SelfParam | MirSpan::Unknown => return None,
    };
    if let MirSpan::ExprEnd(_) = span {
        // The brace that closes a block, which is where rustc has a scope left.
        let range = source.value.syntax_node_ptr().text_range();
        let last = range.end().checked_sub(TextSize::new(1)).filter(|it| *it >= range.start());
        return Some((source.file_id, TextRange::new(last?, range.end())));
    }
    let range = match span {
        MirSpan::ExprId(expr) => {
            let root = source.file_id.parse_or_expand(db);
            let node = source.value.syntax_node_ptr().to_node(&root);
            reported_range(db, owner, expr, source.file_id, &node)?
        }
        _ => source.value.syntax_node_ptr().text_range(),
    };
    Some((source.file_id, range))
}

/// The part of an expression that rustc's span of it covers where it panics: a method call
/// from the method's name on, an index through `Index` from its bracket on, and any other
/// expression together with the parentheses it is written in.
pub fn reported_range(
    db: &Db,
    owner: DefWithBodyId,
    expr: ExprId,
    file_id: HirFileId,
    node: &SyntaxNode,
) -> Option<TextRange> {
    let end = node.text_range().end();
    // clone: a syntax node handle (a refcount)
    if let Some(call) = ast::MethodCallExpr::cast(node.clone()) {
        return Some(TextRange::new(call.name_ref()?.syntax().text_range().start(), end));
    }
    // clone: a syntax node handle (a refcount)
    if let Some(written) = ast::IndexExpr::cast(node.clone())
        && let hir_def::hir::Expr::Index { base, index } = Body::of(db, owner).expr(expr)
        && !is_builtin_index(InferenceResult::of(db, owner), base, index)
    {
        return Some(TextRange::new(written.l_brack_token()?.text_range().start(), end));
    }
    Some(parenthesized(db, file_id, node))
}

/// The range of an expression with the parentheses around it. rustc's span of an expression
/// includes them when both are written in one place, and not when a macro put the
/// parentheses around what it was passed.
fn parenthesized(db: &Db, file_id: HirFileId, node: &SyntaxNode) -> TextRange {
    let passed_in = |range: TextRange| {
        file_id.macro_file().is_none()
            || file_id.span_map(db).span_for_range(TextRange::empty(range.start())).ctx.is_root()
    };
    let inner = passed_in(node.text_range());
    node.ancestors()
        .skip(1)
        .take_while(|it| it.kind() == SyntaxKind::PAREN_EXPR && passed_in(it.text_range()) == inner)
        .last()
        .map_or(node.text_range(), |it| it.text_range())
}

/// Whether calls to `func` pass a `&Location` after the declared arguments. A method of a trait
/// impl inherits the attribute from the trait's declaration.
pub fn track_caller(db: &Db, func: FunctionId) -> bool {
    let has =
        |func: FunctionId| AttrFlags::query(db, func.into()).contains(AttrFlags::TRACK_CALLER);
    if has(func) {
        return true;
    }
    let ItemContainerId::ImplId(impl_id) = func.lookup(db).container else {
        return false;
    };
    let Some(trait_ref) = db.impl_trait(impl_id) else {
        return false;
    };
    let name = &FunctionSignature::of(db, func).name;
    trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name).is_some_and(has)
}
