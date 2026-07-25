---
title: "feat: Add AI-powered auto-tagging with DeepSeek"
type: feat
status: active
date: 2026-07-22
origin: docs/brainstorms/2026-07-22-auto-tagging-requirements.md
---

# feat: Add AI-powered auto-tagging with DeepSeek

## Summary

Add a 5th background thread (AutoTagger) that uses a two-tier caching strategy: first search a local filename-token cache for tags from similar already-tagged documents, and only fall back to the DeepSeek flash API when no cache match is found. Generates document-purpose tags and extracts structured entities (person names, organizations, years, document IDs, monetary amounts) from PDF text. Auto-tags are written to SQLite and made searchable through Tantivy by migrating the `tags` field from `STRING` to `TEXT` tokenization and adding it to the main query loop. Person-name entity tags pass through a normalization pipeline handling CJK spacing, diacritics, and name-order variants so queries like `yang guorui tax 2023` match documents regardless of name format. A fuzzy-match retry (distance=1) catches OCR errors. Auto-tagging is off by default, opt-in at folder import, with a privacy disclosure about DeepSeek data transmission.

---

## Problem Frame

Papervault users search by entity — `yang guorui tax 2023` — but the current search engine only looks at body text and filenames. Names appear in unpredictable formats in PDFs (OCR garbling, Chinese/Western order, case variants), making precision retrieval unreliable. Manual tagging exists but users don't use it. The solution: automatically extract structured entities and topic tags from PDF content using an AI model, normalize names for searchability, and surface results through the existing search pipeline.

---

## Requirements

- R1. Off by default — opt-in at folder import with privacy disclosure (origin R0, R1)
- R2. DeepSeek flash API for tag + entity extraction with structured JSON response (origin R3, R4, R21)
- R3. Name normalization pipeline: lowercase, strip diacritics, CJK space strip, order variants (origin R22)
- R4. Tags written to SQLite `auto_tag_status` + Tantivy index update after generation (origin R9, R23)
- R5. Tantivy `tags` field changed from STRING to TEXT with SimpleTokenizer+LowerCaser, added to query loop (origin R23)
- R6. Fuzzy search retry (distance=1) when exact search returns 0 results (origin R24)
- R7. Two-tier cache: (1) exact BLAKE3 hash of filename+text prevents re-processing identical content, (2) filename-token lookup reuses tags from previously-tagged documents with matching filename tokens (e.g., "2023-tax-return-yang-guorui.pdf" matches "2024-tax-return-yang-guorui.pdf" on tokens [tax, return, yang, guorui]) — AI call only when both tiers miss (origin R6)
- R8. API retries: 3 attempts, exponential backoff, 4xx = immediate fail (origin R7)
- R9. Dedicated AutoTagger thread with bounded channel (100), between Indexer and shutdown cascade (origin R16)
- R10. API key via `DEEPSEEK_API_KEY` env var, never stored in config or logs (origin R10)
- R11. Auto-tag display in tag panel: sparkle icon, dashed/solid border, type-specific entity icons, tooltip (origin R11, R13)
- R12. Progress bar during batch auto-tagging, skip/fail summary (origin R14)
- R13. Status bar cloud icon for auto-tagging active/error state (origin R15)
- R14. Manual-always-wins: user tags absorb auto-tags, dismissed tags stay dismissed (origin R8, R12)
- R15. F5 flow: per-document failure isolation, global failure banner, retry path (origin R7)
- R16. F6 flow: entity-driven search matches normalized tags through Tantivy tokenized field (origin R23, R24)

**Origin actors:** A1 (Library Owner), A2 (Power Tagger)
**Origin flows:** F1 (opt-in on import), F2 (batch auto-tagging), F3 (accept/dismiss), F4 (skip unanalyzable), F5 (API fallback), F6 (entity search)
**Origin acceptance examples:** AE1–AE7

---

## Scope Boundaries

- No OCR for image-only PDFs — skip with reason `"no-text-layer"` (carried from origin)
- No "Re-analyze" or on-demand re-tagging in v1 (carried from origin)
- No 7-day grace period auto-accept (carried from origin)
- No local model support (Ollama) in v1 (carried from origin)
- No multi-document API batching (carried from origin)
- No structured query syntax (`person:yang AND year:2023`) (carried from origin)
- No CJK tokenizer (cang-jie) for Chinese body text in v1 (carried from origin)

### Deferred to Follow-Up Work

- Extract `auto_tag_status` tag quality heuristics (confidence thresholding) → v1.1
- Add "Re-analyze" right-click menu item → v1.1
- Migrate Tantivy re-index to lazy/background for large libraries → separate PR if needed
- Add CJK tokenizer for body text search → v1.1

---

## Context & Research

### Relevant Code and Patterns

