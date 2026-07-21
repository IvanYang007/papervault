---
title: fix: Resolve AI Code Review Findings — Lifecycle, Correctness, Responsiveness
type: fix
status: active
date: 2026-07-21
---

# fix: Resolve AI Code Review Findings — Lifecycle, Correctness, Responsiveness

## Summary

Fix 13 correctness, lifecycle, responsiveness, and robustness issues identified in the consolidated AI code review of the `feat/pdf-search-viewer` branch. The work spans first-launch folder indexing, thread join on shutdown, per-folder index scoping, search focus, per-frame database work removal, Unicode snippet safety, render request identity, selection stability, progress semantics, tag-filtering N+1 queries, text preview cap, error visibility, and diagnostic improvements.

---

## Problem Frame

An AI code review (July 2026) of papervault's `feat/pdf-search-viewer` branch found that while the modular architecture is sound, several release-blocking issues prevent correct behavior at first launch, during folder changes, and on shutdown. The search bar does not accept keyboard input at startup, the UI thread performs database I/O every frame, folder switching is not implemented (the index is global rather than per-folder), threads are not explicitly joined on exit, and several correctness issues (Unicode panics, stale render results, index-based selection, incorrect progress display, N+1 tag queries) compromise reliability at scale. The review also identified diagnostic gaps: errors are silently discarded, and the Windows subsystem attribute prevents console output even in debug builds.

---

## Requirements

- R1. First-launch folder selection must start the indexer, watcher, and initial scan without requiring an app restart.
- R2. Changing the watched folder must stop old workers, switch to a folder-scoped index, and start new workers — no cross-contamination of search results between folders.
- R3. Graceful shutdown must explicitly join all background threads in order (watcher → indexer → renderer) so the indexer completes its final commit before process exit.
- R4. Search input must be immediately interactive at startup and remain responsive during indexing (no per-frame database I/O, bounded channel processing).
- R5. Unicode text highlighting must not panic on multi-byte characters when byte offsets from lowercased strings are applied to original snippets.
- R6. Render results must carry identity (request ID, path, page) so the UI can discard stale results and disable page navigation at bounds.
- R7. Search result selection must use stable document identity (content hash) rather than a vector index that becomes invalid after every search.
- R8. Indexing progress display must accurately reflect the number of processed files without misleading fraction displays.
- R9. Tag filtering must not miss results ranked beyond an arbitrary cutoff and must not perform N+1 database queries on the UI thread.
- R10. Text file preview must cap at a reasonable size to prevent UI freezes on large files.
- R11. Database and configuration errors must be surfaced visibly, not silently discarded, and configuration must be saved atomically.
- R12. The Windows subsystem attribute must be conditional (`cfg_attr(not(debug_assertions))`) so debug builds show console output for diagnostics.
- R13. Cargo.toml must include `license` and `repository` fields, and a LICENSE file must exist in the repository root.

**Origin flows:** F1 (Search and Preview), F2 (File Discovery and Indexing), F3 (Tag Assignment)
**Origin acceptance examples:** AE1 (new files searchable within 2s), AE2 (deleted files removed from search), AE3 (highlighted search terms), AE4 (tag filtering)

---

## Scope Boundaries

- CI pipeline setup, default branch merge, and release publishing are deferred to follow-up — they are repo-quality items that do not block app correctness.
- Latin-1 encoding support beyond the current UTF-8 lossy fallback is deferred — the current behavior degrades gracefully (replacement characters) rather than crashing.
- Persistent error log file for non-panic errors is deferred — the conditional `windows_subsystem` fix enables console-based debugging in dev; a permanent log file is a separate enhancement.
- New features (OCR, AI auto-tagging, multi-folder watching, dark mode, keyboard shortcuts) are out of scope — this plan fixes existing defects only.
- PDF editing, merging, cloud sync, multi-user, and non-Windows platforms remain out of scope per the origin requirements document.

### Deferred to Follow-Up Work

- CI pipeline (GitHub Actions) — separate PR after code fixes land
- Default branch merge + release publishing — separate PR
- Latin-1 / Windows-1252 explicit decoding — future PR
- Persistent `papervault.log` for non-panic errors — future PR

---

## Context & Research

### Relevant Code and Patterns

