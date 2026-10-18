//! Coordinate reference systems and transforms (replacement for the GeoTools `CRS` / `MathTransform` helpers in
//! `GeoUtils.java`).
//!
//! Supported CRS definitions:
//! - `EPSG:4326`, `EPSG:4269`, `CRS:84`, `WGS84` (geographic lon/lat; EPSG codes are latitude-first unless
//!   `longitude_first` is requested or a `:lon_first` suffix is used, matching GeoTools `CRS.decode`)
//! - `EPSG:3857`, `EPSG:900913`, `EPSG:3785`, `EPSG:102100` (spherical web mercator)
//! - PROJ strings (`+proj=...`) via `proj4rs`
//! - WKT `GEOGCS[...]` (treated as WGS84-equivalent geographic, axis order from `AXIS[...]`, longitude first by default)
//!   and WKT `PROJCS[...]` for pseudo/web mercator.
//!
//! TODO(port): arbitrary EPSG codes and general WKT CRS definitions are not supported (GeoTools has a full EPSG
//! database; `proj4rs` would need the `crs-definitions` feature and a WKT -> proj string converter).

use std::fmt;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use geo_types::Coord;

const EARTH_RADIUS: f64 = 6_378_137.0;

#[derive(Clone)]
enum Kind {
    /// WGS84-like geographic coordinates in degrees
    Geographic,
    /// spherical web mercator in meters
    WebMercator,
    /// any other projection supported by proj4rs
    Proj(Arc<proj4rs::Proj>),
}

/// A coordinate reference system with an axis order.
#[derive(Clone)]
pub struct Crs {
    kind: Kind,
    /// True if the first axis is easting/longitude (GeoTools `AxisOrder.EAST_NORTH`).
    longitude_first: bool,
    definition: String,
}

impl fmt::Debug for Crs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Crs({}, longitude_first={})", self.definition, self.longitude_first)
    }
}

impl PartialEq for Crs {
    fn eq(&self, other: &Self) -> bool {
        self.definition == other.definition && self.longitude_first == other.longitude_first
    }
}

impl Crs {
    /// WGS84 longitude/latitude (`EPSG:4326` with longitude first).
    pub fn wgs84() -> Crs {
        Crs { kind: Kind::Geographic, longitude_first: true, definition: "EPSG:4326".into() }
    }

    /// Spherical web mercator (`EPSG:3857`).
    pub fn web_mercator() -> Crs {
        Crs { kind: Kind::WebMercator, longitude_first: true, definition: "EPSG:3857".into() }
    }

    /// Equivalent of GeoTools `CRS.decode(code, longitudeFirst)`.
    pub fn decode(code: &str, longitude_first: bool) -> Result<Crs> {
        let trimmed = code.trim();
        let upper = trimmed.to_ascii_uppercase();
        match upper.as_str() {
            "EPSG:4326" | "EPSG:4269" | "EPSG:4258" => {
                Ok(Crs { kind: Kind::Geographic, longitude_first, definition: upper })
            }
            "CRS:84" | "WGS84" | "OGC:CRS84" => {
                Ok(Crs { kind: Kind::Geographic, longitude_first: true, definition: upper })
            }
            "EPSG:3857" | "EPSG:900913" | "EPSG:3785" | "EPSG:102100" | "EPSG:102113" => {
                Ok(Crs { kind: Kind::WebMercator, longitude_first: true, definition: upper })
            }
            _ if trimmed.starts_with('+') => {
                let proj = proj4rs::Proj::from_proj_string(trimmed).map_err(|e| anyhow!("invalid proj string: {e}"))?;
                if proj.is_latlong() {
                    Ok(Crs { kind: Kind::Geographic, longitude_first: true, definition: trimmed.into() })
                } else {
                    Ok(Crs { kind: Kind::Proj(Arc::new(proj)), longitude_first: true, definition: trimmed.into() })
                }
            }
            _ => bail!("Unsupported CRS: {code}"),
        }
    }

