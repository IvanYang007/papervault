---
title: feat: Replace pdf-extract with pdf_oxide for faster PDF text extraction
type: feat
status: active
date: 2026-07-21
---

# feat: Replace pdf-extract with pdf_oxide for faster PDF text extraction

## Summary

Swap the indexer's PDF text extraction from `pdf-extract` 0.9 to `pdf_oxide` 0.3 — a higher-performance pure-Rust PDF library. `pdf_oxide` claims ~5x faster extraction (0.8ms vs 4.08ms mean) with better pass rates (100% vs 91.5% on a 3,830-document corpus). The renderer, search engine, tags, and all other subsystems remain unchanged.

---

## Requirements

- R1. PDF text extraction in the indexer must use `pdf_oxide` instead of `pdf-extract`.
- R2. The new extractor must preserve the existing `Extractor` trait contract — `extract(&self, path: &Path) -> Result<Option<ExtractedContent>>`.
- R3. All 49 existing tests must pass, with unit tests updated to match the new API behavior.
- R4. The `page_count` field in `ExtractedContent` should be populated when the PDF reports its page count (was previously `None`).

---

## Scope Boundaries

- The PDF renderer (`PdfRenderer`) is unchanged — it continues to use `pdfium-render`.
- Non-PDF extraction (txt, md, log via `TextExtractor`) is unchanged.
- The extractor chain in `src/indexer/stages.rs` is unchanged (still calls `PdfExtractor` first).
- OCR, CJK tokenization, and other deferred features are not in scope.
- PDF rendering performance is not in scope — this is an indexer-side change only.

---

## Context & Research

### Relevant Code and Patterns

- **PdfExtractor:** `src/indexer/extractors/pdf.rs` — pure-Rust extractor using `pdf_extract::extract_text(path)`. No DLLs, no locks. The `Extractor` trait has a single method: `fn extract(&self, path: &Path) -> Result<Option<ExtractedContent>>`.
- **Extractor chain:** `src/indexer/stages.rs` — `create_extractor_chain()` adds `PdfExtractor` first, `TextExtractor` second. `run_chain()` tries each extractor in order and returns the first `Ok(Some(...))`.
- **TextExtractor:** `src/indexer/extractors/text.rs` — handles txt, md, log. Reference for extractor implementation pattern.
- **ExtractedContent:** `src/indexer/extractors/mod.rs` — `{ text: String, title: Option<String>, page_count: Option<usize> }`. Currently `page_count` is always `None`.

### External References

- `pdf_oxide` 0.3 — MIT/Apache-2.0 license, pure Rust. API: `PdfDocument::open(path)?` → `doc.page_count()?` → `doc.extract_text(page_index)?`.
- Performance benchmarks: https://pdf.oxide.fyi/docs/performance — claims 0.8ms mean extraction vs 4.08ms for pdf-extract, 100% pass rate on 3,830 real-world PDFs.
- Rust API docs: https://docs.rs/pdf_oxide/latest/pdf_oxide/api/index.html
- Text extraction guide: https://pdf.oxide.fyi/docs/extraction/text

### Institutional Learnings

- Prior plan `2026-07-21-007` switched the indexer from pdfium-based extraction to `pdf-extract` to eliminate the `FPDF_InitLibrary` deadlock. The key constraint was: *pure-Rust extraction for the indexer, pdfium only for renderer*. `pdf_oxide` satisfies this constraint.
- The `Extractor` trait is designed so initialization never fails (`PdfExtractor::new()` returns `Result` but always succeeds). This pattern must be preserved.

---

## Key Technical Decisions

- **Use `PdfDocument::open()` with per-page loop:** `pdf_oxide` opens the document once and iterates pages. This is slightly more code than `pdf_extract::extract_text(path)` (one call), but gives us access to `page_count()` — enabling R4.
- **Keep `PdfExtractor` as a unit struct:** No state needed — `pdf_oxide::PdfDocument` is opened per-extraction call, same as `pdf-extract`'s implicit open.
- **Do not change the `Extractor` trait:** The trait is the stable contract between the pipeline and extractors. Other extractors are untouched.

---

## Implementation Units

### U1. Swap pdf-extract for pdf_oxide in PdfExtractor

**Goal:** Replace the `pdf_extract::extract_text()` call with `pdf_oxide::PdfDocument::open()` plus per-page `extract_text()` loop. Populate `page_count` when available.

