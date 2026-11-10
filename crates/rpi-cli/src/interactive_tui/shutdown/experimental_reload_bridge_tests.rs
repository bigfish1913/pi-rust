//! Runtime component acceptance for the experimental v2 reload bridge.
//!
//! These tests exercise only the in-process mailbox/channel seam. They do not
//! start the terminal UI, load an extension, or invoke a provider.

use super::TuiMessage;
use rpi_extensions::ReloadMailbox;
use tokio::sync::mpsc::{self, UnboundedSender};

fn spawn_reload_bridge(
    mailbox: &ReloadMailbox,
    tui_tx: UnboundedSender<TuiMessage>,
) -> tokio::task::JoinHandle<()> {
    let (reload_sig_tx, mut reload_sig_rx) = mpsc::unbounded_channel::<()>();
    mailbox.install(reload_sig_tx);
    tokio::spawn(async move {
        while reload_sig_rx.recv().await.is_some() {
            if tui_tx.send(TuiMessage::ReloadExtensions).is_err() {
                break;
            }
        }
    })
}

#[tokio::test(flavor = "current_thread")]
async fn clear_closes_reload_bridge_and_rejects_late_signal() {
    let mailbox = ReloadMailbox::new();
    let (tui_tx, mut tui_rx) = mpsc::unbounded_channel();
    let bridge = spawn_reload_bridge(&mailbox, tui_tx);

    assert_eq!(mailbox.signal(), Ok(()));
    assert!(matches!(
        tui_rx.recv().await,
        Some(TuiMessage::ReloadExtensions)
    ));

    mailbox.clear();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), bridge).await,
        Ok(Ok(()))
    ));
    assert_eq!(mailbox.signal(), Err(()));
    assert!(tui_rx.try_recv().is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_exits_when_main_loop_receiver_is_gone() {
    let mailbox = ReloadMailbox::new();
    let (tui_tx, tui_rx) = mpsc::unbounded_channel();
    drop(tui_rx);
    let bridge = spawn_reload_bridge(&mailbox, tui_tx);

    // ReloadMailbox reports that a TUI bridge is installed; the bridge itself
    // observes the main-loop send failure and retires without a leaked task.
    assert_eq!(mailbox.signal(), Ok(()));
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), bridge).await,
        Ok(Ok(()))
    ));
    mailbox.clear();
    assert_eq!(mailbox.signal(), Err(()));
}
