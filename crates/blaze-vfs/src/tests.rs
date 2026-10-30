use std::{collections::BTreeMap, time::Duration};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, IoError, Meta, Tag,
    sim::{Faults, Rng, Sim},
};
use blaze_ids::{ContentId, Hasher};
use rustc_hash::FxHashMap;

use crate::{
    Change, ChangeKind, CommitError, DIR_DOMAIN, FILE_DOMAIN, Mode, Op, OpError, Reader, Ticket,
    Vfs, View, Watched, Watcher,
    tree::{DIR_MODE, NodeId, Tree},
};

fn write(path: &str, bytes: &str) -> Op {
    write_as(path, bytes, Mode::Regular)
}

fn write_as(path: &str, bytes: &str, mode: Mode) -> Op {
    Op::Write { path: path.into(), bytes: bytes.as_bytes().to_vec(), mode }
}

fn edit(path: &str, at: u64, len: u64, bytes: &str) -> Op {
    Op::Edit { path: path.into(), at, len, bytes: bytes.as_bytes().to_vec() }
}

fn delete(path: &str) -> Op {
    Op::Delete { path: path.into() }
}

fn rename(from: &str, to: &str) -> Op {
    Op::Rename { from: from.into(), to: to.into() }
}

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn files(tree: &Tree, root: NodeId) -> BTreeMap<String, Vec<u8>> {
    moded(tree, root).into_iter().map(|(path, (_, bytes))| (path, bytes)).collect()
}

fn moded(tree: &Tree, root: NodeId) -> BTreeMap<String, (Mode, Vec<u8>)> {
    tree.files(root, Mode::Regular, "")
        .into_iter()
        .map(|(path, node, mode)| (path, (mode, tree.bytes(node).expect("a file").to_vec())))
        .collect()
}

fn text_at(tree: &Tree, root: NodeId, path: &str) -> Option<String> {
    tree.read(root, path).map(|it| String::from_utf8(it.to_vec()).expect("utf-8"))
}

fn new_vfs() -> Vfs {
    Vfs::recover(name("state/journal"), 0, &[]).0
}

fn contents(vfs: &Vfs, root: ContentId) -> BTreeMap<String, Vec<u8>> {
    files(&vfs.tree, vfs.root(root).expect("a root"))
}

fn text(vfs: &Vfs, root: ContentId, path: &str) -> Option<String> {
    text_at(&vfs.tree, vfs.root(root).expect("a root"), path)
}

/// Runs `effects` and every effect they lead to under `sim`; returns the tickets made durable.
fn settle(sim: &mut Sim, vfs: &mut Vfs, effects: &mut Vec<Effect>) -> Vec<Ticket> {
    vfs.flush(effects);
    let mut durable = Vec::new();
    let mut done = Vec::new();
    while !effects.is_empty() || sim.in_flight() > 0 {
        sim.submit(effects);
        sim.poll(&mut done, Duration::ZERO);
        for completion in done.drain(..) {
            assert!(vfs.owns(completion.tag));
            durable.extend(vfs.complete(completion, effects));
        }
    }
    durable
}

#[test]
fn writes_make_directories_and_read_back() {
    let mut tree = Tree::new();
    let root = tree
        .apply(tree.empty(), &[write("src/lib.rs", "fn a() {}"), write("Cargo.toml", "[package]")])
        .unwrap();
    assert_eq!(text_at(&tree, root, "src/lib.rs").as_deref(), Some("fn a() {}"));
    assert!(tree.is_dir(tree.lookup(root, "src").unwrap()));
    assert_eq!(tree.read(root, "src"), None);
    assert_eq!(tree.lookup(root, "src/main.rs"), None);
    assert_eq!(tree.lookup(root, ""), Some(root));
}

#[test]
fn the_same_files_hash_alike_however_they_were_made() {
    let mut tree = Tree::new();
    let one = tree
        .apply(tree.empty(), &[write("a/b/c", "1"), write("a/d", "2"), write("e", "3")])
        .unwrap();
    let two = tree.apply(tree.empty(), &[write("e", "3"), write("a/d", "2")]).unwrap();
    let two = tree.apply(two, &[write("a/b/c", "1")]).unwrap();
    let three = tree
        .apply(
            tree.empty(),
            &[write("x/y", "0"), write("e", "3"), write("a/b/c", "1"), write("a/d", "2")],
        )
        .unwrap();
    let three = tree.apply(three, &[delete("x")]).unwrap();
    assert_eq!(one, two);
    assert_eq!(one, three);
    assert_eq!(tree.id(one), tree.id(three));

    // A directory and a file with the same name and bytes are not the same tree.
    let file = tree.apply(tree.empty(), &[write("a", "")]).unwrap();
    let dir = tree.apply(tree.empty(), &[write("a/b", "")]).unwrap();
    assert_ne!(tree.id(file), tree.id(dir));
    // A file's ID is the hash of its bytes, under the file domain.
    let a = tree.lookup(file, "a").unwrap();
    assert_eq!(tree.id(a), ContentId::of(FILE_DOMAIN, b""));
}

#[test]
fn a_transaction_applies_whole_or_not_at_all() {
    let mut tree = Tree::new();
    let base = tree.apply(tree.empty(), &[write("a", "1")]).unwrap();
    let nodes = tree.len();
    let error = tree.apply(base, &[write("b", "2"), edit("a", 5, 1, "x")]).unwrap_err();
    assert_eq!(error, OpError::OutOfRange { path: "a".into(), at: 5, len: 1, size: 1 });
    assert_eq!(tree.len(), nodes);
    assert_eq!(files(&tree, base).len(), 1);
}

#[test]
fn ops_see_the_ops_before_them() {
    let mut tree = Tree::new();
    let root = tree
        .apply(
            tree.empty(),
            &[
                write("a", "hello"),
                edit("a", 5, 0, " world"),
                rename("a", "b/c"),
                edit("b/c", 0, 5, "goodbye"),
            ],
        )
        .unwrap();
    assert_eq!(files(&tree, root), BTreeMap::from([("b/c".into(), b"goodbye world".to_vec())]));
}

#[test]
fn deletes_and_renames_of_directories() {
    let mut tree = Tree::new();
    let base = tree
        .apply(tree.empty(), &[write("src/a.rs", "a"), write("src/x/b.rs", "b"), write("top", "t")])
        .unwrap();
    let moved = tree.apply(base, &[rename("src", "lib/src")]).unwrap();
    assert_eq!(text_at(&tree, moved, "lib/src/x/b.rs").as_deref(), Some("b"));
    assert_eq!(tree.lookup(moved, "src"), None);
    // Moving a directory reuses its node.
    assert_eq!(tree.lookup(moved, "lib/src"), tree.lookup(base, "src"));

    let gone = tree.apply(base, &[delete("src/x/b.rs")]).unwrap();
    assert_eq!(tree.lookup(gone, "src/x"), None, "an emptied directory goes");
    let all = tree.apply(base, &[delete("src"), delete("top")]).unwrap();
    assert_eq!(all, tree.empty());

    let nodes = tree.len();
    let mut error = |ops: &[Op]| tree.apply(base, ops).unwrap_err();
    assert_eq!(error(&[rename("src", "src/x/inner")]), OpError::BadPath("src/x/inner".into()));
    assert_eq!(error(&[rename("top", "src")]), OpError::Exists("src".into()));
    assert_eq!(error(&[write("top/a", "")]), OpError::NotADirectory("top/a".into()));
    assert_eq!(error(&[write("src", "")]), OpError::IsADirectory("src".into()));
    assert_eq!(error(&[delete("nope")]), OpError::NotFound("nope".into()));
    assert_eq!(error(&[write("../a", "")]), OpError::BadPath("../a".into()));
    assert_eq!(error(&[write("a//b", "")]), OpError::BadPath("a//b".into()));
    assert_eq!(tree.len(), nodes, "a refused transaction adds no node");
}

