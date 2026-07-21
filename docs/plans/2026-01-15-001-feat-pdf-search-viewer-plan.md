---
date: 2026-01-15
status: active
origin: docs/brainstorms/2026-01-15-pdf-search-viewer-requirements.md
deepened:
---

# feat: PDF Search & Viewer — Design Document

## Summary

Design for a Windows 11 desktop application (Rust + egui) that watches a single folder, indexes PDFs and text files via Tantivy for instant full-text search, and provides a single-window search-with-preview experience. pdfium handles PDF text extraction and page rendering. The indexing pipeline uses a pluggable stage architecture to accept OCR and AI auto-tagging in v2.

**Target repo:** papervault (greenfield)

---

## Problem Frame

See origin document `docs/brainstorms/2026-01-15-pdf-search-viewer-requirements.md`. The user manages 1,000–10,000 searchable PDFs and text files. Windows Search is too slow at this scale and does not search inside PDF content effectively. The goal is an old-Evernote-style single-window search viewer: type → results appear with highlights → click to preview.

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│                    egui Application                      │
│  ┌──────────┐  ┌──────────────┐  ┌───────────────────┐  │
│  │ Search   │  │ Results List │  │ Preview Pane      │  │
│  │ Bar      │  │ (snippets)   │  │ (pdfium page      │  │
│  │          │  │              │  │  + highlight      │  │
│  │          │  │              │  │  overlay)         │  │
│  └──────────┘  └──────────────┘  └───────────────────┘  │
│                     │                                    │
│              ┌──────┴──────┐                             │
│              │  App State  │                             │
│              │  (channels) │                             │
│              └──────┬──────┘                             │
└─────────────────────┼────────────────────────────────────┘
                      │
        ┌─────────────┼─────────────┐
        ▼             ▼             ▼
┌───────────┐  ┌────────────┐  ┌────────────┐
│  Search   │  │  Indexer   │  │  Watcher   │
│  Engine   │  │  Pipeline  │  │  (notify)  │
│ (tantivy) │  │            │  │            │
└───────────┘  └─────┬──────┘  └─────┬──────┘
                     │               │
              ┌──────┴──────┐        │
              │  Extractors │        │
              │  ┌────────┐ │        │
              │  │ PDF    │ │        │
              │  │(pdfium)│ │        │
              │  ├────────┤ │        │
              │  │ Text   │ │        │
              │  │(direct)│ │        │
              │  ├────────┤ │        │
              │  │ OCR v2 │ │        │
              │  ├────────┤ │        │
              │  │ AI v2  │ │        │
              │  └────────┘ │        │
              └──────┬──────┘        │
                     │               │
              ┌──────┴──────┐        │
              │   SQLite    │        │
              │ (tags, meta)│        │
              └─────────────┘        │
                     │               │
            ┌────────┴────────┐      │
            │  File System    │◄─────┘
            │  (watched dir)  │
            └─────────────────┘
