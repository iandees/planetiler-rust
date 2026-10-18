//! Coordinate reference systems and transforms (replacement for the GeoTools `CRS` / `MathTransform` helpers in
//! `GeoUtils.java`), in pure Rust on top of `proj4rs`.
//!
//! Supported CRS definitions:
//! - EPSG codes: `EPSG:1234`, `urn:ogc:def:crs:EPSG::1234`, `http://www.opengis.net/def/crs/EPSG/0/1234`, ...
//!   Every 2D geographic and projected CRS that GeoTools' EPSG database (`gt-epsg-hsql`) can decode is in an
//!   embedded table ([`epsg`]) generated from GeoTools, with GeoTools' datum shifts to WGS84 and its EPSG axis
//!   order: like GeoTools `CRS.decode(code, longitudeFirst)`, EPSG codes use the EPSG axis order (latitude first
//!   for geographic CRS, northing first for some projected ones) unless `longitude_first` is requested.
//! - `CRS:84`, `OGC:CRS84`, `WGS84` (longitude/latitude), and the web mercator aliases `EPSG:900913`,
//!   `EPSG:102100`, `EPSG:102113`, `ESRI:102100`, ...
//! - PROJ strings (`+proj=...`) via `proj4rs`, plus the projections in [`projections`] (with PROJ semantics: no
//!   datum shift unless `+towgs84`/`+datum` say so). GeoTools does not accept PROJ strings.
//! - WKT: WKT1 (OGC, ESRI `.prj` and GDAL flavors) and WKT2 geographic and projected CRS, see [`wkt`].
//!
//! Datum shifts use 3/7-parameter (Bursa-Wolf, `+towgs84`) or Molodensky (the non-standard `+molodensky=dx,dy,dz`
//! PROJ parameter) transformations, whichever GeoTools uses. Like GeoTools in lenient mode (which planetiler uses),
//! a datum without parameters is treated as WGS84 and coordinates are only moved between ellipsoids
//! (`+molodensky=0,0,0`). Grid based shifts (NTv2, NADCON, `+nadgrids=`) are not supported. Coordinates are 2D:
//! ellipsoidal heights are dropped after datum shifts, like GeoTools' 2D transforms.

mod epsg;
pub(crate) mod projections;
pub(crate) mod wkt;

use std::f64::consts::FRAC_PI_2;
use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use geo_types::Coord;

use projections::{CUSTOM_PROJECTIONS, CustomProjection, Ellipsoid, ProjParams};

const EARTH_RADIUS: f64 = 6_378_137.0;
pub(crate) const DEGREE: f64 = std::f64::consts::PI / 180.0;

/// Order of the first 2 axes of a CRS (GeoTools `CRS.AxisOrder`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxisOrder {
    /// longitude/easting first
    EastNorth,
    /// latitude/northing first
    NorthEast,
    /// neither, for example polar CRS with axes like `"South along 45 deg East"`
    Inapplicable,
}

/// A CRS definition: a PROJ string plus axis information.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CrsDef {
    /// PROJ string; `+pm=` (in degrees) is applied by this module, not `proj4rs`
    pub proj: String,
    /// GeoTools axis order
    pub axis_order: AxisOrder,
    /// whether coordinates are (north, east) relative to the PROJ string
    pub swap: bool,
    /// radians per unit of geographic coordinates (`+proj=longlat` only)
    pub angular_unit: f64,
}

impl CrsDef {
    /// The datum shift parameters of this definition (` +towgs84=...`, ` +molodensky=...` or empty for none).
    pub(crate) fn datum_shift_tokens(&self) -> String {
        self.proj
            .split_whitespace()
            .filter(|t| {
                let t = t.trim_start_matches('+');
                t.starts_with("towgs84=") || t.starts_with("molodensky=")
            })
            .map(|t| format!(" {t}"))
            .collect()
    }
}

/// How projected/geographic coordinates of a [`ProjCrs`] map to geodetic coordinates on its own datum.
enum Projection {
    /// geographic coordinates in `unit` (radians per unit)
    LatLong { unit: f64 },
    /// a `proj4rs` projection (without prime meridian; same datum as [`ProjCrs::geodetic`])
    Proj4rs(Box<proj4rs::Proj>),
    /// a projection implemented in [`projections`]
    Custom(CustomProjection),
}

