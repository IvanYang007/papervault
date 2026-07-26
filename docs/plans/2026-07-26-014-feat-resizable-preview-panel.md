---
title: feat: Resizable preview panel via SidePanel::right
type: feat
status: active
date: 2026-07-26
---

# feat: Resizable Preview Panel

## Summary

Replace `ui.columns(2, …)` with `SidePanel::right("preview")` for the preview panel, making it resizable by the user — matching the existing file browser panel behavior.

## Requirements

- R1. Preview panel is resizable by dragging its left border
- R2. Existing preview functionality preserved (PDF, text, empty states, tags at top)
- R3. Search results remain in the CentralPanel with search bar
- R4. Default preview panel width: 50% of window (like current columns split)

## Implementation Units

### U1. Move preview from ui.columns to SidePanel::right

**Files:** `src/app.rs`

**Approach:**
- Remove `ui.columns(2, |columns| { ... })` wrapper
- Left column (search results) stays in CentralPanel
- Right column content moves to a `SidePanel::right("preview_panel").resizable(true).default_width(ctx.screen_rect().width() * 0.5)`
- Preview empty states, PDF, text, and tags all render in the SidePanel::right
- `preview_panel_size` tracking uses `ui.available_size()` in the side panel

**Patterns:** Existing `SidePanel::left("file_browser").resizable(true)` at line ~1280

## Sources

- `src/app.rs` lines 1445-1512: current `ui.columns(2, ...)` layout
- `src/app.rs` line 1280: resizable file browser panel pattern
