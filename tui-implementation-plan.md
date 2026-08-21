# TUI Implementation Plan for pi-rust

## Overview

This document outlines the plan to implement a Terminal User Interface (TUI) for the pi-rust project, following the design from `.reference/pi/tui-plan.md` and the TypeScript implementation in `.reference/pi/packages/tui/`.

## Architecture

### New Crate: `pi-tui`

Create a new crate `crates/pi-tui` with the following modules:

```
crates/pi-tui/
├── Cargo.toml
└── src/
    ├── lib.rs           # Public API exports
    ├── component.rs     # Component trait and base types
    ├── container.rs     # Container component
    ├── text.rs          # Text component
    ├── spacer.rs        # Spacer component
    ├── vstack.rs        # Vertical stack layout
    ├── hstack.rs        # Horizontal stack layout
    ├── scroll_view.rs   # Scrollable view
    ├── terminal.rs      # Terminal abstraction
    ├── render.rs        # Rendering utilities
    ├── ansi.rs          # ANSI escape code utilities
    ├── tui.rs           # Base TUI trait
    ├── tui_main_screen.rs   # Main screen (scrollback mode)
    ├── tui_alt_screen.rs    # Alternate screen (fullscreen mode)
    └── keybindings.rs   # Keybinding system
```

## Implementation Phases

### Phase 1: Core Components (MVP)

1. **Component Trait**
   ```rust
   pub trait Component: Send + Sync {
       fn render(&self, width: usize) -> Vec<String>;
       fn invalidate(&mut self);
       fn handle_input(&mut self, data: &str) -> bool { false }
   }
   ```

2. **Basic Components**
   - `Text` - Simple text rendering with padding
   - `Spacer` - Empty space
   - `Container` - Component container

3. **Terminal Abstraction**
   - `Terminal` trait for terminal operations
   - `ProcessTerminal` implementation using crossterm

4. **Basic TUI**
   - `TuiBase` - Common TUI functionality
   - `TuiMainScreen` - Simple scrollback mode

### Phase 2: Layout System

1. **VStack** - Vertical stack layout
   - Support for `basis`, `grow`, `shrink`, `minSize`, `maxSize`
   - Gap between children
   - Alignment options

2. **HStack** - Horizontal stack layout
   - ANSI-aware column slicing
   - Style isolation between children

3. **ScrollView** - Scrollable container
   - `follow: "end"` behavior
   - Primary scroll view concept
   - Keyboard and mouse wheel scrolling

### Phase 3: Full TUI Features

1. **TuiAltScreen** - Full-screen mode
   - Alternate screen buffer
   - Fixed bottom dock layout
   - Mouse support (wheel, selection)
   - Search functionality

2. **Interactive Mode Integration**
   - Update `pi-cli` to use the new TUI
   - Editor component
   - Message rendering
   - Tool execution display

### Phase 4: Polish

1. **Image Support** (optional)
   - Kitty image protocol
   - iTerm2 image support

2. **Performance Optimizations**
   - Differential rendering
   - Render caching

## Dependencies

Add to `Cargo.toml`:

```toml
[dependencies]
crossterm = { version = "0.27", features = ["events"] }
unicode-width = "0.1"
unicode-segmentation = "1"
```

## API Design

### Basic Usage

```rust
use pi_tui::{TuiMainScreen, Container, Text, Spacer, Terminal, ProcessTerminal};

fn main() -> Result<()> {
    let terminal = ProcessTerminal::new();
    let mut tui = TuiMainScreen::new(terminal, true, None);
    
    let container = Container::new();
    container.add_child(Text::new("Hello, TUI!", 1, 0));
    container.add_child(Spacer::new(1));
    
    tui.add_child(container);
    tui.start();
    
    // Main loop...
    
    tui.stop();
    Ok(())
}
```

### Full-screen Mode with Layout

```rust
use pi_tui::{TuiAltScreen, VStack, ScrollView, Container};

fn create_layout() -> VStack {
    let transcript = Container::new();
    // Add messages to transcript...
    
    let scroll_view = ScrollView::new(transcript, ScrollViewOptions {
        follow: FollowMode::End,
        primary: true,
    });
    
    let dock = Container::new();
    // Add editor, footer to dock...
    
    VStack::new(vec![
        StackEntry::new(scroll_view).grow(1).min_size(1),
        StackEntry::new(dock).auto(),
    ])
}
```

## Implementation Order

1. Create `pi-tui` crate skeleton
2. Implement `Terminal` and `ProcessTerminal`
3. Implement `Component` trait and basic components
4. Implement `TuiMainScreen`
5. Implement layout components (VStack, HStack)
6. Implement `ScrollView`
7. Implement `TuiAltScreen`
8. Update `pi-cli` interactive mode
9. Add advanced features (mouse, search, images)

## Testing Strategy

- Unit tests for layout algorithms
- Integration tests with virtual terminal
- Manual testing in tmux (as described in tui-plan.md)