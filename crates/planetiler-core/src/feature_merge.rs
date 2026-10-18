//! Port of `FeatureMerge.java`.
//!
//! A collection of utilities for merging features with the same attributes in a rendered tile from
//! `Profile::post_process_layer_features` immediately before a tile is written to the output archive.
//!
//! Unlike postgis-based solutions that have a full view of all features after they are loaded into the database, the
//! planetiler engine only sees a single input feature at a time while processing source features, then only has
//! visibility into multiple features when they are grouped into a tile immediately before emitting. This ends up being
//! sufficient for most real-world use-cases but to do anything more that requires a view of multiple features _not_
//! within the same tile, profiles must store input features manually.
//!
//! Differences from Java:
//! - Functions take the feature list by value (unchanged features are moved to the output, not cloned).
//! - JTS `BufferOp` (mitre joins, mitre limit 5) is replaced by `geo`'s `Buffer` (backed by `i_overlay`'s outline
//!   offset) with `LineJoin::Miter(2 * asin(1 / 5))`, the equivalent minimum corner angle. `i_overlay` limits sharp
//!   corners slightly differently than JTS so buffered vertices can differ by a small fraction of the buffer distance.
//!   `buffer(0)` uses [`polygon_overlay::buffer_zero`] and union uses [`polygon_overlay::union_all`].
//! - Negative buffers of a collection with more than one polygon are applied to the union of the polygons (JTS
//!   treats a collection passed to `BufferOp` as a union; `geo` would offset each polygon independently).
//! - JTS `isWithinDistance` is the envelope check followed by `geo`'s Euclidean distance.
//! - The adjacency list passed to [`extract_connected_components`] is indexed by item (`&[Vec<usize>]`) instead of a
//!   hash map.

use geo::algorithm::buffer::{Buffer, BufferStyle, LineJoin};
use geo::{Area, Distance, Euclidean};
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use hashbrown::HashMap;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use tracing::warn;

use crate::geo::envelope::Envelope;
use crate::geo::geo_utils;
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::geometry_pipeline::{GeometryPipeline, GeometryPipelineRef};
use crate::geo::geometry_type::GeometryType;
use crate::geo::polygon_overlay;
use crate::stats::default_stats;
use crate::stats::stats::Stats;
use crate::util::loop_line_merger::LoopLineMerger;
use crate::value::Tags;
use crate::vector_tile::{self, ALL, Feature, VectorGeometry};

/// Modifies a feature ID to end in 0, indicating the feature was created by merging and doesn't correspond to a single
/// source element.
///
/// For OSM features, IDs are encoded as `osm_id * 10 + element_type` where element_type is 1 for nodes, 2 for ways, and
/// 3 for relations. IDs ending in 0 are reserved for features that don't map to a single OSM element.
fn make_merged_id(id: i64) -> i64 {
    (id / 10) * 10
}

#[inline]
fn tags_of(feature: &Feature) -> &Tags {
    &feature.tags
}

/// Returns a copy of `feature` (consumed) with `id` and `geometry` replaced.
#[inline]
fn with_id_and_geometry(feature: Feature, id: i64, geometry: VectorGeometry) -> Feature {
    Feature { id, geometry, ..feature }
}

/// Returns a copy of `feature` (consumed) with `geometry` replaced.
#[inline]
fn with_geometry(feature: Feature, geometry: VectorGeometry) -> Feature {
    Feature { geometry, ..feature }
}

/// Combines linestrings with the same set of attributes into a multilinestring where segments with touching endpoints
/// are merged by [`LoopLineMerger`], removing any linestrings under `min_length`.
///
/// Ignores any non-linestrings and passes them through to the output unaltered.
///
/// - `min_length`: minimum tile pixel length of features to emit, or 0 to emit all merged linestrings
/// - `tolerance`: after merging, simplify linestrings using this pixel tolerance, or -1 to skip simplification step
/// - `buffer`: number of pixels outside the visible tile area to include detail for, or -1 to skip clipping step
/// - `resimplify`: true if linestrings should be simplified even if they don't get merged with another
///
/// Returns a new list containing all unaltered features in their original order, then each of the merged groups
/// ordered by the index of the first element in that group from the input list.
pub fn merge_line_strings_resimplify(
    features: Vec<Feature>,
    min_length: f64,
    tolerance: f64,
    buffer: f64,
    resimplify: bool,
) -> Vec<Feature> {
    merge_line_strings_with(features, |_| min_length, tolerance, buffer, resimplify, None)
}

/// Merges linestrings with the same attributes as [`merge_line_strings_resimplify`] with `resimplify=false`.
pub fn merge_line_strings(features: Vec<Feature>, min_length: f64, tolerance: f64, buffer: f64) -> Vec<Feature> {
    merge_line_strings_resimplify(features, min_length, tolerance, buffer, false)
}

/// Merges linestrings with the same attributes as [`merge_line_strings_with`] with `resimplify=false` and no
/// pipeline.
pub fn merge_line_strings_dynamic(
    features: Vec<Feature>,
    length_limit_calculator: impl Fn(&Tags) -> f64,
    tolerance: f64,
    buffer: f64,
) -> Vec<Feature> {
    merge_line_strings_with(features, length_limit_calculator, tolerance, buffer, false, None)
}

/// Merges linestrings with the same attributes as [`merge_line_strings_resimplify`] except with a dynamic length limit
/// computed by `length_limit_calculator` for the attributes of each group, and an optional `pipeline` that is applied
/// to each segment between intersections instead of simplification.
pub fn merge_line_strings_with(
    features: Vec<Feature>,
    length_limit_calculator: impl Fn(&Tags) -> f64,
    tolerance: f64,
    buffer: f64,
    resimplify: bool,
    pipeline: Option<GeometryPipelineRef>,
) -> Vec<Feature> {
    let mut result = Vec::with_capacity(features.len());
    let grouped = group_by_attrs(features, &mut result, GeometryType::Line);
    for group in grouped {
        let length_limit = length_limit_calculator(tags_of(&group[0]));
        // as a shortcut, can skip line merging only if:
        // - only 1 element in the group
        // - it doesn't need to be clipped
        // - and it can't possibly be filtered out for being too short
        // - and it does not need to be simplified
        if group.len() == 1 && buffer <= 0.0 && length_limit <= 0.0 && (!resimplify || tolerance <= 0.0) {
            result.extend(group);
            continue;
        }
        let mut merger = LoopLineMerger::new();
        merger
            .set_tolerance(tolerance)
            .set_merge_strokes(true)
            .set_min_length(length_limit)
            .set_loop_min_length(length_limit)
            .set_stub_min_length(length_limit.min(0.5));
        if let Some(pipeline) = &pipeline {
            merger.set_segment_transform(pipeline.clone());
        }
        for feature in &group {
            match feature.geometry.decode() {
                Ok(geom) => {
                    merger.add(&geom);
                }
                Err(e) => warn!("Error decoding vector tile feature for line merge: {feature:?}: {e}"),
            }
        }
        let mut output_segments: Vec<LineString<f64>> = Vec::new();
        for line in merger.get_merged_line_strings() {
            if buffer >= 0.0 {
                remove_detail_outside_tile(&line, buffer, &mut output_segments);
            } else {
                output_segments.push(line);
            }
        }

        if !output_segments.is_empty() {
            sort_by_hilbert_index(&mut output_segments, |l| l.0.first().copied());
            let new_geometry = geo_utils::combine_line_strings(output_segments);
            let size = group.len();
            let feature1 = group.into_iter().next().unwrap();
            let encoded = vector_tile::encode_geometry(&new_geometry);
            if size > 1 {
                let id = make_merged_id(feature1.id);
                result.push(with_id_and_geometry(feature1, id, encoded));
            } else {
                result.push(with_geometry(feature1, encoded));
            }
        }
    }
    result
}

/// Merges points with the same attributes into multipoints.
pub fn merge_multi_point(features: Vec<Feature>) -> Vec<Feature> {
    merge_geometries(features, GeometryType::Point)
}

/// Merges polygons with the same attributes into multipolygons.
///
/// NOTE: This does not attempt to combine overlapping geometries, see [`merge_overlapping_polygons`] or
/// [`merge_nearby_polygons`] for that.
pub fn merge_multi_polygon(features: Vec<Feature>) -> Vec<Feature> {
    merge_geometries(features, GeometryType::Polygon)
}

/// Merges linestrings with the same attributes into multilinestrings.
///
/// NOTE: This does not attempt to connect linestrings that intersect at endpoints, see [`merge_line_strings`] for
/// that. Also, this removes extra detail that was preserved to improve connected-linestring merging, so you should
/// only use one or the other.
pub fn merge_multi_line_string(features: Vec<Feature>) -> Vec<Feature> {
    merge_geometries(features, GeometryType::Line)
}

fn merge_geometries(features: Vec<Feature>, geometry_type: GeometryType) -> Vec<Feature> {
    let mut result = Vec::with_capacity(features.len());
    let grouped = group_by_attrs(features, &mut result, geometry_type);
    for group in grouped {
        if group.len() == 1 {
            result.extend(group);
            continue;
        }
        let mut sorted: Vec<(i32, &VectorGeometry)> =
            group.iter().map(|f| (f.geometry.hilbert_index(), &f.geometry)).collect();
        sorted.sort_by_key(|(h, _)| *h);
        let mut combined = vector_tile::new_merger(geometry_type);
        for (_, geometry) in sorted {
            // can't fail: every geometry in the group has `geometry_type`
            let _ = combined.accept(geometry);
        }
        let merged = combined.finish();
        let feature1 = group.into_iter().next().unwrap();
        let id = make_merged_id(feature1.id);
        result.push(with_id_and_geometry(feature1, id, merged));
    }
    result
}

