use std::env;
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use portable_pty::CommandBuilder;

use crate::domain::{ShellCommandDialect, StartupDirectory};

const CMD_UTF8_INIT_COMMAND: &str = "chcp 65001 >NUL";
const POWERSHELL_UTF8_INIT_COMMAND: &str = concat!(
    "$utf8 = New-Object System.Text.UTF8Encoding $false; ",
    "[Console]::InputEncoding = $utf8; ",
    "[Console]::OutputEncoding = $utf8; ",
    "$OutputEncoding = $utf8"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DefaultShell {
    CommandPrompt(PathBuf),
    PowerShell(PathBuf),
    #[cfg(not(target_os = "windows"))]
    Posix(PathBuf),
}

impl DefaultShell {
    pub(super) fn detect() -> Self {
        detect_default_shell()
    }

    pub(super) fn command_builder(
        &self,
        startup_directory: Option<&StartupDirectory>,
    ) -> CommandBuilder {
        let mut command = match self {
            Self::CommandPrompt(path) => {
                let mut command = CommandBuilder::new(path.as_os_str());
                command.arg("/K");
                command.arg(CMD_UTF8_INIT_COMMAND);
                command
            }
            Self::PowerShell(path) => {
                let mut command = CommandBuilder::new(path.as_os_str());
                command.arg("-NoLogo");
                command.arg("-NoExit");
                command.arg("-Command");
                command.arg(POWERSHELL_UTF8_INIT_COMMAND);
                command
            }
            #[cfg(not(target_os = "windows"))]
            Self::Posix(path) => {
                let mut command = CommandBuilder::new(path.as_os_str());
                command.env("TERM", "xterm-256color");
                command
            }
        };

        if let Some(startup_directory) = startup_directory {
            command.cwd(startup_directory.path().as_os_str());
        }

        command
    }

    pub(super) fn graceful_exit_sequence(&self) -> &'static [u8] {
        match self {
            Self::CommandPrompt(_) | Self::PowerShell(_) => b"exit\r",
            #[cfg(not(target_os = "windows"))]
            Self::Posix(_) => b"exit\n",
        }
    }

    pub(super) fn command_dialect(&self) -> ShellCommandDialect {
        match self {
            Self::CommandPrompt(_) => ShellCommandDialect::CommandPrompt,
            Self::PowerShell(_) => ShellCommandDialect::PowerShell,
            #[cfg(not(target_os = "windows"))]
            Self::Posix(_) => ShellCommandDialect::Posix,
        }
    }
}

#[cfg(target_os = "windows")]
fn detect_default_shell() -> DefaultShell {
    if let Some(comspec) = non_empty_env("ComSpec") {
        let path = PathBuf::from(comspec);
        if is_powershell_path(&path) {
            return DefaultShell::PowerShell(path);
        }
        return DefaultShell::CommandPrompt(path);
    }

    if let Some(powershell) = windows_powershell_path() {
        return DefaultShell::PowerShell(powershell);
    }

    DefaultShell::CommandPrompt(PathBuf::from("cmd.exe"))
}

#[cfg(not(target_os = "windows"))]
fn detect_default_shell() -> DefaultShell {
    detect_posix_shell(non_empty_env("SHELL"))
}

#[cfg(not(target_os = "windows"))]
fn detect_posix_shell(shell: Option<OsString>) -> DefaultShell {
    shell
        .map(PathBuf::from)
        .filter(|path| is_executable_file(path))
        .map(DefaultShell::Posix)
        .unwrap_or_else(|| DefaultShell::Posix(PathBuf::from("/bin/sh")))
}

#[cfg(not(target_os = "windows"))]
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };

    metadata.is_file() && has_execute_permission(&metadata)
}

#[cfg(unix)]
fn has_execute_permission(metadata: &std::fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(all(not(target_os = "windows"), not(unix)))]
fn has_execute_permission(_metadata: &std::fs::Metadata) -> bool {
    true
}

