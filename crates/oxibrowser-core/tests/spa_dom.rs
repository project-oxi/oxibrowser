use oxibrowser_core::js::runtime::JsRuntime;
use serde_json::{Value, json};
async fn runtime(html: &str) -> JsRuntime {
    let mut rt = JsRuntime::new();
    rt.set_document(html, Some("http://fixture.invalid/"), (640, 480))
        .await
        .unwrap();
    rt
}
async fn eval(rt: &mut JsRuntime, script: &str) -> Value {
    let result = rt.evaluate(script).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    result.value.unwrap_or(Value::Null)
}
#[tokio::test]
async fn stable_wrappers_preserve_expandos_tree_identity_and_events() {
    let mut rt = runtime("<div id='parent'><button id='child'>hello</button></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{
 const p=document.getElementById('parent'),c=document.getElementById('child');c.marker=42;
 let target=false,current=false;c.addEventListener('click',e=>{target=e.target===c;current=e.currentTarget===c;});
 c.click();
 return [c===document.querySelector('#child'),p.firstChild===c,c.parentNode===p,c.marker,target,current];
 })()"#).await,json!([true,true,true,42,true,true]));
}
#[tokio::test]
async fn detached_nodes_reinsert_with_identity_and_state() {
    let mut rt = runtime("<div id='parent'><button id='child'>hello</button></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const p=document.getElementById('parent'),c=p.firstChild;c.marker=42;c.remove();c.textContent='detached';p.appendChild(c);return [p.firstChild===c,c.marker,c.textContent];})()"#).await,json!([true,42,"detached"]));
}
#[tokio::test]
async fn navigation_does_not_retarget_old_wrappers() {
    let mut rt = runtime("<button id='same'>old</button>").await;
    eval(
        &mut rt,
        "globalThis.oldNode=document.getElementById('same'); globalThis.oldDocument=document;",
    )
    .await;
    rt.set_document(
        "<button id='same'>new</button>",
        Some("http://fixture.invalid/next"),
        (640, 480),
    )
    .await
    .unwrap();
    assert_eq!(eval(&mut rt,"oldNode.textContent='retained';[oldNode===document.getElementById('same'),oldNode.textContent,document.getElementById('same').textContent,oldNode.ownerDocument===oldDocument]").await,json!([false,"retained","new",true]));
}
#[tokio::test]
async fn realm_and_dom_interfaces_are_distinct() {
    let mut rt = runtime("<input id='i'><iframe id='f'></iframe>").await;
    assert_eq!(eval(&mut rt,r#"[window===globalThis,self===window,document.defaultView===window,window instanceof Window,document instanceof Document,document instanceof Element,document.getElementById('i') instanceof HTMLInputElement,document.getElementById('i') instanceof HTMLElement,document.getElementById('i') instanceof HTMLIFrameElement,document.getElementById('f') instanceof HTMLIFrameElement,({}) instanceof Element]"#).await,json!([true,true,true,true,true,false,true,true,false,true,false]));
}
#[tokio::test]
async fn event_phases_options_removal_and_cancellation() {
    let mut rt = runtime("<div id='p'><button id='c'>go</button></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{
 const p=document.getElementById('p'),c=document.getElementById('c'),out=[];
 window.addEventListener('test',()=>out.push('window-capture'),true);
 document.addEventListener('test',()=>out.push('document-capture'),true);
 p.addEventListener('test',e=>out.push('parent:'+e.eventPhase),true);
 c.addEventListener('test',e=>out.push('target-capture:'+e.eventPhase),true);
 c.addEventListener('test',e=>{out.push('target:'+e.eventPhase);e.preventDefault();},{once:true});
 p.addEventListener('test',e=>out.push('bubble:'+e.eventPhase));
 const removed=()=>out.push('removed');c.addEventListener('test',removed);c.removeEventListener('test',removed);
 const first=c.dispatchEvent(new Event('test',{bubbles:true,cancelable:true}));
 c.dispatchEvent(new Event('test',{bubbles:true}));
 return [out,first];})()"#).await,json!([["window-capture","document-capture","parent:1","target-capture:2","target:2","bubble:3","window-capture","document-capture","parent:1","target-capture:2","bubble:3"],false]));
}
#[tokio::test]
async fn propagation_and_non_cancelable_events() {
    let mut rt = runtime("<div id='p'><button id='c'>go</button></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const p=document.getElementById('p'),c=p.firstChild,out=[];p.addEventListener('test',()=>out.push('parent'));c.addEventListener('test',e=>{out.push(1);e.stopPropagation();e.preventDefault();});c.addEventListener('test',()=>out.push(2));const e=new Event('test',{bubbles:true});return [c.dispatchEvent(e),out,e.defaultPrevented,e.currentTarget,e.eventPhase];})()"#).await,json!([true,[1,2],false,null,0]));
}
#[tokio::test]
async fn focus_tracks_element_and_text_nodes_are_live() {
    let mut rt = runtime("<input id='i'><div id='out'></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const i=document.getElementById('i'),out=document.getElementById('out'),events=[];i.addEventListener('focus',()=>events.push('focus'));i.addEventListener('blur',()=>events.push('blur'));i.focus();const active=document.activeElement===i;i.blur();const t=document.createTextNode('a');out.appendChild(t);t.nodeValue='b';return [active,document.activeElement===document.body,events,t===out.firstChild,t instanceof Text,out.textContent];})()"#).await,json!([true,true,["focus","blur"],true,true,"b"]));
}
#[tokio::test]
async fn computed_style_reads_renderer_and_dynamic_styles() {
    let mut rt=runtime("<style>#box{width:123px;height:45px;color:rgb(12,34,56);background-color:rgb(1,2,3)}</style><div id='box'></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const el=document.getElementById('box'),s=getComputedStyle(el);const before=[s.width,s.color,s.getPropertyValue('background-color'),s.backgroundColor];el.style.width='200px';return [before,s.width,getComputedStyle(el).width];})()"#).await,json!([["123px","rgb(12, 34, 56)","rgb(1, 2, 3)","rgb(1, 2, 3)"],"200px","200px"]));
}
#[tokio::test]
async fn native_input_uses_capture_once_removal_and_stable_event_identity() {
    use oxibrowser_core::js::runtime::NativeInput;
    let mut rt = runtime("<div id='p'><button id='c'>go</button></div>").await;
    eval(&mut rt,r#"globalThis.log=[];globalThis.savedEvent=null;
 const p=document.getElementById('p'),c=document.getElementById('c');
 window.addEventListener('click',()=>log.push('window'),true);
 document.addEventListener('click',()=>log.push('document'),true);
 p.addEventListener('click',e=>{savedEvent=e;log.push('capture:'+e.eventPhase);},true);
 c.addEventListener('click',e=>log.push('once:'+e.eventPhase+':'+(e.target===c)+':'+(e.currentTarget===c)+':'+(e===savedEvent)),{once:true});
 const remove=()=>log.push('removed');c.addEventListener('click',remove);c.removeEventListener('click',remove);
 p.addEventListener('click',e=>log.push('bubble:'+e.eventPhase));"#).await;
    for _ in 0..2 {
        let observation = rt.native_observe("http://fixture.invalid/").await.unwrap();
        let target = observation
            .target(observation.snapshot.query_selector("#c").unwrap())
            .unwrap();
        rt.native_act(target, NativeInput::Click).await.unwrap();
    }
    assert_eq!(
        eval(
            &mut rt,
            "[log,savedEvent.currentTarget,savedEvent.eventPhase]"
        )
        .await,
        json!([
            [
                "window",
                "document",
                "capture:1",
                "once:2:true:true:true",
                "bubble:3",
                "window",
                "document",
                "capture:1",
                "bubble:3"
            ],
            null,
            0
        ])
    );
}
#[tokio::test]
async fn removed_during_dispatch_and_once_reentrancy_follow_listener_records() {
    let mut rt = runtime("<button id='c'>go</button>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const c=document.getElementById('c'),log=[];const later=()=>log.push('late');c.addEventListener('test',()=>{log.push('first');c.removeEventListener('test',later);},{once:true});c.addEventListener('test',later);c.addEventListener('test',()=>{log.push('once');c.dispatchEvent(new Event('test'));},{once:true});c.dispatchEvent(new Event('test'));return log;})()"#).await,json!(["first","once"]));
}

/// Optional local framework diagnosis. Real pinned artifacts are supplied by
/// the SPA fixture suite; this helper does not substitute mocked frameworks.
#[tokio::test]
#[ignore = "requires the local audit React 18.2 artifacts"]
async fn real_react18_counter_mounts_and_clicks() {
    let mut rt = runtime("<div id='root'></div>").await;
    for path in [
        "/tmp/oxi-browser-audit-20261027/react.js",
        "/tmp/oxi-browser-audit-20261027/react-dom.js",
    ] {
        let script = std::fs::read_to_string(path).unwrap();
        eval(&mut rt, &script).await;
    }
    eval(&mut rt,r#"const root=ReactDOM.createRoot(document.getElementById('root'));function App(){const [n,set]=React.useState(0);return React.createElement('button',{id:'counter',onClick:()=>set(n+1)},'count:'+n);}ReactDOM.flushSync(()=>root.render(React.createElement(App)));"#).await;
    assert_eq!(
        eval(&mut rt, "document.getElementById('counter').textContent").await,
        json!("count:0")
    );
    eval(
        &mut rt,
        "ReactDOM.flushSync(()=>document.getElementById('counter').click())",
    )
    .await;
    assert_eq!(
        eval(&mut rt, "document.getElementById('counter').textContent").await,
        json!("count:1")
    );
}
#[tokio::test]
async fn isolated_world_shares_live_dom_but_keeps_globals_and_wrappers_separate() {
    let mut rt = runtime("<div id='out'>main</div>").await;
    eval(
        &mut rt,
        "globalThis.pageOnly=42;document.getElementById('out').marker='page';",
    )
    .await;
    rt.create_isolated_world(100, 1).await.unwrap();
    let result=rt.evaluate_in_context("globalThis.worldOnly=7;const el=document.getElementById('out');el.textContent='utility';[typeof pageOnly,el.marker===undefined,el===document.querySelector('#out'),window===globalThis]",100,false).await.unwrap();
    assert!(result.exception.is_none(), "{result:?}");
    assert_eq!(result.value, json!(["undefined", true, true, true]).into());
    assert_eq!(eval(&mut rt,"[document.getElementById('out').textContent,typeof worldOnly,document.getElementById('out').marker]").await,json!(["utility","undefined","page"]));
    rt.set_document("<div id='out'>next</div>", None, (640, 480))
        .await
        .unwrap();
    assert!(rt.evaluate_in_context("1", 100, false).await.is_err());
}
#[tokio::test]
async fn remote_commands_use_owner_origin_for_frame_and_utility_fetches() {
    use std::sync::mpsc;
    let mut rt = runtime("<p>main</p>").await;
    let (tx, rx) = mpsc::channel();
    let (_responses, response_rx) = mpsc::channel();
    rt.set_fetch_channel(tx, response_rx);
    rt.set_frame_document(
        2,
        "<p>child</p>",
        "http://child.invalid/page",
        (640, 480),
        vec![],
    )
    .await
    .unwrap();
    rt.create_isolated_world(100, 2).await.unwrap();
    for (context, origin, url) in [
        (2, "http://child.invalid", "http://child.invalid/api"),
        (100, "http://child.invalid", "http://child.invalid/utility"),
        (1, "http://fixture.invalid", "http://fixture.invalid/main"),
    ] {
        rt.remote_command(context,json!({"method":"evaluate","params":{"expression":format!("fetch('{url}');'sent'"),"returnByValue":true}})).await.unwrap();
        let request = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(request.origin.as_deref(), Some(origin));
        assert_eq!(request.url, url);
    }
}
#[tokio::test]
async fn live_tree_removal_replacement_and_insert_before_null() {
    let mut rt = runtime("<div id='p'><i id='a'>a</i><i id='b'>b</i></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const p=document.getElementById('p'),a=p.firstChild,b=p.lastChild,c=document.createElement('strong');c.textContent='c';const removed=p.removeChild(a);p.insertBefore(a,null);const replaced=p.replaceChild(c,b);return [removed===a,replaced===b,p.firstChild===c,p.lastChild===a,p.textContent,a.parentNode===p,b.parentNode===null];})()"#).await,json!([true,true,true,true,"ca",true,true]));
}
#[tokio::test]
async fn native_fill_updates_focus_and_value_before_input_events() {
    use oxibrowser_core::js::runtime::NativeInput;
    let mut rt = runtime("<input id='i'>").await;
    eval(&mut rt,"globalThis.log=[];const i=document.getElementById('i');i.addEventListener('focus',()=>log.push('focus'));i.addEventListener('input',e=>log.push([document.activeElement===i,e.target===i,e.target.value]));").await;
    let observation = rt.native_observe("http://fixture.invalid/").await.unwrap();
    let target = observation
        .target(observation.snapshot.query_selector("#i").unwrap())
        .unwrap();
    rt.native_act(target, NativeInput::Fill("typed".into()))
        .await
        .unwrap();
    assert_eq!(
        eval(&mut rt, "log").await,
        json!(["focus", [true, true, "typed"]])
    );
}
#[tokio::test]
#[ignore = "requires npm ci in tests/spa for real pinned Vue 3.5.13"]
async fn real_vue35_counter_list_and_conditional() {
    let mut rt = runtime("<div id='root'></div>").await;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/spa/node_modules/vue/dist/vue.global.prod.js");
    eval(&mut rt, &std::fs::read_to_string(path).unwrap()).await;
    eval(&mut rt,r#"globalThis.state=Vue.reactive({count:0,items:[],show:false});Vue.createApp({setup(){return{state}},template:'<div><button id="counter" @click="state.count++">count:{{state.count}}</button><ul><li v-for="it in state.items" :key="it">{{it}}</li></ul><p v-if="state.show" id="conditional">visible</p></div>'}).mount('#root');"#).await;
    assert_eq!(
        eval(&mut rt, "document.getElementById('counter').textContent").await,
        json!("count:0")
    );
    eval(&mut rt,"document.getElementById('counter').click();state.items.push('one');state.show=true;Vue.nextTick()").await;
    assert_eq!(eval(&mut rt,"[document.getElementById('counter').textContent,document.querySelector('li').textContent,document.getElementById('conditional').textContent]").await,json!(["count:1","one","visible"]));
    eval(&mut rt, "state.items=[];state.show=false;Vue.nextTick()").await;
    assert_eq!(
        eval(
            &mut rt,
            "[document.querySelector('li'),document.getElementById('conditional')]"
        )
        .await,
        json!([null, null])
    );
}
#[tokio::test]
async fn stale_and_forged_wrappers_cannot_alias_the_current_document() {
    let mut rt = runtime("<div id='old'>old</div>").await;
    eval(
        &mut rt,
        "globalThis.oldNode=document.getElementById('old');",
    )
    .await;
    rt.set_document("<div id='new'>new</div>", None, (640, 480))
        .await
        .unwrap();
    assert_eq!(eval(&mut rt,r#"(()=>{const p=document.body,n=document.getElementById('new'),result=[];for(const action of [()=>p.appendChild(oldNode),()=>p.insertBefore(oldNode,null),()=>p.replaceChild(oldNode,n),()=>p.appendChild({__nodeId:n.__nodeId})]){try{action();result.push('accepted')}catch(e){result.push('rejected')}}return [result,getComputedStyle(oldNode),getComputedStyle({__nodeId:n.__nodeId}),n.textContent,n.parentNode===p,p.contains(oldNode),n.contains(n)];})()"#).await,json!([["rejected","rejected","rejected","rejected"],null,null,"new",true,false,true]));
}
#[tokio::test]
async fn old_document_listener_registration_cannot_attach_to_new_document_nodes() {
    use oxibrowser_core::js::runtime::NativeInput;
    let mut rt = runtime("<button id='same'>old</button>").await;
    eval(&mut rt,"globalThis.oldNode=document.getElementById('same');globalThis.oldCalls=0;window.addEventListener('click',()=>oldCalls++);").await;
    rt.set_document("<button id='same'>new</button>", None, (640, 480))
        .await
        .unwrap();
    eval(&mut rt,"globalThis.newCalls=0;document.getElementById('same').addEventListener('click',()=>newCalls++);oldNode.addEventListener('click',()=>oldCalls++);").await;
    let observation = rt.native_observe("").await.unwrap();
    let target = observation
        .target(observation.snapshot.query_selector("#same").unwrap())
        .unwrap();
    rt.native_act(target, NativeInput::Click).await.unwrap();
    eval(&mut rt, "document.getElementById('same').click();").await;
    assert_eq!(eval(&mut rt, "[oldCalls,newCalls]").await, json!([0, 2]));
}
#[tokio::test]
async fn attributes_are_live_and_tree_walker_visits_native_nodes() {
    let mut rt = runtime("<div id='p' data-x='one'><span id='s'>txt</span></div>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const p=document.getElementById('p'),attrs=p.attributes;p.setAttribute('title','new');attrs.getNamedItem('data-x').value='two';const walker=document.createTreeWalker(p,NodeFilter.SHOW_ELEMENT|NodeFilter.SHOW_TEXT),out=[];while(walker.nextNode())out.push(walker.currentNode.nodeName);return[attrs===p.attributes,attrs.length,attrs.item(0).name,attrs.getNamedItem('missing'),p.getAttribute('data-x'),out];})()"#).await,json!([true,3,"id",null,"two",["SPAN","#text"]]));
}
#[tokio::test]
async fn event_listener_errors_are_observable_and_later_listeners_continue() {
    use oxibrowser_core::js::runtime::CoreEvent;
    let mut rt = runtime("<button id='c'>go</button>").await;
    let (tx, rx) = std::sync::mpsc::channel();
    rt.set_event_sink(tx);
    assert_eq!(eval(&mut rt,"let called=false;const c=document.getElementById('c');c.addEventListener('test',()=>{throw new Error('fixture listener failed')});c.addEventListener('test',()=>called=true);c.dispatchEvent(new Event('test'));called").await,json!(true));
    assert!(rx.try_iter().any(|event|matches!(event,CoreEvent::Exception{message,..} if message.contains("fixture listener failed"))));
}

#[tokio::test]
async fn required_and_custom_validity_dispatch_invalid_and_gate_submit() {
    let mut rt=runtime("<form id='form' action='/saved'><input id='name' name='name' required><button id='submit'>Save</button></form>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const input=document.getElementById('name'),form=document.getElementById('form');globalThis.invalid=[];globalThis.submits=0;form.addEventListener('invalid',e=>invalid.push([e.target===input,e.bubbles,e.cancelable]),true);form.addEventListener('submit',e=>{submits++;e.preventDefault()});document.getElementById('submit').click();const empty=[input.willValidate,input.validity.valueMissing,input.validity.valid,input.validationMessage.length>0,submits,invalid.slice()];input.value='Ada';const filled=input.checkValidity();input.setCustomValidity('Please choose another name');const custom=[input.validity.customError,input.validationMessage,input.checkValidity()];input.setCustomValidity('');document.getElementById('submit').click();return [empty,filled,custom,input.validity.valid,submits];})()"#).await,json!([[true,true,false,true,0,[[true,false,true]]],true,[true,"Please choose another name",false],true,1]));
    assert!(rt.drain_mutations().is_empty());
}
#[tokio::test]
async fn required_checkbox_radio_and_barred_controls_use_native_state() {
    let mut rt=runtime("<form><input id='box' type='checkbox' required><input id='r1' type='radio' name='pick' required><input id='r2' type='radio' name='pick'><input id='disabled' required disabled><input id='readonly' required readonly></form>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const box=document.getElementById('box'),a=document.getElementById('r1'),b=document.getElementById('r2');const before=[box.checkValidity(),a.checkValidity(),document.getElementById('disabled').willValidate,document.getElementById('disabled').checkValidity(),document.getElementById('readonly').willValidate];box.checked=true;b.checked=true;return[before,box.checkValidity(),a.checkValidity(),document.querySelector('form').checkValidity()];})()"#).await,json!([[false,false,false,true,false],true,true,true]));
}
#[tokio::test]
async fn native_submit_checks_required_without_firing_submit_on_invalid_form() {
    use oxibrowser_core::js::runtime::NativeInput;
    let mut rt =
        runtime("<form><input id='name' required><button id='submit'>Save</button></form>").await;
    eval(&mut rt,"globalThis.invalid=0;globalThis.submits=0;document.querySelector('form').addEventListener('invalid',()=>invalid++,true);document.querySelector('form').addEventListener('submit',e=>{submits++;e.preventDefault()});").await;
    for valid in [false, true] {
        if valid {
            eval(&mut rt, "document.getElementById('name').value='Ada'").await;
        }
        let observation = rt.native_observe("").await.unwrap();
        let target = observation
            .target(observation.snapshot.query_selector("#submit").unwrap())
            .unwrap();
        rt.native_act(target, NativeInput::Click).await.unwrap();
    }
    assert_eq!(eval(&mut rt, "[invalid,submits]").await, json!([1, 1]));
    assert!(rt.drain_mutations().is_empty());
}

#[tokio::test]
async fn checkbox_and_radio_activation_update_before_click_and_honor_cancel() {
    let mut rt=runtime("<input id='box' type='checkbox'><input id='a' type='radio' name='g' checked><input id='b' type='radio' name='g'>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const box=document.getElementById('box'),a=document.getElementById('a'),b=document.getElementById('b'),log=[];box.addEventListener('click',()=>log.push(['click',box.checked]));box.addEventListener('input',()=>log.push(['input',box.checked]));box.addEventListener('change',()=>log.push(['change',box.checked]));box.click();box.addEventListener('click',e=>e.preventDefault(),{once:true});box.click();b.click();return[box.checked,a.checked,b.checked,log];})()"#).await,json!([true,false,true,[["click",true],["input",true],["change",true],["click",false]]]));
}
#[tokio::test]
async fn input_type_defaults_and_event_handler_idl_reflect_real_dispatch() {
    let mut rt = runtime("<input id='i'><button id='b'>go</button>").await;
    assert_eq!(eval(&mut rt,r#"(()=>{const i=document.getElementById('i'),b=document.getElementById('b');let called=0;const defaults=[i.type,b.type,'oninput' in document,'oninput' in i,i.oninput];i.oninput=()=>called++;i.dispatchEvent(new Event('input'));i.oninput=null;i.dispatchEvent(new Event('input'));i.type='EMAIL';const email=i.type;i.type='not-real';b.type='not-real';return[defaults,called,email,i.type,i.getAttribute('type'),b.type];})()"#).await,json!([["text","submit",true,true,null],1,"email","text","not-real","submit"]));
}
#[tokio::test]
async fn anchor_activation_updates_hash_and_journals_normal_navigation() {
    let mut rt=runtime("<a id='hash' href='#/next'>next</a><a id='path' href='/next'>path</a><a id='blank' href='/new' target='_blank'>blank</a>").await;
    assert_eq!(eval(&mut rt,"globalThis.hashes=[];addEventListener('hashchange',e=>hashes.push([e.oldURL,e.newURL]));document.getElementById('hash').click();[location.hash,hashes.length,hashes[0][1]]").await,json!(["#/next",1,"http://fixture.invalid/#/next"]));
    assert!(rt.drain_mutations().is_empty());
    eval(
        &mut rt,
        "document.getElementById('path').click();document.getElementById('blank').click();",
    )
    .await;
    let mutations = rt.drain_mutations();
    assert!(mutations.iter().any(|m|matches!(m,oxibrowser_core::js::dom_snapshot::DomMutation::Navigate{url} if url=="http://fixture.invalid/next")));
    assert!(mutations.iter().any(|m|matches!(m,oxibrowser_core::js::dom_snapshot::DomMutation::OpenTab{url,..} if url=="http://fixture.invalid/new")));
}
#[tokio::test]
async fn history_authority_is_native_and_shared_with_utility_realms() {
    let mut rt = runtime("<p id='state'>live</p>").await;
    let first = rt
        .remote_command(1, json!({"method":"historySnapshot","params":{}}))
        .await
        .unwrap();
    assert_eq!(first["entries"].as_array().unwrap().len(), 1);
    rt.create_isolated_world(100, 1).await.unwrap();
    eval(&mut rt,"globalThis.events=[];addEventListener('popstate',e=>events.push(['pop',e.state]));addEventListener('hashchange',()=>events.push(['hash',location.hash]));history.pushState({page:1},'', '#one');history.pushState({page:2},'', '#two');globalThis.__oxiHistoryEntries=[{url:'https://forged.invalid/'}];globalThis.__oxiHistoryIndex=0;").await;
    let pushed = rt
        .remote_command(1, json!({"method":"historySnapshot","params":{}}))
        .await
        .unwrap();
    assert_eq!(pushed["entries"].as_array().unwrap().len(), 3);
    assert_eq!(pushed["url"], json!("http://fixture.invalid/#two"));
    let utility = rt
        .evaluate_in_context("[location.hash,history.length,document.URL]", 100, false)
        .await
        .unwrap();
    assert_eq!(
        utility.value,
        Some(json!(["#two", 3, "http://fixture.invalid/#two"]))
    );
    let back = rt
        .remote_command(1, json!({"method":"navigateHistory","params":{"index":1}}))
        .await
        .unwrap();
    assert_eq!(back["url"], json!("http://fixture.invalid/#one"));
    assert_eq!(
        eval(
            &mut rt,
            "[events,document.getElementById('state').textContent]"
        )
        .await,
        json!([[["pop",{"page":1}],["hash","#one"]],"live"])
    );
    let before = back["revision"].as_u64().unwrap();
    eval(&mut rt, "history.pushState({same:true},'',location.href)").await;
    let same = rt
        .remote_command(1, json!({"method":"historySnapshot","params":{}}))
        .await
        .unwrap();
    assert!(same["revision"].as_u64().unwrap() > before);
    assert!(rt.drain_mutations().is_empty());
}
#[tokio::test]
async fn history_state_is_snapshotted_and_cross_document_traversal_stays_pending() {
    let mut rt = runtime("<p>A</p>").await;
    assert_eq!(eval(&mut rt,"let state={nested:{value:1}};history.replaceState(state,'');state.nested.value=9;let rejected=false;try{history.pushState({fn:()=>1},'')}catch(e){rejected=String(e).includes('DataCloneError')}[history.state.nested.value,rejected]").await,json!([1,true]));
    rt.set_document("<p>B</p>", Some("http://other.invalid/"), (640, 480))
        .await
        .unwrap();
    let before = rt
        .remote_command(1, json!({"method":"historySnapshot","params":{}}))
        .await
        .unwrap();
    let pending = rt
        .remote_command(1, json!({"method":"navigateHistory","params":{"index":0}}))
        .await
        .unwrap();
    assert_eq!(pending["url"], json!("http://other.invalid/"));
    assert_eq!(pending["index"], before["index"]);
    assert_eq!(pending["pendingIndex"], json!(0));
    assert_eq!(eval(&mut rt, "history.state").await, Value::Null);
    assert!(rt.drain_mutations().iter().any(|m|matches!(m,oxibrowser_core::js::dom_snapshot::DomMutation::Navigate{url}if url=="http://fixture.invalid/")));
    rt.set_document(
        "<p>A again</p>",
        Some("http://fixture.invalid/"),
        (640, 480),
    )
    .await
    .unwrap();
    assert_eq!(
        eval(&mut rt, "[history.state.nested.value,location.href]").await,
        json!([1, "http://fixture.invalid/"])
    );
}
#[tokio::test]
async fn cross_origin_navigation_replaces_realm_and_restores_only_native_storage_seeds() {
    use oxibrowser_core::js::runtime::{FetchResponseMsg, LocalStorageMsg};
    use oxibrowser_core::storage_state::IdbDatabase;
    use std::collections::{BTreeMap, HashMap};
    let mut rt = runtime("<p>A</p>").await;
    let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
    let (response_tx, response_rx) = std::sync::mpsc::channel();
    rt.set_fetch_channel(fetch_tx, response_rx);
    let (ls_tx, ls_rx) = std::sync::mpsc::channel();
    rt.set_local_storage_channel(ls_tx);
    let (idb_tx, _idb_rx) = std::sync::mpsc::channel();
    rt.set_indexed_db_channel(idb_tx);
    rt.set_page_url("http://fixture.invalid/");
    eval(&mut rt,"globalThis.oldSecret='A';globalThis.oldFunction=()=>oldSecret;Object.prototype.originPoison='A';localStorage.setItem('token','A');setTimeout(()=>globalThis.oldTimerRan=true,10000);").await;
    let old=rt.remote_command(1,json!({"method":"evaluate","params":{"expression":"({origin:'A'})","returnByValue":false}})).await.unwrap();
    let old_id = old["result"]["objectId"].as_str().unwrap().to_owned();
    rt.remote_command(1,json!({"method":"evaluate","params":{"expression":"fetch('http://fixture.invalid/late').then(()=>globalThis.oldFetchRan=true);'queued'","returnByValue":true}})).await.unwrap();
    let late = fetch_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let databases = BTreeMap::from([(
        "seeded".into(),
        IdbDatabase {
            name: "seeded".into(),
            version: 7,
            stores: BTreeMap::new(),
            key_paths: BTreeMap::new(),
        },
    )]);
    rt.prepare_document_url_with_storage_seed_idb(
        "http://other.invalid/",
        Some(HashMap::from([("token".into(), "B-seed".into())])),
        Some(databases),
    );
    assert_eq!(
        eval(
            &mut rt,
            "[location.origin,localStorage.getItem('token'),oldSecret]"
        )
        .await,
        json!(["http://fixture.invalid", "A", "A"])
    );
    rt.set_document(
        "<input id='new'>",
        Some("http://other.invalid/"),
        (640, 480),
    )
    .await
    .unwrap();
    response_tx
        .send(FetchResponseMsg {
            id: late.id,
            status: 200,
            status_text: "OK".into(),
            url: late.url,
            headers: vec![],
            body: "late".into(),
            body_bytes: None,
            error: None,
        })
        .unwrap();
    assert_eq!(eval(&mut rt,"[typeof oldSecret,typeof oldFunction,typeof oldTimerRan,typeof oldFetchRan,Object.prototype.originPoison,localStorage.getItem('token'),document.getElementById('new').ownerDocument===document]").await,json!(["undefined","undefined","undefined","undefined",null,"B-seed",true]));
    assert!(
        rt.remote_command(
            1,
            json!({"method":"getProperties","params":{"objectId":old_id,"ownProperties":true}})
        )
        .await
        .is_err()
    );
    let database=rt.evaluate_with_await("new Promise(resolve=>{let request=indexedDB.open('seeded');request.onsuccess=()=>resolve(request.result.version);request.onerror=()=>resolve('error')})",true).await.unwrap();
    assert!(database.exception.is_none(), "{database:?}");
    assert_eq!(database.value, Some(json!(7)));
    eval(&mut rt, "localStorage.setItem('written','B')").await;
    assert!(ls_rx.try_iter().any(|message|matches!(message,LocalStorageMsg::SetItem{origin,key,value}if origin=="http://other.invalid"&&key=="written"&&value=="B")));
}
#[tokio::test]
async fn prepared_same_origin_reload_replaces_globals_but_preserves_native_history_and_seeds() {
    use std::collections::HashMap;
    let mut rt = runtime("<p>before</p>").await;
    rt.set_page_url("http://fixture.invalid/");
    eval(&mut rt,"const pageLexical=1;globalThis.pageGlobal='old';Object.prototype.pagePrototype='old';localStorage.setItem('setting','saved');history.replaceState({kept:7},'');").await;
    rt.create_isolated_world(100, 1).await.unwrap();
    let before = rt
        .remote_command(1, json!({"method":"historySnapshot","params":{}}))
        .await
        .unwrap();
    rt.prepare_document_url_with_storage_seed_idb(
        "http://fixture.invalid/",
        Some(HashMap::from([("setting".into(), "saved".into())])),
        None,
    );
    rt.set_document("<p>after</p>", Some("http://fixture.invalid/"), (640, 480))
        .await
        .unwrap();
    assert_eq!(eval(&mut rt,"const pageLexical=2;[pageLexical,typeof pageGlobal,Object.prototype.pagePrototype,localStorage.getItem('setting'),history.state.kept]").await,json!([2,"undefined",null,"saved",7]));
    assert!(rt.evaluate_in_context("1", 100, false).await.is_err());
    let after = rt
        .remote_command(1, json!({"method":"historySnapshot","params":{}}))
        .await
        .unwrap();
    assert_eq!(after["entries"][0]["id"], before["entries"][0]["id"]);
    assert_ne!(after["documentId"], before["documentId"]);
}
#[tokio::test]
async fn cross_document_replace_is_pending_and_accepts_cross_origin_redirect_result() {
    let mut rt = runtime("<p>A</p>").await;
    eval(&mut rt,"history.replaceState({current:'A'},'');history.pushState({current:'A2'},'','#two');location.replace('http://destination.invalid/requested');").await;
    assert_eq!(
        eval(
            &mut rt,
            "[location.href,history.state.current,history.length]"
        )
        .await,
        json!(["http://fixture.invalid/#two", "A2", 2])
    );
    assert!(rt.drain_mutations().iter().any(|m|matches!(m,oxibrowser_core::js::dom_snapshot::DomMutation::Navigate{url}if url=="http://destination.invalid/requested")));
    rt.prepare_document_url_with_storage_seed_idb("http://redirected.invalid/final", None, None);
    rt.set_document(
        "<p>B</p>",
        Some("http://redirected.invalid/final"),
        (640, 480),
    )
    .await
    .unwrap();
    assert_eq!(
        eval(&mut rt, "[location.href,history.state,history.length]").await,
        json!(["http://redirected.invalid/final", null, 2])
    );
}
#[tokio::test]
async fn history_json_snapshot_preserves_numeric_string_property_keys() {
    let mut rt = runtime("<p>state</p>").await;
    assert_eq!(eval(&mut rt,"let state={'0':'zero','12':{nested:'kept'},normal:'value'};history.pushState(state,'');state[0]='changed';[history.state[0],history.state[12].nested,history.state.normal]").await,json!(["zero","kept","value"]));
}
#[tokio::test]
async fn physical_child_navigation_is_explicitly_denied_and_never_journaled_to_root() {
    use oxibrowser_core::js::runtime::CoreEvent;
    let mut rt = runtime("<p id='top'>top</p><iframe id='frame' src='/child'></iframe>").await;
    let owner = rt.frame_owners(1).await.unwrap().remove(0);
    rt.set_owned_frame_document(2, owner,
        "<a id='physical' href='/next'>next</a><a id='hash' href='#child'>hash</a><form id='form' action='/submit'><button id='send'>send</button></form>",
        "http://fixture.invalid/child", (320, 240), vec![]).await.unwrap();
    rt.create_isolated_world(102, 2).await.unwrap();
    rt.create_isolated_world(101, 1).await.unwrap();
    rt.drain_mutations();
    let script = r#"(()=>{
      const operations=[()=>location.href='/next',()=>location.assign('/next'),()=>location.replace('/next'),()=>location.reload(),()=>history.go(0),()=>__oxiNavigate('/next'),()=>__oxiReload(),()=>window.open('/next'),()=>document.getElementById('physical').click(),()=>document.getElementById('form').submit(),()=>document.getElementById('send').click(),()=>__oxiFormSubmit(document.getElementById('form').__nodeId)];
      return operations.map(operation=>{try{operation();return 'allowed'}catch(error){return error.name}});
    })()"#;
    for context in [2, 102] {
        let result = rt
            .evaluate_in_context(script, context, false)
            .await
            .unwrap();
        assert!(result.exception.is_none(), "{result:?}");
        assert_eq!(result.value, Some(json!(vec!["NotSupportedError"; 12])));
    }
    assert!(
        rt.drain_mutations().is_empty(),
        "child effects leaked into root journal"
    );
    let hash=rt.evaluate_in_context("document.getElementById('hash').click();[location.href,document.getElementById('hash')===document.querySelector('#hash')]",2,false).await.unwrap();
    assert!(hash.exception.is_none(), "{hash:?}");
    assert_eq!(
        hash.value,
        Some(json!(["http://fixture.invalid/child#child", true]))
    );
    assert_eq!(
        eval(
            &mut rt,
            "[location.href,document.getElementById('top').textContent]"
        )
        .await,
        json!(["http://fixture.invalid/", "top"])
    );
    // Utility worlds of the top document retain legitimate navigation rights.
    let top = rt
        .evaluate_in_context("location.href='/top-next'", 101, false)
        .await
        .unwrap();
    assert!(top.exception.is_none(), "{top:?}");
    assert!(rt.drain_mutations().iter().any(|m|matches!(m,oxibrowser_core::js::dom_snapshot::DomMutation::Navigate{url}if url=="http://fixture.invalid/top-next")));
    let (tx, rx) = std::sync::mpsc::channel();
    rt.set_event_sink(tx);
    let denied = rt
        .evaluate_in_context("location.href='/uncaught'", 2, false)
        .await
        .unwrap();
    assert!(denied.exception.is_some());
    assert!(
        rx.try_iter()
            .any(|event| matches!(event,CoreEvent::Exception{name,..}if name=="NotSupportedError"))
    );
    assert!(rt.drain_mutations().is_empty());
}
#[tokio::test]
async fn text_control_selection_exists_before_focus_and_uses_utf16_shared_native_state() {
    let mut rt=runtime("<input id='empty'><input id='text' value='a😀c'><textarea id='area'>hello</textarea><input id='number' type='number' value='3'>").await;
    rt.create_isolated_world(100, 1).await.unwrap();
    assert_eq!(eval(&mut rt,"let empty=document.getElementById('empty');['selectionStart' in empty,empty.selectionStart,empty.selectionEnd,empty.selectionDirection];").await,json!([true,0,0,"none"]));
    assert_eq!(
        eval(
            &mut rt,
            "empty.focus();[document.activeElement===empty,empty.selectionStart,empty.selectionEnd]"
        )
        .await,
        json!([true, 0, 0])
    );
    assert_eq!(eval(&mut rt,"let text=document.getElementById('text');text.setSelectionRange(1,3,'backward');[text.selectionStart,text.selectionEnd,text.selectionDirection]").await,json!([1,3,"backward"]));
    let utility=rt.evaluate_in_context("let text=document.getElementById('text');[text.selectionStart,text.selectionEnd,text.selectionDirection]",100,false).await.unwrap();
    assert_eq!(utility.value, Some(json!([1, 3, "backward"])));
    let changed = rt
        .evaluate_in_context(
            "text.selectionStart=99;[text.selectionStart,text.selectionEnd]",
            100,
            false,
        )
        .await
        .unwrap();
    assert_eq!(changed.value, Some(json!([4, 4])));
    assert_eq!(
        eval(&mut rt, "[text.selectionStart,text.selectionEnd]").await,
        json!([4, 4])
    );
    assert_eq!(eval(&mut rt,"text.selectionEnd=1;let shrink=[text.selectionStart,text.selectionEnd];text.setSelectionRange(3,1,'forward');[shrink,text.selectionStart,text.selectionEnd,text.selectionDirection]").await,json!([[1,1],1,1,"forward"]));
    assert_eq!(
        eval(
            &mut rt,
            "text.value='😀';[text.selectionStart,text.selectionEnd];"
        )
        .await,
        json!([2, 2])
    );
    assert_eq!(eval(&mut rt,"let area=document.getElementById('area');area.setSelectionRange(1,4);[area.value,area.selectionStart,area.selectionEnd]").await,json!(["hello",1,4]));
    assert_eq!(eval(&mut rt,"let number=document.getElementById('number');let error;try{number.setSelectionRange(0,1)}catch(e){error=e.name};[number.selectionStart,number.selectionEnd,error]").await,json!([null,null,"InvalidStateError"]));
    assert_eq!(eval(&mut rt,"text.type='number';let error2;try{text.selectionStart=1}catch(e){error2=e.name};[text.selectionStart,error2]").await,json!([null,"InvalidStateError"]));
}
