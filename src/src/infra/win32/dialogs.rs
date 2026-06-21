use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CLIP_DEFAULT_PRECIS, COLOR_WINDOW, DEFAULT_CHARSET, DEFAULT_GUI_FONT, DEFAULT_QUALITY,
    FF_MODERN, FIXED_PITCH, FW_NORMAL, GetDC, GetDeviceCaps, GetStockObject, LOGFONTW, LOGPIXELSY,
    OUT_DEFAULT_PRECIS, ReleaseDC,
};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CF_FIXEDPITCHONLY, CF_FORCEFONTEXIST, CF_INITTOLOGFONTSTRUCT, CF_LIMITSIZE, CF_NOVERTFONTS,
    CF_SCREENFONTS, CHOOSEFONTW, ChooseFontW, CommDlgExtendedError, GetOpenFileNameW, OFN_EXPLORER,
    OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Controls::EM_SETLIMITTEXT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::{
    BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW,
    SHGetPathFromIDListW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE,
    ES_READONLY, GWLP_USERDATA, GetDlgItem, GetMessageW, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, HMENU, IDC_ARROW, IsDialogMessageW, LoadCursorW,
    MB_ICONERROR, MB_OK, MSG, MessageBoxW, RegisterClassW, SW_SHOW, SendMessageW,
    SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_DESTROY, WM_NCDESTROY, WM_SETFONT, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_HSCROLL, WS_POPUP, WS_SYSMENU, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL,
};

use crate::domain::{
    ABOUT_FILE, ABOUT_TEXT, APP_DISPLAY_NAME, AUTHOR_PROFILE_URL, CommandArguments,
    CommandButtonDefinition, CommandCategoryDefinition, MAX_FONT_SIZE_POINTS, MIN_FONT_SIZE_POINTS,
    TerminalFont, application_version_label,
};
use crate::error::{AppError, AppResult};
use crate::infra::distribution;

use super::store_window_userdata;
use super::windowing::{current_instance, wide_null};

const EDITOR_CLASS_NAME: &str = "J3TermButtonEditorDialog";
const TEXT_INPUT_CLASS_NAME: &str = "J3TermTextInputDialog";
const ABOUT_CLASS_NAME: &str = "J3TermAboutDialog";

const BUTTON_EDITOR_WIDTH: i32 = 560;
const BUTTON_EDITOR_HEIGHT: i32 = 300;
const TEXT_INPUT_WIDTH: i32 = 420;
const TEXT_INPUT_HEIGHT: i32 = 150;
const ABOUT_WIDTH: i32 = 560;
const ABOUT_HEIGHT: i32 = 340;

const ID_BUTTON_LABEL: u16 = 2101;
const ID_BUTTON_EXECUTABLE: u16 = 2102;
const ID_BUTTON_ARGUMENTS: u16 = 2103;
const ID_BROWSE_EXECUTABLE: u16 = 2104;
const ID_INSERT_PATH: u16 = 2111;
const ID_INSERT_NAME: u16 = 2112;
const ID_INSERT_SELECT_FILE: u16 = 2113;
const ID_INSERT_SELECT_DIR: u16 = 2114;
const ID_INSERT_INPUT_TEXT: u16 = 2115;
const ID_SAVE: u16 = 2121;
const ID_CANCEL: u16 = 2122;

const ID_TEXT_VALUE: u16 = 2201;
const ID_TEXT_OK: u16 = 2202;
const ID_TEXT_CANCEL: u16 = 2203;

const ID_ABOUT_TEXT: u16 = 2301;
const ID_ABOUT_LINK: u16 = 2302;
const ID_ABOUT_OK: u16 = 2303;

const S_OK: i32 = 0;
const S_FALSE: i32 = 1;

pub(super) fn edit_command_button(
    owner: HWND,
    initial: &CommandButtonDefinition,
) -> AppResult<Option<CommandButtonDefinition>> {
    register_dialog_class(EDITOR_CLASS_NAME, Some(button_editor_proc))?;

    let state = Box::new(ButtonEditorState {
        initial: initial.clone(),
        result: RefCell::new(None),
        done: Cell::new(false),
    });
    let class_name = wide_null(EDITOR_CLASS_NAME);
    let title = wide_null("Edit Button");
    let (x, y) = centered_window_position(owner, BUTTON_EDITOR_WIDTH, BUTTON_EDITOR_HEIGHT);

    let _owner_guard = DisabledOwner::new(owner);
    // SAFETY: class, title, owner, and heap-owned state pointer are valid for this modal call.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            BUTTON_EDITOR_WIDTH,
            BUTTON_EDITOR_HEIGHT,
            owner,
            ptr::null_mut(),
            current_instance()?,
            (state.as_ref() as *const ButtonEditorState).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(AppError::win32("CreateWindowExW button editor"));
    }

    run_modal_loop(hwnd, || state.done.get())?;
    Ok(state.result.replace(None))
}

