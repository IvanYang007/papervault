---
title: fix: Startup scan misses files added while app was closed
type: fix
status: active
date: 2026-08-01
---

# fix: Startup scan misses files added while app was closed

## Summary

Make the pipeline always scan the watched folder at startup, skipping files whose (path, size, mtime) already exist in SQLite via the existing metadata fast-path. New or changed files get indexed; unchanged files cost one indexed SQLite lookup each. This closes the gap where files created while Papervault was closed never appear in the file panel.

---

## Problem Frame

`Z:\ScanOriginal\08012026145050.pdf` (an SMB-share scanner folder) was created at 14:50:50 while Papervault was closed; the app started at 14:51:55. The watcher (ReadDirectoryChangesW via `notify`) only delivers events that occur while it is watching, so the Create event was missed. Startup reconciliation (`reconcile()` in `src/indexer/pipeline.rs`) only syncs Tantivy ↔ SQLite and never touches the filesystem, and `Pipeline::run()` skips the initial scan entirely when the index is non-empty ("Index already has N documents — skipping initial scan", introduced by commit e8b8c3e as a startup-speed optimization). Verified: the file exists on disk but has zero rows in the SQLite `documents` table; a live probe confirmed the watcher itself works on the SMB share (Create event delivered in ~500 ms). Any file added or modified while the app is closed is silently missed forever.

---

## Requirements

- R1. Files created or modified while the app is closed are indexed at next startup and appear in the file panel.
- R2. Startup stays fast for unchanged folders: existing files are not re-extracted or re-indexed.
- R3. Files changed while the app was closed are re-indexed (stale text/tags refreshed).

---

## Scope Boundaries

- Does not prune index entries for files deleted while the app was closed (pre-existing gap, tracked separately).
- Does not change watcher behavior or notify configuration (verified working on the SMB share).
- Does not change the single-file watcher path (`process_upsert`) — it already uses the same metadata fast-path.

### Deferred to Follow-Up Work

- Files deleted while the app was closed remain in the index until re-synced; a future startup pass can compare the walked file set against the `documents` table and emit deletes.

---

## Context & Research

### Relevant Code and Patterns

- `src/indexer/pipeline.rs` — `Pipeline::run()` startup block (scan-skip branch), `process_upsert()` metadata fast-path, `process_batch()` batch pipeline, existing end-to-end test that calls `run()` with a seeded document.
- `src/tags/store.rs` — `already_indexed_by_metadata(path, size, mtime)`: indexed `SELECT COUNT(*)` on `file_path + file_size + modified_ts`; returns `SqlResult<bool>`.
- `src/watcher/watcher.rs` — `emit_initial_scan()`: walkdir of the folder emitting `IndexerMessage::Upsert` for supported files.
- Existing test conventions: `setup_tag_store()` helper; in-memory Tantivy engine setup; `metadata_fast_path_skips_unchanged_file` at the store level.

### Institutional Learnings

- None specific to watching; `D:/Github/docs/solutions/rust-windows/shutdown-coordination.md` covers thread shutdown discipline (scan loop already checks the shutdown flag per batch).

---

## Key Technical Decisions

- **Always run the startup scan; filter at the scan-message loop, not inside `process_batch`.** The scan loop in `run()` checks `already_indexed_by_metadata` per message and skips matches before batching. `process_batch` stays untouched (parallel extraction only for genuinely new/changed files), and the watcher path keeps its own fast-path in `process_upsert` — both converge on the same idempotent upsert.
- **Error direction on SQLite lookup failure: treat as "not indexed" (re-index).** Re-indexing is idempotent (content-hash-keyed upsert); skipping a file that actually changed would lose data.
- **Remove the `doc_count > 0` skip branch.** The scan becomes unconditional; correctness of the fast-path makes it cheap (1,431 walkdir entries + one indexed lookup each ≈ tens of ms on the target folder).

---

## Open Questions

### Resolved During Planning

- Is the watcher broken on the SMB share? No — live probe on `Z:\ScanOriginal` delivered Create events within the 500 ms debounce.
- Why is the file absent? It was created ~65 s before the watcher started, and the startup scan was skipped because the index was non-empty.

### Deferred to Implementation

- None. The fix surface is fully known.

---

## Implementation Units

### U1. Always-run startup scan with metadata fast-path

**Goal:** Files added or changed while the app is closed are indexed at startup; unchanged files are skipped cheaply.

**Requirements:** R1, R2, R3

**Dependencies:** None

**Files:**
- Modify: `src/indexer/pipeline.rs`
- Test: `src/indexer/pipeline.rs` (test module)

**Approach:**
- Remove the `doc_count > 0` skip branch in `run()` so the initial scan always executes.
- In the scan-message loop, before pushing an `Upsert` into the batch, call `tag_store.already_indexed_by_metadata(path_str, size, mtime)`; on match, skip with a debug log (same wording as `process_upsert`'s fast-path). On lookup error, fall through to batch (re-index is safe).
- Keep `scan_processed` counting only files actually processed; `ScanComplete` semantics unchanged.

**Execution note:** Write the failing regression test first (see scenarios), verify it fails on the current code, then implement the fix.

**Patterns to follow:**
- `process_upsert` fast-path in `src/indexer/pipeline.rs` (same skip condition and log style).
- End-to-end `run()` test pattern already present in the test module (seeded document, dropped channels, snapshot assertions).

**Test scenarios:**
- Happy path: seed index with doc A (real file on disk, metadata recorded), add file B to the watched folder, run `run()` with no watcher events → final `DocsSnapshot` contains B; A appears exactly once (not duplicated by the scan).
- Edge case: file A on disk has metadata matching the seeded row → scan skips it without re-extraction (assert A's content hash in the snapshot is unchanged).
- Edge case: file A on disk changed (different size/mtime) while "closed" → scan re-indexes it (snapshot shows the updated hash/path entry).

**Verification:**
- New regression test passes; full `cargo test` suite stays green.
- Against the real folder: after launching the app, `08012026145050.pdf` is indexed (log line "Initial scan complete: N files indexed", N ≥ 1) and listed by the file panel.

---

## System-Wide Impact

- **Interaction graph:** Startup scan now emits Upserts concurrently with watcher events; both funnel through the same idempotent upsert (content-hash keyed) — duplicates collapse.
- **State lifecycle risks:** A file created during startup could be queued by both the scan and the watcher; the second arrival hits the metadata fast-path and is skipped. Auto-tag requests are deduped via the existing `already_tagged`/status-row logic.
- **Integration coverage:** The end-to-end `run()` test covers the scan→index→snapshot path; watcher coverage already exists via the live probe.
- **Unchanged invariants:** `process_batch`, `process_upsert`, `emit_initial_scan`, `already_indexed_by_metadata`, and the SQLite schema are untouched.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Startup scan slows large folders | Fast-path skips unchanged files with one indexed lookup; only new/changed files are extracted. |
| DB lookup error during scan causes mass re-index | Error direction is re-index (idempotent); worst case duplicates collapse on hash key. |
| Scan re-indexes files mid-write by the scanner | Same risk as the watcher path today; upsert is content-hash idempotent and the file gets re-indexed on its next Modify event. |

---

## Documentation / Operational Notes

- None — internal behavior fix; no config, CLI, or docs surface changes.
