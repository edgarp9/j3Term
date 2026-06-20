use std::cell::Cell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_BACK, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_MENU, VK_RETURN, VK_RIGHT,
    VK_SHIFT, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::domain::{
    TerminalInput, TerminalKey, TerminalKeyModifiers, UiPoint, terminal_input_from_char,
    terminal_input_from_key,
};
use crate::error::{AppError, AppResult};

const WM_CHAR_BACKSPACE: u16 = 0x08;
const WM_CHAR_TAB: u16 = 0x09;
const WM_CHAR_ENTER: u16 = 0x0d;
const WM_CHAR_ESCAPE: u16 = 0x1b;

thread_local! {
    static PENDING_KEYDOWN_OWNED_CONTROL: Cell<Option<u16>> = const { Cell::new(None) };
}

#[derive(Default)]
pub(super) struct InputMapper {
    pending_high_surrogate: Option<u16>,
}

impl InputMapper {
    pub(super) fn char_input(&mut self, wparam: WPARAM) -> Option<TerminalInput> {
        char_input(wparam, &mut self.pending_high_surrogate)
    }

    pub(super) fn clear_pending_char(&mut self) {
        self.pending_high_surrogate = None;
    }
}

pub(super) fn point_from_lparam(lparam: LPARAM) -> UiPoint {
    let raw = lparam as u32;
    UiPoint {
        x: (raw & 0xffff) as u16 as i16 as i32,
        y: ((raw >> 16) & 0xffff) as u16 as i16 as i32,
    }
}

pub(super) fn screen_point_from_lparam(lparam: LPARAM) -> UiPoint {
    if lparam != -1 {
        return point_from_lparam(lparam);
    }

    let mut point = POINT::default();
    // SAFETY: point is valid writable storage for the cursor position.
    let ok = unsafe { GetCursorPos(&mut point) };
    if ok == 0 {
        UiPoint { x: 0, y: 0 }
    } else {
        UiPoint {
            x: point.x,
            y: point.y,
        }
    }
}

pub(super) fn client_point_from_screen(hwnd: HWND, point: UiPoint) -> AppResult<UiPoint> {
    let mut point = POINT {
        x: point.x,
        y: point.y,
    };
    // SAFETY: hwnd is a live window on the UI thread; point is valid writable storage.
    let ok = unsafe { ScreenToClient(hwnd, &mut point) };
    if ok == 0 {
        Err(AppError::win32("ScreenToClient"))
    } else {
        Ok(UiPoint {
            x: point.x,
            y: point.y,
        })
    }
}

pub(super) fn mouse_wheel_delta(wparam: WPARAM) -> i32 {
    ((wparam >> 16) & 0xffff) as u16 as i16 as i32
}

pub(super) fn key_input(wparam: WPARAM, modifiers: TerminalKeyModifiers) -> Option<TerminalInput> {
    let key = u16::try_from(wparam).ok()?;
    let (code, keydown_owned_control) = match key {
        VK_RETURN => (TerminalKey::Enter, Some(WM_CHAR_ENTER)),
        VK_BACK => (TerminalKey::Backspace, Some(WM_CHAR_BACKSPACE)),
        VK_TAB => (TerminalKey::Tab, Some(WM_CHAR_TAB)),
        VK_ESCAPE => (TerminalKey::Escape, Some(WM_CHAR_ESCAPE)),
        VK_UP => (TerminalKey::ArrowUp, None),
        VK_DOWN => (TerminalKey::ArrowDown, None),
        VK_RIGHT => (TerminalKey::ArrowRight, None),
        VK_LEFT => (TerminalKey::ArrowLeft, None),
        _ => return None,
    };

    if let Some(unit) = keydown_owned_control {
        remember_keydown_owned_control(unit);
    }

    Some(terminal_input_from_key(code, modifiers))
}

pub(super) fn suppress_next_control_char(unit: u16) {
    remember_keydown_owned_control(unit);
}

pub(super) fn current_key_modifiers() -> TerminalKeyModifiers {
    TerminalKeyModifiers::new(
        key_is_down(VK_SHIFT),
        key_is_down(VK_MENU),
        key_is_down(VK_CONTROL),
    )
}

fn char_input(wparam: WPARAM, pending_high_surrogate: &mut Option<u16>) -> Option<TerminalInput> {
    let unit = u16::try_from(wparam).ok()?;

    if take_keydown_owned_control_duplicate(unit) {
        *pending_high_surrogate = None;
        return None;
    }

    if is_high_surrogate(unit) {
        *pending_high_surrogate = Some(unit);
        return None;
    }

    if is_low_surrogate(unit) {
        let high = pending_high_surrogate.take()?;
        let character = char_from_surrogate_pair(high, unit)?;
        return Some(terminal_input_from_char(character));
    }

    *pending_high_surrogate = None;
    char::from_u32(u32::from(unit)).map(terminal_input_from_char)
}

fn key_is_down(virtual_key: u16) -> bool {
    // SAFETY: GetKeyState reads the calling thread's keyboard state for a virtual key code.
    let state = unsafe { GetKeyState(i32::from(virtual_key)) };
    (state as u16 & 0x8000) != 0
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xd800..=0xdbff).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xdc00..=0xdfff).contains(&unit)
}

fn char_from_surrogate_pair(high: u16, low: u16) -> Option<char> {
    if !is_high_surrogate(high) || !is_low_surrogate(low) {
        return None;
    }

    let high_ten_bits = u32::from(high) - 0xd800;
    let low_ten_bits = u32::from(low) - 0xdc00;
    let codepoint = 0x10000 + ((high_ten_bits << 10) | low_ten_bits);
    char::from_u32(codepoint)
}

