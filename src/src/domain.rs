pub mod appearance;
pub mod command;
pub mod identity;
pub mod input;
pub mod layout;
pub mod session;
pub mod terminal;

pub use appearance::{MAX_FONT_SIZE_POINTS, MIN_FONT_SIZE_POINTS, TerminalFont};
#[cfg(all(test, target_os = "windows"))]
pub use command::default_command_panel;
pub use command::{
    ButtonArgumentValues, CommandArguments, CommandButton, CommandButtonDefinition,
    CommandButtonId, CommandCategory, CommandCategoryDefinition, CommandPanel, CommandText,
    ShellCommandDialect, StartupCommand, StartupDirectory, StartupInvocation,
    default_platform_command_panel,
};
pub use identity::{APP_DISPLAY_NAME, APP_VERSION, AUTHOR_PROFILE_URL};
#[cfg(target_os = "linux")]
pub use identity::{APP_NAME, LINUX_APPLICATION_ID};
#[cfg(any(test, target_os = "windows"))]
pub use input::terminal_input_from_char;
pub use input::{
    TerminalInput, TerminalInputBytes, TerminalKey, TerminalKeyModifiers, terminal_input_from_key,
    terminal_paste_text_to_pty_bytes,
};
pub use layout::{UiPoint, UiRect, WindowLayout};
pub use session::{SessionStatus, TerminalEvent, TerminalFailure, session_status_after_event};
pub use terminal::{
    CursorPosition, DEFAULT_COLUMNS, DEFAULT_ROWS, MAX_TERMINAL_TABS, MIN_COLUMNS, MIN_ROWS,
    TerminalCell, TerminalCommand, TerminalGridPoint, TerminalScroll, TerminalScrollState,
    TerminalSelection, TerminalSize, TerminalTabId, TerminalTabView, TerminalViewport,
};
