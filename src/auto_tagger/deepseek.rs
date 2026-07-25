use std::time::Duration;

use serde_json::Value;

use super::provider::{TagError, TagProvider, TagResponse};

/// Prompt template for the DeepSeek API.
const PROMPT_TEMPLATE: &str = r#"You are a document entity extractor and classifier. Given a filename and extracted text, perform TWO tasks:

TASK 1 — Classify the document. Return 3-5 purpose/type tags (lowercase, 1-3 words, hyphen-separated, no punctuation). At least one must describe the document type (e.g., "research-paper", "tax-return", "legal-contract", "invoice", "lecture-slides", "form").

TASK 2 — Extract structured entities from the text where clearly present:
- persons: Full names of people mentioned. Use the most complete/proper form found (e.g., "Yang Guorui" not "yang").
- organizations: Company names, government agencies, institutions.
- years: 4-digit years referenced as dates or tax years (not page numbers or arbitrary numbers).
- doc_id: Document/form identifier if present (e.g., "1040", "W-2", case number, invoice number).
- amounts: Monetary amounts with currency if detectable (e.g., "$45,000", "EUR 1200").

Rules:
- The FILENAME is the strongest signal for classification — use it first.
- The three anchor tags derived from the filename (person name, document type, year) are the MOST IMPORTANT and MUST be included.
- After including anchor tags, add any additional keywords you find important from the document content.
- For entities, ONLY extract what is clearly present — do not hallucinate.
- If OCR garbling makes a name unreadable, OMIT it rather than guessing.
- If an entity appears in multiple forms, use the most complete/proper form.
- Prefer existing tags from this vocabulary for classification when they fit: {existing_tags}
- For each person entity, also generate common name variations as additional person entries (e.g., for "Yang Guorui" also output "guorui yang").
- Return ONLY valid JSON in this exact structure, nothing else:

{
  "tags": ["tax-return", "tax", "irs"],
  "entities": {
    "persons": ["Yang Guorui", "guorui yang"],
    "organizations": ["Internal Revenue Service"],
    "years": ["2023"],
    "doc_id": ["1040"],
    "amounts": ["$12,450"]
  }
}

Example:
Filename: "2023-tax-return-yang-guorui.pdf"
Text: "Form 1040. Yang Guorui. Tax year 2023. Adjusted gross income $45,230..."
Output: {"tags": ["tax-return", "tax", "irs", "form-1040"], "entities": {"persons": ["Yang Guorui", "guorui yang"], "organizations": ["IRS"], "years": ["2023"], "doc_id": ["1040"], "amounts": ["$45,230"]}}

Filename: {{FILENAME}}
Text: {{TEXT}}
Existing tags: {{EXISTING_TAGS}}
Output:"#;

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
            max_text_words: 2000,
        }
    }

    /// Read the API key from the environment variable at request time.
    fn api_key(&self) -> Result<String, TagError> {
        std::env::var(&self.api_key_env).map_err(|_| {
            TagError::Auth(format!(
                "environment variable '{}' is not set. Set it to your DeepSeek API key.",
                self.api_key_env
            ))
        })
    }

    /// Truncate text to at most `max_text_words` words at a word boundary.
    fn truncate_text<'a>(&self, text: &'a str) -> &'a str {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.len() <= self.max_text_words {
            return text;
        }
        let mut cutoff = words[self.max_text_words..]
            .first()
            .map(|w| w.as_ptr() as usize - text.as_ptr() as usize)
            .unwrap_or(text.len());
        // Trim trailing whitespace
        while cutoff > 0 && text.as_bytes().get(cutoff - 1) == Some(&b' ') {
            cutoff -= 1;
        }
        &text[..cutoff]
    }

    /// Build the prompt with filename, text, and existing tags substituted.
    fn build_prompt(&self, filename: &str, text: &str, existing_tags: &[String]) -> String {
        let truncated = self.truncate_text(text);
        let existing = if existing_tags.is_empty() {
            "[]".to_string()
        } else {
            serde_json::to_string(existing_tags).unwrap_or_else(|_| "[]".to_string())
        };

        PROMPT_TEMPLATE
            .replace("{{FILENAME}}", filename)
            .replace("{{EXISTING_TAGS}}", &existing)
            .replace("{{TEXT}}", truncated)
    }

    /// Parse the DeepSeek API response JSON into a TagResponse.
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

        let tag_response: TagResponse = serde_json::from_str(content).map_err(|e| {
            TagError::Parse(format!(
                "failed to parse tag JSON from model output: {}. Raw content: {}",
                e,
                &content[..content.len().min(200)]
            ))
        })?;

        Ok(tag_response)
    }
}

impl TagProvider for DeepSeekProvider {
    fn name(&self) -> &str {
        "deepseek"
    }

    fn generate_tags(
        &self,
        filename: &str,
        text: &str,
        existing_tags: &[String],
    ) -> Result<TagResponse, TagError> {
        let api_key = self.api_key()?;
        let prompt = self.build_prompt(filename, text, existing_tags);

        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {
                    "role": "user",
                    "content": prompt
                }
            ],
            "temperature": 0.2,
            "max_tokens": 256,
            "stream": false
        });

        let body_str = serde_json::to_string(&body).map_err(|e| {
            TagError::BadRequest(format!("failed to serialize request body: {}", e))
        })?;

        let response = self
            .agent
            .post(&self.endpoint)
            .set("Authorization", &format!("Bearer {}", api_key))
            .set("Content-Type", "application/json")
            .timeout(self.timeout)
            .send_string(&body_str)
            .map_err(|e| {
                match &e {
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
                }
            })?;

        let body_str = response.into_string().map_err(|e| {
            TagError::Unavailable(format!("failed to read API response body: {}", e))
        })?;

        Self::parse_response(&body_str)
    }

    fn health_check(&self) -> Result<(), TagError> {
        self.api_key()?;
        Ok(())
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
        let words: Vec<String> = (0..2005).map(|i| format!("word{}", i)).collect();
        let text = words.join(" ");
        let truncated = provider.truncate_text(&text);

        let word_count = truncated.split_whitespace().count();
        assert_eq!(word_count, 2000);
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
}
