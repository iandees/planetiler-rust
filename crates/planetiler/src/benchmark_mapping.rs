//! Port of `planetiler-benchmarks/.../OpenMapTilesMapping.java` (the `benchmark-mapping` task): times how long it
//! takes to match a sample of elements from an OSM input file against the OpenMapTiles imposm3 table mappings
//! (`MultiExpression` performance).
//!
//! Reads 90% of the elements (sampled with `java.util.Random(0)` like Java) and then repeatedly matches them all,
//! printing `took:<ms>ms found:<matches> avg:<ms>ms` to stderr after each pass, like Java.
//!
//! Arguments (Rust-only; Java has none):
//! - `osm_path` (default `data/sources/massachusetts.osm.pbf`, like Java) path to the `.osm.pbf` file
//! - `iterations` (default: run forever like Java) number of timed passes after the warmup pass

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use geo_types::Geometry;
use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::geo::geometry_exception::{GeometryException, GeometryResult};
use planetiler_core::geo::with_geometry::{GeometryCache, WithGeometry};
use planetiler_core::reader::osm::osm_block_source::OsmBlockSource;
use planetiler_core::reader::osm::osm_element::OsmElementType;
use planetiler_core::reader::osm::osm_input_file::OsmInputFile;
use planetiler_core::reader::source_feature::{SourceFeature, SourceFeatureBase};
use planetiler_core::reader::with_geometry_type::WithGeometryType;
use planetiler_core::stats::progress_loggers::ProgressLoggers;
use planetiler_core::stats::stats;
use planetiler_core::util::translations::Translations;
use planetiler_openmaptiles::open_map_tiles_profile::OpenMapTilesProfile;

/// The subset of `java.util.Random` needed to sample the same elements as Java.
struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5DEECE66D;
    const MASK: u64 = (1 << 48) - 1;

    fn new(seed: u64) -> JavaRandom {
        JavaRandom { seed: (seed ^ Self::MULTIPLIER) & Self::MASK }
    }

    fn next(&mut self, bits: u32) -> u64 {
        self.seed = (self.seed.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xB)) & Self::MASK;
        self.seed >> (48 - bits)
    }

    fn next_double(&mut self) -> f64 {
        ((self.next(26) << 27) + self.next(27)) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

/// An element's tags without a geometry (Java's anonymous `SourceFeature` subclass).
struct TagsOnlyFeature {
    base: SourceFeatureBase,
    element_type: OsmElementType,
}

impl std::fmt::Debug for TagsOnlyFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.base.fmt(f)
    }
}

impl WithGeometryType for TagsOnlyFeature {
    fn is_point(&self) -> bool {
        self.element_type == OsmElementType::Node
    }

    fn can_be_polygon(&self) -> bool {
        matches!(self.element_type, OsmElementType::Way | OsmElementType::Relation)
    }

    fn can_be_line(&self) -> bool {
        self.element_type == OsmElementType::Way
    }
}

impl WithGeometry for TagsOnlyFeature {
    fn world_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Err(GeometryException::new("benchmark_no_geometry", "no geometry"))
    }

    fn lat_lon_geometry(&self) -> GeometryResult<&Geometry<f64>> {
        Err(GeometryException::new("benchmark_no_geometry", "no geometry"))
    }

    fn geometry_cache(&self) -> &GeometryCache {
        &self.base.geometry_cache
    }
}

impl SourceFeature for TagsOnlyFeature {
    fn base(&self) -> &SourceFeatureBase {
        &self.base
    }

    fn as_any_feature(&self) -> &dyn Any {
        self
    }
}

planetiler_core::impl_source_feature_traits!(TagsOnlyFeature);

pub fn main(args: &[String]) -> anyhow::Result<()> {
    let arguments = Arguments::from_args_or_config_file(args)?;
    let path = arguments.file("osm_path", "path to osm file", "data/sources/massachusetts.osm.pbf");
    let iterations = arguments.get_long("iterations", "number of timed passes to run (default: forever)", -1)?;
    let profile = OpenMapTilesProfile::new(
        Arc::new(Translations::null_provider::<&str>(&[])),
        Arc::new(PlanetilerConfig::defaults()),
        stats::in_memory(),
    );
    let mut random = JavaRandom::new(0);
    let mut inputs: Vec<TagsOnlyFeature> = Vec::new();
    let num_inputs = Arc::new(AtomicI64::new(0));
    let counter = num_inputs.clone();
    let logger = ProgressLoggers::create()
        .add_rate_counter("inputs", move || counter.load(Ordering::Relaxed))
        .add_process_stats();
    OsmInputFile::new(&path, false).get()?.for_each_block(&mut |block| {
        for mut element in block.decode_elements()? {
            if random.next_double() < 0.9 {
                if inputs.len().is_multiple_of(1_000_000) {
                    logger.log();
                }
                let element_type = element.element_type();
                let id = element.id();
                let tags = std::mem::take(element.tags_mut());
                inputs.push(TagsOnlyFeature {
                    base: SourceFeatureBase::new(tags, Some("".into()), Some("".into()), vec![], id),
                    element_type,
                });
                num_inputs.store(inputs.len() as i64, Ordering::Relaxed);
            }
        }
        Ok(())
    })?;

    logger.log();
    eprintln!("read {} elems", inputs.len());

    let mut start_start = Instant::now();
    let mut count: i64 = -1;
    while iterations < 0 || count < iterations {
        count += 1;
        let start = Instant::now();
        let mut found = 0usize;
        for input in &inputs {
            found += profile.get_table_matches(input).len();
        }
        if count == 0 {
            start_start = Instant::now();
            logger.log();
            eprintln!("finished warmup");
        } else {
            logger.log();
            eprintln!(
                "took:{}ms found:{found} avg:{}ms",
                start.elapsed().as_millis(),
                start_start.elapsed().as_millis() / count as u128
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_random_next_double() {
        // new java.util.Random(0).nextDouble() x3
        let mut random = JavaRandom::new(0);
        assert_eq!(0.730967787376657, random.next_double());
        assert_eq!(0.24053641567148587, random.next_double());
        assert_eq!(0.6374174253501083, random.next_double());
    }
}
