//! Allocation budgets of what every query reads first and of setting a view
//! (docs/v2/plan/lanes/db/speed-of-light.md, "base-db: inputs and views"): a read allocates
//! nothing, a view's unchanged file allocates nothing, and a changed file costs a constant.

use salsa::Durability;
use span::Edition;
use triomphe::Arc;
use vfs::{
    AbsPathBuf, FileId, VfsPath,
    file_set::{FileSet, FileSetConfig},
};

use crate::{
    ContentId, CrateGraphBuilder, CrateOrigin, CrateWorkspaceData, Db, EditionedFileId, Env,
    SourceRoot, SourceRootId, TargetKind, ViewChange, ViewFile, ViewRoot, relevant_crates,
};

const FILES: u32 = 64;

/// One local source root of [`FILES`] files and a crate rooted at the first.
fn loaded() -> (Db, Vec<FileId>) {
    let mut db = Db::for_tests();
    let files: Vec<FileId> = (0..FILES).map(FileId::from_raw).collect();
    let mut set = FileSet::default();
    for &file in &files {
        db.set_file_text_with_durability(file, "fn f() { 1 }\n", Durability::LOW);
        db.set_file_source_root_with_durability(file, SourceRootId(0), Durability::MEDIUM);
        set.insert(file, VfsPath::new_virtual_path(format!("/budget/f{}.rs", file.index())));
    }
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
    let mut graph = CrateGraphBuilder::default();
    graph.add_crate_root(
        files[0],
        Edition::Edition2021,
        None,
        None,
        Default::default(),
        None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in budget tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    );
    graph.set_in_db(&mut db);
    (db, files)
}

/// Every read the brief names, of every file: its text, whether it has text, its source root
/// and the root's file set, its crates and its parse.
fn read_all(db: &Db, files: &[FileId]) -> usize {
    let mut seen = 0;
    for &file in files {
        seen += db.file_text(file).text(db).len();
        seen += usize::from(db.has_file_text(file));
        let root = db.file_source_root(file).source_root_id(db);
        seen += usize::from(db.source_root(root).source_root_ref(db).path_for_file(&file).is_some());
        seen += relevant_crates(db, file).len();
        seen += usize::from(EditionedFileId::new(db, file, Edition::Edition2021).parse(db).is_outline());
    }
    seen
}

#[test]
fn reads_allocate_nothing() {
    let (db, files) = loaded();
    let warm = read_all(&db, &files);
    let (again, used) = blaze_alloc_count::measure(|| read_all(&db, &files));
    assert_eq!(again, warm);
    assert_eq!(used.allocations, 0, "reads of {FILES} files, every memo warm: {used:?}");
}

/// Source roots 0 (the checkout) and 1 (everything else), both empty.
fn checkout() -> (Db, AbsPathBuf, FileSetConfig) {
    let mut db = Db::for_tests();
    let base = AbsPathBuf::assert_utf8(std::env::temp_dir().join("blaze-base-db-budgets"));
    let mut partition = FileSetConfig::builder();
    partition.add_file_set(vec![VfsPath::from(base.clone())]);
    for root in [0, 1] {
        let empty = Arc::new(SourceRoot::new_local(FileSet::default()));
        db.set_source_root_with_durability(SourceRootId(root), empty, Durability::MEDIUM);
    }
    (db, base, partition.build())
}

/// `n` files under `src/`, the first `edited` of them with another body than the rest have.
fn texts(n: usize, edited: usize) -> Vec<(String, Vec<u8>)> {
    (0..n)
        .map(|at| {
            let body = if at < edited { 2 } else { 1 };
            (format!("src/m{at}.rs"), format!("fn f{at}() {{ {body} }}\n").into_bytes())
        })
        .collect()
}

fn view_files(files: &[(String, Vec<u8>)]) -> Vec<ViewFile<'_>> {
    files
        .iter()
        .map(|(path, bytes)| ViewFile {
            path: path.as_str(),
            file: blaze_vfs::File {
                id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes),
                mode: blaze_vfs::Mode::Regular,
                bytes: bytes.as_slice(),
            },
        })
        .collect()
}

/// Sets the view and counts what that alone allocated: the view's list is made before.
fn set_view(
    db: &mut Db,
    base: &AbsPathBuf,
    partition: &FileSetConfig,
    files: &[ViewFile<'_>],
) -> (ViewChange, blaze_alloc_count::Used) {
    blaze_alloc_count::measure(|| {
        db.set_view_root(ViewRoot { base, built: &[], partition, other: SourceRootId(1), files })
            .expect("a view of good paths and matching IDs")
    })
}

#[test]
fn an_unchanged_view_allocates_nothing() {
    for n in [8, 512] {
        let (mut db, base, partition) = checkout();
        let files = texts(n, 0);
        let view = view_files(&files);
        let (first, _) = set_view(&mut db, &base, &partition, &view);
        assert_eq!(first.added.len(), n);
        let (again, used) = set_view(&mut db, &base, &partition, &view);
        assert_eq!(again, ViewChange::default());
        assert_eq!(used.allocations, 0, "a view of {n} unchanged files: {used:?}");
    }
}

#[test]
fn a_changed_file_costs_a_constant() {
    // The call's own batches (plan, byte and text IDs, dirty buckets), whatever it changes.
    const PER_CALL: u64 = 64;
    // The file's `Arc<str>` and its reparse slot, plus each batch's growth.
    const PER_CHANGED_FILE: u64 = 6;
    let cost = |edited: usize| {
        let (mut db, base, partition) = checkout();
        let files = texts(64, 0);
        set_view(&mut db, &base, &partition, &view_files(&files));
        let edits = texts(64, edited);
        let view = view_files(&edits);
        let (change, used) = set_view(&mut db, &base, &partition, &view);
        assert_eq!(change.set.len(), edited);
        used.allocations
    };
    let one = cost(1);
    let many = cost(32);
    assert!(one <= PER_CALL + PER_CHANGED_FILE, "one changed file of 64: {one} allocations");
    assert!(
        many <= one + 31 * PER_CHANGED_FILE,
        "32 changed files of 64: {many} allocations, against {one} for one"
    );
}
