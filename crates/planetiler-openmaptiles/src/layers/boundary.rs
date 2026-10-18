//! Port of `layers/Boundary.java`: defines the logic for generating map elements for country, state, and town
//! boundaries in the `boundary` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles boundary sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/boundary).
//!
//! Uses natural earth at lower zoom levels and OpenStreetMap at higher zoom levels.
//!
//! For OpenStreetMap data at higher zoom levels:
//! 1. Preprocess relations on the first pass to extract info for relations where `type=boundary` and
//!    `boundary=administrative` and store the admin_level for later.
//! 2. When processing individual ways, take the minimum (most important) admin level of every relation they are a
//!    part of and use that as the admin level for the way.
//! 3. If `boundary_country_names` argument is true and the way is part of a country (admin_level=2) boundary, then
//!    hold onto it for later
//! 4. When we finish processing the OSM source, build country polygons from the saved ways and use that to determine
//!    which country is on the left and right side of each way, then emit the way with ADM0_L and ADM0_R keys set.
//! 5. Before emitting boundary lines, merge linestrings with the same tags.
//!
//! Differences from Java: JTS `Polygonizer` is approximated by merging each region's ways with JTS-style
//! `LineMerger` and turning every closed ring into a polygon before unioning them (JTS also fills holes because
//! it unions the hole faces back in). Rings that only close through a node shared by 3+ ways are dropped.

use std::collections::HashMap;
use std::sync::Arc;

use geo_types::{Coord, Geometry, LineString, MultiPolygon, Point, Polygon};
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::{self, FeatureCollector};
use planetiler_core::forwarding_profile::{FinishHandler, Handler, LayerPostProcessor, OsmRelationPreprocessor};
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::geo::polygon_overlay;
use planetiler_core::profile::FeatureConsumer;
use planetiler_core::reader::osm::osm_element::Relation;
use planetiler_core::reader::osm::osm_relation_info::OsmRelationInfo;
use planetiler_core::reader::simple_feature::SimpleFeature;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::reader::with_tags::WithTags;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::format;
use planetiler_core::util::loop_line_merger::line_merger::LineMerger;
use planetiler_core::util::parse;
use planetiler_core::util::translations::Translations;
use planetiler_core::value::{Tags, Value};
use planetiler_core::vector_tile;
use tracing::{info, warn};

use crate::feature_merge;
use crate::generated::open_map_tiles_schema::boundary::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::open_map_tiles_profile::{NaturalEarthProcessor, OSM_SOURCE, OsmAllProcessor};
use crate::util::omt_language_utils;

const COUNTRY_KE: &str = "Kenya";
const COUNTRY_SS: &str = "South Sudan";

fn country_test_offset() -> f64 {
    geo_utils::meters_to_pixel_at_equator(0, 10.0) / 256.0
}

/// Minimal set of information extracted from a boundary relation to be used when processing each way in that
/// relation.
#[derive(Debug, Clone)]
pub struct BoundaryRelation {
    pub id: i64,
    pub admin_level: i32,
    pub disputed: bool,
    pub name: Option<String>,
    pub claimed_by: Option<String>,
    pub iso3166alpha3: Option<String>,
}

impl OsmRelationInfo for BoundaryRelation {
    fn id(&self) -> i64 {
        self.id
    }

    fn estimate_memory_usage_bytes(&self) -> u64 {
        let s = |s: &Option<String>| s.as_ref().map_or(0, |s| 16 + s.len() as u64);
        16 + 8 + 4 + 1 + 3 * 8 + s(&self.name) + s(&self.claimed_by) + s(&self.iso3166alpha3)
    }
}

/// Grouping key for a way in a country boundary relation that needs left/right region IDs computed later.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CountryBoundaryKey {
    id: i64,
    admin_level: i32,
    disputed: bool,
    maritime: bool,
    minzoom: u32,
    /// sorted, deduplicated
    regions: Vec<i64>,
    claimed_by: Option<String>,
    name: Option<String>,
}

#[derive(Default)]
struct SharedState {
    region_geometries: HashMap<i64, Vec<Geometry<f64>>>,
    boundaries_to_merge: IndexMap<CountryBoundaryKey, Vec<Geometry<f64>>>,
}

pub struct Boundary {
    stats: StatsRef,
    add_country_names: bool,
    only_osm_boundaries: bool,
    min_feature_size_at_max_zoom: f64,
    config: Arc<PlanetilerConfig>,
    translations: Arc<Translations>,
    // may be updated concurrently by multiple threads
    region_names: RwLock<HashMap<i64, String>>,
    // need to synchronize updates to these shared data structures:
    state: Mutex<SharedState>,
}

fn is_disputed(tags: &Tags) -> bool {
    tags.get("disputed").is_some_and(parse::bool)
        || tags.get("dispute").is_some_and(parse::bool)
        || tags.get("border_status").and_then(Value::as_str) == Some("dispute")
        || tags.contains_key("disputed_by")
        || tags.contains_key("claimed_by")
}

fn edit_name(name: Option<&str>) -> Option<String> {
    name.map(|name| {
        name.replace(" at ", "").chars().filter(|c| !is_java_whitespace(*c)).collect::<String>().replace("Extentof", "")
    })
}

/// Java regex `\s`: ASCII whitespace only.
fn is_java_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

fn tag_str<'a>(tags: &'a Tags, key: &str) -> Option<std::borrow::Cow<'a, str>> {
    tags.get(key).filter(|v| !v.is_null()).map(Value::to_string_lossy)
}

