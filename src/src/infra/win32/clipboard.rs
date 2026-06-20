use std::mem;
use std::ptr;
use std::slice;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};

use crate::error::{AppError, AppResult};

const CF_UNICODETEXT: u32 = 13;

pub(super) fn set_text(hwnd: HWND, text: &str) -> AppResult<()> {
    let mut wide = text.encode_utf16().collect::<Vec<_>>();
    wide.push(0);
    let byte_len = wide
        .len()
        .checked_mul(mem::size_of::<u16>())
        .ok_or(AppError::InvalidInput("clipboard text is too large"))?;

    let mut memory = ClipboardMemory::allocate(byte_len)?;
    memory.write_utf16(&wide)?;

    let _clipboard = OpenClipboardGuard::open(hwnd)?;
    // SAFETY: the clipboard is open for the current thread.
    let emptied = unsafe { EmptyClipboard() };
    if emptied == 0 {
        return Err(AppError::win32("EmptyClipboard"));
    }

    // SAFETY: memory.handle is a moveable global memory block containing NUL-terminated UTF-16.
    let stored = unsafe { SetClipboardData(CF_UNICODETEXT, memory.handle() as HANDLE) };
    if stored.is_null() {
        return Err(AppError::win32("SetClipboardData"));
    }

    memory.release_to_clipboard();
    Ok(())
}

pub(super) fn get_text(hwnd: HWND) -> AppResult<Option<String>> {
    // SAFETY: reads clipboard format availability only.
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
        return Ok(None);
    }

    let _clipboard = OpenClipboardGuard::open(hwnd)?;
    // SAFETY: the clipboard is open and CF_UNICODETEXT availability was checked.
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if handle.is_null() {
        return Err(AppError::win32("GetClipboardData"));
    }

    read_global_utf16_text(handle as HGLOBAL).map(Some)
}

fn read_global_utf16_text(handle: HGLOBAL) -> AppResult<String> {
    // SAFETY: handle is owned by the clipboard and remains valid while the clipboard is open.
    let byte_len = unsafe { GlobalSize(handle) };
    let unit_len = byte_len / mem::size_of::<u16>();

    // SAFETY: handle is a clipboard global memory handle for reading.
    let data = unsafe { GlobalLock(handle) } as *const u16;
    if data.is_null() {
        return Err(AppError::win32("GlobalLock clipboard text"));
    }

    let text = {
        // SAFETY: GlobalLock returned a valid pointer to byte_len bytes; unit_len truncates to u16s.
        let units = unsafe { slice::from_raw_parts(data, unit_len) };
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    };

    // SAFETY: data was locked from this handle above. GlobalUnlock can report a benign zero when
    // the lock count reaches zero, so this path does not surface it as a recoverable UI error.
    unsafe {
        GlobalUnlock(handle);
    }

    Ok(text)
}

struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn open(hwnd: HWND) -> AppResult<Self> {
        // SAFETY: hwnd is the main window or null in tests; OpenClipboard validates ownership.
        let opened = unsafe { OpenClipboard(hwnd) };
        if opened == 0 {
            Err(AppError::win32("OpenClipboard"))
        } else {
            Ok(Self)
        }
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: this guard exists only after OpenClipboard succeeds on this thread.
        unsafe {
            CloseClipboard();
        }
    }
}

struct ClipboardMemory {
    handle: HGLOBAL,
    owned: bool,
}

impl ClipboardMemory {
    fn allocate(byte_len: usize) -> AppResult<Self> {
        // SAFETY: GlobalAlloc returns a handle owned by this wrapper until SetClipboardData succeeds.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
        if handle.is_null() {
            Err(AppError::win32("GlobalAlloc clipboard text"))
        } else {
            Ok(Self {
                handle,
                owned: true,
            })
        }
    }

    fn handle(&self) -> HGLOBAL {
        self.handle
    }

    fn write_utf16(&mut self, units: &[u16]) -> AppResult<()> {
        let byte_len = units
            .len()
            .checked_mul(mem::size_of::<u16>())
            .ok_or(AppError::InvalidInput("clipboard text is too large"))?;

        // SAFETY: handle is an allocated moveable global memory object owned by this wrapper.
        let data = unsafe { GlobalLock(self.handle) } as *mut u8;
        if data.is_null() {
            return Err(AppError::win32("GlobalLock clipboard text"));
        }

        // SAFETY: data points to a block of at least byte_len bytes allocated above.
        unsafe {
            ptr::copy_nonoverlapping(units.as_ptr().cast::<u8>(), data, byte_len);
            GlobalUnlock(self.handle);
        }

        Ok(())
    }

    fn release_to_clipboard(&mut self) {
        self.owned = false;
    }
}

impl Drop for ClipboardMemory {
    fn drop(&mut self) {
        if !self.owned || self.handle.is_null() {
            return;
        }

        // SAFETY: handle is still owned by this wrapper because SetClipboardData did not take it.
        unsafe {
            GlobalFree(self.handle);
        }
    }
}
