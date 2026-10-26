//! Actual input-generated DOM events on local documents, without CDP or GPU.
use oxibrowser_core::js::input::js_dispatch_key_event;
use oxibrowser_core::js::runtime::JsRuntime;
use serde_json::{Value, json};

async fn setup() -> JsRuntime {
    let mut runtime = JsRuntime::new();
    runtime
        .set_document(
            "<html><body><div id='parent'><input id='field' value='abc'></div></body></html>",
            None,
            (640, 480),
        )
        .await
        .unwrap();
    runtime
}

async fn evaluate(runtime: &mut JsRuntime, source: &str) -> Value {
    let result = runtime.evaluate(source).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    result.value.unwrap_or(Value::Null)
}

#[tokio::test]
async fn cdp_key_names_reach_body_document_and_window_in_dom_order() {
    let mut runtime = setup().await;
    evaluate(&mut runtime,r#"
      globalThis.keys=[];
      for(const [name,target] of [['body',document.body],['html',document.documentElement],['document',document],['window',window]]) {
        for(const type of ['keydown','keyup'])target.addEventListener(type,function(event){
          keys.push([name,event.type,event.key,event.code,event.target.tagName,this===event.currentTarget,event.eventPhase,event.shiftKey,event.ctrlKey]);
        });
      }
    "#).await;
    for kind in ["keyDown", "rawKeyDown", "keyUp"] {
        evaluate(
            &mut runtime,
            &js_dispatch_key_event("w", "KeyW", kind, 12, 42.0),
        )
        .await;
    }
    let observed = evaluate(&mut runtime, "JSON.stringify(keys)").await;
    let expected: Vec<Value> = ["keydown", "keydown", "keyup"]
        .into_iter()
        .flat_map(|kind| {
            ["body", "html", "document", "window"]
                .into_iter()
                .enumerate()
                .map(move |(index, name)| {
                    json!([
                        name,
                        kind,
                        "w",
                        "KeyW",
                        "BODY",
                        true,
                        if index == 0 { 2 } else { 3 },
                        true,
                        true
                    ])
                })
        })
        .collect();
    assert_eq!(observed, json!(serde_json::to_string(&expected).unwrap()));
}

#[tokio::test]
async fn keypress_inserts_at_zero_and_prevent_default_cancels_printable_text() {
    let mut runtime = setup().await;
    evaluate(&mut runtime,r#"
      globalThis.field=document.getElementById('field');
      Object.defineProperty(document,'activeElement',{configurable:true,value:field});
      field.selectionStart=field.selectionEnd=0;globalThis.inputEvents=[];
      field.addEventListener('keypress',event=>{inputEvents.push(event.type);if(event.key==='x')event.preventDefault();});
      field.addEventListener('input',event=>inputEvents.push(event.type));
      field.addEventListener('change',event=>inputEvents.push(event.type));
    "#).await;
    evaluate(
        &mut runtime,
        &js_dispatch_key_event("😀", "", "char", 0, 0.0),
    )
    .await;
    assert_eq!(
        evaluate(
            &mut runtime,
            "JSON.stringify([field.value,field.selectionStart,inputEvents])"
        )
        .await,
        json!(r#"["😀abc",2,["keypress","input","change"]]"#)
    );
    evaluate(
        &mut runtime,
        &js_dispatch_key_event("x", "KeyX", "char", 0, 0.0),
    )
    .await;
    assert_eq!(
        evaluate(
            &mut runtime,
            "JSON.stringify([field.value,field.selectionStart,inputEvents])"
        )
        .await,
        json!(r#"["😀abc",2,["keypress","input","change","keypress"]]"#)
    );
}

#[tokio::test]
async fn fallback_targets_and_document_dispatch_preserve_cancellation_and_identity() {
    let mut runtime = setup().await;
    evaluate(&mut runtime,r#"
      Object.defineProperty(document,'activeElement',{configurable:true,value:null});
      globalThis.events=[];
      document.addEventListener('keydown',function(event){events.push(['document',event.target.tagName||event.target.nodeName,this===event.currentTarget]);});
      window.addEventListener('keydown',function(event){events.push(['window',event.target.tagName||event.target.nodeName,this===event.currentTarget]);});
    "#).await;
    evaluate(
        &mut runtime,
        &js_dispatch_key_event("w", "KeyW", "keyDown", 0, 0.0),
    )
    .await;
    evaluate(&mut runtime,r#"Object.defineProperty(document,'body',{configurable:true,value:null});Object.defineProperty(document,'documentElement',{configurable:true,value:null});"#).await;
    evaluate(
        &mut runtime,
        &js_dispatch_key_event("w", "KeyW", "keyDown", 0, 0.0),
    )
    .await;
    assert_eq!(
        evaluate(&mut runtime, "JSON.stringify(events)").await,
        json!(
            r##"[["document","BODY",true],["window","BODY",true],["document","#document",true],["window","#document",true]]"##
        )
    );
}

#[tokio::test]
async fn propagation_stops_and_non_bubbling_events_do_not_duplicate_or_escape() {
    let mut runtime = setup().await;
    let result=evaluate(&mut runtime,r#"(()=>{
      const result=[];
      for(const kind of ['stopped','immediate','ancestor','ancestor-immediate','local']) {
        const seen=[];
        document.body.addEventListener(kind,function(event){seen.push('first');if(kind==='stopped')event.stopPropagation();if(kind==='immediate')event.stopImmediatePropagation();});
        document.body.addEventListener(kind,()=>seen.push('second'));
        document.documentElement.addEventListener(kind,function(event){seen.push('ancestor-first');if(kind==='ancestor')event.stopPropagation();if(kind==='ancestor-immediate')event.stopImmediatePropagation();});
        document.documentElement.addEventListener(kind,()=>seen.push('ancestor-second'));
        document.addEventListener(kind,()=>seen.push('document'));
        window.addEventListener(kind,()=>seen.push('window'));
        const event=new Event(kind,{bubbles:kind!=='local',cancelable:true});
        document.body.dispatchEvent(event);
        result.push([seen,event.currentTarget,event.eventPhase]);
      }
      return JSON.stringify(result);
    })()"#).await;
    assert_eq!(
        result,
        json!(
            r#"[[["first","second"],null,0],[["first"],null,0],[["first","second","ancestor-first","ancestor-second"],null,0],[["first","second","ancestor-first"],null,0],[["first","second"],null,0]]"#
        )
    );
}

#[tokio::test]
async fn detached_events_stay_local_and_direct_window_events_keep_window_identity() {
    let mut runtime = setup().await;
    let result=evaluate(&mut runtime,r#"(()=>{
      const seen=[],detached=document.createElement('div');
      detached.addEventListener('detached',()=>seen.push('target'));
      document.addEventListener('detached',()=>seen.push('document'));
      window.addEventListener('detached',()=>seen.push('window'));
      detached.dispatchEvent(new Event('detached',{bubbles:true}));
      window.addEventListener('direct',function(event){seen.push(this===window&&event.target===window&&event.currentTarget===window);event.preventDefault();});
      const event=new Event('direct',{bubbles:true,cancelable:true});
      const accepted=window.dispatchEvent(event);
      return JSON.stringify([seen,accepted,event.currentTarget,event.eventPhase]);
    })()"#).await;
    assert_eq!(result, json!(r#"[["target",true],false,null,0]"#));
}
