//! Host-only commands. No page global, prototype, selector function, or callback
//! return value is used to resolve a target or construct an action receipt.
use super::*;
use std::cell::Cell;

#[derive(Debug, Clone)]
pub struct NativeTarget {
    document_id: u64,
    tree_revision: u64,
    node_id: u32,
    incarnation: u64,
    attachment_revision: u64,
    fingerprint: String,
    attributes: HashMap<String, String>,
}

pub struct NativeObservation {
    pub snapshot: DomSnapshot,
    document_id: u64,
    tree_revision: u64,
    incarnations: HashMap<u32, (u64, u64)>,
}

impl NativeObservation {
    pub(super) fn empty(document_id: u64) -> Self {
        Self {
            snapshot: DomSnapshot::empty(),
            document_id,
            tree_revision: 0,
            incarnations: HashMap::new(),
        }
    }
    /// Only nodes connected to the physical main document can become targets.
    pub fn target(&self, node_id: u32) -> Option<NativeTarget> {
        Some(NativeTarget {
            document_id: self.document_id,
            tree_revision: self.tree_revision,
            node_id,
            incarnation: self.incarnations.get(&node_id)?.0,
            attachment_revision: self.incarnations.get(&node_id)?.1,
            fingerprint: self.snapshot.fingerprint(node_id),
            attributes: self.snapshot.nodes.get(&node_id)?.attributes.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum NativeInput {
    Click,
    /// Literal text fill. Password controls are denied — only the
    /// broker-resolved [`NativeInput::FillCredential`] may fill those.
    Fill(String),
    /// Broker-authorized credential fill (`OXI.fillCredential` / the REPL
    /// host): the caller resolved the value through the credential broker
    /// (policy + consent + audit) before constructing this input. May fill
    /// `input[type=password]`; the value never transits page JS.
    FillCredential(String),
}

#[derive(Debug, Clone)]
pub struct NativeReceipt {
    pub callbacks: usize,
    pub navigation_requested: bool,
}

pub(super) struct Failure {
    pub message: &'static str,
    pub may_have_acted: bool,
}

fn stale() -> Failure {
    Failure {
        message: "stale ref — re-observe",
        may_have_acted: false,
    }
}

fn failed(message: &'static str, may_have_acted: bool) -> Failure {
    Failure {
        message,
        may_have_acted,
    }
}

fn connected(doc: &RenderDocument, node: usize) -> bool {
    let mut cursor = Some(node);
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return false;
        }
        if id == doc.root_element_id() {
            return true;
        }
        let Some(parent) = doc.document().get_node(id).and_then(|n| n.parent) else {
            return false;
        };
        if !doc
            .document()
            .get_node(parent)
            .is_some_and(|p| p.children.contains(&id))
        {
            return false;
        }
        cursor = Some(parent);
    }
    false
}

pub(super) fn observe(doc: &mut RenderDocument, document_id: u64, url: &str) -> NativeObservation {
    let title = doc
        .query_selector("title")
        .map(|id| doc.node_text(id))
        .unwrap_or_default();
    let snapshot = DomSnapshot::from_render_document(doc, url, &title);
    let incarnations = snapshot
        .nodes
        .keys()
        .copied()
        .filter_map(|id| {
            if !connected(doc, id as usize) {
                return None;
            }
            doc.node_incarnation(id as usize)
                .map(|token| (id, (token, doc.node_attachment_revision(id as usize))))
        })
        .collect();
    NativeObservation {
        snapshot,
        document_id,
        tree_revision: doc.tree_revision(),
        incarnations,
    }
}

pub(super) fn pending_navigation(mutations: &Arc<RwLock<Vec<DomMutation>>>) -> bool {
    mutations.read().iter().any(|m| {
        matches!(
            m,
            DomMutation::Navigate { .. } | DomMutation::NavigatePost { .. } | DomMutation::Reload
        )
    })
}

/// A bounded pump for the native path. Does not use page eval or inspect thenables.
/// Network/WebSocket/IDB completion pumping retains the existing engine path.
pub(super) fn pump(
    ctx: &mut Context,
    queue: &Rc<TokioJobQueue>,
) -> std::result::Result<(), Failure> {
    let start = Instant::now();
    let mut budget = 256usize;
    loop {
        let jobs = queue
            .run_bounded_jobs(ctx, budget)
            .map_err(|_| failed("NATIVE_JOB_FAILED_OR_LIMIT", true))?;
        budget -= jobs;
        if budget == 0 || start.elapsed() > Duration::from_millis(500) {
            if queue
                .next_timer_deadline()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                return Err(failed("NATIVE_JOB_FAILED_OR_LIMIT", true));
            }
            return Ok(());
        }
        let Some(timer) = queue.pop_due_timers_bounded(1).pop() else {
            return Ok(());
        };
        if !queue.timer_is_active(timer.id) {
            continue;
        }
        budget -= 1;
        let result = timer.callback.call(&JsValue::undefined(), &timer.args, ctx);
        queue.complete_timer(timer, result.is_ok());
        result.map_err(|_| failed("NATIVE_JOB_FAILED_OR_LIMIT", true))?;
    }
}

fn readonly(ctx: &mut Context, obj: &JsObject, key: &'static str, value: JsValue) {
    obj.define_property_or_throw(
        JsString::from(key),
        boa_engine::property::PropertyDescriptor::builder()
            .value(value)
            .writable(false)
            .enumerable(true)
            .configurable(false),
        ctx,
    )
    .expect("fresh host event property");
}

fn function(ctx: &mut Context, obj: &JsObject, key: &'static str, native: NativeFunction) {
    let function = FunctionObjectBuilder::new(ctx.realm(), native)
        .name(JsString::from(key))
        .build();
    readonly(ctx, obj, key, function.into());
}

fn event_current(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    node: u32,
) -> JsValue {
    match node {
        u32::MAX => ctx
            .global_object()
            .get(js_string!("window"), ctx)
            .unwrap_or_default(),
        n if n == u32::MAX - 1 => ctx
            .global_object()
            .get(js_string!("document"), ctx)
            .unwrap_or_default(),
        n if n == u32::MAX - 2 => JsValue::null(),
        n => create_render_element_object(ctx, render_doc.clone(), n as usize, dirty.clone()),
    }
}
#[allow(clippy::too_many_arguments)]
fn event(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    target: usize,
    current: &Rc<Cell<u32>>,
    phase: &Rc<Cell<u32>>,
    passive: &Rc<Cell<bool>>,
    kind: &'static str,
    stopped: &Rc<Cell<bool>>,
    immediate: &Rc<Cell<bool>>,
    prevented: &Rc<Cell<bool>>,
) -> JsObject {
    let target_object =
        create_render_element_object(ctx, render_doc.clone(), target, dirty.clone());
    let obj = JsObject::with_null_proto();
    readonly(ctx, &obj, "type", JsString::from(kind).into());
    readonly(ctx, &obj, "target", target_object);
    readonly(
        ctx,
        &obj,
        "bubbles",
        (!matches!(kind, "focus" | "blur" | "invalid")).into(),
    );
    readonly(
        ctx,
        &obj,
        "cancelable",
        matches!(kind, "click" | "submit" | "invalid").into(),
    );
    readonly(ctx, &obj, "isTrusted", false.into());
    let state = prevented.clone();
    let getter = FunctionObjectBuilder::new(ctx.realm(), unsafe {
        NativeFunction::from_closure(move |_, _, _| Ok(state.get().into()))
    })
    .build();
    obj.define_property_or_throw(
        js_string!("defaultPrevented"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(getter)
            .enumerable(true)
            .configurable(false),
        ctx,
    )
    .expect("fresh host event accessor");
    let current = current.clone();
    let rd = render_doc.clone();
    let dirty = dirty.clone();
    let getter = FunctionObjectBuilder::new(ctx.realm(), unsafe {
        NativeFunction::from_closure(move |_, _, ctx| {
            Ok(event_current(ctx, &rd, &dirty, current.get()))
        })
    })
    .build();
    obj.define_property_or_throw(
        js_string!("currentTarget"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(getter)
            .enumerable(true)
            .configurable(false),
        ctx,
    )
    .expect("fresh host event accessor");
    let phase = phase.clone();
    let getter = FunctionObjectBuilder::new(ctx.realm(), unsafe {
        NativeFunction::from_closure(move |_, _, _| Ok(phase.get().into()))
    })
    .build();
    obj.define_property_or_throw(
        js_string!("eventPhase"),
        boa_engine::property::PropertyDescriptor::builder()
            .get(getter)
            .enumerable(true)
            .configurable(false),
        ctx,
    )
    .expect("fresh host event accessor");
    let stop = stopped.clone();
    function(ctx, &obj, "stopPropagation", unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            stop.set(true);
            Ok(JsValue::undefined())
        })
    });
    let stop = stopped.clone();
    let now = immediate.clone();
    function(ctx, &obj, "stopImmediatePropagation", unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            stop.set(true);
            now.set(true);
            Ok(JsValue::undefined())
        })
    });
    let cancel = prevented.clone();
    let passive = passive.clone();
    function(ctx, &obj, "preventDefault", unsafe {
        NativeFunction::from_closure(move |_, _, _| {
            if matches!(kind, "click" | "submit" | "invalid") && !passive.get() {
                cancel.set(true);
            }
            Ok(JsValue::undefined())
        })
    });
    obj
}

