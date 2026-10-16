//! Synthetic local documents: no network, account, keychain, profile or GUI.
use oxibrowser_core::error::CoreError;
use oxibrowser_core::js::runtime::{JsRuntime, NativeInput, NativeTarget};
use serde_json::json;
use std::time::Duration;

const HTML: &str = "<h1 id='marker'>before</h1><input id='name'><button id='go'>Go</button>";
async fn setup() -> JsRuntime {
    let mut rt = JsRuntime::new();
    rt.set_document(HTML, Some("http://fixture.invalid/"), (640, 480))
        .await
        .unwrap();
    rt
}
async fn eval(rt: &mut JsRuntime, script: &str) {
    let result = rt.evaluate(script).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
}
async fn target(rt: &mut JsRuntime, selector: &str) -> NativeTarget {
    let observation = rt.native_observe("http://fixture.invalid/").await.unwrap();
    observation
        .target(observation.snapshot.query_selector(selector).unwrap())
        .unwrap()
}
fn failure(error: CoreError, uncertain: bool, message: &str) {
    match error {
        CoreError::NativeAction {
            message: actual,
            may_have_acted,
        } => {
            assert_eq!(may_have_acted, uncertain, "{actual}");
            assert!(actual.contains(message), "{actual}");
        }
        error => panic!("unexpected error: {error}"),
    }
}

#[tokio::test]
async fn hostile_page_apis_and_callback_returns_cannot_resolve_or_forge_action() {
    let mut rt = setup().await;
    eval(&mut rt,r#"
        document.getElementById('name').addEventListener('input',function(e){
            if(e.isTrusted!==false || e.type!=='input') throw new Error('event');
            document.getElementById('marker').textContent=e.target.value;
            return {get then(){throw new Error('then getter');},get toJSON(){throw new Error('json getter');},approved:true};
        });
        document.getElementById('go').addEventListener('click',function(e){
            document.getElementById('marker').textContent='native click';
        });
        document.querySelector=function(){throw new Error('hijacked selector');};
        document.querySelectorAll=document.querySelector;
        Event=function(){throw new Error('hijacked Event');};
        MouseEvent=Event; InputEvent=Event;
        Function.prototype.call=function(){throw new Error('hijacked call');};
        Object.prototype.dispatchEvent=function(){throw new Error('hijacked dispatch');};
        Object.prototype.click=Object.prototype.dispatchEvent;
        Object.prototype.value='wrong';
    "#).await;
    let t = target(&mut rt, "#name").await;
    let receipt = rt
        .native_act(t, NativeInput::Fill("native fill".into()))
        .await
        .unwrap();
    assert_eq!(receipt.callbacks, 1);
    let snap = rt.native_observe("").await.unwrap().snapshot;
    assert_eq!(
        snap.text_content(snap.query_selector("#marker").unwrap())
            .unwrap(),
        "native fill"
    );
    let t = target(&mut rt, "#go").await;
    assert_eq!(
        rt.native_act(t, NativeInput::Click)
            .await
            .unwrap()
            .callbacks,
        1
    );
    let snap = rt.native_observe("").await.unwrap().snapshot;
    assert_eq!(
        snap.text_content(snap.query_selector("#marker").unwrap())
            .unwrap(),
        "native click"
    );
}

#[tokio::test]
async fn duplicate_ids_use_native_node_identity() {
    let mut rt = JsRuntime::new();
    rt.set_document("<h1 id='marker'>before</h1><button id='same'>First</button><button id='same'>Second</button>",None,(640,480)).await.unwrap();
    eval(&mut rt,"document.querySelectorAll('#same')[1].addEventListener('click',function(){document.getElementById('marker').textContent='second';});").await;
    let obs = rt.native_observe("").await.unwrap();
    let el = obs
        .snapshot
        .interactive_elements()
        .into_iter()
        .find(|el| el.text == "Second")
        .unwrap();
    let t = obs.target(el.node_id).unwrap();
    rt.native_act(t, NativeInput::Click).await.unwrap();
    let r = rt
        .evaluate("document.getElementById('marker').textContent")
        .await
        .unwrap();
    assert_eq!(r.value, Some(json!("second")));
}

#[tokio::test]
async fn detached_replacement_and_cross_document_refs_fail_before_callbacks() {
    let mut rt = setup().await;
    eval(&mut rt,"document.getElementById('go').addEventListener('click',function(){throw new Error('old listener');});").await;
    let t = target(&mut rt, "#go").await;
    eval(&mut rt,"document.getElementById('go').remove();var b=document.createElement('button');b.setAttribute('id','go');b.textContent='Go';document.body.appendChild(b);").await;
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        false,
        "stale ref",
    );
    // Reused slab IDs must not inherit removed-node listeners.
    let fresh = target(&mut rt, "#go").await;
    assert_eq!(
        rt.native_act(fresh, NativeInput::Click)
            .await
            .unwrap()
            .callbacks,
        0
    );
    let t = target(&mut rt, "#go").await;
    rt.set_document(HTML, None, (640, 480)).await.unwrap();
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        false,
        "stale ref",
    );
    let fresh = target(&mut rt, "#go").await;
    assert_eq!(
        rt.native_act(fresh, NativeInput::Click)
            .await
            .unwrap()
            .callbacks,
        0
    );
}

