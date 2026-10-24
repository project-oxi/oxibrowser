//! Simple text-based DOM renderer.
//!
//! Renders the DOM tree as ASCII/Unicode text for terminal output.
//! Uses a simplified block layout model.

use crate::js::dom_snapshot::DomNode;
use crate::js::dom_snapshot::DomSnapshot;

/// One pending unit of rendering work.
///
/// Both walkers use an explicit stack instead of recursion: framework
/// markup routinely nests thousands deep, and a recursive DOM walk
/// overflows the native stack and aborts the whole browser (measured:
/// figma.com, 2026-10-24). With the heap-backed stack, depth is bounded
/// only by memory.
enum Task {
    /// Render the node. The `usize` is the text-indent depth (text renderer
    /// only; the markdown walker ignores it).
    Node(u32, usize),
    /// Emit a pre-computed literal.
    Emit(String),
}

/// Render a DomSnapshot to a text string.
pub fn render_to_text(snapshot: &DomSnapshot) -> String {
    let mut output = String::new();
    let root_id = snapshot.body_id.or(Some(snapshot.root_id));

    if let Some(root) = root_id.and_then(|id| snapshot.nodes.get(&id)) {
        render_node(snapshot, root, &mut output);
    } else if let Some(root) = snapshot.nodes.get(&snapshot.root_id) {
        render_node(snapshot, root, &mut output);
    }

    output
}

fn render_node(snapshot: &DomSnapshot, root: &DomNode, output: &mut String) {
    let mut stack = vec![Task::Node(root.id, 0)];
    while let Some(task) = stack.pop() {
        match task {
            Task::Emit(text) => output.push_str(&text),
            Task::Node(id, depth) => render_node_step(snapshot, id, depth, &mut stack, output),
        }
    }
}

/// Render one node, pushing follow-up work onto `stack`.
fn render_node_step(
    snapshot: &DomSnapshot,
    id: u32,
    depth: usize,
    stack: &mut Vec<Task>,
    output: &mut String,
) {
    let Some(node) = snapshot.nodes.get(&id) else {
        return;
    };
    match node.node_type {
        3 => {
            // TEXT_NODE
            let text = node.text_content.trim();
            if !text.is_empty() {
                let indent = "  ".repeat(depth);
                for line in text.lines() {
                    output.push_str(&indent);
                    output.push_str(line.trim());
                    output.push('\n');
                }
            }
        }
        1 => {
            // ELEMENT_NODE
            let tag = node.tag.to_uppercase();
            let invisible = matches!(
                tag.as_str(),
                "SCRIPT"
                    | "STYLE"
                    | "LINK"
                    | "META"
                    | "HEAD"
                    | "HTML"
                    | "TITLE"
                    | "NOSCRIPT"
                    | "BASE"
                    | "SVG"
                    | "DEFS"
                    | "PATH"
            );
            if invisible {
                return;
            }

            let is_self_closing = matches!(
                tag.as_str(),
                "BR" | "HR"
                    | "IMG"
                    | "INPUT"
                    | "AREA"
                    | "COL"
                    | "EMBED"
                    | "PARAM"
                    | "SOURCE"
                    | "TRACK"
                    | "WBR"
            );

            if is_self_closing {
                let indent = "  ".repeat(depth);
                output.push_str(&indent);
                if tag == "BR" {
                    output.push('\n');
                } else if tag == "HR" {
                    output.push_str("─────────────────────────────────\n");
                } else if tag == "IMG" {
                    let alt = node
                        .attributes
                        .get("alt")
                        .map(|s| s.as_str())
                        .unwrap_or("img");
                    output.push_str("[IMAGE: ");
                    output.push_str(alt);
                    output.push_str("]\n");
                }
                return;
            }

            let indent = "  ".repeat(depth);
            let is_block = matches!(
                tag.as_str(),
                "DIV"
                    | "P"
                    | "H1"
                    | "H2"
                    | "H3"
                    | "H4"
                    | "H5"
                    | "H6"
                    | "UL"
                    | "OL"
                    | "LI"
                    | "TABLE"
                    | "TR"
                    | "TD"
                    | "TH"
                    | "FORM"
                    | "SECTION"
                    | "ARTICLE"
                    | "HEADER"
                    | "FOOTER"
                    | "NAV"
                    | "MAIN"
                    | "ADDRESS"
                    | "BLOCKQUOTE"
                    | "PRE"
                    | "FIGURE"
                    | "FIGCAPTION"
                    | "ASIDE"
            );

            if is_block && depth > 0 {
                output.push('\n');
            }

            // Open tag
            output.push_str(&indent);
            output.push('<');
            output.push_str(&tag.to_lowercase());
            if let Some(id) = node.attributes.get("id") {
                output.push_str(" id=\"");
                output.push_str(id);
                output.push('"');
            }
            output.push_str(">\n");

            // Children first, then the closing tag — pushed in reverse so the
            // stack pops them in document order.
            let closing = format!("{}</{}>\n", indent, node.tag.to_lowercase());
            stack.push(Task::Emit(closing));
            for &child_id in node.children.iter().rev() {
                stack.push(Task::Node(child_id, depth + 1));
            }
        }
        _ => {
            // Document, Comment, etc.
            for &child_id in node.children.iter().rev() {
                stack.push(Task::Node(child_id, depth));
            }
        }
    }
}

