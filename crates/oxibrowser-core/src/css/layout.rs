//! Simplified layout engine for agent evaluation.
//!
//! Computes "semantically adequate" style and layout information from inline
//! styles, tag defaults, and DOM structure.  NOT a real CSS layout engine —
//! flexbox, grid, margin collapsing, media queries are all ignored.
//!
//! The purpose is to let AI agents answer:
//! - "Is this element visible?"
//! - "Is this element interactive?"
//! - "Roughly where is this element on the page?"

use crate::js::dom_snapshot::DomSnapshot;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Computed style for a single DOM node.
///
/// Contains only the properties an AI agent needs for evaluation.
/// Non-standard `_visible` and `_interactive` convenience flags are included
/// so agents can make decisions with a single boolean check.
#[derive(Debug, Clone)]
pub struct ComputedStyle {
    // Box model basics
    pub display: String,
    pub visibility: String,
    pub opacity: f64,
    pub position: String,
    pub overflow: String,
    pub pointer_events: String,

    // Typography
    pub color: String,
    pub background_color: String,
    pub font_size: f64, // px
    pub font_weight: String,
    pub text_align: String,

    // Sizing
    pub width: Option<f64>,  // None = auto
    pub height: Option<f64>, // None = auto
    pub margin_top: f64,
    pub margin_bottom: f64,
    pub padding_top: f64,
    pub padding_bottom: f64,
    pub z_index: Option<i32>,

    // Agent convenience flags
    pub visible: bool,
    pub interactive: bool,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: "block".into(),
            visibility: "visible".into(),
            opacity: 1.0,
            position: "static".into(),
            overflow: "visible".into(),
            pointer_events: "auto".into(),
            color: "#000000".into(),
            background_color: "transparent".into(),
            font_size: 16.0,
            font_weight: "normal".into(),
            text_align: "left".into(),
            width: None,
            height: None,
            margin_top: 0.0,
            margin_bottom: 0.0,
            padding_top: 0.0,
            padding_bottom: 0.0,
            z_index: None,
            visible: true,
            interactive: false,
        }
    }
}

/// Estimated bounding rectangle for a DOM node.
///
/// Values are approximations based on DOM order and inline styles, not actual
/// CSS layout calculation.
#[derive(Debug, Clone)]
pub struct LayoutRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl LayoutRect {
    pub fn zero() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Layout engine
// ---------------------------------------------------------------------------

pub struct LayoutEngine;

impl LayoutEngine {
    // =======================================================================
    // Public API
    // =======================================================================

    /// Compute the resolved (aka "computed") style for a node.
    ///
    /// Resolution order:
    /// 1. Tag defaults (user-agent stylesheet)
    /// 2. Inline `style` attribute
    /// 3. `style:*` entries from `setProperty()` calls
    /// 4. Inherited properties from ancestors
    /// 5. Convenience flags (`visible`, `interactive`)
    pub fn compute_style(snapshot: &DomSnapshot, node_id: u32) -> Option<ComputedStyle> {
        let node = snapshot.nodes.get(&node_id)?;

        // 1. Tag defaults
        let mut style = Self::tag_defaults(&node.tag);

        // 2. Inline style attribute
        if let Some(style_str) = node.attributes.get("style") {
            Self::apply_inline_style(&mut style, style_str);
        }

        // 3. style:* entries from setProperty()
        for (key, val) in &node.attributes {
            if let Some(prop) = key.strip_prefix("style:") {
                Self::set_property(&mut style, prop, val);
            }
        }

        // 4. Inherit from parent chain
        Self::inherit_from_parent(snapshot, node_id, &mut style);

        // 5. Convenience flags
        let disabled = node.attributes.contains_key("disabled");

        style.visible = style.display != "none"
            && style.visibility != "hidden"
            && style.visibility != "collapse"
            && style.opacity > 0.0;

        style.interactive = style.visible
            && !disabled
            && style.pointer_events != "none"
            && Self::is_interactive_tag(&node.tag);

        Some(style)
    }

    /// Estimate the bounding rectangle for a node.
    ///
    /// Uses DOM order and inline styles — **not** actual CSS layout.
    pub fn compute_rect(snapshot: &DomSnapshot, node_id: u32) -> LayoutRect {
        // Compute style first — if display:none, return zero rect
        let style = match Self::compute_style(snapshot, node_id) {
            Some(s) => s,
            None => return LayoutRect::zero(),
        };

        if style.display == "none" {
            return LayoutRect::zero();
        }

        let viewport_w = 1280.0;

        let x = 0.0;
        let y = Self::estimate_y(snapshot, node_id);
        let w = Self::estimate_width(snapshot, node_id, viewport_w, &style);
        let h = Self::quick_height(snapshot, node_id, &style);

        LayoutRect {
            x,
            y,
            width: w,
            height: h,
            top: y,
            left: x,
            bottom: y + h,
            right: x + w,
        }
    }

