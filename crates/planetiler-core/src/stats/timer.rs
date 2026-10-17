//! Port of `stats/Timer.java`.

use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::stats::process_time::ProcessTime;

/// Measures elapsed wall and CPU time. Clones share the same underlying timer.
#[derive(Clone, Debug)]
pub struct Timer {
    start: ProcessTime,
    end: Arc<Mutex<Option<ProcessTime>>>,
}

impl Timer {
    pub fn start() -> Timer {
        Timer { start: ProcessTime::now(), end: Arc::new(Mutex::new(None)) }
    }

    pub fn stop(&self) -> &Self {
        *self.end.lock() = Some(ProcessTime::now());
        self
    }

    pub fn running(&self) -> bool {
        self.end.lock().is_none()
    }

    pub fn elapsed(&self) -> ProcessTime {
        let end = self.end.lock().clone().unwrap_or_else(ProcessTime::now);
        end.minus(&self.start)
    }
}

impl fmt::Display for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.elapsed())
    }
}
