use std::sync::Arc;

use blaze_query::{Config, Database, MemTree, Query, QueryKey};
use blaze_units::query::{BuildConfig, Command, Fetch, FetchKind, Invocation, Mount, Origin};
use blaze_vfs::{View, ViewId};

use super::*;

const WS: &str = "/home/u/ws";
const A: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"b\" }\n\n[dev-dependencies]\nc = { path = \"c\" }\n";
const B: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const C: &str = "[package]\nname = \"c\"\nversion = \"0.2.0\"\nedition = \"2024\"\n";

fn asked() -> TreeAsk {
    TreeAsk {
        resolve: ResolveAsk::default(),
        targets: Vec::new(),
        edges: vec![Edge::Normal, Edge::Build, Edge::Dev],
        invert: Vec::new(),
        prune: Vec::new(),
        prefix: TreePrefix::Indent,
        no_dedupe: false,
        duplicates: false,
        format: "{p}".to_owned(),
        depth: TreeDepth::Max(u32::MAX),
        no_proc_macro: false,
        public: false,
        unicode: true,
        color: false,
    }
}

fn database(tree: &Arc<MemTree>) -> Database {
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    blaze_units::query::register(&mut db);
    register(&mut db);
    db
}

fn open(db: &Database, tree: &MemTree, files: &[(&str, &str)]) -> ViewId {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(path, text)| (*path, text.as_bytes())).collect();
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: WS.to_owned(),
            cwd: WS.to_owned(),
            env: vec![("HOME".to_owned(), "/home/u".to_owned())],
            files: Vec::new(),
        },
        mounts: vec![Mount { at: WS.to_owned(), origin: Origin::View, under: "ws".to_owned() }],
        host: "x86_64-unknown-linux-gnu".to_owned(),
        library: "/rustc/0/library".to_owned(),
    };
    db.open_with(View { root: tree.root(&files) }, &config.encode())
}

fn workspace() -> [(&'static str, &'static str); 6] {
    [
        ("ws/Cargo.toml", A),
        ("ws/src/lib.rs", ""),
        ("ws/b/Cargo.toml", B),
        ("ws/b/src/lib.rs", ""),
        ("ws/c/Cargo.toml", C),
        ("ws/c/src/lib.rs", ""),
    ]
}

fn printed(db: &Database, view: ViewId, ask: &TreeAsk) -> String {
    match db.pin().get::<Tree>(view, ask).expect("printed").get() {
        Printed::Ready { out, warnings } => {
            assert!(warnings.is_empty(), "{warnings:?}");
            out
        }
        other => panic!("not printed: {other:?}"),
    }
}

#[test]
fn the_tree_is_cargos() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    assert_eq!(
        printed(&db, view, &asked()),
        "a v0.1.0 (/home/u/ws)\n└── b v0.1.0 (/home/u/ws/b)\n[dev-dependencies]\n└── c v0.2.0 (/home/u/ws/c)\n"
    );
    let normal = TreeAsk { edges: vec![Edge::Normal], ..asked() };
    assert_eq!(printed(&db, view, &normal), "a v0.1.0 (/home/u/ws)\n└── b v0.1.0 (/home/u/ws/b)\n");
    let inverted = TreeAsk { invert: vec!["b".to_owned()], ..asked() };
    assert_eq!(printed(&db, view, &inverted), "b v0.1.0 (/home/u/ws/b)\n└── a v0.1.0 (/home/u/ws)\n");
    let ascii = TreeAsk { unicode: false, prefix: TreePrefix::Depth, ..normal };
    assert_eq!(printed(&db, view, &ascii), "0a v0.1.0 (/home/u/ws)\n1b v0.1.0 (/home/u/ws/b)\n");
}

#[test]
fn a_source_edit_prints_from_the_tree_it_had() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let first = printed(&db, open(&db, &tree, &workspace()), &asked());
    let mut edited = workspace();
    edited[1] = ("ws/src/lib.rs", "pub fn f() {}\n");
    let view = open(&db, &tree, &edited);
    assert_eq!(printed(&db, view, &asked()), first);
    assert!(db.why(view, &QueryKey { kind: Tree::KIND, input: Tree::input(&asked()) }).is_none());
}

#[test]
fn a_registry_dependency_is_fetched_first() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let manifest = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n";
    let view = open(&db, &tree, &[("ws/Cargo.toml", manifest), ("ws/src/lib.rs", "")]);
    assert_eq!(
        db.pin().get::<Tree>(view, &asked()).expect("printed").get(),
        Printed::Needs(vec![Fetch {
            what: "crates.io index".to_owned(),
            url: "https://index.crates.io/se/rd/serde".to_owned(),
            kind: FetchKind::IndexFile,
            at: "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/se/rd/serde".to_owned(),
        }])
    );
}

#[test]
fn features_and_duplicates_are_cargos_error() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    let both = TreeAsk { edges: vec![Edge::Features], duplicates: true, ..asked() };
    assert_eq!(
        db.pin().get::<Tree>(view, &both).expect("printed").get(),
        Printed::Failed(LoadError {
            what: "the `-e features` flag does not support `--duplicates`".to_owned(),
            at: None
        })
    );
}
