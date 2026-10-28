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

use geo::{Area, BoundingRect};
use geo_types::{Geometry, Polygon};
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor, OsmRelationPreprocessor};
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::geo::polygon_overlay;
use planetiler_core::reader::osm::osm_element::Relation;
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::value::Value;
use planetiler_core::vector_tile;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};

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

const TEMP_PART: &str = "_part";
/// Share of a building's area that its parts have to cover before the building is hidden in 3D.
const MIN_PART_COVERAGE: f64 = 0.5;

pub struct Building {
    merge_z13_buildings: bool,
    hide_outlines_with_parts: bool,
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
            hide_outlines_with_parts: config.arguments.get_boolean(
                "building_hide_outlines_with_parts",
                "building layer: set hide_3d on buildings that are mostly covered by building parts",
                false,
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

        let is_part = (self.hide_outlines_with_parts && element.buildingpart().is_some()).then_some(true);

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
                .set_attr_with_minzoom(TEMP_PART, is_part, 14)
                .set_sort_key(render_height);
            if self.merge_z13_buildings {
                feature.set_min_pixel_size(0.1).set_pixel_tolerance(0.25);
            }
        }
    }
}

impl LayerPostProcessor for Building {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        if self.hide_outlines_with_parts && zoom >= 14 {
            hide_outlines_covered_by_parts(items)?;
        }
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

type PartIndex = RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>;

/// Sets `hide_3d` on buildings that are mostly covered by `building:part`s, because
/// [Simple 3D Buildings](https://wiki.openstreetmap.org/wiki/Simple_3D_Buildings) does not use the outline of a
/// building for 3D rendering when it has parts.
fn hide_outlines_covered_by_parts(items: &mut [vector_tile::Feature]) -> GeometryResult<()> {
    let mut outlines = Vec::new();
    let mut parts: Vec<Polygon<f64>> = Vec::new();
    for (i, item) in items.iter_mut().enumerate() {
        if !item.tags.contains_key(TEMP_PART) {
            outlines.push(i);
            continue;
        }
        item.tags.shift_remove(TEMP_PART);
        match item.geometry.decode()? {
            Geometry::Polygon(p) => parts.push(p),
            Geometry::MultiPolygon(mp) => parts.extend(mp.0),
            _ => {}
        }
    }
    if parts.is_empty() {
        return Ok(());
    }
    let index: PartIndex = RTree::bulk_load(
        parts
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.bounding_rect().map(|r| (i, r)))
            .map(|(i, r)| GeomWithData::new(Rectangle::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y]), i))
            .collect(),
    );
    for i in outlines {
        let item = &mut items[i];
        if item.tags.get(fields::HIDE_3D).and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if part_coverage(&item.geometry.decode()?, &parts, &index) > MIN_PART_COVERAGE {
            item.tags.insert(fields::HIDE_3D.to_string(), Value::Bool(true));
        }
    }
    Ok(())
}

