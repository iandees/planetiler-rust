//! Port of `layers/Building.java`: defines the logic for generating map elements for buildings in the `building`
//! layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles building sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/building).
//!
//! Emit all buildings from OSM data at z14.
//!
//! At z13, emit all buildings at process-time, but then at tile render-time, merge buildings that are overlapping or
//! almost touching into combined buildings so that entire city blocks show up as a single building polygon.
//!
//! THIS IS VERY EXPENSIVE! Merging buildings at z13 adds about 50% to the total map generation time. To disable it,
//! set `building_merge_z13` argument to false.

use std::borrow::Cow;
use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor, OsmRelationPreprocessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::reader::osm::osm_element::Relation;
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::vector_tile;

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::building::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::util::utils::null_if_empty;

fn material_color(material: &str) -> Option<&'static str> {
    Some(match material {
        "cement_block" => "#6a7880",
        "brick" => "#bd8161",
        "plaster" => "#dadbdb",
        "wood" => "#d48741",
        "concrete" => "#d3c2b0",
        "metal" => "#b7b1a6",
        "stone" => "#b4a995",
        "mud" => "#9d8b75",
        "steel" => "#b7b1a6", // same as metal
        "glass" => "#5a81a0",
        "traditional" => "#bd8161", // same as brick
        "masonry" => "#bd8161",     // same as brick
        "Brick" => "#bd8161",       // same as brick
        "tin" => "#b7b1a6",         // same as metal
        "timber_framing" => "#b3b0a9",
        "sandstone" => "#b4a995", // same as stone
        "clay" => "#9d8b75",      // same as mud
        _ => return None,
    })
}

pub struct Building {
    merge_z13_buildings: bool,
}

/// Info about a `type=building` relation.
#[derive(Debug, Clone, Copy)]
pub struct BuildingRelationInfo {
    pub id: i64,
}

impl OsmRelationInfo for BuildingRelationInfo {
    fn id(&self) -> i64 {
        self.id
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        // CLASS_HEADER_BYTES + long
        16 + 8
    }
}

impl Building {
    pub fn new(_translations: &Arc<Translations>, config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> Building {
        Building {
            merge_z13_buildings: config.arguments.get_boolean(
                "building_merge_z13",
                "building layer: merge nearby buildings at z13",
                true,
            ),
        }
    }
}

impl OsmRelationPreprocessor for Building {
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        if relation.has_tag_str("type", "building") {
            Some(vec![Arc::new(BuildingRelationInfo { id: relation.id })])
        } else {
            None
        }
    }
}

fn first_non_empty<'a>(a: Option<Cow<'a, str>>, b: Option<Cow<'a, str>>) -> Option<Cow<'a, str>> {
    null_if_empty(a).or_else(|| null_if_empty(b))
}

fn parse_double(s: Option<Cow<'_, str>>) -> Option<f64> {
    s.as_deref().and_then(parse::parse_double_or_null_str)
}

