//! Remote objects must stay live across protocol commands rather than roundtrip
//! through JSON or page-visible temporary variables.
use oxibrowser_core::js::runtime::{JsRuntime, JsRuntimeConfig};
use serde_json::{Value, json};

async fn command(runtime: &mut JsRuntime, context: u32, method: &str, params: Value) -> Value {
    runtime
        .remote_command(context, json!({"method":method,"params":params}))
        .await
        .unwrap_or_else(|error| panic!("context {context} {method}: {error}"))
}

#[tokio::test]
async fn remote_primitives_keep_cdp_types() {
    let mut runtime = JsRuntime::new();
    for (expression, expected) in [
        ("undefined", json!({"type":"undefined"})),
        (
            "null",
            json!({"type":"object", "subtype":"null", "value":null}),
        ),
        ("true", json!({"type":"boolean", "value":true})),
        ("'hello'", json!({"type":"string", "value":"hello"})),
    ] {
        let response = command(
            &mut runtime,
            1,
            "evaluate",
            json!({"expression":expression}),
        )
        .await;
        assert_eq!(response["result"], expected);
    }
    for expression in ["NaN", "Infinity", "-Infinity", "-0", "123n"] {
        let response = command(
            &mut runtime,
            1,
            "evaluate",
            json!({"expression":expression}),
        )
        .await;
        assert_eq!(response["result"]["unserializableValue"], expression);
        assert!(response["result"].get("value").is_none());
    }
}

#[tokio::test]
async fn remote_references_preserve_this_arguments_closures_and_descriptors() {
    let mut runtime = JsRuntime::new();
    let object = command(&mut runtime, 1, "evaluate", json!({"expression":
        "globalThis.getterCalls = 0; ({ count: 7, get secret() { getterCalls++; return 99; }, nested: { x: 4 } })",
        "objectGroup":"objects"})).await;
    let id = object["result"]["objectId"].as_str().unwrap();
    assert!(object["result"].get("value").is_none());
    let response = command(&mut runtime, 1, "callFunctionOn", json!({
        "objectId":id, "functionDeclaration":"function(other, missing, special) { this.count += other.count; return [this.count, this === other, missing === undefined, Object.is(special, -0)]; }",
        "arguments":[{"objectId":id}, {}, {"unserializableValue":"-0"}], "returnByValue":true
    })).await;
    assert_eq!(response["result"]["value"], json!([14, true, true, true]));
    let response = command(
        &mut runtime,
        1,
        "getProperties",
        json!({"objectId":id,"ownProperties":true}),
    )
    .await;
    let properties = response["result"].as_array().unwrap();
    assert_eq!(
        properties.iter().find(|p| p["name"] == "count").unwrap()["value"]["value"],
        14
    );
    let secret = properties.iter().find(|p| p["name"] == "secret").unwrap();
    assert_eq!(secret["get"]["type"], "function");
    assert!(secret.get("value").is_none());
    let getter_calls = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"getterCalls"}),
    )
    .await;
    assert_eq!(getter_calls["result"]["value"], 0);

    let function = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"(() => { let n = 0; return () => ++n; })()"}),
    )
    .await;
    let function_id = function["result"]["objectId"].as_str().unwrap();
    for expected in [1, 2] {
        let response = command(
            &mut runtime,
            1,
            "callFunctionOn",
            json!({"objectId":function_id,"functionDeclaration":"function() { return this(); }"}),
        )
        .await;
        assert_eq!(response["result"]["value"], expected);
    }
}

