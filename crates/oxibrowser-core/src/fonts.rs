//! `@font-face` webfont loading (public-API path, no Blitz fork).
//!
//! `@font-face` rules declared in inline `<style>` are scanned for font-file
//! URLs, the files are fetched via the normal network stack, and the bytes are
//! handed to the render crate's `from_html_with_fonts`, which registers them
//! into a Parley `FontContext` via `Collection::register_fonts`. Blitz's
//! `DocumentConfig.font_ctx` (public) carries that context into layout, so the
//! declared fonts reach Stylo/Taffy text shaping.
//!
//! Session injection scans both inline styles and fetched initial stylesheets.
//! External font URLs use their stylesheet's final response URL as the base.
//! Dynamic stylesheet fetching and recursive network `@import` loading remain
//! outside the current resource-loading scope.

/// Extract font-file URLs from `@font-face` rules in CSS (or HTML containing
/// inline `<style>`). Scans each `@font-face { … }` block for `src:` `url(…)`
/// candidates and returns them in document order. Only `http`/`https`/`data`
/// URLs are returned; other schemes are dropped.
pub fn extract_font_face_urls(css_or_html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = css_or_html;
    while let Some(start) = rest.find("@font-face") {
        rest = &rest[start..];
        let Some(brace_off) = rest.find('{') else {
            break;
        };
        let after_brace = &rest[brace_off + 1..];
        let Some(close_rel) = after_brace.find('}') else {
            break;
        };
        let body = &after_brace[..close_rel];

        // Only collect URLs from `src:` declarations (not other url() uses).
        let mut b = body;
        while let Some(src_off) = b.to_ascii_lowercase().find("src") {
            b = &b[src_off..];
            let Some(colon) = b.find(':') else { break };
            let decl_end = b[colon..].find(';').map(|p| colon + p).unwrap_or(b.len());
            let decl = &b[colon + 1..decl_end];
            // Capture every url(...) within this src declaration.
            let mut d = decl;
            while let Some(u) = d.find("url(") {
                d = &d[u + 4..];
                let d_trim = d.trim_start_matches([' ', '\t', '\n', '\r', '\'', '"']);
                let end = d_trim
                    .find([')', '\'', '"', ' ', '\t'])
                    .unwrap_or(d_trim.len());
                let url = d_trim[..end].trim();
                if !url.is_empty()
                    && (url.starts_with("http")
                        || url.starts_with('/')
                        || url.starts_with("data:")
                        || url.starts_with("./")
                        || !url.contains(':'))
                {
                    urls.push(url.to_string());
                }
                d = &d[end..];
            }
            b = &b[decl_end..];
        }
        rest = &after_brace[close_rel..];
    }
    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_font_face_url() {
        let css =
            "@font-face { font-family: 'Test'; src: url('/fonts/test.woff2') format('woff2'); }";
        let urls = extract_font_face_urls(css);
        assert_eq!(urls, vec!["/fonts/test.woff2".to_string()]);
    }

    #[test]
    fn extracts_multiple_rules_in_order() {
        let css = "@font-face{font-family:A;src:url(a.woff);} body{color:red} @font-face{font-family:B;src:url('b.ttf')}";
        let urls = extract_font_face_urls(css);
        assert_eq!(urls, vec!["a.woff".to_string(), "b.ttf".to_string()]);
    }

    #[test]
    fn ignores_non_src_url_uses() {
        // background-image url() inside a rule that is NOT @font-face must be ignored;
        // and even within @font-face only src: counts.
        let css = "@font-face { font-family: X; background: url(bg.png); src: url(x.woff); }";
        let urls = extract_font_face_urls(css);
        assert_eq!(urls, vec!["x.woff".to_string()]);
    }

    #[test]
    fn no_font_face_returns_empty() {
        assert!(extract_font_face_urls("body { color: red; }").is_empty());
        assert!(extract_font_face_urls("").is_empty());
    }
}