/// Render a DomSnapshot to actual Markdown.
///
/// Produces proper Markdown formatting: headings with `#`, bold with `**`,
/// italic with `*`, links with `[text](url)`, code with backticks, etc.
pub fn render_to_markdown(snapshot: &DomSnapshot) -> String {
    let mut output = String::new();

    if let Some(body_id) = snapshot.body_id {
        render_markdown_node(snapshot, body_id, &mut output);
    }

    output.trim().to_string()
}

fn render_markdown_node(snapshot: &DomSnapshot, root_id: u32, output: &mut String) {
    let mut stack = vec![Task::Node(root_id, 0)];
    while let Some(task) = stack.pop() {
        match task {
            Task::Emit(text) => output.push_str(&text),
            Task::Node(id, _) => render_markdown_step(snapshot, id, &mut stack, output),
        }
    }
}

/// Render one markdown node, pushing follow-up work onto `stack`.
fn render_markdown_step(
    snapshot: &DomSnapshot,
    id: u32,
    stack: &mut Vec<Task>,
    output: &mut String,
) {
    let Some(node) = snapshot.nodes.get(&id) else {
        return;
    };
    match node.node_type {
        3 => {
            // TEXT_NODE
            let text = node.text_content.trim();
            if !text.is_empty() {
                output.push_str(text);
                output.push(' ');
            }
        }
        1 => {
            // ELEMENT_NODE
            let tag = node.tag.to_uppercase();
            let skip = matches!(
                tag.as_str(),
                "SCRIPT"
                    | "STYLE"
                    | "LINK"
                    | "META"
                    | "HEAD"
                    | "HTML"
                    | "NOSCRIPT"
                    | "SVG"
                    | "DEFS"
                    | "PATH"
                    | "BASE"
                    | "TITLE"
            );
            if skip {
                return;
            }

            // Self-closing elements
            match tag.as_str() {
                "BR" => {
                    output.push('\n');
                    return;
                }
                "HR" => {
                    output.push_str("\n---\n");
                    return;
                }
                "IMG" => {
                    let alt = node.attributes.get("alt").map(|s| s.as_str()).unwrap_or("");
                    let src = node.attributes.get("src").map(|s| s.as_str()).unwrap_or("");
                    output.push_str(&format!("![{alt}]({src})"));
                    return;
                }
                _ => {}
            }

            // Tags rendered by collecting their descendant text inline.
            let text_wrapper: Option<(String, String)> = match tag.as_str() {
                "H1" => Some(("\n# ".into(), "\n\n".into())),
                "H2" => Some(("\n## ".into(), "\n\n".into())),
                "H3" => Some(("\n### ".into(), "\n\n".into())),
                "H4" => Some(("\n#### ".into(), "\n\n".into())),
                "H5" => Some(("\n##### ".into(), "\n\n".into())),
                "H6" => Some(("\n###### ".into(), "\n\n".into())),
                "STRONG" | "B" => Some(("**".into(), "** ".into())),
                "EM" | "I" => Some(("*".into(), "* ".into())),
                "CODE" => Some(("`".into(), "` ".into())),
                "PRE" => Some(("\n```\n".into(), "\n```\n".into())),
                "A" => {
                    let href = node.attributes.get("href").map(|s| s.as_str()).unwrap_or("");
                    let close = if href.is_empty() {
                        "] ".to_string()
                    } else {
                        format!("]({href}) ")
                    };
                    Some(("[".into(), close))
                }
                _ => None,
            };
            if let Some((prefix, suffix)) = text_wrapper {
                output.push_str(&prefix);
                render_children_text(snapshot, id, output);
                output.push_str(&suffix);
                return;
            }

            // Everything else renders children as pending tasks, in document
            // order (the stack pops them front-to-back, so extend reversed).
            let seq: Vec<Task> = match tag.as_str() {
                "P" => {
                    let mut seq = child_tasks(snapshot, id);
                    seq.push(Task::Emit("\n\n".into()));
                    seq
                }
                "BLOCKQUOTE" => {
                    output.push_str("\n> ");
                    let mut seq = child_tasks(snapshot, id);
                    seq.push(Task::Emit("\n".into()));
                    seq
                }
                "UL" => {
                    let mut seq = list_tasks(snapshot, id, || "- ".to_string());
                    seq.push(Task::Emit("\n".into()));
                    seq
                }
                "OL" => {
                    let mut counter = 1usize;
                    let mut seq = list_tasks(snapshot, id, || {
                        let marker = format!("{counter}. ");
                        counter += 1;
                        marker
                    });
                    seq.push(Task::Emit("\n".into()));
                    seq
                }
                "LI" => {
                    // Fallback for <li> outside <ol>/<ul>
                    output.push_str("- ");
                    let mut seq = child_tasks(snapshot, id);
                    seq.push(Task::Emit("\n".into()));
                    seq
                }
                _ => {
                    // Generic block/inline: just descend into children
                    let mut seq = child_tasks(snapshot, id);
                    // Add trailing newlines for block-level elements
                    if matches!(
                        tag.as_str(),
                        "DIV"
                            | "SECTION"
                            | "ARTICLE"
                            | "HEADER"
                            | "FOOTER"
                            | "NAV"
                            | "MAIN"
                            | "TR"
                            | "FIGURE"
                            | "ASIDE"
                            | "ADDRESS"
                    ) {
                        seq.push(Task::Emit("\n".into()));
                    }
                    seq
                }
            };
            stack.extend(seq.into_iter().rev());
        }
        _ => {
            for &child_id in node.children.iter().rev() {
                stack.push(Task::Node(child_id, 0));
            }
        }
    }
}

