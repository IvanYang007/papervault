---
date: 2026-07-22
topic: auto-tagging-test-plan
origin: docs/plans/2026-07-22-001-feat-auto-tagging-deepseek-plan.md
---

# Test Plan: Auto-Tagging with DeepSeek

## Test Strategy

This feature touches four layers: HTTP client (new), SQLite (extended), Tantivy (schema migration), and egui UI (new panel elements). Each layer has different test needs:

| Layer | Strategy | Tools |
|-------|----------|-------|
| DeepSeek provider (U1) | Unit tests with mock provider; integration tests with test fixtures | `#[cfg(test)]` modules, fixture JSON files |
| TagStore + SQLite (U2) | Unit tests against in-memory SQLite | `#[cfg(test)] mod tests`, `tempfile` |
| Tantivy schema + search (U3) | Unit tests with in-memory index; property tests for text tokenization | `#[cfg(test)] mod tests`, `tempfile`, possibly proptest |
| AutoTagger thread (U4) | Unit tests for normalization; integration tests with mock provider + real channels | `#[cfg(test)] mod tests`, `crossbeam` |
| FolderRuntime + Pipeline (U5) | Integration tests for thread lifecycle, channel wiring | `tests/` directory |
| egui UI (U6) | Manual verification; snapshot tests for tag rendering | Manual, `insta` for tag render output |
| Config (U7) | Unit tests for load/save round-trip | `#[cfg(test)] mod tests`, `tempfile` |

---

## Test Rules Applied (from rust-skills)

| Rule | Application |
|------|------------|
| `test-cfg-test-module` | All unit tests in `#[cfg(test)] mod tests` within each source file |
| `test-use-super` | `use super::*;` in each test module (all `#[cfg(test)] mod tests` blocks must include it) |
| `test-integration-dir` | Thread lifecycle + channel wiring tests in `tests/auto_tagger_integration.rs` |
| `test-descriptive-names` | Every test function name describes what is tested and the expected outcome |
| `test-arrange-act-assert` | Tests structured with clear Arrange / Act / Assert sections marked by comments |
| `test-mockall-mocking` | `MockProvider` behind `#[cfg(feature = "test-util")]` for `TagProvider` trait |
| `test-fixture-raii` | `TempDir` for Tantivy index and SQLite database — cleaned up on drop |
| `test-should-panic` | Used where appropriate for expected panics (e.g., invalid states) |
| `test-snapshot-testing` | `insta` snapshots for entity normalization output |
| `test-no-tautological` | Every test verifies real behavior, not restatements of the implementation |
| `test-feature-gated-utils` | `MockProvider` and test helpers gated behind `test-util` feature flag |
| `test-mockable-syscalls` | `TagProvider` trait enables swapping real provider for mock in integration tests |

---

## Test Cases by Implementation Unit

### U1: DeepSeek Provider (`src/auto_tagger/provider.rs`, `src/auto_tagger/deepseek.rs`)

#### Unit Tests (in-file `#[cfg(test)] mod tests`)

**`test_generate_tags_returns_structured_response`**
- Arrange: mock HTTP server returning valid JSON with `tags` and `entities`
- Act: call `provider.generate_tags("test.pdf", "sample text", &[])`
- Assert: `TagResponse.tags` is non-empty, `entities.persons` contains expected names

**`test_empty_entities_returns_empty_arrays`**
- Arrange: mock response where `entities` fields are empty arrays
- Act: `provider.generate_tags(...)`
- Assert: `entities.persons` is `[]`, not null

**`test_401_returns_auth_error_no_retry`**
- Arrange: mock HTTP 401 response
- Act: `provider.generate_tags(...)`
- Assert: returns `Err(TagError::Auth(...))`, no retry attempted

**`test_500_returns_unavailable_error`**
- Arrange: mock HTTP 500 response
- Act: `provider.generate_tags(...)`
- Assert: returns `Err(TagError::Unavailable(...))`

**`test_malformed_json_returns_parse_error`**
- Arrange: mock response with invalid JSON body
- Act: `provider.generate_tags(...)`
- Assert: returns `Err(TagError::Parse(...))`

**`test_missing_api_key_returns_clear_error`**
- Arrange: `DEEPSEEK_API_KEY` env var unset
- Act: `provider.generate_tags(...)`
- Assert: returns `Err(TagError::Auth(...))` with message about env var

