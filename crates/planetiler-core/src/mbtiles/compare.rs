//! Port of `mbtiles/Compare.java`: a utility to compare two mbtiles files feature-by-feature.
//!
//! Feature ids are not compared and the `rank` attribute is ignored since the value planetiler produces is not
//! stable.
//!
//! TODO(port): Java compares *normalized decoded JTS geometries* (`VectorTile.Feature.geometry().decode().norm()`).
//! Until `vector_tile.rs` geometry decoding is ported, this compares the raw geometry command streams instead, which
//! is stricter (geometries that only differ in ring start point/orientation count as different).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::bail;
use prost::Message;
use tracing::{info, warn};

use crate::geo::tile_coord::TileCoord;
use crate::mbtiles::mbtiles::Mbtiles;
use crate::proto::vector_tile as vt;
use crate::util::gzip::gunzip;

/// Comparable representation of a vector tile feature.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct VectorTileFeatureForCmp {
    layer: String,
    geom_type: i32,
    geometry: Vec<u32>,
    attrs: BTreeMap<String, String>,
}

fn value_to_string(v: &vt::tile::Value) -> String {
    if let Some(s) = &v.string_value {
        format!("{s:?}")
    } else if let Some(f) = v.float_value {
        format!("{f}f")
    } else if let Some(d) = v.double_value {
        format!("{d}d")
    } else if let Some(i) = v.int_value {
        format!("{i}")
    } else if let Some(u) = v.uint_value {
        format!("{u}")
    } else if let Some(s) = v.sint_value {
        format!("{s}")
    } else if let Some(b) = v.bool_value {
        format!("{b}")
    } else {
        "null".into()
    }
}

fn decode_features(data: Option<Vec<u8>>) -> anyhow::Result<BTreeSet<VectorTileFeatureForCmp>> {
    let Some(data) = data else { return Ok(BTreeSet::new()) };
    let tile = vt::Tile::decode(gunzip(&data)?.as_slice())?;
    let mut result = BTreeSet::new();
    for layer in &tile.layers {
        for feature in &layer.features {
            let mut attrs = BTreeMap::new();
            for pair in feature.tags.chunks(2) {
                if let [k, v] = pair
                    && let (Some(key), Some(value)) = (layer.keys.get(*k as usize), layer.values.get(*v as usize))
                    && key != "rank"
                {
                    attrs.insert(key.clone(), value_to_string(value));
                }
            }
            result.insert(VectorTileFeatureForCmp {
                layer: layer.name.clone(),
                geom_type: feature.r#type() as i32,
                geometry: feature.geometry.clone(),
                attrs,
            });
        }
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
    use crate::archive::tile_archive_config::ArchiveOptions;
    use crate::archive::tile_encoding_result::TileEncodingResult;
    use crate::archive::writeable_tile_archive::WriteableTileArchive;
    use crate::util::gzip::gzip_zlib as gzip;

    fn tile(value: &str) -> Vec<u8> {
        let t = vt::Tile {
            layers: vec![vt::tile::Layer {
                version: 2,
                name: "layer".into(),
                features: vec![vt::tile::Feature {
                    id: Some(1),
                    tags: vec![0, 0, 1, 1],
                    r#type: Some(1),
                    geometry: vec![9, 0, 0],
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
        let mut db = Mbtiles::new_write_to_file_database(path, &ArchiveOptions::empty()).unwrap();
        db.initialize().unwrap();
        let mut w = db.new_tile_writer().unwrap();
        w.write(TileEncodingResult::new(TileCoord::of_xyz(0, 0, 0), tile(value), None)).unwrap();
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
}
