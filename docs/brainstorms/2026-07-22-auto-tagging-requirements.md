---
date: 2026-07-22
topic: auto-tagging
---

# Auto-Tagging for Papervault

## Summary

Auto-generate document tags and extract named entities from PDF content using the DeepSeek flash model. The system performs two tasks per document: (1) classify the document's purpose and topic, and (2) extract structured entities — person names, organizations, years, and document IDs. Entity tags are normalized to handle name variations (e.g., "Yang Guorui" ↔ "guorui yang" ↔ "yangguorui") so the user can search `yang guorui tax 2023` and find all matching documents regardless of how names appear in the PDF. Tags are written to both SQLite and Tantivy's search index for immediate searchability.

---

## Problem Frame

Papervault users accumulate hundreds or thousands of PDFs. The app already supports full-text search and manual tagging, but in practice **users don't tag anything**. They dump files into watched folders and rely on full-text search to find specific documents by memory.

The dominant search pattern is **entity-driven retrieval**, not topic browsing. A user searches `yang guorui tax 2023` because they need their father's 2023 tax return. The PDF might contain "Guorui Yang," "YANG GUORUI," "Yang Guo Rui" (character-separated), or OCR-garbled variants like "Yang Guorul". Full-text search alone fails here — it can't normalize name variations or distinguish a person mention from ambient text.

Manual tagging is the counterfactual. Users don't do it because the cost (opening each PDF, deciding on tags, typing them) vastly outweighs the perceived value. Auto-tagging changes the tradeoff: structured entities appear without effort, normalized for searchability, and the user can find documents by who they're about, what type they are, and what year they reference.

The filename is the strongest untapped signal. A file named `2023-tax-return-yang-guorui.pdf` carries person, year, and document type in the name the user chose. Combined with text extraction, the model can extract entities the user will actually search for.

---

## Actors

- A1. **Library Owner**: Has 100–10,000 PDFs — personal documents, academic papers, legal contracts, tax records. Needs to find specific documents by person + type + year. Has never manually tagged anything.
- A2. **Power Tagger**: A user who already manually tags some documents. Wants auto-tags to fill gaps without disrupting their existing tag vocabulary.

---

## Key Flows

- F1. **Opt-in on folder import**
  - **Trigger:** User clicks "Add Folder" or drops a folder onto the app.
  - **Actors:** A1, A2
  - **Steps:**
    1. App presents folder import dialog with an auto-tagging checkbox: "Generate tags automatically using AI."
    2. Checkbox defaults to unchecked (off by default).
    3. If user checks it, a disclosure appears: "Document text and filenames are sent to DeepSeek (servers in China) for entity extraction and tag generation. Text is not stored by the service. A DeepSeek API key is required. [Configure API key]"
    4. User confirms. Folder is indexed. Auto-tag suggestions appear as indexing completes.
  - **Outcome:** Auto-tagging is enabled for the folder. Tags are generated for all indexable documents. A status bar indicator shows auto-tagging is active.
  - **Covered by:** R0, R1, R2

- F2. **Batch auto-tagging on import**
  - **Trigger:** Folder import completes (auto-tagging enabled).
  - **Actors:** A1 (indirect — happens automatically)
  - **Steps:**
    1. Indexer extracts text from each PDF (first 3 pages + metadata, cap 2000 words).
    2. Indexer indexes text into Tantivy (document is immediately searchable via body text, sans tags).
    3. Indexer sends each document to the AutoTagger thread via bounded channel with `(filename, extracted_text, metadata)`.
    4. AutoTagger sends a request to the DeepSeek API: filename + extracted text → model returns structured JSON with `tags` (purpose/type) and `entities` (persons, organizations, years, doc_ids, amounts).
    5. AutoTagger normalizes entity tags (see R22 name normalization pipeline).
    6. AutoTagger writes tags + entities to SQLite and updates the Tantivy index (tags added to the `tags` field as TEXT tokens).
    7. UI shows progress bar: "Analyzing 47 documents — 12/47."
  - **Outcome:** Auto-tag suggestions appear in the tag panel. Entity tags (person, org, year) are visually distinct from topic tags. The Tantivy index includes all tags and normalizes variants, so searching "yang guorui" matches "Guorui Yang" documents. Skipped/failed documents are counted and reported.
  - **Covered by:** R2, R3, R4, R5, R6, R21, R22, R23

