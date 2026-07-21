---
date: 2026-01-15
status: active
type: fix
---

# fix: Resolve P0+P1 Issues from Consolidated Expert Review

## Summary

Fix 10 P0 and 15 P1 issues identified by 4 independent expert reviewers (Security, Architecture, Concurrency, UX). Issues span crash recovery, data integrity, tag sync, UX dead-ends, and performance.

---

## Implementation Units

### U1. Tag Sync — Make TagUpdate Functional (P0-1)
- **Goal:** `Pipeline::process_tag_update` actually updates Tantivy tags instead of logging
- **Files:** `src/indexer/pipeline.rs`, `src/tags/store.rs`
- **Approach:** Look up body text from Tantivy, delete old doc, re-index with new tags. Or re-read file and re-extract.
- **Verification:** Tag assigned in UI reflects in search results within 2s

### U2. Graceful Shutdown — Commit Pending on Close (P0-2)
- **Goal:** Indexer final commit executes before process exit
- **Files:** `src/main.rs`, `src/indexer/pipeline.rs`, `src/watcher/watcher.rs`
- **Approach:** Add shutdown signal channel. On app close, signal indexer to flush+commit, wait with timeout.
- **Verification:** `cargo test` passes; shutdown test commits pending docs

### U3. Write Ordering + Reconciliation (P0-3)
- **Goal:** Implement SQLite-first write order per plan; add basic reconciliation
- **Files:** `src/indexer/pipeline.rs`
- **Approach:** Swap write order back to SQLite-first. Add iterate-Tantivy-docs → verify-SQLite in reconcile().
- **Verification:** Crash recovery test passes

### U4. Error Types — Wire PapervaultError (P0-4)
- **Goal:** Replace anyhow::Result with crate::error::Result in library code
- **Files:** `src/indexer/pipeline.rs`, `src/indexer/extractors/mod.rs`, `src/preview/pdf_render.rs`, `src/search/engine.rs`
- **Approach:** Convert pipeline, extractors, renderer to use `PapervaultError`. Keep anyhow only in main.rs.
- **Verification:** All tests pass, error variants used in production

### U5. Context-Aware Search Snippets (P0-5, R2)
- **Goal:** Use Tantivy's SnippetGenerator for context-aware snippets
- **Files:** `src/search/engine.rs`
- **Approach:** Replace raw first-200-chars with `SnippetGenerator::snippet_from_doc()` centered on match location.
- **Verification:** Snippets show text surrounding match term

### U6. Zero-Results Empty State (P0-6)
- **Goal:** Show "No results found" when search query returns empty
- **Files:** `src/app.rs`
- **Approach:** Add else-if branch in center panel render for empty results + non-empty query.
- **Verification:** Visual: type nonexistent term, see "No results for 'X'"

### U7. PDF Page Navigation (P0-7)
- **Goal:** Add next/prev page buttons to preview pane
- **Files:** `src/app.rs`
- **Approach:** Store current_page state, add arrow buttons above preview, update RenderRequest.
- **Verification:** Multi-page PDF shows page controls, navigation works

### U8. Native Folder Picker (P0-8)
- **Goal:** Replace text-input dialog with native Windows folder picker
- **Files:** `src/app.rs`, `Cargo.toml`
- **Approach:** Use `rfd::FileDialog::pick_folder()` or egui's native dialog integration.
- **Verification:** Click folder button → OS-native folder browser opens

### U9. Silent Engine Failure Display (P0-9)
- **Goal:** Show prominent error when search engine is unavailable
- **Files:** `src/app.rs`
- **Approach:** Render error banner in center panel + disable search input when engine is None.
- **Verification:** Visual: broken index shows clear error message

### U10. Move lopdf to dev-dependencies (P0-10)
- **Goal:** Remove test-only crate from production binary
- **Files:** `Cargo.toml`
- **Approach:** Move `lopdf` from `[dependencies]` to `[dev-dependencies]`.
- **Verification:** `cargo build --release` succeeds, binary smaller

### U11-20. P1 Bug Fixes
- U11: Highlight search terms in snippets (R3)
- U12: Debounce search-as-you-type (150ms)
- U13: SQLite connection caching (reuse connections)
- U14: Pipeline read-once (avoid double file read)
- U15: Replace Box::leak with owned handle for shutdown
- U16: Make unbounded channels bounded
- U17: dirs_next fallback → show error instead of "." path
- U18: Fix tag panel layout (tags inside results panel)
- U19: Add keyboard shortcuts (Enter, arrows, Ctrl+F)
- U20: Indexing progress bar instead of tiny label
- U21: Symlink filtering in watcher
- U22: WAL checkpoint on shutdown
- U23: Fix match_count metric
- U24: Remove Extractor trait private wrapper
- U25: Centralize supported extensions

### U26. Test Validation
- **Goal:** All 47 existing tests pass, new tests for fixes
- **Verification:** `cargo test` 0 failures