/// Removes any segments from `input` where both the start and end are outside the tile boundary (plus `buffer`) and
/// puts the resulting segments into `output`.
fn remove_detail_outside_tile(input: &LineString<f64>, buffer: f64, output: &mut Vec<LineString<f64>>) {
    let seq = &input.0;
    if seq.is_empty() {
        return;
    }
    let mut current: Vec<Coord<f64>> = Vec::new();
    let mut was_in = false;
    let min = -buffer;
    let max = 256.0 + buffer;
    // envelope of a segment intersects the (min, max) square
    let intersects = |x1: f64, x2: f64, y1: f64, y2: f64| {
        x1.max(x2) >= min && x1.min(x2) <= max && y1.max(y2) >= min && y1.min(y2) <= max
    };
    let (mut x, mut y) = (seq[0].x, seq[0].y);
    for next in &seq[1..] {
        let (next_x, next_y) = (next.x, next.y);
        let now_in = intersects(x, next_x, y, next_y);
        if now_in || was_in {
            current.push(Coord { x, y });
        } else if !current.is_empty() {
            // out: wait to flush until 2 consecutive outs
            if current.len() >= 2 {
                output.push(LineString(std::mem::take(&mut current)));
            } else {
                current.clear();
            }
        }
        was_in = now_in;
        x = next_x;
        y = next_y;
    }

    // last point
    let last = seq[seq.len() - 1];
    if intersects(x, last.x, y, last.y) || was_in {
        current.push(last);
    }

    if current.len() >= 2 {
        output.push(LineString(current));
    }
}

/// Combines polygons with the same set of attributes into a multipolygon where overlapping/touching polygons are
/// combined into fewer polygons covering the same area.
///
/// Ignores any non-polygons and passes them through to the output unaltered.
///
/// - `min_area`: minimum area in square tile pixels of polygons to emit
///
/// Returns a new list containing all unaltered features in their original order, then each of the merged groups
/// ordered by the index of the first element in that group from the input list.
pub fn merge_overlapping_polygons(features: Vec<Feature>, min_area: f64) -> GeometryResult<Vec<Feature>> {
    merge_nearby_polygons(features, min_area, 0.0, 0.0, 0.0)
}

/// [`merge_nearby_polygons_with`] using the default stats and no pipeline.
pub fn merge_nearby_polygons(
    features: Vec<Feature>,
    min_area: f64,
    min_hole_area: f64,
    min_dist: f64,
    buffer: f64,
) -> GeometryResult<Vec<Feature>> {
    let stats = default_stats::get();
    merge_nearby_polygons_with(features, min_area, min_hole_area, min_dist, buffer, &*stats, None)
}

/// Combines polygons with the same set of attributes within `min_dist` from each other, expanding then contracting
/// the merged geometry by `buffer` to combine polygons that are almost touching.
///
/// Ignores any non-polygons and passes them through to the output unaltered.
///
/// - `min_area`: minimum area in square tile pixels of polygons to emit
/// - `min_hole_area`: the minimum area in square tile pixels of inner rings of polygons to emit
/// - `min_dist`: the minimum threshold in tile pixels between polygons to combine into a group
/// - `buffer`: the amount (in tile pixels) to expand then contract polygons by in order to combine almost-touching
///   polygons
/// - `stats`: for counting data errors
/// - `pipeline`: a transform that should be applied to each merged polygon in tile pixel coordinates where `0,0` is
///   the top-left and `256,256` is the bottom-right corner of the tile
///
/// Returns a new list containing all unaltered features in their original order, then each of the merged groups
/// ordered by the index of the first element in that group from the input list.
pub fn merge_nearby_polygons_with(
    features: Vec<Feature>,
    min_area: f64,
    min_hole_area: f64,
    min_dist: f64,
    buffer: f64,
    stats: &dyn Stats,
    pipeline: Option<&dyn GeometryPipeline>,
) -> GeometryResult<Vec<Feature>> {
    let mut result = Vec::with_capacity(features.len());
    let grouped = group_by_attrs(features, &mut result, GeometryType::Polygon);
    for group in grouped {
        let mut out_polygons: Vec<Polygon<f64>> = Vec::new();
        let mut geometries: Vec<Geometry<f64>> = Vec::with_capacity(group.len());
        for feature in &group {
            match feature.geometry.decode() {
                Ok(geom) => geometries.push(geom),
                Err(e) => warn!("Error decoding vector tile feature for polygon merge: {feature:?}: {e}"),
            }
        }
        let grouped_by_proximity = group_polygons_by_proximity(&geometries, min_dist);
        for polygon_group in grouped_by_proximity {
            let merged = if polygon_group.len() > 1 {
                let mut merged = if buffer > 0.0 {
                    // there are 2 ways to merge polygons:
                    // 1) bufferUnbuffer: merged.buffer(amount).buffer(-amount)
                    // 2) bufferUnionUnbuffer: polygon.buffer(amount) on each polygon then merged.union().buffer(-amount)
                    // #1 is faster on average, but can become very slow and use a lot of memory when there is a large
                    // overlap between buffered polygons (i.e. most of them are smaller than the buffer amount) so we
                    // use #2 to avoid spinning for a very long time on very dense tiles.
                    buffer_union_unbuffer(buffer, &polygon_group, stats)?
                } else {
                    let collection =
                        geo_utils::create_geometry_collection(polygon_group.iter().map(|g| (*g).clone()).collect());
                    buffer_geometry(buffer, &collection)?
                };
                if !is_polygonal(&merged) || Envelope::of_geometry(&merged).area() < min_area {
                    continue;
                }
                if let Some(pipeline) = pipeline {
                    merged = pipeline.apply(&merged);
                    if !is_polygonal(&merged) {
                        continue;
                    }
                }
                reverse(geo_utils::snap_and_fix_polygon(&merged, stats, "merge")?)
            } else {
                let merged = polygon_group[0];
                if !is_polygonal(merged) || Envelope::of_geometry(merged).area() < min_area {
                    continue;
                }
                match pipeline {
                    Some(pipeline) => {
                        let after = pipeline.apply(merged);
                        if !is_polygonal(&after) {
                            continue;
                        } else if after != *merged {
                            reverse(geo_utils::snap_and_fix_polygon(&after, stats, "merge_after_pipeline")?)
                        } else {
                            after
                        }
                    }
                    None => merged.clone(),
                }
            };
            extract_polygons(merged, &mut out_polygons, min_area, min_hole_area);
        }
        if !out_polygons.is_empty() {
            sort_by_hilbert_index(&mut out_polygons, |p| p.exterior().0.first().copied());
            let combined = geo_utils::combine_polygons(out_polygons);
            let size = group.len();
            let feature1 = group.into_iter().next().unwrap();
            let encoded = vector_tile::encode_geometry(&combined);
            if size > 1 {
                let id = make_merged_id(feature1.id);
                result.push(with_id_and_geometry(feature1, id, encoded));
            } else {
                result.push(with_geometry(feature1, encoded));
            }
        }
    }
    Ok(result)
}

fn is_polygonal(geom: &Geometry<f64>) -> bool {
    matches!(geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_))
}

/// JTS `Geometry.reverse()` for polygonal geometries: reverses the direction of every ring.
fn reverse(geom: Geometry<f64>) -> Geometry<f64> {
    fn reverse_poly(p: Polygon<f64>) -> Polygon<f64> {
        let (mut exterior, mut interiors) = p.into_inner();
        exterior.0.reverse();
        for ring in &mut interiors {
            ring.0.reverse();
        }
        Polygon::new(exterior, interiors)
    }
    match geom {
        Geometry::Polygon(p) => Geometry::Polygon(reverse_poly(p)),
        Geometry::MultiPolygon(mp) => {
            Geometry::MultiPolygon(MultiPolygon(mp.0.into_iter().map(reverse_poly).collect()))
        }
        Geometry::GeometryCollection(gc) => {
            Geometry::GeometryCollection(geo_types::GeometryCollection(gc.0.into_iter().map(reverse).collect()))
        }
        Geometry::LineString(mut l) => {
            l.0.reverse();
            Geometry::LineString(l)
        }
        other => other,
    }
}

/// Stable sort of `geometries` by the hilbert index of their first coordinate (Java `sortByHilbertIndex`).
fn sort_by_hilbert_index<G>(geometries: &mut [G], first: impl Fn(&G) -> Option<Coord<f64>>) {
    if geometries.len() <= 1 {
        return;
    }
    let index = |g: &G| match first(g) {
        Some(c) => hilbert_index_of(c),
        None => 0,
    };
    geometries.sort_by_cached_key(index);
}

#[inline]
fn hilbert_index_of(coord: Coord<f64>) -> i32 {
    vector_tile::hilbert_index(&Geometry::Point(geo_types::Point(coord)))
}

/// Returns all the clusters from `geometries` where elements in the group are less than `min_dist` from another
/// element in the group.
pub fn group_polygons_by_proximity(geometries: &[Geometry<f64>], min_dist: f64) -> Vec<Vec<&Geometry<f64>>> {
    let adjacency_list = extract_adjacency_list(geometries, min_dist);
    extract_connected_components(&adjacency_list, geometries.len())
        .into_iter()
        .map(|ids| ids.into_iter().map(|i| &geometries[i]).collect())
        .collect()
}

/// Returns each group of vector tile features that share the exact same attributes.
///
/// - `features`: the set of input features
/// - `others`: list to add any feature that does not match `geometry_type`
/// - `geometry_type`: the type of geometries to return in the result
///
/// Returns all the elements from `features` of type `geometry_type` grouped by attributes, in order of the first
/// element of each group.
pub fn group_by_attrs(
    features: Vec<Feature>,
    others: &mut Vec<Feature>,
    geometry_type: GeometryType,
) -> Vec<Vec<Feature>> {
    let mut groups: Vec<Vec<Feature>> = Vec::new();
    // order-independent hash of the tags (matching map equality) -> indices of groups with that hash
    let mut by_hash: HashMap<u64, smallvec_lite::Indices> = HashMap::new();
    let hasher = ahash::RandomState::with_seeds(1, 2, 3, 4);
    for feature in features {
        if feature.geometry.geom_type() != geometry_type {
            // just ignore and pass through non-matching features
            others.push(feature);
            continue;
        }
        let tags = tags_of(&feature);
        let hash = tags.iter().map(|entry| hasher.hash_one(entry)).fold(tags.len() as u64, u64::wrapping_add);
        let candidates = by_hash.entry(hash).or_default();
        let found = candidates.iter().copied().find(|&g| tags_of(&groups[g][0]) == tags);
        match found {
            Some(g) => groups[g].push(feature),
            None => {
                candidates.push(groups.len());
                groups.push(vec![feature]);
            }
        }
    }
    groups
}

