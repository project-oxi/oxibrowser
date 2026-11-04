//! Point queries use the renderer's resolved paint order and live DOM wrappers.
use super::*;
use boa_engine::property::PropertyDescriptor;
use std::collections::HashSet;

fn local_point(quad: [f64; 8], x: f64, y: f64) -> Option<(f64, f64)> {
    let (ax, ay) = (quad[2] - quad[0], quad[3] - quad[1]);
    let (bx, by) = (quad[6] - quad[0], quad[7] - quad[1]);
    let det = ax * by - ay * bx;
    if !det.is_finite() || det.abs() < 1e-9 {
        return None;
    }
    let (px, py) = (x - quad[0], y - quad[1]);
    Some(((px * by - py * bx) / det, (ax * py - ay * px) / det))
}

fn paint_order(doc: &RenderDocument, id: usize, seen: &mut HashSet<usize>, out: &mut Vec<usize>) {
    if !seen.insert(id) {
        return;
    }
    let Some(node) = doc.document().get_node(id) else {
        return;
    };
    if let Some(hoisted) = &node.stacking_context {
        for child in hoisted.pos_z_hoisted_children().rev() {
            paint_order(doc, child.node_id, seen, out);
        }
    }
    for child in node.paint_children.borrow().iter().flatten().rev() {
        paint_order(doc, *child, seen, out);
    }
    if let Some(hoisted) = &node.stacking_context {
        for child in hoisted.neg_z_hoisted_children().rev() {
            paint_order(doc, child.node_id, seen, out);
        }
    }
    // Inline fragments are painted inside their formatting root rather than
    // listed in paint_children. Their DOM order supplies the fragment order.
    for child in node.children.iter().rev() {
        paint_order(doc, *child, seen, out);
    }
    if doc.node_kind(id) == Some("element") {
        out.push(id);
    }
}

fn eligible(doc: &RenderDocument, id: usize, x: f64, y: f64, hit_path: &HashSet<usize>) -> bool {
    if doc.resolved_style(id, "pointer-events") == "none"
        || doc.resolved_style(id, "visibility") != "visible"
        || matches!(
            doc.resolved_style(id, "display").as_str(),
            "none" | "contents"
        )
        || (doc.tag_name(id).as_deref() == Some("input")
            && doc
                .node_attr(id, "type")
                .is_some_and(|v| v.eq_ignore_ascii_case("hidden")))
    {
        return false;
    }
    let root = doc.root_element_id();
    let own_box = doc
        .node_box_quads(id)
        .and_then(|model| local_point(model.2, x, y));
    let inside = own_box.is_some_and(|(u, v)| (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v));
    let native_inline_hit = own_box.is_none() && hit_path.contains(&id);
    if !inside && id != root && !native_inline_hit {
        return false;
    }
    let mut parent = doc.parent_id(id);
    while let Some(ancestor) = parent {
        if ancestor == doc.document().root_node().id {
            break;
        }
        if doc.resolved_style(ancestor, "display") == "none"
            || doc.resolved_style(ancestor, "visibility") != "visible"
        {
            return false;
        }
        if ancestor != root {
            let clip_x = !matches!(
                doc.resolved_style(ancestor, "overflow-x").as_str(),
                "visible" | ""
            );
            let clip_y = !matches!(
                doc.resolved_style(ancestor, "overflow-y").as_str(),
                "visible" | ""
            );
            if clip_x || clip_y {
                let Some((u, v)) = doc
                    .node_box_quads(ancestor)
                    .and_then(|model| local_point(model.1, x, y))
                else {
                    return false;
                };
                if (clip_x && !(0.0..=1.0).contains(&u)) || (clip_y && !(0.0..=1.0).contains(&v)) {
                    return false;
                }
            }
        }
        parent = doc.parent_id(ancestor);
    }
    true
}

fn stack(doc: &mut RenderDocument, x: f64, y: f64) -> Vec<usize> {
    let viewport = doc.viewport();
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= viewport.width as f64
        || y >= viewport.height as f64
    {
        return Vec::new();
    }
    doc.flush_layout();
    let scroll = doc.document().viewport_scroll();
    let mut hit = doc
        .document()
        .hit((x + scroll.x) as f32, (y + scroll.y) as f32)
        .map(|hit| hit.node_id);
    let mut hit_path = HashSet::new();
    let mut top = None;
    while let Some(id) = hit {
        if !hit_path.insert(id) {
            break;
        }
        if top.is_none() && doc.node_kind(id) == Some("element") {
            top = Some(id);
        }
        hit = doc.parent_id(id);
    }
    let mut order = Vec::new();
    paint_order(doc, doc.root_element_id(), &mut HashSet::new(), &mut order);
    order.retain(|id| eligible(doc, *id, x, y, &hit_path));
    if let Some(top) = top.and_then(|top| order.iter().position(|id| *id == top)) {
        let id = order.remove(top);
        order.insert(0, id);
    }
    order
}

pub(super) fn attach(
    ctx: &mut Context,
    document_obj: &JsObject,
    rd: Rc<RefCell<Option<RenderDocument>>>,
    dirty: Arc<AtomicBool>,
) {
    for (name, multiple) in [("elementsFromPoint", true), ("elementFromPoint", false)] {
        let rd = rd.clone();
        let dirty = dirty.clone();
        // Captures hold only native document state; no untraced JS values.
        let function = unsafe {
            NativeFunction::from_closure(move |_this, args, ctx| {
                let x = args
                    .first()
                    .unwrap_or(&JsValue::undefined())
                    .to_number(ctx)?;
                let y = args
                    .get(1)
                    .unwrap_or(&JsValue::undefined())
                    .to_number(ctx)?;
                let ids = rd
                    .borrow_mut()
                    .as_mut()
                    .map(|doc| stack(doc, x, y))
                    .unwrap_or_default();
                // Release the document borrow before stable-wrapper lookup borrows it.
                let values: Vec<_> = ids
                    .into_iter()
                    .map(|id| create_render_element_object(ctx, rd.clone(), id, dirty.clone()))
                    .collect();
                if multiple {
                    Ok(JsArray::from_iter(values, ctx).into())
                } else {
                    Ok(values.into_iter().next().unwrap_or(JsValue::null()))
                }
            })
        }
        .to_js_function(ctx.realm());
        let _ = document_obj.define_property_or_throw(
            JsString::from(name),
            PropertyDescriptor::builder()
                .value(function)
                .writable(true)
                .configurable(true),
            ctx,
        );
    }
}
