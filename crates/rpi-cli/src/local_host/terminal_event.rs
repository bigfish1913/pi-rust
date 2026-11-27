//! Crossterm event representations for the private local Host terminal transport.

use crossterm::event::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(remote = "Event")]
pub(super) enum EventDef {
    FocusGained,
    FocusLost,
    #[serde(with = "KeyEventDef")]
    Key(KeyEvent),
    #[serde(with = "MouseEventDef")]
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "MouseEvent")]
struct MouseEventDef {
    #[serde(with = "MouseEventKindDef")]
    pub kind: MouseEventKind,
    pub column: u16,
    pub row: u16,
    #[serde(with = "KeyModifiersDef")]
    pub modifiers: KeyModifiers,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "MouseEventKind")]
enum MouseEventKindDef {
    #[serde(with = "MouseButtonDef")]
    Down(MouseButton),
    #[serde(with = "MouseButtonDef")]
    Up(MouseButton),
    #[serde(with = "MouseButtonDef")]
    Drag(MouseButton),
    Moved,
    ScrollDown,
    ScrollUp,
    ScrollLeft,
    ScrollRight,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "MouseButton")]
enum MouseButtonDef {
    Left,
    Right,
    Middle,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "KeyEvent")]
struct KeyEventDef {
    #[serde(with = "KeyCodeDef")]
    pub code: KeyCode,
    #[serde(with = "KeyModifiersDef")]
    pub modifiers: KeyModifiers,
    #[serde(with = "KeyEventKindDef")]
    pub kind: KeyEventKind,
    #[serde(with = "KeyEventStateDef")]
    pub state: KeyEventState,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "KeyEventKind")]
enum KeyEventKindDef {
    Press,
    Repeat,
    Release,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "KeyCode")]
enum KeyCodeDef {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    F(u8),
    Char(char),
    Null,
    Esc,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    KeypadBegin,
    #[serde(with = "MediaKeyCodeDef")]
    Media(MediaKeyCode),
    #[serde(with = "ModifierKeyCodeDef")]
    Modifier(ModifierKeyCode),
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "MediaKeyCode")]
enum MediaKeyCodeDef {
    Play,
    Pause,
    PlayPause,
    Reverse,
    Stop,
    FastForward,
    Rewind,
    TrackNext,
    TrackPrevious,
    Record,
    LowerVolume,
    RaiseVolume,
    MuteVolume,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ModifierKeyCode")]
enum ModifierKeyCodeDef {
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

struct KeyModifiersDef;

impl KeyModifiersDef {
    fn serialize<S: serde::Serializer>(
        value: &KeyModifiers,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(value.bits())
    }

    fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<KeyModifiers, D::Error> {
        Ok(KeyModifiers::from_bits_retain(u8::deserialize(
            deserializer,
        )?))
    }
}

struct KeyEventStateDef;

impl KeyEventStateDef {
    fn serialize<S: serde::Serializer>(
        value: &KeyEventState,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(value.bits())
    }

    fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<KeyEventState, D::Error> {
        Ok(KeyEventState::from_bits_retain(u8::deserialize(
            deserializer,
        )?))
    }
}