- F3. **Accept or dismiss a suggestion**
  - **Trigger:** User clicks on an auto-tag in the tag panel.
  - **Actors:** A1, A2
  - **Steps:**
    1. User clicks the dashed-border auto-tag. It toggles to solid border (accepted). A second click toggles back to dashed.
    2. User right-clicks the auto-tag and selects "Dismiss." The tag is removed and marked as dismissed in SQLite — it will not reappear.
    3. If user manually adds a tag with the same normalized name as an existing auto-tag, the auto-tag is absorbed — it becomes a single manual tag. No duplicate display.
  - **Outcome:** Accepted tags behave identically to manual tags. Dismissed tags stay gone. Manual tags always win.
  - **Covered by:** R8, R11, R12

- F4. **Skip unanalyzable documents**
  - **Trigger:** A PDF in the import batch cannot be processed.
  - **Actors:** A1 (informed passively)
  - **Steps:**
    1. Extraction returns < 50 chars of text → mark `skipped` with reason `"no-text-layer"`.
    2. pdf_oxide reports encrypted/password-protected → mark `skipped` with reason `"locked"`.
    3. File size > 50MB or page count > 500 → cap extraction at 5 pages, proceed normally.
    4. After batch completes, UI shows: "3 of 47 documents skipped" with a hover tooltip listing filenames and reasons.
  - **Outcome:** Unanalyzable documents don't block the batch. Users see which documents were skipped and why. No OCR in v1.
  - **Covered by:** R5, R9

- F5. **API unavailability fallback**
  - **Trigger:** DeepSeek API returns an error or is unreachable during batch processing.
  - **Actors:** A1 (informed passively)
  - **Steps:**
    1. AutoTagger retries up to 3 times with exponential backoff (1s, 2s, 4s).
    2. If all retries fail, the document is marked `failed` with the error reason stored.
    3. Remaining documents in the queue continue unaffected.
    4. After batch completes, UI shows: "2 of 47 failed — check your API key and connection. [Retry failed]"
    5. If every document in the batch fails (global failure: invalid API key, network down), the AutoTagger pauses and the UI shows a persistent error banner.
  - **Outcome:** Per-document failure isolation. Global failures are clearly communicated with a retry path.
  - **Covered by:** R6, R7, R9

- F6. **Search by entity (precision retrieval)**
  - **Trigger:** User types a search query containing person name + document type + year (e.g., "yang guorui tax 2023").
  - **Actors:** A1
  - **Steps:**
    1. Query is split on whitespace into terms: `["yang", "guorui", "tax", "2023"]`.
    2. Each term is searched against `body`, `file_name`, AND `tags` fields (TEXT, tokenized).
    3. Because tags are tokenized and normalized (e.g., tag `"yang-guorui"` → tokens `["yang", "guorui"]`), the search matches documents tagged with `"Guorui Yang"`, `"YANG GUORUI"`, or `"yangguorui"` (all normalized to the same token set).
    4. If zero results are returned, the search engine automatically retries with fuzzy matching (Levenshtein distance 1) on body and tags fields to catch OCR errors.
    5. Results are ranked by Tantivy's BM25 scoring across all matched fields.
  - **Outcome:** The user finds all documents related to Yang Guorui's 2023 taxes, regardless of how the name appears in the PDF or how the AI chose to tag it.
  - **Covered by:** R23, R24

---

## Requirements

**Privacy gate**
- R0. Before any document text is sent to an external API, the system must warn the user. The opt-in disclosure in F1 must communicate: (a) data is sent to DeepSeek (servers in China), (b) which data is sent (filename + extracted text from first 3 pages), (c) text is not stored by the service per DeepSeek's policy, (d) the user is responsible for ensuring documents sent to the API do not violate applicable data protection obligations. The API key is never stored in config files or logs (R10).

