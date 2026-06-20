#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalInput {
    Character(char),
    Control(u8),
    Key {
        code: TerminalKey,
        modifiers: TerminalKeyModifiers,
    },
}

const TERMINAL_INPUT_BYTES_CAPACITY: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalInputBytes {
    bytes: [u8; TERMINAL_INPUT_BYTES_CAPACITY],
    len: u8,
}

impl TerminalInputBytes {
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    pub fn to_vec(self) -> Vec<u8> {
        self.as_slice().to_vec()
    }

    fn from_character(character: char) -> Self {
        let mut bytes = [0_u8; TERMINAL_INPUT_BYTES_CAPACITY];
        let len = character.encode_utf8(&mut bytes).len() as u8;
        Self { bytes, len }
    }

    fn from_byte(byte: u8) -> Self {
        Self {
            bytes: [byte, 0, 0, 0, 0, 0],
            len: 1,
        }
    }

    fn from_three(bytes: [u8; 3]) -> Self {
        Self {
            bytes: [bytes[0], bytes[1], bytes[2], 0, 0, 0],
            len: 3,
        }
    }

    fn from_six(bytes: [u8; 6]) -> Self {
        Self { bytes, len: 6 }
    }
}

impl TerminalInput {
    pub fn to_pty_bytes(&self) -> TerminalInputBytes {
        match self {
            Self::Character(character) => TerminalInputBytes::from_character(*character),
            Self::Control(byte) => TerminalInputBytes::from_byte(*byte),
            Self::Key { code, modifiers } => key_to_pty_bytes(*code, *modifiers),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKey {
    Enter,
    Backspace,
    Tab,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowRight,
    ArrowLeft,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalKeyModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl TerminalKeyModifiers {
    pub fn new(shift: bool, alt: bool, ctrl: bool) -> Self {
        Self { shift, alt, ctrl }
    }

    fn xterm_modifier_parameter(self) -> Option<u8> {
        let parameter =
            1 + u8::from(self.shift) + (u8::from(self.alt) * 2) + (u8::from(self.ctrl) * 4);

        if parameter == 1 {
            None
        } else {
            Some(parameter)
        }
    }
}

pub fn terminal_input_from_char(character: char) -> TerminalInput {
    let codepoint = character as u32;
    if codepoint <= 0x1f || codepoint == 0x7f {
        TerminalInput::Control(codepoint as u8)
    } else {
        TerminalInput::Character(character)
    }
}

#[cfg(any(not(target_os = "windows"), test))]
pub fn terminal_input_from_modified_char(
    character: char,
    modifiers: TerminalKeyModifiers,
) -> TerminalInput {
    if modifiers.ctrl
        && let Some(byte) = ctrl_modified_character_byte(character)
    {
        return TerminalInput::Control(byte);
    }

    terminal_input_from_char(character)
}

#[cfg(any(not(target_os = "windows"), test))]
fn ctrl_modified_character_byte(character: char) -> Option<u8> {
    match character {
        ' ' | '@' | '`' => Some(0x00),
        'a'..='z' => Some((character as u8) - b'a' + 1),
        'A'..='Z' => Some((character as u8) - b'A' + 1),
        '[' | '{' => Some(0x1b),
        '\\' | '|' => Some(0x1c),
        ']' | '}' => Some(0x1d),
        '^' | '~' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

pub fn terminal_input_from_key(
    code: TerminalKey,
    modifiers: TerminalKeyModifiers,
) -> TerminalInput {
    TerminalInput::Key { code, modifiers }
}

pub fn terminal_paste_text_to_pty_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if matches!(characters.peek(), Some('\n')) {
                    let _ = characters.next();
                }
                bytes.push(b'\r');
            }
            '\n' => bytes.push(b'\r'),
            character => {
                let mut encoded = [0; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
        }
    }

    bytes
}

fn key_to_pty_bytes(code: TerminalKey, modifiers: TerminalKeyModifiers) -> TerminalInputBytes {
    let final_byte = match code {
        TerminalKey::Enter => return TerminalInputBytes::from_byte(b'\r'),
        TerminalKey::Backspace => return TerminalInputBytes::from_byte(0x7f),
        TerminalKey::Tab if modifiers.shift => {
            return TerminalInputBytes::from_three([0x1b, b'[', b'Z']);
        }
        TerminalKey::Tab => return TerminalInputBytes::from_byte(b'\t'),
        TerminalKey::Escape => return TerminalInputBytes::from_byte(0x1b),
        TerminalKey::ArrowUp => b'A',
        TerminalKey::ArrowDown => b'B',
        TerminalKey::ArrowRight => b'C',
        TerminalKey::ArrowLeft => b'D',
    };

    csi_key_sequence(final_byte, modifiers)
}

fn csi_key_sequence(final_byte: u8, modifiers: TerminalKeyModifiers) -> TerminalInputBytes {
    match modifiers.xterm_modifier_parameter() {
        Some(parameter) => {
            TerminalInputBytes::from_six([0x1b, b'[', b'1', b';', b'0' + parameter, final_byte])
        }
        None => TerminalInputBytes::from_three([0x1b, b'[', final_byte]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_input_maps_to_utf8_text() {
        assert_eq!(
            terminal_input_from_char('h').to_pty_bytes().as_slice(),
            b"h"
        );
        assert_eq!(
            terminal_input_from_char('한').to_pty_bytes().as_slice(),
            "한".as_bytes()
        );
    }

    #[test]
    fn control_character_input_maps_to_control_bytes() {
        assert_eq!(
            terminal_input_from_char('\r').to_pty_bytes().as_slice(),
            b"\r"
        );
        assert_eq!(
            terminal_input_from_char('\u{08}').to_pty_bytes().as_slice(),
            &[0x08]
        );
        assert_eq!(
            terminal_input_from_char('\t').to_pty_bytes().as_slice(),
            b"\t"
        );
        assert_eq!(
            terminal_input_from_char('\u{03}').to_pty_bytes().as_slice(),
            &[0x03]
        );
    }

    #[test]
    fn ctrl_modified_character_input_maps_to_control_bytes() {
        let ctrl = TerminalKeyModifiers::new(false, false, true);
        let ctrl_shift = TerminalKeyModifiers::new(true, false, true);

        assert_eq!(
            terminal_input_from_modified_char('c', ctrl)
                .to_pty_bytes()
                .as_slice(),
            &[0x03]
        );
        assert_eq!(
            terminal_input_from_modified_char('D', ctrl_shift)
                .to_pty_bytes()
                .as_slice(),
            &[0x04]
        );
        assert_eq!(
            terminal_input_from_modified_char('l', ctrl)
                .to_pty_bytes()
                .as_slice(),
            &[0x0c]
        );
        assert_eq!(
            terminal_input_from_modified_char('[', ctrl)
                .to_pty_bytes()
                .as_slice(),
            &[0x1b]
        );
        assert_eq!(
            terminal_input_from_modified_char('?', ctrl)
                .to_pty_bytes()
                .as_slice(),
            &[0x7f]
        );
    }

    #[test]
    fn special_keys_map_to_terminal_control_sequences() {
        assert_eq!(
            terminal_input_from_key(TerminalKey::Enter, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            b"\r"
        );
        assert_eq!(
            terminal_input_from_key(TerminalKey::Backspace, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            &[0x7f]
        );
        assert_eq!(
            terminal_input_from_key(TerminalKey::Tab, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            b"\t"
        );
        assert_eq!(
            terminal_input_from_key(TerminalKey::Escape, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            &[0x1b]
        );
    }

    #[test]
    fn arrow_keys_map_to_vt_sequences() {
        assert_eq!(
            terminal_input_from_key(TerminalKey::ArrowUp, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            b"\x1b[A"
        );
        assert_eq!(
            terminal_input_from_key(TerminalKey::ArrowDown, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            b"\x1b[B"
        );
        assert_eq!(
            terminal_input_from_key(TerminalKey::ArrowRight, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            b"\x1b[C"
        );
        assert_eq!(
            terminal_input_from_key(TerminalKey::ArrowLeft, TerminalKeyModifiers::default())
                .to_pty_bytes()
                .as_slice(),
            b"\x1b[D"
        );
    }

    #[test]
    fn modified_arrow_keys_use_xterm_modifier_parameters() {
        assert_eq!(
            terminal_input_from_key(
                TerminalKey::ArrowLeft,
                TerminalKeyModifiers::new(false, false, true),
            )
            .to_pty_bytes()
            .as_slice(),
            b"\x1b[1;5D"
        );
        assert_eq!(
            terminal_input_from_key(
                TerminalKey::ArrowRight,
                TerminalKeyModifiers::new(true, true, false),
            )
            .to_pty_bytes()
            .as_slice(),
            b"\x1b[1;4C"
        );
    }

    #[test]
    fn shifted_tab_uses_reverse_tab_sequence() {
        assert_eq!(
            terminal_input_from_key(
                TerminalKey::Tab,
                TerminalKeyModifiers::new(true, false, false)
            )
            .to_pty_bytes()
            .as_slice(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn paste_text_normalizes_line_endings_to_enter_input() {
        assert_eq!(
            terminal_paste_text_to_pty_bytes("one\r\ntwo\nthree\rfour"),
            b"one\rtwo\rthree\rfour".to_vec()
        );
        assert_eq!(
            terminal_paste_text_to_pty_bytes("한글"),
            "한글".as_bytes().to_vec()
        );
    }
}
