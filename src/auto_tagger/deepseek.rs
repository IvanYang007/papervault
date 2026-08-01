use std::time::Duration;

use serde_json::Value;
use tracing::{info, warn};

use super::provider::{TagError, TagProvider, TagResponse};

/// Prompt template for the DeepSeek API — kept minimal to force JSON compliance.
/// Prompt template — minimal to force JSON compliance with deepseek-v4-flash.
const PROMPT_TEMPLATE: &str = r#"Output ONLY this JSON. No other text.
If text is empty or unextractable, generate tags from the filename alone.
IMPORTANT for person names: extract ALL name variants (full, partial, initials). Names may appear with/without spaces, reversed order, or CamelCase.
{"tags":["keyword1","keyword2"],"entities":{"persons":["Full Name"],"organizations":["Org Name"],"years":["2024"],"doc_id":["form-number"],"amounts":["$1,000"]}}

Filename: {{FILENAME}}
Text (first 2 pages): {{TEXT}}
Existing tags: {{EXISTING_TAGS}}
JSON:"#;

/// Provider that calls the DeepSeek API for tag generation.
pub struct DeepSeekProvider {
    agent: ureq::Agent,
    endpoint: String,
    model: String,
    api_key_env: String,
    timeout: Duration,
    /// Maximum words of text to send (soft cap — truncates at word boundary).
    max_text_words: usize,
}

impl DeepSeekProvider {
    /// Create a new DeepSeek provider.
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key_env: impl Into<String>,
        timeout_secs: u64,
        max_text_words: usize,
    ) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(timeout_secs))
                .build(),
            endpoint: endpoint.into(),
            model: model.into(),
            api_key_env: api_key_env.into(),
            timeout: Duration::from_secs(timeout_secs),
            max_text_words,
        }
    }

    /// Read the API key from the environment variable at request time.
    fn api_key(&self) -> Result<String, TagError> {
        let key = std::env::var(&self.api_key_env).map_err(|_| {
            TagError::Auth(format!(
                "environment variable '{}' is not set. Set it to your DeepSeek API key.",
                self.api_key_env
            ))
        })?;
        if key.trim().is_empty() {
            return Err(TagError::Auth(format!(
                "environment variable '{}' is set but empty",
                self.api_key_env
            )));
        }
        Ok(key)
    }

    /// Truncate text to at most `max_text_words` words at a word boundary.
    fn truncate_text<'a>(&self, text: &'a str) -> &'a str {
        let mut word_end = text.len();
        for (word_count, word) in text.split_whitespace().enumerate() {
            if word_count >= self.max_text_words {
                word_end = word.as_ptr() as usize - text.as_ptr() as usize;
                break;
            }
        }
        let trimmed = text[..word_end].trim_end();
        let trim_len = trimmed.as_ptr() as usize - text.as_ptr() as usize + trimmed.len();
        &text[..trim_len]
    }

    /// Build the prompt with filename, text, and existing tags substituted.
    fn build_prompt(&self, filename: &str, text: &str, existing_tags: &[String]) -> String {
        let truncated = self.truncate_text(text);
        let existing = if existing_tags.is_empty() {
            "[]".to_string()
        } else {
            serde_json::to_string(existing_tags).unwrap_or_else(|_| "[]".to_string())
        };
        // Single-pass replacement to avoid multiple intermediate allocations
        PROMPT_TEMPLATE
            .replace("{{FILENAME}}", filename)
            .replace("{{TEXT}}", truncated)
            .replace("{{EXISTING_TAGS}}", &existing)
    }

    /// Parse the DeepSeek API response JSON into a TagResponse.
    /// The model's `content` is extracted robustly: LLMs frequently wrap
    /// the JSON in markdown fences or prefix/suffix it with prose despite
    /// the "output ONLY this JSON" instruction.
    fn parse_response(body: &str) -> Result<TagResponse, TagError> {
        let root: Value = serde_json::from_str(body)
            .map_err(|e| TagError::Parse(format!("invalid JSON from API: {}", e)))?;

        let content = root["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice["message"]["content"].as_str())
            .ok_or_else(|| {
                TagError::Parse("API response missing 'choices[0].message.content'".into())
            })?;

        // Empty completions are transient model behavior — retryable so
        // the worker's retry loop gives the model another chance.
        if content.trim().is_empty() {
            return Err(TagError::Unavailable(
                "model returned empty content — retrying".into(),
            ));
        }

        let tag_json = extract_json_object(content).ok_or_else(|| {
            let preview: String = content.chars().take(200).collect();
            TagError::Unavailable(format!(
                "no JSON object in model output — retrying. Raw content: {}",
                preview
            ))
        })?;

        let tag_response: TagResponse = serde_json::from_str(tag_json).map_err(|e| {
            // char-safe preview — byte slicing panics inside CJK characters
            let preview: String = content.chars().take(200).collect();
            TagError::Parse(format!(
                "failed to parse tag JSON from model output: {}. Raw content: {}",
                e, preview
            ))
        })?;

        Ok(tag_response)
    }
}

