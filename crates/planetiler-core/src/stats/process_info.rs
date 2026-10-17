//! Port of `stats/ProcessInfo.java`.

use std::time::Duration;

/// Total CPU time (user + system) consumed by this process, if available.
pub fn get_process_cpu_time() -> Option<Duration> {
    #[cfg(unix)]
    {
        // SAFETY: getrusage only writes to the provided struct.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
            return Some(tv(usage.ru_utime) + tv(usage.ru_stime));
        }
    }
    None
}

/// Number of processors available to this process.
pub fn get_available_processors() -> usize {
    num_cpus::get()
}

/// Maximum memory available to this process in bytes (total physical memory).
pub fn get_max_memory_bytes() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

#[cfg(test)]
mod tests {
    #[test]
    fn cpu_time() {
        assert!(super::get_process_cpu_time().is_some());
        assert!(super::get_available_processors() >= 1);
    }
}
