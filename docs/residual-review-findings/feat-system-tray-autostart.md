## Residual Review Findings

Code review run on `feat/system-tray-autostart` branch (2026-07-26).
Plan: `docs/plans/2026-07-26-016-feat-system-tray-autostart-plan.md`.

### Applied safe_auto fixes (2)
- P0: Removed duplicate auto-start enable/disable logic in status bar — now calls `sync_auto_start()`.
- P3: Replaced `first_frame: bool` with `auto_start_synced: Option<()>` using `Option::take()` one-shot pattern.

### Residual non-auto findings

- **P2 — gated_auto**: `src/main.rs:~154-205` — Tray icon creation block (~52 lines, 3 nested match expressions) not extracted to a free function. Makes `main()` harder to scan. Suggested: extract to `fn create_tray_icon() -> Option<(...)>`.
- **P2 — manual**: `src/app.rs:1760-1796` — Status bar mixes status display with settings controls (two checkboxes + config save). Suggested: move settings to dedicated menu/panel.
- **P2 — manual**: `src/app.rs:~88` — PapervaultApp has 62 fields (God struct). Tray additions are the latest increment. Suggested: group into sub-structs (TrayState, PreviewState, SearchState, etc.).
- **P3 — advisory**: `src/app.rs:1008-1027` — Tray/close/auto-start lifecycle interleaved at top of `update()` with `should_exit` reset inside close_requested guard. Acceptable for now; extract to method if more lifecycle concerns are added.

### Pre-existing findings (not caused by this diff, noted for awareness)

- **P1 — gated_auto**: `src/config.rs` tests validate serde library behavior, not `Config::save()`/`Config::load()` filesystem paths.
- **P1 — safe_auto**: `src/app.rs:758` — Config save failure silently swallowed on exit (`let _ = self.config.save()`).
- **P1 — safe_auto**: `src/app.rs:584` — Config save failure silently swallowed on folder switch.
- **P1 — manual**: Zero integration tests exist despite test plan defining 30+ cases.
- **P2 — gated_auto**: `src/app.rs:648-650` — Folder switch thread aborts silently on missing tag store.

### Run artifact
Path: `docs/residual-review-findings/feat-system-tray-autostart.md`
Review mode: autofix
Plan: docs/plans/2026-07-26-016-feat-system-tray-autostart-plan.md
