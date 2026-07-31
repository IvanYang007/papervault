use anyhow::Context;
use crossbeam::channel::{Receiver, Sender};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::app::{RenderRequest, RenderResult};

/// Maximum file size to cache in memory (200 MB).
const BYTE_CACHE_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Maximum number of rendered page bitmaps to keep in the LRU cache.
const MAX_CACHED_PAGES: usize = 8;

/// Cache key for rendered page bitmaps: (path, page, zoom_percent).
type PageCacheKey = (PathBuf, usize, u32);

/// Process-wide pdfium handle, initialized exactly once (a second
/// FPDF_InitLibrary would block forever on pdfium's global marshall lock).
/// Stored as an atomic usize because Pdfium is !Sync; every access happens on
/// the renderer thread. Once provides the happens-before for the store.
static PDFIUM_INIT: std::sync::Once = std::sync::Once::new();
static PDFIUM_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Background PDF renderer with page cache, two-pass rendering, and prefetch support.
pub struct PdfRenderer {
    request_rx: Receiver<RenderRequest>,
    result_tx: Sender<RenderResult>,
    /// Leaked once — PdfDocument borrows the bindings, so the Pdfium must
    /// outlive every cached document. Intentional leak: the OS cleans up at
    /// process exit (same pattern as the single-instance mutex handle).
    pdfium: Option<&'static pdfium_render::prelude::Pdfium>,
    /// Currently loaded PDF document, keyed by path. Reused across page/zoom
    /// renders so the PDF is parsed once per file instead of once per render.
    cached_doc: Option<(PathBuf, pdfium_render::prelude::PdfDocument<'static>)>,
    /// Files larger than this are rendered per-request without caching.
    byte_cache_max_bytes: u64,
    /// LRU page cache: most-recently-used entry is at the front.
    page_cache: VecDeque<(PageCacheKey, Arc<Vec<u8>>, u32, u32)>,
}

impl PdfRenderer {
    pub fn new(request_rx: Receiver<RenderRequest>, result_tx: Sender<RenderResult>) -> Self {
        Self::new_with_cache_limit(request_rx, result_tx, BYTE_CACHE_MAX_BYTES)
    }

    /// Renderer with an overridable per-file cache limit (tests force the
    /// oversized-fallback path with a normal-size fixture).
    pub fn new_with_cache_limit(
        request_rx: Receiver<RenderRequest>,
        result_tx: Sender<RenderResult>,
        byte_cache_max_bytes: u64,
    ) -> Self {
        // Eagerly initialize pdfium so the first render is instant.
        let pdfium = Self::init_pdfium();
        if pdfium.is_some() {
            info!("Pdfium initialized eagerly on renderer thread");
        }
        Self {
            request_rx,
            result_tx,
            pdfium,
            cached_doc: None,
            byte_cache_max_bytes,
            page_cache: VecDeque::new(),
        }
    }