fn non_empty_env(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.as_os_str().is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(target_os = "windows")]
pub(super) fn windows_powershell_path() -> Option<PathBuf> {
    let system_root = PathBuf::from(non_empty_env("SystemRoot")?);
    let powershell = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");

    if powershell.is_file() {
        Some(powershell)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn is_powershell_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("powershell.exe") || name.eq_ignore_ascii_case("pwsh.exe")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::io::Write;

    #[test]
    fn command_prompt_builder_uses_explicit_startup_directory() -> crate::error::AppResult<()> {
        let shell = DefaultShell::CommandPrompt(PathBuf::from("cmd.exe"));
        let startup_directory = StartupDirectory::new(PathBuf::from(r"C:\Windows"))?;
        let command = shell.command_builder(Some(&startup_directory));

        assert_eq!(
            command.get_cwd().map(|cwd| PathBuf::from(cwd.as_os_str())),
            Some(startup_directory.path().to_path_buf())
        );
        Ok(())
    }

    #[test]
    fn command_prompt_builder_initializes_utf8_code_page() {
        let shell = DefaultShell::CommandPrompt(PathBuf::from("cmd.exe"));
        let command = shell.command_builder(None);

        assert_eq!(
            command_argv(&command),
            vec!["cmd.exe", "/K", CMD_UTF8_INIT_COMMAND]
        );
    }

    #[test]
    fn powershell_builder_uses_explicit_startup_directory() -> crate::error::AppResult<()> {
        let shell = DefaultShell::PowerShell(PathBuf::from("powershell.exe"));
        let startup_directory = StartupDirectory::new(PathBuf::from(r"C:\Windows"))?;
        let command = shell.command_builder(Some(&startup_directory));

        assert_eq!(
            command.get_cwd().map(|cwd| PathBuf::from(cwd.as_os_str())),
            Some(startup_directory.path().to_path_buf())
        );
        Ok(())
    }

    #[test]
    fn powershell_builder_initializes_utf8_console_encoding() {
        let shell = DefaultShell::PowerShell(PathBuf::from("powershell.exe"));
        let command = shell.command_builder(None);

        assert_eq!(
            command_argv(&command),
            vec![
                "powershell.exe",
                "-NoLogo",
                "-NoExit",
                "-Command",
                POWERSHELL_UTF8_INIT_COMMAND
            ]
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn posix_builder_uses_terminal_environment() {
        let shell = DefaultShell::Posix(PathBuf::from("/bin/sh"));
        let command = shell.command_builder(None);

        assert_eq!(
            command.get_env("TERM").and_then(|value| value.to_str()),
            Some("xterm-256color")
        );
    }

    #[cfg(unix)]
    #[test]
    fn posix_shell_detection_falls_back_for_non_executable_file() -> crate::error::AppResult<()> {
        let candidate = TestFile::create("non-executable-shell", 0o600)?;

        assert_eq!(
            detect_posix_shell(Some(candidate.path().as_os_str().to_os_string())),
            DefaultShell::Posix(PathBuf::from("/bin/sh"))
        );

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn posix_shell_detection_accepts_executable_file() -> crate::error::AppResult<()> {
        let candidate = TestFile::create("executable-shell", 0o700)?;

        assert_eq!(
            detect_posix_shell(Some(candidate.path().as_os_str().to_os_string())),
            DefaultShell::Posix(candidate.path().to_path_buf())
        );

        Ok(())
    }

    #[cfg(unix)]
    struct TestFile {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl TestFile {
        fn create(name: &str, mode: u32) -> crate::error::AppResult<Self> {
            let path = std::env::temp_dir().join(format!(
                "j3term-{name}-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            let mut file = fs::File::create(&path)?;
            file.write_all(b"#!/bin/sh\n")?;

            let mut permissions = file.metadata()?.permissions();
            permissions.set_mode(mode);
            fs::set_permissions(&path, permissions)?;

            Ok(Self { path })
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    #[cfg(unix)]
    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn command_argv(command: &CommandBuilder) -> Vec<String> {
        command
            .get_argv()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(unix)]
    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    }
}
