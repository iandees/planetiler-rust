//! Port of `mbtiles/Verify.java`: a utility to verify the contents of an mbtiles file.
//!
//! [`Verify::verify`] does a basic set of checks that the schema is correct and contains a "name" attribute and at
//! least one tile. Other code can add more checks to it.

use geo_types::Geometry;

use crate::archive::readable_tile_archive::ReadableTileArchive;
use crate::geo::envelope::Envelope;
use crate::geo::geometry_type::GeometryType;
use crate::geo::is_valid::is_simple_polygonal;
use crate::geo::tile_coord::TileCoord;
use crate::mbtiles::mbtiles::Mbtiles;
use crate::util::gzip::gunzip;
use crate::value::Tags;
use crate::vector_tile::{self, Feature};

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

fn decode(zipped: &[u8]) -> anyhow::Result<Vec<Feature>> {
    vector_tile::decode(&gunzip(zipped)?)
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

    /// Returns the number of geometries of `geometry_type` in features of `layer` at `zoom` that have all of `attrs`
    /// (Java's `Verify.getNumFeatures`). Like Java, `envelope` is currently ignored.
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
                    num += geometry_count(&feature.geometry.decode()?, geometry_type);
                }
            }
        }
        Ok(num)
    }

    /// Adds a check per zoom level from `minzoom` to `maxzoom` that succeeds if at least `min_count` features are found
    /// matching the criteria.
    #[allow(clippy::too_many_arguments)]
    pub fn check_min_feature_count_range(
        &mut self,
        bounds: &Envelope,
        layer: &str,
        tags: &Tags,
        minzoom: u32,
        maxzoom: u32,
        min_count: usize,
        geometry_type: GeometryType,
    ) {
        for z in minzoom..=maxzoom {
            self.check_min_feature_count(bounds, layer, tags, z, min_count, geometry_type);
        }
    }

    /// Adds a check that succeeds if at least `min_count` features are found matching the criteria.
    pub fn check_min_feature_count(
        &mut self,
        bounds: &Envelope,
        layer: &str,
        tags: &Tags,
        zoom: u32,
        min_count: usize,
        geometry_type: GeometryType,
    ) {
        let mbtiles = self.mbtiles;
        let name = format!("at least {min_count} {layer} {tags:?} features at z{zoom}");
        self.check_with_message(&name, || {
            let count = Self::get_num_features(mbtiles, layer, zoom, tags, bounds, geometry_type)?;
            Ok(if count >= min_count { None } else { Some(format!("found {count}")) })
        });
    }

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

/// Java `Verify.isValid`: polygons (including those inside collections) must be simple.
fn is_valid(geometry: &Geometry<f64>) -> bool {
    match geometry {
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) => is_simple_polygonal(geometry),
        Geometry::GeometryCollection(c) => c.iter().all(is_valid),
        _ => true,
    }
}

fn check_validity(coord: &TileCoord, data: &[u8]) -> Option<String> {
    let features = match decode(data) {
        Err(e) => return Some(format!("{coord} error decoding tile: {e}")),
        Ok(features) => features,
    };
    for feature in features {
        match feature.geometry.decode() {
            Ok(geometry) if !is_valid(&geometry) => return Some(format!("{coord}/{}", feature.layer)),
            Ok(_) => {}
            Err(_) => return Some(format!("{coord} error decoding {} feature", feature.layer)),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tile_encoding_result::TileEncodingResult;
    use crate::archive::writeable_tile_archive::WriteableTileArchive;
    use crate::geo::test_utils::{new_point, new_polygon};
    use crate::proto::vector_tile as vt;
    use crate::util::gzip::gzip_zlib as gzip;
    use crate::vector_tile::VectorTile;
    use prost::Message;

    fn write_tile(mbtiles: &Mbtiles, bytes: Vec<u8>) {
        let mut writer = mbtiles.new_tile_writer().unwrap();
        writer.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), bytes, None)).unwrap();
        writer.close().unwrap();
    }

    fn tile_with(geometry: Geometry<f64>, tags: Tags) -> Vec<u8> {
        let mut tile = VectorTile::new();
        tile.add_layer_features("layer", &[Feature::new("layer", 1, vector_tile::encode_geometry(&geometry), tags)]);
        gzip(&tile.encode()).unwrap()
    }

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

    #[test]
    fn test_invalid_geometry() {
        let mbtiles = Mbtiles::new_in_memory_database(true);
        mbtiles.create_tables_with_indexes().unwrap();
        mbtiles.metadata_table().set_metadata("name", "name");
        // self-intersecting bow-tie shape
        let bow_tie = new_polygon(&[0.0, 0.0, 10.0, 0.0, 0.0, 10.0, 10.0, 10.0, 0.0, 0.0]);
        write_tile(&mbtiles, tile_with(bow_tie.into(), Tags::default()));
        assert!(Verify::verify(&mbtiles).num_errors() > 0);
    }

    #[test]
    fn test_min_feature_count() {
        let mbtiles = Mbtiles::new_in_memory_database(true);
        mbtiles.create_tables_with_indexes().unwrap();
        mbtiles.metadata_table().set_metadata("name", "name");
        let mut tags = Tags::default();
        tags.insert("class".into(), "a".into());
        write_tile(&mbtiles, tile_with(new_point(1.0, 1.0).into(), tags.clone()));
        let bounds = Envelope::new(-180.0, 180.0, -85.0, 85.0);
        assert_eq!(1, Verify::get_num_features(&mbtiles, "layer", 0, &tags, &bounds, GeometryType::Point).unwrap());
        assert_eq!(0, Verify::get_num_features(&mbtiles, "layer", 0, &tags, &bounds, GeometryType::Line).unwrap());
        assert_eq!(0, Verify::get_num_features(&mbtiles, "other", 0, &tags, &bounds, GeometryType::Point).unwrap());

        let mut verify = Verify::verify(&mbtiles);
        assert_eq!(0, verify.num_errors(), "{:?}", verify.results());
        verify.check_min_feature_count(&bounds, "layer", &tags, 0, 1, GeometryType::Point);
        assert_eq!(0, verify.num_errors());
        verify.check_min_feature_count_range(&bounds, "layer", &Tags::default(), 0, 1, 1, GeometryType::Point);
        // z0 has the feature, z1 doesn't
        assert_eq!(1, verify.num_errors());
    }
}