```

**Thread model:**

| Thread | Role |
|--------|------|
| **UI thread** | egui render loop, user input, search query dispatch |
| **Indexer thread** | Runs the extraction pipeline, commits to Tantivy, writes SQLite metadata |
| **Watcher thread** | notify event loop, enqueues files for indexing |
| **Searcher** | Tantivy reader (lock-free), queried from UI thread — no dedicated thread needed |

**Communication:** Crossbeam channels between UI thread and background threads. The UI sends `Search(query)` messages; the indexer sends `IndexingProgress(file, done/total)` updates if batch operations are active.

---

## Crate Selection

| Component | Crate | Version (approx) | Rationale |
|-----------|-------|-------------------|-----------|
| GUI | `egui` + `eframe` | 0.30+ | Pure Rust, fast, good for data-dense UIs. Default wgpu backend. |
| Search index | `tantivy` | 0.22+ | Rust-native full-text search, lock-free readers, incremental commits. |
| PDF text/rendering | `pdfium-render` | 0.8+ | Rust bindings to Chromium's pdfium. Single engine for both extraction and rendering. |
| File watching | `notify` | 7.0+ | Cross-platform filesystem events. Uses ReadDirectoryChangesW on Windows. |
| Metadata/tags | `rusqlite` | 0.32+ | SQLite for tag storage. Lightweight, embeddable, zero-config. |
| Channels | `crossbeam` | 0.8+ | MPMC channels for UI ↔ background thread communication. |
| Content hashing | `blake3` | 1.5+ | Fast content hash for detecting file changes and stable document identity. |
| Logging | `tracing` + `tracing-subscriber` | 0.1+/0.3+ | Structured logging. |
| Error handling | `anyhow` + `thiserror` | latest | anyhow for application code, thiserror for library-level error types. |
| Config | `dirs` | 5.0+ | Platform-appropriate config/data directories. |

---

## Data Model

### Tantivy Schema

```text
Field               Type        Stored  Indexed  Fast
───                 ────        ──────  ───────  ────
doc_id              Str          yes     yes      no
file_path           Str          yes     no       no   (display only)
file_name           Str          yes     yes      no   (searchable filename)
body                Text         no      yes      yes  (full-text content)
file_type           Str          yes     yes      no   ("pdf" | "txt" | "md" | "log")
modified_ts         Date         yes     no       no   (for sorting)
content_hash        Str          yes     yes      no   (stable ID)
tags                Str (multi)  yes     yes      no   (multi-valued for tag filtering)
```

Key decisions:
- `doc_id = content_hash + file_type` — stable even if file is renamed.
- `body` is not stored (re-extract from file for preview). This keeps the index small.
- `tags` are denormalized into Tantivy for fast filtering without a SQLite join during search.
- `fast` field (`body`) enables sub-200ms search at 10K scale.

### SQLite Schema (tags and metadata)

```sql
CREATE TABLE documents (
    content_hash TEXT PRIMARY KEY,  -- blake3 hex
    file_path   TEXT NOT NULL,      -- current path
    file_type   TEXT NOT NULL,
    file_size   INTEGER,
    indexed_at  TEXT NOT NULL       -- ISO 8601
);

CREATE TABLE tags (
    id   INTEGER PRIMARY KEY,
    name TEXT UNIQUE NOT NULL
);

CREATE TABLE document_tags (
    content_hash TEXT NOT NULL REFERENCES documents(content_hash),
    tag_id      INTEGER NOT NULL REFERENCES tags(id),
    PRIMARY KEY (content_hash, tag_id)
);
```

Future AI integration point: `document_tags` is a simple junction table. An AI model can INSERT/UPDATE/DELETE from `document_tags` with no structural changes.

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
│   └── query.rs             # Query builder (phrase, fuzzy, tag filters)
├── indexer/
│   ├── mod.rs
│   ├── pipeline.rs          # Pipeline orchestrator (stage runner)
│   ├── extractors/
│   │   ├── mod.rs           # Extractor trait definition
│   │   ├── pdf.rs           # pdfium-based PDF text extraction
│   │   └── text.rs          # Plain text / .md / .log extraction
│   └── stages.rs            # Concrete stage chain assembly
├── watcher/
│   ├── mod.rs
│   └── watcher.rs           # notify event loop, file discovery
├── tags/
│   ├── mod.rs
│   ├── store.rs             # SQLite CRUD for tags
│   └── model.rs             # Tag, DocumentTag structs
├── preview/
│   ├── mod.rs
│   ├── pdf_render.rs        # pdfium page → egui texture
│   └── highlight.rs         # Search term highlight overlay
├── config.rs                # Watched folder path, app settings
└── error.rs                 # Error types (thiserror)
```

---

## Concurrency Model

### UI Thread (main)
- Runs `eframe::run_native` event loop.
- On each frame: checks channel for new search results, applies to UI state.
- Dispatches search queries via channel to search engine (Tantivy reader is cheap, could run on UI thread, but channel decoupling is cleaner).

**Decision: Run Tantivy search directly on UI thread.** Tantivy's `Searcher` is lock-free and a query against 10K docs completes in microseconds. A channel would add latency for no benefit. Background indexing writes to a separate `IndexWriter` — Tantivy's MVCC ensures readers see a consistent snapshot.

