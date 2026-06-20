use std::ptr;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, HMENU, IDYES, MB_ICONWARNING, MB_YESNO, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MessageBoxW, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu,
};

use crate::domain::UiPoint;
use crate::error::{AppError, AppResult};

use super::windowing::wide_null;

const CATEGORY_MENU_NEW_CATEGORY: usize = 1;
const CATEGORY_MENU_RENAME_CATEGORY: usize = 2;
const CATEGORY_MENU_DELETE_CATEGORY: usize = 3;
const CATEGORY_MENU_MOVE_UP: usize = 4;
const CATEGORY_MENU_MOVE_DOWN: usize = 5;
const CATEGORY_MENU_ADD_BUTTON: usize = 6;
const CATEGORY_MENU_FONT_SETTINGS: usize = 7;
const CATEGORY_MENU_ABOUT: usize = 8;

const BUTTON_MENU_RUN: usize = 101;
const BUTTON_MENU_EDIT: usize = 102;
const BUTTON_MENU_DELETE: usize = 103;
const BUTTON_MENU_MOVE_UP: usize = 104;
const BUTTON_MENU_MOVE_DOWN: usize = 105;
const BUTTON_MENU_FONT_SETTINGS: usize = 106;
const BUTTON_MENU_ABOUT: usize = 107;

pub(super) struct CategoryMenuState {
    pub can_delete: bool,
    pub can_move_up: bool,
    pub can_move_down: bool,
}

