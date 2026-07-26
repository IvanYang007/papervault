---
title: "feat: Add system tray and Windows auto-start"
type: feat
status: active
date: 2026-07-26
---

# feat: Add system tray and Windows auto-start

## Summary

Add Windows system tray integration (minimize to tray on close, right-click Open/Exit) and registry-based auto-start so Papervault launches with Windows and stays accessible without occupying the taskbar.

---

## Problem Frame

Papervault is a desktop companion app meant to stay available while the user works. Currently, closing the window fully exits the app (tearing down background indexer, watcher, and renderer threads), forcing a cold restart every time. On reboot, the user must manually locate and launch the executable. Both friction points work against the app's design goal of being a fast, always-ready document search tool. The original project plan explicitly deferred system tray and auto-start as follow-up work — this plan delivers them.

---

## Requirements

- R1. Closing the Papervault window minimizes it to the system tray instead of exiting the app.
- R2. The system tray icon shows a right-click context menu with **Open** (restore window) and **Exit** (quit the app).
- R3. Left-click or double-click on the tray icon restores the window.
- R4. The app can register itself to launch at Windows 11 startup via a user-facing toggle.
- R5. When auto-started, the app launches minimized to tray without showing the window.
- R6. The full shutdown cascade (save config, drop channels, join background threads) fires only on explicit Exit from the tray menu, never on minimize-to-tray.
- R7. Existing single-instance guard continues to work correctly: closing to tray does not release the mutex; only Exit does.

---

## Scope Boundaries

- Cross-platform tray support (macOS, Linux) — this is Windows-only for now.
- Notification balloons / toast popups from the tray icon.
- Minimize button behavior — only the close (X) button triggers minimize-to-tray; the minimize button continues to minimize to the taskbar normally.
- Settings UI polish for the tray/startup toggles — out of scope for this plan; toggles can live in the existing bottom status bar or a simple checkbox in the UI.

### Deferred to Follow-Up Work

- Upgrade eframe from 0.30 to 0.34+ to get the `Visible(false)` deadlock fix and cleaner tray integration — tracked separately to avoid scope creep.
- "Start minimized" setting independent of "Start with Windows" — current design ties them together (auto-start always starts minimized).

---

## Context & Research

### Relevant Code and Patterns

- **Windows FFI pattern**: `src/main.rs` lines 22–61 — `CreateMutexW` via `extern "system"` with `OsStrExt::encode_wide()`. Follow this for any new Win32 calls.
- **Config pattern**: `src/config.rs` — derive `Serialize, Deserialize, Default`, atomic write via `.tmp` rename. New bool fields use `#[serde(default)]` for backward compatibility.
- **Graceful shutdown**: `src/app.rs` `on_exit()` (lines 1789–1801) and `src/runtime.rs` `FolderRuntime::stop()` (lines 173–209) — drop sender channels, set shutdown flags, join threads. Must only fire on real exit.
- **Close interception API**: eframe 0.30 — `ctx.input(|i| i.viewport().close_requested())` + `ViewportCommand::CancelClose`. The old `on_close_event` was removed in 0.24.
- **Frame parameter**: `update(&mut self, ctx, _frame)` in `app.rs` line 952 — `_frame` is currently unused but available for `frame.info().window_info` and `frame.wgpu_render_state()`.

### Institutional Learnings

- **Tray + auto-start were planned from day one**: `docs/plans/2026-01-15-001-feat-pdf-search-viewer-plan.md` lists both under "Deferred to Follow-Up Work."
- **Prior window-close freeze**: commit `c0a657a` fixed a freeze on window close. The fix pattern (drop sender channels before joining threads) must be preserved. Regression risk is real when modifying the close path.
- **Single-instance mutex interaction**: The `CreateMutexW` guard in `main.rs` must survive minimize-to-tray — the mutex is released only when the process exits.

### External References

