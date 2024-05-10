// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Utility for sending log related messages to a storing destination or simply to stdout/stderr.
//! The logging destination is specified upon the initialization of the logging system.
//!
//! # Enabling logging
//! There are 2 ways to enable the logging functionality:
//!
//! 1) Calling `LOGGER.configure()`. This will enable the logger to work in limited mode.
//! In this mode the logger can only write messages to stdout or stderr.

//! The logger can be configured in this way any number of times before calling `LOGGER.init()`.
//!
//! 2) Calling `LOGGER.init()`. This will enable the logger to work in full mode.
//! In this mode the logger can write messages to arbitrary buffers.
//! The logger can be initialized only once. Any call to the `LOGGER.init()` following that will
//! fail with an explicit error.
//!
//! ## Example for logging to stdout/stderr
//!
//! ```
//! use std::ops::Deref;
//!
//! use logger::{error, warn, LOGGER};
//!
//! // Optionally do an initial configuration for the logger.
//! if let Err(err) = LOGGER.deref().configure(Some("MY-INSTANCE".to_string())) {
//!     println!("Could not configure the log subsystem: {}", err);
//!     return;
//! }
//! warn!("this is a warning");
//! error!("this is an error");
//! ```
//! ## Example for logging to a `File`:
//!
//! ```
//! use std::io::Cursor;
//!
//! use libc::c_char;
//! use logger::{error, warn, LOGGER};
//!
//! let mut logs = Cursor::new(vec![0; 15]);
//!
//! // Initialize the logger to log to a FIFO that was created beforehand.
//! assert!(LOGGER
//!     .init("Running Firecracker v.x".to_string(), Box::new(logs),)
//!     .is_ok());
//! // The following messages should appear in the in-memory buffer `logs`.
//! warn!("this is a warning");
//! error!("this is an error");
//! ```

//! # Plain log format
//! The current logging system is built upon the upstream crate 'log' and reexports the macros
//! provided by it for flushing plain log content. Log messages are printed through the use of five
//! macros:
//! * error!(<string>)
//! * warning!(<string>)
//! * info!(<string>)
//! * debug!(<string>)
//! * trace!(<string>)
//!
//! Each call to the desired macro will flush a line of the following format:
//! ```<timestamp> [<instance_id>:<level>:<file path>:<line number>] <log content>```.
//! The first component is always the timestamp which has the `%Y-%m-%dT%H:%M:%S.%f` format.
//! The level will depend on the macro used to flush a line and will be one of the following:
//! `ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`.
//! The file path and the line provides the exact location of where the call to the macro was made.
//! ## Example of a log line:
//! ```bash
//! 2018-11-07T05:34:25.180751152 [anonymous-instance:ERROR:vmm/src/lib.rs:1173] Failed to write
//! metrics: Failed to write logs. Error: operation would block
//! ```
//! # Limitations
//! Logs can be flushed either to stdout/stderr or to a byte-oriented sink (File, FIFO, Ring Buffer
//! etc).

use std::io::{sink, stderr, stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use std::{result, thread};

use lazy_static::lazy_static;
use log::{max_level, set_logger, set_max_level, Level, LevelFilter, Log, Metadata, Record};
use utils::time::LocalTime;

use super::extract_guard;
use crate::init;
use crate::init::Init;
use crate::metrics::{IncMetric, METRICS};

/// Type for returning functions outcome.
pub type Result<T> = result::Result<T, LoggerError>;

// Values used by the Logger.
const DEFAULT_MAX_LEVEL: LevelFilter = LevelFilter::Warn;

lazy_static! {
    static ref _LOGGER_INNER: Logger = Logger::new();

    /// Static instance used for handling human-readable logs.
    pub static ref LOGGER: &'static Logger = {
        set_logger(_LOGGER_INNER.deref()).expect("Failed to set logger");
        _LOGGER_INNER.deref()
    };
}

