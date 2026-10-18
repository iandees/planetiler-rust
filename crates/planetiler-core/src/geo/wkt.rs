//! Minimal WKT reader and writer (replacement for JTS `WKTReader` / `WKTWriter`).
//!
//! Supports 2D `POINT`, `LINESTRING`, `LINEARRING`, `POLYGON`, `MULTIPOINT`, `MULTILINESTRING`, `MULTIPOLYGON` and
//! `GEOMETRYCOLLECTION`, including `EMPTY`. `Z`/`M`/`ZM` ordinates are parsed and dropped.

use std::fmt::Write;

use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

use crate::geo::geometry_exception::{GeometryException, GeometryResult};

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
    dims: usize,
}

fn err(msg: impl Into<String>) -> GeometryException {
    GeometryException::new("wkt_parse_error", msg)
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.pos).copied()
    }

    fn expect(&mut self, c: u8) -> GeometryResult<()> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(err(format!("expected '{}' at position {}", c as char, self.pos)))
        }
    }

    fn word(&mut self) -> String {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.pos]).to_ascii_uppercase()
    }

    fn number(&mut self) -> GeometryResult<f64> {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.s.len()
            && (self.s[self.pos].is_ascii_alphanumeric() || matches!(self.s[self.pos], b'.' | b'-' | b'+'))
        {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.s[start..self.pos]).map_err(|e| err(e.to_string()))?;
        text.parse::<f64>().map_err(|_| err(format!("invalid number '{text}' at position {start}")))
    }

    /// Parses the optional `Z`, `M`, `ZM` dimension tag, then returns true if the geometry is `EMPTY`.
    fn dims_and_empty(&mut self) -> GeometryResult<bool> {
        let save = self.pos;
        let w = self.word();
        match w.as_str() {
            "Z" | "M" => {
                self.dims = 3;
                self.dims_and_empty()
            }
            "ZM" => {
                self.dims = 4;
                self.dims_and_empty()
            }
            "EMPTY" => Ok(true),
            "" => Ok(false),
            _ => {
                self.pos = save;
                Err(err(format!("unexpected '{w}'")))
            }
        }
    }

    fn coord(&mut self) -> GeometryResult<Coord<f64>> {
        let x = self.number()?;
        let y = self.number()?;
        // extra ordinates
        while matches!(self.peek(), Some(c) if c != b',' && c != b')') {
            self.number()?;
        }
        Ok(Coord { x, y })
    }

    fn coords(&mut self) -> GeometryResult<Vec<Coord<f64>>> {
        if self.peek() != Some(b'(') {
            if self.word() == "EMPTY" {
                return Ok(vec![]);
            }
            return Err(err("expected '(' or EMPTY"));
        }
        self.expect(b'(')?;
        let mut out = vec![self.coord()?];
        while self.peek() == Some(b',') {
            self.pos += 1;
            out.push(self.coord()?);
        }
        self.expect(b')')?;
        Ok(out)
    }

    fn list<T>(&mut self, mut f: impl FnMut(&mut Self) -> GeometryResult<T>) -> GeometryResult<Vec<T>> {
        if self.peek() != Some(b'(') {
            if self.word() == "EMPTY" {
                return Ok(vec![]);
            }
            return Err(err("expected '(' or EMPTY"));
        }
        self.expect(b'(')?;
        let mut out = vec![f(self)?];
        while self.peek() == Some(b',') {
            self.pos += 1;
            out.push(f(self)?);
        }
        self.expect(b')')?;
        Ok(out)
    }

    fn polygon_body(&mut self) -> GeometryResult<Polygon<f64>> {
        let mut rings = self.list(|p| p.coords())?.into_iter().map(LineString);
        Ok(match rings.next() {
            None => Polygon::new(LineString(vec![]), vec![]),
            Some(shell) => Polygon::new(shell, rings.collect()),
        })
    }

    fn point_in_multipoint(&mut self) -> GeometryResult<Point<f64>> {
        if self.peek() == Some(b'(') {
            self.expect(b'(')?;
            let c = self.coord()?;
            self.expect(b')')?;
            Ok(Point(c))
        } else {
            Ok(Point(self.coord()?))
        }
    }

    fn geometry(&mut self) -> GeometryResult<Geometry<f64>> {
        let kind = self.word();
        let save_dims = self.dims;
        let empty = self.dims_and_empty().unwrap_or(false);
        let result = match kind.as_str() {
            "POINT" => {
                if empty {
                    Geometry::MultiPoint(MultiPoint(vec![]))
                } else {
                    self.expect(b'(')?;
                    let c = self.coord()?;
                    self.expect(b')')?;
                    Geometry::Point(Point(c))
                }
            }
            "LINESTRING" | "LINEARRING" => {
                Geometry::LineString(LineString(if empty { vec![] } else { self.coords()? }))
            }
            "POLYGON" => {
                Geometry::Polygon(if empty { Polygon::new(LineString(vec![]), vec![]) } else { self.polygon_body()? })
            }
            "MULTIPOINT" => {
                Geometry::MultiPoint(MultiPoint(if empty { vec![] } else { self.list(|p| p.point_in_multipoint())? }))
            }
            "MULTILINESTRING" => Geometry::MultiLineString(MultiLineString(if empty {
                vec![]
            } else {
                self.list(|p| p.coords().map(LineString))?
            })),
            "MULTIPOLYGON" => {
                Geometry::MultiPolygon(MultiPolygon(if empty { vec![] } else { self.list(|p| p.polygon_body())? }))
            }
            "GEOMETRYCOLLECTION" => Geometry::GeometryCollection(GeometryCollection(if empty {
                vec![]
            } else {
                self.list(|p| p.geometry())?
            })),
            other => return Err(err(format!("unknown geometry type '{other}'"))),
        };
        self.dims = save_dims;
        Ok(result)
    }
}

