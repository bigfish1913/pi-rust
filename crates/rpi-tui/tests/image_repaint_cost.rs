//! An idle repaint must not re-send the image payloads a transcript holds.
//!
//! Reported symptom: with a session whose transcript held ~38 `read`-returned
//! screenshots (~82 MB of base64), the render thread stayed at 100% of a core
//! while the session was completely idle — no streaming, no input, no terminal
//! output. Every repaint re-rendered the transcript's image rows in full.
//!
//! A Kitty payload row is megabytes; the terminal only needs it once. These
//! probes pin the two halves of that rule on the regular (main-screen) renderer,
//! which is the default on macOS and therefore the reported path:
//!
//! 1. the frame that first shows an image carries the payload, and
//! 2. every later frame places the image by id — kilobytes, not megabytes,
//!    and an unchanged primary document then writes nothing at all.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rpi_tui::image::{Image, ImageOptions};
use rpi_tui::terminal::{Terminal, TerminalInfo};
use rpi_tui::tui_alt_screen::TuiAltScreen;
use rpi_tui::{
    get_capabilities, set_capabilities, Container, ImageProtocol, TerminalCapabilities, Text, TUI,
};

static LOCK: Mutex<()> = Mutex::new(());

struct Restore(TerminalCapabilities);
impl Drop for Restore {
    fn drop(&mut self) {
        set_capabilities(self.0.clone());
    }
}

fn kitty_capabilities() -> Restore {
    let saved = get_capabilities();
    let mut caps = saved.clone();
    caps.protocol = ImageProtocol::Kitty;
    caps.cell_width = 10;
    caps.cell_height = 20;
    set_capabilities(caps);
    Restore(saved)
}

/// A PNG header with the given pixel size and `payload` bytes behind it.
///
/// Only the header is decoded (it gives the placement its row count); the rest
/// stands in for the base64 payload a screenshot read puts in the transcript.
fn png_with_payload(width: u32, height: u32, payload: usize) -> Vec<u8> {
    let mut png = vec![0u8; 24];
    png[..8].copy_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    png[16..20].copy_from_slice(&width.to_be_bytes());
    png[20..24].copy_from_slice(&height.to_be_bytes());
    png.resize(payload, 0x5a);
    png
}

/// Records everything the renderer writes, so a probe can assert on both the
/// bytes that reached the terminal and the size of a frame.
#[derive(Clone)]
struct RecordingTerminal {
    written_bytes: Arc<AtomicUsize>,
    log: Arc<Mutex<String>>,
}

impl RecordingTerminal {
    fn new() -> Self {
        Self {
            written_bytes: Arc::new(AtomicUsize::new(0)),
            log: Arc::new(Mutex::new(String::new())),
        }
    }

    /// Bytes and text written since the last call.
    fn take(&self) -> (usize, String) {
        let bytes = self.written_bytes.swap(0, Ordering::SeqCst);
        let text = std::mem::take(&mut *self.log.lock().unwrap());
        (bytes, text)
    }
}

impl Terminal for RecordingTerminal {
    fn info(&self) -> TerminalInfo {
        TerminalInfo {
            columns: 80,
            rows: 12,
            ..Default::default()
        }
    }

    fn write(&self, data: &str) {
        self.written_bytes.fetch_add(data.len(), Ordering::SeqCst);
        self.log.lock().unwrap().push_str(data);
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

/// A main-screen TUI whose transcript holds one large screenshot.
fn screenshot_transcript(payload: usize) -> Arc<Container> {
    let transcript = Arc::new(Container::new());
    transcript.add_child(Arc::new(Text::new("● read screenshot.png", 0, 0)));
    transcript.add_child(Arc::new(Image::from_data(
        png_with_payload(400, 200, payload),
        ImageOptions {
            width: Some(20),
            alt_text: Some("[Attached image]".into()),
            ..Default::default()
        },
    )));
    transcript.add_child(Arc::new(Text::new("…and the next message", 0, 0)));
    transcript
}

#[test]
fn an_idle_main_screen_repaint_places_the_image_instead_of_resending_it() {
    let _g = LOCK.lock().unwrap();
    let _caps = kitty_capabilities();
    let payload = 256 * 1024;

    let term = RecordingTerminal::new();
    let tui = TuiAltScreen::new(Box::new(term.clone()), false, None);
    tui.set_main_screen_mode(true);
    tui.set_layout_root(Some(screenshot_transcript(payload)));
    tui.start_readerless();

    // The first frame has to hand the terminal the pixels.
    let (first_bytes, first) = term.take();
    assert!(
        first.contains("a=T"),
        "the first frame must transmit the payload"
    );
    assert!(
        first_bytes > payload,
        "the first frame carries the image: {first_bytes} bytes"
    );

    // An idle repaint: the same document, so the payload is already in the
    // terminal and only the placement has to be painted.
    tui.request_render(false);
    let (repaint_bytes, repaint) = term.take();
    assert!(
        repaint.contains("a=p"),
        "an idle repaint must place the image it already sent: {repaint:?}"
    );
    assert!(
        !repaint.contains("a=T"),
        "an idle repaint must not re-transmit the payload"
    );
    assert!(
        repaint_bytes < 4096,
        "an idle repaint is kilobytes, not megabytes: {repaint_bytes} bytes"
    );

    // And once both the document and the terminal agree, an idle repaint costs
    // no terminal output at all. This also pins the ledger: if the placement
    // row were forgotten, the next frame would re-render the payload and this
    // frame would write megabytes again.
    for frame in 0..3 {
        tui.request_render(false);
        let (bytes, text) = term.take();
        assert_eq!(
            bytes,
            0,
            "repaint {frame} of an unchanged screen must write nothing, got: {:?}",
            &text[..text.len().min(200)]
        );
        assert!(!text.contains("a=T"), "the payload must not come back");
    }
}

#[test]
fn an_uploaded_screenshot_costs_the_same_as_a_short_row() {
    let _g = LOCK.lock().unwrap();
    let _caps = kitty_capabilities();

    // Same transcript, two payload sizes. Once the image is known to the
    // terminal, a repaint must not scale with the screenshot's size — that
    // scaling is what a 38-image session paid on every single frame.
    let small = single_repaint_bytes(64 * 1024);
    let large = single_repaint_bytes(1024 * 1024);
    assert!(
        large <= small + 256,
        "repaint cost must not follow the payload: {small} bytes for 64 KiB, {large} for 1 MiB"
    );
}

/// Bytes an idle repaint writes after the payload is already on screen.
fn single_repaint_bytes(payload: usize) -> usize {
    let term = RecordingTerminal::new();
    let tui = TuiAltScreen::new(Box::new(term.clone()), false, None);
    tui.set_main_screen_mode(true);
    tui.set_layout_root(Some(screenshot_transcript(payload)));
    tui.start_readerless();
    let _ = term.take();
    // The transition frame (payload rows become placement rows).
    tui.request_render(false);
    let _ = term.take();
    // A steady-state repaint.
    tui.request_render(false);
    term.take().0
}
