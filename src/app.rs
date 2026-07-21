use crate::config::Config;
use crate::runtime::FolderRuntime;
use crate::search::engine::SearchEngine;
use crate::search::query::{SearchRequest, SearchResult};
use crate::search::schema::SchemaFields;
use crate::tags::model::Tag;
use crate::tags::store::TagStore;
use crate::watcher::watcher::IndexerMessage;
use crossbeam::channel::{Receiver, Sender};
use egui::{
    CentralPanel, Color32, Frame, RichText, ScrollArea, SidePanel, TextEdit, TopBottomPanel,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Messages from the indexer thread to the UI thread.
#[derive(Debug, Clone)]
pub enum IndexerProgress {
    /// A single file was processed.
    Progress { processed: usize },
    /// Initial scan complete; total is the final count.
    ScanComplete { total: usize },
    /// An error occurred processing a file.
    Error { path: PathBuf, error: String },
}

/// Messages from the UI thread to the indexer thread for tag updates.
#[derive(Debug, Clone)]
pub enum TagUpdate {
    UpdateDocumentTags {
        content_hash: String,
        tags: Vec<String>,
    },
}

/// Messages from the UI thread to the renderer thread.
#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub request_id: u64,
    pub path: PathBuf,
    pub page: usize,
}

/// Messages from the renderer thread to the UI thread.
#[derive(Debug, Clone)]
pub struct RenderResult {
    pub request_id: u64,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub page: usize,
    pub page_count: usize,
    pub rgba_bytes: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Top-level application state.
pub struct PapervaultApp {
    config: Config,
    /// Lock-free reader for search — no Mutex contention with indexer writes.
    search_reader: Option<tantivy::IndexReader>,
    /// Pre-cloned schema fields — avoids Mutex lock during search.
    search_fields: Option<SchemaFields>,
    search_engine: Option<Arc<Mutex<SearchEngine>>>,
    /// Owns watcher, indexer, renderer threads and channels for folder lifecycle.
    folder_runtime: Option<FolderRuntime>,
    search_query: String,
    search_results: Vec<SearchResult>,
    total_hits: usize,
    selected_result: Option<usize>,
    /// Stable document identity — survives search query changes.
    selected_hash: Option<String>,
    folder_picker_open: bool,
    status_message: String,
    // Preview
    preview_texture: Option<egui::TextureHandle>,
    preview_text: Option<String>,
    preview_file_type: Option<String>,
    // Channels
    indexer_progress_rx: Receiver<IndexerProgress>,
    tag_update_tx: Option<Sender<TagUpdate>>,
    render_request_tx: Option<Sender<RenderRequest>>,
    render_result_rx: Option<Receiver<RenderResult>>,
    // Tag system
    tag_store: Option<TagStore>,
    all_tags: Vec<Tag>,
    active_tag_filters: HashSet<String>,
    tag_panel_open: bool,
    new_tag_name: String,
    /// Persistent text input for the folder picker dialog.
    folder_picker_input: String,
    // Indexing progress
    indexing_total: usize,
    indexing_done: usize,
    // Pending click target (resolved outside results loop to avoid borrow conflict)
    clicked_index: Option<usize>,
    // PDF page navigation
    current_page: usize,
    /// Monotonic render request counter — used to discard stale results.
    latest_render_request_id: u64,
    /// Path of the currently previewed PDF — used to detect stale results.
    current_preview_path: Option<PathBuf>,
    /// Page count of the currently previewed PDF — last page bound.
    current_pdf_page_count: usize,
    // Graceful shutdown: signals watcher to stop, closing the channel to indexer
    watcher_shutdown_flag: Option<Arc<AtomicBool>>,
    #[allow(dead_code)]
    watcher_shutdown_tx: Option<Sender<IndexerMessage>>,
    // Debounced search-as-you-type
    last_search_instant: Option<Instant>,
    pending_search: Option<String>,
    /// Request search input focus on the next frame (first-launch UX).
    focus_search_next_frame: bool,
}

impl PapervaultApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        search_engine: Option<Arc<Mutex<SearchEngine>>>,
        search_reader: Option<tantivy::IndexReader>,
        search_fields: Option<SchemaFields>,
        progress_rx: Receiver<IndexerProgress>,
        tag_tx: Option<Sender<TagUpdate>>,
        render_tx: Option<Sender<RenderRequest>>,
        render_rx: Option<Receiver<RenderResult>>,
        tag_store: Option<TagStore>,
        watcher_shutdown_flag: Option<Arc<AtomicBool>>,
        watcher_shutdown_tx: Option<Sender<IndexerMessage>>,
        folder_runtime: Option<FolderRuntime>,
    ) -> Self {
        let status = if config.watched_folder.is_some() && search_engine.is_some() {
            "Ready".to_string()
        } else {
            String::new()
        };
        // Load tags once at startup — not every frame.
        let all_tags = tag_store
            .as_ref()
            .and_then(|store| store.list_tags().ok())
            .unwrap_or_default();
        Self {
            config,
            search_reader,
            search_fields,
            search_engine,
            search_query: String::new(),
            search_results: Vec::new(),
            total_hits: 0,
            selected_result: None,
            selected_hash: None,
            folder_picker_open: false,
            status_message: status,
            preview_texture: None,
            preview_text: None,
            preview_file_type: None,
            indexer_progress_rx: progress_rx,
            tag_update_tx: tag_tx,
            render_request_tx: render_tx,
            render_result_rx: render_rx,
            tag_store,
            all_tags,
            active_tag_filters: HashSet::new(),
            tag_panel_open: false,
            new_tag_name: String::new(),
            folder_picker_input: String::new(),
            indexing_total: 0,
            indexing_done: 0,
            clicked_index: None,
            current_page: 1,
            latest_render_request_id: 0,
            current_preview_path: None,
            current_pdf_page_count: 0,
            watcher_shutdown_flag,
            watcher_shutdown_tx,
            last_search_instant: None,
            pending_search: None,
            focus_search_next_frame: true,
            folder_runtime,
        }
    }

    /// Start the full folder runtime: opens engine, spawns indexer/watcher/renderer,
    /// runs reconciliation, and begins watching for file changes.
    fn start_folder_runtime(&mut self, folder: &Path) {
        let Some(ref tag_store) = self.tag_store else {
            self.status_message = "Tag store not available — cannot index.".to_string();
            return;
        };
        match FolderRuntime::start(folder, tag_store) {
            Ok(runtime) => {
                self.search_reader = Some(runtime.search_reader.clone());
                self.search_fields = Some(runtime.search_fields.clone());
                self.search_engine = Some(runtime.search_engine.clone());
                // Replace channels with ones from the new runtime
                self.indexer_progress_rx = runtime.progress_rx.clone();
                self.tag_update_tx = runtime.tag_tx.clone();
                self.render_request_tx = Some(runtime.render_tx.clone());
                self.render_result_rx = Some(runtime.render_result_rx.clone());
                self.watcher_shutdown_flag = Some(runtime.watcher_shutdown());
                self.watcher_shutdown_tx = runtime.watcher_shutdown_tx();
                self.folder_runtime = Some(runtime);
                self.status_message = format!("Watching: {}", folder.display());
            }
            Err(e) => {
                self.status_message = format!("Failed to start indexing: {}", e);
            }
        }
    }

    /// Perform a search query using lock-free reader (no Mutex during search).
    fn do_search(&mut self) {
        let query = self.search_query.trim();
        if query.is_empty() {
            self.search_results.clear();
            self.total_hits = 0;
            self.selected_result = None;
            self.selected_hash = None;
            return;
        }

        if let Some(ref reader) = self.search_reader {
            if let Some(ref fields) = self.search_fields {
                // Don't pass tag filters to Tantivy — Tantivy tags may be stale.
                // Instead, post-filter using SQLite which always has the truth.
                let request = SearchRequest::new(query.to_string()).with_limit(50);
                match crate::search::engine::search_with_reader(fields, reader, &request) {
                    Ok(mut results) => {
                        // Batch-fetch tags for all results (single query, chunked by 500)
                        if let Some(ref store) = self.tag_store {
                            if !results.items.is_empty() {
                                let hashes: Vec<String> = results
                                    .items
                                    .iter()
                                    .map(|r| r.content_hash.clone())
                                    .collect();
                                if let Ok(tag_map) = store.get_tags_for_hashes(&hashes) {
                                    for item in &mut results.items {
                                        if let Some(tags) = tag_map.get(&item.content_hash) {
                                            item.tags =
                                                tags.iter().map(|t| t.name.clone()).collect();
                                        }
                                    }
                                }
                            }
                            // Post-filter by active tag filters (O(result_tags) HashSet lookup)
                            if !self.active_tag_filters.is_empty() {
                                results.items.retain(|result| {
                                    self.active_tag_filters
                                        .iter()
                                        .all(|f| result.tags.iter().any(|t| t == f))
                                });
                                results.items.truncate(50);
                                results.total_hits = results.items.len();
                            }
                        } else if !self.active_tag_filters.is_empty() {
                            // Tag store unavailable but filters are active — warn and
                            // return empty results rather than showing unfiltered documents.
                            tracing::warn!(
                                "Tag filters active but tag store unavailable; showing empty results"
                            );
                            self.search_results.clear();
                            self.total_hits = 0;
                            return;
                        }
                        self.total_hits = results.total_hits;
                        self.search_results = results.items;
                        // Remap stable hash to index after results change
                        self.selected_result = self.selected_hash.as_ref().and_then(|hash| {
                            self.search_results
                                .iter()
                                .position(|r| r.content_hash == *hash)
                        });
                        if self.selected_result.is_none() {
                            self.selected_hash = None;
                        }
                    }
                    Err(e) => {
                        self.status_message = format!("Search error: {}", e);
                    }
                }
            }
        }
    }

    /// Select a search result and trigger preview rendering.
    fn select_result(&mut self, index: usize) {
        if index >= self.search_results.len() {
            return;
        }
        self.selected_result = Some(index);
        self.selected_hash = Some(self.search_results[index].content_hash.clone());
        self.current_page = 1;
        // Clear stale preview when switching documents
        self.preview_texture = None;
        self.current_pdf_page_count = 0;
        let is_pdf = self.search_results[index].file_type == "pdf";
        let file_path = self.search_results[index].file_path.clone();
        let file_type = self.search_results[index].file_type.clone();

        if is_pdf {
            self.request_page_render();
            self.preview_text = None;
        } else {
            self.preview_texture = None;
            const PREVIEW_MAX_BYTES: u64 = 2 * 1024 * 1024; // 2 MB
            match std::fs::metadata(&file_path) {
                Ok(meta) if meta.len() > PREVIEW_MAX_BYTES => {
                    // Read only the first 2 MB to avoid UI freeze on large files
                    match std::fs::File::open(&file_path) {
                        Ok(file) => {
                            use std::io::Read;
                            let mut reader = std::io::BufReader::new(file.take(PREVIEW_MAX_BYTES));
                            let mut content = String::new();
                            if reader.read_to_string(&mut content).is_ok() {
                                content.push_str("\n\n─── Preview truncated at 2 MB ───");
                                self.preview_text = Some(content);
                            } else {
                                self.preview_text = Some("Error reading file.".to_string());
                            }
                        }
                        Err(e) => {
                            self.preview_text = Some(format!("Error reading file: {}", e));
                        }
                    }
                }
                _ => match std::fs::read_to_string(&file_path) {
                    Ok(content) => {
                        self.preview_text = Some(content);
                    }
                    Err(e) => {
                        self.preview_text = Some(format!("Error reading file: {}", e));
                    }
                },
            }
        }
        self.preview_file_type = Some(file_type);
    }

    /// Send a render request for the current page of the selected result.
    fn request_page_render(&mut self) {
        let Some(selected) = self.selected_result else {
            return;
        };
        if selected >= self.search_results.len() {
            return;
        }
        let result = &self.search_results[selected];
        let path = PathBuf::from(&result.file_path);
        self.latest_render_request_id += 1;
        self.current_preview_path = Some(path.clone());
        if let Some(ref tx) = self.render_request_tx {
            let request = RenderRequest {
                request_id: self.latest_render_request_id,
                path,
                page: self.current_page,
            };
            let _ = tx.send(request);
        }
    }

    /// Maximum messages to process per channel per frame — prevents UI starvation.
    const MAX_MESSAGES_PER_FRAME: usize = 64;

    /// Poll for render results and indexer progress.
    fn poll_channels(&mut self, ctx: &egui::Context) {
        if let Some(ref rx) = self.render_result_rx {
            for _ in 0..Self::MAX_MESSAGES_PER_FRAME {
                let Ok(result) = rx.try_recv() else {
                    break;
                };
                // Discard stale results from previous requests or different documents
                if result.request_id != self.latest_render_request_id {
                    continue;
                }
                if self.current_preview_path.as_deref() != Some(&result.path) {
                    continue;
                }
                self.current_pdf_page_count = result.page_count;
                if result.width > 0 && result.height > 0 {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied(
                        [result.width, result.height],
                        &result.rgba_bytes,
                    );
                    self.preview_texture = Some(ctx.load_texture(
                        "pdf-preview",
                        color_image,
                        egui::TextureOptions::default(),
                    ));
                }
            }
        }

        for _ in 0..Self::MAX_MESSAGES_PER_FRAME {
            let Ok(progress) = self.indexer_progress_rx.try_recv() else {
                break;
            };
            match progress {
                IndexerProgress::Progress { processed } => {
                    self.indexing_done = processed;
                }
                IndexerProgress::ScanComplete { total } => {
                    self.indexing_total = total;
                    self.indexing_done = total;
                }
                IndexerProgress::Error { path, error } => {
                    self.status_message = format!("Error indexing {}: {}", path.display(), error);
                }
            }
        }

        // Tags are now loaded at startup and refreshed only after create/delete.
        // No per-frame tag query.
    }

    // ── Tag operations ──

    fn create_tag(&mut self, name: &str) {
        if let Some(ref store) = self.tag_store {
            if store.create_tag(name).is_ok() {
                if let Ok(tags) = store.list_tags() {
                    self.all_tags = tags;
                }
                self.new_tag_name.clear();
            }
        }
    }

    fn toggle_tag_filter(&mut self, tag_name: &str) {
        if self.active_tag_filters.contains(tag_name) {
            self.active_tag_filters.remove(tag_name);
        } else {
            self.active_tag_filters.insert(tag_name.to_string());
        }
        self.do_search();
    }

    fn assign_tag_to_selected(&mut self, tag_id: i64) {
        let Some(ref content_hash) = self.selected_hash else {
            return;
        };
        let content_hash = content_hash.clone();
        let Some(ref store) = self.tag_store else {
            return;
        };

        if store.assign_tag(&content_hash, tag_id).is_ok() {
            // Sync to Tantivy
            if let Some(ref tx) = self.tag_update_tx {
                let tags = store
                    .get_tags_for_document(&content_hash)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| t.name)
                    .collect();
                let _ = tx.send(TagUpdate::UpdateDocumentTags {
                    content_hash: content_hash.clone(),
                    tags,
                });
            }
            // Re-search to reflect tag changes
            self.do_search();
        }
    }

    /// Render a snippet with matched terms highlighted in gold.
    fn render_highlighted_snippet(ui: &mut egui::Ui, snippet: &str, match_terms: &[String]) {
        if match_terms.is_empty() {
            ui.label(RichText::new(snippet).small().color(Color32::GRAY));
            return;
        }

        let lower_snippet = snippet.to_lowercase();
        let mut spans: Vec<(usize, usize)> = Vec::new();

        // Find all match positions (case-insensitive)
        for term in match_terms {
            let lower_term = term.to_lowercase();
            if lower_term.is_empty() {
                continue;
            }
            let mut search_start = 0;
            while let Some(pos) = lower_snippet[search_start..].find(&lower_term) {
                let abs_start = search_start + pos;
                let abs_end = abs_start + lower_term.len();
                spans.push((abs_start, abs_end));
                search_start = abs_end;
            }
        }

        if spans.is_empty() {
            ui.label(RichText::new(snippet).small().color(Color32::GRAY));
            return;
        }

        // Sort and merge overlapping spans, validating against original byte boundaries.
        // Byte offsets from the lowercased string may not align with UTF-8 boundaries
        // in the original snippet (e.g., Turkish İ → i̇ changes byte length).
        spans.sort_by_key(|s| s.0);
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for span in spans {
            // Validate span boundaries against the original snippet
            if !snippet.is_char_boundary(span.0) || !snippet.is_char_boundary(span.1) {
                tracing::debug!(
                    "Skipping highlight span at byte offsets ({}, {}) — not char-aligned in original",
                    span.0, span.1
                );
                continue;
            }
            if let Some(last) = merged.last_mut() {
                if span.0 <= last.1 {
                    last.1 = last.1.max(span.1);
                } else {
                    merged.push(span);
                }
            } else {
                merged.push(span);
            }
        }

        // Render segments with alternating colors
        ui.horizontal(|ui| {
            let mut cursor = 0;
            for (start, end) in &merged {
                if cursor < *start {
                    ui.label(
                        RichText::new(&snippet[cursor..*start])
                            .small()
                            .color(Color32::GRAY),
                    );
                }
                ui.label(
                    RichText::new(&snippet[*start..*end])
                        .small()
                        .color(Color32::BLACK)
                        .background_color(Color32::from_rgb(255, 215, 0)),
                );
                cursor = *end;
            }
            if cursor < snippet.len() {
                ui.label(
                    RichText::new(&snippet[cursor..])
                        .small()
                        .color(Color32::GRAY),
                );
            }
        });
    }
}

