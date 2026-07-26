---
title: fix: Address Outstanding Review P0/P1 Findings (Round 2)
type: fix
status: active
date: 2026-07-26
---

# fix: Address Outstanding Review P0/P1 Findings (Round 2)

## Summary

Apply the remaining P0 (functional/crash/data-loss) and P1 (perf/robustness) fixes from the external code review that were not already addressed by the prior performance audit plan (`010-perf-rust-performance-p0-p1-fixes-plan.md`). The P0 work focuses on CJK tokenization, single-instance guard, clean shutdown, panic isolation, and file rename handling. The P1 work covers channel sizing, log rotation, SQLite pragmas, rate limiting, and build tuning.

---

## Problem Frame

An external review of the PaperVault2 codebase identified 20+ recommendations across performance, robustness, and process categories. The prior plan (`010-perf-rust-performance-p0-p1-fixes-plan.md`) addressed 13 of these. The remaining issues include a critical functional gap (Chinese/Japanese search is silently broken), crash-on-double-launch, incomplete shutdown leaking threads and WAL growth, and several performance/robustness improvements.

---

## Requirements

- R1. CJK text in PDFs and documents must be searchable with substring queries (not silently dropped)
- R2. Launching a second `papervault.exe` must show a clear "Papervault is already running" dialog, not an obscure lock error
- R3. A schema version stamp in the index must prevent unnecessary full rebuilds when only the schema changed (vs. true corruption)
- R4. Clean shutdown must join all threads, checkpoint WAL, and wait for Tantivy merge completion
- R5. One malformed PDF must not crash the batch extraction or kill the indexer thread
- R6. File renames must not leave ghost entries in the search index
- R7. Unbounded channels on render and progress paths must be replaced with bounded or latest-wins alternatives
- R8. The log file must rotate (daily, 5 retained files)
- R9. SQLite must use `synchronous=NORMAL`, `mmap_size`, and prepared statement caching for hot queries
- R10. DeepSeek auto-tagging must rate-limit API calls and show a running call counter
- R11. Rayon thread pool must use physical cores, not logical, for PDF extraction
- R12. Release profile must optimize for runtime speed (`lto="fat"`, `codegen-units=1`)

---

## Scope Boundaries

- CJK-aware search is scoped to tokenization at index and query time — full NLP segmentation (lindera) is deferred
- Single-instance guard uses a named Win32 mutex; cross-platform is not in scope
- Log rotation uses `tracing-appender`; structured logging migration is deferred
- CI setup and release pipeline are deferred to a separate plan since they require GitHub Actions configuration

### Deferred to Follow-Up Work

- CI on `windows-latest`: `cargo fmt --check`, `cargo clippy`, `cargo test`, `cargo deny check` — separate PR for `.github/workflows/ci.yml`
- Nasty-PDF test corpus (`tests/fixtures/`) — separate PR for test assets
- Criterion benchmarks — separate PR for benchmark harness
- Release zip pipeline (`papervault.exe` + `pdfium.dll` + `LICENSE`) — separate PR for release workflow
- Per-frame allocation caching in UI (`format!` in search results loop) — low ROI at current corpus size, revisit after benchmarks

---

## Context & Research

### Relevant Code and Patterns

- `src/search/engine.rs:66` — tokenizer registration (currently `SimpleTokenizer` + `LowerCaser`)
- `src/search/schema.rs` — `build_schema()` defines `body` as `TEXT | STORED`
- `src/runtime.rs:FolderRuntime::stop()` — shutdown path, currently `std::mem::forget`s auto-tagger threads
- `src/indexer/pipeline.rs:process_batch()` — rayon `par_iter()` without per-file `catch_unwind`
- `src/watcher/watcher.rs` — only handles `Create`/`Modify`/`Remove`, no rename handling
- `src/tags/store.rs` — `open_or_create()` sets `journal_mode=WAL`, `busy_timeout=5000`, `foreign_keys=ON`
- `src/preview/pdf_render.rs` — `PageCacheKey = (PathBuf, usize, u32)` already includes zoom
- `Cargo.toml` — `[profile.release]` has `lto = false`, `codegen-units = 16`, `strip = true`

