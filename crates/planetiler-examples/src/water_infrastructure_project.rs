//! Port of `WaterInfrastructureProject.java`: a map of water supply points (wells, taps, pumps, drinking water) and
//! water storage (tanks, towers, reservoirs) from OpenStreetMap.

use std::path::Path;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;

use crate::OSM_ATTRIBUTION;

/// Water infrastructure profile.
#[derive(Debug, Default)]
pub struct WaterInfrastructureProject;

/// Main entrypoint for the example program.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    run(crate::parse_args(args)?)
}

/// Runs the example with `args`.
pub fn run(args: Arguments) -> anyhow::Result<()> {
    let area = args.get_string("area", "geofabrik area to download", "monaco");
    Planetiler::create(args)?
        .set_profile(WaterInfrastructureProject)
        .add_osm_source_with_url(
            "osm",
            Path::new("data").join("sources").join(format!("{area}.osm.pbf")),
            Some(&format!("geofabrik:{area}")),
        )?
        .overwrite_output(Path::new("data").join("water_map.mbtiles"))?
        .run()
}

impl Profile for WaterInfrastructureProject {
    fn process_feature(&self, source: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if source.has_tag_any("man_made", &["water_well", "water_tap", "water_pump"])
            || source.has_tag_any("amenity", &["drinking_water", "water_point"])
        {
            features
                .point("water_supply")
                .set_attr("name", source.get_string("name"))
                .set_attr("source", source.get_tag("man_made").cloned()) // is it a well or a pump
                .set_attr("operator", source.get_string("operator")) // who maintains it ?
                .set_zoom_range(10, 14);
        }
        if source.has_tag_any("man_made", &["storage_tank", "water_tower"])
            || source.has_tag_str("landuse", "reservoir")
        {
            features
                .point("water_storage")
                .set_attr("type", source.get_tag("man_made").cloned())
                .set_zoom_range(11, 14);
        }
    }

    fn name(&self) -> String {
        "Water Infrastructure Map".into()
    }

    fn description(&self) -> Option<String> {
        Some("Mapping water and amenities for GSoC project.".into())
    }

    fn attribution(&self) -> Option<String> {
        Some(OSM_ATTRIBUTION.into())
    }
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::geo::test_utils::new_point;
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::{Tags, Value};

    use super::*;
    use crate::test_support::*;

    #[test]
    fn test_water_well_mapping() {
        // 1. create a fake OSM well
        let input = SimpleFeature::create_tags(
            new_point(0.0, 0.0),
            tags! {"man_made" => "water_well", "name" => "Juba Central Well"},
        );
        // 2. Execute + 3. Verify: should be in 'water_supply' layer with the right name
        process_source_feature(&input, &WaterInfrastructureProject, |features| {
            assert_eq!(1, features.len());
            let feature = features[0];
            assert_eq!("water_supply", feature.get_layer());
            assert_eq!(Some(&Value::str("Juba Central Well")), feature.get_attrs_at_zoom(14).get("name"));
        });
    }

    #[test]
    fn test_ignore_non_water_amenities() {
        let input = SimpleFeature::create_tags(new_point(0.0, 0.0), tags! {"amenity" => "school"});
        process_source_feature(&input, &WaterInfrastructureProject, |features| assert!(features.is_empty()));
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
        assert_metadata(&mbtiles, "Water Infrastructure Map");
        assert_num_features(&mbtiles, "water_supply", 14, &Tags::default(), 16, GeometryType::Point);
        assert_tile_duplicates(&mbtiles, 0);
    }
}
