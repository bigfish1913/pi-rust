# demo/rpi.tape — record the interactive TUI.
#
#   vhs demo/rpi.tape        # -> docs/images/rpi-demo.gif
#
# Unlike demo/sdk.tape, this one is NOT reproducible: an interactive session is
# only worth watching with a real model, so it uses whatever provider is
# configured (~/.rpi/agent/models.json, or ANTHROPIC_API_KEY / OPENAI_API_KEY).
# That is why the README leads with the offline SDK GIF and keeps this as the
# "here is the actual terminal" shot — pair it with the still
# (docs/images/rpi-interactive.png) rather than replacing it.
#
# Before recording: build and install so the first frame is not a compile, and
# pick a prompt whose output is stable enough to read in ~15 seconds.
#
#   cargo build --release -p rpi-cli && cargo install --path crates/pi-cli --force
#
# Then run with a prompt that shows the parts worth showing: a tool call, the
# approval/streaming view, and the resumed session. Keep it under ~20s — a GIF
# past that is a video, and a video nobody watches to the end.

Output docs/images/rpi-demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1100
Set Height 620
Set Padding 20
Set MarginFill "#101113"
Set BorderRadius 8
Set TypingSpeed 45ms
Set CursorBlink false

Sleep 700ms
Type "rpi"
Enter
Sleep 2500ms

# A prompt that exercises a read + a shell tool, so the transcript shows the
# panel layout (arguments, elapsed time, streaming output) rather than a chat.
Type "read the first 20 lines of Cargo.toml and tell me the workspace version"
Enter
Sleep 12000ms

# Show that the session survived: quit, then resume it.
Type "exit"
Enter
Sleep 1500ms
Type "rpi -c"
Enter
Sleep 2500ms