/// Tiny inline list of group indices (almost always exactly one per hash).
mod smallvec_lite {
    #[derive(Default)]
    pub(super) struct Indices {
        first: Option<usize>,
        rest: Vec<usize>,
    }

    impl Indices {
        pub(super) fn iter(&self) -> impl Iterator<Item = &usize> {
            self.first.iter().chain(self.rest.iter())
        }

        pub(super) fn push(&mut self, idx: usize) {
            if self.first.is_none() {
                self.first = Some(idx);
            } else {
                self.rest.push(idx);
            }
        }
    }
}

/// Merges nearby polygons by expanding each individual polygon by `buffer`, unioning them, and contracting the
/// result.
pub fn buffer_union_unbuffer<G: std::borrow::Borrow<Geometry<f64>>>(
    buffer: f64,
    polygon_group: &[G],
    stats: &dyn Stats,
) -> GeometryResult<Geometry<f64>> {
    // A simpler alternative that might initially appear faster would be:
    //
    // merged = unbuffer(buffer, buffer(buffer, collection(polygonGroup)))
    //
    // But since buffer() is faster than union() only when the amount of overlap is small, this technique becomes very
    // slow for merging many small nearby polygons.
    //
    // The following approach is slower most of the time, but faster on average because it does not choke on dense
    // nearby polygons:
    let mut buffered: Vec<Geometry<f64>> = Vec::with_capacity(polygon_group.len());
    let mut max_area_before: f64 = 0.0;
    for geometry in polygon_group {
        let geometry = geometry.borrow();
        max_area_before = max_area_before.max(geometry.unsigned_area());
        buffered.push(buffer_geometry(buffer, geometry)?);
    }
    let before_union = geo_utils::create_geometry_collection(buffered);
    let merged = match polygon_overlay::union_all(&before_union) {
        Ok(merged) => merged,
        Err(_) => {
            // buffer result is sometimes invalid, which makes union throw so fix it and try again (see #700)
            stats.data_error("buffer_union_unbuffer_union_failed");
            let fixed = polygon_overlay::geometry_fixer(&before_union)?;
            polygon_overlay::union_all(&fixed)?
        }
    };
    let mut merged = unbuffer(buffer, &merged)?;
    // in extremely rare cases this can make output too small (see https://github.com/locationtech/jts/issues/1183).
    // Until that issue is fixed, try un-buffering the output of the buffer step before unioning
    if max_area_before > 100.0 {
        let area_after = merged.unsigned_area();
        if area_after < max_area_before * 0.95 {
            let merged2 = unbuffer(buffer, &before_union)?;
            let area_after2 = merged2.unsigned_area();
            if area_after2 < max_area_before * 0.95 {
                stats.data_error("buffer_union_unbuffer_still_too_small");
                // and if that still fails, just union all of the input polygons without buffer/unbuffering
                let all = polygon_group.iter().map(|g| g.borrow().clone()).collect();
                return polygon_overlay::union_all(&geo_utils::create_geometry_collection(all));
            } else {
                stats.data_error("buffer_union_unbuffer_too_small");
            }
            if area_after2 > area_after {
                merged = merged2;
            }
        }
    }
    Ok(merged)
}

/// Minimum corner angle that gets a full mitre join, equivalent to the JTS default mitre limit of 5.
fn mitre_angle() -> f64 {
    2.0 * (1.0f64 / 5.0).asin()
}

fn unbuffer(buffer: f64, merged: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    buffer_geometry(-buffer, merged)
}

/// JTS `BufferOp(geom, JOIN_MITRE).getResultGeometry(distance)` for polygonal geometries.
fn buffer_geometry(distance: f64, geom: &Geometry<f64>) -> GeometryResult<Geometry<f64>> {
    if distance == 0.0 {
        return polygon_overlay::buffer_zero(geom);
    }
    let mut polys: Vec<Polygon<f64>> =
        polygon_overlay::polygons_of(geom).into_iter().filter(|p| !p.exterior().0.is_empty()).cloned().collect();
    if polys.is_empty() {
        return Ok(polygon_overlay::to_polygonal(vec![]));
    }
    if distance < 0.0 && polys.len() > 1 {
        // JTS buffers a collection as the union of its parts; contracting each part separately would not erase the
        // boundaries between overlapping parts
        polys =
            polygon_overlay::polygons_of(&polygon_overlay::buffer_zero(&Geometry::MultiPolygon(MultiPolygon(polys)))?)
                .into_iter()
                .cloned()
                .collect();
    }
    let style = BufferStyle::new(distance).line_join(LineJoin::Miter(mitre_angle()));
    let tolerance = collinear_tolerance(&polys, distance);
    let result = if polys.len() == 1 {
        polys.pop().unwrap().buffer_with_style(style)
    } else {
        MultiPolygon(polys).buffer_with_style(style)
    };
    let result = result.0.into_iter().filter_map(|p| remove_near_collinear_vertices(p, tolerance)).collect();
    Ok(polygon_overlay::to_polygonal(result))
}

/// Tolerance for [`remove_near_collinear_vertices`] on the buffer output of `polys` by `distance`.
///
/// geo's `Buffer` (i_overlay) works on an integer grid fitted to the output bounds; its mitre joins emit the two ends
/// of the adjacent offset segments plus the mitre point, and the two ends are rounded to that grid independently, so
/// they end up a few grid units off the true (straight) edge. `H * 2^-25` with `H = 0.5 * max(width, height) + 6|d|`
/// is about 16-32 grid units: well above that rounding error, and far below the offset of any real vertex on the
/// 1/16 px tile grid.
fn collinear_tolerance(polys: &[Polygon<f64>], distance: f64) -> f64 {
    let mut min = Coord { x: f64::INFINITY, y: f64::INFINITY };
    let mut max = Coord { x: f64::NEG_INFINITY, y: f64::NEG_INFINITY };
    for c in polys.iter().flat_map(|p| p.exterior().0.iter()) {
        min = Coord { x: min.x.min(c.x), y: min.y.min(c.y) };
        max = Coord { x: max.x.max(c.x), y: max.y.max(c.y) };
    }
    let half_extent = 0.5 * (max.x - min.x).max(max.y - min.y);
    (half_extent + 6.0 * distance.abs()) * 2f64.powi(-25)
}

/// Removes vertices that lie within `tolerance` of the line through their neighbors from the rings of a `geo::Buffer`
/// output polygon (JTS buffer output never has collinear vertices). Spikes (where the path doubles back) are kept.
/// Returns `None` if the exterior ring collapses; collapsed holes are dropped.
fn remove_near_collinear_vertices(poly: Polygon<f64>, tolerance: f64) -> Option<Polygon<f64>> {
    let (exterior, interiors) = poly.into_inner();
    let exterior = remove_near_collinear_ring(exterior, tolerance)?;
    let interiors = interiors.into_iter().filter_map(|r| remove_near_collinear_ring(r, tolerance)).collect();
    Some(Polygon::new(exterior, interiors))
}

/// Distance of `b` from the line through `a` and `c`, or `None` if `b` is not between them (a spike, where the path
/// doubles back at `b`, or a degenerate `a == c`).
fn collinear_deviation(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> Option<f64> {
    let (ab, ac, bc) = (b - a, c - a, c - b);
    let len = ac.x.hypot(ac.y);
    if len == 0.0 || ab.x * bc.x + ab.y * bc.y <= 0.0 {
        return None;
    }
    Some((ab.x * ac.y - ab.y * ac.x).abs() / len)
}

/// Removes vertices within `tolerance` of the line through their (remaining) neighbors, smallest deviation first.
///
/// Going smallest-first matters at very shallow outside corners: there geo emits three vertices (the two offset
/// segment ends and the mitre point) that are all within `tolerance` of each other's lines, but only the two ends lie
/// on the true edges. Removing greedily leaves one of the three (all within `d * tan(angle / 2)` of the corner), which
/// is then far from the line through the next real vertices on either side and is kept, like the single vertex JTS
/// emits for such corners.
fn remove_near_collinear_ring(ring: LineString<f64>, tolerance: f64) -> Option<LineString<f64>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    let mut pts = ring.0;
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    let n = pts.len();
    if n < 3 {
        return None;
    }
    let mut prev: Vec<usize> = (0..n).map(|i| (i + n - 1) % n).collect();
    let mut next: Vec<usize> = (0..n).map(|i| (i + 1) % n).collect();
    let mut version = vec![0u32; n];
    let mut alive = vec![true; n];
    let mut remaining = n;
    // deviations are non-negative, so their bit patterns sort like the values
    let mut heap: BinaryHeap<Reverse<(u64, usize, u32)>> = BinaryHeap::new();
    let push = |heap: &mut BinaryHeap<_>, i: usize, prev: &[usize], next: &[usize], version: &[u32]| {
        if let Some(dev) = collinear_deviation(pts[prev[i]], pts[i], pts[next[i]])
            && dev <= tolerance
        {
            heap.push(Reverse((dev.to_bits(), i, version[i])));
        }
    };
    for i in 0..n {
        push(&mut heap, i, &prev, &next, &version);
    }
    while let Some(Reverse((_, i, v))) = heap.pop() {
        if !alive[i] || v != version[i] {
            continue;
        }
        if remaining <= 3 {
            return None;
        }
        alive[i] = false;
        remaining -= 1;
        let (p, q) = (prev[i], next[i]);
        next[p] = q;
        prev[q] = p;
        for j in [p, q] {
            version[j] += 1;
            push(&mut heap, j, &prev, &next, &version);
        }
    }
    let start = (0..n).find(|&i| alive[i])?;
    let mut out = Vec::with_capacity(remaining + 1);
    let mut i = start;
    loop {
        out.push(pts[i]);
        i = next[i];
        if i == start {
            break;
        }
    }
    out.push(pts[start]);
    Some(LineString(out))
}