    // =======================================================================
    // Tag defaults
    // =======================================================================

    fn tag_defaults(tag: &str) -> ComputedStyle {
        let mut s = ComputedStyle::default();
        let t = tag.to_uppercase();
        match t.as_str() {
            // Hidden elements
            "HEAD" | "STYLE" | "SCRIPT" | "META" | "LINK" | "NOSCRIPT" | "BASE" | "TITLE" => {
                s.display = "none".into();
            }
            // Block elements
            "DIV" | "SECTION" | "ARTICLE" | "MAIN" | "HEADER" | "FOOTER" | "NAV" | "ASIDE"
            | "ADDRESS" | "FIGURE" | "FIGCAPTION" | "FIELDSET" => {
                s.display = "block".into();
            }
            "P" => {
                s.display = "block".into();
                s.margin_top = 16.0;
                s.margin_bottom = 16.0;
            }
            "H1" => {
                s.display = "block".into();
                s.font_size = 32.0;
                s.font_weight = "bold".into();
                s.margin_top = 21.0;
                s.margin_bottom = 21.0;
            }
            "H2" => {
                s.display = "block".into();
                s.font_size = 24.0;
                s.font_weight = "bold".into();
                s.margin_top = 19.0;
                s.margin_bottom = 19.0;
            }
            "H3" => {
                s.display = "block".into();
                s.font_size = 18.72;
                s.font_weight = "bold".into();
                s.margin_top = 18.0;
                s.margin_bottom = 18.0;
            }
            "H4" => {
                s.display = "block".into();
                s.font_size = 16.0;
                s.font_weight = "bold".into();
                s.margin_top = 17.0;
                s.margin_bottom = 17.0;
            }
            "H5" => {
                s.display = "block".into();
                s.font_size = 13.28;
                s.font_weight = "bold".into();
            }
            "H6" => {
                s.display = "block".into();
                s.font_size = 10.72;
                s.font_weight = "bold".into();
            }
            "BLOCKQUOTE" => {
                s.display = "block".into();
                s.margin_top = 16.0;
                s.margin_bottom = 16.0;
                s.padding_top = 8.0;
                s.padding_bottom = 8.0;
            }
            "PRE" => {
                s.display = "block".into();
                s.font_size = 13.0;
                s.margin_top = 16.0;
                s.margin_bottom = 16.0;
            }
            "HR" => {
                s.display = "block".into();
                s.margin_top = 8.0;
                s.margin_bottom = 8.0;
                s.height = Some(2.0);
            }
            "UL" | "OL" => {
                s.display = "block".into();
                s.margin_top = 16.0;
                s.margin_bottom = 16.0;
                s.padding_top = 8.0;
                s.padding_bottom = 8.0;
            }
            "LI" => {
                s.display = "list-item".into();
            }
            // Inline elements
            "SPAN" | "A" | "STRONG" | "B" | "EM" | "I" | "U" | "SMALL" | "CODE" | "ABBR"
            | "CITE" | "MARK" | "SUB" | "SUP" | "TIME" => {
                s.display = "inline".into();
            }
            "BR" => {
                s.display = "inline".into();
            }
            // Interactive elements
            "BUTTON" => {
                s.display = "inline-block".into();
                s.padding_top = 6.0;
                s.padding_bottom = 6.0;
            }
            "INPUT" => {
                s.display = "inline-block".into();
                s.height = Some(20.0);
            }
            "SELECT" => {
                s.display = "inline-block".into();
                s.height = Some(20.0);
            }
            "TEXTAREA" => {
                s.display = "inline-block".into();
                s.height = Some(40.0);
            }
            // Image
            "IMG" => {
                s.display = "inline".into();
            }
            // Table
            "TABLE" => {
                s.display = "table".into();
            }
            "TR" => {
                s.display = "table-row".into();
            }
            "TD" | "TH" => {
                s.display = "table-cell".into();
            }
            "THEAD" | "TBODY" | "TFOOT" => {
                s.display = "table-row-group".into();
            }
            // Form
            "FORM" => {
                s.display = "block".into();
                s.margin_top = 16.0;
                s.margin_bottom = 16.0;
            }
            "LABEL" => {
                s.display = "inline".into();
            }
            // Default: block
            _ => {
                s.display = "block".into();
            }
        }
        s
    }

