use crate::config::Config;
use crate::indexer::extractors::Extractor;
use crate::runtime::FolderRuntime;
use crate::search::engine::SearchEngine;
use crate::search::query::{SearchRequest, SearchResult};
use crate::search::schema::SchemaFields;

const PDF_TYPE: &str = "pdf";
use crate::tags::model::Tag;
use crate::tags::store::DocumentInfo;
use crate::tags::store::TagStore;
use crate::watcher::watcher::IndexerMessage;
use chrono::DateTime;
use crossbeam::channel::{Receiver, Sender};
use egui::{
    CentralPanel, Color32, Frame, RichText, ScrollArea, SidePanel, TextEdit, TopBottomPanel,
};
use raw_window_handle::HasWindowHandle;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::warn;

/// Which column the file browser is sorted by.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortColumn {
    Name,
    Date,
    Size,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SortDirection {
    Ascending,
    Descending,
}

/// Compute new sort state when a column header is clicked.
/// If the same column is clicked again, toggle direction; otherwise switch to the new column ascending.
fn handle_sort_column_click(
    current_column: SortColumn,
    current_direction: SortDirection,
    clicked_column: SortColumn,
) -> (SortColumn, SortDirection) {
    if current_column == clicked_column {
        let new_direction = match current_direction {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        };
        (current_column, new_direction)
    } else {
        (clicked_column, SortDirection::Ascending)
    }
}

/// Sort file browser documents in-place according to the given column and direction.
fn sort_docs(docs: &mut Vec<DocumentInfo>, column: SortColumn, direction: SortDirection) {
    match column {
        // Precompute lowercase keys once — comparing with fresh to_lowercase()
        // per comparison was O(n log n) allocations per refresh.
        SortColumn::Name => {
            let keys: Vec<String> = docs
                .iter()
                .map(|d| {
                    std::path::Path::new(&d.file_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&d.file_path)
                        .to_lowercase()
                })
                .collect();
            let mut order: Vec<usize> = (0..docs.len()).collect();
            order.sort_by(|&ia, &ib| {
                let cmp = keys[ia].cmp(&keys[ib]);
                match direction {
                    SortDirection::Ascending => cmp,
                    SortDirection::Descending => cmp.reverse(),
                }
            });
            let sorted: Vec<DocumentInfo> = order.iter().map(|&i| docs[i].clone()).collect();
            *docs = sorted;
        }
        SortColumn::Date => docs.sort_by(|a, b| {
            let cmp = a.modified_ts.cmp(&b.modified_ts);
            match direction {
                SortDirection::Ascending => cmp,
                SortDirection::Descending => cmp.reverse(),
            }
        }),
        SortColumn::Size => docs.sort_by(|a, b| {
            let cmp = a.file_size.cmp(&b.file_size);
            match direction {
                SortDirection::Ascending => cmp,
                SortDirection::Descending => cmp.reverse(),
            }
        }),
    }
}

/// Format a file size in bytes into a human-readable string.
/// Examples: "0 B", "512 B", "23.4 KB", "1.2 MB", "1.4 GB".
fn format_file_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut float_size = size as f64;
    let mut unit_idx = 0;
    while float_size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        float_size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{size} B")
    } else {
        format!("{float_size:.1} {}", UNITS[unit_idx])
    }
}

/// Pre-rendered file browser row — display strings are computed once per
/// refresh, so the per-frame render loop only borrows them (no allocations,
/// no date formatting, no size formatting per row per frame).
struct FileBrowserRow {
    file_path: String,
    content_hash: String,
    has_tags: bool,
    /// e.g. "📄 ✨ 2023-tax-return.pdf"
    label: String,
    /// e.g. "2023-11-15 10:30 · 1.2 MB"
    date_size: String,
}

