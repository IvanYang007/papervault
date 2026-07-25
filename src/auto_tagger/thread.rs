use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam::channel::Receiver;
use tracing::{debug, info, warn};

use super::provider::TagProvider;
use crate::tags::store::TagStore;

/// Check if a character is a Unicode combining mark (diacritic, tone mark).
fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'   // Combining Diacritical Marks
        | '\u{0483}'..='\u{0489}' // Cyrillic
        | '\u{0591}'..='\u{05C7}' // Hebrew
        | '\u{0610}'..='\u{065F}' // Arabic
        | '\u{0670}'..='\u{0670}'
        | '\u{06D6}'..='\u{06ED}'
        | '\u{0711}'..='\u{0711}'
        | '\u{0730}'..='\u{074A}'
        | '\u{07A6}'..='\u{07B0}'
        | '\u{0900}'..='\u{0902}' // Devanagari
        | '\u{093A}'..='\u{094D}'
        | '\u{0951}'..='\u{0957}'
        | '\u{0962}'..='\u{0963}'
        | '\u{0981}'..='\u{09CD}' // Bengali
        | '\u{09E2}'..='\u{09E3}'
        | '\u{0A01}'..='\u{0A4D}' // Gurmukhi
        | '\u{0A70}'..='\u{0A71}'
        | '\u{0A81}'..='\u{0ACD}' // Gujarati
        | '\u{0B3E}'..='\u{0B3F}' // Tamil/Telugu
        | '\u{0E31}'..='\u{0E3A}' // Thai
        | '\u{0E47}'..='\u{0E4E}'
        | '\u{0EB1}'..='\u{0EBC}' // Lao
        | '\u{0EC8}'..='\u{0ECD}'
        | '\u{0F18}'..='\u{0F19}' // Tibetan
        | '\u{0F35}'..='\u{0FBC}'
        | '\u{0FC6}'..='\u{0FC6}'
        | '\u{1DC0}'..='\u{1DFF}' // Comb. Diacritical Marks Supplement
        | '\u{20D0}'..='\u{20FF}' // Comb. Marks for Symbols
        | '\u{FE00}'..='\u{FE0F}' // Variation Selectors
        | '\u{FE20}'..='\u{FE2F}' // Combining Half Marks
    )
}

/// Extract significant tokens from a filename for cache lookup.
pub fn extract_filename_tokens(filename: &str) -> Vec<String> {
    let stopwords: &[&str] = &[
        "copy", "final", "v1", "v2", "v3", "draft", "scan", "scanned", "ocr", "new", "old",
        "revised",
    ];

    let stem = filename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(filename);

    stem.split(|c: char| c == '-' || c == '_' || c == '.' || c.is_whitespace())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty())
        .filter(|s| !stopwords.contains(&s.as_str()))
        .filter(|s| s.parse::<u64>().is_err())
        .collect()
}

/// Normalize a person name for searchability.
///
/// Pipeline: lowercase → strip combining marks → CJK space strip → order variants.
pub fn normalize_person_name(name: &str) -> Vec<String> {
    let mut variants = Vec::with_capacity(3);

    let lower = name.to_lowercase();
    let ascii: String = lower.chars().filter(|c| !is_combining_mark(*c)).collect();
    let ascii = ascii.trim();

    if ascii.is_empty() {
        return variants;
    }

    variants.push(ascii.to_string());

    let parts: Vec<&str> = ascii.split_whitespace().collect();
    if parts.len() >= 2 {
        let reversed = parts.iter().rev().copied().collect::<Vec<_>>().join(" ");
        if reversed != ascii {
            variants.push(reversed);
        }

        let concatenated: String = parts.join("");
        if concatenated != ascii {
            variants.push(concatenated);
        }
    }

    variants
}

