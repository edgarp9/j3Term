mod clipboard;
mod controls;
mod dialogs;
mod dispatch;
mod input;
mod menus;
mod resources;
mod windowing;

use std::cell::RefCell;
use std::ops::Range;
use std::ptr;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{GetLastError, HWND, RECT, SetLastError, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CW_USEDEFAULT, CreateWindowExW, DestroyWindow, GWLP_USERDATA, IDC_SIZEWE,
    KillTimer, LoadCursorW, SetCursor, SetTimer, SetWindowLongPtrW, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};

use crate::app::{TerminalTabs, TerminalTimerDrain, startup_window_size_message};
#[cfg(test)]
use crate::domain::default_command_panel;
use crate::domain::layout::{COMMAND_PANEL_WIDTH, terminal_content_area};
use crate::domain::terminal::TerminalChangedRows;
use crate::domain::{
    ButtonArgumentValues, CommandArguments, CommandButton, CommandButtonDefinition,
    CommandButtonId, CommandPanel, CommandText, DEFAULT_COLUMNS, DEFAULT_ROWS, StartupInvocation,
    TerminalCommand, TerminalFont, TerminalGridPoint, TerminalInput, TerminalKeyModifiers,
    TerminalScroll, TerminalSelection, TerminalSize, TerminalTabView, TerminalViewport, UiPoint,
    UiRect, WindowLayout,
};
use crate::error::{AppError, AppResult};
use crate::infra::config::{AppSettings, ConfigStore};
use crate::infra::pty::{PortablePtySession, join_detached_cleanup_tasks};
use crate::infra::renderer::{CellMetrics, GdiRenderer};
use crate::infra::terminal::AlacrittyTerminalBuffer;

use self::controls::{
    CommandButtonScrollRequest, CommandPanelControls, TerminalScrollBarControl,
    TerminalScrollBarRequest,
};
use self::input::InputMapper;
use self::menus::{ButtonMenuState, CategoryMenuState};
use self::resources::WindowIcons;
use self::windowing::{
    client_rect, current_instance, destroy_window_if_alive, focus_main_window, message_loop,
    register_window_class, show_main_window, wide_null,
};

const TIMER_ID: usize = 1;
const PTY_ACTIVE_TIMER_MS: u32 = 33;
const PTY_IDLE_TIMER_MS: u32 = 100;
const PTY_IDLE_TIMER_BACKOFF_TICKS: u8 = 10;
const PTY_SUSTAINED_IDLE_TIMER_MS: u32 = 250;
const PTY_SUSTAINED_IDLE_TIMER_BACKOFF_TICKS: u8 = 20;
const COMMAND_PANEL_SAVE_DELAY_MS: u64 = 300;
const WINDOW_WIDTH: i32 = 750;
const WINDOW_HEIGHT: i32 = 520;
const CTRL_C_CHAR: u16 = 0x03;
const CTRL_V_CHAR: u16 = 0x16;
const KEY_C: u16 = b'C' as u16;
const KEY_V: u16 = b'V' as u16;

type ShutdownErrorSink = Rc<RefCell<Option<String>>>;
type SharedCommandPanel = Rc<CommandPanel>;

enum ContextMenuRequest {
    Category {
        point: UiPoint,
        state: CategoryMenuState,
    },
    Button {
        button_id: CommandButtonId,
        point: UiPoint,
        state: ButtonMenuState,
    },
}

struct ButtonCommandPrompt {
    button: CommandButton,
}

struct PendingButtonCommand {
    button: CommandButton,
    values: ButtonArgumentValues,
}

impl ButtonCommandPrompt {
    fn collect_values(self, hwnd: HWND) -> AppResult<Option<PendingButtonCommand>> {
        let Some(values) = collect_button_argument_values(hwnd, &self.button)? else {
            return Ok(None);
        };

        Ok(Some(PendingButtonCommand {
            button: self.button,
            values,
        }))
    }
}

pub fn run(startup: StartupInvocation) -> AppResult<()> {
    let class_name = wide_null("J3TermWindow");
    let window_title = wide_null("j3Term");
    let instance = current_instance()?;
    register_window_class(instance, &class_name, Some(dispatch::window_proc))?;

    let initial_size = TerminalSize::new(DEFAULT_ROWS, DEFAULT_COLUMNS)?;
    let shutdown_error = new_shutdown_error_sink();
    let state = Box::new(WindowState::new(
        initial_size,
        startup,
        Rc::clone(&shutdown_error),
    )?);
    let state_ptr = Box::into_raw(state);
    let (window_width, window_height) = window_size_for_client_size(WINDOW_WIDTH, WINDOW_HEIGHT)?;

    // SAFETY: class name, title, and instance are valid for the duration of the call.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            window_width,
            window_height,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            state_ptr.cast(),
        )
    };

    if hwnd.is_null() {
        let startup_error = with_raw_window_state(state_ptr, WindowState::take_last_error)
            .ok()
            .flatten();
        drop_unstored_window_state(state_ptr);
        return match startup_error {
            Some(message) => Err(AppError::ui_message("create main window", message)),
            None => Err(AppError::win32("CreateWindowExW")),
        };
    }

    let startup_error = with_raw_window_state(state_ptr, WindowState::take_last_error)?;
    if let Some(message) = startup_error {
        // SAFETY: hwnd is a valid window returned by CreateWindowExW.
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(AppError::ui_message("initialize main window", message));
    }

    let start_result = with_raw_window_state(state_ptr, |state| state.start(hwnd))?;
    if let Err(error) = start_result {
        // SAFETY: hwnd is a valid window returned by CreateWindowExW.
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(error);
    }

    show_main_window(hwnd);

    if let Err(error) = with_raw_window_state(state_ptr, |state| state.start_timer(hwnd))? {
        // SAFETY: hwnd is a valid window returned by CreateWindowExW.
        unsafe {
            DestroyWindow(hwnd);
        }
        return Err(error);
    }

    let loop_result = message_loop(hwnd);

    let destroy_result = destroy_window_if_alive(hwnd);
    let shutdown_result = take_shutdown_error_result(&shutdown_error);
    let cleanup_errors = join_detached_cleanup_tasks();

    finish_shutdown_result(loop_result, destroy_result, shutdown_result, cleanup_errors)
}

fn window_size_for_client_size(width: i32, height: i32) -> AppResult<(i32, i32)> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width.max(1),
        bottom: height.max(1),
    };

    // SAFETY: rect points to valid writable storage and the style flags match CreateWindowExW.
    let adjusted = unsafe { AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0) };
    if adjusted == 0 {
        return Err(AppError::win32("AdjustWindowRectEx"));
    }

    Ok((
        rect.right.saturating_sub(rect.left).max(1),
        rect.bottom.saturating_sub(rect.top).max(1),
    ))
}

fn finish_shutdown_result(
    loop_result: AppResult<()>,
    destroy_result: AppResult<()>,
    shutdown_result: AppResult<()>,
    cleanup_errors: Vec<AppError>,
) -> AppResult<()> {
    let mut errors = Vec::new();
    // Preserve caller-visible shutdown priority: message loop, window destroy,
    // window state shutdown, then detached PTY cleanup.
    if let Err(error) = loop_result {
        errors.push(error);
    }
    if let Err(error) = destroy_result {
        errors.push(error);
    }
    if let Err(error) = shutdown_result {
        errors.push(error);
    }
    errors.extend(cleanup_errors);

    let mut errors = errors.into_iter();
    let Some(first_error) = errors.next() else {
        return Ok(());
    };
    let Some(second_error) = errors.next() else {
        return Err(first_error);
    };

    let mut message = first_error.to_string();
    message.push_str("; ");
    message.push_str(&second_error.to_string());
    for error in errors {
        message.push_str("; ");
        message.push_str(&error.to_string());
    }

    Err(AppError::pty_message("shutdown application", message))
}

fn new_shutdown_error_sink() -> ShutdownErrorSink {
    Rc::new(RefCell::new(None))
}

fn take_shutdown_error_result(shutdown_error: &ShutdownErrorSink) -> AppResult<()> {
    let message = {
        let mut shutdown_error = shutdown_error
            .try_borrow_mut()
            .map_err(|_| AppError::InvalidState("shutdown error state is already borrowed"))?;
        shutdown_error.take()
    };

    match message {
        Some(message) => Err(AppError::ui_message("shutdown window", message)),
        None => Ok(()),
    }
}

fn with_raw_window_state<R>(
    state_ptr: *mut WindowState,
    operation: impl FnOnce(&mut WindowState) -> R,
) -> AppResult<R> {
    let mut state_ptr =
        NonNull::new(state_ptr).ok_or(AppError::InvalidState("window state pointer is null"))?;
    // SAFETY: the pointer was created by Box::into_raw in run(); this helper is used only on the
    // UI thread while the window owns the Box and before WM_NCDESTROY clears GWLP_USERDATA.
    Ok(operation(unsafe { state_ptr.as_mut() }))
}

fn drop_unstored_window_state(state_ptr: *mut WindowState) {
    let Some(state_ptr) = NonNull::new(state_ptr) else {
        return;
    };

    // SAFETY: this path is used only when CreateWindowExW failed before the pointer was stored in
    // GWLP_USERDATA, so Box ownership still belongs to run().
    unsafe {
        drop(Box::from_raw(state_ptr.as_ptr()));
    }
}

fn store_window_userdata(hwnd: HWND, value: isize) -> AppResult<()> {
    // SAFETY: GWLP_USERDATA is a per-window pointer-sized slot. Last error is cleared first
    // because a successful SetWindowLongPtrW call can legitimately return the previous value 0.
    unsafe {
        SetLastError(0);
        let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, value);
        if previous == 0 && GetLastError() != 0 {
            return Err(AppError::win32("SetWindowLongPtrW GWLP_USERDATA"));
        }
    }

    Ok(())
}

struct WindowState {
    session: WindowSessionCoordinator,
    view: WindowViewAdapters,
    runtime: WindowRuntimeState,
    last_error: WindowErrorState,
    shutdown_error: ShutdownErrorSink,
}

struct WindowSessionCoordinator {
    tabs: TerminalTabs<PortablePtySession, AlacrittyTerminalBuffer>,
    command_panel_state: CommandPanelSessionState,
    startup: StartupInvocation,
    #[cfg(test)]
    terminal_command_overrides: Vec<TerminalCommandOverride>,
    #[cfg(test)]
    terminal_viewport_refresh_overrides: Vec<TerminalViewportRefreshOverride>,
    #[cfg(test)]
    terminal_resize_commands: Vec<TerminalSize>,
}

#[cfg(test)]
enum TerminalCommandOverride {
    Fail(&'static str),
}

#[cfg(test)]
enum TerminalViewportRefreshOverride {
    Fail(&'static str),
}

struct CommandPanelSessionState {
    current: CommandPanel,
    terminal_font: TerminalFont,
    config_store: Option<ConfigStore>,
    selection_save_pending: bool,
    command_panel_save_pending: Option<PendingCommandPanelSave>,
}

#[derive(Clone)]
struct CommandPanelSessionSnapshot {
    current: CommandPanel,
    selection_save_pending: bool,
}

#[derive(Clone)]
struct PendingCommandPanelSave {
    rollback_current: CommandPanel,
    rollback_selection_save_pending: bool,
    due_at: Instant,
}

impl CommandPanelSessionState {
    fn new(
        current: CommandPanel,
        terminal_font: TerminalFont,
        config_store: Option<ConfigStore>,
    ) -> Self {
        Self {
            current,
            terminal_font,
            config_store,
            selection_save_pending: false,
            command_panel_save_pending: None,
        }
    }

    fn current(&self) -> &CommandPanel {
        &self.current
    }

    fn current_mut(&mut self) -> &mut CommandPanel {
        &mut self.current
    }

    fn current_handle(&self) -> SharedCommandPanel {
        Rc::new(self.current.clone())
    }

    fn terminal_font(&self) -> &TerminalFont {
        &self.terminal_font
    }

    fn set_terminal_font(&mut self, terminal_font: TerminalFont) -> AppResult<()> {
        let previous = std::mem::replace(&mut self.terminal_font, terminal_font);
        if let Err(error) = self.save() {
            self.terminal_font = previous;
            return Err(error);
        }

        Ok(())
    }

    fn snapshot(&self) -> CommandPanelSessionSnapshot {
        CommandPanelSessionSnapshot {
            current: self.current.clone(),
            selection_save_pending: self.selection_save_pending,
        }
    }

    fn restore(&mut self, snapshot: CommandPanelSessionSnapshot) {
        self.current = snapshot.current;
        self.selection_save_pending = snapshot.selection_save_pending;
    }

    fn defer_selection_save(&mut self) {
        self.selection_save_pending = true;
    }

    fn save(&mut self) -> AppResult<()> {
        let Some(config_store) = self.config_store.as_ref() else {
            self.selection_save_pending = false;
            self.command_panel_save_pending = None;
            return Ok(());
        };

        config_store.save_settings(&AppSettings {
            command_panel: self.current.clone(),
            terminal_font: self.terminal_font.clone(),
        })?;
        self.selection_save_pending = false;
        self.command_panel_save_pending = None;
        Ok(())
    }

    fn defer_command_panel_save(&mut self, rollback: CommandPanelSessionSnapshot, now: Instant) {
        if self.config_store.is_none() {
            self.selection_save_pending = false;
            self.command_panel_save_pending = None;
            return;
        }

        let due_at = now + Duration::from_millis(COMMAND_PANEL_SAVE_DELAY_MS);
        if let Some(pending) = self.command_panel_save_pending.as_mut() {
            pending.due_at = due_at;
            return;
        }

        self.command_panel_save_pending = Some(PendingCommandPanelSave {
            rollback_current: rollback.current,
            rollback_selection_save_pending: rollback.selection_save_pending,
            due_at,
        });
    }

    fn save_due_command_panel_change(&mut self, now: Instant) -> AppResult<()> {
        let Some(pending) = self.command_panel_save_pending.as_ref() else {
            return Ok(());
        };

        if now < pending.due_at {
            return Ok(());
        }

        self.save_pending_command_panel_change()
    }

    fn save_pending_command_panel_change(&mut self) -> AppResult<()> {
        let Some(pending) = self.command_panel_save_pending.take() else {
            return Ok(());
        };

        if let Err(error) = self.save() {
            self.current = pending.rollback_current;
            self.selection_save_pending = pending.rollback_selection_save_pending;
            self.command_panel_save_pending = None;
            return Err(error);
        }

        Ok(())
    }

    fn save_pending_selection(&mut self) -> AppResult<()> {
        if self.command_panel_save_pending.is_some() {
            return self.save_pending_command_panel_change();
        }

        if !self.selection_save_pending {
            return Ok(());
        }

        self.save()
    }
}

impl WindowSessionCoordinator {
    fn load(initial_size: TerminalSize, startup: StartupInvocation) -> AppResult<Self> {
        let config_store = ConfigStore::from_current_exe()?;
        let settings = config_store.load_settings_or_default()?;
        Ok(Self::new(
            initial_size,
            startup,
            settings.command_panel,
            settings.terminal_font,
            Some(config_store),
        ))
    }

    fn new(
        initial_size: TerminalSize,
        startup: StartupInvocation,
        command_panel: CommandPanel,
        terminal_font: TerminalFont,
        config_store: Option<ConfigStore>,
    ) -> Self {
        Self {
            tabs: TerminalTabs::new(
                initial_size,
                PortablePtySession::new,
                AlacrittyTerminalBuffer::new,
            ),
            command_panel_state: CommandPanelSessionState::new(
                command_panel,
                terminal_font,
                config_store,
            ),
            startup,
            #[cfg(test)]
            terminal_command_overrides: Vec::new(),
            #[cfg(test)]
            terminal_viewport_refresh_overrides: Vec::new(),
            #[cfg(test)]
            terminal_resize_commands: Vec::new(),
        }
    }

