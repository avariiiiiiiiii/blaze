use super::*;

fn path(p: &str) -> VfsPath {
    VfsPath::new_virtual_path(p.into())
}

#[test]
fn a_new_path_takes_the_id_its_caller_hands_out() {
    let mut vfs = Vfs::default();
    assert!(vfs.set_file_contents_with(path("/a.rs"), Some(b"a".to_vec()), || {
        FileId::from_raw(5)
    }));
    assert_eq!(vfs.file_id(&path("/a.rs")), Some((FileId::from_raw(5), FileExcluded::No)));
    assert_eq!(vfs.file_path(FileId::from_raw(5)), &path("/a.rs"));
    assert!(!vfs.exists(FileId::from_raw(0)));
    let files: Vec<_> = vfs.iter().map(|(id, p)| (id, p.clone())).collect();
    assert_eq!(files, [(FileId::from_raw(5), path("/a.rs"))]);
}

#[test]
fn a_known_path_keeps_its_id_without_asking_for_one() {
    let mut vfs = Vfs::default();
    vfs.set_file_contents_with(path("/a.rs"), Some(b"a".to_vec()), || FileId::from_raw(3));
    assert!(vfs.set_file_contents_with(path("/a.rs"), Some(b"b".to_vec()), || {
        panic!("a known path asked for a new ID")
    }));
    let changes = vfs.take_changes();
    assert_eq!(changes.len(), 1);
    let hash = hash_once::<FxHasher>(b"b".as_slice());
    assert_eq!(changes[&FileId::from_raw(3)].change, Change::Create(b"b".to_vec(), hash));
}

#[test]
fn dense_ids_never_meet_handed_out_ones() {
    let mut vfs = Vfs::default();
    vfs.set_file_contents(path("/a.rs"), Some(Vec::new()));
    vfs.set_file_contents_with(path("/b.rs"), Some(Vec::new()), || FileId::from_raw(4));
    vfs.set_file_contents(path("/c.rs"), Some(Vec::new()));
    assert_eq!(vfs.file_id(&path("/a.rs")).map(|(id, _)| id), Some(FileId::from_raw(0)));
    assert_eq!(vfs.file_id(&path("/c.rs")).map(|(id, _)| id), Some(FileId::from_raw(5)));
    assert_eq!(vfs.file_path(FileId::from_raw(4)), &path("/b.rs"));
}

#[test]
#[should_panic = "file ID 0 is already a path's"]
fn a_handed_out_id_another_path_has_is_refused() {
    let mut vfs = Vfs::default();
    vfs.set_file_contents(path("/a.rs"), Some(Vec::new()));
    vfs.set_file_contents_with(path("/b.rs"), Some(Vec::new()), || FileId::from_raw(0));
}
