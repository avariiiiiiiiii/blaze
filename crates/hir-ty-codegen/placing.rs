//! Where the code of the base says it is, kept apart from the code: an edit that only moves a
//! body, or lays it out again, asks nothing of what it compiles to, and the places it moved
//! are written over the base's where the base has them.
//!
//! The base gives each place its code tells of a data object of its own ([`PlaceKey`]): the
//! `core::panic::Location` of a span of a function's body, or of where a function is declared.
//! A patch defines the ones an edit moved again under the base's names, which a splice writes
//! where the executable has them. A debugger's rows of code are told by a patch of the code,
//! so code whose lines moved is in the patch when one is told them.
//!
//! A function's code asks for the places of its body by the ids of the expressions there,
//! which are the same while the body lowers the same: lowered, a body has no layout, comments
//! or places in it, which its source map has, and the macros it calls are expanded. So code
//! compiled from bodies that lower as they did is not asked again: its places are found by the
//! ids the base has. Code of a body that lowers otherwise is asked, and when it is the same
//! but for those ids, its places are found by the ids it has now ([`Places::remap`]), which
//! the next edits find them by.

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use base_db::FileId;
use hir_def::{DefWithBodyId, FunctionId, expr_store::Body, hir::ExprId};
use hir_ty::{db::HirDatabase, mir::MirSpan};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{
    Fan, Respace, declaration_location,
    fragment::{CompiledFunction, DataRequest},
    home::body_file,
    patch::probe,
    print_of, row_location, source_location,
};

/// A file, line and column, as [`source_location`] finds them.
pub(super) type Location = Option<(FileId, u32, u32)>;

/// A place the code of the base tells of, which has a data object of its own there.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum PlaceKey {
    /// The `core::panic::Location` of a span of a function's body.
    At(FunctionId, MirSpan),
    /// Where a function is declared: what a pointer to a `#[track_caller]` one passes it.
    Declared(FunctionId),
}

impl PlaceKey {
    /// The place a request is for, when it has a data object of its own in the base.
    pub(super) fn of(request: &DataRequest) -> Option<PlaceKey> {
        match request {
            DataRequest::Location(Some((DefWithBodyId::FunctionId(func), span))) => {
                Some(PlaceKey::At(*func, *span))
            }
            DataRequest::Declaration(func) => Some(PlaceKey::Declared(*func)),
            _ => None,
        }
    }

    fn func(self) -> FunctionId {
        match self {
            PlaceKey::At(func, _) | PlaceKey::Declared(func) => func,
        }
    }

    pub(super) fn location(self, db: &dyn HirDatabase) -> Location {
        match self {
            PlaceKey::At(func, span) => source_location(db, func.into(), span),
            PlaceKey::Declared(func) => *declaration_location(db, func),
        }
    }
}

/// The rows a debugger is told of a function's code, by the span each is of.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Rows {
    owner: DefWithBodyId,
    closure: Option<ExprId>,
    spans: Box<[Option<MirSpan>]>,
}

impl Rows {
    fn of(compiled: &CompiledFunction) -> Option<Rows> {
        let (owner, spans) = compiled.source.as_ref()?;
        let spans = compiled.code.lines.iter().map(|row| spans.get(row.span as usize).copied());
        Some(Rows { owner: *owner, closure: compiled.closure, spans: spans.collect() })
    }