**`test_text_longer_than_2000_words_is_truncated`**
- Arrange: text with 2500 words
- Act: `provider.generate_tags("doc.pdf", long_text, &[])`
- Assert: request body contains truncated text, not full 2500 words

**`test_filename_included_in_request_body`**
- Arrange: filename "2023-tax-return-yang-guorui.pdf"
- Act: `provider.generate_tags(filename, text, &[])`
- Assert: JSON request body includes the filename string

#### Integration Tests (`tests/` directory)

**`test_deepseek_provider_real_api_call`** (ignored by default, requires API key)
- Arrange: real `DEEPSEEK_API_KEY` set, real provider
- Act: `provider.generate_tags("test-tax-doc.pdf", "Form 1040 tax return text...", &[])`
- Assert: valid JSON response with tags about tax/document

---

### U2: TagStore Extensions (`src/tags/store.rs`, `src/tags/model.rs`)

#### Unit Tests (in-file `#[cfg(test)] mod tests`)

**`test_auto_tag_status_round_trips`**
- Arrange: create test store, upsert auto_tag_status with all fields
- Act: `get_auto_tag_status(hash)`
- Assert: returned struct matches all input values

**`test_auto_tag_status_overwrites_on_duplicate_hash`**
- Arrange: upsert with status "pending", then upsert same hash with status "tagged"
- Act: `get_auto_tag_status(hash)`
- Assert: status is "tagged" (last write wins)

**`test_get_pending_auto_tags_respects_limit`**
- Arrange: insert 5 pending, 3 tagged, 2 failed
- Act: `get_pending_auto_tags(10)`
- Assert: returns 5 rows, all with status "pending"

**`test_get_pending_auto_tags_returns_empty_when_none_pending`**
- Arrange: all rows have status "tagged" or "failed"
- Act: `get_pending_auto_tags(10)`
- Assert: returns empty vec

**`test_dismiss_auto_tag_removes_one_tag_from_json_array`**
- Arrange: tags_json = `{"tags":["tax","irs","2023"],"entities":{...}}`
- Act: `dismiss_auto_tag(hash, "irs")`
- Assert: tags_json no longer contains "irs"; "tax" and "2023" still present

**`test_dismiss_auto_tag_preserves_entities`**
- Arrange: tags_json with both tags and entities
- Act: `dismiss_auto_tag(hash, "tax")`
- Assert: entities section unchanged

**`test_cache_lookup_full_overlap_returns_tags`**
- Arrange: insert cache entry with tokens "tax return yang guorui" and tags_json
- Act: `lookup_cache_by_tokens(["tax", "return", "yang", "guorui"], 0.5)`
- Assert: returns `Some(tags_json)` (100% overlap ≥ 50%)

**`test_cache_lookup_zero_overlap_returns_none`**
- Arrange: insert cache entry with tokens "tax return yang guorui"
- Act: `lookup_cache_by_tokens(["recipe", "cookbook", "pasta"], 0.5)`
- Assert: returns `None` (0% overlap < 50%)

**`test_cache_lookup_partial_below_threshold_returns_none`**
- Arrange: insert cache entry with tokens "tax return yang guorui"
- Act: `lookup_cache_by_tokens(["tax", "2023", "form", "1040", "irs"], 0.5)` → only "tax" overlaps = 20%
- Assert: returns `None` (20% < 50%)

**`test_cache_upsert_increments_hit_count`**
- Arrange: insert cache entry, then upsert same tokens with different source_hash
- Act: query `hit_count`
- Assert: `hit_count = 2`

**`test_cascade_delete_when_document_deleted`**
- Arrange: upsert document + auto_tag_status
- Act: `delete_document_by_path(path)`
- Assert: `get_auto_tag_status(hash)` returns `None`

**`test_get_auto_tag_status_nonexistent_returns_none`**
- Arrange: query for hash that doesn't exist
- Act: `get_auto_tag_status("no-such-hash")`
- Assert: returns `None`

---

### U3: Tantivy Schema Migration (`src/search/schema.rs`, `src/search/engine.rs`)

#### Unit Tests (in-file `#[cfg(test)] mod tests`)

