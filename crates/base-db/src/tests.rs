use std::fmt;

use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use salsa::Durability;
use span::{Edition, TextSize};
use triomphe::Arc;
use vfs::{
    AbsPathBuf, FileId, VfsPath,
    file_set::{FileSet, FileSetConfig},
};

use crate::{
    ContentId, Crate, CrateBuilderId, CrateGraphBuilder, CrateName, CrateOrigin,
    CrateWorkspaceData, CratesMap, DependencyBuilder, Env, FileSourceRootInput, FileText, Files,
    Nonce, SourceDatabase, SourceRoot, SourceRootId, SourceRootInput, TargetKind, ViewChange,
    ViewError, ViewFile, ViewRoot, all_crates_content, cfg_options_content,
    set_all_crates_with_durability,
};

#[salsa::db]
#[derive(Clone)]
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
    blaze_vfs::File { id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes), bytes }
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
            file: blaze_vfs::File { id: ContentId::of(blaze_vfs::FILE_DOMAIN, b"other"), bytes },
        },
    ];
    let view =
        ViewRoot { base: &base, built: &[], partition: &partition, other: SourceRootId(1), files: &files };
    let error = db.set_view_root(view).unwrap_err();
    assert!(matches!(&error, ViewError::Mismatch { path, .. } if path == "src/main.rs"));
    assert_eq!(db.revision(), revision);
    assert_eq!(file_at(&db, &base, "src/lib.rs"), None);
}
