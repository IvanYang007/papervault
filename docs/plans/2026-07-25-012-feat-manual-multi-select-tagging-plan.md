---
title: feat: Manual multi-select file tagging from file browser
type: feat
status: active
date: 2026-07-25
---

# feat: Manual Multi-Select File Tagging from File Browser

## Summary

Add multi-select to the file browser panel so users can Ctrl+click files to build a selection, then trigger DeepSeek auto-tagging on just those files via a "Tag Selected (N)" button. Content is extracted from disk for each file and sent through the existing `AutoTagRequest` channel.

---

## Problem Frame

Currently, auto-tagging triggers automatically on all newly indexed files or via "Re-index for tags" (which re-tags everything). There is no way to manually select specific files and tag just those. Users with many files need to re-tag a small subset without re-processing the entire collection.

---

## Assumptions

*This plan was authored without synchronous user confirmation. The items below are agent inferences — un-validated bets.*

- The user prefers a button-based trigger over right-click context menu
- Text files are extracted via `std::fs::read_to_string`; PDFs use `pdf_oxide` (already a dependency)
- Multi-select state is local to the app session (not persisted across restarts)
- The "Tag Selected" button appears only when files are selected and `auto_tag_enabled` is true
- Progress shown via existing `auto_tag_progress` mechanism

---

## Requirements

- R1. Users can Ctrl+click files in the file browser to toggle multi-selection independently of single-click browsing
- R2. A "🏷 Tag Selected (N)" button appears in the file browser header when N ≥ 1 files are selected
- R3. Clicking the button extracts content for each selected file and sends `AutoTagRequest::TagDocument` via the auto-tagger channel
- R4. Text files are read with `std::fs::read_to_string`; PDFs are extracted with `pdf_oxide`
- R5. Progress is tracked via the existing `auto_tag_progress` / `auto_tag_progress` (AtomicUsize) counters
- R6. Existing single-click file browsing (`browsed_file`) continues to work independently of multi-select

---

## Scope Boundaries

- No right-click context menu (button approach only)
- No tag-before-indexing (files must already be in the index)
- No multi-select in search results (file browser only)
- No persisted selection state across app restarts

---

## Context & Research

### Relevant Code and Patterns

- `src/app.rs` lines 1140–1200 — File browser rendering with `ScrollArea`, `selectable_label` per file
- `src/app.rs` lines 620–685 — `retag_selected()` method: reads file from disk, sends `AutoTagRequest::TagDocument`
- `src/app.rs` lines 820–850 — Reindex path: sends tag requests with placeholder text for all docs
- `src/app.rs` lines 44–51 — `AutoTagRequest::TagDocument` struct: `content_hash`, `filename`, `text`, `content_hash_before_tag`
- `src/app.rs` lines 166–172 — `file_browser_docs`, `browsed_file` state fields
- `src/indexer/extractors/pdf.rs` — `pdf_oxide` extraction pipeline (reusable for PDF content extraction)

### Institutional Learnings

- No `docs/solutions/` exists — no prior learnings on multi-select or file browser changes

---

## Key Technical Decisions

- **Multi-select via `HashSet<String>`** (file paths) rather than indices. Paths are stable identifiers; indices shift when the file list is refreshed during active indexing.
- **Ctrl+click detection** via egui's `Response::modifiers`. egui 0.30 surfaces keyboard modifiers through `ui.input(|i| i.modifiers.ctrl)`.
- **Reuse `retag_selected` extraction pattern** rather than building a new extraction path. The same `std::fs::read_to_string` + BLAKE3 hash approach works for text files. For PDFs, call `pdf_oxide` extraction directly.
- **Button in file browser header** rather than in the toolbar. Keeps the trigger close to the selection UI and avoids cluttering the search bar area.

---

## Implementation Units

### U1. Add multi-select state and file browser interaction

**Goal:** Enable Ctrl+click to toggle files in a selection set, with visual feedback.

**Requirements:** R1, R6

**Dependencies:** None

**Files:**
- Modify: `src/app.rs`

**Approach:**
- Add `selected_files: HashSet<String>` field to `PapervaultApp`, default empty
- In the file browser render loop, check `ui.input(|i| i.modifiers.ctrl)` on each click
- Ctrl+click: toggle the file path in `selected_files`; do NOT set `browsed_file`
- Regular click: clear `selected_files`, set `browsed_file` as before
- Change the `selectable_label` to a `checkbox` or add a visual indicator (e.g., colored background) for selected files

