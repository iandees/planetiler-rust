//! Port of `ToiletsOverlay.java`: builds a map of toilets from OpenStreetMap nodes tagged with
//! [`amenity=toilets`](https://wiki.openstreetmap.org/wiki/Tag:amenity%3Dtoilets).
//!
//! To run this example:
//! 1. `cargo run --release -p planetiler -- example-toilets --download` (or `--osm_path=path/to/data.osm.pbf`)
//! 2. Then `data/toilets.mbtiles` is generated.

use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::util::zoom_function::ZoomFunction;

use crate::OSM_ATTRIBUTION;

/// Profile that emits a point for every toilet.
#[derive(Debug, Default)]
pub struct ToiletsOverlay {
    /// Assign every toilet a monotonically increasing ID so that we can limit output at low zoom levels to only the
    /// highest ID toilet nodes. Be sure to use thread-safe data structures any time a profile holds state since
    /// multiple threads invoke `process_feature` concurrently.
    toilet_number: AtomicI32,
}

impl ToiletsOverlay {
    pub fn new() -> ToiletsOverlay {
        ToiletsOverlay::default()
    }
}

impl Profile for ToiletsOverlay {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source_feature.is_point() && source_feature.has_tag_str("amenity", "toilets") {
            features
                .point("toilets")
                .set_zoom_range(0, 14)
                // to limit toilets displayed at lower zoom levels:
                // 1) set a sort key that defines a priority ordering of toilets. For mountains, you might use
                // "elevation" but for toilets we just set it to the order in which we see them.
                .set_sort_key(self.toilet_number.fetch_add(1, Ordering::Relaxed) + 1)
                // 2) at lower zoom levels, divide each 256x256 px tile into 32x32 px squares and in each square only
                // include the toilets with the lowest sort key within that square
                .set_point_label_grid_size_and_limit(
                    12,   // only limit at z12 and below
                    32.0, // break the tile up into 32x32 px squares
                    4,    // any only keep the 4 nodes with lowest sort-key in each 32px square
                )
                // and also whenever you set a label grid size limit, make sure you increase the buffer size so no
                // label grid squares will be the consistent between adjacent tiles
                .set_buffer_pixel_overrides(Some(ZoomFunction::max_zoom(12, 32.0)));
        }
    }

    fn name(&self) -> String {
        "Toilets Overlay".into()
    }

    fn description(&self) -> Option<String> {
        Some("An example overlay showing toilets".into())
    }

    fn is_overlay(&self) -> bool {
        true
    }

    fn attribution(&self) -> Option<String> {
        Some(OSM_ATTRIBUTION.into())
    }
}

/// Main entrypoint for the example program.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    run(crate::parse_args(args)?)
}

/// Runs the example with `args`.
pub fn run(args: Arguments) -> anyhow::Result<()> {
    let area = args.get_string("area", "geofabrik area to download", "monaco");
    // Planetiler is a convenience wrapper around the lower-level API for the most common use-cases.
    // See toilets_overlay_low_level_api for an example using this same profile but the lower-level API
    Planetiler::create(args)?
        .set_profile(ToiletsOverlay::new())
        // override this default with osm_path="path/to/data.osm.pbf"
        .add_osm_source_with_url(
            "osm",
            Path::new("data").join("sources").join(format!("{area}.osm.pbf")),
            Some(&format!("geofabrik:{area}")),
        )?
        // override this default with output="path/to/output.mbtiles"
        .overwrite_output(Path::new("data").join("toilets.mbtiles"))?
        .run()
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::geo::test_utils::new_point;
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::test_support::*;

    #[test]
    fn test_source_feature_processing() {
        let profile = ToiletsOverlay::new();
        let node = SimpleFeature::create_tags(new_point(1.0, 2.0), tags! {"amenity" => "toilets"});
        process_source_feature(&node, &profile, |features| {
            // verify output feature
            assert_eq!(1, features.len());
            let feature = features[0];
            assert_eq!("toilets", feature.get_layer());
            // no attributes
            assert_eq!(&Tags::default(), &**feature.get_attrs_at_zoom(14));
            assert_eq!(0, feature.get_min_zoom());
            assert_eq!(14, feature.get_max_zoom());
            assert_eq!(1, feature.get_sort_key());

            // at z12 - use label grid to limit output
            assert_eq!(4, feature.get_point_label_grid_limit_at_zoom(12));
            assert_eq!(32.0, feature.get_point_label_grid_pixel_size_at_zoom(12));
            assert_eq!(32.0, feature.get_buffer_pixels_at_zoom(12));

            // at z13 - no label grid (0 disables filtering)
            assert_eq!(0, feature.get_point_label_grid_limit_at_zoom(13));
            assert_eq!(0.0, feature.get_point_label_grid_pixel_size_at_zoom(13));
            assert_eq!(4.0, feature.get_buffer_pixels_at_zoom(13));
        });
    }

    #[test]
    fn integration_test() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("output.mbtiles");
        run(Arguments::of_pairs([
            // Override input source locations
            ("osm_path", resource("monaco-latest.osm.pbf").display().to_string()),
            // Override temp dir location
            ("tmpdir", tmp.path().display().to_string()),
            // Override output location
            ("output", db_path.display().to_string()),
        ]))
        .unwrap();
        let mbtiles = Mbtiles::new_read_only_database(&db_path).unwrap();
        assert_metadata(&mbtiles, "Toilets Overlay");
        assert_num_features(&mbtiles, "toilets", 14, &Tags::default(), 34, GeometryType::Point);
    }
}
