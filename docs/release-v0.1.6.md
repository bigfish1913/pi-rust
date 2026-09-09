# rpi v0.1.6

rpi v0.1.6 is a Rust-native implementation of the Pi agent SDK, with a library-first multi-crate workspace and a terminal coding-agent CLI.

## Highlights

- Async, streaming agent runtime
- Anthropic Messages, OpenAI-compatible Chat Completions, and faux provider
- Built-in `read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls` coding tools
- JSONL sessions, context compaction, hooks, queues, and prompt templates
- `rpi-plugin-sdk` and stable ABI extension points

## Install

```bash
cargo install rpi-cli
rpi -p "hello"
```

## Links

- Documentation: https://rpi.laofu.online/docs.html
- Architecture: https://github.com/bigfish1913/pi-rust/blob/main/docs/architecture.md
- Website: https://rpi.laofu.online/

MIT License.
