//! Port of `stream/StreamArchiveConfig.java`.

use crate::config::arguments::Arguments;
use crate::config::planetiler_config::PlanetilerConfig;

#[derive(Clone, Debug, Default)]
pub struct StreamArchiveConfig {
    pub append_to_file: bool,
    pub more_options: Arguments,
}

impl StreamArchiveConfig {
    pub fn new(append_to_file: bool, more_options: Arguments) -> StreamArchiveConfig {
        StreamArchiveConfig { append_to_file, more_options }
    }

    pub fn from_config(planetiler_config: &PlanetilerConfig, more_options: Arguments) -> StreamArchiveConfig {
        StreamArchiveConfig::new(planetiler_config.append, more_options)
    }
}
