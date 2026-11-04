//! Initial external CSS retains its source link instead of becoming an inline
//! replacement. The HTTP fixture, DOM helper and rendered pixels are real.

use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn session() -> Session {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap()
}

async fn evaluate(session: &mut Session, expression: &str) -> Value {
    let result = session
        .evaluate_js(&format!("JSON.stringify({expression})"))
        .await
        .unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    serde_json::from_str(result.value.as_ref().and_then(Value::as_str).unwrap()).unwrap()
}

const HTML: &str = r#"<html><head>
<link id='original' rel='stylesheet' href='/assets/app.css' media='screen' crossorigin='anonymous' nonce='retained'>
<script>
globalThis.preloadDone = false;
function preload(deps) {
  return Promise.all(deps.map(dep => {
    const href = new URL(dep, location.href).href;
    if (Array.from(document.querySelectorAll('link')).some(link => link.href === href && link.rel === 'stylesheet')) return;
    return new Promise(resolve => {
      const link = document.createElement('link'); link.rel = 'stylesheet'; link.href = href;
      link.addEventListener('load', resolve); document.head.appendChild(link);
    });
  }));
}
preload(['/assets/app.css']).then(() => { preloadDone = true; });
</script></head><body><div id='box'></div></body></html>"#;

#[tokio::test]
async fn downloaded_css_link_satisfies_preload_helper_and_paints_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HTML, "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/app.css"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "body{margin:0} #box{width:91px;height:64px;background:rgb(0,0,255)}",
            "text/css",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(evaluate(&mut session, "(() => { const l = document.querySelector('link[rel=stylesheet]'); return [l.href,l.id,l.getAttribute('media'),l.getAttribute('crossorigin'),l.getAttribute('nonce'),l.getAttribute('href'),document.querySelectorAll('link').length,preloadDone,document.querySelector('#box').getBoundingClientRect().width]; })()").await,
        json!([format!("{}/assets/app.css",server.uri()),"original","screen","anonymous","retained","/assets/app.css",1,true,91]));
    let png = session.capture_screenshot_png(1280).await.unwrap();
    let image = image::load_from_memory(&png).unwrap().into_rgba8();
    assert_eq!(image.get_pixel(40, 40).0, [0, 0, 255, 255]);
    server.verify().await;
}

#[tokio::test]
async fn opaque_document_and_missing_stylesheet_are_safe_and_keep_link_nodes() {
    let mut session = session().await;
    session.navigate("data:text/html,<html><head><link id='relative' rel='stylesheet' href='app.css'></head><body><p>safe</p></body></html>").await.unwrap();
    assert_eq!(
        evaluate(
            &mut session,
            "document.querySelector('#relative').getAttribute('href')"
        )
        .await,
        json!("app.css")
    );
    assert!(
        !session
            .capture_screenshot_png(1280)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn revisiting_same_url_fetches_new_document_css_without_resource_bleed() {
    let server = MockServer::start().await;
    let mut session = session().await;
    for (width, color) in [(91, "blue"), (123, "red")] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(HTML, "text/html"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/assets/app.css"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("body{{margin:0}} #box{{width:{width}px;height:64px;background:{color}}}"),
                "text/css",
            ))
            .expect(1)
            .mount(&server)
            .await;
        session
            .navigate(&format!("{}/page", server.uri()))
            .await
            .unwrap();
        assert_eq!(
            evaluate(
                &mut session,
                "document.querySelector('#box').getBoundingClientRect().width"
            )
            .await,
            json!(width)
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn http_css_charset_is_decoded_before_blitz_utf8_parsing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HTML, "text/html"))
        .mount(&server)
        .await;
    let mut css =
        b"body{margin:0} #box{width:88px;height:64px;background:blue} #box::after{content:'caf"
            .to_vec();
    css.push(0xe9); // genuine Windows-1252 é, invalid as standalone UTF-8
    css.extend_from_slice(b"'}");
    Mock::given(method("GET"))
        .and(path("/assets/app.css"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(css, "text/css; charset=windows-1252"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(
        evaluate(
            &mut session,
            "document.querySelector('#box').getBoundingClientRect().width"
        )
        .await,
        json!(88)
    );
    let image = image::load_from_memory(&session.capture_screenshot_png(1280).await.unwrap())
        .unwrap()
        .into_rgba8();
    assert_eq!(image.get_pixel(40, 40).0, [0, 0, 255, 255]);
    server.verify().await;
}

#[tokio::test]
async fn redirected_stylesheet_keeps_original_href_and_resolves_relative_font_at_final_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(HTML, "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/app.css"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/relocated/theme.css"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/relocated/theme.css"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("@font-face{font-family:Fixture;src:url('fonts/fixture.woff')} #box{width:77px;height:64px;background:blue}","text/css"))
        .expect(1).mount(&server).await;
    // This fixture checks routing only; it makes no font rendering claim.
    Mock::given(method("GET"))
        .and(path("/relocated/fonts/fixture.woff"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![0, 1, 2, 3], "font/woff"))
        .expect(1)
        .mount(&server)
        .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(evaluate(&mut session,"[document.querySelector('#original').getAttribute('href'),document.querySelector('#original').href,document.querySelector('#box').getBoundingClientRect().width]").await,
        json!(["/assets/app.css",format!("{}/assets/app.css",server.uri()),77]));
    server.verify().await;
}

#[tokio::test]
async fn different_redirect_requests_to_one_final_url_apply_both_actual_responses() {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html><head><link id='link-a' rel='stylesheet' href='/a.css'><link id='link-b' rel='stylesheet' href='/b.css'></head><body><div id='a'></div><div id='b'></div></body></html>","text/html"))
        .mount(&server).await;
    for name in ["a", "b"] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}.css")))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/theme.css"))
            .expect(1)
            .mount(&server)
            .await;
    }
    let responses = Arc::new(AtomicU64::new(0));
    Mock::given(method("GET"))
        .and(path("/theme.css"))
        .respond_with(move |_: &wiremock::Request| {
            let body = if responses.fetch_add(1, Ordering::SeqCst) == 0 {
                "#a{width:10px;height:10px}"
            } else {
                "#b{width:20px;height:10px}"
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/css")
        })
        .expect(2)
        .mount(&server)
        .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(evaluate(&mut session,"[document.querySelector('#a').getBoundingClientRect().width,document.querySelector('#b').getBoundingClientRect().width,document.querySelector('#link-a').getAttribute('href'),document.querySelector('#link-b').getAttribute('href')]").await,
        json!([10,20,"/a.css","/b.css"]));
    server.verify().await;
}