**Auto-tagging engine**
- R1. Auto-tagging is **off by default**. Users must opt in at folder-import time via a checkbox in the import dialog. A DeepSeek API key must be configured before auto-tagging can be enabled.
- R2. When enabled and configured, auto-tagging fires automatically after indexing — no separate "analyze" button to discover.
- R3. Tags are generated by the **DeepSeek flash model** via the DeepSeek API (`api.deepseek.com`). The request payload includes the document's filename and its extracted text (first 3 pages, 2000-word cap). No other AI provider is required for v1.
- R4. The prompt instructs the model to perform **two tasks**: (T1) classify the document's purpose/type into 3–5 topic tags, and (T2) extract structured named entities — person names, organizations, years, document IDs, and monetary amounts. The filename is the primary signal for classification. Output is structured JSON with separate `tags` and `entities` objects (see Prompt Strategy section).
- R5. Documents with fewer than 50 characters of extractable text across the first 3 pages are skipped with reason `"no-text-layer"`. Password-protected documents are skipped with reason `"locked"`. No OCR is attempted in v1.

**Resilience & caching**
- R6. A BLAKE3 content hash of the extracted text + filename is stored alongside auto-tag results. Re-indexing a document whose hash matches an existing `tagged` entry must skip the API call entirely.
- R7. API calls are retried up to 3 times with exponential backoff (1s, 2s, 4s). 4xx client errors (invalid API key, bad request) are not retried and fail immediately. 5xx server errors and network timeouts are retried. Per-document failures do not block the remaining queue.
- R8. User-dismissed auto-tags must persist as dismissed across app restarts. They are never regenerated unless the user explicitly triggers re-analysis (deferred to v1.1).

**Data integrity**
- R9. Per-document auto-tagging status (`pending`, `in_progress`, `tagged`, `failed`, `skipped`) and metadata (reason, attempts, last_error, timestamps) are stored in a new `auto_tag_status` database table, created via schema migration on upgrade.
- R10. The API key is referenced by environment variable (`DEEPSEEK_API_KEY`) and never stored in config files, the database, or logs. The config file stores only the variable name reference, model name, and endpoint URL.

**UI surface**
- R11. Auto-tags appear in the tag panel visually distinct from manual tags: sparkle icon, dashed border for unconfirmed suggestions, solid border when accepted. Entity tags (person, org, year, doc_id, amount) are rendered with distinct type icons (person silhouette, building, calendar, document, currency) so users can visually distinguish entities from topic tags.
- R12. When a user manually creates a tag with the same normalized name as an existing auto-tag, the auto-tag is absorbed — it becomes a single manual tag. No duplicate display.
- R13. A "Why this tag?" tooltip on hover shows the document filename and 2–3 text snippets that contributed to the tag.
- R14. A progress bar with `(completed / total)` and estimated remaining time is shown during batch auto-tagging. A final summary reports tagged, skipped, and failed counts with per-document detail on hover.
- R15. A status bar indicator (cloud icon) shows when auto-tagging is active for at least one watched folder. The icon shows an error state (red) when the API is unreachable.

**Architecture**
- R16. Auto-tagging runs on a dedicated thread, separate from the existing UI, Indexer, Renderer, and Watcher threads. A bounded crossbeam channel (capacity 100) decouples the Indexer from the AutoTagger.
- R17. The tag generation logic is behind a `TagProvider` trait. v1 ships with a single `DeepSeekProvider` implementation. The trait is designed so additional providers (local Ollama, OpenAI) can be added without refactoring consumers.

**Configuration & security**
- R18. Auto-tagging configuration is stored in `auto_tag.json` in the app data directory: provider name, model name (`deepseek-chat`), endpoint URL, API key environment variable name, max retries, request timeout. The API key value is never written to this file.
- R19. The DeepSeek API endpoint, model, and timeout are user-configurable. The default endpoint is `https://api.deepseek.com/v1`, default model is `deepseek-chat` (flash variant).

**Observability**
- R20. All auto-tagging events (document queued, API request sent, response received, tag parsed, skipped, failed, retry) are emitted as structured `tracing` events with document ID, filename, status, latency, and token usage fields.

