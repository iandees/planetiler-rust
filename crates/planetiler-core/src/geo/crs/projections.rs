//! Map projections that GeoTools supports but `proj4rs` does not: Cassini-Soldner (`cass`), Hotine oblique mercator
//! (`omerc`, variants A and B, also used for the Swiss oblique mercator), American polyconic (`poly`), sinusoidal
//! (`sinu`), Robinson (`robin`), equidistant conic (`eqdc`), gnomonic (`gnom`), orthographic (`ortho`), Equal Earth
//! (`eqearth`), New Zealand Map Grid (`nzmg`), azimuthal equidistant (`aeqd`, with GeographicLib geodesics), and
//! Lambert azimuthal equal area (`laea`, replacing `proj4rs`'
//! version which has bugs in its spherical oblique and ellipsoidal south polar forms).
//!
//! The formulas are ports of the corresponding PROJ (<https://proj.org>, MIT license) implementations, which are also
//! what GeoTools' implementations derive from. Each projection works on geodetic longitude/latitude in radians
//! (longitude relative to the prime meridian) and projected coordinates in meters, before false easting/northing,
//! units and axis directions, which [`CustomProjection`] applies.

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

use anyhow::{Result, anyhow, bail};

const EPS10: f64 = 1e-10;

/// Parsed `+key=value` / `+flag` PROJ string parameters.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjParams(Vec<(String, Option<String>)>);

impl ProjParams {
    pub(crate) fn parse(s: &str) -> ProjParams {
        ProjParams(
            s.split_whitespace()
                .filter_map(|t| {
                    let t = t.strip_prefix('+').unwrap_or(t);
                    if t.is_empty() {
                        return None;
                    }
                    Some(match t.split_once('=') {
                        Some((k, v)) => (k.to_string(), Some(v.to_string())),
                        None => (t.to_string(), None),
                    })
                })
                .collect(),
        )
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
    }

    pub(crate) fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    fn num(&self, key: &str) -> Result<Option<f64>> {
        self.get(key).map(|v| v.parse::<f64>().map_err(|_| anyhow!("invalid value for +{key}: {v}"))).transpose()
    }

    /// An angle in degrees (PROJ default) converted to radians; supports a trailing `r` for radians.
    fn angle(&self, key: &str) -> Result<Option<f64>> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => {
                let (v, scale) = match v.strip_suffix(['r', 'R']) {
                    Some(r) => (r, 1.0),
                    None => (v, PI / 180.0),
                };
                let n = v.parse::<f64>().map_err(|_| anyhow!("invalid angle for +{key}: {v}"))?;
                Ok(Some(n * scale))
            }
        }
    }

    pub(crate) fn proj(&self) -> Option<&str> {
        self.get("proj")
    }

    /// The parameters that define the ellipsoid and datum (to build a `+proj=longlat` definition on the same datum).
    pub(crate) fn datum_tokens(&self) -> String {
        const KEYS: &[&str] =
            &["ellps", "datum", "a", "b", "rf", "f", "es", "e", "R", "towgs84", "nadgrids", "R_A", "molodensky"];
        let mut out = String::new();
        for (k, v) in &self.0 {
            if KEYS.contains(&k.as_str()) {
                out.push_str(" +");
                out.push_str(k);
                if let Some(v) = v {
                    out.push('=');
                    out.push_str(v);
                }
            }
        }
        out
    }
}

/// Ellipsoid parameters used by the projection formulas.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ellipsoid {
    pub a: f64,
    pub es: f64,
}

impl Ellipsoid {
    fn e(&self) -> f64 {
        self.es.sqrt()
    }
    fn one_es(&self) -> f64 {
        1.0 - self.es
    }
}

/// A projection implemented here (not in `proj4rs`).
trait Projection: Send + Sync {
    /// geodetic (lambda relative to the projection's central meridian, phi) in radians -> (x, y) in meters
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)>;
    /// (x, y) in meters -> (lambda relative to the central meridian, phi) in radians
    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)>;
    /// the central meridian (some projections compute it from other parameters)
    fn lam0(&self) -> f64;
}

/// Names of the projections implemented here.
pub(crate) const CUSTOM_PROJECTIONS: &[&str] =
    &["cass", "omerc", "poly", "sinu", "robin", "eqdc", "gnom", "ortho", "eqearth", "nzmg", "laea", "aeqd"];

/// A projection from [`CUSTOM_PROJECTIONS`] with false easting/northing, units and axis directions applied.
pub(crate) struct CustomProjection {
    projection: Box<dyn Projection>,
    x0: f64,
    y0: f64,
    to_meter: f64,
    /// sign of the x and y axes (`+axis=wsu` -> (-1, -1))
    x_sign: f64,
    y_sign: f64,
    /// `+axis=neu`-style swaps
    swap_axes: bool,
}

impl CustomProjection {
    pub(crate) fn new(params: &ProjParams, ellps: Ellipsoid) -> Result<CustomProjection> {
        let name = params.proj().unwrap_or_default();
        let lat0 = params.angle("lat_0")?.unwrap_or(0.0);
        let lon0 = params.angle("lon_0")?.unwrap_or(0.0);
        let k0 = params.num("k_0")?.or(params.num("k")?).unwrap_or(1.0);
        let projection: Box<dyn Projection> = match name {
            "cass" => Box::new(Cassini::new(ellps, lat0, lon0)),
            "omerc" => Box::new(ObliqueMercator::new(ellps, params, lat0, k0)?),
            "poly" => Box::new(Polyconic::new(ellps, lat0, lon0)),
            "sinu" => Box::new(Sinusoidal::new(ellps, lon0)),
            "robin" => Box::new(Robinson { a: ellps.a, lam0: lon0 }),
            "eqdc" => Box::new(EquidistantConic::new(
                ellps,
                lat0,
                lon0,
                params.angle("lat_1")?.unwrap_or(0.0),
                params.angle("lat_2")?,
            )?),
            "gnom" => Box::new(Azimuthal::new(ellps.a, lat0, lon0, true)),
            "ortho" => Box::new(Azimuthal::new(ellps.a, lat0, lon0, false)),
            // +spherical (not a PROJ parameter): the spherical formulas with R = a, like GeoTools
            "eqearth" if params.has("spherical") => Box::new(EqualEarth::new(Ellipsoid { a: ellps.a, es: 0.0 }, lon0)),
            "eqearth" => Box::new(EqualEarth::new(ellps, lon0)),
            "nzmg" => Box::new(NewZealandMapGrid { a: ellps.a }),
            "laea" => Box::new(LambertAzimuthalEqualArea::new(ellps, lat0, lon0)),
            "aeqd" => Box::new(AzimuthalEquidistant::new(ellps, lat0, lon0)),
            other => bail!("unsupported projection +proj={other}"),
        };
        let (x0, y0) = if name == "nzmg" {
            (params.num("x_0")?.unwrap_or(2510000.0), params.num("y_0")?.unwrap_or(6023150.0))
        } else {
            (params.num("x_0")?.unwrap_or(0.0), params.num("y_0")?.unwrap_or(0.0))
        };
        let to_meter = match (params.num("to_meter")?, params.get("units")) {
            (Some(m), _) => m,
            (None, Some(units)) => unit_to_meter(units).ok_or_else(|| anyhow!("unsupported +units={units}"))?,
            _ => 1.0,
        };
        let axis = params.get("axis").unwrap_or("enu");
        let axis = axis.as_bytes();
        if axis.len() < 2 {
            bail!("invalid +axis");
        }
        let sign = |c: u8| if c == b'w' || c == b's' { -1.0 } else { 1.0 };
        let swap_axes = matches!(axis[0], b'n' | b's');
        let (x_sign, y_sign) = if swap_axes { (sign(axis[1]), sign(axis[0])) } else { (sign(axis[0]), sign(axis[1])) };
        Ok(CustomProjection { projection, x0, y0, to_meter, x_sign, y_sign, swap_axes })
    }