/// How geodetic coordinates on the datum of a [`ProjCrs`] map to WGS84.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DatumShift {
    /// the datum is equivalent to WGS84
    None,
    /// `proj4rs` datum transformation (`+towgs84` Bursa-Wolf parameters) from [`ProjCrs::geodetic`]
    Proj4rs,
    /// A (non-abridged, 2D) Molodensky transformation (non-standard `+molodensky=dx,dy,dz` PROJ parameter) from an
    /// ellipsoid with semi-major axis `a` and flattening `f`, which GeoTools uses for some EPSG operations, and
    /// without translation as its lenient "ellipsoid shift" for datums without Bursa-Wolf parameters.
    Molodensky { a: f64, f: f64, t: [f64; 3] },
}

const WGS84_A: f64 = 6_378_137.0;
const WGS84_F: f64 = 1.0 / 298.257_223_563;

/// Molodensky transformation (h = 0) of (lam, phi) radians from an ellipsoid (a, f) to (a + da, f + df) with
/// translation `t`, like GeoTools' `MolodenskyTransform` in 2D.
fn molodensky(lam: f64, phi: f64, a: f64, f: f64, da: f64, df: f64, t: [f64; 3]) -> (f64, f64) {
    let [dx, dy, dz] = t;
    let e2 = 2.0 * f - f * f;
    let (sphi, cphi) = phi.sin_cos();
    let (slam, clam) = lam.sin_cos();
    let w = 1.0 - e2 * sphi * sphi;
    let rn = a / w.sqrt();
    let rm = a * (1.0 - e2) / (w * w.sqrt());
    let b = a * (1.0 - f);
    let dphi = (-dx * sphi * clam - dy * sphi * slam
        + dz * cphi
        + da * rn * e2 * sphi * cphi / a
        + df * (rm * a / b + rn * b / a) * sphi * cphi)
        / rm;
    let dlam = if cphi.abs() < 1e-15 { 0.0 } else { (-dx * slam + dy * clam) / (rn * cphi) };
    (lam + dlam, phi + dphi)
}

/// A CRS backed by a PROJ string.
struct ProjCrs {
    proj: String,
    projection: Projection,
    /// `+proj=longlat` on the datum of this CRS
    geodetic: proj4rs::Proj,
    datum: DatumShift,
    /// prime meridian, radians east of Greenwich
    pm: f64,
}

impl ProjCrs {
    fn new(def: &CrsDef) -> Result<ProjCrs> {
        let params = ProjParams::parse(&def.proj);
        let name = params.proj().ok_or_else(|| anyhow!("missing +proj in {}", def.proj))?.to_string();
        let pm = match params.get("pm") {
            None => 0.0,
            Some(pm) => prime_meridian(pm).ok_or_else(|| anyhow!("unsupported prime meridian +pm={pm}"))?,
        };
        if params.get("nadgrids").is_some_and(|g| g != "@null" && g != "null") {
            bail!("grid based datum shifts (+nadgrids={}) are not supported", params.get("nadgrids").unwrap_or(""));
        }
        let datum_tokens = params.datum_tokens();
        let geodetic = proj4rs::Proj::from_proj_string(&format!("+proj=longlat{datum_tokens} +no_defs"))
            .map_err(|e| anyhow!("invalid datum/ellipsoid in {}: {e}", def.proj))?;
        let datum = if let Some(m) = params.get("molodensky") {
            let t: Vec<f64> = m.split(',').filter_map(|v| v.parse().ok()).collect();
            let [dx, dy, dz] = t[..] else { bail!("invalid +molodensky={m}, expected dx,dy,dz") };
            let Ellipsoid { a, es } = ellipsoid(&datum_tokens)?;
            let f = 1.0 - (1.0 - es).sqrt();
            if (a - WGS84_A).abs() < 1e-6 && (f - WGS84_F).abs() < 1e-12 && dx == 0.0 && dy == 0.0 && dz == 0.0 {
                DatumShift::None
            } else {
                DatumShift::Molodensky { a, f, t: [dx, dy, dz] }
            }
        } else if is_wgs84_equivalent(&geodetic)? {
            DatumShift::None
        } else {
            DatumShift::Proj4rs
        };
        // proj4rs does not handle +pm correctly, it is applied here instead; +molodensky is not a PROJ parameter
        let proj4rs_def: String = def
            .proj
            .split_whitespace()
            .filter(|t| {
                let t = t.trim_start_matches('+');
                !t.starts_with("pm=") && !t.starts_with("molodensky=")
            })
            .collect::<Vec<_>>()
            .join(" ");
        let projection = match name.as_str() {
            "longlat" | "latlong" | "lonlat" | "latlon" => Projection::LatLong { unit: def.angular_unit },
            n if CUSTOM_PROJECTIONS.contains(&n) => {
                Projection::Custom(CustomProjection::new(&params, ellipsoid(&datum_tokens)?)?)
            }
            _ => Projection::Proj4rs(Box::new(proj4rs::Proj::from_proj_string(&proj4rs_def).map_err(|e| {
                if matches!(e, proj4rs::errors::Error::ProjectionNotFound) {
                    anyhow!("unsupported projection +proj={name}")
                } else {
                    anyhow!("invalid projection {}: {e}", def.proj)
                }
            })?)),
        };
        Ok(ProjCrs { proj: def.proj.clone(), projection, geodetic, datum, pm })
    }