/// Returns the share of `outline`'s area that is covered by `parts`.
fn part_coverage(outline: &Geometry<f64>, parts: &[Polygon<f64>], index: &PartIndex) -> f64 {
    let Some(rect) = geo::BoundingRect::bounding_rect(outline) else { return 0.0 };
    let envelope = AABB::from_corners([rect.min().x, rect.min().y], [rect.max().x, rect.max().y]);
    let nearby: Vec<Geometry<f64>> =
        index.locate_in_envelope_intersecting(envelope).map(|p| Geometry::Polygon(parts[p.data].clone())).collect();
    if nearby.is_empty() {
        return 0.0;
    }
    // i_overlay-based boolean ops stand in for OverlayNGRobust: they do not throw on near-degenerate input
    let covered = polygon_overlay::union_all(&Geometry::GeometryCollection(nearby.into()))
        .and_then(|union| polygon_overlay::intersection(outline, &union));
    match covered {
        Ok(covered) => covered.unsigned_area() / outline.unsigned_area(),
        // Java: TopologyException -> 0
        Err(_) => 0.0,
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
    use planetiler_core::config::arguments::Arguments;
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

    fn hiding_config() -> PlanetilerConfig {
        PlanetilerConfig::from(&Arguments::of_pairs([("building_hide_outlines_with_parts", "true")])).unwrap()
    }

    fn tags_of(result: &[vector_tile::Feature]) -> Vec<Tags> {
        result.iter().map(|f| f.tags.clone()).collect()
    }

    #[test]
    fn test_part_is_marked_when_hiding_outlines_with_parts() {
        let t = LayerTest::with_config(hiding_config());
        let part = polygon_feature(tags! {"building:part" => "yes"});
        assert_features(14, &[tags! {"_part" => true}], &t.process(&part));
        let default = LayerTest::new();
        assert_features(14, &[tags! {"_part" => "<null>"}], &default.process(&part));
    }

    #[test]
    fn test_outline_mostly_covered_by_parts_hides_3d() {
        let t = LayerTest::with_config(hiding_config());
        let outline = vt_feature(LAYER_NAME, 1, &rectangle(10.0, 20.0).into(), tags! {"render_height" => 30}, 0);
        let part = vt_feature(
            LAYER_NAME,
            2,
            &rectangle_xy(10.0, 10.0, 20.0, 18.0).into(),
            tags! {"render_height" => 20, "_part" => true},
            0,
        );
        let neighbor =
            vt_feature(LAYER_NAME, 3, &rectangle_xy(20.0, 10.0, 30.0, 20.0).into(), tags! {"render_height" => 10}, 0);
        let result = t.post_process(LAYER_NAME, 14, vec![outline, part, neighbor]);
        assert_eq!(
            vec![
                tags! {"render_height" => 30, "hide_3d" => true},
                tags! {"render_height" => 20},
                tags! {"render_height" => 10}
            ],
            tags_of(&result)
        );
    }

    #[test]
    fn test_outline_with_small_part_stays_visible() {
        let t = LayerTest::with_config(hiding_config());
        let outline = vt_feature(LAYER_NAME, 1, &rectangle(10.0, 20.0).into(), tags! {"render_height" => 30}, 0);
        let part =
            vt_feature(LAYER_NAME, 2, &rectangle(12.0, 14.0).into(), tags! {"render_height" => 40, "_part" => true}, 0);
        let result = t.post_process(LAYER_NAME, 14, vec![outline, part]);
        assert_eq!(vec![tags! {"render_height" => 30}, tags! {"render_height" => 40}], tags_of(&result));
    }

    #[test]
    fn test_outline_covered_by_parts_where_default_intersection_throws() {
        // 90 Washington Street, New York (way 278033632) and its parts, as they are in tile 14/4823/6160
        let t = LayerTest::with_config(hiding_config());
        let wkt = |s: &str| planetiler_core::geo::wkt::read_wkt(s).unwrap();
        let outline = vt_feature(
            LAYER_NAME,
            1,
            &wkt("POLYGON ((131.6875 89.8125, 129.8125 94.1875, 124.625 91.75, 126.625 87.4375, 131.6875 89.8125))"),
            tags! {"render_height" => 30},
            0,
        );
        let mut items = vec![outline];
        for part in [
            "POLYGON ((125.25 92.0625, 126.875 88.5, 127.8125 88.875, 126.1875 92.5, 125.25 92.0625))",
            "POLYGON ((127.4375 89.75, 129.6875 90.8125, 129.25 91.75, 127 90.6875, 127.4375 89.75))",
            "POLYGON ((129.8125 94.1875, 128.9375 93.8125, 130.625 90.1875, 126.875 88.5, 125.25 92.0625, 124.625 91.75, 126.625 87.4375, 131.6875 89.8125, 129.8125 94.1875))",
            "POLYGON ((127.125 92.9375, 126.1875 92.5, 127.8125 88.875, 130.625 90.1875, 129.875 91.875, 129.4375 91.6875, 129.125 92.4375, 127.6875 91.8125, 127.125 92.9375))",
            "POLYGON ((129.875 91.875, 128.9375 93.8125, 127.125 92.9375, 127.6875 91.8125, 129.125 92.4375, 129.4375 91.6875, 129.875 91.875))",
            "POLYGON ((125.625 92.25, 125.4375 92.1875, 125.0625 92.4375, 124.5625 93.375, 124.25 94.1875, 123.5625 93.875, 124.25 91.625, 125.625 92.25))",
            "POLYGON ((128.1875 96.0625, 127.875 96.375, 127.6875 96.5, 127.1875 96.4375, 125.375 95.625, 124.25 94.9375, 124.1875 94.75, 124.25 94.1875, 123.5625 93.875, 124.25 91.625, 129.8125 94.1875, 129.125 96.4375, 128.1875 96.0625))",
            "POLYGON ((126.1875 92.5, 128.9375 93.8125, 129.0625 94, 129.125 94.375, 129 94.6875, 127.875 96.375, 127.5 96.5, 126.3125 96.125, 124.25 94.9375, 124.1875 94.75, 124.25 94.1875, 124.8125 92.875, 125.1875 92.3125, 125.625 92.25, 126.1875 92.5))",
            "POLYGON ((128.9375 93.8125, 129.8125 94.1875, 129.125 96.4375, 128.1875 96.0625, 129.125 94.375, 129.0625 94, 128.9375 93.8125))",
        ] {
            items.push(vt_feature(LAYER_NAME, 2, &wkt(part), tags! {"render_height" => 20, "_part" => true}, 0));
        }
        let result = t.post_process(LAYER_NAME, 14, items);
        let outline = result.iter().find(|f| f.id == 1).unwrap();
        assert_eq!(tags! {"render_height" => 30, "hide_3d" => true}, outline.tags);
    }

    #[test]
    fn test_merge_polygons_z13() {
        let t = LayerTest::new();
        let poly1 = vt_feature(LAYER_NAME, 1, &rectangle(10.0, 20.0).into(), Tags::default(), 0);
        let poly2 = vt_feature(LAYER_NAME, 1, &rectangle_xy(20.0, 10.0, 22.0, 20.0).into(), Tags::default(), 0);
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