/// Direct children of `node_id` as pending tasks, in document order.
fn child_tasks(snapshot: &DomSnapshot, node_id: u32) -> Vec<Task> {
    snapshot
        .nodes
        .get(&node_id)
        .map(|n| n.children.iter().map(|&c| Task::Node(c, 0)).collect())
        .unwrap_or_default()
}

/// `<ul>`/`<ol>` items: each direct `<li>` emits its marker, renders its
/// subtree, then a newline. Non-`<li>` children render normally.
fn list_tasks<F: FnMut() -> String>(snapshot: &DomSnapshot, list_id: u32, mut marker: F) -> Vec<Task> {
    let mut seq = Vec::new();
    if let Some(list) = snapshot.nodes.get(&list_id) {
        for &child_id in &list.children {
            let Some(child) = snapshot.nodes.get(&child_id) else {
                continue;
            };
            if child.node_type == 1 && child.tag.to_uppercase() == "LI" {
                seq.push(Task::Emit(marker()));
                seq.extend(child.children.iter().map(|&gc| Task::Node(gc, 0)));
                seq.push(Task::Emit("\n".into()));
            } else {
                seq.push(Task::Node(child_id, 0));
            }
        }
    }
    seq
}

/// Collect all text content from a node's descendants (no formatting).
fn render_children_text(snapshot: &DomSnapshot, node_id: u32, output: &mut String) {
    let Some(node) = snapshot.nodes.get(&node_id) else {
        return;
    };
    let mut stack: Vec<u32> = node.children.clone();
    stack.reverse();
    while let Some(id) = stack.pop() {
        let Some(child) = snapshot.nodes.get(&id) else {
            continue;
        };
        match child.node_type {
            3 => {
                let text = child.text_content.trim();
                if !text.is_empty() {
                    output.push_str(text);
                    output.push(' ');
                }
            }
            1 => {
                let mut grandchildren = child.children.clone();
                grandchildren.reverse();
                stack.extend(grandchildren);
            }
            _ => {}
        }
    }
}
