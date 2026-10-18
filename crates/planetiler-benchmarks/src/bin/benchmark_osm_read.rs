//! Port of `planetiler-benchmarks/.../BenchmarkOsmRead.java`.
//!
//! Repeatedly runs `OsmReader::pass1` with a null profile and no-op node location / multipolygon storage, printing the
//! elapsed time of each pass to stderr in the same format as Java (`<wall> cpu:<cpu> avg:<cpus>`).
//!
//! Arguments (parsed like Java's `Arguments.fromArgsOrConfigFile`, so any `PlanetilerConfig` option works):
//! - `osm_path` (default `data/sources/northeast.osm.pbf`) path to the `.osm.pbf` file (may also be passed as a bare
//!   positional argument)
//! - `threads` (default: number of CPUs, minimum 2)
//! - `osm_lazy_reads` (default `true`) read OSM blocks from disk in worker threads
//! - `iterations` (Rust-only, default: run forever like Java) number of passes to run
//! - `precise` (Rust-only, default `false`) append the exact wall/cpu milliseconds to each line, useful for small
//!   inputs like monaco where the Java format rounds to `0s`

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;
use std::time::{Duration, Instant};

use planetiler_core::collection::{long_long_map, long_long_multimap};
use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::profile::NullProfile;
use planetiler_core::reader::osm::osm_input_file::OsmInputFile;
use planetiler_core::reader::osm::osm_reader::OsmReader;
use planetiler_core::stats::stats;

/// Process CPU time (user + system) of this process.
fn cpu_time() -> Option<Duration> {
    // SAFETY: getrusage only writes into the provided struct
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        let to_duration = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
        Some(to_duration(usage.ru_utime) + to_duration(usage.ru_stime))
    }
}

/// Java `Format.decimal`: at most 1 fraction digit, trailing zeros dropped.
fn decimal(value: f64) -> String {
    let s = format!("{value:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// Java `Format.duration`: `0.5s`, `3s`, `1m1s`, `1h`, ...
fn format_duration(d: Duration) -> String {
    let seconds = d.as_secs_f64();
    if seconds < 1.0 {
        return format!("{}s", decimal(seconds));
    }
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 {
        out.push_str(&format!("{s}s"));
    }
    out
}

fn main() -> anyhow::Result<()> {
    // accept a bare `*.pbf` argument as the osm_path
    let args: Vec<String> = std::env::args()
        .skip(1)
        .map(|arg| if arg.ends_with(".pbf") && !arg.contains('=') { format!("--osm_path={arg}") } else { arg })
        .collect();
    let arguments = Arguments::from_args_or_config_file(&args)?;
    let config = PlanetilerConfig::from(&arguments)?;
    let path = arguments.input_file("osm_path", "path to osm file", "data/sources/northeast.osm.pbf")?;
    let iterations = arguments.get_long("iterations", "number of passes to run (default: forever)", -1)?;
    let precise = arguments.get_boolean("precise", "print exact wall/cpu milliseconds", false);
    eprintln!("osm_path={} threads={} osm_lazy_reads={}", path.display(), config.threads, config.osm_lazy_reads);
    let file = OsmInputFile::new(&path, config.osm_lazy_reads);
    let stats = stats::in_memory();
    let profile = Arc::new(NullProfile);

    let mut i = 0;
    while iterations < 0 || i < iterations {
        let start = Instant::now();
        let start_cpu = cpu_time();
        {
            let reader = OsmReader::new(
                "osm",
                file.get()?,
                Arc::from(long_long_map::noop()),
                Box::new(long_long_multimap::noop()),
                profile.clone(),
                stats.clone(),
            );
            reader.pass1(&config)?;
            reader.close();
        }
        let wall = start.elapsed();
        let cpu = start_cpu.zip(cpu_time()).map(|(a, b)| b.saturating_sub(a));
        let cpu_string = cpu.map_or("-".to_string(), format_duration);
        let avg = cpu.map_or(String::new(), |c| format!(" avg:{}", decimal(c.as_secs_f64() / wall.as_secs_f64())));
        if precise {
            let cpu_ms = cpu.map_or(0.0, |c| c.as_secs_f64() * 1000.0);
            eprintln!(
                "{} cpu:{}{} (wall_ms={:.1} cpu_ms={:.1})",
                format_duration(wall),
                cpu_string,
                avg,
                wall.as_secs_f64() * 1000.0,
                cpu_ms
            );
        } else {
            eprintln!("{} cpu:{}{}", format_duration(wall), cpu_string, avg);
        }
        i += 1;
    }
    Ok(())
}