### External References

- Tantivy 0.22 tokenizer API: `NgramTokenizer` and `RawTokenizer` with `max_token_length` on `TextFieldIndexing`
- `notify` 7 `EventKind::Modify(ModifyKind::Name(RenameMode::From))` / `RenameMode::To` for rename handling
- `tracing-appender` non-blocking rotation
- `ureq` rate limiting patterns

---

## Key Technical Decisions

- **CJK: n-gram bigram, not lindera**: N-gram (`NgramTokenizer::new(1, 2)`) is simple, zero-dependency, works equally for all CJK languages, and keeps binary size unchanged. Lindera adds ~50MB of dictionary data and complexity. Bigram indexes are ~2x larger but query latency stays sub-10ms at target corpus sizes.
- **Single-instance: named Win32 mutex**: Simple, well-understood, no dependency. Cross-platform is out of scope per README ("for Windows 11").
- **Schema stamp: store `schema_version: u32` as a Tantivy JSON field in index settings (`IndexSettings::docstore_compression` is not for custom metadata — use a stored document or `index.load_metas()` with a custom key)**: Actually use a sentinel file `meta.json` with a `schema_version` field alongside the index directory. Simpler and doesn't pollute the search schema.
- **Rate limiting: token-bucket with UI counter**: Simple cell-based limiter; not a full distributed rate limiter. Running counter displayed in the tag panel.
- **N-gram over CJK only: dual-field approach**: Index `body` (unchanged, SimpleTokenizer for ASCII) and `body_cjk` (NgramTokenizer). Query both with `BooleanQuery` SHOULD clauses so BM25 combines scores naturally. N-gram indexing only applies overhead to CJK documents (detected by Unicode range).

---

## Open Questions

### Resolved During Planning

- **CJK detection strategy**: Index all documents into both `body` and `body_cjk` fields. The n-gram field is cheap for ASCII (few bigrams) and critical for CJK. No per-document language detection needed — let BM25 weight the right field.
- **Rate limit value**: 30 requests/minute across all workers. Matches DeepSeek free-tier limits.

### Deferred to Implementation

- Exact rate-limit configuration location (hardcoded constant vs. config file)
- Whether `mimalloc` global allocator swap gives the claimed 10-25% improvement on this specific workload

---

## Implementation Units

### U1. CJK Tokenization for Search

**Goal:** Make Chinese, Japanese, and Korean document text searchable

**Requirements:** R1

**Dependencies:** None

**Files:**
- Modify: `src/search/schema.rs`
- Modify: `src/search/engine.rs` (tokenizer registration + search query)
- Modify: `src/search/query.rs` (SearchRequest may need CJK flag)
- Test: `src/search/engine.rs` (add CJK fixture tests)

**Approach:**
1. Add `body_cjk` field to schema — `TEXT | STORED` with an n-gram tokenizer
2. Register `NgramTokenizer::new(1, 2, false)` under `"cjk_bigram"` tokenizer name, assign to `body_cjk` field
3. Raise `max_token_length` on `body` field to 120 (chars, not bytes) so long ASCII tokens aren't silently dropped
4. In `search_with_reader`, query both `body` and `body_cjk` with SHOULD clauses for each term
5. Index documents into both fields (same text content, different tokenization)

**Patterns to follow:**
- Tokenizer registration pattern at `src/search/engine.rs:66-70`
- `BooleanQuery` construction at `src/search/engine.rs:150-160`

**Test scenarios:**
- Happy path: Index a document with Chinese text "你好世界这是一份税务文件" and search for "税务" → 1 result returned
- Happy path: Index a document with Japanese text "これは税金の書類です" and search for "税金" → 1 result returned
- Happy path: Existing ASCII search tests pass unchanged (regression)
- Edge case: Mix of CJK and ASCII in same document, search for ASCII term → same result with old behavior
- Edge case: Very long CJK document (5000+ chars) → tokens not dropped, search works
- Edge case: CJK document with no matching search term → 0 results, no panic
- Error path: Index corruption during CJK indexing → handled by existing corruption recovery

