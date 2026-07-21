---
date: 2026-01-15
plan: docs/plans/2026-01-15-001-feat-pdf-search-viewer-plan.md
---

# Papervault — Test Strategy & Test Plan

## Test Strategy Overview

### Testing Philosophy

Papervault is a single-user desktop application with four threads, a search index, a SQLite database, and a GUI. The testing strategy prioritizes:

1. **Correctness of the indexing pipeline** — data corruption here silently breaks search.
2. **Search accuracy** — the core value proposition.
3. **Concurrency safety** — four threads with channels; races manifest as flaky tests.
4. **Error resilience** — corrupt PDFs, locked files, shutdown mid-index must not crash.

GUI rendering (egui panels, texture uploads, highlight overlays) is tested manually — egui apps have no practical automated UI test harness for pixel-perfect verification.

### Test Layers

| Layer | Directory | Tool | Scope |
|-------|-----------|------|-------|
| Unit tests | `src/` (inline `#[cfg(test)]` modules) | `cargo test` | Pure logic: extractors, schema, query builder, config, error types, tag store |
| Integration tests | `tests/` | `cargo test` | Multi-module: pipeline end-to-end, search correctness, watcher events, crash recovery, tag sync |
| Doc tests | `src/` (doc comments) | `cargo test --doc` | Public API examples on `SearchEngine`, `TagStore`, `Pipeline` |
| Benchmarks | `benches/` | `cargo bench` | Search latency at scale, indexing throughput, startup time |
| Visual tests | Manual | Human | PDF rendering quality, highlight positioning, UI layout, window resize |

### Test Fixtures

All fixtures live under `tests/fixtures/`. Generate PDF fixtures programmatically using the `printpdf` crate (dev-dependency only) to avoid checking binary files into git.

| Fixture | Source | Purpose |
|---------|--------|---------|
| `searchable.pdf` | Generated (printpdf, 1 page, known text) | Happy-path text extraction |
| `multipage.pdf` | Generated (printpdf, 5 pages, unique text per page) | Multi-page extraction |
| `corrupt.pdf` | Hand-crafted (truncated PDF header) | Error path: corrupt file |
| `password.pdf` | Generated with owner password | Error path: locked PDF |
| `empty.pdf` | Generated (printpdf, 0 pages) | Edge case: empty PDF |
| `text_and_image.pdf` | Generated (printpdf, text + embedded image) | Mixed content |
| `alice.txt` | UTF-8 plain text, known content | Text extraction |
| `readme.md` | UTF-8 markdown with formatting | Markdown extraction |
| `non_utf8.txt` | Latin-1 encoded text | Encoding error path |
| `empty.txt` | 0-byte file | Edge case: empty file |
| `large.txt` | Generated (>100MB) or truncated in test | Large file truncation |
| `app.log` | Log-style multi-line text | Log extraction |
| `crashed_index/` | Tantivy index from killed process | Crash recovery |

---

## Unit Tests

### Module: `src/config.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `load_returns_default_when_no_config_file` | Missing config file → default `Config` with no watched folder | Unit |
| `save_and_load_round_trips` | Write config to temp dir, read back → values preserved | Unit |
| `load_rejects_invalid_json` | Corrupt JSON → `Err`, not panic | Unit |
| `watched_folder_nonexistent_path_accepted` | Config stores path string; existence check is UI-layer concern | Unit |

### Module: `src/search/schema.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `schema_has_all_required_fields` | Tantivy schema builder produces schema with doc_id, file_path, file_name, body, file_type, modified_ts, content_hash, tags | Unit |
| `body_field_is_text_with_positions` | The `body` field uses `TEXT` options (includes `INDEXED` + positions) — required for `SnippetGenerator` | Unit |
| `content_hash_field_is_indexed_str` | The `content_hash` field is `Stored` + `Indexed` as `Str` — required for `delete_term` | Unit |
| `schema_rejects_duplicate_field_names` | Adding same field name twice → builder error | Unit |

### Module: `src/search/query.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `parse_single_term_builds_term_query` | Input "invoice" → Tantivy `TermQuery` on body field | Unit |
| `parse_multiple_terms_builds_boolean_and` | Input "invoice March" → `BooleanQuery` with two `TermQuery` MUST clauses | Unit |
| `search_request_default_limit_is_50` | `SearchRequest::new("query")` has `limit = 50` | Unit |
| `search_request_with_tag_filters` | Adding `tag_filters` produces correct `TermQuery` clauses on tags field | Unit |
| `empty_query_returns_empty_results` | Empty string or whitespace-only query → no Tantivy error, empty result set | Unit |

