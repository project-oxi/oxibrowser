//! Browser module loading through the real session/network/JS bridge.

use oxibrowser_core::js::CoreEvent;
use oxibrowser_core::session::Session;
use oxibrowser_core::{Browser, BrowserConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn serve(server: &MockServer, route: &str, mime: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes(), mime))
        .mount(server)
        .await;
}

async fn js(server: &MockServer, route: &str, body: &str) {
    serve(server, route, "text/javascript; charset=utf-8", body).await;
}

async fn session() -> Session {
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    let browser = Browser::new(config.clone()).await.unwrap();
    Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap()
}

async fn value(session: &mut Session, expression: &str) -> Value {
    let result = session
        .evaluate_js(&format!("JSON.stringify({expression})"))
        .await
        .unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    serde_json::from_str(
        result
            .value
            .as_ref()
            .and_then(Value::as_str)
            .expect("JSON result"),
    )
    .unwrap()
}

fn observe_errors(session: &mut Session) -> std::sync::mpsc::Receiver<CoreEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    session.set_event_sink(tx);
    rx
}

fn assert_no_exceptions(events: &std::sync::mpsc::Receiver<CoreEvent>) {
    let errors: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            CoreEvent::Exception { message, stack, .. } => Some((message, stack)),
            _ => None,
        })
        .collect();
    assert!(errors.is_empty(), "module exceptions: {errors:?}");
}

#[tokio::test]
async fn module_graph_live_bindings_cycles_and_document_cache() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        r#"
        <script type="module" src="/app/main.js"></script>
        <script type="module" src="/app/main.js"></script>
        <script>globalThis.order = ['classic']; globalThis.runs = 0;</script>
        <script type="module">const privateName = 1; globalThis.inlineA = privateName;</script>
        <script type="module">const privateName = 2; globalThis.inlineB = privateName;</script>
    "#,
    )
    .await;
    js(&server, "/app/main.js", r#"
        import { count, inc } from './shared.js';
        import { answer } from './reexport.js';
        import { getA } from './a.js';
        globalThis.order.push('module'); inc();
        globalThis.graph = [count, answer, getA(), import.meta.url, import.meta.resolve('../lazy.js')];
    "#).await;
    js(
        &server,
        "/app/shared.js",
        "globalThis.runs++; export let count=1; export function inc(){count++;}",
    )
    .await;
    js(
        &server,
        "/app/reexport.js",
        "export { count as answer } from './shared.js';",
    )
    .await;
    js(&server, "/app/a.js", "import { getB } from './b.js'; export function getA(){return 'a'+getB();} export const A='a';").await;
    js(
        &server,
        "/app/b.js",
        "import { A } from './a.js'; export function getB(){return A+'b';}",
    )
    .await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    let uri = server.uri();
    for _ in 0..2 {
        session.navigate(&format!("{uri}/page")).await.unwrap();
        assert_no_exceptions(&events);
        assert_eq!(
            value(
                &mut session,
                "[order,runs,graph,inlineA,inlineB,typeof privateName]"
            )
            .await,
            json!([
                ["classic", "module"],
                1,
                [
                    2,
                    2,
                    "aab",
                    format!("{uri}/app/main.js"),
                    format!("{uri}/lazy.js")
                ],
                1,
                2,
                "undefined"
            ])
        );
    }
    let requests = server.received_requests().await.unwrap();
    // Each module record runs/fetches once per document, including repeated roots.
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/app/shared.js")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/app/main.js")
            .count(),
        2
    );
}

#[tokio::test]
async fn module_dynamic_import_top_level_await_and_unrelated_fetch() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        r#"
        <script>
          fetch('/message').then(r=>r.text()).then(t=>globalThis.message=t);
          import('./lazy.js').then(m=>globalThis.inlineClassic=m.default);
        </script>
        <script type="module" src="/app/main.js"></script>
    "#,
    )
    .await;
    serve(&server, "/message", "text/plain", "preserved").await;
    js(
        &server,
        "/app/main.js",
        r#"
        await new Promise(resolve=>setTimeout(resolve,10));
        const mod = await import('../lazy.js?version=1#one');
        globalThis.dynamic = [mod.default, import.meta.url];
    "#,
    )
    .await;
    js(&server, "/lazy.js", "export default 42;").await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "[dynamic,message,inlineClassic]").await,
        json!([
            [42, format!("{}/app/main.js", server.uri())],
            "preserved",
            42
        ])
    );
    // Dynamic import from an evaluated classic script falls back to its realm's URL.
    session
        .evaluate_js("import('./lazy.js').then(m=>globalThis.fromClassic=m.default)")
        .await
        .unwrap();
    assert_eq!(value(&mut session, "fromClassic").await, json!(42));
}

