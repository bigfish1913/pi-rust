//! Terminal operations used by the shared interactive loop.

use super::*;
use rpi_tui::Terminal;
use std::io;
use std::time::Duration;

#[cfg(unix)]
use crate::local_host::HostTerminal;

#[derive(Clone)]
pub(crate) enum InteractiveTerminal {
    Process,
    #[cfg(unix)]
    Host(HostTerminal),
}

pub(super) enum TerminalInput {
    Event(Event),
    #[cfg(unix)]
    Editor(Result<String, String>),
}

impl InteractiveTerminal {
    pub(crate) fn is_host(&self) -> bool {
        match self {
            Self::Process => false,
            #[cfg(unix)]
            Self::Host(_) => true,
        }
    }

    pub(super) fn output(&self) -> Box<dyn Terminal> {
        match self {
            Self::Process => Box::new(ProcessTerminal::new()),
            #[cfg(unix)]
            Self::Host(terminal) => Box::new(terminal.clone()),
        }
    }

    pub(super) fn read(&self, wait: Duration) -> io::Result<Option<TerminalInput>> {
        match self {
            Self::Process => {
                if crossterm::event::poll(wait)? {
                    let event = normalize_decoded_event(crossterm::event::read()?);
                    Ok(Some(TerminalInput::Event(event)))
                } else {
                    Ok(None)
                }
            }
            #[cfg(unix)]
            Self::Host(terminal) => Ok(terminal.read_event(wait)?.map(|event| match event {
                crate::local_host::NativeInput::Event(event) => TerminalInput::Event(event),
                crate::local_host::NativeInput::Editor(result) => TerminalInput::Editor(result),
            })),
        }
    }

    pub(super) fn suspend(&self, tui: &Arc<TuiAltScreen>) {
        match self {
            Self::Process => handle_suspend(tui),
            #[cfg(unix)]
            Self::Host(terminal) => {
                tui.suspend();
                terminal.suspend_client();
                tui.resume();
            }
        }
    }

    pub(super) fn edit(
        &self,
        _tui: &Arc<TuiAltScreen>,
        draft: String,
        tx: &mpsc::UnboundedSender<TuiMessage>,
    ) {
        match self {
            Self::Process => launch_external_editor(draft, tx.clone()),
            #[cfg(unix)]
            Self::Host(terminal) => {
                _tui.suspend();
                terminal.edit(draft);
            }
        }
    }

    pub(super) fn exit_active(
        &self,
        tui: &Arc<TuiAltScreen>,
        _tx: &mpsc::UnboundedSender<TuiMessage>,
    ) {
        match self {
            Self::Process => emergency_exit(tui),
            #[cfg(unix)]
            Self::Host(terminal) => {
                terminal.force_close();
                let _ = _tx.send(TuiMessage::Exit);
            }
        }
    }

    pub(super) fn is_closing(&self) -> bool {
        match self {
            Self::Process => false,
            #[cfg(unix)]
            Self::Host(terminal) => terminal.is_closing(),
        }
    }

    pub(super) fn idle_timeout(&self) -> Option<Duration> {
        match self {
            Self::Process => None,
            #[cfg(unix)]
            Self::Host(terminal) => terminal.idle_timeout(),
        }
    }

    pub(crate) fn report(&self, line: &str, error: bool) {
        match self {
            Self::Process if error => eprintln!("{line}"),
            Self::Process => println!("{line}"),
            #[cfg(unix)]
            Self::Host(terminal) => {
                terminal.write(line);
                terminal.write("\n");
                terminal.flush();
            }
        }
    }
}
