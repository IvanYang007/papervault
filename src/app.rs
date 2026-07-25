use crate::config::Config;
use crate::runtime::FolderRuntime;
use crate::search::engine::SearchEngine;
use crate::search::query::{SearchRequest, SearchResult};
use crate::search::schema::SchemaFields;

const PDF_TYPE: &str = "pdf";
use crate::tags::model::Tag;
use crate::tags::store::DocumentInfo;
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

/// Messages sent from the Indexer to the AutoTagger thread.
#[derive(Debug, Clone)]
pub enum AutoTagRequest {
    /// Request auto-tagging for a document.
    TagDocument {
        content_hash: String,
        filename: String,
        text: String,
        content_hash_before_tag: String,
    },
    /// Graceful shutdown — drain pending, then exit.
    Shutdown,
}

/// Messages from the UI thread to the renderer thread.
#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub request_id: u64,
    pub path: PathBuf,
    pub page: usize,
    pub zoom: f32,
    /// Target display dimensions in physical pixels (0 = use default max).
    pub target_width: u32,
    pub target_height: u32,
    /// Priority: 0 = prefetch (process only when idle), 1 = normal.
    pub priority: u8,
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
    /// Whether this is a low-res preview (true) or the final full-res render (false).
    pub is_preview: bool,
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
    auto_tagger_tx: Option<Sender<AutoTagRequest>>,
    cached_auto_tag_hash: Option<String>,
    cached_auto_tag_value: Option<serde_json::Value>,
    auto_tag_enabled: bool,
    auto_tag_progress: Option<(usize, usize)>,
    auto_tag_error: Option<String>,
    show_auto_tag_opt_in: bool,
    accepted_auto_tags: std::collections::HashMap<String, std::collections::HashSet<String>>,
    pending_retag: bool,
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
    /// Pre-computed (name, id) pairs — updated when tags are created/deleted/loaded.
    tag_list_cache: Vec<(String, i64)>,
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
    /// PDF zoom level (1.0 = 100%).
    pdf_zoom: f32,
    /// Last known preview panel size for display-resolution rendering.
    preview_panel_size: (u32, u32),
    // Graceful shutdown: signals watcher to stop, closing the channel to indexer
    watcher_shutdown_flag: Option<Arc<AtomicBool>>,
    #[allow(dead_code)]
    watcher_shutdown_tx: Option<Sender<IndexerMessage>>,
    // Debounced search-as-you-type
    last_search_instant: Option<Instant>,
    pending_search: Option<String>,
    /// Request search input focus on the next frame (first-launch UX).
    focus_search_next_frame: bool,
    /// Folder path queued for switch — started when old runtime finishes.
    pending_runtime: Option<Arc<Mutex<Option<FolderRuntime>>>>,
    /// Error from background folder switch thread (shared with spawned thread).
    background_error: Option<Arc<Mutex<Option<String>>>>,
    /// Cached file list for the file browser panel.
    file_browser_docs: Vec<DocumentInfo>,
    /// Whether the file browser needs a refresh.
    file_browser_dirty: bool,
    /// Cooldown counter: only refresh file browser every N frames during active indexing.
    file_browser_refresh_cooldown: usize,
    /// Currently previewed file path (from file browser, not search).
    browsed_file: Option<String>,
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
        auto_tagger_tx: Option<Sender<AutoTagRequest>>,
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
        let tag_list_cache = all_tags.iter().map(|t| (t.name.clone(), t.id)).collect();
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
            tag_list_cache,
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
            pdf_zoom: 1.0,
            preview_panel_size: (800, 600),
            watcher_shutdown_flag,
            watcher_shutdown_tx,
            cached_auto_tag_hash: None,
            cached_auto_tag_value: None,
            auto_tag_enabled: crate::auto_tagger::config::AutoTagConfig::load().enabled,
            auto_tag_progress: None,
            auto_tag_error: None,
            show_auto_tag_opt_in: false,
            accepted_auto_tags: std::collections::HashMap::new(),
            pending_retag: false,
            last_search_instant: None,
            pending_search: None,
            focus_search_next_frame: true,
            folder_runtime,
            auto_tagger_tx,
            pending_runtime: None,
            background_error: None,
            file_browser_docs: Vec::new(),
            file_browser_dirty: true,
            file_browser_refresh_cooldown: 0,
            browsed_file: None,
        }
    }

    /// Start the full folder runtime: opens engine, spawns indexer/watcher/renderer,
    /// runs reconciliation, and begins watching for file changes.
    #[allow(dead_code)]
    fn start_folder_runtime(&mut self, folder: &Path) {
        let Some(ref tag_store) = self.tag_store else {
            self.status_message = "Tag store not available — cannot index.".to_string();
            return;
        };
        tracing::info!("Starting folder runtime for: {}", folder.display());
        match FolderRuntime::start(folder, tag_store) {
            Ok(runtime) => {
                tracing::info!(
                    "Folder runtime started successfully for: {}",
                    folder.display()
                );
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
                // Fetch more results when tag filters are active so the post-filter
                // has enough documents to work with beyond the default 50-result window.
                let limit = if self.active_tag_filters.is_empty() {
                    50
                } else {
                    200
                };
                let request = SearchRequest::new(query.to_string()).with_limit(limit);
                match crate::search::engine::search_with_reader(fields, reader, &request) {
                    Ok(mut results) => {
                        // Batch-fetch tags for all results (single query, chunked by 500)
                        if let Some(ref store) = self.tag_store {
                            if !results.items.is_empty() {
                                let hashes: Vec<&str> = results
                                    .items
                                    .iter()
                                    .map(|r| r.content_hash.as_str())
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
                                // Capture true filtered count before truncation so
                                // the UI can show "X shown of N matches" correctly.
                                let filtered = results.items.len();
                                results.items.truncate(50);
                                results.total_hits = filtered;
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
                        // Pre-lowercase match terms so render_highlighted_snippet
                        // doesn't allocate per-term per-frame.
                        for item in &mut results.items {
                            item.match_terms =
                                item.match_terms.iter().map(|t| t.to_lowercase()).collect();
                        }
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
        let is_pdf = self.search_results[index].file_type == PDF_TYPE;
        let file_path = self.search_results[index].file_path.clone();
        let file_type = self.search_results[index].file_type.clone();

        if is_pdf {
            self.request_page_render();
            self.preview_text = None;
        } else {
            self.preview_texture = None;
            self.load_text_preview(&PathBuf::from(&file_path));
        }
        self.preview_file_type = Some(file_type);
    }

    /// Preview a file from the file browser (not a search result).
    fn browse_file(&mut self, path: &str) {
        self.browsed_file = Some(path.to_string());
        self.selected_result = None;
        self.selected_hash = None;
        self.current_page = 1;
        self.preview_texture = None;
        self.current_pdf_page_count = 0;

        let file_path = PathBuf::from(path);
        let is_pdf = path.to_lowercase().ends_with(".pdf");
        self.preview_file_type = Some(if is_pdf {
            PDF_TYPE.into()
        } else {
            "txt".into()
        });

        if is_pdf {
            self.latest_render_request_id += 1;
            self.current_preview_path = Some(file_path.clone());
            if let Some(ref tx) = self.render_request_tx {
                let _ = tx.send(RenderRequest {
                    request_id: self.latest_render_request_id,
                    path: file_path,
                    page: 1,
                    zoom: self.pdf_zoom,
                    target_width: self.preview_panel_size.0,
                    target_height: self.preview_panel_size.1,
                    priority: 1,
                });
            }
            self.preview_text = None;
        } else {
            self.preview_texture = None;
            self.load_text_preview(&file_path);
        }
    }

    /// Load a text file preview with 2MB cap.
    fn load_text_preview(&mut self, file_path: &Path) {
        const PREVIEW_MAX_BYTES: u64 = 2 * 1024 * 1024;
        match std::fs::metadata(file_path) {
            Ok(meta) if meta.len() > PREVIEW_MAX_BYTES => match std::fs::File::open(file_path) {
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
            },
            _ => match std::fs::read_to_string(file_path) {
                Ok(content) => {
                    self.preview_text = Some(content);
                }
                Err(e) => {
                    self.preview_text = Some(format!("Error reading file: {}", e));
                }
            },
        }
    }

    /// Send a render request for the current page of the selected result.
    fn request_page_render(&mut self) {
        let path = if let Some(selected) = self.selected_result {
            if selected >= self.search_results.len() {
                return;
            }
            PathBuf::from(&self.search_results[selected].file_path)
        } else if let Some(ref browsed) = self.browsed_file {
            PathBuf::from(browsed)
        } else {
            return;
        };
        self.latest_render_request_id += 1;
        self.current_preview_path = Some(path.clone());
        if let Some(ref tx) = self.render_request_tx {
            let request = RenderRequest {
                request_id: self.latest_render_request_id,
                path,
                page: self.current_page,
                zoom: self.pdf_zoom,
                target_width: self.preview_panel_size.0,
                target_height: self.preview_panel_size.1,
                priority: 1,
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
                let result = match rx.try_recv() {
                    Ok(r) => r,
                    Err(_) => break,
                };
                if result.request_id != self.latest_render_request_id {
                    continue;
                }
                if self.current_preview_path.as_deref() != Some(&result.path) {
                    continue;
                }
                if result.page_count > 0 {
                    self.current_pdf_page_count = result.page_count;
                }
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
                    // After a full-res render, prefetch the next and previous pages
                    if !result.is_preview && self.current_pdf_page_count > 0 {
                        if let Some(ref tx) = self.render_request_tx {
                            let path = if let Some(ref p) = self.current_preview_path {
                                p.clone()
                            } else {
                                continue;
                            };
                            // Prefetch next page
                            // Use request_id=0 to avoid collision with user-initiated requests.
                            // Prefetch results only warm the cache; the UI never accepts request_id=0.
                            let next = self.current_page + 1;
                            if next <= self.current_pdf_page_count {
                                let _ = tx.send(RenderRequest {
                                    request_id: 0,
                                    path: path.clone(),
                                    page: next,
                                    zoom: self.pdf_zoom,
                                    target_width: self.preview_panel_size.0,
                                    target_height: self.preview_panel_size.1,
                                    priority: 0,
                                });
                            }
                        }
                    }
                } else {
                    // Render failed — but keep any existing preview (e.g., low-res
                    // preview succeeded before full-res failed). Only clear if we
                    // have no texture at all (first render truly failed).
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
                    if self.file_browser_refresh_cooldown == 0 {
                        self.file_browser_dirty = true;
                        self.file_browser_refresh_cooldown = 30; // ~0.5s at 60fps
                    }
                }
                IndexerProgress::ScanComplete { total } => {
                    self.indexing_total = total;
                    self.indexing_done = total;
                    self.file_browser_dirty = true;
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

    /// Re-trigger auto-tagging for the currently selected document.
    /// Reads the file from disk and sends it through the auto-tagger.
    fn retag_selected(&mut self) {
        let hash = match &self.selected_hash { Some(h) => h.clone(), None => return };
        let tx = match &self.auto_tagger_tx { Some(t) => t.clone(), None => return };
        let store = match &self.tag_store { Some(s) => s.clone(), None => return };

        // Find the file path from search results
        let file_path = self
            .search_results
            .iter()
            .find(|r| r.content_hash == hash)
            .map(|r| r.file_path.clone());

        let Some(file_path) = file_path else { return };
        let path = std::path::Path::new(&file_path);
        if !path.exists() { return }

        // Re-extract text from the file
        let Ok(content) = std::fs::read_to_string(path) else { return };
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let content_hash_before_tag = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(file_name.as_bytes());
            hasher.update(content.as_bytes());
            hasher.finalize().to_hex().to_string()
        };

        // Clear old cache entry so Tier 1 doesn't short-circuit
        let _ = store.upsert_auto_tag_status(&hash, file_name, &content_hash_before_tag, "pending", None, None);

        let _ = tx.send(AutoTagRequest::TagDocument {
            content_hash: hash.clone(),
            filename: file_name.to_string(),
            text: content,
            content_hash_before_tag,
        });
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
    fn render_highlighted_snippet(ui: &mut egui::Ui, snippet: &str, lower_snippet: &str, match_terms: &[String]) {
        if match_terms.is_empty() {
            ui.label(RichText::new(snippet).color(Color32::GRAY));
            return;
        }

        let mut spans: Vec<(usize, usize)> = Vec::new();

        // Find all match positions (case-insensitive).
        // match_terms are pre-lowercased in do_search(), so no per-frame alloc here.
        for term in match_terms {
            if term.is_empty() {
                continue;
            }
            let mut search_start = 0;
            while let Some(pos) = lower_snippet[search_start..].find(term) {
                let abs_start = search_start + pos;
                let abs_end = abs_start + term.len();
                spans.push((abs_start, abs_end));
                search_start = abs_end;
            }
        }

        if spans.is_empty() {
            ui.label(RichText::new(snippet).color(Color32::GRAY));
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

        // Process deferred retag request (set by UI during rendering)
        if self.pending_retag {
            self.pending_retag = false;
            self.retag_selected();
        }

        // Check if a background folder switch has completed
        let pending = self.pending_runtime.clone();
        if let Some(pending) = pending {
            if let Ok(mut guard) = pending.lock() {
                if let Some(new_rt) = guard.take() {
                    // Background thread finished — wire up the new runtime
                    self.search_reader = Some(new_rt.search_reader.clone());
                    self.search_fields = Some(new_rt.search_fields.clone());
                    self.search_engine = Some(new_rt.search_engine.clone());
                    self.indexer_progress_rx = new_rt.progress_rx.clone();
                    self.tag_update_tx = new_rt.tag_tx.clone();
                    self.render_request_tx = Some(new_rt.render_tx.clone());
                    self.render_result_rx = Some(new_rt.render_result_rx.clone());
                    self.watcher_shutdown_flag = Some(new_rt.watcher_shutdown());
                    self.watcher_shutdown_tx = new_rt.watcher_shutdown_tx();
                    self.folder_runtime = Some(new_rt);
                    self.pending_runtime = None;
                    if let Some(ref folder) = self.config.watched_folder {
                        self.status_message = format!("Watching: {}", folder.display());
                    }
                }
            }
        }
        // Show background thread errors if any
        let err_flag = self.background_error.clone();
        if let Some(err_flag) = err_flag {
            let mut guard = err_flag.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(msg) = guard.take() {
                self.status_message = msg;
                self.background_error = None;
            }
        }

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

                    // Auto-tag status indicator
                    if self.auto_tag_enabled {
                        let (status_text, status_color) = if self.auto_tag_error.is_some() {
                            ("☁ Auto-tag: error", Color32::RED)
                        } else if self.auto_tag_progress.is_some() {
                            ("☁ Auto-tag: running...", Color32::from_rgb(100, 180, 255))
                        } else {
                            ("☁ Auto-tag: ready", Color32::from_rgb(100, 200, 100))
                        };
                        ui.label(RichText::new(status_text).size(10.0).color(status_color));

                        // Progress bar
                        if let Some((completed, total)) = self.auto_tag_progress {
                            let pct = completed as f32 / total.max(1) as f32;
                            ui.add(egui::ProgressBar::new(pct).text(format!("{completed}/{total}")));
                            if completed >= total {
                                self.auto_tag_progress = None;
                            }
                        }

                        if let Some(ref err) = self.auto_tag_error {
                            ui.label(RichText::new(err).size(10.0).color(Color32::RED));
                            if ui.small_button("Dismiss").clicked() {
                                self.auto_tag_error = None;
                            }
                        }
                        ui.separator();
                    } else {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("☁ Auto-tag: disabled").size(10.0).color(Color32::GRAY));
                            if ui.small_button("Enable").clicked() {
                                self.auto_tag_enabled = true;
                                let mut cfg = crate::auto_tagger::config::AutoTagConfig::load();
                                cfg.enabled = true;
                                let _ = cfg.save();
                            }
                        });
                        ui.separator();
                    }

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
                    ScrollArea::vertical().id_salt("tag_scroll").show(ui, |ui| {
                        let tag_names: Vec<(String, i64)> = self.tag_list_cache.clone();
                        for (tag_name, tag_id) in &tag_names {
                            let mut checked = self.active_tag_filters.contains(tag_name);
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut checked, "").changed() {
                                    self.toggle_tag_filter(tag_name);
                                }
                                ui.label(tag_name);

                                // Assign to selected document
                                if self.selected_result.is_some() && ui.small_button("📌").clicked()
                                {
                                    self.assign_tag_to_selected(*tag_id);
                                }
                            });
                        }

                        // ── Auto-tag suggestions ──
                        if let Some(hash) = &self.selected_hash {
                            if let Some(ref store) = self.tag_store {
                                if let Ok(Some(auto_status)) = store.auto_tag_status(hash) {
                                    if let Some(ref json) = auto_status.tags_json {
                                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
                                            // Render topic tags
                                            if let Some(tags) = value["tags"].as_array() {
                                                if !tags.is_empty() {
                                                    ui.separator();
                                                    ui.horizontal(|ui| {
                                                        ui.label(RichText::new("✨ Auto-tags").size(11.0).color(Color32::GRAY));
                                                        if ui.small_button("🔄 Re-tag").clicked() {
                                                            self.pending_retag = true;
                                                        }
                                                    });
                                                    let mut to_dismiss: Option<String> = None;
                                                    let mut to_toggle: Option<String> = None;
                                                    let accepted = self.accepted_auto_tags
                                                        .entry(hash.clone())
                                                        .or_default();
                                                    for tag_value in tags {
                                                        if let Some(tag_name) = tag_value.as_str() {
                                                            let is_accepted = accepted.contains(tag_name);
                                                            let frame = if is_accepted {
                                                                egui::Frame::default()
                                                                    .fill(Color32::from_rgb(40, 80, 40))
                                                                    .rounding(egui::Rounding::same(4.0))
                                                            } else {
                                                                egui::Frame::default()
                                                                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(100, 100, 100)))
                                                                    .rounding(egui::Rounding::same(4.0))
                                                            };
                                                            frame.show(ui, |ui| {
                                                                ui.horizontal(|ui| {
                                                                    ui.label("✨");
                                                                    if ui.selectable_label(is_accepted, tag_name)
                                                                        .on_hover_text(format!("AI tag for \"{}\"", auto_status.filename))
                                                                        .clicked() {
                                                                        to_toggle = Some(tag_name.to_string());
                                                                    }
                                                                    if ui.small_button("✕").clicked() {
                                                                        to_dismiss = Some(tag_name.to_string());
                                                                    }
                                                                });
                                                            });
                                                        }
                                                    }
                                                    if let Some(tag) = to_toggle {
                                                        if accepted.contains(&tag) {
                                                            accepted.remove(&tag);
                                                        } else {
                                                            accepted.insert(tag);
                                                        }
                                                    }
                                                    if let Some(tag) = to_dismiss {
                                                        accepted.remove(&tag);
                                                        if let Some(ref store) = self.tag_store {
                                                            let _ = store.dismiss_auto_tag(hash, &tag);
                                                        }
                                                    }
                                                }
                                            }
                                            // Render entity tags with type icons
                                            if let Some(entities) = value["entities"].as_object() {
                                                for (entity_type, entity_values) in entities {
                                                    let icon = match entity_type.as_str() {
                                                        "persons" => "👤",
                                                        "organizations" => "🏢",
                                                        "years" => "📅",
                                                        "doc_id" => "📄",
                                                        "amounts" => "💰",
                                                        _ => "🏷",
                                                    };
                                                    if let Some(arr) = entity_values.as_array() {
                                                        for ev in arr {
                                                            if let Some(ev_name) = ev.as_str() {
                                                                ui.horizontal(|ui| {
                                                                    ui.label(icon);
                                                                    ui.label(ev_name);
                                                                });
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    });
                });
        }

        // Tick down file browser refresh cooldown (rate-limits list_all_documents)
        self.file_browser_refresh_cooldown = self.file_browser_refresh_cooldown.saturating_sub(1);

        // ── Left panel: file browser ──
        if self.config.watched_folder.is_some() {
            // Refresh file list if dirty
            if self.file_browser_dirty {
                if let Some(ref store) = self.tag_store {
                    if let Ok(docs) = store.list_all_documents() {
                        self.file_browser_docs = docs;
                    }
                }
                self.file_browser_dirty = false;
            }

            SidePanel::left("file_browser")
                .resizable(true)
                .default_width(280.0)
                .show(ctx, |ui| {
                    ui.heading("📂 Files");
                    ui.label(format!("{} documents", self.file_browser_docs.len()));
                    ui.separator();

                    let mut clicked_file: Option<String> = None;
                    ScrollArea::vertical()
                        .id_salt("file_browser_scroll")
                        .show(ui, |ui| {
                            for doc in &self.file_browser_docs {
                                let file_name = std::path::Path::new(&doc.file_path)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(&doc.file_path);
                                let icon = match doc.file_type.as_str() {
                                    PDF_TYPE => "📄",
                                    "txt" => "📝",
                                    "md" => "📋",
                                    _ => "📎",
                                };
                                let label = format!("{} {}", icon, file_name);
                                let is_browsed =
                                    self.browsed_file.as_deref() == Some(&doc.file_path);
                                let resp = ui.selectable_label(is_browsed, label);
                                if resp.clicked() {
                                    clicked_file = Some(doc.file_path.clone());
                                }
                            }
                        });
                    if let Some(path) = clicked_file {
                        self.browse_file(&path);
                    }
                });
        }

        // ── Center: search + results / preview ──
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
                    let filters: Vec<&str> =
                        self.active_tag_filters.iter().map(|s| s.as_str()).collect();
                    for tag in &filters {
                        ui.label(format!("🔖 {}", tag));
                    }
                });
            }
            ui.separator();

            // ── Search results (shown when typing) ──
            if !self.search_query.trim().is_empty() {
                if self.total_hits > self.search_results.len() {
                    ui.label(format!(
                        "{} shown of {} matches",
                        self.search_results.len(),
                        self.total_hits
                    ));
                }

                let mut clicked_idx = self.clicked_index.take();
                ScrollArea::vertical()
                    .id_salt("results_scroll")
                    .show(ui, |ui| {
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
                                    &result.lower_snippet,
                                    &result.match_terms,
                                );
                            });
                        }
                    });

                if let Some(idx) = clicked_idx {
                    self.select_result(idx);
                }

                ui.separator();
            }

            // ── Preview / empty states ──

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
            } else if self.selected_result.is_none()
                && self.search_query.is_empty()
                && self.browsed_file.is_none()
            {
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
                let is_pdf = self.preview_file_type.as_deref() == Some(PDF_TYPE);
                let current_page = self.current_page;

                // PDF page navigation (before preview borrow)
                if is_pdf {
                    let at_last_page = self.current_pdf_page_count > 0
                        && self.current_page >= self.current_pdf_page_count;
                    let mut zoom_changed = false;
                    ui.horizontal(|ui| {
                        if ui.button("◀ Prev").clicked() && current_page > 1 {
                            self.current_page -= 1;
                            self.request_page_render();
                        }
                        if ui.button("🔍−").clicked() && self.pdf_zoom > 0.25 {
                            self.pdf_zoom -= 0.25;
                            zoom_changed = true;
                        }
                        ui.label(format!("{}%", (self.pdf_zoom * 100.0) as i32));
                        if ui.button("🔍+").clicked() && self.pdf_zoom < 4.0 {
                            self.pdf_zoom += 0.25;
                            zoom_changed = true;
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
                    if zoom_changed {
                        self.request_page_render();
                    }
                    ui.separator();
                }

                let tex_id = self
                    .preview_texture
                    .as_ref()
                    .expect("preview has texture")
                    .id();
                let tex_size = self
                    .preview_texture
                    .as_ref()
                    .expect("preview has texture")
                    .size_vec2();

                // Update preview panel size for display-resolution rendering
                let avail = ui.available_size();
                self.preview_panel_size = (avail.x as u32, avail.y as u32);

                ui.image(egui::ImageSource::Texture(egui::load::SizedTexture::new(
                    tex_id, tex_size,
                )));
            } else if self
                .browsed_file
                .as_ref()
                .is_some_and(|f| f.to_lowercase().ends_with(".pdf"))
            {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label("PDF preview not available.");
                    ui.label("Place pdfium.dll next to papervault.exe for PDF rendering.");
                });
            } else if let Some(ref text) = self.preview_text {
                ScrollArea::vertical()
                    .id_salt("preview_scroll")
                    .show(ui, |ui| {
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
                    ui.add_space(4.0);
                    ui.checkbox(&mut self.auto_tag_enabled, "Enable AI auto-tagging (DeepSeek)");
                    if self.auto_tag_enabled {
                        ui.label(
                            RichText::new("⚠ Document text is sent to DeepSeek (servers in China). Text is not stored. Requires DEEPSEEK_API_KEY env var.")
                                .size(11.0)
                                .color(Color32::from_rgb(180, 140, 60)),
                        );
                    }
                    ui.add_space(4.0);
                    if ui.button("Set Folder").clicked() {
                        let path = PathBuf::from(&self.folder_picker_input);
                        if path.exists() && path.is_dir() {
                            // Clear old channel references first — they hold sender clones
                            // that keep the renderer and indexer channels open, preventing
                            // stop() from joining the threads.
                            self.render_request_tx = None;
                            self.render_result_rx = None;
                            self.tag_update_tx = None;
                            self.watcher_shutdown_flag = None;
                            self.watcher_shutdown_tx = None;
                            self.search_reader = None;
                            self.search_fields = None;
                            self.search_engine = None;
                            self.indexer_progress_rx =
                                crossbeam::channel::unbounded::<IndexerProgress>().1;
                            let old_runtime = self.folder_runtime.take();
                            let new_folder = path.clone();
                            let tag_store = self.tag_store.clone();
                            self.pending_runtime = Some(Arc::new(Mutex::new(None)));
                            let pending = self
                                .pending_runtime
                                .as_ref()
                                .cloned()
                                .expect("pending_runtime just set");
                            let error_flag = Arc::new(Mutex::new(None::<String>));
                            self.background_error = Some(error_flag.clone());
                            std::thread::spawn(move || {
                                if let Some(rt) = old_runtime {
                                    let _ = rt.stop();
                                }
                                // Old engine released — start new runtime
                                if let Some(ref ts) = tag_store {
                                    match FolderRuntime::start(&new_folder, ts) {
                                        Ok(new_rt) => {
                                            *pending.lock().unwrap_or_else(|e| e.into_inner()) =
                                                Some(new_rt);
                                        }
                                        Err(e) => {
                                            tracing::error!(
                                                "Failed to start folder runtime: {}",
                                                e
                                            );
                                            *error_flag.lock().unwrap_or_else(|e| e.into_inner()) =
                                                Some(format!("Failed to start indexing: {}", e));
                                        }
                                    }
                                }
                            });
                            self.config.watched_folder = Some(path);
                            let _ = self.config.save();
                            self.folder_picker_open = false;
                            self.browsed_file = None;
                            self.status_message = "Switching folder...".to_string();
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

        // Drop sender clones before stop() — if these stay alive, the
        // indexer and renderer channels never close and thread joins hang.
        self.render_request_tx = None;
        self.watcher_shutdown_tx = None;
        self.watcher_shutdown_flag = None;

        // Gracefully stop the folder runtime (joins all background threads).
        if let Some(rt) = self.folder_runtime.take() {
            let _ = rt.stop();
        }
    }
}
