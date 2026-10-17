//! Port of `geo/GeometryException.java`.

use std::fmt;

use crate::stats::stats::Stats;

/// An error that occurs while processing a geometry. `stat` is a short identifier used to count occurrences of this
/// error in [`Stats::data_error`].
#[derive(Debug, Clone)]
pub struct GeometryException {
    stat: String,
    message: String,
    non_fatal: bool,
    /// When true (Java's `GeometryException.Verbose`), log at trace level instead of warn.
    verbose: bool,
    details: Vec<String>,
    cause: Option<String>,
}

pub type GeometryResult<T> = Result<T, GeometryException>;

impl GeometryException {
    pub fn new(stat: impl Into<String>, message: impl Into<String>) -> Self {
        GeometryException {
            stat: stat.into(),
            message: message.into(),
            non_fatal: false,
            verbose: false,
            details: Vec::new(),
            cause: None,
        }
    }

    /// An error that is expected to happen with real-world data and should not fail unit tests.
    pub fn non_fatal(stat: impl Into<String>, message: impl Into<String>) -> Self {
        GeometryException { non_fatal: true, ..Self::new(stat, message) }
    }

    /// An error that is only logged at trace level (Java's `GeometryException.Verbose`).
    pub fn verbose(stat: impl Into<String>, message: impl Into<String>) -> Self {
        GeometryException { verbose: true, ..Self::new(stat, message) }
    }

    pub fn with_cause(mut self, cause: impl fmt::Display) -> Self {
        self.cause = Some(cause.to_string());
        self
    }

    pub fn add_details(mut self, details: impl Into<String>) -> Self {
        self.details.push(details.into());
        self
    }

    /// Attach WKT of the geometry that caused this error.
    pub fn add_geometry_details(self, original: &str, geometry: &geo_types::Geometry<f64>) -> Self {
        self.add_details(format!("{original} (wkt): {geometry:?}"))
    }

    pub fn stat(&self) -> &str {
        &self.stat
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_non_fatal(&self) -> bool {
        self.non_fatal
    }

    pub fn details(&self) -> &[String] {
        &self.details
    }

    /// Increment the `{stat_prefix}_{stat}` data error counter and log the error.
    pub fn log_with_stats(&self, stats: &dyn Stats, stat_prefix: &str, log_prefix: &str) {
        stats.data_error(&format!("{stat_prefix}_{}", self.stat));
        self.log(log_prefix);
    }

    /// Same as [`GeometryException::log_with_stats`] but also includes geometry details when `log_details` is true.
    pub fn log_with_details(&self, stats: &dyn Stats, stat_prefix: &str, log_prefix: &str, log_details: bool) {
        if log_details {
            stats.data_error(&format!("{stat_prefix}_{}", self.stat));
            let mut log = format!("{log_prefix}: {}", self.message);
            for detail in &self.details {
                log.push('\n');
                log.push_str(detail);
            }
            self.log_message(&log);
        } else {
            self.log_with_stats(stats, stat_prefix, log_prefix);
        }
    }

    pub fn log(&self, log_context: &str) {
        self.log_message(&format!("{log_context}: {}", self.message));
    }

    fn log_message(&self, log: &str) {
        if self.verbose {
            tracing::trace!("{log}");
        } else {
            tracing::warn!("{log}");
            debug_assert!(self.non_fatal || cfg!(not(test)), "{log}");
        }
    }
}

impl fmt::Display for GeometryException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.stat)?;
        if let Some(cause) = &self.cause {
            write!(f, ": {cause}")?;
        }
        Ok(())
    }
}

impl std::error::Error for GeometryException {}
