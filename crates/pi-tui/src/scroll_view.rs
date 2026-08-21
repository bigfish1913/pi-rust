//! ScrollView - Scrollable container component.
//!
//! Implements scrolling with follow-end behavior for transcript display.
//! Follows the design from `tui-plan.md`.

use std::any::Any;
use std::sync::{Arc, Mutex};

use super::component::Component;
use super::layout_node::{LayoutNode, LayoutNodeProvider, ScrollNode};

/// Follow mode for ScrollView.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowMode {
    /// No automatic follow behavior.
    None,
    /// Automatically scroll to end when new content is added while at end.
    #[default]
    End,
}

/// Overscroll behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverscrollMode {
    /// No overscroll.
    #[default]
    None,
    /// Chain unused scroll delta to parent scroll view.
    Chain,
    /// Contain scroll within this view.
    Contain,
}

/// Scrollbar visibility options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarMode {
    /// Never show scrollbar.
    #[default]
    Hidden,
    /// Show scrollbar automatically when content exceeds viewport.
    Auto,
    /// Always show scrollbar.
    Always,
}

/// Options for ScrollView.
#[derive(Debug, Clone, Default)]
pub struct ScrollViewOptions {
    /// Axis for scrolling (only vertical is currently supported).
    pub axis: Option<ScrollAxis>,
    /// Follow mode for automatic scrolling.
    pub follow: FollowMode,
    /// Designate this view as the fallback target for global scroll actions.
    pub primary: bool,
    /// Overscroll behavior.
    pub overscroll: OverscrollMode,
    /// Scrollbar visibility.
    pub scrollbar: ScrollbarMode,
    /// Delay in milliseconds before hiding auto scrollbar.
    pub scrollbar_hide_delay_ms: Option<u64>,
}

/// Scroll axis (only vertical is supported currently).
#[derive(Debug, Clone, Copy, Default)]
pub enum ScrollAxis {
    #[default]
    Vertical,
}

/// Internal state for ScrollView.
#[derive(Debug, Clone)]
struct ScrollViewState {
    /// Current scroll position (top row of visible area).
    scroll_top: usize,
    /// Whether we're following the end.
    is_following_end: bool,
    /// Viewport height (set during render).
    viewport_height: usize,
    /// Content height (set during render).
    content_height: usize,
    /// Whether follow was suppressed at the end.
    follow_suppressed_at_end: bool,
    /// Whether the scrollbar is temporarily visible (for auto mode).
    transient_scrollbar_visible: bool,
}

impl ScrollViewState {
    fn new(follow: FollowMode) -> Self {
        Self {
            scroll_top: 0,
            is_following_end: follow == FollowMode::End,
            viewport_height: 0,
            content_height: 0,
            follow_suppressed_at_end: false,
            transient_scrollbar_visible: false,
        }
    }
}