    /// True if this is WGS84 longitude/latitude in degrees.
    fn is_wgs84(&self) -> bool {
        matches!(self.projection, Projection::LatLong { unit } if (unit - DEGREE).abs() < 1e-15)
            && self.datum == DatumShift::None
            && self.pm == 0.0
    }

    /// Convert from this CRS (in east/north order) to WGS84 lon/lat degrees.
    fn to_lon_lat(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        let (mut lam, mut phi) = match &self.projection {
            Projection::LatLong { unit } => (c.x * unit, c.y * unit),
            Projection::Proj4rs(proj) => {
                let mut p = (c.x, c.y, 0.0);
                proj4rs::transform::transform(proj, &self.geodetic, &mut p)
                    .map_err(|e| anyhow!("transform error: {e}"))?;
                (p.0, p.1)
            }
            Projection::Custom(proj) => proj.inverse(c.x, c.y)?,
        };
        lam += self.pm;
        match self.datum {
            DatumShift::None => {}
            DatumShift::Proj4rs => {
                let mut p = (lam, phi, 0.0);
                proj4rs::transform::transform(&self.geodetic, wgs84_proj()?, &mut p)
                    .map_err(|e| anyhow!("transform error: {e}"))?;
                (lam, phi) = (p.0, p.1);
            }
            DatumShift::Molodensky { a, f, t } => (lam, phi) = molodensky(lam, phi, a, f, WGS84_A - a, WGS84_F - f, t),
        }
        Ok(Coord { x: lam.to_degrees(), y: phi.to_degrees() })
    }

    /// Convert WGS84 lon/lat degrees to this CRS (in east/north order).
    fn lon_lat_to_crs(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        let (mut lam, mut phi) = (c.x.to_radians(), c.y.to_radians());
        match self.datum {
            DatumShift::None => {}
            DatumShift::Proj4rs => {
                let mut p = (lam, phi, 0.0);
                proj4rs::transform::transform(wgs84_proj()?, &self.geodetic, &mut p)
                    .map_err(|e| anyhow!("transform error: {e}"))?;
                (lam, phi) = (p.0, p.1);
            }
            // start with a Molodensky transformation in the other direction (what GeoTools does), then refine it so
            // that this is the inverse of the CRS -> WGS84 direction
            DatumShift::Molodensky { a, f, t } => {
                let (target_lam, target_phi) = (lam, phi);
                (lam, phi) = molodensky(lam, phi, WGS84_A, WGS84_F, a - WGS84_A, f - WGS84_F, t.map(|v| -v));
                for _ in 0..5 {
                    let (l, p) = molodensky(lam, phi, a, f, WGS84_A - a, WGS84_F - f, t);
                    lam -= l - target_lam;
                    phi -= p - target_phi;
                }
            }
        }
        lam -= self.pm;
        match &self.projection {
            Projection::LatLong { unit } => Ok(Coord { x: lam / unit, y: phi / unit }),
            Projection::Proj4rs(proj) => {
                let mut p = (lam, phi, 0.0);
                proj4rs::transform::transform(&self.geodetic, proj, &mut p)
                    .map_err(|e| anyhow!("transform error: {e}"))?;
                Ok(Coord { x: p.0, y: p.1 })
            }
            Projection::Custom(proj) => {
                let (x, y) = proj.forward(lam, phi)?;
                Ok(Coord { x, y })
            }
        }
    }
}