#[test]
fn diffs_skip_what_is_shared() {
    let mut tree = Tree::new();
    let base = tree
        .apply(
            tree.empty(),
            &[write("a/b/c", "1"), write("a/d", "2"), write("e", "3"), write("f", "4")],
        )
        .unwrap();
    let next = tree
        .apply(
            base,
            &[edit("a/b/c", 0, 1, "9"), delete("e"), write("g/h", "5"), rename("f", "a/b")],
        )
        .unwrap_err();
    assert_eq!(next, OpError::Exists("a/b".into()));
    let next = tree
        .apply(
            base,
            &[
                edit("a/b/c", 0, 1, "9"),
                delete("e"),
                write("g/h", "5"),
                delete("f"),
                write("f/i", "6"),
            ],
        )
        .unwrap();
    let change = |path: &str, kind| Change { path: path.into(), kind };
    assert_eq!(
        tree.diff(base, next),
        vec![
            change("a/b/c", ChangeKind::Modified),
            change("e", ChangeKind::Removed),
            change("f", ChangeKind::Removed),
            change("f/i", ChangeKind::Added),
            change("g/h", ChangeKind::Added),
        ]
    );
    assert_eq!(tree.diff(next, next), vec![]);

    // A directory replaced by a file: the file first, then what was under it.
    let back = tree.apply(next, &[delete("f/i"), write("f", "4")]).unwrap();
    assert_eq!(
        tree.diff(next, back),
        vec![change("f", ChangeKind::Added), change("f/i", ChangeKind::Removed)]
    );
}

#[test]
fn diffs_are_in_path_order_by_component() {
    let mut tree = Tree::new();
    let base = tree.apply(tree.empty(), &[write("a/x", "1"), write("a.b", "2")]).unwrap();
    let next = tree.apply(base, &[write("a/x", "3"), write("a.b", "4")]).unwrap();
    let paths: Vec<String> = tree.diff(base, next).into_iter().map(|it| it.path).collect();
    assert_eq!(paths, ["a/x", "a.b"]);
    assert_eq!(
        tree.files(next, Mode::Regular, "").into_iter().map(|it| it.0).collect::<Vec<_>>(),
        paths
    );
}

#[test]
fn a_stale_transaction_rebases_or_names_the_conflict() {
    let mut vfs = new_vfs();
    assert_eq!(vfs.head("w"), None);
    let base = vfs.commit("w", vfs.empty(), &[write("a", "0123456789"), write("b", "b")]).unwrap();
    assert!(!base.rebased);
    let base = base.root;

    // Another agent edits the start of `a` and adds `c`.
    vfs.commit("w", base, &[edit("a", 0, 2, "AAAA"), write("c", "c")]).unwrap();

    // An edit of the end of `a` made against the old base moves past it.
    let commit = vfs.commit("w", base, &[edit("a", 8, 2, "zz"), write("b", "B")]).unwrap();
    assert!(commit.rebased);
    assert_eq!(text(&vfs, commit.root, "a").as_deref(), Some("AAAA234567zz"));
    assert_eq!(text(&vfs, commit.root, "b").as_deref(), Some("B"));
    assert_eq!(text(&vfs, commit.root, "c").as_deref(), Some("c"));

    // One overlapping the change, or replacing a changed file, is refused with the path.
    let head = vfs.head("w");
    assert_eq!(
        vfs.commit("w", base, &[edit("a", 1, 2, "x")]),
        Err(CommitError::Conflict { path: "a".into() })
    );
    assert_eq!(
        vfs.commit("w", base, &[write("a", "x")]),
        Err(CommitError::Conflict { path: "a".into() })
    );
    assert_eq!(
        vfs.commit("w", base, &[write("c", "x")]),
        Err(CommitError::Conflict { path: "c".into() })
    );
    assert_eq!(vfs.head("w"), head);

    let unknown = ContentId([7; 32]);
    assert_eq!(vfs.commit("w", unknown, &[]), Err(CommitError::UnknownBase(unknown)));
    // A file's ID is not a root.
    let file = ContentId::of(FILE_DOMAIN, b"B");
    assert!(!vfs.has(file));
    assert_eq!(vfs.commit("w", file, &[]), Err(CommitError::UnknownBase(file)));
    assert_eq!(
        vfs.commit("w", head.unwrap(), &[delete("nope")]),
        Err(CommitError::Op(OpError::NotFound("nope".into())))
    );
}

#[test]
fn views_read_share_and_outlive_collection_until_closed() {
    let mut vfs = new_vfs();
    let root = vfs
        .commit(
            "main",
            vfs.empty(),
            &[write("src/lib.rs", "x"), write("src/a/b.rs", "y"), write("README", "r")],
        )
        .unwrap()
        .root;
    let main = vfs.open(View { root }).unwrap();
    assert_eq!(vfs.view(main), View { root });
    let file = vfs.read(main, "src/lib.rs").unwrap();
    assert_eq!(file.bytes, b"x");
    assert_eq!(file.id, ContentId::of(FILE_DOMAIN, b"x"));
    assert_eq!(vfs.read(main, "src"), None);
    assert_eq!(vfs.read(main, "nope"), None);
    assert_eq!(vfs.lookup(main, ""), Some(root));
    assert_eq!(vfs.lookup(main, "src/lib.rs"), Some(file.id));
    assert_eq!(vfs.list(main, ""), Some(vec![("README".into(), false), ("src".into(), true)]));
    assert_eq!(vfs.list(main, "src"), Some(vec![("a".into(), true), ("lib.rs".into(), false)]));
    assert_eq!(vfs.list(main, "README"), None);

    // A PR's what-if: ops on the head's root, moving no head.
    let pr = vfs.apply(root, &[edit("src/lib.rs", 0, 1, "z"), delete("README")]).unwrap();
    assert_eq!(vfs.head("main"), Some(root));
    let view = vfs.open(View { root: pr }).unwrap();
    assert_ne!(view, main);
    assert_eq!(vfs.read(view, "src/lib.rs").unwrap().bytes, b"z");
    assert_eq!(vfs.lookup(view, "src/a"), vfs.lookup(main, "src/a"), "a subtree is shared");
    assert_eq!(
        vfs.diff(root, pr),
        vec![
            Change { path: "README".into(), kind: ChangeKind::Removed },
            Change { path: "src/lib.rs".into(), kind: ChangeKind::Modified },
        ]
    );

    // Collection keeps what open views and `keep` reach, and drops the rest.
    let ci = vfs.apply(root, &[write("ci-only", "1")]).unwrap();
    let kept = vfs.apply(root, &[write("kept", "1")]).unwrap();
    vfs.collect(&[kept]);
    assert!(!vfs.has(ci));
    assert!(vfs.has(pr) && vfs.has(kept) && vfs.has(root));
    assert_eq!(vfs.read(view, "src/lib.rs").unwrap().bytes, b"z");
    assert_eq!(vfs.view(view), View { root: pr });
    vfs.close(view);
    vfs.collect(&[]);
    assert!(!vfs.has(pr) && !vfs.has(kept));
    assert_eq!(vfs.read(main, "src/a/b.rs").unwrap().bytes, b"y");

    // A closed view's number is not given again, and a root that is gone does not open.
    let again = vfs.open(View { root }).unwrap();
    assert!(again != view && again != main);
    assert_eq!(vfs.open(View { root: pr }), Err(CommitError::UnknownBase(pr)));
    assert_eq!(vfs.apply(pr, &[]), Err(OpError::UnknownRoot(pr)));
}

#[test]
#[should_panic(expected = "is not open")]
fn a_closed_view_is_not_read() {
    let mut vfs = new_vfs();
    let view = vfs.open(View { root: vfs.empty() }).unwrap();
    vfs.close(view);
    let _ = vfs.read(view, "a");
}

