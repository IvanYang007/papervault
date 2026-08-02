# Papervault

Fast, lightweight PDF & text file search and viewer for Windows 11. Full-text search with snippets, PDF preview with zoom, AI-powered auto-tagging via DeepSeek, split-panel layout, multi-selection batch tagging, and CJK (Chinese/Japanese) font support — all local, built in Rust.

## Features

- **Instant full-text search** across 1,000–10,000+ PDFs and text files (Tantivy, typically <10ms)
- **Search-as-you-type** with highlighted snippets (match spans pre-computed per search — zero per-frame work)
- **Side-by-side layout** — resizable preview panel via drag handle, search results and preview both visible
- **Explorer-style file browser** — virtualized Name / Modified / Size columns with drag-to-resize headers, sortable by click, tag indicators (✨)
- **Parallel indexing** — initial folder scan extracts 32 files at once via rayon; subsequent launches skip the scan if the index is up-to-date; SQLite writes batched into one transaction per 32-file batch
- **PDF preview** — cached parsed document (parsed once per file, ~6× faster page flips), two-pass rendering (low-res→full-res), LRU page cache, display-resolution rendering, encrypted PDF support
- **Page prefetch** — next page renders during idle, forward nav feels instant
- **AI auto-tagging** — DeepSeek API generates document tags from content with 3-tier caching (exact hash, filename token overlap, AI fallback)
- **No wasted API calls** — already-tagged files are never re-sent to the API; re-indexing preserves both AI and manual tags (no wipe via FK cascade)
- **One-click re-index for tags** — re-tags the whole library through a durable DB queue (survives queue backpressure, picks up automatically)
- **API circuit breaker** — a dead DeepSeek endpoint fails fast instead of churning for hours; recovers with a probe call after the cooldown
- **Manual batch tagging** — Ctrl+click multiple files, click "Tag Selected" to trigger tagging on specific documents
- **Tag system** — organize, filter, and search by tags with post-filtering; tags refresh live without restart
- **Chinese (CJK) font support** — Chinese, Japanese characters render correctly
- **Recursive subfolder indexing** — watches all subdirectories
- **Comprehensive logging** — full pipeline audit trail in `papervault.log`
- **Zero cloud** — everything runs locally (except optional DeepSeek auto-tagging)

## Quick Start

1. Download `papervault.exe` from [Releases](https://github.com/IvanYang007/papervault/releases)
2. Place `pdfium.dll` next to `papervault.exe`
3. Launch the app, click 📁 Folder, select your documents folder
4. Files are indexed automatically — start searching immediately
5. Optional: Set `DEEPSEEK_API_KEY` env var and enable Auto-tagging in the Folder dialog for AI-generated tags

## Build from Source

```powershell
git clone https://github.com/IvanYang007/papervault.git
cd papervault
cargo build --release

# Get pdfium.dll (Chromium 7543):
# Download from: bblanchon/pdfium-binaries → chromium/7543 → pdfium-win-x64.tgz
# Extract bin/pdfium.dll next to papervault.exe

# Run:
.\target\release\papervault.exe
```

## Tech Stack

| Component | Technology |
|-----------|-----------|
| UI | egui 0.30 + egui_extras 0.30 (immediate-mode, Rust-native) |
| Search | Tantivy 0.22 (full-text search engine) |
| PDF extraction | pdf_oxide 0.3 (pure Rust, ~5x faster than pdf-extract) |
| Parallel indexing | rayon (32-file batch extraction) |
| PDF rendering | pdfium-render 0.8.37 (Chromium pdfium) |
| File watching | notify 7 + walkdir (recursive) |
| Tags | SQLite (rusqlite, WAL + synchronous=NORMAL, prepared-statement cache, batched transactions) |
| Auto-tagging | DeepSeek API (ureq, 3-worker thread pool, circuit breaker, atomic row claiming) |
| Fonts | Microsoft YaHei / SimSun CJK via egui FontDefinitions |

## Architecture

```
4 threads + rayon pool + 3 auto-tagger workers: UI | Indexer | Renderer | Watcher | AutoTagger×3
6 channels: watcher (bounded 256), tag, render, result, progress + auto_tagger (bounded 256)
File-browser snapshots are computed on the indexer thread; the UI never scans the DB
```

## Performance

| Workload | Before | After |
|----------|--------|-------|
| SQLite writes, 5000-file scan | 402 ms (per-file autocommit) | 36 ms (batched transactions) — **11×** |
| PDF page flip (warm) | 2.2 ms (document re-parsed) | 0.37 ms (cached document) — **6×** |
| Auto-tag status fetch, 50 results | 435 µs (50 per-row queries) | 152 µs (1 batch query) — **3×** |
| Search | <10 ms | <10 ms |

Additional gains that don't show in micro-benchmarks: zero per-frame SQLite queries (results list, tag panel, preview all read an in-memory cache), virtualized file browser (only visible rows laid out), and no duplicate AI calls (atomic row claiming).

## License

MIT
