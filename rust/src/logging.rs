//! Leveled diagnostic logging with three levels: `off`, `info` and `debug`.
//!
//! Records are single-line and key=value ordered, so they stay readable in the
//! preserved lab evidence and remain greppable without a parser:
//!
//! ```text
//! info event=serve_config max_request_bytes=131072 depth=32
//! debug event=request tag=3 op=write start_sector=4096 nr_sectors=32
//! ```
//!
//! `info` carries lifecycle and aggregate events only, so it costs nothing per
//! request. `debug` adds one record per request and per log write, so it is
//! opt-in: it perturbs timing and must never be enabled during measurement.

use std::env;
use std::fmt::Arguments;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU8, Ordering};

/// Environment variable that selects the active level.
pub const LEVEL_VARIABLE: &str = "BLOCK_STORAGE_LOG";

/// Diagnostic verbosity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Level {
    /// Emit nothing.
    Off = 0,
    /// Lifecycle and aggregate events; no per-request cost.
    Info = 1,
    /// Per-request and per-record detail; perturbs timing.
    Debug = 2,
}

impl Level {
    /// Parses the accepted spellings, which are the level names themselves.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "off" => Some(Self::Off),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }

    /// The name used both in records and in `BLOCK_STORAGE_LOG`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

/// Decides whether `wanted` is emitted when `active` is selected.
#[must_use]
const fn permits(active: Level, wanted: Level) -> bool {
    (active as u8) >= (wanted as u8) && (wanted as u8) > (Level::Off as u8)
}

static ACTIVE: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Sets the active level.
pub fn set_level(level: Level) {
    ACTIVE.store(level as u8, Ordering::Relaxed);
}

/// Returns the active level.
#[must_use]
pub fn level() -> Level {
    match ACTIVE.load(Ordering::Relaxed) {
        0 => Level::Off,
        1 => Level::Info,
        _ => Level::Debug,
    }
}

/// Returns whether a record at `wanted` is currently emitted.
#[must_use]
pub fn enabled(wanted: Level) -> bool {
    permits(level(), wanted)
}

/// Applies `BLOCK_STORAGE_LOG`. An unset variable keeps the `info` default; an
/// unrecognized value is an error, because silently ignoring it would hide
/// exactly the diagnostics the operator asked for.
pub fn configure_from_env() -> io::Result<Level> {
    let Some(value) = env::var_os(LEVEL_VARIABLE) else {
        return Ok(level());
    };
    let selected = value.to_str().and_then(Level::parse).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{LEVEL_VARIABLE} must be off, info or debug, not {:?}",
                value.to_string_lossy()
            ),
        )
    })?;
    set_level(selected);
    Ok(selected)
}

/// Writes one record. Prefer the `log_info!` and `log_debug!` macros, which
/// skip formatting when the level is not active.
pub fn emit(wanted: Level, fields: Arguments<'_>) {
    if !enabled(wanted) {
        return;
    }
    // One locked write per record keeps lines intact when the daemon queue and
    // the shutdown path log concurrently. Logging never fails the caller.
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{} {fields}", wanted.name());
}

/// Emits a lifecycle or aggregate record.
#[macro_export]
macro_rules! log_info {
    ($($argument:tt)*) => {
        if $crate::logging::enabled($crate::logging::Level::Info) {
            $crate::logging::emit($crate::logging::Level::Info, format_args!($($argument)*));
        }
    };
}

/// Emits a per-request or per-record detail record.
#[macro_export]
macro_rules! log_debug {
    ($($argument:tt)*) => {
        if $crate::logging::enabled($crate::logging::Level::Debug) {
            $crate::logging::emit($crate::logging::Level::Debug, format_args!($($argument)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_round_trip_through_parsing() {
        for level in [Level::Off, Level::Info, Level::Debug] {
            assert_eq!(Level::parse(level.name()), Some(level));
        }
        for text in ["", "OFF", "trace", "warn", "1", "debug "] {
            assert_eq!(Level::parse(text), None, "accepted {text:?}");
        }
    }

    #[test]
    fn each_level_permits_exactly_the_records_at_or_below_it() {
        assert!(!permits(Level::Off, Level::Info));
        assert!(!permits(Level::Off, Level::Debug));
        assert!(permits(Level::Info, Level::Info));
        assert!(!permits(Level::Info, Level::Debug));
        assert!(permits(Level::Debug, Level::Info));
        assert!(permits(Level::Debug, Level::Debug));
        // `Off` is a selection, never a record level.
        assert!(!permits(Level::Off, Level::Off));
        assert!(!permits(Level::Debug, Level::Off));
    }
}
