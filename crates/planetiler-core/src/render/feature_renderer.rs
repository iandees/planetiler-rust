//! Port of `render/FeatureRenderer.java`: converts source feature geometries to encoded vector tile features
//! according to settings configured in the map profile (like zoom range, min pixel size, output attributes and their
//! zoom ranges).
//!
//! Unlike Java, the consumer of rendered features is passed to each [`FeatureRenderer::accept`] call instead of being
//! stored in the renderer, so a single renderer can be shared by all threads:
//!
//! ```ignore
//! let renderer = FeatureRenderer::new(config.clone(), stats.clone());
//! let mut write = feature_group.feature_writer()?; // per thread
//! for feature in collector.iter() {
//!     renderer.accept(feature, &mut |rendered| write(&rendered));
//! }
//! ```

use std::sync::Arc;

use geo::{CoordsIter, HasDimensions, MapCoords};
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use tracing::warn;

use crate::config::planetiler_config::PlanetilerConfig;
use crate::feature_collector::Feature;
use crate::geo::douglas_peucker_simplifier;
use crate::geo::geo_utils;
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::geometry_pipeline::GeometryPipeline;
use crate::geo::simplify_method::SimplifyMethod;
use crate::geo::tile_coord::TileCoord;
use crate::geo::tile_extents::TileExtents;
use crate::geo::vw_simplifier::VWSimplifier;
use crate::render::geometry_coordinate_sequences as gcs;
use crate::render::rendered_feature::{Group, RenderedFeature};
use crate::render::tiled_geometry::TiledGeometry;
use crate::stats::stats::StatsRef;
use crate::value::{Tags, Value};
use crate::vector_tile;

/// Converts shared attributes into whatever type `vector_tile::Feature::tags` uses (`Tags` or `Arc<Tags>`).
trait TagsFromArc {
    fn from_arc(tags: &Arc<Tags>) -> Self;
}

impl TagsFromArc for Tags {
    fn from_arc(tags: &Arc<Tags>) -> Self {
        (**tags).clone()
    }
}

impl TagsFromArc for Arc<Tags> {
    fn from_arc(tags: &Arc<Tags>) -> Self {
        tags.clone()
    }
}

fn new_vector_tile_feature(
    layer: &str,
    id: i64,
    geometry: vector_tile::VectorGeometry,
    attrs: &Arc<Tags>,
    group: i64,
) -> vector_tile::Feature {
    vector_tile::Feature { layer: layer.to_string(), id, geometry, tags: TagsFromArc::from_arc(attrs), group }
}

/// Renders [`Feature`]s into [`RenderedFeature`]s sliced into tiles.
pub struct FeatureRenderer {
    config: Arc<PlanetilerConfig>,
    stats: StatsRef,
    extents: Arc<TileExtents>,
}

fn is_polygonal(geom: &Geometry<f64>) -> bool {
    matches!(geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_) | Geometry::Rect(_) | Geometry::Triangle(_))
}

fn is_puntal(geom: &Geometry<f64>) -> bool {
    matches!(geom, Geometry::Point(_) | Geometry::MultiPoint(_))
}

/// Reverses the orientation of every ring (JTS `Geometry.reverse()` for polygons).
fn reverse_polygonal(geom: Geometry<f64>) -> Geometry<f64> {
    fn reverse_ring(ring: &mut LineString<f64>) {
        ring.0.reverse();
    }
    fn reverse_polygon(poly: Polygon<f64>) -> Polygon<f64> {
        let (mut exterior, mut interiors) = poly.into_inner();
        reverse_ring(&mut exterior);
        interiors.iter_mut().for_each(reverse_ring);
        Polygon::new(exterior, interiors)
    }
    match geom {
        Geometry::Polygon(p) => Geometry::Polygon(reverse_polygon(p)),
        Geometry::MultiPolygon(mp) => {
            Geometry::MultiPolygon(MultiPolygon(mp.0.into_iter().map(reverse_polygon).collect()))
        }
        Geometry::GeometryCollection(gc) => Geometry::GeometryCollection(geo_types::GeometryCollection(
            gc.0.into_iter().map(reverse_polygonal).collect(),
        )),
        other => other,
    }
}

/// Applies the default simplification for `feature` at `zoom` (Java `GeometryPipeline.defaultSimplify`), without
/// allocating a pipeline object.
fn default_simplify(feature: &Feature<'_>, zoom: u32, geom: &Geometry<f64>) -> Geometry<f64> {
    let tolerance = feature.get_pixel_tolerance_at_zoom(zoom) / 256.0;
    match feature.get_simplify_method_at_zoom(zoom) {
        SimplifyMethod::RetainImportantPoints => douglas_peucker_simplifier::simplify(geom, tolerance),
        // DP tolerance is displacement, and VW tolerance is area, so square what the user entered to convert from
        // DP to VW tolerance
        SimplifyMethod::RetainEffectiveAreas => VWSimplifier::new().set_tolerance(tolerance * tolerance).apply(geom),
        SimplifyMethod::RetainWeightedEffectiveAreas => {
            VWSimplifier::new().set_weight(0.7).set_tolerance(tolerance * tolerance).apply(geom)
        }
    }
}

impl FeatureRenderer {
    /// Constructs a new feature renderer for `config`.
    pub fn new(config: Arc<PlanetilerConfig>, stats: StatsRef) -> FeatureRenderer {
        let extents = config.bounds.clone().tile_extents();
        FeatureRenderer { config, stats, extents }
    }

    /// Renders `feature` at every zoom level it appears in and sends each tile feature to `consumer`.
    pub fn accept(&self, feature: &Feature<'_>, consumer: &mut impl FnMut(RenderedFeature)) {
        self.accept_dyn(feature, consumer)
    }

    fn accept_dyn(&self, feature: &Feature<'_>, consumer: &mut dyn FnMut(RenderedFeature)) {
        let geometry = feature.get_geometry();
        let simple_line_length = match geometry {
            Geometry::LineString(_) | Geometry::Line(_) => geo_utils::length(geometry),
            Geometry::MultiLineString(mls) if mls.0.len() == 1 => geo_utils::length(geometry),
            _ => -1.0,
        };
        if geometry.is_empty() {
            warn!("Empty geometry {:?}", feature);
            return;
        }
        // geometries are filtered by min size after processing before they are emitted, but do cheap pre-filtering
        // here to avoid processing features that won't emit anything
        let puntal = is_puntal(geometry);
        for zoom in (feature.get_min_zoom()..=feature.get_max_zoom()).rev() {
            let scale = (1u64 << zoom) as f64;
            let min_size = feature.get_min_pixel_size_at_zoom(zoom);
            if feature.has_linear_ranges() {
                let length = simple_line_length * scale * 256.0;
                for range in feature.get_linear_ranges_at_zoom(zoom) {
                    if min_size > 0.0 && length * (range.end - range.start) > min_size {
                        self.accept_at_zoom(zoom, &range.geom, &range.attrs, feature, consumer);
                    }
                }
            } else {
                if min_size > 0.0 {
                    if puntal {
                        if feature.get_source_feature_pixel_size_at_zoom(zoom) < min_size {
                            // don't emit points if the line or polygon feature it came from was too small
                            continue;
                        }
                    } else if simple_line_length >= 0.0 && simple_line_length * scale * 256.0 < min_size {
                        // skip processing lines that are too short
                        continue;
                    }
                }
                self.accept_at_zoom(zoom, geometry, feature.get_attrs_at_zoom(zoom), feature, consumer);
            }
        }
    }

