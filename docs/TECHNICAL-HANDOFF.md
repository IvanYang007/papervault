# Technical Handoff — Papervault v2.0

**Date:** 2026-07-31  
**Branch:** `master`  
**Status:** ✅ v3.0 complete — 176 tests, clippy clean, release build

---

## What's Built

Windows 11 desktop PDF/text search & viewer. Rust + egui 0.30 + Tantivy 0.22 + pdfium-render 0.8.37 + pdf_oxide 0.3 + SQLite + rayon.

| Feature | Status |
|---------|--------|
| Full-text search with highlighted snippets | ✅ |
| Recursive subfolder indexing (walkdir) | ✅ |
| Parallel extraction via rayon (8→32 batch size) | ✅ |
| File browser panel with refresh cooldown | ✅ |
| PDF preview — two-pass rendering (low-res→full-res) | ✅ |
| PDF preview — LRU page cache (8 pages, true LRU) | ✅ |
| PDF preview — display-resolution rendering | ✅ |
| PDF preview — page prefetch (N+1 during idle) | ✅ |
| PDF preview — render at display resolution | ✅ |
| Text file preview with 2MB cap | ✅ |
| Tag management (create, assign, filter) | ✅ |
| Tag post-filtering with 200-result window | ✅ |
| FolderRuntime lifecycle (start/stop/switch) | ✅ |
| Per-folder index via blake3 hash | ✅ |
| Lock-free search (SchemaFields pre-cloned) | ✅ |
| Graceful shutdown (thread joins in order) | ✅ |
| SQLite connection caching (Arc&lt;Mutex&lt;Connection&gt;&gt;) | ✅ |
| Streaming/blake3 content hashing (text-based, not file bytes) | ✅ |
| Batch reconcile with HashSet preload | ✅ |
| Duplicate document cleanup on file modification | ✅ |

---

## Architecture

```
4 Threads + Rayon pool (for initial scan):
  UI         — egui rendering, lock-free Tantivy search, file browser, preview
  Indexer    — pdf_oxide/TextExtractor → blake3 → Tantivy + SQLite write
  Renderer   — pdfium page → RGBA bitmap → channel → UI TextureHandle
  Watcher    — notify-debouncer-full → recursive folder watching

6 Channels:
  watcher_tx     ──bounded(256)──▶ watcher_rx → Pipeline
  tag_tx         ──unbounded────▶ tag_rx    → Pipeline (no-op handler, kept for future)
  render_tx      ──unbounded────▶ render_rx → PdfRenderer (coalescing: latest-wins)
  result_tx      ──unbounded────▶ UI        → TextureHandle
  progress_tx    ──unbounded────▶ UI        → status bar (+ DocsSnapshot for file browser)
  auto_tagger_tx ──bounded(256)──▶ 3 workers → DeepSeek (DB is the durable queue)

Layout:
  Left panel:    📂 File browser (virtualized table — resizable Name/Modified/Size columns)
                 🏷 Tag panel (pre-computed cache, updated on tag change)
  Center top:    🔍 Search bar
  Center below:  Search results (when typing) OR file preview
  Bottom:        Status bar
```

---

## Key Technical Decisions (v2.0)

| Decision | Rationale |
|----------|-----------|
| `pdf_oxide` for indexer (replaces pdf-extract) | ~5x faster extraction (0.8ms vs 4.08ms), 100% pass rate on 3,830 PDFs |
| `rayon` parallel extraction for initial scan | 32-file batches extracted in parallel, then indexed sequentially |
| `Arc<Mutex<Connection>>` in TagStore | Single persistent connection, ~100x faster than per-operation connect() |
| Content hash from extracted text | Eliminates double file I/O (no separate hashing pass) |
| Initial scan on pipeline thread | Watcher never blocks on bounded channel — always responds to shutdown |
| Two-pass rendering (low-res → full-res) | ~10ms low-res preview appears, replaced by full-res within ~100ms |
| 8-entry LRU page cache | Back-navigation is instant (0ms from cache), true LRU ordering |
| Display-resolution rendering | Renders at preview panel pixel size, not fixed 2000px max |
| Page prefetch (N+1 during idle) | Forward navigation feels instant after current page renders |
| Tag post-filter with 200-result window | Prevents false-empty results when tag matches fall outside top-50 |
| HashSet-based reconcile preload | Single SQL query loads all hashes → O(1) in-memory lookups |
| File browser refresh cooldown (30 frames) | Rate-limits `list_all_documents()` during active indexing |
| Pre-lowercased match terms in do_search | Eliminates per-frame String allocations in snippet highlighting |
| (v3.0) Cached parsed PdfDocument per file | PDF parsed once per file instead of per render — ~6x faster page flips/zooms |
| (v3.0) Atomic claim_pending_auto_tags (UPDATE…RETURNING) | Concurrent workers each claim disjoint rows — zero duplicate AI calls |
| (v3.0) Batched SQLite writes (one tx per 32-file batch) | ~11x faster scan writes; per-item fallback on batch failure |
| (v3.0) Circuit breaker on the DeepSeek endpoint | A dead API fails fast (no hours of churn); half-open probe after cooldown |
| (v3.0) File-browser snapshots on the indexer thread | list_all_documents never runs on the UI thread (async snapshot channel) |
| (v3.0) INSERT…ON CONFLICT instead of INSERT OR REPLACE | Re-indexing no longer fires the FK cascade — AI and manual tags survive |
| (v3.0) already_tagged guard in the indexer | Never wipes tags or re-calls the API for unchanged tagged content |
| (v3.0) DB-driven re-index for tags | Rows marked pending first — one click re-tags the whole library, drops self-heal |
| (v3.0) prepare_cached + 64-slot statement cache | Hot SQL compiled once per connection |
| (v3.0) egui_extras TableBuilder (resizable columns) | Explorer-style Name/Modified/Size resize; rows() keeps virtualization |

