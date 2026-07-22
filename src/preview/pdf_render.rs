use anyhow::Context;
use crossbeam::channel::{Receiver, Sender};
use std::path::PathBuf;
use tracing::{debug, error, info, warn};

use crate::app::{RenderRequest, RenderResult};

/// Maximum file size to cache in memory (200 MB).
/// Files larger than this are re-read from disk on each render.
const BYTE_CACHE_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Background PDF renderer.
/// Runs on a dedicated thread, receives render requests and sends back RGBA bitmaps.
///
/// ## Performance
/// File bytes are cached in memory after the first read. Subsequent page turns
/// and zoom changes skip disk I/O entirely — only the requested page is re-rasterized.
/// For files over 200 MB, the cache is bypassed to avoid excessive memory use.
pub struct PdfRenderer {
    request_rx: Receiver<RenderRequest>,
    result_tx: Sender<RenderResult>,
    /// Pdfium library binding — owned directly by the renderer thread.
    /// No Mutex needed since there is only one renderer thread.
    pdfium: Option<pdfium_render::prelude::Pdfium>,
    /// Cached raw bytes of the last-opened PDF file.
    /// Cleared when the file path changes or when the file exceeds the cache limit.
    cached_bytes: Option<(PathBuf, Vec<u8>)>,
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
        }
    }

    /// Try to bind pdfium from the exe directory or system path.
    fn init_pdfium() -> Option<pdfium_render::prelude::Pdfium> {
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

        while let Ok(mut request) = self.request_rx.recv() {
            // Coalesce: drain stale requests, keep only the latest
            while let Ok(newer) = self.request_rx.try_recv() {
                request = newer;
            }
            match self.render_page(&request) {
                Ok(result) => {
                    if self.result_tx.send(result).is_err() {
                        break;
                    }
                }
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
                    });
                }
            }
        }

        info!("PDF renderer stopped");
    }

    /// Ensure pdfium is initialized (must be called before any immutable borrow).
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
    /// Reads from disk only when the path changes.
    fn ensure_bytes(&mut self, path: &PathBuf) -> anyhow::Result<()> {
        let cache_hit = self
            .cached_bytes
            .as_ref()
            .is_some_and(|(cached_path, _)| cached_path == path);

        if cache_hit {
            debug!("PDF bytes cache hit: {}", path.display());
            return Ok(());
        }

        let metadata = std::fs::metadata(path)
            .with_context(|| format!("Failed to stat: {}", path.display()))?;
        let file_size = metadata.len();

        if file_size > BYTE_CACHE_MAX_BYTES {
            warn!(
                "PDF {} is {:.1} MB — exceeds cache limit, will re-read on each page turn",
                path.display(),
                file_size as f64 / (1024.0 * 1024.0)
            );
        } else {
            info!(
                "Reading PDF bytes: {} ({:.1} MB)",
                path.display(),
                file_size as f64 / (1024.0 * 1024.0)
            );
        }

        let bytes =
            std::fs::read(path).with_context(|| format!("Failed to read: {}", path.display()))?;

        // Store in cache. Oversized files are cached temporarily for this render
        // and evicted in the immutable phase below.
        self.cached_bytes = Some((path.clone(), bytes));
        Ok(())
    }

    /// Immutable render phase — all mutable prep is done.
    fn do_render(&self, request: &RenderRequest) -> anyhow::Result<RenderResult> {
        let pdfium = self.pdfium.as_ref().expect("pdfium initialized");
        let (_, bytes) = self.cached_bytes.as_ref().expect("bytes cached");

        let doc = pdfium
            .load_pdf_from_byte_slice(bytes, None)
            .context("Failed to load PDF")?;

        let pages = doc.pages();
        info!("PDF opened: {} pages", pages.len());
        if pages.is_empty() {
            return Ok(RenderResult {
                request_id: request.request_id,
                path: request.path.clone(),
                page: request.page,
                page_count: 0,
                rgba_bytes: Vec::new(),
                width: 0,
                height: 0,
            });
        }

        // Get requested page (1-indexed, clamp to valid range)
        let page_count = pages.len();
        let page_idx: pdfium_render::prelude::PdfPageIndex =
            ((request.page.max(1) - 1) as u16).min(page_count.saturating_sub(1));
        let page = pages.get(page_idx).context("Failed to get page")?;

        let width = page.width().value as usize;
        let height = page.height().value as usize;

        // Apply zoom factor, then limit for performance
        let zoom = (request.zoom as f64).clamp(0.25, 4.0);
        let max_dim = (2000.0 * zoom) as usize;
        let (render_width, render_height) = if width > max_dim || height > max_dim {
            let scale = max_dim as f64 / width.max(height) as f64;
            (
                (width as f64 * scale) as usize,
                (height as f64 * scale) as usize,
            )
        } else {
            let w = (width as f64 * zoom) as usize;
            let h = (height as f64 * zoom) as usize;
            (w.max(1), h.max(1))
        };

        // Render to bitmap
        let render_config = pdfium_render::prelude::PdfRenderConfig::new()
            .set_target_width(render_width as i32)
            .set_target_height(render_height as i32);
        let bitmap = page
            .render_with_config(&render_config)
            .context("Failed to render page")?;

        debug!(
            "Page {} rendered: {}x{}",
            request.page, render_width, render_height
        );

        // Extract RGBA bytes
        let rgba_bytes = bitmap.as_rgba_bytes();

        Ok(RenderResult {
            request_id: request.request_id,
            path: request.path.clone(),
            page: request.page,
            page_count: page_count as usize,
            rgba_bytes,
            width: render_width,
            height: render_height,
        })
    }

    fn render_page(&mut self, request: &RenderRequest) -> anyhow::Result<RenderResult> {
        // ── Phase 1: mutable prep (ensure pdfium + bytes are ready) ──
        self.ensure_pdfium()?;
        self.ensure_bytes(&request.path)?;

        // ── Phase 2: immutable render (no mutable self access needed) ──
        let result = self.do_render(request);

        // ── Phase 3: evict oversized cache entry after render ──
        if let Some((_, ref cached)) = self.cached_bytes {
            if cached.len() as u64 > BYTE_CACHE_MAX_BYTES {
                self.cached_bytes = None;
            }
        }

        result
    }
}
