#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
#![cfg_attr(target_os = "linux", allow(dead_code))]

mod app;
mod domain;
mod entry;
mod error;
mod infra;

fn main() -> std::process::ExitCode {
    match entry::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.user_message());
            eprintln!("cause: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
