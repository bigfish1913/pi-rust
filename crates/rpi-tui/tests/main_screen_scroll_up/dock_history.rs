//! iTerm2 saves the live screen to scrollback on ED(2). A regular-mode
//! repaint must not archive the editor/status dock along with the transcript.
use super::*;

fn iterm_terminal() -> VirtualTerminal {
    let term = VirtualTerminal::new(80, 12);
    term.state.lock().unwrap().save_screen_on_display_erase = true;
    term
}

#[test]
fn display_erase_can_archive_the_screen_while_line_erase_does_not() {
    let term = iterm_terminal();
    term.feed("DRAFT-MARKER\r\nFOOTER-MARKER");
    term.feed("\x1b[2J");
    let top = term.state.lock().unwrap().viewport_top;
    assert_eq!(top, term.rows());
    assert!(term.document_row(0).contains("DRAFT-MARKER"));
    assert!(term.document_row(1).contains("FOOTER-MARKER"));
    term.feed("\x1b[Hnew-live-row\x1b[2K");
    assert_eq!(term.state.lock().unwrap().viewport_top, top);
    assert!(term.visible()[0].trim().is_empty());
}

fn exercise_repaint(trigger: &str) {
    let term = iterm_terminal();
    let tui = TuiAltScreen::new(Box::new(term.clone()), true, None);
    tui.set_main_screen_mode(true);
    let root = Arc::new(Container::new());
    root.add_child(Arc::new(Text::new(
        (0..80)
            .map(|i| format!("old-message-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
        0,
        0,
    )));
    let transcript = Arc::new(Text::new(
        (0..40)
            .map(|i| format!("tool-output-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
        0,
        0,
    ));
    root.add_child(transcript.clone());
    let editor = Arc::new(rpi_tui::Editor::simple());
    editor.set_text("DRAFT-MARKER");
    editor.set_focused(true);
    root.add_child(editor);
    root.add_child(Arc::new(Text::new("FOOTER-MARKER", 0, 0)));
    tui.set_layout_root(Some(root.clone()));
    tui.start_readerless();
    term.browse_history_at(10);
    let browsed = term.visible();
    let saved = {
        let st = term.state.lock().unwrap();
        st.document[..st.viewport_top].to_vec()
    };
    for iteration in 0..3 {
        match trigger {
            "force" => tui.render_now(true),
            "resize" => tui.note_resize(),
            "shrink" => {
                transcript.set_text(
                    &(0..3 - iteration)
                        .map(|i| format!("tool-output-{i:03}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                tui.request_render(false);
            }
            _ => unreachable!(),
        }
        let st = term.state.lock().unwrap();
        assert_eq!(&st.document[..saved.len()], saved.as_slice());
        assert_eq!(
            st.viewport_top,
            saved.len(),
            "{trigger} must not archive the live screen"
        );
        assert_eq!(st.scrollback_erase_count, 0);
        assert_eq!(st.history_view_top, Some(10));
        drop(st);
        assert_eq!(term.visible(), browsed);
    }

    // Continue with real growth so the repaired physical row ledger is exercised.
    transcript.set_text(
        &(0..100)
            .map(|i| format!("tool-output-{i:03}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    tui.request_render(false);
    let st = term.state.lock().unwrap();
    let rows: Vec<_> = st
        .document
        .iter()
        .map(|row| row.iter().collect::<String>())
        .collect();
    for marker in ["DRAFT-MARKER", "FOOTER-MARKER"] {
        assert_eq!(
            rows.iter().filter(|row| row.contains(marker)).count(),
            1,
            "{trigger}: {marker} was archived/duplicated"
        );
        assert!(
            !rows[..st.viewport_top]
                .iter()
                .any(|row| row.contains(marker)),
            "{trigger}: dock entered saved history"
        );
    }
    // Shrinking preserves old immutable snapshots; every newly appended row
    // beyond the old output range must still be published once.
    for i in if trigger == "shrink" { 40 } else { 0 }..100 {
        let marker = format!("tool-output-{i:03}");
        assert_eq!(
            rows.iter().filter(|row| row.contains(&marker)).count(),
            1,
            "{trigger}: missing/duplicate {marker}"
        );
    }
    drop(st);
    let expected = rpi_tui::render::with_main_screen_frame(|| root.render(term.columns()));
    let expected = &expected[expected.len() - term.rows()..];
    let st = term.state.lock().unwrap();
    for (index, row) in expected.iter().enumerate() {
        let actual = st.document[st.viewport_top + index]
            .iter()
            .collect::<String>();
        assert_eq!(actual.trim_end(), rpi_tui::strip_ansi(row).trim_end());
    }
}

#[test]
fn forced_repaints_keep_the_input_dock_out_of_iterm_history() {
    exercise_repaint("force");
}

#[test]
fn resize_repaints_keep_the_input_dock_out_of_iterm_history() {
    exercise_repaint("resize");
}

#[test]
fn shrinking_output_keeps_the_input_dock_out_of_iterm_history() {
    exercise_repaint("shrink");
}