impl Boundary {
    pub fn new(translations: &Arc<Translations>, config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Boundary {
        Boundary {
            add_country_names: config.arguments.get_boolean(
                "boundary_country_names",
                "boundary layer: add left/right codes of neighboring countries",
                true,
            ),
            only_osm_boundaries: config.arguments.get_boolean(
                "boundary_osm_only",
                "boundary layer: only use OSM, even at low zoom levels",
                false,
            ),
            min_feature_size_at_max_zoom: config.min_feature_size_at_max_zoom,
            config: config.clone(),
            stats: stats.clone(),
            translations: translations.clone(),
            region_names: RwLock::new(HashMap::new()),
            state: Mutex::new(SharedState::default()),
        }
    }

    /// Returns the left and right country for `line_string`.
    fn get_bordering_regions(
        &self,
        country_boundaries: &HashMap<i64, PreparedPolygon>,
        all_regions: &[i64],
        line_string: &LineString<f64>,
    ) -> (Option<i64>, Option<i64>) {
        let valid_regions: Vec<i64> =
            all_regions.iter().copied().filter(|r| country_boundaries.contains_key(r)).collect();
        if valid_regions.is_empty() {
            return (None, None);
        }
        let mut rights = Vec::new();
        let mut lefts = Vec::new();
        let steps = 10;
        let offset = country_test_offset();
        for i in 0..steps {
            let ratio = (i + 1) as f64 / (steps + 2) as f64;
            let right = geo_utils::point_along_offset(line_string, ratio, offset);
            let left = geo_utils::point_along_offset(line_string, ratio, -offset);
            for region_id in &valid_regions {
                if let Some(geom) = country_boundaries.get(region_id) {
                    if geom.contains(right) {
                        rights.push(*region_id);
                    } else if geom.contains(left) {
                        lefts.push(*region_id);
                    }
                }
            }
        }

        let right = mode(&rights);
        if let Some(right) = right {
            lefts.retain(|l| *l != right);
        }
        let left = mode(&lefts);

        if left.is_none() && right.is_none() {
            let point = geo_utils::point_along_offset(line_string, 0.5, 0.0);
            warn!(
                "no left or right country for border between OSM country relations: {:?} around {}",
                valid_regions,
                format::osm_debug_url(10, geo_utils::get_world_lon(point.x()), geo_utils::get_world_lat(point.y()))
            );
        }
        (left, right)
    }

    /// Returns a map from region ID to prepared geometry optimized for `contains` queries.
    fn prepare_region_polygons(
        &self,
        region_geometries: &HashMap<i64, Vec<Geometry<f64>>>,
    ) -> HashMap<i64, PreparedPolygon> {
        info!("Creating polygons for {} boundaries", region_geometries.len());
        let mut country_boundaries = HashMap::new();
        for (region_id, lines) in region_geometries {
            // TODO(port): JTS Polygonizer; approximated by merging lines into rings
            let mut merger = LineMerger::new();
            for line in lines {
                merger.add(line);
            }
            let polygons: Vec<Polygon<f64>> = merger
                .get_merged_line_strings()
                .into_iter()
                .filter(|l| l.0.len() >= 4 && l.is_closed())
                .map(|l| Polygon::new(l, vec![]))
                .collect();
            let combined = if polygons.is_empty() {
                None
            } else {
                match polygon_overlay::union_all(&MultiPolygon(polygons).into()) {
                    Ok(combined) => Some(combined),
                    Err(e) => {
                        warn!("Unable to build boundary polygon for OSM relation {region_id}: {e:?}");
                        continue;
                    }
                }
            };
            match combined.as_ref().and_then(PreparedPolygon::new) {
                Some(prepared) => {
                    country_boundaries.insert(*region_id, prepared);
                }
                None => warn!("Unable to form closed polygon for OSM relation {region_id} (likely missing edges)"),
            }
        }
        info!("Finished creating {} country polygons", country_boundaries.len());
        country_boundaries
    }
}

/// Returns most frequently-occurring element in `list`.
fn mode(list: &[i64]) -> Option<i64> {
    let mut counts: IndexMap<i64, usize> = IndexMap::new();
    for item in list {
        *counts.entry(*item).or_default() += 1;
    }
    let mut best: Option<(i64, usize)> = None;
    for (k, v) in counts {
        if best.is_none_or(|(_, c)| v > c) {
            best = Some((k, v));
        }
    }
    best.map(|(k, _)| k)
}

/// A polygon prepared for fast point-in-polygon tests (Java `PreparedGeometry.contains`): edges are bucketed into
/// horizontal bands and containment uses the even-odd crossing rule over the edges in the point's band.
struct PreparedPolygon {
    min_y: f64,
    max_y: f64,
    min_x: f64,
    max_x: f64,
    band_height: f64,
    edges: Vec<(Coord<f64>, Coord<f64>)>,
    bands: Vec<Vec<u32>>,
}

impl PreparedPolygon {
    fn new(geometry: &Geometry<f64>) -> Option<PreparedPolygon> {
        let mut edges = Vec::new();
        for polygon in polygon_overlay::polygons_of(geometry) {
            for ring in std::iter::once(polygon.exterior()).chain(polygon.interiors()) {
                for line in ring.lines() {
                    if line.start.y != line.end.y {
                        edges.push((line.start, line.end));
                    }
                }
            }
        }
        if edges.is_empty() {
            return None;
        }
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (a, b) in &edges {
            for c in [a, b] {
                min_x = min_x.min(c.x);
                max_x = max_x.max(c.x);
                min_y = min_y.min(c.y);
                max_y = max_y.max(c.y);
            }
        }
        let num_bands = ((edges.len() as f64).sqrt() as usize).clamp(1, 4096);
        let band_height = ((max_y - min_y) / num_bands as f64).max(f64::MIN_POSITIVE);
        let mut bands = vec![Vec::new(); num_bands];
        let band_of = |y: f64| (((y - min_y) / band_height) as usize).min(num_bands - 1);
        for (i, (a, b)) in edges.iter().enumerate() {
            let (lo, hi) = (band_of(a.y.min(b.y)), band_of(a.y.max(b.y)));
            for band in &mut bands[lo..=hi] {
                band.push(i as u32);
            }
        }
        Some(PreparedPolygon { min_y, max_y, min_x, max_x, band_height, edges, bands })
    }

