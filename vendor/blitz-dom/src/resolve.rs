//! Resolve style and layout

use std::{
    cell::RefCell,
    time::{SystemTime, UNIX_EPOCH},
};

use debug_timer::debug_timer;
use kurbo::{Affine, Rect};
use parley::LayoutContext;
use selectors::Element as _;
use style::dom::TDocument;

#[cfg(feature = "parallel-construct")]
use rayon::prelude::*;

// FIXME: static thread_local FontCtx isn't necessarily correct in multi-document context.
// Should use thread_local crate with ThreadLocal value store in the Document.
thread_local! {
    pub(crate) static LAYOUT_CTX: RefCell<Option<Box<LayoutContext<TextBrush>>>> = const { RefCell::new(None) };
}

use style::selector_parser::RestyleDamage;
use taffy::AvailableSpace;

use crate::{
    BaseDocument,
    events::ScrollAnimationState,
    layout::{
        construct::{
            ConstructionTask, ConstructionTaskData, ConstructionTaskResult,
            ConstructionTaskResultData, LayoutChildren, build_inline_layout_into,
            collect_layout_children,
        },
        damage::{ALL_DAMAGE, CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC},
    },
    node::TextBrush,
};

impl BaseDocument {
    /// Restyle the tree and then relayout it
    pub fn resolve(&mut self, current_time_for_animations: f64) {
        self.sync_stylesheets();
        if TDocument::as_node(&&self.nodes[0])
            .first_element_child()
            .is_none()
        {
            #[cfg(feature = "tracing")]
            tracing::warn!("No DOM - not resolving");
            return;
        }

        // Process messages that have been sent to our message channel (e.g. loaded resource)
        self.handle_messages();

        self.resolve_scroll_animation();

        // Drop scrollbar-activity entries whose fade-out has finished (also
        // sheds entries for removed nodes).
        {
            use crate::node::scrollbar::{FADE_DELAY, FADE_DURATION};
            self.scrollbar_activity
                .retain(|_, last| last.elapsed() < FADE_DELAY + FADE_DURATION);
        }

        let root_node_id = self.root_element().id;
        debug_timer!(timer, feature = "log-phase-times");

        // we need to resolve stylist first since it will need to drive our layout bits
        self.resolve_stylist(current_time_for_animations);
        timer.record_time("style");

        // Propagate damage flags (from mutation and restyles) up and down the tree
        if self.incremental_layout {
            self.propagate_damage_flags(root_node_id, RestyleDamage::empty());
            timer.record_time("damage");
        }

        // Fix up tree for layout (insert anonymous blocks as necessary, etc)
        self.resolve_layout_children();
        timer.record_time("construct");

        self.resolve_deferred_tasks();
        timer.record_time("pconstruct");

        // Merge stylo into taffy
        self.flush_styles_to_layout(root_node_id);
        timer.record_time("flush");

        // Next we resolve layout with the data resolved by stlist
        self.resolve_layout();
        timer.record_time("layout");

        self.resolve_transforms(root_node_id);
        timer.record_time("transform");

        // Clear all damage and dirty flags
        if self.incremental_layout {
            for (_, node) in self.nodes.iter_mut() {
                node.clear_damage_mut();
                node.unset_dirty_descendants();
            }
            timer.record_time("c_damage");
        }

        let mut subdoc_is_animating = false;
        for &node_id in &self.sub_document_nodes {
            let node = &mut self.nodes[node_id];
            let size = node.final_layout.size;
            if let Some(mut sub_doc) = node.subdoc_mut().map(|doc| doc.inner_mut()) {
                // Set viewport
                // viewport_mut handles change detection. So we just unconditionally set the values;
                let mut sub_viewport = sub_doc.viewport_mut();
                sub_viewport.hidpi_scale = self.viewport.hidpi_scale;
                sub_viewport.zoom = self.viewport.zoom;
                sub_viewport.color_scheme = self.viewport.color_scheme;

                let viewport_scale = self.viewport.scale();
                sub_viewport.window_size = (
                    (size.width * viewport_scale) as u32,
                    (size.height * viewport_scale) as u32,
                );
                drop(sub_viewport);

                sub_doc.resolve(current_time_for_animations);

                subdoc_is_animating |= sub_doc.is_animating();
            }
        }
        self.subdoc_is_animating = subdoc_is_animating;
        timer.record_time("subdocs");

        timer.print_times(&format!("Resolve({}): ", self.id()));
    }

