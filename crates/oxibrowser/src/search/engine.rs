//! Core types: SearchResult, SearchError, SearchEngine trait, shared client builder.

use serde::Serialize;

// ---------------------------------------------------------------------------
// SearchResult — unified result type across all engines
// ---------------------------------------------------------------------------

/// A single search result from any engine.
///
/// GitHub sources include an optional `extra` field with repo metadata.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// Human-readable source label, e.g. "DuckDuckGo", "Wikipedia", "GitHub".
    pub source: String,
    /// GitHub-specific metadata (stars, forks, language, topics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<GitHubExtra>,
}

/// GitHub repository metadata attached to SearchResult when source is GitHub.
#[derive(Debug, Clone, Serialize)]
pub struct GitHubExtra {
    pub stars: u64,
    pub forks: u64,
    pub language: Option<String>,
    pub topics: Vec<String>,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// SearchOutput — top-level data container
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SearchOutput {
    pub query: String,
    pub source: String,
    pub engine: String,
    pub total_results: usize,
    pub results: Vec<SearchResult>,
}

// ---------------------------------------------------------------------------
// SearchError
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SearchError {
    Network(String),
    Parse(String),
    RateLimited(String),
    Captcha(String),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::Parse(msg) => write!(f, "parse error: {msg}"),
            Self::RateLimited(msg) => write!(f, "rate limited: {msg}"),
            Self::Captcha(msg) => write!(f, "captcha blocked: {msg}"),
        }
    }
}

impl std::error::Error for SearchError {}

// ---------------------------------------------------------------------------
// SearchEngine trait
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
#[allow(clippy::all)]
pub trait SearchEngine: Send + Sync {
    /// Human-readable engine name (e.g. "DuckDuckGo", "Wikipedia", "Bing").
    fn name(&self) -> &'static str;
    /// Execute a search query and return up to max_results results.
    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

// ---------------------------------------------------------------------------
// Shared client builder
// ---------------------------------------------------------------------------

/// Build a lightweight reqwest::Client for search (no SSRF filter, no cookies).
pub fn build_search_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent(format!("oxibrowser/{}", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .build()
        .expect("reqwest::Client::builder() failed")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Percent-encode a query string for URL inclusion.
pub fn url_encode(query: &str) -> String {
    url::form_urlencoded::byte_serialize(query.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// Simple HTML entity decoder for common entities.
pub fn decode_html_entities(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&'
            && let Some(end) = bytes[i..].iter().position(|&b| b == b';')
        {
            let entity = &text[i + 1..i + end];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                // Numeric entities
                _ if entity.starts_with('#') => {
                    let num = &entity[1..];
                    if let Ok(code) = num.parse::<u32>() {
                        char::from_u32(code)
                    } else if let Ok(code) = num.parse::<u32>() {
                        char::from_u32(code)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(c) = ch {
                result.push(c);
                i += end + 1; // skip past closing ;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_html_entities_basic() {
        assert_eq!(decode_html_entities("foo &amp; bar"), "foo & bar");
        assert_eq!(decode_html_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_html_entities("&quot;hello&quot;"), "\"hello\"");
    }

    #[test]
    fn test_decode_html_entities_numeric() {
        // 'A' is code point 65
        assert_eq!(decode_html_entities("&#65;"), "A");
        // 🙂 is U+1F642
        assert_eq!(decode_html_entities("&#128578;"), "🙂");
    }

    #[test]
    fn test_decode_html_entities_no_entity() {
        assert_eq!(decode_html_entities("hello world"), "hello world");
    }

    #[test]
    fn test_decode_html_entities_partial() {
        // Invalid entity — leave as-is
        let result = decode_html_entities("foo &bar; baz");
        assert_eq!(result, "foo &bar; baz");
    }

    #[test]
    fn test_url_encode_simple() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("rust"), "rust");
    }

    #[test]
    fn test_url_encode_special() {
        assert_eq!(url_encode("a+b"), "a%2Bb");
    }
}