    fn contains(&self, point: Point<f64>) -> bool {
        let (x, y) = (point.x(), point.y());
        if y < self.min_y || y > self.max_y || x < self.min_x || x > self.max_x {
            return false;
        }
        let band = (((y - self.min_y) / self.band_height) as usize).min(self.bands.len() - 1);
        let mut inside = false;
        for &i in &self.bands[band] {
            let (a, b) = self.edges[i as usize];
            if (a.y > y) != (b.y > y) {
                let cross_x = a.x + (y - a.y) * (b.x - a.x) / (b.y - a.y);
                if cross_x > x {
                    inside = !inside;
                }
            }
        }
        inside
    }
}

impl NaturalEarthProcessor for Boundary {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        if self.only_osm_boundaries {
            return;
        }
        let tags = feature.tags();
        let disputed = tag_str(tags, "featurecla").is_some_and(|f| f.starts_with("Disputed"));
        // (admin_level, minzoom, maxzoom)
        let info: Option<(i32, u32, u32)> = match table {
            "ne_110m_admin_0_boundary_lines_land" => Some((2, 0, 0)),
            "ne_50m_admin_0_boundary_lines_land" => Some((2, 1, 3)),
            "ne_10m_admin_0_boundary_lines_land" => {
                let mut is_disputed_south_sudan_and_kenya = false;
                if disputed {
                    let left = tag_str(tags, "adm0_left");
                    let right = tag_str(tags, "adm0_right");
                    if left.as_deref() == Some(COUNTRY_SS) {
                        is_disputed_south_sudan_and_kenya = right.as_deref() == Some(COUNTRY_KE);
                    } else if left.as_deref() == Some(COUNTRY_KE) {
                        is_disputed_south_sudan_and_kenya = right.as_deref() == Some(COUNTRY_SS);
                    }
                }
                if is_disputed_south_sudan_and_kenya {
                    Some((2, 1, 4))
                } else if feature.has_tag_str("featurecla", "Lease limit") {
                    None
                } else {
                    Some((2, 4, 4))
                }
            }
            "ne_10m_admin_1_states_provinces_lines" => {
                match feature.get_tag("min_zoom").and_then(parse::parse_double_or_null) {
                    Some(min_zoom) if min_zoom <= 7.0 => Some((4, 1, 4)),
                    Some(min_zoom) if min_zoom <= 7.7 => Some((4, 4, 4)),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some((admin_level, minzoom, maxzoom)) = info {
            features
                .line(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .set_zoom_range(minzoom, maxzoom)
                .set_min_pixel_size_at_all_zooms(0.0)
                .set_attr(fields::ADMIN_LEVEL, admin_level)
                .set_attr(fields::MARITIME, 0)
                .set_attr(fields::DISPUTED, if disputed { 1 } else { 0 });
        }
    }
}

impl OsmRelationPreprocessor for Boundary {
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        if relation.has_tag_str("type", "boundary")
            && relation.has_tag("admin_level")
            && relation.has_tag_str("boundary", "administrative")
        {
            let admin_level = relation.get_tag("admin_level").and_then(parse::parse_round_int);
            let code = tag_str(&relation.tags, "ISO3166-1:alpha3").map(|s| s.into_owned());
            if let Some(admin_level) = admin_level.filter(|l| (2..=10).contains(l)) {
                let disputed = is_disputed(&relation.tags);
                if let Some(code) = &code {
                    self.region_names.write().insert(relation.id, code.clone());
                }
                return Some(vec![Arc::new(BoundaryRelation {
                    id: relation.id,
                    admin_level,
                    disputed,
                    name: tag_str(&relation.tags, "name").map(|s| s.into_owned()),
                    claimed_by: if disputed {
                        tag_str(&relation.tags, "claimed_by").map(|s| s.into_owned())
                    } else {
                        None
                    },
                    iso3166alpha3: code,
                })]);
            }
        }
        None
    }
}

impl OsmAllProcessor for Boundary {
    fn process_all_osm(&self, feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        // fast path: most elements are not in any relation
        let relation_infos = feature.relation_infos();
        if relation_infos.is_empty() || !feature.can_be_line() {
            return;
        }
        let boundary_relations = || {
            relation_infos
                .iter()
                .filter(|r| !r.is_super_relation())
                .filter_map(|r| r.relation.downcast_ref::<BoundaryRelation>())
        };
        if boundary_relations().next().is_none() {
            return;
        }
        let mut min_admin_level = i32::MAX;
        let mut disputed_name: Option<String> = None;
        let mut claimed_by: Option<String> = None;
        let mut region_ids: Vec<i64> = Vec::new();
        let mut disputed = false;
        {
            let region_names = self.region_names.read();
            // aggregate all borders this way is a part of - take the lowest
            // admin level, and assume it is disputed if any relation is disputed.
            for rel in boundary_relations() {
                disputed |= rel.disputed;
                if rel.admin_level < min_admin_level {
                    min_admin_level = rel.admin_level;
                }
                if rel.disputed {
                    if disputed_name.is_none() {
                        disputed_name.clone_from(&rel.name);
                    }
                    if claimed_by.is_none() {
                        claimed_by.clone_from(&rel.claimed_by);
                    }
                }
                if min_admin_level == 2 && region_names.contains_key(&rel.id) && !region_ids.contains(&rel.id) {
                    region_ids.push(rel.id);
                }
            }
        }

        if min_admin_level <= 10 {
            let tags = feature.tags();
            let way_is_disputed = is_disputed(tags);
            disputed |= way_is_disputed;
            if way_is_disputed {
                if disputed_name.is_none() {
                    disputed_name = tag_str(tags, "name").map(|s| s.into_owned());
                }
                if claimed_by.is_none() {
                    claimed_by = tag_str(tags, "claimed_by").map(|s| s.into_owned());
                }
            }
            let maritime = feature.get_boolean("maritime")
                || feature.has_tag_str("natural", "coastline")
                || feature.has_tag_str("boundary_type", "maritime");
            let mut minzoom = if maritime && min_admin_level == 2 {
                4
            } else if min_admin_level <= 4 {
                5
            } else if min_admin_level <= 6 {
                9
            } else if min_admin_level <= 8 {
                11
            } else {
                12
            };
            if self.only_osm_boundaries && min_admin_level <= 4 {
                minzoom = if min_admin_level == 2 { if maritime { 4 } else { 0 } } else { 1 };
            }
            if self.add_country_names && !region_ids.is_empty() {
                // save for later
                match feature.line() {
                    Ok(line) => {
                        region_ids.sort_unstable();
                        let key = CountryBoundaryKey {
                            id: feature.id(),
                            admin_level: min_admin_level,
                            disputed,
                            maritime,
                            minzoom,
                            regions: region_ids,
                            claimed_by,
                            name: disputed_name,
                        };
                        let line = line.clone();
                        // multiple threads may update this concurrently
                        let mut state = self.state.lock();
                        for rel in boundary_relations() {
                            if rel.admin_level <= 2 {
                                state.region_geometries.entry(rel.id).or_default().push(line.clone());
                            }
                        }
                        state.boundaries_to_merge.entry(key).or_default().push(line);
                    }
                    Err(_) => warn!("Cannot extract boundary line from {}", feature.id()),
                }
            } else {
                features
                    .line(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .set_attr(fields::ADMIN_LEVEL, min_admin_level)
                    .set_attr(fields::DISPUTED, if disputed { 1 } else { 0 })
                    .set_attr(fields::MARITIME, if maritime { 1 } else { 0 })
                    .set_min_pixel_size_at_all_zooms(0.0)
                    .set_min_zoom(minzoom)
                    .set_attr(fields::CLAIMED_BY, claimed_by)
                    .set_attr(fields::DISPUTED_NAME, edit_name(disputed_name.as_deref()));
            }
        }
    }
}

impl OsmBoundaryPolygonHandler for Boundary {
    fn process_osm_boundary_polygon(&self, element: &OsmBoundaryPolygon<'_>, features: &mut FeatureCollector<'_>) {
        features
            .polygon(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .put_attrs(omt_language_utils::get_names(element.source.tags(), Some(&self.translations)))
            .set_attr(fields::CLASS, element.boundary().as_deref())
            .set_min_pixel_size_below_zoom(13, 4.0) // for Z4: `sql_filter: area>power(ZRES3,2)`, etc.
            .set_min_zoom(4);
    }
}

impl FinishHandler for Boundary {
    fn finish(
        &self,
        source_name: &str,
        feature_collectors: &feature_collector::Factory,
        emit: &mut FeatureConsumer<'_>,
    ) {
        if source_name != OSM_SOURCE {
            return;
        }
        let timer = self.stats.start_stage("boundaries");
        let (region_geometries, boundaries_to_merge) = {
            let mut state = self.state.lock();
            (std::mem::take(&mut state.region_geometries), std::mem::take(&mut state.boundaries_to_merge))
        };
        let country_boundaries = self.prepare_region_polygons(&region_geometries);
        drop(region_geometries);
        let region_names = self.region_names.read();

        for (key, lines) in boundaries_to_merge {
            let mut merger = LineMerger::new();
            for geom in &lines {
                merger.add(geom);
            }
            drop(lines);
            for line_string in merger.get_merged_line_strings() {
                let (left, right) = self.get_bordering_regions(&country_boundaries, &key.regions, &line_string);
                let source = SimpleFeature::from_world_geometry_with_id(line_string, key.id);
                let mut features = feature_collectors.get(&source);
                let new_feature = features
                    .line(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .set_attr(fields::ADMIN_LEVEL, key.admin_level)
                    .set_attr(fields::DISPUTED, if key.disputed { 1 } else { 0 })
                    .set_attr(fields::MARITIME, if key.maritime { 1 } else { 0 })
                    .set_attr(fields::CLAIMED_BY, key.claimed_by.as_deref())
                    .set_attr(fields::DISPUTED_NAME, if key.disputed { edit_name(key.name.as_deref()) } else { None })
                    .set_min_pixel_size_at_all_zooms(0.0)
                    .set_min_zoom(key.minzoom);
                if key.admin_level == 2 && !key.disputed {
                    // only non-disputed admin 2 boundaries get to have adm0_{l,r}, at zoom 5 and more
                    new_feature
                        .set_attr_with_minzoom(
                            fields::ADM0_L,
                            left.and_then(|l| region_names.get(&l)).map(String::as_str),
                            5,
                        )
                        .set_attr_with_minzoom(
                            fields::ADM0_R,
                            right.and_then(|r| region_names.get(&r)).map(String::as_str),
                            5,
                        );
                }
                for feature in features.iter() {
                    emit(feature);
                }
            }
        }
        timer.stop();
    }
}

impl LayerPostProcessor for Boundary {
    fn post_process(&self, zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        // only omit a segment if it is shorter than a pixel
        let min_length = self.min_feature_size_at_max_zoom;
        let tolerance = self.config.tolerance(zoom);
        *items =
            feature_merge::merge_line_strings_dynamic(std::mem::take(items), |_| min_length, tolerance, BUFFER_SIZE);
        Ok(())
    }
}

impl Handler for Boundary {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn release(&self) {
        let mut state = self.state.lock();
        state.region_geometries.clear();
        state.boundaries_to_merge.clear();
        self.region_names.write().clear();
    }

    fn as_osm_relation_preprocessor(&self) -> Option<&dyn OsmRelationPreprocessor> {
        Some(self)
    }

    fn as_finish_handler(&self) -> Option<&dyn FinishHandler> {
        Some(self)
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Boundary {
    fn as_osm_boundary_polygon_handler(&self) -> Option<&dyn OsmBoundaryPolygonHandler> {
        Some(self)
    }
}

impl Layer for Boundary {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }

    fn as_osm_all_processor(&self) -> Option<&dyn OsmAllProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use geo::BoundingRect;
    use planetiler_core::config::arguments::Arguments;
    use planetiler_core::geo::test_utils::{new_line_string, rectangle};
    use planetiler_core::profile::Profile;
    use planetiler_core::reader::osm::osm_relation_info::RelationMember;
    use planetiler_core::tags;

    use super::*;
    use crate::open_map_tiles_profile::NATURAL_EARTH_SOURCE;
    use crate::test_support::*;

    fn ne_line(tags: Tags, layer: &str, id: i64) -> SimpleFeature {
        SimpleFeature::create(new_line_string(&[0.0, 0.0, 1.0, 1.0]), tags, Some(NATURAL_EARTH_SOURCE), Some(layer), id)
    }

    fn relation(id: i64, tags: &[(&str, &str)]) -> Relation {
        let mut relation = Relation::new(id);
        for (k, v) in tags {
            relation.tags.insert(k.to_string(), (*v).into());
        }
        relation
    }

    fn infos(t: &LayerTest, relation: &Relation) -> Vec<Arc<dyn OsmRelationInfo>> {
        t.profile.preprocess_osm_relation(relation).unwrap_or_default()
    }

    fn members(infos: Vec<Arc<dyn OsmRelationInfo>>) -> Vec<RelationMember<dyn OsmRelationInfo>> {
        infos.into_iter().map(|r| RelationMember::new("", r)).collect()
    }

    fn fake_osm(
        geometry: impl Into<Geometry<f64>>,
        tags: Tags,
        id: i64,
        infos: Vec<Arc<dyn OsmRelationInfo>>,
    ) -> SimpleFeature {
        SimpleFeature::create_fake_osm_feature(geometry, tags, Some(OSM_SOURCE), None::<&str>, id, members(infos), None)
    }

    /// A feature emitted from `finish`, captured at the zooms tests look at.
    struct Finished {
        z0: Tags,
        z4: Tags,
        z5: Tags,
        geometry: Geometry<f64>,
    }

    fn finish(t: &LayerTest) -> Vec<Finished> {
        let mut result = Vec::new();
        t.profile.finish(OSM_SOURCE, &t.feature_collector_factory, &mut |f| {
            result.push(Finished {
                z0: to_map(f, 0),
                z4: to_map(f, 4),
                z5: to_map(f, 5),
                geometry: f.get_geometry().clone(),
            })
        });
        result
    }

    fn first_coord(g: &Geometry<f64>) -> Coord<f64> {
        match g {
            Geometry::LineString(l) => l.0[0],
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Asserts each expected map is a submap of some actual map (1:1, any order).
    fn assert_submaps_any_order(expected: &[Tags], actual: &[&Tags]) {
        assert_eq!(expected.len(), actual.len(), "{actual:?}");
        let matches = |e: &Tags, a: &Tags| {
            e.iter().all(|(k, v)| if v.as_str() == Some("<null>") { !a.contains_key(k) } else { a.get(k) == Some(v) })
        };
        let mut used = vec![false; actual.len()];
        for e in expected {
            let idx = (0..actual.len()).find(|i| !used[*i] && matches(e, actual[*i]));
            assert!(idx.is_some(), "no match for {e:?} in {actual:?}");
            used[idx.unwrap()] = true;
        }
    }

    #[test]
    fn test_natural_earth_country_boundaries() {
        let t = LayerTest::new();
        assert_covers_zoom_range(
            0,
            4,
            "boundary",
            &[
                &t.process(&ne_line(Tags::new(), "ne_110m_admin_0_boundary_lines_land", 0)),
                &t.process(&ne_line(Tags::new(), "ne_50m_admin_0_boundary_lines_land", 1)),
                &t.process(&ne_line(Tags::new(), "ne_10m_admin_0_boundary_lines_land", 2)),
            ],
        );

        assert_features(
            0,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed" => 0, "maritime" => 0, "admin_level" => 2,
                "_minzoom" => 0, "_buffer" => 4.0,
            }],
            &t.process(&ne_line(
                tags! {"featurecla" => "International boundary (verify)"},
                "ne_110m_admin_0_boundary_lines_land",
                0,
            )),
        );
        assert_features(
            0,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed" => 1, "maritime" => 0, "admin_level" => 2,
                "_buffer" => 4.0,
            }],
            &t.process(&ne_line(
                tags! {"featurecla" => "Disputed (please verify)"},
                "ne_110m_admin_0_boundary_lines_land",
                0,
            )),
        );
        assert_features(
            0,
            &[tags! {"_layer" => "boundary", "_type" => "line", "admin_level" => 2}],
            &t.process(&ne_line(
                tags! {"featurecla" => "International boundary (verify)"},
                "ne_50m_admin_0_boundary_lines_land",
                0,
            )),
        );
        assert_features(
            0,
            &[tags! {"_layer" => "boundary", "_type" => "line", "_minzoom" => 4, "admin_level" => 2}],
            &t.process(&ne_line(
                tags! {"featurecla" => "International boundary (verify)"},
                "ne_10m_admin_0_boundary_lines_land",
                0,
            )),
        );
        assert_features(
            0,
            &[tags! {"_layer" => "boundary", "_type" => "line", "_minzoom" => 1, "admin_level" => 2}],
            &t.process(&ne_line(
                tags! {"featurecla" => "Disputed (please verify)", "adm0_left" => "South Sudan", "adm0_right" => "Kenya"},
                "ne_10m_admin_0_boundary_lines_land",
                0,
            )),
        );
        assert_features(
            0,
            &[],
            &t.process(&ne_line(tags! {"featurecla" => "Lease limit"}, "ne_10m_admin_0_boundary_lines_land", 0)),
        );
    }

    #[test]
    fn test_natural_earth_country_ke_ss_boundary_reversed() {
        let t = LayerTest::new();
        assert_features(
            0,
            &[tags! {"_layer" => "boundary", "_minzoom" => 1, "admin_level" => 2}],
            &t.process(&ne_line(
                tags! {"featurecla" => "Disputed (please verify)", "adm0_right" => "South Sudan", "adm0_left" => "Kenya"},
                "ne_10m_admin_0_boundary_lines_land",
                0,
            )),
        );
    }

    #[test]
    fn test_natural_earth_country_not_ke_ss_boundary() {
        let t = LayerTest::new();
        assert_features(
            0,
            &[tags! {"_layer" => "boundary", "_minzoom" => 4, "admin_level" => 2}],
            &t.process(&ne_line(
                tags! {"featurecla" => "Disputed (please verify)", "adm0_left" => "South Sudan", "adm0_right" => "Uganda"},
                "ne_10m_admin_0_boundary_lines_land",
                0,
            )),
        );
    }

    #[test]
    fn test_natural_earth_state_boundaries() {
        let t = LayerTest::new();
        let layer = "ne_10m_admin_1_states_provinces_lines";
        assert_features(
            0,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed" => 0, "maritime" => 0, "admin_level" => 4,
                "_minzoom" => 1, "_maxzoom" => 4, "_buffer" => 4.0,
            }],
            &t.process(&ne_line(tags! {"min_zoom" => 7.0}, layer, 0)),
        );
        assert_features(
            0,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed" => 0, "maritime" => 0, "admin_level" => 4,
                "_minzoom" => 4, "_maxzoom" => 4, "_buffer" => 4.0,
            }],
            &t.process(&ne_line(tags! {"min_zoom" => 7.6}, layer, 0)),
        );
        assert_features(0, &[], &t.process(&ne_line(tags! {"min_zoom" => 7.9}, layer, 0)));
        assert_features(0, &[], &t.process(&ne_line(Tags::new(), layer, 0)));
    }

