# fm_trash

Move files to the desktop trash instead of deleting them. Implements the
[freedesktop.org Trash spec 1.0](https://specifications.freedesktop.org/trash/latest/), so GNOME, KDE,
Thunar, `gio trash` and `trash-cli` see exactly the same trash.

* No `unsafe`, **no egui dependency** (only `Path`s in, `Path`s out). Linux only; other platforms return `Error::Unsupported`.
* Dependencies: `chrono` (local time for `DeletionDate`), `rustix` (safe `renameat2`, `getuid`).

```rust
let item = fm_trash::trash("/home/me/old.txt")?;   // Delete
let all  = fm_trash::list()?;                      // Recycle-bin view, newest first
item.restore()?;                                   // Undo (never overwrites)
item.purge()?;                                     // delete one item for good
fm_trash::empty()?;                                // empty the trash
```

## Wiring into `egui_explorer`

```rust
Event::Command { cmd: Command::Delete, targets } => {
    let done: Vec<_> = fm_trash::trash_all(&targets)?.into_iter().flatten().collect();
    undo_stack.push(done);                          // Command::Undo -> item.restore()
}
Event::Command { cmd: Command::DeletePermanently, targets } => { /* std::fs::remove_* */ }
```

## What it does

* Same filesystem as the home trash -> `$XDG_DATA_HOME/Trash` (atomic rename).
* Another filesystem -> `$topdir/.Trash/$uid` (only if `.Trash` is a real sticky-bit dir), else `$topdir/.Trash-$uid` (0700, owned by you).
  If neither can be created, falls back to a copy into the home trash.
* `.trashinfo` is created with `O_EXCL` first (claims the name atomically); names never collide or overwrite.
* Refuses mount points, `/`, `foo/..`, and anything inside / containing a trash dir.
* Symlinks are trashed as links, never followed. Restore uses `RENAME_NOREPLACE`.

Not done: the optional `directorysizes` cache is not written (spec says SHOULD); entries are removed when you purge/restore.
Cross-device fallback copies don't preserve timestamps/ownership.