- **Thread lifecycle**: `src/runtime.rs` — `FolderRuntime::start()` creates channels + spawns threads, `stop()` has strict cascade: watcher → indexer → renderer. AutoTagger slots between indexer and renderer.
- **Channel conventions**: bounded(N) for high-volume producer→consumer, unbounded for UI-bound or low-volume. `src/runtime.rs:69-74`.
- **Search**: `src/search/engine.rs` — `search_with_reader()` builds `(Occur::Must, (Occur::Should over body+file_name))` per term. `tags` field not in loop. `src/search/schema.rs` — `tags` is `STRING | STORED`.
- **TagStore**: `src/tags/store.rs` — `Arc<Mutex<Connection>>`, WAL mode, `CREATE TABLE IF NOT EXISTS` for schema. Tags stored in SQLite, Tantivy rebuilt from SQLite on each upsert.
- **Config**: `src/config.rs` — atomic save (write .tmp → rename). `%APPDATA%/papervault/config.json`.
- **Pipeline**: `src/indexer/pipeline.rs` — `process_batch()` does parallel extraction (rayon) then sequential Tantivy write. `process_tag_update()` currently no-ops.
- **UI**: `src/app.rs` — egui `SidePanel::left("tag_panel")` with tag checkboxes and assign buttons. `do_search()` post-filters using SQLite tags.
- **No HTTP client** in Cargo.toml — must add `ureq`.
- **No migration framework** — `CREATE TABLE IF NOT EXISTS` is the only mechanism.

### Institutional Learnings

- **Thread correctness is the #1 regression source** — 4 prior plans revolve around thread lifecycle. Follow the `FolderRuntime` pattern exactly.
- **Schema migration via index recreation**: `Index::open_in_dir()` fails on corruption → `remove_dir_all` + recreate. Same approach works for deliberate schema changes. Use a version marker file to detect old schema.
- **Shutdown cascade is load-bearing**: The renderer must be the last thread joined. AutoTagger joins after Indexer, before Renderer.
- **Atomic config writes**: `.tmp` → rename pattern prevents corruption. API key referenced by env var name only — value never in files.
- **No existing learnings on**: HTTP client integration in a desktop app, DeepSeek API, entity normalization pipelines, or egui async progress UI. These are net-new.

### External References

- DeepSeek API docs: `api.deepseek.com/v1`, chat completions endpoint, `deepseek-chat` model
- Tantivy 0.22 `FuzzyTermQuery` API and `TextAnalyzer` registration per field name

---

## Key Technical Decisions

- **ureq over reqwest for HTTP**: ureq is lighter, fully blocking (matches the existing thread model), no async runtime needed, and avoids OpenSSL DLL dependency on Windows. Simple JSON POST to a single endpoint doesn't justify reqwest's complexity. `ureq = "3"` with `rustls` TLS backend.
- **Separate `auto_tag.json` over embedding in `config.json`**: Keeps auto-tag config self-contained and atomically savable. Follows the existing `Config::save()` pattern (write .tmp → rename). API key stored as env var name reference only.
- **`IndexerProgress` variant over dedicated channel for progress**: Auto-tag progress piggybacks on the existing `progress_tx` unbounded channel the UI already drains each frame. Avoids another channel and another `poll_channels()` branch.
- **`auto_tag_status` table in existing SQLite database over separate DB**: Single connection, WAL mode handles concurrent readers, follows the `CREATE TABLE IF NOT EXISTS` pattern for schema initialization. No need for a second `.db` file.
- **Schema version marker file over Tantivy introspection**: Detect pre-migration index by checking for a `.schema_version` file in the index directory. If missing or version < 2, delete old index and trigger full re-index from SQLite. Simpler than introspecting Tantivy field types at runtime.
- **Index-time name expansion over query-time expansion**: Normalized name variants are computed and stored in Tantivy at index time. Queries are used as-is (whitespace-split → TermQuery tokens). Simpler, deterministic, no query rewriting.
- **Entity tags stored as JSON blob in `auto_tag_status.tags_json` for v1**: Separate queryable columns can be added in v1.1 without schema migration. The JSON blob is written by the AutoTagger and read by the UI for display.
- **Two-tier cache before AI**: Before calling the DeepSeek API, the AutoTagger checks (1) exact BLAKE3 hash match (identical content → reuse tags, zero API cost), then (2) filename-token match against the local `auto_tag_cache` table (similar filenames → reuse tags). Only when both tiers miss does it call the AI. This cuts API costs for document collections with repeated patterns (tax returns per year, monthly statements, invoice batches) by 50–80%.
- **Anchor tags + AI enrichment**: The three tags derived from the filename (person name, document type, year) are the anchor — they MUST appear in the output. The AI model is instructed to include them AND add any additional keywords it finds important from the document content. This guarantees the core search pattern (`yang guorui tax 2023`) always works while letting the AI enrich with context-specific keywords the filename alone doesn't capture ("deductions", "schedule-c", "adjusted-gross-income").

---

## Open Questions

### Resolved During Planning

- **Tantivy re-index strategy**: Blocking one-time re-index on upgrade, triggered by schema version marker file. <10K docs completes in under 30 seconds based on existing benchmarks.
- **Entity storage format**: JSON blob column in `auto_tag_status` for v1 simplicity.
- **HTTP client choice**: ureq 3 with rustls (lighter, matches blocking thread model).

