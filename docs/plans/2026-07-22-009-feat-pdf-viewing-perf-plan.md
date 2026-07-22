---
title: feat: Improve PDF viewing performance — pre-warm, cache, prefetch, two-pass, display-res
type: feat
status: active
date: 2026-07-22
---

# feat: Improve PDF viewing performance

## Summary

Five targeted optimizations to make PDF page-flipping and zooming feel instant: pre-warm pdfium on startup, cache rendered page bitmaps in an LRU, prefetch the next page during idle time, two-pass render (low-res→high-res) for instant feedback, and render at the preview panel's actual pixel size instead of a fixed maximum.

---

## Problem Frame

Currently every page turn and zoom change triggers a full `pdfium` re-parse + render at up to 2000×zoom pixels. First click latency includes pdfium init + file read + parse + render (~100–500ms). Page flipping is 100% raster-bound with no caching. Zooming re-renders at the same resolution. The render target is a fixed max dimension, not the panel size — a 400px-wide panel gets a 2000px render that's immediately downscaled.

---

## Requirements

- R1. First PDF click renders in under 100ms (currently 100–500ms).
- R2. Flipping back to a recently-visited page shows instantly (0ms, from cache).
- R3. Forward page navigation shows the next page in under 50ms after the current page renders.
- R4. Zoom change shows a recognizable low-res preview within 10ms, replaced by full-res within 100ms.
- R5. Render target matches the preview panel's visible pixel dimensions.

---

## Scope Boundaries

- Only `src/preview/pdf_render.rs` and the preview UI in `src/app.rs` are modified.
- TagStore, search engine, indexer, watcher, and extractors are unchanged.
- Text file preview is unchanged.
- The PDF renderer thread architecture (single thread, channel-based) is preserved — no thread pool added.

---

## Context & Research

### Relevant Code and Patterns

- **PdfRenderer:** `src/preview/pdf_render.rs` — runs on dedicated thread, receives `RenderRequest`, sends `RenderResult`. Already eagerly initializes pdfium on the renderer thread.
- **Preview UI:** `src/app.rs` — `request_page_render()`, `browse_file()`, `select_result()`, `poll_channels()`. The preview texture is an `egui::TextureHandle` stored in `preview_texture: Option<egui::TextureHandle>`.
- **RenderRequest:** `src/app.rs:48` — `{ request_id, path, page, zoom }`.
- **RenderResult:** `src/app.rs:57` — `{ request_id, path, page, page_count, rgba_bytes, width, height }`.

### External References

- pdfium-render 0.8.37 API — `PdfPage::render_with_config()` takes `PdfRenderConfig` with `set_target_width()` / `set_target_height()`.
- egui 0.30 — `ui.available_size()` returns the panel's pixel dimensions. `ctx.load_texture()` creates a `TextureHandle` from RGBA bytes.

---

## Key Technical Decisions

- **LRU cache in PdfRenderer** — a `VecDeque<(CacheKey, Vec<u8>)>` with simple linear eviction is sufficient for a single-user desktop app. The `lru` crate adds a dependency for ~30 lines of code savings; not worth it.
- **Two-pass via render_then_replace** — the renderer sends a low-res `RenderResult` immediately, then a full-res one. The UI just calls `ctx.load_texture()` again for the same texture name; egui replaces it automatically.
- **Prefetch via priority field** — add `priority: u8` to `RenderRequest` (0=prefetch, 1=normal). The renderer coalesces normal requests (latest-wins) but processes prefetch requests from a separate queue only when the normal queue is empty.
- **Display resolution via available_size** — add `target_width: u32, target_height: u32` to `RenderRequest`. The UI computes these from `ui.available_size()` before sending the request.

---

## Implementation Units

### U1. Add `target_width`/`target_height` to RenderRequest and use display resolution

**Goal:** The renderer renders at the preview panel's actual pixel size, not a fixed 2000px max.

**Requirements:** R5

**Dependencies:** None

**Files:**
- Modify: `src/app.rs` (RenderRequest, call sites)
- Modify: `src/preview/pdf_render.rs` (do_render)

