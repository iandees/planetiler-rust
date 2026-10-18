//! Port of `stats/ProcessInfo.java`: utilities to gather runtime information about this process.
//!
//! Java reads most of this from JVM management beans. In Rust:
//! - process CPU time comes from `getrusage`
//! - "heap" memory is the resident set size of the process, and the "max memory" is total system memory
//! - there is no garbage collector, so GC time is always zero and there is no post-GC memory usage
//! - per-thread CPU time is only available for threads that call [`register_current_thread`] (all
//!   [`Worker`](crate::worker::worker::Worker) threads do this automatically), using `/proc/self/task/<tid>/stat` on
//!   Linux and mach `thread_info` on macOS.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

/// Total CPU time (user + system) consumed by this process, if available.
pub fn get_process_cpu_time() -> Option<Duration> {
    #[cfg(unix)]
    {
        // SAFETY: getrusage only writes to the provided struct.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            return Some(timeval(usage.ru_utime) + timeval(usage.ru_stime));
        }
    }
    None
}

#[cfg(unix)]
fn timeval(t: libc::timeval) -> Duration {
    Duration::new(t.tv_sec.max(0) as u64, (t.tv_usec.max(0) as u32) * 1000)
}

/// Number of processors available to this process.
pub fn get_available_processors() -> usize {
    num_cpus::get()
}

/// Maximum memory available to this process in bytes (Java's `-Xmx`; here total physical memory).
pub fn get_max_memory_bytes() -> u64 {
    static TOTAL: LazyLock<u64> = LazyLock::new(|| {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        sys.total_memory()
    });
    *TOTAL
}

/// Memory used by this process (Java's used heap memory; here resident set size).
pub fn get_on_heap_used_memory_bytes() -> u64 {
    let Ok(pid) = sysinfo::get_current_pid() else {
        return 0;
    };
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        false,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    sys.process(pid).map_or(0, |p| p.memory())
}

/// Java's direct (off-heap) `ByteBuffer` memory. Rust has no separate direct memory pool, so this is always 0.
pub fn get_direct_used_memory_bytes() -> u64 {
    0
}

/// Java's `-XX:MaxDirectMemorySize`; here the same as [`get_max_memory_bytes`].
pub fn get_direct_used_memory_limit() -> u64 {
    get_max_memory_bytes()
}

/// Total amount of memory on the system, if available.
pub fn get_system_memory_bytes() -> Option<u64> {
    Some(get_max_memory_bytes()).filter(|m| *m > 0)
}

/// Amount of memory currently available on this system, if available.
pub fn get_system_free_memory_bytes() -> Option<u64> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    Some(sys.available_memory()).filter(|m| *m > 0)
}

/// Time spent in garbage collection: always zero since Rust has no garbage collector.
pub fn get_gc_time() -> Duration {
    Duration::ZERO
}

/// Memory pool sizes after the last GC: always empty since Rust has no garbage collector.
pub fn get_post_gc_pool_sizes() -> BTreeMap<String, u64> {
    BTreeMap::new()
}

/// Total memory usage after the last GC: always `None` since Rust has no garbage collector.
pub fn get_memory_usage_after_last_gc() -> Option<u64> {
    None
}

/// System load average over the last minute, or a negative number if not available.
pub fn get_system_load_average() -> f64 {
    let load = sysinfo::System::load_average();
    if load.one.is_finite() { load.one } else { -1.0 }
}

/// Processor usage statistics for a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadState {
    pub name: String,
    pub cpu_time: Duration,
    pub user_time: Duration,
    /// Time spent waiting (not tracked in Rust, always zero unless constructed explicitly).
    pub waiting: Duration,
    /// Time spent blocked (not tracked in Rust, always zero unless constructed explicitly).
    pub blocking: Duration,
    pub id: i64,
}

impl Default for ThreadState {
    fn default() -> Self {
        ThreadState {
            name: String::new(),
            cpu_time: Duration::ZERO,
            user_time: Duration::ZERO,
            waiting: Duration::ZERO,
            blocking: Duration::ZERO,
            id: -1,
        }
    }
}

impl ThreadState {
    /// Adds up the timers in two `ThreadState` instances.
    pub fn plus(&self, other: &ThreadState) -> ThreadState {
        ThreadState {
            name: "<multiple threads>".to_string(),
            cpu_time: self.cpu_time + other.cpu_time,
            user_time: self.user_time + other.user_time,
            waiting: self.waiting + other.waiting,
            blocking: self.blocking + other.blocking,
            id: -1,
        }
    }
}

/// OS-specific handle that can be used to read the CPU time of a thread from another thread.
#[derive(Clone, Copy, Debug)]
struct ThreadHandle {
    #[cfg(target_os = "linux")]
    tid: libc::pid_t,
    #[cfg(target_os = "macos")]
    port: libc::mach_port_t,
}

