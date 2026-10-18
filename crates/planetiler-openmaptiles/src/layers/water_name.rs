//! Port of `layers/WaterName.java`.
//!
//! TODO(port): not ported yet (stub).

use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema::water_name::*;
use crate::generated::tables::RowHandlers;
use crate::layer::Layer;

pub struct WaterName {}

impl WaterName {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> WaterName {
        WaterName {}
    }
}

impl Handler for WaterName {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }
}

impl RowHandlers for WaterName {}

impl Layer for WaterName {}