**Entity extraction & normalization**
- R21. The AI model returns structured JSON with two objects: `tags` (3–5 topic/type tags) and `entities` (persons, organizations, years, doc_ids, amounts). Entities are stored separately from topic tags in SQLite so the UI can render them with distinct styling and the search index can weight them appropriately. See Prompt Strategy section for the exact output schema.
- R22. All person-name entity tags undergo a normalization pipeline before storage: (1) lowercase, (2) strip diacritics/accents/tone marks via Unicode NFD + strip combining marks, (3) strip internal spaces for CJK names ("yang guo rui" → "yangguorui"), (4) generate name-order variants (given+surname, surname+given). All normalized variants are stored in the Tantivy index. The canonical form (as returned by the model) is displayed in the UI.

**Search integration**
- R23. After auto-tags are committed to SQLite, the AutoTagger must update the document's Tantivy index entry with the new tags. The existing `tags` field in the Tantivy schema must be included in the main query term loop (`search_with_reader`) alongside `body` and `file_name` — so user searches match auto-generated tags. Additionally, the `tags` field must use a tokenized TEXT analyzer (SimpleTokenizer + LowerCaser) rather than raw STRING, or the ingestion pipeline must emit token-level expansions at index time so that hyphenated tags like `"yang-guorui"` are searchable by component tokens `["yang", "guorui"]`.
- R24. When a user search returns zero results, the search engine automatically retries with fuzzy matching (Levenshtein distance 1) on body, file_name, and tags fields. This catches OCR errors in scanned documents where extracted names differ by 1 character from the query. A "Fuzzy search" toggle in the UI allows the user to force fuzzy matching on initial search.

---

## Prompt Strategy

The prompt sent to DeepSeek performs two tasks: document classification and structured entity extraction. It returns a single JSON object with both outputs.

```
You are a document entity extractor and classifier. Given a filename and extracted text, perform TWO tasks:

TASK 1 — Classify the document. Return 3-5 purpose/type tags (lowercase, 1-3 words, hyphen-separated, no punctuation). At least one must describe the document type (e.g., "research-paper", "tax-return", "legal-contract", "invoice", "lecture-slides", "form").

TASK 2 — Extract structured entities from the text where clearly present:
- persons: Full names of people mentioned. Use the most complete/proper form found (e.g., "Yang Guorui" not "yang").
- organizations: Company names, government agencies, institutions.
- years: 4-digit years referenced as dates or tax years (not page numbers or arbitrary numbers).
- doc_id: Document/form identifier if present (e.g., "1040", "W-2", case number, invoice number).
- amounts: Monetary amounts with currency if detectable (e.g., "$45,000", "EUR 1200").

Rules:
- The FILENAME is the strongest signal for classification — use it first.
- For entities, ONLY extract what is clearly present — do not hallucinate.
- If OCR garbling makes a name unreadable, OMIT it rather than guessing.
- If an entity appears in multiple forms, use the most complete/proper form.
- Prefer existing tags from this vocabulary for classification when they fit: {existing_tags}
- Return ONLY valid JSON in this exact structure, nothing else:

{
  "tags": ["tax-return", "tax", "irs"],
  "entities": {
    "persons": ["Yang Guorui"],
    "organizations": ["Internal Revenue Service"],
    "years": ["2023"],
    "doc_id": ["1040"],
    "amounts": ["$12,450"]
  }
}

Example:
Filename: "2023-tax-return-yang-guorui.pdf"
Text: "Form 1040. Yang Guorui. Tax year 2023. Adjusted gross income $45,230..."
Output: {"tags": ["tax-return", "tax", "irs", "form-1040"], "entities": {"persons": ["Yang Guorui"], "organizations": ["IRS"], "years": ["2023"], "doc_id": ["1040"], "amounts": ["$45,230"]}}

Filename: {filename}
Text: {text}
Existing tags: {existing_tags}
Output:
```