#[tokio::test]
async fn module_failures_are_reported_and_do_not_abort_siblings() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        r#"
        <script type="module" src="/bad-mime.js"></script>
        <script type="module">import './missing.js';</script>
        <script type="module">import 'unmapped-package';</script>
        <script type="module">await Promise.reject(new Error('module rejected'));</script>
        <script type="module">export const broken = ;</script>
        <script type="module">globalThis.survived = true;</script>
    "#,
    )
    .await;
    serve(
        &server,
        "/bad-mime.js",
        "text/html",
        "globalThis.mustNotRun = true;",
    )
    .await;
    let mut session = session().await;
    let (tx, rx) = std::sync::mpsc::channel();
    session.set_event_sink(tx);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(
        value(&mut session, "[survived,typeof mustNotRun]").await,
        json!([true, "undefined"])
    );
    let errors: Vec<_> = rx
        .try_iter()
        .filter_map(|event| match event {
            CoreEvent::Exception { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    for expected in [
        "JavaScript MIME",
        "HTTP 404",
        "unmapped bare",
        "module rejected",
        "syntax",
    ] {
        assert!(
            errors.iter().any(|e| e.contains(expected)),
            "missing {expected}: {errors:?}"
        );
    }
}

#[tokio::test]
async fn module_dynamic_failures_reject_promises_and_remain_cached() {
    let server = MockServer::start().await;
    serve(&server,"/page","text/html",r#"<script type="module">
        globalThis.failures=[];
        for (let i=0;i<2;i++) {try {await import('./missing.js')} catch(e) {failures.push(e.message)}}
        try {await import('./lazy.js',{with:{type:'json'}})} catch(e) {failures.push(e.message)}
        globalThis.done=true;
    </script>"#).await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(
        value(&mut session, "[done,failures.length,failures[2]]").await,
        json!([true, 3, "module import attributes are not supported"])
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/missing.js")
            .count(),
        1
    );
}

#[tokio::test]
async fn module_imports_obey_offline_mode() {
    let server = MockServer::start().await;
    serve(&server, "/page", "text/html", "<p>modules</p>").await;
    js(&server, "/offline.js", "globalThis.offlineRan=true;").await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    session.set_offline(true);
    session
        .evaluate_js("import('./offline.js').catch(e=>globalThis.offlineError=e.message)")
        .await
        .unwrap();
    assert!(
        value(&mut session, "offlineError")
            .await
            .as_str()
            .unwrap()
            .contains("offline")
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/offline.js")
            .count(),
        0
    );
}

#[tokio::test]
async fn module_redirects_resolve_dependencies_against_response_url() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        r#"<script type="module" src="/redirect.js"></script>"#,
    )
    .await;
    Mock::given(path("/redirect.js"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/app/target.js"))
        .mount(&server)
        .await;
    js(
        &server,
        "/app/target.js",
        "import {value} from './dep.js'; globalThis.redirectResult=[value,import.meta.url];",
    )
    .await;
    js(&server, "/app/dep.js", "export const value=9;").await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "redirectResult").await,
        json!([9, format!("{}/app/target.js", server.uri())])
    );
}

#[tokio::test]
async fn module_localization_uses_live_dataset_attributes() {
    let server = MockServer::start().await;
    serve(&server,"/page","text/html",r#"
        <p id="label" data-i18n="welcome" data-long-name="first">Loading</p>
        <script type="module">
          const element=document.querySelector('[data-i18n]');
          const dataset=element.dataset;
          element.textContent=({welcome:'Ready'})[dataset.i18n];
          element.setAttribute('data-long-name','second');
          dataset.enabled=true;
          dataset['foo-Bar']=null;
          delete dataset.i18n;
          const link=document.createElement('link'); link.href='./style.css';
          const image=document.createElement('img'); image.src='./image.png';
          globalThis.urlsResult=[link.getAttribute('href'),link.href,image.getAttribute('src'),image.src];
          globalThis.defaultViewMatches=element.ownerDocument.defaultView===window;
          globalThis.attributesResult=[element.textContent,dataset.longName,element.getAttribute('data-enabled'),
            element.getAttribute('data-foo--bar'),dataset===element.dataset,'i18n' in dataset,
            Object.keys(dataset).sort(),document.nodeType,defaultViewMatches];
        </script>
    "#).await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "attributesResult").await,
        json!([
            "Ready",
            "second",
            "true",
            "null",
            true,
            false,
            ["enabled", "foo-Bar", "longName"],
            9,
            true
        ])
    );
    assert_eq!(
        value(&mut session, "urlsResult").await,
        json!([
            "./style.css",
            format!("{}/style.css", server.uri()),
            "./image.png",
            format!("{}/image.png", server.uri())
        ])
    );
}