- **`tray-icon` crate** (Tauri team, v0.24.1): Best-maintained cross-platform tray crate. Has a working egui example. Internal Windows backend wraps `Shell_NotifyIcon`. DPI fix merged (PR #164). Menu via `muda`.
- **`auto-launch` crate** (v0.6): Standard Rust crate for registry `Run` key. Used by Tauri's `tauri-plugin-autostart`. Handles `StartupApproved\Run` for Task Manager integration. CurrentUser mode (no admin required).
- **eframe `Visible` deadlock** (issue #5229): On eframe 0.30, `ViewportCommand::Visible(false)` followed by `Visible(true)` never restores because Windows stops sending `RedrawRequested` to hidden windows. Fixed in eframe 0.34+ (PR #7905). Workaround for 0.30: use `ViewportCommand::Minimized(true)` to hide (window goes to taskbar, eframe keeps ticking at ~0.9% CPU).
- **`image` crate** (v0.25): Required by `tray-icon` for loading PNG icons as RGBA bytes.

---

## Key Technical Decisions

- **Use `tray-icon` crate, not raw Win32 FFI**: The tray `NOTIFYICONDATAW` / `Shell_NotifyIcon` API is significantly more complex than the existing mutex FFI (callback message pump, icon resource management, DPI scaling). `tray-icon` wraps this correctly and is maintained by the Tauri organization — the highest bus-factor in the Rust desktop ecosystem.
- **Use `Minimized(true)` for hiding on eframe 0.30**: `Visible(false)` is broken on 0.30 (deadlock). `Minimized(true)` keeps the window in the taskbar but hidden; eframe continues ticking at low CPU. This is an acceptable intermediate state until the eframe upgrade.
- **Use `auto-launch` crate for startup**: Handles registry `Run` key + `StartupApproved\Run` for Task Manager compatibility. Cleaner than raw FFI for registry operations, which are error-prone (wide-string encoding, key handle lifetime, multiple registry paths).
- **Close interception in `update()`, not via winit hook**: `ctx.input(|i| i.viewport().close_requested())` + `CancelClose` is the supported eframe 0.30 pattern. No need to reach into winit directly for close events.
- **Tray icon created in eframe's init closure**: `tray-icon` requires the winit event loop to be running before the tray icon is created (Windows limitation). The eframe creation closure is the right place.
- **Exit controlled by `should_exit: bool` flag**: When the tray Exit menu item is clicked, set `should_exit = true` → send `ViewportCommand::Close` → next `close_requested()` check sees the flag and lets the close proceed → `on_exit()` fires normally.

---

## Implementation Units

### U1. Add dependencies and tray icon asset

**Goal:** Add required crate dependencies and create a tray icon asset.

**Requirements:** R2 (tray icon appearance)

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml`
- Create: `assets/tray-icon.png`

**Approach:**
- Add `tray-icon = "0.24"`, `image = "0.25"`, `auto-launch = "0.6"` to `[dependencies]`.
- Create a 256×256 PNG icon in `assets/tray-icon.png` — a simple "P" or book glyph on a colored background. If a design asset isn't available, generate a minimal geometric icon programmatically or use a placeholder.
- The `image` crate is needed because `tray-icon::Icon::from_rgba()` requires raw RGBA bytes; `image::open().into_rgba8()` provides this.

**Patterns to follow:**
- `Cargo.toml` already has well-organized `[dependencies]` and `[features]` sections — add new entries in alphabetical position.

**Test scenarios:**
- Test expectation: none — dependency addition and asset creation have no behavioral change.

**Verification:**
- `cargo build` succeeds with new dependencies.
- `assets/tray-icon.png` exists and is a valid PNG.

---

### U2. Extend Config with tray and startup settings

**Goal:** Add `start_with_windows` and `minimize_to_tray` boolean fields to `Config`, persisted via the existing JSON save/load mechanism.

**Requirements:** R4 (auto-start toggleable), R1 (minimize-to-tray behavior)

**Dependencies:** None

**Files:**
- Modify: `src/config.rs`

**Approach:**
- Add two `bool` fields to the `Config` struct:
  ```rust
  #[serde(default)]
  pub start_with_windows: bool,
  #[serde(default)]
  pub minimize_to_tray: bool,
  ```
- `#[serde(default)]` ensures existing config files without these fields deserialize cleanly (both default to `false`).
- No changes to `load()` or `save()` needed — serde handles the new fields automatically.
- Add test: round-trip with new fields, deserialization of old-format JSON without the fields.

**Patterns to follow:**
- `src/config.rs` — `Config` struct pattern, `#[derive(Serialize, Deserialize, Default)]`, atomic save via `.tmp` rename.

**Test scenarios:**
- Happy path: Save config with `start_with_windows: true`, `minimize_to_tray: true`, reload, verify both fields preserved.
- Edge case: Deserialize old config JSON without the new fields — both should default to `false`.
- Edge case: Default `Config` has both fields as `false`.

**Verification:**
- `cargo test` passes with new config tests.
- Existing config files are not broken by the new fields.

---

### U3. Implement system tray icon with context menu

**Goal:** Create a system tray icon with right-click context menu (Open, Exit) and left-click restore. Tray events are polled in the app update loop.

**Requirements:** R2, R3

**Dependencies:** U1 (dependencies), U2 (minimize_to_tray config)

**Files:**
- Modify: `src/main.rs`
- Modify: `src/app.rs`

**Approach:**
- Load the tray icon PNG in the eframe creation closure using `image::open()` → `into_rgba8()` → `tray_icon::Icon::from_rgba()`.
- Build the tray menu with `tray_icon::menu::Menu` + `MenuItem` ("Open" with id 1, separator, "Exit" with id 2).
- Create the tray icon via `TrayIconBuilder::new().with_tooltip("Papervault").with_icon(...).with_menu(...).build()`.
- In `PapervaultApp`, store `tray_icon: Option<tray_icon::TrayIcon>` and two menu item IDs.
- In `update()`, before the main UI code, poll `TrayIconEvent::receiver().try_recv()` and `MenuEvent::receiver().try_recv()`:
  - Left-click or double-click → restore window: send `Minimized(false)` viewport command, set `minimized_to_tray = false`.
  - "Open" menu item → same as left-click.
  - "Exit" menu item → set `should_exit = true`, send `ViewportCommand::Close`.
- The tray icon lifecycle: created in eframe init closure, dropped automatically when `PapervaultApp` is dropped (on real exit).
- The `image` crate is only used for icon loading and isn't needed at runtime after init.

**Patterns to follow:**
- `src/main.rs` eframe creation closure — currently loads CJK fonts; tray icon creation follows the same init-closure pattern.

**Test scenarios:**
- Happy path: App starts → tray icon appears in notification area → right-click shows menu with Open and Exit.
- Happy path: Left-click tray icon → window restores from minimized state.
- Happy path: Double-click tray icon → window restores.
- Edge case: Tray icon creation failure (e.g., missing icon file) — app should still start with window visible, log a warning.

**Verification:**
- Tray icon visible in Windows 11 notification area.
- Right-click context menu appears with Open and Exit options.
- Left-click restores the window.

---

### U4. Implement window close interception (minimize-to-tray)

**Goal:** Intercept the window close button (X) and redirect to tray minimization instead of exit.

**Requirements:** R1, R6, R7

**Dependencies:** U3 (tray icon exists), U2 (minimize_to_tray config)

**Files:**
- Modify: `src/app.rs`

**Approach:**
- Add `minimized_to_tray: bool` and `should_exit: bool` fields to `PapervaultApp`.
- In `update()`, before UI rendering, check `ctx.input(|i| i.viewport().close_requested())`:
  - If `should_exit` is `true`: allow the close to proceed (do nothing — the default behavior exits).
  - If `minimize_to_tray` config is `true`: send `ViewportCommand::CancelClose`, then send `ViewportCommand::Minimized(true)`, set `self.minimized_to_tray = true`.
  - If `minimize_to_tray` config is `false`: allow close (current behavior).
- The `should_exit` flag is set to `true` only by the tray Exit menu item (U3). It bypasses the minimize-to-tray redirect.
- When the window is restored from tray (U3), send `ViewportCommand::Minimized(false)` to bring it back.
- The existing `on_exit()` handler runs only on real exit — the shutdown cascade (save config, drop channels, join threads) is preserved exactly as-is.

**Patterns to follow:**
- `on_exit()` in `src/app.rs` lines 1789–1801 — preserves the existing shutdown sequence unchanged.

**Test scenarios:**
- Happy path: Click X button → window minimizes to tray, taskbar entry disappears (Minimized state), tray icon remains.
- Happy path: Click Exit in tray menu → app fully exits, `on_exit()` fires, threads joined.
- Happy path: With `minimize_to_tray: false` config → X button exits normally (unchanged behavior).
- Edge case: Click X while already minimized to tray → second close attempt is a no-op (Minimized already).
- Integration: Background threads (indexer, watcher, renderer) continue running after minimize-to-tray.
- Regression: Prior close-freeze fix (commit `c0a657a`) still works — exit via tray does not freeze.

**Verification:**
- Window close minimizes to tray when config is enabled.
- Window close exits when config is disabled.
- Background operations continue after minimize.
- Full exit from tray menu completes cleanly with no hanging threads.

---

### U5. Implement Windows auto-start via registry

**Goal:** Register/unregister Papervault in the Windows startup registry key, toggled by the `start_with_windows` config field.

**Requirements:** R4, R5

**Dependencies:** U2 (start_with_windows config)

**Files:**
- Modify: `src/main.rs`
- Modify: `src/app.rs`

**Approach:**
- In eframe's init closure, construct an `auto_launch::AutoLaunch` instance targeting the current executable path with `WindowsEnableMode::CurrentUser` and args `&["--minimized"]`.
- Store it in `PapervaultApp` as `Option<auto_launch::AutoLaunch>`.
- In `main()`, before `eframe::run_native`, parse `std::env::args()` for `--minimized`. If present, set `ViewportBuilder::with_visible(false)` so the window starts hidden.
- In the UI, when the user toggles `start_with_windows`:
  - If enabling: call `auto.enable()`.
  - If disabling: call `auto.disable()`.
  - Sync the toggle state by calling `auto.is_enabled()`.
- On app startup (in the first `update()` call), if `config.start_with_windows` differs from `auto.is_enabled()`, sync them (handles the case where the user manually removed the registry entry).
- The `auto_launch` crate writes to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Papervault` and manages the `StartupApproved\Run` companion key for Task Manager compatibility.

**Patterns to follow:**
- `src/config.rs` — config fields persisted via serde.
- `src/main.rs` — `#[cfg(target_os = "windows")]` gating for Windows-only code.

**Test scenarios:**
- Happy path: Enable "Start with Windows" → registry `Run` key contains the Papervault path with `--minimized`.
- Happy path: Disable "Start with Windows" → registry key removed.
- Happy path: Auto-started with `--minimized` → window starts hidden, tray icon visible.
- Edge case: `auto_launch::AutoLaunch::new()` fails (e.g., unusual executable path) — app starts normally without auto-start, logs a warning.
- Edge case: Config says enabled but registry key missing → first `update()` syncs and disables the config toggle.

**Verification:**
- Registry key appears/disappears when toggling "Start with Windows" in the app.
- `--minimized` flag causes window to start hidden.
- Confirmed via Task Manager > Startup tab that Papervault appears.

---

### U6. Wire together in main.rs and add UI toggle

**Goal:** Integrate all pieces in `main.rs` and add a minimal UI control for tray/startup settings in the app.

**Requirements:** R1–R7

**Dependencies:** U3, U4, U5

**Files:**
- Modify: `src/main.rs`
- Modify: `src/app.rs`

**Approach:**
- In `main.rs`:
  - Parse `--minimized` from `std::env::args()`.
  - Set `ViewportBuilder::with_visible(!start_minimized)`.
  - Load tray icon PNG, construct tray menu, create `TrayIcon`.
  - Construct `AutoLaunch` instance.
  - Pass all new state into `PapervaultApp::new()`.
- In `app.rs`:
  - Add new fields: `tray_icon`, `auto_launch`, `minimized_to_tray`, `should_exit`, `menu_open_id`, `menu_exit_id`.
  - In `update()`, before existing UI code:
    1. Poll tray events (U3).
    2. Check close interception (U4).
    3. Sync auto-start state on first frame.
  - Add a minimal settings section in the existing bottom bar or a side panel: two checkboxes for "Start with Windows" and "Minimize to tray".
  - Toggle handlers sync config and call `config.save()`.

**Patterns to follow:**
- `src/main.rs` — existing eframe creation closure pattern, CJK font loading.
- `src/app.rs` — existing field layout, bottom status bar pattern.

**Test scenarios:**
- Happy path: Full flow — enable both toggles → close window → window minimizes to tray → left-click tray icon → window restores → disable auto-start → click Exit → app closes cleanly.
- Edge case: Config file corruption — defaults apply, app still starts.
- Integration: Single-instance mutex is held throughout minimize-to-tray cycle, released only on full exit.

**Verification:**
- `cargo build --release` succeeds.
- Full manual smoke test of all flows on Windows 11.
- No regressions in existing search, preview, tagging, or file browser behavior.

---

## System-Wide Impact

- **Interaction graph:** The close path now has a conditional branch (minimize vs. exit). Tray events add a new input source polled every frame. The auto-start toggle adds a registry write path.
- **Error propagation:** Tray icon creation failure logs a warning and continues (app remains usable without tray). Auto-launch failure logs a warning. Neither is fatal.
- **State lifecycle risks:** `should_exit` must be reset to `false` when the first `close_requested` check fires and the close is allowed through — otherwise a subsequent cancel/restore cycle could leave it stuck. Implementation should reset `should_exit` after the close is permitted or on window restore.
- **Unchanged invariants:** The existing `on_exit()` shutdown cascade is preserved exactly. `FolderRuntime::stop()` is still called only once, only on real exit. Channel lifecycle is unchanged. Tantivy index commit still happens on exit.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `Minimized(true)` workaround may cause unwanted taskbar presence when hidden | Acceptable for now; the eframe 0.34 upgrade (deferred) will replace with `Visible(false)` which properly hides the taskbar entry. |
| `tray-icon` crate version may have breaking changes mid-development | Pin to exact version `=0.24` to avoid surprise updates. |
| Tray icon creation after eframe init may fail on some Windows configurations | Wrapped in `match` / `ok()`, logs warning, app continues without tray. |
| Existing ~50-field `PapervaultApp` struct becoming unwieldy | 6 new fields is manageable; a future refactor to group UI state into sub-structs is deferred. |
| Window close regression (the prior `c0a657a` freeze) | Explicitly tested in U4 — exit path exercises the full shutdown cascade. |

---

## Sources & References

- **Origin roadmap:** [docs/plans/2026-01-15-001-feat-pdf-search-viewer-plan.md](docs/plans/2026-01-15-001-feat-pdf-search-viewer-plan.md) — system tray and auto-start listed as deferred follow-up.
- Related code: `src/main.rs` — Windows FFI pattern (CreateMutexW), eframe init closure.
- Related code: `src/app.rs` — `update()`, `on_exit()`, `PapervaultApp` struct.
- Related code: `src/config.rs` — Config serialization pattern.
- Related code: `src/runtime.rs` — `FolderRuntime::stop()` graceful shutdown.
- External: [tray-icon crate](https://crates.io/crates/tray-icon) v0.24
- External: [auto-launch crate](https://crates.io/crates/auto_launch) v0.6
- External: [eframe Visible deadlock issue](https://github.com/emilk/egui/issues/5229)
- External: [eframe close_requested API](https://docs.rs/eframe/0.30/eframe/trait.App.html)