/// True if a `+proj=longlat` definition is the same as WGS84 (within 1e-12 radians).
fn is_wgs84_equivalent(geodetic: &proj4rs::Proj) -> Result<bool> {
    let wgs84 = wgs84_proj()?;
    for (lam, phi) in [(0.3, 0.8), (-2.0, -0.5)] {
        let mut p = (lam, phi, 0.0);
        if proj4rs::transform::transform(geodetic, wgs84, &mut p).is_err()
            || (p.0 - lam).abs() > 1e-12
            || (p.1 - phi).abs() > 1e-12
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Semi-major axis and eccentricity of the ellipsoid in PROJ datum/ellipsoid parameters (computed through
/// `proj4rs` so that all of its ellipsoid and datum names work).
fn ellipsoid(datum_tokens: &str) -> Result<Ellipsoid> {
    let geodetic = proj4rs::Proj::from_proj_string(&format!("+proj=longlat{datum_tokens}"))
        .map_err(|e| anyhow!("invalid ellipsoid {datum_tokens}: {e}"))?;
    let geocent = proj4rs::Proj::from_proj_string(&format!("+proj=geocent{datum_tokens}"))
        .map_err(|e| anyhow!("invalid ellipsoid {datum_tokens}: {e}"))?;
    let mut equator = (0.0, 0.0, 0.0);
    let mut pole = (0.0, FRAC_PI_2, 0.0);
    proj4rs::transform::transform(&geodetic, &geocent, &mut equator).map_err(|e| anyhow!("{e}"))?;
    proj4rs::transform::transform(&geodetic, &geocent, &mut pole).map_err(|e| anyhow!("{e}"))?;
    let (a, b) = (equator.0, pole.2);
    if !(a > 0.0 && b > 0.0 && b <= a) {
        bail!("invalid ellipsoid {datum_tokens}");
    }
    Ok(Ellipsoid { a, es: 1.0 - (b * b) / (a * a) })
}

/// A `+pm=` value (degrees or a PROJ prime meridian name) in radians.
fn prime_meridian(pm: &str) -> Option<f64> {
    let degrees = match pm.to_ascii_lowercase().as_str() {
        "greenwich" => 0.0,
        "lisbon" => -9.131906111111,
        "paris" => 2.337229166667,
        "bogota" => -74.080916666667,
        "madrid" => -3.687938888889,
        "rome" => 12.452333333333,
        "bern" => 7.439583333333,
        "jakarta" => 106.807719444444,
        "ferro" => -17.666666666667,
        "brussels" => 4.367975,
        "stockholm" => 18.058277777778,
        "athens" => 23.7163375,
        "oslo" => 10.722916666667,
        "copenhagen" => 12.57788,
        other => other.parse().ok()?,
    };
    Some(degrees * DEGREE)
}

#[derive(Clone)]
enum Kind {
    /// WGS84-like geographic coordinates in degrees
    Geographic,
    /// spherical web mercator in meters
    WebMercator,
    /// anything else, from a PROJ string
    Proj(Arc<ProjCrs>),
}

/// A coordinate reference system with an axis order.
#[derive(Clone)]
pub struct Crs {
    kind: Kind,
    /// coordinates are (north, east) instead of (east, north)
    swap: bool,
    /// GeoTools `CRS.getAxisOrder`
    axis_order: AxisOrder,
    definition: String,
}

impl fmt::Debug for Crs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Crs({}, axis_order={:?})", self.definition, self.axis_order)
    }
}

impl PartialEq for Crs {
    fn eq(&self, other: &Self) -> bool {
        self.definition == other.definition && self.swap == other.swap && self.axis_order == other.axis_order
    }
}

/// Codes that are always spherical web mercator. GeoTools only knows `EPSG:3857` (`EPSG:3785` is a mercator on a
/// spherical datum there, which the EPSG table reproduces); the others are
/// accepted here too.
const WEB_MERCATOR_ALIASES: &[&str] =
    &["EPSG:3857", "EPSG:900913", "EPSG:102100", "EPSG:102113", "ESRI:102100", "ESRI:102113"];

impl Crs {
    /// WGS84 longitude/latitude (`EPSG:4326` with longitude first).
    pub fn wgs84() -> Crs {
        Crs { kind: Kind::Geographic, swap: false, axis_order: AxisOrder::EastNorth, definition: "EPSG:4326".into() }
    }

    /// Spherical web mercator (`EPSG:3857`).
    pub fn web_mercator() -> Crs {
        Crs { kind: Kind::WebMercator, swap: false, axis_order: AxisOrder::EastNorth, definition: "EPSG:3857".into() }
    }

    /// Build a CRS from a resolved definition.
    pub(crate) fn from_def(def: CrsDef, definition: String) -> Result<Crs> {
        let params = ProjParams::parse(&def.proj);
        let kind = if is_standard_web_mercator(&params) {
            Kind::WebMercator
        } else {
            let proj = ProjCrs::new(&def)?;
            if proj.is_wgs84() { Kind::Geographic } else { Kind::Proj(Arc::new(proj)) }
        };
        Ok(Crs { kind, swap: def.swap, axis_order: def.axis_order, definition })
    }

    /// This CRS with (north, east) coordinate order if `swap` is set.
    pub(crate) fn with_swap(mut self, swap: bool) -> Crs {
        self.swap = swap;
        self.axis_order = match (self.axis_order, swap) {
            (AxisOrder::Inapplicable, _) => AxisOrder::Inapplicable,
            (_, true) => AxisOrder::NorthEast,
            (_, false) => AxisOrder::EastNorth,
        };
        self
    }

    /// Equivalent of GeoTools `CRS.decode(code, longitudeFirst)`: an EPSG code (`EPSG:1234`,
    /// `urn:ogc:def:crs:EPSG::1234`, `http://www.opengis.net/def/crs/EPSG/0/1234`), `CRS:84`, or a PROJ string.
    /// EPSG codes use the EPSG axis order unless `longitude_first` is set.
    pub fn decode(code: &str, longitude_first: bool) -> Result<Crs> {
        let trimmed = code.trim();
        let upper = trimmed.to_ascii_uppercase();
        if WEB_MERCATOR_ALIASES.contains(&upper.as_str()) {
            return Ok(Crs { definition: upper, ..Crs::web_mercator() });
        }
        match upper.as_str() {
            "CRS:84" | "WGS84" | "OGC:CRS84" | "URN:OGC:DEF:CRS:OGC:1.3:CRS84" | "URN:OGC:DEF:CRS:OGC::CRS84" => {
                return Ok(Crs { definition: upper, ..Crs::wgs84() });
            }
            _ => {}
        }
        if trimmed.starts_with('+') {
            return Crs::from_proj_string(trimmed);
        }
        let Some(number) = epsg_code(trimmed) else { bail!("Unsupported CRS: {code}") };
        let definition = format!("EPSG:{number}");
        let def = match number {
            4326 => {
                let crs = Crs { definition, ..Crs::wgs84() };
                return Ok(if longitude_first { crs } else { crs.with_swap(true) });
            }
            3857 => return Ok(Crs { definition, ..Crs::web_mercator() }),
            _ => match epsg::lookup(number) {
                Some(epsg::Entry::Supported(def)) => def,
                Some(epsg::Entry::Unsupported(reason)) => bail!("{definition} is not supported: {reason}"),
                None => bail!(
                    "Unknown CRS {definition}: not a 2D geographic or projected CRS from the EPSG database ({} codes \
                     known)",
                    epsg::len()
                ),
            },
        };
        let crs = Crs::from_def(def, definition)?;
        Ok(if longitude_first && crs.axis_order == AxisOrder::NorthEast { crs.with_swap(false) } else { crs })
    }

    /// A CRS from a PROJ string (`+proj=...`), always east/north.
    pub fn from_proj_string(proj: &str) -> Result<Crs> {
        let def = CrsDef {
            proj: proj.trim().to_string(),
            axis_order: AxisOrder::EastNorth,
            swap: false,
            angular_unit: DEGREE,
        };
        let params = ProjParams::parse(&def.proj);
        if params.has("axis") && matches!(params.proj(), Some("longlat" | "latlong" | "lonlat" | "latlon")) {
            bail!("+axis is not supported for geographic PROJ strings: {proj}");
        }
        Crs::from_def(def, proj.trim().to_string()).with_context(|| format!("invalid proj string: {proj}"))
    }

    /// Equivalent of GeoTools `CRS.parseWKT`: WKT1 (OGC/ESRI/GDAL) or WKT2 geographic and projected CRS.
    pub fn parse_wkt(wkt: &str) -> Result<Crs> {
        let trimmed = wkt.trim();
        let lookup = |code: u32| match epsg::lookup(code) {
            Some(epsg::Entry::Supported(def)) => Some(def),
            _ => None,
        };
        let def = wkt::wkt_to_def(trimmed, &lookup).with_context(|| format!("Unsupported WKT CRS: {trimmed}"))?;
        Crs::from_def(def, trimmed.to_string()).with_context(|| format!("Unsupported WKT CRS: {trimmed}"))
    }

    /// True if the first axis is longitude/easting (GeoTools `CRS.getAxisOrder(crs) == EAST_NORTH`).
    pub fn is_longitude_first(&self) -> bool {
        self.axis_order == AxisOrder::EastNorth
    }

    /// GeoTools `CRS.getAxisOrder`.
    pub fn axis_order(&self) -> AxisOrder {
        self.axis_order
    }

    pub fn definition(&self) -> &str {
        &self.definition
    }

    /// The PROJ string this CRS is computed with, if it is not WGS84 or web mercator.
    pub fn proj_string(&self) -> Option<&str> {
        match &self.kind {
            Kind::Proj(p) => Some(&p.proj),
            _ => None,
        }
    }

    /// Convert from this CRS (in east/north order) to WGS84 lon/lat degrees.
    fn to_lon_lat(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        match &self.kind {
            Kind::Geographic => Ok(c),
            Kind::WebMercator => Ok(Coord {
                x: (c.x / EARTH_RADIUS).to_degrees(),
                y: (2.0 * (c.y / EARTH_RADIUS).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees(),
            }),
            Kind::Proj(proj) => proj.to_lon_lat(c),
        }
    }

    /// Convert WGS84 lon/lat degrees to this CRS (in east/north order).
    fn lon_lat_to_crs(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        match &self.kind {
            Kind::Geographic => Ok(c),
            Kind::WebMercator => Ok(Coord {
                x: EARTH_RADIUS * c.x.to_radians(),
                y: EARTH_RADIUS * (std::f64::consts::FRAC_PI_4 + c.y.to_radians() / 2.0).tan().ln(),
            }),
            Kind::Proj(proj) => proj.lon_lat_to_crs(c),
        }
    }

    fn same_kind(&self, other: &Crs) -> bool {
        match (&self.kind, &other.kind) {
            (Kind::Geographic, Kind::Geographic) | (Kind::WebMercator, Kind::WebMercator) => true,
            (Kind::Proj(a), Kind::Proj(b)) => a.proj == b.proj,
            _ => false,
        }
    }
}

/// `+proj=merc` on a sphere of radius 6378137 with no offsets and no datum shift.
fn is_standard_web_mercator(params: &ProjParams) -> bool {
    let num = |k: &str, default: f64| params.get(k).map_or(Some(default), |v| v.parse::<f64>().ok());
    (params.proj() == Some("merc") || params.proj() == Some("webmerc"))
        && (params.proj() == Some("webmerc")
            || (num("a", 0.0) == Some(EARTH_RADIUS)
                && (num("b", 0.0) == Some(EARTH_RADIUS) || num("R", 0.0) == Some(EARTH_RADIUS))
                && params.get("nadgrids") == Some("@null")))
        && num("lon_0", 0.0) == Some(0.0)
        && num("x_0", 0.0) == Some(0.0)
        && num("y_0", 0.0) == Some(0.0)
        && num("k", 1.0) == Some(1.0)
        && num("k_0", 1.0) == Some(1.0)
        && num("lat_ts", 0.0) == Some(0.0)
        && num("to_meter", 1.0) == Some(1.0)
        && !params.has("units")
        && !params.has("axis")
        && !params.has("pm")
}

/// The number of an EPSG code in one of the forms GeoTools accepts.
fn epsg_code(code: &str) -> Option<u32> {
    let lower = code.trim().to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("epsg:") {
        rest.trim_start_matches(':')
    } else if let Some(rest) =
        lower.strip_prefix("urn:ogc:def:crs:epsg:").or_else(|| lower.strip_prefix("urn:x-ogc:def:crs:epsg:"))
    {
        // optional version: urn:ogc:def:crs:EPSG:6.6:4326
        rest.rsplit(':').next()?
    } else if let Some(rest) = lower
        .strip_prefix("http://www.opengis.net/def/crs/epsg/")
        .or_else(|| lower.strip_prefix("https://www.opengis.net/def/crs/epsg/"))
    {
        rest.rsplit('/').next()?
    } else {
        lower.strip_prefix("http://www.opengis.net/gml/srs/epsg.xml#")?
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn wgs84_proj() -> Result<&'static proj4rs::Proj> {
    static WGS84: std::sync::OnceLock<Option<proj4rs::Proj>> = std::sync::OnceLock::new();
    WGS84
        .get_or_init(|| proj4rs::Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").ok())
        .as_ref()
        .ok_or_else(|| anyhow!("unable to create WGS84 projection"))
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
        !self.swap_input && self.source.same_kind(&self.dest) && self.source.swap == self.dest.swap
    }

    /// Transform a coordinate from the source to the destination CRS.
    pub fn transform(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        let mut c = if self.swap_input { Coord { x: c.y, y: c.x } } else { c };
        if self.source.swap {
            c = Coord { x: c.y, y: c.x };
        }
        if !self.source.same_kind(&self.dest) {
            c = self.dest.lon_lat_to_crs(self.source.to_lon_lat(c)?)?;
        }
        if self.dest.swap {
            c = Coord { x: c.y, y: c.x };
        }
        Ok(c)
    }

    /// Transform a coordinate from the destination back to the source CRS (GeoTools `MathTransform.inverse()`).
    pub fn inverse_transform(&self, c: Coord<f64>) -> Result<Coord<f64>> {
        let mut c = c;
        if self.dest.swap {
            c = Coord { x: c.y, y: c.x };
        }
        if !self.source.same_kind(&self.dest) {
            c = self.source.lon_lat_to_crs(self.dest.to_lon_lat(c)?)?;
        }
        if self.source.swap {
            c = Coord { x: c.y, y: c.x };
        }
        if self.swap_input {
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
/// set and does not match the axis order of `source` (`EAST_NORTH` or not), input coordinates are swapped first.
pub fn find_math_transform(source: &Crs, dest: &Crs, force_longitude_first: Option<bool>) -> MathTransform {
    let swap_input = matches!(force_longitude_first, Some(f) if f != source.is_longitude_first());
    MathTransform { swap_input, source: source.clone(), dest: dest.clone() }
}

/// Equivalent of `GeoUtils.decodeCRS(crs, base)`: decodes an EPSG code (optionally suffixed with `:lon_first` or
/// `:lat_first`), a PROJ string or WKT. Axis order defaults to the order of `base` if specified, otherwise the EPSG
/// axis order (latitude-first for geographic EPSG codes).
pub fn decode_crs(crs: &str, base: Option<&Crs>) -> Result<Crs> {
    let mut longitude_first = base.is_some_and(|b| b.is_longitude_first());
    let mut code = crs;
    if let Some(stripped) = crs.strip_suffix(":lon_first") {
        longitude_first = true;
        code = stripped;
    } else if let Some(stripped) = crs.strip_suffix(":lat_first") {
        longitude_first = false;
        code = stripped;
    }
    match Crs::decode(code, longitude_first) {
        Ok(crs) => Ok(crs),
        Err(e) if !crs.contains('[') => Err(e),
        Err(_) => Crs::parse_wkt(crs),
    }
}

#[cfg(test)]
mod tests;
