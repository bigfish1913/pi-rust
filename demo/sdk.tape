# demo/sdk.tape — record the offline, deterministic SDK demo.
#
#   vhs demo/sdk.tape        # -> docs/images/rpi-sdk-demo.gif
#
# Everything shown is the real output of real code driven by the `faux` provider:
# no API key, no network, same bytes every run. That is the whole point of the
# demo — it is the claim in the README ("no network needed to test the loop"),
# shown rather than asserted.
#
# Pre-build first so the recording is not dominated by compiling:
#
#   cargo build -p minimal -p tools-example
#
# Recording it from scratch in CI: `scoop install vhs ttyd` (Windows) or
# `brew install vhs` (macOS); ffmpeg is the other dependency.

Output docs/images/rpi-sdk-demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1080
Set Height 480
Set Padding 20
Set MarginFill "#101113"
Set BorderRadius 8
Set TypingSpeed 45ms
Set CursorBlink false

Sleep 500ms

Type "# The agent loop, with no API key and no network:"
Enter
Sleep 400ms
Type "cargo run -q -p minimal"
Enter
Sleep 2500ms

Type "# Now the same loop driving real fs + shell tools:"
Enter
Sleep 400ms
Type "cargo run -q -p tools-example"
Enter
Sleep 3500ms
