# Wayland unresponsive/ping fix diagnostic

## What this change does

This patch upgrades `egui` and `eframe` from 0.36.2 to 0.37.0 in the app and the `egui_explorer` crate. The eframe 0.37.0 release includes the upstream fix for continuing to process Wayland events when compositor frame callbacks stop (#8631).

This is a targeted candidate fix for the reported Hyprland 0.56.2 / eframe glow renderer issue. It has **not been built or runtime-tested in this environment** because the Rust `cargo` toolchain is unavailable here.

Upstream references:
- https://github.com/emilk/egui/pull/8631
- https://github.com/emilk/egui/blob/master/crates/eframe/CHANGELOG.md

## Build and test

From this workspace directory, using a Rust toolchain:

```bash
cargo build --release -p fm_app
```

Do not pass `--locked` on the first build: Cargo needs to refresh `Cargo.lock` to match the new `egui`/`eframe` 0.37.0 constraints. The resulting binary should be at `target/release/file-manager`.

Keep the existing installed binary intact. Test the new binary separately, reproducing the covered/unfocused-window situation. Confirm whether Hyprland's ping/pong trace now shows timely pongs before replacing the installed binary.
