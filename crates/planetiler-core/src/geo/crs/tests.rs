use super::*;

const WKT_NO_AXIS: &str = r#"GEOGCS["WGS 84", DATUM["WGS_1984", SPHEROID["WGS 84", 6378137, 298.257223563]], PRIMEM["Greenwich", 0], UNIT["degree", 0.0174532925199433]]"#;
const WKT_LAT_LON: &str = r#"GEOGCS["WGS 84", DATUM["WGS_1984", SPHEROID["WGS 84", 6378137, 298.257223563]], PRIMEM["Greenwich", 0], AXIS["Latitude", NORTH], AXIS["Longitude", EAST], UNIT["degree", 0.0174532925199433]]"#;
const WKT_LON_LAT: &str = r#"GEOGCS["WGS 84", DATUM["WGS_1984", SPHEROID["WGS 84", 6378137, 298.257223563]], PRIMEM["Greenwich", 0], AXIS["Longitude", EAST], AXIS["Latitude", NORTH], UNIT["degree", 0.0174532925199433]]"#;

// GeoUtilsTest.testCrsDecode
#[test]
fn crs_decode() {
    for (code, expected_lat, expected_lon) in [
        ("EPSG:4326", 1.0, 2.0),
        ("EPSG:4326:lon_first", 2.0, 1.0),
        ("EPSG:4326:lat_first", 1.0, 2.0),
        ("EPSG:3857", 222684.2, 111319.5),
        (WKT_NO_AXIS, 2.0, 1.0),
        (WKT_LAT_LON, 1.0, 2.0),
    ] {
        let from = Crs::decode("EPSG:4326", true).unwrap();
        let to = decode_crs(code, None).unwrap();
        let xform = find_math_transform(&from, &to, None);
        let b = xform.transform(Coord { x: 1.0, y: 2.0 }).unwrap();
        assert!((expected_lon - b.x).abs() < 1e-1, "{code} {b:?}");
        assert!((expected_lat - b.y).abs() < 1e-1, "{code} {b:?}");
    }
}

// GeoUtilsTest.testCrsDecodeForceCrs
#[test]
fn crs_decode_force_crs() {
    for code in ["EPSG:4326", "EPSG:4326:lon_first", "EPSG:4326:lat_first", WKT_NO_AXIS, WKT_LAT_LON, WKT_LON_LAT] {
        let from = decode_crs(code, None).unwrap();
        let to = Crs::decode("EPSG:4326", true).unwrap();
        let xform1 = find_math_transform(&from, &to, Some(true));
        let xform2 = find_math_transform(&from, &to, Some(false));
        let b = xform1.transform(Coord { x: 1.0, y: 2.0 }).unwrap();
        assert!((1.0 - b.x).abs() < 1e-1);
        assert!((2.0 - b.y).abs() < 1e-1);
        for x in [-180.0, 0.0, 180.0] {
            for y in [-85.0, 0.0, 85.0] {
                let a = Coord { x, y };
                let b = xform1.transform(a).unwrap();
                assert!((x - b.x).abs() < 1e-1 && (y - b.y).abs() < 1e-1, "{code} {x}, {y}");
                let b = xform2.transform(a).unwrap();
                assert!((y - b.x).abs() < 1e-1 && (x - b.y).abs() < 1e-1, "{code} {x}, {y}");
            }
        }
    }
}

// GeoUtilsTest.testCrsDecodeGarbage
#[test]
fn crs_decode_garbage() {
    assert!(decode_crs("garbage", None).is_err());
    assert!(decode_crs("GEGCS[", None).is_err());
}

// GeoUtilsTest.testCrsDecodeDefaultAxisOrder
#[test]
fn crs_decode_default_axis_order() {
    let lon_first = Crs::decode("EPSG:4326", true).unwrap();
    let lat_first = Crs::decode("EPSG:4326", false).unwrap();
    let point = Coord { x: 1.0, y: 2.0 };
    let transposed = Coord { x: 2.0, y: 1.0 };
    let t = |dest: Crs| find_math_transform(&lon_first, &dest, None).transform(point).unwrap();
    assert_eq!(point, t(decode_crs("EPSG:4326", Some(&lon_first)).unwrap()));
    assert_eq!(transposed, t(decode_crs("EPSG:4326", Some(&lat_first)).unwrap()));
    assert_eq!(point, t(decode_crs("EPSG:4326:lon_first", Some(&lat_first)).unwrap()));
}

