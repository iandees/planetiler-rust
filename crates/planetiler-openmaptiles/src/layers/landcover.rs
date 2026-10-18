//! Port of `layers/Landcover.java`.
//!
//! TODO(port): not ported yet (stub).

use std::sync::Arc;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema::landcover::*;
use crate::generated::tables::RowHandlers;
use crate::layer::Layer;

pub struct Landcover {}

impl Landcover {
    pub fn new(_translations: &Arc<Translations>, _config: &Arc<PlanetilerConfig>, _stats: &StatsRef) -> Landcover {
        Landcover {}
    }
}

impl Handler for Landcover {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }
}

impl RowHandlers for Landcover {}

impl Layer for Landcover {}