#[tokio::test]
async fn tree_edits_are_conservatively_stale_and_due_timer_precedes_validation() {
    let mut rt = setup().await;
    let t = target(&mut rt, "#go").await;
    eval(
        &mut rt,
        "document.body.appendChild(document.getElementById('go'));",
    )
    .await;
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        false,
        "stale ref",
    );
    rt.set_document(HTML, None, (640, 480)).await.unwrap();
    eval(
        &mut rt,
        "setTimeout(function(){document.getElementById('go').remove();},100);",
    )
    .await;
    let t = target(&mut rt, "#go").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        false,
        "stale ref",
    );
}

#[tokio::test]
async fn fill_detach_between_input_and_change_is_uncertain() {
    let mut rt = setup().await;
    eval(&mut rt,"document.getElementById('name').addEventListener('input',function(e){e.target.remove();});document.getElementById('name').addEventListener('change',function(){throw new Error('change ran');});").await;
    let t = target(&mut rt, "#name").await;
    failure(
        rt.native_act(t, NativeInput::Fill("fixture".into()))
            .await
            .unwrap_err(),
        true,
        "DETACHED_DURING_ACTION",
    );
}

#[tokio::test]
async fn click_navigation_is_typed_and_queued_navigation_blocks_old_action() {
    let mut rt = setup().await;
    eval(&mut rt,"document.getElementById('go').addEventListener('click',function(){location.href='http://fixture.invalid/next';return {approved:true};});").await;
    let t = target(&mut rt, "#go").await;
    let receipt = rt.native_act(t.clone(), NativeInput::Click).await.unwrap();
    assert!(receipt.navigation_requested);
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        false,
        "stale ref",
    );
    assert_eq!(rt.drain_mutations().len(), 1);
}

#[tokio::test]
async fn listener_error_does_not_read_page_exception_and_reports_partial_effect() {
    let mut rt = setup().await;
    eval(&mut rt,"document.getElementById('name').addEventListener('input',function(){throw {get toString(){throw new Error('forged');},approved:true};});").await;
    let t = target(&mut rt, "#name").await;
    failure(
        rt.native_act(t, NativeInput::Fill("already changed".into()))
            .await
            .unwrap_err(),
        true,
        "NATIVE_LISTENER_ERROR",
    );
    let snap = rt.native_observe("").await.unwrap().snapshot;
    assert_eq!(
        snap.nodes[&snap.query_selector("#name").unwrap()].attributes["value"],
        "already changed"
    );
}

