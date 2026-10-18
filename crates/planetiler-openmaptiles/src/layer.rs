//! Port of `Layer.java`: the trait for all vector tile layer implementations that
//! [`OpenMapTilesProfile`](crate::open_map_tiles_profile::OpenMapTilesProfile) delegates to.
//!
//! A layer implements:
//! - [`Handler`] with [`Handler::name`] returning the layer name, plus the `as_*` accessors for the core capabilities
//!   it supports (layer post-processing, finish handling, OSM relation/node/way preprocessing, ...)
//! - [`RowHandlers`] returning the `*Handler` implementations for the imposm3 tables it subscribes to (Java
//!   `Tables.X.Handler` interfaces)
//! - [`Layer`] returning the OpenMapTiles-specific source processors it implements (Java `NaturalEarthProcessor`,
//!   `LakeCenterlineProcessor`, `OsmWaterPolygonProcessor`, `OsmAllProcessor`) and whether it ignores wikidata.

use std::any::Any;

use planetiler_core::forwarding_profile::Handler;

use crate::generated::tables::RowHandlers;
use crate::open_map_tiles_profile::{
    LakeCenterlineProcessor, NaturalEarthProcessor, OsmAllProcessor, OsmWaterPolygonProcessor,
};

/// A vector tile layer implementation. See the [module docs](self).
pub trait Layer: Handler + RowHandlers + Any + Send + Sync {
    /// Subscribe to elements from [natural earth](https://www.naturalearthdata.com/).
    fn as_natural_earth_processor(&self) -> Option<&dyn NaturalEarthProcessor> {
        None
    }

    /// Subscribe to elements from the [OSM lake centerlines source](https://github.com/openmaptiles/osm-lakelines).
    fn as_lake_centerline_processor(&self) -> Option<&dyn LakeCenterlineProcessor> {
        None
    }

    /// Subscribe to elements from the
    /// [OSM water polygons source](https://osmdata.openstreetmap.de/data/water-polygons.html).
    fn as_osm_water_polygon_processor(&self) -> Option<&dyn OsmWaterPolygonProcessor> {
        None
    }

    /// Subscribe to every OSM element.
    fn as_osm_all_processor(&self) -> Option<&dyn OsmAllProcessor> {
        None
    }

    /// Return true to indicate this layer does not need wikidata name translations, to avoid downloading more
    /// translations than are needed (Java `IgnoreWikidata` marker interface).
    fn ignore_wikidata(&self) -> bool {
        false
    }
}
