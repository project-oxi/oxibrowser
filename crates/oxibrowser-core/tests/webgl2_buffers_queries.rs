#![cfg(feature = "webgl")]
//! Genuine GLES3 UBO draws, query results, ownership and native reflection.

use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::{Value, json};

async fn session() -> Session {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session.navigate("data:text/html,<html><body><canvas id='gl' width='32' height='32'></canvas><canvas id='other' width='32' height='32'></canvas></body></html>").await.unwrap();
    session
}

async fn evaluate(session: &mut Session, source: &str) -> Value {
    let result = session.evaluate_js(source).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    let text = result.value.as_ref().and_then(Value::as_str).unwrap();
    serde_json::from_str(text).unwrap()
}

const DRAW_SETUP: &str = r#"
    var gl=document.getElementById('gl').getContext('webgl2',{alpha:false,preserveDrawingBuffer:true});
    if(!gl)throw Error('No WebGL2');
    function shader(type,source){const s=gl.createShader(type);gl.shaderSource(s,source);gl.compileShader(s);if(!gl.getShaderParameter(s,gl.COMPILE_STATUS))throw Error(gl.getShaderInfoLog(s));return s;}
    var program=gl.createProgram();
    gl.attachShader(program,shader(gl.VERTEX_SHADER,'#version 300 es\nlayout(location=0) in vec2 position;void main(){gl_Position=vec4(position,0.0,1.0);}'));
    gl.attachShader(program,shader(gl.FRAGMENT_SHADER,'#version 300 es\nprecision highp float;layout(std140) uniform Tint {vec4 color;};out vec4 outputColor;void main(){outputColor=color;}'));
    gl.linkProgram(program);if(!gl.getProgramParameter(program,gl.LINK_STATUS))throw Error(gl.getProgramInfoLog(program));gl.useProgram(program);
    var vertex=gl.createBuffer();gl.bindBuffer(gl.ARRAY_BUFFER,vertex);
    gl.bufferData(gl.ARRAY_BUFFER,new Float32Array([-1,-1,1,-1,-1,1]),gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);gl.vertexAttribPointer(0,2,gl.FLOAT,false,0,0);
    var ubo=gl.createBuffer();gl.bindBuffer(gl.UNIFORM_BUFFER,ubo);
    if(gl.getParameter(gl.UNIFORM_BUFFER_BINDING)!==ubo)throw Error('Generic UBO wrapper identity lost before indexed binding');
    gl.bufferData(gl.UNIFORM_BUFFER,new Float32Array([1,0,0,1]),gl.DYNAMIC_DRAW);
    var block=gl.getUniformBlockIndex(program,'Tint');
    if(block===gl.INVALID_INDEX)throw Error('Active UBO missing');
    gl.uniformBlockBinding(program,block,2);gl.bindBufferBase(gl.UNIFORM_BUFFER,2,ubo);
    gl.uniformBlockBinding(program,-1,2);
    if(gl.getError()!==gl.INVALID_VALUE)throw Error('Negative uniform block index accepted');
    gl.uniformBlockBinding(program,block,-1);
    if(gl.getError()!==gl.INVALID_VALUE)throw Error('Negative uniform block binding accepted');
    gl.uniformBlockBinding(program,2**32+block,2**32+2);
    if(gl.getError()!==gl.NO_ERROR)throw Error('Unsigned uniform block parameters did not wrap');
    gl.viewport(0,0,32,32);
    function pixel(){const p=new Uint8Array(4);gl.readPixels(8,8,1,1,gl.RGBA,gl.UNSIGNED_BYTE,p);return Array.from(p);}
"#;

