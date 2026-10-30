use std::{
    cell::Cell,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use salsa::Durability;
use span::{Edition, TextSize};
use syntax::TextRange;
use triomphe::Arc;
use vfs::{
    AbsPathBuf, AnchoredPath, FileId, VfsPath,
    file_set::{FileSet, FileSetConfig},
};

use crate::{
    BuiltDependency, ContentId, Crate, CrateBuilderId, CrateGraphBuilder, CrateName, CrateOrigin,
    CrateType, CrateTypes, CrateWorkspaceData, CratesMap, DependencyBuilder, EditionedFileId, Env,
    FileSourceRootInput, FileText, Files, Nonce, SourceDatabase, SourceRoot, SourceRootId,
    SourceRootInput, TargetKind, ViewChange, ViewError, ViewFile, ViewRoot, all_crates_content,
    cfg_options_content, install_plane_hooks,
    reparse::{
        REPARSED, TextEdit, assert_whole, common_prefix, common_prefix_scalar, common_suffix,
        common_suffix_scalar,
    },
    run_on_pool, set_all_crates_with_durability,
    target::{Arch, TargetData},
};

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[salsa::db]
struct TestDb {
    storage: salsa::Storage<Self>,
    files: Arc<Files>,
    crates_map: Arc<CratesMap>,
    nonce: Nonce,
}

impl TestDb {
    fn new() -> TestDb {
        let mut db = TestDb {
            storage: salsa::Storage::default(),
            files: Arc::default(),
            crates_map: Arc::default(),
            nonce: Nonce::new(),
        };
        set_all_crates_with_durability(&mut db, std::iter::empty(), Durability::HIGH);
        db
    }

    fn revision(&self) -> salsa::Revision {
        self.nonce_and_revision().1
    }
}

#[salsa::db]
impl salsa::Database for TestDb {}

impl fmt::Debug for TestDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestDb").finish()
    }
}

#[salsa::db]
impl SourceDatabase for TestDb {
    fn file_text(&self, file_id: FileId) -> FileText {
        self.files.file_text(file_id)
    }

    fn set_file_text(&mut self, file_id: FileId, text: &str) {
        let files = Arc::clone(&self.files);
        files.set_file_text(self, file_id, text);
    }

    fn has_file_text(&self, file_id: FileId) -> bool {
        self.files.has_file_text(file_id)
    }

    fn set_file_text_with_durability(&mut self, file_id: FileId, text: &str, durability: Durability) {
        let files = Arc::clone(&self.files);
        files.set_file_text_with_durability(self, file_id, text, durability);
    }

    fn source_root(&self, id: SourceRootId) -> SourceRootInput {
        self.files.source_root(id)
    }

    fn file_source_root(&self, id: FileId) -> FileSourceRootInput {
        self.files.file_source_root(self, id)
    }

    fn set_file_source_root_with_durability(
        &mut self,
        id: FileId,
        source_root_id: SourceRootId,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_file_source_root_with_durability(self, id, source_root_id, durability);
    }

    fn set_source_root_with_durability(
        &mut self,
        source_root_id: SourceRootId,
        source_root: Arc<SourceRoot>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_source_root_with_durability(self, source_root_id, source_root, durability);
    }

    fn set_appended_root_module(
        &mut self,
        file_id: FileId,
        module: Option<&'static str>,
        durability: Durability,
    ) {
        let files = Arc::clone(&self.files);
        files.set_appended_root_module(self, file_id, module, durability);
    }

    fn set_view_root(&mut self, view: ViewRoot<'_>) -> Result<ViewChange, ViewError> {
        let files = Arc::clone(&self.files);
        files.set_view_root(self, view)
    }

    fn crates_map(&self) -> Arc<CratesMap> {
        self.crates_map.clone()
    }

