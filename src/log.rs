//! Minimal levelled logging to stderr.
//!
//! Under systemd (`JOURNAL_STREAM` is set) each line starts with an sd-daemon
//! `<N>` priority prefix, so journald stores the right level and
//! `journalctl -p warning` works. Otherwise lines get a readable level label.

use std::{
    env, fmt,
    io::{self, Write},
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

/// Message severity, numbered like syslog priorities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Level {
    /// Something failed and needs attention.
    Error = 3,
    /// Something is wrong but handled (e.g. fail-safe engaged).
    Warn = 4,
    /// Normal operation (duty changes).
    Info = 6,
    /// Per-tick detail.
    Debug = 7,
}

impl Level {
    const fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static JOURNAL: OnceLock<bool> = OnceLock::new();

/// Sets the most verbose level that is printed (default [`Level::Info`]).
pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Whether messages at `level` are currently printed.
#[must_use]
pub fn enabled(level: Level) -> bool {
    level as u8 <= MAX_LEVEL.load(Ordering::Relaxed)
}

/// Logs `args` at `level`. Write errors are ignored: logging must never stop fan control.
pub fn log(level: Level, args: fmt::Arguments<'_>) {
    if enabled(level) {
        let journal = *JOURNAL.get_or_init(|| env::var_os("JOURNAL_STREAM").is_some());
        let _ = writeln!(io::stderr().lock(), "{}", format_line(level, args, journal));
    }
}

/// Logs at [`Level::Error`].
pub fn error(args: fmt::Arguments<'_>) {
    log(Level::Error, args);
}

/// Logs at [`Level::Warn`].
pub fn warn(args: fmt::Arguments<'_>) {
    log(Level::Warn, args);
}

/// Logs at [`Level::Info`].
pub fn info(args: fmt::Arguments<'_>) {
    log(Level::Info, args);
}

/// Logs at [`Level::Debug`].
pub fn debug(args: fmt::Arguments<'_>) {
    log(Level::Debug, args);
}

fn format_line(level: Level, args: fmt::Arguments<'_>, journal: bool) -> String {
    if journal {
        format!("<{}>{args}", level as u8)
    } else {
        format!("{:<5} {args}", level.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_lines_carry_priority_prefix() {
        assert_eq!(format_line(Level::Error, format_args!("x {}", 1), true), "<3>x 1");
        assert_eq!(format_line(Level::Warn, format_args!("w"), true), "<4>w");
        assert_eq!(format_line(Level::Info, format_args!("fan 65%"), true), "<6>fan 65%");
        assert_eq!(format_line(Level::Debug, format_args!("d"), true), "<7>d");
    }

    #[test]
    fn terminal_lines_carry_label() {
        assert_eq!(format_line(Level::Info, format_args!("fan 65%"), false), "INFO  fan 65%");
        assert_eq!(format_line(Level::Error, format_args!("bad"), false), "ERROR bad");
    }

    #[test]
    fn levels_order_by_severity() {
        assert!(Level::Error < Level::Warn && Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
    }

    #[test]
    fn max_level_filters() {
        // Global state: keep all assertions about it in this one test.
        assert!(enabled(Level::Info));
        assert!(!enabled(Level::Debug));
        set_max_level(Level::Debug);
        assert!(enabled(Level::Debug));
        set_max_level(Level::Warn);
        assert!(!enabled(Level::Info) && enabled(Level::Error));
        set_max_level(Level::Info);
    }
}
