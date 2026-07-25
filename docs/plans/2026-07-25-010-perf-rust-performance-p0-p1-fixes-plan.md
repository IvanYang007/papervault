---
status: active
created: 2026-07-25
author: Pi / lfg
---

# perf: Fix P0/P1 Rust Performance Audit Findings

## Problem Frame

A systematic rust-performance audit of the papervault codebase identified P0
(critical, measurable impact on hot paths) and P1 (moderate impact) issues.
All findings are in the search engine, indexing pipeline, tag store, and
PDF renderer — the core performance-sensitive modules.

## P0 Findings

### P0-1: `match_terms.clone()` per search result — `src/search/engine.rs:386`

Each search result clones the full `match_terms: Vec<String>` vector.
For 50 results with 5 terms each, that's 250 string allocations and 50
vector allocations per search. The `match_terms` is the same for every
result — it should be shared.

**Fix:** Wrap `match_terms` in `Arc<[String]>` so cloning is a ref-count bump.

### P0-2: `get_tags_for_hashes` — per-parameter `format!` allocation — `src/tags/store.rs:171`

For each chunk of up to 500 hashes, the placeholder string `"?1, ?2, ..."`
is built by `format!("?{}", i+1)` called N times in a loop. This creates
N temporary String allocations per chunk.

**Fix:** Pre-allocate with `String::with_capacity` or use `itertools::join`.

### P0-3: `process_batch` / `process_upsert` — double `Mutex` lock per item — `src/indexer/pipeline.rs`

Each document acquires the `search_engine` Mutex lock twice: once for
`delete_by_hash` (old content cleanup), once for `index_document`.
For 32 items per batch, that's 64 lock/unlock cycles.

**Fix:** Combine both operations into a single lock scope where possible,
reducing lock acquisitions by 50%.

## P1 Findings

### P1-1: `lookup_cache_by_tokens` — per-row `HashSet` allocation — `src/tags/store.rs:245`

For each of up to 200 cache rows, `cached_tokens_str.split_whitespace()`
creates a temporary iterator and a `HashSet<&str>`. This allocates a
HashSet for each row.

**Fix:** Pre-compute set sizes or use a Vec+sort+dedup approach with reuse.

### P1-2: `process_batch` / `process_upsert` — duplicate blake3 hash boilerplate — `src/indexer/pipeline.rs`

The blake3 hash computation pattern (`Hasher::new()`, `update()`,
`finalize().to_hex().to_string()`) repeats identically 4+ times across
the pipeline. Beyond code clarity, each site does `.to_hex().to_string()`
which allocates a String.

**Fix:** Extract a helper function `compute_content_hash(text: &str, file_type: &str) -> String`.

### P1-3: `cache_insert` — `rgba_bytes.clone()` on cache store — `src/preview/pdf_render.rs:344`

Full-res rendered RGBA bitmaps are cloned into the page cache. This is
by design (the cache must own its data), but use of `Arc<Vec<u8>>` in
the cache would avoid copying multi-MB buffers.

**Fix:** Store `Arc<Vec<u8>>` in the page cache instead of `Vec<u8>`.

## Implementation Units

### U1. Extract `compute_content_hash` helper (`src/indexer/pipeline.rs`)

- **Goal:** Eliminate duplicate blake3 hashing boilerplate
- **Files:** `src/indexer/pipeline.rs`
- **Approach:** Add a free function `fn compute_content_hash(text: &str, file_type: &str) -> String` and replace all inline instances
- **Test scenarios:**
  - Same text + type produces same hash
  - Different text produces different hash
  - Different type produces different hash
- **Verification:** `cargo test -p papervault` passes, 4+ duplicate blocks removed

### U2. Use `Arc<[String]>` for `match_terms` in search results (`src/search/engine.rs`, `src/search/query.rs`, `src/app.rs`)

- **Goal:** Eliminate per-result `match_terms.clone()` allocations
- **Dependencies:** None
- **Files:** `src/search/engine.rs`, `src/search/query.rs`, `src/app.rs`
- **Approach:** Change `SearchResult.match_terms` from `Vec<String>` to `Arc<[String]>`. Compute once, clone Arc (cheap) for each result.
- **Test scenarios:**
  - Search returns results with correct match_terms
  - match_terms accessible across results without extra allocation
  - Existing search tests pass unchanged
- **Verification:** `cargo test -p papervault` passes, `match_terms.clone()` removed from result loop

### U3. Optimize `get_tags_for_hashes` placeholder generation (`src/tags/store.rs`)

- **Goal:** Eliminate per-parameter `format!` allocations
- **Files:** `src/tags/store.rs`
- **Approach:** Use `std::iter::repeat("?").take(chunk.len()).collect::<Vec<_>>().join(", ")` which is more efficient, or pre-allocate a String with known capacity
- **Test scenarios:**
  - Batch query with >500 hashes works correctly across chunks
  - Empty hash list returns empty map
  - Single hash query works
- **Verification:** `cargo test -p papervault` passes, `get_tags_for_hashes` tests pass

### U4. Combine Mutex lock scopes in pipeline indexing (`src/indexer/pipeline.rs`)

- **Goal:** Reduce lock acquisitions by 50% in batch indexing
- **Files:** `src/indexer/pipeline.rs`
- **Approach:** Merge the `delete_by_hash` and `index_document` calls into a single lock scope, acquiring the MutexGuard once
- **Test scenarios:**
  - Re-indexing a file with changed content properly removes old + adds new
  - Batch indexing produces correct document count
  - Shutdown test still passes
- **Verification:** `cargo test -p papervault` passes, `process_batch` tests pass

### U5. Optimize `lookup_cache_by_tokens` HashSet reuse (`src/tags/store.rs`)

- **Goal:** Avoid per-row HashSet allocation (up to 200 per call)
- **Files:** `src/tags/store.rs`
- **Approach:** Use a `Vec<&str>` + sort + dedup for the cached tokens comparison, or pre-build the lookup in a reusable way
- **Test scenarios:**
  - Cache lookup with full overlap returns match
  - Cache lookup with zero overlap returns None
  - Cache lookup with partial below threshold returns None
- **Verification:** `cargo test -p papervault` passes, existing cache tests pass

### U6. Use `Arc<Vec<u8>>` in page cache (`src/preview/pdf_render.rs`)

- **Goal:** Avoid cloning multi-MB RGBA buffers on cache insert
- **Files:** `src/preview/pdf_render.rs`
- **Approach:** Change `page_cache: VecDeque<(PageCacheKey, Vec<u8>, u32, u32)>` to `VecDeque<(PageCacheKey, Arc<Vec<u8>>, u32, u32)>`. On cache hit, clone the Arc instead of the Vec.
- **Test scenarios:**
  - Page cache hit returns correct RGBA bytes
  - Cache eviction works normally with Arc
  - Two-pass rendering still functions correctly
- **Verification:** `cargo build --release` succeeds, `cargo clippy` clean

## Risks

- **U2 Arc change**: `SearchResult` derives `Clone` and `Serialize`. `Arc<[String]>` supports both naturally.
- **U6 Arc change**: The render result path clones bytes from cache — switching to `Arc` is a strict improvement.
- **U4 lock scope merge**: Must ensure the MutexGuard doesn't cross any await points (it doesn't — pipeline is synchronous).

## Deferred to Follow-Up Work

- Extract more shared blake3 hashing beyond pipeline.rs (app.rs has similar patterns)
- Consider `criterion` benchmarks for regression testing perf fixes
- Profile search engine after fixes to identify next bottlenecks