    fn run_all(&self, count: usize, task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync)) {
        (0..count).for_each(|at| task(self, at));
    }

    fn nonce_and_revision(&self) -> (Nonce, salsa::Revision) {
        (self.nonce, salsa::plumbing::ZalsaDatabase::zalsa(self).current_revision())
    }

    fn line_column(&self, file: FileId, offset: TextSize) -> Result<(u32, u32), ()> {
        let text = self.file_text(file).text(self).clone();
        let before = text.get(..u32::from(offset) as usize).ok_or(())?;
        let line = before.matches('\n').count() as u32;
        let column = before.rsplit('\n').next().map_or(0, str::len) as u32;
        Ok((line, column))
    }
}

/// Puts `files` (path, text) in source root 0, as file IDs from `first` on.
fn load(db: &mut TestDb, first: u32, files: &[(&str, &str)]) -> Vec<FileId> {
    let mut set = FileSet::default();
    let mut ids = Vec::new();
    for (n, (path, text)) in files.iter().enumerate() {
        let id = FileId::from_raw(first + n as u32);
        db.set_file_text_with_durability(id, text, Durability::LOW);
        db.set_file_source_root_with_durability(id, SourceRootId(0), Durability::MEDIUM);
        set.insert(id, VfsPath::new_virtual_path(path.to_string()));
        ids.push(id);
    }
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
    ids
}

fn cfg(atoms: &[CfgAtom]) -> CfgOptions {
    atoms.iter().cloned().collect()
}

fn flag(name: &str) -> CfgAtom {
    CfgAtom::Flag(Symbol::intern(name))
}

fn key_value(key: &str, value: &str) -> CfgAtom {
    CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) }
}

fn add(graph: &mut CrateGraphBuilder, root: FileId, cfg: CfgOptions, env: Env) -> CrateBuilderId {
    graph.add_crate_root(
        root,
        Edition::Edition2021,
        None,
        None,
        cfg,
        None,
        env,
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    )
}

fn depend(graph: &mut CrateGraphBuilder, from: CrateBuilderId, to: CrateBuilderId, name: &str) {
    graph.add_dep(from, DependencyBuilder::new(CrateName::new(name).unwrap(), to)).unwrap();
}

const FILES: &[(&str, &str)] =
    &[("/a/lib.rs", "pub mod b;"), ("/a/b.rs", "pub fn f() {}"), ("/c/lib.rs", "pub struct C;")];

#[test]
fn equal_content_gives_equal_ids_across_databases() {
    let ids = |first: u32| {
        let mut db = TestDb::new();
        let files = load(&mut db, first, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], cfg(&[flag("unix")]), Env::default());
        let c = add(&mut graph, files[2], cfg(&[flag("unix")]), Env::default());
        depend(&mut graph, a, c, "c");
        let crates = graph.set_in_db(&mut db);
        (
            db.file_text(files[1]).content(&db),
            db.source_root(SourceRootId(0)).content(&db),
            crates[&a].content(&db),
            crates[&c].content(&db),
            all_crates_content(&db),
        )
    };
    // The file IDs differ between the two; nothing of them is hashed.
    assert_eq!(ids(0), ids(40));
}

#[test]
fn a_file_text_edit_changes_its_id_and_its_source_roots() {
    let mut db = TestDb::new();
    let files = load(&mut db, 0, FILES);
    let ids = |db: &TestDb| {
        (db.file_text(files[1]).content(db), db.source_root(SourceRootId(0)).content(db))
    };
    let before = ids(&db);
    assert_eq!(before.0, FileText::content_of("pub fn f() {}", None));

    db.set_file_text(files[1], "pub fn g() {}");
    let edited = ids(&db);
    assert_ne!(edited.0, before.0);
    assert_ne!(edited.1, before.1);
    assert_eq!(db.file_text(files[0]).content(&db), FileText::content_of("pub mod b;", None));

    db.set_file_text(files[1], "pub fn f() {}");
    assert_eq!(ids(&db), before);

    let root = db.source_root(SourceRootId(0)).content(&db);
    db.set_appended_root_module(files[0], Some("mod __harness {}"), Durability::LOW);
    assert_eq!(
        db.file_text(files[0]).content(&db),
        FileText::content_of("pub mod b;", Some("mod __harness {}"))
    );
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), root);
}

