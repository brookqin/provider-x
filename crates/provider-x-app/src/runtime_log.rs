use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{Days, Local, NaiveDate, SecondsFormat};
use provider_x_egress::{EgressEvent, EgressObserver};
use serde::Serialize;
use thiserror::Error;

use crate::storage::atomic_file::{ensure_private_directory, validate_regular_file};

const RETAINED_DAYS: u64 = 10;
const LOG_CHANNEL_CAPACITY: usize = 1_024;
const LOG_PREFIX: &str = "provider-x-";
const LOG_SUFFIX: &str = ".log";

#[derive(Debug, Error)]
pub(crate) enum RuntimeLogError {
    #[error(transparent)]
    SecureFile(#[from] crate::storage::SecureFileError),

    #[error("runtime log I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to serialize a runtime log record: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("failed to start the runtime log worker: {0}")]
    Worker(#[source] std::io::Error),
}

#[derive(Debug, Serialize)]
#[serde(tag = "component", rename_all = "snake_case")]
enum LogEntry {
    Egress {
        #[serde(flatten)]
        event: EgressEvent,
    },
    Runtime {
        #[serde(skip)]
        level: LogLevel,
        code: String,
        message: String,
    },
}

#[derive(Clone, Copy, Debug)]
enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl LogEntry {
    const fn level(&self) -> LogLevel {
        match self {
            Self::Egress {
                event: EgressEvent::RequestObserved(_) | EgressEvent::UpstreamObserved(_),
            } => LogLevel::Info,
            Self::Egress {
                event: EgressEvent::FallbackObserved(_),
            } => LogLevel::Warn,
            Self::Egress {
                event: EgressEvent::ErrorObserved(_),
            } => LogLevel::Error,
            Self::Runtime { level, .. } => *level,
        }
    }
}

struct LogContext {
    app_version: &'static str,
    process_id: u32,
    run_id: String,
}

impl LogContext {
    fn current() -> Self {
        let process_id = std::process::id();
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        Self {
            app_version: env!("CARGO_PKG_VERSION"),
            process_id,
            run_id: format!("{process_id}-{started_at}"),
        }
    }
}

#[derive(Serialize)]
struct LogRecord<'a> {
    timestamp: &'a str,
    level: &'static str,
    app_version: &'static str,
    process_id: u32,
    run_id: &'a str,
    #[serde(flatten)]
    entry: &'a LogEntry,
}

pub(crate) struct RuntimeLog {
    sender: Mutex<Option<mpsc::SyncSender<LogEntry>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    dropped: Arc<AtomicU64>,
}

impl RuntimeLog {
    pub(crate) fn start(directory: &Path) -> Result<Arc<Self>, RuntimeLogError> {
        let writer = DailyLogWriter::new(directory, Local::now().date_naive())?;
        let (sender, receiver) = mpsc::sync_channel(LOG_CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let worker = thread::Builder::new()
            .name("provider-x-log".to_owned())
            .spawn(move || run_worker(writer, &receiver, &worker_dropped))
            .map_err(RuntimeLogError::Worker)?;
        let logger = Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            dropped,
        });
        logger.send(LogEntry::Runtime {
            level: LogLevel::Info,
            code: "app_started".to_owned(),
            message: "ProviderX runtime logging started".to_owned(),
        });
        Ok(logger)
    }

    pub(crate) fn record_runtime_error(&self, code: &str, message: &str) {
        self.send(LogEntry::Runtime {
            level: LogLevel::Error,
            code: code.to_owned(),
            message: message.to_owned(),
        });
    }