Key design elements:
- **Two tasks, one call**: Classification and extraction in a single API request to avoid doubling cost.
- **Structured JSON**: `tags` (flat topic array) + `entities` (typed categories) — prevents person names from polluting topic tags and enables distinct UI rendering.
- **Filename first**: The prompt front-loads the filename as the primary signal for classification.
- **Explicit entity types**: `persons`, `organizations`, `years`, `doc_id`, `amounts` — each stored and rendered distinctly.
- **Anti-hallucination guard**: "If OCR garbling makes a name unreadable, OMIT it rather than guessing" — prevents garbage entity tags.
- **Vocabulary anchoring**: Existing user tags are injected so the model maps to the user's conceptual space.

---

## Name Normalization Pipeline (R22 Detail)

After the model returns entity tags, a Rust post-processing step normalizes person names before storage:

1. **Lowercase**: `"Yang Guorui"` → `"yang guorui"`
2. **Strip diacritics**: `"yáng guōruì"` → `"yang guorui"` (Unicode NFD decomposition → filter combining marks)
3. **Strip internal spaces** (CJK names): `"yang guo rui"` → `"yangguorui"`
4. **Generate order variants**: `"yang guorui"` → also produce `"guorui yang"`
5. **Store all variants**: All normalized forms are written to the Tantivy `tags` field as separate TEXT tokens. The canonical form (`"Yang Guorui"`) is displayed in the UI.

**Example pipeline:**

| Step | Output |
|------|--------|
| Model returns | `"Yang Guorui"` |
| Lowercase | `"yang guorui"` |
| Strip diacritics | `"yang guorui"` (no change, already ASCII) |
| Strip internal spaces | `"yangguorui"` |
| Order variant | `"guorui yang"` |
| Index tokens | `["yang", "guorui", "yangguorui", "guorui", "yang"]` (TEXT tokenizer splits on spaces + hyphens) |
| UI display | `"Yang Guorui"` |

When the user searches "yang guorui", the query tokenizes to `["yang", "guorui"]` and matches any document containing both tokens in any tag — regardless of whether the model returned `"Yang Guorui"`, `"guorui yang"`, or `"YANG GUORUI"`.

---

## Acceptance Examples

- AE1. **Covers R3, R4, R21, R23.** Given a folder with a PDF named `2023-tax-return-yang-guorui.pdf`, when auto-tagging runs, the model returns `{"tags": ["tax-return", "tax", "irs"], "entities": {"persons": ["Yang Guorui"], "organizations": ["IRS"], "years": ["2023"], "doc_id": ["1040"]}}`. The name normalization pipeline generates variants `["yang", "guorui", "yangguorui", "guorui", "yang"]` and writes them to Tantivy. Searching "yang guorui tax 2023" returns this document as a top result.
- AE2. **Covers R5, R9.** Given a folder containing 50 text PDFs, 2 image-only PDFs, and 1 password-protected PDF, when auto-tagging runs, 50 documents are tagged, 3 are skipped. The UI reports "3 of 53 skipped" with per-file reasons on hover.
- AE3. **Covers R8, R12.** Given a document with auto-entity-tag `person: "Yang Guorui"` and auto-topic-tag `"tax"`, when the user dismisses `"tax"` and manually adds a tag `"yang-guorui"`, the person entity and the manual tag coexist. The auto topic tag `"tax"` is gone and stays gone after app restart.
- AE4. **Covers R22, R23.** Given two PDFs: one tagged with `person: "Yang Guorui"` and another tagged with `person: "guorui yang"` (different name order in the source), when the user searches "yang guorui", both documents appear in results because the normalization pipeline stores the same token set for both variants.
- AE5. **Covers R11, R13.** When hovering over an auto-tag with sparkle icon and dashed border, a tooltip shows the document filename and 2–3 highlighted text snippets that contributed to the tag. Entity tags show type-specific icons: person silhouette for names, calendar for years, building for organizations.
- AE6. **Covers R7.** Given the DeepSeek API returns a 500 error on the first attempt, the AutoTagger retries after 1s. If the retry succeeds, the document is tagged normally. If all 3 retries fail, the document is marked `failed` with the stored error. The next document in the queue is processed without delay.
- AE7. **Covers R24.** Given a scanned PDF where OCR garbled "Yang Guorui" as "Yang Guorul" and the AI extracted `person: "Yang Guorul"`, when the user searches "yang guorui" and exact match returns 0 results, the auto-retry with fuzzy distance=1 matches "Guorul" ↔ "Guorui" (1 character difference) and returns the document.

