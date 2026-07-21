---
date: 2026-01-15
status: active
origin: docs/brainstorms/2026-01-15-pdf-search-viewer-requirements.md
deepened:
reviewed: 2026-01-15
review_round: 4
---

# feat: PDF Search & Viewer — Design Document (v2)

## Summary

Design for a Windows 11 desktop application (Rust + egui) that watches a single folder, indexes PDFs and text files via Tantivy for instant full-text search, and provides a single-window search-with-preview experience. pdfium handles PDF text extraction and page rendering. The indexing pipeline uses a pluggable stage architecture to accept OCR and AI auto-tagging in v2.

**Target repo:** papervault (greenfield)

---

## Problem Frame

See origin document `docs/brainstorms/2026-01-15-pdf-search-viewer-requirements.md`. The user manages 1,000–10,000 searchable PDFs and text files. Windows Search is too slow at this scale and does not search inside PDF content effectively. The goal is an old-Evernote-style single-window search viewer: type → results appear with highlights → click to preview.

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│                      egui Application                        │
│  ┌──────────┐  ┌──────────────┐  ┌────────────────────────┐  │
│  │ Search   │  │ Results List │  │ Preview Pane           │  │
│  │ Bar      │  │ (snippets)   │  │ (TextureHandle from    │  │
│  │          │  │              │  │  render thread via     │  │
│  │          │  │              │  │  channel + highlights) │  │
│  └──────────┘  └──────────────┘  └────────────────────────┘  │
│                     │                                         │
│              ┌──────┴──────┐                                  │
│              │  App State  │                                  │
│              │  (channels) │                                  │
│              └──────┬──────┘                                  │
└─────────────────────┼────────────────────────────────────────┘
                      │
        ┌─────────────┼──────────────────┐
        ▼             ▼                  ▼
┌───────────┐  ┌────────────┐  ┌──────────────────┐  ┌──────────┐
│  Search   │  │  Indexer   │  │  PDF Renderer    │  │  Watcher │
│  Engine   │  │  Pipeline  │  │  Thread          │  │ (notify- │
│ (tantivy) │  │            │  │  (pdfium)        │  │debouncer │
│           │  │            │  │                  │  │ -full)   │
│  on UI    │  │            │  │  renders pages    │  │          │
│  thread   │  │            │  │  → sends bitmaps  │  │          │
└───────────┘  └─────┬──────┘  └──────────────────┘  └─────┬────┘
                     │                                      │
              ┌──────┴──────┐                               │
              │  Extractors │                               │
              │  ┌────────┐ │                               │
              │  │ PDF    │ │                               │
              │  │(pdfium)│ │                               │
              │  ├────────┤ │                               │
              │  │ Text   │ │                               │
              │  │(direct)│ │                               │
              │  ├────────┤ │                               │
              │  │ OCR v2 │ │                               │
              │  ├────────┤ │                               │
              │  │ AI v2  │ │                               │
              │  └────────┘ │                               │
              └──────┬──────┘                               │
                     │                                      │
              ┌──────┴──────┐                               │
              │   SQLite    │                               │
              │ (WAL mode,  │                               │
              │  2 conns,   │                               │
              │  busy_tmo)  │                               │
              └─────────────┘                               │
                     │                                      │
            ┌────────┴────────┐                             │
            │  File System    │◄────────────────────────────┘
            │  (watched dir)  │
            └─────────────────┘