### Indexer Thread (spawned once, runs for app lifetime)
- Receives file paths from watcher thread via `crossbeam::unbounded`.
- Runs pipeline: extract text → commit to Tantivy → update SQLite.
- Sends progress events to UI thread via `crossbeam::unbounded`.
- Shuts down on drop of sender.

### Watcher Thread (spawned once, runs for app lifetime)
- Runs `notify::Watcher` event loop.
- Deduplicates rapid-fire events (debounce 500ms per path).
- Sends `PathBuf` to indexer thread.

### Thread Safety
- Tantivy: `Index` is `Send + Sync`. `IndexWriter` lives on indexer thread. `IndexReader` created on UI thread.
- SQLite: `rusqlite::Connection` is `Send` but not `Sync`. Each thread gets its own connection, or wrap in `Mutex<Connection>` for the indexer thread.
- pdfium: `Pdfium` is `Send` but not `Sync`. One instance on indexer thread for extraction, one on UI thread for rendering (or share via `Arc<Mutex<Pdfium>>` — decision deferred to implementation).

---

## Indexing Pipeline Design

```rust
/// Each extractor takes a file path and returns extracted text + metadata.
trait Extractor: Send + Sync {
    /// Returns None if this extractor cannot handle this file type.
    fn can_handle(&self, path: &Path) -> bool;
    /// Extracts text content. Returns error on extraction failure (not on unsupported files).
    fn extract(&self, path: &Path) -> Result<ExtractedContent>;
}

struct ExtractedContent {
    text: String,
    title: Option<String>,
    page_count: Option<usize>, // PDF only
}

struct Pipeline {
    stages: Vec<Box<dyn Extractor>>,
    index_writer: IndexWriter,
    db: Connection,
}

impl Pipeline {
    fn process(&mut self, path: PathBuf) -> Result<()> {
        let content_hash = blake3_hash(&path)?;
        // Check if already indexed with same hash → skip
        if self.already_indexed(&content_hash, &path)? {
            return Ok(());
        }
        let extracted = self.run_extractors(&path)?;
        self.commit_to_tantivy(&path, &content_hash, &extracted)?;
        self.update_sqlite(&path, &content_hash, &extracted)?;
        Ok(())
    }
}
```

**v2 extension points:**
- Add `OcrExtractor` that runs on image-based PDFs (runs before or after text extraction).
- Add `AiTagExtractor` that calls a local model and writes to `document_tags`.
- Both are new `impl Extractor` structs added to `stages` vector — no pipeline code changes.

---

## PDF Rendering Strategy

pdfium renders a page to a bitmap (`FPDFBitmap`). The flow for preview:

1. User clicks a search result.
2. UI thread uses `pdfium-render` to open the PDF and render the page containing the first match.
3. Bitmap is converted to `egui::ColorImage` → `egui::TextureHandle`.
4. Texture is displayed in an `egui::Image` widget inside a scroll area.
5. Search term highlights: pdfium provides `FPDFText_FindStart` / `FPDFText_GetCharBox` which returns character bounding boxes. Map those boxes to pixel coordinates on the rendered bitmap, then draw colored rectangles via egui painter on top of the image.

**Memory:** One rendered page at a time. Textures are cached per page (LRU, max 10 pages) for fast page flipping.

**Risk:** pdfium-render's API surface may not expose all the text-find primitives needed for highlight bounding boxes. Mitigation: if pdfium-render is insufficient, fall back to re-extracting text with position info via `lopdf` for the currently-viewed page only, then manual text search + coordinate mapping.

---

## File Watcher Debounce Strategy

Windows `notify` can fire multiple events for a single file operation (e.g., Create → Write → Write). Strategy:

1. Watcher thread receives raw events.
2. Per-path debounce: on first event for path P, start a 500ms timer.
3. If another event arrives for P within 500ms, reset the timer.
4. When timer expires, enqueue P to the indexer thread.
5. Special case: `Remove` events skip the timer and are processed immediately (with index deletion).

This prevents indexing the same file 3 times when a scanner writes it.

---

## Search Query Pipeline

