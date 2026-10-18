//! Port of `AdminBordersOverlay.java`: an overlay of administrative borders (lines) and labels for named
//! administrative areas (points) from OpenStreetMap `boundary=administrative` ways and relations.

use std::path::Path;

use planetiler_core::config::arguments::Arguments;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::planetiler::Planetiler;
use planetiler_core::profile::Profile;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::util::parse;
use planetiler_core::vector_tile;

use crate::{OSM_ATTRIBUTION, feature_merge};

const BORDER_LAYER_NAME_PREFIX: &str = "admin_borders-";
const POLYGON_LABEL_LAYER_NAME_PREFIX: &str = "polygon_area_labels-";

struct AdminLevelConfig {
    kind: &'static str,
    label_level: &'static str,
    border_min_zoom: u32,
    label_min_zoom: u32,
    label_max_zoom: u32,
}

const fn level(
    kind: &'static str,
    label_level: &'static str,
    border_min_zoom: u32,
    label_min_zoom: u32,
    label_max_zoom: u32,
) -> AdminLevelConfig {
    AdminLevelConfig { kind, label_level, border_min_zoom, label_min_zoom, label_max_zoom }
}

const CONFIG_COUNTRY: AdminLevelConfig = level("country", "Country", 0, 2, 5);
const CONFIG_REGION: AdminLevelConfig = level("region", "Region", 4, 6, 8);
const CONFIG_COUNTY: AdminLevelConfig = level("county", "County", 7, 9, 10);
const CONFIG_CITY: AdminLevelConfig = level("city", "City", 9, 11, 12);
const CONFIG_CITY_LEVEL_8: AdminLevelConfig = level("city", "City", 11, 12, 13);
const CONFIG_LOCAL: AdminLevelConfig = level("local", "Local", 12, 12, 14);

fn config_for_admin_level(admin_level: i32) -> &'static AdminLevelConfig {
    match admin_level {
        2 => &CONFIG_COUNTRY,
        3 | 4 => &CONFIG_REGION,
        5 | 6 => &CONFIG_COUNTY,
        7 => &CONFIG_CITY,
        8 => &CONFIG_CITY_LEVEL_8,
        _ => &CONFIG_LOCAL,
    }
}

/// Administrative borders overlay profile.
#[derive(Debug, Default)]
pub struct AdminBordersOverlay;

impl Profile for AdminBordersOverlay {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if !source_feature.has_tag_str("boundary", "administrative") {
            return;
        }
        let admin_level = source_feature.get_tag("admin_level").and_then(parse::parse_round_int);
        let Some(admin_level) = admin_level.filter(|l| (2..=10).contains(l)) else { return };
        let config = config_for_admin_level(admin_level);
        let name = source_feature.get_string_or("name", "");

        if source_feature.can_be_line() {
            features
                .line(&format!("{BORDER_LAYER_NAME_PREFIX}{}", config.kind))
                .set_attr("admin_level", admin_level)
                .set_attr("kind", config.kind)
                .set_attr("name", name.as_str())
                .set_attr("maritime", source_feature.get_boolean("maritime"))
                .set_zoom_range(config.border_min_zoom, 14)
                .set_min_pixel_size(0.0);
        }

        if source_feature.can_be_polygon() && !name.trim().is_empty() {
            features
                .innermost_point(&format!("{POLYGON_LABEL_LAYER_NAME_PREFIX}{}", config.kind))
                .set_attr("name", name.as_str())
                .set_attr("admin_level", admin_level)
                .set_attr("kind", config.kind)
                .set_attr("label_level", config.label_level)
                .set_attr("label_text", format!("{}: {name}", config.label_level))
                .set_zoom_range(config.label_min_zoom, config.label_max_zoom)
                .set_buffer_pixels(64.0);
        }
    }

    fn post_process_layer_features(
        &self,
        _layer: &str,
        _zoom: u32,
        items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        *items = feature_merge::merge_line_strings(std::mem::take(items), 0.5, 0.1, 4.0);
        Ok(())
    }

    fn name(&self) -> String {
        "Administrative Borders Overlay".into()
    }

    fn description(&self) -> Option<String> {
        Some("An example overlay showing country, region, and city administrative borders".into())
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
        .set_profile(AdminBordersOverlay)
        .add_osm_source_with_url(
            "osm",
            Path::new("data").join("sources").join(format!("{area}.osm.pbf")),
            Some(&format!("geofabrik:{area}")),
        )?
        .overwrite_output(Path::new("data").join("admin-borders.mbtiles"))?
        .run()
}

