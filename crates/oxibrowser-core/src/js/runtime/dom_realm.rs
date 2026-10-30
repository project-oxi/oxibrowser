//! Realm-owned DOM wrappers. The realm traces wrappers; navigation drops the
//! registry, while live JS references retain their original render document.
use super::*;
use boa_engine::JsData;
use boa_engine::object::builtins::JsWeakMap;
use boa_gc::{Finalize, Trace};

#[derive(Clone, Trace, Finalize)]
struct WrapperEntry {
    weak: JsObject,
    strong: Option<JsObject>,
}
#[derive(Default, Trace, Finalize, JsData)]
struct Wrappers {
    nodes: HashMap<(usize, usize, u64), WrapperEntry>,
}
#[derive(Clone, Trace, Finalize, JsData)]
struct WeakRuntime {
    constructor: JsObject,
    deref: JsObject,
}
pub(super) fn initialize(ctx: &mut Context) {
    let constructor = ctx.intrinsics().constructors().weak_ref().constructor();
    let deref = ctx
        .intrinsics()
        .constructors()
        .weak_ref()
        .prototype()
        .get(js_string!("deref"), ctx)
        .unwrap()
        .as_object()
        .unwrap();
    ctx.realm()
        .host_defined_mut()
        .insert(WeakRuntime { constructor, deref });
}
#[derive(Trace, Finalize, JsData)]
struct CurrentDocument {
    pointer: usize,
    context_id: u32,
    document_id: u64,
    present: bool,
}
static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(1);
type DocumentGeneration = (std::rc::Weak<RefCell<Option<RenderDocument>>>, u64);
thread_local! {static DOCUMENT_IDS:RefCell<HashMap<usize,DocumentGeneration>>=RefCell::new(HashMap::new());}
#[cfg(test)]
pub(super) fn test_document_counts() -> (usize, usize) {
    DOCUMENT_IDS.with(|ids| {
        let ids = ids.borrow();
        let physical = ids
            .values()
            .filter(|(weak, _)| {
                weak.upgrade()
                    .is_some_and(|document| document.borrow().is_some())
            })
            .count();
        (physical, ids.len())
    })
}

