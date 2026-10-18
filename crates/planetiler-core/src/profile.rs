//! Port of `Profile.java`: provides methods for implementations to control how maps are generated.
//!
//! This includes:
//! - how source features (OSM elements, shapefile elements, etc.) map to output features and their tags
//! - how vector tile features in an output tile should be post-processed (see `FeatureMerge`)
//! - what attributes to include in the archive metadata output
//! - whether to fetch wikidata translations for an OSM element
//!
//! [`Profile::process_feature`] only handles a single element at a time. To "join" elements across or within sources,
//! implementations can store data in fields (behind a `Mutex` or similar) and wait to process them until an element
//! is encountered in a later source, or [`Profile::finish`] is called for a source. All methods may be called
//! concurrently by multiple threads.
//!
//! For complex profiles, [`ForwardingProfile`](crate::forwarding_profile::ForwardingProfile) provides a framework for
//! splitting the logic up into several handlers (i.e. one per layer).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::expression::expression::PartialInput;
use crate::feature_collector::{self, FeatureCollector};
use crate::geo::geometry_exception::GeometryResult;
use crate::geo::tile_coord::TileCoord;
use crate::reader::osm::osm_element::{Node, OsmElement, Relation, Way};
use crate::reader::osm::osm_relation_info::OsmRelationInfo;
use crate::reader::source_feature::SourceFeature;
use crate::vector_tile;

/// Default attribution recommended for profiles using OpenStreetMap data.
pub const OSM_ATTRIBUTION: &str =
    r#"<a href="https://www.openstreetmap.org/copyright" target="_blank">&copy; OpenStreetMap contributors</a>"#;

/// Output features of each layer in a tile, keyed (and iterated) by layer name.
pub type LayerFeatures = BTreeMap<String, Vec<vector_tile::Feature>>;

/// Callback that [`Profile::finish`] passes finished map features to.
pub type FeatureConsumer<'c> = dyn FnMut(&feature_collector::Feature<'_>) + 'c;

/// Controls how maps are generated. See the [module docs](self).
pub trait Profile: Send + Sync {
    /// Generates output features for any input feature that should appear in the map (see
    /// [`FeatureProcessor`](crate::feature_processor::FeatureProcessor)).
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>);

    /// Allows the profile to extract any information it needs from a node during the first pass through OSM elements.
    fn preprocess_osm_node(&self, _node: &Node) {}

    /// Allows the profile to extract any information it needs from a way during the first pass through OSM elements.
    fn preprocess_osm_way(&self, _way: &Way) {}

    /// Return true for this way to participate in way splitting at intersections. Profiles then ask for the split
    /// way instead of the full one using [`FeatureCollector::split_line`].
    fn split_osm_way_at_intersections(&self, _way: &Way) -> bool {
        false
    }

    /// Extracts information from OSM relations that will be passed along to [`Profile::process_feature`] for any OSM
    /// element in that relation (see
    /// [`SourceFeatureExt::relation_info`](crate::reader::source_feature::SourceFeatureExt::relation_info)).
    ///
    /// The result is stored in memory. Return `None` to ignore the relation.
    fn preprocess_osm_relation(&self, _relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        None
    }

    /// Free any resources associated with this profile (i.e. shared data structures).
    fn release(&self) {}

    /// Apply any post-processing to features in an output layer of a tile before writing it to the output file.
    ///
    /// These transformations may add, remove, or change the tags, geometry, or ordering of output features based on
    /// other features present in this tile. Modify `items` in place (or replace it). If this returns an error, the
    /// framework logs it and emits `items` in whatever state they were left in, so implementations should only
    /// replace `items` once they have computed the full result (Java returns `null` to leave input unchanged).
    ///
    /// Many threads invoke this concurrently. The default implementation leaves features unaltered.
    fn post_process_layer_features(
        &self,
        _layer: &str,
        _zoom: u32,
        _items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        Ok(())
    }

    /// Apply any post-processing to layers in an output tile before writing it to the output.
    ///
    /// This is called before [`Profile::post_process_layer_features`] gets called for each layer. Use this if
    /// features in one layer should influence features in another layer, to create new layers from existing ones,
    /// or to remove a layer entirely from the output.
    ///
    /// Many threads invoke this concurrently. The default implementation leaves features unaltered.
    fn post_process_tile_features(&self, _tile_coord: TileCoord, _layers: &mut LayerFeatures) -> GeometryResult<()> {
        Ok(())
    }

    /// The name of the generated tileset to put into the archive metadata. Defaults to the type name.
    fn name(&self) -> String {
        let full = std::any::type_name_of_val(self);
        let without_generics = full.split('<').next().unwrap_or(full);
        without_generics.rsplit("::").next().unwrap_or(without_generics).to_string()
    }

    /// The description of the generated tileset to put into the archive metadata.
    fn description(&self) -> Option<String> {
        None
    }

    /// The attribution of the generated tileset to put into the archive metadata.
    fn attribution(&self) -> Option<String> {
        None
    }

    /// The version of the generated tileset to put into the archive metadata.
    fn version(&self) -> Option<String> {
        None
    }

    /// Returns `true` to set `type="overlay"` in the archive metadata otherwise sets `type="baselayer"`.
    fn is_overlay(&self) -> bool {
        false
    }

    /// Extra key/value pairs to put into the archive metadata.
    fn extra_archive_metadata(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// Whether wikidata translations should be fetched for the input element. Defaults to `true`.
    fn cares_about_wikidata_translation(&self, _elem: &OsmElement) -> bool {
        true
    }

    /// Invoked once for each source after all elements for that source have been processed.
    ///
    /// Create new collectors with `feature_collectors.get(&source_feature)`, and pass finished features to `next`.
    fn finish(
        &self,
        _source_name: &str,
        _feature_collectors: &feature_collector::Factory,
        _next: &mut FeatureConsumer<'_>,
    ) {
    }

    /// Returns true if this profile will use any of the elements from an input source. Defaults to `true`.
    fn cares_about_source(&self, _name: &str) -> bool {
        true
    }

    /// Estimate of how many bytes of disk this profile will use for intermediate feature storage.
    fn estimate_intermediate_disk_bytes(&self, _osm_file_size: u64) -> u64 {
        0
    }

    /// Estimate of how many bytes the output file will be.
    fn estimate_output_bytes(&self, _osm_file_size: u64) -> u64 {
        0
    }

    /// Estimate of how many bytes of RAM this will use (including relations and multipolygons stored in memory).
    fn estimate_ram_required(&self, _osm_file_size: u64) -> u64 {
        0
    }

    /// Returns false if this profile will ignore every feature in a set where partial attributes are known ahead of
    /// time.
    fn cares_about(&self, _input: &PartialInput) -> bool {
        true
    }
}

