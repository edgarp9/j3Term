use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, GWLP_USERDATA, GetWindowLongPtrW, PostQuitMessage, SetWindowLongPtrW,
    WM_CAPTURECHANGED, WM_CHAR, WM_COMMAND, WM_CONTEXTMENU, WM_CREATE, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT,
    WM_SIZE, WM_SYSKEYDOWN, WM_TIMER, WM_VSCROLL,
};

use crate::error::{AppError, AppResult};
use crate::infra::renderer::GdiRenderer;

use super::controls::{
    category_combo_selection_changed, command_button_scroll_request, is_command_button_child,
    terminal_scrollbar_request,
};
use super::dialogs;
use super::input::{
    current_key_modifiers, key_input, mouse_wheel_delta, point_from_lparam,
    screen_point_from_lparam,
};
use super::menus::{
    self, ButtonMenuAction, ButtonMenuState, CategoryMenuAction, CategoryMenuState,
};
use super::windowing::{default_window_proc, focus_main_window};
use super::{ButtonCommandPrompt, ContextMenuRequest, WindowState, store_window_userdata};

// SAFETY: Win32 calls this callback with WNDPROC-compatible arguments for the
// registered window class; each message branch validates raw pointers before use.
pub(super) unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            let create = lparam as *const CREATESTRUCTW;
            if create.is_null() {
                return -1;
            }

            // SAFETY: WM_CREATE lparam points to a CREATESTRUCTW for this window.
            let state_ptr = unsafe { (*create).lpCreateParams as *mut WindowState };
            if state_ptr.is_null() {
                return -1;
            }

            if let Err(error) = store_window_userdata(hwnd, state_ptr as isize) {
                // SAFETY: state_ptr is the non-null WindowState pointer passed through
                // CreateWindowExW and has not been stored in GWLP_USERDATA.
                unsafe {
                    (*state_ptr).record_initialization_error(error);
                }
                return -1;
            }

            with_state(hwnd, |state| state.on_create(hwnd));
            0
        }
        WM_PAINT => {
            with_state_for_paint(hwnd, |state| state.paint(hwnd));
            0
        }
        WM_ERASEBKGND => 1,
        WM_SIZE => {
            with_state(hwnd, |state| {
                let resize = state.resize_to_client(hwnd)?;
                if resize.should_invalidate_client() {
                    GdiRenderer::invalidate(hwnd);
                }
                Ok(())
            });
            0
        }
        WM_COMMAND => {
            let command_source = lparam as HWND;
            if category_combo_selection_changed(wparam, command_source) {
                with_state(hwnd, |state| state.handle_category_selection_changed(hwnd));
                0
            } else if is_command_button_child(hwnd, command_source) {
                focus_main_window(hwnd);
                handle_command_button(hwnd, wparam);
                0
            } else {
                default_window_proc(hwnd, message, wparam, lparam)
            }
        }
        WM_CONTEXTMENU => {
            let source = wparam as HWND;
            let point = screen_point_from_lparam(lparam);
            handle_context_menu(hwnd, source, point);
            0
        }
        WM_VSCROLL => {
            let source = lparam as HWND;
            if let Some(request) = command_button_scroll_request(hwnd, source, wparam) {
                with_state(hwnd, |state| {
                    state.handle_command_button_scroll(hwnd, request)
                });
                0
            } else if let Some(request) = terminal_scrollbar_request(hwnd, source, wparam) {
                with_state(hwnd, |state| state.handle_terminal_scroll(hwnd, request));
                0
            } else {
                default_window_proc(hwnd, message, wparam, lparam)
            }
        }
        WM_MOUSEWHEEL => {
            let point = screen_point_from_lparam(lparam);
            let delta = mouse_wheel_delta(wparam);
            with_state(hwnd, |state| state.handle_mouse_wheel(hwnd, point, delta));
            0
        }
        WM_TIMER => {
            with_state(hwnd, |state| state.drain_pty(hwnd));
            0
        }
        WM_LBUTTONDOWN => {
            let point = point_from_lparam(lparam);
            with_state(hwnd, |state| state.handle_left_button_down(hwnd, point));
            0
        }
        WM_MOUSEMOVE => {
            let point = point_from_lparam(lparam);
            with_state(hwnd, |state| state.handle_mouse_move(hwnd, point));
            0
        }
        WM_LBUTTONUP => {
            with_state(hwnd, |state| state.handle_left_button_up(hwnd));
            0
        }
        WM_CAPTURECHANGED => {
            with_state(hwnd, |state| state.handle_capture_changed());
            0
        }
        WM_CHAR => {
            with_state(hwnd, |state| state.handle_char(wparam));
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let modifiers = current_key_modifiers();
            if with_state_value(hwnd, |state| {
                Ok(state.handle_clipboard_shortcut(hwnd, wparam, modifiers))
            })
            .unwrap_or(false)
            {
                0
            } else if let Some(input) = key_input(wparam, modifiers) {
                with_state(hwnd, |state| state.handle_input(input));
                0
            } else {
                default_window_proc(hwnd, message, wparam, lparam)
            }
        }
        WM_DESTROY => {
            shutdown_state(hwnd);
            // SAFETY: posting quit is valid during WM_DESTROY on the UI thread.
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        WM_NCDESTROY => {
            cleanup_window_state(hwnd);
            default_window_proc(hwnd, message, wparam, lparam)
        }
        _ => default_window_proc(hwnd, message, wparam, lparam),
    }
}

