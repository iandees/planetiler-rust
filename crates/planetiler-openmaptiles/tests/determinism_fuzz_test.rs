//! Port of upstream `DeterminismFuzzer`: checks that the output of the whole OpenMapTiles pipeline does not depend on
//! thread timing or on the tuning options that should only affect how fast it runs.
//!
//! Runs the OpenMapTiles profile on the small Monaco extract several times:
//!
//! 1. with the default arguments, then again with the same arguments: the tiles must be byte-for-byte identical
//! 2. then with randomly chosen thread counts, node map types and temp storage options: the decoded tiles, including
//!    the order of the features within each layer, must be identical (archive metadata may differ since it records some
//!    of these options).
//!
//! Thread timing varies from run to run so passing does not prove output is deterministic, but each run with
//! different options gives a race another chance to show up. The default is a handful of runs (a few seconds); use
//! environment variables for a longer search:
//!
//! ```text
//! PLANETILER_FUZZ_RUNS=100 PLANETILER_FUZZ_SEED=1234 cargo test --release -p planetiler-openmaptiles \
//!     --test determinism_fuzz_test -- --nocapture
//! ```
//!
//! Monaco is small, so many races (including the two fixed alongside the upstream fuzzer) rarely show up here; this
//! catches gross regressions and is meant to be run with a large `PLANETILER_FUZZ_RUNS` or on a larger extract.
//!
//! Failures print the seed; rerun with `PLANETILER_FUZZ_SEED` to try the same options again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::config::arguments::Arguments;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::util::gzip::gunzip;
use planetiler_core::vector_tile;
use planetiler_openmaptiles::open_map_tiles_main;

/// Small deterministic PRNG (splitmix64) so the fuzzer needs no extra dependencies and seeds are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Returns a number in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn flag(&mut self) -> bool {
        self.below(2) == 0
    }
}

fn core_resource(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../planetiler-core/tests/resources").join(name)
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Returns randomly chosen options that should not change the tiles planetiler generates.
fn random_options(rng: &mut Rng, max_threads: usize) -> Vec<(&'static str, String)> {
    // up to twice as many threads as cores, more threads than cores interleave work differently
    let threads = 2 + rng.below(2 * max_threads - 1);
    let nodemap_type = ["sparsearray", "sortedtable"][rng.below(2)];
    let storage = |rng: &mut Rng| if rng.flag() { "mmap" } else { "ram" };
    vec![
        ("threads", threads.to_string()),
        ("process_threads", (1 + rng.below(threads)).to_string()),
        ("write_threads", (1 + rng.below(2)).to_string()),
        ("feature_read_threads", (1 + rng.below(2)).to_string()),
        ("sort_max_readers", (1 + rng.below(threads)).to_string()),
        ("sort_max_writers", (1 + rng.below(threads)).to_string()),
        ("parallel_tmp_io", rng.flag().to_string()),
        ("storage", storage(rng).to_string()),
        ("nodemap_type", nodemap_type.to_string()),
        ("nodemap_storage", storage(rng).to_string()),
        ("compress_temp", rng.flag().to_string()),
        ("osm_lazy_reads", rng.flag().to_string()),
    ]
}

fn run(dir: &Path, water: &Path, index: usize, options: &[(&'static str, String)]) -> PathBuf {
    let output = dir.join(format!("run-{index}.mbtiles"));
    let path = |p: &Path| p.display().to_string();
    let mut args = vec![
        ("osm_path", path(&core_resource("monaco-latest.osm.pbf"))),
        ("natural_earth_path", path(&core_resource("natural_earth_vector.sqlite.zip"))),
        ("water_polygons_path", path(water)),
        // no centerlines in monaco - so fake it out with an empty source
        ("lake_centerlines_path", path(water)),
        ("tmpdir", path(&dir.join(format!("tmp-{index}")))),
        ("mbtiles", path(&output)),
        ("force", "true".to_string()),
    ];
    args.extend(options.iter().map(|(k, v)| (*k, v.clone())));
    open_map_tiles_main::run(&Arguments::of_pairs(args)).unwrap();
    std::fs::remove_dir_all(dir.join(format!("tmp-{index}"))).ok();
    output
}

/// Returns the compressed bytes of every tile.
fn raw_tiles(path: &Path) -> BTreeMap<(u32, u32, u32), Vec<u8>> {
    let db = Mbtiles::new_read_only_database(path).unwrap();
    let mut result = BTreeMap::new();
    for tile in db.get_all_tiles().unwrap() {
        let tile = tile.unwrap();
        result.insert((tile.coord.z, tile.coord.x, tile.coord.y), tile.bytes.to_vec());
    }
    result
}

/// Asserts that both archives have the same tiles with the same features in the same order.
fn assert_same_decoded_tiles(
    a: &BTreeMap<(u32, u32, u32), Vec<u8>>,
    b: &BTreeMap<(u32, u32, u32), Vec<u8>>,
    context: &str,
) {
    assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>(), "{context}: different tiles");
    for (coord, bytes_a) in a {
        let features_a = vector_tile::decode(&gunzip(bytes_a).unwrap()).unwrap();
        let features_b = vector_tile::decode(&gunzip(&b[coord]).unwrap()).unwrap();
        assert!(features_a == features_b, "{context}: tile {coord:?} differs");
    }
}

#[test]
fn test_output_does_not_depend_on_thread_timing_or_tuning_options() {
    let runs = env_u64("PLANETILER_FUZZ_RUNS").unwrap_or(4).max(2) as usize;
    let seed = env_u64("PLANETILER_FUZZ_SEED").unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
    });
    let max_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 16);
    println!("Checking determinism of the OpenMapTiles profile with seed {seed} and {runs} runs");
    let mut rng = Rng(seed);

    let dir = tempfile::tempdir().unwrap();
    let water = dir.path().join("water");
    planetiler_core::util::file_utils::unzip_file(&core_resource("water-polygons-split-3857.zip"), &water).unwrap();

    let reference = raw_tiles(&run(dir.path(), &water, 0, &[]));
    assert!(!reference.is_empty());
    for i in 1..runs {
        // run 1 repeats the arguments as given, so the tile bytes must be identical
        let options = if i == 1 { vec![] } else { random_options(&mut rng, max_threads) };
        println!("Run {i}: {options:?}");
        let tiles = raw_tiles(&run(dir.path(), &water, i, &options));
        let context = format!("run {i} differs from run 0, rerun with PLANETILER_FUZZ_SEED={seed} ({options:?})");
        if options.is_empty() {
            assert!(reference == tiles, "{context}");
        } else {
            assert_same_decoded_tiles(&reference, &tiles, &context);
        }
        std::fs::remove_file(dir.path().join(format!("run-{i}.mbtiles"))).ok();
    }
}