pub(super) fn prompt_input_text(owner: HWND) -> AppResult<Option<String>> {
    prompt_text(owner, "Input Text", "Text", "", TextInputValidation::None)
}

pub(super) fn prompt_new_category_name(owner: HWND, initial: &str) -> AppResult<Option<String>> {
    prompt_text(
        owner,
        "New Category",
        "Name",
        initial,
        TextInputValidation::CategoryName,
    )
}

pub(super) fn prompt_rename_category_name(owner: HWND, initial: &str) -> AppResult<Option<String>> {
    prompt_text(
        owner,
        "Rename Category",
        "Name",
        initial,
        TextInputValidation::CategoryName,
    )
}

pub(super) fn choose_terminal_font(
    owner: HWND,
    current: &TerminalFont,
) -> AppResult<Option<TerminalFont>> {
    let mut logfont = logfont_from_terminal_font(owner, current);
    let mut dialog = CHOOSEFONTW {
        lStructSize: size_of::<CHOOSEFONTW>() as u32,
        hwndOwner: owner,
        lpLogFont: &mut logfont,
        iPointSize: i32::from(current.size_points()).saturating_mul(10),
        Flags: CF_SCREENFONTS
            | CF_INITTOLOGFONTSTRUCT
            | CF_LIMITSIZE
            | CF_FORCEFONTEXIST
            | CF_FIXEDPITCHONLY
            | CF_NOVERTFONTS,
        nSizeMin: i32::from(MIN_FONT_SIZE_POINTS),
        nSizeMax: i32::from(MAX_FONT_SIZE_POINTS),
        ..CHOOSEFONTW::default()
    };

    // SAFETY: dialog points to initialized CHOOSEFONTW and logfont remains valid for the call.
    if unsafe { ChooseFontW(&mut dialog) } != 0 {
        return Ok(Some(terminal_font_from_logfont(
            &logfont,
            dialog.iPointSize,
        )?));
    }

    // SAFETY: reads the extended common-dialog error for the just-failed call.
    let extended_error = unsafe { CommDlgExtendedError() };
    if extended_error == 0 {
        Ok(None)
    } else {
        Err(AppError::ui_message(
            "ChooseFontW",
            format!("common font dialog error {extended_error}"),
        ))
    }
}

pub(super) fn show_about(owner: HWND) -> AppResult<()> {
    register_dialog_class(ABOUT_CLASS_NAME, Some(about_proc))?;

    let state = Box::new(AboutState {
        done: Cell::new(false),
    });
    let class_name = wide_null(ABOUT_CLASS_NAME);
    let title = format!("About {APP_DISPLAY_NAME}");
    let title = wide_null(&title);
    let (x, y) = centered_window_position(owner, ABOUT_WIDTH, ABOUT_HEIGHT);

    let _owner_guard = DisabledOwner::new(owner);
    // SAFETY: class, title, owner, and heap-owned state pointer are valid for this modal call.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            ABOUT_WIDTH,
            ABOUT_HEIGHT,
            owner,
            ptr::null_mut(),
            current_instance()?,
            (state.as_ref() as *const AboutState).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(AppError::win32("CreateWindowExW about dialog"));
    }

    run_modal_loop(hwnd, || state.done.get())
}

fn prompt_text(
    owner: HWND,
    title: &str,
    label: &str,
    initial: &str,
    validation: TextInputValidation,
) -> AppResult<Option<String>> {
    register_dialog_class(TEXT_INPUT_CLASS_NAME, Some(text_input_proc))?;

    let state = Box::new(TextInputState {
        label: label.to_owned(),
        initial: initial.to_owned(),
        validation,
        result: RefCell::new(None),
        done: Cell::new(false),
    });
    let class_name = wide_null(TEXT_INPUT_CLASS_NAME);
    let title = wide_null(title);
    let (x, y) = centered_window_position(owner, TEXT_INPUT_WIDTH, TEXT_INPUT_HEIGHT);

    let _owner_guard = DisabledOwner::new(owner);
    // SAFETY: class, title, owner, and heap-owned state pointer are valid for this modal call.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            TEXT_INPUT_WIDTH,
            TEXT_INPUT_HEIGHT,
            owner,
            ptr::null_mut(),
            current_instance()?,
            (state.as_ref() as *const TextInputState).cast(),
        )
    };
    if hwnd.is_null() {
        return Err(AppError::win32("CreateWindowExW input text dialog"));
    }

    run_modal_loop(hwnd, || state.done.get())?;
    Ok(state.result.replace(None))
}

