use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam::channel::Receiver;
use tracing::{debug, error, info, warn};

use super::provider::TagProvider;
use crate::tags::store::TagStore;

/// Shared API circuit breaker for the auto-tagger workers.
///
/// When the provider is down (consecutive `Unavailable` errors), the breaker
/// opens and every worker fails fast WITHOUT calling the API or sleeping —
/// otherwise 3 workers x pending docs x 3 retries x 30s timeouts churn for
/// hours against a dead endpoint. After the cooldown, one probe call is
/// allowed (half-open); success closes the breaker, failure re-trips it.
///
/// Atomics only — safe to share across the worker threads.
pub struct ApiCircuitBreaker {
    consecutive_failures: AtomicUsize,
    /// Millis since UNIX epoch when the breaker tripped (0 = closed).
    tripped_at_ms: AtomicUsize,
    trip_threshold: usize,
    cooldown_ms: u64,
}

impl ApiCircuitBreaker {
    pub fn new(trip_threshold: usize, cooldown_ms: u64) -> Self {
        Self {
            consecutive_failures: AtomicUsize::new(0),
            tripped_at_ms: AtomicUsize::new(0),
            trip_threshold: trip_threshold.max(1),
            cooldown_ms,
        }
    }

    fn now_ms() -> usize {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as usize)
            .unwrap_or(0)
    }

    /// Whether an API call is allowed right now (closed, or half-open probe).
    pub fn allow_request(&self) -> bool {
        let tripped = self.tripped_at_ms.load(Ordering::Relaxed);
        if tripped == 0 {
            return true;
        }
        if Self::now_ms().saturating_sub(tripped) >= self.cooldown_ms as usize {
            // Half-open: allow ONE probe — the CAS winner only, so concurrent
            // workers cannot all hit the API at once. A failed probe re-trips
            // via record_failure (the counter is still >= threshold).
            if self
                .tripped_at_ms
                .compare_exchange(tripped, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                info!("Circuit breaker closed — probe call allowed after cooldown");
                return true;
            }
            return false;
        }
        false
    }

    /// Record a transient provider failure (5xx/timeout/network).
    pub fn record_failure(&self) {
        let n = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= self.trip_threshold {
            let was_open = self.tripped_at_ms.swap(Self::now_ms(), Ordering::Relaxed) != 0;
            if !was_open {
                warn!("Circuit breaker OPEN after {} consecutive API failures", n);
            }
        }
    }

    /// Record a successful provider call — closes the breaker.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let was_open = self.tripped_at_ms.swap(0, Ordering::Relaxed) != 0;
        if was_open {
            info!("Circuit breaker closed after a successful probe");
        }
    }
}

fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}' | '\u{0483}'..='\u{0489}' | '\u{0591}'..='\u{05C7}'
        | '\u{0610}'..='\u{065F}' | '\u{0670}'..='\u{0670}' | '\u{06D6}'..='\u{06ED}'
        | '\u{0711}'..='\u{0711}' | '\u{0730}'..='\u{074A}' | '\u{07A6}'..='\u{07B0}'
        | '\u{0900}'..='\u{0902}' | '\u{093A}'..='\u{094D}' | '\u{0951}'..='\u{0957}'
        | '\u{0962}'..='\u{0963}' | '\u{0981}'..='\u{09CD}' | '\u{09E2}'..='\u{09E3}'
        | '\u{0A01}'..='\u{0A4D}' | '\u{0A70}'..='\u{0A71}' | '\u{0A81}'..='\u{0ACD}'
        | '\u{0B3E}'..='\u{0B3F}' | '\u{0E31}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}'
        | '\u{0EB1}'..='\u{0EBC}' | '\u{0EC8}'..='\u{0ECD}' | '\u{0F18}'..='\u{0FBC}'
        | '\u{0FC6}'..='\u{0FC6}' | '\u{1DC0}'..='\u{1DFF}' | '\u{20D0}'..='\u{20FF}'
        | '\u{FE00}'..='\u{FE0F}' | '\u{FE20}'..='\u{FE2F}'
    )
}

pub fn extract_filename_tokens(filename: &str) -> Vec<String> {
    let stopwords: &[&str] = &[
        "copy", "final", "v1", "v2", "v3", "draft", "scan", "scanned", "ocr", "new", "old",
        "revised",
    ];
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    stem.split(|c: char| c == '-' || c == '_' || c == '.' || c.is_whitespace())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty())
        .filter(|s| !stopwords.contains(&s.as_str()))
        .filter(|s| s.parse::<u64>().is_err())
        .collect()
}