#[tokio::test]
async fn module_and_classic_defer_scripts_keep_document_order() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        r#"
        <script type="module">globalThis.order.push('module');</script>
        <script defer src="/deferred.js"></script>
        <script>globalThis.order=['blocking'];</script>
        <script type="module">globalThis.order.push('last');</script>
    "#,
    )
    .await;
    js(&server, "/deferred.js", "order.push('deferred');").await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "order").await,
        json!(["blocking", "module", "deferred", "last"])
    );
}

#[tokio::test]
async fn module_cross_origin_requires_explicit_credentialed_cors() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        &format!(
            r#"<script type="module">
      globalThis.denied=false;
      try {{await import('{}/denied.js')}} catch(e) {{globalThis.denied=e.message.includes('CORS')}}
      const allowed=await import('{}/allowed.js');
      globalThis.corsResult=[denied,allowed.default];
    </script>"#,
            other.uri(),
            other.uri()
        ),
    )
    .await;
    js(&other, "/denied.js", "globalThis.deniedRan=true;").await;
    Mock::given(path("/allowed.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("export default 7;", "text/javascript")
                .insert_header("access-control-allow-origin", server.uri())
                .insert_header("access-control-allow-credentials", "true"),
        )
        .mount(&other)
        .await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "[corsResult,typeof deniedRan]").await,
        json!([[true, 7], "undefined"])
    );
}

#[tokio::test]
async fn module_fetch_respects_short_evaluation_deadline() {
    use oxibrowser_core::js::{JsRuntime, JsRuntimeConfig};
    use std::time::{Duration, Instant};
    let mut runtime = JsRuntime::with_config(JsRuntimeConfig::default());
    let (tx, rx) = std::sync::mpsc::channel();
    let (responses, response_rx) = std::sync::mpsc::channel();
    runtime.set_fetch_channel(tx, response_rx);
    runtime
        .set_document("<p>Ready</p>", Some("https://example.com/page"), (320, 240))
        .await
        .unwrap();
    // Keep the response sender alive but deliberately never answer a request.
    let start = Instant::now();
    let result = runtime
        .evaluate_with_timeout(
            "import('./stalled.js').catch(e=>globalThis.moduleTimeout=e.message)",
            Some(100),
        )
        .await;
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "module ignored evaluation deadline"
    );
    match result {
        Err(oxibrowser_core::error::CoreError::JsTimeout(100)) => {}
        Ok(_) => {
            let message = runtime.evaluate("globalThis.moduleTimeout").await.unwrap();
            assert!(
                message
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .contains("timed out"),
                "{message:?}"
            );
        }
        other => panic!("unexpected deadline result: {other:?}"),
    }
    assert_eq!(runtime.evaluate("1").await.unwrap().value, Some(json!(1)));
    assert_eq!(rx.try_recv().unwrap().url, "https://example.com/stalled.js");
    drop(responses);
}

#[tokio::test]
async fn module_game_ui_clones_detached_art_and_restores_inline_style() {
    let server = MockServer::start().await;
    serve(&server,"/page","text/html",r#"
      <section id="game"><svg class="entry"><g id="clip"><path clip-path="url(#clip)"/></g></svg></section>
      <script type="module">
        const root=document.getElementById('game');
        const art=root.querySelector('svg');
        const clone=art.cloneNode(true);
        for(const node of clone.querySelectorAll('[id]')) node.id='travel-'+node.id;
        for(const node of clone.querySelectorAll('[clip-path]')) node.setAttribute('clip-path','url(#travel-clip)');
        globalThis.cloneResult=[clone.matches('svg.entry'),clone.querySelectorAll('g > path').length,
          clone.querySelector('g').id,art.querySelector('g').id,clone.hasAttribute('class'),art.closest('section').id];
        root.append(clone);
        const style=root.style;
        style.setProperty('--viewport-width','640px','important');
        const priority=style.getPropertyPriority('--viewport-width');
        style.setProperty('--viewport-width','320px');
        const updated=document.getElementById('game').style.getPropertyValue('--viewport-width');
        const removed=style.removeProperty('--viewport-width');
        style.cssText='width: 2px; --saved: yes !important';
        style.width='invalid';
        globalThis.styleResult=[style===root.style,priority,updated,removed,style.getPropertyValue('--viewport-width'),
          style.width,style.getPropertyPriority('--saved')];
        style.width='4px';
        globalThis.styleWidth=style.width;
      </script>
    "#).await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "cloneResult").await,
        json!([true, 1, "travel-clip", "clip", true, "game"])
    );
    assert_eq!(
        value(&mut session, "styleResult").await,
        json!([true, "important", "320px", "320px", "", "2px", "important"])
    );
    assert_eq!(value(&mut session, "styleWidth").await, json!("4px"));
}