```
User types "invoice March"
    │
    ▼
┌─────────────────────────────┐
│ Query Parser                │
│ - Split terms               │
│ - Build Tantivy query:      │
│   BooleanQuery {            │
│     MUST: body:"invoice"    │
│     MUST: body:"March"      │
│     (optional: fuzzy if     │
│      no results)            │
│   }                         │
│ - Apply tag filters:        │
│   TermQuery on tags field   │
└────────────┬────────────────┘
             ▼
┌─────────────────────────────┐
│ Tantivy Searcher            │
│ - Lock-free read            │
│ - BM25 scoring              │
│ - Returns TopDocs           │
└────────────┬────────────────┘
             ▼
┌─────────────────────────────┐
│ Result Formatter            │
│ - Extract snippets with     │
│   highlighted terms         │
│ - Format: filename,         │
│   snippet, match count,     │
│   file type icon            │
└────────────┬────────────────┘
             ▼
       UI Results List
```

---

## Implementation Units

### U1. Project Scaffold and Crate Setup

- **Goal:** Initialize the Cargo project with all dependencies, directory structure, and basic egui window.
- **Requirements:** (infrastructure)
- **Dependencies:** None
- **Files:**
  - `Cargo.toml`
  - `src/main.rs`
  - `src/app.rs`
  - `src/error.rs`
- **Approach:** Create a Cargo binary crate. Add all dependencies from the crate selection table. `main.rs` launches an `eframe::run_native` with a basic `PapervaultApp` struct. The window shows a placeholder search bar and "No folder configured" message.
- **Patterns to follow:** Standard egui `eframe` template from egui docs.
- **Test scenarios:**
  - App compiles and launches, displays an empty window with title "Papervault".
  - `cargo build --release` succeeds on Windows.
- **Test expectation: none — scaffolding, verified by `cargo build`.**

### U2. Configuration and Watched Folder Setup

- **Goal:** Allow the user to configure a watched folder path, persisted between sessions.
- **Requirements:** R6
- **Dependencies:** U1
- **Files:**
  - `src/config.rs`
  - `src/app.rs` (modify: folder selection UI)
- **Approach:** Store config as JSON in `dirs::config_dir()/papervault/config.json`. On first launch, show a "Select folder" button that opens `rfd::FileDialog` (native folder picker). Validate the path exists and is readable. Config struct: `{ watched_folder: PathBuf }`. Load on startup, save on change.
- **Patterns to follow:** `dirs` crate for platform config paths, `serde_json` for persistence.
- **Test scenarios:**
  - First launch shows folder selection UI.
  - After selecting a folder, the path is displayed and persists across restarts.
  - Changing the watched folder updates the config file.
  - Invalid folder path shows an error message, does not crash.
- **Verification:** Select a folder through the UI, restart the app, confirm the folder path is remembered.

### U3. Tantivy Index: Schema, Open, and Search

- **Goal:** Define the Tantivy schema, create/open the index on disk, and implement basic full-text search.
- **Requirements:** R1, R2
- **Dependencies:** U1
- **Files:**
  - `src/search/schema.rs`
  - `src/search/engine.rs`
  - `src/search/query.rs`
  - `src/search/mod.rs`
- **Approach:** Define the Tantivy schema as specified in the data model section. `SearchEngine` struct manages `Index` lifecycle (open or create). `search(&self, query: &str) -> Vec<SearchResult>` parses the query string into a Tantivy `Query`, executes it, and formats results with snippets. Use Tantivy's `SnippetGenerator` for highlighted snippets. Index directory: `dirs::data_dir()/papervault/index/`.
- **Patterns to follow:** Tantivy's official examples for schema definition and query building.
- **Test scenarios:**
  - `SearchEngine::open_or_create` creates a new index if none exists, opens existing one.
  - Indexing a document with known text, then searching for a term in that text returns the document.
  - Search for a term not in any document returns empty results.
  - Search results include filename, snippet with highlighted term, and match count.
  - Multiple terms are AND-combined (searching "invoice March" matches only documents with both).
  - Search completes in under 50ms with 10,000 documents in the index.
