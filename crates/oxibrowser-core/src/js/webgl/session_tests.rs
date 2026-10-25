//! End-to-end WebGL pipeline tests (in-crate: the session helpers reach
//! crate-private constructors).
//!
//! These exercise the full path — page JS → Boa bindings → ANGLE GL →
//! readback → blitz compositing → PNG — so they need the pinned ANGLE
//! libraries: run `scripts/setup-angle.sh` (or `scripts/build-angle.sh`),
//! then `cargo test -p oxibrowser-core --features webgl webgl_`.
//!
//! Geometry note (cost a full debugging pass once): the test triangle
//! (-1,-1),(1,-1),(-1,1) has its hypotenuse on x+y=0; pixels whose CENTER
//! lands on that line are fill-rule excluded. Sample strictly interior
//! points: bitmap(x, y) = GL(x, 63-y).

use std::sync::Arc;

use parking_lot::RwLock;

use crate::browser::BrowserId;
use crate::config::BrowserConfig;
use crate::context::{BrowserContext, ContextId};
use crate::network::HttpClient;
use crate::network::cookie::CookieJar;
use crate::session::Session;

fn make_context(config: &BrowserConfig) -> Arc<BrowserContext> {
    let http_client =
        Arc::new(HttpClient::new(config, Arc::new(RwLock::new(CookieJar::new()))).unwrap());
    Arc::new(BrowserContext::with_cookie_jar(
        ContextId::test_next(),
        None,
        http_client,
        Arc::new(RwLock::new(CookieJar::new())),
    ))
}

fn urlencoding_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn session_with(html: &str) -> Session {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let context = make_context(&config);
    let mut session = Session::new(BrowserId::next(), config, context)
        .await
        .expect("session");
    session
        .navigate(&format!(
            "data:text/html;charset=utf-8,{}",
            urlencoding_escape(html)
        ))
        .await
        .expect("navigate");
    session
}

const TRIANGLE_HTML: &str = r##"<!doctype html>
<html><head><style>
  body { margin: 0; }
  canvas { width: 64px; height: 64px; display: block; }
</style></head>
<body>
<canvas id="gl" width="64" height="64"></canvas>
<script>
  const canvas = document.getElementById('gl');
  const gl = canvas.getContext('webgl', { alpha: false });
  if (!gl) { throw new Error('no webgl context'); }
  const vs = gl.createShader(gl.VERTEX_SHADER);
  gl.shaderSource(vs, 'attribute vec2 a_pos; void main() { gl_Position = vec4(a_pos, 0.0, 1.0); }');
  gl.compileShader(vs);
  if (!gl.getShaderParameter(vs, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(vs));
  const fs = gl.createShader(gl.FRAGMENT_SHADER);
  gl.shaderSource(fs, 'precision mediump float; void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }');
  gl.compileShader(fs);
  if (!gl.getShaderParameter(fs, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(fs));
  const prog = gl.createProgram();
  gl.attachShader(prog, vs);
  gl.attachShader(prog, fs);
  gl.bindAttribLocation(prog, 0, 'a_pos');
  gl.linkProgram(prog);
  if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(prog));
  gl.useProgram(prog);
  const buf = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, buf);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  gl.viewport(0, 0, 64, 64);
  gl.clearColor(0.0, 0.0, 1.0, 1.0);
  gl.clear(gl.COLOR_BUFFER_BIT);
  gl.drawArrays(gl.TRIANGLES, 0, 3);
</script>
</body></html>
"##;

/// Full pipeline: a WebGL triangle must appear in the page screenshot, with
/// the blue clear color around it. Red triangle over blue background makes a
/// created-but-not-rasterizing backend fail loudly.
#[tokio::test(flavor = "multi_thread")]
async fn webgl_triangle_reaches_screenshot() {
    let mut session = session_with(TRIANGLE_HTML).await;
    let png = session.capture_screenshot_png(256).await.expect("capture");
    let img = image::load_from_memory(&png).expect("decode");
    let rgba = img.to_rgba8();

    let at = |x: u32, y: u32| -> [u8; 3] {
        let p = rgba.get_pixel(x, y);
        [p[0], p[1], p[2]]
    };
    // GL (8,8) → NDC (-0.75,-0.75): strictly inside → red fragment.
    let c = at(8, 55);
    assert!(
        c[0] >= 200 && c[1] <= 60 && c[2] <= 60,
        "expected red triangle pixel at (8,55), got {c:?}"
    );
    // GL (60,60) → NDC (0.875,0.875): outside → blue clear color.
    let o = at(60, 3);
    assert!(
        o[2] >= 200 && o[0] <= 60 && o[1] <= 60,
        "expected blue clear pixel at (60,3), got {o:?}"
    );
}

