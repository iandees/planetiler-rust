//! Port of `FeatureProcessor.java`.

use crate::feature_collector::FeatureCollector;
use crate::reader::source_feature::SourceFeature;

/// Subcomponent of [`Profile`](crate::profile::Profile) that generates output features for input features.
///
/// Multiple threads may invoke this concurrently for a single data source so implementations should ensure
/// thread-safe access to any shared data structures. Separate data sources are processed sequentially.
///
/// All OSM nodes are processed first, then ways, then relations.
pub trait FeatureProcessor: Send + Sync {
    /// Generates output features for any input feature that should appear in the map.
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>);
}

impl<F: Fn(&dyn SourceFeature, &mut FeatureCollector<'_>) + Send + Sync> FeatureProcessor for F {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        self(source_feature, features)
    }
}
