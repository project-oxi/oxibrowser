#![cfg(feature = "webgl")]
//! Native KHR completion polling in both WebGL versions; no simulated status.

use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

async fn evaluate(session: &mut Session, source: &str) -> Value {
    let result = session.evaluate_js(source).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    serde_json::from_str(result.value.as_ref().and_then(Value::as_str).unwrap()).unwrap()
}

#[tokio::test]
async fn khr_parallel_compilation_polls_real_status_then_draws_in_both_webgl_versions() {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session.navigate("data:text/html,<html><body><canvas id='first' width='32' height='32'></canvas><canvas id='second' width='32' height='32'></canvas></body></html>").await.unwrap();
    let setup = evaluate(&mut session,r#"(()=>{
      globalThis.jobs=['webgl','webgl2'].map((kind,index)=>{
        const gl=document.getElementById(index?'second':'first').getContext(kind,{alpha:false,preserveDrawingBuffer:true});
        if(!gl)throw Error('No '+kind);
        const program=gl.createProgram();
        const beforeShader=gl.createShader(gl.VERTEX_SHADER);
        const before=gl.getProgramParameter(program,0x91B1)===null&&gl.getError()===gl.INVALID_ENUM
          &&gl.getShaderParameter(beforeShader,0x91B1)===null&&gl.getError()===gl.INVALID_ENUM;
        gl.deleteShader(beforeShader);
        const supported=gl.getSupportedExtensions().includes('KHR_parallel_shader_compile');
        const ext=gl.getExtension('KHR_parallel_shader_compile');
        if(!ext)throw Error('ANGLE KHR extension unavailable');
        const contract=ext.COMPLETION_STATUS_KHR===0x91B1&&gl.getExtension('KHR_parallel_shader_compile')===ext
          &&Object.keys(ext).length===1&&ext.maxShaderCompilerThreadsKHR===undefined
          &&ext.MAX_SHADER_COMPILER_THREADS_KHR===undefined;
        function shader(type,source){const s=gl.createShader(type);gl.shaderSource(s,source);gl.compileShader(s);return s;}
        const vertex=shader(gl.VERTEX_SHADER,index?'#version 300 es\nlayout(location=0) in vec2 position;void main(){gl_Position=vec4(position,0,1);}'
          :'attribute vec2 position;void main(){gl_Position=vec4(position,0,1);}');
        const fragment=shader(gl.FRAGMENT_SHADER,index?'#version 300 es\nprecision highp float;out vec4 color;void main(){color=vec4(1,0,0,1);}'
          :'precision highp float;void main(){gl_FragColor=vec4(1,0,0,1);}');
        gl.attachShader(program,vertex);gl.attachShader(program,fragment);gl.bindAttribLocation(program,0,'position');gl.linkProgram(program);
        const first=gl.getProgramParameter(program,ext.COMPLETION_STATUS_KHR);
        const bad=shader(gl.FRAGMENT_SHADER,'this is intentionally invalid shader source');
        return {gl,program,vertex,fragment,bad,ext,before,supported,contract,first};
      });
      return JSON.stringify(jobs.map(j=>({before:j.before,supported:j.supported,contract:j.contract,statusType:typeof j.first,error:j.gl.getError()})));
    })()"#).await;
    assert_eq!(
        setup,
        json!([
            {"before":true,"supported":true,"contract":true,"statusType":"boolean","error":0},
            {"before":true,"supported":true,"contract":true,"statusType":"boolean","error":0}
        ])
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let completed=evaluate(&mut session,r#"JSON.stringify(jobs.map(j=>j.gl.getProgramParameter(j.program,j.ext.COMPLETION_STATUS_KHR)
          &&j.gl.getShaderParameter(j.vertex,j.ext.COMPLETION_STATUS_KHR)
          &&j.gl.getShaderParameter(j.fragment,j.ext.COMPLETION_STATUS_KHR)
          &&j.gl.getShaderParameter(j.bad,j.ext.COMPLETION_STATUS_KHR)))"#).await;
        if completed == json!([true, true]) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native shader completion stalled: {completed}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let result=evaluate(&mut session,r#"JSON.stringify(jobs.map(j=>{
      const {gl,program,bad}=j;
      const linked=gl.getProgramParameter(program,gl.LINK_STATUS);
      const invalid=!gl.getShaderParameter(bad,gl.COMPILE_STATUS)&&gl.getShaderInfoLog(bad).length>0;
      gl.useProgram(program);const buffer=gl.createBuffer();gl.bindBuffer(gl.ARRAY_BUFFER,buffer);
      gl.bufferData(gl.ARRAY_BUFFER,new Float32Array([-1,-1,1,-1,-1,1]),gl.STATIC_DRAW);
      gl.vertexAttribPointer(0,2,gl.FLOAT,false,0,0);gl.enableVertexAttribArray(0);gl.viewport(0,0,32,32);gl.drawArrays(gl.TRIANGLES,0,3);
      const pixel=new Uint8Array(4);gl.readPixels(8,8,1,1,gl.RGBA,gl.UNSIGNED_BYTE,pixel);
      return {linked,invalid,pixel:Array.from(pixel),error:gl.getError()};
    }))"#).await;
    assert_eq!(
        result,
        json!([
            {"linked":true,"invalid":true,"pixel":[255,0,0,255],"error":0},
            {"linked":true,"invalid":true,"pixel":[255,0,0,255],"error":0}
        ])
    );
    session.close().await.unwrap();
}
