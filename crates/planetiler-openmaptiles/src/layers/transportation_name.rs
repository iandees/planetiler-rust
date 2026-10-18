//! Port of `layers/TransportationName.java`.
//!
//! TODO(port): not ported yet (stub).

use std::sync::{Arc, OnceLock};

use crate::layers::transportation::Transportation;

use planetiler_core::config::planetiler_config::PlanetilerConfig;
use planetiler_core::forwarding_profile::Handler;
use planetiler_core::stats::stats::StatsRef;
use planetiler_core::util::translations::Translations;

use crate::generated::open_map_tiles_schema::transportation_name::*;
use crate::generated::tables::RowHandlers;
use crate::layer::Layer;

pub struct TransportationName {
    transportation: OnceLock<Arc<Transportation>>,
}

impl TransportationName {
    pub fn new(
        _translations: &Arc<Translations>,
        _config: &Arc<PlanetilerConfig>,
        _stats: &StatsRef,
    ) -> TransportationName {
        TransportationName { transportation: OnceLock::new() }
    }

    /// Gives this layer access to the transportation layer (route relations, minzoom/class logic).
    pub fn needs_transportation_layer(&self, transportation: Arc<Transportation>) {
        let _ = self.transportation.set(transportation);
    }
}

impl Handler for TransportationName {
    fn name(&self) -> Option<&str> {
        Some(LAYER_NAME)
    }
}

impl RowHandlers for TransportationName {}

impl Layer for TransportationName {}