#[tokio::test]
async fn releases_and_navigation_invalidate_handles() {
    let mut runtime = JsRuntime::new();
    let response = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"({child:{answer:42}})","objectGroup":"temporary"}),
    )
    .await;
    let id = response["result"]["objectId"].as_str().unwrap();
    let props = command(
        &mut runtime,
        1,
        "getProperties",
        json!({"objectId":id,"ownProperties":true}),
    )
    .await;
    let child = props["result"][0]["value"]["objectId"].as_str().unwrap();
    command(
        &mut runtime,
        1,
        "releaseObjectGroup",
        json!({"objectGroup":"temporary"}),
    )
    .await;
    for object_id in [id, child] {
        assert!(
            runtime
                .remote_command(
                    1,
                    json!({"method":"getProperties","params":{"objectId":object_id}})
                )
                .await
                .is_err()
        );
    }
    let response = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"({old:true})"}),
    )
    .await;
    let id = response["result"]["objectId"].as_str().unwrap();
    runtime
        .set_document(
            "<html><body>new</body></html>",
            Some("https://example.test/"),
            (800, 600),
        )
        .await
        .unwrap();
    assert!(runtime.remote_command(1, json!({"method":"callFunctionOn","params":{"objectId":id,"functionDeclaration":"function() { return this.old; }"}})).await.is_err());
}

#[tokio::test]
async fn awaited_promises_resolve_reject_and_bound_execution() {
    let mut runtime = JsRuntime::with_config(JsRuntimeConfig {
        background_event_loop: false,
        ..Default::default()
    });
    for (expression, expected) in [
        ("Promise.resolve({answer:42})", json!({"answer":42})),
        (
            "new Promise(resolve => setTimeout(() => resolve(23), 5))",
            json!(23),
        ),
    ] {
        let response = command(
            &mut runtime,
            1,
            "evaluate",
            json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
        )
        .await;
        assert_eq!(response["result"]["value"], expected);
    }
    let rejection = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"Promise.reject(new Error('rejected'))","awaitPromise":true}),
    )
    .await;
    assert!(rejection.get("exceptionDetails").is_some());
    let timeout = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"new Promise(() => {})","awaitPromise":true,"timeout":10}),
    )
    .await;
    assert!(
        timeout["exceptionDetails"]["text"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
    let limit = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"while(true) {}"}),
    )
    .await;
    assert!(limit.get("exceptionDetails").is_some());
    let alive = command(&mut runtime, 1, "evaluate", json!({"expression":"2+2"})).await;
    assert_eq!(alive["result"]["value"], 4);
}

#[tokio::test]
async fn handles_are_context_scoped_and_invalid_ids_never_become_undefined() {
    let mut runtime = JsRuntime::new();
    runtime
        .set_frame_document(
            2,
            "<html><body>frame</body></html>",
            "https://example.test/frame",
            (800, 600),
            Vec::new(),
        )
        .await
        .unwrap();
    let response = command(
        &mut runtime,
        2,
        "evaluate",
        json!({"expression":"({answer:42})"}),
    )
    .await;
    let id = response["result"]["objectId"].as_str().unwrap();
    assert!(id.starts_with("oxi-remote-2-"));
    assert!(
        runtime
            .remote_command(
                1,
                json!({"method":"getProperties","params":{"objectId":id}})
            )
            .await
            .is_err()
    );
    let response = command(
        &mut runtime,
        2,
        "callFunctionOn",
        json!({"objectId":id,"functionDeclaration":"function() { return this.answer; }"}),
    )
    .await;
    assert_eq!(response["result"]["value"], 42);
    assert!(runtime.remote_command(1, json!({"method":"callFunctionOn","params":{"functionDeclaration":"x => x", "arguments":[{"objectId":"missing"}]}})).await.is_err());
    command(&mut runtime, 2, "releaseObject", json!({"objectId":id})).await;
    assert!(
        runtime
            .remote_command(
                2,
                json!({"method":"getProperties","params":{"objectId":id}})
            )
            .await
            .is_err()
    );
    assert!(
        runtime
            .remote_command(99, json!({"method":"evaluate","params":{"expression":"1"}}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_bindings_emit_typed_events_and_validate_payloads() {
    let mut runtime = JsRuntime::new();
    let (tx, rx) = std::sync::mpsc::channel();
    runtime.set_event_sink(tx);
    command(&mut runtime, 1, "addBinding", json!({"name":"testBinding"})).await;
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"testBinding('payload')"}),
    )
    .await;
    assert!(result.get("exceptionDetails").is_none());
    assert!(rx.try_iter().any(|event| matches!(event, oxibrowser_core::js::CoreEvent::BindingCalled {name, payload, execution_context_id:1, ..} if name == "testBinding" && payload == "payload")));
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"testBinding(42)"}),
    )
    .await;
    assert!(result.get("exceptionDetails").is_some());
    runtime
        .set_document("<html><body>new</body></html>", None, (800, 600))
        .await
        .unwrap();
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"testBinding('after navigation')"}),
    )
    .await;
    assert!(result.get("exceptionDetails").is_none());
}

