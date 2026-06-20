use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::Path;

use crate::domain::{StartupCommand, StartupDirectory, StartupInvocation};
use crate::error::{AppError, AppResult};

#[cfg(target_os = "linux")]
use crate::infra::gtk as platform_ui;
#[cfg(target_os = "linux")]
use crate::infra::linux_desktop;
#[cfg(target_os = "windows")]
use crate::infra::win32 as platform_ui;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessInvocation {
    Run(StartupInvocation),
    InstallLinuxDesktopEntry,
    UninstallLinuxDesktopEntry,
}

pub fn run() -> AppResult<()> {
    match process_invocation_from_process_arguments()? {
        ProcessInvocation::Run(startup) => run_platform(startup),
        ProcessInvocation::InstallLinuxDesktopEntry => install_linux_desktop_entry(),
        ProcessInvocation::UninstallLinuxDesktopEntry => uninstall_linux_desktop_entry(),
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn run_platform(startup: StartupInvocation) -> AppResult<()> {
    platform_ui::run(startup)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn run_platform(_startup: StartupInvocation) -> AppResult<()> {
    Err(AppError::InvalidState(
        "j3term supports Windows and Linux only",
    ))
}

#[cfg(target_os = "linux")]
fn install_linux_desktop_entry() -> AppResult<()> {
    linux_desktop::install_for_current_exe()
}

#[cfg(not(target_os = "linux"))]
fn install_linux_desktop_entry() -> AppResult<()> {
    Err(AppError::InvalidInput(
        "Linux desktop entry installation is supported only on Linux",
    ))
}

#[cfg(target_os = "linux")]
fn uninstall_linux_desktop_entry() -> AppResult<()> {
    linux_desktop::uninstall()
}

#[cfg(not(target_os = "linux"))]
fn uninstall_linux_desktop_entry() -> AppResult<()> {
    Err(AppError::InvalidInput(
        "Linux desktop entry removal is supported only on Linux",
    ))
}

fn process_invocation_from_process_arguments() -> AppResult<ProcessInvocation> {
    process_invocation_from_os_arguments(env::args_os().skip(1).collect())
}

fn process_invocation_from_os_arguments(arguments: Vec<OsString>) -> AppResult<ProcessInvocation> {
    let Some(first_argument) = arguments.first() else {
        return Ok(ProcessInvocation::Run(StartupInvocation::default()));
    };

    if os_argument_eq(first_argument, "--install") {
        if arguments.len() == 1 {
            return Ok(ProcessInvocation::InstallLinuxDesktopEntry);
        }
        return Err(AppError::InvalidInput(
            "--install does not accept additional arguments",
        ));
    }

    if os_argument_eq(first_argument, "--uninstall") {
        if arguments.len() == 1 {
            return Ok(ProcessInvocation::UninstallLinuxDesktopEntry);
        }
        return Err(AppError::InvalidInput(
            "--uninstall does not accept additional arguments",
        ));
    }

    if os_argument_starts_with(first_argument, "--") {
        return Err(AppError::InvalidInput("unknown command line option"));
    }

    startup_invocation_from_os_arguments(arguments).map(ProcessInvocation::Run)
}

fn os_argument_eq(argument: &OsString, expected: &str) -> bool {
    argument.to_string_lossy() == expected
}

fn os_argument_starts_with(argument: &OsString, prefix: &str) -> bool {
    argument.to_string_lossy().starts_with(prefix)
}

fn startup_invocation_from_os_arguments(arguments: Vec<OsString>) -> AppResult<StartupInvocation> {
    let mut arguments = arguments.into_iter();
    let Some(first_argument) = arguments.next() else {
        return Ok(StartupInvocation::default());
    };

    let remaining_arguments = arguments.collect::<Vec<_>>();
    let (working_directory, command_arguments) = if is_startup_directory_argument(&first_argument)?
    {
        (
            Some(StartupDirectory::new(first_argument)?),
            remaining_arguments,
        )
    } else {
        let mut command_arguments = Vec::with_capacity(remaining_arguments.len().saturating_add(1));
        command_arguments.push(first_argument);
        command_arguments.extend(remaining_arguments);
        (None, command_arguments)
    };

    Ok(StartupInvocation::new(
        working_directory,
        startup_command_from_os_arguments(command_arguments)?,
    ))
}

fn is_startup_directory_argument(argument: &OsString) -> AppResult<bool> {
    match fs::metadata(Path::new(argument)) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(source)
            if matches!(
                source.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(false)
        }
        Err(source) => Err(AppError::io("inspect startup path", source)),
    }
}

fn startup_command_from_os_arguments(
    arguments: Vec<OsString>,
) -> AppResult<Option<StartupCommand>> {
    StartupCommand::from_os_arguments(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ShellCommandDialect;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn startup_invocation_is_empty_without_arguments() -> AppResult<()> {
        let startup = startup_invocation_from_os_arguments(Vec::new())?;

        assert!(startup.working_directory().is_none());
        assert!(startup.command().is_none());
        Ok(())
    }

    #[test]
    fn process_invocation_installs_linux_desktop_entry() -> AppResult<()> {
        let invocation = process_invocation_from_os_arguments(vec![OsString::from("--install")])?;

        assert_eq!(invocation, ProcessInvocation::InstallLinuxDesktopEntry);
        Ok(())
    }

    #[test]
    fn process_invocation_uninstalls_linux_desktop_entry() -> AppResult<()> {
        let invocation = process_invocation_from_os_arguments(vec![OsString::from("--uninstall")])?;

        assert_eq!(invocation, ProcessInvocation::UninstallLinuxDesktopEntry);
        Ok(())
    }

    #[test]
    fn process_invocation_rejects_install_with_extra_arguments() {
        assert!(matches!(
            process_invocation_from_os_arguments(vec![
                OsString::from("--install"),
                OsString::from("extra"),
            ]),
            Err(AppError::InvalidInput(
                "--install does not accept additional arguments"
            ))
        ));
    }

    #[test]
    fn process_invocation_rejects_uninstall_with_extra_arguments() {
        assert!(matches!(
            process_invocation_from_os_arguments(vec![
                OsString::from("--uninstall"),
                OsString::from("extra"),
            ]),
            Err(AppError::InvalidInput(
                "--uninstall does not accept additional arguments"
            ))
        ));
    }

    #[test]
    fn process_invocation_rejects_unknown_option() {
        assert!(matches!(
            process_invocation_from_os_arguments(vec![OsString::from("--unknown")]),
            Err(AppError::InvalidInput("unknown command line option"))
        ));
    }

    #[test]
    fn startup_invocation_uses_existing_first_directory_as_working_directory() -> AppResult<()> {
        let directory = unique_test_directory("startup-directory-only")?;

        let startup = startup_invocation_from_os_arguments(vec![directory.as_os_str().to_owned()])?;

        assert_eq!(
            startup.working_directory().map(StartupDirectory::path),
            Some(directory.as_path())
        );
        assert!(startup.command().is_none());

        cleanup_test_directory(&directory);
        Ok(())
    }

    #[test]
    fn startup_invocation_runs_remaining_arguments_after_startup_directory() -> AppResult<()> {
        let directory = unique_test_directory("startup-directory-command")?;

        let startup = startup_invocation_from_os_arguments(vec![
            directory.as_os_str().to_owned(),
            OsString::from("cargo"),
            OsString::from("test"),
        ])?;

        assert_eq!(
            startup.working_directory().map(StartupDirectory::path),
            Some(directory.as_path())
        );
        let command = startup
            .command()
            .ok_or(AppError::InvalidInput("startup command should exist"))?;
        assert_eq!(
            command.to_pty_bytes(ShellCommandDialect::CommandPrompt),
            b"cargo test\r".to_vec()
        );

        cleanup_test_directory(&directory);
        Ok(())
    }

    #[test]
    fn startup_invocation_preserves_non_directory_arguments_as_command() -> AppResult<()> {
        let startup = startup_invocation_from_os_arguments(vec![
            OsString::from("cargo"),
            OsString::from("check"),
        ])?;

        assert!(startup.working_directory().is_none());
        let command = startup
            .command()
            .ok_or(AppError::InvalidInput("startup command should exist"))?;
        assert_eq!(
            command.to_pty_bytes(ShellCommandDialect::CommandPrompt),
            b"cargo check\r".to_vec()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn startup_invocation_preserves_non_utf8_command_argument_bytes_for_posix() -> AppResult<()> {
        use std::os::unix::ffi::OsStringExt;

        let path_bytes = vec![
            b'/', b't', b'm', b'p', b'/', b'n', b'o', b'n', b'u', b't', b'f', b'-', 0xff, b'-',
            b'\'', b'e', b'n', b'd',
        ];
        let startup = startup_invocation_from_os_arguments(vec![
            OsString::from("cat"),
            OsString::from_vec(path_bytes.clone()),
        ])?;

        assert!(startup.working_directory().is_none());
        let command = startup
            .command()
            .ok_or(AppError::InvalidInput("startup command should exist"))?;

        let mut expected = b"'cat' '".as_slice().to_vec();
        for byte in path_bytes {
            if byte == b'\'' {
                expected.extend(b"'\\''");
            } else {
                expected.push(byte);
            }
        }
        expected.extend(b"'\r");
        assert_eq!(command.to_pty_bytes(ShellCommandDialect::Posix), expected);
        Ok(())
    }

    fn unique_test_directory(name: &str) -> AppResult<PathBuf> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|source| AppError::ui_message("resolve test timestamp", source.to_string()))?
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "j3term-entry-{}-{}-{}",
            name,
            std::process::id(),
            timestamp
        ));
        fs::create_dir(&directory)
            .map_err(|source| AppError::io("create test startup directory", source))?;
        Ok(directory)
    }

    fn cleanup_test_directory(directory: &Path) {
        let _ = fs::remove_dir(directory);
    }
}