    #[cfg(test)]
    fn new_for_test(
        initial_size: TerminalSize,
        startup_command: Option<crate::domain::StartupCommand>,
    ) -> Self {
        Self::new(
            initial_size,
            StartupInvocation::new(None, startup_command),
            default_command_panel(),
            TerminalFont::default(),
            None,
        )
    }

    #[cfg(test)]
    fn command_panel(&self) -> &CommandPanel {
        self.command_panel_state.current()
    }

    fn command_panel_handle(&self) -> SharedCommandPanel {
        self.command_panel_state.current_handle()
    }

    fn terminal_font(&self) -> &TerminalFont {
        self.command_panel_state.terminal_font()
    }

    fn set_terminal_font(&mut self, terminal_font: TerminalFont) -> AppResult<()> {
        self.command_panel_state.set_terminal_font(terminal_font)
    }

    fn suggested_new_command_category_name(&self) -> String {
        self.command_panel_state
            .current()
            .suggested_new_category_name()
    }

    fn selected_command_category_name(&self) -> AppResult<String> {
        self.command_panel_state
            .current()
            .selected_category()
            .map(|category| category.name.clone())
            .ok_or(AppError::InvalidState(
                "selected command category is missing",
            ))
    }

    fn save_pending_command_panel_selection(&mut self) -> AppResult<()> {
        self.command_panel_state.save_pending_selection()
    }

    fn defer_command_panel_save(&mut self, rollback: CommandPanelSessionSnapshot, now: Instant) {
        self.command_panel_state
            .defer_command_panel_save(rollback, now);
    }

    fn save_due_command_panel_change(&mut self, now: Instant) -> AppResult<()> {
        self.command_panel_state.save_due_command_panel_change(now)
    }

    fn save_pending_command_panel_change(&mut self) -> AppResult<()> {
        self.command_panel_state.save_pending_command_panel_change()
    }

    #[cfg(test)]
    fn save_command_panel_change(
        &mut self,
        change: impl FnOnce(&mut Self) -> AppResult<()>,
    ) -> AppResult<()> {
        self.save_command_panel_change_at(change, Instant::now())
    }

    #[cfg(test)]
    fn save_command_panel_change_at(
        &mut self,
        change: impl FnOnce(&mut Self) -> AppResult<()>,
        now: Instant,
    ) -> AppResult<()> {
        let previous = self.command_panel_state.snapshot();
        if let Err(error) = change(self) {
            self.command_panel_state.restore(previous);
            return Err(error);
        }

        self.defer_command_panel_save(previous, now);
        Ok(())
    }

    fn command_panel_snapshot(&self) -> CommandPanelSessionSnapshot {
        self.command_panel_state.snapshot()
    }

    fn restore_command_panel(&mut self, snapshot: CommandPanelSessionSnapshot) {
        self.command_panel_state.restore(snapshot);
    }

    fn start(&mut self) -> AppResult<()> {
        match self.startup.working_directory() {
            Some(startup_directory) => self
                .tabs
                .start_with_startup_directory(Some(startup_directory)),
            None => self.tabs.start(),
        }
    }

    fn run_startup_command(&mut self) -> AppResult<()> {
        let Some(command) = self.startup.command() else {
            return Ok(());
        };

        self.tabs.run_startup_command(command)?;
        self.startup.clear_command();
        Ok(())
    }

    fn handle_input(&mut self, input: TerminalInput) -> AppResult<()> {
        self.tabs.handle_input(input)
    }

    fn paste_text(&mut self, text: &str) -> AppResult<()> {
        self.tabs.paste_text(text)
    }

    fn command_button(&self, id: CommandButtonId) -> AppResult<CommandButton> {
        Ok(self
            .command_panel_state
            .current()
            .button_by_id(id)
            .ok_or(AppError::InvalidInput("unknown command button"))?
            .to_owned())
    }

    fn active_shell_command_dialect(&mut self) -> AppResult<crate::domain::ShellCommandDialect> {
        self.tabs.active_shell_command_dialect()
    }

    fn run_command_text(&mut self, command_text: &CommandText) -> AppResult<()> {
        self.tabs.run_command_text(command_text)
    }

    fn select_command_category_by_index(&mut self, index: usize) -> AppResult<()> {
        let previous_index = self.command_panel_state.current().selected_category_index();
        self.command_panel_state
            .current_mut()
            .select_category_by_index(index)?;
        if self.command_panel_state.current().selected_category_index() != previous_index {
            self.command_panel_state.defer_selection_save();
        }
        Ok(())
    }

    #[cfg(test)]
    fn add_command_category(&mut self) -> AppResult<()> {
        self.command_panel_state.current_mut().add_category()?;
        Ok(())
    }

    fn add_command_category_named(&mut self, name: String) -> AppResult<()> {
        self.command_panel_state
            .current_mut()
            .add_category_named(name)?;
        Ok(())
    }

    fn rename_selected_command_category(&mut self, name: String) -> AppResult<()> {
        self.command_panel_state
            .current_mut()
            .rename_selected_category(name)
    }

    fn delete_selected_command_category(&mut self) -> AppResult<()> {
        self.command_panel_state
            .current_mut()
            .delete_selected_category()
    }

    fn move_selected_command_category_up(&mut self) -> AppResult<()> {
        self.command_panel_state
            .current_mut()
            .move_selected_category_up()
    }

    fn move_selected_command_category_down(&mut self) -> AppResult<()> {
        self.command_panel_state
            .current_mut()
            .move_selected_category_down()
    }

    fn add_button_to_selected_category(
        &mut self,
        definition: CommandButtonDefinition,
    ) -> AppResult<()> {
        self.command_panel_state
            .current_mut()
            .add_button_to_selected_category(definition)?;
        Ok(())
    }

    fn update_command_button(
        &mut self,
        id: CommandButtonId,
        definition: CommandButtonDefinition,
    ) -> AppResult<()> {
        self.command_panel_state
            .current_mut()
            .update_button(id, definition)?;
        Ok(())
    }

    fn delete_command_button(&mut self, id: CommandButtonId) -> AppResult<()> {
        self.command_panel_state.current_mut().delete_button(id)
    }

    fn move_command_button_up(&mut self, id: CommandButtonId) -> AppResult<()> {
        self.command_panel_state.current_mut().move_button_up(id)
    }

    fn move_command_button_down(&mut self, id: CommandButtonId) -> AppResult<()> {
        self.command_panel_state.current_mut().move_button_down(id)
    }

    fn category_menu_state(&self) -> CategoryMenuState {
        let command_panel = self.command_panel_state.current();
        CategoryMenuState {
            can_delete: command_panel.categories().len() > 1,
            can_move_up: command_panel.can_move_selected_category_up(),
            can_move_down: command_panel.can_move_selected_category_down(),
        }
    }

    fn button_menu_state(&self, id: CommandButtonId) -> ButtonMenuState {
        let command_panel = self.command_panel_state.current();
        ButtonMenuState {
            can_move_up: command_panel.can_move_button_up(id),
            can_move_down: command_panel.can_move_button_down(id),
        }
    }

    fn execute(&mut self, command: TerminalCommand) -> AppResult<()> {
        #[cfg(test)]
        if let Some(command_override) = self.next_terminal_command_override() {
            return match command_override {
                TerminalCommandOverride::Fail(message) => Err(AppError::InvalidState(message)),
            };
        }

        #[cfg(test)]
        if let TerminalCommand::Resize(size) = &command {
            self.terminal_resize_commands.push(*size);
        }

        self.tabs.execute(command)
    }

    fn open_tab(&mut self) -> AppResult<()> {
        self.tabs.open_tab()?;
        Ok(())
    }

    fn close_tab(&mut self, id: crate::domain::TerminalTabId) -> AppResult<()> {
        self.tabs.close_tab(id)
    }

    fn switch_to_tab(&mut self, id: crate::domain::TerminalTabId) -> AppResult<()> {
        self.tabs.switch_to_tab(id)
    }

    fn drain_timer_events(&mut self) -> AppResult<TerminalTimerDrain> {
        self.tabs.drain_timer_events()
    }

    fn terminal_viewport(&mut self) -> AppResult<crate::domain::TerminalViewport> {
        #[cfg(test)]
        if let Some(refresh_override) = self.next_terminal_viewport_refresh_override() {
            return match refresh_override {
                TerminalViewportRefreshOverride::Fail(message) => {
                    Err(AppError::InvalidState(message))
                }
            };
        }

        self.tabs.terminal_viewport()
    }

    fn refresh_terminal_viewport(
        &mut self,
        viewport: &mut crate::domain::TerminalViewport,
    ) -> AppResult<()> {
        #[cfg(test)]
        if let Some(refresh_override) = self.next_terminal_viewport_refresh_override() {
            return match refresh_override {
                TerminalViewportRefreshOverride::Fail(message) => {
                    Err(AppError::InvalidState(message))
                }
            };
        }

        self.tabs.refresh_terminal_viewport(viewport)
    }

    fn scroll_terminal_display(&mut self, scroll: TerminalScroll) -> AppResult<bool> {
        self.tabs.scroll_terminal_display(scroll)
    }

    fn display_recoverable_error(&mut self, user_message: &str) -> AppResult<()> {
        self.tabs.display_recoverable_error(user_message)
    }

    fn display_status_message(&mut self, user_message: &str) -> AppResult<()> {
        self.tabs.display_status_message(user_message)
    }

    fn tab_views(&self) -> Vec<TerminalTabView> {
        self.tabs.tab_views()
    }

    #[cfg(test)]
    fn push_terminal_command_failure(&mut self, message: &'static str) {
        self.terminal_command_overrides
            .push(TerminalCommandOverride::Fail(message));
    }

    #[cfg(test)]
    fn next_terminal_command_override(&mut self) -> Option<TerminalCommandOverride> {
        if self.terminal_command_overrides.is_empty() {
            None
        } else {
            Some(self.terminal_command_overrides.remove(0))
        }
    }

    #[cfg(test)]
    fn push_terminal_viewport_refresh_failure(&mut self, message: &'static str) {
        self.terminal_viewport_refresh_overrides
            .push(TerminalViewportRefreshOverride::Fail(message));
    }