    // =======================================================================
    // Inline style parsing
    // =======================================================================

    /// Parse a CSS inline style string like `"display:none; color: red"`
    fn apply_inline_style(style: &mut ComputedStyle, css: &str) {
        for decl in css.split(';') {
            let decl = decl.trim();
            if let Some((prop, val)) = decl.split_once(':') {
                Self::set_property(style, prop.trim(), val.trim());
            }
        }
    }

    /// Apply a single CSS property to a ComputedStyle.
    fn set_property(style: &mut ComputedStyle, prop: &str, val: &str) {
        let p = prop.to_lowercase();
        let v = val.trim();
        match p.as_str() {
            "display" => style.display = v.to_lowercase(),
            "visibility" => style.visibility = v.to_lowercase(),
            "opacity" => style.opacity = v.parse().unwrap_or(1.0),
            "color" => style.color = normalize_color(v),
            "background-color" | "backgroundcolor" => {
                style.background_color = normalize_color(v);
            }
            "font-size" | "fontsize" => style.font_size = parse_length(v).unwrap_or(16.0),
            "font-weight" | "fontweight" => style.font_weight = normalize_font_weight(v),
            "text-align" | "textalign" => style.text_align = v.to_lowercase(),
            "overflow" => style.overflow = v.to_lowercase(),
            "pointer-events" | "pointerevents" => style.pointer_events = v.to_lowercase(),
            "position" => style.position = v.to_lowercase(),
            "width" => style.width = parse_length(v),
            "height" => style.height = parse_length(v),
            "margin-top" | "margintop" => style.margin_top = parse_length(v).unwrap_or(0.0),
            "margin-bottom" | "marginbottom" => {
                style.margin_bottom = parse_length(v).unwrap_or(0.0);
            }
            "padding-top" | "paddingtop" => style.padding_top = parse_length(v).unwrap_or(0.0),
            "padding-bottom" | "paddingbottom" => {
                style.padding_bottom = parse_length(v).unwrap_or(0.0);
            }
            "margin" => {
                let vals = parse_box_shorthand(v);
                style.margin_top = vals[0];
                style.margin_bottom = vals[2];
            }
            "padding" => {
                let vals = parse_box_shorthand(v);
                style.padding_top = vals[0];
                style.padding_bottom = vals[2];
            }
            "z-index" | "zindex" => style.z_index = v.parse().ok(),
            _ => {} // ignore unknown properties
        }
    }

    // =======================================================================
    // Inheritance
    // =======================================================================

    /// Walk the ancestor chain and inherit inheritable CSS properties.
    ///
    /// Inheritable properties: visibility, color, font-size, font-weight, text-align.
    fn inherit_from_parent(snapshot: &DomSnapshot, node_id: u32, style: &mut ComputedStyle) {
        // Collect ancestor IDs from node → body
        let ancestors = collect_ancestors(snapshot, node_id);

        // Apply ancestors in root→leaf order so closer ancestors override farther ones.
        let mut inherited = InheritedProps::default();
        for &aid in ancestors.iter().rev() {
            if let Some(anode) = snapshot.nodes.get(&aid) {
                // Apply inline styles of the ancestor to the inherited props
                if let Some(style_str) = anode.attributes.get("style") {
                    apply_inherited_from_inline(&mut inherited, style_str);
                }
                // Also check style:* entries
                for (key, val) in &anode.attributes {
                    if let Some(prop) = key.strip_prefix("style:") {
                        apply_inherited_prop(&mut inherited, prop, val);
                    }
                }
            }
        }

        // Override style with inherited values (only if our style wasn't explicitly set)
        // Actually, per CSS spec: inherited properties take the *computed* value from parent.
        // We approximate by applying the ancestor chain's inline values.
        if let Some(v) = inherited.visibility {
            style.visibility = v;
        }
        if let Some(v) = inherited.color {
            style.color = v;
        }
        if let Some(v) = inherited.font_size {
            style.font_size = v;
        }
        if let Some(v) = inherited.font_weight {
            style.font_weight = v;
        }
        if let Some(v) = inherited.text_align {
            style.text_align = v;
        }
    }

    // =======================================================================
    // Interactivity
    // =======================================================================

    fn is_interactive_tag(tag: &str) -> bool {
        matches!(
            tag.to_uppercase().as_str(),
            "A" | "BUTTON" | "INPUT" | "SELECT" | "TEXTAREA" | "DETAILS" | "SUMMARY"
        )
    }

