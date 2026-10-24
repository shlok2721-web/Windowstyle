use std::{fs, time::Duration};

use fm_notify::{Config, DirWatcher, Event, Watcher};

fn collect(rx: &std::sync::mpsc::Receiver<Vec<Event>>) -> Vec<Event> {
    let mut all = Vec::new();
    while let Ok(b) = rx.recv_timeout(Duration::from_millis(600)) {
        all.extend(b);
    }
    all
}

#[test]
fn create_rename_remove_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (w, rx) = Watcher::channel(Config {
        debounce: Duration::from_millis(30),
    })
    .unwrap();
    w.watch(dir).unwrap();

    fs::write(dir.join("a.txt"), "x").unwrap();
    let ev = collect(&rx);
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Created { path, .. } if path.ends_with("a.txt"))),
        "{ev:?}"
    );

    fs::rename(dir.join("a.txt"), dir.join("b.txt")).unwrap();
    let ev = collect(&rx);
    assert!(ev.iter().any(|e| matches!(e, Event::Renamed { from, to, .. } if from.ends_with("a.txt") && to.ends_with("b.txt"))), "{ev:?}");

    fs::remove_file(dir.join("b.txt")).unwrap();
    let ev = collect(&rx);
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Removed { path, .. } if path.ends_with("b.txt"))),
        "{ev:?}"
    );
}

#[test]
fn dir_watcher_follows_navigation_and_stops_on_drop() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut dw = DirWatcher::new(|| {}).unwrap();
    dw.set_dir(Some(a.path())).unwrap();
    dw.set_dir(Some(b.path())).unwrap();
    assert_eq!(dw.dir(), Some(b.path()));

    fs::write(a.path().join("ignored"), "").unwrap(); // old folder: no longer watched
    fs::write(b.path().join("seen"), "").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let ev = dw.poll();
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::Created { path, .. } if path.ends_with("seen"))),
        "{ev:?}"
    );
    assert!(
        !ev.iter()
            .any(|e| matches!(e, Event::Created { path, .. } if path.ends_with("ignored"))),
        "{ev:?}"
    );
    drop(dw); // must not hang
}

#[cfg(target_os = "linux")]
#[test]
fn watching_a_symlink_alias_does_not_corrupt_the_original_watch() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    let alias = tmp.path().join("alias");
    fs::create_dir(&real).unwrap();
    symlink(&real, &alias).unwrap();

    let (watcher, rx) = Watcher::channel(Config {
        debounce: Duration::from_millis(20),
    })
    .unwrap();
    watcher.watch(&real).unwrap();
    assert_eq!(
        watcher.watch(&alias).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );

    fs::write(real.join("first.txt"), "x").unwrap();
    let events = collect(&rx);
    assert!(
        events.iter().any(|event| {
            matches!(event, Event::Created { path, .. } if path == &real.join("first.txt"))
        }),
        "{events:?}"
    );

    watcher.unwatch(&real).unwrap();
    watcher.watch(&alias).unwrap();
    fs::write(real.join("second.txt"), "x").unwrap();
    let events = collect(&rx);
    assert!(
        events.iter().any(|event| {
            matches!(event, Event::Created { path, .. } if path == &alias.join("second.txt"))
        }),
        "{events:?}"
    );
}