- **Verification:** Integration test: index 5 documents with known content, search for specific terms, verify correct results with snippets.

### U4. File Watcher with Debounce

- **Goal:** Watch the configured folder for new, modified, and deleted files, with debounced event delivery.
- **Requirements:** R6, R8, R12
- **Dependencies:** U2
- **Files:**
  - `src/watcher/watcher.rs`
  - `src/watcher/mod.rs`
- **Approach:** Use `notify::recommended_watcher`. On `EventKind::Create` or `EventKind::Modify`, debounce per-path with a 500ms timer (use `std::time::Instant` tracking in a `HashMap<PathBuf, Instant>`). On timer expiry, send the path through a `crossbeam::Sender<PathBuf>` to the indexer. On `EventKind::Remove`, skip debounce and send immediately with a `FileEvent::Deleted` variant. Filter events to only supported extensions (pdf, txt, md, log).
- **Patterns to follow:** `notify` crate examples.
- **Test scenarios:**
  - Creating a .pdf file in the watched folder sends a `FileEvent::Created` after 500ms debounce.
  - Rapid writes to same file within 500ms produce a single event.
  - Deleting a file sends a `FileEvent::Deleted` with no debounce delay.
  - Creating an unsupported file (.exe, .jpg) does not trigger an event.
  - Watcher emits events for files already present at startup (initial scan).
- **Verification:** Unit test with a temp directory: create/modify/delete files, assert events arrive with correct debounce behavior.

### U5. PDF Text Extraction via pdfium

- **Goal:** Extract searchable text from PDFs using pdfium-render.
- **Requirements:** R7, R11
- **Dependencies:** U1
- **Files:**
  - `src/indexer/extractors/mod.rs` (Extractor trait)
  - `src/indexer/extractors/pdf.rs`
- **Approach:** Implement the `Extractor` trait for PDF files. Use `pdfium-render` to open the PDF, iterate pages, call `page.text().all()` to extract text. Return `ExtractedContent { text, title, page_count }`. Handle errors gracefully: return `Err` for corrupt PDFs, password-protected PDFs, and PDFs with no extractable text. Log warnings, do not crash.
- **Patterns to follow:** pdfium-render crate examples for text extraction.
- **Test scenarios:**
  - Extract text from a searchable PDF with known content and verify the text is present.
  - PDF with no text layer returns an empty string (not an error — the file is indexed with empty body).
  - Corrupt PDF returns an `Err`, does not crash.
  - Password-protected PDF returns an `Err`.
  - Multi-page PDF extracts text from all pages.
- **Verification:** Unit test with fixture PDFs (one searchable, one corrupt, one password-protected).

### U6. Text File Extraction

- **Goal:** Extract text from .txt, .md, and .log files.
- **Requirements:** R7, R11
- **Dependencies:** U1
- **Files:**
  - `src/indexer/extractors/text.rs`
- **Approach:** Implement the `Extractor` trait. Read file as UTF-8 string. For .md files, strip markdown syntax or keep raw — decision: keep raw text (simpler, searchable either way). For very large files (>100MB), read only the first 10MB to avoid memory issues.
- **Patterns to follow:** Standard `std::fs::read_to_string` with UTF-8 error handling.
- **Test scenarios:**
  - Extract text from a .txt file and verify content matches.
  - Extract text from a .md file with markdown formatting — raw text is extracted.
  - Non-UTF-8 file returns an `Err`.
  - Empty file returns empty string.
- **Verification:** Unit test with fixture text files.

### U7. Indexing Pipeline Orchestrator

- **Goal:** Wire the watcher, extractors, and Tantivy indexer together into a coherent pipeline.
- **Requirements:** R7, R8, R12
- **Dependencies:** U3, U4, U5, U6
- **Files:**
  - `src/indexer/pipeline.rs`
  - `src/indexer/stages.rs`
  - `src/indexer/mod.rs`