**Approach:**
- Add `target_width: u32, target_height: u32` fields to `RenderRequest`.
- In `do_render()`, if `target_width > 0 && target_height > 0`, use those instead of the `max_dim` computation. Still clamp zoom: `let w = target_width * zoom; let h = target_height * zoom`.
- In the UI, compute available size: `let avail = ui.available_size(); request.target_width = avail.x as u32; request.target_height = avail.y as u32`.
- Remove the old `max_dim` cap — display resolution is the cap.

**Test scenarios:**
- Happy path: 400px panel → render target ~400px wide → render completes faster than 2000px
- Happy path: zoom at 200% → 800px wide render → still correct
- Edge case: panel resized → next render uses new dimensions

**Verification:**
- `cargo test` passes.
- Manual: open a PDF, check render quality at default zoom vs 200% zoom.

---

### U2. LRU page cache in PdfRenderer

**Goal:** Cache recently rendered page bitmaps so back-navigation is instant.

**Requirements:** R2

**Dependencies:** U1

**Files:**
- Modify: `src/preview/pdf_render.rs`

**Approach:**
- Define a cache key: `(PathBuf, usize /* page */, u32 /* zoom percent */)`.
- Add `page_cache: VecDeque<(CacheKey, Vec<u8>, u32 /* width */, u32 /* height */)>` to `PdfRenderer`.
- Add `const MAX_CACHED_PAGES: usize = 8`.
- In `render_page()`, before rendering, check the cache: if key matches, return cached `RenderResult` immediately.
- After rendering, push into cache front. If cache exceeds `MAX_CACHED_PAGES`, pop from back.
- On path change (new file selected), clear the cache.
- The cache stores raw RGBA bytes — at 1200×1600 typical page, that's ~7.6 MB per page. 8 pages = ~60 MB max.

**Test scenarios:**
- Happy path: flip to page 2, then back to page 1 → page 1 returns from cache (0ms).
- Happy path: flip through pages 1→2→3→4→5, cache holds last 8 pages, older ones evicted.
- Edge case: change zoom → cache miss (different zoom = different key), re-render.
- Edge case: open different PDF → cache cleared.

**Verification:**
- `cargo test` passes.
- Manual: open multi-page PDF, flip pages forward and back, observe instant back-navigation.

---

### U3. Two-pass rendering (low-res preview → full-res replacement)

**Goal:** Show a recognizable low-res preview within 10ms, then replace with full-res.

**Requirements:** R4

**Dependencies:** U1, U2

**Files:**
- Modify: `src/preview/pdf_render.rs`
- Modify: `src/app.rs` (handle replacement)

**Approach:**
- In `render_page()`, first render at 1/4 target dimensions (e.g., `target_width/4 × target_height/4`). Send this as a `RenderResult` with a flag `is_low_res: bool = true`.
- Then render at full target dimensions. Send as `is_low_res: false`.
- The UI receives both results. Both create textures with the same name (e.g., `"pdf-preview"`). egui's `ctx.load_texture()` replaces the old texture when called again with the same name.
- The low-res result carries all the same metadata (page_count, path) so the page navigation UI works immediately.
- Add `is_preview: bool` to `RenderResult` — the UI can optionally show a spinner or loading indicator while `is_preview` is true, but the immediate image swap is likely good enough.

**Test scenarios:**
- Happy path: render page 1 → low-res appears in ~10ms → full-res replaces it in ~80ms.
- Happy path: navigate to page 3 → low-res page 3 appears → full-res page 3 replaces.
- Edge case: rapid page clicking → coalescing ensures only the latest request's full-res is delivered. Intermediate low-res results are discarded by the `request_id` check in `poll_channels()`.

**Verification:**
- `cargo test` passes.
- Manual: open any PDF, observe near-instant preview followed by sharpening within ~100ms.

---

### U4. Prefetch next page

**Goal:** While the user reads page N, pre-render page N+1 so forward navigation is instant.

**Requirements:** R3

**Dependencies:** U2