    /// projected coordinates -> geodetic (lon relative to the prime meridian, lat) in radians
    pub(crate) fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (x, y) = if self.swap_axes { (y, x) } else { (x, y) };
        let x = x * self.x_sign * self.to_meter - self.x0;
        let y = y * self.y_sign * self.to_meter - self.y0;
        let (lam, phi) = self.projection.inverse(x, y)?;
        if !lam.is_finite() || !phi.is_finite() {
            bail!("point outside of projection domain");
        }
        Ok((adjlon(lam + self.projection.lam0()), phi))
    }

    /// geodetic (lon relative to the prime meridian, lat) in radians -> projected coordinates
    pub(crate) fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        if phi.abs() > FRAC_PI_2 + 1e-12 {
            bail!("latitude out of range");
        }
        let (x, y) = self.projection.forward(adjlon(lam - self.projection.lam0()), phi)?;
        if !x.is_finite() || !y.is_finite() {
            bail!("point outside of projection domain");
        }
        let x = (x + self.x0) / self.to_meter * self.x_sign;
        let y = (y + self.y0) / self.to_meter * self.y_sign;
        Ok(if self.swap_axes { (y, x) } else { (x, y) })
    }
}

/// PROJ `+units=` names.
pub(crate) fn unit_to_meter(units: &str) -> Option<f64> {
    Some(match units {
        "m" => 1.0,
        "km" => 1000.0,
        "dm" => 0.1,
        "cm" => 0.01,
        "mm" => 0.001,
        "kmi" => 1852.0,
        "in" => 0.0254,
        "ft" => 0.3048,
        "yd" => 0.9144,
        "mi" => 1609.344,
        "fath" => 1.8288,
        "ch" => 20.1168,
        "link" => 0.201168,
        "us-in" => 1.0 / 39.37,
        "us-ft" => 1200.0 / 3937.0,
        "us-yd" => 3600.0 / 3937.0,
        "us-ch" => 79200.0 / 3937.0,
        "us-mi" => 6336000.0 / 3937.0,
        "ind-yd" => 0.91439523,
        "ind-ft" => 0.30479841,
        "ind-ch" => 20.11669506,
        _ => return None,
    })
}

/// Normalize a longitude in radians to [-pi, pi].
fn adjlon(mut lon: f64) -> f64 {
    if lon.abs() <= PI + 1e-12 {
        return lon;
    }
    lon += PI;
    lon -= 2.0 * PI * (lon / (2.0 * PI)).floor();
    lon - PI
}

// ---------------------------------------------------------------------------------------------------------------
// shared helpers (PROJ pj_enfn / pj_mlfn / pj_inv_mlfn / pj_tsfn / pj_phi2 / pj_msfn / pj_qsfn)
// ---------------------------------------------------------------------------------------------------------------

/// Coefficients for the meridian distance.
#[derive(Debug, Clone, Copy)]
struct Meridian {
    en: [f64; 5],
    es: f64,
}

impl Meridian {
    fn new(es: f64) -> Meridian {
        const C00: f64 = 1.0;
        const C02: f64 = 0.25;
        const C04: f64 = 0.046875;
        const C06: f64 = 0.01953125;
        const C08: f64 = 0.01068115234375;
        const C22: f64 = 0.75;
        const C44: f64 = 0.46875;
        const C46: f64 = 0.013_020_833_333_333_334;
        const C48: f64 = 0.007_120_768_229_166_667;
        const C66: f64 = 0.364_583_333_333_333_3;
        const C68: f64 = 0.005_696_614_583_333_333;
        const C88: f64 = 0.3076171875;
        let mut en = [0.0; 5];
        en[0] = C00 - es * (C02 + es * (C04 + es * (C06 + es * C08)));
        en[1] = es * (C22 - es * (C04 + es * (C06 + es * C08)));
        let mut t = es * es;
        en[2] = t * (C44 - es * (C46 + es * C48));
        t *= es;
        en[3] = t * (C66 - es * C68);
        en[4] = t * es * C88;
        Meridian { en, es }
    }

    /// meridian distance from the equator to `phi` on an ellipsoid with a = 1
    fn mlfn(&self, phi: f64, sphi: f64, cphi: f64) -> f64 {
        let en = &self.en;
        let cphi = cphi * sphi;
        let sphi = sphi * sphi;
        en[0] * phi - cphi * (en[1] + sphi * (en[2] + sphi * (en[3] + sphi * en[4])))
    }

    fn ml(&self, phi: f64) -> f64 {
        self.mlfn(phi, phi.sin(), phi.cos())
    }

    /// inverse of [`Self::mlfn`]
    fn inv_mlfn(&self, arg: f64) -> Result<f64> {
        let k = 1.0 / (1.0 - self.es);
        let mut phi = arg;
        for _ in 0..20 {
            let s = phi.sin();
            let mut t = 1.0 - self.es * s * s;
            t = (self.mlfn(phi, s, phi.cos()) - arg) * (t * t.sqrt()) * k;
            phi -= t;
            if t.abs() < 1e-11 {
                return Ok(phi);
            }
        }
        bail!("meridian distance inversion did not converge")
    }
}

/// `asin` clamped to [-1, 1] (PROJ `aasin`)
fn aasin(v: f64) -> f64 {
    v.clamp(-1.0, 1.0).asin()
}

fn tsfn(phi: f64, sinphi: f64, e: f64) -> f64 {
    let sinphi = sinphi * e;
    (0.5 * (FRAC_PI_2 - phi)).tan() / ((1.0 - sinphi) / (1.0 + sinphi)).powf(0.5 * e)
}

fn phi2(ts: f64, e: f64) -> Result<f64> {
    let eccnth = 0.5 * e;
    let mut phi = FRAC_PI_2 - 2.0 * ts.atan();
    for _ in 0..30 {
        let con = e * phi.sin();
        let dphi = FRAC_PI_2 - 2.0 * (ts * ((1.0 - con) / (1.0 + con)).powf(eccnth)).atan() - phi;
        phi += dphi;
        if dphi.abs() <= 1e-12 {
            return Ok(phi);
        }
    }
    bail!("latitude iteration did not converge")
}

fn msfn(sinphi: f64, cosphi: f64, es: f64) -> f64 {
    cosphi / (1.0 - es * sinphi * sinphi).sqrt()
}

