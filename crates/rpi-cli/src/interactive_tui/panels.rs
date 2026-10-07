//! Generic passive plugin panels. No plugin names or domain metrics live here.
use super::*;
use rpi_extensions::{ExtensionPanel, PanelAnchor, PanelLayout};
use rpi_tui::ansi::visible_width;
use rpi_tui::tui::OverlayHandle;
use rpi_tui::{OverlayAnchor, OverlayOptions};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Panel(Mutex<ExtensionPanel>);
fn clean(s: &str) -> String {
    rpi_tui::strip_ansi(s)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}
impl Component for Panel {
    fn render(&self, screen_width: usize) -> Vec<String> {
        let spec = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if screen_width < spec.min_screen_width {
            return vec![];
        }
        let width = screen_width.min(spec.width);
        if width < 4 {
            return vec![];
        }
        let colors = current_theme().colors;
        let inner = width.saturating_sub(if spec.border { 4 } else { 0 });
        let fit = |text: &str| rpi_tui::utils::truncate_to_width(&clean(text), inner, "");
        let mut content = Vec::new();
        if !spec.title.is_empty() {
            content.push(fit(&spec.title));
        }
        content.extend(spec.lines.iter().map(|row| fit(row)));
        let mut rows = Vec::new();
        if spec.border && spec.max_height >= 2 {
            rows.push(colors.dim.fg(&format!("┌{}┐", "─".repeat(width - 2))));
            for row in content.into_iter().take(spec.max_height - 2) {
                rows.push(colors.dim.fg(&format!(
                    "│ {row}{} │",
                    " ".repeat(inner.saturating_sub(visible_width(&row)))
                )));
            }
            rows.push(colors.dim.fg(&format!("└{}┘", "─".repeat(width - 2))));
        } else {
            rows.extend(
                content
                    .into_iter()
                    .take(spec.max_height)
                    .map(|row| colors.dim.fg(&row)),
            );
        }
        rows
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
fn options(spec: &ExtensionPanel) -> OverlayOptions {
    let anchor = match spec.anchor {
        PanelAnchor::TopLeft => OverlayAnchor::TopLeft,
        PanelAnchor::TopCenter => OverlayAnchor::TopCenter,
        PanelAnchor::TopRight => OverlayAnchor::TopRight,
        PanelAnchor::LeftCenter => OverlayAnchor::LeftCenter,
        PanelAnchor::Center => OverlayAnchor::Center,
        PanelAnchor::RightCenter => OverlayAnchor::RightCenter,
        PanelAnchor::BottomLeft => OverlayAnchor::BottomLeft,
        PanelAnchor::BottomCenter => OverlayAnchor::BottomCenter,
        PanelAnchor::BottomRight => OverlayAnchor::BottomRight,
    };
    OverlayOptions {
        anchor,
        offset_x: spec.offset_x,
        offset_y: spec.offset_y,
        width: Some(rpi_tui::tui::SizeValue::Absolute(spec.width)),
        max_height: Some(rpi_tui::tui::SizeValue::Absolute(spec.max_height)),
        non_capturing: true,
        ..Default::default()
    }
}
fn left_anchor(anchor: PanelAnchor) -> bool {
    matches!(
        anchor,
        PanelAnchor::TopLeft | PanelAnchor::LeftCenter | PanelAnchor::BottomLeft
    )
}
struct Rail {
    mailbox: rpi_extensions::ExtensionStatusMailbox,
    left: bool,
    height: AtomicUsize,
    screen_width: AtomicUsize,
}
impl Component for Rail {
    fn render(&self, width: usize) -> Vec<String> {
        if width < 6 {
            return vec![];
        }
        let screen_width = self.screen_width.load(Ordering::Relaxed);
        let mut specs = self
            .mailbox
            .panels()
            .into_values()
            .filter(|p| {
                p.layout == PanelLayout::Sidebar
                    && left_anchor(p.anchor) == self.left
                    && screen_width >= p.min_screen_width
            })
            .collect::<Vec<_>>();
        if specs.is_empty() {
            return vec![];
        }
        let anchor = specs[0].anchor;
        let offset = specs[0].offset_y;
        let height = self.height.load(Ordering::Relaxed).max(1);
        let single = specs.len() == 1;
        let mut rows = Vec::new();
        for spec in &mut specs {
            spec.min_screen_width = 0;
            if !rows.is_empty() {
                rows.push(String::new());
            }
            let panel = Panel(Mutex::new(spec.clone()));
            for row in panel.render(width - 2) {
                let padding = " ".repeat((width - 2).saturating_sub(visible_width(&row)));
                rows.push(if self.left {
                    format!("{row}{padding} │")
                } else {
                    format!("│ {row}{padding}")
                });
            }
        }
        rows.truncate(height);
        if single {
            let free = height.saturating_sub(rows.len());
            let start = match anchor {
                PanelAnchor::BottomLeft | PanelAnchor::BottomCenter | PanelAnchor::BottomRight => {
                    free
                }
                PanelAnchor::LeftCenter | PanelAnchor::Center | PanelAnchor::RightCenter => {
                    free / 2
                }
                _ => 0,
            };
            let start = (start as i64 + offset as i64).clamp(0, free as i64) as usize;
            rows.splice(0..0, std::iter::repeat_n(String::new(), start));
        }
        rows
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
struct Dock {
    stack: Arc<rpi_tui::HStack>,
    scroll: Arc<ScrollView>,
    left: Arc<Rail>,
    right: Arc<Rail>,
    sizes: Option<(usize, usize)>,
}
#[derive(Default)]
pub(super) struct Panels {
    entries: BTreeMap<String, (Arc<Panel>, Arc<dyn OverlayHandle>)>,
    dock: Option<Dock>,
    revision: Option<u64>,
    viewport: (usize, usize),
}
impl Panels {
    pub(super) fn with_sidebar(
        stack: Arc<rpi_tui::HStack>,
        scroll: Arc<ScrollView>,
        mailbox: rpi_extensions::ExtensionStatusMailbox,
    ) -> Self {
        let rail = |left| {
            Arc::new(Rail {
                mailbox: mailbox.clone(),
                left,
                height: AtomicUsize::new(1),
                screen_width: AtomicUsize::new(0),
            })
        };
        let mut panels = Self::default();
        panels.dock = Some(Dock {
            stack,
            scroll,
            left: rail(true),
            right: rail(false),
            sizes: None,
        });
        panels
    }
    pub(super) fn sync(
        &mut self,
        tui: &TuiAltScreen,
        mailbox: &rpi_extensions::ExtensionStatusMailbox,
    ) -> bool {
        let viewport = (
            tui.width(),
            self.dock
                .as_ref()
                .map(|dock| dock.scroll.viewport_height())
                .unwrap_or(0),
        );
        self.sync_viewport(tui, mailbox, viewport)
    }
    fn sync_viewport(
        &mut self,
        tui: &dyn TUI,
        mailbox: &rpi_extensions::ExtensionStatusMailbox,
        viewport: (usize, usize),
    ) -> bool {
        let revision = mailbox.revision();
        if self.revision == Some(revision) && self.viewport == viewport {
            return false;
        }
        self.revision = Some(revision);
        self.viewport = viewport;
        let mut specs = mailbox.panels();
        if let Some(dock) = &mut self.dock {
            let (mut left, mut right) = (0, 0);
            for spec in specs.values().filter(|spec| {
                spec.layout == PanelLayout::Sidebar && viewport.0 >= spec.min_screen_width
            }) {
                if left_anchor(spec.anchor) {
                    left = left.max(spec.width + 2);
                } else {
                    right = right.max(spec.width + 2);
                }
            }
            // Keep at least 48 columns for chat. Narrow screens use the original full-width layout.
            if viewport.0.saturating_sub(left + right) < 48 {
                left = 0;
                right = 0;
            }
            dock.left.height.store(viewport.1, Ordering::Relaxed);
            dock.right.height.store(viewport.1, Ordering::Relaxed);
            dock.left.screen_width.store(viewport.0, Ordering::Relaxed);
            dock.right.screen_width.store(viewport.0, Ordering::Relaxed);
            if dock.sizes != Some((left, right)) {
                dock.stack.clear();
                if left > 0 {
                    dock.stack.add_child_with_options(
                        dock.left.clone(),
                        rpi_tui::StackEntryOptions {
                            basis: Some(left),
                            shrink: 0,
                            ..Default::default()
                        },
                    );
                }
                dock.stack.add_child_with_options(
                    dock.scroll.clone(),
                    rpi_tui::StackEntryOptions {
                        basis: Some(0),
                        grow: 1,
                        shrink: 1,
                        min_size: 1,
                        ..Default::default()
                    },
                );
                if right > 0 {
                    dock.stack.add_child_with_options(
                        dock.right.clone(),
                        rpi_tui::StackEntryOptions {
                            basis: Some(right),
                            shrink: 0,
                            ..Default::default()
                        },
                    );
                }
                dock.sizes = Some((left, right));
            }
        }
        specs.retain(|_, spec| spec.layout == PanelLayout::Overlay);
        self.entries.retain(|key, (_, handle)| {
            if specs.contains_key(key) {
                true
            } else {
                handle.hide();
                false
            }
        });
        for (key, spec) in specs {
            if let Some((panel, handle)) = self.entries.get(&key) {
                let mut current = panel.0.lock().unwrap_or_else(|p| p.into_inner());
                if current.same_geometry(&spec) {
                    *current = spec;
                    continue;
                }
                handle.hide();
            }
            let opts = options(&spec);
            let panel = Arc::new(Panel(Mutex::new(spec)));
            let handle = tui.show_overlay(panel.clone(), Some(opts));
            self.entries.insert(key, (panel, handle));
        }
        true
    }
}
impl Drop for Panels {
    fn drop(&mut self) {
        for (_, handle) in self.entries.values() {
            handle.hide();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sidebar_reserves_space_preserves_scroll_and_editor_and_restores_layout_when_hidden() {
        let tui = TuiAltScreen::new(Box::new(ProcessTerminal::new()), false, None);
        let mailbox = rpi_extensions::ExtensionStatusMailbox::new();
        let scroll = Arc::new(ScrollView::new(
            Arc::new(Text::new("CHAT\n".repeat(50), 0, 0)),
            ScrollViewOptions {
                primary: true,
                follow: FollowMode::End,
                scrollbar: ScrollbarMode::Hidden,
                ..Default::default()
            },
        ));
        let stack = Arc::new(rpi_tui::HStack::from_entries(vec![StackEntry::new(
            scroll.clone(),
        )
        .basis(0)
        .grow(1)]));
        let root: Arc<dyn Component> = Arc::new(VStack::from_children(vec![
            StackChild::Entry(StackEntry::new(stack.clone()).basis(0).grow(1).min_size(1)),
            StackChild::Entry(
                StackEntry::new(Arc::new(Text::new("INPUT", 0, 0)))
                    .basis(3)
                    .shrink(0),
            ),
        ]));
        let mut panels = Panels::with_sidebar(stack, scroll.clone(), mailbox.clone());
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        let frame = rpi_tui::layout::render_layout_frame(root.clone(), 120, 24);
        assert_eq!(frame.root.children[0].children[0].rect.width, 120);
        mailbox.handle(serde_json::json!({"key":"monitor","panel":{"version":1,"layout":"sidebar","width":24,"minScreenWidth":80,"border":false,"lines":["TTFT 0.42 s","TPS 252 tok/s"]}})).unwrap();
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        let frame = rpi_tui::layout::render_layout_frame(root.clone(), 120, 24);
        assert_eq!(frame.primary_scroll_view.unwrap().viewport_height(),21);
        assert_eq!(scroll.viewport_height(),21);
        let chat = &frame.root.children[0].children[0];
        let rail = &frame.root.children[0].children[1];
        assert_eq!(chat.rect.width, 94);
        assert_eq!(chat.rect.height, 21);
        assert_eq!(rail.rect.x, 94);
        assert_eq!(rail.rect.width, 26);
        assert_eq!(frame.root.children[1].rect.width, 120);
        assert!(frame.lines[0].contains("TTFT 0.42 s"));
        assert!(frame.lines[21].contains("INPUT"));
        assert!(!tui.has_overlay());
        panels.sync_viewport(&tui, &mailbox, (64, 21));
        let frame = rpi_tui::layout::render_layout_frame(root.clone(), 64, 24);
        assert_eq!(frame.root.children[0].children.len(), 1);
        assert_eq!(frame.root.children[0].children[0].rect.width, 64);
        mailbox
            .handle(serde_json::json!({"key":"monitor","panel":null}))
            .unwrap();
        panels.sync_viewport(&tui, &mailbox, (120, 21));
        let frame = rpi_tui::layout::render_layout_frame(root, 120, 24);
        assert_eq!(frame.root.children[0].children[0].rect.width, 120);
    }
    #[test]
    fn multiple_panels_update_move_and_remove_without_stealing_editor_focus() {
        // Do not start the terminal: registry/focus operations need no IO.
        let tui = TuiAltScreen::new(Box::new(ProcessTerminal::new()), false, None);
        let editor: Arc<dyn Component> = Arc::new(Editor::simple());
        tui.set_focus(Some(editor.clone()));
        let mailbox = rpi_extensions::ExtensionStatusMailbox::new();
        let mut panels = Panels::default();
        mailbox.handle(serde_json::json!({"key":"a","panel":{"version":1,"anchor":"top-left","lines":["first"]}})).unwrap();
        mailbox.handle(serde_json::json!({"key":"b","panel":{"version":1,"anchor":"bottom-right","lines":["other"]}})).unwrap();
        panels.sync(&tui, &mailbox);
        assert_eq!(panels.entries.len(), 2);
        assert!(Arc::ptr_eq(&tui.get_focus().unwrap(), &editor));
        let original = panels.entries["a"].0.clone();
        mailbox.handle(serde_json::json!({"key":"a","panel":{"version":1,"anchor":"top-left","lines":["updated"]}})).unwrap();
        panels.sync(&tui, &mailbox);
        assert!(Arc::ptr_eq(&original, &panels.entries["a"].0));
        assert!(original.render(80).join("\n").contains("updated"));
        mailbox.handle(serde_json::json!({"key":"a","panel":{"version":1,"anchor":"bottom-left","offsetX":2,"lines":["moved"]}})).unwrap();
        panels.sync(&tui, &mailbox);
        assert!(!Arc::ptr_eq(&original, &panels.entries["a"].0));
        assert_eq!(
            panels.entries["a"].0 .0.lock().unwrap().anchor,
            PanelAnchor::BottomLeft
        );
        mailbox
            .handle(serde_json::json!({"key":"a","panel":null}))
            .unwrap();
        panels.sync(&tui, &mailbox);
        assert_eq!(panels.entries.len(), 1);
        assert!(tui.has_overlay());
        drop(panels);
        assert!(!tui.has_overlay());
        assert!(Arc::ptr_eq(&tui.get_focus().unwrap(), &editor));
    }
    #[test]
    fn arbitrary_plugin_content_is_sanitized_and_fits_unicode_and_height_limits() {
        let spec:ExtensionPanel=serde_json::from_value(serde_json::json!({"version":1,"width":44,"maxHeight":4,
            "title":"插件\u{1b}[31m标题\u{1b}[0m","lines":["1 轮 87 步 252 tok/s","中文宽度测试".repeat(30),"truncated"]})).unwrap();
        let panel = Panel(Mutex::new(spec));
        for width in [1, 4, 12, 30, 44, 80] {
            let rows = panel.render(width);
            assert!(rows.len() <= 4);
            assert!(rows.iter().all(|row| visible_width(row) <= width));
            assert!(!rows.join("\n").contains("truncated"));
        }
        assert!(panel.render(80).join("\n").contains("1 轮 87 步 252 tok/s"));
    }
}
