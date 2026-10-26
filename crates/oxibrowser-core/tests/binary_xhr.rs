//! Babylon-style listener-only XHR on the real local HTTP bridge.

use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::{Value, json};
use std::time::Duration;
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
        .evaluate_js_with_await(expression, true)
        .await
        .unwrap();
    assert!(!result.timed_out, "{result:?}");
    assert!(result.exception.is_none(), "{result:?}");
    serde_json::from_str(
        result
            .value
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("JSON result: {result:?}")),
    )
    .unwrap_or_else(|error| panic!("JSON parse: {result:?}: {error}"))
}

#[tokio::test]
async fn listener_only_binary_xhr_preserves_bytes_metadata_and_event_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/binary"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![0, 128, 255, 65], "model/gltf-binary")
                .insert_header("x-asset", "yes")
                .insert_header("set-cookie", "hidden=value"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut session = session().await;
    let url = serde_json::to_string(&format!("{}/binary", server.uri())).unwrap();
    let result = evaluate(&mut session, &format!(r#"
        new Promise((resolve, reject) => {{
            const xhr = new XMLHttpRequest(), events = [], targets = [];
            const capture = function(event) {{
                events.push(event.type === 'readystatechange' ? 'rs:'+xhr.readyState : event.type);
                targets.push(event.target === xhr && event.currentTarget === xhr && this === xhr);
            }};
            for (const type of ['readystatechange','loadstart','progress','load','loadend']) xhr.addEventListener(type,capture);
            xhr.addEventListener('error',reject);
            let removed = 0, once = 0;
            const remove = () => removed++;
            xhr.addEventListener('readystatechange',remove); xhr.removeEventListener('readystatechange',remove);
            xhr.addEventListener('readystatechange',() => once++,{{once:true}});
            xhr.onload = () => events.push('onload');
            xhr.addEventListener('loadend',() => resolve(JSON.stringify({{
                bytes:Array.from(new Uint8Array(xhr.response)), buffer:xhr.response instanceof ArrayBuffer,
                identity:xhr.response === xhr.response, status:xhr.status, statusText:xhr.statusText,
                url:xhr.responseURL, mime:xhr.getResponseHeader('CONTENT-TYPE'), asset:xhr.getResponseHeader('x-asset'),
                cookie:xhr.getResponseHeader('set-cookie'), allHeaders:xhr.getAllResponseHeaders().includes('set-cookie'),
                events, targets, removed, once, type:xhr.responseType, done:xhr.readyState === XMLHttpRequest.DONE
            }})));
            xhr.open('GET',{url}); xhr.responseType='arraybuffer';
            if(xhr.response !== null) reject(Error('Premature binary response'));
            xhr.setRequestHeader('X-Test','one'); xhr.setRequestHeader('x-test','two');
            xhr.setRequestHeader('Cookie','override=blocked'); xhr.send();
        }})
    "#)).await;
    assert_eq!(
        result,
        json!({
            "bytes":[0,128,255,65], "buffer":true, "identity":true,
            "status":200, "statusText":"OK", "url":format!("{}/binary",server.uri()),
            "mime":"model/gltf-binary", "asset":"yes", "cookie":null, "allHeaders":false,
            "events":["rs:1","loadstart","rs:2","rs:3","progress","rs:4","onload","load","loadend"],
            "targets":[true,true,true,true,true,true,true,true], "removed":0, "once":1, "type":"arraybuffer", "done":true
        })
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("x-test").unwrap().to_str().unwrap(),
        "one, two"
    );
    assert!(requests[0].headers.get("cookie").is_none());
    session.close().await.unwrap();
    server.verify().await;
}

#[tokio::test]
async fn synchronous_binary_xhr_settles_before_send_returns() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/binary"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0, 128, 255]))
        .mount(&server)
        .await;
    let mut session = session().await;
    let url = serde_json::to_string(&format!("{}/binary", server.uri())).unwrap();
    let result = evaluate(
        &mut session,
        &format!(
            r#"
        (() => {{
            const xhr=new XMLHttpRequest(), events=[];
            xhr.open('GET',{url},false); xhr.responseType='arraybuffer';
            xhr.addEventListener('readystatechange',()=>events.push('rs:'+xhr.readyState));
            xhr.addEventListener('load',()=>events.push('load'));
            xhr.addEventListener('loadend',()=>events.push('loadend'));
            xhr.send();
            return JSON.stringify([xhr.readyState,xhr.status,xhr.response instanceof ArrayBuffer,
                Array.from(new Uint8Array(xhr.response)),events]);
        }})()
    "#
        ),
    )
    .await;
    assert_eq!(
        result,
        json!([
            4,
            200,
            true,
            [0, 128, 255],
            ["rs:2", "rs:3", "rs:4", "load", "loadend"]
        ])
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn xhr_relative_url_and_response_type_change_at_headers_received() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html></html>", "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/binary"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0, 128, 255]))
        .mount(&server)
        .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let result = evaluate(
        &mut session,
        r#"
        new Promise((resolve,reject)=>{
            const xhr=new window.XMLHttpRequest();
            xhr.addEventListener('readystatechange',()=>{
                if(xhr.readyState===2) xhr.responseType='arraybuffer';
            });
            xhr.addEventListener('loadend',()=>resolve(JSON.stringify([
                window.XMLHttpRequest===globalThis.XMLHttpRequest,
                xhr.responseType,xhr.response instanceof ArrayBuffer,
                Array.from(new Uint8Array(xhr.response))
            ])));
            xhr.addEventListener('error',reject);
            xhr.open('GET','/binary');xhr.send();
        })
    "#,
    )
    .await;
    assert_eq!(result, json!([true, "arraybuffer", true, [0, 128, 255]]));
    session.close().await.unwrap();
}

#[tokio::test]
async fn xhr_loadstart_reopen_sends_replacement_post_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/replacement"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;
    let mut session = session().await;
    let base = serde_json::to_string(&server.uri()).unwrap();
    let result = evaluate(
        &mut session,
        &format!(
            r#"
        new Promise((resolve,reject)=>{{
            const xhr=new XMLHttpRequest();
            xhr.addEventListener('loadstart',()=>{{
                xhr.open('POST',{base}+'/replacement');xhr.send('replacement');
            }},{{once:true}});
            xhr.addEventListener('error',reject);
            xhr.addEventListener('loadend',()=>resolve(JSON.stringify(xhr.responseText)));
            xhr.open('POST',{base}+'/original');xhr.send('original');
        }})
    "#
        ),
    )
    .await;
    assert_eq!(result, json!("ok"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/replacement");
    assert_eq!(requests[0].body, b"replacement");
    session.close().await.unwrap();
    server.verify().await;
}

#[tokio::test]
async fn xhr_network_error_and_http_error_have_distinct_terminal_events() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
        .mount(&server)
        .await;
    let mut session = session().await;
    let url = serde_json::to_string(&format!("{}/missing", server.uri())).unwrap();
    let script = format!(
        r#"
        new Promise(resolve => {{
            const xhr=new XMLHttpRequest(),events=[];
            xhr.open('GET',{url}); xhr.responseType='arraybuffer';
            for(const type of ['readystatechange','load','error','loadend']) xhr.addEventListener(type,()=>events.push(type));
            xhr.addEventListener('loadend',()=>resolve(JSON.stringify([xhr.status,xhr.statusText,
                xhr.response===null,xhr.response instanceof ArrayBuffer,events])));
            xhr.send();
        }})
    "#
    );
    assert_eq!(
        evaluate(&mut session, &script).await,
        json!([
            404,
            "Not Found",
            false,
            true,
            [
                "readystatechange",
                "readystatechange",
                "readystatechange",
                "load",
                "loadend"
            ]
        ])
    );
    session.set_offline(true);
    assert_eq!(
        evaluate(&mut session, &script).await,
        json!([0, "", true, false, ["readystatechange", "error", "loadend"]])
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn xhr_abort_ignores_late_response_and_reopen_keeps_new_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(vec![0, 128, 255])
                .set_delay(Duration::from_millis(150)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/next"))
        .respond_with(ResponseTemplate::new(200).set_body_string("next"))
        .mount(&server)
        .await;
    let mut session = session().await;
    let base = serde_json::to_string(&server.uri()).unwrap();
    evaluate(&mut session,&format!(r#"
        (() => {{
            globalThis.abortXhr=new XMLHttpRequest();globalThis.abortEvents=[];
            const xhr=abortXhr,events=abortEvents;
            xhr.open('GET',{base}+'/slow'); xhr.responseType='arraybuffer';
            for(const type of ['load','error','abort','loadend']) xhr.addEventListener(type,()=>events.push(type));
            xhr.send(); xhr.abort();
            return JSON.stringify(true);
        }})()
    "#)).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let result = evaluate(
        &mut session,
        "JSON.stringify([abortXhr.readyState,abortXhr.status,abortXhr.response,abortEvents])",
    )
    .await;
    assert_eq!(result, json!([0, 0, null, ["abort", "loadend"]]));
    evaluate(
        &mut session,
        &format!(
            r#"
        (()=>{{
            globalThis.reuseXhr=new XMLHttpRequest();globalThis.reuseLoaded=[];
            const reuse=reuseXhr;
            reuse.addEventListener('loadend',()=>reuseLoaded.push(reuse.responseText));
            reuse.open('GET',{base}+'/slow'); reuse.send();
            reuse.open('GET',{base}+'/next'); reuse.send();
            return JSON.stringify(true);
        }})()
    "#
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let result = evaluate(
        &mut session,
        "JSON.stringify([reuseLoaded,reuseXhr.responseText])",
    )
    .await;
    assert_eq!(result, json!([["next"], "next"]));
    session.close().await.unwrap();
}
