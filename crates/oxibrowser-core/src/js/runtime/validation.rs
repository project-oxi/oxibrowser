//! Constraint validation for required controls and custom validity messages.
//! Other HTML constraints (pattern, ranges, type syntax) remain outside this
//! initial contract; no flags for those constraints are fabricated here.
use super::*;
use boa_engine::JsData;
use boa_gc::{Finalize, Trace};

#[derive(Default, Trace, Finalize, JsData)]
struct Messages {
    values: HashMap<(usize, usize), String>,
}
pub(super) fn reset(ctx: &mut Context) {
    ctx.realm().host_defined_mut().remove::<Messages>();
}
fn custom(ctx: &Context, rd: &Rc<RefCell<Option<RenderDocument>>>, node: usize) -> String {
    ctx.realm()
        .host_defined()
        .get::<Messages>()
        .and_then(|m| m.values.get(&(Rc::as_ptr(rd) as usize, node)))
        .cloned()
        .unwrap_or_default()
}
fn candidate(doc: &RenderDocument, node: usize) -> bool {
    let tag = doc.tag_name(node).unwrap_or_default();
    if !matches!(tag.as_str(), "input" | "textarea" | "select" | "button")
        || doc.node_attr(node, "disabled").is_some()
    {
        return false;
    }
    let ty = doc
        .node_attr(node, "type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if tag == "button"
        || (tag == "input"
            && matches!(
                ty.as_str(),
                "hidden" | "button" | "reset" | "submit" | "image"
            ))
    {
        return false;
    }
    if matches!(tag.as_str(), "input" | "textarea") && doc.node_attr(node, "readonly").is_some() {
        return false;
    }
    let mut parent = doc.parent_id(node);
    while let Some(id) = parent {
        if doc.tag_name(id).as_deref() == Some("fieldset")
            && doc.node_attr(id, "disabled").is_some()
        {
            return false;
        }
        parent = doc.parent_id(id);
    }
    true
}
fn owner(snapshot: &DomSnapshot, node: u32) -> Option<u32> {
    let control = snapshot.nodes.get(&node)?;
    if let Some(id) = control.attributes.get("form") {
        return snapshot
            .get_element_by_id(id)
            .filter(|id| snapshot.nodes.get(id).is_some_and(|n| n.tag == "form"));
    }
    form_activation::nearest_form(snapshot, node)
}
fn missing(doc: &RenderDocument, node: usize) -> bool {
    if !candidate(doc, node) || doc.node_attr(node, "required").is_none() {
        return false;
    }
    let tag = doc.tag_name(node).unwrap_or_default();
    let ty = doc
        .node_attr(node, "type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if tag == "input" && ty == "checkbox" {
        return doc.node_attr(node, "checked").is_none();
    }
    if tag == "input" && ty == "radio" {
        let name = doc.node_attr(node, "name").unwrap_or_default();
        if name.is_empty() {
            return doc.node_attr(node, "checked").is_none();
        }
        let snapshot = DomSnapshot::from_render_document(doc, "", "");
        let form = owner(&snapshot, node as u32);
        return !snapshot.nodes.values().any(|n| {
            n.tag == "input"
                && n.attributes
                    .get("type")
                    .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
                && n.attributes.get("name") == Some(&name)
                && owner(&snapshot, n.id) == form
                && n.attributes.contains_key("checked")
        });
    }
    if tag == "select" {
        let mut stack = doc.children_ids(node);
        let mut options = vec![];
        while let Some(id) = stack.pop() {
            if doc.tag_name(id).as_deref() == Some("option") {
                options.push(id);
            } else {
                stack.extend(doc.children_ids(id));
            }
        }
        options.reverse();
        let selected = options
            .iter()
            .copied()
            .filter(|id| doc.node_attr(*id, "selected").is_some())
            .collect::<Vec<_>>();
        if doc.node_attr(node, "multiple").is_some() {
            return selected.is_empty();
        }
        let chosen = selected
            .first()
            .copied()
            .or_else(|| options.first().copied());
        return chosen
            .map(|id| {
                doc.node_attr(id, "value")
                    .unwrap_or_else(|| doc.node_text(id))
                    .is_empty()
            })
            .unwrap_or(true);
    }
    let value = doc.node_attr(node, "value").unwrap_or_else(|| {
        if tag == "textarea" {
            doc.node_text(node)
        } else {
            String::new()
        }
    });
    value.is_empty()
}
fn status(
    ctx: &Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
) -> (bool, bool, String) {
    let custom = custom(ctx, rd, node);
    let cell = rd.borrow();
    let Some(doc) = cell.as_ref() else {
        return (false, false, String::new());
    };
    let missing = missing(doc, node);
    let candidate = candidate(doc, node);
    let message = if !custom.is_empty() {
        custom
    } else if missing {
        "Please fill out this field.".into()
    } else {
        String::new()
    };
    (candidate, missing, message)
}
pub(super) fn invalid_controls(
    ctx: &Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    form: usize,
) -> Vec<usize> {
    let ids = {
        let cell = rd.borrow();
        let Some(doc) = cell.as_ref() else {
            return vec![];
        };
        let snapshot = DomSnapshot::from_render_document(doc, "", "");
        let mut ids = snapshot
            .nodes
            .values()
            .filter(|node| owner(&snapshot, node.id) == Some(form as u32))
            .map(|n| n.id as usize)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    };
    ids.into_iter()
        .filter(|node| {
            let (candidate, _, message) = status(ctx, rd, *node);
            candidate && !message.is_empty()
        })
        .collect()
}
pub(super) fn should_validate(
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    form: usize,
    submitter: usize,
) -> bool {
    rd.borrow().as_ref().is_some_and(|doc| {
        doc.node_attr(form, "novalidate").is_none()
            && doc.node_attr(submitter, "formnovalidate").is_none()
    })
}
fn invalid_event(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    node: usize,
) -> JsResult<()> {
    let mut ids = vec![node];
    {
        let cell = rd.borrow();
        if let Some(doc) = cell.as_ref() {
            let mut parent = doc.parent_id(node);
            while let Some(id) = parent {
                if doc.tag_name(id).is_none() {
                    break;
                }
                ids.push(id);
                parent = doc.parent_id(id);
            }
        }
    }
    let mut path = ids
        .into_iter()
        .map(|id| {
            (
                create_render_element_object(ctx, rd.clone(), id, dirty.clone()),
                EventListenerStore::Element(id as u32),
            )
        })
        .collect::<Vec<_>>();
    append_document_event_targets(ctx, &mut path)?;
    let event = dom_realm::synthetic_event(ctx, "invalid", false, true);
    dispatch_bubbling_event(ctx, &event, &path)?;
    Ok(())
}
pub(super) fn check_form(
    ctx: &mut Context,
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    dirty: &Arc<AtomicBool>,
    form: usize,
    report: bool,
) -> JsResult<bool> {
    let invalid = invalid_controls(ctx, rd, form);
    for node in &invalid {
        invalid_event(ctx, rd, dirty, *node)?;
    }
    if report && let Some(node) = invalid.first() {
        let object = create_render_element_object(ctx, rd.clone(), *node, dirty.clone());
        if let Some(f) = object
            .as_object()
            .and_then(|o| o.get(js_string!("focus"), ctx).ok())
            .and_then(|v| v.as_callable())
        {
            f.call(&object, &[], ctx)?;
        }
    }
    Ok(invalid.is_empty())
}
pub(super) fn attach(
    ctx: &mut Context,
    object: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    dirty: Arc<AtomicBool>,
) {
    let tag = rd
        .borrow()
        .as_ref()
        .and_then(|d| d.tag_name(node))
        .unwrap_or_default();
    if !matches!(
        tag.as_str(),
        "form" | "input" | "textarea" | "select" | "button"
    ) {
        return;
    }
    let form = tag == "form";
    for (name, report) in [("checkValidity", false), ("reportValidity", true)] {
        let rd = rd.clone();
        let dirty = dirty.clone();
        let f = unsafe {
            NativeFunction::from_closure(move |_, _, ctx| {
                if form {
                    return Ok(check_form(ctx, &rd, &dirty, node, report)?.into());
                }
                let (candidate, _, message) = status(ctx, &rd, node);
                if !candidate || message.is_empty() {
                    return Ok(true.into());
                }
                invalid_event(ctx, &rd, &dirty, node)?;
                Ok(false.into())
            })
        }
        .to_js_function(ctx.realm());
        let _ = object.create_data_property_or_throw(JsString::from(name), f, ctx);
    }
    if form {
        return;
    }
    let document = rd.clone();
    let set = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let value = args
                .first()
                .unwrap_or(&JsValue::undefined())
                .to_string(ctx)?
                .to_std_string_escaped();
            let realm = ctx.realm().clone();
            let mut host = realm.host_defined_mut();
            if host.get::<Messages>().is_none() {
                host.insert(Messages::default());
            }
            let key = (Rc::as_ptr(&document) as usize, node);
            if value.is_empty() {
                host.get_mut::<Messages>().unwrap().values.remove(&key);
            } else {
                host.get_mut::<Messages>()
                    .unwrap()
                    .values
                    .insert(key, value);
            }
            Ok(JsValue::undefined())
        })
    }
    .to_js_function(ctx.realm());
    let _ = object.create_data_property_or_throw(js_string!("setCustomValidity"), set, ctx);
    for name in ["willValidate", "validationMessage", "validity"] {
        let rd = rd.clone();
        let getter = unsafe {
            NativeFunction::from_closure(move |_, _, ctx| {
                let (candidate, missing, message) = status(ctx, &rd, node);
                if name == "willValidate" {
                    return Ok(candidate.into());
                }
                if name == "validationMessage" {
                    return Ok(
                        JsString::from(if candidate { message } else { String::new() }).into(),
                    );
                }
                let custom = !custom(ctx, &rd, node).is_empty();
                Ok(boa_engine::object::ObjectInitializer::new(ctx)
                    .property(js_string!("valueMissing"), missing, Attribute::CONFIGURABLE)
                    .property(js_string!("customError"), custom, Attribute::CONFIGURABLE)
                    .property(
                        js_string!("valid"),
                        !missing && !custom,
                        Attribute::CONFIGURABLE,
                    )
                    .build()
                    .into())
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
}
