//! Element subtree operations on the live render DOM, including detached clones.

use super::*;

pub(super) fn attach(
    ctx: &mut Context,
    object: &JsObject,
    render_doc: Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
    dirty: Arc<AtomicBool>,
) -> JsResult<()> {
    for (name, all) in [("querySelector", false), ("querySelectorAll", true)] {
        let rd = render_doc.clone();
        let dirty = dirty.clone();
        let function = unsafe {
            NativeFunction::from_closure(move |_, args, ctx| {
                let selector = argument(args, ctx)?;
                let ids = rd
                    .borrow()
                    .as_ref()
                    .map(|doc| doc.query_selector_all_from(node, &selector))
                    .transpose()
                    .map_err(|message| JsNativeError::syntax().with_message(message))?
                    .unwrap_or_default();
                if all {
                    let values = ids
                        .into_iter()
                        .map(|id| create_render_element_object(ctx, rd.clone(), id, dirty.clone()))
                        .collect::<Vec<_>>();
                    Ok(JsArray::from_iter(values, ctx).into())
                } else {
                    Ok(ids
                        .first()
                        .map(|id| create_render_element_object(ctx, rd.clone(), *id, dirty.clone()))
                        .unwrap_or(JsValue::null()))
                }
            })
        }
        .to_js_function(ctx.realm());
        object.create_data_property_or_throw(JsString::from(name), function, ctx)?;
    }
    let rd = render_doc.clone();
    let clone_dirty = dirty.clone();
    let clone = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let deep = args.first().is_some_and(JsValue::to_boolean);
            let id = rd
                .borrow_mut()
                .as_mut()
                .and_then(|doc| doc.clone_node(node, deep))
                .ok_or_else(|| JsNativeError::typ().with_message("cannot clone this node"))?;
            clone_dirty.store(true, Ordering::Relaxed);
            Ok(create_render_element_object(
                ctx,
                rd.clone(),
                id,
                clone_dirty.clone(),
            ))
        })
    }
    .to_js_function(ctx.realm());
    object.create_data_property_or_throw(js_string!("cloneNode"), clone, ctx)?;
    let rd = render_doc.clone();
    let has = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let name = argument(args, ctx)?;
            Ok(rd
                .borrow()
                .as_ref()
                .is_some_and(|doc| doc.node_attr(node, &name).is_some())
                .into())
        })
    }
    .to_js_function(ctx.realm());
    object.create_data_property_or_throw(js_string!("hasAttribute"), has, ctx)?;
    let rd = render_doc.clone();
    let matches = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let selector = argument(args, ctx)?;
            let matched = rd
                .borrow()
                .as_ref()
                .map(|doc| doc.element_matches(node, &selector))
                .transpose()
                .map_err(|message| JsNativeError::syntax().with_message(message))?
                .unwrap_or(false);
            Ok(matched.into())
        })
    }
    .to_js_function(ctx.realm());
    object.create_data_property_or_throw(js_string!("matches"), matches, ctx)?;
    let closest = unsafe {
        NativeFunction::from_closure(move |_, args, ctx| {
            let selector = argument(args, ctx)?;
            let id = render_doc
                .borrow()
                .as_ref()
                .map(|doc| doc.element_closest(node, &selector))
                .transpose()
                .map_err(|message| JsNativeError::syntax().with_message(message))?
                .flatten();
            Ok(id
                .map(|id| create_render_element_object(ctx, render_doc.clone(), id, dirty.clone()))
                .unwrap_or(JsValue::null()))
        })
    }
    .to_js_function(ctx.realm());
    object.create_data_property_or_throw(js_string!("closest"), closest, ctx)?;
    Ok(())
}

fn argument(args: &[JsValue], ctx: &mut Context) -> JsResult<String> {
    args.first()
        .ok_or_else(|| JsNativeError::typ().with_message("argument required"))?
        .to_string(ctx)
        .map(|value| value.to_std_string_escaped())
}
