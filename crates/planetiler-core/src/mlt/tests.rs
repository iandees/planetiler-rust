use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};

use super::*;
use crate::tags;

fn point(x: f64, y: f64) -> Geometry<f64> {
    Geometry::Point(Point::new(x, y))
}

fn line(coords: &[(f64, f64)]) -> LineString<f64> {
    LineString::new(coords.iter().map(|&(x, y)| Coord { x, y }).collect())
}

fn ring(coords: &[(f64, f64)]) -> LineString<f64> {
    let mut l = line(coords);
    l.0.push(l.0[0]);
    l
}

fn feature(id: i64, geometry: Geometry<f64>, properties: Tags) -> Feature {
    Feature { id: Some(id), geometry, properties }
}

fn tile(layers: Vec<(&str, Vec<Feature>)>) -> MapboxVectorTile {
    MapboxVectorTile {
        layers: layers
            .into_iter()
            .map(|(name, features)| Layer { name: name.into(), features, extent: 4096 })
            .collect(),
    }
}

fn all_options() -> Vec<MltOptions> {
    let mut result = Vec::new();
    for bits in 0..64u32 {
        let b = |i: u32| bits & (1 << i) != 0;
        result.push(MltOptions {
            fsst: b(0),
            fast_pfor: b(1),
            tessellate_polygons: b(2),
            polygon_outline: b(3) || b(2),
            reorder_features: b(4),
            shared_dictionaries: b(5),
        });
    }
    result
}

fn assert_roundtrip(input: &MapboxVectorTile) {
    for include_ids in [true, false] {
        for options in all_options() {
            let encoded = encode_tile(input, include_ids, &options).unwrap();
            let decoded = decoder::decode(&encoded).unwrap_or_else(|e| panic!("{options:?} {e:?}"));
            assert_eq!(input.layers.len(), decoded.layers.len());
            for (expected, actual) in input.layers.iter().zip(&decoded.layers) {
                assert_eq!(expected.name, actual.name);
                let mut expected_features = expected.features.clone();
                if options.reorder_features && include_ids {
                    expected_features.sort_by_key(|f| f.id);
                }
                assert_eq!(expected_features.len(), actual.features.len());
                for (e, a) in expected_features.iter().zip(&actual.features) {
                    assert_eq!(if include_ids { e.id } else { None }, a.id, "{options:?}");
                    assert_eq!(e.geometry, a.geometry, "{options:?}");
                    assert_eq!(e.properties, a.properties, "{options:?}");
                }
            }
        }
    }
}

#[test]
fn roundtrip_points_and_attributes() {
    let names = ["Main St", "Broadway", "Main St", "Elm", "Broadway"];
    let features: Vec<Feature> = (0..100)
        .map(|i| {
            let mut props = tags! {"name" => names[i % names.len()], "rank" => (i % 7) as i64, "flag" => i % 3 == 0};
            if i % 4 == 0 {
                props.insert("float".into(), Value::Float(i as f32 / 2.0));
                props.insert("double".into(), Value::Double(i as f64 / 3.0));
                props.insert("big".into(), Value::Int(1 << 40));
                props.insert("name:en".into(), Value::str(format!("en {}", i % 3)));
            }
            feature(100 - i as i64, point((i * 37 % 4096) as f64, (i * 91 % 4096) as f64), props)
        })
        .collect();
    assert_roundtrip(&tile(vec![("points", features)]));
}

