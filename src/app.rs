use egui::{CentralPanel, TopBottomPanel, SidePanel, ScrollArea, TextEdit, RichText, Color32, Frame};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use crossbeam::channel::{Sender, Receiver};
use crate::config::Config;
use crate::search::engine::SearchEngine;
use crate::search::query::{SearchRequest, SearchResult};
use crate::tags::store::TagStore;
use crate::tags::model::Tag;

/// Messages from the indexer thread to the UI thread.
#[derive(Debug, Clone)]
pub enum IndexerProgress {
    Indexed { path: PathBuf, total: usize },
    Error { path: PathBuf, error: String },
    Done { total: usize },
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
    pub path: PathBuf,
    pub page: usize,
    pub search_terms: Vec<String>,
}

/// Messages from the renderer thread to the UI thread.
#[derive(Debug, Clone)]
pub struct RenderResult {
    pub rgba_bytes: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub highlights: Vec<HighlightRect>,
}

#[derive(Debug, Clone)]
pub struct HighlightRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Top-level application state.
pub struct PapervaultApp {
    config: Config,
    search_engine: Option<Arc<Mutex<SearchEngine>>>,
    search_query: String,
    search_results: Vec<SearchResult>,
    total_hits: usize,
    selected_result: Option<usize>,
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
    active_tag_filters: Vec<String>,
    tag_panel_open: bool,
    new_tag_name: String,
    // Indexing progress
    indexing_total: usize,
    indexing_done: usize,
    // Pending click target (resolved outside results loop to avoid borrow conflict)
    clicked_index: Option<usize>,
}

impl PapervaultApp {
    pub fn new(
        config: Config,
        search_engine: Option<Arc<Mutex<SearchEngine>>>,
        progress_rx: Receiver<IndexerProgress>,
        tag_tx: Option<Sender<TagUpdate>>,
        render_tx: Option<Sender<RenderRequest>>,
        render_rx: Option<Receiver<RenderResult>>,
        tag_store: Option<TagStore>,
    ) -> Self {
        let status = if config.watched_folder.is_some() && search_engine.is_some() {
            "Ready".to_string()
        } else {
            String::new()
        };
        Self {
            config,
            search_engine,
            search_query: String::new(),
            search_results: Vec::new(),
            total_hits: 0,
            selected_result: None,
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
            all_tags: Vec::new(),
            active_tag_filters: Vec::new(),
            tag_panel_open: false,
            new_tag_name: String::new(),
            indexing_total: 0,
            indexing_done: 0,
            clicked_index: None,
        }
    }

    /// Initialize the search engine for the configured watched folder.
    fn init_search_engine(&mut self) {
        if let Some(ref folder) = self.config.watched_folder {
            match SearchEngine::open_or_create(folder) {
                Ok(engine) => {
                    self.search_engine = Some(Arc::new(Mutex::new(engine)));
                    self.status_message = format!("Watching: {}", folder.display());
                }
                Err(e) => {
                    self.status_message = format!("Failed to open index: {}", e);
                }
            }
        }
    }

