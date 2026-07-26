---
title: feat: Split-panel search results and preview layout
type: feat
status: active
date: 2026-07-25
---

# feat: Split-panel Search Results and Preview Layout

## Summary

Split the CentralPanel into two side-by-side columns — search results on the left, preview on the right — so both are always visible regardless of result count. Increase font sizes for search text and tag labels.

---

## Problem Frame

Currently, search results and the PDF/text preview share a single vertical column in the CentralPanel. When searches return many results, the results ScrollArea consumes all available space and pushes the preview panel entirely off-screen. Users must scroll past all results to see a preview, making quick scanning impossible. Additionally, search text and tag labels use disproportionately small fonts relative to other UI elements.

---

## Assumptions

*This plan was authored without synchronous user confirmation. The items below are agent inferences that fill gaps in the input — un-validated bets that should be reviewed before implementation proceeds.*

- The user wants a side-by-side horizontal split (not a max-height constraint on results with continued vertical stacking)
- Font size increases should be modest (current body ~14px default; tags currently `.small()` at ~10px; target ~12-14px for tags, search input should match default body)
- The `preview_panel_size` display-resolution rendering pipeline must continue tracking available preview area size

---

## Requirements

- R1. Search results and preview panel display simultaneously side by side, each independently scrollable
- R2. Preview panel remains visible at all times regardless of search result count
- R3. Search query text input uses a larger font size
- R4. Tag labels in search results and filter chips use a larger font size
- R5. Existing PDF preview rendering (display-resolution, page navigation, zoom) continues to work correctly
- R6. Existing text file preview continues to work correctly
- R7. Empty states and error messages continue to display correctly
- R8. The file browser and tag side panels are unaffected

---

## Scope Boundaries

- Window resize behavior remains automatically handled by egui layout — no custom resize logic
- No change to file browser panel, tag panel, or status bar
- No extraction of layout code into a separate module (deferred to follow-up)
- No configurable/user-adjustable split ratios

### Deferred to Follow-Up Work

- Extract layout logic from `src/app.rs` into `src/ui/layout.rs`: separate PR after this layout stabilizes
- Centralize font sizes into a `UiTheme` config struct: separate PR

---

## Context & Research

### Relevant Code and Patterns

- `src/app.rs` lines 1138–1374 — Current CentralPanel containing search bar + results + preview
- `src/app.rs` lines 1077–1136 — `SidePanel::left("file_browser")` pattern for resizable panels
- `src/app.rs` lines 150–151, 1350–1352 — `preview_panel_size` tracking for display-resolution rendering
- `src/app.rs` lines 1200–1246 — Search results `ScrollArea` pattern with `ui.add_sized()`, `Frame`, `SelectableLabel`
- `src/app.rs` lines 690–770 — `render_highlighted_snippet` with `RichText` color/size patterns
- egui 0.30: `ui.columns(2, |columns| { ... })` API for splitting UI space
- egui 0.30: `ScrollArea::vertical().max_height(f32)` for constraining scroll region height

### Institutional Learnings

- No `docs/solutions/` directory exists — no prior layout refactoring learnings
- The original design explicitly prioritized "utilitarian, no fancy features" simplicity (per `docs/brainstorms/2026-01-15-pdf-search-viewer-requirements.md`)
- No past layout bugs documented across 10 prior plans

### External References

- egui 0.30 `Ui::columns()` API: creates temporary column split, returns result then releases

---

## Key Technical Decisions

- **Use `ui.columns(2, …)` inside CentralPanel** rather than `SidePanel::right`. This keeps conditional rendering logic local to the CentralPanel (empty states, error messages, and preview all stay in the right column without restructuring). It also preserves the existing panel stacking order. A `SidePanel::right` would require moving all preview/empty-state rendering out of the CentralPanel and into a new panel closure, increasing diff surface and risking ordering issues with panel stacking.
- **Set a `max_height` on the results ScrollArea** as a fallback defense even with the column split, preventing the results column from expanding beyond viewport in edge cases (e.g., no preview selected yet)
- **Use `RichText::size(14.0)` for tag labels** (up from `.small()` which is ~10px) — matches default egui body text size
- **Let search input use default egui text size** — the current `text_edit_singleline` already uses the default body font which is adequate; the issue was the output labels being too small, not the input

---

## Implementation Units

### U1. Split CentralPanel into results (left) and preview (right) columns

**Goal:** Replace the single vertical stack in CentralPanel with a two-column layout so search results and preview are simultaneously visible.

**Requirements:** R1, R2, R7

**Dependencies:** None

**Files:**
- Modify: `src/app.rs`

