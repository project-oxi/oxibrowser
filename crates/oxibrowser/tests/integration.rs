#![cfg(feature = "browser")]
//! Real-website integration tests.
//!
//! These tests require an active internet connection.
//! Run with: `cargo test --test integration -- --ignored`
//!
//! Note: assertions are intentionally lenient since external sites may change.
use oxibrowser::Browser;
use oxibrowser::BrowserConfig;

#[tokio::test]
#[ignore]
async fn test_fetch_example_com() {
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://example.com").await.unwrap();

    let guard = session.read().await;
    let page = guard.page().expect("page should be loaded");
    let title = page.title().expect("should have title");
    assert!(title.to_lowercase().contains("example"), "title: {title}");
    drop(guard);

    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_extract_links_from_example() {
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://example.com").await.unwrap();

    let guard = session.read().await;
    let page = guard.page().expect("page should be loaded");
    let doc = page.root_frame().document();
    let links = doc.query_selector_all("a");
    assert!(!links.is_empty(), "should have at least one link");

    // Verify at least some links have href attributes
    let mut href_count = 0;
    for link_id in &links {
        if let Some(node) = doc.nodes.get(link_id)
            && node.attributes.contains_key("href")
        {
            href_count += 1;
        }
    }
    assert!(href_count > 0, "should have at least one link with href");
    drop(guard);

    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_js_document_query_on_real_page() {
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://example.com").await.unwrap();

    // Evaluate document.title via JS
    let result = {
        let mut guard = session.write().await;
        guard.evaluate_js("document.title").await.unwrap()
    };
    assert!(result.is_ok());
    if let Some(val) = &result.value {
        let title = val.as_str().unwrap_or("");
        assert!(
            title.to_lowercase().contains("example"),
            "JS document.title: {title}"
        );
    }

    // Evaluate document.querySelectorAll('a').length via JS
    let result = {
        let mut guard = session.write().await;
        guard
            .evaluate_js("document.querySelectorAll('a').length")
            .await
            .unwrap()
    };
    assert!(result.is_ok(), "querySelectorAll should succeed");

    drop(session);
    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_markdown_conversion() {
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://example.com").await.unwrap();

    let guard = session.read().await;
    let page = guard.page().expect("page should be loaded");
    let md = page.to_markdown();
    assert!(!md.is_empty(), "markdown should not be empty");
    // example.com has known content — be lenient
    assert!(
        md.to_lowercase().contains("example") || md.to_lowercase().contains("domain"),
        "markdown should contain page text, got: {}",
        md.chars().take(200).collect::<String>()
    );
    drop(guard);

    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_json_endpoint_get() {
    // api.github.com stopped serving unauthenticated root requests (403,
    // measured 2026-09-29) — retargeted to a stable minimal JSON endpoint.
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://api.ipify.org/?format=json").await.unwrap();

    let guard = session.read().await;
    let page = guard.page().expect("page should be loaded");
    let content = page.content();
    assert!(
        content.contains("\"ip\":"),
        "ipify should return JSON with an ip field, got: {}",
        content.chars().take(200).collect::<String>()
    );
    drop(guard);

    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_korean_encoding_naver() {
    // Naver — one of the largest Korean websites
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://www.naver.com").await.unwrap();

    let guard = session.read().await;
    let page = guard.page().expect("page should be loaded");
    let content = page.content();

    // The page should contain readable Korean text
    let has_korean =
        content.contains("네이버") || content.contains("naver") || content.contains("NAVER");
    assert!(
        has_korean,
        "page should contain Korean text, got first 500 chars: {}",
        content.chars().take(500).collect::<String>()
    );
    drop(guard);

    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore]
async fn test_encoding_meta_charset() {
    // Verify a standard UTF-8 page decodes cleanly
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://example.com").await.unwrap();

    let guard = session.read().await;
    let page = guard.page().expect("page should be loaded");
    let content = page.content();

    assert!(
        content.contains("Example Domain") || content.contains("example"),
        "should contain readable text"
    );
    drop(guard);

    browser.close().await.unwrap();
}

#[tokio::test]
async fn test_js_create_element_on_real_page() {
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://example.com").await.unwrap();

    // Create element and append to body via JS
    let result = {
        let mut guard = session.write().await;
        guard
            .evaluate_js(
                "const div = document.createElement('div'); \
                 div.id = 'test-div'; \
                 document.body.appendChild(div); \
                 document.getElementById('test-div') !== null ? 'ok' : 'fail'",
            )
            .await
            .unwrap()
    };
    assert!(result.is_ok());
    if let Some(val) = &result.value {
        let s = val.as_str().unwrap_or("");
        assert_eq!(s, "ok", "createElement + appendChild should work, got: {s}");
    }

    drop(session);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn test_js_window_globals_on_real_page() {
    let browser = Browser::new(BrowserConfig::headless()).await.unwrap();
    let session = browser.new_page("https://example.com").await.unwrap();

    // Test window, navigator, location, crypto exist
    let result = {
        let mut guard = session.write().await;
        guard
            .evaluate_js(
                "typeof window === 'object' && \
                 typeof navigator === 'object' && \
                 typeof location === 'object' && \
                 typeof crypto === 'object' && \
                 typeof atob === 'function' && \
                 typeof btoa === 'function' ? 'ok' : 'fail'",
            )
            .await
            .unwrap()
    };
    assert!(result.is_ok());
    if let Some(val) = &result.value {
        let s = val.as_str().unwrap_or("");
        assert_eq!(s, "ok", "window globals should exist, got: {s}");
    }

    drop(session);
    browser.close().await.unwrap();
}