#[cfg(test)]
mod tests {
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::geo::test_utils::{new_line_string, new_polygon};
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::{Tags, Value};

    use super::*;
    use crate::test_support::*;

    /// adminLevel, kind, borderMinZoom, labelMinZoom, labelMaxZoom
    const LEVELS: &[(i32, &str, u32, u32, u32)] = &[
        (2, "country", 0, 2, 5),
        (3, "region", 4, 6, 8),
        (4, "region", 4, 6, 8),
        (5, "county", 7, 9, 10),
        (6, "county", 7, 9, 10),
        (7, "city", 9, 11, 12),
        (8, "city", 11, 12, 13),
        (9, "local", 12, 12, 14),
        (10, "local", 12, 12, 14),
    ];

    fn square() -> geo_types::Polygon<f64> {
        new_polygon(&[0.0, 0.0, 3.0, 0.0, 3.0, 3.0, 0.0, 3.0, 0.0, 0.0])
    }

    #[test]
    fn test_country_boundary_feature() {
        let boundary = SimpleFeature::create_tags(
            new_line_string(&[10.0, 20.0, 30.0, 40.0]),
            tags! {"boundary" => "administrative", "admin_level" => "2", "name" => "Sample Border"},
        );
        process_source_feature(&boundary, &AdminBordersOverlay, |features| {
            assert_eq!(1, features.len(), "country boundary should produce exactly 1 feature");
            let feature = features[0];
            assert_eq!("admin_borders-country", feature.get_layer());
            assert_eq!(0, feature.get_min_zoom());
            assert_eq!(14, feature.get_max_zoom());
            assert_eq!(
                &tags! {"admin_level" => 2, "kind" => "country", "name" => "Sample Border", "maritime" => false},
                &**feature.get_attrs_at_zoom(8)
            );
        });
    }

    #[test]
    fn test_country_label_comes_from_polygon_center() {
        let polygon = SimpleFeature::create_tags(
            square(),
            tags! {"boundary" => "administrative", "admin_level" => "2", "name" => "Monaco"},
        );
        process_source_feature(&polygon, &AdminBordersOverlay, |features| {
            assert_eq!(1, features.len(), "country polygon should produce exactly 1 label feature");
            let label = features[0];
            assert_eq!("polygon_area_labels-country", label.get_layer());
            assert_eq!(2, label.get_min_zoom());
            assert_eq!(5, label.get_max_zoom());
            assert_eq!(
                &tags! {
                    "admin_level" => 2,
                    "kind" => "country",
                    "name" => "Monaco",
                    "label_level" => "Country",
                    "label_text" => "Country: Monaco",
                },
                &**label.get_attrs_at_zoom(8)
            );
        });
    }

    #[test]
    fn test_border_zoom_ranges_for_all_admin_levels() {
        for &(admin_level, kind, border_min_zoom, _, _) in LEVELS {
            let boundary = SimpleFeature::create_tags(
                new_line_string(&[10.0, 20.0, 30.0, 40.0]),
                tags! {"boundary" => "administrative", "admin_level" => admin_level.to_string(), "name" => format!("L{admin_level}")},
            );
            process_source_feature(&boundary, &AdminBordersOverlay, |features| {
                assert_eq!(1, features.len(), "admin_level={admin_level}");
                let border = features[0];
                assert_eq!(format!("admin_borders-{kind}"), border.get_layer(), "admin_level={admin_level}");
                assert_eq!(border_min_zoom, border.get_min_zoom(), "admin_level={admin_level}");
                assert_eq!(14, border.get_max_zoom(), "admin_level={admin_level}");
            });
        }
    }