    /// Equivalent of GeoTools `CRS.parseWKT` for the subset of WKT planetiler needs.
    pub fn parse_wkt(wkt: &str) -> Result<Crs> {
        let trimmed = wkt.trim();
        let upper = trimmed.to_ascii_uppercase();
        if !upper.ends_with(']') || upper.matches('[').count() != upper.matches(']').count() {
            bail!("Invalid WKT CRS: {wkt}");
        }
        if upper.starts_with("GEOGCS[") || upper.starts_with("GEOGCRS[") {
            // axis order: longitude first unless the first AXIS is latitude/north
            let longitude_first = match upper.find("AXIS[") {
                Some(idx) => {
                    let axis = &upper[idx..];
                    let end = axis.find(']').unwrap_or(axis.len());
                    let axis = &axis[..end];
                    !(axis.contains("LAT") || axis.contains("NORTH"))
                }
                None => true,
            };
            Ok(Crs { kind: Kind::Geographic, longitude_first, definition: trimmed.into() })
        } else if (upper.starts_with("PROJCS[") || upper.starts_with("PROJCRS["))
            && (upper.contains("PSEUDO") || upper.contains("WEB_MERCATOR") || upper.contains("3857"))
        {
            Ok(Crs { kind: Kind::WebMercator, longitude_first: true, definition: trimmed.into() })
        } else {
            bail!("Unsupported WKT CRS: {wkt}")
        }
    }

    pub fn is_longitude_first(&self) -> bool {
        self.longitude_first
    }

    pub fn definition(&self) -> &str {
        &self.definition
    }

    /// Convert from this CRS (in east/north order) to WGS84 lon/lat degrees.
    fn to_lon_lat(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        match &self.kind {
            Kind::Geographic => Ok(c),
            Kind::WebMercator => Ok(Coord {
                x: (c.x / EARTH_RADIUS).to_degrees(),
                y: (2.0 * (c.y / EARTH_RADIUS).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees(),
            }),
            Kind::Proj(proj) => {
                let wgs84 = proj4rs::Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
                    .map_err(|e| anyhow!("{e}"))?;
                let mut p = (c.x, c.y, 0.0);
                proj4rs::transform::transform(proj, &wgs84, &mut p).map_err(|e| anyhow!("transform error: {e}"))?;
                Ok(Coord { x: p.0.to_degrees(), y: p.1.to_degrees() })
            }
        }
    }

    /// Convert WGS84 lon/lat degrees to this CRS (in east/north order).
    fn from_lon_lat(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        match &self.kind {
            Kind::Geographic => Ok(c),
            Kind::WebMercator => Ok(Coord {
                x: EARTH_RADIUS * c.x.to_radians(),
                y: EARTH_RADIUS * (std::f64::consts::FRAC_PI_4 + c.y.to_radians() / 2.0).tan().ln(),
            }),
            Kind::Proj(proj) => {
                let wgs84 = proj4rs::Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
                    .map_err(|e| anyhow!("{e}"))?;
                let mut p = (c.x.to_radians(), c.y.to_radians(), 0.0);
                proj4rs::transform::transform(&wgs84, proj, &mut p).map_err(|e| anyhow!("transform error: {e}"))?;
                Ok(Coord { x: p.0, y: p.1 })
            }
        }
    }

    fn same_kind(&self, other: &Crs) -> bool {
        match (&self.kind, &other.kind) {
            (Kind::Geographic, Kind::Geographic) | (Kind::WebMercator, Kind::WebMercator) => true,
            (Kind::Proj(_), Kind::Proj(_)) => self.definition == other.definition,
            _ => false,
        }
    }
}

/// A transform between 2 coordinate reference systems (GeoTools `MathTransform`).
#[derive(Clone, Debug)]
pub struct MathTransform {
    /// swap x/y of input coordinates before anything else (Java's `swapXYTransform`)
    swap_input: bool,
    source: Crs,
    dest: Crs,
}

impl MathTransform {
    /// A transform that only swaps the X/Y coordinates.
    pub fn swap_xy() -> MathTransform {
        MathTransform { swap_input: true, source: Crs::wgs84(), dest: Crs::wgs84() }
    }

