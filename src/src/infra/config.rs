use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{
    CommandArguments, CommandButtonDefinition, CommandCategoryDefinition, CommandPanel,
    TerminalFont, default_platform_command_panel,
};
use crate::error::{AppError, AppResult};

const CONFIG_VERSION: u32 = 1;
const TEMP_FILE_ATTEMPTS: u32 = 100;

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    #[link_name = "ReplaceFileW"]
    fn replace_file_w(
        _: *const u16,
        _: *const u16,
        _: *const u16,
        _: u32,
        _: *mut std::ffi::c_void,
        _: *mut std::ffi::c_void,
    ) -> i32;
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
    legacy_load_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AppSettings {
    pub command_panel: CommandPanel,
    pub terminal_font: TerminalFont,
}

impl ConfigStore {
    pub fn from_current_exe() -> AppResult<Self> {
        let executable_path =
            env::current_exe().map_err(|source| AppError::io("resolve executable path", source))?;
        let path = default_settings_path(&executable_path)?;
        let legacy_load_path = legacy_settings_path_for_executable(&executable_path)?;
        let legacy_load_path = if legacy_load_path == path {
            None
        } else {
            Some(legacy_load_path)
        };
        Ok(Self {
            path,
            legacy_load_path,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test_path(path: PathBuf) -> Self {
        Self {
            path,
            legacy_load_path: None,
        }
    }

    #[cfg(test)]
    pub fn load_or_default(&self) -> AppResult<CommandPanel> {
        self.load_settings_or_default()
            .map(|settings| settings.command_panel)
    }

    pub fn load_settings_or_default(&self) -> AppResult<AppSettings> {
        let Some(path) = self.existing_settings_file_path()? else {
            return Ok(self.load_default_settings());
        };

        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(self.load_default_settings());
            }
            Err(source) => return Err(AppError::io("read settings file", source)),
        };

        let config: ConfigFile = toml::from_str(&content)
            .map_err(|source| AppError::ui_message("parse settings file", source.to_string()))?;
        config.into_app_settings()
    }

    #[cfg(test)]
    pub fn save_command_panel(&self, command_panel: &CommandPanel) -> AppResult<()> {
        self.prepare_command_panel_save(command_panel).save()
    }

    pub fn save_settings(&self, settings: &AppSettings) -> AppResult<()> {
        self.prepare_settings_save(settings).save()
    }

    #[cfg(test)]
    pub fn prepare_command_panel_save(
        &self,
        command_panel: &CommandPanel,
    ) -> CommandPanelSaveRequest {
        let config = ConfigFile::from_command_panel(command_panel);
        CommandPanelSaveRequest {
            path: self.path.clone(),
            config,
        }
    }

    pub fn prepare_settings_save(&self, settings: &AppSettings) -> CommandPanelSaveRequest {
        let config = ConfigFile::from_app_settings(settings);
        CommandPanelSaveRequest {
            path: self.path.clone(),
            config,
        }
    }

    fn existing_settings_file_path(&self) -> AppResult<Option<&Path>> {
        if self
            .path
            .try_exists()
            .map_err(|source| AppError::io("check settings file", source))?
        {
            return Ok(Some(&self.path));
        }

        if let Some(path) = &self.legacy_load_path
            && path
                .try_exists()
                .map_err(|source| AppError::io("check legacy settings file", source))?
        {
            return Ok(Some(path));
        }

        Ok(None)
    }

    fn load_default_settings(&self) -> AppSettings {
        AppSettings {
            command_panel: default_platform_command_panel(),
            terminal_font: TerminalFont::default(),
        }
    }
}

pub struct CommandPanelSaveRequest {
    path: PathBuf,
    config: ConfigFile,
}

impl CommandPanelSaveRequest {
    pub fn save(self) -> AppResult<()> {
        let content = toml::to_string_pretty(&self.config).map_err(|source| {
            AppError::ui_message("serialize settings file", source.to_string())
        })?;

        write_settings_file_atomically(&self.path, content.as_bytes())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ConfigFile {
    #[serde(default = "config_version")]
    version: u32,
    #[serde(default)]
    selected_category: usize,
    #[serde(default)]
    categories: Vec<ConfigCategory>,
    #[serde(default)]
    font: ConfigFont,
}

impl ConfigFile {
    #[cfg(test)]
    fn from_command_panel(command_panel: &CommandPanel) -> Self {
        Self::from_parts(command_panel, &TerminalFont::default())
    }

    fn from_app_settings(settings: &AppSettings) -> Self {
        Self::from_parts(&settings.command_panel, &settings.terminal_font)
    }

    fn from_parts(command_panel: &CommandPanel, terminal_font: &TerminalFont) -> Self {
        Self {
            version: CONFIG_VERSION,
            selected_category: command_panel.selected_category_index().unwrap_or(0),
            categories: command_panel
                .categories()
                .iter()
                .map(ConfigCategory::from_command_category)
                .collect(),
            font: ConfigFont::from_terminal_font(terminal_font),
        }
    }

    #[cfg(test)]
    fn into_command_panel(self) -> AppResult<CommandPanel> {
        self.into_app_settings()
            .map(|settings| settings.command_panel)
    }

    fn into_app_settings(self) -> AppResult<AppSettings> {
        if self.version != CONFIG_VERSION {
            return Err(AppError::InvalidInput("unsupported settings file version"));
        }

        let categories = self
            .categories
            .into_iter()
            .map(ConfigCategory::into_definition)
            .collect::<AppResult<Vec<_>>>()?;

        Ok(AppSettings {
            command_panel: CommandPanel::from_definitions(categories, self.selected_category)?,
            terminal_font: self.font.into_terminal_font()?,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ConfigFont {
    #[serde(default = "default_font_family")]
    family: String,
    #[serde(default = "default_font_size_points")]
    size: u16,
}

impl ConfigFont {
    fn from_terminal_font(font: &TerminalFont) -> Self {
        Self {
            family: font.family().to_owned(),
            size: font.size_points(),
        }
    }

    fn into_terminal_font(self) -> AppResult<TerminalFont> {
        TerminalFont::new(self.family, self.size)
    }
}

impl Default for ConfigFont {
    fn default() -> Self {
        Self::from_terminal_font(&TerminalFont::default())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ConfigCategory {
    name: String,
    #[serde(default)]
    buttons: Vec<ConfigButton>,
}

impl ConfigCategory {
    fn from_command_category(category: &crate::domain::CommandCategory) -> Self {
        Self {
            name: category.name.clone(),
            buttons: category
                .buttons
                .iter()
                .map(ConfigButton::from_command_button)
                .collect(),
        }
    }

    fn into_definition(self) -> AppResult<CommandCategoryDefinition> {
        let buttons = self
            .buttons
            .into_iter()
            .map(ConfigButton::into_definition)
            .collect::<AppResult<Vec<_>>>()?;

        CommandCategoryDefinition::new(self.name, buttons)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ConfigButton {
    label: String,
    executable_path: String,
    #[serde(default)]
    arguments: String,
}

impl ConfigButton {
    fn from_command_button(button: &crate::domain::CommandButton) -> Self {
        Self {
            label: button.label.clone(),
            executable_path: button.executable_path.clone(),
            arguments: button.arguments.value().to_owned(),
        }
    }

    fn into_definition(self) -> AppResult<CommandButtonDefinition> {
        CommandButtonDefinition::new(
            self.label,
            self.executable_path,
            CommandArguments::new(self.arguments)?,
        )
    }
}

fn default_settings_path(executable_path: &Path) -> AppResult<PathBuf> {
    executable_settings_path(executable_path)
}

fn legacy_settings_path_for_executable(executable_path: &Path) -> AppResult<PathBuf> {
    executable_settings_path(executable_path)
}

fn executable_settings_path(executable_path: &Path) -> AppResult<PathBuf> {
    if executable_path.file_name().is_none() {
        return Err(AppError::InvalidState("executable file name is missing"));
    }

    let mut settings_path = executable_path.to_path_buf();
    settings_path.set_extension("toml");
    Ok(settings_path)
}

fn config_version() -> u32 {
    CONFIG_VERSION
}

fn default_font_family() -> String {
    TerminalFont::default().family().to_owned()
}

fn default_font_size_points() -> u16 {
    TerminalFont::default().size_points()
}

fn write_settings_file_atomically(path: &Path, content: &[u8]) -> AppResult<()> {
    write_settings_file_atomically_with_directory_sync(path, content, sync_settings_directory)
}

fn write_settings_file_atomically_with_directory_sync(
    path: &Path,
    content: &[u8],
    sync_directory: impl FnOnce(&Path) -> AppResult<()>,
) -> AppResult<()> {
    let (temp_path, mut temp_file) = create_temporary_settings_file(path)?;

    if let Err(source) = temp_file.write_all(content) {
        drop(temp_file);
        cleanup_temporary_settings_file(&temp_path);
        return Err(AppError::io("write temporary settings file", source));
    }

    if let Err(source) = temp_file.sync_all() {
        drop(temp_file);
        cleanup_temporary_settings_file(&temp_path);
        return Err(AppError::io("sync temporary settings file", source));
    }

    drop(temp_file);

    if let Err(source) = replace_settings_file(&temp_path, path) {
        cleanup_temporary_settings_file(&temp_path);
        return Err(AppError::io("replace settings file", source));
    }

    // Once replacement succeeds, the new settings are already visible. Directory
    // sync only improves crash durability, so it must not make callers roll back
    // in-memory state while disk keeps the new file.
    let _ = sync_directory(path);
    Ok(())
}

fn create_temporary_settings_file(path: &Path) -> AppResult<(PathBuf, File)> {
    let file_name = path
        .file_name()
        .ok_or(AppError::InvalidState("settings file name is missing"))?;
    let directory = settings_directory(path);
    fs::create_dir_all(directory)
        .map_err(|source| AppError::io("create settings directory", source))?;

    for attempt in 0..TEMP_FILE_ATTEMPTS {
        let mut temp_name = OsString::from(".");
        temp_name.push(file_name);
        temp_name.push(format!(".{}.{}.new", std::process::id(), attempt));
        let temp_path = directory.join(temp_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(AppError::io("create temporary settings file", source)),
        }
    }

    Err(AppError::io(
        "create temporary settings file",
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary settings file path already exists",
        ),
    ))
}

fn settings_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn cleanup_temporary_settings_file(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(windows)]
fn replace_settings_file(temp_path: &Path, target_path: &Path) -> io::Result<()> {
    if target_path.try_exists()? {
        return replace_existing_settings_file(temp_path, target_path);
    }

    match fs::rename(temp_path, target_path) {
        Ok(()) => Ok(()),
        Err(rename_error) => match target_path.try_exists() {
            Ok(true) => replace_existing_settings_file(temp_path, target_path),
            _ => Err(rename_error),
        },
    }
}

#[cfg(windows)]
fn replace_existing_settings_file(temp_path: &Path, target_path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    const REPLACEFILE_WRITE_THROUGH: u32 = 0x00000001;

    let target_path = target_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let temp_path = temp_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();

    // SAFETY: The Windows API receives valid null-terminated UTF-16 buffers that
    // remain alive for the call. Optional backup/exclude/reserved pointers are null,
    // and ReplaceFileW does not retain any pointer after returning.
    let result = unsafe {
        replace_file_w(
            target_path.as_ptr(),
            temp_path.as_ptr(),
            ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };

    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_settings_file(temp_path: &Path, target_path: &Path) -> io::Result<()> {
    fs::rename(temp_path, target_path)
}

#[cfg(unix)]
fn sync_settings_directory(path: &Path) -> AppResult<()> {
    let directory = settings_directory(path);
    let directory =
        File::open(directory).map_err(|source| AppError::io("open settings directory", source))?;
    directory
        .sync_all()
        .map_err(|source| AppError::io("sync settings directory", source))
}

#[cfg(not(unix))]
fn sync_settings_directory(_path: &Path) -> AppResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn executable_settings_path_uses_executable_directory_and_name_with_toml_extension()
    -> AppResult<()> {
        let executable = Path::new("bin").join("j3term.exe");

        assert_eq!(
            executable_settings_path(&executable)?,
            Path::new("bin").join("j3term.toml")
        );
        Ok(())
    }

    #[test]
    fn default_settings_path_uses_executable_directory() -> AppResult<()> {
        let executable = Path::new("/opt/j3term/bin").join("j3term");

        assert_eq!(
            default_settings_path(&executable)?,
            Path::new("/opt/j3term/bin").join("j3term.toml")
        );
        Ok(())
    }

    #[test]
    fn legacy_settings_path_uses_executable_directory() -> AppResult<()> {
        let executable = Path::new("/opt/j3term/bin").join("j3term");

        assert_eq!(
            legacy_settings_path_for_executable(&executable)?,
            Path::new("/opt/j3term/bin").join("j3term.toml")
        );
        Ok(())
    }

    #[test]
    fn command_panel_round_trips_through_toml_config() -> AppResult<()> {
        let command_panel = CommandPanel::from_definitions(
            vec![
                CommandCategoryDefinition::new(
                    "Default",
                    vec![CommandButtonDefinition::new(
                        "echo hello",
                        "echo",
                        CommandArguments::new("hello")?,
                    )?],
                )?,
                CommandCategoryDefinition::new("Tools", Vec::new())?,
            ],
            1,
        )?;

        let serialized = toml::to_string_pretty(&ConfigFile::from_command_panel(&command_panel))
            .map_err(|source| {
                AppError::ui_message("serialize settings file", source.to_string())
            })?;
        let loaded: ConfigFile = toml::from_str(&serialized)
            .map_err(|source| AppError::ui_message("parse settings file", source.to_string()))?;
        let loaded_panel = loaded.into_command_panel()?;

        assert_eq!(
            loaded_panel
                .selected_category()
                .map(|category| category.name.as_str()),
            Some("Tools")
        );
        assert_eq!(loaded_panel.categories()[0].buttons[0].label, "echo hello");
        assert_eq!(
            loaded_panel.categories()[0].buttons[0].arguments.value(),
            "hello"
        );
        Ok(())
    }

    #[test]
    fn app_settings_round_trips_font_through_toml_config() -> AppResult<()> {
        let settings = AppSettings {
            command_panel: CommandPanel::from_definitions(
                vec![CommandCategoryDefinition::new("Default", Vec::new())?],
                0,
            )?,
            terminal_font: TerminalFont::new("Fira Code", 18)?,
        };

        let serialized = toml::to_string_pretty(&ConfigFile::from_app_settings(&settings))
            .map_err(|source| {
                AppError::ui_message("serialize settings file", source.to_string())
            })?;
        let loaded: ConfigFile = toml::from_str(&serialized)
            .map_err(|source| AppError::ui_message("parse settings file", source.to_string()))?;
        let loaded = loaded.into_app_settings()?;

        assert_eq!(loaded.terminal_font.family(), "Fira Code");
        assert_eq!(loaded.terminal_font.size_points(), 18);
        Ok(())
    }

    #[test]
    fn config_without_font_uses_default_terminal_font() -> AppResult<()> {
        let loaded: ConfigFile = toml::from_str(
            r#"
version = 1
selected_category = 0

[[categories]]
name = "Default"
"#,
        )
        .map_err(|source| AppError::ui_message("parse settings file", source.to_string()))?;

        let loaded = loaded.into_app_settings()?;

        assert_eq!(loaded.terminal_font, TerminalFont::default());
        Ok(())
    }

    #[test]
    fn save_command_panel_replaces_existing_settings_file() -> AppResult<()> {
        let settings_path = unique_test_settings_path("replace-existing")?;
        let store = ConfigStore::for_test_path(settings_path.clone());
        fs::write(&settings_path, "not valid toml")
            .map_err(|source| AppError::io("write existing test settings file", source))?;

        let command_panel = CommandPanel::from_definitions(
            vec![CommandCategoryDefinition::new(
                "Default",
                vec![CommandButtonDefinition::new(
                    "echo saved",
                    "echo",
                    CommandArguments::new("saved")?,
                )?],
            )?],
            0,
        )?;

        store.save_command_panel(&command_panel)?;
        let loaded_panel = store.load_or_default()?;

        assert_eq!(loaded_panel.categories()[0].buttons[0].label, "echo saved");

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn save_command_panel_creates_missing_settings_directory() -> AppResult<()> {
        let settings_path = unique_test_settings_path("create-missing-directory")?;
        let directory = settings_path
            .parent()
            .ok_or(AppError::InvalidState("test settings directory is missing"))?
            .to_path_buf();
        fs::remove_dir(&directory)
            .map_err(|source| AppError::io("remove test settings directory", source))?;
        let store = ConfigStore::for_test_path(settings_path.clone());

        let command_panel = CommandPanel::from_definitions(
            vec![CommandCategoryDefinition::new(
                "Default",
                vec![CommandButtonDefinition::new(
                    "echo saved",
                    "echo",
                    CommandArguments::new("saved")?,
                )?],
            )?],
            0,
        )?;

        store.save_command_panel(&command_panel)?;

        assert!(
            settings_path
                .try_exists()
                .map_err(|source| AppError::io("check created test settings file", source))?
        );

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn write_settings_file_atomically_succeeds_when_directory_sync_fails_after_replace()
    -> AppResult<()> {
        let settings_path = unique_test_settings_path("directory-sync-failure")?;
        let content = b"saved settings";

        write_settings_file_atomically_with_directory_sync(&settings_path, content, |_| {
            Err(AppError::io(
                "sync settings directory",
                io::Error::new(io::ErrorKind::PermissionDenied, "forced sync failure"),
            ))
        })?;

        let saved_content = fs::read(&settings_path)
            .map_err(|source| AppError::io("read saved test settings file", source))?;
        assert_eq!(saved_content, content);

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn load_or_default_returns_default_panel_without_creating_missing_settings_file()
    -> AppResult<()> {
        let settings_path = unique_test_settings_path("missing-settings")?;
        let store = ConfigStore::for_test_path(settings_path.clone());

        let loaded_panel = store.load_or_default()?;
        let default_panel = default_platform_command_panel();

        assert_eq!(
            loaded_panel.selected_category_index(),
            default_panel.selected_category_index()
        );
        assert_eq!(
            loaded_panel.categories()[0].name.as_str(),
            default_panel.categories()[0].name.as_str()
        );
        assert_eq!(
            loaded_panel.categories()[0].buttons.len(),
            default_panel.categories()[0].buttons.len()
        );
        assert!(
            !settings_path
                .try_exists()
                .map_err(|source| AppError::io("check missing test settings file", source))?
        );

        cleanup_test_settings_path(&settings_path);
        Ok(())
    }

    #[test]
    fn load_or_default_reads_legacy_settings_when_primary_is_missing() -> AppResult<()> {
        let settings_path = unique_test_settings_path("primary-missing")?;
        let legacy_settings_path = unique_test_settings_path("legacy-existing")?;
        let legacy_store = ConfigStore::for_test_path(legacy_settings_path.clone());
        let store = ConfigStore {
            path: settings_path.clone(),
            legacy_load_path: Some(legacy_settings_path.clone()),
        };
        let command_panel = CommandPanel::from_definitions(
            vec![CommandCategoryDefinition::new(
                "Legacy",
                vec![CommandButtonDefinition::new(
                    "echo legacy",
                    "echo",
                    CommandArguments::new("legacy")?,
                )?],
            )?],
            0,
        )?;
        legacy_store.save_command_panel(&command_panel)?;

        let loaded_panel = store.load_or_default()?;

        assert_eq!(loaded_panel.categories()[0].name, "Legacy");
        assert_eq!(loaded_panel.categories()[0].buttons[0].label, "echo legacy");
        assert!(
            !settings_path.try_exists().map_err(|source| AppError::io(
                "check missing primary test settings file",
                source
            ))?
        );

        cleanup_test_settings_path(&settings_path);
        cleanup_test_settings_path(&legacy_settings_path);
        Ok(())
    }

    fn unique_test_settings_path(name: &str) -> AppResult<PathBuf> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|source| AppError::ui_message("resolve test timestamp", source.to_string()))?
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "j3term-config-{}-{}-{}",
            name,
            std::process::id(),
            timestamp
        ));
        fs::create_dir(&directory)
            .map_err(|source| AppError::io("create test settings directory", source))?;
        Ok(directory.join("settings.toml"))
    }

    fn cleanup_test_settings_path(path: &Path) {
        if let Some(directory) = path.parent() {
            let _ = fs::remove_file(path);
            let _ = fs::remove_dir(directory);
        }
    }
}