#[tokio::test]
async fn module_errors_keep_native_source_locations() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        r#"<script type="module" src="/throws.js"></script>"#,
    )
    .await;
    js(
        &server,
        "/throws.js",
        "function fail(){throw new Error('module trace');} fail();",
    )
    .await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let stacks: Vec<_> = events
        .try_iter()
        .filter_map(|event| match event {
            CoreEvent::Exception { stack, .. } => stack,
            _ => None,
        })
        .collect();
    assert!(
        stacks
            .iter()
            .any(|stack| stack.contains("/throws.js:") && stack.contains("fail")),
        "{stacks:?}"
    );
}

#[tokio::test]
async fn module_error_stacks_survive_repeated_document_url_updates() {
    let mut runtime = oxibrowser_core::js::JsRuntime::new();
    runtime.evaluate("globalThis.originalStackGetter=Object.getOwnPropertyDescriptor(Error.prototype,'stack').get;globalThis.originalCrypto=crypto").await.unwrap();
    for i in 0..120 {
        runtime.set_page_url(&format!("https://example.com/page/{i}"));
    }
    let result=runtime.evaluate("Object.getOwnPropertyDescriptor(Error.prototype,'stack').get === originalStackGetter && new Error('after navigation').stack.includes('after navigation') && crypto === originalCrypto && crypto.getRandomValues(new Uint8Array(64)).some(value=>value!==0)").await.unwrap();
    assert_eq!(result.value, Some(json!(true)), "{result:?}");
}

#[tokio::test]
async fn module_abort_errors_use_dom_exception_names_and_legacy_codes() {
    let mut runtime = oxibrowser_core::js::JsRuntime::new();
    let result=runtime.evaluate("JSON.stringify([new DOMException('cancelled','AbortError').name,new DOMException('cancelled','AbortError').code,new DOMException('invalid','SyntaxError').code,new DOMException('obsolete','DOMStringSizeError').code,new DOMException().message,new DOMException().name])").await.unwrap();
    assert_eq!(
        result.value,
        Some(json!("[\"AbortError\",20,12,0,\"\",\"Error\"]")),
        "{result:?}"
    );
}

#[tokio::test]
async fn module_asset_urls_resolve_bases_and_keep_query_parameters_live() {
    let server = MockServer::start().await;
    serve(&server,"/page","text/html",r#"<script type="module">
      const base=new URL('/dist/map/',location.href);
      const manifest=new URL('manifest.json?key=one&key=two',base);
      const params=manifest.searchParams;
      params.set('sha256','a b+é');params.set('key','three');
      globalThis.manifestUrl=manifest.href;
      manifest.search='?next=1';params.append('next','2');
      globalThis.queryState=[params===manifest.searchParams,params.getAll('next'),String(params),manifest.pathname,manifest.host,
        new URL('../mesh.glb',base).pathname];
      const pairs=new URLSearchParams([['a','1'],['a','2'],['x','a b']]);
      globalThis.pairsState=[pairs.getAll('a'),pairs.size,[...pairs],String(pairs),pairs instanceof URLSearchParams,manifest instanceof URL];
      try {new URL('relative')} catch(e) {globalThis.badUrl=e instanceof TypeError}
    </script>"#).await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "manifestUrl").await,
        json!(format!(
            "{}/dist/map/manifest.json?key=three&sha256=a+b%2B%C3%A9",
            server.uri()
        ))
    );
    assert_eq!(
        value(&mut session, "queryState").await,
        json!([
            true,
            ["1", "2"],
            "next=1&next=2",
            "/dist/map/manifest.json",
            server.address().to_string(),
            "/dist/mesh.glb"
        ])
    );
    assert_eq!(
        value(&mut session, "pairsState").await,
        json!([
            ["1", "2"],
            3,
            [["a", "1"], ["a", "2"], ["x", "a b"]],
            "a=1&a=2&x=a+b",
            true,
            true
        ])
    );
    assert_eq!(value(&mut session, "badUrl").await, json!(true));
}