/// Split CamelCase or PascalCase into words, treating spaces as delimiters.
fn split_camel_case(s: &str) -> Option<Vec<String>> {
    if !s.chars().any(|c| c.is_uppercase()) {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        if c.is_whitespace() {
            if !current.is_empty() {
                parts.push(current.to_lowercase());
                current = String::new();
            }
            continue;
        }
        if c.is_uppercase() && !current.is_empty() {
            parts.push(current.to_lowercase());
            current = String::new();
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    if parts.len() >= 2 {
        Some(parts)
    } else {
        None
    }
}

/// Normalize a person name for searchability, covering all edge cases.
///
/// Handles:
/// - No spaces: "YangGuoRui" → ["yangguorui", "yang guo rui"]
/// - Partial spaces: "YangGuo Rui" → ["yang guo rui", "yangguorui"]
/// - Full spaces: "Yang Guo Rui" → ["yang guo rui", "guorui yang", "yangguorui"]
/// - Single word: "Mia" → ["mia"]
/// - Mixed case: "YANG GUORUI" → ["yang guorui", "guorui yang", "yangguorui"]
/// - Diacritics: "José" → ["jose"]
pub fn normalize_person_name(name: &str) -> Vec<String> {
    let mut variants: std::collections::HashSet<String> = std::collections::HashSet::new();

    let lower = name.to_lowercase();
    let ascii: String = lower.chars().filter(|c| !is_combining_mark(*c)).collect();
    let ascii = ascii.trim();

    if ascii.is_empty() {
        return vec![];
    }

    // Always include the basic lowercase form
    variants.insert(ascii.to_string());

    // Split by whitespace
    let parts: Vec<&str> = ascii.split_whitespace().collect();

    if parts.len() >= 2 {
        let reversed = parts.iter().rev().copied().collect::<Vec<_>>().join(" ");
        variants.insert(reversed);
        let concatenated: String = parts.join("");
        variants.insert(concatenated);
    }

    // Try CamelCase split on the ORIGINAL name (before lowercasing)
    let original = name.trim();
    if let Some(camel_parts) = split_camel_case(original) {
        let spaced = camel_parts.join(" ");
        variants.insert(spaced);
        let reversed = camel_parts
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        variants.insert(reversed);
        let concat = camel_parts.join("");
        variants.insert(concat);
    }

    // Also try CamelCase on individual space-separated parts (handles "YangGuo Rui")
    for part in original.split_whitespace() {
        if let Some(_sub_parts) = split_camel_case(part) {
            let mut all_parts: Vec<String> = Vec::new();
            for p in original.split_whitespace() {
                if let Some(cp) = split_camel_case(p) {
                    all_parts.extend(cp);
                } else {
                    all_parts.push(p.to_lowercase());
                }
            }
            if all_parts.len() > 1 {
                let spaced = all_parts.join(" ");
                variants.insert(spaced);
                let reversed = all_parts
                    .iter()
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ");
                variants.insert(reversed);
                let concat = all_parts.join("");
                variants.insert(concat);
            }
            break; // Only need to expand once
        }
    }

    variants.into_iter().collect()
}

#[allow(clippy::too_many_arguments)]
/// Re-extract the document text for a DB-claimed row so the AI tags the
/// real content instead of only the filename. Falls back to empty text
/// when the file is missing or extraction fails (previous behavior).
fn claimed_row_text(
    tag_store: &TagStore,
    content_hash: &str,
    stages: &[Box<dyn crate::indexer::extractors::Extractor>],
) -> String {
    let path = match tag_store.file_path_for_content_hash(content_hash) {
        Ok(Some(p)) => p,
        _ => return String::new(),
    };
    let path = std::path::PathBuf::from(path);
    if !path.exists() {
        return String::new();
    }
    match crate::indexer::stages::run_chain(&path, stages) {
        Ok(Some(content)) => content.text,
        _ => String::new(),
    }
}

pub fn run_auto_tagger(
    rx: Receiver<crate::app::AutoTagRequest>,
    tag_store: TagStore,
    provider: Box<dyn TagProvider>,
    auto_tag_config: crate::auto_tagger::config::AutoTagConfig,
    shutdown_flag: Arc<AtomicBool>,
    progress: Option<Arc<AtomicUsize>>,
    breaker: Arc<ApiCircuitBreaker>,
    completed: Option<Arc<std::sync::Mutex<Option<String>>>>,
    queue_snapshot: Arc<std::sync::Mutex<Option<Vec<crate::tags::model::AutoTagQueueItem>>>>,
) {
    info!("AutoTagger thread started");
    // Watchdog: periodically sweep rows stranded in 'processing' (a worker
    // died or a call stalled). Age-gated by the claim's updated_at refresh
    // so in-flight calls are never double-claimed.
    let mut last_sweep = std::time::Instant::now();
    const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
    const STALE_PROCESSING_AGE_MINUTES: i64 = 10;
    // Process any pending documents from DB (recovery after crash or channel
    // drops). claim_pending_auto_tags is atomic — concurrent workers each get
    // a disjoint batch, so no document is ever sent to the API twice.
    if let Ok(pending) = tag_store.claim_pending_auto_tags(100) {
        // Publish immediately after the claim so the UI shows these rows
        // as in-flight while the batch runs.
        refresh_queue_snapshot(&tag_store, &queue_snapshot);
        let stages = crate::indexer::stages::create_extractor_chain();
        for p in pending {
            if shutdown_flag.load(Ordering::Acquire) {
                break;
            }
            let text = claimed_row_text(&tag_store, &p.content_hash, &stages);
            tag_document(
                &p.content_hash,
                &p.filename,
                &text,
                &p.content_hash_before_tag,
                &tag_store,
                provider.as_ref(),
                &auto_tag_config,
                progress.as_deref(),
                &breaker,
                completed.as_deref(),
            );
        }
        // Batch finished — publish the queue state (rows that remain).
        refresh_queue_snapshot(&tag_store, &queue_snapshot);
    }
    // Cadence gate for idle refreshes — the snapshot must not cost a DB
    // query per loop tick.
    let mut last_queue_refresh = std::time::Instant::now();
    while !shutdown_flag.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                if shutdown_flag.load(Ordering::Acquire) {
                    break;
                }
                let is_shutdown = process_request(
                    request,
                    &tag_store,
                    provider.as_ref(),
                    &auto_tag_config,
                    progress.as_deref(),
                    &breaker,
                    completed.as_deref(),
                );
                // A manual re-tag click should be visible immediately.
                refresh_queue_snapshot(&tag_store, &queue_snapshot);
                if is_shutdown {
                    break;
                }
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                // Watchdog sweep (idle time only — cheap, age-gated).
                if last_sweep.elapsed() >= SWEEP_INTERVAL {
                    match tag_store.reset_stale_processing_older_than(STALE_PROCESSING_AGE_MINUTES) {
                        Ok(n) if n > 0 => {
                            info!("Watchdog: reset {} stale 'processing' rows", n);
                        }
                        _ => {}
                    }
                    last_sweep = std::time::Instant::now();
                }
                // Channel idle — drain pending rows from the DB (the durable
                // queue). This also recovers requests dropped by a full
                // bounded channel instead of waiting for the next startup.
                // Publish the queue state after the claim (rows show as
                // in-flight while the batch runs) and at a 2s cadence when
                // idle.
                if last_queue_refresh.elapsed() >= Duration::from_secs(2) {
                    refresh_queue_snapshot(&tag_store, &queue_snapshot);
                    last_queue_refresh = std::time::Instant::now();
                }
                if let Ok(pending) = tag_store.claim_pending_auto_tags(8) {
                    refresh_queue_snapshot(&tag_store, &queue_snapshot);
                    let stages = crate::indexer::stages::create_extractor_chain();
                    for p in pending {
                        if shutdown_flag.load(Ordering::Acquire) {
                            break;
                        }
                        let text = claimed_row_text(&tag_store, &p.content_hash, &stages);
                        tag_document(
                            &p.content_hash,
                            &p.filename,
                            &text,
                            &p.content_hash_before_tag,
                            &tag_store,
                            provider.as_ref(),
                            &auto_tag_config,
                            progress.as_deref(),
                            &breaker,
                            completed.as_deref(),
                        );
                    }
                    refresh_queue_snapshot(&tag_store, &queue_snapshot);
                }
            }
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                info!("AutoTagger channel disconnected, shutting down");
                break;
            }
        }
    }
    info!("AutoTagger thread stopped");
}

