//! Port of `reader/geojson/GeoJsonFeatureIterator.java`: internal streaming geojson parsing utility.
//!
//! Java uses Jackson's streaming `JsonParser`. This file contains a small pull tokenizer ([`JsonParser`]) with the same
//! token model (`START_OBJECT`, `FIELD_NAME`, `VALUE_*`, ...) and `skipChildren` semantics, so the iterator state
//! machine is a direct port of the Java one. Only one token is held in memory at a time; `properties` and `geometry`
//! objects are materialized individually.

use std::io::Read;

use anyhow::{Result, anyhow, bail};
use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use tracing::warn;

use crate::geo::geo_utils;
use crate::reader::geojson::geo_json_feature::GeoJsonFeature;
use crate::value::{Tags, Value};

/// A JSON token, equivalent to Jackson's `JsonToken` (with its value).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Token {
    StartObject,
    EndObject,
    StartArray,
    EndArray,
    FieldName(String),
    String(String),
    Int(i64),
    Float(f64),
    True,
    False,
    Null,
}

impl Token {
    fn is_struct_end(&self) -> bool {
        matches!(self, Token::EndObject | Token::EndArray)
    }

    fn describe(&self) -> &'static str {
        match self {
            Token::StartObject => "START_OBJECT",
            Token::EndObject => "END_OBJECT",
            Token::StartArray => "START_ARRAY",
            Token::EndArray => "END_ARRAY",
            Token::FieldName(_) => "FIELD_NAME",
            Token::String(_) => "VALUE_STRING",
            Token::Int(_) => "VALUE_NUMBER_INT",
            Token::Float(_) => "VALUE_NUMBER_FLOAT",
            Token::True => "VALUE_TRUE",
            Token::False => "VALUE_FALSE",
            Token::Null => "VALUE_NULL",
        }
    }
}

/// A fully-parsed JSON value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn as_number(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(b),
            Json::Int(i) => Value::Int(i),
            Json::Float(f) => Value::Double(f),
            Json::String(s) => Value::str(s),
            Json::Array(a) => Value::list(a.into_iter().map(Json::into_value)),
            Json::Object(o) => Value::map(into_tags(o)),
        }
    }
}

