# Technical Handoff — Papervault v1 → v1.1+

**Date:** 2026-07-21  
**Branch:** `feat/pdf-search-viewer`  
**Status:** v1 feature-complete, 47 unit tests passing, release binary 14MB

---

## What's Built (v1)

A Windows 11 desktop PDF/text search & viewer with egui, Tantivy, pdfium, and SQLite. Full-text search with snippets, tag management, PDF preview with page navigation, file system watching, crash recovery, and graceful shutdown.

### Key Technical Decisions

| Decision | Rationale | See |
|----------|-----------|-----|
| Tantivy `IndexReader` cloned for UI — lock-free search | Mutex-free search path, <1ms latency | `src/search/engine.rs` |
| Tags denormalized into Tantivy + SQLite post-filter | Immediate tag filtering without Tantivy re-index | `src/app.rs::do_search()` |
| Extractor chain built on indexer thread (not main) | Avoids `Send` requirement on pdfium | `src/indexer/pipeline.rs::run()` |
| `ReloadPolicy::Manual` in tests, `OnCommitWithDelay` in prod | Deterministic tests, async reload in production | `src/search/engine.rs` |
| Tokenizer registered under field name `"body"` | Tantivy 0.22 quirk — `TEXT` fields need namespaced tokenizer | `src/search/engine.rs:43` |
| SQLite WAL mode + `busy_timeout=5000` | Concurrent UI reads + indexer writes without SQLITE_BUSY | `src/tags/store.rs` |
| `Box::leak` for watcher debouncer | Lifetime must match process; alternative is `Arc<AtomicBool>` shutdown | `src/watcher/watcher.rs` |

### Test Coverage

- **47 unit tests** across config, error, search, extraction, tags, pipeline
- **1 ignored** (10K-doc performance benchmark)
- PDF tests gracefully skip when `pdfium.dll` unavailable
- No integration tests yet (`tests/` directory has fixtures only)

### Known Issues (Accepted for v1)

| Issue | Severity | Mitigation |
|-------|----------|------------|
| `PapervaultError` variants unused in library code | P2 | `anyhow::Result` used throughout; typed errors exist for future wiring |
| Tag Tantivy sync is deferred | P1 | UI post-filters via SQLite; tags apply on next file event |
| `match_count` always 0 or 1 | P2 | BM25 scoring provides relevance ordering |
| No integration tests | P2 | Unit tests cover core logic; manual testing for UI |
| Unbounded channels (except watcher) | P2 | Low-volume channels in desktop app; bounded for watcher backpressure |

---

## v1.1+ Roadmap

### P0 — Must Do

1. **OCR for scanned/image PDFs** — Add `OcrExtractor` to pipeline stages. Options: Windows built-in OCR API (`windows-rs`) or Tesseract via `leptess`. The `Extractor` trait and pipeline are designed for this — just add a new struct.
   - Files: `src/indexer/extractors/mod.rs` (add implementation), `src/indexer/stages.rs` (register in chain)
   - Related: `docs/plans/2026-01-15-001-feat-pdf-search-viewer-plan.md` Section "v2 extension points"

2. **AI auto-tagging** — Add `AiTagExtractor` to pipeline. Calls a local or cloud model to suggest/generate tags. The `TagUpdate` channel and `TagStore` are ready for programmatic tag assignment.
   - Files: `src/indexer/extractors/mod.rs`, `src/indexer/stages.rs`, `src/app.rs` (TagUpdate)
   - Key property: `document_tags` SQLite table is a simple junction — AI model can INSERT directly

3. **Fix tag Tantivy sync** — Store body text in SQLite so `process_tag_update` can re-index without file re-extraction.
   - Files: `src/tags/store.rs` (add `body_text` column), `src/indexer/pipeline.rs` (`process_tag_update`)

### P1 — Should Do

4. **Integration tests** — `tests/` directory per the test plan (`docs/test-plan.md`). Priority: pipeline end-to-end, concurrent search+index, crash recovery
5. **Keyboard shortcuts** — Enter to select, arrows to navigate, Ctrl+F to focus search
6. **Search history** — Persist last 10 queries in `Config`
7. **Light mode** — Follow egui `Visuals::light()` toggle
8. **Installer** — MSIX packaging via `cargo wix` or NSIS
9. **SQLite connection pooling** — Replace per-operation `connect()` with persistent connections
10. **Progress bar** — Visual progress during initial scan/indexing

### P2 — Nice to Have

