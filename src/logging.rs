use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tracing_subscriber::{EnvFilter, fmt, fmt::MakeWriter, prelude::*};

const DEFAULT_FILTER: &str = "tiny=info,reqwest=warn,hyper=warn,hyper_util=warn";
const LOG_FILE_ENV: &str = "TINY_LOG_FILE";
const LOG_MAX_BYTES_ENV: &str = "TINY_LOG_MAX_BYTES";
const LOG_BACKUP_COUNT_ENV: &str = "TINY_LOG_BACKUP_COUNT";
const LOG_QUEUE_CAPACITY_ENV: &str = "TINY_LOG_QUEUE_CAPACITY";
const DEFAULT_LOG_MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_LOG_BACKUP_COUNT: usize = 2;
const DEFAULT_LOG_QUEUE_CAPACITY: usize = 4_096;
const MAX_LOG_EVENT_BYTES: usize = 64 * 1024;
const DROPPED_LOG_SUMMARY_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) fn init() -> Option<NonBlockingLogGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let (file_layer, file_guard) = match log_file_path_from_env() {
        Some(path) => match open_non_blocking_log_writer(&path) {
            Ok((writer, guard)) => (
                Some(
                    fmt::layer()
                        .with_target(true)
                        .with_ansi(false)
                        .with_writer(writer),
                ),
                Some(guard),
            ),
            Err(error) => {
                eprintln!(
                    "failed to open log file from {LOG_FILE_ENV}={}: {error}",
                    path.display()
                );
                (None, None)
            }
        },
        None => (None, None),
    };

    if tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).pretty())
        .with(file_layer)
        .try_init()
        .is_ok()
    {
        file_guard
    } else {
        None
    }
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

fn log_queue_capacity_from_env() -> usize {
    std::env::var(LOG_QUEUE_CAPACITY_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(65_536))
        .unwrap_or(DEFAULT_LOG_QUEUE_CAPACITY)
}

fn open_log_writer(path: &Path) -> io::Result<RotatingFileWriter> {
    RotatingFileWriter::new(
        path.to_path_buf(),
        log_max_bytes_from_env(),
        log_backup_count_from_env(),
    )
}

fn open_non_blocking_log_writer(
    path: &Path,
) -> io::Result<(NonBlockingLogMakeWriter, NonBlockingLogGuard)> {
    let writer = open_log_writer(path)?;
    let queue_capacity = log_queue_capacity_from_env();
    let (sender, receiver) = mpsc::sync_channel(queue_capacity);
    let dropped_events = Arc::new(AtomicU64::new(0));
    let worker_dropped_events = Arc::clone(&dropped_events);
    let handle = thread::Builder::new()
        .name("tiny-log-writer".to_string())
        .spawn(move || {
            run_non_blocking_log_worker(writer, receiver, worker_dropped_events, queue_capacity)
        })?;
    Ok((
        NonBlockingLogMakeWriter {
            sender: sender.clone(),
            dropped_events,
        },
        NonBlockingLogGuard {
            sender: Some(sender),
            handle: Some(handle),
        },
    ))
}

enum NonBlockingLogCommand {
    Event(Vec<u8>),
    Shutdown,
}

#[derive(Clone)]
struct NonBlockingLogMakeWriter {
    sender: SyncSender<NonBlockingLogCommand>,
    dropped_events: Arc<AtomicU64>,
}

struct NonBlockingLogEventWriter {
    sender: SyncSender<NonBlockingLogCommand>,
    dropped_events: Arc<AtomicU64>,
    buffer: Vec<u8>,
    truncated: bool,
}

impl<'a> MakeWriter<'a> for NonBlockingLogMakeWriter {
    type Writer = NonBlockingLogEventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        NonBlockingLogEventWriter {
            sender: self.sender.clone(),
            dropped_events: Arc::clone(&self.dropped_events),
            buffer: Vec::with_capacity(512),
            truncated: false,
        }
    }
}

