#![cfg(feature = "webgl")]

use oxibrowser_core::{Browser, BrowserConfig, session::Session};
use serde_json::json;

#[tokio::test]
async fn float_and_integer_uniforms_use_only_the_typed_array_view() {
    let config = BrowserConfig::headless();
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate("data:text/html,<body></body>")
        .await
        .unwrap();
    let result = session
        .evaluate_js(
            r#"JSON.stringify((() => {
                const canvas = document.createElement('canvas');
                canvas.width = canvas.height = 4;
                const gl = canvas.getContext('webgl2', { preserveDrawingBuffer: true });
                const compile = (kind, source) => {
                    const shader = gl.createShader(kind);
                    gl.shaderSource(shader, source);
                    gl.compileShader(shader);
                    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
                        throw new Error(gl.getShaderInfoLog(shader));
                    }
                    return shader;
                };
                const program = gl.createProgram();
                gl.attachShader(program, compile(gl.VERTEX_SHADER,
                    '#version 300 es\nvoid main() { gl_Position = vec4(0, 0, 0, 1); gl_PointSize = 4.0; }'));
                gl.attachShader(program, compile(gl.FRAGMENT_SHADER,
                    '#version 300 es\nprecision highp float; uniform vec4 rgba; uniform ivec2 offset; out vec4 color; void main() { color = rgba + vec4(vec2(offset) / 255.0, 0, 0); }'));
                gl.linkProgram(program);
                if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
                    throw new Error(gl.getProgramInfoLog(program));
                }
                gl.useProgram(program);
                gl.viewport(0, 0, 4, 4);
                const rgba = gl.getUniformLocation(program, 'rgba');
                const offset = gl.getUniformLocation(program, 'offset');
                const floatData = new Float32Array([99, 32/255, 64/255, 128/255, 1, 99]);
                const intData = new Int32Array([999, -16, 32, 999]);
                const read = (values, offsets) => {
                    gl.uniform4fv(rgba, values);
                    gl.uniform2iv(offset, offsets);
                    gl.drawArrays(gl.POINTS, 0, 1);
                    const pixel = new Uint8Array(4);
                    gl.readPixels(1, 1, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
                    return { pixel: Array.from(pixel), error: gl.getError() };
                };
                const views = read(floatData.subarray(1, 5), intData.subarray(1, 3));
                const sequences = read([32/255, 64/255, 128/255, 1], [-16, 32]);
                return { views, sequences };
            })())"#,
        )
        .await
        .unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    let pixels: serde_json::Value =
        serde_json::from_str(result.value.as_ref().unwrap().as_str().unwrap()).unwrap();
    let expected = json!({"pixel": [16, 96, 128, 255], "error": 0});
    assert_eq!(pixels["views"], expected);
    assert_eq!(pixels["sequences"], expected);
    session.close().await.unwrap();
}