#[test]
fn branches_and_prs_are_heads_that_survive_recovery() {
    let file = name("state/journal");
    let mut sim = Sim::new(3, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let main = vfs.commit("main", vfs.empty(), &[write("a", "1")]).unwrap().root;
    vfs.set_head("branch/x", main).unwrap();
    let pr = vfs.commit("branch/x", main, &[write("b", "2")]).unwrap().root;
    vfs.set_head("pr/7", pr).unwrap();
    assert!(vfs.remove_head("branch/x").is_some());
    assert_eq!(vfs.remove_head("branch/x"), None);
    let unknown = ContentId([1; 32]);
    assert_eq!(vfs.set_head("x", unknown), Err(CommitError::UnknownBase(unknown)));
    assert_eq!(vfs.heads(), vec![("main", main), ("pr/7", pr)]);
    assert_eq!(settle(&mut sim, &mut vfs, &mut Vec::new()).len(), 5);

    let (back, found) = Vfs::recover(file.clone(), 0, &sim.contents(&file));
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    assert_eq!(back.heads(), vec![("main", main), ("pr/7", pr)]);
    assert_eq!(back.head("branch/x"), None);
    assert!(back.sequence("pr/7") > back.sequence("main"));
    assert_eq!(contents(&back, pr), contents(&vfs, pr));
}

#[test]
fn collecting_keeps_what_is_reached() {
    let mut vfs = new_vfs();
    let mut root = vfs.empty();
    for at in 0..50 {
        root = vfs
            .commit("w", root, &[write(&format!("d{}/f{at}", at % 4), &at.to_string())])
            .unwrap()
            .root;
    }
    let what_if = vfs.apply(root, &[write("only/here", "x")]).unwrap();
    let before = (contents(&vfs, root), contents(&vfs, what_if));
    let size = vfs.tree.len();
    vfs.collect(&[what_if]);
    assert!(vfs.tree.len() < size / 2, "{} of {size} nodes left", vfs.tree.len());
    assert_eq!((contents(&vfs, root), contents(&vfs, what_if)), before);
    assert_eq!(vfs.head("w"), Some(root));
    // The tree still works after renumbering.
    let next = vfs.commit("w", root, &[write("d0/f0", "new")]).unwrap().root;
    assert_eq!(text(&vfs, next, "d0/f0").as_deref(), Some("new"));
}

type Model = BTreeMap<String, (Mode, Vec<u8>)>;

/// The old layout as a model: random transactions against a map from path to mode and bytes,
/// with each root's ID hashed from the map alone, entry by entry, as the node table before the
/// column store did. Paths with `-` and `.` sort between a name and the same name with `/`, so
/// a component's changes are not one run of the sorted paths. Every few transactions a
/// collection renumbers the table; diffs, the reader and IDs are checked against the model
/// after each.
#[test]
fn transactions_match_a_model() {
    const PATHS: &[&str] =
        &["a", "a.b", "a-c", "b", "d", "d/a", "d/b", "d.x", "d/e/a", "d/e.f", "f/g/h", "f-g"];
    const DIRS: &[&str] = &["d", "d/e", "f", "f/g"];
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut vfs = new_vfs();
        let reader = vfs.reader();
        let mut root = vfs.empty();
        let mut model = Model::new();
        let mut kept: Vec<(ContentId, Model)> = Vec::new();
        for step in 0..40 {
            let mut ops = Vec::new();
            let mut next = model.clone();
            for _ in 0..=rng.below(4) {
                let path = PATHS[rng.below(PATHS.len() as u64) as usize];
                let bytes = format!("{seed}-{}", rng.below(1000)).into_bytes();
                let op = match rng.below(5) {
                    0 | 1 => {
                        let mode =
                            [Mode::Regular, Mode::Executable, Mode::Symlink][rng.below(3) as usize];
                        Op::Write { path: path.into(), bytes, mode }
                    }
                    2 => match next.get(path) {
                        Some((_, old)) => {
                            let at = rng.below(old.len() as u64 + 1);
                            let len = rng.below(old.len() as u64 - at + 1);
                            Op::Edit { path: path.into(), at, len, bytes }
                        }
                        None => continue,
                    },
                    3 if rng.one_in(2) => delete(DIRS[rng.below(DIRS.len() as u64) as usize]),
                    3 => delete(path),
                    _ => match next.keys().nth(rng.below(next.len() as u64 + 1) as usize) {
                        Some(from) => rename(from, path),
                        None => continue,
                    },
                };
                // Only ops the tree takes: none puts a file where a directory is.
                if !model_apply(&mut next, &op) {
                    continue;
                }
                ops.push(op);
            }
            let after = vfs
                .apply(root, &ops)
                .unwrap_or_else(|error| panic!("seed {seed}: {error} for {ops:?}"));
            assert_eq!(after, model_id(&next), "seed {seed}: the root's ID for {ops:?}");
            let paths: Vec<String> = vfs.diff(root, after).into_iter().map(|it| it.path).collect();
            assert_eq!(paths, model_diff(&model, &next), "seed {seed}: the diff of {ops:?}");
            (root, model) = (after, next);
            assert_eq!(moded(&vfs.tree, vfs.root(root).unwrap()), model, "seed {seed}");
            for (path, (mode, bytes)) in &model {
                let read = reader.file(root, path).map(|(_, mode, bytes)| (mode, bytes.to_vec()));
                assert_eq!(read, Some((*mode, bytes.clone())), "seed {seed}: {path}");
            }
            if step % 2 == 0 {
                kept.push((root, model.clone()));
            }
            if step % 7 == 6 {
                kept.drain(..kept.len().saturating_sub(2));
                let keep: Vec<ContentId> = kept.iter().map(|it| it.0).chain([root]).collect();
                vfs.collect(&keep);
                for (id, files) in &kept {
                    assert_eq!(&moded(&vfs.tree, vfs.root(*id).unwrap()), files, "seed {seed}");
                }
            }
        }
    }
}

fn model_apply(model: &mut Model, op: &Op) -> bool {
    let under = |path: &str| format!("{path}/");
    let clashes = |model: &Model, path: &str| {
        model.keys().any(|it| it.starts_with(&under(path)) || path.starts_with(&under(it)))
    };
    match op {
        Op::Write { path, bytes, mode } => {
            if clashes(model, path) {
                return false;
            }
            model.insert(path.clone(), (*mode, bytes.clone()));
        }
        Op::Edit { path, at, len, bytes } => {
            let (_, old) = model.get_mut(path).expect("edits name a file");
            old.splice(*at as usize..(at + len) as usize, bytes.iter().copied());
        }
        Op::Delete { path } => {
            let before = model.len();
            model.retain(|it, _| it != path && !it.starts_with(&under(path)));
            return model.len() < before;
        }
        Op::Rename { from, to } => {
            if from == to || to.starts_with(&under(from)) || model.contains_key(to) {
                return false;
            }
            let Some(file) = model.remove(from) else { return false };
            if clashes(model, to) {
                model.insert(from.clone(), file);
                return false;
            }
            model.insert(to.clone(), file);
        }
    }
    true
}

enum ModelNode<'a> {
    File(Mode, &'a [u8]),
    Dir(BTreeMap<&'a str, ModelNode<'a>>),
}

/// A root's ID from the model alone: each directory hashed entry by entry with an incremental
/// hasher (git's code, length-prefixed name, child ID), as the old node table did.
fn model_id(model: &Model) -> ContentId {
    let mut root = BTreeMap::new();
    for (path, (mode, bytes)) in model {
        let mut names: Vec<&str> = path.split('/').collect();
        let file = names.pop().unwrap();
        let mut dir = &mut root;
        for name in names {
            let ModelNode::Dir(next) =
                dir.entry(name).or_insert_with(|| ModelNode::Dir(BTreeMap::new()))
            else {
                panic!("{path} under a file")
            };
            dir = next;
        }
        dir.insert(file, ModelNode::File(*mode, bytes));
    }
    fn hash(node: &ModelNode<'_>) -> (u32, ContentId) {
        match node {
            ModelNode::File(mode, bytes) => (mode.git(), ContentId::of(FILE_DOMAIN, bytes)),
            ModelNode::Dir(entries) => {
                let mut hasher = Hasher::new(DIR_DOMAIN);
                for (name, child) in entries {
                    let (code, id) = hash(child);
                    hasher.update(&code.to_le_bytes()).part(name.as_bytes()).id(&id);
                }
                (DIR_MODE, hasher.finish())
            }
        }
    }
    hash(&ModelNode::Dir(root)).1
}

/// The paths a diff names, in path order by component: each file added, removed, or changed
/// in bytes or mode.
fn model_diff(old: &Model, new: &Model) -> Vec<String> {
    let mut paths: Vec<&String> =
        old.keys().chain(new.keys()).filter(|it| old.get(*it) != new.get(*it)).collect();
    paths.sort_unstable_by(|a, b| a.split('/').cmp(b.split('/')));
    paths.dedup();
    paths.into_iter().cloned().collect()
}

