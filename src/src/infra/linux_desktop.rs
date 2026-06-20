use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::domain::{APP_NAME, LINUX_APPLICATION_ID};
use crate::error::{AppError, AppResult};

const SVG_ICON_FILE_NAME: &str = "icon.svg";
const PNG_ICON_FILE_NAME: &str = "icon.png";
const LEGACY_APP_IDS: &[&str] = &[
    "io.github.j3term",
    "j3Term",
    "j3term",
    "j3Launcher",
    "j3launcher",
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DesktopInstallReport {
    written_files: usize,
    removed_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopInstallRequest {
    executable_path: PathBuf,
    current_directory: PathBuf,
    environment: DesktopEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopEnvironment {
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopInstallPaths {
    data_home: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopIdentity {
    app_id: String,
    no_display: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconSourceKind {
    Svg,
    Png,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IconSource {
    path: PathBuf,
    kind: IconSourceKind,
}

pub fn install_for_current_exe() -> AppResult<()> {
    let request = DesktopInstallRequest::from_process_environment()?;
    let paths = request.install_paths()?;
    let _report = install_with_request(&request, &paths)?;
    refresh_desktop_caches(paths.data_home());
    Ok(())
}

pub fn uninstall() -> AppResult<()> {
    let environment = DesktopEnvironment::from_process_environment();
    let paths = DesktopInstallPaths::new(environment.data_home()?);
    let _report = uninstall_from_paths(&paths)?;
    refresh_desktop_caches(paths.data_home());
    Ok(())
}

pub fn find_runtime_icon_path() -> AppResult<Option<PathBuf>> {
    let request = DesktopInstallRequest::from_process_environment()?;
    Ok(
        find_icon_source(&request.executable_path, &request.current_directory)
            .map(|source| source.path),
    )
}

impl DesktopInstallRequest {
    fn from_process_environment() -> AppResult<Self> {
        Ok(Self {
            executable_path: current_executable_path()?,
            current_directory: env::current_dir()
                .map_err(|source| AppError::io("resolve current directory", source))?,
            environment: DesktopEnvironment::from_process_environment(),
        })
    }

    fn install_paths(&self) -> AppResult<DesktopInstallPaths> {
        Ok(DesktopInstallPaths::new(self.environment.data_home()?))
    }
}

impl DesktopEnvironment {
    fn from_process_environment() -> Self {
        Self {
            xdg_data_home: env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            home: env::var_os("HOME").map(PathBuf::from),
        }
    }

    fn data_home(&self) -> AppResult<PathBuf> {
        if let Some(path) = self
            .xdg_data_home
            .as_ref()
            .filter(|path| path.is_absolute())
        {
            return Ok(path.clone());
        }

        let home = self.home.as_ref().ok_or(AppError::InvalidState(
            "HOME is required to install Linux desktop integration",
        ))?;
        Ok(home.join(".local").join("share"))
    }
}

impl DesktopInstallPaths {
    fn new(data_home: PathBuf) -> Self {
        Self { data_home }
    }

    fn data_home(&self) -> &Path {
        &self.data_home
    }

    fn applications_dir(&self) -> PathBuf {
        self.data_home.join("applications")
    }

    fn scalable_icon_dir(&self) -> PathBuf {
        self.data_home
            .join("icons")
            .join("hicolor")
            .join("scalable")
            .join("apps")
    }

    fn png_icon_dir(&self) -> PathBuf {
        self.data_home
            .join("icons")
            .join("hicolor")
            .join("256x256")
            .join("apps")
    }

    fn desktop_entry_path(&self, app_id: &str) -> PathBuf {
        self.applications_dir().join(format!("{app_id}.desktop"))
    }

    fn svg_icon_path(&self, app_id: &str) -> PathBuf {
        self.scalable_icon_dir().join(format!("{app_id}.svg"))
    }

    fn png_icon_path(&self, app_id: &str) -> PathBuf {
        self.png_icon_dir().join(format!("{app_id}.png"))
    }
}

fn current_executable_path() -> AppResult<PathBuf> {
    let path =
        env::current_exe().map_err(|source| AppError::io("resolve current executable", source))?;
    if path.is_absolute() {
        return Ok(path);
    }

    let current_directory =
        env::current_dir().map_err(|source| AppError::io("resolve current directory", source))?;
    Ok(current_directory.join(path))
}

fn install_with_request(
    request: &DesktopInstallRequest,
    paths: &DesktopInstallPaths,
) -> AppResult<DesktopInstallReport> {
    let mut report = DesktopInstallReport::default();
    remove_legacy_files(paths, &mut report)?;
    install_desktop_entries(request, paths, &mut report)?;
    install_icons(request, paths, &mut report)?;
    Ok(report)
}

fn install_desktop_entries(
    request: &DesktopInstallRequest,
    paths: &DesktopInstallPaths,
    report: &mut DesktopInstallReport,
) -> AppResult<()> {
    for identity in managed_desktop_identities() {
        let content = desktop_entry_content(&request.executable_path, &identity)?;
        write_file_if_changed(
            &paths.desktop_entry_path(&identity.app_id),
            content.as_bytes(),
            report,
        )?;
    }
    Ok(())
}

fn install_icons(
    request: &DesktopInstallRequest,
    paths: &DesktopInstallPaths,
    report: &mut DesktopInstallReport,
) -> AppResult<()> {
    let Some(source) = find_icon_source(&request.executable_path, &request.current_directory)
    else {
        return Ok(());
    };

    match source.kind {
        IconSourceKind::Svg => {
            for app_id in managed_app_ids() {
                copy_file_if_changed(&source.path, &paths.svg_icon_path(&app_id), report)?;
                remove_file_if_exists(&paths.png_icon_path(&app_id), report)?;
            }
        }
        IconSourceKind::Png => {
            for app_id in managed_app_ids() {
                copy_file_if_changed(&source.path, &paths.png_icon_path(&app_id), report)?;
                remove_file_if_exists(&paths.svg_icon_path(&app_id), report)?;
            }
        }
    }

    Ok(())
}

fn uninstall_from_paths(paths: &DesktopInstallPaths) -> AppResult<DesktopInstallReport> {
    let mut report = DesktopInstallReport::default();
    for app_id in managed_app_ids() {
        remove_installed_files(paths, &app_id, &mut report)?;
    }
    remove_legacy_files(paths, &mut report)?;
    Ok(report)
}

fn remove_legacy_files(
    paths: &DesktopInstallPaths,
    report: &mut DesktopInstallReport,
) -> AppResult<()> {
    for app_id in LEGACY_APP_IDS {
        remove_installed_files(paths, app_id, report)?;
    }
    Ok(())
}

fn remove_installed_files(
    paths: &DesktopInstallPaths,
    app_id: &str,
    report: &mut DesktopInstallReport,
) -> AppResult<()> {
    remove_file_if_exists(&paths.desktop_entry_path(app_id), report)?;
    remove_file_if_exists(&paths.svg_icon_path(app_id), report)?;
    remove_file_if_exists(&paths.png_icon_path(app_id), report)?;
    Ok(())
}

fn managed_desktop_identities() -> Vec<DesktopIdentity> {
    let mut identities = vec![DesktopIdentity {
        app_id: LINUX_APPLICATION_ID.to_owned(),
        no_display: false,
    }];

    if let Some(alias) = lowercase_alias_id(LINUX_APPLICATION_ID) {
        identities.push(DesktopIdentity {
            app_id: alias,
            no_display: true,
        });
    }

    identities
}

fn managed_app_ids() -> Vec<String> {
    managed_desktop_identities()
        .into_iter()
        .map(|identity| identity.app_id)
        .collect()
}

fn lowercase_alias_id(app_id: &str) -> Option<String> {
    if app_id.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Some(app_id.to_ascii_lowercase())
    } else {
        None
    }
}

fn desktop_entry_content(executable_path: &Path, identity: &DesktopIdentity) -> AppResult<String> {
    let exec = executable_path_to_desktop_entry_value(executable_path)?;
    let mut content = format!(
        "# Managed by {APP_NAME} --install\n\
         [Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         Comment={APP_NAME}\n\
         Exec={exec}\n\
         Icon={icon}\n\
         Terminal=false\n\
         Categories=Utility;\n\
         StartupNotify=true\n\
         StartupWMClass={startup_wm_class}\n",
        icon = identity.app_id,
        startup_wm_class = identity.app_id
    );
    if identity.no_display {
        content.push_str("NoDisplay=true\n");
    }
    Ok(content)
}

fn executable_path_to_desktop_entry_value(executable_path: &Path) -> AppResult<String> {
    let value = executable_path.to_str().ok_or(AppError::InvalidInput(
        "desktop entry executable path must be valid UTF-8",
    ))?;
    if value.contains('\n') || value.contains('\r') {
        return Err(AppError::InvalidInput(
            "desktop entry executable path must not contain newlines",
        ));
    }
    Ok(value.to_owned())
}

fn find_icon_source(executable_path: &Path, current_directory: &Path) -> Option<IconSource> {
    find_icon_source_by_name(executable_path, current_directory, SVG_ICON_FILE_NAME).map_or_else(
        || {
            find_icon_source_by_name(executable_path, current_directory, PNG_ICON_FILE_NAME).map(
                |path| IconSource {
                    path,
                    kind: IconSourceKind::Png,
                },
            )
        },
        |path| {
            Some(IconSource {
                path,
                kind: IconSourceKind::Svg,
            })
        },
    )
}

fn find_icon_source_by_name(
    executable_path: &Path,
    current_directory: &Path,
    file_name: &str,
) -> Option<PathBuf> {
    icon_source_directories(executable_path, current_directory)
        .into_iter()
        .map(|directory| directory.join(file_name))
        .find(|path| path.is_file())
}

fn icon_source_directories(executable_path: &Path, current_directory: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(directory) = executable_path.parent() {
        directories.push(directory.to_owned());
    }
    directories.push(current_directory.to_owned());
    directories
}

fn copy_file_if_changed(
    source: &Path,
    destination: &Path,
    report: &mut DesktopInstallReport,
) -> AppResult<()> {
    let content = fs::read(source).map_err(|error| AppError::io("read icon source", error))?;
    write_file_if_changed(destination, &content, report)
}

fn write_file_if_changed(
    path: &Path,
    content: &[u8],
    report: &mut DesktopInstallReport,
) -> AppResult<()> {
    match fs::read(path) {
        Ok(existing) if existing == content => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(AppError::io("read installed Linux desktop file", error)),
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| AppError::io("create Linux desktop directory", error))?;
    }
    fs::write(path, content).map_err(|error| AppError::io("write Linux desktop file", error))?;
    report.written_files += 1;
    Ok(())
}

fn remove_file_if_exists(path: &Path, report: &mut DesktopInstallReport) -> AppResult<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            report.removed_files += 1;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::io("remove Linux desktop file", error)),
    }
}

fn refresh_desktop_caches(data_home: &Path) {
    run_silent_command("update-desktop-database", &[]);

    let hicolor_icon_dir = data_home.join("icons").join("hicolor");
    run_silent_command(
        "gtk-update-icon-cache",
        &[
            OsString::from("-f"),
            OsString::from("-t"),
            hicolor_icon_dir.into_os_string(),
        ],
    );

    if !run_silent_command("kbuildsycoca6", &[OsString::from("--noincremental")]) {
        run_silent_command("kbuildsycoca5", &[OsString::from("--noincremental")]);
    }
}

fn run_silent_command(program: &str, arguments: &[OsString]) -> bool {
    match Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn data_home_uses_absolute_xdg_data_home() -> AppResult<()> {
        let data_home = unique_test_directory("xdg-data-home")?;
        let environment = DesktopEnvironment {
            xdg_data_home: Some(data_home.clone()),
            home: Some(PathBuf::from("/home/example")),
        };

        assert_eq!(environment.data_home()?, data_home);

        cleanup_test_directory(&environment.data_home()?);
        Ok(())
    }

    #[test]
    fn data_home_ignores_relative_xdg_data_home() -> AppResult<()> {
        let environment = DesktopEnvironment {
            xdg_data_home: Some(PathBuf::from("relative-data")),
            home: Some(PathBuf::from("/home/example")),
        };

        assert_eq!(
            environment.data_home()?,
            Path::new("/home/example").join(".local").join("share")
        );
        Ok(())
    }

    #[test]
    fn install_prefers_svg_and_removes_stale_png() -> AppResult<()> {
        let layout = TestLayout::new("install-svg")?;
        write_test_file(
            &layout.executable_dir.join(PNG_ICON_FILE_NAME),
            b"png-source",
        )?;
        write_test_file(&layout.current_dir.join(SVG_ICON_FILE_NAME), b"svg-source")?;

        let paths = layout.install_paths();
        write_test_file(&paths.png_icon_path(LINUX_APPLICATION_ID), b"stale-png")?;
        write_test_file(&paths.desktop_entry_path("j3Launcher"), b"legacy")?;
        write_test_file(&paths.svg_icon_path("j3Launcher"), b"legacy")?;

        let report = install_with_request(&layout.request, &paths)?;

        assert!(report.written_files >= 4);
        assert!(report.removed_files >= 3);
        assert_eq!(
            fs::read(paths.svg_icon_path(LINUX_APPLICATION_ID))
                .map_err(|error| AppError::io("read installed svg", error))?,
            b"svg-source"
        );
        assert!(!paths.png_icon_path(LINUX_APPLICATION_ID).exists());
        assert!(!paths.desktop_entry_path("j3Launcher").exists());
        assert!(!paths.svg_icon_path("j3Launcher").exists());

        layout.cleanup();
        Ok(())
    }

    #[test]
    fn install_uses_png_fallback_and_removes_stale_svg() -> AppResult<()> {
        let layout = TestLayout::new("install-png")?;
        write_test_file(&layout.current_dir.join(PNG_ICON_FILE_NAME), b"png-source")?;

        let paths = layout.install_paths();
        write_test_file(&paths.svg_icon_path(LINUX_APPLICATION_ID), b"stale-svg")?;

        install_with_request(&layout.request, &paths)?;

        assert_eq!(
            fs::read(paths.png_icon_path(LINUX_APPLICATION_ID))
                .map_err(|error| AppError::io("read installed png", error))?,
            b"png-source"
        );
        assert!(!paths.svg_icon_path(LINUX_APPLICATION_ID).exists());

        layout.cleanup();
        Ok(())
    }

    #[test]
    fn repeated_install_with_same_content_does_not_rewrite_files() -> AppResult<()> {
        let layout = TestLayout::new("install-idempotent")?;
        write_test_file(&layout.current_dir.join(SVG_ICON_FILE_NAME), b"svg-source")?;
        let paths = layout.install_paths();

        let first = install_with_request(&layout.request, &paths)?;
        let second = install_with_request(&layout.request, &paths)?;

        assert!(first.written_files > 0);
        assert_eq!(second, DesktopInstallReport::default());

        layout.cleanup();
        Ok(())
    }

    #[test]
    fn repeated_install_updates_changed_executable_path() -> AppResult<()> {
        let layout = TestLayout::new("install-moved")?;
        write_test_file(&layout.current_dir.join(SVG_ICON_FILE_NAME), b"svg-source")?;
        let paths = layout.install_paths();
        install_with_request(&layout.request, &paths)?;

        let moved_executable_dir = layout.root.join("new-bin");
        fs::create_dir_all(&moved_executable_dir)
            .map_err(|error| AppError::io("create moved executable dir", error))?;
        let moved_executable = moved_executable_dir.join("j3term");
        write_test_file(&moved_executable, b"bin")?;
        let moved_request = DesktopInstallRequest {
            executable_path: moved_executable.clone(),
            current_directory: layout.current_dir.clone(),
            environment: layout.request.environment.clone(),
        };

        let report = install_with_request(&moved_request, &paths)?;
        let desktop_entry = read_test_string(&paths.desktop_entry_path(LINUX_APPLICATION_ID))?;

        assert!(report.written_files >= 2);
        assert!(desktop_entry.contains(&format!("Exec={}", moved_executable.display())));

        layout.cleanup();
        Ok(())
    }

    #[test]
    fn installs_lowercase_alias_desktop_entry_and_icon() -> AppResult<()> {
        let layout = TestLayout::new("install-alias")?;
        write_test_file(&layout.current_dir.join(SVG_ICON_FILE_NAME), b"svg-source")?;
        let paths = layout.install_paths();

        install_with_request(&layout.request, &paths)?;

        let alias_id = lowercase_alias_id(LINUX_APPLICATION_ID)
            .ok_or(AppError::InvalidInput("alias should exist"))?;
        let alias_desktop = read_test_string(&paths.desktop_entry_path(&alias_id))?;

        assert!(alias_desktop.contains(&format!("Icon={alias_id}\n")));
        assert!(alias_desktop.contains(&format!("StartupWMClass={alias_id}\n")));
        assert!(alias_desktop.contains("NoDisplay=true\n"));
        assert_eq!(
            fs::read(paths.svg_icon_path(&alias_id))
                .map_err(|error| AppError::io("read alias svg", error))?,
            b"svg-source"
        );

        layout.cleanup();
        Ok(())
    }

    #[test]
    fn uninstall_removes_managed_alias_and_legacy_files() -> AppResult<()> {
        let layout = TestLayout::new("uninstall")?;
        let paths = layout.install_paths();
        let alias_id = lowercase_alias_id(LINUX_APPLICATION_ID)
            .ok_or(AppError::InvalidInput("alias should exist"))?;

        for app_id in [
            LINUX_APPLICATION_ID,
            alias_id.as_str(),
            "io.github.j3term",
            "j3Launcher",
            "j3launcher",
        ] {
            write_test_file(&paths.desktop_entry_path(app_id), b"desktop")?;
            write_test_file(&paths.svg_icon_path(app_id), b"svg")?;
            write_test_file(&paths.png_icon_path(app_id), b"png")?;
        }

        let first = uninstall_from_paths(&paths)?;
        let second = uninstall_from_paths(&paths)?;

        assert!(first.removed_files > 0);
        assert_eq!(second, DesktopInstallReport::default());
        for app_id in [
            LINUX_APPLICATION_ID,
            alias_id.as_str(),
            "io.github.j3term",
            "j3Launcher",
            "j3launcher",
        ] {
            assert!(!paths.desktop_entry_path(app_id).exists());
            assert!(!paths.svg_icon_path(app_id).exists());
            assert!(!paths.png_icon_path(app_id).exists());
        }

        layout.cleanup();
        Ok(())
    }

    struct TestLayout {
        root: PathBuf,
        executable_dir: PathBuf,
        current_dir: PathBuf,
        request: DesktopInstallRequest,
    }

    impl TestLayout {
        fn new(name: &str) -> AppResult<Self> {
            let root = unique_test_directory(name)?;
            let executable_dir = root.join("bin");
            let current_dir = root.join("cwd");
            let data_home = root.join("data-home");
            fs::create_dir_all(&executable_dir)
                .map_err(|error| AppError::io("create executable dir", error))?;
            fs::create_dir_all(&current_dir)
                .map_err(|error| AppError::io("create current dir", error))?;

            let executable_path = executable_dir.join("j3term");
            write_test_file(&executable_path, b"bin")?;

            let request = DesktopInstallRequest {
                executable_path,
                current_directory: current_dir.clone(),
                environment: DesktopEnvironment {
                    xdg_data_home: Some(data_home),
                    home: None,
                },
            };

            Ok(Self {
                root,
                executable_dir,
                current_dir,
                request,
            })
        }

        fn install_paths(&self) -> DesktopInstallPaths {
            DesktopInstallPaths::new(
                self.request
                    .environment
                    .xdg_data_home
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.root.join("data-home")),
            )
        }

        fn cleanup(self) {
            cleanup_test_directory(&self.root);
        }
    }

    fn unique_test_directory(name: &str) -> AppResult<PathBuf> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|source| AppError::ui_message("resolve test timestamp", source.to_string()))?
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "j3term-linux-desktop-{}-{}-{}",
            name,
            std::process::id(),
            timestamp
        ));
        fs::create_dir(&directory)
            .map_err(|source| AppError::io("create test directory", source))?;
        Ok(directory)
    }

    fn write_test_file(path: &Path, content: &[u8]) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| AppError::io("create test file parent", error))?;
        }
        fs::write(path, content).map_err(|error| AppError::io("write test file", error))
    }

    fn read_test_string(path: &Path) -> AppResult<String> {
        fs::read_to_string(path).map_err(|error| AppError::io("read test string", error))
    }

    fn cleanup_test_directory(directory: &Path) {
        let _ = fs::remove_dir_all(directory);
    }
}
