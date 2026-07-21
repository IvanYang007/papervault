---
title: fix: Switch Indexer to Pure-Rust PDF Extraction, pdfium Rendering-Only
type: fix
status: active
date: 2026-07-21
---

# fix: Switch Indexer to Pure-Rust PDF Extraction, pdfium Rendering-Only

## Summary

Replace the indexer's `PdfExtractor` (pdfium-render-based) with `pdf-extract`, a pure-Rust PDF text extraction library (MIT license). The renderer becomes the sole pdfium user, eliminating init contention, the `pdfium_lock` global Mutex, the `thread_safe` feature requirement, and the destroy-on-drop hazard. One thread = no locking needed.

---

## Problem Frame

The PDF rendering deadlock exists because both the indexer (PdfExtractor) and the renderer (PdfRenderer) create separate `Pdfium` instances, each calling the non-reentrant `FPDF_InitLibrary()`. The indexer only needs text extraction — it doesn't need pdfium's rendering capabilities. Switching the indexer to a pure-Rust extractor removes it from the pdfium equation entirely, leaving the renderer as the sole pdfium user.

---

## Requirements

- R1. PDF text extraction in the indexer must use a pure-Rust library (`pdf-extract`) instead of pdfium.
- R2. The global `pdfium_lock` Mutex and `thread_safe` feature are removed since only one thread uses pdfium.
- R3. Existing PDF extraction tests must pass (or be updated to use the new extractor).
- R4. Text extraction quality must be verified against the 74-file test corpus.

---

## Scope Boundaries

- The renderer (PdfRenderer) is unchanged — it continues to use pdfium-render for page rendering.
- The old `PdfExtractor` code is removed; git history preserves it.
- Non-PDF extraction (txt, md, log) is unchanged.
- pdfium.dll placement and version (Chromium 7543) are unchanged.

---

## Context & Research

### Relevant Code

- **PdfExtractor:** `src/indexer/extractors/pdf.rs` — creates pdfium instance, extracts text via `load_pdf_from_byte_slice()`, iterates pages, calls `page.text().all()`.
- **Extractor chain:** `src/indexer/stages.rs` — `create_extractor_chain()` adds PdfExtractor first, TextExtractor second.
- **pdfium_lock:** `src/main.rs:14-16` — `mod pdfium_lock` with global `Mutex<()>`.
- **Cargo.toml:** `pdfium-render = { version = "0.8", features = ["thread_safe"] }` — thread_safe feature no longer needed.
- **Test:** `src/indexer/extractors/pdf.rs` — `extract_searchable_pdf_returns_text` and other PDF tests.

### External References

- `pdf-extract` 0.9 — MIT license, ~50K downloads/month, one-call API: `pdf_extract::extract_text(path)`
- `pdf-extract` crate: https://crates.io/crates/pdf-extract

---

## Implementation Units

### U1. Add pdf-extract Dependency and Rewrite PdfExtractor

**Goal:** Replace pdfium-based PDF text extraction with `pdf-extract` in the indexer pipeline.

