//! Pure logic: turn a burst of raw inotify events into a short, meaningful list.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

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
pub(crate) fn coalesce(mut raw: Vec<Raw>) -> Vec<Event> {
    if raw.iter().any(|r| matches!(r, Raw::Overflow)) {
        let mut seen = HashSet::new();
        let mut v = vec![Event::Rescan];
        v.extend(raw.into_iter().filter_map(|r| match r {
            Raw::WatchRemoved { path } if seen.insert(path.clone()) => {
                Some(Event::WatchRemoved { path })
            }
            _ => None,
        }));
        return v;
    }

    // A zero move cookie does not identify a rename pair. In particular, do
    // not accidentally combine unrelated move events when the cookie is zero.
    let mut to_by_cookie: HashMap<u32, usize> = HashMap::new();
    for (i, r) in raw.iter().enumerate() {
        if let Raw::MovedTo { cookie, .. } = r {
            if *cookie != 0 {
                to_by_cookie.entry(*cookie).or_insert(i);
            }
        }
    }

    // Consume raw entries in place. `Overflow` is a sentinel here because an
    // actual overflow was handled above; this avoids cloning every PathBuf into
    // a second staging vector and eliminates a separate `consumed` bitmap.
    let mut stage: Vec<Event> = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let item = std::mem::replace(&mut raw[i], Raw::Overflow);
        match item {
            Raw::Created { path, is_dir } => stage.push(Event::Created { path, is_dir }),
            Raw::Removed { path, is_dir } => stage.push(Event::Removed { path, is_dir }),
            Raw::Modified { path } => stage.push(Event::Modified { path }),
            Raw::WatchRemoved { path } => stage.push(Event::WatchRemoved { path }),
            Raw::Overflow => {}
            Raw::MovedFrom {
                path,
                is_dir,
                cookie,
            } => {
                let paired = if cookie == 0 {
                    None
                } else {
                    to_by_cookie.get(&cookie).copied().filter(|&j| j > i)
                };
                if let Some(j) = paired {
                    let destination = std::mem::replace(&mut raw[j], Raw::Overflow);
                    if let Raw::MovedTo { path: to, .. } = destination {
                        stage.push(Event::Renamed {
                            from: path,
                            to,
                            is_dir,
                        });
                    } else {
                        // Defensive fallback: preserve the source-side event
                        // if the indexed destination is unexpectedly absent.
                        stage.push(Event::Removed { path, is_dir });
                    }
                } else {
                    stage.push(Event::Removed { path, is_dir });
                }
            }
            Raw::MovedTo { path, is_dir, .. } => stage.push(Event::Created { path, is_dir }),
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
                Some((_, Last::Created)) => {
                    // Duplicate create notifications for one path do not
                    // change the externally visible state.
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
                Some((_, Last::Removed)) => {
                    // Duplicate removals do not change the path's final state.
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
    fn zero_move_cookies_are_not_paired() {
        let raw = vec![
            Raw::MovedFrom {
                path: p("/d/a"),
                is_dir: false,
                cookie: 0,
            },
            Raw::MovedTo {
                path: p("/d/b"),
                is_dir: false,
                cookie: 0,
            },
        ];
        assert_eq!(
            coalesce(raw),
            vec![
                Event::Removed {
                    path: p("/d/a"),
                    is_dir: false,
                },
                Event::Created {
                    path: p("/d/b"),
                    is_dir: false,
                },
            ]
        );
    }

    #[test]
    fn repeated_create_for_same_path_is_collapsed() {
        let raw = vec![
            Raw::Created {
                path: p("/d/f"),
                is_dir: false,
            },
            Raw::Created {
                path: p("/d/f"),
                is_dir: false,
            },
        ];
        assert_eq!(
            coalesce(raw),
            vec![Event::Created {
                path: p("/d/f"),
                is_dir: false,
            }]
        );
    }

    #[test]
    fn repeated_remove_for_same_path_is_collapsed() {
        let raw = vec![
            Raw::Removed {
                path: p("/d/f"),
                is_dir: false,
            },
            Raw::Removed {
                path: p("/d/f"),
                is_dir: false,
            },
        ];
        assert_eq!(
            coalesce(raw),
            vec![Event::Removed {
                path: p("/d/f"),
                is_dir: false,
            }]
        );
    }

    #[test]
    fn overflow_deduplicates_watch_removed_paths() {
        let raw = vec![
            Raw::Overflow,
            Raw::WatchRemoved { path: p("/d") },
            Raw::WatchRemoved { path: p("/d") },
        ];
        assert_eq!(
            coalesce(raw),
            vec![Event::Rescan, Event::WatchRemoved { path: p("/d") }]
        );
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
