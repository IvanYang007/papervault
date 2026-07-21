# Technical Handoff — Papervault v1.1

**Date:** 2026-07-21  
**Branch:** `feat/pdf-search-viewer`  
**Status:** ✅ v1.1 feature-complete — 48 unit tests, clippy clean, PDF preview working

---

## What's Built

Windows 11 desktop PDF/text search & viewer. Rust + egui 0.30 + Tantivy 0.22 + pdfium-render 0.8.37 + pdf-extract 0.9 + SQLite.

| Feature | Status |
|---------|--------|
| Full-text search with highlighted snippets | ✅ |
| Recursive subfolder indexing (walkdir) | ✅ |
| File browser panel (all indexed docs from SQLite) | ✅ |
| PDF preview with page navigation + zoom | ✅ |
| Text file preview with 2MB cap | ✅ |
| Tag management (create, assign, filter) | ✅ |
| FolderRuntime lifecycle (start/stop/switch) | ✅ |
| Per-folder index via blake3 hash | ✅ |
| Lock-free search (SchemaFields pre-cloned) | ✅ |
| Graceful shutdown (thread joins in order) | ✅ |
| Atomic config save (tmp → rename) | ✅ |
| Conditional windows_subsystem (debug=console, release=GUI) | ✅ |
| Release profile (lto, strip, panic=abort) | ✅ |

---

## Architecture

```
4 Threads:
  UI         — egui rendering, lock-free Tantivy search, file browser, preview
  Indexer    — pdf-extract/TextExtractor → blake3 → Tantivy + SQLite write
  Renderer   — pdfium page → RGBA bitmap → channel → UI TextureHandle
  Watcher    — notify-debouncer-full → recursive folder watching

5 Channels:
  watcher_tx ──bounded(10k)──▶ watcher_rx → Pipeline
  tag_tx     ──unbounded────▶ tag_rx    → Pipeline
  render_tx  ──unbounded────▶ render_rx → PdfRenderer (coalescing: latest-wins)
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

## Key Technical Decisions

| Decision | Rationale |
|----------|-----------|
| `pdf-extract` for indexer, pdfium for renderer only | Eliminates FPDF_InitLibrary deadlock. Pure-Rust extraction, pdfium only for rendering — one thread, no contention |
| `FolderRuntime` owns thread lifecycle | Clean start/stop/switch with `stop()` joining in order: watcher→indexer→renderer |
| `SchemaFields` pre-cloned for UI | Removes Mutex from hot search path. Reader cloned at startup |
| Per-folder index via blake3 of canonical path | Isolates indexes, supports switching folders without cross-contamination |
| Batch tag query (`get_tags_for_hashes`) | Single SQL query for all result tags, eliminates N+1 |
| `active_tag_filters: HashSet<String>` | O(1) tag check instead of O(n) Vec |
| Tags loaded once at startup | No per-frame `list_tags()` SQLite query |
| `MAX_MESSAGES_PER_FRAME = 64` | Bounded channel processing prevents UI starvation |
| Render request coalescing | `try_recv()` drain loop keeps only latest request |
| `current_exe()`-relative DLL path | Works regardless of CWD |
| Reconcile on indexer thread | UI starts instantly, reconciliation runs in background |
| Eager pdfium init on renderer thread | First PDF click is instant (no lazy-init delay) |
| Zoom debounce (per-frame) | 10 rapid zoom clicks → 1 render request |

---

## PDF Rendering Saga — Resolved

The original design had both indexer and renderer creating separate `Pdfium` instances. `FPDF_InitLibrary()` is not reentrant — two threads calling it simultaneously deadlocked.

| Attempt | What | Outcome |
|---------|------|---------|
| 1 | `thread_safe` feature on pdfium-render | ❌ Still deadlocks — protects FFI calls, not init |
| 2 | Global `pdfium_lock::INIT` Mutex | ❌ Indexer holds lock for 17s during batch extraction |
| 3 | Lock scope shrink + pre-init + `mem::forget` | ❌ `FPDF_DestroyLibrary` tears down global state |
| 4 | **Switch indexer to `pdf-extract` (pure Rust)** | ✅ One pdfium user, no contention |

---

## Files by Concern

| Concern | File |
|---------|------|
| PDF rendering | `src/preview/pdf_render.rs` |
| PDF extraction (indexer) | `src/indexer/extractors/pdf.rs` |
| Search engine | `src/search/engine.rs` |
| UI (app state, search, preview, layout) | `src/app.rs` |
| Indexing pipeline | `src/indexer/pipeline.rs` |
| Runtime orchestrator | `src/runtime.rs` |
| File watcher | `src/watcher/watcher.rs` |
| Tag storage | `src/tags/store.rs` |
| Thread orchestration, channels | `src/main.rs` |
| Config (atomic save) | `src/config.rs` |
| Error types | `src/error.rs` |

---

## Fresh Setup

```powershell
git clone https://github.com/IvanYang007/papervault.git
cd papervault
git checkout feat/pdf-search-viewer
cargo build

# Download pdfium.dll (Chromium 7543):
# https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/7543/pdfium-win-x64.tgz
# Extract bin/pdfium.dll → target/debug/

# Clean state:
Remove-Item -Recurse -Force $env:LOCALAPPDATA\papervault
Remove-Item -Force $env:APPDATA\papervault\config.json

# Run:
.\target\debug\papervault.exe
```

---

## Key Git Commits (Recent)

```
cb6061f fix: debounce PDF zoom — one render per frame instead of per click
b6eeaf0 perf: faster startup, eager pdfium init, PDF zoom
197e533 fix: add unique id_salt to all ScrollArea widgets
ac6ad1f chore: fix clippy warnings
292ee36 fix: switch indexer to pure-Rust pdf-extract, remove pdfium_lock
866d278 docs: update technical handoff with PDF rendering saga
6cdcbe8 fix: keep pre-init Pdfium alive with mem::forget
be40850 fix: pre-init pdfium on main thread before spawning worker threads
1b09924 fix: shrink lock scope, add render coalescing
7eca893 fix: serialize FPDF_InitLibrary() across threads with global Mutex
f22d42f fix: enable thread_safe feature for pdfium-render
dc77133 fix: PDF rendering now works (debug session confirmed)
cb9264f fix: handle Mutex poisoning, replace fragile unwrap with expect
4ee4ee7 fix: file browser preview, PDF page nav, search result font size
c777cbf fix: clear old channel clones before stopping runtime
353e7fb feat(runtime): add FolderRuntime with per-folder indexes
097aaab fix: correctness fixes — unicode safety, render identity, progress, tags
```

---

## Remaining Work

| Priority | Item |
|----------|------|
| P1 | SQLite connection caching (per-operation `connect()` is wasteful) |
| P1 | Integration tests (pipeline end-to-end, concurrent search+index) |
| P2 | OCR for scanned PDFs |
| P2 | Keyboard shortcuts |
| P2 | Release packaging / installer |
| P3 | Latin-1/Windows-1252 encoding support |
| P3 | CJK tokenizer |
