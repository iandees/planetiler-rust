//! Port of `mbtiles/Verify.java`: a utility to verify the contents of an mbtiles file.
//!
//! [`Verify::verify`] does a basic set of checks that the schema is correct and contains a "name" attribute and at
//! least one tile. Other code can add more checks to it.

use prost::Message;

use crate::archive::readable_tile_archive::ReadableTileArchive;
use crate::geo::tile_coord::TileCoord;
use crate::mbtiles::mbtiles::Mbtiles;
use crate::pmtiles::pmtiles::gunzip;
use crate::proto::vector_tile as vt;

const GOOD: &str = "\u{1B}[32m✓\u{1B}[0m";
const BAD: &str = "\u{1B}[31m✕\u{1B}[0m";

/// Result of one check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub error: Option<String>,
}

pub struct Verify<'a> {
    checks: Vec<Check>,
    mbtiles: &'a Mbtiles,
}

fn decode(zipped: &[u8]) -> anyhow::Result<vt::Tile> {
    Ok(vt::Tile::decode(gunzip(zipped)?.as_slice())?)
}

impl<'a> Verify<'a> {
    /// Returns a verification result of a basic set of checks on an mbtiles file:
    /// - has a metadata and tiles table
    /// - has a name metadata attribute
    /// - has at least one tile
    /// - all vector tiles can be decoded
    #[allow(clippy::self_named_constructors)]
    pub fn verify(mbtiles: &'a Mbtiles) -> Verify<'a> {
        let mut result = Verify { checks: Vec::new(), mbtiles };
        result.check_basic_structure();
        result
    }

    /// CLI entrypoint equivalent of `Verify.main`: verifies `path`, prints results and returns the number of errors.
    pub fn main(path: &std::path::Path) -> anyhow::Result<usize> {
        let mbtiles = Mbtiles::new_read_only_database(path)?;
        let result = Verify::verify(&mbtiles);
        result.print();
        let errors = result.num_errors();
        println!("{errors} errors");
        Ok(errors)
    }

    fn check_basic_structure(&mut self) {
        let mbtiles = self.mbtiles;
        self.check("contains name attribute", || {
            Ok(ReadableTileArchive::metadata(mbtiles)?.is_some_and(|m| m.to_map().contains_key("name")))
        });
        self.check("contains at least one tile", || Ok(mbtiles.get_all_tile_coords()?.next().transpose()?.is_some()));
        self.check_with_message("all tiles are valid", || {
            let mut invalid_tiles = Vec::new();
            for tile in mbtiles.get_all_tiles()? {
                let tile = tile?;
                if let Some(error) = check_validity(&tile.coord, &tile.bytes) {
                    invalid_tiles.push(error);
                }
            }
            Ok(if invalid_tiles.is_empty() {
                None
            } else {
                Some(format!(
                    "{} invalid tiles: [{}]",
                    invalid_tiles.len(),
                    invalid_tiles.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                ))
            })
        });
    }

    // TODO(port): `getNumFeatures` / `checkMinFeatureCount(bounds, layer, tags, zoom(s), minCount, geometryType)`
    // require decoding vector tile geometries (`VectorTile.decode` + `VectorGeometry.decode`), which is being ported
    // concurrently in `vector_tile.rs`.

    /// Logs verification results.
    pub fn print(&self) {
        for check in &self.checks {
            match &check.error {
                Some(error) => println!("{BAD} {}: {error}", check.name),
                None => println!("{GOOD} {}", check.name),
            }
        }
    }

    /// Returns an error if there were any failures (Java `failIfErrors` exits with status 1 instead).
    pub fn fail_if_errors(&self) -> anyhow::Result<()> {
        let errors = self.num_errors();
        println!("{errors} errors");
        if errors > 0 {
            anyhow::bail!("{errors} verification errors");
        }
        Ok(())
    }

    pub fn check_with_message(&mut self, name: &str, check: impl FnOnce() -> anyhow::Result<Option<String>>) {
        let error = match check() {
            Ok(error) => error,
            Err(e) => Some(e.to_string()),
        };
        self.checks.push(Check { name: name.to_string(), error });
    }

    pub fn check(&mut self, name: &str, check: impl FnOnce() -> anyhow::Result<bool>) {
        self.check_with_message(name, || Ok(if check()? { None } else { Some("false".into()) }));
    }

    pub fn results(&self) -> &[Check] {
        &self.checks
    }

    pub fn num_errors(&self) -> usize {
        self.checks.iter().filter(|c| c.error.is_some()).count()
    }
}

fn check_validity(coord: &TileCoord, data: &[u8]) -> Option<String> {
    match decode(data) {
        Err(e) => Some(format!("{coord} error decoding tile: {e}")),
        // TODO(port): also decode each feature geometry and check that polygons are simple (Java `isValid`) once
        // `VectorTile` geometry decoding is ported.
        Ok(_tile) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tile_encoding_result::TileEncodingResult;
    use crate::archive::writeable_tile_archive::WriteableTileArchive;
    use crate::pmtiles::pmtiles::gzip;

    fn point_tile() -> Vec<u8> {
        let tile = vt::Tile {
            layers: vec![vt::tile::Layer {
                version: 2,
                name: "layer".into(),
                features: vec![vt::tile::Feature {
                    id: Some(1),
                    tags: vec![],
                    r#type: Some(vt::tile::GeomType::Point as i32),
                    geometry: vec![9, 0, 0],
                }],
                keys: vec![],
                values: vec![],
                extent: Some(4096),
            }],
        };
        gzip(&tile.encode_to_vec()).unwrap()
    }

    #[test]
    fn test_empty_file_invalid() {
        let mbtiles = Mbtiles::new_in_memory_database(true);
        assert!(Verify::verify(&mbtiles).num_errors() > 0);
    }

    #[test]
    fn test_empty_tables_invalid() {
        let mbtiles = Mbtiles::new_in_memory_database(true);
        mbtiles.create_tables_with_indexes().unwrap();
        assert!(Verify::verify(&mbtiles).num_errors() > 0);
    }

    #[test]
    fn test_valid_with_name_and_one_tile() {
        let mbtiles = Mbtiles::new_in_memory_database(true);
        mbtiles.create_tables_with_indexes().unwrap();
        mbtiles.metadata_table().set_metadata("name", "name");
        {
            let mut writer = mbtiles.new_tile_writer().unwrap();
            writer.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), point_tile(), None)).unwrap();
            writer.close().unwrap();
        }
        let result = Verify::verify(&mbtiles);
        assert_eq!(0, result.num_errors(), "{:?}", result.results());
    }

    #[test]
    fn test_invalid_tile_data() {
        let mbtiles = Mbtiles::new_in_memory_database(true);
        mbtiles.create_tables_with_indexes().unwrap();
        mbtiles.metadata_table().set_metadata("name", "name");
        {
            let mut writer = mbtiles.new_tile_writer().unwrap();
            writer.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), vec![1, 2, 3], None)).unwrap();
            writer.close().unwrap();
        }
        assert_eq!(1, Verify::verify(&mbtiles).num_errors());
    }

    // TODO(port): testInvalidGeometry (self-intersecting polygon) once geometry validity checks are ported.
}