```

**Thread model (4 threads):**

| Thread | Role |
|--------|------|
| **UI thread** | egui render loop, user input, Tantivy search queries (lock-free reader) |
| **Indexer thread** | Runs the extraction pipeline, commits to Tantivy, writes SQLite metadata |
| **Renderer thread** | Owns pdfium instance, renders PDF pages to bitmaps on request, sends back via channel |
| **Watcher thread** | `notify-debouncer-full` event loop, enqueues files for indexing |

**Communication:** Crossbeam channels between all threads. The UI thread sends `RenderRequest(page)` to renderer thread; renderer sends `RenderResult(bitmap)` back. Watcher sends `IndexerMessage` to indexer via `crossbeam::bounded(10_000)`. Indexer sends `IndexingProgress` to UI thread via `crossbeam::unbounded`. UI thread sends `TagUpdateMessage` to indexer via `crossbeam::unbounded` (low volume). Shutdown signal flows from UI thread to all background threads.

**Graceful shutdown:** On app close (egui `on_close_event`), UI thread sends shutdown signal to all background threads, waits for each to flush and acknowledge, then exits. Indexer commits final batch before stopping. See [Graceful Shutdown](#graceful-shutdown) below.

---

## Crate Selection

| Component | Crate | Rationale |
|-----------|-------|-----------|
| GUI | `egui` + `eframe` | Pure Rust, fast, good for data-dense UIs. Default wgpu backend. |
| Search index | `tantivy` | Rust-native full-text search, lock-free readers, incremental commits. |
| PDF text/rendering | `pdfium-render` | Rust bindings to Chromium's pdfium. Single engine for both extraction and rendering. |
| File watching | `notify` + `notify-debouncer-full` | `notify-debouncer-full` handles event dedup, rename tracking, and backpressure. Avoids fragile manual debounce implementation. |
| Metadata/tags | `rusqlite` | SQLite for tag storage. WAL mode for concurrent reads+writes. |
| Channels | `crossbeam` | MPMC channels for UI ↔ background thread communication. |
| Content hashing | `blake3` | Fast content hash for detecting file changes and stable document identity. Unkeyed — suitable for dedup, not for adversarial contexts. |
| Logging | `tracing` + `tracing-subscriber` | Structured logging. |
| Error handling | `anyhow` + `thiserror` | anyhow for application code, thiserror for library-level error types. Extractors return `anyhow::Result` — errors are logged, not programmatically matched. |
| Config | `dirs-next` | Actively maintained fork of `dirs`. `data_local_dir()` for index, `config_dir()` for settings. |

---

## Data Model

### Tantivy Schema

```text
Field               Type        Stored  Indexed  Fast
───                 ────        ──────  ───────  ────
doc_id              Str          yes     yes      no
file_path           Str          yes     no       no   (display only)
file_name           Str          yes     yes      no   (searchable filename)
body                Text         yes     yes      no   (full-text — STORED + INDEXED with positions for SnippetGenerator, NOT fast)
file_type           Str          yes     yes      no   ("pdf" | "txt" | "md" | "log")
modified_ts         Date         yes     no       no   (for sorting)
content_hash        Str          yes     yes      no   (Stored + Indexed as Str — required for IndexWriter::delete_term)
tags                Str (multi)  yes     yes      no   (multi-valued for tag filtering)
```

Key decisions:
- `doc_id = content_hash + file_type` — stable even if file is renamed.
- `body` is `STORED` + `INDEXED` with `TEXT` options (includes term positions, required for `SnippetGenerator`). Stored text enables contextual snippets around matched terms. Index size impact: ~500MB of body text at 10K documents (50KB avg per PDF) — acceptable on modern SSDs. `body` is NOT `FAST` — `FAST` on text fields enables columnar access (useful for u64/f64/date, not text search) and adds ZSTD compression overhead with zero search benefit.
- `tags` are denormalized into Tantivy for fast filtering without a SQLite round-trip during search. Tag changes on the UI thread are synced to Tantivy via a dedicated UI→indexer channel (see Tag Synchronization below).
- `content_hash` is `Stored` + `Indexed` as `Str` (not `Text`). `IndexWriter::delete_term` requires an indexed field to locate documents — without INDEXED, `delete_term` silently becomes a no-op. A `Str` field creates a minimal inverted index (one 64-char hex string per document, <1MB overhead at 10K scale).

### SQLite Schema (tags and metadata)

```sql
PRAGMA journal_mode=WAL;
PRAGMA busy_timeout=5000;

CREATE TABLE documents (
    content_hash TEXT PRIMARY KEY,  -- blake3 hex
    file_path   TEXT NOT NULL,      -- current path
    file_type   TEXT NOT NULL,
    file_size   INTEGER NOT NULL,
    modified_ts INTEGER NOT NULL,   -- Unix timestamp
    indexed_at  TEXT NOT NULL,      -- ISO 8601
    last_error  TEXT                -- NULL = ok, otherwise last extraction error
);

CREATE TABLE tags (
    id   INTEGER PRIMARY KEY,
    name TEXT UNIQUE NOT NULL
);