pub(super) fn select_file(owner: HWND, title: &str) -> AppResult<Option<String>> {
    let mut buffer = vec![0_u16; 32_768];
    let title = wide_null(title);
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFile: buffer.as_mut_ptr(),
        nMaxFile: u32::try_from(buffer.len())
            .map_err(|_| AppError::InvalidInput("file dialog buffer is too large"))?,
        lpstrTitle: title.as_ptr(),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
        ..OPENFILENAMEW::default()
    };

    // SAFETY: dialog references valid writable buffers for the duration of the call.
    if unsafe { GetOpenFileNameW(&mut dialog) } != 0 {
        return Ok(Some(wide_buffer_to_string(&buffer)));
    }

    // SAFETY: reads the extended common-dialog error for the just-failed call.
    let extended_error = unsafe { CommDlgExtendedError() };
    if extended_error == 0 {
        Ok(None)
    } else {
        Err(AppError::ui_message(
            "GetOpenFileNameW",
            format!("common file dialog error {extended_error}"),
        ))
    }
}

pub(super) fn select_folder(owner: HWND) -> AppResult<Option<String>> {
    let _com_guard = ComApartmentGuard::initialize_for_shell_dialog()?;
    let title = wide_null("Select Folder");
    let browse = BROWSEINFOW {
        hwndOwner: owner,
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..BROWSEINFOW::default()
    };

    // SAFETY: browse contains valid fields and no callback.
    let item_id_list = unsafe { SHBrowseForFolderW(&browse) };
    if item_id_list.is_null() {
        return Ok(None);
    }

    let mut path = vec![0_u16; 32_768];
    // SAFETY: item_id_list was returned by SHBrowseForFolderW; path is writable.
    let ok = unsafe { SHGetPathFromIDListW(item_id_list, path.as_mut_ptr()) };
    // SAFETY: shell allocated the item id list with the COM task allocator.
    unsafe {
        CoTaskMemFree(item_id_list.cast::<c_void>());
    }

    if ok == 0 {
        Err(AppError::win32("SHGetPathFromIDListW"))
    } else {
        Ok(Some(wide_buffer_to_string(&path)))
    }
}

struct ButtonEditorState {
    initial: CommandButtonDefinition,
    result: RefCell<Option<CommandButtonDefinition>>,
    done: Cell<bool>,
}

struct TextInputState {
    label: String,
    initial: String,
    validation: TextInputValidation,
    result: RefCell<Option<String>>,
    done: Cell<bool>,
}

struct AboutState {
    done: Cell<bool>,
}

#[derive(Clone, Copy)]
enum TextInputValidation {
    None,
    CategoryName,
}

#[derive(Clone, Copy)]
enum ComApartmentState {
    InitializedByGuard,
    AlreadyInitialized,
}

struct ComApartmentGuard {
    state: ComApartmentState,
}

