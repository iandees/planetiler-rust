//! Rust port of [planetiler-openmaptiles](https://github.com/openmaptiles/planetiler-openmaptiles): an
//! [OpenMapTiles](https://openmaptiles.org/) vector tile schema profile for planetiler.
//!
//! - [`open_map_tiles_profile::OpenMapTilesProfile`] is the [`Profile`](planetiler_core::profile::Profile) that
//!   delegates to the per-layer implementations in [`layers`].
//! - [`open_map_tiles_main`] describes the input sources (OSM, water polygons, natural earth, lake centerlines) and
//!   their default download locations.
//! - [`generated`] holds the code generated from the OpenMapTiles schema by [`generate`].

pub mod addons;
pub mod generate;
pub mod generated;
pub mod layer;
pub mod layers;
pub mod open_map_tiles_main;
pub mod open_map_tiles_profile;
pub mod util;

pub(crate) use planetiler_core::feature_merge;

#[cfg(test)]
pub(crate) mod test_support;
