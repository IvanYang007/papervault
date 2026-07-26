# Papervault

Fast, lightweight PDF & text file search and viewer for Windows 11. Full-text search with snippets, PDF preview with zoom, AI-powered auto-tagging via DeepSeek, split-panel layout, multi-selection batch tagging, and CJK (Chinese/Japanese) font support — all local, built in Rust.

## Features

- **Instant full-text search** across 1,000–10,000+ PDFs and text files
- **Search-as-you-type** with highlighted snippets (pre-lowercased for zero per-frame allocs)
- **Side-by-side layout** — resizable preview panel via drag handle, search results and preview both visible
- **Parallel indexing** — initial folder scan extracts 32 files at once via rayon; subsequent launches skip the scan if index is up-to-date
- **File browser** — browse all indexed documents by folder with auto-refresh and tag indicators
- **PDF preview** — two-pass rendering (low-res→full-res), LRU page cache, display-resolution rendering, encrypted PDF support
- **Page prefetch** — next page renders during idle, forward nav feels instant
- **AI auto-tagging** — DeepSeek API generates document tags from content with 3-tier caching (exact hash, filename token overlap, AI fallback)
- **Manual batch tagging** — Ctrl+click multiple files, click "Tag Selected" to trigger tagging on specific documents
- **Tags on preview** — auto-tags display at the top of the preview panel when a file is selected
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
| UI | egui 0.30 (immediate-mode, Rust-native) |
| Search | Tantivy 0.22 (full-text search engine) |
| PDF extraction | pdf_oxide 0.3 (pure Rust, ~5x faster than pdf-extract) |
| Parallel indexing | rayon (32-file batch extraction) |
| PDF rendering | pdfium-render 0.8.37 (Chromium pdfium) |
| File watching | notify 7 + walkdir (recursive) |
| Tags | SQLite (rusqlite, WAL mode, single persistent connection) |
| Auto-tagging | DeepSeek API (ureq, 3-worker thread pool) |
| Fonts | Microsoft YaHei / SimSun CJK via egui FontDefinitions |

## Architecture

```
4 threads + rayon pool + 3 auto-tagger workers: UI | Indexer | Renderer | Watcher | AutoTagger×3
5 channels: watcher, tag, render, result, progress + auto_tagger (unbounded)
```

## Performance

| 700-file collection | Time |
|---------------------|------|
| Small PDFs (1-page) | ~1.5s initial scan |
| Large PDFs (100-page) | ~20s initial scan |
| Search | <10ms |
| Page flip (cache hit) | 0ms |
| Page flip (cache miss) | ~10ms preview, ~80ms full-res |

## License

MIT