CREATE TABLE document_tags (
    content_hash TEXT NOT NULL REFERENCES documents(content_hash) ON DELETE CASCADE,
    tag_id      INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (content_hash, tag_id)
);
```

**SQLite concurrency strategy:** Two separate `rusqlite::Connection` instances — one on the indexer thread (writes to `documents`), one on the UI thread (`TagStore` reads + tag CRUD). WAL mode allows concurrent readers and one writer without `SQLITE_BUSY`. `busy_timeout=5000` provides a safety net.

---

## Module Layout

```
src/
├── main.rs                  # Entry point: eframe::run_native
├── app.rs                   # egui App impl, top-level UI layout
├── search/
│   ├── mod.rs
│   ├── engine.rs            # Tantivy index open/write/search
│   ├── schema.rs            # Tantivy schema definition
│   └── query.rs             # Query builder + SearchRequest struct
├── indexer/
│   ├── mod.rs
│   ├── pipeline.rs          # Pipeline orchestrator (stage runner + reconciliation)
│   ├── extractors/
│   │   ├── mod.rs           # Extractor trait + ExtractedContent
│   │   ├── pdf.rs           # pdfium-based PDF text extraction
│   │   └── text.rs          # Plain text / .md / .log extraction
│   └── stages.rs            # Concrete stage chain assembly
├── watcher/
│   ├── mod.rs
│   └── watcher.rs           # notify-debouncer-full event loop
├── tags/
│   ├── mod.rs
│   ├── store.rs             # SQLite CRUD for tags
│   └── model.rs             # Tag, DocumentTag structs
├── preview/
│   ├── mod.rs
│   ├── pdf_render.rs        # Render thread: pdfium page → bitmap → channel
│   └── highlight.rs         # Search term highlight overlay
├── config.rs                # Watched folder path, app settings
└── error.rs                 # Error types (thiserror)
```

---

## Concurrency Model

### UI Thread (main)
- Runs `eframe::run_native` event loop.
- Tantivy `Searcher` runs directly on UI thread — lock-free MVCC reader, queries complete in microseconds.
- Displays results, manages texture handles from renderer thread.
- Sends `RenderRequest` to renderer thread via channel; receives `RenderResult` with bitmap bytes.

### Indexer Thread
- Receives file paths from watcher thread via `crossbeam::bounded(10_000)`.
- Runs pipeline: fast-path metadata check → extract text → re-hash to verify → commit to Tantivy → update SQLite.
- Commits to Tantivy every 10 documents or every 2 seconds, whichever comes first, so incremental results appear during batch indexing.
- Sends `IndexingProgress` events to UI thread via `crossbeam::unbounded`.
- On shutdown signal: commits any pending documents, closes IndexWriter, acknowledges.

### Renderer Thread
- Owns a dedicated `Pdfium` instance (no sharing needed — one `Pdfium` per thread).
- Receives `RenderRequest { path, page, search_terms }` from UI thread.
- Opens PDF, renders page to pre-allocated reusable bitmap buffer, finds text positions for highlights.
- Sends `RenderResult { rgba_bytes, width, height, highlights: Vec<Rect> }` back to UI thread.
- Caches open `PdfDocument` handles (LRU, max 5 documents) for fast page-flipping.
- pdfium is lazy-initialized: `Pdfium::new()` is called on first render request, not at app startup.

### Watcher Thread
- Uses `notify-debouncer-full` with 500ms timeout.
- Handles event deduplication, rename tracking, and backpressure automatically.
- For deletions: skips debounce, sends immediately.
- Sends `PathBuf` + metadata `(modified_ts, file_size)` to indexer thread.

### Graceful Shutdown

On app close:
1. UI thread receives close event via egui.
2. Sends shutdown signal to watcher, indexer, and renderer threads.
3. Watcher stops immediately.
4. Indexer commits any pending Tantivy documents, calls `IndexWriter::prepare_commit()`, drops `IndexWriter`.
5. Renderer drops cached `PdfDocument` handles, drops `Pdfium`.
6. UI thread waits for all threads to acknowledge (with 5-second timeout, then force-exits).
7. On next startup: `garbage_collect_files()` runs to clean stale segments from any prior unclean shutdown.

---

## Indexing Pipeline Design

```rust
/// Single-method extractor — returns Ok(None) for unsupported files.
trait Extractor: Send {
    /// Attempt text extraction. Returns Ok(None) if this extractor
    /// cannot handle this file type (not an error — try next extractor).
    /// Returns Err only on genuine extraction failures.
    fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>>;
}

struct ExtractedContent {
    text: String,
    title: Option<String>,
    page_count: Option<usize>, // PDF only
}

