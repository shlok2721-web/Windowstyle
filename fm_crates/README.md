# File-manager service crates

These crates are workspace members of the repository-root `Cargo.toml`. Run Cargo commands from the repository root; there is intentionally no nested workspace manifest here.

| Crate | Purpose |
|---|---|
| `fm_mime` | MIME detection and file categorization |
| `fm_thumbs` | Image/video thumbnail generation, RAM LRU, cost-based persist policy with batched writes, and read-only audits |
| `fm_settings` | Versioned TOML preferences and atomic no-op-aware saves |
| `fm_walk` | Recursive traversal, search and size/count helpers (the on-disk index was retired in favour of hot-tier RAM search) |
| `fm_notify` | Debounced directory change notifications |
| `fm_trash` | Freedesktop Trash operations |
| `fm_behav` | RAM-first behaviour mapping: 16-byte event ring folded into bounded aggregates (frecency, next-folder table, pins, tier scores) with a deterministic text encoding. No I/O |
| `fm_lifecycle` | SIGTERM/SIGHUP/SIGINT and systemd-logind suspend/shutdown hooks (with a delay inhibitor) delivered as polled events |
| `fm_persist` | One small atomic state file: hash-skipped writes, flush policy (shutdown, or enough change + 30 min) and a bytes-written counter |
| `fm_tiers` | RAM-only hot/warm/cold directory-listing tiers: mtime-validated warm cache, deny list, hysteresis promotion. Never writes. Also provides hot-only in-RAM name search (no on-disk index) |
