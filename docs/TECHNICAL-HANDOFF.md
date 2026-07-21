# Technical Handoff — Papervault v1 → v1.1+

**Date:** 2026-07-21  
**Branch:** `feat/pdf-search-viewer`  
**Status:** v1 feature-complete, 48 unit tests passing, PDF rendering partially working

---

## What's Built (v1)

A Windows 11 desktop PDF/text search & viewer with egui 0.30, Tantivy 0.22, pdfium-render 0.8.37, and SQLite. Full-text search with highlighted snippets, tag management, recursive subfolder indexing, file browser panel, graceful shutdown, and atomic config save.

### Current Architecture

```
4 Threads:
  UI         — egui rendering, lock-free Tantivy search, file browser
  Indexer    — PDF/text extraction → blake3 hash → Tantivy + SQLite write
  Renderer   — pdfium page → RGBA bitmap → channel → UI TextureHandle
  Watcher    — notify-debouncer-full → recursive folder watching

5 Channels:
  watcher_tx ──bounded(10k)──▶ watcher_rx → Pipeline
  tag_tx     ──unbounded────▶ tag_rx    → Pipeline
  render_tx  ──unbounded────▶ render_rx → PdfRenderer
  result_tx  ──unbounded────▶ UI        → TextureHandle
  progress_tx──unbounded────▶ UI        → status bar

Layout:
  Left panel:    📂 File browser (all indexed docs from SQLite)
                 🏷 Tag panel (conditional)
  Center top:    🔍 Search bar
  Center below:  Search results (when typing) OR file preview
  Bottom:        Status bar
```

---

## PDF Rendering — Critical Issue

### Current State: PARTIALLY WORKING

PDF **rendering** was verified to work in a debug session (2026-07-21 21:02 UTC):
```
✓ Pdfium instance created OK
✓ Reading PDF bytes: ...0125.pdf
✓ PDF opened: 36 pages
✓ Page rendered: 612x792
```
Multiple PDFs rendered successfully. **However**, after removing debug eprintlns and cleaning up, the production build still shows "PDF preview not available" when clicking PDFs. The exact blocker is:

### Root Cause Chain (Confirmed)

1. **`FPDF_InitLibrary()` is NOT reentrant** — pdfium's C-level init function deadlocks when called simultaneously from two threads.
2. **Both indexer and renderer create separate `Pdfium` instances** — each calls `FPDF_InitLibrary()` internally.
3. **`Pdfium` is `!Send`** — pdfium-render's type cannot be shared across threads, preventing a single-instance design.
4. **`Pdfium::Drop` calls `FPDF_DestroyLibrary()`** — destroys pdfium's global state. If one thread drops its instance while the other is alive, documents become invalid.

### Fixes Applied (in order)

| Fix | Status | Detail |
|-----|--------|--------|
| `thread_safe` feature on pdfium-render | ✅ Applied | Wraps each FFI call in per-instance Mutex |
| `current_exe()`-relative DLL path | ✅ Applied | `D:\...\target\debug\pdfium.dll` instead of bare `"pdfium.dll"` |
| `pdfium_lock::INIT` global Mutex | ✅ Applied | Serializes `Pdfium::new()` calls: `src/main.rs:14-16` |
| Lock scope shrink | ✅ Applied | `_lock` drops immediately after `Pdfium::new()` in `PdfExtractor::new()` |
| Pre-init on main thread | ✅ Applied | `FolderRuntime::start()` calls `Pdfium::new()` before spawning threads |
| `std::mem::forget` on pre-init Pdfium | ✅ Applied | Prevents `FPDF_DestroyLibrary()` from tearing down global state |
| Render request coalescing | ✅ Applied | `try_recv()` drain loop keeps only latest request |
| Correct pdfium.dll | ✅ Applied | Chromium build 7543 (matches pdfium-render 0.8.37), 5.8MB |

### Remaining Issue

Despite all fixes, the production build shows `"PDF preview not available"` when clicking PDFs. The debug session proved rendering works — the issue is likely a timing/initialization race.

### Recommended Next Steps

1. **Add back eprintln traces around pdfium init** in `src/preview/pdf_render.rs` and `src/runtime.rs` to trace exactly where the init fails
2. **Verify `pdfium.dll` is correctly placed** next to `papervault.exe` (Chromium 7543, 64-bit, ~5.8MB)
3. **Test with `RUST_BACKTRACE=1`** to catch panics in the renderer thread
4. **Consider the single-thread pdfium approach** (see U1 below)

---

## Fresh Setup Guide

### 1. Prerequisites
- Rust 1.97+ (MSVC toolchain, Windows 11)
- Git

### 2. Clone and Build
```powershell
git clone https://github.com/IvanYang007/papervault.git
cd papervault
git checkout feat/pdf-search-viewer
cargo build
```