- **Thread orchestration:** `src/main.rs` creates 4 threads (UI, Indexer "indexer", Watcher "watcher", Renderer "renderer") and 5 crossbeam channels. The watcher channel is bounded (10K); render, progress, tag update channels are unbounded.
- **Search engine lifecycle:** `src/search/engine.rs` — `SearchEngine::open_or_create` (recently fixed with `create_dir_all` + corruption recovery). The index lives at `%LOCALAPPDATA%/papervault/index/` regardless of which folder is watched. `IndexReader` is cloned for lock-free UI search via `ReloadPolicy::OnCommitWithDelay`.
- **Folder-less startup:** `main.rs:71-82` — indexer/watcher threads are spawned only when `config.watched_folder` is already set at startup. If the user starts without a folder and sets one via UI, `init_search_engine()` only opens an engine — no indexer, watcher, or initial scan.
- **Per-frame database I/O:** `app.rs:322-325` — `poll_channels()` calls `TagStore::list_tags()` every frame (60/s), opening a new SQLite connection each time.
- **Unbounded channel draining:** `app.rs:287-325` — `poll_channels()` uses unlimited `while let Ok(...)` loops on render results and indexer progress channels.
- **Shutdown via drop ordering:** `app.rs:736-746` — `on_exit()` sets `AtomicBool`, drops one sender clone. The indexer `JoinHandle` uses `_` prefix — joined only via Drop impl, no timeout, no explicit join. Watcher and renderer handles are not stored.
- **Watcher `Box::leak`:** `src/watcher/watcher.rs` — the debouncer handle is leaked for lifetime simplicity. A `JoinHandle` would enable proper cleanup.
- **Tag N+1 queries:** `app.rs:287` — `do_search()` calls `store.get_tags_for_document(hash)` for up to 200 results individually, each opening a new SQLite connection.
- **Unicode snippet rendering:** `app.rs:382-445` — lowercases the snippet, finds byte offsets via `str::find()`, applies those offsets to the original snippet. Characters like `İ` (Turkish I) change byte length on lowercasing, causing potential panics.
- **Render identity gap:** `app.rs:236,287` — `RenderResult` contains `rgba_bytes, width, height, highlights` with no request ID, path, or page. The UI accepts every result for the `"pdf-preview"` texture without stale detection.
- **Selection by index:** `app.rs:112` — `selected_result: Option<usize>` points into `search_results`. After `do_search()` replaces the results vector, the same index may refer to a different document.
- **Error discarding:** At least 15 locations use `.ok()` or `let _ =` to silently discard errors — thread spawns, channel sends, config save, tag store open, search engine open.
- **Config save:** `src/config.rs:32-38` — uses `fs::write()` directly, which is non-atomic. A crash during write can corrupt `config.json`.

### Institutional Learnings

- **Tantivy corruption recovery:** The codebase already uses `garbage_collect_files()` on open and `remove_dir_all` + recreate on corrupted open. This pattern works and should be preserved.
- **SQLite connection overhead:** Each `TagStore::connect()` call re-runs 3 `PRAGMA` statements. The handoff doc (TECHNICAL-HANDOFF.md) explicitly calls this out as needing connection caching.
- **Watcher shutdown cascade:** `AtomicBool` → watcher exits → debouncer drops → sender channel closes → indexer gets `Disconnected` → final commit → exits. This cascade works but needs explicit join to guarantee completion.
- **Lock-free search reader pattern:** Clone `IndexReader` at startup → store in UI state without Mutex → brief Mutex lock only to clone `SchemaFields`. This pattern is sound; the plan's U1 makes it fully lock-free by pre-cloning `SchemaFields`.
- **Unicode highlight byte-offset bug:** Documented as a latent panic risk — lowercased string byte offsets don't map 1:1 to original UTF-8 bytes for characters like `İ` → `i̇` (Turkish I).

### External References