#[tokio::test]
async fn module_asset_integrity_hashes_exact_bytes_and_view_ranges() {
    let server = MockServer::start().await;
    serve(&server,"/page","text/html",r#"<script type="module">
      const hex=buffer=>Array.from(new Uint8Array(buffer),b=>b.toString(16).padStart(2,'0')).join('');
      const response=await fetch('./asset');const data=await response.arrayBuffer();
      const bytes=new Uint8Array([0,97,98,99,255]);
      globalThis.hashes=[hex(await crypto.subtle.digest('SHA-256',data)),
        hex(await crypto.subtle.digest({name:'sha-256'},bytes.subarray(1,4))),
        hex(await crypto.subtle.digest('SHA-256',new DataView(bytes.buffer,1,3))),
        hex(await crypto.subtle.digest('SHA-384',data)),hex(await crypto.subtle.digest('SHA-512',data)),window.crypto===crypto,new TextDecoder().decode(data),
        new TextEncoder().encode('abc') instanceof Uint8Array,
        hex(await crypto.subtle.digest('SHA-256',new TextEncoder().encode('abc'))),
        new TextDecoder().decode(new TextEncoder().encode('청계천 🌿')),
        new TextEncoder().encode().length];
      try{await crypto.subtle.digest('unsupported',data)}catch(e){globalThis.hashError=e.name}
      try{await crypto.subtle.digest('SHA-256',[1,2,3])}catch(e){globalThis.sourceError=e.name}
    </script>"#).await;
    serve(&server, "/asset", "application/octet-stream", "abc").await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    let sha256 = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(
        value(&mut session, "hashes").await,
        json!([
            sha256,
            sha256,
            sha256,
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            true,
            "abc",
            true,
            sha256,
            "청계천 🌿",
            0
        ])
    );
    assert_eq!(
        value(&mut session, "[hashError,sourceError]").await,
        json!(["NotSupportedError", "TypeError"])
    );
}

#[tokio::test]
async fn module_viewport_uses_window_functions_and_reflected_element_state() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/page",
        "text/html",
        r#"
      <section id="game" style="width:320px;height:240px" hidden></section>
      <script type="module">
        const root=document.getElementById('game');
        root.hidden=false;
        const probe=document.createElement('span');root.append(probe);
        const view=probe.ownerDocument.defaultView;
        const computed=view.getComputedStyle(probe);
        const button=document.createElement('button');button.type='button';
        button.disabled=true;button.title='Continue';root.append(button);
        globalThis.viewportResult=[window===self,view===window&&devicePixelRatio===window.devicePixelRatio,typeof view.requestAnimationFrame,
          typeof view.cancelAnimationFrame,typeof computed.getPropertyValue,
          root.clientWidth>0,root.clientHeight>0,root.hasAttribute('hidden'),
          button.getAttribute('type'),button.hasAttribute('disabled'),button.getAttribute('title')];
      </script>
    "#,
    )
    .await;
    let mut session = session().await;
    let events = observe_errors(&mut session);
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_no_exceptions(&events);
    assert_eq!(
        value(&mut session, "viewportResult").await,
        json!([
            true, true, "function", "function", "function", true, true, false, "button", true,
            "Continue"
        ])
    );
}

#[tokio::test]
async fn abort_signal_throws_exact_reason_and_default_dom_exception() {
    let mut runtime = oxibrowser_core::js::JsRuntime::new();
    let result = runtime.evaluate(r#"JSON.stringify((() => {
      const controller = new AbortController(), reason = {cancelled:true};
      const untouched = controller.signal.throwIfAborted() === undefined;
      controller.abort(reason); controller.abort('replacement');
      let identity = false, primitive = false, brand = false;
      try { controller.signal.throwIfAborted(); } catch (error) { identity = error === reason; }
      const second = new AbortController(); second.abort(42);
      try { second.signal.throwIfAborted(); } catch (error) { primitive = error === 42; }
      const standard = new AbortController(); standard.abort();
      try { AbortSignal.prototype.throwIfAborted.call({}); } catch (error) { brand = error instanceof TypeError; }
      return [untouched,identity,primitive,brand,standard.signal.reason instanceof DOMException,
        standard.signal.reason.name,standard.signal.reason.code,window.AbortSignal === AbortSignal];
    })())"#).await.unwrap();
    assert_eq!(
        result.value,
        Some(json!("[true,true,true,true,true,\"AbortError\",20,true]")),
        "{result:?}"
    );
}
