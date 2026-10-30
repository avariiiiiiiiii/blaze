//! What a diagnostic is lowered with: the crate it is of, the names rustc gives files, spans
//! as rustc gives them (in the file a macro was called in, or in the macro's own definition
//! when it is the crate's), the text of what a span covers, rustc's printing of types and
//! paths, and the keys of the facts a diagnostic read. It also holds what every pass of one
//! check shares, each made the first time a pass asks: the walk of the crate's syntax, the
//! HIR lint levels, and the set of lints that may fire.

use std::{
    cell::{OnceCell, RefCell},
    collections::HashMap,
    sync::LazyLock,
};

use base_db::Db;
use blaze_items::{Fact, ItemId, ItemKey, NameHash, SimplifiedType};
use blaze_mem::Frame;
use hir::{DisplayTarget, InFile, MacroCallId, Semantics};
use hir_expand::{MacroCallKind, MacroDefKind};
use rustc_hash::FxHashMap;
use span::{Edition, FileId};
use syntax::{
    AstNode, AstPtr, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasAttrs as _},
};

use crate::{
    lints::{
        ItemWalk, LevelMap,
        registry::{LintId, LintSet},
        syntax::Walk,
    },
    model::{Expansion, MacroKind, Span},
};

/// The names files are shown by: the front end's path of each file, and the root of the
/// workspace they are named from.
#[derive(Clone, Copy)]
pub struct Files<'a> {
    paths: &'a HashMap<FileId, String>,
    root: &'a str,
}

impl<'a> Files<'a> {
    /// `paths` named from `root`; a `root` of `""` keeps each relative path whole, as the tests'
    /// paths, already names, are.
    pub fn new(paths: &'a HashMap<FileId, String>, root: &'a str) -> Files<'a> {
        Files { paths, root }
    }

    /// The name rustc gives `file` in a diagnostic, by [`Files::relative`]; a file the front end
    /// has no path for is `<file N>`.
    pub fn name(&self, file: FileId) -> String {
        match self.paths.get(&file) {
            Some(path) => Files::relative(path, self.root).to_owned(),
            None => format!("<file {}>", file.index()),
        }
    }

    /// The name rustc gives `path`: from `root` when it is below it, as cargo runs rustc there,
    /// else the whole path, as `root` itself keeps it.
    pub fn relative<'p>(path: &'p str, root: &str) -> &'p str {
        let root = root.trim_end_matches('/');
        match path.strip_prefix(root).and_then(|rest| rest.strip_prefix('/')) {
            Some(rest) if !rest.is_empty() => rest,
            _ => path,
        }
    }
}

/// A diagnostic of a crate being lowered.
pub struct Cx<'a, 'db> {
    pub sema: &'a Semantics<'db>,
    pub db: &'db Db,
    pub krate: hir::Crate,
    pub edition: Edition,
    pub display_target: DisplayTarget,
    files: Files<'a>,
    /// The check's task frame: what a pass builds for the crate (numberings, worklists,
    /// bitsets) is allocated here, `Vec<T, &Frame>`, and freed when the check returns.
    frame: &'a Frame,
    /// The lint flags the crate is checked with, for a lowering whose words depend on a
    /// lint's level; none is rustc's defaults.
    lints: Option<&'a crate::lints::CommandLine>,
    /// The file each name shown stands for, as spans were made.
    named: RefCell<FxHashMap<String, FileId>>,
    /// The paths types are printed by, made the first time one is printed.
    paths: OnceCell<crate::print::Paths<'a>>,
    /// The crate as rustc's early lint pass walks it, made the first time a pass asks.
    walk: OnceCell<Walk<'db>>,
    /// The crate's items as rustc's item passes visit them, made the first time a pass asks.
    items: OnceCell<ItemWalk>,
    /// The HIR lint levels, made the first time a level is asked and filled in as levels are.
    levels: RefCell<Option<LevelMap<'db>>>,
    /// The lints that may fire anywhere in the crate, made the first time a pass asks.
    may_fire: OnceCell<LintSet>,
}

impl<'a, 'db> Cx<'a, 'db> {
    pub fn new(
        sema: &'a Semantics<'db>,
        krate: hir::Crate,
        files: Files<'a>,
        frame: &'a Frame,
    ) -> Cx<'a, 'db> {
        let db = sema.db;
        Cx {
            sema,
            db,
            krate,
            edition: krate.edition(db),
            display_target: krate.to_display_target(db),
            files,
            frame,
            lints: None,
            named: RefCell::new(FxHashMap::default()),
            paths: OnceCell::new(),
            walk: OnceCell::new(),
            items: OnceCell::new(),
            levels: RefCell::new(None),
            may_fire: OnceCell::new(),
        }
    }