- **Approach:** The `Pipeline` struct receives `PathBuf` events from the watcher channel. For each event: compute blake3 hash → check if already indexed with same hash (skip if unchanged) → run extractors → commit document to Tantivy → update SQLite. For `FileEvent::Deleted`: delete from Tantivy by `doc_id` and remove from SQLite. Pipeline runs on its own thread. Sends `PipelineEvent::Indexed(path)` or `PipelineEvent::Error(path, error)` to UI thread.
- **Patterns to follow:** Tantivy `IndexWriter::delete_term` for deletion.
- **Test scenarios:**
  - A new file through the pipeline is searchable via U3's search engine.
  - Modifying a file (same path, different content) updates the index with new content.
  - Renaming a file (same content, different path) does not re-index — detected by content hash.
  - Deleting a file removes it from search results.
  - Adding 200 files in quick succession indexes all of them without data races.
- **Verification:** Integration test: mock watcher sends events to pipeline, verify Tantivy index state via search.

### U8. SQLite Tag Storage

- **Goal:** Implement the tag storage layer — create, list, assign, and unassign tags.
- **Requirements:** R9, R10
- **Dependencies:** U7 (needs documents table to exist)
- **Files:**
  - `src/tags/store.rs`
  - `src/tags/model.rs`
  - `src/tags/mod.rs`
- **Approach:** `TagStore` struct wraps a `rusqlite::Connection`. On initialization, runs `CREATE TABLE IF NOT EXISTS` for the three tables. Provides methods: `create_tag(name)`, `list_tags()`, `assign_tag(content_hash, tag_id)`, `remove_tag(content_hash, tag_id)`, `get_tags_for_document(content_hash)`, `get_documents_with_tag(tag_id)`. Uses prepared statements. Errors are `thiserror` variants.
- **Patterns to follow:** rusqlite best practices — prepared statements, single connection behind `Mutex` for the indexer thread, or connection-per-thread for read operations.
- **Test scenarios:**
  - Create a tag, list tags, verify it appears.
  - Assign a tag to a document, retrieve tags for that document, verify.
  - Assign multiple tags, retrieve all, verify.
  - Remove a tag assignment, verify it's gone.
  - Deleting a tag cascades to remove all `document_tags` entries.
  - Duplicate tag name returns an error.
- **Verification:** Unit test with in-memory SQLite database.

### U9. Tag Filtering in Search

- **Goal:** Allow search results to be filtered by tags.
- **Requirements:** R10
- **Dependencies:** U3, U8
- **Files:**
  - `src/search/query.rs` (modify)
  - `src/search/engine.rs` (modify)
