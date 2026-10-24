# Windowstyle V4: acknowledged window creation and safe fallback

## Root cause

The Unix socket listener previously acknowledged a folder request as soon as it was placed on an `mpsc` channel. That did not mean the root egui frame had consumed the request or that `show_viewport_deferred` had produced a native window. On Hyprland/Wayland, the root surface can be on an inactive workspace and may not run the required UI frame promptly. A repaint request or focus command alone cannot guarantee that the child window will be created.

## V4 changes

1. The Unix socket performs a V4 protocol greeting before sending a folder request. A pre-V4 primary receives no folder request from the V4 launcher, preventing an un-cancelable request from appearing much later as a duplicate. The launcher opens that folder standalone instead.
2. A V4 request includes a unique ID. `QUEUED` means only that it reached the channel; `OPENED` is sent only after the native child viewport callback starts. The launcher waits up to three seconds for `OPENED`.
3. If the viewport does not appear in time, the launcher sends a cancellation over a separate socket connection and opens the requested folder in a standalone process. The normal path still reuses the primary process and its caches. The fallback is used only when the primary cannot confirm native window creation.
4. Cancellation is represented by a shared atomic flag. The UI checks it before creating a child and again when the viewport callback runs. A cancellation that arrives before the server has registered the request is remembered, avoiding late queue resurrection. This closes the principal duplicate-window race.
5. Child-close cleanup explicitly requests a root viewport repaint. If the root was already hidden and the last child closes (including a child canceled during fallback), V4 sends a close command directly to the root viewport so the hidden primary does not linger.
6. The `egui` and `eframe` declarations stay at the project original `0.37.0`. Do not downgrade these crates to make the existing `Cargo.lock` appear consistent: this code uses APIs absent in `0.36.2` (`Id::unique` and `Ui::scope_id`). The original lockfile is stale at `0.36.2`; regenerate it with `cargo generate-lockfile` or let the repository release workflow regenerate it before a `--locked` build.

## Regression test plan

1. Build the V4 source and completely close any V1–V3 Windowstyle process before starting the new primary. V4 deliberately uses standalone fallback against an older primary until the old process exits.
2. Start Windowstyle on Hyprland workspace 1. Switch to workspace 2 and run the binary with a directory path. The new window should open on workspace 2 without manually visiting workspace 1. If the primary does not create the viewport in three seconds, the launcher opens it independently instead.
3. Repeat while the root window is hidden but a child remains open. Opening another folder should either create a confirmed child or fall back standalone; it must not wait for the root workspace.
4. Open several folders rapidly, then close the resulting windows. Canceled requests must not materialize after the fallback has opened, and the process whose root was hidden should exit after its final child closes.
5. Confirm the normal case still has a single process; a separate process is expected only when the primary fails to acknowledge actual viewport creation within the timeout or when an older primary is still running.

## Verification status

The supplied environment does not contain `cargo` or `rustc`, so a Rust compile and runtime test could not be executed here. Static checks confirmed balanced Rust delimiters and that the manifest versions match the checked-in lockfile. The regression plan above still needs to be run in the target Arch Linux / Hyprland environment or in CI.