#[test]
fn a_files_parse_shares_its_text() {
    let mut db = TestDb::new();
    let files = load(&mut db, 0, FILES);
    let parse = EditionedFileId::new(&db, files[1], Edition::Edition2021).parse(&db);
    let root = parse.syntax_node();
    let tree = root.flat_tree().expect("a parsed file is a flat tree");
    assert!(std::sync::Arc::ptr_eq(tree.source(), db.file_text(files[1]).text(&db)));
}

/// #1294: a dependency's file is an outline, whose `fn` bodies stay unparsed until reached.
#[test]
fn a_library_files_parse_leaves_bodies_unparsed() {
    let mut db = TestDb::new();
    let files = load(&mut db, 0, &[("/a/lib.rs", "fn f() { let x = 1 + 1; }")]);
    let body_is_lazy = |db: &TestDb| {
        let parse = EditionedFileId::new(db, files[0], Edition::Edition2021).parse(db);
        let root = parse.syntax_node();
        let function = root.children().next().expect("the `fn`");
        let body = function.children().find(|it| it.kind() == syntax::SyntaxKind::BLOCK_EXPR);
        body.expect("its body").is_lazy()
    };
    assert!(!body_is_lazy(&db));

    let mut set = FileSet::default();
    set.insert(files[0], VfsPath::new_virtual_path("/a/lib.rs".to_owned()));
    let root = Arc::new(SourceRoot::new_library(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::HIGH);
    assert!(body_is_lazy(&db));
}

#[test]
fn the_order_of_cfg_atoms_does_not_change_a_crates_id() {
    let atoms = [flag("unix"), key_value("feature", "a"), key_value("feature", "b"), flag("test")];
    let mut reversed = atoms.clone();
    reversed.reverse();
    assert_eq!(cfg_options_content(&cfg(&atoms)), cfg_options_content(&cfg(&reversed)));
    assert_ne!(cfg_options_content(&cfg(&atoms)), cfg_options_content(&cfg(&atoms[1..])));

    let crate_id = |atoms: &[CfgAtom]| {
        let mut db = TestDb::new();
        let files = load(&mut db, 0, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], cfg(atoms), Env::default());
        graph.set_in_db(&mut db)[&a].content(&db)
    };
    assert_eq!(crate_id(&atoms), crate_id(&reversed));
    assert_ne!(crate_id(&atoms), crate_id(&atoms[1..]));
}

#[test]
fn a_dependencys_change_changes_its_dependents_ids() {
    let mut db = TestDb::new();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let b = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[1], CfgOptions::default(), Env::default());
    depend(&mut graph, a, b, "b");
    let crates = graph.set_in_db(&mut db);
    let (a, b, c): (Crate, Crate, Crate) = (crates[&a], crates[&b], crates[&c]);
    let ids = |db: &TestDb| (a.content(db), b.content(db), c.content(db), all_crates_content(db));
    let before = ids(&db);

    let env: Env = [("KEY".to_owned(), "value".to_owned())].into_iter().collect();
    b.set_env(&mut db, env.clone());
    assert_eq!(b.env(&db), &env);
    let after = ids(&db);
    assert_ne!(after.1, before.1);
    assert_ne!(after.0, before.0);
    assert_eq!(after.2, before.2);
    assert_ne!(after.3, before.3);

    b.set_env(&mut db, Env::default());
    assert_eq!(ids(&db), before);
}

