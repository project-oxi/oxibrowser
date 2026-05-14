//! Benchmarks for OxiBrowser core operations.
//!
//! Run with: cargo bench

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use oxibrowser_core::{Browser, BrowserConfig};
use oxibrowser_webapi::Document;
use std::time::Duration;

fn bench_html_parsing(c: &mut Criterion) {
    let simple_html =
        r#"<html><head><title>Test</title></head><body><h1>Hello</h1><p>World</p></body></html>"#;
    let complex_html = include_str!("../benches/fixtures/complex.html");

    let mut group = c.benchmark_group("html_parsing");
    group.bench_function("simple", |b| b.iter(|| Document::parse(simple_html)));
    group.bench_function("complex", |b| b.iter(|| Document::parse(complex_html)));
    group.finish();
}

fn bench_dom_queries(c: &mut Criterion) {
    let html = r#"
    <html><body>
        <div id="main" class="container">
            <h1>Title</h1>
            <p class="text">Paragraph 1</p>
            <p class="text">Paragraph 2</p>
            <a href="https://example.com">Link</a>
            <ul><li>Item 1</li><li>Item 2</li><li>Item 3</li></ul>
        </div>
    </body></html>"#;

    let doc = Document::parse(html);

    let mut group = c.benchmark_group("dom_queries");
    group.bench_function("query_selector_id", |b| {
        b.iter(|| doc.query_selector("#main"))
    });
    group.bench_function("query_selector_tag", |b| {
        b.iter(|| doc.query_selector("h1"))
    });
    group.bench_function("query_selector_class", |b| {
        b.iter(|| doc.query_selector(".text"))
    });
    group.bench_function("query_selector_all_p", |b| {
        b.iter(|| doc.query_selector_all("p"))
    });
    group.bench_function("query_text", |b| b.iter(|| doc.query_text("h1")));
    group.finish();
}

fn bench_to_markdown(c: &mut Criterion) {
    let html = r#"
    <html><body>
        <article>
            <h1>Main Title</h1>
            <h2>Section 1</h2>
            <p>This is a paragraph with <strong>bold</strong> and <em>italic</em> text.</p>
            <ul>
                <li>Item 1</li>
                <li>Item 2</li>
                <li>Item 3</li>
            </ul>
            <h2>Section 2</h2>
            <p>Another paragraph with a <a href="https://example.com">link</a>.</p>
            <code>let x = 42;</code>
        </article>
    </body></html>"#;

    let doc = Document::parse(html);

    c.bench_function("to_markdown", |b| b.iter(|| doc.to_markdown()));
}

// ---------------------------------------------------------------------------
// Browser lifecycle benchmarks
// ---------------------------------------------------------------------------

fn bench_browser_startup(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    c.bench_function("browser_startup", |b| {
        b.to_async(&rt).iter(|| async {
            let browser = Browser::new(BrowserConfig::default()).await.unwrap();
            browser.close().await.unwrap();
        });
    });
}

fn bench_session_navigate(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let browser = rt.block_on(Browser::new(BrowserConfig::default())).unwrap();

    c.bench_function("session_navigate_data_uri", |b| {
        b.to_async(&rt).iter(|| {
            let browser = browser.clone();
            async move {
                browser.new_page("data:text/html,<h1>Hello</h1>").await
            }
        });
    });

    rt.block_on(browser.close()).unwrap();
}

fn bench_js_eval(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let browser = rt.block_on(Browser::new(BrowserConfig::default())).unwrap();
    let session = rt.block_on(browser.new_page("data:text/html,<p>test</p>")).unwrap();

    c.bench_function("js_eval_simple", |b| {
        b.to_async(&rt).iter(|| {
            let session = session.clone();
            async move {
                session.write().await.evaluate_js("1 + 1")
            }
        });
    });

    c.bench_function("js_eval_dom_query", |b| {
        b.to_async(&rt).iter(|| {
            let session = session.clone();
            async move {
                session.write().await.evaluate_js("document.querySelector('p').textContent")
            }
        });
    });

    rt.block_on(browser.close()).unwrap();
}

fn bench_session_memory(c: &mut Criterion) {
    c.bench_function("session_memory_overhead", |b| {
        b.iter(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let browser = rt.block_on(Browser::new(BrowserConfig::default())).unwrap();
            let _session = rt.block_on(browser.new_page("data:text/html,<p>test</p>"));

            #[cfg(target_os = "macos")]
            {
                // On macOS, use `/usr/bin/time -l` for accurate RSS measurement
                // This benchmark just creates the session and measures overhead
                println!("Session created (measure RSS via /usr/bin/time -l)");
            }

            #[cfg(target_os = "linux")]
            {
                if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                    for line in status.lines() {
                        if line.starts_with("VmRSS:") {
                            println!("VmRSS: {}", line.trim());
                        }
                    }
                }
            }

            rt.block_on(browser.close()).unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_html_parsing,
    bench_dom_queries,
    bench_to_markdown,
    bench_browser_startup,
    bench_session_navigate,
    bench_js_eval,
    bench_session_memory
);
criterion_main!(benches);