#[tokio::test]
async fn focus_emulation_exposes_live_headless_document_focus() {
    let mut runtime = JsRuntime::new();
    runtime
        .set_document(
            "<html><body><input id='field'></body></html>",
            None,
            (800, 600),
        )
        .await
        .unwrap();
    command(
        &mut runtime,
        1,
        "setFocusEmulationEnabled",
        json!({"enabled":true}),
    )
    .await;
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"document.hasFocus()"}),
    )
    .await;
    assert_eq!(result["result"]["value"], true);
}

#[tokio::test]
async fn layout_metrics_read_native_viewport_and_content() {
    let mut runtime = JsRuntime::new();
    runtime
        .set_document(
            "<html><body style='margin:0;height:900px'>content</body></html>",
            None,
            (640, 480),
        )
        .await
        .unwrap();
    command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"window.innerWidth = 12345"}),
    )
    .await;
    let result = command(&mut runtime, 1, "getLayoutMetrics", json!({})).await;
    assert_eq!(result["cssLayoutViewport"]["clientWidth"], 640);
    assert_eq!(result["cssLayoutViewport"]["clientHeight"], 480);
    assert!(result["cssContentSize"]["height"].as_u64().unwrap() >= 900);
    assert_eq!(result["visualViewport"]["scale"], 1);
}

#[tokio::test]
async fn dom_geometry_uses_live_native_reference_and_distinct_css_boxes() {
    let mut runtime = JsRuntime::new();
    runtime.set_document("<html><body style='margin:0'><div id='box' style='width:100px;height:40px;padding:10px;border:2px solid black'>content</div></body></html>", None, (640,480)).await.unwrap();
    let handle = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"document.querySelector('#box')"}),
    )
    .await;
    let id = handle["result"]["objectId"].as_str().unwrap();
    command(&mut runtime, 1, "callFunctionOn", json!({"objectId":id,"functionDeclaration":"function() { this.getBoundingClientRect = () => ({x:99999,y:99999,width:1,height:1}); }"})).await;
    let response = command(&mut runtime, 1, "getBoxModel", json!({"objectId":id})).await;
    let model = &response["model"];
    assert_eq!(model["width"].as_f64(), Some(124.0));
    assert_eq!(model["content"][0].as_f64(), Some(12.0));
    assert_eq!(model["border"][0].as_f64(), Some(0.0));
    let quads = command(&mut runtime, 1, "getContentQuads", json!({"objectId":id})).await;
    assert_eq!(quads["quads"][0], model["content"]);
    let node = command(&mut runtime, 1, "requestNode", json!({"objectId":id})).await;
    let hit = command(
        &mut runtime,
        1,
        "getNodeForLocation",
        json!({"x":15,"y":15}),
    )
    .await;
    assert_eq!(hit["nodeId"], node["nodeId"]);
}

