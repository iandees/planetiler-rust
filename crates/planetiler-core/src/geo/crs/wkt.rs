//! WKT CRS parsing (WKT1 in its OGC, ESRI `.prj` and GDAL flavors, and WKT2 / ISO 19162) and conversion to a
//! PROJ string, following the semantics of GeoTools' `CRS.parseWKT` that planetiler (Java) uses:
//!
//! - WKT1 angular projection parameters and the prime meridian are in the angular unit of the `GEOGCS`, linear
//!   parameters (false easting/northing) are in the linear unit of the `PROJCS`.
//! - Axis order and direction come from the `AXIS` elements (east/north if there are none).
//! - The datum shift to WGS84 comes from the EPSG database when the geographic CRS has an `AUTHORITY["EPSG", ...]`
//!   code (GeoTools' authority-backed operation lookup), otherwise `TOWGS84`, otherwise none at all (GeoTools'
//!   lenient mode: coordinates are only moved between ellipsoids with a Molodensky transformation without
//!   translation, written as the non-standard `+molodensky=0,0,0` PROJ parameter).

use std::fmt::Write;

use anyhow::{Context, Result, anyhow, bail};

use super::{AxisOrder, CrsDef};

/// A parsed WKT node: `NAME[arg, arg, ...]` where each arg is a quoted string, number, bare word or nested node.
#[derive(Debug, Clone)]
pub(crate) enum WktNode {
    Text(String),
    Number(f64),
    Node(String, Vec<WktNode>),
}

impl WktNode {
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            WktNode::Node(name, _) => Some(name),
            _ => None,
        }
    }

    fn children(&self) -> &[WktNode] {
        match self {
            WktNode::Node(_, children) => children,
            _ => &[],
        }
    }

    fn child(&self, names: &[&str]) -> Option<&WktNode> {
        self.children().iter().find(|c| c.name().is_some_and(|n| names.contains(&n)))
    }

    fn all<'a>(&'a self, names: &'a [&'a str]) -> impl Iterator<Item = &'a WktNode> + 'a {
        self.children().iter().filter(move |c| c.name().is_some_and(|n| names.contains(&n)))
    }

    fn text(&self, idx: usize) -> Option<&str> {
        match self.children().get(idx) {
            Some(WktNode::Text(t)) => Some(t),
            _ => None,
        }
    }

    fn number(&self, idx: usize) -> Option<f64> {
        match self.children().get(idx) {
            Some(WktNode::Number(n)) => Some(*n),
            Some(WktNode::Text(t)) => t.trim().parse().ok(),
            _ => None,
        }
    }

    /// `AUTHORITY["EPSG","1234"]` (WKT1) or `ID["EPSG",1234]` (WKT2) EPSG code.
    fn epsg_code(&self) -> Option<u32> {
        let auth = self.child(&["AUTHORITY", "ID"])?;
        if !auth.text(0)?.eq_ignore_ascii_case("EPSG") {
            return None;
        }
        match auth.children().get(1)? {
            WktNode::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u32),
            WktNode::Text(t) => t.trim().parse().ok(),
            _ => None,
        }
    }
}

/// Parse WKT text into a tree of [`WktNode`]s.
pub(crate) fn parse_wkt_node(input: &str) -> Result<WktNode> {
    struct Parser<'a> {
        chars: &'a [u8],
        pos: usize,
    }
    impl Parser<'_> {
        fn skip_ws(&mut self) {
            while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
        }
        fn parse(&mut self, depth: usize) -> Result<WktNode> {
            if depth > 64 {
                bail!("WKT nested too deeply");
            }
            self.skip_ws();
            let Some(&c) = self.chars.get(self.pos) else { bail!("unexpected end of WKT") };
            if c == b'"' {
                // quoted text, "" is an escaped quote
                let mut out = Vec::new();
                self.pos += 1;
                loop {
                    match self.chars.get(self.pos) {
                        None => bail!("unterminated string in WKT"),
                        Some(b'"') if self.chars.get(self.pos + 1) == Some(&b'"') => {
                            out.push(b'"');
                            self.pos += 2;
                        }
                        Some(b'"') => {
                            self.pos += 1;
                            break;
                        }
                        Some(&b) => {
                            out.push(b);
                            self.pos += 1;
                        }
                    }
                }
                return Ok(WktNode::Text(String::from_utf8_lossy(&out).into_owned()));
            }
            let start = self.pos;
            while self.pos < self.chars.len() && !matches!(self.chars[self.pos], b'[' | b'(' | b']' | b')' | b',') {
                self.pos += 1;
            }
            let token = std::str::from_utf8(&self.chars[start..self.pos])?.trim().to_string();
            self.skip_ws();
            if self.pos < self.chars.len() && matches!(self.chars[self.pos], b'[' | b'(') {
                if token.is_empty() {
                    bail!("missing keyword in WKT at {start}");
                }
                self.pos += 1;
                let mut children = vec![];
                loop {
                    self.skip_ws();
                    if self.pos < self.chars.len() && matches!(self.chars[self.pos], b']' | b')') {
                        self.pos += 1;
                        break;
                    }
                    children.push(self.parse(depth + 1)?);
                    self.skip_ws();
                    match self.chars.get(self.pos) {
                        Some(b',') => self.pos += 1,
                        Some(b']' | b')') => {
                            self.pos += 1;
                            break;
                        }
                        _ => bail!("expected ',' or ']' in WKT at {}", self.pos),
                    }
                }
                Ok(WktNode::Node(token.to_ascii_uppercase(), children))
            } else if token.is_empty() {
                bail!("empty value in WKT at {start}")
            } else if let Ok(n) = token.parse::<f64>() {
                Ok(WktNode::Number(n))
            } else {
                Ok(WktNode::Text(token))
            }
        }
    }
    let mut parser = Parser { chars: input.as_bytes(), pos: 0 };
    let node = parser.parse(0)?;
    parser.skip_ws();
    if parser.pos != parser.chars.len() {
        bail!("unexpected trailing characters in WKT");
    }
    if node.name().is_none() {
        bail!("not a WKT CRS");
    }
    Ok(node)
}

