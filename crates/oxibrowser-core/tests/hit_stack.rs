use oxibrowser_core::js::runtime::JsRuntime;
use serde_json::{Value, json};

async fn eval(rt: &mut JsRuntime, code: &str) -> Value {
    let result = rt.evaluate(code).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    result.value.unwrap_or(Value::Null)
}

async fn runtime() -> JsRuntime {
    let mut rt = JsRuntime::new();
    rt.set_document(
        r#"<style>
      body { margin: 0; }
      #host { position: relative; width: 200px; height: 200px; }
      #back, #front { position: absolute; left: 10px; top: 10px; width: 100px; height: 60px; }
      #back { z-index: 1; } #front { z-index: 2; }
    </style><div id="host"><button id="front">front</button><button id="back">back</button></div>"#,
        None,
        (320, 240),
    )
    .await
    .unwrap();
    rt
}

#[tokio::test]
async fn overlapping_hit_stack_uses_paint_order_and_stable_wrappers() {
    let mut rt = runtime().await;
    assert_eq!(eval(&mut rt, r#"(()=>{
      const front=document.getElementById('front'),back=document.getElementById('back');
      front.marker=42;const stack=document.elementsFromPoint(30,30);
      return [stack.map(e=>e.id||e.tagName.toLowerCase()),stack[0]===front,stack[1]===back,
        stack[0].marker,document.elementFromPoint(30,30)===stack[0],new Set(stack).size===stack.length];
    })()"#).await, json!([["front","back","host","body","html"],true,true,42,true,true]));
}

#[tokio::test]
async fn hit_stack_tracks_visibility_display_and_pointer_events_mutations() {
    let mut rt = runtime().await;
    assert_eq!(
        eval(
            &mut rt,
            r#"(()=>{
      const front=document.getElementById('front'),out=[];
      const top=()=>document.elementsFromPoint(30,30)[0].id;
      front.style.pointerEvents='none';out.push(top());
      front.style.pointerEvents='auto';front.style.visibility='hidden';out.push(top());
      front.style.visibility='visible';front.style.display='none';out.push(top());
      front.style.display='block';out.push(top());return out;
    })()"#
        )
        .await,
        json!(["back", "back", "back", "front"])
    );
}

#[tokio::test]
async fn hit_stack_rejects_points_outside_viewport() {
    let mut rt = runtime().await;
    assert_eq!(
        eval(
            &mut rt,
            r#"[[-1,20],[20,-1],[320,20],[20,240],[Infinity,20],[NaN,20]].map(p=>[
      document.elementsFromPoint(...p).length,document.elementFromPoint(...p)===null])"#
        )
        .await,
        json!([
            [0, true],
            [0, true],
            [0, true],
            [0, true],
            [0, true],
            [0, true]
        ])
    );
}

#[tokio::test]
async fn checkbox_inside_inline_label_is_its_content_quad_hit_target() {
    let mut rt = JsRuntime::new();
    rt.set_document(
        include_str!("../../../tests/spa/fixtures/vanilla.html"),
        None,
        (1280, 720),
    )
    .await
    .unwrap();
    assert_eq!(eval(&mut rt, r#"(()=>{
      const checkbox=document.getElementById('agree'),label=checkbox.parentElement;
      const rect=checkbox.getBoundingClientRect(),x=(rect.left+rect.right)/2,y=(rect.top+rect.bottom)/2;
      return [rect.width>0,rect.height>0,document.elementsFromPoint(x,y)[0]===checkbox,
        document.elementFromPoint(x,y)===checkbox,checkbox.parentElement===label,label.tagName];
    })()"#).await, json!([true,true,true,true,true,"LABEL"]));
}