#[tokio::test]
async fn cdp_text_edits_native_value_without_invoking_page_setter() {
    let mut runtime = JsRuntime::new();
    runtime
        .set_document(
            "<html><body><input id='field' value='abc'><input id='next'></body></html>",
            None,
            (640, 480),
        )
        .await
        .unwrap();
    command(&mut runtime,1,"evaluate",json!({"expression":r#"
      globalThis.field=document.querySelector('#field'); field.focus();
      field.selectionStart=field.selectionEnd=0;globalThis.inputs=[];globalThis.keys=[];
      Object.defineProperty(field,'value',{get(){return field.getAttribute('value')},set(){throw new Error('page setter must not be used')}});
      field.addEventListener('keydown',e=>keys.push([e.key,e.ctrlKey,e.shiftKey,e.isTrusted]));
      field.addEventListener('beforeinput',e=>{if(e.data==='x')e.preventDefault()});
      field.addEventListener('input',e=>inputs.push([field.value,e.data,e.inputType,e.isTrusted]));
    "#})).await;
    command(
        &mut runtime,
        1,
        "dispatchKeyEvent",
        json!({"type":"keyDown","key":"😀","text":"😀","code":"","modifiers":0}),
    )
    .await;
    command(
        &mut runtime,
        1,
        "dispatchKeyEvent",
        json!({"type":"keyDown","key":"x","text":"x","code":"KeyX","modifiers":0}),
    )
    .await;
    command(
        &mut runtime,
        1,
        "dispatchKeyEvent",
        json!({"type":"keyDown","key":"c","text":"c","code":"KeyC","modifiers":10}),
    )
    .await;
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"[field.value,field.selectionStart,inputs,keys]","returnByValue":true}),
    )
    .await;
    assert_eq!(
        result["result"]["value"],
        json!([
            "😀abc",
            2,
            [["😀abc", "😀", "insertText", true]],
            [
                ["😀", false, false, true],
                ["x", false, false, true],
                ["c", true, true, true]
            ]
        ])
    );
    command(
        &mut runtime,
        1,
        "dispatchKeyEvent",
        json!({"type":"rawKeyDown","key":"Tab","code":"Tab","modifiers":0}),
    )
    .await;
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"document.activeElement.id"}),
    )
    .await;
    assert_eq!(result["result"]["value"], "next");
}

#[tokio::test]
async fn uncancelled_mouse_down_performs_focus_default_action() {
    let mut runtime = JsRuntime::new();
    runtime.set_document("<html><body style='margin:0'><input id='field' style='width:100px;height:30px'></body></html>", None, (640,480)).await.unwrap();
    command(
        &mut runtime,
        1,
        "dispatchMouseEvent",
        json!({"type":"mousePressed","x":20,"y":15,"button":"left","clickCount":1}),
    )
    .await;
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"document.activeElement.id"}),
    )
    .await;
    assert_eq!(result["result"]["value"], "field");
    command(&mut runtime,1,"evaluate",json!({"expression":"document.querySelector('#field').blur(); document.querySelector('#field').addEventListener('mousedown', e => e.preventDefault());"})).await;
    command(
        &mut runtime,
        1,
        "dispatchMouseEvent",
        json!({"type":"mousePressed","x":20,"y":15,"button":"left","clickCount":1}),
    )
    .await;
    let result = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"document.activeElement.id"}),
    )
    .await;
    assert_ne!(result["result"]["value"], "field");
}