/// Puts all polygons within `geom` over `min_area` into `result`, removing holes under `min_hole_area`.
fn extract_polygons(geom: Geometry<f64>, result: &mut Vec<Polygon<f64>>, min_area: f64, min_hole_area: f64) {
    match geom {
        Geometry::Polygon(poly) => extract_polygon(poly, result, min_area, min_hole_area),
        Geometry::MultiPolygon(mp) => {
            for poly in mp.0 {
                extract_polygon(poly, result, min_area, min_hole_area);
            }
        }
        Geometry::GeometryCollection(gc) => {
            for g in gc.0 {
                extract_polygons(g, result, min_area, min_hole_area);
            }
        }
        _ => {}
    }
}

fn extract_polygon(poly: Polygon<f64>, result: &mut Vec<Polygon<f64>>, min_area: f64, min_hole_area: f64) {
    let outer_area = geo_utils::ring_area(&poly.exterior().0);
    if outer_area > min_area {
        let (mut exterior, mut interiors) = poly.into_inner();
        // NOTE: Java compares hole area to `min_area` (not `min_hole_area`); kept for parity
        interiors.retain(|ring| min_hole_area <= 0.0 || geo_utils::ring_area(&ring.0) > min_area);
        // optimization: when merged polygon fill the entire tile, replace it with a canonical fill geometry to ensure
        // that filled tiles are byte-for-byte equivalent. This allows archives that deduplicate tiles to better
        // compress large filled areas like the ocean.
        let fill_buffer = is_fill(outer_area, &exterior);
        if fill_buffer >= 0.0 {
            exterior = create_fill(fill_buffer);
        }
        result.push(Polygon::new(exterior, interiors));
    }
}

const NOT_FILL: f64 = -1.0;

/// If `exterior_ring` fills the entire tile, return the number of pixels that it overhangs, otherwise -1.
fn is_fill(outer_area: f64, exterior_ring: &LineString<f64>) -> f64 {
    if outer_area < 256.0 * 256.0 {
        return NOT_FILL;
    }
    let proposed_buffer = (outer_area.sqrt() - 256.0) / 2.0;
    let min = -(proposed_buffer * 0.9);
    let max = 256.0 + proposed_buffer * 0.9;
    let mut visited = 0;
    let cs = &exterior_ring.0;
    if cs.is_empty() {
        return NOT_FILL;
    }
    let mut next_side = VectorGeometry::get_side_f64(cs[0].x, cs[0].y, min, max);
    for c in &cs[1..] {
        let side = next_side;
        visited |= side;
        next_side = VectorGeometry::get_side_f64(c.x, c.y, min, max);
        if VectorGeometry::segment_crosses_tile(side, next_side) {
            return NOT_FILL;
        }
    }
    if visited == ALL { proposed_buffer } else { NOT_FILL }
}

fn create_fill(buffer: f64) -> LineString<f64> {
    let min = -buffer;
    let max = buffer + 256.0;
    LineString(geo_utils::coordinate_sequence(&[min, min, max, min, max, max, min, max, min, min]))
}

fn envelope_distance(a: &Envelope, b: &Envelope) -> f64 {
    if a.intersects(b) {
        return 0.0;
    }
    let dx = if a.max_x < b.min_x {
        b.min_x - a.max_x
    } else if a.min_x > b.max_x {
        a.min_x - b.max_x
    } else {
        0.0
    };
    let dy = if a.max_y < b.min_y {
        b.min_y - a.max_y
    } else if a.min_y > b.max_y {
        a.min_y - b.max_y
    } else {
        0.0
    };
    dx.hypot(dy)
}

/// Returns, for each index in `geometries`, the index of every other geometry within `min_dist`.
fn extract_adjacency_list(geometries: &[Geometry<f64>], min_dist: f64) -> Vec<Vec<usize>> {
    let mut result: Vec<Vec<usize>> = vec![Vec::new(); geometries.len()];
    if geometries.len() <= 1 {
        return result;
    }
    let envelopes: Vec<Envelope> = geometries.iter().map(Envelope::of_geometry).collect();
    let items: Vec<GeomWithData<Rectangle<[f64; 2]>, usize>> = envelopes
        .iter()
        .enumerate()
        .filter(|(_, env)| !env.is_null())
        .map(|(i, env)| {
            let mut env = *env;
            env.expand_by(min_dist);
            GeomWithData::new(Rectangle::from_corners([env.min_x, env.min_y], [env.max_x, env.max_y]), i)
        })
        .collect();
    let index = RTree::bulk_load(items);
    // Every candidate pair is usually visited from both sides; remember the (symmetric) distance test result of pairs
    // whose other side has not been queried yet so it is computed once.
    let mut pending: hashbrown::HashMap<(usize, usize), bool, foldhash::fast::FixedState> = Default::default();
    for (i, a) in geometries.iter().enumerate() {
        let env_a = &envelopes[i];
        if env_a.is_null() {
            continue;
        }
        let query = AABB::from_corners([env_a.min_x, env_a.min_y], [env_a.max_x, env_a.max_y]);
        for item in index.locate_in_envelope_intersecting(query) {
            let j = item.data;
            let within = if i == j && min_dist >= 0.0 {
                // a non-empty geometry is at distance 0 from itself
                true
            } else if j < i {
                match pending.remove(&(j, i)) {
                    Some(within) => within,
                    None => is_within_distance(a, env_a, &geometries[j], &envelopes[j], min_dist),
                }
            } else {
                let within = is_within_distance(a, env_a, &geometries[j], &envelopes[j], min_dist);
                if j > i {
                    pending.insert((i, j), within);
                }
                within
            };
            if within {
                result[i].push(j);
                result[j].push(i);
            }
        }
    }
    result
}

/// JTS `Geometry.isWithinDistance`.
fn is_within_distance(a: &Geometry<f64>, env_a: &Envelope, b: &Geometry<f64>, env_b: &Envelope, dist: f64) -> bool {
    if envelope_distance(env_a, env_b) > dist {
        return false;
    }
    Euclidean.distance(a, b) <= dist
}

/// Returns the connected components of the graph with `num_items` nodes and edges from `adjacency_list` (indexed by
/// node; may be shorter than `num_items`), each in depth-first visit order, ordered by smallest node.
pub fn extract_connected_components(adjacency_list: &[Vec<usize>], num_items: usize) -> Vec<Vec<usize>> {
    let mut result = Vec::new();
    let mut visited = vec![false; num_items];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..num_items {
        if !visited[i] {
            visited[i] = true;
            let mut group = vec![i];
            // do iterative (not recursive) depth-first search since when merging z13 building in very dense areas like
            // Jakarta recursive calls can overflow the stack
            stack.push(i);
            while let Some(start) = stack.pop() {
                if let Some(adjacent) = adjacency_list.get(start) {
                    for &index in adjacent {
                        if !visited[index] {
                            visited[index] = true;
                            group.push(index);
                            // technically, depth-first search would push onto the stack in reverse order but don't
                            // bother since ordering doesn't matter
                            stack.push(index);
                        }
                    }
                }
            }
            result.push(group);
        }
    }
    result
}