impl<P: Profile + ?Sized> Profile for Arc<P> {
    fn process_feature(&self, source_feature: &dyn SourceFeature, features: &mut FeatureCollector<'_>) {
        (**self).process_feature(source_feature, features)
    }
    fn preprocess_osm_node(&self, node: &Node) {
        (**self).preprocess_osm_node(node)
    }
    fn preprocess_osm_way(&self, way: &Way) {
        (**self).preprocess_osm_way(way)
    }
    fn split_osm_way_at_intersections(&self, way: &Way) -> bool {
        (**self).split_osm_way_at_intersections(way)
    }
    fn preprocess_osm_relation(&self, relation: &Relation) -> Option<Vec<Arc<dyn OsmRelationInfo>>> {
        (**self).preprocess_osm_relation(relation)
    }
    fn release(&self) {
        (**self).release()
    }
    fn post_process_layer_features(
        &self,
        layer: &str,
        zoom: u32,
        items: &mut Vec<vector_tile::Feature>,
    ) -> GeometryResult<()> {
        (**self).post_process_layer_features(layer, zoom, items)
    }
    fn post_process_tile_features(&self, tile_coord: TileCoord, layers: &mut LayerFeatures) -> GeometryResult<()> {
        (**self).post_process_tile_features(tile_coord, layers)
    }
    fn name(&self) -> String {
        (**self).name()
    }
    fn description(&self) -> Option<String> {
        (**self).description()
    }
    fn attribution(&self) -> Option<String> {
        (**self).attribution()
    }
    fn version(&self) -> Option<String> {
        (**self).version()
    }
    fn is_overlay(&self) -> bool {
        (**self).is_overlay()
    }
    fn extra_archive_metadata(&self) -> BTreeMap<String, String> {
        (**self).extra_archive_metadata()
    }
    fn cares_about_wikidata_translation(&self, elem: &OsmElement) -> bool {
        (**self).cares_about_wikidata_translation(elem)
    }
    fn finish(
        &self,
        source_name: &str,
        feature_collectors: &feature_collector::Factory,
        next: &mut FeatureConsumer<'_>,
    ) {
        (**self).finish(source_name, feature_collectors, next)
    }
    fn cares_about_source(&self, name: &str) -> bool {
        (**self).cares_about_source(name)
    }
    fn estimate_intermediate_disk_bytes(&self, osm_file_size: u64) -> u64 {
        (**self).estimate_intermediate_disk_bytes(osm_file_size)
    }
    fn estimate_output_bytes(&self, osm_file_size: u64) -> u64 {
        (**self).estimate_output_bytes(osm_file_size)
    }
    fn estimate_ram_required(&self, osm_file_size: u64) -> u64 {
        (**self).estimate_ram_required(osm_file_size)
    }
    fn cares_about(&self, input: &PartialInput) -> bool {
        (**self).cares_about(input)
    }
}

/// A default implementation of [`Profile`] that emits no output elements.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullProfile;

impl Profile for NullProfile {
    fn process_feature(&self, _source_feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {}

    fn name(&self) -> String {
        "Null".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MyProfile;
    impl Profile for MyProfile {
        fn process_feature(&self, _source_feature: &dyn SourceFeature, _features: &mut FeatureCollector<'_>) {}
    }

    #[test]
    fn names() {
        assert_eq!("Null", NullProfile.name());
        assert_eq!("MyProfile", MyProfile.name());
        let arc: Arc<dyn Profile> = Arc::new(MyProfile);
        assert_eq!("MyProfile", arc.name());
        assert!(OSM_ATTRIBUTION.starts_with("<a href"));
    }
}