/// A directory's changes that sort apart as strings ("a", then "a.b", then "a/y") are one
/// group by component: the delete of `a` and the write under it are applied together.
#[test]
fn a_directory_deleted_and_written_under_again_keeps_its_dotted_sibling() {
    let mut tree = Tree::new();
    let base = tree
        .apply(tree.empty(), &[write("a/x", "1"), write("a-c/z", "0"), write("b", "2")])
        .unwrap();
    let next = tree
        .apply(base, &[delete("a"), write("a.b", "4"), write("a/y", "5"), write("a-c/w", "6")])
        .unwrap();
    let want = [("a-c/w", "6"), ("a-c/z", "0"), ("a.b", "4"), ("a/y", "5"), ("b", "2")];
    assert_eq!(
        files(&tree, next),
        want.into_iter()
            .map(|(path, bytes)| (path.to_owned(), bytes.as_bytes().to_vec()))
            .collect()
    );
}

/// Readers on several threads, each pinned to read a root whole, while the writer commits and
/// collects: every root a reader finds reads complete and self-consistent, and one a
/// collection dropped reads as none, never as part of a tree.
#[cfg(not(blaze_check))] // many threads, long loops
#[test]
fn readers_on_many_threads_see_whole_roots_while_commits_and_collections_run() {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    let mut vfs = new_vfs();
    let reader = vfs.reader();
    let published = Mutex::new(Vec::<ContentId>::new());
    let finished = AtomicUsize::new(0);
    const READERS: usize = 4;
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..READERS as u64)
            .map(|seed| {
                let (reader, published, finished) = (reader.clone(), &published, &finished);
                scope.spawn(move || {
                    let mut rng = Rng::new(seed);
                    let (mut whole, mut gone) = (0, 0);
                    while whole + gone < 3000 {
                        let root = {
                            let roots = published.lock().unwrap();
                            let len = roots.len() as u64;
                            // The latest half the time: the head keeps it, so most reads find
                            // one.
                            let at =
                                if rng.one_in(2) { len.saturating_sub(1) } else { rng.below(len) };
                            roots.get(at as usize).copied()
                        };
                        let Some(root) = root else {
                            std::thread::yield_now();
                            continue;
                        };
                        let pinned = reader.pin();
                        let a = pinned.file(root, "a").map(|it| it.bytes.to_vec());
                        let b = pinned.file(root, "d/e/b").map(|it| it.bytes.to_vec());
                        let listed = pinned.list(root, "d").map(Iterator::count);
                        match (a, b, listed) {
                            (Some(a), Some(b), Some(2)) => {
                                assert_eq!(a, b, "one root's files are one commit's");
                                whole += 1;
                            }
                            (None, None, None) => gone += 1,
                            other => panic!("part of a root: {other:?}"),
                        }
                    }
                    finished.fetch_add(1, Ordering::Release);
                    whole
                })
            })
            .collect();
        let mut root = vfs.empty();
        let mut at = 0u64;
        while finished.load(Ordering::Acquire) < READERS || at < 300 {
            let bytes = at.to_string();
            let ops = [
                write("a", &bytes),
                write("d/e/b", &bytes),
                write("d/c", "c"),
                write(&format!("x/{}", at % 8), &bytes),
            ];
            root = vfs.commit("w", root, &ops).unwrap().root;
            published.lock().unwrap().push(root);
            if at % 25 == 24 {
                let keep = {
                    let roots = published.lock().unwrap();
                    roots[roots.len().saturating_sub(3)..].to_vec()
                };
                vfs.collect(&keep);
            }
            at += 1;
        }
        for reading in readers {
            assert!(reading.join().unwrap() > 0, "a reader found a whole root");
        }
    });
    vfs.collect(&[]);
    let head = vfs.head("w").unwrap();
    let read = reader.pin().file(head, "d/c").map(|it| it.bytes.to_vec());
    assert_eq!(read, Some(b"c".to_vec()));
}

fn listed(reader: &Reader, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
    Some(reader.list(root, path)?.collect())
}

/// What a pinned read lends, owned, to compare with [`Reader::file`].
fn pinned_file(reader: &Reader, root: ContentId, path: &str) -> Option<(ContentId, Mode, Vec<u8>)> {
    reader.pin().file(root, path).map(|it| (it.id, it.mode, it.bytes.to_vec()))
}

fn pinned_list(reader: &Reader, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
    Some(reader.pin().list(root, path)?.map(|(name, dir)| (name.to_owned(), dir)).collect())
}

#[cfg(not(blaze_check))] // many threads, long loops
#[test]
fn the_reader_reads_every_root_as_the_vfs_does() {
    let mut vfs = new_vfs();
    let reader = vfs.reader();
    let empty = vfs.empty();
    assert_eq!(
        listed(&reader, empty, ""),
        Some(Vec::new()),
        "the empty root is there from the start"
    );
    let ops = [
        write("src/lib.rs", "fn a() {}"),
        write_as("run.sh", "echo", Mode::Executable),
        write_as("link", "run.sh", Mode::Symlink),
    ];
    let root = vfs.commit("w", empty, &ops).unwrap().root;
    let view = vfs.open(View { root }).unwrap();
    for path in ["src/lib.rs", "run.sh", "link", "src", "", "nope", "src/lib.rs/x", "a//b", "../x"]
    {
        let file = vfs.read(view, path).map(|it| (it.id, it.mode, it.bytes.to_vec()));
        let read = reader.file(root, path).map(|(id, mode, bytes)| (id, mode, bytes.to_vec()));
        assert_eq!(read, file, "{path}");
        assert_eq!(pinned_file(&reader, root, path), file, "{path}");
    }
    for path in ["", "src", "run.sh", "nope"] {
        assert_eq!(listed(&reader, root, path), vfs.list(view, path), "{path}");
        assert_eq!(pinned_list(&reader, root, path), vfs.list(view, path), "{path}");
    }
    // A read lends the tree's own bytes: no copy, whichever handle reads them.
    let lent = reader.file(root, "run.sh").unwrap().2;
    assert!(std::ptr::eq(lent.as_ptr(), vfs.read(view, "run.sh").unwrap().bytes.as_ptr()));
    let file_id = vfs.lookup(view, "run.sh").unwrap();
    assert_eq!(listed(&reader, file_id, ""), None, "a file is no root");
    assert_eq!(reader.file(file_id, ""), None);

    // A what-if is readable as soon as `apply` gives its ID. Readers on other threads go on
    // while the table grows and collections swap levels under them; what a collection keeps
    // reads the same throughout.
    let what_if = vfs.apply(root, &[edit("src/lib.rs", 0, 2, "pub fn")]).unwrap();
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let reader = reader.clone();
                scope.spawn(move || {
                    (0..2000).all(|_| {
                        reader
                            .file(what_if, "src/lib.rs")
                            .is_some_and(|it| *it.2 == *b"pub fn a() {}")
                    })
                })
            })
            .collect();
        for at in 0..200 {
            let head = vfs.head("w").unwrap();
            vfs.commit("w", head, &[write(&format!("f{at}"), "x")]).unwrap();
            if at % 50 == 49 {
                vfs.collect(&[what_if]);
            }
        }
        for reading in readers {
            assert!(reading.join().unwrap());
        }
    });

    // Collection takes from the reader what it takes from the tree.
    vfs.close(view);
    vfs.remove_head("w");
    vfs.collect(&[what_if]);
    assert!(vfs.has(what_if) && !vfs.has(root));
    assert_eq!(reader.file(root, "run.sh"), None);
    assert_eq!(listed(&reader, root, ""), None);
    assert_eq!(reader.file(what_if, "run.sh").map(|it| it.1), Some(Mode::Executable));
    let next = vfs.commit("w", empty, &[write("after", "1")]).unwrap().root;
    assert_eq!(reader.file(next, "after").map(|it| it.2.to_vec()), Some(b"1".to_vec()));

    // A reader outlives its `Vfs`: what was retired and not reclaimed goes with the last
    // handle, and what the table holds still reads.
    vfs.collect(&[]);
    drop(vfs);
    assert_eq!(reader.file(what_if, "run.sh"), None);
    assert_eq!(reader.file(next, "after").map(|it| it.2.to_vec()), Some(b"1".to_vec()));
    assert_eq!(listed(&reader, empty, ""), Some(Vec::new()));
}

