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
        let end = (*self.end.lock()).unwrap_or_else(ProcessTime::now);
        end.minus(&self.start)
    }
}

impl fmt::Display for Timer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer() {
        let timer = Timer::start();
        let elapsed1 = timer.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let elapsed2 = timer.stop().elapsed();
        let elapsed3 = timer.elapsed();
        assert_eq!(elapsed2.wall, elapsed3.wall);
        assert!(elapsed1.wall < elapsed2.wall);
        assert!(elapsed3.cpu.is_some(), "no CPU time");
        assert!(!timer.running());
    }
}
