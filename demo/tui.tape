# Record from the repository root after: cargo build -p rpi-cli --bin rpi --locked
# Uses an empty config and ephemeral session; never sends a model request.
# Capture the startup still from this recording with:
# ffmpeg -i docs/images/rpi-tui-demo.gif -ss 1 -frames:v 1 -update 1 docs/images/rpi-interactive.png
Output docs/images/rpi-tui-demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1100
Set Height 680
Set Padding 20
Set MarginFill "#101113"
Set BorderRadius 8
Set TypingSpeed 100ms
Set CursorBlink false

Hide
Type@1ms "bash demo/tui.sh"
Enter
Sleep 2s
Show
Sleep 2s
Type "/help"
Enter
Sleep 5s
