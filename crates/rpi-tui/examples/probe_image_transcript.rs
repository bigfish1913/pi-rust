//! Probe: cost of an idle repaint of a transcript holding large screenshots.
//!
//!   cargo run --release -p rpi-tui --example probe_image_transcript -- 38 2.4
//!
//! Drives a real `TuiAltScreen` with a transcript of `images` images of `mb`
//! megabytes each and prints what an idle repaint costs. Two document shapes
//! are compared:
//!
//! * `payload rows` — every image row carries its PNG payload. This is what an
//!   image row looked like on *every* frame before the renderer could place an
//!   image it had already sent, and it is what a repaint used to pay for.
//! * `Image rows` — the real [`Image`] component, which emits the payload once
//!   and a placement afterwards.
//!
//! Both modes are measured: `regular` (main-screen, the default on macOS and
//! the mode the 100%-CPU report came from) and `fullscreen` (alternate screen).
use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rpi_tui::component::Component;
use rpi_tui::image::{Image, ImageOptions};
use rpi_tui::scroll_view::{ScrollView, ScrollViewOptions};
use rpi_tui::terminal::{Terminal, TerminalInfo};
use rpi_tui::tui_alt_screen::TuiAltScreen;
use rpi_tui::vstack::{StackChild, StackEntry};
use rpi_tui::{
    get_capabilities, set_capabilities, Container, HStack, ImageProtocol, TerminalCapabilities,
    Text, VStack, TUI,
};

