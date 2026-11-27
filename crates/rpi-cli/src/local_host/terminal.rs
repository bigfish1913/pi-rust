//! Run the native TUI against a terminal owned by the attaching CLI process.

use std::io;
use std::sync::{mpsc as blocking, Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use crossterm::terminal::WindowSize;
use rpi_harness::agent_harness::{AgentHarness, AgentLane};
use rpi_tui::terminal_image::{ImageProtocol, TerminalCapabilities};
use rpi_tui::{InputEvent, ProcessTerminal, Terminal, TerminalInfo};
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::terminal_event::EventDef;
use super::{read_json, write_json};

const IO_INTERVAL: Duration = Duration::from_millis(10);
const FRAME_CHUNK_BYTES: usize = 64 * 1024;
const QUEUE_CAPACITY: usize = 128;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalRequest {
    pub regular: bool,
    pub theme: Option<String>,
    pub no_themes: bool,
    geometry: Geometry,
    images: ImageSupport,
}

impl TerminalRequest {
    pub fn new(args: &crate::args::Args) -> Self {
        let terminal = ProcessTerminal::new();
        Self {
            regular: args.tui_mode == crate::args::TuiMode::Regular,
            theme: args.theme.clone(),
            no_themes: args.no_themes,
            geometry: Geometry::read(&terminal),
            images: match rpi_tui::terminal_image::detect_capabilities().protocol {
                ImageProtocol::None => ImageSupport::None,
                ImageProtocol::Kitty => ImageSupport::Kitty,
                ImageProtocol::ITerm2 => ImageSupport::ITerm2,
            },
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
enum ImageSupport {
    None,
    Kitty,
    ITerm2,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
struct Geometry {
    columns: u16,
    rows: u16,
    width: u16,
    height: u16,
}

impl Geometry {
    fn read(terminal: &ProcessTerminal) -> Self {
        let info = terminal.info();
        let size = terminal.window_size();
        let size = size.as_ref();
        Self {
            columns: size.map_or(info.columns as u16, |size| size.columns).max(1),
            rows: size.map_or(info.rows as u16, |size| size.rows).max(1),
            width: size.map_or(0, |size| size.width),
            height: size.map_or(0, |size| size.height),
        }
    }
}

#[derive(Deserialize, Serialize)]
enum Input {
    Event(#[serde(with = "EventDef")] Event),
    Geometry(Geometry),
    Editor(Result<String, String>),
}

#[derive(Deserialize, Serialize)]
enum Output {
    Write(String),
    RawMode(bool),
    Suspend,
    Edit(String),
    Finished(i32),
}

#[derive(Clone)]
pub(crate) struct HostTerminal {
    shared: Arc<Shared>,
}

struct Shared {
    geometry: Mutex<Geometry>,
    output: mpsc::Sender<Output>,
    input: Mutex<blocking::Receiver<Input>>,
    buffer: Mutex<String>,
    disconnected: CancellationToken,
    forced: CancellationToken,
    idle_timeout: Option<Duration>,
    images: ImageSupport,
}

impl HostTerminal {
    pub(crate) fn read_event(&self, wait: Duration) -> io::Result<Option<NativeInput>> {
        if self.is_closing() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        match self.shared.input.lock().unwrap().recv_timeout(wait) {
            Ok(Input::Event(event)) => Ok(Some(NativeInput::Event(event))),
            Ok(Input::Geometry(geometry)) => Ok(Some(NativeInput::Event(Event::Resize(
                geometry.columns,
                geometry.rows,
            )))),
            Ok(Input::Editor(result)) => Ok(Some(NativeInput::Editor(result))),
            Err(blocking::RecvTimeoutError::Timeout) => Ok(None),
            Err(blocking::RecvTimeoutError::Disconnected) => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn send(&self, output: Output) {
        if self.shared.output.try_send(output).is_err() {
            self.shared.disconnected.cancel();
        }
    }

    pub(crate) fn suspend_client(&self) {
        self.flush();
        self.send(Output::Suspend);
    }

    pub(crate) fn edit(&self, draft: String) {
        self.flush();
        self.send(Output::Edit(draft));
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.shared.disconnected.is_cancelled() || self.shared.forced.is_cancelled()
    }

    pub(crate) fn force_close(&self) {
        self.shared.forced.cancel();
    }

    pub(crate) fn idle_timeout(&self) -> Option<Duration> {
        self.shared.idle_timeout
    }

    pub(crate) fn image_capabilities(&self) -> TerminalCapabilities {
        let geometry = *self.shared.geometry.lock().unwrap();
        let mut capabilities = TerminalCapabilities {
            columns: geometry.columns,
            rows: geometry.rows,
            protocol: match self.shared.images {
                ImageSupport::None => ImageProtocol::None,
                ImageSupport::Kitty => ImageProtocol::Kitty,
                ImageSupport::ITerm2 => ImageProtocol::ITerm2,
            },
            ..Default::default()
        };
        if let Some(cells) = rpi_tui::terminal_image::query_cell_dimensions(
            self.window_size(),
            Some((geometry.columns, geometry.rows)),
        ) {
            capabilities.cell_width = cells.width;
            capabilities.cell_height = cells.height;
        }
        capabilities
    }

    fn flush_buffer(&self, buffer: &mut String) {
        if !buffer.is_empty() {
            self.send(Output::Write(std::mem::take(buffer)));
        }
    }
}

pub(crate) enum NativeInput {
    Event(Event),
    Editor(Result<String, String>),
}

impl Terminal for HostTerminal {
    fn info(&self) -> TerminalInfo {
        let geometry = self.shared.geometry.lock().unwrap();
        TerminalInfo {
            columns: usize::from(geometry.columns),
            rows: usize::from(geometry.rows),
            supports_true_color: true,
            supports_mouse: true,
            supports_kitty_keyboard: false,
        }
    }

    fn write(&self, mut data: &str) {
        let mut buffer = self.shared.buffer.lock().unwrap();
        while buffer.len() + data.len() > FRAME_CHUNK_BYTES {
            let mut end = FRAME_CHUNK_BYTES - buffer.len();
            while !data.is_char_boundary(end) {
                end -= 1;
            }
            buffer.push_str(&data[..end]);
            self.flush_buffer(&mut buffer);
            data = &data[end..];
        }
        buffer.push_str(data);
    }

    fn hide_cursor(&self) {
        self.write("\x1b[?25l");
    }
    fn show_cursor(&self) {
        self.write("\x1b[?25h");
    }
    fn move_cursor(&self, row: usize, col: usize) {
        self.write(&format!("\x1b[{};{}H", row + 1, col + 1));
    }
    fn clear_screen(&self) {
        self.write("\x1b[2J\x1b[H");
    }
    fn set_title(&self, title: &str) {
        self.write(&format!("\x1b]2;{title}\x07"));
    }
    fn enable_mouse(&self) {
        self.write("\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    }
    fn disable_mouse(&self) {
        self.write("\x1b[?1006l\x1b[?1002l\x1b[?1000l");
    }
    fn enter_raw_mode(&self) {
        self.flush();
        self.send(Output::RawMode(true));
    }
    fn exit_raw_mode(&self) {
        self.flush();
        self.send(Output::RawMode(false));
    }
    fn refresh_size(&self) {}
    fn window_size(&self) -> Option<WindowSize> {
        let geometry = *self.shared.geometry.lock().unwrap();
        Some(WindowSize {
            columns: geometry.columns,
            rows: geometry.rows,
            width: geometry.width,
            height: geometry.height,
        })
    }
    fn start(&self, _: Box<dyn Fn(InputEvent) + Send + Sync>, _: Box<dyn Fn() + Send + Sync>) {
        self.enter_raw_mode();
    }
    fn stop(&self) {
        self.disable_mouse();
        self.show_cursor();
        self.exit_raw_mode();
    }
    fn is_tty(&self) -> bool {
        true
    }
    fn set_progress(&self, active: bool) {
        let state = u8::from(active);
        self.write(&format!("\x1b]9;4;{state};0\x07"));
        self.flush();
    }
    fn flush(&self) {
        self.flush_buffer(&mut self.shared.buffer.lock().unwrap());
    }
}

pub(super) async fn serve(
    harness: &AgentHarness,
    request: TerminalRequest,
    idle_timeout: Option<Duration>,
    (mut input, mut output): (BufReader<OwnedReadHalf>, OwnedWriteHalf),
    run: impl AsyncFnOnce(HostTerminal) -> i32,
) -> Result<(), String> {
    let (input_tx, input_rx) = blocking::sync_channel(QUEUE_CAPACITY);
    let (output_tx, mut output_rx) = mpsc::channel(QUEUE_CAPACITY);
    let disconnected = CancellationToken::new();
    let forced = CancellationToken::new();
    if request.geometry.columns == 0 || request.geometry.rows == 0 {
        return Err("terminal dimensions must be positive".into());
    }
    let terminal = HostTerminal {
        shared: Arc::new(Shared {
            geometry: Mutex::new(request.geometry),
            output: output_tx,
            input: Mutex::new(input_rx),
            buffer: Mutex::new(String::new()),
            disconnected: disconnected.clone(),
            forced: forced.clone(),
            idle_timeout,
            images: request.images,
        }),
    };
    let reader = tokio::spawn({
        let terminal = terminal.clone();
        async move {
            while let Ok(Some(message)) = read_json::<Input>(&mut input).await {
                if let Input::Geometry(geometry) = message {
                    if geometry.columns == 0 || geometry.rows == 0 {
                        break;
                    }
                    *terminal.shared.geometry.lock().unwrap() = geometry;
                }
                if input_tx.try_send(message).is_err() {
                    break;
                }
            }
            terminal.shared.disconnected.cancel();
        }
    });
    let writer = tokio::spawn({
        let disconnected = disconnected.clone();
        async move {
            while let Some(message) = output_rx.recv().await {
                let end = matches!(message, Output::Finished(_));
                if let Err(error) = write_json(&mut output, &message).await {
                    disconnected.cancel();
                    return Err(error);
                }
                if end {
                    break;
                }
            }
            Ok(())
        }
    });
    // Keep the TUI alive through normal shutdown and native session settlement.
    // A disconnected or forcibly closed terminal must also release an active turn.
    let stop = async {
        tokio::select! {
            _ = disconnected.cancelled() => {},
            _ = forced.cancelled() => {},
        }
        loop {
            let _ = harness.abort().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let display = Arc::new(rpi_tui::display_context::DisplayContext::new(
        terminal.image_capabilities(),
    ));
    let run = rpi_tui::display_context::scope(Some(display), run(terminal.clone()));
    tokio::pin!(run);
    let code = tokio::select! { code = &mut run => code, _ = stop => unreachable!() };
    terminal.flush();
    reader.abort();
    if !disconnected.is_cancelled() {
        let _ = terminal
            .shared
            .output
            .send(Output::Finished(if forced.is_cancelled() {
                130
            } else {
                code
            }))
            .await;
    } else {
        writer.abort();
    }
    match writer.await {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

struct ClientTerminal {
    terminal: ProcessTerminal,
    raw: bool,
    fullscreen: bool,
}

impl Drop for ClientTerminal {
    fn drop(&mut self) {
        self.terminal.set_progress(false);
        self.terminal.stop();
        if self.fullscreen {
            self.terminal.write("\x1b[?7h\x1b[?1049l");
            self.terminal.flush();
        }
    }
}

pub(super) async fn attach(
    mut input: BufReader<OwnedReadHalf>,
    output: OwnedWriteHalf,
) -> Result<i32, String> {
    let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
    let read = async move {
        loop {
            let message = read_json::<Output>(&mut input).await;
            let end = !matches!(message, Ok(Some(_)));
            if tx.send(message).await.is_err() || end {
                break;
            }
        }
        std::future::pending::<()>().await;
    };
    tokio::select! {
        result = attach_terminal(rx, output) => result,
        _ = read => unreachable!(),
    }
}

async fn attach_terminal(
    mut input: mpsc::Receiver<Result<Option<Output>, String>>,
    mut output: OwnedWriteHalf,
) -> Result<i32, String> {
    let mut tty = ClientTerminal {
        terminal: ProcessTerminal::new(),
        raw: false,
        fullscreen: false,
    };
    let mut geometry = Geometry::read(&tty.terminal);
    let mut timer = tokio::time::interval(IO_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_resize_check = Instant::now();
    loop {
        tokio::select! {
            message = input.recv() => match message.transpose()?.flatten() {
                Some(Output::Write(text)) => {
                    if let Some((_, rest)) = text.rsplit_once("\x1b[?1049h") {
                        tty.fullscreen = !rest.contains("\x1b[?1049l");
                    } else if text.contains("\x1b[?1049l") {
                        tty.fullscreen = false;
                    }
                    tty.terminal.write(&text);
                    tty.terminal.flush();
                }
                Some(Output::RawMode(raw)) => {
                    if raw { tty.terminal.enter_raw_mode(); } else { tty.terminal.exit_raw_mode(); }
                    tty.raw = raw;
                    if raw {
                        let current = Geometry::read(&tty.terminal);
                        if current != geometry {
                            write_json(&mut output, &Input::Geometry(current)).await?;
                            geometry = current;
                        }
                    }
                }
                Some(Output::Suspend) => {
                    let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP);
                }
                Some(Output::Edit(draft)) => {
                    let result = tokio::task::spawn_blocking(move || crate::interactive_tui::edit_draft(draft))
                        .await.map_err(|error| error.to_string())?;
                    write_json(&mut output, &Input::Editor(result)).await?;
                }
                Some(Output::Finished(code)) => return Ok(code),
                None => return Err("local Host closed the terminal connection".into()),
            },
            _ = timer.tick() => {
                if tty.raw && Instant::now() >= next_resize_check {
                    tty.terminal.refresh_size();
                    let current = Geometry::read(&tty.terminal);
                    if geometry != current {
                        write_json(&mut output, &Input::Geometry(current)).await?;
                        geometry = current;
                    }
                    next_resize_check = Instant::now() + Duration::from_millis(80);
                }
                if tty.raw {
                    for _ in 0..32 {
                        if !event::poll(Duration::ZERO).map_err(|error| error.to_string())? {
                            break;
                        }
                        let event = event::read().map_err(|error| error.to_string())?;
                        let event = crate::interactive_tui::normalize_decoded_event(event);
                        write_json(&mut output, &Input::Event(event)).await?;
                    }
                }
            }
        }
    }
}
