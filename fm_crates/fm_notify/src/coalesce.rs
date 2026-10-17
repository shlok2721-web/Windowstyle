//! Pure logic: turn a burst of raw inotify events into a short, meaningful list.

use std::{collections::HashMap, path::PathBuf};

use crate::Event;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Raw {
    Created {
        path: PathBuf,
        is_dir: bool,
    },
    Removed {
        path: PathBuf,
        is_dir: bool,
    },
    Modified {
        path: PathBuf,
    },
    MovedFrom {
        path: PathBuf,
        is_dir: bool,
        cookie: u32,
    },
    MovedTo {
        path: PathBuf,
        is_dir: bool,
        cookie: u32,
    },
    WatchRemoved {
        path: PathBuf,
    },
    Overflow,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    Created,
    Modified,
    Removed,
}

/// Rules, applied in order of arrival:
/// * `MovedFrom`+`MovedTo` with the same cookie become `Renamed`; unpaired halves become `Removed` / `Created`.
/// * repeated `Modified`, or `Modified` right after `Created`, collapse into one event.
/// * `Created` then `Removed` of the same path cancel out (temp files).
/// * `Removed` then `Created` of the same path (atomic save) becomes `Modified`.
/// * an overflow replaces everything but `WatchRemoved` with a single `Rescan`.
pub(crate) fn coalesce(raw: Vec<Raw>) -> Vec<Event> {
    if raw.iter().any(|r| matches!(r, Raw::Overflow)) {
        let mut v = vec![Event::Rescan];
        v.extend(raw.into_iter().filter_map(|r| match r {
            Raw::WatchRemoved { path } => Some(Event::WatchRemoved { path }),
            _ => None,
        }));
        return v;
    }

    // 1. pair renames
    let mut to_by_cookie: HashMap<u32, usize> = HashMap::new();
    for (i, r) in raw.iter().enumerate() {
        if let Raw::MovedTo { cookie, .. } = r {
            to_by_cookie.entry(*cookie).or_insert(i);
        }
    }
    let mut consumed = vec![false; raw.len()];
    let mut stage: Vec<Event> = Vec::with_capacity(raw.len());
    for (i, r) in raw.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        match r {
            Raw::Created { path, is_dir } => stage.push(Event::Created {
                path: path.clone(),
                is_dir: *is_dir,
            }),
            Raw::Removed { path, is_dir } => stage.push(Event::Removed {
                path: path.clone(),
                is_dir: *is_dir,
            }),
            Raw::Modified { path } => stage.push(Event::Modified { path: path.clone() }),
            Raw::WatchRemoved { path } => stage.push(Event::WatchRemoved { path: path.clone() }),
            Raw::Overflow => {}
            Raw::MovedFrom {
                path,
                is_dir,
                cookie,
            } => match to_by_cookie
                .get(cookie)
                .copied()
                .filter(|&j| j > i && !consumed[j])
            {
                Some(j) => {
                    consumed[j] = true;
                    if let Raw::MovedTo { path: to, .. } = &raw[j] {
                        stage.push(Event::Renamed {
                            from: path.clone(),
                            to: to.clone(),
                            is_dir: *is_dir,
                        });
                    }
                }
                None => stage.push(Event::Removed {
                    path: path.clone(),
                    is_dir: *is_dir,
                }),
            },
            Raw::MovedTo { path, is_dir, .. } => stage.push(Event::Created {
                path: path.clone(),
                is_dir: *is_dir,
            }),
        }
    }

    // 2. merge per path
    let mut out: Vec<Option<Event>> = Vec::with_capacity(stage.len());
    let mut last: HashMap<PathBuf, (usize, Last)> = HashMap::new();
    for ev in stage {
        match ev {
            Event::Modified { path } => match last.get(&path) {
                Some((_, Last::Created | Last::Modified)) => {}
                _ => {
                    last.insert(path.clone(), (out.len(), Last::Modified));
                    out.push(Some(Event::Modified { path }));
                }
            },
            Event::Created { path, is_dir } => match last.get(&path).copied() {
                Some((i, Last::Removed)) => {
                    out[i] = None;
                    last.insert(path.clone(), (out.len(), Last::Modified));
                    out.push(Some(Event::Modified { path }));
                }
                _ => {
                    last.insert(path.clone(), (out.len(), Last::Created));
                    out.push(Some(Event::Created { path, is_dir }));
                }
            },
            Event::Removed { path, is_dir } => match last.get(&path).copied() {
                Some((i, Last::Created)) => {
                    out[i] = None;
                    last.remove(&path);
                }
                Some((i, Last::Modified)) => {
                    out[i] = None;
                    last.insert(path.clone(), (out.len(), Last::Removed));
                    out.push(Some(Event::Removed { path, is_dir }));
                }
                _ => {
                    last.insert(path.clone(), (out.len(), Last::Removed));
                    out.push(Some(Event::Removed { path, is_dir }));
                }
            },
            other => {
                if let Event::Renamed { from, to, .. } = &other {
                    last.remove(from);
                    last.remove(to);
                }
                out.push(Some(other));
            }
        }
    }
    out.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn copy_burst_is_one_created() {
        let raw = vec![
            Raw::Created {
                path: p("/d/f"),
                is_dir: false,
            },
            Raw::Modified { path: p("/d/f") },
            Raw::Modified { path: p("/d/f") },
            Raw::Modified { path: p("/d/f") },
        ];
        assert_eq!(
            coalesce(raw),
            vec![Event::Created {
                path: p("/d/f"),
                is_dir: false
            }]
        );
    }

    #[test]
    fn rename_pairs_and_unpaired_halves() {
        let raw = vec![
            Raw::MovedFrom {
                path: p("/d/a"),
                is_dir: false,
                cookie: 7,
            },
            Raw::MovedTo {
                path: p("/d/b"),
                is_dir: false,
                cookie: 7,
            },
            Raw::MovedFrom {
                path: p("/d/out"),
                is_dir: true,
                cookie: 8,
            },
            Raw::MovedTo {
                path: p("/d/in"),
                is_dir: false,
                cookie: 9,
            },
        ];
        assert_eq!(
            coalesce(raw),
            vec![
                Event::Renamed {
                    from: p("/d/a"),
                    to: p("/d/b"),
                    is_dir: false
                },
                Event::Removed {
                    path: p("/d/out"),
                    is_dir: true
                },
                Event::Created {
                    path: p("/d/in"),
                    is_dir: false
                },
            ]
        );
    }

    #[test]
    fn temp_file_cancels_and_atomic_save_is_modified() {
        let raw = vec![
            Raw::Created {
                path: p("/d/tmp"),
                is_dir: false,
            },
            Raw::Modified { path: p("/d/tmp") },
            Raw::Removed {
                path: p("/d/tmp"),
                is_dir: false,
            },
            Raw::Removed {
                path: p("/d/doc"),
                is_dir: false,
            },
            Raw::Created {
                path: p("/d/doc"),
                is_dir: false,
            },
        ];
        assert_eq!(coalesce(raw), vec![Event::Modified { path: p("/d/doc") }]);
    }

    #[test]
    fn overflow_means_rescan() {
        let raw = vec![
            Raw::Modified { path: p("/d/x") },
            Raw::Overflow,
            Raw::WatchRemoved { path: p("/d") },
        ];
        assert_eq!(
            coalesce(raw),
            vec![Event::Rescan, Event::WatchRemoved { path: p("/d") }]
        );
    }
}