/// Publish the live auto-tag queue (waiting + in-flight rows) to the
/// shared snapshot the UI reads. Runs off the UI thread; the table is
/// small so a per-file or gated refresh stays cheap.
fn refresh_queue_snapshot(
    tag_store: &TagStore,
    snapshot: &Arc<std::sync::Mutex<Option<Vec<crate::tags::model::AutoTagQueueItem>>>>,
) {
    let items = match tag_store.list_auto_tag_queue() {
        Ok(items) => items,
        Err(e) => {
            warn!("Failed to refresh auto-tag queue snapshot: {}", e);
            return;
        }
    };
    let waiting = items.iter().filter(|i| i.status == "pending").count();
    let in_flight = items.iter().filter(|i| i.status == "processing").count();
    debug!("Auto-tag queue: {} waiting, {} in flight", waiting, in_flight);
    let mut g = snapshot.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some(items);
}

fn process_request(
    request: crate::app::AutoTagRequest,
    tag_store: &TagStore,
    provider: &dyn TagProvider,
    config: &crate::auto_tagger::config::AutoTagConfig,
    progress: Option<&std::sync::atomic::AtomicUsize>,
    breaker: &ApiCircuitBreaker,
    completed: Option<&std::sync::Mutex<Option<String>>>,
) -> bool {
    match request {
        crate::app::AutoTagRequest::TagDocument {
            content_hash,
            filename,
            text,
            content_hash_before_tag,
        } => {
            // Another worker may have claimed this row (startup recovery or
            // idle re-claim) — tier 1 only short-circuits 'tagged', so skip
            // rows owned elsewhere to avoid a duplicate AI call.
            if let Ok(Some(status)) = tag_store.auto_tag_status(&content_hash) {
                if status.status == "processing" {
                    debug!("skipping {filename}: row claimed by another worker");
                    return false;
                }
            }
            tag_document(
                &content_hash,
                &filename,
                &text,
                &content_hash_before_tag,
                tag_store,
                provider,
                config,
                progress,
                breaker,
                completed,
            );
            false
        }
        crate::app::AutoTagRequest::Shutdown => {
            info!("AutoTagger received shutdown, draining queue");
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn tag_document(
    content_hash: &str,
    filename: &str,
    text: &str,
    content_hash_before_tag: &str,
    tag_store: &TagStore,
    provider: &dyn TagProvider,
    config: &crate::auto_tagger::config::AutoTagConfig,
    progress: Option<&std::sync::atomic::AtomicUsize>,
    breaker: &ApiCircuitBreaker,
    completed: Option<&std::sync::Mutex<Option<String>>>,
) {
    if !config.enabled {
        debug!("auto-tagger disabled, skipping {filename}");
        // Claimed rows must not stay 'processing' — restore 'pending' so a
        // later session (or a reindex) can claim them again.
        let _ = tag_store.upsert_auto_tag_status(
            content_hash,
            filename,
            content_hash_before_tag,
            "pending",
            None,
            None,
        );
        return;
    }

    let text_preview = if text.chars().count() > 120 {
        // char-safe truncation — byte slicing panics inside CJK characters
        let truncated: String = text.chars().take(120).collect();
        format!("{}...", truncated)
    } else {
        text.to_string()
    };
    let short_hash = &content_hash[..content_hash.len().min(12)];
    info!(
        "Auto-tagging: {filename} [{short_hash}] (text={} chars, preview='{text_preview}')",
        text.len()
    );

    // Tier 1: exact BLAKE3 hash match (content hasn't changed)
    if let Ok(Some(status)) = tag_store.auto_tag_status(content_hash) {
        if status.content_hash_before_tag == content_hash_before_tag && status.status == "tagged" {
            info!(
                "  → tier 1 (exact hash): {filename} — reusing {} byte tags_json",
                status.tags_json.as_ref().map(|j| j.len()).unwrap_or(0)
            );
            if let Some(p) = progress {
                p.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
    }

    // Tier 2: filename-token lookup — only when >=5 tokens AND >=80% overlap
    let tokens = extract_filename_tokens(filename);
    info!("  tokens({}): {:?}", tokens.len(), tokens);
    if tokens.len() >= 5 {
        if let Ok(Some(cached_json)) = tag_store.lookup_cache_by_tokens(&tokens, 0.8) {
            info!(
                "  → tier 2 (token overlap): {filename} — reusing cached tags ({} bytes)",
                cached_json.len()
            );
            let _ = tag_store
                .upsert_auto_tag_status(
                    content_hash,
                    filename,
                    content_hash_before_tag,
                    "tagged",
                    Some(&cached_json),
                    None,
                )
                .map_err(|e| warn!("failed to write cache result for {content_hash}: {e}"));
            if let Some(p) = progress {
                p.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(m) = completed {
                if let Ok(mut g) = m.lock() {
                    *g = Some(content_hash.to_string());
                }
            }
            return;
        }
    } else if !tokens.is_empty() {
        debug!("  skipping tier 2: only {} tokens (need >=5)", tokens.len());
    }

    // Tier 3: AI fallback
    // Circuit breaker: while the provider is down, fail fast — no API call,
    // no retry sleeps. The docs stay marked 'failed' and can be re-queued
    // after the outage (manual retag or re-index).
    if !breaker.allow_request() {
        info!("  → circuit breaker open — skipping AI call for {filename}");
        let _ = tag_store
            .upsert_auto_tag_status(
                content_hash,
                filename,
                content_hash_before_tag,
                "failed",
                None,
                Some("Skipped: API circuit breaker open (provider unavailable)"),
            )
            .map_err(|e| warn!("failed to write breaker skip for {content_hash}: {e}"));
        if let Some(p) = progress {
            p.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(m) = completed {
            if let Ok(mut g) = m.lock() {
                *g = Some(content_hash.to_string());
            }
        }
        return;
    }
    info!("  → tier 3 (AI fallback): {filename} — calling DeepSeek");
    let existing_tags: Vec<String> = tag_store
        .list_tags()
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.name)
        .collect();

    let mut last_error = String::new();
    for attempt in 0..config.max_retries {
        match provider.generate_tags(filename, text, &existing_tags) {
            Ok(response) => {
                breaker.record_success();
                let mut entities = response.entities;
                // Merge person names into tags — only one canonical form each
                let mut all_tags = response.tags.clone();
                for person in &entities.persons {
                    let variants = normalize_person_name(person);
                    if let Some(canonical) = variants.first() {
                        if !all_tags.contains(canonical) {
                            all_tags.push(canonical.clone());
                        }
                    }
                }
                all_tags.truncate(config.max_tags_per_doc);
                entities.persons = entities
                    .persons
                    .iter()
                    .flat_map(|n| normalize_person_name(n))
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();

                let tags_json =
                    serde_json::json!({"tags": all_tags, "entities": entities}).to_string();

                if let Err(e) = tag_store.upsert_auto_tag_status(
                    content_hash,
                    filename,
                    content_hash_before_tag,
                    "tagged",
                    Some(&tags_json),
                    None,
                ) {
                    warn!("failed to write auto-tag result for {content_hash}: {e}");
                } else {
                    info!(
                        "💾 Wrote to DB: {filename} → status=tagged, {} tags",
                        all_tags.len()
                    );
                }
                if !tokens.is_empty() {
                    let _ =
                        tag_store.upsert_cache_entry(&tokens.join(" "), &tags_json, content_hash);
                }
                info!(
                    "  ✓ AI tagged {filename}: tags={:?}, persons={:?}, orgs={:?}",
                    all_tags, entities.persons, entities.organizations
                );
                if let Some(p) = progress {
                    p.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(m) = completed {
                    if let Ok(mut g) = m.lock() {
                        *g = Some(content_hash.to_string());
                    }
                }
                return;
            }
            Err(e) => {
                let is_retryable = matches!(&e, super::provider::TagError::Unavailable(_));
                if is_retryable {
                    breaker.record_failure();
                }
                warn!(
                    "  ✗ AI attempt {}/{} for {filename} [{short_hash}]: {e}",
                    attempt + 1,
                    config.max_retries
                );
                if !is_retryable || attempt + 1 >= config.max_retries {
                    last_error = e.to_string();
                    break;
                }
                debug!(
                    "retry {}/{} for {filename}: {e}",
                    attempt + 1,
                    config.max_retries
                );
                std::thread::sleep(Duration::from_secs(2u64.saturating_pow(attempt)));
            }
        }
    }

    warn!("auto-tag failed for {content_hash}: {last_error}");
    if let Err(e) = tag_store.upsert_auto_tag_status(
        content_hash,
        filename,
        content_hash_before_tag,
        "failed",
        None,
        Some(&last_error),
    ) {
        error!("failed to write auto-tag failure status for {content_hash}: {e}");
    }
    if let Some(p) = progress {
        p.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(m) = completed {
        if let Ok(mut g) = m.lock() {
            *g = Some(content_hash.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_person_name_lowercase() {
        let v = normalize_person_name("Yang Guorui");
        assert!(v.contains(&"yang guorui".to_string()));
    }
    #[test]
    fn normalize_person_name_order_variants() {
        let v = normalize_person_name("Yang Guorui");
        assert!(v.contains(&"yang guorui".to_string()));
        assert!(v.contains(&"guorui yang".to_string()));
    }
    #[test]
    fn normalize_person_name_cjk_space_strip() {
        let v = normalize_person_name("Yang Guo Rui");
        assert!(v.contains(&"yangguorui".to_string()));
    }
    #[test]
    fn normalize_person_name_single_word() {
        assert_eq!(normalize_person_name("Yang"), vec!["yang"]);
    }
    #[test]
    fn normalize_person_name_empty() {
        assert!(normalize_person_name("").is_empty());
    }
    #[test]
    fn normalize_no_spaces_camelcase() {
        let v = normalize_person_name("YangGuoRui");
        assert!(
            v.contains(&"yang guo rui".to_string()),
            "CamelCase should be split: {:?}",
            v
        );
        assert!(v.contains(&"yangguorui".to_string()));
    }
    #[test]
    fn normalize_partial_spaces() {
        let v = normalize_person_name("YangGuo Rui");
        assert!(
            v.contains(&"yang guo rui".to_string()),
            "partial spaces: {:?}",
            v
        );
        assert!(v.contains(&"yangguorui".to_string()));
    }
    #[test]
    fn normalize_single_name_mia() {
        let v = normalize_person_name("Mia");
        assert_eq!(v, vec!["mia"]);
    }
    #[test]
    fn normalize_mixed_case() {
        let v = normalize_person_name("YANG GUORUI");
        assert!(v.contains(&"yang guorui".to_string()));
        assert!(v.contains(&"guorui yang".to_string()));
    }
    #[test]
    fn normalize_chen_yang() {
        let v = normalize_person_name("Chen Yang");
        assert!(v.contains(&"chen yang".to_string()));
        assert!(v.contains(&"yang chen".to_string()));
        assert!(v.contains(&"chenyang".to_string()));
    }

    #[test]
    fn split_camel_case_handles_embedded_spaces() {
        let parts = split_camel_case("YangGuo Rui").unwrap();
        assert_eq!(parts, vec!["yang", "guo", "rui"]);
    }

    #[test]
    fn split_camel_case_treats_spaces_as_delimiters() {
        let parts = split_camel_case("Hello World").unwrap();
        assert_eq!(parts, vec!["hello", "world"]);
    }
    #[test]
    fn extract_filename_tokens_strips_extension_and_splits() {
        let t = extract_filename_tokens("2023-tax-return-yang-guorui.pdf");
        assert!(t.contains(&"tax".to_string()));
        assert!(t.contains(&"return".to_string()));
    }
    #[test]
    fn extract_filename_tokens_filters_stopwords() {
        let t = extract_filename_tokens("final-draft-scan-tax-return.pdf");
        assert!(!t.contains(&"final".to_string()));
        assert!(t.contains(&"tax".to_string()));
    }
    #[test]
    fn circuit_breaker_trips_after_threshold_and_resets_on_success() {
        let breaker = ApiCircuitBreaker::new(3, 60_000);
        assert!(breaker.allow_request(), "closed breaker allows calls");
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.allow_request(), "below threshold still allows");
        breaker.record_failure();
        assert!(
            !breaker.allow_request(),
            "threshold reached — circuit must be open"
        );
        breaker.record_success();
        assert!(breaker.allow_request(), "success closes the circuit");
    }

    #[test]
    fn circuit_breaker_reopens_after_cooldown() {
        // 1ms cooldown: the half-open probe is allowed almost immediately.
        let breaker = ApiCircuitBreaker::new(1, 1);
        breaker.record_failure();
        assert!(!breaker.allow_request());
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            breaker.allow_request(),
            "after cooldown a probe call must be allowed"
        );
        // The probe re-trips on failure (threshold 1).
        breaker.record_failure();
        assert!(!breaker.allow_request());
    }

    #[test]
    fn tag_document_skips_ai_call_when_breaker_open() {
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{Entities, TagError, TagProvider, TagResponse};
        use rusqlite::Connection;

        struct CountingProvider(std::sync::atomic::AtomicUsize);
        impl TagProvider for CountingProvider {
            fn generate_tags(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<TagResponse, TagError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(TagResponse {
                    tags: vec![],
                    entities: Entities::default(),
                })
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (content_hash TEXT PRIMARY KEY, file_path TEXT NOT NULL, file_type TEXT NOT NULL, file_size INTEGER DEFAULT 0, modified_ts INTEGER DEFAULT 0, indexed_at TEXT DEFAULT '', last_error TEXT); CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL); CREATE TABLE document_tags (content_hash TEXT REFERENCES documents ON DELETE CASCADE, tag_id INTEGER REFERENCES tags ON DELETE CASCADE, PRIMARY KEY(content_hash, tag_id)); CREATE TABLE auto_tag_status (content_hash TEXT PRIMARY KEY REFERENCES documents ON DELETE CASCADE, filename TEXT NOT NULL, content_hash_before_tag TEXT NOT NULL, status TEXT DEFAULT 'pending', tags_json TEXT, attempts INTEGER DEFAULT 0, last_error TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now'))); CREATE TABLE auto_tag_cache (id INTEGER PRIMARY KEY AUTOINCREMENT, filename_tokens TEXT NOT NULL, tags_json TEXT NOT NULL, source_hash TEXT NOT NULL, hit_count INTEGER DEFAULT 1, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));",
        )
        .unwrap();
        let store = TagStore::new_for_test(conn);
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();

        let provider = CountingProvider(std::sync::atomic::AtomicUsize::new(0));
        let config = AutoTagConfig::default(); // enabled = false
        let breaker = ApiCircuitBreaker::new(1, 60_000);
        breaker.record_failure(); // trip the breaker

        let progress = Arc::new(AtomicUsize::new(0));
        tag_document(
            "h1",
            "a.pdf",
            "some text",
            "hash-before",
            &store,
            &provider,
            &config,
            Some(&progress),
            &breaker,
            None,
        );

        // Disabled config returns before the breaker — force enabled to reach
        // the tier-3 skip path.
        let mut config = config;
        config.enabled = true;
        let progress = Arc::new(AtomicUsize::new(0));
        tag_document(
            "h1",
            "a.pdf",
            "some text",
            "hash-before",
            &store,
            &provider,
            &config,
            Some(&progress),
            &breaker,
            None,
        );

        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            0,
            "no API call must be made while the breaker is open"
        );
        assert_eq!(progress.load(Ordering::Relaxed), 1);
        let status = store.auto_tag_status("h1").unwrap().unwrap();
        assert_eq!(status.status, "failed");
        assert!(
            status
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("circuit breaker"),
            "status must record the breaker skip: {:?}",
            status.last_error
        );
    }

    /// Full-schema test store (matches production DDL).
    fn auto_tagger_test_store() -> (TagStore, tempfile::TempDir) {
        use rusqlite::Connection;
        let dir = tempfile::TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (content_hash TEXT PRIMARY KEY, file_path TEXT NOT NULL, file_type TEXT NOT NULL, file_size INTEGER DEFAULT 0, modified_ts INTEGER DEFAULT 0, indexed_at TEXT DEFAULT '', last_error TEXT); CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL); CREATE TABLE document_tags (content_hash TEXT REFERENCES documents ON DELETE CASCADE, tag_id INTEGER REFERENCES tags ON DELETE CASCADE, PRIMARY KEY(content_hash, tag_id)); CREATE TABLE auto_tag_status (content_hash TEXT PRIMARY KEY REFERENCES documents ON DELETE CASCADE, filename TEXT NOT NULL, content_hash_before_tag TEXT NOT NULL, status TEXT DEFAULT 'pending', tags_json TEXT, attempts INTEGER DEFAULT 0, last_error TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now'))); CREATE TABLE auto_tag_cache (id INTEGER PRIMARY KEY AUTOINCREMENT, filename_tokens TEXT NOT NULL, tags_json TEXT NOT NULL, source_hash TEXT NOT NULL, hit_count INTEGER DEFAULT 1, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));",
        )
        .unwrap();
        (TagStore::new_for_test(conn), dir)
    }

    struct DummyProvider;
    impl TagProvider for DummyProvider {
        fn generate_tags(
            &self,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<super::super::provider::TagResponse, super::super::provider::TagError> {
            Ok(super::super::provider::TagResponse {
                tags: vec![],
                entities: super::super::provider::Entities::default(),
            })
        }
    }

    #[test]
    fn process_request_skips_rows_claimed_by_another_worker() {
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{Entities, TagError, TagProvider, TagResponse};

        struct CountingProvider(std::sync::atomic::AtomicUsize);
        impl TagProvider for CountingProvider {
            fn generate_tags(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<TagResponse, TagError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(TagResponse {
                    tags: vec![],
                    entities: Entities::default(),
                })
            }
        }

        let (store, _dir) = auto_tagger_test_store();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        // Row claimed by another worker (startup recovery in flight).
        store
            .upsert_auto_tag_status("h1", "a.pdf", "x", "pending", None, None)
            .unwrap();
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE auto_tag_status SET status = 'processing' WHERE content_hash = 'h1'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let provider = CountingProvider(std::sync::atomic::AtomicUsize::new(0));
        let config = AutoTagConfig {
            enabled: true,
            ..AutoTagConfig::default()
        };
        let breaker = ApiCircuitBreaker::new(6, 60_000);
        let completed = std::sync::Mutex::new(None::<String>);
        let result = process_request(
            crate::app::AutoTagRequest::TagDocument {
                content_hash: "h1".into(),
                filename: "a.pdf".into(),
                text: "text".into(),
                content_hash_before_tag: "x".into(),
            },
            &store,
            &provider,
            &config,
            None,
            &breaker,
            Some(&completed),
        );
        assert!(!result);
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            0,
            "a row claimed by another worker must not trigger a duplicate AI call"
        );
    }

    #[test]
    fn tag_document_disabled_restores_pending() {
        use crate::auto_tagger::config::AutoTagConfig;
        let (store, _dir) = auto_tagger_test_store();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status("h1", "a.pdf", "x", "pending", None, None)
            .unwrap();
        store
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE auto_tag_status SET status = 'processing' WHERE content_hash = 'h1'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let config = AutoTagConfig::default(); // enabled = false
        let breaker = ApiCircuitBreaker::new(6, 60_000);
        tag_document(
            "h1",
            "a.pdf",
            "",
            "x",
            &store,
            &DummyProvider,
            &config,
            None,
            &breaker,
            None,
        );

        let status = store.auto_tag_status("h1").unwrap().unwrap();
        assert_eq!(
            status.status, "pending",
            "a claimed row must not stay 'processing' when tagging is disabled"
        );
    }

    #[test]
    fn tag_document_signals_completed_hash() {
        use crate::auto_tagger::config::AutoTagConfig;
        let (store, _dir) = auto_tagger_test_store();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status("h1", "a.pdf", "x", "pending", None, None)
            .unwrap();

        let config = AutoTagConfig {
            enabled: true,
            ..AutoTagConfig::default()
        };
        let breaker = ApiCircuitBreaker::new(6, 60_000);
        let completed = std::sync::Mutex::new(None::<String>);
        tag_document(
            "h1",
            "a.pdf",
            "some text",
            "x",
            &store,
            &DummyProvider,
            &config,
            None,
            &breaker,
            Some(&completed),
        );

        assert_eq!(
            *completed.lock().unwrap(),
            Some("h1".to_string()),
            "completion signal must carry the hash so the UI can drop its stale entry"
        );
        assert_eq!(
            store.auto_tag_status("h1").unwrap().unwrap().status,
            "tagged"
        );
    }

    #[test]
    fn tag_document_breaker_wiring_unavailable_trips_non_transient_does_not() {
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{TagError, TagProvider, TagResponse};

        struct FailProvider(TagError);
        impl TagProvider for FailProvider {
            fn generate_tags(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<TagResponse, TagError> {
                Err(self.0.clone())
            }
        }

        // Transient (Unavailable) failures must trip the breaker.
        let (store, _dir) = auto_tagger_test_store();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status("h1", "a.pdf", "x", "pending", None, None)
            .unwrap();
        let config = AutoTagConfig {
            enabled: true,
            max_retries: 1, // no backoff sleeps
            ..AutoTagConfig::default()
        };
        let breaker = ApiCircuitBreaker::new(1, 60_000);
        let provider = FailProvider(TagError::Unavailable("down".into()));
        tag_document(
            "h1", "a.pdf", "text", "x", &store, &provider, &config, None, &breaker, None,
        );
        assert!(
            !breaker.allow_request(),
            "Unavailable failures must trip the circuit breaker"
        );

        // Non-transient (auth) failures must NOT trip it.
        let (store, _dir) = auto_tagger_test_store();
        store.upsert_document("h2", "/b.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status("h2", "b.pdf", "x", "pending", None, None)
            .unwrap();
        let breaker = ApiCircuitBreaker::new(1, 60_000);
        let provider = FailProvider(TagError::Auth("bad key".into()));
        tag_document(
            "h2", "b.pdf", "text", "x", &store, &provider, &config, None, &breaker, None,
        );
        assert!(
            breaker.allow_request(),
            "permanent errors (auth) must not trip the breaker — retries are pointless anyway"
        );
    }

    #[test]
    fn tag_document_cjk_text_preview_does_not_panic() {
        use crate::auto_tagger::config::AutoTagConfig;
        let (store, _dir) = auto_tagger_test_store();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();

        // 121 CJK chars (363 bytes) — the old byte-slice `&text[..120]`
        // panicked inside a multibyte character.
        let cjk_text = "中文内容".repeat(30) + "结尾";
        assert!(cjk_text.len() > 120);

        let config = AutoTagConfig::default(); // disabled — returns after preview
        let breaker = ApiCircuitBreaker::new(6, 60_000);
        tag_document(
            "h1",
            "a.pdf",
            &cjk_text,
            "x",
            &store,
            &DummyProvider,
            &config,
            None,
            &breaker,
            None,
        );
        // Reaching this line means the preview truncation did not panic.
    }

    #[test]
    fn tag_document_retags_content_when_hash_before_differs() {
        // "Re-index for tags" semantics: the worker must STILL call the API
        // for an already-tagged row when the hash-before differs (the reindex
        // flow sends a synthetic hash so tier-1 cannot short-circuit).
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{Entities, TagError, TagProvider, TagResponse};

        struct CountingProvider(std::sync::atomic::AtomicUsize);
        impl TagProvider for CountingProvider {
            fn generate_tags(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<TagResponse, TagError> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(TagResponse {
                    tags: vec!["fresh".into()],
                    entities: Entities::default(),
                })
            }
        }

        let (store, _dir) = auto_tagger_test_store();
        store.upsert_document("h1", "/a.pdf", "pdf", 0, 0).unwrap();
        store
            .upsert_auto_tag_status(
                "h1",
                "a.pdf",
                "stale-hash-before",
                "tagged",
                Some(r#"{"tags":["old"]}"#),
                None,
            )
            .unwrap();

        let provider = CountingProvider(std::sync::atomic::AtomicUsize::new(0));
        let config = AutoTagConfig {
            enabled: true,
            ..AutoTagConfig::default()
        };
        let breaker = ApiCircuitBreaker::new(6, 60_000);
        tag_document(
            "h1",
            "a.pdf",
            "text",
            "reindex-hash", // differs from the stored hash-before
            &store,
            &provider,
            &config,
            None,
            &breaker,
            None,
        );

        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            1,
            "reindex must still reach the API for changed content"
        );
        let status = store.auto_tag_status("h1").unwrap().unwrap();
        assert_eq!(status.status, "tagged");
        assert!(
            status.tags_json.as_deref().unwrap_or("").contains("fresh"),
            "the fresh re-tag result must be stored"
        );
    }

    #[test]
    fn shutdown_request_returns_true() {
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{Entities, TagError, TagProvider, TagResponse};
        use crate::tags::store::TagStore;
        use rusqlite::Connection;

        struct DummyProvider;
        impl TagProvider for DummyProvider {
            fn generate_tags(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<TagResponse, TagError> {
                Ok(TagResponse {
                    tags: vec![],
                    entities: Entities::default(),
                })
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (content_hash TEXT PRIMARY KEY, file_path TEXT NOT NULL, file_type TEXT NOT NULL, file_size INTEGER DEFAULT 0, modified_ts INTEGER DEFAULT 0, indexed_at TEXT DEFAULT '', last_error TEXT); CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL); CREATE TABLE document_tags (content_hash TEXT REFERENCES documents ON DELETE CASCADE, tag_id INTEGER REFERENCES tags ON DELETE CASCADE, PRIMARY KEY(content_hash, tag_id)); CREATE TABLE auto_tag_status (content_hash TEXT PRIMARY KEY REFERENCES documents ON DELETE CASCADE, filename TEXT NOT NULL, content_hash_before_tag TEXT NOT NULL, status TEXT DEFAULT 'pending', tags_json TEXT, attempts INTEGER DEFAULT 0, last_error TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now'))); CREATE TABLE auto_tag_cache (id INTEGER PRIMARY KEY AUTOINCREMENT, filename_tokens TEXT NOT NULL, tags_json TEXT NOT NULL, source_hash TEXT NOT NULL, hit_count INTEGER DEFAULT 1, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));",
        ).unwrap();
        let store = TagStore::new_for_test(conn);
        let provider = DummyProvider;
        let config = AutoTagConfig::default();
        let breaker = ApiCircuitBreaker::new(1, 60_000);
        let result = process_request(
            crate::app::AutoTagRequest::Shutdown,
            &store,
            &provider,
            &config,
            None,
            &breaker,
            None,
        );
        assert!(result, "Shutdown request should return true");
    }

    #[test]
    fn db_claimed_rows_pass_extracted_text_to_provider() {
        // A row recovered from the DB queue must be tagged from the
        // document's extracted text, not an empty string — the extracted
        // text is the entire point of the AI tagging call.
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{Entities, TagError, TagProvider, TagResponse};
        use crate::tags::store::TagStore;
        use rusqlite::Connection;

        struct CapturingProvider(std::sync::Arc<std::sync::Mutex<Option<String>>>);
        impl TagProvider for CapturingProvider {
            fn generate_tags(
                &self,
                _filename: &str,
                text: &str,
                _tokens: &[String],
            ) -> Result<TagResponse, TagError> {
                *self.0.lock().unwrap() = Some(text.to_string());
                Ok(TagResponse {
                    tags: vec![],
                    entities: Entities::default(),
                })
            }
        }

        // Real file on disk with extractable text.
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("notice.txt");
        std::fs::write(
            &file,
            "Canada Revenue Agency tax assessment for Guorui Yang",
        )
        .unwrap();

        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (content_hash TEXT PRIMARY KEY, file_path TEXT NOT NULL, file_type TEXT NOT NULL, file_size INTEGER DEFAULT 0, modified_ts INTEGER DEFAULT 0, indexed_at TEXT DEFAULT '', last_error TEXT); CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL); CREATE TABLE document_tags (content_hash TEXT REFERENCES documents ON DELETE CASCADE, tag_id INTEGER REFERENCES tags ON DELETE CASCADE, PRIMARY KEY(content_hash, tag_id)); CREATE TABLE auto_tag_status (content_hash TEXT PRIMARY KEY REFERENCES documents ON DELETE CASCADE, filename TEXT NOT NULL, content_hash_before_tag TEXT NOT NULL, status TEXT DEFAULT 'pending', tags_json TEXT, attempts INTEGER DEFAULT 0, last_error TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now'))); CREATE TABLE auto_tag_cache (id INTEGER PRIMARY KEY AUTOINCREMENT, filename_tokens TEXT NOT NULL, tags_json TEXT NOT NULL, source_hash TEXT NOT NULL, hit_count INTEGER DEFAULT 1, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));",
        )
        .unwrap();
        let store = TagStore::new_for_test(conn);
        let path_str = file.display().to_string();
        store
            .upsert_document("h1", &path_str, "txt", 0, 0)
            .unwrap();
        store
            .upsert_auto_tag_status("h1", "notice.txt", "before", "pending", None, None)
            .unwrap();

        let captured_text = std::sync::Arc::new(std::sync::Mutex::new(None));
        let provider = CapturingProvider(captured_text.clone());
        let config = AutoTagConfig {
            enabled: true,
            ..Default::default()
        };
        let (tx, rx) = crossbeam::channel::bounded::<crate::app::AutoTagRequest>(1);
        drop(tx); // no channel requests — DB claim path only
        let shutdown = Arc::new(AtomicBool::new(false));
        let breaker = ApiCircuitBreaker::new(100, 60_000);
        let queue_snapshot = Arc::new(std::sync::Mutex::new(None));
        run_auto_tagger(
            rx,
            store,
            Box::new(provider),
            config,
            shutdown,
            None,
            Arc::new(breaker),
            None,
            queue_snapshot,
        );

        let captured = captured_text.lock().unwrap().clone();
        assert_eq!(
            captured.as_deref(),
            Some("Canada Revenue Agency tax assessment for Guorui Yang"),
            "DB-claimed rows must send the extracted document text to the API"
        );
    }

    #[test]
    fn db_claimed_row_with_missing_file_falls_back_to_empty_text() {
        // A recovered row whose file was deleted must not crash — empty
        // text keeps the previous filename-only behavior.
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{Entities, TagError, TagProvider, TagResponse};
        use crate::tags::store::TagStore;
        use rusqlite::Connection;

        struct CapturingProvider(std::sync::Arc<std::sync::Mutex<Option<String>>>);
        impl TagProvider for CapturingProvider {
            fn generate_tags(
                &self,
                _filename: &str,
                text: &str,
                _tokens: &[String],
            ) -> Result<TagResponse, TagError> {
                *self.0.lock().unwrap() = Some(text.to_string());
                Ok(TagResponse {
                    tags: vec![],
                    entities: Entities::default(),
                })
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (content_hash TEXT PRIMARY KEY, file_path TEXT NOT NULL, file_type TEXT NOT NULL, file_size INTEGER DEFAULT 0, modified_ts INTEGER DEFAULT 0, indexed_at TEXT DEFAULT '', last_error TEXT); CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL); CREATE TABLE document_tags (content_hash TEXT REFERENCES documents ON DELETE CASCADE, tag_id INTEGER REFERENCES tags ON DELETE CASCADE, PRIMARY KEY(content_hash, tag_id)); CREATE TABLE auto_tag_status (content_hash TEXT PRIMARY KEY REFERENCES documents ON DELETE CASCADE, filename TEXT NOT NULL, content_hash_before_tag TEXT NOT NULL, status TEXT DEFAULT 'pending', tags_json TEXT, attempts INTEGER DEFAULT 0, last_error TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now'))); CREATE TABLE auto_tag_cache (id INTEGER PRIMARY KEY AUTOINCREMENT, filename_tokens TEXT NOT NULL, tags_json TEXT NOT NULL, source_hash TEXT NOT NULL, hit_count INTEGER DEFAULT 1, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));",
        )
        .unwrap();
        let store = TagStore::new_for_test(conn);
        store
            .upsert_document("h1", "/gone/notice.pdf", "pdf", 0, 0)
            .unwrap();
        store
            .upsert_auto_tag_status("h1", "notice.pdf", "before", "pending", None, None)
            .unwrap();

        let captured_text = std::sync::Arc::new(std::sync::Mutex::new(None));
        let provider = CapturingProvider(captured_text.clone());
        let config = AutoTagConfig {
            enabled: true,
            ..Default::default()
        };
        let (tx, rx) = crossbeam::channel::bounded::<crate::app::AutoTagRequest>(1);
        drop(tx);
        let shutdown = Arc::new(AtomicBool::new(false));
        let breaker = ApiCircuitBreaker::new(100, 60_000);
        let queue_snapshot = Arc::new(std::sync::Mutex::new(None));
        run_auto_tagger(
            rx,
            store,
            Box::new(provider),
            config,
            shutdown,
            None,
            Arc::new(breaker),
            None,
            queue_snapshot,
        );

        let captured = captured_text.lock().unwrap().clone();
        assert_eq!(
            captured.as_deref(),
            Some(""),
            "missing file must fall back to empty text"
        );
    }

    #[test]
    fn queue_snapshot_shows_in_flight_row_and_clears_after_drain() {
        // The live queue snapshot must list a claimed row as in-flight
        // while its API call is running, and show an empty queue once
        // the drain finishes.
        use crate::auto_tagger::config::AutoTagConfig;
        use crate::auto_tagger::provider::{Entities, TagError, TagProvider, TagResponse};
        use crate::tags::model::AutoTagQueueItem;
        use crate::tags::store::TagStore;
        use rusqlite::Connection;

        struct SlowProvider(std::sync::Arc<std::sync::Mutex<usize>>);
        impl TagProvider for SlowProvider {
            fn generate_tags(
                &self,
                _: &str,
                _: &str,
                _: &[String],
            ) -> Result<TagResponse, TagError> {
                *self.0.lock().unwrap() += 1;
                std::thread::sleep(Duration::from_millis(400)); // API in flight
                Ok(TagResponse {
                    tags: vec!["tax".to_string()],
                    entities: Entities::default(),
                })
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("notice.txt");
        std::fs::write(&file, "Canada Revenue Agency tax assessment").unwrap();

        let conn = Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (content_hash TEXT PRIMARY KEY, file_path TEXT NOT NULL, file_type TEXT NOT NULL, file_size INTEGER DEFAULT 0, modified_ts INTEGER DEFAULT 0, indexed_at TEXT DEFAULT '', last_error TEXT); CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL); CREATE TABLE document_tags (content_hash TEXT REFERENCES documents ON DELETE CASCADE, tag_id INTEGER REFERENCES tags ON DELETE CASCADE, PRIMARY KEY(content_hash, tag_id)); CREATE TABLE auto_tag_status (content_hash TEXT PRIMARY KEY REFERENCES documents ON DELETE CASCADE, filename TEXT NOT NULL, content_hash_before_tag TEXT NOT NULL, status TEXT DEFAULT 'pending', tags_json TEXT, attempts INTEGER DEFAULT 0, last_error TEXT, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now'))); CREATE TABLE auto_tag_cache (id INTEGER PRIMARY KEY AUTOINCREMENT, filename_tokens TEXT NOT NULL, tags_json TEXT NOT NULL, source_hash TEXT NOT NULL, hit_count INTEGER DEFAULT 1, created_at TEXT DEFAULT (datetime('now')), updated_at TEXT DEFAULT (datetime('now')));",
        )
        .unwrap();
        let store = TagStore::new_for_test(conn);
        let path_str = file.display().to_string();
        store
            .upsert_document("h1", &path_str, "txt", 0, 0)
            .unwrap();
        store
            .upsert_auto_tag_status("h1", "notice.txt", "before", "pending", None, None)
            .unwrap();

        let calls = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let provider = SlowProvider(calls.clone());
        let config = AutoTagConfig {
            enabled: true,
            ..Default::default()
        };
        let (tx, rx) = crossbeam::channel::bounded::<crate::app::AutoTagRequest>(1);
        drop(tx);
        let shutdown = Arc::new(AtomicBool::new(false));
        let breaker = ApiCircuitBreaker::new(100, 60_000);
        let queue_snapshot: Arc<std::sync::Mutex<Option<Vec<AutoTagQueueItem>>>> =
            Arc::new(std::sync::Mutex::new(None));

        let worker = {
            let snapshot = queue_snapshot.clone();
            std::thread::spawn(move || {
                run_auto_tagger(
                    rx,
                    store,
                    Box::new(provider),
                    config,
                    shutdown,
                    None,
                    Arc::new(breaker),
                    None,
                    snapshot,
                );
            })
        };

        // While the provider is blocked in the API call, the row must be
        // visible as in-flight.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut seen_in_flight = false;
        while std::time::Instant::now() < deadline {
            let items = queue_snapshot.lock().unwrap().clone();
            if let Some(items) = items {
                if items.iter().any(|i| i.status == "processing" && i.filename == "notice.txt") {
                    seen_in_flight = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            seen_in_flight,
            "the claimed row must appear in the snapshot as in-flight during the API call"
        );

        worker.join().unwrap();
        let final_items = queue_snapshot.lock().unwrap().clone();
        assert_eq!(
            final_items,
            Some(vec![]),
            "once the drain finishes, the queue snapshot must be empty"
        );
        assert_eq!(*calls.lock().unwrap(), 1, "the API must have been called once");
    }
}
