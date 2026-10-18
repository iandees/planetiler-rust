//! Port of `reader/osm/PolyFileReader.java`: parse a polygon file used for filtering planetiler output to a specific
//! shape.
//!
//! See <https://wiki.openstreetmap.org/wiki/Osmosis/Polygon_Filter_File_Format>.

use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, bail};
use geo::BooleanOps;
use geo_types::{Coord, LineString, MultiPolygon, Polygon};

/// Reads a polygon from a file.
pub fn parse_poly_file(path: &Path) -> anyhow::Result<MultiPolygon<f64>> {
    let file = std::fs::File::open(path).with_context(|| format!("Unable to open {path:?}"))?;
    parse_poly_reader(std::io::BufReader::new(file))
}

/// Reads a polygon from a string.
pub fn parse_poly_string(poly: &str) -> anyhow::Result<MultiPolygon<f64>> {
    parse_poly_reader(poly.as_bytes())
}

/// Reads a polygon from a reader.
pub fn parse_poly_reader(reader: impl BufRead) -> anyhow::Result<MultiPolygon<f64>> {
    let mut result = MultiPolygon::<f64>(vec![]);
    let mut current_ring: Vec<Coord<f64>> = Vec::new();
    let (mut first_line, mut in_ring, mut in_polygon, mut hole) = (true, false, true, false);
    for line in reader.lines() {
        let line = line?;
        let stripped = line.trim();
        if stripped.is_empty() {
            // ignore line
        } else if !in_polygon {
            bail!("File continues after end of polygon");
        } else if first_line {
            // first line is junk.
            first_line = false;
        } else if in_ring {
            if stripped == "END" {
                // we are at the end of a ring, perhaps with more to come.
                let mut ring = LineString::from(std::mem::take(&mut current_ring));
                ring.close();
                let polygon = MultiPolygon(vec![Polygon::new(ring, vec![])]);
                result = if hole { result.difference(&polygon) } else { result.union(&polygon) };
                in_ring = false;
            } else {
                // we are in a ring and picking up new coordinates.
                let mut parts = stripped.split_whitespace();
                let mut next = || -> anyhow::Result<f64> {
                    let part = parts.next().with_context(|| format!("Invalid coordinate line: {line}"))?;
                    part.parse::<f64>().with_context(|| format!("Invalid coordinate: {part}"))
                };
                let x = next()?;
                let y = next()?;
                current_ring.push(Coord { x, y });
            }
        } else if stripped == "END" {
            // we are at the end of the whole polygon.
            in_polygon = false;
        } else {
            // we are at the start of a polygon part.
            current_ring.clear();
            hole = stripped.starts_with('!');
            in_ring = true;
        }
    }
    if in_ring {
        bail!("Unclosed ring");
    }
    if in_polygon {
        bail!("File ends before end of polygon");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    #[test]
    fn test_parse_australia() {
        let poly = parse_poly_string(
            "australia_v
first_area
     0.1446693E+03    -0.3826255E+02
     0.1446627E+03    -0.3825661E+02
     0.1446763E+03    -0.3824465E+02
     0.1446813E+03    -0.3824343E+02
     0.1446824E+03    -0.3824484E+02
     0.1446826E+03    -0.3825356E+02
     0.1446876E+03    -0.3825210E+02
     0.1446919E+03    -0.3824719E+02
     0.1447006E+03    -0.3824723E+02
     0.1447042E+03    -0.3825078E+02
     0.1446758E+03    -0.3826229E+02
     0.1446693E+03    -0.3826255E+02
END
second_area
     0.1422436E+03    -0.3839315E+02
     0.1422496E+03    -0.3839070E+02
     0.1422543E+03    -0.3839025E+02
     0.1422574E+03    -0.3839155E+02
     0.1422467E+03    -0.3840065E+02
     0.1422433E+03    -0.3840048E+02
     0.1422420E+03    -0.3839857E+02
     0.1422436E+03    -0.3839315E+02
END
END
",
        )
        .unwrap();
        assert_eq!(2, poly.0.len());
        assert!((4.60252e-4 - poly.unsigned_area()).abs() < 1e-10, "{}", poly.unsigned_area());
    }

    #[test]
    fn test_parse_australia_oceania() {
        let poly = parse_poly_string(
            "australia-oceania
1
   -107.863281   11.780702
   -104.171875   -28.082042
   -179.999999   -45.652740
   -179.999999   4.082818
   -107.863281   11.780702
END
0
   89.512500   -11.143360
   61.663780   -9.177713
   44.655470   -57.087780
   180.000000   -57.164820
   180.000000   26.277810
   141.547997   22.628320
   130.145100   3.640314
   129.953200   -0.535293
   131.061600   -3.784815
   130.266900   -10.043780
   118.255700   -13.011650
   102.800900   -8.390453
   89.512500   -11.143360
END
END
",
        )
        .unwrap();
        assert_eq!(2, poly.0.len());
        assert!((10876.51613 - poly.unsigned_area()).abs() < 1e-4, "{}", poly.unsigned_area());
    }

    #[test]
    fn test_parse_invalid() {
        assert!(parse_poly_string("name\nsection\n   1 2\n   3 4\n   5 6\n   7 8\n").is_err());
        assert!(parse_poly_string("name\nsection\n   1 2\n   3 4\n   5 6\n   7 8\nEND\n").is_err());
        parse_poly_string("name\nsection\n   1 2\n   3 4\n   5 6\n   7 8\nEND\nEND\n").unwrap();
        parse_poly_string("\n\nname\n\nsection\n\n   1 2\n   3 4\n   5 6\n   7 8\n\nEND\n\nEND\n\n\n").unwrap();
        assert!(
            parse_poly_string(
                "name\nsection\n   1 2\n   3 4\n   5 6\n   7 8\nEND\nEND\nname\nsection\n   1 2\n   3 4\nEND\nEND\n"
            )
            .is_err()
        );
    }

    #[test]
    fn test_parse_hole() {
        let poly = parse_poly_string(
            "poly\nouter\n   0 0\n   0 10\n   10 10\n   10 0\n   0 0\nEND\n!inner\n   1 1\n   1 9\n   9 9\n   9 1\n   1 1\nEND\nEND\n",
        )
        .unwrap();
        assert_eq!(1, poly.0.len());
        assert!((100.0 - 64.0 - poly.unsigned_area()).abs() < 1e-4);
    }

    #[test]
    fn test_parse_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/bottomrightearth.poly");
        let poly = parse_poly_file(&path).unwrap();
        assert_eq!(1, poly.0.len());
    }
}