#[test]
fn a_crates_types_read_back_and_are_in_its_id() {
    let mut db = TestDb::new();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = graph.set_in_db(&mut db)[&a];
    assert_eq!(a.data(&db).crate_types, CrateTypes::of(&[CrateType::Lib]));
    let as_lib = a.content(&db);

    let mut data = a.data(&db).clone();
    data.crate_types = CrateTypes::of(&[CrateType::Cdylib, CrateType::Rlib]);
    a.set_data(&mut db, data);
    let types = a.data(&db).crate_types;
    assert_eq!(types.iter().collect::<Vec<_>>(), [CrateType::Rlib, CrateType::Cdylib]);
    assert!(!types.contains(CrateType::Lib));
    assert_ne!(a.content(&db), as_lib);
}

#[test]
fn a_dependency_added_outside_the_prelude_says_so() {
    let mut db = TestDb::new();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let b = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = graph.set_in_db(&mut db);
    let (a, b): (Crate, Crate) = (crates[&a], crates[&b]);
    let name = CrateName::new("b").unwrap();
    let depend_on = |db: &mut TestDb, dependency: BuiltDependency| {
        let mut data = a.data(&*db).clone();
        data.dependencies = vec![dependency];
        a.set_data(db, data);
        a.content(&*db)
    };

    let in_prelude = depend_on(&mut db, BuiltDependency::new(name.clone(), b));
    let outside = depend_on(&mut db, BuiltDependency::with_prelude(name.clone(), b, false, true));
    let dependency = &a.data(&db).dependencies[0];
    assert!(!dependency.is_prelude());
    assert!(dependency.is_sysroot());
    assert_ne!(outside, in_prelude);
    assert_eq!(depend_on(&mut db, BuiltDependency::with_prelude(name, b, true, false)), in_prelude);
}

/// Source roots 0 (the checkout) and 1 (everything else), both empty.
fn checkout(db: &mut TestDb) -> (AbsPathBuf, FileSetConfig) {
    let base = AbsPathBuf::assert_utf8(std::env::temp_dir().join("blaze-base-db-view"));
    let mut partition = FileSetConfig::builder();
    partition.add_file_set(vec![VfsPath::from(base.clone())]);
    for root in [0, 1] {
        let empty = Arc::new(SourceRoot::new_local(FileSet::default()));
        db.set_source_root_with_durability(SourceRootId(root), empty, Durability::MEDIUM);
    }
    (base, partition.build())
}

fn vfs_file(bytes: &[u8]) -> blaze_vfs::File<'_> {
    blaze_vfs::File {
        id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes),
        mode: blaze_vfs::Mode::Regular,
        bytes,
    }
}

fn set_view(
    db: &mut TestDb,
    base: &AbsPathBuf,
    partition: &FileSetConfig,
    files: &[(&str, &[u8])],
) -> Result<ViewChange, ViewError> {
    let files: Vec<ViewFile<'_>> =
        files.iter().map(|&(path, bytes)| ViewFile { path, file: vfs_file(bytes) }).collect();
    db.set_view_root(ViewRoot { base, built: &[], partition, other: SourceRootId(1), files: &files })
}

fn file_at(db: &TestDb, base: &AbsPathBuf, path: &str) -> Option<FileId> {
    let root = db.source_root(SourceRootId(0)).source_root(db);
    root.file_for_path(&VfsPath::from(base.join(path))).copied()
}

const VIEW: &[(&str, &[u8])] =
    &[("src/lib.rs", b"mod a;"), ("src/a.rs", b"fn f() {}"), ("README.md", b"hello")];

#[test]
fn setting_a_view_twice_writes_nothing() {
    let mut db = TestDb::new();
    let (base, partition) = checkout(&mut db);
    let first = set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(first.added.len(), 3);
    assert_eq!(first.roots, vec![SourceRootId(0)]);
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    assert_eq!(&**db.file_text(a).text(&db), "fn f() {}");
    assert_eq!(db.file_source_root(a).source_root_id(&db), SourceRootId(0));

    let revision = db.revision();
    let root = db.source_root(SourceRootId(0)).content(&db);
    let second = set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(second, ViewChange::default());
    assert_eq!(db.revision(), revision);
    assert_eq!(db.source_root(SourceRootId(0)).content(&db), root);
}

