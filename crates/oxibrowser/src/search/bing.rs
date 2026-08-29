//! Bing search engine — HTML scraping with automatic DuckDuckGo fallback on CAPTCHA.
//!
//! Endpoint: GET https://www.bing.com/search?q=<query>
//!
//! Parse `<li class="b_algo">` blocks for results.
//! CAPTCHA detection: response body contains "CaptchaChallenge" or " Bing captcha ".
//! On CAPTCHA → transparent fallback to DuckDuckGo (no error propagated to caller).

use super::ddg::DuckDuckGoEngine;
use super::engine::{SearchEngine, SearchError, SearchResult, decode_html_entities, url_encode};

pub struct BingEngine {
    client: reqwest::Client,
    /// Fallback engine used when Bing returns a CAPTCHA challenge.
    ddg: DuckDuckGoEngine,
}

impl BingEngine {
    pub fn new(client: reqwest::Client) -> Self {
        let ddg_client = client.clone();
        Self {
            client,
            ddg: DuckDuckGoEngine::new(ddg_client),
        }
    }
}

#[async_trait::async_trait]
impl SearchEngine for BingEngine {
    fn name(&self) -> &'static str {
        "Bing"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        match self.search_bing(query, max_results).await {
            Ok(results) => Ok(results),
            Err(SearchError::Captcha(msg)) => {
                tracing::warn!("{msg}, falling back to DuckDuckGo");
                self.ddg.search(query, max_results).await
            }
            Err(e) => Err(e),
        }
    }
}

impl BingEngine {
    async fn search_bing(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let encoded = url_encode(query);
        let url = format!("https://www.bing.com/search?q={encoded}");

        let response = self
            .client
            .get(&url)
            .header("Accept-Language", "en-US,en;q=0.5")
            .send()
            .await
            .map_err(|e| SearchError::Network(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(SearchError::Network(format!("Bing returned HTTP {status}")));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|e| SearchError::Network(e.to_string()))?;
        // Bing sometimes mislabels UTF-8 responses with a legacy charset
        // header, which makes reqwest's `.text()` mojibake CJK results.
        // Trust the bytes: Bing's HTML is UTF-8 in practice.
        let html = String::from_utf8_lossy(&bytes);

        // CAPTCHA detection
        if html.contains("CaptchaChallenge") || html.contains(" Bing captcha ") {
            return Err(SearchError::Captcha(
                "Bing CAPTCHA challenge detected".into(),
            ));
        }

        Ok(parse_bing_html(&html, max_results))
    }
}

/// Parse Bing search results HTML.
///
/// Bing result structure:
/// ```html
/// <li class="b_algo">
///   <h2><a href="URL">Title</a></h2>
///   <div class="b_caption"><p>Snippet text</p></div>
/// </li>
/// ```
fn parse_bing_html(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    for block in html.split(r##"class="b_algo""##).skip(1) {
        if results.len() >= max_results {
            break;
        }

        // Extract URL and title from <h2><a href="URL">Title</a></h2>
        let (title, url) = extract_bing_link(block);

        // Extract snippet from <p> inside .b_caption
        let snippet = extract_bing_snippet(block);

        if url.is_empty() || title.is_empty() {
            continue;
        }

        results.push(SearchResult {
            title: decode_html_entities(&title),
            url,
            snippet: decode_html_entities(&snippet),
            source: "Bing".into(),
            extra: None,
        });
    }

    results
}

/// Extract URL and title from a Bing result block.
///
/// Handles `<h2 class=""><a href="...">Title (with <strong>tags</strong>)</a></h2>`
fn extract_bing_link(block: &str) -> (String, String) {
    // Find <h2 (with or without attributes)
    let h2_start = find_tag_open(block, "h2").unwrap_or(usize::MAX);
    if h2_start == usize::MAX {
        return (String::new(), String::new());
    }
    let after_h2_close = match block[h2_start..].find('>') {
        Some(pos) => h2_start + pos + 1,
        None => return (String::new(), String::new()),
    };
    let after_h2 = &block[after_h2_close..];

    // Find <a
    let after_a = match after_h2.find("<a ") {
        Some(pos) => &after_h2[pos + 3..],
        None => return (String::new(), String::new()),
    };

    // Extract href="..."
    let url = extract_attr_value(after_a, "href");

    // Find > after <a> attributes
    let after_gt = match after_a.find('>') {
        Some(pos) => &after_a[pos + 1..],
        None => return (String::new(), String::new()),
    };

    // Find </a> and extract text (strip inner HTML tags like <strong>)
    let title = match after_gt.find("</a>") {
        Some(pos) => {
            let raw = &after_gt[..pos];
            strip_html_tags(raw)
        }
        None => String::new(),
    };

    (title, decode_bing_redirect(&url))
}

/// Decode Bing's `/ck/a` tracking redirect to the real target URL.
///
/// Bing wraps organic results in
/// `https://www.bing.com/ck/a?...&u=a1<base64url>` where the payload after
/// the `a1` prefix is the base64url (unpadded) encoded target. Non-tracking
/// URLs are returned unchanged.
fn decode_bing_redirect(url: &str) -> String {
    if !url.contains("/ck/a") {
        return url.to_string();
    }
    let Some(u_pos) = url.find("u=a1") else {
        return url.to_string();
    };
    let payload = &url[u_pos + "u=a1".len()..];
    let payload = match payload.find('&') {
        Some(end) => &payload[..end],
        None => payload,
    };
    use base64::Engine as _;
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => url.to_string(),
    }
}

