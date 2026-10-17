//! Port of `stats/ProcessTime.java`.

use std::fmt;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// A snapshot of wall-clock and process CPU time, or the difference between two snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessTime {
    pub wall: Duration,
    pub cpu: Option<Duration>,
}

impl ProcessTime {
    pub fn now() -> ProcessTime {
        ProcessTime { wall: EPOCH.elapsed(), cpu: crate::stats::process_info::get_process_cpu_time() }
    }

    pub fn minus(&self, other: &ProcessTime) -> ProcessTime {
        ProcessTime {
            wall: self.wall.saturating_sub(other.wall),
            cpu: match (self.cpu, other.cpu) {
                (Some(a), Some(b)) => Some(a.saturating_sub(b)),
                _ => None,
            },
        }
    }
}

impl fmt::Display for ProcessTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use crate::util::format::duration;
        let cpu = self.cpu.map_or_else(|| "-".to_string(), duration);
        write!(f, "{} cpu:{cpu}", duration(self.wall))?;
        if let Some(cpu) = self.cpu
            && !self.wall.is_zero()
        {
            write!(f, " avg:{:.1}", cpu.as_secs_f64() / self.wall.as_secs_f64())?;
        }
        Ok(())
    }
}
