//! Port of `reader/parquet/GeoArrow.java`: utilities for converting nested
//! [geoarrow](https://github.com/opengeospatial/geoparquet/blob/main/format-specs/geoparquet.md#native-encodings-based-on-geoarrow)
//! coordinate lists to geometries.
//!
//! There are 2 entry points:
//! - [`GeoArrowColumn`] decodes geometries directly from arrow arrays (`struct<x, y>` coordinates nested in 0-3 levels
//!   of lists), which is what [`ParquetInputFile`](super::parquet_input_file::ParquetInputFile) uses
//! - [`from_value`] decodes geometries from already-converted [`Value`]s (`{x, y}` maps nested in lists), like Java's
//!   `GeoArrow.multipolygon(List<List<CoordinateSequence>>)` etc.

use std::ops::Range;

use anyhow::{Result, anyhow, bail};
use arrow::array::{Array, ArrayRef, AsArray, Float64Array, GenericListArray, OffsetSizeTrait, StructArray};
use arrow::datatypes::{DataType, Float64Type};
use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};

use crate::geo::geo_utils;
use crate::value::Value;

/// The geoarrow geometry encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeoArrowEncoding {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

impl GeoArrowEncoding {
    pub fn parse(encoding: &str) -> Option<GeoArrowEncoding> {
        Some(match encoding {
            "point" => GeoArrowEncoding::Point,
            "linestring" => GeoArrowEncoding::LineString,
            "polygon" => GeoArrowEncoding::Polygon,
            "multipoint" => GeoArrowEncoding::MultiPoint,
            "multilinestring" => GeoArrowEncoding::MultiLineString,
            "multipolygon" => GeoArrowEncoding::MultiPolygon,
            _ => return None,
        })
    }

    /// Number of list levels around the coordinate struct.
    pub fn nesting(self) -> usize {
        match self {
            GeoArrowEncoding::Point => 0,
            GeoArrowEncoding::LineString | GeoArrowEncoding::MultiPoint => 1,
            GeoArrowEncoding::Polygon | GeoArrowEncoding::MultiLineString => 2,
            GeoArrowEncoding::MultiPolygon => 3,
        }
    }

    /// Human-readable arrow type this encoding requires.
    pub fn pretty_type(self) -> &'static str {
        match self.nesting() {
            0 => "Coordinate",
            1 => "list<Coordinate>",
            2 => "list<list<Coordinate>>",
            _ => "list<list<list<Coordinate>>>",
        }
    }
}

/// True if `data_type` is a geoarrow coordinate struct: `struct<x, y[, z[, m]]>` of doubles.
pub fn is_coordinate_type(data_type: &DataType) -> bool {
    let DataType::Struct(fields) = data_type else { return false };
    let names: Vec<&str> = fields.iter().map(|f| f.name().as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    fields.iter().all(|f| f.data_type() == &DataType::Float64)
        && (sorted == ["x", "y"] || sorted == ["x", "y", "z"] || sorted == ["m", "x", "y", "z"])
}

/// True if `data_type` matches the arrow type required by `encoding`.
pub fn is_valid_type(encoding: GeoArrowEncoding, data_type: &DataType) -> bool {
    let mut t = data_type;
    for _ in 0..encoding.nesting() {
        t = match t {
            DataType::List(f) | DataType::LargeList(f) => f.data_type(),
            _ => return false,
        };
    }
    is_coordinate_type(t)
}

enum Level<'a> {
    I32(&'a GenericListArray<i32>),
    I64(&'a GenericListArray<i64>),
}

impl Level<'_> {
    fn range(&self, i: usize) -> Range<usize> {
        fn r<O: OffsetSizeTrait>(l: &GenericListArray<O>, i: usize) -> Range<usize> {
            let offsets = l.value_offsets();
            offsets[i].as_usize()..offsets[i + 1].as_usize()
        }
        match self {
            Level::I32(l) => r(l, i),
            Level::I64(l) => r(l, i),
        }
    }

    fn is_null(&self, i: usize) -> bool {
        match self {
            Level::I32(l) => l.is_null(i),
            Level::I64(l) => l.is_null(i),
        }
    }
}

/// Decodes geometries from a geoarrow-encoded arrow column.
pub struct GeoArrowColumn<'a> {
    encoding: GeoArrowEncoding,
    root: &'a ArrayRef,
    levels: Vec<Level<'a>>,
    coords: &'a StructArray,
    x: &'a Float64Array,
    y: &'a Float64Array,
}