**Verification:**
- `cargo test -p papervault` passes, including new CJK fixture tests
- Manual smoke test: index a Chinese PDF, search for Chinese substring, get results

---

### U2. Single-Instance Guard

**Goal:** Prevent multiple `papervault.exe` instances from silently failing on Tantivy lock contention

**Requirements:** R2

**Dependencies:** None

**Files:**
- Modify: `src/main.rs`

**Approach:**
1. At startup (before Tantivy index open), attempt to create a named Win32 mutex (`Global\PapervaultSingleInstance`)
2. If `CreateMutexW` succeeds with `ERROR_ALREADY_EXISTS`, show a `rfd::MessageDialog` "Papervault is already running" and exit cleanly
3. Hold the mutex handle for the process lifetime (drop on exit)

**Patterns to follow:**
- `rfd` already used in the project for folder picker
- Use `std::ptr::null_mut()` and raw Win32 API via `windows-sys` or inline FFI

**Test scenarios:**
- Happy path: First launch creates mutex successfully → app proceeds normally
- Edge case: Second launch detects existing mutex → dialog shown, process exits with code 0
- Edge case: First process closes → mutex released → third launch succeeds

**Verification:**
- Launch `papervault.exe` twice; second instance shows dialog and exits
- Previous instance continues functioning normally

---

### U3. Schema Version Stamp

**Goal:** Distinguish schema migrations from true index corruption to avoid unnecessary full rebuilds

**Requirements:** R3

**Dependencies:** None

**Files:**
- Modify: `src/search/engine.rs` (open_or_create)
- Modify: `src/search/schema.rs` (add `SCHEMA_VERSION` constant)

**Approach:**
1. Define `SCHEMA_VERSION: u32 = 2` in `schema.rs`
2. On `open_or_create`, write `meta.json` with `{"schema_version": 2}` next to the index directory
3. On subsequent opens, read `meta.json`; if `schema_version` differs, log a clear message ("Schema version changed: X → Y. Rebuilding index.") and recreate
4. Distinguish from corruption: `meta.json` mismatch → rebuild (expected). `Index::open_in_dir` failure with matching version → corruption (keep existing recovery).

**Patterns to follow:**
- `meta.json` write pattern already used in `engine.rs:50` (`index_dir.join("meta.json").exists()`)

**Test scenarios:**
- Happy path: Fresh index created with schema version 2 → `meta.json` written
- Happy path: Re-open with same version → opens normally
- Edge case: Version mismatch → rebuild with log message, old index moved aside
- Edge case: `meta.json` missing (legacy index) → treat as version 1, rebuild

**Verification:**
- `cargo test -p papervault -- search` passes
- Manual: bump `SCHEMA_VERSION`, relaunch, verify rebuild log

---

### U4. Clean Shutdown Path

**Goal:** On window close, join all threads, checkpoint WAL, and wait for Tantivy merge completion

**Requirements:** R4

**Dependencies:** None

**Files:**
- Modify: `src/runtime.rs`

**Approach:**
1. In `FolderRuntime::stop()`:
   a. Signal auto-tagger shutdown (already done)
   b. **Join** auto-tagger threads with a 5-second timeout instead of `std::mem::forget`
   c. After indexer join, acquire engine lock, call `writer.wait_merging_threads()`, then `writer.commit()`
   d. Call `tag_store` checkpoint: `PRAGMA wal_checkpoint(TRUNCATE)` via a new `TagStore::checkpoint()` method
   e. Join renderer thread (already done)

**Patterns to follow:**
- Existing shutdown signaling via `AtomicBool` at `runtime.rs:144`
- `JoinHandle::join()` pattern from watcher/indexer threads