fn remember_keydown_owned_control(unit: u16) {
    PENDING_KEYDOWN_OWNED_CONTROL.with(|pending| pending.set(Some(unit)));
}

fn take_keydown_owned_control_duplicate(unit: u16) -> bool {
    PENDING_KEYDOWN_OWNED_CONTROL.with(|pending| {
        let duplicate = pending.get();
        pending.set(None);
        duplicate == Some(unit)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_keydown_owned_control() {
        PENDING_KEYDOWN_OWNED_CONTROL.with(|pending| pending.set(None));
    }

    #[test]
    fn wm_char_regular_text_and_ctrl_c_map_to_terminal_input() {
        reset_keydown_owned_control();
        let mut pending_high_surrogate = None;

        assert_eq!(
            char_input(usize::from('h' as u16), &mut pending_high_surrogate)
                .map(|input| input.to_pty_bytes().to_vec()),
            Some(b"h".to_vec())
        );
        assert_eq!(
            char_input(0x03, &mut pending_high_surrogate)
                .map(|input| input.to_pty_bytes().to_vec()),
            Some(vec![0x03])
        );
    }

    #[test]
    fn wm_char_control_characters_map_without_keydown_duplicate() {
        reset_keydown_owned_control();

        for unit in [
            WM_CHAR_BACKSPACE,
            WM_CHAR_TAB,
            WM_CHAR_ENTER,
            WM_CHAR_ESCAPE,
        ] {
            let mut pending_high_surrogate = None;

            assert_eq!(
                char_input(usize::from(unit), &mut pending_high_surrogate)
                    .map(|input| input.to_pty_bytes().to_vec()),
                Some(vec![unit as u8])
            );
        }
    }

    #[test]
    fn wm_char_ignores_only_keydown_owned_control_duplicates() {
        reset_keydown_owned_control();
        let modifiers = TerminalKeyModifiers::default();

        for (virtual_key, control_unit) in [
            (VK_RETURN, WM_CHAR_ENTER),
            (VK_BACK, WM_CHAR_BACKSPACE),
            (VK_TAB, WM_CHAR_TAB),
            (VK_ESCAPE, WM_CHAR_ESCAPE),
        ] {
            let mut pending_high_surrogate = None;

            assert!(
                key_input(usize::from(virtual_key), modifiers).is_some(),
                "special key should be handled by WM_KEYDOWN"
            );
            assert_eq!(
                char_input(usize::from(control_unit), &mut pending_high_surrogate),
                None
            );
            assert_eq!(
                char_input(usize::from(control_unit), &mut pending_high_surrogate)
                    .map(|input| input.to_pty_bytes().to_vec()),
                Some(vec![control_unit as u8])
            );
        }
    }

    #[test]
    fn wm_char_ctrl_letter_controls_are_not_treated_as_keydown_duplicates() {
        reset_keydown_owned_control();
        const VK_LEFT_BRACKET: u16 = 0xdb;
        let modifiers = TerminalKeyModifiers::new(false, false, true);

        for (virtual_key, control_unit) in [
            (u16::from(b'H'), WM_CHAR_BACKSPACE),
            (u16::from(b'I'), WM_CHAR_TAB),
            (u16::from(b'M'), WM_CHAR_ENTER),
            (VK_LEFT_BRACKET, WM_CHAR_ESCAPE),
        ] {
            let mut pending_high_surrogate = None;

            assert_eq!(key_input(usize::from(virtual_key), modifiers), None);
            assert_eq!(
                char_input(usize::from(control_unit), &mut pending_high_surrogate)
                    .map(|input| input.to_pty_bytes().to_vec()),
                Some(vec![control_unit as u8])
            );
        }
    }

    #[test]
    fn wm_char_combines_surrogate_pair_before_domain_mapping() {
        reset_keydown_owned_control();
        let mut pending_high_surrogate = None;

        assert_eq!(
            char_input(0xd83d, &mut pending_high_surrogate),
            None,
            "high surrogate should wait for the next UTF-16 unit"
        );
        assert_eq!(
            char_input(0xde00, &mut pending_high_surrogate)
                .map(|input| input.to_pty_bytes().to_vec()),
            Some("😀".as_bytes().to_vec())
        );
    }

    #[test]
    fn wm_keydown_special_keys_map_to_terminal_sequences() {
        reset_keydown_owned_control();
        let modifiers = TerminalKeyModifiers::default();

        assert_eq!(
            key_input(usize::from(VK_RETURN), modifiers).map(|input| input.to_pty_bytes().to_vec()),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            key_input(usize::from(VK_BACK), modifiers).map(|input| input.to_pty_bytes().to_vec()),
            Some(vec![0x7f])
        );
        assert_eq!(
            key_input(usize::from(VK_TAB), modifiers).map(|input| input.to_pty_bytes().to_vec()),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            key_input(usize::from(VK_ESCAPE), modifiers).map(|input| input.to_pty_bytes().to_vec()),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn wm_keydown_arrow_keys_include_modifiers() {
        reset_keydown_owned_control();
        let modifiers = TerminalKeyModifiers::new(true, false, true);

        assert_eq!(
            key_input(usize::from(VK_LEFT), modifiers).map(|input| input.to_pty_bytes().to_vec()),
            Some(b"\x1b[1;6D".to_vec())
        );
        assert_eq!(
            key_input(usize::from(VK_UP), TerminalKeyModifiers::default())
                .map(|input| input.to_pty_bytes().to_vec()),
            Some(b"\x1b[A".to_vec())
        );
    }
}