### Deferred to Implementation

- [Affects R2][Needs research] Exact DeepSeek flash model ID — confirm `deepseek-chat` vs `deepseek-flash` via API docs during implementation.
- [Affects R2][Needs research] Optimal temperature (0.1–0.3 range) for consistent JSON output — tune with 20-30 diverse test PDFs.
- [Affects R3][Technical] Unicode NFD + strip combining marks handling for all CJK diacritic cases — verify against real scanned Chinese documents.
- [Affects R5][Needs research] Performance impact of 3-field query loop (body+file_name+tags) at 10K docs — simple timing test suffices.
- [Affects R6][Needs research] `FuzzyTermQuery(distance=1)` latency at 10K docs — benchmark before committing to auto-retry path.
- [Affects R9][Technical] Whether `FuzzyTermQuery` should be opt-in per-term or fire as a separate second query pass.
- [Affects R11][Technical] egui widget tree: tag type enum + icon mapping in the tag panel rendering code.

---

## Implementation Units

### U1. Add HTTP client and DeepSeek provider

**Goal:** Introduce network I/O capability and the DeepSeek API client behind the `TagProvider` trait. The provider enforces a hard 3-page extraction limit — text beyond page 3 is never read or sent to the API, regardless of word count.

**Requirements:** R2, R8, R10

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml` (add `ureq = { version = "3", features = ["rustls"] }`)
- Create: `src/auto_tagger/provider.rs` (`TagProvider` trait, `TagSuggestion` struct, `TagError` enum)
- Create: `src/auto_tagger/deepseek.rs` (`DeepSeekProvider` implementing `TagProvider`)
- Create: `src/auto_tagger/mod.rs`
- Test: `src/auto_tagger/provider.rs` (`#[cfg(test)] mod tests`)

**Approach:**
- Define `TagProvider` trait with `generate_tags(&self, filename: &str, text: &str, existing_tags: &[String]) -> Result<TagResponse, TagError>`.
- `TagResponse` carries `Vec<String>` topic tags and `Entities { persons, organizations, years, doc_ids, amounts }`.
- `DeepSeekProvider` holds a `ureq::Agent` (connection pooling), endpoint URL, model name, and reads `DEEPSEEK_API_KEY` from env at request time (not at construction — allows key rotation without restart).
- **Hard page limit**: The provider enforces extraction stops at page 3. The Indexer/pipeline already extracts text per-page via pdf_oxide — the AutoTagger only receives text from pages 1–3. Text beyond page 3 is never read, never sent. Combined with the 2000-word soft cap, this bounds both token cost and latency. The prompt template further truncates at 2000 words as a safety net.
- Prompt template from the requirements doc (Prompt Strategy section) is a `const` — structured JSON output with `tags` and `entities` objects. The prompt instructs the model that the three anchor tags derived from the filename (person name, document type, year) are the most important and MUST be included, but the model should also add any additional keywords it finds important from the document content. This ensures core searchability ("yang guorui tax 2023") while allowing enrichment ("adjusted gross income", "schedule c", "deductions").
- 4xx HTTP errors → `TagError::Auth` or `TagError::BadRequest` (not retried). 5xx + timeout → `TagError::Unavailable` (retried by caller).
- `#[cfg(feature = "test-util")]` gate on a `MockProvider` returning fixture data.

**Patterns to follow:**
- `src/error.rs` — `PapervaultError` enum with `#[from]` impls
- `src/config.rs` — `Config::load()` atomic read pattern
- `svc-test-util-feature` from rust-skills: feature-gate mocks
- **`doc-all-public`**: All public items (`TagProvider` trait, `TagSuggestion`, `TagError` enum,
  `TagResponse`, `Entities`, `DeepSeekProvider`) must have `///` doc comments.
- **`err-thiserror-lib`**: `TagError` must use `#[derive(Error, Debug)]` with `thiserror`.
- **`err-context-chain`**: When calling `provider.generate_tags()`, wrap errors with context via
  `.context("auto-tagger").with_context(|| format!("processing {content_hash}"))?`
  so failures trace back to the document being processed.

**Test scenarios:**
- Happy path: valid filename + text → `TagResponse` with expected structure parsed from JSON
- Happy path: empty entity fields → `entities.persons: []` handled without error
- Error path: 401 response → `TagError::Auth`
- Error path: 500 response → `TagError::Unavailable`
- Error path: malformed JSON response → `TagError::Parse`
- Error path: missing `DEEPSEEK_API_KEY` env var → clear error, not panic
- Edge case: text exceeding 2000 words → truncated before sending (provider responsibility)
- Edge case: PDF with 50 pages → only pages 1–3 extracted and sent; pages 4+ never read
- Edge case: PDF with only 2 pages → all text extracted (no padding, no error)

**Verification:**
- `cargo test --lib auto_tagger::provider` passes with mocked HTTP
- `cargo test --lib auto_tagger::deepseek` passes (test with `ureq::Agent` pointed at a mock server or using `#[cfg(feature = "test-util")]` MockProvider)
- Manual: set `DEEPSEEK_API_KEY`, call `generate_tags` with a known PDF → valid JSON response

