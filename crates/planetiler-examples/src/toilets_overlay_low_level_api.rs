//! Port of `ToiletsOverlayLowLevelApi.java`: an alternative implementation of [`ToiletsOverlay`] that uses the
//! low-level planetiler APIs instead of the [`Planetiler`](planetiler_core::planetiler::Planetiler) convenience
//! wrapper.
//!
//! Differences from Java: configuration is read from environment variables (Java reads JVM `-D` properties), and the
//! layer stats file (only written with `--output_layer_stats`) goes to `<output>.layerstats.tsv.gz` (Java passes the
//! archive path itself).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use planetiler_core::archive::tile_archive_config::TileArchiveConfig;
use planetiler_core::archive::tile_archive_metadata::TileArchiveMetadata;
use planetiler_core::archive::{tile_archive_writer, tile_archives};
use planetiler_core::collection::feature_group::FeatureGroup;
use planetiler_core::collection::{long_long_map, long_long_multimap};
use planetiler_core::config::arguments::Arguments;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::geo::tile_order::TileOrder;
use planetiler_core::profile::Profile;
use planetiler_core::reader::osm::osm_input_file::OsmInputFile;
use planetiler_core::reader::osm::osm_reader::OsmReader;
use planetiler_core::stats::stats::InMemoryStats;
use planetiler_core::util::file_utils::{self, DiskBacked};
use planetiler_core::util::tile_size_stats;

use crate::toilets_overlay::ToiletsOverlay;

/// Main entrypoint for the example program (ignores `args` like Java).
pub fn main<S: AsRef<str>>(_args: &[S]) -> anyhow::Result<()> {
    run(
        &Path::new("data").join("sources").join("input.pbf"),
        &Path::new("data").join("tmp"),
        &Path::new("data").join("toilets.mbtiles"),
    )
}

struct FileSize(PathBuf);

impl DiskBacked for FileSize {
    fn disk_usage_bytes(&self) -> u64 {
        file_utils::file_size(&self.0)
    }
}

/// Reads `input`, using `tmp_dir` for temporary files, and writes toilets to `output`.
pub fn run(input: &Path, tmp_dir: &Path, output: &Path) -> anyhow::Result<()> {
    let stats = InMemoryStats::arc();
    let profile: Arc<dyn Profile> = Arc::new(ToiletsOverlay::new());

    let config = Arc::new(PlanetilerConfig::from(&Arguments::from_environment())?);

    let tile_archive_metadata = TileArchiveMetadata::from_profile(&*profile, &config);

    file_utils::delete_file(output);
    std::fs::create_dir_all(tmp_dir)?;

    // Set up the FeatureGroup utility that groups features by tile. To group features by tile, FeatureSort first
    // sorts them by a key that includes the tile ID in it, then FeatureGroup reads features in order, batching all
    // consecutive features in the same tile together.
    //
    // Most applications should use external merge sort which writes features to disk, sorts chunks from disk in
    // parallel, and iterates through the sorted chunks to efficiently sort more data than fits in memory - but if you
    // have a lot of RAM there is an in-memory option too.
    let feature_group = Arc::new(FeatureGroup::new_disk_backed(
        TileOrder::Tms,
        &tmp_dir.join("feature.db"),
        profile.clone(),
        config.clone(),
        stats.clone(),
        false,
    )?);

    {
        // OSM nodes each have a latitude/longitude, and ways are a collection of node IDs so to get the shape of ways
        // we need to store the latitude and longitude of each node and look them up when processing each way. There
        // are several in-memory and disk-based options, but even for the disk-based ones you'll likely need enough
        // RAM to fit the whole thing for random-access lookups to be fast.
        //
        // BUT, since this profile only needs nodes and not ways we can use the noop version to avoid storing any
        // node locations.
        let node_locations = Arc::from(long_long_map::noop());
        let multipolygons = Box::new(long_long_multimap::noop());
        let osm_reader = OsmReader::new(
            "osm",
            OsmInputFile::new(input, false).get()?,
            node_locations,
            multipolygons,
            profile.clone(),
            stats.clone(),
        );
        tracing::info!("Skipping OsmReader.pass1(config) since we don't care about ways or relations");
        let result = osm_reader.pass2(&feature_group, &config);
        osm_reader.close();
        profile.release();
        result?;
    }

    let mut feature_group =
        Arc::into_inner(feature_group).context("feature group still referenced after reading input")?;
    feature_group.prepare()?;

    let archive_config = TileArchiveConfig::from(&output.to_string_lossy())?;
    let mut write = || -> anyhow::Result<()> {
        let mut db = tile_archives::new_writer(&archive_config, &config)?;
        let layer_stats_path =
            tile_size_stats::get_default_layerstats_path(&archive_config.get_local_path().unwrap_or_default());
        tile_archive_writer::write_output(
            &mut feature_group,
            db.as_mut(),
            Arc::new(FileSize(output.to_path_buf())),
            &tile_archive_metadata,
            Some(&layer_stats_path),
            &config,
            &stats,
        )?;
        db.close()
    };
    write().with_context(|| format!("Unable to write to {}", output.display()))?;

    stats.print_summary();
    Ok(())
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::test_support::*;

    #[test]
    fn integration_test() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("output.mbtiles");
        run(&resource("monaco-latest.osm.pbf"), tmp.path(), &db_path).unwrap();
        let mbtiles = Mbtiles::new_read_only_database(&db_path).unwrap();
        assert_metadata(&mbtiles, "Toilets Overlay");
        assert_num_features(&mbtiles, "toilets", 14, &Tags::default(), 34, GeometryType::Point);
    }
}
