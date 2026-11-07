//! Test-only source-order acceptance for TUI dialog attachment admission.
//!
//! The check is synthetic: it protects the startup fail-closed branch without
//! opening a terminal or exercising a real provider/plugin.

const HOST_SOURCE: &str = include_str!("../../interactive_tui.rs");

#[test]
fn rejected_dialog_attachment_stops_tui_startup_before_main_screen_setup() {
    let source = HOST_SOURCE;
    let attach = source
        .find("if !ask_user_bridge.attach()")
        .expect("TUI startup must inspect the dialog attachment result");
    let main_screen = source
        .find("tui.set_main_screen_mode(matches!(args.tui_mode")
        .expect("TUI startup main-screen setup must remain present");
    let return_code = source[attach..main_screen]
        .find("return 1;")
        .expect("rejected dialog attachment must fail closed with exit code 1");
    assert!(attach < main_screen);
    assert!(attach + return_code < main_screen);
    assert!(source[attach..main_screen].contains("refusing to start the TUI"));
}