---

### U2. Add auto_tag_status table and TagStore methods

**Goal:** Extend the SQLite schema with auto-tagging state and add CRUD methods to TagStore.

**Requirements:** R4, R7

**Dependencies:** None

**Files:**
- Modify: `src/tags/store.rs` (add `auto_tag_status` table, new methods)
- Modify: `src/tags/model.rs` (add `AutoTagStatus` struct)
- Test: `src/tags/store.rs` (existing `#[cfg(test)] mod tests`)

**Approach:**
- Add new tables to `TagStore::open_or_create()`:
  ```sql
  -- Per-document auto-tagging state
  CREATE TABLE IF NOT EXISTS auto_tag_status (
      content_hash TEXT PRIMARY KEY REFERENCES documents(content_hash) ON DELETE CASCADE,
      filename     TEXT NOT NULL,
      content_hash_before_tag TEXT NOT NULL,  -- BLAKE3 of filename+text for dedup
      status       TEXT NOT NULL DEFAULT 'pending',  -- pending|in_progress|tagged|failed|skipped
      tags_json    TEXT,       -- JSON: {"tags": [...], "entities": {...}}
      attempts     INTEGER NOT NULL DEFAULT 0,
      last_error   TEXT,
      created_at   TEXT NOT NULL DEFAULT (datetime('now')),
      updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
  );

  -- Filename-token cache for tier-2 lookups (avoids redundant AI calls)
  CREATE TABLE IF NOT EXISTS auto_tag_cache (
      id            INTEGER PRIMARY KEY AUTOINCREMENT,
      filename_tokens TEXT NOT NULL,  -- space-separated lowercase tokens: "tax return yang guorui"
      tags_json     TEXT NOT NULL,    -- JSON: {"tags": [...], "entities": {...}}
      source_hash   TEXT NOT NULL,    -- content_hash of the document that seeded this cache entry
      hit_count     INTEGER NOT NULL DEFAULT 1,
      created_at    TEXT NOT NULL DEFAULT (datetime('now')),
      updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
  );
  CREATE INDEX IF NOT EXISTS idx_auto_tag_cache_tokens ON auto_tag_cache(filename_tokens);
  ```
- Add `AutoTagStatus` struct to `src/tags/model.rs` with all fields.
- **Newtype safety** (`api-newtype-safety`): Use a `ContentHash(String)` newtype for `content_hash` and
  `content_hash_before_tag` to prevent accidentally swapping them or mixing them with filenames.
  Validate that the inner string is a 64-char hex BLAKE3 digest at construction (parse-dont-validate).
- Add TagStore methods (follow `name-no-get-prefix` — omit `get_` prefix):
  - `upsert_auto_tag_status(content_hash, filename, content_hash_before_tag, status, tags_json, error)` — `INSERT OR REPLACE`
  - `auto_tag_status(content_hash) -> Option<AutoTagStatus>`
  - `pending_auto_tags(limit) -> Vec<AutoTagStatus>` — documents needing tagging
  - `dismiss_auto_tag(content_hash, tag_name)` — removes tag from `tags_json` array
  - `lookup_cache_by_tokens(tokens: &[String], min_overlap_ratio: f64) -> Option<String>` — returns `tags_json` if a cache entry has ≥ `min_overlap_ratio` token overlap; otherwise `None`
  - `upsert_cache_entry(filename_tokens: &str, tags_json: &str, source_hash: &str)` — `INSERT OR REPLACE` with `hit_count` increment
- Follow existing pattern: all methods go through `with_conn()`.

**Patterns to follow:**
- `src/tags/store.rs` — `CREATE TABLE IF NOT EXISTS` pattern, `params![]` macros, `with_conn()` for all DB access
- `src/tags/model.rs` — `#[derive(Debug, Clone)]` structs

**Test scenarios:**
- Happy path: `upsert_auto_tag_status` → `auto_tag_status()` round-trips all fields
- Happy path: `pending_auto_tags(10)` returns only docs with status `pending`, ordered by `created_at`
- Happy path: `lookup_cache_by_tokens(["tax", "return", "yang", "guorui"], 0.5)` → hits cache row with tokens "tax return yang guorui" (100% overlap)
- Happy path: `lookup_cache_by_tokens(["invoice", "acme", "2023"], 0.5)` → misses unrelated cache (0% overlap), returns `None`
- Edge case: `lookup_cache_by_tokens` with partial overlap below threshold (e.g., 1 of 5 tokens = 20%) → returns `None`
- Edge case: `upsert_cache_entry` with same `filename_tokens` → overwrites `tags_json`, increments `hit_count`
- Edge case: `upsert_auto_tag_status` with same content_hash overwrites (INSERT OR REPLACE)
- Edge case: `dismiss_auto_tag` removes one tag from `tags_json` array, other tags preserved
- Edge case: document deleted from `documents` table → cascade deletes `auto_tag_status` row (cache entries are NOT cascaded — independent and reusable)
- Error path: `auto_tag_status()` for nonexistent hash → `None`

