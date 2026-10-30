//! Native owner tokens and a deliberately read-only same-origin frame membrane.
//! Window and Document proxies target the real child objects. Reads and queries
//! use the shared native DOM; arbitrary foreign JS/native calls are unsupported
//! until every host API dispatches by realm rather than caller Context.
use super::*;
use boa_engine::{JsData, object::builtins::JsProxy};
use boa_gc::{Finalize, Trace};

static DOCUMENT_ID: AtomicU64 = AtomicU64::new(1);
static BINDING_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct FrameOwner {
    pub parent_context_id: u32,
    pub document_id: u64,
    pub node_id: u32,
    pub incarnation: u64,
    pub attachment_revision: u64,
    pub src: Option<String>,
    pub srcdoc: Option<String>,
    pub sandbox: Option<String>,
}

#[derive(Clone, Copy)]
enum Kind {
    Window,
    Document,
    Node(usize, u64),
}

#[derive(Clone, Trace, Finalize)]
struct Binding {
    id: u64,
    child_context_id: u32,
    #[unsafe_ignore_trace]
    owner: FrameOwner,
    #[unsafe_ignore_trace]
    parent: Rc<RefCell<Option<RenderDocument>>>,
    #[unsafe_ignore_trace]
    child: Rc<RefCell<Option<RenderDocument>>>,
    #[unsafe_ignore_trace]
    alive: Rc<Cell<bool>>,
    allowed: bool,
    window: JsObject,
    document: JsObject,
}
#[derive(Clone, Trace, Finalize)]
struct Entry {
    binding: u64,
    #[unsafe_ignore_trace]
    kind: Kind,
    proxy: JsObject,
}

#[derive(Default, Trace, Finalize, JsData)]
struct Frames {
    document_id: u64,
    #[unsafe_ignore_trace]
    document_pointer: usize,
    parent_origin: Option<String>,
    bindings: HashMap<u32, Binding>,
    objects: HashMap<JsObject, Entry>,
    nodes: HashMap<(u64, usize, u64), JsObject>,
}

