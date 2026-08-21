//! Terminal User Interface for pi-rust
//!
//! This crate provides a TUI system similar to the TypeScript implementation
//! in `packages/tui/`. It supports:
//! - Main screen mode (terminal scrollback)
//! - Alternate screen mode (fullscreen with constrained layout)
//! - Component-based UI with VStack, HStack, ScrollView
//! - Mouse and keyboard input handling

pub mod ansi;
pub mod component;
pub mod container;
pub mod editor;
pub mod hstack;
pub mod keybindings;
pub mod layout;
pub mod layout_node;
pub mod markdown;
pub mod overlay;
pub mod render;
pub mod scroll_view;
pub mod spacer;
pub mod terminal;
pub mod text;
pub mod tui;
pub mod tui_alt_screen;
pub mod tui_main_screen;
pub mod vstack;

// Re-export main types
pub use ansi::{strip_ansi, visible_width, CURSOR_MARKER};
pub use component::{Component, Focusable};
pub use container::Container;
pub use editor::{Editor, EditorOptions, EditorStyle};
pub use hstack::HStack;
pub use keybindings::{KeyCombo, Keybindings, KeybindingId};
pub use layout::{composite_tui_line, extract_cursor_position, hit_test, render_layout_frame, LayoutBox, LayoutFrame, LayoutRect};
pub use layout_node::{allocate_stack_sizes, LayoutNode, LayoutNodeProvider, LayoutViewport, StackLayoutEntry};
pub use markdown::{Markdown, MarkdownOptions};
pub use overlay::{Dialog, OverlayHandle, OverlayManager, Selector, SelectorItem};
pub use scroll_view::{FollowMode, ScrollView, ScrollViewOptions};
pub use spacer::Spacer;
pub use terminal::{ProcessTerminal, Terminal, TerminalError};
pub use text::Text;
pub use tui::{OverlayAnchor, OverlayHandle as TuiOverlayHandle, OverlayMargin, OverlayOptions, TuiMode, TUI};
pub use tui_alt_screen::{is_viewport_tui, TuiAltScreen, VIEWPORT_TUI};
pub use tui_main_screen::TuiMainScreen;
pub use vstack::{layout_vstack_constrained, StackAlign, StackChild, StackEntry, StackEntryOptions, StackOptions, VStack};