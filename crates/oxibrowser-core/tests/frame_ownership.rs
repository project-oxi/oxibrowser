//! Frame access is bound to a live native owner, never URL or sibling position.
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
async fn mount(server: &MockServer, route: &str, html: &str) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html, "text/html"))
        .mount(server)
        .await;
}
async fn evaluate(session: &mut Session, source: &str) -> Value {
    let result = session.evaluate_js(source).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    result.value.unwrap_or(Value::Null)
}
async fn value(session: &mut Session, expression: &str) -> Value {
    let encoded = evaluate(session, &format!("JSON.stringify({expression})")).await;
    serde_json::from_str(encoded.as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn denied_child_navigation_never_fetches_or_replaces_the_top_document() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/top",
        "<body><p id='top'>preserved</p><iframe src='/child'></iframe></body>",
    )
    .await;
    mount(&server, "/child", "<body><a id='link' href='/escape'>leave</a><form action='/escape'><button id='submit'>submit</button></form></body>").await;
    mount(&server, "/escape", "<body>must not load</body>").await;
    let mut session = session().await;
    let top_url = format!("{}/top", server.uri());
    session.navigate(&top_url).await.unwrap();
    let (frame, child) = session
        .frame_context_map()
        .read()
        .iter()
        .next()
        .map(|(frame, context)| (frame.clone(), *context))
        .unwrap();
    let utility = session
        .create_isolated_world(&frame, "navigation-guard")
        .await
        .unwrap();
    for context in [child, utility] {
        let result = session.evaluate_js_in_context(
            "JSON.stringify([()=>{location.href='/escape'},()=>location.replace('/escape'),()=>location.reload(),()=>document.getElementById('link').click(),()=>document.getElementById('submit').click()].map(action=>{try{action();return 'allowed'}catch(error){return error.name}}))",
            context, false,
        ).await.unwrap();
        assert!(result.exception.is_none(), "{result:?}");
        let names: Value =
            serde_json::from_str(result.value.as_ref().unwrap().as_str().unwrap()).unwrap();
        assert_eq!(names, json!(vec!["NotSupportedError"; 5]));
        // Flush Session effects as the CDP bridge does after a child operation.
        assert_eq!(
            value(&mut session, "document.getElementById('top').textContent").await,
            json!("preserved")
        );
        assert_eq!(session.current_url().unwrap().as_str(), top_url);
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "denied navigation must perform no extra I/O"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.url.path() != "/escape")
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn parent_reads_the_same_live_child_document_as_child_context() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/frames.html",
        include_str!("../../../tests/spa/fixtures/frames.html"),
    )
    .await;
    mount(
        &server,
        "/child.html",
        include_str!("../../../tests/spa/fixtures/child.html"),
    )
    .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/frames.html", server.uri()))
        .await
        .unwrap();
    let context = *session.frame_context_map().read().values().next().unwrap();
    let child=session.evaluate_js_in_context("document.getElementById('inc').click();document.getElementById('inc').click();document.getElementById('ccount').textContent",context,false).await.unwrap();
    assert!(child.exception.is_none(), "{child:?}");
    assert_eq!(child.value, Some(json!("clicks:2")));
    assert_eq!(value(&mut session,"(() => {const f=document.getElementById('frame');return [f.contentDocument.getElementById('ccount').textContent,f.contentWindow.document===f.contentDocument,f.contentDocument.getElementById('ccount')===f.contentDocument.querySelector('#ccount')]})()").await,json!(["clicks:2",true,true]));
    evaluate(&mut session, "document.getElementById('read').click()").await;
    assert_eq!(
        value(
            &mut session,
            "document.getElementById('parent-read').textContent"
        )
        .await,
        json!("clicks:2")
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn identical_src_and_failed_sibling_keep_their_distinct_native_owners() {
    let server = MockServer::start().await;
    mount(&server,"/page","<html><body><iframe id='broken' src='http://127.0.0.1:1/unreachable'></iframe><iframe id='first' src='/child'></iframe><iframe id='second' src='/child'></iframe></body></html>").await;
    mount(
        &server,
        "/child",
        "<html><body><p id='value'>initial</p></body></html>",
    )
    .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let children = session
        .page()
        .unwrap()
        .root_frame()
        .children()
        .iter()
        .map(|frame| (frame.id().to_string(), frame.owner().unwrap().node_id))
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2);
    for (index, (frame, _)) in children.iter().enumerate() {
        let context = session.frame_context_map().read()[frame];
        let result = session
            .evaluate_js_in_context(
                &format!("document.getElementById('value').textContent='child-{index}'"),
                context,
                false,
            )
            .await
            .unwrap();
        assert!(result.exception.is_none(), "{result:?}");
    }
    assert_ne!(children[0].1, children[1].1);
    assert_eq!(value(&mut session,"[document.getElementById('broken').contentDocument,document.getElementById('first').contentDocument.getElementById('value').textContent,document.getElementById('second').contentDocument.getElementById('value').textContent]").await,json!([null,"child-0","child-1"]));
    session.close().await.unwrap();
}

