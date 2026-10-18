//! Port of `stats/ProcessTime.java`: wall and CPU time this process consumes between snapshots.
//!
//! Java also tracks garbage-collection time; Rust has no GC so that component is omitted.

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
    /// Takes a snapshot of current wall and CPU time of this process.
    pub fn now() -> ProcessTime {
        ProcessTime { wall: EPOCH.elapsed(), cpu: crate::stats::process_info::get_process_cpu_time() }
    }

    /// Returns the amount of time elapsed between `other` and `self`.
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
        use crate::util::format;
        let cpu = self.cpu.map_or_else(|| "-".to_string(), format::duration);
        write!(f, "{} cpu:{cpu}", format::duration(self.wall))?;
        if let Some(cpu) = self.cpu {
            let avg = cpu.as_nanos() as f64 / self.wall.as_nanos() as f64;
            write!(f, " avg:{}", format::decimal(avg))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        let t = ProcessTime { wall: Duration::from_secs(10), cpu: Some(Duration::from_secs(20)) };
        assert_eq!("10s cpu:20s avg:2", t.to_string());
        let t = ProcessTime { wall: Duration::from_secs(10), cpu: None };
        assert_eq!("10s cpu:-", t.to_string());
    }

    #[test]
    fn minus() {
        let a = ProcessTime { wall: Duration::from_secs(10), cpu: Some(Duration::from_secs(20)) };
        let b = ProcessTime { wall: Duration::from_secs(3), cpu: Some(Duration::from_secs(5)) };
        assert_eq!(ProcessTime { wall: Duration::from_secs(7), cpu: Some(Duration::from_secs(15)) }, a.minus(&b));
    }
}