    pub fn is_identity(&self) -> bool {
        !self.swap_input
            && self.source.same_kind(&self.dest)
            && self.source.longitude_first == self.dest.longitude_first
    }

    /// Transform a coordinate from the source to the destination CRS.
    pub fn transform(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        let mut c = if self.swap_input { Coord { x: c.y, y: c.x } } else { c };
        if !self.source.longitude_first {
            c = Coord { x: c.y, y: c.x };
        }
        if !self.source.same_kind(&self.dest) {
            c = self.dest.from_lon_lat(self.source.to_lon_lat(c)?)?;
        }
        if !self.dest.longitude_first {
            c = Coord { x: c.y, y: c.x };
        }
        Ok(c)
    }

    /// Transform every coordinate of a geometry.
    pub fn transform_geometry(&self, geom: &geo_types::Geometry<f64>) -> Result<geo_types::Geometry<f64>> {
        use geo::MapCoords;
        if self.is_identity() {
            return Ok(geom.clone());
        }
        geom.try_map_coords(|c| self.transform(c))
    }
}

/// Equivalent of `GeoUtils.findMathTransform`: a transform from `source` to `dest`. If `force_longitude_first` is
/// set and does not match the axis order of `source`, input coordinates are swapped first.
pub fn find_math_transform(source: &Crs, dest: &Crs, force_longitude_first: Option<bool>) -> MathTransform {
    let swap_input = matches!(force_longitude_first, Some(f) if f != source.longitude_first);
    MathTransform { swap_input, source: source.clone(), dest: dest.clone() }
}

/// Equivalent of `GeoUtils.decodeCRS(crs, base)`: decodes an `EPSG:1234` code (optionally suffixed with
/// `:lon_first` or `:lat_first`) or WKT. Axis order defaults to the order of `base` if specified, otherwise
/// latitude-first for geographic EPSG codes.
pub fn decode_crs(crs: &str, base: Option<&Crs>) -> Result<Crs> {
    let mut longitude_first = base.is_some_and(|b| b.longitude_first);
    let mut code = crs;
    if let Some(stripped) = crs.strip_suffix(":lon_first") {
        longitude_first = true;
        code = stripped;
    } else if let Some(stripped) = crs.strip_suffix(":lat_first") {
        longitude_first = false;
        code = stripped;
    }
    Crs::decode(code, longitude_first).or_else(|_| Crs::parse_wkt(crs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WKT_NO_AXIS: &str = r#"GEOGCS["WGS 84", DATUM["WGS_1984", SPHEROID["WGS 84", 6378137, 298.257223563]], PRIMEM["Greenwich", 0], UNIT["degree", 0.0174532925199433]]"#;
    const WKT_LAT_LON: &str = r#"GEOGCS["WGS 84", DATUM["WGS_1984", SPHEROID["WGS 84", 6378137, 298.257223563]], PRIMEM["Greenwich", 0], AXIS["Latitude", NORTH], AXIS["Longitude", EAST], UNIT["degree", 0.0174532925199433]]"#;
    const WKT_LON_LAT: &str = r#"GEOGCS["WGS 84", DATUM["WGS_1984", SPHEROID["WGS 84", 6378137, 298.257223563]], PRIMEM["Greenwich", 0], AXIS["Longitude", EAST], AXIS["Latitude", NORTH], UNIT["degree", 0.0174532925199433]]"#;

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

    #[test]
    fn crs_decode_garbage() {
        assert!(decode_crs("garbage", None).is_err());
        assert!(decode_crs("GEGCS[", None).is_err());
    }

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
    fn proj_string() {
        let utm = decode_crs("+proj=utm +zone=18 +datum=WGS84 +units=m +no_defs", None).unwrap();
        let to = find_math_transform(&Crs::wgs84(), &utm, None);
        let p = to.transform(Coord { x: -75.0, y: 0.0 }).unwrap();
        assert!((p.x - 500000.0).abs() < 1.0, "{p:?}");
        let back = find_math_transform(&utm, &Crs::wgs84(), None).transform(p).unwrap();
        assert!((back.x + 75.0).abs() < 1e-6 && back.y.abs() < 1e-6, "{back:?}");
    }
}