    /// Where each row is, as [`super::patch`]'s marks hash them.
    pub(super) fn lines(&self, db: &dyn HirDatabase) -> u64 {
        let mut hasher = rustc_hash::FxHasher::default();
        for span in &self.spans {
            span.map(|span| row_location(db, self.owner, self.closure, span)).hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// What the base keeps of a function's code to find its places again without asking it.
#[derive(Clone)]
pub(super) struct Placing {
    /// The places the code asks for, in the order it does, as the base has them.
    keys: Box<[PlaceKey]>,
    /// The bodies the code is compiled from, or tells a place of.
    owners: Box<[DefWithBodyId]>,
    /// Its rows when a debugger is told them, by the spans they are of now.
    rows: Option<Arc<Rows>>,
    /// Where its rows were in the base.
    pub(super) lines: u64,
    /// It has where something is written in it, which no request stands for: see
    /// [`super::fragment::Requests::placed`].
    placed: bool,
}

impl Placing {
    /// `of` is the body the code is of, and `bodies` those it mentions.
    pub(super) fn new(
        db: &dyn HirDatabase,
        compiled: &CompiledFunction,
        bodies: &[DefWithBodyId],
        debug: bool,
    ) -> Placing {
        let now = Now::of(db, compiled, debug);
        let mut owners: Vec<DefWithBodyId> = bodies.to_vec();
        owners.extend(now.keys.iter().map(|it| DefWithBodyId::from(it.func())));
        owners.extend(now.rows.as_ref().map(|it| it.owner));
        owners.sort_unstable();
        owners.dedup();
        Placing {
            keys: now.keys,
            owners: owners.into(),
            rows: now.rows.map(Arc::new),
            lines: now.lines,
            placed: compiled.requests.placed,
        }
    }

    /// Whether the edits since the base can have changed the code, or where it is in a way
    /// the base can't find again: one of its bodies lowers otherwise, or `edited`, its file
    /// was edited and it has where something is written in it.
    pub(super) fn asked(&self, places: &Places, edited: bool) -> bool {
        self.placed && edited || self.owners.iter().any(|it| places.rewritten.contains_key(it))
    }

    pub(super) fn owners(&self) -> &[DefWithBodyId] {
        &self.owners
    }

    /// Whether where something is written is a value in the code, which only its code tells.
    pub(super) fn writes_places(&self) -> bool {
        self.placed
    }

    pub(super) fn set_rows(&mut self, rows: Arc<Rows>) {
        self.rows = Some(rows);
    }

    /// Where its rows are now, the base's spans of them being where they are.
    pub(super) fn lines_now(&self, db: &dyn HirDatabase) -> u64 {
        self.rows.as_ref().map_or(0, |it| it.lines(db))
    }
}

/// What the code of a function asked again tells of its places.
pub(super) struct Now {
    keys: Box<[PlaceKey]>,
    rows: Option<Rows>,
    pub(super) lines: u64,
}

impl Now {
    pub(super) fn of(db: &dyn HirDatabase, compiled: &CompiledFunction, debug: bool) -> Now {
        let keys = compiled.requests.datas.iter().filter_map(PlaceKey::of).collect();
        let rows = debug.then(|| Rows::of(compiled)).flatten();
        let lines = rows.as_ref().map_or(0, |it| it.lines(db));
        Now { keys, rows, lines }
    }

    /// What the look that asked the code of `placing` found of its places, the code being the
    /// same: the places of the base found by other spans now, and its rows when they are
    /// others. `None` when the code asks for other places than the base's.
    pub(super) fn found(
        self,
        placing: &Placing,
        places: &Places,
    ) -> Option<(Vec<(PlaceKey, PlaceKey)>, Option<Arc<Rows>>)> {
        if self.keys.len() != placing.keys.len() {
            return None;
        }
        let mut remapped = Vec::new();
        for (&was, &now) in placing.keys.iter().zip(&self.keys) {
            if was.func() != now.func() {
                return None;
            }
            if places.current(was) != Some(now) {
                remapped.push((was, now));
            }
        }
        let rows = self.rows.filter(|it| placing.rows.as_deref() != Some(it)).map(Arc::new);
        Some((remapped, rows))
    }
}

/// A hash of the code of a function but for where it is: what tells the code of a body that
/// lowers otherwise from its code before, whose spans have other ids.
pub(super) fn code_print(compiled: &CompiledFunction) -> u128 {
    struct Unplaced<'a>(&'a CompiledFunction);
    impl Hash for Unplaced<'_> {
        fn hash<H: Hasher>(&self, state: &mut H) {
            let compiled = self.0;
            compiled.code.hash(state);
            compiled.requests.funcs.hash(state);
            compiled.requests.placed.hash(state);
            for request in &compiled.requests.datas {
                match request {
                    DataRequest::Location(at) => (0u8, at.map(|it| it.0)).hash(state),
                    other => (1u8, other).hash(state),
                }
            }
            compiled.source.as_ref().map(|it| it.0).hash(state);
            compiled.variables.as_ref().map(|it| &it.1).hash(state);
        }
    }
    print_of(&Unplaced(compiled))
}

/// Where the base has the places its code tells of, and what of the edits since it was built
/// moved them.
#[derive(Clone, Default)]
pub(super) struct Places {
    /// The symbol of each place's data object, and where the place was when the base was
    /// linked, which each patch starts from.
    of: FxHashMap<PlaceKey, (String, Location)>,
    /// The places by the file of their body, and the file they are in when that is another:
    /// an edit of either can move one.
    of_file: FxHashMap<FileId, Vec<PlaceKey>>,
    /// The bodies the code of the base comes from, by the file each is written in.
    owners_of_file: FxHashMap<FileId, Vec<DefWithBodyId>>,
    /// The files edited since the base was built, whose places are found again for every
    /// patch.
    edited: FxHashSet<FileId>,
    /// The files of `edited` that no edit since the base changed a token of: their bodies
    /// lower as the base has them, and are taken in before the first edit that changes more.
    unread: FxHashSet<FileId>,
    /// The files written otherwise whose text the database keeps: where their places are now.
    respace: FxHashMap<FileId, Arc<Respace>>,
    /// Each body of `edited` lowered as the spans of its places are of: the base's, or those
    /// the last look that asked its code found. `None` for one that a file edited without
    /// being looked at first has, or whose syntax is gone.
    written: FxHashMap<DefWithBodyId, Option<triomphe::Arc<Body>>>,
    /// The bodies that lower otherwise than `written` has them, as they do now.
    rewritten: FxHashMap<DefWithBodyId, Option<triomphe::Arc<Body>>>,
    /// The spans the places of bodies that lower otherwise are found by now, by the base's.
    /// `None` when no code asked again is the same but for them: only code that is not the
    /// base's has them.
    now: FxHashMap<PlaceKey, Option<PlaceKey>>,
    /// What the last look found [`Places::moved`], for the patch it is made for.
    pub(super) moving: Vec<(String, Location)>,
}

impl Places {
    /// `places` are the data objects of the base by place, with the symbol of each and where
    /// it is.
    pub(super) fn new<'a>(
        db: &dyn HirDatabase,
        places: impl Iterator<Item = (PlaceKey, String, Location)>,
        owners: impl Iterator<Item = &'a DefWithBodyId>,
    ) -> Places {
        let mut it = Places::default();
        let mut files: FxHashMap<FunctionId, FileId> = FxHashMap::default();
        for (key, symbol, at) in places {
            let file = *files.entry(key.func()).or_insert_with(|| body_file(db, key.func().into()));
            it.of_file.entry(file).or_default().push(key);
            if let Some((other, ..)) = at.filter(|it| it.0 != file) {
                it.of_file.entry(other).or_default().push(key);
            }
            it.of.insert(key, (symbol, at));
        }
        let owners: FxHashSet<DefWithBodyId> = owners.copied().collect();
        for owner in owners {
            it.owners_of_file.entry(body_file(db, owner)).or_default().push(owner);
        }
        it
    }