/// `getContext` identity + type pinning at the session level.
#[tokio::test(flavor = "multi_thread")]
async fn webgl_context_identity_and_pinning() {
    let mut session = session_with("<canvas id=\"a\"></canvas>").await;
    let script = r#"
  const c = document.getElementById('a');
  const g1 = c.getContext('webgl');
  const g2 = c.getContext('webgl');
  const other = c.getContext('webgl2');
  JSON.stringify({
    ok: !!g1, same: g1 === g2, pinned: other === null,
    lost: g1.isContextLost(),
    attrs: g1.getContextAttributes().alpha === true,
    w: g1.drawingBufferWidth, h: g1.drawingBufferHeight,
  })
"#;
    let r = session.evaluate_js(script).await.expect("evaluate");
    let err = r.exception.clone();
    let s = r
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("script produced no value; error = {err:?}"));
    let v: serde_json::Value = serde_json::from_str(s).expect("json");
    assert_eq!(v["ok"], true, "context must be created");
    assert_eq!(v["same"], true, "identity: same object both calls");
    assert_eq!(v["pinned"], true, "webgl2 after webgl must be null");
    assert_eq!(v["lost"], false);
    assert_eq!(v["attrs"], true, "getContextAttributes().alpha");
    assert_eq!(v["w"], 300);
    assert_eq!(v["h"], 150);
}

/// Cross-context resource rejection + deletion state.
#[tokio::test(flavor = "multi_thread")]
async fn webgl_resource_ownership() {
    let html = r#"<!doctype html><html><body>
<canvas id="a"></canvas><canvas id="b"></canvas></body>
<script>
  const ga = document.getElementById('a').getContext('webgl');
  const gb = document.getElementById('b').getContext('webgl');
  const buf = ga.createBuffer();
  ga.bindBuffer(ga.ARRAY_BUFFER, buf); // own context: fine
  const errOwn = ga.getError();
  gb.bindBuffer(gb.ARRAY_BUFFER, buf); // cross-context: INVALID_OPERATION
  const errCross = gb.getError();
  ga.deleteBuffer(buf);
  ga.bindBuffer(ga.ARRAY_BUFFER, buf); // deleted: INVALID_OPERATION
  const errDeleted = ga.getError();
  window.__result = JSON.stringify({
    own: errOwn, cross: errCross, deleted: errDeleted,
    isBuf: ga.isBuffer(buf),
  });
</script></html>"#;
    let mut session = session_with(html).await;
    let r = session
        .evaluate_js("window.__result")
        .await
        .expect("evaluate");
    let s = r.value.as_ref().and_then(|v| v.as_str()).expect("string");
    let v: serde_json::Value = serde_json::from_str(s).expect("json");
    assert_eq!(v["own"], 0, "own-context bind must not error");
    assert_eq!(v["cross"], 0x0502, "cross-context bind → INVALID_OPERATION");
    assert_eq!(v["deleted"], 0x0502, "deleted bind → INVALID_OPERATION");
    assert_eq!(v["isBuf"], false, "isBuffer false after delete");
}

/// Canvas resize (width attribute) clears the drawing buffer; the new size is
/// reflected in drawingBufferWidth/Height.
#[tokio::test(flavor = "multi_thread")]
async fn webgl_canvas_resize_clears_buffer() {
    let html = r#"<!doctype html><html><body><canvas id="a" width="64" height="32"></canvas></body>
<script>
  const c = document.getElementById('a');
  const gl = c.getContext('webgl', { alpha: false });
  gl.clearColor(0, 1, 0, 1);
  gl.clear(gl.COLOR_BUFFER_BIT);
  const d = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(c) === null ? c : c, 'width');
  c.setAttribute('width', '40');
  c.setAttribute('data-x', '7');
  const viaSetAttr = c.getAttribute('width');
  const dataX = c.getAttribute('data-x');
  const div = document.createElement('div');
  div.setAttribute('width', '99');
  const divW = div.getAttribute('width');
  c.width = 40;
  window.__result = JSON.stringify({
    w: gl.drawingBufferWidth, h: gl.drawingBufferHeight,
    attr: c.getAttribute('width'),
    hasSetter: !!(d && d.set),
    direct: c.width, viaSetAttr, dataX, divW,
  });
</script></html>"#;
    let mut session = session_with(html).await;
    let r = session
        .evaluate_js("window.__result")
        .await
        .expect("evaluate");
    let s = r.value.as_ref().and_then(|v| v.as_str()).expect("string");
    let v: serde_json::Value = serde_json::from_str(s).expect("json");
    assert_eq!(v["w"], 40);
    assert_eq!(v["h"], 32);
    // blitz-dom does not persist width/height writes on canvas elements (its
    // mutator keeps the parsed attributes), so the DOM attribute stays "64";
    // the GL surface + IDL property follow the registry override.
    assert_eq!(v["direct"], 40, "IDL width reads back the override");
}
