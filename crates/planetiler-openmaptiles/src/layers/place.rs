/*
Copyright (c) 2021, MapTiler.com & OpenMapTiles contributors.
All rights reserved.

Code license: BSD 3-Clause License

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.

* Neither the name of the copyright holder nor the names of its
  contributors may be used to endorse or promote products derived from
  this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Design license: CC-BY 4.0

See https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md for details on usage
*/
//! Port of `layers/Place.java`: defines the logic for generating label points for populated places like continents,
//! countries, cities, and towns in the `place` layer from source features.
//!
//! This is ported from the
//! [OpenMapTiles place sql files](https://github.com/openmaptiles/openmaptiles/tree/master/layers/place).
//!
//! Place labels locations and names come from OpenStreetMap, but we also join with natural earth state/country
//! geographic areas and city point labels to give a hint for what rank and minimum zoom level to use for those
//! points.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use geo::Centroid;
use geo_types::Point;
use icu_normalizer::DecomposingNormalizerBorrowed;
use parking_lot::RwLock;
use planetiler_core::collection::feature_group::SORT_KEY_BITS;
use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::feature_collector::FeatureCollector;
use planetiler_core::forwarding_profile::{Handler, LayerPostProcessor};
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::geometry_exception::{GeometryException, GeometryResult};
use planetiler_core::geo::point_index::PointIndex;
use planetiler_core::geo::polygon_index::PolygonIndex;
use planetiler_core::reader::source_feature::SourceFeature;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::parse;
use planetiler_core::util::sort_key::SortKey;
use planetiler_core::util::translations::Translations;
use planetiler_core::util::zoom_function::ZoomFunction;
use planetiler_core::value::Value;
use planetiler_core::vector_tile;

use crate::generated::open_map_tiles_schema::boundary;
use crate::generated::open_map_tiles_schema::place::*;
use crate::generated::tables::*;
use crate::layer::Layer;
use crate::open_map_tiles_profile::NaturalEarthProcessor;
use crate::util::omt_language_utils;
use crate::util::utils::{null_if_empty, null_or_empty, null_or_empty_value};

/// Returns the portion of the world that `square_meters` covers where 1 is the entire planet.
fn square_meters_to_world_area(square_meters: f64) -> f64 {
    let one_side_meters = square_meters.sqrt();
    let one_side_world = geo_utils::meters_to_pixel_at_equator(0, one_side_meters) / 256.0;
    one_side_world.powi(2)
}

/// `(max world area, rank)` pairs, ascending by area (Java `AREA_RANKS.ceilingEntry`).
static AREA_RANKS: LazyLock<[(f64, i32); 6]> = LazyLock::new(|| {
    [
        (square_meters_to_world_area(1_000_000.0), 6),
        (square_meters_to_world_area(15_000_000.0), 5),
        (square_meters_to_world_area(40_000_000.0), 4),
        (square_meters_to_world_area(160_000_000.0), 3),
        (square_meters_to_world_area(640_000_000.0), 2),
        (f64::MAX, 1),
    ]
});

fn area_rank(area: f64) -> i32 {
    AREA_RANKS.iter().find(|(max, _)| *max >= area).map_or(1, |(_, rank)| *rank)
}

/// 2^14 tiles, 2^12 pixels per tile
const MIN_ISLAND_WORLD_AREA: f64 = 1.0 / (1u64 << 52) as f64;
// constants for packing place label precedence into the sort-key field
const MAX_CITY_POPULATION: f64 = 100_000_000.0;
const MAJOR_CITY_PLACES: &[&str] = &["city", "town", "village"];

fn city_join_distance() -> f64 {
    geo_utils::meters_to_pixel_at_equator(0, 50_000.0) / 256.0
}

static LABEL_GRID_LIMITS: LazyLock<ZoomFunction<i32>> =
    LazyLock::new(|| ZoomFunction::from_max_zoom_thresholds_or([(8, 4), (9, 8), (10, 12), (12, 14)], Some(0)));

/// Ordering defines the precedence of place classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaceType {
    City,
    Town,
    Village,
    Hamlet,
    Suburb,
    Quarter,
    Neighborhood,
    IsolatedDwelling,
    Unknown,
}

impl PlaceType {
    pub fn for_name(name: Option<&str>) -> PlaceType {
        match name {
            Some("city") => PlaceType::City,
            Some("town") => PlaceType::Town,
            Some("village") => PlaceType::Village,
            Some("hamlet") => PlaceType::Hamlet,
            Some("suburb") => PlaceType::Suburb,
            Some("quarter") => PlaceType::Quarter,
            Some("neighbourhood") => PlaceType::Neighborhood,
            Some("isolated_dwelling") => PlaceType::IsolatedDwelling,
            _ => PlaceType::Unknown,
        }
    }

    fn ordinal(self) -> i32 {
        self as i32
    }
}