### Module: `src/search/engine.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `open_or_create_creates_new_index` | First call with fresh directory → new Tantivy `Index` on disk | Unit (temp dir) |
| `open_or_create_opens_existing_index` | Second call with existing directory → opens, does not overwrite | Unit (temp dir) |
| `index_and_search_single_document` | Index doc with known body text, search for term → found, snippet contains term | Unit (temp dir) |
| `search_absent_term_returns_empty` | Index doc, search for term not in doc → empty results | Unit (temp dir) |
| `search_respects_limit` | Index 10 docs, search with `limit: 3` → max 3 results | Unit (temp dir) |
| `search_reports_overflow_count` | Index 10 docs, search common term with `limit: 3` → `total_hits = 10`, results truncated | Unit (temp dir) |
| `delete_document_removes_from_search` | Index doc, delete by `content_hash`, search → not found | Unit (temp dir) |
| `garbage_collect_files_does_not_crash` | Call after dirty shutdown (uncommitted segments) → no panic | Unit (temp dir) |
| `search_performance_with_10k_docs` | Index 10K minimal docs, search → completes in <10ms (warn if >50ms) | Unit (temp dir) |

### Module: `src/indexer/extractors/pdf.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `extract_searchable_pdf_returns_text` | Known-text PDF → `Ok(Some(ExtractedContent { text: "…", page_count: Some(1) }))` | Unit |
| `extract_multipage_pdf_returns_all_pages` | 5-page PDF → text from all pages concatenated, `page_count = 5` | Unit |
| `extract_no_text_layer_returns_empty` | PDF with no text → `Ok(Some(...))` with empty `text`, not `Err` | Unit |
| `extract_corrupt_pdf_returns_error` | Truncated PDF → `Err`, not panic | Unit |
| `extract_password_protected_pdf_returns_error` | Locked PDF → `Err`, not hang | Unit |
| `extract_empty_pdf_returns_zero_pages` | 0-page PDF → `Ok(Some(...))` with `page_count: Some(0)`, empty text | Unit |
| `extract_mixed_content_pdf_returns_text_only` | PDF with text + image → text extracted, no image data in output | Unit |
| `extract_non_pdf_file_returns_none` | `.txt` file passed → `Ok(None)` (not an error — try next extractor) | Unit |
| `pdfium_instance_reused_across_extractions` | Call `extract` twice on different PDFs → same `Pdfium` instance, no re-init overhead | Unit |

### Module: `src/indexer/extractors/text.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `extract_utf8_txt_returns_content` | Known text file → content matches exactly | Unit (temp file) |
| `extract_markdown_returns_raw_text` | `.md` file with `# Heading` and `**bold**` → raw markdown preserved | Unit (temp file) |
| `extract_log_returns_content` | `.log` file → content matches | Unit (temp file) |
| `extract_non_utf8_lossy_decode` | Latin-1 file → `String::from_utf8_lossy` applied, content decoded | Unit (temp file) |
| `extract_empty_file_returns_empty_string` | 0-byte file → `text: ""`, not error | Unit (temp file) |
| `extract_large_file_truncates` | >100MB file → only first 10MB in output text | Unit (temp file) |
| `extract_non_text_extension_returns_none` | `.pdf` or `.exe` passed → `Ok(None)` | Unit (temp file) |
| `extract_missing_file_returns_error` | Path that doesn't exist → `Err` | Unit |

### Module: `src/tags/store.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `create_tag_returns_id` | Create tag "tax" → returns `id`, appears in `list_tags` | Unit (in-memory DB) |
| `create_duplicate_tag_returns_error` | Create "tax" twice → second call returns `Err` | Unit (in-memory DB) |
| `assign_tag_to_document_succeeds` | Assign tag to content_hash → `get_tags_for_document` returns it | Unit (in-memory DB) |
| `assign_multiple_tags_to_document` | Assign 3 tags → all 3 returned | Unit (in-memory DB) |
| `remove_tag_assignment_succeeds` | Assign then remove → tag no longer in `get_tags_for_document` | Unit (in-memory DB) |
| `delete_tag_cascades_to_document_tags` | Create tag, assign to doc, delete tag → no orphaned `document_tags` row | Unit (in-memory DB) |
| `get_documents_with_tag_returns_correct_docs` | Assign tag to 2 docs, query → 2 content_hashes returned | Unit (in-memory DB) |
| `concurrent_reader_during_writer_no_busy` | WAL mode: reader on one connection, writer on another → reader not blocked | Unit (in-memory DB) |

