use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

pub fn load_text_file_or_embedded(file_name: &str, embedded_text: &str) -> String {
    read_distribution_text_file(file_name).unwrap_or_else(|_| embedded_text.to_owned())
}

fn read_distribution_text_file(file_name: &str) -> io::Result<String> {
    for directory in distribution_text_directories() {
        let path = directory.join(file_name);
        match fs::read_to_string(&path) {
            Ok(content) => return Ok(content),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("distribution text file was not found: {file_name}"),
    ))
}

fn distribution_text_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();

    if let Ok(executable_path) = env::current_exe()
        && let Some(directory) = executable_path.parent()
    {
        directories.push(directory.to_owned());
    }

    if let Ok(current_directory) = env::current_dir()
        && !directories
            .iter()
            .any(|directory| directory == &current_directory)
    {
        directories.push(current_directory);
    }

    directories
}