/// Logger representing the logging subsystem.
// All member fields have types which are Sync, and exhibit interior mutability, so
// we can call logging operations using a non-mut static global variable.
pub struct Logger {
    init: Init,
    // Human readable logs will be outputted here.
    log_buf: Mutex<Box<dyn Write + Send>>,
    show_level: AtomicBool,
    show_file_path: AtomicBool,
    show_line_numbers: AtomicBool,
    instance_id: RwLock<String>,
}

impl Logger {
    /// Creates a new instance of the current logger.
    fn new() -> Logger {
        Logger {
            init: Init::new(),
            log_buf: Mutex::new(Box::new(sink())),
            show_level: AtomicBool::new(true),
            show_line_numbers: AtomicBool::new(true),
            show_file_path: AtomicBool::new(true),
            instance_id: RwLock::new(String::new()),
        }
    }

    fn show_level(&self) -> bool {
        self.show_level.load(Ordering::Relaxed)
    }

    fn show_file_path(&self) -> bool {
        self.show_file_path.load(Ordering::Relaxed)
    }

    fn show_line_numbers(&self) -> bool {
        self.show_line_numbers.load(Ordering::Relaxed)
    }

    /// Enables or disables including the level in the log message's tag portion.
    ///
    /// # Arguments
    ///
    /// * `option` - Boolean deciding whether to include log level in log message.
    ///
    /// # Example
    ///
    /// ```
    /// use std::ops::Deref;
    ///
    /// use logger::{warn, LOGGER};
    ///
    /// let l = LOGGER.deref();
    /// l.set_include_level(true);
    /// assert!(l.configure(Some("MY-INSTANCE".to_string())).is_ok());
    /// warn!("A warning log message with level included");
    /// ```
    /// The code above will more or less print:
    /// ```bash
    /// 2018-11-07T05:34:25.180751152 [MY-INSTANCE:WARN:logger/src/lib.rs:290] A warning log
    /// message with level included
    /// ```
    pub fn set_include_level(&self, option: bool) -> &Self {
        self.show_level.store(option, Ordering::Relaxed);
        self
    }

    /// Enables or disables including the file path and the line numbers in the tag of
    /// the log message. Not including the file path will also hide the line numbers from the tag.
    ///
    /// # Arguments
    ///
    /// * `file_path` - Boolean deciding whether to include file path of the log message's origin.
    /// * `line_numbers` - Boolean deciding whether to include the line number of the file where the
    /// log message orginated.
    ///
    /// # Example
    ///
    /// ```
    /// use std::ops::Deref;
    ///
    /// use logger::{warn, LOGGER};
    ///
    /// let l = LOGGER.deref();
    /// l.set_include_origin(false, false);
    /// assert!(l.configure(Some("MY-INSTANCE".to_string())).is_ok());
    ///
    /// warn!("A warning log message with log origin disabled");
    /// ```
    /// The code above will more or less print:
    /// ```bash
    /// 2018-11-07T05:34:25.180751152 [MY-INSTANCE:WARN] A warning log message with log origin
    /// disabled
    /// ```
    pub fn set_include_origin(&self, file_path: bool, line_numbers: bool) -> &Self {
        self.show_file_path.store(file_path, Ordering::Relaxed);
        // If the file path is not shown, do not show line numbers either.
        self.show_line_numbers
            .store(file_path && line_numbers, Ordering::Relaxed);
        self
    }

    /// Sets the ID for this logger session.
    pub fn set_instance_id(&self, instance_id: String) -> &Self {
        let mut guard = extract_guard(self.instance_id.write());
        *guard = instance_id;
        self
    }