**Requirements:** R1

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/indexer/extractors/pdf.rs`

**Approach:**
- Add `pdf-extract = "0.9"` to `[dependencies]` in `Cargo.toml`.
- Rewrite `PdfExtractor::new()` — no longer needs pdfium initialization:
  ```rust
  pub struct PdfExtractor;
  
  impl PdfExtractor {
      pub fn new() -> Result<Self> {
          Ok(Self)
      }
  }
  ```
- Rewrite `PdfExtractor::extract()` — use `pdf_extract::extract_text(path)`:
  ```rust
  fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>> {
      let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
      if !ext.eq_ignore_ascii_case("pdf") {
          return Ok(None);
      }
      let text = pdf_extract::extract_text(path)
          .with_context(|| format!("Failed to extract PDF: {}", path.display()))?;
      let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
      Ok(Some(ExtractedContent {
          text,
          title: Some(file_name.to_string()),
          page_count: None,
      }))
  }
  ```
- Remove `PdfExtractor` struct field (no longer holds a pdfium instance).
- Remove `use crate::pdfium_lock;` import from this file.

**Test scenarios:**
- Happy path: `extract_searchable_pdf_returns_text` — extract text from generated PDF, verify non-empty
- Happy path: `extract_multipage_pdf_returns_all_pages` — verify text from all pages
- Edge case: `extract_non_pdf_file_returns_none` — should still return `Ok(None)` for non-PDF
- Error path: `extract_corrupt_pdf_returns_error` — corrupt PDF returns error
- Error path: `extract_password_protected_pdf_returns_error` — encrypted PDF returns error
- Edge case: `extract_empty_pdf_returns_zero_pages` — empty PDF handled gracefully

**Verification:**
- `cargo test` passes all PDF extractor tests
- Manual test: launch app, set Documents folder, verify search returns results for PDF content

---

### U2. Remove pdfium Lock and thread_safe Feature

**Goal:** Remove the global `pdfium_lock` Mutex, the `thread_safe` feature from pdfium-render, and the `mem::forget` pre-init since the renderer is the sole pdfium user.

**Requirements:** R2

**Dependencies:** U1

**Files:**
- Modify: `src/main.rs`
- Modify: `Cargo.toml`
- Modify: `src/runtime.rs`

**Approach:**
- Delete `mod pdfium_lock { ... }` block from `src/main.rs`.
- Remove `features = ["thread_safe"]` from `pdfium-render` in `Cargo.toml` (revert to bare `pdfium-render = "0.8"`).
- Remove pdfium pre-init block from `FolderRuntime::start()` in `src/runtime.rs` (the `std::mem::forget` section). The renderer handles its own lazy init.
- Remove `use crate::pdfium_lock;` from `src/runtime.rs`.
- Remove `let _lock = pdfium_lock::INIT...` from `src/preview/pdf_render.rs` (no longer needed — single user).

**Test scenarios:**
- Happy path: app starts without "pdfium_lock" references in logs
- Happy path: renderer initializes pdfium on first PDF click
- Edge case: no deadlock when renderer is the only pdfium user

**Verification:**
- `cargo test` passes all 48 tests
- No `pdfium_lock` references in the codebase
- Renderer works independently

---

### U3. Validation Against Full Document Corpus

**Goal:** Verify text extraction quality with `pdf-extract` against the 74-file Documents folder.

**Requirements:** R3, R4

**Dependencies:** U1, U2

**Files:**
- Manual test only

**Approach:**
- Launch the app with a clean state (delete `%LOCALAPPDATA%/papervault/`).
- Set watched folder to `C:\Users\kaipi\Documents`.
- Wait for initial scan to complete (74 files indexed).
- Search for terms known to exist in PDF files (e.g., "Billing", "invoice", "receipt").
- Verify PDFs appear in search results with correct snippets.
- Compare against known content to verify extraction quality.
- If any PDF fails extraction, the indexer logs the error and continues — the file just won't be searchable.

**Test scenarios:**
- Search "Billing" → `800171169 - Billing Notice.pdf` appears in results with snippet
- Search content from a receipt PDF → results include the expected file
- Click a PDF in file browser → preview renders (pdfium renderer working independently)

**Verification:**
- All previously-searchable PDF content is still found
- No regressions in search recall vs. pdfium-based extraction

---

## System-Wide Impact

- **pdfium usage:** Renderer only — one thread, no contention. All locks and thread-safety features are removed.
- **Indexer:** No longer depends on pdfium.dll for extraction. PDF text extraction uses pure Rust (`pdf-extract`).
- **Startup:** Pdfium pre-init on main thread removed — renderer lazy-inits on first request as originally designed.
- **Dependencies:** `pdf-extract` added (~transitive deps), `thread_safe` feature removed from pdfium-render.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|-----------|
| `pdf-extract` fails on some PDFs that pdfium handled | Test against 74-file corpus; keep pdfium-based extractor code in git history for revert |
| `pdf-extract` doesn't support password-protected or corrupt PDF handling | Existing tests cover these cases; pdf-extract returns errors that propagate through the pipeline |
| Build time increases with new dependency | `pdf-extract` is lightweight (~no native compilation needed) |

---

## Sources & References

- `src/indexer/extractors/pdf.rs` — current PdfExtractor implementation
- `src/main.rs:14-16` — `mod pdfium_lock`
- `src/runtime.rs:84-99` — pdfium pre-init with `mem::forget`
- `src/preview/pdf_render.rs` — renderer lazy-init
- Evaluation of PDF options (July 2026)