- **Approach:** Add `tag_filter: Option<Vec<String>>` parameter to `SearchEngine::search`. When present, add `TermQuery` clauses on the `tags` field to the BooleanQuery. Tags are denormalized into Tantivy at index time (added to the Tantivy document's `tags` field). When tags change in SQLite, the Tantivy document is updated to keep them in sync.
- **Test scenarios:**
  - Search with tag filter returns only documents with that tag.
  - Search with multiple tag filters returns only documents with all tags.
  - Search with no tag filter returns all matching documents regardless of tags.
  - Updating a document's tags in SQLite is reflected in search results.
- **Verification:** Integration test: tag documents, search with tag filter, verify filtered results.

### U10. PDF Preview Pane

- **Goal:** Render PDF pages in the preview pane with search term highlights.
- **Requirements:** R3, R4, R5
- **Dependencies:** U1 (needs egui app structure)
- **Files:**
  - `src/preview/pdf_render.rs`
  - `src/preview/highlight.rs`
  - `src/preview/mod.rs`
  - `src/app.rs` (modify: add preview panel)
- **Approach:** When a search result is clicked, `PdfPreview` opens the PDF with pdfium-render. Finds the first page with a match (using the same text extraction as U5). Renders the page to a `FPDFBitmap`, converts to `egui::ColorImage`, uploads as `egui::TextureHandle`. For highlights: use pdfium's text-find API (`FPDFTextFind*`) to get character bounding boxes, map to texture coordinates, draw semi-transparent colored rectangles via `egui::Painter`. Page navigation buttons call `render_page(n+1)` / `render_page(n-1)`. Texture cache uses LRU eviction (max 10 pages).
- **Patterns to follow:** egui texture upload example in egui docs.
- **Test scenarios:**
  - Clicking a search result renders the first page with a match.
  - Search terms are highlighted with colored overlays on the rendered page.
  - "Next page" renders the next page; "Previous page" renders the previous page.
  - Navigating past the last/first page is handled (buttons disabled or wrap).
  - Switching between search results updates the preview correctly.
  - Large PDF pages are scaled to fit the preview pane width.
- **Verification:** Manual testing with real PDFs (visual verification). Automated: verify that `pdfium-render` successfully opens and renders a page bitmap from fixture PDFs.

### U11. Text File Preview

- **Goal:** Display text file content in the preview pane with search term highlights.
- **Requirements:** R3, R4, R11
- **Dependencies:** U1
- **Files:**
  - `src/preview/mod.rs` (modify: add text preview variant)
  - `src/app.rs` (modify)
- **Approach:** Read the text file, display content in an `egui::TextEdit` (read-only) or a `ScrollArea` with `Label` widgets. Search term highlighting: scan text for matches, render matched spans with a background color using egui's rich text (`egui::RichText` with `background_color`).
- **Test scenarios:**
  - Clicking a .txt search result shows file content in the preview pane.
  - Search terms are highlighted in the previewed text.
  - .md and .log files render as plain text with highlights.
  - Large files render in a scrollable area.
- **Verification:** Integration test with fixture text files.

### U12. UI Assembly — Search Bar, Results List, and Layout

- **Goal:** Build the full three-panel UI layout and integrate all subsystems.
- **Requirements:** R1, R2, R3, R4, R5
- **Dependencies:** U2, U3, U4, U7, U10, U11
- **Files:**
  - `src/app.rs` (major rewrite from placeholder)
- **Approach:** `PapervaultApp` holds all state. Layout uses `egui::TopBottomPanel` for search bar, `egui::SidePanel` for results list, `egui::CentralPanel` for preview pane. Search bar: `TextEdit::singleline` that calls `search_engine.search()` on each keystroke. Results list: `ScrollArea` with selectable rows showing filename + snippet. Preview pane: delegates to `PdfPreview` or `TextPreview` based on file type. State transitions: `Idle → Searching → ResultsShown → Previewing`.
- **Patterns to follow:** egui demo app patterns for panels and layouts.
- **Test scenarios:**
  - Typing in the search bar updates results list in real-time.
  - Clicking a result shows the file in the preview pane.
  - Resizing the window maintains the three-panel layout proportionally.
  - Empty state: no folder configured shows setup prompt.
  - Empty state: folder configured but no results shows "No documents found."
- **Verification:** Visual verification. Can be partially automated with egui's test harness for layout assertions.

### U13. Tag UI

- **Goal:** Provide a tag panel for creating tags and assigning them to documents.
- **Requirements:** R9
- **Dependencies:** U8, U12
- **Files:**
  - `src/app.rs` (modify: add tag panel)
- **Approach:** Add a collapsible tag panel (sidebar). Shows list of all tags with checkboxes for filtering. Below the preview pane or as an overlay: "Add tag" button that shows a dropdown/combobox with existing tags plus a "Create new tag..." option. When a document is selected in the results list, its current tags are shown. Tag changes call `TagStore` methods and trigger a Tantivy document update for the tags field.
- **Test scenarios:**
  - Creating a new tag adds it to the tag list.
  - Assigning a tag to the selected document persists across app restart.
  - Removing a tag from a document updates immediately.
  - Tag filter checkboxes filter search results.
- **Verification:** Manual testing primarily. Automated: verify SQLite state after tag operations through the store layer.

---

## Test Strategy

| Layer | Tool | Scope |
|-------|------|-------|
| Unit tests | `cargo test` (standard) | Extractor correctness, SQLite operations, query building, debounce logic |
| Integration tests | `cargo test` with temp dirs | Pipeline end-to-end, Tantivy search correctness, file watcher events |
| Visual tests | Manual | PDF rendering quality, highlight positioning, UI layout |

Test fixtures live in `tests/fixtures/`:
- `sample_searchable.pdf` — a single-page PDF with known text
- `sample_multipage.pdf` — a multi-page PDF
- `sample_corrupt.pdf` — a truncated/broken PDF
- `sample.txt`, `sample.md`, `sample.log` — text files with known content

---

## System-Wide Impact

- **Disk usage:** Tantivy index for 10,000 documents with extracted text (not stored) ≈ 50–200MB. SQLite database ≈ 1–5MB. pdfium.dll ≈ 5MB.
- **Memory:** egui + Tantivy reader + pdfium ≈ 50–100MB at steady state. Texture cache adds ~50MB for 10 rendered pages at screen resolution.
- **CPU:** Idle: near-zero. Indexing burst: 1–2 cores at 100% for PDF text extraction (pdfium). Search: negligible (BM25 over 10K docs is microseconds).
- **Startup time:** Opening existing Tantivy index + SQLite + pdfium init < 1 second.

---

## Risk Analysis

| Risk | Severity | Likelihood | Mitigation |
|------|----------|------------|------------|
| pdfium-render API insufficient for highlight bounding boxes | High | Medium | Fallback: use `lopdf` for the currently-viewed page only to extract text positions |
| pdfium.dll not found on some Windows installs | High | Low | Bundle pdfium.dll with the release binary; pdfium-render supports bundled DLLs |
| Tantivy index corruption on unexpected shutdown | Medium | Low | Tantivy is crash-resistant by design; add `IndexWriter::garbage_collect_files` on startup |
| UI freeze during indexing of large PDF batch | Medium | Medium | Text extraction and Tantivy commit run on background thread; UI only receives results via channel |
| egui texture upload latency for PDF page rendering | Medium | Medium | Pre-render next page in background when user is idle; keep texture cache |

---

## Scope Boundaries

### Deferred for later
- OCR for scanned/image-based PDFs (v2)
- AI auto-tagging (v2)
- Multi-folder watching
- Dark mode
- Keyboard shortcuts

### Deferred to Follow-Up Work
- Installer/packaging (MSIX or NSIS) — manual `cargo build` for v1
- Auto-start with Windows
- System tray integration

### Outside this product's identity
- PDF editing, merging, annotation
- Cloud sync or sharing
- Non-Windows platforms
- Mobile/tablet

---

## Key Technical Decisions

- **Tantivy search on UI thread, not background channel.** Rationale: Tantivy's `Searcher` is lock-free and queries complete in microseconds. A channel round-trip adds latency with no benefit.
- **Content hash as stable document identity.** Using blake3 hash instead of file path means renames and moves don't cause re-indexing or broken tag associations.
- **Tags denormalized into Tantivy.** Tag filters are applied as Tantivy query clauses rather than a separate SQLite filter step. This keeps search fast and unified. SQLite is the source of truth; Tantivy is updated on tag changes.
- **pdfium for both extraction and rendering.** Single native dependency, single PDF parsing code path, consistent behavior between extracted text and rendered pages.
- **Index directory in AppData, not the watched folder.** Keeps the watched folder clean (no hidden index files) and avoids accidentally indexing the index.
- **Pipeline extractor trait.** Pluggable stages cost ~50 lines of abstraction now and save a rewrite when OCR and AI tagging arrive. This was a stated primary motivation for building the tool.

---

## Dependencies / Assumptions

- pdfium-render crate is well-maintained and supports the text-find API needed for highlight bounding boxes.
- notify crate's Windows backend (ReadDirectoryChangesW) reliably detects file changes on the user's system.
- The user's scanner produces PDFs with extractable text layers — OCR-only PDFs will not be searchable in v1.
- Single-threaded indexing is sufficient for the user's usage pattern (typically adding one scanned file at a time).

---

## Outstanding Questions

### Resolve Before Planning

_None._

### Deferred to Implementation

- [U10] Exact pdfium API for getting character bounding boxes with `FPDFText_Find*` — verify during U10 implementation.
- [U7] Whether Tantivy `IndexWriter::delete_term` performance is acceptable for batch deletes at 10K scale.
- [U12] Optimal egui panel sizing strategy for different window sizes.
- [U2] Whether `rfd::FileDialog` works reliably on the user's specific Windows 11 configuration.