    /// The check's task frame, for the crate-sized tables a pass builds.
    pub fn frame(&self) -> &'a Frame {
        self.frame
    }

    /// The names the check's files are shown by.
    pub fn files(&self) -> Files<'a> {
        self.files
    }

    /// What rustc's printing of paths and the import suggestions read: the crate's visible
    /// names and its declarations by name, built in the check's frame when first read.
    pub(crate) fn paths(&self) -> &crate::print::Paths<'a> {
        self.paths.get_or_init(|| crate::print::Paths::new(self))
    }

    /// The context checking with the lint flags `lints`, before anything was asked of it.
    pub fn with_lints(self, lints: &'a crate::lints::CommandLine) -> Cx<'a, 'db> {
        assert!(
            self.walk.get().is_none() && self.levels.borrow().is_none() && self.may_fire.get().is_none(),
            "the lint flags are given before a level is asked"
        );
        Cx { lints: Some(lints), ..self }
    }

    /// The lint flags the crate is checked with: rustc's defaults when none were given.
    pub fn lint_flags(&self) -> &'a crate::lints::CommandLine {
        static DEFAULT: LazyLock<crate::lints::CommandLine> = LazyLock::new(crate::lints::CommandLine::default);
        self.lints.unwrap_or(&*DEFAULT)
    }

    /// The crate as rustc's early lint pass walks it: made once per check, for every pass that
    /// walks the crate's syntax.
    pub(crate) fn walk(&self) -> &Walk<'db> {
        self.walk.get_or_init(|| Walk::of(self))
    }

    /// The crate's items in the order rustc's item passes visit them: made once per check, for
    /// the item lints before the abort and after it.
    pub(crate) fn items(&self) -> &ItemWalk {
        self.items.get_or_init(|| ItemWalk::of(self))
    }

    /// The level `lint` has at `at`, from the attributes there and around it and the lint
    /// flags, as rustc's HIR levels decide it (`unsafe_op_in_unsafe_fn`'s level changes E0133's
    /// words). The levels are rustc's `ShallowLintLevelMap`, kept for the check: asked again at
    /// a node, or at one whose set is known, it is one hash probe, with nothing allocated.
    pub fn lint_level(&self, lint: LintId, at: InFile<SyntaxNodePtr>) -> crate::lints::Level {
        let cmd = self.lint_flags();
        let mut levels = self.levels.borrow_mut();
        let levels = levels.get_or_insert_with(|| LevelMap::new(self, cmd));
        levels.level(self, cmd, lint, at)
    }

    /// Whether a diagnostic of `lint` may be shown anywhere in the crate: false when the lint
    /// flags and every lint attribute leave it allowed (rustc's `lints_that_dont_need_to_run`),
    /// so a pass that gives nothing but it can be skipped. The set is made once per check from
    /// one scan of the walk's attributes; after that this is one bit test.
    pub fn lint_may_fire(&self, lint: LintId) -> bool {
        self.lints_that_may_fire().contains(lint)
    }

    /// Whether a diagnostic of some lint of `lints` may be shown: [`Cx::lint_may_fire`] of a
    /// pass's lints at once, one AND per word.
    pub fn any_may_fire(&self, lints: &LintSet) -> bool {
        self.lints_that_may_fire().intersects(lints)
    }

    fn lints_that_may_fire(&self) -> &LintSet {
        self.may_fire.get_or_init(|| crate::lints::may_fire(self, self.lint_flags(), self.walk()))
    }

    /// Whether cargo runs the compile (rustc's `was_invoked_from_cargo`: `CARGO_CRATE_NAME` is
    /// set), as it is under blaze: some help then names `cargo add` and `Cargo.toml`.
    pub fn cargo(&self) -> bool {
        self.lints.is_some_and(|it| it.check_cfg.cargo)
    }

    /// The name rustc shows `file` by.
    pub fn file_name(&self, file: FileId) -> String {
        let name = self.files.name(file);
        let mut named = self.named.borrow_mut();
        if !named.contains_key(name.as_str()) {
            named.insert(name.clone(), file); // alloc: once per file named
        }
        name
    }

    /// The file a span names, once a span of it was made.
    pub fn file_of(&self, span: &Span) -> Option<FileId> {
        self.named.borrow().get(&span.file).copied()
    }

    pub fn file_text(&self, file: FileId) -> &'db str {
        self.db.file_text(file).text(self.db)
    }

    /// `range` of `file`, as a span with no expansion.
    pub fn span_in_file(&self, file: FileId, range: TextRange) -> Span {
        Span::new(&self.file_name(file), range)
    }

    /// The source text a span covers.
    pub fn snippet(&self, span: &Span) -> Option<&'db str> {
        let file = self.file_of(span)?;
        self.file_text(file).get(span.lo as usize..span.hi as usize)
    }

    /// The span rustc gives `range`: where it is written when it is in a file or in what a
    /// macro call was handed; where the crate's own macro's definition writes it, inside that
    /// macro's expansion; else the call of the outermost macro of another crate it came from,
    /// inside that macro's expansion.
    pub fn span(&self, range: InFile<TextRange>) -> Span {
        let Some(call) = range.file_id.macro_file() else {
            let file = range.file_id.file_id().expect("a file that is not a macro's");
            return self.span_in_file(file.file_id(self.db), range.value);
        };
        let mapped = range.original_node_file_range_opt(self.db);
        if let Some((at, ctxt)) = mapped
            && ctxt.is_root()
        {
            return self.span_in_file(at.file_id.file_id(self.db), at.range);
        }
        let expansion = self.expansion_of(call).map(Box::new);
        let loc = call.loc(self.db);
        let local = hir::Crate::from(loc.def.krate) == self.krate;
        let at = match mapped {
            Some((at, _)) if local => Span::new(&self.file_name(at.file_id.file_id(self.db)), at.range),
            _ => match &expansion {
                Some(it) => Span { expansion: None, ..it.call_site.clone() },
                None => {
                    let at = loc.kind.original_call_range(self.db, loc.krate);
                    Span::new(&self.file_name(at.file_id.file_id(self.db)), at.range)
                }
            },
        };
        Span { expansion, ..at }
    }

    /// [`Cx::span`] of a node.
    pub fn span_of(&self, node: InFile<SyntaxNodePtr>) -> Span {
        self.span(node.map(|it| it.text_range()))
    }

    /// [`Cx::span`] of a typed node.
    pub fn span_of_ptr<N: AstNode>(&self, ptr: InFile<AstPtr<N>>) -> Span {
        self.span(ptr.map(|it| it.text_range()))
    }

    /// The node `ptr` points to, parsed or expanded.
    pub fn node<N: AstNode>(&self, ptr: InFile<AstPtr<N>>) -> InFile<N> {
        let root = self.sema.parse_or_expand(ptr.file_id);
        ptr.map(|it| it.to_node(&root))
    }

    /// The syntax node `ptr` points to, parsed or expanded.
    pub fn syntax(&self, ptr: InFile<SyntaxNodePtr>) -> InFile<SyntaxNode> {
        let root = self.sema.parse_or_expand(ptr.file_id);
        ptr.map(|it| it.to_node(&root))
    }

    /// What rustc says of the expansion of the macro call `call`: where it was called (itself
    /// in an expansion when it was), what it is and where it is defined.
    pub fn expansion_of(&self, call: MacroCallId) -> Option<Expansion> {
        let db = self.db;
        let loc = call.loc(db);
        let node = loc.to_node(db);
        let call_range = |range: TextRange| self.span(node.with_value(range));
        let (kind, name, call_site) = match &loc.kind {
            MacroCallKind::FnLike { .. } => {
                let call = ast::MacroCall::cast(node.value.clone())?;
                let name = call.path()?.segment()?.name_ref()?.text().to_string();
                (MacroKind::Bang, name, call_range(node.value.text_range()))
            }
            MacroCallKind::Derive { derive_index, .. } => {
                let attr = ast::Attr::cast(node.value.clone())?;
                let (range, path) = derive_paths(&attr).into_iter().nth(*derive_index as usize)?;
                let name = path.rsplit("::").next()?.to_owned();
                (MacroKind::Derive, name, call_range(range))
            }
            MacroCallKind::Attr { .. } => {
                let item = ast::Item::cast(node.value.clone())?;
                let attr = item.attrs().next()?;
                let name = attr.path()?.segment()?.name_ref()?.text().to_string();
                (MacroKind::Attr, name, call_range(attr.syntax().text_range()))
            }
        };
        let def_site = match loc.def.kind {
            MacroDefKind::ProcMacro(id, ..) => {
                let file = id.file_id.file_id()?.file_id(db);
                let item = id.to_node(db);
                Some(self.span_in_file(file, signature_range(&item)))
            }
            MacroDefKind::Declarative(id, _)
            | MacroDefKind::BuiltIn(id, _)
            | MacroDefKind::BuiltInAttr(id, _)
            | MacroDefKind::BuiltInDerive(id, _)
            | MacroDefKind::BuiltInEager(id, _)
            | MacroDefKind::UnimplementedBuiltIn(id) => {
                let item = id.to_node(db);
                Some(self.span(id.with_value(item_range(item.syntax()))))
            }
        };
        Some(Expansion { call_site, kind, name, def_site })
    }

    /// rustc's printing of `ty`: trimmed paths, `{integer}` for an integer variable, closures
    /// by where they are written.
    pub fn ty(&self, ty: &hir::Type<'db>) -> String {
        crate::print::ty(self, ty)
    }

    /// rustc's `def_path_str` of `def`: the shortest path that names it, trimmed to its name
    /// when that name is unique.
    pub fn def_path(&self, def: hir::ModuleDef) -> String {
        crate::print::def_path(self, def)
    }

    /// The key of an item, for a fact.
    pub fn key(&self, item: ItemId) -> ItemKey {
        blaze_items::item_key(self.db, item)
    }

    /// The key of a module, a scope names resolve in.
    pub fn module_key(&self, module: hir::Module) -> ItemKey {
        blaze_items::key::module_key(self.db, module.into())
    }

    pub fn name_hash(&self, name: &str) -> NameHash {
        blaze_items::key::name_hash(name)
    }

    /// The simplified type impls and methods of `ty` are found under, if it has one.
    pub fn simplified(&self, ty: &hir::Type<'db>) -> Option<SimplifiedType> {
        let simplified = ty.fingerprint_for_trait_impl(self.db)?;
        Some(blaze_items::key::simplified(self.db, &simplified))
    }

    /// `Fact::Method` of `name` on `ty`.
    pub fn method_fact(&self, ty: &hir::Type<'db>, name: &str) -> Option<Fact> {
        Some(Fact::Method { self_ty: self.simplified(ty)?, name: self.name_hash(name) })
    }

    /// `Fact::Implements` of `trait_` for `ty`.
    pub fn implements_fact(&self, trait_: hir::Trait, ty: &hir::Type<'db>) -> Option<Fact> {
        let trait_ = self.key(ItemId::Trait(trait_.into()));
        Some(Fact::Implements { trait_, self_ty: self.simplified(ty)? })
    }

    /// `Fact::Resolves` of `name` in `module`.
    pub fn resolves_fact(&self, module: hir::Module, name: &str) -> Fact {
        Fact::Resolves { scope: self.module_key(module), name: self.name_hash(name) }
    }
}