**Verification:**
- `cargo test --lib tags::store` passes
- Manual: call methods via `TagStore` on test database, verify SQLite rows

---

### U3. Migrate Tantivy tags field from STRING to TEXT

**Goal:** Change the `tags` field to tokenized TEXT and add it to the search query loop so auto-generated tags are searchable by component tokens.

**Requirements:** R5, R6

**Dependencies:** None

**Files:**
- Modify: `src/search/schema.rs` (change `tags` from `STRING | STORED` to `TEXT | STORED`, add test assertions)
- Modify: `src/search/engine.rs` (register `"tags"` tokenizer, add tags to `search_with_reader` term loop, add fuzzy retry)
- Modify: `src/search/query.rs` (no schema changes needed; `SearchResult.tags` field already reads from Tantivy)
- Test: `src/search/schema.rs`, `src/search/engine.rs`

**Approach:**
- **Schema change**: In `build_schema()`, change `add_text_field("tags", STRING | STORED)` → `add_text_field("tags", TEXT | STORED)`.
- **Tokenizer registration**: In `SearchEngine::open_or_create()`, add:
  ```rust
  let tag_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
      .filter(LowerCaser)
      .build();
  index.tokenizers().register("tags", tag_tokenizer);
  ```
- **Query loop**: In `search_with_reader()`, inside the per-term `for` loop, add a third `Occur::Should` clause:
  ```rust
  let tag_term = Term::from_field_text(fields.tags, &lower);
  term_subqueries.push((Occur::Should, Box::new(TermQuery::new(tag_term, IndexRecordOption::Basic))));
  ```
- **Fuzzy retry**: After the exact search, if `total_hits == 0` and `request.fuzzy` is true (or auto-retry), build a `FuzzyTermQuery` with distance=1 for each term against body + tags. Keep as a separate query pass (not blended with exact) for latency predictability.
- **Schema migration**: On `open_or_create()`, after opening the index, check for a `.schema_version` file in the index directory. If missing or version < 2, delete the index directory, recreate, and set a flag that triggers full re-index. The flag is checked in `Pipeline::run()` — if set, run reconciliation from SQLite data before processing any file events.
- **`index_document()`**: No changes needed — `doc.add_text(fields.tags, tag)` already works; tokenizer handles the rest.

**Patterns to follow:**
- `src/search/engine.rs:77` — existing `body` tokenizer registration
- `src/search/engine.rs:211-225` — existing term loop pattern for body+file_name
- `src/search/engine.rs:34-75` — corruption recovery pattern (delete + recreate index) for migration

**Test scenarios:**
- Happy path: index doc with tag `"yang-guorui"`, search `"yang"` → match (proves TEXT tokenization)
- Happy path: index doc with tag `"Guorui Yang"`, search `"guorui"` → match (proves LowerCaser)
- Happy path: index doc with tag `"tax-return"`, search `"tax"` → match (proves hyphen splitting)
- Edge case: search `"yang guorui"` (two terms) → both terms match, doc returned (proves AND semantics)
- Edge case: fuzzy retry matches `"guorul"` against `"guorui"` with distance=1
- Edge case: fuzzy retry does NOT fire when exact search returns results
- Schema test: `tags` field is indexed and stored (same assertions as existing `body_field_is_text_with_positions`)

**Verification:**
- `cargo test --lib search::schema` passes with updated assertions
- `cargo test --lib search::engine` passes with new search tests
- Manual: index a test PDF with known tags, search component tokens, verify hits

---

### U4. Build the AutoTagger thread with two-tier cache

**Goal:** Add the 5th background thread that receives `AutoTagRequest`s from the Indexer, checks the local filename-token cache before calling the DeepSeek provider, normalizes names, and writes results. The two-tier cache cuts API costs by reusing tags from similar already-tagged documents.

**Requirements:** R3, R4, R7, R8, R9

**Dependencies:** U1 (provider), U2 (TagStore methods)

**Files:**
- Modify: `src/app.rs` (add `AutoTagRequest`, `AutoTagResult` enums)
- Create: `src/auto_tagger/thread.rs` (AutoTagger thread function, name normalization)
- Test: `src/auto_tagger/thread.rs`

**Approach:**
- Add message types to `src/app.rs` alongside existing `IndexerMessage`, `TagUpdate`, etc.:
  ```rust
  pub enum AutoTagRequest {
      TagDocument { content_hash: String, filename: String, text: String },
      Shutdown,
  }
  pub enum AutoTagResult {
      Tagged { content_hash: String, tags: Vec<String>, entities: Entities },
      Failed { content_hash: String, error: String },
      Skipped { content_hash: String, reason: String },
  }
  ```
