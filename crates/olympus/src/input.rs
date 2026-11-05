/// A key, independent of the windowing layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Insert,
    PageUp,
    PageDown,
    F(u8),
}

/// Held modifier keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[cfg(test)]
impl KeyModifiers {
    pub const NONE: Self = Self { ctrl: false, alt: false, shift: false };
    pub const CONTROL: Self = Self { ctrl: true, alt: false, shift: false };
    pub const ALT: Self = Self { ctrl: false, alt: true, shift: false };
}

/// A key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyEvent {
    /// Builds a key press.
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }
}