    // =======================================================================
    // Rect estimation
    // =======================================================================

    /// Estimate the Y position of a node by accumulating heights of preceding
    /// siblings in the parent's children list, recursively.
    fn estimate_y(snapshot: &DomSnapshot, target_id: u32) -> f64 {
        // Walk from body down to target, accumulating y offsets
        let body_id = match snapshot.body_id {
            Some(id) => id,
            None => return 0.0,
        };

        Self::estimate_y_under(snapshot, body_id, target_id, 0.0).unwrap_or(0.0)
    }

    /// Search for `target_id` under `parent_id`, accumulating Y offsets from
    /// preceding siblings. Returns `None` if target not found in this
    /// subtree.
    ///
    /// Explicit-stack DFS with per-frame running `y` and `AddHeight`
    /// markers (a subtree's height is added to the enclosing frame only
    /// after the subtree search completes, exactly like the recursive
    /// original). No native recursion — deep DOMs must not overflow.
    fn estimate_y_under(
        snapshot: &DomSnapshot,
        parent_id: u32,
        target_id: u32,
        base_y: f64,
    ) -> Option<f64> {
        enum Task {
            Siblings { children: Vec<u32>, idx: usize, y: f64 },
            AddHeight(u32),
        }
        let children = snapshot.nodes.get(&parent_id)?.children.clone();
        let mut stack: Vec<Task> = vec![Task::Siblings { children, idx: 0, y: base_y }];

        enum Step {
            Pop,
            Found(f64),
            Descend { child: u32, y: f64 },
            ApplyHeight(u32),
        }

        loop {
            // Peek (read-only) to decide the next step.
            let step = match stack.last() {
                None => return None,
                Some(Task::AddHeight(child_id)) => Step::ApplyHeight(*child_id),
                Some(Task::Siblings { children, idx, y }) => {
                    if *idx >= children.len() {
                        Step::Pop
                    } else {
                        let child_id = children[*idx];
                        let y_now = *y;
                        if let Some(Task::Siblings { idx: i, .. }) = stack.last_mut() {
                            *i += 1;
                        }
                        if child_id == target_id {
                            return Some(y_now);
                        }
                        Step::Descend { child: child_id, y: y_now }
                    }
                }
            };

            match step {
                Step::Pop => {
                    stack.pop();
                    if stack.is_empty() {
                        return None;
                    }
                }
                Step::Found(y) => return Some(y),
                Step::ApplyHeight(child_id) => {
                    stack.pop(); // the marker itself
                    if let Some(Task::Siblings { y, .. }) = stack.last_mut()
                        && let Some(cs) = Self::compute_style(snapshot, child_id)
                        && cs.display != "none"
                    {
                        let h = Self::quick_height(snapshot, child_id, &cs);
                        *y += cs.margin_top + h + cs.margin_bottom;
                    }
                }
                Step::Descend { child, y } => {
                    let grandchildren = snapshot
                        .nodes
                        .get(&child)
                        .map(|n| n.children.clone())
                        .unwrap_or_default();
                    stack.push(Task::AddHeight(child));
                    stack.push(Task::Siblings { children: grandchildren, idx: 0, y });
                }
            }
        }
    }

    /// Quick (non-recursive) height estimate for sibling accumulation.
    /// Uses explicit height, font-size based estimate, or a default.
    fn quick_height(snapshot: &DomSnapshot, node_id: u32, style: &ComputedStyle) -> f64 {
        if let Some(h) = style.height {
            return h;
        }
        if style.display == "inline" {
            return style.font_size * 1.2;
        }
        let node = snapshot.nodes.get(&node_id);
        let text_h = node
            .map(|n| {
                let len = n.text_content.trim().len() as f64;
                if len == 0.0 {
                    0.0
                } else {
                    ((len / 80.0).ceil().max(1.0)) * style.font_size * 1.2
                }
            })
            .unwrap_or(0.0);
        let child_count = node.map(|n| n.children.len()).unwrap_or(0) as f64;
        let default_h = if child_count > 0.0 {
            child_count * 20.0
        } else {
            style.font_size * 1.2
        };
        text_h.max(default_h) + style.padding_top + style.padding_bottom
    }