#[test]
fn a_view_with_one_changed_file_sets_that_one() {
    let mut db = TestDb::new();
    let (base, partition) = checkout(&mut db);
    set_view(&mut db, &base, &partition, VIEW).unwrap();
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    let root = db.source_root(SourceRootId(0)).content(&db);

    let edited: &[(&str, &[u8])] =
        &[("src/lib.rs", b"mod a;"), ("src/a.rs", b"fn g() {}"), ("README.md", b"hello")];
    let change = set_view(&mut db, &base, &partition, edited).unwrap();
    assert_eq!(change, ViewChange { set: vec![a], ..ViewChange::default() });
    assert_eq!(&**db.file_text(a).text(&db), "fn g() {}");
    assert_eq!(db.file_text(a).content(&db), FileText::content_of("fn g() {}", None));
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), root);
}

#[test]
fn a_view_without_a_file_takes_it_out_of_its_source_root() {
    let mut db = TestDb::new();
    let (base, partition) = checkout(&mut db);
    set_view(&mut db, &base, &partition, VIEW).unwrap();
    let readme = file_at(&db, &base, "README.md").unwrap();

    let change = set_view(&mut db, &base, &partition, &VIEW[..2]).unwrap();
    assert_eq!(change.removed, vec![readme]);
    assert_eq!(change.roots, vec![SourceRootId(0)]);
    assert_eq!(file_at(&db, &base, "README.md"), None);

    // Back again, with the file ID it had.
    let change = set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(change.added, vec![readme]);
    assert_eq!(file_at(&db, &base, "README.md"), Some(readme));
}

#[test]
fn bytes_that_are_not_utf8_are_a_file_without_text() {
    let mut db = TestDb::new();
    let (base, partition) = checkout(&mut db);
    let binary: &[(&str, &[u8])] = &[("src/lib.rs", b"mod a;"), ("data.bin", &[0xff, 0xfe])];
    let change = set_view(&mut db, &base, &partition, binary).unwrap();
    let data = file_at(&db, &base, "data.bin").unwrap();
    assert_eq!(change.no_text, vec![data]);
    assert!(!db.has_file_text(data));

    let text: &[(&str, &[u8])] = &[("src/lib.rs", b"mod a;"), ("data.bin", b"now text")];
    let change = set_view(&mut db, &base, &partition, text).unwrap();
    assert_eq!(change.set, vec![data]);
    assert!(db.has_file_text(data));
    assert_eq!(&**db.file_text(data).text(&db), "now text");

    let change = set_view(&mut db, &base, &partition, binary).unwrap();
    assert_eq!(change.no_text, vec![data]);
    assert!(!db.has_file_text(data));
}

#[test]
fn a_content_id_that_is_not_its_bytes_is_refused() {
    let mut db = TestDb::new();
    let (base, partition) = checkout(&mut db);
    let revision = db.revision();
    let bytes: &[u8] = b"fn main() {}";
    let files = [
        ViewFile { path: "src/lib.rs", file: vfs_file(b"mod a;") },
        ViewFile {
            path: "src/main.rs",
            file: blaze_vfs::File {
                id: ContentId::of(blaze_vfs::FILE_DOMAIN, b"other"),
                mode: blaze_vfs::Mode::Regular,
                bytes,
            },
        },
    ];
    let view =
        ViewRoot { base: &base, built: &[], partition: &partition, other: SourceRootId(1), files: &files };
    let error = db.set_view_root(view).unwrap_err();
    assert!(matches!(&error, ViewError::Mismatch { path, .. } if path == "src/main.rs"));
    assert_eq!(db.revision(), revision);
    assert_eq!(file_at(&db, &base, "src/lib.rs"), None);
}