**Requirements:** R1, R2, R4

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/indexer/extractors/pdf.rs`

**Approach:**
- Replace `pdf-extract = "0.9"` with `pdf_oxide = "0.3"` in `Cargo.toml`.
- Rewrite `PdfExtractor::extract()` to:
  1. Check file extension → return `Ok(None)` for non-PDF (unchanged).
  2. Open the PDF with `pdf_oxide::PdfDocument::open(path)`.
  3. Get `page_count` via `doc.page_count()`.
  4. Loop through pages 0..page_count, call `doc.extract_text(i)`, join with newlines.
  5. Return `ExtractedContent { text, title, page_count: Some(page_count) }`.
- Keep `PdfExtractor::new() -> Result<Self>` unchanged (still `Ok(Self)`).
- Remove `use pdf_extract` (implicit); add `use pdf_oxide::PdfDocument`.

**Patterns to follow:**
- `src/indexer/extractors/text.rs` — `Extractor` trait implementation pattern.
- `src/indexer/extractors/pdf.rs` — current implementation structure to mirror.

**Test scenarios:**
- Happy path: `extract_searchable_pdf_returns_text` — verify extracted text contains known content AND `page_count` is populated.
- Happy path: `extract_multipage_pdf_returns_all_pages` — verify text from all 5 pages AND `page_count == 5`.
- Edge case: `extract_non_pdf_file_returns_none` — unchanged behavior.
- Error path: `extract_corrupt_pdf_returns_error` — corrupt PDF (`%PDF-1.4\n%%EOF`) returns `Err`.
- Edge case: `extract_no_text_layer_returns_empty_or_minimal` — no-text PDF does not crash, returns `Ok`.
- Edge case: `extract_empty_pdf_returns_text` — 0-page PDF handled gracefully (pdf_oxide may error or return empty).
- Edge case: `extract_password_protected_pdf_returns_error` — encrypted PDF handled gracefully.

**Verification:**
- `cargo test` passes all 49 tests (PDF extractor tests updated, not removed).
- `cargo build --release` compiles cleanly.
- Manual smoke test: index a folder with PDFs, search for known content, verify results appear.

---

### U2. Remove pdf-extract from Cargo.toml and Clean Up

**Goal:** Ensure `pdf-extract` is fully removed from the dependency tree and no stale references remain.

**Requirements:** R1, R3

**Dependencies:** U1

**Files:**
- Modify: `Cargo.lock` (auto-pruned by `cargo update`; `Cargo.toml` was already updated in U1)

**Approach:**
- The `pdf-extract` dependency line in `Cargo.toml` was replaced by `pdf_oxide = "0.3"` in U1. This unit is verification-only.
- Run `cargo update` to prune the lock file of pdf-extract's transitive dependencies.
- Verify `cargo tree | grep pdf-extract` returns nothing.
- The `printpdf` and `lopdf` dev-dependencies are NOT affected — they are used by the test helpers (`generate_searchable_pdf`, etc.) and remain.

**Test scenarios:**
- `cargo build` and `cargo test` succeed with `pdf_oxide` as the sole PDF extraction dependency.
- `cargo tree` shows no `pdf-extract` in the dependency tree.

**Verification:**
- `cargo test` passes all existing tests. New tests added in U1 bring the total to 50+.
- `cargo clippy` produces no new warnings.
- `cargo build --release` produces a working binary.

---

## System-Wide Impact

- **Indexer performance:** PDF text extraction speed should improve ~5x based on vendor benchmarks. Real-world gains depend on document characteristics.
- **Binary size:** `pdf_oxide` may have a different size footprint than `pdf-extract`. Monitor release binary size.
- **Error propagation:** Error handling is unchanged — extraction failures return `Err` through the `Extractor` trait, logged by the pipeline, and the file is skipped (with `last_error` written to SQLite).
- **Unchanged invariants:**
  - The `Extractor` trait signature is preserved.
  - The extractor chain order (`PdfExtractor` → `TextExtractor`) is preserved.
  - The `ExtractedContent` struct fields are unchanged (only `page_count` goes from `None` to `Some(n)` for the happy path).
  - The renderer thread, search engine, tags, file watcher, and UI are untouched.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|-----------|
| `pdf_oxide` fails on PDFs that `pdf-extract` handled | Test against existing test corpus; `pdf_oxide` claims 100% pass rate on 3,830 PDFs vs `pdf-extract`'s 91.5%. The old implementation is preserved in git history for revert. |
| `pdf_oxide` 0.3 is relatively new (pre-1.0 API) | The API surface we use is minimal (open, page_count, extract_text) — unlikely to break in minor updates. Pin to `"0.3"` for reproducibility. |
| Binary size increase | `pdf_oxide` 0.3 pulls in ~60+ transitive dependencies (ttf-parser, image, rustybuzz, etc.) vs `pdf-extract`'s lighter tree. Baseline current binary (11.8 MB release) before the swap; flag degradation >15% and investigate feature flags if needed. |

---

## Sources & References

- **Origin document:** `docs/plans/2026-07-21-007-pure-rust-extraction-plan.md` (prior extraction migration)
- **Technical handoff:** `docs/TECHNICAL-HANDOFF.md`
- Related code: `src/indexer/extractors/pdf.rs`, `src/indexer/stages.rs`, `src/indexer/extractors/mod.rs`
- External docs: https://docs.rs/pdf_oxide/latest/pdf_oxide/api/index.html
- Performance: https://pdf.oxide.fyi/docs/performance