#[tokio::test]
async fn webgl2_uniform_buffers_drive_native_gles3_pixels_and_reflection() {
    let mut session = session().await;
    let result=evaluate(&mut session,&format!(r#"{DRAW_SETUP}
      (()=>{{
        gl.drawArrays(gl.TRIANGLES,0,3);const red=pixel();
        const generic=gl.getParameter(gl.UNIFORM_BUFFER_BINDING)===ubo;
        const indexed=gl.getIndexedParameter(gl.UNIFORM_BUFFER_BINDING,2)===ubo;
        const size=gl.getIndexedParameter(gl.UNIFORM_BUFFER_SIZE,2),start=gl.getIndexedParameter(gl.UNIFORM_BUFFER_START,2);
        gl.bufferSubData(gl.UNIFORM_BUFFER,0,new Float32Array([0,1,0,1]));
        gl.bindBuffer(gl.UNIFORM_BUFFER,null);
        const sizeAfterUnbind=gl.getIndexedParameter(gl.UNIFORM_BUFFER_SIZE,2);
        const unbound=gl.getParameter(gl.UNIFORM_BUFFER_BINDING)===null;
        const vertexStillBound=gl.getParameter(gl.ARRAY_BUFFER_BINDING)!==null;
        gl.drawArrays(gl.TRIANGLES,0,3);const green=pixel();
        gl.deleteBuffer(ubo);const deleted=gl.getIndexedParameter(gl.UNIFORM_BUFFER_BINDING,2)===null;
        const limits=[gl.MAX_VERTEX_UNIFORM_BLOCKS,gl.MAX_FRAGMENT_UNIFORM_BLOCKS,gl.MAX_COMBINED_UNIFORM_BLOCKS,
          gl.MAX_UNIFORM_BUFFER_BINDINGS,gl.MAX_UNIFORM_BLOCK_SIZE,gl.UNIFORM_BUFFER_OFFSET_ALIGNMENT].map(p=>gl.getParameter(p));
        const missing=gl.getUniformBlockIndex(program,'NotDeclared')===0xffffffff;
        const depth=gl.createTexture();gl.bindTexture(gl.TEXTURE_2D,depth);
        gl.texImage2D(gl.TEXTURE_2D,0,gl.DEPTH_COMPONENT24,4,4,0,gl.DEPTH_COMPONENT,gl.UNSIGNED_INT,null);
        gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_COMPARE_MODE,gl.COMPARE_REF_TO_TEXTURE);
        gl.texParameteri(gl.TEXTURE_2D,gl.TEXTURE_COMPARE_FUNC,gl.LEQUAL);
        const comparison=gl.getTexParameter(gl.TEXTURE_2D,gl.TEXTURE_COMPARE_MODE)===gl.COMPARE_REF_TO_TEXTURE;
        return JSON.stringify({{red,green,generic,indexed,size,start,sizeAfterUnbind,unbound,deleted,vertexStillBound,missing,comparison,
          limits:limits.every(x=>typeof x==='number'&&x>0),error:gl.getError()}});
      }})()"#)).await;
    assert_eq!(
        result,
        json!({"red":[255,0,0,255],"green":[0,255,0,255],"generic":true,"indexed":true,"size":16,"start":0,"sizeAfterUnbind":16,
        "unbound":true,"deleted":true,"vertexStillBound":true,"missing":true,"comparison":true,"limits":true,"error":0})
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn webgl2_occlusion_queries_have_real_lifecycle_numeric_results_and_identity() {
    let mut session = session().await;
    let started=evaluate(&mut session,&format!(r#"{DRAW_SETUP}
      (()=>{{
        globalThis.query=gl.createQuery();
        gl.beginQuery(gl.ANY_SAMPLES_PASSED,query);
        const current=gl.getQuery(gl.ANY_SAMPLES_PASSED,gl.CURRENT_QUERY)===query;
        const active=gl.getQueryParameter(query,gl.QUERY_RESULT)===null;
        const activeError=gl.getError();
        gl.drawArrays(gl.TRIANGLES,0,3);gl.endQuery(gl.ANY_SAMPLES_PASSED);gl.finish();
        return JSON.stringify([current,active,activeError,gl.getQuery(gl.ANY_SAMPLES_PASSED,gl.CURRENT_QUERY)===null,gl.isQuery(query),gl.getError()]);
      }})()"#)).await;
    assert_eq!(started, json!([true, true, 0x0502, true, true, 0]));
    // A separate evaluation returns control to the host between ending and
    // reading the asynchronous GPU query. gl.finish never fabricates a result.
    let result=evaluate(&mut session,r#"(()=>{
        const available=gl.getQueryParameter(query,gl.QUERY_RESULT_AVAILABLE);
        const result=gl.getQueryParameter(query,gl.QUERY_RESULT);
        gl.beginQuery(gl.TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN,query);const retarget=gl.getError();
        gl.deleteQuery(query);const deleted=!gl.isQuery(query);
        gl.deleteQuery(query);const repeated=gl.getError();
        const invalid=gl.getQueryParameter(query,gl.QUERY_RESULT)===null,error=gl.getError();
        return JSON.stringify([typeof available,available,typeof result,result,retarget,deleted,repeated,invalid,error]);
    })()"#).await;
    assert_eq!(
        result,
        json!(["boolean", true, "number", 1, 0x0502, true, 0, true, 0x0502])
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn webgl2_buffers_and_queries_reject_foreign_objects_and_invalid_parameters() {
    let mut session = session().await;
    let result=evaluate(&mut session,r#"(()=>{
        const a=document.getElementById('gl').getContext('webgl2');
        const b=document.getElementById('other').getContext('webgl2');
        const query=a.createQuery(),buffer=a.createBuffer(),own=b.createBuffer();
        b.beginQuery(b.ANY_SAMPLES_PASSED,query);const beginError=b.getError();
        const foreign=b.getQueryParameter(query,b.QUERY_RESULT)===null,queryError=b.getError();
        b.deleteQuery(query);const deleteError=b.getError();
        b.bindBufferBase(b.UNIFORM_BUFFER,0,buffer);const bufferError=b.getError();
        b.bindBufferBase(b.UNIFORM_BUFFER,b.getParameter(b.MAX_UNIFORM_BUFFER_BINDINGS),own);const indexError=b.getError();
        b.bindBufferBase(b.UNIFORM_BUFFER,0,own);
        b.bindBufferBase(b.UNIFORM_BUFFER,-1,null);const negativeBind=b.getError();
        const zeroUnchanged=b.getIndexedParameter(b.UNIFORM_BUFFER_BINDING,0)===own;
        const negativeRead=b.getIndexedParameter(b.UNIFORM_BUFFER_BINDING,-1)===null,negativeReadError=b.getError();
        let coercionRead=false;
        const indexObject={valueOf(){coercionRead=b.getIndexedParameter(b.UNIFORM_BUFFER_BINDING,0)===own;return -1;}};
        b.bindBufferBase(b.UNIFORM_BUFFER,indexObject,null);const coercionError=b.getError();
        const coercionUnchanged=b.getIndexedParameter(b.UNIFORM_BUFFER_BINDING,0)===own;
        b.bindBufferBase(b.UNIFORM_BUFFER,2**32+1,own);
        const wrapped=b.getIndexedParameter(b.UNIFORM_BUFFER_BINDING,1)===own&&b.getIndexedParameter(b.UNIFORM_BUFFER_BINDING,2**32+1)===own;
        const wrappedError=b.getError();
        const badPname=b.getQuery(b.ANY_SAMPLES_PASSED,b.QUERY_RESULT)===null,pnameError=b.getError();
        const badTarget=b.getIndexedParameter(b.TEXTURE_2D,0)===null,targetError=b.getError();
        const first=document.createElement('canvas').getContext('webgl');
        const hidden=['createQuery','deleteQuery','bindBufferBase','getUniformBlockIndex'].every(name=>first[name]===undefined);
        return JSON.stringify([beginError,foreign,queryError,deleteError,bufferError,indexError,negativeBind,zeroUnchanged,negativeRead,negativeReadError,coercionRead,coercionError,coercionUnchanged,wrapped,wrappedError,badPname,pnameError,badTarget,targetError,hidden]);
    })()"#).await;
    assert_eq!(
        result,
        json!([
            0x0502, true, 0x0502, 0x0502, 0x0502, 0x0501, 0x0501, true, true, 0x0501, true, 0x0501,
            true, true, 0, true, 0x0500, true, 0x0500, true
        ])
    );
    session.close().await.unwrap();
}

/// Import the unchanged production runtime for its Babylon UBO side effects.
/// This fixture constructs an Engine; it never calls the game's boot function.
#[tokio::test]
#[ignore = "manual retained assets via OXI_CHEONGGYE_ASSETS; requires ANGLE GPU"]
async fn retained_babylon_engine_keeps_genuine_webgl2() {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let root = PathBuf::from(
        std::env::var_os("OXI_CHEONGGYE_ASSETS")
            .expect("set OXI_CHEONGGYE_ASSETS to the retained assets directory"),
    );
    assert!(root.join("runtime-BBlrYGBF.js").is_file());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/assets/[A-Za-z0-9_.-]+\.js$"))
        .respond_with(move |request: &Request| {
            let name = request.url.path().strip_prefix("/assets/").unwrap();
            match std::fs::read(root.join(name)) {
                Ok(bytes) => ResponseTemplate::new(200).set_body_raw(bytes, "text/javascript"),
                Err(_) => ResponseTemplate::new(404),
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"<!doctype html><html><body>
          <canvas id="gl" width="32" height="32"></canvas><pre id="state">pending</pre>
          <script type="module">
          const report=value=>document.getElementById('state').textContent=JSON.stringify(value);
          try {
            await import('/assets/runtime-BBlrYGBF.js');
            const {r:Engine}=await import('/assets/KHR_gaussian_splatting.pure-DbCgQG4U.js');
            const engine=new Engine(document.getElementById('gl'),false,{preserveDrawingBuffer:true,stencil:true});
            const started=performance.now();
            const effect=engine.createEffect({vertexSource:'attribute vec2 position;void main(){gl_Position=vec4(position,0.0,1.0);}',
              fragmentSource:'precision highp float;void main(){gl_FragColor=vec4(1.0,0.0,0.0,1.0);}'},
              {attributes:['position'],uniformsNames:[],samplers:[],defines:'',
                onError:(_,message)=>report({status:'error',message})});
            effect.executeWhenCompiled(()=>{
              report({status:'done',version:engine.webGLVersion,ubo:engine.supportsUniformBuffers,
              disabled:engine.disableUniformBuffers,macChrome:/Mac OS.+Chrome/.test(navigator.userAgent),
              parallel:!!engine.getCaps().parallelShaderCompile,asyncPipeline:effect.getPipelineContext().isAsync,
              compiled:effect.isReady(),callbackMs:performance.now()-started,error:engine._gl.getError()});
              engine.dispose();
            });
          } catch(error) {report({status:'error',message:String(error),stack:error.stack});}
          </script></body></html>"#, "text/html"))
        .mount(&server).await;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    config.background_event_loop = true;
    config.nav_script_timeout_ms = 30_000;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let result = loop {
        if let Some(snapshot) = session.dom_snapshot().await.unwrap()
            && let Some(node) = snapshot.query_selector("#state")
            && let Some(text) = snapshot.text_content(node)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "retained Engine module graph stalled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(result["status"], "done", "{result}");
    let mut normalized = result;
    let callback_ms = normalized
        .as_object_mut()
        .unwrap()
        .remove("callbackMs")
        .unwrap();
    assert!(callback_ms.as_f64().is_some_and(|elapsed| elapsed >= 0.0));
    eprintln!("retained Babylon synthetic Effect callback in {callback_ms}ms");
    assert_eq!(
        normalized,
        // Babylon 9.31's unchanged ExceptionsList disables UBOs for this
        // browser's default Mac Chrome UA. Native UBO draws are tested above.
        json!({"status":"done","version":2,"ubo":false,"disabled":true,"macChrome":true,
            "parallel":true,"asyncPipeline":true,"compiled":true,"error":0})
    );
    session.close().await.unwrap();
}