**Files:**
- Modify: `src/app.rs` (RenderRequest, request_page_render)
- Modify: `src/preview/pdf_render.rs` (render loop)

**Approach:**
- Add `priority: u8` to `RenderRequest`. `0 = prefetch`, `1 = normal`.
- After a normal render completes and the result is sent on `poll_channels()`, the UI sends a prefetch request for page N+1 (and optionally N-1).
- The renderer maintains a separate `prefetch_rx` channel or uses the existing channel but processes prefetch only when no normal requests are pending.
- Simplest approach: use a single channel. The renderer loop checks: drain normal requests first (coalesce, keep latest), then if no normal request, check for one prefetch request. Prefetch results are sent to the UI but the UI only applies them if they match the current page.
- The UI ignores prefetch results for pages that are no longer current (already handled by `request_id` check in `poll_channels()`).

**Test scenarios:**
- Happy path: view page 1 → prefetch page 2 → click Next → page 2 from cache.
- Happy path: view page 5 → prefetch page 6 and page 4.
- Edge case: rapid clicking skips prefetched pages → prefetch results discarded by request_id.

**Verification:**
- `cargo test` passes.
- Manual: open multi-page PDF, observe that Next Page is faster than first render.

---

### U5. Move pdfium pre-warm to startup

**Goal:** Initialize pdfium during `FolderRuntime::start()` so the renderer never pays the init cost on first render.

**Requirements:** R1

**Dependencies:** None (can be done independently)

**Files:**
- Modify: `src/preview/pdf_render.rs` (make `init_pdfium` pub(crate))
- Modify: `src/runtime.rs` (call init_pdfium during start)

**Approach:**
- The renderer already eagerly initializes pdfium in `PdfRenderer::new()` — but this happens when the renderer thread spawns, which is after the user has already clicked a folder.
- Move `PdfRenderer::init_pdfium()` to a `pub(crate)` function.
- In `FolderRuntime::start()`, call `PdfRenderer::init_pdfium()` and store the result in a field. Pass it to `PdfRenderer::new()` as `Option<Pdfium>`.
- If init fails, the renderer can retry — this just moves the init earlier, it doesn't break the retry path.
- Benefit: by the time the user clicks their first PDF, pdfium is already loaded. Only file read + parse + render remain.

**Test scenarios:**
- Happy path: start app with configured folder → pdfium initialized during startup → first PDF click renders faster.
- Edge case: pdfium.dll missing → error logged at startup, first PDF click shows error as before.

**Verification:**
- `cargo test` passes.
- Manual: start app, click first PDF, observe latency.

---

## System-Wide Impact

- **Renderer memory:** ~60 MB max for the 8-page LRU cache.
- **Channel traffic:** Two-pass rendering doubles the number of `RenderResult` messages per render (one low-res, one full-res). The coalescing drain loop already handles this.
- **Prefetch:** Adds at most 1 extra `RenderRequest` in the channel at any time.
- **Unchanged invariants:** Single renderer thread, channel-based communication, `Pdfium` instance not shared across threads, `ensure_pdfium()` retry path still works.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|-----------|
| LRU cache memory for large pages (A1/A0 PDFs) | Cap at 8 pages, evict LRU. 8× A4 pages = ~60 MB acceptable for desktop. |
| Two-pass rendering causes flicker | egui replaces textures by name — no flicker. Low-res image is shown until full-res replaces it seamlessly. |
| Prefetch renders waste CPU if user clicks rapidly | Coalescing drain loop discards stale prefetch results. Request_id check in poll_channels ignores stale responses. |
| Display resolution too low for high-DPI displays | Use `ui.available_size()` which returns physical pixels on egui with `pixels_per_point > 1.0`. |

---

## Sources & References

- `src/preview/pdf_render.rs` — current PdfRenderer implementation
- `src/app.rs` — preview UI and RenderRequest/RenderResult types
- `src/runtime.rs` — FolderRuntime thread lifecycle
- pdfium-render 0.8.37 docs — pdfium_render::prelude::PdfRenderConfig
