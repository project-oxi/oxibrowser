#![cfg(feature = "webgl")]

use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};

fn percent_encode_html(html: &str) -> String {
    let mut encoded = String::with_capacity(html.len());
    for byte in html.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[tokio::test]
async fn webgl_contexts_on_separate_canvases_keep_independent_pixels() {
    let html = r##"<!doctype html>
<html><head><style>body{margin:0}canvas{display:block;width:32px;height:32px}</style></head>
<body><canvas id="first" width="32" height="32"></canvas><canvas id="second" width="32" height="32"></canvas>
<script>
(() => {
  const firstCanvas = document.getElementById('first');
  const secondCanvas = document.getElementById('second');
  const first = firstCanvas.getContext('webgl', { alpha: false });
  const second = secondCanvas.getContext('webgl', { alpha: false });
  if (!first || !second || first === second) throw new Error('contexts were not isolated');
  if (firstCanvas.getContext('webgl') !== first || secondCanvas.getContext('webgl') !== second) {
    throw new Error('context identity was not stable');
  }
  first.viewport(0, 0, 32, 32);
  second.viewport(0, 0, 32, 32);
  first.clearColor(1, 0, 0, 1);
  first.clear(first.COLOR_BUFFER_BIT);
  second.clearColor(0, 0, 1, 1);
  second.clear(second.COLOR_BUFFER_BIT);
  first.clearColor(0, 1, 0, 1);
  first.clear(first.COLOR_BUFFER_BIT);
  second.clearColor(1, 1, 0, 1);
  second.clear(second.COLOR_BUFFER_BIT);
  window.__webglContextsReady = true;
})();
</script></body></html>"##;
    let url = format!("data:text/html;charset=utf-8,{}", percent_encode_html(html));

    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.expect("browser");
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .expect("session");
    session.navigate(&url).await.expect("navigate");

    let ready = session
        .evaluate_js("String(window.__webglContextsReady === true)")
        .await
        .expect("evaluate readiness");
    assert_eq!(
        ready.value.as_ref().and_then(|value| value.as_str()),
        Some("true")
    );

    let png = session
        .capture_screenshot_png(64)
        .await
        .expect("capture both canvases");
    let image = image::load_from_memory(&png)
        .expect("decode screenshot")
        .to_rgba8();
    let first = image.get_pixel(16, 16);
    let second = image.get_pixel(16, 48);
    assert_eq!(&first.0[..3], &[0, 255, 0], "first context pixel");
    assert_eq!(&second.0[..3], &[255, 255, 0], "second context pixel");
}

#[tokio::test]
async fn creating_and_resizing_another_canvas_keeps_current_context_pixels() {
    let html = r#"<!doctype html><html><body><canvas id="first" width="32" height="32"></canvas><canvas id="second" width="32" height="32"></canvas>
    <script>
      const a=document.getElementById('first').getContext('webgl',{alpha:false,preserveDrawingBuffer:true});
      a.clearColor(1,0,0,1);a.clear(a.COLOR_BUFFER_BIT);
      const second=document.getElementById('second');
      const b=second.getContext('webgl',{alpha:false,preserveDrawingBuffer:true});
      a.clearColor(0,1,0,1);a.clear(a.COLOR_BUFFER_BIT);
      b.clearColor(0,0,1,1);b.clear(b.COLOR_BUFFER_BIT);
      function pixel(gl){const p=new Uint8Array(4);gl.readPixels(0,0,1,1,gl.RGBA,gl.UNSIGNED_BYTE,p);return [...p,gl.getError()];}
      const afterCreate=pixel(a);
      second.width=48;
      a.clearColor(1,1,0,1);a.clear(a.COLOR_BUFFER_BIT);
      b.clearColor(0,0,1,1);b.clear(b.COLOR_BUFFER_BIT);
      globalThis.currentContextProbe={afterCreate,afterResize:pixel(a),second:pixel(b)};
    </script></body></html>"#;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate(&format!(
            "data:text/html;charset=utf-8,{}",
            percent_encode_html(html)
        ))
        .await
        .unwrap();
    let result = session
        .evaluate_js("JSON.stringify(currentContextProbe)")
        .await
        .unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    let value: serde_json::Value = serde_json::from_str(
        result
            .value
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "afterCreate":[0,255,0,255,0],"afterResize":[255,255,0,255,0],"second":[0,0,255,255,0]
        })
    );
}
