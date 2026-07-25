use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam::channel::Receiver;
use tracing::{debug, error, info, warn};

use super::provider::TagProvider;
use crate::tags::store::TagStore;

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
        "copy", "final", "v1", "v2", "v3", "draft", "scan", "scanned", "ocr", "new", "old", "revised",
    ];
    let stem = filename.rsplit_once('.').map(|(s, _)| s).unwrap_or(filename);
    stem.split(|c: char| c == '-' || c == '_' || c == '.' || c.is_whitespace())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty())
        .filter(|s| !stopwords.contains(&s.as_str()))
        .filter(|s| s.parse::<u64>().is_err())
        .collect()
}

pub fn normalize_person_name(name: &str) -> Vec<String> {
    let mut variants = Vec::with_capacity(3);
    let lower = name.to_lowercase();
    let ascii: String = lower.chars().filter(|c| !is_combining_mark(*c)).collect();
    let ascii = ascii.trim();
    if ascii.is_empty() { return variants; }
    variants.push(ascii.to_string());
    let parts: Vec<&str> = ascii.split_whitespace().collect();
    if parts.len() >= 2 {
        let reversed = parts.iter().rev().copied().collect::<Vec<_>>().join(" ");
        if reversed != ascii { variants.push(reversed); }
        let concatenated: String = parts.join("");
        if concatenated != ascii { variants.push(concatenated); }
    }
    variants
}

pub fn run_auto_tagger(
    rx: Receiver<crate::app::AutoTagRequest>,
    tag_store: TagStore,
    provider: Box<dyn TagProvider>,
    auto_tag_config: crate::auto_tagger::config::AutoTagConfig,
    shutdown_flag: Arc<AtomicBool>,
) {
    info!("AutoTagger thread started");
    while !shutdown_flag.load(Ordering::Acquire) {
        // Primary: poll pending docs from DB
        let pending = tag_store.pending_auto_tags(10).unwrap_or_default();
        for doc in pending {
            if shutdown_flag.load(Ordering::Acquire) { break; }
            process_pending(doc, &tag_store, provider.as_ref(), &auto_tag_config);
        }
        // Secondary: drain channel for hot-path notifications
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(request) => {
                if shutdown_flag.load(Ordering::Acquire) { break; }
                process_request(request, &tag_store, provider.as_ref(), &auto_tag_config);
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                info!("AutoTagger channel disconnected, shutting down");
                break;
            }
        }
    }
    info!("AutoTagger thread stopped");
}

fn process_request(
    request: crate::app::AutoTagRequest,
    tag_store: &TagStore,
    provider: &dyn TagProvider,
    config: &crate::auto_tagger::config::AutoTagConfig,
) {
    match request {
        crate::app::AutoTagRequest::TagDocument { content_hash, filename, text, content_hash_before_tag } => {
            tag_document(&content_hash, &filename, &text, &content_hash_before_tag, tag_store, provider, config);
        }
        crate::app::AutoTagRequest::Shutdown => {
            info!("AutoTagger received shutdown");
        }
    }
}

fn process_pending(
    doc: crate::tags::model::AutoTagStatus,
    tag_store: &TagStore,
    provider: &dyn TagProvider,
    config: &crate::auto_tagger::config::AutoTagConfig,
) {
    // DB-pending docs without text can't be tagged — skip
    if doc.tags_json.is_none() && doc.status == "pending" { return; }
}

fn tag_document(
    content_hash: &str,
    filename: &str,
    text: &str,
    content_hash_before_tag: &str,
    tag_store: &TagStore,
    provider: &dyn TagProvider,
    config: &crate::auto_tagger::config::AutoTagConfig,
) {
    // Tier 1: exact BLAKE3 hash match
    if let Ok(Some(status)) = tag_store.auto_tag_status(content_hash) {
        if status.content_hash_before_tag == content_hash_before_tag && status.status == "tagged" {
            debug!("cache hit (tier 1) for {filename}: exact hash match");
            return;
        }
    }

    // Tier 2: filename-token lookup
    let tokens = extract_filename_tokens(filename);
    if !tokens.is_empty() {
        if let Ok(Some(cached_json)) = tag_store.lookup_cache_by_tokens(&tokens, 0.5) {
            debug!("cache hit (tier 2) for {filename}: token overlap");
            let _ = tag_store.upsert_auto_tag_status(content_hash, filename, content_hash_before_tag, "tagged", Some(&cached_json), None)
                .map_err(|e| warn!("failed to write cache result for {content_hash}: {e}"));
            return;
        }
    }

    // Tier 3: AI fallback
    let existing_tags: Vec<String> = tag_store.list_tags().unwrap_or_default()
        .into_iter().map(|t| t.name).collect();

    let mut last_error = String::new();
    for attempt in 0..config.max_retries {
        match provider.generate_tags(filename, text, &existing_tags) {
            Ok(response) => {
                let mut entities = response.entities;
                let normalized_persons: Vec<String> = entities.persons.iter()
                    .flat_map(|n| normalize_person_name(n)).collect();
                entities.persons = normalized_persons;

                let tags_json = serde_json::json!({"tags": response.tags, "entities": entities}).to_string();

                if let Err(e) = tag_store.upsert_auto_tag_status(content_hash, filename, content_hash_before_tag, "tagged", Some(&tags_json), None) {
                    warn!("failed to write auto-tag result for {content_hash}: {e}");
                }
                if !tokens.is_empty() {
                    let _ = tag_store.upsert_cache_entry(&tokens.join(" "), &tags_json, content_hash);
                }
                debug!("AI tagged {filename}: {} tags", response.tags.len());
                return;
            }
            Err(e) => {
                let is_retryable = matches!(&e, super::provider::TagError::Unavailable(_));
                if !is_retryable || attempt + 1 >= config.max_retries {
                    last_error = e.to_string();
                    break;
                }
                debug!("retry {}/{} for {filename}: {e}", attempt + 1, config.max_retries);
                std::thread::sleep(Duration::from_secs(2u64.saturating_pow(attempt)));
            }
        }
    }

    warn!("auto-tag failed for {content_hash}: {last_error}");
    if let Err(e) = tag_store.upsert_auto_tag_status(content_hash, filename, content_hash_before_tag, "failed", None, Some(&last_error)) {
        error!("failed to write auto-tag failure status for {content_hash}: {e}");
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
}