impl eframe::App for PapervaultApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_channels(ctx);

        // Pre-compute whether the search query is non-empty (avoids redundant trim scans)
        let has_search_query = !self.search_query.trim().is_empty();

        // Debounced search: execute when 150ms has elapsed since last keystroke
        if let (Some(ref pending), Some(ref instant)) =
            (&self.pending_search, &self.last_search_instant)
        {
            let elapsed = instant.elapsed();
            if elapsed >= std::time::Duration::from_millis(150) {
                if self.search_query == *pending {
                    self.do_search();
                    self.pending_search = None;
                    self.last_search_instant = None;
                }
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(150) - elapsed);
            }
        }

        // ── Left panel: tag panel (when open) ──
        if self.tag_panel_open {
            SidePanel::left("tag_panel")
                .resizable(false)
                .default_width(200.0)
                .show(ctx, |ui| {
                    ui.heading("Tags");
                    ui.separator();

                    // Create new tag
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [120.0, 20.0],
                            TextEdit::singleline(&mut self.new_tag_name).hint_text("New tag..."),
                        );
                        if ui.button("+").clicked() && !self.new_tag_name.trim().is_empty() {
                            let name = self.new_tag_name.trim().to_string();
                            self.create_tag(&name);
                        }
                    });

                    ui.separator();

                    // Tag list with checkboxes
                    ScrollArea::vertical().show(ui, |ui| {
                        let tags = self.all_tags.clone();
                        for tag in &tags {
                            let mut checked = self.active_tag_filters.contains(&tag.name);
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut checked, "").changed() {
                                    self.toggle_tag_filter(&tag.name);
                                }
                                ui.label(&tag.name);

                                // Assign to selected document
                                if self.selected_result.is_some() && ui.small_button("📌").clicked()
                                {
                                    self.assign_tag_to_selected(tag.id);
                                }
                            });
                        }
                    });
                });
        }

        // ── Left panel: search results ──
        SidePanel::left("results_panel")
            .resizable(true)
            .default_width(350.0)
            .show(ctx, |ui| {
                ui.heading("Results");
                if self.total_hits > self.search_results.len() {
                    ui.label(format!(
                        "{} shown of {} matches",
                        self.search_results.len(),
                        self.total_hits
                    ));
                }

                let mut clicked_idx = self.clicked_index.take();
                ScrollArea::vertical().show(ui, |ui| {
                    for (i, result) in self.search_results.iter().enumerate() {
                        let selected = self.selected_result == Some(i);
                        let bg = if selected {
                            Color32::from_rgb(40, 80, 120)
                        } else if i % 2 == 0 {
                            Color32::from_rgb(30, 30, 35)
                        } else {
                            Color32::TRANSPARENT
                        };

                        Frame::default().fill(bg).inner_margin(4.0).show(ui, |ui| {
                            let resp = ui.add_sized(
                                [ui.available_width(), 40.0],
                                egui::SelectableLabel::new(
                                    selected,
                                    RichText::new(format!(
                                        "{} ({})",
                                        result.file_name, result.match_count
                                    ))
                                    .strong(),
                                ),
                            );
                            if resp.clicked() {
                                clicked_idx = Some(i);
                            }

                            // Show tags
                            if !result.tags.is_empty() {
                                ui.horizontal(|ui| {
                                    for t in &result.tags {
                                        ui.label(RichText::new(format!("🏷{}", t)).small());
                                    }
                                });
                            }

                            Self::render_highlighted_snippet(
                                ui,
                                &result.snippet,
                                &result.match_terms,
                            );
                        });
                    }
                });

                // Handle click outside the borrow scope
                if let Some(idx) = clicked_idx {
                    self.select_result(idx);
                }
            });

        // ── Center: preview ──
        CentralPanel::default().show(ctx, |ui| {
            // Search bar
            ui.horizontal(|ui| {
                ui.label("🔍");
                let resp = ui.text_edit_singleline(&mut self.search_query);
                if self.focus_search_next_frame {
                    resp.request_focus();
                    self.focus_search_next_frame = false;
                }
                if resp.changed() {
                    self.pending_search = Some(self.search_query.clone());
                    self.last_search_instant = Some(Instant::now());
                    ctx.request_repaint_after(std::time::Duration::from_millis(50));
                }

                if ui.button("📁 Folder").clicked() {
                    self.folder_picker_open = true;
                    self.folder_picker_input = self
                        .config
                        .watched_folder
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                }
                if ui.button("🏷 Tags").clicked() {
                    self.tag_panel_open = !self.tag_panel_open;
                }

                if self.indexing_done > 0 {
                    if self.indexing_total > 0 {
                        ui.label(format!(
                            "Indexed {}/{}",
                            self.indexing_done, self.indexing_total
                        ));
                    } else {
                        ui.label(format!("Indexed {} files…", self.indexing_done));
                    }
                }
            });
            // Active tag filter chips
            if !self.active_tag_filters.is_empty() {
                ui.horizontal(|ui| {
                    for tag in self.active_tag_filters.iter().cloned().collect::<Vec<_>>() {
                        ui.label(format!("🔖 {}", tag));
                    }
                });
            }
            ui.separator();

            if self.config.watched_folder.is_some() && self.search_engine.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.colored_label(
                        Color32::RED,
                        format!(
                            "Search engine failed to initialize for {}",
                            self.config
                                .watched_folder
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                        ),
                    );
                    ui.label("The index may be corrupted. Try re-selecting the folder.");
                });
            } else if self.config.watched_folder.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.heading("Papervault");
                    ui.label("No watched folder configured.");
                    ui.label("Click 📁 Folder to get started.");
                });
            } else if self.selected_result.is_none() && self.search_query.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(100.0);
                    ui.label("Type a search query to find documents.");
                });
            } else if self.search_results.is_empty() && has_search_query {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label(format!("No results for '{}'", self.search_query.trim()));
                    ui.label("Try different terms or remove tag filters.");
                });
            } else if self.preview_texture.is_some() {
                let is_pdf = self.preview_file_type.as_deref() == Some("pdf");
                let current_page = self.current_page;

                // PDF page navigation (before preview borrow)
                if is_pdf {
                    let at_last_page = self.current_pdf_page_count > 0
                        && self.current_page >= self.current_pdf_page_count;
                    ui.horizontal(|ui| {
                        if ui.button("◀ Prev").clicked() && current_page > 1 {
                            self.current_page -= 1;
                            self.request_page_render();
                        }
                        if self.current_pdf_page_count > 0 {
                            ui.label(format!(
                                "Page {} / {}",
                                self.current_page, self.current_pdf_page_count
                            ));
                        } else {
                            ui.label(format!("Page {}", self.current_page));
                        }
                        if ui
                            .add_enabled(!at_last_page, egui::Button::new("Next ▶"))
                            .clicked()
                        {
                            self.current_page += 1;
                            self.request_page_render();
                        }
                    });
                    ui.separator();
                }

                let tex_id = self.preview_texture.as_ref().unwrap().id();
                let tex_size = self.preview_texture.as_ref().unwrap().size_vec2();
                ui.image(egui::ImageSource::Texture(egui::load::SizedTexture::new(
                    tex_id, tex_size,
                )));
            } else if let Some(ref text) = self.preview_text {
                ScrollArea::vertical().show(ui, |ui| {
                    ui.monospace(text);
                });
            }
        });

        // ── Bottom: status bar ──
        TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.label(&self.status_message);
        });

        // ── Folder picker dialog ──
        if self.folder_picker_open {
            egui::Window::new("Select Watched Folder")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Enter the folder path to watch:");
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [250.0, 20.0],
                            TextEdit::singleline(&mut self.folder_picker_input),
                        );
                        if ui.button("Browse...").clicked() {
                            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                self.folder_picker_input = path.display().to_string();
                            }
                        }
                    });
                    if ui.button("Set Folder").clicked() {
                        let path = PathBuf::from(&self.folder_picker_input);
                        if path.exists() && path.is_dir() {
                            // Stop old runtime before starting new one
                            if let Some(rt) = self.folder_runtime.take() {
                                let _ = rt.stop();
                            }
                            self.config.watched_folder = Some(path.clone());
                            let _ = self.config.save();
                            self.start_folder_runtime(&path);
                            self.folder_picker_open = false;
                        } else {
                            self.status_message =
                                format!("Invalid folder: {}", self.folder_picker_input);
                        }
                    }
                });
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.config.save();

        // Gracefully stop the folder runtime (joins all background threads).
        if let Some(rt) = self.folder_runtime.take() {
            let _ = rt.stop();
        }
    }
}