/// Parse a WKT string (JTS `WKTReader.read`).
pub fn read_wkt(wkt: &str) -> GeometryResult<Geometry<f64>> {
    let mut parser = Parser { s: wkt.as_bytes(), pos: 0, dims: 2 };
    let result = parser.geometry()?;
    parser.skip_ws();
    if parser.pos != parser.s.len() {
        return Err(err(format!("unexpected trailing input at position {}", parser.pos)));
    }
    Ok(result)
}

fn write_num(out: &mut String, v: f64) {
    if v == v.trunc() && v.abs() < 1e15 {
        let _ = write!(out, "{}", v as i64);
    } else {
        let _ = write!(out, "{v}");
    }
}

fn write_coords(out: &mut String, coords: &[Coord<f64>]) {
    if coords.is_empty() {
        out.push_str("EMPTY");
        return;
    }
    out.push('(');
    for (i, c) in coords.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_num(out, c.x);
        out.push(' ');
        write_num(out, c.y);
    }
    out.push(')');
}

fn write_polygon(out: &mut String, p: &Polygon<f64>) {
    if p.exterior().0.is_empty() {
        out.push_str("EMPTY");
        return;
    }
    out.push('(');
    write_coords(out, &p.exterior().0);
    for h in p.interiors() {
        out.push_str(", ");
        write_coords(out, &h.0);
    }
    out.push(')');
}

fn write_list<T>(out: &mut String, items: &[T], f: impl Fn(&mut String, &T)) {
    if items.is_empty() {
        out.push_str("EMPTY");
        return;
    }
    out.push('(');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        f(out, item);
    }
    out.push(')');
}

fn write_geometry(out: &mut String, geom: &Geometry<f64>) {
    match geom {
        Geometry::Point(p) => {
            out.push_str("POINT ");
            write_coords(out, &[p.0]);
        }
        Geometry::Line(l) => {
            out.push_str("LINESTRING ");
            write_coords(out, &[l.start, l.end]);
        }
        Geometry::LineString(ls) => {
            out.push_str("LINESTRING ");
            write_coords(out, &ls.0);
        }
        Geometry::Polygon(p) => {
            out.push_str("POLYGON ");
            write_polygon(out, p);
        }
        Geometry::Rect(r) => write_geometry(out, &Geometry::Polygon(r.to_polygon())),
        Geometry::Triangle(t) => write_geometry(out, &Geometry::Polygon(t.to_polygon())),
        Geometry::MultiPoint(mp) => {
            out.push_str("MULTIPOINT ");
            write_list(out, &mp.0, |o, p| write_coords(o, &[p.0]));
        }
        Geometry::MultiLineString(mls) => {
            out.push_str("MULTILINESTRING ");
            write_list(out, &mls.0, |o, l| write_coords(o, &l.0));
        }
        Geometry::MultiPolygon(mp) => {
            out.push_str("MULTIPOLYGON ");
            write_list(out, &mp.0, write_polygon);
        }
        Geometry::GeometryCollection(gc) => {
            out.push_str("GEOMETRYCOLLECTION ");
            write_list(out, &gc.0, write_geometry);
        }
    }
}

/// Format a geometry as WKT (JTS `WKTWriter.write`).
pub fn to_wkt(geom: &Geometry<f64>) -> String {
    let mut out = String::new();
    write_geometry(&mut out, geom);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::test_utils::*;

    #[test]
    fn round_trip() {
        for wkt in [
            "POINT (1 2)",
            "LINESTRING (0 0, 1 1.5)",
            "POLYGON ((0 0, 1 0, 1 1, 0 0), (0.1 0.1, 0.2 0.1, 0.2 0.2, 0.1 0.1))",
            "MULTIPOINT ((1 2), (3 4))",
            "MULTILINESTRING ((0 0, 1 1), (2 2, 3 3))",
            "MULTIPOLYGON (((0 0, 1 0, 1 1, 0 0)), ((5 5, 6 5, 6 6, 5 5)))",
            "GEOMETRYCOLLECTION (POINT (1 2), LINESTRING (0 0, 1 1))",
            "GEOMETRYCOLLECTION EMPTY",
            "POLYGON EMPTY",
        ] {
            assert_eq!(wkt, to_wkt(&read_wkt(wkt).unwrap()));
        }
    }

    #[test]
    fn parse_variants() {
        assert_eq!(Geometry::Point(new_point(1.0, 2.0)), read_wkt("point z(1 2 3)").unwrap());
        assert_eq!(
            Geometry::MultiPoint(new_multi_point(vec![new_point(1.0, 2.0), new_point(3.0, 4.0)])),
            read_wkt("MULTIPOINT (1 2, 3 4)").unwrap()
        );
        assert_eq!(Geometry::Polygon(rectangle(0.0, 1.0)), read_wkt("POLYGON(\n(0 0, 1 0, 1 1, 0 1, 0 0)\n)").unwrap());
        assert!(read_wkt("POLYGON((0 0, 1").is_err());
        assert!(read_wkt("garbage").is_err());
        assert_eq!(
            1e-7,
            match read_wkt("POINT (1e-7 0)").unwrap() {
                Geometry::Point(p) => p.x(),
                _ => panic!(),
            }
        );
    }
}