#[test]
fn modes_are_git_modes() {
    assert_eq!(
        [Mode::Regular, Mode::Executable, Mode::Symlink].map(Mode::git),
        [0o100644, 0o100755, 0o120000]
    );
    for mode in [Mode::Regular, Mode::Executable, Mode::Symlink] {
        assert_eq!(Mode::from_git(mode.git()), Some(mode));
    }
    assert_eq!(Mode::from_git(0o100664), Some(Mode::Regular), "git's old group-writable mode");
    assert_eq!(Mode::from_git(0o040000), None, "a directory is no file");
    assert_eq!(Mode::from_git(0o160000), None, "a submodule is no file");
}

#[test]
fn a_mode_is_in_the_tree_not_in_the_file() {
    let mut vfs = new_vfs();
    let plain = vfs.commit("w", vfs.empty(), &[write("run.sh", "echo")]).unwrap().root;
    let exec =
        vfs.commit("w", plain, &[write_as("run.sh", "echo", Mode::Executable)]).unwrap().root;
    assert_ne!(plain, exec, "a chmod is a new root");
    assert_eq!(
        vfs.diff(plain, exec),
        vec![Change { path: "run.sh".into(), kind: ChangeKind::Modified }]
    );
    let (a, b) = (vfs.open(View { root: plain }).unwrap(), vfs.open(View { root: exec }).unwrap());
    let (a, b) = (vfs.read(a, "run.sh").unwrap(), vfs.read(b, "run.sh").unwrap());
    assert_eq!((a.mode, b.mode), (Mode::Regular, Mode::Executable));
    assert_eq!(a.id, b.id, "the same bytes are the same file");

    // Edits and renames keep the mode; a write sets it.
    let mut tree = Tree::new();
    let root = tree
        .apply(
            tree.empty(),
            &[
                write_as("bin/run", "a", Mode::Executable),
                edit("bin/run", 1, 0, "b"),
                rename("bin", "tools"),
            ],
        )
        .unwrap();
    assert_eq!(
        moded(&tree, root),
        BTreeMap::from([("tools/run".into(), (Mode::Executable, b"ab".to_vec()))])
    );
    let back = tree.apply(root, &[write("tools/run", "ab")]).unwrap();
    assert_eq!(moded(&tree, back)["tools/run"].0, Mode::Regular);
    let again = tree.apply(back, &[write_as("tools/run", "ab", Mode::Executable)]).unwrap();
    assert_eq!(again, root, "the same files and modes hash alike");
}

#[test]
fn a_symlink_reads_as_its_target_and_follows_to_its_file() {
    let mut vfs = new_vfs();
    let root = vfs
        .commit(
            "w",
            vfs.empty(),
            &[
                write("src/lib.rs", "fn a() {}"),
                write_as("lib.rs", "src/lib.rs", Mode::Symlink),
                write_as("src/up", "../lib.rs", Mode::Symlink),
                write_as("dir", "src", Mode::Symlink),
                write_as("out", "../x", Mode::Symlink),
                write_as("abs", "/etc/passwd", Mode::Symlink),
                write_as("loop", "loop", Mode::Symlink),
                write_as("dangling", "nope", Mode::Symlink),
            ],
        )
        .unwrap()
        .root;
    let view = vfs.open(View { root }).unwrap();
    let link = vfs.read(view, "lib.rs").unwrap();
    assert_eq!((link.mode, link.bytes), (Mode::Symlink, &b"src/lib.rs"[..]));

    let target = vfs.read(view, "src/lib.rs").unwrap();
    for path in ["lib.rs", "src/up", "dir/lib.rs", "src/lib.rs"] {
        assert_eq!(vfs.follow(view, path), Ok(Some(target)), "{path}");
    }
    assert_eq!(vfs.follow(view, "dir"), Ok(None), "a directory");
    assert_eq!(vfs.follow(view, "dangling"), Ok(None));
    for path in ["out", "abs", "loop"] {
        assert!(matches!(vfs.follow(view, path), Err(OpError::BadLink { .. })), "{path}");
    }
}