/// Extract snippet text from a Bing result block.
/// Looks for `<p>...</p>` inside the result caption area.
fn extract_bing_snippet(block: &str) -> String {
    // Find <div class="b_caption"> or the first <p> after it
    let search_area = match block.find("b_caption") {
        Some(pos) => &block[pos..],
        None => block,
    };

    // Find first <p
    let p_start = find_tag_open(search_area, "p").unwrap_or(usize::MAX);
    if p_start == usize::MAX {
        return String::new();
    };

    // Find > after <p attributes
    let after_p_start = match search_area[p_start..].find('>') {
        Some(pos) => p_start + pos + 1,
        None => return String::new(),
    };

    let after_p = &search_area[after_p_start..];

    // Find </p>
    match after_p.find("</p>") {
        Some(pos) => {
            let text = &after_p[..pos];
            strip_html_tags(text)
        }
        None => String::new(),
    }
}

/// Extract the value of an attribute from an HTML tag opening.
/// Handles `attr="value"` and `attr='value'` and `attr=value` (unquoted).
fn extract_attr_value(s: &str, attr: &str) -> String {
    let pattern = format!(r#"{attr}="#);
    let pos = match s.find(&pattern) {
        Some(p) => p + pattern.len(),
        None => return String::new(),
    };

    let after = &s[pos..];
    let chars: Vec<char> = after.chars().collect();
    if chars.is_empty() {
        return String::new();
    }

    if chars[0] == '"' || chars[0] == '\'' {
        let quote = chars[0];
        let mut val = String::new();
        for &c in &chars[1..] {
            if c == quote {
                break;
            }
            val.push(c);
        }
        val
    } else {
        // Unquoted — read until whitespace, `>`, or `/`
        let mut val = String::new();
        for &c in &chars {
            if c == ' ' || c == '\t' || c == '>' || c == '/' {
                break;
            }
            val.push(c);
        }
        val
    }
}

/// Strip HTML tags from text.
fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    result.trim().to_string()
}

/// Find an HTML open tag like `<tag>` or `<tag attr=...>`.
fn find_tag_open(s: &str, tag: &str) -> Option<usize> {
    let exact = format!("<{tag}>");
    let with_attrs = format!("<{tag} ");
    let with_slash = format!("<{tag}/");
    let pos1 = s.find(&exact);
    let pos2 = s.find(&with_attrs);
    let pos3 = s.find(&with_slash);
    match (pos1, pos2, pos3) {
        (Some(p1), Some(p2), _) => Some(p1.min(p2)),
        (Some(p1), None, _) => Some(p1),
        (None, Some(p2), _) => Some(p2),
        (None, None, Some(p3)) => Some(p3),
        (None, None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_attr_value_double_quoted() {
        assert_eq!(
            extract_attr_value(r#"href="https://example.com" class="x""#, "href"),
            "https://example.com"
        );
    }

    #[test]
    fn test_extract_attr_value_single_quoted() {
        assert_eq!(
            extract_attr_value(r#"href='https://example.com'"#, "href"),
            "https://example.com"
        );
    }

    #[test]
    fn test_extract_attr_value_not_found() {
        assert_eq!(extract_attr_value("<div>hello</div>", "href"), "");
    }

    #[test]
    fn test_parse_bing_html_simple() {
        let html = r#"
<ol id="b_results">
<li class="b_algo">
<h2><a href="https://example.com">Example Domain</a></h2>
<div class="b_caption"><p>This domain is for use in illustrative examples.</p></div>
</li>
</ol>"#;
        let results = parse_bing_html(html, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example Domain");
        assert_eq!(results[0].url, "https://example.com");
        assert_eq!(
            results[0].snippet,
            "This domain is for use in illustrative examples."
        );
        assert_eq!(results[0].source, "Bing");
    }

    #[test]
    fn test_parse_bing_html_no_results() {
        let html = "<html><body>No results found.</body></html>";
        let results = parse_bing_html(html, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_bing_html_max_results() {
        let html = r#"
<li class="b_algo"><h2><a href="https://a.com">A</a></h2><div class="b_caption"><p>First</p></div></li>
<li class="b_algo"><h2><a href="https://b.com">B</a></h2><div class="b_caption"><p>Second</p></div></li>
<li class="b_algo"><h2><a href="https://c.com">C</a></h2><div class="b_caption"><p>Third</p></div></li>
"#;
        let results = parse_bing_html(html, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "A");
        assert_eq!(results[1].title, "B");
    }

    #[test]
    fn test_captcha_detection() {
        let html = "<html>CaptchaChallenge</html>";
        assert!(html.contains("CaptchaChallenge"));

        let html2 = "<html> Bing captcha detected</html>";
        assert!(html2.contains(" Bing captcha "));
    }

    #[test]
    fn test_decode_bing_redirect_real_payload() {
        // Real-world shape: base64url (unpadded) of the target after `u=a1`.
        let tracking = "https://www.bing.com/ck/a?!&&p=abc&u=a1aHR0cHM6Ly9uYW11Lndpa2kvdy9UT0tJTyglRUMlOTUlODQlRUMlOUQlQjQlRUIlOEYlOEMp&ntb=1";
        assert_eq!(
            decode_bing_redirect(tracking),
            "https://namu.wiki/w/TOKIO(%EC%95%84%EC%9D%B4%EB%8F%8C)"
        );
    }

    #[test]
    fn test_decode_bing_redirect_passthrough() {
        assert_eq!(
            decode_bing_redirect("https://example.com/page"),
            "https://example.com/page"
        );
        // /ck/a without a u=a1 payload — return unchanged.
        assert_eq!(
            decode_bing_redirect("https://www.bing.com/ck/a?x=1"),
            "https://www.bing.com/ck/a?x=1"
        );
    }
}