/// Lowercase alphanumeric form of a name for matching (`"Transverse_Mercator"` -> `"transversemercator"`).
fn norm(name: &str) -> String {
    name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

const DEGREE: f64 = std::f64::consts::PI / 180.0;

/// Resolves EPSG codes to definitions (the embedded EPSG table).
pub(crate) type EpsgLookup<'a> = &'a dyn Fn(u32) -> Option<CrsDef>;

/// Convert WKT to a [`CrsDef`].
pub(crate) fn wkt_to_def(wkt: &str, epsg: EpsgLookup) -> Result<CrsDef> {
    let root = parse_wkt_node(wkt.trim())?;
    node_to_def(&root, epsg)
}

fn node_to_def(root: &WktNode, epsg: EpsgLookup) -> Result<CrsDef> {
    match root.name().unwrap_or_default() {
        "GEOGCS" | "GEOGCRS" | "GEOGRAPHICCRS" | "GEODCRS" | "GEODETICCRS" => geographic(root, epsg, None),
        "PROJCS" | "PROJCRS" | "PROJECTEDCRS" => projected(root, epsg, None),
        // horizontal component of a compound CRS
        "COMPD_CS" | "COMPOUNDCRS" => {
            let horizontal = root
                .children()
                .iter()
                .find(|c| {
                    matches!(
                        c.name(),
                        Some(
                            "GEOGCS"
                                | "PROJCS"
                                | "GEOGCRS"
                                | "GEODCRS"
                                | "PROJCRS"
                                | "GEOGRAPHICCRS"
                                | "PROJECTEDCRS"
                                | "BOUNDCRS"
                        )
                    )
                })
                .ok_or_else(|| anyhow!("compound CRS without a horizontal CRS"))?;
            node_to_def(horizontal, epsg)
        }
        // WKT2 bound CRS: source CRS + transformation to WGS84
        "BOUNDCRS" => {
            let source = root
                .child(&["SOURCECRS"])
                .and_then(|s| s.children().iter().find(|c| c.name().is_some()))
                .ok_or_else(|| anyhow!("BOUNDCRS without SOURCECRS"))?;
            let towgs84 = root.child(&["ABRIDGEDTRANSFORMATION"]).map(abridged_transformation).transpose()?;
            match source.name().unwrap_or_default() {
                "GEOGCRS" | "GEOGRAPHICCRS" | "GEODCRS" | "GEODETICCRS" => geographic(source, epsg, towgs84),
                "PROJCRS" | "PROJECTEDCRS" => projected(source, epsg, towgs84),
                other => bail!("unsupported BOUNDCRS source CRS type {other}"),
            }
        }
        "GEOCCS" | "GEOCCRS" => bail!("geocentric CRS are not supported"),
        "VERT_CS" | "VERTCRS" | "VERTICALCRS" => bail!("vertical CRS are not supported"),
        "LOCAL_CS" | "ENGCRS" | "ENGINEERINGCRS" | "FITTED_CS" => bail!("engineering/local CRS are not supported"),
        other => bail!("unsupported WKT CRS type {other}"),
    }
}

/// The geodetic part of a CRS: ellipsoid, datum shift to WGS84, prime meridian, angular unit.
struct Geodetic {
    a: f64,
    /// inverse flattening, 0 for a sphere
    rf: f64,
    towgs84: Option<[f64; 7]>,
    /// datum shift PROJ parameters from the EPSG database, overriding `towgs84`
    epsg_datum: Option<String>,
    /// prime meridian in degrees east of Greenwich
    pm: f64,
    /// radians per angular unit
    angular_unit: f64,
    axes: Vec<Axis>,
    epsg: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Axis {
    East,
    West,
    North,
    South,
    /// polar `"North along 90 deg East"`-style or unknown directions
    Other,
}

fn parse_axes(node: &WktNode) -> Vec<Axis> {
    let mut axes = vec![];
    for axis in node.all(&["AXIS"]) {
        // WKT1 AXIS["name", DIRECTION], WKT2 AXIS["name (abbrev)", direction, ORDER[n], ...]
        let dir = match axis.children().get(1) {
            Some(WktNode::Text(t)) => t.to_ascii_uppercase(),
            _ => String::new(),
        };
        axes.push(match dir.as_str() {
            "EAST" => Axis::East,
            "WEST" => Axis::West,
            "NORTH" => Axis::North,
            "SOUTH" => Axis::South,
            _ => Axis::Other,
        });
    }
    axes
}

/// Axis order (GeoTools `CRS.getAxisOrder`), whether coordinates are (north, east) and the PROJ `+axis` direction
/// flips (`"wsu"`) for a list of axes.
fn axis_info(axes: &[Axis]) -> Result<(AxisOrder, bool, Option<&'static str>)> {
    use Axis::*;
    Ok(match axes {
        [] | [East, North, ..] => (AxisOrder::EastNorth, false, None),
        [North, East, ..] => (AxisOrder::NorthEast, true, None),
        [West, South, ..] => (AxisOrder::EastNorth, false, Some("wsu")),
        [West, North, ..] => (AxisOrder::EastNorth, false, Some("wnu")),
        [East, South, ..] => (AxisOrder::EastNorth, false, Some("esu")),
        [South, West, ..] => (AxisOrder::NorthEast, true, Some("wsu")),
        [North, West, ..] => (AxisOrder::NorthEast, true, Some("wnu")),
        [South, East, ..] => (AxisOrder::NorthEast, true, Some("esu")),
        [Other, Other, ..] => (AxisOrder::Inapplicable, false, None),
        _ => bail!("unsupported axis directions {axes:?}"),
    })
}

/// Unit conversion factor of a `UNIT[name, factor]` / `ANGLEUNIT[...]` / `LENGTHUNIT[...]` node.
fn unit_factor(node: Option<&WktNode>) -> Option<f64> {
    node.and_then(|u| u.number(1)).filter(|f| *f > 0.0 && f.is_finite())
}

/// Snap a unit to an exact value when it is a rounded version of a well-known one (ESRI writes
/// `0.0174532925199433` for degrees and `0.3048006096012192` for US survey feet).
fn snap(value: f64, exact: &[f64]) -> f64 {
    exact.iter().copied().find(|e| ((value - e) / e).abs() < 1e-12).unwrap_or(value)
}

fn angular(value: f64) -> f64 {
    snap(value, &[DEGREE, std::f64::consts::PI / 200.0, 1.0, DEGREE / 3600.0])
}

fn linear(value: f64) -> f64 {
    snap(value, &[1.0, 0.3048, 1200.0 / 3937.0, 0.9144, 1000.0])
}

fn geodetic(node: &WktNode, epsg: EpsgLookup, bound_towgs84: Option<[f64; 7]>) -> Result<Geodetic> {
    let datum = node
        .child(&["DATUM", "GEODETICDATUM", "TRF", "ENSEMBLE"])
        .ok_or_else(|| anyhow!("missing DATUM in {}", node.name().unwrap_or_default()))?;
    let spheroid =
        datum.child(&["SPHEROID", "ELLIPSOID"]).ok_or_else(|| anyhow!("missing SPHEROID/ELLIPSOID in DATUM"))?;
    let a = spheroid.number(1).ok_or_else(|| anyhow!("invalid SPHEROID"))?;
    let rf = spheroid.number(2).ok_or_else(|| anyhow!("invalid SPHEROID"))?;
    let a = a * unit_factor(spheroid.child(&["LENGTHUNIT", "UNIT"])).unwrap_or(1.0);
    if a.is_nan() || a <= 0.0 || !rf.is_finite() || rf < 0.0 {
        bail!("invalid ellipsoid {a} {rf}");
    }
    let towgs84 = match datum.child(&["TOWGS84"]) {
        Some(t) => {
            let values: Vec<f64> = (0..t.children().len()).filter_map(|i| t.number(i)).collect();
            match values.as_slice() {
                [dx, dy, dz] => Some([*dx, *dy, *dz, 0.0, 0.0, 0.0, 0.0]),
                [dx, dy, dz, rx, ry, rz, s] => Some([*dx, *dy, *dz, *rx, *ry, *rz, *s]),
                _ => bail!("invalid TOWGS84"),
            }
        }
        None => bound_towgs84,
    };
    // WKT2: CS-level or CRS-level ANGLEUNIT; WKT1: UNIT of the GEOGCS
    let angular_unit = angular(
        unit_factor(node.child(&["UNIT", "ANGLEUNIT"]))
            .or_else(|| unit_factor(node.all(&["AXIS"]).find_map(|a| a.child(&["ANGLEUNIT", "UNIT"]))))
            .unwrap_or(DEGREE),
    );
    let pm = match node.child(&["PRIMEM", "PRIMEMERIDIAN"]) {
        Some(pm) => {
            let value = pm.number(1).ok_or_else(|| anyhow!("invalid PRIMEM"))?;
            // WKT2 has an explicit unit; in WKT1 (like GeoTools) the prime meridian is in the GEOGCS angular unit
            let unit = unit_factor(pm.child(&["ANGLEUNIT", "UNIT"])).map(angular).unwrap_or(angular_unit);
            value * unit / DEGREE
        }
        None => 0.0,
    };
    let code = node.epsg_code();
    let code = code.filter(|c| *c != 0);
    // like GeoTools: prefer the datum shift from the EPSG database when the geographic CRS has an EPSG code
    let epsg_datum = code.and_then(epsg).map(|def| def.datum_shift_tokens());
    Ok(Geodetic { a, rf, towgs84, epsg_datum, pm, angular_unit, axes: parse_axes(node), epsg: code })
}

impl Geodetic {
    fn is_sphere(&self) -> bool {
        self.rf == 0.0 || self.rf.is_infinite()
    }

    /// PROJ ellipsoid and datum parameters.
    fn proj_tokens(&self, out: &mut String) {
        if self.is_sphere() {
            let _ = write!(out, " +a={} +b={}", self.a, self.a);
        } else {
            let _ = write!(out, " +a={} +rf={}", self.a, self.rf);
        }
        if let Some(datum) = &self.epsg_datum {
            out.push_str(datum);
        } else {
            self.towgs84_tokens(out);
        }
        if self.pm != 0.0 {
            let _ = write!(out, " +pm={}", self.pm);
        }
    }

    fn towgs84_tokens(&self, out: &mut String) {
        match self.towgs84 {
            Some(t) if t[3..].iter().all(|v| *v == 0.0) => {
                let _ = write!(out, " +towgs84={},{},{}", t[0], t[1], t[2]);
            }
            Some(t) => {
                let _ = write!(out, " +towgs84={},{},{},{},{},{},{}", t[0], t[1], t[2], t[3], t[4], t[5], t[6]);
            }
            // GeoTools lenient mode: without Bursa-Wolf parameters, coordinates are only moved between ellipsoids
            // (a Molodensky transformation without translation)
            None => out.push_str(" +molodensky=0,0,0"),
        }
    }
}

/// WKT2 `ABRIDGEDTRANSFORMATION[..., METHOD[...], PARAMETER[...]...]` to TOWGS84 (position vector) parameters.
fn abridged_transformation(node: &WktNode) -> Result<[f64; 7]> {
    let method = node.child(&["METHOD"]).and_then(|m| m.text(0)).map(norm).unwrap_or_default();
    let method_code = node.child(&["METHOD"]).and_then(WktNode::epsg_code);
    let coordinate_frame = method.contains("coordinateframe") || matches!(method_code, Some(9607 | 1032 | 9633));
    let mut result = [0.0; 7];
    for param in node.all(&["PARAMETER"]) {
        let (Some(name), Some(value)) = (param.text(0), param.number(1)) else { continue };
        let idx = match (param.epsg_code(), norm(name).as_str()) {
            (Some(8605), _) | (_, "xaxistranslation") => 0,
            (Some(8606), _) | (_, "yaxistranslation") => 1,
            (Some(8607), _) | (_, "zaxistranslation") => 2,
            (Some(8608), _) | (_, "xaxisrotation") => 3,
            (Some(8609), _) | (_, "yaxisrotation") => 4,
            (Some(8610), _) | (_, "zaxisrotation") => 5,
            (Some(8611), _) | (_, "scaledifference") => 6,
            _ => continue,
        };
        // TOWGS84 units: meters, arc-seconds, parts per million
        let unit = unit_factor(param.child(&["LENGTHUNIT", "ANGLEUNIT", "SCALEUNIT", "UNIT"]));
        result[idx] = match (idx, unit) {
            (3..=5, Some(f)) => value * f / (DEGREE / 3600.0),
            (6, Some(f)) => value * f * 1e6,
            (_, Some(f)) => value * f,
            (_, None) => value,
        };
    }
    if coordinate_frame {
        for r in &mut result[3..6] {
            *r = -*r;
        }
    }
    Ok(result)
}

fn geographic(node: &WktNode, epsg: EpsgLookup, bound_towgs84: Option<[f64; 7]>) -> Result<CrsDef> {
    if let Some(cs) = node.child(&["CS"])
        && cs.text(0).is_some_and(|t| !t.eq_ignore_ascii_case("ellipsoidal"))
    {
        bail!("unsupported geodetic CRS coordinate system {}", cs.text(0).unwrap_or_default());
    }
    let g = geodetic(node, epsg, bound_towgs84)?;
    let (axis_order, swap, _) = axis_info(&g.axes)?;
    let mut proj = "+proj=longlat".to_string();
    g.proj_tokens(&mut proj);
    proj.push_str(" +no_defs");
    Ok(CrsDef { proj, axis_order, swap, angular_unit: g.angular_unit })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Param {
    FalseEasting,
    FalseNorthing,
    CentralMeridian,
    LatitudeOfOrigin,
    ScaleFactor,
    StandardParallel1,
    StandardParallel2,
    Azimuth,
    RectifiedGridAngle,
    PseudoStandardParallel,
    AuxiliarySphereType,
    XScale,
    YScale,
}

/// Unit kind of a parameter for unit conversion.
#[derive(PartialEq)]
enum ParamUnit {
    Angle,
    Length,
    Scale,
}

impl Param {
    fn from(name: &str, code: Option<u32>) -> Option<Param> {
        use Param::*;
        Some(match (code, norm(name).as_str()) {
            (Some(8806 | 8826 | 8816), _)
            | (
                _,
                "falseeasting" | "eastingatfalseorigin" | "eastingatprojectioncentre" | "eastingatprojectioncenter",
            ) => FalseEasting,
            (Some(8807 | 8827 | 8817), _)
            | (
                _,
                "falsenorthing" | "northingatfalseorigin" | "northingatprojectioncentre" | "northingatprojectioncenter",
            ) => FalseNorthing,
            (Some(8802 | 8822 | 8812), _)
            | (
                _,
                "centralmeridian"
                | "longitudeofnaturalorigin"
                | "longitudeoforigin"
                | "longitudeofcenter"
                | "longitudeofcentre"
                | "longitudeoffalseorigin"
                | "longitudeofprojectioncentre"
                | "longitudeofprojectioncenter",
            ) => CentralMeridian,
            (Some(8801 | 8821 | 8811), _)
            | (
                _,
                "latitudeoforigin"
                | "latitudeofnaturalorigin"
                | "latitudeofcenter"
                | "latitudeofcentre"
                | "latitudeoffalseorigin"
                | "latitudeofprojectioncentre"
                | "latitudeofprojectioncenter",
            ) => LatitudeOfOrigin,
            (Some(8805 | 8815 | 8819), _)
            | (
                _,
                "scalefactor"
                | "scalefactoratnaturalorigin"
                | "scalefactoroninitialline"
                | "scalefactoronpseudostandardparallel"
                | "scalefactoratcentreline",
            ) => ScaleFactor,
            (Some(8823 | 8832), _)
            | (_, "standardparallel1" | "latitudeof1ststandardparallel" | "latitudeofstandardparallel") => {
                StandardParallel1
            }
            (Some(8824), _) | (_, "standardparallel2" | "latitudeof2ndstandardparallel") => StandardParallel2,
            (Some(8813 | 1036), _)
            | (_, "azimuth" | "azimuthofinitialline" | "azimuthofcentreline" | "colatitudeofconeaxis") => Azimuth,
            (Some(8814), _)
            | (
                _,
                "rectifiedgridangle"
                | "anglefromrectifiedtoskewgrid"
                | "angefromrectifiedtoskewgrid"
                | "xyplanerotation",
            ) => RectifiedGridAngle,
            (Some(8818), _) | (_, "pseudostandardparallel1" | "latitudeofpseudostandardparallel") => {
                PseudoStandardParallel
            }
            (_, "auxiliaryspheretype") => AuxiliarySphereType,
            (_, "xscale") => XScale,
            (_, "yscale") => YScale,
            _ => return None,
        })
    }

    fn unit(self) -> ParamUnit {
        match self {
            Param::FalseEasting | Param::FalseNorthing => ParamUnit::Length,
            Param::ScaleFactor | Param::AuxiliarySphereType | Param::XScale | Param::YScale => ParamUnit::Scale,
            _ => ParamUnit::Angle,
        }
    }
}

struct Params(Vec<(Param, f64)>);

impl Params {
    fn get(&self, p: Param) -> Option<f64> {
        self.0.iter().find(|(k, _)| *k == p).map(|(_, v)| *v)
    }
    fn or(&self, p: Param, default: f64) -> f64 {
        self.get(p).unwrap_or(default)
    }
}

/// Projection methods, identified by EPSG method code or (normalized) name.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Method {
    TransverseMercator,
    TransverseMercatorSouth,
    LambertConformalConic,
    AlbersEqualArea,
    ObliqueStereographic,
    Stereographic,
    PolarStereographicA,
    PolarStereographicB,
    StereographicNorthPole,
    StereographicSouthPole,
    LambertAzimuthalEqualArea,
    Mercator1Sp,
    Mercator2Sp,
    PseudoMercator,
    MercatorAuxiliarySphere,
    EquidistantCylindrical,
    CassiniSoldner,
    HotineA,
    HotineB,
    Krovak,
    KrovakNorth,
    AzimuthalEquidistant,
    Polyconic,
    EqualEarth,
    CylindricalEqualArea,
    Mollweide,
    Sinusoidal,
    Robinson,
    EquidistantConic,
    Gnomonic,
    Orthographic,
    NewZealandMapGrid,
    MillerCylindrical,
}

impl Method {
    fn from(name: &str, code: Option<u32>) -> Result<Method> {
        use Method::*;
        let n = norm(name);
        Ok(match (code, n.as_str()) {
            (Some(9807), _)
            | (_, "transversemercator" | "gausskruger" | "gaussianconformal" | "transversemercatorcomplex") => {
                TransverseMercator
            }
            (Some(9808), _) | (_, "transversemercatorsouthorientated" | "transversemercatorsouthoriented") => {
                TransverseMercatorSouth
            }
            (Some(9801 | 9802 | 9817 | 1102), _)
            | (
                _,
                "lambertconformalconic"
                | "lambertconformalconic1sp"
                | "lambertconformalconic2sp"
                | "lambertconicconformal1sp"
                | "lambertconicconformal2sp"
                | "lambertconformalconicnearconformal",
            ) => LambertConformalConic,
            (Some(9822), _) | (_, "albersconicequalarea" | "albers" | "albersequalarea") => AlbersEqualArea,
            (Some(9809), _) | (_, "obliquestereographic" | "doublestereographic") => ObliqueStereographic,
            (_, "stereographic") => Stereographic,
            (Some(9810), _) | (_, "polarstereographic" | "polarstereographicvarianta") => PolarStereographicA,
            (Some(9829), _) | (_, "polarstereographicvariantb") => PolarStereographicB,
            (_, "stereographicnorthpole") => StereographicNorthPole,
            (_, "stereographicsouthpole") => StereographicSouthPole,
            (Some(9820 | 1027), _) | (_, "lambertazimuthalequalarea" | "lambertazimuthalequalareaspherical") => {
                LambertAzimuthalEqualArea
            }
            (Some(9804 | 9841), _) | (_, "mercator1sp" | "mercatorvarianta" | "mercator1spspherical") => Mercator1Sp,
            (Some(9805), _) | (_, "mercator2sp" | "mercatorvariantb" | "mercator") => Mercator2Sp,
            (Some(1024), _) | (_, "popularvisualisationpseudomercator" | "pseudomercator" | "googlemercator") => {
                PseudoMercator
            }
            (_, "mercatorauxiliarysphere") => MercatorAuxiliarySphere,
            (Some(1028 | 1029 | 9823 | 9842), _)
            | (
                _,
                "equidistantcylindrical"
                | "equidistantcylindricalspherical"
                | "equirectangular"
                | "platecarree"
                | "platecaree",
            ) => EquidistantCylindrical,
            (Some(9806), _) | (_, "cassinisoldner" | "cassini") => CassiniSoldner,
            (Some(9812), _)
            | (
                _,
                "hotineobliquemercator"
                | "hotineobliquemercatorvarianta"
                | "hotineobliquemercatorazimuthnaturalorigin"
                | "rectifiedskeworthomorphic"
                | "rectifiedskeworthomorphicnaturalorigin",
            ) => HotineA,
            (Some(9815), _)
            | (
                _,
                "obliquemercator"
                | "hotineobliquemercatorvariantb"
                | "hotineobliquemercatorazimuthcenter"
                | "rectifiedskeworthomorphiccenter"
                | "swissobliquecylindrical"
                | "swissobliquemercator",
            ) => HotineB,
            (Some(9819), _) | (_, "krovak" | "krovakobliqueconformalconic") => Krovak,
            (Some(1041), _) | (_, "krovaknorthorientated" | "krovaknorthoriented") => KrovakNorth,
            (Some(1125), _) | (_, "azimuthalequidistant") => AzimuthalEquidistant,
            (Some(9818), _) | (_, "polyconic" | "americanpolyconic") => Polyconic,
            (Some(1078), _) | (_, "equalearth") => EqualEarth,
            (Some(9834 | 9835), _)
            | (
                _,
                "lambertcylindricalequalarea"
                | "lambertcylindricalequalareaspherical"
                | "cylindricalequalarea"
                | "behrmann",
            ) => CylindricalEqualArea,
            (_, "mollweide") => Mollweide,
            (_, "sinusoidal") => Sinusoidal,
            (_, "robinson") => Robinson,
            (_, "equidistantconic") => EquidistantConic,
            (_, "gnomonic") => Gnomonic,
            (Some(9840), _) | (_, "orthographic") => Orthographic,
            (Some(9811), _) | (_, "newzealandmapgrid") => NewZealandMapGrid,
            (_, "millercylindrical") => MillerCylindrical,
            _ => bail!(
                "unsupported projection \"{name}\"{} (supported: transverse mercator incl. UTM and south orientated, \
                 lambert conformal conic 1SP/2SP, albers, oblique/polar stereographic, lambert azimuthal equal area, \
                 mercator 1SP/2SP, pseudo mercator, equidistant cylindrical, cassini-soldner, hotine oblique mercator \
                 A/B, krovak, azimuthal equidistant, polyconic, equal earth, cylindrical equal area, mollweide, \
                 sinusoidal, robinson, equidistant conic, gnomonic, orthographic, new zealand map grid, miller)",
                code.map(|c| format!(" (EPSG method {c})")).unwrap_or_default()
            ),
        })
    }
}

fn projected(root: &WktNode, epsg: EpsgLookup, bound_towgs84: Option<[f64; 7]>) -> Result<CrsDef> {
    let base = root
        .child(&["GEOGCS", "BASEGEOGCRS", "BASEGEODCRS", "GEOGCRS", "GEODCRS"])
        .ok_or_else(|| anyhow!("missing GEOGCS in PROJCS"))?;
    let mut geodetic = geodetic(base, epsg, bound_towgs84).context("invalid base geographic CRS")?;
    // GeoTools also uses the EPSG datum shift if only the projected CRS has an EPSG code
    if geodetic.epsg.is_none()
        && let Some(def) = root.epsg_code().and_then(epsg)
    {
        geodetic.epsg_datum = Some(def.datum_shift_tokens());
    }

    // WKT1: PROJECTION + PARAMETERs on the PROJCS; WKT2: CONVERSION[name, METHOD[...], PARAMETER[...]...]
    let (method_node, param_parent) = match root.child(&["CONVERSION", "DERIVINGCONVERSION"]) {
        Some(conversion) => (conversion.child(&["METHOD", "PROJECTION"]), conversion),
        None => (root.child(&["PROJECTION"]), root),
    };
    let method_node = method_node.ok_or_else(|| anyhow!("missing PROJECTION"))?;
    let method_name = method_node.text(0).unwrap_or_default();
    let method = Method::from(method_name, method_node.epsg_code())?;

    // linear unit: WKT1 UNIT of the PROJCS; WKT2 LENGTHUNIT of the CRS or of the axes
    let axes = parse_axes(root);
    let linear_unit = linear(
        unit_factor(root.child(&["UNIT", "LENGTHUNIT"]))
            .or_else(|| unit_factor(root.all(&["AXIS"]).find_map(|a| a.child(&["LENGTHUNIT", "UNIT"]))))
            .unwrap_or(1.0),
    );

    let mut params = Params(vec![]);
    for param in param_parent.all(&["PARAMETER"]) {
        let (Some(name), Some(value)) = (param.text(0), param.number(1)) else { continue };
        let Some(key) = Param::from(name, param.epsg_code()) else { continue };
        let explicit_unit = unit_factor(param.child(&["ANGLEUNIT", "LENGTHUNIT", "SCALEUNIT", "UNIT"]));
        let value = match key.unit() {
            // in degrees
            ParamUnit::Angle => value * explicit_unit.map(angular).unwrap_or(geodetic.angular_unit) / DEGREE,
            // in meters
            ParamUnit::Length => value * explicit_unit.map(linear).unwrap_or(linear_unit),
            ParamUnit::Scale => value * explicit_unit.unwrap_or(1.0),
        };
        params.0.push((key, value));
    }

    let (axis_order, swap, axis_flip) = axis_info(&axes)?;
    let mut proj = String::new();
    let lon_0 = params.or(Param::CentralMeridian, 0.0);
    let lat_0 = params.or(Param::LatitudeOfOrigin, 0.0);
    let k = params.or(Param::ScaleFactor, 1.0);
    let fe = params.or(Param::FalseEasting, 0.0);
    let fn_ = params.or(Param::FalseNorthing, 0.0);
    let mut flip = axis_flip;
    let mut sphere_override = None;
    use Method::*;
    match method {
        TransverseMercator | TransverseMercatorSouth => {
            let _ = write!(proj, "+proj=tmerc +lat_0={lat_0} +lon_0={lon_0} +k={k}");
        }
        LambertConformalConic => {
            let lat_1 = params.get(Param::StandardParallel1).unwrap_or(lat_0);
            let lat_2 = params.get(Param::StandardParallel2).unwrap_or(lat_1);
            if norm(method_name).contains("belgium") || method_node.epsg_code() == Some(9803) {
                bail!("unsupported projection Lambert Conic Conformal (2SP Belgium)");
            }
            let _ = write!(proj, "+proj=lcc +lat_0={lat_0} +lon_0={lon_0} +lat_1={lat_1} +lat_2={lat_2} +k={k}");
        }
        AlbersEqualArea => {
            let lat_1 = params.or(Param::StandardParallel1, lat_0);
            let lat_2 = params.or(Param::StandardParallel2, lat_1);
            let _ = write!(proj, "+proj=aea +lat_0={lat_0} +lon_0={lon_0} +lat_1={lat_1} +lat_2={lat_2}");
        }
        ObliqueStereographic => {
            let _ = write!(proj, "+proj=sterea +lat_0={lat_0} +lon_0={lon_0} +k={k}");
        }
        Stereographic => {
            let _ = write!(proj, "+proj=stere +lat_0={lat_0} +lon_0={lon_0} +k={k}");
        }
        PolarStereographicA => {
            // GeoTools only uses the sign of latitude_of_origin
            let lat_0 = if lat_0 < 0.0 { -90.0 } else { 90.0 };
            match params.get(Param::StandardParallel1) {
                Some(lat_ts) => {
                    let lat_0 = if lat_ts < 0.0 { -90.0 } else { 90.0 };
                    let _ = write!(proj, "+proj=stere +lat_0={lat_0} +lat_ts={lat_ts} +lon_0={lon_0}");
                }
                None => {
                    let _ = write!(proj, "+proj=stere +lat_0={lat_0} +lon_0={lon_0} +k={k}");
                }
            }
        }
        PolarStereographicB | StereographicNorthPole | StereographicSouthPole => {
            let lat_ts = params
                .get(Param::StandardParallel1)
                .or(params.get(Param::LatitudeOfOrigin))
                .unwrap_or(if method == StereographicSouthPole { -90.0 } else { 90.0 });
            let south = match method {
                StereographicNorthPole => false,
                StereographicSouthPole => true,
                _ => lat_ts < 0.0,
            };
            let lat_0 = if south { -90.0 } else { 90.0 };
            let _ = write!(proj, "+proj=stere +lat_0={lat_0} +lat_ts={lat_ts} +lon_0={lon_0}");
        }
        LambertAzimuthalEqualArea => {
            let _ = write!(proj, "+proj=laea +lat_0={lat_0} +lon_0={lon_0}");
        }
        // like GeoTools, a non-zero latitude of origin is used as standard parallel
        Mercator1Sp if lat_0 != 0.0 && k == 1.0 => {
            let _ = write!(proj, "+proj=merc +lon_0={lon_0} +lat_ts={}", lat_0.abs());
        }
        Mercator1Sp => {
            let _ = write!(proj, "+proj=merc +lon_0={lon_0} +k={k}");
        }
        Mercator2Sp => match params.get(Param::StandardParallel1) {
            Some(lat_ts) => {
                let _ = write!(proj, "+proj=merc +lon_0={lon_0} +lat_ts={lat_ts}");
            }
            None => {
                let _ = write!(proj, "+proj=merc +lon_0={lon_0} +k={k}");
            }
        },
        PseudoMercator | MercatorAuxiliarySphere => {
            let aux = params.or(Param::AuxiliarySphereType, 0.0);
            if aux != 0.0 {
                bail!("unsupported Mercator_Auxiliary_Sphere type {aux} (only 0 is supported)");
            }
            let _ = write!(proj, "+proj=merc +lon_0={lon_0} +k=1");
            sphere_override = Some(geodetic.a);
        }
        EquidistantCylindrical => {
            let lat_ts = params.or(Param::StandardParallel1, 0.0);
            let _ = write!(proj, "+proj=eqc +lat_ts={lat_ts} +lat_0={lat_0} +lon_0={lon_0}");
        }
        CassiniSoldner => {
            let _ = write!(proj, "+proj=cass +lat_0={lat_0} +lon_0={lon_0}");
        }
        HotineA | HotineB => {
            let alpha = params.or(Param::Azimuth, 90.0);
            let gamma = params.or(Param::RectifiedGridAngle, alpha);
            let _ = write!(proj, "+proj=omerc +lat_0={lat_0} +lonc={lon_0} +alpha={alpha} +gamma={gamma} +k={k}");
            if method == HotineA {
                proj.push_str(" +no_uoff");
            }
        }
        Krovak | KrovakNorth => {
            let lat_ts = params.or(Param::PseudoStandardParallel, 78.5);
            let alpha = params.or(Param::Azimuth, 30.288_139_752_777_78);
            if (lat_ts - 78.5).abs() > 1e-9 || (alpha - 30.288_139_752_777_78).abs() > 1e-6 {
                bail!("unsupported Krovak parameters (only the standard S-JTSK pseudo standard parallel and azimuth)");
            }
            let _ = write!(proj, "+proj=krovak +lat_0={lat_0} +lon_0={lon_0} +k={k}");
            // the math produces (negative) easting/northing, AXIS[..., WEST], AXIS[..., SOUTH] flips them. ESRI .prj
            // files have no AXIS: X_Scale=-1 + XY_Plane_Rotation=90 means east/north, otherwise positive
            // westing/southing
            if axes.is_empty() && method == Krovak {
                let x_scale = params.or(Param::XScale, 1.0);
                let rotation = params.or(Param::RectifiedGridAngle, 0.0);
                flip = match (x_scale < 0.0, rotation) {
                    (true, r) if (r - 90.0).abs() < 1e-9 => None,
                    (false, r) if r.abs() < 1e-9 => Some("wsu"),
                    _ => bail!("unsupported Krovak X_Scale/XY_Plane_Rotation combination"),
                };
            }
        }
        AzimuthalEquidistant => {
            let _ = write!(proj, "+proj=aeqd +lat_0={lat_0} +lon_0={lon_0}");
        }
        Polyconic => {
            let _ = write!(proj, "+proj=poly +lat_0={lat_0} +lon_0={lon_0}");
        }
        EqualEarth => {
            // GeoTools only implements the spherical form (with R = a)
            let _ = write!(proj, "+proj=eqearth +lon_0={lon_0} +spherical");
        }
        CylindricalEqualArea => {
            let lat_ts = params.or(Param::StandardParallel1, if norm(method_name) == "behrmann" { 30.0 } else { 0.0 });
            let _ = write!(proj, "+proj=cea +lat_ts={lat_ts} +lon_0={lon_0}");
        }
        Mollweide => {
            let _ = write!(proj, "+proj=moll +lon_0={lon_0}");
        }
        Sinusoidal => {
            let _ = write!(proj, "+proj=sinu +lon_0={lon_0}");
        }
        Robinson => {
            let _ = write!(proj, "+proj=robin +lon_0={lon_0}");
        }
        EquidistantConic => {
            let lat_1 = params.or(Param::StandardParallel1, lat_0);
            let lat_2 = params.or(Param::StandardParallel2, lat_1);
            let _ = write!(proj, "+proj=eqdc +lat_0={lat_0} +lon_0={lon_0} +lat_1={lat_1} +lat_2={lat_2}");
        }
        Gnomonic | Orthographic => {
            let name = if method == Gnomonic { "gnom" } else { "ortho" };
            let _ = write!(proj, "+proj={name} +lat_0={lat_0} +lon_0={lon_0}");
        }
        NewZealandMapGrid => {
            proj.push_str("+proj=nzmg");
        }
        MillerCylindrical => {
            let _ = write!(proj, "+proj=mill +lon_0={lon_0}");
        }
    }
    let _ = write!(proj, " +x_0={fe} +y_0={fn_}");
    match sphere_override {
        // web mercator: spherical projection of geographic coordinates, no datum shift
        Some(r) => {
            let _ = write!(proj, " +a={r} +b={r} +nadgrids=@null");
            if geodetic.pm != 0.0 {
                let _ = write!(proj, " +pm={}", geodetic.pm);
            }
        }
        None => geodetic.proj_tokens(&mut proj),
    }
    if linear_unit != 1.0 {
        let _ = write!(proj, " +to_meter={linear_unit}");
    }
    if let Some(flip) = flip {
        let _ = write!(proj, " +axis={flip}");
    }
    proj.push_str(" +no_defs");
    Ok(CrsDef { proj, axis_order, swap, angular_unit: DEGREE })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_epsg(_: u32) -> Option<CrsDef> {
        None
    }

    #[test]
    fn parse_nodes() {
        let node = parse_wkt_node(r#"A["x ""y""", 1.5, B[C], D("z")]"#).unwrap();
        assert_eq!(node.name(), Some("A"));
        assert_eq!(node.text(0), Some(r#"x "y""#));
        assert_eq!(node.number(1), Some(1.5));
        assert!(parse_wkt_node("A[").is_err());
        assert!(parse_wkt_node("A[1]]").is_err());
        assert!(parse_wkt_node("garbage").is_err());
    }

    #[test]
    fn esri_state_plane_feet() {
        let def = wkt_to_def(
            r#"PROJCS["NAD_1983_StatePlane_New_York_Long_Island_FIPS_3104_Feet",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",984250.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-74.0],PARAMETER["Standard_Parallel_1",40.66666666666666],PARAMETER["Standard_Parallel_2",41.03333333333333],PARAMETER["Latitude_Of_Origin",40.16666666666666],UNIT["Foot_US",0.3048006096012192]]"#,
            &no_epsg,
        )
        .unwrap();
        assert!(def.proj.starts_with("+proj=lcc"), "{}", def.proj);
        assert!(def.proj.contains("+x_0=300000 ") || def.proj.contains("+x_0=300000.0000"), "{}", def.proj);
        assert_eq!(def.axis_order, AxisOrder::EastNorth);
    }

    #[test]
    fn unsupported_projection_message() {
        let err = wkt_to_def(
            r#"PROJCS["x",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Winkel_Tripel"],UNIT["Meter",1.0]]"#,
            &no_epsg,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsupported projection \"Winkel_Tripel\""), "{err}");
    }
}
