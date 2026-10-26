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

use base64::Engine as _;
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
    session_with_config(html, config).await
}

async fn session_with_config(html: &str, config: BrowserConfig) -> Session {
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

fn with_draw_probe(html: String) -> String {
    html.replace("<script>", "<script>try {")
        .replace(
            "gl.drawArrays(gl.TRIANGLES, 0, 3);",
            "gl.drawArrays(gl.TRIANGLES, 0, 3);\n\
             const pixel = new Uint8Array(4);\n\
             gl.readPixels(16,16,1,1,gl.RGBA,gl.UNSIGNED_BYTE,pixel);\n\
             globalThis.drawProbe = [...pixel, gl.getError()];",
        )
        .replace(
            "</script>",
            "} catch(error) { globalThis.drawError = String(error); }</script>",
        )
}

#[tokio::test]
async fn webgl_disabled_webgl2_falls_back_to_real_webgl1_without_pinning_canvas() {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    config.webgl2_enabled = false;
    let html = TRIANGLE_HTML.replace(
        "const gl = canvas.getContext('webgl', { alpha: false });",
        "for (const type of ['webgl2', 'experimental-webgl2', 'unknown']) {\n\
           if (canvas.getContext(type) !== null) throw new Error('unexpected context: ' + type);\n\
         }\n\
         const gl = canvas.getContext('webgl', { alpha: false });\n\
         if (gl.TEXTURE_BINDING_3D !== undefined) throw new Error('WebGL2 constants');",
    );
    let mut session = session_with_config(&with_draw_probe(html), config).await;
    let result = session
        .evaluate_js("[globalThis.drawError ?? null, globalThis.drawProbe]")
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(serde_json::json!([null, [255, 0, 0, 255, 0]])),
        "{result:?}"
    );

    // Capability selection belongs to each runtime's JS thread.
    let mut default_session = session_with("<canvas id='default'></canvas>").await;
    let result = default_session
        .evaluate_js("!!document.getElementById('default').getContext('webgl2')")
        .await
        .unwrap();
    assert_eq!(result.value, Some(serde_json::json!(true)), "{result:?}");
}

#[tokio::test]
async fn webgl_shader_precision_capability_queries_have_valid_enums() {
    let mut session = session_with("<canvas id='gl'></canvas>").await;
    let result=session.evaluate_js(r#"(() => {
      const gl=document.getElementById('gl').getContext('webgl');
      const formats=['LOW_FLOAT','MEDIUM_FLOAT','HIGH_FLOAT','LOW_INT','MEDIUM_INT','HIGH_INT'];
      const results=formats.map(name=>({name,value:gl[name],
        shaders:[gl.VERTEX_SHADER,gl.FRAGMENT_SHADER].map(shader=>{
          const result=gl.getShaderPrecisionFormat(shader,gl[name]);
          return result&&{precision:result.precision,rangeMin:result.rangeMin,rangeMax:result.rangeMax};
        })}));
      const error=gl.getError();
      globalThis.__precisionProbe=JSON.stringify({results,error});
      return results.every(result=>typeof result.value==='number'&&result.shaders.every(format=>
        format&&Number.isFinite(format.precision)&&Number.isFinite(format.rangeMin)&&Number.isFinite(format.rangeMax)))&&error===gl.NO_ERROR;
    })()"#).await.unwrap();
    let probe = session
        .evaluate_js("globalThis.__precisionProbe")
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(serde_json::json!(true)),
        "{result:?}; {probe:?}"
    );
}