    /// Explicitly sets the max log level for the Logger.
    /// The default level is WARN. So, ERROR and WARN statements will be shown (i.e. all that is
    /// bigger than the level code).
    ///
    /// # Arguments
    ///
    /// * `level` - Set the highest log level.
    /// # Example
    ///
    /// ```
    /// use std::ops::Deref;
    ///
    /// use logger::{info, warn, LOGGER};
    ///
    /// let l = LOGGER.deref();
    /// l.set_max_level(log::LevelFilter::Warn);
    /// assert!(l.configure(Some("MY-INSTANCE".to_string())).is_ok());
    /// info!("An informational log message");
    /// warn!("A test warning message");
    /// ```
    /// The code above will more or less print:
    /// ```bash
    /// 2018-11-07T05:34:25.180751152 [MY-INSTANCE:INFO:logger/src/lib.rs:389] A test warning
    /// message
    /// ```
    pub fn set_max_level(&self, level: LevelFilter) -> &Self {
        set_max_level(level);
        self
    }

    /// Get the current thread's name.
    fn get_thread_name(&self) -> String {
        thread::current().name().unwrap_or("-").to_string()
    }

    /// Creates the first portion (to the left of the separator)
    /// of the log statement based on the logger settings.
    fn create_prefix(&self, record: &Record) -> String {
        let mut prefix: Vec<String> = vec![];

        let instance_id = extract_guard(self.instance_id.read());
        if !instance_id.is_empty() {
            prefix.push(instance_id.to_string());
        }

        // Attach current thread name to prefix.
        prefix.push(self.get_thread_name());

        if self.show_level() {
            prefix.push(record.level().to_string());
        };

        if self.show_file_path() {
            prefix.push(record.file().unwrap_or("unknown").to_string());
        };

        if self.show_line_numbers() {
            if let Some(line) = record.line() {
                prefix.push(line.to_string());
            }
        }

        format!("[{}]", prefix.join(":"))
    }

    /// if the max level hasn't been configured yet, set it to default
    fn try_init_max_level(&self) {
        // if the max level hasn't been configured yet, set it to default
        if max_level() == LevelFilter::Off {
            self.set_max_level(DEFAULT_MAX_LEVEL);
        }
    }

    /// Preconfigure the logger prior to initialization.
    /// Performs the most basic steps in order to enable the logger to write to stdout or stderr
    /// even before calling LOGGER.init(). Calling this method is optional.
    /// This function can be called any number of times before the initialization.
    /// Any calls made after the initialization will result in `Err()`.
    ///
    /// # Arguments
    ///
    /// * `instance_id` - Unique string identifying this logger session. This id is temporary and
    ///   will be overwritten upon initialization.
    ///
    /// # Example
    ///
    /// ```
    /// use std::ops::Deref;
    ///
    /// use logger::LOGGER;
    ///
    /// LOGGER
    ///     .deref()
    ///     .configure(Some("MY-INSTANCE".to_string()))
    ///     .unwrap();
    /// ```
    pub fn configure(&self, instance_id: Option<String>) -> Result<()> {
        self.init
            .call_init(|| {
                if let Some(some_instance_id) = instance_id {
                    self.set_instance_id(some_instance_id);
                }

                self.try_init_max_level();

                // don't finish the initialization
                false
            })
            .map_err(LoggerError::Init)
    }

    /// Initialize log system (once and only once).
    /// Every call made after the first will have no effect besides returning `Ok` or `Err`.
    ///
    /// # Arguments
    ///
    /// * `header` - Info about the app that uses the logger.
    /// * `log_dest` - Buffer for plain text logs. Needs to implements `Write` and `Send`.
    ///
    /// # Example
    ///
    /// ```
    /// use std::io::Cursor;
    ///
    /// use logger::LOGGER;
    ///
    /// let mut logs = Cursor::new(vec![0; 15]);
    ///
    /// LOGGER.init("Running Firecracker v.x".to_string(), Box::new(logs));
    /// ```
    pub fn init(&self, header: String, log_dest: Box<dyn Write + Send>) -> Result<()> {
        self.init
            .call_init(|| {
                let mut g = extract_guard(self.log_buf.lock());
                *g = log_dest;

                self.try_init_max_level();

                // finish init
                true
            })
            .map_err(LoggerError::Init)?;

        self.write_log(header, Level::Info);

        Ok(())
    }