fn still_same(doc: &mut RenderDocument, node: usize, token: u64, attachment: u64) -> bool {
    connected(doc, node)
        && doc.node_incarnation(node) == Some(token)
        && doc.node_attachment_revision(node) == attachment
}

#[allow(clippy::too_many_arguments)]
fn deliver(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    target: &NativeTarget,
    kind: &'static str,
    callbacks: &mut usize,
    prevented: &Rc<Cell<bool>>,
) -> std::result::Result<(), Failure> {
    let route = {
        let mut cell = render_doc.borrow_mut();
        let doc = cell.as_mut().ok_or_else(stale)?;
        let mut id = Some(target.node_id as usize);
        let mut route = Vec::new();
        while let Some(node) = id {
            if route.len() >= 1024 {
                return Err(failed("NATIVE_EVENT_ROUTE_LIMIT", true));
            }
            if doc.tag_name(node).is_none() {
                break;
            }
            route.push((
                node,
                doc.node_incarnation(node).ok_or_else(stale)?,
                doc.node_attachment_revision(node),
            ));
            id = doc.document().get_node(node).and_then(|n| n.parent);
        }
        route
    };
    let stopped = Rc::new(Cell::new(false));
    let immediate = Rc::new(Cell::new(false));
    let current = Rc::new(Cell::new(u32::MAX - 2));
    let phase = Rc::new(Cell::new(0));
    let passive = Rc::new(Cell::new(false));
    let event = event(
        ctx,
        render_doc,
        dirty,
        target.node_id as usize,
        &current,
        &phase,
        &passive,
        kind,
        &stopped,
        &immediate,
        prevented,
    );
    let mut route = route;
    route.push(((u32::MAX - 1) as usize, 0, 0));
    route.push((u32::MAX as usize, 0, 0));
    let mut visits = (1..route.len())
        .rev()
        .map(|i| (i, true, 1))
        .collect::<Vec<_>>();
    visits.push((0, true, 2));
    visits.push((0, false, 2));
    if !matches!(kind, "focus" | "blur" | "invalid") {
        visits.extend((1..route.len()).map(|i| (i, false, 3)));
    }
    let mut previous = None;
    for (index, capture, event_phase) in visits {
        if immediate.get()
            || pending_navigation(mutations)
            || (previous != Some(index) && stopped.get())
        {
            break;
        }
        previous = Some(index);
        let (node, token, attachment) = route[index];
        current.set(node as u32);
        phase.set(event_phase);
        let this = event_current(ctx, render_doc, dirty, node as u32);
        for listener in
            registry_get_native(node as u32, token, kind, Rc::as_ptr(render_doc) as usize)
        {
            if immediate.get() || pending_navigation(mutations) {
                break;
            }
            if listener.removed.get() || listener.capture != capture {
                continue;
            }
            let Some(callback) = listener.callback(ctx) else {
                continue;
            };
            {
                let mut cell = render_doc.borrow_mut();
                let doc = cell
                    .as_mut()
                    .ok_or_else(|| failed("NATIVE_TARGET_DETACHED_DURING_ACTION", true))?;
                if !still_same(
                    doc,
                    target.node_id as usize,
                    target.incarnation,
                    target.attachment_revision,
                ) || (node < (u32::MAX - 1) as usize
                    && !still_same(doc, node, token, attachment))
                {
                    return Err(failed("NATIVE_TARGET_DETACHED_DURING_ACTION", true));
                }
            }
            if *callbacks >= 256 {
                return Err(failed("NATIVE_LISTENER_LIMIT", true));
            }
            if listener.once {
                listener.removed.set(true);
                if let Some(record) = listener.record(ctx) {
                    let _ = record.set(js_string!("removed"), true, true, ctx);
                }
            }
            *callbacks += 1;
            passive.set(listener.passive);
            let result = if callback.is_callable() {
                callback.call(&this, &[event.clone().into()], ctx)
            } else {
                callback
                    .get(js_string!("handleEvent"), ctx)
                    .and_then(|value| {
                        let callback = value.as_callable().ok_or_else(|| {
                            JsNativeError::typ()
                                .with_message("listener.handleEvent is not callable")
                        })?;
                        callback.call(&callback.clone().into(), &[event.clone().into()], ctx)
                    })
            };
            passive.set(false);
            result.map_err(|_| failed("NATIVE_LISTENER_ERROR", true))?;
        }
    }
    current.set(u32::MAX - 2);
    phase.set(0);

    Ok(())
}