#[tokio::test]
async fn denied_control_changes_fail_before_write_and_callback_budget_is_bounded() {
    let mut rt = setup().await;
    let t = target(&mut rt, "#name").await;
    eval(
        &mut rt,
        "document.getElementById('name').setAttribute('type','PASSWORD');",
    )
    .await;
    failure(
        rt.native_act(t, NativeInput::Fill("denied".into()))
            .await
            .unwrap_err(),
        false,
        "stale ref",
    );
    let t = target(&mut rt, "#name").await;
    failure(
        rt.native_act(t, NativeInput::Fill("denied".into()))
            .await
            .unwrap_err(),
        false,
        "CONTROL_DENIED",
    );
    eval(&mut rt,"for(var i=0;i<257;i++){document.getElementById('go').addEventListener('click',function(){});}").await;
    let t = target(&mut rt, "#go").await;
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        true,
        "LISTENER_LIMIT",
    );
}

#[tokio::test]
async fn detached_reinserted_node_needs_fresh_ref_but_preserves_listener() {
    let mut rt = setup().await;
    eval(&mut rt,"var saved=document.getElementById('go');saved.addEventListener('click',function(){document.getElementById('marker').textContent='reattached';});").await;
    let old = target(&mut rt, "#go").await;
    eval(&mut rt, "saved.remove();document.body.appendChild(saved);").await;
    failure(
        rt.native_act(old, NativeInput::Click).await.unwrap_err(),
        false,
        "stale ref",
    );
    let fresh = target(&mut rt, "#go").await;
    assert_eq!(
        rt.native_act(fresh, NativeInput::Click)
            .await
            .unwrap()
            .callbacks,
        1
    );
}

#[tokio::test]
async fn detach_and_reinsert_inside_input_is_still_uncertain() {
    let mut rt = setup().await;
    eval(&mut rt,"document.getElementById('name').addEventListener('input',function(e){e.target.remove();document.body.appendChild(e.target);});").await;
    let t = target(&mut rt, "#name").await;
    failure(
        rt.native_act(t, NativeInput::Fill("fixture".into()))
            .await
            .unwrap_err(),
        true,
        "DETACHED_DURING_ACTION",
    );
}

#[tokio::test]
async fn event_controls_are_host_backed_and_microtask_navigation_is_in_receipt() {
    let mut rt = setup().await;
    eval(&mut rt,r#"
        document.getElementById('go').addEventListener('click',function(e){
            e.preventDefault();
            if(!e.defaultPrevented) throw new Error('cancel state');
            e.isTrusted=true;
            if(e.isTrusted!==false) throw new Error('forged trust');
            e.stopImmediatePropagation();
            Promise.resolve().then(function(){location.href='http://fixture.invalid/next';});
        });
        document.getElementById('go').addEventListener('click',function(){throw new Error('second listener');});
        document.body.addEventListener('click',function(){throw new Error('bubbled listener');});
    "#).await;
    let t = target(&mut rt, "#go").await;
    let r = rt.native_act(t, NativeInput::Click).await.unwrap();
    assert_eq!(r.callbacks, 1);
    assert!(r.navigation_requested);
}

#[tokio::test]
async fn callback_queued_microtasks_cannot_extend_native_command_without_bound() {
    let mut rt = setup().await;
    eval(&mut rt,"document.getElementById('go').addEventListener('click',function(){function again(){Promise.resolve().then(again);}again();});").await;
    let t = target(&mut rt, "#go").await;
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        true,
        "JOB_FAILED_OR_LIMIT",
    );
}

#[tokio::test]
async fn attribute_fingerprint_delimiter_collision_still_rejects_drift() {
    let mut rt = setup().await;
    eval(
        &mut rt,
        "document.getElementById('go').setAttribute('data-a','v,data-b=w');",
    )
    .await;
    let t = target(&mut rt, "#go").await;
    // These attribute sets have the same legacy comma-joined fingerprint.
    eval(&mut rt,"document.getElementById('go').setAttribute('data-a','v');document.getElementById('go').setAttribute('data-b','w');").await;
    failure(
        rt.native_act(t, NativeInput::Click).await.unwrap_err(),
        false,
        "stale ref",
    );
}
