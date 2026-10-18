//! Port of `config/Bounds.java`: the latitude/longitude bounds (and optional shape) to render.

use std::sync::Arc;

use geo_types::Geometry;

use crate::config::planetiler_config::MAX_MAXZOOM;
use crate::geo::envelope::Envelope;
use crate::geo::geo_utils;
use crate::geo::tile_extents::TileExtents;

/// Something that can provide the bounds of its data, for example an OSM file header.
pub trait BoundsProvider {
    fn get_lat_lon_bounds(&self) -> Option<Envelope>;
}

/// Bounds of the map to render. `None` lat/lon means the whole world.
#[derive(Clone, Debug, Default)]
pub struct Bounds {
    lat_lon: Option<Envelope>,
    world: Option<Envelope>,
    tile_extents: Option<Arc<TileExtents>>,
    shape: Option<Arc<Geometry<f64>>>,
}

impl Bounds {
    pub fn world() -> Bounds {
        Bounds::default()
    }

    pub fn new(lat_lon: Option<Envelope>) -> Bounds {
        let mut b = Bounds::default();
        b.set(lat_lon);
        b
    }

    pub fn lat_lon(&self) -> Envelope {
        self.lat_lon.unwrap_or(*geo_utils::WORLD_LAT_LON_BOUNDS)
    }

    /// The explicitly-set (or inferred) lat/lon bounds, `None` if not set yet.
    pub fn lat_lon_opt(&self) -> Option<Envelope> {
        self.lat_lon
    }

    pub fn world_bounds(&self) -> Envelope {
        self.world.unwrap_or(geo_utils::WORLD_BOUNDS)
    }

    pub fn is_world(&self) -> bool {
        self.lat_lon.is_none_or(|l| l == *geo_utils::WORLD_LAT_LON_BOUNDS)
    }

    pub fn shape(&self) -> Option<&Geometry<f64>> {
        self.shape.as_deref()
    }

    pub fn tile_extents(&mut self) -> Arc<TileExtents> {
        if self.tile_extents.is_none() {
            self.tile_extents = Some(Arc::new(self.compute_tile_extents()));
        }
        self.tile_extents.clone().unwrap()
    }

    fn compute_tile_extents(&self) -> TileExtents {
        TileExtents::compute_from_world_bounds_with_shape(MAX_MAXZOOM, &self.world_bounds(), self.shape.as_deref())
    }

    pub fn add_fallback_provider(&mut self, provider: &dyn BoundsProvider) -> &mut Self {
        if self.lat_lon.is_none()
            && let Some(bounds) = provider.get_lat_lon_bounds()
            && !bounds.is_null()
            && bounds.area() > 0.0
        {
            tracing::info!("Setting map bounds from input: {}", geo_utils::envelope_to_string(&bounds));
            self.set(Some(bounds));
        }
        self
    }

    pub fn set_shape(&mut self, shape: Geometry<f64>) -> &mut Self {
        let env = Envelope::of_geometry(&shape);
        self.shape = Some(Arc::new(shape));
        if self.lat_lon.is_none() {
            self.set(Some(env));
        } else {
            self.tile_extents = None;
        }
        self
    }

    fn set(&mut self, lat_lon: Option<Envelope>) {
        if let Some(lat_lon) = lat_lon {
            self.lat_lon = Some(lat_lon);
            self.world = Some(geo_utils::to_world_bounds(&lat_lon));
            self.tile_extents = None;
        }
    }
}