    #[test]
    fn test_label_zoom_ranges_for_all_admin_levels() {
        for &(admin_level, kind, _, label_min_zoom, label_max_zoom) in LEVELS {
            let polygon = SimpleFeature::create_tags(
                square(),
                tags! {"boundary" => "administrative", "admin_level" => admin_level.to_string(), "name" => format!("A{admin_level}")},
            );
            process_source_feature(&polygon, &AdminBordersOverlay, |features| {
                assert_eq!(1, features.len(), "admin_level={admin_level}");
                let label = features[0];
                assert_eq!(format!("polygon_area_labels-{kind}"), label.get_layer());
                assert_eq!(label_min_zoom, label.get_min_zoom(), "admin_level={admin_level}");
                assert_eq!(label_max_zoom, label.get_max_zoom(), "admin_level={admin_level}");
                assert_eq!(Some(&Value::str(kind)), label.get_attrs_at_zoom(label_min_zoom).get("kind"));
            });
        }
    }

    #[test]
    fn test_label_for_zoom11_only_includes_city_and_local_kinds() {
        for &(admin_level, _, _, _, _) in LEVELS {
            let polygon = SimpleFeature::create_tags(
                square(),
                tags! {"boundary" => "administrative", "admin_level" => admin_level.to_string(), "name" => format!("Z{admin_level}")},
            );
            process_source_feature(&polygon, &AdminBordersOverlay, |features| {
                let label = features[0];
                let visible_at_z11 = 11 >= label.get_min_zoom() && 11 <= label.get_max_zoom();
                assert_eq!(admin_level == 7, visible_at_z11, "admin_level={admin_level}");
            });
        }
    }

    #[test]
    fn test_ignores_non_administrative_boundaries() {
        let boundary = SimpleFeature::create_tags(
            new_line_string(&[10.0, 20.0, 30.0, 40.0]),
            tags! {"boundary" => "national_park", "admin_level" => "2"},
        );
        process_source_feature(&boundary, &AdminBordersOverlay, |features| assert!(features.is_empty()));
    }

    #[test]
    fn test_tile_post_processing_merges_connected_lines() {
        let layer = "admin_borders-country";
        let attrs = tags! {"admin_level" => 2, "kind" => "country"};
        let line = |id, coords: &[f64]| {
            vector_tile::Feature::new(
                layer,
                id,
                vector_tile::encode_geometry(&new_line_string(coords).into()),
                attrs.clone(),
            )
        };
        let mut items = vec![line(1, &[0.0, 0.0, 10.0, 0.0]), line(2, &[10.0, 0.0, 20.0, 0.0])];
        AdminBordersOverlay.post_process_layer_features(layer, 10, &mut items).unwrap();
        assert_eq!(1, items.len(), "two connected lines should be merged into 1 line");
        assert_eq!(vec![line(0, &[0.0, 0.0, 20.0, 0.0])], items);
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
        assert_metadata(&mbtiles, "Administrative Borders Overlay");
        let none = Tags::new();
        assert!(num_features(&mbtiles, "admin_borders-country", 5, &none, GeometryType::Line) > 0);
        assert!(
            num_features(&mbtiles, "admin_borders-city", 11, &none, GeometryType::Line) > 0,
            "expected city-level border lines at higher zoom"
        );
        assert!(
            num_features(&mbtiles, "polygon_area_labels-city", 13, &none, GeometryType::Point) > 0,
            "expected named city-level labels at zoom 13+"
        );
        assert_tile_duplicates(&mbtiles, 0);
    }
}