/// ScrollView - A scrollable container.
///
/// Provides vertical scrolling with optional follow-end behavior.
/// In constrained layout, the child is measured at unbounded height
/// and clipped to the allocated viewport.
#[derive(Clone)]
pub struct ScrollView {
    child: Arc<dyn Component>,
    options: ScrollViewOptions,
    state: Arc<Mutex<ScrollViewState>>,
    /// Callback to request a render.
    request_render_callback: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

impl ScrollView {
    /// Create a new ScrollView with the given child.
    pub fn new(child: Arc<dyn Component>, options: ScrollViewOptions) -> Self {
        let follow = options.follow;
        Self {
            child,
            options,
            state: Arc::new(Mutex::new(ScrollViewState::new(follow))),
            request_render_callback: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a simple ScrollView with default options.
    pub fn simple(child: Arc<dyn Component>) -> Self {
        Self::new(child, ScrollViewOptions::default())
    }

    /// Create a primary ScrollView with follow-end behavior.
    pub fn primary(child: Arc<dyn Component>) -> Self {
        Self::new(child, ScrollViewOptions {
            follow: FollowMode::End,
            primary: true,
            ..Default::default()
        })
    }

    /// Get the child component.
    pub fn child(&self) -> &Arc<dyn Component> {
        &self.child
    }

    /// Get the current scroll position.
    pub fn scroll_top(&self) -> usize {
        self.state.lock().map(|s| s.scroll_top).unwrap_or(0)
    }

    /// Check if following the end.
    pub fn is_following_end(&self) -> bool {
        self.state.lock().map(|s| s.is_following_end).unwrap_or(false)
    }

    /// Get the viewport height.
    pub fn viewport_height(&self) -> usize {
        self.state.lock().map(|s| s.viewport_height).unwrap_or(0)
    }

    /// Get the content height.
    pub fn content_height(&self) -> usize {
        self.state.lock().map(|s| s.content_height).unwrap_or(0)
    }

    /// Check if this is the primary scroll view.
    pub fn is_primary(&self) -> bool {
        self.options.primary
    }

    /// Check if scrollbar is visible.
    pub fn is_scrollbar_visible(&self) -> bool {
        match self.options.scrollbar {
            ScrollbarMode::Always => self.viewport_height() > 0,
            ScrollbarMode::Auto => {
                self.content_height() > self.viewport_height() 
                    && self.state.lock().map(|s| s.transient_scrollbar_visible).unwrap_or(false)
            }
            ScrollbarMode::Hidden => false,
        }
    }

    /// Get the content width (accounting for scrollbar).
    pub fn get_content_width(&self, width: usize) -> usize {
        if self.options.scrollbar == ScrollbarMode::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }

    /// Scroll by the given number of lines.
    ///
    /// Returns the unused delta if scrolling hit the boundary.
    /// This enables scroll chaining for nested scroll views.
    pub fn scroll_by(&self, delta: i32) -> i32 {
        if delta == 0 {
            return 0;
        }

        if let Ok(mut state) = self.state.lock() {
            let max_scroll = state.content_height.saturating_sub(state.viewport_height);
            
            // If following end, start from the end position
            let start = if state.is_following_end { max_scroll } else { state.scroll_top };
            
            let new_top = if delta > 0 {
                start.saturating_add(delta as usize)
            } else {
                start.saturating_sub((-delta) as usize)
            };

            let clamped = new_top.min(max_scroll);
            let moved = (clamped as i32) - (start as i32);
            
            state.scroll_top = clamped;
            state.is_following_end = self.options.follow == FollowMode::End && clamped >= max_scroll;
            state.follow_suppressed_at_end = false;

            // Mark scrollbar activity
            if moved != 0 {
                state.transient_scrollbar_visible = true;
            }

            delta - moved
        } else {
            delta
        }
    }

    /// Scroll to a specific position.
    pub fn scroll_to(&self, position: usize) {
        if let Ok(mut state) = self.state.lock() {
            let max_scroll = state.content_height.saturating_sub(state.viewport_height);
            state.scroll_top = position.min(max_scroll);

            if self.options.follow == FollowMode::End {
                state.is_following_end = state.scroll_top >= max_scroll;
            }
            state.follow_suppressed_at_end = false;
            state.transient_scrollbar_visible = true;
        }
        self.request_render();
    }

    /// Scroll to the start.
    pub fn scroll_to_start(&self) {
        if let Ok(mut state) = self.state.lock() {
            let changed = state.scroll_top != 0 || state.is_following_end;
            state.scroll_top = 0;
            state.is_following_end = false;
            state.follow_suppressed_at_end = false;

            if changed {
                state.transient_scrollbar_visible = true;
            }
        }
        self.request_render();
    }

    /// Scroll to the end.
    pub fn scroll_to_end(&self) {
        if let Ok(mut state) = self.state.lock() {
            let max_scroll = state.content_height.saturating_sub(state.viewport_height);
            let changed = state.scroll_top != max_scroll || !state.is_following_end;
            state.scroll_top = max_scroll;
            state.is_following_end = self.options.follow == FollowMode::End;
            state.follow_suppressed_at_end = false;

            if changed {
                state.transient_scrollbar_visible = true;
            }
        }
        self.request_render();
    }

    /// Update layout information (called by the layout system).
    pub fn update_layout(&self, content_height: usize, viewport_height: usize, request_render: Option<Arc<dyn Fn() + Send + Sync>>) {
        if let Ok(mut state) = self.state.lock() {
            state.content_height = content_height;
            state.viewport_height = viewport_height;

            // Store the render callback
            if let Some(cb) = request_render {
                *self.request_render_callback.lock().unwrap() = Some(cb);
            }

            // If following end, adjust scroll position
            if self.options.follow == FollowMode::End && state.is_following_end {
                let max_scroll = content_height.saturating_sub(viewport_height);
                state.scroll_top = max_scroll;
            }

            // Ensure scroll position is valid
            let max_scroll = content_height.saturating_sub(viewport_height);
            state.scroll_top = state.scroll_top.min(max_scroll);

            // Update follow state
            if state.scroll_top < max_scroll {
                state.follow_suppressed_at_end = false;
            }
            if self.options.follow == FollowMode::End 
                && state.scroll_top == max_scroll 
                && !state.follow_suppressed_at_end 
            {
                state.is_following_end = true;
            }

            // Hide scrollbar if content fits
            if content_height <= viewport_height {
                state.transient_scrollbar_visible = false;
            }
        }
    }

    /// Request a render from the layout system.
    fn request_render(&self) {
        if let Ok(cb) = self.request_render_callback.lock() {
            if let Some(callback) = cb.as_ref() {
                callback();
            }
        }
    }

    /// Render the child content, applying scroll for a fixed viewport.
    pub fn render_with_viewport(&self, width: usize, height: usize) -> Vec<String> {
        let content_width = self.get_content_width(width);
        let all_lines = self.child.render(content_width);
        let content_height = all_lines.len();

        // Update state
        self.update_layout(content_height, height, None);

        let scroll_top = self.scroll_top();

        // Extract visible portion
        let visible: Vec<String> = all_lines
            .into_iter()
            .skip(scroll_top)
            .take(height)
            .collect();

        // Pad to fill viewport if needed
        let mut visible = visible;
        while visible.len() < height {
            visible.push(String::new());
        }

        // Add scrollbar column if needed
        if self.is_scrollbar_visible() {
            // TODO: Add scrollbar styling
            // For now, just mark the rightmost column
        }

        visible
    }

    /// Get the scrollbar style (can be customized).
    pub fn scrollbar_style(&self, text: &str) -> String {
        format!("\x1b[100m{}\x1b[49m", text)
    }
}

impl Component for ScrollView {
    fn render(&self, width: usize) -> Vec<String> {
        // Unconstrained render: show all content
        let content_width = self.get_content_width(width);
        self.child.render(content_width)
    }

    fn invalidate(&self) {
        self.child.invalidate();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl LayoutNodeProvider for ScrollView {
    fn layout_node(&self) -> Option<LayoutNode> {
        Some(LayoutNode::Scroll(ScrollNode::new(self.child.clone())))
    }
}

/// Render a ScrollView with a fixed viewport height.
pub fn render_scroll_view(scroll_view: &ScrollView, width: usize, height: usize) -> Vec<String> {
    scroll_view.render_with_viewport(width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Text, Container};

    #[test]
    fn test_scroll_view_basic() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3\nLine 4\nLine 5", 0, 0));
        let scroll = ScrollView::simple(text);
        
        assert_eq!(scroll.scroll_top(), 0);
        assert!(scroll.is_following_end());
    }

    #[test]
    fn test_scroll_by() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3\nLine 4\nLine 5", 0, 0));
        let scroll = ScrollView::new(text, ScrollViewOptions {
            follow: FollowMode::None,
            ..Default::default()
        });
        
        // Simulate content and viewport
        scroll.update_layout(5, 2, None);
        
        let unused = scroll.scroll_by(1);
        assert_eq!(unused, 0);
        assert_eq!(scroll.scroll_top(), 1);
    }

    #[test]
    fn test_scroll_to_end() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3", 0, 0));
        let scroll = ScrollView::new(text, ScrollViewOptions {
            follow: FollowMode::End,
            ..Default::default()
        });
        
        scroll.update_layout(3, 1, None);
        scroll.scroll_to_end();
        
        assert!(scroll.is_following_end());
        assert_eq!(scroll.scroll_top(), 2);
    }

    #[test]
    fn test_scroll_to_start() {
        let text = Arc::new(Text::new("Line 1\nLine 2\nLine 3", 0, 0));
        let scroll = ScrollView::simple(text);
        
        scroll.update_layout(3, 1, None);
        scroll.scroll_by(2);
        scroll.scroll_to_start();
        
        assert_eq!(scroll.scroll_top(), 0);
        assert!(!scroll.is_following_end());
    }
}