    fn send(&self, entry: LogEntry) {
        let Ok(sender) = self.sender.lock() else {
            return;
        };
        if let Some(sender) = sender.as_ref() {
            if matches!(sender.try_send(entry), Err(mpsc::TrySendError::Full(_))) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl EgressObserver for RuntimeLog {
    fn record(&self, event: EgressEvent) {
        self.send(LogEntry::Egress { event });
    }
}

impl Drop for RuntimeLog {
    fn drop(&mut self) {
        self.sender
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn run_worker(
    mut writer: DailyLogWriter,
    receiver: &mpsc::Receiver<LogEntry>,
    dropped: &AtomicU64,
) {
    while let Ok(entry) = receiver.recv() {
        write_dropped_marker(&mut writer, dropped);
        write_entry(&mut writer, &entry);
    }
    write_dropped_marker(&mut writer, dropped);
}

fn write_dropped_marker(writer: &mut DailyLogWriter, dropped: &AtomicU64) {
    let dropped = dropped.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        write_entry(
            writer,
            &LogEntry::Runtime {
                level: LogLevel::Error,
                code: "log_events_dropped".to_owned(),
                message: format!("{dropped} diagnostic log events were dropped during a burst"),
            },
        );
    }
}

fn write_entry(writer: &mut DailyLogWriter, entry: &LogEntry) {
    let now = Local::now();
    let timestamp = now.to_rfc3339_opts(SecondsFormat::Millis, false);
    if let Err(error) = writer.write(now.date_naive(), &timestamp, entry) {
        eprintln!("runtime log write failed: {error}");
    }
}

struct DailyLogWriter {
    directory: PathBuf,
    date: NaiveDate,
    file: File,
    context: LogContext,
}

impl DailyLogWriter {
    fn new(directory: &Path, date: NaiveDate) -> Result<Self, RuntimeLogError> {
        ensure_private_directory(directory)?;
        cleanup_old_logs(directory, date)?;
        let file = open_log_file(&log_path(directory, date))?;
        Ok(Self {
            directory: directory.to_path_buf(),
            date,
            file,
            context: LogContext::current(),
        })
    }

    fn write(
        &mut self,
        date: NaiveDate,
        timestamp: &str,
        entry: &LogEntry,
    ) -> Result<(), RuntimeLogError> {
        if date != self.date {
            cleanup_old_logs(&self.directory, date)?;
            self.file = open_log_file(&log_path(&self.directory, date))?;
            self.date = date;
        }
        let mut bytes = serde_json::to_vec(&LogRecord {
            timestamp,
            level: entry.level().as_str(),
            app_version: self.context.app_version,
            process_id: self.context.process_id,
            run_id: &self.context.run_id,
            entry,
        })?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .map_err(|source| log_io_error(log_path(&self.directory, self.date), source))
    }
}

fn log_path(directory: &Path, date: NaiveDate) -> PathBuf {
    directory.join(format!("{LOG_PREFIX}{date}{LOG_SUFFIX}"))
}

fn open_log_file(path: &Path) -> Result<File, RuntimeLogError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_regular_file(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = OpenOptions::new()
                .append(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|source| log_io_error(path.to_path_buf(), source))?;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| log_io_error(path.to_path_buf(), source))?;
            validate_regular_file(
                path,
                &file
                    .metadata()
                    .map_err(|source| log_io_error(path.to_path_buf(), source))?,
            )?;
            return Ok(file);
        }
        Err(source) => return Err(log_io_error(path.to_path_buf(), source)),
    }

    let file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|source| log_io_error(path.to_path_buf(), source))?;
    validate_regular_file(
        path,
        &file
            .metadata()
            .map_err(|source| log_io_error(path.to_path_buf(), source))?,
    )?;
    Ok(file)
}

fn cleanup_old_logs(directory: &Path, today: NaiveDate) -> Result<(), RuntimeLogError> {
    let cutoff = today
        .checked_sub_days(Days::new(RETAINED_DAYS - 1))
        .unwrap_or(NaiveDate::MIN);
    let entries =
        fs::read_dir(directory).map_err(|source| log_io_error(directory.to_path_buf(), source))?;
    for entry in entries {
        let entry = entry.map_err(|source| log_io_error(directory.to_path_buf(), source))?;
        let path = entry.path();
        let Some(date) = log_date(&path) else {
            continue;
        };
        if date >= cutoff {
            continue;
        }
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| log_io_error(path.clone(), source))?;
        validate_regular_file(&path, &metadata)?;
        fs::remove_file(&path).map_err(|source| log_io_error(path, source))?;
    }
    Ok(())
}

