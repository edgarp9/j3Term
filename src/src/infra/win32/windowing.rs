use std::mem::MaybeUninit;
use std::ptr;

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::UpdateWindow;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageW, IDC_ARROW, IsWindow, LoadCursorW, MSG, RegisterClassW, SW_SHOW, ShowWindow,
    TranslateMessage, WM_CHAR, WM_KEYDOWN, WM_SYSKEYDOWN, WNDCLASSW, WNDPROC,
};

use crate::error::{AppError, AppResult};

use super::controls::is_command_button_child;

pub(super) fn current_instance() -> AppResult<HINSTANCE> {
    // SAFETY: null requests the module handle for the current process.
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance.is_null() {
        Err(AppError::win32("GetModuleHandleW"))
    } else {
        Ok(instance)
    }
}

pub(super) fn register_window_class(
    instance: HINSTANCE,
    class_name: &[u16],
    window_proc: WNDPROC,
) -> AppResult<()> {
    // SAFETY: requesting a predefined system cursor with a null instance is valid.
    let cursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    let window_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: window_proc,
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: class_name.as_ptr(),
        ..WNDCLASSW::default()
    };

    // SAFETY: WNDCLASSW contains valid pointers for the duration of this call.
    let atom = unsafe { RegisterClassW(&window_class) };
    if atom == 0 {
        Err(AppError::win32("RegisterClassW"))
    } else {
        Ok(())
    }
}

pub(super) fn show_main_window(hwnd: HWND) {
    // SAFETY: hwnd is a valid top-level window.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetFocus(hwnd);
    }
}

pub(super) fn destroy_window_if_alive(hwnd: HWND) -> AppResult<()> {
    if hwnd.is_null() {
        return Ok(());
    }

    // SAFETY: IsWindow only inspects the HWND value.
    if unsafe { IsWindow(hwnd) } == 0 {
        return Ok(());
    }

    // SAFETY: hwnd is live according to IsWindow and belongs to this process.
    let destroyed = unsafe { DestroyWindow(hwnd) };
    if destroyed == 0 {
        Err(AppError::win32("DestroyWindow"))
    } else {
        Ok(())
    }
}

pub(super) fn focus_main_window(hwnd: HWND) {
    // SAFETY: hwnd is the main window and can receive keyboard focus while it is live.
    unsafe {
        SetFocus(hwnd);
    }
}

pub(super) fn client_rect(hwnd: HWND) -> AppResult<RECT> {
    let mut rect = RECT::default();
    // SAFETY: rect points to valid writable memory for GetClientRect.
    let ok = unsafe { GetClientRect(hwnd, &mut rect) };
    if ok == 0 {
        Err(AppError::win32("GetClientRect"))
    } else {
        Ok(rect)
    }
}

pub(super) fn message_loop(main_hwnd: HWND) -> AppResult<()> {
    let mut message_storage = MaybeUninit::<MSG>::zeroed();

    loop {
        // SAFETY: message points to valid writable storage and filters are not used.
        let result = unsafe { GetMessageW(message_storage.as_mut_ptr(), ptr::null_mut(), 0, 0) };
        if result == -1 {
            return Err(AppError::win32("GetMessageW"));
        }

        if result == 0 {
            break;
        }

        // SAFETY: GetMessageW returned a positive value and initialized MSG.
        let mut message = unsafe { message_storage.assume_init() };
        retarget_terminal_keyboard_message(main_hwnd, &mut message);
        // SAFETY: message was produced by GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        message_storage = MaybeUninit::<MSG>::zeroed();
    }

    Ok(())
}

pub(super) fn default_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: hwnd/message/wparam/lparam are received from the system for this proc.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

pub(super) fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn retarget_terminal_keyboard_message(main_hwnd: HWND, message: &mut MSG) {
    if !is_terminal_keyboard_message(message.message)
        || message.hwnd == main_hwnd
        || !is_command_button_child(main_hwnd, message.hwnd)
    {
        return;
    }

    focus_main_window(main_hwnd);
    message.hwnd = main_hwnd;
}

fn is_terminal_keyboard_message(message: u32) -> bool {
    matches!(message, WM_CHAR | WM_KEYDOWN | WM_SYSKEYDOWN)
}