/// Packs place precedence ordering (`rank asc, place asc, population desc, name.length asc`) into an integer for the
/// sort-key field.
pub fn get_sort_key(rank: Option<i32>, place: Option<PlaceType>, population: i64, name: Option<&str>) -> i32 {
    SortKey
        // ORDER BY "rank" ASC NULLS LAST,
        ::order_by_int(rank.unwrap_or(15), 0, 15) // 4 bits
    // place ASC NULLS LAST,
    .then_by_int(place.map_or(15, PlaceType::ordinal), 0, 15) // 4 bits
    // population DESC NULLS LAST,
    .then_by_log(population as f64, MAX_CITY_POPULATION, 1.0, 1 << (SORT_KEY_BITS - 13 - 1))
    // length(name) ASC
    .then_by_int(name.map_or(0, |n| n.encode_utf16().count() as i32), 0, 31) // 5 bits
    .get()
}

/// Information extracted from a natural earth geographic region that will be inspected when joining with
/// OpenStreetMap data.
#[derive(Clone, Debug)]
struct NaturalEarthRegion {
    name: Option<Arc<str>>,
    rank: i32,
}

impl NaturalEarthRegion {
    fn new(name: Option<String>, max_rank: i32, ranks: &[f64]) -> NaturalEarthRegion {
        let rank =
            if ranks.is_empty() { max_rank } else { (ranks.iter().sum::<f64>() / ranks.len() as f64).ceil() as i32 };
        NaturalEarthRegion { name: name.map(Arc::from), rank }
    }
}

/// Information extracted from a natural earth place label that will be inspected when joining with OpenStreetMap
/// data.
#[derive(Debug)]
struct NaturalEarthPoint {
    name: Option<String>,
    wikidata: Option<String>,
    scale_rank: i32,
    names: Vec<String>,
}

/// Spatial indexes for joining natural earth place labels with their corresponding points from openstreetmap.
struct Indexes {
    countries: PolygonIndex<NaturalEarthRegion>,
    states: PolygonIndex<NaturalEarthRegion>,
    cities: PointIndex<Arc<NaturalEarthPoint>>,
}

impl Indexes {
    fn new() -> Indexes {
        Indexes { countries: PolygonIndex::create(), states: PolygonIndex::create(), cities: PointIndex::create() }
    }
}

/// Java `StringUtils.stripAccents`: removes diacritical marks (NFD + strip combining marks, `Ł`/`ł` -> `L`/`l`).
fn strip_accents(input: &str) -> String {
    let decomposed = DecomposingNormalizerBorrowed::new_nfd().normalize(input);
    decomposed
        .chars()
        .filter(|c| !('\u{0300}'..='\u{036F}').contains(c))
        .map(|c| match c {
            '\u{0141}' => 'L',
            '\u{0142}' => 'l',
            other => other,
        })
        .collect()
}

fn world_centroid(source: &dyn SourceFeature) -> GeometryResult<Point<f64>> {
    source
        .world_geometry()?
        .centroid()
        .ok_or_else(|| GeometryException::new("omt_place_centroid", "unable to compute centroid of empty geometry"))
}

pub struct Place {
    translations: Arc<Translations>,
    stats: StatsRef,
    indexes: RwLock<Indexes>,
    city_join_distance: f64,
}

impl Place {
    pub fn new(translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, stats: &StatsRef) -> Place {
        Place {
            translations: translations.clone(),
            stats: stats.clone(),
            indexes: RwLock::new(Indexes::new()),
            city_join_distance: city_join_distance(),
        }
    }

    fn names(&self, source: &dyn SourceFeature) -> planetiler_core::value::Tags {
        omt_language_utils::get_names(source.tags(), Some(&self.translations))
    }

