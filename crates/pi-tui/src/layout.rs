//! Layout system for constrained rendering in TuiAltScreen.
//!
//! This module implements the layout system described in `tui-plan.md`:
//! - LayoutBox/LayoutFrame for tracking component positions
//! - Stack layout allocation with basis/grow/shrink
//! - Painting visible rows with clipping
//! - Hit testing for mouse events

use std::sync::Arc;

use super::component::Component;
use super::layout_node::LayoutViewport;
use super::scroll_view::ScrollView;
use super::vstack::{layout_vstack_constrained, VStack};
use crate::ansi::{slice_by_column, visible_width, CURSOR_MARKER};

/// A rectangle in the layout.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl LayoutRect {
    pub fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self { x, y, width, height }
    }

    /// Check if a point is inside this rectangle.
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// Intersect with another rectangle.
    pub fn intersect(&self, other: &LayoutRect) -> LayoutRect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        LayoutRect {
            x,
            y,
            width: right.saturating_sub(x),
            height: bottom.saturating_sub(y),
        }
    }
}

/// A box in the layout tree.
#[derive(Clone)]
pub struct LayoutBox {
    /// The component this box represents.
    pub component: Arc<dyn Component>,
    /// The allocated rectangle.
    pub rect: LayoutRect,
    /// The clipping rectangle (intersection of all ancestor clips).
    pub clip: LayoutRect,
    /// Child boxes.
    pub children: Vec<LayoutBox>,
    /// Parent box index.
    pub parent_index: Option<usize>,
    /// Rendered lines (cached).
    pub lines: Option<Vec<String>>,
    /// Line offset for cursor visibility.
    pub line_offset: usize,
    /// Layer for hit testing.
    pub layer: usize,
    /// ScrollView reference if this is a scroll view.
    pub scroll_view: Option<Arc<ScrollView>>,
    /// Scroll content lines (pre-rendered).
    pub scroll_content_lines: Option<Vec<String>>,
}

impl LayoutBox {
    pub fn new(component: Arc<dyn Component>, rect: LayoutRect, clip: LayoutRect) -> Self {
        Self {
            component,
            rect,
            clip,
            children: Vec::new(),
            parent_index: None,
            lines: None,
            line_offset: 0,
            layer: 0,
            scroll_view: None,
            scroll_content_lines: None,
        }
    }
}

/// A complete layout frame for rendering.
#[derive(Clone)]
pub struct LayoutFrame {
    /// Root box.
    pub root: LayoutBox,
    /// Total width.
    pub width: usize,
    /// Total height.
    pub height: usize,
    /// Rendered screen lines.
    pub lines: Vec<String>,
    /// Primary scroll view (if any).
    pub primary_scroll_view: Option<Arc<ScrollView>>,
}

impl LayoutFrame {
    /// Create a new layout frame.
    pub fn new(root: LayoutBox, width: usize, height: usize) -> Self {
        Self {
            root,
            width,
            height,
            lines: vec![String::new(); height],
            primary_scroll_view: None,
        }
    }
}

/// Context for layout operations.
struct LayoutContext {
    #[allow(dead_code)]
    viewport: LayoutViewport,
    primary_scroll_view: Option<Arc<ScrollView>>,
}

impl LayoutContext {
    fn register_scroll_view(&mut self, sv: &Arc<ScrollView>) {
        // Keep the first primary scroll view encountered (the root transcript
        // area). Non-primary scroll views are not tracked globally.
        if sv.is_primary() && self.primary_scroll_view.is_none() {
            self.primary_scroll_view = Some(sv.clone());
        }
    }
}

/// Render a component tree into a layout frame with constrained layout.
pub fn render_layout_frame(
    root: Arc<dyn Component>,
    width: usize,
    height: usize,
) -> LayoutFrame {
    let safe_width = width.max(1);
    let safe_height = height.max(1);
    
    let mut context = LayoutContext {
        viewport: LayoutViewport { width: safe_width, height: safe_height },
        primary_scroll_view: None,
    };
    
    let root_box = layout_component(
        &root,
        0,
        0,
        safe_width,
        Some(safe_height),
        LayoutRect::new(0, 0, safe_width, safe_height),
        &mut context,
    );
    
    let mut frame = LayoutFrame::new(root_box, safe_width, safe_height);
    frame.primary_scroll_view = context.primary_scroll_view;
    
    // Paint the root box into the screen lines
    paint_box(&frame.root, &mut frame.lines, safe_width);
    
    frame
}