impl<'a> GeoArrowColumn<'a> {
    pub fn new(encoding: GeoArrowEncoding, array: &'a ArrayRef) -> Result<GeoArrowColumn<'a>> {
        let mut levels = vec![];
        let mut current: &'a dyn Array = array.as_ref();
        for _ in 0..encoding.nesting() {
            current = match current.data_type() {
                DataType::List(_) => {
                    let l = current.as_list::<i32>();
                    levels.push(Level::I32(l));
                    l.values().as_ref()
                }
                DataType::LargeList(_) => {
                    let l = current.as_list::<i64>();
                    levels.push(Level::I64(l));
                    l.values().as_ref()
                }
                other => bail!("{} type required for geoarrow column, got {other}", encoding.pretty_type()),
            };
        }
        let coords = current.as_struct_opt().ok_or_else(|| anyhow!("geoarrow coordinates must be a struct"))?;
        let x = coords.column_by_name("x").and_then(|c| c.as_primitive_opt::<Float64Type>());
        let y = coords.column_by_name("y").and_then(|c| c.as_primitive_opt::<Float64Type>());
        let (Some(x), Some(y)) = (x, y) else { bail!("geoarrow coordinates must have double x and y fields") };
        Ok(GeoArrowColumn { encoding, root: array, levels, coords, x, y })
    }

    fn coords(&self, range: Range<usize>) -> Vec<Coord<f64>> {
        let x = self.x.values();
        let y = self.y.values();
        range.map(|i| Coord { x: x[i], y: y[i] }).collect()
    }

    fn ring_list(&self, level: usize, range: Range<usize>) -> Vec<LineString<f64>> {
        range.map(|i| LineString(self.coords(self.levels[level].range(i)))).collect()
    }

    fn polygon(&self, level: usize, i: usize) -> Polygon<f64> {
        let mut rings = self.ring_list(level + 1, self.levels[level].range(i)).into_iter();
        match rings.next() {
            Some(shell) => Polygon::new(shell, rings.collect()),
            None => geo_utils::empty_polygon(),
        }
    }

    /// Decode the geometry at `row`, or `None` if it is null.
    pub fn get(&self, row: usize) -> Option<Geometry<f64>> {
        if self.root.is_null(row) {
            return None;
        }
        Some(match self.encoding {
            GeoArrowEncoding::Point => {
                if self.coords.is_null(row) {
                    return None;
                }
                Geometry::Point(Point::new(self.x.value(row), self.y.value(row)))
            }
            GeoArrowEncoding::LineString => Geometry::LineString(LineString(self.coords(self.levels[0].range(row)))),
            GeoArrowEncoding::MultiPoint => Geometry::MultiPoint(MultiPoint(
                self.coords(self.levels[0].range(row)).into_iter().map(Point).collect(),
            )),
            GeoArrowEncoding::MultiLineString => {
                Geometry::MultiLineString(MultiLineString(self.ring_list(1, self.levels[0].range(row))))
            }
            GeoArrowEncoding::Polygon => Geometry::Polygon(self.polygon(0, row)),
            GeoArrowEncoding::MultiPolygon => Geometry::MultiPolygon(MultiPolygon(
                self.levels[0].range(row).filter(|&i| !self.levels[1].is_null(i)).map(|i| self.polygon(1, i)).collect(),
            )),
        })
    }
}

fn coord_from_value(value: &Value) -> Result<Coord<f64>> {
    let map = value.as_map().ok_or_else(|| anyhow!("expected coordinate struct, got {value}"))?;
    let x = map.get("x").and_then(Value::as_f64).ok_or_else(|| anyhow!("missing x in {value}"))?;
    let y = map.get("y").and_then(Value::as_f64).ok_or_else(|| anyhow!("missing y in {value}"))?;
    Ok(Coord { x, y })
}

fn list(value: &Value) -> Result<&[Value]> {
    value.as_list().ok_or_else(|| anyhow!("expected list, got {value}"))
}

fn line_from_value(value: &Value) -> Result<LineString<f64>> {
    Ok(LineString(list(value)?.iter().map(coord_from_value).collect::<Result<_>>()?))
}

fn polygon_from_value(value: &Value) -> Result<Polygon<f64>> {
    let mut rings = list(value)?.iter().map(line_from_value).collect::<Result<Vec<_>>>()?.into_iter();
    Ok(match rings.next() {
        Some(shell) => Polygon::new(shell, rings.collect()),
        None => geo_utils::empty_polygon(),
    })
}

/// Decode a geoarrow geometry from nested [`Value`] lists of `{x, y}` maps.
pub fn from_value(encoding: GeoArrowEncoding, value: &Value) -> Result<Geometry<f64>> {
    Ok(match encoding {
        GeoArrowEncoding::Point => Geometry::Point(Point(coord_from_value(value)?)),
        GeoArrowEncoding::LineString => Geometry::LineString(line_from_value(value)?),
        GeoArrowEncoding::MultiPoint => Geometry::MultiPoint(MultiPoint(
            list(value)?.iter().map(|c| coord_from_value(c).map(Point)).collect::<Result<_>>()?,
        )),
        GeoArrowEncoding::MultiLineString => {
            Geometry::MultiLineString(MultiLineString(list(value)?.iter().map(line_from_value).collect::<Result<_>>()?))
        }
        GeoArrowEncoding::Polygon => Geometry::Polygon(polygon_from_value(value)?),
        GeoArrowEncoding::MultiPolygon => {
            Geometry::MultiPolygon(MultiPolygon(list(value)?.iter().map(polygon_from_value).collect::<Result<_>>()?))
        }
    })
}