**`test_tags_field_uses_tokenizer_registered_by_name`**
- Arrange: `build_schema()`, get tags field
- Act: check `field_entry.is_indexed()` and retrieve the tokenizer registered under `"tags"`
- Assert: tokenizer is `SimpleTokenizer` with `LowerCaser` filter (not the default `RawTokenizer` that STRING fields use).
  This distinguishes TEXT from STRING — both are indexed, but only TEXT uses a custom tokenizer.
  Per `test-no-tautological`: the assertion must test a property of the schema (which tokenizer is active),
  not mirror the implementation (`add_text_field("tags", TEXT | STORED)`).

**`test_tags_field_accepts_multiple_values`**
- Arrange: index doc with `doc.add_text(tags, "tax").add_text(tags, "irs")`
- Act: retrieve doc
- Assert: `doc.get_all(tags)` returns 2 values

**`test_search_matches_tag_by_component_token`**
- Arrange: index doc with tag "yang-guorui", commit, reload
- Act: search for "yang"
- Assert: total_hits = 1

**`test_search_matches_tag_case_insensitive`**
- Arrange: index doc with tag "Yang Guorui", commit, reload
- Act: search for "guorui"
- Assert: total_hits = 1

**`test_search_matches_normalized_name_variant`**
- Arrange: index doc with tags "yang-guorui" + "guorui-yang", commit, reload
- Act: search for "guorui yang"
- Assert: total_hits = 1 (both terms match the same doc via OR within tag field)

**`test_fuzzy_retry_when_exact_returns_zero`**
- Arrange: index doc with tag "yang-guorui", commit, reload
- Act: search for "yangg" (misspelled — should fuzzy match "yang"), with fuzzy=true
- Assert: total_hits = 1

**`test_fuzzy_retry_not_fired_when_exact_returns_results`**
- Arrange: index doc with tag "yang-guorui", commit, reload
- Act: search for "yang" (exact match works), verify only one query pass
- Assert: total_hits = 1, no fuzzy overhead

**`test_tag_tokenizer_splits_on_hyphens`**
- Arrange: index doc with tag "tax-return-2023"
- Act: search for "tax", "return", "2023" individually
- Assert: each search returns the document