/// A PNG header with `width` x `height` and `payload` bytes of filler behind it.
///
/// Only the header is read (it gives the placement its row count); the filler
/// stands in for the base64 a screenshot read puts in the transcript.
fn png_with_payload(width: u32, height: u32, payload: usize) -> Vec<u8> {
    let mut png = vec![0u8; 24];
    png[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    png[16..20].copy_from_slice(&width.to_be_bytes());
    png[20..24].copy_from_slice(&height.to_be_bytes());
    png.resize(payload, 0x5a);
    png
}

fn kitty_capabilities() -> TerminalCapabilities {
    let saved = get_capabilities();
    let mut caps = saved.clone();
    caps.protocol = ImageProtocol::Kitty;
    caps.cell_width = 10;
    caps.cell_height = 20;
    set_capabilities(caps);
    saved
}

/// Counts the bytes the renderer writes.
#[derive(Clone, Default)]
struct CountingTerminal {
    bytes: Arc<AtomicUsize>,
    transmits: Arc<AtomicUsize>,
}

impl Terminal for CountingTerminal {
    fn info(&self) -> TerminalInfo {
        TerminalInfo {
            columns: 120,
            rows: 50,
            ..Default::default()
        }
    }
    fn write(&self, data: &str) {
        self.bytes.fetch_add(data.len(), Ordering::Relaxed);
        self.transmits
            .fetch_add(data.matches("a=T").count(), Ordering::Relaxed);
    }
    fn hide_cursor(&self) {}
    fn show_cursor(&self) {}
    fn move_cursor(&self, _row: usize, _col: usize) {}
    fn clear_screen(&self) {}
    fn set_title(&self, _title: &str) {}
    fn enable_mouse(&self) {}
    fn disable_mouse(&self) {}
    fn enter_raw_mode(&self) {}
    fn exit_raw_mode(&self) {}
    fn refresh_size(&self) {}
    fn start(
        &self,
        _on_input: Box<dyn Fn(rpi_tui::InputEvent) + Send + Sync>,
        _on_resize: Box<dyn Fn() + Send + Sync>,
    ) {
    }
    fn stop(&self) {}
    fn is_tty(&self) -> bool {
        true
    }
    fn set_progress(&self, _active: bool) {}
    fn flush(&self) {}
}

/// A transcript whose image rows carry their payload inline (the shape an image
/// row had on every frame before the renderer tracked what it had uploaded).
struct PayloadRows(Arc<Vec<String>>);

impl Component for PayloadRows {
    fn render(&self, _width: usize) -> Vec<String> {
        self.0.as_ref().clone()
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn transcript(images: usize, mb: f64, payload_rows: bool) -> Arc<Container> {
    let payload = (mb * 1024.0 * 1024.0) as usize;
    let chat = Arc::new(Container::new());
    if payload_rows {
        let fill = "A".repeat(payload);
        let mut rows: Vec<String> = Vec::new();
        for i in 0..images {
            rows.push(format!("● read screenshot-{i}.png"));
            rows.push(format!(
                "\x1b_Ga=T,f=100,C=1,q=2,i={},p=1,c=60,r=12,m=0;{fill}\x1b\\",
                1_000 + i
            ));
            for _ in 1..12 {
                rows.push(String::new());
            }
        }
        chat.add_child(Arc::new(PayloadRows(Arc::new(rows))));
    } else {
        for i in 0..images {
            chat.add_child(Arc::new(Text::new(
                format!("● read screenshot-{i}.png"),
                0,
                0,
            )));
            chat.add_child(Arc::new(Image::from_data(
                png_with_payload(1600, 900, payload),
                ImageOptions {
                    width: Some(60),
                    alt_text: Some("[Attached image]".into()),
                    ..Default::default()
                },
            )));
        }
    }
    chat
}

/// The CLI's layout root: a transcript scroll view in a stack above a dock.
///
/// The probe has to go through the same root the app builds, because the path a
/// frame takes depends on it: a `ScrollView` renders its child as a document
/// (sharing rows), while a bare `Container` root flattens the whole transcript
/// into owned strings on every frame.
fn layout_root(transcript: Arc<Container>) -> Arc<dyn Component> {
    let scroll = Arc::new(ScrollView::new(
        transcript,
        ScrollViewOptions {
            follow: rpi_tui::FollowMode::End,
            primary: true,
            ..Default::default()
        },
    ));
    let transcript_stack = Arc::new(HStack::from_entries(vec![StackEntry::new(scroll)
        .basis(0)
        .grow(1)
        .shrink(1)
        .min_size(1)]));
    let dock = Arc::new(VStack::from_children(vec![StackChild::Entry(
        StackEntry::new(Arc::new(Text::new("│ dock", 0, 0))),
    )]));
    Arc::new(VStack::from_children(vec![
        StackChild::Entry(
            StackEntry::new(transcript_stack)
                .basis(0)
                .grow(1)
                .shrink(1)
                .min_size(1),
        ),
        StackChild::Entry(StackEntry::new(dock).shrink(1)),
    ]))
}

fn measure(label: &str, transcript: Arc<Container>, main_screen: bool, frames: usize) {
    let term = CountingTerminal::default();
    let tui = TuiAltScreen::new(Box::new(term.clone()), false, None);
    tui.set_main_screen_mode(main_screen);
    tui.set_layout_root(Some(layout_root(transcript)));

    let first = Instant::now();
    tui.start_readerless();
    let first_frame = first.elapsed();
    let first_written = term.bytes.swap(0, Ordering::Relaxed);

    // The frame that turns payload rows into placement rows: the terminal holds
    // the pixels, so the document shrinks to the placement. One-time.
    tui.request_render(false);
    let transition = term.bytes.swap(0, Ordering::Relaxed);

    // Steady state: nothing changed, the session is idle.
    let mut steady = Duration::ZERO;
    for _ in 0..frames {
        let start = Instant::now();
        tui.request_render(false);
        steady += start.elapsed();
    }
    let idle = steady / frames as u32;
    let written = term.bytes.swap(0, Ordering::Relaxed) / frames;
    println!(
        "{label:<14} first frame {:>7.1} ms / {:>6} MB | transition {:>5} KB | idle repaint {:>8.3} ms | idle writes {:>6} B",
        first_frame.as_secs_f64() * 1000.0,
        first_written / (1024 * 1024),
        transition / 1024,
        idle.as_secs_f64() * 1000.0,
        written
    );
    tui.stop(rpi_tui::TuiStopOptions::default());
}

fn main() {
    let images: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(38);
    let mb: f64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.4);
    let frames = 10;

    let saved_caps = kitty_capabilities();
    println!("images={images} payload={mb} MB each  (each idle repaint of the transcript)");
    for (mode, main_screen) in [("regular/main-screen", true), ("fullscreen", false)] {
        println!("{mode}:");
        measure(
            "  payload rows",
            transcript(images, mb, true),
            main_screen,
            frames,
        );
        measure(
            "  Image rows",
            transcript(images, mb, false),
            main_screen,
            frames,
        );
    }
    set_capabilities(saved_caps);
}