/// Run the auto-tagger thread loop.
pub fn run_auto_tagger(
    rx: Receiver<crate::app::AutoTagRequest>,
    tag_store: TagStore,
    provider: Box<dyn TagProvider>,
    auto_tag_config: crate::auto_tagger::config::AutoTagConfig,
    shutdown_flag: Arc<AtomicBool>,
) {
    info!("AutoTagger thread started");

    while !shutdown_flag.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(request) => {
                if shutdown_flag.load(Ordering::Acquire) {
                    break;
                }
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
        crate::app::AutoTagRequest::TagDocument {
            content_hash,
            filename,
            text,
        } => {
            let content_hash_before_tag = {
                let mut hasher = blake3::Hasher::new();
                hasher.update(filename.as_bytes());
                hasher.update(text.as_bytes());
                hasher.finalize().to_hex().to_string()
            };

            // Tier 1: exact BLAKE3 hash match
            if let Ok(Some(status)) = tag_store.auto_tag_status(&content_hash) {
                if status.content_hash_before_tag == content_hash_before_tag
                    && status.status == "tagged"
                {
                    debug!("cache hit (tier 1) for {filename}: exact hash match");
                    return;
                }
            }

            // Tier 2: filename-token lookup
            let tokens = extract_filename_tokens(&filename);
            if !tokens.is_empty() {
                if let Ok(Some(cached_json)) = tag_store.lookup_cache_by_tokens(&tokens, 0.5) {
                    debug!("cache hit (tier 2) for {filename}: token overlap");
                    let _ = tag_store.upsert_auto_tag_status(
                        &content_hash,
                        &filename,
                        &content_hash_before_tag,
                        "tagged",
                        Some(&cached_json),
                        None,
                    );
                    return;
                }
            }

            // Tier 3: AI fallback
            let existing_tags: Vec<String> = tag_store
                .list_tags()
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.name)
                .collect();

            let mut last_error = String::new();
            for attempt in 0..config.max_retries {
                match provider.generate_tags(&filename, &text, &existing_tags) {
                    Ok(response) => {
                        let mut entities = response.entities;
                        let normalized_persons: Vec<String> = entities
                            .persons
                            .iter()
                            .flat_map(|n| normalize_person_name(n))
                            .collect();
                        entities.persons = normalized_persons;

                        let tags_json = serde_json::json!({
                            "tags": response.tags,
                            "entities": entities,
                        })
                        .to_string();

                        let _ = tag_store.upsert_auto_tag_status(
                            &content_hash,
                            &filename,
                            &content_hash_before_tag,
                            "tagged",
                            Some(&tags_json),
                            None,
                        );

                        if !tokens.is_empty() {
                            let token_str = tokens.join(" ");
                            let _ = tag_store
                                .upsert_cache_entry(&token_str, &tags_json, &content_hash);
                        }

                        debug!("AI tagged {filename}: {} tags", response.tags.len());
                        return;
                    }
                    Err(e) => {
                        let is_retryable =
                            matches!(&e, super::provider::TagError::Unavailable(_));
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
            let _ = tag_store.upsert_auto_tag_status(
                &content_hash,
                &filename,
                &content_hash_before_tag,
                "failed",
                None,
                Some(&last_error),
            );
        }
        crate::app::AutoTagRequest::Shutdown => {
            info!("AutoTagger received shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_person_name_lowercase() {
        let variants = normalize_person_name("Yang Guorui");
        assert!(variants.contains(&"yang guorui".to_string()));
    }

    #[test]
    fn normalize_person_name_order_variants() {
        let variants = normalize_person_name("Yang Guorui");
        assert!(variants.contains(&"yang guorui".to_string()));
        assert!(variants.contains(&"guorui yang".to_string()));
    }

    #[test]
    fn normalize_person_name_cjk_space_strip() {
        let variants = normalize_person_name("Yang Guo Rui");
        assert!(variants.contains(&"yangguorui".to_string()));
    }

    #[test]
    fn normalize_person_name_single_word() {
        let variants = normalize_person_name("Yang");
        assert_eq!(variants, vec!["yang"]);
    }

    #[test]
    fn normalize_person_name_empty() {
        let variants = normalize_person_name("");
        assert!(variants.is_empty());
    }

    #[test]
    fn extract_filename_tokens_strips_extension_and_splits() {
        let tokens = extract_filename_tokens("2023-tax-return-yang-guorui.pdf");
        assert!(tokens.contains(&"tax".to_string()));
        assert!(tokens.contains(&"return".to_string()));
        assert!(tokens.contains(&"yang".to_string()));
        assert!(tokens.contains(&"guorui".to_string()));
    }

    #[test]
    fn extract_filename_tokens_filters_stopwords() {
        let tokens = extract_filename_tokens("final-draft-scan-tax-return.pdf");
        assert!(!tokens.contains(&"final".to_string()));
        assert!(!tokens.contains(&"draft".to_string()));
        assert!(!tokens.contains(&"scan".to_string()));
        assert!(tokens.contains(&"tax".to_string()));
        assert!(tokens.contains(&"return".to_string()));
    }
}
