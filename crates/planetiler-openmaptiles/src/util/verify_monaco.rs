//! Port of `util/VerifyMonaco.java`: a utility to check the contents of an mbtiles file generated for Monaco.

use std::path::Path;

use geo_types::Geometry;
use planetiler_core::archive::readable_tile_archive::ReadableTileArchive;
use planetiler_core::geo::envelope::Envelope;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::mbtiles::verify::Verify;
use planetiler_core::util::gzip::gunzip;
use planetiler_core::value::Tags;
use planetiler_core::vector_tile;

/// Lat/lon bounds of Monaco.
pub const MONACO_BOUNDS: Envelope = Envelope { min_x: 7.40921, max_x: 7.44864, min_y: 43.72335, max_y: 43.75169 };

/// Returns a verification result with a basic set of checks against an openmaptiles map built from an extract for
/// Monaco.
pub fn verify(mbtiles: &Mbtiles) -> Verify<'_> {
    let mut verify = Verify::verify(mbtiles);
    check_min_feature_count_range(
        &mut verify,
        mbtiles,
        &MONACO_BOUNDS,
        "building",
        &Tags::default(),
        13,
        14,
        100,
        GeometryType::Polygon,
    );
    check_min_feature_count_range(
        &mut verify,
        mbtiles,
        &MONACO_BOUNDS,
        "transportation",
        &Tags::default(),
        10,
        14,
        5,
        GeometryType::Line,
    );
    let mut park = Tags::default();
    park.insert("class".into(), "grass".into());
    park.insert("subclass".into(), "park".into());
    check_min_feature_count(&mut verify, mbtiles, &MONACO_BOUNDS, "landcover", &park, 14, 10, GeometryType::Polygon);
    let mut ocean = Tags::default();
    ocean.insert("class".into(), "ocean".into());
    check_min_feature_count_range(
        &mut verify,
        mbtiles,
        &MONACO_BOUNDS,
        "water",
        &ocean,
        0,
        14,
        1,
        GeometryType::Polygon,
    );
    let mut country = Tags::default();
    country.insert("class".into(), "country".into());
    check_min_feature_count_range(
        &mut verify,
        mbtiles,
        &MONACO_BOUNDS,
        "place",
        &country,
        2,
        14,
        1,
        GeometryType::Point,
    );
    verify
}

/// `VerifyMonaco.main`: verifies the mbtiles file at `path`, prints the results and fails if there were any errors.
pub fn main(path: &Path) -> anyhow::Result<()> {
    let mbtiles = Mbtiles::new_read_only_database(path)?;
    let result = verify(&mbtiles);
    result.print();
    result.fail_if_errors()
}

// TODO(port): the following are `Verify.getNumFeatures`/`checkMinFeatureCount` from planetiler-core, which are not
// ported in `planetiler_core::mbtiles::verify` yet; move them there once it supports decoding features.

/// `Verify.checkMinFeatureCount(bounds, layer, tags, minzoom, maxzoom, minCount, geometryType)`.
#[allow(clippy::too_many_arguments)]
pub fn check_min_feature_count_range(
    verify: &mut Verify<'_>,
    mbtiles: &Mbtiles,
    bounds: &Envelope,
    layer: &str,
    tags: &Tags,
    minzoom: u32,
    maxzoom: u32,
    min_count: usize,
    geometry_type: GeometryType,
) {
    for z in minzoom..=maxzoom {
        check_min_feature_count(verify, mbtiles, bounds, layer, tags, z, min_count, geometry_type);
    }
}

/// `Verify.checkMinFeatureCount(bounds, layer, tags, zoom, minCount, geometryType)`: adds a check that succeeds if at
/// least `min_count` features are found matching the criteria.
#[allow(clippy::too_many_arguments)]
pub fn check_min_feature_count(
    verify: &mut Verify<'_>,
    mbtiles: &Mbtiles,
    bounds: &Envelope,
    layer: &str,
    tags: &Tags,
    zoom: u32,
    min_count: usize,
    geometry_type: GeometryType,
) {
    let name = format!("at least {min_count} {layer} {tags:?} features at z{zoom}");
    verify.check_with_message(&name, || {
        let count = get_num_features(mbtiles, layer, zoom, tags, bounds, geometry_type)?;
        Ok(if count >= min_count { None } else { Some(format!("found {count}")) })
    });
}

/// `Verify.getNumFeatures`: the number of geometries of `geometry_type` in features of `layer` at `zoom` that have all
/// of `attrs`. Like Java, `envelope` is currently ignored.
pub fn get_num_features(
    mbtiles: &Mbtiles,
    layer: &str,
    zoom: u32,
    attrs: &Tags,
    _envelope: &Envelope,
    geometry_type: GeometryType,
) -> anyhow::Result<usize> {
    let mut num = 0;
    for tile in mbtiles.get_all_tiles()? {
        let tile = tile?;
        if tile.coord.z != zoom {
            continue;
        }
        let data = gunzip(&tile.bytes).unwrap_or_else(|_| tile.bytes.clone());
        for feature in vector_tile::decode(&data)? {
            if feature.layer == layer && attrs.iter().all(|(k, v)| feature.tags.get(k) == Some(v)) {
                let geometry = feature.geometry.decode()?;
                num += geometry_count(&geometry, geometry_type);
            }
        }
    }
    Ok(num)
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

#[cfg(test)]
mod tests {
    use planetiler_core::archive::tile_encoding_result::TileEncodingResult;
    use planetiler_core::archive::writeable_tile_archive::WriteableTileArchive;
    use planetiler_core::geo::test_utils::new_point;
    use planetiler_core::geo::tile_coord::TileCoord;
    use planetiler_core::util::gzip::gzip;
    use planetiler_core::vector_tile::VectorTile;

    use super::*;

    fn assert_invalid(mbtiles: &Mbtiles) {
        assert!(verify(mbtiles).num_errors() > 0);
    }

    #[test]
    fn test_empty_file_invalid() {
        let mbtiles = Mbtiles::new_in_memory_database(false);
        assert_invalid(&mbtiles);
    }

    #[test]
    fn test_empty_tables_invalid() {
        let mbtiles = Mbtiles::new_in_memory_database(false);
        mbtiles.create_tables_with_indexes().unwrap();
        assert_invalid(&mbtiles);
    }

    #[test]
    fn test_still_invalid_with_one_tile() {
        let mbtiles = Mbtiles::new_in_memory_database(false);
        mbtiles.create_tables_with_indexes().unwrap();
        mbtiles.metadata_table().set_metadata("name", "name");
        {
            let mut writer = mbtiles.new_tile_writer().unwrap();
            let mut tile = VectorTile::new();
            tile.add_layer_features(
                "layer",
                &[vector_tile::Feature::new(
                    "layer",
                    1,
                    vector_tile::encode_geometry(&new_point(0.0, 0.0).into()),
                    Tags::default(),
                )],
            );
            writer
                .write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), gzip(&tile.encode()).unwrap(), None))
                .unwrap();
            writer.close().unwrap();
        }
        assert_invalid(&mbtiles);
    }
}