#[test]
fn the_journal_keeps_modes() {
    let file = name("j");
    let mut sim = Sim::new(3, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let ops = [
        write("a", "1"),
        write_as("bin/run", "1", Mode::Executable),
        write_as("link", "a", Mode::Symlink),
    ];
    let root = vfs.commit("w", vfs.empty(), &ops).unwrap().root;
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let (recovered, found) = Vfs::recover(file, 0, &sim.contents(&name("j")));
    assert_eq!(found.skipped, 0);
    assert_eq!(recovered.head("w"), Some(root));
    let modes: Vec<(String, Mode)> = moded(&recovered.tree, recovered.root(root).unwrap())
        .into_iter()
        .map(|(path, (mode, _))| (path, mode))
        .collect();
    let want = [("a", Mode::Regular), ("bin/run", Mode::Executable), ("link", Mode::Symlink)];
    assert_eq!(modes, want.map(|(path, mode)| (path.to_owned(), mode)));
}

/// One run of the journal under the simulated machine: commits to two heads, some made against
/// a stale base, heads moved and removed, injected IO failures, collections (and the rewrites
/// they ask for), and power cuts. After each cut the recovered heads must hold every
/// acknowledged record, each head must be a whole move, and its files must be the ones that
/// move left.
fn journal_run(seed: u64) {
    let file = name("vfs/journal");
    let mut rng = Rng::new(seed ^ 0x5eed);
    let faults = Faults {
        fail_one_in: 1 + rng.below(20) as u32,
        max_latency: Duration::from_micros(300),
        ..Faults::default()
    };
    let mut sim = Sim::new(seed, faults);
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let mut effects = Vec::new();
    let mut done = Vec::new();
    // Each head move's sequence number, and the head and files it moved to.
    let mut moves: FxHashMap<u64, (String, BTreeMap<String, Vec<u8>>)> = FxHashMap::default();
    // Each ticket's head, sequence and whether it removed the head.
    let mut tickets: FxHashMap<Ticket, (String, u64, bool)> = FxHashMap::default();
    // By head: the latest record acknowledged durable, and whether it was a removal.
    let mut acknowledged: FxHashMap<String, (u64, bool)> = FxHashMap::default();
    let mut old_bases: Vec<ContentId> = Vec::new();

    for step in 0..300 {
        match rng.below(100) {
            0..=51 => {
                let head = if rng.one_in(3) { "b" } else { "a" };
                let at = vfs.head(head).unwrap_or(vfs.empty());
                let base = match old_bases.get(rng.below(old_bases.len() as u64 * 4) as usize) {
                    Some(&old) if vfs.has(old) => old,
                    _ => at,
                };
                let base_node = vfs.root(base).unwrap();
                let mut ops = Vec::new();
                for _ in 0..=rng.below(3) {
                    let path = format!(
                        "{}/f{}",
                        ["src", "src/x", "tests"][rng.below(3) as usize],
                        rng.below(6)
                    );
                    let bytes = format!("{seed}:{step}:{}", rng.below(1 << 20));
                    ops.push(match (rng.below(4), vfs.tree.read(base_node, &path)) {
                        (0, Some(_)) => delete(&path),
                        (1, Some(old)) => edit(
                            &path,
                            old.len() as u64 / 2,
                            rng.below(old.len() as u64 / 2 + 1),
                            &bytes,
                        ),
                        _ => write(&path, &bytes),
                    });
                }
                match vfs.commit(head, base, &ops) {
                    Ok(commit) => {
                        let sequence = vfs.sequence(head).unwrap();
                        moves.insert(sequence, (head.into(), contents(&vfs, commit.root)));
                        tickets.insert(commit.ticket, (head.into(), sequence, false));
                        old_bases.push(commit.root);
                    }
                    Err(CommitError::Conflict { .. } | CommitError::Op(_)) => {}
                    Err(error) => panic!("seed {seed}: {error}"),
                }
                vfs.flush(&mut effects);
            }
            52..=55 => {
                // A branch made from the other, or removed.
                let (from, to) = if rng.one_in(2) { ("a", "b") } else { ("b", "a") };
                if rng.one_in(2) {
                    if let Some(ticket) = vfs.remove_head(to) {
                        tickets.insert(ticket, (to.into(), vfs.journal.last_sequence(), true));
                    }
                } else if let Some(root) = vfs.head(from) {
                    let ticket = vfs.set_head(to, root).unwrap();
                    let sequence = vfs.sequence(to).unwrap();
                    moves.insert(sequence, (to.into(), contents(&vfs, root)));
                    tickets.insert(ticket, (to.into(), sequence, false));
                }
                vfs.flush(&mut effects);
            }
            56..=93 => {
                sim.submit(&mut effects);
                sim.poll(&mut done, Duration::ZERO);
                for completion in done.drain(..) {
                    for ticket in vfs.complete(completion, &mut effects) {
                        let (head, sequence, removal) =
                            tickets.remove(&ticket).expect("a ticket handed out");
                        let at = acknowledged.entry(head).or_insert((0, false));
                        if sequence > at.0 {
                            *at = (sequence, removal);
                        }
                    }
                }
            }
            94..=96 => {
                vfs.collect(&[]);
                old_bases.retain(|it| vfs.has(*it));
                vfs.flush(&mut effects);
            }
            _ => {
                sim.submit(&mut effects);
                sim.crash();
                effects.clear();
                tickets.clear();
                old_bases.clear();
                vfs = Vfs::recover(file.clone(), 0, &sim.contents(&file)).0;
                for (head, root) in vfs.heads() {
                    let sequence = vfs.sequence(head).unwrap();
                    let (by, files) = moves.get(&sequence).unwrap_or_else(|| {
                        panic!("seed {seed}: head {sequence} was never moved to")
                    });
                    assert_eq!(by, head, "seed {seed}: head {sequence}");
                    assert_eq!(&contents(&vfs, root), files, "seed {seed}: head {sequence}");
                }
                for (head, &(sequence, removal)) in &acknowledged {
                    match vfs.sequence(head) {
                        Some(recovered) => assert!(
                            recovered > sequence || (recovered == sequence && !removal),
                            "seed {seed}: {head} acknowledged {sequence}, recovered {recovered}"
                        ),
                        None => assert!(
                            removal,
                            "seed {seed}: {head} acknowledged {sequence}, recovered nothing"
                        ),
                    }
                }
                // Sequences after the cut may reuse numbers of moves it lost.
                let last = vfs.journal.last_sequence();
                moves.retain(|&sequence, _| sequence <= last);
            }
        }
    }
}

#[test]
fn the_journal_survives_crashes_and_failures() {
    let seeds =
        std::env::var("BLAZE_DST_SEEDS").ok().and_then(|it| it.parse().ok()).unwrap_or(2000u64);
    let first = std::env::var("BLAZE_DST_SEED").ok().and_then(|it| it.parse().ok()).unwrap_or(0u64);
    for seed in first..first + seeds {
        journal_run(seed);
    }
}

/// A torn record left at the end must be cut off before the next append: followed by new records
/// it could borrow their first bytes for its checksum and hide them.
#[test]
fn a_recovered_journal_cuts_its_torn_tail_before_appending() {
    let file = name("j");
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    vfs.commit("w", vfs.empty(), &[write("a", "1")]).unwrap();
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    let Some(Effect::Append { bytes, .. }) = effects.pop() else { panic!("an append") };
    let torn = &bytes[..bytes.len() - 1];

    let (mut vfs, found) = Vfs::recover(file, 0, torn);
    assert_eq!(found.heads_dropped, 0);
    assert!(found.skipped > 0);
    let valid = (torn.len() - found.skipped) as u64;
    let base = vfs.head("w").unwrap_or(vfs.empty());
    vfs.commit("w", base, &[write("b", "2")]).unwrap();
    vfs.flush(&mut effects);
    assert!(
        matches!(effects.as_slice(), [Effect::Truncate { len, .. }] if *len == valid),
        "{effects:?}"
    );
}

#[test]
fn the_journal_round_trips_without_faults() {
    let file = name("j");
    let mut sim = Sim::new(1, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 10, &[]).0;
    let empty = vfs.empty();
    let one = vfs.commit("w", empty, &[write("a/b", "1"), write("c", "2")]).unwrap();
    let two = vfs.commit("w", one.root, &[edit("c", 0, 1, "3")]).unwrap();
    let three = vfs.commit("other", empty, &[write("a/b", "1")]).unwrap();
    assert!(!vfs.owns(Tag(9)) && vfs.owns(Tag(10)) && !vfs.owns(Tag(14)));
    let durable = settle(&mut sim, &mut vfs, &mut Vec::new());
    assert_eq!(durable, vec![one.ticket, two.ticket, three.ticket]);
    let bytes = sim.contents(&file);
    let (recovered, found) = Vfs::recover(file, 10, &bytes);
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    // Seven nodes (`1`, `a`, `2`, the first root, `3`, the second root, the other root) and
    // three heads; `a` is shared by both heads and written once.
    assert_eq!(found.records, 10);
    assert_eq!(text(&recovered, recovered.head("w").unwrap(), "c").as_deref(), Some("3"));
    assert_eq!(contents(&recovered, recovered.head("other").unwrap()).len(), 1);
}

#[test]
fn collecting_rewrites_the_journal_as_what_the_heads_reach() {
    let file = name("state/journal");
    let mut sim = Sim::new(5, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let mut root = vfs.empty();
    for at in 0..40 {
        let big = at.to_string().repeat(1000);
        root = vfs
            .commit("w", root, &[write("big", &big), write("d/small", &at.to_string())])
            .unwrap()
            .root;
    }
    vfs.set_head("gone", root).unwrap();
    vfs.remove_head("gone").unwrap();
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let before = sim.contents(&file).len();

    vfs.collect(&[]);
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    let [Effect::Write { tag, .. }] = effects.as_slice() else { panic!("{effects:?}") };
    let tag = *tag;
    // A commit made while the rewrite is in flight is appended after it.
    let late = vfs.commit("w", root, &[write("late", "1")]).unwrap();
    // A failed rewrite is made again.
    effects.clear();
    let failed = Completion { tag, done: Done::Failed(IoError("no space".into())) };
    assert_eq!(vfs.complete(failed, &mut effects), vec![]);
    assert!(matches!(effects.as_slice(), [Effect::Write { .. }]), "{effects:?}");
    let durable = settle(&mut sim, &mut vfs, &mut effects);
    assert_eq!(durable, vec![late.ticket]);

    let after = sim.contents(&file);
    assert!(after.len() < before / 10, "{} bytes of {before}", after.len());
    let (back, found) = Vfs::recover(file.clone(), 0, &after);
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    assert_eq!(back.heads(), vec![("w", late.root)]);
    assert_eq!(contents(&back, late.root), contents(&vfs, late.root));
    assert_eq!(back.sequence("w"), vfs.sequence("w"));
    assert_eq!(back.journal.last_sequence(), vfs.journal.last_sequence());

    // Nothing appended since: a collection asks for no rewrite.
    vfs.collect(&[]);
    settle(&mut sim, &mut vfs, &mut Vec::new());
    vfs.collect(&[]);
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    assert_eq!(effects, vec![]);
}

/// Runs everything in flight under `sim`, handing each completion to whichever asked for it.
fn pump(
    sim: &mut Sim,
    vfs: &mut Vfs,
    watcher: &mut Watcher,
    effects: &mut Vec<Effect>,
) -> Vec<Watched> {
    let mut out = Vec::new();
    let mut done = Vec::new();
    loop {
        sim.submit(effects);
        if sim.poll(&mut done, Duration::ZERO) == 0 {
            return out;
        }
        for completion in done.drain(..) {
            if vfs.owns(completion.tag) {
                vfs.complete(completion, effects);
            } else {
                assert!(watcher.owns(completion.tag), "{completion:?}");
                out.extend(watcher.complete(vfs, completion, effects));
            }
        }
    }
}

fn committed(out: &[Watched]) -> bool {
    !out.is_empty() && out.iter().all(|it| matches!(it, Watched::Committed(_)))
}

#[test]
fn the_disk_becomes_transactions_on_its_head() {
    let mut sim = Sim::new(1, Faults::default());
    let mut vfs = new_vfs();
    sim.change(&name("ws/Cargo.toml"), b"[package]");
    let mut watcher = Watcher::new("disk", name("ws"), 100);
    let mut effects = Vec::new();
    watcher.start(&mut effects);
    let out = pump(&mut sim, &mut vfs, &mut watcher, &mut effects);
    assert!(matches!(out[..], [Watched::Committed(_), Watched::Scanned]), "{out:?}");
    sim.change(&name("ws/src/lib.rs"), b"fn a() {}");
    sim.change(&name("elsewhere"), b"x");
    let out = pump(&mut sim, &mut vfs, &mut watcher, &mut effects);
    assert!(committed(&out), "{out:?}");
    let head = vfs.head("disk").unwrap();
    assert_eq!(
        contents(&vfs, head),
        BTreeMap::from([
            ("Cargo.toml".into(), b"[package]".to_vec()),
            ("src/lib.rs".into(), b"fn a() {}".to_vec()),
        ])
    );

    // The bytes the head has already make no transaction.
    sim.change(&name("ws/src/lib.rs"), b"fn a() {}");
    assert_eq!(pump(&mut sim, &mut vfs, &mut watcher, &mut effects), vec![]);
    assert_eq!(vfs.head("disk"), Some(head));

    // A deletion, and a file replaced by a directory.
    sim.delete(&name("ws/Cargo.toml"));
    sim.delete(&name("ws/src/lib.rs"));
    sim.change(&name("ws/src/lib.rs/inner"), b"i");
    let out = pump(&mut sim, &mut vfs, &mut watcher, &mut effects);
    assert!(committed(&out), "{out:?}");
    let head = vfs.head("disk").unwrap();
    assert_eq!(contents(&vfs, head), BTreeMap::from([("src/lib.rs/inner".into(), b"i".to_vec())]));
    assert!(watcher.idle());

    watcher.stop(&mut effects);
    assert_eq!(pump(&mut sim, &mut vfs, &mut watcher, &mut effects), vec![Watched::Ended(None)]);
}

fn told(files: &[&str]) -> Completion {
    Completion {
        tag: Tag(100),
        done: Done::Changed { files: files.iter().map(|it| name(it)).collect(), lost: false },
    }
}

/// The stat and read tags of each path `effects` asks to look at, which are taken: both are
/// asked at once, in the same order.
fn looks(effects: &mut Vec<Effect>) -> Vec<(Tag, Tag)> {
    let (mut stats, mut reads) = (Vec::new(), Vec::new());
    effects.retain(|it| match it {
        Effect::Stat { file, tag } => {
            stats.push((file.clone(), *tag));
            false
        }
        Effect::ReadAll { file, tag } => {
            reads.push((file.clone(), *tag));
            false
        }
        _ => true,
    });
    assert_eq!(stats.len(), reads.len(), "each stat is asked with its read");
    stats
        .into_iter()
        .zip(reads)
        .map(|((stat_file, stat), (read_file, read))| {
            assert_eq!(stat_file, read_file);
            (stat, read)
        })
        .collect()
}

fn a_file(len: u64) -> Done {
    Done::Stat(Some(Meta { len, dir: false, modified_ns: 0 }))
}

/// Answers the scan `start` asked for as a directory holding just what the head does.
fn scan_as_head(vfs: &mut Vfs, watcher: &mut Watcher, effects: &mut Vec<Effect>) {
    let at = effects.iter().position(|it| matches!(it, Effect::List { .. })).expect("a listing");
    let Effect::List { tag, .. } = effects.remove(at) else { unreachable!() };
    let files = vfs.head("disk").map(|root| contents(vfs, root)).unwrap_or_default();
    let listed = files
        .iter()
        .map(|(path, bytes)| {
            (
                name(&format!("ws/{path}")),
                Meta { len: bytes.len() as u64, dir: false, modified_ns: 0 },
            )
        })
        .collect();
    let mut out = watcher.complete(vfs, Completion { tag, done: Done::Listed(listed) }, effects);
    let mut reads = Vec::new();
    effects.retain(|it| match it {
        Effect::ReadAll { file, tag } => {
            reads.push((*tag, file.as_str()["ws/".len()..].to_owned()));
            false
        }
        _ => true,
    });
    for (tag, path) in reads {
        let done = Done::Read(files[&path].clone());
        out.extend(watcher.complete(vfs, Completion { tag, done }, effects));
    }
    assert_eq!(out, vec![Watched::Scanned]);
}

#[test]
fn a_watch_starts_by_making_the_head_what_the_directory_holds() {
    let mut sim = Sim::new(2, Faults::default());
    let mut vfs = new_vfs();
    let stale = vfs.commit(
        "disk",
        vfs.empty(),
        &[write("gone", "1"), write("same", "s"), write("old", "o")],
    );
    let stale = stale.unwrap().root;
    sim.change(&name("ws/same"), b"s");
    sim.change(&name("ws/old"), b"new");
    sim.change(&name("ws/d/added"), b"a");
    let mut watcher = Watcher::new("disk", name("ws"), 100);
    let mut effects = Vec::new();
    watcher.start(&mut effects);
    assert_eq!(
        effects,
        vec![
            Effect::Watch { dir: name("ws"), tag: Tag(100) },
            Effect::List { dir: name("ws"), tag: Tag(101) },
        ]
    );
    assert!(!watcher.idle(), "the listing is in flight");
    let out = pump(&mut sim, &mut vfs, &mut watcher, &mut effects);
    assert!(matches!(out[..], [Watched::Committed(_), Watched::Scanned]), "{out:?}");
    let head = vfs.head("disk").unwrap();
    assert_eq!(
        vfs.diff(stale, head),
        vec![
            Change { path: "d/added".into(), kind: ChangeKind::Added },
            Change { path: "gone".into(), kind: ChangeKind::Removed },
            Change { path: "old".into(), kind: ChangeKind::Modified },
        ]
    );
    assert!(watcher.idle());

    // A directory that can't be listed stops the watch, naming the root.
    let mut lone = Watcher::new("other", name("nowhere"), 1 << 33);
    lone.start(&mut effects);
    let out = pump(&mut sim, &mut vfs, &mut lone, &mut effects);
    assert!(
        matches!(&out[..], [Watched::Unreadable { path, .. }, ..] if path.is_empty()),
        "{out:?}"
    );
    assert!(matches!(out.last(), Some(Watched::Ended(_))), "{out:?}");
    assert_eq!(vfs.head("other"), None);
}

#[test]
fn a_path_changed_since_the_report_is_a_conflict() {
    let mut vfs = new_vfs();
    let base = vfs.commit("disk", vfs.empty(), &[write("c", "old"), write("d", "old")]).unwrap();
    let mut watcher = Watcher::new("disk", name("ws"), 100);
    let mut effects = Vec::new();
    watcher.start(&mut effects);
    scan_as_head(&mut vfs, &mut watcher, &mut effects);
    effects.clear();
    assert_eq!(watcher.complete(&mut vfs, told(&["ws/c", "ws/d"]), &mut effects), vec![]);
    let tags = looks(&mut effects);
    assert_eq!(tags.len(), 2);

    // An agent changes `c` before the disk's change is read.
    vfs.commit("disk", base.root, &[write("c", "agent")]).unwrap();
    let mut out = Vec::new();
    for &(tag, _) in &tags {
        out.extend(watcher.complete(&mut vfs, Completion { tag, done: a_file(4) }, &mut effects));
    }
    assert_eq!(looks(&mut effects), vec![], "a stat asks for nothing more: the read is in flight");
    for &(_, tag) in &tags {
        let read = Completion { tag, done: Done::Read(b"disk".to_vec()) };
        out.extend(watcher.complete(&mut vfs, read, &mut effects));
    }
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0], Watched::Conflict { path: "c".into() });
    assert!(matches!(out[1], Watched::Committed(_)));
    let head = vfs.head("disk").unwrap();
    assert_eq!(text(&vfs, head, "c").as_deref(), Some("agent"));
    assert_eq!(text(&vfs, head, "d").as_deref(), Some("disk"));
}

