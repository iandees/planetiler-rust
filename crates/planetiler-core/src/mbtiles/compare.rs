//! Port of `mbtiles/Compare.java`: a utility to compare two mbtiles files feature-by-feature.
//!
//! Feature ids are not compared and the `rank` attribute is ignored since the value planetiler produces is not
//! stable.
//!
//! Like Java, geometries are compared after decoding and normalizing them (`geometry().decode().norm()`), so
//! geometries that only differ in ring start point or orientation are equal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::bail;
use tracing::{info, warn};

use crate::geo::normalize::norm;
use crate::geo::tile_coord::TileCoord;
use crate::geo::wkt::to_wkt;
use crate::mbtiles::mbtiles::Mbtiles;
use crate::util::gzip::gunzip;
use crate::vector_tile;

/// Comparable representation of a vector tile feature: the layer, normalized geometry (as WKT so it can be ordered)
/// and attributes other than `rank`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct VectorTileFeatureForCmp {
    layer: String,
    normalized_geometry: String,
    attrs: BTreeMap<String, String>,
}

fn decode_features(data: Option<Vec<u8>>) -> anyhow::Result<BTreeSet<VectorTileFeatureForCmp>> {
    let Some(data) = data else { return Ok(BTreeSet::new()) };
    let mut result = BTreeSet::new();
    for feature in vector_tile::decode(&gunzip(&data)?)? {
        let attrs = feature.tags.iter().filter(|(k, _)| *k != "rank").map(|(k, v)| (k.to_string(), format!("{v:?}")));
        result.insert(VectorTileFeatureForCmp {
            normalized_geometry: to_wkt(&norm(&feature.geometry.decode()?)),
            layer: feature.layer,
            attrs: attrs.collect(),
        });
    }
    Ok(result)
}

fn get_tiles_count(db: &Mbtiles) -> anyhow::Result<i64> {
    let conn = db.connection();
    match conn.query_row("select count(*) from tiles_shallow", [], |r| r.get(0)) {
        Ok(count) => Ok(count),
        Err(_) => Ok(conn.query_row("select count(*) from tiles", [], |r| r.get(0))?),
    }
}

fn diff_message(
    coord: &TileCoord,
    features0: &BTreeSet<VectorTileFeatureForCmp>,
    features1: &BTreeSet<VectorTileFeatureForCmp>,
) -> String {
    let joined = |a: &BTreeSet<VectorTileFeatureForCmp>, b: &BTreeSet<VectorTileFeatureForCmp>| {
        a.difference(b).map(|f| format!("{f:?}")).collect::<Vec<_>>().join("\n")
    };
    format!(
        "<<<\nfeature diff on coord {coord} - featureCountMatches: {} ({} vs {})\n\nadditional in db0\n---\n{}\n\n\
         additional in db1\n---\n{}\n>>>\n",
        features0.len() == features1.len(),
        features0.len(),
        features1.len(),
        joined(features0, features1),
        joined(features1, features0)
    )
}

/// Compares two mbtiles files, returning `(total tiles, tiles with diffs)`. Fails on the first diff if
/// `fail_on_feature_diff`.
pub fn compare(db_path0: &Path, db_path1: &Path, fail_on_feature_diff: bool) -> anyhow::Result<(u64, u64)> {
    let db0 = Mbtiles::new_read_only_database(db_path0)?;
    let db1 = Mbtiles::new_read_only_database(db_path1)?;
    let tiles_count0 = get_tiles_count(&db0)?;
    let tiles_count1 = get_tiles_count(&db1)?;
    if tiles_count0 != tiles_count1 {
        bail!("expected tiles count to be equal but tilesCount0={tiles_count0} tilesCount1={tiles_count1}");
    }
    let coords: Vec<TileCoord> = {
        use crate::archive::readable_tile_archive::ReadableTileArchive;
        db0.get_all_tile_coords()?.collect::<anyhow::Result<_>>()?
    };
    let mut last_percentage = -1;
    let mut processed = 0u64;
    let mut tiles_with_diffs = 0u64;
    for coord in coords {
        use crate::archive::readable_tile_archive::ReadableTileArchive;
        processed += 1;
        let current_percentage = (processed * 100 / tiles_count0.max(1) as u64) as i64;
        if last_percentage != current_percentage {
            info!("processed {current_percentage}%");
        }
        last_percentage = current_percentage;
        let features0 = decode_features(db0.get_tile_coord(&coord)?)?;
        let features1 = decode_features(db1.get_tile_coord(&coord)?)?;
        if features0 != features1 {
            tiles_with_diffs += 1;
            let message = diff_message(&coord, &features0, &features1);
            if fail_on_feature_diff {
                bail!(message);
            } else {
                warn!("{message}");
            }
        }
    }
    info!("totalTiles={processed} tilesWithDiffs={tiles_with_diffs}");
    Ok((processed, tiles_with_diffs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tile_encoding_result::TileEncodingResult;
    use crate::archive::writeable_tile_archive::WriteableTileArchive;
    use crate::config::arguments::Arguments;
    use crate::proto::vector_tile as vt;
    use crate::util::gzip::gzip_zlib as gzip;
    use prost::Message;

    fn tile(value: &str) -> Vec<u8> {
        tile_with_geometry(value, vec![9, 0, 0])
    }

    fn tile_with_geometry(value: &str, geometry: Vec<u32>) -> Vec<u8> {
        let t = vt::Tile {
            layers: vec![vt::tile::Layer {
                version: 2,
                name: "layer".into(),
                features: vec![vt::tile::Feature {
                    id: Some(1),
                    tags: vec![0, 0, 1, 1],
                    r#type: Some(if geometry.len() > 3 { 3 } else { 1 }),
                    geometry,
                }],
                keys: vec!["key".into(), "rank".into()],
                values: vec![
                    vt::tile::Value { string_value: Some(value.into()), ..Default::default() },
                    vt::tile::Value { int_value: Some(rand_rank(value)), ..Default::default() },
                ],
                extent: Some(4096),
            }],
        };
        gzip(&t.encode_to_vec()).unwrap()
    }

    fn rand_rank(value: &str) -> i64 {
        value.len() as i64
    }

    fn write(path: &Path, value: &str) {
        write_bytes(path, tile(value));
    }

    fn write_bytes(path: &Path, bytes: Vec<u8>) {
        let mut db = Mbtiles::new_write_to_file_database(path, &Arguments::of()).unwrap();
        db.initialize().unwrap();
        let mut w = db.new_tile_writer().unwrap();
        w.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), bytes, None)).unwrap();
        w.close().unwrap();
    }

    #[test]
    fn test_compare() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = (tmp.path().join("a.mbtiles"), tmp.path().join("b.mbtiles"), tmp.path().join("c.mbtiles"));
        write(&a, "x");
        write(&b, "x");
        write(&c, "yy");
        assert_eq!((1, 0), compare(&a, &b, true).unwrap());
        assert_eq!((1, 1), compare(&a, &c, false).unwrap());
        assert!(compare(&a, &c, true).is_err());
    }

    #[test]
    fn test_compare_normalizes_geometry() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.mbtiles"), tmp.path().join("b.mbtiles"));
        // the same square starting at different corners: (0,0) (1,0) (1,1) (0,1) vs (1,0) (1,1) (0,1) (0,0)
        write_bytes(&a, tile_with_geometry("x", vec![9, 0, 0, 26, 2, 0, 0, 2, 1, 0, 15]));
        write_bytes(&b, tile_with_geometry("x", vec![9, 2, 0, 26, 0, 2, 1, 0, 0, 1, 15]));
        assert_eq!((1, 0), compare(&a, &b, true).unwrap());
    }
}