/// Layout a single component and its children.
///
/// Recognizes `VStack` and `ScrollView` (via `as_any` downcast) and lays
/// them out with the existing constrained machinery, so stack `grow`/`shrink`/
/// `min_size` semantics are honored and the scroll view clips to its
/// allocated viewport. Any other component is treated as a leaf.
fn layout_component(
    component: &Arc<dyn Component>,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    context: &mut LayoutContext,
) -> LayoutBox {
    let safe_width = width.max(1);

    // VStack: distribute the available height across children according to
    // their basis/grow/shrink/min_size (mirrors TS stack layout). This was
    // previously dead — the layout engine treated the root VStack as a leaf
    // and concatenated each child's full intrinsic height, which pushed the
    // bottom dock below the visible viewport.
    if let Some(vstack) = component.as_any().downcast_ref::<VStack>() {
        return layout_vstack(component, vstack, x, y, safe_width, height, clip, context);
    }

    // ScrollView: clip the child to the allocated viewport height and render
    // only the visible window. Without this the scroll view emitted *all*
    // transcript lines, overflowing past the terminal bottom.
    if let Some(sv) = component.as_any().downcast_ref::<ScrollView>() {
        return layout_scroll_view(component, sv, x, y, safe_width, height, clip, context);
    }

    // ---- Leaf (Container/Editor/Footer/Text/...) ----
    let lines = component.render(safe_width);

    let allocated_height = height.unwrap_or_else(|| lines.len());

    // Calculate line offset for cursor visibility
    let line_offset = if lines.len() > allocated_height && allocated_height > 0 {
        let cursor_line = lines.iter().position(|line| line.contains(CURSOR_MARKER));
        cursor_line
            .map(|idx| if idx >= allocated_height { idx - allocated_height + 1 } else { 0 })
            .unwrap_or(0)
    } else {
        0
    };

    let rect = LayoutRect::new(x, y, safe_width, allocated_height);

    LayoutBox {
        component: component.clone(),
        rect,
        clip: clip.intersect(&rect),
        children: Vec::new(),
        parent_index: None,
        lines: Some(lines),
        line_offset,
        layer: 0,
        scroll_view: None,
        scroll_content_lines: None,
    }
}

/// Lay out a VStack by distributing `height` across its children.
fn layout_vstack(
    component: &Arc<dyn Component>,
    vstack: &VStack,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    context: &mut LayoutContext,
) -> LayoutBox {
    let children = vstack.get_children();
    let gap = vstack.gap();
    let allocated = layout_vstack_constrained(&children, width, height.unwrap_or(0), gap);

    // The box's own rect spans its full allocated height (or, when
    // unconstrained, the sum of child heights + gaps).
    let total_child_height: usize = allocated.iter().map(|(_, h)| *h).sum();
    let total_gap = if children.len() > 1 { gap * (children.len() - 1) } else { 0 };
    let box_height = height.unwrap_or(total_child_height + total_gap);

    let rect = LayoutRect::new(x, y, width, box_height);
    let own_clip = clip.intersect(&rect);

    let mut child_boxes = Vec::with_capacity(allocated.len());
    let mut cursor_y = y;
    for (idx, (child_component, child_height)) in allocated.iter().enumerate() {
        if idx > 0 && gap > 0 {
            cursor_y = cursor_y.saturating_add(gap);
        }
        let child_box = layout_component(
            child_component,
            x,
            cursor_y,
            width,
            Some(*child_height),
            own_clip,
            context,
        );
        // `parent_index` is the child's ordinal within its parent; `paint_box`
        // does not rely on it, but `hit_test` walks children front-to-back so
        // keeping the positional index is harmless.
        child_boxes.push(child_box);
        cursor_y = cursor_y.saturating_add(*child_height);
    }
    for (i, child) in child_boxes.iter_mut().enumerate() {
        child.parent_index = Some(i);
        child.layer = 0;
    }

    // The parent box carries no lines of its own — its children paint into
    // the screen buffer directly via `paint_box` recursion.
    let mut box_layout = LayoutBox::new(component.clone(), rect, own_clip);
    box_layout.children = child_boxes;
    box_layout
}