### 3. Get pdfium.dll
Download Chromium build 7543 from bblanchon/pdfium-binaries:
```
https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7543/pdfium-win-x64.tgz
```
Extract `bin/pdfium.dll` and place it next to `target/debug/papervault.exe`.

### 4. Clean State (Fresh Start)
```powershell
Remove-Item -Recurse -Force $env:LOCALAPPDATA\papervault
Remove-Item -Force $env:APPDATA\papervault\config.json
```

### 5. Run
```powershell
D:\Github\papervault\target\debug\papervault.exe
```
Click 📁 Folder → select a folder with PDFs → files will be indexed within seconds.

---

## Key Technical Decisions

| Decision | Rationale | See |
|----------|-----------|-----|
| Tantivy `IndexReader` cloned for UI | Lock-free search, <1ms latency | `src/search/engine.rs` |
| Tags: SQLite authoritative + Tantivy denormalized | Immediate tag filtering without re-index | `src/app.rs::do_search()` |
| `FolderRuntime` owns thread lifecycle | Start/stop/switch folders cleanly | `src/runtime.rs` |
| Per-folder index via blake3 hash of canonical path | Isolate indexes, support switching back | `src/search/engine.rs:index_directory()` |
| `SchemaFields` pre-cloned for UI | Remove Mutex from hot search path | `src/app.rs` (U1 lock-free search) |
| `thread_safe` feature on pdfium-render | Per-instance Mutex for FFI calls | `Cargo.toml` |
| `pdfium_lock::INIT` global Mutex | Serialize `FPDF_InitLibrary()` calls | `src/main.rs:14-16` |
| `std::mem::forget` on pre-init Pdfium | Prevent `FPDF_DestroyLibrary` during operation | `src/runtime.rs:96` |

---

## Test Coverage

- **48 unit tests** across config, error, search, extraction, tags, pipeline, store
- **1 ignored** (10K-doc performance benchmark)
- No integration tests yet

---

## Key Files by Concern

| Concern | Primary File | Notes |
|---------|-------------|-------|
| PDF rendering | `src/preview/pdf_render.rs` | Pdfium lazy-init, render loop, coalescing |
| PDF extraction (indexer) | `src/indexer/extractors/pdf.rs` | Text extraction, separate Pdfium instance |
| Pdfium lock | `src/main.rs:14-16` | `mod pdfium_lock` — global Mutex |
| Runtime orchestrator | `src/runtime.rs` | `FolderRuntime::start/stop`, thread spawn, pdfium pre-init |
| Search engine | `src/search/engine.rs` | `SearchEngine`, lock-free `search_with_reader()` |
| UI layout | `src/app.rs` | File browser, search results, preview, `browse_file()` |
| Indexing pipeline | `src/indexer/pipeline.rs` | Event loop, commit batching, reconciliation |
| File watcher | `src/watcher/watcher.rs` | Recursive, `walkdir` initial scan |
| Tag storage | `src/tags/store.rs` | SQLite, batch `get_tags_for_hashes()`, `list_all_documents()` |
| Thread orchestration | `src/main.rs` | Channel creation, `FolderRuntime` wiring |
| Config | `src/config.rs` | Atomic save (tmp → rename) |
| Plans | `docs/plans/` | 6 plan documents covering implementation history |

---

## Git History (Key Commits)

```
6cdcbe8 fix: keep pre-init Pdfium alive with mem::forget
be40850 fix: pre-init pdfium on main thread before spawning worker threads
1b09924 fix: shrink lock scope, add render coalescing
7eca893 fix: serialize FPDF_InitLibrary() across threads with global Mutex
f22d42f fix: enable thread_safe feature for pdfium-render
dc77133 fix: PDF rendering now works — serialized FPDF_InitLibrary, clean debug
f6ea215 fix: wire FolderRuntime into PapervaultApp for first-launch indexing
cb9264f fix: handle Mutex poisoning, replace fragile unwrap with expect
4ee4ee7 fix: file browser preview, PDF page nav, search result font size
c777cbf fix: clear old channel clones before stopping runtime
353e7fb feat(runtime): add FolderRuntime with per-folder indexes
097aaab fix: correctness fixes — unicode safety, render identity, progress, tags
```

---

## Remaining Work

### P0 — PDF Rendering
- Diagnose why production build shows "PDF preview not available" despite debug session proving rendering works
- Consider single-thread pdfium owner architecture:
  ```
  One thread owns Pdfium for both extraction and rendering
  Uses two channels: high-priority render requests, low-priority extraction jobs
  Eliminates all locking complexity, prevents destroy-on-drop hazard
  ```

### P1 — Polish
- SQLite connection caching (per-operation `connect()` is wasteful)
- `all_tags.clone()` per frame in tag panel
- Latin-1/Windows-1252 explicit encoding support
- CI pipeline (GitHub Actions)

### P2 — Future
- OCR for scanned PDFs
- AI auto-tagging
- Keyboard shortcuts
- Release packaging / installer