fn into_tags(entries: Vec<(String, Json)>) -> Tags {
    let mut tags = crate::value::tags_with_capacity(entries.len());
    for (k, v) in entries {
        tags.insert(k, v.into_value());
    }
    tags
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Json::Null => write!(f, "null"),
            Json::Bool(b) => write!(f, "{b}"),
            Json::Int(i) => write!(f, "{i}"),
            Json::Float(d) => write!(f, "{d}"),
            Json::String(s) => write!(f, "{s}"),
            Json::Array(a) => {
                write!(f, "[")?;
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Json::Object(o) => {
                write!(f, "{{")?;
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}={v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Context {
    /// inside an object, next string is a field name
    ObjectKey,
    /// inside an object, after a field name
    ObjectValue,
    Array,
}

/// Minimal streaming JSON tokenizer modelled after Jackson's `JsonParser` (accepts multiple whitespace-separated root
/// values, like newline-delimited JSON).
pub(crate) struct JsonParser<R: Read> {
    reader: R,
    buf: Box<[u8]>,
    pos: usize,
    len: usize,
    eof: bool,
    line: usize,
    column: usize,
    token_line: usize,
    token_column: usize,
    stack: Vec<Context>,
    current: Option<Token>,
    closed: bool,
    scratch: Vec<u8>,
}

impl<R: Read> JsonParser<R> {
    pub(crate) fn new(reader: R) -> JsonParser<R> {
        JsonParser {
            reader,
            buf: vec![0u8; 64 * 1024].into_boxed_slice(),
            pos: 0,
            len: 0,
            eof: false,
            line: 1,
            column: 1,
            token_line: 1,
            token_column: 1,
            stack: Vec::new(),
            current: None,
            closed: false,
            scratch: Vec::new(),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    /// Line and column of the start of the current token.
    pub(crate) fn token_location(&self) -> (usize, usize) {
        (self.token_line, self.token_column)
    }

    /// The current token.
    #[cfg(test)]
    pub(crate) fn current_token(&self) -> Option<&Token> {
        self.current.as_ref()
    }

    fn peek_byte(&mut self) -> Result<Option<u8>> {
        if self.pos >= self.len {
            if self.eof {
                return Ok(None);
            }
            loop {
                match self.reader.read(&mut self.buf) {
                    Ok(0) => {
                        self.eof = true;
                        return Ok(None);
                    }
                    Ok(n) => {
                        self.pos = 0;
                        self.len = n;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(Some(self.buf[self.pos]))
    }

    fn next_byte(&mut self) -> Result<Option<u8>> {
        let b = self.peek_byte()?;
        if let Some(b) = b {
            self.pos += 1;
            if b == b'\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        Ok(b)
    }

    fn expect_byte(&mut self) -> Result<u8> {
        self.next_byte()?.ok_or_else(|| self.error("Unexpected end-of-input"))
    }

    fn error(&self, message: &str) -> anyhow::Error {
        anyhow!("{message} at line {} column {}", self.line, self.column)
    }

    fn skip_whitespace(&mut self) -> Result<Option<u8>> {
        loop {
            match self.peek_byte()? {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.next_byte()?;
                }
                other => return Ok(other),
            }
        }
    }

    /// Advance to the next token, returning `None` (and closing the parser) at the end of input.
    pub(crate) fn next_token(&mut self) -> Result<Option<Token>> {
        let token = self.read_token()?;
        if token.is_none() {
            self.closed = true;
        }
        self.current = token.clone();
        Ok(token)
    }

    fn read_token(&mut self) -> Result<Option<Token>> {
        let mut b = self.skip_whitespace()?;
        // separators
        // (the ':' after a field name is consumed when reading the field name)
        if matches!(self.stack.last(), Some(Context::ObjectKey | Context::Array)) && b == Some(b',') {
            self.next_byte()?;
            b = self.skip_whitespace()?;
        }
        self.token_line = self.line;
        self.token_column = self.column;
        let Some(c) = b else {
            if !self.stack.is_empty() {
                bail!(self.error("Unexpected end-of-input: expected close marker"));
            }
            return Ok(None);
        };
        let ctx = self.stack.last().copied();
        let token = match c {
            b'{' => {
                self.next_byte()?;
                self.after_value();
                self.stack.push(Context::ObjectKey);
                Token::StartObject
            }
            b'[' => {
                self.next_byte()?;
                self.after_value();
                self.stack.push(Context::Array);
                Token::StartArray
            }
            b'}' => {
                self.next_byte()?;
                if !matches!(ctx, Some(Context::ObjectKey)) {
                    bail!(self.error("Unexpected close marker '}'"));
                }
                self.stack.pop();
                Token::EndObject
            }
            b']' => {
                self.next_byte()?;
                if !matches!(ctx, Some(Context::Array)) {
                    bail!(self.error("Unexpected close marker ']'"));
                }
                self.stack.pop();
                Token::EndArray
            }
            b'"' if ctx == Some(Context::ObjectKey) => {
                self.next_byte()?;
                let name = self.read_string()?;
                if self.skip_whitespace()? != Some(b':') {
                    bail!(self.error("Expected ':' after field name"));
                }
                self.next_byte()?;
                *self.stack.last_mut().unwrap() = Context::ObjectValue;
                Token::FieldName(name)
            }
            _ if ctx == Some(Context::ObjectKey) => bail!(self.error("Expected field name")),
            b'"' => {
                self.next_byte()?;
                let s = self.read_string()?;
                self.after_value();
                Token::String(s)
            }
            b't' => {
                self.read_literal(b"true")?;
                self.after_value();
                Token::True
            }
            b'f' => {
                self.read_literal(b"false")?;
                self.after_value();
                Token::False
            }
            b'n' => {
                self.read_literal(b"null")?;
                self.after_value();
                Token::Null
            }
            b'-' | b'0'..=b'9' => {
                let t = self.read_number()?;
                self.after_value();
                t
            }
            other => bail!(self.error(&format!("Unexpected character '{}'", other as char))),
        };
        Ok(Some(token))
    }

    /// After reading a value (or start marker) in an object, the next thing is a key.
    fn after_value(&mut self) {
        if let Some(last) = self.stack.last_mut()
            && *last == Context::ObjectValue
        {
            *last = Context::ObjectKey;
        }
    }

    fn read_literal(&mut self, literal: &[u8]) -> Result<()> {
        for &expected in literal {
            if self.expect_byte()? != expected {
                bail!(self.error("Unrecognized token"));
            }
        }
        Ok(())
    }

    fn read_number(&mut self) -> Result<Token> {
        self.scratch.clear();
        let mut is_float = false;
        while let Some(b) = self.peek_byte()? {
            match b {
                b'0'..=b'9' | b'-' | b'+' => {}
                b'.' | b'e' | b'E' => is_float = true,
                _ => break,
            }
            self.scratch.push(b);
            self.next_byte()?;
        }
        let s = std::str::from_utf8(&self.scratch)?;
        if !is_float && let Ok(i) = s.parse::<i64>() {
            return Ok(Token::Int(i));
        }
        s.parse::<f64>().map(Token::Float).map_err(|_| self.error(&format!("Invalid number '{s}'")))
    }

    fn read_hex4(&mut self) -> Result<u32> {
        let mut v = 0;
        for _ in 0..4 {
            let b = self.expect_byte()?;
            let d = (b as char).to_digit(16).ok_or_else(|| self.error("Invalid unicode escape"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn read_string(&mut self) -> Result<String> {
        self.scratch.clear();
        loop {
            // fast path: copy runs of plain bytes
            if self.pos < self.len {
                let start = self.pos;
                let mut end = start;
                while end < self.len && self.buf[end] != b'"' && self.buf[end] != b'\\' && self.buf[end] != b'\n' {
                    end += 1;
                }
                if end > start {
                    self.scratch.extend_from_slice(&self.buf[start..end]);
                    self.column += end - start;
                    self.pos = end;
                }
            }
            match self.expect_byte()? {
                b'"' => break,
                b'\\' => match self.expect_byte()? {
                    b'"' => self.scratch.push(b'"'),
                    b'\\' => self.scratch.push(b'\\'),
                    b'/' => self.scratch.push(b'/'),
                    b'b' => self.scratch.push(8),
                    b'f' => self.scratch.push(12),
                    b'n' => self.scratch.push(b'\n'),
                    b'r' => self.scratch.push(b'\r'),
                    b't' => self.scratch.push(b'\t'),
                    b'u' => {
                        let mut code = self.read_hex4()?;
                        if (0xD800..0xDC00).contains(&code) {
                            // surrogate pair
                            if self.expect_byte()? == b'\\' && self.expect_byte()? == b'u' {
                                let low = self.read_hex4()?;
                                code = 0x10000 + ((code - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
                            }
                        }
                        let c = char::from_u32(code).unwrap_or('\u{FFFD}');
                        let mut tmp = [0u8; 4];
                        self.scratch.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                    _ => bail!(self.error("Invalid escape")),
                },
                b => self.scratch.push(b),
            }
        }
        Ok(String::from_utf8_lossy(&self.scratch).into_owned())
    }

    /// If the current token is a start marker, skip to its matching end marker (Jackson's `skipChildren`).
    pub(crate) fn skip_children(&mut self) -> Result<()> {
        if !matches!(self.current, Some(Token::StartObject | Token::StartArray)) {
            return Ok(());
        }
        let mut depth = 1;
        while depth > 0 {
            match self.next_token()? {
                Some(Token::StartObject | Token::StartArray) => depth += 1,
                Some(Token::EndObject | Token::EndArray) => depth -= 1,
                Some(_) => {}
                None => bail!(self.error("Unexpected end-of-input")),
            }
        }
        Ok(())
    }

    /// Read the value starting at the current token (Jackson's `ObjectMapper.readValue(parser, ...)`).
    pub(crate) fn read_value(&mut self) -> Result<Json> {
        let token = self.current.take().ok_or_else(|| self.error("No current token"))?;
        self.read_value_from(token)
    }

    fn read_value_from(&mut self, token: Token) -> Result<Json> {
        Ok(match token {
            Token::StartObject => {
                let mut entries = vec![];
                loop {
                    match self.next_token()? {
                        Some(Token::EndObject) => break,
                        Some(Token::FieldName(name)) => {
                            let value_token =
                                self.next_token()?.ok_or_else(|| self.error("Unexpected end-of-input"))?;
                            let value = self.read_value_from(value_token)?;
                            if let Some(existing) = entries.iter_mut().find(|(k, _): &&mut (String, Json)| *k == name) {
                                existing.1 = value;
                            } else {
                                entries.push((name, value));
                            }
                        }
                        _ => bail!(self.error("Unexpected token in object")),
                    }
                }
                Json::Object(entries)
            }
            Token::StartArray => {
                let mut items = vec![];
                loop {
                    match self.next_token()? {
                        Some(Token::EndArray) => break,
                        Some(t) => items.push(self.read_value_from(t)?),
                        None => bail!(self.error("Unexpected end-of-input")),
                    }
                }
                Json::Array(items)
            }
            Token::String(s) => Json::String(s),
            Token::Int(i) => Json::Int(i),
            Token::Float(f) => Json::Float(f),
            Token::True => Json::Bool(true),
            Token::False => Json::Bool(false),
            Token::Null => Json::Null,
            Token::EndObject | Token::EndArray | Token::FieldName(_) => bail!(self.error("Unexpected token")),
        })
    }
}

#[derive(Debug)]
enum GeometryState {
    Missing,
    Empty,
    Parsed { kind: Json, coordinates: Json },
}

const WARN_LIMIT: usize = 100;

/// Streaming iterator over the features in a geojson document. Yields an error (and then stops) on invalid JSON
/// syntax; unexpected geojson objects log a warning and produce empty geometries.
pub struct GeoJsonFeatureIterator<R: Read> {
    parser: JsonParser<R>,
    properties: Option<Tags>,
    geometry: GeometryState,
    nesting_level: i32,
    geometry_start: Option<(usize, usize)>,
    warnings: usize,
    name: Option<String>,
    failed: bool,
}

impl<R: Read> GeoJsonFeatureIterator<R> {
    pub fn new(input: R, name: Option<String>) -> GeoJsonFeatureIterator<R> {
        GeoJsonFeatureIterator {
            parser: JsonParser::new(input),
            properties: None,
            geometry: GeometryState::Missing,
            nesting_level: 0,
            geometry_start: None,
            warnings: 0,
            name,
            failed: false,
        }
    }

    fn advance(&mut self) -> Result<Option<GeoJsonFeature>> {
        self.geometry = GeometryState::Missing;
        self.properties = None;
        let mut next = None;
        while next.is_none() && !self.parser.is_closed() {
            if self.nesting_level == 0 {
                self.find_next_struct()?;
            }
            let mut token = None;
            while !self.parser.is_closed() && self.nesting_level > 0 {
                token = self.parser.next_token()?;
                match &token {
                    None => break,
                    Some(t) if t.is_struct_end() => break,
                    Some(Token::StartObject) => self.nesting_level += 1,
                    Some(Token::FieldName(field)) => match field.as_str() {
                        "geometry" => self.consume_geometry()?,
                        "properties" => self.consume_properties()?,
                        "type" => {
                            self.consume(|t| matches!(t, Token::String(_)), "VALUE_STRING")?;
                        }
                        "features" => {
                            if self.consume(|t| *t == Token::StartArray, "START_ARRAY")? {
                                self.nesting_level += 1;
                                if self.consume(|t| *t == Token::StartObject, "START_OBJECT")? {
                                    self.nesting_level += 1;
                                }
                            }
                        }
                        _ => {
                            self.parser.next_token()?;
                            self.parser.skip_children()?;
                        }
                    },
                    Some(other) => {
                        let message = format!("Unexpected token inside struct: {}", other.describe());
                        self.warn(&message);
                    }
                }
            }
            match token {
                Some(Token::EndArray) => self.nesting_level -= 1,
                Some(Token::EndObject) => {
                    if let Some(geometry) = self.get_geometry() {
                        next = Some(GeoJsonFeature { geometry, tags: self.properties.take().unwrap_or_default() });
                    }
                    self.nesting_level -= 1;
                }
                _ => {}
            }
        }
        Ok(next)
    }

    fn consume(&mut self, matches: impl Fn(&Token) -> bool, expected: &str) -> Result<bool> {
        let token = self.parser.next_token()?;
        if !token.as_ref().is_some_and(matches) {
            self.warn(&format!("Unexpected token type: {expected}"));
            self.parser.skip_children()?;
            return Ok(false);
        }
        Ok(true)
    }

    fn consume_properties(&mut self) -> Result<()> {
        if self.consume(|t| *t == Token::StartObject, "START_OBJECT")?
            && let Json::Object(entries) = self.parser.read_value()?
        {
            self.properties = Some(into_tags(entries));
        }
        Ok(())
    }

    fn consume_geometry(&mut self) -> Result<()> {
        if self.consume(|t| *t == Token::StartObject, "START_OBJECT")? {
            self.geometry_start = Some(self.parser.token_location());
            let mut kind = Json::Null;
            let mut coordinates = Json::Null;
            if let Json::Object(entries) = self.parser.read_value()? {
                for (k, v) in entries {
                    match k.as_str() {
                        "type" => kind = v,
                        "coordinates" => coordinates = v,
                        _ => {}
                    }
                }
            }
            self.geometry = GeometryState::Parsed { kind, coordinates };
        } else {
            self.geometry = GeometryState::Empty;
        }
        Ok(())
    }

    fn find_next_struct(&mut self) -> Result<()> {
        loop {
            match self.parser.next_token()? {
                None | Some(Token::StartObject) => break,
                Some(token) => {
                    self.warn(&format!("Unexpected top-level token: {}", token.describe()));
                    self.parser.skip_children()?;
                }
            }
        }
        if !self.parser.is_closed() {
            self.nesting_level += 1;
        }
        Ok(())
    }

    fn get_geometry(&mut self) -> Option<Geometry<f64>> {
        let state = std::mem::replace(&mut self.geometry, GeometryState::Missing);
        let result = match &state {
            GeometryState::Empty => Some(geo_utils::empty_geometry()),
            GeometryState::Missing => None,
            GeometryState::Parsed { kind, coordinates } => Some(match (coordinates.as_array(), kind) {
                (None, _) => {
                    self.warn_geometry(&format!("Expected coordinate list but got: {coordinates}"));
                    geo_utils::empty_geometry()
                }
                (Some(_), kind) if !matches!(kind, Json::String(_)) => {
                    self.warn_geometry(&format!("Expected geometry type string but got: {coordinates}"));
                    geo_utils::empty_geometry()
                }
                (Some(coords), Json::String(kind)) => match kind.as_str() {
                    "Point" => self.point(coords).map_or_else(geo_utils::empty_point, Geometry::Point),
                    "LineString" => Geometry::LineString(self.line_string(coords)),
                    "Polygon" => Geometry::Polygon(self.polygon(coords)),
                    "MultiPoint" => {
                        let lists = self.lists(coords);
                        Geometry::MultiPoint(MultiPoint(lists.into_iter().filter_map(|c| self.point(c)).collect()))
                    }
                    "MultiLineString" => {
                        let lists = self.lists(coords);
                        Geometry::MultiLineString(MultiLineString(
                            lists.into_iter().map(|c| self.line_string(c)).collect(),
                        ))
                    }
                    "MultiPolygon" => {
                        let lists = self.lists(coords);
                        Geometry::MultiPolygon(MultiPolygon(lists.into_iter().map(|c| self.polygon(c)).collect()))
                    }
                    other => {
                        self.warn_geometry(&format!("Unexpected geometry type: {other}"));
                        geo_utils::empty_geometry()
                    }
                },
                _ => unreachable!(),
            }),
        };
        self.geometry = state;
        result
    }

    /// Items of `coordinates` that are lists, warning about the others.
    fn lists<'a>(&mut self, coordinates: &'a [Json]) -> Vec<&'a [Json]> {
        let mut result = Vec::with_capacity(coordinates.len());
        for item in coordinates {
            match item.as_array() {
                Some(list) => result.push(list),
                None => self.warn_geometry(&format!("Expecting list in geojson geometry but got: {item}")),
            }
        }
        result
    }

    fn coordinate(item: &[Json]) -> Option<Coord<f64>> {
        if item.len() >= 2
            && let (Some(x), Some(y)) = (item[0].as_number(), item[1].as_number())
        {
            return Some(Coord { x, y });
        }
        None
    }

    fn point(&mut self, coordinates: &[Json]) -> Option<Point<f64>> {
        let result = Self::coordinate(coordinates).map(Point);
        if result.is_none() {
            self.warn_geometry(&format!("Invalid geojson point coordinate: {}", Json::Array(coordinates.to_vec())));
        }
        result
    }

    fn coordinate_sequence(&mut self, list: &[Json]) -> Vec<Coord<f64>> {
        let mut result = Vec::with_capacity(list.len());
        for item in list {
            match item.as_array().and_then(Self::coordinate) {
                Some(c) => result.push(c),
                None => self.warn_geometry(&format!("Invalid geojson coordinate: {item}")),
            }
        }
        result
    }

    fn line_string(&mut self, coordinates: &[Json]) -> LineString<f64> {
        LineString(self.coordinate_sequence(coordinates))
    }

    fn linear_ring(&mut self, list: &[Json]) -> LineString<f64> {
        let coords = self.coordinate_sequence(list);
        let is_ring = coords.is_empty() || (coords.len() >= 4 && coords.first() == coords.last());
        if !is_ring {
            self.warn_geometry(&format!("Invalid geojson polygon ring {}", Json::Array(list.to_vec())));
            return LineString(vec![]);
        }
        LineString(coords)
    }

    fn polygon(&mut self, coordinates: &[Json]) -> Polygon<f64> {
        let rings: Vec<LineString<f64>> = coordinates
            .iter()
            .filter_map(Json::as_array)
            .collect::<Vec<_>>()
            .into_iter()
            .map(|ring| self.linear_ring(ring))
            .collect();
        let mut rings = rings.into_iter();
        match rings.next() {
            Some(shell) if !shell.0.is_empty() => Polygon::new(shell, rings.filter(|r| !r.0.is_empty()).collect()),
            _ => geo_utils::empty_polygon(),
        }
    }

    fn warn(&mut self, message: &str) {
        let location = self.parser.token_location();
        self.warn_at(message, location);
    }

    fn warn_geometry(&mut self, message: &str) {
        let location = self.geometry_start.unwrap_or_else(|| self.parser.token_location());
        self.warn_at(message, location);
    }

    fn warn_at(&mut self, message: &str, (line, column): (usize, usize)) {
        self.warnings += 1;
        if self.warnings < WARN_LIMIT {
            let name = self.name.as_ref().map(|n| format!("{n}:")).unwrap_or_default();
            warn!("[{name}{line}:{column}] {message}");
        } else if self.warnings == WARN_LIMIT {
            warn!("Too many warnings, geojson file might be invalid");
        }
    }
}

impl<R: Read> Iterator for GeoJsonFeatureIterator<R> {
    type Item = Result<GeoJsonFeature>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        match self.advance() {
            Ok(Some(feature)) => Some(Ok(feature)),
            Ok(None) => None,
            Err(e) => {
                self.failed = true;
                let name = self.name.as_deref().unwrap_or("input");
                Some(Err(e.context(format!("JSON syntax error in {name}"))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(json: &str) -> Vec<Token> {
        let mut parser = JsonParser::new(json.as_bytes());
        let mut result = vec![];
        while let Some(t) = parser.next_token().unwrap() {
            result.push(t);
        }
        result
    }

    #[test]
    fn tokenize() {
        assert_eq!(
            vec![
                Token::StartObject,
                Token::FieldName("a".into()),
                Token::StartArray,
                Token::Int(1),
                Token::Float(2.5),
                Token::String("x\"\u{e9}".into()),
                Token::True,
                Token::False,
                Token::Null,
                Token::EndArray,
                Token::EndObject,
                Token::Int(-3),
            ],
            tokens(r#"{"a": [1, 2.5, "x\"é", true, false, null]} -3"#)
        );
    }

    #[test]
    fn syntax_error() {
        let mut parser = JsonParser::new(&b"{\"a\" 1}"[..]);
        parser.next_token().unwrap();
        assert!(parser.next_token().is_err());
    }

    #[test]
    fn skip_children() {
        let mut parser = JsonParser::new(&br#"[{"a": [1, {"b": 2}]}, 3]"#[..]);
        assert_eq!(Some(Token::StartArray), parser.next_token().unwrap());
        assert_eq!(Some(Token::StartObject), parser.next_token().unwrap());
        parser.skip_children().unwrap();
        assert_eq!(Some(&Token::EndObject), parser.current_token());
        assert_eq!(Some(Token::Int(3)), parser.next_token().unwrap());
    }
}
