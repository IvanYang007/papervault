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

/// Background PDF renderer with page cache, two-pass rendering, and prefetch support.
pub struct PdfRenderer {
    request_rx: Receiver<RenderRequest>,
    result_tx: Sender<RenderResult>,
    pdfium: Option<pdfium_render::prelude::Pdfium>,
    cached_bytes: Option<(PathBuf, Vec<u8>)>,
    /// LRU page cache: most-recently-used entry is at the front.
    page_cache: VecDeque<(PageCacheKey, Arc<Vec<u8>>, u32, u32)>,
}

impl PdfRenderer {
    pub fn new(request_rx: Receiver<RenderRequest>, result_tx: Sender<RenderResult>) -> Self {
        // Eagerly initialize pdfium so the first render is instant.
        let pdfium = Self::init_pdfium();
        if pdfium.is_some() {
            info!("Pdfium initialized eagerly on renderer thread");
        }
        Self {
            request_rx,
            result_tx,
            pdfium,
            cached_bytes: None,
            page_cache: VecDeque::new(),
        }
    }

    /// Try to bind pdfium from the exe directory or system path.
    /// Public so FolderRuntime can pre-warm pdfium during startup.
    pub fn init_pdfium() -> Option<pdfium_render::prelude::Pdfium> {
        let dll_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let dll_path = dll_dir.join("pdfium.dll");
        let bindings = pdfium_render::prelude::Pdfium::bind_to_library(&dll_path)
            .or_else(|_| pdfium_render::prelude::Pdfium::bind_to_system_library())
            .ok()?;
        Some(pdfium_render::prelude::Pdfium::new(bindings))
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

    /// Ensure file bytes are cached for the given path.
    fn ensure_bytes(&mut self, path: &PathBuf) -> anyhow::Result<()> {
        let cache_hit = self
            .cached_bytes
            .as_ref()
            .is_some_and(|(cached_path, _)| cached_path == path);

        if cache_hit {
            return Ok(());
        }

        let metadata = std::fs::metadata(path)
            .with_context(|| format!("Failed to stat: {}", path.display()))?;
        let file_size = metadata.len();

        if file_size > BYTE_CACHE_MAX_BYTES {
            warn!(
                "PDF {} is {:.1} MB — exceeds cache limit",
                path.display(),
                file_size as f64 / (1024.0 * 1024.0)
            );
        }

        let bytes =
            std::fs::read(path).with_context(|| format!("Failed to read: {}", path.display()))?;

        // Path changed — clear page cache since old pages are for a different file
        self.page_cache.clear();
        self.cached_bytes = Some((path.clone(), bytes));
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
        let (_, bytes) = self.cached_bytes.as_ref().expect("bytes cached");

        let doc = pdfium
            .load_pdf_from_byte_slice(bytes, None)
            .context("Failed to load PDF")?;

        let pages = doc.pages();
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
        self.ensure_bytes(&request.path)?;
        let result = self.do_render(request, is_preview);

        // Evict oversized byte cache
        if let Some((_, ref cached)) = self.cached_bytes {
            if cached.len() as u64 > BYTE_CACHE_MAX_BYTES {
                self.cached_bytes = None;
            }
        }

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