#[tokio::test]
async fn trusted_input_bypasses_page_dispatch_event_overrides() {
    let mut runtime = JsRuntime::new();
    runtime.set_document("<html><body style='margin:0'><input id='field' style='width:100px;height:30px'><input id='check' type='checkbox' style='position:absolute;left:150px;top:0;width:30px;height:30px'></body></html>", None, (640,480)).await.unwrap();
    command(&mut runtime,1,"evaluate",json!({"expression":r#"
      globalThis.field=document.querySelector('#field'); globalThis.check=document.querySelector('#check');
      globalThis.overrides=0; globalThis.events=[];
      field.dispatchEvent=check.dispatchEvent=()=>{overrides++;return true};
      field.addEventListener('beforeinput',e=>{events.push(['beforeinput',e.data,e.isTrusted]);if(e.data==='blocked')e.preventDefault()});
      field.addEventListener('input',e=>events.push(['input',e.data,e.isTrusted]));
      document.addEventListener('keydown',e=>{events.push(['keydown',e.key,e.isTrusted]);e.preventDefault()},{capture:true,once:true});
      check.addEventListener('mousedown',e=>{events.push(['mousedown',e.isTrusted]);e.preventDefault()},{once:true});
      check.addEventListener('click',e=>events.push(['click',check.checked,e.isTrusted]));
      field.focus();
    "#})).await;
    command(&mut runtime, 1, "insertText", json!({"text":"blocked"})).await;
    command(
        &mut runtime,
        1,
        "dispatchKeyEvent",
        json!({"type":"keyDown","key":"x","text":"x"}),
    )
    .await;
    command(
        &mut runtime,
        1,
        "dispatchKeyEvent",
        json!({"type":"keyDown","key":"y","text":"y"}),
    )
    .await;
    command(
        &mut runtime,
        1,
        "dispatchMouseEvent",
        json!({"type":"mousePressed","x":165,"y":15,"button":"left","clickCount":1}),
    )
    .await;
    let response = command(&mut runtime,1,"evaluate",json!({"expression":"[field.value,document.activeElement.id,overrides,events]","returnByValue":true})).await;
    assert_eq!(
        response["result"]["value"],
        json!([
            "y",
            "field",
            0,
            [
                ["beforeinput", "blocked", true],
                ["keydown", "x", true],
                ["beforeinput", "y", true],
                ["input", "y", true],
                ["mousedown", true]
            ]
        ])
    );
    command(
        &mut runtime,
        1,
        "dispatchMouseEvent",
        json!({"type":"mouseReleased","x":165,"y":15,"button":"left","clickCount":1}),
    )
    .await;
    let response = command(&mut runtime,1,"evaluate",json!({"expression":"[check.checked,overrides,events[events.length-1]]","returnByValue":true})).await;
    assert_eq!(
        response["result"]["value"],
        json!([true, 0, ["click", true, true]])
    );
}

#[tokio::test]
async fn nested_frame_geometry_projects_live_native_boxes_into_root_viewport() {
    let mut runtime = JsRuntime::new();
    runtime.set_page_url("https://frames.test/");
    runtime.set_document("<html><body style='margin:0;height:2000px'><iframe id='outer' src='about:blank' style='position:absolute;left:60px;top:100px;width:240px;height:200px;border:4px solid;padding:6px;transform:rotate(12deg) scale(1.1)'></iframe><div id='scroll' style='position:absolute;top:1000px;width:1px;height:1px'></div></body></html>", Some("https://frames.test/"), (800,600)).await.unwrap();
    let outer = runtime.frame_owners(1).await.unwrap().remove(0);
    runtime.set_owned_frame_document(2,outer,"<html><body style='margin:0;height:1600px'><iframe id='inner' src='about:blank' style='position:absolute;left:30px;top:70px;width:100px;height:80px;border:3px solid;padding:5px;transform:rotate(-9deg) scale(0.9)'></iframe><div id='scroll' style='position:absolute;top:1000px;width:1px;height:1px'></div></body></html>", "https://frames.test/middle", (240,200), vec![]).await.unwrap();
    let inner = runtime.frame_owners(2).await.unwrap().remove(0);
    runtime.set_owned_frame_document(3,inner,"<html><body style='margin:0;height:1200px'><div id='leaf' style='position:absolute;left:11px;top:25px;width:20px;height:10px;padding:2px;border:1px solid;margin:4px'></div><div id='scroll' style='position:absolute;top:1000px;width:1px;height:1px'></div></body></html>", "https://frames.test/leaf", (100,80), vec![]).await.unwrap();
    runtime.create_isolated_world(4, 3).await.unwrap();
    let outer_handle = command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"document.querySelector('#outer')"}),
    )
    .await;
    for context in [1, 2, 3] {
        let target = command(
            &mut runtime,
            context,
            "evaluate",
            json!({"expression":"document.querySelector('#scroll')"}),
        )
        .await;
        command(
            &mut runtime,
            context,
            "scrollIntoViewIfNeeded",
            json!({"objectId":target["result"]["objectId"]}),
        )
        .await;
        let metrics = command(&mut runtime, context, "getLayoutMetrics", json!({})).await;
        assert!(metrics["cssLayoutViewport"]["pageY"].as_f64().unwrap() > 0.0);
    }
    // Scrolling must preserve the pre-scroll native wrapper incarnation.
    command(
        &mut runtime,
        1,
        "getBoxModel",
        json!({"objectId":outer_handle["result"]["objectId"]}),
    )
    .await;
    let inner = command(
        &mut runtime,
        2,
        "evaluate",
        json!({"expression":"document.querySelector('#inner')"}),
    )
    .await;
    let inner_id = inner["result"]["objectId"].as_str().unwrap();
    let inner_model = command(&mut runtime, 2, "getBoxModel", json!({"objectId":inner_id})).await;
    let content = inner_model["model"]["content"].as_array().unwrap();
    let content: Vec<f64> = content.iter().map(|v| v.as_f64().unwrap()).collect();
    let leaf = command(
        &mut runtime,
        3,
        "evaluate",
        json!({"expression":"document.querySelector('#leaf')"}),
    )
    .await;
    let leaf_id = leaf["result"]["objectId"].as_str().unwrap();
    let isolated = command(
        &mut runtime,
        4,
        "evaluate",
        json!({"expression":"document.querySelector('#leaf')"}),
    )
    .await;
    let isolated_id = isolated["result"]["objectId"].as_str().unwrap();
    let rect = command(&mut runtime,3,"evaluate",json!({"expression":"(()=>{let r=document.querySelector('#leaf').getBoundingClientRect();return [r.x,r.y,r.width,r.height,window.innerWidth,window.innerHeight]})()","returnByValue":true})).await;
    let rect = rect["result"]["value"]
        .as_array()
        .unwrap_or_else(|| panic!("Expected local native rectangle: {rect}"));
    let x = rect[0].as_f64().unwrap();
    let y = rect[1].as_f64().unwrap();
    assert_eq!(rect[2], 26);
    assert_eq!(rect[3], 16);
    assert_eq!(rect[4], 100);
    assert_eq!(rect[5], 80);
    let model = command(&mut runtime, 3, "getBoxModel", json!({"objectId":leaf_id})).await;
    let utility_model = command(
        &mut runtime,
        4,
        "getBoxModel",
        json!({"objectId":isolated_id}),
    )
    .await;
    assert_eq!(model["model"], utility_model["model"]);
    assert_eq!(model["model"]["width"], 26.0);
    assert_eq!(model["model"]["height"], 16.0);
    for (name, inset) in [
        ("content", 3.0),
        ("padding", 1.0),
        ("border", 0.0),
        ("margin", -4.0),
    ] {
        let local = [
            x + inset,
            y + inset,
            x + 26.0 - inset,
            y + inset,
            x + 26.0 - inset,
            y + 16.0 - inset,
            x + inset,
            y + 16.0 - inset,
        ];
        let actual = model["model"][name].as_array().unwrap();
        for (index, point) in local.as_chunks::<2>().0.iter().enumerate() {
            let expected_x = content[0]
                + point[0] * (content[2] - content[0]) / 100.0
                + point[1] * (content[6] - content[0]) / 80.0;
            let expected_y = content[1]
                + point[0] * (content[3] - content[1]) / 100.0
                + point[1] * (content[7] - content[1]) / 80.0;
            assert!(
                (actual[index * 2].as_f64().unwrap() - expected_x).abs() < 1e-5,
                "{name} x"
            );
            assert!(
                (actual[index * 2 + 1].as_f64().unwrap() - expected_y).abs() < 1e-5,
                "{name} y"
            );
        }
    }
    let quads = command(
        &mut runtime,
        4,
        "getContentQuads",
        json!({"objectId":isolated_id}),
    )
    .await;
    assert_eq!(quads["quads"][0], model["model"]["content"]);
    // Local DOM geometry stays in the leaf viewport, despite CDP projection.
    let local = command(
        &mut runtime,
        4,
        "evaluate",
        json!({"expression":"document.querySelector('#leaf').getBoundingClientRect().x"}),
    )
    .await;
    assert_eq!(local["result"]["value"], x);
    command(
        &mut runtime,
        1,
        "evaluate",
        json!({"expression":"document.querySelector('#outer').remove()"}),
    )
    .await;
    for method in ["getContentQuads", "getBoxModel"] {
        assert!(
            runtime
                .remote_command(
                    4,
                    json!({"method":method,"params":{"objectId":isolated_id}})
                )
                .await
                .is_err(),
            "detached owner must reject {method}"
        );
    }
}