### Module: `src/error.rs`

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `error_types_implement_std_error` | All error variants satisfy `std::error::Error` | Unit (compile-time) |
| `error_display_is_human_readable` | Error messages include context, not just raw OS strings | Unit |

### Module: `src/indexer/pipeline.rs` (unit-level)

| Test | What it verifies | Test type |
|------|-----------------|-----------|
| `metadata_fast_path_skips_unchanged_file` | Same `(path, mtime, size)` → `already_indexed_by_metadata` returns `true`, extraction skipped | Unit (mocked) |
| `content_hash_dedup_skips_duplicate` | Same content hash, different path → `already_indexed_by_hash` returns `true`, only path updated | Unit (mocked) |
| `sqlite_written_before_tantivy` | Verify SQLite row exists before Tantivy commit in `process` flow | Unit (mocked) |
| `commit_cadence_every_10_docs` | Process 25 docs → 3 commits (10, 10, 5) | Unit (mocked) |
| `commit_cadence_every_2_seconds` | Process 3 docs slowly → timer-based commit fires | Unit (mocked, tokio time) |
| `shutdown_commits_pending_and_acknowledges` | Send shutdown signal with 3 pending docs → all committed before ack | Unit (mocked) |
| `failed_file_logged_to_last_error` | Corrupt PDF → `documents.last_error` populated, pipeline continues | Unit (mocked) |
| `failed_file_retried_on_change` | Same path, different `mtime` → retry attempted | Unit (mocked) |

---

## Integration Tests

### File: `tests/search_integration.rs`

| Test | Covers | What it verifies |
|------|--------|-----------------|
| `index_five_docs_search_finds_correct_one` | U3, R1 | Index 5 docs with distinct text, search for term in doc 3 → only doc 3 returned |
| `search_and_combines_multiple_terms` | U3, R1 | Index docs with "invoice" and "March", search "invoice March" → only docs with both |
| `snippet_generator_includes_highlighted_term` | U3, R2 | Search result snippet contains the matched term with highlight markers |
| `search_after_delete_excludes_document` | U3, R8 | Index doc, delete via Tantivy, search → not found |
| `search_during_indexing_sees_committed_data` | U7, R7 | Indexer commits every 10 docs; search during batch of 25 → sees incremental results at commit boundaries |
| `stress_10k_doc_index_and_search` | U3, R1 | Index 10K minimal docs, run 100 random searches → all complete in <50ms |

### File: `tests/pipeline_integration.rs`

| Test | Covers | What it verifies |
|------|--------|-----------------|
| `new_file_through_pipeline_becomes_searchable` | U7, R7 | Write file to watched dir → pipeline indexes → search finds it |
| `modified_file_reindexed` | U7, R7 | Modify file content → re-indexed, search returns new text |
| `renamed_file_skips_reindex` | U7 | Rename file (same content) → metadata fast-path: no re-extraction, path updated |
| `deleted_file_removed_from_both_stores` | U7, R8 | Delete file → Tantivy search doesn't find it, SQLite row gone |
| `batch_200_files_all_indexed` | U7, R12 | Create 200 files in temp dir → all indexed within timeout, all searchable |
| `toctou_detection_triggers_retry` | U7 | Modify file between hash and extraction → retry fires, final hash matches |
| `corrupt_pdf_does_not_block_pipeline` | U7 | Corrupt PDF + valid PDF in same batch → valid indexed, corrupt logged to `last_error` |
| `pipeline_rejects_unsupported_extension_silently` | U7 | `.exe` in watched folder → not indexed, no error |
| `graceful_shutdown_commits_pending` | U7 | Send shutdown mid-batch → pending docs committed, no data loss |
| `reconcile_removes_orphaned_tantivy_docs` | U7 | Manually create Tantivy doc with no SQLite row → reconcile removes it |
| `reconcile_removes_missing_file_docs` | U7 | Delete file from disk (not through watcher), restart → reconcile removes from both stores |
| `reconcile_marks_stale_files_for_reindex` | U7 | Change file mtime externally → reconcile enqueues for re-indexing |

### File: `tests/watcher_integration.rs`

