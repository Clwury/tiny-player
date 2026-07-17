use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

const DEFAULT_FILTER: &str = "tiny=info,reqwest=warn,hyper=warn,hyper_util=warn";
const LOG_FILE_ENV: &str = "TINY_LOG_FILE";
const LOG_MAX_BYTES_ENV: &str = "TINY_LOG_MAX_BYTES";
const LOG_BACKUP_COUNT_ENV: &str = "TINY_LOG_BACKUP_COUNT";
const DEFAULT_LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_LOG_BACKUP_COUNT: usize = 2;

pub(crate) fn init() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let file_layer = log_file_path_from_env().and_then(|path| match open_log_writer(&path) {
        Ok(writer) => Some(
            fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(Mutex::new(writer)),
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

fn log_max_bytes_from_env() -> u64 {
    std::env::var(LOG_MAX_BYTES_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_LOG_MAX_BYTES)
}

fn log_backup_count_from_env() -> usize {
    std::env::var(LOG_BACKUP_COUNT_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LOG_BACKUP_COUNT)
}

fn open_log_writer(path: &Path) -> io::Result<RotatingFileWriter> {
    RotatingFileWriter::new(
        path.to_path_buf(),
        log_max_bytes_from_env(),
        log_backup_count_from_env(),
    )
}

struct RotatingFileWriter {
    path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
    max_bytes: u64,
    backup_count: usize,
}

impl RotatingFileWriter {
    fn new(path: PathBuf, max_bytes: u64, backup_count: usize) -> io::Result<Self> {
        let file = open_log_file(&path)?;
        Ok(Self {
            path,
            file: Some(file),
            bytes_written: 0,
            max_bytes: max_bytes.max(1),
            backup_count,
        })
    }

    fn backup_path(&self, index: usize) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(format!(".{index}"));
        PathBuf::from(path)
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        if self.backup_count == 0 {
            if self.path.exists() {
                fs::remove_file(&self.path)?;
            }
        } else {
            let oldest = self.backup_path(self.backup_count);
            if oldest.exists() {
                fs::remove_file(oldest)?;
            }
            for index in (1..self.backup_count).rev() {
                let source = self.backup_path(index);
                if source.exists() {
                    fs::rename(source, self.backup_path(index + 1))?;
                }
            }
            if self.path.exists() {
                fs::rename(&self.path, self.backup_path(1))?;
            }
        }

        self.file = Some(open_log_file(&self.path)?);
        self.bytes_written = 0;
        Ok(())
    }
}

impl io::Write for RotatingFileWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let incoming = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if self.bytes_written > 0 && self.bytes_written.saturating_add(incoming) > self.max_bytes {
            self.rotate()?;
        }
        let written = self
            .file
            .as_mut()
            .expect("rotating log file is open")
            .write(buffer)?;
        self.bytes_written = self
            .bytes_written
            .saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("rotating log file is open")
            .flush()
    }
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

    #[test]
    fn rotating_log_writer_caps_files_and_keeps_recent_backups() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("tiny.log");
        let mut writer = RotatingFileWriter::new(log_path.clone(), 8, 2).unwrap();

        writer.write_all(b"first\n").unwrap();
        writer.write_all(b"second\n").unwrap();
        writer.write_all(b"third\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(fs::read_to_string(&log_path).unwrap(), "third\n");
        assert_eq!(
            fs::read_to_string(writer.backup_path(1)).unwrap(),
            "second\n"
        );
        assert_eq!(
            fs::read_to_string(writer.backup_path(2)).unwrap(),
            "first\n"
        );
    }
}