---

## Success Criteria

- A user searching `yang guorui tax 2023` finds all documents related to Yang Guorui's 2023 taxes, regardless of whether the PDF contains "Yang Guorui", "Guorui Yang", "YANG GUORUI", or minor OCR variants.
- Entity tags (person, organization, year) are visually distinct from topic tags, with type-specific icons. Users can scan the tag panel and immediately see who a document is about.
- A planning agent can read this document and produce an implementation plan without inventing product behavior, name normalization logic, or Tantivy schema changes.
- Cost is transparent: token usage is logged per document. A 1000-document library costs roughly $0.50–$1.50 in DeepSeek API fees (flash model, ~$0.14/1M input, ~$0.28/1M output).
- The API key is never logged, stored in config files, or committed to version control.

---

## Scope Boundaries

- No OCR for image-only or scanned PDFs. Documents without extractable text are skipped. Name normalization handles OCR garbling in documents that DO have a text layer.
- No "Re-analyze" or on-demand re-tagging. Auto-tagging runs once at import time. Re-analysis is v1.1.
- No 7-day grace period auto-accept. Users must explicitly accept or dismiss suggestions. Auto-accept logic is v1.1.
- No per-folder auto-tagging configuration overrides. v1 uses global settings from `auto_tag.json`.
- No local model support in v1 (Ollama, llama.cpp). DeepSeek API is the only shipping provider.
- No multi-document API batching. Each document is a separate API request. Batching is v1.1.
- No structured query syntax (`person:yang AND year:2023`). Entity-field-aware query syntax is v1.1 — v1 uses token matching against the flat `tags` TEXT field.
- No CJK tokenizer (cang-jie / jieba-rs) for Chinese body text in v1. The `SimpleTokenizer` on body text does not split Chinese characters into words, but AI-generated tags are in English, and the filename + tags fields cover the dominant search patterns. CJK tokenization is v1.1.

---

## Key Decisions

- **Structured entity extraction over flat tags**: Person names, years, and document types live in separate `entities` objects in the AI response, not in a flat tag array. This prevents "yang-guorui" from polluting the topic tag space and enables type-specific UI rendering and index weighting.
- **Name normalization in post-processing, not in the prompt**: The model extracts the canonical name form. A Rust pipeline normalizes (lowercase, strip diacritics, CJK space stripping, order variants) and writes all variants to Tantivy. The model is unreliable at systematic transformation; Rust is deterministic.
- **Index-time expansion over query-time expansion**: Name variants are pre-computed and stored in Tantivy at index time. The user query is used as-is (split on whitespace → TermQuery tokens). This avoids query rewriting complexity and keeps search latency predictable.
- **Structured JSON output from AI**: The model returns `{"tags": [...], "entities": {"persons": [...], ...}}` — not a flat string array. The JSON schema is validated in Rust before storage. Malformed responses trigger a retry.
- **DeepSeek flash over local TF-IDF or local models**: Entity extraction requires semantic understanding (distinguishing a person name from a company name, inferring that "1040" is a tax form ID). TF-IDF cannot do this. The DeepSeek flash model does it at ~$0.0005 per document.
- **Content hash includes filename**: The BLAKE3 hash covers `filename + extracted_text`. Renaming a file (e.g., correcting "yang-guorui" to "Yang Guorui") changes its purpose signal — re-tagging is warranted.
- **Bounded channel backpressure**: A 100-slot channel blocks the Indexer when the AutoTagger falls behind. API calls have 1–3s latency; backpressure prevents unbounded queue growth.
- **API key via environment variable**: `DEEPSEEK_API_KEY` is read at startup. The config file references the variable name, not the value. Never in logs, config files, or version control.
- **Manual always wins**: User-created tags absorb auto-tags with the same normalized name; dismissed auto-tags never regenerate. The AI is a guest.
- **`tags` field changed from STRING to TEXT**: The existing `STRING` (exact match) `tags` field cannot match component tokens of hyphenated tags. The field will be changed to TEXT with SimpleTokenizer + LowerCaser. This requires a one-time re-index on upgrade. Decision: Option A (simpler schema) chosen over Option B (parallel field, non-breaking).