fn unsupported() -> boa_engine::JsError {
    JsNativeError::typ()
        .with_message("NotSupportedError: cross-frame access supports native DOM reads only")
        .into()
}
fn security() -> boa_engine::JsError {
    JsNativeError::typ()
        .with_message("SecurityError: cross-origin frame access denied")
        .into()
}
fn stale() -> boa_engine::JsError {
    JsNativeError::typ()
        .with_message("InvalidStateError: frame owner is no longer current")
        .into()
}
fn connected(doc: &RenderDocument, node: usize) -> bool {
    let mut cursor = Some(node);
    let mut seen = std::collections::HashSet::new();
    while let Some(node) = cursor {
        if !seen.insert(node) {
            return false;
        }
        if node == doc.root_element_id() {
            return true;
        }
        let Some(parent) = doc.parent_id(node) else {
            return false;
        };
        if !doc.children_ids(parent).contains(&node) {
            return false;
        }
        cursor = Some(parent);
    }
    false
}
fn current(owner: &FrameOwner, rd: &Rc<RefCell<Option<RenderDocument>>>) -> bool {
    let mut cell = rd.borrow_mut();
    let Some(doc) = cell.as_mut() else {
        return false;
    };
    let node = owner.node_id as usize;
    connected(doc, node)
        && doc.tag_name(node).as_deref() == Some("iframe")
        && doc.node_incarnation(node) == Some(owner.incarnation)
        && doc.node_attachment_revision(node) == owner.attachment_revision
        && doc.node_attr(node, "src") == owner.src
        && doc.node_attr(node, "srcdoc") == owner.srcdoc
        && doc.node_attr(node, "sandbox") == owner.sandbox
}
fn check(binding: &Binding) -> JsResult<()> {
    if !binding.alive.get() || !current(&binding.owner, &binding.parent) {
        return Err(stale());
    }
    if !binding.allowed {
        return Err(security());
    }
    if binding.child.borrow().is_none() {
        return Err(stale());
    }
    Ok(())
}
#[derive(Clone, Trace, Finalize, JsData)]
struct FrameOrigin {
    origin: Option<String>,
}
fn tuple_origin(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    match url.origin() {
        url::Origin::Tuple(..) => Some(url.origin().ascii_serialization()),
        _ => None,
    }
}
pub(super) fn set_origin(ctx: &mut Context, origin: Option<String>) {
    ctx.realm()
        .host_defined_mut()
        .insert(FrameOrigin { origin });
}
pub(super) fn child_origin(ctx: &Context, owner: &FrameOwner, child_url: &str) -> Option<String> {
    let host = ctx.realm().host_defined();
    let state = host.get::<Frames>()?;
    if state.document_id != owner.document_id {
        return None;
    }
    if owner.sandbox.as_ref().is_some_and(|sandbox| {
        !sandbox
            .split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case("allow-same-origin"))
    }) {
        return None;
    }
    // This scoped implementation cannot safely reconstruct inherited sandbox
    // flag sets from an opaque ancestor. Keep the entire descendant subtree
    // opaque rather than granting a URL-derived tuple origin again.
    state.parent_origin.as_ref()?;
    if let Some(origin) = tuple_origin(child_url) {
        return Some(origin);
    }
    if (child_url == "about:srcdoc" && owner.srcdoc.is_some())
        || (child_url == "about:blank"
            && owner.srcdoc.is_none()
            && owner.src.as_deref().is_none_or(|src| src == "about:blank"))
    {
        return state.parent_origin.clone();
    }
    None
}
fn ensure(ctx: &mut Context, rd: &Rc<RefCell<Option<RenderDocument>>>, url: &str) {
    let pointer = Rc::as_ptr(rd) as usize;
    let origin = ctx
        .realm()
        .host_defined()
        .get::<FrameOrigin>()
        .map(|origin| origin.origin.clone())
        .unwrap_or_else(|| tuple_origin(url));
    let realm = ctx.realm().clone();
    let mut host = realm.host_defined_mut();
    if host
        .get::<Frames>()
        .is_some_and(|state| state.document_pointer != pointer)
        && let Some(state) = host.remove::<Frames>()
    {
        for binding in state.bindings.values() {
            binding.alive.set(false);
        }
    }
    if host.get::<Frames>().is_none() {
        host.insert(Frames {
            document_id: DOCUMENT_ID.fetch_add(1, Ordering::Relaxed),
            document_pointer: pointer,
            parent_origin: origin,
            bindings: HashMap::new(),
            objects: HashMap::new(),
            nodes: HashMap::new(),
        });
    }
}