pub(super) struct ButtonMenuState {
    pub can_move_up: bool,
    pub can_move_down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CategoryMenuAction {
    NewCategory,
    RenameCategory,
    DeleteCategory,
    MoveCategoryUp,
    MoveCategoryDown,
    AddButton,
    FontSettings,
    About,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ButtonMenuAction {
    Run,
    Edit,
    Delete,
    MoveUp,
    MoveDown,
    FontSettings,
    About,
}

pub(super) fn show_category_menu(
    hwnd: HWND,
    point: UiPoint,
    state: CategoryMenuState,
) -> AppResult<Option<CategoryMenuAction>> {
    let menu = OwnedMenu::popup()?;
    append_action(
        menu.handle(),
        CATEGORY_MENU_NEW_CATEGORY,
        "New Category",
        true,
    )?;
    append_action(
        menu.handle(),
        CATEGORY_MENU_RENAME_CATEGORY,
        "Rename Category",
        true,
    )?;
    append_action(
        menu.handle(),
        CATEGORY_MENU_DELETE_CATEGORY,
        "Delete Category",
        state.can_delete,
    )?;
    append_separator(menu.handle())?;
    append_action(
        menu.handle(),
        CATEGORY_MENU_MOVE_UP,
        "Move Category Up",
        state.can_move_up,
    )?;
    append_action(
        menu.handle(),
        CATEGORY_MENU_MOVE_DOWN,
        "Move Category Down",
        state.can_move_down,
    )?;
    append_separator(menu.handle())?;
    append_action(menu.handle(), CATEGORY_MENU_ADD_BUTTON, "Add Button", true)?;
    append_separator(menu.handle())?;
    append_action(
        menu.handle(),
        CATEGORY_MENU_FONT_SETTINGS,
        "Font Settings...",
        true,
    )?;
    append_separator(menu.handle())?;
    append_action(menu.handle(), CATEGORY_MENU_ABOUT, "About j3Term...", true)?;

    let selected = track_menu(hwnd, point, menu.handle())?;
    Ok(match selected {
        0 => None,
        CATEGORY_MENU_NEW_CATEGORY => Some(CategoryMenuAction::NewCategory),
        CATEGORY_MENU_RENAME_CATEGORY => Some(CategoryMenuAction::RenameCategory),
        CATEGORY_MENU_DELETE_CATEGORY => Some(CategoryMenuAction::DeleteCategory),
        CATEGORY_MENU_MOVE_UP => Some(CategoryMenuAction::MoveCategoryUp),
        CATEGORY_MENU_MOVE_DOWN => Some(CategoryMenuAction::MoveCategoryDown),
        CATEGORY_MENU_ADD_BUTTON => Some(CategoryMenuAction::AddButton),
        CATEGORY_MENU_FONT_SETTINGS => Some(CategoryMenuAction::FontSettings),
        CATEGORY_MENU_ABOUT => Some(CategoryMenuAction::About),
        _ => None,
    })
}

pub(super) fn show_button_menu(
    hwnd: HWND,
    point: UiPoint,
    state: ButtonMenuState,
) -> AppResult<Option<ButtonMenuAction>> {
    let menu = OwnedMenu::popup()?;
    append_action(menu.handle(), BUTTON_MENU_RUN, "Run Command", true)?;
    append_action(menu.handle(), BUTTON_MENU_EDIT, "Edit Button", true)?;
    append_separator(menu.handle())?;
    append_action(menu.handle(), BUTTON_MENU_DELETE, "Delete Button", true)?;
    append_separator(menu.handle())?;
    append_action(
        menu.handle(),
        BUTTON_MENU_MOVE_UP,
        "Move Button Up",
        state.can_move_up,
    )?;
    append_action(
        menu.handle(),
        BUTTON_MENU_MOVE_DOWN,
        "Move Button Down",
        state.can_move_down,
    )?;
    append_separator(menu.handle())?;
    append_action(
        menu.handle(),
        BUTTON_MENU_FONT_SETTINGS,
        "Font Settings...",
        true,
    )?;
    append_separator(menu.handle())?;
    append_action(menu.handle(), BUTTON_MENU_ABOUT, "About j3Term...", true)?;

    let selected = track_menu(hwnd, point, menu.handle())?;
    Ok(match selected {
        0 => None,
        BUTTON_MENU_RUN => Some(ButtonMenuAction::Run),
        BUTTON_MENU_EDIT => Some(ButtonMenuAction::Edit),
        BUTTON_MENU_DELETE => Some(ButtonMenuAction::Delete),
        BUTTON_MENU_MOVE_UP => Some(ButtonMenuAction::MoveUp),
        BUTTON_MENU_MOVE_DOWN => Some(ButtonMenuAction::MoveDown),
        BUTTON_MENU_FONT_SETTINGS => Some(ButtonMenuAction::FontSettings),
        BUTTON_MENU_ABOUT => Some(ButtonMenuAction::About),
        _ => None,
    })
}

pub(super) fn confirm_delete_category(hwnd: HWND) -> bool {
    confirm(
        hwnd,
        "Delete this category and all buttons in it?",
        "Delete Category",
    )
}

pub(super) fn confirm_delete_button(hwnd: HWND) -> bool {
    confirm(hwnd, "Delete this command button?", "Delete Button")
}

fn confirm(hwnd: HWND, message: &str, title: &str) -> bool {
    let message = wide_null(message);
    let title = wide_null(title);
    // SAFETY: strings are valid null-terminated UTF-16 buffers for this call.
    unsafe {
        MessageBoxW(
            hwnd,
            message.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONWARNING,
        ) == IDYES
    }
}

fn append_action(menu: HMENU, id: usize, label: &str, enabled: bool) -> AppResult<()> {
    let label = wide_null(label);
    let flags = MF_STRING | if enabled { 0 } else { MF_GRAYED };
    // SAFETY: label points to a null-terminated UTF-16 string for the duration of the call.
    let appended = unsafe { AppendMenuW(menu, flags, id, label.as_ptr()) };
    if appended == 0 {
        Err(AppError::win32("AppendMenuW"))
    } else {
        Ok(())
    }
}

fn append_separator(menu: HMENU) -> AppResult<()> {
    // SAFETY: appending a separator ignores id and text.
    let appended = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null()) };
    if appended == 0 {
        Err(AppError::win32("AppendMenuW separator"))
    } else {
        Ok(())
    }
}

fn track_menu(hwnd: HWND, point: UiPoint, menu: HMENU) -> AppResult<usize> {
    // SAFETY: hwnd is the active top-level window; this improves menu dismissal behavior.
    unsafe {
        SetForegroundWindow(hwnd);
    }

    // SAFETY: menu is a live popup menu and hwnd owns the command target.
    let selected = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            0,
            hwnd,
            ptr::null(),
        )
    };
    if selected < 0 {
        Err(AppError::win32("TrackPopupMenu"))
    } else {
        usize::try_from(selected).map_err(|_| AppError::InvalidInput("menu command is invalid"))
    }
}

struct OwnedMenu {
    handle: HMENU,
}

impl OwnedMenu {
    fn popup() -> AppResult<Self> {
        // SAFETY: CreatePopupMenu returns an owned menu handle that Drop destroys.
        let handle = unsafe { CreatePopupMenu() };
        if handle.is_null() {
            Err(AppError::win32("CreatePopupMenu"))
        } else {
            Ok(Self { handle })
        }
    }

    fn handle(&self) -> HMENU {
        self.handle
    }
}

impl Drop for OwnedMenu {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }

        // SAFETY: handle is an owned menu created by CreatePopupMenu.
        unsafe {
            DestroyMenu(self.handle);
        }
    }
}
