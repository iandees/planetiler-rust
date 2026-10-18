//! Port of `OsmQaTiles.java`: generates tiles with a raw copy of OSM data in a single "osm" layer at one zoom level,
//! similar to [OSM QA Tiles](http://osmlab.github.io/osm-qa-tiles/).
//!
//! Nodes are mapped to points and ways are mapped to polygons or linestrings, and multipolygon relations are mapped to
//! polygons. Each output feature contains all key/value tags from the input feature, plus these extra attributes:
//! `@type` (node, way, or relation), `@id`, `@version`, `@timestamp`, `@changeset`, `@uid` and `@user`.

use std::path::Path;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::osm::osm_element::OsmElement;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::value::Value;

use crate::OSM_ATTRIBUTION;

/// Raw OSM data profile.
#[derive(Debug, Default)]
pub struct OsmQaTiles;

/// Main entrypoint for the example program.
pub fn main<S: AsRef<str>>(args: &[S]) -> anyhow::Result<()> {
    run(crate::parse_args(args)?)
}

/// Runs the example with `in_args`.
pub fn run(in_args: Arguments) -> anyhow::Result<()> {
    let zoom = in_args.get_integer("zoom", "zoom level to generate tiles at", 12)?;
    let args = in_args.or_else(Arguments::of_pairs([
        ("minzoom", zoom.to_string()),
        ("maxzoom", zoom.to_string()),
        ("tile_warning_size_mb", "100".to_string()),
    ]));
    let area = args.get_string("area", "geofabrik area to download", "monaco");
    let url = if area.eq_ignore_ascii_case("planet") { "aws:latest".to_string() } else { format!("geofabrik:{area}") };
    Planetiler::create(args)?
        .set_profile(OsmQaTiles)
        .add_osm_source_with_url("osm", Path::new("data").join("sources").join(format!("{area}.osm.pbf")), Some(&url))?
        .overwrite_output(Path::new("data").join("qa.mbtiles"))?
        .run()
}

impl Profile for OsmQaTiles {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        // Java: `sourceFeature instanceof OsmSourceFeature`
        let Some(element) = source_feature.original_element() else { return };
        if source_feature.tags().is_empty() {
            return;
        }
        let feature = if source_feature.can_be_polygon() {
            features.polygon("osm")
        } else if source_feature.can_be_line() {
            features.line("osm")
        } else if source_feature.is_point() {
            features.point("osm")
        } else {
            return;
        };
        feature.set_min_pixel_size(0.0).set_pixel_tolerance(0.0).set_buffer_pixels(0.0);
        for (key, value) in source_feature.tags() {
            feature.set_attr(key.clone(), value);
        }
        feature.set_attr("@id", source_feature.id()).set_attr(
            "@type",
            match element {
                OsmElement::Node(_) => Some("node"),
                OsmElement::Way(_) => Some("way"),
                OsmElement::Relation(_) => Some("relation"),
                _ => None,
            },
        );
        if let Some(info) = element.info() {
            let non_zero = |v: i64| if v == 0 { Value::Null } else { Value::Int(v) };
            feature
                .set_attr("@version", non_zero(info.version as i64))
                .set_attr("@timestamp", non_zero(info.timestamp))
                .set_attr("@changeset", non_zero(info.changeset))
                .set_attr("@uid", non_zero(info.user_id as i64))
                .set_attr("@user", if info.user.trim().is_empty() { None } else { Some(info.user.clone()) });
        }
    }

    fn name(&self) -> String {
        "osm qa".into()
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
    use planetiler_core::reader::osm::osm_element::Info;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;

    use super::*;
    use crate::test_support::*;

    #[test]
    fn test_node() {
        // Java builds a custom OsmSourceFeature with world geometry (0.5, 0.5) = lat/lon (0, 0)
        let node = SimpleFeature::create_fake_osm_feature(
            new_point(0.0, 0.0),
            tags! {"key" => "value"},
            "source",
            "layer",
            0,
            vec![],
            Some(Info::new(1, 2, 3, 4, "user")),
        );
        process_source_feature(&node, &OsmQaTiles, |features| {
            // verify output attributes
            assert_eq!(1, features.len());
            let feature = features[0];
            assert_eq!("osm", feature.get_layer());
            assert_eq!(
                &tags! {
                    "key" => "value",
                    "@changeset" => 1,
                    "@timestamp" => 2,
                    "@id" => 0,
                    "@type" => "node",
                    "@uid" => 3,
                    "@user" => "user",
                    "@version" => 4,
                },
                &**feature.get_attrs_at_zoom(12)
            );
            assert_eq!(0.0, feature.get_buffer_pixels_at_zoom(12));
        });
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
        assert_metadata(&mbtiles, "osm qa");
        assert_num_features(
            &mbtiles,
            "osm",
            12,
            &tags! {
                "bus" => "yes",
                "name" => "Crémaillère",
                "public_transport" => "stop_position",
                "@type" => "node",
                "@version" => 4,
                "@id" => 1699777833i64,
            },
            1,
            GeometryType::Point,
        );
        assert_num_features(
            &mbtiles,
            "osm",
            12,
            &tags! {
                "boundary" => "administrative",
                "admin_level" => "10",
                "name" => "Monte-Carlo",
                "wikipedia" => "fr:Monte-Carlo",
                "ISO3166-2" => "MC-MC",
                "type" => "boundary",
                "wikidata" => "Q45240",
                "@type" => "relation",
                "@version" => 9,
                "@id" => 5986438,
            },
            1,
            GeometryType::Polygon,
        );
        assert_num_features(
            &mbtiles,
            "osm",
            12,
            &tags! {
                "name" => "Avenue de la Costa",
                "maxspeed" => "50",
                "lit" => "yes",
                "surface" => "asphalt",
                "lanes" => "2",
                "@type" => "way",
                "@version" => 5,
                "@id" => 166009791,
            },
            1,
            GeometryType::Line,
        );
    }
}