impl ComApartmentGuard {
    fn initialize_for_shell_dialog() -> AppResult<Self> {
        let flags = (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32;
        // SAFETY: null reserved pointer is required; shell folder dialogs need STA COM.
        let result = unsafe { CoInitializeEx(ptr::null(), flags) };

        match result {
            S_OK => Ok(Self {
                state: ComApartmentState::InitializedByGuard,
            }),
            S_FALSE => Ok(Self {
                state: ComApartmentState::AlreadyInitialized,
            }),
            result if result < 0 => Err(AppError::ui_message(
                "CoInitializeEx folder dialog",
                format!(
                    "COM initialization failed with HRESULT 0x{:08X}",
                    result as u32
                ),
            )),
            _ => Ok(Self {
                state: ComApartmentState::AlreadyInitialized,
            }),
        }
    }
}

impl Drop for ComApartmentGuard {
    fn drop(&mut self) {
        match self.state {
            ComApartmentState::InitializedByGuard | ComApartmentState::AlreadyInitialized => {
                // SAFETY: every successful CoInitializeEx call, including S_FALSE, is balanced.
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ControlBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl ControlBounds {
    fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

struct DisabledOwner {
    hwnd: HWND,
}

impl DisabledOwner {
    fn new(hwnd: HWND) -> Self {
        if !hwnd.is_null() {
            // SAFETY: hwnd is the modal owner and can be disabled during the modal loop.
            unsafe {
                EnableWindow(hwnd, 0);
            }
        }

        Self { hwnd }
    }
}

impl Drop for DisabledOwner {
    fn drop(&mut self) {
        if self.hwnd.is_null() {
            return;
        }

        // SAFETY: hwnd is the modal owner disabled by this guard.
        unsafe {
            EnableWindow(self.hwnd, 1);
            SetFocus(self.hwnd);
        }
    }
}

unsafe extern "system" fn button_editor_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            let create =
                lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            if create.is_null() {
                return -1;
            }

            // SAFETY: WM_CREATE lparam is a valid CREATESTRUCTW for this window.
            let state = unsafe { (*create).lpCreateParams as *const ButtonEditorState };
            if state.is_null() {
                return -1;
            }

            if store_window_userdata(hwnd, state as isize).is_err() {
                return -1;
            }

            // SAFETY: state is the non-null pointer passed through CreateWindowExW for this call.
            let state = unsafe { &*state };
            if create_button_editor_controls(hwnd, state).is_err() {
                return -1;
            }
            0
        }
        WM_COMMAND => {
            let id = loword(wparam);
            handle_button_editor_command(hwnd, id);
            0
        }
        WM_CLOSE => {
            finish_button_editor(hwnd, None);
            0
        }
        WM_DESTROY => {
            mark_button_editor_done(hwnd);
            0
        }
        WM_NCDESTROY => {
            clear_window_state(hwnd);
            0
        }
        _ => {
            // SAFETY: default processing for unhandled window messages.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

unsafe extern "system" fn text_input_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            let create =
                lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            if create.is_null() {
                return -1;
            }

            // SAFETY: WM_CREATE lparam is a valid CREATESTRUCTW for this window.
            let state = unsafe { (*create).lpCreateParams as *const TextInputState };
            if state.is_null() {
                return -1;
            }

            if store_window_userdata(hwnd, state as isize).is_err() {
                return -1;
            }

            // SAFETY: state is the non-null pointer passed through CreateWindowExW for this call.
            let state = unsafe { &*state };
            if create_text_input_controls(hwnd, state).is_err() {
                return -1;
            }
            0
        }
        WM_COMMAND => {
            let id = loword(wparam);
            handle_text_input_command(hwnd, id);
            0
        }
        WM_CLOSE => {
            finish_text_input(hwnd, None);
            0
        }
        WM_DESTROY => {
            mark_text_input_done(hwnd);
            0
        }
        WM_NCDESTROY => {
            clear_window_state(hwnd);
            0
        }
        _ => {
            // SAFETY: default processing for unhandled window messages.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

unsafe extern "system" fn about_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CREATE => {
            let create =
                lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            if create.is_null() {
                return -1;
            }

            // SAFETY: WM_CREATE lparam is a valid CREATESTRUCTW for this window.
            let state = unsafe { (*create).lpCreateParams as *const AboutState };
            if state.is_null() {
                return -1;
            }

            if store_window_userdata(hwnd, state as isize).is_err() {
                return -1;
            }

            if create_about_controls(hwnd).is_err() {
                return -1;
            }
            0
        }
        WM_COMMAND => {
            let id = loword(wparam);
            handle_about_command(hwnd, id);
            0
        }
        WM_CLOSE => {
            finish_about(hwnd);
            0
        }
        WM_DESTROY => {
            mark_about_done(hwnd);
            0
        }
        WM_NCDESTROY => {
            clear_window_state(hwnd);
            0
        }
        _ => {
            // SAFETY: default processing for unhandled window messages.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn create_button_editor_controls(hwnd: HWND, state: &ButtonEditorState) -> AppResult<()> {
    create_label(hwnd, "Button Name", 18, 20, 110, 22)?;
    create_edit(
        hwnd,
        ID_BUTTON_LABEL,
        &state.initial.label,
        132,
        18,
        390,
        24,
    )?;
    create_label(hwnd, "Executable", 18, 58, 110, 22)?;
    create_edit(
        hwnd,
        ID_BUTTON_EXECUTABLE,
        &state.initial.executable_path,
        132,
        56,
        306,
        24,
    )?;
    create_button(hwnd, ID_BROWSE_EXECUTABLE, "Browse...", 446, 55, 76, 26)?;
    create_label(hwnd, "Arguments", 18, 96, 110, 22)?;
    create_edit(
        hwnd,
        ID_BUTTON_ARGUMENTS,
        state.initial.arguments.value(),
        132,
        94,
        390,
        24,
    )?;
    create_label(hwnd, "Insert Token", 18, 136, 110, 22)?;
    create_button(hwnd, ID_INSERT_PATH, "{path}", 132, 132, 70, 28)?;
    create_button(hwnd, ID_INSERT_NAME, "{name}", 208, 132, 70, 28)?;
    create_button(
        hwnd,
        ID_INSERT_SELECT_FILE,
        "{selectfile}",
        284,
        132,
        96,
        28,
    )?;
    create_button(hwnd, ID_INSERT_SELECT_DIR, "{selectdir}", 386, 132, 88, 28)?;
    create_button(hwnd, ID_INSERT_INPUT_TEXT, "{inputtext}", 132, 166, 100, 28)?;
    create_label(
        hwnd,
        "{path}: current terminal path, {name}: last path segment",
        242,
        170,
        280,
        22,
    )?;
    create_button(hwnd, ID_SAVE, "Save", 344, 224, 84, 30)?;
    create_button(hwnd, ID_CANCEL, "Cancel", 438, 224, 84, 30)?;
    focus_control(hwnd, ID_BUTTON_LABEL);
    Ok(())
}

fn create_text_input_controls(hwnd: HWND, state: &TextInputState) -> AppResult<()> {
    create_label(hwnd, &state.label, 18, 22, 96, 22)?;
    create_edit(hwnd, ID_TEXT_VALUE, &state.initial, 124, 20, 262, 24)?;
    create_button(hwnd, ID_TEXT_OK, "OK", 210, 76, 80, 30)?;
    create_button(hwnd, ID_TEXT_CANCEL, "Cancel", 306, 76, 80, 30)?;
    focus_control(hwnd, ID_TEXT_VALUE);
    Ok(())
}

fn create_about_controls(hwnd: HWND) -> AppResult<()> {
    let version_label = application_version_label();
    let about_text = distribution::load_text_file_or_embedded(ABOUT_FILE, ABOUT_TEXT);
    create_label(hwnd, &version_label, 18, 18, 504, 22)?;
    create_readonly_multiline_edit(
        hwnd,
        ID_ABOUT_TEXT,
        &about_text.replace('\n', "\r\n"),
        18,
        46,
        504,
        188,
    )?;
    create_button(hwnd, ID_ABOUT_LINK, AUTHOR_PROFILE_URL, 18, 250, 230, 30)?;
    create_button(hwnd, ID_ABOUT_OK, "OK", 452, 250, 70, 30)?;
    focus_control(hwnd, ID_ABOUT_OK);
    Ok(())
}

fn handle_button_editor_command(hwnd: HWND, id: u16) {
    match id {
        ID_BROWSE_EXECUTABLE => match select_file(hwnd, "Select Executable") {
            Ok(Some(path)) => set_control_text(hwnd, ID_BUTTON_EXECUTABLE, &path),
            Ok(None) => {}
            Err(error) => show_error(hwnd, error.user_message()),
        },
        ID_INSERT_PATH => append_argument_token(hwnd, "{path}"),
        ID_INSERT_NAME => append_argument_token(hwnd, "{name}"),
        ID_INSERT_SELECT_FILE => append_argument_token(hwnd, "{selectfile}"),
        ID_INSERT_SELECT_DIR => append_argument_token(hwnd, "{selectdir}"),
        ID_INSERT_INPUT_TEXT => append_argument_token(hwnd, "{inputtext}"),
        ID_SAVE => match read_button_definition(hwnd) {
            Ok(definition) => finish_button_editor(hwnd, Some(definition)),
            Err(error) => show_error(hwnd, error.user_message()),
        },
        ID_CANCEL => finish_button_editor(hwnd, None),
        _ => {}
    }
}

fn handle_text_input_command(hwnd: HWND, id: u16) {
    match id {
        ID_TEXT_OK => {
            let value = control_text(hwnd, ID_TEXT_VALUE);
            match validate_text_input(hwnd, &value) {
                Ok(()) => finish_text_input(hwnd, Some(value)),
                Err(error) => show_error(hwnd, error.user_message()),
            }
        }
        ID_TEXT_CANCEL => finish_text_input(hwnd, None),
        _ => {}
    }
}

fn handle_about_command(hwnd: HWND, id: u16) {
    match id {
        ID_ABOUT_LINK => {
            if let Err(error) = open_author_profile(hwnd) {
                show_error(hwnd, error.user_message());
            }
        }
        ID_ABOUT_OK => finish_about(hwnd),
        _ => {}
    }
}

fn open_author_profile(owner: HWND) -> AppResult<()> {
    let operation = wide_null("open");
    let url = wide_null(AUTHOR_PROFILE_URL);
    // SAFETY: operation and URL are valid null-terminated UTF-16 buffers for this call.
    let result = unsafe {
        ShellExecuteW(
            owner,
            operation.as_ptr(),
            url.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOW,
        )
    };

    let code = result as isize;
    if code <= 32 {
        Err(AppError::ui_message(
            "ShellExecuteW author profile",
            format!("ShellExecuteW returned {code}"),
        ))
    } else {
        Ok(())
    }
}

fn read_button_definition(hwnd: HWND) -> AppResult<CommandButtonDefinition> {
    let label = control_text(hwnd, ID_BUTTON_LABEL);
    let executable_path = control_text(hwnd, ID_BUTTON_EXECUTABLE);
    let arguments = CommandArguments::new(control_text(hwnd, ID_BUTTON_ARGUMENTS))?;
    CommandButtonDefinition::new(label, executable_path, arguments)
}

fn validate_text_input(hwnd: HWND, value: &str) -> AppResult<()> {
    with_text_input_state(hwnd, |state| match state.validation {
        TextInputValidation::None => Ok(()),
        TextInputValidation::CategoryName => {
            CommandCategoryDefinition::new(value.to_owned(), Vec::new()).map(|_| ())
        }
    })
    .unwrap_or(Ok(()))
}

fn finish_button_editor(hwnd: HWND, result: Option<CommandButtonDefinition>) {
    with_button_editor_state(hwnd, |state| {
        state.result.replace(result);
        state.done.set(true);
    });

    destroy_modal_window(hwnd);
}

fn finish_text_input(hwnd: HWND, result: Option<String>) {
    with_text_input_state(hwnd, |state| {
        state.result.replace(result);
        state.done.set(true);
    });

    destroy_modal_window(hwnd);
}

fn finish_about(hwnd: HWND) {
    with_about_state(hwnd, |state| {
        state.done.set(true);
    });

    destroy_modal_window(hwnd);
}

fn append_argument_token(hwnd: HWND, token: &str) {
    let mut arguments = control_text(hwnd, ID_BUTTON_ARGUMENTS);
    if !arguments.is_empty() && !arguments.ends_with(char::is_whitespace) {
        arguments.push(' ');
    }
    arguments.push_str(token);
    set_control_text(hwnd, ID_BUTTON_ARGUMENTS, &arguments);
    focus_control(hwnd, ID_BUTTON_ARGUMENTS);
}

fn create_label(
    parent: HWND,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> AppResult<HWND> {
    create_child(
        parent,
        "STATIC",
        text,
        0,
        0,
        ControlBounds::new(x, y, width, height),
    )
}

fn create_edit(
    parent: HWND,
    id: u16,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> AppResult<HWND> {
    create_child(
        parent,
        "EDIT",
        text,
        WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
        id,
        ControlBounds::new(x, y, width, height),
    )
}

fn create_readonly_multiline_edit(
    parent: HWND,
    id: u16,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> AppResult<HWND> {
    let hwnd = create_child(
        parent,
        "EDIT",
        "",
        WS_TABSTOP
            | WS_BORDER
            | WS_VSCROLL
            | WS_HSCROLL
            | ES_MULTILINE as u32
            | ES_AUTOVSCROLL as u32
            | ES_AUTOHSCROLL as u32
            | ES_READONLY as u32,
        id,
        ControlBounds::new(x, y, width, height),
    )?;

    let limit = text.encode_utf16().count().saturating_add(1);
    // SAFETY: hwnd is a live edit control; EM_SETLIMITTEXT accepts the new UTF-16 text limit.
    unsafe {
        SendMessageW(hwnd, EM_SETLIMITTEXT, limit, 0);
    }

    let text = wide_null(text);
    // SAFETY: hwnd is a live edit control and text is valid null-terminated UTF-16.
    unsafe {
        SetWindowTextW(hwnd, text.as_ptr());
    }

    Ok(hwnd)
}

fn create_button(
    parent: HWND,
    id: u16,
    text: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> AppResult<HWND> {
    let button_style = if id == ID_SAVE || id == ID_TEXT_OK || id == ID_ABOUT_OK {
        BS_DEFPUSHBUTTON
    } else {
        BS_PUSHBUTTON
    };
    create_child(
        parent,
        "BUTTON",
        text,
        WS_TABSTOP | button_style as u32,
        id,
        ControlBounds::new(x, y, width, height),
    )
}

fn create_child(
    parent: HWND,
    class_name: &str,
    text: &str,
    style: u32,
    id: u16,
    bounds: ControlBounds,
) -> AppResult<HWND> {
    let class_name = wide_null(class_name);
    let text = wide_null(text);
    // SAFETY: parent is a live dialog; class and text live for the call.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            text.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            parent,
            control_menu(id),
            current_instance()?,
            ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err(AppError::win32("CreateWindowExW dialog child"));
    }

    set_default_font(hwnd);
    Ok(hwnd)
}

fn set_default_font(hwnd: HWND) {
    // SAFETY: DEFAULT_GUI_FONT is a shared stock object and WM_SETFONT accepts it.
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    if font.is_null() {
        return;
    }

    // SAFETY: hwnd is a live child window; font is a shared stock font.
    unsafe {
        SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1);
    }
}

fn focus_control(parent: HWND, id: u16) {
    let control = get_control(parent, id);
    if control.is_null() {
        return;
    }

    // SAFETY: control is a live child window.
    unsafe {
        SetFocus(control);
    }
}

fn get_control(parent: HWND, id: u16) -> HWND {
    // SAFETY: parent is a live dialog; missing controls return null.
    unsafe { GetDlgItem(parent, i32::from(id)) }
}

fn control_text(parent: HWND, id: u16) -> String {
    let hwnd = get_control(parent, id);
    if hwnd.is_null() {
        return String::new();
    }

    // SAFETY: hwnd is a live control.
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }

    let mut buffer = vec![0_u16; usize::try_from(length).unwrap_or(0).saturating_add(1)];
    // SAFETY: buffer is valid writable UTF-16 storage including null terminator.
    let copied = unsafe {
        GetWindowTextW(
            hwnd,
            buffer.as_mut_ptr(),
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
        )
    };
    if copied <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buffer[..usize::try_from(copied).unwrap_or(0)])
    }
}

fn set_control_text(parent: HWND, id: u16, text: &str) {
    let hwnd = get_control(parent, id);
    if hwnd.is_null() {
        return;
    }

    let text = wide_null(text);
    // SAFETY: hwnd is a live control; text is valid for this call.
    unsafe {
        SetWindowTextW(hwnd, text.as_ptr());
    }
}

fn show_error(hwnd: HWND, message: &str) {
    let message = wide_null(message);
    let title = wide_null("j3Term");
    // SAFETY: strings are valid null-terminated UTF-16 buffers for this call.
    unsafe {
        MessageBoxW(hwnd, message.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
    }
}

fn run_modal_loop(hwnd: HWND, done: impl Fn() -> bool) -> AppResult<()> {
    // SAFETY: hwnd is a newly created modal dialog.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
    }

    let mut message_storage = MaybeUninit::<MSG>::zeroed();
    while !done() {
        // SAFETY: message points to writable storage and filters are not used.
        let result = unsafe { GetMessageW(message_storage.as_mut_ptr(), ptr::null_mut(), 0, 0) };
        if result == -1 {
            let error = AppError::win32("GetMessageW modal dialog");
            destroy_modal_window(hwnd);
            return Err(error);
        }
        if result == 0 {
            // SAFETY: GetMessageW returned 0 after retrieving WM_QUIT and initialized MSG.
            let quit_message = unsafe { message_storage.assume_init() };
            if !done() {
                destroy_modal_window(hwnd);
            }
            // SAFETY: re-posting WM_QUIT on the UI thread lets the outer loop observe shutdown.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostQuitMessage(
                    quit_message.wParam as i32,
                );
            }
            break;
        }

        // SAFETY: GetMessageW returned a positive value and initialized MSG.
        let message = unsafe { message_storage.assume_init() };
        // SAFETY: hwnd is a dialog-like window; message is initialized.
        if unsafe { IsDialogMessageW(hwnd, &message) } == 0 {
            // SAFETY: dispatching initialized MSG from GetMessageW.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        message_storage = MaybeUninit::<MSG>::zeroed();
    }

    Ok(())
}

fn register_dialog_class(
    class_name: &str,
    window_proc: windows_sys::Win32::UI::WindowsAndMessaging::WNDPROC,
) -> AppResult<()> {
    let class_name = wide_null(class_name);
    // SAFETY: requesting a predefined system cursor with a null instance is valid.
    let cursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    let window_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: window_proc,
        hInstance: current_instance()?,
        hCursor: cursor,
        hbrBackground: (COLOR_WINDOW + 1) as _,
        lpszClassName: class_name.as_ptr(),
        ..WNDCLASSW::default()
    };

    // SAFETY: WNDCLASSW contains valid pointers for the duration of this call.
    let atom = unsafe { RegisterClassW(&window_class) };
    if atom != 0 {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(1410) {
        Ok(())
    } else {
        Err(AppError::win32("RegisterClassW modal dialog"))
    }
}

fn centered_window_position(owner: HWND, width: i32, height: i32) -> (i32, i32) {
    if owner.is_null() {
        return (CW_USEDEFAULT, CW_USEDEFAULT);
    }

    let mut rect = RECT::default();
    // SAFETY: owner is a window handle from this UI thread.
    if unsafe { GetWindowRect(owner, &mut rect) } == 0 {
        return (CW_USEDEFAULT, CW_USEDEFAULT);
    }

    let owner_width = rect.right.saturating_sub(rect.left);
    let owner_height = rect.bottom.saturating_sub(rect.top);
    (
        rect.left + owner_width.saturating_sub(width) / 2,
        rect.top + owner_height.saturating_sub(height) / 2,
    )
}

fn logfont_from_terminal_font(owner: HWND, font: &TerminalFont) -> LOGFONTW {
    let mut logfont = LOGFONTW {
        lfHeight: point_size_to_logical_height(font.size_points(), dialog_dpi_y(owner)),
        lfWeight: FW_NORMAL as i32,
        lfCharSet: DEFAULT_CHARSET,
        lfOutPrecision: OUT_DEFAULT_PRECIS,
        lfClipPrecision: CLIP_DEFAULT_PRECIS,
        lfQuality: DEFAULT_QUALITY,
        lfPitchAndFamily: FIXED_PITCH | FF_MODERN,
        ..LOGFONTW::default()
    };
    set_logfont_face_name(&mut logfont, font.family());
    logfont
}

fn terminal_font_from_logfont(logfont: &LOGFONTW, point_size: i32) -> AppResult<TerminalFont> {
    let family = wide_buffer_to_string(&logfont.lfFaceName);
    let size = font_size_from_dialog_point_size(point_size)?;
    TerminalFont::new(family, size)
}

fn font_size_from_dialog_point_size(point_size: i32) -> AppResult<u16> {
    let rounded = point_size.saturating_add(5) / 10;
    let size = u16::try_from(rounded)
        .map_err(|_| AppError::InvalidInput("font size is outside supported range"))?;
    TerminalFont::new("monospace", size).map(|font| font.size_points())
}

fn set_logfont_face_name(logfont: &mut LOGFONTW, family: &str) {
    logfont.lfFaceName = [0; 32];
    let max_len = logfont.lfFaceName.len().saturating_sub(1);
    for (slot, unit) in logfont
        .lfFaceName
        .iter_mut()
        .take(max_len)
        .zip(family.encode_utf16())
    {
        *slot = unit;
    }
}

fn dialog_dpi_y(owner: HWND) -> i32 {
    if owner.is_null() {
        return 96;
    }

    // SAFETY: owner is a live window on the UI thread; ReleaseDC balances a non-null DC.
    unsafe {
        let hdc = GetDC(owner);
        if hdc.is_null() {
            return 96;
        }
        let dpi_y = GetDeviceCaps(hdc, LOGPIXELSY as i32).max(96);
        ReleaseDC(owner, hdc);
        dpi_y
    }
}

fn point_size_to_logical_height(size_points: u16, dpi_y: i32) -> i32 {
    let pixels = i32::from(size_points)
        .saturating_mul(dpi_y.max(1))
        .saturating_add(36)
        / 72;
    pixels.max(1).saturating_neg()
}

fn wide_buffer_to_string(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn mark_button_editor_done(hwnd: HWND) {
    with_button_editor_state(hwnd, |state| state.done.set(true));
}

fn mark_text_input_done(hwnd: HWND) {
    with_text_input_state(hwnd, |state| state.done.set(true));
}

fn mark_about_done(hwnd: HWND) {
    with_about_state(hwnd, |state| state.done.set(true));
}

fn with_button_editor_state<R>(hwnd: HWND, f: impl FnOnce(&ButtonEditorState) -> R) -> Option<R> {
    let ptr = window_state_ptr::<ButtonEditorState>(hwnd);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: pointer is cleared before the modal caller can drop the heap-owned state.
        Some(f(unsafe { &*ptr }))
    }
}

fn with_text_input_state<R>(hwnd: HWND, f: impl FnOnce(&TextInputState) -> R) -> Option<R> {
    let ptr = window_state_ptr::<TextInputState>(hwnd);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: pointer is cleared before the modal caller can drop the heap-owned state.
        Some(f(unsafe { &*ptr }))
    }
}

fn with_about_state<R>(hwnd: HWND, f: impl FnOnce(&AboutState) -> R) -> Option<R> {
    let ptr = window_state_ptr::<AboutState>(hwnd);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: pointer is cleared before the modal caller can drop the heap-owned state.
        Some(f(unsafe { &*ptr }))
    }
}

fn destroy_modal_window(hwnd: HWND) {
    clear_window_state(hwnd);
    // SAFETY: hwnd is the modal dialog handle owned by this modal loop.
    unsafe {
        DestroyWindow(hwnd);
    }
}

fn clear_window_state(hwnd: HWND) {
    // SAFETY: clearing GWLP_USERDATA is valid for this dialog HWND during teardown.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }
}

fn window_state_ptr<T>(hwnd: HWND) -> *const T {
    // SAFETY: reading GWLP_USERDATA is valid for a live HWND.
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const T }
}

fn loword(value: WPARAM) -> u16 {
    (value & 0xffff) as u16
}

fn control_menu(control_id: u16) -> HMENU {
    usize::from(control_id) as HMENU
}
