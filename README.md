# Papervault

Fast, lightweight PDF & text file search and viewer for Windows 11. Full-text search with snippets, PDF preview with zoom, parallel indexing, two-pass rendering — all local, built in Rust.

## Features

- **Instant full-text search** across 1,000–10,000+ PDFs and text files
- **Search-as-you-type** with highlighted snippets (pre-lowercased for zero per-frame allocs)
- **Parallel indexing** — initial folder scan extracts 32 files at once via rayon
- **File browser** — browse all indexed documents by folder with refresh cooldown
- **PDF preview** — two-pass rendering (low-res→full-res), LRU page cache, display-resolution rendering
- **Page prefetch** — next page renders during idle, forward nav feels instant
- **Tag system** — organize, filter, and search by tags with post-filtering
- **Recursive subfolder indexing** — watches all subdirectories
- **Zero cloud** — everything runs locally

## Quick Start

1. Download `papervault.exe` from [Releases](https://github.com/IvanYang007/papervault/releases)
2. Place `pdfium.dll` next to `papervault.exe`
3. Launch the app, click 📁 Folder, select your documents folder
4. Files are indexed automatically — start searching immediately

## Build from Source

```powershell
git clone https://github.com/IvanYang007/papervault.git
cd papervault
git checkout feat/pdf-oxide-extraction
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
| Channels | crossbeam |

## Architecture

```
4 threads + rayon pool: UI | Indexer | Renderer | Watcher
5 channels: watcher, tag, render, result, progress
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
