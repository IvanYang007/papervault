---
title: fix: Debug PDF Renderer Thread Startup — Thread Never Logs
type: fix
status: active
date: 2026-07-21
---

# fix: Debug PDF Renderer Thread Startup — Thread Never Logs

## Summary

Diagnose why the PDF renderer thread produces no log output ("PDF renderer started" never appears) and the UI immediately shows a fallback message. The most likely root cause is a startup pdfium availability probe failing due to CWD-relative DLL path resolution, which gates the renderer thread spawn. Secondary theories: dummy channel wiring from the no-folder startup path, and the renderer thread panicking before its first log statement.

---

## Problem Frame

When the app launches with a configured watched folder, the UI immediately displays "PDF preview not available. Place pdfium.dll next to papervault.exe." The renderer thread's startup log message ("PDF renderer started") never appears, and no renderer messages appear when PDFs are clicked. The pdfium.dll (Chromium 7543, 5.8MB, x64) is correctly placed next to the executable and is verified to load successfully in unit tests. The `browse_file()` method successfully sends render requests, but the renderer never receives them — suggesting either the renderer thread was never spawned, or the render_tx channel is a stale dummy from the no-folder startup path.

---

## Requirements

- R1. Prove whether the renderer thread is spawned at all using `eprintln!` (bypassing the tracing subscriber).
- R2. Identify what code path sets the "PDF preview not available" fallback and whether it gates the renderer spawn.
- R3. Verify that `render_tx` in PapervaultApp is the live channel from the active FolderRuntime, not a stale dummy.
- R4. Fix the DLL path resolution to use the executable directory rather than the current working directory.
- R5. Ensure the renderer thread is spawned unconditionally and reports errors through the channel rather than blocking spawn.

---

## Scope Boundaries

- The plan diagnoses and fixes the renderer thread startup and channel wiring only.
- pdfium.dll acquisition (U1 from the prior plan) is already complete — the correct Chromium 7543 DLL is in place.
- Step-level renderer diagnostics (U2 from prior plan) and error hardening (U3) are already implemented.
- UI layout, search, file browser, text preview, and indexing are unchanged.

---

## Context & Research

### Relevant Code

- **Renderer spawn:** `src/runtime.rs` — `FolderRuntime::start()` spawns the renderer thread via `std::thread::Builder::new().name("renderer").spawn(...)` and stores the `JoinHandle` in `renderer_handle`. The thread closure calls `PdfRenderer::new(render_rx, render_result_tx).run()`.
- **Startup channel wiring:** `src/main.rs` — if `config.watched_folder` is set, `FolderRuntime::start()` creates channels. Otherwise, dummy `bounded(1)` channels are created. Both real and dummy channels are passed to `PapervaultApp::new()`.
- **Fallback message:** `src/app.rs` — center panel shows "PDF preview not available" when `browsed_file` ends with `.pdf` and `preview_texture` is `None`.
- **DLL binding:** `src/preview/pdf_render.rs:78` — `Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name())` which on Windows returns `"pdfium.dll"` (bare filename, searched relative to the exe directory by `LoadLibraryW`).
- **browse_file:** `src/app.rs` — sends `RenderRequest` via `self.render_request_tx`.

### Key Insight from AI Feedback

The most probable single explanation is a **Pattern A + B combo**:
- A UI-side pdfium availability probe using a `./pdfium.dll` path fails because CWD ≠ exe dir
- The renderer spawn is gated behind that probe → thread never starts
- The fallback message is shown immediately, not after a timeout

The secondary theory: the no-folder startup path in `main.rs` creates dummy channels, and when the user sets a folder later, the new FolderRuntime's channels replace the dummies in `update()`, but the initial startup path (when config already has a watched folder) may have a wiring defect.

---

## Implementation Units

### U1. Prove Renderer Thread Existence with eprintln

**Goal:** Bypass the tracing subscriber entirely to confirm whether the renderer thread is spawned, alive, and receiving requests.

**Requirements:** R1

**Dependencies:** None

**Files:**
- Modify: `src/runtime.rs`
- Modify: `src/preview/pdf_render.rs`
- Modify: `src/main.rs`

**Approach:**
- In `main.rs`, add a global panic hook before anything else:
  ```rust
  std::panic::set_hook(Box::new(|info| {
      eprintln!("!!! PANIC in thread {:?}: {info}", std::thread::current().id());
  }));
  ```
- In `runtime.rs`, add `eprintln!` before and after spawning the renderer thread, and inside the thread closure as the very first statement:
  ```rust
  eprintln!(">>> about to spawn renderer");
  // ... spawn ...
  eprintln!(">>> renderer thread ALIVE, tid={:?}", std::thread::current().id());
  ```
- In `pdf_render.rs`, add `eprintln!(">>> renderer entering recv loop")` as the first line of `PdfRenderer::run()`, before any `info!()`.
- Store the renderer `JoinHandle` and log `handle.is_finished()` from `poll_channels()` at most once.
- In `browse_file()`, log the result of `render_tx.send()`:
  ```rust
  match self.render_request_tx.as_ref().map(|tx| tx.send(req)) {
      Some(Ok(_)) => eprintln!(">>> render request SENT"),
      Some(Err(e)) => eprintln!(">>> render channel DEAD: {e}"),
      None => eprintln!(">>> render_tx is None"),
  }
  ```

**Test scenarios:**
- "about to spawn renderer" prints but "ALIVE" doesn't → spawn site is reached but thread panics/aborts before first line; panic hook should catch it
- "ALIVE" prints but "entering recv loop" doesn't → PdfRenderer::new() itself hangs or panics during construction
- "entering recv loop" prints but "render request SENT" never appears → browse_file not called or guard fails
- "render request SENT" prints but renderer doesn't log receipt → channel mismatch (sender reaches a different channel than the renderer reads)
- None of the spawn-site messages print → spawn site is never reached (U2 covers this)

