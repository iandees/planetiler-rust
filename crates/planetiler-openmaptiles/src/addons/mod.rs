//! Port of `addons/ExtraLayers.java`: registry of extra custom layers that you can add to the openmaptiles schema.

use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::layer::Layer;

/// Create structs that implement [`Layer`] in the `addons` module, then instantiate them here.
pub fn create(
    _translations: &Arc<Translations>,
    _config: &Arc<PlanetilerConfig>,
    _stats: &StatsRef,
) -> Vec<Arc<dyn Layer>> {
    vec![]
}
