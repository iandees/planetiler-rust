//! Times reading (decode + reproject to lat/lon, no profile processing) of the inputs that dominate small planetiler
//! runs: the OSM water polygons shapefile and the Natural Earth sqlite database.
//!
//! Arguments (`--key=value` or `key=value`):
//! - `water` (default `../planetiler-bench/data/sources/water-polygons-split-3857.zip`)
//! - `natural_earth` (default `../planetiler-bench/data/sources/natural_earth_vector.sqlite.zip`)
//! - `tmp` (default: a temporary directory) where Natural Earth is extracted
//! - `keep_unzipped` (default `false`) reuse the extracted Natural Earth sqlite file between runs
//! - `iterations` (default `1`)
//! - `only` (`water` or `natural_earth`) to run just one

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