- Tantivy 0.22 `ReloadPolicy::OnCommitWithDelay` documentation — reader auto-reloads within milliseconds of commit
- egui 0.30 `TextEdit` focus behavior — widgets gain focus on click via `Sense::click()` but require `request_focus()` for programmatic focus
- egui known issue: TextEdit may not respond to keyboard input on Windows without explicit focus request on first frame (GitHub issues #4806, #7923)

---

## Key Technical Decisions

- **Hash-based per-folder indexes:** Each watched folder gets a separate Tantivy index under `%LOCALAPPDATA%/papervault/indexes/<blake3_hex>/`. This preserves indexes across folder switches (user can switch back without full rebuild) and prevents cross-contamination. Trade-off: more disk usage for multiple indexes.
- **Minimum-safety Unicode fix:** Add `is_char_boundary()` guards before slicing rather than implementing full Unicode segmentation or Tantivy-provided offsets. This prevents panics with minimal code change. A full fix (Tantivy snippet provider with char-level offsets) is deferred.
- **Batch tag query:** Replace per-result `get_tags_for_document()` calls with a single `SELECT content_hash, tag_name FROM document_tags JOIN tags WHERE content_hash IN (...)` query. This eliminates N+1 overhead and removes the 200-result arbitrary cap (all results get tags without reloading SQLite per result).
- **SchemaFields pre-clone:** Store `SchemaFields` in `PapervaultApp` at engine initialization time rather than cloning under Mutex on every search. `search_with_reader` already takes `&SchemaFields` — this removes the last Mutex acquisition from the search path.
- **Render request monotonic ID:** A `u64` counter incremented on each request, carried into `RenderResult`. The UI discards any result whose ID does not match the latest issued. A bounded channel or latest-wins drain on the renderer side handles rapid page-navigation.

---

## Open Questions

### Resolved During Planning

- **Index-per-folder vs destructive switch:** Hash-based per-folder indexes — supports switching back, matches the review's recommendation. If storage pressure becomes a concern, a future cleanup of old indexes can be added.
- **Unicode fix depth:** Minimum safety (`is_char_boundary()`) — prevents crashes without over-engineering a rare edge case.
- **Tag filtering approach:** Batch SQLite query — simpler than Tantivy tag sync, removes the 200-result cap workaround, eliminates N+1 overhead.
- **Tag refresh timing:** Load tags at startup and after create/delete only (removed from per-frame `poll_channels`). A slow timer (e.g., every 5 seconds) can be added later if external tag changes need polling.

### Deferred to Implementation

- Exact batch SQL query shape — depends on the `TagStore` API surface at implementation time.
- `FolderRuntime` channel buffer sizes — the existing bounded 10K for the watcher channel is appropriate; implementer should match existing sizes for the new channels.
- Exact renderer drain strategy — bounded channel or `try_recv` drain loop; implementer chooses based on latency measurements.
- Connection caching strategy for `TagStore` — `RefCell<Option<Connection>>` vs persistent connection behind Mutex; implementer chooses based on the borrow patterns in the tag methods.

---

## Implementation Units

### Phase 1: Immediate Responsiveness (Critical UX fixes)

### U1. Search Responsiveness & Focus & Lock-Free Search

**Goal:** Make the search bar immediately interactive, remove per-frame database I/O, budget channel processing, and make search truly lock-free.

**Requirements:** R4

**Dependencies:** None

**Files:**
- Modify: `src/app.rs`
- Modify: `src/main.rs`

**Approach:**
- Add `focus_search_next_frame: bool` to `PapervaultApp`, set to `true` in `new()`. In `update()`, call `response.request_focus()` on the search TextEdit response when this flag is set, then set it to `false`.
- Remove `TagStore::list_tags()` call from `poll_channels()`. Load tags once in `PapervaultApp::new()` and refresh only in `create_tag()` and `assign_tag_to_selected()` (and after tag deletion, not yet implemented).
- Add `MAX_MESSAGES_PER_FRAME = 64` constant. Convert `poll_channels()` loops from `while let Ok(...)` to `for _ in 0..MAX_MESSAGES_PER_FRAME` with `try_recv()` break.
- Clone `SchemaFields` at engine init and store in `PapervaultApp` alongside the `IndexReader`. In `do_search()`, use the pre-cloned fields directly — remove the `self.search_engine.as_ref().map(|e| e.lock().unwrap().fields().clone())` Mutex acquisition.
- Ensure `search_query` state persists correctly — no struct-level resets per frame.

**Patterns to follow:**
- Existing `PapervaultApp` struct field pattern in `src/app.rs:71-107`
- Existing `TagStore::list_tags()` in `src/tags/store.rs` for the correct SQL query
- `search_with_reader()` free function in `src/search/engine.rs:152-225` already takes `&SchemaFields`

**Test scenarios:**
- Happy path: Launch app → search field has focus → type "test" → text appears in the search bar
- Happy path: App runs for 10 seconds → `poll_channels()` processes at most 64 messages per channel, not unlimited
- Happy path: `do_search()` executes without acquiring the search engine Mutex
- Edge case: Indexer sends 1,000 progress messages between frames → only 64 are processed per frame, remainder processed on subsequent frames
- Error path: Tag store unavailable at startup → `all_tags` defaults to empty Vec, no crash

**Verification:**
- Search bar accepts keyboard input immediately after app launch without clicking
- `list_tags()` is not called during `update()` — tags load once at startup and after mutations
- `poll_channels()` loops are bounded — no frame can consume unbounded messages
- `do_search()` never locks `self.search_engine` — it reads from pre-cloned `SchemaFields`

---

### Phase 2: Lifecycle & Architecture (Largest functional gaps)

### U2. Folder Runtime Lifecycle & Per-Folder Index

**Goal:** Implement proper folder runtime lifecycle so first-launch folder selection starts all workers, folder changes cleanly stop old workers and start new ones, and each watched folder has its own isolated index.

**Requirements:** R1, R2, R8 (partial — channel creation enables progress reporting)

**Dependencies:** U1

**Files:**
- Create: `src/runtime.rs` (new module)
- Modify: `src/main.rs`
- Modify: `src/app.rs`
- Modify: `src/search/engine.rs`
- Modify: `src/config.rs`

**Approach:**
- Create `src/runtime.rs` with a `FolderRuntime` struct holding: `watcher_shutdown: Arc<AtomicBool>`, `watcher_handle: Option<JoinHandle<()>>`, `indexer_handle: Option<JoinHandle<()>>`, `watcher_tx: Sender<IndexerMessage>`, `tag_tx: Sender<TagUpdate>`, `progress_rx: Receiver<IndexerProgress>`, `render_tx: Sender<RenderRequest>`, `render_result_rx: Receiver<RenderResult>`, `search_engine: Arc<Mutex<SearchEngine>>`, `search_reader: IndexReader`.
- Implement `FolderRuntime::start(folder: &Path, tag_store: &TagStore) -> Result<Self>` — creates channels (including the `tag_tx`/`tag_rx` pair inside `start()`, moving `tag_rx` into the pipeline thread), opens/creates per-folder search engine, spawns indexer/watcher/renderer threads, performs initial scan. Only `tag_tx` is stored in the struct for UI access; `tag_rx` lives in the pipeline thread.
- Implement `FolderRuntime::stop(self) -> Result<()>` — sets shutdown flag, joins watcher, drops senders, joins indexer (with final commit), drops render sender, joins renderer. Returns any join errors.
- Change `SearchEngine::index_directory()` to produce a per-folder path: `%LOCALAPPDATA%/papervault/indexes/<blake3_hex_of_canonical_path>/`. The `watched_folder` parameter is no longer ignored — canonicalize via `std::fs::canonicalize(folder)?` (folder existence already validated by the UI), then hash to derive the index path. Remove the underscore.
- In `PapervaultApp`: replace the discrete channel/handle/engine fields with `Option<FolderRuntime>`. Add `start_folder_runtime(&mut self, folder: PathBuf)` and `stop_folder_runtime(&mut self)` methods that delegate to `FolderRuntime`.
- In `PapervaultApp::update()`: when "Set Folder" is clicked and the path is valid, call `stop_folder_runtime()` (if one is running), then `start_folder_runtime(new_path)`, update `self.config.watched_folder`, save config.
- Move the reconciliation call (`pipeline::reconcile`) from `main.rs` startup into `FolderRuntime::start` — it is folder-scoped and must run after the engine and tag store are ready, before the watcher starts.
- `main.rs` startup: if `config.watched_folder` is already set, call `FolderRuntime::start()` to create the initial runtime. Pass the runtime's components (channels, handles, engine, reader) into `PapervaultApp::new()`.

**Execution note:** Test-first for `FolderRuntime::start` / `stop` — these are the new abstractions with the most failure modes. Use a temp directory with a few test files to verify full round-trip (start → index → search → stop → start with different folder).

**Patterns to follow:**
- Existing channel creation pattern in `main.rs:54-68` (crossbeam bounded/unbounded channels)
- Existing thread spawn pattern in `main.rs:98-142` (named threads, `.ok()` → replace with `Result` propagation)
- Existing `SearchEngine::open_or_create` in `src/search/engine.rs:30-77`
- Existing `pipeline::reconcile` in `src/indexer/pipeline.rs:201-263`
- `blake3` hashing already in use for content-based dedup in `src/indexer/pipeline.rs`

**Test scenarios:**
- Happy path: `FolderRuntime::start("/tmp/test-folder")` → creates channels, opens engine, spawns threads, returns Ok
- Happy path: `FolderRuntime::stop()` → watcher exits → indexer commits → threads joined → Ok
- Happy path: First launch → user selects folder → indexer starts, watcher starts, files indexed
- Happy path: Switch folder from A to B → old runtime stopped → new runtime started → search results from B only
- Happy path: Switch back to folder A → old index preserved (hash matches), no full reindex needed
- Happy path: Two different folders produce different index directories under `indexes/<different_hashes>/`
- Edge case: `start_folder_runtime` called while a runtime is already active → old runtime stopped cleanly first
- Error path: Folder does not exist → `start` returns error, old runtime unaffected
- Error path: Thread spawn failure → `start` returns error, all previously created resources cleaned up
- Integration: Start runtime with a folder containing 3 PDFs → run `pipeline::reconcile` → all 3 documents appear in search results within 5 seconds
- Integration: Start runtime, assign a tag, stop runtime, start runtime again → tag persists in SQLite, `reconcile` backfills Tantivy

**Verification:**
- First-launch folder selection triggers indexing without app restart
- Switching folders isolates search results — no cross-folder document leakage
- Per-folder index directories exist under `%LOCALAPPDATA%/papervault/indexes/<hash>/`
- `FolderRuntime::stop()` joins all threads — process does not exit before final commit

---

### U3. Graceful Shutdown with Explicit Thread Joins

**Goal:** Guarantee all background threads complete their final work before process exit by explicitly joining in order: watcher → indexer → renderer.

**Requirements:** R3

**Dependencies:** U2 (FolderRuntime owns the handles)

**Files:**
- Modify: `src/runtime.rs`
- Modify: `src/watcher/watcher.rs`
- Modify: `src/main.rs`

**Approach:**
- In `FolderRuntime::stop()`: (1) signal shutdown flag, (2) join watcher thread with timeout, (3) drop all sender clones to close channels, (4) join indexer thread — this triggers the final commit in the indexer's `Disconnected` handler, (5) drop render sender, (6) join renderer thread.
- Store the watcher thread's `JoinHandle` in `FolderRuntime` so `stop()` can explicitly join it. Currently `watcher_handle` is created in `main.rs` but dropped without joining — `stop()` fixes this. The debouncer is already a local variable in `start_watching()` and drops naturally on function exit; `join()` ensures we wait for that to complete.
- `main.rs`: after `eframe::run_native` returns (app window closed), call `runtime.stop()` explicitly before process exit. The implicit drop-ordering backup still works but the explicit call is the primary mechanism.
- Add a join timeout (e.g., 5 seconds) for the indexer thread — if the indexer hangs, log an error and exit rather than hanging the process.

**Patterns to follow:**
- Existing `AtomicBool` shutdown flag pattern in `src/main.rs:83-87`
- Existing `thread::Builder::new().name(...).spawn()` pattern in `src/main.rs:98-142`

**Test scenarios:**
- Happy path: Close app → watcher signaled → watcher exits → senders dropped → indexer commits → indexer joined → renderer joined → process exits
- Happy path: Indexer has 5 pending documents at shutdown → all 5 committed before join returns
- Edge case: Watcher thread panics → join captures the panic, indexer still shut down cleanly
- Edge case: Indexer thread hangs (simulated infinite loop) → join times out after 5 seconds → error logged → process exits
- Error path: Renderer channel already disconnected → renderer join succeeds immediately

**Verification:**
- `FolderRuntime::stop()` explicitly joins all three threads
- No `Box::leak` remains in the watcher — debouncer is dropped normally
- Existing shutdown-commits pipeline test (`pipeline.rs:328`) still passes
- Process does not exit before the indexer's final `commit()` completes

---

### Phase 3: Correctness Fixes

### U4. Unicode Highlighting Safety Fix

**Goal:** Prevent panics in `render_highlighted_snippet()` when byte offsets from the lowercased snippet are applied to the original UTF-8 snippet.

**Requirements:** R5, AE3

**Dependencies:** None

**Files:**
- Modify: `src/app.rs`

**Approach:**
- In `render_highlighted_snippet()`, after finding byte offsets in the lowercased string and before slicing the original snippet, validate each offset with `original.is_char_boundary(start)` and `original.is_char_boundary(end)`. If either check fails, skip that span (log a warning at debug level).
- This prevents the panic while preserving correct highlighting for the vast majority of cases (ASCII + most Western European text). Characters that change byte length on lowercasing (Turkish `İ` → `i̇`, German `ß` → `ss`) may not highlight correctly but will not crash.

**Patterns to follow:**
- Existing `render_highlighted_snippet` in `src/app.rs:382-445`
- `str::is_char_boundary()` — standard library method, no new dependencies

**Test scenarios:**
- Happy path: Search "invoice" in English text → highlights "invoice" correctly
- Happy path: Search "über" in German text → highlights "Über" correctly (case-insensitive match, byte offsets valid)
- Edge case: Snippet contains Turkish `İ` (U+0130) → search for lowercase "i" → offset validation skips the invalid span, no panic
- Edge case: Snippet contains `ß` (German sharp s) → search for "ss" → replaced by different byte length → offset validation skips, no panic
- Edge case: All spans in a snippet have invalid offsets → snippet renders as plain gray text (no highlights), no panic

**Verification:**
- `render_highlighted_snippet` never panics on any Unicode input
- Existing test file search tests in `src/search/engine.rs` continue to pass

---

### U5. Render Request Identity & Page Bounds

**Goal:** Add request identity tracking to `RenderRequest`/`RenderResult` so stale results are discarded, and add `page_count` so page navigation has correct bounds.

**Requirements:** R6

**Dependencies:** None

**Files:**
- Modify: `src/app.rs`
- Modify: `src/preview/pdf_render.rs`

**Approach:**
- Add fields to `RenderRequest`: `request_id: u64`, `path: PathBuf`, `page: usize`. Remove `search_terms: Vec<String>` (highlights are unimplemented — stub returns empty vec).
- Add fields to `RenderResult`: `request_id: u64`, `path: PathBuf`, `page: usize`, `page_count: usize`. The existing `rgba_bytes`, `width`, `height` remain.
- In `PapervaultApp`: add `latest_render_request_id: u64` and `current_pdf_page_count: usize`. Increment `latest_render_request_id` on each `request_page_render()` call.
- In `poll_channels()`: accept a `RenderResult` only if `result.request_id == self.latest_render_request_id && result.path == self.current_preview_path`. Otherwise discard.
- In `PdfRenderer::render_page()`: after loading the PDF, extract `page_count` via `pdf.bindings().get_page_count()` and include it in the result.
- UI: disable "Next ▶" button when `self.current_page >= self.current_pdf_page_count`. Clear `current_pdf_page_count` when selecting a different document.
- Clear the preview texture immediately when selecting a new document — prevents stale display during render queue drain.

**Patterns to follow:**
- Existing `RenderRequest`/`RenderResult` structs in `src/app.rs:22-55`
- Existing `PdfRenderer::render_page()` in `src/preview/pdf_render.rs:90-155`
- Monotonic counter pattern — `u64` wrapping is effectively impossible at GUI frame rates

**Test scenarios:**
- Happy path: Select PDF A → render request ID 1 sent → result ID 1 arrives → preview updates
- Happy path: Select PDF → page_count returned in result → UI limits Next button at last page
- Edge case: Rapidly navigate 5 pages → requests 1-5 sent → results 1-4 discarded (stale IDs), result 5 shown
- Edge case: Select PDF A, then PDF B before A's render completes → A's result discarded, B's result shown when it arrives
- Edge case: PDF has 1 page → Next button disabled, Prev button disabled
- Error path: Render fails → result carries error indicator → old texture cleared, no stale display

**Verification:**
- Rapid page navigation always shows the correct page — no stale visuals
- Next button is disabled on the last page of a multi-page PDF
- Selecting a different document clears the old preview texture immediately

---

### U6. Selection Stability via Content Hash

**Goal:** Replace index-based result selection (`selected_result: Option<usize>`) with stable document identity (`selected_content_hash: Option<String>`) so selection survives search query changes.

**Requirements:** R7, R9 (partial — removes 200-result cap in do_search), AE4

**Dependencies:** None

**Files:**
- Modify: `src/app.rs`

**Approach:**
- Replace `selected_result: Option<usize>` with `selected_hash: Option<String>`.
- In `select_result()`: set `selected_hash` from `search_results[idx].content_hash` and clear preview state.
- After `do_search()` returns new results: if `selected_hash` is `Some`, search the new results for a matching `content_hash`. If found, set `self.selected_index = Some(idx)` (internal mapping for rendering). If not found, clear `selected_hash`, `selected_index`, and preview state.
- Add `selected_index: Option<usize>` — a transient field computed from `selected_hash` against the current results. This preserves the existing rendering logic that checks `selected_result == Some(i)`.
- Tag assignment (`assign_tag_to_selected`) now looks up the selected document by `selected_hash` rather than `selected_result` index into `search_results`.
- In `do_search()` when tag filters are active: the 200-result limit is removed (U8 will handle this); all matching results get their tags batched.

**Patterns to follow:**
- Existing `SearchResult.content_hash` field in `src/search/query.rs`

**Test scenarios:**
- Happy path: Search "invoice" → select result with hash "abc123" → change query to "receipt" → selection cleared (hash not in new results)
- Happy path: Search "invoice" → select result → change query to "invoice 2025" → same document still in results → selection maintained
- Edge case: Selected document deleted from disk → next search → selection cleared
- Edge case: No search results → `selected_hash` is `None`, tag assignment operations are no-ops
- Integration: Select document, assign tag, re-search with that tag filter → document still selected if it appears

**Verification:**
- Selection persists across searches when the document remains in results
- Selection clears when the document is no longer in results
- Tag operations target the correct document after search query changes

---

### U7. Indexing Progress Semantics

**Goal:** Replace misleading progress display with accurate per-file progress that handles the unknown-total case.

**Requirements:** R8

**Dependencies:** U2 (FolderRuntime owns the progress channel)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/indexer/pipeline.rs`

**Approach:**
- Replace `IndexerProgress` enum variants: remove `Indexed { total }` and `Done { total }`. Add `Progress { processed: usize }` (sent per file) and `ScanComplete { total: usize }` (sent once after initial scan finishes).
- In the pipeline: after each successful `process_upsert()`, send `Progress { processed: self.total_processed }` (increment counter per file, not per commit batch). After the initial scan loop completes (all watcher channel messages drained post-startup), send `ScanComplete { total: self.total_processed }`.
- In the UI: track `files_processed: usize` and `files_discovered: Option<usize>`. On `Progress`, set `files_processed = processed`. On `ScanComplete`, set `files_discovered = Some(total)`. Display: if `files_discovered` is `Some`, show "Indexed 123/500 files"; if `None`, show "Indexed 123 files…".
- Remove `indexing_done` and `indexing_total` fields from `PapervaultApp`. The progress indicator in the search bar updates from `files_processed` and `files_discovered`.

**Patterns to follow:**
- Existing `IndexerProgress` enum in `src/app.rs:27`
- Existing progress send in `src/indexer/pipeline.rs:58`

**Test scenarios:**
- Happy path: Indexer processes 50 files during initial scan → UI shows "Indexed 50 files…"
- Happy path: Initial scan completes → `ScanComplete { total: 50 }` sent → UI shows "Indexing 50/50"
- Happy path: New file added after scan → `Progress { processed: 51 }` sent → UI shows "Indexed 51/50 files" (or just "51 files indexed" if implementation chooses to cap denominator)
- Edge case: Watcher sends 0 files → `ScanComplete { total: 0 }` → UI shows nothing (no files)
- Edge case: Pipeline processes files in batch commits → each file sends its own `Progress`

**Verification:**
- Progress display never shows misleading fractions (e.g., "10/100" after 100 files)
- Initial scan shows "Indexed N files…" until complete, then shows total
- The existing pipeline test (`pipeline.rs`) is updated to use the new progress variants

---

### U8. Tag Filtering N+1 Query Fix

**Goal:** Replace per-result `get_tags_for_document()` calls with a single batch query, removing the 200-result arbitrary cap and N+1 SQLite connection overhead.

**Requirements:** R9, AE4

**Dependencies:** U1 (removes per-frame tag list)

**Files:**
- Modify: `src/tags/store.rs`
- Modify: `src/app.rs`

**Approach:**
- Add method to `TagStore`: `get_tags_for_hashes(hashes: &[String]) -> Result<HashMap<String, Vec<Tag>>>`. SQL: `SELECT dt.content_hash, t.id, t.name FROM document_tags dt JOIN tags t ON dt.tag_id = t.id WHERE dt.content_hash IN (?, ?, ...) ORDER BY t.name`. Build the `IN` clause dynamically with parameterized placeholders. Split `hashes` into chunks of 500 (SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 999). Execute one query per chunk, merge resulting `HashMap`s.
- In `do_search()`: after obtaining `SearchResults`, collect all `content_hash` values from result items. Call `get_tags_for_hashes()` once. Populate each result's `tags` field from the returned map.
- Remove the 200-result limit workaround for tag-filtered searches. With batch tag retrieval, all results can be fetched and post-filtered without N+1 overhead.
- Remove the individual `get_tags_for_document()` calls from the result loop. The `do_search()` method now calls `get_tags_for_hashes()` exactly once.
- Keep the SQLite post-filter for tag accuracy (Tantivy tags may be stale). The batch query is now cheap enough to run for all results.

**Patterns to follow:**
- Existing parameterized query pattern in `src/tags/store.rs` (e.g., `conn.prepare(...)`, binding parameters)
- Existing `HashMap` usage for document-to-tag mapping

**Test scenarios:**
- Happy path: Search returns 50 results → `get_tags_for_hashes()` called once → all results have correct tags
- Happy path: Search with active tag filter → results filtered correctly, no 200-result limit
- Edge case: 0 search results → `get_tags_for_hashes()` not called (empty hashes list → early return)
- Edge case: 500 search results → single batch query with 500 hashes in IN clause → all tags retrieved
- Edge case: Some hashes have no tags → those hashes map to empty Vec in the result map
- Integration: Tag assigned via UI → next search reflects the tag → filtering by that tag returns the tagged document even if ranked below the old 200-result cutoff

**Verification:**
- `do_search()` calls `get_tags_for_hashes()` exactly once per search, not per result
- No arbitrary result cap remains for tag-filtered searches
- The existing tag test `get_documents_with_tag_returns_correct_docs` in `src/tags/store.rs` still passes
- Tag filtering works correctly for documents ranked beyond position 200

---

### U9. Text Preview Size Cap

**Goal:** Prevent UI freezes when previewing large text files by capping preview content at 2MB.

**Requirements:** R10

**Dependencies:** None

**Files:**
- Modify: `src/app.rs`

**Approach:**
- In the text preview loading path (triggered when the user selects a text file result): read the file size via `std::fs::metadata(path)?.len()`. If > 2MB, read only the first 2MB via `std::fs::File::open` + `read_to_string` on a `BufReader` with `take(2_097_152)`.
- After loading: if the file was truncated, set the preview text to the truncated content and append a truncation notice: `"\n\n─── Preview truncated at 2 MB ───"`.
- For consistency, apply the same cap in the indexer pipeline's text extraction (already capped at 10MB for indexing — preview cap is separate and lower).

**Patterns to follow:**
- Existing file reading in `src/indexer/extractors/text.rs` (uses `std::fs::read_to_string`)
- Existing body length cap in `src/indexer/extractors/text.rs` (10MB cap for indexing)

**Test scenarios:**
- Happy path: Select a 500KB text file → full content displayed in preview
- Happy path: Select a 5MB text file → first 2MB displayed + truncation notice at bottom
- Edge case: Empty text file → no content shown, no truncation notice
- Edge case: File exactly 2MB → full content displayed, no truncation notice
- Error path: File cannot be read (permissions) → error displayed in status bar, preview cleared

**Verification:**
- Large text file (>2MB) preview loads instantly without UI freeze
- Truncation notice appears only when content is actually truncated
- Existing text preview functionality for normal-sized files is unchanged

---

### Phase 4: Diagnostics & Polish

### U10. Error Visibility & Atomic Config Save

**Goal:** Surface startup errors in a visible panel rather than silently discarding them, and save config atomically to prevent corruption.

**Requirements:** R11

**Dependencies:** U2 (FolderRuntime provides the error surface)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/config.rs`
- Modify: `src/main.rs`

**Approach:**
- Add `StartupState` enum: `Ready`, `NoFolder`, `Failed { component: &'static str, message: String }`. Store in `PapervaultApp`.
- In `FolderRuntime::start()`: propagate errors via `Result` instead of `.ok()`. Thread spawn failures, engine open failures, and tag store open failures all produce meaningful error messages.
- In `PapervaultApp::new()`: set `StartupState` based on component availability. If any critical component failed, set to `Failed`.
- In the center panel: if `StartupState::Failed`, render a persistent error panel above the normal content showing the component name and error message. Allow the user to dismiss and retry (re-select folder).
- Atomic config save: in `Config::save()`, serialize to `config.json.tmp` in the same directory, then `std::fs::rename(tmp, final)`. On Windows NTFS, `rename` is atomic for same-volume moves. If `rename` fails, keep the old config (no data loss).
- In `main.rs`: replace `TagStore::open_or_create().ok()` with proper error handling — if the tag store fails, set `StartupState::Failed` and surface the error. The app still launches but shows the error panel.
- Replace thread spawn `.ok()` with `.map_err(|e| PapervaultError::Io(e))?` in `FolderRuntime::start()`.

**Patterns to follow:**
- Existing `Config::save()` in `src/config.rs:32-38`
- `std::fs::rename` — atomic on NTFS for same-volume operations
- `PapervaultError` enum in `src/error.rs` — wire up previously dead variants

**Test scenarios:**
- Happy path: All components start successfully → `StartupState::Ready` → normal UI shown
- Happy path: Config saved → `config.json.tmp` written, then renamed → `config.json` updated atomically
- Error path: Tag store DB locked → `StartupState::Failed { component: "tag-store", message: "..." }` → error panel visible
- Error path: Index directory not writable → `StartupState::Failed { component: "search-engine", message: "..." }` → error panel visible
- Edge case: Config save during power loss / crash → either old config OR new config exists, never a truncated file
- Integration: Launch app, see error panel → click "Re-select folder" → choose a valid folder → error cleared, runtime starts

**Verification:**
- All previously discarded errors (`.ok()`, `let _ =`) in the startup path are now propagated
- Config file is never left in a half-written state
- Error panel is visible and actionable when components fail

---

### U11. Diagnostic Improvements & Package Metadata

**Goal:** Enable console output in debug builds, add license and repository metadata.

**Requirements:** R12, R13

**Dependencies:** None

**Files:**
- Modify: `src/main.rs`
- Modify: `Cargo.toml`
- Create: `LICENSE`

**Approach:**
- In `src/main.rs`: change `#![windows_subsystem = "windows"]` to `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`. Debug builds (`cargo run`, `cargo build`) will show a console window with tracing output. Release builds remain GUI-only.
- In `Cargo.toml`: add `license = "MIT"` and `repository = "https://github.com/IvanYang007/papervault"` to the `[package]` section.
- Create `LICENSE` file in the repository root with the full MIT license text. Copy the standard MIT license from https://opensource.org/licenses/MIT.

**Patterns to follow:**
- Existing `#![windows_subsystem = "windows"]` in `src/main.rs:1`
- Existing `[package]` metadata in `Cargo.toml`

**Test scenarios:**
- Happy path: `cargo build` (debug) → binary launches with console window, `tracing` output visible
- Happy path: `cargo build --release` → binary launches without console window (GUI-only)
- Happy path: `cargo publish --dry-run` → validates license and repository fields

**Verification:**
- Debug build shows console output for all `info!`, `warn!`, `error!` log messages
- Release build behavior is unchanged (no console window)
- `Cargo.toml` has valid `license` and `repository` fields
- `LICENSE` file exists in repository root with MIT text

---

## System-Wide Impact

- **Interaction graph:** `FolderRuntime` becomes the central lifecycle owner — replacing the current pattern of discrete fields in `PapervaultApp` and `main.rs`. All channel creation, thread spawning, and resource cleanup routes through it. The existing `watcher` → `indexer` → `UI` channel topology is preserved.
- **Error propagation:** Previously silent errors (thread spawn, tag store, config save) now propagate through `Result` in `FolderRuntime::start/stop` and surface in `StartupState`. Channel send errors remain silent (acceptable — channel closure is a normal shutdown signal, not an error).
- **State lifecycle risks:** `FolderRuntime` must be fully stopped before a new one starts — partial cleanup (e.g., watcher stopped but indexer still running) would orphan threads. The `stop()` → `start()` sequence in `switch_folder` must be strictly sequential.
- **API surface parity:** `SearchEngine::index_directory` changes signature semantics — it now depends on the watched folder path. Tests that create engines without a real watched folder must pass a temp directory path. `init_search_engine()` in `app.rs` is replaced by `FolderRuntime::start()`.
- **Integration coverage:** The full round-trip — start app → select folder → files indexed → search → select result → preview → change folder → results cleared — is the critical integration scenario. No unit test covers this; it must be verified manually or via future integration tests.
- **Unchanged invariants:** The `pipeline::reconcile()` logic is preserved — it still backfills Tantivy → SQLite. The extractor chain, SQLite schema, and Tantivy schema are unchanged. The `TagStore` API surface is extended (batch query added) but existing methods are not modified.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|-----------|
| `FolderRuntime` introduces a new abstraction that centralizes too much state, making it hard to evolve | Keep the struct's fields focused on lifecycle ownership only — channels, handles, engine. The UI state (search results, preview, pagination) stays in `PapervaultApp`. |
| Per-folder index path change breaks existing users' indexes | On first launch after the change, the old `papervault/index/` is silently orphaned. New indexes are created under `papervault/indexes/<hash>/`. This is acceptable for a pre-release tool with no published users. |
| Hash-based index paths prevent manual cleanup / migration | Document the index location in README. Add a future cleanup mechanism to remove indexes for folders that no longer exist on disk. |
| Batch SQL `IN` clause with 500+ hashes may hit SQLite's variable limit | SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 999. For 500+ results, split into chunks of 500. |
| Unicode fix with `is_char_boundary()` skips highlighting for affected characters | Acceptable trade-off — non-ASCII edge case with no crash is better than correct highlighting with a rare panic. Full fix tracked as follow-up. |
| Debug console window may confuse users running `cargo run` | The console displays `tracing` output which is valuable for debugging. Release builds are unaffected. Document in README. |

---

## Documentation / Operational Notes

- Update README: first-launch flow now works (select folder → indexing starts immediately)
- Update README: debug build shows console output for diagnostics
- Update TECHNICAL-HANDOFF.md: document `FolderRuntime`, per-folder indexes, batch tag query pattern
- Index location change: `%LOCALAPPDATA%/papervault/index/` → `%LOCALAPPDATA%/papervault/indexes/<hash>/`

---

## Sources & References

- **Origin document:** `docs/brainstorms/2026-01-15-pdf-search-viewer-requirements.md`
- **Related plan:** `docs/plans/2026-01-15-002-fix-p0-p1-issues-plan.md` (P0+P1 fixes — some overlap with U4 Unwrap Safety, U6 Zero-Results, U14 Read-Once)
- **Handoff doc:** `docs/TECHNICAL-HANDOFF.md`
- **AI Code Review:** User-provided consolidated review (July 2026)
- Related code: `src/main.rs` (thread orchestration), `src/app.rs` (UI and search), `src/search/engine.rs` (index lifecycle), `src/indexer/pipeline.rs` (indexing loop), `src/watcher/watcher.rs` (file watching), `src/tags/store.rs` (SQLite tag storage), `src/preview/pdf_render.rs` (PDF rendering), `src/config.rs` (config persistence)