#[tokio::test]
async fn cross_origin_redirect_and_sandbox_do_not_expose_child_documents() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    mount(
        &other,
        "/secret",
        "<html><body><p id='secret'>private</p></body></html>",
    )
    .await;
    mount(
        &server,
        "/child",
        "<html><body><p id='secret'>sandboxed</p></body></html>",
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/redirect"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/secret", other.uri())),
        )
        .mount(&server)
        .await;
    mount(&server,"/page","<html><body><iframe id='redirect' src='/redirect'></iframe><iframe id='sandbox' sandbox='allow-scripts' src='/child'></iframe></body></html>").await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(value(&mut session,"['redirect','sandbox'].map(id=>{const f=document.getElementById(id);let denied=false;try{f.contentWindow.document}catch(error){denied=String(error).includes('SecurityError')}return [f.contentDocument,denied]})").await,json!([[null,true],[null,true]]));
    assert!(
        session
            .page()
            .unwrap()
            .root_frame()
            .children()
            .iter()
            .any(|frame| frame.url().as_str() == format!("{}/secret", other.uri()))
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn source_changes_owner_replacement_and_navigation_revoke_saved_references() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/page",
        "<html><body><div id='host'><iframe id='frame' src='/child'></iframe></div></body></html>",
    )
    .await;
    mount(
        &server,
        "/child",
        "<html><body><p id='value'>old</p></body></html>",
    )
    .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    evaluate(&mut session,"globalThis.oldFrame=document.getElementById('frame');globalThis.oldDoc=oldFrame.contentDocument;globalThis.oldNode=oldDoc.getElementById('value');oldFrame.src='/replacement'").await;
    assert_eq!(value(&mut session,"[oldFrame.contentDocument,(()=>{try{return oldNode.textContent}catch(error){return String(error).includes('InvalidStateError')}})()]").await,json!([null,true]));
    evaluate(
        &mut session,
        "document.getElementById('host').innerHTML='<iframe id=frame src=/child></iframe>'",
    )
    .await;
    assert_eq!(
        value(
            &mut session,
            "[oldFrame.contentDocument,document.getElementById('frame').contentDocument]"
        )
        .await,
        json!([null, null])
    );
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    // A real document navigation creates a fresh realm. The earlier assertions
    // still verify in-document owner/source revocation; no old JS globals cross
    // the document boundary at all.
    assert_eq!(
        value(&mut session, "[typeof oldDoc,typeof oldFrame,typeof oldNode,document.getElementById('frame').contentDocument.getElementById('value').textContent]").await,
        json!(["undefined", "undefined", "undefined", "old"])
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_foreign_native_calls_and_reflection_fail_explicitly() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/page",
        "<html><body><iframe id='frame' src='/child'></iframe></body></html>",
    )
    .await;
    mount(
        &server,
        "/child",
        "<html><body><p id='value'>unchanged</p></body></html>",
    )
    .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(value(&mut session,"(()=>{const w=document.getElementById('frame').contentWindow,d=w.document,n=d.getElementById('value');return [()=>w.fetch('/escape'),()=>n.textContent='changed',()=>Object.getPrototypeOf(d),()=>Object.getOwnPropertyDescriptor(d,'getElementById'),()=>n.click(),()=>('secret' in w),()=>Object.isExtensible(w)].map(call=>{try{call();return false}catch(error){return String(error).includes('NotSupportedError')}})})()").await,json!([true,true,true,true,true,true,true]));
    assert_eq!(
        value(
            &mut session,
            "document.getElementById('frame').contentDocument.getElementById('value').textContent"
        )
        .await,
        json!("unchanged")
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn owner_discovery_uses_live_dom_after_parent_scripts() {
    let server = MockServer::start().await;
    mount(&server,"/page","<html><body><iframe id='doomed' src='/not-loaded'></iframe><script>document.getElementById('doomed').remove();const frame=document.createElement('iframe');frame.id='live';frame.src='/child';document.body.appendChild(frame)</script></body></html>").await;
    Mock::given(method("GET"))
        .and(path("/not-loaded"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<p>wrong</p>", "text/html"))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/child"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><body><p id='value'>live</p></body></html>",
            "text/html",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(
        value(
            &mut session,
            "document.getElementById('live').contentDocument.getElementById('value').textContent"
        )
        .await,
        json!("live")
    );
    assert_eq!(session.frame_context_map().read().len(), 1);
    server.verify().await;
    session.close().await.unwrap();
}

#[tokio::test]
async fn sandboxed_parent_cannot_regain_tuple_origin_through_nested_frames() {
    let server = MockServer::start().await;
    mount(&server,"/page","<html><body><iframe id='opaque' sandbox='allow-scripts' src='/parent'></iframe></body></html>").await;
    mount(&server,"/parent","<html><body><iframe id='nested' src='/child'></iframe><iframe id='inline' srcdoc='<p id=value>inline</p>'></iframe></body></html>").await;
    mount(
        &server,
        "/child",
        "<html><body><p id='value'>secret</p></body></html>",
    )
    .await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let parent_id = session.page().unwrap().root_frame().children()[0]
        .id()
        .to_string();
    let context = session.frame_context_map().read()[&parent_id];
    let result=session.evaluate_js_in_context("JSON.stringify(['nested','inline'].map(id=>{const f=document.getElementById(id);let denied=false;try{f.contentWindow.document}catch(error){denied=String(error).includes('SecurityError')}return [f.contentDocument,denied]}))",context,false).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    assert_eq!(
        serde_json::from_str::<Value>(result.value.unwrap().as_str().unwrap()).unwrap(),
        json!([[null, true], [null, true]])
    );
    let nested_id = session.page().unwrap().root_frame().children()[0].children()[0]
        .id()
        .to_string();
    let nested_context = session.frame_context_map().read()[&nested_id];
    let result = session
        .evaluate_js_in_context(
            "(()=>{try{return localStorage.getItem('secret')}catch(error){return error.name}})()",
            nested_context,
            false,
        )
        .await
        .unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    assert_eq!(result.value, Some(json!("SecurityError")));
    session.close().await.unwrap();
}

#[tokio::test]
async fn ordinary_srcdoc_and_initial_blank_inherit_parent_document_origin() {
    let server = MockServer::start().await;
    mount(&server,"/page","<html><body><iframe id='inline' srcdoc='<p id=value>inline</p>'></iframe><iframe id='blank'></iframe></body></html>").await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    assert_eq!(value(&mut session,"[document.getElementById('inline').contentWindow.document.getElementById('value').textContent,document.getElementById('blank').contentDocument.body.textContent]").await,json!(["inline",""]));
    session.close().await.unwrap();
}

#[tokio::test]
async fn opaque_frame_storage_cookies_network_and_utility_world_fail_before_io() {
    let server = MockServer::start().await;
    mount(&server,"/page","<html><body><script>localStorage.setItem('secret','parent');document.cookie='secret=parent; path=/'</script><iframe sandbox='allow-scripts' src='/opaque'></iframe></body></html>").await;
    mount(&server,"/opaque",r#"<html><body><p id='status'>pending</p><script>
      globalThis.pageScriptRan=true;
      const probes=[()=>localStorage.getItem('secret'),()=>localStorage.setItem('secret','changed'),()=>sessionStorage.getItem('secret'),()=>indexedDB.open('secret'),()=>document.cookie,()=>document.cookie='secret=changed; path=/',()=>fetch('/escape'),()=>new XMLHttpRequest(),()=>new WebSocket('ws://127.0.0.1:1/escape')];
      document.getElementById('status').textContent=JSON.stringify(probes.map(probe=>{try{probe();return 'escaped'}catch(error){return error.name}}));
      const image=new Image();image.src='/escape-image';
    </script></body></html>"#).await;
    for route in ["/escape", "/escape-image"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
    }
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let frame_id = session.page().unwrap().root_frame().children()[0]
        .id()
        .to_string();
    let context = session.frame_context_map().read()[&frame_id];
    let result=session.evaluate_js_in_context("JSON.stringify([pageScriptRan,JSON.parse(document.getElementById('status').textContent)])",context,false).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    assert_eq!(
        serde_json::from_str::<Value>(result.value.unwrap().as_str().unwrap()).unwrap(),
        json!([true, vec!["SecurityError"; 9]])
    );
    let utility = session
        .create_isolated_world(&frame_id, "opaque-test")
        .await
        .unwrap();
    let result=session.evaluate_js_in_context("JSON.stringify([()=>localStorage.getItem('secret'),()=>sessionStorage.setItem('secret','changed'),()=>indexedDB.open('secret'),()=>document.cookie,()=>fetch('/escape'),()=>new XMLHttpRequest(),()=>new WebSocket('ws://127.0.0.1:1/escape')].map(probe=>{try{probe();return 'escaped'}catch(error){return error.name}}))",utility,false).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    assert_eq!(
        serde_json::from_str::<Value>(result.value.unwrap().as_str().unwrap()).unwrap(),
        json!(vec!["SecurityError"; 7])
    );
    assert_eq!(
        value(
            &mut session,
            "[localStorage.getItem('secret'),document.cookie]"
        )
        .await,
        json!(["parent", "secret=parent"])
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    server.verify().await;
    session.close().await.unwrap();
}

#[tokio::test]
async fn sandbox_without_allow_scripts_never_runs_or_fetches_page_scripts() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/page",
        "<html><body><iframe sandbox='allow-same-origin' src='/blocked'></iframe></body></html>",
    )
    .await;
    mount(&server,"/blocked","<html><body><p id='status'>initial</p><script>globalThis.pageScriptRan=true;document.getElementById('status').textContent='changed'</script><script src='/blocked.js'></script><iframe src='/descendant'></iframe></body></html>").await;
    Mock::given(method("GET"))
        .and(path("/blocked.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("globalThis.externalRan=true", "text/javascript"),
        )
        .expect(0)
        .mount(&server)
        .await;
    mount(&server,"/descendant","<html><body><p id='status'>initial</p><script>globalThis.pageScriptRan=true;document.getElementById('status').textContent='changed'</script><script src='/blocked.js'></script></body></html>").await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let frame = session.page().unwrap().root_frame().children()[0]
        .id()
        .to_string();
    let context = session.frame_context_map().read()[&frame];
    let descendant = session.page().unwrap().root_frame().children()[0].children()[0]
        .id()
        .to_string();
    let descendant_context = session.frame_context_map().read()[&descendant];
    for context in [context, descendant_context] {
        let result=session.evaluate_js_in_context("JSON.stringify([typeof pageScriptRan,typeof externalRan,document.getElementById('status').textContent])",context,false).await.unwrap();
        assert!(result.exception.is_none(), "{result:?}");
        assert_eq!(
            serde_json::from_str::<Value>(result.value.unwrap().as_str().unwrap()).unwrap(),
            json!(["undefined", "undefined", "initial"])
        );
    }
    server.verify().await;
    session.close().await.unwrap();
}

async fn mouse(session: &mut Session, x: f64, y: f64) {
    for kind in ["mousePressed", "mouseReleased"] {
        session.remote_command(1,json!({"method":"dispatchMouseEvent","params":{"type":kind,"x":x,"y":y,"button":"left","clickCount":1}})).await.unwrap();
    }
}

#[tokio::test]
async fn native_pointer_keyboard_and_capture_use_child_content_viewport() {
    let server = MockServer::start().await;
    mount(&server,"/page","<html><body style='margin:0'><iframe id='frame' src='/child' style='position:absolute;left:31px;top:42px;width:300px;height:100px;border:4px solid black;padding:3px'></iframe></body></html>").await;
    mount(&server,"/child","<html><body style='margin:0'><button id='inc' style='position:absolute;left:10px;top:10px;width:60px;height:22px'>Inc</button><p id='count' style='position:absolute;left:10px;top:35px;margin:0'>0</p><input id='input' style='position:absolute;left:10px;top:65px;width:100px;height:20px'><span id='mirror'></span><div style='position:absolute;left:120px;top:40px;width:25px;height:25px;background:rgb(0,0,255)'></div><script>let count=0;document.getElementById('inc').addEventListener('click',()=>document.getElementById('count').textContent=String(++count));document.getElementById('input').addEventListener('input',e=>document.getElementById('mirror').textContent=e.target.value)</script></body></html>").await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let context = *session.frame_context_map().read().values().next().unwrap();
    let dimensions = session
        .evaluate_js_in_context("JSON.stringify([innerWidth,innerHeight])", context, false)
        .await
        .unwrap();
    assert!(dimensions.exception.is_none(), "{dimensions:?}");
    assert_eq!(
        serde_json::from_str::<Value>(dimensions.value.unwrap().as_str().unwrap()).unwrap(),
        json!([300, 100])
    );
    mouse(&mut session, 38.0 + 30.0, 49.0 + 20.0).await;
    mouse(&mut session, 38.0 + 30.0, 49.0 + 20.0).await;
    assert_eq!(
        value(
            &mut session,
            "document.getElementById('frame').contentDocument.getElementById('count').textContent"
        )
        .await,
        json!("2")
    );
    mouse(&mut session, 38.0 + 30.0, 49.0 + 75.0).await;
    session
        .remote_command(1, json!({"method":"insertText","params":{"text":"hi"}}))
        .await
        .unwrap();
    assert_eq!(value(&mut session,"[document.activeElement.id,document.getElementById('frame').contentDocument.getElementById('mirror').textContent]").await,json!(["frame","hi"]));
    let png = session.capture_screenshot_png(1280).await.unwrap();
    let image = image::load_from_memory(&png).unwrap().into_rgba8();
    assert_eq!(image.get_pixel(38 + 130, 49 + 50).0, [0, 0, 255, 255]);
    session.close().await.unwrap();
}

#[tokio::test]
async fn focused_child_password_blocks_composited_capture_until_focus_leaves_frame() {
    let server = MockServer::start().await;
    mount(&server,"/page","<html><body style='margin:0'><iframe id='frame' src='/child' style='position:absolute;left:20px;top:20px;width:200px;height:100px;border:0'></iframe><button id='outside' style='position:absolute;left:300px;top:20px;width:80px;height:30px'>Outside</button></body></html>").await;
    mount(&server,"/child","<html><body style='margin:0'><input type='password' style='position:absolute;left:10px;top:10px;width:100px;height:20px' value='secret'></body></html>").await;
    let mut session = session().await;
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    mouse(&mut session, 50.0, 40.0).await;
    let result = session.capture_screenshot_png(1280).await;
    assert!(result.is_err(), "focused child password was captured");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains(oxibrowser_core::session::CAPTURE_BLOCKED_MSG)
    );
    mouse(&mut session, 330.0, 35.0).await;
    assert!(session.capture_screenshot_png(1280).await.is_ok());
    session.close().await.unwrap();
}

#[tokio::test]
async fn opaque_guards_survive_default_and_utility_context_timeout_recreation() {
    let server = MockServer::start().await;
    mount(
        &server,
        "/page",
        "<html><body><iframe sandbox='allow-scripts' src='/opaque'></iframe></body></html>",
    )
    .await;
    mount(&server, "/opaque", "<html><body>opaque</body></html>").await;
    let mut config = BrowserConfig::headless();
    config.enable_ssrf_filter = false;
    config.js_timeout_ms = 30;
    config.js_max_loop_iterations = 100;
    let browser = Browser::new(config.clone()).await.unwrap();
    let mut session = Session::new(browser.id(), config, browser.default_context())
        .await
        .unwrap();
    session
        .navigate(&format!("{}/page", server.uri()))
        .await
        .unwrap();
    let frame = session.page().unwrap().root_frame().children()[0]
        .id()
        .to_string();
    let context = session.frame_context_map().read()[&frame];
    let utility = session
        .create_isolated_world(&frame, "opaque-timeout")
        .await
        .unwrap();
    for context in [context, utility] {
        let timeout = session
            .evaluate_js_in_context("while(true){}", context, false)
            .await;
        assert!(
            match &timeout {
                Err(_) => true,
                Ok(result) => result.exception.is_some() || result.timed_out,
            },
            "{timeout:?}"
        );
        let result=session.evaluate_js_in_context("JSON.stringify([()=>localStorage,()=>sessionStorage,()=>indexedDB,()=>document.cookie,()=>fetch('/escape'),()=>new XMLHttpRequest(),()=>new WebSocket('ws://127.0.0.1:1/escape')].map(probe=>{try{probe();return 'escaped'}catch(error){return error.name}}))",context,false).await.unwrap();
        assert!(result.exception.is_none(), "{result:?}");
        assert_eq!(
            serde_json::from_str::<Value>(result.value.unwrap().as_str().unwrap()).unwrap(),
            json!(vec!["SecurityError"; 7])
        );
    }
    session.close().await.unwrap();
}