#[tokio::test]
async fn webgl_requestable_derivatives_compile_and_render() {
    let html = TRIANGLE_HTML
        .replace(
            "if (!gl) { throw new Error('no webgl context'); }",
            "if (!gl) throw new Error('no webgl context');\n\
             if (!gl.getSupportedExtensions().includes('OES_standard_derivatives')) throw new Error('derivatives not advertised');\n\
             const extension = gl.getExtension('OES_standard_derivatives');\n\
             if (!extension || extension.FRAGMENT_SHADER_DERIVATIVE_HINT_OES !== 0x8B8B) throw new Error('derivatives not enabled');\n\
             if (gl.getExtension('OES_vertex_array_object') !== null || gl.getSupportedExtensions().includes('OES_vertex_array_object')) throw new Error('unimplemented extension advertised');",
        )
        .replace(
            "attribute vec2 a_pos; void main() { gl_Position = vec4(a_pos, 0.0, 1.0); }",
            "attribute vec2 a_pos; varying vec2 v_pos; void main() { v_pos = a_pos; gl_Position = vec4(a_pos, 0.0, 1.0); }",
        )
        .replace(
            "precision mediump float; void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }",
            "#extension GL_OES_standard_derivatives : require\\nprecision mediump float; varying vec2 v_pos; void main() { gl_FragColor = vec4(abs(dFdx(v_pos.x)) * 32.0, 0.0, 0.0, 1.0); }",
        );
    let mut session = session_with(&with_draw_probe(html)).await;
    let result = session
        .evaluate_js("[globalThis.drawError ?? null, globalThis.drawProbe]")
        .await
        .unwrap();
    assert_eq!(
        result.value,
        Some(serde_json::json!([null, [255, 0, 0, 255, 0]])),
        "{result:?}"
    );
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
    let mut session = session_with(&with_draw_probe(TRIANGLE_HTML.to_owned())).await;
    let probe = session
        .evaluate_js("[globalThis.drawError ?? null, globalThis.drawProbe]")
        .await
        .unwrap();
    assert_eq!(
        probe.value,
        Some(serde_json::json!([null, [255, 0, 0, 255, 0]])),
        "before presentation: {probe:?}"
    );
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

/// M5 — Three.js fixture: vendored r128 drives our WebGL1 surface through a
/// deterministic scene — 2×2 DataTexture quad, an embedded glTF triangle
/// (GLTFLoader.parse), fixed-step animation, and a real dispatched mousemove
/// that steers the camera. Pixel assertions run on the session screenshot.
#[tokio::test(flavor = "multi_thread")]
async fn webgl_threejs_fixture() {
    let mut session = session_with(
        "<!doctype html><html><head><style>body{margin:0}canvas{width:128px;height:128px;display:block}</style></head><body><canvas id=\"c\" width=\"128\" height=\"128\"></canvas></body></html>",
    )
    .await;

    let three = std::fs::read_to_string("tests/fixtures/three.min.js").expect("vendored three.js");
    let r = session.evaluate_js(&three).await.expect("evaluate three");
    assert!(r.exception.is_none(), "three.js load: {:?}", r.exception);
    let loader =
        std::fs::read_to_string("tests/fixtures/GLTFLoader.js").expect("vendored GLTFLoader");
    let r = session.evaluate_js(&loader).await.expect("evaluate loader");
    assert!(r.exception.is_none(), "GLTFLoader load: {:?}", r.exception);

    let glb = std::fs::read_to_string("tests/fixtures/minimal.glb.b64").expect("glTF fixture");
    let glb_bytes = base64::engine::general_purpose::STANDARD
        .decode(glb.trim())
        .expect("decode glTF fixture");
    let script = format!(
        r#"
  (async function () {{
    const canvas = document.getElementById('c');
    const ctx = canvas.getContext('webgl', {{ alpha: false, antialias: false }});
    if (!ctx) throw new Error('no webgl');
    const renderer = new THREE.WebGLRenderer({{ canvas, context: ctx, antialias: false }});
    renderer.setSize(128, 128, false);
    const scene = new THREE.Scene();
    scene.background = new THREE.Color(0x203040);
    const camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.1, 10);
    camera.position.x = 0;
    camera.position.y = 0;
    camera.position.z = 3;

    // 2×2 procedural DataTexture on a fullscreen quad (nearest filtering).
    const px = new Uint8Array([
      255, 0, 0, 255, 0, 255, 0, 255,
      0, 0, 255, 255, 255, 255, 255, 255,
    ]);
    const tex = new THREE.DataTexture(px, 2, 2, THREE.RGBAFormat);
    tex.needsUpdate = true;
    const quad = new THREE.Mesh(
      new THREE.PlaneGeometry(2, 2),
      new THREE.MeshBasicMaterial({{ map: tex, side: THREE.DoubleSide }}),
    );
    scene.add(quad);

    // Real dispatched mousemove steers the camera (user-input reproduction).
    window.__inputDx = 0;
    canvas.addEventListener('mousemove', (e) => {{
      window.__inputDx = e.clientX;
      camera.position.x = (e.clientX - 64) * 0.01;
    }});

    // Embedded glTF through GLTFLoader.parse — the binary GLB includes
    // positions and indices; the PBR material is overridden for deterministic pixels.
    const gltfBinary = new Uint8Array({glb_bytes:?}).buffer;
    const gltfPromise = new Promise((res, rej) => {{
      new THREE.GLTFLoader().parse(
        gltfBinary,
        '',
        (g) => {{
          const root = g.scene;
          root.position.z = 0.1; // draw in front of the fullscreen texture quad
          let mesh = null;
          root.traverse((o) => {{
            if (o.isMesh) {{
              o.material = new THREE.MeshBasicMaterial({{ color: 0x00ff00, side: THREE.DoubleSide }});
              mesh = o;
            }}
          }});
          if (!mesh) return rej(new Error('glTF had no mesh'));
          scene.add(root);
          res(mesh);
        }},
        (e) => rej(new Error('gltf: ' + e)),
      );
    }});
    const gltfMesh = await gltfPromise;

    renderer.render(scene, camera);
    // Expose handles for the later animation/input steps.
    window.__stage = {{ renderer, scene, camera, gltfMesh }};
    window.__stageError = null;
  }})().catch((e) => {{
    window.__stageError = String((e && e.message) || e);
  }});
  JSON.stringify({{ started: true }})
"#,
        glb_bytes = glb_bytes
    );
    let r = session.evaluate_js(&script).await.expect("evaluate scene");
    assert!(r.exception.is_none(), "scene script: {:?}", r.exception);

    // GLTFLoader's callback settles asynchronously; wait until the IIFE
    // signals readiness (bounded).
    let mut ready = false;
    for _ in 0..50 {
        let r = session
            .evaluate_js(
                "JSON.stringify({s: window.__stage ? 'ready' : (window.__stageError || 'pending')})",
            )
            .await
            .expect("poll");
        let s = r
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if s.contains("\"ready\"") {
            ready = true;
            break;
        }
        assert!(s.contains("\"pending\""), "scene failed: {s}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "scene never became ready");

    // Screenshot pixel assertions. Canvas 128×128 at page origin; the quad
    // stretches the 2×2 texture into quadrants (nearest filtering).
    let png = session.capture_screenshot_png(256).await.expect("capture");
    let img = image::load_from_memory(&png).expect("decode");
    let rgba = img.to_rgba8();
    let at = |x: u32, y: u32| -> [u8; 3] {
        let p = rgba.get_pixel(x, y);
        [p[0], p[1], p[2]]
    };
    // Texture quadrants (GL (x,y) → bitmap (x, 127-y)):
    assert_eq!(at(16, 111), [255, 0, 0], "tex red quadrant"); // GL (-0.75,-0.75)
    assert_eq!(at(111, 111), [0, 255, 0], "tex green quadrant"); // GL (0.75,-0.75)
    assert_eq!(at(16, 16), [0, 0, 255], "tex blue quadrant"); // GL (-0.75,0.75)
    assert_eq!(at(111, 16), [255, 255, 255], "tex white quadrant");
    // glTF triangle interior — green on top of the red quadrant.
    let tri = at(48, 79); // GL (-0.25,-0.25)
    assert_eq!(tri, [0, 255, 0], "glTF triangle (green)");
    // Background outside the quad? The quad covers everything; check the
    // page margin instead (outside canvas = white page background).
    let margin = at(200, 4);
    assert!(
        margin[0] > 240 && margin[1] > 240,
        "page margin should be white, got {margin:?}"
    );

    // Fixed-step animation: rotate the glTF mesh 180° in 4 fixed steps and
    // re-render each step (deterministic animation reproduction).
    let anim = r#"
  const gltfMesh = window.__stage.gltfMesh;
  const renderer = window.__stage.renderer;
  const scene = window.__stage.scene;
  const camera = window.__stage.camera;
  for (let i = 0; i < 4; i++) {
    gltfMesh.rotation.z += Math.PI / 4;
    renderer.render(scene, camera);
  }
  gltfMesh.rotation.z = Math.PI;
  renderer.render(scene, camera);
  JSON.stringify({ rot: gltfMesh.rotation.z })
"#;
    let r = session.evaluate_js(anim).await.expect("evaluate anim");
    let s = r.value.as_ref().and_then(|v| v.as_str()).expect("json");
    let v: serde_json::Value = serde_json::from_str(s).expect("json");
    let rot = v["rot"].as_f64().unwrap_or(0.0);
    assert!(
        (rot - std::f64::consts::PI).abs() < 1e-6,
        "fixed-step animation state, got {rot}"
    );
    // After 180° the green triangle now covers GL (0.25,0.25) → bitmap (80,48);
    // the old interior spot is quad-red again.
    let png = session
        .capture_screenshot_png(256)
        .await
        .expect("capture 2");
    let img = image::load_from_memory(&png).expect("decode 2");
    let rgba = img.to_rgba8();
    let at = |x: u32, y: u32| -> [u8; 3] {
        let p = rgba.get_pixel(x, y);
        [p[0], p[1], p[2]]
    };
    let tri2 = at(80, 48); // GL (0.25,0.25)
    assert_eq!(tri2, [0, 255, 0], "rotated glTF triangle (green)");
    let old = at(48, 79);
    assert_eq!(old, [255, 0, 0], "previous spot back to red texel");
    assert_eq!(at(48, 16), [0, 0, 255], "blue texel before input");

    // User input: real dispatched mousemove at canvas x=100 (dx=+36) shifts
    // the camera +0.36 — the texture quadrant boundaries shift left.
    let hover_js = crate::js::mouse::js_move_mouse(100.0, 64.0);
    let r = session.evaluate_js(&hover_js).await.expect("hover eval");
    assert!(r.exception.is_none(), "hover: {:?}", r.exception);
    let check = r#"
  window.__stage.renderer.render(window.__stage.scene, window.__stage.camera);
  JSON.stringify({ inputDx: window.__inputDx, camX: window.__stage.camera.position.x })
"#;
    let r = session.evaluate_js(check).await.expect("eval check");
    let s = r
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("check value {:?}, exception {:?}", r.value, r.exception));
    let v: serde_json::Value = serde_json::from_str(s).expect("json");
    assert_eq!(v["inputDx"], 100, "mousemove clientX reached the page");
    let camx = v["camX"].as_f64().unwrap_or(0.0);
    assert!(
        (camx - 0.36).abs() < 1e-9,
        "camera moved by input, got {camx}"
    );
    let png = session
        .capture_screenshot_png(256)
        .await
        .expect("capture after input");
    let img = image::load_from_memory(&png)
        .expect("decode after input")
        .to_rgba8();
    let pixel = img.get_pixel(48, 16);
    assert_eq!(
        [pixel[0], pixel[1], pixel[2]],
        [255, 255, 255],
        "mousemove shifts the textured scene"
    );
}
