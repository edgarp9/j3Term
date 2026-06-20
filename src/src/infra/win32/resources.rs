use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetSystemMetrics, HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
    LoadImageW, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SYSTEM_METRICS_INDEX, SendMessageW,
    WM_SETICON,
};

use crate::error::{AppError, AppResult};

const APPLICATION_ICON_RESOURCE_ID: u16 = 1;

pub(super) struct WindowIcons {
    big: HICON,
    small: HICON,
}

impl WindowIcons {
    pub(super) fn load(instance: HINSTANCE) -> AppResult<Self> {
        let big = load_application_icon(
            instance,
            system_metric(SM_CXICON, 32),
            system_metric(SM_CYICON, 32),
        )?;
        let small = match load_application_icon(
            instance,
            system_metric(SM_CXSMICON, 16),
            system_metric(SM_CYSMICON, 16),
        ) {
            Ok(icon) => icon,
            Err(error) => {
                destroy_icon(big);
                return Err(error);
            }
        };

        Ok(Self { big, small })
    }

    pub(super) fn apply(&self, hwnd: HWND) {
        // SAFETY: hwnd is the live top-level window, and both HICON handles remain owned by
        // WindowState until the window is destroyed.
        unsafe {
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as WPARAM, self.big as LPARAM);
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as WPARAM, self.small as LPARAM);
        }
    }
}

impl Drop for WindowIcons {
    fn drop(&mut self) {
        destroy_icon(self.big);
        destroy_icon(self.small);
    }
}

fn load_application_icon(instance: HINSTANCE, width: i32, height: i32) -> AppResult<HICON> {
    // SAFETY: the resource id points to the icon embedded by build.rs, and the requested size
    // comes from GetSystemMetrics or a positive fallback.
    let handle = unsafe {
        LoadImageW(
            instance,
            int_resource(APPLICATION_ICON_RESOURCE_ID),
            IMAGE_ICON,
            width,
            height,
            LR_DEFAULTCOLOR,
        )
    };
    let icon = handle as HICON;
    if icon.is_null() {
        Err(AppError::win32("LoadImageW application icon"))
    } else {
        Ok(icon)
    }
}

fn int_resource(id: u16) -> *const u16 {
    usize::from(id) as *const u16
}

fn system_metric(index: SYSTEM_METRICS_INDEX, fallback: i32) -> i32 {
    // SAFETY: GetSystemMetrics reads a process-independent system setting for a known index.
    let value = unsafe { GetSystemMetrics(index) };
    if value > 0 { value } else { fallback }
}

fn destroy_icon(icon: HICON) {
    if icon.is_null() {
        return;
    }

    // SAFETY: icons are loaded without LR_SHARED and are owned by WindowIcons.
    unsafe {
        DestroyIcon(icon);
    }
}
