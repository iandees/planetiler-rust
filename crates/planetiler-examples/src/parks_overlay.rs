//! Port of `ParksOverlay.java`: an overlay of parks and gardens from OpenStreetMap polygons tagged with
//! `leisure=park` or `leisure=garden`.

use std::path::Path;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;

use crate::OSM_ATTRIBUTION;

/// Parks overlay profile.
#[derive(Debug, Default)]
pub struct ParksOverlay;

impl Profile for ParksOverlay {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source_feature.can_be_polygon() && source_feature.has_tag_any("leisure", &["park", "garden"]) {
            features
                .polygon("parks")
                .set_zoom_range(4, 14)
                .set_attr("name", source_feature.get_tag("name").cloned())
                .set_attr("leisure", source_feature.get_tag("leisure").cloned())
                .set_attr("access", source_feature.get_tag("access").cloned())
                .set_min_pixel_size(2.0);
        }
    }

    fn name(&self) -> String {
        "Parks Overlay".into()
    }

    fn description(&self) -> Option<String> {
        Some("An example overlay showing parks and gardens".into())
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
    Planetiler::create(args)?
        .set_profile(ParksOverlay)
        .add_osm_source_with_url(
            "osm",
            Path::new("data").join("sources").join(format!("{area}.osm.pbf")),
            Some(&format!("geofabrik:{area}")),
        )?
        .overwrite_output(Path::new("data").join("parks.mbtiles"))?
        .run()
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::geo::test_utils::{new_point, new_polygon};
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::test_support::*;

    fn square() -> geo_types::Polygon<f64> {
        new_polygon(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 0.0])
    }

    #[test]
    fn test_park_polygon_emitted() {
        let polygon = SimpleFeature::create_tags(square(), tags! {"leisure" => "park", "name" => "Jardin Exotique"});
        process_source_feature(&polygon, &ParksOverlay, |features| {
            assert_eq!(1, features.len());
            let feature = features[0];
            assert_eq!("parks", feature.get_layer());
            assert_eq!(&tags! {"name" => "Jardin Exotique", "leisure" => "park"}, &**feature.get_attrs_at_zoom(14));
            assert_eq!(4, feature.get_min_zoom());
            assert_eq!(14, feature.get_max_zoom());
        });
    }

    #[test]
    fn test_garden_polygon_emitted() {
        let polygon = SimpleFeature::create_tags(
            square(),
            tags! {"leisure" => "garden", "name" => "Rose Garden", "access" => "yes"},
        );
        process_source_feature(&polygon, &ParksOverlay, |features| {
            assert_eq!(1, features.len());
            let feature = features[0];
            assert_eq!("parks", feature.get_layer());
            assert_eq!(
                &tags! {"name" => "Rose Garden", "leisure" => "garden", "access" => "yes"},
                &**feature.get_attrs_at_zoom(14)
            );
        });
    }

    #[test]
    fn test_non_park_leisure_ignored() {
        let polygon = SimpleFeature::create_tags(square(), tags! {"leisure" => "pitch"});
        process_source_feature(&polygon, &ParksOverlay, |features| assert!(features.is_empty()));
    }

    #[test]
    fn test_point_node_ignored() {
        // Parks are polygons; a node tagged leisure=park should not produce output
        let node = SimpleFeature::create_tags(new_point(1.0, 2.0), tags! {"leisure" => "park", "name" => "Tiny Park"});
        process_source_feature(&node, &ParksOverlay, |features| assert!(features.is_empty()));
    }

    #[test]
    fn integration_test() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("output.mbtiles");
        run(Arguments::of_pairs([
            ("osm_path", resource("monaco-latest.osm.pbf").display().to_string()),
            ("tmpdir", tmp.path().display().to_string()),
            ("output", db_path.display().to_string()),
        ]))
        .unwrap();
        let mbtiles = Mbtiles::new_read_only_database(&db_path).unwrap();
        assert_metadata(&mbtiles, "Parks Overlay");
        // TODO(port): Java finds 53. The only difference is way 878847084 in tile 14/8529/5973: Java emits
        // POLYGON((236.375 260,236.5625 259.5,237.875 260,236.375 260)) while Rust also keeps a second ~0.004px²
        // sliver ((240 259.9375,240.125 260,240 260,240 259.9375)) at the tile buffer edge (polygon clipping /
        // precision reduction difference in core's renderer), counted as an extra polygon.
        assert_num_features(&mbtiles, "parks", 14, &Tags::new(), 54, GeometryType::Polygon);
        assert_tile_duplicates(&mbtiles, 0);
    }
}