| Test | Covers | What it verifies |
|------|--------|-----------------|
| `create_pdf_emits_event` | U4, R6 | Create `.pdf` in watched dir → single event after 500ms debounce |
| `rapid_writes_produce_single_event` | U4 | Write same file 5 times rapidly → 1 event, not 5 |
| `delete_emits_immediate_event` | U4, R8 | Delete file → event within 200ms (no debounce) |
| `unsupported_extension_no_event` | U4 | Create `.jpg` → no event emitted |
| `initial_scan_emits_events_for_existing_files` | U4 | Watcher starts with files already present → event for each existing file |
| `rename_emits_both_old_and_new_path_events` | U4 | Rename file → old path removed, new path added |
| `watcher_survives_watched_dir_deletion` | U4 | Delete watched folder → watcher errors gracefully, not panic |

### File: `tests/tag_integration.rs`

| Test | Covers | What it verifies |
|------|--------|-----------------|
| `tag_assignment_survives_restart` | U8, R9 | Assign tag, close DB, reopen → tag still assigned |
| `tag_filter_narrows_search_results` | U9, R10 | Tag doc A "tax", doc B "receipt"; search "report" with tag "tax" → only doc A |
| `multiple_tag_filters_and_semantics` | U9, R10 | Tag doc with "tax" and "2025"; filter both → doc appears; filter "tax"+"2024" → not |
| `tag_change_synced_to_tantivy` | U9 | Assign tag via UI channel → next search with tag filter includes document |
| `tag_remove_synced_to_tantivy` | U9 | Remove tag via UI channel → next search with tag filter excludes document |
| `delete_tag_cascades_to_search_results` | U8, U9 | Tag assigned to doc, delete tag → tag filter no longer matches doc |
| `concurrent_read_write_no_sqlite_busy` | U8 | Reader thread queries tags while writer commits → no `SQLITE_BUSY` (WAL mode) |

### File: `tests/concurrency_integration.rs`

| Test | Covers | What it verifies |
|------|--------|-----------------|
| `search_during_indexing_no_data_race` | U3, U7 | Spawn search thread + indexer thread concurrently → no panic, consistent results (MVCC snapshot) |
| `tag_sync_during_indexing_no_deadlock` | U8, U9 | Indexer processes files while UI sends tag updates → both complete, no deadlock |
| `render_request_during_indexing_no_contention` | U10, U7 | Indexer extracts PDF while renderer renders PDF → separate `Pdfium` instances, no conflict |
| `shutdown_during_active_operations` | Graceful Shutdown | Send shutdown while indexer and renderer are busy → both finish current operation, ack within timeout |
| `channel_backpressure_watcher_to_indexer` | U4, U7 | Flood watcher with 15K events (exceeds bounded 10K) → oldest dropped or sender blocks, no panic |

---

## Benchmark Tests

Use `cargo bench` (nightly) or criterion. Place under `benches/`.

### `benches/search_bench.rs`

| Benchmark | What it measures | Target |
|-----------|-----------------|--------|
| `search_single_term_10k_index` | BM25 search latency for 1 term against 10K docs | <1ms |
| `search_three_terms_10k_index` | BM25 search latency for 3 AND terms against 10K docs | <5ms |
| `search_with_tag_filter_10k_index` | Search + 2 tag filters against 10K docs | <5ms |
| `snippet_generation_50_results` | Generate snippets for top 50 search results | <10ms |

### `benches/indexing_bench.rs`

| Benchmark | What it measures | Target |
|-----------|-----------------|--------|
| `index_single_pdf_50_pages` | Wall-clock time to extract + index a 50-page PDF | (baseline, no hard target) |
| `index_batch_200_text_files` | Throughput: files/second for batch text extraction | (baseline) |
| `startup_open_existing_10k_index` | Cold-open time for 10K-doc Tantivy index | <1s |

---

## Test Execution Commands

```bash
# All tests
cargo test --workspace --all-targets

# Unit tests only (fast feedback)
cargo test --lib

# Integration tests only
cargo test --test '*' 

# Specific integration test file
cargo test --test pipeline_integration

# Run with output (for debugging)
cargo test -- --nocapture

# Run ignored (slow/stress) tests
cargo test -- --ignored

# Benchmarks (nightly)
cargo bench

# With nextest (faster, if installed)
cargo nextest run --all-features
```

### CI-Only Stress Tests (marked `#[ignore]`)