---

## Dependencies / Assumptions

- **DeepSeek API availability**: v1 assumes `api.deepseek.com` is reachable. No offline fallback — if the API is down, documents are marked `failed` and the user is notified.
- **DeepSeek flash model pricing**: ~$0.14/1M input tokens, ~$0.28/1M output tokens. With structured output (~110 tokens/doc), 1000 documents cost roughly $0.50 input + $0.03 output. Pricing may change.
- **DeepSeek does not store document text**: Per DeepSeek's API policy. The user is informed of data transmission in the opt-in disclosure (R0).
- **Tantivy `tags` field**: Will be changed from `STRING` (exact match) to `TEXT` with SimpleTokenizer + LowerCaser. This is a breaking schema change that requires a one-time re-index on upgrade. Tantivy fields cannot be removed once added, but they can be modified by re-creating the index.
- **Tantivy schema stability**: Adding the `tags` field to the main query loop and potentially changing its type are the only Tantivy schema changes required for v1. No new fields are needed for entity types in v1 — they are stored as tagged tokens within the flat field.
- **Existing tag vocabulary** is queryable from SQLite in the AutoTagger thread using a separate read connection (WAL mode supports concurrent readers).
- **Schema migration** from the current SQLite schema to one including `auto_tag_status` is handled on app upgrade without data loss.
- **pdf_oxide** reliably returns character counts. If it underreports, some image-only PDFs may pass the 50-char threshold and produce garbage tags (which the model may still salvage from the filename alone).
- **User has a DeepSeek account and API key**: v1 does not provide in-app account creation. The user obtains a key from `platform.deepseek.com` and sets `DEEPSEEK_API_KEY`.

---

## Outstanding Questions

### Resolve Before Planning

_None. All architectural decisions resolved._

### Deferred to Planning

- [Affects R3][Needs research] What is the exact DeepSeek flash model ID for the API? `deepseek-chat` points to the standard model; the flash variant may require `deepseek-flash` or a different model name. Confirm via DeepSeek API docs.
- [Affects R4][Needs research] What temperature and max_tokens settings produce the most consistent structured JSON output from DeepSeek? The prompt requires valid JSON with a specific schema — low temperature (0.1–0.3) is expected. Test with 20–30 diverse PDFs (Chinese + English, scanned + native) to tune.
- [Affects R22][Technical] Does the Unicode NFD + strip combining marks approach handle all CJK diacritic/tone mark cases? Chinese pinyin tone marks (é, ü, ǎ) and their OCR variants need verification against real scanned documents.
- [Affects R22][Technical] How should name-order variant generation handle names with more than 2 components? "Yang Guo Rui" → splitting into 3 components produces 6 permutations. Should all be stored, or just given+surname and surname+given?
- [Affects R16][Technical] Where does the AutoTagger thread live in the startup sequence? Does it own a Tantivy `IndexWriter` for updating tags, or does it send updates through the Indexer thread via a channel?
- [Affects R11][Technical] How does the tag panel distinguish entity tags (person, org, year) from topic tags in the egui widget tree? A tag type enum + icon mapping is required.
- [Affects R14][Technical] Progress events need a channel from the AutoTagger thread to the UI thread. Should this reuse the existing `progress` channel or use a dedicated one?
- [Affects R18][Technical] Should `auto_tag.json` support per-provider configuration sections, or is a flat structure sufficient for v1 with only one provider?
- [Affects R10][Technical] How to securely read `DEEPSEEK_API_KEY` from the environment variable in a Windows desktop app context? `std::env::var` is straightforward but consider whether the key should be re-read on each request or cached at startup.
- [Affects R24][Needs research] What is the performance impact of `FuzzyTermQuery` with distance=1 on a 10,000-document Tantivy index? Testing is needed to confirm acceptable latency for the auto-retry path.
- [Affects R23][Technical] The `search_with_reader` function currently loops over `body` and `file_name` fields for each term. Adding `tags` as a third field per term increases query clauses by 50%. Does this impact Tantivy query performance measurably for large indexes?