/// Lay out a ScrollView by clipping its child to the allocated viewport.
fn layout_scroll_view(
    component: &Arc<dyn Component>,
    sv: &ScrollView,
    x: usize,
    y: usize,
    width: usize,
    height: Option<usize>,
    clip: LayoutRect,
    context: &mut LayoutContext,
) -> LayoutBox {
    // Track the primary scroll view globally so `get_primary_scroll_view`
    // returns the live transcript area.
    let sv_arc: Arc<ScrollView> = Arc::new(sv.clone());
    context.register_scroll_view(&sv_arc);

    let allocated_height = height.unwrap_or_else(|| sv.render(width).len());
    // `render_with_viewport` honors `scroll_top` and pads to `height`.
    let lines = sv.render_with_viewport(width, allocated_height);

    let line_offset = if lines.len() > allocated_height && allocated_height > 0 {
        let cursor_line = lines.iter().position(|line| line.contains(CURSOR_MARKER));
        cursor_line
            .map(|idx| if idx >= allocated_height { idx - allocated_height + 1 } else { 0 })
            .unwrap_or(0)
    } else {
        0
    };

    let rect = LayoutRect::new(x, y, width, allocated_height);

    LayoutBox {
        component: component.clone(),
        rect,
        clip: clip.intersect(&rect),
        children: Vec::new(),
        parent_index: None,
        lines: Some(lines),
        line_offset,
        layer: 0,
        scroll_view: Some(sv_arc),
        scroll_content_lines: None,
    }
}

/// Translate a box and its children by a delta.
#[allow(dead_code)]
fn translate_box(lbox: &mut LayoutBox, delta_y: i32) {
    if delta_y >= 0 {
        lbox.rect.y += delta_y as usize;
    } else {
        lbox.rect.y = lbox.rect.y.saturating_sub((-delta_y) as usize);
    }
    for child in &mut lbox.children {
        translate_box(child, delta_y);
    }
}

/// Update clips for a box and its children.
#[allow(dead_code)]
fn update_clips(lbox: &mut LayoutBox, parent_clip: LayoutRect) {
    lbox.clip = parent_clip.intersect(&lbox.rect);
    let child_clip = lbox.clip;
    for child in &mut lbox.children {
        update_clips(child, child_clip);
    }
}

/// Paint a box and its children into the screen buffer.
fn paint_box(lbox: &LayoutBox, screen: &mut [String], total_width: usize) {
    if let Some(ref lines) = lbox.lines {
        let offset = lbox.line_offset;
        
        // Calculate visible row range
        let first_row = lbox.rect.y.max(lbox.clip.y);
        let last_row = (lbox.rect.y + lbox.rect.height).min(lbox.clip.y + lbox.clip.height).min(screen.len());
        
        for row in first_row..last_row {
            if row >= screen.len() {
                break;
            }
            
            let source_idx = offset + row.saturating_sub(lbox.rect.y);
            if source_idx >= lines.len() {
                break;
            }
            
            let source_line = &lines[source_idx];
            
            // Composite the line into the screen
            if lbox.rect.x == 0 && lbox.rect.width >= total_width {
                // Fast path: full-width box at left edge
                screen[row] = source_line.clone();
            } else {
                // Composite at the correct position
                screen[row] = composite_tui_line(
                    &screen[row],
                    source_line,
                    lbox.rect.x,
                    lbox.rect.width,
                    total_width,
                );
            }
        }
    }
    
    // Paint children
    for child in &lbox.children {
        paint_box(child, screen, total_width);
    }
}