/// Returns a new list of features with points that are more than `buffer` pixels outside the tile boundary removed,
/// assuming a 256x256px tile.
pub fn remove_points_outside_buffer(features: Vec<Feature>, buffer: f64) -> Vec<Feature> {
    if !buffer.is_finite() {
        return features;
    }
    let mut result = Vec::with_capacity(features.len());
    for feature in features {
        if feature.geometry.geom_type() == GeometryType::Point {
            let new_geometry = feature.geometry.filter_points_outside_buffer(buffer);
            if !new_geometry.is_empty() {
                result.push(with_geometry(feature, new_geometry));
            }
        } else {
            result.push(feature);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::readable_tile_archive::ReadableTileArchive;
    use crate::geo::geometry_pipeline;
    use crate::geo::test_utils::*;
    use crate::mbtiles::mbtiles::Mbtiles;
    use crate::stats::stats::InMemoryStats;
    use crate::tags;
    use crate::util::gzip::gunzip;
    use geo::Intersects;
    use std::path::PathBuf;

    fn feature(id: i64, geom: impl Into<Geometry<f64>>, attrs: Tags) -> Feature {
        Feature::new("layer", id, vector_tile::encode_geometry(&geom.into()), attrs)
    }

    fn resource(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name)
    }

    fn line(coords: &[f64]) -> Geometry<f64> {
        new_line_string(coords).into()
    }

    fn mls(lines: Vec<LineString<f64>>) -> Geometry<f64> {
        new_multi_line_string(lines).into()
    }

    fn a1() -> Tags {
        tags! {"a" => 1}
    }

    fn without_geometry(features: &[Feature]) -> Vec<Feature> {
        features.iter().map(|f| f.copy_with_new_geometry(&new_point(0.0, 0.0).into())).collect()
    }

    fn decoded(features: &[Feature]) -> Vec<Geometry<f64>> {
        features.iter().map(|f| f.geometry.decode().unwrap()).collect()
    }

    #[track_caller]
    fn assert_equivalent_features(expected: &[Feature], actual: &[Feature]) {
        for g in decoded(actual) {
            validate_geometry(&g);
        }
        assert_eq!(without_geometry(expected), without_geometry(actual), "comparison without geometries");
        let (e, a) = (decoded(expected), decoded(actual));
        assert_eq!(e.len(), a.len());
        for (e, a) in e.iter().zip(a.iter()) {
            assert_same_normalized_feature(e, a);
        }
    }

    #[track_caller]
    fn assert_topologically_equivalent_features(expected: &[Feature], actual: &[Feature]) {
        for g in decoded(actual) {
            validate_geometry(&g);
        }
        assert_eq!(without_geometry(expected), without_geometry(actual), "comparison without geometries");
        let (e, a) = (decoded(expected), decoded(actual));
        assert_eq!(e.len(), a.len());
        for (e, a) in e.iter().zip(a.iter()) {
            assert_topologically_equivalent_feature(e, a);
        }
    }

    #[test]
    fn merge_merge_zero_line_strings() {
        assert_eq!(Vec::<Feature>::new(), merge_line_strings(vec![], 0.0, 0.0, 0.0));
    }

    #[test]
    fn merge_merge_one_line_strings() {
        assert_eq!(
            vec![feature(1, line(&[10.0, 10.0, 20.0, 20.0]), tags! {})],
            merge_line_strings(vec![feature(1, line(&[10.0, 10.0, 20.0, 20.0]), tags! {})], 0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn dont_merge_disconnected_line_strings() {
        assert_eq!(
            vec![feature(
                0,
                mls(vec![new_line_string(&[10.0, 10.0, 20.0, 20.0]), new_line_string(&[30.0, 30.0, 40.0, 40.0])]),
                tags! {}
            )],
            merge_line_strings(
                vec![
                    feature(1, line(&[10.0, 10.0, 20.0, 20.0]), tags! {}),
                    feature(2, line(&[30.0, 30.0, 40.0, 40.0]), tags! {}),
                ],
                0.0,
                0.0,
                0.0
            )
        );
    }

    #[test]
    fn dont_merge_connected_line_strings_different_attr() {
        let input = vec![
            feature(1, line(&[10.0, 10.0, 20.0, 20.0]), tags! {"a" => 1}),
            feature(2, line(&[20.0, 20.0, 30.0, 30.0]), tags! {"b" => 2}),
        ];
        assert_eq!(input.clone(), merge_line_strings(input, 0.0, 0.0, 0.0));
    }

    #[test]
    fn merge_connected_line_strings_same_attrs() {
        assert_eq!(
            vec![feature(0, line(&[10.0, 10.0, 30.0, 30.0]), a1())],
            merge_line_strings(
                vec![
                    feature(1, line(&[10.0, 10.0, 20.0, 20.0]), a1()),
                    feature(2, line(&[20.0, 20.0, 30.0, 30.0]), a1()),
                ],
                0.0,
                0.0,
                0.0
            )
        );
    }

    #[test]
    fn simplify_line_string_if_tolerance_is_set() {
        let input = || vec![feature(1, line(&[10.0, 10.0, 20.0, 20.0, 30.0, 30.0]), a1())];
        // does not resimplify by default
        assert_eq!(input(), merge_line_strings(input(), 0.0, 1.0, 0.0));
        // but does resimplify when resimplify=true
        assert_eq!(
            vec![feature(1, line(&[10.0, 10.0, 30.0, 30.0]), a1())],
            merge_line_strings_resimplify(input(), 0.0, 1.0, 0.0, true)
        );
        // but doesn't resimplify if the tolerance is negative even when resimplify=true
        assert_eq!(input(), merge_line_strings_resimplify(input(), 0.0, -1.0, 0.0, true));
    }

    #[test]
    fn merge_multi_line_string_test() {
        assert_eq!(
            vec![feature(0, line(&[10.0, 10.0, 40.0, 40.0]), a1())],
            merge_line_strings(
                vec![
                    feature(
                        1,
                        mls(vec![
                            new_line_string(&[10.0, 10.0, 20.0, 20.0]),
                            new_line_string(&[30.0, 30.0, 40.0, 40.0])
                        ]),
                        a1()
                    ),
                    feature(2, line(&[20.0, 20.0, 30.0, 30.0]), a1()),
                ],
                0.0,
                0.0,
                0.0
            )
        );
    }

    #[test]
    fn merge_line_string_ignore_non_line_string() {
        assert_eq!(
            vec![
                feature(3, new_point(5.0, 5.0), a1()),
                feature(4, rectangle(50.0, 60.0), a1()),
                feature(0, line(&[10.0, 10.0, 30.0, 30.0]), a1()),
            ],
            merge_line_strings(
                vec![
                    feature(1, line(&[10.0, 10.0, 20.0, 20.0]), a1()),
                    feature(2, line(&[20.0, 20.0, 30.0, 30.0]), a1()),
                    feature(3, new_point(5.0, 5.0), a1()),
                    feature(4, rectangle(50.0, 60.0), a1()),
                ],
                0.0,
                0.0,
                0.0
            )
        );
    }

    #[test]
    fn merge_line_string_remove_detail_outside_tile() {
        assert_eq!(
            vec![feature(
                0,
                mls(vec![
                    new_line_string(&[
                        10.0, 10.0, -10.0, 20.0, 10.0, 30.0, -10.0, 40.0, -10.0, 50.0, 10.0, 60.0, -10.0, 70.0
                    ]),
                    new_line_string(&[-10.0, 100.0, 10.0, 100.0]),
                ]),
                a1()
            )],
            merge_line_strings(
                vec![
                    // one point goes out - don't clip
                    feature(1, line(&[10.0, 10.0, -10.0, 20.0]), a1()),
                    feature(2, line(&[-10.0, 20.0, 10.0, 30.0]), a1()),
                    feature(3, line(&[10.0, 30.0, -10.0, 40.0]), a1()),
                    // two points goes out - don't clip
                    feature(4, line(&[-10.0, 40.0, -10.0, 50.0]), a1()),
                    feature(5, line(&[-10.0, 50.0, 10.0, 60.0]), a1()),
                    feature(5, line(&[10.0, 60.0, -10.0, 70.0]), a1()),
                    // three points out - do clip
                    feature(6, line(&[-10.0, 70.0, -10.0, 80.0]), a1()),
                    feature(7, line(&[-10.0, 80.0, -11.0, 90.0]), a1()),
                    feature(8, line(&[-10.0, 90.0, -10.0, 100.0]), a1()),
                    feature(9, line(&[-10.0, 100.0, 10.0, 100.0]), a1()),
                ],
                0.0,
                0.0,
                1.0
            )
        );
    }

    #[test]
    fn merge_line_string_min_length() {
        assert_eq!(
            vec![feature(0, line(&[20.0, 20.0, 20.0, 25.0]), a1())],
            merge_line_strings(
                vec![
                    // too short - omit entire feature
                    feature(1, line(&[10.0, 10.0, 10.0, 14.0]), tags! {"b" => 1}),
                    // too short - omit from combined group
                    feature(2, line(&[20.0, 10.0, 20.0, 12.0]), a1()),
                    feature(3, line(&[20.0, 12.0, 20.0, 14.0]), a1()),
                    // just long enough
                    feature(4, line(&[20.0, 20.0, 20.0, 24.0]), a1()),
                    feature(5, line(&[20.0, 24.0, 20.0, 25.0]), a1()),
                ],
                5.0,
                0.0,
                0.0
            )
        );
    }

    // POLYGON MERGE TESTS

    fn rect_xy(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Polygon<f64> {
        rectangle_xy(min_x, min_y, max_x, max_y)
    }

    fn mp(polys: Vec<Polygon<f64>>) -> Geometry<f64> {
        new_multi_polygon(polys).into()
    }

    fn merge_nearby(features: Vec<Feature>, min_area: f64, min_hole: f64, min_dist: f64, buffer: f64) -> Vec<Feature> {
        merge_nearby_polygons(features, min_area, min_hole, min_dist, buffer).unwrap()
    }

    #[test]
    fn merge_polygon_empty_list() {
        assert_equivalent_features(&[], &merge_nearby(vec![], 0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn dont_merge_disconnected_polygons() {
        assert_equivalent_features(
            &[feature(0, mp(vec![rectangle(10.0, 20.0), rect_xy(22.0, 10.0, 30.0, 20.0)]), a1())],
            &merge_nearby(
                vec![feature(1, rectangle(10.0, 20.0), a1()), feature(2, rect_xy(22.0, 10.0, 30.0, 20.0), a1())],
                0.0,
                0.0,
                0.0,
                0.0,
            ),
        );
    }

    #[test]
    fn dont_merge_connected_polygons_with_different_attrs() {
        assert_equivalent_features(
            &[feature(1, rectangle(10.0, 20.0), a1()), feature(2, rect_xy(20.0, 10.0, 30.0, 20.0), tags! {"b" => 1})],
            &merge_nearby(
                vec![
                    feature(1, rectangle(10.0, 20.0), a1()),
                    feature(2, rect_xy(20.0, 10.0, 30.0, 20.0), tags! {"b" => 1}),
                ],
                0.0,
                0.0,
                0.0,
                0.0,
            ),
        );
    }

    #[test]
    fn merge_connected_polygons_with_same_attrs() {
        assert_equivalent_features(
            &[feature(0, rect_xy(10.0, 10.0, 30.0, 20.0), a1())],
            &merge_nearby(
                vec![feature(1, rectangle(10.0, 20.0), a1()), feature(2, rect_xy(20.0, 10.0, 30.0, 20.0), a1())],
                0.0,
                0.0,
                0.0,
                1.0,
            ),
        );
    }

    #[test]
    fn geometry_pipeline_when_merging_overlapping_polygons() {
        let features =
            vec![feature(1, rect_xy(10.0, 10.0, 20.0, 19.0), a1()), feature(2, rect_xy(11.0, 10.0, 20.0, 20.0), a1())];
        let vw = geometry_pipeline::simplify_vw(1.0);
        assert_equivalent_features(
            &[feature(
                0,
                new_polygon(&[
                    10.0, 10.0, 20.0, 10.0, 20.0, 20.0, 11.0, 20.0,
                    // remove this point due to simplification: 11, 19,
                    10.0, 19.0, 10.0, 10.0,
                ]),
                a1(),
            )],
            &merge_nearby_polygons_with(features, 0.0, 0.0, 0.0, 1.0, &InMemoryStats::new(), Some(&vw)).unwrap(),
        );
    }

    #[test]
    fn geometry_pipeline_applied_when_merging_single_polygon() {
        let features = vec![feature(
            1,
            new_polygon(&[10.0, 10.0, 20.0, 10.0, 20.0, 20.0, 11.0, 20.0, 11.0, 19.0, 10.0, 19.0, 10.0, 10.0]),
            a1(),
        )];
        let vw = geometry_pipeline::simplify_vw(1.0);
        assert_equivalent_features(
            &[feature(
                1,
                new_polygon(&[
                    10.0, 10.0, 20.0, 10.0, 20.0, 20.0, 11.0, 20.0,
                    // remove this point due to simplification: 11, 19,
                    10.0, 19.0, 10.0, 10.0,
                ]),
                a1(),
            )],
            &merge_nearby_polygons_with(features, 0.0, 0.0, 0.0, 1.0, &InMemoryStats::new(), Some(&vw)).unwrap(),
        );
    }

    #[test]
    fn merge_multi_polygons_nearby() {
        assert_equivalent_features(
            &[feature(0, rect_xy(10.0, 10.0, 40.0, 20.0), a1())],
            &merge_nearby(
                vec![
                    feature(1, mp(vec![rectangle(10.0, 20.0), rect_xy(30.0, 10.0, 40.0, 20.0)]), a1()),
                    feature(2, rect_xy(15.0, 10.0, 35.0, 20.0), a1()),
                ],
                0.0,
                0.0,
                0.0,
                1.0,
            ),
        );
    }

    #[test]
    fn merge_polygons_ignore_non_polygons() {
        assert_equivalent_features(
            &[feature(2, line(&[20.0, 10.0, 30.0, 20.0]), a1()), feature(1, rectangle(10.0, 20.0), a1())],
            &merge_nearby(
                vec![feature(1, rectangle(10.0, 20.0), a1()), feature(2, line(&[20.0, 10.0, 30.0, 20.0]), a1())],
                0.0,
                0.0,
                0.0,
                0.0,
            ),
        );
    }

    #[test]
    fn merge_polygons_within_min_dist() {
        assert_equivalent_features(
            &[feature(0, rect_xy(10.0, 10.0, 30.0, 20.0), a1())],
            &merge_nearby(
                vec![feature(1, rectangle(10.0, 20.0), a1()), feature(2, rect_xy(20.9, 10.0, 30.0, 20.0), a1())],
                0.0,
                0.0,
                1.0,
                1.0,
            ),
        );
    }

    #[test]
    fn merge_polygons_inside_eachother() {
        assert_equivalent_features(
            &[feature(0, rectangle(10.0, 40.0), a1())],
            &merge_nearby(
                vec![feature(1, rectangle(10.0, 40.0), a1()), feature(2, rectangle(20.0, 30.0), a1())],
                0.0,
                0.0,
                1.0,
                1.0,
            ),
        );
    }

    #[test]
    fn dont_merge_polygons_above_min_dist() {
        assert_equivalent_features(
            &[feature(0, mp(vec![rectangle(10.0, 20.0), rect_xy(21.1, 10.0, 30.0, 20.0)]), a1())],
            &merge_nearby(
                vec![feature(1, rectangle(10.0, 20.0), a1()), feature(2, rect_xy(21.1, 10.0, 30.0, 20.0), a1())],
                0.0,
                0.0,
                1.0,
                1.0,
            ),
        );
    }

    #[test]
    fn remove_polygons_below_min_size() {
        assert_equivalent_features(
            &[],
            &merge_nearby(
                vec![
                    feature(1, rectangle(10.0, 20.0), a1()),
                    feature(2, rect_xy(30.0, 10.0, 36.0, 20.0), a1()),
                    feature(3, rect_xy(35.0, 10.0, 40.0, 20.0), a1()),
                ],
                101.0,
                0.0,
                0.0,
                0.0,
            ),
        );
    }

    #[test]
    fn allow_polygons_above_min_size() {
        assert_equivalent_features(
            &[feature(0, mp(vec![rectangle(10.0, 20.0), rect_xy(30.0, 10.0, 40.0, 20.0)]), a1())],
            &merge_nearby(
                vec![
                    feature(1, rectangle(10.0, 20.0), a1()),
                    feature(2, rect_xy(30.0, 10.0, 36.0, 20.0), a1()),
                    feature(3, rect_xy(35.0, 10.0, 40.0, 20.0), a1()),
                ],
                99.0,
                0.0,
                0.0,
                1.0,
            ),
        );
    }

    fn adjacency_list_from_groups(num: usize, groups: &[&[usize]]) -> Vec<Vec<usize>> {
        let mut result = vec![Vec::new(); num];
        for group in groups {
            for i in 0..group.len() {
                for j in 0..i {
                    let (a, b) = (group[i], group[j]);
                    result[a].push(b);
                    result[b].push(a);
                }
            }
        }
        result
    }

    #[test]
    fn test_extract_connected_components_empty() {
        assert_eq!(Vec::<Vec<usize>>::new(), extract_connected_components(&[], 0));
    }

    #[test]
    fn test_extract_connected_components_one() {
        assert_eq!(vec![vec![0]], extract_connected_components(&[], 1));
    }

    #[test]
    fn test_extract_connected_components_two_disconnected() {
        assert_eq!(vec![vec![0], vec![1]], extract_connected_components(&[], 2));
    }

    #[test]
    fn test_extract_connected_components_two_connected() {
        assert_eq!(vec![vec![0, 1]], extract_connected_components(&adjacency_list_from_groups(2, &[&[0, 1]]), 2));
    }

    #[test]
    fn test_extract_connected_components() {
        assert_eq!(
            vec![vec![0, 1, 2, 3], vec![4], vec![5, 6]],
            extract_connected_components(&adjacency_list_from_groups(7, &[&[0, 1, 2, 3], &[4], &[5, 6]]), 7)
        );
    }

    fn read_tile(file: &str, x: u32, y: u32, z: u32) -> Vec<Feature> {
        let db = Mbtiles::new_read_only_database(&resource(file)).unwrap();
        let tile_data = db.get_tile(x, y, z).unwrap().unwrap();
        vector_tile::decode(&gunzip(&tile_data).unwrap()).unwrap()
    }

    fn merge_many_polygons(file: &str, x: u32, y: u32, z: u32, expected: usize) {
        let features = read_tile(file, x, y, z);
        let merged = merge_nearby(features, 4.0, 0.0, 0.5, 0.5);
        let mut total = 0;
        for feature in &merged {
            let geometry = feature.geometry.decode().unwrap();
            total += match &geometry {
                Geometry::MultiPolygon(mp) => mp.0.len(),
                Geometry::GeometryCollection(gc) => gc.0.len(),
                _ => 1,
            };
            validate_geometry(&geometry);
        }
        // NOTE: buffering is not bit-for-bit identical to JTS so the number of output polygons differs slightly
        let diff = (total as f64 - expected as f64).abs() / expected as f64;
        assert!(diff <= 0.01, "expected ~{expected} polygons (Java), got {total}");
    }

    #[test]
    #[ignore = "slow in debug builds (~20s); run with --release -- --ignored"]
    fn test_merge_many_polygons_boston_1() {
        merge_many_polygons("bostonbuildings.mbtiles", 2477, 3028, 13, 1141);
    }

    #[test]
    #[ignore = "slow in debug builds (~20s); run with --release -- --ignored"]
    fn test_merge_many_polygons_boston_2() {
        merge_many_polygons("bostonbuildings.mbtiles", 2481, 3026, 13, 947);
    }

    #[test]
    #[ignore = "slow in debug builds (~20s); run with --release -- --ignored"]
    fn test_merge_many_polygons_boston_3() {
        merge_many_polygons("bostonbuildings.mbtiles", 2479, 3028, 13, 1073);
    }

    #[test]
    #[ignore = "slow in debug builds (~20s); run with --release -- --ignored"]
    fn test_merge_many_polygons_jakarta() {
        merge_many_polygons("jakartabuildings.mbtiles", 6527, 4240, 13, 410);
    }

    #[test]
    fn test_protomaps538_lake_merge() {
        // see https://github.com/protomaps/basemaps/issues/538
        let features = read_tile("protomaps538lakemerge.mbtiles", 35, 46, 7);
        let area_before: f64 = features.iter().map(|f| f.geometry.decode().unwrap().unsigned_area()).sum();
        let merged = merge_nearby_polygons_with(features, 1.0, 1.0, 0.5, 0.0625, &InMemoryStats::new(), None).unwrap();
        let mut area_after = 0.0;
        for feature in &merged {
            let geometry = feature.geometry.decode().unwrap();
            area_after += geometry.unsigned_area();
            validate_geometry(&geometry);
        }
        let ratio = area_after / area_before;
        assert!(ratio > 0.99 && ratio < 1.01, "Area changed too much {area_before} -> {area_after}");
    }

    fn read_wkb_collection(path: &str) -> Vec<Geometry<f64>> {
        let bytes = std::fs::read(resource(path)).unwrap();
        match geo_utils::read_wkb(&bytes).unwrap() {
            Geometry::GeometryCollection(gc) => gc.0,
            Geometry::MultiPolygon(mp) => mp.0.into_iter().map(Geometry::Polygon).collect(),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn test_protomaps538_lake_merge_wkb() {
        let mut files: Vec<String> = Vec::new();
        for dir in ["protomaps538/buffer_union_unbuffer_05", "protomaps538/buffer_union_unbuffer_01"] {
            for entry in std::fs::read_dir(resource(dir)).unwrap() {
                files.push(format!("{dir}/{}", entry.unwrap().file_name().to_string_lossy()));
            }
        }
        files.sort();
        assert_eq!(38, files.len());
        let mut failures = Vec::new();
        for file in files {
            let geoms = read_wkb_collection(&file);
            let collection = geo_utils::create_geometry_collection(geoms.clone());
            let area_before = polygon_overlay::buffer_zero(&collection)
                .unwrap()
                .unsigned_area()
                .max(polygon_overlay::union_all(&collection).unwrap().unsigned_area());
            let buffer = if file.contains("unbuffer_01") { 0.1 } else { 0.5 };
            let stats = InMemoryStats::new();
            let merged = buffer_union_unbuffer(buffer, &geoms, &stats).unwrap();
            let merged = reverse(geo_utils::snap_and_fix_polygon(&merged, &stats, "merge").unwrap());
            assert!(is_polygonal(&merged), "{file}");
            let area_after = merged.unsigned_area();
            validate_geometry(&merged);
            let ratio = area_after / area_before;
            // NOTE: Java requires the ratio to be < 1.01, but for 3 of these inputs JTS's negative buffer collapses
            // (locationtech/jts#1183) so Java falls back to unioning the unbuffered input polygons. The Rust
            // buffer/union/unbuffer succeeds, which fills sub-pixel gaps between the input polygons (up to ~3% area).
            if !(ratio > 0.99 && ratio < 1.03) {
                failures.push(format!(
                    "{file}: Area changed too much {area_before} -> {area_after} {:?}",
                    stats.data_errors()
                ));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn merge_multi_polygon_with_hole() {
        let mut inner_ring = rectangle_coord_list(12.0, 18.0);
        inner_ring.reverse();
        assert_topologically_equivalent_features(
            &[feature(0, new_polygon_with_holes(rectangle_coord_list(10.0, 22.0), vec![inner_ring.clone()]), a1())],
            &merge_nearby(
                vec![
                    feature(1, new_polygon_with_holes(rectangle_coord_list(10.0, 20.0), vec![inner_ring]), a1()),
                    feature(1, rect_xy(20.0, 10.0, 22.0, 22.0), a1()),
                    feature(1, rect_xy(10.0, 20.0, 22.0, 22.0), a1()),
                ],
                0.0,
                0.0,
                0.0,
                0.0,
            ),
        );
    }

    #[test]
    fn merge_multi_polygon_exclude_small_inner_rings() {
        let mut inner_ring = rectangle_coord_list(12.0, 12.99);
        inner_ring.reverse();
        assert_topologically_equivalent_features(
            &[feature(0, rectangle(10.0, 22.0), a1())],
            &merge_nearby(
                vec![
                    feature(1, new_polygon_with_holes(rectangle_coord_list(10.0, 20.0), vec![inner_ring]), a1()),
                    feature(1, rect_xy(20.0, 10.0, 22.0, 22.0), a1()),
                    feature(1, rect_xy(10.0, 20.0, 22.0, 22.0), a1()),
                ],
                1.0,
                1.0,
                0.0,
                0.0,
            ),
        );
    }

    fn test_multigeometry_merger(
        generate: impl Fn(f64) -> Geometry<f64>,
        combine: impl Fn(Vec<Geometry<f64>>) -> Geometry<f64>,
        other_geometry: Geometry<f64>,
        merge: impl Fn(Vec<Feature>) -> Vec<Feature>,
    ) {
        let [g1, g2, g3, g4, g5] = [1.0, 2.0, 3.0, 4.0, 5.0].map(&generate);
        assert_topologically_equivalent_features(&[], &merge(vec![]));
        assert_topologically_equivalent_features(
            &[feature(1, g1.clone(), a1())],
            &merge(vec![feature(1, g1.clone(), a1())]),
        );
        let geom_type = GeometryType::type_of(&g1);
        assert_topologically_equivalent_features(
            &[
                feature(4, other_geometry.clone(), a1()),
                feature(0, combine(vec![g1.clone(), g2.clone(), g3.clone(), g4.clone()]), a1()),
                feature(3, g5.clone(), tags! {"a" => 2}),
            ],
            &merge(vec![
                feature(1, combine(vec![g1, g2]), a1()),
                feature(2, combine(vec![g3, g4]), a1()),
                feature(3, g5, tags! {"a" => 2}),
                feature(4, other_geometry, a1()),
                Feature::new("layer", 5, VectorGeometry::new(Vec::new(), geom_type, 0), a1()),
            ]),
        );
    }

    #[test]
    fn merge_multipoints() {
        test_multigeometry_merger(
            |i| new_point(i, 2.0 * i).into(),
            |items| {
                new_multi_point(
                    items
                        .into_iter()
                        .map(|g| match g {
                            Geometry::Point(p) => p,
                            _ => unreachable!(),
                        })
                        .collect(),
                )
                .into()
            },
            rectangle(0.0, 1.0).into(),
            merge_multi_point,
        );
    }

    #[test]
    fn merge_multipolygons() {
        test_multigeometry_merger(
            |i| rectangle(i, i + 1.0).into(),
            |items| {
                mp(items
                    .into_iter()
                    .map(|g| match g {
                        Geometry::Polygon(p) => p,
                        _ => unreachable!(),
                    })
                    .collect())
            },
            new_point(0.0, 0.0).into(),
            merge_multi_polygon,
        );
    }

    #[test]
    fn merge_multiline() {
        test_multigeometry_merger(
            |i| line(&[i, i + 1.0, i + 2.0, i + 3.0]),
            |items| {
                mls(items
                    .into_iter()
                    .map(|g| match g {
                        Geometry::LineString(l) => l,
                        _ => unreachable!(),
                    })
                    .collect())
            },
            new_point(0.0, 0.0).into(),
            merge_multi_line_string,
        );
    }

    #[test]
    fn remove_points_outside_buffer_empty() {
        assert_eq!(Vec::<Feature>::new(), remove_points_outside_buffer(vec![], 4.0));
    }

    fn pts(coords: &[(f64, f64)]) -> Vec<Feature> {
        coords.iter().map(|&(x, y)| feature(1, new_point(x, y), tags! {})).collect()
    }

    const INSIDE: [(f64, f64); 6] =
        [(0.0, 0.0), (256.0, 256.0), (-4.0, -4.0), (-4.0, 260.0), (260.0, -4.0), (260.0, 260.0)];
    const OUTSIDE: [(f64, f64); 4] = [(-5.0, -5.0), (-5.0, 261.0), (261.0, -5.0), (261.0, 261.0)];

    #[test]
    fn remove_points_outside_buffer_single_points() {
        let mut input = pts(&INSIDE);
        input.extend(pts(&OUTSIDE));
        assert_topologically_equivalent_features(&pts(&INSIDE), &remove_points_outside_buffer(input, 4.0));
    }

    #[test]
    fn remove_points_outside_buffer_multi_points() {
        let multi = |coords: &[(f64, f64)]| -> Geometry<f64> {
            new_multi_point(coords.iter().map(|&(x, y)| new_point(x, y)).collect()).into()
        };
        let all: Vec<(f64, f64)> = INSIDE.iter().chain(OUTSIDE.iter()).copied().collect();
        assert_topologically_equivalent_features(
            &[feature(1, multi(&INSIDE), tags! {})],
            &remove_points_outside_buffer(
                vec![feature(1, multi(&all), tags! {}), feature(1, multi(&OUTSIDE), tags! {})],
                4.0,
            ),
        );
    }

    #[test]
    fn test_issue700_buffer_union_unbuffer_failure() {
        for i in 1..=9 {
            let geometries = read_wkb_collection(&format!("issue_700/exception_{i}.wkb"));
            buffer_union_unbuffer(0.5, &geometries, &InMemoryStats::new()).unwrap();
        }
    }

    #[test]
    fn merge_fill_polygons_normalizes() {
        let result = merge_nearby(
            vec![
                feature(1, rect_xy(-2.0, -2.0, 200.0, 258.0), tags! {}),
                feature(2, rect_xy(180.0, -2.0, 258.0, 258.0), tags! {}),
            ],
            0.0,
            0.0,
            0.0,
            0.0,
        );
        assert_eq!(vec![Geometry::Polygon(rectangle(-2.0, 258.0))], decoded(&result));
    }

    #[test]
    fn merge_normalize_outer_ring() {
        let result = merge_nearby(
            vec![
                feature(1, rect_xy(-2.0, -2.0, 10.0, 258.0), tags! {}),
                feature(1, rect_xy(-2.0, -2.0, 258.0, 10.0), tags! {}),
                feature(1, rect_xy(246.0, -2.0, 258.0, 258.0), tags! {}),
                feature(1, rect_xy(-2.0, 246.0, 258.0, 258.0), tags! {}),
            ],
            0.0,
            0.0,
            0.0,
            0.0,
        );
        let Geometry::Polygon(poly) = result[0].geometry.decode().unwrap() else { panic!("not a polygon") };
        assert_eq!(rectangle(-2.0, 258.0).exterior(), poly.exterior());
        assert_eq!(1, poly.interiors().len());
        let mut expected_hole = rectangle(10.0, 246.0).exterior().clone();
        expected_hole.0.reverse();
        assert_topologically_equivalent_feature(&expected_hole.into(), &poly.interiors()[0].clone().into());
    }

    #[test]
    fn merge_fill_polygons_does_not_normalize_irregular_fill() {
        assert_equivalent_features(
            &[feature(
                0,
                new_polygon(&[
                    -2.0, -2.0, 200.0, -2.0, 200.0, -1.0, 258.0, -1.0, 258.0, 257.0, 200.0, 257.0, 200.0, 258.0, -2.0,
                    258.0, -2.0, -2.0,
                ]),
                tags! {},
            )],
            &merge_nearby(
                vec![
                    feature(1, rect_xy(-2.0, -2.0, 200.0, 258.0), tags! {}),
                    feature(2, rect_xy(180.0, -1.0, 258.0, 257.0), tags! {}),
                ],
                0.0,
                0.0,
                0.0,
                0.0,
            ),
        );
    }

    #[test]
    fn merge_line_string_zero_min_length() {
        for (min_length, min_tolerance, buffer) in
            [(0.0, 0.0, 0.0), (0.0, -1.0, 0.0), (0.0, -1.0, -1.0), (0.0, 0.0, -1.0)]
        {
            let input = feature(1, line(&[10.0, 10.0, 10.25, 10.0, 20.0, 10.0]), tags! {});
            let actual = merge_line_strings(vec![input.clone()], min_length, min_tolerance, buffer);
            assert_eq!(input.geometry.decode().unwrap(), actual[0].geometry.decode().unwrap());
            assert_eq!(vec![input], actual);
        }
    }

    #[test]
    fn merged_line_string_id_ends_in_zero() {
        let result = merge_line_strings(
            vec![
                feature(123, line(&[10.0, 10.0, 20.0, 20.0]), a1()),
                feature(456, line(&[20.0, 20.0, 30.0, 30.0]), a1()),
            ],
            0.0,
            0.0,
            0.0,
        );
        assert_eq!(1, result.len());
        assert_eq!(120, result[0].id, "merged feature ID should end in 0");
    }

    #[test]
    fn merged_multi_point_id_ends_in_zero() {
        let result = merge_multi_point(vec![
            feature(123, new_point(10.0, 10.0), a1()),
            feature(456, new_point(20.0, 20.0), a1()),
        ]);
        assert_eq!(1, result.len());
        assert_eq!(120, result[0].id, "merged feature ID should end in 0");
    }

    #[test]
    fn merged_multi_polygon_id_ends_in_zero() {
        let result = merge_multi_polygon(vec![
            feature(123, rectangle(10.0, 20.0), a1()),
            feature(456, rectangle(30.0, 40.0), a1()),
        ]);
        assert_eq!(1, result.len());
        assert_eq!(120, result[0].id, "merged feature ID should end in 0");
    }

    #[test]
    fn merged_multi_line_string_id_ends_in_zero() {
        let result = merge_multi_line_string(vec![
            feature(123, line(&[10.0, 10.0, 20.0, 20.0]), a1()),
            feature(456, line(&[30.0, 30.0, 40.0, 40.0]), a1()),
        ]);
        assert_eq!(1, result.len());
        assert_eq!(120, result[0].id, "merged feature ID should end in 0");
    }

    #[test]
    fn merged_nearby_polygons_id_ends_in_zero() {
        let result = merge_nearby(
            vec![feature(123, rectangle(10.0, 20.0), a1()), feature(456, rect_xy(20.0, 10.0, 30.0, 20.0), a1())],
            0.0,
            0.0,
            0.0,
            1.0,
        );
        assert_eq!(1, result.len());
        assert_eq!(120, result[0].id, "merged feature ID should end in 0");
    }

    #[test]
    fn single_feature_id_unchanged() {
        let result = merge_multi_point(vec![feature(123, new_point(10.0, 10.0), a1())]);
        assert_eq!(1, result.len());
        assert_eq!(123, result[0].id, "single feature ID should not be modified");
    }

    #[test]
    fn test_issue_adriatic_ocean() {
        // see https://github.com/onthegomap/planetiler/issues/1622
        let bytes = std::fs::read(resource("issue_1622_8_138_92_tiled_polygons.wkb")).unwrap();
        let Geometry::MultiPolygon(geom) = geo_utils::read_wkb(&bytes).unwrap() else { panic!("not a multipolygon") };
        let should_be_island = new_point(2950.0 * 256.0 / 4096.0, 2750.0 * 256.0 / 4096.0);
        let mut features = Vec::new();
        for (i, poly) in geom.0.iter().enumerate() {
            let feature = Feature::new(
                "water",
                i as i64,
                vector_tile::encode_geometry(&Geometry::Polygon(poly.clone())),
                tags! {},
            );
            features.push(feature.clone());
            assert!(!poly.intersects(&should_be_island));
            for item in merge_overlapping_polygons(vec![feature], 1.0).unwrap() {
                assert!(!item.geometry.decode().unwrap().intersects(&should_be_island));
            }
        }
        for feature in merge_overlapping_polygons(features, 1.0).unwrap() {
            let decoded = feature.geometry.decode().unwrap();
            assert!(crate::geo::is_valid::is_valid(&decoded));
            assert!(!decoded.intersects(&should_be_island));
        }
    }

    fn rotated(poly: &Polygon<f64>, degrees: f64, origin: (f64, f64)) -> Polygon<f64> {
        let (sin, cos) = degrees.to_radians().sin_cos();
        let rotate =
            |c: &Coord<f64>| Coord { x: origin.0 + c.x * cos - c.y * sin, y: origin.1 + c.x * sin + c.y * cos };
        Polygon::new(LineString(poly.exterior().0.iter().map(rotate).collect()), vec![])
    }

    fn merged_exterior(polys: &[Polygon<f64>]) -> Polygon<f64> {
        let features = polys.iter().enumerate().map(|(i, p)| feature(i as i64 + 1, p.clone(), a1())).collect();
        let merged = merge_nearby(features, 0.0, 0.0, 0.5, 0.5);
        assert_eq!(1, merged.len());
        let Geometry::Polygon(poly) = merged[0].geometry.decode().unwrap() else { panic!("not a polygon") };
        poly
    }

    /// geo's mitre buffer emits 3 vertices per outside corner where JTS emits 1; after buffer/union/unbuffer the 2
    /// extra (near-collinear) points survive on every concave corner of non-axis-aligned shapes unless they are
    /// removed (docs/analysis-merge-vertices.md). Expected output is Java's
    /// `FeatureMerge.mergeNearbyPolygons(features, 0, 0, 0.5, 0.5)` on the same input (some keep a vertex or two
    /// where rounding the input to the tile grid left the bridged edges slightly non-collinear). Before the fix the
    /// 30° L had 11 points instead of 8.
    #[test]
    fn merge_rotated_polygons_match_java() {
        /// (rotation, L point count, L from Java, rectangles point count, rectangles from Java)
        type JavaCase = (f64, usize, &'static [f64], usize, &'static [f64]);
        #[rustfmt::skip]
        let cases: [JavaCase; 5] = [
            (0.0,
             7, &[100.0, 100.0, 120.0, 100.0, 120.0, 108.0, 108.0, 108.0, 108.0, 120.0, 100.0, 120.0, 100.0, 100.0],
             5, &[100.0, 100.0, 130.0, 100.0, 130.0, 120.0, 100.0, 120.0, 100.0, 100.0]),
            (30.0,
             8, &[95.875, 107.125, 100.0, 100.0, 117.3125, 110.0, 113.3125, 116.9375, 102.9375, 110.9375, 96.9375,
                  121.3125, 90.0, 117.3125, 95.875, 107.125],
             7, &[94.875, 108.875, 100.0, 100.0, 126.0, 115.0, 121.0, 123.6875, 116.0, 132.3125, 90.0, 117.3125,
                  94.875, 108.875]),
            (17.0,
             9, &[97.5625, 107.875, 97.625, 107.875, 100.0, 100.0, 119.125, 105.875, 116.8125, 113.5, 105.3125, 110.0,
                  101.8125, 121.4375, 94.125, 119.125, 97.5625, 107.875],
             7, &[97.0625, 109.5625, 100.0, 100.0, 128.6875, 108.75, 125.6875, 118.5625, 122.8125, 127.875, 94.125,
                  119.125, 97.0625, 109.5625]),
            (45.0,
             7, &[100.0, 100.0, 114.125, 114.125, 108.5, 119.8125, 100.0, 111.3125, 91.5, 119.8125, 85.875, 114.125,
                  100.0, 100.0],
             7, &[100.0, 100.0, 121.1875, 121.1875, 114.125, 128.3125, 114.0625, 128.25, 107.0625, 135.375, 85.875,
                  114.125, 100.0, 100.0]),
            (72.0,
             8, &[92.125, 102.5625, 100.0, 100.0, 106.1875, 119.0, 98.5625, 121.5, 94.875, 110.125, 83.4375, 113.8125,
                  81.0, 106.1875, 92.125, 102.5625],
             8, &[90.5, 103.0625, 100.0, 100.0, 109.25, 128.5625, 99.75, 131.625, 90.25, 134.6875, 81.0, 106.1875,
                  90.5, 103.125, 90.5, 103.0625]),
        ];
        for (degrees, l_points, l_java, r_points, r_java) in cases {
            let rot = |p: Polygon<f64>| rotated(&p, degrees, (100.0, 100.0));
            // two rectangles with a small gap forming an L
            let l = merged_exterior(&[rot(rect_xy(0.0, 0.0, 20.0, 8.0)), rot(rect_xy(0.0, 8.25, 8.0, 20.0))]);
            assert_eq!(l_points, l.exterior().0.len(), "L rotated {degrees}: {l:?}");
            assert_same_normalized_feature(&new_polygon(l_java).into(), &l.into());
            // two rectangles with a small gap along a shared edge
            let r = merged_exterior(&[rot(rect_xy(0.0, 0.0, 30.0, 10.0)), rot(rect_xy(0.0, 10.25, 30.0, 20.0))]);
            assert_eq!(r_points, r.exterior().0.len(), "rectangles rotated {degrees}: {r:?}");
            assert_same_normalized_feature(&new_polygon(r_java).into(), &r.into());
        }
    }

    #[test]
    fn remove_near_collinear_ring_keeps_corners_and_spikes() {
        let ring = |c: &[f64]| LineString(c.chunks(2).map(|p| Coord { x: p[0], y: p[1] }).collect::<Vec<_>>());
        // a point 1e-9 off the bottom edge, and one exactly on the closing edge (the ring's start)
        let input = ring(&[5.0, 0.0, 10.0, 1e-9, 20.0, 0.0, 20.0, 10.0, 0.0, 10.0, 0.0, 0.0, 5.0, 0.0]);
        let out = remove_near_collinear_ring(input, 1e-6).unwrap();
        assert_eq!(ring(&[20.0, 0.0, 20.0, 10.0, 0.0, 10.0, 0.0, 0.0, 20.0, 0.0]), out);
        // spikes double back so they are kept
        let spike = ring(&[0.0, 0.0, 10.0, 0.0, 5.0, 0.0, 5.0, 5.0, 0.0, 0.0]);
        assert_eq!(spike.clone(), remove_near_collinear_ring(spike, 1e-6).unwrap());
        // a very shallow corner emitted as (offset end, mitre point, offset end) keeps exactly one vertex
        let input = ring(&[0.0, 0.0, 49.999, 0.0, 50.0, 0.0, 50.001, 0.00001, 100.0, 0.5, 100.0, 50.0, 0.0, 50.0]);
        let out = remove_near_collinear_ring(input, 1e-5).unwrap();
        assert_eq!(6, out.0.len(), "{out:?}");
        assert!(out.0.iter().any(|c| (c.x - 50.0).abs() < 0.002 && c.y.abs() < 0.001), "{out:?}");
        // degenerate rings collapse
        assert_eq!(None, remove_near_collinear_ring(ring(&[0.0, 0.0, 1.0, 0.0, 2.0, 0.0, 0.0, 0.0]), 1e-6));
    }
}