impl Write for NonBlockingLogEventWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = MAX_LOG_EVENT_BYTES.saturating_sub(self.buffer.len());
        let retained = remaining.min(buffer.len());
        self.buffer.extend_from_slice(&buffer[..retained]);
        self.truncated |= retained < buffer.len();
        // Formatting must never retry or block because the file sink reached
        // its bounded memory budget.
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for NonBlockingLogEventWriter {
    fn drop(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        if self.truncated {
            const SUFFIX: &[u8] = b" [file log event truncated]\n";
            let suffix_start = MAX_LOG_EVENT_BYTES.saturating_sub(SUFFIX.len());
            self.buffer.truncate(suffix_start);
            self.buffer.extend_from_slice(SUFFIX);
        }
        let event = std::mem::take(&mut self.buffer);
        if let Err(error) = self.sender.try_send(NonBlockingLogCommand::Event(event)) {
            match error {
                TrySendError::Full(_) | TrySendError::Disconnected(_) => {
                    self.dropped_events.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

pub(crate) struct NonBlockingLogGuard {
    sender: Option<SyncSender<NonBlockingLogCommand>>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for NonBlockingLogGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            // Shutdown happens after the UI loop returns, so draining the
            // bounded tail here does not add latency to playback.
            let _ = sender.send(NonBlockingLogCommand::Shutdown);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_non_blocking_log_worker(
    mut writer: RotatingFileWriter,
    receiver: Receiver<NonBlockingLogCommand>,
    dropped_events: Arc<AtomicU64>,
    queue_capacity: usize,
) {
    let mut last_drop_summary_at = Instant::now();
    loop {
        match receiver.recv_timeout(DROPPED_LOG_SUMMARY_INTERVAL) {
            Ok(NonBlockingLogCommand::Event(event)) => {
                if let Err(error) = writer.write_all(&event) {
                    eprintln!("failed to write asynchronous tiny log event: {error}");
                }
                if last_drop_summary_at.elapsed() >= DROPPED_LOG_SUMMARY_INTERVAL {
                    write_dropped_log_summary(
                        &mut writer,
                        &dropped_events,
                        queue_capacity,
                        last_drop_summary_at.elapsed(),
                    );
                    last_drop_summary_at = Instant::now();
                }
            }
            Ok(NonBlockingLogCommand::Shutdown) => {
                write_dropped_log_summary(
                    &mut writer,
                    &dropped_events,
                    queue_capacity,
                    last_drop_summary_at.elapsed(),
                );
                let _ = writer.flush();
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                write_dropped_log_summary(
                    &mut writer,
                    &dropped_events,
                    queue_capacity,
                    last_drop_summary_at.elapsed(),
                );
                last_drop_summary_at = Instant::now();
                let _ = writer.flush();
            }
            Err(RecvTimeoutError::Disconnected) => {
                write_dropped_log_summary(
                    &mut writer,
                    &dropped_events,
                    queue_capacity,
                    last_drop_summary_at.elapsed(),
                );
                let _ = writer.flush();
                break;
            }
        }
    }
}

fn write_dropped_log_summary(
    writer: &mut RotatingFileWriter,
    dropped_events: &AtomicU64,
    queue_capacity: usize,
    elapsed: Duration,
) {
    let dropped = dropped_events.swap(0, Ordering::AcqRel);
    if dropped == 0 {
        return;
    }
    let summary = format!(
        " WARN tiny_player::logging: suppressed_file_log_events={dropped} summary_interval_ms={:.3} bounded_queue_capacity={queue_capacity}\n",
        elapsed.as_secs_f64() * 1_000.0,
    );
    if let Err(error) = writer.write_all(summary.as_bytes()) {
        eprintln!("failed to write dropped tiny-player log summary: {error}");
    }
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

    #[test]
    fn non_blocking_log_writer_drops_instead_of_waiting_when_queue_is_full() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let make_writer = NonBlockingLogMakeWriter {
            sender,
            dropped_events: Arc::clone(&dropped_events),
        };

        {
            let mut writer = make_writer.make_writer();
            writer.write_all(b"first\n").unwrap();
        }
        {
            let mut writer = make_writer.make_writer();
            writer.write_all(b"second\n").unwrap();
        }

        let NonBlockingLogCommand::Event(event) = receiver.try_recv().unwrap() else {
            panic!("first bounded log command is an event");
        };
        assert_eq!(event, b"first\n");
        assert_eq!(dropped_events.load(Ordering::Acquire), 1);
    }

    #[test]
    fn asynchronous_log_worker_drains_before_guard_shutdown() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("tiny.log");
        let (make_writer, guard) = open_non_blocking_log_writer(&log_path).unwrap();

        {
            let mut writer = make_writer.make_writer();
            writer.write_all(b"asynchronous\n").unwrap();
        }
        drop(make_writer);
        drop(guard);

        assert_eq!(fs::read_to_string(log_path).unwrap(), "asynchronous\n");
    }
}