    /// Estimate the width of a node.
    fn estimate_width(
        snapshot: &DomSnapshot,
        node_id: u32,
        parent_w: f64,
        style: &ComputedStyle,
    ) -> f64 {
        // Explicit width
        if let Some(w) = style.width {
            return w;
        }

        // Inline elements: text length based
        if style.display == "inline" || style.display == "inline-block" {
            let node = snapshot.nodes.get(&node_id);
            return node
                .map(|n| {
                    let text = n.text_content.trim();
                    if text.is_empty() {
                        style.font_size * 2.0 // minimum width
                    } else {
                        (text.len() as f64) * style.font_size * 0.6
                    }
                })
                .unwrap_or(parent_w);
        }

        // Block elements: fill parent width
        parent_w
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Inherited properties accumulator.
#[derive(Default)]
struct InheritedProps {
    visibility: Option<String>,
    color: Option<String>,
    font_size: Option<f64>,
    font_weight: Option<String>,
    text_align: Option<String>,
}

/// Collect ancestor node IDs from `node_id` up to (but not including) `body_id`.
/// Returns in leaf→root order.
fn collect_ancestors(snapshot: &DomSnapshot, node_id: u32) -> Vec<u32> {
    let mut ancestors = Vec::new();
    let mut current = node_id;
    loop {
        let parent_id = snapshot.nodes.get(&current).and_then(|n| n.parent);
        match parent_id {
            Some(pid) => {
                ancestors.push(pid);
                current = pid;
                // Stop at body (don't inherit from html/head)
                if snapshot.body_id == Some(pid) {
                    break;
                }
            }
            None => break,
        }
    }
    ancestors
}

fn apply_inherited_from_inline(inherited: &mut InheritedProps, css: &str) {
    for decl in css.split(';') {
        let decl = decl.trim();
        if let Some((prop, val)) = decl.split_once(':') {
            apply_inherited_prop(inherited, prop.trim(), val.trim());
        }
    }
}

fn apply_inherited_prop(inherited: &mut InheritedProps, prop: &str, val: &str) {
    match prop.to_lowercase().as_str() {
        "visibility" => inherited.visibility = Some(val.to_lowercase()),
        "color" => inherited.color = Some(normalize_color(val)),
        "font-size" | "fontsize" => {
            inherited.font_size = parse_length(val);
        }
        "font-weight" | "fontweight" => {
            inherited.font_weight = Some(normalize_font_weight(val));
        }
        "text-align" | "textalign" => {
            inherited.text_align = Some(val.to_lowercase());
        }
        _ => {}
    }
}

/// Parse a CSS length value to f64 pixels.
/// Handles: `16px`, `1.5em` (assumes 16px base), `100%` (returns None), plain numbers.
fn parse_length(val: &str) -> Option<f64> {
    let v = val.trim().to_lowercase();

    if v == "auto" || v == "none" || v == "inherit" || v == "initial" {
        return None;
    }

    // Strip units
    if let Some(num) = v.strip_suffix("px") {
        return num.trim().parse::<f64>().ok();
    }
    if let Some(num) = v.strip_suffix("rem") {
        return num.trim().parse::<f64>().ok().map(|n| n * 16.0);
    }
    if let Some(num) = v.strip_suffix("em") {
        return num.trim().parse::<f64>().ok().map(|n| n * 16.0);
    }
    if v.ends_with('%') {
        return None; // percentage requires context
    }
    if let Some(num) = v.strip_suffix("pt") {
        return num.trim().parse::<f64>().ok().map(|n| n * 1.333);
    }
    if v.ends_with("vh") {
        return None; // viewport-relative
    }
    if v.ends_with("vw") {
        return None;
    }

    // Plain number (treat as px)
    v.parse::<f64>().ok()
}

/// Normalize a CSS color value to hex format `#rrggbb`.
fn normalize_color(val: &str) -> String {
    let v = val.trim().to_lowercase();

    // Named colors (most common subset)
    match v.as_str() {
        "transparent" => return "transparent".into(),
        "white" => return "#ffffff".into(),
        "black" => return "#000000".into(),
        "red" => return "#ff0000".into(),
        "green" => return "#008000".into(),
        "blue" => return "#0000ff".into(),
        "yellow" => return "#ffff00".into(),
        "orange" => return "#ffa500".into(),
        "purple" => return "#800080".into(),
        "pink" => return "#ffc0cb".into(),
        "gray" | "grey" => return "#808080".into(),
        "silver" => return "#c0c0c0".into(),
        "maroon" => return "#800000".into(),
        "navy" => return "#000080".into(),
        "teal" => return "#008080".into(),
        "olive" => return "#808000".into(),
        "lime" => return "#00ff00".into(),
        "aqua" | "cyan" => return "#00ffff".into(),
        "fuchsia" | "magenta" => return "#ff00ff".into(),
        "currentcolor" => return "#000000".into(), // fallback
        _ => {}
    }

    // #rgb → #rrggbb
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() == 3
            && let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..1].repeat(2), 16),
                u8::from_str_radix(&hex[1..2].repeat(2), 16),
                u8::from_str_radix(&hex[2..3].repeat(2), 16),
            )
        {
            return format!("#{:02x}{:02x}{:02x}", r, g, b);
        }
        if hex.len() == 6
            && let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            )
        {
            return format!("#{:02x}{:02x}{:02x}", r, g, b);
        }
        // Return as-is for 8-digit hex or invalid
        return format!("#{}", hex);
    }

    // rgb(r, g, b)
    if v.starts_with("rgb(") {
        let inner = v.trim_start_matches("rgb(").trim_end_matches(')');
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3 {
            let r = parts[0].trim().parse::<u8>().unwrap_or(0);
            let g = parts[1].trim().parse::<u8>().unwrap_or(0);
            let b = parts[2].trim().parse::<u8>().unwrap_or(0);
            return format!("#{:02x}{:02x}{:02x}", r, g, b);
        }
    }

    // rgba(r, g, b, a) — ignore alpha, convert to rgb
    if v.starts_with("rgba(") {
        let inner = v.trim_start_matches("rgba(").trim_end_matches(')');
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() >= 3 {
            let r = parts[0].trim().parse::<u8>().unwrap_or(0);
            let g = parts[1].trim().parse::<u8>().unwrap_or(0);
            let b = parts[2].trim().parse::<u8>().unwrap_or(0);
            return format!("#{:02x}{:02x}{:02x}", r, g, b);
        }
    }

    // Unknown format — return as-is
    val.to_string()
}