11. **Multi-folder watching** — Extend config and watcher for multiple watch locations
12. **Markdown syntax stripping** — Clean snippet rendering for `.md` files
13. **Sort options** — Date modified, filename, file type
14. **Dark/light mode toggle** — Persist in config
15. **Zoom support** — Ctrl+= / Ctrl+- for UI and PDF zoom
16. **Open externally** — "Open with default app" and "Show in Explorer" actions
17. **Multi-select + batch tagging** — Ctrl+Click for multi-selection
18. **WAL checkpoint on shutdown** — `PRAGMA wal_checkpoint(TRUNCATE)` 
19. **Symlink filtering** — Skip symlinks in watcher
20. **CJK tokenizer** — Register `CangJieTokenizer` alongside `SimpleTokenizer`

---

## Architecture Quick Reference

### Thread Model
```
UI Thread    ─── search (lock-free reader), egui render
Indexer      ─── extraction → blake3 hash → Tantivy write → SQLite write → commit/2s
Renderer     ─── pdfium page → RGBA bitmap → channel → UI TextureHandle
Watcher      ─── notify-debouncer-full → IndexerMessage {Upsert, Delete}
```

### Channel Map
```
watcher_tx ──bounded(10k)──▶ watcher_rx → Pipeline
tag_tx     ──unbounded────▶ tag_rx    → Pipeline.process_tag_update()
render_tx  ──unbounded────▶ render_rx → PdfRenderer
render_rx  ──unbounded────▶ UI        → TextureHandle
progress_tx──unbounded────▶ UI        → status bar
shutdown   ──AtomicBool───▶ watcher stop → channel close → indexer exit
```

### Data Flow: Indexing
```
File event → Watcher.debounce(500ms) → IndexerMessage::Upsert
  → Pipeline.process_upsert()
    → metadata fast-path (SQLite: path+mtime+size → skip if unchanged)
    → extractor chain (PdfExtractor or TextExtractor)
    → blake3 hash + dedup check
    → old-hash cleanup if content changed
    → Tantivy index_document (delete_term + add_document)
    → SQLite upsert_document
    → commit every 10 docs or 2s
```

### Data Flow: Search
```
User types → 150ms debounce → do_search()
  → clone SchemaFields under brief Mutex
  → search_with_reader(fields, reader, request) — NO Mutex held
    → TermQuery on body + file_name (OR per term, AND across terms)
    → MultiCollector: Count + TopDocs(limit)
    → SnippetGenerator for context-aware snippets
    → If tag filters: post-filter via SQLite TagStore
```

### Crash Recovery
```
Startup → reconcile()
  → garbage_collect_files() (Tantivy stale segments)
  → for each Tantivy doc:
    → check file exists on disk
    → check SQLite has matching row
    → backfill missing SQLite rows from Tantivy stored fields
```

---

## Key Files by Concern

| Concern | Primary File | Secondary |
|---------|-------------|-----------|
| Search correctness | `src/search/engine.rs` | `src/search/schema.rs` |
| Indexing pipeline | `src/indexer/pipeline.rs` | `src/indexer/stages.rs` |
| PDF extraction | `src/indexer/extractors/pdf.rs` | `src/preview/pdf_render.rs` |
| File watching | `src/watcher/watcher.rs` | — |
| Tag storage | `src/tags/store.rs` | `src/tags/model.rs` |
| UI layout | `src/app.rs` | — |
| Thread orchestration | `src/main.rs` | — |
| Test plan | `docs/test-plan.md` | — |
| Design document | `docs/plans/2026-01-15-001-feat-pdf-search-viewer-plan.md` | — |
| Requirements | `docs/brainstorms/2026-01-15-pdf-search-viewer-requirements.md` | — |

---

## Common Development Tasks

### Running with console output (debugging)
Comment out `#![windows_subsystem = "windows"]` in `src/main.rs` and use `cargo run`.

### Adding a new file type
1. Add extension to `SUPPORTED_EXTENSIONS` in `src/indexer/extractors/mod.rs`
2. Create a new extractor implementing `Extractor` trait
3. Register it in `src/indexer/stages.rs::create_extractor_chain()`

### Adding a new tag operation
1. Add variant to `TagUpdate` enum in `src/app.rs`
2. Handle in `Pipeline::process_tag_update()` in `src/indexer/pipeline.rs`
3. Add UI in `src/app.rs`

### Running a specific test
```bash
cargo test search::engine::tests::index_and_search_single_document -- --nocapture
```

### Release build
```bash
cargo build --release
# Binary: target/release/papervault.exe (~14MB)
```

---

## Git Workflow

- **Main branch:** `master` — documents only (README, plans, test plan)
- **Feature branch:** `feat/pdf-search-viewer` — all source code
- **Branch naming:** `feat/<description>`, `fix/<description>`
- **Commit style:** conventional commits (`feat:`, `fix:`, `perf:`, `docs:`, `refactor:`)

---

## Contact / Repo

- **GitHub:** https://github.com/IvanYang007/papervault
- **Author:** Ivan Pi (`xn31415@gmail.com`)
- **License:** MIT
