//! Times reading (decode + reproject to lat/lon, no profile processing) of the inputs that dominate small planetiler
//! runs: the OSM water polygons shapefile and the Natural Earth sqlite database.
//!
//! Arguments (`--key=value` or `key=value`):
//! - `water` (default `../planetiler-bench/data/sources/water-polygons-split-3857.zip`)
//! - `natural_earth` (default `../planetiler-bench/data/sources/natural_earth_vector.sqlite.zip`)
//! - `tmp` (default: a temporary directory) where Natural Earth is extracted
//! - `keep_unzipped` (default `false`) reuse the extracted Natural Earth sqlite file between runs
//! - `iterations` (default `1`)
//! - `only` (`water`, `natural_earth` or `inflate`) to run just one; `inflate` only decompresses the water polygons
//!   `.shp` entry (`util::zip_entry_reader::ZipEntryReader`, parallel above 16 MB) and reports wall and CPU time
//! - `threads` (default: available parallelism) inflate threads for `only=inflate`

// see the `mimalloc` feature in Cargo.toml
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use planetiler_core::reader::natural_earth_reader::NaturalEarthReader;
use planetiler_core::reader::shapefile_reader::ShapefileReader;
use planetiler_core::reader::simple_reader::{ReaderFeature, SimpleReader};

fn parse_args() -> HashMap<String, String> {
    let mut result = HashMap::new();
    for arg in std::env::args().skip(1) {
        let arg = arg.trim_start_matches('-');
        match arg.split_once('=') {
            Some((k, v)) => result.insert(k.replace('-', "_"), v.to_string()),
            None => result.insert(arg.replace('-', "_"), "true".to_string()),
        };
    }
    result
}

fn count_coords(feature: &ReaderFeature) -> usize {
    use geo::CoordsIter;
    feature.geometry.coords_count()
}

fn run(name: &str, reader: &dyn SimpleReader) -> anyhow::Result<()> {
    let start = Instant::now();
    let expected = reader.feature_count()?;
    let count_time = start.elapsed();
    let start = Instant::now();
    let mut features = 0u64;
    let mut coords = 0usize;
    let mut tags = 0usize;
    reader.read_features(&mut |f| {
        features += 1;
        coords += count_coords(&f);
        tags += f.tags.len();
        Ok(())
    })?;
    let read_time = start.elapsed();
    eprintln!(
        "{name}: count {expected} in {:.3}s, read {features} features ({coords} coords, {tags} tags) in {:.3}s",
        count_time.as_secs_f64(),
        read_time.as_secs_f64()
    );
    Ok(())
}

/// User + system CPU time of this process in seconds.
fn cpu_seconds() -> f64 {
    // SAFETY: getrusage only writes the zeroed out-param
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

/// Decompresses the water polygons `.shp` entry and reports throughput.
fn inflate(water: &std::path::Path, threads: usize) -> anyhow::Result<()> {
    use std::io::Read;
    let (start, cpu) = (Instant::now(), cpu_seconds());
    let mut reader = planetiler_core::util::zip_entry_reader::ZipEntryReader::open_with_threads(
        water,
        "water-polygons-split-3857/water_polygons.shp",
        threads,
    )?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0usize;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    let (wall, cpu) = (start.elapsed().as_secs_f64(), cpu_seconds() - cpu);
    eprintln!(
        "inflate: {total} bytes on {threads} threads in {wall:.3}s wall, {cpu:.3}s CPU ({:.0} MB/s per CPU second)",
        total as f64 / 1e6 / cpu
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args = parse_args();
    let base = PathBuf::from("../planetiler-bench/data/sources");
    let water = args.get("water").map(PathBuf::from).unwrap_or_else(|| base.join("water-polygons-split-3857.zip"));
    let ne =
        args.get("natural_earth").map(PathBuf::from).unwrap_or_else(|| base.join("natural_earth_vector.sqlite.zip"));
    let iterations: usize = args.get("iterations").map_or(Ok(1), |s| s.parse())?;
    let keep_unzipped = args.get("keep_unzipped").is_some_and(|s| s == "true");
    let only = args.get("only").cloned();
    let tmp_holder = tempfile::tempdir()?;
    let tmp = args.get("tmp").map(PathBuf::from).unwrap_or_else(|| tmp_holder.path().to_path_buf());

    if only.as_deref() == Some("inflate") {
        let threads: usize = args
            .get("threads")
            .map_or_else(|| Ok(std::thread::available_parallelism().map_or(1, |n| n.get())), |s| s.parse())?;
        for _ in 0..iterations {
            inflate(&water, threads)?;
        }
        return Ok(());
    }
    for _ in 0..iterations {
        if only.as_deref().is_none_or(|o| o == "water") {
            let start = Instant::now();
            let reader = ShapefileReader::new(None, "water_polygons", &water)?;
            eprintln!("water: open in {:.3}s", start.elapsed().as_secs_f64());
            run("water", &reader)?;
            eprintln!("water: total {:.3}s", start.elapsed().as_secs_f64());
        }
        if only.as_deref().is_none_or(|o| o == "natural_earth") {
            let start = Instant::now();
            let reader = NaturalEarthReader::new("natural_earth", &ne, &tmp, keep_unzipped)?;
            eprintln!("natural_earth: open (unzip) in {:.3}s", start.elapsed().as_secs_f64());
            run("natural_earth", &reader)?;
            drop(reader);
            eprintln!("natural_earth: total {:.3}s", start.elapsed().as_secs_f64());
        }
    }
    Ok(())
}
