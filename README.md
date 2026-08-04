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

## AI Agent Installation Guide

Step-by-step install an AI agent (or a new user) can follow end-to-end on a
fresh Windows 11 machine. A deeper postmortem of the auto-tagging pitfalls
lives at `D:/Github/docs/solutions/rust-windows/auto-tagger-retag-churn.md`.

### 1. Prerequisites

- Windows 11, 64-bit
- [Rust toolchain](https://rustup.rs) (stable)
- Git
- (Optional) A DeepSeek API key for AI auto-tagging

### 2. Build

```powershell
# Close any running papervault.exe first — it locks the build output.
git clone https://github.com/IvanYang007/papervault.git
cd papervault
cargo build --release
```

### 3. pdfium.dll (required for PDF preview)

```powershell
# Download from bblanchon/pdfium-binaries → chromium/7543 → pdfium-win-x64.tgz
# Extract and place bin/pdfium.dll NEXT TO papervault.exe:
Copy-Item pdfium.dll .\target\release\pdfium.dll
```

### 4. First run + folder

```powershell
.\target\release\papervault.exe
```

1. Click 📁 Folder and select the documents folder, OR pre-seed
   `%APPDATA%\papervault\config.json`:
   `{ "watched_folder": "Z:\\ScanOriginal" }`
2. Files index automatically (metadata fast-path skips unchanged files on
   later launches).

### 5. Auto-tagging (optional, requires DeepSeek)

1. Set the API key as a **user environment variable** (the app reads it at
   request time; never write the key into any file):
   ```powershell
   setx DEEPSEEK_API_KEY "your-key-here"
   ```
2. Create `%APPDATA%\papervault\auto_tag.json`:
   ```json
   {
     "enabled": true,
     "provider": "deepseek",
     "model": "deepseek-v4-flash",
     "endpoint": "https://api.deepseek.com/v1/chat/completions",
     "api_key_env": "DEEPSEEK_API_KEY",
     "max_retries": 3,
     "request_timeout_secs": 240,
     "max_tags_per_doc": 5,
     "max_text_words": 500,
     "thinking_enabled": false,
     "max_tokens": 24000
   }
   ```
   `thinking_enabled: false` is the default and recommended — thinking
   mode burns tokens for no benefit on tag extraction.
3. Restart the app. The queue shows tagging progress; the first pass tags
   every document.

### 6. Verify the install (do this after the first pass finishes)

1. Restart the app once more.
2. The tagging queue must stay at zero (no "waiting" files).
3. Check the session log — it must show **0** API calls:
   ```powershell
   $log = Get-ChildItem "$env:LOCALAPPDATA\papervault\papervault-*.log" |
          Sort-Object LastWriteTime | Select-Object -Last 1
   (Select-String -Path $log.FullName -Pattern "tier 3 \(AI fallback\)").Count
   # expect: 0
   ```
4. Sanity: the status line in the log reads `871 docs total, 871 with tags`
   (counts match your library size).

### Known pitfalls (all fixed in code — do not "fix" them again)

- **Network-share watcher:** on SMB drives (Z:), `notify` reports spurious
  `Remove` events. The watcher now only deletes rows when the file is
  really gone — do not remove that `path.exists()` guard.
- **Re-tag churn:** duplicate scans under different filenames share one
  content-hash row; `already_tagged` compares **content only**
  (`blake3(text)`), accepting legacy name-based rows. A launch showing
  API calls after the second restart means something regressed.
- **DeepSeek thinking:** the only valid switch is
  `thinking: {"type": "disabled"}`; `thinking_effort` is silently ignored.
- **Debug logging:** `RUST_LOG=debug` grows the session log ~90 MB/min.
  Use it briefly, then unset it.
- **Logs & DB locations:** session logs and `papervault.db` live in
  `%LOCALAPPDATA%\papervault\` (retention: 3 days); config in
  `%APPDATA%\papervault\`.

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