    #[cfg(test)]
    fn next_terminal_viewport_refresh_override(
        &mut self,
    ) -> Option<TerminalViewportRefreshOverride> {
        if self.terminal_viewport_refresh_overrides.is_empty() {
            None
        } else {
            Some(self.terminal_viewport_refresh_overrides.remove(0))
        }
    }
}

struct WindowViewAdapters {
    command_panel: SharedCommandPanel,
    tab_views: Vec<TerminalTabView>,
    terminal_viewport: Option<TerminalViewport>,
    terminal_selection: Option<TerminalSelection>,
    command_controls: CommandPanelControls,
    terminal_scrollbar: TerminalScrollBarControl,
    icons: Option<WindowIcons>,
    command_panel_width: i32,
    command_button_scroll_position: usize,
    layout: WindowLayout,
    renderer: GdiRenderer,
    pending_terminal_paint_rows: Option<TerminalChangedRows>,
    #[cfg(test)]
    command_panel_sync_overrides: Vec<CommandPanelSyncOverride>,
}

struct CommandPanelResize {
    previous: CommandPanelResizeSnapshot,
    previous_terminal_size: TerminalSize,
    terminal_size: TerminalSize,
    terminal_grid_changed: bool,
}

struct ClientResize {
    previous: CommandPanelResizeSnapshot,
    terminal_size: TerminalSize,
    terminal_grid_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientResizeOutcome {
    terminal_grid_changed: bool,
    refreshed_terminal_viewport: bool,
}

impl ClientResizeOutcome {
    fn should_invalidate_client(self) -> bool {
        self.terminal_grid_changed || self.refreshed_terminal_viewport
    }
}

#[derive(Clone, Copy)]
struct CommandPanelResizeSnapshot {
    client_width: i32,
    client_height: i32,
    command_panel_width: i32,
    command_button_scroll_position: usize,
}

struct CommandPanelViewState {
    command_panel: SharedCommandPanel,
    command_button_scroll_position: usize,
    layout: WindowLayout,
}

#[cfg(test)]
enum CommandPanelSyncOverride {
    Succeed,
    Fail(&'static str),
    ApplyCommandControlsThenFail(&'static str),
}

impl WindowViewAdapters {
    fn new(
        command_panel: SharedCommandPanel,
        tab_views: Vec<TerminalTabView>,
        terminal_font: TerminalFont,
    ) -> Self {
        let command_panel_width = COMMAND_PANEL_WIDTH;
        let layout = WindowLayout::for_client_with_command_panel_width(
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            command_panel_width,
            command_panel.selected_buttons(),
            &tab_views,
        );
        let mut renderer = GdiRenderer::default();
        renderer.set_font(terminal_font);

        Self {
            command_panel,
            tab_views,
            terminal_viewport: None,
            terminal_selection: None,
            command_controls: CommandPanelControls::default(),
            terminal_scrollbar: TerminalScrollBarControl::default(),
            icons: None,
            command_panel_width,
            command_button_scroll_position: 0,
            layout,
            renderer,
            pending_terminal_paint_rows: None,
            #[cfg(test)]
            command_panel_sync_overrides: Vec::new(),
        }
    }

    fn apply_window_icons(&mut self, hwnd: HWND) -> AppResult<()> {
        if self.icons.is_some() {
            return Ok(());
        }

        let icons = WindowIcons::load(current_instance()?)?;
        icons.apply(hwnd);
        self.icons = Some(icons);
        Ok(())
    }

    fn create_command_controls(&mut self, parent: HWND) -> AppResult<()> {
        self.command_controls.create(parent, &self.command_panel)?;
        self.terminal_scrollbar.create(parent)
    }

    fn resize_to_client(&mut self, hwnd: HWND) -> AppResult<ClientResize> {
        let rect = client_rect(hwnd)?;
        let metrics = self.renderer.ensure_metrics(hwnd)?;
        let previous = self.command_panel_resize_snapshot();
        let previous_terminal_size = terminal_size_from_area(self.layout.terminal, metrics)?;
        self.refresh_layout_for_client_rect(rect);
        self.layout_command_controls()?;
        let terminal_size = terminal_size_from_area(self.layout.terminal, metrics)?;
        Ok(ClientResize {
            previous,
            terminal_size,
            terminal_grid_changed: terminal_grid_changed(previous_terminal_size, terminal_size),
        })
    }

    fn client_size(&self) -> (i32, i32) {
        (
            self.layout.tab_bar.width.max(1),
            self.layout
                .tab_bar
                .height
                .saturating_add(self.layout.terminal.height)
                .max(1),
        )
    }

    fn set_terminal_font(
        &mut self,
        hwnd: HWND,
        terminal_font: TerminalFont,
    ) -> AppResult<ClientResize> {
        self.renderer.set_font(terminal_font);
        self.resize_to_client(hwnd)
    }

    fn resize_command_panel_from_splitter(
        &mut self,
        hwnd: HWND,
        splitter_x: i32,
    ) -> AppResult<Option<CommandPanelResize>> {
        let rect = client_rect(hwnd)?;
        let client_width = rect.right.saturating_sub(rect.left).max(1);
        let command_panel_width =
            WindowLayout::command_panel_width_from_splitter_x(client_width, splitter_x);
        if command_panel_width == self.command_panel_width
            && command_panel_width == self.layout.command_panel.width
        {
            return Ok(None);
        }

        // Splitter drags do not change font metrics; avoid repeating GDI
        // metric measurement on every WM_MOUSEMOVE.
        let metrics = self.renderer.cell_metrics();
        let previous = self.command_panel_resize_snapshot();
        let previous_terminal_size = terminal_size_from_area(self.layout.terminal, metrics)?;
        self.command_panel_width = command_panel_width;
        self.refresh_layout_for_splitter_resize(rect);
        let terminal_size = match (|| -> AppResult<TerminalSize> {
            self.layout_command_controls()?;
            terminal_size_from_area(self.layout.terminal, metrics)
        })() {
            Ok(terminal_size) => terminal_size,
            Err(error) => {
                self.rollback_command_panel_resize(previous, &error)?;
                return Err(error);
            }
        };
        Ok(Some(CommandPanelResize {
            previous,
            previous_terminal_size,
            terminal_size,
            terminal_grid_changed: terminal_grid_changed(previous_terminal_size, terminal_size),
        }))
    }

    fn command_panel_resize_snapshot(&self) -> CommandPanelResizeSnapshot {
        CommandPanelResizeSnapshot {
            client_width: self.layout.tab_bar.width.max(1),
            client_height: self
                .layout
                .tab_bar
                .height
                .saturating_add(self.layout.terminal.height)
                .max(1),
            command_panel_width: self.command_panel_width,
            command_button_scroll_position: self.command_button_scroll_position,
        }
    }

    fn rollback_command_panel_resize(
        &mut self,
        snapshot: CommandPanelResizeSnapshot,
        original_error: &AppError,
    ) -> AppResult<()> {
        self.command_panel_width = snapshot.command_panel_width;
        let layout = layout_from_client_size(
            snapshot.client_width,
            snapshot.client_height,
            snapshot.command_panel_width,
            self.command_panel.selected_buttons(),
            &self.tab_views,
            snapshot.command_button_scroll_position,
        );
        self.command_button_scroll_position = layout.command_button_scroll_position();
        self.layout = layout;

        if let Err(rollback_error) = self.layout_command_controls() {
            return Err(AppError::ui_message(
                "rollback command panel resize",
                format!(
                    "{original_error}; additionally failed to rollback command panel layout: {rollback_error}"
                ),
            ));
        }

        Ok(())
    }

    fn paint(&mut self, hwnd: HWND) -> AppResult<()> {
        let Some(viewport) = self.terminal_viewport.as_ref() else {
            return Err(AppError::InvalidState("terminal viewport cache is empty"));
        };

        let terminal_paint_rows = self.pending_terminal_paint_rows.take();
        self.renderer.paint(
            hwnd,
            viewport,
            &self.layout,
            &self.tab_views,
            self.terminal_selection,
            terminal_paint_rows.as_ref(),
        )
    }

    fn sync_command_panel_to_client(
        &mut self,
        hwnd: HWND,
        command_panel: SharedCommandPanel,
    ) -> AppResult<()> {
        let command_button_scroll_position = self.command_button_scroll_position;
        self.sync_command_panel_to_client_with_button_scroll_position(
            hwnd,
            command_panel,
            command_button_scroll_position,
        )
    }

    fn sync_command_panel_to_client_with_button_scroll_position(
        &mut self,
        hwnd: HWND,
        command_panel: SharedCommandPanel,
        command_button_scroll_position: usize,
    ) -> AppResult<()> {
        #[cfg(test)]
        if let Some(sync_override) = self.next_command_panel_sync_override() {
            return match sync_override {
                CommandPanelSyncOverride::Succeed => {
                    self.set_command_panel(command_panel);
                    self.command_button_scroll_position = command_button_scroll_position;
                    Ok(())
                }
                CommandPanelSyncOverride::Fail(message) => Err(AppError::InvalidState(message)),
                CommandPanelSyncOverride::ApplyCommandControlsThenFail(message) => self
                    .sync_command_panel_to_client_with_test_failure(hwnd, command_panel, message),
            };
        }

        let previous = self.command_panel_view_state();
        let rect = client_rect(hwnd)?;
        let next = self.command_panel_view_state_for_client_rect(
            rect,
            command_panel,
            command_button_scroll_position,
        );

        if let Err(error) = self.sync_command_panel_controls_to_client(hwnd, &next) {
            self.rollback_command_panel_controls(hwnd, previous, &error)?;
            return Err(error);
        }

        self.apply_command_panel_view_state(next);
        Ok(())
    }

    fn command_panel_view_state(&self) -> CommandPanelViewState {
        CommandPanelViewState {
            command_panel: Rc::clone(&self.command_panel),
            command_button_scroll_position: self.command_button_scroll_position,
            layout: self.layout.clone(),
        }
    }

    fn command_panel_view_state_for_client_rect(
        &self,
        rect: RECT,
        command_panel: SharedCommandPanel,
        command_button_scroll_position: usize,
    ) -> CommandPanelViewState {
        let layout = layout_from_client_rect(
            rect,
            self.command_panel_width,
            command_panel.selected_buttons(),
            &self.tab_views,
            command_button_scroll_position,
        );
        let command_button_scroll_position = layout.command_button_scroll_position();

        CommandPanelViewState {
            command_panel,
            command_button_scroll_position,
            layout,
        }
    }

    fn apply_command_panel_view_state(&mut self, state: CommandPanelViewState) {
        self.command_panel = state.command_panel;
        self.command_button_scroll_position = state.command_button_scroll_position;
        self.layout = state.layout;
    }

    fn sync_command_panel_controls_to_client(
        &mut self,
        hwnd: HWND,
        state: &CommandPanelViewState,
    ) -> AppResult<()> {
        self.command_controls.sync(hwnd, &state.command_panel)?;
        self.command_controls.layout(&state.layout)?;
        self.terminal_scrollbar
            .layout(&state.layout, self.terminal_scroll_state())
    }

    fn rollback_command_panel_controls(
        &mut self,
        hwnd: HWND,
        state: CommandPanelViewState,
        original_error: &AppError,
    ) -> AppResult<()> {
        self.apply_command_panel_view_state(state);

        #[cfg(test)]
        if hwnd.is_null() {
            self.command_controls
                .sync_buttons_for_test(&self.command_panel)?;
            return Ok(());
        }

        if let Err(rollback_error) = self.rollback_command_panel_controls_to_client(hwnd) {
            return Err(AppError::ui_message(
                "rollback command panel controls",
                format!(
                    "{original_error}; additionally failed to rollback command panel controls: {rollback_error}"
                ),
            ));
        }

        Ok(())
    }

    fn rollback_command_panel_controls_to_client(&mut self, hwnd: HWND) -> AppResult<()> {
        self.command_controls.sync(hwnd, &self.command_panel)?;
        self.command_controls.layout(&self.layout)?;
        self.layout_terminal_scrollbar()
    }

    fn refresh_layout_for_client_rect(&mut self, rect: RECT) {
        let layout = layout_from_client_rect(
            rect,
            self.command_panel_width,
            self.command_panel.selected_buttons(),
            &self.tab_views,
            self.command_button_scroll_position,
        );
        self.command_button_scroll_position = layout.command_button_scroll_position();
        self.layout = layout;
    }

    fn refresh_layout_for_splitter_resize(&mut self, rect: RECT) {
        let width = rect.right.saturating_sub(rect.left).max(1);
        let height = rect.bottom.saturating_sub(rect.top).max(1);
        let buttons = self.command_panel.selected_buttons();

        if !self.layout.try_resize_command_panel_width(
            width,
            height,
            self.command_panel_width,
            buttons,
            self.command_button_scroll_position,
        ) {
            self.refresh_layout_for_client_rect(rect);
            return;
        }

        self.command_button_scroll_position = self.layout.command_button_scroll_position();
    }

    fn refresh_layout_for_tab_views(&mut self) {
        let snapshot = self.command_panel_resize_snapshot();
        let layout = layout_from_client_size(
            snapshot.client_width,
            snapshot.client_height,
            snapshot.command_panel_width,
            self.command_panel.selected_buttons(),
            &self.tab_views,
            snapshot.command_button_scroll_position,
        );
        self.command_button_scroll_position = layout.command_button_scroll_position();
        self.layout = layout;
    }

    fn layout_command_controls(&self) -> AppResult<()> {
        self.command_controls.layout(&self.layout)?;
        self.layout_terminal_scrollbar()
    }

    fn layout_terminal_scrollbar(&self) -> AppResult<()> {
        self.terminal_scrollbar
            .layout(&self.layout, self.terminal_scroll_state())
    }

    fn sync_terminal_scrollbar(&self) -> AppResult<()> {
        self.terminal_scrollbar
            .sync_scroll_state(self.terminal_scroll_state())
    }

    fn set_tab_views(&mut self, tab_views: Vec<TerminalTabView>) {
        self.tab_views = tab_views;
        self.refresh_layout_for_tab_views();
    }

    #[cfg(test)]
    fn set_command_panel(&mut self, command_panel: SharedCommandPanel) {
        self.command_panel = command_panel;
    }

    #[cfg(test)]
    fn push_command_panel_sync_success(&mut self) {
        self.command_panel_sync_overrides
            .push(CommandPanelSyncOverride::Succeed);
    }

    #[cfg(test)]
    fn push_command_panel_sync_failure(&mut self, message: &'static str) {
        self.command_panel_sync_overrides
            .push(CommandPanelSyncOverride::Fail(message));
    }

    #[cfg(test)]
    fn push_command_panel_sync_apply_controls_then_failure(&mut self, message: &'static str) {
        self.command_panel_sync_overrides.push(
            CommandPanelSyncOverride::ApplyCommandControlsThenFail(message),
        );
    }

    #[cfg(test)]
    fn next_command_panel_sync_override(&mut self) -> Option<CommandPanelSyncOverride> {
        if self.command_panel_sync_overrides.is_empty() {
            None
        } else {
            Some(self.command_panel_sync_overrides.remove(0))
        }
    }

    #[cfg(test)]
    fn sync_command_panel_to_client_with_test_failure(
        &mut self,
        hwnd: HWND,
        command_panel: SharedCommandPanel,
        message: &'static str,
    ) -> AppResult<()> {
        let previous = self.command_panel_view_state();

        self.command_controls
            .sync_buttons_for_test(command_panel.as_ref())?;
        let error = AppError::InvalidState(message);
        self.rollback_command_panel_controls(hwnd, previous, &error)?;
        Err(error)
    }

    fn scroll_command_buttons(
        &mut self,
        hwnd: HWND,
        request: CommandButtonScrollRequest,
    ) -> AppResult<bool> {
        let Some(scroll) = self.layout.command_button_scroll else {
            self.command_button_scroll_position = 0;
            return Ok(false);
        };

        let page = scroll.page_len.max(1);
        let next = match request {
            CommandButtonScrollRequest::LineUp => scroll.position.saturating_sub(1),
            CommandButtonScrollRequest::LineDown => {
                scroll.position.saturating_add(1).min(scroll.max_position)
            }
            CommandButtonScrollRequest::PageUp => scroll.position.saturating_sub(page),
            CommandButtonScrollRequest::PageDown => scroll
                .position
                .saturating_add(page)
                .min(scroll.max_position),
            CommandButtonScrollRequest::Absolute(position) => position.min(scroll.max_position),
            CommandButtonScrollRequest::Top => 0,
            CommandButtonScrollRequest::Bottom => scroll.max_position,
        };

        self.apply_command_button_scroll_position(hwnd, next)
    }

    fn scroll_command_buttons_by_lines(&mut self, hwnd: HWND, line_delta: i32) -> AppResult<bool> {
        let Some(scroll) = self.layout.command_button_scroll else {
            self.command_button_scroll_position = 0;
            return Ok(false);
        };

        let next = scrolled_position_by_lines(scroll.position, scroll.max_position, line_delta);
        self.apply_command_button_scroll_position(hwnd, next)
    }

    fn apply_command_button_scroll_position(
        &mut self,
        hwnd: HWND,
        position: usize,
    ) -> AppResult<bool> {
        if position == self.layout.command_button_scroll_position() {
            return Ok(false);
        }

        self.command_button_scroll_position = position;
        let rect = client_rect(hwnd)?;
        self.refresh_layout_for_client_rect(rect);
        self.layout_command_controls()?;
        Ok(true)
    }

    fn selected_category_index(&self) -> Option<usize> {
        self.command_controls.selected_category_index()
    }

    fn command_button_id_from_wparam(&self, wparam: WPARAM) -> Option<CommandButtonId> {
        self.command_controls.command_button_id_from_wparam(wparam)
    }

    fn command_button_id_from_hwnd(&self, hwnd: HWND) -> Option<CommandButtonId> {
        self.command_controls.command_button_id_from_hwnd(hwnd)
    }

    fn set_terminal_viewport(&mut self, viewport: TerminalViewport) {
        self.replace_terminal_viewport_state(Some(viewport));
    }

    fn replace_terminal_viewport(
        &mut self,
        viewport: TerminalViewport,
    ) -> Option<TerminalViewport> {
        self.replace_terminal_viewport_state(Some(viewport))
    }

    fn restore_terminal_viewport(&mut self, viewport: Option<TerminalViewport>) {
        self.replace_terminal_viewport_state(viewport);
    }

    fn replace_terminal_viewport_state(
        &mut self,
        viewport: Option<TerminalViewport>,
    ) -> Option<TerminalViewport> {
        let previous = std::mem::replace(&mut self.terminal_viewport, viewport);
        self.terminal_selection = None;
        self.renderer.clear_terminal_line_cache();
        previous
    }

    fn terminal_viewport_mut(&mut self) -> Option<&mut TerminalViewport> {
        self.terminal_viewport.as_mut()
    }

    fn clear_terminal_viewport(&mut self) {
        self.replace_terminal_viewport_state(None);
    }

    fn has_terminal_viewport(&self) -> bool {
        self.terminal_viewport.is_some()
    }

    fn has_terminal_selection(&self) -> bool {
        self.terminal_selection.is_some()
    }

    fn clear_terminal_selection(&mut self) -> bool {
        self.terminal_selection.take().is_some()
    }

    fn update_terminal_selection(
        &mut self,
        anchor: TerminalGridPoint,
        focus: TerminalGridPoint,
    ) -> bool {
        let selection = if anchor == focus {
            None
        } else {
            Some(TerminalSelection::new(anchor, focus))
        };

        if self.terminal_selection == selection {
            return false;
        }

        self.terminal_selection = selection;
        true
    }

    fn selected_terminal_text(&self) -> Option<String> {
        let viewport = self.terminal_viewport.as_ref()?;
        let selection = self.terminal_selection?;
        Some(viewport.selected_text(selection))
    }

    fn terminal_grid_point_at(
        &self,
        point: UiPoint,
        clamp_to_grid: bool,
    ) -> Option<TerminalGridPoint> {
        let viewport = self.terminal_viewport.as_ref()?;
        if viewport.rows == 0 || viewport.columns == 0 {
            return None;
        }

        let metrics = self.renderer.cell_metrics();
        let cell_width = metrics.width.max(1);
        let cell_height = metrics.height.max(1);
        let content = terminal_content_area(self.layout.terminal);
        if content.width <= 0 || content.height <= 0 {
            return None;
        }

        let grid_width = i32_from_usize_saturating(viewport.columns)
            .saturating_mul(cell_width)
            .min(content.width);
        let grid_height = i32_from_usize_saturating(viewport.rows)
            .saturating_mul(cell_height)
            .min(content.height);
        if grid_width <= 0 || grid_height <= 0 {
            return None;
        }

        let grid = UiRect {
            x: content.x,
            y: content.y,
            width: grid_width,
            height: grid_height,
        };
        if !clamp_to_grid && !grid.contains(point) {
            return None;
        }

        let relative_x = point
            .x
            .saturating_sub(content.x)
            .clamp(0, grid_width.saturating_sub(1));
        let relative_y = point
            .y
            .saturating_sub(content.y)
            .clamp(0, grid_height.saturating_sub(1));
        let column = usize_from_i32_saturating(relative_x / cell_width)
            .min(viewport.columns.saturating_sub(1));
        let row = usize_from_i32_saturating(relative_y / cell_height)
            .min(viewport.rows.saturating_sub(1));

        Some(TerminalGridPoint::new(row, column))
    }

    fn terminal_scroll_state(&self) -> crate::domain::TerminalScrollState {
        self.terminal_viewport
            .as_ref()
            .map_or(crate::domain::TerminalScrollState::default(), |viewport| {
                viewport.scroll
            })
    }

    fn terminal_scroll_request(&self, request: TerminalScrollBarRequest) -> TerminalScroll {
        let scroll = self.terminal_scroll_state();
        match request {
            TerminalScrollBarRequest::LineUp => TerminalScroll::Lines(1),
            TerminalScrollBarRequest::LineDown => TerminalScroll::Lines(-1),
            TerminalScrollBarRequest::PageUp => TerminalScroll::PageUp,
            TerminalScrollBarRequest::PageDown => TerminalScroll::PageDown,
            TerminalScrollBarRequest::Absolute(position) => {
                TerminalScroll::Absolute(scroll.max_position.saturating_sub(position))
            }
            TerminalScrollBarRequest::Top => TerminalScroll::Top,
            TerminalScrollBarRequest::Bottom => TerminalScroll::Bottom,
        }
    }

    fn tab_close_at(&self, point: UiPoint) -> Option<crate::domain::TerminalTabId> {
        self.layout.tab_close_at(point)
    }

    fn new_tab_at(&self, point: UiPoint) -> bool {
        self.layout.new_tab_at(point)
    }

    fn tab_at(&self, point: UiPoint) -> Option<crate::domain::TerminalTabId> {
        self.layout.tab_at(point)
    }

    fn terminal_contains(&self, point: UiPoint) -> bool {
        self.layout.terminal.contains(point)
    }

    fn splitter_contains(&self, point: UiPoint) -> bool {
        self.layout.splitter_at(point)
    }

    fn command_button_viewport_contains(&self, point: UiPoint) -> bool {
        self.layout.command_button_viewport.contains(point)
    }

    fn splitter(&self) -> UiRect {
        self.layout.splitter
    }

    fn invalidate(&mut self, hwnd: HWND) {
        self.pending_terminal_paint_rows = None;
        GdiRenderer::invalidate(hwnd);
    }

    fn invalidate_terminal_content(&mut self, hwnd: HWND) {
        self.pending_terminal_paint_rows = None;
        GdiRenderer::invalidate_rect(hwnd, terminal_content_area(self.layout.terminal));
    }

    fn invalidate_terminal_content_rows(&mut self, hwnd: HWND, rows: TerminalChangedRows) {
        let mut invalidated = false;
        let visible_rows = self
            .terminal_viewport
            .as_ref()
            .map_or(0, |viewport| viewport.rows);
        for row_range in rows.ranges() {
            let Some(rect) = terminal_rows_rect(
                terminal_content_area(self.layout.terminal),
                self.renderer.cell_metrics(),
                row_range.clone(),
                visible_rows,
            ) else {
                continue;
            };

            GdiRenderer::invalidate_rect(hwnd, rect);
            invalidated = true;
        }

        if !invalidated {
            return;
        }

        match self.pending_terminal_paint_rows.as_mut() {
            Some(pending) => pending.merge(&rows),
            None => self.pending_terminal_paint_rows = Some(rows),
        }
    }

    fn invalidate_command_button_viewport(&mut self, hwnd: HWND) {
        self.pending_terminal_paint_rows = None;
        GdiRenderer::invalidate_rect(hwnd, self.layout.command_button_viewport);
    }
}

struct WindowRuntimeState {
    input: InputMapper,
    timer_active: bool,
    timer_interval_ms: u32,
    idle_pty_drain_ticks: u8,
    splitter_drag: Option<SplitterDrag>,
    terminal_selection_drag: Option<TerminalSelectionDrag>,
}

#[derive(Clone, Copy)]
struct SplitterDrag {
    pointer_offset_x: i32,
    deferred_terminal_size: Option<TerminalSize>,
}

#[derive(Clone, Copy)]
struct TerminalSelectionDrag {
    anchor: TerminalGridPoint,
}

impl WindowRuntimeState {
    fn new() -> Self {
        Self {
            input: InputMapper::default(),
            timer_active: false,
            timer_interval_ms: PTY_ACTIVE_TIMER_MS,
            idle_pty_drain_ticks: 0,
            splitter_drag: None,
            terminal_selection_drag: None,
        }
    }

    fn char_input(&mut self, wparam: WPARAM) -> Option<TerminalInput> {
        self.input.char_input(wparam)
    }

    fn clear_pending_char(&mut self) {
        self.input.clear_pending_char();
    }

    fn start_splitter_drag(&mut self, hwnd: HWND, point: UiPoint, splitter: UiRect) {
        self.splitter_drag = Some(SplitterDrag {
            pointer_offset_x: point.x.saturating_sub(splitter.x),
            deferred_terminal_size: None,
        });

        // SAFETY: hwnd is the live main window on the UI thread.
        unsafe {
            SetCapture(hwnd);
        }
    }

    fn start_terminal_selection_drag(&mut self, hwnd: HWND, anchor: TerminalGridPoint) {
        self.terminal_selection_drag = Some(TerminalSelectionDrag { anchor });

        // SAFETY: hwnd is the live main window on the UI thread.
        unsafe {
            SetCapture(hwnd);
        }
    }

    fn terminal_selection_drag_anchor(&self) -> Option<TerminalGridPoint> {
        self.terminal_selection_drag.map(|drag| drag.anchor)
    }

    fn splitter_drag_x(&self, point: UiPoint) -> Option<i32> {
        self.splitter_drag
            .map(|drag| point.x.saturating_sub(drag.pointer_offset_x))
    }

    fn is_splitter_dragging(&self) -> bool {
        self.splitter_drag.is_some()
    }

    fn is_terminal_selection_dragging(&self) -> bool {
        self.terminal_selection_drag.is_some()
    }

    fn defer_splitter_terminal_resize(&mut self, size: TerminalSize) {
        if let Some(drag) = self.splitter_drag.as_mut() {
            drag.deferred_terminal_size = Some(size);
        }
    }

    fn clear_deferred_splitter_terminal_resize(&mut self) {
        if let Some(drag) = self.splitter_drag.as_mut() {
            drag.deferred_terminal_size = None;
        }
    }

    fn finish_splitter_drag(&mut self) -> AppResult<Option<TerminalSize>> {
        let Some(drag) = self.splitter_drag.take() else {
            return Ok(None);
        };

        // SAFETY: this releases mouse capture held by the current UI thread.
        let released = unsafe { ReleaseCapture() };
        if released == 0 {
            self.splitter_drag = Some(drag);
            Err(AppError::win32("ReleaseCapture"))
        } else {
            Ok(drag.deferred_terminal_size)
        }
    }

    fn finish_terminal_selection_drag(&mut self) -> AppResult<()> {
        let Some(drag) = self.terminal_selection_drag.take() else {
            return Ok(());
        };

        // SAFETY: this releases mouse capture held by the current UI thread.
        let released = unsafe { ReleaseCapture() };
        if released == 0 {
            self.terminal_selection_drag = Some(drag);
            Err(AppError::win32("ReleaseCapture"))
        } else {
            Ok(())
        }
    }

    fn cancel_splitter_drag(&mut self) -> Option<TerminalSize> {
        self.splitter_drag
            .take()
            .and_then(|drag| drag.deferred_terminal_size)
    }

    fn cancel_terminal_selection_drag(&mut self) {
        self.terminal_selection_drag = None;
    }

    fn start_timer(&mut self, hwnd: HWND) -> AppResult<()> {
        if self.timer_active {
            return Ok(());
        }

        self.idle_pty_drain_ticks = 0;
        self.set_timer_interval(hwnd, PTY_ACTIVE_TIMER_MS)
    }

    fn stop_timer(&mut self, hwnd: HWND) -> AppResult<()> {
        if !self.timer_active {
            return Ok(());
        }

        // SAFETY: hwnd is still live during WM_DESTROY and belongs to this UI thread.
        let stopped = unsafe { KillTimer(hwnd, TIMER_ID) };
        self.timer_active = false;
        self.timer_interval_ms = PTY_ACTIVE_TIMER_MS;
        self.idle_pty_drain_ticks = 0;
        if stopped == 0 {
            Err(AppError::win32("KillTimer"))
        } else {
            Ok(())
        }
    }

    fn update_pty_timer_after_drain(&mut self, hwnd: HWND, had_events: bool) -> AppResult<()> {
        let interval_ms = self.desired_pty_timer_interval_after_drain(had_events);
        if !self.timer_active || self.timer_interval_ms == interval_ms {
            return Ok(());
        }

        self.set_timer_interval(hwnd, interval_ms)
    }

    fn desired_pty_timer_interval_after_drain(&mut self, had_events: bool) -> u32 {
        if had_events {
            self.idle_pty_drain_ticks = 0;
            return PTY_ACTIVE_TIMER_MS;
        }

        self.idle_pty_drain_ticks = self.idle_pty_drain_ticks.saturating_add(1);
        if self.idle_pty_drain_ticks >= PTY_SUSTAINED_IDLE_TIMER_BACKOFF_TICKS {
            PTY_SUSTAINED_IDLE_TIMER_MS
        } else if self.idle_pty_drain_ticks >= PTY_IDLE_TIMER_BACKOFF_TICKS {
            PTY_IDLE_TIMER_MS
        } else {
            PTY_ACTIVE_TIMER_MS
        }
    }

    fn set_timer_interval(&mut self, hwnd: HWND, interval_ms: u32) -> AppResult<()> {
        // SAFETY: hwnd is a valid window and no callback is used, so WM_TIMER is posted.
        let timer = unsafe { SetTimer(hwnd, TIMER_ID, interval_ms, None) };
        if timer == 0 {
            return Err(AppError::win32("SetTimer"));
        }

        self.timer_active = true;
        self.timer_interval_ms = interval_ms;
        Ok(())
    }
}

impl Default for WindowRuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct WindowErrorState {
    message: Option<String>,
}

impl WindowErrorState {
    fn take(&mut self) -> Option<String> {
        self.message.take()
    }

    fn set(&mut self, message: String) {
        self.message = Some(message);
    }

    fn as_deref(&self) -> Option<&str> {
        self.message.as_deref()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalViewportInvalidation {
    None,
    Rows(TerminalChangedRows),
    Full,
}

impl WindowState {
    fn new(
        initial_size: TerminalSize,
        startup: StartupInvocation,
        shutdown_error: ShutdownErrorSink,
    ) -> AppResult<Self> {
        let session = WindowSessionCoordinator::load(initial_size, startup)?;
        let tab_views = session.tab_views();
        let command_panel = session.command_panel_handle();
        let terminal_font = session.terminal_font().clone();
        Ok(Self {
            session,
            view: WindowViewAdapters::new(command_panel, tab_views, terminal_font),
            runtime: WindowRuntimeState::default(),
            last_error: WindowErrorState::default(),
            shutdown_error,
        })
    }

    #[cfg(test)]
    fn new_for_test(
        initial_size: TerminalSize,
        startup_command: Option<crate::domain::StartupCommand>,
    ) -> Self {
        let session = WindowSessionCoordinator::new_for_test(initial_size, startup_command);
        let tab_views = session.tab_views();
        let command_panel = session.command_panel_handle();
        let terminal_font = session.terminal_font().clone();
        Self {
            session,
            view: WindowViewAdapters::new(command_panel, tab_views, terminal_font),
            runtime: WindowRuntimeState::default(),
            last_error: WindowErrorState::default(),
            shutdown_error: new_shutdown_error_sink(),
        }
    }

    fn on_create(&mut self, hwnd: HWND) -> AppResult<()> {
        self.apply_window_icons(hwnd)?;
        self.create_command_controls(hwnd)?;
        self.resize_to_client(hwnd).map(|_| ())
    }

    fn start(&mut self, hwnd: HWND) -> AppResult<()> {
        self.session.start()?;
        self.resize_to_client(hwnd)?;
        self.display_startup_window_size()?;
        self.session.run_startup_command()?;
        self.refresh_terminal_viewport()
    }

    fn display_startup_window_size(&mut self) -> AppResult<()> {
        let (width, height) = self.view.client_size();
        let message = startup_window_size_message(width, height);
        self.session.display_status_message(&message)
    }

    fn handle_input(&mut self, input: TerminalInput) -> AppResult<()> {
        self.session.handle_input(input)
    }

    fn handle_char(&mut self, wparam: WPARAM) -> AppResult<()> {
        let Some(input) = self.runtime.char_input(wparam) else {
            return Ok(());
        };
        self.handle_input(input)
    }

    fn handle_clipboard_shortcut(
        &mut self,
        hwnd: HWND,
        wparam: WPARAM,
        modifiers: TerminalKeyModifiers,
    ) -> bool {
        match self.try_handle_clipboard_shortcut(hwnd, wparam, modifiers) {
            Ok(handled) => handled,
            Err(error) => {
                self.record_error(error);
                true
            }
        }
    }

    fn try_handle_clipboard_shortcut(
        &mut self,
        hwnd: HWND,
        wparam: WPARAM,
        modifiers: TerminalKeyModifiers,
    ) -> AppResult<bool> {
        if !modifiers.ctrl || modifiers.alt {
            return Ok(false);
        }

        let Some(key) = u16::try_from(wparam).ok() else {
            return Ok(false);
        };

        match key {
            KEY_C if self.view.has_terminal_selection() => {
                input::suppress_next_control_char(CTRL_C_CHAR);
                self.copy_terminal_selection(hwnd)?;
                Ok(true)
            }
            KEY_V => {
                input::suppress_next_control_char(CTRL_V_CHAR);
                self.paste_terminal_clipboard(hwnd)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn copy_terminal_selection(&mut self, hwnd: HWND) -> AppResult<()> {
        let text = self
            .view
            .selected_terminal_text()
            .ok_or(AppError::InvalidState("terminal selection is empty"))?;
        clipboard::set_text(hwnd, &text)
    }

    fn paste_terminal_clipboard(&mut self, hwnd: HWND) -> AppResult<()> {
        let Some(text) = clipboard::get_text(hwnd)? else {
            return Ok(());
        };
        if text.is_empty() {
            return Ok(());
        }

        if self.view.clear_terminal_selection() {
            self.view.invalidate_terminal_content(hwnd);
        }
        self.session.paste_text(&text)
    }

    fn prepare_button_command(&self, button_id: CommandButtonId) -> AppResult<ButtonCommandPrompt> {
        Ok(ButtonCommandPrompt {
            button: self.session.command_button(button_id)?,
        })
    }

    fn handle_command_button_scroll(
        &mut self,
        hwnd: HWND,
        request: CommandButtonScrollRequest,
    ) -> AppResult<()> {
        if self.view.scroll_command_buttons(hwnd, request)? {
            self.view.invalidate_command_button_viewport(hwnd);
        }
        focus_main_window(hwnd);
        Ok(())
    }

    fn handle_terminal_scroll(
        &mut self,
        hwnd: HWND,
        request: TerminalScrollBarRequest,
    ) -> AppResult<()> {
        let scroll = self.view.terminal_scroll_request(request);
        if self.session.scroll_terminal_display(scroll)? {
            self.refresh_terminal_viewport()?;
            self.view.clear_terminal_selection();
            self.view.invalidate_terminal_content(hwnd);
        }
        focus_main_window(hwnd);
        Ok(())
    }

    fn handle_mouse_wheel(
        &mut self,
        hwnd: HWND,
        screen_point: UiPoint,
        wheel_delta: i32,
    ) -> AppResult<()> {
        let point = input::client_point_from_screen(hwnd, screen_point)?;
        if self.view.command_button_viewport_contains(point) {
            let line_delta = command_button_scroll_lines_from_wheel_delta(wheel_delta);
            if line_delta == 0 {
                return Ok(());
            }

            if self
                .view
                .scroll_command_buttons_by_lines(hwnd, line_delta)?
            {
                self.view.invalidate_command_button_viewport(hwnd);
            }
            return Ok(());
        }

        if self.view.terminal_contains(point) {
            let line_delta = terminal_scroll_lines_from_wheel_delta(wheel_delta);
            if line_delta == 0 {
                return Ok(());
            }

            if self
                .session
                .scroll_terminal_display(TerminalScroll::Lines(line_delta))?
            {
                self.refresh_terminal_viewport()?;
                self.view.clear_terminal_selection();
                self.view.invalidate_terminal_content(hwnd);
            }
        }

        Ok(())
    }

    fn command_button_id_from_wparam(&self, wparam: WPARAM) -> Option<CommandButtonId> {
        self.view.command_button_id_from_wparam(wparam)
    }

    fn handle_category_selection_changed(&mut self, hwnd: HWND) -> AppResult<()> {
        let Some(index) = self.view.selected_category_index() else {
            return Ok(());
        };

        self.apply_category_selection_changed(hwnd, index)
    }

    fn apply_category_selection_changed(&mut self, hwnd: HWND, index: usize) -> AppResult<()> {
        let previous = self.session.command_panel_snapshot();
        if let Err(error) = self.session.select_command_category_by_index(index) {
            self.rollback_command_panel_change(hwnd, previous, &error)?;
            return Err(error);
        }

        if let Err(error) = self.refresh_command_panel_controls_with_button_scroll_position(hwnd, 0)
        {
            self.rollback_command_panel_change(hwnd, previous, &error)?;
            return Err(error);
        }

        Ok(())
    }

    fn context_menu_request(
        &self,
        hwnd: HWND,
        source: HWND,
        screen_point: UiPoint,
    ) -> Option<ContextMenuRequest> {
        if controls::is_category_combo_child(hwnd, source) {
            let state = self.session.category_menu_state();
            return Some(ContextMenuRequest::Category {
                point: screen_point,
                state,
            });
        }

        self.view
            .command_button_id_from_hwnd(source)
            .map(|button_id| ContextMenuRequest::Button {
                button_id,
                point: screen_point,
                state: self.session.button_menu_state(button_id),
            })
    }

    fn suggested_new_command_category_name(&self) -> String {
        self.session.suggested_new_command_category_name()
    }

    fn selected_command_category_name(&self) -> AppResult<String> {
        self.session.selected_command_category_name()
    }

    fn add_command_category(&mut self, hwnd: HWND, name: String) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| session.add_command_category_named(name))
    }

    fn rename_selected_command_category(&mut self, hwnd: HWND, name: String) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| {
            session.rename_selected_command_category(name)
        })
    }

    fn delete_selected_command_category(&mut self, hwnd: HWND) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| session.delete_selected_command_category())
    }

    fn move_selected_command_category_up(&mut self, hwnd: HWND) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| session.move_selected_command_category_up())
    }