/// splitmix64: the seeded inputs of the model tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn a_table_finds_what_a_hash_map_does() {
    use crate::table::{Table, hash_of};
    use rustc_hash::FxHashMap;

    let table: Table<Box<str>> = Table::new();
    let mut model: FxHashMap<String, u32> = FxHashMap::default();
    let mut order = Vec::new();
    let mut rng = Rng(7);
    for _ in 0..20_000 {
        let key = format!("key {}", rng.below(5_000));
        let hash = hash_of(key.as_str());
        let (at, inserted) = table.insert(hash, |it| **it == *key, || key.as_str().into());
        match model.get(&key) {
            Some(&known) => assert_eq!((at, inserted), (known, false), "{key}"),
            None => {
                assert!(inserted, "{key}");
                model.insert(key.clone(), at);
                order.push(key.clone());
            }
        }
        assert_eq!(&**table.get(at), key.as_str());
    }
    for (key, &at) in &model {
        assert_eq!(table.find(hash_of(key.as_str()), |it| **it == **key), Some(at));
    }
    assert_eq!(table.find(hash_of("absent"), |it| &**it == "absent"), None);
    let kept: Vec<&str> = table.iter().map(|it| &**it).collect();
    assert_eq!(kept, order.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn threads_inserting_one_key_get_one_value() {
    use crate::table::{Table, hash_of};

    let table: Table<u64> = Table::new();
    let found: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let table = &table;
                scope.spawn(move || {
                    let mut rng = Rng(thread);
                    let mut at = vec![u32::MAX; 2_000];
                    for _ in 0..10_000 {
                        let key = rng.below(2_000);
                        let (got, _) = table.insert(hash_of(key), |it| *it == key, || key);
                        assert!(at[key as usize] == u32::MAX || at[key as usize] == got);
                        at[key as usize] = got;
                    }
                    at
                })
            })
            .collect();
        threads.into_iter().map(|it| it.join().unwrap()).collect()
    });
    assert!(table.iter().count() <= 2_000);
    let mut seen = std::collections::BTreeSet::new();
    for value in table.iter() {
        assert!(seen.insert(*value), "{value} kept twice");
    }
    for key in 0..2_000u64 {
        let at = table.find(hash_of(key), |it| *it == key);
        for thread in &found {
            if thread[key as usize] != u32::MAX {
                assert_eq!(Some(thread[key as usize]), at);
            }
        }
    }
}

#[test]
fn a_log_and_a_column_give_back_what_was_put() {
    use crate::table::{Column, Log};
    use salsa::plumbing::Id;

    let log: Log<u32> = Log::new();
    for value in 0..5_000u32 {
        assert_eq!(log.push(value * 3), value);
    }
    for value in 0..5_000u32 {
        assert_eq!(log.get(value), Some(&(value * 3)));
    }
    assert_eq!(log.get(5_000), None);
    assert!(log.iter().copied().eq((0..5_000).map(|it| it * 3)));

    let column: Column<Id> = Column::new();
    let mut model = std::collections::BTreeMap::new();
    let mut rng = Rng(11);
    for _ in 0..10_000 {
        let raw = rng.below(1 << 16) as u32;
        if rng.below(4) == 0 {
            column.set(raw, None);
            model.remove(&raw);
        } else {
            let id = Id::from_bits(rng.below(u32::MAX as u64) + 1);
            column.set(raw, Some(id));
            model.insert(raw, id);
        }
    }
    for raw in 0..1 << 16 {
        assert_eq!(column.get(raw), model.get(&raw).copied(), "{raw}");
    }
    assert_eq!(column.get(Column::<Id>::MAX + 1), None);
    assert!(column.iter().eq(model.iter().map(|(&raw, &id)| (raw, id))));
}