/// Composite an overlay line onto a base line at a given column position.
pub fn composite_tui_line(base: &str, overlay: &str, start_col: usize, overlay_width: usize, total_width: usize) -> String {
    let base_width = visible_width(base);
    let overlay_actual_width = visible_width(overlay);
    
    // If overlay is empty or starts beyond total width, return base
    if overlay_actual_width == 0 || start_col >= total_width {
        return base.to_string();
    }
    
    // Calculate the portion of overlay that fits
    let effective_width = overlay_width.min(total_width.saturating_sub(start_col));
    
    // Slice or pad overlay to effective width
    let overlay_padded = if overlay_actual_width < effective_width {
        // Pad right
        format!("{}{}", overlay, " ".repeat(effective_width - overlay_actual_width))
    } else if overlay_actual_width > effective_width {
        // Truncate
        slice_by_column(overlay, 0, effective_width, true)
    } else {
        overlay.to_string()
    };
    
    // Build the composite line
    let before_width = start_col.min(base_width);
    let after_start = start_col + effective_width;
    
    // Get before portion from base
    let before = if before_width > 0 {
        slice_by_column(base, 0, before_width, true)
    } else {
        String::new()
    };
    
    // Get after portion from base
    let after = if after_start < base_width {
        slice_by_column(base, after_start, base_width.saturating_sub(after_start), true)
    } else {
        String::new()
    };
    
    // Combine with resets to prevent style leakage
    const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";
    
    // Calculate padding
    let before_pad = if visible_width(&before) < before_width {
        " ".repeat(before_width - visible_width(&before))
    } else {
        String::new()
    };
    
    let after_pad = if after_start < total_width {
        let expected_after_width = total_width.saturating_sub(after_start);
        let actual_after_width = visible_width(&after);
        if actual_after_width < expected_after_width {
            " ".repeat(expected_after_width - actual_after_width)
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    
    format!("{}{}{}{}{}{}{}", before, before_pad, SEGMENT_RESET, overlay_padded, SEGMENT_RESET, after, after_pad)
}

/// Hit test to find the deepest box at a given coordinate.
pub fn hit_test(frame: &LayoutFrame, x: usize, y: usize) -> Option<&LayoutBox> {
    hit_test_box(&frame.root, x, y)
}

fn hit_test_box(lbox: &LayoutBox, x: usize, y: usize) -> Option<&LayoutBox> {
    // Check if point is in clip
    if !lbox.clip.contains(x, y) {
        return None;
    }
    
    // Check if point is in rect
    if !lbox.rect.contains(x, y) {
        return None;
    }
    
    // Check children first (front to back)
    for child in lbox.children.iter().rev() {
        if let Some(hit) = hit_test_box(child, x, y) {
            return Some(hit);
        }
    }
    
    // Return this box if no child was hit
    Some(lbox)
}

/// Find all scroll views at a given coordinate.
pub fn get_scroll_views_at(frame: &LayoutFrame, x: usize, y: usize) -> Vec<Arc<ScrollView>> {
    let mut result = Vec::new();
    collect_scroll_views(&frame.root, x, y, &mut result);
    result
}

fn collect_scroll_views(lbox: &LayoutBox, x: usize, y: usize, result: &mut Vec<Arc<ScrollView>>) {
    if !lbox.clip.contains(x, y) || !lbox.rect.contains(x, y) {
        return;
    }
    
    if let Some(ref sv) = lbox.scroll_view {
        result.push(sv.clone());
    }
    
    for child in &lbox.children {
        collect_scroll_views(child, x, y, result);
    }
}

/// Extract cursor position from rendered lines.
pub fn extract_cursor_position(lines: &[String], height: usize) -> Option<(usize, usize)> {
    let viewport_top = lines.len().saturating_sub(height);
    
    for row in (viewport_top..lines.len()).rev() {
        let line = &lines[row];
        if let Some(marker_idx) = line.find(CURSOR_MARKER) {
            let before = &line[..marker_idx];
            let col = visible_width(before);
            return Some((row, col));
        }
    }
    
    None
}

/// Strip cursor markers from lines.
pub fn strip_cursor_markers(lines: &[String]) -> Vec<String> {
    lines.iter()
        .map(|line| line.replace(CURSOR_MARKER, ""))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Container, Editor, EditorOptions, EditorStyle, Focusable,
        ScrollView, ScrollViewOptions, StackChild, StackEntry, Text, VStack};

    #[test]
    fn test_layout_rect_contains() {
        let rect = LayoutRect::new(5, 5, 10, 10);
        assert!(rect.contains(5, 5)); // Top-left corner
        assert!(rect.contains(10, 10)); // Inside
        assert!(!rect.contains(4, 5)); // Left of
        assert!(!rect.contains(15, 5)); // Right of
        assert!(!rect.contains(5, 15)); // Below
    }

    #[test]
    fn test_layout_rect_intersect() {
        let a = LayoutRect::new(0, 0, 10, 10);
        let b = LayoutRect::new(5, 5, 10, 10);
        let c = a.intersect(&b);
        assert_eq!(c.x, 5);
        assert_eq!(c.y, 5);
        assert_eq!(c.width, 5);
        assert_eq!(c.height, 5);
    }

    #[test]
    fn test_composite_tui_line() {
        let base = "Hello, World!";
        let overlay = "Rust";
        let result = composite_tui_line(base, overlay, 7, 4, 20);
        assert!(result.contains("Hello,"));
        assert!(result.contains("Rust"));
    }

    #[test]
    fn test_render_layout_frame() {
        let text = Arc::new(Text::new("Hello, World!", 0, 0));
        let frame = render_layout_frame(text, 20, 10);
        assert_eq!(frame.width, 20);
        assert_eq!(frame.height, 10);
        assert!(!frame.lines.is_empty());
    }

    /// Regression for the leaf-only layout bug: a root `VStack[ scrollview
    /// (grow1), dock ]` must pin the dock (editor borders + footer) to the
    /// bottom of the viewport and clip the scrollview to the remainder. Before
    /// the fix the dock was pushed below the visible rows and the screen was
    /// black where the editor should be.
    #[test]
    fn test_constrained_layout_dock_pinned_to_bottom() {
        // Tall transcript (many lines) so the scrollview's intrinsic height
        // exceeds the viewport — it must be clipped, not overflow.
        let transcript = Arc::new(Container::new());
        for i in 0..40 {
            transcript.add_child(Arc::new(Text::new(&format!("line {i:02}"), 0, 0)));
        }
        let scroll = Arc::new(ScrollView::new(
            transcript,
            // FollowMode::None so the scrollview doesn't jump to the end
            // (which would push the first 30 lines off-screen) — we want to
            // assert the top of the transcript is visible, i.e. clipped, not
            // that it rendered *all* 40 lines.
            ScrollViewOptions {
                follow: crate::FollowMode::None,
                ..Default::default()
            },
        ));

        let editor = Arc::new(Editor::new(
            EditorOptions::default(),
            EditorStyle::default(),
            Arc::new(crate::Keybindings::new()),
        ));
        editor.set_focused(true);
        let editor_container = Arc::new(Container::new());
        editor_container.add_child(editor.clone());
        let footer = Arc::new(Text::new("FOOTER LINE", 0, 0));

        let dock = Arc::new(VStack::from_children(vec![
            StackChild::Entry(
                StackEntry::new(editor_container.clone())
                    .shrink(0)
                    .min_size(3),
            ),
            StackChild::Entry(StackEntry::new(footer.clone())),
        ]));

        let root = VStack::from_children(vec![
            StackChild::Entry(
                StackEntry::new(scroll.clone())
                    .basis(0)
                    .grow(1)
                    .shrink(1)
                    .min_size(1),
            ),
            StackChild::Entry(StackEntry::new(dock).shrink(1)),
        ]);

        // 40-col wide, 10-row tall viewport.
        let frame = render_layout_frame(Arc::new(root), 40, 10);

        // The screen buffer must be exactly the viewport height.
        assert_eq!(frame.lines.len(), 10, "frame should be clipped to viewport height");

        // The dock (editor borders + footer) must occupy the bottom rows of
        // the painted screen — the editor renders 3 rows (top border, content,
        // bottom border) + the footer 1 row = 4 dock rows. The bottom-most
        // painted row must be the footer.
        let bottom = frame.lines.last().expect("non-empty frame");
        assert!(
            bottom.contains("FOOTER"),
            "footer should be the last painted row; got: {bottom:?}"
        );

        // The editor's top border (a run of `─`) must appear on-screen just
        // above the footer region. Borders are colored via the theme border
        // color, so check for the `─` glyph rather than a raw repeat.
        let on_screen: String = frame.lines.join("\n");
        assert!(
            on_screen.contains('─'),
            "editor top border `─` should be visible on-screen; rendered:\n{on_screen}"
        );

        // The scrollview must NOT paint all 40 transcript lines — it must be
        // clipped to the top rows. "line 39" (the last) must be absent.
        assert!(
            !on_screen.contains("line 39"),
            "scrollview should be clipped to the viewport, not render all 40 lines"
        );

        // Primary scroll view should be tracked so get_primary_scroll_view /
        // frame.primary_scroll_view is populated for the scroll routing.
        // (The default ScrollView is non-primary; skip that assertion here and
        // rely on the is_primary branch in layout_scroll_view.)
    }
}