**Verification:**
- All `eprintln!` messages appear in console output in the expected order
- If renderer thread panics, the panic hook captures and prints the location

---

### U2. Trace the Fallback Message Origin

**Goal:** Identify exactly what code path sets the condition that causes "PDF preview not available" to display, and whether that code gates the renderer spawn.

**Requirements:** R2

**Dependencies:** U1 (we need to know if the thread even exists)

**Files:**
- Modify: `src/app.rs`

**Approach:**
- The fallback displays when `browsed_file.as_ref().map_or(false, |f| f.to_lowercase().ends_with(".pdf"))` AND `preview_texture` is `None`.
- `preview_texture` is only set in `poll_channels()` when a render result arrives with `width > 0`.
- If the renderer thread is never spawned (U1 confirms), the texture is never set → fallback always shows.
- Search for any other code path that checks pdfium availability before spawning — specifically look for `Pdfium::bind_to_library` or pdfium probe calls outside the renderer thread.
- If U1 shows the renderer IS alive but the channel is dead, verify `render_tx` wiring in the FolderRuntime creation and channel handoff.

**Test scenarios:**
- Spawn site unreached → add `eprintln!(">>> FolderRuntime::start called")` at the top of `FolderRuntime::start`
- Spawn site reached but dummy channel → verify `render_tx` sent to PapervaultApp is from the active runtime

**Verification:**
- The exact code path producing the fallback is identified
- If a probe gates the spawn, the probe is removed and spawn is unconditional

---

### U3. Fix DLL Path Resolution

**Goal:** Ensure `Pdfium::bind_to_library()` resolves `pdfium.dll` relative to the executable directory, not the current working directory.

**Requirements:** R4

**Dependencies:** U1 (verify renderer is alive and reaches the bind call)

**Files:**
- Modify: `src/preview/pdf_render.rs`

**Approach:**
- Replace `Pdfium::pdfium_platform_library_name()` (returns bare `"pdfium.dll"`) with an absolute path built from `std::env::current_exe()`:
  ```rust
  let dll_dir = std::env::current_exe()
      .ok()
      .and_then(|p| p.parent().map(|d| d.to_path_buf()))
      .unwrap_or_else(|| std::path::PathBuf::from("."));
  let dll_path = dll_dir.join("pdfium.dll");
  ```
- Use `Pdfium::bind_to_library(dll_path)` instead of `Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name())`.
- Apply the same fix to the indexer's `PdfExtractor` in `src/indexer/extractors/pdf.rs` for consistency.

**Test scenarios:**
- App launched from `C:\Users\kaipi` (CWD ≠ exe dir) → DLL loads from exe directory
- App launched from `D:\Github\papervault\target\debug` → DLL loads from same directory (no regression)
- `current_exe()` fails (unlikely on Windows) → falls back to `"."` gracefully

**Verification:**
- "Binding pdfium library..." message appears after clicking PDF, regardless of CWD
- `Pdfium::new()` succeeds → "Pdfium instance created" appears

---

### U4. Spawn Renderer Unconditionally

**Goal:** The renderer thread must be spawned regardless of pdfium availability. Bind failures are reported through the result channel, not by gating the spawn.

**Requirements:** R5

**Dependencies:** U2 (identify any spawn-gating code)

**Files:**
- Modify: `src/runtime.rs` (or wherever spawn gating exists)
- Modify: `src/preview/pdf_render.rs`

**Approach:**
- Remove any pdfium availability check that gates the renderer spawn in `FolderRuntime::start()`.
- The renderer thread is spawned unconditionally.
- If `Pdfium::bind_to_library()` fails, the error is caught in `render_page()`, sent as an empty `RenderResult` (width=0) to the UI, and displayed as a meaningful error (e.g., "PDF rendering unavailable: Failed to bind pdfium library").
- The `Pdfium::new()` call remains inside the renderer thread (not shared across threads, since pdfium types are not `Send`).

**Test scenarios:**
- Renderer spawns → "renderer thread ALIVE" prints → pdfium bind fails → error result sent → UI shows specific error
- Renderer spawns → pdfium bind succeeds → render works → preview displayed

**Verification:**
- "PDF renderer started" log appears at app startup regardless of pdfium availability
- No pdfium probe exists outside the renderer thread

---

## System-Wide Impact

- **Renderer thread lifecycle:** The renderer becomes an always-present thread, not conditionally spawned. `FolderRuntime::stop()` already handles joining it.
- **Error propagation:** Pdfium bind failures now flow through the existing render error path (empty `RenderResult` → UI fallback with specific message).
- **DLL path:** Switching from bare filename to absolute exe-relative path makes the DLL resolution independent of the current working directory.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|-----------|
| Renderer thread consumes resources even when pdfium is unavailable | The thread is lightweight when idle (blocked on `recv()`). Acceptable trade-off for deterministic behavior. |
| `current_exe()` returns an unexpected path on some Windows configurations | Fall back to bare `"pdfium.dll"` if the exe directory can't be determined |

---

## Sources & References

- AI diagnostic feedback (July 2026) — structured debugging plan with 5 steps
- `src/preview/pdf_render.rs` — renderer thread implementation
- `src/runtime.rs` — FolderRuntime::start() thread spawning
- `src/main.rs` — initial runtime creation and channel wiring
- `src/app.rs` — browse_file(), poll_channels(), fallback display