struct Pipeline {
    stages: Vec<Box<dyn Extractor>>,
    index_writer: IndexWriter,
    db: Connection,             // indexer-owned SQLite connection
}
```

### Pipeline Process Flow

```rust
impl Pipeline {
    fn process(&mut self, path: PathBuf, mtime: u64, size: u64) -> Result<()> {
        // Step 1: Fast-path — check metadata (timestamp + size) in SQLite.
        //          Avoids reading/hashing file if nothing changed. <1ms.
        if self.already_indexed_by_metadata(&path, mtime, size)? {
            return Ok(());
        }

        // Step 2: Compute content hash (reads file once).
        //          Hash is computed INCREMENTALLY during extraction
        //          (pipe file bytes through blake3 hasher while extracting)
        //          to avoid reading the file twice.
        let (extracted, content_hash) = self.extract_and_hash(&path)?;

        // Step 3: Check if content hash already exists in index (dedup/rename).
        if self.already_indexed_by_hash(&content_hash)? {
            // Same content, different path — update path in SQLite, skip re-index.
            self.update_path(&content_hash, &path)?;
            return Ok(());
        }

        // Step 4: Extract text via pipeline stages.
        let text = self.run_extractors(&path)?;

        // Step 5: Re-verify hash after extraction (catches TOCTOU modification).
        //          If hash changed, retry once. If still different, log warning and skip.
        let post_hash = blake3::hash(&std::fs::read(&path)?);
        if post_hash != content_hash {
            tracing::warn!("File modified during extraction: {}", path.display());
            // Retry once
            let retry_text = self.run_extractors(&path)?;
            let retry_hash = blake3::hash(&std::fs::read(&path)?);
            if retry_hash != post_hash {
                return Err(anyhow::anyhow!("File modified during extraction after retry"));
            }
            // Use retry results
        }

        // Step 6: Commit to Tantivy and SQLite.
        //          Write order: SQLite FIRST, then Tantivy.
        //          Rationale: an orphan in SQLite is harmless and can be cleaned up
        //          on the next index pass. An orphan in Tantivy with no SQLite row
        //          breaks tag referential integrity.
        self.update_sqlite(&path, &content_hash, &text)?;
        self.commit_to_tantivy(&path, &content_hash, &text)?;

        Ok(())
    }
}
```

**Commit cadence:** `commit_to_tantivy` calls `IndexWriter::commit()` every 10 documents or every 2 seconds (whichever comes first). Intermediate documents are buffered in `IndexWriter` memory (which is crash-safe for Tantivy's MVCC). This ensures incremental visibility during batch indexing.

**Deletion flow:** The watcher→indexer channel uses a typed message enum:

```rust
enum IndexerMessage {
    Upsert { path: PathBuf, mtime: u64, size: u64 },
    Delete { path: PathBuf },
}
```

On `Delete`: pipeline looks up `content_hash` from SQLite by path, calls `IndexWriter::delete_term(Term::from_field_text(content_hash, &content_hash))` using the indexed `content_hash` field (not `doc_id` — `doc_id = hash + file_type` and won't match with the hash alone), commits, and removes the SQLite row. Batch deletions can be committed together.

**Tag synchronization:** A dedicated channel carries tag updates from the UI thread to the indexer thread:

```rust
enum TagUpdateMessage {
    UpdateDocumentTags { content_hash: String, tags: Vec<String> },
}
```

The indexer thread processes these by: (1) look up current Tantivy document by `content_hash`, (2) call `IndexWriter::delete_term` to remove it, (3) re-insert with updated `tags` field, (4) commit. This ensures tag-filtered search results are always current.

**Error recovery:** Failed files are logged with their error to SQLite (`documents.last_error` column). On the next file event, if the `(path, mtime, size)` changed since the failed attempt, the file is retried. The UI shows "N files with indexing errors" with a viewable list.

### Startup Reconciliation

On app startup, before the watcher begins processing events:

1. Open Tantivy index and SQLite.
2. Run `IndexWriter::garbage_collect_files()` to clean stale segments.
3. Iterate all `doc_id`s in Tantivy. For each:
   - If missing from SQLite `documents`: backfill a SQLite row (metadata from Tantivy stored fields).
   - If present in SQLite but file no longer exists on disk: remove from both Tantivy and SQLite.
   - If file exists but `mtime` or `size` changed: mark for re-indexing (enqueue to pipeline).
4. Remove SQLite rows with no corresponding Tantivy document.

This ensures Tantivy and SQLite are consistent regardless of prior crashes.

---

## File Watcher Strategy

Use `notify-debouncer-full` (not manual debounce). It provides:

- Filesystem event deduplication by file ID (inode/handle), not just path.
- Rename tracking (emits both old and new paths).
- Event kind coalescing (Create+Write → single event).
- Backpressure handling for high-frequency event storms.

Configuration: 500ms debounce timeout. Delete events bypass debounce.

For scanner workflows: if the scanner writes slowly (>500ms gaps), the user should configure the scanner to write to a temp directory and atomically move completed files into the watched folder. Document this in user-facing help.

On each event, the watcher emits `(PathBuf, modified_ts, file_size)` tuples to enable the pipeline's metadata fast-path.

---

## PDF Rendering Strategy

PDF rendering runs on a **dedicated background thread** to avoid blocking the egui UI thread.

### Flow

```
UI click → send RenderRequest { path, page, search_terms } via channel
  → Render thread:
      1. Open PDF (or reuse cached PdfDocument handle, LRU max 5)
      2. Render page to pre-allocated reusable PdfBitmap buffer
         (PdfBitmap::new_from_bytes() with reused allocation — no per-frame allocation)
      3. Find text positions for search terms via pdfium text-find API
      4. Map character boxes to pixel coordinates for highlight overlays
      5. Send RenderResult { rgba_bytes, width, height, highlights } back
  → UI thread:
      1. Create egui::ColorImage from rgba_bytes
      2. Upload as TextureHandle (fast, GPU operation)
      3. Draw highlights as semi-transparent colored rectangles via egui::Painter
```

### Memory

- **Bitmap buffer:** One pre-allocated RGBA buffer on the renderer thread, reused across renders. Size: `preview_width × preview_height × 4` bytes, typical ~8–30MB depending on monitor resolution.
- **Texture cache:** Byte-budgeted LRU cache on the UI thread (default budget: 200MB). Eviction explicitly calls `ctx.tex_manager().write().forget(&texture_id)` to free GPU memory. At a typical 1440p monitor with the preview pane at ~1200px wide, this caches 3–5 pages.
- **Document handle cache:** LRU cache of open `PdfDocument` handles (max 5) on the renderer thread. Combined with texture cache, page-flipping within a recently-viewed document is instant.

### Highlight Bounding Boxes

pdfium's `FPDFText_Find*` API provides character bounding boxes. If pdfium-render's binding does not expose this API sufficiently, fall back to: extract text with position info via `lopdf` for the currently-viewed page only, then manual search + coordinate mapping.

### pdfium Lazy Initialization

`Pdfium::new()` is called on the renderer thread only when the first `RenderRequest` arrives — not at app startup. Cold startup (first launch after reboot) is dominated by pdfium init (400–800ms for DLL load + V8 JS engine + font tables). With lazy init, the UI becomes responsive in <200ms (egui window + Tantivy index open + SQLite). The user sees a brief "Loading PDF engine..." spinner on first preview click. Warm startup (pdfium.dll in filesystem cache): <500ms total.

---

## Search Query Pipeline

```
User types "invoice March"
    │
    ▼
┌─────────────────────────────┐
│ SearchRequest               │
│ { query, tag_filters,       │
│   fuzzy: false, limit: 50 } │
└────────────┬────────────────┘
             ▼
┌─────────────────────────────┐
│ Tantivy Searcher            │
│ - Lock-free read (UI thread)│
│ - BM25 scoring              │
│ - BooleanQuery AND terms    │
│ - TermQuery for tag filters │
│ - Returns TopDocs (max 50)  │
└────────────┬────────────────┘
             ▼