    /// Handles the common logic of writing regular log messages.
    ///
    /// Writes `msg` followed by a newline to the destination, flushing afterwards.
    fn write_log(&self, msg: String, msg_level: Level) {
        let mut guard;
        let mut writer: Box<dyn Write> = if self.init.is_initialized() {
            guard = extract_guard(self.log_buf.lock());
            Box::new(guard.as_mut())
        } else {
            match msg_level {
                Level::Error | Level::Warn => Box::new(stderr()),
                _ => Box::new(stdout()),
            }
        };
        // Writes `msg` followed by newline and flushes, if either operation returns an error,
        // increment missed log count.
        // This approach is preferable over `Result::and` as if `write!` returns  an error it then
        // does not attempt to flush.
        if writeln!(writer, "{}", msg)
            .and_then(|_| writer.flush())
            .is_err()
        {
            // No reason to log the error to stderr here, just increment the metric.
            METRICS.logger.missed_log_count.inc();
        }
    }
}

/// Describes the errors which may occur while handling logging scenarios.
#[derive(Debug, thiserror::Error)]
pub enum LoggerError {
    /// Initialization Error.
    #[error("Logger initialization failure: {0}")]
    Init(init::Error),
}

/// Implements the "Log" trait from the externally used "log" crate.
impl Log for Logger {
    // This is currently not used.
    fn enabled(&self, _metadata: &Metadata) -> bool {
        unreachable!();
    }

    fn log(&self, record: &Record) {
        let msg = format!(
            "{} {} {}",
            LocalTime::now(),
            self.create_prefix(record),
            record.args()
        );
        self.write_log(msg, record.metadata().level());
    }

    // This is currently not used.
    fn flush(&self) {
        unreachable!();
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{create_dir, read_to_string, remove_dir, remove_file, OpenOptions};
    use std::io::{BufWriter, Read, Write};
    use std::sync::Arc;

    use log::info;

    use super::*;

    const TEST_INSTANCE_ID: &str = "TEST-INSTANCE-ID";
    const TEST_APP_HEADER: &str = "App header";

    const LOG_SOURCE: &str = "logger.rs";
    const LOG_LINE: u32 = 0;

    struct LogWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut data = self.buf.lock().unwrap();
            data.append(&mut buf.to_vec());

            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct LogReader {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl Read for LogReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut data = self.buf.lock().unwrap();

            let len = std::cmp::min(data.len(), buf.len());
            buf[..len].copy_from_slice(&data[..len]);

            data.drain(..len);

            Ok(len)
        }
    }

    fn log_channel() -> (LogWriter, LogReader) {
        let buf = Arc::new(Mutex::new(vec![]));
        (LogWriter { buf: buf.clone() }, LogReader { buf })
    }

    impl Logger {
        fn mock_new() -> Logger {
            let logger = Logger::new();
            logger.set_instance_id(TEST_INSTANCE_ID.to_string());

            logger
        }

        fn mock_log(&self, level: Level, msg: &str) {
            self.log(
                &log::Record::builder()
                    .level(level)
                    .args(format_args!("{}", msg))
                    .file(Some(LOG_SOURCE))
                    .line(Some(LOG_LINE))
                    .build(),
            );
        }

        fn mock_init(&self) -> LogReader {
            let (writer, mut reader) = log_channel();
            assert!(self
                .init(TEST_APP_HEADER.to_string(), Box::new(writer))
                .is_ok());
            validate_log(
                &mut Box::new(&mut reader),
                &format!("{}\n", TEST_APP_HEADER),
            );

            reader
        }
    }

    fn validate_log(log_reader: &mut dyn Read, expected: &str) {
        let mut log = Vec::new();
        log_reader.read_to_end(&mut log).unwrap();

        assert!(log.len() >= expected.len());
        assert_eq!(
            expected,
            std::str::from_utf8(&log[log.len() - expected.len()..]).unwrap()
        );
    }

    #[test]
    fn test_default_values() {
        let l = Logger::new();
        assert!(l.show_line_numbers());
        assert!(l.show_level());
    }