**Approach:**
- After the search bar and tag filter chips (before the separator), split remaining space with `ui.columns(2, |columns|)`
- Left column (columns[0]): search results with `ScrollArea::vertical().max_height(columns[0].available_size().y)`
- Right column (columns[1]): preview or empty state, with its own `ScrollArea::vertical()` for text or `ui.image()` for PDF
- Preserve all existing conditional rendering logic: empty/no-folder/no-engine states, "no results" message, PDF preview with navigation controls, text preview
- The `has_search_query` variable (defined before the column split) controls whether the left column shows results
- When no search is active, the left column can collapse or show the file browser, and the right column shows the empty/landing state

**Patterns to follow:**
- Existing `ScrollArea::vertical().id_salt(...)` pattern from lines 1200 and 1368
- Existing `Frame::default().fill(bg).inner_margin(4.0)` result row pattern from lines 1210-1244

**Test scenarios:**
- Happy path: Search with 50+ results → results scroll in left column, preview of selected result visible in right column simultaneously
- Happy path: Click result in left column → preview updates in right column without scrolling
- Edge case: Window resized to narrow width → columns distribute proportionally, both still visible
- Edge case: No result selected yet → right column shows empty/landing state, left column shows results
- Edge case: Text file preview (non-PDF) → renders correctly in right column ScrollArea

**Verification:**
- Visual inspection: search results and preview visible side by side in the app window
- Preview never pushed off-screen regardless of result count

---

### U2. Increase search text and tag label font sizes

**Goal:** Make search result filenames, match counts, and tag labels more readable at larger font sizes.

**Requirements:** R3, R4

**Dependencies:** None (can be done independently of U1)

**Files:**
- Modify: `src/app.rs`

**Approach:**
- Search result filename: currently `.strong()` (inherits default body size ~14px) — increase to `.size(16.0).strong()` for the `SelectableLabel`
- Tag labels in results (line 1232): change `RichText::new(format!("🏷{}", t)).small()` to `.size(14.0)` to match body text
- Tag filter chips in search bar (line 1184): change `ui.label(format!("🔖 {}", tag))` to use `RichText::new(...).size(14.0)`
- Search input field: keep at default egui body text size (already ~14px, no change needed)

**Patterns to follow:**
- Existing `RichText::new(...).size(N.0).color(...)` pattern from tags panel (line 899)
- Existing `RichText::new(...).strong()` pattern from results (line 1218)

**Test scenarios:**
- Happy path: Search results show filenames at 16px bold, tags at 14px
- Edge case: Long filenames with tags → text fits within result row without overflow or clipping
- Edge case: Filter chips with long tag names → chips display correctly at 14px

**Verification:**
- Visual inspection: search result text and tags are noticeably larger than before
- No text clipping or layout overflow in result rows

---

### U3. Preserve preview_panel_size tracking in new layout

**Goal:** Ensure the display-resolution PDF rendering pipeline receives correct panel dimensions after the layout split.

**Requirements:** R5, R6

**Dependencies:** U1

**Files:**
- Modify: `src/app.rs`

**Approach:**
- In the right column of the column split, capture `ui.available_size()` into `self.preview_panel_size` before rendering the PDF image — identical to the existing line 1350-1352 pattern
- Verify the render request path (lines 440-441 for initial render, 503-504 and 559-560 for page-flip render) still receives the updated panel size
- No changes needed to the `RenderRequest` struct or renderer thread — they remain agnostic to layout

**Patterns to follow:**
- Existing `let avail = ui.available_size(); self.preview_panel_size = (avail.x as u32, avail.y as u32);` from lines 1350-1352

**Test scenarios:**
- Happy path: Open a PDF, observe correct resolution rendering in the right panel
- Happy path: Resize window → preview re-renders at new panel dimensions
- Edge case: Navigate pages → each page renders at the current panel size

**Verification:**
- PDF preview renders at correct resolution matching the right-panel dimensions
- Page navigation, zoom controls all function as before
- No regression in text file preview

---

## System-Wide Impact

- **Interaction graph:** None — layout change is self-contained within `src/app.rs` CentralPanel rendering
- **Error propagation:** Unchanged — existing status bar error messages unaffected
- **State lifecycle risks:** Low — no new state fields; `preview_panel_size` continues to be updated each frame
- **API surface parity:** N/A — desktop GUI only
- **Integration coverage:** Manual visual verification covers the layout change
- **Unchanged invariants:** File browser panel, tag panel, status bar, folder picker dialog — all unchanged; renderer thread and search engine are agnostic to layout

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `ui.columns(2, …)` has poor resize behavior on narrow windows | egui columns distribute proportionally by default; test with narrow window widths (800px, 600px) |
| Conditional rendering logic becomes tangled with the split | Keep existing control flow — the split is purely a layout wrapper; all conditions inside columns stay unchanged |

---

## Sources & References

- Existing code: `src/app.rs`
- egui 0.30 API: `Ui::columns()`, `ScrollArea::vertical().max_height()`
- Prior plan: `docs/plans/2026-07-22-009-feat-pdf-viewing-perf-plan.md` (preview_panel_size tracking)