#[test]
fn reports_commit_in_the_order_they_were_told() {
    let mut vfs = new_vfs();
    let mut watcher = Watcher::new("disk", name("ws"), 100);
    let mut effects = Vec::new();
    watcher.start(&mut effects);
    scan_as_head(&mut vfs, &mut watcher, &mut effects);
    effects.clear();
    watcher.complete(&mut vfs, told(&["ws/a"]), &mut effects);
    watcher.complete(&mut vfs, told(&["ws/a"]), &mut effects);
    let tags = looks(&mut effects);
    // The read comes back before its stat: it waits for the stat to say a file.
    let read = |vfs: &mut Vfs, watcher: &mut Watcher, (stat, read): (Tag, Tag), bytes: &[u8]| {
        let mut effects = Vec::new();
        let read = Completion { tag: read, done: Done::Read(bytes.to_vec()) };
        let mut out = watcher.complete(vfs, read, &mut effects);
        assert_eq!(out, vec![]);
        out.extend(watcher.complete(vfs, Completion { tag: stat, done: a_file(1) }, &mut effects));
        out
    };
    // The later report is read first, and waits for the earlier one.
    assert_eq!(read(&mut vfs, &mut watcher, tags[1], b"2"), vec![]);
    assert_eq!(vfs.head("disk"), None);
    let out = read(&mut vfs, &mut watcher, tags[0], b"1");
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(committed(&out), "its own commit is no conflict: {out:?}");
    assert_eq!(text(&vfs, vfs.head("disk").unwrap(), "a").as_deref(), Some("2"));
    assert!(watcher.idle());
}

