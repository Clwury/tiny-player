use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const DEFAULT_FILTER: &str = "tiny=info,reqwest=warn,hyper=warn,hyper_util=warn";
const LOG_FILE_ENV: &str = "TINY_LOG_FILE";

pub(crate) fn init() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let file_layer = log_file_path_from_env().and_then(|path| match open_log_file(&path) {
        Ok(file) => Some(
            fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(Mutex::new(file)),
        ),
        Err(error) => {
            eprintln!(
                "failed to open log file from {LOG_FILE_ENV}={}: {error}",
                path.display()
            );
            None
        }
    });

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).pretty())
        .with(file_layer)
        .try_init();
}

fn log_file_path_from_env() -> Option<PathBuf> {
    std::env::var_os(LOG_FILE_ENV).and_then(log_file_path_from_value)
}

fn log_file_path_from_value(value: OsString) -> Option<PathBuf> {
    if value == OsStr::new("") {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn open_log_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| parent.as_os_str() != OsStr::new(""))
    {
        fs::create_dir_all(parent)?;
    }

    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn log_file_path_ignores_empty_env_value() {
        assert_eq!(log_file_path_from_value(OsString::new()), None);
    }

    #[test]
    fn log_file_path_accepts_non_empty_env_value() {
        assert_eq!(
            log_file_path_from_value(OsString::from("tiny.log")),
            Some(PathBuf::from("tiny.log"))
        );
    }

    #[test]
    fn open_log_file_creates_parent_dirs_and_truncates() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("logs").join("tiny.log");

        {
            let mut file = open_log_file(&log_path).unwrap();
            writeln!(file, "first").unwrap();
        }

        {
            let mut file = open_log_file(&log_path).unwrap();
            writeln!(file, "second").unwrap();
        }

        assert_eq!(fs::read_to_string(log_path).unwrap(), "second\n");
    }
}
