use egui::{CentralPanel, TopBottomPanel, SidePanel, ScrollArea, TextEdit, RichText, Color32};
use std::path::PathBuf;
use std::sync::Arc;
use crossbeam::channel::{self, Sender, Receiver};
use crate::config::Config;
use crate::search::engine::SearchEngine;
use crate::search::query::{SearchRequest, SearchResult, SearchResults};

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
    search_engine: Option<Arc<SearchEngine>>,
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
    // Render channel
    render_request_tx: Option<Sender<RenderRequest>>,
    render_result_rx: Option<Receiver<RenderResult>>,
    // Indexing progress
    indexing_total: usize,
    indexing_done: usize,
}

impl Default for PapervaultApp {
    fn default() -> Self {
        let (_, progress_rx) = channel::unbounded();
        let (_, render_rx) = channel::unbounded();
        Self {
            config: Config::load(),
            search_engine: None,
            search_query: String::new(),
            search_results: Vec::new(),
            total_hits: 0,
            selected_result: None,
            folder_picker_open: false,
            status_message: String::new(),
            preview_texture: None,
            preview_text: None,
            preview_file_type: None,
            indexer_progress_rx: progress_rx,
            tag_update_tx: None,
            render_request_tx: None,
            render_result_rx: Some(render_rx),
            indexing_total: 0,
            indexing_done: 0,
        }
    }
}

impl PapervaultApp {
    /// Initialize the search engine for the configured watched folder.
    fn init_search_engine(&mut self) {
        if let Some(ref folder) = self.config.watched_folder {
            match SearchEngine::open_or_create(folder) {
                Ok(engine) => {
                    self.search_engine = Some(Arc::new(engine));
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
            let request = SearchRequest::new(query);
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
    fn select_result(&mut self, index: usize, _ctx: &egui::Context) {
        if index >= self.search_results.len() {
            return;
        }
        self.selected_result = Some(index);
        let result = &self.search_results[index];
        let path = PathBuf::from(&result.file_path);

        // For PDF files, send render request to background thread
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
            // For text files, read directly
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

    /// Poll for render results from the background render thread.
    fn poll_render_results(&mut self, ctx: &egui::Context) {
        if let Some(ref rx) = self.render_result_rx {
            while let Ok(result) = rx.try_recv() {
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

        // Poll for indexer progress
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
    }
}

impl eframe::App for PapervaultApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll async results
        self.poll_render_results(ctx);

        // Top bar: search
        TopBottomPanel::top("search_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("🔍");
                let response = ui.add_sized(
                    [ui.available_width() - 120.0, 24.0],
                    TextEdit::singleline(&mut self.search_query)
                        .hint_text("Search documents..."),
                );
                if response.changed() {
                    self.do_search();
                }

                // Folder selection button
                if ui.button("📁 Folder").clicked() {
                    self.folder_picker_open = true;
                }

                // Indexing progress
                if self.indexing_total > 0 && self.indexing_done < self.indexing_total {
                    ui.label(format!(
                        "Indexing {}/{}",
                        self.indexing_done, self.indexing_total
                    ));
                }
            });
        });

        // Left panel: search results
        SidePanel::left("results_panel")
            .resizable(true)
            .default_width(350.0)
            .show(ctx, |ui| {
                ui.heading("Results");
                if self.total_hits > self.search_results.len() {
                    ui.label(format!(
                        "{} of {} matches — refine your query",
                        self.search_results.len(),
                        self.total_hits
                    ));
                }

                let mut clicked_index: Option<usize> = None;

                ScrollArea::vertical().show(ui, |ui| {
                    for (i, result) in self.search_results.iter().enumerate() {
                        let selected = self.selected_result == Some(i);
                        let frame = if selected {
                            egui::Frame::default()
                                .fill(Color32::from_rgb(40, 80, 120))
                                .inner_margin(4.0)
                        } else {
                            egui::Frame::default().inner_margin(4.0)
                        };

                        frame.show(ui, |ui| {
                            let response = ui.add_sized(
                                [ui.available_width(), 40.0],
                                egui::SelectableLabel::new(
                                    selected,
                                    RichText::new(format!(
                                        "{} ({})",
                                        result.file_name,
                                        result.match_count
                                    ))
                                    .strong(),
                                ),
                            );
                            if response.clicked() {
                                clicked_index = Some(i);
                            }
                            ui.label(
                                RichText::new(&result.snippet)
                                    .small()
                                    .color(Color32::GRAY),
                            );
                        });
                    }
                });

                if let Some(i) = clicked_index {
                    self.select_result(i, ctx);
                }
            });

        // Center: preview
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

        // Bottom status bar
        TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.label(&self.status_message);
        });

        // Folder picker
        if self.folder_picker_open {
            // Use egui's file dialog — for now, provide a simple folder path input
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
        // Graceful shutdown: drop channels to signal threads
        let _ = self.config.save();
    }
}