    #[test]
    fn test_write_log() {
        // Data to log to file for test.
        const TEST_HEADER: &str = "test_log";
        const TEST_STR: &str = "testing flushing";
        // File to use for test.
        const TEST_DIR: &str = "./tmp";
        const TEST_FILE: &str = "test.txt";
        let test_path = format!("{}/{}", TEST_DIR, TEST_FILE);

        // Creates ./tmp directory
        create_dir(TEST_DIR).unwrap();
        // A buffered writer to a file
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&test_path)
            .unwrap();
        let writer = Box::new(BufWriter::new(file));
        // Create a logger with this buffered writer as the `dest`.
        let logger = Logger::new();
        logger.init(String::from(TEST_HEADER), writer).unwrap();
        // Log some generic data
        logger.write_log(String::from(TEST_STR), Level::Info);
        // To drop the logger without calling its destructor, or to `forget` it
        // (https://doc.rust-lang.org/stable/std/mem/fn.forget.html) will lead
        // to a memory leak, so for this test I do not do this.
        // As such this test simply illustrates the `write_log` function will
        // always flush such that in the occurrence of the crash the expected
        // behavior that all temporally distant logs from a crash are flushed.

        // Read from the log file.
        let file_contents = read_to_string(&test_path).unwrap();
        // Asserts the contents of the log file are as expected.
        assert_eq!(file_contents, format!("{}\n{}\n", TEST_HEADER, TEST_STR));
        // Removes the log file.
        remove_file(&test_path).unwrap();
        // Removes /tmp directory
        remove_dir(TEST_DIR).unwrap();
    }

    #[test]
    fn test_configure() {
        let logger = Logger::new();
        let crnt_thread_name = logger.get_thread_name();

        // Assert that `configure()` can be called successfully any number of times.
        assert!(logger.configure(Some(TEST_INSTANCE_ID.to_string())).is_ok());
        assert!(logger.configure(None).is_ok());
        assert!(logger.configure(Some(TEST_INSTANCE_ID.to_string())).is_ok());

        // Assert that `init()` works after `configure()`
        let (writer, mut reader) = log_channel();
        assert!(logger
            .init(TEST_APP_HEADER.to_string(), Box::new(writer))
            .is_ok());
        validate_log(
            &mut Box::new(&mut reader),
            &format!("{}\n", TEST_APP_HEADER),
        );
        // Check that the logs are written to the configured writer.
        logger.mock_log(Level::Info, "info");
        validate_log(
            &mut Box::new(&mut reader),
            &format!(
                "[TEST-INSTANCE-ID:{}:INFO:logger.rs:0] info\n",
                crnt_thread_name
            ),
        );
    }

    #[test]
    fn test_init() {
        let logger = Logger::new();
        let crnt_thread_name = logger.get_thread_name();
        // Assert that the first call to `init()` is successful.
        let (writer, mut reader) = log_channel();
        logger.set_instance_id(TEST_INSTANCE_ID.to_string());
        assert!(logger
            .init(TEST_APP_HEADER.to_string(), Box::new(writer))
            .is_ok());
        validate_log(
            &mut Box::new(&mut reader),
            &format!("{}\n", TEST_APP_HEADER),
        );
        // Check that the logs are written to the configured writer.
        logger.mock_log(Level::Info, "info");
        validate_log(
            &mut Box::new(&mut reader),
            &format!(
                "[TEST-INSTANCE-ID:{}:INFO:logger.rs:0] info\n",
                crnt_thread_name
            ),
        );

        // Assert that initialization works only once.
        let (writer_2, mut reader_2) = log_channel();
        assert!(logger
            .init(TEST_APP_HEADER.to_string(), Box::new(writer_2))
            .is_err());
        // Check that the logs are written only to the first writer.
        logger.mock_log(Level::Info, "info");
        validate_log(
            &mut Box::new(&mut reader),
            &format!(
                "[TEST-INSTANCE-ID:{}:INFO:logger.rs:0] info\n",
                crnt_thread_name
            ),
        );
        validate_log(&mut Box::new(&mut reader_2), "");
    }

    #[test]
    fn test_create_prefix() {
        let logger = Logger::mock_new();
        let mut reader = logger.mock_init();
        let crnt_thread_name = logger.get_thread_name();
        // Test with empty instance id.
        logger.set_instance_id("".to_string());

        // Check that the prefix is empty when `show_level` and `show_origin` are false.
        logger
            .set_include_level(false)
            .set_include_origin(false, true);
        logger.mock_log(Level::Info, "msg");
        validate_log(
            &mut Box::new(&mut reader),
            &format!("[{}] msg\n", crnt_thread_name),
        );

        // Check that the prefix is correctly shown when all flags are true.
        logger
            .set_include_level(true)
            .set_include_origin(true, true);
        logger.mock_log(Level::Info, "msg");
        validate_log(
            &mut Box::new(&mut reader),
            &format!("[{}:INFO:logger.rs:0] msg\n", crnt_thread_name),
        );

        // Check show_line_numbers=false.
        logger
            .set_include_level(true)
            .set_include_origin(true, false);
        logger.mock_log(Level::Debug, "msg");
        validate_log(
            &mut Box::new(&mut reader),
            &format!("[{}:DEBUG:logger.rs] msg\n", crnt_thread_name),
        );

        // Check show_file_path=false.
        logger
            .set_include_level(true)
            .set_include_origin(false, true);
        logger.mock_log(Level::Error, "msg");
        validate_log(
            &mut Box::new(&mut reader),
            &format!("[{}:ERROR] msg\n", crnt_thread_name),
        );

        // Check show_level=false.
        logger
            .set_include_level(false)
            .set_include_origin(true, true);
        logger.mock_log(Level::Info, "msg");
        validate_log(
            &mut Box::new(&mut reader),
            &format!("[{}:logger.rs:0] msg\n", crnt_thread_name),
        );

        // Test with a mock instance id.
        logger.set_instance_id(TEST_INSTANCE_ID.to_string());

        // Check that the prefix contains only the instance id when all flags are false.
        logger
            .set_include_level(false)
            .set_include_origin(false, false);
        logger.mock_log(Level::Info, "msg");
        validate_log(
            &mut Box::new(&mut reader),
            &format!("[TEST-INSTANCE-ID:{}] msg\n", crnt_thread_name),
        );

        // Check that the prefix is correctly shown when all flags are true.
        logger
            .set_include_level(true)
            .set_include_origin(true, true);
        logger.mock_log(Level::Warn, "msg");
        validate_log(
            &mut Box::new(&mut reader),
            &format!(
                "[TEST-INSTANCE-ID:{}:WARN:logger.rs:0] msg\n",
                crnt_thread_name
            ),
        );
    }

    #[test]
    fn test_thread_name_custom() {
        let custom_thread = thread::Builder::new()
            .name("custom-thread".to_string())
            .spawn(move || {
                let logger = Logger::mock_new();
                let mut reader = logger.mock_init();
                logger
                    .set_include_level(false)
                    .set_include_origin(false, false);
                logger.set_instance_id("".to_string());
                logger.mock_log(Level::Info, "thread-msg");
                validate_log(&mut Box::new(&mut reader), "[custom-thread] thread-msg\n");
            })
            .unwrap();
        let r = custom_thread.join();
        assert!(r.is_ok());
    }

    #[test]
    fn test_static_logger() {
        log::set_max_level(log::LevelFilter::Info);
        LOGGER.set_instance_id(TEST_INSTANCE_ID.to_string());

        let mut reader = LOGGER.mock_init();

        info!("info");
        validate_log(&mut Box::new(&mut reader), "info\n");
    }

    #[test]
    fn test_error_messages() {
        assert_eq!(
            format!("{}", LoggerError::Init(init::Error::AlreadyInitialized)),
            "Logger initialization failure: The component is already initialized."
        );
    }
}