    /// The span a place of the base is found by now, `None` when no code of the base tells
    /// of it any more.
    fn current(&self, key: PlaceKey) -> Option<PlaceKey> {
        match self.now.get(&key) {
            Some(now) => *now,
            None => Some(key),
        }
    }

    pub(super) fn rewritten_owners(&self) -> impl Iterator<Item = &DefWithBodyId> {
        self.rewritten.keys()
    }

    /// Before an edit of `files`, of the revision before it: the bodies of those not edited
    /// since the base, lowered as the base has them.
    pub(super) fn before_edit(&mut self, db: &dyn HirDatabase, files: &[FileId], fan: &Fan<'_>) {
        let new: Vec<FileId> = (files.iter().copied())
            .filter(|it| !self.edited.contains(it) || self.unread.remove(it))
            .collect();
        let owners = self.owners_of(&new);
        for (owner, written) in owners.iter().zip(lowered_all(db, &owners, fan)) {
            self.written.insert(*owner, written);
        }
        self.edited.extend(new);
    }

    /// After an edit that changed no token of `files`, only where they are, which the
    /// database keeps the text of: their places are found again for every patch, and moved.
    pub(super) fn respaced(&mut self, files: &[(FileId, Arc<Respace>)]) {
        for (file, respace) in files {
            if self.edited.insert(*file) {
                self.unread.insert(*file);
            }
            self.respace.insert(*file, respace.clone());
        }
    }

    /// After an edit of `files`: which of their bodies lower otherwise now. A file edited
    /// without being looked at before has none the base's places can be found in by the
    /// base's spans.
    pub(super) fn after_edit(&mut self, db: &dyn HirDatabase, files: &[FileId], fan: &Fan<'_>) {
        for file in files {
            self.respace.remove(file);
            if self.edited.insert(*file) {
                for owner in self.owners_of_file.get(file).into_iter().flatten() {
                    self.written.insert(*owner, None);
                }
            }
        }
        let owners = self.owners_of(files);
        for (owner, now) in owners.iter().zip(lowered_all(db, &owners, fan)) {
            let same = match (self.written.get(owner), &now) {
                (Some(Some(was)), Some(now)) => triomphe::Arc::ptr_eq(was, now) || was == now,
                _ => false,
            };
            match same {
                true => _ = self.rewritten.remove(owner),
                false => _ = self.rewritten.insert(*owner, now),
            }
        }
    }