#[test]
fn mercator_round_trip() {
    let to = find_math_transform(&Crs::wgs84(), &Crs::web_mercator(), None);
    let back = find_math_transform(&Crs::web_mercator(), &Crs::wgs84(), None);
    let c = Coord { x: -73.5, y: 45.2 };
    let r = back.transform(to.transform(c).unwrap()).unwrap();
    assert!((r.x - c.x).abs() < 1e-9 && (r.y - c.y).abs() < 1e-9);
}

#[test]
fn wkt_transverse_mercator() {
    let sweref = Crs::parse_wkt(
        r#"PROJCS["SWEREF99_TM",GEOGCS["GCS_SWEREF99",DATUM["D_SWEREF99",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",15.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#,
    )
    .unwrap();
    let p = find_math_transform(&sweref, &Crs::wgs84(), None).transform(Coord { x: 1.0, y: 2.0 }).unwrap();
    assert!((p.x - 10.5113).abs() < 1e-4, "{p:?}");
    assert!(p.y.abs() < 1e-4, "{p:?}");
}

#[test]
fn proj_string() {
    let utm = decode_crs("+proj=utm +zone=18 +datum=WGS84 +units=m +no_defs", None).unwrap();
    let to = find_math_transform(&Crs::wgs84(), &utm, None);
    let p = to.transform(Coord { x: -75.0, y: 0.0 }).unwrap();
    assert!((p.x - 500000.0).abs() < 1.0, "{p:?}");
    let back = find_math_transform(&utm, &Crs::wgs84(), None).transform(p).unwrap();
    assert!((back.x + 75.0).abs() < 1e-6 && back.y.abs() < 1e-6, "{back:?}");
}

fn resources() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/crs")
}

fn points(s: &str) -> Vec<Coord<f64>> {
    s.split(';')
        .map(|p| {
            let (x, y) = p.split_once(',').unwrap();
            Coord { x: x.parse().unwrap(), y: y.parse().unwrap() }
        })
        .collect()
}

/// Reference coordinates computed by GeoTools (`tools/crs/RefTransform.java`, see `tests/resources/crs/cases.tsv`)
/// for EPSG codes, ESRI/GDAL `.prj` files, WKT2 and PROJ strings, decoded like `ShapefileReader` does.
#[test]
fn reference_coordinates_from_geotools() {
    let text = std::fs::read_to_string(resources().join("reference.tsv")).unwrap();
    let mut count = 0;
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        let [name, def, input, expected] = cols[..] else { panic!("bad line {line}") };
        assert!(!expected.starts_with("ERR"), "{name}: GeoTools failed: {expected}");
        let crs = match def.strip_prefix('@') {
            Some(file) => Crs::parse_wkt(&std::fs::read_to_string(resources().join(file)).unwrap()),
            None => decode_crs(def, None),
        }
        .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let transform = find_math_transform(&crs, &Crs::wgs84(), None);
        // GeoTools' Hotine oblique mercator does not map the projection center of the Hungarian EOV (an azimuth of
        // 90 degrees like the Swiss grids) exactly to its coordinates: it is ~10 cm off
        let tolerance = if name == "hungarian_eov" { 2e-6 } else { 1e-6 };
        for (input, expected) in points(input).into_iter().zip(points(expected)) {
            let actual = transform.transform(input).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert!(
                (actual.x - expected.x).abs() < tolerance && (actual.y - expected.y).abs() < tolerance,
                "{name}: {input:?} -> {actual:?}, GeoTools: {expected:?} ({:?})",
                crs.proj_string()
            );
            // the inverse (used to filter by bounds) round-trips
            let back = transform.inverse_transform(actual).unwrap();
            let scale = if crs.proj_string().is_some_and(|p| p.starts_with("+proj=longlat")) {
                1e-7
            } else if name == "web_mercator_3785" {
                // geocentric datum shift between a sphere and WGS84 drops a large ellipsoidal height (like GeoTools)
                50.0
            } else {
                1e-2
            };
            assert!(
                (back.x - input.x).abs() < scale && (back.y - input.y).abs() < scale,
                "{name}: {input:?} -> {actual:?} -> {back:?}"
            );
            count += 1;
        }
    }
    assert!(count > 100, "{count}");
}

