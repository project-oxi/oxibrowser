#![cfg(feature = "webgl")]

use oxibrowser_core::{Browser, BrowserConfig, session::Session};

#[tokio::test]
async fn indexed_draw_offsets_are_bytes_for_each_index_type() {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate("data:text/html,<body></body>")
        .await
        .unwrap();
    let result = session.evaluate_js(r#"JSON.stringify((() => {
        const results = [];
        for (const version of ['webgl','webgl2']) {
            const canvas = document.createElement('canvas'); canvas.width=16; canvas.height=16;
            const gl = canvas.getContext(version, {alpha:false,preserveDrawingBuffer:true});
            if (!gl) throw new Error('missing ' + version);
            if(version==='webgl' && !gl.getExtension('OES_element_index_uint')) throw new Error('missing uint indices');
            const shader=(type,source)=>{const shader=gl.createShader(type);gl.shaderSource(shader,source);gl.compileShader(shader);if(!gl.getShaderParameter(shader,gl.COMPILE_STATUS))throw new Error(gl.getShaderInfoLog(shader));return shader;};
            const program=gl.createProgram();
            gl.attachShader(program,shader(gl.VERTEX_SHADER,version==='webgl2'?'#version 300 es\nin vec2 p;void main(){gl_Position=vec4(p,0,1);}':'attribute vec2 p;void main(){gl_Position=vec4(p,0,1);}'));
            gl.attachShader(program,shader(gl.FRAGMENT_SHADER,version==='webgl2'?'#version 300 es\nprecision mediump float;out vec4 color;void main(){color=vec4(1,0,0,1);}':'precision mediump float;void main(){gl_FragColor=vec4(1,0,0,1);}'));
            gl.bindAttribLocation(program,0,'p');gl.linkProgram(program);
            if(!gl.getProgramParameter(program,gl.LINK_STATUS))throw new Error(gl.getProgramInfoLog(program));
            gl.useProgram(program);gl.bindBuffer(gl.ARRAY_BUFFER,gl.createBuffer());
            gl.bufferData(gl.ARRAY_BUFFER,new Float32Array([-1,-1,1,-1,-1,1]),gl.STATIC_DRAW);
            gl.enableVertexAttribArray(0);gl.vertexAttribPointer(0,2,gl.FLOAT,false,0,0);gl.viewport(0,0,16,16);
            for (const [ArrayType,type,sentinel] of [[Uint8Array,gl.UNSIGNED_BYTE,255],[Uint16Array,gl.UNSIGNED_SHORT,65535],[Uint32Array,gl.UNSIGNED_INT,4294967295]]) {
                gl.bindBuffer(gl.ELEMENT_ARRAY_BUFFER,gl.createBuffer());
                const indices=new ArrayType([sentinel,sentinel,0,1,2]);gl.bufferData(gl.ELEMENT_ARRAY_BUFFER,indices,gl.STATIC_DRAW);
                for(const instanced of version==='webgl2'?[false,true]:[false]) {
                    gl.clearColor(0,0,1,1);gl.clear(gl.COLOR_BUFFER_BIT);
                    const offset=2*ArrayType.BYTES_PER_ELEMENT;
                    if(instanced)gl.drawElementsInstanced(gl.TRIANGLES,3,type,offset,1);else gl.drawElements(gl.TRIANGLES,3,type,offset);
                    const pixel=new Uint8Array(4);gl.readPixels(2,2,1,1,gl.RGBA,gl.UNSIGNED_BYTE,pixel);
                    results.push({version,type,instanced,pixel:Array.from(pixel),error:gl.getError()});
                }
            }
        }
        return results;
    })())"#).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    let results: Vec<serde_json::Value> = serde_json::from_str(
        result
            .value
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(results.len(), 9);
    for result in results {
        assert_eq!(result["error"], 0, "{result}");
        assert_eq!(
            result["pixel"],
            serde_json::json!([255, 0, 0, 255]),
            "{result}"
        );
    }
}
