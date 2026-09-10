# rpi v0.1.9

`rpi v0.1.9` makes the active agent environment visible as soon as the
interactive TUI starts.

## Highlights

- The welcome screen now lists every active tool, including tools registered
  by runtime extensions.
- Discovered skills are shown alongside tools, so the capabilities available
  to the current session are clear before the first prompt.
- Empty skill sets are represented explicitly as `Skills (0) none`.
- Crate metadata now points to the correct GitHub repository.

## Install

```bash
cargo install rpi-cli --version 0.1.9
```