    fn resolve_transforms(&mut self, node_id: usize) -> Rect {
        if !self.nodes.contains(node_id) {
            return Rect::ZERO;
        }

        if !self.nodes[node_id]
            .damage()
            .map(|d| d.contains(style::selector_parser::RestyleDamage::RECALCULATE_OVERFLOW))
            .unwrap_or(false)
        {
            return self.nodes[node_id].scrollable_overflow;
        }

        let scale = self.viewport.scale_f64();

        let transform = self.nodes[node_id].set_transform(scale as f32);

        let w = self.nodes[node_id].final_layout.size.width as f64 * scale;
        let h = self.nodes[node_id].final_layout.size.height as f64 * scale;
        let mut overflow = Rect::new(0.0, 0.0, w, h);

        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());

        if let Some(ref children) = layout_children {
            for &child_id in children {
                let child_rect_in_self = self.resolve_transforms(child_id);
                overflow = overflow.union(child_rect_in_self);
            }
        }
        if let Some(before) = self.nodes[node_id].before {
            let child_rect_in_self = self.resolve_transforms(before);
            overflow = overflow.union(child_rect_in_self);
        }
        if let Some(after) = self.nodes[node_id].after {
            let child_rect_in_self = self.resolve_transforms(after);
            overflow = overflow.union(child_rect_in_self);
        }

        self.nodes[node_id].scrollable_overflow = overflow;
        *self.nodes[node_id].layout_children.get_mut() = layout_children;

        let scaled_x = self.nodes[node_id].final_layout.location.x as f64 * scale;
        let scaled_y = self.nodes[node_id].final_layout.location.y as f64 * scale;

        let full = if let Some(t) = transform {
            Affine::translate((scaled_x, scaled_y)) * t
        } else {
            Affine::translate((scaled_x, scaled_y))
        };

