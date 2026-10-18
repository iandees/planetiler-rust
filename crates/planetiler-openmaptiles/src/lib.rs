//! Rust port of [planetiler-openmaptiles](https://github.com/openmaptiles/planetiler-openmaptiles): an
//! [OpenMapTiles](https://openmaptiles.org/) vector tile schema profile for planetiler.
//!
//! - [`open_map_tiles_profile::OpenMapTilesProfile`] is the [`Profile`](planetiler_core::profile::Profile) that
//!   delegates to the per-layer implementations in [`layers`].
//! - [`open_map_tiles_main`] describes the input sources (OSM, water polygons, natural earth, lake centerlines) and
//!   their default download locations.
//! - [`generated`] holds the code generated from the OpenMapTiles schema by [`generate`].

pub mod addons;
mod feature_merge_shim;
pub mod generate;
pub mod generated;
pub mod layer;
pub mod layers;
pub mod open_map_tiles_main;
pub mod open_map_tiles_profile;
pub mod util;

/// TODO(port): FeatureMerge is being ported into `planetiler_core::feature_merge` on another branch; until it lands
/// this crate uses a copy with the same API. Replace with `pub(crate) use planetiler_core::feature_merge;`.
#[allow(unused_imports)]
pub(crate) use feature_merge_shim as feature_merge;

#[cfg(test)]
pub(crate) mod test_support;