#[test]
fn what_queries_note_is_kept_once_and_sorted() {
    let mut db = TestDb::new();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = graph.set_in_db(&mut db);
    let (a, c) = (crates[&a], crates[&c]);
    for (krate, key) in [(c, "B"), (a, "B"), (a, "A"), (c, "B"), (a, "B")] {
        db.files.env_is_read(krate, key);
    }
    db.files.env_is_unset(a, "X");
    db.files.env_is_unset(a, "X");
    let mut expected = vec![(a, "A".to_owned()), (a, "B".to_owned()), (c, "B".to_owned())];
    expected.sort();
    assert_eq!(db.files.env_read(), expected);
    assert_eq!(db.files.env_unset(), vec![(a, "X".to_owned())]);

    let dir = AbsPathBuf::assert_utf8(std::env::temp_dir());
    let (x, y) = (dir.join("blaze-base-db-x.rs"), dir.join("blaze-base-db-y.rs"));
    for (path, krate) in [(&y, a), (&x, c), (&y, a), (&y, c)] {
        db.files.include_source(path, SourceRootId(0), krate);
    }
    assert_eq!(
        db.files.included_sources(),
        vec![(x.clone(), SourceRootId(0)), (y.clone(), SourceRootId(0))]
    );
    let mut named = vec![(a, y.clone()), (c, x.clone()), (c, y.clone())];
    named.sort();
    assert_eq!(db.files.sources_included_by(), named);
}

/// The view a seed makes at a step: some of a fixed set of paths, each with one of a few
/// contents, some of which are not UTF-8.
fn seeded_view(rng: &mut Rng) -> Vec<(String, Vec<u8>)> {
    const CONTENTS: &[&[u8]] = &[b"fn a() {}", b"fn b() {}", b"mod c;", &[0xff, 0x00], &[0xfe]];
    let mut view = Vec::new();
    for at in 0..16 {
        if rng.below(10) < 7 {
            let path = match at % 4 {
                0 => format!("src/m{at}.rs"),
                1 => format!("src/deep/n{at}.rs"),
                2 => format!("data/b{at}.bin"),
                _ => format!("target/out{at}.rs"),
            };
            view.push((path, CONTENTS[rng.below(CONTENTS.len() as u64) as usize].to_vec()));
        }
    }
    view
}

/// Everything a view's files are as inputs, by path: present, has text, its text ID and
/// source root; then each source root's content ID.
fn view_state(
    db: &TestDb,
    base: &AbsPathBuf,
    paths: &[String],
) -> (Vec<Option<(bool, Option<ContentId>, SourceRootId)>>, [ContentId; 2]) {
    let files = paths
        .iter()
        .map(|path| {
            let id = file_at(db, base, path)?;
            let text = db.has_file_text(id);
            let content = text.then(|| db.file_text(id).content(db));
            Some((text, content, db.file_source_root(id).source_root_id(db)))
        })
        .collect();
    let roots = [0, 1].map(|root| db.source_root(SourceRootId(root)).content(db));
    (files, roots)
}

