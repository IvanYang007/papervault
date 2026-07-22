# Papervault

Fast, lightweight PDF & text file search and viewer for Windows 11. Full-text search with snippets, PDF preview with zoom, recursive subfolder indexing — all local, built in Rust.

## Features

- **Instant full-text search** across 1,000–10,000+ PDFs and text files
- **Search-as-you-type** with highlighted snippets
- **File browser** — browse all indexed documents by folder
- **PDF preview** with page navigation and zoom (25%-400%)
- **Tag system** — organize, filter, and search by tags
- **Recursive subfolder indexing** — watches all subdirectories
- **Zero cloud** — everything runs locally

## Quick Start

1. Download `papervault.exe` from [Releases](https://github.com/IvanYang007/papervault/releases)
2. Place `pdfium.dll` next to `papervault.exe` (included in the repo at `target/release/`)
3. Launch the app, click 📁 Folder, select your documents folder
4. Files are indexed automatically — start searching immediately

## Build from Source

```powershell
git clone https://github.com/IvanYang007/papervault.git
cd papervault
git checkout feat/pdf-search-viewer
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
| PDF extraction | pdf-extract 0.9 (pure Rust) |
| PDF rendering | pdfium-render 0.8.37 (Chromium pdfium) |
| File watching | notify 7 + walkdir (recursive) |
| Tags | SQLite (rusqlite, WAL mode) |
| Channels | crossbeam |

## Architecture

```
4 threads: UI | Indexer | Renderer | Watcher
5 channels: watcher, tag, render, result, progress
```

See `docs/TECHNICAL-HANDOFF.md` for full details.

## License

MIT