/// The paths of a `#[derive(...)]`, in order, with where each is.
pub fn derive_paths(attr: &ast::Attr) -> Vec<(TextRange, String)> {
    let Some((_, tt)) = attr.as_simple_call() else { return Vec::new() };
    let mut paths = Vec::new();
    let mut current: Option<(TextSize, TextSize, String)> = None;
    for token in tt.syntax().children_with_tokens().filter_map(NodeOrToken::into_token) {
        match token.kind() {
            SyntaxKind::IDENT | SyntaxKind::COLON2 | SyntaxKind::COLON => {
                let range = token.text_range();
                match &mut current {
                    Some((_, end, text)) => {
                        *end = range.end();
                        text.push_str(token.text());
                    }
                    None => current = Some((range.start(), range.end(), token.text().to_owned())),
                }
            }
            SyntaxKind::COMMA | SyntaxKind::R_PAREN => {
                if let Some((start, end, text)) = current.take() {
                    paths.push((TextRange::new(start, end), text));
                }
            }
            _ => {}
        }
    }
    if let Some((start, end, text)) = current {
        paths.push((TextRange::new(start, end), text));
    }
    paths
}

/// Where rustc's span of an item starts: after its attributes, doc comments and the comments
/// before it.
pub fn item_range(node: &SyntaxNode) -> TextRange {
    let start = node
        .children_with_tokens()
        .find(|it| match it {
            NodeOrToken::Node(node) => {
                !matches!(node.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
            }
            NodeOrToken::Token(token) => !token.kind().is_trivia(),
        })
        .map_or(node.text_range().start(), |it| it.text_range().start());
    TextRange::new(start, node.text_range().end())
}

/// The span rustc gives a function's definition (`def_span`): its signature, without its body.
pub fn signature_range(item: &ast::Fn) -> TextRange {
    let range = item_range(item.syntax());
    let end: TextSize = match item.body() {
        Some(body) => {
            let mut end = body.syntax().text_range().start();
            let mut at = body.syntax().prev_sibling_or_token();
            while let Some(it) = at {
                if !it.kind().is_trivia() {
                    end = it.text_range().end();
                    break;
                }
                at = it.prev_sibling_or_token();
            }
            end
        }
        None => range.end(),
    };
    TextRange::new(range.start(), end.max(range.start()))
}

/// The span rustc gives an item's definition (`def_span`): a function's signature; of a type,
/// a trait or an impl, from its first keyword to the end of its header, before `{` or `;`;
/// else the whole item.
pub fn def_range(node: &SyntaxNode) -> TextRange {
    if let Some(item) = ast::Fn::cast(node.clone()) {
        return signature_range(&item);
    }
    let range = item_range(node);
    let header_end = node
        .children_with_tokens()
        .filter(|it| it.text_range().start() >= range.start())
        .find(|it| match it {
            NodeOrToken::Node(node) => matches!(
                node.kind(),
                SyntaxKind::RECORD_FIELD_LIST
                    | SyntaxKind::VARIANT_LIST
                    | SyntaxKind::ASSOC_ITEM_LIST
                    | SyntaxKind::ITEM_LIST
            ),
            NodeOrToken::Token(token) => token.kind() == SyntaxKind::SEMICOLON,
        })
        .map(|it| it.text_range().start());
    let Some(end) = header_end else { return range };
    let text = node.to_string();
    let base = node.text_range().start();
    let header = &text[usize::from(range.start() - base)..usize::from(end - base)];
    let trimmed = TextSize::of(header.trim_end());
    TextRange::at(range.start(), trimmed)
}

#[cfg(test)]
mod tests {
    use syntax::{AstNode, SourceFile, ast};

    use super::*;

    fn first<N: AstNode>(text: &str) -> N {
        SourceFile::parse(text, Edition::CURRENT).tree().syntax().descendants().find_map(N::cast).unwrap()
    }

    #[test]
    fn a_function_is_spanned_by_its_signature() {
        let text = "/// doc\n#[inline]\npub fn f(x: u32) -> u32 { x }";
        let range = signature_range(&first::<ast::Fn>(text));
        assert_eq!(&text[range], "pub fn f(x: u32) -> u32");
    }

    #[test]
    fn a_type_is_spanned_by_its_header() {
        let text = "#[derive(Debug)]\npub struct S<T> where T: Copy {\n    a: T,\n}";
        let node = first::<ast::Struct>(text);
        assert_eq!(&text[def_range(node.syntax())], "pub struct S<T> where T: Copy");
        let text = "struct U;";
        assert_eq!(&text[def_range(first::<ast::Struct>(text).syntax())], "struct U");
    }

    #[test]
    fn derive_paths_are_each_named() {
        let text = "#[derive(Debug, serde::Serialize)]\nstruct S;";
        let attr = first::<ast::Attr>(text);
        let paths: Vec<_> = derive_paths(&attr).into_iter().map(|(range, it)| (&text[range], it)).collect();
        assert_eq!(paths, [("Debug", "Debug".to_owned()), ("serde::Serialize", "serde::Serialize".to_owned())]);
    }

    #[test]
    fn files_are_named_from_the_root_below_it() {
        assert_eq!(Files::relative("/ws/src/lib.rs", "/ws"), "src/lib.rs");
        assert_eq!(Files::relative("/ws/src/lib.rs", "/ws/"), "src/lib.rs");
        assert_eq!(Files::relative("/ws", "/ws"), "/ws");
        assert_eq!(Files::relative("/ws/", "/ws"), "/ws/");
        assert_eq!(Files::relative("/wsx/lib.rs", "/ws"), "/wsx/lib.rs");
        assert_eq!(Files::relative("/std/lib.rs", "/ws"), "/std/lib.rs");
        assert_eq!(Files::relative("src/lib.rs", ""), "src/lib.rs");
        assert_eq!(Files::relative("$DIR/a.rs", ""), "$DIR/a.rs");

        let paths = HashMap::from([(FileId::from_raw(0), "/ws/src/main.rs".to_owned())]);
        let files = Files::new(&paths, "/ws");
        assert_eq!(files.name(FileId::from_raw(0)), "src/main.rs");
        assert_eq!(files.name(FileId::from_raw(7)), "<file 7>");
    }
}