/// Normalize font-weight values.
fn normalize_font_weight(val: &str) -> String {
    match val.to_lowercase().as_str() {
        "normal" => "normal".into(),
        "bold" => "bold".into(),
        "bolder" => "bold".into(),
        "lighter" => "normal".into(),
        n => {
            // Numeric: 100-900
            if let Ok(w) = n.parse::<u32>() {
                match w {
                    100..=300 => "normal".into(),
                    400..=500 => "normal".into(),
                    600..=900 => "bold".into(),
                    _ => "normal".into(),
                }
            } else {
                val.to_lowercase()
            }
        }
    }
}

/// Parse CSS box shorthand: `"10px"` → [10, 10, 10, 10], `"10px 20px"` → [10, 20, 10, 20], etc.
fn parse_box_shorthand(val: &str) -> [f64; 4] {
    let parts: Vec<f64> = val.split_whitespace().filter_map(parse_length).collect();

    match parts.len() {
        1 => [parts[0]; 4],
        2 => [parts[0], parts[1], parts[0], parts[1]],
        3 => [parts[0], parts[1], parts[2], parts[1]],
        4 => [parts[0], parts[1], parts[2], parts[3]],
        _ => [0.0; 4],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::dom_snapshot::{DomNode, DomSnapshot};

    /// Helper: build a DomSnapshot from HTML-like description.
    /// Returns the snapshot and a map of tag→node_id for the body's direct children.
    fn make_simple_snapshot(html_body: &str) -> (DomSnapshot, Vec<(String, u32)>) {
        let mut nodes = std::collections::HashMap::new();
        let mut id_counter = 1u32;

        let root_id = id_counter;
        id_counter += 1;
        nodes.insert(
            root_id,
            DomNode {
                id: root_id,
                tag: "html".into(),
                attributes: Default::default(),
                text_content: String::new(),
                children: vec![],
                parent: None,
                node_type: 1,
            },
        );

        let body_id = id_counter;
        id_counter += 1;
        nodes.insert(
            body_id,
            DomNode {
                id: body_id,
                tag: "body".into(),
                attributes: Default::default(),
                text_content: String::new(),
                children: vec![],
                parent: Some(root_id),
                node_type: 1,
            },
        );
        nodes.get_mut(&root_id).unwrap().children.push(body_id);

        let mut tagged = Vec::new();

        // Hand-rolled parser for <tag attr="val">text</tag>
        let mut pos = 0;
        let b = html_body.as_bytes();
        while pos < b.len() {
            if b[pos] != b'<' {
                pos += 1;
                continue;
            }
            if pos + 1 < b.len() && b[pos + 1] == b'/' {
                pos += 1;
                continue;
            }
            let tag_start = pos + 1;
            let Some(rel_gt) = b[pos..].iter().position(|&c| c == b'>') else {
                break;
            };
            let gt_pos = pos + rel_gt;
            let tag_inner = &html_body[tag_start..gt_pos];
            pos = gt_pos + 1;
            let (tag, attr_str) = tag_inner
                .find(|c: char| c.is_whitespace())
                .map_or((tag_inner, ""), |i| {
                    (&tag_inner[..i], tag_inner[i..].trim())
                });
            if tag.is_empty() {
                continue;
            }
            let close = format!("</{}>", tag);
            let text_end = html_body[pos..]
                .find(&close)
                .map(|i| pos + i)
                .unwrap_or(html_body.len());
            let text = html_body[pos..text_end].trim().to_string();
            pos = std::cmp::min(text_end + close.len(), html_body.len());
            let nid = id_counter;
            id_counter += 1;
            let mut attrs = std::collections::HashMap::new();
            let mut ac = attr_str.chars().peekable();
            while let Some(&ch) = ac.peek() {
                if ch.is_whitespace() {
                    ac.next();
                    continue;
                }
                // Read key manually (don't consume delimiter)
                let mut key = String::new();
                while let Some(&c) = ac.peek() {
                    if c == '=' || c.is_whitespace() {
                        break;
                    }
                    key.push(c);
                    ac.next();
                }
                if key.is_empty() {
                    ac.next();
                    continue;
                }
                // Check for =
                if ac.peek() == Some(&'=') {
                    ac.next(); // consume =
                    if ac.peek() == Some(&'"') {
                        ac.next();
                    } // consume opening "
                    let mut val = String::new();
                    while let Some(&c) = ac.peek() {
                        if c == '"' {
                            ac.next();
                            break;
                        } // consume closing "
                        val.push(c);
                        ac.next();
                    }
                    attrs.insert(key, val);
                } else {
                    attrs.insert(key, String::new());
                }
            }
            nodes.insert(
                nid,
                DomNode {
                    id: nid,
                    tag: tag.to_string(),
                    attributes: attrs,
                    text_content: text,
                    children: vec![],
                    parent: Some(body_id),
                    node_type: 1,
                },
            );
            nodes.get_mut(&body_id).unwrap().children.push(nid);
            tagged.push((tag.to_string(), nid));
        }

        let mut snap = DomSnapshot::empty();
        snap.url = "http://test/".into();
        snap.nodes = nodes;
        snap.root_id = root_id;
        snap.body_id = Some(body_id);

        (snap, tagged)
    }

    // ── ComputedStyle tests ──

    #[test]
    fn test_display_none_is_invisible() {
        let (snap, tags) = make_simple_snapshot(r##"<div style="display:none">hidden</div>"##);
        let (tag, id) = &tags[0];
        assert_eq!(tag, "div");
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert_eq!(style.display, "none");
        assert!(!style.visible);
    }

    #[test]
    fn test_visibility_hidden_is_invisible() {
        let (snap, tags) = make_simple_snapshot(r#"<p style="visibility:hidden">hidden</p>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert!(!style.visible);
    }

    #[test]
    fn test_opacity_zero_is_invisible() {
        let (snap, tags) = make_simple_snapshot(r#"<span style="opacity:0">ghost</span>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert!(!style.visible);
    }

    #[test]
    fn test_button_is_interactive() {
        let (snap, tags) = make_simple_snapshot(r#"<button>Click</button>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert!(style.interactive);
        assert!(style.visible);
    }

    #[test]
    fn test_disabled_button_not_interactive() {
        let (snap, tags) = make_simple_snapshot(r#"<button disabled>Click</button>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert!(!style.interactive);
    }

    #[test]
    fn test_anchor_is_interactive() {
        let (snap, tags) = make_simple_snapshot(r#"<a>Link</a>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert!(style.interactive);
    }

    #[test]
    fn test_div_not_interactive() {
        let (snap, tags) = make_simple_snapshot(r#"<div>Block</div>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert!(!style.interactive);
    }

    #[test]
    fn test_inline_style_color() {
        let (snap, tags) = make_simple_snapshot(r##"<p style="color:red">Red</p>"##);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert_eq!(style.color, "#ff0000");
    }

    #[test]
    fn test_inline_style_font_size_px() {
        let (snap, tags) = make_simple_snapshot(r#"<p style="font-size:24px">Big</p>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert_eq!(style.font_size, 24.0);
    }

    #[test]
    fn test_inline_style_font_size_em() {
        let (snap, tags) = make_simple_snapshot(r#"<p style="font-size:1.5em">Big</p>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert_eq!(style.font_size, 24.0); // 1.5 * 16
    }

    #[test]
    fn test_explicit_width_and_height() {
        let (snap, tags) =
            make_simple_snapshot(r#"<div style="width:200px;height:100px">Box</div>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert_eq!(style.width, Some(200.0));
        assert_eq!(style.height, Some(100.0));
    }

    #[test]
    fn test_position_absolute() {
        let (snap, tags) =
            make_simple_snapshot(r#"<div style="position:absolute;top:50px;left:100px">Abs</div>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert_eq!(style.position, "absolute");
    }

    #[test]
    fn test_heading_defaults() {
        let (snap, tags) = make_simple_snapshot(r#"<h1>Title</h1>"#);
        let (_, id) = &tags[0];
        let style = LayoutEngine::compute_style(&snap, *id).unwrap();
        assert_eq!(style.font_size, 32.0);
        assert_eq!(style.font_weight, "bold");
    }

    // ── LayoutRect tests ──

    #[test]
    fn test_display_none_zero_rect() {
        let (snap, tags) = make_simple_snapshot(r#"<div style="display:none">hidden</div>"#);
        let (_, id) = &tags[0];
        let rect = LayoutEngine::compute_rect(&snap, *id);
        assert_eq!(rect.width, 0.0);
        assert_eq!(rect.height, 0.0);
    }

    #[test]
    fn test_explicit_size_rect() {
        let (snap, tags) =
            make_simple_snapshot(r#"<div style="width:200px;height:100px">Box</div>"#);
        let (_, id) = &tags[0];
        let rect = LayoutEngine::compute_rect(&snap, *id);
        assert_eq!(rect.width, 200.0);
        assert_eq!(rect.height, 100.0);
    }

    #[test]
    fn test_sibling_below_previous() {
        let (snap, tags) = make_simple_snapshot(
            r#"<div style="height:50px">A</div><div style="height:80px">B</div>"#,
        );
        let (_, id_a) = &tags[0];
        let (_, id_b) = &tags[1];
        let rect_a = LayoutEngine::compute_rect(&snap, *id_a);
        let rect_b = LayoutEngine::compute_rect(&snap, *id_b);
        assert!(
            rect_b.top >= rect_a.top,
            "B ({}) should be below A ({})",
            rect_b.top,
            rect_a.top
        );
        assert!(rect_b.top > 0.0);
    }

    #[test]
    fn test_inline_element_narrower_than_block() {
        let (snap, tags) = make_simple_snapshot(
            r#"<span style="display:inline">short</span><div style="display:block;width:1280px">wide</div>"#,
        );
        let (_, span_id) = &tags[0];
        let (_, div_id) = &tags[1];
        let span_rect = LayoutEngine::compute_rect(&snap, *span_id);
        let div_rect = LayoutEngine::compute_rect(&snap, *div_id);
        assert!(span_rect.width < div_rect.width);
    }

    // ── Color parsing tests ──

    #[test]
    fn test_normalize_color_hex() {
        assert_eq!(normalize_color("#ff0000"), "#ff0000");
        assert_eq!(normalize_color("#f00"), "#ff0000");
        assert_eq!(normalize_color("#abc"), "#aabbcc");
    }

    #[test]
    fn test_normalize_color_rgb() {
        assert_eq!(normalize_color("rgb(255, 0, 128)"), "#ff0080");
    }

    #[test]
    fn test_normalize_color_named() {
        assert_eq!(normalize_color("red"), "#ff0000");
        assert_eq!(normalize_color("BLUE"), "#0000ff");
        assert_eq!(normalize_color("transparent"), "transparent");
    }

    // ── Length parsing tests ──

    #[test]
    fn test_parse_length_px() {
        assert_eq!(parse_length("16px"), Some(16.0));
        assert_eq!(parse_length("24.5px"), Some(24.5));
    }

    #[test]
    fn test_parse_length_em() {
        assert_eq!(parse_length("1.5em"), Some(24.0));
        assert_eq!(parse_length("2rem"), Some(32.0));
    }

    #[test]
    fn test_parse_length_auto() {
        assert_eq!(parse_length("auto"), None);
        assert_eq!(parse_length("none"), None);
    }

    #[test]
    fn test_parse_length_plain_number() {
        assert_eq!(parse_length("100"), Some(100.0));
    }

    #[test]
    fn test_font_weight_normalization() {
        assert_eq!(normalize_font_weight("bold"), "bold");
        assert_eq!(normalize_font_weight("700"), "bold");
        assert_eq!(normalize_font_weight("400"), "normal");
        assert_eq!(normalize_font_weight("bolder"), "bold");
    }
}
