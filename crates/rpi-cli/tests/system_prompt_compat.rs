use rpi_cli::session::{
    default_system_prompt, default_system_prompt_with_tools, BUILTIN_TOOL_NAMES,
};

#[test]
fn published_two_argument_prompt_api_remains_callable() {
    // Type-check the API as a downstream crate, rather than through a private helper.
    let prompt: fn(&str, bool) -> String = default_system_prompt;
    for replayed in [false, true] {
        let text = prompt("/compat/project", replayed);
        assert!(text.contains("Current working directory: /compat/project"));
        for tool in BUILTIN_TOOL_NAMES {
            assert!(text.contains(&format!("- {tool}")));
        }
        assert_eq!(
            text.contains("Your own reasoning is carried back"),
            replayed
        );
        assert!(!text.contains("todo tool"));
    }
}

#[test]
fn selected_tool_prompt_keeps_restricted_runs_restricted() {
    let text = default_system_prompt_with_tools("/compat/project", true, &["read".to_owned()]);
    assert!(text.contains("- read"));
    for absent in [
        "- bash",
        "- edit",
        "- write",
        "- docs",
        "todo tool",
        "PowerShell cmdlets",
    ] {
        assert!(
            !text.contains(absent),
            "unavailable tool guidance: {absent}"
        );
    }
}
