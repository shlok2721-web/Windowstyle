use std::fs;

use fm_trash::{Error, Trash};

fn setup() -> (tempfile::TempDir, Trash) {
    let tmp = tempfile::tempdir().unwrap();
    let trash = Trash::with_home_trash(tmp.path().join("share/Trash"));
    (tmp, trash)
}

#[test]
fn trash_list_restore_roundtrip() {
    let (tmp, trash) = setup();
    let file = tmp.path().join("hello world.txt");
    fs::write(&file, "hi").unwrap();

    let item = trash.trash(&file).unwrap();
    assert!(!file.exists());
    assert!(item.trashed_path().exists());
    assert!(item.info_path().exists());

    let listed = trash.list().unwrap();
    assert!(listed.iter().any(|i| {
        i.original_path()
            == fs::canonicalize(tmp.path())
                .unwrap()
                .join("hello world.txt")
    }));

    let back = item.restore().unwrap();
    assert_eq!(fs::read_to_string(back).unwrap(), "hi");
    assert!(!item.info_path().exists());
}

#[test]
fn same_name_twice_does_not_overwrite_and_restore_never_clobbers() {
    let (tmp, trash) = setup();
    let file = tmp.path().join("a.txt");
    fs::write(&file, "one").unwrap();
    let first = trash.trash(&file).unwrap();
    fs::write(&file, "two").unwrap();
    let second = trash.trash(&file).unwrap();
    assert_ne!(first.trashed_path(), second.trashed_path());

    fs::write(&file, "occupied").unwrap();
    assert!(matches!(first.restore(), Err(Error::AlreadyExists(_))));
    assert_eq!(fs::read_to_string(&file).unwrap(), "occupied");
}

#[test]
fn directories_and_symlinks_and_errors() {
    let (tmp, trash) = setup();
    let dir = tmp.path().join("d");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/f"), "x").unwrap();
    let link = tmp.path().join("l");
    std::os::unix::fs::symlink(&dir, &link).unwrap();

    let results = trash.trash_all([&link, &dir, &tmp.path().join("missing")]);
    assert!(results[0].is_ok() && results[1].is_ok());
    assert!(matches!(results[2], Err(Error::NotFound(_))));
    assert!(dir.exists() == false && fs::symlink_metadata(&link).is_err());

    assert!(matches!(trash.trash("/"), Err(Error::InvalidPath(_))));
    let item = results.into_iter().nth(1).unwrap().unwrap();
    assert!(matches!(
        trash.trash(item.trashed_path()),
        Err(Error::OverlapsTrash(_))
    ));
}

#[test]
fn purge_and_empty() {
    let (tmp, trash) = setup();
    for n in ["x", "y"] {
        fs::write(tmp.path().join(n), n).unwrap();
    }
    let x = trash.trash(tmp.path().join("x")).unwrap();
    trash.trash(tmp.path().join("y")).unwrap();
    x.purge().unwrap();
    assert!(!x.trashed_path().exists());
    assert_eq!(trash.empty().unwrap(), 1);
    assert!(
        trash
            .list()
            .unwrap()
            .iter()
            .all(|i| !i.original_path().starts_with(tmp.path()))
    );
}