pub(super) fn context_id(ctx: &Context) -> u32 {
    ctx.realm()
        .host_defined()
        .get::<CurrentDocument>()
        .map(|data| data.context_id)
        .unwrap_or_else(active_context_id)
}
pub(super) fn has_document(ctx: &Context) -> bool {
    ctx.realm()
        .host_defined()
        .get::<CurrentDocument>()
        .is_some_and(|data| data.present)
}
pub(super) fn document_id(ctx: &Context) -> u64 {
    ctx.realm()
        .host_defined()
        .get::<CurrentDocument>()
        .map(|data| data.document_id)
        .unwrap_or(0)
}
pub(super) fn bind_current(ctx: &mut Context, rd: &Rc<RefCell<Option<RenderDocument>>>) {
    let pointer = Rc::as_ptr(rd) as usize;
    let document_id = DOCUMENT_IDS.with(|ids| {
        let mut ids = ids.borrow_mut();
        ids.retain(|_, (document, _)| document.strong_count() > 0);
        if let Some((_, id)) = ids.get(&pointer) {
            *id
        } else {
            let id = NEXT_DOCUMENT_ID.fetch_add(1, Ordering::Relaxed);
            ids.insert(pointer, (Rc::downgrade(rd), id));
            id
        }
    });
    ctx.realm().host_defined_mut().insert(CurrentDocument {
        pointer,
        context_id: active_context_id(),
        document_id,
        present: rd.borrow().is_some(),
    });
}
pub(super) fn current_pointer(ctx: &Context) -> usize {
    ctx.realm()
        .host_defined()
        .get::<CurrentDocument>()
        .map(|d| d.pointer)
        .unwrap_or(0)
}
pub(super) fn is_current(ctx: &Context, rd: &Rc<RefCell<Option<RenderDocument>>>) -> bool {
    current_pointer(ctx) == Rc::as_ptr(rd) as usize
}
fn key(rd: &Rc<RefCell<Option<RenderDocument>>>, node: usize) -> (usize, usize, u64) {
    let token = rd
        .borrow_mut()
        .as_mut()
        .and_then(|d| d.node_incarnation(node))
        .unwrap_or(0);
    (Rc::as_ptr(rd) as usize, node, token)
}
fn connected(rd: &Rc<RefCell<Option<RenderDocument>>>, node: usize) -> bool {
    let cell = rd.borrow();
    let Some(doc) = cell.as_ref() else {
        return false;
    };
    let mut id = Some(node);
    let mut seen = std::collections::HashSet::new();
    while let Some(node) = id {
        if !seen.insert(node) {
            return false;
        }
        if node == doc.document().root_node().id {
            return true;
        }
        let parent = doc.parent_id(node);
        if let Some(parent) = parent
            && !doc.children_ids(parent).contains(&node)
        {
            return false;
        }
        id = parent;
    }
    false
}
pub(super) fn make_weak(ctx: &mut Context, object: &JsObject) -> JsObject {
    let constructor = ctx
        .realm()
        .host_defined()
        .get::<WeakRuntime>()
        .unwrap()
        .constructor
        .clone();
    constructor
        .construct(&[object.clone().into()], None, ctx)
        .expect("native WeakRef construction")
}
pub(super) fn read_weak(ctx: &mut Context, weak: &JsObject) -> Option<JsObject> {
    let deref = ctx
        .realm()
        .host_defined()
        .get::<WeakRuntime>()?
        .deref
        .clone();
    deref.call(&weak.clone().into(), &[], ctx).ok()?.as_object()
}
fn upgrade(ctx: &mut Context, entry: &WrapperEntry) -> Option<JsObject> {
    if let Some(object) = &entry.strong {
        return Some(object.clone());
    }
    let deref = ctx
        .realm()
        .host_defined()
        .get::<WeakRuntime>()?
        .deref
        .clone();
    deref
        .call(&entry.weak.clone().into(), &[], ctx)
        .ok()?
        .as_object()
}
pub(super) fn get(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
) -> Option<JsValue> {
    let key = key(rd, node);
    let entry = ctx
        .realm()
        .host_defined()
        .get::<Wrappers>()?
        .nodes
        .get(&key)?
        .clone();
    upgrade(ctx, &entry).map(Into::into)
}
pub(super) fn remember(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    object: &JsObject,
    interface: &str,
) {
    let ctor = ctx
        .global_object()
        .get(JsString::from(interface), ctx)
        .ok()
        .and_then(|v| v.as_object());
    if let Some(ctor) = ctor
        && let Ok(proto) = ctor.get(js_string!("prototype"), ctx)
        && let Some(proto) = proto.as_object()
    {
        object.set_prototype(Some(proto));
    }
    let document = ctx
        .global_object()
        .get(js_string!("document"), ctx)
        .unwrap_or_default();
    let _ = object.define_property_or_throw(
        js_string!("ownerDocument"),
        boa_engine::property::PropertyDescriptor::builder()
            .value(document)
            .configurable(true),
        ctx,
    );
    let runtime = ctx
        .realm()
        .host_defined()
        .get::<WeakRuntime>()
        .unwrap()
        .clone();
    let weak = runtime
        .constructor
        .construct(&[object.clone().into()], None, ctx)
        .expect("native WeakRef construction");
    let entry = WrapperEntry {
        weak,
        strong: connected(rd, node).then(|| object.clone()),
    };
    let key = key(rd, node);
    let realm = ctx.realm().clone();
    let mut host = realm.host_defined_mut();
    if host.get::<Wrappers>().is_none() {
        host.insert(Wrappers::default());
    }
    host.get_mut::<Wrappers>().unwrap().nodes.insert(key, entry);
}
fn identity(ctx: &mut Context, object: &JsObject) -> Option<(usize, usize, u64)> {
    let entries = ctx
        .realm()
        .host_defined()
        .get::<Wrappers>()?
        .nodes
        .iter()
        .map(|(key, entry)| (*key, entry.clone()))
        .collect::<Vec<_>>();
    entries
        .into_iter()
        .find_map(|(key, entry)| (upgrade(ctx, &entry).as_ref() == Some(object)).then_some(key))
}
pub(super) fn node_id_in_document(
    ctx: &mut Context,
    object: &JsObject,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
) -> Option<usize> {
    let (document, node, token) = identity(ctx, object)?;
    if document != Rc::as_ptr(rd) as usize {
        return None;
    }
    (rd.borrow_mut().as_mut()?.node_incarnation(node) == Some(token)).then_some(node)
}
pub(super) fn node_id(ctx: &mut Context, object: &JsObject) -> Option<usize> {
    identity(ctx, object).map(|(_, node, _)| node)
}
pub(super) fn refresh(ctx: &mut Context, rd: &Rc<RefCell<Option<RenderDocument>>>) {
    let pointer = Rc::as_ptr(rd) as usize;
    let entries = ctx
        .realm()
        .host_defined()
        .get::<Wrappers>()
        .map(|r| {
            r.nodes
                .iter()
                .filter(|(key, _)| key.0 == pointer)
                .map(|(key, entry)| (*key, entry.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (key, entry) in entries {
        let strong = if connected(rd, key.1) {
            upgrade(ctx, &entry)
        } else {
            None
        };
        if let Some(registry) = ctx.realm().host_defined_mut().get_mut::<Wrappers>()
            && let Some(entry) = registry.nodes.get_mut(&key)
        {
            entry.strong = strong;
        }
    }
}
pub(super) fn clear(ctx: &mut Context) {
    let pointer = current_pointer(ctx);
    DOCUMENT_FOCUS.with(|focus| {
        focus.borrow_mut().remove(&pointer);
    });
    ctx.realm().host_defined_mut().remove::<Wrappers>();
    let global = ctx.global_object();
    let maps = ctx
        .realm()
        .host_defined()
        .get::<Handlers>()
        .map(|h| {
            h.maps
                .iter()
                .map(|(event, map)| (event.clone(), map.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for (event, map) in maps {
        let _ = map.delete(&global, ctx);
        let _ = global.delete_property_or_throw(JsString::from(format!("on{event}")), ctx);
    }
    let listeners = JsObject::with_null_proto();
    let _ = ctx.global_object().create_data_property_or_throw(
        js_string!("__oxiEventListeners"),
        listeners,
        ctx,
    );
}

pub(super) fn interface(tag: &str) -> &str {
    match tag {
        "input" => "HTMLInputElement",
        "textarea" => "HTMLTextAreaElement",
        "button" => "HTMLButtonElement",
        "form" => "HTMLFormElement",
        "iframe" => "HTMLIFrameElement",
        "select" => "HTMLSelectElement",
        "option" => "HTMLOptionElement",
        "canvas" => "HTMLCanvasElement",
        "img" => "HTMLImageElement",
        "a" => "HTMLAnchorElement",
        "div" => "HTMLDivElement",
        "span" => "HTMLSpanElement",
        "p" => "HTMLParagraphElement",
        "script" => "HTMLScriptElement",
        "style" => "HTMLStyleElement",
        "link" => "HTMLLinkElement",
        _ => "HTMLElement",
    }
}

#[derive(Clone, Copy)]
struct FocusEmulation {
    active: bool,
    emulated: bool,
}
pub(super) fn set_focus_emulation(ctx: &mut Context, enabled: bool) {
    ctx.insert_data(FocusEmulation {
        active: true,
        emulated: enabled,
    });
}
pub(super) fn document_focus(ctx: &mut Context) {
    let has_focus = NativeFunction::from_fn_ptr(|_, _, ctx| {
        // Headless browsing contexts are active; emulation can force focus
        // even if a future host-window integration reports them inactive.
        let focused = ctx
            .get_data::<FocusEmulation>()
            .map(|state| state.active || state.emulated)
            .unwrap_or(true);
        Ok(focused.into())
    })
    .to_js_function(ctx.realm());
    if let Ok(document) = ctx.global_object().get(js_string!("document"), ctx)
        && let Some(document) = document.as_object()
    {
        let _ = document.create_data_property_or_throw(js_string!("hasFocus"), has_focus, ctx);
    }
}
#[derive(Default, Trace, Finalize, JsData)]
struct Handlers {
    maps: HashMap<String, JsWeakMap>,
}
fn install_handlers(ctx: &mut Context) {
    if ctx.realm().host_defined().get::<Handlers>().is_some() {
        return;
    }
    let mut maps = HashMap::new();
    let prototypes = ["Document", "HTMLElement", "Window"]
        .into_iter()
        .filter_map(|name| {
            ctx.global_object()
                .get(JsString::from(name), ctx)
                .ok()
                .and_then(|v| v.as_object())
                .and_then(|ctor| ctor.get(js_string!("prototype"), ctx).ok())
                .and_then(|v| v.as_object())
        })
        .collect::<Vec<_>>();
    for event in [
        "input",
        "change",
        "beforeinput",
        "click",
        "dblclick",
        "focus",
        "blur",
        "focusin",
        "focusout",
        "keydown",
        "keyup",
        "keypress",
        "submit",
        "invalid",
        "mousedown",
        "mouseup",
        "mousemove",
        "pointerdown",
        "pointerup",
        "pointermove",
        "load",
        "error",
        "popstate",
        "hashchange",
    ] {
        let map = JsWeakMap::new(ctx);
        let property = JsString::from(format!("on{event}"));
        for prototype in &prototypes {
            let getter = NativeFunction::from_copy_closure_with_captures(
                |this, _, map, ctx| {
                    let Some(owner) = this.as_object() else {
                        return Ok(JsValue::null());
                    };
                    let value = map.get(&owner, ctx)?;
                    Ok(if value.is_undefined() {
                        JsValue::null()
                    } else {
                        value
                    })
                },
                map.clone(),
            )
            .to_js_function(ctx.realm());
            let setter = NativeFunction::from_copy_closure_with_captures(
                |this, args, map, ctx| {
                    let Some(owner) = this.as_object() else {
                        return Ok(JsValue::undefined());
                    };
                    let value = args
                        .first()
                        .filter(|v| v.is_callable())
                        .cloned()
                        .unwrap_or(JsValue::null());
                    map.set(&owner, value, ctx)?;
                    Ok(JsValue::undefined())
                },
                map.clone(),
            )
            .to_js_function(ctx.realm());
            let _ = prototype.define_property_or_throw(
                property.clone(),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(getter)
                    .set(setter)
                    .enumerable(true)
                    .configurable(true),
                ctx,
            );
        }
        maps.insert(event.into(), map);
    }
    ctx.realm().host_defined_mut().insert(Handlers { maps });
}
pub(super) fn install(ctx: &mut Context) {
    document_focus(ctx);
    // Interfaces have distinct prototypes. Keep existing Element shim methods,
    // but never adopt Object.prototype as a DOM interface prototype.
    let _ = ctx.eval(Source::from_bytes(r#"
    (function(){
      if (!globalThis.__oxiDOMInterfaces) {
        var old = globalThis.Element && Element.prototype;
        function define(name,parent) {
          var C = function(){throw new TypeError('Illegal constructor: '+name);};
          C.prototype = Object.create(parent ? parent.prototype : Object.prototype);
          Object.defineProperty(C.prototype,'constructor',{value:C,writable:true,configurable:true});
          globalThis[name]=C; return C;
        }
        var N=define('Node',null), E=define('Element',N), H=define('HTMLElement',E);
        if (old && old !== Object.prototype) Object.getOwnPropertyNames(old).forEach(function(k){
          if(k!=='constructor') Object.defineProperty(E.prototype,k,Object.getOwnPropertyDescriptor(old,k));
        });
        if (old === Object.prototype) ['attachShadow','shadowRoot','getRootNode'].forEach(function(k){
          var descriptor=Object.getOwnPropertyDescriptor(old,k);if(descriptor)Object.defineProperty(E.prototype,k,descriptor);
        });
        define('Document',N); define('Text',N); define('Comment',N); define('DocumentFragment',N);
        ['Input','TextArea','Button','Form','IFrame','Select','Option','Canvas','Image','Anchor','Div','Span','Paragraph','Script','Style','Link'].forEach(function(n){define('HTML'+n+'Element',H);});
        define('SVGElement',E); define('Window',null);
        Object.assign(N,{ELEMENT_NODE:1,TEXT_NODE:3,COMMENT_NODE:8,DOCUMENT_NODE:9,DOCUMENT_FRAGMENT_NODE:11});
        globalThis.__oxiDOMInterfaces=true;
      }
      document.createDocumentFragment=function(){
        const frag=Object.create(DocumentFragment.prototype);frag.nodeType=11;frag.nodeName='#document-fragment';frag.ownerDocument=document;frag.childNodes=[];
        frag.appendChild=function(child){if(child===frag)throw new TypeError('cannot append a fragment to itself');if(!child)throw new TypeError('appendChild requires a Node');if(child.nodeType===11){for(const n of child.childNodes)frag.childNodes.push(n);child.childNodes.length=0;}else frag.childNodes.push(child);return child;};
        return frag;
      };
      globalThis.NodeFilter = { FILTER_ACCEPT:1,FILTER_REJECT:2,FILTER_SKIP:3,SHOW_ALL:4294967295,SHOW_ELEMENT:1,SHOW_ATTRIBUTE:2,SHOW_TEXT:4,SHOW_CDATA_SECTION:8,SHOW_PROCESSING_INSTRUCTION:64,SHOW_COMMENT:128,SHOW_DOCUMENT:256,SHOW_DOCUMENT_TYPE:512,SHOW_DOCUMENT_FRAGMENT:1024 };
      Object.setPrototypeOf(document,Document.prototype);
      Object.setPrototypeOf(globalThis,Window.prototype);
    })();
    "#));
    install_handlers(ctx);
}

pub(super) fn window_alias(ctx: &mut Context) {
    // A prototype get trap observes missing APIs without splitting Window
    // identity. Its target inherits the real Window prototype, so instanceof
    // remains valid in telemetry mode as well.
    let _ = ctx.eval(Source::from_bytes(r#"
      (function(g,w){
        if(g!==w) {
          Object.getOwnPropertyNames(w).forEach(function(k){
            if(k==='self'||k==='parent'||k==='top'||k==='window')return;
            var d=Object.getOwnPropertyDescriptor(w,k);
            if(d && 'value' in d) { d.configurable=true; try{Object.defineProperty(g,k,d);}catch(e){} }
          });
          g.window=g; g.self=g;
          Object.defineProperty(g,'parent',{value:g,configurable:true});
          Object.defineProperty(g,'top',{value:g,configurable:true});
        }
        if(typeof __oxi_tel==='function') {
          var proto=typeof Window==='function'&&Window.prototype || Object.prototype;
          Object.setPrototypeOf(g,new Proxy(Object.create(proto),{get:function(t,p,r){
            if(typeof p==='string'&&!Reflect.has(t,p))__oxi_tel(p);
            return Reflect.get(t,p,r);
          }}));
        }
      })(globalThis,window);
    "#));
}

pub(super) fn listeners(
    ctx: &mut Context,
    object: &JsObject,
    rd: Option<Rc<RefCell<Option<RenderDocument>>>>,
    node: u32,
) {
    let native = rd.clone();
    let native_document = rd.as_ref().map(|rd| Rc::as_ptr(rd) as usize).unwrap_or(0);
    let add = unsafe {
        NativeFunction::from_closure(move |this, args, ctx| {
            let name = args
                .first()
                .unwrap_or(&JsValue::undefined())
                .to_string(ctx)?
                .to_std_string_escaped();
            let Some(cb) = args.get(1).and_then(JsValue::as_object) else {
                return Ok(JsValue::undefined());
            };
            let Some(owner) = this
                .as_object()
                .or_else(|| (node == u32::MAX).then(|| ctx.global_object()))
            else {
                return Ok(JsValue::undefined());
            };
            let (capture, once, passive) = options(args.get(2), ctx)?;
            let array = records(ctx, &owner, &name)?;
            for i in 0..array.length(ctx)? {
                if let Some(r) = array.at(i as i64, ctx)?.as_object()
                    && r.get(js_string!("callback"), ctx)? == JsValue::from(cb.clone())
                    && r.get(js_string!("capture"), ctx)?.to_boolean() == capture
                    && !r.get(js_string!("removed"), ctx)?.to_boolean()
                {
                    return Ok(JsValue::undefined());
                }
            }
            let record = boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("callback"), cb.clone(), Attribute::all())
                .property(js_string!("capture"), capture, Attribute::all())
                .property(js_string!("once"), once, Attribute::all())
                .property(js_string!("passive"), passive, Attribute::all())
                .property(js_string!("removed"), false, Attribute::all())
                .build();
            array.push(record.clone(), ctx)?;
            let token = native
                .as_ref()
                .and_then(|rd| {
                    rd.borrow_mut()
                        .as_mut()
                        .and_then(|d| d.node_incarnation(node as usize))
                })
                .unwrap_or(0);
            registry_add_native_options(
                node,
                token,
                &name,
                cb.clone(),
                capture,
                once,
                passive,
                Some(record),
                native_document,
                ctx,
            );

            Ok(JsValue::undefined())
        })
    }
    .to_js_function(ctx.realm());
    let remove = unsafe {
        NativeFunction::from_closure(move |this, args, ctx| {
            let name = args
                .first()
                .unwrap_or(&JsValue::undefined())
                .to_string(ctx)?
                .to_std_string_escaped();
            let Some(owner) = this
                .as_object()
                .or_else(|| (node == u32::MAX).then(|| ctx.global_object()))
            else {
                return Ok(JsValue::undefined());
            };
            let cb = args.get(1).cloned().unwrap_or_default();
            let (capture, _, _) = options(args.get(2), ctx)?;
            if let Some(callback) = cb.as_object() {
                registry_remove_native_listener(
                    node,
                    &name,
                    &callback,
                    capture,
                    native_document,
                    ctx,
                );
            }
            let array = records(ctx, &owner, &name)?;
            for i in 0..array.length(ctx)? {
                if let Some(r) = array.at(i as i64, ctx)?.as_object()
                    && r.get(js_string!("callback"), ctx)? == cb
                    && r.get(js_string!("capture"), ctx)?.to_boolean() == capture
                {
                    r.set(js_string!("removed"), true, true, ctx)?;
                }
            }
            Ok(JsValue::undefined())
        })
    }
    .to_js_function(ctx.realm());
    let _ = object.create_data_property_or_throw(js_string!("addEventListener"), add, ctx);
    let _ = object.create_data_property_or_throw(js_string!("removeEventListener"), remove, ctx);
}
fn options(value: Option<&JsValue>, ctx: &mut Context) -> JsResult<(bool, bool, bool)> {
    let Some(v) = value else {
        return Ok((false, false, false));
    };
    if let Some(o) = v.as_object() {
        Ok((
            o.get(js_string!("capture"), ctx)?.to_boolean(),
            o.get(js_string!("once"), ctx)?.to_boolean(),
            o.get(js_string!("passive"), ctx)?.to_boolean(),
        ))
    } else {
        Ok((v.to_boolean(), false, false))
    }
}
fn records(ctx: &mut Context, owner: &JsObject, event: &str) -> JsResult<JsArray> {
    let store = match owner
        .get(js_string!("__oxiEventListeners"), ctx)?
        .as_object()
    {
        Some(o) => o,
        None => {
            let o = JsObject::with_object_proto(ctx.intrinsics());
            owner.create_data_property_or_throw(
                js_string!("__oxiEventListeners"),
                o.clone(),
                ctx,
            )?;
            o
        }
    };
    if let Some(o) = store.get(JsString::from(event), ctx)?.as_object()
        && let Ok(a) = JsArray::from_object(o)
    {
        return Ok(a);
    }
    let a = JsArray::new(ctx)?;
    store.create_data_property_or_throw(JsString::from(event), a.clone(), ctx)?;
    Ok(a)
}
pub(super) fn invoke(
    ctx: &mut Context,
    target: &JsValue,
    event: &JsValue,
    name: &str,
    capture: bool,
) -> JsResult<()> {
    let Some(owner) = target.as_object() else {
        return Ok(());
    };
    let array = records(ctx, &owner, name)?;
    let length = array.length(ctx)?;
    for i in 0..length {
        let e = event.as_object().unwrap();
        if e.get(js_string!("__stopImmediatePropagation"), ctx)?
            .to_boolean()
        {
            break;
        }
        let Some(r) = array.at(i as i64, ctx)?.as_object() else {
            continue;
        };
        if r.get(js_string!("removed"), ctx)?.to_boolean()
            || r.get(js_string!("capture"), ctx)?.to_boolean() != capture
        {
            continue;
        }
        if r.get(js_string!("once"), ctx)?.to_boolean() {
            r.set(js_string!("removed"), true, true, ctx)?;
            if let Some(callback) = r.get(js_string!("callback"), ctx)?.as_object() {
                let node = node_id(ctx, &owner).map(|n| n as u32).unwrap_or_else(|| {
                    if owner == ctx.global_object() {
                        u32::MAX
                    } else {
                        u32::MAX - 1
                    }
                });
                let document = identity(ctx, &owner)
                    .map(|key| key.0)
                    .unwrap_or_else(|| current_pointer(ctx));
                registry_remove_native_listener(node, name, &callback, capture, document, ctx);
            }
        }
        e.set(
            js_string!("__passive"),
            r.get(js_string!("passive"), ctx)?,
            true,
            ctx,
        )?;
        if let Some(cb) = r.get(js_string!("callback"), ctx)?.as_object() {
            let result = if cb.is_callable() {
                cb.call(target, std::slice::from_ref(event), ctx)
            } else {
                cb.get(js_string!("handleEvent"), ctx)
                    .and_then(|f| match f.as_object() {
                        Some(f) if f.is_callable() => {
                            f.call(&cb.clone().into(), std::slice::from_ref(event), ctx)
                        }
                        _ => Ok(JsValue::undefined()),
                    })
            };
            if let Err(error) = result {
                let (message, name, stack) = error_sink_details(&error, ctx);
                push_event(CoreEvent::Exception {
                    message,
                    name,
                    stack,
                    timestamp: now_ms(),
                });
            }
        }
        e.set(js_string!("__passive"), false, true, ctx)?;
    }
    Ok(())
}

pub(super) fn text(
    ctx: &mut Context,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    dirty: Arc<AtomicBool>,
) -> JsValue {
    let comment = rd
        .borrow()
        .as_ref()
        .is_some_and(|doc| doc.node_kind(node) == Some("comment"));
    let object = boa_engine::object::ObjectInitializer::new(ctx)
        .property(js_string!("__nodeId"), node, Attribute::CONFIGURABLE)
        .property(
            js_string!("nodeType"),
            if comment { 8 } else { 3 },
            Attribute::CONFIGURABLE,
        )
        .property(
            js_string!("nodeName"),
            JsString::from(if comment { "#comment" } else { "#text" }),
            Attribute::CONFIGURABLE,
        )
        .build();
    remember(
        ctx,
        &rd,
        node,
        &object,
        if comment { "Comment" } else { "Text" },
    );
    for name in ["textContent", "nodeValue", "data"] {
        let read = rd.clone();
        let write = rd.clone();
        let dirty = dirty.clone();
        let getter = unsafe {
            NativeFunction::from_closure(move |this, _, ctx| {
                if comment {
                    return this
                        .as_object()
                        .map(|o| o.get(js_string!("__oxiCommentData"), ctx))
                        .unwrap_or(Ok(js_string!("").into()));
                }
                Ok(JsString::from(
                    read.borrow()
                        .as_ref()
                        .map(|d| d.node_text(node))
                        .unwrap_or_default(),
                )
                .into())
            })
        }
        .to_js_function(ctx.realm());
        let setter = unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                let value = args
                    .first()
                    .unwrap_or(&JsValue::undefined())
                    .to_string(ctx)?
                    .to_std_string_escaped();
                if comment {
                    if let Some(object) = this.as_object() {
                        object.create_data_property_or_throw(
                            js_string!("__oxiCommentData"),
                            JsString::from(value),
                            ctx,
                        )?;
                    }
                } else if let Some(d) = write.borrow_mut().as_mut() {
                    d.set_text(node, &value);
                    dirty.store(true, Ordering::Relaxed);
                }
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .set(setter)
                .configurable(true),
            ctx,
        );
    }
    for (name, relation) in [
        (
            "parentNode",
            RenderDocument::parent_id as fn(&RenderDocument, usize) -> Option<usize>,
        ),
        ("nextSibling", RenderDocument::next_sibling),
        ("previousSibling", RenderDocument::previous_sibling),
    ] {
        let rd = rd.clone();
        let dirty = dirty.clone();
        let getter = unsafe {
            NativeFunction::from_closure(move |_, _, ctx| {
                let id = rd.borrow().as_ref().and_then(|d| relation(d, node));
                Ok(id
                    .map(|id| create_render_element_object(ctx, rd.clone(), id, dirty.clone()))
                    .unwrap_or(JsValue::null()))
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .configurable(true),
            ctx,
        );
    }
    node_state(ctx, &object, rd.clone(), node, dirty.clone());
    listeners(ctx, &object, Some(rd), node as u32);
    object.into()
}
thread_local! {
    static DOCUMENT_FOCUS: RefCell<HashMap<usize,(usize,u64)>> = RefCell::new(HashMap::new());
    static FOCUSED_CONTEXT: Cell<u32> = const { Cell::new(1) };
}
pub(super) fn forget_focus(rd: &Rc<RefCell<Option<RenderDocument>>>) {
    DOCUMENT_FOCUS.with(|focus| {
        focus.borrow_mut().remove(&(Rc::as_ptr(rd) as usize));
    });
}
pub(super) fn focused_context() -> u32 {
    FOCUSED_CONTEXT.with(Cell::get)
}
pub(super) fn focused_node(rd: &Rc<RefCell<Option<RenderDocument>>>) -> Option<usize> {
    let (node, token) =
        DOCUMENT_FOCUS.with(|focus| focus.borrow().get(&(Rc::as_ptr(rd) as usize)).copied())?;
    if rd.borrow_mut().as_mut()?.node_incarnation(node) != Some(token) {
        return None;
    }
    connected(rd, node).then_some(node)
}
pub(super) fn set_focused(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: Option<usize>,
) {
    DOCUMENT_FOCUS.with(|focus| {
        let mut focus = focus.borrow_mut();
        if let Some(node) = node {
            if let Some(token) = rd
                .borrow_mut()
                .as_mut()
                .and_then(|doc| doc.node_incarnation(node))
            {
                focus.insert(Rc::as_ptr(rd) as usize, (node, token));
            }
        } else {
            focus.remove(&(Rc::as_ptr(rd) as usize));
        }
    });
    if node.is_some() {
        let id = ctx
            .realm()
            .host_defined()
            .get::<CurrentDocument>()
            .map(|d| d.context_id)
            .unwrap_or_else(active_context_id);
        FOCUSED_CONTEXT.with(|context| context.set(id));
    }
    frames::record_focus(ctx, rd, node);
}
pub(super) fn active_element(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
) -> JsValue {
    let node =
        focused_node(rd).or_else(|| rd.borrow().as_ref().and_then(|d| d.query_selector("body")));
    node.map(|node| create_render_element_object(ctx, rd.clone(), node, dirty.clone()))
        .unwrap_or(JsValue::null())
}
pub(super) fn focus(
    ctx: &mut Context,
    object: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    dirty: Arc<AtomicBool>,
) {
    for (name, blur) in [("focus", false), ("blur", true)] {
        let rd = rd.clone();
        let dirty = dirty.clone();
        let function = unsafe {
            NativeFunction::from_closure(move |this, _, ctx| {
                if !is_current(ctx, &rd)
                    || !connected(&rd, node)
                    || this
                        .as_object()
                        .and_then(|object| node_id_in_document(ctx, &object, &rd))
                        != Some(node)
                {
                    return Ok(JsValue::undefined());
                }
                let focusable = rd.borrow().as_ref().is_some_and(|doc| {
                    doc.node_attr(node, "disabled").is_none()
                        && (matches!(
                            doc.tag_name(node).as_deref(),
                            Some("input" | "textarea" | "button" | "select")
                        ) || doc.node_attr(node, "tabindex").is_some()
                            || (doc.tag_name(node).as_deref() == Some("a")
                                && doc.node_attr(node, "href").is_some()))
                });
                if !focusable {
                    return Ok(JsValue::undefined());
                }
                let previous = focused_node(&rd);
                if blur && previous != Some(node) || !blur && previous == Some(node) {
                    return Ok(JsValue::undefined());
                }
                let next = (!blur).then_some(node);
                set_focused(ctx, &rd, next);
                if let Some(old) = previous {
                    render_event(ctx, &rd, &dirty, old, "blur", false, false)?;
                    render_event(ctx, &rd, &dirty, old, "focusout", true, false)?;
                }
                if let Some(next) = next {
                    render_event(ctx, &rd, &dirty, next, "focus", false, false)?;
                    render_event(ctx, &rd, &dirty, next, "focusin", true, false)?;
                }
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.create_data_property_or_throw(JsString::from(name), function, ctx);
    }
}

pub(super) fn tree_mutations(
    ctx: &mut Context,
    object: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    parent: usize,
    dirty: Arc<AtomicBool>,
) {
    for (name, replace) in [("removeChild", false), ("replaceChild", true)] {
        let rd = rd.clone();
        let dirty = dirty.clone();
        let method = unsafe {
            NativeFunction::from_closure(move |_, args, ctx| {
                let old = args
                    .get(if replace { 1 } else { 0 })
                    .cloned()
                    .unwrap_or_default();
                let old_id = old
                    .as_object()
                    .and_then(|o| node_id_in_document(ctx, &o, &rd))
                    .ok_or_else(|| JsNativeError::typ().with_message("argument is not a Node"))?;
                if !rd
                    .borrow()
                    .as_ref()
                    .is_some_and(|d| d.parent_id(old_id) == Some(parent))
                {
                    return Err(JsNativeError::typ()
                        .with_message("NotFoundError: node is not a child of this parent")
                        .into());
                }
                if replace {
                    let new = args.first().cloned().unwrap_or_default();
                    let new_id = new
                        .as_object()
                        .and_then(|o| node_id_in_document(ctx, &o, &rd))
                        .ok_or_else(|| {
                            JsNativeError::typ().with_message("argument is not a Node")
                        })?;
                    if new_id == old_id {
                        return Ok(old);
                    }
                    if let Some(d) = rd.borrow_mut().as_mut() {
                        d.insert_before(old_id, new_id);
                        d.remove_node(old_id);
                    }
                    call_global_helper(ctx, "__oxi_fire_connected", &[new]);
                } else if let Some(d) = rd.borrow_mut().as_mut() {
                    d.remove_node(old_id);
                }
                dirty.store(true, Ordering::Relaxed);
                refresh(ctx, &rd);
                call_global_helper(ctx, "__oxi_fire_disconnected", std::slice::from_ref(&old));
                Ok(old)
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.create_data_property_or_throw(JsString::from(name), method, ctx);
    }
}

pub(super) fn synthetic_event(
    ctx: &mut Context,
    name: &str,
    bubbles: bool,
    cancelable: bool,
) -> JsValue {
    let object = boa_engine::object::ObjectInitializer::new(ctx)
        .property(js_string!("type"), JsString::from(name), Attribute::all())
        .property(js_string!("bubbles"), bubbles, Attribute::all())
        .property(js_string!("isTrusted"), false, Attribute::CONFIGURABLE)
        .property(js_string!("cancelable"), cancelable, Attribute::all())
        .property(js_string!("defaultPrevented"), false, Attribute::all())
        .function(
            NativeFunction::from_fn_ptr(|this, _, ctx| {
                if let Some(o) = this.as_object()
                    && o.get(js_string!("cancelable"), ctx)?.to_boolean()
                    && !o.get(js_string!("__passive"), ctx)?.to_boolean()
                {
                    o.set(js_string!("defaultPrevented"), true, true, ctx)?;
                }
                Ok(JsValue::undefined())
            }),
            js_string!("preventDefault"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(|this, _, ctx| {
                if let Some(o) = this.as_object() {
                    o.set(js_string!("__stopPropagation"), true, true, ctx)?;
                }
                Ok(JsValue::undefined())
            }),
            js_string!("stopPropagation"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(|this, _, ctx| {
                if let Some(o) = this.as_object() {
                    o.set(js_string!("__stopPropagation"), true, true, ctx)?;
                    o.set(js_string!("__stopImmediatePropagation"), true, true, ctx)?;
                }
                Ok(JsValue::undefined())
            }),
            js_string!("stopImmediatePropagation"),
            0,
        )
        .build();
    object.into()
}

pub(super) fn attributes(
    ctx: &mut Context,
    element: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    dirty: Arc<AtomicBool>,
) {
    let getter=unsafe {NativeFunction::from_closure(move |this,_,ctx| {
      let Some(owner)=this.as_object() else{return Ok(JsValue::undefined());};
      if let Some(cached)=owner.get(js_string!("__oxiAttributes"),ctx)?.as_object(){return Ok(cached.into());}
      let read_document=rd.clone();let write_dirty=dirty.clone();
      let read={NativeFunction::from_closure(move |_,_,ctx| {
        let attrs=read_document.borrow().as_ref().map(|d|d.node_attributes(node)).unwrap_or_default();
        let mut values=vec![];
        for (name,_) in attrs {
          let read=read_document.clone();let write=read_document.clone();let key=name.clone();let write_key=name.clone();let dirty=write_dirty.clone();
          let getter={NativeFunction::from_closure(move |_,_,_|Ok(JsString::from(read.borrow().as_ref().and_then(|d|d.node_attr(node,&key)).unwrap_or_default()).into()))}.to_js_function(ctx.realm());
          let setter={NativeFunction::from_closure(move |_,args,ctx|{let value=args.first().unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();if let Some(d)=write.borrow_mut().as_mut(){d.set_attribute(node,&write_key,&value);dirty.store(true,Ordering::Relaxed);}Ok(JsValue::undefined())})}.to_js_function(ctx.realm());
          values.push(boa_engine::object::ObjectInitializer::new(ctx).property(js_string!("name"),JsString::from(name.clone()),Attribute::CONFIGURABLE).property(js_string!("nodeName"),JsString::from(name),Attribute::CONFIGURABLE).property(js_string!("nodeType"),2,Attribute::CONFIGURABLE).accessor(js_string!("value"),Some(getter),Some(setter),Attribute::CONFIGURABLE).build().into());
        }
        Ok(JsArray::from_iter(values,ctx).into())
      })}.to_js_function(ctx.realm());
      let helper=ctx.eval(Source::from_bytes(r#"(function(read){const named=name=>read().find(a=>a.name===String(name))||null;return new Proxy(Object.create(null),{get(t,k){if(k==='length')return read().length;if(k==='item')return i=>read()[Number(i)]||null;if(k==='getNamedItem')return named;if(k===Symbol.iterator)return function(){return read()[Symbol.iterator]()};if(typeof k==='string'&&/^[0-9]+$/.test(k))return read()[Number(k)];if(typeof k==='string')return named(k)||undefined;}})})"#))?;
      let map=helper.as_callable().unwrap().call(&JsValue::undefined(),&[read.into()],ctx)?;
      owner.create_data_property_or_throw(js_string!("__oxiAttributes"),map.clone(),ctx)?;
      Ok(map)
    })}.to_js_function(ctx.realm());
    let _ = element.define_property_or_throw(
        js_string!("attributes"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(getter)
            .configurable(true),
        ctx,
    );
}

pub(super) fn document_tree(
    ctx: &mut Context,
    document: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    dirty: Arc<AtomicBool>,
) {
    let walker = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            if args
                .get(2)
                .is_some_and(|filter| !filter.is_null() && !filter.is_undefined())
            {
                return Err(JsNativeError::typ()
                    .with_message("TreeWalker filters are not implemented")
                    .into());
            }
            let root = args.first().cloned().unwrap_or_default();
            let root_id = root
                .as_object()
                .and_then(|object| node_id_in_document(ctx, &object, &rd));
            let document_root = root.as_object().is_some_and(|object| {
                ctx.global_object()
                    .get(js_string!("document"), ctx)
                    .ok()
                    .and_then(|v| v.as_object())
                    .as_ref()
                    == Some(&object)
            });
            let mask = args
                .get(1)
                .map(|v| v.to_u32(ctx))
                .transpose()?
                .unwrap_or(u32::MAX);
            let mut ids = vec![];
            {
                let cell = rd.borrow();
                let Some(doc) = cell.as_ref() else {
                    return Ok(JsValue::null());
                };
                let start = if let Some(root) = root_id {
                    root
                } else if document_root {
                    doc.document().root_node().id
                } else {
                    return Err(JsNativeError::typ()
                        .with_message("TreeWalker root is not a node of this document")
                        .into());
                };
                let mut stack = doc.children_ids(start);
                stack.reverse();
                while let Some(node) = stack.pop() {
                    let kind = doc.node_kind(node);
                    let bit = match kind {
                        Some("element") => 1,
                        Some("text") => 4,
                        Some("comment") => 128,
                        _ => 0,
                    };
                    if mask & bit != 0 {
                        ids.push(node);
                    }
                    let mut children = doc.children_ids(node);
                    children.reverse();
                    stack.extend(children);
                }
            }
            let cursor = Rc::new(Cell::new(0usize));
            let document = rd.clone();
            let dirty = dirty.clone();
            let next = {
                NativeFunction::from_closure(move |this, _, ctx| {
                    let index = cursor.get();
                    let Some(node) = ids.get(index).copied() else {
                        return Ok(JsValue::null());
                    };
                    cursor.set(index + 1);
                    let value =
                        create_render_element_object(ctx, document.clone(), node, dirty.clone());
                    if let Some(object) = this.as_object() {
                        object.set(js_string!("currentNode"), value.clone(), true, ctx)?;
                    }
                    Ok(value)
                })
            };
            Ok(boa_engine::object::ObjectInitializer::new(ctx)
                .property(js_string!("root"), root.clone(), Attribute::CONFIGURABLE)
                .property(js_string!("currentNode"), root, Attribute::all())
                .property(js_string!("whatToShow"), mask, Attribute::CONFIGURABLE)
                .function(next, js_string!("nextNode"), 0)
                .build()
                .into())
        })
    }
    .to_js_function(ctx.realm());
    let _ = document.create_data_property_or_throw(js_string!("createTreeWalker"), walker, ctx);
    // Navigation has already fetched and installed its declared fonts before
    // the Document is exposed. This promise describes that initial set.
    let fonts = boa_engine::object::ObjectInitializer::new(ctx)
        .property(
            js_string!("status"),
            js_string!("loaded"),
            Attribute::CONFIGURABLE,
        )
        .build();
    if let Ok(ready) = JsPromise::resolve(fonts.clone(), ctx) {
        let _ = fonts.create_data_property_or_throw(js_string!("ready"), ready, ctx);
    }
    let _ = document.create_data_property_or_throw(js_string!("fonts"), fonts, ctx);
}

#[derive(Clone)]
struct ControlSelection {
    value: String,
    start: usize,
    end: usize,
    direction: &'static str,
}
struct DocumentSelections {
    document: std::rc::Weak<RefCell<Option<RenderDocument>>>,
    controls: HashMap<(usize, u64), ControlSelection>,
}
thread_local! {
    static CONTROL_SELECTIONS: RefCell<HashMap<usize, DocumentSelections>> = RefCell::new(HashMap::new());
}
fn with_control_selection<T>(
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    operation: impl FnOnce(&mut ControlSelection, usize) -> T,
) -> Option<T> {
    let (token, value) = {
        let mut cell = rd.borrow_mut();
        let doc = cell.as_mut()?;
        let tag = doc.tag_name(node)?;
        if tag == "input" {
            let kind = doc
                .node_attr(node, "type")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if matches!(
                kind.as_str(),
                "hidden"
                    | "email"
                    | "number"
                    | "range"
                    | "color"
                    | "date"
                    | "datetime-local"
                    | "month"
                    | "week"
                    | "time"
                    | "checkbox"
                    | "radio"
                    | "file"
                    | "submit"
                    | "image"
                    | "reset"
                    | "button"
            ) {
                return None;
            }
        } else if tag != "textarea" {
            return None;
        }
        let token = doc.node_incarnation(node)?;
        let value = doc.node_attr(node, "value").unwrap_or_else(|| {
            if tag == "textarea" {
                doc.node_text(node)
            } else {
                String::new()
            }
        });
        (token, value)
    };
    let length = value.encode_utf16().count();
    CONTROL_SELECTIONS.with(|selections| {
        let mut documents = selections.borrow_mut();
        documents.retain(|_, document| document.document.upgrade().is_some());
        let document =
            documents
                .entry(Rc::as_ptr(rd) as usize)
                .or_insert_with(|| DocumentSelections {
                    document: Rc::downgrade(rd),
                    controls: HashMap::new(),
                });
        {
            let mut cell = rd.borrow_mut();
            document.controls.retain(|(id, incarnation), _| {
                cell.as_mut().and_then(|doc| doc.node_incarnation(*id)) == Some(*incarnation)
            });
        }
        let selection =
            document
                .controls
                .entry((node, token))
                .or_insert_with(|| ControlSelection {
                    value: value.clone(),
                    start: 0,
                    end: 0,
                    direction: "none",
                });
        if selection.value != value {
            selection.start = length;
            selection.end = length;
            selection.direction = "none";
            selection.value = value;
        }
        Some(operation(selection, length))
    })
}
pub(super) fn control_selection(
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
) -> Option<(usize, usize)> {
    with_control_selection(rd, node, |selection, _| (selection.start, selection.end))
}
pub(super) fn set_control_selection(
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    start: usize,
    end: usize,
) -> bool {
    with_control_selection(rd, node, |selection, length| {
        selection.end = end.min(length);
        selection.start = start.min(selection.end);
        selection.direction = "none";
    })
    .is_some()
}
fn selection_error(ctx: &mut Context) -> boa_engine::JsError {
    let error: boa_engine::JsError = JsNativeError::typ()
        .with_message("this input type does not support text selection")
        .into();
    match error.into_opaque(ctx) {
        Ok(value) => {
            if let Some(object) = value.as_object() {
                let _ = object.create_data_property_or_throw(
                    js_string!("name"),
                    js_string!("InvalidStateError"),
                    ctx,
                );
                let _ = object.create_data_property_or_throw(js_string!("code"), 11, ctx);
            }
            boa_engine::JsError::from_opaque(value)
        }
        Err(error) => error,
    }
}
fn attach_control_selection(
    ctx: &mut Context,
    object: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
) {
    if !rd
        .borrow()
        .as_ref()
        .is_some_and(|doc| matches!(doc.tag_name(node).as_deref(), Some("input" | "textarea")))
    {
        return;
    }
    // Initialize before scripts can assign value, including before the first focus.
    let _ = control_selection(&rd, node);
    for property in ["selectionStart", "selectionEnd", "selectionDirection"] {
        let read = rd.clone();
        let get = unsafe {
            NativeFunction::from_closure(move |this, _, ctx| {
                if this
                    .as_object()
                    .and_then(|object| node_id_in_document(ctx, &object, &read))
                    != Some(node)
                {
                    return Err(JsNativeError::typ()
                        .with_message("invalid text control receiver")
                        .into());
                }
                Ok(
                    with_control_selection(&read, node, |selection, _| match property {
                        "selectionStart" => JsValue::from(selection.start as u32),
                        "selectionEnd" => JsValue::from(selection.end as u32),
                        _ => JsString::from(selection.direction).into(),
                    })
                    .unwrap_or(JsValue::null()),
                )
            })
        }
        .to_js_function(ctx.realm());
        let write = rd.clone();
        let set = unsafe {
            NativeFunction::from_closure(move |this, args, ctx| {
                if this
                    .as_object()
                    .and_then(|object| node_id_in_document(ctx, &object, &write))
                    != Some(node)
                {
                    return Err(JsNativeError::typ()
                        .with_message("invalid text control receiver")
                        .into());
                }
                let value = args.first().cloned().unwrap_or_default();
                let (position, direction) = if property == "selectionDirection" {
                    (0, value.to_string(ctx)?.to_std_string_escaped())
                } else {
                    (value.to_u32(ctx)? as usize, String::new())
                };
                with_control_selection(&write, node, |selection, length| match property {
                    "selectionStart" => {
                        selection.start = position.min(length);
                        selection.end = selection.end.max(selection.start);
                    }
                    "selectionEnd" => {
                        selection.end = position.min(length);
                        selection.start = selection.start.min(selection.end);
                    }
                    _ => {
                        selection.direction = match direction.as_str() {
                            "forward" => "forward",
                            "backward" => "backward",
                            _ => "none",
                        }
                    }
                })
                .ok_or_else(|| selection_error(ctx))?;
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.define_property_or_throw(
            JsString::from(property),
            boa_engine::property::PropertyDescriptor::builder()
                .get(get)
                .set(set)
                .configurable(true),
            ctx,
        );
    }
    let function = unsafe {
        NativeFunction::from_closure(move |this, args, ctx| {
            if this
                .as_object()
                .and_then(|object| node_id_in_document(ctx, &object, &rd))
                != Some(node)
            {
                return Err(JsNativeError::typ()
                    .with_message("invalid text control receiver")
                    .into());
            }
            let start = args.first().unwrap_or(&JsValue::undefined()).to_u32(ctx)? as usize;
            let end = args.get(1).unwrap_or(&JsValue::undefined()).to_u32(ctx)? as usize;
            let direction = args
                .get(2)
                .map(|value| {
                    value
                        .to_string(ctx)
                        .map(|value| value.to_std_string_escaped())
                })
                .transpose()?
                .unwrap_or_default();
            with_control_selection(&rd, node, |selection, length| {
                selection.end = end.min(length);
                selection.start = start.min(selection.end);
                selection.direction = match direction.as_str() {
                    "forward" => "forward",
                    "backward" => "backward",
                    _ => "none",
                };
            })
            .ok_or_else(|| selection_error(ctx))?;
            Ok(JsValue::undefined())
        })
    }
    .to_js_function(ctx.realm());
    let _ = object.create_data_property_or_throw(js_string!("setSelectionRange"), function, ctx);
}

pub(super) fn node_state(
    ctx: &mut Context,
    object: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    dirty: Arc<AtomicBool>,
) {
    attach_control_selection(ctx, object, rd.clone(), node);
    let document = rd.clone();
    let connected = unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            let cell = document.borrow();
            let Some(doc) = cell.as_ref() else {
                return Ok(false.into());
            };
            let mut id = Some(node);
            let mut seen = std::collections::HashSet::new();
            while let Some(node) = id {
                if !seen.insert(node) {
                    return Ok(false.into());
                }
                if node == doc.document().root_node().id {
                    return Ok(true.into());
                }
                let parent = doc.parent_id(node);
                if let Some(parent) = parent
                    && !doc.children_ids(parent).contains(&node)
                {
                    return Ok(false.into());
                }
                id = parent;
            }
            Ok(false.into())
        })
    }
    .to_js_function(ctx.realm());
    let _ = object.define_property_or_throw(
        js_string!("isConnected"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(connected)
            .configurable(true),
        ctx,
    );
    if rd
        .borrow()
        .as_ref()
        .is_some_and(|d| d.node_kind(node) == Some("element"))
    {
        let document = rd.clone();
        let rects = unsafe {
            NativeFunction::from_closure(move |_, _, ctx| {
                let (x, y, w, h) = document
                    .borrow_mut()
                    .as_mut()
                    .map(|d| {
                        d.flush_layout();
                        d.node_layout_rect(node)
                    })
                    .unwrap_or_default();
                if w <= 0.0 || h <= 0.0 {
                    return Ok(JsArray::from_iter(Vec::<JsValue>::new(), ctx).into());
                }
                let rect = boa_engine::object::ObjectInitializer::new(ctx)
                    .property(js_string!("x"), x, Attribute::all())
                    .property(js_string!("y"), y, Attribute::all())
                    .property(js_string!("left"), x, Attribute::all())
                    .property(js_string!("top"), y, Attribute::all())
                    .property(js_string!("right"), x + w, Attribute::all())
                    .property(js_string!("bottom"), y + h, Attribute::all())
                    .property(js_string!("width"), w, Attribute::all())
                    .property(js_string!("height"), h, Attribute::all())
                    .build();
                Ok(JsArray::from_iter([JsValue::from(rect)], ctx).into())
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.create_data_property_or_throw(js_string!("getClientRects"), rects, ctx);
    }
    let tag = rd
        .borrow()
        .as_ref()
        .and_then(|d| d.tag_name(node))
        .unwrap_or_default();
    for (property, attribute, applies) in [
        ("checked", "checked", tag == "input"),
        ("selected", "selected", tag == "option"),
        (
            "required",
            "required",
            matches!(tag.as_str(), "input" | "textarea" | "select"),
        ),
        (
            "readOnly",
            "readonly",
            matches!(tag.as_str(), "input" | "textarea"),
        ),
        (
            "multiple",
            "multiple",
            matches!(tag.as_str(), "input" | "select"),
        ),
        ("noValidate", "novalidate", tag == "form"),
        (
            "formNoValidate",
            "formnovalidate",
            matches!(tag.as_str(), "input" | "button"),
        ),
    ] {
        if !applies {
            continue;
        }
        let read = rd.clone();
        let write = rd.clone();
        let dirty = dirty.clone();
        let get = unsafe {
            NativeFunction::from_closure(move |_, _, _| {
                Ok(read
                    .borrow()
                    .as_ref()
                    .is_some_and(|d| d.node_attr(node, attribute).is_some())
                    .into())
            })
        }
        .to_js_function(ctx.realm());
        let set = unsafe {
            NativeFunction::from_closure(move |_, args, _| {
                let value = args.first().is_some_and(JsValue::to_boolean);
                if let Some(d) = write.borrow_mut().as_mut() {
                    if value {
                        d.set_attribute(node, attribute, "");
                    } else {
                        d.remove_attribute(node, attribute);
                    }
                    dirty.store(true, Ordering::Relaxed);
                }
                Ok(JsValue::undefined())
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.define_property_or_throw(
            JsString::from(property),
            boa_engine::property::PropertyDescriptor::builder()
                .get(get)
                .set(set)
                .configurable(true),
            ctx,
        );
    }
}

pub(super) fn render_event(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    node: usize,
    name: &str,
    bubbles: bool,
    cancelable: bool,
) -> JsResult<bool> {
    let (ids, connected) = {
        let cell = rd.borrow();
        let mut ids = vec![node];
        let mut connected = false;
        if let Some(doc) = cell.as_ref() {
            let mut parent = doc.parent_id(node);
            while let Some(id) = parent {
                if id == doc.document().root_node().id {
                    connected = true;
                    break;
                }
                if doc.tag_name(id).is_none() {
                    break;
                }
                ids.push(id);
                parent = doc.parent_id(id);
            }
        }
        (ids, connected)
    };
    let mut path = ids
        .into_iter()
        .map(|id| {
            (
                create_render_element_object(ctx, rd.clone(), id, dirty.clone()),
                EventListenerStore::Element(id as u32),
            )
        })
        .collect::<Vec<_>>();
    if connected {
        append_document_event_targets(ctx, &mut path)?;
    }
    let event = synthetic_event(ctx, name, bubbles, cancelable);
    dispatch_bubbling_event(ctx, &event, &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_cache_preserves_live_detached_nodes_and_prunes_destroyed_incarnations() {
        let document = Rc::new(RefCell::new(Some(RenderDocument::from_html(
            "<input id='removed' value='secret'><input id='kept' value='detached'><input id='trigger'>",
            Some("https://fixture.invalid/"), Viewport::default(),
        ).unwrap())));
        let (removed, kept, trigger) = {
            let cell = document.borrow();
            let doc = cell.as_ref().unwrap();
            (
                doc.query_selector("#removed").unwrap(),
                doc.query_selector("#kept").unwrap(),
                doc.query_selector("#trigger").unwrap(),
            )
        };
        assert!(set_control_selection(&document, removed, 1, 4));
        assert!(set_control_selection(&document, kept, 2, 5));
        assert_eq!(control_selection(&document, trigger), Some((0, 0)));
        document.borrow_mut().as_mut().unwrap().remove_node(kept);
        assert_eq!(control_selection(&document, trigger), Some((0, 0)));
        assert_eq!(
            control_selection(&document, kept),
            Some((2, 5)),
            "live detached nodes retain their range"
        );
        // This unrestricted native mutation invalidates observed incarnations.
        // Accessing a different input must discard every stale cache entry.
        document
            .borrow_mut()
            .as_mut()
            .unwrap()
            .document_mut()
            .mutate()
            .remove_and_drop_node(removed);
        assert_eq!(control_selection(&document, trigger), Some((0, 0)));
        CONTROL_SELECTIONS.with(|selections| {
            let selections = selections.borrow();
            let cache = &selections[&(Rc::as_ptr(&document) as usize)].controls;
            assert_eq!(cache.len(), 1);
            assert!(cache.values().all(|entry| entry.value != "secret"));
        });
        let reused = document
            .borrow_mut()
            .as_mut()
            .unwrap()
            .create_element("input");
        assert_eq!(reused, removed, "regression must exercise slab reuse");
        document
            .borrow_mut()
            .as_mut()
            .unwrap()
            .set_attribute(reused, "value", "replacement");
        assert_eq!(control_selection(&document, reused), Some((0, 0)));
    }

    #[test]
    fn connected_listener_survives_gc_and_detached_listener_cycles_collect() {
        let output = Arc::new(RwLock::new(Vec::new()));
        let snapshot = Arc::new(RwLock::new(None));
        let mutations = Arc::new(RwLock::new(Vec::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        let document = Rc::new(RefCell::new(Some(
            RenderDocument::from_html(
                "<html><body></body></html>",
                Some("https://fixture.invalid/"),
                Viewport::default(),
            )
            .unwrap(),
        )));
        let (mut ctx, _queue) = create_context(
            &output,
            &snapshot,
            &mutations,
            &dirty,
            (640, 480),
            "https://fixture.invalid/",
            "GC regression",
            &Arc::new(RwLock::new(None)),
            &Arc::new(RwLock::new(None)),
            &document,
            false,
        );
        ctx.eval(Source::from_bytes(r#"globalThis.calls=0;(()=>{const node=document.createElement('button');node.id='kept';node.marker=42;node.addEventListener('click',()=>{calls+=node.marker});document.body.appendChild(node);globalThis.connectedRef=new WeakRef(node);})();(()=>{const other=document.createElement('button');other.addEventListener('click',()=>other.id);globalThis.detachedRef=new WeakRef(other);})();"#)).unwrap();
        ctx.clear_kept_objects();
        boa_gc::force_collect();
        assert_eq!(
            ctx.eval(Source::from_bytes("detachedRef.deref()===undefined"))
                .unwrap()
                .as_boolean(),
            Some(true),
            "unreferenced detached listener cycle must collect"
        );
        let observation = native_action::observe(
            document.borrow_mut().as_mut().unwrap(),
            1,
            "https://fixture.invalid/",
        );
        let target = observation
            .target(observation.snapshot.query_selector("#kept").unwrap())
            .unwrap();
        assert!(
            native_action::act(
                &mut ctx,
                &document,
                &dirty,
                &mutations,
                1,
                &target,
                NativeInput::Click,
                "https://fixture.invalid/"
            )
            .is_ok()
        );
        assert_eq!(
            ctx.eval(Source::from_bytes("calls")).unwrap().as_number(),
            Some(42.0)
        );
        ctx.eval(Source::from_bytes(
            "document.getElementById('kept').remove()",
        ))
        .unwrap();
        ctx.clear_kept_objects();
        boa_gc::force_collect();
        assert_eq!(
            ctx.eval(Source::from_bytes("connectedRef.deref()===undefined"))
                .unwrap()
                .as_boolean(),
            Some(true),
            "removed connected listener cycle must collect"
        );
    }
}