fn focus_target(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    target: &NativeTarget,
    callbacks: &mut usize,
) -> std::result::Result<(), Failure> {
    let focusable = render_doc.borrow().as_ref().is_some_and(|doc| {
        matches!(
            doc.tag_name(target.node_id as usize).as_deref(),
            Some("input" | "textarea" | "button" | "select")
        ) || doc.node_attr(target.node_id as usize, "tabindex").is_some()
            || (doc.tag_name(target.node_id as usize).as_deref() == Some("a")
                && doc.node_attr(target.node_id as usize, "href").is_some())
    });
    if !focusable {
        return Ok(());
    }
    let previous = dom_realm::focused_node(render_doc);
    if previous == Some(target.node_id as usize) {
        return Ok(());
    }
    dom_realm::set_focused(ctx, render_doc, Some(target.node_id as usize));
    let prevented = Rc::new(Cell::new(false));
    if let Some(node) = previous {
        let identity = render_doc.borrow_mut().as_mut().and_then(|doc| {
            if !connected(doc, node) {
                return None;
            }
            Some((
                doc.node_incarnation(node)?,
                doc.node_attachment_revision(node),
            ))
        });
        if let Some((token, attachment)) = identity {
            let mut previous = target.clone();
            previous.node_id = node as u32;
            previous.incarnation = token;
            previous.attachment_revision = attachment;
            deliver(
                ctx, render_doc, dirty, mutations, &previous, "blur", callbacks, &prevented,
            )?;
            deliver(
                ctx, render_doc, dirty, mutations, &previous, "focusout", callbacks, &prevented,
            )?;
        }
    }
    deliver(
        ctx, render_doc, dirty, mutations, target, "focus", callbacks, &prevented,
    )?;
    deliver(
        ctx, render_doc, dirty, mutations, target, "focusin", callbacks, &prevented,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn act(
    ctx: &mut Context,
    render_doc: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    mutations: &Arc<RwLock<Vec<DomMutation>>>,
    document_id: u64,
    target: &NativeTarget,
    input: NativeInput,
    page_url: &str,
) -> std::result::Result<NativeReceipt, Failure> {
    if pending_navigation(mutations) {
        return Err(stale());
    }
    {
        let mut cell = render_doc.borrow_mut();
        let doc = cell.as_mut().ok_or_else(stale)?;
        if document_id != target.document_id
            || doc.tree_revision() != target.tree_revision
            || !still_same(
                doc,
                target.node_id as usize,
                target.incarnation,
                target.attachment_revision,
            )
        {
            return Err(stale());
        }
        let snapshot = DomSnapshot::from_render_document(doc, "", "");
        if snapshot.fingerprint(target.node_id) != target.fingerprint
            || !snapshot
                .nodes
                .get(&target.node_id)
                .is_some_and(|n| n.attributes == target.attributes)
        {
            return Err(stale());
        }
        let id = target.node_id as usize;
        let tag = doc.tag_name(id).unwrap_or_default();
        let ty = doc
            .node_attr(id, "type")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if doc.node_attr(id, "disabled").is_some()
            || matches!(ty.as_str(), "file" | "hidden")
            || (ty == "password" && !matches!(input, NativeInput::FillCredential(_)))
            || (ty != "password" && matches!(input, NativeInput::FillCredential(_)))
        {
            return Err(failed("NATIVE_CONTROL_DENIED", false));
        }
        if let NativeInput::Fill(value) = &input {
            if doc.node_attr(id, "readonly").is_some() {
                return Err(failed("TARGET_NOT_FILLABLE", false));
            }
            if tag == "textarea"
                || (tag == "input"
                    && !matches!(
                        ty.as_str(),
                        "checkbox" | "radio" | "button" | "submit" | "reset" | "image"
                    ))
            {
                doc.set_attribute(id, "value", value);
            } else if doc
                .node_attr(id, "contenteditable")
                .is_some_and(|v| v.is_empty() || v.eq_ignore_ascii_case("true"))
            {
                doc.set_text(id, value);
            } else {
                return Err(failed("TARGET_NOT_FILLABLE", false));
            }
        }
        if let NativeInput::FillCredential(value) = &input {
            // The target check above already pinned this to input[type=password].
            if doc.node_attr(id, "readonly").is_some() {
                return Err(failed("TARGET_NOT_FILLABLE", false));
            }
            doc.set_attribute(id, "value", value);
        }
    }
    dirty.store(true, Ordering::Relaxed);
    let mut callbacks = 0;
    focus_target(ctx, render_doc, dirty, mutations, target, &mut callbacks)?;
    let prevented = Rc::new(Cell::new(false));
    match input {
        NativeInput::Click => {
            let check_activation = checkable::activate(render_doc, target.node_id as usize);

            deliver(
                ctx,
                render_doc,
                dirty,
                mutations,
                target,
                "click",
                &mut callbacks,
                &prevented,
            )?;
            if let Some(activation) = check_activation {
                if prevented.get() {
                    checkable::rollback(render_doc, activation);
                } else {
                    deliver(
                        ctx,
                        render_doc,
                        dirty,
                        mutations,
                        target,
                        "input",
                        &mut callbacks,
                        &prevented,
                    )?;
                    deliver(
                        ctx,
                        render_doc,
                        dirty,
                        mutations,
                        target,
                        "change",
                        &mut callbacks,
                        &prevented,
                    )?;
                }
            }
            // Click default action: a submit control activates its owner form
            // unless a listener called preventDefault (on click or on the
            // synthetic submit event).
            if !prevented.get()
                && let Ok(page) = url::Url::parse(page_url)
            {
                let form_info = {
                    let mut cell = render_doc.borrow_mut();
                    match cell.as_mut() {
                        Some(doc) => {
                            let snapshot = DomSnapshot::from_render_document(doc, "", "");
                            snapshot.nodes.get(&target.node_id).and_then(|n| {
                                let ty = n.attributes.get("type").map(|x| x.as_str()).unwrap_or("");
                                if crate::js::form_activation::is_submit_control(&n.tag, ty) {
                                    crate::js::form_activation::nearest_form(
                                        &snapshot,
                                        target.node_id,
                                    )
                                    .map(|form_id| {
                                        (
                                            form_id,
                                            doc.node_incarnation(form_id as usize)
                                                .unwrap_or_default(),
                                            doc.node_attachment_revision(form_id as usize),
                                        )
                                    })
                                } else {
                                    None
                                }
                            })
                        }
                        None => None,
                    }
                };
                if let Some((form_id, form_token, _form_attachment)) = form_info {
                    let invalid = if validation::should_validate(
                        render_doc,
                        form_id as usize,
                        target.node_id as usize,
                    ) {
                        validation::invalid_controls(ctx, render_doc, form_id as usize)
                    } else {
                        vec![]
                    };
                    if !invalid.is_empty() {
                        for node in invalid {
                            let identity = render_doc
                                .borrow_mut()
                                .as_mut()
                                .and_then(|doc| {
                                    Some((
                                        doc.node_incarnation(node)?,
                                        doc.node_attachment_revision(node),
                                    ))
                                })
                                .ok_or_else(stale)?;
                            let mut control = target.clone();
                            control.node_id = node as u32;
                            control.incarnation = identity.0;
                            control.attachment_revision = identity.1;
                            deliver(
                                ctx,
                                render_doc,
                                dirty,
                                mutations,
                                &control,
                                "invalid",
                                &mut callbacks,
                                &Rc::new(Cell::new(false)),
                            )?;
                        }
                        return Ok(NativeReceipt {
                            callbacks,
                            navigation_requested: pending_navigation(mutations),
                        });
                    }

                    let submit_prevented = Rc::new(Cell::new(false));
                    let mut form_target = target.clone();
                    form_target.node_id = form_id;
                    form_target.incarnation = form_token;
                    form_target.attachment_revision = _form_attachment;
                    deliver(
                        ctx,
                        render_doc,
                        dirty,
                        mutations,
                        &form_target,
                        "submit",
                        &mut callbacks,
                        &submit_prevented,
                    )?;
                    if !submit_prevented.get() {
                        let mutation = {
                            let mut cell = render_doc.borrow_mut();
                            match cell.as_mut() {
                                Some(doc) => {
                                    let snapshot =
                                        DomSnapshot::from_render_document_unmasked(doc, "", "");
                                    crate::js::form_activation::submit_mutation(
                                        &snapshot, form_id, &page,
                                    )
                                }
                                None => None,
                            }
                        };
                        if let Some(m) = mutation {
                            mutations.write().push(m);
                        }
                    }
                }
                navigation::anchor(ctx, render_doc, mutations, target.node_id as usize)
                    .map_err(|_| failed("NATIVE_NAVIGATION_FAILED", true))?;
            }
        }
        NativeInput::Fill(_) | NativeInput::FillCredential(_) => {
            deliver(
                ctx,
                render_doc,
                dirty,
                mutations,
                target,
                "input",
                &mut callbacks,
                &prevented,
            )?;
            if pending_navigation(mutations) {
                return Err(failed("NATIVE_NAVIGATION_DURING_FILL", true));
            }
            {
                let mut cell = render_doc.borrow_mut();
                if !cell.as_mut().is_some_and(|doc| {
                    still_same(
                        doc,
                        target.node_id as usize,
                        target.incarnation,
                        target.attachment_revision,
                    )
                }) {
                    return Err(failed("NATIVE_TARGET_DETACHED_DURING_ACTION", true));
                }
            }
            deliver(
                ctx,
                render_doc,
                dirty,
                mutations,
                target,
                "change",
                &mut callbacks,
                &prevented,
            )?;
        }
    }
    Ok(NativeReceipt {
        callbacks,
        navigation_requested: pending_navigation(mutations),
    })
}