    #[test]
    fn test_merges_disconnected_line_features() {
        let t = LayerTest::new();
        t.test_merges_linestrings(&tags! {"admin_level" => 2}, LAYER_NAME, 10.0, 13);
        t.test_merges_linestrings(&tags! {"admin_level" => 2}, LAYER_NAME, 10.0, 14);
    }

    #[test]
    fn test_osm_town_boundary() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "10"), ("boundary", "administrative")]);
        assert_features(
            14,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed" => 0, "maritime" => 0, "admin_level" => 10,
                "_minzoom" => 12, "_maxzoom" => 14, "_buffer" => 4.0, "_minpixelsize" => 0.0,
            }],
            &t.process(&line_feature_with_relation(infos(&t, &rel), Tags::new())),
        );
    }

    #[test]
    fn test_osm_boundary_level_two_and_a_half() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "2.5"), ("boundary", "administrative")]);
        assert_features(
            14,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed" => 0, "maritime" => 0, "admin_level" => 3,
                "_minzoom" => 5, "_maxzoom" => 14, "_buffer" => 4.0, "_minpixelsize" => 0.0,
            }],
            &t.process(&line_feature_with_relation(infos(&t, &rel), Tags::new())),
        );
    }

    #[test]
    fn test_osm_boundary_takes_min_admin_level() {
        let t = LayerTest::new();
        let rel1 = relation(
            1,
            &[("type", "boundary"), ("admin_level", "10"), ("name", "Town"), ("boundary", "administrative")],
        );
        let rel2 = relation(
            2,
            &[("type", "boundary"), ("admin_level", "4"), ("name", "State"), ("boundary", "administrative")],
        );
        let mut all = infos(&t, &rel2);
        all.extend(infos(&t, &rel1));
        assert_features(
            14,
            &[tags! {"_layer" => "boundary", "_type" => "line", "disputed" => 0, "maritime" => 0, "admin_level" => 4}],
            &t.process(&line_feature_with_relation(all, Tags::new())),
        );
    }

    #[test]
    fn test_osm_boundary_sets_maritime_from_way() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "10"), ("boundary", "administrative")]);
        for way_tags in
            [tags! {"maritime" => "yes"}, tags! {"natural" => "coastline"}, tags! {"boundary_type" => "maritime"}]
        {
            assert_features(
                14,
                &[tags! {"maritime" => 1}],
                &t.process(&line_feature_with_relation(infos(&t, &rel), way_tags)),
            );
        }
    }

    #[test]
    fn test_ignores_protected_areas() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "10"), ("boundary", "protected_area")]);
        assert!(t.profile.preprocess_osm_relation(&rel).is_none());
    }

    #[test]
    fn test_ignores_protected_admin_level_over_10() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "11"), ("boundary", "administrative")]);
        assert!(t.profile.preprocess_osm_relation(&rel).is_none());
    }

    #[test]
    fn test_osm_boundary_disputed() {
        let t = LayerTest::new();
        let rel = relation(
            1,
            &[
                ("type", "boundary"),
                ("admin_level", "5"),
                ("boundary", "administrative"),
                ("disputed", "yes"),
                ("name", "Border A - B"),
                ("claimed_by", "A"),
            ],
        );
        assert_features(
            14,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed_name" => "BorderA-B", "claimed_by" => "A",
                "disputed" => 1, "maritime" => 0, "admin_level" => 5,
            }],
            &t.process(&line_feature_with_relation(infos(&t, &rel), Tags::new())),
        );
    }

    #[test]
    fn test_osm_boundary_disputed_from_way() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "5"), ("boundary", "administrative")]);
        assert_features(
            14,
            &[tags! {"_layer" => "boundary", "_type" => "line", "disputed" => 1, "maritime" => 0, "admin_level" => 5}],
            &t.process(&line_feature_with_relation(infos(&t, &rel), tags! {"disputed" => "yes"})),
        );
        assert_features(
            14,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "disputed" => 1, "maritime" => 0, "admin_level" => 5,
                "claimed_by" => "A", "disputed_name" => "AB",
            }],
            &t.process(&line_feature_with_relation(
                infos(&t, &rel),
                tags! {"disputed" => "yes", "claimed_by" => "A", "name" => "AB"},
            )),
        );
    }

    #[test]
    fn test_osm_al2_boundary_disputed_min_zoom() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "2"), ("boundary", "administrative")]);
        assert_features(
            3,
            &[tags! {
                "_layer" => "boundary", "_type" => "line", "_minzoom" => 5, "disputed" => 1, "maritime" => 0,
                "admin_level" => 2,
            }],
            &t.process(&line_feature_with_relation(infos(&t, &rel), tags! {"disputed" => "yes"})),
        );
    }

    #[test]
    fn test_country_boundary_emitted_if_no_name() {
        let t = LayerTest::new();
        let rel = relation(1, &[("type", "boundary"), ("admin_level", "2"), ("boundary", "administrative")]);
        assert_features(
            14,
            &[tags! {"_layer" => "boundary", "_type" => "line", "disputed" => 0, "maritime" => 0, "admin_level" => 2}],
            &t.process(&line_feature_with_relation(infos(&t, &rel), Tags::new())),
        );
    }

    fn country(id: i64, code: &str) -> Relation {
        relation(
            id,
            &[("type", "boundary"), ("admin_level", "2"), ("boundary", "administrative"), ("ISO3166-1:alpha3", code)],
        )
    }

    fn setup_country_left_right_name_test(t: &LayerTest, tags: Tags) -> Vec<Finished> {
        let country1 = country(1, "C1");
        let country2 = country(2, "C2");
        let mut both = infos(t, &country1);
        both.extend(infos(t, &country2));

        // shared edge
        assert_features(14, &[], &t.process(&fake_osm(new_line_string(&[0.0, 0.0, 0.0, 10.0]), tags.clone(), 3, both)));
        // other 2 edges of country 1, then country 2
        for (coords, c) in [
            ([0.0, 0.0, 5.0, 10.0], &country1),
            ([0.0, 10.0, 5.0, 10.0], &country1),
            ([0.0, 0.0, -5.0, 10.0], &country2),
            ([0.0, 10.0, -5.0, 10.0], &country2),
        ] {
            assert_features(14, &[], &t.process(&fake_osm(new_line_string(&coords), tags.clone(), 4, infos(t, c))));
        }
        finish(t)
    }

    fn get<'a>(map: &'a Tags, key: &str) -> Option<&'a str> {
        map.get(key).and_then(Value::as_str)
    }

    #[test]
    fn test_country_left_right_name() {
        let t = LayerTest::new();
        let features = setup_country_left_right_name_test(&t, Tags::new());
        assert_eq!(3, features.len());

        // ensure shared edge has country labels on right sides, from z5
        let shared_edge = features.iter().find(|c| c.z5.contains_key("adm0_l") && c.z5.contains_key("adm0_r")).unwrap();
        if first_coord(&shared_edge.geometry).y == 0.5 {
            // going up
            assert_eq!(Some("C1"), get(&shared_edge.z5, "adm0_r"));
            assert_eq!(Some("C2"), get(&shared_edge.z5, "adm0_l"));
        } else {
            // going down
            assert_eq!(Some("C2"), get(&shared_edge.z5, "adm0_r"));
            assert_eq!(Some("C1"), get(&shared_edge.z5, "adm0_l"));
        }
        let c1 = features.iter().find(|c| c.geometry.bounding_rect().unwrap().max().x > 0.5).unwrap();
        if first_coord(&c1.geometry).y == 0.5 {
            assert_eq!(Some("C1"), get(&c1.z5, "adm0_l"));
        } else {
            assert_eq!(Some("C1"), get(&c1.z5, "adm0_r"));
        }
        let c2 = features.iter().find(|c| c.geometry.bounding_rect().unwrap().min().x < 0.5).unwrap();
        if first_coord(&c2.geometry).y == 0.5 {
            assert_eq!(Some("C2"), get(&c2.z5, "adm0_r"));
        } else {
            assert_eq!(Some("C2"), get(&c2.z5, "adm0_l"));
        }

        // but not at z4, see https://github.com/openmaptiles/planetiler-openmaptiles/issues/18
        assert_eq!(None, shared_edge.z4.get("adm0_r"));
        assert_eq!(None, shared_edge.z4.get("adm0_l"));
    }

    #[test]
    fn test_country_left_right_name_disputed() {
        let t = LayerTest::new();
        let features = setup_country_left_right_name_test(&t, tags! {"disputed" => 1});
        assert_eq!(3, features.len());
        // ensure shared edge does not have country labels at z5 and not even on z4
        for feature in &features {
            for map in [&feature.z5, &feature.z4] {
                assert_eq!(None, map.get("adm0_r"));
                assert_eq!(None, map.get("adm0_l"));
            }
        }
    }

    #[test]
    fn test_country_boundary_not_closed() {
        let t = LayerTest::new();
        let country1 = country(1, "C1");
        let line = new_line_string(&[0.0, 0.0, 0.0, 10.0, 5.0, 5.0]);
        assert_features(14, &[], &t.process(&fake_osm(line, Tags::new(), 3, infos(&t, &country1))));
        let features = finish(&t);
        assert_submaps_any_order(
            &[tags! {
                "adm0_r" => "<null>", "adm0_l" => "<null>", "maritime" => 0, "disputed" => 0, "admin_level" => 2,
                "_layer" => "boundary",
            }],
            &features.iter().map(|f| &f.z0).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_nested_country() {
        let t = LayerTest::new();
        let country1 = country(1, "C1");
        for (min, max) in [(0.0, 10.0), (1.0, 9.0)] {
            let ring = geo_utils::polygon_to_line_string(&rectangle(min, max).into()).unwrap();
            assert_features(14, &[], &t.process(&fake_osm(ring, Tags::new(), 3, infos(&t, &country1))));
        }
        let features = finish(&t);
        assert_submaps_any_order(
            &[tags! {"adm0_l" => "C1", "adm0_r" => "<null>"}, tags! {"adm0_r" => "C1", "adm0_l" => "<null>"}],
            &features.iter().map(|f| &f.z5).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_dont_label_bad_polygon() {
        let t = LayerTest::new();
        let country1 = country(1, "C1");
        let line = geo_utils::world_to_lat_lon_coords(
            &new_line_string(&[0.0, 0.0, 0.1, 0.0, 0.1, 0.1, 0.02, 0.1, 0.02, -0.02]).into(),
        );
        assert_features(14, &[], &t.process(&fake_osm(line, Tags::new(), 3, infos(&t, &country1))));
        let features = finish(&t);
        assert_submaps_any_order(
            &[tags! {"adm0_l" => "<null>", "adm0_r" => "<null>"}],
            &features.iter().map(|f| &f.z0).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_ignore_bad_polygon_and_label_good_part() {
        let t = LayerTest::new();
        let country1 = country(1, "C1");
        let bad = geo_utils::world_to_lat_lon_coords(
            &new_line_string(&[0.0, 0.0, 0.1, 0.0, 0.1, 0.1, 0.2, 0.1, 0.2, -0.2]).into(),
        );
        assert_features(14, &[], &t.process(&fake_osm(bad, Tags::new(), 3, infos(&t, &country1))));
        let good = geo_utils::world_to_lat_lon_coords(
            &geo_utils::polygon_to_line_string(&rectangle(0.2, 0.3).into()).unwrap(),
        );
        assert_features(14, &[], &t.process(&fake_osm(good, Tags::new(), 3, infos(&t, &country1))));
        let features = finish(&t);
        assert_submaps_any_order(
            &[tags! {"adm0_l" => "<null>", "adm0_r" => "<null>"}, tags! {"adm0_l" => "<null>", "adm0_r" => "C1"}],
            &features.iter().map(|f| &f.z5).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_osm_boundaries_only() {
        let t = LayerTest::new();
        let config = PlanetilerConfig::from(&Arguments::from_args(&["--boundary-osm-only=true"])).expect("config");
        let osm_only = LayerTest::with_config(config);
        assert_features(
            0,
            &[],
            &osm_only.process(&ne_line(
                tags! {"featurecla" => "International boundary (verify)"},
                "ne_110m_admin_0_boundary_lines_land",
                0,
            )),
        );
        assert_features(
            0,
            &[],
            &osm_only.process(&ne_line(tags! {"min_zoom" => 7.0}, "ne_10m_admin_1_states_provinces_lines", 0)),
        );

        let mut rel = relation(1, &[("type", "boundary"), ("admin_level", "2"), ("boundary", "administrative")]);
        assert_features(
            14,
            &[tags! {"_minzoom" => 0}],
            &osm_only.process(&line_feature_with_relation(infos(&t, &rel), Tags::new())),
        );
        assert_features(
            14,
            &[tags! {"_minzoom" => 4}],
            &osm_only.process(&line_feature_with_relation(infos(&t, &rel), tags! {"maritime" => 1})),
        );
        rel.tags.insert("admin_level".into(), "4".into());
        assert_features(
            14,
            &[tags! {"_minzoom" => 1}],
            &osm_only.process(&line_feature_with_relation(infos(&t, &rel), Tags::new())),
        );
    }

    #[test]
    fn test_indigenous_land() {
        let t = LayerTest::new();
        assert_features(
            0,
            &[
                tags! {
                    "_layer" => "boundary", "class" => "aboriginal_lands", "name" => "Seminole Nation",
                    "name_en" => "Seminole Nation", "name:latin" => "Seminole Nation", "_type" => "polygon",
                    "_minzoom" => 4,
                },
                tags! {"_layer" => "park", "_type" => "polygon"},
                tags! {"_layer" => "park", "_type" => "point"},
                tags! {"_layer" => "place"},
            ],
            &t.process(&polygon_feature_with_area(
                1.0,
                tags! {
                    "type" => "boundary", "boundary" => "aboriginal_lands", "name" => "Seminole Nation",
                    "name:en" => "Seminole Nation",
                },
            )),
        );
    }
}