    fn move_selected_command_category_down(&mut self, hwnd: HWND) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| {
            session.move_selected_command_category_down()
        })
    }

    fn new_button_definition() -> AppResult<CommandButtonDefinition> {
        CommandButtonDefinition::new("new command", "echo", CommandArguments::new("{inputtext}")?)
    }

    fn add_button(&mut self, hwnd: HWND, definition: CommandButtonDefinition) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| {
            session.add_button_to_selected_category(definition)
        })
    }

    fn button_definition(&self, button_id: CommandButtonId) -> AppResult<CommandButtonDefinition> {
        Ok(self.session.command_button(button_id)?.definition())
    }

    fn terminal_font(&self) -> TerminalFont {
        self.session.terminal_font().clone()
    }

    fn change_terminal_font(&mut self, hwnd: HWND, terminal_font: TerminalFont) -> AppResult<()> {
        let previous_font = self.session.terminal_font().clone();
        if previous_font == terminal_font {
            return Ok(());
        }

        let resize = self.view.set_terminal_font(hwnd, terminal_font.clone())?;
        if let Err(error) = self.apply_client_resize(resize) {
            self.rollback_terminal_font(hwnd, previous_font, &error)?;
            return Err(error);
        }

        if let Err(error) = self.session.set_terminal_font(terminal_font) {
            self.rollback_terminal_font(hwnd, previous_font, &error)?;
            return Err(error);
        }

        self.view.invalidate(hwnd);
        Ok(())
    }

    fn update_button(
        &mut self,
        hwnd: HWND,
        button_id: CommandButtonId,
        definition: CommandButtonDefinition,
    ) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| {
            session.update_command_button(button_id, definition)
        })
    }

    fn delete_button(&mut self, hwnd: HWND, button_id: CommandButtonId) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| session.delete_command_button(button_id))
    }

    fn move_button_up(&mut self, hwnd: HWND, button_id: CommandButtonId) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| session.move_command_button_up(button_id))
    }

    fn move_button_down(&mut self, hwnd: HWND, button_id: CommandButtonId) -> AppResult<()> {
        self.save_command_panel_change(hwnd, |session| session.move_command_button_down(button_id))
    }

    fn run_button_command(&mut self, command: PendingButtonCommand) -> AppResult<()> {
        let dialect = self.session.active_shell_command_dialect()?;
        let command_text = command.button.to_command_text(&command.values, dialect)?;
        self.session.run_command_text(&command_text)
    }

    fn paint(&mut self, hwnd: HWND) -> AppResult<()> {
        if !self.view.has_terminal_viewport() {
            self.refresh_terminal_viewport()?;
        }

        self.view.paint(hwnd)
    }

    fn start_timer(&mut self, hwnd: HWND) -> AppResult<()> {
        self.runtime.start_timer(hwnd)
    }

    fn stop_timer(&mut self, hwnd: HWND) -> AppResult<()> {
        self.runtime.stop_timer(hwnd)
    }

    fn shutdown(&mut self, hwnd: HWND) -> AppResult<()> {
        self.runtime.clear_pending_char();
        let _ = self.runtime.cancel_splitter_drag();
        self.runtime.cancel_terminal_selection_drag();
        let timer_result = self.stop_timer(hwnd);
        let command_panel_result = self
            .save_pending_command_panel_change(hwnd)
            .and_then(|()| self.session.save_pending_command_panel_selection());
        let session_result = self.session.execute(TerminalCommand::Shutdown);

        timer_result?;
        command_panel_result?;
        session_result
    }

    fn resize_to_client(&mut self, hwnd: HWND) -> AppResult<ClientResizeOutcome> {
        let resize = self.view.resize_to_client(hwnd)?;
        self.apply_client_resize(resize)
    }

    fn apply_client_resize(&mut self, resize: ClientResize) -> AppResult<ClientResizeOutcome> {
        if let Err(error) = self
            .session
            .execute(TerminalCommand::Resize(resize.terminal_size))
        {
            self.view
                .rollback_command_panel_resize(resize.previous, &error)?;
            return Err(error);
        }

        let should_refresh_viewport =
            resize.terminal_grid_changed || !self.view.has_terminal_viewport();
        if should_refresh_viewport {
            self.refresh_terminal_viewport()?;
        }
        if resize.terminal_grid_changed {
            self.view.clear_terminal_selection();
        }

        Ok(ClientResizeOutcome {
            terminal_grid_changed: resize.terminal_grid_changed,
            refreshed_terminal_viewport: should_refresh_viewport,
        })
    }

    fn refresh_tab_views(&mut self) {
        self.view.set_tab_views(self.session.tab_views());
    }

    fn refresh_command_panel_controls(&mut self, hwnd: HWND) -> AppResult<()> {
        let command_panel = self.session.command_panel_handle();
        self.view
            .sync_command_panel_to_client(hwnd, command_panel)?;
        #[cfg(test)]
        {
            if !hwnd.is_null() {
                self.view.invalidate(hwnd);
            }
        }
        #[cfg(not(test))]
        self.view.invalidate(hwnd);
        Ok(())
    }

    fn refresh_command_panel_controls_with_button_scroll_position(
        &mut self,
        hwnd: HWND,
        command_button_scroll_position: usize,
    ) -> AppResult<()> {
        let command_panel = self.session.command_panel_handle();
        self.view
            .sync_command_panel_to_client_with_button_scroll_position(
                hwnd,
                command_panel,
                command_button_scroll_position,
            )?;
        #[cfg(test)]
        {
            if !hwnd.is_null() {
                self.view.invalidate(hwnd);
            }
        }
        #[cfg(not(test))]
        self.view.invalidate(hwnd);
        Ok(())
    }

    fn save_command_panel_change(
        &mut self,
        hwnd: HWND,
        change: impl FnOnce(&mut WindowSessionCoordinator) -> AppResult<()>,
    ) -> AppResult<()> {
        self.save_command_panel_change_at(hwnd, change, Instant::now())
    }

    fn save_command_panel_change_at(
        &mut self,
        hwnd: HWND,
        change: impl FnOnce(&mut WindowSessionCoordinator) -> AppResult<()>,
        now: Instant,
    ) -> AppResult<()> {
        let previous = self.session.command_panel_snapshot();

        if let Err(error) = change(&mut self.session) {
            self.rollback_command_panel_change(hwnd, previous, &error)?;
            return Err(error);
        }

        if let Err(error) = self.refresh_command_panel_controls(hwnd) {
            self.rollback_command_panel_change(hwnd, previous, &error)?;
            return Err(error);
        }

        self.session.defer_command_panel_save(previous, now);
        Ok(())
    }

    fn save_due_command_panel_change(&mut self, hwnd: HWND, now: Instant) -> AppResult<()> {
        if let Err(error) = self.session.save_due_command_panel_change(now) {
            self.refresh_command_panel_controls_after_rollback(hwnd, &error)?;
            return Err(error);
        }

        Ok(())
    }

    fn save_pending_command_panel_change(&mut self, hwnd: HWND) -> AppResult<()> {
        if let Err(error) = self.session.save_pending_command_panel_change() {
            self.refresh_command_panel_controls_after_rollback(hwnd, &error)?;
            return Err(error);
        }

        Ok(())
    }

    fn rollback_command_panel_change(
        &mut self,
        hwnd: HWND,
        snapshot: CommandPanelSessionSnapshot,
        original_error: &AppError,
    ) -> AppResult<()> {
        self.session.restore_command_panel(snapshot);
        if let Err(refresh_error) = self.refresh_command_panel_controls(hwnd) {
            return Err(AppError::ui_message(
                "rollback command panel controls",
                format!(
                    "{original_error}; additionally failed to refresh command panel controls: {refresh_error}"
                ),
            ));
        }

        Ok(())
    }

    fn rollback_terminal_font(
        &mut self,
        hwnd: HWND,
        previous_font: TerminalFont,
        original_error: &AppError,
    ) -> AppResult<()> {
        let resize = self.view.set_terminal_font(hwnd, previous_font)?;
        if let Err(rollback_error) = self.apply_client_resize(resize) {
            return Err(AppError::ui_message(
                "rollback terminal font",
                format!(
                    "{original_error}; additionally failed to rollback terminal font: {rollback_error}"
                ),
            ));
        }

        Ok(())
    }

    fn refresh_command_panel_controls_after_rollback(
        &mut self,
        hwnd: HWND,
        original_error: &AppError,
    ) -> AppResult<()> {
        if let Err(refresh_error) = self.refresh_command_panel_controls(hwnd) {
            return Err(AppError::ui_message(
                "rollback command panel controls",
                format!(
                    "{original_error}; additionally failed to refresh command panel controls: {refresh_error}"
                ),
            ));
        }

        Ok(())
    }

    fn refresh_terminal_viewport(&mut self) -> AppResult<()> {
        let session = &mut self.session;
        let view = &mut self.view;
        if let Some(viewport) = view.terminal_viewport_mut() {
            session.refresh_terminal_viewport(viewport)?;
        } else {
            let viewport = session.terminal_viewport()?;
            view.set_terminal_viewport(viewport);
        }
        view.sync_terminal_scrollbar()
    }

    fn replace_terminal_viewport_cache(&mut self) -> AppResult<()> {
        let viewport = self.session.terminal_viewport()?;
        let previous = self.view.replace_terminal_viewport(viewport);
        if let Err(error) = self.view.sync_terminal_scrollbar() {
            self.view.restore_terminal_viewport(previous);
            return Err(error);
        }
        Ok(())
    }

    fn refresh_active_tab_viewport_cache(&mut self) -> AppResult<()> {
        self.view.clear_terminal_viewport();
        self.refresh_terminal_viewport()
    }

    fn sync_tab_view_after_structural_change(&mut self, hwnd: HWND) -> AppResult<()> {
        self.refresh_tab_views();
        self.resize_to_client(hwnd)?;
        self.refresh_active_tab_viewport_cache()
    }

    fn sync_tab_view_after_active_change(&mut self) -> AppResult<()> {
        self.refresh_tab_views();
        self.refresh_active_tab_viewport_cache()
    }

    fn refresh_terminal_viewport_invalidation(
        &mut self,
    ) -> AppResult<TerminalViewportInvalidation> {
        let previous = self
            .view
            .terminal_viewport
            .as_ref()
            .map(|viewport| viewport.change_baseline());
        self.refresh_terminal_viewport()?;

        let invalidation = match (previous.as_ref(), self.view.terminal_viewport.as_ref()) {
            (Some(previous), Some(current)) => {
                match current.changed_rows_since_baseline(previous) {
                    Some(rows) => TerminalViewportInvalidation::Rows(rows),
                    None => TerminalViewportInvalidation::None,
                }
            }
            (None, Some(_)) => TerminalViewportInvalidation::Full,
            (_, None) => TerminalViewportInvalidation::None,
        };
        Ok(invalidation)
    }

    fn create_command_controls(&mut self, parent: HWND) -> AppResult<()> {
        self.view.create_command_controls(parent)
    }

    fn apply_window_icons(&mut self, hwnd: HWND) -> AppResult<()> {
        self.view.apply_window_icons(hwnd)
    }

    fn drain_pty(&mut self, hwnd: HWND) -> AppResult<()> {
        let drain = self.session.drain_timer_events()?;
        if let Some(cause) = drain.failure_cause {
            self.last_error.set(cause);
        }

        let timer_result = self
            .runtime
            .update_pty_timer_after_drain(hwnd, drain.had_events);
        let command_panel_result = self.save_due_command_panel_change(hwnd, Instant::now());

        if drain.active_tab_dirty {
            let selection_cleared = self.view.clear_terminal_selection();
            match self.refresh_terminal_viewport_invalidation()? {
                TerminalViewportInvalidation::None => {
                    if selection_cleared {
                        self.view.invalidate_terminal_content(hwnd);
                    }
                }
                TerminalViewportInvalidation::Rows(rows) => {
                    if selection_cleared {
                        self.view.invalidate_terminal_content(hwnd);
                    } else {
                        self.view.invalidate_terminal_content_rows(hwnd, rows);
                    }
                }
                TerminalViewportInvalidation::Full => {
                    self.view.invalidate_terminal_content(hwnd);
                }
            }
        }

        timer_result.and(command_panel_result)
    }

    fn handle_left_button_down(&mut self, hwnd: HWND, point: UiPoint) -> AppResult<()> {
        if self.view.splitter_contains(point) {
            self.runtime
                .start_splitter_drag(hwnd, point, self.view.splitter());
            set_resize_cursor();
            focus_main_window(hwnd);
            return Ok(());
        }

        if let Some(id) = self.view.tab_close_at(point) {
            self.session.close_tab(id)?;
            self.sync_tab_view_after_structural_change(hwnd)?;
            self.view.clear_terminal_selection();
            focus_main_window(hwnd);
            self.view.invalidate(hwnd);
            return Ok(());
        }

        if self.view.new_tab_at(point) {
            self.session.open_tab()?;
            self.sync_tab_view_after_structural_change(hwnd)?;
            self.view.clear_terminal_selection();
            focus_main_window(hwnd);
            self.view.invalidate(hwnd);
            return Ok(());
        }

        if let Some(id) = self.view.tab_at(point) {
            self.session.switch_to_tab(id)?;
            self.sync_tab_view_after_active_change()?;
            self.view.clear_terminal_selection();
            focus_main_window(hwnd);
            self.view.invalidate(hwnd);
            return Ok(());
        }

        if let Some(anchor) = self.view.terminal_grid_point_at(point, false) {
            self.runtime.start_terminal_selection_drag(hwnd, anchor);
            if self.view.clear_terminal_selection() {
                self.view.invalidate_terminal_content(hwnd);
            }
            focus_main_window(hwnd);
            return Ok(());
        }

        self.focus_terminal_if_inside(hwnd, point);
        Ok(())
    }

    fn handle_mouse_move(&mut self, hwnd: HWND, point: UiPoint) -> AppResult<()> {
        if let Some(anchor) = self.runtime.terminal_selection_drag_anchor() {
            if let Some(focus) = self.view.terminal_grid_point_at(point, true)
                && self.view.update_terminal_selection(anchor, focus)
            {
                self.view.invalidate_terminal_content(hwnd);
            }
            return Ok(());
        }

        if let Some(splitter_x) = self.runtime.splitter_drag_x(point) {
            set_resize_cursor();
            self.resize_command_panel_from_splitter(hwnd, splitter_x)?;
            return Ok(());
        }

        if self.view.splitter_contains(point) {
            set_resize_cursor();
        }

        Ok(())
    }

    fn handle_left_button_up(&mut self, hwnd: HWND) -> AppResult<()> {
        if self.runtime.is_splitter_dragging() {
            let deferred_terminal_size = self.runtime.finish_splitter_drag()?;
            self.apply_deferred_splitter_terminal_resize(deferred_terminal_size)?;
            focus_main_window(hwnd);
        }
        if self.runtime.is_terminal_selection_dragging() {
            self.runtime.finish_terminal_selection_drag()?;
            focus_main_window(hwnd);
        }

        Ok(())
    }

    fn handle_capture_changed(&mut self) -> AppResult<()> {
        let deferred_terminal_size = self.runtime.cancel_splitter_drag();
        self.runtime.cancel_terminal_selection_drag();
        self.apply_deferred_splitter_terminal_resize(deferred_terminal_size)
    }

    fn resize_command_panel_from_splitter(&mut self, hwnd: HWND, splitter_x: i32) -> AppResult<()> {
        let Some(resize) = self
            .view
            .resize_command_panel_from_splitter(hwnd, splitter_x)?
        else {
            return Ok(());
        };

        self.apply_command_panel_resize(hwnd, resize)
    }

    fn apply_command_panel_resize(
        &mut self,
        hwnd: HWND,
        resize: CommandPanelResize,
    ) -> AppResult<()> {
        if self.runtime.is_splitter_dragging()
            && !resize.terminal_grid_changed
            && self.view.has_terminal_viewport()
        {
            self.runtime
                .defer_splitter_terminal_resize(resize.terminal_size);
            self.view.invalidate(hwnd);
            return Ok(());
        }

        if let Err(error) = self
            .session
            .execute(TerminalCommand::Resize(resize.terminal_size))
        {
            self.view
                .rollback_command_panel_resize(resize.previous, &error)?;
            return Err(error);
        }

        let should_refresh_viewport =
            resize.terminal_grid_changed || !self.view.has_terminal_viewport();

        if should_refresh_viewport && let Err(error) = self.replace_terminal_viewport_cache() {
            if let Err(rollback_error) = self
                .session
                .execute(TerminalCommand::Resize(resize.previous_terminal_size))
            {
                let rollback_error = AppError::ui_message(
                    "rollback command panel resize",
                    format!(
                        "{error}; additionally failed to rollback terminal session size: {rollback_error}"
                    ),
                );
                self.view
                    .rollback_command_panel_resize(resize.previous, &rollback_error)?;
                return Err(rollback_error);
            }

            self.view
                .rollback_command_panel_resize(resize.previous, &error)?;
            return Err(error);
        }

        self.runtime.clear_deferred_splitter_terminal_resize();
        self.view.invalidate(hwnd);
        Ok(())
    }

    fn apply_deferred_splitter_terminal_resize(
        &mut self,
        size: Option<TerminalSize>,
    ) -> AppResult<()> {
        let Some(size) = size else {
            return Ok(());
        };

        self.session.execute(TerminalCommand::Resize(size))
    }

    fn focus_terminal_if_inside(&self, hwnd: HWND, point: UiPoint) {
        if self.view.terminal_contains(point) {
            focus_main_window(hwnd);
        }
    }

    fn take_last_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    fn record_error(&mut self, error: AppError) {
        let user_message = error.user_message().to_owned();
        let cause = error.to_string();

        match self.session.display_recoverable_error(&user_message) {
            Err(display_error) => {
                self.last_error.set(format!(
                    "{cause}; additionally failed to display user-facing error: {display_error}"
                ));
            }
            Ok(()) => {
                self.last_error.set(cause.clone());
                if let Err(refresh_error) = self.refresh_terminal_viewport() {
                    self.last_error.set(format!(
                        "{cause}; additionally failed to refresh terminal viewport: {refresh_error}"
                    ));
                }
            }
        }
    }

    fn record_shutdown_error(&mut self, error: AppError) {
        self.record_error(error);

        let Some(message) = self.last_error.as_deref() else {
            return;
        };
        if let Ok(mut shutdown_error) = self.shutdown_error.try_borrow_mut() {
            *shutdown_error = Some(message.to_owned());
        }
    }

    fn record_paint_error(&mut self, error: AppError) {
        self.last_error.set(error.to_string());
    }

    fn record_initialization_error(&mut self, error: AppError) {
        self.last_error.set(error.to_string());
    }
}

