# Papervault

Fast, lightweight PDF & text file search and viewer for Windows 11. Like old Evernote's search — but local, instant, and built in Rust.

## Features

- **Instant full-text search** across 1,000–10,000+ PDFs and text files
- **Search-as-you-type** with context-aware snippets and term highlighting
- **Single-window UI** — search bar, results list, and preview pane in one view
- **PDF preview** with page navigation (next/previous)
- **Tag system** — organize documents with tags, filter search results by tag
- **Auto-indexing** — watches a folder, indexes new/changed files automatically
- **Non-searchable PDFs** — scanned/image PDFs are viewable and findable by filename
- **Zero cloud dependencies** — everything runs locally on your machine

## Quick Start

### Download & Run

Download `papervault.exe` from [Releases](https://github.com/IvanYang007/papervault/releases) and double-click.

### First Launch

1. Click **📁 Folder** → **Browse…** → select your documents folder
2. The app scans and indexes all PDF, `.txt`, `.md`, and `.log` files
3. Start typing in the search bar — results appear instantly

### Usage

| Action | How |
|--------|-----|
| **Search** | Type in the search bar (search-as-you-type with 150ms debounce) |
| **Preview** | Click a search result to view it in the center pane |
| **PDF pages** | Use ◀ / ▶ buttons above the preview |
| **Tags** | Click 🏷 to open tag panel — create, assign, and filter by tags |
| **Change folder** | Click 📁 Folder to select a different watched folder |

### Where Data Is Stored

| Data | Location |
|------|----------|
| Settings | `%LOCALAPPDATA%\papervault\config.json` |
| Search Index | `%LOCALAPPDATA%\papervault\index\` |
| Tags Database | `%LOCALAPPDATA%\papervault\papervault.db` |
| Crash Log | `%LOCALAPPDATA%\papervault\crash.log` |

## Supported File Types

| Extension | Type | Notes |
|-----------|------|-------|
| `.pdf` | PDF | Searchable text layers required for full-text search. Scanned/image PDFs are viewable and findable by filename. |
| `.txt` | Plain text | UTF-8 and Latin-1 supported |
| `.md` | Markdown | Raw markdown indexed |
| `.log` | Log files | Multi-line log text |

## Development

### Prerequisites

- Rust 1.80+ (MSVC toolchain)
- Windows 11 (uses `windows_subsystem = "windows"`)
- `pdfium.dll` — bundled with release builds; for development, place in `PATH` or project root

### Build

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test

# Run with logging (debug only, console window visible)
cargo run
```

### Project Structure

```
papervault/
├── src/
│   ├── main.rs           # Entry point, thread orchestration, panic hook
│   ├── app.rs            # egui UI — search, results, preview, tags
│   ├── config.rs         # JSON config persistence
│   ├── error.rs          # Error types (thiserror)
│   ├── search/
│   │   ├── engine.rs     # Tantivy index lifecycle, search, indexing
│   │   ├── schema.rs     # Tantivy schema definition
│   │   └── query.rs      # SearchRequest, SearchResult DTOs
│   ├── indexer/
│   │   ├── pipeline.rs   # Indexing orchestrator, reconciliation
│   │   ├── stages.rs     # Extractor chain assembly
│   │   └── extractors/
│   │       ├── mod.rs    # Extractor trait + ExtractedContent
│   │       ├── pdf.rs    # PDF text extraction via pdfium
│   │       └── text.rs   # Text file extraction
│   ├── watcher/
│   │   └── watcher.rs    # notify-debouncer-full file watcher
│   ├── tags/
│   │   ├── model.rs      # Tag, DocumentTag structs
│   │   └── store.rs      # SQLite tag storage (WAL mode)
│   └── preview/
│       ├── pdf_render.rs # Background PDF render thread
│       └── highlight.rs  # Search term highlight overlay
├── tests/
│   └── fixtures/         # Test PDF and text fixtures
├── docs/
│   ├── brainstorms/      # Requirements documents
│   ├── plans/            # Design & implementation plans
│   ├── test-plan.md      # Test strategy & cases
│   └── TECHNICAL-HANDOFF.md  # Developer handoff for next stage
└── benches/              # Benchmarks (planned)
```

### Tech Stack

| Component | Crate | Description |
|-----------|-------|-------------|
| GUI | `egui` + `eframe` | Immediate-mode GUI, wgpu backend |
| Search | `tantivy` 0.22 | Full-text search engine (lock-free MVCC readers) |
| PDF | `pdfium-render` 0.8 | Chrome's PDF engine for extraction + rendering |
| File Watch | `notify-debouncer-full` 0.4 | Debounced filesystem events |
| Database | `rusqlite` 0.32 | WAL-mode SQLite for tags and metadata |
| Channels | `crossbeam` 0.8 | Multi-producer channels for thread communication |
| Hashing | `blake3` 1.5 | Content-based document identity |

### Architecture

```
┌──────────────────────────────────┐
│        egui Application          │
│  Search Bar │ Results │ Preview  │
└──────────────┬───────────────────┘
               │ crossbeam channels
    ┌──────────┼──────────┬──────────┐
    ▼          ▼          ▼          ▼
  Search    Indexer    Renderer   Watcher
 (UI thr)  (bg thr)   (bg thr)   (bg thr)
 tantivy   pipeline   pdfium     notify
 reader    + SQLite              debouncer
```

- **4 threads**: UI (search + render), Indexer (extract + commit), Renderer (PDF bitmap), Watcher (file events)
- **Lock-free search**: Tantivy's `IndexReader` cloned for UI thread — no Mutex during search
- **Crash-safe**: `reconcile()` at startup repairs Tantivy/SQLite inconsistencies
- **Graceful shutdown**: `AtomicBool` signal → watcher stops → channel closes → indexer commits → thread joins

## v1 Limitations

- Single folder only (no subdirectories, no multi-folder)
- PDFs must have searchable text layers for full-text search (scanned PDFs: filename search only)
- Tags sync to search on next file change (not instantly after assignment)
- No keyboard shortcuts
- Dark mode only
- No installer — portable `.exe`

## License

MIT