        full.transform_rect_bbox(overflow)
    }

    pub fn resolve_scroll_animation(&mut self) {
        match &mut self.scroll_animation {
            ScrollAnimationState::Fling(fling_state) => {
                let time_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64 as f64;

                let time_diff_ms = time_ms - fling_state.last_seen_time;

                // 0.95 @ 60fps normalized to actual frame times
                let deceleration = 1.0 - ((0.05 / 16.66666) * time_diff_ms);

                fling_state.x_velocity *= deceleration;
                fling_state.y_velocity *= deceleration;
                fling_state.last_seen_time = time_ms;
                let fling_state = fling_state.clone();

                let dx = fling_state.x_velocity * time_diff_ms;
                let dy = fling_state.y_velocity * time_diff_ms;

                self.scroll_by(Some(fling_state.target), dx, dy, &mut |_| {});
                if fling_state.x_velocity.abs() < 0.1 && fling_state.y_velocity.abs() < 0.1 {
                    self.scroll_animation = ScrollAnimationState::None;
                }
            }
            ScrollAnimationState::None => {
                // Do nothing
            }
        }
    }

    /// Ensure that the layout_children field is populated for all nodes
    pub fn resolve_layout_children(&mut self) {
        resolve_layout_children_recursive(self, self.root_node().id);

        fn resolve_layout_children_recursive(doc: &mut BaseDocument, node_id: usize) {
            // Anonymous blocks and pseudo-elements can be removed from the slab
            // between render passes. Bail out rather than panicking on a stale key.
            if doc.nodes.get(node_id).is_none() {
                return;
            }

            let mut damage = doc.nodes[node_id].damage().unwrap_or(ALL_DAMAGE);
            let _flags = doc.nodes[node_id].flags;

            if !doc.incremental_layout || damage.intersects(CONSTRUCT_FC | CONSTRUCT_BOX) {
                //} || flags.contains(NodeFlags::IS_INLINE_ROOT) {
                let mut collected = LayoutChildren::default();
                collect_layout_children(doc, node_id, &mut collected);
                let layout_children = collected.children;

                // Recurse into newly collected layout children
                for child_id in layout_children.iter().copied() {
                    resolve_layout_children_recursive(doc, child_id);
                    doc.nodes[child_id].layout_parent.set(Some(node_id));
                    if let Some(mut data) = doc.nodes[child_id].stylo_element_data.get_mut() {
                        data.damage
                            .remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                }

                *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children.clone());
                // *doc.nodes[node_id].paint_children.borrow_mut() = Some(layout_children);

                damage.remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            } else {
                //if damage.contains(CONSTRUCT_DESCENDENT) {
                let layout_children = doc.nodes[node_id].layout_children.borrow_mut().take();
                if let Some(layout_children) = layout_children {
                    for child_id in layout_children.iter().copied() {
                        // Anonymous blocks and pseudo-elements can be removed from the
                        // slab between render passes; skip stale IDs.
                        if !doc.nodes.contains(child_id) {
                            continue;
                        }
                        resolve_layout_children_recursive(doc, child_id);
                        doc.nodes[child_id].layout_parent.set(Some(node_id));
                    }

                    *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children);
                }

                // damage.remove(CONSTRUCT_DESCENDENT);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            }

            doc.nodes[node_id].set_damage(damage);
        }
    }

    pub fn resolve_deferred_tasks(&mut self) {
        let mut deferred_construction_nodes = std::mem::take(&mut self.deferred_construction_nodes);

        // Deduplicate deferred tasks by node_id to avoid redundant work
        deferred_construction_nodes.sort_unstable_by_key(|task| task.node_id);
        deferred_construction_nodes.dedup_by_key(|task| task.node_id);

        #[cfg(feature = "parallel-construct")]
        let iter = deferred_construction_nodes.into_par_iter();
        #[cfg(not(feature = "parallel-construct"))]
        let iter = deferred_construction_nodes.into_iter();

        let results: Vec<ConstructionTaskResult> = iter
            .map(|task: ConstructionTask| match task.data {
                ConstructionTaskData::InlineLayout(mut layout) => {
                    #[cfg(feature = "parallel-construct")]
                    let mut layout_ctx = LAYOUT_CTX
                        .take()
                        .unwrap_or_else(|| Box::new(LayoutContext::new()));
                    #[cfg(feature = "parallel-construct")]
                    let layout_ctx_mut = &mut layout_ctx;

                    #[cfg(feature = "parallel-construct")]
                    let mut font_ctx = self
                        .thread_font_contexts
                        .get_or(|| RefCell::new(Box::new(self.font_ctx.lock().unwrap().clone())))
                        .borrow_mut();
                    #[cfg(feature = "parallel-construct")]
                    let font_ctx_mut = &mut *font_ctx;

                    #[cfg(not(feature = "parallel-construct"))]
                    let layout_ctx_mut = &mut self.layout_ctx;
                    #[cfg(not(feature = "parallel-construct"))]
                    let font_ctx_mut = &mut *self.font_ctx.lock().unwrap();

                    layout.content_widths = None;
                    build_inline_layout_into(
                        &self.nodes,
                        layout_ctx_mut,
                        font_ctx_mut,
                        &mut layout,
                        self.viewport.scale(),
                        task.node_id,
                    );

                    #[cfg(feature = "parallel-construct")]
                    {
                        LAYOUT_CTX.set(Some(layout_ctx));
                    }

                    // If layout doesn't contain any inline boxes, then it is safe to populate the content_widths
                    // cache during this parallelized stage.
                    // if layout.layout.inline_boxes().is_empty() {
                    //     layout.content_widths();
                    // }

                    ConstructionTaskResult {
                        node_id: task.node_id,
                        data: ConstructionTaskResultData::InlineLayout(layout),
                    }
                }
            })
            .collect();

        for result in results {
            match result.data {
                ConstructionTaskResultData::InlineLayout(layout) => {
                    self.nodes[result.node_id].cache.clear();
                    self.nodes[result.node_id]
                        .element_data_mut()
                        .unwrap()
                        .inline_layout_data = Some(layout);
                    // Span ownership in Parley's style tree is not layout
                    // ownership. Embedded boxes are positioned and painted in
                    // the formatting root, even when nested in an inline label.
                    let children = self.nodes[result.node_id]
                        .layout_children
                        .borrow()
                        .clone()
                        .unwrap_or_default();
                    for child in children {
                        self.nodes[child].layout_parent.set(Some(result.node_id));
                    }
                }
            }
        }

        self.deferred_construction_nodes.clear();
    }

    /// Walk the nodes now that they're properly styled and transfer their styles to the taffy style system
    ///
    /// TODO: update taffy to use an associated type instead of slab key
    /// TODO: update taffy to support traited styles so we don't even need to rely on taffy for storage
    pub fn resolve_layout(&mut self) {
        let size = self.stylist.device().au_viewport_size();

        let available_space = taffy::Size {
            width: AvailableSpace::Definite(size.width.to_f32_px()),
            height: AvailableSpace::Definite(size.height.to_f32_px()),
        };

        let root_element_id = taffy::NodeId::from(self.root_element().id);

        // println!("\n\nRESOLVE LAYOUT\n===========\n");

        taffy::compute_root_layout(self, root_element_id, available_space);
        self.resolve_absolute_containing_blocks();
        taffy::round_layout(self, root_element_id);
        self.resolve_inline_element_boxes();

        // println!("\n\n");
        // taffy::print_tree(self, root_node_id)
    }

    // Taffy positions absolute items against their immediate layout container.
    // CSS instead uses the nearest containing block, which may skip static
    // ancestors. Reuse the same absolute sizing algorithm with that area and
    // express its result back in the original layout parent's coordinates so
    // paint, scrolling, hit testing and geometry retain a single layout tree.
    fn resolve_absolute_containing_blocks(&mut self) {
        use style::computed_values::position::T as CssPosition;
        use taffy::{Point, Size};

        fn origin(doc: &BaseDocument, mut id: usize) -> Point<f32> {
            let mut point = Point::ZERO;
            loop {
                let node = &doc.nodes[id];
                point.x += node.unrounded_layout.location.x;
                point.y += node.unrounded_layout.location.y;
                let Some(parent) = node.layout_parent.get() else {
                    break;
                };
                id = parent;
            }
            point
        }

        let viewport = self.stylist.device().au_viewport_size();
        let viewport = Size {
            width: viewport.width.to_f32_px(),
            height: viewport.height.to_f32_px(),
        };
        let mut stack = vec![self.root_element().id];
        while let Some(id) = stack.pop() {
            stack.extend(self.nodes[id].children.iter().rev().copied());
            let Some(styles) = self.nodes[id].primary_styles() else {
                continue;
            };
            let position = styles.clone_position();
            if !position.is_absolutely_positioned() || styles.clone_display().is_none() {
                continue;
            }
            drop(styles);
            let Some(parent) = self.nodes[id].layout_parent.get() else {
                continue;
            };
            let parent_origin = origin(self, parent);
            let mut ancestor = self.nodes[id].parent;
            let mut containing = None;
            while let Some(candidate) = ancestor {
                let node = &self.nodes[candidate];
                let establishes = node.primary_styles().is_some_and(|style| {
                    !style.clone_display().is_contents()
                        && ((position != CssPosition::Fixed
                            && style.clone_position() != CssPosition::Static)
                            || !style.get_box().transform.0.is_empty())
                });
                if establishes {
                    containing = Some(candidate);
                    break;
                }
                ancestor = node.parent;
            }
            // The normal Taffy container algorithm already has the correct
            // area here (including grid/flex-specific absolute placement).
            if containing == Some(parent) {
                continue;
            }
            let (area_size, area_origin) = if let Some(containing) = containing {
                let layout = &self.nodes[containing].unrounded_layout;
                let offset = origin(self, containing);
                (
                    Size {
                        width: layout.size.width - layout.border.left - layout.border.right,
                        height: layout.size.height - layout.border.top - layout.border.bottom,
                    },
                    Point {
                        x: offset.x + layout.border.left,
                        y: offset.y + layout.border.top,
                    },
                )
            } else {
                (viewport, Point::ZERO)
            };
            let layout = self.nodes[id].unrounded_layout;
            let item = parley::PositionedInlineBox {
                id: id as u64,
                kind: parley::InlineBoxKind::OutOfFlow,
                x: layout.location.x - layout.margin.left,
                y: layout.location.y - layout.margin.top,
                width: layout.size.width,
                height: layout.size.height,
            };
            let area_offset = Point {
                x: area_origin.x - parent_origin.x,
                y: area_origin.y - parent_origin.y,
            };
            let direction = self.nodes[id].style.direction;
            self.nodes[id].cache.clear();
            crate::layout::inline::layout_abspos_child(
                self,
                item,
                area_size,
                area_offset,
                direction,
            );
        }
    }

    // Inline text is painted from its formatting root's Parley layout, so it
    // never receives a Taffy box. Record the same glyph-run rectangles used by
    // painting as observation metadata, without creating additional paint boxes.
    fn resolve_inline_element_boxes(&mut self) {
        use parley::layout::PositionedLayoutItem;
        use std::collections::{HashMap, HashSet};

        let layout_boxes: HashSet<_> = self
            .nodes
            .iter()
            .flat_map(|(_, node)| node.layout_children.borrow().clone().unwrap_or_default())
            .collect();
        let mut boxes: HashMap<usize, (usize, Rect)> = HashMap::new();
        let scale = self.viewport.scale_f64();
        for (root_id, root) in self.nodes.iter() {
            let Some(layout) = root
                .element_data()
                .and_then(|el| el.inline_layout_data.as_ref())
            else {
                continue;
            };
            let offset_x = (root.final_layout.border.left + root.final_layout.padding.left) as f64;
            let offset_y = (root.final_layout.border.top + root.final_layout.padding.top) as f64;
            for line in layout.layout.lines() {
                for item in line.items() {
                    let PositionedLayoutItem::GlyphRun(run) = item else {
                        continue;
                    };
                    let metrics = run.run().metrics();
                    let rect = Rect::new(
                        offset_x + run.offset() as f64 / scale,
                        offset_y + (run.baseline() - metrics.ascent) as f64 / scale,
                        offset_x + (run.offset() + run.advance()) as f64 / scale,
                        offset_y + (run.baseline() + metrics.descent) as f64 / scale,
                    );
                    let mut owner = Some(run.style().brush.id);
                    while let Some(id) = owner {
                        if id == root_id {
                            break;
                        }
                        let node = &self.nodes[id];
                        if !layout_boxes.contains(&id)
                            && node
                                .primary_styles()
                                .is_some_and(|s| s.clone_display().is_inline_flow())
                        {
                            boxes
                                .entry(id)
                                .and_modify(|(_, bounds)| *bounds = bounds.union(rect))
                                .or_insert((root_id, rect));
                        }
                        owner = node.parent;
                    }
                }
            }
        }
        for (id, node) in self.nodes.iter_mut() {
            if !layout_boxes.contains(&id)
                && node
                    .primary_styles()
                    .is_some_and(|s| s.clone_display().is_inline_flow())
            {
                node.final_layout = taffy::Layout::new();
                node.unrounded_layout = taffy::Layout::new();
            }
        }
        for (id, (root, rect)) in boxes {
            let node = &mut self.nodes[id];
            node.layout_parent.set(Some(root));
            node.final_layout.location = taffy::Point {
                x: rect.x0 as f32,
                y: rect.y0 as f32,
            };
            node.final_layout.size = taffy::Size {
                width: rect.width() as f32,
                height: rect.height() as f32,
            };
            node.unrounded_layout = node.final_layout;
        }
    }
}