fn layout_from_client_rect(
    rect: RECT,
    command_panel_width: i32,
    buttons: &[CommandButton],
    tabs: &[TerminalTabView],
    command_button_scroll_position: usize,
) -> WindowLayout {
    let width = rect.right.saturating_sub(rect.left).max(1);
    let height = rect.bottom.saturating_sub(rect.top).max(1);
    layout_from_client_size(
        width,
        height,
        command_panel_width,
        buttons,
        tabs,
        command_button_scroll_position,
    )
}

fn layout_from_client_size(
    width: i32,
    height: i32,
    command_panel_width: i32,
    buttons: &[CommandButton],
    tabs: &[TerminalTabView],
    command_button_scroll_position: usize,
) -> WindowLayout {
    WindowLayout::for_client_with_command_panel_width_and_button_scroll(
        width,
        height,
        command_panel_width,
        buttons,
        tabs,
        command_button_scroll_position,
    )
}

fn scrolled_position_by_lines(current: usize, max_position: usize, line_delta: i32) -> usize {
    if line_delta < 0 {
        current.saturating_sub(usize_from_u32_saturating(line_delta.unsigned_abs()))
    } else {
        current
            .saturating_add(usize_from_u32_saturating(line_delta as u32))
            .min(max_position)
    }
}

fn command_button_scroll_lines_from_wheel_delta(wheel_delta: i32) -> i32 {
    const WHEEL_DELTA: i32 = 120;
    const LINES_PER_WHEEL_STEP: i32 = 3;

    if wheel_delta == 0 {
        return 0;
    }

    let steps = if wheel_delta.unsigned_abs() >= WHEEL_DELTA as u32 {
        wheel_delta / WHEEL_DELTA
    } else {
        wheel_delta.signum()
    };

    steps.saturating_mul(-LINES_PER_WHEEL_STEP)
}