    fn process_natural_earth_inner(&self, table: &str, feature: &dyn SourceFeature) -> GeometryResult<()> {
        let indexes = self.indexes.read();
        match table {
            "ne_10m_admin_0_countries" => indexes.countries.put(
                feature.world_geometry()?,
                NaturalEarthRegion::new(
                    feature.get_string("name"),
                    6,
                    &[feature.get_long("scalerank") as f64, feature.get_long("labelrank") as f64],
                ),
            ),
            "ne_10m_admin_1_states_provinces" => {
                let scalerank = feature.get_tag("scalerank").and_then(parse::parse_double_or_null);
                let labelrank = feature.get_tag("labelrank").and_then(parse::parse_double_or_null);
                if let (Some(scalerank), Some(labelrank)) = (scalerank, labelrank)
                    && scalerank <= 6.0
                    && labelrank <= 7.0
                {
                    indexes.states.put(
                        feature.world_geometry()?,
                        NaturalEarthRegion::new(
                            feature.get_string("name"),
                            6,
                            &[scalerank, labelrank, feature.get_long("datarank") as f64],
                        ),
                    );
                }
            }
            "ne_10m_populated_places" => {
                let mut names: Vec<String> = Vec::new();
                for key in ["name", "namealt", "meganame", "name_en", "nameascii"] {
                    if let Some(name) = feature.get_string(key) {
                        let lower = name.to_lowercase();
                        if !names.contains(&lower) {
                            names.push(lower);
                        }
                    }
                }
                indexes.cities.put(
                    feature.world_geometry()?,
                    Arc::new(NaturalEarthPoint {
                        name: feature.get_string("name"),
                        wikidata: feature.get_string("wikidataid"),
                        scale_rank: feature.get_long("scalerank") as i32,
                        names,
                    }),
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn country_rank(&self, element: &OsmCountryPoint<'_>) -> GeometryResult<Option<NaturalEarthRegion>> {
        let centroid = world_centroid(element.source)?;
        Ok(self.indexes.read().countries.get(&centroid))
    }

    fn city_rank(&self, element: &OsmCityPoint<'_>) -> GeometryResult<Option<i32>> {
        let point = world_centroid(element.source)?;
        let ne_cities = self.indexes.read().cities.get_within(&point, self.city_join_distance);
        if ne_cities.is_empty() {
            return Ok(None);
        }
        let raw_name = element.name();
        let raw_name = raw_name.as_deref().unwrap_or("");
        let name = raw_name.to_lowercase();
        let name_en = element.name_en().as_deref().unwrap_or("").to_lowercase();
        let normalized_name = strip_accents(raw_name);
        let wikidata = element.source.get_tag("wikidata").map(|v| v.to_string_lossy());
        let wikidata = wikidata.as_deref().unwrap_or("");
        for ne_city in ne_cities {
            if ne_city.wikidata.as_deref() == Some(wikidata)
                || ne_city.names.contains(&name)
                || ne_city.names.contains(&name_en)
                || ne_city.name.as_deref() == Some(normalized_name.as_str())
            {
                return Ok(Some(if ne_city.scale_rank <= 5 { ne_city.scale_rank + 1 } else { ne_city.scale_rank }));
            }
        }
        Ok(None)
    }
}

impl NaturalEarthProcessor for Place {
    fn process_natural_earth(&self, table: &str, feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {
        // store data from natural earth to help with ranks and min zoom levels when actually
        // emitting features from openstreetmap data.
        if let Err(e) = self.process_natural_earth_inner(table, feature) {
            e.log_with_stats(
                &*self.stats,
                "omt_place_ne",
                &format!(
                    "Error getting geometry for natural earth feature {table} {:?}",
                    feature.get_tag("ogc_fid").map(Value::to_string)
                ),
            );
        }
    }
}

impl OsmContinentPointHandler for Place {
    fn process_osm_continent_point(&self, element: &OsmContinentPoint<'_>, features: &mut FeatureCollector<'_>) {
        if !null_or_empty(element.name().as_deref()) {
            features
                .point(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .put_attrs(self.names(element.source))
                .set_attr(fields::CLASS, field_values::CLASS_CONTINENT)
                .set_attr(fields::RANK, 1)
                .set_zoom_range(0, 3);
        }
    }
}

impl OsmCountryPointHandler for Place {
    fn process_osm_country_point(&self, element: &OsmCountryPoint<'_>, features: &mut FeatureCollector<'_>) {
        if null_or_empty(element.name().as_deref()) {
            return;
        }
        let Some(iso_a2) = null_if_empty(element.country_code_iso3166_1_alpha_2())
            .or_else(|| null_if_empty(element.iso3166_1_alpha_2()))
            .or_else(|| null_if_empty(element.iso3166_1()))
        else {
            return;
        };
        match self.country_rank(element) {
            Ok(country) => {
                // set country rank to 6, unless there is a match in natural earth that indicates it
                // should be lower
                let mut rank = 7;
                let mut names = self.names(element.source);
                if let Some(country) = country {
                    if null_or_empty_value(names.get(fields::NAME_EN))
                        && let Some(name) = &country.name
                    {
                        names.insert(fields::NAME_EN.into(), Value::String(name.clone()));
                    }
                    rank = country.rank;
                }
                let rank = rank.clamp(1, 6);
                features
                    .point(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .put_attrs(names)
                    .set_attr(fields::ISO_A2, &*iso_a2)
                    .set_attr(fields::CLASS, field_values::CLASS_COUNTRY)
                    .set_attr(fields::RANK, rank)
                    .set_min_zoom((rank - 1) as u32)
                    .set_sort_key(rank);
            }
            Err(e) => e.log_with_stats(
                &*self.stats,
                "omt_place_country",
                &format!("Unable to get point for OSM country {}", element.source.id()),
            ),
        }
    }
}

impl OsmStatePointHandler for Place {
    fn process_osm_state_point(&self, element: &OsmStatePoint<'_>, features: &mut FeatureCollector<'_>) {
        // want the containing (not nearest) state polygon since we pre-filter the states in the polygon index
        // use natural earth to filter out any spurious states, and to set the rank field
        let state = match world_centroid(element.source) {
            Ok(centroid) => self.indexes.read().states.get_only_containing(&centroid),
            Err(e) => {
                e.log_with_stats(
                    &*self.stats,
                    "omt_place_state",
                    &format!("Unable to get point for OSM state {}", element.source.id()),
                );
                return;
            }
        };
        if let Some(state) = state {
            let mut names = self.names(element.source);
            if null_or_empty_value(names.get(fields::NAME_EN))
                && let Some(name) = &state.name
            {
                names.insert(fields::NAME_EN.into(), Value::String(name.clone()));
            }
            let rank = state.rank.clamp(1, 6);
            features
                .point(LAYER_NAME)
                .set_buffer_pixels(BUFFER_SIZE)
                .put_attrs(names)
                .set_attr(fields::CLASS, element.place().as_deref())
                .set_attr(fields::RANK, rank)
                // TODO: This starts including every "state" point at z2, even before many countries show up.
                //       Instead we might want to set state min zooms based on rank from natural earth?
                .set_min_zoom(2)
                .set_sort_key(rank);
        }
    }
}

impl OsmIslandPolygonHandler for Place {
    fn process_osm_island_polygon(&self, element: &OsmIslandPolygon<'_>, features: &mut FeatureCollector<'_>) {
        match element.source.area() {
            Ok(area) => {
                let rank = area_rank(area);
                let minzoom = if rank <= 3 {
                    8
                } else if rank <= 4 {
                    9
                } else {
                    10
                };
                features
                    .point_on_surface(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .put_attrs(self.names(element.source))
                    .set_attr(fields::CLASS, "island")
                    .set_attr(fields::RANK, rank)
                    .set_min_zoom(minzoom)
                    .set_sort_key(SortKey::order_by_log_max_levels(area, 1.0, MIN_ISLAND_WORLD_AREA).get());
            }
            Err(e) => e.log_with_stats(
                &*self.stats,
                "omt_place_island_poly",
                &format!("Unable to get point for OSM island polygon {}", element.source.id()),
            ),
        }
    }
}

impl OsmIslandPointHandler for Place {
    fn process_osm_island_point(&self, element: &OsmIslandPoint<'_>, features: &mut FeatureCollector<'_>) {
        features
            .point(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .put_attrs(self.names(element.source))
            .set_attr(fields::CLASS, "island")
            .set_attr(fields::RANK, 7)
            .set_min_zoom(12);
    }
}

impl OsmCityPointHandler for Place {
    fn process_osm_city_point(&self, element: &OsmCityPoint<'_>, features: &mut FeatureCollector<'_>) {
        let place = element.place();
        let place = place.as_deref();
        let mut rank: Option<i32> = None;
        if place.is_some_and(|p| MAJOR_CITY_PLACES.contains(&p)) {
            // only for major cities, attempt to find a nearby natural earth label with a similar
            // name and use that to set a rank from OSM that causes the label to be shown at lower
            // zoom levels
            match self.city_rank(element) {
                Ok(r) => rank = r,
                Err(e) => e.log_with_stats(
                    &*self.stats,
                    "omt_place_city",
                    &format!("Unable to get point for OSM city {}", element.source.id()),
                ),
            }
        }

        let capital = element.capital();
        let place_type = PlaceType::for_name(place);

        let minzoom = match rank {
            Some(1) => 2,
            Some(r) if r <= 8 => 3.max(r - 1) as u32,
            _ if place_type <= PlaceType::Town => 7,
            _ if place_type <= PlaceType::Village => 8,
            _ if place_type <= PlaceType::Suburb => 11,
            _ => 14,
        };

        let name = element.name();
        let feature = features
            .point(LAYER_NAME)
            .set_buffer_pixels(BUFFER_SIZE)
            .put_attrs(self.names(element.source))
            .set_attr(fields::CLASS, place)
            .set_attr(fields::RANK, rank)
            .set_min_zoom(minzoom)
            .set_sort_key(get_sort_key(rank, Some(place_type), element.population(), name.as_deref()))
            .set_point_label_grid_pixel_size(12, 128.0);

        if rank.is_none() {
            feature.set_point_label_grid_limit(Some(LABEL_GRID_LIMITS.clone()));
        }

        let capital_level = match capital.as_deref() {
            Some("2" | "yes") => Some(2),
            Some("3") => Some(3),
            Some("4") => Some(4),
            Some("5") => Some(5),
            Some("6") => Some(6),
            _ => None,
        };
        if let Some(level) = capital_level {
            feature.set_attr(fields::CAPITAL, level);
        }
    }
}

impl OsmBoundaryPolygonHandler for Place {
    fn process_osm_boundary_polygon(&self, element: &OsmBoundaryPolygon<'_>, features: &mut FeatureCollector<'_>) {
        match element.source.area() {
            Ok(area) => {
                let rank = area_rank(area);
                let minzoom = if rank <= 4 { rank + 5 } else { 10 };
                features
                    .point_on_surface(LAYER_NAME)
                    .set_buffer_pixels(BUFFER_SIZE)
                    .put_attrs(self.names(element.source))
                    .set_attr(boundary::fields::CLASS, element.boundary().as_deref())
                    .set_attr(fields::RANK, rank)
                    .set_min_zoom(minzoom as u32);
            }
            Err(e) => e.log_with_stats(
                &*self.stats,
                "omt_boundary_poly",
                &format!("Unable to get point for OSM boundary polygon {}", element.source.id()),
            ),
        }
    }
}

impl LayerPostProcessor for Place {
    fn post_process(&self, _zoom: u32, items: &mut Vec<vector_tile::Feature>) -> GeometryResult<()> {
        // infer the rank field from ordering of the place labels with each label grid square
        let mut group_counts: HashMap<i64, i64> = HashMap::new();
        for feature in items.iter_mut() {
            let count = group_counts.entry(feature.group).or_insert(1);
            let gridrank = *count;
            *count += 1;
            if !feature.tags.contains_key(fields::RANK) {
                feature.tags.insert(fields::RANK.into(), Value::Int(10 + gridrank));
            }
        }
        Ok(())
    }
}

impl Handler for Place {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }

    fn release(&self) {
        *self.indexes.write() = Indexes::new();
    }

    fn as_layer_post_processor(&self) -> Option<&dyn LayerPostProcessor> {
        Some(self)
    }
}

impl RowHandlers for Place {
    fn as_osm_continent_point_handler(&self) -> Option<&dyn OsmContinentPointHandler> {
        Some(self)
    }

    fn as_osm_country_point_handler(&self) -> Option<&dyn OsmCountryPointHandler> {
        Some(self)
    }

    fn as_osm_state_point_handler(&self) -> Option<&dyn OsmStatePointHandler> {
        Some(self)
    }

    fn as_osm_island_point_handler(&self) -> Option<&dyn OsmIslandPointHandler> {
        Some(self)
    }

    fn as_osm_island_polygon_handler(&self) -> Option<&dyn OsmIslandPolygonHandler> {
        Some(self)
    }

    fn as_osm_city_point_handler(&self) -> Option<&dyn OsmCityPointHandler> {
        Some(self)
    }

    fn as_osm_boundary_polygon_handler(&self) -> Option<&dyn OsmBoundaryPolygonHandler> {
        Some(self)
    }
}

impl Layer for Place {
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use planetiler_core::collection::feature_group::{SORT_KEY_MAX, SORT_KEY_MIN};
    use planetiler_core::geo::geo_utils;
    use planetiler_core::geo::test_utils::{new_point, rectangle};
    use planetiler_core::reader::simple_feature::SimpleFeature;
    use planetiler_core::tags;
    use planetiler_core::value::Tags;

    use super::{LAYER_NAME, PlaceType, get_sort_key, strip_accents};
    use crate::open_map_tiles_profile::{NATURAL_EARTH_SOURCE, OSM_SOURCE};
    use crate::test_support::*;

    fn ne(geometry: impl Into<geo_types::Geometry<f64>>, tags: Tags, layer: &str) -> SimpleFeature {
        SimpleFeature::create(geometry, tags, Some(NATURAL_EARTH_SOURCE), Some(layer), 0)
    }

    fn osm_at(x: f64, y: f64, tags: Tags) -> SimpleFeature {
        SimpleFeature::create(new_point(x, y), tags, Some(OSM_SOURCE), None::<&str>, 0)
    }

    #[test]
    fn test_strip_accents() {
        assert_eq!("Boston", strip_accents("Böston"));
        assert_eq!("Lodz", strip_accents("Łódź"));
    }

    #[test]
    fn test_continent() {
        let t = LayerTest::new();
        t.wikidata_translations.put(49, "es", "América del Norte y América Central");
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "continent",
                "name" => "North America",
                "name:en" => "North America",
                "name:es" => "América del Norte",
                "name:latin" => "North America",
                "rank" => 1,
                "_type" => "point",
                "_minzoom" => 0,
                "_maxzoom" => 3,
            }],
            &t.process(&point_feature(tags! {
                "place" => "continent",
                "wikidata" => "Q49",
                "name:es" => "América del Norte",
                "name" => "North America",
                "name:en" => "North America",
            })),
        );
    }

    #[test]
    fn test_country() {
        let t = LayerTest::new();
        t.wikidata_translations.put(30, "es", "Estados Unidos");
        t.process(&ne(
            rectangle(0.0, 0.25),
            tags! {"name" => "United States", "scalerank" => 0, "labelrank" => 2},
            "ne_10m_admin_0_countries",
        ));
        let osm_tags = || {
            tags! {
                "place" => "country",
                "wikidata" => "Q30",
                "name:es" => "Estados Unidos de América",
                "name" => "United States of America",
                "name:en" => "United States of America",
                "country_code_iso3166_1_alpha_2" => "US",
            }
        };
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "country",
                "name" => "United States of America",
                "name_en" => "United States of America",
                "name:es" => "Estados Unidos de América",
                "name:latin" => "United States of America",
                "iso_a2" => "US",
                "rank" => 6,
                "_type" => "point",
                "_minzoom" => 5,
            }],
            &t.process(&osm_at(0.5, 0.5, osm_tags())),
        );
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "country",
                "name" => "United States of America",
                "name_en" => "United States of America",
                "name:es" => "Estados Unidos de América",
                "name:latin" => "United States of America",
                "iso_a2" => "US",
                "rank" => 1,
                "_type" => "point",
                "_minzoom" => 0,
            }],
            &t.process(&osm_at(0.1, 0.1, osm_tags())),
        );
    }

    #[test]
    fn test_state() {
        let t = LayerTest::new();
        t.wikidata_translations.put(771, "es", "Massachusetts es");
        t.process(&ne(
            rectangle(0.0, 0.25),
            tags! {"name" => "Massachusetts", "scalerank" => 0, "labelrank" => 2, "datarank" => 1},
            "ne_10m_admin_1_states_provinces",
        ));
        t.process(&ne(
            rectangle(0.4, 0.6),
            tags! {"name" => "Massachusetts - not important", "scalerank" => 8, "labelrank" => 8, "datarank" => 1},
            "ne_10m_admin_1_states_provinces",
        ));
        let osm_tags = || {
            tags! {"place" => "state", "wikidata" => "Q771", "name" => "Massachusetts", "name:en" => "Massachusetts"}
        };

        // no match
        assert_features(0, &[], &t.process(&osm_at(1.0, 1.0, osm_tags())));

        // unimportant match
        assert_features(0, &[], &t.process(&osm_at(0.5, 0.5, osm_tags())));

        // important match
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "state",
                "name" => "Massachusetts",
                "name_en" => "Massachusetts",
                "name:es" => "Massachusetts es",
                "name:latin" => "Massachusetts",
                "rank" => 1,
                "_type" => "point",
                "_minzoom" => 2,
            }],
            &t.process(&osm_at(0.1, 0.1, osm_tags())),
        );
    }

    #[test]
    fn test_province() {
        let t = LayerTest::new();
        t.wikidata_translations.put(95027, "es", "provincia de Lugo");
        t.process(&ne(
            rectangle(0.0, 0.25),
            tags! {"name" => "Nova Scotia", "scalerank" => 3, "labelrank" => 3, "datarank" => 3},
            "ne_10m_admin_1_states_provinces",
        ));
        assert_features(
            4,
            &[tags! {
                "_layer" => "place",
                "class" => "province",
                "name" => "Lugo",
                "name:es" => "provincia de Lugo",
                "rank" => 3,
                "_type" => "point",
                "_minzoom" => 2,
            }],
            &t.process(&osm_at(0.1, 0.1, tags! {"place" => "province", "wikidata" => "Q95027", "name" => "Lugo"})),
        );
    }

    #[test]
    fn test_island_point() {
        let t = LayerTest::new();
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "island",
                "name" => "Nantucket",
                "name_en" => "Nantucket",
                "name:latin" => "Nantucket",
                "rank" => 7,
                "_type" => "point",
                "_minzoom" => 12,
            }],
            &t.process(&point_feature(tags! {"place" => "island", "name" => "Nantucket", "name:en" => "Nantucket"})),
        );
    }

    #[test]
    fn test_island_polygon() {
        let t = LayerTest::new();
        let island = || tags! {"place" => "island", "name" => "Nantucket", "name:en" => "Nantucket"};
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "island",
                "name" => "Nantucket",
                "name_en" => "Nantucket",
                "name:latin" => "Nantucket",
                "rank" => 1,
                "_type" => "point",
                "_minzoom" => 8,
            }],
            &t.process(&polygon_feature_with_area(1.0, island())),
        );

        let rank4area = (geo_utils::meters_to_pixel_at_equator(0, (40_000_000.0f64 - 1.0).sqrt()) / 256.0).powi(2);
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "island",
                "name" => "Nantucket",
                "rank" => 4,
                "_type" => "point",
                "_minzoom" => 9,
            }],
            &t.process(&polygon_feature_with_area(rank4area, island())),
        );
    }

    #[test]
    fn test_indigenous_land() {
        let t = LayerTest::new();
        let land = || {
            tags! {
                "type" => "boundary",
                "boundary" => "aboriginal_lands",
                "name" => "Seminole Nation",
                "name:en" => "Seminole Nation",
            }
        };
        assert_features(
            0,
            &[
                tags! {"_layer" => "boundary"},
                tags! {"_layer" => "park", "_type" => "polygon"},
                tags! {"_layer" => "park", "_type" => "point"},
                tags! {
                    "_layer" => "place",
                    "class" => "aboriginal_lands",
                    "name" => "Seminole Nation",
                    "name_en" => "Seminole Nation",
                    "name:latin" => "Seminole Nation",
                    "rank" => 1,
                    "_type" => "point",
                    "_minzoom" => 6,
                },
            ],
            &t.process(&polygon_feature_with_area(1.0, land())),
        );

        let rank2area = (geo_utils::meters_to_pixel_at_equator(0, (640_000_000.0f64 - 1.0).sqrt()) / 256.0).powi(2);
        assert_features(
            0,
            &[
                tags! {"_layer" => "boundary"},
                tags! {"_layer" => "park", "_type" => "polygon"},
                tags! {"_layer" => "park", "_type" => "point"},
                tags! {
                    "_layer" => "place",
                    "class" => "aboriginal_lands",
                    "name" => "Seminole Nation",
                    "rank" => 2,
                    "_type" => "point",
                    "_minzoom" => 7,
                },
            ],
            &t.process(&polygon_feature_with_area(rank2area, land())),
        );
    }

    /// The place part of `test_indigenous_land`, which runs without the boundary and park layers.
    #[test]
    fn test_indigenous_land_place_only() {
        let t = LayerTest::new();
        let feature = polygon_feature_with_area(
            1.0,
            tags! {
                "type" => "boundary",
                "boundary" => "aboriginal_lands",
                "name" => "Seminole Nation",
                "name:en" => "Seminole Nation",
            },
        );
        let collector = t.process(&feature);
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "aboriginal_lands",
                "name" => "Seminole Nation",
                "rank" => 1,
                "_type" => "point",
                "_minzoom" => 6,
            }],
            collector.iter().filter(|f| f.get_layer() == LAYER_NAME),
        );
    }

    #[test]
    fn test_place_sort_key_ranking() {
        let sort_keys = [
            // max
            get_sort_key(Some(0), Some(PlaceType::City), 1_000_000_000, Some("name")),
            get_sort_key(Some(0), Some(PlaceType::City), 1_000_000_000, Some("name longer")),
            get_sort_key(Some(0), Some(PlaceType::City), 1_000_000_000, Some(&"x".repeat(32))),
            get_sort_key(Some(0), Some(PlaceType::City), 10_000_000, Some("name")),
            get_sort_key(Some(0), Some(PlaceType::City), 0, Some("name")),
            get_sort_key(Some(0), Some(PlaceType::Town), 1_000_000_000, Some("name")),
            get_sort_key(Some(0), Some(PlaceType::IsolatedDwelling), 1_000_000_000, Some("name")),
            get_sort_key(Some(0), None, 1_000_000_000, Some("name")),
            get_sort_key(Some(1), Some(PlaceType::City), 1_000_000_000, Some("name")),
            get_sort_key(Some(10), Some(PlaceType::City), 1_000_000_000, Some("name")),
            get_sort_key(None, Some(PlaceType::City), 1_000_000_000, Some("name")),
            // min
            get_sort_key(None, None, 0, None),
        ];
        for (i, key) in sort_keys.iter().enumerate() {
            assert!(*key >= SORT_KEY_MIN, "Item at index {i} is < {SORT_KEY_MIN}: {key}");
            assert!(*key <= SORT_KEY_MAX, "Item at index {i} is > {SORT_KEY_MAX}: {key}");
        }
        assert_ascending(&sort_keys);
    }

    #[test]
    fn test_country_capital() {
        let t = LayerTest::new();
        t.process(&ne(
            new_point(0.0, 0.0),
            tags! {"name" => "Washington, D.C.", "scalerank" => 0, "wikidataid" => "Q61"},
            "ne_10m_populated_places",
        ));
        assert_features(
            7,
            &[tags! {
                "_layer" => "place",
                "class" => "city",
                "name" => "Washington, D.C.",
                "rank" => 1,
                "capital" => 2,
                "_labelgrid_limit" => 0,
                "_labelgrid_size" => 128.0,
                "_type" => "point",
                "_minzoom" => 2,
            }],
            &t.process(&point_feature(tags! {
                "place" => "city",
                "name" => "Washington, D.C.",
                "population" => "672228",
                "wikidata" => "Q61",
                "capital" => "yes",
            })),
        );
    }

    #[test]
    fn test_state_capital() {
        let t = LayerTest::new();
        t.process(&ne(
            new_point(0.0, 0.0),
            tags! {"name" => "Boston", "scalerank" => 2, "wikidataid" => "Q100"},
            "ne_10m_populated_places",
        ));
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "city",
                "name" => "Boston",
                "rank" => 3,
                "capital" => 4,
                "_type" => "point",
                "_minzoom" => 3,
            }],
            &t.process(&point_feature(
                tags! {"place" => "city", "name" => "Boston", "population" => "667137", "capital" => "4"},
            )),
        );
        // no match when far away
        assert_features(
            0,
            &[tags! {"_layer" => "place", "class" => "city", "name" => "Boston", "rank" => "<null>"}],
            &t.process(&osm_at(
                1.0,
                1.0,
                tags! {"place" => "city", "name" => "Boston", "wikidata" => "Q100", "population" => "667137", "capital" => "4"},
            )),
        );
        // unaccented name match
        assert_features(
            0,
            &[tags! {"_layer" => "place", "class" => "city", "rank" => 3}],
            &t.process(&point_feature(
                tags! {"place" => "city", "name" => "Böston", "population" => "667137", "capital" => "4"},
            )),
        );
        // wikidata only match
        assert_features(
            0,
            &[tags! {"_layer" => "place", "class" => "city", "rank" => 3}],
            &t.process(&point_feature(tags! {
                "place" => "city",
                "name" => "Other name",
                "population" => "667137",
                "wikidata" => "Q100",
                "capital" => "4",
            })),
        );
    }

    #[test]
    fn test_county_capital() {
        let t = LayerTest::new();
        t.process(&ne(
            new_point(0.0, 0.0),
            tags! {"name" => "Pueblo", "scalerank" => 7, "wikidataid" => "Q675576"},
            "ne_10m_populated_places",
        ));
        assert_features(
            0,
            &[tags! {
                "_layer" => "place",
                "class" => "city",
                "name" => "Pueblo",
                "rank" => 7,
                "capital" => 6,
                "_type" => "point",
                "_minzoom" => 6,
            }],
            &t.process(&point_feature(
                tags! {"place" => "city", "name" => "Pueblo", "population" => "111876", "capital" => "6"},
            )),
        );
        // no match when far away
        assert_features(
            0,
            &[tags! {"_layer" => "place", "class" => "city", "name" => "Pueblo", "rank" => "<null>"}],
            &t.process(&osm_at(
                1.0,
                1.0,
                tags! {"place" => "city", "name" => "Pueblo", "wikidata" => "Q675576", "population" => "111876", "capital" => "6"},
            )),
        );
        // unaccented name match
        assert_features(
            0,
            &[tags! {"_layer" => "place", "class" => "city", "rank" => 7}],
            &t.process(&point_feature(
                tags! {"place" => "city", "name" => "Pueblo", "population" => "111876", "capital" => "6"},
            )),
        );
        // wikidata only match
        assert_features(
            0,
            &[tags! {"_layer" => "place", "class" => "city", "rank" => 7}],
            &t.process(&point_feature(tags! {
                "place" => "city",
                "name" => "Other name",
                "population" => "111876",
                "wikidata" => "Q675576",
                "capital" => "6",
            })),
        );
    }

    #[test]
    fn test_city_without_natural_earth_match() {
        let t = LayerTest::new();
        assert_features(
            7,
            &[tags! {
                "_layer" => "place",
                "class" => "city",
                "rank" => "<null>",
                "_minzoom" => 7,
                "_labelgrid_limit" => 4,
                "_labelgrid_size" => 128.0,
            }],
            &t.process(&point_feature(tags! {"place" => "city", "name" => "City name"})),
        );
        assert_features(
            13,
            &[tags! {
                "_layer" => "place",
                "class" => "isolated_dwelling",
                "rank" => "<null>",
                "_labelgrid_limit" => 0,
                "_labelgrid_size" => 0.0,
                "_minzoom" => 14,
            }],
            &t.process(&point_feature(tags! {"place" => "isolated_dwelling", "name" => "City name"})),
        );
        assert_features(
            12,
            &[tags! {
                "_layer" => "place",
                "class" => "isolated_dwelling",
                "rank" => "<null>",
                "_labelgrid_limit" => 14,
                "_labelgrid_size" => 128.0,
                "_minzoom" => 14,
            }],
            &t.process(&point_feature(tags! {"place" => "isolated_dwelling", "name" => "City name"})),
        );
    }

    #[test]
    fn test_city_set_rank_from_gridrank() {
        let t = LayerTest::new();
        assert_eq!(Vec::<planetiler_core::vector_tile::Feature>::new(), t.post_process(LAYER_NAME, 13, vec![]));

        assert_eq!(
            vec![vt_point_feature(LAYER_NAME, tags! {"rank" => 11}, 1)],
            t.post_process(LAYER_NAME, 13, vec![vt_point_feature(LAYER_NAME, tags! {}, 1)])
        );

        assert_eq!(
            vec![
                vt_point_feature(LAYER_NAME, tags! {"name" => "a", "rank" => 11}, 1),
                vt_point_feature(LAYER_NAME, tags! {"name" => "b", "rank" => 12}, 1),
                vt_point_feature(LAYER_NAME, tags! {"name" => "c", "rank" => 11}, 2),
            ],
            t.post_process(
                LAYER_NAME,
                13,
                vec![
                    vt_point_feature(LAYER_NAME, tags! {"name" => "a"}, 1),
                    vt_point_feature(LAYER_NAME, tags! {"name" => "b"}, 1),
                    vt_point_feature(LAYER_NAME, tags! {"name" => "c"}, 2),
                ]
            )
        );
    }
}