    /// Try to bind pdfium from the exe directory or system path.
    /// `PAPERVAULT_PDFIUM_DLL` overrides the DLL location (used by tests).
    /// The library is initialized exactly once per process and leaked —
    /// PdfDocument borrows the bindings, so the Pdfium must outlive every
    /// document (same pattern as the single-instance mutex handle).
    /// Public so FolderRuntime can pre-warm pdfium during startup.
    pub fn init_pdfium() -> Option<&'static pdfium_render::prelude::Pdfium> {
        PDFIUM_INIT.call_once(|| {
            let dll_path = std::env::var_os("PAPERVAULT_PDFIUM_DLL")
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .or_else(|| {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.join("pdfium.dll")))
                        .filter(|p| p.exists())
                });
            let bindings = match dll_path {
                Some(path) => pdfium_render::prelude::Pdfium::bind_to_library(&path)
                    .or_else(|_| pdfium_render::prelude::Pdfium::bind_to_system_library()),
                None => pdfium_render::prelude::Pdfium::bind_to_system_library(),
            }
            .ok();
            if let Some(bindings) = bindings {
                let pdfium = pdfium_render::prelude::Pdfium::new(bindings);
                // SAFETY: Box::into_raw leaks the Pdfium deliberately (it must
                // outlive all PdfDocuments). The pointer is created once, never
                // freed, aligned and non-null; the Once store/release pair plus
                // the Acquire load make it visible to later threads. All
                // dereferences happen on the single renderer thread while the
                // pointer stays valid.
                PDFIUM_PTR.store(
                    Box::into_raw(Box::new(pdfium)) as usize,
                    std::sync::atomic::Ordering::Release,
                );
            }
        });
        let ptr = PDFIUM_PTR.load(std::sync::atomic::Ordering::Acquire);
        if ptr == 0 {
            None
        } else {
            // SAFETY: see the safety comment at the store site above.
            Some(unsafe { &*(ptr as *const pdfium_render::prelude::Pdfium) })
        }
    }

    /// Run the render loop (blocks until channel closes).
    pub fn run(&mut self) {
        info!("PDF renderer started");

        #[allow(clippy::while_let_loop)]
        loop {
            // Check for normal-priority requests first (coalesce, latest-wins)
            let request = match self.request_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(req) => req,
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
            };
            if request.priority == 0 {
                // Prefetch request — only process if no normal request waiting
                if self.request_rx.is_empty() {
                    let _ = self.render_and_send(&request, false);
                }
                continue;
            }

            // Coalesce: drain stale normal-priority requests, keep only the latest
            let mut request = request;
            while let Ok(newer) = self.request_rx.try_recv() {
                if newer.priority >= 1 {
                    request = newer;
                }
            }

            // Two-pass: send low-res preview first, then full-res
            self.render_two_pass(&request);
        }

        info!("PDF renderer stopped");
    }

    /// Two-pass render: low-res preview → full-res replacement.
    fn render_two_pass(&mut self, request: &RenderRequest) {
        // Pass 1: low-res (1/4 dimensions)
        let preview_result = self.render_and_send(request, true);
        if preview_result {
            // Collect the latest normal-priority request that arrived during preview.
            // Don't discard it — render it so the user's navigation is respected.
            let mut latest: Option<RenderRequest> = None;
            while let Ok(newer) = self.request_rx.try_recv() {
                if newer.priority >= 1 {
                    latest = Some(newer);
                }
            }
            if let Some(new_req) = latest {
                // User navigated while preview was rendering — render the new page
                let _ = self.render_and_send(&new_req, true);
                let _ = self.render_and_send(&new_req, false);
                return;
            }
            // No newer request — render full-res of the original page
            let _ = self.render_and_send(request, false);
        }
    }

    /// Render and send result. `is_preview`: use 1/4 dimensions for speed.
    /// Returns true if the render was sent successfully.
    fn render_and_send(&mut self, request: &RenderRequest, is_preview: bool) -> bool {
        match self.render_page(request, is_preview) {
            Ok(result) => self.result_tx.send(result).is_ok(),
            Err(e) => {
                error!("Render error for {}: {}", request.path.display(), e);
                let _ = self.result_tx.send(RenderResult {
                    request_id: request.request_id,
                    path: request.path.clone(),
                    page: request.page,
                    page_count: 0,
                    rgba_bytes: Vec::new(),
                    width: 0,
                    height: 0,
                    is_preview: false,
                });
                false
            }
        }
    }

    /// Ensure pdfium is initialized.
    fn ensure_pdfium(&mut self) -> anyhow::Result<()> {
        if self.pdfium.is_none() {
            self.pdfium = Self::init_pdfium();
        }
        if self.pdfium.is_none() {
            anyhow::bail!("Pdfium library not available — place pdfium.dll next to papervault.exe");
        }
        Ok(())
    }

    /// Ensure the PDF document for the given path is loaded and cached.
    /// The parsed document is reused across page/zoom renders — the PDF is
    /// parsed once per file instead of once per render request.
    fn ensure_document(&mut self, path: &PathBuf) -> anyhow::Result<()> {
        let cache_hit = self
            .cached_doc
            .as_ref()
            .is_some_and(|(cached_path, _)| cached_path == path);

        if cache_hit {
            return Ok(());
        }

        let metadata = std::fs::metadata(path)
            .with_context(|| format!("Failed to stat: {}", path.display()))?;
        let file_size = metadata.len();
        let oversized = file_size > self.byte_cache_max_bytes;

        if oversized {
            warn!(
                "PDF {} is {:.1} MB — exceeds cache limit",
                path.display(),
                file_size as f64 / (1024.0 * 1024.0)
            );
            // Don't parse here — do_render loads oversized files once per
            // render (fallback path). Parsing twice (once to discard, once
            // to render) would double the cost of the slow path.
            self.page_cache.clear();
            self.cached_doc = None;
            return Ok(());
        }

        let bytes =
            std::fs::read(path).with_context(|| format!("Failed to read: {}", path.display()))?;

        let pdfium = self.pdfium.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Pdfium library not available — place pdfium.dll next to papervault.exe"
            )
        })?;
        let doc = pdfium
            .load_pdf_from_byte_vec(bytes, None)
            .context("Failed to load PDF")?;

        // Path changed — clear page cache since old pages are for a different file
        self.page_cache.clear();
        // Oversized files load per render to bound memory — do not cache them.
        self.cached_doc = if oversized {
            None
        } else {
            Some((path.clone(), doc))
        };
        Ok(())
    }

    /// Compute render dimensions from the request.
    fn compute_dimensions(&self, request: &RenderRequest, is_preview: bool) -> (usize, usize) {
        let zoom = (request.zoom as f64).clamp(0.25, 4.0);
        let (w, h) = if request.target_width > 0 && request.target_height > 0 {
            (
                (request.target_width as f64 * zoom) as usize,
                (request.target_height as f64 * zoom) as usize,
            )
        } else {
            // Fallback: fixed max dimension when no target size is provided
            let max_dim = (2000.0 * zoom) as usize;
            (max_dim, max_dim)
        };

        if is_preview {
            ((w / 4).max(1), (h / 4).max(1))
        } else {
            (w.max(1), h.max(1))
        }
    }

    /// Check the page cache for a matching render.
    /// Moves the found entry to the front (most-recently-used position).
    fn cache_lookup(&mut self, request: &RenderRequest) -> Option<(Arc<Vec<u8>>, u32, u32)> {
        let zoom_pct = (request.zoom * 100.0) as u32;
        let key = (request.path.clone(), request.page, zoom_pct);
        let pos = self.page_cache.iter().position(|(k, _, _, _)| *k == key);
        match pos {
            Some(idx) => {
                // Move to front to maintain true LRU ordering
                let entry = self.page_cache.remove(idx).unwrap();
                let (_, bytes, w, h) = &entry;
                let result = (Arc::clone(bytes), *w, *h);
                self.page_cache.push_front(entry);
                Some(result)
            }
            None => None,
        }
    }

    /// Insert a rendered page into the cache, evicting the oldest if needed.
    fn cache_insert(&mut self, request: &RenderRequest, bytes: Vec<u8>, width: u32, height: u32) {
        let zoom_pct = (request.zoom * 100.0) as u32;
        let key = (request.path.clone(), request.page, zoom_pct);

        // Remove existing entry for this key if present (move to front on re-insert)
        self.page_cache.retain(|(k, _, _, _)| *k != key);

        self.page_cache
            .push_front((key, Arc::new(bytes), width, height));
        if self.page_cache.len() > MAX_CACHED_PAGES {
            self.page_cache.pop_back();
        }
    }

    /// Immutable render phase.
    fn do_render(&self, request: &RenderRequest, is_preview: bool) -> anyhow::Result<RenderResult> {
        let pdfium = self.pdfium.as_ref().expect("pdfium initialized");

        // Use the cached document (parsed once per file). Oversized files fall
        // back to a per-render load to bound memory.
        let doc;
        let doc_ref = match self.cached_doc.as_ref() {
            Some((_, cached)) => cached,
            None => {
                let bytes = std::fs::read(&request.path)
                    .with_context(|| format!("Failed to read: {}", request.path.display()))?;
                doc = pdfium
                    .load_pdf_from_byte_vec(bytes, None)
                    .context("Failed to load PDF")?;
                &doc
            }
        };

        let pages = doc_ref.pages();
        if pages.is_empty() {
            return Ok(RenderResult {
                request_id: request.request_id,
                path: request.path.clone(),
                page: request.page,
                page_count: 0,
                rgba_bytes: Vec::new(),
                width: 0,
                height: 0,
                is_preview,
            });
        }

        let page_count = pages.len();
        let page_idx: pdfium_render::prelude::PdfPageIndex =
            ((request.page.max(1) - 1) as u16).min(page_count.saturating_sub(1));
        let page = pages.get(page_idx).context("Failed to get page")?;

        let (render_width, render_height) = self.compute_dimensions(request, is_preview);

        // Fit to page aspect ratio if we have display dimensions
        let page_w = page.width().value as usize;
        let page_h = page.height().value as usize;
        let (final_w, final_h) = if request.target_width > 0 && request.target_height > 0 {
            let scale_w = render_width as f64 / page_w as f64;
            let scale_h = render_height as f64 / page_h as f64;
            let scale = scale_w.min(scale_h);
            (
                (page_w as f64 * scale) as usize,
                (page_h as f64 * scale) as usize,
            )
        } else {
            (render_width, render_height)
        };

        let render_config = pdfium_render::prelude::PdfRenderConfig::new()
            .set_target_width(final_w as i32)
            .set_target_height(final_h as i32);
        let bitmap = page
            .render_with_config(&render_config)
            .context("Failed to render page")?;

        let rgba_bytes = bitmap.as_rgba_bytes();

        Ok(RenderResult {
            request_id: request.request_id,
            path: request.path.clone(),
            page: request.page,
            page_count: page_count as usize,
            rgba_bytes,
            width: final_w,
            height: final_h,
            is_preview,
        })
    }

    fn render_page(
        &mut self,
        request: &RenderRequest,
        is_preview: bool,
    ) -> anyhow::Result<RenderResult> {
        // Check page cache first (only for full-res renders, not previews)
        if !is_preview {
            if let Some((bytes, w, h)) = self.cache_lookup(request) {
                debug!(
                    "Page cache hit: {} page {}",
                    request.path.display(),
                    request.page
                );
                return Ok(RenderResult {
                    request_id: request.request_id,
                    path: request.path.clone(),
                    page: request.page,
                    page_count: 0, // unknown from cache, UI fills from last known
                    rgba_bytes: Arc::unwrap_or_clone(bytes),
                    width: w as usize,
                    height: h as usize,
                    is_preview: false,
                });
            }
        }

        // Render
        self.ensure_pdfium()?;
        self.ensure_document(&request.path)?;
        let result = self.do_render(request, is_preview);

        // Cache the result if it's a full-res render
        if let Ok(ref r) = result {
            if !is_preview && r.width > 0 {
                self.cache_insert(
                    request,
                    r.rgba_bytes.clone(),
                    r.width as u32,
                    r.height as u32,
                );
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam::channel;

    /// pdfium is a process-global resource (init holds its marshall lock for
    /// the process lifetime). Tests touching pdfium must run one at a time.
    static PDFIUM_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_pdfium() -> std::sync::MutexGuard<'static, ()> {
        PDFIUM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Generate a 3-page searchable PDF via printpdf (dev-dependency).
    fn generate_pdf(path: &std::path::Path) {
        use printpdf::*;
        use std::io::BufWriter;
        let (doc, page1_idx, layer1_idx) =
            PdfDocument::new("Test", Mm(210.0), Mm(297.0), "Layer 1");
        let font = doc.add_builtin_font(BuiltinFont::Helvetica).unwrap();
        let layer = doc.get_page(page1_idx).get_layer(layer1_idx);
        layer.use_text("page one", 12.0, Mm(10.0), Mm(280.0), &font);
        for i in 2..=3 {
            let (page_idx, layer_idx) = doc.add_page(Mm(210.0), Mm(297.0), format!("Page {}", i));
            let layer = doc.get_page(page_idx).get_layer(layer_idx);
            layer.use_text(format!("page {}", i), 12.0, Mm(10.0), Mm(280.0), &font);
        }
        doc.save(&mut BufWriter::new(std::fs::File::create(path).unwrap()))
            .unwrap();
    }

    /// Point the renderer at the repo's release DLL; false when it is missing.
    /// NOTE: must not create/drop a Pdfium here — init/destroy cycles would
    /// destroy the library under the renderer's leaked instance.
    fn require_pdfium() -> bool {
        std::env::set_var("PAPERVAULT_PDFIUM_DLL", "target/release/pdfium.dll");
        std::path::Path::new("target/release/pdfium.dll").exists()
    }

    fn make_renderer() -> PdfRenderer {
        let (_tx, rx) = channel::unbounded::<RenderRequest>();
        let (_result_tx, _result_rx) = channel::unbounded::<RenderResult>();
        PdfRenderer::new(rx, _result_tx)
    }

    fn render_request(path: PathBuf, page: usize) -> RenderRequest {
        RenderRequest {
            request_id: 1,
            path,
            page,
            zoom: 1.0,
            target_width: 400,
            target_height: 600,
            priority: 1,
        }
    }

    #[test]
    fn cached_document_reused_across_page_renders() {
        if !require_pdfium() {
            eprintln!("SKIP: pdfium.dll not available");
            return;
        }
        let _guard = lock_pdfium();
        let dir = tempfile::TempDir::new().unwrap();
        let pdf_path = dir.path().join("doc.pdf");
        generate_pdf(&pdf_path);

        let mut renderer = make_renderer();

        // First render parses the PDF once and caches the parsed document.
        let r1 = renderer
            .render_page(&render_request(pdf_path.clone(), 1), false)
            .unwrap();
        assert_eq!(r1.page_count, 3, "3-page fixture must report 3 pages");
        assert!(r1.width > 0 && r1.height > 0);
        assert!(
            renderer.cached_doc.is_some(),
            "parsed document must be cached after the first render"
        );
        assert_eq!(renderer.cached_doc.as_ref().unwrap().0, pdf_path);

        // Second render (different page) must reuse the cached document —
        // a stale or re-parsed doc would still render, but the cache must
        // still point at this path with the same cached entry.
        let r2 = renderer
            .render_page(&render_request(pdf_path.clone(), 2), false)
            .unwrap();
        assert_eq!(r2.page_count, 3);
        assert!(r2.width > 0 && r2.height > 0);
        assert_eq!(renderer.cached_doc.as_ref().unwrap().0, pdf_path);
    }

    #[test]
    fn document_swap_reloads_doc_and_clears_page_cache() {
        if !require_pdfium() {
            eprintln!("SKIP: pdfium.dll not available");
            return;
        }
        let _guard = lock_pdfium();
        let dir = tempfile::TempDir::new().unwrap();
        let pdf_a = dir.path().join("a.pdf");
        let pdf_b = dir.path().join("b.pdf");
        generate_pdf(&pdf_a);
        generate_pdf(&pdf_b);

        let mut renderer = make_renderer();
        renderer
            .render_page(&render_request(pdf_a.clone(), 1), false)
            .unwrap();
        assert_eq!(renderer.cached_doc.as_ref().unwrap().0, pdf_a);

        // Rendering a different file must swap the cached document.
        renderer
            .render_page(&render_request(pdf_b.clone(), 1), false)
            .unwrap();
        assert_eq!(renderer.cached_doc.as_ref().unwrap().0, pdf_b);
        // The stale page entry for the old document must be evicted; the cache
        // now holds only the new document's rendered page.
        assert_eq!(
            renderer.page_cache.len(),
            1,
            "old pages must be evicted when the document changes"
        );
        let (key, ..) = renderer.page_cache.front().unwrap();
        assert_eq!(
            key.0, pdf_b,
            "remaining page must belong to the new document"
        );
    }

    #[test]
    fn oversized_files_use_per_render_load_without_caching() {
        if !require_pdfium() {
            eprintln!("SKIP: pdfium.dll not available");
            return;
        }
        let _guard = lock_pdfium();
        let dir = tempfile::TempDir::new().unwrap();
        let pdf_path = dir.path().join("big.pdf");
        generate_pdf(&pdf_path);

        // Limit of 1 byte forces every file onto the oversized path.
        let (_tx, rx) = channel::unbounded::<RenderRequest>();
        let (_result_tx, _result_rx) = channel::unbounded::<RenderResult>();
        let mut renderer = PdfRenderer::new_with_cache_limit(rx, _result_tx, 1);

        // Render must succeed through the per-render fallback load.
        let r1 = renderer
            .render_page(&render_request(pdf_path.clone(), 1), false)
            .unwrap();
        assert_eq!(r1.page_count, 3);
        assert!(r1.width > 0 && r1.height > 0);
        assert!(
            renderer.cached_doc.is_none(),
            "oversized files must not be cached"
        );
        assert_eq!(
            renderer.page_cache.len(),
            1,
            "rendered pages still populate the LRU page cache"
        );

        // A second render on the same path keeps working (no stale state).
        let r2 = renderer
            .render_page(&render_request(pdf_path, 2), false)
            .unwrap();
        assert!(r2.width > 0 && r2.height > 0);
    }

    /// Measurement: page render with (warm) and without (cold) document re-parse.
    /// Run with `cargo test --release pdf_document_cache_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn pdf_document_cache_timing() {
        if !require_pdfium() {
            eprintln!("SKIP: pdfium.dll not available");
            return;
        }
        let _guard = lock_pdfium();
        use std::time::Instant;
        let dir = tempfile::TempDir::new().unwrap();
        let pdf_a = dir.path().join("a.pdf");
        let pdf_b = dir.path().join("b.pdf");
        generate_pdf(&pdf_a);
        generate_pdf(&pdf_b);

        let mut renderer = make_renderer();

        // Cold: page 1 — includes PDF parse.
        let start = Instant::now();
        renderer
            .render_page(&render_request(pdf_a.clone(), 1), false)
            .unwrap();
        let cold = start.elapsed();

        // Warm: page 2 — cached document, render only.
        let start = Instant::now();
        renderer
            .render_page(&render_request(pdf_a.clone(), 2), false)
            .unwrap();
        let warm = start.elapsed();

        // Cold again on a second file (forces re-parse).
        let start = Instant::now();
        renderer
            .render_page(&render_request(pdf_b.clone(), 1), false)
            .unwrap();
        let cold2 = start.elapsed();

        eprintln!(
            "cold page render (with parse): {:?}, warm page render (cached doc): {:?}, \
             cold-2nd-file: {:?}, speedup {:.1}x",
            cold,
            warm,
            cold2,
            cold.as_secs_f64() / warm.as_secs_f64().max(1e-9)
        );
    }
}