impl OsmBuildingPolygonHandler for Building {
    fn process_osm_building_polygon(&self, element: &OsmBuildingPolygon<'_>, features: &mut FeatureCollector<'_>) {
        let source = element.source;
        let hide_3d = source
            .relation_infos()
            .iter()
            .any(|r| !r.is_super_relation() && r.relation.is::<BuildingRelationInfo>() && &*r.role == "outline")
            .then_some(true);

        let mut color: Option<Cow<'_, str>> = element.colour();
        if color.is_none()
            && let Some(material) = element.material()
        {
            color = material_color(&material).map(Cow::Borrowed);
        }
        let color = color.map(|c| if c.chars().any(|ch| ch.is_uppercase()) { Cow::Owned(c.to_lowercase()) } else { c });

        let height = first_non_empty(element.height(), element.buildingheight()).as_deref().and_then(parse::meters_str);
        let min_height =
            first_non_empty(element.min_height(), element.buildingmin_height()).as_deref().and_then(parse::meters_str);
        let levels = parse_double(element.levels()).or_else(|| parse_double(element.buildinglevels()));
        let min_levels = parse_double(element.min_level()).or_else(|| parse_double(element.buildingmin_level()));

        let render_height = height.unwrap_or_else(|| levels.map_or(5.0, |l| l * 3.66)).ceil() as i32;
        let render_min_height = min_height.unwrap_or_else(|| min_levels.map_or(0.0, |l| l * 3.66)).floor() as i32;

        if render_height < 3660 && render_min_height < 3660 {
            let feature = features
                .polygon(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_min_zoom(13)
                .set_min_pixel_size(2.0)
                .set_attr_with_minzoom(fields::RENDER_HEIGHT, render_height, 14)
                .set_attr_with_minzoom(fields::RENDER_MIN_HEIGHT, render_min_height, 14)
                .set_attr_with_minzoom(fields::COLOUR, color.as_deref(), 14)
                .set_attr_with_minzoom(fields::HIDE_3D, hide_3d, 14)
                .set_sort_key(render_height);
            if self.merge_z13_buildings {
                feature.set_min_pixel_size(0.1).set_pixel_tolerance(0.25);
            }
        }
    }
}

impl LayerPostProcessor for Building {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        let input = std::mem::take(items);
        *items = if self.merge_z13_buildings && zoom == 13 {
            feature_merge::merge_nearby_polygons(input, 4.0, 4.0, 0.5, 0.5)?
        } else {
            // reduces the size of some heavy z14 tiles with many small buildings by 60% or more
            feature_merge::merge_multi_polygon(input)
        };
        Ok(())
    }
}

impl Handler for Building {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }

    fn as_osm_relation_preprocessor(&self) -> Option<&dyn OsmRelationPreprocessor> {
        Some(self)
    }
}

impl RowHandlers for Building {
    fn as_osm_building_polygon_handler(&self) -> Option<&dyn OsmBuildingPolygonHandler> {
        Some(self)
    }
}

impl Layer for Building {}

#[cfg(test)]
mod tests {
    use geo::CoordsIter;
    use planetiler_core::geo::geo_utils;
    use planetiler_core::geo::test_utils::{rectangle, rectangle_xy};
    use planetiler_core::profile::Profile;
    use planetiler_core::reader::osm::osm_relation_info::RelationMember;
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::*;
    use crate::open_map_tiles_profile::OSM_SOURCE;
    use crate::test_support::*;

    #[test]
    fn test_building() {
        let t = LayerTest::new();
        assert_features(
            13,
            &[tags! {
                "colour" => "<null>",
                "hide_3d" => "<null>",
                "_layer" => "building",
                "_type" => "polygon",
                "_minzoom" => 13,
                "_maxzoom" => 14,
                "_buffer" => 4.0,
                "_minpixelsize" => 0.1,
            }],
            &t.process(&polygon_feature(tags! {"building" => "yes"})),
        );
        assert_features(
            13,
            &[tags! {"_layer" => "building", "_type" => "polygon"}],
            &t.process(&polygon_feature(tags! {"building:part" => "yes"})),
        );
        assert_features(13, &[], &t.process(&polygon_feature(tags! {"building" => "no"})));
    }