#[test]
fn an_unreadable_path_or_lost_changes_are_named() {
    let mut vfs = new_vfs();
    let mut watcher = Watcher::new("disk", name("ws"), 100);
    assert!(!watcher.owns(Tag(99)) && watcher.owns(Tag(100)));
    assert!(watcher.owns(Tag(100 + (1 << 32) - 1)) && !watcher.owns(Tag(100 + (1 << 32))));
    let mut effects = Vec::new();
    watcher.start(&mut effects);
    scan_as_head(&mut vfs, &mut watcher, &mut effects);
    effects.clear();

    watcher.complete(&mut vfs, told(&["ws/a"]), &mut effects);
    let tags = looks(&mut effects);
    let &[(tag, read)] = tags.as_slice() else { panic!("one look: {tags:?}") };
    let error = IoError("permission denied".into());
    let failed = Completion { tag, done: Done::Failed(error.clone()) };
    assert_eq!(
        watcher.complete(&mut vfs, failed, &mut effects),
        vec![Watched::Unreadable { path: "a".into(), error }]
    );
    assert_eq!(vfs.head("disk"), None);
    assert!(watcher.idle(), "the failed stat settled the path; its read is not waited for");
    // The read the stat made moot comes back late, and is dropped.
    let late = Completion { tag: read, done: Done::Read(b"late".to_vec()) };
    assert_eq!(watcher.complete(&mut vfs, late, &mut effects), vec![]);
    assert_eq!(vfs.head("disk"), None);

    // Dropped changes are a rescan; a second loss while it lists asks for one more after.
    let lost =
        || Completion { tag: Tag(100), done: Done::Changed { files: Vec::new(), lost: true } };
    let list = Effect::List { dir: name("ws"), tag: Tag(101) };
    assert_eq!(watcher.complete(&mut vfs, lost(), &mut effects), vec![Watched::Lost]);
    assert_eq!(effects, vec![list.clone()]);
    assert_eq!(watcher.complete(&mut vfs, lost(), &mut effects), vec![Watched::Lost]);
    assert_eq!(effects, vec![list.clone()]);
    assert!(!watcher.idle());
    effects.clear();
    let b = (name("ws/b"), Meta { len: 1, dir: false, modified_ns: 0 });
    let listed = || Completion { tag: Tag(101), done: Done::Listed(vec![b.clone()]) };
    let read_b = |effects: &mut Vec<Effect>| {
        let at =
            effects.iter().position(|it| matches!(it, Effect::ReadAll { .. })).expect("a read");
        let Effect::ReadAll { file, tag } = effects.remove(at) else { unreachable!() };
        assert_eq!(file, name("ws/b"));
        Completion { tag, done: Done::Read(b"b".to_vec()) }
    };
    assert_eq!(watcher.complete(&mut vfs, listed(), &mut effects), vec![]);
    assert!(effects.contains(&list), "the second rescan: {effects:?}");
    let read = read_b(&mut effects);
    let out = watcher.complete(&mut vfs, read, &mut effects);
    assert!(matches!(out[..], [Watched::Committed(_), Watched::Scanned]), "{out:?}");
    assert_eq!(text(&vfs, vfs.head("disk").unwrap(), "b").as_deref(), Some("b"));
    effects.retain(|it| !matches!(it, Effect::List { .. }));
    assert_eq!(watcher.complete(&mut vfs, listed(), &mut effects), vec![]);
    let read = read_b(&mut effects);
    assert_eq!(watcher.complete(&mut vfs, read, &mut effects), vec![Watched::Scanned]);
    assert!(watcher.idle());

    watcher.stop(&mut effects);
    assert!(effects.contains(&Effect::Unwatch { tag: Tag(100) }));
    let ended = Completion { tag: Tag(100), done: Done::Unwatched };
    assert_eq!(watcher.complete(&mut vfs, ended, &mut effects), vec![Watched::Ended(None)]);
}

/// Random writes and deletes on the simulated disk, read back in any order the seed gives:
/// once everything is in, the head holds exactly what the disk does.
#[test]
fn the_head_follows_the_disk() {
    const PATHS: &[&str] = &["a", "b", "d/x", "d/y", "d/e/z"];
    for seed in 0..300 {
        let lose = if seed % 2 == 0 { 0 } else { 5 };
        let mut sim = Sim::new(seed, Faults { lose_change_one_in: lose, ..Faults::default() });
        let mut rng = Rng::new(seed ^ 0xd15c);
        let mut vfs = new_vfs();
        // The simulation lists only a directory that holds a file.
        sim.change(&name("ws/keep"), b"k");
        let mut watcher = Watcher::new("disk", name("ws"), 1 << 40);
        let mut effects = Vec::new();
        watcher.start(&mut effects);
        sim.submit(&mut effects);
        let mut disk: BTreeMap<String, Vec<u8>> = BTreeMap::from([("keep".into(), b"k".to_vec())]);
        let mut done = Vec::new();
        let mut out = Vec::new();
        for step in 0..60 {
            let path = PATHS[rng.below(PATHS.len() as u64) as usize];
            let file = name(&format!("ws/{path}"));
            if disk.contains_key(path) && rng.one_in(3) {
                sim.delete(&file);
                disk.remove(path);
            } else {
                let bytes = format!("{seed}:{step}:{}", rng.below(4)).into_bytes();
                sim.change(&file, &bytes);
                disk.insert(path.into(), bytes);
            }
            if rng.one_in(2) {
                sim.submit(&mut effects);
                sim.poll(&mut done, Duration::ZERO);
                for completion in done.drain(..) {
                    if vfs.owns(completion.tag) {
                        vfs.complete(completion, &mut effects);
                    } else {
                        out.extend(watcher.complete(&mut vfs, completion, &mut effects));
                    }
                }
            }
        }
        out.extend(pump(&mut sim, &mut vfs, &mut watcher, &mut effects));
        assert!(
            out.iter()
                .all(|it| matches!(it, Watched::Committed(_) | Watched::Lost | Watched::Scanned)),
            "seed {seed}: {out:?}"
        );
        assert!(out.contains(&Watched::Scanned), "seed {seed}: the first scan");
        assert!(watcher.idle());
        let head = vfs.head("disk").unwrap_or(vfs.empty());
        assert_eq!(contents(&vfs, head), disk, "seed {seed}");
    }
}
