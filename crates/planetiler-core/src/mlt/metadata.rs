//! Port of `org.maplibre.mlt.metadata.tileset.MltMetadata`, `MltTypeMap.Tag0x01` and the embedded layer metadata
//! header (`MltConverter.createEmbeddedMetadata` / `MltDecoder.parseEmbeddedMetadata`).

use super::encoding::{decode_varint, put_string, put_varint};

/// `MltMetadata.ScalarType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarType {
    Boolean,
    Int8,
    Uint8,
    Int32,
    Uint32,
    Int64,
    Uint64,
    Float,
    Double,
    String,
}

/// The type of a column or field: a physical scalar, the logical `ID` scalar, or a complex type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnType {
    /// `LogicalScalarType.ID`, with `hasLongId`.
    Id {
        long: bool,
    },
    Scalar(ScalarType),
    /// `ComplexType.GEOMETRY`.
    Geometry,
    /// `ComplexType.STRUCT` with its child fields.
    Struct(Vec<Column>),
}

/// `MltMetadata.Column` / `Field` (all columns are `FEATURE` scoped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub nullable: bool,
    pub column_type: ColumnType,
}

impl Column {
    pub fn scalar(name: impl Into<String>, nullable: bool, scalar_type: ScalarType) -> Column {
        Column { name: name.into(), nullable, column_type: ColumnType::Scalar(scalar_type) }
    }

    pub fn scalar_type(&self) -> Option<ScalarType> {
        match self.column_type {
            ColumnType::Scalar(t) => Some(t),
            _ => None,
        }
    }

    /// `MltTypeMap.Tag0x01.isID`.
    pub fn is_id(&self) -> bool {
        matches!(self.column_type, ColumnType::Id { .. })
    }

    /// `MltTypeMap.Tag0x01.isGeometry`.
    pub fn is_geometry(&self) -> bool {
        matches!(self.column_type, ColumnType::Geometry)
    }

    /// `MltTypeMap.Tag0x01.hasStreamCount`.
    pub fn has_stream_count(&self) -> bool {
        match &self.column_type {
            ColumnType::Scalar(ScalarType::String) | ColumnType::Geometry | ColumnType::Struct(_) => true,
            ColumnType::Scalar(_) | ColumnType::Id { .. } => false,
        }
    }

    /// `MltTypeMap.Tag0x01.encodeColumnType`.
    pub fn type_code(&self) -> anyhow::Result<u32> {
        let n = self.nullable as u32;
        Ok(match &self.column_type {
            ColumnType::Scalar(t) => {
                let base = match t {
                    ScalarType::Boolean => 10,
                    ScalarType::Int8 => 12,
                    ScalarType::Uint8 => 14,
                    ScalarType::Int32 => 16,
                    ScalarType::Uint32 => 18,
                    ScalarType::Int64 => 20,
                    ScalarType::Uint64 => 22,
                    ScalarType::Float => 24,
                    ScalarType::Double => 26,
                    ScalarType::String => 28,
                };
                base + n
            }
            ColumnType::Id { long } => {
                if self.nullable {
                    if *long { 3 } else { 1 }
                } else if *long {
                    2
                } else {
                    0
                }
            }
            ColumnType::Geometry if !self.nullable => 4,
            ColumnType::Struct(children) if !self.nullable && !children.is_empty() => 30,
            _ => anyhow::bail!("Unsupported Type {self:?}"),
        })
    }

    /// `MltTypeMap.Tag0x01.decodeColumnType` (children and name are filled in by the caller).
    fn from_type_code(code: u32) -> anyhow::Result<Column> {
        let nullable = code & 1 != 0;
        let column_type = match code {
            0..=3 => ColumnType::Id { long: code > 1 },
            4 => ColumnType::Geometry,
            10..=29 => ColumnType::Scalar(match code {
                10 | 11 => ScalarType::Boolean,
                12 | 13 => ScalarType::Int8,
                14 | 15 => ScalarType::Uint8,
                16 | 17 => ScalarType::Int32,
                18 | 19 => ScalarType::Uint32,
                20 | 21 => ScalarType::Int64,
                22 | 23 => ScalarType::Uint64,
                24 | 25 => ScalarType::Float,
                26 | 27 => ScalarType::Double,
                _ => ScalarType::String,
            }),
            30 => ColumnType::Struct(Vec::new()),
            other => anyhow::bail!("Unsupported Type {other}"),
        };
        let nullable = match column_type {
            ColumnType::Geometry | ColumnType::Struct(_) => false,
            _ => nullable,
        };
        Ok(Column { name: String::new(), nullable, column_type })
    }

