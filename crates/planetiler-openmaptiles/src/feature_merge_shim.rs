// TODO(port): temporary copy of planetiler_core::feature_merge (ported concurrently on another branch, same API).
// Once it is merged into planetiler-core, delete this file and replace `mod feature_merge` in lib.rs with
// `pub(crate) use planetiler_core::feature_merge;`.
#![allow(dead_code)]
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

use planetiler_core::geo::envelope::Envelope;
use planetiler_core::geo::geo_utils;
use planetiler_core::geo::geometry_exception::GeometryResult;
use planetiler_core::geo::geometry_pipeline::{GeometryPipeline, GeometryPipelineRef};
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::geo::polygon_overlay;
use planetiler_core::stats::default_stats;
use planetiler_core::stats::stats::Stats;
use planetiler_core::util::loop_line_merger::LoopLineMerger;
use planetiler_core::value::Tags;
use planetiler_core::vector_tile::{self, ALL, Feature, VectorGeometry};

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
                    let owned: Vec<Geometry<f64>> = polygon_group.iter().map(|g| (*g).clone()).collect();
                    buffer_union_unbuffer(buffer, &owned, stats)?
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
pub fn buffer_union_unbuffer(
    buffer: f64,
    polygon_group: &[Geometry<f64>],
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
                return polygon_overlay::union_all(&geo_utils::create_geometry_collection(polygon_group.to_vec()));
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
    let result = if polys.len() == 1 {
        polys.pop().unwrap().buffer_with_style(style)
    } else {
        MultiPolygon(polys).buffer_with_style(style)
    };
    Ok(polygon_overlay::to_polygonal(result.0))
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
    for (i, a) in geometries.iter().enumerate() {
        let env_a = &envelopes[i];
        if env_a.is_null() {
            continue;
        }
        let query = AABB::from_corners([env_a.min_x, env_a.min_y], [env_a.max_x, env_a.max_y]);
        for item in index.locate_in_envelope_intersecting(query) {
            let j = item.data;
            if is_within_distance(a, env_a, &geometries[j], &envelopes[j], min_dist) {
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