**Test scenarios:**
- Happy path: Clean exit → WAL file truncated, no thread leaks
- Edge case: Auto-tagger mid-API-call during shutdown → timeout join, thread detached after 5s
- Edge case: Tantivy mid-merge during shutdown → `wait_merging_threads()` completes, clean exit

**Verification:**
- Launch app, index a file, close window
- Verify `papervault.db-wal` is empty/truncated
- Verify `papervault.log` shows clean shutdown sequence

---

### U5. Per-File Panic Isolation in Batch Extraction

**Goal:** A single malformed PDF must not abort the entire batch or crash the indexer thread

**Requirements:** R5

**Dependencies:** None

**Files:**
- Modify: `src/indexer/pipeline.rs` (process_batch)
- Modify: `src/indexer/stages.rs` (run_chain — optional wrapper)

**Approach:**
1. Wrap each `par_iter().map()` closure in `std::panic::catch_unwind(AssertUnwindSafe(|| { ... }))`
2. On panic, log the offending path and return `(path, mtime, size, None)` so the file is skipped but batch continues
3. Add a per-file timeout: spawn extraction on a short-lived thread with `recv_timeout`; if exceeded, log and skip

**Patterns to follow:**
- `par_iter()` pattern at `pipeline.rs:155`
- `catch_unwind` already used in schema tests at `schema.rs:85`

**Test scenarios:**
- Edge case: Corrupt PDF that panics pdf_oxide → logged, skipped, remaining files in batch indexed successfully
- Error path: PDF that hangs extraction → timeout triggers, file skipped, batch continues
- Happy path: Normal batch extraction unchanged (no performance regression)

**Verification:**
- `cargo test -p papervault` passes
- Create a deliberately corrupt PDF fixture, run indexing, verify no crash

---

### U6. Handle File Rename/Move Events

**Goal:** File renames must not create ghost entries in the search index

**Requirements:** R6

**Dependencies:** None

**Files:**
- Modify: `src/watcher/watcher.rs`

