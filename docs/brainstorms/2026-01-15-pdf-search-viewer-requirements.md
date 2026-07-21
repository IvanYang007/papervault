---
date: 2026-01-15
topic: pdf-search-viewer
---

# Papervault — PDF Search & Viewer

## Summary

A Windows 11 desktop app that watches a single folder of PDFs and text files, indexes their full text instantly, and provides a single-window search experience: type → see results with highlighted matches → click to preview. Built in Rust with egui, Tantivy, and pdfium.

---

## Problem Frame

Ivan manages 1,000–10,000 PDF documents and text files in a single folder on Windows 11. His scanner produces searchable PDFs with embedded text layers, but finding specific content requires Windows Search or File Explorer — both too slow, neither searches inside PDFs effectively, and results scatter across separate windows. He used to rely on old Evernote's fast, unified search-with-preview experience, but no longer has that tool. The daily cost is time spent opening files one by one to find the right document, and documents that are effectively lost because their content isn't searchable through the OS.

---

## Actors

- A1. **Ivan (document owner):** Searches for documents by full-text content, previews them, and assigns tags. Single user, local machine.

---

## Key Flows

- F1. **Search and Preview**
  - **Trigger:** Ivan types a search query into the search bar.
  - **Actors:** A1
  - **Steps:** (1) App queries Tantivy index with the search string. (2) Results appear in the results list as Ivan types, each showing filename, match snippet, and match count. (3) Ivan clicks a result. (4) Preview pane renders the document with search terms highlighted. (5) For PDFs, the preview shows the first page containing a match. Ivan can scroll pages.
  - **Outcome:** Ivan finds the document and reads the relevant content in one window.
  - **Covered by:** R1, R2, R3, R4, R5

- F2. **File Discovery and Indexing**
  - **Trigger:** A new PDF or text file appears in the watched folder (created by scanner, download, or manual copy).
  - **Actors:** System (no human actor)
  - **Steps:** (1) File system watcher detects the new file. (2) Text extraction runs on a background thread — pdfium for PDFs, direct read for text files. (3) Extracted text is committed to the Tantivy index. (4) File metadata (path, modified time, content hash) is stored.
  - **Outcome:** New file is searchable in under 2 seconds. Existing search results are not disrupted during indexing.
  - **Covered by:** R6, R7, R8

- F3. **Tag Assignment**
  - **Trigger:** Ivan wants to categorize a document.
  - **Actors:** A1
  - **Steps:** (1) Ivan selects a document from results or preview. (2) Assigns or removes tags via the tag panel. (3) Tags persist across app restarts.
  - **Outcome:** Document is tagged. Search can be filtered by tag.
  - **Covered by:** R9, R10

---

## Requirements

**Search**
- R1. Full-text search across all indexed PDFs and text files must return results as the user types (search-as-you-type), with results appearing within 200ms of each keystroke at 10,000-document scale.
- R2. Search results display filename, a snippet of matching text with the search term, and the total match count per file.
- R3. Search terms must be visually highlighted in both the results list snippets and the preview pane content.

**Preview**
- R4. Clicking a search result opens the document in the preview pane. For PDFs, the preview renders the actual page (via pdfium) with search term highlights overlaid.
- R5. The preview pane supports page navigation (next/previous) for multi-page PDFs.

**Indexing**
- R6. The app watches a single configured folder for new, modified, and deleted files using filesystem events.
- R7. New and modified PDFs are text-extracted and indexed in the background. The search index remains queryable during indexing.
- R8. Deleted files are removed from the index automatically.

**Tags**
- R9. Users can create tags and assign them to documents. Tag state persists across app restarts.
- R10. Search results can be filtered by one or more tags.

**File Support**
- R11. Supported formats in v1: PDF (with embedded text layers), plain text (.txt), Markdown (.md), log files (.log).
- R12. File additions, modifications, and deletions in the watched folder are reflected in search results with minimal delay.

---

## Acceptance Examples

- AE1. **Covers R1, R6, R7.** Given a watched folder with 5,000 indexed PDFs, when Ivan copies 200 new searchable PDFs into the folder, each new file becomes searchable within 2 seconds of appearing on disk, and existing search queries continue to return results during indexing.

- AE2. **Covers R1, R8.** Given a watched folder with indexed files, when Ivan deletes 50 files from the folder, those files no longer appear in search results within 5 seconds.

- AE3. **Covers R3, R4.** Given a search query "invoice March", when Ivan clicks a result, the preview pane shows the PDF page with every occurrence of both "invoice" and "March" highlighted, and the result snippet in the list also shows highlighted matches.

- AE4. **Covers R9, R10.** Given a document tagged "tax" and "2025", when Ivan filters search results by the tag "tax", only documents with that tag appear, and adding the "2025" filter further narrows to documents with both tags.

---

## Success Criteria

- Ivan stops using Windows Search for finding document content and uses Papervault as his primary document lookup tool.
- Search feels instant — no perceptible lag between typing and seeing results at full scale.
- An implementer reading this document and the downstream plan can build the app without inventing product behavior.

---

## Scope Boundaries

- OCR is deferred to v2 — v1 handles only PDFs that already contain searchable text layers.
- AI auto-tagging is deferred to v2 — v1 has manual tags only, but the tag storage design must expose a clean interface for future AI integration.
- Multi-folder watching is out of scope — v1 watches a single folder.
- PDF editing, merging, splitting, annotation, and form-filling are out of scope — this is a viewer and search tool.
- Cloud sync, network storage, multi-user, and collaboration are out of scope.
- Non-Windows platforms are out of scope — Windows 11 only.
- Dark mode, i18n, and accessibility are deferred to post-v1 polish.

---

## Key Decisions

- **pdfium over pure-Rust PDF crates:** Chrome's PDF engine for both text extraction and page rendering. Reliability and compatibility for 1,000–10,000 real-world PDFs outweigh the cost of bundling pdfium.dll (~5MB).
- **Pipeline architecture for indexing:** The indexing pipeline is designed as pluggable stages (text extraction → [OCR v2] → [AI tagging v2] → Tantivy commit) so future features slot in without restructuring.
- **Search+preview over search+tags for v1 MVP:** Tags are essential but ship the core pain-killer (find documents fast) first. Tag data model goes into v1; tag assignment UI waits for v1.1.
- **egui for UI:** Pure Rust, fast, lightweight, and well-suited to data-dense search result UIs. Matches the "no fancy features, utilitarian" constraint.
- **Single-folder watching:** Simpler architecture, matches the user's actual file organization.

---

## Dependencies / Assumptions

- **pdfium.dll:** Must be bundled or discoverable on the user's Windows 11 system. pdfium-render crate handles this.
- **PDF text layers:** Assumes all PDFs in the watched folder have extractable text layers. Scanned-image-only PDFs without OCR text will not be searchable in v1.
- **Single user, local files:** No authentication, authorization, or network resilience needed.
- **NTFS filesystem:** notify crate's Windows backend relies on ReadDirectoryChangesW, which is NTFS-native.

---

## Outstanding Questions

### Resolve Before Planning

_None — all product decisions are resolved._

### Deferred to Planning

- [Affects R3][Technical] Best approach for overlaying highlighted search terms on pdfium-rendered page bitmaps in egui.
- [Affects R7][Technical] Concurrency model for background indexing: dedicated thread vs. async tokio task.
- [Affects R9][Technical] Tag storage schema: SQLite schema design for tags with future AI-writable interface.
- [Needs research] pdfium-render vs pdfium-crate trade-offs for Windows — which binding is better maintained.
- [Needs research] Tantivy index directory placement: inside watched folder vs. AppData.