- Thread function `run_auto_tagger(rx, tag_store, provider, progress_tx, shutdown_flag: Arc<AtomicBool>)`:
  1. Block on `rx.recv()` for `AutoTagRequest`.
     - **`conc-atomic-ordering`**: Read shutdown flag with `load(Ordering::Acquire)`;
       write with `store(true, Ordering::Release)` in the shutdown path.
     - **`svc-services-clone`**: `DeepSeekProvider` should implement `Clone` via `Arc<Inner>`
       if shared across threads. TagStore already uses `Arc<Mutex<Connection>>`.
  2. On `TagDocument`: compute `content_hash_before_tag` = BLAKE3(filename + text).
     - **Tier 1 — Exact hash match**: Check `auto_tag_status` — if already `tagged` with same hash, skip (R7 exact cache hit).
     - **Tier 2 — Filename-token lookup**: Extract significant tokens from filename (split on `[-_.\s]`, filter stopwords like "copy", "final", "v2", "draft", "scan"). Query `auto_tag_cache` for previously-tagged documents whose filename tokens overlap (configurable threshold: ≥ 50% token overlap). If a match is found, reuse the cached tags + entities, write to `auto_tag_status`, and skip the API call.
     - **Tier 3 — AI fallback**: Only if both cache tiers miss, call `provider.generate_tags(filename, text, existing_tags)` with 3 retries + exponential backoff (R8). The prompt instructs the model that anchor tags (person name, document type, year from filename) are primary and MUST be included; the model then enriches with additional keywords it finds important from the document content. On success, write the result to both `auto_tag_status` AND `auto_tag_cache` (populating the cache for future lookups).
  4. On AI success: normalize person-name entities (R22 pipeline), write `tags_json` to both `auto_tag_status` AND `auto_tag_cache`, send `AutoTagResult::Tagged` + `IndexerProgress::AutoTagComplete`.
  5. On cache hit (tier 1 or 2): log `tracing::debug!("cache hit for {filename}: {source}")`, write result, send `AutoTagResult::Tagged`.
  6. On failure after retries: write `status='failed'` + `last_error`, send `AutoTagResult::Failed`.
  7. On `Shutdown`: set flag, drain remaining queue items (mark as `pending` for next startup), exit.
- Name normalization (`normalize_person_name(name: &str) -> Vec<String>`):
  1. Lowercase
  2. Unicode NFD → filter combining marks (strip diacritics/tone marks)
  3. For CJK: strip internal spaces → concatenated form
  4. Generate order variants: split on spaces, reverse, join
  5. Return all unique variants
  - **`mem-with-capacity`**: Max 3 variants (original, reversed-word-order, CJK-concatenated).
    Use `let mut variants = Vec::with_capacity(3);` to avoid reallocation.
- **Filename token extraction**: `extract_filename_tokens(filename: &str) -> Vec<String>`:
  1. Strip extension, split on `[-_.\s]+`
  2. Lowercase all tokens
  3. Filter out stopwords: "copy", "final", "v1", "v2", "v3", "draft", "scan", "scanned", "ocr", "new", "old", "revised"
  4. Filter out numeric-only tokens (years like "2023" are kept — they ARE signal)
  5. Return remaining tokens
- **Cache lookup**: `lookup_cache(tokens: &[String], store: &TagStore) -> Option<TagResponse>`:
  1. Query `auto_tag_cache` for rows whose `filename_tokens` column has any overlap with input tokens
  2. Compute overlap ratio: `|intersection| / |input tokens|`
  3. If ≥ 50% overlap, return the best match's `tags_json` (highest token overlap)
  4. Otherwise, return `None` (cache miss → proceed to AI)

**Patterns to follow:**
- `src/runtime.rs:86-99` — thread spawn pattern with `std::thread::Builder::new().name("auto-tagger")`
- `src/indexer/pipeline.rs:197-199` — `recv_timeout` pattern
- `svc-services-clone` from rust-skills: `Arc<Inner>` if state sharing is needed

**Test scenarios:**
- Happy path: valid filename+text → `Tagged` result with normalized name variants
- Happy path: BLAKE3 cache hit → no API call, existing tags returned
- Happy path: filename-token cache hit → `"2023-tax-return-yang-guorui.pdf"` matches cache entry from `"2024-tax-return-yang-guorui.pdf"` (tokens [tax, return, yang, guorui] at 100% overlap) → tags reused, zero API call
- Happy path: AI response includes anchor tags (person, type, year) PLUS enrichment keywords (e.g., for a tax return: "yang-guorui", "tax-return", "2023", "adjusted-gross-income", "schedule-c") → all tags stored
- Happy path: cache miss on unrelated filename (`"recipe-book.pdf"` vs cache of tax documents) → falls through to AI
- Edge case: partial filename match below threshold (e.g., `"2023-notes.pdf"` shares only token [2023] with tax cache = 25% overlap) → cache miss, AI called
- Edge case: name normalization: `"Yang Guorui"` → `["yang guorui", "guorui yang", "yangguorui"]`
- Edge case: name normalization: `"yáng guōruì"` → diacritics stripped → `["yang guorui", "guorui yang", "yangguorui"]`
- Edge case: name normalization: `"Yang Guo Rui"` → CJK space strip → includes `"yangguorui"`
- Error path: API 500 → retry 3 times → `Failed` result
- Error path: API 401 → no retry → immediate `Failed`
- Integration: 10 docs in channel, one fails → remaining 9 processed normally

