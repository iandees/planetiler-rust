//! Port of `util/LogUtil.java`: prepends `[stage] ` to log messages from the current thread.
//!
//! Java stores the stage in the SLF4J `MDC` (a thread-local map) and the log4j pattern prints it before each message.
//! Here the stage is a thread-local string, and [`fields`] is a `tracing-subscriber` field formatter that writes it
//! before each event's message:
//!
//! ```no_run
//! tracing_subscriber::fmt().fmt_fields(planetiler_core::util::log_util::fields()).init();
//! ```
//!
//! [`Stats::start_stage`](crate::stats::stats::Stats::start_stage) sets the stage, and
//! [`Worker`](crate::worker::worker::Worker) threads inherit the stage of the thread that started them as
//! `[parent:prefix]`, like Java.

use std::cell::RefCell;

use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::Writer;

thread_local! {
    static STAGE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Prepends `[stage] ` to all subsequent logs from this thread.
pub fn set_stage(stage: &str) {
    STAGE.with(|s| *s.borrow_mut() = Some(stage.to_string()));
}

/// Removes `[stage] ` from subsequent logs from this thread.
pub fn clear_stage() {
    STAGE.with(|s| *s.borrow_mut() = None);
}

/// Returns the current stage that is prepended to logs from this thread.
pub fn get_stage() -> Option<String> {
    STAGE.with(|s| s.borrow().clone())
}

/// Prepends `[parent:child] ` to all subsequent logs from this thread (just `[child] ` when there is no parent), with
/// a leading `parent` (and optional `_`) removed from `child`.
pub fn set_stage_with_parent(parent: Option<&str>, child: &str) {
    match parent {
        None => set_stage(child),
        Some(parent) => {
            let child = child.strip_prefix(parent).map_or(child, |rest| rest.strip_prefix('_').unwrap_or(rest));
            set_stage(&format!("{parent}:{child}"));
        }
    }
}

/// The `[stage] ` prefix for log messages from this thread, or an empty string when no stage is set.
pub fn prefix() -> String {
    STAGE.with(|s| s.borrow().as_deref().map_or(String::new(), |stage| format!("[{stage}] ")))
}

/// A field formatter for `tracing_subscriber::fmt` that prepends [`prefix`] to event messages and writes messages
/// verbatim: progress logs embed ANSI colors, which the default field formatter escapes.
pub fn fields() -> StageFields {
    StageFields
}

/// See [`fields`].
#[derive(Debug, Default, Clone, Copy)]
pub struct StageFields;

impl<'writer> FormatFields<'writer> for StageFields {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'writer>, fields: R) -> std::fmt::Result {
        let mut visitor = Visitor { writer: &mut writer, first: true, result: Ok(()) };
        fields.record(&mut visitor);
        visitor.result
    }
}

struct Visitor<'a, 'writer> {
    writer: &'a mut Writer<'writer>,
    first: bool,
    result: std::fmt::Result,
}

impl tracing::field::Visit for Visitor<'_, '_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if self.result.is_err() {
            return;
        }
        let separator = if self.first { "" } else { " " };
        self.first = false;
        self.result = if field.name() == "message" {
            write!(self.writer, "{separator}{}{value:?}", prefix())
        } else {
            write!(self.writer, "{separator}{field}={value:?}")
        };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[test]
    fn stage() {
        clear_stage();
        assert_eq!(None, get_stage());
        assert_eq!("", prefix());
        set_stage("osm_pass1");
        assert_eq!(Some("osm_pass1".to_string()), get_stage());
        assert_eq!("[osm_pass1] ", prefix());
        std::thread::spawn(|| assert_eq!(None, get_stage())).join().unwrap();
        clear_stage();
        assert_eq!(None, get_stage());
    }

    #[test]
    fn stage_with_parent() {
        set_stage_with_parent(None, "child");
        assert_eq!(Some("child".to_string()), get_stage());
        set_stage_with_parent(Some("osm_pass1"), "osm_pass1_process");
        assert_eq!(Some("osm_pass1:process".to_string()), get_stage());
        set_stage_with_parent(Some("osm_pass1"), "osm_pass1process");
        assert_eq!(Some("osm_pass1:process".to_string()), get_stage());
        set_stage_with_parent(Some("download"), "osm");
        assert_eq!(Some("download:osm".to_string()), get_stage());
        clear_stage();
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;

        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    #[test]
    fn formats_stage_prefix() {
        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .without_time()
            .with_level(false)
            .with_target(false)
            .with_ansi(false)
            .fmt_fields(fields())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            clear_stage();
            tracing::info!("before");
            set_stage("stage");
            tracing::info!("in \u{1b}[32mstage\u{1b}[0m");
            tracing::info!(key = 1, "with field");
            clear_stage();
        });
        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert_eq!("before\n[stage] in \u{1b}[32mstage\u{1b}[0m\n[stage] with field key=1\n", output);
    }
}
