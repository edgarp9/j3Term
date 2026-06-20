use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;

use crate::app::{TerminalTabs, TerminalTimerDrain, startup_window_size_message};
use crate::domain::input::terminal_input_from_modified_char;
use crate::domain::layout::{
    COMMAND_PANEL_WIDTH, terminal_content_area, terminal_scrollbar_bounds,
};
use crate::domain::terminal::TerminalChangedRows;
use crate::domain::{
    APP_DISPLAY_NAME, APP_VERSION, AUTHOR_PROFILE_URL, ButtonArgumentValues, CommandArguments,
    CommandButton, CommandButtonDefinition, CommandButtonId, CommandPanel, CommandText,
    DEFAULT_COLUMNS, DEFAULT_ROWS, LINUX_APPLICATION_ID, MAX_FONT_SIZE_POINTS,
    MIN_FONT_SIZE_POINTS, StartupInvocation, TerminalCell, TerminalCommand, TerminalFont,
    TerminalGridPoint, TerminalInput, TerminalKey, TerminalKeyModifiers, TerminalScroll,
    TerminalScrollState, TerminalSelection, TerminalSize, TerminalTabId, TerminalTabView,
    TerminalViewport, UiPoint, UiRect, WindowLayout, terminal_input_from_key,
};
use crate::error::{AppError, AppResult};
use crate::infra::config::{AppSettings, ConfigStore};
use crate::infra::linux_desktop;
use crate::infra::pty::{
    PortablePtySession, is_detached_cleanup_timeout_error, join_detached_cleanup_tasks,
};
use crate::infra::terminal::AlacrittyTerminalBuffer;

const WINDOW_WIDTH: i32 = 750;
const WINDOW_HEIGHT: i32 = 520;
const PTY_ACTIVE_TIMER_MS: u64 = 33;
const PTY_IDLE_TIMER_MS: u64 = 250;
const PTY_IDLE_BACKOFF_AFTER_EMPTY_DRAINS: u8 = 3;
const COMMAND_PANEL_SAVE_DEBOUNCE_MS: u64 = 250;
const COMMAND_PANEL_SAVE_RESULT_POLL_MS: u64 = 16;
const COMMAND_PANEL_SAVE_SHUTDOWN_WAIT_MS: u64 = 500;
const CTRL_V_CHAR: char = '\u{16}';

const CHROME_BACKGROUND: Color = Color::rgb(34, 38, 46);
const BACKGROUND: Color = Color::rgb(12, 14, 18);
const FOREGROUND: Color = Color::rgb(220, 226, 235);
const TAB_ACTIVE_BACKGROUND: Color = Color::rgb(12, 14, 18);
const TAB_INACTIVE_BACKGROUND: Color = Color::rgb(55, 61, 72);
const TAB_MUTED_FOREGROUND: Color = Color::rgb(170, 180, 194);
const SPLITTER_BACKGROUND: Color = Color::rgb(42, 47, 56);
const SPLITTER_GRIP: Color = Color::rgb(90, 100, 116);
const SELECTION_BACKGROUND: Color = Color::rgb(52, 100, 156);
const COMMAND_BUTTON_CSS: &str = ".j3term-command-button { padding: 0 4px; }";

type GtkTabs = TerminalTabs<PortablePtySession, AlacrittyTerminalBuffer>;
type SharedState = Rc<RefCell<GtkWindowState>>;

#[derive(Debug, Clone, Copy)]
struct Color {
    red: f64,
    green: f64,
    blue: f64,
}