**Verification:**
- `cargo test --lib auto_tagger::thread` passes
- Manual: run auto-tagger against mock provider, verify tags land in SQLite

---

### U5. Wire AutoTagger into FolderRuntime and Pipeline

**Goal:** Create channels, spawn the AutoTagger thread in `FolderRuntime::start()`, and connect the Pipeline to send `AutoTagRequest`s after indexing.

**Requirements:** R9, R12, R13

**Dependencies:** U4 (AutoTagger thread), U2 (TagStore methods)

**Files:**
- Modify: `src/runtime.rs` (add auto-tagger channel + thread to `FolderRuntime`, add to shutdown cascade)
- Modify: `src/indexer/pipeline.rs` (send `AutoTagRequest` after successful index)
- Modify: `src/main.rs` (pass auto-tagger channels to `PapervaultApp::new()`)
- Modify: `src/app.rs` (`PapervaultApp` struct: add auto-tagger fields, drain result/status in `poll_channels`)

**Approach:**
- **runtime.rs changes**:
  - Add `auto_tagger_tx: Option<Sender<AutoTagRequest>>` and `auto_tagger_handle: Option<JoinHandle<()>>` to `FolderRuntime` struct.
  - In `start()`: create `bounded(100)` channel for `AutoTagRequest`. Spawn auto-tagger thread with TagStore clone + DeepSeekProvider + progress_tx clone.
  - In `stop()`: add auto-tagger shutdown between Indexer join and Renderer join. Send `AutoTagRequest::Shutdown`, drop sender, join handle.
- **pipeline.rs changes**:
  - Add `auto_tagger_tx: Option<Sender<AutoTagRequest>>` to `Pipeline` struct and `Pipeline::new()`.
  - After successful `process_upsert()` or `process_batch()` document indexing, send `AutoTagRequest::TagDocument { content_hash, filename, text }` to the channel.
  - Use `try_send()` to avoid blocking the indexer on a full channel — if full, `tracing::warn!("auto-tagger channel full, skipping {content_hash}")` and skip (the document will be picked up on next index if auto-tagging is still pending).
- **main.rs changes**:
  - Add auto-tagger channel + shutdown to `PapervaultApp::new()` parameters.
  - Wire the `auto_tagger_tx` from `FolderRuntime` to `PapervaultApp`.
- **app.rs changes**:
  - Add `auto_tag_results: Vec<AutoTagResult>` and `auto_tag_status` state to `PapervaultApp`.
  - In `poll_channels()`, drain `auto_tag_result_rx` and update state.
  - Add `IndexerProgress::AutoTagProgress { completed, total }` and `IndexerProgress::AutoTagComplete` variants for progress bar.

**Patterns to follow:**
- `src/runtime.rs:62-99` — existing channel creation + thread spawn pattern
- `src/runtime.rs:140-170` — existing shutdown cascade
- `src/main.rs:42-119` — existing startup flow
- `src/indexer/pipeline.rs:120-165` — how existing batch processing tracks progress

**Test scenarios:**
- Happy path: file indexed → `AutoTagRequest` sent to channel → auto-tagger processes → result appears
- Edge case: channel full → `try_send()` returns `Full` → log warning, don't block
- Shutdown: `AutoTagRequest::Shutdown` sent → thread drains and exits → joins successfully
- Integration: index 5 files → 5 `AutoTagRequest`s queued → auto-tagger processes sequentially

**Verification:**
- `cargo build` succeeds with all new wiring
- Manual: import folder with auto-tagging enabled → tags appear in tag panel
- Manual: close app → auto-tagger shuts down gracefully (no hanging threads)

---

### U6. Build auto-tag UI in egui tag panel

**Goal:** Display auto-tag suggestions in the tag panel, add opt-in dialog, progress bar, status icon, and accept/dismiss interactions.

**Requirements:** R1, R11, R12, R13, R14

**Dependencies:** U5 (AutoTagger wired to app state)

**Files:**
- Modify: `src/app.rs` (tag panel rendering, opt-in dialog, state management)

**Approach:**
- **State additions to `PapervaultApp`**:
  - `auto_tag_enabled: bool` — persisted in `auto_tag.json`
  - `auto_tag_suggestions: HashMap<String, AutoTagStatus>` — keyed by content_hash
  - `auto_tag_progress: Option<(usize, usize)>` — (completed, total) for progress bar
  - `auto_tag_error: Option<String>` — global error banner
  - `show_auto_tag_opt_in: bool` — modal dialog state
