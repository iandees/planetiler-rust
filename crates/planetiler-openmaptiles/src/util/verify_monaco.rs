//! Port of `util/VerifyMonaco.java`: a utility to check the contents of an mbtiles file generated for Monaco.

use std::path::Path;

use planetiler_core::geo::envelope::Envelope;
use planetiler_core::geo::geometry_type::GeometryType;
use planetiler_core::mbtiles::mbtiles::Mbtiles;
use planetiler_core::mbtiles::verify::Verify;
use planetiler_core::value::Tags;

/// Lat/lon bounds of Monaco.
pub const MONACO_BOUNDS: Envelope = Envelope { min_x: 7.40921, max_x: 7.44864, min_y: 43.72335, max_y: 43.75169 };

/// Returns a verification result with a basic set of checks against an openmaptiles map built from an extract for
/// Monaco.
pub fn verify(mbtiles: &Mbtiles) -> Verify<'_> {
    let mut verify = Verify::verify(mbtiles);
    verify.check_min_feature_count_range(
        &MONACO_BOUNDS,
        "building",
        &Tags::default(),
        13,
        14,
        100,
        GeometryType::Polygon,
    );
    verify.check_min_feature_count_range(
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
    verify.check_min_feature_count(&MONACO_BOUNDS, "landcover", &park, 14, 10, GeometryType::Polygon);
    let mut ocean = Tags::default();
    ocean.insert("class".into(), "ocean".into());
    verify.check_min_feature_count_range(&MONACO_BOUNDS, "water", &ocean, 0, 14, 1, GeometryType::Polygon);
    let mut country = Tags::default();
    country.insert("class".into(), "country".into());
    verify.check_min_feature_count_range(&MONACO_BOUNDS, "place", &country, 2, 14, 1, GeometryType::Point);
    verify
}

/// `VerifyMonaco.main`: verifies the mbtiles file at `path`, prints the results and fails if there were any errors.
pub fn main(path: &Path) -> anyhow::Result<()> {
    let mbtiles = Mbtiles::new_read_only_database(path)?;
    let result = verify(&mbtiles);
    result.print();
    result.fail_if_errors()
}

#[cfg(test)]
mod tests {
    use planetiler_core::archive::tile_encoding_result::TileEncodingResult;
    use planetiler_core::archive::writeable_tile_archive::WriteableTileArchive;
    use planetiler_core::geo::test_utils::new_point;
    use planetiler_core::geo::tile_coord::TileCoord;
    use planetiler_core::util::gzip::gzip;
    use planetiler_core::vector_tile::{self, VectorTile};

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