fn terminal_scroll_lines_from_wheel_delta(wheel_delta: i32) -> i32 {
    command_button_scroll_lines_from_wheel_delta(wheel_delta).saturating_neg()
}

fn usize_from_u32_saturating(value: u32) -> usize {
    match usize::try_from(value) {
        Ok(value) => value,
        Err(_) => usize::MAX,
    }
}

fn usize_from_i32_saturating(value: i32) -> usize {
    if value <= 0 {
        0
    } else {
        match usize::try_from(value) {
            Ok(value) => value,
            Err(_) => usize::MAX,
        }
    }
}

fn i32_from_usize_saturating(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn terminal_size_from_area(area: UiRect, metrics: CellMetrics) -> AppResult<TerminalSize> {
    let content_area = terminal_content_area(area);
    let width = content_area.width.max(1);
    let height = content_area.height.max(1);
    let columns = (width / metrics.width.max(1)).max(i32::from(crate::domain::MIN_COLUMNS));
    let rows = (height / metrics.height.max(1)).max(i32::from(crate::domain::MIN_ROWS));

    TerminalSize::with_pixels(
        to_u16_saturating(rows),
        to_u16_saturating(columns),
        to_u16_saturating(width),
        to_u16_saturating(height),
    )
}

fn terminal_rows_rect(
    content_area: UiRect,
    metrics: CellMetrics,
    rows: Range<usize>,
    visible_rows: usize,
) -> Option<UiRect> {
    if rows.start >= rows.end || content_area.width <= 0 || content_area.height <= 0 {
        return None;
    }

    let start = rows.start.min(visible_rows);
    let end = rows.end.min(visible_rows);
    if start >= end {
        return None;
    }

    let row_height = metrics.height.max(1);
    let top_offset = i32_from_usize_saturating(start).saturating_mul(row_height);
    if top_offset >= content_area.height {
        return None;
    }

    let bottom_offset = if end >= visible_rows {
        content_area.height
    } else {
        i32_from_usize_saturating(end)
            .saturating_mul(row_height)
            .min(content_area.height)
    };
    let height = bottom_offset.saturating_sub(top_offset);
    if height <= 0 {
        return None;
    }

    Some(UiRect {
        x: content_area.x,
        y: content_area.y.saturating_add(top_offset),
        width: content_area.width,
        height,
    })
}

fn terminal_grid_changed(previous: TerminalSize, next: TerminalSize) -> bool {
    previous.rows != next.rows || previous.columns != next.columns
}

fn collect_button_argument_values(
    hwnd: HWND,
    button: &CommandButton,
) -> AppResult<Option<ButtonArgumentValues>> {
    let inputs = button.required_argument_inputs();
    if !inputs.any() {
        return Ok(Some(ButtonArgumentValues::default()));
    }

    let mut values = ButtonArgumentValues::default();
    if inputs.select_file {
        let Some(path) = dialogs::select_file(hwnd, "Select File")? else {
            return Ok(None);
        };
        values.selected_file = Some(path);
    }

    if inputs.select_dir {
        let Some(path) = dialogs::select_folder(hwnd)? else {
            return Ok(None);
        };
        values.selected_dir = Some(path);
    }

    if inputs.input_text {
        let Some(text) = dialogs::prompt_input_text(hwnd)? else {
            return Ok(None);
        };
        values.input_text = Some(text);
    }

    values.validate_for(inputs)?;
    Ok(Some(values))
}

fn to_u16_saturating(value: i32) -> u16 {
    if value <= 0 {
        0
    } else if value > i32::from(u16::MAX) {
        u16::MAX
    } else {
        value as u16
    }
}

fn set_resize_cursor() {
    // SAFETY: requesting a predefined system cursor with a null instance is valid.
    let cursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_SIZEWE) };
    if cursor.is_null() {
        return;
    }

    // SAFETY: cursor is a shared system cursor handle returned by LoadCursorW.
    unsafe {
        SetCursor(cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CommandCategoryDefinition, CursorPosition, StartupCommand, TerminalCell, TerminalTabId,
    };
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn shutdown_result_returns_detached_cleanup_failure() -> AppResult<()> {
        let result = finish_shutdown_result(
            Ok(()),
            Ok(()),
            Ok(()),
            vec![AppError::pty_message(
                "cleanup detached pty resources",
                "child terminate failed",
            )],
        );

        let error = result
            .err()
            .ok_or(AppError::InvalidState("shutdown result should fail"))?;

        assert_eq!(error.operation(), Some("cleanup detached pty resources"));
        assert!(error.to_string().contains("child terminate failed"));
        Ok(())
    }

    #[test]
    fn shutdown_result_returns_window_shutdown_failure() -> AppResult<()> {
        let result = finish_shutdown_result(
            Ok(()),
            Ok(()),
            Err(AppError::ui_message(
                "shutdown window",
                "save selected command category failed",
            )),
            Vec::new(),
        );

        let error = result
            .err()
            .ok_or(AppError::InvalidState("shutdown result should fail"))?;

        assert_eq!(error.operation(), Some("shutdown window"));
        assert!(
            error
                .to_string()
                .contains("save selected command category failed")
        );
        Ok(())
    }

    #[test]
    fn shutdown_result_orders_existing_errors_before_cleanup_failure() -> AppResult<()> {
        let result = finish_shutdown_result(
            Err(AppError::ui_message("message loop", "loop failed")),
            Err(AppError::ui_message("destroy window", "destroy failed")),
            Err(AppError::ui_message(
                "shutdown window",
                "selection save failed",
            )),
            vec![AppError::pty_message(
                "cleanup detached pty resources",
                "child terminate failed",
            )],
        );

        let error = result
            .err()
            .ok_or(AppError::InvalidState("shutdown result should fail"))?;
        let message = error.to_string();
        let Some(loop_index) = message.find("loop failed") else {
            return Err(AppError::InvalidState("missing loop failure"));
        };
        let Some(destroy_index) = message.find("destroy failed") else {
            return Err(AppError::InvalidState("missing destroy failure"));
        };
        let Some(shutdown_index) = message.find("selection save failed") else {
            return Err(AppError::InvalidState("missing shutdown failure"));
        };
        let Some(cleanup_index) = message.find("child terminate failed") else {
            return Err(AppError::InvalidState("missing cleanup failure"));
        };

        assert!(loop_index < destroy_index);
        assert!(destroy_index < shutdown_index);
        assert!(shutdown_index < cleanup_index);
        Ok(())
    }

    #[test]
    fn recorded_window_shutdown_error_reaches_shutdown_result() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut state = WindowState::new_for_test(size, None);

        state.record_shutdown_error(AppError::ui_message(
            "save command panel selection",
            "settings write failed",
        ));
        let shutdown_result = take_shutdown_error_result(&state.shutdown_error);
        let result = finish_shutdown_result(Ok(()), Ok(()), shutdown_result, Vec::new());

        let error = result
            .err()
            .ok_or(AppError::InvalidState("shutdown result should fail"))?;
        let message = error.to_string();

        assert_eq!(error.operation(), Some("shutdown window"));
        assert!(message.contains("settings write failed"));
        Ok(())
    }

    #[test]
    fn paint_error_recording_does_not_mutate_terminal_viewport() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut state = WindowState::new_for_test(size, None);
        let before = state.session.terminal_viewport()?;

        state.record_paint_error(AppError::InvalidState("paint failed"));

        assert_eq!(state.session.terminal_viewport()?, before);
        assert_eq!(
            state.last_error.as_deref(),
            Some("invalid state: paint failed")
        );
        Ok(())
    }

    #[test]
    fn tab_view_update_refreshes_cached_layout_tab_placements() {
        let mut view = WindowViewAdapters::new(
            Rc::new(default_command_panel()),
            vec![TerminalTabView::new(TerminalTabId::new(1), "Tab 1", true)],
            TerminalFont::default(),
        );

        view.set_tab_views(vec![
            TerminalTabView::new(TerminalTabId::new(1), "Tab 1", false),
            TerminalTabView::new(TerminalTabId::new(2), "Tab 2", true),
        ]);

        let active_tabs = view
            .layout
            .tabs
            .iter()
            .filter(|placement| placement.active)
            .map(|placement| placement.id)
            .collect::<Vec<_>>();

        assert_eq!(view.layout.tabs.len(), 2);
        assert_eq!(active_tabs, vec![TerminalTabId::new(2)]);
        assert_eq!(
            view.tab_at(UiPoint {
                x: view.layout.tabs[1].bounds.x,
                y: view.layout.tabs[1].bounds.y,
            }),
            Some(TerminalTabId::new(2))
        );
    }

    #[test]
    fn terminal_viewport_replacement_clears_renderer_line_cache() -> AppResult<()> {
        let mut view = WindowViewAdapters::new(
            Rc::new(default_command_panel()),
            vec![TerminalTabView::new(TerminalTabId::new(1), "Tab 1", true)],
            TerminalFont::default(),
        );
        let blank_cells = vec![TerminalCell::default(); 4];

        view.renderer
            .cache_terminal_line_for_test(&blank_cells, Some(1))?;
        view.set_terminal_viewport(test_viewport("echo")?);
        assert_eq!(view.renderer.terminal_line_cache_len_for_test(), 0);

        view.renderer
            .cache_terminal_line_for_test(&blank_cells, Some(1))?;
        let previous = view.replace_terminal_viewport(test_viewport("next")?);
        assert_eq!(view.renderer.terminal_line_cache_len_for_test(), 0);

        view.renderer
            .cache_terminal_line_for_test(&blank_cells, Some(1))?;
        view.restore_terminal_viewport(previous);
        assert_eq!(view.renderer.terminal_line_cache_len_for_test(), 0);

        view.renderer
            .cache_terminal_line_for_test(&blank_cells, Some(1))?;
        view.clear_terminal_viewport();
        assert_eq!(view.renderer.terminal_line_cache_len_for_test(), 0);
        Ok(())
    }

    #[test]
    fn refreshing_active_tab_viewport_cache_discards_stale_cached_cells() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut state = WindowState::new_for_test(size, None);
        state.view.set_terminal_viewport(test_viewport("stale")?);

        state.refresh_active_tab_viewport_cache()?;

        let viewport = state
            .view
            .terminal_viewport
            .as_ref()
            .ok_or(AppError::InvalidState(
                "terminal viewport cache should be present",
            ))?;
        let line = viewport
            .visible_line(0)
            .ok_or(AppError::InvalidState("terminal viewport row is missing"))?;
        assert_eq!(line.text(), "");
        Ok(())
    }

    #[test]
    fn pty_timer_interval_slows_after_repeated_idle_drains() {
        let mut runtime = WindowRuntimeState::default();

        for _ in 1..PTY_IDLE_TIMER_BACKOFF_TICKS {
            assert_eq!(
                runtime.desired_pty_timer_interval_after_drain(false),
                PTY_ACTIVE_TIMER_MS
            );
        }

        assert_eq!(
            runtime.desired_pty_timer_interval_after_drain(false),
            PTY_IDLE_TIMER_MS
        );
        let idle_ticks_before_sustained =
            PTY_SUSTAINED_IDLE_TIMER_BACKOFF_TICKS - PTY_IDLE_TIMER_BACKOFF_TICKS - 1;
        for _ in 0..idle_ticks_before_sustained {
            assert_eq!(
                runtime.desired_pty_timer_interval_after_drain(false),
                PTY_IDLE_TIMER_MS
            );
        }
        assert_eq!(
            runtime.desired_pty_timer_interval_after_drain(false),
            PTY_SUSTAINED_IDLE_TIMER_MS
        );
        assert_eq!(
            runtime.desired_pty_timer_interval_after_drain(false),
            PTY_SUSTAINED_IDLE_TIMER_MS
        );
    }

    #[test]
    fn pty_timer_interval_returns_to_active_after_event() {
        let mut runtime = WindowRuntimeState::default();

        for _ in 0..PTY_SUSTAINED_IDLE_TIMER_BACKOFF_TICKS {
            runtime.desired_pty_timer_interval_after_drain(false);
        }

        assert_eq!(
            runtime.desired_pty_timer_interval_after_drain(true),
            PTY_ACTIVE_TIMER_MS
        );
        assert_eq!(
            runtime.desired_pty_timer_interval_after_drain(false),
            PTY_ACTIVE_TIMER_MS
        );
    }

    #[test]
    fn startup_command_is_preserved_when_initial_write_fails() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let command = StartupCommand::from_arguments(vec!["echo".to_owned(), "hello".to_owned()])?
            .ok_or(AppError::InvalidInput("startup command should exist"))?;
        let mut session = WindowSessionCoordinator::new_for_test(size, Some(command));

        let result = session.run_startup_command();

        assert!(result.is_err());
        assert!(session.startup.command().is_some());

        let retry_result = session.run_startup_command();

        assert!(retry_result.is_err());
        assert!(session.startup.command().is_some());
        Ok(())
    }

    #[test]
    fn command_panel_handle_does_not_share_session_owner() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut session = WindowSessionCoordinator::new_for_test(size, None);
        let handle = session.command_panel_handle();
        let handle_category_count = handle.categories().len();

        assert_eq!(Rc::strong_count(&handle), 1);

        session.add_command_category()?;

        assert_eq!(handle.categories().len(), handle_category_count);
        assert_ne!(
            session.command_panel().categories().len(),
            handle_category_count
        );
        Ok(())
    }

    #[test]
    fn command_panel_save_failure_rolls_back_session_state() -> AppResult<()> {
        let settings_path = unique_test_settings_path("rollback-save-failure")?;
        fs::create_dir(&settings_path)
            .map_err(|source| AppError::io("create blocking settings directory", source))?;
        let config_store = ConfigStore::for_test_path(settings_path.clone());
        let initial_panel = CommandPanel::from_definitions(
            vec![CommandCategoryDefinition::new("Default", Vec::new())?],
            0,
        )?;
        let size = TerminalSize::new(4, 16)?;
        let mut session = WindowSessionCoordinator::new(
            size,
            StartupInvocation::default(),
            initial_panel.clone(),
            TerminalFont::default(),
            Some(config_store),
        );

        session.save_command_panel_change(|session| session.add_command_category())?;

        let result = session.save_pending_command_panel_change();
        assert!(result.is_err());
        assert_eq!(
            session.command_panel().categories().len(),
            initial_panel.categories().len()
        );
        assert_eq!(
            session.command_panel().categories()[0].name.as_str(),
            initial_panel.categories()[0].name.as_str()
        );
        assert_eq!(
            session.command_panel().selected_category_index(),
            initial_panel.selected_category_index()
        );

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn command_panel_change_defers_settings_file_write_until_due() -> AppResult<()> {
        let settings_path = unique_test_settings_path("deferred-command-panel-change")?;
        let mut session = WindowSessionCoordinator::new(
            TerminalSize::new(4, 16)?,
            StartupInvocation::default(),
            single_category_command_panel()?,
            TerminalFont::default(),
            Some(ConfigStore::for_test_path(settings_path.clone())),
        );
        let now = Instant::now();

        session.save_command_panel_change_at(|session| session.add_command_category(), now)?;

        assert!(!settings_path.exists());

        session.save_due_command_panel_change(
            now + Duration::from_millis(COMMAND_PANEL_SAVE_DELAY_MS - 1),
        )?;

        assert!(!settings_path.exists());

        session.save_due_command_panel_change(
            now + Duration::from_millis(COMMAND_PANEL_SAVE_DELAY_MS),
        )?;
        let loaded_panel = ConfigStore::for_test_path(settings_path.clone()).load_or_default()?;

        assert_eq!(loaded_panel.categories().len(), 2);

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn category_selection_change_defers_settings_file_write_until_flush() -> AppResult<()> {
        let settings_path = unique_test_settings_path("deferred-category-selection")?;
        let mut session = WindowSessionCoordinator::new(
            TerminalSize::new(4, 16)?,
            StartupInvocation::default(),
            two_category_command_panel()?,
            TerminalFont::default(),
            Some(ConfigStore::for_test_path(settings_path.clone())),
        );

        session.select_command_category_by_index(1)?;

        assert!(!settings_path.exists());

        session.save_pending_command_panel_selection()?;
        let loaded_panel = ConfigStore::for_test_path(settings_path.clone()).load_or_default()?;

        assert_eq!(loaded_panel.selected_category_index(), Some(1));

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn structural_save_persists_deferred_category_selection() -> AppResult<()> {
        let settings_path = unique_test_settings_path("structural-save-selection")?;
        let mut session = WindowSessionCoordinator::new(
            TerminalSize::new(4, 16)?,
            StartupInvocation::default(),
            two_category_command_panel()?,
            TerminalFont::default(),
            Some(ConfigStore::for_test_path(settings_path.clone())),
        );
        let definition =
            CommandButtonDefinition::new("echo selected", "echo", CommandArguments::new("saved")?)?;

        session.select_command_category_by_index(1)?;
        session.save_command_panel_change(|session| {
            session.add_button_to_selected_category(definition)
        })?;
        assert!(!settings_path.exists());

        session.save_pending_command_panel_change()?;
        let loaded_panel = ConfigStore::for_test_path(settings_path.clone()).load_or_default()?;

        assert_eq!(loaded_panel.selected_category_index(), Some(1));
        assert_eq!(
            loaded_panel.categories()[1].buttons[0].label.as_str(),
            "echo selected"
        );

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn menu_backed_category_actions_update_panel_and_persist() -> AppResult<()> {
        let settings_path = unique_test_settings_path("menu-backed-category-actions")?;
        let mut state = window_state_with_command_panel(
            TerminalSize::new(4, 16)?,
            two_category_command_panel()?,
            Some(ConfigStore::for_test_path(settings_path.clone())),
        );
        let hwnd = ptr::null_mut();

        let initial_state = state.session.category_menu_state();
        assert!(initial_state.can_delete);
        assert!(!initial_state.can_move_up);
        assert!(initial_state.can_move_down);

        state.view.push_command_panel_sync_success();
        state.add_command_category(hwnd, "Build".to_owned())?;
        assert_eq!(state.session.selected_command_category_name()?, "Build");
        assert!(!settings_path.exists());

        state.view.push_command_panel_sync_success();
        state.rename_selected_command_category(hwnd, "Build Tools".to_owned())?;
        assert_eq!(
            state.session.selected_command_category_name()?,
            "Build Tools"
        );

        state.view.push_command_panel_sync_success();
        state.move_selected_command_category_up(hwnd)?;
        assert_eq!(
            state.session.command_panel().selected_category_index(),
            Some(1)
        );

        state.view.push_command_panel_sync_success();
        state.move_selected_command_category_down(hwnd)?;
        assert_eq!(
            state.session.command_panel().selected_category_index(),
            Some(2)
        );

        state.view.push_command_panel_sync_success();
        state.add_button(
            hwnd,
            CommandButtonDefinition::new("echo build", "echo", CommandArguments::new("build")?)?,
        )?;
        assert_eq!(state.session.command_panel().selected_buttons().len(), 1);

        state.view.push_command_panel_sync_success();
        state.delete_selected_command_category(hwnd)?;
        assert_eq!(state.session.command_panel().categories().len(), 2);

        state.save_pending_command_panel_change(hwnd)?;
        let loaded_panel = ConfigStore::for_test_path(settings_path.clone()).load_or_default()?;
        let category_names = loaded_panel
            .categories()
            .iter()
            .map(|category| category.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(category_names, vec!["Default", "Tools"]);

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn menu_backed_button_actions_prepare_update_move_delete_and_persist() -> AppResult<()> {
        let settings_path = unique_test_settings_path("menu-backed-button-actions")?;
        let first =
            CommandButtonDefinition::new("echo one", "echo", CommandArguments::new("one")?)?;
        let second =
            CommandButtonDefinition::new("echo two", "echo", CommandArguments::new("two")?)?;
        let panel = CommandPanel::from_definitions(
            vec![CommandCategoryDefinition::new(
                "Default",
                vec![first, second],
            )?],
            0,
        )?;
        let mut state = window_state_with_command_panel(
            TerminalSize::new(4, 16)?,
            panel,
            Some(ConfigStore::for_test_path(settings_path.clone())),
        );
        let hwnd = ptr::null_mut();
        let first_id = state.session.command_panel().selected_buttons()[0].id;
        let second_id = state.session.command_panel().selected_buttons()[1].id;

        let first_menu_state = state.session.button_menu_state(first_id);
        assert!(!first_menu_state.can_move_up);
        assert!(first_menu_state.can_move_down);

        let prompt = state.prepare_button_command(first_id)?;
        let pending = prompt.collect_values(hwnd)?.ok_or(AppError::InvalidState(
            "button command should not be canceled",
        ))?;
        let command_text = pending.button.to_command_text(
            &pending.values,
            crate::domain::ShellCommandDialect::CommandPrompt,
        )?;
        assert_eq!(command_text.to_pty_bytes(), b"echo one\r".to_vec());

        state.view.push_command_panel_sync_success();
        state.update_button(
            hwnd,
            second_id,
            CommandButtonDefinition::new("echo edited", "echo", CommandArguments::new("edited")?)?,
        )?;
        assert_eq!(
            state.session.command_panel().selected_buttons()[1]
                .definition()
                .label,
            "echo edited"
        );

        state.view.push_command_panel_sync_success();
        state.move_button_up(hwnd, second_id)?;
        assert_eq!(
            state.session.command_panel().selected_buttons()[0].id,
            second_id
        );

        state.view.push_command_panel_sync_success();
        state.move_button_down(hwnd, second_id)?;
        assert_eq!(
            state.session.command_panel().selected_buttons()[1].id,
            second_id
        );

        state.view.push_command_panel_sync_success();
        state.delete_button(hwnd, second_id)?;
        assert_eq!(state.session.command_panel().selected_buttons().len(), 1);

        state.save_pending_command_panel_change(hwnd)?;
        let loaded_panel = ConfigStore::for_test_path(settings_path.clone()).load_or_default()?;
        assert_eq!(loaded_panel.selected_buttons().len(), 1);
        assert_eq!(
            loaded_panel.selected_buttons()[0].label.as_str(),
            "echo one"
        );

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn category_selection_change_resets_button_scroll_with_single_refresh() -> AppResult<()> {
        let initial_panel = two_category_command_panel()?;
        let mut state =
            window_state_with_command_panel(TerminalSize::new(4, 16)?, initial_panel, None);
        state.view.command_button_scroll_position = 3;
        state.view.push_command_panel_sync_success();

        state.apply_category_selection_changed(std::ptr::null_mut(), 1)?;

        assert_eq!(
            state.session.command_panel().selected_category_index(),
            Some(1)
        );
        assert_eq!(state.view.command_panel.selected_category_index(), Some(1));
        assert_eq!(state.view.command_button_scroll_position, 0);
        assert!(state.view.command_panel_sync_overrides.is_empty());
        Ok(())
    }

    #[test]
    fn category_selection_change_sync_failure_rolls_back_selection_and_scroll() -> AppResult<()> {
        let initial_panel = two_category_command_panel()?;
        let mut state =
            window_state_with_command_panel(TerminalSize::new(4, 16)?, initial_panel.clone(), None);
        state.view.command_button_scroll_position = 3;
        state
            .view
            .push_command_panel_sync_failure("CreateWindowExW command button");
        state.view.push_command_panel_sync_success();

        let result = state.apply_category_selection_changed(std::ptr::null_mut(), 1);

        assert!(result.is_err());
        assert_command_panel_matches(state.session.command_panel(), &initial_panel);
        assert_command_panel_matches(&state.view.command_panel, &initial_panel);
        assert_eq!(state.view.command_button_scroll_position, 3);
        assert!(state.view.command_panel_sync_overrides.is_empty());
        Ok(())
    }

    #[test]
    fn refresh_command_panel_controls_failure_keeps_view_panel() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut state = WindowState::new_for_test(size, None);
        let initial_panel = state.view.command_panel.clone();
        state.session.add_command_category()?;
        state
            .view
            .push_command_panel_sync_failure("CreateWindowExW command button");

        let result = state.refresh_command_panel_controls(std::ptr::null_mut());

        assert!(result.is_err());
        assert_command_panel_matches(&state.view.command_panel, &initial_panel);
        assert_ne!(
            state.session.command_panel().categories().len(),
            initial_panel.categories().len()
        );
        Ok(())
    }

    #[test]
    fn partial_command_panel_control_sync_failure_rolls_back_view_and_buttons() -> AppResult<()> {
        let initial_panel = single_button_command_panel("initial")?;
        let mut state =
            window_state_with_command_panel(TerminalSize::new(4, 16)?, initial_panel.clone(), None);
        state
            .view
            .command_controls
            .sync_buttons_for_test(&initial_panel)?;
        let initial_button_ids = state.view.command_controls.button_ids_for_test();
        let next_panel = single_category_command_panel()?;
        state
            .view
            .push_command_panel_sync_apply_controls_then_failure("MoveWindow command button");

        let result = state
            .view
            .sync_command_panel_to_client(std::ptr::null_mut(), Rc::new(next_panel));

        assert!(result.is_err());
        assert_command_panel_matches(&state.view.command_panel, &initial_panel);
        assert_eq!(
            state.view.command_controls.button_ids_for_test(),
            initial_button_ids
        );
        Ok(())
    }

    #[test]
    fn save_command_panel_change_sync_failure_rolls_back_before_save() -> AppResult<()> {
        let settings_path = unique_test_settings_path("rollback-sync-failure")?;
        let initial_panel = single_category_command_panel()?;
        let mut state = window_state_with_command_panel(
            TerminalSize::new(4, 16)?,
            initial_panel.clone(),
            Some(ConfigStore::for_test_path(settings_path.clone())),
        );
        state
            .view
            .push_command_panel_sync_failure("CreateWindowExW command button");
        state.view.push_command_panel_sync_success();

        let result = state.save_command_panel_change(std::ptr::null_mut(), |session| {
            session.add_command_category()
        });

        assert!(result.is_err());
        assert_command_panel_matches(state.session.command_panel(), &initial_panel);
        assert_command_panel_matches(&state.view.command_panel, &initial_panel);
        assert!(!settings_path.exists());

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn pending_command_panel_save_failure_rolls_back_view_state() -> AppResult<()> {
        let settings_path = unique_test_settings_path("rollback-window-save-failure")?;
        fs::create_dir(&settings_path)
            .map_err(|source| AppError::io("create blocking settings directory", source))?;
        let initial_panel = single_category_command_panel()?;
        let mut state = window_state_with_command_panel(
            TerminalSize::new(4, 16)?,
            initial_panel.clone(),
            Some(ConfigStore::for_test_path(settings_path.clone())),
        );
        state.view.push_command_panel_sync_success();
        state.view.push_command_panel_sync_success();

        state.save_command_panel_change(std::ptr::null_mut(), |session| {
            session.add_command_category()
        })?;

        let result = state.save_pending_command_panel_change(std::ptr::null_mut());
        assert!(result.is_err());
        assert_command_panel_matches(state.session.command_panel(), &initial_panel);
        assert_command_panel_matches(&state.view.command_panel, &initial_panel);

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn splitter_resize_failure_rolls_back_view_state() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut state = WindowState::new_for_test(size, None);
        let initial_width = state.view.command_panel_width;
        let initial_scroll = state.view.command_button_scroll_position;
        let initial_layout = state.view.layout.clone();
        let initial_viewport = state.view.terminal_viewport.clone();
        let splitter_x = state.view.layout.splitter.x.saturating_sub(32);
        state
            .session
            .push_terminal_command_failure("ResizePseudoConsole");

        let result = state.resize_command_panel_from_splitter(std::ptr::null_mut(), splitter_x);

        assert!(result.is_err());
        assert_command_panel_resize_state_matches(
            &state.view,
            initial_width,
            initial_scroll,
            &initial_layout,
            &initial_viewport,
        );
        Ok(())
    }

    #[test]
    fn splitter_viewport_refresh_failure_rolls_back_view_state() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut state = WindowState::new_for_test(size, None);
        state.refresh_terminal_viewport()?;
        let initial_width = state.view.command_panel_width;
        let initial_scroll = state.view.command_button_scroll_position;
        let initial_layout = state.view.layout.clone();
        let initial_viewport = state.view.terminal_viewport.clone();
        let splitter_x = state.view.layout.splitter.x.saturating_sub(32);
        state
            .session
            .push_terminal_viewport_refresh_failure("refresh terminal viewport");

        let result = state.resize_command_panel_from_splitter(std::ptr::null_mut(), splitter_x);

        assert!(result.is_err());
        assert_command_panel_resize_state_matches(
            &state.view,
            initial_width,
            initial_scroll,
            &initial_layout,
            &initial_viewport,
        );
        Ok(())
    }

    #[test]
    fn window_resize_failure_rolls_back_view_state() -> AppResult<()> {
        let size = TerminalSize::new(4, 16)?;
        let mut state = WindowState::new_for_test(size, None);
        state.view.layout = WindowLayout::for_client_with_command_panel_width(
            WINDOW_WIDTH.saturating_sub(80),
            WINDOW_HEIGHT.saturating_sub(40),
            state.view.command_panel_width,
            state.view.command_panel.selected_buttons(),
            &state.view.tab_views,
        );
        state.view.command_button_scroll_position = 3;
        state.refresh_terminal_viewport()?;
        let initial_width = state.view.command_panel_width;
        let initial_scroll = state.view.command_button_scroll_position;
        let initial_layout = state.view.layout.clone();
        let initial_viewport = state.view.terminal_viewport.clone();
        state
            .session
            .push_terminal_command_failure("ResizePseudoConsole");

        let result = state.resize_to_client(std::ptr::null_mut());

        assert!(result.is_err());
        assert_command_panel_resize_state_matches(
            &state.view,
            initial_width,
            initial_scroll,
            &initial_layout,
            &initial_viewport,
        );
        Ok(())
    }

    #[test]
    fn window_resize_same_grid_skips_viewport_refresh_and_full_invalidation() -> AppResult<()> {
        let current_size = TerminalSize::with_pixels(24, 80, 640, 384)?;
        let mut state = WindowState::new_for_test(current_size, None);
        state.refresh_terminal_viewport()?;
        let initial_viewport = state.view.terminal_viewport.clone();
        state.session.terminal_resize_commands.clear();
        state
            .session
            .push_terminal_viewport_refresh_failure("refresh terminal viewport");
        let resized = TerminalSize::with_pixels(24, 80, 648, 384)?;
        assert!(!terminal_grid_changed(current_size, resized));
        let resize = ClientResize {
            previous: state.view.command_panel_resize_snapshot(),
            terminal_size: resized,
            terminal_grid_changed: false,
        };

        let outcome = state.apply_client_resize(resize)?;

        assert_eq!(state.session.terminal_resize_commands, vec![resized]);
        assert_eq!(state.view.terminal_viewport, initial_viewport);
        assert_eq!(state.session.terminal_viewport_refresh_overrides.len(), 1);
        assert_eq!(
            outcome,
            ClientResizeOutcome {
                terminal_grid_changed: false,
                refreshed_terminal_viewport: false,
            }
        );
        assert!(!outcome.should_invalidate_client());
        Ok(())
    }

    #[test]
    fn splitter_drag_defers_same_grid_terminal_resize_until_drag_end() -> AppResult<()> {
        let current_size = TerminalSize::with_pixels(24, 80, 640, 384)?;
        let mut state = WindowState::new_for_test(current_size, None);
        state
            .session
            .execute(TerminalCommand::Resize(current_size))?;
        state.session.terminal_resize_commands.clear();
        state.refresh_terminal_viewport()?;
        state.runtime.splitter_drag = Some(SplitterDrag {
            pointer_offset_x: 0,
            deferred_terminal_size: None,
        });
        let deferred_size = TerminalSize::with_pixels(24, 80, 648, 384)?;
        assert!(!terminal_grid_changed(current_size, deferred_size));
        let resize = CommandPanelResize {
            previous: state.view.command_panel_resize_snapshot(),
            previous_terminal_size: current_size,
            terminal_size: deferred_size,
            terminal_grid_changed: false,
        };

        state.apply_command_panel_resize(std::ptr::null_mut(), resize)?;

        assert!(state.session.terminal_resize_commands.is_empty());
        assert_eq!(
            state
                .runtime
                .splitter_drag
                .as_ref()
                .and_then(|drag| drag.deferred_terminal_size.as_ref()),
            Some(&deferred_size)
        );

        state.handle_capture_changed()?;

        assert!(state.runtime.splitter_drag.is_none());
        assert_eq!(state.session.terminal_resize_commands, vec![deferred_size]);
        Ok(())
    }

    #[test]
    fn terminal_rows_rect_returns_dirty_row_band() {
        let content_area = UiRect {
            x: 12,
            y: 24,
            width: 320,
            height: 96,
        };
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };

        let Some(rect) = terminal_rows_rect(content_area, metrics, 1..3, 6) else {
            panic!("dirty row band should produce a rect");
        };

        assert_eq!(rect.x, 12);
        assert_eq!(rect.y, 40);
        assert_eq!(rect.width, 320);
        assert_eq!(rect.height, 32);
    }

    #[test]
    fn terminal_rows_rect_clips_to_terminal_content_height() {
        let content_area = UiRect {
            x: 12,
            y: 24,
            width: 320,
            height: 40,
        };
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };

        let Some(rect) = terminal_rows_rect(content_area, metrics, 1..4, 4) else {
            panic!("clipped dirty row band should produce a rect");
        };

        assert_eq!(rect.x, 12);
        assert_eq!(rect.y, 40);
        assert_eq!(rect.width, 320);
        assert_eq!(rect.height, 24);
    }

    #[test]
    fn terminal_rows_rect_extends_last_visible_row_to_content_bottom() {
        let content_area = UiRect {
            x: 12,
            y: 24,
            width: 320,
            height: 38,
        };
        let metrics = CellMetrics {
            width: 8,
            height: 16,
        };

        let Some(rect) = terminal_rows_rect(content_area, metrics, 1..2, 2) else {
            panic!("last dirty row should include trailing content");
        };

        assert_eq!(rect.x, 12);
        assert_eq!(rect.y, 40);
        assert_eq!(rect.width, 320);
        assert_eq!(rect.height, 22);
    }

    fn test_viewport(text: &str) -> AppResult<TerminalViewport> {
        let cells = text.chars().map(TerminalCell::new).collect::<Vec<_>>();
        TerminalViewport::new(1, cells.len(), cells, CursorPosition::new(0, 0))
    }

    fn unique_test_settings_path(name: &str) -> AppResult<PathBuf> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|source| AppError::ui_message("resolve test timestamp", source.to_string()))?
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "j3term-win32-{}-{}-{}",
            name,
            std::process::id(),
            timestamp
        ));
        fs::create_dir(&directory)
            .map_err(|source| AppError::io("create test settings directory", source))?;
        Ok(directory.join("settings.toml"))
    }

    fn single_category_command_panel() -> AppResult<CommandPanel> {
        CommandPanel::from_definitions(
            vec![CommandCategoryDefinition::new("Default", Vec::new())?],
            0,
        )
    }

    fn two_category_command_panel() -> AppResult<CommandPanel> {
        CommandPanel::from_definitions(
            vec![
                CommandCategoryDefinition::new("Default", Vec::new())?,
                CommandCategoryDefinition::new("Tools", Vec::new())?,
            ],
            0,
        )
    }

    fn single_button_command_panel(label: &str) -> AppResult<CommandPanel> {
        let definition =
            CommandButtonDefinition::new(label, "echo", CommandArguments::new("hello")?)?;
        CommandPanel::from_definitions(
            vec![CommandCategoryDefinition::new("Default", vec![definition])?],
            0,
        )
    }

    fn window_state_with_command_panel(
        initial_size: TerminalSize,
        command_panel: CommandPanel,
        config_store: Option<ConfigStore>,
    ) -> WindowState {
        let session = WindowSessionCoordinator::new(
            initial_size,
            StartupInvocation::default(),
            command_panel,
            TerminalFont::default(),
            config_store,
        );
        let tab_views = session.tab_views();
        let command_panel = session.command_panel_handle();
        let terminal_font = session.terminal_font().clone();
        WindowState {
            session,
            view: WindowViewAdapters::new(command_panel, tab_views, terminal_font),
            runtime: WindowRuntimeState::default(),
            last_error: WindowErrorState::default(),
            shutdown_error: new_shutdown_error_sink(),
        }
    }

    fn assert_command_panel_matches(actual: &CommandPanel, expected: &CommandPanel) {
        assert_eq!(actual.categories().len(), expected.categories().len());
        for (actual, expected) in actual.categories().iter().zip(expected.categories()) {
            assert_eq!(actual.name.as_str(), expected.name.as_str());
        }
        assert_eq!(
            actual.selected_category_index(),
            expected.selected_category_index()
        );
        let actual_buttons = actual.selected_buttons();
        let expected_buttons = expected.selected_buttons();
        assert_eq!(actual_buttons.len(), expected_buttons.len());
        for (actual, expected) in actual_buttons.iter().zip(expected_buttons) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.label.as_str(), expected.label.as_str());
        }
    }

    fn assert_command_panel_resize_state_matches(
        view: &WindowViewAdapters,
        command_panel_width: i32,
        command_button_scroll_position: usize,
        layout: &WindowLayout,
        terminal_viewport: &Option<TerminalViewport>,
    ) {
        assert_eq!(view.command_panel_width, command_panel_width);
        assert_eq!(
            view.command_button_scroll_position,
            command_button_scroll_position
        );
        assert_eq!(view.layout.command_panel.x, layout.command_panel.x);
        assert_eq!(view.layout.command_panel.y, layout.command_panel.y);
        assert_eq!(view.layout.command_panel.width, layout.command_panel.width);
        assert_eq!(
            view.layout.command_panel.height,
            layout.command_panel.height
        );
        assert_eq!(view.layout.terminal.x, layout.terminal.x);
        assert_eq!(view.layout.terminal.y, layout.terminal.y);
        assert_eq!(view.layout.terminal.width, layout.terminal.width);
        assert_eq!(view.layout.terminal.height, layout.terminal.height);
        assert_eq!(view.layout.splitter.x, layout.splitter.x);
        assert_eq!(view.layout.splitter.y, layout.splitter.y);
        assert_eq!(view.layout.splitter.width, layout.splitter.width);
        assert_eq!(view.layout.splitter.height, layout.splitter.height);
        assert_eq!(
            view.layout.command_button_scroll_position(),
            layout.command_button_scroll_position()
        );
        assert_eq!(&view.terminal_viewport, terminal_viewport);
    }

    fn cleanup_test_settings_path(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(path);
        if let Some(directory) = path.parent() {
            let _ = fs::remove_dir(directory);
        }
    }
}