    fn accept_at_zoom(
        &self,
        zoom: u32,
        geom: &Geometry<f64>,
        attrs: &Arc<Tags>,
        feature: &Feature<'_>,
        consumer: &mut dyn FnMut(RenderedFeature),
    ) {
        let scale = (1u64 << zoom) as f64;
        let scaled = geom.map_coords(|c| Coord { x: c.x * scale, y: c.y * scale });
        let input_polygonal = is_polygonal(&scaled);
        let geom = if let Some(pipeline) = feature.get_scaled_geometry_transform_at_zoom(zoom) {
            pipeline.apply(&scaled)
        } else if !is_puntal(&scaled) {
            default_simplify(feature, zoom, &scaled)
        } else {
            scaled
        };
        // When simplification collapses a polygon's shell, JTS's GeometryTransformer returns the surviving rings as
        // `LinearRing`s, which Java renders as polygons filtered by the (unsquared) line min size. geo has no
        // LinearRing type, so remember that closed linestrings here are rings (see `gcs::extract_groups_with_rings`).
        let linear_rings = input_polygonal && !is_polygonal(&geom);
        self.render_geometry(zoom, geom, attrs, feature, linear_rings, consumer);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_geometry(
        &self,
        zoom: u32,
        geom: Geometry<f64>,
        attrs: &Arc<Tags>,
        feature: &Feature<'_>,
        linear_rings: bool,
        consumer: &mut dyn FnMut(RenderedFeature),
    ) {
        if geom.is_empty() {
            // skip this feature
            return;
        }
        match geom {
            Geometry::Point(point) => self.render_point(zoom, attrs, feature, &[point.0], consumer),
            Geometry::MultiPoint(points) => {
                // Attempt to encode multipoints as a single feature sharing attributes and sort-key but if it has
                // label grid data then need to fall back to separate features per point, so they can be filtered
                // individually.
                let coords: Vec<Coord<f64>> = points.0.iter().map(|p| p.0).collect();
                if feature.has_label_grid() {
                    for coord in coords {
                        self.render_point(zoom, attrs, feature, &[coord], consumer);
                    }
                } else {
                    self.render_point(zoom, attrs, feature, &coords, consumer);
                }
            }
            g @ (Geometry::Polygon(_)
            | Geometry::MultiPolygon(_)
            | Geometry::LineString(_)
            | Geometry::MultiLineString(_)) => {
                self.render_line_or_polygon(zoom, attrs, feature, g, linear_rings, consumer)
            }
            Geometry::GeometryCollection(collection) => {
                for g in collection.0 {
                    self.render_geometry(zoom, g, attrs, feature, linear_rings, consumer);
                }
            }
            Geometry::Line(line) => {
                self.render_line_or_polygon(zoom, attrs, feature, Geometry::LineString(line.into()), false, consumer)
            }
            Geometry::Rect(rect) => {
                self.render_line_or_polygon(zoom, attrs, feature, Geometry::Polygon(rect.to_polygon()), false, consumer)
            }
            Geometry::Triangle(triangle) => {
                let polygon = Geometry::Polygon(triangle.to_polygon());
                self.render_line_or_polygon(zoom, attrs, feature, polygon, false, consumer)
            }
        }
    }

    fn render_point(
        &self,
        zoom: u32,
        attrs: &Arc<Tags>,
        feature: &Feature<'_>,
        coords: &[Coord<f64>],
        consumer: &mut dyn FnMut(RenderedFeature),
    ) {
        let has_label_grid = feature.has_label_grid();
        let buffer = feature.get_buffer_pixels_at_zoom(zoom) / 256.0;
        let tiles_at_zoom = 1i32 << zoom;

        // for "label grid" point density limiting, compute the grid square that this point sits in
        // only valid if not a multipoint
        let mut group_info = None;
        if has_label_grid && coords.len() == 1 {
            let label_grid_tile_size = feature.get_point_label_grid_pixel_size_at_zoom(zoom) / 256.0;
            if label_grid_tile_size >= 1.0 / 4096.0 {
                group_info = Some(Group {
                    group: geo_utils::label_grid_id(tiles_at_zoom, label_grid_tile_size, coords[0]),
                    limit: feature.get_point_label_grid_limit_at_zoom(zoom),
                });
            }
        }

        // compute the tile coordinate of every tile these points should show up in at the given buffer size
        let extents = self.extents.get_for_zoom(zoom);
        let tiled = TiledGeometry::slice_points_into_tiles(extents, buffer, zoom, coords);
        let mut emitted = 0;
        for (tile, result) in tiled.into_tile_data() {
            let geom = gcs::reassemble_points_owned(result);
            self.encode_and_emit_feature(feature, feature.get_id(), attrs, tile, &geom, group_info, 0, consumer);
            emitted += 1;
        }
        self.stats.emitted_features(zoom, feature.get_layer(), emitted);
        self.stats.processed_element("point", feature.get_layer(), zoom);
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_and_emit_feature(
        &self,
        feature: &Feature<'_>,
        id: i64,
        attrs: &Arc<Tags>,
        tile: TileCoord,
        geom: &Geometry<f64>,
        group_info: Option<Group>,
        scale: u32,
        consumer: &mut dyn FnMut(RenderedFeature),
    ) {
        let vector_tile_feature = new_vector_tile_feature(
            feature.get_layer(),
            id,
            vector_tile::encode_geometry_scaled(geom, scale),
            attrs,
            group_info.map_or(vector_tile::Feature::NO_GROUP, |g| g.group),
        );
        consumer(RenderedFeature {
            tile,
            vector_tile_feature: Arc::new(vector_tile_feature),
            sort_key: feature.get_sort_key(),
            group: group_info,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn slice(
        &self,
        geom: &Geometry<f64>,
        min_size: f64,
        buffer: f64,
        area: bool,
        linear_rings: bool,
        zoom: u32,
    ) -> GeometryResult<TiledGeometry> {
        let extents = self.extents.get_for_zoom(zoom);
        let groups = gcs::extract_groups_with_rings(geom, min_size, linear_rings);
        TiledGeometry::slice_into_tiles(&groups, buffer, area, zoom, extents)
    }

    fn render_line_or_polygon(
        &self,
        zoom: u32,
        attrs: &Arc<Tags>,
        feature: &Feature<'_>,
        mut geom: Geometry<f64>,
        linear_rings: bool,
        consumer: &mut dyn FnMut(RenderedFeature),
    ) {
        let area = matches!(geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
        let mut min_size = feature.get_min_pixel_size_at_zoom(zoom) / 256.0;
        let buffer = feature.get_buffer_pixels_at_zoom(zoom) / 256.0;
        if area {
            // treat minPixelSize as the edge of a square that defines minimum area for features
            min_size *= min_size;
        }
        let sliced = match self.slice(&geom, min_size, buffer, area, linear_rings, zoom) {
            Ok(sliced) => Some(sliced),
            Err(_) => {
                match geo_utils::fix_polygon(&geom).and_then(|fixed| {
                    let sliced = self.slice(&fixed, min_size, buffer, area, linear_rings, zoom)?;
                    Ok((fixed, sliced))
                }) {
                    Ok((fixed, sliced)) => {
                        geom = fixed;
                        Some(sliced)
                    }
                    Err(e) => {
                        e.log_with_stats(
                            &*self.stats,
                            "slice_line_or_polygon",
                            &format!("Error slicing feature at z{zoom}: {feature:?}"),
                        );
                        // omit from this zoom level, but maybe the next will be better
                        None
                    }
                }
            }
        };
        if let Some(sliced) = sliced {
            if let Some(num_points_attr) = feature.get_num_points_attr() {
                // if profile wants the original number of points that the simplified but untiled geometry started with
                let mut with_points = (**attrs).clone();
                with_points.insert(num_points_attr.to_string(), Value::Int(geom.coords_count() as i64));
                self.write_tile_features(zoom, feature.get_id(), feature, sliced, &Arc::new(with_points), consumer);
            } else {
                self.write_tile_features(zoom, feature.get_id(), feature, sliced, attrs, consumer);
            }
        }
        self.stats.processed_element(if area { "polygon" } else { "line" }, feature.get_layer(), zoom);
    }

    fn write_tile_features(
        &self,
        zoom: u32,
        id: i64,
        feature: &Feature<'_>,
        sliced: TiledGeometry,
        attrs: &Arc<Tags>,
        consumer: &mut dyn FnMut(RenderedFeature),
    ) {
        let mut emitted = 0;
        let sliced_zoom = sliced.zoom_level();
        let (filled_tiles, tile_data) = sliced.into_parts();
        for (tile, geoms) in tile_data {
            let result: GeometryResult<(Geometry<f64>, u32)> = if feature.is_polygon() {
                gcs::reassemble_polygons_owned(geoms).and_then(|geom| {
                    // Use the very expensive, but necessary buffer(0) trick to repair invalid polygons (with self-
                    // intersections) and the precision reducer to snap polygon nodes to the vector tile grid without
                    // introducing self-intersections.
                    //
                    // See https://docs.mapbox.com/vector-tiles/specification/#simplification for issues that can
                    // arise from naive coordinate rounding.
                    let geom = geo_utils::snap_and_fix_polygon(&geom, &*self.stats, "render")?;
                    // The fixers make the geometry clockwise outer/CCW inner but vector tiles flip Y coordinate, so
                    // we need outer CCW/inner clockwise
                    Ok((reverse_polygonal(geom), 0))
                })
            } else {
                let geom = gcs::reassemble_line_strings_owned(geoms);
                // Store lines with extra precision (2^scale) in intermediate feature storage so that rounding does
                // not introduce artificial endpoint intersections and confuse line merge post-processing. Features
                // need to be "unscaled" in FeatureGroup after line merging, and before emitting to the output archive.
                let scale = self.config.maxzoom.max(14) - zoom;
                // need 14 bits to represent tile coordinates (4096 * 2 for buffer * 2 for zigzag encoding) so cap the
                // scale factor to avoid overflowing 32-bit integer space
                Ok((geom, scale.min(31 - 14)))
            };
            match result {
                Ok((geom, scale)) => {
                    if !geom.is_empty() {
                        self.encode_and_emit_feature(feature, id, attrs, tile, &geom, None, scale, consumer);
                        emitted += 1;
                    }
                }
                Err(e) => e.log_with_stats(
                    &*self.stats,
                    "write_tile_features",
                    &format!("Error writing tile {tile:?} feature {feature:?}"),
                ),
            }
        }

        // polygons that span multiple tiles contain detail about the outer edges separate from the filled tiles, so
        // emit filled tiles now
        if feature.is_polygon() {
            emitted += self.emit_filled_tiles(zoom, sliced_zoom, id, feature, &filled_tiles, consumer);
        }

        self.stats.emitted_features(zoom, feature.get_layer(), emitted);
    }

    fn emit_filled_tiles(
        &self,
        zoom: u32,
        sliced_zoom: u32,
        id: i64,
        feature: &Feature<'_>,
        filled_tiles: &[TileCoord],
        consumer: &mut dyn FnMut(RenderedFeature),
    ) -> usize {
        if filled_tiles.is_empty() {
            return 0;
        }
        // Optimization: large input polygons that generate many filled interior tiles (i.e. the ocean): the encoder
        // avoids re-encoding if the vector tile feature is the same (Arc::ptr_eq) as the previous one, so compute one
        // instance at the start of each zoom level for this feature.
        let vector_tile_feature = Arc::new(new_vector_tile_feature(
            feature.get_layer(),
            id,
            vector_tile::encode_fill(feature.get_buffer_pixels_at_zoom(zoom)),
            feature.get_attrs_at_zoom(sliced_zoom),
            vector_tile::Feature::NO_GROUP,
        ));
        for tile in filled_tiles {
            consumer(RenderedFeature {
                tile: *tile,
                vector_tile_feature: vector_tile_feature.clone(),
                sort_key: feature.get_sort_key(),
                group: None,
            });
        }
        filled_tiles.len()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use geo::Buffer;
    use geo::algorithm::buffer::{BufferStyle, LineCap, LineJoin};

    use super::*;
    use crate::config::arguments::Arguments;
    use crate::feature_collector::Factory;
    use crate::geo::geometry_pipeline;
    use crate::geo::polygon_overlay;
    use crate::geo::precision_model::PrecisionModel;
    use crate::geo::test_utils::*;
    use crate::reader::simple_feature::SimpleFeature;
    use crate::stats::stats::in_memory;
    use crate::tags;

    const Z14_TILES: u32 = 1 << 14;
    const Z14_WIDTH: f64 = 1.0 / Z14_TILES as f64;
    const Z14_PX: f64 = Z14_WIDTH / 256.0;
    const Z13_TILES: u32 = 1 << 13;
    const Z13_WIDTH: f64 = 1.0 / Z13_TILES as f64;
    const Z13_PX: f64 = Z13_WIDTH / 256.0;
    const TILE_RESOLUTION_PX: f64 = 256.0 / 4096.0;

    #[derive(Clone, Copy)]
    enum Kind {
        Point,
        Line,
        Polygon,
    }

    fn default_config() -> Arc<PlanetilerConfig> {
        Arc::new(PlanetilerConfig::defaults())
    }

    /// Renders a feature of `kind` from a source feature with `world_geom`, configured by `configure`.
    fn render_features_with(
        config: &Arc<PlanetilerConfig>,
        world_geom: impl Into<Geometry<f64>>,
        kind: Kind,
        configure: impl FnOnce(&mut Feature<'_>),
    ) -> BTreeMap<TileCoord, Vec<RenderedFeature>> {
        let lat_lon = geo_utils::world_to_lat_lon_coords(&world_geom.into());
        let factory = Factory::new(config.clone(), in_memory());
        let source = SimpleFeature::create(lat_lon, Tags::default(), None::<&str>, None::<&str>, 1);
        let mut collector = factory.get(&source);
        let feature = match kind {
            Kind::Point => collector.point("layer"),
            Kind::Line => collector.line("layer"),
            Kind::Polygon => collector.polygon("layer"),
        };
        configure(feature);
        let renderer = FeatureRenderer::new(config.clone(), in_memory());
        let mut result: BTreeMap<TileCoord, Vec<RenderedFeature>> = BTreeMap::new();
        for feature in collector.iter() {
            renderer.accept(feature, &mut |rendered: RenderedFeature| {
                result.entry(rendered.tile).or_default().push(rendered)
            });
        }
        for features in result.values() {
            for f in features {
                validate_geometry(&decode_silently(&f.vector_tile_feature.geometry));
            }
        }
        result
    }

    fn render_geometry_with(
        config: &Arc<PlanetilerConfig>,
        world_geom: impl Into<Geometry<f64>>,
        kind: Kind,
        configure: impl FnOnce(&mut Feature<'_>),
    ) -> TileGeometries {
        render_features_with(config, world_geom, kind, configure)
            .into_iter()
            .map(|(tile, features)| {
                (tile, features.iter().map(|f| decode_silently(&f.vector_tile_feature.geometry)).collect())
            })
            .collect()
    }

    fn render_geometry(
        world_geom: impl Into<Geometry<f64>>,
        kind: Kind,
        configure: impl FnOnce(&mut Feature<'_>),
    ) -> TileGeometries {
        render_geometry_with(&default_config(), world_geom, kind, configure)
    }

    fn render_features(
        world_geom: impl Into<Geometry<f64>>,
        kind: Kind,
        configure: impl FnOnce(&mut Feature<'_>),
    ) -> BTreeMap<TileCoord, Vec<RenderedFeature>> {
        render_features_with(&default_config(), world_geom, kind, configure)
    }

    /// Decoded geometry and tags of every feature rendered into `coord`, sorted for comparison.
    fn rendered_tile_features(
        world_geom: impl Into<Geometry<f64>>,
        kind: Kind,
        configure: impl FnOnce(&mut Feature<'_>),
        coord: TileCoord,
    ) -> Vec<(Geometry<f64>, Tags)> {
        let rendered = render_features(world_geom, kind, configure);
        sorted_features(rendered.get(&coord).expect("tile"))
    }

    fn sorted_features(features: &[RenderedFeature]) -> Vec<(Geometry<f64>, Tags)> {
        let mut result: Vec<(Geometry<f64>, Tags)> = features
            .iter()
            .map(|f| {
                let d = &f.vector_tile_feature;
                let tags: Tags = d.tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                (d.geometry.decode().unwrap(), tags)
            })
            .collect();
        result.sort_by_key(|x| format!("{x:?}"));
        result
    }

    fn sorted(mut items: Vec<(Geometry<f64>, Tags)>) -> Vec<(Geometry<f64>, Tags)> {
        items.sort_by_key(|x| format!("{x:?}"));
        items
    }

    fn t14(dx: i32, dy: i32) -> TileCoord {
        TileCoord::of_xyz((Z14_TILES as i32 / 2 + dx) as u32, (Z14_TILES as i32 / 2 + dy) as u32, 14)
    }

    fn t13(dx: i32, dy: i32) -> TileCoord {
        TileCoord::of_xyz((Z13_TILES as i32 / 2 + dx) as u32, (Z13_TILES as i32 / 2 + dy) as u32, 13)
    }

    fn txyz(x: u32, y: u32, z: u32) -> TileCoord {
        TileCoord::of_xyz(x, y, z)
    }

    fn pt(x: f64, y: f64) -> Geometry<f64> {
        Geometry::Point(new_point(x, y))
    }

    fn mpt(points: &[(f64, f64)]) -> Geometry<f64> {
        Geometry::MultiPoint(new_multi_point(points.iter().map(|&(x, y)| new_point(x, y)).collect()))
    }

    fn ls(coords: &[f64]) -> Geometry<f64> {
        Geometry::LineString(new_line_string(coords))
    }

    fn mls(lines: &[&[f64]]) -> Geometry<f64> {
        Geometry::MultiLineString(new_multi_line_string(lines.iter().map(|l| new_line_string(l)).collect()))
    }

    fn poly(coords: &[f64]) -> Geometry<f64> {
        Geometry::Polygon(new_polygon(coords))
    }

    fn rect(min: f64, max: f64) -> Geometry<f64> {
        Geometry::Polygon(rectangle(min, max))
    }

    fn rect_xy(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Geometry<f64> {
        Geometry::Polygon(rectangle_xy(min_x, min_y, max_x, max_y))
    }

    fn gc(geoms: Vec<Geometry<f64>>) -> Geometry<f64> {
        Geometry::GeometryCollection(new_geometry_collection(geoms))
    }

    fn z14_world_coords(coords: &[f64]) -> Vec<f64> {
        coords.iter().map(|c| 0.5 + Z14_PX * c).collect()
    }

    fn scale_translate(geom: &Geometry<f64>, scale: f64, tx: f64, ty: f64) -> Geometry<f64> {
        geom.map_coords(|c| Coord { x: c.x * scale + tx, y: c.y * scale + ty })
    }

    /// JTS `AffineTransformation.rotationInstance(theta, x, y)`.
    fn rotate(geom: &Geometry<f64>, theta: f64, cx: f64, cy: f64) -> Geometry<f64> {
        let (sin, cos) = theta.sin_cos();
        geom.map_coords(|c| {
            let (dx, dy) = (c.x - cx, c.y - cy);
            Coord { x: cx + dx * cos - dy * sin, y: cy + dx * sin + dy * cos }
        })
    }

    fn rotate_world(geom: &Geometry<f64>, degrees: f64) -> Geometry<f64> {
        rotate(geom, -std::f64::consts::PI * degrees / 180.0, 0.5 + Z14_WIDTH / 2.0, 0.5 + Z14_WIDTH / 2.0)
    }

    fn rotate_tile(geom: &Geometry<f64>, degrees: f64) -> Geometry<f64> {
        rotate(geom, -std::f64::consts::PI * degrees / 180.0, 128.0, 128.0)
    }

    #[test]
    fn test_empty_geometry() {
        let rendered = render_geometry(geo_utils::empty_point(), Kind::Point, |_| {});
        assert_same_normalized_features(&tile_geometries([]), &rendered);
    }

    /*
     * POINT TESTS
     */

    #[test]
    fn test_single_point() {
        let point = pt(0.5 + Z14_WIDTH / 2.0, 0.5 + Z14_WIDTH / 2.0);
        for geom in [point.clone(), gc(vec![point])] {
            let rendered = render_geometry(geom, Kind::Point, |f| {
                f.set_zoom_range(14, 14);
            });
            assert_same_normalized_features(&tile_geometries([(t14(0, 0), vec![pt(128.0, 128.0)])]), &rendered);
        }
    }

    #[test]
    fn test_repeat_single_point_neighboring_tiles() {
        let rendered = render_geometry(pt(0.5 + 1.0 / 512.0, 0.5 + 1.0 / 512.0), Kind::Point, |f| {
            f.set_zoom_range(0, 1).set_buffer_pixels(2.0);
        });
        assert_same_normalized_features(
            &tile_geometries([
                (txyz(0, 0, 0), vec![pt(128.5, 128.5)]),
                (txyz(0, 0, 1), vec![pt(257.0, 257.0)]),
                (txyz(1, 0, 1), vec![pt(1.0, 257.0)]),
                (txyz(0, 1, 1), vec![pt(257.0, 1.0)]),
                (txyz(1, 1, 1), vec![pt(1.0, 1.0)]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_repeat_single_point_neighboring_tiles_buffer0() {
        let rendered = render_geometry(pt(0.5, 0.5), Kind::Point, |f| {
            f.set_zoom_range(1, 1).set_buffer_pixels(0.0);
        });
        assert_same_normalized_features(
            &tile_geometries([
                (txyz(0, 0, 1), vec![pt(256.0, 256.0)]),
                (txyz(1, 0, 1), vec![pt(0.0, 256.0)]),
                (txyz(0, 1, 1), vec![pt(256.0, 0.0)]),
                (txyz(1, 1, 1), vec![pt(0.0, 0.0)]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_emit_points_respect_extents() {
        let config = Arc::new(PlanetilerConfig::from(&Arguments::of_pairs([("bounds", "0,-80,180,0")])).unwrap());
        let rendered = render_geometry_with(&config, pt(0.5 + 1.0 / 512.0, 0.5 + 1.0 / 512.0), Kind::Point, |f| {
            f.set_zoom_range(0, 1).set_buffer_pixels(2.0);
        });
        assert_same_normalized_features(
            &tile_geometries([(txyz(0, 0, 0), vec![pt(128.5, 128.5)]), (txyz(1, 1, 1), vec![pt(1.0, 1.0)])]),
            &rendered,
        );
    }

    #[test]
    fn test_emit_points_respect_shape() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/bottomrightearth.poly");
        let config = Arc::new(
            PlanetilerConfig::from(&Arguments::of_pairs([("polygon", path.to_string_lossy().to_string())])).unwrap(),
        );
        let rendered = render_geometry_with(&config, pt(0.5 + 1.0 / 512.0, 0.5 + 1.0 / 512.0), Kind::Point, |f| {
            f.set_zoom_range(0, 2).set_buffer_pixels(2.0);
        });
        assert_same_normalized_features(
            &tile_geometries([(txyz(0, 0, 0), vec![pt(128.5, 128.5)]), (txyz(1, 1, 1), vec![pt(1.0, 1.0)])]),
            &rendered,
        );
    }

    #[test]
    fn test_process_points_near_international_date_line_and_poles() {
        let d = 1.0 / 512.0;
        // (x, wrapped, z1x0, z1x1)
        let xs = [
            (-d, 1.0 - d, -1.0, 255.0),
            (d, 1.0 + d, 1.0, 257.0),
            (1.0 - d, -d, -1.0, 255.0),
            (1.0 + d, d, 1.0, 257.0),
        ];
        // (y, z1ty, tyoff)
        let ys = [(0.25, 0, 128.0), (-d, 0, -1.0), (d, 0, 1.0), (1.0 - d, 1, 255.0), (1.0 + d, 1, 257.0)];
        for (x, wrapped, z1x0, z1x1) in xs {
            for (y, z1ty, tyoff) in ys {
                let rendered = render_geometry(pt(x, y), Kind::Point, |f| {
                    f.set_zoom_range(0, 1).set_buffer_pixels(2.0);
                });
                assert_same_normalized_features(
                    &tile_geometries([
                        (txyz(0, 0, 0), vec![mpt(&[(x * 256.0, y * 256.0), (wrapped * 256.0, y * 256.0)])]),
                        (txyz(0, z1ty, 1), vec![pt(z1x0, tyoff)]),
                        (txyz(1, z1ty, 1), vec![pt(z1x1, tyoff)]),
                    ]),
                    &rendered,
                );
            }
        }
    }

    #[test]
    fn test_z0_full_tile_buffer() {
        let rendered = render_geometry(pt(0.25, 0.25), Kind::Point, |f| {
            f.set_zoom_range(0, 1).set_buffer_pixels(256.0);
        });
        assert_same_normalized_features(
            &tile_geometries([
                (txyz(0, 0, 0), vec![mpt(&[(-192.0, 64.0), (64.0, 64.0), (320.0, 64.0)])]),
                (txyz(0, 0, 1), vec![pt(128.0, 128.0)]),
                (txyz(1, 0, 1), vec![mpt(&[(-128.0, 128.0), (256.0 + 128.0, 128.0)])]),
                (txyz(0, 1, 1), vec![pt(128.0, -128.0)]),
                (txyz(1, 1, 1), vec![mpt(&[(-128.0, -128.0), (256.0 + 128.0, -128.0)])]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_multipoint_no_label_grid() {
        let rendered =
            render_geometry(mpt(&[(0.25, 0.25), (0.25 + 1.0 / 256.0, 0.25 + 1.0 / 256.0)]), Kind::Point, |f| {
                f.set_zoom_range(0, 1).set_buffer_pixels(4.0);
            });
        assert_same_normalized_features(
            &tile_geometries([
                (txyz(0, 0, 0), vec![mpt(&[(64.0, 64.0), (65.0, 65.0)])]),
                (txyz(0, 0, 1), vec![mpt(&[(128.0, 128.0), (130.0, 130.0)])]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_multipoint_with_label_grid_splits() {
        let rendered =
            render_geometry(mpt(&[(0.25, 0.25), (0.25 + 1.0 / 256.0, 0.25 + 1.0 / 256.0)]), Kind::Point, |f| {
                f.set_point_label_grid_pixel_size(10, 10.0).set_zoom_range(0, 1).set_buffer_pixels(10.0);
            });
        assert_same_normalized_features(
            &tile_geometries([
                (txyz(0, 0, 0), vec![pt(64.0, 64.0), pt(65.0, 65.0)]),
                (txyz(0, 0, 1), vec![pt(128.0, 128.0), pt(130.0, 130.0)]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_label_grid_requires_buffer_pixels_greater_than_grid_size() {
        let result = std::panic::catch_unwind(|| {
            render_features(pt(0.75, 0.75), Kind::Point, |f| {
                f.set_point_label_grid_size_and_limit(10, 10.0, 2).set_buffer_pixels(9.0);
            })
        });
        assert!(result.is_err(), "expected assertion error");
        render_features(pt(0.75, 0.75), Kind::Point, |f| {
            f.set_point_label_grid_size_and_limit(10, 10.0, 2).set_buffer_pixels(10.0);
        });
    }

    #[test]
    fn test_label_grid() {
        let rendered = render_features(pt(0.75, 0.75), Kind::Point, |f| {
            f.set_point_label_grid_size_and_limit(10, 256.0, 2).set_zoom_range(0, 1).set_buffer_pixels(256.0);
        });
        let z0 = &rendered[&txyz(0, 0, 0)][0];
        let z1 = &rendered[&txyz(1, 1, 1)][0];
        assert_eq!(Some(Group { group: 0, limit: 2 }), z0.group);
        assert_eq!(Some(Group { group: (1i64 << 32) + 1, limit: 2 }), z1.group);
    }

    #[test]
    fn test_wrap_label_grid() {
        let rendered = render_features(pt(1.1, -0.1), Kind::Point, |f| {
            f.set_point_label_grid_size_and_limit(10, 256.0, 2).set_zoom_range(0, 1).set_buffer_pixels(256.0);
        });
        let z0 = &rendered[&txyz(0, 0, 0)][0];
        let z1 = &rendered[&txyz(0, 0, 1)][0];
        assert_eq!(Some(Group { group: (1i64 << 32) - 1, limit: 2 }), z0.group);
        assert_eq!(Some(Group { group: (1i64 << 32) - 1, limit: 2 }), z1.group);
    }

    /*
     * LINE TESTS
     */

    #[test]
    fn test_split_line_feature_single_tile() {
        let z14hypot = (Z14_WIDTH * Z14_WIDTH).sqrt();
        let line =
            ls(&[0.5 + z14hypot / 4.0, 0.5 + z14hypot / 4.0, 0.5 + z14hypot * 3.0 / 4.0, 0.5 + z14hypot * 3.0 / 4.0]);
        for geom in [line.clone(), gc(vec![line])] {
            let rendered = render_geometry(geom, Kind::Line, |f| {
                f.set_zoom_range(14, 14).set_buffer_pixels(8.0);
            });
            assert_exact_same_features(
                &tile_geometries([(t14(0, 0), vec![ls(&[64.0, 64.0, 192.0, 192.0])])]),
                &rendered,
            );
        }
    }

    #[test]
    fn test_simplify_line() {
        let z14hypot = (Z14_WIDTH * Z14_WIDTH).sqrt();
        let rendered = render_geometry(
            ls(&[
                0.5 + z14hypot / 4.0,
                0.5 + z14hypot / 4.0,
                0.5 + z14hypot / 2.0,
                0.5 + z14hypot / 2.0,
                0.5 + z14hypot * 3.0 / 4.0,
                0.5 + z14hypot * 3.0 / 4.0,
            ]),
            Kind::Line,
            |f| {
                f.set_zoom_range(14, 14).set_buffer_pixels(8.0);
            },
        );
        assert_exact_same_features(&tile_geometries([(t14(0, 0), vec![ls(&[64.0, 64.0, 192.0, 192.0])])]), &rendered);
    }

    #[test]
    fn test_split_line_feature_touching_neighboring_tile() {
        let z14hypot = (Z14_WIDTH * Z14_WIDTH).sqrt();
        let rendered = render_geometry(
            ls(&[
                0.5 + z14hypot / 4.0,
                0.5 + z14hypot / 4.0,
                0.5 + Z14_WIDTH * (256.0 - 8.0) / 256.0,
                0.5 + Z14_WIDTH * (256.0 - 8.0) / 256.0,
            ]),
            Kind::Line,
            |f| {
                f.set_zoom_range(14, 14).set_buffer_pixels(8.0);
            },
        );
        // only a single point in neighboring tile, exclude
        assert_exact_same_features(
            &tile_geometries([(t14(0, 0), vec![ls(&[64.0, 64.0, 256.0 - 8.0, 256.0 - 8.0])])]),
            &rendered,
        );
    }

    #[test]
    fn test_split_line_feature_entering_neighboring_tile_boundary() {
        let z14hypot = (Z14_WIDTH * Z14_WIDTH).sqrt();
        let rendered = render_geometry(
            ls(&[
                0.5 + z14hypot / 4.0,
                0.5 + z14hypot / 4.0,
                0.5 + Z14_WIDTH * (256.0 - 7.0) / 256.0,
                0.5 + Z14_WIDTH * (256.0 - 7.0) / 256.0,
            ]),
            Kind::Line,
            |f| {
                f.set_zoom_range(14, 14).set_buffer_pixels(8.0);
            },
        );
        assert_exact_same_features(
            &tile_geometries([
                (t14(0, 0), vec![ls(&[64.0, 64.0, 256.0 - 7.0, 256.0 - 7.0])]),
                (t14(1, 0), vec![ls(&[-8.0, 248.0, -7.0, 249.0])]),
                (t14(0, 1), vec![ls(&[248.0, -8.0, 249.0, -7.0])]),
                (t14(1, 1), vec![ls(&[-8.0, -8.0, -7.0, -7.0])]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_3_point_line() {
        let rendered = render_geometry(
            ls(&[
                0.5 + Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0,
                0.5 + 3.0 * Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0,
                0.5 + 3.0 * Z14_WIDTH / 2.0,
                0.5 + 3.0 * Z14_WIDTH / 2.0,
            ]),
            Kind::Line,
            |f| {
                f.set_zoom_range(14, 14).set_buffer_pixels(8.0);
            },
        );
        assert_exact_same_features(
            &tile_geometries([
                (t14(0, 0), vec![ls(&[128.0, 128.0, 256.0 + 8.0, 128.0])]),
                (t14(1, 0), vec![ls(&[-8.0, 128.0, 128.0, 128.0, 128.0, 256.0 + 8.0])]),
                (t14(1, 1), vec![ls(&[128.0, -8.0, 128.0, 128.0])]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_limit_single_line_string_length() {
        let eps = Z13_WIDTH / 4096.0;
        let pixel = Z13_WIDTH / 256.0;
        // below limit - ignore
        let below = render_geometry(mls(&[&[0.5, 0.5 + pixel, 0.5 + pixel - eps, 0.5 + pixel]]), Kind::Line, |f| {
            f.set_min_pixel_size(1.0).set_zoom_range(13, 13).set_buffer_pixels(0.0);
        });
        // above limit - allow
        let above = render_geometry(mls(&[&[0.5, 0.5 + pixel, 0.5 + pixel + eps, 0.5 + pixel]]), Kind::Line, |f| {
            f.set_min_pixel_size(1.0).set_zoom_range(13, 13).set_buffer_pixels(0.0);
        });
        assert_exact_same_features(&tile_geometries([]), &below);
        assert_exact_same_features(
            &tile_geometries([(t13(0, 0), vec![ls(&[0.0, 1.0, 1.0 + 256.0 / 4096.0, 1.0])])]),
            &above,
        );
    }

    #[test]
    fn test_limit_multi_line_string_length() {
        let eps = Z13_WIDTH / 4096.0;
        let pixel = Z13_WIDTH / 256.0;
        let rendered = render_geometry(
            mls(&[
                // below limit - ignore
                &[0.5, 0.5 + pixel, 0.5 + pixel - eps, 0.5 + pixel],
                // above limit - allow
                &[0.5, 0.5 + pixel, 0.5 + pixel + eps, 0.5 + pixel],
            ]),
            Kind::Line,
            |f| {
                f.set_min_pixel_size(1.0).set_zoom_range(13, 13).set_buffer_pixels(0.0);
            },
        );
        assert_exact_same_features(
            &tile_geometries([(t13(0, 0), vec![ls(&[0.0, 1.0, 1.0 + 256.0 / 4096.0, 1.0])])]),
            &rendered,
        );
    }

    #[test]
    fn test_duplicate_points_removed_after_rounding() {
        let eps = Z14_WIDTH / 4096.0;
        let pixel = Z14_WIDTH / 256.0;
        let rendered = render_geometry(
            ls(&[
                0.5 + pixel * 10.0,
                0.5 + pixel * 10.0,
                0.5 + pixel * 20.0,
                0.5 + pixel * 10.0,
                0.5 + pixel * 20.0,
                0.5 + pixel * 10.0 + eps / 3.0,
            ]),
            Kind::Line,
            |f| {
                f.set_min_pixel_size(1.0)
                    .set_zoom_range(14, 14)
                    .set_buffer_pixels(0.0)
                    .set_pixel_tolerance_at_all_zooms(0.0);
            },
        );
        assert_exact_same_features(&tile_geometries([(t14(0, 0), vec![ls(&[10.0, 10.0, 20.0, 10.0])])]), &rendered);
    }

    #[test]
    fn test_line_string_collapses_to_point_with_rounding() {
        let eps = Z14_WIDTH / 4096.0;
        let pixel = Z14_WIDTH / 256.0;
        let rendered = render_geometry(
            ls(&[0.5 + pixel * 10.0, 0.5 + pixel * 10.0, 0.5 + pixel * 10.0 + eps / 3.0, 0.5 + pixel * 10.0]),
            Kind::Line,
            |f| {
                f.set_min_pixel_size(1.0)
                    .set_zoom_range(14, 14)
                    .set_buffer_pixels(0.0)
                    .set_pixel_tolerance_at_all_zooms(0.0);
            },
        );
        assert_exact_same_features(&tile_geometries([]), &rendered);
    }

    #[test]
    fn test_self_intersecting_line_string_ok() {
        let rendered = render_geometry(
            ls(&z14_world_coords(&[10.0, 10.0, 20.0, 20.0, 10.0, 20.0, 20.0, 10.0, 10.0, 10.0])),
            Kind::Line,
            |f| {
                f.set_min_pixel_size(1.0).set_zoom_range(14, 14);
            },
        );
        assert_exact_same_features(
            &tile_geometries([(t14(0, 0), vec![ls(&[10.0, 10.0, 20.0, 20.0, 10.0, 20.0, 20.0, 10.0, 10.0, 10.0])])]),
            &rendered,
        );
    }

    #[test]
    fn test_line_wrap() {
        let rendered =
            render_geometry(ls(&[-1.0 / 256.0, -1.0 / 256.0, 257.0 / 256.0, 257.0 / 256.0]), Kind::Line, |f| {
                f.set_min_pixel_size(1.0).set_buffer_pixels(4.0).set_zoom_range(0, 1);
            });
        assert_same_normalized_features(
            &tile_geometries([
                (
                    txyz(0, 0, 0),
                    vec![mls(&[&[-1.0, -1.0, 257.0, 257.0], &[-4.0, 252.0, 1.0, 257.0], &[255.0, -1.0, 260.0, 4.0]])],
                ),
                (txyz(0, 0, 1), vec![ls(&[-2.0, -2.0, 260.0, 260.0])]),
                (txyz(1, 0, 1), vec![mls(&[&[-4.0, 252.0, 4.0, 260.0], &[254.0, -2.0, 260.0, 4.0]])]),
                (txyz(0, 1, 1), vec![mls(&[&[252.0, -4.0, 260.0, 4.0], &[-4.0, 252.0, 2.0, 258.0]])]),
                (txyz(1, 1, 1), vec![ls(&[-4.0, -4.0, 258.0, 258.0])]),
            ]),
            &rendered,
        );
    }

    /*
     * POLYGON TESTS
     */

    fn polygon_14(geom: impl Into<Geometry<f64>>, buffer: f64) -> TileGeometries {
        render_geometry(geom, Kind::Polygon, |f| {
            f.set_min_pixel_size(1.0).set_zoom_range(14, 14).set_buffer_pixels(buffer);
        })
    }

    #[test]
    fn test_simple_triangle_ccw() {
        let polygon = poly(&[
            0.5 + Z14_PX * 10.0,
            0.5 + Z14_PX * 10.0,
            0.5 + Z14_PX * 20.0,
            0.5 + Z14_PX * 10.0,
            0.5 + Z14_PX * 10.0,
            0.5 + Z14_PX * 20.0,
            0.5 + Z14_PX * 10.0,
            0.5 + Z14_PX * 10.0,
        ]);
        for geom in [polygon.clone(), gc(vec![polygon])] {
            assert_same_normalized_features(
                &tile_geometries([(t14(0, 0), vec![poly(&[10.0, 10.0, 20.0, 10.0, 10.0, 20.0, 10.0, 10.0])])]),
                &polygon_14(geom, 0.0),
            );
        }
    }

    #[test]
    fn test_simple_triangle_cw() {
        let rendered = polygon_14(
            poly(&[
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 20.0,
                0.5 + Z14_PX * 20.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
            ]),
            0.0,
        );
        assert_same_normalized_features(
            &tile_geometries([(t14(0, 0), vec![poly(&[10.0, 10.0, 10.0, 20.0, 20.0, 10.0, 10.0, 10.0])])]),
            &rendered,
        );
    }

    #[test]
    fn test_triangle_touching_neighboring_tile_does_not_emit() {
        let rendered = polygon_14(
            poly(&[
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 256.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 20.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
            ]),
            0.0,
        );
        assert_same_normalized_features(
            &tile_geometries([(t14(0, 0), vec![poly(&[10.0, 10.0, 256.0, 10.0, 10.0, 20.0, 10.0, 10.0])])]),
            &rendered,
        );
    }

    #[test]
    fn test_triangle_touching_neighboring_tile_below_does_not_emit() {
        let rendered = polygon_14(
            poly(&[
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 20.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 256.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
            ]),
            0.0,
        );
        assert_same_normalized_features(
            &tile_geometries([(t14(0, 0), vec![poly(&[10.0, 10.0, 20.0, 10.0, 10.0, 256.0, 10.0, 10.0])])]),
            &rendered,
        );
    }

    #[test]
    fn test_rectangle_touching_neighboring_tiles_does_not_emit() {
        let cases: [(f64, f64, f64, f64); 13] = [
            (0.0, 256.0, 0.0, 256.0),     // all
            (0.0, 10.0, 0.0, 10.0),       // top-left
            (5.0, 10.0, 0.0, 10.0),       // top partial
            (250.0, 256.0, 0.0, 10.0),    // top all
            (250.0, 256.0, 0.0, 10.0),    // top-right
            (250.0, 256.0, 0.0, 256.0),   // right all
            (250.0, 256.0, 10.0, 250.0),  // right partial
            (250.0, 256.0, 250.0, 256.0), // right bottom
            (0.0, 256.0, 250.0, 256.0),   // bottom all
            (240.0, 250.0, 250.0, 256.0), // bottom partial
            (0.0, 10.0, 250.0, 256.0),    // bottom left
            (0.0, 10.0, 0.0, 256.0),      // left all
            (0.0, 10.0, 240.0, 250.0),    // left partial
        ];
        for (x1, x2, y1, y2) in cases {
            let rendered =
                polygon_14(rect_xy(0.5 + Z14_PX * x1, 0.5 + Z14_PX * y1, 0.5 + Z14_PX * x2, 0.5 + Z14_PX * y2), 0.0);
            assert_same_normalized_features(&tile_geometries([(t14(0, 0), vec![rect_xy(x1, y1, x2, y2)])]), &rendered);
        }
    }

    #[test]
    fn test_overlap_tile_horizontal() {
        let rendered = polygon_14(
            rect_xy(0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 258.0, 0.5 + Z14_PX * 20.0),
            1.0,
        );
        assert_same_normalized_features(
            &tile_geometries([
                (t14(0, 0), vec![rect_xy(10.0, 10.0, 257.0, 20.0)]),
                (t14(1, 0), vec![rect_xy(-1.0, 10.0, 2.0, 20.0)]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_overlap_tile_vertical() {
        let rendered = polygon_14(
            rect_xy(0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 20.0, 0.5 + Z14_PX * 258.0),
            1.0,
        );
        assert_same_normalized_features(
            &tile_geometries([
                (t14(0, 0), vec![rect_xy(10.0, 10.0, 20.0, 257.0)]),
                (t14(0, 1), vec![rect_xy(10.0, -1.0, 20.0, 2.0)]),
            ]),
            &rendered,
        );
    }

    #[test]
    fn test_overlap_tile_corner() {
        let rendered = polygon_14(
            rect_xy(0.5 - Z14_PX * 10.0, 0.5 - Z14_PX * 10.0, 0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 10.0),
            1.0,
        );
        assert_same_normalized_features(
            &tile_geometries([
                (t14(-1, -1), vec![rect(246.0, 257.0)]),
                (t14(0, -1), vec![rect_xy(-1.0, 246.0, 10.0, 257.0)]),
                (t14(-1, 0), vec![rect_xy(246.0, -1.0, 257.0, 10.0)]),
                (t14(0, 0), vec![rect(-1.0, 10.0)]),
            ]),
            &rendered,
        );
    }

    fn surrounded_expectations(center: Geometry<f64>) -> TileGeometries {
        tile_geometries([
            // row 1
            (t14(-1, -1), vec![tile_bottom_right(1.0).into()]),
            (t14(0, -1), vec![tile_bottom(1.0).into()]),
            (t14(1, -1), vec![tile_bottom_left(1.0).into()]),
            // row 2
            (t14(-1, 0), vec![tile_right(1.0).into()]),
            (t14(0, 0), vec![center]),
            (t14(1, 0), vec![tile_left(1.0).into()]),
            // row 3
            (t14(-1, 1), vec![tile_top_right(1.0).into()]),
            (t14(0, 1), vec![tile_top(1.0).into()]),
            (t14(1, 1), vec![tile_top_left(1.0).into()]),
        ])
    }

    #[test]
    fn test_fill() {
        let rendered = polygon_14(rect(0.5 - Z14_WIDTH / 2.0, 0.5 + Z14_WIDTH * 3.0 / 2.0), 1.0);
        assert_same_normalized_features(
            // the filled tile!
            &surrounded_expectations(new_polygon_with_holes(tile_fill(1.0), vec![]).into()),
            &rendered,
        );
    }

    #[test]
    fn test_world_fill() {
        let max_zoom = 8;
        let config = default_config();
        let factory = Factory::new(config.clone(), in_memory());
        let source = SimpleFeature::create(
            geo_utils::world_to_lat_lon_coords(&rect(Z14_WIDTH / 2.0, 1.0 - Z14_WIDTH / 2.0)),
            Tags::default(),
            None::<&str>,
            None::<&str>,
            1,
        );
        let mut collector = factory.get(&source);
        collector.polygon("layer").set_min_pixel_size(1.0).set_zoom_range(max_zoom, max_zoom).set_buffer_pixels(0.0);
        let renderer = FeatureRenderer::new(config, in_memory());
        let mut num = 0u64;
        for feature in collector.iter() {
            renderer.accept(feature, &mut |_| num += 1);
        }
        assert_eq!(4u64.pow(max_zoom), num);
    }

    #[test]
    fn test_complex_polygon() {
        let rendered = polygon_14(
            new_polygon_with_holes(
                rectangle_coord_list(0.5 + Z14_PX * 1.0, 0.5 + Z14_PX * 255.0),
                vec![rectangle_coord_list(0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 250.0)],
            ),
            1.0,
        );
        assert_same_normalized_features(
            &tile_geometries([(
                t14(0, 0),
                vec![
                    new_polygon_with_holes(rectangle_coord_list(1.0, 255.0), vec![rectangle_coord_list(10.0, 250.0)])
                        .into(),
                ],
            )]),
            &rendered,
        );
    }

    #[test]
    fn test_complex_polygon_hole_infers_outer_fill() {
        let rendered = polygon_14(
            new_polygon_with_holes(
                rectangle_coord_list(0.5 - Z14_WIDTH / 2.0, 0.5 + 3.0 * Z14_WIDTH / 2.0),
                vec![rectangle_coord_list(0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 250.0)],
            ),
            1.0,
        );
        assert_same_normalized_features(
            // the filled tile with a hole!
            &surrounded_expectations(
                new_polygon_with_holes(tile_fill(1.0), vec![rectangle_coord_list(10.0, 250.0)]).into(),
            ),
            &rendered,
        );
    }

    #[test]
    fn test_complex_polygon_hole_blocks_fill() {
        let rendered = polygon_14(
            new_polygon_with_holes(
                rectangle_coord_list(0.5 - Z14_WIDTH / 2.0, 0.5 + 3.0 * Z14_WIDTH / 2.0),
                vec![rectangle_coord_list(0.5 - Z14_PX * 10.0, 0.5 + Z14_PX * 260.0)],
            ),
            1.0,
        );
        // should be no data for center tile since it's inside the inner fill
        assert!(!rendered.contains_key(&t14(0, 0)));
        // notch taken out of bottom right
        assert_same_normalized_features(
            &tile_geometries([(
                t14(-1, -1),
                vec![poly(&[
                    128.0, 128.0, 257.0, 128.0, 257.0, 246.0, 246.0, 246.0, 246.0, 257.0, 128.0, 257.0, 128.0, 128.0,
                ])],
            )]),
            &tile_geometries([(t14(-1, -1), rendered[&t14(-1, -1)].clone())]),
        );
        // 4px taken out of top
        assert_same_normalized_features(
            &tile_geometries([(t14(0, 1), vec![rect_xy(-1.0, 4.0, 257.0, 128.0)])]),
            &tile_geometries([(t14(0, 1), rendered[&t14(0, 1)].clone())]),
        );
    }

    #[test]
    fn test_multipolygon() {
        let rendered = polygon_14(
            new_multi_polygon(vec![
                rectangle(0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 20.0),
                rectangle(0.5 + Z14_PX * 30.0, 0.5 + Z14_PX * 40.0),
            ]),
            1.0,
        );
        assert_same_normalized_features(
            &tile_geometries([(
                t14(0, 0),
                vec![new_multi_polygon(vec![rectangle(10.0, 20.0), rectangle(30.0, 40.0)]).into()],
            )]),
            &rendered,
        );
    }

    #[test]
    fn test_fix_invalid_input_geometry() {
        // bow tie
        let rendered = polygon_14(
            poly(&[
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 30.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 20.0,
                0.5 + Z14_PX * 20.0,
                0.5 + Z14_PX * 20.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
            ]),
            1.0,
        );
        assert_same_normalized_features(
            &tile_geometries([(
                t14(0, 0),
                // it's not perfect, but at least it doesn't have a self-intersection
                vec![poly(&[10.0, 10.0, 30.0, 10.0, 16.6875, 16.6875, 10.0, 10.0])],
            )]),
            &rendered,
        );
    }

    #[test]
    fn test_omits_polygon_under_min_size() {
        let rendered = render_geometry(rect(0.5 + Z13_PX * 10.0, 0.5 + Z13_PX * 11.9), Kind::Polygon, |f| {
            f.set_min_pixel_size(2.0).set_zoom_range(13, 13).set_buffer_pixels(1.0);
        });
        assert_eq!(0, rendered.len());
        let rendered = render_geometry(rect(0.5 + Z13_PX * 10.0, 0.5 + Z13_PX * 12.1), Kind::Polygon, |f| {
            f.set_min_pixel_size(2.0).set_zoom_range(13, 13).set_buffer_pixels(1.0);
        });
        assert_eq!(1, rendered.len());
    }

    #[test]
    fn test_collapsed_shell_leaves_hole_filtered_as_linear_ring() {
        // OSM relation 9683714 (a sliver multipolygon in Switzerland): simplifying at z14 collapses the shell to 3
        // points, so JTS returns just the hole as a LinearRing. Java renders that as a polygon whose area must exceed
        // the *unsquared* min pixel size (since a LinearRing isn't Polygonal), which drops it.
        let ring = |c: &[(f64, f64)]| LineString::from(c.to_vec());
        let polygon = Polygon::new(
            ring(&[
                (0.52075588516891, 0.3519382206723094),
                (0.5207561878487468, 0.35193831007927656),
                (0.5207561831921339, 0.3519383240491152),
                (0.5207558814436197, 0.351938234642148),
                (0.52075588516891, 0.3519382206723094),
            ]),
            vec![ring(&[
                (0.5207558870315552, 0.3519382234662771),
                (0.5207558842375875, 0.35193823277950287),
                (0.5207561813294888, 0.35193832125514746),
                (0.520756185054779, 0.3519383119419217),
                (0.5207558870315552, 0.3519382234662771),
            ])],
        );
        let rendered = render_geometry(polygon, Kind::Polygon, |f| {
            f.set_zoom_range(14, 14);
        });
        assert_eq!(0, rendered.len(), "{rendered:?}");
    }

    #[test]
    fn test_includes_polygon_under_min_tolerance() {
        let rendered = render_geometry(rect(0.5 + Z13_PX * 10.0, 0.5 + Z13_PX * 11.9), Kind::Polygon, |f| {
            f.set_min_pixel_size(1.0).set_pixel_tolerance(2.0).set_zoom_range(13, 13).set_buffer_pixels(1.0);
        });
        assert_eq!(1, rendered.len());
    }

    #[test]
    fn test_uses_1px_min_area_at_max_zoom() {
        let base = 0.5 + Z14_WIDTH / 2.0;
        let rendered = render_geometry(rect(base, base + Z14_WIDTH / 4096.0 / 2.0), Kind::Polygon, |f| {
            f.set_min_pixel_size(4.0).set_zoom_range(14, 14).set_buffer_pixels(1.0);
        });
        assert_eq!(0, rendered.len());
        let rendered = render_geometry(rect(base, base + 2.0 * Z14_WIDTH / 4096.0), Kind::Polygon, |f| {
            f.set_min_pixel_size(4.0).set_zoom_range(14, 14).set_buffer_pixels(1.0);
        });
        assert_eq!(1, rendered.len());
    }

    #[test]
    fn test_rounding_collapses_polygon_to_line() {
        let rendered = render_geometry(
            poly(&[
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * (10.0 + 256.0 / 4096.0 / 2.0),
                0.5 + Z14_PX * 100.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 200.0,
                0.5 + Z14_PX * 10.0,
                0.5 + Z14_PX * 10.0,
            ]),
            Kind::Polygon,
            |f| {
                f.set_min_pixel_size(1.0 / 4096.0).set_zoom_range(13, 13).set_buffer_pixels(1.0);
            },
        );
        assert_same_normalized_features(&tile_geometries([]), &rendered);
    }

    fn polygon_14_tiny_min(geom: impl Into<Geometry<f64>>) -> TileGeometries {
        render_geometry(geom, Kind::Polygon, |f| {
            f.set_min_pixel_size(1.0 / 4096.0).set_zoom_range(14, 14).set_buffer_pixels(1.0);
        })
    }

    #[test]
    fn test_rounding_makes_output_invalid() {
        let rendered = polygon_14_tiny_min(poly(&z14_world_coords(&[
            10.0,
            10.0,
            10.0 + TILE_RESOLUTION_PX,
            200.0,
            20.0,
            200.0,
            10.0 + TILE_RESOLUTION_PX / 3.0,
            11.0,
            10.0,
            10.0,
        ])));
        assert_same_normalized_features(
            &tile_geometries([(
                t14(0, 0),
                // it's not perfect, but at least it doesn't have a self-intersection
                vec![poly(&[10.0, 10.0, 20.0, 200.0, 10.0625, 200.0, 10.0, 10.0])],
            )]),
            &rendered,
        );
    }

    #[test]
    fn test_simplify_makes_output_invalid() {
        let rendered = polygon_14_tiny_min(poly(&z14_world_coords(&[
            10.0,
            10.0,
            20.0,
            20.0,
            10.0,
            30.0,
            20.0 - TILE_RESOLUTION_PX / 0.5,
            30.0,
            20.0 + TILE_RESOLUTION_PX / 10.0,
            20.0,
            20.0 - TILE_RESOLUTION_PX / 0.5,
            10.0,
            10.0,
            10.0,
        ])));
        assert_same_normalized_features(
            &tile_geometries([(
                t14(0, 0),
                vec![
                    new_multi_polygon(vec![
                        new_polygon(&[19.875, 10.0, 20.0, 20.0, 10.0, 10.0, 19.875, 10.0]),
                        new_polygon(&[10.0, 30.0, 20.0, 20.0, 19.875, 30.0, 10.0, 30.0]),
                    ])
                    .into(),
                ],
            )]),
            &rendered,
        );
    }

    #[test]
    fn test_nested_multipolygon() {
        let rendered = polygon_14_tiny_min(new_multi_polygon(vec![
            new_polygon_with_holes(
                rectangle_coord_list(0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * 200.0),
                vec![rectangle_coord_list(0.5 + Z14_PX * 20.0, 0.5 + Z14_PX * 190.0)],
            ),
            rectangle(0.5 + Z14_PX * 30.0, 0.5 + Z14_PX * 180.0),
        ]));
        assert_same_normalized_features(
            &tile_geometries([(
                t14(0, 0),
                vec![
                    new_multi_polygon(vec![
                        new_polygon_with_holes(
                            rectangle_coord_list(10.0, 200.0),
                            vec![rectangle_coord_list(20.0, 190.0)],
                        ),
                        rectangle(30.0, 180.0),
                    ])
                    .into(),
                ],
            )]),
            &rendered,
        );
    }

    fn nested_fill_shell() -> Polygon<f64> {
        new_polygon_with_holes(
            rectangle_coord_list(0.5 - Z14_PX * 30.0, 0.5 + Z14_PX * (256.0 + 30.0)),
            vec![rectangle_coord_list(0.5 - Z14_PX * 20.0, 0.5 + Z14_PX * (256.0 + 20.0))],
        )
    }

    #[test]
    fn test_nested_multipolygon_fill() {
        let rendered = polygon_14_tiny_min(new_multi_polygon(vec![
            nested_fill_shell(),
            rectangle(0.5 - Z14_PX * 10.0, 0.5 + Z14_PX * (256.0 + 10.0)),
        ]));
        let inner = &rendered[&t14(0, 0)];
        assert_eq!(1, inner.len());
        assert_same_normalized_feature(&rect(-1.0, 256.0 + 1.0), &inner[0]);
    }

    #[test]
    fn test_nested_multipolygon_infers_outer_fill() {
        let rendered = polygon_14_tiny_min(new_multi_polygon(vec![
            nested_fill_shell(),
            new_polygon_with_holes(
                rectangle_coord_list(0.5 - Z14_PX * 10.0, 0.5 + Z14_PX * (256.0 + 10.0)),
                vec![rectangle_coord_list(0.5 + Z14_PX * 10.0, 0.5 + Z14_PX * (256.0 - 10.0))],
            ),
        ]));
        let inner = &rendered[&t14(0, 0)];
        assert_eq!(1, inner.len());
        assert_same_normalized_feature(
            &new_polygon_with_holes(rectangle_coord_list(-1.0, 256.0 + 1.0), vec![rectangle_coord_list(10.0, 246.0)])
                .into(),
            &inner[0],
        );
    }

    #[test]
    fn test_nested_multipolygon_cancels_out_inner_fill() {
        let rendered = polygon_14_tiny_min(new_multi_polygon(vec![
            nested_fill_shell(),
            new_polygon_with_holes(
                rectangle_coord_list(0.5 - Z14_PX * 10.0, 0.5 + Z14_PX * (256.0 + 10.0)),
                vec![rectangle_coord_list(0.5 - Z14_PX * 5.0, 0.5 + Z14_PX * (256.0 + 5.0))],
            ),
        ]));
        assert!(!rendered.contains_key(&t14(0, 0)));
    }

    #[test]
    fn test_overlapping_multipolygon() {
        let rendered = render_geometry(
            new_multi_polygon(vec![
                rectangle_xy(10.0 / 256.0, 10.0 / 256.0, 30.0 / 256.0, 30.0 / 256.0),
                rectangle_xy(20.0 / 256.0, 20.0 / 256.0, 40.0 / 256.0, 40.0 / 256.0),
            ]),
            Kind::Polygon,
            |f| {
                f.set_min_pixel_size(1.0).set_buffer_pixels(4.0).set_zoom_range(0, 0);
            },
        );
        assert_same_normalized_features(
            &tile_geometries([(
                txyz(0, 0, 0),
                vec![poly(&[
                    10.0, 10.0, 30.0, 10.0, 30.0, 20.0, 40.0, 20.0, 40.0, 40.0, 20.0, 40.0, 20.0, 30.0, 10.0, 30.0,
                    10.0, 10.0,
                ])],
            )]),
            &rendered,
        );
    }

    #[test]
    fn test_overlapping_multipolygon_side_by_side() {
        let rendered = render_geometry(
            new_multi_polygon(vec![
                rectangle_xy(10.0 / 256.0, 10.0 / 256.0, 20.0 / 256.0, 20.0 / 256.0),
                rectangle_xy(15.0 / 256.0, 10.0 / 256.0, 25.0 / 256.0, 20.0 / 256.0),
            ]),
            Kind::Polygon,
            |f| {
                f.set_min_pixel_size(1.0).set_buffer_pixels(4.0).set_zoom_range(0, 0);
            },
        );
        assert_topologically_equivalent_features(
            &tile_geometries([(txyz(0, 0, 0), vec![rect_xy(10.0, 10.0, 25.0, 20.0)])]),
            &rendered,
        );
    }

    #[test]
    fn test_polygon_wrap() {
        let rendered =
            render_geometry(rect_xy(-1.0 / 256.0, -1.0 / 256.0, 257.0 / 256.0, 1.0 / 256.0), Kind::Polygon, |f| {
                f.set_min_pixel_size(1.0).set_buffer_pixels(4.0).set_zoom_range(0, 1);
            });
        assert_topologically_equivalent_features(
            &tile_geometries([
                (txyz(0, 0, 0), vec![rect_xy(-4.0, -1.0, 260.0, 1.0)]),
                (txyz(0, 0, 1), vec![rect_xy(-4.0, -2.0, 260.0, 2.0)]),
                (txyz(1, 0, 1), vec![rect_xy(-4.0, -2.0, 260.0, 2.0)]),
            ]),
            &rendered,
        );
    }

    fn test_clip_with_rotation(rotation: f64, input_tile: Geometry<f64>) {
        let input = scale_translate(&input_tile, 1.0 / 256.0 / Z14_TILES as f64, 0.5, 0.5);
        let expected_output = polygon_overlay::intersection(&input_tile, &rect(-4.0, 260.0)).unwrap();
        let rendered = render_geometry(rotate_world(&input, rotation), Kind::Polygon, |f| {
            f.set_buffer_pixels(4.0).set_zoom_range(14, 14);
        });
        let geom = &rendered[&t14(0, 0)][0];
        assert_topologically_equivalent_feature(&round(&rotate_tile(&expected_output, rotation)), &round(geom));
    }

    #[test]
    fn test_back_and_forths_outside_tile() {
        for rotation in [0.0, 90.0, 180.0, -90.0] {
            test_clip_with_rotation(
                rotation,
                poly(&[
                    300.0, -10.0, 310.0, 300.0, 320.0, -10.0, 330.0, 300.0, 340.0, 400.0, 128.0, 400.0, 128.0, 128.0,
                    128.0, -10.0, 300.0, -10.0,
                ]),
            );
        }
    }

    const REPLAY_EDGES: [f64; 18] = [
        130.0, -10.0, 270.0, -10.0, 270.0, 270.0, -10.0, 270.0, -10.0, -10.0, 120.0, -10.0, 120.0, 10.0, 130.0, 10.0,
        130.0, -10.0,
    ];

    #[test]
    fn test_replay_edges_outer_poly() {
        for rotation in [0.0, 90.0, 180.0, -90.0] {
            test_clip_with_rotation(rotation, poly(&REPLAY_EDGES));
        }
    }

    #[test]
    fn test_replay_edges_inner_poly() {
        for rotation in [0.0, 90.0, 180.0, -90.0] {
            let mut inner = new_coordinate_list(&REPLAY_EDGES);
            inner.reverse();
            test_clip_with_rotation(
                rotation,
                new_polygon_with_holes(rectangle_coord_list(-20.0, 300.0), vec![inner]).into(),
            );
        }
    }

    #[test]
    fn test_spiral() {
        let offsets = [
            (0.0, 0.0),
            (0.5, 0.0),
            (0.5, 0.5),
            (0.0, 0.5),
            (-0.5, 0.5),
            (-0.5, 0.0),
            (-0.5, -0.5),
            (0.0, -0.5),
            (0.5, -0.5),
        ];
        for (dx, dy) in offsets {
            // generate spirals at different offsets and make sure that tile clipping returns the same result as an
            // intersection with the tile's boundary
            let mut coords = Vec::new();
            let outer_radius = 300.0;
            let iters = 25;
            for i in 0..iters {
                let radius = outer_radius - (i * 10) as f64;
                coords.extend([-radius, 0.0, 0.0, -radius, radius, 0.0, 0.0, radius]);
            }
            // JTS `buffer(1, 1)`: 1 segment per quadrant for round joins and caps
            let quadrant = std::f64::consts::FRAC_PI_2;
            let style = BufferStyle::new(1.0).line_join(LineJoin::Round(quadrant)).line_cap(LineCap::Round(quadrant));
            let poly: Geometry<f64> = Geometry::MultiPolygon(new_line_string(&coords).buffer_with_style(style));
            let poly = scale_translate(&poly, 1.0, 128.0 + dx * 256.0, 128.0 + dy * 256.0);
            let input = scale_translate(&poly, 1.0 / 256.0 / Z14_TILES as f64, 0.5, 0.5);
            let expected_output = polygon_overlay::intersection(&poly, &rect(-4.0, 260.0)).unwrap();
            let rendered = render_geometry(input, Kind::Polygon, |f| {
                f.set_buffer_pixels(4.0).set_zoom_range(14, 14);
            });
            let actual = &rendered[&t14(0, 0)][0];
            let expected =
                polygon_overlay::reduce_precision(&expected_output, &PrecisionModel::fixed(4096.0 / 256.0)).unwrap();
            assert_topologically_equivalent_feature(&expected, actual);
        }
    }

    #[test]
    fn test_linear_range_feature() {
        let rendered = render_features(
            ls(&[
                0.5 + Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0 + Z14_PX * 10.0,
                0.5 + Z14_WIDTH / 2.0 + Z14_PX * 10.0,
            ]),
            Kind::Line,
            |f| {
                f.linear_range(0.5, 1.0).set_attr("k", "v");
            },
        );
        assert_eq!(
            sorted(vec![
                (ls(&[128.0 + 5.0, 128.0 + 5.0, 128.0 + 10.0, 128.0 + 10.0]), tags! {"k" => "v"}),
                (ls(&[128.0, 128.0, 128.0 + 5.0, 128.0 + 5.0]), tags! {}),
            ]),
            sorted_features(&rendered[&t14(0, 0)])
        );
    }

    #[test]
    fn test_linear_range_feature_partial_minzoom() {
        for via_minzoom in [false, true] {
            let rendered = render_features(
                ls(&[
                    0.5 + Z13_WIDTH / 2.0,
                    0.5 + Z13_WIDTH / 2.0,
                    0.5 + Z13_WIDTH / 2.0 + Z13_PX * 10.0,
                    0.5 + Z13_WIDTH / 2.0 + Z13_PX * 10.0,
                ]),
                Kind::Line,
                |f| {
                    if via_minzoom {
                        f.linear_range(0.5, 1.0).set_min_zoom(14);
                    } else {
                        f.linear_range(0.5, 1.0).omit();
                    }
                },
            );
            assert_eq!(
                vec![(ls(&[128.0, 128.0, 128.0 + 5.0, 128.0 + 5.0]), tags! {})],
                sorted_features(&rendered[&t13(0, 0)])
            );
        }
    }

    #[test]
    fn test_geometry_pipeline() {
        let rendered = rendered_tile_features(
            ls(&[
                0.5 + Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0 + Z14_PX * 10.0,
                0.5 + Z14_WIDTH / 2.0,
            ]),
            Kind::Line,
            |f| {
                f.transform_scaled_geometry(geometry_pipeline::pipeline(|g| {
                    crate::geo::interior_point::centroid(g)
                        .map_or_else(geo_utils::empty_point, |c| Geometry::Point(c.into()))
                }))
                .set_attr("k", "v");
            },
            t14(0, 0),
        );
        assert_eq!(vec![(pt(128.0 + 5.0, 128.0), tags! {"k" => "v"})], rendered);
    }

    #[test]
    fn test_geometry_pipeline_simplify() {
        let rendered = rendered_tile_features(
            ls(&[
                0.5 + Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0,
                0.5 + Z14_WIDTH / 2.0 + Z14_PX * 10.0,
                0.5 + Z14_WIDTH / 2.0,
            ]),
            Kind::Line,
            |f| {
                f.transform_scaled_geometry(geometry_pipeline::compose(
                    Arc::new(geometry_pipeline::simplify_vw(1.0).set_weight(0.9)),
                    Arc::new(geometry_pipeline::simplify_dp(1.0)),
                ))
                .set_attr("k", "v");
            },
            t14(0, 0),
        );
        assert_eq!(vec![(ls(&[128.0, 128.0, 128.0 + 10.0, 128.0]), tags! {"k" => "v"})], rendered);
    }
}