    /// Perform a search query.
    fn do_search(&mut self) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            self.search_results.clear();
            self.total_hits = 0;
            return;
        }

        if let Some(ref engine) = self.search_engine {
            let engine = engine.lock().unwrap();
            let request = SearchRequest::new(query.clone())
                .with_tags(self.active_tag_filters.clone());
            match engine.search(&request) {
                Ok(results) => {
                    self.total_hits = results.total_hits;
                    self.search_results = results.items;
                }
                Err(e) => {
                    self.status_message = format!("Search error: {}", e);
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
        let result = &self.search_results[index];
        let path = PathBuf::from(&result.file_path);

        if result.file_type == "pdf" {
            if let Some(ref tx) = self.render_request_tx {
                let request = RenderRequest {
                    path,
                    page: 1,
                    search_terms: self.search_query
                        .split_whitespace()
                        .map(|s| s.to_string())
                        .collect(),
                };
                let _ = tx.send(request);
            }
            self.preview_text = None;
        } else {
            self.preview_texture = None;
            match std::fs::read_to_string(&result.file_path) {
                Ok(content) => {
                    self.preview_text = Some(content);
                }
                Err(e) => {
                    self.preview_text = Some(format!("Error reading file: {}", e));
                }
            }
        }
        self.preview_file_type = Some(result.file_type.clone());
    }

    /// Poll for render results and indexer progress.
    fn poll_channels(&mut self, ctx: &egui::Context) {
        if let Some(ref rx) = self.render_result_rx {
            while let Ok(result) = rx.try_recv() {
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

        while let Ok(progress) = self.indexer_progress_rx.try_recv() {
            match progress {
                IndexerProgress::Indexed { total, .. } => {
                    self.indexing_total = total;
                    self.indexing_done += 1;
                }
                IndexerProgress::Done { total } => {
                    self.indexing_total = total;
                    self.indexing_done = total;
                }
                IndexerProgress::Error { path, error } => {
                    self.status_message = format!("Error indexing {}: {}", path.display(), error);
                }
            }
        }

        // Reload tags occasionally
        if let Some(ref store) = self.tag_store {
            if let Ok(tags) = store.list_tags() {
                self.all_tags = tags;
            }
        }
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
        if let Some(pos) = self.active_tag_filters.iter().position(|t| t == tag_name) {
            self.active_tag_filters.remove(pos);
        } else {
            self.active_tag_filters.push(tag_name.to_string());
        }
        self.do_search();
    }

    fn assign_tag_to_selected(&mut self, tag_id: i64) {
        if let (Some(selected), Some(ref store)) = (self.selected_result, &self.tag_store) {
            let content_hash = &self.search_results[selected].content_hash;
            if store.assign_tag(content_hash, tag_id).is_ok() {
                // Sync to Tantivy
                if let Some(ref tx) = self.tag_update_tx {
                    let tags = store.get_tags_for_document(content_hash)
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
    }
}

impl eframe::App for PapervaultApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_channels(ctx);

        // ── Top bar: search ──
        TopBottomPanel::top("search_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("🔍");
                let resp = ui.add_sized(
                    [ui.available_width() - 180.0, 24.0],
                    TextEdit::singleline(&mut self.search_query)
                        .hint_text("Search documents..."),
                );
                if resp.changed() {
                    self.do_search();
                }

                if ui.button("📁 Folder").clicked() {
                    self.folder_picker_open = true;
                }
                if ui.button("🏷 Tags").clicked() {
                    self.tag_panel_open = !self.tag_panel_open;
                }

                if self.indexing_total > 0 && self.indexing_done < self.indexing_total {
                    ui.label(format!("Indexing {}/{}", self.indexing_done, self.indexing_total));
                }
            });
            // Active tag filter chips
            if !self.active_tag_filters.is_empty() {
                ui.horizontal(|ui| {
                    for tag in &self.active_tag_filters.clone() {
                        ui.label(format!("🔖 {}", tag));
                    }
                });
            }
        });

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
                        ui.add_sized([120.0, 20.0], TextEdit::singleline(&mut self.new_tag_name)
                            .hint_text("New tag..."));
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
                                if self.selected_result.is_some() {
                                    if ui.small_button("📌").clicked() {
                                        self.assign_tag_to_selected(tag.id);
                                    }
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

                        Frame::default()
                            .fill(bg)
                            .inner_margin(4.0)
                            .show(ui, |ui| {
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

                                ui.label(
                                    RichText::new(&result.snippet)
                                        .small()
                                        .color(Color32::GRAY),
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
            if self.config.watched_folder.is_none() {
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
            } else if let Some(ref texture) = self.preview_texture {
                ui.image(egui::ImageSource::Texture(
                    egui::load::SizedTexture::new(texture.id(), texture.size_vec2()),
                ));
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
                    let mut path_str = self.config.watched_folder
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    if ui.text_edit_singleline(&mut path_str).lost_focus() ||
                       ui.button("Set Folder").clicked() {
                        let path = PathBuf::from(&path_str);
                        if path.exists() && path.is_dir() {
                            self.config.watched_folder = Some(path);
                            let _ = self.config.save();
                            self.init_search_engine();
                            self.folder_picker_open = false;
                            self.status_message = format!("Watching: {}", path_str);
                        } else {
                            self.status_message = format!("Invalid folder: {}", path_str);
                        }
                    }
                });
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.config.save();
    }
}
