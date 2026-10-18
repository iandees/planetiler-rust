//! Port of `planetiler-examples`: small example profiles that show how to use the planetiler API.
//!
//! Each example module exposes `main(args)` (Java `main(String...)`) and `run(arguments)`, plus the [`Profile`]
//! implementation.
//!
//! [`Profile`]: planetiler_core::profile::Profile

pub mod admin_borders_overlay;
pub mod bike_route_overlay;
pub mod osm_qa_tiles;
pub mod overture;
pub mod parks_overlay;
pub mod toilets_overlay;
pub mod toilets_overlay_low_level_api;
pub mod tram_route_overlay;
pub mod water_infrastructure_project;

pub(crate) use planetiler_core::feature_merge;

/// Attribution required whenever OpenStreetMap data is used.
pub const OSM_ATTRIBUTION: &str =
    r#"<a href="https://www.openstreetmap.org/copyright" target="_blank">&copy; OpenStreetMap contributors</a>"#;

/// Parses `args` like Java's `Arguments.fromArgsOrConfigFile`.
pub(crate) fn parse_args<S: AsRef<str>>(args: &[S]) -> anyhow::Result<planetiler_core::config::arguments::Arguments> {
    planetiler_core::config::arguments::Arguments::from_args_or_config_file(args)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Ports of the `TestUtils` helpers the example tests use.

    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use geo_types::Geometry;
    use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
    use planetiler_core::config::planetiler_config::PlanetilerConfig;
    use planetiler_core::feature_collector::{Factory, Feature};
    use planetiler_core::geo::geometry_type::GeometryType;
    use planetiler_core::mbtiles::mbtiles::Mbtiles;
    use planetiler_core::profile::Profile;
    use planetiler_core::reader::source_feature::SourceFeature;
    use planetiler_core::stats::stats::InMemoryStats;
    use planetiler_core::util::gzip::gunzip;
    use planetiler_core::value::Tags;
    use planetiler_core::vector_tile;

    /// Path to a test resource (examples resources first, then planetiler-core's).
    pub fn resource(name: &str) -> PathBuf {
        let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources").join(name);
        if local.exists() {
            local
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../planetiler-core/tests/resources").join(name)
        }
    }

    /// `TestUtils.processSourceFeature`: runs `profile` on `feature` and passes the emitted features to `f`.
    pub fn process_source_feature<R>(
        feature: &dyn SourceFeature,
        profile: &dyn Profile,
        f: impl FnOnce(Vec<&Feature<'_>>) -> R,
    ) -> R {
        let factory = Factory::new(Arc::new(PlanetilerConfig::defaults()), InMemoryStats::arc());
        let mut collector = factory.get(feature);
        profile.process_feature(feature, &mut collector);
        f(collector.iter().collect())
    }

    /// `Verify.getNumFeatures` (the envelope is ignored, like Java).
    pub fn num_features(mbtiles: &Mbtiles, layer: &str, zoom: u32, attrs: &Tags, geometry_type: GeometryType) -> usize {
        let mut num = 0;
        for tile in mbtiles.get_all_tiles().unwrap() {
            let tile = tile.unwrap();
            if tile.coord.z != zoom {
                continue;
            }
            let data = gunzip(&tile.bytes).unwrap_or_else(|_| tile.bytes.clone());
            for feature in vector_tile::decode(&data).unwrap() {
                if feature.layer == layer && attrs.iter().all(|(k, v)| feature.tags.get(k) == Some(v)) {
                    num += geometry_count(&feature.geometry.decode().unwrap(), geometry_type);
                }
            }
        }
        num
    }

    fn geometry_count(geometry: &Geometry<f64>, geometry_type: GeometryType) -> usize {
        match (geometry, geometry_type) {
            (Geometry::GeometryCollection(c), _) => c.iter().map(|g| geometry_count(g, geometry_type)).sum(),
            (Geometry::MultiPoint(m), GeometryType::Point) => m.0.len(),
            (Geometry::MultiLineString(m), GeometryType::Line) => m.0.len(),
            (Geometry::MultiPolygon(m), GeometryType::Polygon) => m.0.len(),
            (Geometry::Point(_), GeometryType::Point)
            | (Geometry::LineString(_), GeometryType::Line)
            | (Geometry::Polygon(_), GeometryType::Polygon) => 1,
            _ => 0,
        }
    }

    /// `TestUtils.assertNumFeatures`.
    #[track_caller]
    pub fn assert_num_features(
        mbtiles: &Mbtiles,
        layer: &str,
        zoom: u32,
        attrs: &Tags,
        expected: usize,
        geometry_type: GeometryType,
    ) {
        assert_eq!(expected, num_features(mbtiles, layer, zoom, attrs, geometry_type), "z{zoom} features in {layer}");
    }

    /// `TestUtils.assertTileDuplicates`: number of `tiles_data` rows with the same bytes as an earlier row.
    #[track_caller]
    pub fn assert_tile_duplicates(mbtiles: &Mbtiles, expected: usize) {
        let conn = mbtiles.connection();
        let mut stmt = conn.prepare("SELECT tile_data FROM tiles_data").unwrap();
        let tiles: Vec<Vec<u8>> = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        let mut seen = HashSet::new();
        let dups = tiles.iter().filter(|t| !seen.insert(t.as_slice())).count();
        assert_eq!(expected, dups, "{expected} duplicates expected, {dups} found");
    }

    /// Asserts that the `name` metadata entry matches and `attribution` contains the OSM copyright link.
    #[track_caller]
    pub fn assert_metadata(mbtiles: &Mbtiles, name: &str) {
        let metadata = mbtiles.metadata_table().get_all();
        assert_eq!(Some(name), metadata.get("name").map(String::as_str));
        let attribution = metadata.get("attribution").cloned().unwrap_or_default();
        assert!(attribution.contains("openstreetmap.org/copyright"), "{attribution}");
    }
}