impl ThreadHandle {
    fn current() -> ThreadHandle {
        ThreadHandle {
            #[cfg(target_os = "linux")]
            // SAFETY: gettid has no preconditions.
            tid: unsafe { libc::gettid() },
            #[cfg(target_os = "macos")]
            // SAFETY: pthread_self always returns a valid handle for the current thread.
            port: unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) },
        }
    }

    /// Returns (cpu time, user time) of the thread, or `None` if not available.
    fn times(&self) -> Option<(Duration, Duration)> {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/self/task/{}/stat", self.tid)).ok()?;
            // fields after the parenthesized command name, starting with field 3 (state)
            let rest = &stat[stat.rfind(')')? + 2..];
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            // SAFETY: sysconf has no preconditions.
            let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
            let user = Duration::from_secs_f64(utime as f64 / ticks);
            let sys = Duration::from_secs_f64(stime as f64 / ticks);
            Some((user + sys, user))
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: thread_info writes at most `count` integers into `info`.
            unsafe {
                let mut info: libc::thread_basic_info = std::mem::zeroed();
                let mut count = libc::THREAD_BASIC_INFO_COUNT;
                let result = libc::thread_info(
                    self.port,
                    libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                    &mut info as *mut _ as libc::thread_info_t,
                    &mut count,
                );
                if result != libc::KERN_SUCCESS {
                    return None;
                }
                let tv = |t: libc::time_value_t| {
                    Duration::new(t.seconds.max(0) as u64, (t.microseconds.max(0) as u32) * 1000)
                };
                let user = tv(info.user_time);
                Some((user + tv(info.system_time), user))
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }
}

struct RegisteredThread {
    name: String,
    handle: ThreadHandle,
}

static NEXT_THREAD_ID: AtomicU64 = AtomicU64::new(1);
static THREADS: LazyLock<Mutex<BTreeMap<i64, RegisteredThread>>> = LazyLock::new(|| Mutex::new(BTreeMap::new()));

struct Registration {
    id: i64,
    registered: bool,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.registered {
            THREADS.lock().remove(&self.id);
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Registration> = RefCell::new(Registration {
        id: NEXT_THREAD_ID.fetch_add(1, Ordering::Relaxed) as i64,
        registered: false,
    });
}

/// Unique ID of the current thread used in [`ThreadState::id`].
pub fn current_thread_id() -> i64 {
    CURRENT.with(|c| c.borrow().id)
}

/// Make the current thread show up in [`get_thread_stats`] (and so in per-thread CPU usage logs and metrics) until it
/// exits. Uses the name from [`std::thread::current`].
pub fn register_current_thread() {
    CURRENT.with(|c| {
        let mut reg = c.borrow_mut();
        let name = std::thread::current().name().unwrap_or("unnamed").to_string();
        THREADS.lock().insert(reg.id, RegisteredThread { name, handle: ThreadHandle::current() });
        reg.registered = true;
    });
}

/// Returns a map from thread ID to stats about that thread for every live thread registered with
/// [`register_current_thread`].
pub fn get_thread_stats() -> BTreeMap<i64, ThreadState> {
    let threads = THREADS.lock();
    threads
        .iter()
        .map(|(id, t)| {
            let (cpu_time, user_time) = t.handle.times().unwrap_or_default();
            (*id, ThreadState { name: t.name.clone(), cpu_time, user_time, id: *id, ..Default::default() })
        })
        .collect()
}

/// Returns the CPU usage stats for the current thread.
pub fn get_current_thread_state() -> ThreadState {
    let (cpu_time, user_time) = ThreadHandle::current().times().unwrap_or_else(|| {
        let cpu = current_thread_cpu_time();
        (cpu, cpu)
    });
    ThreadState {
        name: std::thread::current().name().unwrap_or("unnamed").to_string(),
        cpu_time,
        user_time,
        id: current_thread_id(),
        ..Default::default()
    }
}

/// CPU time consumed by the current thread.
pub fn current_thread_cpu_time() -> Duration {
    #[cfg(unix)]
    {
        // SAFETY: clock_gettime only writes to the provided struct.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(ts.tv_sec.max(0) as u64, ts.tv_nsec.max(0) as u32);
        }
    }
    Duration::ZERO
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc() {
        assert_eq!(Duration::ZERO, get_gc_time());
    }

    #[test]
    fn memory() {
        assert_eq!(0, get_direct_used_memory_bytes());
        assert!(get_direct_used_memory_limit() > 0);
        assert!(get_on_heap_used_memory_bytes() > 0);
        assert!(get_system_memory_bytes().unwrap() > 0);
        assert!(get_max_memory_bytes() > 0);
    }

    #[test]
    fn cpu() {
        assert!(get_process_cpu_time().is_some());
        assert!(get_available_processors() >= 1);
    }

    #[test]
    fn threads() {
        std::thread::Builder::new()
            .name("test-thread-stats".into())
            .spawn(|| {
                register_current_thread();
                let mut x = 0u64;
                for i in 0..1_000_000u64 {
                    x = x.wrapping_add(i * i);
                }
                std::hint::black_box(x);
                let stats = get_thread_stats();
                let me = stats.get(&current_thread_id()).expect("registered");
                assert_eq!("test-thread-stats", me.name);
                assert!(!get_thread_stats().is_empty());
                let state = get_current_thread_state();
                assert_eq!("test-thread-stats", state.name);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn thread_unregistered_on_exit() {
        let id = std::thread::spawn(|| {
            register_current_thread();
            current_thread_id()
        })
        .join()
        .unwrap();
        assert!(!get_thread_stats().contains_key(&id));
    }

    #[test]
    fn add() {
        let s = Duration::from_secs;
        let a =
            ThreadState { name: String::new(), cpu_time: s(1), user_time: s(2), waiting: s(3), blocking: s(4), id: -1 };
        let b =
            ThreadState { name: String::new(), cpu_time: s(5), user_time: s(6), waiting: s(7), blocking: s(8), id: -1 };
        let sum = a.plus(&b);
        assert_eq!(s(6), sum.cpu_time);
        assert_eq!(s(8), sum.user_time);
        assert_eq!(s(10), sum.waiting);
        assert_eq!(s(12), sum.blocking);
    }
}
