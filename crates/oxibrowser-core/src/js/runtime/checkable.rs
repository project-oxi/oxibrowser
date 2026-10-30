//! Native checkbox/radio click activation shared by script and host input.
use super::*;
pub(super) struct Activation {
    before: Vec<(usize, bool)>,
}
fn form(snapshot: &DomSnapshot, node: u32) -> Option<u32> {
    if let Some(id) = snapshot.nodes.get(&node)?.attributes.get("form") {
        return snapshot.get_element_by_id(id);
    }
    form_activation::nearest_form(snapshot, node)
}
pub(super) fn activate(
    rd: &Rc<RefCell<Option<RenderDocument>>>,
    node: usize,
) -> Option<Activation> {
    let mut cell = rd.borrow_mut();
    let doc = cell.as_mut()?;
    if doc.tag_name(node).as_deref() != Some("input") || doc.node_attr(node, "disabled").is_some() {
        return None;
    }
    let ty = doc
        .node_attr(node, "type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(ty.as_str(), "checkbox" | "radio") {
        return None;
    }
    let checked = doc.node_attr(node, "checked").is_some();
    if ty == "radio" && checked {
        return None;
    }
    let mut before = vec![(node, checked)];
    if ty == "radio" {
        let name = doc.node_attr(node, "name").unwrap_or_default();
        if !name.is_empty() {
            let snapshot = DomSnapshot::from_render_document(doc, "", "");
            let owner = form(&snapshot, node as u32);
            for other in snapshot.nodes.values() {
                if other.id as usize != node
                    && other.tag == "input"
                    && other
                        .attributes
                        .get("type")
                        .is_some_and(|v| v.eq_ignore_ascii_case("radio"))
                    && other.attributes.get("name") == Some(&name)
                    && form(&snapshot, other.id) == owner
                {
                    before.push((other.id as usize, other.attributes.contains_key("checked")));
                }
            }
        }
        for (other, _) in &before {
            if *other != node {
                doc.remove_attribute(*other, "checked");
            }
        }
        doc.set_attribute(node, "checked", "");
    } else if checked {
        doc.remove_attribute(node, "checked");
    } else {
        doc.set_attribute(node, "checked", "");
    }
    Some(Activation { before })
}
pub(super) fn rollback(rd: &Rc<RefCell<Option<RenderDocument>>>, activation: Activation) {
    if let Some(doc) = rd.borrow_mut().as_mut() {
        for (node, checked) in activation.before {
            if checked {
                doc.set_attribute(node, "checked", "");
            } else {
                doc.remove_attribute(node, "checked");
            }
        }
    }
}