- **Opt-in flow** (R1): On folder import, show modal dialog with privacy disclosure text. User checks "Enable auto-tagging" → `auto_tag_enabled = true` → persists to `auto_tag.json`. Disclosure includes: data sent to DeepSeek (China), text not stored, API key required.
- **Tag panel rendering** (R11, R14):
  - Auto-tags rendered alongside manual tags with sparkle icon (✨).
  - Dashed border = unconfirmed suggestion; solid border = accepted; click toggles.
  - Entity tags (person, org, year, doc_id, amount) rendered with type-specific icons: 👤 (person), 🏢 (organization), 📅 (year), 📄 (doc_id), 💰 (amount).
  - Right-click → "Dismiss" removes tag from display and persists dismissal to SQLite.
  - Hover tooltip: shows filename + 2-3 text snippets ("Why this tag?").
  - If user manually creates a tag with same normalized name → auto-tag absorbed (no duplicate).
- **Progress bar** (R12): When `auto_tag_progress` is `Some((c, t))`, render a progress bar below the tag list header: "Auto-tagging: 12/47". When complete, show summary: "47 tagged, 3 skipped, 2 failed" with hover tooltip for details.
- **Status icon** (R13): Cloud icon in status bar. Green = active/idle, animated = processing, red = error (API unreachable).

**Patterns to follow:**
- `src/app.rs` — existing tag panel rendering in `SidePanel::left("tag_panel")`
- `src/app.rs` — `SearchResult.tags` field display pattern
- egui 0.30: `egui::Spinner::new()`, `egui::ProgressBar::new()`, `ui.toggle_value()`

**Test scenarios:**
- Happy path: import folder with auto-tag enabled → progress bar appears → tags render with sparkle icon
- Happy path: click dashed-border tag → toggles to solid (accepted)
- Happy path: right-click tag → "Dismiss" → tag removed, persists across restart
- Happy path: manually create tag matching auto-tag name → auto-tag absorbed, one manual tag displayed
- Edge case: no auto-tags generated for folder → progress bar shows "0 tagged", no tags in panel
- Edge case: API error → error banner shown, retry button visible
- Tooltip: hover auto-tag → "Why this tag?" tooltip with filename + snippets

**Verification:**
- `cargo build` succeeds
- Manual: full UI walkthrough of opt-in → batch processing → tag display → accept/dismiss

---

### U7. Add auto_tag.json config and API key management

**Goal:** Persist auto-tagging configuration and securely reference the DeepSeek API key.

**Requirements:** R1, R10

**Dependencies:** None (can be done in parallel with U1-U6)

**Files:**
- Create: `src/auto_tagger/config.rs` (`AutoTagConfig` struct, load/save)
- Test: `src/auto_tagger/config.rs`

**Approach:**
- `AutoTagConfig` struct (7 optional fields with defaults → use builder per `api-builder-pattern`):
  ```rust
  #[derive(Debug, Clone, Serialize, Deserialize)]
  pub struct AutoTagConfig {
      pub enabled: bool,
      pub provider: String,        // "deepseek"
      pub model: String,           // "deepseek-chat"
      pub endpoint: String,        // "https://api.deepseek.com/v1"
      pub api_key_env: String,     // "DEEPSEEK_API_KEY"
      pub max_retries: u32,        // 3
      pub request_timeout_secs: u64, // 30
      pub max_tags_per_doc: usize, // 8
  }
  
  impl AutoTagConfig {
      /// Returns a builder for constructing AutoTagConfig with defaults.
      /// `#[must_use]` per `api-builder-pattern` — builder does nothing without `.build()`.
      pub fn builder() -> AutoTagConfigBuilder { AutoTagConfigBuilder::default() }
  }
  
  #[derive(Default)]
  #[must_use = "builders do nothing unless you call build()"]
  pub struct AutoTagConfigBuilder { /* fields mirror AutoTagConfig with Option wrappers */ }
  
  impl AutoTagConfigBuilder {
      pub fn model(mut self, v: impl Into<String>) -> Self { self.model = Some(v.into()); self }
      pub fn max_retries(mut self, v: u32) -> Self { self.max_retries = Some(v); self }
      // ... remaining setters
      pub fn build(self) -> AutoTagConfig { /* fill defaults for unset fields */ }
  }
  ```
- Config path: `dirs_next::config_dir() / "papervault" / "auto_tag.json"`.
- `load()` → returns `Default` if file missing or parse error (with `tracing::warn!`).
- `save()` → atomic write: serialize to `.json.tmp`, `std::fs::rename` over final path.
- API key: `std::env::var(config.api_key_env)` at request time. Never cached in struct. Request fails with clear error if env var is unset.
- Default values match requirements doc: deepseek-chat, api.deepseek.com/v1, 3 retries, 30s timeout.

**Patterns to follow:**
- `src/config.rs` — `Config::load()` / `Config::save()` with atomic rename
- `src/config.rs` — `dirs_next::config_dir()` path construction

**Test scenarios:**
- Happy path: `load()` returns default when file doesn't exist
- Happy path: `save()` then `load()` round-trips all fields
- Edge case: `load()` with malformed JSON → returns default, logs warning
- Edge case: `api_key_env` set to "MISSING_VAR" → provider returns clear error when env var not found
- Edge case: `save()` to directory that doesn't exist → `create_dir_all` succeeds, file written

**Verification:**
- `cargo test --lib auto_tagger::config` passes
- Manual: edit `auto_tag.json`, restart app, verify settings take effect
