//! Port of `layers/Water.java`.
//!
//! TODO(port): not ported yet (stub).

use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema::water::*;
use crate::generated::tables::RowHandlers;
use crate::layer::Layer;

pub struct Water {}

impl Water {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> Water {
        Water {}
    }
}

impl Handler for Water {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }
}

impl RowHandlers for Water {}

impl Layer for Water {}