    #[test]
    fn test_ignore_underground_building() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[],
            &t.process(&polygon_feature(tags! {"building" => "yes", "location" => "underground"})),
        );
    }

    #[test]
    fn test_airport_buildings() {
        let t = LayerTest::new();
        for aeroway in ["terminal", "hangar"] {
            assert_features(
                13,
                &[tags! {"_layer" => "building", "_type" => "polygon"}],
                &t.process(&polygon_feature(tags! {"aeroway" => aeroway})),
            );
        }
    }

    #[test]
    fn test_render_heights() {
        let t = LayerTest::new();
        assert_features(
            13,
            &[tags! {"render_height" => "<null>", "render_min_height" => "<null>"}],
            &t.process(&polygon_feature(tags! {"building" => "yes"})),
        );
        assert_features(
            14,
            &[tags! {"render_height" => 5, "render_min_height" => 0}],
            &t.process(&polygon_feature(tags! {"building" => "yes"})),
        );
        assert_features(
            14,
            &[tags! {"render_height" => 12, "render_min_height" => 3}],
            &t.process(&polygon_feature(
                tags! {"building" => "yes", "building:min_height" => "3", "building:height" => "12"},
            )),
        );
        assert_features(
            14,
            &[tags! {"render_height" => 44, "render_min_height" => 10}],
            &t.process(&polygon_feature(
                tags! {"building" => "yes", "building:min_level" => "3", "building:levels" => "12"},
            )),
        );
        assert_features(
            14,
            &[],
            &t.process(&polygon_feature(
                tags! {"building" => "yes", "building:min_level" => "1500", "building:levels" => "1500"},
            )),
        );
    }

    #[test]
    fn test_render_height_units() {
        let t = LayerTest::new();
        for (input, expected) in [("2", 2), ("2m", 2), ("2 m", 2), ("2.5 m", 3), ("6'", 2), ("6'10\"", 3)] {
            assert_features(
                14,
                &[tags! {"render_height" => expected}],
                &t.process(&polygon_feature(tags! {"building" => "yes", "building:height" => input})),
            );
        }
    }

    #[test]
    fn test_outline_hides_3d() {
        let t = LayerTest::new();
        let mut relation = Relation::new(1);
        relation.tags.insert("type".into(), "building".into());
        let relation_infos: Vec<RelationMember<dyn OsmRelationInfo>> = t
            .profile
            .preprocess_osm_relation(&relation)
            .unwrap()
            .into_iter()
            .map(|i| RelationMember::new("outline", i))
            .collect();
        let feature = SimpleFeature::create_fake_osm_feature(
            geo_utils::world_to_lat_lon_coords(&rectangle(0.0, 1.0).into()),
            tags! {"building" => "yes"},
            Some(OSM_SOURCE),
            None::<&str>,
            0,
            relation_infos,
            None,
        );
        assert_features(14, &[tags! {"_layer" => "building", "hide_3d" => true}], &t.process(&feature));
    }

    #[test]
    fn test_merge_polygons_z13() {
        let t = LayerTest::new();
        let poly1 = vt_feature(LAYER_NAME, 1, &rectangle(10.0, 20.0).into(), Tags::new(), 0);
        let poly2 = vt_feature(LAYER_NAME, 1, &rectangle_xy(20.0, 10.0, 22.0, 20.0).into(), Tags::new(), 0);
        let num_geometries = |f: &vector_tile::Feature| match f.geometry.decode().unwrap() {
            geo_types::Geometry::MultiPolygon(mp) => mp.0.len(),
            geo_types::Geometry::Polygon(_) => 1,
            other => panic!("unexpected {:?}", other.coords_count()),
        };

        let z14 = t.post_process(LAYER_NAME, 14, vec![poly1.clone(), poly2.clone()]);
        // merged into 1 multipolygon
        assert_eq!(1, z14.len());
        assert_eq!(2, num_geometries(&z14[0]));
        let z13 = t.post_process(LAYER_NAME, 13, vec![poly1, poly2]);
        assert_eq!(1, z13.len());
        assert_eq!(1, num_geometries(&z13[0]));
    }

    #[test]
    fn test_color() {
        let t = LayerTest::new();
        assert_features(
            14,
            &[tags! {"colour" => "#ff0000"}],
            &t.process(&polygon_feature(
                tags! {"building" => "yes", "building:colour" => "#ff0000", "building:material" => "brick"},
            )),
        );
        assert_features(
            14,
            &[tags! {"colour" => "#bd8161"}],
            &t.process(&polygon_feature(
                tags! {"building" => "yes", "building:building" => "yes", "building:material" => "brick"},
            )),
        );
        assert_features(
            13,
            &[tags! {"colour" => "<null>"}],
            &t.process(&polygon_feature(
                tags! {"building" => "yes", "building:building" => "yes", "building:colour" => "#ff0000"},
            )),
        );
    }
}
