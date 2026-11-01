//! Stable, non-sensitive values for diagnostic events.
//!
//! An error's `Display` text can quote an absolute path, an archive or class name or a
//! fragment of the content being read, so tracing events never record it. They record the
//! closed category of the first recognizable error in the cause chain instead.

use std::{io, time::Instant};

/// The category of `error`, derived from the type of the first recognizable cause.
pub(crate) fn error_category(error: &anyhow::Error) -> &'static str {
    error.chain().find_map(cause_category).unwrap_or("other")
}

fn cause_category(cause: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    if let Some(error) = cause.downcast_ref::<io::Error>() {
        return Some(io_error_category(error.kind()));
    }
    if cause.is::<crate::blocking::Cancelled>() {
        return Some("cancelled");
    }
    if cause.is::<zip::result::ZipError>() {
        return Some("archive_format");
    }
    if cause.is::<cafebabe::ParseError>() {
        return Some("class_format");
    }
    if cause.is::<rusqlite::Error>() {
        return Some("sqlite");
    }
    if cause.is::<serde_json::Error>() {
        return Some("encoding");
    }
    if cause.is::<walkdir::Error>() {
        return Some("directory_walk");
    }
    None
}

/// The category of an I/O failure; the OS message is never part of it.
pub(crate) fn io_error_category(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "io_not_found",
        io::ErrorKind::PermissionDenied => "io_permission_denied",
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => "io_invalid_data",
        io::ErrorKind::TimedOut => "io_timed_out",
        _ => "io",
    }
}

/// Whole milliseconds since `started`, saturating instead of wrapping.
pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "../tests/unit/log_safety.rs"]
pub(crate) mod tests;
