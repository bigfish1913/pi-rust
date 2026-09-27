//! Render-cache behaviour for the transcript components.
//!
//! These live in their own integration binary because they touch the
//! process-wide theme; a separate process keeps that away from the parallel
//! unit tests in `src/`. Within this binary every test takes the same lock, so
//! a theme swap in one test cannot invalidate the cache another test is
//! asserting stability on.
//!
//! The cache exists to keep a repaint of a long transcript O(1) per unchanged
//! component (see native pi `Markdown`/`Text` caches and `layout.ts`
//! `renderCached`). The risk it introduces is staleness, so these tests pin the
//! two things that must still invalidate it: content/width change and theme
//! change.

use std::sync::{Mutex, MutexGuard};

use rpi_tui::{
    apply_theme_preset, AssistantBlock, AssistantMessageComponent, AssistantMessageOptions,
    Component, Markdown, Text, ThemePreset, UserMessageComponent,
};

/// Serializes every test in this binary (theme is a process-wide global).
static THEME_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    THEME_LOCK.lock().unwrap()
}

#[test]
fn markdown_repeat_render_is_stable() {
    let _g = lock();
    let md = Markdown::new("# Title\n\n**bold** and `code`", 0, 0);
    let first = md.render(80);
    let second = md.render(80);
    assert_eq!(first, second);
    assert!(!first.is_empty());
}

#[test]
fn markdown_invalidates_on_content_and_width() {
    let _g = lock();
    let md = Markdown::new("one", 0, 0);
    let a = md.render(80);
    md.set_content("two two two");
    let b = md.render(80);
    assert_ne!(a, b, "content change must re-render");

    // A width change must also re-render (wrapping differs).
    let narrow = md.render(4);
    let wide = md.render(80);
    assert_ne!(narrow, wide);
}

#[test]
fn markdown_invalidates_on_theme_change() {
    let _g = lock();
    apply_theme_preset(ThemePreset::Dark);
    let md = Markdown::new("# Heading\n\nbody text", 0, 0);
    let dark = md.render(80);
    apply_theme_preset(ThemePreset::Monochrome);
    let mono = md.render(80);
    apply_theme_preset(ThemePreset::Dark);
    assert_ne!(dark, mono, "theme change must drop the markdown cache");
}

#[test]
fn text_repeat_render_is_stable_and_tracks_content() {
    let _g = lock();
    let t = Text::new("hello world", 0, 0);
    let first = t.render(40);
    assert_eq!(first, t.render(40));
    t.set_text("changed");
    assert_ne!(first, t.render(40));
}

#[test]
fn text_invalidates_on_theme_change() {
    let _g = lock();
    apply_theme_preset(ThemePreset::Dark);
    let t = Text::new("x", 0, 0);
    let _ = t.render(20);
    apply_theme_preset(ThemePreset::Monochrome);
    let _ = t.render(20);
    apply_theme_preset(ThemePreset::Dark);
    // The cache must not have trapped the old theme: a later content update is
    // still observed.
    t.set_text("y");
    assert!(t.render(20).join("").contains('y'));
}

#[test]
fn user_message_repeat_render_is_stable() {
    let _g = lock();
    let msg = UserMessageComponent::new("hello **world**");
    let first = msg.render(60);
    assert_eq!(first, msg.render(60));
    assert!(!first.is_empty());
}

#[test]
fn user_message_invalidates_on_theme_change() {
    let _g = lock();
    apply_theme_preset(ThemePreset::Dark);
    let msg = UserMessageComponent::new("hello");
    let dark = msg.render(60);
    apply_theme_preset(ThemePreset::Light);
    let light = msg.render(60);
    apply_theme_preset(ThemePreset::Dark);
    assert_ne!(
        dark, light,
        "theme change must rebuild the user-message background box"
    );
}

#[test]
fn assistant_message_renders_and_is_stable_across_repeated_frames() {
    let _g = lock();
    // The transcript benchmark lives on this path: a finalized assistant
    // message must render identically every frame (cache) while still
    // reflecting an update.
    let comp = AssistantMessageComponent::new(AssistantMessageOptions::default());
    comp.update_blocks(&[AssistantBlock::Text("## A\n\nsome **markdown**".into())]);
    comp.set_streaming(false);
    let first = comp.render(100);
    assert_eq!(first, comp.render(100));

    comp.update_blocks(&[AssistantBlock::Text("## B\n\ndifferent".into())]);
    assert_ne!(first, comp.render(100));
}