pub(super) fn owners(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    context_id: u32,
    url: &str,
) -> Vec<FrameOwner> {
    ensure(ctx, rd, url);
    let document_id = ctx
        .realm()
        .host_defined()
        .get::<Frames>()
        .unwrap()
        .document_id;
    let mut cell = rd.borrow_mut();
    let Some(doc) = cell.as_mut() else {
        return Vec::new();
    };
    doc.query_selector_all("iframe")
        .into_iter()
        .filter_map(|node| {
            if !connected(doc, node) {
                return None;
            }
            Some(FrameOwner {
                parent_context_id: context_id,
                document_id,
                node_id: node as u32,
                incarnation: doc.node_incarnation(node)?,
                attachment_revision: doc.node_attachment_revision(node),
                src: doc.node_attr(node, "src"),
                srcdoc: doc.node_attr(node, "srcdoc"),
                sandbox: doc.node_attr(node, "sandbox"),
            })
        })
        .collect()
}
pub(super) fn reset(ctx: &mut Context) {
    ctx.realm().host_defined_mut().remove::<FrameOrigin>();
    if let Some(state) = ctx.realm().host_defined_mut().remove::<Frames>() {
        for binding in state.bindings.values() {
            binding.alive.set(false);
        }
    }
}
pub(super) fn revoke_child(ctx: &mut Context, child_context_id: u32) {
    let realm = ctx.realm().clone();
    let mut host = realm.host_defined_mut();
    if let Some(state) = host.get_mut::<Frames>() {
        for binding in state
            .bindings
            .values()
            .filter(|binding| binding.child_context_id == child_context_id)
        {
            binding.alive.set(false);
            clear_pixels(binding);
        }
        let removed = state
            .bindings
            .values()
            .filter(|binding| binding.child_context_id == child_context_id)
            .map(|binding| binding.id)
            .collect::<std::collections::HashSet<_>>();
        state
            .bindings
            .retain(|_, binding| binding.child_context_id != child_context_id);
        state
            .objects
            .retain(|_, entry| !removed.contains(&entry.binding));
        state
            .nodes
            .retain(|(binding, _, _), _| !removed.contains(binding));
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn attach(
    ctx: &mut Context,
    parent: Rc<RefCell<Option<RenderDocument>>>,
    owner: &FrameOwner,
    child_context_id: u32,
    window: JsObject,
    document: JsObject,
    child: Rc<RefCell<Option<RenderDocument>>>,
    child_url: &str,
) -> JsResult<()> {
    let (generation, parent_origin) = ctx
        .realm()
        .host_defined()
        .get::<Frames>()
        .map(|state| (state.document_id, state.parent_origin.clone()))
        .ok_or_else(stale)?;
    if generation != owner.document_id || !current(owner, &parent) {
        return Err(stale());
    }
    let origin = child_origin(ctx, owner, child_url);
    let allowed = parent_origin.is_some() && origin == parent_origin;
    let binding = Binding {
        id: BINDING_ID.fetch_add(1, Ordering::Relaxed),
        child_context_id,
        owner: owner.clone(),
        parent,
        child,
        alive: Rc::new(Cell::new(true)),
        allowed,
        window,
        document,
    };
    let realm = ctx.realm().clone();
    let mut host = realm.host_defined_mut();
    let state = host.get_mut::<Frames>().unwrap();
    if let Some(previous) = state.bindings.insert(owner.node_id, binding) {
        previous.alive.set(false);
        clear_pixels(&previous);
        state
            .objects
            .retain(|_, entry| entry.binding != previous.id);
        state
            .nodes
            .retain(|(binding, _, _), _| *binding != previous.id);
    }
    Ok(())
}

pub(super) fn install(ctx: &mut Context, object: &JsObject, _node: usize) {
    for (name, is_window) in [("contentWindow", true), ("contentDocument", false)] {
        let getter = NativeFunction::from_copy_closure_with_captures(
            |this, _, is_window, ctx| {
                let Some(object) = this.as_object() else {
                    return Ok(JsValue::null());
                };
                let Some(node) = dom_realm::node_id(ctx, &object) else {
                    return Ok(JsValue::null());
                };
                let binding = ctx
                    .realm()
                    .host_defined()
                    .get::<Frames>()
                    .and_then(|state| state.bindings.get(&(node as u32)))
                    .cloned();
                let Some(binding) = binding else {
                    return Ok(JsValue::null());
                };
                if dom_realm::node_id_in_document(ctx, &object, &binding.parent) != Some(node) {
                    return Ok(JsValue::null());
                }
                if !binding.alive.get() || !current(&binding.owner, &binding.parent) {
                    return Ok(JsValue::null());
                }
                if !binding.allowed && !is_window {
                    return Ok(JsValue::null());
                }
                wrap(
                    ctx,
                    &binding,
                    if *is_window {
                        Kind::Window
                    } else {
                        Kind::Document
                    },
                )
            },
            is_window,
        )
        .to_js_function(ctx.realm());
        let _ = object.define_property_or_throw(
            JsString::from(name),
            boa_engine::property::PropertyDescriptor::builder()
                .get(getter)
                .configurable(true),
            ctx,
        );
    }
}

fn binding(ctx: &Context, id: u64) -> JsResult<Binding> {
    let result = ctx
        .realm()
        .host_defined()
        .get::<Frames>()
        .and_then(|state| state.bindings.values().find(|binding| binding.id == id))
        .cloned()
        .ok_or_else(stale)?;
    check(&result)?;
    Ok(result)
}
fn denied(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Err(unsupported())
}
fn wrap(ctx: &mut Context, binding: &Binding, kind: Kind) -> JsResult<JsValue> {
    if let Kind::Node(node, incarnation) = kind
        && binding
            .child
            .borrow_mut()
            .as_mut()
            .and_then(|doc| doc.node_incarnation(node))
            != Some(incarnation)
    {
        return Err(stale());
    }
    if let Kind::Node(node, incarnation) = kind
        && let Some(proxy) = ctx
            .realm()
            .host_defined()
            .get::<Frames>()
            .and_then(|state| state.nodes.get(&(binding.id, node, incarnation)))
    {
        return Ok(proxy.clone().into());
    }
    let target = match kind {
        Kind::Window => binding.window.clone(),
        Kind::Document => binding.document.clone(),
        // This is a native read wrapper over the child's actual DOM node.
        // Do not invoke the general element factory in the parent's realm:
        // image/custom-element hooks could expose its unrestricted raw object.
        Kind::Node(_, _) => JsObject::with_null_proto(),
    };
    if let Some(entry) = ctx
        .realm()
        .host_defined()
        .get::<Frames>()
        .and_then(|state| state.objects.get(&target))
        .filter(|entry| entry.binding == binding.id)
    {
        return Ok(entry.proxy.clone().into());
    }
    let proxy = JsProxy::builder(target.clone())
        .get(read)
        .has(denied)
        .is_extensible(denied)
        .set(denied)
        .define_property(denied)
        .delete_property(denied)
        .get_prototype_of(denied)
        .set_prototype_of(denied)
        .get_own_property_descriptor(denied)
        .own_keys(denied)
        .prevent_extensions(denied)
        .build(ctx)?;
    let proxy: JsObject = proxy.into();
    ctx.realm()
        .host_defined_mut()
        .get_mut::<Frames>()
        .unwrap()
        .objects
        .insert(
            target,
            Entry {
                binding: binding.id,
                kind,
                proxy: proxy.clone(),
            },
        );
    if let Kind::Node(node, incarnation) = kind {
        ctx.realm()
            .host_defined_mut()
            .get_mut::<Frames>()
            .unwrap()
            .nodes
            .insert((binding.id, node, incarnation), proxy.clone());
    }
    Ok(proxy.into())
}
fn wrap_node(ctx: &mut Context, binding: &Binding, node: usize) -> JsResult<JsValue> {
    let incarnation = binding
        .child
        .borrow_mut()
        .as_mut()
        .and_then(|doc| doc.node_incarnation(node))
        .ok_or_else(stale)?;
    wrap(ctx, binding, Kind::Node(node, incarnation))
}
fn read(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let target = args
        .first()
        .and_then(JsValue::as_object)
        .ok_or_else(stale)?;
    let entry = ctx
        .realm()
        .host_defined()
        .get::<Frames>()
        .and_then(|state| state.objects.get(&target))
        .cloned()
        .ok_or_else(stale)?;
    let binding = binding(ctx, entry.binding)?;
    if let Kind::Node(node, incarnation) = entry.kind
        && binding
            .child
            .borrow_mut()
            .as_mut()
            .and_then(|doc| doc.node_incarnation(node))
            != Some(incarnation)
    {
        return Err(stale());
    }
    let key = args
        .get(1)
        .and_then(JsValue::as_string)
        .map(|key| key.to_std_string_escaped())
        .ok_or_else(unsupported)?;
    match (entry.kind, key.as_str()) {
        (Kind::Window, "document") | (Kind::Node(_, _), "ownerDocument") => {
            wrap(ctx, &binding, Kind::Document)
        }
        (Kind::Document, "defaultView") => wrap(ctx, &binding, Kind::Window),
        (Kind::Document, "body" | "head" | "documentElement") => {
            let selector = match key.as_str() {
                "body" => "body",
                "head" => "head",
                _ => "html",
            };
            let node = binding
                .child
                .borrow()
                .as_ref()
                .and_then(|doc| doc.query_selector(selector));
            node.map(|node| wrap_node(ctx, &binding, node))
                .unwrap_or(Ok(JsValue::null()))
        }
        (Kind::Document, "getElementById" | "querySelector")
        | (Kind::Node(_, _), "querySelector" | "getAttribute" | "hasAttribute") => {
            let capture = Method {
                binding: binding.id,
                kind: entry.kind,
                name: key,
            };
            Ok(NativeFunction::from_copy_closure_with_captures(
                |_, args, capture, ctx| method(ctx, capture, args),
                capture,
            )
            .to_js_function(ctx.realm())
            .into())
        }
        (Kind::Node(node, _), "textContent" | "id" | "tagName" | "nodeName" | "nodeType") => {
            let cell = binding.child.borrow();
            let doc = cell.as_ref().ok_or_else(stale)?;
            match key.as_str() {
                "textContent" => Ok(JsString::from(doc.node_text(node)).into()),
                "id" => Ok(JsString::from(doc.node_attr(node, "id").unwrap_or_default()).into()),
                "nodeType" => Ok(1.into()),
                _ => Ok(JsString::from(
                    doc.tag_name(node).unwrap_or_default().to_ascii_uppercase(),
                )
                .into()),
            }
        }
        _ => Err(unsupported()),
    }
}
#[derive(Trace, Finalize)]
struct Method {
    binding: u64,
    #[unsafe_ignore_trace]
    kind: Kind,
    name: String,
}
fn method(ctx: &mut Context, capture: &Method, args: &[JsValue]) -> JsResult<JsValue> {
    let binding = binding(ctx, capture.binding)?;
    let argument = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();
    check(&binding)?;
    if let Kind::Node(node, incarnation) = capture.kind
        && binding
            .child
            .borrow_mut()
            .as_mut()
            .and_then(|doc| doc.node_incarnation(node))
            != Some(incarnation)
    {
        return Err(stale());
    }
    if matches!(capture.name.as_str(), "getAttribute" | "hasAttribute") {
        let Kind::Node(node, _) = capture.kind else {
            return Err(unsupported());
        };
        let value = binding
            .child
            .borrow()
            .as_ref()
            .and_then(|doc| doc.node_attr(node, &argument));
        return if capture.name == "hasAttribute" {
            Ok(value.is_some().into())
        } else {
            Ok(value
                .map(|value| JsString::from(value).into())
                .unwrap_or(JsValue::null()))
        };
    }
    let node = {
        let cell = binding.child.borrow();
        let doc = cell.as_ref().ok_or_else(stale)?;
        if capture.name == "getElementById" {
            doc.query_selector_all("*")
                .into_iter()
                .find(|node| doc.node_attr(*node, "id").as_deref() == Some(argument.as_str()))
        } else if let Kind::Node(node, _) = capture.kind {
            doc.query_selector_all_from(node, &argument)
                .map_err(|message| JsNativeError::syntax().with_message(message))?
                .first()
                .copied()
        } else {
            doc.query_selector(&argument)
        }
    };
    node.map(|node| wrap_node(ctx, &binding, node))
        .unwrap_or(Ok(JsValue::null()))
}

/// Opaque is a native realm property, never a page-controlled global marker.
pub(super) fn is_opaque(ctx: &Context) -> bool {
    ctx.realm()
        .host_defined()
        .get::<FrameOrigin>()
        .is_some_and(|origin| origin.origin.is_none())
}

#[derive(Trace, Finalize, JsData)]
struct OpaqueGuard;
fn deny_opaque_call(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let error = JsNativeError::error()
        .with_message("Storage and network APIs are unsupported for an opaque frame origin")
        .into_opaque(ctx);
    error.set(js_string!("name"), js_string!("SecurityError"), true, ctx)?;
    Err(boa_engine::JsError::from_opaque(error.into()))
}

/// Install before the first page script. Until opaque CORS and isolated
/// storage exist, these APIs fail explicitly rather than using URL-origin
/// credentials. The fetch bridge independently rejects opaque-origin requests
/// from internal APIs which captured the original fetch callable at bootstrap.
pub(super) fn guard_opaque(ctx: &mut Context) -> JsResult<()> {
    if !is_opaque(ctx) || ctx.realm().host_defined().get::<OpaqueGuard>().is_some() {
        return Ok(());
    }
    let global = ctx.global_object();
    let document = global.get(js_string!("document"), ctx)?.as_object();
    let window = global.get(js_string!("window"), ctx)?.as_object();
    let mut targets = vec![global];
    if let Some(window) = window
        && window != targets[0]
    {
        targets.push(window);
    }
    let denied = NativeFunction::from_fn_ptr(deny_opaque_call).to_js_function(ctx.realm());
    for target in &targets {
        for name in ["localStorage", "sessionStorage", "indexedDB"] {
            target.define_property_or_throw(
                JsString::from(name),
                boa_engine::property::PropertyDescriptor::builder()
                    .get(denied.clone())
                    .set(denied.clone())
                    .enumerable(true)
                    .configurable(false),
                ctx,
            )?;
        }
        for name in ["fetch", "XMLHttpRequest", "WebSocket"] {
            let function = FunctionObjectBuilder::new(
                ctx.realm(),
                NativeFunction::from_fn_ptr(deny_opaque_call),
            )
            .name(JsString::from(name))
            .constructor(true)
            .build();
            target.define_property_or_throw(
                JsString::from(name),
                boa_engine::property::PropertyDescriptor::builder()
                    .value(function)
                    .writable(false)
                    .enumerable(false)
                    .configurable(false),
                ctx,
            )?;
        }
    }
    if let Some(document) = document {
        document.define_property_or_throw(
            js_string!("cookie"),
            boa_engine::property::PropertyDescriptor::builder()
                .get(denied.clone())
                .set(denied)
                .enumerable(true)
                .configurable(false),
            ctx,
        )?;
    }
    ctx.realm().host_defined_mut().insert(OpaqueGuard);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct FrameHit {
    pub context_id: u32,
    pub x: f64,
    pub y: f64,
    pub owner_node_id: u32,
}

/// The child viewport is the owner's local content box, excluding borders.
pub(super) fn owner_viewport(
    ctx: &Context,
    parent: &Rc<RefCell<Option<RenderDocument>>>,
    owner: &FrameOwner,
) -> JsResult<(u32, u32)> {
    let generation = ctx
        .realm()
        .host_defined()
        .get::<Frames>()
        .map(|state| state.document_id);
    if generation != Some(owner.document_id) || !current(owner, parent) {
        return Err(stale());
    }
    let mut cell = parent.borrow_mut();
    let doc = cell.as_mut().ok_or_else(stale)?;
    doc.flush_layout();
    let node = doc
        .document()
        .get_node(owner.node_id as usize)
        .ok_or_else(stale)?;
    let width = node.final_layout.content_box_width() as f64;
    let height = node.final_layout.content_box_height() as f64;
    if !width.is_finite() || !height.is_finite() || width > 16384.0 || height > 16384.0 {
        return Err(unsupported());
    }
    Ok((width.ceil().max(1.0) as u32, height.ceil().max(1.0) as u32))
}

/// Project a child viewport quad through its current native iframe content box.
/// CDP geometry and trusted input may cross origins without granting page JS
/// access through the read membrane.
pub(super) fn project_child_quad(
    ctx: &Context,
    child_id: u32,
    points: [f64; 8],
) -> JsResult<[f64; 8]> {
    let binding = {
        let host = ctx.realm().host_defined();
        let state = host.get::<Frames>().ok_or_else(stale)?;
        let binding = state
            .bindings
            .values()
            .find(|binding| binding.child_context_id == child_id)
            .ok_or_else(stale)?;
        if state.document_id != binding.owner.document_id || !binding.alive.get() {
            return Err(stale());
        }
        binding.clone()
    };
    if !current(&binding.owner, &binding.parent) {
        return Err(stale());
    }
    let quad = {
        let mut cell = binding.parent.borrow_mut();
        let doc = cell.as_mut().ok_or_else(stale)?;
        doc.flush_layout();
        doc.node_box_quads(binding.owner.node_id as usize)
            .ok_or_else(unsupported)?
            .0
    };
    let viewport = binding
        .child
        .borrow()
        .as_ref()
        .ok_or_else(stale)?
        .viewport();
    let width = viewport.width as f64;
    let height = viewport.height as f64;
    let ax = quad[2] - quad[0];
    let ay = quad[3] - quad[1];
    let bx = quad[6] - quad[0];
    let by = quad[7] - quad[1];
    if width <= 0.0 || height <= 0.0 || (ax * by - ay * bx).abs() < 1e-10 {
        return Err(unsupported());
    }
    let mut projected = [0.0; 8];
    for (point, output) in points
        .as_chunks::<2>()
        .0
        .iter()
        .zip(projected.as_chunks_mut::<2>().0.iter_mut())
    {
        output[0] = quad[0] + point[0] * ax / width + point[1] * bx / height;
        output[1] = quad[1] + point[0] * ay / width + point[1] * by / height;
    }
    if !projected.iter().all(|value| value.is_finite()) {
        return Err(unsupported());
    }
    Ok(projected)
}

/// One native hit-test step. CDP input may target cross-origin frames; the
/// read membrane's same-origin permission is deliberately not used here.
pub(super) fn hit_child(ctx: &Context, x: f64, y: f64) -> Option<FrameHit> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let bindings = ctx
        .realm()
        .host_defined()
        .get::<Frames>()?
        .bindings
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let parent = bindings.first()?.parent.clone();
    let mut cell = parent.borrow_mut();
    let doc = cell.as_mut()?;
    doc.flush_layout();
    // Use the painter's native hit traversal, including overflow, transforms,
    // stacking order and pointer-events. A zero-height positioned parent may
    // still contain a visible, clickable iframe.
    let scroll = doc.document().viewport_scroll();
    let hit = doc
        .document()
        .hit((x + scroll.x) as f32, (y + scroll.y) as f32)?
        .node_id as u32;
    let binding = bindings
        .iter()
        .find(|binding| binding.owner.node_id == hit && binding.alive.get())?;
    let quad = doc.node_box_quads(hit as usize)?.0;
    drop(cell);
    if !current(&binding.owner, &binding.parent) {
        return None;
    }
    let ax = quad[2] - quad[0];
    let ay = quad[3] - quad[1];
    let bx = quad[6] - quad[0];
    let by = quad[7] - quad[1];
    let determinant = ax * by - ay * bx;
    if determinant.abs() < 1e-10 {
        return None;
    }
    let dx = x - quad[0];
    let dy = y - quad[1];
    let u = (dx * by - dy * bx) / determinant;
    let v = (ax * dy - ay * dx) / determinant;
    if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
        return None;
    }
    let viewport = binding.child.borrow().as_ref()?.viewport();
    Some(FrameHit {
        context_id: binding.child_context_id,
        x: u * viewport.width as f64,
        y: v * viewport.height as f64,
        owner_node_id: hit,
    })
}
pub(super) fn children(ctx: &Context) -> Vec<u32> {
    ctx.realm()
        .host_defined()
        .get::<Frames>()
        .map(|state| {
            state
                .bindings
                .values()
                .filter(|binding| binding.alive.get() && current(&binding.owner, &binding.parent))
                .map(|binding| binding.child_context_id)
                .collect()
        })
        .unwrap_or_default()
}

fn clear_pixels(binding: &Binding) {
    if let Some(doc) = binding.parent.borrow_mut().as_mut()
        && doc.node_incarnation(binding.owner.node_id as usize) == Some(binding.owner.incarnation)
    {
        doc.clear_canvas_bitmap(binding.owner.node_id as usize);
    }
}

/// Called in postorder by the JS-thread capture command. Only genuine child
/// pixels are installed, with the existing iframe identity and CSS box intact.
pub(super) fn composite(ctx: &mut Context) -> JsResult<()> {
    let bindings = ctx
        .realm()
        .host_defined()
        .get::<Frames>()
        .map(|state| state.bindings.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for binding in bindings {
        if !binding.alive.get() || !current(&binding.owner, &binding.parent) {
            if let Some(doc) = binding.parent.borrow_mut().as_mut()
                && doc.node_incarnation(binding.owner.node_id as usize)
                    == Some(binding.owner.incarnation)
            {
                doc.clear_canvas_bitmap(binding.owner.node_id as usize);
            }
            continue;
        }
        let png = binding
            .child
            .borrow_mut()
            .as_mut()
            .ok_or_else(stale)?
            .capture_png(&CaptureOpts {
                viewport: None,
                full_page: false,
            })
            .map_err(|error| JsNativeError::error().with_message(error.to_string()))?;
        let pixels = ::image::load_from_memory(&png)
            .map_err(|error| JsNativeError::error().with_message(error.to_string()))?
            .into_rgba8();
        let (width, height) = pixels.dimensions();
        if let Some(doc) = binding.parent.borrow_mut().as_mut()
            && !doc.set_canvas_bitmap(
                binding.owner.node_id as usize,
                width,
                height,
                Arc::new(pixels.into_raw()),
            )
        {
            return Err(stale());
        }
    }
    Ok(())
}

#[derive(Trace, Finalize, JsData)]
struct NativeFocus {
    #[unsafe_ignore_trace]
    document: usize,
    node: usize,
    incarnation: u64,
}
pub(super) fn record_focus(
    ctx: &mut Context,
    document: &Rc<RefCell<Option<RenderDocument>>>,
    node: Option<usize>,
) {
    let focus = node.and_then(|node| {
        document
            .borrow_mut()
            .as_mut()
            .and_then(|doc| doc.node_incarnation(node))
            .map(|incarnation| NativeFocus {
                document: Rc::as_ptr(document) as usize,
                node,
                incarnation,
            })
    });
    let realm = ctx.realm().clone();
    let mut host = realm.host_defined_mut();
    host.remove::<NativeFocus>();
    if let Some(focus) = focus {
        host.insert(focus);
    }
}
pub(super) fn password_focused(
    ctx: &Context,
    document: &Rc<RefCell<Option<RenderDocument>>>,
) -> bool {
    let Some(node) = dom_realm::focused_node(document) else {
        return false;
    };
    let host = ctx.realm().host_defined();
    let expected = host
        .get::<NativeFocus>()
        .filter(|focus| focus.document == Rc::as_ptr(document) as usize && focus.node == node)
        .map(|focus| focus.incarnation);
    let mut cell = document.borrow_mut();
    let Some(doc) = cell.as_mut() else {
        return false;
    };
    connected(doc, node)
        && expected.is_none_or(|token| doc.node_incarnation(node) == Some(token))
        && doc.tag_name(node).as_deref() == Some("input")
        && doc
            .node_attr(node, "type")
            .is_some_and(|kind| kind.eq_ignore_ascii_case("password"))
}