/// Build pre-rendered rows for the file browser panel.
fn build_file_browser_rows(docs: &[DocumentInfo]) -> Vec<FileBrowserRow> {
    docs.iter()
        .map(|doc| {
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
            let sparkle = if doc.has_tags { "✨" } else { "" };
            // Cap pathological names — SelectableLabel wraps by default, and
            // virtualized rows need a uniform height (30px).
            const MAX_NAME_CHARS: usize = 40;
            let mut name: String = file_name.chars().take(MAX_NAME_CHARS).collect();
            if file_name.chars().count() > MAX_NAME_CHARS {
                name.push('…');
            }
            let date_str = DateTime::from_timestamp(doc.modified_ts as i64, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            let size_str = format_file_size(doc.file_size);
            FileBrowserRow {
                file_path: doc.file_path.clone(),
                content_hash: doc.content_hash.clone(),
                has_tags: doc.has_tags,
                label: format!("{} {}{}", icon, sparkle, name),
                date_size: format!("{} · {}", date_str, size_str),
            }
        })
        .collect()
}

/// Messages from the indexer thread to the UI thread.
#[derive(Debug, Clone)]
pub enum IndexerProgress {
    /// A single file was processed.
    Progress { processed: usize },
    /// Initial scan complete; total is the final count.
    ScanComplete { total: usize },
    /// An error occurred processing a file.
    Error { path: PathBuf, error: String },
    /// Fresh file-browser snapshot, computed on the indexer thread.
    /// Keeps list_all_documents (a full scan under the DB mutex) off the
    /// UI thread.
    DocsSnapshot { docs: Vec<DocumentInfo> },
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

/// Cached auto-tag display data for one document (parsed once, read many frames).
struct CachedAutoTag {
    filename: String,
    value: serde_json::Value,
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
    auto_tag_enabled: bool,
    auto_tag_progress: Option<(usize, usize)>,
    auto_tag_error: Option<String>,
    accepted_auto_tags: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// Parsed auto-tag data per content hash — avoids per-frame SQLite queries.
    /// Entry value: None = fetched but has no status/tags; Some = display data.
    auto_tag_cache: std::collections::HashMap<String, Option<std::sync::Arc<CachedAutoTag>>>,
    pending_retag: bool,
    pending_reindex: bool,
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
    /// Pre-rendered rows for the file browser (see FileBrowserRow).
    file_browser_rows: Vec<FileBrowserRow>,
    /// Whether the file browser needs a refresh.
    file_browser_dirty: bool,
    /// Cooldown counter: only refresh file browser every N frames during active indexing.
    file_browser_refresh_cooldown: usize,
    /// Periodic refresh timer: refresh every 5 seconds even when idle.
    file_browser_periodic_timer: usize,
    /// Currently previewed file path (from file browser, not search).
    browsed_file: Option<String>,
    /// Cached (hash, has_tags) of the browsed file — avoids a per-frame
    /// linear scan of file_browser_rows to render the footer tags.
    browsed_row: Option<(String, bool)>,
    /// Multi-selected file paths for batch tagging (Ctrl+click in file browser).
    selected_files: HashSet<String>,
    /// Current sort column for the file browser.
    sort_column: SortColumn,
    /// Current sort direction for the file browser.
    sort_direction: SortDirection,
    // ── System tray ──
    tray_cmd_rx: Option<Receiver<crate::tray::TrayCommand>>,
    auto_launch: Option<auto_launch::AutoLaunch>,
    minimized_to_tray: bool,
    should_exit: bool,
    /// Sync auto-start on first frame.
    auto_start_synced: Option<()>,
    /// Raw window handle for ShowWindow/SetForegroundWindow.
    hwnd: Option<isize>,
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
        tray_cmd_rx: Option<Receiver<crate::tray::TrayCommand>>,
        auto_launch: Option<auto_launch::AutoLaunch>,
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
            auto_tag_enabled: crate::auto_tagger::config::AutoTagConfig::load().enabled,
            auto_tag_progress: None,
            auto_tag_error: None,
            accepted_auto_tags: std::collections::HashMap::new(),
            auto_tag_cache: std::collections::HashMap::new(),
            pending_retag: false,
            pending_reindex: false,
            last_search_instant: None,
            pending_search: None,
            focus_search_next_frame: true,
            folder_runtime,
            auto_tagger_tx,
            pending_runtime: None,
            background_error: None,
            file_browser_docs: Vec::new(),
            file_browser_rows: Vec::new(),
            file_browser_dirty: true,
            file_browser_refresh_cooldown: 0,
            file_browser_periodic_timer: 0,
            browsed_file: None,
            browsed_row: None,
            selected_files: HashSet::new(),
            sort_column: SortColumn::Name,
            sort_direction: SortDirection::Ascending,
            // ── System tray ──
            tray_cmd_rx,
            auto_launch,
            minimized_to_tray: false,
            should_exit: false,
            auto_start_synced: Some(()),
            hwnd: None,
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
                self.auto_tagger_tx = runtime.auto_tagger_tx.clone();
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
        self.browsed_file = None;
        self.browsed_row = None;
        self.selected_result = Some(index);
        self.selected_hash = Some(self.search_results[index].content_hash.clone());
        self.current_page = 1;
        // Clear stale preview when switching documents
        self.preview_texture = None;
        self.current_pdf_page_count = 0;
        let file_path = self.search_results[index].file_path.clone();
        let is_pdf = self.search_results[index].file_type == PDF_TYPE
            || file_path.to_lowercase().ends_with(".pdf");
        let file_type = self.search_results[index].file_type.clone();

        if is_pdf {
            self.request_page_render();
            self.preview_text = None;
        } else {
            self.preview_texture = None;
            self.load_text_preview(&PathBuf::from(&file_path));
        }
        self.preview_file_type = Some(if is_pdf { PDF_TYPE.into() } else { file_type });
    }

    /// Preview a file from the file browser (not a search result).
    fn browse_file(&mut self, path: &str) {
        self.browsed_file = Some(path.to_string());
        self.refresh_browsed_row();
        self.selected_result = None;
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
    /// Handles non-UTF-8 files by falling back to lossy decode (same approach
    /// as `TextExtractor` in the indexer).
    fn load_text_preview(&mut self, file_path: &Path) {
        const PREVIEW_MAX_BYTES: u64 = 2 * 1024 * 1024;
        match std::fs::metadata(file_path) {
            Ok(meta) if meta.len() > PREVIEW_MAX_BYTES => {
                use std::io::Read;
                match std::fs::File::open(file_path) {
                    Ok(file) => {
                        let mut reader = file.take(PREVIEW_MAX_BYTES);
                        let mut bytes = Vec::new();
                        match reader.read_to_end(&mut bytes) {
                            Ok(_) => {
                                let content = String::from_utf8_lossy(&bytes).to_string();
                                self.preview_text =
                                    Some(content + "\n\n─── Preview truncated at 2 MB ───");
                            }
                            Err(e) => {
                                self.preview_text = Some(format!("Error reading file: {}", e));
                            }
                        }
                    }
                    Err(e) => {
                        self.preview_text = Some(format!("Error reading file: {}", e));
                    }
                }
            }
            _ => match std::fs::read(file_path) {
                Ok(bytes) => {
                    let content = String::from_utf8_lossy(&bytes).to_string();
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
                IndexerProgress::DocsSnapshot { docs } => {
                    self.apply_docs_snapshot(docs);
                }
            }
        }

        // Tags are now loaded at startup and refreshed only after create/delete.
        // No per-frame tag query.
    }

    /// Drop the stale display-cache entry for one hash (tag data changed).
    fn invalidate_auto_tag(&mut self, content_hash: &str) {
        self.auto_tag_cache.remove(content_hash);
    }

    /// Drop every stale display-cache entry (bulk re-queue events).
    fn clear_auto_tag_cache(&mut self) {
        self.auto_tag_cache.clear();
    }

    /// Take the last-completed hash from the auto-tagger signal and drop its
    /// stale display-cache entry (if any).
    fn consume_auto_tag_completed(
        signal: &std::sync::Mutex<Option<String>>,
        cache: &mut std::collections::HashMap<String, Option<std::sync::Arc<CachedAutoTag>>>,
    ) {
        if let Ok(mut g) = signal.lock() {
            if let Some(hash) = g.take() {
                cache.remove(&hash);
            }
        }
    }

    /// Re-derive the browsed-file footer cache from the current rows.
    fn refresh_browsed_row(&mut self) {
        self.browsed_row = self.browsed_file.as_ref().and_then(|b| {
            self.file_browser_rows
                .iter()
                .find(|r| r.file_path == *b)
                .map(|r| (r.content_hash.clone(), r.has_tags))
        });
    }

    /// Apply a file-browser snapshot computed on the indexer thread.
    fn apply_docs_snapshot(&mut self, docs: Vec<DocumentInfo>) {
        self.file_browser_docs = docs;
        sort_docs(
            &mut self.file_browser_docs,
            self.sort_column,
            self.sort_direction,
        );
        self.file_browser_rows = build_file_browser_rows(&self.file_browser_docs);
        self.refresh_browsed_row();
        self.file_browser_dirty = false;
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
        let hash = match &self.selected_hash {
            Some(h) => h.clone(),
            None => return,
        };
        let tx = match &self.auto_tagger_tx {
            Some(t) => t.clone(),
            None => return,
        };
        let store = match &self.tag_store {
            Some(s) => s.clone(),
            None => return,
        };

        // Find the file path from search results
        let file_path = self
            .search_results
            .iter()
            .find(|r| r.content_hash == hash)
            .map(|r| r.file_path.clone());

        let Some(file_path) = file_path else { return };
        let path = std::path::Path::new(&file_path);
        if !path.exists() {
            return;
        }

        // Re-extract text from the file
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let content_hash_before_tag = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(file_name.as_bytes());
            hasher.update(content.as_bytes());
            hasher.finalize().to_hex().to_string()
        };

        // Clear old cache entry so Tier 1 doesn't short-circuit
        let _ = store.upsert_auto_tag_status(
            &hash,
            file_name,
            &content_hash_before_tag,
            "pending",
            None,
            None,
        );
        // The pending reset wipes tags_json — drop the stale display cache entry.
        self.invalidate_auto_tag(&hash);

        let _ = tx.send(AutoTagRequest::TagDocument {
            content_hash: hash.clone(),
            filename: file_name.to_string(),
            text: content,
            content_hash_before_tag,
        });
    }

    /// Send an auto-tag request from the UI thread without ever blocking.
    /// Returns false when the queue is full or disconnected — the caller
    /// should stop queueing and report the partial count.
    fn try_queue_auto_tag(tx: &Sender<AutoTagRequest>, request: AutoTagRequest) -> bool {
        tx.try_send(request).is_ok()
    }

    /// Manually trigger auto-tagging for all currently selected files in the file browser.
    fn tag_selected_files(&mut self) {
        let tx = match &self.auto_tagger_tx {
            Some(t) => t.clone(),
            None => return,
        };
        let store = match &self.tag_store {
            Some(s) => s.clone(),
            None => return,
        };

        let files: Vec<String> = self.selected_files.iter().cloned().collect();
        let total = files.len();

        // Reset and set progress
        if let Some(ref rt) = self.folder_runtime {
            rt.auto_tag_progress
                .store(0, std::sync::atomic::Ordering::Relaxed);
        }
        self.auto_tag_progress = Some((0, total));

        let mut completed = 0usize;
        let mut queued = 0usize;
        for file_path in &files {
            let path = std::path::Path::new(file_path);
            if !path.exists() {
                tracing::warn!("Skipping missing file for tagging: {}", file_path);
                completed += 1;
                continue;
            }

            // Find the DocumentInfo for this path
            let doc = match self
                .file_browser_docs
                .iter()
                .find(|d| d.file_path == *file_path)
            {
                Some(d) => d.clone(),
                None => {
                    completed += 1;
                    continue;
                }
            };

            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Extract text: read directly for text files; PDFs use pdf_oxide
            let content = if doc.file_type == PDF_TYPE {
                let extractor = crate::indexer::extractors::pdf::PdfExtractor;
                match extractor.extract(path) {
                    Ok(Some(extracted)) => extracted.text,
                    _ => {
                        tracing::warn!("Could not extract PDF text for {}", file_path);
                        "[Document text could not be extracted. Use filename to determine topic.]"
                            .to_string()
                    }
                }
            } else {
                std::fs::read_to_string(path)
                    .unwrap_or_else(|_| format!("[Could not read file: {}]", file_name))
            };

            let content_hash_before_tag = {
                let mut hasher = blake3::Hasher::new();
                hasher.update(file_name.as_bytes());
                hasher.update(content.as_bytes());
                hasher.finalize().to_hex().to_string()
            };

            // Mark as pending so Tier 1 cache doesn't short-circuit
            let _ = store.upsert_auto_tag_status(
                &doc.content_hash,
                file_name,
                &content_hash_before_tag,
                "pending",
                None,
                None,
            );
            // Reset wipes tags_json — drop the stale display cache entry.
            self.invalidate_auto_tag(&doc.content_hash);

            let request = AutoTagRequest::TagDocument {
                content_hash: doc.content_hash.clone(),
                filename: file_name.to_string(),
                text: content,
                content_hash_before_tag,
            };
            if !Self::try_queue_auto_tag(&tx, request) {
                tracing::warn!(
                    "Auto-tagger queue full, stopping batch at {}/{}",
                    completed,
                    total
                );
                self.status_message = format!(
                    "Queue full — tagged {}/{} files (retry when current batch completes)",
                    queued, total
                );
                // No more sends will complete — clear the progress state so
                // the panel is not stuck at a partial count.
                self.auto_tag_progress = None;
                break;
            }
            tracing::info!("Queued for tagging: {}", file_name);
            queued += 1;
            completed += 1;
            self.auto_tag_progress = Some((completed, total));
        }

        self.selected_files.clear();
        self.file_browser_dirty = true;
        self.status_message = format!("Queued {} files for tagging", total);
    }

    /// Read the cached auto-tag display data for a hash.
    /// On cache miss, fetch the status once (single query) and fill the cache.
    fn cached_auto_tag(&mut self, content_hash: &str) -> Option<std::sync::Arc<CachedAutoTag>> {
        if let Some(entry) = self.auto_tag_cache.get(content_hash) {
            return entry.clone();
        }
        self.ensure_auto_tag_cache(&[content_hash]);
        self.auto_tag_cache
            .get(content_hash)
            .and_then(|e| e.clone())
    }

    /// Auto-tags for a hash from the cache. Empty when absent or not fetched.
    fn cached_auto_tags(&mut self, content_hash: &str) -> Vec<String> {
        if let Some(tags) = Self::tags_from_cache(&self.auto_tag_cache, content_hash) {
            return tags;
        }
        // Miss — fetch once, then read from the cache.
        self.cached_auto_tag(content_hash);
        Self::tags_from_cache(&self.auto_tag_cache, content_hash).unwrap_or_default()
    }

    /// Extract the "tags" array from cached auto-tag data.
    /// Returns None when the hash has no cache entry (not yet fetched).
    fn tags_from_cache(
        cache: &std::collections::HashMap<String, Option<std::sync::Arc<CachedAutoTag>>>,
        content_hash: &str,
    ) -> Option<Vec<String>> {
        let entry = cache.get(content_hash)?;
        entry.as_ref().map(|c| Self::tags_of(c))
    }

    fn tags_of(cached: &CachedAutoTag) -> Vec<String> {
        cached.value["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Batch-fetch auto-tag status for the given hashes (single SQL query) and
    /// fill the cache. Existing entries are kept. Hashes without a status row
    /// are marked as fetched (None) so they are not re-queried every frame.
    fn ensure_auto_tag_cache(&mut self, hashes: &[&str]) {
        let missing: Vec<&str> = hashes
            .iter()
            .copied()
            .filter(|h| !self.auto_tag_cache.contains_key(*h))
            .collect();
        if missing.is_empty() {
            return;
        }
        let Some(ref store) = self.tag_store else {
            // No store — mark as fetched so the UI never retries per frame.
            for hash in &missing {
                self.auto_tag_cache.insert((*hash).to_string(), None);
            }
            return;
        };
        match store.get_auto_tag_statuses_for_hashes(&missing) {
            Ok(map) => {
                for hash in &missing {
                    let entry = map.get(*hash).and_then(|status| {
                        status.tags_json.as_deref().and_then(|json| {
                            serde_json::from_str::<serde_json::Value>(json)
                                .ok()
                                .map(|value| {
                                    std::sync::Arc::new(CachedAutoTag {
                                        filename: status.filename.clone(),
                                        value,
                                    })
                                })
                        })
                    });
                    self.auto_tag_cache.insert((*hash).to_string(), entry);
                }
                // Bound the in-memory display cache (the DB cache has its own
                // cap) — a very large folder could otherwise grow it unbounded.
                if self.auto_tag_cache.len() > 4096 {
                    self.auto_tag_cache.clear();
                }
            }
            Err(e) => {
                tracing::warn!("Failed to batch-fetch auto-tag status: {}", e);
            }
        }
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

    /// Render a snippet with pre-computed highlight spans highlighted in gold.
    fn render_highlighted_snippet(ui: &mut egui::Ui, snippet: &str, spans: &[(usize, usize)]) {
        if spans.is_empty() {
            ui.label(RichText::new(snippet).size(12.0).color(Color32::GRAY));
            return;
        }

        // Render segments with alternating colors
        ui.horizontal(|ui| {
            let mut cursor = 0;
            for (start, end) in spans {
                if cursor < *start {
                    ui.label(
                        RichText::new(&snippet[cursor..*start])
                            .size(12.0)
                            .color(Color32::GRAY),
                    );
                }
                ui.label(
                    RichText::new(&snippet[*start..*end])
                        .size(12.0)
                        .color(Color32::BLACK)
                        .background_color(Color32::from_rgb(255, 215, 0)),
                );
                cursor = *end;
            }
            if cursor < snippet.len() {
                ui.label(
                    RichText::new(&snippet[cursor..])
                        .size(12.0)
                        .color(Color32::GRAY),
                );
            }
        });
    }

    /// Poll the tray command channel for Open/Exit commands.
    fn poll_tray_commands(&mut self, ctx: &egui::Context) {
        if let Some(ref rx) = self.tray_cmd_rx {
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    crate::tray::TrayCommand::Open => {
                        self.set_tool_window(false);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        self.show_window();
                        self.minimized_to_tray = false;
                    }
                    crate::tray::TrayCommand::Exit => {
                        // Immediately hide the window so it disappears from view
                        self.set_tool_window(true);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        // Move heavy cleanup (thread joins + Tantivy commit) to background
                        if let Some(rt) = self.folder_runtime.take() {
                            std::thread::spawn(move || {
                                let _ = rt.stop();
                            });
                        }
                        self.should_exit = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
    }

    /// Show and focus the window.
    fn show_window(&self) {
        #[cfg(windows)]
        if let Some(hwnd) = self.hwnd {
            unsafe {
                crate::win32::ShowWindow(hwnd, crate::win32::SW_SHOW);
                crate::win32::SetForegroundWindow(hwnd);
            }
        }
    }

    /// Toggle WS_EX_TOOLWINDOW to remove/restore taskbar visibility.
    fn set_tool_window(&self, tool: bool) {
        #[cfg(windows)]
        if let Some(hwnd) = self.hwnd {
            unsafe {
                use crate::win32::*;
                let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                let new_style = if tool {
                    (ex_style | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW
                } else {
                    (ex_style & !WS_EX_TOOLWINDOW) | WS_EX_APPWINDOW
                };
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
                SetWindowPos(
                    hwnd,
                    0,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }

    /// Sync the auto-start registry state with the config toggle.
    fn sync_auto_start(&mut self) {
        if let Some(ref auto) = self.auto_launch {
            let is_enabled = auto.is_enabled().unwrap_or(false);
            if self.config.start_with_windows != is_enabled {
                if self.config.start_with_windows {
                    if let Err(e) = auto.enable() {
                        warn!("Failed to enable auto-start: {}", e);
                    }
                } else if let Err(e) = auto.disable() {
                    warn!("Failed to disable auto-start: {}", e);
                }
            }
        }
    }
}

impl eframe::App for PapervaultApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ── Capture HWND on first frame ──
        if self.hwnd.is_none() {
            self.hwnd = extract_hwnd(_frame);
        }

        // ── System tray event processing (channel-based) ──
        self.poll_tray_commands(ctx);

        // ── Close interception (minimize to tray) ──
        if ctx.input(|i| i.viewport().close_requested()) {
            if !self.should_exit && self.config.minimize_to_tray && self.tray_cmd_rx.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                // Minimized keeps eframe ticking; WS_EX_TOOLWINDOW removes taskbar entry
                self.set_tool_window(true);
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                self.minimized_to_tray = true;
            }
            self.should_exit = false;
        }

        // ── Sync auto-start on first frame ──
        if self.auto_start_synced.take().is_some() {
            self.sync_auto_start();
        }

        // ── Keep eframe ticking when minimized to tray ──
        if self.minimized_to_tray {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }

        // Process deferred search result click from previous frame
        if let Some(idx) = self.clicked_index.take() {
            self.select_result(idx);
        }

        self.poll_channels(ctx);

        // Sync auto-tag progress from the shared counter (reindex batch only)
        // Manual batches via tag_selected_files() set progress locally and
        // use try_send — no shared counter tracking needed.
        if let Some(ref rt) = self.folder_runtime {
            // A doc finished tagging outside any explicit batch — drop its
            // stale display-cache entry so the UI shows the fresh tags.
            Self::consume_auto_tag_completed(&rt.auto_tag_completed, &mut self.auto_tag_cache);
            if let Some((prev_completed, total)) = self.auto_tag_progress {
                let completed = rt
                    .auto_tag_progress
                    .load(std::sync::atomic::Ordering::Relaxed);
                if completed > prev_completed {
                    // Progress moved — refresh file browser
                    self.file_browser_dirty = true;
                    // The completed doc's tags changed — drop its cached display data
                    if let Some(hash) = self.selected_hash.clone() {
                        self.invalidate_auto_tag(&hash);
                    }
                    if completed >= total {
                        self.auto_tag_progress = None;
                        self.status_message =
                            format!("Tagging complete — {} files processed", total);
                        if !self.search_query.trim().is_empty() {
                            self.do_search();
                        }
                    } else {
                        self.auto_tag_progress = Some((completed, total));
                    }
                }
            }
        }

        // Process deferred retag request (set by UI during rendering)
        if self.pending_retag {
            self.pending_retag = false;
            self.retag_selected();
        }

        // Process deferred reindex — queue ALL docs for auto-tagging
        if self.pending_reindex {
            self.pending_reindex = false;
            if let Some(ref store) = self.tag_store {
                if let Some(ref tx) = self.auto_tagger_tx {
                    if let Ok(docs) = store.list_all_documents() {
                        let total = docs.len();
                        // Reset progress counter
                        if let Some(ref rt) = self.folder_runtime {
                            rt.auto_tag_progress
                                .store(0, std::sync::atomic::Ordering::Relaxed);
                        }
                        self.auto_tag_progress = Some((0, total));
                        let mut queued = 0usize;
                        for doc in docs {
                            let file_name = std::path::Path::new(&doc.file_path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();
                            let content_hash_before_tag = {
                                let mut hasher = blake3::Hasher::new();
                                hasher.update(file_name.as_bytes());
                                hasher.update(b"reindex");
                                hasher.finalize().to_hex().to_string()
                            };
                            // Never block the UI thread on the bounded queue.
                            if !Self::try_queue_auto_tag(
                                tx,
                                AutoTagRequest::TagDocument {
                                    content_hash: doc.content_hash.clone(),
                                    filename: file_name,
                                    text: "[Document text could not be extracted. Use filename to determine topic.]".to_string(),
                                    content_hash_before_tag,
                                },
                            ) {
                                tracing::warn!(
                                    "Auto-tagger queue full during reindex — queued {}/{}",
                                    queued,
                                    total
                                );
                                self.status_message = format!(
                                    "Re-index queued {}/{} files — queue full (run again to continue)",
                                    queued, total
                                );
                                // No more sends will complete — clear the
                                // progress state so the panel is not stuck.
                                self.auto_tag_progress = None;
                                break;
                            }
                            queued += 1;
                        }
                        self.auto_tag_progress = Some((queued, total));
                        // All docs re-queued — any cached auto-tag display is now stale
                        self.clear_auto_tag_cache();
                    }
                }
            }
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
                        if ui.small_button("🔄 Re-index for tags").clicked() {
                            self.pending_reindex = true;
                        }

                        // Progress bar
                        if let Some((completed, total)) = self.auto_tag_progress {
                            let pct = completed as f32 / total.max(1) as f32;
                            ui.add(
                                egui::ProgressBar::new(pct).text(format!("{completed}/{total}")),
                            );
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
                            ui.label(
                                RichText::new("☁ Auto-tag: disabled")
                                    .size(10.0)
                                    .color(Color32::GRAY),
                            );
                            if ui.small_button("Enable").clicked() {
                                self.auto_tag_enabled = true;
                                let mut cfg = crate::auto_tagger::config::AutoTagConfig::load();
                                cfg.enabled = true;
                                let _ = cfg.save();
                                self.status_message =
                                    "Auto-tagging enabled. Re-import folder to tag existing files."
                                        .to_string();
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
                        let selected_hash = self.selected_hash.clone();
                        if let Some(hash) = selected_hash {
                            if let Some(status) = self.cached_auto_tag(&hash) {
                                let value = &status.value;
                                // Render topic tags
                                if let Some(tags) = value["tags"].as_array() {
                                    if !tags.is_empty() {
                                        ui.separator();
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new("✨ Auto-tags")
                                                    .size(11.0)
                                                    .color(Color32::GRAY),
                                            );
                                            if ui.small_button("🔄 Re-tag").clicked() {
                                                self.pending_retag = true;
                                            }
                                        });
                                        let mut to_dismiss: Option<String> = None;
                                        let mut to_toggle: Option<String> = None;
                                        let accepted = self
                                            .accepted_auto_tags
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
                                                        .stroke(egui::Stroke::new(
                                                            1.0_f32,
                                                            Color32::from_rgb(100, 100, 100),
                                                        ))
                                                        .rounding(egui::Rounding::same(4.0))
                                                };
                                                frame.show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.label("✨");
                                                        if ui
                                                            .selectable_label(is_accepted, tag_name)
                                                            .on_hover_text(format!(
                                                                "AI tag for \"{}\"",
                                                                status.filename
                                                            ))
                                                            .clicked()
                                                        {
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
                                                let _ = store.dismiss_auto_tag(&hash, &tag);
                                            }
                                            // The stored JSON changed — drop the stale entry
                                            self.invalidate_auto_tag(&hash);
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
                    });
                });
        }

        // Tick down file browser refresh cooldown
        self.file_browser_refresh_cooldown = self.file_browser_refresh_cooldown.saturating_sub(1);
        // Periodic refresh every 5 seconds (300 frames at 60fps) — picks up new tags
        self.file_browser_periodic_timer += 1;
        if self.file_browser_periodic_timer >= 300 {
            self.file_browser_periodic_timer = 0;
            self.file_browser_dirty = true;
        }

        // ── Left panel: file browser ──
        if self.config.watched_folder.is_some() {
            // Refresh request: with a live runtime the indexer thread computes
            // the snapshot (list_all_documents is a full scan under the DB
            // mutex — it must not run on the UI thread). The result arrives
            // as IndexerProgress::DocsSnapshot. Without a runtime, fall back
            // to an inline refresh.
            if self.file_browser_dirty {
                if let Some(ref rt) = self.folder_runtime {
                    rt.browser_refresh_flag.store(true, Ordering::Relaxed);
                } else {
                    if let Some(ref store) = self.tag_store {
                        if let Ok(docs) = store.list_all_documents() {
                            self.file_browser_docs = docs;
                        }
                    }
                    sort_docs(
                        &mut self.file_browser_docs,
                        self.sort_column,
                        self.sort_direction,
                    );
                    // Pre-render display strings once per refresh — the per-frame
                    // loop below must not format dates/sizes or allocate per row.
                    self.file_browser_rows = build_file_browser_rows(&self.file_browser_docs);
                    self.refresh_browsed_row();
                    self.file_browser_dirty = false;
                }
            }

            // Ensure the browsed file's auto-tags are cached before the row loop
            if let Some(ref browsed) = self.browsed_file {
                let hash = self
                    .file_browser_docs
                    .iter()
                    .find(|d| d.file_path == *browsed)
                    .map(|d| d.content_hash.clone());
                if let Some(hash) = hash {
                    self.ensure_auto_tag_cache(&[hash.as_str()]);
                }
            }

            SidePanel::left("file_browser")
                .resizable(true)
                .default_width(280.0)
                .show(ctx, |ui| {
                    ui.heading("📂 Files");
                    ui.horizontal(|ui| {
                        ui.label(format!("{} documents", self.file_browser_docs.len()));
                        // Tag Selected button — only when files selected and auto-tagger available
                        if !self.selected_files.is_empty() && self.auto_tagger_tx.is_some() {
                            let n = self.selected_files.len();
                            if ui.button(format!("🏷 Tag Selected ({})", n)).clicked() {
                                self.tag_selected_files();
                            }
                        }
                    });
                    ui.separator();

                    // Column header row
                    ui.horizontal(|ui| {
                        // Name
                        let name_indicator = match self.sort_column {
                            SortColumn::Name => match self.sort_direction {
                                SortDirection::Ascending => " ▲",
                                SortDirection::Descending => " ▼",
                            },
                            _ => "",
                        };
                        let name_text = RichText::new(format!("Name{}", name_indicator)).strong();
                        if ui.selectable_label(false, name_text).clicked() {
                            let (col, dir) = handle_sort_column_click(
                                self.sort_column,
                                self.sort_direction,
                                SortColumn::Name,
                            );
                            self.sort_column = col;
                            self.sort_direction = dir;
                            self.file_browser_dirty = true;
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Size
                            let size_indicator = match self.sort_column {
                                SortColumn::Size => match self.sort_direction {
                                    SortDirection::Ascending => " ▲",
                                    SortDirection::Descending => " ▼",
                                },
                                _ => "",
                            };
                            let size_text =
                                RichText::new(format!("Size{}", size_indicator)).strong();
                            if ui.selectable_label(false, size_text).clicked() {
                                let (col, dir) = handle_sort_column_click(
                                    self.sort_column,
                                    self.sort_direction,
                                    SortColumn::Size,
                                );
                                self.sort_column = col;
                                self.sort_direction = dir;
                                self.file_browser_dirty = true;
                            }

                            ui.label("  ");
                            // Date
                            let date_indicator = match self.sort_column {
                                SortColumn::Date => match self.sort_direction {
                                    SortDirection::Ascending => " ▲",
                                    SortDirection::Descending => " ▼",
                                },
                                _ => "",
                            };
                            let date_text =
                                RichText::new(format!("Modified{}", date_indicator)).strong();
                            if ui.selectable_label(false, date_text).clicked() {
                                let (col, dir) = handle_sort_column_click(
                                    self.sort_column,
                                    self.sort_direction,
                                    SortColumn::Date,
                                );
                                self.sort_column = col;
                                self.sort_direction = dir;
                                self.file_browser_dirty = true;
                            }
                        });
                    });

                    let ctrl_held = ui.input(|i| i.modifiers.ctrl);
                    let mut clicked_file: Option<String> = None;
                    let mut toggled_file: Option<String> = None;
                    // Fixed row height — required by show_rows virtualization.
                    // (The browsed file's auto-tags moved to a footer below the
                    // list so rows stay uniform.)
                    let row_height = 30.0;
                    ScrollArea::vertical()
                        .id_salt("file_browser_scroll")
                        .show_rows(ui, row_height, self.file_browser_rows.len(), |ui, range| {
                            for idx in range {
                                let row = &self.file_browser_rows[idx];
                                let is_selected = self.selected_files.contains(&row.file_path);
                                let is_browsed =
                                    self.browsed_file.as_deref() == Some(&row.file_path);
                                // Background for selected files
                                let sel_bg = if is_selected {
                                    Color32::from_rgb(40, 80, 40)
                                } else {
                                    Color32::TRANSPARENT
                                };
                                ui.set_min_height(row_height);
                                let resp = Frame::default()
                                    .fill(sel_bg)
                                    .rounding(egui::Rounding::same(2.0))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            // Left: clickable filename
                                            // (selectable_label provides click
                                            // sense and browsed highlight)
                                            let label_resp =
                                                ui.selectable_label(is_browsed, &row.label);
                                            // Right: date · size, pushed to the edge
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        RichText::new(&row.date_size)
                                                            .size(11.0)
                                                            .color(Color32::from_rgb(
                                                                150, 150, 150,
                                                            )),
                                                    );
                                                },
                                            );
                                            label_resp
                                        })
                                        .inner
                                    });
                                if resp.inner.clicked() {
                                    if ctrl_held {
                                        toggled_file = Some(row.file_path.clone());
                                    } else {
                                        self.selected_hash = Some(row.content_hash.clone());
                                        clicked_file = Some(row.file_path.clone());
                                    }
                                }
                            }
                        });
                    // Auto-tags for the browsed file — shown below the list so
                    // virtualized rows keep a uniform height. browsed_row is
                    // cached at refresh time (no per-frame scan of all rows).
                    if let Some((ref hash, has_tags)) = self.browsed_row {
                        if has_tags {
                            let tags = Self::tags_from_cache(&self.auto_tag_cache, hash)
                                .unwrap_or_default();
                            if !tags.is_empty() {
                                ui.add_space(4.0);
                                let preview: Vec<&str> =
                                    tags.iter().map(|s| s.as_str()).take(5).collect();
                                ui.label(
                                    RichText::new(format!("🏷 {}", preview.join(", ")))
                                        .size(10.0)
                                        .color(Color32::from_rgb(140, 160, 200)),
                                );
                            }
                        }
                    }
                    if let Some(path) = clicked_file {
                        self.selected_files.clear();
                        self.browse_file(&path);
                    }
                    if let Some(path) = toggled_file {
                        if self.selected_files.contains(&path) {
                            self.selected_files.remove(&path);
                        } else {
                            self.selected_files.insert(path);
                        }
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
            // Sizing constants for search results; row height relates to font sizes.
            let result_filename_size = 11.0_f32;
            let tag_label_size = 14.0_f32;

            // Active tag filter chips
            if !self.active_tag_filters.is_empty() {
                ui.horizontal(|ui| {
                    let filters: Vec<&str> =
                        self.active_tag_filters.iter().map(|s| s.as_str()).collect();
                    for tag in &filters {
                        ui.label(RichText::new(format!("🔖 {}", tag)).size(tag_label_size));
                    }
                });
            }
            ui.separator();

            // ── Split remaining space: results (left) + preview (right) ──
            // ── Search results ──
            if has_search_query {
                if self.total_hits > self.search_results.len() {
                    ui.label(format!(
                        "{} shown of {} matches",
                        self.search_results.len(),
                        self.total_hits
                    ));
                }

                // Batch-fill the auto-tag cache for all displayed results —
                // the row loop below must only read the cache (no per-row SQL).
                {
                    let missing: Vec<String> = self
                        .search_results
                        .iter()
                        .filter(|r| !self.auto_tag_cache.contains_key(&r.content_hash))
                        .map(|r| r.content_hash.clone())
                        .collect();
                    if !missing.is_empty() {
                        let refs: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
                        self.ensure_auto_tag_cache(&refs);
                    }
                }

                let mut clicked_idx: Option<usize> = None;
                let max_h = ui.available_size().y;
                ScrollArea::vertical()
                    .id_salt("results_scroll")
                    .max_height(max_h)
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
                                let resp = ui.add(egui::SelectableLabel::new(
                                    selected,
                                    RichText::new(format!(
                                        "{} ({})",
                                        result.file_name, result.match_count
                                    ))
                                    .size(result_filename_size)
                                    .strong(),
                                ));
                                if resp.clicked() {
                                    clicked_idx = Some(i);
                                }

                                if !result.tags.is_empty() {
                                    ui.horizontal(|ui| {
                                        for t in &result.tags {
                                            ui.label(
                                                RichText::new(format!("🏷{}", t))
                                                    .size(tag_label_size),
                                            );
                                        }
                                    });
                                }
                                if !result.content_hash.is_empty() {
                                    let auto_tags = Self::tags_from_cache(
                                        &self.auto_tag_cache,
                                        &result.content_hash,
                                    )
                                    .unwrap_or_default();
                                    if !auto_tags.is_empty() {
                                        ui.horizontal(|ui| {
                                            for t in &auto_tags {
                                                ui.label(
                                                    RichText::new(format!("✨{}", t))
                                                        .size(tag_label_size)
                                                        .color(Color32::from_rgb(160, 190, 140)),
                                                );
                                            }
                                        });
                                    }
                                }

                                Self::render_highlighted_snippet(
                                    ui,
                                    &result.snippet,
                                    &result.highlight_spans,
                                );
                            });
                        }
                    });

                // Defer to next frame's update() so SidePanel::right sees selected_hash
                if let Some(idx) = clicked_idx {
                    self.clicked_index = Some(idx);
                    ctx.request_repaint();
                }
            }

            // ── Right panel: preview ──
            SidePanel::right("preview_panel")
                .resizable(true)
                .default_width(ctx.screen_rect().width() * 0.45)
                .min_width(250.0)
                .show(ctx, |ui| {
                    // ── Show tags at top of preview when a file is selected ──
                    let selected_hash = self.selected_hash.clone();
                    if let Some(hash) = selected_hash {
                        let tags = self.cached_auto_tags(&hash);
                        if !tags.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.label("🏷");
                                for tag in &tags {
                                    ui.label(
                                        RichText::new(tag)
                                            .size(13.0)
                                            .color(Color32::from_rgb(200, 220, 255)),
                                    );
                                }
                            });
                            ui.separator();
                        }
                    }

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
        });

        // ── Bottom: status bar ──
        TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status_message);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut changed = false;
                    if ui
                        .add(egui::Checkbox::new(
                            &mut self.config.minimize_to_tray,
                            "Minimize to tray",
                        ))
                        .changed()
                    {
                        changed = true;
                    }
                    if self.auto_launch.is_some()
                        && ui
                            .add(egui::Checkbox::new(
                                &mut self.config.start_with_windows,
                                "Start with Windows",
                            ))
                            .changed()
                    {
                        self.sync_auto_start();
                        changed = true;
                    }
                    if changed {
                        if let Err(e) = self.config.save() {
                            warn!("Failed to save config: {}", e);
                        }
                    }
                });
            });
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
                                // Clean up old database entries and Tantivy index
                                if let Some(ref ts) = tag_store {
                                    let _ = ts.clear_all_documents();
                                }
                                // Delete old Tantivy index
                                let index_dir = dirs_next::data_local_dir()
                                    .unwrap_or_else(|| PathBuf::from("."))
                                    .join("papervault")
                                    .join("indexes");
                                let _ = std::fs::remove_dir_all(&index_dir);
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

        // Folder runtime cleanup was moved to a background thread when
        // the user clicked Exit — to avoid a multi-second UI freeze.
        // If still present (e.g., direct window close), do inline stop.
        if let Some(rt) = self.folder_runtime.take() {
            let _ = rt.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_doc(path: &str, file_type: &str, size: u64, ts: u64) -> DocumentInfo {
        DocumentInfo {
            file_path: path.to_string(),
            file_type: file_type.to_string(),
            content_hash: String::new(),
            file_size: size,
            modified_ts: ts,
            has_tags: false,
        }
    }

    // ── sort_docs tests ──

    #[test]
    fn sort_by_name_ascending() {
        let mut docs = vec![
            make_doc("/z.pdf", "pdf", 100, 300),
            make_doc("/a.txt", "txt", 200, 100),
            make_doc("/m.md", "md", 300, 200),
        ];
        sort_docs(&mut docs, SortColumn::Name, SortDirection::Ascending);
        let names: Vec<&str> = docs.iter().map(|d| d.file_path.as_str()).collect();
        assert_eq!(names, vec!["/a.txt", "/m.md", "/z.pdf"]);
    }

    #[test]
    fn sort_by_name_descending() {
        let mut docs = vec![
            make_doc("/a.txt", "txt", 200, 100),
            make_doc("/z.pdf", "pdf", 100, 300),
            make_doc("/m.md", "md", 300, 200),
        ];
        sort_docs(&mut docs, SortColumn::Name, SortDirection::Descending);
        let names: Vec<&str> = docs.iter().map(|d| d.file_path.as_str()).collect();
        assert_eq!(names, vec!["/z.pdf", "/m.md", "/a.txt"]);
    }

    #[test]
    fn sort_by_name_case_insensitive() {
        let mut docs = vec![
            make_doc("/Z.pdf", "pdf", 100, 300),
            make_doc("/a.txt", "txt", 200, 100),
        ];
        sort_docs(&mut docs, SortColumn::Name, SortDirection::Ascending);
        let names: Vec<&str> = docs.iter().map(|d| d.file_path.as_str()).collect();
        assert_eq!(names, vec!["/a.txt", "/Z.pdf"]);
    }

    #[test]
    fn sort_by_date_ascending() {
        let mut docs = vec![
            make_doc("/z.pdf", "pdf", 100, 300),
            make_doc("/a.txt", "txt", 200, 100),
            make_doc("/m.md", "md", 300, 200),
        ];
        sort_docs(&mut docs, SortColumn::Date, SortDirection::Ascending);
        let ts: Vec<u64> = docs.iter().map(|d| d.modified_ts).collect();
        assert_eq!(ts, vec![100, 200, 300]);
    }

    #[test]
    fn sort_by_date_descending() {
        let mut docs = vec![
            make_doc("/a.txt", "txt", 200, 100),
            make_doc("/z.pdf", "pdf", 100, 300),
            make_doc("/m.md", "md", 300, 200),
        ];
        sort_docs(&mut docs, SortColumn::Date, SortDirection::Descending);
        let ts: Vec<u64> = docs.iter().map(|d| d.modified_ts).collect();
        assert_eq!(ts, vec![300, 200, 100]);
    }

    #[test]
    fn sort_by_date_equal_timestamps() {
        // Equal timestamps: original relative order preserved (stable sort)
        let mut docs = vec![
            make_doc("/a.txt", "txt", 200, 100),
            make_doc("/b.pdf", "pdf", 100, 100),
        ];
        sort_docs(&mut docs, SortColumn::Date, SortDirection::Ascending);
        let names: Vec<&str> = docs.iter().map(|d| d.file_path.as_str()).collect();
        assert_eq!(names, vec!["/a.txt", "/b.pdf"]);
    }

    #[test]
    fn sort_by_size_ascending() {
        let mut docs = vec![
            make_doc("/z.pdf", "pdf", 300, 100),
            make_doc("/a.txt", "txt", 100, 200),
            make_doc("/m.md", "md", 200, 300),
        ];
        sort_docs(&mut docs, SortColumn::Size, SortDirection::Ascending);
        let sizes: Vec<u64> = docs.iter().map(|d| d.file_size).collect();
        assert_eq!(sizes, vec![100, 200, 300]);
    }

    #[test]
    fn sort_by_size_descending() {
        let mut docs = vec![
            make_doc("/a.txt", "txt", 100, 200),
            make_doc("/z.pdf", "pdf", 300, 100),
            make_doc("/m.md", "md", 200, 300),
        ];
        sort_docs(&mut docs, SortColumn::Size, SortDirection::Descending);
        let sizes: Vec<u64> = docs.iter().map(|d| d.file_size).collect();
        assert_eq!(sizes, vec![300, 200, 100]);
    }

    #[test]
    fn sort_toggle_same_column_toggles_direction() {
        // Name asc → click Name → Name desc
        let (col, dir) =
            handle_sort_column_click(SortColumn::Name, SortDirection::Ascending, SortColumn::Name);
        assert_eq!(col, SortColumn::Name);
        assert_eq!(dir, SortDirection::Descending);

        // Name desc → click Name → Name asc
        let (col, dir) = handle_sort_column_click(
            SortColumn::Name,
            SortDirection::Descending,
            SortColumn::Name,
        );
        assert_eq!(col, SortColumn::Name);
        assert_eq!(dir, SortDirection::Ascending);
    }

    #[test]
    fn sort_toggle_switches_to_new_column_ascending() {
        // Name asc → click Date → Date asc
        let (col, dir) =
            handle_sort_column_click(SortColumn::Name, SortDirection::Ascending, SortColumn::Date);
        assert_eq!(col, SortColumn::Date);
        assert_eq!(dir, SortDirection::Ascending);

        // Name desc → click Size → Size asc
        let (col, dir) = handle_sort_column_click(
            SortColumn::Name,
            SortDirection::Descending,
            SortColumn::Size,
        );
        assert_eq!(col, SortColumn::Size);
        assert_eq!(dir, SortDirection::Ascending);
    }

    #[test]
    fn sort_state_persists_across_refresh() {
        // Simulate a refresh cycle: sort, re-create docs, sort again with same state.
        // The sort state (column, direction) must survive.
        let column = SortColumn::Date;
        let direction = SortDirection::Descending;

        let mut docs = vec![
            make_doc("/a.txt", "txt", 200, 100),
            make_doc("/z.pdf", "pdf", 100, 300),
            make_doc("/m.md", "md", 300, 200),
        ];
        sort_docs(&mut docs, column, direction);
        let ts: Vec<u64> = docs.iter().map(|d| d.modified_ts).collect();
        assert_eq!(ts, vec![300, 200, 100]);

        // Simulate a re-fetch from DB (same data, new Vec)
        let mut docs2 = vec![
            make_doc("/a.txt", "txt", 200, 100),
            make_doc("/z.pdf", "pdf", 100, 300),
            make_doc("/m.md", "md", 300, 200),
        ];
        // Sort again with the SAME state (state persisted across refresh)
        sort_docs(&mut docs2, column, direction);
        let ts2: Vec<u64> = docs2.iter().map(|d| d.modified_ts).collect();
        assert_eq!(ts2, vec![300, 200, 100]);
    }

    #[test]
    fn sort_by_size_equal_sizes() {
        let mut docs = vec![
            make_doc("/b.pdf", "pdf", 100, 200),
            make_doc("/a.txt", "txt", 100, 100),
        ];
        sort_docs(&mut docs, SortColumn::Size, SortDirection::Ascending);
        let names: Vec<&str> = docs.iter().map(|d| d.file_path.as_str()).collect();
        assert_eq!(names, vec!["/b.pdf", "/a.txt"]);
    }

    // ── build_file_browser_rows tests ──

    #[test]
    fn build_file_browser_rows_precomputes_display_strings() {
        let mut docs = vec![
            make_doc("/docs/Tax Return.pdf", "pdf", 1_048_576, 1700044200),
            make_doc("/docs/notes.txt", "txt", 512, 0),
            make_doc("/docs/archive.tar.gz", "gz", 1024, 0),
        ];
        docs[0].has_tags = true;
        docs[0].content_hash = "abc".into();

        let rows = build_file_browser_rows(&docs);
        assert_eq!(rows.len(), 3);
        // PDF row: icon + sparkle + name; size + separator formatted once.
        assert_eq!(rows[0].label, "📄 ✨Tax Return.pdf");
        assert!(
            rows[0].date_size.contains("1.0 MB"),
            "{}",
            rows[0].date_size
        );
        assert!(rows[0].date_size.contains(" · "), "{}", rows[0].date_size);
        // Plain txt row: no sparkle.
        assert_eq!(rows[1].label, "📝 notes.txt");
        assert!(rows[1].date_size.contains("512 B"), "{}", rows[1].date_size);
        // Unknown type: generic icon.
        assert_eq!(rows[2].label, "📎 archive.tar.gz");
        // Identity fields survive for click handling.
        assert_eq!(rows[0].file_path, "/docs/Tax Return.pdf");
        assert_eq!(rows[0].content_hash, "abc");
        assert!(rows[0].has_tags);
        assert!(!rows[1].has_tags);
    }

    #[test]
    fn apply_docs_snapshot_sorts_and_builds_rows() {
        let mut app = make_transition_test_app();
        let docs = vec![
            make_doc("/b.txt", "txt", 100, 2),
            make_doc("/a.pdf", "pdf", 200, 1),
        ];
        app.apply_docs_snapshot(docs);

        assert_eq!(app.file_browser_docs.len(), 2);
        assert_eq!(app.file_browser_rows.len(), 2);
        assert!(!app.file_browser_dirty, "snapshot satisfies the refresh");
        // Default sort is Name ascending — a.pdf first, rows aligned with docs.
        assert_eq!(app.file_browser_docs[0].file_path, "/a.pdf");
        assert!(app.file_browser_rows[0].label.contains("a.pdf"));
        assert!(app.file_browser_rows[1].label.contains("b.txt"));
    }

    // ── select_result / browse_file state transition tests ──

    /// Minimal PapervaultApp for testing state transitions.
    fn make_transition_test_app() -> PapervaultApp {
        let (_progress_tx, progress_rx) = crossbeam::channel::unbounded();
        PapervaultApp::new(
            Config::default(),
            None, // search_engine
            None, // search_reader
            None, // search_fields
            progress_rx,
            None, // tag_tx
            None, // render_tx
            None, // render_rx
            None, // tag_store
            None, // watcher_shutdown_flag
            None, // watcher_shutdown_tx
            None, // folder_runtime
            None, // auto_tagger_tx
            None, // tray_cmd_rx
            None, // auto_launch
        )
    }

    fn make_search_result(path: &str, hash: &str, file_type: &str) -> SearchResult {
        SearchResult {
            file_name: std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_string(),
            file_path: path.to_string(),
            file_type: file_type.to_string(),
            snippet: String::new(),
            match_count: 1,
            match_terms: std::sync::Arc::new([]),
            highlight_spans: Vec::new(),
            content_hash: hash.to_string(),
            tags: vec![],
            lower_snippet: String::new(),
        }
    }

    fn make_pdf_search_result(path: &str, hash: &str) -> SearchResult {
        SearchResult {
            file_name: std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_string(),
            file_path: path.to_string(),
            file_type: "pdf".to_string(),
            snippet: String::new(),
            match_count: 1,
            match_terms: std::sync::Arc::new([]),
            highlight_spans: Vec::new(),
            content_hash: hash.to_string(),
            tags: vec![],
            lower_snippet: String::new(),
        }
    }

    #[test]
    fn select_result_pdf_goes_to_render_not_text_preview() {
        let mut app = make_transition_test_app();
        app.search_results = vec![make_pdf_search_result("/docs/report.pdf", "hash1")];
        app.total_hits = 1;

        app.select_result(0);

        // PDF search results must route to request_page_render,
        // not load_text_preview — preview_text must remain None
        assert!(
            app.preview_text.is_none(),
            "PDF search result must NOT set preview_text (would show garbled text)"
        );
        assert_eq!(app.selected_result, Some(0));
        assert!(app.browsed_file.is_none());
        assert_eq!(
            app.preview_file_type.as_deref(),
            Some("pdf"),
            "preview_file_type must be 'pdf' for PDF search results"
        );
    }

    #[test]
    fn select_result_clears_browsed_file() {
        let mut app = make_transition_test_app();
        // Simulate: user browsed a PDF in file browser
        app.browsed_file = Some("/docs/report.pdf".to_string());
        // Add a search result so select_result has something to select
        app.search_results = vec![make_search_result("/docs/notes.txt", "hash1", "txt")];
        app.total_hits = 1;

        app.select_result(0);

        assert!(
            app.browsed_file.is_none(),
            "select_result must clear stale browsed_file"
        );
        assert_eq!(app.selected_result, Some(0));
    }

    #[test]
    fn select_result_pdf_uses_extension_fallback() {
        let mut app = make_transition_test_app();
        // Simulate: index has wrong file_type ("") but path ends with .pdf
        let mut result = make_search_result("/docs/report.pdf", "hash1", "txt");
        result.file_type = String::new(); // broken index
        app.search_results = vec![result];
        app.total_hits = 1;

        app.select_result(0);

        assert!(
            app.preview_text.is_none(),
            "Even with empty file_type, .pdf extension must route to render"
        );
    }

    #[test]
    fn select_result_clears_browsed_file_with_pdf_path() {
        let mut app = make_transition_test_app();
        // Simulate: user browsed a PDF, then clicked a search result
        app.browsed_file = Some("/docs/report.pdf".to_string());
        app.search_results = vec![make_search_result("/docs/notes.txt", "hash1", "txt")];
        app.total_hits = 1;

        app.select_result(0);

        assert!(
            app.browsed_file.is_none(),
            "browsed_file must be cleared even when it ends with .pdf"
        );
    }

    #[test]
    fn select_result_out_of_bounds_does_not_panic() {
        let mut app = make_transition_test_app();
        app.browsed_file = Some("/docs/doc.pdf".to_string());
        app.search_results = vec![make_search_result("/a.txt", "h1", "txt")];
        app.total_hits = 1;

        // Should return early without clearing browsed_file
        app.select_result(5);
        assert!(
            app.browsed_file.is_some(),
            "out-of-bounds select_result must not touch browsed_file"
        );
    }

    #[test]
    fn browse_file_sets_browsed_and_clears_selected_result() {
        let mut app = make_transition_test_app();
        // Simulate: user had a search result selected
        app.selected_result = Some(0);
        app.browsed_file = None;

        // browse_file with a non-existent path — the function will error on
        // file read but the state transitions happen before that.
        app.browse_file("/nonexistent/doc.txt");

        assert_eq!(
            app.browsed_file.as_deref(),
            Some("/nonexistent/doc.txt"),
            "browse_file must set browsed_file"
        );
        assert!(
            app.selected_result.is_none(),
            "browse_file must clear selected_result"
        );
    }

    #[test]
    fn transition_from_search_to_browse() {
        let mut app = make_transition_test_app();
        // Start: user clicks a search result
        app.search_results = vec![make_search_result("/docs/notes.txt", "hash1", "txt")];
        app.total_hits = 1;
        app.select_result(0);
        assert!(app.browsed_file.is_none());
        assert_eq!(app.selected_result, Some(0));

        // Then: user clicks a file in the file browser
        app.browse_file("/docs/report.pdf");

        assert_eq!(
            app.browsed_file.as_deref(),
            Some("/docs/report.pdf"),
            "After browse, browsed_file must be set"
        );
        assert!(
            app.selected_result.is_none(),
            "After browse, selected_result must be cleared"
        );
    }

    #[test]
    fn transition_from_browse_to_search() {
        let mut app = make_transition_test_app();
        // Start: user browsed a PDF in the file browser
        app.browsed_file = Some("/docs/report.pdf".to_string());
        // If that PDF was actually rendered, selected_hash would also be set

        // Then: user clicks a search result
        app.search_results = vec![make_search_result("/docs/notes.txt", "hash1", "txt")];
        app.total_hits = 1;
        app.select_result(0);

        assert!(
            app.browsed_file.is_none(),
            "After select_result, stale browsed_file must be cleared"
        );
        assert_eq!(
            app.selected_result,
            Some(0),
            "After select_result, selected_result must be set"
        );
    }

    #[test]
    fn load_text_preview_non_utf8_shows_content_not_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("latin1.txt");
        // Latin-1 bytes that are NOT valid UTF-8 (0xE9 = é in Latin-1)
        let bytes: Vec<u8> = vec![
            0x48, 0x65, 0x6C, 0x6C, 0x6F, 0x20, // "Hello "
            0xE9, // lone byte, invalid UTF-8
            0x20, 0x77, 0x6F, 0x72, 0x6C, 0x64, // " world"
        ];
        std::fs::write(&path, &bytes).unwrap();

        let mut app = make_transition_test_app();
        app.browse_file(path.to_str().unwrap());

        let preview = app.preview_text.as_deref().unwrap_or_default();
        assert!(
            !preview.contains("Error reading file"),
            "non-UTF-8 file should not produce an error: {preview}"
        );
        assert!(
            preview.contains("Hello"),
            "non-UTF-8 file should contain readable content"
        );
    }

    #[test]
    fn try_queue_auto_tag_respects_bounded_channel() {
        use crate::app::AutoTagRequest;
        let (tx, rx) = crossbeam::channel::bounded::<AutoTagRequest>(1);
        let req = |h: &str| AutoTagRequest::TagDocument {
            content_hash: h.to_string(),
            filename: "a.pdf".to_string(),
            text: "text".to_string(),
            content_hash_before_tag: "x".to_string(),
        };

        assert!(PapervaultApp::try_queue_auto_tag(&tx, req("h1")));
        assert!(
            !PapervaultApp::try_queue_auto_tag(&tx, req("h2")),
            "full bounded queue must report backpressure, not block"
        );
        drop(rx);
        assert!(
            !PapervaultApp::try_queue_auto_tag(&tx, req("h3")),
            "disconnected queue must report failure"
        );
    }

    // ── format_file_size tests ──

    /// Create an app backed by a temp-file TagStore with the full schema.
    fn make_store_app() -> (PapervaultApp, tempfile::TempDir) {
        use rusqlite::Connection;
        let dir = tempfile::TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (
                content_hash TEXT PRIMARY KEY,
                file_path   TEXT NOT NULL,
                file_type   TEXT NOT NULL,
                file_size   INTEGER NOT NULL DEFAULT 0,
                modified_ts INTEGER NOT NULL DEFAULT 0,
                indexed_at  TEXT NOT NULL DEFAULT '',
                last_error  TEXT
            );
            CREATE TABLE tags (
                id   INTEGER PRIMARY KEY,
                name TEXT UNIQUE NOT NULL
            );
            CREATE TABLE document_tags (
                content_hash TEXT NOT NULL REFERENCES documents(content_hash) ON DELETE CASCADE,
                tag_id      INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (content_hash, tag_id)
            );
            CREATE TABLE auto_tag_status (
                content_hash TEXT PRIMARY KEY REFERENCES documents(content_hash) ON DELETE CASCADE,
                filename     TEXT NOT NULL,
                content_hash_before_tag TEXT NOT NULL,
                status       TEXT NOT NULL DEFAULT 'pending',
                tags_json    TEXT,
                attempts     INTEGER NOT NULL DEFAULT 0,
                last_error   TEXT,
                created_at   TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE auto_tag_cache (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                filename_tokens TEXT NOT NULL,
                tags_json       TEXT NOT NULL,
                source_hash     TEXT NOT NULL,
                hit_count       INTEGER NOT NULL DEFAULT 1,
                created_at      TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .unwrap();
        let store = TagStore::new_for_test(conn);
        let (_progress_tx, progress_rx) = crossbeam::channel::unbounded();
        let app = PapervaultApp::new(
            Config::default(),
            None,
            None,
            None,
            progress_rx,
            None,
            None,
            None,
            Some(store),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        (app, dir)
    }

    // ── auto-tag cache tests ──

    #[test]
    fn cached_auto_tags_batch_fill_returns_parsed_tags() {
        let (mut app, _dir) = make_store_app();
        let store = app.tag_store.clone().unwrap();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store.upsert_document("h2", "/b.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status(
                "h1",
                "a.pdf",
                "x",
                "tagged",
                Some(r#"{"tags":["tax","irs"]}"#),
                None,
            )
            .unwrap();
        store
            .upsert_auto_tag_status("h2", "b.pdf", "y", "tagged", None, None)
            .unwrap();

        // Batch fill: 3 hashes in one query (h1 tagged, h2 no json, missing absent)
        app.ensure_auto_tag_cache(&["h1", "h2", "missing"]);
        assert_eq!(app.cached_auto_tags("h1"), vec!["tax", "irs"]);
        assert!(
            app.cached_auto_tags("h2").is_empty(),
            "fetched-but-empty must not re-query"
        );
        assert!(
            app.cached_auto_tags("missing").is_empty(),
            "absent row must be marked fetched, not re-queried"
        );

        // The tag panel path: full display data (filename + parsed JSON)
        let status = app.cached_auto_tag("h1").expect("h1 cached");
        assert_eq!(status.filename, "a.pdf");
        assert_eq!(status.value["tags"][0], "tax");
    }

    #[test]
    fn invalidate_and_clear_auto_tag_cache_methods() {
        let (mut app, _dir) = make_store_app();
        let store = app.tag_store.clone().unwrap();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store.upsert_document("h2", "/b.pdf", "pdf", 0, 0).unwrap();
        for h in ["h1", "h2"] {
            store
                .upsert_auto_tag_status(
                    h,
                    "a.pdf",
                    "x",
                    "tagged",
                    Some(r#"{"tags":["tax"]}"#),
                    None,
                )
                .unwrap();
        }
        app.ensure_auto_tag_cache(&["h1", "h2"]);
        assert_eq!(app.auto_tag_cache.len(), 2);

        // Single-hash invalidation (what dismiss/retag/progress do).
        app.invalidate_auto_tag("h1");
        assert!(!app.auto_tag_cache.contains_key("h1"));
        assert!(app.auto_tag_cache.contains_key("h2"));

        // Bulk invalidation (what reindex does).
        app.clear_auto_tag_cache();
        assert!(app.auto_tag_cache.is_empty());
        // A read after invalidation re-fetches (miss path) instead of
        // serving the stale snapshot.
        assert_eq!(app.cached_auto_tags("h1"), vec!["tax"]);
    }

    #[test]
    fn cached_auto_tags_serves_snapshot_until_invalidated() {
        let (mut app, _dir) = make_store_app();
        let store = app.tag_store.clone().unwrap();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status(
                "h1",
                "a.pdf",
                "x",
                "tagged",
                Some(r#"{"tags":["tax"]}"#),
                None,
            )
            .unwrap();

        app.ensure_auto_tag_cache(&["h1"]);
        assert_eq!(app.cached_auto_tags("h1"), vec!["tax"]);

        // Change the DB behind the cache — a filled entry must NOT re-query.
        store
            .upsert_auto_tag_status(
                "h1",
                "a.pdf",
                "x",
                "tagged",
                Some(r#"{"tags":["changed"]}"#),
                None,
            )
            .unwrap();
        assert_eq!(
            app.cached_auto_tags("h1"),
            vec!["tax"],
            "cache must serve the snapshot until invalidated"
        );

        // Invalidation (what dismiss/retag/progress do) forces a re-fetch.
        app.auto_tag_cache.remove("h1");
        assert_eq!(app.cached_auto_tags("h1"), vec!["changed"]);
    }

    #[test]
    fn cached_auto_tags_miss_queries_once_and_fills() {
        let (mut app, _dir) = make_store_app();
        let store = app.tag_store.clone().unwrap();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status(
                "h1",
                "a.pdf",
                "x",
                "tagged",
                Some(r#"{"tags":["tax"]}"#),
                None,
            )
            .unwrap();

        // Direct read without prefetch — the miss path queries once and fills.
        assert_eq!(app.cached_auto_tags("h1"), vec!["tax"]);
        assert!(app.auto_tag_cache.contains_key("h1"));
    }

    #[test]
    fn cached_auto_tags_without_store_returns_empty_without_retry() {
        let mut app = make_transition_test_app(); // no tag store
        app.ensure_auto_tag_cache(&["h1"]);
        assert!(app.cached_auto_tags("h1").is_empty());
        // A second read must not attempt a query (would be per-frame churn).
        assert!(app.cached_auto_tags("h1").is_empty());
    }

    #[test]
    fn format_file_size_zero() {
        assert_eq!(format_file_size(0), "0 B");
    }

    #[test]
    fn format_file_size_bytes() {
        assert_eq!(format_file_size(1), "1 B");
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(1023), "1023 B");
    }

    #[test]
    fn format_file_size_kb_boundary() {
        // Exactly 1 KB
        assert_eq!(format_file_size(1024), "1.0 KB");
        // Just over 1 KB
        assert_eq!(format_file_size(1025), "1.0 KB");
        // Typical KB value
        assert_eq!(format_file_size(23_040), "22.5 KB"); // 22.5 KB
    }

    #[test]
    fn format_file_size_mb() {
        assert_eq!(format_file_size(1_048_576), "1.0 MB"); // 1 MB
        assert_eq!(format_file_size(1_572_864), "1.5 MB");
    }

    #[test]
    fn format_file_size_gb() {
        assert_eq!(format_file_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_file_size(1_610_612_736), "1.5 GB");
    }

    #[test]
    fn format_file_size_large() {
        // 1 TB in bytes — should show GB at max unit
        assert_eq!(format_file_size(1_099_511_627_776), "1024.0 GB");
    }

    #[test]
    fn date_format_epoch_zero() {
        let dt = DateTime::from_timestamp(0, 0).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "1970-01-01 00:00");
    }

    #[test]
    fn date_format_known_timestamp() {
        // 2023-11-15T10:30:00Z
        let dt = DateTime::from_timestamp(1700044200, 0).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2023-11-15 10:30");
    }

    #[test]
    fn date_format_far_future() {
        // 2099-12-31T23:59:59Z
        let dt = DateTime::from_timestamp(4102444799, 0).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2099-12-31 23:59");
    }
}

/// Extract the Win32 HWND from an eframe Frame.
/// Returns None on non-Windows or if the handle is unavailable.
fn extract_hwnd(frame: &eframe::Frame) -> Option<isize> {
    #[cfg(windows)]
    {
        use raw_window_handle::{RawWindowHandle, Win32WindowHandle};
        if let Ok(handle) = frame.window_handle() {
            if let RawWindowHandle::Win32(Win32WindowHandle { hwnd, .. }) = handle.as_raw() {
                return Some(hwnd.get() as isize);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = frame;
    None
}
