//! Port of `layers/Aeroway.java`.
//!
//! TODO(port): not ported yet (stub).

use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema::aeroway::*;
use crate::generated::tables::RowHandlers;
use crate::layer::Layer;

pub struct Aeroway {}

impl Aeroway {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> Aeroway {
        Aeroway {}
    }
}

impl Handler for Aeroway {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }
}

impl RowHandlers for Aeroway {}

impl Layer for Aeroway {}