    fn owners_of(&self, files: &[FileId]) -> Vec<DefWithBodyId> {
        let owners = files.iter().filter_map(|it| self.owners_of_file.get(it));
        owners.flatten().copied().collect()
    }

    /// Takes what a whole look found: the places of the base found by other spans now. All
    /// the code of a body that lowers otherwise was asked, so what it was asked by is the
    /// body's from now on, and a place of it that no code the same as the base's asked for is
    /// had by none. `None` when code the same as the base's finds a place by two spans.
    fn remap(
        &mut self,
        found: impl Iterator<Item = (PlaceKey, PlaceKey)>,
    ) -> Option<()> {
        let mut by: FxHashMap<PlaceKey, PlaceKey> = FxHashMap::default();
        for (was, now) in found {
            if *by.entry(was).or_insert(now) != now {
                return None;
            }
        }
        let rewritten = std::mem::take(&mut self.rewritten);
        let funcs: FxHashSet<FunctionId> = (rewritten.keys())
            .filter_map(|it| match it {
                DefWithBodyId::FunctionId(func) => Some(*func),
                _ => None,
            })
            .collect();
        for file in &self.edited {
            for &key in self.of_file.get(file).into_iter().flatten() {
                if funcs.contains(&key.func()) {
                    self.now.insert(key, by.get(&key).copied());
                }
            }
        }
        for (was, now) in by {
            self.now.insert(was, Some(now));
        }
        self.now.retain(|key, now| *now != Some(*key));
        self.written.extend(rewritten);
        Some(())
    }

    /// The places of the base that are elsewhere now, by the symbol of each and where it is.
    /// One found in a file it was not in is looked at again when that file is edited.
    fn moved(&mut self, db: &dyn HirDatabase) -> Vec<(String, Location)> {
        let mut keys: Vec<PlaceKey> =
            self.edited.iter().filter_map(|it| self.of_file.get(it)).flatten().copied().collect();
        keys.sort_unstable_by_key(|it| self.of[it].0.as_str());
        keys.dedup();
        let mut moved = Vec::new();
        for key in keys {
            let Some(now) = self.current(key) else { continue };
            let Some(at) = probe(|| now.location(db)) else { continue };
            let at = at.map(|(file, line, column)| match self.respace.get(&file) {
                Some(respace) => {
                    let (line, column) = respace.place(line, column);
                    (file, line, column)
                }
                None => (file, line, column),
            });
            let (symbol, was) = &self.of[&key];
            if at == *was {
                continue;
            }
            moved.push((symbol.clone(), at));
            if let Some((file, ..)) = at
                && !self.of_file.get(&file).is_some_and(|it| it.contains(&key))
            {
                self.of_file.entry(file).or_default().push(key);
            }
        }
        moved
    }

    /// Takes what a whole look found, as [`Places::remap`], and what moved with it.
    pub(super) fn settle(
        &mut self,
        db: &dyn HirDatabase,
        found: impl Iterator<Item = (PlaceKey, PlaceKey)>,
    ) -> Option<()> {
        self.remap(found)?;
        self.moving = self.moved(db);
        Some(())
    }
}

/// Each of `owners` lowered, on the threads `fan` has: `None` for one whose syntax is gone.
fn lowered_all(
    db: &dyn HirDatabase,
    owners: &[DefWithBodyId],
    fan: &Fan<'_>,
) -> Vec<Option<triomphe::Arc<Body>>> {
    let lowered = |db: &dyn HirDatabase, owner: DefWithBodyId| {
        probe(|| match Body::relative(db, owner) {
            Some((body, _)) => body.clone(),
            None => Body::with_source_map(db, owner).0.clone(),
        })
    };
    let found: Vec<std::sync::OnceLock<Option<triomphe::Arc<Body>>>> =
        owners.iter().map(|_| std::sync::OnceLock::new()).collect();
    fan(owners.len(), &|db, at| _ = found[at].get_or_init(|| lowered(db, owners[at])));
    (found.into_iter().enumerate())
        .map(|(at, it)| it.into_inner().unwrap_or_else(|| lowered(db, owners[at])))
        .collect()
}