#[test]
fn epsg_code_forms() {
    let reference = Crs::decode("EPSG:2056", false).unwrap();
    let p = Coord { x: 2600000.0, y: 1200000.0 };
    let expected = find_math_transform(&reference, &Crs::wgs84(), None).transform(p).unwrap();
    for code in [
        "epsg:2056",
        " EPSG:2056 ",
        "EPSG::2056",
        "urn:ogc:def:crs:EPSG::2056",
        "urn:ogc:def:crs:EPSG:9.9.1:2056",
        "urn:x-ogc:def:crs:EPSG:2056",
        "http://www.opengis.net/def/crs/EPSG/0/2056",
        "https://www.opengis.net/def/crs/EPSG/0/2056",
        "http://www.opengis.net/gml/srs/epsg.xml#2056",
    ] {
        let crs = Crs::decode(code, false).unwrap_or_else(|e| panic!("{code}: {e}"));
        assert_eq!("EPSG:2056", crs.definition(), "{code}");
        assert_eq!(expected, find_math_transform(&crs, &Crs::wgs84(), None).transform(p).unwrap(), "{code}");
    }
    assert!(Crs::decode("EPSG:", false).is_err());
    assert!(Crs::decode("EPSG:12a", false).is_err());
    assert!(epsg::len() > 6000, "{}", epsg::len());
}

#[test]
fn epsg_errors() {
    let unknown = Crs::decode("EPSG:1", false).unwrap_err().to_string();
    assert!(unknown.contains("Unknown CRS EPSG:1"), "{unknown}");
    // vertical CRS
    assert!(Crs::decode("EPSG:5703", false).is_err());
    let unsupported = Crs::decode("EPSG:31300", false).unwrap_err().to_string();
    assert!(unsupported.contains("EPSG:31300 is not supported") && unsupported.contains("Belgium"), "{unsupported}");
    let err = format!("{:#}", decode_crs("EPSG:31300", None).unwrap_err());
    assert!(err.contains("Belgium"), "{err}");
}

#[test]
fn axis_order_like_geotools() {
    // geographic: latitude first unless longitude first is requested
    assert_eq!(AxisOrder::NorthEast, Crs::decode("EPSG:4258", false).unwrap().axis_order());
    assert_eq!(AxisOrder::EastNorth, Crs::decode("EPSG:4258", true).unwrap().axis_order());
    // projected with northing first
    let laea = Crs::decode("EPSG:3035", false).unwrap();
    assert_eq!(AxisOrder::NorthEast, laea.axis_order());
    let laea_lon_first = decode_crs("EPSG:3035", Some(&Crs::wgs84())).unwrap();
    assert_eq!(AxisOrder::EastNorth, laea_lon_first.axis_order());
    let a = find_math_transform(&laea, &Crs::wgs84(), None).transform(Coord { x: 3.2e6, y: 4.3e6 }).unwrap();
    let b = find_math_transform(&laea_lon_first, &Crs::wgs84(), None).transform(Coord { x: 4.3e6, y: 3.2e6 }).unwrap();
    assert_eq!(a, b);
    // polar axes like "South along 45 deg East": no axis order, so GeoPackageReader-style forcing swaps the input
    let polar = Crs::decode("EPSG:3413", true).unwrap();
    assert_eq!(AxisOrder::Inapplicable, polar.axis_order());
    assert!(!polar.is_longitude_first());
    let p = Coord { x: 1e6, y: -1e6 };
    let direct = find_math_transform(&polar, &Crs::wgs84(), None).transform(p).unwrap();
    let forced = find_math_transform(&polar, &Crs::wgs84(), Some(true)).transform(Coord { x: p.y, y: p.x }).unwrap();
    assert_eq!(direct, forced);
    // projected east/north
    assert_eq!(AxisOrder::EastNorth, Crs::decode("EPSG:2056", false).unwrap().axis_order());
}