impl Color {
    const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self {
            red: red as f64 / 255.0,
            green: green as f64 / 255.0,
            blue: blue as f64 / 255.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CellMetrics {
    width: i32,
    height: i32,
    baseline: f64,
}

impl Default for CellMetrics {
    fn default() -> Self {
        Self {
            width: 8,
            height: 16,
            baseline: 12.0,
        }
    }
}

pub fn run(startup: StartupInvocation) -> AppResult<()> {
    let app = gtk::Application::builder()
        .application_id(LINUX_APPLICATION_ID)
        .build();
    let startup = Rc::new(RefCell::new(Some(startup)));
    let run_error = Rc::new(RefCell::new(None));

    {
        let startup = Rc::clone(&startup);
        let run_error = Rc::clone(&run_error);
        app.connect_activate(move |app| {
            let Some(startup) = startup.borrow_mut().take() else {
                return;
            };

            if let Err(error) = build_window(app, startup) {
                *run_error.borrow_mut() = Some(error);
                app.quit();
            }
        });
    }

    let _exit_code = app.run();

    if let Some(error) = run_error.borrow_mut().take() {
        return Err(error);
    }

    let cleanup_errors = cleanup_detached_pty_tasks_after_gtk_exit();
    if cleanup_errors.is_empty() {
        Ok(())
    } else {
        let message = cleanup_errors
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        Err(AppError::pty_message("shutdown application", message))
    }
}

fn cleanup_detached_pty_tasks_after_gtk_exit() -> Vec<AppError> {
    collect_detached_cleanup_errors_after_gtk_exit(
        join_detached_cleanup_tasks(),
        report_detached_cleanup_timeout,
    )
}

fn collect_detached_cleanup_errors_after_gtk_exit(
    detached_cleanup_errors: Vec<AppError>,
    mut report_timeout: impl FnMut(&AppError),
) -> Vec<AppError> {
    let mut cleanup_errors = Vec::new();

    for error in detached_cleanup_errors {
        if is_detached_cleanup_timeout_error(&error) {
            report_timeout(&error);
        } else {
            cleanup_errors.push(error);
        }
    }

    cleanup_errors
}

fn report_detached_cleanup_timeout(error: &AppError) {
    eprintln!("{}", error.user_message());
    eprintln!("cause: {error}");
}

fn build_window(app: &gtk::Application, startup: StartupInvocation) -> AppResult<()> {
    let initial_size = TerminalSize::new(DEFAULT_ROWS, DEFAULT_COLUMNS)?;
    let session = GtkSessionCoordinator::load(initial_size, startup)?;
    install_css_provider()?;
    gtk::Window::set_default_icon_name(LINUX_APPLICATION_ID);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(APP_DISPLAY_NAME)
        .icon_name(LINUX_APPLICATION_ID)
        .default_width(WINDOW_WIDTH)
        .default_height(WINDOW_HEIGHT)
        .build();
    window.set_icon_name(Some(LINUX_APPLICATION_ID));
    install_runtime_window_icon(&window);

    let overlay = gtk::Overlay::new();
    let drawing_area = gtk::DrawingArea::new();
    drawing_area.set_hexpand(true);
    drawing_area.set_vexpand(true);
    drawing_area.set_focusable(true);

    overlay.set_child(Some(&drawing_area));
    window.set_child(Some(&overlay));

    let category_combo = gtk::ComboBoxText::new();
    configure_category_combo(&category_combo);
    let command_button_scrollbar =
        gtk::Scrollbar::new(gtk::Orientation::Vertical, None::<&gtk::Adjustment>);
    let terminal_scrollbar =
        gtk::Scrollbar::new(gtk::Orientation::Vertical, None::<&gtk::Adjustment>);

    add_overlay_widget(&overlay, &category_combo);
    add_overlay_widget(&overlay, &command_button_scrollbar);
    add_overlay_widget(&overlay, &terminal_scrollbar);

    let state = Rc::new(RefCell::new(GtkWindowState::new(
        window.clone(),
        overlay,
        drawing_area.clone(),
        category_combo,
        command_button_scrollbar,
        terminal_scrollbar,
        session,
    )));

    install_draw_handler(&state);
    install_keyboard_handler(&state);
    install_mouse_handlers(&state);
    install_combo_handler(&state);
    install_scrollbar_handlers(&state);
    install_close_handler(&state);

    {
        let mut state_ref = state.borrow_mut();
        state_ref.session.start()?;
        state_ref.resize_to_client(WINDOW_WIDTH, WINDOW_HEIGHT)?;
        state_ref.display_startup_window_size()?;
        state_ref.session.run_startup_command()?;
        state_ref.refresh_terminal_viewport()?;
    }
    sync_all_widgets(&state)?;

    schedule_pty_drain_timer(&state, active_pty_timer_interval());

    window.present();
    state.borrow().drawing_area.grab_focus();
    Ok(())
}

fn install_css_provider() -> AppResult<()> {
    let display =
        gdk::Display::default().ok_or(AppError::InvalidState("GTK display is not available"))?;
    let provider = gtk::CssProvider::new();
    provider.load_from_data(COMMAND_BUTTON_CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    Ok(())
}

fn install_runtime_window_icon(window: &gtk::ApplicationWindow) {
    let Ok(Some(icon_path)) = linux_desktop::find_runtime_icon_path() else {
        return;
    };

    window.connect_realize(move |window| {
        apply_runtime_window_icon(window, &icon_path);
    });
}

fn apply_runtime_window_icon(window: &gtk::ApplicationWindow, icon_path: &Path) {
    let file = gio::File::for_path(icon_path);
    let Ok(texture) = gdk::Texture::from_file(&file) else {
        return;
    };
    let Some(surface) = window.surface() else {
        return;
    };
    let Ok(toplevel) = surface.downcast::<gdk::Toplevel>() else {
        return;
    };

    toplevel.set_icon_list(&[texture]);
}

struct GtkSessionCoordinator {
    tabs: GtkTabs,
    terminal_size: TerminalSize,
    command_panel: CommandPanel,
    terminal_font: TerminalFont,
    config_store: ConfigStore,
    startup: StartupInvocation,
    command_panel_dirty: bool,
}

impl GtkSessionCoordinator {
    fn load(initial_size: TerminalSize, startup: StartupInvocation) -> AppResult<Self> {
        let config_store = ConfigStore::from_current_exe()?;
        let settings = config_store.load_settings_or_default()?;
        Ok(Self {
            tabs: TerminalTabs::new(
                initial_size,
                PortablePtySession::new,
                AlacrittyTerminalBuffer::new,
            ),
            terminal_size: initial_size,
            command_panel: settings.command_panel,
            terminal_font: settings.terminal_font,
            config_store,
            startup,
            command_panel_dirty: false,
        })
    }

    #[cfg(test)]
    fn new_for_test(
        initial_size: TerminalSize,
        command_panel: CommandPanel,
        config_store: ConfigStore,
    ) -> Self {
        Self {
            tabs: TerminalTabs::new(
                initial_size,
                PortablePtySession::new,
                AlacrittyTerminalBuffer::new,
            ),
            terminal_size: initial_size,
            command_panel,
            terminal_font: TerminalFont::default(),
            config_store,
            startup: StartupInvocation::default(),
            command_panel_dirty: false,
        }
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

    fn mark_command_panel_dirty(&mut self) {
        self.command_panel_dirty = true;
    }

    fn mark_command_panel_saved(&mut self) {
        self.command_panel_dirty = false;
    }

    fn save_command_panel(&mut self) -> AppResult<()> {
        self.config_store.save_settings(&AppSettings {
            command_panel: self.command_panel.clone(),
            terminal_font: self.terminal_font.clone(),
        })?;
        self.mark_command_panel_saved();
        Ok(())
    }

    fn terminal_font(&self) -> &TerminalFont {
        &self.terminal_font
    }

    fn set_terminal_font(&mut self, terminal_font: TerminalFont) -> AppResult<()> {
        let previous = std::mem::replace(&mut self.terminal_font, terminal_font);
        if let Err(error) = self.config_store.save_settings(&AppSettings {
            command_panel: self.command_panel.clone(),
            terminal_font: self.terminal_font.clone(),
        }) {
            self.terminal_font = previous;
            return Err(error);
        }

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
            .command_panel
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

    fn run_button_command(
        &mut self,
        button: CommandButton,
        values: ButtonArgumentValues,
    ) -> AppResult<()> {
        let dialect = self.active_shell_command_dialect()?;
        let command_text = button.to_command_text(&values, dialect)?;
        self.run_command_text(&command_text)
    }

    fn select_command_category_by_index(&mut self, index: usize) -> AppResult<()> {
        let previous = self.command_panel.selected_category_index();
        self.command_panel.select_category_by_index(index)?;
        if self.command_panel.selected_category_index() != previous {
            self.mark_command_panel_dirty();
        }
        Ok(())
    }

    fn drain_timer_events(&mut self) -> AppResult<TerminalTimerDrain> {
        self.tabs.drain_timer_events()
    }

    fn terminal_viewport(&mut self) -> AppResult<TerminalViewport> {
        self.tabs.terminal_viewport()
    }

    fn refresh_terminal_viewport(&mut self, viewport: &mut TerminalViewport) -> AppResult<()> {
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

    fn terminal_size(&self) -> TerminalSize {
        self.terminal_size
    }

    fn shutdown(&mut self) -> AppResult<()> {
        if self.command_panel_dirty {
            self.save_command_panel()?;
        }
        self.shutdown_terminal_session()
    }

    fn shutdown_terminal_session(&mut self) -> AppResult<()> {
        self.tabs.execute(TerminalCommand::Shutdown)
    }
}

struct GtkWindowState {
    window: gtk::ApplicationWindow,
    overlay: gtk::Overlay,
    drawing_area: gtk::DrawingArea,
    category_combo: gtk::ComboBoxText,
    command_button_scrollbar: gtk::Scrollbar,
    terminal_scrollbar: gtk::Scrollbar,
    command_buttons: Vec<CommandButtonWidget>,
    session: GtkSessionCoordinator,
    tab_views: Vec<TerminalTabView>,
    terminal_viewport: Option<TerminalViewport>,
    terminal_line_text_cache: TerminalLineTextCache,
    terminal_surface_cache: TerminalSurfaceCache,
    pending_terminal_paint: TerminalViewportInvalidation,
    terminal_selection: Option<TerminalSelection>,
    command_panel_width: i32,
    command_button_scroll_position: usize,
    layout: WindowLayout,
    metrics: CellMetrics,
    runtime: GtkRuntimeState,
    syncing_category_combo: bool,
    syncing_command_scrollbar: bool,
    syncing_terminal_scrollbar: bool,
    last_error: Option<String>,
}

#[derive(Default)]
struct GtkRuntimeState {
    splitter_drag: Option<SplitterDrag>,
    terminal_selection_drag: Option<TerminalSelectionDrag>,
    pty_drain_timer: PtyDrainTimerState,
    command_panel_save: CommandPanelSaveState,
}

#[derive(Debug, Default)]
struct PtyDrainTimerState {
    generation: u64,
    empty_drain_count: u8,
}

impl PtyDrainTimerState {
    fn schedule_next(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    fn record_activity(&mut self) {
        self.empty_drain_count = 0;
    }

    fn interval_after_drain(&mut self, drain: Option<&TerminalTimerDrain>) -> Duration {
        match drain {
            Some(drain) if drain.had_events || drain.needs_active_poll => {
                self.empty_drain_count = 0;
                active_pty_timer_interval()
            }
            Some(_) => {
                self.empty_drain_count = self.empty_drain_count.saturating_add(1);
                if self.empty_drain_count >= PTY_IDLE_BACKOFF_AFTER_EMPTY_DRAINS {
                    Duration::from_millis(PTY_IDLE_TIMER_MS)
                } else {
                    active_pty_timer_interval()
                }
            }
            None => {
                self.empty_drain_count = 0;
                active_pty_timer_interval()
            }
        }
    }
}

#[derive(Default)]
struct CommandPanelSaveState {
    generation: u64,
    pending: bool,
    in_progress: Option<CommandPanelSaveTask>,
}

struct CommandPanelSaveTask {
    generation: u64,
    receiver: Receiver<AppResult<()>>,
}

enum CommandPanelSavePoll {
    Pending,
    Finished(AppResult<()>),
    Missing,
}

enum CommandPanelShutdownSave {
    Complete,
    Delayed,
}

enum CommandPanelSaveShutdownWait {
    Finished {
        generation: u64,
        result: AppResult<()>,
    },
    Delayed(CommandPanelSaveTask),
}

impl CommandPanelSaveTask {
    fn wait_before_shutdown(self, timeout: Duration) -> AppResult<CommandPanelSaveShutdownWait> {
        let wait = self.receiver.recv_timeout(timeout);
        match wait {
            Ok(result) => Ok(CommandPanelSaveShutdownWait::Finished {
                generation: self.generation,
                result,
            }),
            Err(RecvTimeoutError::Timeout) => Ok(CommandPanelSaveShutdownWait::Delayed(self)),
            Err(RecvTimeoutError::Disconnected) => Err(AppError::InvalidState(
                "command panel save worker stopped before shutdown",
            )),
        }
    }
}

impl CommandPanelSaveState {
    fn schedule_next(&mut self) -> u64 {
        self.pending = true;
        self.generation = self.generation.saturating_add(1);
        self.generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.pending && self.generation == generation
    }

    fn mark_saved(&mut self, generation: u64) {
        if self.generation == generation {
            self.pending = false;
        }
    }

    fn mark_save_started(&mut self, generation: u64, receiver: Receiver<AppResult<()>>) -> bool {
        if !self.is_current(generation) || self.in_progress.is_some() {
            return false;
        }

        self.pending = false;
        self.in_progress = Some(CommandPanelSaveTask {
            generation,
            receiver,
        });
        true
    }

    fn clear_save_in_progress(&mut self, generation: u64) {
        if self
            .in_progress
            .as_ref()
            .is_some_and(|task| task.generation == generation)
        {
            self.in_progress = None;
        }
    }

    fn poll_save_result(&mut self, generation: u64) -> CommandPanelSavePoll {
        let Some(task) = self.in_progress.as_ref() else {
            return CommandPanelSavePoll::Missing;
        };
        if task.generation != generation {
            return CommandPanelSavePoll::Missing;
        }

        let poll = task.receiver.try_recv();
        match poll {
            Ok(result) => {
                self.in_progress = None;
                self.mark_saved(generation);
                CommandPanelSavePoll::Finished(result)
            }
            Err(TryRecvError::Empty) => CommandPanelSavePoll::Pending,
            Err(TryRecvError::Disconnected) => {
                self.in_progress = None;
                self.mark_saved(generation);
                CommandPanelSavePoll::Finished(Err(AppError::InvalidState(
                    "command panel save worker stopped",
                )))
            }
        }
    }

    fn take_save_in_progress(&mut self) -> Option<CommandPanelSaveTask> {
        self.in_progress.take()
    }

    fn restore_save_in_progress(&mut self, task: CommandPanelSaveTask) {
        if self.in_progress.is_none() {
            self.in_progress = Some(task);
        }
    }

    fn has_pending_save(&self) -> bool {
        self.pending
    }

    fn save_in_progress(&self) -> bool {
        self.in_progress.is_some()
    }
}

#[derive(Default)]
struct TerminalLineTextCache {
    rows: Vec<CachedTerminalLineText>,
    viewport_rows: usize,
    viewport_columns: usize,
}

#[derive(Default)]
struct CachedTerminalLineText {
    version: Option<u64>,
    text: String,
}

impl TerminalLineTextCache {
    fn clear(&mut self) {
        self.viewport_rows = 0;
        self.viewport_columns = 0;
        for row in &mut self.rows {
            row.version = None;
            row.text.clear();
        }
    }

    fn line_text<'a>(&'a mut self, viewport: &TerminalViewport, row: usize) -> Option<&'a str> {
        if row >= viewport.rows {
            return None;
        }

        self.sync_viewport_shape(viewport.rows, viewport.columns);
        let row_cache = self.rows.get_mut(row)?;
        let version = viewport.row_version(row)?;
        if row_cache.version != Some(version) {
            let cells = viewport.line_cells(row)?;
            row_cache.version = Some(version);
            let _ = write_line_text(&mut row_cache.text, cells);
        }

        if row_cache.text.is_empty() {
            None
        } else {
            Some(row_cache.text.as_str())
        }
    }

    fn sync_viewport_shape(&mut self, rows: usize, columns: usize) {
        if self.viewport_rows == rows && self.viewport_columns == columns {
            return;
        }

        self.viewport_rows = rows;
        self.viewport_columns = columns;
        self.rows.resize_with(rows, CachedTerminalLineText::default);
        for row in &mut self.rows {
            row.version = None;
            row.text.clear();
        }
    }
}

#[derive(Default)]
struct TerminalSurfaceCache {
    surface: Option<gtk::cairo::ImageSurface>,
    viewport_rows: usize,
    viewport_columns: usize,
    content_width: i32,
    content_height: i32,
    metrics: CellMetrics,
}

#[derive(Clone, Copy)]
struct TerminalFontRenderState<'a> {
    metrics: CellMetrics,
    font: &'a TerminalFont,
}

impl TerminalSurfaceCache {
    fn clear(&mut self) {
        self.surface = None;
        self.viewport_rows = 0;
        self.viewport_columns = 0;
        self.content_width = 0;
        self.content_height = 0;
        self.metrics = CellMetrics::default();
    }

    fn paint(
        &mut self,
        context: &gtk::cairo::Context,
        viewport: &TerminalViewport,
        content: UiRect,
        font: TerminalFontRenderState<'_>,
        line_text_cache: &mut TerminalLineTextCache,
        invalidation: TerminalViewportInvalidation,
    ) -> bool {
        let content_width = content.width.max(1);
        let content_height = content.height.max(1);
        let shape_changed = self.viewport_rows != viewport.rows
            || self.viewport_columns != viewport.columns
            || self.content_width != content_width
            || self.content_height != content_height
            || self.metrics != font.metrics;

        if shape_changed {
            self.surface = None;
        }

        if self.surface.is_none() {
            let Ok(surface) = gtk::cairo::ImageSurface::create(
                gtk::cairo::Format::ARgb32,
                content_width,
                content_height,
            ) else {
                return false;
            };
            self.surface = Some(surface);
            self.viewport_rows = viewport.rows;
            self.viewport_columns = viewport.columns;
            self.content_width = content_width;
            self.content_height = content_height;
            self.metrics = font.metrics;
        }

        let redraw_all =
            shape_changed || matches!(invalidation, TerminalViewportInvalidation::Full);
        if redraw_all {
            self.redraw_row_ranges(
                viewport,
                font,
                line_text_cache,
                std::iter::once(0..viewport.rows),
            );
        } else if let TerminalViewportInvalidation::Rows(rows) = invalidation {
            self.redraw_row_ranges(
                viewport,
                font,
                line_text_cache,
                rows.ranges().iter().cloned(),
            );
        }

        let Some(surface) = self.surface.as_ref() else {
            return false;
        };

        if context
            .set_source_surface(surface, content.x as f64, content.y as f64)
            .is_err()
        {
            return false;
        }
        context.rectangle(
            content.x as f64,
            content.y as f64,
            content_width as f64,
            content_height as f64,
        );
        let _ = context.fill();
        true
    }

    fn redraw_row_ranges(
        &mut self,
        viewport: &TerminalViewport,
        font: TerminalFontRenderState<'_>,
        line_text_cache: &mut TerminalLineTextCache,
        row_ranges: impl IntoIterator<Item = std::ops::Range<usize>>,
    ) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        let Ok(row_context) = gtk::cairo::Context::new(surface) else {
            return;
        };

        select_terminal_font(&row_context, font.font);
        let content = UiRect {
            x: 0,
            y: 0,
            width: self.content_width,
            height: self.content_height,
        };

        for row_range in row_ranges {
            let Some(rect) = terminal_rows_rect(content, font.metrics, row_range.clone()) else {
                continue;
            };
            set_color(&row_context, BACKGROUND);
            fill_rect(&row_context, rect);

            for row in row_range {
                if row >= viewport.rows {
                    break;
                }
                if let Some(line) = line_text_cache.line_text(viewport, row) {
                    set_color(&row_context, FOREGROUND);
                    draw_text(
                        &row_context,
                        0,
                        i32_from_usize_saturating(row)
                            .saturating_mul(font.metrics.height)
                            .saturating_add(font.metrics.baseline as i32),
                        line,
                    );
                }
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
enum TerminalViewportInvalidation {
    #[default]
    None,
    Rows(TerminalChangedRows),
    Full,
}

impl TerminalViewportInvalidation {
    fn merge(&mut self, invalidation: Self) {
        match (&mut *self, invalidation) {
            (Self::Full, _) | (_, Self::None) => {}
            (state, Self::Full) => *state = Self::Full,
            (Self::None, rows @ Self::Rows(_)) => *self = rows,
            (Self::Rows(existing), Self::Rows(rows)) => existing.merge(&rows),
        }
    }
}

#[derive(Clone, Copy)]
struct SplitterDrag {
    pointer_offset_x: i32,
    pending_splitter_x: Option<i32>,
    resize_scheduled: bool,
    deferred_terminal_size: Option<TerminalSize>,
}

#[derive(Clone, Copy)]
struct TerminalSelectionDrag {
    anchor: TerminalGridPoint,
}

struct CommandButtonWidget {
    id: CommandButtonId,
    label: String,
    button: gtk::Button,
    label_widget: gtk::Label,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SplitterResizeWidgetSync {
    Unchanged,
    GeometryOnly,
    RebuildCommandWidgets,
}

impl GtkWindowState {
    fn new(
        window: gtk::ApplicationWindow,
        overlay: gtk::Overlay,
        drawing_area: gtk::DrawingArea,
        category_combo: gtk::ComboBoxText,
        command_button_scrollbar: gtk::Scrollbar,
        terminal_scrollbar: gtk::Scrollbar,
        session: GtkSessionCoordinator,
    ) -> Self {
        let tab_views = session.tab_views();
        let layout = WindowLayout::for_client_with_command_panel_width(
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            COMMAND_PANEL_WIDTH,
            session.command_panel.selected_buttons(),
            &tab_views,
        );

        Self {
            window,
            overlay,
            drawing_area,
            category_combo,
            command_button_scrollbar,
            terminal_scrollbar,
            command_buttons: Vec::new(),
            session,
            tab_views,
            terminal_viewport: None,
            terminal_line_text_cache: TerminalLineTextCache::default(),
            terminal_surface_cache: TerminalSurfaceCache::default(),
            pending_terminal_paint: TerminalViewportInvalidation::Full,
            terminal_selection: None,
            command_panel_width: COMMAND_PANEL_WIDTH,
            command_button_scroll_position: 0,
            layout,
            metrics: CellMetrics::default(),
            runtime: GtkRuntimeState::default(),
            syncing_category_combo: false,
            syncing_command_scrollbar: false,
            syncing_terminal_scrollbar: false,
            last_error: None,
        }
    }

    fn resize_to_client(&mut self, width: i32, height: i32) -> AppResult<()> {
        self.layout = layout_from_client_size(
            width,
            height,
            self.command_panel_width,
            self.session.command_panel.selected_buttons(),
            &self.tab_views,
            self.command_button_scroll_position,
        );
        self.command_button_scroll_position = self.layout.command_button_scroll_position();
        let next_size = terminal_size_from_area(self.layout.terminal, self.metrics)?;
        if self.session.terminal_size() != next_size {
            self.session.execute(TerminalCommand::Resize(next_size))?;
            self.clear_terminal_selection();
            self.replace_terminal_viewport()?;
        }
        Ok(())
    }

    fn display_startup_window_size(&mut self) -> AppResult<()> {
        let (width, height) = self.client_size();
        let message = startup_window_size_message(width, height);
        self.session.display_status_message(&message)
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

    fn update_cell_metrics(&mut self, metrics: CellMetrics) -> bool {
        if self.metrics == metrics {
            return false;
        }

        self.metrics = metrics;
        self.terminal_surface_cache.clear();
        self.pending_terminal_paint = TerminalViewportInvalidation::Full;
        true
    }

    fn refresh_terminal_viewport(&mut self) -> AppResult<()> {
        if let Some(viewport) = self.terminal_viewport.as_mut() {
            self.session.refresh_terminal_viewport(viewport)?;
        } else {
            self.terminal_viewport = Some(self.session.terminal_viewport()?);
        }
        Ok(())
    }

    fn refresh_terminal_viewport_invalidation(
        &mut self,
    ) -> AppResult<TerminalViewportInvalidation> {
        let previous = self
            .terminal_viewport
            .as_ref()
            .map(|viewport| viewport.change_baseline());
        self.refresh_terminal_viewport()?;

        let invalidation = match (previous.as_ref(), self.terminal_viewport.as_ref()) {
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

    fn replace_terminal_viewport(&mut self) -> AppResult<()> {
        self.terminal_viewport = Some(self.session.terminal_viewport()?);
        self.terminal_line_text_cache.clear();
        self.terminal_surface_cache.clear();
        self.pending_terminal_paint = TerminalViewportInvalidation::Full;
        Ok(())
    }

    fn invalidate_terminal_paint(&mut self, invalidation: TerminalViewportInvalidation) {
        self.pending_terminal_paint.merge(invalidation);
    }

    fn set_tab_views(&mut self) {
        self.tab_views = self.session.tab_views();
        self.layout = layout_from_client_size(
            self.layout.tab_bar.width,
            self.layout
                .tab_bar
                .height
                .saturating_add(self.layout.terminal.height),
            self.command_panel_width,
            self.session.command_panel.selected_buttons(),
            &self.tab_views,
            self.command_button_scroll_position,
        );
        self.command_button_scroll_position = self.layout.command_button_scroll_position();
    }

    fn refresh_command_panel_layout(&mut self, requested_scroll_position: usize) {
        relayout_command_panel(
            &mut self.layout,
            &mut self.command_button_scroll_position,
            self.command_panel_width,
            &self.session.command_panel,
            &self.tab_views,
            requested_scroll_position,
        );
    }

    fn refresh_command_panel_layout_preserving_scroll(&mut self) {
        self.refresh_command_panel_layout(self.command_button_scroll_position);
    }

    fn apply_command_button_scroll_lines(&mut self, line_delta: i32) -> bool {
        let Some(scroll) = self.layout.command_button_scroll else {
            self.command_button_scroll_position = 0;
            return false;
        };
        let next = scrolled_position_by_lines(scroll.position, scroll.max_position, line_delta);
        if next == scroll.position {
            return false;
        }

        self.refresh_command_panel_layout(next);
        true
    }

    fn apply_terminal_scroll(&mut self, scroll: TerminalScroll) -> AppResult<bool> {
        if self.session.scroll_terminal_display(scroll)? {
            self.replace_terminal_viewport()?;
            self.clear_terminal_selection();
            return Ok(true);
        }

        Ok(false)
    }

    fn selected_terminal_text(&self) -> Option<String> {
        let viewport = self.terminal_viewport.as_ref()?;
        let selection = self.terminal_selection?;
        Some(viewport.selected_text(selection))
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

    fn terminal_grid_point_at(
        &self,
        point: UiPoint,
        clamp_to_grid: bool,
    ) -> Option<TerminalGridPoint> {
        let viewport = self.terminal_viewport.as_ref()?;
        if viewport.rows == 0 || viewport.columns == 0 {
            return None;
        }

        let content = terminal_content_area(self.layout.terminal);
        if content.width <= 0 || content.height <= 0 {
            return None;
        }

        let grid_width = i32_from_usize_saturating(viewport.columns)
            .saturating_mul(self.metrics.width.max(1))
            .min(content.width);
        let grid_height = i32_from_usize_saturating(viewport.rows)
            .saturating_mul(self.metrics.height.max(1))
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
        let column = usize_from_i32_saturating(relative_x / self.metrics.width.max(1))
            .min(viewport.columns.saturating_sub(1));
        let row = usize_from_i32_saturating(relative_y / self.metrics.height.max(1))
            .min(viewport.rows.saturating_sub(1));

        Some(TerminalGridPoint::new(row, column))
    }

    fn record_error(&mut self, error: AppError) {
        let user_message = error.user_message().to_owned();
        let cause = error.to_string();
        if let Err(display_error) = self.session.display_recoverable_error(&user_message) {
            self.last_error = Some(format!(
                "{cause}; additionally failed to display user-facing error: {display_error}"
            ));
            eprintln!("{}", self.last_error.as_deref().unwrap_or(&cause));
            return;
        }

        self.last_error = Some(cause);
        if let Err(error) = self.refresh_terminal_viewport() {
            self.last_error = Some(format!(
                "{}; additionally failed to refresh terminal viewport: {error}",
                self.last_error.as_deref().unwrap_or("unknown error")
            ));
        }
        if let Some(error) = self.last_error.as_deref() {
            eprintln!("{error}");
        }
    }

    fn terminal_scroll_state(&self) -> TerminalScrollState {
        self.terminal_viewport
            .as_ref()
            .map_or(TerminalScrollState::default(), |viewport| viewport.scroll)
    }
}

trait GtkSessionExecute {
    fn execute(&mut self, command: TerminalCommand) -> AppResult<()>;
}

impl GtkSessionExecute for GtkSessionCoordinator {
    fn execute(&mut self, command: TerminalCommand) -> AppResult<()> {
        let resized = match &command {
            TerminalCommand::Resize(size) => Some(*size),
            TerminalCommand::Shutdown => None,
        };

        self.tabs.execute(command)?;
        if let Some(size) = resized {
            self.terminal_size = size;
        }
        Ok(())
    }
}

fn install_draw_handler(state: &SharedState) {
    let state_for_draw = Rc::clone(state);
    state
        .borrow()
        .drawing_area
        .set_draw_func(move |_, context, width, height| {
            let terminal_font = state_for_draw.borrow().session.terminal_font().clone();
            let metrics = measure_cell_metrics(context, &terminal_font);
            let mut state = state_for_draw.borrow_mut();
            let metrics_changed = metrics.is_some_and(|metrics| state.update_cell_metrics(metrics));
            let size_changed = state.layout.tab_bar.width != width
                || state.layout.tab_bar.height + state.layout.terminal.height != height;
            if (metrics_changed || size_changed)
                && let Err(error) = state.resize_to_client(width, height)
            {
                state.record_error(error);
            }
            draw_scene(&mut state, context, width, height);
        });

    let state_for_resize = Rc::clone(state);
    state
        .borrow()
        .drawing_area
        .connect_resize(move |_, width, height| {
            let result = {
                let mut state = state_for_resize.borrow_mut();
                state.resize_to_client(width, height)
            };
            if let Err(error) = result {
                state_for_resize.borrow_mut().record_error(error);
            }
            if let Err(error) = sync_all_widgets(&state_for_resize) {
                state_for_resize.borrow_mut().record_error(error);
            }
            state_for_resize.borrow().drawing_area.queue_draw();
        });
}

fn install_keyboard_handler(state: &SharedState) {
    let controller = gtk::EventControllerKey::new();
    let state_for_key = Rc::clone(state);
    controller.connect_key_pressed(move |_, key, _code, modifiers| {
        if handle_key_pressed(&state_for_key, key, modifiers) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    state.borrow().window.add_controller(controller);
}

fn handle_key_pressed(state: &SharedState, key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
    let key_modifiers = terminal_key_modifiers(modifiers);
    if key_modifiers.ctrl && !key_modifiers.alt {
        if (key == gdk::Key::c || key == gdk::Key::C) && copy_selection_to_clipboard(state) {
            return true;
        }
        if key == gdk::Key::v || key == gdk::Key::V {
            paste_clipboard_into_terminal(state);
            return true;
        }
    }

    let input = match key {
        gdk::Key::Return | gdk::Key::KP_Enter => {
            Some(terminal_input_from_key(TerminalKey::Enter, key_modifiers))
        }
        gdk::Key::BackSpace => Some(terminal_input_from_key(
            TerminalKey::Backspace,
            key_modifiers,
        )),
        gdk::Key::Tab | gdk::Key::ISO_Left_Tab => {
            Some(terminal_input_from_key(TerminalKey::Tab, key_modifiers))
        }
        gdk::Key::Escape => Some(terminal_input_from_key(TerminalKey::Escape, key_modifiers)),
        gdk::Key::Up => Some(terminal_input_from_key(TerminalKey::ArrowUp, key_modifiers)),
        gdk::Key::Down => Some(terminal_input_from_key(
            TerminalKey::ArrowDown,
            key_modifiers,
        )),
        gdk::Key::Right => Some(terminal_input_from_key(
            TerminalKey::ArrowRight,
            key_modifiers,
        )),
        gdk::Key::Left => Some(terminal_input_from_key(
            TerminalKey::ArrowLeft,
            key_modifiers,
        )),
        _ => key
            .to_unicode()
            .filter(|character| *character != CTRL_V_CHAR)
            .map(|character| terminal_input_from_modified_char(character, key_modifiers)),
    };

    let Some(input) = input else {
        return false;
    };
    let result = state.borrow_mut().session.handle_input(input);
    match result {
        Ok(()) => wake_pty_drain_timer(state),
        Err(error) => state.borrow_mut().record_error(error),
    }
    true
}

fn terminal_key_modifiers(modifiers: gdk::ModifierType) -> TerminalKeyModifiers {
    TerminalKeyModifiers::new(
        modifiers.contains(gdk::ModifierType::SHIFT_MASK),
        modifiers.contains(gdk::ModifierType::ALT_MASK),
        modifiers.contains(gdk::ModifierType::CONTROL_MASK),
    )
}

fn copy_selection_to_clipboard(state: &SharedState) -> bool {
    let Some(text) = state.borrow().selected_terminal_text() else {
        return false;
    };
    let Some(display) = gdk::Display::default() else {
        state
            .borrow_mut()
            .record_error(AppError::InvalidState("GTK display is not available"));
        return true;
    };
    display.clipboard().set_text(&text);
    true
}

fn paste_clipboard_into_terminal(state: &SharedState) {
    let Some(display) = gdk::Display::default() else {
        state
            .borrow_mut()
            .record_error(AppError::InvalidState("GTK display is not available"));
        return;
    };
    let clipboard = display.clipboard();
    let state = Rc::clone(state);
    glib::MainContext::default().spawn_local(async move {
        match clipboard.read_text_future().await {
            Ok(Some(text)) if !text.is_empty() => {
                let result = {
                    let mut state_ref = state.borrow_mut();
                    state_ref.clear_terminal_selection();
                    state_ref.session.paste_text(text.as_str())
                };
                match result {
                    Ok(()) => wake_pty_drain_timer(&state),
                    Err(error) => state.borrow_mut().record_error(error),
                }
                state.borrow().drawing_area.queue_draw();
            }
            Ok(_) => {}
            Err(error) => state.borrow_mut().record_error(AppError::ui_message(
                "read GTK clipboard",
                error.to_string(),
            )),
        }
    });
}

fn install_mouse_handlers(state: &SharedState) {
    let click = gtk::GestureClick::new();
    click.set_button(1);
    let state_for_press = Rc::clone(state);
    click.connect_pressed(move |_, _n_press, x, y| {
        handle_left_button_down(&state_for_press, ui_point(x, y));
    });
    let state_for_release = Rc::clone(state);
    click.connect_released(move |_, _n_press, _x, _y| {
        handle_left_button_up(&state_for_release);
    });
    state.borrow().drawing_area.add_controller(click);

    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    let state_for_drag = Rc::clone(state);
    drag.connect_drag_update(move |gesture, offset_x, offset_y| {
        if let Some((start_x, start_y)) = gesture.start_point() {
            handle_mouse_motion(
                &state_for_drag,
                ui_point(start_x + offset_x, start_y + offset_y),
            );
        }
    });
    let state_for_drag_end = Rc::clone(state);
    drag.connect_drag_end(move |_, _offset_x, _offset_y| {
        handle_left_button_up(&state_for_drag_end);
    });
    state.borrow().drawing_area.add_controller(drag);

    let motion = gtk::EventControllerMotion::new();
    let state_for_motion = Rc::clone(state);
    motion.connect_motion(move |_, x, y| {
        handle_mouse_motion(&state_for_motion, ui_point(x, y));
    });
    state.borrow().drawing_area.add_controller(motion);

    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let state_for_scroll = Rc::clone(state);
    scroll.connect_scroll(move |controller, _dx, dy| {
        handle_mouse_scroll(&state_for_scroll, controller_event_point(controller), dy);
        glib::Propagation::Stop
    });
    state.borrow().drawing_area.add_controller(scroll);
}

fn handle_left_button_down(state: &SharedState, point: UiPoint) {
    let mut should_wake_pty_timer = false;
    let mut command_button_to_run = None;
    let result = {
        let mut state_ref = state.borrow_mut();
        state_ref.drawing_area.grab_focus();

        if state_ref.layout.splitter_at(point) {
            state_ref.runtime.splitter_drag = Some(SplitterDrag {
                pointer_offset_x: point.x.saturating_sub(state_ref.layout.splitter.x),
                pending_splitter_x: None,
                resize_scheduled: false,
                deferred_terminal_size: None,
            });
            return;
        }

        if let Some(id) = state_ref.layout.tab_close_at(point) {
            should_wake_pty_timer = true;
            state_ref.session.tabs.close_tab(id).and_then(|()| {
                state_ref.set_tab_views();
                state_ref.replace_terminal_viewport()
            })
        } else if state_ref.layout.new_tab_at(point) {
            should_wake_pty_timer = true;
            state_ref.session.tabs.open_tab().and_then(|_| {
                state_ref.set_tab_views();
                state_ref.replace_terminal_viewport()
            })
        } else if let Some(id) = state_ref.layout.tab_at(point) {
            should_wake_pty_timer = true;
            state_ref.session.tabs.switch_to_tab(id).and_then(|()| {
                state_ref.set_tab_views();
                state_ref.replace_terminal_viewport()
            })
        } else if let Some(id) = state_ref.layout.command_button_at(point) {
            command_button_to_run = Some(id);
            Ok(())
        } else if let Some(anchor) = state_ref.terminal_grid_point_at(point, false) {
            state_ref.runtime.terminal_selection_drag = Some(TerminalSelectionDrag { anchor });
            state_ref.clear_terminal_selection();
            Ok(())
        } else {
            Ok(())
        }
    };

    let wake_pty_timer = should_wake_pty_timer && result.is_ok();
    if let Err(error) = result {
        state.borrow_mut().record_error(error);
    }
    if wake_pty_timer {
        wake_pty_drain_timer(state);
    }
    if let Err(error) = sync_all_widgets(state) {
        state.borrow_mut().record_error(error);
    }
    if let Some(button_id) = command_button_to_run {
        run_command_button(state, button_id);
    }
    state.borrow().drawing_area.queue_draw();
}

fn handle_mouse_motion(state: &SharedState, point: UiPoint) {
    {
        let mut state_ref = state.borrow_mut();
        if let Some(drag) = state_ref.runtime.terminal_selection_drag {
            if let Some(focus) = state_ref.terminal_grid_point_at(point, true)
                && state_ref.update_terminal_selection(drag.anchor, focus)
            {
                state_ref.drawing_area.queue_draw();
            }
            return;
        }

        let Some(splitter) = state_ref.runtime.splitter_drag else {
            return;
        };
        let splitter_x = point.x.saturating_sub(splitter.pointer_offset_x);
        let Some(splitter) = state_ref.runtime.splitter_drag.as_mut() else {
            return;
        };
        if splitter.pending_splitter_x == Some(splitter_x) {
            return;
        }
        splitter.pending_splitter_x = Some(splitter_x);
        if splitter.resize_scheduled {
            return;
        }
        splitter.resize_scheduled = true;
    }
    schedule_splitter_resize(state);
}

fn schedule_splitter_resize(state: &SharedState) {
    let state_for_resize = Rc::clone(state);
    glib::idle_add_local_once(move || {
        process_pending_splitter_resize(&state_for_resize);
    });
}

fn process_pending_splitter_resize(state: &SharedState) {
    let result = {
        let mut state_ref = state.borrow_mut();
        let Some(splitter) = state_ref.runtime.splitter_drag.as_mut() else {
            return;
        };
        let Some(splitter_x) = splitter.pending_splitter_x.take() else {
            splitter.resize_scheduled = false;
            return;
        };
        splitter.resize_scheduled = false;
        resize_command_panel_from_splitter(&mut state_ref, splitter_x)
    };

    match result {
        Ok(SplitterResizeWidgetSync::Unchanged) => return,
        Ok(sync) => {
            if let Err(error) = sync_splitter_resize_widgets(state, sync) {
                state.borrow_mut().record_error(error);
            }
        }
        Err(error) => state.borrow_mut().record_error(error),
    }
    state.borrow().drawing_area.queue_draw();
}

fn handle_left_button_up(state: &SharedState) {
    process_pending_splitter_resize(state);
    let result = {
        let mut state_ref = state.borrow_mut();
        state_ref.runtime.terminal_selection_drag = None;
        let deferred_size = state_ref
            .runtime
            .splitter_drag
            .take()
            .and_then(|drag| drag.deferred_terminal_size);
        match deferred_size {
            Some(size) => state_ref.session.execute(TerminalCommand::Resize(size)),
            None => Ok(()),
        }
    };

    if let Err(error) = result {
        state.borrow_mut().record_error(error);
    }
}

fn handle_mouse_scroll(state: &SharedState, point: Option<UiPoint>, dy: f64) {
    if point.is_some_and(|point| {
        state
            .borrow()
            .layout
            .command_button_viewport
            .contains(point)
    }) {
        handle_command_button_scroll_delta(state, dy);
        return;
    }

    if point.is_some_and(|point| !state.borrow().layout.terminal.contains(point)) {
        return;
    }

    let line_delta = terminal_scroll_lines_from_delta(dy);
    if line_delta == 0 {
        return;
    }

    let result = {
        let mut state_ref = state.borrow_mut();
        state_ref.apply_terminal_scroll(TerminalScroll::Lines(line_delta))
    };
    if let Err(error) = result {
        state.borrow_mut().record_error(error);
    }
    if let Err(error) = sync_terminal_scrollbar(state) {
        state.borrow_mut().record_error(error);
    }
    state.borrow().drawing_area.queue_draw();
}

fn handle_command_button_scroll_delta(state: &SharedState, dy: f64) {
    let line_delta = command_button_scroll_lines_from_delta(dy);
    if line_delta == 0 {
        return;
    }

    let scrolled = state
        .borrow_mut()
        .apply_command_button_scroll_lines(line_delta);
    if !scrolled {
        return;
    }

    if let Err(error) = sync_command_widgets(state) {
        state.borrow_mut().record_error(error);
    }
    state.borrow().drawing_area.queue_draw();
}

fn resize_command_panel_from_splitter(
    state: &mut GtkWindowState,
    splitter_x: i32,
) -> AppResult<SplitterResizeWidgetSync> {
    let width = state.layout.tab_bar.width.max(1);
    let height = state
        .layout
        .tab_bar
        .height
        .saturating_add(state.layout.terminal.height)
        .max(1);
    let command_panel_width = WindowLayout::command_panel_width_from_splitter_x(width, splitter_x);
    if command_panel_width == state.command_panel_width {
        return Ok(SplitterResizeWidgetSync::Unchanged);
    }

    let previous_terminal_size = terminal_size_from_area(state.layout.terminal, state.metrics)?;
    state.command_panel_width = command_panel_width;
    let resized_in_place = state.layout.try_resize_command_panel_width(
        width,
        height,
        state.command_panel_width,
        state.session.command_panel.selected_buttons(),
        state.command_button_scroll_position,
    );
    if !resized_in_place {
        state.layout = layout_from_client_size(
            width,
            height,
            state.command_panel_width,
            state.session.command_panel.selected_buttons(),
            &state.tab_views,
            state.command_button_scroll_position,
        );
    }
    state.command_button_scroll_position = state.layout.command_button_scroll_position();
    let terminal_size = terminal_size_from_area(state.layout.terminal, state.metrics)?;
    if state.runtime.splitter_drag.is_some()
        && previous_terminal_size.rows == terminal_size.rows
        && previous_terminal_size.columns == terminal_size.columns
        && state.terminal_viewport.is_some()
    {
        if let Some(drag) = state.runtime.splitter_drag.as_mut() {
            drag.deferred_terminal_size = Some(terminal_size);
        }
    } else {
        if let Some(drag) = state.runtime.splitter_drag.as_mut() {
            drag.deferred_terminal_size = None;
        }
        state
            .session
            .execute(TerminalCommand::Resize(terminal_size))?;
        state.clear_terminal_selection();
        state.replace_terminal_viewport()?;
    }
    if resized_in_place {
        Ok(SplitterResizeWidgetSync::GeometryOnly)
    } else {
        Ok(SplitterResizeWidgetSync::RebuildCommandWidgets)
    }
}

fn install_combo_handler(state: &SharedState) {
    let state_for_combo = Rc::clone(state);
    state.borrow().category_combo.connect_changed(move |combo| {
        if state_for_combo.borrow().syncing_category_combo {
            return;
        }
        let Some(index) = combo.active() else {
            return;
        };
        apply_category_selection(&state_for_combo, index as usize);
    });

    let state_for_menu = Rc::clone(state);
    install_secondary_click_menu(&state.borrow().category_combo, move |widget| {
        show_category_menu(&state_for_menu, &widget);
    });
}

fn apply_category_selection(state: &SharedState, index: usize) {
    let previous = state.borrow().session.command_panel.clone();
    let previous_scroll = state.borrow().command_button_scroll_position;
    let result = (|| -> AppResult<()> {
        let mut state_ref = state.borrow_mut();
        state_ref.session.select_command_category_by_index(index)?;
        state_ref.refresh_command_panel_layout(0);
        Ok(())
    })();

    if let Err(error) = result {
        let mut state_ref = state.borrow_mut();
        state_ref.session.command_panel = previous;
        state_ref.refresh_command_panel_layout(previous_scroll);
        state_ref.record_error(error);
    }
    if let Err(error) = sync_all_widgets(state) {
        state.borrow_mut().record_error(error);
    }
    state.borrow().drawing_area.queue_draw();
}

fn install_scrollbar_handlers(state: &SharedState) {
    let command_adjustment = state.borrow().command_button_scrollbar.adjustment();
    let state_for_command_scroll = Rc::clone(state);
    command_adjustment.connect_value_changed(move |adjustment| {
        if state_for_command_scroll.borrow().syncing_command_scrollbar {
            return;
        }
        let position = adjustment.value().round().max(0.0) as usize;
        {
            let mut state = state_for_command_scroll.borrow_mut();
            state.command_button_scroll_position = position;
            state.layout = layout_from_client_size(
                state.layout.tab_bar.width,
                state
                    .layout
                    .tab_bar
                    .height
                    .saturating_add(state.layout.terminal.height),
                state.command_panel_width,
                state.session.command_panel.selected_buttons(),
                &state.tab_views,
                state.command_button_scroll_position,
            );
            state.command_button_scroll_position = state.layout.command_button_scroll_position();
        }
        if let Err(error) = sync_command_widgets(&state_for_command_scroll) {
            state_for_command_scroll.borrow_mut().record_error(error);
        }
        state_for_command_scroll.borrow().drawing_area.queue_draw();
    });

    let terminal_adjustment = state.borrow().terminal_scrollbar.adjustment();
    let state_for_terminal_scroll = Rc::clone(state);
    terminal_adjustment.connect_value_changed(move |adjustment| {
        if state_for_terminal_scroll
            .borrow()
            .syncing_terminal_scrollbar
        {
            return;
        }
        let scroll_state = state_for_terminal_scroll.borrow().terminal_scroll_state();
        let position = adjustment.value().round().max(0.0) as usize;
        let scroll = TerminalScroll::Absolute(scroll_state.max_position.saturating_sub(position));
        let result = state_for_terminal_scroll
            .borrow_mut()
            .apply_terminal_scroll(scroll);
        if let Err(error) = result {
            state_for_terminal_scroll.borrow_mut().record_error(error);
        }
        state_for_terminal_scroll.borrow().drawing_area.queue_draw();
    });
}

fn install_close_handler(state: &SharedState) {
    let state_for_close = Rc::clone(state);
    state.borrow().window.connect_close_request(move |_| {
        let save = match finish_command_panel_save_worker_before_shutdown(&state_for_close) {
            Ok(save) => save,
            Err(error) => {
                state_for_close.borrow_mut().record_error(error);
                return glib::Propagation::Stop;
            }
        };

        match save {
            CommandPanelShutdownSave::Complete => {
                if let Err(error) = state_for_close.borrow_mut().session.shutdown() {
                    state_for_close.borrow_mut().record_error(error);
                    return glib::Propagation::Stop;
                }
            }
            CommandPanelShutdownSave::Delayed => {
                state_for_close
                    .borrow_mut()
                    .record_error(AppError::InvalidState(
                        "command panel settings save is still running; try closing again",
                    ));
                return glib::Propagation::Stop;
            }
        }
        glib::Propagation::Proceed
    });
}

fn active_pty_timer_interval() -> Duration {
    Duration::from_millis(PTY_ACTIVE_TIMER_MS)
}

fn command_panel_save_debounce_interval() -> Duration {
    Duration::from_millis(COMMAND_PANEL_SAVE_DEBOUNCE_MS)
}

fn command_panel_save_result_poll_interval() -> Duration {
    Duration::from_millis(COMMAND_PANEL_SAVE_RESULT_POLL_MS)
}

fn command_panel_save_shutdown_wait_timeout() -> Duration {
    Duration::from_millis(COMMAND_PANEL_SAVE_SHUTDOWN_WAIT_MS)
}

fn schedule_pty_drain_timer(state: &SharedState, interval: Duration) {
    let generation = {
        let mut state_ref = state.borrow_mut();
        state_ref.runtime.pty_drain_timer.schedule_next()
    };
    let state = Rc::clone(state);
    glib::timeout_add_local(interval, move || {
        run_pty_drain_timer(&state, generation);
        glib::ControlFlow::Break
    });
}

fn wake_pty_drain_timer(state: &SharedState) {
    state.borrow_mut().runtime.pty_drain_timer.record_activity();
    schedule_pty_drain_timer(state, active_pty_timer_interval());
}

fn schedule_command_panel_save(state: &SharedState) {
    let generation = {
        let mut state_ref = state.borrow_mut();
        state_ref.runtime.command_panel_save.schedule_next()
    };
    let state = Rc::clone(state);
    glib::timeout_add_local(command_panel_save_debounce_interval(), move || {
        flush_deferred_command_panel_save(&state, generation);
        glib::ControlFlow::Break
    });
}

fn flush_deferred_command_panel_save(state: &SharedState, generation: u64) {
    let save_request = {
        let mut state_ref = state.borrow_mut();
        if !state_ref.runtime.command_panel_save.is_current(generation) {
            return;
        }
        if state_ref.runtime.command_panel_save.save_in_progress() {
            return;
        }

        let (sender, receiver) = mpsc::channel();
        let settings = AppSettings {
            command_panel: state_ref.session.command_panel.clone(),
            terminal_font: state_ref.session.terminal_font.clone(),
        };
        let request = state_ref
            .session
            .config_store
            .prepare_settings_save(&settings);
        if !state_ref
            .runtime
            .command_panel_save
            .mark_save_started(generation, receiver)
        {
            return;
        }

        (request, sender)
    };

    let (request, sender) = save_request;
    match thread::Builder::new()
        .name("j3term-command-panel-save".to_owned())
        .spawn(move || {
            let _ = sender.send(request.save());
        }) {
        Ok(_handle) => schedule_command_panel_save_result_poll(state, generation),
        Err(source) => {
            let mut state_ref = state.borrow_mut();
            state_ref
                .runtime
                .command_panel_save
                .clear_save_in_progress(generation);
            state_ref.record_error(AppError::io("start command panel save worker", source));
        }
    }
}

fn schedule_command_panel_save_result_poll(state: &SharedState, generation: u64) {
    let state = Rc::clone(state);
    glib::timeout_add_local(command_panel_save_result_poll_interval(), move || {
        poll_command_panel_save_result(&state, generation)
    });
}

fn poll_command_panel_save_result(state: &SharedState, generation: u64) -> glib::ControlFlow {
    let poll = {
        state
            .borrow_mut()
            .runtime
            .command_panel_save
            .poll_save_result(generation)
    };

    match poll {
        CommandPanelSavePoll::Pending => glib::ControlFlow::Continue,
        CommandPanelSavePoll::Missing => glib::ControlFlow::Break,
        CommandPanelSavePoll::Finished(result) => {
            match result {
                Ok(()) => {
                    let mut state_ref = state.borrow_mut();
                    if !state_ref.runtime.command_panel_save.has_pending_save() {
                        state_ref.session.mark_command_panel_saved();
                    }
                }
                Err(error) => state.borrow_mut().record_error(error),
            }
            if state.borrow().runtime.command_panel_save.has_pending_save() {
                schedule_command_panel_save(state);
            }
            glib::ControlFlow::Break
        }
    }
}

fn finish_command_panel_save_worker_before_shutdown(
    state: &SharedState,
) -> AppResult<CommandPanelShutdownSave> {
    let task = {
        state
            .borrow_mut()
            .runtime
            .command_panel_save
            .take_save_in_progress()
    };

    let Some(task) = task else {
        return Ok(CommandPanelShutdownSave::Complete);
    };

    match task.wait_before_shutdown(command_panel_save_shutdown_wait_timeout())? {
        CommandPanelSaveShutdownWait::Finished { generation, result } => {
            finish_command_panel_save_result_before_shutdown(state, generation, result)?;
            Ok(CommandPanelShutdownSave::Complete)
        }
        CommandPanelSaveShutdownWait::Delayed(task) => {
            state
                .borrow_mut()
                .runtime
                .command_panel_save
                .restore_save_in_progress(task);
            Ok(CommandPanelShutdownSave::Delayed)
        }
    }
}

fn finish_command_panel_save_result_before_shutdown(
    state: &SharedState,
    generation: u64,
    result: AppResult<()>,
) -> AppResult<()> {
    if result.is_ok() {
        let mut state_ref = state.borrow_mut();
        state_ref.runtime.command_panel_save.mark_saved(generation);
        if !state_ref.runtime.command_panel_save.has_pending_save() {
            state_ref.session.mark_command_panel_saved();
        }
    }
    result
}

fn run_pty_drain_timer(state: &SharedState, generation: u64) {
    if !state
        .borrow()
        .runtime
        .pty_drain_timer
        .is_current(generation)
    {
        return;
    }

    let drain = drain_pty_and_redraw(state);
    let interval = {
        let mut state_ref = state.borrow_mut();
        if !state_ref.runtime.pty_drain_timer.is_current(generation) {
            return;
        }
        state_ref
            .runtime
            .pty_drain_timer
            .interval_after_drain(drain.as_ref())
    };
    schedule_pty_drain_timer(state, interval);
}

fn sync_all_widgets(state: &SharedState) -> AppResult<()> {
    sync_category_combo(state)?;
    sync_command_widgets(state)?;
    sync_terminal_scrollbar(state)
}

fn sync_splitter_resize_widgets(
    state: &SharedState,
    sync: SplitterResizeWidgetSync,
) -> AppResult<()> {
    match sync {
        SplitterResizeWidgetSync::Unchanged => Ok(()),
        SplitterResizeWidgetSync::GeometryOnly => {
            sync_category_combo_geometry(state);
            sync_command_widget_bounds(state)?;
            sync_command_scrollbar(state)?;
            sync_terminal_scrollbar(state)
        }
        SplitterResizeWidgetSync::RebuildCommandWidgets => {
            sync_category_combo_geometry(state);
            sync_command_widgets(state)?;
            sync_terminal_scrollbar(state)
        }
    }
}

fn sync_category_combo(state: &SharedState) -> AppResult<()> {
    let (combo, names, active, bounds) = {
        let mut state_ref = state.borrow_mut();
        state_ref.syncing_category_combo = true;
        (
            state_ref.category_combo.clone(),
            state_ref
                .session
                .command_panel
                .categories()
                .iter()
                .map(|category| category.name.clone())
                .collect::<Vec<_>>(),
            state_ref
                .session
                .command_panel
                .selected_category_index()
                .map(|i| i as u32),
            state_ref.layout.command_category_selector,
        )
    };

    combo.remove_all();
    for name in names {
        combo.append_text(&name);
    }
    configure_category_combo(&combo);
    combo.set_active(active);

    if let Some(bounds) = bounds {
        combo.set_visible(true);
        move_overlay_widget(&combo, bounds);
    } else {
        combo.set_visible(false);
    }

    state.borrow_mut().syncing_category_combo = false;
    Ok(())
}

fn sync_category_combo_geometry(state: &SharedState) {
    let (combo, bounds) = {
        let state_ref = state.borrow();
        (
            state_ref.category_combo.clone(),
            state_ref.layout.command_category_selector,
        )
    };

    if let Some(bounds) = bounds {
        combo.set_visible(true);
        move_overlay_widget(&combo, bounds);
    } else {
        combo.set_visible(false);
    }
}

fn sync_command_widgets(state: &SharedState) -> AppResult<()> {
    let (overlay, placements, old_buttons) = {
        let mut state_ref = state.borrow_mut();
        (
            state_ref.overlay.clone(),
            state_ref.layout.buttons.clone(),
            std::mem::take(&mut state_ref.command_buttons),
        )
    };
    let mut buttons_by_id = old_buttons
        .into_iter()
        .map(|widget| (widget.id, widget))
        .collect::<HashMap<_, _>>();
    let mut next_buttons = Vec::with_capacity(placements.len());

    for placement in placements {
        if let Some(mut widget) = buttons_by_id.remove(&placement.id) {
            if widget.label != placement.label {
                widget.label_widget.set_text(&placement.label);
                widget.button.set_tooltip_text(Some(&placement.label));
                widget.label = placement.label;
            }
            move_overlay_widget(&widget.button, placement.bounds);
            next_buttons.push(widget);
        } else {
            let (button, label_widget) =
                new_command_button_widget(state, placement.id, &placement.label);
            add_overlay_widget(&overlay, &button);
            move_overlay_widget(&button, placement.bounds);
            next_buttons.push(CommandButtonWidget {
                id: placement.id,
                label: placement.label,
                button,
                label_widget,
            });
        }
    }

    for widget in buttons_by_id.into_values() {
        overlay.remove_overlay(&widget.button);
    }

    state.borrow_mut().command_buttons = next_buttons;
    sync_command_scrollbar(state)
}

fn sync_command_widget_bounds(state: &SharedState) -> AppResult<()> {
    let reusable = {
        let state_ref = state.borrow();
        state_ref.command_buttons.len() == state_ref.layout.buttons.len()
            && state_ref
                .command_buttons
                .iter()
                .zip(&state_ref.layout.buttons)
                .all(|(widget, placement)| widget.id == placement.id)
    };
    if !reusable {
        return sync_command_widgets(state);
    }

    let state_ref = state.borrow();
    for (widget, placement) in state_ref
        .command_buttons
        .iter()
        .zip(&state_ref.layout.buttons)
    {
        move_overlay_widget(&widget.button, placement.bounds);
    }
    Ok(())
}

fn new_command_button_widget(
    state: &SharedState,
    button_id: CommandButtonId,
    label: &str,
) -> (gtk::Button, gtk::Label) {
    let button = gtk::Button::new();
    let label_widget = gtk::Label::new(Some(label));
    configure_ellipsized_label(&label_widget, 10);
    label_widget.set_hexpand(true);
    label_widget.set_halign(gtk::Align::Fill);
    button.set_child(Some(&label_widget));
    button.set_focusable(false);
    button.set_tooltip_text(Some(label));
    button.set_css_classes(&["j3term-command-button"]);
    button.set_hexpand(false);
    button.set_vexpand(false);

    let state_for_click = Rc::clone(state);
    button.connect_clicked(move |_| {
        run_command_button(&state_for_click, button_id);
    });

    let button_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let state_for_scroll = Rc::clone(state);
    button_scroll.connect_scroll(move |_, _dx, dy| {
        handle_command_button_scroll_delta(&state_for_scroll, dy);
        glib::Propagation::Stop
    });
    button.add_controller(button_scroll);

    let state_for_menu = Rc::clone(state);
    install_secondary_click_menu(&button, move |widget| {
        show_button_menu(&state_for_menu, &widget, button_id);
    });

    (button, label_widget)
}

fn configure_category_combo(combo: &gtk::ComboBoxText) {
    combo.set_hexpand(false);
    combo.set_vexpand(false);
    combo.set_popup_fixed_width(true);
    combo.set_overflow(gtk::Overflow::Hidden);
    for cell in combo.cells() {
        if let Ok(text_cell) = cell.downcast::<gtk::CellRendererText>() {
            text_cell.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text_cell.set_width_chars(1);
            text_cell.set_max_width_chars(8);
        }
    }
}

fn configure_ellipsized_label(label: &gtk::Label, max_width_chars: i32) {
    label.set_single_line_mode(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_width_chars(1);
    label.set_max_width_chars(max_width_chars);
    label.set_xalign(0.5);
    label.set_yalign(0.5);
}

fn install_secondary_click_menu<W>(widget: &W, action: impl Fn(gtk::Widget) + 'static)
where
    W: IsA<gtk::Widget>,
{
    let action: Rc<dyn Fn(gtk::Widget)> = Rc::new(action);
    let controller = gtk::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_event(move |controller, event| {
        if !is_secondary_button_release(event) {
            return glib::Propagation::Proceed;
        }
        let Some(widget) = controller.widget() else {
            return glib::Propagation::Proceed;
        };

        let action = Rc::clone(&action);
        glib::idle_add_local_once(move || action(widget));
        glib::Propagation::Stop
    });
    widget.add_controller(controller);
}

fn is_secondary_button_release(event: &gdk::Event) -> bool {
    if event.event_type() != gdk::EventType::ButtonRelease {
        return false;
    }

    event.triggers_context_menu()
        || event
            .downcast_ref::<gdk::ButtonEvent>()
            .is_some_and(|event| event.button() == 3)
}

fn sync_command_scrollbar(state: &SharedState) -> AppResult<()> {
    let (scrollbar, scroll) = {
        let mut state_ref = state.borrow_mut();
        let Some(scroll) = state_ref.layout.command_button_scroll else {
            state_ref.command_button_scrollbar.set_visible(false);
            return Ok(());
        };
        state_ref.syncing_command_scrollbar = true;
        (state_ref.command_button_scrollbar.clone(), scroll)
    };

    let adjustment = scrollbar.adjustment();
    adjustment.set_lower(0.0);
    adjustment.set_upper(scroll.total_len.max(1) as f64);
    adjustment.set_page_size(scroll.page_len.max(1) as f64);
    adjustment.set_step_increment(1.0);
    adjustment.set_page_increment(scroll.page_len.max(1) as f64);
    adjustment.set_value(scroll.position as f64);

    scrollbar.set_visible(true);
    move_overlay_widget(&scrollbar, scroll.bounds);
    state.borrow_mut().syncing_command_scrollbar = false;
    Ok(())
}

fn sync_terminal_scrollbar(state: &SharedState) -> AppResult<()> {
    let (scrollbar, bounds, scroll, value) = {
        let mut state_ref = state.borrow_mut();
        let Some(bounds) = terminal_scrollbar_bounds(state_ref.layout.terminal) else {
            state_ref.terminal_scrollbar.set_visible(false);
            return Ok(());
        };
        let scroll = state_ref.terminal_scroll_state();
        let value = scroll.max_position.saturating_sub(scroll.position);
        state_ref.syncing_terminal_scrollbar = true;
        (state_ref.terminal_scrollbar.clone(), bounds, scroll, value)
    };

    let adjustment = scrollbar.adjustment();
    adjustment.set_lower(0.0);
    adjustment.set_upper(scroll.total_len.max(1) as f64);
    adjustment.set_page_size(scroll.page_len.max(1) as f64);
    adjustment.set_step_increment(1.0);
    adjustment.set_page_increment(scroll.page_len.max(1) as f64);
    adjustment.set_value(value as f64);

    scrollbar.set_visible(true);
    move_overlay_widget(&scrollbar, bounds);
    state.borrow_mut().syncing_terminal_scrollbar = false;
    Ok(())
}

fn add_overlay_widget<W>(overlay: &gtk::Overlay, widget: &W)
where
    W: IsA<gtk::Widget>,
{
    widget.set_halign(gtk::Align::Start);
    widget.set_valign(gtk::Align::Start);
    overlay.add_overlay(widget);
}

fn move_overlay_widget<W>(widget: &W, bounds: UiRect)
where
    W: IsA<gtk::Widget>,
{
    widget.set_margin_start(bounds.x.max(0));
    widget.set_margin_top(bounds.y.max(0));
    widget.set_margin_end(0);
    widget.set_margin_bottom(0);
    widget.set_size_request(bounds.width.max(1), bounds.height.max(1));
}

fn run_command_button(state: &SharedState, button_id: CommandButtonId) {
    let button = match state.borrow().session.command_button(button_id) {
        Ok(button) => button,
        Err(error) => {
            state.borrow_mut().record_error(error);
            return;
        }
    };

    let values = match collect_button_argument_values(state, &button) {
        Ok(Some(values)) => values,
        Ok(None) => return,
        Err(error) => {
            state.borrow_mut().record_error(error);
            return;
        }
    };

    let result = state
        .borrow_mut()
        .session
        .run_button_command(button, values);
    match result {
        Ok(()) => wake_pty_drain_timer(state),
        Err(error) => state.borrow_mut().record_error(error),
    }
    state.borrow().drawing_area.grab_focus();
}

fn collect_button_argument_values(
    state: &SharedState,
    button: &CommandButton,
) -> AppResult<Option<ButtonArgumentValues>> {
    let inputs = button.required_argument_inputs();
    if !inputs.any() {
        return Ok(Some(ButtonArgumentValues::default()));
    }

    let window = state.borrow().window.clone();
    let mut values = ButtonArgumentValues::default();
    if inputs.select_file {
        let Some(path) = select_file(&window, "Select File")? else {
            return Ok(None);
        };
        values.set_selected_file_path(path);
    }

    if inputs.select_dir {
        let Some(path) = select_folder(&window)? else {
            return Ok(None);
        };
        values.set_selected_dir_path(path);
    }

    if inputs.input_text {
        let Some(text) = prompt_text(&window, "Input Text", "Text", "", TextInputValidation::None)?
        else {
            return Ok(None);
        };
        values.input_text = Some(text);
    }

    values.validate_for(inputs)?;
    Ok(Some(values))
}

fn show_category_menu(state: &SharedState, widget: &gtk::Widget) {
    let _ = widget;
    let can_delete = state.borrow().session.command_panel.categories().len() > 1;
    let can_move_up = state
        .borrow()
        .session
        .command_panel
        .can_move_selected_category_up();
    let can_move_down = state
        .borrow()
        .session
        .command_panel
        .can_move_selected_category_down();
    let window = state.borrow().window.clone();
    let (menu, items) = new_menu_window(&window, "Category Menu");

    add_window_menu_button(&menu, &items, "New Category", true, {
        let state = Rc::clone(state);
        move || prompt_new_category(&state)
    });
    add_window_menu_button(&menu, &items, "Rename Category", true, {
        let state = Rc::clone(state);
        move || prompt_rename_category(&state)
    });
    add_window_menu_button(&menu, &items, "Delete Category", can_delete, {
        let state = Rc::clone(state);
        move || delete_selected_category(&state)
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "Move Category Up", can_move_up, {
        let state = Rc::clone(state);
        move || save_command_panel_change(&state, |panel| panel.move_selected_category_up())
    });
    add_window_menu_button(&menu, &items, "Move Category Down", can_move_down, {
        let state = Rc::clone(state);
        move || save_command_panel_change(&state, |panel| panel.move_selected_category_down())
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "Add Button", true, {
        let state = Rc::clone(state);
        move || add_button_to_selected_category(&state)
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "Font Settings...", true, {
        let state = Rc::clone(state);
        move || edit_terminal_font(&state)
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "About j3Term...", true, {
        let state = Rc::clone(state);
        move || {
            let window = state.borrow().window.clone();
            show_about(&window);
        }
    });

    menu.present();
}

fn show_button_menu(state: &SharedState, widget: &gtk::Widget, button_id: CommandButtonId) {
    let _ = widget;
    let can_move_up = state
        .borrow()
        .session
        .command_panel
        .can_move_button_up(button_id);
    let can_move_down = state
        .borrow()
        .session
        .command_panel
        .can_move_button_down(button_id);
    let window = state.borrow().window.clone();
    let (menu, items) = new_menu_window(&window, "Button Menu");

    add_window_menu_button(&menu, &items, "Run Command", true, {
        let state = Rc::clone(state);
        move || run_command_button(&state, button_id)
    });
    add_window_menu_button(&menu, &items, "Edit Button", true, {
        let state = Rc::clone(state);
        move || edit_button(&state, button_id)
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "Delete Button", true, {
        let state = Rc::clone(state);
        move || delete_button(&state, button_id)
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "Move Button Up", can_move_up, {
        let state = Rc::clone(state);
        move || save_command_panel_change(&state, |panel| panel.move_button_up(button_id))
    });
    add_window_menu_button(&menu, &items, "Move Button Down", can_move_down, {
        let state = Rc::clone(state);
        move || save_command_panel_change(&state, |panel| panel.move_button_down(button_id))
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "Font Settings...", true, {
        let state = Rc::clone(state);
        move || edit_terminal_font(&state)
    });
    add_menu_separator(&items);
    add_window_menu_button(&menu, &items, "About j3Term...", true, {
        let state = Rc::clone(state);
        move || {
            let window = state.borrow().window.clone();
            show_about(&window);
        }
    });

    menu.present();
}

fn new_menu_window(parent: &gtk::ApplicationWindow, title: &str) -> (gtk::Window, gtk::Box) {
    let menu = gtk::Window::builder()
        .title(title)
        .modal(true)
        .transient_for(parent)
        .default_width(260)
        .resizable(false)
        .build();
    if let Some(application) = parent.application() {
        menu.set_application(Some(&application));
    }
    install_escape_to_close(&menu);

    let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
    items.set_spacing(0);
    items.set_margin_top(8);
    items.set_margin_bottom(8);
    items.set_margin_start(8);
    items.set_margin_end(8);
    menu.set_child(Some(&items));
    (menu, items)
}

fn add_window_menu_button(
    menu: &gtk::Window,
    items: &gtk::Box,
    label: &str,
    sensitive: bool,
    action: impl Fn() + 'static,
) {
    let button = gtk::Button::with_label(label);
    button.set_focusable(true);
    button.set_sensitive(sensitive);
    button.set_css_classes(&["flat"]);
    button.set_tooltip_text(Some(label));
    button.set_margin_top(1);
    button.set_margin_bottom(1);
    button.set_hexpand(true);
    let menu = menu.clone();
    button.connect_clicked(move |_| {
        menu.close();
        action();
    });
    items.append(&button);
}

fn install_escape_to_close(window: &gtk::Window) {
    let key = gtk::EventControllerKey::new();
    let window_for_key = window.clone();
    key.connect_key_pressed(move |_, key, _code, _modifiers| {
        if key == gdk::Key::Escape {
            window_for_key.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key);
}

fn add_menu_separator(items: &gtk::Box) {
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(3);
    separator.set_margin_bottom(3);
    items.append(&separator);
}

fn prompt_new_category(state: &SharedState) {
    let window = state.borrow().window.clone();
    let initial = state
        .borrow()
        .session
        .command_panel
        .suggested_new_category_name();
    match prompt_text(
        &window,
        "New Category",
        "Name",
        &initial,
        TextInputValidation::CategoryName,
    ) {
        Ok(Some(name)) => {
            save_command_panel_change(state, |panel| panel.add_category_named(name).map(|_| ()))
        }
        Ok(None) => {}
        Err(error) => state.borrow_mut().record_error(error),
    }
}

fn prompt_rename_category(state: &SharedState) {
    let window = state.borrow().window.clone();
    let initial = state
        .borrow()
        .session
        .command_panel
        .selected_category()
        .map(|category| category.name.clone())
        .unwrap_or_default();
    match prompt_text(
        &window,
        "Rename Category",
        "Name",
        &initial,
        TextInputValidation::CategoryName,
    ) {
        Ok(Some(name)) => {
            save_command_panel_change(state, |panel| panel.rename_selected_category(name))
        }
        Ok(None) => {}
        Err(error) => state.borrow_mut().record_error(error),
    }
}

fn delete_selected_category(state: &SharedState) {
    let window = state.borrow().window.clone();
    if !confirm(
        &window,
        "Delete Category",
        "Delete this category and all buttons in it?",
    ) {
        return;
    }
    save_command_panel_change(state, |panel| panel.delete_selected_category());
}

fn add_button_to_selected_category(state: &SharedState) {
    let window = state.borrow().window.clone();
    let arguments = match CommandArguments::new("{inputtext}") {
        Ok(arguments) => arguments,
        Err(error) => {
            state.borrow_mut().record_error(error);
            return;
        }
    };
    let initial = match CommandButtonDefinition::new("new command", "echo", arguments) {
        Ok(definition) => definition,
        Err(error) => {
            state.borrow_mut().record_error(error);
            return;
        }
    };

    match edit_command_button(&window, &initial) {
        Ok(Some(definition)) => save_command_panel_change(state, |panel| {
            panel
                .add_button_to_selected_category(definition)
                .map(|_| ())
        }),
        Ok(None) => {}
        Err(error) => state.borrow_mut().record_error(error),
    }
}

fn edit_button(state: &SharedState, button_id: CommandButtonId) {
    let window = state.borrow().window.clone();
    let initial = match state.borrow().session.command_button(button_id) {
        Ok(button) => button.definition(),
        Err(error) => {
            state.borrow_mut().record_error(error);
            return;
        }
    };

    match edit_command_button(&window, &initial) {
        Ok(Some(definition)) => {
            save_command_panel_change(state, |panel| panel.update_button(button_id, definition))
        }
        Ok(None) => {}
        Err(error) => state.borrow_mut().record_error(error),
    }
}

fn delete_button(state: &SharedState, button_id: CommandButtonId) {
    let window = state.borrow().window.clone();
    if !confirm(&window, "Delete Button", "Delete this command button?") {
        return;
    }
    save_command_panel_change(state, |panel| panel.delete_button(button_id));
}

fn edit_terminal_font(state: &SharedState) {
    let window = state.borrow().window.clone();
    let current = state.borrow().session.terminal_font().clone();
    let font = match choose_terminal_font(&window, &current) {
        Ok(Some(font)) => font,
        Ok(None) => return,
        Err(error) => {
            state.borrow_mut().record_error(error);
            return;
        }
    };

    let result = {
        let mut state_ref = state.borrow_mut();
        state_ref.session.set_terminal_font(font).map(|()| {
            state_ref.terminal_line_text_cache.clear();
            state_ref.terminal_surface_cache.clear();
            state_ref.pending_terminal_paint = TerminalViewportInvalidation::Full;
        })
    };

    match result {
        Ok(()) => {
            schedule_command_panel_save(state);
            state.borrow().drawing_area.queue_draw();
            state.borrow().drawing_area.grab_focus();
        }
        Err(error) => state.borrow_mut().record_error(error),
    }
}

fn save_command_panel_change(
    state: &SharedState,
    change: impl FnOnce(&mut CommandPanel) -> AppResult<()>,
) {
    let previous = state.borrow().session.command_panel.clone();
    let previous_scroll = state.borrow().command_button_scroll_position;
    let result = (|| -> AppResult<()> {
        let mut state_ref = state.borrow_mut();
        change(&mut state_ref.session.command_panel)?;
        state_ref.session.mark_command_panel_dirty();
        state_ref.refresh_command_panel_layout_preserving_scroll();
        Ok(())
    })();

    if let Err(error) = result {
        let mut state_ref = state.borrow_mut();
        state_ref.session.command_panel = previous;
        state_ref.refresh_command_panel_layout(previous_scroll);
        state_ref.record_error(error);
    } else {
        schedule_command_panel_save(state);
    }
    if let Err(error) = sync_all_widgets(state) {
        state.borrow_mut().record_error(error);
    }
    state.borrow().drawing_area.queue_draw();
}

#[derive(Clone, Copy)]
enum TextInputValidation {
    None,
    CategoryName,
}

fn prompt_text(
    parent: &gtk::ApplicationWindow,
    title: &str,
    label: &str,
    initial: &str,
    validation: TextInputValidation,
) -> AppResult<Option<String>> {
    let dialog = gtk::Dialog::builder()
        .title(title)
        .modal(true)
        .transient_for(parent)
        .default_width(420)
        .default_height(140)
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("OK", gtk::ResponseType::Ok);
    dialog.set_default_response(gtk::ResponseType::Ok);

    let content = dialog.content_area();
    content.set_spacing(8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let text_label = gtk::Label::new(Some(label));
    text_label.set_xalign(0.0);
    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    {
        let dialog = dialog.clone();
        entry.connect_activate(move |_| {
            dialog.response(gtk::ResponseType::Ok);
        });
    }
    content.append(&text_label);
    content.append(&entry);

    loop {
        let response = run_dialog_blocking(&dialog);
        if response != gtk::ResponseType::Ok {
            dialog.close();
            return Ok(None);
        }

        let value = entry.text().to_string();
        match validate_text_input(&value, validation) {
            Ok(()) => {
                dialog.close();
                return Ok(Some(value));
            }
            Err(error) => show_error(parent, error.user_message()),
        }
    }
}

fn edit_command_button(
    parent: &gtk::ApplicationWindow,
    initial: &CommandButtonDefinition,
) -> AppResult<Option<CommandButtonDefinition>> {
    let dialog = gtk::Dialog::builder()
        .title("Edit Button")
        .modal(true)
        .transient_for(parent)
        .default_width(560)
        .default_height(320)
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button("Save", gtk::ResponseType::Ok);
    dialog.set_default_response(gtk::ResponseType::Ok);

    let content = dialog.content_area();
    content.set_spacing(8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let grid = gtk::Grid::new();
    grid.set_column_spacing(8);
    grid.set_row_spacing(8);
    content.append(&grid);

    let label_entry = gtk::Entry::new();
    label_entry.set_text(&initial.label);
    label_entry.set_activates_default(true);
    let executable_entry = gtk::Entry::new();
    executable_entry.set_text(&initial.executable_path);
    executable_entry.set_activates_default(true);
    let arguments_entry = gtk::Entry::new();
    arguments_entry.set_text(initial.arguments.value());
    arguments_entry.set_activates_default(true);
    for entry in [&label_entry, &executable_entry, &arguments_entry] {
        let dialog = dialog.clone();
        entry.connect_activate(move |_| {
            dialog.response(gtk::ResponseType::Ok);
        });
    }

    add_labeled_entry(&grid, 0, "Button Name", &label_entry);
    add_labeled_entry(&grid, 1, "Executable", &executable_entry);
    add_labeled_entry(&grid, 2, "Arguments", &arguments_entry);

    let browse_button = gtk::Button::with_label("Browse...");
    browse_button.set_focusable(false);
    {
        let parent = parent.clone();
        let executable_entry = executable_entry.clone();
        browse_button.connect_clicked(move |_| match select_file(&parent, "Select Executable") {
            Ok(Some(path)) => executable_entry.set_text(&path.to_string_lossy()),
            Ok(None) => {}
            Err(error) => show_error(&parent, error.user_message()),
        });
    }
    grid.attach(&browse_button, 2, 1, 1, 1);

    let token_label = gtk::Label::new(Some("Insert Token"));
    token_label.set_xalign(0.0);
    grid.attach(&token_label, 0, 3, 1, 1);
    let tokens = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    for token in [
        "{path}",
        "{name}",
        "{selectfile}",
        "{selectdir}",
        "{inputtext}",
    ] {
        let button = gtk::Button::with_label(token);
        button.set_focusable(false);
        button.set_tooltip_text(Some(token));
        button.set_css_classes(&["flat"]);
        let arguments_entry = arguments_entry.clone();
        button.connect_clicked(move |_| {
            let mut text = arguments_entry.text().to_string();
            if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                text.push(' ');
            }
            text.push_str(token);
            arguments_entry.set_text(&text);
            arguments_entry.grab_focus();
        });
        tokens.append(&button);
    }
    grid.attach(&tokens, 1, 3, 2, 1);

    let token_note = gtk::Label::new(Some(
        "{path}: current terminal path, {name}: last path segment",
    ));
    token_note.set_xalign(0.0);
    grid.attach(&token_note, 1, 4, 2, 1);

    loop {
        let response = run_dialog_blocking(&dialog);
        if response != gtk::ResponseType::Ok {
            dialog.close();
            return Ok(None);
        }

        let result =
            CommandArguments::new(arguments_entry.text().to_string()).and_then(|arguments| {
                CommandButtonDefinition::new(
                    label_entry.text().to_string(),
                    executable_entry.text().to_string(),
                    arguments,
                )
            });
        match result {
            Ok(definition) => {
                dialog.close();
                return Ok(Some(definition));
            }
            Err(error) => show_error(parent, error.user_message()),
        }
    }
}

fn add_labeled_entry(grid: &gtk::Grid, row: i32, label: &str, entry: &gtk::Entry) {
    let label = gtk::Label::new(Some(label));
    label.set_xalign(0.0);
    entry.set_hexpand(true);
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(entry, 1, row, 1, 1);
}

fn choose_terminal_font(
    parent: &gtk::ApplicationWindow,
    current: &TerminalFont,
) -> AppResult<Option<TerminalFont>> {
    let dialog = gtk::FontChooserDialog::new(Some("Font Settings"), Some(parent));
    dialog.set_modal(true);
    dialog.set_level(
        gtk::FontChooserLevel::FAMILY | gtk::FontChooserLevel::STYLE | gtk::FontChooserLevel::SIZE,
    );
    dialog.set_filter_func(|family, _face| is_terminal_font_family_candidate(family));
    dialog.set_font(&format!("{} {}", current.family(), current.size_points()));

    let response = run_dialog_blocking(dialog.upcast_ref());
    let result = if response == gtk::ResponseType::Ok {
        let description = dialog.font_desc().ok_or(AppError::InvalidInput(
            "selected font description is unavailable",
        ))?;
        Some(terminal_font_from_pango_description(&description)?)
    } else {
        None
    };
    dialog.close();
    Ok(result)
}

fn is_terminal_font_family_candidate(family: &gtk::pango::FontFamily) -> bool {
    if family.is_monospace() {
        return true;
    }

    let name = family.name();
    font_family_name_suggests_monospace(name.as_str())
}

fn font_family_name_suggests_monospace(name: &str) -> bool {
    name.split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case("mono") || part.eq_ignore_ascii_case("monospace"))
}

fn terminal_font_from_pango_description(
    description: &gtk::pango::FontDescription,
) -> AppResult<TerminalFont> {
    let family = description
        .family()
        .ok_or(AppError::InvalidInput(
            "selected font family is unavailable",
        ))?
        .to_string();
    let size = font_size_points_from_pango_size(description.size())?;
    TerminalFont::new(family, size)
}

fn font_size_points_from_pango_size(size: i32) -> AppResult<u16> {
    let rounded = size.saturating_add(gtk::pango::SCALE / 2) / gtk::pango::SCALE;
    let size = u16::try_from(rounded)
        .map_err(|_| AppError::InvalidInput("font size is outside supported range"))?;
    if !(MIN_FONT_SIZE_POINTS..=MAX_FONT_SIZE_POINTS).contains(&size) {
        return Err(AppError::InvalidInput(
            "font size must be between 6 and 72 points",
        ));
    }

    Ok(size)
}

fn select_file(parent: &gtk::ApplicationWindow, title: &str) -> AppResult<Option<PathBuf>> {
    select_file_with_action(parent, title, gtk::FileChooserAction::Open)
}

fn select_folder(parent: &gtk::ApplicationWindow) -> AppResult<Option<PathBuf>> {
    select_file_with_action(
        parent,
        "Select Folder",
        gtk::FileChooserAction::SelectFolder,
    )
}

fn select_file_with_action(
    parent: &gtk::ApplicationWindow,
    title: &str,
    action: gtk::FileChooserAction,
) -> AppResult<Option<PathBuf>> {
    let chooser = gtk::FileChooserDialog::new(
        Some(title),
        Some(parent),
        action,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Select", gtk::ResponseType::Accept),
        ],
    );
    chooser.set_modal(true);
    chooser.set_default_size(720, 480);
    let response = run_dialog_blocking(chooser.upcast_ref());
    let result = if response == gtk::ResponseType::Accept {
        chooser.file().and_then(|file| file.path())
    } else {
        None
    };
    chooser.close();
    Ok(result)
}

fn confirm(parent: &gtk::ApplicationWindow, title: &str, message: &str) -> bool {
    let dialog = gtk::Dialog::builder()
        .title(title)
        .modal(true)
        .transient_for(parent)
        .default_width(360)
        .default_height(120)
        .build();
    dialog.add_button("No", gtk::ResponseType::No);
    dialog.add_button("Yes", gtk::ResponseType::Yes);
    dialog.set_default_response(gtk::ResponseType::Yes);
    let content = dialog.content_area();
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&gtk::Label::new(Some(message)));
    let response = run_dialog_blocking(&dialog);
    dialog.close();
    response == gtk::ResponseType::Yes
}

fn validate_text_input(value: &str, validation: TextInputValidation) -> AppResult<()> {
    match validation {
        TextInputValidation::None => Ok(()),
        TextInputValidation::CategoryName => {
            crate::domain::CommandCategoryDefinition::new(value.to_owned(), Vec::new()).map(|_| ())
        }
    }
}

fn show_error(parent: &gtk::ApplicationWindow, message: &str) {
    let dialog = gtk::Dialog::builder()
        .title(APP_DISPLAY_NAME)
        .modal(true)
        .transient_for(parent)
        .default_width(360)
        .default_height(120)
        .build();
    dialog.add_button("OK", gtk::ResponseType::Ok);
    dialog.set_default_response(gtk::ResponseType::Ok);
    let content = dialog.content_area();
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&gtk::Label::new(Some(message)));
    let _ = run_dialog_blocking(&dialog);
    dialog.close();
}

fn show_about(parent: &gtk::ApplicationWindow) {
    let title = format!("About {APP_DISPLAY_NAME}");
    let version = format!("Version {APP_VERSION}");
    let dialog = gtk::Dialog::builder()
        .title(title.as_str())
        .modal(true)
        .transient_for(parent)
        .default_width(360)
        .default_height(160)
        .build();
    dialog.add_button("OK", gtk::ResponseType::Ok);
    dialog.set_default_response(gtk::ResponseType::Ok);

    let content = dialog.content_area();
    content.set_spacing(8);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let name = gtk::Label::new(Some(APP_DISPLAY_NAME));
    name.set_xalign(0.0);
    let version = gtk::Label::new(Some(&version));
    version.set_xalign(0.0);
    let link = gtk::LinkButton::with_label(AUTHOR_PROFILE_URL, AUTHOR_PROFILE_URL);
    link.set_halign(gtk::Align::Start);

    content.append(&name);
    content.append(&version);
    content.append(&link);

    let _ = run_dialog_blocking(&dialog);
    dialog.close();
}

fn run_dialog_blocking(dialog: &gtk::Dialog) -> gtk::ResponseType {
    let main_loop = glib::MainLoop::new(None, false);
    let response = Rc::new(Cell::new(gtk::ResponseType::None));
    {
        let main_loop = main_loop.clone();
        let response = Rc::clone(&response);
        dialog.connect_response(move |dialog, next_response| {
            response.set(next_response);
            dialog.hide();
            main_loop.quit();
        });
    }
    dialog.set_visible(true);
    dialog.present();
    main_loop.run();
    response.get()
}

fn drain_pty_and_redraw(state: &SharedState) -> Option<TerminalTimerDrain> {
    let result = (|| -> AppResult<TerminalTimerDrain> {
        let mut state_ref = state.borrow_mut();
        let drain = state_ref.session.drain_timer_events()?;
        if let Some(cause) = drain.failure_cause.as_ref() {
            state_ref.last_error = Some(cause.clone());
        }
        if drain.active_tab_dirty {
            let selection_cleared = state_ref.clear_terminal_selection();
            let invalidation = state_ref.refresh_terminal_viewport_invalidation()?;
            if selection_cleared {
                state_ref.invalidate_terminal_paint(TerminalViewportInvalidation::Full);
            } else {
                state_ref.invalidate_terminal_paint(invalidation);
            }
        }
        Ok(drain)
    })();

    match result {
        Ok(drain) => {
            if drain.active_tab_dirty {
                if let Err(error) = sync_terminal_scrollbar(state) {
                    state.borrow_mut().record_error(error);
                }
                state.borrow().drawing_area.queue_draw();
            }
            Some(drain)
        }
        Err(error) => {
            state.borrow_mut().record_error(error);
            None
        }
    }
}

fn draw_scene(state: &mut GtkWindowState, context: &gtk::cairo::Context, width: i32, height: i32) {
    set_color(context, CHROME_BACKGROUND);
    context.rectangle(0.0, 0.0, width as f64, height as f64);
    let _ = context.fill();

    draw_tab_bar(state, context);
    draw_splitter(state, context);
    draw_terminal(state, context);
}

fn draw_tab_bar(state: &GtkWindowState, context: &gtk::cairo::Context) {
    for tab in &state.layout.tabs {
        set_color(
            context,
            if tab.active {
                TAB_ACTIVE_BACKGROUND
            } else {
                TAB_INACTIVE_BACKGROUND
            },
        );
        fill_rect(context, tab.bounds);
        set_color(
            context,
            if tab.active {
                FOREGROUND
            } else {
                TAB_MUTED_FOREGROUND
            },
        );
        draw_text(
            context,
            tab.bounds.x.saturating_add(8),
            tab.bounds.y.saturating_add(19),
            &tab_title(&state.tab_views, tab.id),
        );
        if let Some(close) = tab.close_bounds {
            draw_text(
                context,
                close.x.saturating_add(4),
                close.y.saturating_add(13),
                "x",
            );
        }
    }

    if let Some(bounds) = state.layout.new_tab_button {
        set_color(context, TAB_INACTIVE_BACKGROUND);
        fill_rect(context, bounds);
        set_color(context, FOREGROUND);
        draw_text(
            context,
            bounds.x.saturating_add(8),
            bounds.y.saturating_add(19),
            "+",
        );
    }
}

fn draw_splitter(state: &GtkWindowState, context: &gtk::cairo::Context) {
    set_color(context, SPLITTER_BACKGROUND);
    fill_rect(context, state.layout.splitter);
    let grip_x = state
        .layout
        .splitter
        .x
        .saturating_add(state.layout.splitter.width / 2);
    set_color(context, SPLITTER_GRIP);
    context.rectangle(
        grip_x as f64,
        state.layout.splitter.y.saturating_add(12) as f64,
        1.0,
        state.layout.splitter.height.saturating_sub(24).max(1) as f64,
    );
    let _ = context.fill();
}

fn draw_terminal(state: &mut GtkWindowState, context: &gtk::cairo::Context) {
    set_color(context, BACKGROUND);
    fill_rect(context, state.layout.terminal);

    let Some(viewport) = state.terminal_viewport.as_ref() else {
        return;
    };

    let terminal_font = state.session.terminal_font().clone();
    select_terminal_font(context, &terminal_font);

    let content = terminal_content_area(state.layout.terminal);
    let invalidation = std::mem::take(&mut state.pending_terminal_paint);
    if state.terminal_surface_cache.paint(
        context,
        viewport,
        content,
        TerminalFontRenderState {
            metrics: state.metrics,
            font: &terminal_font,
        },
        &mut state.terminal_line_text_cache,
        invalidation,
    ) {
        if let Some(selection) = state.terminal_selection {
            draw_terminal_selection_overlay(
                context,
                viewport,
                selection,
                state.metrics,
                content,
                &mut state.terminal_line_text_cache,
            );
        }
        draw_terminal_cursor(context, viewport, content, state.metrics);
        return;
    }
    state.pending_terminal_paint = TerminalViewportInvalidation::Full;

    draw_terminal_rows(
        context,
        viewport,
        state.terminal_selection,
        state.metrics,
        content,
        &mut state.terminal_line_text_cache,
        0..viewport.rows,
    );
    draw_terminal_cursor(context, viewport, content, state.metrics);
}

fn draw_terminal_selection_overlay(
    context: &gtk::cairo::Context,
    viewport: &TerminalViewport,
    selection: TerminalSelection,
    metrics: CellMetrics,
    content: UiRect,
    line_text_cache: &mut TerminalLineTextCache,
) {
    for row in 0..viewport.rows {
        let Some(range) = selection.row_range(row, viewport.columns) else {
            continue;
        };

        set_color(context, SELECTION_BACKGROUND);
        let x = content
            .x
            .saturating_add(i32_from_usize_saturating(range.start).saturating_mul(metrics.width));
        let y = content
            .y
            .saturating_add(i32_from_usize_saturating(row).saturating_mul(metrics.height));
        let width = i32_from_usize_saturating(range.end.saturating_sub(range.start))
            .saturating_mul(metrics.width);
        let selection_rect = UiRect {
            x,
            y,
            width,
            height: metrics.height,
        };
        fill_rect(context, selection_rect);

        if let Some(line) = line_text_cache.line_text(viewport, row)
            && context.save().is_ok()
        {
            context.rectangle(
                selection_rect.x as f64,
                selection_rect.y as f64,
                selection_rect.width.max(1) as f64,
                selection_rect.height.max(1) as f64,
            );
            context.clip();
            set_color(context, FOREGROUND);
            draw_text(
                context,
                content.x,
                selection_rect.y.saturating_add(metrics.baseline as i32),
                line,
            );
            let _ = context.restore();
        }
    }
}

fn draw_terminal_rows(
    context: &gtk::cairo::Context,
    viewport: &TerminalViewport,
    selection: Option<TerminalSelection>,
    metrics: CellMetrics,
    content: UiRect,
    line_text_cache: &mut TerminalLineTextCache,
    rows: std::ops::Range<usize>,
) {
    for row in rows {
        if row >= viewport.rows {
            break;
        }
        if let Some(selection) = selection
            && let Some(range) = selection.row_range(row, viewport.columns)
        {
            set_color(context, SELECTION_BACKGROUND);
            let x = content.x.saturating_add(
                i32_from_usize_saturating(range.start).saturating_mul(metrics.width),
            );
            let width = i32_from_usize_saturating(range.end.saturating_sub(range.start))
                .saturating_mul(metrics.width);
            fill_rect(
                context,
                UiRect {
                    x,
                    y: content.y.saturating_add(
                        i32_from_usize_saturating(row).saturating_mul(metrics.height),
                    ),
                    width,
                    height: metrics.height,
                },
            );
        }

        if let Some(line) = line_text_cache.line_text(viewport, row) {
            set_color(context, FOREGROUND);
            draw_text(
                context,
                content.x,
                content
                    .y
                    .saturating_add(i32_from_usize_saturating(row).saturating_mul(metrics.height))
                    .saturating_add(metrics.baseline as i32),
                line,
            );
        }
    }
}

fn draw_terminal_cursor(
    context: &gtk::cairo::Context,
    viewport: &TerminalViewport,
    content: UiRect,
    metrics: CellMetrics,
) {
    if viewport.cursor.row < viewport.rows && viewport.cursor.column < viewport.columns {
        let cursor = UiRect {
            x: content.x.saturating_add(
                i32_from_usize_saturating(viewport.cursor.column).saturating_mul(metrics.width),
            ),
            y: content.y.saturating_add(
                i32_from_usize_saturating(viewport.cursor.row).saturating_mul(metrics.height),
            ),
            width: metrics.width,
            height: metrics.height,
        };
        set_color(context, FOREGROUND);
        context.rectangle(
            cursor.x as f64,
            cursor.y as f64,
            cursor.width.max(1) as f64,
            cursor.height.max(1) as f64,
        );
        let _ = context.stroke();
    }
}

fn write_line_text(buffer: &mut String, cells: &[TerminalCell]) -> bool {
    buffer.clear();
    let Some(end) = cells
        .iter()
        .rposition(|cell| !cell.character.is_whitespace())
        .map(|index| index + 1)
    else {
        return false;
    };
    buffer.extend(cells[..end].iter().map(|cell| cell.character));
    true
}

fn tab_title(tabs: &[TerminalTabView], id: TerminalTabId) -> String {
    tabs.iter()
        .find(|tab| tab.id == id)
        .map(|tab| tab.title.clone())
        .unwrap_or_else(|| "Tab".to_owned())
}

fn set_color(context: &gtk::cairo::Context, color: Color) {
    context.set_source_rgb(color.red, color.green, color.blue);
}

fn fill_rect(context: &gtk::cairo::Context, rect: UiRect) {
    if rect.width <= 0 || rect.height <= 0 {
        return;
    }
    context.rectangle(
        rect.x as f64,
        rect.y as f64,
        rect.width as f64,
        rect.height as f64,
    );
    let _ = context.fill();
}

fn draw_text(context: &gtk::cairo::Context, x: i32, y: i32, text: &str) {
    context.move_to(x as f64, y as f64);
    let _ = context.show_text(text);
}

fn select_terminal_font(context: &gtk::cairo::Context, font: &TerminalFont) {
    context.select_font_face(
        font.family(),
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    context.set_font_size(f64::from(font.size_points()));
}

fn measure_cell_metrics(
    context: &gtk::cairo::Context,
    terminal_font: &TerminalFont,
) -> Option<CellMetrics> {
    select_terminal_font(context, terminal_font);
    let font = context.font_extents().ok()?;
    let text = context.text_extents("M").ok()?;
    Some(CellMetrics {
        width: f64_to_i32_cell_extent(text.x_advance()),
        height: f64_to_i32_cell_extent(font.height()),
        baseline: font.ascent().ceil().max(1.0),
    })
}

fn f64_to_i32_cell_extent(value: f64) -> i32 {
    if !value.is_finite() || value <= 1.0 {
        1
    } else if value >= i32::MAX as f64 {
        i32::MAX
    } else {
        value.ceil() as i32
    }
}

fn controller_event_point(controller: &gtk::EventControllerScroll) -> Option<UiPoint> {
    controller
        .current_event()
        .and_then(|event| event.position())
        .map(|(x, y)| ui_point(x, y))
}

fn relayout_command_panel(
    layout: &mut WindowLayout,
    command_button_scroll_position: &mut usize,
    command_panel_width: i32,
    command_panel: &CommandPanel,
    tabs: &[TerminalTabView],
    requested_scroll_position: usize,
) {
    let (width, height) = layout_client_size(layout);
    *layout = layout_from_client_size(
        width,
        height,
        command_panel_width,
        command_panel.selected_buttons(),
        tabs,
        requested_scroll_position,
    );
    *command_button_scroll_position = layout.command_button_scroll_position();
}

fn layout_client_size(layout: &WindowLayout) -> (i32, i32) {
    (
        layout.tab_bar.width.max(1),
        layout
            .tab_bar
            .height
            .saturating_add(layout.terminal.height)
            .max(1),
    )
}

fn command_button_scroll_lines_from_delta(dy: f64) -> i32 {
    const LINES_PER_WHEEL_STEP: i32 = 3;

    if dy > 0.0 {
        LINES_PER_WHEEL_STEP
    } else if dy < 0.0 {
        -LINES_PER_WHEEL_STEP
    } else {
        0
    }
}

fn terminal_scroll_lines_from_delta(dy: f64) -> i32 {
    command_button_scroll_lines_from_delta(dy).saturating_neg()
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

fn layout_from_client_size(
    width: i32,
    height: i32,
    command_panel_width: i32,
    buttons: &[CommandButton],
    tabs: &[TerminalTabView],
    command_button_scroll_position: usize,
) -> WindowLayout {
    WindowLayout::for_client_with_command_panel_width_and_button_scroll(
        width.max(1),
        height.max(1),
        command_panel_width,
        buttons,
        tabs,
        command_button_scroll_position,
    )
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
    rows: std::ops::Range<usize>,
) -> Option<UiRect> {
    if rows.start >= rows.end || content_area.width <= 0 || content_area.height <= 0 {
        return None;
    }

    let cell_height = metrics.height.max(1);
    let y_offset = i32_from_usize_saturating(rows.start).saturating_mul(cell_height);
    if y_offset >= content_area.height {
        return None;
    }

    let row_count = rows.end.saturating_sub(rows.start);
    let height = i32_from_usize_saturating(row_count)
        .saturating_mul(cell_height)
        .min(content_area.height.saturating_sub(y_offset));
    if height <= 0 {
        return None;
    }

    Some(UiRect {
        x: content_area.x,
        y: content_area.y.saturating_add(y_offset),
        width: content_area.width,
        height,
    })
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

fn ui_point(x: f64, y: f64) -> UiPoint {
    UiPoint {
        x: x.round() as i32,
        y: y.round() as i32,
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

fn usize_from_u32_saturating(value: u32) -> usize {
    match usize::try_from(value) {
        Ok(value) => value,
        Err(_) => usize::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CommandArguments, CommandButtonDefinition, CommandCategoryDefinition, CursorPosition,
        TerminalTabId,
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn command_panel_relayout_uses_selected_category_and_resets_scroll() -> AppResult<()> {
        let mut panel = CommandPanel::from_definitions(
            vec![
                CommandCategoryDefinition::new("Default", button_definitions("default", 8)?)?,
                CommandCategoryDefinition::new(
                    "Tools",
                    vec![CommandButtonDefinition::new(
                        "deploy",
                        "echo",
                        CommandArguments::new("deploy")?,
                    )?],
                )?,
            ],
            0,
        )?;
        let tabs = single_tab_views();
        let mut layout = layout_from_client_size(
            900,
            200,
            COMMAND_PANEL_WIDTH,
            panel.selected_buttons(),
            &tabs,
            3,
        );
        let mut scroll_position = layout.command_button_scroll_position();
        assert_eq!(scroll_position, 3);

        panel.select_category_by_index(1)?;
        relayout_command_panel(
            &mut layout,
            &mut scroll_position,
            COMMAND_PANEL_WIDTH,
            &panel,
            &tabs,
            0,
        );

        assert_eq!(scroll_position, 0);
        assert_eq!(layout.buttons.len(), 1);
        assert_eq!(layout.buttons[0].label, "deploy");
        Ok(())
    }

    #[test]
    fn gtk_scroll_deltas_match_windows_scroll_direction() {
        assert_eq!(command_button_scroll_lines_from_delta(1.0), 3);
        assert_eq!(command_button_scroll_lines_from_delta(-1.0), -3);
        assert_eq!(terminal_scroll_lines_from_delta(1.0), -3);
        assert_eq!(terminal_scroll_lines_from_delta(-1.0), 3);
        assert_eq!(scrolled_position_by_lines(4, 10, -3), 1);
        assert_eq!(scrolled_position_by_lines(9, 10, 3), 10);
    }

    #[test]
    fn pty_drain_timer_backs_off_after_idle_drains_and_resets_on_activity() {
        let mut timer = PtyDrainTimerState::default();
        let idle = TerminalTimerDrain::default();

        assert_eq!(
            timer.interval_after_drain(Some(&idle)),
            active_pty_timer_interval()
        );
        assert_eq!(
            timer.interval_after_drain(Some(&idle)),
            active_pty_timer_interval()
        );
        assert_eq!(
            timer.interval_after_drain(Some(&idle)),
            Duration::from_millis(PTY_IDLE_TIMER_MS)
        );

        timer.record_activity();
        assert_eq!(
            timer.interval_after_drain(Some(&idle)),
            active_pty_timer_interval()
        );

        let busy = TerminalTimerDrain {
            had_events: true,
            ..TerminalTimerDrain::default()
        };
        assert_eq!(
            timer.interval_after_drain(Some(&busy)),
            active_pty_timer_interval()
        );

        let cleanup_pending = TerminalTimerDrain {
            needs_active_poll: true,
            ..TerminalTimerDrain::default()
        };
        assert_eq!(
            timer.interval_after_drain(Some(&cleanup_pending)),
            active_pty_timer_interval()
        );
    }

    #[test]
    fn pty_drain_timer_generation_invalidates_previous_timeout() {
        let mut timer = PtyDrainTimerState::default();

        let first = timer.schedule_next();
        assert!(timer.is_current(first));

        let second = timer.schedule_next();
        assert!(!timer.is_current(first));
        assert!(timer.is_current(second));
    }

    #[test]
    fn command_panel_save_generation_invalidates_previous_timeout() {
        let mut save = CommandPanelSaveState::default();

        let first = save.schedule_next();
        assert!(save.is_current(first));

        let second = save.schedule_next();
        assert!(!save.is_current(first));
        assert!(save.is_current(second));

        save.mark_saved(first);
        assert!(save.is_current(second));

        save.mark_saved(second);
        assert!(!save.is_current(second));
    }

    #[test]
    fn command_panel_save_shutdown_wait_retains_delayed_task() -> AppResult<()> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let task = CommandPanelSaveTask {
            generation: 1,
            receiver,
        };

        let wait = task.wait_before_shutdown(Duration::ZERO)?;

        let CommandPanelSaveShutdownWait::Delayed(task) = wait else {
            panic!("expected delayed command panel save task");
        };

        let mut save = CommandPanelSaveState::default();
        save.restore_save_in_progress(task);
        assert!(save.save_in_progress());
        assert!(sender.send(Ok(())).is_ok());
        Ok(())
    }

    #[test]
    fn gtk_exit_cleanup_reports_timeout_without_waiting_for_finish() {
        let timeout_error = AppError::pty_message(
            "join detached pty cleanup tasks",
            "timed out waiting for detached cleanup",
        );
        let cleanup_error = AppError::pty_message(
            "cleanup detached pty resources",
            "cleanup failed after shutdown",
        );
        let mut reported_timeouts = 0;

        let errors = collect_detached_cleanup_errors_after_gtk_exit(
            vec![timeout_error, cleanup_error],
            |_| reported_timeouts += 1,
        );

        assert_eq!(reported_timeouts, 1);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].operation(),
            Some("cleanup detached pty resources")
        );
    }

    #[test]
    fn category_selection_change_does_not_write_settings_file_until_explicit_save() -> AppResult<()>
    {
        let settings_path = unique_test_settings_path("deferred-category-selection")?;
        let mut session = GtkSessionCoordinator::new_for_test(
            TerminalSize::new(4, 16)?,
            two_category_command_panel()?,
            ConfigStore::for_test_path(settings_path.clone()),
        );

        session.select_command_category_by_index(1)?;

        assert!(
            !settings_path
                .try_exists()
                .map_err(|source| AppError::io("check test settings file", source))?
        );

        session.save_command_panel()?;
        let loaded_panel = ConfigStore::for_test_path(settings_path.clone()).load_or_default()?;

        assert_eq!(loaded_panel.selected_category_index(), Some(1));

        let _ = fs::remove_file(&settings_path);
        Ok(())
    }

    #[test]
    fn shutdown_skips_command_panel_save_when_clean() -> AppResult<()> {
        let settings_path = unique_test_settings_path("clean-shutdown")?;
        let mut session = GtkSessionCoordinator::new_for_test(
            TerminalSize::new(4, 16)?,
            two_category_command_panel()?,
            ConfigStore::for_test_path(settings_path.clone()),
        );

        session.shutdown()?;

        assert!(
            !settings_path
                .try_exists()
                .map_err(|source| AppError::io("check test settings file", source))?
        );
        Ok(())
    }

    #[test]
    fn shutdown_saves_dirty_command_panel() -> AppResult<()> {
        let settings_path = unique_test_settings_path("dirty-shutdown")?;
        let mut session = GtkSessionCoordinator::new_for_test(
            TerminalSize::new(4, 16)?,
            two_category_command_panel()?,
            ConfigStore::for_test_path(settings_path.clone()),
        );

        session.select_command_category_by_index(1)?;
        session.shutdown()?;

        let loaded_panel = ConfigStore::for_test_path(settings_path.clone()).load_or_default()?;
        assert_eq!(loaded_panel.selected_category_index(), Some(1));

        let _ = fs::remove_file(&settings_path);
        Ok(())
    }

    #[test]
    fn write_line_text_reuses_buffer_and_trims_trailing_whitespace() {
        let mut buffer = String::with_capacity(16);
        let capacity = buffer.capacity();
        let cells = [
            TerminalCell::new('a'),
            TerminalCell::new(' '),
            TerminalCell::new('한'),
            TerminalCell::new('\t'),
            TerminalCell::new(' '),
        ];

        assert!(write_line_text(&mut buffer, &cells));
        assert_eq!(buffer, "a 한");
        assert_eq!(buffer.capacity(), capacity);

        let blank_cells = [TerminalCell::new(' '), TerminalCell::new('\t')];
        assert!(!write_line_text(&mut buffer, &blank_cells));
        assert_eq!(buffer, "");
        assert_eq!(buffer.capacity(), capacity);
    }

    #[test]
    fn terminal_line_text_cache_uses_row_versions_for_invalidations() -> AppResult<()> {
        let mut viewport = terminal_viewport_from_rows(&["old "])?;
        let mut cache = TerminalLineTextCache::default();

        assert_eq!(cache.line_text(&viewport, 0), Some("old"));
        viewport.cells[0] = TerminalCell::new('n');
        viewport.cells[1] = TerminalCell::new('e');
        viewport.cells[2] = TerminalCell::new('w');
        assert_eq!(cache.line_text(&viewport, 0), Some("old"));

        viewport.set_changed_rows(Some(0..1));
        assert_eq!(cache.line_text(&viewport, 0), Some("new"));
        Ok(())
    }

    #[test]
    fn terminal_line_text_cache_clear_invalidates_same_shape_viewport() -> AppResult<()> {
        let first = terminal_viewport_from_rows(&["one "])?;
        let second = terminal_viewport_from_rows(&["two "])?;
        let mut cache = TerminalLineTextCache::default();

        assert_eq!(cache.line_text(&first, 0), Some("one"));
        assert_eq!(cache.line_text(&second, 0), Some("one"));

        cache.clear();
        assert_eq!(cache.line_text(&second, 0), Some("two"));
        Ok(())
    }

    #[test]
    fn terminal_rows_rect_returns_dirty_row_band() {
        let content_area = UiRect {
            x: 10,
            y: 20,
            width: 200,
            height: 80,
        };
        let metrics = CellMetrics {
            width: 8,
            height: 16,
            baseline: 12.0,
        };

        let Some(rect) = terminal_rows_rect(content_area, metrics, 1..3) else {
            panic!("expected dirty row rectangle");
        };

        assert_eq!(
            rect,
            UiRect {
                x: 10,
                y: 36,
                width: 200,
                height: 32,
            }
        );
    }

    #[test]
    fn terminal_rows_rect_clips_to_content_height() {
        let content_area = UiRect {
            x: 0,
            y: 0,
            width: 160,
            height: 40,
        };
        let metrics = CellMetrics {
            width: 8,
            height: 16,
            baseline: 12.0,
        };

        let Some(rect) = terminal_rows_rect(content_area, metrics, 1..4) else {
            panic!("expected clipped dirty row rectangle");
        };

        assert_eq!(
            rect,
            UiRect {
                x: 0,
                y: 16,
                width: 160,
                height: 24,
            }
        );
    }

    #[test]
    fn terminal_viewport_invalidation_merges_dirty_rows() -> AppResult<()> {
        let mut first = terminal_viewport_from_rows(&["one", "two", "three"])?;
        let baseline = first.change_baseline();
        first.set_changed_rows(Some(0..1));
        let mut invalidation = TerminalViewportInvalidation::Rows(
            first
                .changed_rows_since_baseline(&baseline)
                .ok_or(AppError::InvalidInput("expected changed rows"))?,
        );

        let mut second = terminal_viewport_from_rows(&["one", "two", "three"])?;
        let baseline = second.change_baseline();
        second.set_changed_rows(Some(2..3));
        let rows = second
            .changed_rows_since_baseline(&baseline)
            .ok_or(AppError::InvalidInput("expected changed rows"))?;

        invalidation.merge(TerminalViewportInvalidation::Rows(rows));

        let TerminalViewportInvalidation::Rows(rows) = invalidation else {
            return Err(AppError::InvalidInput("expected row invalidation"));
        };
        assert_eq!(changed_row_ranges(Some(rows)), Some(vec![(0, 1), (2, 3)]));
        Ok(())
    }

    #[test]
    fn category_name_validation_matches_command_panel_rules() {
        assert!(validate_text_input("Build", TextInputValidation::CategoryName).is_ok());
        assert!(validate_text_input("", TextInputValidation::CategoryName).is_err());
        assert!(validate_text_input("bad\nname", TextInputValidation::CategoryName).is_err());
    }

    #[test]
    fn font_family_name_suggests_monospace_for_cjk_mono_family() {
        assert!(font_family_name_suggests_monospace("Noto Sans Mono CJK KR"));
        assert!(font_family_name_suggests_monospace("Noto Mono"));
        assert!(font_family_name_suggests_monospace("DejaVu Sans Mono"));
        assert!(!font_family_name_suggests_monospace("Noto Sans CJK KR"));
        assert!(!font_family_name_suggests_monospace("Noto Serif CJK KR"));
        assert!(!font_family_name_suggests_monospace("Monaco"));
    }

    fn single_tab_views() -> Vec<TerminalTabView> {
        vec![TerminalTabView {
            id: TerminalTabId::new(1),
            title: "cmd".to_owned(),
            active: true,
        }]
    }

    fn terminal_viewport_from_rows(rows: &[&str]) -> AppResult<TerminalViewport> {
        let columns = rows
            .iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0);
        let mut cells = Vec::with_capacity(rows.len().saturating_mul(columns));
        for row in rows {
            cells.extend(row.chars().map(TerminalCell::new));
            let padding = columns.saturating_sub(row.chars().count());
            cells.extend(std::iter::repeat_n(TerminalCell::new(' '), padding));
        }
        TerminalViewport::new(rows.len(), columns, cells, CursorPosition::new(0, 0))
    }

    fn changed_row_ranges(rows: Option<TerminalChangedRows>) -> Option<Vec<(usize, usize)>> {
        rows.map(|rows| {
            rows.ranges()
                .iter()
                .map(|rows| (rows.start, rows.end))
                .collect()
        })
    }

    fn button_definitions(prefix: &str, count: usize) -> AppResult<Vec<CommandButtonDefinition>> {
        let mut definitions = Vec::with_capacity(count);
        for index in 0..count {
            definitions.push(CommandButtonDefinition::new(
                format!("{prefix}-{index}"),
                "echo",
                CommandArguments::new(format!("{prefix}-{index}"))?,
            )?);
        }
        Ok(definitions)
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

    fn unique_test_settings_path(name: &str) -> AppResult<PathBuf> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|source| AppError::ui_message("resolve test timestamp", source.to_string()))?
            .as_nanos();
        Ok(std::env::temp_dir().join(format!(
            "j3term-gtk-{name}-{}-{timestamp}.toml",
            std::process::id()
        )))
    }
}