#[test]
fn roundtrip_lines_and_polygons() {
    let square = |x: f64, y: f64, s: f64| ring(&[(x, y), (x + s, y), (x + s, y + s), (x, y + s)]);
    let features = vec![
        feature(1, Geometry::LineString(line(&[(0.0, 0.0), (10.0, 10.0), (20.0, 5.0)])), tags! {"kind" => "road"}),
        feature(
            2,
            Geometry::Polygon(Polygon::new(square(0.0, 0.0, 100.0), vec![square(10.0, 10.0, 20.0)])),
            tags! {"kind" => "building"},
        ),
        feature(
            3,
            Geometry::MultiPolygon(MultiPolygon::new(vec![
                Polygon::new(square(200.0, 200.0, 50.0), vec![]),
                Polygon::new(square(300.0, 300.0, 50.0), vec![]),
            ])),
            tags! {"kind" => "building", "height" => 10.5},
        ),
        feature(
            4,
            Geometry::MultiLineString(MultiLineString::new(vec![
                line(&[(0.0, 0.0), (-10.0, -10.0)]),
                line(&[(5000.0, 5.0), (5010.0, 6.0), (5020.0, 7.0)]),
            ])),
            tags! {"kind_num" => 5},
        ),
        feature(5, Geometry::MultiPoint(MultiPoint::new(vec![Point::new(1.0, 2.0), Point::new(3.0, 4.0)])), tags! {}),
    ];
    assert_roundtrip(&tile(vec![("mixed", features)]));
    let polygons: Vec<Feature> = (0..30)
        .map(|i| {
            let x = (i * 100) as f64;
            feature(
                i,
                Geometry::Polygon(Polygon::new(square(x, x, 80.0), vec![square(x + 10.0, x + 10.0, 5.0)])),
                tags! {},
            )
        })
        .collect();
    let lines: Vec<Feature> = (0..30)
        .map(|i| {
            let x = (i * 100) as f64;
            feature(i, Geometry::LineString(line(&[(x, 0.0), (x, 50.0), (x + 1.0, 100.0)])), tags! {"i" => i})
        })
        .collect();
    assert_roundtrip(&tile(vec![("lines", lines), ("polygons", polygons)]));
}

#[test]
fn type_coercion() {
    let features = vec![
        feature(1, point(1.0, 1.0), tags! {"attr" => "string"}),
        feature(2, point(1.0, 1.0), tags! {"attr" => 1}),
        feature(3, point(1.0, 1.0), tags! {"attr" => 1.5}),
        feature(4, point(1.0, 1.0), tags! {"attr" => true}),
    ];
    let encoded = encode_tile(&tile(vec![("layer", features)]), true, &MltOptions::default()).unwrap();
    let decoded = decoder::decode(&encoded).unwrap();
    let values: Vec<Value> = decoded.layers[0].features.iter().map(|f| f.properties["attr"].clone()).collect();
    assert_eq!(vec![Value::str("string"), Value::str("1"), Value::str("1.5"), Value::str("true")], values);
}

#[test]
fn int_upgrades() {
    let features = vec![
        feature(1, point(1.0, 1.0), tags! {"a" => 1, "b" => Value::Float(1.5)}),
        feature(2, point(1.0, 1.0), tags! {"a" => 1i64 << 40, "b" => 2.5}),
    ];
    let input = tile(vec![("layer", features)]);
    let metadata = create_tileset_metadata(&input, TypeMismatchPolicy::Fail, &[], true).unwrap();
    let columns = &metadata.feature_tables[0].columns;
    assert_eq!(Some(metadata::ScalarType::Int64), columns[2].scalar_type());
    assert_eq!(Some(metadata::ScalarType::Double), columns[3].scalar_type());
    let mismatch = tile(vec![(
        "layer",
        vec![feature(1, point(1.0, 1.0), tags! {"a" => 1}), feature(2, point(1.0, 1.0), tags! {"a" => "x"})],
    )]);
    assert!(create_tileset_metadata(&mismatch, TypeMismatchPolicy::Fail, &[], true).is_err());
}

#[test]
fn long_and_negative_ids() {
    for ids in [vec![1i64, 1 << 40, 3], vec![-5, 2, 3], vec![i64::MIN, 0, i64::MAX]] {
        let features = ids.iter().map(|&id| feature(id, point(1.0, 2.0), tags! {})).collect();
        let input = tile(vec![("layer", features)]);
        let encoded = encode_tile(&input, true, &MltOptions::default()).unwrap();
        let decoded = decoder::decode(&encoded).unwrap();
        let decoded_ids: Vec<Option<i64>> = decoded.layers[0].features.iter().map(|f| f.id).collect();
        let expected: Vec<Option<i64>> = ids
            .iter()
            .map(|&id| Some(if (i32::MIN as i64..0).contains(&id) { id as u32 as i64 } else { id }))
            .collect();
        assert_eq!(expected, decoded_ids);
    }
}