#[tokio::test]
async fn native_caret_is_shared_and_ignores_page_selection_overrides() {
    let mut runtime = JsRuntime::new();
    runtime
        .set_document(
            "<html><body><input id='field' value='abc'></body></html>",
            None,
            (640, 480),
        )
        .await
        .unwrap();
    runtime.create_isolated_world(2, 1).await.unwrap();
    let initial = command(&mut runtime,1,"evaluate",json!({"expression":"(()=>{let f=document.querySelector('#field');f.focus();return ['selectionStart' in f,f.selectionStart,f.selectionEnd,f.selectionDirection]})()","returnByValue":true})).await;
    assert_eq!(initial["result"]["value"], json!([true, 0, 0, "none"]));
    command(&mut runtime,1,"evaluate",json!({"expression":r#"
      let field=document.querySelector('#field');field.setSelectionRange(1,2,'backward');
      for(let property of ['selectionStart','selectionEnd'])Object.defineProperty(field,property,{get(){throw Error('page selection getter')},set(){throw Error('page selection setter')}});
    "#})).await;
    let shared = command(&mut runtime,2,"evaluate",json!({"expression":"(()=>{let f=document.querySelector('#field');return [f.selectionStart,f.selectionEnd,f.selectionDirection]})()","returnByValue":true})).await;
    assert_eq!(shared["result"]["value"], json!([1, 2, "backward"]));
    command(&mut runtime, 1, "insertText", json!({"text":"😀"})).await;
    let shared = command(&mut runtime,2,"evaluate",json!({"expression":"(()=>{let f=document.querySelector('#field');return [f.value,f.selectionStart,f.selectionEnd,f.selectionDirection]})()","returnByValue":true})).await;
    assert_eq!(shared["result"]["value"], json!(["a😀c", 3, 3, "none"]));
}

async fn screenshot_context(html: Option<&str>) -> oxibrowser_cdp::domains::DispatchContext {
    use oxibrowser_core::{Browser, BrowserConfig};
    use std::sync::Arc;
    let mut config = BrowserConfig::headless();
    config.viewport_width = 1280;
    config.viewport_height = 800;
    let browser = Arc::new(Browser::new(config).await.unwrap());
    let session = browser.new_session().await.unwrap();
    if let Some(html) = html {
        session
            .write()
            .await
            .navigate(&format!("data:text/html,{html}"))
            .await
            .unwrap();
    }
    let (events, _) = oxibrowser_cdp::event::event_channel();
    oxibrowser_cdp::domains::DispatchContext {
        dialog_gate: session.read().await.dialog_gate(),
        session: session.clone(),
        events,
        fetch_registry: oxibrowser_core::network::intercept::shared_registry(),
        browser: browser.clone(),
        child_targets: Arc::new(oxibrowser_cdp::domains::TargetRegistry::new()),
        browser_context: browser.default_context(),
        credentials: None,
        role: oxibrowser_cdp::session::RoleKind::Agent,
        logins: None,
    }
}

async fn screenshot(
    ctx: &oxibrowser_cdp::domains::DispatchContext,
    params: Value,
) -> image::RgbaImage {
    use base64::Engine;
    let result = oxibrowser_cdp::domains::page::handle("captureScreenshot", Some(params), ctx)
        .await
        .unwrap()
        .unwrap();
    assert!(result.get("metadata").is_none());
    let png = base64::engine::general_purpose::STANDARD
        .decode(result["data"].as_str().unwrap())
        .unwrap();
    image::load_from_memory(&png).unwrap().to_rgba8()
}

#[tokio::test]
async fn viewport_screenshot_keeps_short_page_background_and_playwright_clip() {
    let ctx=screenshot_context(Some("<html style='background:rgb(17,34,51)'><body style='margin:0'><div style='height:240px;background:red'>short</div></body></html>")).await;
    let image = screenshot(&ctx, json!({"format":"png"})).await;
    assert_eq!(image.dimensions(), (1280, 800));
    assert_eq!(image.get_pixel(100, 700).0, [17, 34, 51, 255]);
    let image=screenshot(&ctx,json!({"format":"png","captureBeyondViewport":false,"clip":{"x":0,"y":0,"width":1280,"height":800,"scale":1}})).await;
    assert_eq!(image.dimensions(), (1280, 800));
    assert_eq!(image.get_pixel(100, 700).0, [17, 34, 51, 255]);
    let image = screenshot(
        &ctx,
        json!({"clip":{"x":10,"y":500,"width":40,"height":50,"scale":1}}),
    )
    .await;
    assert_eq!(image.dimensions(), (40, 50));
    assert_eq!(image.get_pixel(20, 20).0, [17, 34, 51, 255]);
}

#[tokio::test]
async fn viewport_screenshot_clips_tall_content_and_preserves_full_page_api() {
    let ctx=screenshot_context(Some("<html><body style='margin:0'><div style='height:1200px;background:rgb(0,153,0)'><span id='scroll-target' style='position:absolute;top:1000px;width:1px;height:1px'></span></div></body></html>")).await;
    let image = screenshot(&ctx, json!({})).await;
    assert_eq!(image.dimensions(), (1280, 800));
    assert_eq!(image.get_pixel(100, 799).0, [0, 153, 0, 255]);
    let full = ctx
        .session
        .write()
        .await
        .capture_screenshot_png(1280)
        .await
        .unwrap();
    assert_eq!(image::load_from_memory(&full).unwrap().height(), 1200);
    let metrics = {
        let mut session = ctx.session.write().await;
        let target=session.remote_command(1,json!({"method":"evaluate","params":{"expression":"document.querySelector('#scroll-target')"}})).await.unwrap();
        session.remote_command(1,json!({"method":"scrollIntoViewIfNeeded","params":{"objectId":target["result"]["objectId"]}})).await.unwrap();
        session
            .remote_command(1, json!({"method":"getLayoutMetrics","params":{}}))
            .await
            .unwrap()
    };
    let viewport = &metrics["cssLayoutViewport"];
    assert!(viewport["pageY"].as_f64().unwrap() > 0.0);
    let image=screenshot(&ctx,json!({"captureBeyondViewport":false,"clip":{"x":viewport["pageX"],"y":viewport["pageY"],"width":1280,"height":800,"scale":1}})).await;
    assert_eq!(image.dimensions(), (1280, 800));
    assert_eq!(image.get_pixel(100, 799).0, [0, 153, 0, 255]);
}

#[tokio::test]
async fn viewport_screenshot_propagates_password_and_missing_document_failures() {
    let empty = screenshot_context(None).await;
    assert!(
        oxibrowser_cdp::domains::page::handle("captureScreenshot", None, &empty)
            .await
            .is_err()
    );
    let ctx = screenshot_context(Some(
        "<html><body><input id='secret' type='password'></body></html>",
    ))
    .await;
    ctx.session
        .write()
        .await
        .evaluate_js("document.querySelector('#secret').focus()")
        .await
        .unwrap();
    let error = oxibrowser_cdp::domains::page::handle("captureScreenshot", None, &ctx)
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .contains(oxibrowser_core::session::CAPTURE_BLOCKED_MSG)
    );
    assert!(
        ctx.session
            .write()
            .await
            .capture_screenshot_png(1280)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn viewport_screenshot_rejects_unsupported_capture_parameters() {
    let ctx = screenshot_context(Some("<html><body>page</body></html>")).await;
    for params in [
        json!({"format":"jpeg"}),
        json!({"format":"webp"}),
        json!({"captureBeyondViewport":true}),
        json!({"fromSurface":false}),
        json!({"clip":{"x":0,"y":0,"width":1280,"height":800,"scale":2}}),
        json!({"clip":{"x":0,"y":0,"width":1280,"height":801,"scale":1}}),
        json!({"clip":{"x":0.5,"y":0,"width":10,"height":10,"scale":1}}),
    ] {
        let error =
            oxibrowser_cdp::domains::page::handle("captureScreenshot", Some(params.clone()), &ctx)
                .await
                .unwrap_err();
        assert_eq!(error.code, -32602, "{params}: {}", error.message);
    }
}