These tests take minutes to run and belong in CI, not local `cargo test`:

| Test | Why ignored locally |
|------|--------------------|
| `stress_10k_doc_index_and_search` | ~5 minutes to generate and index 10K docs |
| `batch_200_files_all_indexed` | ~30 seconds for PDF extraction on 200 files |
| `initial_scan_10k_existing_files` | Startup simulation with 10K files |

---

## Coverage Targets

| Module | Target | Rationale |
|--------|--------|----------|
| `src/search/` | 90%+ line coverage | Core value — search correctness is non-negotiable |
| `src/indexer/` | 85%+ line coverage | Pipeline is the data integrity boundary |
| `src/tags/` | 85%+ line coverage | SQLite operations are error-prone |
| `src/watcher/` | 75%+ line coverage | Mostly glue code; edge cases in integration tests |
| `src/preview/` | Manual verification | PDF rendering is visual; automated tests cover RGBA output correctness |
| `src/config.rs` | 90%+ | Simple, easily covered |
| `src/error.rs` | 100% | Trivial derive — one test confirms trait impls |
| `src/app.rs` | Manual verification | egui UI code — visual correctness, not logic coverage |

Use `cargo tarpaulin` for coverage reports:
```bash
cargo tarpaulin --out Html --output-dir coverage/
```

---

## Test Data Generation

PDF fixtures are generated programmatically at test time using `printpdf` (dev-dependency):

```rust
// tests/common/mod.rs
use printpdf::*;
use std::fs;

pub fn generate_searchable_pdf(path: &Path, text: &str) {
    let (doc, page_idx, layer_idx) = PdfDocument::new("Test PDF", Mm(210.0), Mm(297.0), "Layer 1");
    let font = doc.add_builtin_font(BuiltinFont::Helvetica).unwrap();
    let current_layer = doc.get_page(page_idx).get_layer(layer_idx);
    current_layer.use_text(text, 12.0, Mm(10.0), Mm(280.0), &font);
    doc.save(&mut fs::File::create(path).unwrap()).unwrap();
}
```

This avoids binary fixtures in git and ensures deterministic test data. For the corrupt PDF fixture, hand-craft a file with a truncated header. For the password-protected PDF, use `printpdf` with encryption options.

---

## What Each Test Protects

| Risk | Tests that catch it |
|------|--------------------|
| Search returns wrong documents | `search_integration.rs` — all |
| Deleted file still appears in results | `pipeline_integration.rs::deleted_file_removed_from_both_stores` |
| Corrupt PDF crashes the app | `pipeline_integration.rs::corrupt_pdf_does_not_block_pipeline` |
| Data loss on shutdown | `pipeline_integration.rs::graceful_shutdown_commits_pending` |
| Index corruption after crash | `pipeline_integration.rs::reconcile_*` |
| Tag changes not reflected in search | `tag_integration.rs::tag_change_synced_to_tantivy` |
| SQLITE_BUSY on concurrent access | `tag_integration.rs::concurrent_read_write_no_sqlite_busy` |
| UI freeze during indexing | `concurrency_integration.rs::search_during_indexing_no_data_race` |
| Search returns stale results | `search_integration.rs::search_during_indexing_sees_committed_data` |
| Half-written file indexed | `pipeline_integration.rs::toctou_detection_triggers_retry` |
| Renamed file re-indexed unnecessarily | `pipeline_integration.rs::renamed_file_skips_reindex` |
| Duplicate files silently deleted | `pipeline_integration.rs::*` (content-hash dedup edge case) |

---

## Coverage Gaps (Deliberate)

| Area | Why not covered | How verified |
|------|----------------|-------------|
| egui panel layout correctness | No practical egui UI test harness for winit apps | Manual: launch app, verify 3-panel layout, resize window |
| PDF highlight pixel accuracy | Requires visual comparison of rendered bitmap vs expected overlay | Manual: open known PDF, search, visually verify highlight positions |
| pdfium lazy-init spinner UX | Transient UI state, hard to assert in code | Manual: cold-start app, click first result, verify spinner appears |
| Texture cache eviction GPU memory | GPU state not inspectable from Rust tests | Manual: open many PDFs, monitor GPU memory via Task Manager |
| Scanner integration (real hardware) | Requires physical scanner | Manual: user tests with their scanner |
| Windows 11-specific file dialog behavior | OS-specific, not mockable | Manual: verify on target Windows 11 machine |