/// Extract the first top-level JSON object from a model reply.
/// Strips markdown fences and leading/trailing prose, then finds the
/// brace-balanced object. Brace counting is string-aware (braces inside
/// quoted strings or escaped quotes do not affect nesting).
fn extract_json_object(s: &str) -> Option<&str> {
    let s = s.trim();
    // Strip ```json / ``` fences if present.
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let s = s.strip_suffix("```").unwrap_or(s);

    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None // truncated or unparseable — no balanced object
}

impl TagProvider for DeepSeekProvider {
    fn generate_tags(
        &self,
        filename: &str,
        text: &str,
        existing_tags: &[String],
    ) -> Result<TagResponse, TagError> {
        let api_key = self.api_key()?;
        // Truncate ONCE here: the prompt uses the truncated text and the
        // log line reports the actual size sent (not the raw input size).
        let truncated = self.truncate_text(text);
        let prompt = self.build_prompt(filename, truncated, existing_tags);

        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "temperature": 0.1,
            "max_tokens": 4000,
            "stream": false
        });

        let body_str = serde_json::to_string(&body).map_err(|e| {
            TagError::BadRequest(format!("failed to serialize request body: {}", e))
        })?;

        info!(
            "🤖 DeepSeek API call: {filename} ({} chars of text)",
            truncated.len()
        );
        let started = std::time::Instant::now();
        let response = self
            .agent
            .post(&self.endpoint)
            .set("Authorization", &format!("Bearer {}", api_key))
            .set("Content-Type", "application/json")
            .timeout(self.timeout)
            .send_string(&body_str)
            .map_err(|e| match &e {
                ureq::Error::Status(401 | 403, _) => {
                    TagError::Auth(format!("DeepSeek API auth failed: {}", e))
                }
                ureq::Error::Status(429, _) => {
                    TagError::Unavailable(format!("DeepSeek API rate limited: {}", e))
                }
                ureq::Error::Status(code, _) if *code >= 400 && *code < 500 => {
                    TagError::BadRequest(format!("DeepSeek API bad request ({}): {}", code, e))
                }
                ureq::Error::Status(_, _) => {
                    TagError::Unavailable(format!("DeepSeek API server error: {}", e))
                }
                ureq::Error::Transport(_) => {
                    TagError::Unavailable(format!("DeepSeek API unreachable: {}", e))
                }
            })?;

        let body_str = response.into_string().map_err(|e| {
            TagError::Unavailable(format!("failed to read API response body: {}", e))
        })?;

        let result = Self::parse_response(&body_str);
        let elapsed_ms = started.elapsed().as_millis();
        match &result {
            Ok(r) => info!(
                "📥 DeepSeek response for {filename}: {} tags, {} persons ({} ms)",
                r.tags.len(),
                r.entities.persons.len(),
                elapsed_ms
            ),
            Err(e) => warn!(
                "📥 DeepSeek parse error for {filename} ({} ms): {e}",
                elapsed_ms
            ),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider() -> DeepSeekProvider {
        DeepSeekProvider::new(
            "https://api.deepseek.com/v1/chat/completions",
            "deepseek-chat",
            "DEEPSEEK_API_KEY",
            30,
            500,
        )
    }

    #[test]
    fn truncate_text_under_limit_returns_full_text() {
        let provider = make_provider();
        let text = "hello world";
        assert_eq!(provider.truncate_text(text), "hello world");
    }

    #[test]
    fn truncate_text_at_word_boundary() {
        let provider = make_provider();
        let words: Vec<String> = (0..505).map(|i| format!("word{}", i)).collect();
        let text = words.join(" ");
        let truncated = provider.truncate_text(&text);

        let word_count = truncated.split_whitespace().count();
        assert_eq!(word_count, 500, "provider must truncate to its max_text_words");
        assert!(!truncated.ends_with(' '));
    }

    #[test]
    fn truncate_text_empty_string() {
        let provider = make_provider();
        assert_eq!(provider.truncate_text(""), "");
    }

    #[test]
    fn build_prompt_includes_filename_and_truncated_text() {
        let provider = make_provider();
        let prompt = provider.build_prompt(
            "2023-tax-return.pdf",
            "Form 1040 tax document content here",
            &["tax".into(), "irs".into()],
        );

        assert!(prompt.contains("2023-tax-return.pdf"));
        assert!(prompt.contains("Form 1040 tax document content here"));
        assert!(prompt.contains(r#""tax""#));
        assert!(prompt.contains(r#""irs""#));
        // Verify no unreplaced placeholders remain
        assert!(
            !prompt.contains("{existing_tags"),
            "placeholder not replaced"
        );
        assert!(!prompt.contains("{{FILENAME}}"), "FILENAME not replaced");
    }

    #[test]
    fn truncate_text_handles_consecutive_whitespace() {
        let provider = make_provider();
        let text = "a   b   c   d   e";
        assert_eq!(provider.truncate_text(text), "a   b   c   d   e");
    }

    #[test]
    fn build_prompt_empty_existing_tags_uses_empty_array() {
        let provider = make_provider();
        let prompt = provider.build_prompt("test.pdf", "content", &[]);
        assert!(prompt.contains("[]"), "empty tags should produce '[]'");
    }

    #[test]
    fn parse_response_extracts_tags_and_entities() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "{\"tags\": [\"tax-return\", \"tax\"], \"entities\": {\"persons\": [\"Yang Guorui\"], \"years\": [\"2023\"], \"organizations\": [], \"doc_id\": [], \"amounts\": []}}"
                }
            }]
        }"#;

        let result = DeepSeekProvider::parse_response(body).unwrap();

        assert_eq!(result.tags, vec!["tax-return", "tax"]);
        assert_eq!(result.entities.persons, vec!["Yang Guorui"]);
        assert_eq!(result.entities.years, vec!["2023"]);
    }

    #[test]
    fn parse_response_handles_empty_entities() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "{\"tags\": [\"test\"], \"entities\": {}}"
                }
            }]
        }"#;

        let result = DeepSeekProvider::parse_response(body).unwrap();

        assert_eq!(result.tags, vec!["test"]);
        assert!(result.entities.persons.is_empty());
    }

    #[test]
    fn parse_response_rejects_missing_choices() {
        let body = r#"{"error": "not found"}"#;
        let result = DeepSeekProvider::parse_response(body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_response_rejects_invalid_json_in_content() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "not valid json {{{"
                }
            }]
        }"#;
        let result = DeepSeekProvider::parse_response(body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_response_accepts_markdown_fenced_json() {
        // LLMs often wrap JSON in ```json fences despite the prompt.
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "Here are the tags:\n```json\n{\"tags\": [\"tax\", \"CRA\"], \"entities\": {\"persons\": [], \"years\": [], \"organizations\": [], \"doc_id\": [], \"amounts\": []}}\n```"
                }
            }]
        }"#;
        let result = DeepSeekProvider::parse_response(body).unwrap();
        assert_eq!(result.tags, vec!["tax", "CRA"]);
    }

    #[test]
    fn parse_response_accepts_prose_around_json() {
        // Prose before/after the object must not break parsing.
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "Sure! Based on the document:\n{\"tags\": [\"notice\"], \"entities\": {\"persons\": [], \"years\": [], \"organizations\": [], \"doc_id\": [], \"amounts\": []}}\nHope that helps."
                }
            }]
        }"#;
        let result = DeepSeekProvider::parse_response(body).unwrap();
        assert_eq!(result.tags, vec!["notice"]);
    }

    #[test]
    fn parse_response_rejects_truncated_json() {
        // Connection cut mid-output: no balanced object → retryable error.
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "{\"tags\": [\"tax\"]"
                }
            }]
        }"#;
        let result = DeepSeekProvider::parse_response(body);
        assert!(
            matches!(result, Err(TagError::Unavailable(_))),
            "truncated JSON must be a retryable Unavailable error"
        );
    }

    #[test]
    fn parse_response_retries_empty_model_content() {
        // Empty completions happen under load — must be retryable, not
        // a permanent failure.
        let body = r#"{
            "choices": [{
                "message": {
                    "content": ""
                }
            }]
        }"#;
        let result = DeepSeekProvider::parse_response(body);
        assert!(
            matches!(result, Err(TagError::Unavailable(_))),
            "empty content must be retryable"
        );
    }

    #[test]
    fn extract_json_object_ignores_braces_inside_strings() {
        // A tag containing braces must not confuse the extractor.
        let s = r#"pre {"tags": ["a{b}"], "note": "}"} post"#;
        let extracted = extract_json_object(s).unwrap();
        let v: serde_json::Value = serde_json::from_str(extracted).unwrap();
        assert_eq!(v["tags"][0], "a{b}");
    }
}
