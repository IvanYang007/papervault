use anyhow::Context;
use crossbeam::channel::{Receiver, Sender};
use std::sync::Arc;
use std::sync::Mutex;
use tracing::{error, info};

use crate::app::{RenderRequest, RenderResult};

/// Background PDF renderer.
/// Runs on a dedicated thread, receives render requests and sends back RGBA bitmaps.
pub struct PdfRenderer {
    request_rx: Receiver<RenderRequest>,
    result_tx: Sender<RenderResult>,
}

impl PdfRenderer {
    pub fn new(request_rx: Receiver<RenderRequest>, result_tx: Sender<RenderResult>) -> Self {
        Self {
            request_rx,
            result_tx,
        }
    }

    /// Run the render loop (blocks until channel closes).
    pub fn run(&mut self) {
        info!("PDF renderer started (pdfium lazy-init on first request)");

        // pdfium is lazy-initialized on the first render request
        let pdfium: Arc<Mutex<Option<pdfium_render::prelude::Pdfium>>> = Arc::new(Mutex::new(None));

        while let Ok(request) = self.request_rx.recv() {
            match self.render_page(&request, &pdfium) {
                Ok(result) => {
                    if self.result_tx.send(result).is_err() {
                        break; // UI thread dropped receiver
                    }
                }
                Err(e) => {
                    error!("Render error for {}: {}", request.path.display(), e);
                    // Send empty result as error indicator
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

    fn render_page(
        &mut self,
        request: &RenderRequest,
        pdfium_ref: &Arc<Mutex<Option<pdfium_render::prelude::Pdfium>>>,
    ) -> anyhow::Result<RenderResult> {
        // Lazy-init pdfium
        let mut guard = pdfium_ref.lock().unwrap();
        if guard.is_none() {
            info!("Initializing pdfium (first render request)...");
            let pdfium = pdfium_render::prelude::Pdfium::new(
                pdfium_render::prelude::Pdfium::bind_to_library(
                    pdfium_render::prelude::Pdfium::pdfium_platform_library_name(),
                )
                .or_else(|_| pdfium_render::prelude::Pdfium::bind_to_system_library())
                .context("Failed to bind pdfium library")?,
            );
            *guard = Some(pdfium);
        }
        let pdfium = guard.as_ref().unwrap();
        // Keep the mutex guard alive for the duration of this render call
        // (the pdfium reference borrows from guard)

        // Read file
        let bytes = std::fs::read(&request.path)
            .with_context(|| format!("Failed to read: {}", request.path.display()))?;

        let doc = pdfium
            .load_pdf_from_byte_slice(&bytes, None)
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
            });
        }

        // Get requested page (1-indexed, clamp to valid range)
        let page_count = pages.len();
        let page_idx: pdfium_render::prelude::PdfPageIndex =
            ((request.page.max(1) - 1) as u16).min(page_count.saturating_sub(1));
        let page = pages.get(page_idx).context("Failed to get page")?;

        let width = page.width().value as usize;
        let height = page.height().value as usize;

        // Limit render size for performance
        let max_dim = 2000;
        let (render_width, render_height) = if width > max_dim || height > max_dim {
            let scale = max_dim as f64 / width.max(height) as f64;
            (
                (width as f64 * scale) as usize,
                (height as f64 * scale) as usize,
            )
        } else {
            (width.max(1), height.max(1))
        };

        // Render to bitmap
        let render_config = pdfium_render::prelude::PdfRenderConfig::new()
            .set_target_width(render_width as i32)
            .set_target_height(render_height as i32);
        let bitmap = page
            .render_with_config(&render_config)
            .context("Failed to render page")?;

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
}