    fn write(&self, out: &mut Vec<u8>) -> anyhow::Result<()> {
        let code = self.type_code()?;
        put_varint(out, code);
        // MltTypeMap.Tag0x01.columnTypeHasName
        if code >= 10 {
            put_string(out, &self.name);
        }
        if let ColumnType::Struct(children) = &self.column_type {
            put_varint(out, children.len() as u32);
            for child in children {
                child.write(out)?;
            }
        }
        Ok(())
    }

    fn read(data: &[u8], offset: &mut usize) -> anyhow::Result<Column> {
        let code = decode_varint(data, offset)?;
        let mut column = Column::from_type_code(code)?;
        if code >= 10 {
            column.name = read_string(data, offset)?;
        }
        if code == 30 {
            let count = decode_varint(data, offset)?;
            let mut children = Vec::with_capacity(count as usize);
            for _ in 0..count {
                children.push(Column::read(data, offset)?);
            }
            column.column_type = ColumnType::Struct(children);
        }
        Ok(column)
    }
}

/// `MltMetadata.FeatureTable`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureTable {
    pub name: String,
    pub columns: Vec<Column>,
}

impl FeatureTable {
    /// `MltConverter.createEmbeddedMetadata`.
    pub fn encode_embedded(&self, extent: u32, out: &mut Vec<u8>) -> anyhow::Result<()> {
        put_string(out, &self.name);
        put_varint(out, extent);
        put_varint(out, self.columns.len() as u32);
        for column in &self.columns {
            column.write(out)?;
        }
        Ok(())
    }

    /// `MltDecoder.parseEmbeddedMetadata`: returns the table and the tile extent.
    pub fn parse_embedded(data: &[u8], offset: &mut usize) -> anyhow::Result<(FeatureTable, u32)> {
        let name = read_string(data, offset)?;
        let extent = decode_varint(data, offset)?;
        let count = decode_varint(data, offset)?;
        let mut columns = Vec::with_capacity(count as usize);
        for _ in 0..count {
            columns.push(Column::read(data, offset)?);
        }
        Ok((FeatureTable { name, columns }, extent))
    }
}

fn read_string(data: &[u8], offset: &mut usize) -> anyhow::Result<String> {
    let len = decode_varint(data, offset)? as usize;
    let bytes = data.get(*offset..*offset + len).ok_or_else(|| anyhow::anyhow!("truncated string"))?;
    *offset += len;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// `MltMetadata.TileSetMetadata` (only the feature tables are used).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TileSetMetadata {
    pub feature_tables: Vec<FeatureTable>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_metadata_roundtrip() {
        let table = FeatureTable {
            name: "layer".into(),
            columns: vec![
                Column { name: String::new(), nullable: false, column_type: ColumnType::Id { long: false } },
                Column { name: String::new(), nullable: false, column_type: ColumnType::Geometry },
                Column::scalar("a", true, ScalarType::String),
                Column {
                    name: "name".into(),
                    nullable: false,
                    column_type: ColumnType::Struct(vec![
                        Column::scalar("", true, ScalarType::String),
                        Column::scalar(":en", true, ScalarType::String),
                    ]),
                },
            ],
        };
        let mut out = Vec::new();
        table.encode_embedded(4096, &mut out).unwrap();
        let mut offset = 0;
        let (parsed, extent) = FeatureTable::parse_embedded(&out, &mut offset).unwrap();
        assert_eq!(4096, extent);
        assert_eq!(table, parsed);
        assert_eq!(out.len(), offset);
    }
}
