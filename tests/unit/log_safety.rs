use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context;

use super::*;

/// Collects the formatted debug events emitted while `work` runs on this thread, without
/// colors, for the event-field tests of the logging modules.
pub(crate) fn captured_events(work: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, work);
    let bytes = buffer.0.lock().expect("buffer lock").clone();
    String::from_utf8(bytes).expect("UTF-8 log output")
}

/// The value of the `name=value` field of one formatted event line.
pub(crate) fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix(name)?.strip_prefix('='))
}

const SENSITIVE_PATH: &str = "/home/synthetic-user/private-project/com/example/Secret.class";

#[test]
fn io_errors_map_to_their_kind_without_the_message() {
    let cases = [
        (io::ErrorKind::NotFound, "io_not_found"),
        (io::ErrorKind::PermissionDenied, "io_permission_denied"),
        (io::ErrorKind::InvalidData, "io_invalid_data"),
        (io::ErrorKind::UnexpectedEof, "io_invalid_data"),
        (io::ErrorKind::TimedOut, "io_timed_out"),
        (io::ErrorKind::Interrupted, "io"),
    ];
    for (kind, expected) in cases {
        let error = anyhow::Error::new(io::Error::new(kind, SENSITIVE_PATH));
        assert_eq!(error_category(&error), expected);
    }
}

#[test]
fn context_messages_with_paths_are_never_the_category() {
    let error = Err::<(), _>(io::Error::from(io::ErrorKind::NotFound))
        .with_context(|| format!("cannot open {SENSITIVE_PATH}"))
        .expect_err("synthetic failure");
    assert_eq!(error_category(&error), "io_not_found");

    let error = anyhow::anyhow!("invalid archive {SENSITIVE_PATH}");
    assert_eq!(error_category(&error), "other");
}

#[test]
fn library_errors_map_to_closed_categories() {
    let zip = anyhow::Error::new(zip::result::ZipError::FileNotFound).context(SENSITIVE_PATH);
    assert_eq!(error_category(&zip), "archive_format");

    let sqlite = anyhow::Error::new(rusqlite::Error::QueryReturnedNoRows);
    assert_eq!(error_category(&sqlite), "sqlite");

    let json = serde_json::from_str::<u8>("not json").expect_err("invalid JSON");
    assert_eq!(error_category(&anyhow::Error::new(json)), "encoding");

    let cancelled = anyhow::Error::new(crate::blocking::Cancelled);
    assert_eq!(error_category(&cancelled), "cancelled");

    let class = cafebabe::parse_class(&[0, 1, 2]).expect_err("invalid class file");
    assert_eq!(error_category(&anyhow::Error::new(class)), "class_format");

    let walk = walkdir::WalkDir::new("/nonexistent/synthetic-walk-root")
        .into_iter()
        .find_map(Result::err)
        .expect("missing walk root");
    assert_eq!(error_category(&anyhow::Error::new(walk)), "directory_walk");
}

#[test]
fn elapsed_milliseconds_are_whole_and_non_negative() {
    let started = Instant::now()
        .checked_sub(Duration::from_millis(25))
        .expect("monotonic clock has 25 ms of history");
    assert!(elapsed_ms(started) >= 25);
    assert_eq!(elapsed_ms(Instant::now() + Duration::from_secs(60)), 0);
}