#[test]
fn a_view_set_over_others_is_the_view_set_alone() {
    let mut rng = Rng(3);
    let mut db = TestDb::new();
    let (base, partition) = checkout(&mut db);
    let built = [base.join("target")];
    let mut every_path: Vec<String> = Vec::new();
    let mut before: Vec<(String, Vec<u8>)> = Vec::new();
    for step in 0..40 {
        let view = seeded_view(&mut rng);
        let id_of = |db: &TestDb, path: &str| file_at(db, &base, path);
        let ids_before: Vec<(String, Option<FileId>)> =
            before.iter().map(|(path, _)| (path.clone(), id_of(&db, path))).collect();
        let files: Vec<ViewFile<'_>> =
            view.iter().map(|(path, bytes)| ViewFile { path, file: vfs_file(bytes) }).collect();
        let root = ViewRoot { base: &base, built: &built, partition: &partition, other: SourceRootId(1), files: &files };
        let change = db.set_view_root(root).unwrap();

        // What the change says, from the two views alone. A file under `target` is built: one
        // the view lacks stays; one it has is set like any other.
        let old: std::collections::BTreeMap<&str, &[u8]> =
            before.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        let new: std::collections::BTreeMap<&str, &[u8]> =
            view.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        let in_root: std::collections::BTreeSet<&str> = old
            .keys()
            .copied()
            .filter(|path| ids_before.iter().any(|(known, id)| known == path && id.is_some()))
            .collect();
        let mut set = Vec::new();
        let mut added = Vec::new();
        let mut no_text = Vec::new();
        for (&path, &bytes) in &new {
            let id = id_of(&db, path).unwrap();
            let written = match old.get(path) {
                Some(&was) if in_root.contains(path) => {
                    if was != bytes {
                        let both_binary =
                            std::str::from_utf8(was).is_err() && std::str::from_utf8(bytes).is_err();
                        if !both_binary {
                            set.push(id);
                        }
                    }
                    was != bytes
                }
                _ => {
                    added.push(id);
                    true
                }
            };
            if written && std::str::from_utf8(bytes).is_err() {
                no_text.push(id);
            }
        }
        let mut removed: Vec<FileId> = ids_before
            .iter()
            .filter(|(path, _)| !new.contains_key(path.as_str()) && !path.starts_with("target/"))
            .filter_map(|(_, id)| *id)
            .collect();
        for list in [&mut set, &mut added, &mut no_text, &mut removed] {
            list.sort_unstable();
        }
        assert_eq!((change.set, change.added, change.no_text, change.removed), (set, added, no_text, removed), "step {step}");

        // Built files the view lacks stay; take them into the view for the comparison.
        let mut whole = view.clone();
        for (path, bytes) in &before {
            if path.starts_with("target/") && !new.contains_key(path.as_str()) {
                whole.push((path.clone(), bytes.clone()));
            }
        }
        for (path, _) in &whole {
            if !every_path.contains(path) {
                every_path.push(path.clone());
            }
        }
        let mut fresh = TestDb::new();
        let (fresh_base, fresh_partition) = checkout(&mut fresh);
        let fresh_files: Vec<(&str, &[u8])> =
            whole.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        set_view(&mut fresh, &fresh_base, &fresh_partition, &fresh_files).unwrap();
        assert_eq!(
            view_state(&db, &base, &every_path),
            view_state(&fresh, &fresh_base, &every_path),
            "step {step}"
        );
        before = whole;
    }
}

fn two_workers() -> blaze_graph::pool::Pool {
    blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(2), false).unwrap()
}

#[test]
fn a_task_that_waited_on_itself_runs_again() {
    install_plane_hooks();
    install_plane_hooks();
    let db = TestDb::new();
    let pool = two_workers();
    let runs: Vec<AtomicUsize> = (0..8).map(|_| AtomicUsize::new(0)).collect();
    run_on_pool(&db, &pool, runs.len(), &|_: &dyn SourceDatabase, at: usize| {
        if runs[at].fetch_add(1, Ordering::Relaxed) == 0 && at % 3 == 0 {
            std::panic::resume_unwind(Box::new(salsa::Cancelled::WouldWaitOnItself));
        }
    });
    let runs: Vec<usize> = runs.iter().map(|it| it.load(Ordering::Relaxed)).collect();
    assert_eq!(runs, [2, 1, 1, 2, 1, 1, 2, 1]);
}

#[test]
fn any_other_panic_of_a_task_reaches_the_caller() {
    let db = TestDb::new();
    let pool = two_workers();
    let ran = AtomicUsize::new(0);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_on_pool(&db, &pool, 4, &|_: &dyn SourceDatabase, at: usize| {
            ran.fetch_add(1, Ordering::Relaxed);
            if at == 2 {
                std::panic::resume_unwind(Box::new("task 2 failed"));
            }
        });
    }));
    assert_eq!(caught.unwrap_err().downcast_ref::<&str>(), Some(&"task 2 failed"));
    assert_eq!(ran.load(Ordering::Relaxed), 4);
}