fn handle_command_button(hwnd: HWND, wparam: WPARAM) {
    let Some(prompt) = with_state_value(hwnd, |state| {
        let Some(id) = state.command_button_id_from_wparam(wparam) else {
            return Ok(None);
        };

        Ok(Some(state.prepare_button_command(id)?))
    })
    .flatten() else {
        return;
    };

    run_button_command_prompt(hwnd, prompt);
}

fn handle_context_menu(hwnd: HWND, source: HWND, point: crate::domain::UiPoint) {
    let Some(request) = with_state_value(hwnd, |state| {
        Ok(state.context_menu_request(hwnd, source, point))
    })
    .flatten() else {
        return;
    };

    match request {
        ContextMenuRequest::Category { point, state } => {
            handle_category_context_menu(hwnd, point, state);
        }
        ContextMenuRequest::Button {
            button_id,
            point,
            state,
        } => {
            handle_button_context_menu(hwnd, button_id, point, state);
        }
    }

    focus_main_window(hwnd);
}

fn handle_category_context_menu(
    hwnd: HWND,
    point: crate::domain::UiPoint,
    state: CategoryMenuState,
) {
    let action = match menus::show_category_menu(hwnd, point, state) {
        Ok(Some(action)) => action,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    handle_category_menu_action(hwnd, action);
}

fn handle_button_context_menu(
    hwnd: HWND,
    button_id: crate::domain::CommandButtonId,
    point: crate::domain::UiPoint,
    state: ButtonMenuState,
) {
    let action = match menus::show_button_menu(hwnd, point, state) {
        Ok(Some(action)) => action,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    handle_button_menu_action(hwnd, button_id, action);
}

fn handle_category_menu_action(hwnd: HWND, action: CategoryMenuAction) {
    match action {
        CategoryMenuAction::NewCategory => handle_new_category(hwnd),
        CategoryMenuAction::RenameCategory => handle_rename_category(hwnd),
        CategoryMenuAction::DeleteCategory => {
            if menus::confirm_delete_category(hwnd) {
                with_state(hwnd, |state| state.delete_selected_command_category(hwnd));
            }
        }
        CategoryMenuAction::MoveCategoryUp => {
            with_state(hwnd, |state| state.move_selected_command_category_up(hwnd));
        }
        CategoryMenuAction::MoveCategoryDown => {
            with_state(hwnd, |state| {
                state.move_selected_command_category_down(hwnd)
            });
        }
        CategoryMenuAction::AddButton => handle_add_button(hwnd),
        CategoryMenuAction::FontSettings => handle_font_settings(hwnd),
        CategoryMenuAction::About => handle_about(hwnd),
    }
}

fn handle_button_menu_action(
    hwnd: HWND,
    button_id: crate::domain::CommandButtonId,
    action: ButtonMenuAction,
) {
    match action {
        ButtonMenuAction::Run => {
            let Some(prompt) =
                with_state_value(hwnd, |state| state.prepare_button_command(button_id))
            else {
                return;
            };
            run_button_command_prompt(hwnd, prompt);
        }
        ButtonMenuAction::Edit => handle_edit_button(hwnd, button_id),
        ButtonMenuAction::Delete => {
            if menus::confirm_delete_button(hwnd) {
                with_state(hwnd, |state| state.delete_button(hwnd, button_id));
            }
        }
        ButtonMenuAction::MoveUp => {
            with_state(hwnd, |state| state.move_button_up(hwnd, button_id));
        }
        ButtonMenuAction::MoveDown => {
            with_state(hwnd, |state| state.move_button_down(hwnd, button_id));
        }
        ButtonMenuAction::FontSettings => handle_font_settings(hwnd),
        ButtonMenuAction::About => handle_about(hwnd),
    }
}

fn handle_about(hwnd: HWND) {
    if let Err(error) = dialogs::show_about(hwnd) {
        record_error(hwnd, error);
    }
}

fn handle_font_settings(hwnd: HWND) {
    let Some(current) = with_state_value(hwnd, |state| Ok(state.terminal_font())) else {
        return;
    };

    let font = match dialogs::choose_terminal_font(hwnd, &current) {
        Ok(Some(font)) => font,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    with_state(hwnd, |state| state.change_terminal_font(hwnd, font));
}

fn handle_new_category(hwnd: HWND) {
    let Some(initial) =
        with_state_value(
            hwnd,
            |state| Ok(state.suggested_new_command_category_name()),
        )
    else {
        return;
    };

    let name = match dialogs::prompt_new_category_name(hwnd, &initial) {
        Ok(Some(name)) => name,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    with_state(hwnd, |state| state.add_command_category(hwnd, name));
}

fn handle_rename_category(hwnd: HWND) {
    let Some(initial) = with_state_value(hwnd, |state| state.selected_command_category_name())
    else {
        return;
    };

    let name = match dialogs::prompt_rename_category_name(hwnd, &initial) {
        Ok(Some(name)) => name,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    with_state(hwnd, |state| {
        state.rename_selected_command_category(hwnd, name)
    });
}

fn handle_add_button(hwnd: HWND) {
    let initial = match WindowState::new_button_definition() {
        Ok(initial) => initial,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    let definition = match dialogs::edit_command_button(hwnd, &initial) {
        Ok(Some(definition)) => definition,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    with_state(hwnd, |state| state.add_button(hwnd, definition));
}

fn handle_edit_button(hwnd: HWND, button_id: crate::domain::CommandButtonId) {
    let Some(initial) = with_state_value(hwnd, |state| state.button_definition(button_id)) else {
        return;
    };

    let definition = match dialogs::edit_command_button(hwnd, &initial) {
        Ok(Some(definition)) => definition,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    with_state(hwnd, |state| {
        state.update_button(hwnd, button_id, definition)
    });
}

fn run_button_command_prompt(hwnd: HWND, prompt: ButtonCommandPrompt) {
    let pending = match prompt.collect_values(hwnd) {
        Ok(Some(pending)) => pending,
        Ok(None) => return,
        Err(error) => {
            record_error(hwnd, error);
            return;
        }
    };

    with_state(hwnd, |state| state.run_button_command(pending));
}

fn with_state<F>(hwnd: HWND, operation: F)
where
    F: FnOnce(&mut WindowState) -> AppResult<()>,
{
    let _ = with_state_value(hwnd, operation);
}

fn with_state_value<F, R>(hwnd: HWND, operation: F) -> Option<R>
where
    F: FnOnce(&mut WindowState) -> AppResult<R>,
{
    let ptr = window_state_ptr(hwnd);
    if ptr.is_null() {
        return None;
    }

    let result = {
        // SAFETY: the pointer was installed from Box::into_raw and remains owned by the window.
        // Callers keep this borrow scoped to immediate state work; modal UI runs after it ends.
        let state = unsafe { &mut *ptr };
        operation(state)
    };

    match result {
        Ok(value) => Some(value),
        Err(error) => {
            record_error_for_ptr(hwnd, ptr, error);
            None
        }
    }
}

fn with_state_for_paint<F>(hwnd: HWND, operation: F)
where
    F: FnOnce(&mut WindowState) -> AppResult<()>,
{
    let ptr = window_state_ptr(hwnd);
    if ptr.is_null() {
        return;
    }

    let result = {
        // SAFETY: the pointer was installed from Box::into_raw and is live for the paint message.
        let state = unsafe { &mut *ptr };
        operation(state)
    };
    if let Err(error) = result {
        record_paint_error_for_ptr(ptr, error);
    }
}

fn shutdown_state(hwnd: HWND) {
    let ptr = window_state_ptr(hwnd);
    if ptr.is_null() {
        return;
    }

    let result = {
        // SAFETY: the pointer was installed from Box::into_raw and remains live during WM_DESTROY.
        let state = unsafe { &mut *ptr };
        state.shutdown(hwnd)
    };

    if let Err(error) = result {
        record_shutdown_error_for_ptr(hwnd, ptr, error);
    }
}

fn record_error(hwnd: HWND, error: AppError) {
    let ptr = window_state_ptr(hwnd);
    if ptr.is_null() {
        return;
    }

    record_error_for_ptr(hwnd, ptr, error);
}

fn record_error_for_ptr(hwnd: HWND, ptr: *mut WindowState, error: AppError) {
    // SAFETY: ptr is the valid WindowState pointer checked by the caller.
    let state = unsafe { &mut *ptr };
    state.record_error(error);
    GdiRenderer::invalidate(hwnd);
}

fn record_shutdown_error_for_ptr(hwnd: HWND, ptr: *mut WindowState, error: AppError) {
    // SAFETY: ptr is the valid WindowState pointer checked by the caller.
    let state = unsafe { &mut *ptr };
    state.record_shutdown_error(error);
    GdiRenderer::invalidate(hwnd);
}

fn record_paint_error_for_ptr(ptr: *mut WindowState, error: AppError) {
    // SAFETY: ptr is the valid WindowState pointer checked by the caller.
    let state = unsafe { &mut *ptr };
    state.record_paint_error(error);
}

fn window_state_ptr(hwnd: HWND) -> *mut WindowState {
    // SAFETY: reading GWLP_USERDATA is valid for any HWND owned by this process.
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    raw as *mut WindowState
}

fn cleanup_window_state(hwnd: HWND) {
    let ptr = window_state_ptr(hwnd);
    if ptr.is_null() {
        return;
    }

    // SAFETY: ptr was allocated by Box::into_raw; clearing prevents double free.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(ptr));
    }
}