**`test_search_three_field_loop_includes_tags`**
- Arrange: doc with body="hello", file_name="world.pdf", tag="foo"
- Act: search for "foo"
- Assert: doc found (proves tags field is in the query loop — body and file_name don't contain "foo")

**`test_empty_query_returns_empty_with_new_schema`**
- Arrange: index doc with tags
- Act: search with empty query
- Assert: returns empty results, no error

#### Property Tests (optional, `tests/` directory)

**`proptest_tag_tokenization_is_idempotent`**
- Arrange: property-based test generating random tag strings
- Act: tokenize, lowercase, re-tokenize
- Assert: second tokenization produces same tokens (idempotent)

---

### U4: AutoTagger Thread (`src/auto_tagger/thread.rs`)

#### Unit Tests (in-file `#[cfg(test)] mod tests`)

**`test_normalize_person_name_lowercase`**
- Arrange: name "Yang Guorui"
- Act: `normalize_person_name("Yang Guorui")`
- Assert: variants include "yang guorui", NOT "Yang Guorui"

**`test_normalize_person_name_strips_diacritics`**
- Arrange: name "yáng guōruì"
- Act: `normalize_person_name("yáng guōruì")`
- Assert: variants all use ASCII "yang guorui" (diacritics stripped)

**`test_normalize_person_name_generates_order_variants`**
- Arrange: name "Yang Guorui"
- Act: `normalize_person_name("Yang Guorui")`
- Assert: variants include BOTH "yang guorui" AND "guorui yang"

**`test_normalize_person_name_strips_cjk_spaces`**
- Arrange: name "Yang Guo Rui" (three-part with spaces)
- Act: `normalize_person_name("Yang Guo Rui")`
- Assert: includes "yangguorui" (concatenated form)

**`test_normalize_person_name_single_word_no_variants`**
- Arrange: name "Yang" (single word)
- Act: `normalize_person_name("Yang")`
- Assert: returns `["yang"]` only

**`test_normalize_person_name_empty_string`**
- Arrange: empty string
- Act: `normalize_person_name("")`
- Assert: returns empty vec (no variants)

**`test_normalize_person_name_only_spaces`**
- Arrange: "   "
- Act: `normalize_person_name("   ")`
- Assert: returns empty vec

**`test_normalize_person_name_mixed_cjk_and_ascii`**
- Arrange: name "Smith 约翰" (Western surname + Chinese given name)
- Act: `normalize_person_name("Smith 约翰")`
- Assert: includes "smith 约翰", "约翰 smith", "smith约翰" variants

**`test_auto_tagger_cache_hit_skips_api_call`**
- Arrange: mock provider with call counter; pre-populate `auto_tag_status` with `status=tagged` + matching hash
- Act: send `AutoTagRequest::TagDocument` for same filename+text
- Assert: provider call count = 0 (cache hit), result returned from DB

**`test_auto_tagger_cache_miss_calls_api`**
- Arrange: mock provider; no existing `auto_tag_status` row
- Act: send `TagDocument` request
- Assert: provider called exactly once

**`test_auto_tagger_retries_on_500`**
- Arrange: mock provider fails twice (500) then succeeds on 3rd call
- Act: send `TagDocument` request
- Assert: result is `Tagged`, provider called 3 times

**`test_auto_tagger_no_retry_on_401`**
- Arrange: mock provider returns 401
- Act: send `TagDocument` request
- Assert: result is `Failed`, provider called exactly 1 time

**`test_auto_tagger_all_retries_exhausted_returns_failed`**
- Arrange: mock provider fails 3 times
- Act: send `TagDocument` request
- Assert: result is `Failed`, `auto_tag_status.attempts = 3`

**`test_auto_tagger_shutdown_drains_remaining`**
- Arrange: 3 `TagDocument` in queue, then `Shutdown`
- Act: run thread loop
- Assert: all 3 processed before exit, channel drained

**`test_auto_tagger_writes_tags_to_sqlite_on_success`**
- Arrange: mock provider returning tags
- Act: process one document
- Assert: `get_auto_tag_status(hash)` returns `status=tagged` with correct `tags_json`

#### Snapshot Tests

**`test_normalize_person_name_snapshot`** (insta)
- Arrange: a set of 20 real-world Chinese + Western names
- Act: run `normalize_person_name` on each
- Assert: output matches snapshot — catches regressions in normalization pipeline

---

### U5: Runtime + Pipeline Integration

#### Integration Tests (`tests/auto_tagger_integration.rs`)

**`test_end_to_end_index_and_auto_tag`**
- Arrange: create temp directory with one sample PDF, mock TagProvider, real TagStore + Tantivy index, spawn auto-tagger thread
- Act: run pipeline to index + auto-tag
- Assert: document searchable in Tantivy, tags in SQLite, auto_tag_status = "tagged"

**`test_end_to_end_search_finds_auto_tags`**
- Arrange: full pipeline run with document tagged "yang-guorui"
- Act: search for "yang"
- Assert: document returned in search results

**`test_channel_backpressure_does_not_block_indexer`**
- Arrange: bounded channel with capacity 1, slow mock provider
- Act: send 5 documents in rapid succession
- Assert: indexer does not deadlock, all documents eventually processed or skipped with warning

**`test_auto_tagger_shutdown_gracefully`**
- Arrange: running auto-tagger with 3 documents in queue
- Act: send Shutdown, join thread with timeout
- Assert: thread exits within 5 seconds, no panic

**`test_no_api_calls_when_auto_tag_disabled`**
- Arrange: auto-tag config `enabled = false`, mock provider with call counter
- Act: index documents
- Assert: provider never called

#### Synchronization Tests (optional, `tests/auto_tagger_sync.rs`)

**`test_concurrent_reads_during_auto_tag_write`**
- Arrange: spawn reader thread querying tags while auto-tagger writes
- Act: run concurrently for 100 iterations
- Assert: reader never sees partial writes, no SQLITE_BUSY errors

---

### U6: egui UI (`src/app.rs`)

No automated unit tests for egui rendering (immediate-mode UI is inherently visual). Testing strategy:

- **Manual verification checklist** (documented, not automated):
  - [ ] Import folder → opt-in dialog appears → check "Enable" → folder imported
  - [ ] Progress bar shows during batch auto-tagging
  - [ ] Auto-tags appear with sparkle icon, dashed border
  - [ ] Entity tags show type-specific icons (person, building, calendar, document, currency)
  - [ ] Click auto-tag → toggles solid/dashed
  - [ ] Right-click → "Dismiss" → tag removed
  - [ ] Hover auto-tag → tooltip with filename + text snippets
  - [ ] Manually create tag "yang-guorui" when auto-tag "yang-guorui" exists → auto-tag absorbed
  - [ ] Status bar cloud icon: green when idle, animated when processing, red on error
  - [ ] Error banner: shown when API unreachable, "Retry" button present
  - [ ] Dismissed tag stays gone after app restart

---

### U7: Configuration (`src/auto_tagger/config.rs`)

#### Unit Tests (in-file `#[cfg(test)] mod tests`)

**`test_load_returns_default_when_no_file`**
- Arrange: config file doesn't exist
- Act: `AutoTagConfig::load()`
- Assert: returns `AutoTagConfig::default()`, enabled=false

**`test_save_and_load_round_trips`**
- Arrange: config with non-default values
- Act: `save()` → `load()`
- Assert: all fields match, including nested values

**`test_load_malformed_json_returns_default`**
- Arrange: write `"not-json"` to config file
- Act: `AutoTagConfig::load()`
- Assert: returns default, does not panic

**`test_api_key_env_name_stored_not_value`**
- Arrange: config with `api_key_env = "MY_KEY"`, real env var `MY_KEY` set to "sk-123"
- Act: save config, read file contents
- Assert: file contains `"MY_KEY"`, NOT `"sk-123"`

**`test_save_creates_parent_directory`**
- Arrange: config path in non-existent directory
- Act: `save()`
- Assert: directory created, file written successfully

**`test_save_is_atomic`**
- Arrange: existing valid config file
- Act: save new config, simulate crash (don't rename), read config
- Assert: original config intact (tmp file may exist but original untouched)

---

## Test Execution

```powershell
# Unit tests (all #[cfg(test)] modules)
cargo test --lib

# Unit tests excluding real API calls
cargo test --lib -- --skip deepseek

# Integration tests
cargo test --test auto_tagger_integration

# With test-util feature (enables MockProvider)
cargo test --features test-util

# Full suite (no real API)
cargo test --features test-util -- --skip real_api
```

---

## Test Fixtures

### Sample PDFs (for integration tests)

| Fixture | Content | Purpose |
|---------|---------|---------|
| `tests/fixtures/simple-text.pdf` | Single page, "Hello World" text | Basic extraction + tagging |
| `tests/fixtures/tax-return-en.pdf` | English tax form with name + year | Entity extraction test |
| `tests/fixtures/no-text-layer.pdf` | Image-only PDF | Skip detection (< 50 chars) |
| `tests/fixtures/encrypted.pdf` | Password-protected PDF | Locked detection |

### Mock Provider Responses (JSON fixtures)

| Fixture | Content | Purpose |
|---------|---------|---------|
| `tests/fixtures/provider_response_valid.json` | Full valid response | Happy path |
| `tests/fixtures/provider_response_empty_entities.json` | Tags present, entities empty | Empty entity handling |
| `tests/fixtures/provider_response_chinese_name.json` | Response with Chinese name | Normalization test input |

---

## Coverage Targets

| Module | Target | Notes |
|--------|--------|-------|
| `src/auto_tagger/provider.rs` | 90%+ | Trait definition — high coverage is cheap |
| `src/auto_tagger/deepseek.rs` | 80%+ | Mock provider covers most paths; real API is manual |
| `src/auto_tagger/thread.rs` | 85%+ | Normalization is pure function (100% reachable); thread loop integration-tested |
| `src/auto_tagger/config.rs` | 95%+ | Small surface, trivial to cover |
| `src/tags/store.rs` (new methods) | 90%+ | Follow existing pattern with same coverage level |
| `src/search/schema.rs` (change) | Existing + new assertions | Schema tests already exist |
| `src/search/engine.rs` (change) | 85%+ | New search tests added; fuzzy retry path covered |
| `src/runtime.rs` (change) | Integration-tested | Thread lifecycle tested via `tests/` |
| `src/app.rs` (change) | Manual | egui not amenable to automated unit tests |