---

## Performance Benchmarks (700-file collection)

| Metric | Value |
|--------|-------|
| Initial scan (small PDFs) | ~1.5s |
| Initial scan (large PDFs) | ~20s |
| Search (any collection size) | <10ms |
| PDF page flip (cache hit) | 0ms |
| PDF page flip (cache miss) | ~10ms low-res, ~80ms full-res |
| Release build time | ~110s |
| Release binary size | ~19 MB |
| Memory (10K doc vault) | ~300-500MB |
| (v3.0) SQLite writes, 5000-file scan | 402 ms → 36 ms (batched transactions, 11x) |
| (v3.0) PDF page flip (warm) | 2.2 ms → 0.37 ms (cached document, 6x) |
| (v3.0) Auto-tag status fetch (50 results) | 435 µs → 152 µs (one batch query, 3x) |
| (v3.0) Per-frame SQLite queries | 50/frame → 0 (in-memory display cache) |

---

## Files by Concern

| Concern | File |
|---------|------|
| PDF rendering | `src/preview/pdf_render.rs` |
| PDF extraction (indexer) | `src/indexer/extractors/pdf.rs` |
| Search engine | `src/search/engine.rs` |
| UI (app state, search, preview, layout) | `src/app.rs` |
| Indexing pipeline | `src/indexer/pipeline.rs` |
| Runtime orchestrator | `src/runtime.rs` |
| File watcher | `src/watcher/watcher.rs` |
| Tag storage | `src/tags/store.rs` |
| Thread orchestration, channels | `src/main.rs` |
| Config (atomic save) | `src/config.rs` |
| Error types | `src/error.rs` |

---

## Fresh Setup

```powershell
git clone https://github.com/IvanYang007/papervault.git
cd papervault
git checkout feat/pdf-oxide-extraction
cargo build --release
# Binary: target/release/papervault.exe (~19 MB)

# Place pdfium.dll next to papervault.exe
# Download Chromium 7543 from:
# https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium/7543

# Run:
.\target\release\papervault.exe
```

---

## Key Git Commits

```
bb900f9 fix(review): 3 P1 bugs from final review cycle
97bb803 test: add 3 tests for parallel extraction
466dd9f perf: increase parallel batch size 8→32
3b0fd3b fix(review): P1 prefetch cancels full-res render + progress reset
862fb52 feat: parallel extraction for initial file scan via rayon
7c476e6 perf(review): eliminate per-frame String allocations in hot paths
553566b fix(review): rust-code-review findings
4e74f71 perf: 2x faster compile with thin LTO
c0a657a fix: freeze on window close
8e83efd perf: aggressive compile-speed tuning
af92540 simplify: address ce-simplify-code review findings
efcd109 perf: file browser refresh cooldown
6429326 perf: fix P0/P1 performance issues
ef68dc8 fix: address rust-code-review P2 findings
032c983 feat: replace pdf-extract with pdf_oxide
867a0fa fix: search engine init failure on Windows
```

---

## v3.1 — Auto-Tagging Cost & Stability Fixes (2026-08-04)

Three root causes were found in one debugging session after the auto-tagger
re-tagged ~250-400 files (paid DeepSeek calls) on every launch. Full write-up
(agent-readable, with deploy checklist): `D:/Github/docs/solutions/rust-windows/
auto-tagger-retag-churn.md`.

| Commit | Fix | Root cause |
|--------|-----|------------|
| `bf2a5e9` | Watcher only honors Remove events when the file is really gone (`path.exists()` check in `watcher.rs` + `process_delete`) | SMB shares report spurious `FILE_ACTION_REMOVED`; each one cascade-wiped the document row and its AI tags, forcing a re-tag |
| `dbe85db` | Send `thinking: {"type": "disabled"}` by default; config key `thinking_enabled` (was `thinking_effort`, an invalid DeepSeek param the API silently ignored) | Every call ran chain-of-thought at full strength: 2-3x latency, empty responses, ~14.6K reasoning tokens per 500-word doc |
| `a9f2231` | `already_tagged`/tier-1 accept the content-only identity `blake3(text)`; new rows store it; legacy name-based rows still match | Duplicate scans of one document under different filenames shared a row, but `content_hash_before_tag` included the filename — every name-variant re-tagged every session |

**Verification:** after the one-time migration pass, a launch shows `0`
DeepSeek calls and the tagging queue stays at zero (871/871 tagged).

---

## Remaining Work

| Priority | Item |
|----------|------|
| ✅ | File browser virtualization + resizable columns (egui_extras table, v3.0) |
| P2 | OCR for scanned PDFs |
| P2 | Keyboard shortcuts |
| P2 | Release packaging / installer |
| P3 | Latin-1/Windows-1252 encoding support |
| P3 | CJK tokenizer |