#[test]
fn web_mercator_aliases() {
    for code in ["EPSG:3857", "EPSG:900913", "EPSG:102100", "EPSG:102113", "ESRI:102100"] {
        let crs = Crs::decode(code, false).unwrap();
        assert!(find_math_transform(&crs, &Crs::web_mercator(), None).is_identity(), "{code}");
    }
    let esri =
        Crs::parse_wkt(&std::fs::read_to_string(resources().join("web_mercator_aux_esri.prj")).unwrap()).unwrap();
    assert!(find_math_transform(&esri, &Crs::web_mercator(), None).is_identity());
    // web mercator with a false easting is not
    let shifted = Crs::from_proj_string("+proj=merc +a=6378137 +b=6378137 +x_0=10 +nadgrids=@null").unwrap();
    assert!(!find_math_transform(&shifted, &Crs::web_mercator(), None).is_identity());
}

#[test]
fn wgs84_equivalents_are_identity() {
    for code in ["EPSG:4326:lon_first", "CRS:84", "+proj=longlat +datum=WGS84 +no_defs", WKT_NO_AXIS] {
        let crs = decode_crs(code, None).unwrap();
        assert!(find_math_transform(&crs, &Crs::wgs84(), None).is_identity(), "{code}");
    }
    let etrs89 = Crs::parse_wkt(&std::fs::read_to_string(resources().join("etrs89_esri.prj")).unwrap()).unwrap();
    let t = find_math_transform(&etrs89, &Crs::wgs84(), None);
    let p = t.transform(Coord { x: 10.0, y: 52.0 }).unwrap();
    assert!((p.x - 10.0).abs() < 1e-9 && (p.y - 52.0).abs() < 1e-8, "{p:?}");
}

#[test]
fn unsupported_definitions() {
    let grid = Crs::from_proj_string("+proj=longlat +ellps=clrk66 +nadgrids=conus +no_defs").unwrap_err();
    assert!(format!("{grid:#}").contains("grid based datum shifts"), "{grid:#}");
    let proj = Crs::from_proj_string("+proj=wintri +no_defs").unwrap_err();
    assert!(format!("{proj:#}").contains("unsupported projection +proj=wintri"), "{proj:#}");
    let wkt = Crs::parse_wkt(
        r#"PROJCS["x",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Van_der_Grinten_I"],UNIT["Meter",1.0]]"#,
    )
    .unwrap_err();
    assert!(format!("{wkt:#}").contains("unsupported projection \"Van_der_Grinten_I\""), "{wkt:#}");
    assert!(
        Crs::parse_wkt(r#"GEOCCS["WGS 84", DATUM["WGS_1984", SPHEROID["WGS 84",6378137,298.257223563]]]"#).is_err()
    );
    assert!(Crs::parse_wkt("PROJCS[").is_err());
}

#[test]
fn compound_crs_uses_horizontal_component() {
    let wkt = r#"COMPD_CS["OSGB 1936 / British National Grid + ODN height",PROJCS["OSGB 1936 / British National Grid",GEOGCS["OSGB 1936",DATUM["OSGB_1936",SPHEROID["Airy 1830",6377563.396,299.3249646,AUTHORITY["EPSG","7001"]],AUTHORITY["EPSG","6277"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4277"]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",49],PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],PARAMETER["false_northing",-100000],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","27700"]],VERT_CS["ODN height",VERT_DATUM["Ordnance Datum Newlyn",2005,AUTHORITY["EPSG","5101"]],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Up",UP],AUTHORITY["EPSG","5701"]],AUTHORITY["EPSG","7405"]]"#;
    let compound = Crs::parse_wkt(wkt).unwrap();
    let bng = Crs::decode("EPSG:27700", false).unwrap();
    let p = Coord { x: 530000.0, y: 180000.0 };
    let a = find_math_transform(&compound, &Crs::wgs84(), None).transform(p).unwrap();
    let b = find_math_transform(&bng, &Crs::wgs84(), None).transform(p).unwrap();
    // the GEOGCS has an EPSG code, so the EPSG datum shift is used even though the WKT has no TOWGS84
    assert!((a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9, "{a:?} {b:?}");
}

#[test]
fn transform_is_thread_safe_and_cloneable() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Crs>();
    assert_send_sync::<MathTransform>();
}
