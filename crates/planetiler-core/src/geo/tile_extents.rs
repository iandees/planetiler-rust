//! Port of `geo/TileExtents.java`.

use std::fmt;
use std::sync::Arc;

use crate::geo::envelope::Envelope;
use crate::geo::tile_coord::TileCoord;

/// Predicate over tile `(x, y)` at a fixed zoom (Java's `TilePredicate`).
pub type TilePredicate = Arc<dyn Fn(u32, u32) -> bool + Send + Sync>;

/// Range of tiles to include at a zoom level: `min` inclusive, `max` exclusive, optionally limited to tiles that
/// intersect a shape.
#[derive(Clone)]
pub struct ForZoom {
    pub z: u32,
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
    pub shape_filter: Option<TilePredicate>,
}

impl fmt::Debug for ForZoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForZoom")
            .field("z", &self.z)
            .field("min_x", &self.min_x)
            .field("min_y", &self.min_y)
            .field("max_x", &self.max_x)
            .field("max_y", &self.max_y)
            .field("has_shape", &self.shape_filter.is_some())
            .finish()
    }
}

impl ForZoom {
    pub fn with_shape(&self, shape: TilePredicate) -> ForZoom {
        ForZoom { shape_filter: Some(shape), ..self.clone() }
    }

    pub fn test(&self, x: u32, y: u32) -> bool {
        self.test_x(x) && self.test_y(y) && self.shape_filter.as_ref().is_none_or(|f| f(x, y))
    }

    pub fn test_x(&self, x: u32) -> bool {
        x >= self.min_x && x < self.max_x
    }

    pub fn test_y(&self, y: u32) -> bool {
        y >= self.min_y && y < self.max_y
    }
}

/// A predicate that returns true for tiles within a bounding box (and optional shape) at every zoom level.
#[derive(Clone, Debug)]
pub struct TileExtents {
    zoom_extents: Vec<ForZoom>,
}

fn quantize_down(value: f64, levels: u32) -> u32 {
    ((value * levels as f64).floor() as i64).clamp(0, levels as i64) as u32
}

fn quantize_up(value: f64, levels: u32) -> u32 {
    ((value * levels as f64).ceil() as i64).clamp(0, levels as i64) as u32
}

impl TileExtents {
    /// Compute tile extents from a world-coordinate (0-1) bounding box.
    ///
    /// `shape_filter`, if present, is called once per zoom level with the [`ForZoom`] bounding box and returns the
    /// predicate for tiles that intersect the shape (Java uses `TiledGeometry.getCoveredTiles` for this; see
    /// `Bounds::tile_extents`).
    pub fn compute_from_world_bounds_with(
        maxzoom: u32,
        world_bounds: &Envelope,
        mut shape_filter: Option<&mut dyn FnMut(&ForZoom) -> TilePredicate>,
    ) -> TileExtents {
        let mut zoom_extents = Vec::with_capacity(maxzoom as usize + 1);
        for zoom in 0..=maxzoom {
            let max = 1u32 << zoom;
            let mut for_zoom = ForZoom {
                z: zoom,
                min_x: quantize_down(world_bounds.min_x, max),
                min_y: quantize_down(world_bounds.min_y, max),
                max_x: quantize_up(world_bounds.max_x, max),
                max_y: quantize_up(world_bounds.max_y, max),
                shape_filter: None,
            };
            if let Some(f) = shape_filter.as_mut() {
                let predicate = f(&for_zoom);
                for_zoom = for_zoom.with_shape(predicate);
            }
            zoom_extents.push(for_zoom);
        }
        TileExtents { zoom_extents }
    }

    pub fn compute_from_world_bounds(maxzoom: u32, world_bounds: &Envelope) -> TileExtents {
        Self::compute_from_world_bounds_with(maxzoom, world_bounds, None)
    }

    pub fn get_for_zoom(&self, zoom: u32) -> &ForZoom {
        &self.zoom_extents[zoom as usize]
    }

    pub fn test_xyz(&self, x: u32, y: u32, z: u32) -> bool {
        self.get_for_zoom(z).test(x, y)
    }

    pub fn test(&self, tile: &TileCoord) -> bool {
        self.test_xyz(tile.x, tile.y, tile.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_world() {
        let e = TileExtents::compute_from_world_bounds(14, &Envelope::new(0.0, 1.0, 0.0, 1.0));
        assert!(e.test_xyz(0, 0, 0));
        assert!(!e.test_xyz(1, 0, 0));
        assert!(e.test_xyz(3, 3, 2));
        assert!(!e.test_xyz(4, 3, 2));
    }

    #[test]
    fn partial() {
        let e = TileExtents::compute_from_world_bounds(14, &Envelope::new(0.25, 0.5, 0.25, 0.5));
        let z2 = e.get_for_zoom(2);
        assert_eq!((1, 1, 2, 2), (z2.min_x, z2.min_y, z2.max_x, z2.max_y));
        assert!(e.test_xyz(1, 1, 2));
        assert!(!e.test_xyz(2, 1, 2));
    }
}