**Approach:**
1. Handle `EventKind::Modify(ModifyKind::Name(RenameMode::From))` → delete old path from index (like Remove)
2. Handle `EventKind::Modify(ModifyKind::Name(RenameMode::To))` → index new path (like Create)
3. Add periodic ghost sweep: during reconciliation, delete Tantivy documents whose `file_path` no longer exists on disk (currently only backfills, doesn't clean up)

**Patterns to follow:**
- Existing event handler at `watcher.rs:42-76`
- Reconciliation loop at `pipeline.rs:reconcile()`

**Test scenarios:**
- Happy path: Rename `a.pdf` → `b.pdf` → old path removed from index, new path indexed
- Edge case: Rename across directories → both Delete and Create events processed
- Edge case: Ghost sweep removes entries for files deleted outside watcher
- Edge case: Supported extension renamed to unsupported → removed from index

**Verification:**
- `cargo test -p papervault` passes
- Manual: rename an indexed file, verify search returns new path, not old

---

### U7. Channel Sizing — Bounded and Latest-Wins

**Goal:** Prevent unbounded memory growth on progress and render channels

**Requirements:** R7

**Dependencies:** None

**Files:**
- Modify: `src/runtime.rs`

**Approach:**
1. **Progress channel**: Replace `unbounded` with `bounded(16)` — the UI only needs the latest progress state, and the indexer can `try_send` without blocking
2. **Render request channel**: Replace `unbounded` with `bounded(4)` — coalescing already drains stale requests; 4 provides headroom
3. **Render result channel**: Replace `unbounded` with `bounded(4)` — the UI can only display one at a time
4. **Auto-tagger channel**: Already checked on recv; replace `unbounded` with `bounded(32)` to limit memory during bulk indexing
5. **Tag channel**: Replace `unbounded` with `bounded(64)` — tag updates are low-volume

**Patterns to follow:**
- Existing bounded channel at `runtime.rs:51` for watcher (256)

**Test scenarios:**
- Happy path: Normal operation — all channels drain without blocking
- Edge case: Burst of 1000 progress messages → indexer `try_send` succeeds for first 16, drops rest (latest progress is all UI needs)
- Edge case: Render channel full → `try_send` fails, UI shows stale frame briefly, next request replaces

**Verification:**
- `cargo build --release` succeeds
- App runs with normal indexing and rendering behavior

---

### U8. Log Rotation

**Goal:** Prevent `papervault.log` from growing without bound

**Requirements:** R8

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml` (add `tracing-appender` dependency)
- Modify: `src/main.rs` (log initialization)

**Approach:**
1. Add `tracing-appender = "0.2"` to dependencies
2. Replace `std::fs::File::create` with `tracing_appender::rolling::daily(&log_dir, "papervault")`
3. Use `tracing_appender::non_blocking` so log writes don't block the caller
4. Keep 5 retained files (daily rotation default)
5. Guard the non-blocking worker: drop it on clean shutdown

**Patterns to follow:**
- Existing log path resolution at `main.rs:22-25`

**Test scenarios:**
- Happy path: Log writes go to `papervault.YYYY-MM-DD` file
- Edge case: Day boundary → new file created, old retained
- Edge case: 6th day → oldest file deleted

**Verification:**
- `cargo build --release` succeeds
- Launch app, verify log file created with date suffix

---

### U9. SQLite Performance Pragmas

**Goal:** Improve tag store write throughput and read performance

**Requirements:** R9

**Dependencies:** None

**Files:**
- Modify: `src/tags/store.rs`

**Approach:**
1. In `open_or_create()`, add `PRAGMA synchronous = NORMAL` (safe with WAL; 2-3x write speed)
2. Add `PRAGMA mmap_size = 268435456` (256MB — reduces read syscalls for hot `get_tags_for_hashes`)
3. Switch top ~5 hot queries to `conn.prepare_cached()`:
   - `get_tags_for_document`
   - `already_indexed_by_metadata`
   - `get_hash_by_path`
   - `list_all_documents`
   - `lookup_cache_by_tokens`

**Patterns to follow:**
- Existing pragma batch at `store.rs:29-31`

**Test scenarios:**
- Happy path: All existing tag store tests pass unmodified (behavior unchanged)
- Edge case: Concurrent read/write with `synchronous=NORMAL` → no corruption (WAL guarantees)

**Verification:**
- `cargo test -p papervault -- tags` passes

---

### U10. DeepSeek Rate Limiter + Call Counter

**Goal:** Prevent surprise API bills from bulk auto-tagging and show running call count in UI

**Requirements:** R10

**Dependencies:** None

**Files:**
- Modify: `src/auto_tagger/thread.rs`
- Modify: `src/auto_tagger/config.rs` (optional `requests_per_minute` field)
- Modify: `src/app.rs` (display call counter)

**Approach:**
1. Add a `std::sync::Mutex<Vec<Instant>>` as a sliding-window rate limiter (30 requests/minute)
2. Before each API call, check window: if 30 calls in last 60 seconds, sleep until a slot opens
3. Show running counter in tag panel: `"☁ Auto-tag: N calls today"` using the existing `progress` `AtomicUsize`
4. Add a confirmation dialog when auto-tagging would trigger >100 calls ("This will make ~N API calls. Continue?")

**Patterns to follow:**
- Existing progress counter at `runtime.rs:81` (`Arc<AtomicUsize>`)
- Existing auto-tag status display at `app.rs:1080-1090`

**Test scenarios:**
- Happy path: Normal tagging flow — calls proceed at ≤30/min
- Edge case: Burst of 200 documents → first 30 processed, rate limiter throttles, all eventually tagged
- Edge case: Rate-limited worker wakes other workers via shared limiter → fair across threads

**Verification:**
- `cargo build --release` succeeds
- Auto-tagger log shows rate-limited calls when indexing 100+ files

---

### U11. Rayon Physical Core Capping

**Goal:** Reduce memory bandwidth contention during parallel PDF extraction

**Requirements:** R11

**Dependencies:** None

**Files:**
- Modify: `src/indexer/pipeline.rs` (process_batch)

**Approach:**
1. Before the `par_iter()` call, configure a scoped rayon thread pool: `rayon::ThreadPoolBuilder::new().num_threads(num_cpus::get_physical()).build()`
2. Add `num_cpus` dependency if not already transitively available
3. Run the parallel extraction inside `pool.install(|| { ... })`

**Patterns to follow:**
- Existing `par_iter()` at `pipeline.rs:155`

**Test scenarios:**
- Happy path: On 8c/16t machine, only 8 threads used for extraction (confirmed via log or task manager)
- Edge case: Single-core machine → 1 thread, no crash
- Happy path: Extraction throughput unchanged or improved vs. logical-core oversubscription

**Verification:**
- `cargo build --release` succeeds
- No regression in `cargo test -p papervault`

---

### U12. Release Build Tuning

**Goal:** Improve runtime performance 10-25% with release build settings

**Requirements:** R12

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml`

**Approach:**
1. Set `lto = "fat"` (link-time optimization)
2. Set `codegen-units = 1` (single LLVM codegen unit for better inlining)
3. Set `opt-level = 3` (already default for release, but explicit)
4. Keep `strip = true` (already set)
5. Add `panic = "unwind"` explicitly (needed for U5 catch_unwind; default is unwind already, but explicit is safer)
6. Consider adding `[profile.dist]` with the fast settings for CI builds vs local dev

**Patterns to follow:**
- Existing `[profile.release]` at `Cargo.toml:27-33`
- `[profile.dev]` section for reference

**Test scenarios:**
- Happy path: `cargo build --release` succeeds (build will be slower)
- Regression: `cargo test -p papervault` passes with release profile
- Edge case: Binary size increase from fat LTO → acceptable (<2x current)

**Verification:**
- `cargo build --release` compiles without error
- App launches and all features work (search, preview, indexing)

---

## System-Wide Impact

- **Interaction graph:** Tokenizer change (U1) affects indexing and search paths. Schema change adds a new field — existing indexes will be rebuilt due to schema version bump (U3). Window close path (U4) affects all thread lifetimes. Channel sizing (U7) changes producer backpressure behavior — `try_send` failures must be handled gracefully.
- **Error propagation:** Per-file panic isolation (U5) converts panics into logged warnings. Rate limiter errors (U10) surface as delayed tagging, not failures.
- **State lifecycle risks:** Schema version bump forces a one-time full reindex for all users on next launch. This should be communicated in the release notes.
- **API surface parity:** The Tantivy schema change is backward-incompatible by design (version-gated rebuild). No external API changes.
- **Integration coverage:** CJK search + regular search must coexist. Shutdown ordering (auto-tagger → indexer → Tantivy → SQLite → log) must be respected.
- **Unchanged invariants:** File watching behavior outside rename handling. PDF rendering pipeline. Tag CRUD API. egui UI layout.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| Schema version bump forces full reindex for all users | Log clear message; this is a one-time cost and the index rebuild is fast (~20s for 700 PDFs) |
| `NgramTokenizer` increases index size for ASCII documents | Bigrams on ASCII produce few unique tokens; index growth is ~10-20%, not 2x |
| `lto = "fat"` makes release builds 3-5x slower | Acceptable tradeoff; dev builds use `[profile.dev]` which is unchanged |
| Rate limiter may cause auto-tagging to take longer for large folders | UI shows progress counter; user can disable auto-tagging |
| Bounded channels may drop messages under extreme load | `try_send` failures are silent; progress is best-effort, render requests are coalesced |

---

## Sources & References

- Review evaluation from prior conversation (Section 2.1–4 in the review document)
- Prior plan: `docs/plans/2026-07-25-010-perf-rust-performance-p0-p1-fixes-plan.md`
- Tantivy 0.22 docs: tokenizer API, `NgramTokenizer`, `TextFieldIndexing::set_index_option`
- `notify` 7 docs: `EventKind::Modify(ModifyKind::Name(RenameMode::...))`