┌─────────────────────────────┐
│ SnippetGenerator            │
│ - Extract snippets with     │
│   highlighted terms         │
│   (requires TEXT positions) │
└────────────┬────────────────┘
             ▼
┌─────────────────────────────┐
│ Result Formatter            │
│ - Truncate to limit (50)    │
│ - Show "50 of 5,234" if     │
│   total hits exceed limit   │
└────────────┬────────────────┘
             ▼
       UI Results List
```

**Search runs on UI thread.** Tantivy's `Searcher` is lock-free (MVCC snapshot), and BM25 scoring over 10K docs with a 2–3 term AND query completes in <1ms. Snippet generation adds ~1ms. A channel round-trip would add latency for no benefit.

**Result limit:** Default 50 results. The UI shows "50 of N matches — refine your query" if total hits exceed the limit. This prevents frame drops from rendering 10K result rows and matches the old-Evernote UX.

`SearchRequest` struct is defined in U3 (not added in U9) to avoid re-signaturing the public API.

---

## Implementation Units

### U1. Project Scaffold and Crate Setup

- **Goal:** Initialize the Cargo project with all dependencies, directory structure, and basic egui window.
- **Requirements:** (infrastructure)
- **Dependencies:** None
- **Files:**
  - `Cargo.toml`
  - `src/main.rs` — includes panic hook + tracing init
  - `src/app.rs` — `PapervaultApp` struct with placeholder UI
  - `src/error.rs`
- **Approach:** Create a Cargo binary crate. Add all dependencies from the crate selection table. Set panic hook in `main.rs`: log panics via `tracing::error!` and write to `crash.log`. `main.rs` launches `eframe::run_native`. Placeholder window shows "No folder configured" message.
- **Patterns to follow:** Standard egui `eframe` template.
- **Test scenarios:**
  - App compiles and launches, displays a window with title "Papervault".
  - `cargo build --release` succeeds on Windows.
- **Test expectation: none — scaffolding, verified by `cargo build`.**

### U2. Configuration and Watched Folder Setup

- **Goal:** Allow the user to configure a watched folder path, persisted between sessions.
- **Requirements:** R6
- **Dependencies:** U1
- **Files:**
  - `src/config.rs`
  - `src/app.rs` (modify: folder selection UI)
- **Approach:** Store config as JSON in `dirs_next::config_dir()/papervault/config.json`. First launch: "Select folder" button via `rfd::FileDialog`. Config struct: `{ watched_folder: PathBuf }`. Load on startup, save on change.
- **Patterns to follow:** `dirs-next` for platform config paths, `serde_json` for persistence.
- **Test scenarios:**
  - First launch shows folder selection UI.
  - After selecting a folder, the path is displayed and persists across restarts.
  - Changing the watched folder updates the config file.
  - Invalid folder path shows an error message, no crash.
- **Verification:** Select a folder through the UI, restart app, confirm folder path is remembered.

### U3. Tantivy Index: Schema, Open, and Search

- **Goal:** Define the Tantivy schema (body STORED+INDEXED for SnippetGenerator, content_hash INDEXED for delete_term), create/open the index, and implement full-text search with `SearchRequest`.
- **Requirements:** R1, R2
- **Dependencies:** U1
- **Files:**
  - `src/search/schema.rs`
  - `src/search/engine.rs` — `SearchEngine::search(SearchRequest) -> SearchResults`
  - `src/search/query.rs` — `SearchRequest { query, tag_filters, fuzzy, limit }`
  - `src/search/mod.rs`
- **Approach:** `SearchEngine` manages `Index` lifecycle. Index directory: `dirs_next::data_local_dir()/papervault/index/`. On startup: `IndexWriter::garbage_collect_files()`. `search()` parses query into BooleanQuery, executes on UI thread, returns results with snippets via `SnippetGenerator`. Result limit: default 50.
- **Test scenarios:**
  - `open_or_create` creates new index if none exists, opens existing one.
  - Index a document, search for a term in that document → found.
  - Search for absent term → empty results.
  - Results include filename, highlight snippet, match count.
  - Multiple terms AND-combined.
  - Search with `limit: 10` returns max 10 results.
  - Total hits > limit returns overflow count.
  - Search completes in <1ms with 10K docs indexed.
- **Verification:** Integration test: index 5 documents, search, verify results.

### U4. File Watcher with notify-debouncer-full

- **Goal:** Watch the configured folder using `notify-debouncer-full`.
- **Requirements:** R6, R8, R12
- **Dependencies:** U2
- **Files:**
  - `src/watcher/watcher.rs`
  - `src/watcher/mod.rs`
- **Approach:** Use `notify-debouncer-full` with 500ms timeout. No manual debounce hashmap. Filter to supported extensions. Emit `(PathBuf, modified_ts, file_size)` tuples. Delete events bypass debounce. Send via `crossbeam::bounded(10_000)` to indexer. On initial startup, emit events for all existing files in the folder (initial scan).
- **Test scenarios:**
  - Creating a .pdf sends a single event after 500ms debounce.
  - Rapid writes to same file produce a single event.
  - Deleting a file sends immediate delete event.
  - Unsupported file (.exe, .jpg) produces no event.
  - Initial scan emits events for existing files.
- **Verification:** Unit test with temp directory.

### U5. PDF Text Extraction via pdfium

- **Goal:** Extract searchable text from PDFs using pdfium-render.
- **Requirements:** R7, R11
- **Dependencies:** U1
- **Files:**
  - `src/indexer/extractors/mod.rs` — `Extractor` trait (single-method: `extract → Ok(None)` for unsupported)
  - `src/indexer/extractors/pdf.rs` — `PdfExtractor` holds its own `Pdfium` instance (no Sync needed, trait is Send only)
- **Approach:** `PdfExtractor` creates one `Pdfium::new()` at construction (reused for all extractions). `extract()` opens PDF, iterates pages, calls `page.text().all()`. Returns `Ok(None)` for non-PDF files. Returns `Err` for corrupt/locked PDFs. Handles empty PDFs (page_count=0) gracefully.
- **Test scenarios:**
  - Extract text from searchable PDF → known text present.
  - PDF with no text layer → returns `Ok(Some(ExtractedContent { text: "" }))`.
  - Corrupt PDF → `Err`.
  - Password-protected PDF → `Err`.
  - Empty PDF (0 pages) → `Ok(Some(ExtractedContent { text: "", page_count: Some(0) }))`.
  - Multi-page PDF → text from all pages.
  - PDF with mixed text+images → text extracted, images ignored.
- **Verification:** Unit tests with fixture PDFs.

### U6. Text File Extraction

- **Goal:** Extract text from .txt, .md, and .log files.
- **Requirements:** R7, R11
- **Dependencies:** U1
- **Files:**
  - `src/indexer/extractors/text.rs` — `TextExtractor`
- **Approach:** Read file as UTF-8. Non-UTF-8: attempt lossy decode (`String::from_utf8_lossy`), log warning. Markdown: keep raw text (markdown syntax indexed — acceptable for v1; strip syntax in deferred improvement). Files >100MB: read first 10MB only. Returns `Ok(None)` for non-text extensions.
- **Test scenarios:**
  - Extract .txt → content matches.
  - Extract .md → raw markdown extracted.
  - Non-UTF-8 file → lossy decode, log warning.
  - Empty file → empty string.
  - >100MB file → first 10MB extracted.
- **Verification:** Unit tests with fixture text files.

### U7. Indexing Pipeline Orchestrator

- **Goal:** Wire watcher, extractors, and Tantivy indexer with atomicity, error recovery, and reconciliation.
- **Requirements:** R7, R8, R12
- **Dependencies:** U3, U4, U5, U6
- **Files:**
  - `src/indexer/pipeline.rs` — full `Pipeline` with process, reconcile, shutdown
  - `src/indexer/stages.rs`
  - `src/indexer/mod.rs`
- **Approach:** Implements the complete `Pipeline::process()` flow from the design section above, including: metadata fast-path → extract-and-hash (single pass) → dedup check → extract text → post-extract hash verification → SQLite write first, Tantivy write second → commit every 10 docs or 2s. Includes `reconcile()` for startup consistency check. Includes `shutdown()` for graceful IndexWriter commit. Failed files recorded in `documents.last_error`.
- **Test scenarios:**
  - New file through pipeline → searchable.
  - Modified file (same path, different content) → re-indexed.
  - Renamed file (same content) → metadata fast-path skips re-index.
  - Deleted file → removed from both Tantivy and SQLite.
  - 200 files in quick succession → all indexed, no data races, incremental commits visible.
  - File modified during extraction → TOCTOU detection fires, retry once.
  - Corrupt PDF → error logged to `last_error`, pipeline continues with next file.
  - Crash recovery: kill process mid-index, restart, `reconcile()` cleans up orphans.
  - Shutdown signal: pending documents committed before exit.
- **Verification:** Integration tests with temp dirs and controlled crash scenarios.

### U8. SQLite Tag Storage

- **Goal:** Tag storage with WAL mode, two connections.
- **Requirements:** R9, R10
- **Dependencies:** U7 (documents table must exist)
- **Files:**
  - `src/tags/store.rs` — `TagStore` with UI-thread connection
  - `src/tags/model.rs`
  - `src/tags/mod.rs`
- **Approach:** `TagStore` opens its own `rusqlite::Connection` (separate from indexer thread's connection). WAL mode enabled on first open. Methods: `create_tag`, `list_tags`, `assign_tag`, `remove_tag`, `get_tags_for_document`. Indexer thread's connection handles `documents` table writes. `ON DELETE CASCADE` ensures tag assignments are cleaned up when documents or tags are deleted.
- **Test scenarios:**
  - Create tag → appears in list.
  - Assign tag to document → retrieved.
  - Multiple tags → all retrieved.
  - Remove assignment → gone.
  - Delete tag → cascades to `document_tags`.
  - Duplicate tag name → error.
  - Concurrent read (UI) during indexer write → no SQLITE_BUSY (WAL mode).
- **Verification:** Unit tests with in-memory SQLite.

### U9. Tag Filtering in Search

- **Goal:** Search results filterable by tags.
- **Requirements:** R10
- **Dependencies:** U3, U8
- **Files:**
  - `src/search/query.rs` (modify: use `SearchRequest.tag_filters`)
  - `src/search/engine.rs` (modify)
- **Approach:** `SearchRequest.tag_filters` adds `TermQuery` clauses on the Tantivy `tags` field. Tags are synced to Tantivy at index time and on tag changes. Tag lookup for display uses UI-thread SQLite connection.
- **Test scenarios:**
  - Search + tag filter → only tagged documents.
  - Multiple tag filters → AND semantics.
  - No tag filter → all documents.
  - Tag change reflected in next search.
- **Verification:** Integration test.

### U10. PDF Preview via Render Thread

- **Goal:** Render PDF pages on a dedicated background thread, display as egui textures with highlights.
- **Requirements:** R3, R4, R5
- **Dependencies:** U1, U3
- **Files:**
  - `src/preview/pdf_render.rs` — `PdfRenderer` actor with render loop
  - `src/preview/highlight.rs` — overlay rect calculation
  - `src/preview/mod.rs`
  - `src/app.rs` (modify)
- **Approach:** `PdfRenderer` runs on dedicated thread. Receives `RenderRequest` via channel, renders to reusable `PdfBitmap` buffer (`PdfBitmap::new_from_bytes()`), finds text positions, sends `RenderResult` back. UI thread uploads bitmap as `TextureHandle`. Byte-budgeted LRU texture cache (default 200MB). `PdfDocument` handle cache (LRU max 5). On eviction: `ctx.tex_manager().write().forget(&texture_id)`. pdfium lazy-init: first render request triggers `Pdfium::new()` with "Loading PDF engine..." spinner on UI.
- **Test scenarios:**
  - Click result → preview shows first match page.
  - Search terms highlighted on rendered page.
  - Next/Previous page navigation.
  - Boundary: first/last page handled.
  - Switching results updates preview.
  - Large page scaled to fit preview pane.
  - Texture eviction frees GPU memory.
  - Cold start: "Loading PDF engine..." shown, then preview renders.
- **Verification:** Manual visual verification. Automated: `PdfRenderer` produces correct RGBA bytes from fixture PDFs.

### U11. Text File Preview

- **Goal:** Display text content in preview pane with highlights.
- **Requirements:** R3, R4, R11
- **Dependencies:** U1
- **Files:**
  - `src/preview/mod.rs` (modify)
  - `src/app.rs` (modify)
- **Approach:** Read file, display in read-only `TextEdit` or `ScrollArea`. Search term highlights via `egui::RichText` with `background_color`.
- **Test scenarios:**
  - .txt, .md, .log show content with highlights.
  - Large file → scrollable.
- **Verification:** Integration test with fixture files.

### U12. UI Assembly — Layout, Search Bar, Results List

- **Goal:** Full three-panel UI with all subsystems integrated.
- **Requirements:** R1, R2, R3, R4, R5
- **Dependencies:** U2, U3, U4, U7, U10, U11
- **Files:**
  - `src/app.rs` (major rewrite)
- **Approach:** `TopBottomPanel` (search bar), `SidePanel` (results list), `CentralPanel` (preview). Search-as-you-type via `TextEdit::singleline`. Results: selectable rows with filename + snippet. Shows "N of M matches" overflow indicator. Indexing progress indicator from `IndexingProgress` channel events. "N files with errors" badge linking to error list.
- **Test scenarios:**
  - Type → results update in real-time.
  - Click result → preview updates.
  - Window resize preserves panel layout.
  - Empty state: no folder → prompt.
  - Empty state: no results → "No documents found."
- **Verification:** Visual + egui test harness for layout assertions.

### U13. Tag UI

- **Goal:** Tag panel for creating/assigning tags.
- **Requirements:** R9
- **Dependencies:** U8, U12
- **Files:**
  - `src/app.rs` (modify)
- **Approach:** Collapsible tag sidebar. Tag list with filter checkboxes. "Add tag" button with existing-tag dropdown + "Create new" option. Selected document's tags shown below preview. Changes call `TagStore` and update Tantivy tags field.
- **Test scenarios:**
  - Create tag → appears in list.
  - Assign tag → persists across restart.
  - Remove tag → immediate update.
  - Tag filter checkboxes filter search results.
- **Verification:** Manual + SQLite state verification.

---

## Test Strategy

| Layer | Tool | Scope |
|-------|------|-------|
| Unit tests | `cargo test` | Extractor correctness, SQLite ops, query building, pipeline stages |
| Integration tests | `cargo test` with temp dirs | Pipeline end-to-end, search correctness, watcher events, crash recovery |
| Stress/bench | `cargo bench` or dedicated test | 10K document indexing, search latency during indexing, memory at steady state |
| Visual tests | Manual | PDF rendering quality, highlight positioning, UI layout |

### Test Fixtures (`tests/fixtures/`)

| Fixture | Purpose |
|---------|---------|
| `sample_searchable.pdf` | Single-page PDF with known text |
| `sample_multipage.pdf` | Multi-page PDF (3+ pages) |
| `sample_corrupt.pdf` | Truncated/broken PDF |
| `sample_password.pdf` | Password-protected PDF |
| `sample_empty.pdf` | Valid PDF with 0 pages |
| `sample_mixed.pdf` | PDF with text + embedded images |
| `sample.txt` | UTF-8 text file |
| `sample_non_utf8.txt` | Non-UTF-8 encoded text |
| `sample.md` | Markdown with formatting |
| `sample_large.txt` | >100MB text file (or generated in test) |
| `sample.log` | Log-style text |
| `crashed_index/` | Tantivy index directory from a killed process (for recovery tests) |

---

## System-Wide Impact

- **Disk:** Tantivy index: 200–700MB (body text stored for snippets). SQLite: 1–5MB. pdfium.dll: ~5MB.
- **Memory:** UI thread: 20–40MB (egui + Tantivy reader). Indexer thread: 30–50MB (pdfium + extraction buffers). Renderer thread: 30–50MB (pdfium + bitmap buffer + document handle cache). Texture cache: up to 200MB (byte-budgeted). Total steady state: ~100–150MB; peak with full texture cache: ~300MB.
- **CPU:** Idle: near-zero. Indexing: 1–2 cores. Search: negligible. PDF rendering: 1 core briefly.
- **Startup (cold):** egui window + Tantivy index open + SQLite < 200ms. pdfium lazy-init on first preview: +400–800ms.
- **Startup (warm):** < 500ms total (pdfium.dll cached by OS).

---

## Risk Analysis

| Risk | Severity | Likelihood | Mitigation |
|------|----------|------------|------------|
| pdfium text-find API insufficient for highlight boxes | High | Medium | Fallback: `lopdf` for current page text positions |
| pdfium.dll not found on some Windows installs | High | Low | Bundle pdfium.dll with release binary |
| Tantivy index corruption on unexpected shutdown | Medium | Low | `prepare_commit()` on shutdown, `garbage_collect_files()` on startup |
| Tantivy+SQLite inconsistency after crash | Medium | Medium | Startup reconciliation step, write SQLite first |
| UI freeze during indexing | Low | Low | All extraction and commit on background threads |
| Texture cache memory pressure | Medium | Low | Byte-budgeted LRU with explicit GPU memory release |
| pdfium cold-start latency | Low | Medium | Lazy-init with loading spinner |
| TOCTOU modification during indexing | Low | Medium | Post-extraction hash verification + retry |
| Scanner partial-file indexing | Medium | Medium | Document temp-dir-then-move workflow; write-lock check as secondary defense |

---

## Scope Boundaries

### Deferred for later
- OCR for scanned/image-based PDFs (v2 — add `OcrExtractor` to pipeline stages)
- AI auto-tagging (v2 — add `AiTagExtractor` to pipeline stages)
- Multi-folder watching
- Dark mode
- Keyboard shortcuts
- Markdown syntax stripping for cleaner snippets
- Parallel extraction for batch indexing

### Deferred to Follow-Up Work
- Installer/packaging (MSIX or NSIS)
- Auto-start with Windows
- System tray integration

### Outside this product's identity
- PDF editing, merging, annotation
- Cloud sync or sharing
- Non-Windows platforms
- Mobile/tablet

---

## Key Technical Decisions

- **SQLite write before Tantivy.** SQLite is written first, then Tantivy. If crash occurs between them, an orphaned SQLite row is harmless (cleaned up on next reconciliation). An orphaned Tantivy document would break tag referential integrity.
- **Single-method Extractor trait.** `extract() -> Result<Option<ExtractedContent>>` — `Ok(None)` means "unsupported file type, try next extractor." Statically prevents the two-step `can_handle`/`extract` anti-pattern.
- **Dedicated PDF render thread, not UI thread.** pdfium page rendering takes 50–500ms. Running on UI thread would freeze the egui render loop. Background thread → channel → UI thread texture upload is fast.
- **`notify-debouncer-full` over manual debounce.** Handles inode-based dedup, rename tracking, and backpressure. Avoids ~100 lines of subtle edge-case code.
- **pdfium lazy-init.** Defers 400–800ms cold-start cost until first preview click. UI responsive in <200ms.
- **Content hash as stable document identity.** blake3 hash instead of file path means renames/moves don't cause re-indexing or broken tag associations.
- **Tags denormalized into Tantivy.** Tag filters applied as Tantivy query clauses for fast unified search.
- **Byte-budgeted texture cache.** Replaces fixed page-count LRU to handle varying monitor resolutions.
- **`SearchRequest` struct from U3.** Avoids re-signaturing `SearchEngine::search` when tag filters are added in U9.

---

## Dependencies / Assumptions

- pdfium-render exposes sufficient text-find API for highlight bounding boxes. If not, `lopdf` fallback exists.
- `notify-debouncer-full`'s Windows backend (ReadDirectoryChangesW) is reliable on the user's system.
- Scanner produces PDFs with extractable text layers — OCR-only PDFs require v2.
- Two `rusqlite::Connection` instances in WAL mode work correctly (standard SQLite pattern, well-tested).
- `dirs-next` and `notify-debouncer-full` are available on crates.io and compatible with the target Rust toolchain.
- Two `Pdfium::new()` calls in the same process (indexer + renderer threads) work correctly. If not, use `Arc<Pdfium>` shared via a dedicated manager thread.

---

## Outstanding Questions

### Resolve Before Planning

_None._

### Deferred to Implementation

- [U10] Exact pdfium-render API surface for `FPDFText_Find*` bounding boxes — verify during U10.
- [U7] Tantivy `delete_term` performance at 10K scale — benchmark during U7.
- [U12] Optimal egui panel sizing for different window sizes.
- [U2] `rfd::FileDialog` compatibility with user's Windows 11 configuration.
- [U5] Whether two `Pdfium::new()` calls in same process share internal state correctly — test during U5.
