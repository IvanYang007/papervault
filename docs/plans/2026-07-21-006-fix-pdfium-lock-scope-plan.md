---
title: fix: Shrink pdfium Lock Scope & Prevent Destroy-on-Drop Hazard
type: fix
status: active
date: 2026-07-21
---

# fix: Shrink pdfium Lock Scope & Prevent Destroy-on-Drop Hazard

## Summary

The global `pdfium_lock::INIT` Mutex is held across the entire batch extraction loop (~17s for 74 files) instead of only during `Pdfium::new()` (~1ms). Fix the lock scope so the guard drops immediately after construction. Additionally, ensure `Pdfium` instances live for the entire process lifetime to avoid `FPDF_DestroyLibrary` tearing down global state while the other thread has open documents. Finally, add render-request coalescing so rapid clicking renders only the latest request.

---

## Problem Frame

The previous fix (global Mutex around `Pdfium::new()`) correctly serializes init calls but the lock scope was too broad — the indexer's lock guard spans the entire batch-processing loop, blocking the renderer for seconds. Additionally, `Pdfium::Drop` calls `FPDF_DestroyLibrary()` which destroys pdfium's global state. If one thread drops its instance while the other is alive, open documents become invalid. Finally, rapid PDF clicking queues unbounded render requests, causing apparent freezes.

---

## Requirements

- R1. Lock scope: `pdfium_lock::INIT` guard must drop immediately after `Pdfium::new()` returns (~1ms), never across work loops.
- R2. Pdfium instances live for the entire thread lifetime — created once, never dropped until thread exit.
- R3. Render requests coalesce: only the latest request is rendered; stale requests are drained with `try_recv`.

---

## Implementation Units

### U1. Fix Lock Scope

**Goal:** The Mutex guard drops immediately after `Pdfium::new()`, not held for the full extraction or render cycle.

**Files:**
- Modify: `src/indexer/extractors/pdf.rs` — `PdfExtractor::new()`
- Modify: `src/preview/pdf_render.rs` — `render_page()`

**Approach:**
- Use block scoping to ensure `_lock` drops at the closing brace:
  ```rust
  let pdfium = {
      let _lock = pdfium_lock::INIT.lock().unwrap_or_else(|e| e.into_inner());
      Pdfium::new(bindings)
  }; // _lock drops here, lock held ~1ms
  ```
- In the renderer's `render_page()`, the lock already uses block scope — verify it drops before file I/O.

**Verification:**
- Lock hold time < 5ms measured via eprintln timestamps around lock acquire/release
- Renderer can initialize pdfium during active indexing (no 17s stall)

---

### U2. Lifetime-Protect Pdfium Instances

**Goal:** `Pdfium` instances live for the full thread lifetime. `FPDF_DestroyLibrary()` is never called while another thread has a live `Pdfium`.

**Files:**
- Modify: `src/indexer/extractors/pdf.rs`
- Modify: `src/indexer/pipeline.rs` or `src/indexer/stages.rs` (where extractor is created)

**Approach:**
- The `PdfExtractor` is currently created via `PdfExtractor::new()` which creates a `Pdfium` internally. Store the `PdfExtractor` in `Pipeline` as a field, created once in `Pipeline::new()`.
- The renderer already stores `Pdfium` in `Arc<Mutex<Option<Pdfium>>>` — it's created once on first render request and never dropped until thread exit. This is already correct.
- Add `#[allow(dead_code)]` or documentation comments noting these fields are intentionally never dropped during operation.

**Verification:**
- No `FPDF_DestroyLibrary` call between creation and process exit
- Both extractor and renderer can operate simultaneously without corruption

---

### U3. Render-Request Coalescing

**Goal:** Rapid PDF clicks queue at most one pending render; only the latest request is processed.

**Files:**
- Modify: `src/preview/pdf_render.rs`

**Approach:**
- In `PdfRenderer::run()`, after `recv()` returns a request, drain the channel with `try_recv()` and keep only the last:
  ```rust
  let mut request = self.request_rx.recv()?;
  // Drain stale requests — only render the latest
  while let Ok(newer) = self.request_rx.try_recv() {
      request = newer;
  }
  self.render_page(&request, &pdfium)?;
  ```
- This handles the case where the user clicks 5 PDFs in rapid succession — only the last one renders.

**Verification:**
- Click 5 different PDFs rapidly → only the last one's preview appears
- No unbounded render queue buildup

---

## System-Wide Impact

- **Lock hold time** drops from 17s to <5ms — renderer initializes instantly
- **No more freeze** when clicking PDFs during initial scan
- **Request coalescing** prevents render queue buildup
- **No cross-thread corruption** from premature `FPDF_DestroyLibrary` calls

---

## Sources & References

- AI diagnostic feedback (July 2026) — lock scope analysis, destroy-on-drop hazard, coalescing fix
- `src/indexer/extractors/pdf.rs` — PdfExtractor creates Pdfium in new()
- `src/preview/pdf_render.rs` — PdfRenderer lazy-creates Pdfium in render_page()
- `src/main.rs:14-16` — `mod pdfium_lock`