fn qsfn(sinphi: f64, e: f64, one_es: f64) -> f64 {
    if e >= 1e-7 {
        let con = e * sinphi;
        let div1 = 1.0 - con * con;
        let div2 = 1.0 + con;
        one_es * (sinphi / div1 - (0.5 / e) * ((1.0 - con) / div2).ln())
    } else {
        sinphi + sinphi
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Cassini-Soldner (PROJ cass.cpp)
// ---------------------------------------------------------------------------------------------------------------

struct Cassini {
    a: f64,
    es: f64,
    lam0: f64,
    phi0: f64,
    m0: f64,
    ml: Meridian,
}

impl Cassini {
    fn new(ellps: Ellipsoid, phi0: f64, lam0: f64) -> Cassini {
        let ml = Meridian::new(ellps.es);
        Cassini { a: ellps.a, es: ellps.es, lam0, phi0, m0: ml.ml(phi0), ml }
    }
}

impl Projection for Cassini {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        const C1: f64 = 1.0 / 6.0;
        const C2: f64 = 1.0 / 120.0;
        const C3: f64 = 1.0 / 24.0;
        if self.es == 0.0 {
            let x = (phi.cos() * lam.sin()).asin();
            let y = phi.tan().atan2(lam.cos()) - self.phi0;
            return Ok((x * self.a, y * self.a));
        }
        let sinphi = phi.sin();
        let cosphi = phi.cos();
        let mut y = self.ml.mlfn(phi, sinphi, cosphi);
        let n = 1.0 / (1.0 - self.es * sinphi * sinphi).sqrt();
        let tn = phi.tan();
        let t = tn * tn;
        let a1 = lam * cosphi;
        let c = self.es * cosphi * cosphi / (1.0 - self.es);
        let a2 = a1 * a1;
        let x = n * a1 * (1.0 - a2 * t * (C1 - (8.0 - t + 8.0 * c) * a2 * C2));
        y -= self.m0 - n * tn * a2 * (0.5 + (5.0 - t + 6.0 * c) * a2 * C3);
        Ok((x * self.a, y * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        const C3: f64 = 1.0 / 24.0;
        const C4: f64 = 1.0 / 3.0;
        const C5: f64 = 1.0 / 15.0;
        let (x, y) = (x / self.a, y / self.a);
        if self.es == 0.0 {
            let dd = y + self.phi0;
            let phi = (dd.sin() * x.cos()).asin();
            let lam = x.tan().atan2(dd.cos());
            return Ok((lam, phi));
        }
        let ph1 = self.ml.inv_mlfn(self.m0 + y)?;
        let tn = ph1.tan();
        let t = tn * tn;
        let n = ph1.sin();
        let mut r = 1.0 / (1.0 - self.es * n * n);
        let n = r.sqrt();
        r *= (1.0 - self.es) * n;
        let dd = x / n;
        let d2 = dd * dd;
        let phi = ph1 - (n * tn / r) * d2 * (0.5 - (1.0 + 3.0 * t) * d2 * C3);
        let lam = dd * (1.0 + t * d2 * (-C4 + (1.0 + 3.0 * t) * d2 * C5)) / ph1.cos();
        Ok((lam, phi))
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Hotine oblique mercator (PROJ omerc.cpp)
// ---------------------------------------------------------------------------------------------------------------

struct ObliqueMercator {
    a: f64,
    e: f64,
    lam0: f64,
    big_a: f64,
    big_b: f64,
    big_e: f64,
    arb: f64,
    bra: f64,
    v_pole_n: f64,
    v_pole_s: f64,
    u_0: f64,
    singam: f64,
    cosgam: f64,
    sinrot: f64,
    cosrot: f64,
    no_rot: bool,
}

impl ObliqueMercator {
    fn new(ellps: Ellipsoid, params: &ProjParams, phi0: f64, k0: f64) -> Result<ObliqueMercator> {
        let es = ellps.es;
        let e = ellps.e();
        let one_es = ellps.one_es();
        let alpha = params.angle("alpha")?;
        let gamma = params.angle("gamma")?;
        let lamc = params.angle("lonc")?.or(params.angle("lon_0")?).unwrap_or(0.0);
        if alpha.is_none() && gamma.is_none() {
            bail!("+proj=omerc requires +alpha or +gamma (two point form is not supported)");
        }
        let no_off = params.has("no_uoff") || params.has("no_off");
        let no_rot = params.has("no_rot");
        let com = one_es.sqrt();
        let (big_a, big_b, big_e, d, mut f);
        if phi0.abs() > EPS10 {
            let sinph0 = phi0.sin();
            let cosph0 = phi0.cos();
            let con = 1.0 - es * sinph0 * sinph0;
            let b = cosph0 * cosph0;
            big_b = (1.0 + es * b * b / one_es).sqrt();
            big_a = big_b * k0 * com / con;
            d = big_b * com / (cosph0 * con.sqrt());
            f = d * d - 1.0;
            if f <= 0.0 {
                f = 0.0;
            } else {
                f = f.sqrt();
                if phi0 < 0.0 {
                    f = -f;
                }
            }
            f += d;
            big_e = f * tsfn(phi0, sinph0, e).powf(big_b);
        } else {
            big_b = 1.0 / com;
            big_a = k0;
            big_e = 1.0;
            d = 1.0;
            f = 1.0;
        }
        let (alpha_c, gamma0, gamma) = match alpha {
            Some(alpha_c) => {
                let gamma0 = aasin(alpha_c.sin() / d);
                (alpha_c, gamma0, gamma.unwrap_or(alpha_c))
            }
            None => {
                let gamma = gamma.unwrap_or(0.0);
                (aasin(d * gamma.sin()), gamma, gamma)
            }
        };
        let lam0 = lamc - aasin(0.5 * (f - 1.0 / f) * gamma0.tan()) / big_b;
        let arb = big_a / big_b;
        let bra = 1.0 / arb;
        let u_0 = if no_off {
            0.0
        } else {
            let u = (arb * ((d * d - 1.0).max(0.0).sqrt() / alpha_c.cos()).atan()).abs();
            if phi0 < 0.0 { -u } else { u }
        };
        let half = 0.5 * gamma0;
        Ok(ObliqueMercator {
            a: ellps.a,
            e,
            lam0,
            big_a,
            big_b,
            big_e,
            arb,
            bra,
            v_pole_n: arb * (FRAC_PI_4 - half).tan().ln(),
            v_pole_s: arb * (FRAC_PI_4 + half).tan().ln(),
            u_0,
            singam: gamma0.sin(),
            cosgam: gamma0.cos(),
            sinrot: gamma.sin(),
            cosrot: gamma.cos(),
            no_rot,
        })
    }
}

impl Projection for ObliqueMercator {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let (u, v);
        if (phi.abs() - FRAC_PI_2).abs() > EPS10 {
            let w = self.big_e / tsfn(phi, phi.sin(), self.e).powf(self.big_b);
            let temp = 1.0 / w;
            let s = 0.5 * (w - temp);
            let t = 0.5 * (w + temp);
            let vv = (self.big_b * lam).sin();
            let uu = (s * self.singam - vv * self.cosgam) / t;
            if (uu.abs() - 1.0).abs() < EPS10 {
                bail!("point outside of projection domain");
            }
            v = 0.5 * self.arb * ((1.0 - uu) / (1.0 + uu)).ln();
            let temp = (self.big_b * lam).cos();
            u = if temp.abs() < 1e-7 {
                self.big_a * lam
            } else {
                self.arb * (s * self.cosgam + vv * self.singam).atan2(temp)
            };
        } else {
            v = if phi > 0.0 { self.v_pole_n } else { self.v_pole_s };
            u = self.arb * phi;
        }
        let (x, y) = if self.no_rot {
            (u, v)
        } else {
            let u = u - self.u_0;
            (v * self.cosrot + u * self.sinrot, u * self.cosrot - v * self.sinrot)
        };
        Ok((x * self.a, y * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (x, y) = (x / self.a, y / self.a);
        let (u, v) = if self.no_rot {
            (x, y)
        } else {
            (y * self.cosrot + x * self.sinrot + self.u_0, x * self.cosrot - y * self.sinrot)
        };
        let qp = (-self.bra * v).exp();
        if qp == 0.0 {
            bail!("point outside of projection domain");
        }
        let temp = 1.0 / qp;
        let sp = 0.5 * (qp - temp);
        let tp = 0.5 * (qp + temp);
        let vp = (self.bra * u).sin();
        let up = (vp * self.cosgam + sp * self.singam) / tp;
        if (up.abs() - 1.0).abs() < EPS10 {
            return Ok((0.0, if up < 0.0 { -FRAC_PI_2 } else { FRAC_PI_2 }));
        }
        let phi = self.big_e / ((1.0 + up) / (1.0 - up)).sqrt();
        let phi = phi2(phi.powf(1.0 / self.big_b), self.e)?;
        let lam = -(sp * self.cosgam - vp * self.singam).atan2((self.bra * u).cos()) / self.big_b;
        Ok((lam, phi))
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// American polyconic (PROJ poly.cpp)
// ---------------------------------------------------------------------------------------------------------------

struct Polyconic {
    a: f64,
    es: f64,
    lam0: f64,
    ml0: f64,
    ml: Meridian,
}

impl Polyconic {
    fn new(ellps: Ellipsoid, phi0: f64, lam0: f64) -> Polyconic {
        let ml = Meridian::new(ellps.es);
        Polyconic { a: ellps.a, es: ellps.es, lam0, ml0: ml.ml(phi0), ml }
    }
}

impl Projection for Polyconic {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        const TOL: f64 = 1e-10;
        let (x, y) = if phi.abs() <= TOL {
            (lam, -self.ml0)
        } else {
            let sp = phi.sin();
            let cp = phi.cos();
            let ms = if cp.abs() > TOL { msfn(sp, cp, self.es) / sp } else { 0.0 };
            let lam = lam * sp;
            (ms * lam.sin(), (self.ml.mlfn(phi, sp, cp) - self.ml0) + ms * (1.0 - lam.cos()))
        };
        Ok((x * self.a, y * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        const TOL: f64 = 1e-10;
        const ITOL: f64 = 1e-12;
        let (x, y) = (x / self.a, y / self.a + self.ml0);
        if y.abs() <= TOL {
            return Ok((x, 0.0));
        }
        let r = y * y + x * x;
        let mut phi = y;
        let mut converged = false;
        for _ in 0..20 {
            let sp = phi.sin();
            let cp = phi.cos();
            if cp.abs() < ITOL {
                bail!("point outside of projection domain");
            }
            let s2ph = sp * cp;
            let mut mlp = (1.0 - self.es * sp * sp).sqrt();
            let c = sp * mlp / cp;
            let ml = self.ml.mlfn(phi, sp, cp);
            let mlb = ml * ml + r;
            mlp = (1.0 - self.es) / (mlp * mlp * mlp);
            let dphi = (ml + ml + c * mlb - 2.0 * y * (c * ml + 1.0))
                / (self.es * s2ph * (mlb - 2.0 * y * ml) / c + 2.0 * (y - ml) * (c * mlp - 1.0 / s2ph) - mlp - mlp);
            phi += dphi;
            if dphi.abs() <= ITOL {
                converged = true;
                break;
            }
        }
        if !converged {
            bail!("polyconic inverse did not converge");
        }
        let c = phi.sin();
        let lam = (x * phi.tan() * (1.0 - self.es * c * c).sqrt()).asin() / c;
        Ok((lam, phi))
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Sinusoidal (PROJ gn_sinu.cpp, ellipsoidal form)
// ---------------------------------------------------------------------------------------------------------------

struct Sinusoidal {
    a: f64,
    es: f64,
    lam0: f64,
    ml: Meridian,
}

impl Sinusoidal {
    fn new(ellps: Ellipsoid, lam0: f64) -> Sinusoidal {
        Sinusoidal { a: ellps.a, es: ellps.es, lam0, ml: Meridian::new(ellps.es) }
    }
}

impl Projection for Sinusoidal {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let s = phi.sin();
        let c = phi.cos();
        let y = self.ml.mlfn(phi, s, c);
        let x = lam * c / (1.0 - self.es * s * s).sqrt();
        Ok((x * self.a, y * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (x, y) = (x / self.a, y / self.a);
        let phi = self.ml.inv_mlfn(y)?;
        let s = phi.abs();
        if s < FRAC_PI_2 {
            let s = phi.sin();
            Ok((x * (1.0 - self.es * s * s).sqrt() / phi.cos(), phi))
        } else if s - EPS10 < FRAC_PI_2 {
            Ok((0.0, phi))
        } else {
            bail!("point outside of projection domain")
        }
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Robinson (PROJ robin.cpp, spherical)
// ---------------------------------------------------------------------------------------------------------------

struct Robinson {
    a: f64,
    lam0: f64,
}

// Coefficients of PROJ >= 4.9 (continuous at the nodes), which GeoTools uses as well.
#[rustfmt::skip]
const ROBIN_X: [[f64; 4]; 19] = [
    [1.0, -5.67239e-12, -7.15511e-05, 3.11028e-06],
    [0.9986, -0.000482241, -2.4897e-05, -1.33094e-06],
    [0.9954, -0.000831031, -4.4861e-05, -9.86588e-07],
    [0.99, -0.00135363, -5.96598e-05, 3.67749e-06],
    [0.9822, -0.00167442, -4.4975e-06, -5.72394e-06],
    [0.973, -0.00214869, -9.03565e-05, 1.88767e-08],
    [0.96, -0.00305084, -9.00732e-05, 1.64869e-06],
    [0.9427, -0.00382792, -6.53428e-05, -2.61493e-06],
    [0.9216, -0.00467747, -0.000104566, 4.8122e-06],
    [0.8962, -0.00536222, -3.23834e-05, -5.43445e-06],
    [0.8679, -0.00609364, -0.0001139, 3.32521e-06],
    [0.835, -0.00698325, -6.40219e-05, 9.34582e-07],
    [0.7986, -0.00755337, -5.00038e-05, 9.35532e-07],
    [0.7597, -0.00798325, -3.59716e-05, -2.27604e-06],
    [0.7186, -0.00851366, -7.0112e-05, -8.63072e-06],
    [0.6732, -0.00986209, -0.000199572, 1.91978e-05],
    [0.6213, -0.010418, 8.83948e-05, 6.24031e-06],
    [0.5722, -0.00906601, 0.000181999, 6.24033e-06],
    [0.5322, 0.0, 0.0, 0.0],
];

#[rustfmt::skip]
const ROBIN_Y: [[f64; 4]; 19] = [
    [0.0, 0.0124, 3.72529e-10, 1.15484e-09],
    [0.062, 0.0124001, 1.76951e-08, -5.92321e-09],
    [0.124, 0.0123998, -7.09668e-08, 2.25753e-08],
    [0.186, 0.0124008, 2.66917e-07, -8.44523e-08],
    [0.248, 0.0123971, -9.99682e-07, 3.15569e-07],
    [0.31, 0.0124108, 3.73349e-06, -1.1779e-06],
    [0.372, 0.0123598, -1.3935e-05, 4.39588e-06],
    [0.434, 0.0125501, 5.20034e-05, -1.00051e-05],
    [0.4968, 0.0123198, -9.80735e-05, 9.22397e-06],
    [0.5571, 0.0120308, 4.02857e-05, -5.2901e-06],
    [0.6176, 0.0120369, -3.90662e-05, 7.36117e-07],
    [0.6769, 0.0117015, -2.80246e-05, -8.54283e-07],
    [0.7346, 0.0113572, -4.08389e-05, -5.18524e-07],
    [0.7903, 0.0109099, -4.86169e-05, -1.0718e-06],
    [0.8435, 0.0103433, -6.46934e-05, 5.36384e-09],
    [0.8936, 0.00969679, -6.46129e-05, -8.54894e-06],
    [0.9394, 0.00840949, -0.000192847, -4.21023e-06],
    [0.9761, 0.00616525, -0.000256001, -4.21021e-06],
    [1.0, 0.0, 0.0, 0.0],
];

impl Robinson {
    const FXC: f64 = 0.8487;
    const FYC: f64 = 1.3523;
    const C1: f64 = 11.459_155_902_616_464;
    const RC1: f64 = 0.087_266_462_599_716_48;
    const NODES: usize = 18;

    fn v(c: &[f64; 4], z: f64) -> f64 {
        c[0] + z * (c[1] + z * (c[2] + z * c[3]))
    }
    fn dv(c: &[f64; 4], z: f64) -> f64 {
        c[1] + z * (c[2] + c[2] + z * 3.0 * c[3])
    }
}

impl Projection for Robinson {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let dphi = phi.abs();
        let i = ((dphi * Self::C1 + 1e-15).floor() as usize).min(Self::NODES - 1);
        let dphi = (dphi - Self::RC1 * i as f64).to_degrees();
        let x = Self::v(&ROBIN_X[i], dphi) * Self::FXC * lam;
        let mut y = Self::v(&ROBIN_Y[i], dphi) * Self::FYC;
        if phi < 0.0 {
            y = -y;
        }
        Ok((x * self.a, y * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (x, y) = (x / self.a, y / self.a);
        let mut lam = x / Self::FXC;
        let mut phi = (y / Self::FYC).abs();
        if phi >= 1.0 {
            if phi > 1.000001 {
                bail!("point outside of projection domain");
            }
            phi = if y < 0.0 { -FRAC_PI_2 } else { FRAC_PI_2 };
            lam /= ROBIN_X[Self::NODES][0];
            return Ok((lam, phi));
        }
        let mut i = ((phi * Self::NODES as f64).floor() as usize).min(Self::NODES - 1);
        loop {
            if ROBIN_Y[i][0] > phi {
                i -= 1;
            } else if ROBIN_Y[i + 1][0] <= phi {
                i += 1;
            } else {
                break;
            }
        }
        let t_row = &ROBIN_Y[i];
        // first guess, linear interpolation, then Newton-Raphson on V(Y[i], t) = phi
        let mut t = 5.0 * (phi - t_row[0]) / (ROBIN_Y[i + 1][0] - t_row[0]);
        for _ in 0..100 {
            let t1 = (Self::v(t_row, t) - phi) / Self::dv(t_row, t);
            t -= t1;
            if t1.abs() < 1e-12 {
                break;
            }
        }
        // the polynomials of adjacent rows do not quite meet at the nodes: map values in between to the node
        let mut t = t.max(0.0);
        if t >= 5.0 {
            i += 1;
            t = 0.0;
        }
        let mut phi = (5.0 * i as f64 + t).to_radians();
        if y < 0.0 {
            phi = -phi;
        }
        lam /= Self::v(&ROBIN_X[i], t);
        Ok((lam, phi))
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Equidistant conic (PROJ eqdc.cpp)
// ---------------------------------------------------------------------------------------------------------------

struct EquidistantConic {
    a: f64,
    es: f64,
    lam0: f64,
    n: f64,
    c: f64,
    rho0: f64,
    ml: Meridian,
}

impl EquidistantConic {
    fn new(ellps: Ellipsoid, phi0: f64, lam0: f64, phi1: f64, phi2: Option<f64>) -> Result<EquidistantConic> {
        let phi2 = phi2.unwrap_or(phi1);
        if (phi1 + phi2).abs() < EPS10 {
            bail!("+proj=eqdc: lat_1 = -lat_2");
        }
        let es = ellps.es;
        let ml = Meridian::new(es);
        let sinphi = phi1.sin();
        let cosphi = phi1.cos();
        let secant = (phi1 - phi2).abs() >= EPS10;
        let mut n = sinphi;
        let (c, rho0);
        if es > 0.0 {
            let m1 = msfn(sinphi, cosphi, es);
            let ml1 = ml.mlfn(phi1, sinphi, cosphi);
            if secant {
                let sinphi = phi2.sin();
                let cosphi = phi2.cos();
                let ms2 = msfn(sinphi, cosphi, es);
                let ml2 = ml.mlfn(phi2, sinphi, cosphi);
                n = (m1 - ms2) / (ml2 - ml1);
            }
            c = ml1 + m1 / n;
            rho0 = c - ml.ml(phi0);
        } else {
            if secant {
                n = (cosphi - phi2.cos()) / (phi2 - phi1);
            }
            c = phi1 + phi1.cos() / n;
            rho0 = c - phi0;
        }
        Ok(EquidistantConic { a: ellps.a, es, lam0, n, c, rho0, ml })
    }
}

impl Projection for EquidistantConic {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let rho = self.c - if self.es > 0.0 { self.ml.ml(phi) } else { phi };
        let lam = lam * self.n;
        Ok((rho * lam.sin() * self.a, (self.rho0 - rho * lam.cos()) * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (mut x, mut y) = (x / self.a, self.rho0 - y / self.a);
        let mut rho = x.hypot(y);
        if rho != 0.0 {
            if self.n < 0.0 {
                rho = -rho;
                x = -x;
                y = -y;
            }
            let mut phi = self.c - rho;
            if self.es > 0.0 {
                phi = self.ml.inv_mlfn(phi)?;
            }
            Ok((x.atan2(y) / self.n, phi))
        } else {
            Ok((0.0, if self.n > 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 }))
        }
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Gnomonic and orthographic (PROJ gnom.cpp / ortho.cpp, spherical forms like GeoTools)
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    NorthPole,
    SouthPole,
    Equatorial,
    Oblique,
}

struct Azimuthal {
    a: f64,
    lam0: f64,
    phi0: f64,
    sinph0: f64,
    cosph0: f64,
    mode: Mode,
    gnomonic: bool,
}

impl Azimuthal {
    fn new(a: f64, phi0: f64, lam0: f64, gnomonic: bool) -> Azimuthal {
        let mode = if (phi0.abs() - FRAC_PI_2).abs() < EPS10 {
            if phi0 < 0.0 { Mode::SouthPole } else { Mode::NorthPole }
        } else if phi0.abs() < EPS10 {
            Mode::Equatorial
        } else {
            Mode::Oblique
        };
        Azimuthal { a, lam0, phi0, sinph0: phi0.sin(), cosph0: phi0.cos(), mode, gnomonic }
    }
}

impl Projection for Azimuthal {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let sinphi = phi.sin();
        let cosphi = phi.cos();
        let mut coslam = lam.cos();
        let (x, y);
        if self.gnomonic {
            let cosz = match self.mode {
                Mode::Equatorial => cosphi * coslam,
                Mode::Oblique => self.sinph0 * sinphi + self.cosph0 * cosphi * coslam,
                Mode::SouthPole => -sinphi,
                Mode::NorthPole => sinphi,
            };
            if cosz <= EPS10 {
                bail!("point outside of projection domain");
            }
            let r = 1.0 / cosz;
            x = r * cosphi * lam.sin();
            y = r * match self.mode {
                Mode::Equatorial => sinphi,
                Mode::Oblique => self.cosph0 * sinphi - self.sinph0 * cosphi * coslam,
                Mode::NorthPole => -cosphi * coslam,
                Mode::SouthPole => cosphi * coslam,
            };
        } else {
            y = match self.mode {
                Mode::Equatorial => {
                    if cosphi * coslam < -EPS10 {
                        bail!("point outside of projection domain");
                    }
                    sinphi
                }
                Mode::Oblique => {
                    if self.sinph0 * sinphi + self.cosph0 * cosphi * coslam < -EPS10 {
                        bail!("point outside of projection domain");
                    }
                    self.cosph0 * sinphi - self.sinph0 * cosphi * coslam
                }
                Mode::NorthPole | Mode::SouthPole => {
                    if self.mode == Mode::NorthPole {
                        coslam = -coslam;
                    }
                    if (phi - self.phi0).abs() - EPS10 > FRAC_PI_2 {
                        bail!("point outside of projection domain");
                    }
                    cosphi * coslam
                }
            };
            x = cosphi * lam.sin();
        }
        Ok((x * self.a, y * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (mut x, mut y) = (x / self.a, y / self.a);
        let rh = x.hypot(y);
        if rh <= EPS10 {
            return Ok((0.0, self.phi0));
        }
        let (sinz, cosz) = if self.gnomonic {
            let z = rh.atan();
            (z.sin(), z.cos())
        } else {
            if rh > 1.0 + EPS10 {
                bail!("point outside of projection domain");
            }
            let s = rh.min(1.0);
            (s, (1.0 - s * s).sqrt())
        };
        let phi;
        match self.mode {
            Mode::Oblique => {
                let p = cosz * self.sinph0 + y * sinz * self.cosph0 / rh;
                phi = if p.abs() >= 1.0 { FRAC_PI_2.copysign(p) } else { p.asin() };
                y = (cosz - self.sinph0 * phi.sin()) * rh;
                x *= sinz * self.cosph0;
            }
            Mode::Equatorial => {
                let p = y * sinz / rh;
                phi = if p.abs() >= 1.0 { FRAC_PI_2.copysign(p) } else { p.asin() };
                y = cosz * rh;
                x *= sinz;
            }
            Mode::SouthPole => {
                phi = sinz.atan2(cosz) - FRAC_PI_2;
            }
            Mode::NorthPole => {
                phi = FRAC_PI_2 - sinz.atan2(cosz);
                y = -y;
            }
        }
        let lam = if y == 0.0 && matches!(self.mode, Mode::Oblique | Mode::Equatorial) {
            if x == 0.0 { 0.0 } else { FRAC_PI_2.copysign(x) }
        } else {
            x.atan2(y)
        };
        Ok((lam, phi))
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Equal Earth (PROJ eqearth.cpp)
// ---------------------------------------------------------------------------------------------------------------

struct EqualEarth {
    a: f64,
    e: f64,
    es: f64,
    one_es: f64,
    lam0: f64,
    qp: f64,
    rqda: f64,
}

impl EqualEarth {
    const A1: f64 = 1.340264;
    const A2: f64 = -0.081106;
    const A3: f64 = 0.000893;
    const A4: f64 = 0.003796;
    const M: f64 = 0.866_025_403_784_438_6; // sqrt(3)/2

    fn new(ellps: Ellipsoid, lam0: f64) -> EqualEarth {
        let e = ellps.e();
        let one_es = ellps.one_es();
        let (qp, rqda) = if ellps.es > 0.0 {
            let qp = qsfn(1.0, e, one_es);
            (qp, (0.5 * qp).sqrt())
        } else {
            (2.0, 1.0)
        };
        EqualEarth { a: ellps.a, e, es: ellps.es, one_es, lam0, qp, rqda }
    }
}

impl Projection for EqualEarth {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let mut sbeta = phi.sin();
        if self.es > 0.0 {
            sbeta = (qsfn(sbeta, self.e, self.one_es) / self.qp).clamp(-1.0, 1.0);
        }
        let psi = (Self::M * sbeta).asin();
        let psi2 = psi * psi;
        let psi6 = psi2 * psi2 * psi2;
        let x = lam * psi.cos()
            / (Self::M * (Self::A1 + 3.0 * Self::A2 * psi2 + psi6 * (7.0 * Self::A3 + 9.0 * Self::A4 * psi2)));
        let y = psi * (Self::A1 + Self::A2 * psi2 + psi6 * (Self::A3 + Self::A4 * psi2));
        Ok((x * self.rqda * self.a, y * self.rqda * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let x = x / (self.a * self.rqda);
        let y = y / (self.a * self.rqda);
        let mut yc = y;
        for _ in 0..12 {
            let y2 = yc * yc;
            let y6 = y2 * y2 * y2;
            let f = yc * (Self::A1 + Self::A2 * y2 + y6 * (Self::A3 + Self::A4 * y2)) - y;
            let fdash = Self::A1 + 3.0 * Self::A2 * y2 + y6 * (7.0 * Self::A3 + 9.0 * Self::A4 * y2);
            let tol = f / fdash;
            yc -= tol;
            if tol.abs() < 1e-12 {
                break;
            }
        }
        let y2 = yc * yc;
        let y6 = y2 * y2 * y2;
        let lam =
            Self::M * x * (Self::A1 + 3.0 * Self::A2 * y2 + y6 * (7.0 * Self::A3 + 9.0 * Self::A4 * y2)) / yc.cos();
        let beta = (yc.sin() / Self::M).clamp(-1.0, 1.0).asin();
        let phi = if self.es > 0.0 { authalic_to_geodetic(beta, self.e, self.es, self.qp)? } else { beta };
        Ok((lam, phi))
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

/// Inverse of the authalic latitude (Snyder 3-16, Newton iteration).
fn authalic_to_geodetic(beta: f64, e: f64, es: f64, qp: f64) -> Result<f64> {
    let q = beta.sin() * qp;
    if (q.abs() - qp.abs()).abs() < 1e-14 {
        return Ok(FRAC_PI_2.copysign(beta));
    }
    let mut phi = (q / 2.0).clamp(-1.0, 1.0).asin();
    for _ in 0..20 {
        let s = phi.sin();
        let c = phi.cos();
        let con = e * s;
        let com = 1.0 - con * con;
        let dphi =
            com * com / (2.0 * c) * (q / (1.0 - es) - s / com + 1.0 / (2.0 * e) * ((1.0 - con) / (1.0 + con)).ln());
        phi += dphi;
        if dphi.abs() < 1e-13 {
            return Ok(phi);
        }
    }
    bail!("authalic latitude inversion did not converge")
}

// ---------------------------------------------------------------------------------------------------------------
// Lambert azimuthal equal area (PROJ laea.cpp; proj4rs' version has bugs in the spherical oblique and the
// ellipsoidal south polar forms)
// ---------------------------------------------------------------------------------------------------------------

struct LambertAzimuthalEqualArea {
    a: f64,
    e: f64,
    es: f64,
    one_es: f64,
    lam0: f64,
    phi0: f64,
    mode: Mode,
    qp: f64,
    rq: f64,
    dd: f64,
    xmf: f64,
    ymf: f64,
    sinb1: f64,
    cosb1: f64,
}

impl LambertAzimuthalEqualArea {
    fn new(ellps: Ellipsoid, phi0: f64, lam0: f64) -> LambertAzimuthalEqualArea {
        let t = phi0.abs();
        let mode = if (t - FRAC_PI_2).abs() < EPS10 {
            if phi0 < 0.0 { Mode::SouthPole } else { Mode::NorthPole }
        } else if t < EPS10 {
            Mode::Equatorial
        } else {
            Mode::Oblique
        };
        let es = ellps.es;
        let e = ellps.e();
        let one_es = ellps.one_es();
        let mut p = LambertAzimuthalEqualArea {
            a: ellps.a,
            e,
            es,
            one_es,
            lam0,
            phi0,
            mode,
            qp: 0.0,
            rq: 0.0,
            dd: 1.0,
            xmf: 1.0,
            ymf: 1.0,
            sinb1: phi0.sin(),
            cosb1: phi0.cos(),
        };
        if es > 0.0 {
            p.qp = qsfn(1.0, e, one_es);
            p.rq = (0.5 * p.qp).sqrt();
            match mode {
                Mode::NorthPole | Mode::SouthPole => p.dd = 1.0,
                Mode::Equatorial => {
                    p.dd = 1.0 / p.rq;
                    p.xmf = 1.0;
                    p.ymf = 0.5 * p.qp;
                }
                Mode::Oblique => {
                    let sinphi = phi0.sin();
                    p.sinb1 = qsfn(sinphi, e, one_es) / p.qp;
                    p.cosb1 = (1.0 - p.sinb1 * p.sinb1).sqrt();
                    p.dd = phi0.cos() / ((1.0 - es * sinphi * sinphi).sqrt() * p.rq * p.cosb1);
                    p.ymf = p.rq / p.dd;
                    p.xmf = p.rq * p.dd;
                }
            }
        }
        p
    }

    fn e_forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let (sinlam, coslam) = lam.sin_cos();
        let mut q = qsfn(phi.sin(), self.e, self.one_es);
        let (mut sinb, mut cosb) = (0.0, 0.0);
        if matches!(self.mode, Mode::Oblique | Mode::Equatorial) {
            sinb = q / self.qp;
            let cosb2 = 1.0 - sinb * sinb;
            cosb = if cosb2 > 0.0 { cosb2.sqrt() } else { 0.0 };
        }
        let b = match self.mode {
            Mode::Oblique => 1.0 + self.sinb1 * sinb + self.cosb1 * cosb * coslam,
            Mode::Equatorial => 1.0 + cosb * coslam,
            Mode::NorthPole => {
                q = self.qp - q;
                FRAC_PI_2 + phi
            }
            Mode::SouthPole => {
                q += self.qp;
                phi - FRAC_PI_2
            }
        };
        if b.abs() < EPS10 {
            bail!("point outside of projection domain");
        }
        Ok(match self.mode {
            Mode::Oblique => {
                let b = (2.0 / b).sqrt();
                (self.xmf * b * cosb * sinlam, self.ymf * b * (self.cosb1 * sinb - self.sinb1 * cosb * coslam))
            }
            Mode::Equatorial => {
                let b = (2.0 / (1.0 + cosb * coslam)).sqrt();
                (self.xmf * b * cosb * sinlam, b * sinb * self.ymf)
            }
            Mode::NorthPole | Mode::SouthPole => {
                if q >= 0.0 {
                    let b = q.sqrt();
                    (b * sinlam, coslam * if self.mode == Mode::SouthPole { b } else { -b })
                } else {
                    (0.0, 0.0)
                }
            }
        })
    }

    fn e_inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (mut x, mut y) = (x, y);
        let ab;
        match self.mode {
            Mode::Equatorial | Mode::Oblique => {
                x /= self.dd;
                y *= self.dd;
                let rho = x.hypot(y);
                if rho < EPS10 {
                    return Ok((0.0, self.phi0));
                }
                let sce = 2.0 * aasin(0.5 * rho / self.rq);
                let (sce, cce) = sce.sin_cos();
                x *= sce;
                if self.mode == Mode::Oblique {
                    ab = cce * self.sinb1 + y * sce * self.cosb1 / rho;
                    y = rho * self.cosb1 * cce - y * self.sinb1 * sce;
                } else {
                    ab = y * sce / rho;
                    y = rho * cce;
                }
            }
            Mode::NorthPole | Mode::SouthPole => {
                if self.mode == Mode::NorthPole {
                    y = -y;
                }
                let q = x * x + y * y;
                if q == 0.0 {
                    return Ok((0.0, self.phi0));
                }
                let mut v = 1.0 - q / self.qp;
                if self.mode == Mode::SouthPole {
                    v = -v;
                }
                ab = v;
            }
        }
        let beta = aasin(ab);
        Ok((x.atan2(y), authalic_to_geodetic(beta, self.e, self.es, self.qp)?))
    }

    fn s_forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let (sinphi, cosphi) = phi.sin_cos();
        let mut coslam = lam.cos();
        match self.mode {
            Mode::Equatorial | Mode::Oblique => {
                let y = if self.mode == Mode::Equatorial {
                    1.0 + cosphi * coslam
                } else {
                    1.0 + self.sinb1 * sinphi + self.cosb1 * cosphi * coslam
                };
                if y <= EPS10 {
                    bail!("point outside of projection domain");
                }
                let y = (2.0 / y).sqrt();
                let x = y * cosphi * lam.sin();
                let y = y * if self.mode == Mode::Equatorial {
                    sinphi
                } else {
                    self.cosb1 * sinphi - self.sinb1 * cosphi * coslam
                };
                Ok((x, y))
            }
            Mode::NorthPole | Mode::SouthPole => {
                if self.mode == Mode::NorthPole {
                    coslam = -coslam;
                }
                if (phi + self.phi0).abs() < EPS10 {
                    bail!("point outside of projection domain");
                }
                let y = FRAC_PI_4 - phi * 0.5;
                let y = 2.0 * if self.mode == Mode::SouthPole { y.cos() } else { y.sin() };
                Ok((y * lam.sin(), y * coslam))
            }
        }
    }

    fn s_inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (mut x, mut y) = (x, y);
        let rh = x.hypot(y);
        let half = rh * 0.5;
        if half > 1.0 + 1e-12 {
            bail!("point outside of projection domain");
        }
        let mut phi = 2.0 * aasin(half);
        let (sinz, cosz) = phi.sin_cos();
        match self.mode {
            Mode::Equatorial => {
                phi = if rh <= EPS10 { 0.0 } else { aasin(y * sinz / rh) };
                x *= sinz;
                y = cosz * rh;
            }
            Mode::Oblique => {
                phi = if rh <= EPS10 { self.phi0 } else { aasin(cosz * self.sinb1 + y * sinz * self.cosb1 / rh) };
                x *= sinz * self.cosb1;
                y = (cosz - phi.sin() * self.sinb1) * rh;
            }
            Mode::NorthPole => {
                y = -y;
                phi = FRAC_PI_2 - phi;
            }
            Mode::SouthPole => phi -= FRAC_PI_2,
        }
        let lam = if y == 0.0 && matches!(self.mode, Mode::Equatorial | Mode::Oblique) { 0.0 } else { x.atan2(y) };
        Ok((lam, phi))
    }
}

impl Projection for LambertAzimuthalEqualArea {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let (x, y) = if self.es > 0.0 { self.e_forward(lam, phi)? } else { self.s_forward(lam, phi)? };
        Ok((x * self.a, y * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (x, y) = (x / self.a, y / self.a);
        if self.es > 0.0 { self.e_inverse(x, y) } else { self.s_inverse(x, y) }
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Azimuthal equidistant, with exact geodesics like PROJ and GeoTools (both use GeographicLib)
// ---------------------------------------------------------------------------------------------------------------

struct AzimuthalEquidistant {
    geodesic: geographiclib_rs::Geodesic,
    lam0: f64,
    /// latitude of the center in degrees
    phi0: f64,
}

impl AzimuthalEquidistant {
    fn new(ellps: Ellipsoid, phi0: f64, lam0: f64) -> AzimuthalEquidistant {
        let f = 1.0 - (1.0 - ellps.es).sqrt();
        AzimuthalEquidistant { geodesic: geographiclib_rs::Geodesic::new(ellps.a, f), lam0, phi0: phi0.to_degrees() }
    }
}

impl Projection for AzimuthalEquidistant {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        use geographiclib_rs::InverseGeodesic;
        let (s12, azi1, _azi2, _a12): (f64, f64, f64, f64) =
            self.geodesic.inverse(self.phi0, 0.0, phi.to_degrees(), lam.to_degrees());
        let (sin, cos) = azi1.to_radians().sin_cos();
        Ok((s12 * sin, s12 * cos))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        use geographiclib_rs::DirectGeodesic;
        let s12 = x.hypot(y);
        if s12 < 1e-9 {
            return Ok((0.0, self.phi0.to_radians()));
        }
        let azi1 = x.atan2(y).to_degrees();
        let (lat2, lon2): (f64, f64) = self.geodesic.direct(self.phi0, 0.0, azi1, s12);
        Ok((lon2.to_radians(), lat2.to_radians()))
    }

    fn lam0(&self) -> f64 {
        self.lam0
    }
}

// ---------------------------------------------------------------------------------------------------------------
// New Zealand Map Grid (PROJ nzmg.cpp)
// ---------------------------------------------------------------------------------------------------------------

struct NewZealandMapGrid {
    a: f64,
}

const NZMG_BF: [(f64, f64); 6] = [
    (0.7557853228, 0.0),
    (0.249204646, 0.003371507),
    (-0.001541739, 0.041058560),
    (-0.10162907, 0.01727609),
    (-0.26623489, -0.36249218),
    (-0.6870983, -1.1651967),
];
const NZMG_TPHI: [f64; 9] =
    [1.5627014243, 0.5185406398, -0.03333098, -0.1052906, -0.0368594, 0.007317, 0.01220, 0.00394, -0.0013];
const NZMG_TPSI: [f64; 10] = [
    0.6399175073,
    -0.1358797613,
    0.063294409,
    -0.02526853,
    0.0117879,
    -0.0055161,
    0.0026906,
    -0.001333,
    0.00067,
    -0.00034,
];

impl NewZealandMapGrid {
    const SEC5_TO_RAD: f64 = 0.484_813_681_109_536;
    const RAD_TO_SEC5: f64 = 2.062_648_062_470_963_6;
    const PHI0: f64 = -41.0 * PI / 180.0;

    fn mul(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
    }

    /// z * (C[0] + C[1] z + ... + C[n] z^n) and its derivative
    fn zpoly1(z: (f64, f64)) -> ((f64, f64), (f64, f64)) {
        let n = NZMG_BF.len() - 1;
        let mut a = NZMG_BF[n];
        let mut b = Self::mul(a, (n as f64 + 1.0, 0.0));
        for i in (0..n).rev() {
            let c = NZMG_BF[i];
            a = Self::mul(a, z);
            a = (a.0 + c.0, a.1 + c.1);
            b = Self::mul(b, z);
            b = (b.0 + c.0 * (i as f64 + 1.0), b.1 + c.1 * (i as f64 + 1.0));
        }
        (Self::mul(a, z), b)
    }
}

impl Projection for NewZealandMapGrid {
    fn forward(&self, lam: f64, phi: f64) -> Result<(f64, f64)> {
        let dphi = (phi - Self::PHI0) * Self::RAD_TO_SEC5;
        let mut r = NZMG_TPSI[NZMG_TPSI.len() - 1];
        for &c in NZMG_TPSI[..NZMG_TPSI.len() - 1].iter().rev() {
            r = c + dphi * r;
        }
        r *= dphi;
        let (p, _) = Self::zpoly1((r, lam));
        Ok((p.1 * self.a, p.0 * self.a))
    }

    fn inverse(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        let (x, y) = (x / self.a, y / self.a);
        let mut p = (y, x);
        let mut converged = false;
        for _ in 0..20 {
            let (f, fp) = Self::zpoly1(p);
            let f = (f.0 - y, f.1 - x);
            let den = fp.0 * fp.0 + fp.1 * fp.1;
            let dp = (-(f.0 * fp.0 + f.1 * fp.1) / den, -(f.1 * fp.0 - f.0 * fp.1) / den);
            p = (p.0 + dp.0, p.1 + dp.1);
            if dp.0.abs() + dp.1.abs() <= EPS10 {
                converged = true;
                break;
            }
        }
        if !converged {
            bail!("nzmg inverse did not converge");
        }
        let mut phi = NZMG_TPHI[NZMG_TPHI.len() - 1];
        for &c in NZMG_TPHI[..NZMG_TPHI.len() - 1].iter().rev() {
            phi = c + p.0 * phi;
        }
        Ok((p.1, Self::PHI0 + p.0 * phi * Self::SEC5_TO_RAD))
    }

    fn lam0(&self) -> f64 {
        173f64.to_radians()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(def: &str, points: &[(f64, f64)]) {
        let params = ProjParams::parse(def);
        let proj = CustomProjection::new(&params, Ellipsoid { a: 6378137.0, es: 0.006694379990141317 }).unwrap();
        for &(lon, lat) in points {
            let (x, y) = proj.forward(lon.to_radians(), lat.to_radians()).unwrap();
            let (lam, phi) = proj.inverse(x, y).unwrap();
            assert!(
                (lam.to_degrees() - lon).abs() < 1e-8 && (phi.to_degrees() - lat).abs() < 1e-8,
                "{def} {lon},{lat} -> {x},{y} -> {},{}",
                lam.to_degrees(),
                phi.to_degrees()
            );
        }
    }

    #[test]
    fn custom_projection_round_trips() {
        let pts = [(10.0, 50.0), (-5.0, 20.0), (3.0, -40.0), (0.5, 0.5)];
        round_trip("+proj=cass +lat_0=10 +lon_0=5 +x_0=100 +y_0=200", &[(5.5, 50.0), (4.0, 20.0), (6.0, -40.0)]);
        round_trip("+proj=poly +lat_0=10 +lon_0=5", &pts);
        round_trip("+proj=sinu +lon_0=5", &pts);
        round_trip("+proj=robin +lon_0=5", &[(10.0, 50.0), (-120.0, 21.0), (3.0, -87.0), (170.0, 89.0)]);
        round_trip("+proj=eqdc +lat_0=39 +lon_0=-96 +lat_1=33 +lat_2=45", &pts);
        round_trip("+proj=eqearth +lon_0=5", &[(10.0, 50.0), (-120.0, 20.0), (3.0, -87.0)]);
        round_trip("+proj=gnom +lat_0=45 +lon_0=5", &[(10.0, 50.0), (-5.0, 40.0)]);
        round_trip("+proj=ortho +lat_0=45 +lon_0=5", &[(10.0, 50.0), (-5.0, 40.0)]);
        round_trip("+proj=ortho +lat_0=90", &[(10.0, 50.0), (-5.0, 40.0)]);
        round_trip("+proj=gnom +lat_0=-90", &[(10.0, -50.0), (-5.0, -40.0)]);
        round_trip("+proj=omerc +lat_0=46.95 +lonc=7.44 +alpha=90 +gamma=90 +k=1", &[(7.0, 46.0), (8.0, 47.5)]);
        round_trip("+proj=omerc +no_uoff +lat_0=4 +lonc=115 +alpha=53.3 +gamma=53.1 +k=0.99984", &[(115.0, 4.0)]);
        round_trip("+proj=nzmg", &[(173.0, -41.0), (175.0, -37.0)]);
    }
}