fn log_date(path: &Path) -> Option<NaiveDate> {
    let file_name = path.file_name()?.to_str()?;
    let date = file_name
        .strip_prefix(LOG_PREFIX)?
        .strip_suffix(LOG_SUFFIX)?;
    if date.len() != 10 {
        return None;
    }
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

fn log_io_error(path: PathBuf, source: std::io::Error) -> RuntimeLogError {
    RuntimeLogError::Io { path, source }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::{Arc, atomic::AtomicU64},
    };

    use chrono::{Local, NaiveDate};
    use provider_x_egress::{
        EgressEvent, EgressObserver, ErrorObserved, FallbackObserved, ObservedRoute,
        ObservedTransport, RequestObserved, UpstreamObserved,
    };

    use super::{
        DailyLogWriter, LogEntry, LogLevel, RuntimeLog, RuntimeLogError, log_path, open_log_file,
        write_dropped_marker,
    };

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn rotates_by_local_date_and_retains_today_plus_nine_days() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("logs");
        let mut writer = DailyLogWriter::new(&directory, date("2026-08-01")).unwrap();
        writer
            .write(
                date("2026-08-01"),
                "2026-08-01T23:59:59+08:00",
                &LogEntry::Runtime {
                    level: LogLevel::Info,
                    code: "first".to_owned(),
                    message: "first day".to_owned(),
                },
            )
            .unwrap();
        writer
            .write(
                date("2026-08-02"),
                "2026-08-02T00:00:01+08:00",
                &LogEntry::Runtime {
                    level: LogLevel::Info,
                    code: "cutoff".to_owned(),
                    message: "oldest retained day".to_owned(),
                },
            )
            .unwrap();
        writer
            .write(
                date("2026-08-11"),
                "2026-08-11T00:00:01+08:00",
                &LogEntry::Runtime {
                    level: LogLevel::Info,
                    code: "second".to_owned(),
                    message: "second day".to_owned(),
                },
            )
            .unwrap();

        assert!(!log_path(&directory, date("2026-08-01")).exists());
        assert!(log_path(&directory, date("2026-08-02")).is_file());
        assert!(log_path(&directory, date("2026-08-11")).is_file());
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(log_path(&directory, date("2026-08-11")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn production_observer_writes_correlated_redacted_diagnostic_events() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("logs");
        let logger = RuntimeLog::start(&directory).unwrap();
        let observer: Arc<dyn EgressObserver> = logger.clone();
        observer.record(EgressEvent::RequestObserved(RequestObserved {
            transport: ObservedTransport::WebSocket,
            path: "/v1/responses".to_owned(),
            session_id: Some(17),
            sequence: 1,
            model: "provider-a/coder".to_owned(),
            route: ObservedRoute::ThirdParty {
                provider_id: "provider-a".to_owned(),
            },
            previous_response_id_present: false,
            client_metadata_present: true,
            codex_turn_metadata_header_present: true,
        }));
        observer.record(EgressEvent::UpstreamObserved(UpstreamObserved {
            transport: ObservedTransport::Http,
            session_id: Some(17),
            route: ObservedRoute::ThirdParty {
                provider_id: "provider-a".to_owned(),
            },
            status: 200,
        }));
        observer.record(EgressEvent::FallbackObserved(FallbackObserved {
            transport: ObservedTransport::WebSocket,
            path: "/v1/responses".to_owned(),
            status: 426,
        }));
        observer.record(EgressEvent::ErrorObserved(ErrorObserved {
            transport: ObservedTransport::Http,
            method: "POST".to_owned(),
            path: "/v1/responses".to_owned(),
            ingress_authorized: true,
            status: Some(502),
            code: "upstream_connect_failed".to_owned(),
            message: "failed to connect to upstream".to_owned(),
            websocket: None,
        }));
        drop(observer);
        drop(logger);

        let content = fs::read_to_string(log_path(&directory, Local::now().date_naive())).unwrap();
        assert!(content.contains("upstream_connect_failed"));
        assert!(content.contains("\"level\":\"error\""));
        assert!(content.contains("\"level\":\"info\""));
        assert!(content.contains("\"level\":\"warn\""));
        assert!(content.contains("\"app_version\":\""));
        assert!(content.contains("\"run_id\":\""));
        assert!(content.contains("\"session_id\":17"));
        assert_eq!(content.lines().count(), 5);
        assert!(!content.contains("authorization"));
        assert!(!content.contains("request_body"));
        assert!(!content.contains("response_body"));
    }

    #[test]
    fn rejects_symbolic_and_hard_link_log_files() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.log");
        fs::write(&target, b"existing").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let path = root.path().join("provider-x-2026-08-15.log");

        symlink(&target, &path).unwrap();
        assert!(matches!(
            open_log_file(&path),
            Err(RuntimeLogError::SecureFile(
                crate::storage::SecureFileError::SymbolicLink(_)
            ))
        ));
        fs::remove_file(&path).unwrap();

        fs::hard_link(&target, &path).unwrap();
        assert!(matches!(
            open_log_file(&path),
            Err(RuntimeLogError::SecureFile(
                crate::storage::SecureFileError::UnexpectedHardLinks { .. }
            ))
        ));
    }

    #[test]
    fn reports_burst_events_dropped_by_the_bounded_channel() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("logs");
        let today = Local::now().date_naive();
        let mut writer = DailyLogWriter::new(&directory, today).unwrap();
        let dropped = AtomicU64::new(3);

        write_dropped_marker(&mut writer, &dropped);
        drop(writer);

        let content = fs::read_to_string(log_path(&directory, today)).unwrap();
        assert!(content.contains("log_events_dropped"));
        assert!(content.contains("3 diagnostic log events"));
    }
}