**Patterns to follow:**
- Existing `browsed_file` single-select pattern
- `ui.input(|i| i.modifiers.ctrl)` for modifier detection
- Frame background coloring from search results (lines 1284-1290)

**Test scenarios:**
- Happy path: Ctrl+click 3 files → all 3 highlighted, `selected_files` contains 3 entries
- Happy path: Regular click on a file → `selected_files` cleared, single file browsed
- Edge case: Files refreshes during indexing → selection persists by path (paths are stable)
- Edge case: Ctrl+click a selected file → deselects it

**Verification:**
- Ctrl+click highlights files visually; regular click browses as before
- `selected_files` count matches visible highlights

---

### U2. Add "Tag Selected" button and content extraction

**Goal:** Show a button when files are selected; on click, extract content and send tag requests.

**Requirements:** R2, R3, R4, R5

**Dependencies:** U1

**Files:**
- Modify: `src/app.rs`

**Approach:**
- In the file browser header (after the document count), render a button when `!selected_files.is_empty()` and `auto_tagger_tx.is_some()`:
  ```
  if ui.button("🏷 Tag Selected ({})", selected_files.len()).clicked() { ... }
  ```
- On click:
  1. Reset progress counter via `folder_runtime.auto_tag_progress.store(0, ...)`
  2. Set `auto_tag_progress = Some((0, selected_files.len()))`
  3. For each file path in `selected_files`:
     - Look up `DocumentInfo` by path in `file_browser_docs`
     - Read content: if `.pdf` extension → use `pdf_oxide`; otherwise → `std::fs::read_to_string`
     - Compute `content_hash_before_tag` (BLAKE3 of filename + content)
     - Upsert status as "pending" in tag_store
     - Send `AutoTagRequest::TagDocument` via channel
  4. Clear `selected_files`
- Show progress as "Tagging N/M files..." in status bar

**Patterns to follow:**
- `retag_selected()` (lines 620–685) for extraction + hash + send pattern
- Reindex batch send (lines 820–850) for progress counter pattern

**Test scenarios:**
- Happy path: Select 2 text files, click button → both extracted and tagged
- Happy path: Select 1 PDF → pdf_oxide extracts text, tag request sent
- Edge case: File deleted between selection and tagging → skip with warning
- Edge case: No auto_tagger_tx (auto-tag disabled) → button not shown

**Verification:**
- Tags appear in tag panel for tagged files after processing
- Progress counter increments correctly
- Status bar shows "Tagged N/M files" during processing

---

### U3. Wire progress display and status feedback

**Goal:** Show tagging progress to the user during manual tag operations.

**Requirements:** R5

**Dependencies:** U2

**Files:**
- Modify: `src/app.rs`

**Approach:**
- Reuse existing `auto_tag_progress: Option<(usize, usize)>` field
- During manual tagging, set `auto_tag_progress = Some((completed, total))` and update as requests are sent
- The existing progress bar in the tag panel (line ~914) already reads `auto_tag_progress`
- Clear progress when tagging completes (all requests sent, or check `folder_runtime.auto_tag_progress`)

**Patterns to follow:**
- Existing progress bar rendering at line ~914: `ui.add(egui::ProgressBar::new(pct).text(format!("{completed}/{total}")))`

**Test scenarios:**
- Happy path: Tag 5 files → progress bar shows 0/5, 1/5, ..., 5/5
- Edge case: Progress completes → progress bar disappears

**Verification:**
- Progress bar visible during manual tagging
- Disappears or shows completion when done

---

## System-Wide Impact

- **Interaction graph:** File browser → auto-tagger channel → tagger thread → SQLite + Tantivy
- **Error propagation:** Failed extractions logged as warnings; individual file failures don't block batch
- **State lifecycle risks:** `selected_files` is cleared on tag trigger and on regular click; stale paths survive file list refreshes (by design — paths are stable)
- **Unchanged invariants:** Search, preview, sidebar panels, status bar unchanged

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| PDF extraction via pdf_oxide may be slow for large files | Only extract first few pages (mirror existing indexer behavior: 3 pages max) |
| Content extraction reads from disk on UI thread | Batch size is small (user-selected); if slow, spawn a background thread |
| `selected_files` grows stale if files are deleted | Skip missing files with a warning log |

---

## Sources & References

- Existing code: `src/app.rs` (file browser, retag_selected, reindex, AutoTagRequest)
- `src/indexer/extractors/pdf.rs` (pdf_oxide extraction)
- `src/auto_tagger/thread.rs` (tag_document processing)
